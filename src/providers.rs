//! Read-only GitHub.com transport. Call synchronous methods on a background task.
//!
//! Credentials live only in a private child environment; no global login is changed.
//! Comparisons use GitHub's three-dot (merge-base to head) PR semantics. REST caps
//! compare files at 300 and current PR files at 3,000; see `comparison` for limits.
//! Sidebar metadata uses one GraphQL batch per REST page (up to 100 PRs), never one
//! request per PR. Participant sources are bounded to 100 entries each and expose
//! incompleteness. Details paginates four top-level connections 50 at a time for at most
//! 20 pages; nested review-thread comments are bounded to 100 with explicit flags.
use crate::comparisons::{
    CommitInventory, CommitInventoryEntry, InventoryAvailability, MAX_COMMIT_INVENTORY,
};
use crate::domain::{
    Account, ActionsLinkage, BranchDeletionAcknowledgement, BranchDeletionRequest, ChangedFile,
    CheckAppIdentity, CheckKind, CheckRepositoryIdentity, CheckShaClass, CheckSuiteIdentity,
    Comparison, DismissalAuthority, FreshReactionCapability, FreshReviewDismissalCapability,
    IssueComment, LinkedReviewComment, MergeAcknowledgement, MergeAction, MergeEligibility,
    MergeExecutionRequest, MergeMethod, MergePreparation, MutationContext,
    PendingFileCommentSource, PendingReviewSnapshot, ProviderCoordinates, ProviderMutationOutcome,
    PullRequest, PullRequestCheck, PullRequestCheckoutSource, PullRequestDetails,
    PullRequestReview, ReactableKind, ReactionContent, ReactionGroupSnapshot, ReactionSnapshot,
    ReactionSubjectSnapshot, Repository, ReviewAuxiliaryAcknowledgement, ReviewAuxiliaryAction,
    ReviewAuxiliaryRequest, ReviewComment, ReviewSubject, ReviewThread, ReviewWriteAcknowledgement,
    Revision, SelectedViewer, SubmittedReviewEditCapability, WorkflowRunIdentity,
};
use crate::participation::{
    DraftStore, PendingCommentIntent, PendingFileCommentIntent, ReviewCommentTarget,
    ReviewComposition, ReviewEvent, ReviewOperationPayload, ReviewOperationStatus,
    SubmissionIntent,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

mod actions_jobs_logs;
mod conditional;
mod general_sync;
pub mod notifications;
mod pr_lifecycle;
mod reactions;
mod review_dismissal;
mod stacks;
pub use actions_jobs_logs::{ActionsCancellation, ActionsReadError, ActionsReadErrorCategory};
#[cfg(feature = "ui-smoke")]
pub use general_sync::synthetic_exact_304_smoke_fixture;
pub use general_sync::{
    GeneralReadCache, GeneralReadDelay, GeneralReadDirective, GeneralReadFailureKind,
    GeneralReadOutcome,
};
pub use pr_lifecycle::{AdmittedMutationAttempt, MutationAdmission};

const HOST: &str = "github.com";
const API_VERSION: &str = "X-GitHub-Api-Version: 2026-03-10";
const PAGE_SIZE: usize = 100;
const MAX_PR_PAGES: usize = 100;
const MAX_FILE_PAGES: usize = 30;
const MAX_COMMIT_INVENTORY_PAGES: usize = 10;
const PARTICIPANT_LIMIT: usize = 100;
const MAX_DETAILS_PAGES: usize = 20;
const MAX_OPERATION_BYTES: usize = 64 * 1024 * 1024;
const MAX_MUTATION_INPUT_BYTES: usize = 1024 * 1024;
const MAX_ACTION_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct GithubProvider {
    account: Account,
    runner: Runner,
}

impl GithubProvider {
    pub fn new(account: Account) -> Self {
        Self {
            account,
            runner: Runner::default(),
        }
    }

    #[cfg(any(test, feature = "ui-smoke"))]
    #[doc(hidden)]
    pub fn synthetic_with_gh(account: Account, gh: PathBuf, timeout: Duration) -> Self {
        Self {
            account,
            runner: Runner {
                gh,
                timeout,
                ..Runner::default()
            },
        }
    }

    #[cfg(any(test, feature = "ui-smoke"))]
    #[doc(hidden)]
    pub fn synthetic_with_actions_transport(
        account: Account,
        gh: PathBuf,
        curl: PathBuf,
        timeout: Duration,
    ) -> Self {
        Self {
            account,
            runner: Runner {
                gh,
                curl,
                timeout,
                ..Runner::default()
            },
        }
    }

    /// Discover all stored GitHub.com identities, including inactive identities.
    /// Broken credentials are omitted; an entirely unusable configuration errors.
    pub fn accounts() -> Result<Vec<Account>> {
        discover_accounts(&Runner::default())
    }

    /// Accept owner/name, an HTTPS/SSH GitHub repository URL, or a local folder.
    /// Local folders use origin, or the sole remote when origin does not exist.
    pub fn repository(&self, input: &str) -> Result<Repository> {
        validate_account(&self.account)?;
        let input = input.trim();
        let (remote, local_path) = if Path::new(input).is_dir() {
            let path = Path::new(input)
                .canonicalize()
                .context("Cannot resolve local folder")?;
            let mut command = self.runner.git_command();
            command.arg("-C").arg(&path).arg("remote");
            let remotes = self.runner.run(command, "list local Git remotes")?;
            let remotes = std::str::from_utf8(&remotes).context("Invalid Git remote names")?;
            let names: Vec<_> = remotes.lines().collect();
            let name = if names.contains(&"origin") {
                "origin"
            } else {
                ensure!(
                    names.len() == 1,
                    "Local folder must have origin or exactly one Git remote"
                );
                names[0]
            };
            ensure!(!name.starts_with('-'), "Invalid Git remote name");
            let mut command = self.runner.git_command();
            command
                .arg("-C")
                .arg(&path)
                .args(["remote", "get-url", "--all", name]);
            let urls = self.runner.run(command, "resolve local Git remote")?;
            let urls = std::str::from_utf8(&urls).context("Invalid Git remote URL encoding")?;
            let urls: Vec<_> = urls.lines().collect();
            ensure!(
                urls.len() == 1,
                "Git remote must have exactly one fetch URL"
            );
            (urls[0].to_owned(), Some(path))
        } else {
            (input.to_owned(), None)
        };
        let (owner, name) = parse_repository(&remote)?;
        let mut session = Session::new(self);
        let metadata: ApiRepository = session.get(&format!("repos/{owner}/{name}"))?;
        ensure!(
            metadata.owner.login.eq_ignore_ascii_case(&owner)
                && metadata.name.eq_ignore_ascii_case(&name),
            "Repository moved or API returned a different repository; add its canonical name explicitly"
        );
        validate_component(&metadata.owner.login, false)?;
        validate_component(&metadata.name, true)?;
        Ok(Repository {
            host: HOST.into(),
            owner: metadata.owner.login,
            name: metadata.name,
            account: self.account.clone(),
            local_path,
        })
    }

    pub fn list_pull_requests(&self, repo: &Repository, state: &str) -> Result<Vec<PullRequest>> {
        self.validate_repo(repo)?;
        let state = state.to_ascii_lowercase();
        ensure!(
            ["open", "closed", "merged", "all"].contains(&state.as_str()),
            "Unsupported PR state"
        );
        let api_state = if state == "merged" { "closed" } else { &state };
        let mut session = Session::new(self);
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        for page in 1..=MAX_PR_PAGES {
            let pulls: Vec<ApiPullRequest> = session.get(&format!(
                "repos/{}/pulls?state={api_state}&sort=created&direction=asc&per_page={PAGE_SIZE}&page={page}", repo.full_name()))?;
            ensure!(pulls.len() <= PAGE_SIZE, "Invalid PR pagination response");
            let last = pulls.len() < PAGE_SIZE;
            let mut batch = Vec::with_capacity(pulls.len());
            for pull in pulls {
                pull.validate(repo, None)?;
                ensure!(
                    seen.insert(pull.number),
                    "PR list changed during pagination; refresh to retry"
                );
                batch.push(pull.into_domain());
            }
            session.hydrate_metadata(repo, &mut batch)?;
            for pull in batch {
                if state != "merged" || pull.state == "MERGED" {
                    result.push(pull);
                }
            }
            if last {
                return Ok(result);
            }
        }
        bail!("PR pagination limit reached; list is incomplete")
    }

    pub fn pull_request(&self, repo: &Repository, number: u64) -> Result<PullRequest> {
        self.validate_repo(repo)?;
        let mut session = Session::new(self);
        let mut pulls = vec![session.pull(repo, number)?.into_domain()];
        session.hydrate_metadata(repo, &mut pulls)?;
        Ok(pulls.pop().expect("one PR"))
    }

    /// Read the current source repository and branch without cloning or changing
    /// the caller's pinned review. Fork coordinates come from the PR head, not
    /// the base repository's origin. The account selects this API read only;
    /// installed Git still owns Git transport authentication and authorship.
    pub fn checkout_source(
        &self,
        repo: &Repository,
        number: u64,
    ) -> Result<PullRequestCheckoutSource> {
        self.validate_repo(repo)?;
        let pull = Session::new(self).pull(repo, number)?;
        for branch in [&pull.head.branch, &pull.base.branch] {
            ensure!(
                !branch.is_empty()
                    && branch.len() <= 1024
                    && !branch.starts_with('-')
                    && !branch.chars().any(char::is_control),
                "Invalid or oversized PR branch name"
            );
        }
        let observed_revision = pull.revision();
        let source_repository = pull
            .head
            .repo
            .map(|source| -> Result<Repository> {
                validate_component(&source.owner.login, false)?;
                validate_component(&source.name, true)?;
                Ok(Repository {
                    host: repo.host.clone(),
                    owner: source.owner.login,
                    name: source.name,
                    account: self.account.clone(),
                    local_path: None,
                })
            })
            .transpose()?;
        Ok(PullRequestCheckoutSource {
            number: pull.number,
            base_repository: repo.clone(),
            source_repository,
            source_branch: pull.head.branch,
            target_branch: pull.base.branch,
            observed_revision,
        })
    }

    /// Fetch current, read-only collaboration data for Overview, Activity, and
    /// Checks. This snapshot intentionally carries no comparison revision and
    /// must not advance or replace the caller's displayed diff.
    pub fn details(&self, repo: &Repository, number: u64) -> Result<PullRequestDetails> {
        self.validate_repo(repo)?;
        ensure!(
            number > 0 && number <= i32::MAX as u64,
            "PR number is outside GitHub GraphQL limits"
        );
        Session::new(self).details(repo, number)
    }

    /// Import the selected account's single pending review and only comments
    /// whose provider-reported parent review ID matches it.
    pub fn pending_review(
        &self,
        repo: &Repository,
        number: u64,
    ) -> Result<Option<PendingReviewSnapshot>> {
        self.validate_repo(repo)?;
        ensure!(number > 0 && number <= i32::MAX as u64, "Invalid PR number");
        Session::new(self).pending_review(repo, number)
    }

    /// Execute exactly one frozen participation payload. The exact InFlight
    /// state is durably saved before dispatch; restored InFlight/Uncertain
    /// operations are never replayed.
    pub fn execute_review_operation(
        &self,
        repo: &Repository,
        composition: &mut ReviewComposition,
        store: &DraftStore,
        operation_id: &str,
        attempt_id: &str,
    ) -> ProviderMutationOutcome<ReviewWriteAcknowledgement> {
        let payload = match composition
            .operations
            .iter()
            .find(|operation| operation.id == operation_id)
        {
            Some(operation) if operation.status == ReviewOperationStatus::Prepared => {
                match operation.payload.clone() {
                    Some(payload) => payload,
                    None => {
                        return rejected("prepared review operation has no frozen payload");
                    }
                }
            }
            Some(operation) if operation.status.requires_reconciliation() => {
                return rejected(
                    "review operation was already dispatched and must be reconciled before retry",
                );
            }
            Some(_) => return rejected("review operation is not executable"),
            None => return rejected("review operation does not exist"),
        };
        if let Err(reason) = validate_review_key(self, repo, composition, &payload) {
            return rejected(reason);
        }
        let prepared = match self.prepare_review_mutation(repo, &payload) {
            Ok(prepared) => prepared,
            Err(reason) => return rejected(reason),
        };
        let context = MutationContext {
            operation_id: operation_id.to_owned(),
            attempt_id: attempt_id.to_owned(),
            action: prepared.action.to_owned(),
            payload: json!({"query": prepared.query, "variables": prepared.variables}),
        };
        if let Err(error) = composition.mark_in_flight(operation_id, attempt_id) {
            return rejected(error.to_string());
        }
        if let Err(error) = store.save(composition) {
            if let Some(operation) = composition
                .operations
                .iter_mut()
                .find(|operation| operation.id == operation_id)
            {
                operation.status = ReviewOperationStatus::Prepared;
            }
            return rejected(format!(
                "could not durably save the in-flight review operation; dispatched zero writes: {error}"
            ));
        }
        let saved_in_flight = composition.clone();
        let transport = Session::new(self)
            .graphql_mutation::<ReviewMutationData>(prepared.query, prepared.variables);
        match transport {
            MutationTransport::Rejected(reason) => {
                let _ = composition.mark_uncertain(operation_id, reason.clone());
                let _ = store.save(composition);
                ProviderMutationOutcome::Uncertain { context, reason }
            }
            MutationTransport::Uncertain(reason) => {
                let _ = composition.mark_uncertain(operation_id, reason.clone());
                let _ = store.save(composition);
                ProviderMutationOutcome::Uncertain { context, reason }
            }
            MutationTransport::Acknowledged(data) => {
                let ack = match prepared.kind.acknowledgement(data) {
                    Ok(ack) => ack,
                    Err(reason) => {
                        let _ = composition.mark_uncertain(operation_id, reason.clone());
                        let _ = store.save(composition);
                        return ProviderMutationOutcome::Uncertain { context, reason };
                    }
                };
                let reconciled = match &payload {
                    ReviewOperationPayload::PendingComment(intent) => composition
                        .reconcile_observed_comment_success(
                            operation_id,
                            ack.review_id.clone(),
                            ack.comment_id.clone().unwrap_or_default(),
                            intent.body.clone(),
                        ),
                    ReviewOperationPayload::ImmediateComment(intent) => composition
                        .reconcile_observed_comment_success(
                            operation_id,
                            ack.review_id.clone(),
                            ack.comment_id.clone().unwrap_or_default(),
                            intent.body.clone(),
                        ),
                    ReviewOperationPayload::PendingFileComment(_) => composition
                        .reconcile_observed_file_comment_success(
                            operation_id,
                            ack.review_id.clone().unwrap_or_default(),
                            ack.comment_id.clone().unwrap_or_default(),
                        ),
                    ReviewOperationPayload::Submission(_) => composition
                        .reconcile_observed_submission_success(
                            operation_id,
                            ack.review_id.clone().unwrap_or_default(),
                        ),
                };
                if let Err(error) = reconciled {
                    let reason = format!(
                        "GitHub acknowledged the write but local reconciliation failed: {error}"
                    );
                    *composition = saved_in_flight.clone();
                    let _ = composition.mark_uncertain(operation_id, reason.clone());
                    let _ = store.save(composition);
                    return ProviderMutationOutcome::Uncertain { context, reason };
                }
                let result = ReviewWriteAcknowledgement {
                    operation_id: operation_id.to_owned(),
                    review_id: ack.review_id,
                    comment_id: ack.comment_id,
                    thread_id: ack.thread_id,
                };
                if let Err(error) = store.save(composition) {
                    *composition = saved_in_flight;
                    return ProviderMutationOutcome::Uncertain {
                        context,
                        reason: format!(
                            "GitHub acknowledged the write but its local acknowledgement could not be saved; reconcile the durable InFlight attempt: {error}"
                        ),
                    };
                }
                ProviderMutationOutcome::Acknowledged(result)
            }
        }
    }

    /// Execute one explicit non-composition review action. The returned
    /// uncertainty contains the exact request payload and attempt identity for
    /// caller-owned durable reconciliation; this method never retries it.
    pub fn execute_review_auxiliary(
        &self,
        repo: &Repository,
        number: u64,
        request: &ReviewAuxiliaryRequest,
    ) -> ProviderMutationOutcome<ReviewAuxiliaryAcknowledgement> {
        let prepared = match self.prepare_auxiliary_mutation(repo, number, request) {
            Ok(prepared) => prepared,
            Err(reason) => return rejected(reason),
        };
        let context = MutationContext {
            operation_id: request.operation_id.clone(),
            attempt_id: request.attempt_id.clone(),
            action: prepared.action.into(),
            payload: json!({"query": prepared.query, "variables": prepared.variables}),
        };
        match Session::new(self)
            .graphql_mutation::<AuxiliaryMutationData>(prepared.query, prepared.variables)
        {
            MutationTransport::Rejected(reason) => rejected(reason),
            MutationTransport::Uncertain(reason) => {
                ProviderMutationOutcome::Uncertain { context, reason }
            }
            MutationTransport::Acknowledged(data) => match prepared.kind.acknowledgement(data) {
                Ok(mut ack) => {
                    ack.operation_id = request.operation_id.clone();
                    ProviderMutationOutcome::Acknowledged(ack)
                }
                Err(reason) => ProviderMutationOutcome::Uncertain { context, reason },
            },
        }
    }

    fn prepare_review_mutation(
        &self,
        repo: &Repository,
        payload: &ReviewOperationPayload,
    ) -> std::result::Result<PreparedReviewMutation, String> {
        let (key, reviewed_sha) = match payload {
            ReviewOperationPayload::PendingComment(intent) => {
                (&intent.key, intent.position.commit_sha.as_str())
            }
            ReviewOperationPayload::ImmediateComment(intent) => {
                (&intent.key, intent.position.commit_sha.as_str())
            }
            ReviewOperationPayload::PendingFileComment(intent) => {
                let ReviewCommentTarget::File(file) = &intent.target else {
                    return Err("pending file comment contains a line target".into());
                };
                (&intent.key, file.commit_sha.as_str())
            }
            ReviewOperationPayload::Submission(intent) => {
                (&intent.key, intent.reviewed_commit_sha.as_str())
            }
        };
        validate_sha(reviewed_sha).map_err(|error| error.to_string())?;
        let mut session = Session::new(self);
        if let ReviewOperationPayload::PendingFileComment(intent) = payload {
            return prepare_pending_file_comment_mutation(&mut session, repo, intent);
        }
        let context = session
            .review_action_context(repo, key.pull_request)
            .map_err(|error| error.to_string())?;
        if !context
            .viewer
            .login
            .eq_ignore_ascii_case(&self.account.login)
        {
            return Err("selected GitHub credential resolved to another account".into());
        }
        if context.pull.state != "OPEN" {
            return Err("pull request is not open for review actions".into());
        }
        match payload {
            ReviewOperationPayload::PendingComment(intent) => {
                prepare_pending_comment_mutation(&mut session, &context, intent)
            }
            ReviewOperationPayload::ImmediateComment(intent) => {
                let variables = json!({
                    "pullRequestId": context.pull.id,
                    "commitOID": intent.position.commit_sha,
                    "event": "COMMENT",
                    "body": Value::Null,
                    "threads": [thread_input(&intent.body, &intent.position)],
                    "clientMutationId": intent.operation_id,
                });
                Ok(PreparedReviewMutation::new(
                    "immediate-comment",
                    ADD_REVIEW_MUTATION,
                    variables,
                    ReviewMutationKind::AddReviewWithComment,
                ))
            }
            ReviewOperationPayload::Submission(intent) => {
                prepare_submission_mutation(&mut session, &context, intent)
            }
            ReviewOperationPayload::PendingFileComment(_) => {
                unreachable!("pending file comments return after their complete preflight")
            }
        }
    }

    fn prepare_auxiliary_mutation(
        &self,
        repo: &Repository,
        number: u64,
        request: &ReviewAuxiliaryRequest,
    ) -> std::result::Result<PreparedAuxiliaryMutation, String> {
        self.validate_repo(repo)
            .map_err(|error| error.to_string())?;
        validate_action_identity(&request.operation_id, "operation_id")?;
        validate_action_identity(&request.attempt_id, "attempt_id")?;
        let mut session = Session::new(self);
        let context = session
            .review_action_context(repo, number)
            .map_err(|error| error.to_string())?;
        if !context
            .viewer
            .login
            .eq_ignore_ascii_case(&self.account.login)
        {
            return Err("selected GitHub credential resolved to another account".into());
        }
        if !matches!(
            &request.action,
            ReviewAuxiliaryAction::UpdateSubmittedSummary { .. }
        ) && context.pull.state != "OPEN"
        {
            return Err("pull request is not open for review actions".into());
        }
        match &request.action {
            ReviewAuxiliaryAction::UpdatePendingSummary { review, body } => {
                validate_action_text(body, true)?;
                validate_coordinates(repo, number, review)?;
                let remote = session
                    .review_node(&review.remote_id)
                    .map_err(|e| e.to_string())?;
                validate_pending_review(
                    &remote,
                    &context,
                    self,
                    remote.commit.as_ref().map(|c| c.oid.as_str()).unwrap_or(""),
                )?;
                Ok(PreparedAuxiliaryMutation::new(
                    "update-pending-summary",
                    UPDATE_REVIEW_MUTATION,
                    json!({"reviewId": review.remote_id, "body": body, "clientMutationId": request.operation_id}),
                    AuxiliaryMutationKind::Review,
                ))
            }
            ReviewAuxiliaryAction::UpdateSubmittedSummary {
                review,
                selected_author,
                submitted_state,
                submitted_commit_sha,
                expected_body,
                body,
            } => {
                validate_action_text(body, true)?;
                validate_action_text(expected_body, true)?;
                validate_coordinates(repo, number, review)?;
                validate_sha(submitted_commit_sha).map_err(|error| error.to_string())?;
                let remote = session
                    .review_node(&review.remote_id)
                    .map_err(|error| error.to_string())?;
                validate_submitted_review(
                    &remote,
                    &context,
                    self,
                    &review.remote_id,
                    selected_author,
                    submitted_state,
                    submitted_commit_sha,
                    expected_body,
                )?;
                Ok(PreparedAuxiliaryMutation::new(
                    "update-submitted-summary",
                    UPDATE_SUBMITTED_REVIEW_MUTATION,
                    json!({"reviewId": review.remote_id, "body": body, "clientMutationId": request.operation_id}),
                    AuxiliaryMutationKind::SubmittedSummary {
                        operation_id: request.operation_id.clone(),
                        review_id: review.remote_id.clone(),
                        body: body.clone(),
                        state: submitted_state.clone(),
                        author: selected_author.clone(),
                        commit_sha: submitted_commit_sha.clone(),
                        pull_request_id: context.pull.id.clone(),
                        pull_request_number: context.pull.number,
                        repository: context.repository.clone(),
                    },
                ))
            }
            ReviewAuxiliaryAction::DeletePendingComment { review, comment } => {
                validate_coordinates(repo, number, review)?;
                validate_coordinates(repo, number, comment)?;
                let remote_review = session
                    .review_node(&review.remote_id)
                    .map_err(|e| e.to_string())?;
                validate_pending_review(
                    &remote_review,
                    &context,
                    self,
                    remote_review
                        .commit
                        .as_ref()
                        .map(|c| c.oid.as_str())
                        .unwrap_or(""),
                )?;
                let remote_comment = session
                    .comment_node(&comment.remote_id)
                    .map_err(|e| e.to_string())?;
                validate_owned_comment(&remote_comment, &remote_review, self)?;
                Ok(PreparedAuxiliaryMutation::new(
                    "delete-pending-comment",
                    DELETE_REVIEW_COMMENT_MUTATION,
                    json!({"commentId": comment.remote_id, "clientMutationId": request.operation_id}),
                    AuxiliaryMutationKind::DeletedComment,
                ))
            }
            ReviewAuxiliaryAction::CancelPendingReview { review } => {
                validate_coordinates(repo, number, review)?;
                let remote = session
                    .review_node(&review.remote_id)
                    .map_err(|e| e.to_string())?;
                validate_pending_review(
                    &remote,
                    &context,
                    self,
                    remote.commit.as_ref().map(|c| c.oid.as_str()).unwrap_or(""),
                )?;
                Ok(PreparedAuxiliaryMutation::new(
                    "cancel-pending-review",
                    DELETE_REVIEW_MUTATION,
                    json!({"reviewId": review.remote_id, "clientMutationId": request.operation_id}),
                    AuxiliaryMutationKind::DeletedReview,
                ))
            }
            ReviewAuxiliaryAction::Reply {
                thread,
                pending_review,
                body,
            } => {
                validate_action_text(body, false)?;
                validate_coordinates(repo, number, thread)?;
                let remote_thread = session
                    .thread_node(&thread.remote_id)
                    .map_err(|e| e.to_string())?;
                validate_thread(&remote_thread, &context)?;
                if !remote_thread.viewer_can_reply {
                    return Err("selected account cannot reply to this review thread".into());
                }
                let pending_id = if let Some(review) = pending_review {
                    validate_coordinates(repo, number, review)?;
                    let remote = session
                        .review_node(&review.remote_id)
                        .map_err(|e| e.to_string())?;
                    validate_pending_review(
                        &remote,
                        &context,
                        self,
                        remote.commit.as_ref().map(|c| c.oid.as_str()).unwrap_or(""),
                    )?;
                    Some(review.remote_id.clone())
                } else {
                    None
                };
                Ok(PreparedAuxiliaryMutation::new(
                    "reply-review-thread",
                    ADD_THREAD_REPLY_MUTATION,
                    json!({"threadId": thread.remote_id, "reviewId": pending_id, "body": body, "clientMutationId": request.operation_id}),
                    AuxiliaryMutationKind::Reply,
                ))
            }
            ReviewAuxiliaryAction::SetThreadResolved { thread, resolved } => {
                validate_coordinates(repo, number, thread)?;
                let remote = session
                    .thread_node(&thread.remote_id)
                    .map_err(|e| e.to_string())?;
                validate_thread(&remote, &context)?;
                if remote.is_resolved == *resolved {
                    return Err("review thread already has the requested resolution state".into());
                }
                if (*resolved && !remote.viewer_can_resolve)
                    || (!*resolved && !remote.viewer_can_unresolve)
                {
                    return Err("selected account cannot change this review thread state".into());
                }
                Ok(PreparedAuxiliaryMutation::new(
                    if *resolved {
                        "resolve-review-thread"
                    } else {
                        "unresolve-review-thread"
                    },
                    if *resolved {
                        RESOLVE_THREAD_MUTATION
                    } else {
                        UNRESOLVE_THREAD_MUTATION
                    },
                    json!({"threadId": thread.remote_id, "clientMutationId": request.operation_id}),
                    AuxiliaryMutationKind::Thread,
                ))
            }
        }
    }

    pub fn prepare_merge(
        &self,
        repo: &Repository,
        number: u64,
        reviewed_head_sha: &str,
    ) -> Result<MergePreparation> {
        self.validate_repo(repo)?;
        validate_sha(reviewed_head_sha)?;
        Session::new(self).merge_preparation(repo, number, reviewed_head_sha)
    }

    /// Execute one guarded PR action. Every merge-capable write carries the
    /// reviewed head as GitHub's server-side `sha`/`expectedHeadOid` condition.
    pub fn execute_merge(
        &self,
        repo: &Repository,
        preparation: &MergePreparation,
        request: &MergeExecutionRequest,
    ) -> ProviderMutationOutcome<MergeAcknowledgement> {
        if let Err(reason) = validate_merge_request(repo, preparation, request) {
            return rejected(reason);
        }
        let fresh = match self.prepare_merge(
            repo,
            preparation.pull_request.pull_request,
            &preparation.reviewed_head_sha,
        ) {
            Ok(fresh) => fresh,
            Err(error) => return rejected(error.to_string()),
        };
        if fresh.current_head_sha != preparation.reviewed_head_sha
            || fresh.current_head_sha != preparation.current_head_sha
        {
            return rejected("pull request head moved since merge preparation");
        }
        let prepared = match prepare_merge_mutation(&fresh, request) {
            Ok(prepared) => prepared,
            Err(reason) => return rejected(reason),
        };
        let context = MutationContext {
            operation_id: request.operation_id.clone(),
            attempt_id: request.attempt_id.clone(),
            action: prepared.action.into(),
            payload: json!({"query": prepared.query, "endpoint": prepared.endpoint, "variables": prepared.variables}),
        };
        let transport = match prepared.transport {
            MergeTransport::Rest => Session::new(self)
                .rest_mutation::<MergeRestResponse>(
                    "PUT",
                    prepared.endpoint.expect("REST endpoint"),
                    prepared.variables,
                )
                .map_ack(MergeMutationAck::Rest),
            MergeTransport::Graphql => Session::new(self)
                .graphql_mutation::<MergeGraphqlData>(
                    prepared.query.expect("GraphQL query"),
                    prepared.variables,
                )
                .map_ack(MergeMutationAck::Graphql),
        };
        match transport {
            MutationTransport::Rejected(reason) => rejected(reason),
            MutationTransport::Uncertain(reason) => {
                ProviderMutationOutcome::Uncertain { context, reason }
            }
            MutationTransport::Acknowledged(ack) => {
                if let Err(reason) = ack.validate(&request.action, &request.operation_id) {
                    return ProviderMutationOutcome::Uncertain { context, reason };
                }
                let observed = match self.prepare_merge(
                    repo,
                    preparation.pull_request.pull_request,
                    &preparation.reviewed_head_sha,
                ) {
                    Ok(observed) => observed,
                    Err(_) => return ProviderMutationOutcome::Uncertain {
                        context,
                        reason: "GitHub acknowledged the merge action but its result could not be reconciled by read".into(),
                    },
                };
                let merged = observed.state == "MERGED";
                let accepted = match request.action {
                    MergeAction::Merge { .. } => merged,
                    MergeAction::EnableAutoMerge { .. } => observed.auto_merge_enabled || merged,
                    MergeAction::DisableAutoMerge => !observed.auto_merge_enabled,
                    MergeAction::Enqueue => observed.in_merge_queue || merged,
                    MergeAction::Dequeue => !observed.in_merge_queue,
                };
                if !accepted && !merged {
                    return ProviderMutationOutcome::Uncertain {
                        context,
                        reason: "GitHub acknowledged the merge action but the reconciliation read did not observe it".into(),
                    };
                }
                ProviderMutationOutcome::Acknowledged(MergeAcknowledgement {
                    operation_id: request.operation_id.clone(),
                    accepted: true,
                    completed: merged,
                    merged,
                    merge_commit_sha: ack.merge_commit_sha(),
                })
            }
        }
    }

    /// Assess source-ref deletion, but keep the write unavailable because
    /// GitHub does not expose an expected-OID condition for `deleteRef`.
    pub fn delete_merged_branch(
        &self,
        repo: &Repository,
        preparation: &MergePreparation,
        request: &BranchDeletionRequest,
    ) -> ProviderMutationOutcome<BranchDeletionAcknowledgement> {
        if let Err(reason) = validate_action_identity(&request.operation_id, "operation_id")
            .and_then(|_| validate_action_identity(&request.attempt_id, "attempt_id"))
        {
            return rejected(reason);
        }
        if request.expected_merged_head_sha != preparation.reviewed_head_sha {
            return rejected("branch deletion expected head differs from the reviewed merge head");
        }
        let fresh = match self.prepare_merge(
            repo,
            preparation.pull_request.pull_request,
            &request.expected_merged_head_sha,
        ) {
            Ok(fresh) => fresh,
            Err(error) => return rejected(error.to_string()),
        };
        if fresh.state != "MERGED"
            || fresh.current_head_sha != request.expected_merged_head_sha
            || fresh.head_repository != repo.full_name()
            || !fresh.viewer_can_delete_head_ref
        {
            return rejected(
                "source branch is not a safely deletable, freshly observed merged ref",
            );
        }
        let Some(_ref_id) = fresh.head_ref_node_id.clone() else {
            return rejected("source branch ref is already absent");
        };
        let descendants =
            match Session::new(self).dependent_pull_requests(repo, &fresh.head_ref_name) {
                Ok(descendants) => descendants,
                Err(error) => return rejected(error.to_string()),
            };
        if !descendants.is_empty() {
            return rejected("source branch is the base of an open descendant pull request");
        }
        rejected(
            "safe branch deletion is unavailable: GitHub deleteRef has no server-side expected-OID condition, so a ref can advance after preflight",
        )
    }

    /// Fetch a pinned PR comparison. A moving live PR is never substituted for the
    /// requested revision. Historical comparisons with >=300 files cannot prove
    /// their full file inventory using this API and are returned with a notice.
    /// Current PRs supplement compare's 300-file cap with paginated PR files,
    /// guarded by matching before/after base/head, update timestamp and file count.
    pub fn comparison(
        &self,
        repo: &Repository,
        number: u64,
        revision: &Revision,
    ) -> Result<Comparison> {
        self.validate_repo(repo)?;
        validate_sha(&revision.base_sha)?;
        validate_sha(&revision.head_sha)?;
        let mut session = Session::new(self);
        let before = session.pull(repo, number)?;
        let comparison: ApiComparison = session.get(&format!(
            "repos/{}/compare/{}...{}?per_page=1&page=1", repo.full_name(), revision.base_sha, revision.head_sha))
            .context("Exact revision comparison unavailable (commits may be inaccessible or unrelated); selected revision was not advanced")?;
        ensure!(
            comparison.base_commit.sha == revision.base_sha,
            "API comparison returned a different base SHA"
        );
        validate_sha(&comparison.merge_base_commit.sha)?;
        ensure!(
            comparison.files.len() <= 300,
            "Invalid compare file response"
        );
        let mut files = comparison.files;
        validate_files(&files)?;
        let mut notices = Vec::new();
        if before.revision() == *revision {
            let expected = before
                .changed_files
                .context("PR response lacks changed-file count")?;
            let mut current_files = Vec::new();
            for page in 1..=MAX_FILE_PAGES {
                let batch: Vec<ApiFile> = session.get(&format!(
                    "repos/{}/pulls/{number}/files?per_page={PAGE_SIZE}&page={page}",
                    repo.full_name()
                ))?;
                ensure!(
                    batch.len() <= PAGE_SIZE,
                    "Invalid PR file pagination response"
                );
                let last = batch.len() < PAGE_SIZE;
                current_files.extend(batch);
                if last {
                    break;
                }
            }
            validate_files(&current_files)?;
            let after = session.pull(repo, number)?;
            if before.revision() != after.revision()
                || before.updated_at != after.updated_at
                || before.changed_files != after.changed_files
            {
                notices.push("PR changed while fetching files; only the immutable comparison is shown. Refresh metadata and retry the selected revision.".to_owned());
            } else {
                // Reject stale/different PR diffs, even if their mutable endpoint
                // happened to retain the same base/head metadata.
                let by_path: HashMap<_, _> = current_files
                    .iter()
                    .map(|f| (f.filename.as_str(), f))
                    .collect();
                let agrees = files.iter().all(|f| {
                    by_path
                        .get(f.filename.as_str())
                        .is_some_and(|live| f.same_change(live))
                }) && (files.len() == 300 || files.len() == current_files.len());
                if agrees && current_files.len() as u64 <= expected {
                    files = current_files;
                    if files.len() as u64 != expected {
                        notices.push(format!("Incomplete file list: GitHub returned {} of {expected} files (PR files API limit: 3,000).", files.len()));
                    }
                } else if current_files.len() as u64 > expected {
                    notices.push(format!(
                        "PR file list is inconsistent with GitHub metadata: the API returned {} files but reported {expected}; only immutable comparison files are shown.",
                        current_files.len()
                    ));
                } else {
                    notices.push("PR file list disagrees with the immutable comparison; only immutable files are shown.".into());
                }
            }
        } else if files.len() == 300 {
            notices.push("Unsupported exact historical file inventory: GitHub caps immutable comparisons at 300 files; the current PR file list belongs to a different revision.".into());
        }
        if files.iter().any(|file| !file.patch_complete()) {
            notices.push("Some patches are unavailable, binary, or truncated. File metadata is retained; media contents are never fetched.".into());
        }
        Ok(Comparison {
            revision: revision.clone(),
            files: files.into_iter().map(ApiFile::into_domain).collect(),
            complete: notices.is_empty(),
            notice: (!notices.is_empty()).then(|| notices.join(" ")),
        })
    }

    /// Enumerate the commit selector against the exact currently displayed PR
    /// base/head. The mutable PR connection is never used for a historical head
    /// and every page repeats the immutable identity guard.
    pub fn commit_inventory(
        &self,
        repo: &Repository,
        number: u64,
        full_revision: &Revision,
    ) -> Result<CommitInventory> {
        self.validate_repo(repo)?;
        validate_sha(&full_revision.base_sha)?;
        validate_sha(&full_revision.head_sha)?;
        ensure!(number > 0, "Invalid pull request number");
        let unavailable = |availability, notice: String| CommitInventory {
            full_revision: full_revision.clone(),
            commits: Vec::new(),
            availability,
            notice: Some(notice),
        };
        let mut session = Session::new(self);
        let mut after: Option<String> = None;
        let mut commits = Vec::new();
        let mut seen = HashSet::new();
        let mut expected_total = None;
        for page in 1..=MAX_COMMIT_INVENTORY_PAGES {
            let response: GraphqlResult<CommitInventoryData> = session.graphql(
                COMMIT_INVENTORY_QUERY,
                json!({
                    "owner": repo.owner,
                    "name": repo.name,
                    "number": number,
                    "after": after,
                }),
            )?;
            let Some(repository) = response.data.repository else {
                return Ok(unavailable(
                    InventoryAvailability::Unavailable,
                    "GitHub did not return the selected repository for commit inventory.".into(),
                ));
            };
            ensure!(
                repository
                    .name_with_owner
                    .eq_ignore_ascii_case(&repo.full_name()),
                "GitHub commit inventory repository identity mismatch"
            );
            let Some(pull) = repository.pull_request else {
                return Ok(unavailable(
                    InventoryAvailability::Unavailable,
                    "GitHub did not return the selected pull request for commit inventory.".into(),
                ));
            };
            ensure!(
                pull.number == number,
                "GitHub commit inventory PR identity mismatch"
            );
            if pull.base_ref_oid != full_revision.base_sha
                || pull.head_ref_oid != full_revision.head_sha
            {
                return Ok(unavailable(
                    if page == 1 {
                        InventoryAvailability::Unavailable
                    } else {
                        InventoryAvailability::Incomplete
                    },
                    "The pull request revision changed or differs from the displayed revision; no mutable commit inventory was accepted.".into(),
                ));
            }
            if response.partial {
                return Ok(unavailable(
                    InventoryAvailability::Incomplete,
                    "GitHub returned a partial commit inventory; selection is unavailable.".into(),
                ));
            }
            let connection = pull.commits;
            ensure!(
                expected_total.is_none_or(|total| total == connection.total_count),
                "GitHub commit inventory total changed during pagination"
            );
            expected_total = Some(connection.total_count);
            if connection.total_count > MAX_COMMIT_INVENTORY {
                return Ok(unavailable(
                    InventoryAvailability::Incomplete,
                    format!(
                        "Commit inventory exceeds the {MAX_COMMIT_INVENTORY}-commit remote limit."
                    ),
                ));
            }
            ensure!(
                connection.nodes.len() <= PAGE_SIZE,
                "Invalid GitHub commit inventory page size"
            );
            if connection.page_info.has_next_page && connection.nodes.len() != PAGE_SIZE {
                return Ok(unavailable(
                    InventoryAvailability::Incomplete,
                    "GitHub returned a truncated commit inventory page; selection is unavailable."
                        .into(),
                ));
            }
            for node in connection.nodes {
                let Some(node) = node else {
                    return Ok(unavailable(
                        InventoryAvailability::Incomplete,
                        "GitHub omitted a commit from the inventory; selection is unavailable."
                            .into(),
                    ));
                };
                validate_sha(&node.commit.oid)?;
                ensure!(
                    seen.insert(node.commit.oid.clone()),
                    "Repeated commit in GitHub inventory"
                );
                ensure!(
                    node.commit.parents.nodes.len() <= 2,
                    "Invalid GitHub commit parent page"
                );
                if node.commit.parents.total_count > node.commit.parents.nodes.len() {
                    return Ok(unavailable(
                        InventoryAvailability::Incomplete,
                        "A commit has more than two parents; its parent inventory is bounded and selection is unavailable.".into(),
                    ));
                }
                let parent_shas = node
                    .commit
                    .parents
                    .nodes
                    .into_iter()
                    .map(|parent| {
                        let parent = parent.context("GitHub omitted a commit parent")?;
                        validate_sha(&parent.oid)?;
                        Ok(parent.oid)
                    })
                    .collect::<Result<Vec<_>>>()?;
                commits.push(CommitInventoryEntry {
                    sha: node.commit.oid,
                    parent_shas,
                    message_headline: node.commit.message_headline,
                    authored_at: node.commit.authored_date,
                    committed_at: node.commit.committed_date,
                });
            }
            if !connection.page_info.has_next_page {
                ensure!(
                    commits.len() == connection.total_count,
                    "GitHub commit inventory ended before its reported total"
                );
                return Ok(CommitInventory {
                    full_revision: full_revision.clone(),
                    commits,
                    availability: InventoryAvailability::Complete,
                    notice: None,
                });
            }
            after = Some(
                connection
                    .page_info
                    .end_cursor
                    .filter(|cursor| !cursor.is_empty())
                    .context("GitHub commit inventory omitted its continuation cursor")?,
            );
        }
        Ok(unavailable(
            InventoryAvailability::Incomplete,
            format!("Commit inventory reached the {MAX_COMMIT_INVENTORY_PAGES}-page remote limit."),
        ))
    }

    /// Fetch an exact direct-tree pair through GitHub's compare API. Because the
    /// endpoint is three-dot, merge-base equality is mandatory. Commit pagination
    /// proves the requested head from the final page instead of a capped first page.
    pub fn direct_comparison(&self, repo: &Repository, revision: &Revision) -> Result<Comparison> {
        self.validate_repo(repo)?;
        validate_sha(&revision.base_sha)?;
        validate_sha(&revision.head_sha)?;
        let mut session = Session::new(self);
        let head: ApiCommit = session.get(&format!(
            "repos/{}/commits/{}",
            repo.full_name(),
            revision.head_sha
        ))?;
        ensure!(
            head.sha == revision.head_sha,
            "GitHub returned a different requested head commit"
        );
        let endpoint = |page| {
            format!(
                "repos/{}/compare/{}...{}?per_page={PAGE_SIZE}&page={page}",
                repo.full_name(),
                revision.base_sha,
                revision.head_sha
            )
        };
        let first: ApiComparison = session
            .get(&endpoint(1))
            .context("Exact direct comparison unavailable; selected endpoints were not advanced")?;
        validate_direct_compare_identity(&first, revision)?;
        ensure!(
            first.commits.len() <= PAGE_SIZE,
            "Invalid direct comparison commit page"
        );
        let page_count = first.total_commits.div_ceil(PAGE_SIZE);
        ensure!(
            page_count <= MAX_COMMIT_INVENTORY_PAGES,
            "Direct comparison exceeds the bounded commit proof limit"
        );
        if first.total_commits == 0 {
            ensure!(
                revision.base_sha == revision.head_sha
                    && first.commits.is_empty()
                    && first.files.is_empty(),
                "Non-identical direct endpoints returned an empty comparison"
            );
        } else {
            ensure!(
                first.commits.len() == first.total_commits.min(PAGE_SIZE),
                "Direct comparison first commit page is truncated"
            );
            let last = if page_count <= 1 {
                first.commits.last().map(|commit| commit.sha.clone())
            } else {
                let last_page: ApiComparison = session.get(&endpoint(page_count))?;
                validate_direct_compare_identity(&last_page, revision)?;
                let expected_last_page = (first.total_commits - 1) % PAGE_SIZE + 1;
                ensure!(
                    last_page.total_commits == first.total_commits
                        && last_page.commits.len() == expected_last_page,
                    "Direct comparison changed during commit pagination"
                );
                last_page.commits.last().map(|commit| commit.sha.clone())
            };
            ensure!(
                last.as_deref() == Some(revision.head_sha.as_str()),
                "GitHub comparison did not end at the requested head"
            );
        }
        ensure!(first.files.len() <= 300, "Invalid compare file response");
        validate_files(&first.files)?;
        let mut notices = Vec::new();
        if first.files.len() == 300 {
            notices.push(
                "Incomplete exact file inventory: GitHub caps immutable comparisons at 300 files."
                    .to_owned(),
            );
        }
        if first.files.iter().any(|file| !file.patch_complete()) {
            notices.push("Some patches are unavailable, binary, or truncated. File metadata is retained; media contents are never fetched.".to_owned());
        }
        Ok(Comparison {
            revision: revision.clone(),
            files: first.files.into_iter().map(ApiFile::into_domain).collect(),
            complete: notices.is_empty(),
            notice: (!notices.is_empty()).then(|| notices.join(" ")),
        })
    }

    fn validate_repo(&self, repo: &Repository) -> Result<()> {
        validate_account(&self.account)?;
        ensure!(
            repo.account == self.account && repo.host == self.account.host,
            "Repository/account mismatch"
        );
        validate_component(&repo.owner, false)?;
        validate_component(&repo.name, true)
    }
}

/// Convenience entry point for repository setup callers.
pub fn accounts() -> Result<Vec<Account>> {
    GithubProvider::accounts()
}

fn discover_accounts(runner: &Runner) -> Result<Vec<Account>> {
    let mut command = runner.gh_command();
    command.args(["auth", "status", "--hostname", HOST, "--json", "hosts"]);
    let bytes = runner.run(command, "discover GitHub accounts")?;
    #[derive(Deserialize)]
    struct Status {
        hosts: HashMap<String, Vec<Identity>>,
    }
    #[derive(Deserialize)]
    struct Identity {
        host: String,
        login: String,
        state: String,
    }
    let status: Status = decode(&bytes)?;
    let identities = status
        .hosts
        .get(HOST)
        .context("No stored GitHub.com accounts; authenticate using gh first")?;
    let mut accounts = Vec::new();
    for identity in identities {
        if identity.host == HOST && identity.state == "success" {
            let account = Account {
                host: identity.host.clone(),
                login: identity.login.clone(),
            };
            validate_account(&account)?;
            if !accounts.contains(&account) {
                accounts.push(account);
            }
        }
    }
    ensure!(
        !accounts.is_empty(),
        "No usable stored GitHub.com accounts; check gh auth status"
    );
    accounts.sort_by(|a, b| a.login.cmp(&b.login));
    Ok(accounts)
}

fn validate_account(account: &Account) -> Result<()> {
    ensure!(
        account.host == HOST,
        "Only github.com accounts are supported in V1"
    );
    validate_component(&account.login, false)
}

fn validate_component(value: &str, repository: bool) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 100
            && value != "."
            && value != ".."
            && (repository || value.as_bytes()[0].is_ascii_alphanumeric())
            && value.bytes().all(|c| c.is_ascii_alphanumeric()
                || c == b'-'
                || (repository && (c == b'_' || c == b'.'))),
        "Invalid GitHub owner, login, or repository name"
    );
    Ok(())
}

fn validate_sha(sha: &str) -> Result<()> {
    ensure!(
        sha.len() == 40
            && sha
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "Exact revision requires full lowercase 40-character commit SHAs"
    );
    Ok(())
}

fn parse_repository(input: &str) -> Result<(String, String)> {
    let path = if let Some(path) = input.strip_prefix("https://github.com/") {
        path
    } else if let Some(path) = input.strip_prefix("ssh://git@github.com/") {
        path
    } else if let Some(path) = input.strip_prefix("git@github.com:") {
        path
    } else {
        input
    };
    let path = path.trim_end_matches('/');
    let (owner, name) = path
        .split_once('/')
        .context("Expected owner/name, a GitHub repository URL, or a local folder")?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    validate_component(owner, false)?;
    validate_component(name, true)?;
    Ok((owner.into(), name.into()))
}

fn rejected<T>(reason: impl Into<String>) -> ProviderMutationOutcome<T> {
    ProviderMutationOutcome::PreflightRejected {
        reason: reason.into(),
    }
}

fn coordinates_match(repo: &Repository, number: u64, value: &ProviderCoordinates) -> bool {
    value.provider == "github"
        && value.host.eq_ignore_ascii_case(&repo.host)
        && value.owner.eq_ignore_ascii_case(&repo.owner)
        && value.repository.eq_ignore_ascii_case(&repo.name)
        && value.pull_request == number
        && !value.remote_id.is_empty()
}

fn validate_review_key(
    provider: &GithubProvider,
    repo: &Repository,
    composition: &ReviewComposition,
    payload: &ReviewOperationPayload,
) -> std::result::Result<(), String> {
    provider
        .validate_repo(repo)
        .map_err(|error| error.to_string())?;
    let key = match payload {
        ReviewOperationPayload::PendingComment(intent) => &intent.key,
        ReviewOperationPayload::PendingFileComment(intent) => &intent.key,
        ReviewOperationPayload::ImmediateComment(intent) => &intent.key,
        ReviewOperationPayload::Submission(intent) => &intent.key,
    };
    if key != &composition.key
        || key.provider != "github"
        || !key.host.eq_ignore_ascii_case(&repo.host)
        || !key.owner.eq_ignore_ascii_case(&repo.owner)
        || !key.repository.eq_ignore_ascii_case(&repo.name)
        || key.account != provider.account
        || key.pull_request == 0
    {
        return Err("frozen review payload belongs to another account, repository, or PR".into());
    }
    Ok(())
}

fn validate_node_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 1024
            && !id.contains('\0')
            && !id.chars().any(char::is_whitespace),
        "Invalid GitHub node ID"
    );
    Ok(())
}

struct Session<'a> {
    provider: &'a GithubProvider,
    started: Instant,
    deadline: Option<Instant>,
    bytes: usize,
    general_read: Option<conditional::GeneralReadTracker>,
    cancellation: Option<Arc<AtomicBool>>,
    byte_limit: usize,
}
impl<'a> Session<'a> {
    fn new(provider: &'a GithubProvider) -> Self {
        let started = Instant::now();
        Self {
            provider,
            started,
            deadline: started.checked_add(Duration::from_secs(180)),
            bytes: 0,
            general_read: conditional::active_general_read_tracker(),
            cancellation: None,
            byte_limit: MAX_OPERATION_BYTES,
        }
    }
    fn new_actions(provider: &'a GithubProvider, cancellation: Arc<AtomicBool>) -> Self {
        let started = Instant::now();
        Self {
            provider,
            started,
            deadline: started.checked_add(Duration::from_secs(180)),
            bytes: 0,
            general_read: conditional::active_general_read_tracker(),
            cancellation: Some(cancellation),
            byte_limit: actions_jobs_logs::MAX_ACTIONS_JSON_BYTES,
        }
    }
    fn new_actions_until(
        provider: &'a GithubProvider,
        cancellation: Arc<AtomicBool>,
        deadline: Instant,
    ) -> Self {
        Self {
            provider,
            started: Instant::now(),
            deadline: Some(deadline),
            bytes: 0,
            general_read: conditional::active_general_read_tracker(),
            cancellation: Some(cancellation),
            byte_limit: actions_jobs_logs::MAX_ACTIONS_JSON_BYTES,
        }
    }
    fn get<T: serde::de::DeserializeOwned>(&mut self, endpoint: &str) -> Result<T> {
        if self.general_read.is_some() {
            return match self.get_conditional(endpoint, None) {
                Ok(conditional::ConditionalGet::Modified { value, .. }) => Ok(value),
                Ok(conditional::ConditionalGet::NotModified { .. }) => {
                    self.record_general_failure(conditional::GeneralReadFailureKind::Incomplete);
                    bail!("Unexpected not-modified response for an unconditional GitHub read")
                }
                Err(error) => Err(anyhow::Error::new(error)),
            };
        }
        ensure!(
            self.started.elapsed() < Duration::from_secs(180),
            "GitHub operation time limit reached"
        );
        let mut token_command = self.provider.runner.gh_command();
        token_command.args([
            "auth",
            "token",
            "--hostname",
            &self.provider.account.host,
            "--user",
            &self.provider.account.login,
        ]);
        let token = self
            .provider
            .runner
            .run(token_command, "resolve selected GitHub credential")?;
        let token = std::str::from_utf8(&token)
            .map_err(|_| anyhow::anyhow!("Invalid credential encoding"))?
            .trim();
        ensure!(
            !token.is_empty() && token.len() <= 4096 && !token.chars().any(char::is_whitespace),
            "Missing or invalid selected GitHub credential"
        );
        let mut command = self.provider.runner.gh_command();
        command.env("GH_TOKEN", token).args([
            "api",
            "--hostname",
            HOST,
            "--method",
            "GET",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            API_VERSION,
            endpoint,
        ]);
        let bytes = self.provider.runner.run(command, "GitHub read request")?;
        self.bytes += bytes.len();
        ensure!(
            self.bytes <= MAX_OPERATION_BYTES,
            "GitHub operation output limit reached"
        );
        decode(&bytes)
    }

    fn get_conditional<T: serde::de::DeserializeOwned>(
        &mut self,
        endpoint: &str,
        validators: Option<&conditional::RestValidators>,
    ) -> std::result::Result<conditional::ConditionalGet<T>, conditional::RestReadError> {
        use conditional::{ConditionalGet, RestReadError};

        if let Some(tracker) = &self.general_read {
            tracker.check_not_halted()?;
        }
        if self
            .deadline
            .is_none_or(|deadline| Instant::now() >= deadline)
        {
            self.record_general_failure(conditional::GeneralReadFailureKind::Incomplete);
            return Err(RestReadError::operation_limit());
        }
        let mut token_command = self.provider.runner.gh_command();
        token_command.args([
            "auth",
            "token",
            "--hostname",
            &self.provider.account.host,
            "--user",
            &self.provider.account.login,
        ]);
        let token = self
            .provider
            .runner
            .run_maybe_cancelled(
                token_command,
                "resolve selected GitHub credential",
                self.cancellation.as_deref(),
                self.deadline,
            )
            .map_err(|_| {
                self.record_general_failure(conditional::GeneralReadFailureKind::Unavailable);
                RestReadError::credential()
            })?;
        let token = std::str::from_utf8(&token)
            .map_err(|_| {
                self.record_general_failure(conditional::GeneralReadFailureKind::Unavailable);
                RestReadError::credential()
            })?
            .trim();
        if token.is_empty() || token.len() > 4096 || token.chars().any(char::is_whitespace) {
            self.record_general_failure(conditional::GeneralReadFailureKind::Unavailable);
            return Err(RestReadError::credential());
        }
        let mut command = self.provider.runner.gh_command();
        command.env("GH_TOKEN", token).args([
            "api",
            "--include",
            "--hostname",
            HOST,
            "--method",
            "GET",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            API_VERSION,
        ]);
        if let Some(validators) = validators
            && let Some((name, value)) = validators.request_header()
        {
            command.arg("--header").arg(format!("{name}: {value}"));
        }
        command.arg(endpoint);
        let output = self
            .provider
            .runner
            .run_with_status_maybe_cancelled(
                command,
                "GitHub conditional read request",
                self.cancellation.as_deref(),
                self.deadline,
            )
            .map_err(|_| {
                self.record_general_failure(conditional::GeneralReadFailureKind::Unavailable);
                RestReadError::transport()
            })?;
        let parsed = conditional::parse_included_response(&output.stdout, output.status.success())
            .inspect_err(|error| {
                self.record_general_error(error);
            })?;
        match &parsed {
            ConditionalGet::Modified { metadata, .. }
            | ConditionalGet::NotModified { metadata } => {
                self.record_general_poll(&metadata.poll);
            }
        }
        self.bytes = self.bytes.checked_add(output.stdout.len()).ok_or_else(|| {
            self.record_general_failure(conditional::GeneralReadFailureKind::Incomplete);
            RestReadError::operation_limit()
        })?;
        if self.bytes > self.byte_limit {
            self.record_general_failure(conditional::GeneralReadFailureKind::Incomplete);
            return Err(RestReadError::operation_limit());
        }
        match parsed {
            ConditionalGet::Modified { value, metadata } => {
                let value = decode(&value).map_err(|_| {
                    self.record_general_failure(conditional::GeneralReadFailureKind::Incomplete);
                    RestReadError::invalid_body(metadata.poll.clone())
                })?;
                Ok(ConditionalGet::Modified { value, metadata })
            }
            ConditionalGet::NotModified { metadata } => {
                Ok(ConditionalGet::NotModified { metadata })
            }
        }
    }

    fn graphql<T: serde::de::DeserializeOwned>(
        &mut self,
        query: &str,
        variables: Value,
    ) -> Result<GraphqlResult<T>> {
        ensure!(
            self.started.elapsed() < Duration::from_secs(180),
            "GitHub operation time limit reached"
        );
        ensure!(
            query.trim_start().starts_with("query ") && !query.contains("mutation"),
            "Only read-only GitHub GraphQL queries are allowed"
        );
        if self.general_read.is_some() {
            return self.graphql_included(query, variables);
        }
        let mut token_command = self.provider.runner.gh_command();
        token_command.args([
            "auth",
            "token",
            "--hostname",
            &self.provider.account.host,
            "--user",
            &self.provider.account.login,
        ]);
        let token = self
            .provider
            .runner
            .run(token_command, "resolve selected GitHub credential")?;
        let token = std::str::from_utf8(&token)
            .map_err(|_| anyhow::anyhow!("Invalid credential encoding"))?
            .trim();
        ensure!(
            !token.is_empty() && token.len() <= 4096 && !token.chars().any(char::is_whitespace),
            "Missing or invalid selected GitHub credential"
        );
        let input = serde_json::to_vec(&json!({"query": query, "variables": variables}))
            .context("Cannot encode GitHub GraphQL read")?;
        ensure!(
            input.len() <= 1024 * 1024,
            "GitHub GraphQL input limit reached"
        );
        let mut command = self.provider.runner.gh_command();
        command.env("GH_TOKEN", token).args([
            "api",
            "--hostname",
            HOST,
            "--method",
            "POST",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            API_VERSION,
            "graphql",
            "--input",
            "-",
        ]);
        let bytes = self
            .provider
            .runner
            .run_with_input(command, "GitHub GraphQL read", &input)?;
        self.bytes += bytes.len();
        ensure!(
            self.bytes <= MAX_OPERATION_BYTES,
            "GitHub operation output limit reached"
        );
        let envelope: GraphqlEnvelope<T> = decode(&bytes)?;
        let data = envelope
            .data
            .context("GitHub GraphQL read returned no usable data")?;
        Ok(GraphqlResult {
            data,
            partial: !envelope.errors.is_empty(),
        })
    }

    fn graphql_included<T: serde::de::DeserializeOwned>(
        &mut self,
        query: &str,
        variables: Value,
    ) -> Result<GraphqlResult<T>> {
        use conditional::ConditionalGet;

        if let Some(tracker) = &self.general_read {
            tracker.check_not_halted().map_err(anyhow::Error::new)?;
        }
        let input = serde_json::to_vec(&json!({"query": query, "variables": variables}))
            .context("Cannot encode GitHub GraphQL read")?;
        ensure!(
            input.len() <= 1024 * 1024,
            "GitHub GraphQL input limit reached"
        );
        let token = self.credential().inspect_err(|_| {
            self.record_general_failure(conditional::GeneralReadFailureKind::Unavailable);
        })?;
        let mut command = self.provider.runner.gh_command();
        command.env("GH_TOKEN", token).args([
            "api",
            "--include",
            "--hostname",
            HOST,
            "--method",
            "POST",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            API_VERSION,
            "graphql",
            "--input",
            "-",
        ]);
        let output = self
            .provider
            .runner
            .run_with_input_status(command, "GitHub included GraphQL read", &input)
            .inspect_err(|_| {
                self.record_general_failure(conditional::GeneralReadFailureKind::Unavailable);
            })?;
        let parsed =
            conditional::parse_graphql_included_response(&output.stdout, output.status.success())
                .map_err(|error| {
                self.record_general_error(&error);
                anyhow::Error::new(error)
            })?;
        let (bytes, metadata) = match parsed {
            ConditionalGet::Modified { value, metadata } => (value, metadata),
            ConditionalGet::NotModified { metadata } => {
                self.record_general_poll(&metadata.poll);
                self.record_general_failure(conditional::GeneralReadFailureKind::Incomplete);
                bail!("Unexpected not-modified response for a GraphQL read")
            }
        };
        self.record_general_poll(&metadata.poll);
        let Some(total_bytes) = self.bytes.checked_add(output.stdout.len()) else {
            if let Some(delay) = metadata.graphql_rate_limit.clone() {
                self.record_general_poll(&conditional::RestPollDirective {
                    x_poll_interval: None,
                    rate_limit: Some(delay),
                });
            } else {
                self.record_general_failure(conditional::GeneralReadFailureKind::Incomplete);
            }
            bail!("GitHub operation output limit reached");
        };
        self.bytes = total_bytes;
        if self.bytes > MAX_OPERATION_BYTES {
            if let Some(delay) = metadata.graphql_rate_limit.clone() {
                self.record_general_poll(&conditional::RestPollDirective {
                    x_poll_interval: None,
                    rate_limit: Some(delay),
                });
            } else {
                self.record_general_failure(conditional::GeneralReadFailureKind::Incomplete);
            }
            bail!("GitHub operation output limit reached");
        }
        let envelope: GraphqlEnvelope<T> = decode(&bytes).inspect_err(|_| {
            if let Some(delay) = metadata.graphql_rate_limit.clone() {
                self.record_general_poll(&conditional::RestPollDirective {
                    x_poll_interval: None,
                    rate_limit: Some(delay),
                });
            } else {
                self.record_general_failure(conditional::GeneralReadFailureKind::Incomplete);
            }
        })?;
        if !envelope.errors.is_empty()
            && let Some(delay) = metadata.graphql_rate_limit
        {
            self.record_general_poll(&conditional::RestPollDirective {
                x_poll_interval: None,
                rate_limit: Some(delay),
            });
            bail!("GitHub GraphQL read was stopped by server rate limiting");
        }
        let data = envelope
            .data
            .context("GitHub GraphQL read returned no usable data")?;
        Ok(GraphqlResult {
            data,
            partial: !envelope.errors.is_empty(),
        })
    }

    fn record_general_poll(&self, poll: &conditional::RestPollDirective) {
        if let Some(tracker) = &self.general_read {
            tracker.record_poll(poll);
        }
    }

    fn record_general_failure(&self, kind: conditional::GeneralReadFailureKind) {
        if let Some(tracker) = &self.general_read {
            tracker.record_failure(kind);
        }
    }

    fn record_general_error(&self, error: &conditional::RestReadError) {
        if let Some(tracker) = &self.general_read {
            tracker.record_error(error);
        }
    }

    fn graphql_mutation<T: serde::de::DeserializeOwned>(
        &mut self,
        query: &str,
        variables: Value,
    ) -> MutationTransport<T> {
        if self.started.elapsed() >= Duration::from_secs(180) {
            return MutationTransport::Rejected("GitHub operation time limit reached".into());
        }
        if !query.trim_start().starts_with("mutation ") {
            return MutationTransport::Rejected(
                "Only explicit GitHub GraphQL mutations may use the write transport".into(),
            );
        }
        let input = match serde_json::to_vec(&json!({"query": query, "variables": variables})) {
            Ok(input) if input.len() <= MAX_MUTATION_INPUT_BYTES => input,
            Ok(_) => {
                return MutationTransport::Rejected(
                    "GitHub GraphQL mutation input limit reached".into(),
                );
            }
            Err(_) => {
                return MutationTransport::Rejected(
                    "Cannot encode bounded GitHub GraphQL mutation".into(),
                );
            }
        };
        let token = match self.credential() {
            Ok(token) => token,
            Err(error) => return MutationTransport::Rejected(error.to_string()),
        };
        let mut command = self.provider.runner.gh_command();
        command.env("GH_TOKEN", token).args([
            "api",
            "--hostname",
            HOST,
            "--method",
            "POST",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            API_VERSION,
            "graphql",
            "--input",
            "-",
        ]);
        let bytes =
            match self
                .provider
                .runner
                .run_with_input(command, "GitHub GraphQL mutation", &input)
            {
                Ok(bytes) => bytes,
                Err(_) => {
                    return MutationTransport::Uncertain(
                        "GitHub mutation started but its result was not acknowledged".into(),
                    );
                }
            };
        self.bytes += bytes.len();
        if self.bytes > MAX_OPERATION_BYTES {
            return MutationTransport::Uncertain(
                "GitHub mutation response exceeded the operation output limit".into(),
            );
        }
        let envelope: GraphqlEnvelope<T> = match decode(&bytes) {
            Ok(envelope) => envelope,
            Err(_) => {
                return MutationTransport::Uncertain(
                    "GitHub mutation returned an invalid or incomplete acknowledgement".into(),
                );
            }
        };
        if !envelope.errors.is_empty() || envelope.data.is_none() {
            return MutationTransport::Uncertain(
                "GitHub mutation did not return an unambiguous acknowledgement".into(),
            );
        }
        MutationTransport::Acknowledged(envelope.data.expect("checked"))
    }

    fn rest_mutation<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        endpoint: String,
        variables: Value,
    ) -> MutationTransport<T> {
        if !matches!(method, "PUT" | "POST" | "PATCH" | "DELETE")
            || endpoint.contains(['\0', '\n', '\r'])
            || !endpoint.starts_with("repos/")
        {
            return MutationTransport::Rejected("unsupported REST mutation entrypoint".into());
        }
        let input = match serde_json::to_vec(&variables) {
            Ok(input) if input.len() <= MAX_MUTATION_INPUT_BYTES => input,
            Ok(_) => {
                return MutationTransport::Rejected(
                    "GitHub REST mutation input limit reached".into(),
                );
            }
            Err(_) => {
                return MutationTransport::Rejected(
                    "Cannot encode bounded GitHub REST mutation".into(),
                );
            }
        };
        let token = match self.credential() {
            Ok(token) => token,
            Err(error) => return MutationTransport::Rejected(error.to_string()),
        };
        let mut command = self.provider.runner.gh_command();
        command.env("GH_TOKEN", token).args([
            "api",
            "--hostname",
            HOST,
            "--method",
            method,
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            API_VERSION,
            &endpoint,
            "--input",
            "-",
        ]);
        let bytes =
            match self
                .provider
                .runner
                .run_with_input(command, "GitHub REST mutation", &input)
            {
                Ok(bytes) => bytes,
                Err(_) => {
                    return MutationTransport::Uncertain(
                        "GitHub mutation started but its result was not acknowledged".into(),
                    );
                }
            };
        match decode(&bytes) {
            Ok(value) => MutationTransport::Acknowledged(value),
            Err(_) => MutationTransport::Uncertain(
                "GitHub mutation returned an invalid or incomplete acknowledgement".into(),
            ),
        }
    }

    fn credential(&self) -> Result<String> {
        let mut token_command = self.provider.runner.gh_command();
        token_command.args([
            "auth",
            "token",
            "--hostname",
            &self.provider.account.host,
            "--user",
            &self.provider.account.login,
        ]);
        let token = self
            .provider
            .runner
            .run(token_command, "resolve selected GitHub credential")?;
        let token = std::str::from_utf8(&token)
            .map_err(|_| anyhow::anyhow!("Invalid credential encoding"))?
            .trim();
        ensure!(
            !token.is_empty() && token.len() <= 4096 && !token.chars().any(char::is_whitespace),
            "Missing or invalid selected GitHub credential"
        );
        Ok(token.to_owned())
    }

    fn review_action_context(
        &mut self,
        repo: &Repository,
        number: u64,
    ) -> Result<ReviewActionContext> {
        ensure!(number > 0 && number <= i32::MAX as u64, "Invalid PR number");
        let response: GraphqlResult<ReviewActionContextData> = self.graphql(
            REVIEW_ACTION_CONTEXT_QUERY,
            json!({"owner": repo.owner, "name": repo.name, "number": number}),
        )?;
        ensure!(!response.partial, "GitHub action preflight was partial");
        let repository = response
            .data
            .repository
            .context("GitHub action repository is unavailable")?;
        ensure!(
            repository
                .name_with_owner
                .eq_ignore_ascii_case(&repo.full_name()),
            "GitHub action repository mismatch"
        );
        let pull = repository
            .pull_request
            .context("GitHub action pull request is unavailable")?;
        ensure!(
            pull.number == number
                && pull.url == format!("https://{}/{}/pull/{number}", repo.host, repo.full_name()),
            "GitHub action pull request mismatch"
        );
        validate_sha(&pull.head_ref_oid)?;
        Ok(ReviewActionContext {
            viewer: response.data.viewer,
            repository: repository.name_with_owner,
            pull,
        })
    }

    fn pending_file_preflight(
        &mut self,
        repo: &Repository,
        number: u64,
    ) -> Result<PendingFilePreflight> {
        let response: GraphqlResult<PendingFilePreflightData> = self.graphql(
            PENDING_FILE_PREFLIGHT_QUERY,
            json!({"owner": repo.owner, "name": repo.name, "number": number}),
        )?;
        ensure!(
            !response.partial,
            "GitHub pending file-comment preflight was partial"
        );
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
            .context("GitHub pending file-comment repository is unavailable")?;
        ensure!(
            repository
                .name_with_owner
                .eq_ignore_ascii_case(&repo.full_name()),
            "GitHub pending file-comment repository mismatch"
        );
        let pull = repository
            .pull_request
            .context("GitHub pending file-comment pull request is unavailable")?;
        ensure!(
            pull.number == number
                && pull.url == format!("https://{}/{}/pull/{number}", repo.host, repo.full_name()),
            "GitHub pending file-comment pull request mismatch"
        );
        validate_sha(&pull.base_ref_oid)?;
        validate_sha(&pull.head_ref_oid)?;
        ensure!(
            !pull.reviews.page_info.has_next_page && pull.reviews.nodes.iter().all(Option::is_some),
            "GitHub pending-review list is incomplete"
        );
        let mut selected = pull.reviews.nodes.into_iter().flatten().filter(|review| {
            review.author.as_ref().is_some_and(|author| {
                author
                    .login
                    .eq_ignore_ascii_case(&self.provider.account.login)
            })
        });
        let review = selected
            .next()
            .context("GitHub returned no pending review for the selected account")?;
        ensure!(
            selected.next().is_none(),
            "GitHub returned multiple pending reviews for the selected account"
        );
        Ok(PendingFilePreflight {
            viewer_login: response.data.viewer.login,
            repository: repository.name_with_owner,
            pull_id: pull.id,
            pull_number: pull.number,
            pull_state: pull.state,
            base_sha: pull.base_ref_oid,
            head_sha: pull.head_ref_oid,
            review,
        })
    }

    fn review_node(&mut self, id: &str) -> Result<ActionReviewNode> {
        validate_node_id(id)?;
        let response: GraphqlResult<ActionReviewNodeData> =
            self.graphql(REVIEW_NODE_QUERY, json!({"id": id}))?;
        ensure!(
            !response.partial,
            "GitHub review identity preflight was partial"
        );
        response
            .data
            .node
            .context("GitHub review ID is unavailable or has the wrong type")
    }

    fn comment_node(&mut self, id: &str) -> Result<ActionCommentNode> {
        validate_node_id(id)?;
        let response: GraphqlResult<ActionCommentNodeData> =
            self.graphql(COMMENT_NODE_QUERY, json!({"id": id}))?;
        ensure!(
            !response.partial,
            "GitHub comment identity preflight was partial"
        );
        response
            .data
            .node
            .context("GitHub comment ID is unavailable or has the wrong type")
    }

    fn thread_node(&mut self, id: &str) -> Result<ActionThreadNode> {
        validate_node_id(id)?;
        let response: GraphqlResult<ActionThreadNodeData> =
            self.graphql(THREAD_NODE_QUERY, json!({"id": id}))?;
        ensure!(
            !response.partial,
            "GitHub thread identity preflight was partial"
        );
        response
            .data
            .node
            .context("GitHub thread ID is unavailable or has the wrong type")
    }

    fn merge_preparation(
        &mut self,
        repo: &Repository,
        number: u64,
        reviewed_head_sha: &str,
    ) -> Result<MergePreparation> {
        ensure!(number > 0 && number <= i32::MAX as u64, "Invalid PR number");
        let response: GraphqlResult<MergePreparationData> = self.graphql(
            MERGE_PREPARATION_QUERY,
            json!({"owner": repo.owner, "name": repo.name, "number": number}),
        )?;
        ensure!(!response.partial, "GitHub merge preparation was partial");
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
            .context("GitHub merge repository is unavailable")?;
        ensure!(
            repository
                .name_with_owner
                .eq_ignore_ascii_case(&repo.full_name()),
            "GitHub merge repository mismatch"
        );
        let pull = repository
            .pull_request
            .context("GitHub merge pull request is unavailable")?;
        ensure!(
            pull.number == number
                && pull.url == format!("https://{}/{}/pull/{number}", repo.host, repo.full_name()),
            "GitHub merge pull request mismatch"
        );
        validate_sha(&pull.head_ref_oid)?;
        let mut allowed_methods = Vec::new();
        if repository.merge_commit_allowed {
            allowed_methods.push(MergeMethod::Merge);
        }
        if repository.squash_merge_allowed {
            allowed_methods.push(MergeMethod::Squash);
        }
        if repository.rebase_merge_allowed {
            allowed_methods.push(MergeMethod::Rebase);
        }
        let mut blockers = Vec::new();
        if pull.head_ref_oid != reviewed_head_sha {
            blockers.push("head moved from the reviewed revision".into());
        }
        if pull.state != "OPEN" && pull.state != "MERGED" {
            blockers.push("pull request is closed without merge".into());
        }
        if pull.is_draft {
            blockers.push("pull request is a draft".into());
        }
        if pull.mergeable != "MERGEABLE" && pull.state != "MERGED" {
            blockers.push(format!("mergeability is {}", pull.mergeable));
        }
        if !matches!(pull.merge_state_status.as_str(), "CLEAN" | "HAS_HOOKS")
            && pull.state != "MERGED"
        {
            blockers.push(format!("merge state is {}", pull.merge_state_status));
        }
        if matches!(
            pull.review_decision.as_deref(),
            Some("CHANGES_REQUESTED" | "REVIEW_REQUIRED")
        ) {
            blockers.push(format!(
                "review decision is {}",
                pull.review_decision.as_deref().unwrap_or("UNKNOWN")
            ));
        }
        let check_status = pull
            .status_check_rollup
            .as_ref()
            .map(|value| value.state.clone())
            .unwrap_or_else(|| "NONE".into());
        if !matches!(check_status.as_str(), "SUCCESS" | "NONE") {
            blockers.push(format!("check status is {check_status}"));
        }
        if allowed_methods.is_empty() {
            blockers.push("repository has no supported merge method".into());
        }
        let permission = repository.viewer_permission.clone();
        if !matches!(permission.as_deref(), Some("WRITE" | "MAINTAIN" | "ADMIN"))
            && !pull.viewer_can_merge_as_admin
        {
            blockers.push("selected account lacks merge permission".into());
        }
        let head_ref = pull.head_ref.as_ref();
        if let Some(reference) = head_ref {
            ensure!(
                reference.name == pull.head_ref_name && reference.target.oid == pull.head_ref_oid,
                "GitHub head ref changed during merge preparation"
            );
        }
        let queue_required = pull.is_merge_queue_enabled;
        Ok(MergePreparation {
            pull_request: coordinates(repo, number, pull.id.clone()),
            pull_request_node_id: pull.id,
            reviewed_head_sha: reviewed_head_sha.into(),
            current_head_sha: pull.head_ref_oid,
            head_ref_name: pull.head_ref_name,
            head_ref_node_id: head_ref.map(|reference| reference.id.clone()),
            head_repository: pull
                .head_repository
                .map(|repository| repository.name_with_owner)
                .unwrap_or_default(),
            state: pull.state,
            draft: pull.is_draft,
            mergeable: pull.mergeable,
            merge_state_status: pull.merge_state_status,
            review_status: pull.review_decision.unwrap_or_else(|| "NONE".into()),
            check_status,
            repository_permission: permission,
            allowed_methods,
            blockers,
            auto_merge_allowed: repository.auto_merge_allowed,
            auto_merge_enabled: pull.auto_merge_request.is_some(),
            can_enable_auto_merge: pull.viewer_can_enable_auto_merge,
            can_disable_auto_merge: pull.viewer_can_disable_auto_merge,
            merge_queue_required: queue_required,
            in_merge_queue: pull.is_in_merge_queue || pull.merge_queue_entry.is_some(),
            viewer_can_merge_as_admin: pull.viewer_can_merge_as_admin,
            viewer_can_delete_head_ref: pull.viewer_can_delete_head_ref,
            preferred_headlines: vec![
                (MergeMethod::Merge, pull.merge_headline),
                (MergeMethod::Squash, pull.squash_headline),
                (MergeMethod::Rebase, pull.rebase_headline),
            ],
            preferred_bodies: vec![
                (MergeMethod::Merge, pull.merge_body),
                (MergeMethod::Squash, pull.squash_body),
                (MergeMethod::Rebase, pull.rebase_body),
            ],
        })
    }

    fn dependent_pull_requests(&mut self, repo: &Repository, branch: &str) -> Result<Vec<u64>> {
        validate_ref_name(branch)?;
        let response: GraphqlResult<DependentPullData> = self.graphql(
            DEPENDENT_PULLS_QUERY,
            json!({"owner": repo.owner, "name": repo.name, "base": branch}),
        )?;
        ensure!(
            !response.partial,
            "GitHub dependent-PR preflight was partial"
        );
        let repository = response
            .data
            .repository
            .context("GitHub dependent-PR repository is unavailable")?;
        ensure!(
            repository
                .name_with_owner
                .eq_ignore_ascii_case(&repo.full_name()),
            "GitHub dependent-PR repository mismatch"
        );
        ensure!(
            !repository.pull_requests.page_info.has_next_page,
            "Dependent PRs exceed the explicit 100-PR safety bound"
        );
        Ok(repository
            .pull_requests
            .nodes
            .into_iter()
            .flatten()
            .map(|pull| pull.number)
            .collect())
    }

    fn hydrate_metadata(&mut self, repo: &Repository, pulls: &mut [PullRequest]) -> Result<()> {
        if pulls.is_empty() {
            return Ok(());
        }
        ensure!(
            pulls.len() <= PAGE_SIZE,
            "GitHub metadata batch is too large"
        );
        ensure!(
            pulls
                .iter()
                .all(|pull| pull.number > 0 && pull.number <= i32::MAX as u64),
            "PR number is outside GitHub GraphQL limits"
        );
        let (query, variables) = bulk_metadata_query(repo, pulls);
        let response: GraphqlResult<BulkMetadataData> = self.graphql(&query, variables)?;
        let repository = response
            .data
            .repository
            .context("GitHub metadata repository is unavailable")?;
        ensure!(
            repository
                .name_with_owner
                .eq_ignore_ascii_case(&repo.full_name()),
            "GitHub metadata repository mismatch"
        );
        for (index, pull) in pulls.iter_mut().enumerate() {
            let alias = format!("pr{index}");
            let metadata = repository
                .pulls
                .get(&alias)
                .and_then(Option::as_ref)
                .context("PR changed or became inaccessible during metadata refresh")?;
            metadata.apply(repo, pull, response.partial)?;
        }
        Ok(())
    }

    fn details(&mut self, repo: &Repository, number: u64) -> Result<PullRequestDetails> {
        let mut cursors = DetailsCursors::initial();
        let mut builder = DetailsBuilder::default();
        for page in 0..MAX_DETAILS_PAGES {
            let response: GraphqlResult<DetailsData> =
                self.graphql(DETAILS_QUERY, details_variables(repo, number, &cursors))?;
            let DetailsData { viewer, repository } = response.data;
            let repository = repository.context("GitHub details repository is unavailable")?;
            ensure!(
                repository
                    .name_with_owner
                    .eq_ignore_ascii_case(&repo.full_name()),
                "GitHub details repository mismatch"
            );
            let viewer_can_administer = repository.viewer_can_administer;
            validate_node_id(&repository.id)?;
            let pull = repository
                .pull_request
                .context("PR details are unavailable or inaccessible")?;
            pull.validate(repo, number, &repository.id)?;
            let next = builder.absorb(
                repo,
                pull,
                viewer,
                DetailsPageContext {
                    viewer_can_administer,
                    partial: response.partial,
                    first: page == 0,
                    checks_requested: cursors.checks.include,
                },
            )?;
            if next.done() {
                return builder.finish(number);
            }
            cursors = next;
        }
        if cursors.comments.include || cursors.reviews.include || cursors.threads.include {
            builder.notices.push(format!(
                "Activity pagination stopped at the explicit {}-page limit.",
                MAX_DETAILS_PAGES
            ));
            builder.activity_complete = false;
        }
        if cursors.checks.include {
            builder.notices.push(format!(
                "Check pagination stopped at the explicit {}-page limit.",
                MAX_DETAILS_PAGES
            ));
            builder.checks_complete = false;
        }
        builder.finish(number)
    }

    fn pending_review(
        &mut self,
        repo: &Repository,
        number: u64,
    ) -> Result<Option<PendingReviewSnapshot>> {
        let response: GraphqlResult<PendingReviewData> = self.graphql(
            PENDING_REVIEW_QUERY,
            json!({"owner": repo.owner, "name": repo.name, "number": number}),
        )?;
        ensure!(
            !response.partial,
            "GitHub pending-review import was partial"
        );
        let viewer_login = response.data.viewer.map(|viewer| viewer.login);
        if let Some(viewer_login) = &viewer_login {
            ensure!(
                viewer_login.eq_ignore_ascii_case(&self.provider.account.login),
                "selected GitHub credential resolved to another account"
            );
        }
        let repository = response
            .data
            .repository
            .context("GitHub pending-review repository is unavailable")?;
        ensure!(
            repository
                .name_with_owner
                .eq_ignore_ascii_case(&repo.full_name()),
            "GitHub pending-review repository mismatch"
        );
        let pull = repository
            .pull_request
            .context("GitHub pending-review PR is unavailable")?;
        ensure!(pull.number == number, "GitHub pending-review PR mismatch");
        if let Some(url) = &pull.url {
            ensure!(
                url == &format!("https://{}/{}/pull/{number}", repo.host, repo.full_name()),
                "GitHub pending-review PR mismatch"
            );
        }
        if let Some(base) = &pull.base_ref_oid {
            validate_sha(base)?;
        }
        if let Some(head) = &pull.head_ref_oid {
            validate_sha(head)?;
        }
        ensure!(
            !pull.reviews.page_info.has_next_page && pull.reviews.nodes.iter().all(Option::is_some),
            "Pending-review list exceeds the explicit 100-review import bound"
        );
        let mut selected = pull.reviews.nodes.into_iter().flatten().filter(|review| {
            review.author.as_ref().is_some_and(|author| {
                author
                    .login
                    .eq_ignore_ascii_case(&self.provider.account.login)
            })
        });
        let Some(review) = selected.next() else {
            return Ok(None);
        };
        ensure!(
            selected.next().is_none(),
            "GitHub returned multiple pending reviews for the selected account"
        );
        let review_id = review.id.clone();
        let mut comments_complete = !review.comments.page_info.has_next_page
            && review.comments.nodes.iter().all(Option::is_some);
        let comments = review
            .comments
            .nodes
            .into_iter()
            .flatten()
            .filter_map(|comment| {
                if comment
                    .pull_request_review
                    .as_ref()
                    .map(|parent| parent.id.as_str())
                    != Some(review_id.as_str())
                {
                    comments_complete = false;
                    return None;
                }
                Some(LinkedReviewComment {
                    pull_request_review_id: review_id.clone(),
                    comment: comment.into_domain(repo, number, None),
                })
            })
            .collect();
        let file_comment_source = (comments_complete
            && pull.state.as_deref() == Some("OPEN")
            && review.state == "PENDING"
            && review.submitted_at.is_none()
            && review.author.as_ref().is_some_and(|author| {
                viewer_login
                    .as_ref()
                    .is_some_and(|viewer| author.login.eq_ignore_ascii_case(viewer))
            })
            && review.commit.as_ref().map(|commit| commit.oid.as_str())
                == pull.head_ref_oid.as_deref()
            && pull.id.is_some()
            && pull.base_ref_oid.is_some()
            && pull.head_ref_oid.is_some()
            && viewer_login.is_some())
        .then(|| PendingFileCommentSource {
            viewer_login: viewer_login.clone().expect("checked"),
            repository: repo.clone(),
            pull_request: coordinates(repo, number, pull.id.clone().expect("checked")),
            pull_request_state: pull.state.clone().expect("checked"),
            current_base_sha: pull.base_ref_oid.clone().expect("checked"),
            current_head_sha: pull.head_ref_oid.clone().expect("checked"),
            review: coordinates(repo, number, review.id.clone()),
            review_author: review
                .author
                .as_ref()
                .map(|author| author.login.clone())
                .unwrap_or_default(),
            review_commit_sha: review
                .commit
                .as_ref()
                .map(|commit| commit.oid.clone())
                .unwrap_or_default(),
        });
        Ok(Some(PendingReviewSnapshot {
            review: PullRequestReview {
                coordinates: coordinates(repo, number, review.id),
                author: review.author.map(|author| author.login),
                body: review.body,
                state: review.state,
                submitted_at: review.submitted_at,
                commit_sha: review.commit.map(|commit| commit.oid),
                edit_summary_capability: None,
                dismissal_capability: None,
                url: review.url,
            },
            comments,
            comments_complete,
            file_comment_source,
        }))
    }

    fn pull(&mut self, repo: &Repository, number: u64) -> Result<ApiPullRequest> {
        ensure!(number > 0, "PR number must be positive");
        let pull: ApiPullRequest =
            self.get(&format!("repos/{}/pulls/{number}", repo.full_name()))?;
        pull.validate(repo, Some(number))?;
        Ok(pull)
    }
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    // Do not propagate remote bodies or subprocess stderr through diagnostics.
    serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("Invalid or incomplete GitHub JSON response"))
}

#[derive(Clone)]
struct Runner {
    gh: PathBuf,
    git: PathBuf,
    curl: PathBuf,
    timeout: Duration,
    input_timeout: Option<Duration>,
    output_limit: usize,
}
impl Default for Runner {
    fn default() -> Self {
        Self {
            gh: "gh".into(),
            git: "git".into(),
            curl: "/usr/bin/curl".into(),
            timeout: Duration::from_secs(30),
            input_timeout: None,
            output_limit: 16 * 1024 * 1024,
        }
    }
}
impl Runner {
    fn gh_command(&self) -> Command {
        let mut command = Command::new(&self.gh);
        for name in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GITHUB_ENTERPRISE_TOKEN",
            "GH_HOST",
            "GH_REPO",
            "GH_DEBUG",
            "DEBUG",
            "GH_FORCE_TTY",
            "GH_HTTP_UNIX_SOCKET",
        ] {
            command.env_remove(name);
        }
        command
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
            .env("GH_PAGER", "/bin/cat")
            .env("PAGER", "/bin/cat");
        command
    }
    fn git_command(&self) -> Command {
        let mut command = Command::new(&self.git);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0");
        command
    }
    fn run(&self, mut command: Command, action: &'static str) -> Result<Vec<u8>> {
        let output = self.run_inner(&mut command, action, None)?;
        ensure_success(output, action)
    }
    fn run_maybe_cancelled(
        &self,
        mut command: Command,
        action: &'static str,
        cancellation: Option<&AtomicBool>,
        deadline: Option<Instant>,
    ) -> Result<Vec<u8>> {
        let output =
            self.run_inner_maybe_cancelled(&mut command, action, None, cancellation, deadline)?;
        ensure_success(output, action)
    }
    fn run_with_input(
        &self,
        mut command: Command,
        action: &'static str,
        input: &[u8],
    ) -> Result<Vec<u8>> {
        let output = self.run_inner(&mut command, action, Some(input))?;
        ensure_success(output, action)
    }
    fn run_with_input_status(
        &self,
        mut command: Command,
        action: &'static str,
        input: &[u8],
    ) -> Result<RunnerOutput> {
        self.run_inner(&mut command, action, Some(input))
    }
    fn run_inner(
        &self,
        command: &mut Command,
        action: &'static str,
        input: Option<&[u8]>,
    ) -> Result<RunnerOutput> {
        self.run_inner_maybe_cancelled(command, action, input, None, None)
    }
    fn run_with_status_maybe_cancelled(
        &self,
        mut command: Command,
        action: &'static str,
        cancellation: Option<&AtomicBool>,
        deadline: Option<Instant>,
    ) -> Result<RunnerOutput> {
        self.run_inner_maybe_cancelled(&mut command, action, None, cancellation, deadline)
    }
    fn run_inner_maybe_cancelled(
        &self,
        command: &mut Command,
        action: &'static str,
        input: Option<&[u8]>,
        cancellation: Option<&AtomicBool>,
        deadline: Option<Instant>,
    ) -> Result<RunnerOutput> {
        let input = input.map(<[u8]>::to_vec);
        command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let started = Instant::now();
        let mut child = command.spawn().map_err(|_| {
            anyhow::anyhow!("Cannot start subprocess to {action}; check installed gh/Git")
        })?;
        let (tx, rx) = mpsc::channel();
        let mut input_done = input.is_none();
        if let Some(input) = input {
            let Some(mut stdin) = child.stdin.take() else {
                terminate_process_group(&mut child);
                bail!("Cannot open bounded input pipe to {action}");
            };
            let tx = tx.clone();
            if thread::Builder::new()
                .name("provider-input".into())
                .spawn(move || {
                    let _ = tx.send(PipeEvent::Input(stdin.write_all(&input)));
                })
                .is_err()
            {
                terminate_process_group(&mut child);
                bail!("Cannot start bounded input writer for {action}");
            }
        }
        let limit = self.output_limit;
        let Some(stdout) = child.stdout.take() else {
            terminate_process_group(&mut child);
            bail!("Cannot open subprocess output pipe while attempting to {action}");
        };
        let Some(stderr) = child.stderr.take() else {
            terminate_process_group(&mut child);
            bail!("Cannot open subprocess output pipe while attempting to {action}");
        };
        for (is_stdout, pipe) in [
            (true, Box::new(stdout) as Box<dyn Read + Send>),
            (false, Box::new(stderr) as Box<dyn Read + Send>),
        ] {
            let tx = tx.clone();
            if thread::Builder::new()
                .name("provider-output".into())
                .spawn(move || {
                    let mut bytes = Vec::new();
                    let result = pipe
                        .take(limit as u64 + 1)
                        .read_to_end(&mut bytes)
                        .map(|_| bytes);
                    let _ = tx.send(PipeEvent::Output(is_stdout, result));
                })
                .is_err()
            {
                terminate_process_group(&mut child);
                bail!("Cannot start bounded output reader while attempting to {action}");
            }
        }
        drop(tx);
        let mut output = None;
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut status = None;
        loop {
            if cancellation.is_some_and(|cancelled| cancelled.load(Ordering::Acquire)) {
                terminate_process_group(&mut child);
                bail!("Cancelled while attempting to {action}");
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                terminate_process_group(&mut child);
                bail!("Timed out attempting to {action}");
            }
            loop {
                match rx.try_recv() {
                    Ok(PipeEvent::Input(result)) => {
                        if result.is_err() {
                            terminate_process_group(&mut child);
                            bail!("Cannot send bounded input to {action}");
                        }
                        input_done = true;
                    }
                    Ok(PipeEvent::Output(is_stdout, result)) => {
                        let bytes = match result {
                            Ok(bytes) if bytes.len() <= limit => bytes,
                            Ok(bytes) => {
                                if is_stdout && action == "GitHub conditional read request" {
                                    conditional::record_general_poll_from_included_prefix(&bytes);
                                }
                                terminate_process_group(&mut child);
                                bail!(
                                    "Subprocess output failed or exceeded limit while attempting to {action}"
                                );
                            }
                            Err(_) => {
                                terminate_process_group(&mut child);
                                bail!(
                                    "Subprocess output failed or exceeded limit while attempting to {action}"
                                );
                            }
                        };
                        if is_stdout {
                            output = Some(bytes);
                            stdout_done = true;
                        } else {
                            stderr_done = true;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if !input_done || !stdout_done || !stderr_done {
                            terminate_process_group(&mut child);
                            bail!("Subprocess I/O stopped while attempting to {action}");
                        }
                        break;
                    }
                }
            }
            if status.is_none() {
                match child.try_wait() {
                    Ok(Some(current)) => status = Some(current),
                    Ok(None) => {}
                    Err(_) => {
                        terminate_process_group(&mut child);
                        bail!("Cannot wait for subprocess to {action}");
                    }
                }
            }
            if let Some(status) = status
                && input_done
                && stdout_done
                && stderr_done
            {
                return Ok(RunnerOutput {
                    stdout: output.unwrap_or_default(),
                    status,
                });
            }
            if !input_done && started.elapsed() >= self.input_timeout.unwrap_or(self.timeout) {
                terminate_process_group(&mut child);
                bail!("Timed out sending bounded input to {action}");
            }
            if started.elapsed() >= self.timeout {
                terminate_process_group(&mut child);
                bail!("Timed out attempting to {action}");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

struct RunnerOutput {
    stdout: Vec<u8>,
    status: std::process::ExitStatus,
}

fn ensure_success(output: RunnerOutput, action: &'static str) -> Result<Vec<u8>> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        bail!(
            "Failed to {action} (exit {}). Check authentication, permissions, rate limits, and connectivity; subprocess output withheld.",
            output
                .status
                .code()
                .map_or_else(|| "signal".into(), |code| code.to_string())
        )
    }
}

enum PipeEvent {
    Input(std::io::Result<()>),
    Output(bool, std::io::Result<Vec<u8>>),
}

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

fn terminate_process_group(child: &mut Child) {
    terminate_process_group_id(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

fn terminate_process_group_id(id: u32) {
    if let Ok(pid) = i32::try_from(id) {
        // Children are spawned as process-group leaders. Signal the negative
        // PGID so helpers holding pipes cannot outlive the deadline.
        let _ = unsafe { kill(-pid, 9) };
    }
}

struct GraphqlResult<T> {
    data: T,
    partial: bool,
}

enum MutationTransport<T> {
    Rejected(String),
    Acknowledged(T),
    Uncertain(String),
}

impl<T> MutationTransport<T> {
    fn map_ack<U>(self, map: impl FnOnce(T) -> U) -> MutationTransport<U> {
        match self {
            Self::Rejected(reason) => MutationTransport::Rejected(reason),
            Self::Acknowledged(value) => MutationTransport::Acknowledged(map(value)),
            Self::Uncertain(reason) => MutationTransport::Uncertain(reason),
        }
    }
}

#[derive(Deserialize)]
struct GraphqlEnvelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlConnection<T> {
    nodes: Vec<Option<T>>,
    page_info: PageInfo,
}

#[derive(Deserialize)]
struct CommitInventoryData {
    repository: Option<CommitInventoryRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitInventoryRepository {
    name_with_owner: String,
    pull_request: Option<CommitInventoryPull>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitInventoryPull {
    number: u64,
    base_ref_oid: String,
    head_ref_oid: String,
    commits: CommitInventoryConnection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitInventoryConnection {
    total_count: usize,
    nodes: Vec<Option<CommitInventoryNode>>,
    page_info: PageInfo,
}

#[derive(Deserialize)]
struct CommitInventoryNode {
    commit: CommitInventoryCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitInventoryCommit {
    oid: String,
    message_headline: String,
    authored_date: String,
    committed_date: String,
    parents: CommitParentConnection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitParentConnection {
    total_count: usize,
    nodes: Vec<Option<GraphqlOid>>,
}

#[derive(Deserialize)]
struct GraphqlActor {
    login: String,
}

#[derive(Deserialize)]
struct BulkReview {
    author: Option<GraphqlActor>,
    state: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BulkReviewRequest {
    requested_reviewer: Option<BulkRequestedReviewer>,
}

#[derive(Deserialize)]
struct BulkRequestedReviewer {
    login: Option<String>,
}

#[derive(Deserialize)]
struct BulkRollup {
    state: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BulkPullMetadata {
    number: u64,
    url: String,
    review_decision: Option<String>,
    status_check_rollup: Option<BulkRollup>,
    comments: Option<GraphqlConnection<GraphqlActorNode>>,
    reviews: Option<GraphqlConnection<BulkReview>>,
    review_requests: Option<GraphqlConnection<BulkReviewRequest>>,
    assignees: Option<GraphqlConnection<GraphqlActor>>,
}

#[derive(Deserialize)]
struct GraphqlActorNode {
    author: Option<GraphqlActor>,
}

impl BulkPullMetadata {
    fn apply(&self, repo: &Repository, pull: &mut PullRequest, partial: bool) -> Result<()> {
        ensure!(
            self.number == pull.number
                && self.url
                    == format!(
                        "https://{}/{}/pull/{}",
                        repo.host,
                        repo.full_name(),
                        pull.number
                    ),
            "GitHub metadata PR identity mismatch"
        );
        pull.review_status = map_review_status(self.review_decision.as_deref(), partial).into();
        pull.check_status = map_check_status(
            self.status_check_rollup
                .as_ref()
                .map(|rollup| rollup.state.as_str()),
            partial,
        )
        .into();

        let mut participants = Vec::new();
        add_identity(&mut participants, &pull.author);
        for reviewer in &pull.reviewers {
            if !reviewer.starts_with("team:") {
                add_identity(&mut participants, reviewer);
            }
        }
        for assignee in &pull.assignees {
            add_identity(&mut participants, assignee);
        }

        let mut complete = !partial;
        match &self.comments {
            Some(connection) => {
                complete &= connection_complete(connection);
                for node in connection.nodes.iter().flatten() {
                    if let Some(author) = &node.author {
                        add_identity(&mut participants, &author.login);
                    }
                }
            }
            None => complete = false,
        }
        match &self.reviews {
            Some(connection) => {
                complete &= connection_complete(connection);
                for review in connection.nodes.iter().flatten() {
                    if review.state != "PENDING"
                        && let Some(author) = &review.author
                    {
                        add_identity(&mut participants, &author.login);
                    }
                }
            }
            None => complete = false,
        }
        match &self.review_requests {
            Some(connection) => {
                complete &= connection_complete(connection);
                for request in connection.nodes.iter().flatten() {
                    if let Some(login) = request
                        .requested_reviewer
                        .as_ref()
                        .and_then(|reviewer| reviewer.login.as_deref())
                    {
                        add_identity(&mut participants, login);
                    }
                }
            }
            None => complete = false,
        }
        match &self.assignees {
            Some(connection) => {
                complete &= connection_complete(connection);
                for assignee in connection.nodes.iter().flatten() {
                    add_identity(&mut participants, &assignee.login);
                }
            }
            None => complete = false,
        }
        participants.sort_by_key(|login| login.to_ascii_lowercase());
        pull.participants = participants;
        pull.participants_complete = complete;
        pull.participants_notice = (!complete).then(|| {
            format!(
                "Participant identities are partial; comments, submitted reviews, requested reviewers, and assignees are each limited to {PARTICIPANT_LIMIT} per PR, and partial API fields are not treated as complete."
            )
        });
        Ok(())
    }
}

fn connection_complete<T>(connection: &GraphqlConnection<T>) -> bool {
    !connection.page_info.has_next_page && connection.nodes.iter().all(Option::is_some)
}

fn add_identity(identities: &mut Vec<String>, login: &str) {
    if !login.is_empty()
        && !identities
            .iter()
            .any(|known| known.eq_ignore_ascii_case(login))
    {
        identities.push(login.to_owned());
    }
}

fn map_review_status(decision: Option<&str>, partial: bool) -> &'static str {
    match decision {
        Some("APPROVED") => "Approved",
        Some("CHANGES_REQUESTED") => "ChangesRequested",
        Some("REVIEW_REQUIRED") => "ReviewRequired",
        Some(_) => "UNKNOWN",
        None if partial => "UNKNOWN",
        None => "None",
    }
}

fn map_check_status(state: Option<&str>, partial: bool) -> &'static str {
    match state {
        Some("SUCCESS") => "Passing",
        Some("PENDING" | "EXPECTED") => "Pending",
        Some("ERROR" | "FAILURE") => "Failing",
        Some(_) => "UNKNOWN",
        None if partial => "UNKNOWN",
        None => "None",
    }
}

fn bulk_metadata_query(repo: &Repository, pulls: &[PullRequest]) -> (String, Value) {
    let mut definitions = vec!["$owner: String!".to_owned(), "$name: String!".to_owned()];
    let mut selections = Vec::with_capacity(pulls.len());
    let mut variables = Map::new();
    variables.insert("owner".into(), Value::String(repo.owner.clone()));
    variables.insert("name".into(), Value::String(repo.name.clone()));
    for (index, pull) in pulls.iter().enumerate() {
        definitions.push(format!("$n{index}: Int!"));
        variables.insert(format!("n{index}"), Value::from(pull.number));
        selections.push(format!(
            "pr{index}: pullRequest(number: $n{index}) {{\n\
                number url reviewDecision statusCheckRollup {{ state }}\n\
                comments(first: {PARTICIPANT_LIMIT}) {{ nodes {{ author {{ login }} }} pageInfo {{ hasNextPage endCursor }} }}\n\
                reviews(first: {PARTICIPANT_LIMIT}, states: [APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]) {{ nodes {{ author {{ login }} state }} pageInfo {{ hasNextPage endCursor }} }}\n\
                reviewRequests(first: {PARTICIPANT_LIMIT}) {{ nodes {{ requestedReviewer {{ ... on User {{ login }} }} }} pageInfo {{ hasNextPage endCursor }} }}\n\
                assignees(first: {PARTICIPANT_LIMIT}) {{ nodes {{ login }} pageInfo {{ hasNextPage endCursor }} }}\n\
            }}"
        ));
    }
    let query = format!(
        "query PullRequestMetadata({}) {{ repository(owner: $owner, name: $name) {{ nameWithOwner {} }} }}",
        definitions.join(", "),
        selections.join("\n")
    );
    (query, Value::Object(variables))
}

#[derive(Deserialize)]
struct BulkMetadataData {
    repository: Option<BulkMetadataRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BulkMetadataRepository {
    name_with_owner: String,
    #[serde(flatten)]
    pulls: HashMap<String, Option<BulkPullMetadata>>,
}

const COMMIT_INVENTORY_QUERY: &str = r#"query PullRequestCommitInventory(
  $owner: String!, $name: String!, $number: Int!, $after: String
) {
  repository(owner: $owner, name: $name) {
    nameWithOwner
    pullRequest(number: $number) {
      number
      baseRefOid
      headRefOid
      commits(first: 100, after: $after) {
        totalCount
        pageInfo { hasNextPage endCursor }
        nodes {
          commit {
            oid
            messageHeadline
            authoredDate
            committedDate
            parents(first: 2) {
              totalCount
              nodes { oid }
            }
          }
        }
      }
    }
  }
}"#;

const MERGE_PREPARATION_QUERY: &str = r#"query MergePreparation(
  $owner: String!, $name: String!, $number: Int!
) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    id nameWithOwner viewerPermission mergeCommitAllowed squashMergeAllowed
    rebaseMergeAllowed autoMergeAllowed
    pullRequest(number: $number) {
      id number url state isDraft headRefOid headRefName mergeable mergeStateStatus
      reviewDecision viewerCanEnableAutoMerge viewerCanDisableAutoMerge
      viewerCanMergeAsAdmin viewerCanDeleteHeadRef
      headRepository { nameWithOwner }
      headRef { id name target { oid } }
      isMergeQueueEnabled isInMergeQueue
      autoMergeRequest { enabledAt }
      mergeQueueEntry { id }
      statusCheckRollup { state }
      mergeHeadline: viewerMergeHeadlineText(mergeType: MERGE)
      mergeBody: viewerMergeBodyText(mergeType: MERGE)
      squashHeadline: viewerMergeHeadlineText(mergeType: SQUASH)
      squashBody: viewerMergeBodyText(mergeType: SQUASH)
      rebaseHeadline: viewerMergeHeadlineText(mergeType: REBASE)
      rebaseBody: viewerMergeBodyText(mergeType: REBASE)
    }
  }
}"#;

const DEPENDENT_PULLS_QUERY: &str = r#"query DependentPullRequests(
  $owner: String!, $name: String!, $base: String!
) {
  repository(owner: $owner, name: $name) {
    nameWithOwner
    pullRequests(first: 100, states: OPEN, baseRefName: $base) {
      nodes { number }
      pageInfo { hasNextPage endCursor }
    }
  }
}"#;

const ENABLE_AUTO_MERGE_MUTATION: &str = r#"mutation EnableAutoMerge(
  $pullRequestId: ID!, $expectedHeadOid: GitObjectID!, $mergeMethod: PullRequestMergeMethod!,
  $commitHeadline: String, $commitBody: String, $clientMutationId: String!
) {
  enablePullRequestAutoMerge(input: {
    pullRequestId: $pullRequestId, expectedHeadOid: $expectedHeadOid,
    mergeMethod: $mergeMethod, commitHeadline: $commitHeadline, commitBody: $commitBody,
    clientMutationId: $clientMutationId
  }) { clientMutationId pullRequest { id state mergedAt autoMergeRequest { enabledAt } } }
}"#;

const DISABLE_AUTO_MERGE_MUTATION: &str = r#"mutation DisableAutoMerge(
  $pullRequestId: ID!, $clientMutationId: String!
) {
  disablePullRequestAutoMerge(input: {
    pullRequestId: $pullRequestId, clientMutationId: $clientMutationId
  }) { clientMutationId pullRequest { id state mergedAt autoMergeRequest { enabledAt } } }
}"#;

const ENQUEUE_PULL_MUTATION: &str = r#"mutation EnqueuePull(
  $pullRequestId: ID!, $expectedHeadOid: GitObjectID!, $clientMutationId: String!
) {
  enqueuePullRequest(input: {
    pullRequestId: $pullRequestId, expectedHeadOid: $expectedHeadOid,
    clientMutationId: $clientMutationId
  }) { clientMutationId mergeQueueEntry { id } }
}"#;

const DEQUEUE_PULL_MUTATION: &str = r#"mutation DequeuePull(
  $pullRequestId: ID!, $clientMutationId: String!
) {
  dequeuePullRequest(input: {
    id: $pullRequestId, clientMutationId: $clientMutationId
  }) { clientMutationId mergeQueueEntry { id } }
}"#;

#[derive(Deserialize)]
struct MergePreparationData {
    viewer: GraphqlActor,
    repository: Option<MergeRepository>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeRepository {
    name_with_owner: String,
    viewer_permission: Option<String>,
    merge_commit_allowed: bool,
    squash_merge_allowed: bool,
    rebase_merge_allowed: bool,
    auto_merge_allowed: bool,
    pull_request: Option<MergePull>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergePull {
    id: String,
    number: u64,
    url: String,
    state: String,
    is_draft: bool,
    head_ref_oid: String,
    head_ref_name: String,
    mergeable: String,
    merge_state_status: String,
    review_decision: Option<String>,
    viewer_can_enable_auto_merge: bool,
    viewer_can_disable_auto_merge: bool,
    viewer_can_merge_as_admin: bool,
    viewer_can_delete_head_ref: bool,
    head_repository: Option<ActionRepositoryIdentity>,
    head_ref: Option<MergeRef>,
    is_merge_queue_enabled: bool,
    is_in_merge_queue: bool,
    auto_merge_request: Option<Value>,
    merge_queue_entry: Option<GraphqlNodeId>,
    status_check_rollup: Option<MergeCheckRollup>,
    merge_headline: String,
    merge_body: String,
    squash_headline: String,
    squash_body: String,
    rebase_headline: String,
    rebase_body: String,
}
#[derive(Deserialize)]
struct MergeRef {
    id: String,
    name: String,
    target: GraphqlOid,
}
#[derive(Deserialize)]
struct MergeCheckRollup {
    state: String,
}

#[derive(Deserialize)]
struct DependentPullData {
    repository: Option<DependentRepository>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DependentRepository {
    name_with_owner: String,
    pull_requests: GraphqlConnection<DependentPull>,
}
#[derive(Deserialize)]
struct DependentPull {
    number: u64,
}

#[derive(Deserialize)]
struct MergeRestResponse {
    sha: Option<String>,
    merged: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeGraphqlData {
    enable_pull_request_auto_merge: Option<MergePullPayload>,
    disable_pull_request_auto_merge: Option<MergePullPayload>,
    enqueue_pull_request: Option<MergeQueuePayload>,
    dequeue_pull_request: Option<MergeQueuePayload>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergePullPayload {
    client_mutation_id: Option<String>,
    pull_request: Option<GraphqlNodeId>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeQueuePayload {
    client_mutation_id: Option<String>,
    merge_queue_entry: Option<GraphqlNodeId>,
}

enum MergeMutationAck {
    Rest(MergeRestResponse),
    Graphql(MergeGraphqlData),
}
impl MergeMutationAck {
    fn validate(
        &self,
        action: &MergeAction,
        operation_id: &str,
    ) -> std::result::Result<(), String> {
        match (self, action) {
            (Self::Rest(response), MergeAction::Merge { .. }) if response.merged => Ok(()),
            (Self::Graphql(data), MergeAction::EnableAutoMerge { .. })
                if data
                    .enable_pull_request_auto_merge
                    .as_ref()
                    .is_some_and(|p| {
                        p.client_mutation_id.as_deref() == Some(operation_id)
                            && p.pull_request.is_some()
                    }) =>
            {
                Ok(())
            }
            (Self::Graphql(data), MergeAction::DisableAutoMerge)
                if data
                    .disable_pull_request_auto_merge
                    .as_ref()
                    .is_some_and(|p| {
                        p.client_mutation_id.as_deref() == Some(operation_id)
                            && p.pull_request.is_some()
                    }) =>
            {
                Ok(())
            }
            (Self::Graphql(data), MergeAction::Enqueue)
                if data.enqueue_pull_request.as_ref().is_some_and(|p| {
                    p.client_mutation_id.as_deref() == Some(operation_id)
                        && p.merge_queue_entry.is_some()
                }) =>
            {
                Ok(())
            }
            (Self::Graphql(data), MergeAction::Dequeue)
                if data
                    .dequeue_pull_request
                    .as_ref()
                    .is_some_and(|p| p.client_mutation_id.as_deref() == Some(operation_id)) =>
            {
                Ok(())
            }
            _ => Err("GitHub omitted the merge action acknowledgement".into()),
        }
    }
    fn merge_commit_sha(&self) -> Option<String> {
        match self {
            Self::Rest(response) => response.sha.clone(),
            Self::Graphql(_) => None,
        }
    }
}

enum MergeTransport {
    Rest,
    Graphql,
}
struct PreparedMergeMutation {
    action: &'static str,
    transport: MergeTransport,
    query: Option<&'static str>,
    endpoint: Option<String>,
    variables: Value,
}

fn prepare_merge_mutation(
    preparation: &MergePreparation,
    request: &MergeExecutionRequest,
) -> std::result::Result<PreparedMergeMutation, String> {
    let common = |action, query, variables| PreparedMergeMutation {
        action,
        transport: MergeTransport::Graphql,
        query: Some(query),
        endpoint: None,
        variables,
    };
    match &request.action {
        MergeAction::Merge {
            method,
            commit_title,
            commit_message,
        } => {
            if preparation.merge_queue_required {
                return Err(
                    "base branch requires merge queue; ordinary merge is unavailable".into(),
                );
            }
            if !preparation.blockers.is_empty() {
                return Err(format!(
                    "merge is blocked: {}",
                    preparation.blockers.join("; ")
                ));
            }
            if !preparation.allowed_methods.contains(method) {
                return Err("requested merge method is unavailable".into());
            }
            Ok(PreparedMergeMutation {
                action: "merge-pull-request",
                transport: MergeTransport::Rest,
                query: None,
                endpoint: Some(format!(
                    "repos/{}/pulls/{}/merge",
                    preparation.pull_request.owner.clone()
                        + "/"
                        + &preparation.pull_request.repository,
                    preparation.pull_request.pull_request
                )),
                variables: json!({"sha": preparation.reviewed_head_sha, "merge_method": method.rest_name(), "commit_title": commit_title, "commit_message": commit_message}),
            })
        }
        MergeAction::EnableAutoMerge {
            method,
            commit_title,
            commit_message,
        } => {
            if !preparation.auto_merge_allowed
                || !preparation.can_enable_auto_merge
                || preparation.auto_merge_enabled
            {
                return Err("auto-merge cannot be enabled for this PR".into());
            }
            if !preparation.allowed_methods.contains(method) {
                return Err("requested auto-merge method is unavailable".into());
            }
            Ok(common(
                "enable-auto-merge",
                ENABLE_AUTO_MERGE_MUTATION,
                json!({
                    "pullRequestId": preparation.pull_request_node_id,
                    "expectedHeadOid": preparation.reviewed_head_sha,
                    "mergeMethod": method.graphql_name(), "commitHeadline": commit_title,
                    "commitBody": commit_message, "clientMutationId": request.operation_id,
                }),
            ))
        }
        MergeAction::DisableAutoMerge => {
            if !preparation.auto_merge_enabled || !preparation.can_disable_auto_merge {
                return Err("auto-merge is not enabled or cannot be disabled".into());
            }
            Ok(common(
                "disable-auto-merge",
                DISABLE_AUTO_MERGE_MUTATION,
                json!({"pullRequestId": preparation.pull_request_node_id, "clientMutationId": request.operation_id}),
            ))
        }
        MergeAction::Enqueue => {
            if !preparation.merge_queue_required || preparation.in_merge_queue {
                return Err("merge queue is unavailable or PR is already queued".into());
            }
            Ok(common(
                "enqueue-pull-request",
                ENQUEUE_PULL_MUTATION,
                json!({"pullRequestId": preparation.pull_request_node_id, "expectedHeadOid": preparation.reviewed_head_sha, "clientMutationId": request.operation_id}),
            ))
        }
        MergeAction::Dequeue => {
            if !preparation.in_merge_queue {
                return Err("pull request is not in the merge queue".into());
            }
            Ok(common(
                "dequeue-pull-request",
                DEQUEUE_PULL_MUTATION,
                json!({"pullRequestId": preparation.pull_request_node_id, "clientMutationId": request.operation_id}),
            ))
        }
    }
}

fn validate_merge_request(
    repo: &Repository,
    preparation: &MergePreparation,
    request: &MergeExecutionRequest,
) -> std::result::Result<(), String> {
    validate_coordinates(
        repo,
        preparation.pull_request.pull_request,
        &preparation.pull_request,
    )?;
    validate_action_identity(&request.operation_id, "operation_id")?;
    validate_action_identity(&request.attempt_id, "attempt_id")?;
    validate_sha(&preparation.reviewed_head_sha).map_err(|error| error.to_string())?;
    if preparation.pull_request.remote_id != preparation.pull_request_node_id {
        return Err("merge preparation PR node identity is inconsistent".into());
    }
    Ok(())
}

fn validate_ref_name(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 1024 && !value.contains(['\0', '\n', '\r']),
        "Invalid Git ref name"
    );
    Ok(())
}

const PENDING_FILE_PREFLIGHT_QUERY: &str = r#"query PendingFileCommentPreflight(
  $owner: String!, $name: String!, $number: Int!
) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    nameWithOwner
    pullRequest(number: $number) {
      id number url state baseRefOid headRefOid
      reviews(first: 100, states: PENDING) {
        nodes { id state submittedAt author { login } commit { oid } }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"#;

const PENDING_REVIEW_QUERY: &str = r#"query PendingReview(
  $owner: String!, $name: String!, $number: Int!
) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    nameWithOwner
    pullRequest(number: $number) {
      id number url state baseRefOid headRefOid
      reviews(first: 100, states: PENDING) {
        nodes {
          id author { login } body state submittedAt commit { oid } url
          comments(first: 100) {
            nodes {
              id author { login } body createdAt updatedAt url path subjectType line originalLine
              startLine originalStartLine diffHunk outdated commit { oid } originalCommit { oid }
              pullRequestReview { id }
            }
            pageInfo { hasNextPage endCursor }
          }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"#;

const REVIEW_ACTION_CONTEXT_QUERY: &str = r#"query ReviewActionContext(
  $owner: String!, $name: String!, $number: Int!
) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    nameWithOwner
    pullRequest(number: $number) { id number url headRefOid state }
  }
}"#;

const REVIEW_NODE_QUERY: &str = r#"query ReviewIdentity($id: ID!) {
  node(id: $id) {
    ... on PullRequestReview {
      id body state submittedAt author { login } commit { oid }
      viewerDidAuthor viewerCanUpdate viewerCannotUpdateReasons
      pullRequest { id number repository { nameWithOwner } }
    }
  }
}"#;

const COMMENT_NODE_QUERY: &str = r#"query ReviewCommentIdentity($id: ID!) {
  node(id: $id) {
    ... on PullRequestReviewComment {
      id author { login } subjectType
      pullRequestReview {
        id state author { login } commit { oid }
        pullRequest { id number repository { nameWithOwner } }
      }
    }
  }
}"#;

const THREAD_NODE_QUERY: &str = r#"query ReviewThreadIdentity($id: ID!) {
  node(id: $id) {
    ... on PullRequestReviewThread {
      id isResolved viewerCanReply viewerCanResolve viewerCanUnresolve
      pullRequest { id number repository { nameWithOwner } }
    }
  }
}"#;

const ADD_REVIEW_MUTATION: &str = r#"mutation AddReview(
  $pullRequestId: ID!, $commitOID: GitObjectID!, $event: PullRequestReviewEvent,
  $body: String, $threads: [DraftPullRequestReviewThread], $clientMutationId: String!
) {
  addPullRequestReview(input: {
    pullRequestId: $pullRequestId, commitOID: $commitOID, event: $event,
    body: $body, threads: $threads, clientMutationId: $clientMutationId
  }) {
    pullRequestReview { id state commit { oid } comments(last: 1) { nodes { id body pullRequestReview { id } } } }
  }
}"#;

const ADD_REVIEW_THREAD_MUTATION: &str = r#"mutation AddReviewThread(
  $pullRequestReviewId: ID!, $body: String!, $path: String!, $line: Int!,
  $side: DiffSide!, $startLine: Int, $startSide: DiffSide, $clientMutationId: String!
) {
  addPullRequestReviewThread(input: {
    pullRequestReviewId: $pullRequestReviewId, body: $body, path: $path,
    line: $line, side: $side, startLine: $startLine, startSide: $startSide,
    clientMutationId: $clientMutationId
  }) { thread { comments(first: 1) { nodes { id body pullRequestReview { id } } } } }
}"#;

const ADD_PENDING_FILE_THREAD_MUTATION: &str = r#"mutation AddPendingFileReviewThread(
  $pullRequestReviewId: ID!, $body: String!, $path: String!,
  $subjectType: PullRequestReviewThreadSubjectType!, $clientMutationId: String!
) {
  addPendingFileReviewThread: addPullRequestReviewThread(input: {
    pullRequestReviewId: $pullRequestReviewId, body: $body, path: $path,
    subjectType: $subjectType, clientMutationId: $clientMutationId
  }) {
    clientMutationId
    thread {
      id path subjectType
      comments(first: 2) {
        totalCount
        nodes {
          id body path subjectType author { login }
          pullRequestReview {
            id state submittedAt author { login } commit { oid }
            pullRequest { id number repository { nameWithOwner } }
          }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"#;

const UPDATE_REVIEW_COMMENT_MUTATION: &str = r#"mutation UpdateReviewComment(
  $commentId: ID!, $body: String!, $clientMutationId: String!
) {
  updatePullRequestReviewComment(input: {
    pullRequestReviewCommentId: $commentId, body: $body, clientMutationId: $clientMutationId
  }) { pullRequestReviewComment { id body pullRequestReview { id } } }
}"#;

const SUBMIT_REVIEW_MUTATION: &str = r#"mutation SubmitReview(
  $reviewId: ID!, $event: PullRequestReviewEvent!, $body: String,
  $clientMutationId: String!
) {
  submitPullRequestReview(input: {
    pullRequestReviewId: $reviewId, event: $event, body: $body,
    clientMutationId: $clientMutationId
  }) { pullRequestReview { id state commit { oid } } }
}"#;

const UPDATE_REVIEW_MUTATION: &str = r#"mutation UpdatePendingReview(
  $reviewId: ID!, $body: String!, $clientMutationId: String!
) {
  updatePullRequestReview(input: {
    pullRequestReviewId: $reviewId, body: $body, clientMutationId: $clientMutationId
  }) { pullRequestReview { id } }
}"#;

const UPDATE_SUBMITTED_REVIEW_MUTATION: &str = r#"mutation UpdateSubmittedReviewSummary(
  $reviewId: ID!, $body: String!, $clientMutationId: String!
) {
  updateSubmittedPullRequestReview: updatePullRequestReview(input: {
    pullRequestReviewId: $reviewId, body: $body, clientMutationId: $clientMutationId
  }) {
    clientMutationId
    pullRequestReview {
      id body state author { login } commit { oid }
      pullRequest { id number repository { nameWithOwner } }
    }
  }
}"#;

const DELETE_REVIEW_COMMENT_MUTATION: &str = r#"mutation DeletePendingReviewComment(
  $commentId: ID!, $clientMutationId: String!
) {
  deletePullRequestReviewComment(input: { id: $commentId, clientMutationId: $clientMutationId }) {
    pullRequestReview { id }
    pullRequestReviewComment { id }
  }
}"#;

const DELETE_REVIEW_MUTATION: &str = r#"mutation CancelPendingReview(
  $reviewId: ID!, $clientMutationId: String!
) {
  deletePullRequestReview(input: {
    pullRequestReviewId: $reviewId, clientMutationId: $clientMutationId
  }) { pullRequestReview { id } }
}"#;

const ADD_THREAD_REPLY_MUTATION: &str = r#"mutation ReplyReviewThread(
  $threadId: ID!, $reviewId: ID, $body: String!, $clientMutationId: String!
) {
  addPullRequestReviewThreadReply(input: {
    pullRequestReviewThreadId: $threadId, pullRequestReviewId: $reviewId,
    body: $body, clientMutationId: $clientMutationId
  }) { comment { id pullRequestReview { id } } }
}"#;

const RESOLVE_THREAD_MUTATION: &str = r#"mutation ResolveReviewThread(
  $threadId: ID!, $clientMutationId: String!
) {
  resolveReviewThread(input: { threadId: $threadId, clientMutationId: $clientMutationId }) {
    thread { id isResolved }
  }
}"#;

const UNRESOLVE_THREAD_MUTATION: &str = r#"mutation UnresolveReviewThread(
  $threadId: ID!, $clientMutationId: String!
) {
  unresolveReviewThread(input: { threadId: $threadId, clientMutationId: $clientMutationId }) {
    thread { id isResolved }
  }
}"#;

#[derive(Deserialize)]
struct ReviewActionContextData {
    viewer: GraphqlActor,
    repository: Option<ReviewActionRepository>,
}

#[derive(Deserialize)]
struct PendingReviewData {
    viewer: Option<GraphqlActor>,
    repository: Option<PendingReviewRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingReviewRepository {
    name_with_owner: String,
    pull_request: Option<PendingReviewPull>,
}

#[derive(Deserialize)]
struct PendingReviewPull {
    id: Option<String>,
    number: u64,
    url: Option<String>,
    state: Option<String>,
    #[serde(rename = "baseRefOid")]
    base_ref_oid: Option<String>,
    #[serde(rename = "headRefOid")]
    head_ref_oid: Option<String>,
    reviews: GraphqlConnection<PendingReviewNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingReviewNode {
    id: String,
    author: Option<GraphqlActor>,
    body: String,
    state: String,
    submitted_at: Option<String>,
    commit: Option<GraphqlOid>,
    url: String,
    comments: GraphqlConnection<DetailsReviewComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewActionRepository {
    name_with_owner: String,
    pull_request: Option<ActionPullIdentity>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionPullIdentity {
    id: String,
    number: u64,
    url: String,
    head_ref_oid: String,
    state: String,
}

struct ReviewActionContext {
    viewer: GraphqlActor,
    repository: String,
    pull: ActionPullIdentity,
}

#[derive(Deserialize)]
struct PendingFilePreflightData {
    viewer: GraphqlActor,
    repository: Option<PendingFilePreflightRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingFilePreflightRepository {
    name_with_owner: String,
    pull_request: Option<PendingFilePreflightPull>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingFilePreflightPull {
    id: String,
    number: u64,
    url: String,
    state: String,
    base_ref_oid: String,
    head_ref_oid: String,
    reviews: GraphqlConnection<PendingFilePreflightReview>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingFilePreflightReview {
    id: String,
    state: String,
    submitted_at: Option<String>,
    author: Option<GraphqlActor>,
    commit: Option<GraphqlOid>,
}

struct PendingFilePreflight {
    viewer_login: String,
    repository: String,
    pull_id: String,
    pull_number: u64,
    pull_state: String,
    base_sha: String,
    head_sha: String,
    review: PendingFilePreflightReview,
}

#[derive(Deserialize)]
struct ActionReviewNodeData {
    node: Option<ActionReviewNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionReviewNode {
    id: String,
    body: Option<String>,
    state: String,
    submitted_at: Option<String>,
    author: Option<GraphqlActor>,
    commit: Option<GraphqlOid>,
    viewer_did_author: Option<bool>,
    viewer_can_update: Option<bool>,
    viewer_cannot_update_reasons: Option<Vec<String>>,
    pull_request: ActionReviewPull,
}

#[derive(Deserialize)]
struct ActionReviewPull {
    id: String,
    number: u64,
    repository: ActionRepositoryIdentity,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionRepositoryIdentity {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct ActionCommentNodeData {
    node: Option<ActionCommentNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionCommentNode {
    id: String,
    author: Option<GraphqlActor>,
    subject_type: Option<String>,
    pull_request_review: ActionReviewNode,
}

#[derive(Deserialize)]
struct ActionThreadNodeData {
    node: Option<ActionThreadNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionThreadNode {
    id: String,
    is_resolved: bool,
    viewer_can_reply: bool,
    viewer_can_resolve: bool,
    viewer_can_unresolve: bool,
    pull_request: ActionReviewPull,
}

struct PreparedReviewMutation {
    action: &'static str,
    query: &'static str,
    variables: Value,
    kind: ReviewMutationKind,
}

impl PreparedReviewMutation {
    fn new(
        action: &'static str,
        query: &'static str,
        variables: Value,
        kind: ReviewMutationKind,
    ) -> Self {
        Self {
            action,
            query,
            variables,
            kind,
        }
    }
}

enum ReviewMutationKind {
    AddReviewWithComment,
    AddThread {
        review_id: String,
    },
    AddFileThread {
        operation_id: String,
        review_id: String,
        selected_author: String,
        commit_sha: String,
        pull_request_id: String,
        pull_request_number: u64,
        repository: String,
        path: String,
        body: String,
    },
    UpdateComment {
        review_id: String,
        comment_id: String,
    },
    SubmitReview {
        review_id: String,
    },
    AddSubmittedReview,
}

struct ReviewAck {
    review_id: Option<String>,
    comment_id: Option<String>,
    thread_id: Option<String>,
}

impl ReviewMutationKind {
    fn acknowledgement(self, data: ReviewMutationData) -> std::result::Result<ReviewAck, String> {
        match self {
            Self::AddReviewWithComment => {
                let review = data
                    .add_pull_request_review
                    .and_then(|payload| payload.pull_request_review)
                    .ok_or_else(|| {
                        "GitHub omitted the created review acknowledgement".to_owned()
                    })?;
                let comment = review
                    .comments
                    .and_then(|comments| comments.nodes.into_iter().flatten().next())
                    .ok_or_else(|| {
                        "GitHub omitted the created review comment acknowledgement".to_owned()
                    })?;
                validate_acknowledgement_id(&review.id, "created review")?;
                validate_acknowledgement_id(&comment.id, "created review comment")?;
                validate_acknowledgement_id(
                    &comment.pull_request_review.id,
                    "created comment parent review",
                )?;
                if comment.pull_request_review.id != review.id {
                    return Err("GitHub acknowledged a comment linked to another review".into());
                }
                Ok(ReviewAck {
                    review_id: Some(review.id),
                    comment_id: Some(comment.id),
                    thread_id: None,
                })
            }
            Self::AddThread { review_id } => {
                let comment = data
                    .add_pull_request_review_thread
                    .and_then(|payload| payload.thread)
                    .and_then(|thread| thread.comments.nodes.into_iter().flatten().next())
                    .ok_or_else(|| {
                        "GitHub omitted the created review thread acknowledgement".to_owned()
                    })?;
                validate_acknowledgement_id(&comment.id, "created review comment")?;
                validate_acknowledgement_id(
                    &comment.pull_request_review.id,
                    "created comment parent review",
                )?;
                if comment.pull_request_review.id != review_id {
                    return Err(
                        "GitHub acknowledged a comment linked to another pending review".into(),
                    );
                }
                Ok(ReviewAck {
                    review_id: Some(comment.pull_request_review.id),
                    comment_id: Some(comment.id),
                    thread_id: None,
                })
            }
            Self::AddFileThread {
                operation_id,
                review_id,
                selected_author,
                commit_sha,
                pull_request_id,
                pull_request_number,
                repository,
                path,
                body,
            } => {
                let payload = data.add_pending_file_review_thread.ok_or_else(|| {
                    "GitHub omitted the created file-level review thread acknowledgement".to_owned()
                })?;
                if payload.client_mutation_id.as_deref() != Some(operation_id.as_str()) {
                    return Err("GitHub echoed another file-comment operation ID".into());
                }
                let thread = payload
                    .thread
                    .ok_or_else(|| "GitHub omitted the created file-level thread".to_owned())?;
                validate_acknowledgement_id(&thread.id, "created file-level thread")?;
                if thread.path != path
                    || ReviewSubject::from_provider(&thread.subject_type) != ReviewSubject::File
                    || thread.comments.total_count != 1
                    || thread.comments.page_info.has_next_page
                    || thread.comments.nodes.len() != 1
                {
                    return Err(
                        "GitHub file-level thread acknowledgement did not match the exact subject and path"
                            .into(),
                    );
                }
                let comment = thread
                    .comments
                    .nodes
                    .into_iter()
                    .next()
                    .flatten()
                    .ok_or_else(|| "GitHub omitted the created file-level comment".to_owned())?;
                validate_acknowledgement_id(&comment.id, "created file-level comment")?;
                validate_acknowledgement_id(
                    &comment.pull_request_review.id,
                    "created file-level comment parent review",
                )?;
                if thread.id == comment.id
                    || thread.id == review_id
                    || comment.id == review_id
                    || comment.path != path
                    || comment.body != body
                    || ReviewSubject::from_provider(&comment.subject_type) != ReviewSubject::File
                    || !comment
                        .author
                        .as_ref()
                        .is_some_and(|author| author.login.eq_ignore_ascii_case(&selected_author))
                    || comment.pull_request_review.id != review_id
                    || comment.pull_request_review.state != "PENDING"
                    || comment.pull_request_review.submitted_at.is_some()
                    || !comment
                        .pull_request_review
                        .author
                        .as_ref()
                        .is_some_and(|author| author.login.eq_ignore_ascii_case(&selected_author))
                    || comment
                        .pull_request_review
                        .commit
                        .as_ref()
                        .map(|commit| commit.oid.as_str())
                        != Some(commit_sha.as_str())
                    || comment.pull_request_review.pull_request.id != pull_request_id
                    || comment.pull_request_review.pull_request.number != pull_request_number
                    || !comment
                        .pull_request_review
                        .pull_request
                        .repository
                        .name_with_owner
                        .eq_ignore_ascii_case(&repository)
                {
                    return Err(
                        "GitHub file-level comment acknowledgement did not match the exact frozen parent, actor, path, body, and commit"
                            .into(),
                    );
                }
                Ok(ReviewAck {
                    review_id: Some(review_id),
                    comment_id: Some(comment.id),
                    thread_id: Some(thread.id),
                })
            }
            Self::UpdateComment {
                review_id,
                comment_id,
            } => {
                let comment = data
                    .update_pull_request_review_comment
                    .and_then(|payload| payload.pull_request_review_comment)
                    .ok_or_else(|| {
                        "GitHub omitted the updated review comment acknowledgement".to_owned()
                    })?;
                validate_acknowledgement_id(&comment.id, "updated review comment")?;
                validate_acknowledgement_id(
                    &comment.pull_request_review.id,
                    "updated comment parent review",
                )?;
                if comment.id != comment_id {
                    return Err("GitHub acknowledged a different updated review comment".into());
                }
                if comment.pull_request_review.id != review_id {
                    return Err(
                        "GitHub acknowledged an updated comment linked to another pending review"
                            .into(),
                    );
                }
                Ok(ReviewAck {
                    review_id: Some(comment.pull_request_review.id),
                    comment_id: Some(comment.id),
                    thread_id: None,
                })
            }
            Self::SubmitReview { review_id } => {
                let review = data
                    .submit_pull_request_review
                    .and_then(|payload| payload.pull_request_review)
                    .ok_or_else(|| {
                        "GitHub omitted the submitted review acknowledgement".to_owned()
                    })?;
                validate_acknowledgement_id(&review.id, "submitted review")?;
                if review.id != review_id {
                    return Err("GitHub acknowledged a different submitted review".into());
                }
                Ok(ReviewAck {
                    review_id: Some(review.id),
                    comment_id: None,
                    thread_id: None,
                })
            }
            Self::AddSubmittedReview => {
                let review = data
                    .add_pull_request_review
                    .and_then(|payload| payload.pull_request_review)
                    .ok_or_else(|| {
                        "GitHub omitted the submitted review acknowledgement".to_owned()
                    })?;
                validate_acknowledgement_id(&review.id, "submitted review")?;
                Ok(ReviewAck {
                    review_id: Some(review.id),
                    comment_id: None,
                    thread_id: None,
                })
            }
        }
    }
}

fn validate_acknowledgement_id(id: &str, kind: &str) -> std::result::Result<(), String> {
    validate_node_id(id)
        .map_err(|_| format!("GitHub returned an invalid {kind} acknowledgement ID"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewMutationData {
    add_pull_request_review: Option<AddReviewPayload>,
    add_pull_request_review_thread: Option<AddThreadPayload>,
    add_pending_file_review_thread: Option<AddFileThreadPayload>,
    update_pull_request_review_comment: Option<UpdateCommentPayload>,
    submit_pull_request_review: Option<SubmitReviewPayload>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddReviewPayload {
    pull_request_review: Option<MutationReview>,
}
#[derive(Deserialize)]
struct AddThreadPayload {
    thread: Option<MutationThread>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddFileThreadPayload {
    client_mutation_id: Option<String>,
    thread: Option<FileMutationThread>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileMutationThread {
    id: String,
    path: String,
    subject_type: String,
    comments: FileMutationCommentConnection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileMutationCommentConnection {
    total_count: u64,
    nodes: Vec<Option<FileMutationComment>>,
    page_info: PageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileMutationComment {
    id: String,
    body: String,
    path: String,
    subject_type: String,
    author: Option<GraphqlActor>,
    pull_request_review: FileMutationParentReview,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileMutationParentReview {
    id: String,
    state: String,
    submitted_at: Option<String>,
    author: Option<GraphqlActor>,
    commit: Option<GraphqlOid>,
    pull_request: ActionReviewPull,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateCommentPayload {
    pull_request_review_comment: Option<MutationComment>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmitReviewPayload {
    pull_request_review: Option<MutationReview>,
}
#[derive(Deserialize)]
struct MutationReview {
    id: String,
    comments: Option<MutationCommentConnection>,
}
#[derive(Deserialize)]
struct MutationThread {
    comments: MutationCommentConnection,
}
#[derive(Deserialize)]
struct MutationCommentConnection {
    nodes: Vec<Option<MutationComment>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MutationComment {
    id: String,
    pull_request_review: GraphqlNodeId,
}

struct PreparedAuxiliaryMutation {
    action: &'static str,
    query: &'static str,
    variables: Value,
    kind: AuxiliaryMutationKind,
}

impl PreparedAuxiliaryMutation {
    fn new(
        action: &'static str,
        query: &'static str,
        variables: Value,
        kind: AuxiliaryMutationKind,
    ) -> Self {
        Self {
            action,
            query,
            variables,
            kind,
        }
    }
}

enum AuxiliaryMutationKind {
    Review,
    SubmittedSummary {
        operation_id: String,
        review_id: String,
        body: String,
        state: String,
        author: String,
        commit_sha: String,
        pull_request_id: String,
        pull_request_number: u64,
        repository: String,
    },
    DeletedComment,
    DeletedReview,
    Reply,
    Thread,
}

impl AuxiliaryMutationKind {
    fn acknowledgement(
        self,
        data: AuxiliaryMutationData,
    ) -> std::result::Result<ReviewAuxiliaryAcknowledgement, String> {
        let mut ack = ReviewAuxiliaryAcknowledgement {
            operation_id: String::new(),
            review_id: None,
            comment_id: None,
            thread_id: None,
            resolved: None,
        };
        match self {
            Self::Review => {
                ack.review_id = Some(
                    data.update_pull_request_review
                        .and_then(|payload| payload.pull_request_review)
                        .ok_or_else(|| {
                            "GitHub omitted the updated pending review acknowledgement".to_owned()
                        })?
                        .id,
                );
            }
            Self::SubmittedSummary {
                operation_id,
                review_id,
                body,
                state,
                author,
                commit_sha,
                pull_request_id,
                pull_request_number,
                repository,
            } => {
                let payload = data.update_submitted_pull_request_review.ok_or_else(|| {
                    "GitHub omitted the submitted review update acknowledgement".to_owned()
                })?;
                if payload.client_mutation_id.as_deref() != Some(operation_id.as_str()) {
                    return Err("GitHub echoed another submitted review update operation ID".into());
                }
                let review = payload
                    .pull_request_review
                    .ok_or_else(|| "GitHub omitted the updated submitted review".to_owned())?;
                validate_acknowledgement_id(&review.id, "updated submitted review")?;
                if review.id != review_id
                    || review.body != body
                    || review.state != state
                    || !review
                        .author
                        .as_ref()
                        .is_some_and(|candidate| candidate.login.eq_ignore_ascii_case(&author))
                    || review.commit.as_ref().map(|commit| commit.oid.as_str())
                        != Some(commit_sha.as_str())
                    || review.pull_request.id != pull_request_id
                    || review.pull_request.number != pull_request_number
                    || !review
                        .pull_request
                        .repository
                        .name_with_owner
                        .eq_ignore_ascii_case(&repository)
                {
                    return Err(
                        "GitHub submitted review update acknowledgement did not match the exact frozen final state"
                            .into(),
                    );
                }
                ack.review_id = Some(review.id);
            }
            Self::DeletedComment => {
                let payload = data.delete_pull_request_review_comment.ok_or_else(|| {
                    "GitHub omitted the deleted comment acknowledgement".to_owned()
                })?;
                ack.review_id = payload.pull_request_review.map(|node| node.id);
                ack.comment_id = payload.pull_request_review_comment.map(|node| node.id);
                if ack.comment_id.is_none() {
                    return Err("GitHub omitted the deleted comment ID".into());
                }
            }
            Self::DeletedReview => {
                ack.review_id = Some(
                    data.delete_pull_request_review
                        .and_then(|payload| payload.pull_request_review)
                        .ok_or_else(|| {
                            "GitHub omitted the cancelled pending review acknowledgement".to_owned()
                        })?
                        .id,
                );
            }
            Self::Reply => {
                let comment = data
                    .add_pull_request_review_thread_reply
                    .and_then(|payload| payload.comment)
                    .ok_or_else(|| "GitHub omitted the review reply acknowledgement".to_owned())?;
                ack.review_id = Some(comment.pull_request_review.id);
                ack.comment_id = Some(comment.id);
            }
            Self::Thread => {
                let thread = data
                    .resolve_review_thread
                    .or(data.unresolve_review_thread)
                    .and_then(|payload| payload.thread)
                    .ok_or_else(|| {
                        "GitHub omitted the thread resolution acknowledgement".to_owned()
                    })?;
                ack.thread_id = Some(thread.id);
                ack.resolved = Some(thread.is_resolved);
            }
        }
        Ok(ack)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuxiliaryMutationData {
    update_pull_request_review: Option<AuxReviewPayload>,
    update_submitted_pull_request_review: Option<AuxSubmittedReviewPayload>,
    delete_pull_request_review_comment: Option<AuxDeleteCommentPayload>,
    delete_pull_request_review: Option<AuxReviewPayload>,
    add_pull_request_review_thread_reply: Option<AuxReplyPayload>,
    resolve_review_thread: Option<AuxThreadPayload>,
    unresolve_review_thread: Option<AuxThreadPayload>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuxReviewPayload {
    pull_request_review: Option<GraphqlNodeId>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuxSubmittedReviewPayload {
    client_mutation_id: Option<String>,
    pull_request_review: Option<AuxSubmittedReview>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuxSubmittedReview {
    id: String,
    body: String,
    state: String,
    author: Option<GraphqlActor>,
    commit: Option<GraphqlOid>,
    pull_request: ActionReviewPull,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuxDeleteCommentPayload {
    pull_request_review: Option<GraphqlNodeId>,
    pull_request_review_comment: Option<GraphqlNodeId>,
}
#[derive(Deserialize)]
struct AuxReplyPayload {
    comment: Option<MutationComment>,
}
#[derive(Deserialize)]
struct AuxThreadPayload {
    thread: Option<AuxThread>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuxThread {
    id: String,
    is_resolved: bool,
}

fn prepare_pending_file_comment_mutation(
    session: &mut Session<'_>,
    repo: &Repository,
    intent: &PendingFileCommentIntent,
) -> std::result::Result<PreparedReviewMutation, String> {
    validate_action_text(&intent.body, false)?;
    let ReviewCommentTarget::File(file) = &intent.target else {
        return Err("pending file comment contains a line target".into());
    };
    validate_sha(&file.base_sha).map_err(|error| error.to_string())?;
    validate_sha(&file.commit_sha).map_err(|error| error.to_string())?;
    validate_sha(&intent.pending.observed_base_sha).map_err(|error| error.to_string())?;
    validate_sha(&intent.pending.observed_head_sha).map_err(|error| error.to_string())?;
    validate_sha(&intent.pending.review_commit_sha).map_err(|error| error.to_string())?;
    if file.path.is_empty()
        || file.path.len() > 4096
        || file.path.contains('\0')
        || file.file_key != file.path
    {
        return Err("pending file target has no exact provider-safe UTF-8 path".into());
    }
    validate_coordinates(repo, intent.key.pull_request, &intent.pending.pull_request)?;
    validate_coordinates(
        repo,
        intent.key.pull_request,
        &intent.pending.pending_review,
    )?;
    if !intent
        .pending
        .selected_author
        .eq_ignore_ascii_case(&session.provider.account.login)
        || intent.pending.observed_base_sha != file.base_sha
        || intent.pending.observed_head_sha != file.commit_sha
        || intent.pending.review_commit_sha != file.commit_sha
    {
        return Err(
            "frozen pending review, selected account, and canonical file target differ".into(),
        );
    }
    let fresh = session
        .pending_file_preflight(repo, intent.key.pull_request)
        .map_err(|error| error.to_string())?;
    if fresh.pull_state != "OPEN"
        || fresh.pull_number != intent.key.pull_request
        || fresh.pull_id != intent.pending.pull_request.remote_id
        || !fresh.repository.eq_ignore_ascii_case(&repo.full_name())
        || !fresh
            .viewer_login
            .eq_ignore_ascii_case(&intent.pending.selected_author)
        || fresh.base_sha != intent.pending.observed_base_sha
        || fresh.head_sha != intent.pending.observed_head_sha
    {
        return Err(
            "pull request identity, canonical revision, or selected viewer changed before dispatch"
                .into(),
        );
    }
    let review = fresh.review;
    if review.id != intent.pending.pending_review.remote_id
        || review.state != "PENDING"
        || review.submitted_at.is_some()
        || !review.author.as_ref().is_some_and(|author| {
            author
                .login
                .eq_ignore_ascii_case(&intent.pending.selected_author)
        })
        || review.commit.as_ref().map(|commit| commit.oid.as_str())
            != Some(intent.pending.review_commit_sha.as_str())
    {
        return Err(
            "selected account pending review changed or targets another commit before dispatch"
                .into(),
        );
    }
    Ok(PreparedReviewMutation::new(
        "add-pending-file-comment",
        ADD_PENDING_FILE_THREAD_MUTATION,
        json!({
            "pullRequestReviewId": intent.pending.pending_review.remote_id,
            "body": intent.body,
            "path": file.path,
            "subjectType": "FILE",
            "clientMutationId": intent.operation_id,
        }),
        ReviewMutationKind::AddFileThread {
            operation_id: intent.operation_id.clone(),
            review_id: intent.pending.pending_review.remote_id.clone(),
            selected_author: intent.pending.selected_author.clone(),
            commit_sha: intent.pending.review_commit_sha.clone(),
            pull_request_id: intent.pending.pull_request.remote_id.clone(),
            pull_request_number: intent.key.pull_request,
            repository: repo.full_name(),
            path: file.path.clone(),
            body: intent.body.clone(),
        },
    ))
}

fn prepare_pending_comment_mutation(
    session: &mut Session<'_>,
    context: &ReviewActionContext,
    intent: &PendingCommentIntent,
) -> std::result::Result<PreparedReviewMutation, String> {
    if intent.body.is_empty() || intent.body.len() > MAX_ACTION_TEXT_BYTES {
        return Err("pending comment body is empty or exceeds its bound".into());
    }
    if let Some(review_id) = &intent.pending_review_id {
        let review = session
            .review_node(review_id)
            .map_err(|error| error.to_string())?;
        validate_pending_review(
            &review,
            context,
            session.provider,
            &intent.position.commit_sha,
        )?;
        if let Some(comment_id) = &intent.existing_comment_id {
            let comment = session
                .comment_node(comment_id)
                .map_err(|error| error.to_string())?;
            validate_owned_comment(&comment, &review, session.provider)?;
            return Ok(PreparedReviewMutation::new(
                "edit-pending-comment",
                UPDATE_REVIEW_COMMENT_MUTATION,
                json!({"commentId": comment_id, "body": intent.body, "clientMutationId": intent.operation_id}),
                ReviewMutationKind::UpdateComment {
                    review_id: review_id.clone(),
                    comment_id: comment_id.clone(),
                },
            ));
        }
        let mut variables = thread_input(&intent.body, &intent.position);
        let object = variables.as_object_mut().expect("thread object");
        object.insert("pullRequestReviewId".into(), json!(review_id));
        object.insert("clientMutationId".into(), json!(intent.operation_id));
        return Ok(PreparedReviewMutation::new(
            "add-pending-comment",
            ADD_REVIEW_THREAD_MUTATION,
            variables,
            ReviewMutationKind::AddThread {
                review_id: review_id.clone(),
            },
        ));
    }
    if intent.existing_comment_id.is_some() {
        return Err("an existing comment cannot be edited without its pending review ID".into());
    }
    Ok(PreparedReviewMutation::new(
        "create-pending-review-comment",
        ADD_REVIEW_MUTATION,
        json!({
            "pullRequestId": context.pull.id,
            "commitOID": intent.position.commit_sha,
            "event": Value::Null,
            "body": Value::Null,
            "threads": [thread_input(&intent.body, &intent.position)],
            "clientMutationId": intent.operation_id,
        }),
        ReviewMutationKind::AddReviewWithComment,
    ))
}

fn prepare_submission_mutation(
    session: &mut Session<'_>,
    context: &ReviewActionContext,
    intent: &SubmissionIntent,
) -> std::result::Result<PreparedReviewMutation, String> {
    if intent.body.len() > MAX_ACTION_TEXT_BYTES {
        return Err("review summary exceeds its bound".into());
    }
    let event = review_event_name(&intent.event);
    if let Some(review_id) = &intent.pending_review_id {
        let review = session
            .review_node(review_id)
            .map_err(|error| error.to_string())?;
        validate_pending_review(
            &review,
            context,
            session.provider,
            &intent.reviewed_commit_sha,
        )?;
        return Ok(PreparedReviewMutation::new(
            "submit-pending-review",
            SUBMIT_REVIEW_MUTATION,
            json!({"reviewId": review_id, "event": event, "body": intent.body, "clientMutationId": intent.operation_id}),
            ReviewMutationKind::SubmitReview {
                review_id: review_id.clone(),
            },
        ));
    }
    Ok(PreparedReviewMutation::new(
        "submit-new-review",
        ADD_REVIEW_MUTATION,
        json!({
            "pullRequestId": context.pull.id,
            "commitOID": intent.reviewed_commit_sha,
            "event": event,
            "body": intent.body,
            "threads": Value::Null,
            "clientMutationId": intent.operation_id,
        }),
        ReviewMutationKind::AddSubmittedReview,
    ))
}

fn thread_input(body: &str, position: &crate::participation::PublishedPosition) -> Value {
    let mut input = Map::new();
    input.insert("body".into(), json!(body));
    input.insert("path".into(), json!(position.path));
    input.insert("line".into(), json!(position.line));
    input.insert("side".into(), json!(position.side.provider_name()));
    if let Some(start_line) = position.start_line {
        input.insert("startLine".into(), json!(start_line));
    }
    if let Some(start_side) = position.start_side {
        input.insert("startSide".into(), json!(start_side.provider_name()));
    }
    Value::Object(input)
}

fn review_event_name(event: &ReviewEvent) -> &'static str {
    match event {
        ReviewEvent::Comment => "COMMENT",
        ReviewEvent::Approve => "APPROVE",
        ReviewEvent::RequestChanges => "REQUEST_CHANGES",
    }
}

fn validate_pending_review(
    review: &ActionReviewNode,
    context: &ReviewActionContext,
    provider: &GithubProvider,
    expected_commit: &str,
) -> std::result::Result<(), String> {
    if review.id.is_empty()
        || review.state != "PENDING"
        || review.pull_request.id != context.pull.id
        || review.pull_request.number != context.pull.number
        || !review
            .pull_request
            .repository
            .name_with_owner
            .eq_ignore_ascii_case(&context.repository)
    {
        return Err("pending review ID belongs to another PR or is no longer pending".into());
    }
    if !review
        .author
        .as_ref()
        .is_some_and(|author| author.login.eq_ignore_ascii_case(&provider.account.login))
    {
        return Err("pending review is not authored by the selected account".into());
    }
    if review.commit.as_ref().map(|commit| commit.oid.as_str()) != Some(expected_commit) {
        return Err("pending review targets another commit".into());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_submitted_review(
    review: &ActionReviewNode,
    context: &ReviewActionContext,
    provider: &GithubProvider,
    expected_review_id: &str,
    selected_author: &str,
    expected_state: &str,
    expected_commit: &str,
    expected_body: &str,
) -> std::result::Result<(), String> {
    if !selected_author.eq_ignore_ascii_case(&provider.account.login) {
        return Err("submitted review selected author changed before dispatch".into());
    }
    if review.id != expected_review_id
        || review.pull_request.id != context.pull.id
        || review.pull_request.number != context.pull.number
        || !review
            .pull_request
            .repository
            .name_with_owner
            .eq_ignore_ascii_case(&context.repository)
    {
        return Err("submitted review belongs to another repository or pull request".into());
    }
    if !matches!(
        expected_state,
        "APPROVED" | "CHANGES_REQUESTED" | "COMMENTED"
    ) || review.state != expected_state
        || review.submitted_at.as_deref().is_none_or(str::is_empty)
    {
        return Err("review is no longer in the frozen submitted state".into());
    }
    if review.body.as_deref() != Some(expected_body) {
        return Err("submitted review body changed before dispatch".into());
    }
    if review.commit.as_ref().map(|commit| commit.oid.as_str()) != Some(expected_commit) {
        return Err("submitted review commit changed or is unavailable".into());
    }
    if !review
        .author
        .as_ref()
        .is_some_and(|author| author.login.eq_ignore_ascii_case(selected_author))
    {
        return Err("submitted review is not authored by the selected account".into());
    }
    let Some(viewer_did_author) = review.viewer_did_author else {
        return Err("GitHub omitted submitted-review author capability evidence".into());
    };
    let Some(viewer_can_update) = review.viewer_can_update else {
        return Err("GitHub omitted submitted-review update capability evidence".into());
    };
    let Some(reasons) = review.viewer_cannot_update_reasons.as_ref() else {
        return Err("GitHub omitted submitted-review capability reasons".into());
    };
    if !viewer_did_author {
        return Err("GitHub says the selected viewer did not author this review".into());
    }
    if !viewer_can_update {
        let reason = reasons
            .iter()
            .take(3)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(if reason.is_empty() {
            "GitHub says the selected author cannot update this submitted review".into()
        } else {
            format!("GitHub says the selected author cannot update this submitted review: {reason}")
        });
    }
    Ok(())
}

fn validate_owned_comment(
    comment: &ActionCommentNode,
    review: &ActionReviewNode,
    provider: &GithubProvider,
) -> std::result::Result<(), String> {
    if comment.id.is_empty()
        || comment.pull_request_review.id != review.id
        || !comment
            .author
            .as_ref()
            .is_some_and(|author| author.login.eq_ignore_ascii_case(&provider.account.login))
    {
        return Err("review comment is foreign or linked to another review".into());
    }
    if ReviewSubject::from_provider(comment.subject_type.as_deref().unwrap_or_default())
        == ReviewSubject::Unknown
    {
        return Err(
            "review comment subject is missing or unknown; the read-only target cannot be mutated"
                .into(),
        );
    }
    Ok(())
}

fn validate_thread(
    thread: &ActionThreadNode,
    context: &ReviewActionContext,
) -> std::result::Result<(), String> {
    if thread.id.is_empty()
        || thread.pull_request.id != context.pull.id
        || thread.pull_request.number != context.pull.number
        || !thread
            .pull_request
            .repository
            .name_with_owner
            .eq_ignore_ascii_case(&context.repository)
    {
        return Err("review thread belongs to another repository or PR".into());
    }
    Ok(())
}

fn validate_coordinates(
    repo: &Repository,
    number: u64,
    coordinates: &ProviderCoordinates,
) -> std::result::Result<(), String> {
    if !coordinates_match(repo, number, coordinates) {
        return Err("provider object ID belongs to another repository or PR".into());
    }
    validate_node_id(&coordinates.remote_id).map_err(|error| error.to_string())
}

fn validate_action_identity(value: &str, field: &str) -> std::result::Result<(), String> {
    if value.is_empty() || value.len() > 1024 || value.contains('\0') {
        return Err(format!("invalid {field}"));
    }
    Ok(())
}

fn validate_action_text(body: &str, empty_allowed: bool) -> std::result::Result<(), String> {
    if (!empty_allowed && body.is_empty()) || body.len() > MAX_ACTION_TEXT_BYTES {
        return Err("action text is empty or exceeds its bound".into());
    }
    Ok(())
}

const DETAILS_QUERY: &str = r#"query PullRequestDetails(
    $owner: String!, $name: String!, $number: Int!,
    $commentsCursor: String, $reviewsCursor: String, $threadsCursor: String,
    $checksCursor: String, $includeComments: Boolean!, $includeReviews: Boolean!,
    $includeThreads: Boolean!, $includeChecks: Boolean!
) {
  viewer { id login }
  repository(owner: $owner, name: $name) {
    id nameWithOwner viewerCanAdminister
    pullRequest(number: $number) {
      id number url headRefOid body state isDraft maintainerCanModify canBeRebased viewerCanReact
      repository { id nameWithOwner }
      headRepository { id nameWithOwner }
      potentialMergeCommit { oid repository { id nameWithOwner } }
      reactionGroups { content viewerHasReacted users { totalCount } }
      viewerCanUpdateBranch mergeable mergeStateStatus reviewDecision
      autoMergeRequest { enabledAt }
      isInMergeQueue
      reviewRequests(first: 100) {
        nodes { requestedReviewer { ... on User { login } ... on Team { slug } } }
        pageInfo { hasNextPage endCursor }
      }
      assignees(first: 100) { nodes { login } pageInfo { hasNextPage endCursor } }
      labels(first: 100) { nodes { name } pageInfo { hasNextPage endCursor } }
      comments(first: 50, after: $commentsCursor) @include(if: $includeComments) {
        nodes {
          id author { login } body createdAt updatedAt url viewerCanReact
          reactionGroups { content viewerHasReacted users { totalCount } }
        }
        pageInfo { hasNextPage endCursor }
      }
      reviews(first: 50, after: $reviewsCursor) @include(if: $includeReviews) {
        nodes {
          id author { login } body state submittedAt commit { oid } url
          viewerDidAuthor viewerCanUpdate viewerCannotUpdateReasons
          viewerCanReact reactionGroups { content viewerHasReacted users { totalCount } }
        }
        pageInfo { hasNextPage endCursor }
      }
      reviewThreads(first: 50, after: $threadsCursor) @include(if: $includeThreads) {
        nodes {
          id path subjectType line originalLine startLine originalStartLine diffSide startDiffSide
          isResolved isOutdated
          comments(first: 100) {
            nodes {
              id author { login } body createdAt updatedAt url path subjectType line originalLine
              startLine originalStartLine diffHunk outdated commit { oid } originalCommit { oid }
              pullRequestReview { id }
              viewerCanReact reactionGroups { content viewerHasReacted users { totalCount } }
            }
            pageInfo { hasNextPage endCursor }
          }
        }
        pageInfo { hasNextPage endCursor }
      }
      statusCheckRollup {
        state
        commit { oid repository { id nameWithOwner } }
        contexts(first: 50, after: $checksCursor) @include(if: $includeChecks) {
          nodes {
            __typename
            ... on CheckRun {
              id databaseId name status conclusion permalink detailsUrl startedAt completedAt
              isRequired(pullRequestNumber: $number)
              repository { id nameWithOwner }
              checkSuite {
                id databaseId
                repository { id nameWithOwner }
                commit { oid repository { id nameWithOwner } }
                app { id name slug }
                workflowRun {
                  id databaseId runAttempt runNumber event url
                  workflow { id databaseId name }
                }
              }
            }
            ... on StatusContext {
              id context state description targetUrl createdAt updatedAt
              isRequired(pullRequestNumber: $number)
              commit { oid repository { id nameWithOwner } }
            }
          }
          pageInfo { hasNextPage endCursor }
        }
      }
    }
  }
}"#;

#[derive(Clone, Default)]
struct ConnectionCursor {
    after: Option<String>,
    include: bool,
}

#[derive(Clone, Default)]
struct DetailsCursors {
    comments: ConnectionCursor,
    reviews: ConnectionCursor,
    threads: ConnectionCursor,
    checks: ConnectionCursor,
}

impl DetailsCursors {
    fn initial() -> Self {
        Self {
            comments: ConnectionCursor {
                include: true,
                ..Default::default()
            },
            reviews: ConnectionCursor {
                include: true,
                ..Default::default()
            },
            threads: ConnectionCursor {
                include: true,
                ..Default::default()
            },
            checks: ConnectionCursor {
                include: true,
                ..Default::default()
            },
        }
    }

    fn done(&self) -> bool {
        !self.comments.include
            && !self.reviews.include
            && !self.threads.include
            && !self.checks.include
    }
}

fn details_variables(repo: &Repository, number: u64, cursors: &DetailsCursors) -> Value {
    json!({
        "owner": repo.owner,
        "name": repo.name,
        "number": number,
        "commentsCursor": cursors.comments.after,
        "reviewsCursor": cursors.reviews.after,
        "threadsCursor": cursors.threads.after,
        "checksCursor": cursors.checks.after,
        "includeComments": cursors.comments.include,
        "includeReviews": cursors.reviews.include,
        "includeThreads": cursors.threads.include,
        "includeChecks": cursors.checks.include,
    })
}

#[derive(Deserialize)]
struct DetailsData {
    viewer: DetailsViewer,
    repository: Option<DetailsRepository>,
}

#[derive(Clone, Deserialize)]
struct DetailsViewer {
    id: String,
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsRepository {
    id: String,
    name_with_owner: String,
    viewer_can_administer: bool,
    pull_request: Option<DetailsPull>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsPull {
    id: String,
    number: u64,
    url: String,
    head_ref_oid: String,
    repository: DetailsRepositoryIdentity,
    #[serde(default)]
    head_repository: ObservedNullable<DetailsRepositoryIdentity>,
    #[serde(default)]
    potential_merge_commit: ObservedNullable<DetailsCommitIdentity>,
    body: String,
    state: String,
    is_draft: bool,
    maintainer_can_modify: bool,
    can_be_rebased: bool,
    viewer_can_update_branch: bool,
    mergeable: String,
    merge_state_status: String,
    review_decision: Option<String>,
    auto_merge_request: Option<Value>,
    is_in_merge_queue: bool,
    viewer_can_react: bool,
    reaction_groups: Vec<DetailsReactionGroup>,
    review_requests: GraphqlConnection<DetailsReviewRequest>,
    assignees: GraphqlConnection<GraphqlActor>,
    labels: GraphqlConnection<DetailsLabel>,
    comments: Option<GraphqlConnection<DetailsIssueComment>>,
    reviews: Option<GraphqlConnection<DetailsReview>>,
    review_threads: Option<GraphqlConnection<DetailsThread>>,
    #[serde(default)]
    status_check_rollup: ObservedNullable<DetailsRollup>,
}

impl DetailsPull {
    fn validate(&self, repo: &Repository, number: u64, repository_id: &str) -> Result<()> {
        ensure!(
            self.number == number
                && self.url == format!("https://{}/{}/pull/{number}", repo.host, repo.full_name()),
            "GitHub details PR identity mismatch"
        );
        self.repository.validate(repo, Some(repository_id))?;
        if let ObservedNullable::Value(head_repository) = &self.head_repository {
            head_repository.validate_components()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum ObservedNullable<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<T> ObservedNullable<T> {
    fn as_ref(&self) -> ObservedNullable<&T> {
        match self {
            Self::Missing => ObservedNullable::Missing,
            Self::Null => ObservedNullable::Null,
            Self::Value(value) => ObservedNullable::Value(value),
        }
    }

    fn value(&self) -> Option<&T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Missing | Self::Null => None,
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for ObservedNullable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DetailsRepositoryIdentity {
    id: String,
    name_with_owner: String,
}

impl DetailsRepositoryIdentity {
    fn validate_components(&self) -> Result<()> {
        validate_node_id(&self.id)?;
        let (owner, name) = self
            .name_with_owner
            .split_once('/')
            .context("GitHub nested repository omitted owner/name coordinates")?;
        validate_component(owner, false)?;
        validate_component(name, true)
    }

    fn validate(&self, repo: &Repository, expected_node_id: Option<&str>) -> Result<()> {
        self.validate_components()?;
        ensure!(
            self.name_with_owner.eq_ignore_ascii_case(&repo.full_name())
                && expected_node_id.is_none_or(|id| id == self.id),
            "GitHub nested repository identity mismatch"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct DetailsCommitIdentity {
    oid: String,
    repository: DetailsRepositoryIdentity,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsReviewRequest {
    requested_reviewer: Option<DetailsRequestedReviewer>,
}

#[derive(Deserialize)]
struct DetailsRequestedReviewer {
    login: Option<String>,
    slug: Option<String>,
}

#[derive(Deserialize)]
struct DetailsLabel {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsIssueComment {
    id: String,
    author: Option<GraphqlActor>,
    body: String,
    created_at: String,
    updated_at: String,
    url: String,
    viewer_can_react: bool,
    reaction_groups: Vec<DetailsReactionGroup>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsReview {
    id: String,
    author: Option<GraphqlActor>,
    body: String,
    state: String,
    submitted_at: Option<String>,
    commit: Option<GraphqlOid>,
    viewer_did_author: bool,
    viewer_can_update: bool,
    viewer_cannot_update_reasons: Vec<String>,
    viewer_can_react: bool,
    reaction_groups: Vec<DetailsReactionGroup>,
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsReactionGroup {
    content: String,
    viewer_has_reacted: bool,
    users: DetailsReactionUsers,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsReactionUsers {
    total_count: u64,
}

#[derive(Deserialize)]
struct GraphqlOid {
    oid: String,
}

#[derive(Deserialize)]
struct GraphqlNodeId {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsThread {
    id: String,
    path: String,
    subject_type: Option<String>,
    line: Option<u64>,
    original_line: Option<u64>,
    start_line: Option<u64>,
    original_start_line: Option<u64>,
    diff_side: Option<String>,
    start_diff_side: Option<String>,
    is_resolved: bool,
    is_outdated: bool,
    comments: GraphqlConnection<DetailsReviewComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsReviewComment {
    id: String,
    author: Option<GraphqlActor>,
    body: String,
    created_at: String,
    updated_at: String,
    url: String,
    path: String,
    subject_type: Option<String>,
    line: Option<u64>,
    original_line: Option<u64>,
    start_line: Option<u64>,
    original_start_line: Option<u64>,
    diff_hunk: String,
    outdated: bool,
    commit: Option<GraphqlOid>,
    original_commit: Option<GraphqlOid>,
    pull_request_review: Option<GraphqlNodeId>,
    // PendingReview shares this comment shape but does not request reactions.
    // Missing fields cannot establish a complete reaction snapshot.
    viewer_can_react: Option<bool>,
    reaction_groups: Option<Vec<DetailsReactionGroup>>,
}

#[derive(Deserialize)]
struct DetailsRollup {
    state: Option<String>,
    #[serde(default)]
    commit: ObservedNullable<DetailsCommitIdentity>,
    #[serde(default)]
    contexts: ObservedNullable<GraphqlConnection<DetailsCheckNode>>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum DetailsCheckNode {
    CheckRun {
        id: Option<String>,
        #[serde(rename = "databaseId", default)]
        database_id: ObservedNullable<i64>,
        name: Option<String>,
        status: Option<String>,
        conclusion: Option<String>,
        permalink: Option<String>,
        #[serde(rename = "detailsUrl")]
        details_url: Option<String>,
        #[serde(rename = "startedAt")]
        started_at: Option<String>,
        #[serde(rename = "completedAt")]
        completed_at: Option<String>,
        #[serde(rename = "isRequired", default)]
        required: Option<bool>,
        repository: Option<DetailsRepositoryIdentity>,
        #[serde(rename = "checkSuite")]
        check_suite: Option<Box<DetailsCheckSuite>>,
    },
    StatusContext {
        id: Option<String>,
        context: Option<String>,
        state: Option<String>,
        description: Option<String>,
        #[serde(rename = "targetUrl")]
        target_url: Option<String>,
        #[serde(rename = "createdAt")]
        created_at: Option<String>,
        #[serde(rename = "updatedAt")]
        updated_at: Option<String>,
        #[serde(rename = "isRequired", default)]
        required: Option<bool>,
        #[serde(default)]
        commit: ObservedNullable<DetailsCommitIdentity>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsCheckSuite {
    id: Option<String>,
    #[serde(default)]
    database_id: ObservedNullable<i64>,
    repository: Option<DetailsRepositoryIdentity>,
    commit: Option<DetailsCommitIdentity>,
    #[serde(default)]
    app: ObservedNullable<DetailsCheckApp>,
    #[serde(default)]
    workflow_run: ObservedNullable<DetailsWorkflowRun>,
}

#[derive(Deserialize)]
struct DetailsCheckApp {
    id: Option<String>,
    name: Option<String>,
    slug: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsWorkflowRun {
    id: Option<String>,
    #[serde(default)]
    database_id: ObservedNullable<i64>,
    run_attempt: Option<i64>,
    run_number: Option<i64>,
    event: Option<String>,
    url: Option<String>,
    workflow: Option<DetailsWorkflow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsWorkflow {
    id: Option<String>,
    #[serde(default)]
    database_id: ObservedNullable<i64>,
    name: Option<String>,
}

struct DetailsOverview {
    body: String,
    requested_reviewers: Vec<String>,
    labels: Vec<String>,
    assignees: Vec<String>,
    merge_eligibility: MergeEligibility,
}

#[allow(clippy::too_many_arguments)]
fn reaction_subject_snapshot(
    repo: &Repository,
    pull_request: &ProviderCoordinates,
    kind: ReactableKind,
    subject: ProviderCoordinates,
    parent_review: Option<ProviderCoordinates>,
    content: String,
    provider_groups: &[DetailsReactionGroup],
    viewer_can_react: bool,
    viewer: &SelectedViewer,
    provider_complete: bool,
) -> ReactionSubjectSnapshot {
    let mut groups = Vec::new();
    let mut complete = provider_complete;
    let mut seen = HashSet::new();
    for group in provider_groups {
        let Some(content) = ReactionContent::from_graphql(&group.content) else {
            complete = false;
            continue;
        };
        if !seen.insert(content) {
            complete = false;
            continue;
        }
        groups.push(ReactionGroupSnapshot {
            content,
            count: group.users.total_count,
            viewer_has_reacted: group.viewer_has_reacted,
        });
    }
    if complete {
        // GitHub omits zero-count entries from a complete reactionGroups
        // result. Materialize those documented empty groups for presentation;
        // the click still performs an exact targeted absence preflight.
        for content in ReactionContent::ALL {
            if seen.insert(content) {
                groups.push(ReactionGroupSnapshot {
                    content,
                    count: 0,
                    viewer_has_reacted: false,
                });
            }
        }
    }
    groups.sort_by_key(|group| {
        ReactionContent::ALL
            .iter()
            .position(|content| content == &group.content)
            .unwrap_or(usize::MAX)
    });
    let exact_parent = match kind {
        ReactableKind::PullRequestReviewComment => parent_review.is_some(),
        _ => parent_review.is_none(),
    };
    let fresh_capability =
        (complete && exact_parent && coordinates_match(repo, pull_request.pull_request, &subject))
            .then(|| FreshReactionCapability {
                viewer: viewer.clone(),
                viewer_can_react,
            });
    ReactionSubjectSnapshot {
        kind,
        pull_request: pull_request.clone(),
        subject,
        parent_review,
        content,
        reactions: ReactionSnapshot { groups, complete },
        fresh_capability,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DetailsSourceIdentity {
    base_repository: DetailsRepositoryIdentity,
    pull_request_node_id: String,
    head_repository: ObservedNullable<DetailsRepositoryIdentity>,
    head_sha: String,
    rollup_commit: ObservedNullable<ObservedCommitIdentity>,
    potential_merge_commit: ObservedNullable<ObservedCommitIdentity>,
    complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ObservedCommitIdentity {
    sha: String,
    repository: DetailsRepositoryIdentity,
}

impl DetailsSourceIdentity {
    fn from_pull(repo: &Repository, pull: &DetailsPull) -> Result<Self> {
        pull.repository.validate(repo, None)?;
        let base_repository = pull.repository.clone();
        let rollup_commit = match &pull.status_check_rollup {
            ObservedNullable::Missing => ObservedNullable::Missing,
            ObservedNullable::Null => ObservedNullable::Null,
            ObservedNullable::Value(rollup) => validate_commit_observation(rollup.commit.as_ref())?,
        };
        let potential_merge_commit =
            validate_commit_observation(pull.potential_merge_commit.as_ref())?;
        if let ObservedNullable::Value(commit) = &rollup_commit {
            ensure!(
                same_repository(&commit.repository, &base_repository)
                    || matches!(
                        &pull.head_repository,
                        ObservedNullable::Value(head)
                            if same_repository(&commit.repository, head)
                    ),
                "GitHub PR rollup repository is neither the base nor observed head repository"
            );
        }
        if let ObservedNullable::Value(commit) = &potential_merge_commit {
            ensure!(
                same_repository(&commit.repository, &base_repository),
                "GitHub potential merge commit belongs to another repository"
            );
        }
        let rollup_state_complete = matches!(
            &pull.status_check_rollup,
            ObservedNullable::Null | ObservedNullable::Value(DetailsRollup { state: Some(_), .. })
        );
        let rollup_commit_complete = match &pull.status_check_rollup {
            ObservedNullable::Null => true,
            ObservedNullable::Value(_) => {
                matches!(&rollup_commit, ObservedNullable::Value(_))
            }
            ObservedNullable::Missing => false,
        };
        let complete = !matches!(&pull.head_repository, ObservedNullable::Missing)
            && rollup_state_complete
            && rollup_commit_complete
            && !matches!(&potential_merge_commit, ObservedNullable::Missing);
        Ok(Self {
            base_repository,
            pull_request_node_id: pull.id.clone(),
            head_repository: pull.head_repository.clone(),
            head_sha: pull.head_ref_oid.clone(),
            rollup_commit,
            potential_merge_commit,
            complete,
        })
    }

    fn rollup_commit_sha(&self) -> Option<&str> {
        self.rollup_commit.value().map(|commit| commit.sha.as_str())
    }

    fn potential_merge_commit_sha(&self) -> Option<&str> {
        self.potential_merge_commit
            .value()
            .map(|commit| commit.sha.as_str())
    }

    fn allows_check_repository(&self, repository: &DetailsRepositoryIdentity) -> bool {
        same_repository(repository, &self.base_repository)
            || matches!(
                &self.head_repository,
                ObservedNullable::Value(head) if same_repository(repository, head)
            )
            || matches!(
                &self.rollup_commit,
                ObservedNullable::Value(rollup)
                    if same_repository(repository, &rollup.repository)
            )
    }
}

fn validate_commit_observation(
    commit: ObservedNullable<&DetailsCommitIdentity>,
) -> Result<ObservedNullable<ObservedCommitIdentity>> {
    Ok(match commit {
        ObservedNullable::Missing => ObservedNullable::Missing,
        ObservedNullable::Null => ObservedNullable::Null,
        ObservedNullable::Value(commit) => {
            validate_sha(&commit.oid)?;
            commit.repository.validate_components()?;
            ObservedNullable::Value(ObservedCommitIdentity {
                sha: commit.oid.clone(),
                repository: commit.repository.clone(),
            })
        }
    })
}

fn same_repository(left: &DetailsRepositoryIdentity, right: &DetailsRepositoryIdentity) -> bool {
    left.id == right.id
        && left
            .name_with_owner
            .eq_ignore_ascii_case(&right.name_with_owner)
}

fn domain_repository(repository: &DetailsRepositoryIdentity) -> CheckRepositoryIdentity {
    CheckRepositoryIdentity {
        node_id: repository.id.clone(),
        name_with_owner: repository.name_with_owner.clone(),
    }
}

struct DetailsBuilder {
    head_oid: Option<String>,
    source_identity: Option<DetailsSourceIdentity>,
    viewer: Option<SelectedViewer>,
    pull_request: Option<ProviderCoordinates>,
    viewer_can_administer: Option<bool>,
    overview: Option<DetailsOverview>,
    issue_comments: Vec<IssueComment>,
    reviews: Vec<PullRequestReview>,
    review_threads: Vec<ReviewThread>,
    reactions: Vec<ReactionSubjectSnapshot>,
    checks: Vec<PullRequestCheck>,
    activity_ids: HashSet<String>,
    thread_ids: HashSet<String>,
    check_ids: HashSet<String>,
    activity_complete: bool,
    checks_complete: bool,
    reaction_authority_complete: bool,
    dismissal_authority_complete: bool,
    notices: Vec<String>,
}

impl Default for DetailsBuilder {
    fn default() -> Self {
        Self {
            head_oid: None,
            source_identity: None,
            viewer: None,
            pull_request: None,
            viewer_can_administer: None,
            overview: None,
            issue_comments: Vec::new(),
            reviews: Vec::new(),
            review_threads: Vec::new(),
            reactions: Vec::new(),
            checks: Vec::new(),
            activity_ids: HashSet::new(),
            thread_ids: HashSet::new(),
            check_ids: HashSet::new(),
            activity_complete: true,
            checks_complete: true,
            reaction_authority_complete: true,
            dismissal_authority_complete: true,
            notices: Vec::new(),
        }
    }
}

struct DetailsPageContext {
    viewer_can_administer: bool,
    partial: bool,
    first: bool,
    checks_requested: bool,
}

impl DetailsBuilder {
    fn absorb(
        &mut self,
        repo: &Repository,
        pull: DetailsPull,
        viewer: DetailsViewer,
        context: DetailsPageContext,
    ) -> Result<DetailsCursors> {
        let DetailsPageContext {
            viewer_can_administer,
            partial,
            first,
            checks_requested,
        } = context;
        let number = pull.number;
        let source_identity = DetailsSourceIdentity::from_pull(repo, &pull)?;
        if let Some(prior) = &self.source_identity {
            ensure!(
                prior == &source_identity,
                "PR Checks source identity changed during pagination; refresh to retry"
            );
        } else {
            self.source_identity = Some(source_identity.clone());
        }
        validate_node_id(&viewer.id)?;
        ensure!(
            viewer.login.eq_ignore_ascii_case(&repo.account.login),
            "selected GitHub credential resolved to another account"
        );
        let selected_viewer = SelectedViewer {
            node_id: viewer.id,
            login: viewer.login,
        };
        if let Some(prior) = &self.viewer {
            ensure!(
                prior == &selected_viewer,
                "selected viewer changed during collaboration pagination"
            );
        } else {
            self.viewer = Some(selected_viewer.clone());
        }
        if let Some(prior) = self.viewer_can_administer {
            ensure!(
                prior == viewer_can_administer,
                "repository administration capability changed during collaboration pagination"
            );
        } else {
            self.viewer_can_administer = Some(viewer_can_administer);
        }
        validate_node_id(&pull.id)?;
        let pull_request = coordinates(repo, number, pull.id.clone());
        if let Some(prior) = &self.pull_request {
            ensure!(
                prior == &pull_request,
                "PR node ID changed during collaboration pagination"
            );
        } else {
            self.pull_request = Some(pull_request.clone());
        }
        validate_sha(&pull.head_ref_oid)?;
        if let Some(head) = &self.head_oid {
            ensure!(
                head == &pull.head_ref_oid,
                "PR head changed during collaboration pagination; refresh to retry"
            );
        } else {
            self.head_oid = Some(pull.head_ref_oid.clone());
        }
        if partial {
            self.activity_complete = false;
            self.checks_complete = false;
            self.notice("GitHub returned partial collaboration data; unavailable fields were not treated as complete.");
            self.reaction_authority_complete = false;
            self.dismissal_authority_complete = false;
        }
        if !source_identity.complete {
            self.checks_complete = false;
            self.notice(
                "GitHub omitted requested Checks source identity; the checks snapshot is partial.",
            );
        }
        if first {
            self.reactions.push(reaction_subject_snapshot(
                repo,
                &pull_request,
                ReactableKind::PullRequest,
                pull_request.clone(),
                None,
                pull.body.clone(),
                &pull.reaction_groups,
                pull.viewer_can_react,
                &selected_viewer,
                !partial,
            ));
            let requested_reviewers = pull
                .review_requests
                .nodes
                .iter()
                .flatten()
                .filter_map(|request| request.requested_reviewer.as_ref())
                .filter_map(|reviewer| {
                    reviewer
                        .login
                        .clone()
                        .or_else(|| reviewer.slug.as_ref().map(|slug| format!("team:{slug}")))
                })
                .collect();
            let labels = pull
                .labels
                .nodes
                .iter()
                .flatten()
                .map(|label| label.name.clone())
                .collect();
            let assignees = pull
                .assignees
                .nodes
                .iter()
                .flatten()
                .map(|actor| actor.login.clone())
                .collect();
            for (label, connection_complete) in [
                (
                    "requested reviewers",
                    connection_complete(&pull.review_requests),
                ),
                ("labels", connection_complete(&pull.labels)),
                ("assignees", connection_complete(&pull.assignees)),
            ] {
                if !connection_complete {
                    self.activity_complete = false;
                    self.notice(&format!(
                        "PR {label} are partial at the explicit 100-item overview limit."
                    ));
                }
            }
            self.overview = Some(DetailsOverview {
                body: pull.body.clone(),
                requested_reviewers,
                labels,
                assignees,
                merge_eligibility: MergeEligibility {
                    state: pull.state.clone(),
                    draft: pull.is_draft,
                    mergeable: pull.mergeable.clone(),
                    merge_state_status: pull.merge_state_status.clone(),
                    review_status: map_review_status(pull.review_decision.as_deref(), partial)
                        .into(),
                    check_status: map_check_status(
                        pull.status_check_rollup
                            .value()
                            .and_then(|rollup| rollup.state.as_deref()),
                        partial,
                    )
                    .into(),
                    maintainer_can_modify: pull.maintainer_can_modify,
                    can_rebase: pull.can_be_rebased,
                    can_update_branch: pull.viewer_can_update_branch,
                    auto_merge_enabled: pull.auto_merge_request.is_some(),
                    in_merge_queue: pull.is_in_merge_queue,
                },
            });
        }

        let mut next = DetailsCursors::default();
        if let Some(connection) = pull.comments {
            next.comments = next_cursor(&connection.page_info)?;
            if connection.nodes.iter().any(Option::is_none) {
                self.activity_complete = false;
                self.notice("Issue comments contained unavailable entries; the activity snapshot is partial.");
            }
            for comment in connection.nodes.into_iter().flatten() {
                ensure!(
                    self.activity_ids.insert(comment.id.clone()),
                    "PR activity changed during pagination; refresh to retry"
                );
                self.reactions.push(reaction_subject_snapshot(
                    repo,
                    &pull_request,
                    ReactableKind::IssueComment,
                    coordinates(repo, number, comment.id.clone()),
                    None,
                    comment.body.clone(),
                    &comment.reaction_groups,
                    comment.viewer_can_react,
                    &selected_viewer,
                    !partial,
                ));
                self.issue_comments.push(comment.into_domain(repo, number));
            }
        }
        if let Some(connection) = pull.reviews {
            next.reviews = next_cursor(&connection.page_info)?;
            if connection.nodes.iter().any(Option::is_none) {
                self.activity_complete = false;
                self.notice(
                    "Reviews contained unavailable entries; the activity snapshot is partial.",
                );
            }
            for review in connection.nodes.into_iter().flatten() {
                ensure!(
                    self.activity_ids.insert(review.id.clone()),
                    "PR activity changed during pagination; refresh to retry"
                );
                self.reactions.push(reaction_subject_snapshot(
                    repo,
                    &pull_request,
                    ReactableKind::PullRequestReview,
                    coordinates(repo, number, review.id.clone()),
                    None,
                    review.body.clone(),
                    &review.reaction_groups,
                    review.viewer_can_react,
                    &selected_viewer,
                    !partial,
                ));
                let dismissal_capability = (!partial).then(|| FreshReviewDismissalCapability {
                    viewer: selected_viewer.clone(),
                    pull_request: pull_request.clone(),
                    authority: dismissal_authority(&review.state, viewer_can_administer),
                });
                self.reviews
                    .push(review.into_domain(repo, number, !partial, dismissal_capability));
            }
        }
        if let Some(connection) = pull.review_threads {
            next.threads = next_cursor(&connection.page_info)?;
            if connection.nodes.iter().any(Option::is_none) {
                self.activity_complete = false;
                self.notice("Review threads contained unavailable entries; the activity snapshot is partial.");
            }
            for thread in connection.nodes.into_iter().flatten() {
                ensure!(
                    self.thread_ids.insert(thread.id.clone()),
                    "PR review threads changed during pagination; refresh to retry"
                );
                let comments_complete = connection_complete(&thread.comments);
                if thread.comments.page_info.has_next_page {
                    self.activity_complete = false;
                    self.notice(&format!(
                        "Review thread comments are partial at the explicit {PARTICIPANT_LIMIT}-comment per-thread limit."
                    ));
                }
                if thread.comments.nodes.iter().any(Option::is_none) {
                    self.activity_complete = false;
                    self.notice("Review thread comments contained unavailable entries; the activity snapshot is partial.");
                }
                for comment in thread.comments.nodes.iter().flatten() {
                    let parent_review = comment
                        .pull_request_review
                        .as_ref()
                        .map(|parent| coordinates(repo, number, parent.id.clone()));
                    self.reactions.push(reaction_subject_snapshot(
                        repo,
                        &pull_request,
                        ReactableKind::PullRequestReviewComment,
                        coordinates(repo, number, comment.id.clone()),
                        parent_review,
                        comment.body.clone(),
                        comment.reaction_groups.as_deref().unwrap_or_default(),
                        comment.viewer_can_react.unwrap_or(false),
                        &selected_viewer,
                        !partial
                            && comments_complete
                            && comment.viewer_can_react.is_some()
                            && comment.reaction_groups.is_some(),
                    ));
                }
                self.review_threads
                    .push(thread.into_domain(repo, number, comments_complete));
            }
        }
        let checks_response_present = matches!(
            &pull.status_check_rollup,
            ObservedNullable::Null
                | ObservedNullable::Value(DetailsRollup {
                    contexts: ObservedNullable::Value(_),
                    ..
                })
        );
        if checks_requested && !checks_response_present {
            self.checks_complete = false;
            self.notice(
                "GitHub omitted the requested Checks page; the checks snapshot is partial.",
            );
        }
        if let ObservedNullable::Value(rollup) = pull.status_check_rollup
            && let ObservedNullable::Value(connection) = rollup.contexts
        {
            next.checks = next_cursor(&connection.page_info)?;
            if connection.nodes.iter().any(Option::is_none) {
                self.checks_complete = false;
                self.notice(
                    "Checks contained unavailable entries; the checks snapshot is partial.",
                );
            }
            for check in connection.nodes.into_iter().flatten() {
                let mapped = check.into_domain(repo, number, &source_identity)?;
                if !mapped.complete {
                    self.checks_complete = false;
                    self.notice("Checks omitted exact identity fields; affected rows remain read-only and unknown.");
                }
                if let Some(check) = mapped.check {
                    ensure!(
                        self.check_ids.insert(check.coordinates.remote_id.clone()),
                        "PR checks changed during pagination; refresh to retry"
                    );
                    self.checks.push(check);
                }
            }
        }
        Ok(next)
    }

    fn notice(&mut self, notice: &str) {
        if !self.notices.iter().any(|known| known == notice) {
            self.notices.push(notice.to_owned());
        }
    }

    fn finish(mut self, number: u64) -> Result<PullRequestDetails> {
        let overview = self
            .overview
            .context("GitHub PR overview was unavailable")?;
        if !self.reaction_authority_complete {
            for reaction in &mut self.reactions {
                reaction.fresh_capability = None;
            }
        }
        if !self.dismissal_authority_complete {
            for review in &mut self.reviews {
                review.dismissal_capability = None;
            }
        }
        Ok(PullRequestDetails {
            number,
            pull_request_node_id: self
                .source_identity
                .as_ref()
                .map(|source| source.pull_request_node_id.clone()),
            base_repository: self
                .source_identity
                .as_ref()
                .map(|source| domain_repository(&source.base_repository)),
            observed_head_sha: self
                .source_identity
                .as_ref()
                .map(|source| source.head_sha.clone()),
            rollup_commit_sha: self
                .source_identity
                .as_ref()
                .and_then(|source| source.rollup_commit_sha().map(str::to_owned)),
            potential_merge_commit_sha: self
                .source_identity
                .as_ref()
                .and_then(|source| source.potential_merge_commit_sha().map(str::to_owned)),
            head_repository: self
                .source_identity
                .as_ref()
                .and_then(|source| source.head_repository.value().map(domain_repository)),
            rollup_repository: self.source_identity.as_ref().and_then(|source| {
                source
                    .rollup_commit
                    .value()
                    .map(|commit| domain_repository(&commit.repository))
            }),
            potential_merge_commit_repository: self.source_identity.as_ref().and_then(|source| {
                source
                    .potential_merge_commit
                    .value()
                    .map(|commit| domain_repository(&commit.repository))
            }),
            body: overview.body,
            requested_reviewers: overview.requested_reviewers,
            labels: overview.labels,
            assignees: overview.assignees,
            merge_eligibility: overview.merge_eligibility,
            issue_comments: self.issue_comments,
            reviews: self.reviews,
            review_threads: self.review_threads,
            reactions: self.reactions,
            checks: self.checks,
            activity_complete: self.activity_complete,
            checks_complete: self.checks_complete,
            notice: (!self.notices.is_empty()).then(|| self.notices.join(" ")),
        })
    }
}

fn next_cursor(page: &PageInfo) -> Result<ConnectionCursor> {
    if !page.has_next_page {
        return Ok(ConnectionCursor::default());
    }
    let after = page
        .end_cursor
        .clone()
        .filter(|cursor| !cursor.is_empty())
        .context("GitHub pagination omitted its continuation cursor")?;
    Ok(ConnectionCursor {
        after: Some(after),
        include: true,
    })
}

fn coordinates(repo: &Repository, pull_request: u64, remote_id: String) -> ProviderCoordinates {
    ProviderCoordinates {
        provider: "github".into(),
        host: repo.host.clone(),
        owner: repo.owner.clone(),
        repository: repo.name.clone(),
        pull_request,
        remote_id,
    }
}

impl DetailsIssueComment {
    fn into_domain(self, repo: &Repository, number: u64) -> IssueComment {
        IssueComment {
            coordinates: coordinates(repo, number, self.id),
            author: self.author.map(|author| author.login),
            body: self.body,
            created_at: self.created_at,
            updated_at: self.updated_at,
            url: self.url,
        }
    }
}

impl DetailsReview {
    fn into_domain(
        self,
        repo: &Repository,
        number: u64,
        capabilities_complete: bool,
        dismissal_capability: Option<FreshReviewDismissalCapability>,
    ) -> PullRequestReview {
        PullRequestReview {
            coordinates: coordinates(repo, number, self.id),
            author: self.author.map(|author| author.login),
            body: self.body,
            state: self.state,
            submitted_at: self.submitted_at,
            commit_sha: self.commit.map(|commit| commit.oid),
            edit_summary_capability: capabilities_complete.then_some(
                SubmittedReviewEditCapability {
                    viewer_did_author: self.viewer_did_author,
                    viewer_can_update: self.viewer_can_update,
                    viewer_cannot_update_reasons: self.viewer_cannot_update_reasons,
                },
            ),
            dismissal_capability,
            url: self.url,
        }
    }
}

fn dismissal_authority(state: &str, viewer_can_administer: bool) -> DismissalAuthority {
    if !matches!(state, "APPROVED" | "CHANGES_REQUESTED") {
        return DismissalAuthority::Unavailable {
            reason: "Only approved or changes-requested submitted reviews can be dismissed.".into(),
        };
    }
    if viewer_can_administer {
        DismissalAuthority::Available
    } else {
        DismissalAuthority::Unknown {
            reason: "GitHub exposes no per-review dismissal capability. Authorization is unknown; GitHub will decide when the confirmed request is sent."
                .into(),
        }
    }
}

impl DetailsThread {
    fn into_domain(self, repo: &Repository, number: u64, comments_complete: bool) -> ReviewThread {
        let side = self.diff_side.clone();
        ReviewThread {
            coordinates: coordinates(repo, number, self.id),
            path: self.path,
            subject: self
                .subject_type
                .as_deref()
                .map(ReviewSubject::from_provider)
                .unwrap_or(ReviewSubject::Unknown),
            line: self.line,
            original_line: self.original_line,
            start_line: self.start_line,
            original_start_line: self.original_start_line,
            side: self.diff_side,
            start_side: self.start_diff_side,
            resolved: self.is_resolved,
            outdated: self.is_outdated,
            comments: self
                .comments
                .nodes
                .into_iter()
                .flatten()
                .map(|comment| comment.into_domain(repo, number, side.clone()))
                .collect(),
            comments_complete,
        }
    }
}

impl DetailsReviewComment {
    fn into_domain(self, repo: &Repository, number: u64, side: Option<String>) -> ReviewComment {
        ReviewComment {
            coordinates: coordinates(repo, number, self.id),
            author: self.author.map(|author| author.login),
            body: self.body,
            created_at: self.created_at,
            updated_at: self.updated_at,
            url: self.url,
            path: self.path,
            subject: self
                .subject_type
                .as_deref()
                .map(ReviewSubject::from_provider)
                .unwrap_or(ReviewSubject::Unknown),
            line: self.line,
            original_line: self.original_line,
            start_line: self.start_line,
            original_start_line: self.original_start_line,
            side,
            diff_hunk: self.diff_hunk,
            commit_sha: self.commit.map(|commit| commit.oid),
            original_commit_sha: self.original_commit.map(|commit| commit.oid),
            outdated: self.outdated,
        }
    }
}

impl DetailsCheckNode {
    fn into_domain(
        self,
        repo: &Repository,
        number: u64,
        source: &DetailsSourceIdentity,
    ) -> Result<MappedCheck> {
        match self {
            Self::CheckRun {
                id,
                database_id,
                name,
                status,
                conclusion,
                permalink,
                details_url,
                started_at,
                completed_at,
                required,
                repository,
                check_suite,
            } => {
                let (Some(id), Some(name), Some(status)) = (id, name, status) else {
                    return Ok(MappedCheck::missing());
                };
                if validate_node_id(&id).is_err()
                    || !valid_identity_text(&name)
                    || !valid_identity_text(&status)
                {
                    return Ok(MappedCheck::missing());
                }
                let mut complete = required.is_some();
                complete &= validate_nested_repository(source, repository.as_ref())?;
                let (database_id, database_id_complete) = optional_graphql_database_id(database_id);
                complete &= database_id_complete;
                let (github_permalink, permalink_complete) = required_github_url(permalink);
                complete &= permalink_complete;
                let (details_url, details_url_complete) = optional_display_uri(details_url);
                complete &= details_url_complete;
                let suite =
                    map_check_suite(source, repository.as_ref(), check_suite.map(|suite| *suite))?;
                complete &= suite.complete;
                let commit_sha = suite.commit_sha;
                let sha_class = classify_check_sha(commit_sha.as_deref(), source);
                Ok(MappedCheck {
                    check: Some(PullRequestCheck {
                        coordinates: coordinates(repo, number, id),
                        kind: CheckKind::CheckRun,
                        name,
                        status,
                        conclusion,
                        description: None,
                        details_url,
                        github_permalink,
                        started_at,
                        completed_at,
                        required,
                        database_id,
                        suite: suite.identity,
                        commit_sha,
                        commit_repository: suite.commit_repository,
                        sha_class,
                        actions_linkage: suite.actions_linkage,
                    }),
                    complete,
                })
            }
            Self::StatusContext {
                id,
                context,
                state,
                description,
                target_url,
                created_at,
                updated_at,
                required,
                commit,
            } => {
                let (Some(id), Some(context), Some(state)) = (id, context, state) else {
                    return Ok(MappedCheck::missing());
                };
                if validate_node_id(&id).is_err()
                    || !valid_identity_text(&context)
                    || !valid_identity_text(&state)
                {
                    return Ok(MappedCheck::missing());
                }
                let commit_repository = commit_repository_for_status(commit.as_ref());
                let (commit_sha, commit_complete) = map_optional_check_commit(source, commit)?;
                let (details_url, details_url_complete) = optional_display_uri(target_url);
                let complete = required.is_some()
                    && created_at.is_some()
                    && updated_at.is_some()
                    && commit_complete
                    && details_url_complete;
                Ok(MappedCheck {
                    check: Some(PullRequestCheck {
                        coordinates: coordinates(repo, number, id),
                        kind: CheckKind::CommitStatus,
                        name: context,
                        status: state,
                        conclusion: None,
                        description,
                        details_url,
                        github_permalink: None,
                        started_at: created_at,
                        completed_at: updated_at,
                        required,
                        database_id: None,
                        suite: None,
                        sha_class: classify_check_sha(commit_sha.as_deref(), source),
                        commit_sha,
                        commit_repository,
                        actions_linkage: ActionsLinkage::NoObservedLink,
                    }),
                    complete,
                })
            }
            Self::Unknown => Ok(MappedCheck::missing()),
        }
    }
}

struct MappedCheck {
    check: Option<PullRequestCheck>,
    complete: bool,
}

impl MappedCheck {
    fn missing() -> Self {
        Self {
            check: None,
            complete: false,
        }
    }
}

struct MappedCheckSuite {
    identity: Option<CheckSuiteIdentity>,
    commit_sha: Option<String>,
    commit_repository: Option<CheckRepositoryIdentity>,
    actions_linkage: ActionsLinkage,
    complete: bool,
}

fn map_check_suite(
    source: &DetailsSourceIdentity,
    check_repository: Option<&DetailsRepositoryIdentity>,
    suite: Option<DetailsCheckSuite>,
) -> Result<MappedCheckSuite> {
    let Some(suite) = suite else {
        return Ok(MappedCheckSuite {
            identity: None,
            commit_sha: None,
            commit_repository: None,
            actions_linkage: ActionsLinkage::Unknown,
            complete: false,
        });
    };
    let mut complete = validate_nested_repository(source, suite.repository.as_ref())?;
    if let (Some(check_repository), Some(suite_repository)) =
        (check_repository, suite.repository.as_ref())
    {
        ensure!(
            same_repository(check_repository, suite_repository),
            "GitHub CheckRun and CheckSuite repository identities disagree"
        );
    }
    let suite_repository = suite.repository;
    let commit_sha = if let Some(commit) = suite.commit {
        validate_sha(&commit.oid)?;
        ensure!(
            source.allows_check_repository(&commit.repository),
            "GitHub check-suite commit belongs to an unrelated repository"
        );
        if let Some(suite_repository) = &suite_repository {
            ensure!(
                same_repository(&commit.repository, suite_repository),
                "GitHub CheckSuite and its commit repository identities disagree"
            );
        }
        Some(commit.oid)
    } else {
        complete = false;
        None
    };
    let (database_id, database_id_complete) = optional_graphql_database_id(suite.database_id);
    complete &= database_id_complete;
    let app = match suite.app {
        ObservedNullable::Missing => {
            complete = false;
            None
        }
        ObservedNullable::Null => None,
        ObservedNullable::Value(app) => match (app.id, app.name, app.slug) {
            (Some(node_id), Some(name), Some(slug))
                if validate_node_id(&node_id).is_ok()
                    && valid_identity_text(&name)
                    && valid_identity_text(&slug) =>
            {
                Some(CheckAppIdentity {
                    node_id,
                    name,
                    slug,
                })
            }
            _ => {
                complete = false;
                None
            }
        },
    };
    let actions_linkage = match suite.workflow_run {
        ObservedNullable::Missing => {
            complete = false;
            ActionsLinkage::Unknown
        }
        ObservedNullable::Null => ActionsLinkage::NoObservedLink,
        ObservedNullable::Value(run) => match map_workflow_run(run) {
            Some(run) => ActionsLinkage::Linked(run),
            None => {
                complete = false;
                ActionsLinkage::Unknown
            }
        },
    };
    finish_check_suite(
        suite.id,
        database_id,
        suite_repository,
        app,
        commit_sha,
        actions_linkage,
        complete,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_check_suite(
    node_id: Option<String>,
    database_id: Option<u64>,
    repository: Option<DetailsRepositoryIdentity>,
    app: Option<CheckAppIdentity>,
    commit_sha: Option<String>,
    actions_linkage: ActionsLinkage,
    mut complete: bool,
) -> Result<MappedCheckSuite> {
    let identity = match (node_id, repository.as_ref()) {
        (Some(node_id), Some(repository)) if validate_node_id(&node_id).is_ok() => {
            Some(CheckSuiteIdentity {
                node_id,
                database_id,
                repository: domain_repository(repository),
                app,
            })
        }
        _ => {
            complete = false;
            None
        }
    };
    Ok(MappedCheckSuite {
        identity,
        commit_sha,
        commit_repository: repository.as_ref().map(domain_repository),
        actions_linkage,
        complete,
    })
}

fn map_workflow_run(run: DetailsWorkflowRun) -> Option<WorkflowRunIdentity> {
    let workflow = run.workflow?;
    let node_id = run.id?;
    let database_id = required_graphql_database_id(run.database_id)?;
    let run_attempt = required_graphql_int(run.run_attempt)?;
    let run_number = required_graphql_int(run.run_number)?;
    let event = run.event?;
    let github_url = run.url?;
    let workflow_node_id = workflow.id?;
    let workflow_database_id = required_graphql_database_id(workflow.database_id)?;
    let workflow_name = workflow.name?;
    (validate_node_id(&node_id).is_ok()
        && validate_node_id(&workflow_node_id).is_ok()
        && valid_identity_text(&event)
        && valid_identity_text(&workflow_name)
        && valid_github_url(&github_url))
    .then_some(WorkflowRunIdentity {
        node_id,
        database_id,
        run_attempt,
        run_number,
        event,
        github_url,
        workflow_node_id,
        workflow_database_id,
        workflow_name,
    })
}

fn map_optional_check_commit(
    source: &DetailsSourceIdentity,
    commit: ObservedNullable<DetailsCommitIdentity>,
) -> Result<(Option<String>, bool)> {
    Ok(match commit {
        ObservedNullable::Missing => (None, false),
        ObservedNullable::Null => (None, false),
        ObservedNullable::Value(commit) => {
            validate_sha(&commit.oid)?;
            ensure!(
                source.allows_check_repository(&commit.repository),
                "GitHub status context belongs to an unrelated repository"
            );
            (Some(commit.oid), true)
        }
    })
}

fn validate_nested_repository(
    source: &DetailsSourceIdentity,
    repository: Option<&DetailsRepositoryIdentity>,
) -> Result<bool> {
    let Some(repository) = repository else {
        return Ok(false);
    };
    repository.validate_components()?;
    ensure!(
        source.allows_check_repository(repository),
        "GitHub check belongs to an unrelated repository"
    );
    Ok(true)
}

fn commit_repository_for_status(
    commit: ObservedNullable<&DetailsCommitIdentity>,
) -> Option<CheckRepositoryIdentity> {
    commit
        .value()
        .map(|commit| domain_repository(&commit.repository))
}

fn classify_check_sha(commit_sha: Option<&str>, source: &DetailsSourceIdentity) -> CheckShaClass {
    match commit_sha {
        Some(sha) if sha == source.head_sha => CheckShaClass::Head,
        Some(sha) if Some(sha) == source.potential_merge_commit_sha() => {
            CheckShaClass::MergeCandidate
        }
        Some(_) => CheckShaClass::Other,
        None => CheckShaClass::Unknown,
    }
}

fn optional_graphql_database_id(value: ObservedNullable<i64>) -> (Option<u64>, bool) {
    match value {
        ObservedNullable::Missing => (None, false),
        ObservedNullable::Null => (None, true),
        ObservedNullable::Value(value) => match required_graphql_int(Some(value)) {
            Some(value) => (Some(value), true),
            None => (None, false),
        },
    }
}

fn required_graphql_database_id(value: ObservedNullable<i64>) -> Option<u64> {
    match value {
        ObservedNullable::Value(value) => required_graphql_int(Some(value)),
        ObservedNullable::Missing | ObservedNullable::Null => None,
    }
}

fn required_graphql_int(value: Option<i64>) -> Option<u64> {
    value
        .filter(|value| (1..=i64::from(i32::MAX)).contains(value))
        .and_then(|value| u64::try_from(value).ok())
}

fn valid_identity_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.chars().any(|character| character.is_control())
}

fn valid_display_uri(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 16 * 1024
        && !value.chars().any(|character| character.is_control())
        && !value.chars().any(char::is_whitespace)
}

fn valid_github_url(value: &str) -> bool {
    value.starts_with("https://github.com/") && valid_display_uri(value)
}

fn optional_display_uri(value: Option<String>) -> (Option<String>, bool) {
    match value {
        Some(value) if valid_display_uri(&value) => (Some(value), true),
        Some(_) => (None, false),
        None => (None, true),
    }
}

fn required_github_url(value: Option<String>) -> (Option<String>, bool) {
    match value {
        Some(value) if valid_github_url(&value) => (Some(value), true),
        Some(_) | None => (None, false),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApiUser {
    login: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApiRepository {
    name: String,
    owner: ApiUser,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApiRef {
    sha: String,
    #[serde(rename = "ref")]
    branch: String,
    repo: Option<ApiRepository>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApiLabel {
    name: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApiTeam {
    slug: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ApiPullRequest {
    number: u64,
    title: String,
    body: Option<String>,
    user: Option<ApiUser>,
    state: String,
    #[serde(default)]
    draft: bool,
    merged_at: Option<String>,
    html_url: String,
    base: ApiRef,
    head: ApiRef,
    #[serde(default)]
    requested_reviewers: Vec<ApiUser>,
    #[serde(default)]
    requested_teams: Vec<ApiTeam>,
    #[serde(default)]
    assignees: Vec<ApiUser>,
    #[serde(default)]
    labels: Vec<ApiLabel>,
    changed_files: Option<u64>,
    updated_at: String,
}
impl ApiPullRequest {
    fn revision(&self) -> Revision {
        Revision {
            base_sha: self.base.sha.clone(),
            head_sha: self.head.sha.clone(),
        }
    }
    fn validate(&self, repo: &Repository, number: Option<u64>) -> Result<()> {
        ensure!(
            self.number > 0 && number.is_none_or(|n| self.number == n),
            "API returned a different PR"
        );
        let base_repo = self
            .base
            .repo
            .as_ref()
            .context("Missing PR base repository")?;
        ensure!(
            base_repo.owner.login.eq_ignore_ascii_case(&repo.owner)
                && base_repo.name.eq_ignore_ascii_case(&repo.name),
            "PR repository mismatch"
        );
        ensure!(
            self.html_url
                == format!(
                    "https://{}/{}/pull/{}",
                    repo.host,
                    repo.full_name(),
                    self.number
                ),
            "PR URL/repository mismatch"
        );
        ensure!(
            ["open", "closed"].contains(&self.state.as_str()),
            "Invalid PR state"
        );
        validate_sha(&self.base.sha)?;
        validate_sha(&self.head.sha)
    }
    fn into_domain(self) -> PullRequest {
        PullRequest {
            number: self.number,
            title: self.title,
            body: self.body.unwrap_or_default(),
            source_branch: self.head.branch,
            target_branch: self.base.branch,
            author: self.user.map(|u| u.login).unwrap_or_default(),
            reviewers: self
                .requested_reviewers
                .into_iter()
                .map(|u| u.login)
                .chain(
                    self.requested_teams
                        .into_iter()
                        .map(|t| format!("team:{}", t.slug)),
                )
                .collect(),
            assignees: self.assignees.into_iter().map(|u| u.login).collect(),
            labels: self.labels.into_iter().map(|l| l.name).collect(),
            participants: Vec::new(),
            participants_complete: false,
            participants_notice: Some("Participant metadata has not been hydrated.".into()),
            draft: self.draft,
            state: if self.merged_at.is_some() {
                "MERGED"
            } else if self.state == "open" {
                "OPEN"
            } else {
                "CLOSED"
            }
            .into(),
            // These are replaced by the account-isolated GraphQL metadata read.
            review_status: "UNKNOWN".into(),
            check_status: "UNKNOWN".into(),
            base_sha: self.base.sha,
            head_sha: self.head.sha,
            url: self.html_url,
        }
    }
}
#[derive(Deserialize)]
struct ApiCommit {
    sha: String,
}
#[derive(Deserialize)]
struct ApiComparison {
    base_commit: ApiCommit,
    merge_base_commit: ApiCommit,
    #[serde(default)]
    total_commits: usize,
    #[serde(default)]
    commits: Vec<ApiCommit>,
    #[serde(default)]
    files: Vec<ApiFile>,
}
#[derive(Deserialize)]
struct ApiFile {
    filename: String,
    previous_filename: Option<String>,
    status: String,
    additions: u64,
    deletions: u64,
    patch: Option<String>,
}
impl ApiFile {
    fn same_change(&self, other: &Self) -> bool {
        self.filename == other.filename
            && self.previous_filename == other.previous_filename
            && self.status == other.status
            && self.additions == other.additions
            && self.deletions == other.deletions
            && self.patch == other.patch
    }
    fn patch_complete(&self) -> bool {
        self.patch
            .as_deref()
            .is_some_and(|patch| patch_is_complete(patch, self.additions, self.deletions))
    }
    fn into_domain(self) -> ChangedFile {
        let patch_complete = self.patch_complete();
        ChangedFile {
            path: self.filename,
            raw_path: None,
            raw_previous_path: None,
            previous_path: self.previous_filename,
            status: self.status,
            additions: self.additions,
            deletions: self.deletions,
            patch: self.patch.filter(|p| !p.is_empty()),
            patch_complete,
        }
    }
}
fn validate_files(files: &[ApiFile]) -> Result<()> {
    let mut seen = HashSet::new();
    for file in files {
        ensure!(
            !file.filename.is_empty()
                && !file.filename.contains('\0')
                && seen.insert(&file.filename),
            "Invalid or repeated file in GitHub pagination"
        );
    }
    Ok(())
}

fn validate_direct_compare_identity(comparison: &ApiComparison, revision: &Revision) -> Result<()> {
    ensure!(
        comparison.base_commit.sha == revision.base_sha,
        "GitHub comparison returned a different base commit"
    );
    ensure!(
        comparison.merge_base_commit.sha == revision.base_sha,
        "GitHub three-dot comparison cannot represent the requested direct-tree pair"
    );
    validate_sha(&comparison.base_commit.sha)?;
    validate_sha(&comparison.merge_base_commit.sha)?;
    for commit in &comparison.commits {
        validate_sha(&commit.sha)?;
    }
    Ok(())
}

/// Validate every hunk's declared line counts and total additions/deletions. A
/// syntactically valid prefix alone does not establish a complete GitHub patch.
fn patch_is_complete(patch: &str, additions: u64, deletions: u64) -> bool {
    if patch.is_empty() {
        return false;
    }
    let mut remaining = (0_u64, 0_u64);
    let (mut added, mut removed, mut hunks) = (0_u64, 0_u64, 0_u64);
    for line in patch.lines() {
        if line.starts_with("@@ ") {
            if remaining != (0, 0) {
                return false;
            }
            let parts: Vec<_> = line.splitn(5, ' ').collect();
            if parts.len() < 4 || parts[0] != "@@" || parts[3] != "@@" {
                return false;
            }
            let count = |text: &str, prefix: char| -> Option<u64> {
                let text = text.strip_prefix(prefix)?;
                let (start, count) = text.split_once(',').unwrap_or((text, "1"));
                start.parse::<u64>().ok()?;
                count.parse().ok()
            };
            let (Some(old), Some(new)) = (count(parts[1], '-'), count(parts[2], '+')) else {
                return false;
            };
            remaining = (old, new);
            hunks += 1;
        } else {
            if hunks == 0 {
                return false;
            }
            let consumes = match line.as_bytes().first() {
                Some(b'+') => {
                    added += 1;
                    (0, 1)
                }
                Some(b'-') => {
                    removed += 1;
                    (1, 0)
                }
                Some(b' ') => (1, 1),
                Some(b'\\') if line == "\\ No newline at end of file" => (0, 0),
                _ => return false,
            };
            let (Some(old), Some(new)) = (
                remaining.0.checked_sub(consumes.0),
                remaining.1.checked_sub(consumes.1),
            ) else {
                return false;
            };
            remaining = (old, new);
        }
    }
    hunks > 0 && remaining == (0, 0) && added == additions && removed == deletions
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{fs, os::unix::fs::PermissionsExt};
    use tempfile::TempDir;

    const BASE: &str = "1111111111111111111111111111111111111111";
    const HEAD: &str = "2222222222222222222222222222222222222222";

    fn account(login: &str) -> Account {
        Account {
            host: HOST.into(),
            login: login.into(),
        }
    }
    fn repo(login: &str) -> Repository {
        Repository {
            host: HOST.into(),
            owner: "owner".into(),
            name: "repo".into(),
            account: account(login),
            local_path: None,
        }
    }
    fn revision() -> Revision {
        Revision {
            base_sha: BASE.into(),
            head_sha: HEAD.into(),
        }
    }
    fn pull(number: u64, count: usize) -> Value {
        json!({"number": number, "title": "PR title", "body": null, "user": {"login": "author"}, "state": "open", "draft": false,
            "html_url": format!("https://github.com/owner/repo/pull/{number}"),
            "base": {"sha": BASE, "ref": "main", "repo": {"name": "repo", "owner": {"login": "owner"}}},
            "head": {"sha": HEAD, "ref": "feature", "repo": null}, "updated_at": "2026-09-12T12:00:00Z", "changed_files": count,
            "requested_reviewers": [{"login": "reviewer"}], "requested_teams": [{"slug": "maintainers"}], "assignees": [{"login": "assignee"}], "labels": [{"name": "bug"}]})
    }
    fn file(index: usize) -> Value {
        json!({"filename": format!("src/file{index}.rs"), "status": "modified", "additions": 1, "deletions": 1, "patch": "@@ -1 +1 @@\n-old\n+new"})
    }
    fn compare(files: Vec<Value>) -> Value {
        json!({"base_commit": {"sha": BASE}, "merge_base_commit": {"sha": BASE}, "files": files})
    }
    fn compare_path() -> String {
        format!("repos/owner/repo/compare/{BASE}...{HEAD}?per_page=1&page=1")
    }
    fn inventory_commit(sha: &str, parent: &str, headline: &str) -> Value {
        json!({"commit": {
            "oid": sha,
            "messageHeadline": headline,
            "authoredDate": "2026-09-13T10:00:00Z",
            "committedDate": "2026-09-13T10:00:00Z",
            "parents": {"totalCount": 1, "nodes": [{"oid": parent}]}
        }})
    }
    fn inventory_page(
        base: &str,
        head: &str,
        nodes: Vec<Value>,
        total: usize,
        has_next: bool,
        cursor: Option<&str>,
    ) -> Value {
        json!({"data": {"repository": {
            "nameWithOwner": "owner/repo",
            "pullRequest": {
                "number": 1,
                "baseRefOid": base,
                "headRefOid": head,
                "commits": {
                    "totalCount": total,
                    "nodes": nodes,
                    "pageInfo": {"hasNextPage": has_next, "endCursor": cursor}
                }
            }
        }}})
    }
    fn direct_compare(
        base: &str,
        merge_base: &str,
        total: usize,
        commits: Vec<Value>,
        files: Vec<Value>,
    ) -> Value {
        json!({
            "base_commit": {"sha": base},
            "merge_base_commit": {"sha": merge_base},
            "total_commits": total,
            "commits": commits,
            "files": files,
        })
    }
    fn step(path: &str, response: Value) -> Value {
        json!({"endpoint": path, "response": response})
    }
    fn graphql_step(response: Value, numbers: &[u64]) -> Value {
        json!({"graphql": true, "response": response, "numbers": numbers})
    }
    fn details_step(response: Value, variables: Value) -> Value {
        json!({"graphql": true, "response": response, "variables": variables})
    }
    fn metadata(numbers: &[u64]) -> Value {
        let mut pulls = Map::new();
        pulls.insert("nameWithOwner".into(), json!("owner/repo"));
        for (index, number) in numbers.iter().enumerate() {
            pulls.insert(
                format!("pr{index}"),
                json!({
                    "number": number,
                    "url": format!("https://github.com/owner/repo/pull/{number}"),
                    "reviewDecision": "APPROVED",
                    "statusCheckRollup": {"state": "SUCCESS"},
                    "comments": {"nodes": [{"author": {"login": "commenter"}}], "pageInfo": {"hasNextPage": false, "endCursor": null}},
                    "reviews": {"nodes": [{"author": {"login": "review-author"}, "state": "APPROVED"}], "pageInfo": {"hasNextPage": false, "endCursor": null}},
                    "reviewRequests": {"nodes": [{"requestedReviewer": {"login": "reviewer"}}], "pageInfo": {"hasNextPage": false, "endCursor": null}},
                    "assignees": {"nodes": [{"login": "assignee"}], "pageInfo": {"hasNextPage": false, "endCursor": null}}
                }),
            );
        }
        json!({"data": {"repository": Value::Object(pulls)}})
    }

    // A real child executable verifies argument arrays and its private environment.
    // Logs contain only a numeric call count; fixture credential bytes never log.
    fn fixture(login: &str, steps: Vec<Value>) -> (TempDir, GithubProvider) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("steps.json"),
            serde_json::to_vec(&json!({"login": login, "steps": steps})).unwrap(),
        )
        .unwrap();
        let executable = dir.path().join("gh");
        fs::write(&executable, r#"#!/usr/bin/python3
import json, os, pathlib, sys
root = pathlib.Path(__file__).parent
config = json.loads((root / 'steps.json').read_text())
args = sys.argv[1:]
for key in ['GITHUB_TOKEN', 'GH_ENTERPRISE_TOKEN', 'GITHUB_ENTERPRISE_TOKEN', 'GH_HOST', 'GH_REPO', 'GH_DEBUG', 'DEBUG', 'GH_HTTP_UNIX_SOCKET']:
    assert key not in os.environ, 'conflicting environment'
assert os.environ.get('GH_PROMPT_DISABLED') == '1'
credential = 'fixture-private-' + config['login']
if args[:2] == ['auth', 'token']:
    assert args == ['auth', 'token', '--hostname', 'github.com', '--user', config['login']]
    assert 'GH_TOKEN' not in os.environ
    counter = root / 'tokens'
    counter.write_text(str(int(counter.read_text()) + 1 if counter.exists() else 1))
    print(config.get('token_output', credential))
    sys.exit(1 if config.get('token_fail') else 0)
counter = root / 'count'
index = int(counter.read_text()) if counter.exists() else 0
step = config['steps'][index]
if args[:2] == ['auth', 'status']:
    assert args == ['auth', 'status', '--hostname', 'github.com', '--json', 'hosts']
    assert 'GH_TOKEN' not in os.environ
else:
    assert os.environ.get('GH_TOKEN') == credential, 'wrong identity'
    if step.get('graphql'):
        assert args == ['api', '--hostname', 'github.com', '--method', 'POST', '--header', 'Accept: application/vnd.github+json', '--header', 'X-GitHub-Api-Version: 2026-03-10', 'graphql', '--input', '-'], 'unexpected GraphQL request'
        payload = json.load(sys.stdin)
        assert payload['query'].lstrip().startswith('query '), 'not a query'
        assert 'mutation' not in payload['query'], 'mutation attempted'
        actual = [payload['variables']['n' + str(i)] for i in range(len(step.get('numbers', [])))]
        assert actual == step.get('numbers', []), 'wrong PR batch'
        for key, value in step.get('variables', {}).items():
            assert payload['variables'].get(key) == value, 'wrong GraphQL variable ' + key
    else:
        assert args == ['api', '--hostname', 'github.com', '--method', 'GET', '--header', 'Accept: application/vnd.github+json', '--header', 'X-GitHub-Api-Version: 2026-03-10', step['endpoint']], 'unexpected request'
counter.write_text(str(index + 1))
if step.get('fail'):
    print(credential, file=sys.stderr)
    print(credential)
    sys.exit(1)
if step.get('raw') is not None:
    print(step['raw'])
else:
    print(json.dumps(step['response']))
"#).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let provider = GithubProvider {
            account: account(login),
            runner: Runner {
                gh: executable,
                // Fixture process startup can contend with parallel native builds.
                timeout: Duration::from_secs(30),
                ..Runner::default()
            },
        };
        (dir, provider)
    }
    fn exhausted(dir: &TempDir, count: usize) {
        assert_eq!(
            fs::read_to_string(dir.path().join("count"))
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            count
        );
    }

    #[test]
    fn checkout_source_preserves_fork_identity_and_case() {
        let mut response = pull(7, 1);
        response["head"]["repo"] = json!({"owner": {"login": "ForkOwner"}, "name": "ForkRepo"});
        response["head"]["ref"] = json!("Feature/ExactCase");
        let (dir, provider) = fixture(
            "second-account",
            vec![step("repos/owner/repo/pulls/7", response)],
        );
        let repo = repo("second-account");
        let source = provider.checkout_source(&repo, 7).unwrap();
        assert_eq!(source.base_repository, repo);
        assert_eq!(source.observed_revision, revision());
        assert_eq!(source.source_branch, "Feature/ExactCase");
        let fork = source.source_repository.unwrap();
        assert_eq!(fork.full_name(), "ForkOwner/ForkRepo");
        assert_eq!(fork.account, account("second-account"));
        assert!(fork.local_path.is_none());
        exhausted(&dir, 1);
    }

    #[test]
    fn checkout_source_distinguishes_same_repo_and_missing_head_repo() {
        for same_repo in [true, false] {
            let mut response = pull(7, 1);
            if same_repo {
                response["head"]["repo"] = response["base"]["repo"].clone();
            }
            let (dir, provider) =
                fixture("alice", vec![step("repos/owner/repo/pulls/7", response)]);
            let source = provider.checkout_source(&repo("alice"), 7).unwrap();
            assert_eq!(source.source_repository.is_some(), same_repo);
            if let Some(head) = source.source_repository {
                assert_eq!(head.full_name(), "owner/repo");
            }
            assert_eq!(source.observed_revision, revision());
            exhausted(&dir, 1);
        }
    }

    #[test]
    fn checkout_source_rejects_invalid_coordinates_and_branch_payloads() {
        for (pointer, value) in [
            ("/head/repo/owner/login", json!("../escape")),
            ("/head/ref", json!("bad\nbranch")),
            ("/head/ref", json!("x".repeat(1025))),
            ("/base/repo/name", json!("different")),
            ("/head/sha", json!("not-an-oid")),
            ("/number", json!(8)),
        ] {
            let mut response = pull(7, 1);
            response["head"]["repo"] = response["base"]["repo"].clone();
            *response.pointer_mut(pointer).unwrap() = value;
            let (dir, provider) =
                fixture("alice", vec![step("repos/owner/repo/pulls/7", response)]);
            assert!(provider.checkout_source(&repo("alice"), 7).is_err());
            exhausted(&dir, 1);
        }
        let (_dir, provider) = fixture("alice", vec![]);
        assert!(provider.checkout_source(&repo("bob"), 7).is_err());
    }

    #[test]
    fn validates_repository_inputs_and_accounts_before_commands() {
        for input in [
            "owner/repo",
            "https://github.com/owner/repo.git/",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo",
        ] {
            assert_eq!(
                parse_repository(input).unwrap(),
                ("owner".into(), "repo".into())
            );
        }
        assert!(parse_repository("owner/.github").is_ok());
        for input in [
            "https://evil.test/owner/repo",
            "https://github.com@evil.test/owner/repo",
            "https://github.com/owner/repo?token=x",
            "owner/repo/pull/1",
            "owner/../repo",
            "owner/repo;touch x",
            "owner/repo%2fother",
            "--hostname/evil",
        ] {
            assert!(parse_repository(input).is_err(), "accepted {input}");
        }
        let (_dir, provider) = fixture("alice", vec![]);
        assert!(
            provider
                .pull_request(&repo("bob"), 1)
                .unwrap_err()
                .to_string()
                .contains("mismatch")
        );
        assert!(
            provider
                .list_pull_requests(&repo("alice"), "open&state=all")
                .is_err()
        );
        let mut wrong_host = repo("alice");
        wrong_host.host = "evil.test".into();
        assert!(provider.pull_request(&wrong_host, 1).is_err());
        assert!(
            validate_account(&Account {
                host: "enterprise.test".into(),
                login: "alice".into()
            })
            .is_err()
        );
        assert!(validate_sha("main").is_err());
    }

    #[test]
    fn credentials_are_child_only_and_account_specific() {
        let steps = vec![
            step("repos/owner/repo/pulls/1", pull(1, 1)),
            graphql_step(metadata(&[1]), &[1]),
        ];
        let (alice_dir, alice) = fixture("alice", steps.clone());
        let (bob_dir, bob) = fixture("bob", steps);
        let command = alice.runner.gh_command();
        let env: HashMap<_, _> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for key in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "GITHUB_ENTERPRISE_TOKEN",
            "GH_DEBUG",
            "DEBUG",
            "GH_HOST",
            "GH_REPO",
        ] {
            assert_eq!(env.get(key), Some(&None));
        }
        thread::scope(|scope| {
            scope.spawn(|| alice.pull_request(&repo("alice"), 1).unwrap());
            scope.spawn(|| bob.pull_request(&repo("bob"), 1).unwrap());
        });
        exhausted(&alice_dir, 2);
        exhausted(&bob_dir, 2);
        for dir in [&alice_dir, &bob_dir] {
            assert_eq!(fs::read_to_string(dir.path().join("tokens")).unwrap(), "2");
        }
    }

    #[test]
    fn accounts_include_inactive_and_omit_broken() {
        let status = json!({"hosts": {"github.com": [
            {"host": HOST, "login": "bob", "state": "success", "active": false},
            {"host": HOST, "login": "alice", "state": "success", "active": true},
            {"host": HOST, "login": "broken", "state": "error"}]}});
        let (dir, provider) = fixture("alice", vec![step("", status)]);
        assert_eq!(
            discover_accounts(&provider.runner).unwrap(),
            vec![account("alice"), account("bob")]
        );
        exhausted(&dir, 1);
        let (_dir, provider) = fixture(
            "alice",
            vec![step(
                "",
                json!({"hosts": {HOST: [{"host": HOST, "login": "broken", "state": "error"}]}}),
            )],
        );
        assert!(discover_accounts(&provider.runner).is_err());
    }

    #[test]
    fn lists_paginated_prs_and_metadata() {
        let first: Vec<_> = (1..=100).map(|n| pull(n, 1)).collect();
        let mut merged = pull(101, 2);
        merged["state"] = json!("closed");
        merged["merged_at"] = json!("2026-09-12T12:00:00Z");
        let (dir, provider) = fixture(
            "alice",
            vec![
                step(
                    "repos/owner/repo/pulls?state=all&sort=created&direction=asc&per_page=100&page=1",
                    json!(first),
                ),
                graphql_step(
                    metadata(&(1..=100).collect::<Vec<_>>()),
                    &(1..=100).collect::<Vec<_>>(),
                ),
                step(
                    "repos/owner/repo/pulls?state=all&sort=created&direction=asc&per_page=100&page=2",
                    json!([merged]),
                ),
                graphql_step(metadata(&[101]), &[101]),
            ],
        );
        let pulls = provider.list_pull_requests(&repo("alice"), "ALL").unwrap();
        assert_eq!(pulls.len(), 101);
        assert_eq!(pulls[100].state, "MERGED");
        assert_eq!(pulls[0].body, "");
        assert_eq!(pulls[0].source_branch, "feature");
        assert_eq!(pulls[0].reviewers, ["reviewer", "team:maintainers"]);
        assert_eq!(pulls[0].review_status, "Approved");
        assert_eq!(pulls[0].check_status, "Passing");
        assert_eq!(
            pulls[0].participants,
            [
                "assignee",
                "author",
                "commenter",
                "review-author",
                "reviewer"
            ]
        );
        assert!(pulls[0].participants_complete);
        exhausted(&dir, 4);
    }

    #[test]
    fn nullable_and_partial_metadata_never_claims_false_status_or_completeness() {
        let partial = json!({
            "data": {"repository": {
                "nameWithOwner": "owner/repo",
                "pr0": {
                    "number": 1,
                    "url": "https://github.com/owner/repo/pull/1",
                    "reviewDecision": null,
                    "statusCheckRollup": null,
                    "comments": {"nodes": [{"author": {"login": "COMMENTER"}}], "pageInfo": {"hasNextPage": true, "endCursor": "more"}},
                    "reviews": {"nodes": [{"author": {"login": "pending-user"}, "state": "PENDING"}, null], "pageInfo": {"hasNextPage": false, "endCursor": null}},
                    "reviewRequests": {"nodes": [{"requestedReviewer": {"login": "reviewer"}}], "pageInfo": {"hasNextPage": false, "endCursor": null}},
                    "assignees": {"nodes": [{"login": "assignee"}], "pageInfo": {"hasNextPage": false, "endCursor": null}}
                }
            }},
            "errors": [{"message": "withheld fixture detail"}]
        });
        let (dir, provider) = fixture(
            "alice",
            vec![
                step("repos/owner/repo/pulls/1", pull(1, 1)),
                graphql_step(partial, &[1]),
            ],
        );
        let hydrated = provider.pull_request(&repo("alice"), 1).unwrap();
        assert_eq!(hydrated.review_status, "UNKNOWN");
        assert_eq!(hydrated.check_status, "UNKNOWN");
        assert!(!hydrated.participants_complete);
        assert!(hydrated.participants.contains(&"COMMENTER".into()));
        assert!(!hydrated.participants.contains(&"pending-user".into()));
        assert!(
            hydrated
                .participants_notice
                .unwrap()
                .contains("limited to 100")
        );
        exhausted(&dir, 2);

        let no_rules = json!({"data": {"repository": {
            "nameWithOwner": "owner/repo",
            "pr0": {
                "number": 1, "url": "https://github.com/owner/repo/pull/1",
                "reviewDecision": null, "statusCheckRollup": null,
                "comments": {"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}},
                "reviews": {"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}},
                "reviewRequests": {"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}},
                "assignees": {"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}}
            }
        }}});
        let (_dir, provider) = fixture(
            "alice",
            vec![
                step("repos/owner/repo/pulls/1", pull(1, 1)),
                graphql_step(no_rules, &[1]),
            ],
        );
        let hydrated = provider.pull_request(&repo("alice"), 1).unwrap();
        assert_eq!(hydrated.review_status, "None");
        assert_eq!(hydrated.check_status, "None");
        assert!(hydrated.participants_complete);
    }

    #[test]
    fn pagination_failure_does_not_return_successful_prefix() {
        let first: Vec<_> = (1..=100).map(|n| pull(n, 1)).collect();
        for response in [
            json!({"fail": true}),
            json!({"raw": "not json"}),
            json!({"response": [pull(1, 1)]}),
        ] {
            let mut second = response;
            second["endpoint"] = json!(
                "repos/owner/repo/pulls?state=open&sort=created&direction=asc&per_page=100&page=2"
            );
            let (_dir, provider) = fixture(
                "alice",
                vec![
                    step(
                        "repos/owner/repo/pulls?state=open&sort=created&direction=asc&per_page=100&page=1",
                        json!(first),
                    ),
                    graphql_step(
                        metadata(&(1..=100).collect::<Vec<_>>()),
                        &(1..=100).collect::<Vec<_>>(),
                    ),
                    second,
                ],
            );
            let err = provider
                .list_pull_requests(&repo("alice"), "open")
                .unwrap_err()
                .to_string();
            assert!(!err.contains("fixture-private"));
        }
    }

    fn details_overview() -> Value {
        json!({
            "id": "PR1",
            "number": 1,
            "headRefOid": "b".repeat(40),
            "repository": {"id": "R-base", "nameWithOwner": "owner/repo"},
            "headRepository": {"id": "R-base", "nameWithOwner": "owner/repo"},
            "potentialMergeCommit": null,
            "url": "https://github.com/owner/repo/pull/1",
            "body": "Overview body",
            "state": "OPEN",
            "isDraft": false,
            "maintainerCanModify": true,
            "canBeRebased": true,
            "viewerCanUpdateBranch": false,
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "reviewDecision": "APPROVED",
            "autoMergeRequest": null,
            "isInMergeQueue": false,
            "statusCheckRollup": null,
            "viewerCanReact": true,
            "reactionGroups": [],
            "reviewRequests": {"nodes": [{"requestedReviewer": {"login": "reviewer"}}, {"requestedReviewer": {"slug": "maintainers"}}], "pageInfo": {"hasNextPage": false, "endCursor": null}},
            "assignees": {"nodes": [{"login": "assignee"}], "pageInfo": {"hasNextPage": false, "endCursor": null}},
            "labels": {"nodes": [{"name": "bug"}], "pageInfo": {"hasNextPage": false, "endCursor": null}}
        })
    }

    fn details_response(pull: Value) -> Value {
        json!({
            "data": {
                "viewer": {"id": "U-alice", "login": "alice"},
                "repository": {
                    "id": "R-base",
                    "nameWithOwner": "owner/repo",
                    "viewerCanAdminister": false,
                    "pullRequest": pull
                }
            }
        })
    }

    fn repository_identity(id: &str, name_with_owner: &str) -> Value {
        json!({"id": id, "nameWithOwner": name_with_owner})
    }

    fn complete_check_run(
        check_id: &str,
        commit_sha: &str,
        repository: Value,
        workflow_run: Value,
    ) -> Value {
        json!({
            "__typename": "CheckRun",
            "id": check_id,
            "databaseId": 101,
            "name": "CI / build",
            "status": "COMPLETED",
            "conclusion": "SUCCESS",
            "permalink": "https://github.com/owner/repo/runs/101",
            "detailsUrl": "https://integrator.example/build?id=101",
            "startedAt": "2026-09-14T10:00:00Z",
            "completedAt": "2026-09-14T10:01:00Z",
            "isRequired": true,
            "repository": repository.clone(),
            "checkSuite": {
                "id": "SUITE-1",
                "databaseId": 202,
                "repository": repository.clone(),
                "commit": {"oid": commit_sha, "repository": repository},
                "app": {"id": "APP-1", "name": "Builder", "slug": "builder"},
                "workflowRun": workflow_run
            }
        })
    }

    fn complete_workflow_run() -> Value {
        json!({
            "id": "RUN-node",
            "databaseId": 303,
            "runAttempt": 2,
            "runNumber": 44,
            "event": "pull_request",
            "url": "https://github.com/owner/repo/actions/runs/303",
            "workflow": {"id": "WORKFLOW-node", "databaseId": 404, "name": "CI"}
        })
    }

    fn install_rollup(pull: &mut Value, commit_sha: &str, repository: Value, nodes: Value) {
        pull["statusCheckRollup"] = json!({
            "state": "SUCCESS",
            "commit": {"oid": commit_sha, "repository": repository},
            "contexts": {
                "nodes": nodes,
                "pageInfo": {"hasNextPage": false, "endCursor": null}
            }
        });
    }

    #[test]
    fn checks_identity_query_uses_current_exact_schema_fields_without_logs_or_actions() {
        for field in [
            "id databaseId name status conclusion permalink detailsUrl",
            "repository { id nameWithOwner }",
            "commit { oid repository { id nameWithOwner } }",
            "app { id name slug }",
            "id databaseId runAttempt runNumber event url",
            "workflow { id databaseId name }",
        ] {
            assert!(
                DETAILS_QUERY.contains(field),
                "missing query field: {field}"
            );
        }
        for excluded in ["annotations", "steps", "jobs", "mutation"] {
            assert!(
                !DETAILS_QUERY.contains(excluded),
                "unexpected field: {excluded}"
            );
        }
    }

    #[test]
    fn checks_identity_complete_actions_preserves_exact_identity_and_separate_urls() {
        let head = "b".repeat(40);
        let base = repository_identity("R-base", "owner/repo");
        let mut pull = details_overview();
        install_rollup(
            &mut pull,
            &head,
            base.clone(),
            json!([complete_check_run(
                "CHECK-node",
                &head,
                base,
                complete_workflow_run(),
            )]),
        );
        let (dir, provider) = fixture(
            "alice",
            vec![details_step(details_response(pull), json!({"number": 1}))],
        );
        let details = provider.details(&repo("alice"), 1).unwrap();
        assert!(details.checks_complete);
        assert_eq!(details.observed_head_sha.as_deref(), Some(head.as_str()));
        assert_eq!(details.rollup_commit_sha.as_deref(), Some(head.as_str()));
        assert_eq!(details.pull_request_node_id.as_deref(), Some("PR1"));
        assert_eq!(details.base_repository.as_ref().unwrap().node_id, "R-base");
        let check = &details.checks[0];
        assert_eq!(check.database_id, Some(101));
        assert_eq!(check.sha_class, CheckShaClass::Head);
        assert_eq!(
            check.github_permalink.as_deref(),
            Some("https://github.com/owner/repo/runs/101")
        );
        assert_eq!(
            check.details_url.as_deref(),
            Some("https://integrator.example/build?id=101")
        );
        let suite = check.suite.as_ref().unwrap();
        assert_eq!(suite.database_id, Some(202));
        assert_eq!(suite.app.as_ref().unwrap().slug, "builder");
        let ActionsLinkage::Linked(run) = &check.actions_linkage else {
            panic!("complete workflow relation must be linked")
        };
        assert_eq!(
            (run.database_id, run.run_attempt, run.run_number),
            (303, 2, 44)
        );
        assert_eq!(run.workflow_database_id, 404);
        assert_eq!(run.workflow_name, "CI");
        exhausted(&dir, 1);
    }

    #[test]
    fn checks_identity_null_unlinked_and_spoofed_slug_never_classify_as_actions() {
        let head = "b".repeat(40);
        let base = repository_identity("R-base", "owner/repo");
        let mut pull = details_overview();
        let mut check = complete_check_run("CHECK-unlinked", &head, base.clone(), Value::Null);
        check["name"] = json!("GitHub Actions / deploy");
        check["checkSuite"]["app"]["slug"] = json!("github-actions");
        install_rollup(&mut pull, &head, base, json!([check]));
        let (dir, provider) = fixture(
            "alice",
            vec![details_step(details_response(pull), json!({"number": 1}))],
        );
        let details = provider.details(&repo("alice"), 1).unwrap();
        assert!(details.checks_complete);
        assert_eq!(details.checks[0].kind, CheckKind::CheckRun);
        assert_eq!(
            details.checks[0].actions_linkage,
            ActionsLinkage::NoObservedLink
        );
        exhausted(&dir, 1);
    }

    #[test]
    fn checks_identity_partial_app_does_not_erase_independent_workflow_evidence() {
        for (case, workflow_run) in [
            ("null-workflow", Value::Null),
            ("linked-workflow", complete_workflow_run()),
        ] {
            let head = "b".repeat(40);
            let base = repository_identity("R-base", "owner/repo");
            let mut pull = details_overview();
            let mut check =
                complete_check_run("CHECK-partial-app", &head, base.clone(), workflow_run);
            check["checkSuite"]["app"] = json!({"id": "APP-1", "name": "Builder"});
            install_rollup(&mut pull, &head, base, json!([check]));
            let (dir, provider) = fixture(
                "alice",
                vec![details_step(details_response(pull), json!({"number": 1}))],
            );
            let details = provider.details(&repo("alice"), 1).unwrap();
            assert!(!details.checks_complete, "{case}");
            assert!(details.checks[0].suite.as_ref().unwrap().app.is_none());
            match case {
                "null-workflow" => assert_eq!(
                    details.checks[0].actions_linkage,
                    ActionsLinkage::NoObservedLink
                ),
                "linked-workflow" => assert!(matches!(
                    details.checks[0].actions_linkage,
                    ActionsLinkage::Linked(_)
                )),
                _ => unreachable!(),
            }
            exhausted(&dir, 1);
        }
    }

    #[test]
    fn checks_identity_status_context_named_ci_remains_commit_status() {
        let head = "b".repeat(40);
        let base = repository_identity("R-base", "owner/repo");
        let mut pull = details_overview();
        install_rollup(
            &mut pull,
            &head,
            base.clone(),
            json!([{
                "__typename": "StatusContext", "id": "STATUS-node", "context": "GitHub Actions CI",
                "state": "SUCCESS", "description": "complete", "targetUrl": "https://ci.example/status",
                "createdAt": "2026-09-14T10:00:00Z", "updatedAt": "2026-09-14T10:01:00Z",
                "isRequired": false, "commit": {"oid": head, "repository": base}
            }]),
        );
        let (dir, provider) = fixture(
            "alice",
            vec![details_step(details_response(pull), json!({"number": 1}))],
        );
        let details = provider.details(&repo("alice"), 1).unwrap();
        let check = &details.checks[0];
        assert_eq!(check.kind, CheckKind::CommitStatus);
        assert!(check.database_id.is_none() && check.suite.is_none());
        assert_eq!(check.actions_linkage, ActionsLinkage::NoObservedLink);
        exhausted(&dir, 1);
    }

    #[test]
    fn checks_identity_null_status_context_commit_is_partial_unknown_sha() {
        let head = "b".repeat(40);
        let base = repository_identity("R-base", "owner/repo");
        let mut pull = details_overview();
        install_rollup(
            &mut pull,
            &head,
            base,
            json!([{
                "__typename": "StatusContext", "id": "STATUS-null-commit", "context": "external/ci",
                "state": "SUCCESS", "description": "complete", "targetUrl": "https://ci.example/status",
                "createdAt": "2026-09-14T10:00:00Z", "updatedAt": "2026-09-14T10:01:00Z",
                "isRequired": true, "commit": null
            }]),
        );
        let (dir, provider) = fixture(
            "alice",
            vec![details_step(details_response(pull), json!({"number": 1}))],
        );
        let details = provider.details(&repo("alice"), 1).unwrap();
        assert!(!details.checks_complete);
        assert_eq!(details.checks.len(), 1);
        assert!(details.checks[0].commit_sha.is_none());
        assert_eq!(details.checks[0].sha_class, CheckShaClass::Unknown);
        exhausted(&dir, 1);
    }

    #[test]
    fn checks_identity_merge_candidate_sha_is_not_presented_as_pr_head() {
        let merge = "c".repeat(40);
        let base = repository_identity("R-base", "owner/repo");
        let mut pull = details_overview();
        pull["potentialMergeCommit"] = json!({"oid": merge, "repository": base.clone()});
        install_rollup(
            &mut pull,
            &"b".repeat(40),
            base.clone(),
            json!([complete_check_run("CHECK-merge", &merge, base, Value::Null,)]),
        );
        let (dir, provider) = fixture(
            "alice",
            vec![details_step(details_response(pull), json!({"number": 1}))],
        );
        let details = provider.details(&repo("alice"), 1).unwrap();
        assert_eq!(details.checks[0].sha_class, CheckShaClass::MergeCandidate);
        assert_eq!(
            details
                .potential_merge_commit_repository
                .as_ref()
                .unwrap()
                .node_id,
            "R-base"
        );
        exhausted(&dir, 1);
    }

    #[test]
    fn checks_identity_exact_fork_origin_accepted_but_foreign_origin_rejected() {
        let head = "b".repeat(40);
        let fork = repository_identity("R-fork", "contributor/fork");
        let mut pull = details_overview();
        pull["headRepository"] = fork.clone();
        install_rollup(
            &mut pull,
            &head,
            fork.clone(),
            json!([complete_check_run("CHECK-fork", &head, fork, Value::Null)]),
        );
        let (dir, provider) = fixture(
            "alice",
            vec![details_step(details_response(pull), json!({"number": 1}))],
        );
        let details = provider.details(&repo("alice"), 1).unwrap();
        assert_eq!(
            details.checks[0]
                .commit_repository
                .as_ref()
                .unwrap()
                .name_with_owner,
            "contributor/fork"
        );
        exhausted(&dir, 1);

        let foreign = repository_identity("R-foreign", "mallory/other");
        let mut pull = details_overview();
        install_rollup(
            &mut pull,
            &head,
            repository_identity("R-base", "owner/repo"),
            json!([complete_check_run(
                "CHECK-foreign",
                &head,
                foreign,
                Value::Null,
            )]),
        );
        let (dir, provider) = fixture(
            "alice",
            vec![details_step(details_response(pull), json!({"number": 1}))],
        );
        assert!(provider.details(&repo("alice"), 1).is_err());
        exhausted(&dir, 1);
    }

    #[test]
    fn checks_identity_omitted_partial_and_out_of_range_fields_remain_unknown() {
        for defect in [
            "workflow-omitted",
            "workflow-db-null",
            "check-db-wide",
            "required-omitted",
        ] {
            let head = "b".repeat(40);
            let base = repository_identity("R-base", "owner/repo");
            let mut pull = details_overview();
            let mut check = complete_check_run(
                "CHECK-partial",
                &head,
                base.clone(),
                complete_workflow_run(),
            );
            match defect {
                "workflow-omitted" => {
                    check["checkSuite"]
                        .as_object_mut()
                        .unwrap()
                        .remove("workflowRun");
                }
                "workflow-db-null" => {
                    check["checkSuite"]["workflowRun"]["databaseId"] = Value::Null
                }
                "check-db-wide" => check["databaseId"] = json!(i64::from(i32::MAX) + 1),
                "required-omitted" => {
                    check.as_object_mut().unwrap().remove("isRequired");
                }
                _ => unreachable!(),
            }
            install_rollup(&mut pull, &head, base, json!([check]));
            let (dir, provider) = fixture(
                "alice",
                vec![details_step(details_response(pull), json!({"number": 1}))],
            );
            let details = provider.details(&repo("alice"), 1).unwrap();
            assert!(!details.checks_complete, "{defect}");
            match defect {
                "workflow-omitted" | "workflow-db-null" => {
                    assert_eq!(details.checks[0].actions_linkage, ActionsLinkage::Unknown)
                }
                "check-db-wide" => assert_eq!(details.checks[0].database_id, None),
                "required-omitted" => assert_eq!(details.checks[0].required, None),
                _ => unreachable!(),
            }
            exhausted(&dir, 1);
        }
    }

    #[test]
    fn checks_identity_null_rollup_differs_from_missing_or_null_page_data() {
        for defect in [
            "null-rollup",
            "missing-rollup",
            "null-rollup-commit",
            "null-contexts",
            "null-node",
            "unknown-node",
            "empty-terminal",
        ] {
            let mut pull = details_overview();
            match defect {
                "null-rollup" => pull["statusCheckRollup"] = Value::Null,
                "missing-rollup" => {
                    pull.as_object_mut().unwrap().remove("statusCheckRollup");
                }
                "null-rollup-commit" => {
                    install_rollup(
                        &mut pull,
                        &"b".repeat(40),
                        repository_identity("R-base", "owner/repo"),
                        json!([]),
                    );
                    pull["statusCheckRollup"]["commit"] = Value::Null;
                }
                "null-contexts" => {
                    pull["statusCheckRollup"] = json!({
                        "state": "SUCCESS",
                        "commit": {"oid": "b".repeat(40), "repository": repository_identity("R-base", "owner/repo")},
                        "contexts": null
                    });
                }
                "null-node" => install_rollup(
                    &mut pull,
                    &"b".repeat(40),
                    repository_identity("R-base", "owner/repo"),
                    json!([null]),
                ),
                "unknown-node" => install_rollup(
                    &mut pull,
                    &"b".repeat(40),
                    repository_identity("R-base", "owner/repo"),
                    json!([{"__typename": "FutureCheckContext", "id": "FUTURE-1"}]),
                ),
                "empty-terminal" => install_rollup(
                    &mut pull,
                    &"b".repeat(40),
                    repository_identity("R-base", "owner/repo"),
                    json!([]),
                ),
                _ => unreachable!(),
            }
            let (dir, provider) = fixture(
                "alice",
                vec![details_step(details_response(pull), json!({"number": 1}))],
            );
            let details = provider.details(&repo("alice"), 1).unwrap();
            let expected_complete = matches!(defect, "null-rollup" | "empty-terminal");
            assert_eq!(details.checks_complete, expected_complete, "{defect}");
            assert!(details.checks.is_empty());
            exhausted(&dir, 1);
        }
    }

    #[test]
    fn checks_identity_moving_rollup_or_merge_candidate_rejects_the_prefix() {
        for moving_field in ["rollup-sha", "rollup-repository", "merge-candidate"] {
            let base = repository_identity("R-base", "owner/repo");
            let mut first = details_overview();
            first["potentialMergeCommit"] =
                json!({"oid": "c".repeat(40), "repository": base.clone()});
            install_rollup(&mut first, &"b".repeat(40), base.clone(), json!([]));
            first["statusCheckRollup"]["contexts"]["pageInfo"] =
                json!({"hasNextPage": true, "endCursor": "checks-next"});

            let mut second = first.clone();
            second["statusCheckRollup"]["contexts"]["pageInfo"] =
                json!({"hasNextPage": false, "endCursor": null});
            match moving_field {
                "rollup-sha" => {
                    second["statusCheckRollup"]["commit"]["oid"] = json!("d".repeat(40));
                }
                "rollup-repository" => {
                    let fork = repository_identity("R-fork", "contributor/fork");
                    second["headRepository"] = fork.clone();
                    second["statusCheckRollup"]["commit"]["repository"] = fork;
                }
                "merge-candidate" => {
                    second["potentialMergeCommit"]["oid"] = json!("e".repeat(40));
                }
                _ => unreachable!(),
            }
            let (dir, provider) = fixture(
                "alice",
                vec![
                    details_step(
                        details_response(first),
                        json!({"checksCursor": null, "includeChecks": true}),
                    ),
                    details_step(
                        details_response(second),
                        json!({"checksCursor": "checks-next", "includeChecks": true}),
                    ),
                ],
            );
            let error = provider.details(&repo("alice"), 1).unwrap_err();
            assert!(
                error.to_string().contains("source identity changed"),
                "{moving_field}: {error:#}"
            );
            exhausted(&dir, 2);
        }
    }

    #[test]
    fn checks_identity_completed_cursor_does_not_penalize_omitted_later_contexts() {
        let base = repository_identity("R-base", "owner/repo");
        let mut first = details_overview();
        first["comments"] = json!({
            "nodes": [],
            "pageInfo": {"hasNextPage": true, "endCursor": "comments-next"}
        });
        install_rollup(&mut first, &"b".repeat(40), base.clone(), json!([]));

        let mut second = details_overview();
        second["comments"] = json!({
            "nodes": [],
            "pageInfo": {"hasNextPage": false, "endCursor": null}
        });
        second["statusCheckRollup"] = json!({
            "state": "SUCCESS",
            "commit": {"oid": "b".repeat(40), "repository": base}
        });
        let (dir, provider) = fixture(
            "alice",
            vec![
                details_step(
                    details_response(first),
                    json!({"commentsCursor": null, "includeComments": true, "includeChecks": true}),
                ),
                details_step(
                    details_response(second),
                    json!({"commentsCursor": "comments-next", "includeComments": true, "includeChecks": false}),
                ),
            ],
        );
        let details = provider.details(&repo("alice"), 1).unwrap();
        assert!(details.checks_complete);
        assert!(details.checks.is_empty());
        exhausted(&dir, 2);
    }

    #[test]
    fn checks_identity_selected_viewer_mismatch_rejects_before_admission() {
        let mut response = details_response(details_overview());
        response["data"]["viewer"]["login"] = json!("mallory");
        let (dir, provider) = fixture("alice", vec![details_step(response, json!({"number": 1}))]);
        assert!(provider.details(&repo("alice"), 1).is_err());
        exhausted(&dir, 1);
    }

    #[test]
    fn missing_review_comment_reactions_preserve_text_without_fresh_authority() {
        for field in ["viewerCanReact", "reactionGroups"] {
            for explicit_null in [false, true] {
                let mut comment = json!({
                    "id": "RC1", "author": {"login": "bob"}, "body": "retained comment",
                    "createdAt": "a", "updatedAt": "b", "url": "u", "path": "src/lib.rs",
                    "subjectType": "FILE", "line": null, "originalLine": null,
                    "startLine": null, "originalStartLine": null, "diffHunk": "",
                    "outdated": false, "commit": null, "originalCommit": null,
                    "pullRequestReview": {"id": "R1"},
                    "viewerCanReact": true, "reactionGroups": []
                });
                if explicit_null {
                    comment[field] = Value::Null;
                } else {
                    comment.as_object_mut().unwrap().remove(field);
                }
                let mut pull = details_overview();
                pull["reviewThreads"] = json!({
                    "nodes": [{
                        "id": "T1", "path": "src/lib.rs", "subjectType": "FILE",
                        "line": null, "originalLine": null, "startLine": null,
                        "originalStartLine": null, "diffSide": null, "startDiffSide": null,
                        "isResolved": false, "isOutdated": false,
                        "comments": {"nodes": [comment],
                            "pageInfo": {"hasNextPage": false, "endCursor": null}}
                    }],
                    "pageInfo": {"hasNextPage": false, "endCursor": null}
                });
                let (dir, provider) = fixture(
                    "alice",
                    vec![details_step(details_response(pull), json!({"number": 1}))],
                );
                let details = provider.details(&repo("alice"), 1).unwrap();
                assert_eq!(
                    details.review_threads[0].comments[0].body,
                    "retained comment"
                );
                let reaction = details
                    .reactions
                    .iter()
                    .find(|snapshot| snapshot.kind == ReactableKind::PullRequestReviewComment)
                    .unwrap();
                assert!(
                    !reaction.reactions.complete,
                    "{field}, null={explicit_null}"
                );
                assert!(reaction.reactions.groups.is_empty());
                assert!(reaction.fresh_capability.is_none());
                exhausted(&dir, 1);
            }
        }
    }

    #[test]
    fn details_rejects_check_pages_from_different_heads() {
        let mut first = details_overview();
        first["statusCheckRollup"] = json!({
            "state": "SUCCESS", "contexts": { "nodes": [],
                "pageInfo": { "hasNextPage": true, "endCursor": "old-head-checks" } }
        });
        let mut second = details_overview();
        second["headRefOid"] = "c".repeat(40).into();
        second["statusCheckRollup"] = json!({
            "state": "FAILURE", "contexts": { "nodes": [],
                "pageInfo": { "hasNextPage": false, "endCursor": null } }
        });
        let (dir, provider) = fixture(
            "alice",
            vec![
                details_step(details_response(first), json!({"checksCursor": null})),
                details_step(
                    details_response(second),
                    json!({"checksCursor": "old-head-checks"}),
                ),
            ],
        );
        let error = provider.details(&repo("alice"), 1).unwrap_err();
        assert!(
            error.to_string().contains("source identity changed"),
            "unexpected moving-head rejection: {error:#}"
        );
        exhausted(&dir, 2);
    }

    #[test]
    fn combined_details_preserve_check_identity_and_dismissal_authority() {
        for (admin, partial) in [(true, false), (false, false), (true, true)] {
            let head = "b".repeat(40);
            let base = repository_identity("R-base", "owner/repo");
            let mut pull = details_overview();
            install_rollup(
                &mut pull,
                &head,
                base.clone(),
                json!([complete_check_run(
                    "CHECK-combined",
                    &head,
                    base,
                    complete_workflow_run(),
                )]),
            );
            pull["reviews"] = json!({
                "nodes": [{
                    "id": "REVIEW-combined", "author": null, "body": "approved",
                    "state": "APPROVED", "submittedAt": "2026-09-12T11:00:00Z",
                    "commit": {"oid": "a".repeat(40)},
                    "viewerDidAuthor": false, "viewerCanUpdate": false,
                    "viewerCannotUpdateReasons": ["NOT_AUTHOR"],
                    "viewerCanReact": true, "reactionGroups": [],
                    "url": "https://github.com/owner/repo/pull/1#pullrequestreview-combined"
                }],
                "pageInfo": {"hasNextPage": false, "endCursor": null}
            });
            let mut response = details_response(pull);
            response["data"]["repository"]["viewerCanAdminister"] = json!(admin);
            if partial {
                response["errors"] = json!([{"message": "some requested data was unavailable"}]);
            }
            let (dir, provider) =
                fixture("alice", vec![details_step(response, json!({"number": 1}))]);
            let details = provider.details(&repo("alice"), 1).unwrap();
            assert_eq!(details.base_repository.as_ref().unwrap().node_id, "R-base");
            assert_eq!(details.observed_head_sha.as_deref(), Some(head.as_str()));
            assert_eq!(details.checks[0].coordinates.remote_id, "CHECK-combined");
            assert_eq!(details.checks_complete, !partial);
            assert!(matches!(
                details.checks[0].actions_linkage,
                ActionsLinkage::Linked(_)
            ));
            let capability = details.reviews[0].dismissal_capability.as_ref();
            if partial {
                assert!(capability.is_none());
            } else {
                let capability =
                    capability.expect("fresh dismissal authority must survive integration");
                assert_eq!(capability.viewer.login, "alice");
                assert_eq!(capability.pull_request.remote_id, "PR1");
                assert_eq!(
                    matches!(capability.authority, DismissalAuthority::Available),
                    admin
                );
                assert!(capability.authority.permits_attempt());
            }
            exhausted(&dir, 1);
        }
    }

    #[test]
    fn complete_details_read_preserves_submitted_review_edit_capability() {
        let mut pull = details_overview();
        pull["reviews"] = json!({
            "nodes": [{
                "id": "R-owned", "author": {"login": "alice"}, "body": "before",
                "state": "COMMENTED", "submittedAt": "2026-09-12T11:00:00Z",
                "commit": {"oid": "a".repeat(40)},
                "viewerDidAuthor": true, "viewerCanUpdate": true,
                "viewerCannotUpdateReasons": [],
                "viewerCanReact": true, "reactionGroups": [],
                "url": "https://github.com/owner/repo/pull/1#pullrequestreview-owned"
            }],
            "pageInfo": {"hasNextPage": false, "endCursor": null}
        });
        let response = details_response(pull);
        let (dir, provider) = fixture("alice", vec![details_step(response, json!({"number": 1}))]);
        let details = provider.details(&repo("alice"), 1).unwrap();
        let capability = details.reviews[0]
            .edit_summary_capability
            .as_ref()
            .expect("complete fresh details must retain capability evidence");
        assert!(capability.viewer_did_author);
        assert!(capability.viewer_can_update);
        assert!(capability.viewer_cannot_update_reasons.is_empty());
        assert_eq!(details.reactions.len(), 2);
        assert!(details.reactions.iter().all(|reaction| {
            reaction.reactions.complete
                && reaction.reactions.groups.len() == ReactionContent::ALL.len()
                && reaction
                    .fresh_capability
                    .as_ref()
                    .is_some_and(|fresh| fresh.viewer.node_id == "U-alice")
        }));
        exhausted(&dir, 1);
    }

    #[test]
    fn complete_details_materializes_all_four_reactables_with_fresh_viewer_not_author() {
        let group = json!([{
            "content": "HEART",
            "viewerHasReacted": true,
            "users": {"totalCount": 4}
        }]);
        let mut pull = details_overview();
        pull["reactionGroups"] = group.clone();
        pull["comments"] = json!({
            "nodes": [{
                "id": "IC-bob", "author": {"login": "bob"}, "body": "discussion",
                "createdAt": "2026-09-12T10:00:00Z", "updatedAt": "2026-09-12T10:00:00Z",
                "url": "https://github.com/owner/repo/pull/1#issuecomment-bob",
                "viewerCanReact": true, "reactionGroups": group.clone()
            }],
            "pageInfo": {"hasNextPage": false, "endCursor": null}
        });
        pull["reviews"] = json!({
            "nodes": [{
                "id": "R-bob", "author": {"login": "bob"}, "body": "review",
                "state": "APPROVED", "submittedAt": "2026-09-12T11:00:00Z", "commit": null,
                "viewerDidAuthor": false, "viewerCanUpdate": false,
                "viewerCannotUpdateReasons": ["NOT_AUTHOR"],
                "viewerCanReact": true, "reactionGroups": group.clone(),
                "url": "https://github.com/owner/repo/pull/1#pullrequestreview-bob"
            }],
            "pageInfo": {"hasNextPage": false, "endCursor": null}
        });
        pull["reviewThreads"] = json!({
            "nodes": [{
                "id": "T-bob", "path": "src/lib.rs", "subjectType": "FILE",
                "line": null, "originalLine": null, "startLine": null,
                "originalStartLine": null, "diffSide": null, "startDiffSide": null,
                "isResolved": false, "isOutdated": false,
                "comments": {
                    "nodes": [{
                        "id": "RC-bob", "author": {"login": "bob"}, "body": "file note",
                        "createdAt": "2026-09-12T12:00:00Z", "updatedAt": "2026-09-12T12:00:00Z",
                        "url": "https://github.com/owner/repo/pull/1#discussion-bob",
                        "path": "src/lib.rs", "subjectType": "FILE", "line": null,
                        "originalLine": null, "startLine": null, "originalStartLine": null,
                        "diffHunk": "", "outdated": false, "commit": null,
                        "originalCommit": null, "pullRequestReview": {"id": "R-bob"},
                        "viewerCanReact": true, "reactionGroups": group.clone()
                    }],
                    "pageInfo": {"hasNextPage": false, "endCursor": null}
                }
            }],
            "pageInfo": {"hasNextPage": false, "endCursor": null}
        });
        let (dir, provider) = fixture(
            "alice",
            vec![details_step(details_response(pull), json!({"number": 1}))],
        );
        let details = provider.details(&repo("alice"), 1).unwrap();
        assert_eq!(details.reactions.len(), 4);
        assert_eq!(
            details
                .reactions
                .iter()
                .map(|reaction| reaction.kind)
                .collect::<HashSet<_>>(),
            HashSet::from([
                ReactableKind::PullRequest,
                ReactableKind::PullRequestReview,
                ReactableKind::IssueComment,
                ReactableKind::PullRequestReviewComment,
            ])
        );
        for reaction in &details.reactions {
            assert_eq!(reaction.reactions.groups.len(), ReactionContent::ALL.len());
            let heart = reaction
                .reactions
                .groups
                .iter()
                .find(|entry| entry.content == ReactionContent::Heart)
                .unwrap();
            assert_eq!((heart.count, heart.viewer_has_reacted), (4, true));
            let fresh = reaction.fresh_capability.as_ref().unwrap();
            assert_eq!(fresh.viewer.node_id, "U-alice");
            assert_eq!(fresh.viewer.login, "alice");
        }
        let comment = details
            .reactions
            .iter()
            .find(|reaction| reaction.kind == ReactableKind::PullRequestReviewComment)
            .unwrap();
        assert_eq!(comment.parent_review.as_ref().unwrap().remote_id, "R-bob");
        assert_eq!(details.issue_comments[0].author.as_deref(), Some("bob"));
        assert_eq!(details.reviews[0].author.as_deref(), Some("bob"));
        exhausted(&dir, 1);
    }

    #[test]
    fn details_pages_activity_and_checks_and_marks_partial_history() {
        let overview = details_overview();
        let mut first = overview.clone();
        first["comments"] = json!({
            "nodes": [{"id": "IC1", "author": {"login": "one"}, "body": "first", "createdAt": "2026-09-12T10:00:00Z", "updatedAt": "2026-09-12T10:00:00Z", "url": "https://github.com/owner/repo/pull/1#issuecomment-1", "viewerCanReact": true, "reactionGroups": []}],
            "pageInfo": {"hasNextPage": true, "endCursor": "comments-1"}
        });
        first["reviews"] = json!({
            "nodes": [{"id": "R1", "author": {"login": "reviewer"}, "body": "approved", "state": "APPROVED", "submittedAt": "2026-09-12T11:00:00Z", "commit": null, "viewerDidAuthor": false, "viewerCanUpdate": false, "viewerCannotUpdateReasons": ["NOT_AUTHOR"], "viewerCanReact": true, "reactionGroups": [], "url": "https://github.com/owner/repo/pull/1#pullrequestreview-1"}],
            "pageInfo": {"hasNextPage": false, "endCursor": null}
        });
        first["reviewThreads"] = json!({
            "nodes": [{
                "id": "T1", "path": "src/lib.rs", "line": 8, "originalLine": 7,
                "startLine": null, "originalStartLine": null, "diffSide": "RIGHT", "startDiffSide": null,
                "isResolved": true, "isOutdated": true,
                "comments": {"nodes": [{
                    "id": "RC1", "author": {"login": "reviewer"}, "body": "old line",
                    "createdAt": "2026-09-12T11:00:00Z", "updatedAt": "2026-09-12T11:01:00Z",
                    "url": "https://github.com/owner/repo/pull/1#discussion_r1", "path": "src/lib.rs",
                    "line": null, "originalLine": 7, "startLine": null, "originalStartLine": null,
                    "diffHunk": "@@ -7 +8 @@", "outdated": true, "commit": null, "originalCommit": null,
                    "pullRequestReview": {"id": "R1"}, "viewerCanReact": true, "reactionGroups": []
                }], "pageInfo": {"hasNextPage": true, "endCursor": "nested-more"}}
            }],
            "pageInfo": {"hasNextPage": false, "endCursor": null}
        });
        first["statusCheckRollup"] = json!({
            "state": "SUCCESS",
            "contexts": {"nodes": [{
                "__typename": "CheckRun", "id": "CR1", "name": "build", "status": "COMPLETED",
                "conclusion": "SUCCESS", "detailsUrl": "https://example.test/build", "startedAt": "2026-09-12T09:00:00Z",
                "completedAt": "2026-09-12T09:01:00Z", "isRequired": true
            }], "pageInfo": {"hasNextPage": true, "endCursor": "checks-1"}}
        });

        let mut second = overview;
        second["comments"] = json!({
            "nodes": [{"id": "IC2", "author": null, "body": "second", "createdAt": "2026-09-12T12:00:00Z", "updatedAt": "2026-09-12T12:00:00Z", "url": "https://github.com/owner/repo/pull/1#issuecomment-2", "viewerCanReact": true, "reactionGroups": []}],
            "pageInfo": {"hasNextPage": false, "endCursor": null}
        });
        second["statusCheckRollup"] = json!({
            "state": "SUCCESS",
            "contexts": {"nodes": [{
                "__typename": "StatusContext", "id": "SC1", "context": "deploy", "state": "SUCCESS",
                "description": "ready", "targetUrl": null, "createdAt": "2026-09-12T09:00:00Z",
                "updatedAt": "2026-09-12T09:02:00Z", "isRequired": false
            }], "pageInfo": {"hasNextPage": false, "endCursor": null}}
        });

        let mut first_response = details_response(first);
        first_response["errors"] = json!([{"message": "one field was inaccessible"}]);
        let second_response = details_response(second);
        let (dir, provider) = fixture(
            "alice",
            vec![
                details_step(
                    first_response,
                    json!({"number": 1, "commentsCursor": null, "checksCursor": null, "includeReviews": true, "includeThreads": true}),
                ),
                details_step(
                    second_response,
                    json!({"number": 1, "commentsCursor": "comments-1", "checksCursor": "checks-1", "includeReviews": false, "includeThreads": false}),
                ),
            ],
        );
        let details = provider.details(&repo("alice"), 1).unwrap();
        assert_eq!(details.body, "Overview body");
        assert_eq!(
            details.requested_reviewers,
            ["reviewer", "team:maintainers"]
        );
        assert_eq!(details.issue_comments.len(), 2);
        assert_eq!(details.reviews.len(), 1);
        assert_eq!(details.reviews[0].commit_sha, None);
        assert_eq!(details.reviews[0].edit_summary_capability, None);
        assert_eq!(details.review_threads.len(), 1);
        assert!(details.review_threads[0].resolved);
        assert!(details.review_threads[0].outdated);
        assert!(!details.review_threads[0].comments_complete);
        assert_eq!(details.review_threads[0].comments[0].commit_sha, None);
        assert_eq!(details.checks.len(), 2);
        assert_eq!(details.checks[0].coordinates.pull_request, 1);
        assert_eq!(details.merge_eligibility.check_status, "Passing");
        assert!(!details.activity_complete);
        assert!(!details.checks_complete);
        assert_eq!(details.reactions.len(), 5);
        assert!(
            details
                .reactions
                .iter()
                .all(|reaction| reaction.fresh_capability.is_none())
        );
        let notice = details.notice.unwrap();
        assert!(notice.contains("partial collaboration data"));
        assert!(notice.contains("100-comment per-thread limit"));
        exhausted(&dir, 2);
    }

    #[test]
    fn comparison_paginates_beyond_compare_cap() {
        let all: Vec<_> = (0..301).map(file).collect();
        let mut steps = vec![
            step("repos/owner/repo/pulls/1", pull(1, 301)),
            step(&compare_path(), compare(all[..300].to_vec())),
        ];
        for (index, page) in all.chunks(100).enumerate() {
            steps.push(step(
                &format!(
                    "repos/owner/repo/pulls/1/files?per_page=100&page={}",
                    index + 1
                ),
                json!(page),
            ));
        }
        steps.push(step("repos/owner/repo/pulls/1", pull(1, 301)));
        let (dir, provider) = fixture("alice", steps);
        let result = provider.comparison(&repo("alice"), 1, &revision()).unwrap();
        assert_eq!(result.files.len(), 301);
        assert!(result.complete);
        assert_eq!(result.revision, revision());
        exhausted(&dir, 7);
    }

    #[test]
    fn fixed_commit_inventory_paginates_and_rejects_moving_or_partial_pages() {
        let mut previous = BASE.to_owned();
        let mut first_nodes = Vec::new();
        for value in 3_u64..103 {
            let sha = format!("{value:040x}");
            first_nodes.push(inventory_commit(&sha, &previous, "page one"));
            previous = sha;
        }
        let middle = previous;
        let first = inventory_page(BASE, HEAD, first_nodes, 101, true, Some("next"));
        let second = inventory_page(
            BASE,
            HEAD,
            vec![inventory_commit(HEAD, &middle, "second")],
            101,
            false,
            None,
        );
        let (dir, provider) = fixture(
            "alice",
            vec![
                details_step(first.clone(), json!({"after": null, "number": 1})),
                details_step(second, json!({"after": "next", "number": 1})),
            ],
        );
        let result = provider
            .commit_inventory(&repo("alice"), 1, &revision())
            .unwrap();
        assert_eq!(result.availability, InventoryAvailability::Complete);
        assert_eq!(result.commits.len(), 101);
        assert_eq!(result.commits.last().unwrap().sha, HEAD);
        exhausted(&dir, 2);

        let moved = inventory_page(
            BASE,
            "4444444444444444444444444444444444444444",
            vec![inventory_commit(HEAD, &middle, "second")],
            101,
            false,
            None,
        );
        let (dir, provider) = fixture(
            "alice",
            vec![
                details_step(first.clone(), json!({"after": null})),
                details_step(moved, json!({"after": "next"})),
            ],
        );
        let result = provider
            .commit_inventory(&repo("alice"), 1, &revision())
            .unwrap();
        assert_eq!(result.availability, InventoryAvailability::Incomplete);
        assert!(result.commits.is_empty());
        assert!(result.notice.unwrap().contains("changed"));
        exhausted(&dir, 2);

        let mut truncated = first.clone();
        truncated["data"]["repository"]["pullRequest"]["commits"]["nodes"]
            .as_array_mut()
            .unwrap()
            .pop();
        let (dir, provider) = fixture(
            "alice",
            vec![details_step(truncated, json!({"after": null}))],
        );
        let result = provider
            .commit_inventory(&repo("alice"), 1, &revision())
            .unwrap();
        assert_eq!(result.availability, InventoryAvailability::Incomplete);
        assert!(result.commits.is_empty());
        assert!(result.notice.unwrap().contains("truncated"));
        exhausted(&dir, 1);

        let mut partial = first;
        partial["errors"] = json!([{"message": "truncated fixture"}]);
        let (dir, provider) = fixture("alice", vec![details_step(partial, json!({"after": null}))]);
        let result = provider
            .commit_inventory(&repo("alice"), 1, &revision())
            .unwrap();
        assert_eq!(result.availability, InventoryAvailability::Incomplete);
        assert!(result.commits.is_empty());
        assert!(result.notice.unwrap().contains("partial"));
        exhausted(&dir, 1);
    }

    #[test]
    fn historical_or_wrong_account_commit_inventory_is_unavailable_without_leakage() {
        let current = inventory_page(
            BASE,
            "4444444444444444444444444444444444444444",
            Vec::new(),
            0,
            false,
            None,
        );
        let (dir, provider) = fixture(
            "selected-account",
            vec![details_step(current, json!({"number": 1}))],
        );
        let inventory = provider
            .commit_inventory(&repo("selected-account"), 1, &revision())
            .unwrap();
        assert_eq!(inventory.availability, InventoryAvailability::Unavailable);
        assert!(inventory.commits.is_empty());
        exhausted(&dir, 1);

        let (_dir, provider) = fixture("selected-account", vec![]);
        assert!(
            provider
                .commit_inventory(&repo("other-account"), 1, &revision())
                .unwrap_err()
                .to_string()
                .contains("mismatch")
        );
    }

    #[test]
    fn direct_compare_proves_exact_head_from_last_page_and_discloses_file_cap() {
        let first_commits: Vec<_> = (3_u64..103)
            .map(|value| json!({"sha": format!("{value:040x}")}))
            .collect();
        let first = direct_compare(BASE, BASE, 101, first_commits, (0..300).map(file).collect());
        let last = direct_compare(BASE, BASE, 101, vec![json!({"sha": HEAD})], Vec::new());
        let (dir, provider) = fixture(
            "alice",
            vec![
                step(
                    &format!("repos/owner/repo/commits/{HEAD}"),
                    json!({"sha": HEAD}),
                ),
                step(
                    &format!("repos/owner/repo/compare/{BASE}...{HEAD}?per_page=100&page=1"),
                    first,
                ),
                step(
                    &format!("repos/owner/repo/compare/{BASE}...{HEAD}?per_page=100&page=2"),
                    last,
                ),
            ],
        );
        let result = provider
            .direct_comparison(&repo("alice"), &revision())
            .unwrap();
        assert_eq!(result.revision, revision());
        assert_eq!(result.files.len(), 300);
        assert!(!result.complete);
        assert!(result.notice.unwrap().contains("300 files"));
        exhausted(&dir, 3);
    }

    #[test]
    fn direct_compare_refuses_wrong_head_divergence_and_nonempty_equal_pair() {
        let wrong = "4444444444444444444444444444444444444444";
        let (dir, provider) = fixture(
            "alice",
            vec![step(
                &format!("repos/owner/repo/commits/{HEAD}"),
                json!({"sha": wrong}),
            )],
        );
        assert!(
            provider
                .direct_comparison(&repo("alice"), &revision())
                .unwrap_err()
                .to_string()
                .contains("different requested head")
        );
        exhausted(&dir, 1);

        let (dir, provider) = fixture(
            "alice",
            vec![
                step(
                    &format!("repos/owner/repo/commits/{HEAD}"),
                    json!({"sha": HEAD}),
                ),
                step(
                    &format!("repos/owner/repo/compare/{BASE}...{HEAD}?per_page=100&page=1"),
                    direct_compare(BASE, wrong, 1, vec![json!({"sha": HEAD})], vec![file(0)]),
                ),
            ],
        );
        assert!(
            provider
                .direct_comparison(&repo("alice"), &revision())
                .unwrap_err()
                .to_string()
                .contains("cannot represent")
        );
        exhausted(&dir, 2);

        let equal = Revision {
            base_sha: BASE.into(),
            head_sha: BASE.into(),
        };
        let (dir, provider) = fixture(
            "alice",
            vec![
                step(
                    &format!("repos/owner/repo/commits/{BASE}"),
                    json!({"sha": BASE}),
                ),
                step(
                    &format!("repos/owner/repo/compare/{BASE}...{BASE}?per_page=100&page=1"),
                    direct_compare(BASE, BASE, 0, Vec::new(), vec![file(0)]),
                ),
            ],
        );
        assert!(provider.direct_comparison(&repo("alice"), &equal).is_err());
        exhausted(&dir, 2);

        let (dir, provider) = fixture(
            "alice",
            vec![
                step(
                    &format!("repos/owner/repo/commits/{BASE}"),
                    json!({"sha": BASE}),
                ),
                step(
                    &format!("repos/owner/repo/compare/{BASE}...{BASE}?per_page=100&page=1"),
                    direct_compare(BASE, BASE, 0, Vec::new(), Vec::new()),
                ),
            ],
        );
        let empty = provider.direct_comparison(&repo("alice"), &equal).unwrap();
        assert!(empty.files.is_empty());
        assert!(empty.complete);
        exhausted(&dir, 2);
    }

    #[test]
    fn divergent_remote_since_review_falls_back_without_relabeling_three_dot_diff() {
        use crate::{
            comparisons::{
                BaselineResolution, ComparisonRequest, ReviewBaseline, ReviewBaselineSource,
                select_github_comparison,
            },
            review::ComparisonMode,
        };

        let reviewed = "3333333333333333333333333333333333333333";
        let (dir, provider) = fixture(
            "alice",
            vec![
                step(
                    &format!("repos/owner/repo/commits/{HEAD}"),
                    json!({"sha": HEAD}),
                ),
                step(
                    &format!("repos/owner/repo/compare/{reviewed}...{HEAD}?per_page=100&page=1"),
                    direct_compare(reviewed, BASE, 1, vec![json!({"sha": HEAD})], vec![file(0)]),
                ),
                step("repos/owner/repo/pulls/1", pull(1, 1)),
                step(&compare_path(), compare(vec![file(0)])),
                step(
                    "repos/owner/repo/pulls/1/files?per_page=100&page=1",
                    json!([file(0)]),
                ),
                step("repos/owner/repo/pulls/1", pull(1, 1)),
            ],
        );
        let inventory = CommitInventory {
            full_revision: revision(),
            commits: Vec::new(),
            availability: InventoryAvailability::Unavailable,
            notice: Some("historical inventory unavailable".into()),
        };
        let selection = select_github_comparison(
            &provider,
            &repo("alice"),
            1,
            &revision(),
            &inventory,
            ComparisonRequest::SinceLastReview {
                baseline: BaselineResolution::Found(ReviewBaseline {
                    reviewed_head_sha: reviewed.into(),
                    completed_at: "2026-09-13T10:00:00Z".into(),
                    source: ReviewBaselineSource::SubmittedReview {
                        review_id: "R1".into(),
                    },
                }),
            },
        )
        .unwrap();
        assert_eq!(selection.comparison.revision, revision());
        assert_eq!(selection.metadata.mode, ComparisonMode::FullPullRequest);
        assert_eq!(
            selection.metadata.requested_mode,
            Some(ComparisonMode::SinceLastReview {
                reviewed_head_sha: reviewed.into()
            })
        );
        assert!(
            selection
                .metadata
                .notice
                .unwrap()
                .contains("cannot represent")
        );
        exhausted(&dir, 6);
    }

    #[test]
    fn changing_revision_keeps_only_pinned_comparison() {
        let mut changed = pull(1, 2);
        changed["head"]["sha"] = json!("3333333333333333333333333333333333333333");
        let (dir, provider) = fixture(
            "alice",
            vec![
                step("repos/owner/repo/pulls/1", pull(1, 1)),
                step(&compare_path(), compare(vec![file(0)])),
                step(
                    "repos/owner/repo/pulls/1/files?per_page=100&page=1",
                    json!([file(0), file(1)]),
                ),
                step("repos/owner/repo/pulls/1", changed),
            ],
        );
        let result = provider.comparison(&repo("alice"), 1, &revision()).unwrap();
        assert_eq!(result.revision, revision());
        assert_eq!(result.files.len(), 1);
        assert!(!result.complete);
        assert!(result.notice.unwrap().contains("PR changed"));
        exhausted(&dir, 4);
    }

    #[test]
    fn mutable_file_over_return_never_replaces_immutable_comparison() {
        let immutable: Vec<_> = (0..300).map(file).collect();
        let mutable: Vec<_> = (0..301).map(file).collect();
        let (dir, provider) = fixture(
            "alice",
            vec![
                step("repos/owner/repo/pulls/1", pull(1, 300)),
                step(&compare_path(), compare(immutable)),
                step(
                    "repos/owner/repo/pulls/1/files?per_page=100&page=1",
                    json!(&mutable[..100]),
                ),
                step(
                    "repos/owner/repo/pulls/1/files?per_page=100&page=2",
                    json!(&mutable[100..200]),
                ),
                step(
                    "repos/owner/repo/pulls/1/files?per_page=100&page=3",
                    json!(&mutable[200..300]),
                ),
                step(
                    "repos/owner/repo/pulls/1/files?per_page=100&page=4",
                    json!(&mutable[300..]),
                ),
                step("repos/owner/repo/pulls/1", pull(1, 300)),
            ],
        );
        let result = provider.comparison(&repo("alice"), 1, &revision()).unwrap();
        assert_eq!(result.files.len(), 300);
        assert!(!result.complete);
        let notice = result.notice.unwrap();
        assert!(notice.contains("returned 301 files but reported 300"));
        assert!(!notice.contains("301 of 300"));
        exhausted(&dir, 7);
    }

    #[test]
    fn missing_truncated_and_renamed_files_remain_visible() {
        let mut binary = file(1);
        binary["filename"] = json!("video.mp4");
        binary["patch"] = Value::Null;
        let mut truncated = file(2);
        truncated["patch"] = json!("@@ -1 +1 @@\n-old");
        let mut renamed = file(3);
        renamed["status"] = json!("renamed");
        renamed["previous_filename"] = json!("old.rs");
        let files = vec![file(0), binary, truncated, renamed];
        let (dir, provider) = fixture(
            "alice",
            vec![
                step("repos/owner/repo/pulls/1", pull(1, 4)),
                step(&compare_path(), compare(files.clone())),
                step(
                    "repos/owner/repo/pulls/1/files?per_page=100&page=1",
                    json!(files),
                ),
                step("repos/owner/repo/pulls/1", pull(1, 4)),
            ],
        );
        let result = provider.comparison(&repo("alice"), 1, &revision()).unwrap();
        assert_eq!(result.files.len(), 4);
        assert!(!result.complete);
        assert!(result.files[0].patch_complete);
        assert!(!result.files[1].patch_complete);
        assert!(result.files[1].patch.is_none());
        assert!(!result.files[2].patch_complete);
        assert_eq!(result.files[3].previous_path.as_deref(), Some("old.rs"));
        exhausted(&dir, 4);
    }

    #[test]
    fn historical_cap_and_current_truncation_are_explicit() {
        let mut current = pull(1, 301);
        current["head"]["sha"] = json!("3333333333333333333333333333333333333333");
        let (_dir, provider) = fixture(
            "alice",
            vec![
                step("repos/owner/repo/pulls/1", current),
                step(&compare_path(), compare((0..300).map(file).collect())),
            ],
        );
        let result = provider.comparison(&repo("alice"), 1, &revision()).unwrap();
        assert!(!result.complete);
        assert!(
            result
                .notice
                .unwrap()
                .contains("Unsupported exact historical")
        );
        let (_dir, provider) = fixture(
            "alice",
            vec![
                step("repos/owner/repo/pulls/1", pull(1, 3001)),
                step(&compare_path(), compare(vec![file(0)])),
                step(
                    "repos/owner/repo/pulls/1/files?per_page=100&page=1",
                    json!([file(0)]),
                ),
                step("repos/owner/repo/pulls/1", pull(1, 3001)),
            ],
        );
        let result = provider.comparison(&repo("alice"), 1, &revision()).unwrap();
        assert!(!result.complete);
        assert!(result.notice.unwrap().contains("1 of 3001"));
    }

    #[test]
    fn historical_small_comparison_and_missing_revision() {
        let mut current = pull(1, 1);
        current["head"]["sha"] = json!("3333333333333333333333333333333333333333");
        let (_dir, provider) = fixture(
            "alice",
            vec![
                step("repos/owner/repo/pulls/1", current.clone()),
                step(&compare_path(), compare(vec![file(0)])),
            ],
        );
        let result = provider.comparison(&repo("alice"), 1, &revision()).unwrap();
        assert!(result.complete);
        assert_eq!(result.revision, revision());
        let (_dir, provider) = fixture(
            "alice",
            vec![
                step("repos/owner/repo/pulls/1", current),
                json!({"endpoint": compare_path(), "fail": true}),
            ],
        );
        let err = format!(
            "{:#}",
            provider
                .comparison(&repo("alice"), 1, &revision())
                .unwrap_err()
        );
        assert!(err.contains("Exact revision comparison unavailable"));
        assert!(!err.contains("fixture-private"));
    }

    #[test]
    fn patch_validation_checks_hunks_and_totals() {
        assert!(patch_is_complete("@@ -1 +1 @@\n-old\n+new", 1, 1));
        assert!(patch_is_complete(
            "@@ -0,0 +1,2 @@\n+one\n+two\n\\ No newline at end of file",
            2,
            0
        ));
        assert!(patch_is_complete(
            "@@ -1 +1 @@ fn\n-a\n+b\n@@ -20 +20 @@\n-c\n+d",
            2,
            2
        ));
        for patch in [
            "",
            "@@ -1,2 +1,2 @@\n-a\n+b",
            "@@ -1 +1 @@\n-a\n+b\n...",
            "@@ -1 +1 @@\n-a\n+b\n@@ -20 +20 @@\n-c",
        ] {
            assert!(!patch_is_complete(patch, 1, 1));
        }
        assert!(!patch_is_complete("@@ -1 +1 @@\n-a\n+b", 2, 2));
    }

    #[test]
    fn local_repository_resolution_is_read_only() {
        let checkout = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .arg(checkout.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(checkout.path())
                .args(["remote", "add", "origin", "git@github.com:owner/repo.git"])
                .status()
                .unwrap()
                .success()
        );
        let config_path = checkout.path().join(".git/config");
        let before = fs::read(&config_path).unwrap();
        let (_dir, provider) = fixture(
            "alice",
            vec![step(
                "repos/owner/repo",
                json!({"name": "repo", "owner": {"login": "owner"}}),
            )],
        );
        let resolved = provider
            .repository(checkout.path().to_str().unwrap())
            .unwrap();
        assert_eq!(resolved.full_name(), "owner/repo");
        assert_eq!(
            resolved.local_path,
            Some(checkout.path().canonicalize().unwrap())
        );
        assert_eq!(before, fs::read(config_path).unwrap());
    }

    #[test]
    fn subprocess_timeout_and_output_limits_withhold_output() {
        let runner = Runner {
            timeout: Duration::from_millis(80),
            output_limit: 64,
            ..Runner::default()
        };
        let mut command = Command::new("/usr/bin/python3");
        command.args(["-c", "import time; time.sleep(5)"]);
        let started = Instant::now();
        assert!(
            runner
                .run(command, "test timeout")
                .unwrap_err()
                .to_string()
                .contains("Timed out")
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let runner = Runner {
            timeout: Duration::from_secs(2),
            ..runner
        };
        for stream in ["sys.stdout", "sys.stderr"] {
            let mut command = Command::new("/usr/bin/python3");
            command.args([
                "-c",
                &format!("import sys; {stream}.write('sensitive' * 100)"),
            ]);
            let err = runner
                .run(command, "test overflow")
                .unwrap_err()
                .to_string();
            assert!(err.contains("limit"));
            assert!(!err.contains("sensitive"));
        }
    }

    #[test]
    fn auth_failure_or_empty_token_never_reaches_api() {
        for change in [
            json!({"token_fail": true}),
            json!({"token_output": ""}),
            json!({"token_output": "bad token"}),
        ] {
            let (dir, provider) = fixture("alice", vec![]);
            let path = dir.path().join("steps.json");
            let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            for (key, value) in change.as_object().unwrap() {
                config[key] = value.clone();
            }
            fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
            let err = provider
                .pull_request(&repo("alice"), 1)
                .unwrap_err()
                .to_string();
            assert!(!err.contains("fixture-private"));
            assert!(!dir.path().join("count").exists());
        }
    }

    #[test]
    fn file_api_cap_stops_at_3000_and_reports_remaining_files() {
        let all: Vec<_> = (0..3000).map(file).collect();
        let mut steps = vec![
            step("repos/owner/repo/pulls/1", pull(1, 3001)),
            step(&compare_path(), compare(all[..300].to_vec())),
        ];
        for (index, page) in all.chunks(100).enumerate() {
            steps.push(step(
                &format!(
                    "repos/owner/repo/pulls/1/files?per_page=100&page={}",
                    index + 1
                ),
                json!(page),
            ));
        }
        steps.push(step("repos/owner/repo/pulls/1", pull(1, 3001)));
        let (dir, provider) = fixture("alice", steps);
        let result = provider.comparison(&repo("alice"), 1, &revision()).unwrap();
        assert_eq!(result.files.len(), 3000);
        assert!(!result.complete);
        assert!(result.notice.unwrap().contains("3000 of 3001"));
        exhausted(&dir, 33);
    }

    #[test]
    fn file_pagination_failure_duplicates_and_mismatches_are_not_complete() {
        for response in [
            json!({"fail": true}),
            json!({"response": [file(0), file(0)]}),
        ] {
            let mut page = response;
            page["endpoint"] = json!("repos/owner/repo/pulls/1/files?per_page=100&page=1");
            let (_dir, provider) = fixture(
                "alice",
                vec![
                    step("repos/owner/repo/pulls/1", pull(1, 1)),
                    step(&compare_path(), compare(vec![file(0)])),
                    page,
                ],
            );
            assert!(provider.comparison(&repo("alice"), 1, &revision()).is_err());
        }
        let (_dir, provider) = fixture(
            "alice",
            vec![
                step("repos/owner/repo/pulls/1", pull(1, 1)),
                step(&compare_path(), compare(vec![file(0)])),
                step(
                    "repos/owner/repo/pulls/1/files?per_page=100&page=1",
                    json!([file(99)]),
                ),
                step("repos/owner/repo/pulls/1", pull(1, 1)),
            ],
        );
        let result = provider.comparison(&repo("alice"), 1, &revision()).unwrap();
        assert!(!result.complete);
        assert_eq!(result.files[0].path, "src/file0.rs");
        assert!(result.notice.unwrap().contains("disagrees"));
    }

    /// Explicit opt-in, public data only. Run with --ignored --nocapture.
    #[test]
    #[ignore = "uses existing gh auth and public cli/cli API reads"]
    fn live_public_repository_to_diff() {
        let accounts = GithubProvider::accounts().unwrap();
        for account in accounts {
            let provider = GithubProvider::new(account.clone());
            let repo = provider.repository("cli/cli").unwrap();
            let pulls = provider.list_pull_requests(&repo, "open").unwrap();
            assert!(!pulls.is_empty());
            let pull = provider.pull_request(&repo, 14398).unwrap();
            let comparison = provider
                .comparison(&repo, pull.number, &pull.revision())
                .unwrap();
            assert!(!comparison.files.is_empty());
            println!(
                "account={}/{} public repo={} PR={} base={} head={} files={} complete={} notice={:?}",
                account.host,
                account.login,
                repo.full_name(),
                pull.number,
                comparison.revision.base_sha,
                comparison.revision.head_sha,
                comparison.files.len(),
                comparison.complete,
                comparison.notice
            );
        }
    }

    /// Explicit opt-in schema check, public data only.
    #[test]
    #[ignore = "uses existing gh auth and public cli/cli API reads"]
    fn live_public_checkout_source() {
        let provider = GithubProvider::new(GithubProvider::accounts().unwrap().remove(0));
        let repo = provider.repository("cli/cli").unwrap();
        let source = provider.checkout_source(&repo, 14130).unwrap();
        assert_eq!(source.number, 14130);
        assert_eq!(source.base_repository, repo);
        println!(
            "Public cli/cli#{} source={} branch={} observed_head={}",
            source.number,
            source
                .source_repository
                .map(|repo| repo.full_name())
                .unwrap_or_else(|| "unavailable".into()),
            source.source_branch,
            source.observed_revision.head_sha,
        );
    }

    /// Explicit opt-in schema and immutable comparison check, public data only.
    #[test]
    #[ignore = "uses existing gh auth and public cli/cli API reads"]
    fn live_public_comparison_inventory_and_direct_commit() {
        let provider = GithubProvider::new(GithubProvider::accounts().unwrap().remove(0));
        let repo = provider.repository("cli/cli").unwrap();
        let pull = provider.pull_request(&repo, 14130).unwrap();
        let inventory = provider
            .commit_inventory(&repo, pull.number, &pull.revision())
            .unwrap();
        assert_eq!(inventory.availability, InventoryAvailability::Complete);
        let commit = inventory
            .commits
            .iter()
            .find(|commit| commit.parent_shas.len() == 1)
            .expect("public fixture has a direct commit");
        let direct = provider
            .direct_comparison(
                &repo,
                &Revision {
                    base_sha: commit.parent_shas[0].clone(),
                    head_sha: commit.sha.clone(),
                },
            )
            .unwrap();
        assert_eq!(direct.revision.head_sha, commit.sha);
        println!(
            "Public cli/cli#{} immutable head={} commits={} selected={} files={} complete={} notice={:?}",
            pull.number,
            pull.head_sha,
            inventory.commits.len(),
            commit.sha,
            direct.files.len(),
            direct.complete,
            direct.notice
        );
    }

    /// Explicit opt-in schema check, public data only.
    #[test]
    #[ignore = "uses existing gh auth and public cli/cli API reads"]
    fn live_public_pull_request_details() {
        let account = GithubProvider::accounts().unwrap().remove(0);
        let provider = GithubProvider::new(account);
        let repo = provider.repository("cli/cli").unwrap();
        let pull = provider.pull_request(&repo, 14398).unwrap();
        assert_eq!(pull.number, 14398);
        assert_ne!(pull.review_status, "UNKNOWN");
        let details = provider.details(&repo, 14398).unwrap();
        assert_eq!(details.number, 14398);
        assert!(
            details
                .issue_comments
                .iter()
                .all(|comment| comment.coordinates.pull_request == 14398)
        );
    }
}

#[cfg(test)]
#[path = "../tests/provider_actions.rs"]
mod provider_actions_fixture;

#[cfg(test)]
mod provider_actions {
    crate::provider_action_tests!();
}

#[cfg(test)]
#[path = "../tests/provider_lifecycle.rs"]
mod provider_lifecycle_fixture;

#[cfg(test)]
mod provider_lifecycle_tests {
    crate::provider_lifecycle_tests!();
}

#[cfg(test)]
#[path = "../tests/provider_reactions.rs"]
mod provider_reactions_fixture;

#[cfg(test)]
mod provider_reaction_tests {
    crate::provider_reaction_tests!();
}

#[cfg(test)]
#[path = "../tests/provider_review_dismissal.rs"]
mod provider_review_dismissal_fixture;

#[cfg(test)]
mod provider_review_dismissal_tests {
    crate::provider_review_dismissal_tests!();
}
