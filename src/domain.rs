//! Shared provider-independent models. No credentials belong in these values.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Account {
    pub host: String,
    pub login: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Repository {
    pub host: String,
    pub owner: String,
    pub name: String,
    pub account: Account,
    pub local_path: Option<PathBuf>,
}
impl Repository {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
    pub fn cache_key(&self) -> String {
        // JSON encoding preserves separators inside fields and account isolation.
        serde_json::to_string(&(
            self.host.as_str(),
            self.owner.as_str(),
            self.name.as_str(),
            self.account.host.as_str(),
            self.account.login.as_str(),
        ))
        .expect("string tuple")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub base_sha: String,
    pub head_sha: String,
}

/// Current provider coordinates for an explicit local-checkout workflow.
/// This observation does not replace an already displayed review revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestCheckoutSource {
    pub number: u64,
    pub base_repository: Repository,
    /// None when the source repository was deleted or is unavailable. Never
    /// substitute the base repository as a publishing destination in that case.
    pub source_repository: Option<Repository>,
    pub source_branch: String,
    pub target_branch: String,
    pub observed_revision: Revision,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub source_branch: String,
    pub target_branch: String,
    pub author: String,
    pub reviewers: Vec<String>,
    pub assignees: Vec<String>,
    pub labels: Vec<String>,
    /// User identities known to have participated in this PR. This includes the
    /// author, requested user reviewers, assignees, issue commenters, and
    /// authors of submitted reviews. Teams are not user identities.
    #[serde(default)]
    pub participants: Vec<String>,
    /// False means `participants` is a safe partial set, never a complete claim.
    #[serde(default)]
    pub participants_complete: bool,
    #[serde(default)]
    pub participants_notice: Option<String>,
    pub draft: bool,
    /// OPEN, CLOSED, MERGED.
    pub state: String,
    pub review_status: String,
    pub check_status: String,
    pub base_sha: String,
    pub head_sha: String,
    pub url: String,
}
impl PullRequest {
    pub fn revision(&self) -> Revision {
        Revision {
            base_sha: self.base_sha.clone(),
            head_sha: self.head_sha.clone(),
        }
    }
}

/// Stable coordinates for a provider-owned collaboration object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCoordinates {
    pub provider: String,
    pub host: String,
    pub owner: String,
    pub repository: String,
    pub pull_request: u64,
    pub remote_id: String,
}

/// The four GitHub pull-request objects that implement GraphQL `Reactable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReactableKind {
    PullRequest,
    PullRequestReview,
    IssueComment,
    PullRequestReviewComment,
}

impl ReactableKind {
    pub fn graphql_name(self) -> &'static str {
        match self {
            Self::PullRequest => "PullRequest",
            Self::PullRequestReview => "PullRequestReview",
            Self::IssueComment => "IssueComment",
            Self::PullRequestReviewComment => "PullRequestReviewComment",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReactionContent {
    ThumbsUp,
    ThumbsDown,
    Laugh,
    Confused,
    Heart,
    Hooray,
    Rocket,
    Eyes,
}

impl ReactionContent {
    pub const ALL: [Self; 8] = [
        Self::ThumbsUp,
        Self::ThumbsDown,
        Self::Laugh,
        Self::Confused,
        Self::Heart,
        Self::Hooray,
        Self::Rocket,
        Self::Eyes,
    ];

    pub fn graphql_name(self) -> &'static str {
        match self {
            Self::ThumbsUp => "THUMBS_UP",
            Self::ThumbsDown => "THUMBS_DOWN",
            Self::Laugh => "LAUGH",
            Self::Confused => "CONFUSED",
            Self::Heart => "HEART",
            Self::Hooray => "HOORAY",
            Self::Rocket => "ROCKET",
            Self::Eyes => "EYES",
        }
    }

    pub fn compact_label(self) -> &'static str {
        match self {
            Self::ThumbsUp => "+1",
            Self::ThumbsDown => "-1",
            Self::Laugh => "Laugh",
            Self::Confused => "Confused",
            Self::Heart => "Heart",
            Self::Hooray => "Hooray",
            Self::Rocket => "Rocket",
            Self::Eyes => "Eyes",
        }
    }

    pub fn from_graphql(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|content| content.graphql_name() == value)
    }
}

/// Historical presentation data. Cached selected-viewer state may be shown,
/// but it never authorizes a mutation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionGroupSnapshot {
    pub content: ReactionContent,
    pub count: u64,
    pub viewer_has_reacted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionSnapshot {
    pub groups: Vec<ReactionGroupSnapshot>,
    /// False means missing, duplicate, partial, or otherwise unusable groups
    /// were preserved as Unknown rather than filled with zeroes.
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedViewer {
    pub node_id: String,
    pub login: String,
}

/// Fresh selected-account evidence from the current details generation. This
/// field is deliberately dropped by every cache round-trip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshReactionCapability {
    pub viewer: SelectedViewer,
    pub viewer_can_react: bool,
}

/// One exact reactable object and its historical/fresh reaction state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionSubjectSnapshot {
    pub kind: ReactableKind,
    pub pull_request: ProviderCoordinates,
    pub subject: ProviderCoordinates,
    /// Required only for PullRequestReviewComment and exact when present.
    pub parent_review: Option<ProviderCoordinates>,
    pub content: String,
    pub reactions: ReactionSnapshot,
    #[serde(skip)]
    pub fresh_capability: Option<FreshReactionCapability>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReactionIntent {
    Add,
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReactionAction {
    Add,
    Remove { existing_reaction_id: String },
}

impl ReactionAction {
    pub fn intent(&self) -> ReactionIntent {
        match self {
            Self::Add => ReactionIntent::Add,
            Self::Remove { .. } => ReactionIntent::Remove,
        }
    }
}

/// Exact immutable target frozen after the first bounded provider preflight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionTarget {
    pub kind: ReactableKind,
    pub repository: Repository,
    pub pull_request: ProviderCoordinates,
    pub subject: ProviderCoordinates,
    pub parent_review: Option<ProviderCoordinates>,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionRequest {
    pub operation_id: String,
    pub attempt_id: String,
    pub target: ReactionTarget,
    pub viewer: SelectedViewer,
    pub content: ReactionContent,
    pub action: ReactionAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionAcknowledgement {
    pub operation_id: String,
    pub target: ReactionTarget,
    pub viewer: SelectedViewer,
    pub content: ReactionContent,
    pub reaction_id: String,
    pub present: bool,
}

/// Complete targeted read used only for preparation or read-only recovery.
/// It records current convergence and never attributes causation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionObservation {
    pub target: ReactionTarget,
    pub viewer: SelectedViewer,
    pub content: ReactionContent,
    pub viewer_can_react: bool,
    pub viewer_has_reacted: bool,
    pub own_reaction_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueComment {
    pub coordinates: ProviderCoordinates,
    pub author: Option<String>,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReview {
    pub coordinates: ProviderCoordinates,
    pub author: Option<String>,
    pub body: String,
    /// APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED, or PENDING.
    pub state: String,
    pub submitted_at: Option<String>,
    /// May be unavailable after history is removed or becomes inaccessible.
    pub commit_sha: Option<String>,
    /// Present only when a fresh provider details read returned every field
    /// needed to decide whether the selected viewer may edit this summary.
    /// It is never serialized; all cached records deserialize to `None` and
    /// remain read-only.
    #[serde(default, skip_serializing)]
    pub edit_summary_capability: Option<SubmittedReviewEditCapability>,
    /// Fresh selected-viewer evidence for dismissing this exact submitted
    /// review. This is deliberately neither serialized nor deserialized, so a
    /// cache record can never manufacture dismissal authority.
    #[serde(skip)]
    pub dismissal_capability: Option<FreshReviewDismissalCapability>,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmittedReviewEditCapability {
    pub viewer_did_author: bool,
    pub viewer_can_update: bool,
    #[serde(default)]
    pub viewer_cannot_update_reasons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DismissalAuthority {
    Available,
    Unknown { reason: String },
    Unavailable { reason: String },
}

impl DismissalAuthority {
    pub fn permits_attempt(&self) -> bool {
        matches!(self, Self::Available | Self::Unknown { .. })
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Available => None,
            Self::Unknown { reason } | Self::Unavailable { reason } => Some(reason),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshReviewDismissalCapability {
    pub viewer: SelectedViewer,
    pub pull_request: ProviderCoordinates,
    pub authority: DismissalAuthority,
}

/// Exact immutable submitted-review tuple frozen by a fresh targeted read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmittedReviewDismissalTarget {
    pub repository: Repository,
    pub pull_request: ProviderCoordinates,
    pub review: ProviderCoordinates,
    pub review_state: String,
    pub review_body: String,
    pub submitted_at: String,
    pub review_author: Option<String>,
    pub review_commit_sha: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmittedReviewDismissalRequest {
    pub operation_id: String,
    pub attempt_id: String,
    pub target: SubmittedReviewDismissalTarget,
    pub viewer: SelectedViewer,
    pub authority: DismissalAuthority,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmittedReviewDismissalAcknowledgement {
    pub operation_id: String,
    pub target: SubmittedReviewDismissalTarget,
    pub viewer: SelectedViewer,
    pub final_state: String,
}

/// Current known-ID state used only for explicit read-only reconciliation.
/// It never proves which actor caused the state or which message was recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmittedReviewDismissalObservation {
    pub target: SubmittedReviewDismissalTarget,
    pub viewer: SelectedViewer,
    pub authority: DismissalAuthority,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub coordinates: ProviderCoordinates,
    pub author: Option<String>,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
    pub url: String,
    pub path: String,
    /// Explicit provider subject. Cached records written before subject
    /// support deserialize as `Unknown` and must remain read-only.
    #[serde(default)]
    pub subject: ReviewSubject,
    pub line: Option<u64>,
    pub original_line: Option<u64>,
    pub start_line: Option<u64>,
    pub original_start_line: Option<u64>,
    pub side: Option<String>,
    pub diff_hunk: String,
    /// Both values are optional because GitHub can retain activity after the
    /// referenced commits are no longer accessible to the selected account.
    pub commit_sha: Option<String>,
    pub original_commit_sha: Option<String>,
    pub outdated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedReviewComment {
    pub pull_request_review_id: String,
    pub comment: ReviewComment,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReviewSnapshot {
    pub review: PullRequestReview,
    /// Only comments whose provider-reported parent is `review` are included.
    pub comments: Vec<LinkedReviewComment>,
    /// False when any review/thread/comment connection needed for linkage was
    /// partial or capped.
    pub comments_complete: bool,
    /// Fresh, complete provider evidence required to target this exact pending
    /// review with a file-level comment. This is deliberately never cached.
    #[serde(skip)]
    pub file_comment_source: Option<PendingFileCommentSource>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewSubject {
    Line,
    File,
    #[default]
    Unknown,
}

impl ReviewSubject {
    pub fn from_provider(value: &str) -> Self {
        match value {
            "LINE" => Self::Line,
            "FILE" => Self::File,
            _ => Self::Unknown,
        }
    }
}

/// Immutable provider evidence from one fresh, complete pending-review read.
/// It is frozen into a file-comment request and checked again immediately
/// before dispatch; it is not cached permission or pending authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingFileCommentSource {
    pub viewer_login: String,
    pub repository: Repository,
    pub pull_request: ProviderCoordinates,
    pub pull_request_state: String,
    pub current_base_sha: String,
    pub current_head_sha: String,
    pub review: ProviderCoordinates,
    pub review_author: String,
    pub review_commit_sha: String,
}

/// Fresh, complete provider proof that the selected account has no pending
/// review for one exact open pull request and head. This capability is kept
/// only in memory: cached or restored absence never authorizes review creation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingFileReviewAbsence {
    pub viewer_login: String,
    pub repository: Repository,
    pub pull_request: ProviderCoordinates,
    pub pull_request_url: String,
    pub pull_request_state: String,
    pub current_base_sha: String,
    pub current_head_sha: String,
}

/// One complete selected-account pending-review read. `absence` is present
/// only for an exact fresh zero-review result and is intentionally not
/// serializable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReviewObservation {
    pub snapshot: Option<PendingReviewSnapshot>,
    pub absence: Option<PendingFileReviewAbsence>,
}

/// Compact result of fully validating the empty pending-review creation
/// acknowledgement. Raw response bodies are never copied into durable state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReviewCreationAcknowledgement {
    pub operation_id: String,
    pub review: ProviderCoordinates,
    pub review_author: String,
    pub review_commit_sha: String,
    pub pull_request: ProviderCoordinates,
    pub repository_name_with_owner: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationContext {
    pub operation_id: String,
    pub attempt_id: String,
    pub action: String,
    /// Exact bounded JSON sent (or intended to be sent) to the provider.
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderMutationOutcome<T> {
    PreflightRejected {
        reason: String,
    },
    Acknowledged(T),
    /// A write was started but its authoritative result is not known. Callers
    /// must reconcile with a read before offering an explicit retry.
    Uncertain {
        context: MutationContext,
        reason: String,
    },
}

/// A reasoned provider capability. `available == false` is never an implicit
/// permission claim: callers should display `reason` and refresh the snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapability {
    pub available: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReviewer {
    /// USER or TEAM.
    pub kind: String,
    /// A user login or team slug, without presentation prefixes.
    pub name: String,
}

/// A current, account-isolated PR metadata observation. This mutable metadata
/// is independent of any comparison revision displayed by the caller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestLifecycleSnapshot {
    pub repository: Repository,
    /// The remote ID is the exact pull request node ID.
    pub pull_request: ProviderCoordinates,
    pub updated_at: String,
    /// OPEN, CLOSED, or MERGED.
    pub state: String,
    pub head_sha: String,
    pub title: String,
    pub body: String,
    pub base_branch: String,
    pub draft: bool,
    pub reviewers: Vec<PullRequestReviewer>,
    pub assignees: Vec<String>,
    pub labels: Vec<String>,
    pub viewer_login: String,
    pub viewer_permission: Option<String>,
    pub can_update_metadata: ProviderCapability,
    pub can_change_state: ProviderCapability,
    pub can_change_draft: ProviderCapability,
    pub can_request_reviewers: ProviderCapability,
    pub can_change_labels: ProviderCapability,
    pub can_change_assignees: ProviderCapability,
    pub can_comment: ProviderCapability,
    pub values_complete: bool,
    pub capabilities_complete: bool,
    pub notice: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderChoice {
    /// Exact provider ID where one is available; never synthesized from a name.
    pub remote_id: Option<String>,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderChoiceSet {
    pub values: Vec<ProviderChoice>,
    pub complete: bool,
    pub notice: Option<String>,
}

/// Explicit repository-wide picker data. This is not part of per-PR sidebar
/// hydration and is bounded independently for each collection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestLifecycleChoices {
    pub repository: Repository,
    pub branches: ProviderChoiceSet,
    pub labels: ProviderChoiceSet,
    pub assignees: ProviderChoiceSet,
    pub reviewer_users: ProviderChoiceSet,
    pub reviewer_teams: ProviderChoiceSet,
}

/// Immutable coordinates and observations shared by one explicit PR action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestMutationTarget {
    /// Includes the selected account; `local_path` is informational and is not
    /// used as provider authority.
    pub repository: Repository,
    /// The remote ID is the exact pull request node ID.
    pub pull_request: ProviderCoordinates,
    pub observed_updated_at: String,
    pub observed_state: String,
    pub observed_head_sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullRequestLifecycleAction {
    UpdateTitle { observed: String, value: String },
    UpdateBody { observed: String, value: String },
    UpdateBaseBranch { observed: String, value: String },
    Close,
    Reopen,
    ConvertToDraft,
    MarkReadyForReview,
    AddReviewer(PullRequestReviewer),
    RemoveReviewer(PullRequestReviewer),
    AddLabel(String),
    RemoveLabel(String),
    AddAssignee(String),
    RemoveAssignee(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestLifecycleRequest {
    pub operation_id: String,
    pub attempt_id: String,
    pub target: PullRequestMutationTarget,
    pub action: PullRequestLifecycleAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestLifecycleAcknowledgement {
    pub operation_id: String,
    pub repository: Repository,
    pub pull_request: ProviderCoordinates,
    pub updated_at: String,
    pub state: String,
    pub head_sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullRequestDiscussionAction {
    Create {
        body: String,
    },
    Edit {
        comment: ProviderCoordinates,
        selected_author: String,
        observed_body: String,
        observed_updated_at: String,
        body: String,
    },
    Delete {
        comment: ProviderCoordinates,
        selected_author: String,
        observed_body: String,
        observed_updated_at: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestDiscussionRequest {
    pub operation_id: String,
    pub attempt_id: String,
    pub target: PullRequestMutationTarget,
    pub action: PullRequestDiscussionAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestDiscussionAcknowledgement {
    pub operation_id: String,
    pub repository: Repository,
    pub pull_request: ProviderCoordinates,
    pub comment: ProviderCoordinates,
    pub body: Option<String>,
    pub deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestCreationInput {
    pub target_repository: Repository,
    pub base_branch: String,
    pub source_repository: Repository,
    pub source_branch: String,
    /// Informational only. It makes a local/provider branch mismatch explicit;
    /// it is never used to infer or publish a provider ref.
    pub local_branch: Option<String>,
    pub title: String,
    pub body: String,
    pub draft: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestCreationPreparation {
    pub input: PullRequestCreationInput,
    pub observed_base_sha: String,
    pub observed_source_head_sha: String,
    pub viewer_login: String,
    pub repository_permission: Option<String>,
    pub can_create: ProviderCapability,
    /// False for GitHub's create endpoint: preflight observes a source SHA, but
    /// the server does not atomically require that SHA during creation.
    pub reviewed_head_atomically_enforced: bool,
    pub notice: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestCreationRequest {
    pub operation_id: String,
    pub attempt_id: String,
    pub preparation: PullRequestCreationPreparation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestCreationAcknowledgement {
    pub operation_id: String,
    pub target_repository: Repository,
    pub source_repository: Repository,
    pub pull_request: ProviderCoordinates,
    pub actual_head_sha: String,
    pub reviewed_head_sha: String,
    pub reviewed_head_atomically_enforced: bool,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationAdmissionReceipt {
    pub operation_id: String,
    pub attempt_id: String,
    pub durable_record_id: String,
}

/// A terminal journal fact recorded while the admitted attempt still owns its
/// cross-process per-target authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationTerminalRecord {
    NotStarted { reason: String },
    Acknowledged { acknowledgement: serde_json::Value },
    Uncertain { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderReadEvidence<T> {
    Observed(T),
    /// A missing or inaccessible object is not evidence that a mutation was
    /// not applied. The caller may keep the attempt visibly unresolved.
    Inconclusive {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewWriteAcknowledgement {
    pub operation_id: String,
    pub review_id: Option<String>,
    pub comment_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewAuxiliaryAction {
    UpdatePendingSummary {
        review: ProviderCoordinates,
        body: String,
    },
    /// An author-owned submitted review summary edit. Every observed target
    /// property is frozen so confirmation, durable recovery, and provider
    /// preflight all refer to the same historical review object.
    UpdateSubmittedSummary {
        review: ProviderCoordinates,
        selected_author: String,
        submitted_state: String,
        submitted_commit_sha: String,
        expected_body: String,
        body: String,
    },
    DeletePendingComment {
        review: ProviderCoordinates,
        comment: ProviderCoordinates,
    },
    CancelPendingReview {
        review: ProviderCoordinates,
    },
    Reply {
        thread: ProviderCoordinates,
        pending_review: Option<ProviderCoordinates>,
        body: String,
    },
    SetThreadResolved {
        thread: ProviderCoordinates,
        resolved: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAuxiliaryRequest {
    pub operation_id: String,
    pub attempt_id: String,
    pub action: ReviewAuxiliaryAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAuxiliaryAcknowledgement {
    pub operation_id: String,
    pub review_id: Option<String>,
    pub comment_id: Option<String>,
    pub thread_id: Option<String>,
    pub resolved: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub fn rest_name(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        }
    }

    pub fn graphql_name(self) -> &'static str {
        match self {
            Self::Merge => "MERGE",
            Self::Squash => "SQUASH",
            Self::Rebase => "REBASE",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePreparation {
    pub pull_request: ProviderCoordinates,
    pub pull_request_node_id: String,
    pub reviewed_head_sha: String,
    pub current_head_sha: String,
    pub head_ref_name: String,
    pub head_ref_node_id: Option<String>,
    pub head_repository: String,
    pub state: String,
    pub draft: bool,
    pub mergeable: String,
    pub merge_state_status: String,
    pub review_status: String,
    pub check_status: String,
    pub repository_permission: Option<String>,
    pub allowed_methods: Vec<MergeMethod>,
    pub blockers: Vec<String>,
    pub auto_merge_allowed: bool,
    pub auto_merge_enabled: bool,
    pub can_enable_auto_merge: bool,
    pub can_disable_auto_merge: bool,
    pub merge_queue_required: bool,
    pub in_merge_queue: bool,
    pub viewer_can_merge_as_admin: bool,
    pub viewer_can_delete_head_ref: bool,
    pub preferred_headlines: Vec<(MergeMethod, String)>,
    pub preferred_bodies: Vec<(MergeMethod, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeAction {
    Merge {
        method: MergeMethod,
        commit_title: Option<String>,
        commit_message: Option<String>,
    },
    EnableAutoMerge {
        method: MergeMethod,
        commit_title: Option<String>,
        commit_message: Option<String>,
    },
    DisableAutoMerge,
    Enqueue,
    Dequeue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeExecutionRequest {
    pub operation_id: String,
    pub attempt_id: String,
    pub action: MergeAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeAcknowledgement {
    pub operation_id: String,
    /// True means GitHub accepted the request. Queue/auto-merge acceptance is
    /// deliberately distinct from a completed merge.
    pub accepted: bool,
    pub completed: bool,
    pub merged: bool,
    pub merge_commit_sha: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchDeletionRequest {
    pub operation_id: String,
    pub attempt_id: String,
    pub expected_merged_head_sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchDeletionAcknowledgement {
    pub operation_id: String,
    pub deleted_ref_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewThread {
    pub coordinates: ProviderCoordinates,
    pub path: String,
    /// Explicit provider subject. `Unknown` is never inferred from missing line
    /// fields and cannot authorize reply/edit/resolve affordances.
    #[serde(default)]
    pub subject: ReviewSubject,
    pub line: Option<u64>,
    pub original_line: Option<u64>,
    pub start_line: Option<u64>,
    pub original_start_line: Option<u64>,
    pub side: Option<String>,
    pub start_side: Option<String>,
    pub resolved: bool,
    pub outdated: bool,
    pub comments: Vec<ReviewComment>,
    /// False when GitHub reported more nested comments than this bounded read.
    pub comments_complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckKind {
    CheckRun,
    CommitStatus,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckShaClass {
    Head,
    MergeCandidate,
    Other,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRepositoryIdentity {
    pub node_id: String,
    pub name_with_owner: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckAppIdentity {
    pub node_id: String,
    pub name: String,
    pub slug: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSuiteIdentity {
    pub node_id: String,
    /// Nullable GitHub GraphQL `Int`, widened after its signed 32-bit bounds
    /// are validated. This is never derived from the opaque node ID.
    pub database_id: Option<u64>,
    pub repository: CheckRepositoryIdentity,
    pub app: Option<CheckAppIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunIdentity {
    pub node_id: String,
    pub database_id: u64,
    pub run_attempt: u64,
    pub run_number: u64,
    pub event: String,
    pub github_url: String,
    pub workflow_node_id: String,
    pub workflow_database_id: u64,
    pub workflow_name: String,
}

/// Exact evidence returned for the check suite's optional Actions relation.
/// `NoObservedLink` is deliberately not a third-party classification.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "SCREAMING_SNAKE_CASE",
    tag = "state",
    content = "identity"
)]
pub enum ActionsLinkage {
    #[default]
    Unknown,
    NoObservedLink,
    Linked(WorkflowRunIdentity),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestCheck {
    pub coordinates: ProviderCoordinates,
    pub kind: CheckKind,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub description: Option<String>,
    /// Integrator-controlled destination (`detailsUrl` / `targetUrl`). It is
    /// display-only and is never treated as GitHub identity or fetched.
    pub details_url: Option<String>,
    /// GitHub's own stable CheckRun summary URL. Commit statuses do not have
    /// this field.
    #[serde(default)]
    pub github_permalink: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    /// `None` means GitHub did not provide complete requiredness evidence.
    pub required: Option<bool>,
    /// Nullable GitHub GraphQL `Int`, widened only after exact bounds checks.
    #[serde(default)]
    pub database_id: Option<u64>,
    #[serde(default)]
    pub suite: Option<CheckSuiteIdentity>,
    #[serde(default)]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub commit_repository: Option<CheckRepositoryIdentity>,
    #[serde(default)]
    pub sha_class: CheckShaClass,
    #[serde(default)]
    pub actions_linkage: ActionsLinkage,
}

/// Complete in-memory locator for one GitHub Actions run attempt. This value is
/// deliberately not serializable: a cached Checks row may locate a fresh read,
/// but it never makes Jobs or Logs current by itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionsAttemptLocator {
    pub account: Account,
    pub base_repository: CheckRepositoryIdentity,
    pub pull_request_node_id: String,
    pub pull_request_number: u64,
    pub observed_head_sha: String,
    pub head_repository: CheckRepositoryIdentity,
    pub rollup_commit_sha: String,
    pub rollup_repository: CheckRepositoryIdentity,
    pub check_node_id: String,
    pub check_database_id: u64,
    pub check_commit_sha: String,
    pub check_repository: CheckRepositoryIdentity,
    pub suite: CheckSuiteIdentity,
    pub workflow_run: WorkflowRunIdentity,
}

/// A locator plus the freshly resolved server-side viewer identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionsAttemptKey {
    pub locator: ActionsAttemptLocator,
    pub viewer_node_id: String,
    pub viewer_login: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionsHeadRelation {
    CurrentHead,
    HistoricalHead,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionsPullRequestIdentity {
    pub number: u64,
    pub base_repository: CheckRepositoryIdentity,
    pub base_sha: String,
    pub head_repository: CheckRepositoryIdentity,
    pub head_sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionsRunAttemptObservation {
    pub key: ActionsAttemptKey,
    pub status: String,
    pub conclusion: Option<String>,
    pub api_url: String,
    pub html_url: String,
    pub workflow_url: String,
    pub returned_pull_requests: Vec<ActionsPullRequestIdentity>,
    pub relation: ActionsHeadRelation,
    pub observed_at_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionsJobStep {
    pub number: u64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionsJob {
    /// REST job ID. It is unrelated to the CheckRun database ID.
    pub id: u64,
    pub node_id: String,
    pub run_id: u64,
    pub run_attempt: u64,
    pub head_sha: String,
    /// Parsed only from the canonical `check_run_url` route.
    pub check_run_database_id: u64,
    pub check_run_url: String,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub api_url: String,
    pub html_url: String,
    pub steps: Vec<ActionsJobStep>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionsJobsSnapshot {
    pub attempt: ActionsRunAttemptObservation,
    /// Provider order retained for the second-pass movement proof.
    pub provider_ordered_job_ids: Vec<u64>,
    /// Presentation order only; selection remains keyed by REST job ID.
    pub jobs: Vec<ActionsJob>,
    pub selected_check_job_id: u64,
    pub complete: bool,
    pub observed_at_unix_ms: u64,
    pub observation_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionsLogProvenance {
    FreshExactRead,
    HistoricalDisplay,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionsJobLog {
    pub key: ActionsAttemptKey,
    pub job: ActionsJob,
    pub jobs_observation_id: u64,
    pub raw_byte_count: usize,
    pub line_count: usize,
    pub sanitized_text: String,
    pub observed_at_unix_ms: u64,
    pub provenance: ActionsLogProvenance,
}

/// One explicit GitHub Actions run control. Force-cancel and arbitrary
/// workflow dispatch are deliberately absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActionsRunControlAction {
    RerunAllJobs,
    RerunFailedJobs,
    CancelRun,
}

impl ActionsRunControlAction {
    /// The final REST path segment documented for this control.
    pub fn rest_segment(self) -> &'static str {
        match self {
            Self::RerunAllJobs => "rerun",
            Self::RerunFailedJobs => "rerun-failed-jobs",
            Self::CancelRun => "cancel",
        }
    }

    /// The single documented accepted status. Every other status is treated as
    /// an unresolved outcome, never as a proven no-op.
    pub fn accepted_status(self) -> u16 {
        match self {
            Self::RerunAllJobs | Self::RerunFailedJobs => 201,
            Self::CancelRun => 202,
        }
    }

    pub fn journal_action(self) -> &'static str {
        match self {
            Self::RerunAllJobs => "rerun-actions-run-all-jobs",
            Self::RerunFailedJobs => "rerun-actions-run-failed-jobs",
            Self::CancelRun => "cancel-actions-run",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::RerunAllJobs => "Re-run all jobs",
            Self::RerunFailedJobs => "Re-run failed jobs",
            Self::CancelRun => "Cancel run",
        }
    }

    /// Stable element identity for this control's native button.
    pub fn control_element_id(self) -> &'static str {
        match self {
            Self::RerunAllJobs => "actions-control-rerun-actions-run-all-jobs",
            Self::RerunFailedJobs => "actions-control-rerun-actions-run-failed-jobs",
            Self::CancelRun => "actions-control-cancel-actions-run",
        }
    }
}

/// A serialized projection of the exact Actions identity one control targets.
/// It is a record of what was frozen, never fresh capability: every dispatch
/// re-establishes identity, status, and authority from a new read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionsRunControlTarget {
    pub account: Account,
    pub repository_node_id: String,
    pub repository_name_with_owner: String,
    pub pull_request_number: u64,
    pub pull_request_node_id: String,
    pub check_node_id: String,
    pub check_database_id: u64,
    pub check_suite_node_id: String,
    pub check_suite_database_id: u64,
    pub workflow_node_id: String,
    pub workflow_database_id: u64,
    pub workflow_name: String,
    pub run_node_id: String,
    pub run_database_id: u64,
    pub run_number: u64,
    /// The attempt the Checks inspector displayed. A control refuses when
    /// GitHub's current attempt differs; it never retargets silently.
    pub run_attempt: u64,
    pub run_event: String,
    pub run_head_sha: String,
    pub run_html_url: String,
}

/// Fresh explicit repository-permission evidence. Resolving `/user` proves
/// which account is selected, never what that account may write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionsRunControlAuthority {
    /// A fresh repository read returned write (push) permission.
    Available,
    /// GitHub returned no usable permission evidence. GitHub decides at
    /// dispatch; this is never treated as permission.
    Unknown { reason: String },
    /// A fresh repository read showed no write permission.
    Unavailable { reason: String },
}

impl ActionsRunControlAuthority {
    pub fn permits_attempt(&self) -> bool {
        !matches!(self, Self::Unavailable { .. })
    }
}

/// One complete fresh observation of the exact run a control would target.
/// Preparation and the post-admission preflight compare this value whole.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionsRunControlObservation {
    pub target: ActionsRunControlTarget,
    pub viewer: SelectedViewer,
    pub run_status: String,
    pub run_conclusion: Option<String>,
    pub authority: ActionsRunControlAuthority,
}

/// The exact outgoing request, frozen once. The same method, path, body, and
/// action name back the durable admission, the dispatch, and every post-send
/// local failure record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionsRunControlPreparation {
    pub action: ActionsRunControlAction,
    pub observation: ActionsRunControlObservation,
    pub method: String,
    pub path: String,
    pub body: serde_json::Value,
    pub observed_at_unix_ms: u64,
    pub notices: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionsRunControlRequest {
    pub operation_id: String,
    pub attempt_id: String,
    pub preparation: ActionsRunControlPreparation,
}

/// What a later read observed about the run. It is disclosure, not proof that
/// this request caused the change: GitHub offers no per-request correlation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionsRunControlProgress {
    pub run_attempt: u64,
    pub run_status: String,
    pub run_conclusion: Option<String>,
}

/// GitHub returned the documented accepted status for the exact frozen
/// request. That is acceptance of the request only. It does not prove a new
/// attempt started, that any job re-ran, or that the run is cancelled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionsRunControlAcknowledgement {
    pub operation_id: String,
    pub action: ActionsRunControlAction,
    pub target: ActionsRunControlTarget,
    pub accepted_status: u16,
    pub observed_after: ProviderReadEvidence<ActionsRunControlProgress>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeEligibility {
    /// OPEN, CLOSED, or MERGED.
    pub state: String,
    pub draft: bool,
    pub mergeable: String,
    pub merge_state_status: String,
    pub review_status: String,
    pub check_status: String,
    pub maintainer_can_modify: bool,
    pub can_rebase: bool,
    pub can_update_branch: bool,
    pub auto_merge_enabled: bool,
    pub in_merge_queue: bool,
}

/// Mutable collaboration data for Overview, Activity, and Checks. It contains
/// no displayed comparison revision and must never replace a pinned diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestDetails {
    pub number: u64,
    /// Source identities observed with this mutable collaboration snapshot.
    /// They are history only and never advance the displayed review revision.
    #[serde(default)]
    pub pull_request_node_id: Option<String>,
    #[serde(default)]
    pub base_repository: Option<CheckRepositoryIdentity>,
    #[serde(default)]
    pub observed_head_sha: Option<String>,
    #[serde(default)]
    pub rollup_commit_sha: Option<String>,
    #[serde(default)]
    pub potential_merge_commit_sha: Option<String>,
    #[serde(default)]
    pub head_repository: Option<CheckRepositoryIdentity>,
    #[serde(default)]
    pub rollup_repository: Option<CheckRepositoryIdentity>,
    #[serde(default)]
    pub potential_merge_commit_repository: Option<CheckRepositoryIdentity>,
    pub body: String,
    pub requested_reviewers: Vec<String>,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub merge_eligibility: MergeEligibility,
    pub issue_comments: Vec<IssueComment>,
    pub reviews: Vec<PullRequestReview>,
    pub review_threads: Vec<ReviewThread>,
    /// Historical snapshots for every reactable object returned by the
    /// bounded details read. Fresh capabilities inside each entry are never
    /// serialized, so a cached round-trip cannot arm a write.
    #[serde(default)]
    pub reactions: Vec<ReactionSubjectSnapshot>,
    pub checks: Vec<PullRequestCheck>,
    /// False when any activity connection was partial or hit an explicit cap.
    pub activity_complete: bool,
    /// False when the status/check context connection was partial or capped.
    pub checks_complete: bool,
    pub notice: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub previous_path: Option<String>,
    /// Exact filesystem bytes when a local path cannot be represented as UTF-8.
    #[serde(default)]
    pub raw_path: Option<Vec<u8>>,
    #[serde(default)]
    pub raw_previous_path: Option<Vec<u8>>,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    /// None means unavailable/binary/truncated, never an empty complete patch.
    pub patch: Option<String>,
    pub patch_complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comparison {
    pub revision: Revision,
    pub files: Vec<ChangedFile>,
    pub complete: bool,
    pub notice: Option<String>,
}

#[cfg(test)]
mod reaction_serialization_tests {
    use super::*;

    fn coordinates(id: &str) -> ProviderCoordinates {
        ProviderCoordinates {
            provider: "github".into(),
            host: "github.com".into(),
            owner: "owner".into(),
            repository: "repo".into(),
            pull_request: 7,
            remote_id: id.into(),
        }
    }

    #[test]
    fn cache_serialization_preserves_history_and_drops_fresh_reaction_capability() {
        let snapshot = ReactionSubjectSnapshot {
            kind: ReactableKind::PullRequestReviewComment,
            pull_request: coordinates("PR-node"),
            subject: coordinates("COMMENT-node"),
            parent_review: Some(coordinates("REVIEW-node")),
            content: "Exact historical body".into(),
            reactions: ReactionSnapshot {
                groups: vec![ReactionGroupSnapshot {
                    content: ReactionContent::Heart,
                    count: 3,
                    viewer_has_reacted: true,
                }],
                complete: true,
            },
            fresh_capability: Some(FreshReactionCapability {
                viewer: SelectedViewer {
                    node_id: "USER-node".into(),
                    login: "alice".into(),
                },
                viewer_can_react: true,
            }),
        };
        let encoded = serde_json::to_value(&snapshot).unwrap();
        assert!(encoded.get("fresh_capability").is_none());
        let mut injected = encoded.clone();
        injected["fresh_capability"] = serde_json::json!({
            "viewer": {"node_id": "FORGED", "login": "alice"},
            "viewer_can_react": true
        });
        let cached: ReactionSubjectSnapshot = serde_json::from_value(injected).unwrap();
        assert_eq!(cached.reactions, snapshot.reactions);
        assert_eq!(cached.content, snapshot.content);
        assert!(cached.fresh_capability.is_none());
    }

    #[test]
    fn old_details_without_reaction_field_loads_as_unknown_empty_history() {
        let old = serde_json::json!({
            "number": 7,
            "body": "old cached details",
            "requested_reviewers": [],
            "labels": [],
            "assignees": [],
            "merge_eligibility": {
                "state": "OPEN", "draft": false, "mergeable": "UNKNOWN",
                "merge_state_status": "UNKNOWN", "review_status": "UNKNOWN",
                "check_status": "UNKNOWN", "maintainer_can_modify": false,
                "can_rebase": false, "can_update_branch": false,
                "auto_merge_enabled": false, "in_merge_queue": false
            },
            "issue_comments": [],
            "reviews": [],
            "review_threads": [],
            "checks": [{
                "coordinates": {
                    "provider": "github", "host": "github.com", "owner": "owner",
                    "repository": "repo", "pull_request": 7, "remote_id": "CHECK-old"
                },
                "kind": "CHECK_RUN", "name": "old check", "status": "COMPLETED",
                "conclusion": "SUCCESS", "description": null,
                "details_url": "https://integrator.example/old", "started_at": null,
                "completed_at": null, "required": null
            }],
            "activity_complete": false,
            "checks_complete": false,
            "notice": "legacy v1 cache"
        });
        let details: PullRequestDetails = serde_json::from_value(old).unwrap();
        assert!(details.reactions.is_empty());
        assert!(!details.activity_complete);
        assert!(details.pull_request_node_id.is_none());
        assert!(details.base_repository.is_none());
        assert!(details.observed_head_sha.is_none());
        assert!(details.potential_merge_commit_repository.is_none());
        assert_eq!(details.checks[0].required, None);
        assert_eq!(details.checks[0].sha_class, CheckShaClass::Unknown);
        assert_eq!(details.checks[0].actions_linkage, ActionsLinkage::Unknown);
        assert!(details.checks[0].suite.is_none());
    }
}
