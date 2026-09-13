//! Read-only GitHub.com transport. Call synchronous methods on a background task.
//!
//! Credentials live only in a private child environment; no global login is changed.
//! Comparisons use GitHub's three-dot (merge-base to head) PR semantics. REST caps
//! compare files at 300 and current PR files at 3,000; see `comparison` for limits.
use crate::domain::{Account, ChangedFile, Comparison, PullRequest, Repository, Revision};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const HOST: &str = "github.com";
const API_VERSION: &str = "X-GitHub-Api-Version: 2026-03-10";
const PAGE_SIZE: usize = 100;
const MAX_PR_PAGES: usize = 100;
const MAX_FILE_PAGES: usize = 30;
const MAX_OPERATION_BYTES: usize = 64 * 1024 * 1024;

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
            for pull in pulls {
                pull.validate(repo, None)?;
                ensure!(
                    seen.insert(pull.number),
                    "PR list changed during pagination; refresh to retry"
                );
                let pull = pull.into_domain();
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
        Ok(Session::new(self).pull(repo, number)?.into_domain())
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
                if agrees {
                    files = current_files;
                    if files.len() as u64 != expected {
                        notices.push(format!("Incomplete file list: GitHub returned {} of {expected} files (PR files API limit: 3,000).", files.len()));
                    }
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

struct Session<'a> {
    provider: &'a GithubProvider,
    started: Instant,
    bytes: usize,
}
impl<'a> Session<'a> {
    fn new(provider: &'a GithubProvider) -> Self {
        Self {
            provider,
            started: Instant::now(),
            bytes: 0,
        }
    }
    fn get<T: serde::de::DeserializeOwned>(&mut self, endpoint: &str) -> Result<T> {
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
    timeout: Duration,
    output_limit: usize,
}
impl Default for Runner {
    fn default() -> Self {
        Self {
            gh: "gh".into(),
            git: "git".into(),
            timeout: Duration::from_secs(30),
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
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|_| {
            anyhow::anyhow!("Cannot start subprocess to {action}; check installed gh/Git")
        })?;
        let (tx, rx) = mpsc::channel();
        let limit = self.output_limit;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        for (is_stdout, pipe) in [
            (true, Box::new(stdout) as Box<dyn Read + Send>),
            (false, Box::new(stderr) as Box<dyn Read + Send>),
        ] {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = pipe.take(limit as u64 + 1).read_to_end(&mut bytes);
                let _ = tx.send((is_stdout, result, bytes));
            });
        }
        drop(tx);
        let start = Instant::now();
        let mut output = None;
        let mut pipes_done = 0;
        let result = loop {
            for (is_stdout, read, bytes) in rx.try_iter() {
                if read.is_err() || bytes.len() > limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "Subprocess output failed or exceeded limit while attempting to {action}"
                    );
                }
                pipes_done += 1;
                if is_stdout {
                    output = Some(bytes);
                }
            }
            match child.try_wait() {
                Ok(Some(status)) if pipes_done == 2 => {
                    if status.success() {
                        break Ok(output.unwrap_or_default());
                    }
                    break Err(anyhow::anyhow!(
                        "Failed to {action} (exit {}). Check authentication, permissions, rate limits, and connectivity; subprocess output withheld.",
                        status
                            .code()
                            .map_or_else(|| "signal".into(), |code| code.to_string())
                    ));
                }
                Err(_) => break Err(anyhow::anyhow!("Cannot wait for subprocess to {action}")),
                _ => {}
            }
            if start.elapsed() >= self.timeout {
                break Err(anyhow::anyhow!("Timed out attempting to {action}"));
            }
            thread::sleep(Duration::from_millis(5));
        };
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }
}

#[derive(Deserialize)]
struct ApiUser {
    login: String,
}
#[derive(Deserialize)]
struct ApiRepository {
    name: String,
    owner: ApiUser,
}
#[derive(Deserialize)]
struct ApiRef {
    sha: String,
    #[serde(rename = "ref")]
    branch: String,
    repo: Option<ApiRepository>,
}
#[derive(Deserialize)]
struct ApiLabel {
    name: String,
}
#[derive(Deserialize)]
struct ApiTeam {
    slug: String,
}
#[derive(Deserialize)]
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
            draft: self.draft,
            state: if self.merged_at.is_some() {
                "MERGED"
            } else if self.state == "open" {
                "OPEN"
            } else {
                "CLOSED"
            }
            .into(),
            // These require separate review/check reads, delivered in M2.
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
    fn step(path: &str, response: Value) -> Value {
        json!({"endpoint": path, "response": response})
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
                timeout: Duration::from_secs(5),
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
        let steps = vec![step("repos/owner/repo/pulls/1", pull(1, 1))];
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
        exhausted(&alice_dir, 1);
        exhausted(&bob_dir, 1);
        for dir in [&alice_dir, &bob_dir] {
            assert_eq!(fs::read_to_string(dir.path().join("tokens")).unwrap(), "1");
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
                step(
                    "repos/owner/repo/pulls?state=all&sort=created&direction=asc&per_page=100&page=2",
                    json!([merged]),
                ),
            ],
        );
        let pulls = provider.list_pull_requests(&repo("alice"), "ALL").unwrap();
        assert_eq!(pulls.len(), 101);
        assert_eq!(pulls[100].state, "MERGED");
        assert_eq!(pulls[0].body, "");
        assert_eq!(pulls[0].source_branch, "feature");
        assert_eq!(pulls[0].reviewers, ["reviewer", "team:maintainers"]);
        assert_eq!(pulls[0].check_status, "UNKNOWN");
        exhausted(&dir, 2);
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
}
