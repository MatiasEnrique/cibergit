//! Personal workspace state and account-partitioned offline data.
use crate::domain::{Comparison, PullRequest, Repository, Revision};
use crate::review::ReviewSession;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const WORKSPACE_SCHEMA_VERSION: u32 = 1;
const WORKSPACE_FILE: &str = "workspace.json";
const MAX_POLL_BACKOFF_SHIFT: u32 = 5;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
    /// Author, requested reviewer, or assignee. Comment and submitted-review
    /// participation is not represented on `PullRequest`, so this is not full
    /// GitHub participating-search parity.
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
            && equals(&pr.target_branch, &self.target_branch)
            && equals(&pr.source_branch, &self.source_branch)
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
                            || includes(&pr.assignees, login))
                }
            }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabState {
    pub repository_key: String,
    pub number: u64,
    pub revision: Revision,
    pub selected_file: Option<String>,
    pub scroll_offset: f32,
    pub diff_mode: String,
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
}

/// Local-only data store. Cache keys include explicit repository account identity.
#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}
#[derive(Serialize, Deserialize)]
struct StoredReviewSession {
    schema_version: u32,
    repository_key: String,
    number: u64,
    session: ReviewSession,
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
        if !path.exists() {
            return Ok(WorkspaceState::default());
        }
        decode_workspace(&fs::read(&path).with_context(|| format!("Read {}", path.display()))?)
            .with_context(|| format!("Load {}", path.display()))
    }
    pub fn save_workspace(&self, state: &WorkspaceState) -> Result<()> {
        if state.schema_version != WORKSPACE_SCHEMA_VERSION {
            bail!("Workspace was saved by an unsupported application version");
        }
        let path = self.root.join(WORKSPACE_FILE);
        if path.exists() {
            let existing = fs::read(&path).with_context(|| format!("Read {}", path.display()))?;
            if let Err(error) = decode_workspace(&existing) {
                bail!("Refusing to overwrite unreadable or unsupported workspace data: {error:#}");
            }
        }
        write_json(&path, state)
    }
    pub fn save_pull_requests(&self, repo: &Repository, prs: &[PullRequest]) -> Result<()> {
        write_json(&self.cache_path(repo, "prs"), &prs)
    }
    pub fn load_pull_requests(&self, repo: &Repository) -> Result<Vec<PullRequest>> {
        read_json(&self.cache_path(repo, "prs"))
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
        write_json(
            &path,
            &StoredReviewSession {
                schema_version: 1,
                repository_key: repo.cache_key(),
                number,
                session: session.clone(),
            },
        )
    }
    pub fn load_review_session(&self, repo: &Repository, number: u64) -> Result<ReviewSession> {
        let stored: StoredReviewSession =
            read_json(&self.cache_path(repo, &format!("review-session/{number}")))?;
        if stored.schema_version != 1 {
            bail!("Review progress was saved by an unsupported application version");
        }
        if stored.repository_key != repo.cache_key() || stored.number != number {
            bail!("Stored review progress does not match the requested repository, account and PR");
        }
        Ok(stored.session)
    }
}
fn decode_workspace(bytes: &[u8]) -> Result<WorkspaceState> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let version = value
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .context("Workspace is missing schema_version")?;
    if version != u64::from(WORKSPACE_SCHEMA_VERSION) {
        bail!("Workspace was saved by an unsupported application version");
    }
    Ok(serde_json::from_value(value)?)
}
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(
        &fs::read(path).with_context(|| format!("Read {}", path.display()))?,
    )?)
}
fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temp = path.with_extension(format!("{}.{}.tmp", std::process::id(), stamp));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
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
