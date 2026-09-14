use super::{
    GithubProvider, GraphqlResult, MutationAdmission, MutationTransport, Session, coordinates,
    coordinates_match, dismissal_authority, rejected, validate_action_identity, validate_node_id,
    validate_sha,
};
use crate::domain::{
    DismissalAuthority, MutationContext, MutationTerminalRecord, ProviderMutationOutcome,
    ProviderReadEvidence, PullRequestReview, Repository, SelectedViewer,
    SubmittedReviewDismissalAcknowledgement, SubmittedReviewDismissalObservation,
    SubmittedReviewDismissalRequest, SubmittedReviewDismissalTarget,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_DISMISSAL_REASON_BYTES: usize = 64 * 1024;
const MAX_DISMISSAL_SOURCE_BYTES: usize = 1024 * 1024;

impl GithubProvider {
    /// Perform one complete targeted read and freeze its exact submitted-review
    /// tuple. Cached details never satisfy this boundary.
    pub fn prepare_review_dismissal(
        &self,
        repo: &Repository,
        number: u64,
        displayed: &PullRequestReview,
        reason: String,
        operation_id: String,
        attempt_id: String,
    ) -> std::result::Result<SubmittedReviewDismissalRequest, String> {
        validate_action_identity(&operation_id, "operation_id")?;
        validate_action_identity(&attempt_id, "attempt_id")?;
        self.validate_repo(repo)
            .map_err(|error| error.to_string())?;
        validate_reason(&reason)?;
        let capability = validate_displayed(self, repo, number, displayed)?;
        let expected = displayed_target(repo, displayed, &capability.pull_request)?;
        let observed = Session::new(self)
            .dismissal_observation(repo, number, &displayed.coordinates.remote_id)
            .map_err(|error| error.to_string())?;
        if observed.target != expected
            || observed.viewer != capability.viewer
            || observed.authority != capability.authority
        {
            return Err(
                "fresh dismissal target, selected viewer, or authority changed during preparation"
                    .into(),
            );
        }
        if !observed.authority.permits_attempt() {
            return Err("fresh dismissal authority is unavailable; zero writes sent".into());
        }
        Ok(SubmittedReviewDismissalRequest {
            operation_id,
            attempt_id,
            target: observed.target,
            viewer: observed.viewer,
            authority: observed.authority,
            reason,
        })
    }

    /// Acquire shared durable target authority, repeat the exact preflight, and
    /// send one GraphQL mutation. This method never retries.
    pub fn execute_review_dismissal(
        &self,
        repo: &Repository,
        request: &SubmittedReviewDismissalRequest,
        admission: &mut impl MutationAdmission,
    ) -> ProviderMutationOutcome<SubmittedReviewDismissalAcknowledgement> {
        if let Err(reason) = validate_request(self, repo, request) {
            return rejected(reason);
        }
        let mutation = PreparedDismissalMutation::new(request);
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
        if receipt.operation_id != request.operation_id
            || receipt.attempt_id != request.attempt_id
            || receipt.durable_record_id.is_empty()
            || receipt.durable_record_id.len() > 1024
            || receipt.durable_record_id.chars().any(char::is_control)
        {
            return dismissal_not_started(
                &context,
                &mut *attempt,
                "durable admission receipt does not match the frozen dismissal attempt".into(),
            );
        }
        let fresh = match Session::new(self).dismissal_observation(
            repo,
            request.target.pull_request.pull_request,
            &request.target.review.remote_id,
        ) {
            Ok(fresh) => fresh,
            Err(error) => {
                return dismissal_not_started(
                    &context,
                    &mut *attempt,
                    format!("post-admission dismissal preflight failed: {error}"),
                );
            }
        };
        if fresh.target != request.target
            || fresh.viewer != request.viewer
            || fresh.authority != request.authority
            || !fresh.authority.permits_attempt()
        {
            return dismissal_not_started(
                &context,
                &mut *attempt,
                "post-admission dismissal target, viewer, or authority changed; zero writes sent"
                    .into(),
            );
        }
        let data = match mutation.dispatch(self) {
            MutationTransport::Rejected(reason) => {
                return dismissal_not_started(&context, &mut *attempt, reason);
            }
            MutationTransport::Uncertain(reason) => {
                return dismissal_uncertain(&context, &mut *attempt, reason);
            }
            MutationTransport::Acknowledged(data) => data,
        };
        let acknowledgement = match validate_acknowledgement(request, data) {
            Ok(acknowledgement) => acknowledgement,
            Err(reason) => return dismissal_uncertain(&context, &mut *attempt, reason),
        };
        let encoded = match compact_terminal_acknowledgement(request, &acknowledgement) {
            Ok(encoded) => encoded,
            Err(error) => {
                return dismissal_uncertain(
                    &context,
                    &mut *attempt,
                    format!("could not encode the dismissal acknowledgement: {error}"),
                );
            }
        };
        if let Err(error) = attempt.record_terminal(&MutationTerminalRecord::Acknowledged {
            acknowledgement: encoded,
        }) {
            return ProviderMutationOutcome::Uncertain {
                context,
                reason: format!(
                    "GitHub acknowledged the dismissal but its durable terminal acknowledgement could not be saved; durable InFlight retained: {error}"
                ),
            };
        }
        ProviderMutationOutcome::Acknowledged(acknowledgement)
    }

    /// Observe current exact known-ID state without resolving message or
    /// causation. Callers must retain uncertainty unless separately exact event
    /// evidence accounts for the frozen message.
    pub fn reconcile_review_dismissal(
        &self,
        repo: &Repository,
        request: &SubmittedReviewDismissalRequest,
    ) -> ProviderReadEvidence<SubmittedReviewDismissalObservation> {
        if let Err(reason) = validate_request(self, repo, request) {
            return ProviderReadEvidence::Inconclusive { reason };
        }
        match Session::new(self).dismissal_observation(
            repo,
            request.target.pull_request.pull_request,
            &request.target.review.remote_id,
        ) {
            Ok(observation) => ProviderReadEvidence::Observed(observation),
            Err(error) => ProviderReadEvidence::Inconclusive {
                reason: error.to_string(),
            },
        }
    }
}

fn compact_terminal_acknowledgement(
    request: &SubmittedReviewDismissalRequest,
    acknowledgement: &SubmittedReviewDismissalAcknowledgement,
) -> std::result::Result<Value, serde_json::Error> {
    let request_bytes = serde_json::to_vec(request)?;
    let request_sha256 = format!("{:x}", Sha256::digest(request_bytes));
    Ok(json!({
        "operation_id": acknowledgement.operation_id.as_str(),
        "review_id": acknowledgement.target.review.remote_id.as_str(),
        "pull_request_id": acknowledgement.target.pull_request.remote_id.as_str(),
        "repository": acknowledgement.target.repository.full_name(),
        "viewer_node_id": acknowledgement.viewer.node_id.as_str(),
        "final_state": acknowledgement.final_state.as_str(),
        "frozen_request_sha256": request_sha256,
    }))
}

fn validate_displayed(
    provider: &GithubProvider,
    repo: &Repository,
    number: u64,
    displayed: &PullRequestReview,
) -> std::result::Result<crate::domain::FreshReviewDismissalCapability, String> {
    let capability = displayed.dismissal_capability.clone().ok_or_else(|| {
        "dismissal authority is cached, partial, or unknown; refresh Activity first".to_owned()
    })?;
    if !coordinates_match(repo, number, &displayed.coordinates)
        || !coordinates_match(repo, number, &capability.pull_request)
        || displayed.coordinates.remote_id.is_empty()
    {
        return Err("displayed dismissal target belongs to another repository or PR".into());
    }
    validate_node_id(&displayed.coordinates.remote_id).map_err(|error| error.to_string())?;
    validate_node_id(&capability.pull_request.remote_id).map_err(|error| error.to_string())?;
    validate_node_id(&capability.viewer.node_id).map_err(|error| error.to_string())?;
    if !capability
        .viewer
        .login
        .eq_ignore_ascii_case(&provider.account.login)
    {
        return Err("fresh dismissal viewer does not match the selected account".into());
    }
    if !matches!(displayed.state.as_str(), "APPROVED" | "CHANGES_REQUESTED")
        || displayed.submitted_at.as_deref().is_none_or(str::is_empty)
        || displayed.body.len() > MAX_DISMISSAL_SOURCE_BYTES
    {
        return Err("review is not an exact eligible submitted dismissal target".into());
    }
    if let Some(commit) = &displayed.commit_sha {
        validate_sha(commit).map_err(|error| error.to_string())?;
    }
    validate_author(displayed.author.as_deref())?;
    validate_authority(&capability.authority)?;
    if !capability.authority.permits_attempt() {
        return Err("fresh dismissal authority is unavailable; zero writes sent".into());
    }
    Ok(capability)
}

fn displayed_target(
    repo: &Repository,
    displayed: &PullRequestReview,
    pull_request: &crate::domain::ProviderCoordinates,
) -> std::result::Result<SubmittedReviewDismissalTarget, String> {
    Ok(SubmittedReviewDismissalTarget {
        repository: repo.clone(),
        pull_request: pull_request.clone(),
        review: displayed.coordinates.clone(),
        review_state: displayed.state.clone(),
        review_body: displayed.body.clone(),
        submitted_at: displayed
            .submitted_at
            .clone()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "submitted review omitted its submission time".to_owned())?,
        review_author: displayed.author.clone(),
        review_commit_sha: displayed.commit_sha.clone(),
    })
}

fn validate_request(
    provider: &GithubProvider,
    repo: &Repository,
    request: &SubmittedReviewDismissalRequest,
) -> std::result::Result<(), String> {
    provider
        .validate_repo(repo)
        .map_err(|error| error.to_string())?;
    validate_action_identity(&request.operation_id, "operation_id")?;
    validate_action_identity(&request.attempt_id, "attempt_id")?;
    validate_reason(&request.reason)?;
    let number = request.target.pull_request.pull_request;
    if request.target.repository != *repo
        || !coordinates_match(repo, number, &request.target.pull_request)
        || !coordinates_match(repo, number, &request.target.review)
        || !request
            .viewer
            .login
            .eq_ignore_ascii_case(&provider.account.login)
        || !matches!(
            request.target.review_state.as_str(),
            "APPROVED" | "CHANGES_REQUESTED"
        )
        || request.target.review_body.len() > MAX_DISMISSAL_SOURCE_BYTES
        || request.target.submitted_at.is_empty()
    {
        return Err("frozen dismissal request is invalid or belongs to another target".into());
    }
    validate_node_id(&request.viewer.node_id).map_err(|error| error.to_string())?;
    validate_node_id(&request.target.pull_request.remote_id).map_err(|error| error.to_string())?;
    validate_node_id(&request.target.review.remote_id).map_err(|error| error.to_string())?;
    validate_author(request.target.review_author.as_deref())?;
    if let Some(commit) = &request.target.review_commit_sha {
        validate_sha(commit).map_err(|error| error.to_string())?;
    }
    validate_authority(&request.authority)?;
    if !request.authority.permits_attempt() {
        return Err("frozen dismissal authority is unavailable".into());
    }
    Ok(())
}

fn validate_reason(reason: &str) -> std::result::Result<(), String> {
    if reason.trim().is_empty()
        || reason.len() > MAX_DISMISSAL_REASON_BYTES
        || reason.contains('\0')
    {
        return Err("dismissal reason is blank or exceeds its explicit bound".into());
    }
    Ok(())
}

fn validate_author(author: Option<&str>) -> std::result::Result<(), String> {
    if author.is_some_and(|value| {
        value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control)
    }) {
        return Err("dismissal review author is malformed".into());
    }
    Ok(())
}

fn validate_authority(authority: &DismissalAuthority) -> std::result::Result<(), String> {
    if authority.reason().is_some_and(|reason| {
        reason.is_empty() || reason.len() > 4096 || reason.chars().any(char::is_control)
    }) {
        return Err("dismissal authority explanation is malformed".into());
    }
    Ok(())
}

fn dismissal_not_started<A>(
    context: &MutationContext,
    attempt: &mut dyn super::AdmittedMutationAttempt,
    reason: String,
) -> ProviderMutationOutcome<A> {
    let record_error = attempt
        .record_terminal(&MutationTerminalRecord::NotStarted {
            reason: reason.clone(),
        })
        .err();
    if let Some(error) = record_error {
        ProviderMutationOutcome::Uncertain {
            context: context.clone(),
            reason: format!(
                "{reason}; dispatched zero writes, but durable NotStarted recording failed and InFlight authority was retained against replay: {error}"
            ),
        }
    } else {
        rejected(format!("{reason}; dispatched zero writes"))
    }
}

fn dismissal_uncertain<A>(
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
    fn dismissal_observation(
        &mut self,
        repo: &Repository,
        number: u64,
        review_id: &str,
    ) -> Result<SubmittedReviewDismissalObservation> {
        let response: GraphqlResult<DismissalTargetData> = self.graphql(
            DISMISSAL_TARGET_QUERY,
            json!({
                "owner": repo.owner,
                "name": repo.name,
                "number": number,
                "reviewId": review_id,
            }),
        )?;
        ensure!(
            !response.partial,
            "GitHub dismissal target read was partial"
        );
        let viewer = SelectedViewer {
            node_id: response.data.viewer.id,
            login: response.data.viewer.login,
        };
        validate_node_id(&viewer.node_id)?;
        ensure!(
            viewer
                .login
                .eq_ignore_ascii_case(&self.provider.account.login),
            "selected GitHub credential resolved to another account"
        );
        let repository = response
            .data
            .repository
            .context("GitHub dismissal repository is unavailable")?;
        ensure!(
            repository
                .name_with_owner
                .eq_ignore_ascii_case(&repo.full_name()),
            "GitHub dismissal repository mismatch"
        );
        let pull = repository
            .pull_request
            .context("GitHub dismissal pull request is unavailable")?;
        validate_node_id(&pull.id)?;
        ensure!(pull.number == number, "GitHub dismissal PR number mismatch");
        let review = response
            .data
            .node
            .context("GitHub dismissal review is unavailable")?;
        ensure!(
            review.typename == "PullRequestReview",
            "dismissal target has the wrong GraphQL type"
        );
        validate_node_id(&review.id)?;
        ensure!(
            review.id == review_id,
            "GitHub dismissal review ID mismatch"
        );
        validate_node_id(&review.pull_request.id)?;
        ensure!(
            review.pull_request.id == pull.id
                && review.pull_request.number == number
                && review
                    .pull_request
                    .repository
                    .name_with_owner
                    .eq_ignore_ascii_case(&repo.full_name()),
            "GitHub dismissal review parent mismatch"
        );
        let submitted_at = review
            .submitted_at
            .filter(|value| !value.is_empty())
            .context("dismissal review omitted its submission time")?;
        ensure!(
            review.body.len() <= MAX_DISMISSAL_SOURCE_BYTES,
            "dismissal review body exceeds its explicit bound"
        );
        let author = review
            .author
            .require_present("author")
            .map_err(anyhow::Error::msg)?
            .map(|actor| actor.login);
        validate_author(author.as_deref()).map_err(anyhow::Error::msg)?;
        let commit = review
            .commit
            .require_present("commit")
            .map_err(anyhow::Error::msg)?
            .map(|commit| commit.oid);
        if let Some(commit) = &commit {
            validate_sha(commit)?;
        }
        let pull_request = coordinates(repo, number, pull.id);
        let authority = dismissal_authority(&review.state, repository.viewer_can_administer);
        Ok(SubmittedReviewDismissalObservation {
            target: SubmittedReviewDismissalTarget {
                repository: repo.clone(),
                pull_request,
                review: coordinates(repo, number, review.id),
                review_state: review.state,
                review_body: review.body,
                submitted_at,
                review_author: author,
                review_commit_sha: commit,
            },
            viewer,
            authority,
        })
    }
}

struct PreparedDismissalMutation {
    variables: Value,
}

impl PreparedDismissalMutation {
    fn new(request: &SubmittedReviewDismissalRequest) -> Self {
        Self {
            variables: json!({
                "reviewId": request.target.review.remote_id,
                "message": request.reason,
                "clientMutationId": request.operation_id,
            }),
        }
    }

    fn context(&self, request: &SubmittedReviewDismissalRequest) -> MutationContext {
        MutationContext {
            operation_id: request.operation_id.clone(),
            attempt_id: request.attempt_id.clone(),
            action: "dismiss-submitted-review".into(),
            payload: json!({
                "request": request,
                "dispatch": {
                    "query": DISMISS_REVIEW_MUTATION,
                    "variables": self.variables,
                },
            }),
        }
    }

    fn dispatch(&self, provider: &GithubProvider) -> MutationTransport<DismissalMutationData> {
        #[cfg(feature = "ui-smoke")]
        if std::env::var_os("CIBERGIT_SMOKE_DISMISSAL").is_some() {
            return MutationTransport::Rejected(
                "native dismissal smoke hard-suppressed mutation transport before credential resolution"
                    .into(),
            );
        }
        Session::new(provider).graphql_mutation(DISMISS_REVIEW_MUTATION, self.variables.clone())
    }
}

fn validate_acknowledgement(
    request: &SubmittedReviewDismissalRequest,
    data: DismissalMutationData,
) -> std::result::Result<SubmittedReviewDismissalAcknowledgement, String> {
    let payload = data
        .dismiss_pull_request_review
        .ok_or_else(|| "GitHub omitted the dismissPullRequestReview acknowledgement".to_owned())?;
    if payload.client_mutation_id.as_deref() != Some(request.operation_id.as_str()) {
        return Err("GitHub echoed another dismissal operation ID".into());
    }
    let review = payload
        .pull_request_review
        .ok_or_else(|| "GitHub omitted the dismissed review".to_owned())?;
    if review.typename != "PullRequestReview" {
        return Err("GitHub returned another GraphQL type for the dismissed review".into());
    }
    validate_node_id(&review.id).map_err(|error| error.to_string())?;
    validate_node_id(&review.pull_request.id).map_err(|error| error.to_string())?;
    let author = review
        .author
        .require_present("author")?
        .map(|actor| actor.login);
    validate_author(author.as_deref())?;
    let commit = review
        .commit
        .require_present("commit")?
        .map(|commit| commit.oid);
    if let Some(commit) = &commit {
        validate_sha(commit).map_err(|error| error.to_string())?;
    }
    if review.id != request.target.review.remote_id
        || review.state != "DISMISSED"
        || review.body != request.target.review_body
        || review.submitted_at.as_deref() != Some(request.target.submitted_at.as_str())
        || author != request.target.review_author
        || commit != request.target.review_commit_sha
        || review.pull_request.id != request.target.pull_request.remote_id
        || review.pull_request.number != request.target.pull_request.pull_request
        || !review
            .pull_request
            .repository
            .name_with_owner
            .eq_ignore_ascii_case(&request.target.repository.full_name())
    {
        return Err(
            "GitHub dismissal acknowledgement did not match the exact frozen review tuple".into(),
        );
    }
    Ok(SubmittedReviewDismissalAcknowledgement {
        operation_id: request.operation_id.clone(),
        target: request.target.clone(),
        viewer: request.viewer.clone(),
        final_state: review.state,
    })
}

#[derive(Debug)]
enum RequestedNullable<T> {
    Missing,
    Null,
    Value(T),
}

impl<T> Default for RequestedNullable<T> {
    fn default() -> Self {
        Self::Missing
    }
}

impl<T> RequestedNullable<T> {
    fn require_present(self, field: &str) -> std::result::Result<Option<T>, String> {
        match self {
            Self::Missing => Err(format!(
                "GitHub omitted requested dismissal {field} metadata"
            )),
            Self::Null => Ok(None),
            Self::Value(value) => Ok(Some(value)),
        }
    }
}

fn deserialize_requested_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<RequestedNullable<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(|value| match value {
        Some(value) => RequestedNullable::Value(value),
        None => RequestedNullable::Null,
    })
}

#[derive(Deserialize)]
struct DismissalTargetData {
    viewer: DismissalActor,
    repository: Option<DismissalRepository>,
    node: Option<DismissalReview>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DismissalRepository {
    name_with_owner: String,
    viewer_can_administer: bool,
    pull_request: Option<DismissalPull>,
}

#[derive(Deserialize)]
struct DismissalPull {
    id: String,
    number: u64,
}

#[derive(Deserialize)]
struct DismissalActor {
    id: String,
    login: String,
}

#[derive(Deserialize)]
struct DismissalLogin {
    login: String,
}

#[derive(Deserialize)]
struct DismissalCommit {
    oid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DismissalReview {
    #[serde(rename = "__typename")]
    typename: String,
    id: String,
    body: String,
    state: String,
    submitted_at: Option<String>,
    #[serde(default, deserialize_with = "deserialize_requested_nullable")]
    author: RequestedNullable<DismissalLogin>,
    #[serde(default, deserialize_with = "deserialize_requested_nullable")]
    commit: RequestedNullable<DismissalCommit>,
    pull_request: DismissalReviewPull,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DismissalReviewPull {
    id: String,
    number: u64,
    repository: DismissalReviewRepository,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DismissalReviewRepository {
    name_with_owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DismissalMutationData {
    dismiss_pull_request_review: Option<DismissalPayload>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DismissalPayload {
    client_mutation_id: Option<String>,
    pull_request_review: Option<DismissalReview>,
}

const DISMISSAL_TARGET_QUERY: &str = r#"query SubmittedReviewDismissalTarget(
  $owner: String!, $name: String!, $number: Int!, $reviewId: ID!
) {
  viewer { id login }
  repository(owner: $owner, name: $name) {
    nameWithOwner viewerCanAdminister
    pullRequest(number: $number) { id number }
  }
  node(id: $reviewId) {
    __typename
    ... on PullRequestReview {
      id body state submittedAt author { login } commit { oid }
      pullRequest { id number repository { nameWithOwner } }
    }
  }
}"#;

const DISMISS_REVIEW_MUTATION: &str = r#"mutation DismissSubmittedReview(
  $reviewId: ID!, $message: String!, $clientMutationId: String!
) {
  dismissPullRequestReview(input: {
    pullRequestReviewId: $reviewId, message: $message,
    clientMutationId: $clientMutationId
  }) {
    clientMutationId
    pullRequestReview {
      __typename id body state submittedAt author { login } commit { oid }
      pullRequest { id number repository { nameWithOwner } }
    }
  }
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_nullable_distinguishes_omitted_null_and_value() {
        #[derive(Deserialize)]
        struct Witness {
            #[serde(default, deserialize_with = "deserialize_requested_nullable")]
            value: RequestedNullable<String>,
        }

        assert!(matches!(
            serde_json::from_value::<Witness>(json!({})).unwrap().value,
            RequestedNullable::Missing
        ));
        assert!(matches!(
            serde_json::from_value::<Witness>(json!({"value": null}))
                .unwrap()
                .value,
            RequestedNullable::Null
        ));
        assert!(matches!(
            serde_json::from_value::<Witness>(json!({"value": "x"}))
                .unwrap()
                .value,
            RequestedNullable::Value(value) if value == "x"
        ));
    }
}
