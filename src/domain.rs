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
    pub url: String,
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
pub struct ReviewThread {
    pub coordinates: ProviderCoordinates,
    pub path: String,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestCheck {
    pub coordinates: ProviderCoordinates,
    pub kind: CheckKind,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub description: Option<String>,
    pub details_url: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub required: Option<bool>,
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
    pub body: String,
    pub requested_reviewers: Vec<String>,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub merge_eligibility: MergeEligibility,
    pub issue_comments: Vec<IssueComment>,
    pub reviews: Vec<PullRequestReview>,
    pub review_threads: Vec<ReviewThread>,
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
