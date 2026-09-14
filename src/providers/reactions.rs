use super::{
    GithubProvider, GraphqlResult, MutationAdmission, MutationTransport, Session, coordinates,
    coordinates_match, rejected, validate_action_identity, validate_node_id,
};
use crate::domain::{
    MutationContext, MutationTerminalRecord, ProviderCoordinates, ProviderMutationOutcome,
    ProviderReadEvidence, ReactableKind, ReactionAcknowledgement, ReactionAction, ReactionContent,
    ReactionIntent, ReactionObservation, ReactionRequest, ReactionSubjectSnapshot, ReactionTarget,
    Repository, SelectedViewer,
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_REACTION_PAGES: usize = 10;
const MAX_REACTION_TEXT_BYTES: usize = 1024 * 1024;

impl GithubProvider {
    /// Freeze an exact Add or Remove request after one complete targeted read.
    /// This read sends no mutation and never substitutes cached authority.
    // Keep every click-fence component explicit at this boundary; collapsing
    // them into a loosely related options object would make omissions easier.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_reaction(
        &self,
        repo: &Repository,
        number: u64,
        displayed: &ReactionSubjectSnapshot,
        content: ReactionContent,
        intent: ReactionIntent,
        operation_id: String,
        attempt_id: String,
    ) -> std::result::Result<ReactionRequest, String> {
        validate_action_identity(&operation_id, "operation_id")?;
        validate_action_identity(&attempt_id, "attempt_id")?;
        self.validate_repo(repo)
            .map_err(|error| error.to_string())?;
        validate_displayed_subject(self, repo, number, displayed, content, intent)?;
        let observed = Session::new(self)
            .reaction_observation(repo, number, &displayed_target(repo, displayed), content)
            .map_err(|error| error.to_string())?;
        if observed.viewer
            != displayed
                .fresh_capability
                .as_ref()
                .expect("validated")
                .viewer
        {
            return Err("selected viewer changed during reaction preparation".into());
        }
        let action = match intent {
            ReactionIntent::Add if !observed.viewer_can_react => {
                return Err("GitHub says the selected viewer cannot add reactions".into());
            }
            ReactionIntent::Add if !observed.viewer_has_reacted => ReactionAction::Add,
            ReactionIntent::Add => {
                return Err(
                    "the selected viewer already has this reaction; zero writes sent".into(),
                );
            }
            ReactionIntent::Remove if observed.viewer_has_reacted => ReactionAction::Remove {
                existing_reaction_id: observed
                    .own_reaction_id
                    .clone()
                    .ok_or_else(|| "own reaction identity is incomplete".to_owned())?,
            },
            ReactionIntent::Remove => {
                return Err(
                    "the selected viewer no longer has this reaction; zero writes sent".into(),
                );
            }
        };
        Ok(ReactionRequest {
            operation_id,
            attempt_id,
            target: observed.target,
            viewer: observed.viewer,
            content,
            action,
        })
    }

    /// Admit, revalidate, and dispatch exactly one GraphQL reaction mutation.
    pub fn execute_reaction(
        &self,
        repo: &Repository,
        request: &ReactionRequest,
        admission: &mut impl MutationAdmission,
    ) -> ProviderMutationOutcome<ReactionAcknowledgement> {
        if let Err(reason) = validate_reaction_request(self, repo, request) {
            return rejected(reason);
        }
        let mutation = PreparedReactionMutation::new(request);
        let context = mutation.context(request);
        let mut attempt = match admission.admit(&context) {
            Ok(attempt) => attempt,
            Err(error) => {
                return rejected(format!(
                    "durable mutation admission failed; dispatched zero writes: {error}"
                ));
            }
        };
        let receipt = attempt.receipt();
        if receipt.operation_id != context.operation_id
            || receipt.attempt_id != context.attempt_id
            || receipt.durable_record_id.is_empty()
            || receipt.durable_record_id.len() > 1024
            || receipt.durable_record_id.chars().any(char::is_control)
        {
            return reaction_not_started(
                &mut *attempt,
                "durable admission receipt does not match the frozen attempt".into(),
            );
        }
        let fresh = match Session::new(self).reaction_observation(
            repo,
            request.target.pull_request.pull_request,
            &request.target,
            request.content,
        ) {
            Ok(fresh) => fresh,
            Err(error) => {
                return reaction_not_started(
                    &mut *attempt,
                    format!("post-admission reaction preflight failed: {error}"),
                );
            }
        };
        if let Err(reason) = validate_fresh_request(request, &fresh) {
            return reaction_not_started(&mut *attempt, reason);
        }
        let data = match mutation.dispatch(self) {
            MutationTransport::Rejected(reason) => {
                return reaction_not_started(&mut *attempt, reason);
            }
            MutationTransport::Uncertain(reason) => {
                return reaction_uncertain(&context, &mut *attempt, reason);
            }
            MutationTransport::Acknowledged(data) => data,
        };
        let acknowledgement = match validate_acknowledgement(request, data) {
            Ok(acknowledgement) => acknowledgement,
            Err(reason) => return reaction_uncertain(&context, &mut *attempt, reason),
        };
        let encoded = match serde_json::to_value(&acknowledgement) {
            Ok(encoded) => encoded,
            Err(error) => {
                return reaction_uncertain(
                    &context,
                    &mut *attempt,
                    format!("could not encode the reaction acknowledgement: {error}"),
                );
            }
        };
        if let Err(error) = attempt.record_terminal(&MutationTerminalRecord::Acknowledged {
            acknowledgement: encoded,
        }) {
            return ProviderMutationOutcome::Uncertain {
                context,
                reason: format!(
                    "GitHub acknowledged the reaction but its durable terminal acknowledgement could not be saved; durable InFlight retained: {error}"
                ),
            };
        }
        ProviderMutationOutcome::Acknowledged(acknowledgement)
    }

    /// Read current exact state for explicit recovery. This never mutates and
    /// does not attribute the observed state to the recorded attempt.
    pub fn reconcile_reaction(
        &self,
        repo: &Repository,
        request: &ReactionRequest,
    ) -> ProviderReadEvidence<ReactionObservation> {
        if let Err(reason) = validate_reaction_request(self, repo, request) {
            return ProviderReadEvidence::Inconclusive { reason };
        }
        match Session::new(self).reaction_observation(
            repo,
            request.target.pull_request.pull_request,
            &request.target,
            request.content,
        ) {
            Ok(observation) => ProviderReadEvidence::Observed(observation),
            Err(error) => ProviderReadEvidence::Inconclusive {
                reason: error.to_string(),
            },
        }
    }
}

fn displayed_target(repo: &Repository, displayed: &ReactionSubjectSnapshot) -> ReactionTarget {
    ReactionTarget {
        kind: displayed.kind,
        repository: repo.clone(),
        pull_request: displayed.pull_request.clone(),
        subject: displayed.subject.clone(),
        parent_review: displayed.parent_review.clone(),
        content: displayed.content.clone(),
    }
}

fn validate_displayed_subject(
    provider: &GithubProvider,
    repo: &Repository,
    number: u64,
    displayed: &ReactionSubjectSnapshot,
    content: ReactionContent,
    intent: ReactionIntent,
) -> std::result::Result<(), String> {
    let capability = displayed.fresh_capability.as_ref().ok_or_else(|| {
        "reaction authority is cached, partial, or unknown; refresh before reacting".to_owned()
    })?;
    if capability.viewer.node_id.is_empty()
        || !capability
            .viewer
            .login
            .eq_ignore_ascii_case(&provider.account.login)
    {
        return Err("fresh reaction viewer does not match the selected account".into());
    }
    validate_node_id(&capability.viewer.node_id).map_err(|error| error.to_string())?;
    if !coordinates_match(repo, number, &displayed.pull_request)
        || !coordinates_match(repo, number, &displayed.subject)
        || displayed.content.len() > MAX_REACTION_TEXT_BYTES
    {
        return Err("displayed reactable identity does not match this repository and PR".into());
    }
    if displayed.kind == ReactableKind::PullRequest && displayed.subject != displayed.pull_request {
        return Err("pull request reaction subject does not match its exact PR node".into());
    }
    match (displayed.kind, &displayed.parent_review) {
        (ReactableKind::PullRequestReviewComment, Some(parent))
            if coordinates_match(repo, number, parent) => {}
        (ReactableKind::PullRequestReviewComment, _) => {
            return Err("review comment reaction is missing its exact parent review".into());
        }
        (_, None) => {}
        _ => return Err("non-comment reactable unexpectedly has a parent review".into()),
    }
    if !displayed.reactions.complete {
        return Err("reaction groups are incomplete; absence was not inferred".into());
    }
    let matches = displayed
        .reactions
        .groups
        .iter()
        .filter(|group| group.content == content)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err("reaction group evidence is missing or ambiguous".into());
    }
    let selected = matches[0].viewer_has_reacted;
    match intent {
        ReactionIntent::Add if !capability.viewer_can_react => {
            Err("GitHub says the selected viewer cannot add reactions".into())
        }
        ReactionIntent::Add if selected => {
            Err("the clicked Add state no longer matches the fresh snapshot".into())
        }
        ReactionIntent::Remove if !selected => {
            Err("the clicked Remove state no longer matches the fresh snapshot".into())
        }
        _ => Ok(()),
    }
}

fn validate_reaction_request(
    provider: &GithubProvider,
    repo: &Repository,
    request: &ReactionRequest,
) -> std::result::Result<(), String> {
    provider
        .validate_repo(repo)
        .map_err(|error| error.to_string())?;
    validate_action_identity(&request.operation_id, "operation_id")?;
    validate_action_identity(&request.attempt_id, "attempt_id")?;
    let number = request.target.pull_request.pull_request;
    if request.target.repository != *repo
        || !coordinates_match(repo, number, &request.target.pull_request)
        || !coordinates_match(repo, number, &request.target.subject)
        || !request
            .viewer
            .login
            .eq_ignore_ascii_case(&provider.account.login)
        || request.target.content.len() > MAX_REACTION_TEXT_BYTES
    {
        return Err("frozen reaction request belongs to another account, repository, or PR".into());
    }
    validate_node_id(&request.viewer.node_id).map_err(|error| error.to_string())?;
    validate_node_id(&request.target.pull_request.remote_id).map_err(|error| error.to_string())?;
    validate_node_id(&request.target.subject.remote_id).map_err(|error| error.to_string())?;
    if let ReactionAction::Remove {
        existing_reaction_id,
    } = &request.action
    {
        validate_node_id(existing_reaction_id).map_err(|error| error.to_string())?;
    }
    match (request.target.kind, &request.target.parent_review) {
        (ReactableKind::PullRequestReviewComment, Some(parent))
            if coordinates_match(repo, number, parent) =>
        {
            validate_node_id(&parent.remote_id).map_err(|error| error.to_string())?;
        }
        (ReactableKind::PullRequestReviewComment, _) => {
            return Err("frozen review comment is missing its exact parent review".into());
        }
        (_, None) => {}
        _ => return Err("frozen non-comment reactable has an unexpected parent review".into()),
    }
    if request.target.kind == ReactableKind::PullRequest
        && request.target.subject != request.target.pull_request
    {
        return Err("frozen PR reactable does not use the exact fresh PR node ID".into());
    }
    Ok(())
}

fn validate_fresh_request(
    request: &ReactionRequest,
    fresh: &ReactionObservation,
) -> std::result::Result<(), String> {
    if fresh.target != request.target
        || fresh.viewer != request.viewer
        || fresh.content != request.content
    {
        return Err(
            "post-admission reaction identity or subject content changed; zero writes sent".into(),
        );
    }
    match &request.action {
        ReactionAction::Add if fresh.viewer_can_react && !fresh.viewer_has_reacted => Ok(()),
        ReactionAction::Add if !fresh.viewer_can_react => Err(
            "post-admission preflight says the selected viewer cannot add reactions; zero writes sent"
                .into(),
        ),
        ReactionAction::Add => {
            Err("post-admission preflight found the reaction already present; zero writes sent".into())
        }
        ReactionAction::Remove {
            existing_reaction_id,
        } if fresh.viewer_has_reacted
            && fresh.own_reaction_id.as_deref() == Some(existing_reaction_id.as_str()) =>
        {
            Ok(())
        }
        ReactionAction::Remove { .. } => Err(
            "post-admission preflight did not find the same exact own reaction ID; zero writes sent"
                .into(),
        ),
    }
}

fn reaction_not_started<A>(
    attempt: &mut dyn super::AdmittedMutationAttempt,
    reason: String,
) -> ProviderMutationOutcome<A> {
    let record_error = attempt
        .record_terminal(&MutationTerminalRecord::NotStarted {
            reason: reason.clone(),
        })
        .err();
    rejected(if let Some(error) = record_error {
        format!(
            "{reason}; dispatched zero writes; durable InFlight retained because NotStarted recording failed: {error}"
        )
    } else {
        format!("{reason}; dispatched zero writes")
    })
}

fn reaction_uncertain<A>(
    context: &MutationContext,
    attempt: &mut dyn super::AdmittedMutationAttempt,
    reason: String,
) -> ProviderMutationOutcome<A> {
    let record_error = attempt
        .record_terminal(&MutationTerminalRecord::Uncertain {
            reason: reason.clone(),
        })
        .err();
    ProviderMutationOutcome::Uncertain {
        context: context.clone(),
        reason: if let Some(error) = record_error {
            format!(
                "{reason}; durable Uncertain recording failed and InFlight authority was retained: {error}"
            )
        } else {
            reason
        },
    }
}

impl<'a> Session<'a> {
    fn reaction_observation(
        &mut self,
        repo: &Repository,
        number: u64,
        expected: &ReactionTarget,
        content: ReactionContent,
    ) -> Result<ReactionObservation> {
        let mut after: Option<String> = None;
        let mut own = Vec::new();
        let mut viewer: Option<SelectedViewer> = None;
        let mut viewer_has_reacted: Option<bool> = None;
        for page in 0..MAX_REACTION_PAGES {
            let response: GraphqlResult<ReactionTargetData> = self.graphql(
                REACTION_TARGET_QUERY,
                json!({
                    "id": expected.subject.remote_id,
                    "content": content.graphql_name(),
                    "after": after,
                }),
            )?;
            ensure!(!response.partial, "GitHub reaction target read was partial");
            let selected = SelectedViewer {
                node_id: response.data.viewer.id,
                login: response.data.viewer.login,
            };
            validate_node_id(&selected.node_id)?;
            ensure!(
                selected
                    .login
                    .eq_ignore_ascii_case(&self.provider.account.login),
                "selected GitHub credential resolved to another account"
            );
            if let Some(prior) = &viewer {
                ensure!(
                    prior == &selected,
                    "reaction viewer changed during pagination"
                );
            } else {
                viewer = Some(selected.clone());
            }
            let node = response
                .data
                .node
                .context("Reactable target is unavailable")?;
            let observed_target = node.target(repo, number)?;
            ensure!(
                &observed_target == expected,
                "reactable target identity, parent, or content changed"
            );
            if let Some(prior) = viewer_has_reacted {
                ensure!(
                    prior == node.reactions.viewer_has_reacted,
                    "selected-viewer reaction state changed during pagination"
                );
            } else {
                viewer_has_reacted = Some(node.reactions.viewer_has_reacted);
            }
            ensure!(
                node.reactions.nodes.iter().all(Option::is_some),
                "reaction entries were unavailable; identity remains Unknown"
            );
            for reaction in node.reactions.nodes.into_iter().flatten() {
                validate_node_id(&reaction.id)?;
                ensure!(
                    reaction.content == content.graphql_name()
                        && reaction.reactable.id == expected.subject.remote_id,
                    "targeted reaction entry has mismatched content or reactable"
                );
                if reaction.user.id == selected.node_id
                    || reaction.user.login.eq_ignore_ascii_case(&selected.login)
                {
                    ensure!(
                        reaction.user.id == selected.node_id
                            && reaction.user.login.eq_ignore_ascii_case(&selected.login),
                        "reaction user only partially matches the selected viewer"
                    );
                    own.push(reaction.id);
                }
            }
            if !node.reactions.viewer_has_reacted {
                ensure!(
                    own.is_empty(),
                    "reaction connection reports absence but returned an own reaction"
                );
                return Ok(ReactionObservation {
                    target: observed_target,
                    viewer: viewer.expect("one page"),
                    content,
                    viewer_can_react: node.viewer_can_react,
                    viewer_has_reacted: false,
                    own_reaction_id: None,
                });
            }
            if !node.reactions.page_info.has_next_page {
                ensure!(
                    own.len() <= 1,
                    "GitHub returned ambiguous duplicate own reactions"
                );
                let selected = viewer.expect("one page");
                let selected_state = viewer_has_reacted.expect("one page");
                ensure!(
                    selected_state == !own.is_empty(),
                    "reaction connection and exact own-reaction identity disagree"
                );
                return Ok(ReactionObservation {
                    target: observed_target,
                    viewer: selected,
                    content,
                    viewer_can_react: node.viewer_can_react,
                    viewer_has_reacted: selected_state,
                    own_reaction_id: own.pop(),
                });
            }
            ensure!(
                page + 1 < MAX_REACTION_PAGES,
                "reaction identity pagination reached its explicit bound"
            );
            after = Some(
                node.reactions
                    .page_info
                    .end_cursor
                    .filter(|cursor| !cursor.is_empty())
                    .context("reaction pagination omitted its continuation cursor")?,
            );
        }
        unreachable!("bounded loop returns or errors")
    }
}

fn validate_node_parent(
    node: &ReactionTargetNode,
    repo: &Repository,
    number: u64,
) -> Result<(ProviderCoordinates, Option<ProviderCoordinates>)> {
    let repository = repo.full_name();
    match node.kind()? {
        ReactableKind::PullRequest => {
            ensure!(
                node.number == Some(number)
                    && node.repository.as_ref().is_some_and(|value| value
                        .name_with_owner
                        .eq_ignore_ascii_case(&repository)),
                "reaction PR parent mismatch"
            );
            Ok((coordinates(repo, number, node.id.clone()), None))
        }
        ReactableKind::IssueComment | ReactableKind::PullRequestReview => {
            let pull = node
                .pull_request
                .as_ref()
                .context("reaction target omitted its PR parent")?;
            pull.validate(&repository, number)?;
            Ok((coordinates(repo, number, pull.id.clone()), None))
        }
        ReactableKind::PullRequestReviewComment => {
            let review = node
                .pull_request_review
                .as_ref()
                .context("review comment omitted its parent review")?;
            review.pull_request.validate(&repository, number)?;
            Ok((
                coordinates(repo, number, review.pull_request.id.clone()),
                Some(coordinates(repo, number, review.id.clone())),
            ))
        }
    }
}

impl ReactionTargetNode {
    fn kind(&self) -> Result<ReactableKind> {
        match self.typename.as_str() {
            "PullRequest" => Ok(ReactableKind::PullRequest),
            "PullRequestReview" => Ok(ReactableKind::PullRequestReview),
            "IssueComment" => Ok(ReactableKind::IssueComment),
            "PullRequestReviewComment" => Ok(ReactableKind::PullRequestReviewComment),
            _ => anyhow::bail!("reaction target has an unsupported provider type"),
        }
    }

    fn target(&self, repo: &Repository, number: u64) -> Result<ReactionTarget> {
        validate_node_id(&self.id)?;
        ensure!(
            self.body.len() <= MAX_REACTION_TEXT_BYTES,
            "reactable content exceeds its explicit bound"
        );
        let kind = self.kind()?;
        let (pull_request, parent_review) = validate_node_parent(self, repo, number)?;
        let subject = coordinates(repo, number, self.id.clone());
        if kind == ReactableKind::PullRequest {
            ensure!(subject == pull_request, "PR reactable node ID mismatch");
        }
        Ok(ReactionTarget {
            kind,
            repository: repo.clone(),
            pull_request,
            subject,
            parent_review,
            content: self.body.clone(),
        })
    }
}

impl ReactionParentPull {
    fn validate(&self, repository: &str, number: u64) -> Result<()> {
        validate_node_id(&self.id)?;
        ensure!(
            self.number == number
                && self
                    .repository
                    .name_with_owner
                    .eq_ignore_ascii_case(repository),
            "reaction target belongs to another repository or PR"
        );
        Ok(())
    }
}

struct PreparedReactionMutation {
    query: &'static str,
    variables: Value,
}

impl PreparedReactionMutation {
    fn new(request: &ReactionRequest) -> Self {
        Self {
            query: match request.action {
                ReactionAction::Add => ADD_REACTION_MUTATION,
                ReactionAction::Remove { .. } => REMOVE_REACTION_MUTATION,
            },
            variables: json!({
                "subjectId": request.target.subject.remote_id,
                "content": request.content.graphql_name(),
                "clientMutationId": request.operation_id,
            }),
        }
    }

    fn context(&self, request: &ReactionRequest) -> MutationContext {
        MutationContext {
            operation_id: request.operation_id.clone(),
            attempt_id: request.attempt_id.clone(),
            action: match request.action {
                ReactionAction::Add => "add-reaction",
                ReactionAction::Remove { .. } => "remove-reaction",
            }
            .into(),
            payload: json!({
                "request": request,
                "dispatch": {"query": self.query, "variables": self.variables},
            }),
        }
    }

    fn dispatch(&self, provider: &GithubProvider) -> MutationTransport<ReactionMutationData> {
        #[cfg(feature = "ui-smoke")]
        if std::env::var_os("CIBERGIT_SMOKE_REACTIONS").is_some() {
            return MutationTransport::Rejected(
                "native reaction smoke hard-suppressed mutation transport".into(),
            );
        }
        Session::new(provider).graphql_mutation(self.query, self.variables.clone())
    }
}

fn validate_acknowledgement(
    request: &ReactionRequest,
    data: ReactionMutationData,
) -> std::result::Result<ReactionAcknowledgement, String> {
    let (payload, present) = match request.action {
        ReactionAction::Add => (
            data.add_reaction
                .ok_or_else(|| "GitHub omitted the addReaction acknowledgement".to_owned())?,
            true,
        ),
        ReactionAction::Remove { .. } => (
            data.remove_reaction
                .ok_or_else(|| "GitHub omitted the removeReaction acknowledgement".to_owned())?,
            false,
        ),
    };
    if payload.client_mutation_id.as_deref() != Some(request.operation_id.as_str()) {
        return Err("GitHub echoed another reaction operation ID".into());
    }
    let reaction = payload
        .reaction
        .ok_or_else(|| "GitHub omitted the exact reaction acknowledgement".to_owned())?;
    validate_node_id(&reaction.id).map_err(|error| error.to_string())?;
    if reaction.content != request.content.graphql_name()
        || reaction.user.id != request.viewer.node_id
        || !reaction
            .user
            .login
            .eq_ignore_ascii_case(&request.viewer.login)
        || reaction.reactable.id != request.target.subject.remote_id
        || reaction.reactable.typename != request.target.kind.graphql_name()
    {
        return Err(
            "GitHub reaction acknowledgement identity does not match the frozen request".into(),
        );
    }
    if let ReactionAction::Remove {
        existing_reaction_id,
    } = &request.action
        && reaction.id != *existing_reaction_id
    {
        return Err(
            "GitHub removed a different reaction ID after dispatch; the outcome is uncertain"
                .into(),
        );
    }
    let subject = payload
        .subject
        .ok_or_else(|| "GitHub omitted the returned reaction subject".to_owned())?;
    let observed_target = subject
        .target(
            &request.target.repository,
            request.target.pull_request.pull_request,
        )
        .map_err(|error| error.to_string())?;
    if observed_target != request.target || subject.reactions.viewer_has_reacted != present {
        return Err("GitHub reaction acknowledgement omitted the exact final subject state".into());
    }
    Ok(ReactionAcknowledgement {
        operation_id: request.operation_id.clone(),
        target: request.target.clone(),
        viewer: request.viewer.clone(),
        content: request.content,
        reaction_id: reaction.id,
        present,
    })
}

#[derive(Deserialize)]
struct ReactionTargetData {
    viewer: ReactionUser,
    node: Option<ReactionTargetNode>,
}

#[derive(Deserialize)]
struct ReactionTargetNode {
    #[serde(rename = "__typename")]
    typename: String,
    id: String,
    body: String,
    #[serde(rename = "viewerCanReact")]
    viewer_can_react: bool,
    repository: Option<ReactionRepository>,
    number: Option<u64>,
    #[serde(rename = "pullRequest")]
    pull_request: Option<ReactionParentPull>,
    #[serde(rename = "pullRequestReview")]
    pull_request_review: Option<ReactionParentReview>,
    reactions: ReactionConnection,
}

#[derive(Deserialize)]
struct ReactionRepository {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Deserialize)]
struct ReactionParentPull {
    id: String,
    number: u64,
    repository: ReactionRepository,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReactionParentReview {
    id: String,
    pull_request: ReactionParentPull,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReactionConnection {
    viewer_has_reacted: bool,
    nodes: Vec<Option<ReactionNode>>,
    page_info: super::PageInfo,
}

#[derive(Deserialize)]
struct ReactionNode {
    id: String,
    content: String,
    user: ReactionUser,
    reactable: ReactionReactable,
}

#[derive(Deserialize)]
struct ReactionUser {
    id: String,
    login: String,
}

#[derive(Deserialize)]
struct ReactionReactable {
    #[serde(rename = "__typename")]
    typename: String,
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReactionPayload {
    client_mutation_id: Option<String>,
    reaction: Option<ReactionNode>,
    subject: Option<ReactionTargetNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReactionMutationData {
    add_reaction: Option<ReactionPayload>,
    remove_reaction: Option<ReactionPayload>,
}

const REACTION_TARGET_QUERY: &str = r#"query ReactionTarget(
    $id: ID!, $content: ReactionContent!, $after: String
) {
  viewer { id login }
  node(id: $id) {
    __typename
    ... on PullRequest {
      id body viewerCanReact number repository { nameWithOwner }
      reactions(content: $content, first: 100, after: $after) {
        viewerHasReacted
        nodes { id content user { id login } reactable { __typename id } }
        pageInfo { hasNextPage endCursor }
      }
    }
    ... on PullRequestReview {
      id body viewerCanReact
      pullRequest { id number repository { nameWithOwner } }
      reactions(content: $content, first: 100, after: $after) {
        viewerHasReacted
        nodes { id content user { id login } reactable { __typename id } }
        pageInfo { hasNextPage endCursor }
      }
    }
    ... on IssueComment {
      id body viewerCanReact repository { nameWithOwner }
      pullRequest { id number repository { nameWithOwner } }
      reactions(content: $content, first: 100, after: $after) {
        viewerHasReacted
        nodes { id content user { id login } reactable { __typename id } }
        pageInfo { hasNextPage endCursor }
      }
    }
    ... on PullRequestReviewComment {
      id body viewerCanReact
      pullRequestReview { id pullRequest { id number repository { nameWithOwner } } }
      reactions(content: $content, first: 100, after: $after) {
        viewerHasReacted
        nodes { id content user { id login } reactable { __typename id } }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"#;

const ADD_REACTION_MUTATION: &str = r#"mutation AddReaction(
    $subjectId: ID!, $content: ReactionContent!, $clientMutationId: String!
) {
  addReaction(input: {
    subjectId: $subjectId, content: $content, clientMutationId: $clientMutationId
  }) {
    clientMutationId
    reaction { id content user { id login } reactable { __typename id } }
    subject {
      __typename
      ... on PullRequest { id body viewerCanReact number repository { nameWithOwner } reactions(content: $content, first: 1) { viewerHasReacted nodes { id content user { id login } reactable { __typename id } } pageInfo { hasNextPage endCursor } } }
      ... on PullRequestReview { id body viewerCanReact pullRequest { id number repository { nameWithOwner } } reactions(content: $content, first: 1) { viewerHasReacted nodes { id content user { id login } reactable { __typename id } } pageInfo { hasNextPage endCursor } } }
      ... on IssueComment { id body viewerCanReact repository { nameWithOwner } pullRequest { id number repository { nameWithOwner } } reactions(content: $content, first: 1) { viewerHasReacted nodes { id content user { id login } reactable { __typename id } } pageInfo { hasNextPage endCursor } } }
      ... on PullRequestReviewComment { id body viewerCanReact pullRequestReview { id pullRequest { id number repository { nameWithOwner } } } reactions(content: $content, first: 1) { viewerHasReacted nodes { id content user { id login } reactable { __typename id } } pageInfo { hasNextPage endCursor } } }
    }
  }
}"#;

const REMOVE_REACTION_MUTATION: &str = r#"mutation RemoveReaction(
    $subjectId: ID!, $content: ReactionContent!, $clientMutationId: String!
) {
  removeReaction(input: {
    subjectId: $subjectId, content: $content, clientMutationId: $clientMutationId
  }) {
    clientMutationId
    reaction { id content user { id login } reactable { __typename id } }
    subject {
      __typename
      ... on PullRequest { id body viewerCanReact number repository { nameWithOwner } reactions(content: $content, first: 1) { viewerHasReacted nodes { id content user { id login } reactable { __typename id } } pageInfo { hasNextPage endCursor } } }
      ... on PullRequestReview { id body viewerCanReact pullRequest { id number repository { nameWithOwner } } reactions(content: $content, first: 1) { viewerHasReacted nodes { id content user { id login } reactable { __typename id } } pageInfo { hasNextPage endCursor } } }
      ... on IssueComment { id body viewerCanReact repository { nameWithOwner } pullRequest { id number repository { nameWithOwner } } reactions(content: $content, first: 1) { viewerHasReacted nodes { id content user { id login } reactable { __typename id } } pageInfo { hasNextPage endCursor } } }
      ... on PullRequestReviewComment { id body viewerCanReact pullRequestReview { id pullRequest { id number repository { nameWithOwner } } } reactions(content: $content, first: 1) { viewerHasReacted nodes { id content user { id login } reactable { __typename id } } pageInfo { hasNextPage endCursor } } }
    }
  }
}"#;
