//! Personal workspace state and account-partitioned offline data.
use crate::{
    comparisons::{ComparisonRequest, LocalFileLoadPlan},
    domain::{Comparison, PullRequest, Repository, Revision},
    review::{ComparisonMode, ReviewSession},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const WORKSPACE_SCHEMA_VERSION: u32 = 1;
const WORKSPACE_FILE: &str = "workspace.json";
const MAX_WORKSPACE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TAB_PRESENTATION_BYTES: usize = 256 * 1024;
const MAX_POLL_BACKOFF_SHIFT: u32 = 5;
const MAX_REVIEW_CONTEXT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filter {
    pub search: String,
    pub author: String,
    pub reviewer: String,
    pub assignee: String,
    pub label: String,
    pub draft: Option<bool>,
    pub review_status: String,
    pub check_status: String,
    pub target_branch: String,
    pub source_branch: String,
    pub personal: PersonalFilter,
    /// open, closed, merged or all.
    pub state: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PersonalFilter {
    #[default]
    All,
    ReviewRequested,
    Own,
    /// Known author, requested reviewer, assignee, commenter or review author.
    /// The UI must disclose incomplete results when participant reads are capped.
    Participating,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupBy {
    Repository,
    TargetBranch,
    SourceBranch,
    SourcePrefix(String),
    Stack,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedView {
    pub name: String,
    pub filter: Filter,
    pub groups: Vec<GroupBy>,
}
impl Default for SavedView {
    fn default() -> Self {
        Self {
            name: "All open pull requests".into(),
            filter: Filter::default(),
            groups: vec![GroupBy::Repository],
        }
    }
}
impl Filter {
    pub fn matches(&self, pr: &PullRequest, login: &str) -> bool {
        let state = if self.state.is_empty() {
            "open"
        } else {
            &self.state
        };
        let search = self.search.to_lowercase();
        let includes = |items: &[String], value: &str| {
            value.is_empty() || items.iter().any(|s| s.eq_ignore_ascii_case(value))
        };
        let equals = |actual: &str, expected: &str| {
            expected.is_empty() || actual.eq_ignore_ascii_case(expected)
        };
        (state == "all" || pr.state.eq_ignore_ascii_case(state))
            && (search.is_empty()
                || format!(
                    "{} #{} {} {}",
                    pr.title, pr.number, pr.source_branch, pr.target_branch
                )
                .to_lowercase()
                .contains(&search))
            && equals(&pr.author, &self.author)
            && includes(&pr.reviewers, &self.reviewer)
            && includes(&pr.assignees, &self.assignee)
            && includes(&pr.labels, &self.label)
            && self.draft.is_none_or(|draft| pr.draft == draft)
            && equals(&pr.review_status, &self.review_status)
            && equals(&pr.check_status, &self.check_status)
            && (self.target_branch.is_empty() || pr.target_branch == self.target_branch)
            && (self.source_branch.is_empty() || pr.source_branch == self.source_branch)
            && match self.personal {
                PersonalFilter::All => true,
                PersonalFilter::ReviewRequested => {
                    !login.is_empty() && includes(&pr.reviewers, login)
                }
                PersonalFilter::Own => !login.is_empty() && pr.author.eq_ignore_ascii_case(login),
                PersonalFilter::Participating => {
                    !login.is_empty()
                        && (pr.author.eq_ignore_ascii_case(login)
                            || includes(&pr.reviewers, login)
                            || includes(&pr.assignees, login)
                            || includes(&pr.participants, login))
                }
            }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabState {
    pub repository_key: String,
    pub number: u64,
    pub revision: Revision,
    /// Last observed provider metadata for presenting this exact saved tab.
    /// This is not lifecycle, details, pending-review, or write authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequest>,
    pub selected_file: Option<String>,
    pub scroll_offset: f32,
    pub diff_mode: String,
}

#[derive(Clone, Debug)]
pub struct RestoredWorkspaceTab {
    pub saved_index: usize,
    pub repository: Repository,
    pub pull_request: PullRequest,
    pub context: PersistedComparisonContext,
}

#[derive(Clone, Debug)]
pub struct WorkspaceRestoreNotice {
    pub saved_index: Option<usize>,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct WorkspaceRestorePlan {
    pub tabs: Vec<RestoredWorkspaceTab>,
    pub active_tab: Option<usize>,
    pub notices: Vec<WorkspaceRestoreNotice>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub schema_version: u32,
    pub repositories: Vec<Repository>,
    pub views: Vec<SavedView>,
    pub selected_view: usize,
    pub tabs: Vec<TabState>,
    pub active_tab: Option<usize>,
}
impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            schema_version: WORKSPACE_SCHEMA_VERSION,
            repositories: vec![],
            views: vec![SavedView::default()],
            selected_view: 0,
            tabs: vec![],
            active_tab: None,
        }
    }
}
impl WorkspaceState {
    pub fn add_repository(&mut self, repository: Repository) {
        if !self
            .repositories
            .iter()
            .any(|r| r.cache_key() == repository.cache_key())
        {
            self.repositories.push(repository);
        }
    }
    pub fn view(&self) -> SavedView {
        self.views
            .get(self.selected_view)
            .cloned()
            .unwrap_or_default()
    }

    /// Merge live tabs into their retained saved slots. Unavailable startup
    /// entries remain durable until that exact identity is opened or closed.
    pub fn merge_tabs(
        &mut self,
        live_tabs: Vec<TabState>,
        closed_tabs: &BTreeSet<(String, u64)>,
        active_identity: Option<(String, u64)>,
    ) {
        let retained_active = self
            .active_tab
            .and_then(|index| self.tabs.get(index))
            .map(|tab| (tab.repository_key.clone(), tab.number));
        let mut live_tabs = live_tabs.into_iter().map(Some).collect::<Vec<_>>();
        let mut merged = Vec::with_capacity(self.tabs.len() + live_tabs.len());
        for saved in std::mem::take(&mut self.tabs) {
            let identity = (saved.repository_key.clone(), saved.number);
            if closed_tabs.contains(&identity) {
                continue;
            }
            let live = live_tabs.iter_mut().find(|candidate| {
                candidate.as_ref().is_some_and(|candidate| {
                    candidate.repository_key == identity.0 && candidate.number == identity.1
                })
            });
            merged.push(live.and_then(Option::take).unwrap_or(saved));
        }
        merged.extend(live_tabs.into_iter().flatten());
        let selected = active_identity.or(retained_active);
        self.active_tab = selected.and_then(|(repository_key, number)| {
            merged
                .iter()
                .position(|tab| tab.repository_key == repository_key && tab.number == number)
        });
        self.tabs = merged;
    }
}

/// Local-only data store. Cache keys include explicit repository account identity.
#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedComparisonContext {
    /// The immutable published PR snapshot. This is never inferred from the
    /// narrower pair in `selected_session`.
    pub canonical_full_revision: Revision,
    /// Full-PR progress and the actual canonical patch used for comment mapping.
    pub canonical_session: ReviewSession,
    /// Lossless request identity, including unavailable since-review requests.
    pub selected_request: ComparisonRequest,
    /// The currently displayed pair and its independent navigation/progress.
    pub selected_session: ReviewSession,
    /// Exact lazy-load semantics for the selected local pair.
    pub local_file_load: Option<LocalFileLoadPlan>,
    /// Bounded, losslessly keyed progress for comparisons inspected during this
    /// pinned canonical review. Full-PR progress remains in `canonical_session`.
    #[serde(default)]
    pub request_sessions: Vec<PersistedComparisonSession>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedComparisonSession {
    pub request: ComparisonRequest,
    pub session: ReviewSession,
    pub local_file_load: Option<LocalFileLoadPlan>,
}

pub const MAX_PERSISTED_COMPARISON_SESSIONS: usize = 8;

impl PersistedComparisonContext {
    pub fn full(session: ReviewSession) -> Self {
        Self {
            canonical_full_revision: session.revision().clone(),
            canonical_session: session.clone(),
            selected_request: ComparisonRequest::FullPullRequest,
            selected_session: session,
            local_file_load: None,
            request_sessions: Vec::new(),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.canonical_session.revision() != &self.canonical_full_revision
            || self.canonical_session.metadata().mode != ComparisonMode::FullPullRequest
        {
            bail!("Stored canonical review session is not the pinned full pull request");
        }
        if matches!(self.selected_request, ComparisonRequest::FullPullRequest)
            && (self.selected_session.revision() != &self.canonical_full_revision
                || self.selected_session.metadata().mode != ComparisonMode::FullPullRequest)
        {
            bail!("Stored full selection does not match the canonical pull request");
        }
        match (
            &self.selected_request,
            &self.selected_session.metadata().mode,
        ) {
            (ComparisonRequest::FullPullRequest, ComparisonMode::FullPullRequest)
            | (ComparisonRequest::CommitRange { .. }, ComparisonMode::CommitRange)
            | (
                ComparisonRequest::SinceLastReview { .. },
                ComparisonMode::SinceLastReview { .. } | ComparisonMode::FullPullRequest,
            ) => {}
            (ComparisonRequest::Commit { sha }, ComparisonMode::Commit { sha: selected_sha })
                if sha == selected_sha && self.selected_session.revision().head_sha == *sha => {}
            _ => bail!("Stored selected request and comparison mode do not agree"),
        }
        if let ComparisonRequest::CommitRange { last_sha, .. } = &self.selected_request
            && self.selected_session.revision().head_sha != *last_sha
        {
            bail!("Stored range head does not match its selected endpoint");
        }
        if matches!(
            self.selected_request,
            ComparisonRequest::SinceLastReview { .. }
        ) && self.selected_session.revision().head_sha != self.canonical_full_revision.head_sha
        {
            bail!("Stored since-review selection does not end at the canonical head");
        }
        if let Some(plan) = &self.local_file_load
            && plan.revision != *self.selected_session.revision()
        {
            bail!("Stored local file-load plan does not match the selected comparison");
        }
        if self.request_sessions.len() > MAX_PERSISTED_COMPARISON_SESSIONS {
            bail!("Stored comparison progress exceeds the bounded request limit");
        }
        for saved in &self.request_sessions {
            if matches!(saved.request, ComparisonRequest::FullPullRequest) {
                bail!("Stored comparison progress duplicates canonical full progress");
            }
            validate_selected_session(
                &self.canonical_full_revision,
                &saved.request,
                &saved.session,
                saved.local_file_load.as_ref(),
            )?;
        }
        Ok(())
    }
}

fn validate_selected_session(
    canonical: &Revision,
    request: &ComparisonRequest,
    session: &ReviewSession,
    local_file_load: Option<&LocalFileLoadPlan>,
) -> Result<()> {
    match (request, &session.metadata().mode) {
        (ComparisonRequest::CommitRange { last_sha, .. }, ComparisonMode::CommitRange)
            if session.revision().head_sha == *last_sha => {}
        (ComparisonRequest::Commit { sha }, ComparisonMode::Commit { sha: selected_sha })
            if sha == selected_sha && session.revision().head_sha == *sha => {}
        (
            ComparisonRequest::SinceLastReview { .. },
            ComparisonMode::SinceLastReview { .. } | ComparisonMode::FullPullRequest,
        ) if session.revision().head_sha == canonical.head_sha => {}
        _ => bail!("Stored saved request and comparison mode do not agree"),
    }
    if let Some(plan) = local_file_load
        && plan.revision != *session.revision()
    {
        bail!("Stored saved request file-load plan does not match its comparison");
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct StoredReviewSession {
    schema_version: u32,
    repository_key: String,
    number: u64,
    #[serde(default)]
    session: Option<ReviewSession>,
    #[serde(default)]
    context: Option<PersistedComparisonContext>,
}
impl Store {
    pub fn open_default() -> Result<Self> {
        let home = std::env::var_os("HOME").context("Cannot locate application data directory")?;
        Self::open(PathBuf::from(home).join("Library/Application Support/cibergit"))
    }
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        Ok(Self { root })
    }
    fn cache_path(&self, repo: &Repository, key: &str) -> PathBuf {
        let digest = Sha256::digest(format!("{}\n{key}", repo.cache_key()));
        self.root.join(format!("{:x}.json", digest))
    }
    pub fn load_workspace(&self) -> Result<WorkspaceState> {
        let path = self.root.join(WORKSPACE_FILE);
        let bytes = match read_workspace_bounded(&path) {
            Ok(bytes) => bytes,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(WorkspaceState::default());
            }
            Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
        };
        decode_workspace(&bytes).with_context(|| format!("Load {}", path.display()))
    }
    pub fn save_workspace(&self, state: &WorkspaceState) -> Result<()> {
        if state.schema_version != WORKSPACE_SCHEMA_VERSION {
            bail!("Workspace was saved by an unsupported application version");
        }
        validate_workspace(state)?;
        let encoded = serde_json::to_vec_pretty(state)?;
        if encoded.len() > MAX_WORKSPACE_BYTES {
            bail!("Serialized workspace exceeds the {MAX_WORKSPACE_BYTES}-byte persistence limit");
        }
        let path = self.root.join(WORKSPACE_FILE);
        if path.exists() {
            let existing = read_workspace_bounded(&path)
                .with_context(|| format!("Read {}", path.display()))?;
            if let Err(error) = decode_workspace(&existing) {
                bail!("Refusing to overwrite unreadable or unsupported workspace data: {error:#}");
            }
        }
        write_bytes(&path, &encoded)
    }
    pub fn save_pull_requests(&self, repo: &Repository, prs: &[PullRequest]) -> Result<()> {
        write_json(&self.cache_path(repo, "prs"), &prs)
    }
    pub fn load_pull_requests(&self, repo: &Repository) -> Result<Vec<PullRequest>> {
        read_json(&self.cache_path(repo, "prs"))
    }
    /// Load every saved tab needed for ordinary startup without consulting a
    /// provider. Call this on a background executor.
    pub fn load_workspace_restore(&self, workspace: &WorkspaceState) -> WorkspaceRestorePlan {
        let mut plan = WorkspaceRestorePlan::default();
        let mut seen = BTreeSet::new();
        if workspace
            .active_tab
            .is_some_and(|index| index >= workspace.tabs.len())
        {
            plan.notices.push(WorkspaceRestoreNotice {
                saved_index: None,
                message: "Saved active tab is outside the saved tab list".into(),
            });
        }
        for (saved_index, saved) in workspace.tabs.iter().enumerate() {
            let identity = (saved.repository_key.clone(), saved.number);
            if !seen.insert(identity.clone()) {
                plan.notices.push(WorkspaceRestoreNotice {
                    saved_index: Some(saved_index),
                    message: format!(
                        "Saved tab #{} duplicates an earlier repository/account/PR identity",
                        saved.number
                    ),
                });
                continue;
            }
            let Some(repository) = workspace
                .repositories
                .iter()
                .find(|repository| repository.cache_key() == saved.repository_key)
                .cloned()
            else {
                plan.notices.push(WorkspaceRestoreNotice {
                    saved_index: Some(saved_index),
                    message: format!(
                        "Saved tab #{} no longer matches an exact configured repository/account",
                        saved.number
                    ),
                });
                continue;
            };
            let pull_request = match &saved.pull_request {
                Some(pull_request) if pull_request.number == saved.number => pull_request.clone(),
                Some(_) => {
                    plan.notices.push(WorkspaceRestoreNotice {
                        saved_index: Some(saved_index),
                        message: format!(
                            "Saved tab #{} has mismatched pull-request presentation metadata",
                            saved.number
                        ),
                    });
                    continue;
                }
                None => match self.load_pull_requests(&repository).ok().and_then(|pulls| {
                    pulls
                        .into_iter()
                        .find(|pull_request| pull_request.number == saved.number)
                }) {
                    Some(pull_request) => pull_request,
                    None => {
                        plan.notices.push(WorkspaceRestoreNotice {
                            saved_index: Some(saved_index),
                            message: format!(
                                "Saved tab #{} has no stored presentation metadata; open it explicitly once while reachable to repair this legacy entry",
                                saved.number
                            ),
                        });
                        continue;
                    }
                },
            };
            let context = match self.load_review_context(&repository, saved.number) {
                Ok(context) if context.canonical_full_revision == saved.revision => context,
                Ok(_) => {
                    plan.notices.push(WorkspaceRestoreNotice {
                        saved_index: Some(saved_index),
                        message: format!(
                            "Saved tab #{} does not match its pinned canonical review session",
                            saved.number
                        ),
                    });
                    continue;
                }
                Err(error) => {
                    plan.notices.push(WorkspaceRestoreNotice {
                        saved_index: Some(saved_index),
                        message: format!(
                            "Saved tab #{} review session was refused and preserved: {error:#}",
                            saved.number
                        ),
                    });
                    continue;
                }
            };
            plan.tabs.push(RestoredWorkspaceTab {
                saved_index,
                repository,
                pull_request,
                context,
            });
        }
        plan.active_tab = workspace.active_tab.and_then(|saved_index| {
            plan.tabs
                .iter()
                .position(|tab| tab.saved_index == saved_index)
        });
        if plan.active_tab.is_none() && !plan.tabs.is_empty() {
            plan.active_tab = Some(0);
        }
        plan
    }
    pub fn save_comparison(
        &self,
        repo: &Repository,
        number: u64,
        comparison: &Comparison,
    ) -> Result<()> {
        write_json(
            &self.cache_path(
                repo,
                &format!(
                    "pr/{number}/{}/{}",
                    comparison.revision.base_sha, comparison.revision.head_sha
                ),
            ),
            comparison,
        )
    }
    pub fn load_comparison(
        &self,
        repo: &Repository,
        number: u64,
        revision: &Revision,
    ) -> Result<Comparison> {
        let comparison: Comparison = read_json(&self.cache_path(
            repo,
            &format!("pr/{number}/{}/{}", revision.base_sha, revision.head_sha),
        ))?;
        if comparison.revision != *revision {
            bail!("Cached comparison revision does not match the requested base/head");
        }
        Ok(comparison)
    }
    /// Drafts are separate from disposable comparison caches and never auto-published.
    pub fn save_draft(&self, repo: &Repository, number: u64, key: &str, text: &str) -> Result<()> {
        write_json(
            &self.cache_path(repo, &format!("draft/{number}/{key}")),
            &text,
        )
    }
    pub fn load_draft(&self, repo: &Repository, number: u64, key: &str) -> Result<String> {
        read_json(&self.cache_path(repo, &format!("draft/{number}/{key}")))
    }
    /// Persist the displayed comparison and progress together. A poll response
    /// must not replace the session that the user has explicitly selected.
    pub fn save_review_session(
        &self,
        repo: &Repository,
        number: u64,
        session: &ReviewSession,
    ) -> Result<()> {
        let path = self.cache_path(repo, &format!("review-session/{number}"));
        if path.try_exists()? {
            self.load_review_session(repo, number)
                .context("Refusing to overwrite unreadable or unsupported review progress")?;
        }
        write_json_bounded(
            &path,
            &StoredReviewSession {
                schema_version: 1,
                repository_key: repo.cache_key(),
                number,
                session: Some(session.clone()),
                context: None,
            },
            MAX_REVIEW_CONTEXT_BYTES,
        )
    }
    pub fn load_review_session(&self, repo: &Repository, number: u64) -> Result<ReviewSession> {
        let stored: StoredReviewSession =
            read_json(&self.cache_path(repo, &format!("review-session/{number}")))?;
        if stored.repository_key != repo.cache_key() || stored.number != number {
            bail!("Stored review progress does not match the requested repository, account and PR");
        }
        match stored.schema_version {
            1 => stored
                .session
                .context("Legacy review progress is missing its session"),
            2 => {
                let context = stored
                    .context
                    .context("Review progress is missing its comparison context")?;
                context.validate()?;
                Ok(context.selected_session)
            }
            _ => bail!("Review progress was saved by an unsupported application version"),
        }
    }
    /// Atomically persist canonical and selected identities/progress in one
    /// bounded record. Callers serialize writes for the repository/account/PR key.
    pub fn save_review_context(
        &self,
        repo: &Repository,
        number: u64,
        context: &PersistedComparisonContext,
    ) -> Result<()> {
        context.validate()?;
        let path = self.cache_path(repo, &format!("review-session/{number}"));
        if path.try_exists()? {
            self.load_review_context(repo, number)
                .context("Refusing to overwrite unreadable or unsupported review progress")?;
        }
        write_json_bounded(
            &path,
            &StoredReviewSession {
                schema_version: 2,
                repository_key: repo.cache_key(),
                number,
                session: None,
                context: Some(context.clone()),
            },
            MAX_REVIEW_CONTEXT_BYTES,
        )
    }
    pub fn load_review_context(
        &self,
        repo: &Repository,
        number: u64,
    ) -> Result<PersistedComparisonContext> {
        let stored: StoredReviewSession =
            read_json(&self.cache_path(repo, &format!("review-session/{number}")))?;
        if stored.repository_key != repo.cache_key() || stored.number != number {
            bail!("Stored review progress does not match the requested repository, account and PR");
        }
        let context = match stored.schema_version {
            1 => {
                let session = stored
                    .session
                    .context("Legacy review progress is missing its session")?;
                if session.metadata().mode != ComparisonMode::FullPullRequest {
                    bail!(
                        "Legacy selected comparison cannot be proven to be the canonical full pull request"
                    );
                }
                PersistedComparisonContext::full(session)
            }
            2 => stored
                .context
                .context("Review progress is missing its comparison context")?,
            _ => bail!("Review progress was saved by an unsupported application version"),
        };
        context.validate()?;
        Ok(context)
    }
}
fn decode_workspace(bytes: &[u8]) -> Result<WorkspaceState> {
    if bytes.len() > MAX_WORKSPACE_BYTES {
        bail!("Workspace exceeds the {MAX_WORKSPACE_BYTES}-byte persistence limit");
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let version = value
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .context("Workspace is missing schema_version")?;
    if version != u64::from(WORKSPACE_SCHEMA_VERSION) {
        bail!("Workspace was saved by an unsupported application version");
    }
    let workspace: WorkspaceState = serde_json::from_value(value)?;
    validate_workspace(&workspace)?;
    Ok(workspace)
}

fn read_workspace_bounded(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take((MAX_WORKSPACE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_WORKSPACE_BYTES {
        bail!("Workspace exceeds the {MAX_WORKSPACE_BYTES}-byte persistence limit");
    }
    Ok(bytes)
}

fn validate_workspace(workspace: &WorkspaceState) -> Result<()> {
    for tab in &workspace.tabs {
        if let Some(pull_request) = &tab.pull_request {
            let size = serde_json::to_vec(pull_request)?.len();
            if size > MAX_TAB_PRESENTATION_BYTES {
                bail!(
                    "Saved tab #{} presentation metadata exceeds the {}-byte persistence limit",
                    tab.number,
                    MAX_TAB_PRESENTATION_BYTES
                );
            }
        }
    }
    Ok(())
}
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(
        &fs::read(path).with_context(|| format!("Read {}", path.display()))?,
    )?)
}
fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    write_bytes(path, &serde_json::to_vec_pretty(value)?)
}
fn write_json_bounded<T: Serialize + ?Sized>(path: &Path, value: &T, maximum: usize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    if bytes.len() > maximum {
        bail!("Serialized review context exceeds the {maximum}-byte persistence limit");
    }
    write_bytes(path, &bytes)
}
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temp = path.with_extension(format!("{}.{}.tmp", std::process::id(), stamp));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            let dir = fs::File::open(parent)?;
            dir.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Groups are ordered paths; ambiguous branch dependencies remain explicitly labelled.
pub fn group_path(
    repo: &Repository,
    pr: &PullRequest,
    groups: &[GroupBy],
    prs: &[PullRequest],
) -> Vec<String> {
    groups
        .iter()
        .map(|group| match group {
            GroupBy::Repository => format!("{} · {}", repo.full_name(), repo.account.login),
            GroupBy::TargetBranch => pr.target_branch.clone(),
            GroupBy::SourceBranch => pr.source_branch.clone(),
            GroupBy::SourcePrefix(prefix) => {
                if pr.source_branch.starts_with(prefix) {
                    prefix.clone()
                } else {
                    "Other branches".into()
                }
            }
            GroupBy::Stack => inferred_stack_group(pr, prs),
        })
        .collect()
}

fn inferred_stack_group(pr: &PullRequest, prs: &[PullRequest]) -> String {
    let mut current = pr;
    let mut seen = BTreeSet::from([pr.number]);
    loop {
        if current.source_branch == current.target_branch {
            return "Cyclic stack".into();
        }
        let parents: Vec<_> = prs
            .iter()
            .filter(|candidate| {
                candidate.number != current.number
                    && candidate.source_branch == current.target_branch
            })
            .collect();
        match parents.as_slice() {
            [] => return format!("{} (inferred)", current.source_branch),
            [parent] => {
                if !seen.insert(parent.number) {
                    return "Cyclic stack".into();
                }
                current = parent;
            }
            _ => return "Ambiguous stack".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PollSchedule {
    pub active_pr: Duration,
    pub sidebar: Duration,
    failures: BTreeMap<String, u32>,
}
impl Default for PollSchedule {
    fn default() -> Self {
        Self {
            active_pr: Duration::from_secs(15),
            sidebar: Duration::from_secs(60),
            failures: BTreeMap::new(),
        }
    }
}
impl PollSchedule {
    pub fn delay(&self, key: &str, active_pr: bool, focused: bool) -> Duration {
        let base = if active_pr {
            self.active_pr
        } else {
            self.sidebar
        };
        let shift = self
            .failures
            .get(key)
            .copied()
            .unwrap_or(0)
            .min(MAX_POLL_BACKOFF_SHIFT);
        base * (1u32 << shift) * if focused { 1 } else { 4 }
    }
    pub fn failed(&mut self, key: &str) {
        let count = self.failures.entry(key.into()).or_default();
        *count = count.saturating_add(1).min(MAX_POLL_BACKOFF_SHIFT);
    }
    pub fn succeeded(&mut self, key: &str) {
        self.failures.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn repo(login: &str) -> Repository {
        Repository {
            host: "github.com".into(),
            owner: "o".into(),
            name: "r".into(),
            account: crate::domain::Account {
                host: "github.com".into(),
                login: login.into(),
            },
            local_path: None,
        }
    }
    #[test]
    fn cache_and_drafts_are_account_partitioned() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .save_draft(&repo("one"), 1, "line", "unfinished")
            .unwrap();
        assert_eq!(
            store.load_draft(&repo("one"), 1, "line").unwrap(),
            "unfinished"
        );
        assert!(store.load_draft(&repo("two"), 1, "line").is_err());
        let mut state = WorkspaceState::default();
        state.add_repository(repo("one"));
        state.add_repository(repo("two"));
        store.save_workspace(&state).unwrap();
        assert_eq!(store.load_workspace().unwrap().repositories.len(), 2);
    }
    #[test]
    fn default_view_is_open_and_personal_filters_use_identity() {
        let pr = PullRequest {
            state: "OPEN".into(),
            author: "me".into(),
            title: "Improve parser".into(),
            number: 42,
            ..Default::default()
        };
        assert!(Filter::default().matches(&pr, "me"));
        assert!(
            !Filter {
                personal: PersonalFilter::ReviewRequested,
                ..Default::default()
            }
            .matches(&pr, "me")
        );
        assert!(
            Filter {
                search: "#42".into(),
                personal: PersonalFilter::Own,
                ..Default::default()
            }
            .matches(&pr, "me")
        );
    }
    #[test]
    fn backoff_does_not_change_poll_defaults() {
        let mut p = PollSchedule::default();
        assert_eq!(p.delay("pr", true, true), Duration::from_secs(15));
        p.failed("pr");
        assert_eq!(p.delay("pr", true, true), Duration::from_secs(30));
        p.succeeded("pr");
        assert_eq!(p.delay("pr", true, true), Duration::from_secs(15));
    }
}
