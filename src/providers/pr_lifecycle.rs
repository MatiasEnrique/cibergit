use super::{
    GithubProvider, GraphqlConnection, GraphqlResult, MutationTransport, PAGE_SIZE, Session,
    coordinates, coordinates_match, rejected, validate_action_identity, validate_component,
    validate_node_id, validate_ref_name, validate_sha,
};
use crate::domain::{
    IssueComment, MutationAdmissionReceipt, MutationContext, MutationTerminalRecord,
    ProviderCapability, ProviderChoice, ProviderChoiceSet, ProviderCoordinates,
    ProviderMutationOutcome, ProviderReadEvidence, PullRequestCreationAcknowledgement,
    PullRequestCreationInput, PullRequestCreationPreparation, PullRequestCreationRequest,
    PullRequestDiscussionAcknowledgement, PullRequestDiscussionAction,
    PullRequestDiscussionRequest, PullRequestLifecycleAcknowledgement, PullRequestLifecycleAction,
    PullRequestLifecycleChoices, PullRequestLifecycleRequest, PullRequestLifecycleSnapshot,
    PullRequestMutationTarget, PullRequestReviewer, Repository,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

const CHOICE_PAGE_LIMIT: usize = 10;
const MAX_TITLE_BYTES: usize = 256 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_IDENTITY_BYTES: usize = 100;

/// Durable authority for exactly one admitted provider mutation. Implementors
/// must hold per-target cross-process exclusivity until `record_terminal`
/// succeeds. Dropping a guard without a terminal record must retain InFlight
/// authority; it must never make the attempt replayable. An Uncertain terminal
/// record remains a no-replay/pending-reconciliation authority record.
pub trait AdmittedMutationAttempt {
    fn receipt(&self) -> &MutationAdmissionReceipt;
    fn record_terminal(&mut self, record: &MutationTerminalRecord) -> Result<()>;
}

/// Caller-owned durable admission. An implementation must refuse a replayed
/// attempt and any other pending attempt for the same exact target. A durable
/// terminal tombstone must continue to reject reuse of an operation/attempt ID.
pub trait MutationAdmission {
    fn admit<'a>(
        &'a mut self,
        context: &MutationContext,
    ) -> Result<Box<dyn AdmittedMutationAttempt + 'a>>;
}

impl GithubProvider {
    /// Read current PR metadata and action capabilities for the selected
    /// account. This explicit read is separate from sidebar list hydration.
    pub fn pr_lifecycle_snapshot(
        &self,
        repo: &Repository,
        number: u64,
    ) -> Result<PullRequestLifecycleSnapshot> {
        self.validate_repo(repo)?;
        ensure!(number > 0 && number <= i32::MAX as u64, "Invalid PR number");
        Session::new(self).lifecycle_snapshot(repo, number)
    }

    /// Read bounded repository-wide picker values. A collection that cannot be
    /// read is returned explicitly incomplete without hiding other choices.
    pub fn pr_lifecycle_choices(&self, repo: &Repository) -> Result<PullRequestLifecycleChoices> {
        self.validate_repo(repo)?;
        let mut session = Session::new(self);
        let prefix = format!("repos/{}", repo.full_name());
        Ok(PullRequestLifecycleChoices {
            repository: repo.clone(),
            branches: read_choice_pages(
                &mut session,
                "branches",
                |page| format!("{prefix}/branches?per_page={PAGE_SIZE}&page={page}"),
                |value: ApiBranchChoice| ProviderChoice {
                    remote_id: None,
                    name: value.name,
                },
            ),
            labels: read_choice_pages(
                &mut session,
                "labels",
                |page| format!("{prefix}/labels?per_page={PAGE_SIZE}&page={page}"),
                |value: ApiNamedChoice| ProviderChoice {
                    remote_id: value.node_id,
                    name: value.name,
                },
            ),
            assignees: read_choice_pages(
                &mut session,
                "assignees",
                |page| format!("{prefix}/assignees?per_page={PAGE_SIZE}&page={page}"),
                |value: ApiLoginChoice| ProviderChoice {
                    remote_id: value.node_id,
                    name: value.login,
                },
            ),
            reviewer_users: read_choice_pages(
                &mut session,
                "reviewer users",
                |page| {
                    format!(
                        "{prefix}/collaborators?affiliation=all&per_page={PAGE_SIZE}&page={page}"
                    )
                },
                |value: ApiLoginChoice| ProviderChoice {
                    remote_id: value.node_id,
                    name: value.login,
                },
            ),
            reviewer_teams: read_choice_pages(
                &mut session,
                "reviewer teams",
                |page| format!("{prefix}/teams?per_page={PAGE_SIZE}&page={page}"),
                |value: ApiTeamChoice| ProviderChoice {
                    remote_id: value.node_id,
                    name: value.slug,
                },
            ),
        })
    }

    pub fn execute_pr_lifecycle(
        &self,
        repo: &Repository,
        request: &PullRequestLifecycleRequest,
        admission: &mut impl MutationAdmission,
    ) -> ProviderMutationOutcome<PullRequestLifecycleAcknowledgement> {
        if let Err(reason) = validate_request_identity(
            self,
            repo,
            &request.operation_id,
            &request.attempt_id,
            &request.target,
        ) {
            return rejected(reason);
        }
        let initial =
            match self.pr_lifecycle_snapshot(repo, request.target.pull_request.pull_request) {
                Ok(value) => value,
                Err(error) => return rejected(error.to_string()),
            };
        if let Err(reason) = validate_lifecycle_action(request, &initial) {
            return rejected(reason);
        }
        let prepared = match prepare_lifecycle_mutation(repo, request, &initial) {
            Ok(value) => value,
            Err(reason) => return rejected(reason),
        };
        let context = prepared.context(request);
        execute_admitted(
            &context,
            admission,
            || {
                let fresh = self
                    .pr_lifecycle_snapshot(repo, request.target.pull_request.pull_request)
                    .map_err(|error| error.to_string())?;
                validate_lifecycle_action(request, &fresh)
            },
            || prepared.dispatch(self),
            |raw| {
                raw.validate(&prepared, repo, &request.action)?;
                let observed = self
                    .pr_lifecycle_snapshot(repo, request.target.pull_request.pull_request)
                    .map_err(|error| {
                        format!(
                            "GitHub acknowledged the PR action but its result could not be reconciled: {error}"
                        )
                    })?;
                validate_lifecycle_result(&request.action, &observed)?;
                Ok(PullRequestLifecycleAcknowledgement {
                    operation_id: request.operation_id.clone(),
                    repository: observed.repository,
                    pull_request: observed.pull_request,
                    updated_at: observed.updated_at,
                    state: observed.state,
                    head_sha: observed.head_sha,
                })
            },
        )
    }

    pub fn execute_pr_discussion(
        &self,
        repo: &Repository,
        request: &PullRequestDiscussionRequest,
        admission: &mut impl MutationAdmission,
    ) -> ProviderMutationOutcome<PullRequestDiscussionAcknowledgement> {
        if let Err(reason) = validate_request_identity(
            self,
            repo,
            &request.operation_id,
            &request.attempt_id,
            &request.target,
        ) {
            return rejected(reason);
        }
        let initial =
            match self.pr_lifecycle_snapshot(repo, request.target.pull_request.pull_request) {
                Ok(value) => value,
                Err(error) => return rejected(error.to_string()),
            };
        let initial_comment = match preflight_discussion(self, repo, request, &initial) {
            Ok(value) => value,
            Err(reason) => return rejected(reason),
        };
        let prepared =
            match prepare_discussion_mutation(request, &initial, initial_comment.as_ref()) {
                Ok(value) => value,
                Err(reason) => return rejected(reason),
            };
        let context = prepared.context(request);
        execute_admitted(
            &context,
            admission,
            || {
                let fresh = self
                    .pr_lifecycle_snapshot(repo, request.target.pull_request.pull_request)
                    .map_err(|error| error.to_string())?;
                preflight_discussion(self, repo, request, &fresh).map(|_| ())
            },
            || prepared.dispatch(self),
            |data| validate_discussion_ack(repo, request, data),
        )
    }

    /// Read exact known-ID evidence for top-level PR comment reconciliation.
    /// Missing/inaccessible/wrong-type IDs stay inconclusive.
    pub fn reconcile_pr_comment(
        &self,
        repo: &Repository,
        number: u64,
        comment: &ProviderCoordinates,
    ) -> ProviderReadEvidence<IssueComment> {
        if self.validate_repo(repo).is_err() || !coordinates_match(repo, number, comment) {
            return ProviderReadEvidence::Inconclusive {
                reason: "comment reconciliation coordinates do not match the selected target"
                    .into(),
            };
        }
        match Session::new(self).top_level_comment(repo, number, &comment.remote_id) {
            Ok(value) => ProviderReadEvidence::Observed(value.into_domain(repo, number)),
            Err(error) => ProviderReadEvidence::Inconclusive {
                reason: format!(
                    "exact comment is absent, inaccessible, or unavailable; absence is not proof of non-application: {error}"
                ),
            },
        }
    }

    pub fn prepare_pr_creation(
        &self,
        input: &PullRequestCreationInput,
    ) -> Result<PullRequestCreationPreparation> {
        self.validate_repo(&input.target_repository)?;
        self.validate_repo(&input.source_repository)?;
        validate_creation_text(input)?;
        let mut session = Session::new(self);
        let viewer: ApiLoginChoice = session.get("user")?;
        ensure!(
            viewer.login.eq_ignore_ascii_case(&self.account.login),
            "selected GitHub credential resolved to another account"
        );
        let target: ApiCreationRepository =
            session.get(&format!("repos/{}", input.target_repository.full_name()))?;
        target.validate(&input.target_repository)?;
        let source: ApiCreationRepository =
            session.get(&format!("repos/{}", input.source_repository.full_name()))?;
        source.validate(&input.source_repository)?;
        ensure!(
            same_repository(&input.source_repository, &input.target_repository)
                || source.parent.as_ref().is_some_and(|value| value
                    .full_name
                    .eq_ignore_ascii_case(&input.target_repository.full_name()))
                || creation_network_root(&source)
                    .eq_ignore_ascii_case(creation_network_root(&target)),
            "source repository is not the target or a verified fork in its network"
        );
        let base = session.published_ref(&input.target_repository, &input.base_branch)?;
        let head = session.published_ref(&input.source_repository, &input.source_branch)?;
        let permission = target.permission_name();
        let can_create = target.permissions.as_ref().is_some_and(|value| value.pull)
            && source.permissions.as_ref().is_some_and(|value| value.push);
        Ok(PullRequestCreationPreparation {
            input: input.clone(),
            observed_base_sha: base,
            observed_source_head_sha: head,
            viewer_login: viewer.login,
            repository_permission: permission,
            can_create: capability(
                can_create,
                if can_create {
                    None
                } else {
                    Some(
                        "selected account cannot read the target or write the published source branch",
                    )
                },
            ),
            reviewed_head_atomically_enforced: false,
            notice: Some(
                "GitHub PR creation has no expected-head condition. The published source head is re-read under admission, but creation cannot atomically enforce that reviewed SHA."
                    .into(),
            ),
        })
    }

    pub fn execute_pr_creation(
        &self,
        request: &PullRequestCreationRequest,
        admission: &mut impl MutationAdmission,
    ) -> ProviderMutationOutcome<PullRequestCreationAcknowledgement> {
        if let Err(reason) = validate_action_identity(&request.operation_id, "operation_id")
            .and_then(|_| validate_action_identity(&request.attempt_id, "attempt_id"))
        {
            return rejected(reason);
        }
        let target = &request.preparation.input.target_repository;
        if let Err(error) = self.validate_repo(target) {
            return rejected(error.to_string());
        }
        if !request.preparation.can_create.available
            || request.preparation.reviewed_head_atomically_enforced
        {
            return rejected("invalid or unavailable PR creation preparation");
        }
        let initial = match self.prepare_pr_creation(&request.preparation.input) {
            Ok(value) => value,
            Err(error) => return rejected(error.to_string()),
        };
        if let Err(reason) = validate_creation_fresh(&request.preparation, &initial) {
            return rejected(reason);
        }
        let prepared = PreparedCreationMutation::new(request);
        let context = prepared.context(request);
        execute_admitted(
            &context,
            admission,
            || {
                let fresh = self
                    .prepare_pr_creation(&request.preparation.input)
                    .map_err(|error| error.to_string())?;
                validate_creation_fresh(&request.preparation, &fresh)
            },
            || prepared.dispatch(self),
            |response| validate_creation_ack(request, response),
        )
    }
}

fn execute_admitted<T, A>(
    context: &MutationContext,
    admission: &mut impl MutationAdmission,
    revalidate: impl FnOnce() -> std::result::Result<(), String>,
    dispatch: impl FnOnce() -> MutationTransport<T>,
    acknowledge: impl FnOnce(T) -> std::result::Result<A, String>,
) -> ProviderMutationOutcome<A>
where
    A: Serialize,
{
    let mut attempt = match admission.admit(context) {
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
        let reason = "durable admission receipt does not match the frozen attempt".to_owned();
        let record_error = attempt
            .record_terminal(&MutationTerminalRecord::NotStarted {
                reason: reason.clone(),
            })
            .err();
        return rejected(if let Some(error) = record_error {
            format!(
                "{reason}; dispatched zero writes; durable InFlight retained because NotStarted recording failed: {error}"
            )
        } else {
            format!("{reason}; dispatched zero writes")
        });
    }
    if let Err(reason) = revalidate() {
        let record_error = attempt
            .record_terminal(&MutationTerminalRecord::NotStarted {
                reason: reason.clone(),
            })
            .err();
        return rejected(if let Some(error) = record_error {
            format!(
                "post-admission preflight refused the write: {reason}; dispatched zero writes; durable InFlight retained because NotStarted recording failed: {error}"
            )
        } else {
            format!("post-admission preflight refused the write: {reason}; dispatched zero writes")
        });
    }
    let value = match dispatch() {
        MutationTransport::Rejected(reason) => {
            let record_error = attempt
                .record_terminal(&MutationTerminalRecord::NotStarted {
                    reason: reason.clone(),
                })
                .err();
            return rejected(if let Some(error) = record_error {
                format!(
                    "{reason}; dispatched zero writes; durable InFlight retained because NotStarted recording failed: {error}"
                )
            } else {
                format!("{reason}; dispatched zero writes")
            });
        }
        MutationTransport::Uncertain(reason) => {
            return uncertain_with_record(context, &mut *attempt, reason);
        }
        MutationTransport::Acknowledged(value) => value,
    };
    let acknowledgement = match acknowledge(value) {
        Ok(value) => value,
        Err(reason) => return uncertain_with_record(context, &mut *attempt, reason),
    };
    let encoded = match serde_json::to_value(&acknowledgement) {
        Ok(value) => value,
        Err(error) => {
            return uncertain_with_record(
                context,
                &mut *attempt,
                format!("could not encode the provider acknowledgement: {error}"),
            );
        }
    };
    if let Err(error) = attempt.record_terminal(&MutationTerminalRecord::Acknowledged {
        acknowledgement: encoded,
    }) {
        return ProviderMutationOutcome::Uncertain {
            context: context.clone(),
            reason: format!(
                "GitHub acknowledged the write but its durable terminal acknowledgement could not be saved; durable InFlight retained: {error}"
            ),
        };
    }
    ProviderMutationOutcome::Acknowledged(acknowledgement)
}

fn uncertain_with_record<A>(
    context: &MutationContext,
    attempt: &mut dyn AdmittedMutationAttempt,
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
    fn lifecycle_snapshot(
        &mut self,
        repo: &Repository,
        number: u64,
    ) -> Result<PullRequestLifecycleSnapshot> {
        let response: GraphqlResult<LifecycleData> = self.graphql(
            LIFECYCLE_QUERY,
            json!({"owner": repo.owner, "name": repo.name, "number": number}),
        )?;
        let partial = response.partial;
        ensure!(
            response
                .data
                .viewer
                .login
                .eq_ignore_ascii_case(&self.provider.account.login),
            "selected GitHub credential resolved to another account"
        );
        let repository = response
            .data
            .repository
            .context("GitHub lifecycle repository is unavailable")?;
        ensure!(
            repository
                .name_with_owner
                .eq_ignore_ascii_case(&repo.full_name()),
            "GitHub lifecycle repository mismatch"
        );
        let pull = repository
            .pull_request
            .context("GitHub lifecycle pull request is unavailable")?;
        pull.validate(repo, number)?;
        let values_complete = !partial
            && connection_complete(&pull.review_requests)
            && connection_complete(&pull.assignees)
            && connection_complete(&pull.labels);
        let mut notices = Vec::new();
        if partial {
            notices.push("GitHub returned partial lifecycle fields.".to_owned());
        }
        if !connection_complete(&pull.review_requests) {
            notices.push("Requested reviewers are partial at the explicit 100-item bound.".into());
        }
        if !connection_complete(&pull.assignees) {
            notices.push("Assignees are partial at the explicit 100-item bound.".into());
        }
        if !connection_complete(&pull.labels) {
            notices.push("Labels are partial at the explicit 100-item bound.".into());
        }
        let reviewers = pull
            .review_requests
            .nodes
            .iter()
            .flatten()
            .filter_map(|request| request.requested_reviewer.as_ref())
            .filter_map(|reviewer| {
                reviewer
                    .login
                    .as_ref()
                    .map(|name| PullRequestReviewer {
                        kind: "USER".into(),
                        name: name.clone(),
                    })
                    .or_else(|| {
                        reviewer.slug.as_ref().map(|name| PullRequestReviewer {
                            kind: "TEAM".into(),
                            name: name.clone(),
                        })
                    })
            })
            .collect();
        let write_permission = matches!(
            repository.viewer_permission.as_deref(),
            Some("WRITE" | "MAINTAIN" | "ADMIN")
        );
        let open = pull.state == "OPEN";
        let metadata = capability(
            pull.viewer_can_update,
            (!pull.viewer_can_update).then_some("selected account cannot update this pull request"),
        );
        let change_state = match pull.state.as_str() {
            "OPEN" => capability(
                pull.viewer_can_close,
                (!pull.viewer_can_close)
                    .then_some("selected account cannot close this pull request"),
            ),
            "CLOSED" => capability(
                pull.viewer_can_reopen,
                (!pull.viewer_can_reopen)
                    .then_some("selected account cannot reopen this pull request"),
            ),
            _ => capability(false, Some("merged pull requests cannot be reopened")),
        };
        Ok(PullRequestLifecycleSnapshot {
            repository: repo.clone(),
            pull_request: coordinates(repo, number, pull.id),
            updated_at: pull.updated_at,
            state: pull.state,
            head_sha: pull.head_ref_oid,
            title: pull.title,
            body: pull.body,
            base_branch: pull.base_ref_name,
            draft: pull.is_draft,
            reviewers,
            assignees: pull
                .assignees
                .nodes
                .into_iter()
                .flatten()
                .map(|value| value.login)
                .collect(),
            labels: pull
                .labels
                .nodes
                .into_iter()
                .flatten()
                .map(|value| value.name)
                .collect(),
            viewer_login: response.data.viewer.login,
            viewer_permission: repository.viewer_permission,
            can_update_metadata: metadata,
            can_change_state: change_state,
            can_change_draft: capability(
                open && pull.viewer_can_update,
                (!open)
                    .then_some("only open pull requests can change draft state")
                    .or_else(|| {
                        (!pull.viewer_can_update)
                            .then_some("selected account cannot update this pull request")
                    }),
            ),
            can_request_reviewers: capability(
                open && write_permission,
                (!open)
                    .then_some("reviewers can only be requested on an open pull request")
                    .or_else(|| {
                        (!write_permission)
                            .then_some("selected account lacks pull-request write permission")
                    }),
            ),
            can_change_labels: capability(
                pull.viewer_can_label,
                (!pull.viewer_can_label)
                    .then_some("selected account cannot edit labels on this pull request"),
            ),
            can_change_assignees: capability(
                pull.viewer_can_assign,
                (!pull.viewer_can_assign)
                    .then_some("selected account cannot edit assignees on this pull request"),
            ),
            can_comment: capability(
                !pull.locked || pull.viewer_can_update,
                (pull.locked && !pull.viewer_can_update)
                    .then_some("pull request discussion is locked for the selected account"),
            ),
            values_complete,
            capabilities_complete: !partial,
            notice: (!notices.is_empty()).then(|| notices.join(" ")),
        })
    }

    fn top_level_comment(
        &mut self,
        repo: &Repository,
        number: u64,
        id: &str,
    ) -> Result<TopLevelComment> {
        validate_node_id(id)?;
        let response: GraphqlResult<CommentNodeData> =
            self.graphql(COMMENT_NODE_QUERY, json!({"id": id}))?;
        ensure!(
            !response.partial,
            "GitHub comment identity preflight was partial"
        );
        ensure!(
            response
                .data
                .viewer
                .login
                .eq_ignore_ascii_case(&self.provider.account.login),
            "selected GitHub credential resolved to another account"
        );
        let comment = response
            .data
            .node
            .context("GitHub issue comment ID is unavailable or has the wrong type")?;
        comment.validate(repo, number, id)?;
        Ok(comment)
    }

    fn published_ref(&mut self, repo: &Repository, branch: &str) -> Result<String> {
        validate_branch_name(branch).map_err(anyhow::Error::msg)?;
        let endpoint = format!(
            "repos/{}/git/ref/heads/{}",
            repo.full_name(),
            url_component(branch)
        );
        let value: ApiPublishedRef = self.get(&endpoint)?;
        ensure!(
            value.reference == format!("refs/heads/{branch}"),
            "GitHub returned a different published branch"
        );
        validate_sha(&value.object.sha)?;
        Ok(value.object.sha)
    }
}

const LIFECYCLE_QUERY: &str = r#"query PullRequestLifecycle(
  $owner: String!, $name: String!, $number: Int!
) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    nameWithOwner viewerPermission
    pullRequest(number: $number) {
      id number url updatedAt state headRefOid title body baseRefName isDraft locked
      viewerCanUpdate viewerCanClose viewerCanReopen viewerCanLabel viewerCanAssign
      reviewRequests(first: 100) {
        nodes { requestedReviewer { ... on User { login } ... on Team { slug } } }
        pageInfo { hasNextPage endCursor }
      }
      assignees(first: 100) { nodes { login } pageInfo { hasNextPage endCursor } }
      labels(first: 100) { nodes { name } pageInfo { hasNextPage endCursor } }
    }
  }
}"#;

#[derive(Deserialize)]
struct LifecycleData {
    viewer: LifecycleActor,
    repository: Option<LifecycleRepository>,
}

#[derive(Clone, Deserialize)]
struct LifecycleActor {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleRepository {
    name_with_owner: String,
    viewer_permission: Option<String>,
    pull_request: Option<LifecyclePull>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LifecyclePull {
    id: String,
    number: u64,
    url: String,
    updated_at: String,
    state: String,
    head_ref_oid: String,
    title: String,
    body: String,
    base_ref_name: String,
    is_draft: bool,
    locked: bool,
    viewer_can_update: bool,
    viewer_can_close: bool,
    viewer_can_reopen: bool,
    viewer_can_label: bool,
    viewer_can_assign: bool,
    review_requests: GraphqlConnection<LifecycleReviewRequest>,
    assignees: GraphqlConnection<LifecycleActor>,
    labels: GraphqlConnection<LifecycleLabel>,
}

impl LifecyclePull {
    fn validate(&self, repo: &Repository, number: u64) -> Result<()> {
        ensure!(
            self.number == number && pull_url_matches(&self.url, repo, number),
            "GitHub lifecycle pull request mismatch"
        );
        validate_node_id(&self.id)?;
        validate_sha(&self.head_ref_oid)?;
        ensure!(
            matches!(self.state.as_str(), "OPEN" | "CLOSED" | "MERGED"),
            "GitHub returned an invalid pull request state"
        );
        validate_branch_name(&self.base_ref_name).map_err(anyhow::Error::msg)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleReviewRequest {
    requested_reviewer: Option<LifecycleReviewer>,
}

#[derive(Deserialize)]
struct LifecycleReviewer {
    login: Option<String>,
    slug: Option<String>,
}

#[derive(Deserialize)]
struct LifecycleLabel {
    name: String,
}

fn connection_complete<T>(value: &GraphqlConnection<T>) -> bool {
    !value.page_info.has_next_page && value.nodes.iter().all(Option::is_some)
}

fn capability(available: bool, reason: Option<&str>) -> ProviderCapability {
    ProviderCapability {
        available,
        reason: (!available).then(|| {
            reason
                .unwrap_or("provider capability is unavailable")
                .into()
        }),
    }
}

#[derive(Deserialize)]
struct ApiBranchChoice {
    name: String,
}

#[derive(Deserialize)]
struct ApiNamedChoice {
    name: String,
    node_id: Option<String>,
}

#[derive(Deserialize)]
struct ApiLoginChoice {
    login: String,
    node_id: Option<String>,
}

#[derive(Deserialize)]
struct ApiTeamChoice {
    slug: String,
    node_id: Option<String>,
}

fn read_choice_pages<T, F, M>(
    session: &mut Session<'_>,
    label: &str,
    endpoint: F,
    map: M,
) -> ProviderChoiceSet
where
    T: for<'de> Deserialize<'de>,
    F: Fn(usize) -> String,
    M: Fn(T) -> ProviderChoice,
{
    let mut values = Vec::new();
    let mut names = HashSet::new();
    for page in 1..=CHOICE_PAGE_LIMIT {
        let batch: Vec<T> = match session.get(&endpoint(page)) {
            Ok(value) => value,
            Err(error) => {
                return ProviderChoiceSet {
                    values,
                    complete: false,
                    notice: Some(format!(
                        "Available {label} could not be read completely: {error}"
                    )),
                };
            }
        };
        if batch.len() > PAGE_SIZE {
            return ProviderChoiceSet {
                values,
                complete: false,
                notice: Some(format!("Available {label} returned an invalid page")),
            };
        }
        let last = batch.len() < PAGE_SIZE;
        for raw in batch {
            let value = map(raw);
            if value.name.is_empty()
                || value.name.len() > 1024
                || value.name.chars().any(char::is_control)
                || !names.insert(value.name.to_ascii_lowercase())
            {
                return ProviderChoiceSet {
                    values,
                    complete: false,
                    notice: Some(format!(
                        "Available {label} contained invalid or duplicate values"
                    )),
                };
            }
            values.push(value);
        }
        if last {
            return ProviderChoiceSet {
                values,
                complete: true,
                notice: None,
            };
        }
    }
    ProviderChoiceSet {
        values,
        complete: false,
        notice: Some(format!(
            "Available {label} reached the explicit {}-item bound",
            CHOICE_PAGE_LIMIT * PAGE_SIZE
        )),
    }
}

fn validate_request_identity(
    provider: &GithubProvider,
    repo: &Repository,
    operation_id: &str,
    attempt_id: &str,
    target: &PullRequestMutationTarget,
) -> std::result::Result<(), String> {
    provider
        .validate_repo(repo)
        .map_err(|error| error.to_string())?;
    validate_action_identity(operation_id, "operation_id")?;
    validate_action_identity(attempt_id, "attempt_id")?;
    if !coordinates_match(repo, target.pull_request.pull_request, &target.pull_request) {
        return Err("mutation target belongs to another provider, repository, or PR".into());
    }
    if !same_repository(repo, &target.repository) {
        return Err("mutation target belongs to another selected account or repository".into());
    }
    validate_node_id(&target.pull_request.remote_id).map_err(|error| error.to_string())?;
    validate_sha(&target.observed_head_sha).map_err(|error| error.to_string())?;
    if !matches!(target.observed_state.as_str(), "OPEN" | "CLOSED" | "MERGED") {
        return Err("invalid observed pull request state".into());
    }
    if target.observed_updated_at.is_empty()
        || target.observed_updated_at.len() > 128
        || target.observed_updated_at.chars().any(char::is_control)
    {
        return Err("invalid observed pull request update time".into());
    }
    Ok(())
}

fn validate_snapshot_target(
    target: &PullRequestMutationTarget,
    fresh: &PullRequestLifecycleSnapshot,
) -> std::result::Result<(), String> {
    if fresh.pull_request != target.pull_request {
        return Err("fresh preflight resolved a different pull request node".into());
    }
    if !same_repository(&fresh.repository, &target.repository) {
        return Err("fresh preflight resolved another selected account or repository".into());
    }
    if fresh.viewer_login.is_empty() {
        return Err("fresh preflight omitted the selected account".into());
    }
    Ok(())
}

fn validate_lifecycle_action(
    request: &PullRequestLifecycleRequest,
    fresh: &PullRequestLifecycleSnapshot,
) -> std::result::Result<(), String> {
    validate_snapshot_target(&request.target, fresh)?;
    let unavailable = |capability: &ProviderCapability| {
        capability
            .reason
            .clone()
            .unwrap_or_else(|| "provider capability is unavailable".into())
    };
    match &request.action {
        PullRequestLifecycleAction::UpdateTitle { observed, value } => {
            validate_title(value)?;
            if observed != &fresh.title {
                return Err("pull request title changed since confirmation".into());
            }
            if value == observed {
                return Err("new pull request title is unchanged".into());
            }
            if !fresh.can_update_metadata.available {
                return Err(unavailable(&fresh.can_update_metadata));
            }
        }
        PullRequestLifecycleAction::UpdateBody { observed, value } => {
            validate_body(value, true)?;
            if observed != &fresh.body {
                return Err("pull request body changed since confirmation".into());
            }
            if value == observed {
                return Err("new pull request body is unchanged".into());
            }
            if !fresh.can_update_metadata.available {
                return Err(unavailable(&fresh.can_update_metadata));
            }
        }
        PullRequestLifecycleAction::UpdateBaseBranch { observed, value } => {
            validate_branch_name(value)?;
            if observed != &fresh.base_branch {
                return Err("pull request base branch changed since confirmation".into());
            }
            if value == observed {
                return Err("new pull request base branch is unchanged".into());
            }
            if fresh.state != "OPEN" {
                return Err("only an open pull request can change base branch".into());
            }
            if !fresh.can_update_metadata.available {
                return Err(unavailable(&fresh.can_update_metadata));
            }
        }
        PullRequestLifecycleAction::Close => {
            if request.target.observed_state != "OPEN" || fresh.state != "OPEN" {
                return Err("only a freshly observed open pull request can be closed".into());
            }
            if !fresh.can_change_state.available {
                return Err(unavailable(&fresh.can_change_state));
            }
        }
        PullRequestLifecycleAction::Reopen => {
            if request.target.observed_state != "CLOSED" || fresh.state != "CLOSED" {
                return Err(
                    "only a freshly observed closed, unmerged pull request can be reopened".into(),
                );
            }
            if !fresh.can_change_state.available {
                return Err(unavailable(&fresh.can_change_state));
            }
        }
        PullRequestLifecycleAction::ConvertToDraft => {
            if request.target.observed_state != "OPEN" || fresh.state != "OPEN" || fresh.draft {
                return Err(
                    "only a freshly observed open ready pull request can become draft".into(),
                );
            }
            if !fresh.can_change_draft.available {
                return Err(unavailable(&fresh.can_change_draft));
            }
        }
        PullRequestLifecycleAction::MarkReadyForReview => {
            if request.target.observed_state != "OPEN" || fresh.state != "OPEN" || !fresh.draft {
                return Err(
                    "only a freshly observed open draft pull request can become ready".into(),
                );
            }
            if !fresh.can_change_draft.available {
                return Err(unavailable(&fresh.can_change_draft));
            }
        }
        PullRequestLifecycleAction::AddReviewer(reviewer) => {
            validate_reviewer(reviewer)?;
            if !fresh.can_request_reviewers.available {
                return Err(unavailable(&fresh.can_request_reviewers));
            }
            if has_reviewer(&fresh.reviewers, reviewer) {
                return Err("requested reviewer is already present".into());
            }
        }
        PullRequestLifecycleAction::RemoveReviewer(reviewer) => {
            validate_reviewer(reviewer)?;
            if !fresh.can_request_reviewers.available {
                return Err(unavailable(&fresh.can_request_reviewers));
            }
            if !has_reviewer(&fresh.reviewers, reviewer) {
                return Err("requested reviewer is no longer present".into());
            }
        }
        PullRequestLifecycleAction::AddLabel(label) => {
            validate_name(label, "label", 100)?;
            if !fresh.can_change_labels.available {
                return Err(unavailable(&fresh.can_change_labels));
            }
            if contains_name(&fresh.labels, label) {
                return Err("label is already present".into());
            }
        }
        PullRequestLifecycleAction::RemoveLabel(label) => {
            validate_name(label, "label", 100)?;
            if !fresh.can_change_labels.available {
                return Err(unavailable(&fresh.can_change_labels));
            }
            if !contains_name(&fresh.labels, label) {
                return Err("label is no longer present".into());
            }
        }
        PullRequestLifecycleAction::AddAssignee(login) => {
            validate_component(login, false).map_err(|error| error.to_string())?;
            if !fresh.can_change_assignees.available {
                return Err(unavailable(&fresh.can_change_assignees));
            }
            if contains_name(&fresh.assignees, login) {
                return Err("assignee is already present".into());
            }
        }
        PullRequestLifecycleAction::RemoveAssignee(login) => {
            validate_component(login, false).map_err(|error| error.to_string())?;
            if !fresh.can_change_assignees.available {
                return Err(unavailable(&fresh.can_change_assignees));
            }
            if !contains_name(&fresh.assignees, login) {
                return Err("assignee is no longer present".into());
            }
        }
    }
    Ok(())
}

fn validate_lifecycle_result(
    action: &PullRequestLifecycleAction,
    observed: &PullRequestLifecycleSnapshot,
) -> std::result::Result<(), String> {
    let applied = match action {
        PullRequestLifecycleAction::UpdateTitle { value, .. } => observed.title == *value,
        PullRequestLifecycleAction::UpdateBody { value, .. } => observed.body == *value,
        PullRequestLifecycleAction::UpdateBaseBranch { value, .. } => {
            observed.base_branch == *value
        }
        PullRequestLifecycleAction::Close => observed.state == "CLOSED",
        PullRequestLifecycleAction::Reopen => observed.state == "OPEN",
        PullRequestLifecycleAction::ConvertToDraft => observed.draft,
        PullRequestLifecycleAction::MarkReadyForReview => !observed.draft,
        PullRequestLifecycleAction::AddReviewer(value) => has_reviewer(&observed.reviewers, value),
        PullRequestLifecycleAction::RemoveReviewer(value) => {
            !has_reviewer(&observed.reviewers, value)
        }
        PullRequestLifecycleAction::AddLabel(value) => contains_name(&observed.labels, value),
        PullRequestLifecycleAction::RemoveLabel(value) => !contains_name(&observed.labels, value),
        PullRequestLifecycleAction::AddAssignee(value) => contains_name(&observed.assignees, value),
        PullRequestLifecycleAction::RemoveAssignee(value) => {
            !contains_name(&observed.assignees, value)
        }
    };
    if applied {
        Ok(())
    } else {
        Err("GitHub acknowledged the PR action but an exact-target reconciliation read did not observe it".into())
    }
}

fn has_reviewer(values: &[PullRequestReviewer], expected: &PullRequestReviewer) -> bool {
    values
        .iter()
        .any(|value| value.kind == expected.kind && value.name.eq_ignore_ascii_case(&expected.name))
}

fn contains_name(values: &[String], expected: &str) -> bool {
    values
        .iter()
        .any(|value| value.eq_ignore_ascii_case(expected))
}

fn validate_reviewer(value: &PullRequestReviewer) -> std::result::Result<(), String> {
    if !matches!(value.kind.as_str(), "USER" | "TEAM") {
        return Err("reviewer kind must be USER or TEAM".into());
    }
    if value.kind == "USER" {
        validate_component(&value.name, false).map_err(|error| error.to_string())
    } else {
        validate_name(&value.name, "team slug", MAX_IDENTITY_BYTES)
    }
}

fn validate_name(value: &str, field: &str, bound: usize) -> std::result::Result<(), String> {
    if value.is_empty()
        || value.len() > bound
        || value.contains(['\0', '\n', '\r'])
        || value.chars().any(char::is_control)
    {
        Err(format!("invalid or oversized {field}"))
    } else {
        Ok(())
    }
}

fn validate_title(value: &str) -> std::result::Result<(), String> {
    if value.trim().is_empty() || value.len() > MAX_TITLE_BYTES || value.contains('\0') {
        Err("pull request title is empty or exceeds its bound".into())
    } else {
        Ok(())
    }
}

fn validate_body(value: &str, empty_allowed: bool) -> std::result::Result<(), String> {
    if (!empty_allowed && value.trim().is_empty())
        || value.len() > MAX_BODY_BYTES
        || value.contains('\0')
    {
        Err("text body is empty or exceeds its bound".into())
    } else {
        Ok(())
    }
}

fn validate_branch_name(value: &str) -> std::result::Result<(), String> {
    validate_ref_name(value).map_err(|error| error.to_string())?;
    if value == "@"
        || value.starts_with('-')
        || value.starts_with('/')
        || value.ends_with('/')
        || value.ends_with('.')
        || value.contains("..")
        || value.contains("@{")
        || value.contains("//")
        || value
            .chars()
            .any(|character| character.is_whitespace() || "~^:?*[\\".contains(character))
        || value
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
    {
        Err("invalid GitHub branch name".into())
    } else {
        Ok(())
    }
}

enum PreparedTransport {
    Graphql(&'static str),
    Rest {
        method: &'static str,
        endpoint: String,
    },
}

struct PreparedLifecycleMutation {
    action: &'static str,
    transport: PreparedTransport,
    variables: Value,
    operation_id: String,
    pull_request_id: String,
    pull_request_number: u64,
}

impl PreparedLifecycleMutation {
    fn context(&self, request: &PullRequestLifecycleRequest) -> MutationContext {
        let dispatch = match &self.transport {
            PreparedTransport::Graphql(query) => json!({
                "transport": "graphql",
                "query": query,
                "variables": self.variables,
            }),
            PreparedTransport::Rest { method, endpoint } => json!({
                "transport": "rest",
                "method": method,
                "endpoint": endpoint,
                "variables": self.variables,
            }),
        };
        MutationContext {
            operation_id: request.operation_id.clone(),
            attempt_id: request.attempt_id.clone(),
            action: self.action.into(),
            payload: json!({"request": request, "dispatch": dispatch}),
        }
    }

    fn dispatch(&self, provider: &GithubProvider) -> MutationTransport<LifecycleRawAck> {
        match &self.transport {
            PreparedTransport::Graphql(query) => Session::new(provider)
                .graphql_mutation::<LifecycleMutationData>(query, self.variables.clone())
                .map_ack(LifecycleRawAck::Graphql),
            PreparedTransport::Rest { method, endpoint } => Session::new(provider)
                .rest_mutation::<Value>(method, endpoint.clone(), self.variables.clone())
                .map_ack(LifecycleRawAck::Rest),
        }
    }
}

fn prepare_lifecycle_mutation(
    repo: &Repository,
    request: &PullRequestLifecycleRequest,
    fresh: &PullRequestLifecycleSnapshot,
) -> std::result::Result<PreparedLifecycleMutation, String> {
    let id = fresh.pull_request.remote_id.clone();
    let operation = request.operation_id.clone();
    let graphql = |action, query, variables| PreparedLifecycleMutation {
        action,
        transport: PreparedTransport::Graphql(query),
        variables,
        operation_id: operation.clone(),
        pull_request_id: id.clone(),
        pull_request_number: fresh.pull_request.pull_request,
    };
    let rest = |action, method, endpoint, variables| PreparedLifecycleMutation {
        action,
        transport: PreparedTransport::Rest { method, endpoint },
        variables,
        operation_id: operation.clone(),
        pull_request_id: id.clone(),
        pull_request_number: fresh.pull_request.pull_request,
    };
    let number = fresh.pull_request.pull_request;
    Ok(match &request.action {
        PullRequestLifecycleAction::UpdateTitle { value, .. } => graphql(
            "update-pr-title",
            UPDATE_PULL_REQUEST_MUTATION,
            json!({"pullRequestId": id, "title": value, "body": Value::Null, "baseRefName": Value::Null, "clientMutationId": operation}),
        ),
        PullRequestLifecycleAction::UpdateBody { value, .. } => graphql(
            "update-pr-body",
            UPDATE_PULL_REQUEST_MUTATION,
            json!({"pullRequestId": id, "title": Value::Null, "body": value, "baseRefName": Value::Null, "clientMutationId": operation}),
        ),
        PullRequestLifecycleAction::UpdateBaseBranch { value, .. } => graphql(
            "update-pr-base",
            UPDATE_PULL_REQUEST_MUTATION,
            json!({"pullRequestId": id, "title": Value::Null, "body": Value::Null, "baseRefName": value, "clientMutationId": operation}),
        ),
        PullRequestLifecycleAction::Close => graphql(
            "close-pr",
            CLOSE_PULL_REQUEST_MUTATION,
            json!({"pullRequestId": id, "clientMutationId": operation}),
        ),
        PullRequestLifecycleAction::Reopen => graphql(
            "reopen-pr",
            REOPEN_PULL_REQUEST_MUTATION,
            json!({"pullRequestId": id, "clientMutationId": operation}),
        ),
        PullRequestLifecycleAction::ConvertToDraft => graphql(
            "convert-pr-to-draft",
            CONVERT_PULL_REQUEST_TO_DRAFT_MUTATION,
            json!({"pullRequestId": id, "clientMutationId": operation}),
        ),
        PullRequestLifecycleAction::MarkReadyForReview => graphql(
            "mark-pr-ready",
            MARK_PULL_REQUEST_READY_MUTATION,
            json!({"pullRequestId": id, "clientMutationId": operation}),
        ),
        PullRequestLifecycleAction::AddReviewer(reviewer) => {
            let (reviewers, team_reviewers) = reviewer_variables(reviewer);
            rest(
                "add-pr-reviewer",
                "POST",
                format!(
                    "repos/{}/pulls/{number}/requested_reviewers",
                    repo.full_name()
                ),
                json!({"reviewers": reviewers, "team_reviewers": team_reviewers}),
            )
        }
        PullRequestLifecycleAction::RemoveReviewer(reviewer) => {
            let (reviewers, team_reviewers) = reviewer_variables(reviewer);
            rest(
                "remove-pr-reviewer",
                "DELETE",
                format!(
                    "repos/{}/pulls/{number}/requested_reviewers",
                    repo.full_name()
                ),
                json!({"reviewers": reviewers, "team_reviewers": team_reviewers}),
            )
        }
        PullRequestLifecycleAction::AddLabel(label) => rest(
            "add-pr-label",
            "POST",
            format!("repos/{}/issues/{number}/labels", repo.full_name()),
            json!({"labels": [label]}),
        ),
        PullRequestLifecycleAction::RemoveLabel(label) => rest(
            "remove-pr-label",
            "DELETE",
            format!(
                "repos/{}/issues/{number}/labels/{}",
                repo.full_name(),
                url_component(label)
            ),
            json!({}),
        ),
        PullRequestLifecycleAction::AddAssignee(login) => rest(
            "add-pr-assignee",
            "POST",
            format!("repos/{}/issues/{number}/assignees", repo.full_name()),
            json!({"assignees": [login]}),
        ),
        PullRequestLifecycleAction::RemoveAssignee(login) => rest(
            "remove-pr-assignee",
            "DELETE",
            format!("repos/{}/issues/{number}/assignees", repo.full_name()),
            json!({"assignees": [login]}),
        ),
    })
}

fn reviewer_variables(reviewer: &PullRequestReviewer) -> (Value, Value) {
    if reviewer.kind == "USER" {
        (json!([reviewer.name]), json!([]))
    } else {
        (json!([]), json!([reviewer.name]))
    }
}

const UPDATE_PULL_REQUEST_MUTATION: &str = r#"mutation UpdatePullRequestMetadata(
  $pullRequestId: ID!, $title: String, $body: String, $baseRefName: String,
  $clientMutationId: String!
) {
  updatePullRequest(input: {
    pullRequestId: $pullRequestId, title: $title, body: $body,
    baseRefName: $baseRefName, clientMutationId: $clientMutationId
  }) { clientMutationId pullRequest { id } }
}"#;

const CLOSE_PULL_REQUEST_MUTATION: &str = r#"mutation ClosePullRequestLifecycle(
  $pullRequestId: ID!, $clientMutationId: String!
) {
  closePullRequest(input: {
    pullRequestId: $pullRequestId, clientMutationId: $clientMutationId
  }) { clientMutationId pullRequest { id } }
}"#;

const REOPEN_PULL_REQUEST_MUTATION: &str = r#"mutation ReopenPullRequestLifecycle(
  $pullRequestId: ID!, $clientMutationId: String!
) {
  reopenPullRequest(input: {
    pullRequestId: $pullRequestId, clientMutationId: $clientMutationId
  }) { clientMutationId pullRequest { id } }
}"#;

const CONVERT_PULL_REQUEST_TO_DRAFT_MUTATION: &str = r#"mutation ConvertPullRequestToDraftLifecycle(
  $pullRequestId: ID!, $clientMutationId: String!
) {
  convertPullRequestToDraft(input: {
    pullRequestId: $pullRequestId, clientMutationId: $clientMutationId
  }) { clientMutationId pullRequest { id } }
}"#;

const MARK_PULL_REQUEST_READY_MUTATION: &str = r#"mutation MarkPullRequestReadyForReviewLifecycle(
  $pullRequestId: ID!, $clientMutationId: String!
) {
  markPullRequestReadyForReview(input: {
    pullRequestId: $pullRequestId, clientMutationId: $clientMutationId
  }) { clientMutationId pullRequest { id } }
}"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleMutationData {
    update_pull_request: Option<LifecycleMutationPayload>,
    close_pull_request: Option<LifecycleMutationPayload>,
    reopen_pull_request: Option<LifecycleMutationPayload>,
    convert_pull_request_to_draft: Option<LifecycleMutationPayload>,
    mark_pull_request_ready_for_review: Option<LifecycleMutationPayload>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleMutationPayload {
    client_mutation_id: Option<String>,
    pull_request: Option<LifecycleMutationPull>,
}

#[derive(Deserialize)]
struct LifecycleMutationPull {
    id: String,
}

enum LifecycleRawAck {
    Graphql(LifecycleMutationData),
    Rest(Value),
}

impl LifecycleRawAck {
    fn validate(
        &self,
        prepared: &PreparedLifecycleMutation,
        repo: &Repository,
        action: &PullRequestLifecycleAction,
    ) -> std::result::Result<(), String> {
        match self {
            Self::Graphql(data) => {
                let payload = match prepared.action {
                    "update-pr-title" | "update-pr-body" | "update-pr-base" => {
                        data.update_pull_request.as_ref()
                    }
                    "close-pr" => data.close_pull_request.as_ref(),
                    "reopen-pr" => data.reopen_pull_request.as_ref(),
                    "convert-pr-to-draft" => data.convert_pull_request_to_draft.as_ref(),
                    "mark-pr-ready" => data.mark_pull_request_ready_for_review.as_ref(),
                    _ => None,
                }
                .ok_or_else(|| {
                    "GitHub omitted the requested PR action acknowledgement".to_owned()
                })?;
                if payload.client_mutation_id.as_deref() != Some(&prepared.operation_id)
                    || payload.pull_request.as_ref().map(|value| value.id.as_str())
                        != Some(prepared.pull_request_id.as_str())
                {
                    return Err("GitHub acknowledged a different operation or pull request".into());
                }
                Ok(())
            }
            Self::Rest(value) => {
                validate_lifecycle_rest_ack(value, repo, prepared.pull_request_number, action)
            }
        }
    }
}

fn validate_lifecycle_rest_ack(
    value: &Value,
    repo: &Repository,
    number: u64,
    action: &PullRequestLifecycleAction,
) -> std::result::Result<(), String> {
    let contains_login = |field: &str, expected: &str| {
        value
            .get(field)
            .and_then(Value::as_array)
            .is_some_and(|values| {
                values.iter().any(|value| {
                    value
                        .get("login")
                        .and_then(Value::as_str)
                        .is_some_and(|name| name.eq_ignore_ascii_case(expected))
                })
            })
    };
    let contains_name = |expected: &str| {
        value.as_array().is_some_and(|values| {
            values.iter().any(|value| {
                value
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.eq_ignore_ascii_case(expected))
            })
        })
    };
    match action {
        PullRequestLifecycleAction::AddReviewer(reviewer)
        | PullRequestLifecycleAction::RemoveReviewer(reviewer) => {
            validate_rest_pull_target(value, repo, number)?;
            let present = if reviewer.kind == "USER" {
                contains_login("requested_reviewers", &reviewer.name)
            } else {
                value
                    .get("requested_teams")
                    .and_then(Value::as_array)
                    .is_some_and(|values| {
                        values.iter().any(|value| {
                            value
                                .get("slug")
                                .and_then(Value::as_str)
                                .is_some_and(|name| name.eq_ignore_ascii_case(&reviewer.name))
                        })
                    })
            };
            let expected = matches!(action, PullRequestLifecycleAction::AddReviewer(_));
            if present != expected {
                return Err("GitHub REST reviewer acknowledgement claimed another result".into());
            }
        }
        PullRequestLifecycleAction::AddLabel(label) => {
            if !contains_name(label) {
                return Err("GitHub REST label acknowledgement omitted the added label".into());
            }
        }
        PullRequestLifecycleAction::RemoveLabel(label) => {
            if !value.is_array() || contains_name(label) {
                return Err("GitHub REST label acknowledgement did not remove the label".into());
            }
        }
        PullRequestLifecycleAction::AddAssignee(login)
        | PullRequestLifecycleAction::RemoveAssignee(login) => {
            validate_rest_issue_target(value, repo, number)?;
            let present = contains_login("assignees", login);
            let expected = matches!(action, PullRequestLifecycleAction::AddAssignee(_));
            if present != expected {
                return Err("GitHub REST assignee acknowledgement claimed another result".into());
            }
        }
        _ => return Err("unexpected REST lifecycle acknowledgement".into()),
    }
    Ok(())
}

fn validate_rest_pull_target(
    value: &Value,
    repo: &Repository,
    number: u64,
) -> std::result::Result<(), String> {
    if value.get("number").and_then(Value::as_u64) != Some(number)
        || !value
            .get("html_url")
            .and_then(Value::as_str)
            .is_some_and(|url| pull_url_matches(url, repo, number))
    {
        return Err("GitHub REST acknowledgement returned another pull request".into());
    }
    Ok(())
}

fn validate_rest_issue_target(
    value: &Value,
    repo: &Repository,
    number: u64,
) -> std::result::Result<(), String> {
    if value.get("number").and_then(Value::as_u64) != Some(number)
        || !value
            .get("html_url")
            .and_then(Value::as_str)
            .is_some_and(|url| pull_url_matches(url, repo, number))
        || !value.get("pull_request").is_some_and(Value::is_object)
    {
        return Err("GitHub REST acknowledgement returned another issue or pull request".into());
    }
    Ok(())
}

fn pull_url_matches(url: &str, repo: &Repository, number: u64) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };
    if !host.eq_ignore_ascii_case(&repo.host) || path.contains('?') || path.contains('#') {
        return false;
    }
    let expected_number = number.to_string();
    let mut components = path.split('/');
    components
        .next()
        .is_some_and(|owner| owner.eq_ignore_ascii_case(&repo.owner))
        && components
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(&repo.name))
        && components.next() == Some("pull")
        && components.next() == Some(expected_number.as_str())
        && components.next().is_none()
}

fn pull_comment_url_matches(url: &str, repo: &Repository, number: u64) -> bool {
    let Some((pull_url, fragment)) = url.split_once('#') else {
        return false;
    };
    let Some(comment_number) = fragment.strip_prefix("issuecomment-") else {
        return false;
    };
    pull_url_matches(pull_url, repo, number)
        && !comment_number.is_empty()
        && comment_number.bytes().all(|byte| byte.is_ascii_digit())
}

fn url_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn preflight_discussion(
    provider: &GithubProvider,
    repo: &Repository,
    request: &PullRequestDiscussionRequest,
    fresh: &PullRequestLifecycleSnapshot,
) -> std::result::Result<Option<TopLevelComment>, String> {
    validate_snapshot_target(&request.target, fresh)?;
    if !fresh.can_comment.available {
        return Err(fresh
            .can_comment
            .reason
            .clone()
            .unwrap_or_else(|| "selected account cannot comment on this pull request".into()));
    }
    match &request.action {
        PullRequestDiscussionAction::Create { body } => {
            validate_body(body, false)?;
            Ok(None)
        }
        PullRequestDiscussionAction::Edit {
            comment,
            selected_author,
            observed_body,
            observed_updated_at,
            body,
        } => {
            validate_body(body, false)?;
            validate_selected_comment(
                provider,
                repo,
                fresh.pull_request.pull_request,
                &fresh.pull_request.remote_id,
                comment,
                selected_author,
                observed_body,
                observed_updated_at,
                true,
            )
            .map(Some)
        }
        PullRequestDiscussionAction::Delete {
            comment,
            selected_author,
            observed_body,
            observed_updated_at,
        } => validate_selected_comment(
            provider,
            repo,
            fresh.pull_request.pull_request,
            &fresh.pull_request.remote_id,
            comment,
            selected_author,
            observed_body,
            observed_updated_at,
            false,
        )
        .map(Some),
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_selected_comment(
    provider: &GithubProvider,
    repo: &Repository,
    number: u64,
    pull_request_id: &str,
    coordinates: &ProviderCoordinates,
    selected_author: &str,
    observed_body: &str,
    observed_updated_at: &str,
    editing: bool,
) -> std::result::Result<TopLevelComment, String> {
    if !coordinates_match(repo, number, coordinates) {
        return Err("top-level comment belongs to another provider, repository, or PR".into());
    }
    if !selected_author.eq_ignore_ascii_case(&provider.account.login) {
        return Err("selected comment author does not match the selected account".into());
    }
    let comment = Session::new(provider)
        .top_level_comment(repo, number, &coordinates.remote_id)
        .map_err(|error| error.to_string())?;
    if comment.pull_request.as_ref().map(|pull| pull.id.as_str()) != Some(pull_request_id) {
        return Err("top-level comment is linked to another pull request node".into());
    }
    if !comment
        .author
        .as_ref()
        .is_some_and(|value| value.login.eq_ignore_ascii_case(selected_author))
        || comment.body != observed_body
        || comment.updated_at != observed_updated_at
    {
        return Err(
            "top-level comment author or affected fields changed since confirmation".into(),
        );
    }
    if editing && !comment.viewer_can_update {
        return Err("selected account cannot edit this top-level comment".into());
    }
    if !editing && !comment.viewer_can_delete {
        return Err("selected account cannot delete this top-level comment".into());
    }
    Ok(comment)
}

struct PreparedDiscussionMutation {
    action: &'static str,
    query: &'static str,
    variables: Value,
}

impl PreparedDiscussionMutation {
    fn context(&self, request: &PullRequestDiscussionRequest) -> MutationContext {
        MutationContext {
            operation_id: request.operation_id.clone(),
            attempt_id: request.attempt_id.clone(),
            action: self.action.into(),
            payload: json!({
                "request": request,
                "dispatch": {
                    "transport": "graphql",
                    "query": self.query,
                    "variables": self.variables,
                }
            }),
        }
    }

    fn dispatch(&self, provider: &GithubProvider) -> MutationTransport<DiscussionMutationData> {
        Session::new(provider).graphql_mutation(self.query, self.variables.clone())
    }
}

fn prepare_discussion_mutation(
    request: &PullRequestDiscussionRequest,
    fresh: &PullRequestLifecycleSnapshot,
    comment: Option<&TopLevelComment>,
) -> std::result::Result<PreparedDiscussionMutation, String> {
    Ok(match &request.action {
        PullRequestDiscussionAction::Create { body } => PreparedDiscussionMutation {
            action: "create-pr-discussion-comment",
            query: ADD_TOP_LEVEL_COMMENT_MUTATION,
            variables: json!({
                "subjectId": fresh.pull_request.remote_id,
                "body": body,
                "clientMutationId": request.operation_id,
            }),
        },
        PullRequestDiscussionAction::Edit { body, .. } => PreparedDiscussionMutation {
            action: "edit-pr-discussion-comment",
            query: UPDATE_TOP_LEVEL_COMMENT_MUTATION,
            variables: json!({
                "commentId": comment.ok_or_else(|| "missing exact comment preflight".to_owned())?.id,
                "body": body,
                "clientMutationId": request.operation_id,
            }),
        },
        PullRequestDiscussionAction::Delete { .. } => PreparedDiscussionMutation {
            action: "delete-pr-discussion-comment",
            query: DELETE_TOP_LEVEL_COMMENT_MUTATION,
            variables: json!({
                "commentId": comment.ok_or_else(|| "missing exact comment preflight".to_owned())?.id,
                "clientMutationId": request.operation_id,
            }),
        },
    })
}

const ADD_TOP_LEVEL_COMMENT_MUTATION: &str = r#"mutation AddTopLevelPullRequestComment(
  $subjectId: ID!, $body: String!, $clientMutationId: String!
) {
  addComment(input: {
    subjectId: $subjectId, body: $body, clientMutationId: $clientMutationId
  }) {
    clientMutationId
    subject { ... on PullRequest { id number url repository { nameWithOwner } } }
    commentEdge { node { id body createdAt updatedAt url author { login } repository { nameWithOwner } pullRequest { id number url repository { nameWithOwner } } } }
  }
}"#;

const UPDATE_TOP_LEVEL_COMMENT_MUTATION: &str = r#"mutation UpdateTopLevelPullRequestComment(
  $commentId: ID!, $body: String!, $clientMutationId: String!
) {
  updateIssueComment(input: {
    id: $commentId, body: $body, clientMutationId: $clientMutationId
  }) {
    clientMutationId
    issueComment { id body createdAt updatedAt url author { login } repository { nameWithOwner } pullRequest { id number url repository { nameWithOwner } } }
  }
}"#;

const DELETE_TOP_LEVEL_COMMENT_MUTATION: &str = r#"mutation DeleteTopLevelPullRequestComment(
  $commentId: ID!, $clientMutationId: String!
) {
  deleteIssueComment(input: {
    id: $commentId, clientMutationId: $clientMutationId
  }) { clientMutationId }
}"#;

const COMMENT_NODE_QUERY: &str = r#"query TopLevelPullRequestComment($id: ID!) {
  viewer { login }
  node(id: $id) {
    ... on IssueComment {
      id body createdAt updatedAt url author { login }
      viewerCanUpdate viewerCanDelete
      repository { nameWithOwner }
      pullRequest { id number url repository { nameWithOwner } }
    }
  }
}"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentNodeData {
    viewer: LifecycleActor,
    node: Option<TopLevelComment>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TopLevelComment {
    id: String,
    body: String,
    created_at: String,
    updated_at: String,
    url: String,
    author: Option<LifecycleActor>,
    viewer_can_update: bool,
    viewer_can_delete: bool,
    repository: NameWithOwner,
    pull_request: Option<DiscussionSubject>,
}

impl TopLevelComment {
    fn validate(&self, repo: &Repository, number: u64, id: &str) -> Result<()> {
        ensure!(
            self.id == id,
            "GitHub returned a different issue comment ID"
        );
        let pull = self
            .pull_request
            .as_ref()
            .context("issue comment is not attached to a pull request")?;
        ensure!(
            self.repository
                .name_with_owner
                .eq_ignore_ascii_case(&repo.full_name())
                && pull
                    .repository
                    .name_with_owner
                    .eq_ignore_ascii_case(&repo.full_name())
                && pull.number == number
                && pull_url_matches(&pull.url, repo, number),
            "top-level comment is linked to another repository or PR"
        );
        ensure!(
            pull_comment_url_matches(&self.url, repo, number),
            "top-level comment URL is linked to another PR"
        );
        Ok(())
    }

    fn into_domain(self, repo: &Repository, number: u64) -> IssueComment {
        IssueComment {
            coordinates: coordinates(repo, number, self.id),
            author: self.author.map(|value| value.login),
            body: self.body,
            created_at: self.created_at,
            updated_at: self.updated_at,
            url: self.url,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NameWithOwner {
    name_with_owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiscussionMutationData {
    add_comment: Option<AddCommentPayload>,
    update_issue_comment: Option<UpdateCommentPayload>,
    delete_issue_comment: Option<DeleteCommentPayload>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddCommentPayload {
    client_mutation_id: Option<String>,
    subject: Option<DiscussionSubject>,
    comment_edge: Option<DiscussionCommentEdge>,
}

#[derive(Deserialize)]
struct DiscussionCommentEdge {
    node: Option<TopLevelComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateCommentPayload {
    client_mutation_id: Option<String>,
    issue_comment: Option<TopLevelComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteCommentPayload {
    client_mutation_id: Option<String>,
}

#[derive(Clone, Deserialize)]
struct DiscussionSubject {
    id: String,
    number: u64,
    url: String,
    repository: NameWithOwner,
}

fn validate_discussion_ack(
    repo: &Repository,
    request: &PullRequestDiscussionRequest,
    data: DiscussionMutationData,
) -> std::result::Result<PullRequestDiscussionAcknowledgement, String> {
    let number = request.target.pull_request.pull_request;
    match &request.action {
        PullRequestDiscussionAction::Create { body } => {
            let payload = data.add_comment.ok_or_else(|| {
                "GitHub omitted the created top-level comment acknowledgement".to_owned()
            })?;
            if payload.client_mutation_id.as_deref() != Some(&request.operation_id) {
                return Err("GitHub acknowledged another comment operation".into());
            }
            let subject = payload
                .subject
                .ok_or_else(|| "GitHub omitted the created comment parent".to_owned())?;
            if subject.id != request.target.pull_request.remote_id
                || subject.number != number
                || !subject
                    .repository
                    .name_with_owner
                    .eq_ignore_ascii_case(&repo.full_name())
                || !pull_url_matches(&subject.url, repo, number)
            {
                return Err("GitHub linked the created comment to another pull request".into());
            }
            let comment = payload
                .comment_edge
                .and_then(|value| value.node)
                .ok_or_else(|| "GitHub omitted the created comment ID".to_owned())?;
            comment
                .validate(repo, number, &comment.id)
                .map_err(|error| error.to_string())?;
            if comment.body != *body {
                return Err("GitHub acknowledged a different created comment body".into());
            }
            Ok(PullRequestDiscussionAcknowledgement {
                operation_id: request.operation_id.clone(),
                repository: repo.clone(),
                pull_request: request.target.pull_request.clone(),
                comment: coordinates(repo, number, comment.id),
                body: Some(comment.body),
                deleted: false,
            })
        }
        PullRequestDiscussionAction::Edit { comment, body, .. } => {
            let payload = data.update_issue_comment.ok_or_else(|| {
                "GitHub omitted the edited top-level comment acknowledgement".to_owned()
            })?;
            if payload.client_mutation_id.as_deref() != Some(&request.operation_id) {
                return Err("GitHub acknowledged another comment operation".into());
            }
            let updated = payload
                .issue_comment
                .ok_or_else(|| "GitHub omitted the edited comment ID".to_owned())?;
            updated
                .validate(repo, number, &comment.remote_id)
                .map_err(|error| error.to_string())?;
            if updated.body != *body {
                return Err("GitHub acknowledged a different edited comment body".into());
            }
            Ok(PullRequestDiscussionAcknowledgement {
                operation_id: request.operation_id.clone(),
                repository: repo.clone(),
                pull_request: request.target.pull_request.clone(),
                comment: comment.clone(),
                body: Some(updated.body),
                deleted: false,
            })
        }
        PullRequestDiscussionAction::Delete { comment, .. } => {
            let payload = data.delete_issue_comment.ok_or_else(|| {
                "GitHub omitted the deleted top-level comment acknowledgement".to_owned()
            })?;
            if payload.client_mutation_id.as_deref() != Some(&request.operation_id) {
                return Err("GitHub acknowledged another comment operation".into());
            }
            Ok(PullRequestDiscussionAcknowledgement {
                operation_id: request.operation_id.clone(),
                repository: repo.clone(),
                pull_request: request.target.pull_request.clone(),
                comment: comment.clone(),
                body: None,
                deleted: true,
            })
        }
    }
}

fn validate_creation_text(input: &PullRequestCreationInput) -> Result<()> {
    validate_title(&input.title).map_err(anyhow::Error::msg)?;
    validate_body(&input.body, true).map_err(anyhow::Error::msg)?;
    validate_branch_name(&input.base_branch).map_err(anyhow::Error::msg)?;
    validate_branch_name(&input.source_branch).map_err(anyhow::Error::msg)?;
    if let Some(local) = &input.local_branch {
        validate_branch_name(local).map_err(anyhow::Error::msg)?;
    }
    ensure!(
        input.target_repository.account == input.source_repository.account,
        "target and source repositories must use the same selected account"
    );
    Ok(())
}

fn validate_creation_fresh(
    expected: &PullRequestCreationPreparation,
    fresh: &PullRequestCreationPreparation,
) -> std::result::Result<(), String> {
    if fresh.input != expected.input {
        return Err("PR creation coordinates or fields differ from the frozen preparation".into());
    }
    if fresh.observed_base_sha != expected.observed_base_sha {
        return Err("target base branch moved since PR creation preparation".into());
    }
    if fresh.observed_source_head_sha != expected.observed_source_head_sha {
        return Err("published source branch moved since PR creation preparation".into());
    }
    if !fresh
        .viewer_login
        .eq_ignore_ascii_case(&expected.viewer_login)
        || fresh.repository_permission != expected.repository_permission
        || !fresh.can_create.available
    {
        return Err("selected account or PR creation capability changed since preparation".into());
    }
    Ok(())
}

fn same_repository(left: &Repository, right: &Repository) -> bool {
    left.host.eq_ignore_ascii_case(&right.host)
        && left.owner.eq_ignore_ascii_case(&right.owner)
        && left.name.eq_ignore_ascii_case(&right.name)
        && left.account == right.account
}

#[derive(Deserialize)]
struct ApiCreationRepository {
    name: String,
    owner: ApiCreationOwner,
    full_name: String,
    permissions: Option<ApiRepositoryPermissions>,
    parent: Option<ApiRepositoryLink>,
    source: Option<ApiRepositoryLink>,
}

impl ApiCreationRepository {
    fn validate(&self, repo: &Repository) -> Result<()> {
        ensure!(
            self.name.eq_ignore_ascii_case(&repo.name)
                && self.owner.login.eq_ignore_ascii_case(&repo.owner)
                && self.full_name.eq_ignore_ascii_case(&repo.full_name()),
            "GitHub returned different creation repository coordinates"
        );
        Ok(())
    }

    fn permission_name(&self) -> Option<String> {
        self.permissions.as_ref().map(|value| {
            if value.admin {
                "ADMIN"
            } else if value.maintain {
                "MAINTAIN"
            } else if value.push {
                "WRITE"
            } else if value.triage {
                "TRIAGE"
            } else if value.pull {
                "READ"
            } else {
                "NONE"
            }
            .into()
        })
    }
}

fn creation_network_root(repository: &ApiCreationRepository) -> &str {
    repository
        .source
        .as_ref()
        .map(|value| value.full_name.as_str())
        .unwrap_or(&repository.full_name)
}

#[derive(Deserialize)]
struct ApiCreationOwner {
    login: String,
}

#[derive(Deserialize)]
struct ApiRepositoryLink {
    full_name: String,
}

#[derive(Deserialize)]
struct ApiRepositoryPermissions {
    #[serde(default)]
    admin: bool,
    #[serde(default)]
    maintain: bool,
    #[serde(default)]
    push: bool,
    #[serde(default)]
    triage: bool,
    #[serde(default)]
    pull: bool,
}

#[derive(Deserialize)]
struct ApiPublishedRef {
    #[serde(rename = "ref")]
    reference: String,
    object: ApiPublishedObject,
}

#[derive(Deserialize)]
struct ApiPublishedObject {
    sha: String,
}

struct PreparedCreationMutation {
    endpoint: String,
    variables: Value,
}

impl PreparedCreationMutation {
    fn new(request: &PullRequestCreationRequest) -> Self {
        let input = &request.preparation.input;
        let cross_repository = !same_repository(&input.target_repository, &input.source_repository);
        let head = if cross_repository {
            format!("{}:{}", input.source_repository.owner, input.source_branch)
        } else {
            input.source_branch.clone()
        };
        let mut variables = json!({
            "title": input.title,
            "body": input.body,
            "base": input.base_branch,
            "head": head,
            "draft": input.draft,
        });
        if cross_repository {
            variables["head_repo"] = json!(input.source_repository.name);
        }
        Self {
            endpoint: format!("repos/{}/pulls", input.target_repository.full_name()),
            variables,
        }
    }

    fn context(&self, request: &PullRequestCreationRequest) -> MutationContext {
        MutationContext {
            operation_id: request.operation_id.clone(),
            attempt_id: request.attempt_id.clone(),
            action: "create-pr".into(),
            payload: json!({
                "request": request,
                "dispatch": {
                    "transport": "rest",
                    "method": "POST",
                    "endpoint": self.endpoint,
                    "variables": self.variables,
                }
            }),
        }
    }

    fn dispatch(&self, provider: &GithubProvider) -> MutationTransport<ApiCreatedPull> {
        Session::new(provider).rest_mutation("POST", self.endpoint.clone(), self.variables.clone())
    }
}

#[derive(Deserialize)]
struct ApiCreatedPull {
    node_id: String,
    number: u64,
    html_url: String,
    state: String,
    title: String,
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    base: ApiCreatedPullRef,
    head: ApiCreatedPullRef,
}

#[derive(Deserialize)]
struct ApiCreatedPullRef {
    sha: String,
    #[serde(rename = "ref")]
    branch: String,
    repo: Option<ApiCreatedPullRepository>,
}

#[derive(Deserialize)]
struct ApiCreatedPullRepository {
    name: String,
    owner: ApiCreationOwner,
}

fn validate_creation_ack(
    request: &PullRequestCreationRequest,
    response: ApiCreatedPull,
) -> std::result::Result<PullRequestCreationAcknowledgement, String> {
    let input = &request.preparation.input;
    validate_node_id(&response.node_id).map_err(|error| error.to_string())?;
    validate_sha(&response.head.sha).map_err(|error| error.to_string())?;
    validate_sha(&response.base.sha).map_err(|error| error.to_string())?;
    let base_repo = response
        .base
        .repo
        .as_ref()
        .ok_or_else(|| "GitHub omitted the created PR target repository".to_owned())?;
    let head_repo = response
        .head
        .repo
        .as_ref()
        .ok_or_else(|| "GitHub omitted the created PR source repository".to_owned())?;
    if response.number == 0
        || !pull_url_matches(
            &response.html_url,
            &input.target_repository,
            response.number,
        )
        || !base_repo
            .owner
            .login
            .eq_ignore_ascii_case(&input.target_repository.owner)
        || !base_repo
            .name
            .eq_ignore_ascii_case(&input.target_repository.name)
        || !head_repo
            .owner
            .login
            .eq_ignore_ascii_case(&input.source_repository.owner)
        || !head_repo
            .name
            .eq_ignore_ascii_case(&input.source_repository.name)
        || response.base.branch != input.base_branch
        || response.head.branch != input.source_branch
        || response.base.sha != request.preparation.observed_base_sha
        || response.title != input.title
        || response.body.unwrap_or_default() != input.body
        || response.draft != input.draft
        || response.state != "open"
    {
        return Err(
            "GitHub acknowledged a PR with different target, source, or creation fields".into(),
        );
    }
    Ok(PullRequestCreationAcknowledgement {
        operation_id: request.operation_id.clone(),
        target_repository: input.target_repository.clone(),
        source_repository: input.source_repository.clone(),
        pull_request: coordinates(&input.target_repository, response.number, response.node_id),
        actual_head_sha: response.head.sha,
        reviewed_head_sha: request.preparation.observed_source_head_sha.clone(),
        reviewed_head_atomically_enforced: false,
        url: response.html_url,
    })
}
