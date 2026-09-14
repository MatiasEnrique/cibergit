use super::*;
use crate::{
    domain::{
        PendingReviewCreationAcknowledgement, ProviderMutationOutcome, ReviewWriteAcknowledgement,
    },
    participation::{
        PendingFileCommentIntent, PendingFileReviewStartIntent, PendingFileReviewTarget,
        validate_pending_file_review_start_intent,
    },
};

pub struct PreparedPendingReviewCreate {
    variables: Value,
    intent: PendingFileReviewStartIntent,
}

pub struct PreparedPendingFileStartThread {
    mutation: PreparedReviewMutation,
    operation_id: String,
}

impl GithubProvider {
    /// Repeat the complete zero-review read and freeze an empty create request.
    /// This performs no mutation and returns no replayable authority after load.
    pub fn prepare_pending_review_start_create(
        &self,
        repo: &Repository,
        intent: &PendingFileReviewStartIntent,
    ) -> std::result::Result<PreparedPendingReviewCreate, String> {
        self.validate_repo(repo)
            .map_err(|error| error.to_string())?;
        validate_pending_file_review_start_intent(intent).map_err(|error| error.to_string())?;
        let expected_key = crate::participation::ReviewKey::for_repository(
            "github",
            repo,
            intent.key.pull_request,
        )
        .map_err(|error| error.to_string())?;
        if intent.key != expected_key {
            return Err("pending-review start belongs to another selected target".into());
        }
        let observation = Session::new(self)
            .pending_review(repo, intent.key.pull_request)
            .map_err(|error| error.to_string())?;
        if observation.snapshot.is_some() {
            return Err(
                "the selected account now has a pending review; zero create writes sent".into(),
            );
        }
        let absence = observation.absence.ok_or_else(|| {
            "fresh pending-review data did not prove complete exact absence; zero create writes sent"
                .to_owned()
        })?;
        if !absence
            .viewer_login
            .eq_ignore_ascii_case(&intent.selected_author)
            || absence.repository.cache_key() != repo.cache_key()
            || absence.pull_request != intent.pull_request
            || absence.pull_request_state != "OPEN"
            || absence.current_base_sha != intent.observed_base_sha
            || absence.current_head_sha != intent.observed_head_sha
        {
            return Err(
                "selected viewer, pull request, or head changed before pending-review creation; zero create writes sent"
                    .into(),
            );
        }
        Ok(PreparedPendingReviewCreate {
            variables: json!({
                "pullRequestId": intent.pull_request.remote_id,
                "commitOID": intent.observed_head_sha,
                "event": Value::Null,
                "body": Value::Null,
                "threads": Value::Null,
                "clientMutationId": intent.create_operation_id,
            }),
            intent: intent.clone(),
        })
    }

    /// Dispatch the already-prepared empty review mutation exactly once.
    pub fn dispatch_pending_review_start_create(
        &self,
        prepared: PreparedPendingReviewCreate,
        attempt_id: &str,
    ) -> ProviderMutationOutcome<PendingReviewCreationAcknowledgement> {
        let context = MutationContext {
            operation_id: prepared.intent.create_operation_id.clone(),
            attempt_id: attempt_id.to_owned(),
            action: "create-empty-pending-review".into(),
            payload: json!({
                "query": ADD_EMPTY_PENDING_REVIEW_MUTATION,
                "variables": prepared.variables,
            }),
        };
        let variables = context.payload["variables"].clone();
        let transport = Session::new(self).graphql_mutation::<PendingReviewCreateData>(
            ADD_EMPTY_PENDING_REVIEW_MUTATION,
            variables,
        );
        let data = match transport {
            MutationTransport::Rejected(reason) => {
                return ProviderMutationOutcome::PreflightRejected { reason };
            }
            MutationTransport::Uncertain(reason) => {
                return ProviderMutationOutcome::Uncertain { context, reason };
            }
            MutationTransport::Acknowledged(data) => data,
        };
        match validate_create_acknowledgement(&prepared.intent, data) {
            Ok(ack) => ProviderMutationOutcome::Acknowledged(ack),
            Err(reason) => ProviderMutationOutcome::Uncertain { context, reason },
        }
    }

    /// Repeat the sole-exact-created-review preflight and freeze a FILE request
    /// addressed to that exact durable ID.
    pub fn prepare_pending_review_start_thread(
        &self,
        repo: &Repository,
        intent: &PendingFileReviewStartIntent,
        creation: &PendingReviewCreationAcknowledgement,
    ) -> std::result::Result<PreparedPendingFileStartThread, String> {
        validate_pending_file_review_start_intent(intent).map_err(|error| error.to_string())?;
        if creation.operation_id != intent.create_operation_id
            || creation.pull_request != intent.pull_request
            || creation.review_commit_sha != intent.observed_head_sha
            || !creation
                .review_author
                .eq_ignore_ascii_case(&intent.selected_author)
            || !intent.key.matches(&creation.review)
        {
            return Err("created review receipt does not match the frozen FILE request".into());
        }
        let file_intent = PendingFileCommentIntent {
            operation_id: intent.thread_operation_id.clone(),
            key: intent.key.clone(),
            draft_id: intent.draft_id.clone(),
            body: intent.body.clone(),
            target: intent.target.clone(),
            pending: PendingFileReviewTarget {
                pull_request: intent.pull_request.clone(),
                pending_review: creation.review.clone(),
                selected_author: intent.selected_author.clone(),
                observed_base_sha: intent.observed_base_sha.clone(),
                observed_head_sha: intent.observed_head_sha.clone(),
                review_commit_sha: creation.review_commit_sha.clone(),
            },
        };
        let mutation =
            prepare_pending_file_comment_mutation(&mut Session::new(self), repo, &file_intent)?;
        Ok(PreparedPendingFileStartThread {
            mutation,
            operation_id: intent.thread_operation_id.clone(),
        })
    }

    pub fn dispatch_pending_review_start_thread(
        &self,
        prepared: PreparedPendingFileStartThread,
        attempt_id: &str,
    ) -> ProviderMutationOutcome<ReviewWriteAcknowledgement> {
        let context = MutationContext {
            operation_id: prepared.operation_id.clone(),
            attempt_id: attempt_id.to_owned(),
            action: prepared.mutation.action.into(),
            payload: json!({
                "query": prepared.mutation.query,
                "variables": prepared.mutation.variables,
            }),
        };
        let variables = context.payload["variables"].clone();
        let transport = Session::new(self)
            .graphql_mutation::<ReviewMutationData>(prepared.mutation.query, variables);
        let data = match transport {
            MutationTransport::Rejected(reason) => {
                return ProviderMutationOutcome::PreflightRejected { reason };
            }
            MutationTransport::Uncertain(reason) => {
                return ProviderMutationOutcome::Uncertain { context, reason };
            }
            MutationTransport::Acknowledged(data) => data,
        };
        match prepared.mutation.kind.acknowledgement(data) {
            Ok(ack) => ProviderMutationOutcome::Acknowledged(ReviewWriteAcknowledgement {
                operation_id: prepared.operation_id,
                review_id: ack.review_id,
                comment_id: ack.comment_id,
                thread_id: ack.thread_id,
            }),
            Err(reason) => ProviderMutationOutcome::Uncertain { context, reason },
        }
    }
}

const ADD_EMPTY_PENDING_REVIEW_MUTATION: &str = r#"mutation CreateEmptyPendingReview(
  $pullRequestId: ID!, $commitOID: GitObjectID!, $event: PullRequestReviewEvent,
  $body: String, $threads: [DraftPullRequestReviewThread], $clientMutationId: String!
) {
  createEmptyPendingReview: addPullRequestReview(input: {
    pullRequestId: $pullRequestId, commitOID: $commitOID, event: $event,
    body: $body, threads: $threads, clientMutationId: $clientMutationId
  }) {
    clientMutationId
    pullRequestReview {
      id state submittedAt author { login } commit { oid }
      pullRequest { id number repository { nameWithOwner } }
    }
  }
}"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingReviewCreateData {
    create_empty_pending_review: Option<PendingReviewCreatePayload>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingReviewCreatePayload {
    client_mutation_id: Option<String>,
    pull_request_review: Option<PendingReviewCreateNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingReviewCreateNode {
    id: String,
    state: String,
    submitted_at: Option<String>,
    author: Option<GraphqlActor>,
    commit: Option<GraphqlOid>,
    pull_request: ActionReviewPull,
}

fn validate_create_acknowledgement(
    intent: &PendingFileReviewStartIntent,
    data: PendingReviewCreateData,
) -> std::result::Result<PendingReviewCreationAcknowledgement, String> {
    let payload = data.create_empty_pending_review.ok_or_else(|| {
        "GitHub omitted the empty pending-review creation acknowledgement".to_owned()
    })?;
    if payload.client_mutation_id.as_deref() != Some(intent.create_operation_id.as_str()) {
        return Err("GitHub echoed another pending-review creation operation ID".into());
    }
    let review = payload
        .pull_request_review
        .ok_or_else(|| "GitHub omitted the newly created pending review".to_owned())?;
    validate_acknowledgement_id(&review.id, "created pending review")?;
    validate_acknowledgement_id(
        &review.pull_request.id,
        "created pending review pull request",
    )?;
    let author = review
        .author
        .map(|author| author.login)
        .ok_or_else(|| "GitHub omitted the created pending-review author".to_owned())?;
    let commit = review
        .commit
        .map(|commit| commit.oid)
        .ok_or_else(|| "GitHub omitted the created pending-review commit".to_owned())?;
    if review.state != "PENDING"
        || review.submitted_at.is_some()
        || review.id == review.pull_request.id
        || !author.eq_ignore_ascii_case(&intent.selected_author)
        || commit != intent.observed_head_sha
        || review.pull_request.id != intent.pull_request.remote_id
        || review.pull_request.number != intent.key.pull_request
        || !review
            .pull_request
            .repository
            .name_with_owner
            .eq_ignore_ascii_case(&format!("{}/{}", intent.key.owner, intent.key.repository))
    {
        return Err(
            "GitHub pending-review creation acknowledgement did not match the exact new ID, actor, PENDING state, unsubmitted state, commit, PR, and repository"
                .into(),
        );
    }
    Ok(PendingReviewCreationAcknowledgement {
        operation_id: intent.create_operation_id.clone(),
        review: ProviderCoordinates {
            provider: intent.key.provider.clone(),
            host: intent.key.host.clone(),
            owner: intent.key.owner.clone(),
            repository: intent.key.repository.clone(),
            pull_request: intent.key.pull_request,
            remote_id: review.id,
        },
        review_author: author,
        review_commit_sha: commit,
        pull_request: intent.pull_request.clone(),
        repository_name_with_owner: review.pull_request.repository.name_with_owner,
    })
}
