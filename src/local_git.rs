//! Installed-Git backend for an explicitly attached checkout.
//!
//! Reads are authoritative only when returned. Every mutation requires the
//! opaque guard from a prior [`LocalSnapshot`], is revalidated while holding a
//! process-local lock shared by linked worktrees, and requires a fresh read
//! after success. Git remains the authority and external tools do not share the
//! process-local lock.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        process::CommandExt,
    },
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const DEFAULT_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
const DEFAULT_INPUT_LIMIT: usize = 4 * 1024 * 1024;
const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandLimits {
    pub deadline: Duration,
    pub max_output_bytes: usize,
    pub max_input_bytes: usize,
}

impl Default for CommandLimits {
    fn default() -> Self {
        Self {
            deadline: DEFAULT_DEADLINE,
            max_output_bytes: DEFAULT_OUTPUT_LIMIT,
            max_input_bytes: DEFAULT_INPUT_LIMIT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeCertainty {
    /// The command could not have changed the repository.
    Certain,
    /// Git was started; inspect actual local/remote state before an explicit retry.
    Uncertain,
}

#[derive(Debug)]
pub enum LocalGitError {
    Io {
        context: &'static str,
        source: std::io::Error,
    },
    NotAWorktree,
    InvalidInput(&'static str),
    StaleSnapshot,
    RepositoryLocked {
        action: &'static str,
    },
    CommandFailed {
        action: &'static str,
        exit_code: Option<i32>,
    },
    TimedOut {
        action: &'static str,
        certainty: OutcomeCertainty,
    },
    OutputLimit {
        action: &'static str,
        certainty: OutcomeCertainty,
    },
    MalformedOutput(&'static str),
    PoisonedLock,
}

impl fmt::Display for LocalGitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, .. } => write!(formatter, "{context}"),
            Self::NotAWorktree => formatter.write_str("path is not inside a Git worktree"),
            Self::InvalidInput(reason) => write!(formatter, "invalid Git action input: {reason}"),
            Self::StaleSnapshot => formatter.write_str(
                "local Git state changed since it was displayed; refresh before retrying",
            ),
            Self::RepositoryLocked { action } => {
                write!(
                    formatter,
                    "Git {action} was blocked by an existing Git lock"
                )
            }
            Self::CommandFailed { action, exit_code } => {
                write!(formatter, "Git {action} failed (exit {exit_code:?})")
            }
            Self::TimedOut { action, certainty } => {
                write!(formatter, "Git {action} timed out ({certainty:?} outcome)")
            }
            Self::OutputLimit { action, certainty } => write!(
                formatter,
                "Git {action} exceeded its output limit ({certainty:?} outcome)"
            ),
            Self::MalformedOutput(reason) => write!(formatter, "malformed Git output: {reason}"),
            Self::PoisonedLock => formatter.write_str("local Git operation lock is unavailable"),
        }
    }
}

impl std::error::Error for LocalGitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, LocalGitError>;

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GitPath {
    /// Human-facing representation. Identity and commands always use `raw`.
    pub display: String,
    /// Complete repository-relative path bytes from Git's NUL-delimited output.
    pub raw: Vec<u8>,
}

impl fmt::Debug for GitPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitPath")
            .field("display", &self.display)
            .field("raw", &self.raw)
            .finish()
    }
}

impl GitPath {
    pub fn from_raw(raw: Vec<u8>) -> Result<Self> {
        validate_path(&raw)?;
        Ok(Self {
            display: display_path(&raw),
            raw,
        })
    }

    fn os_string(&self) -> OsString {
        OsString::from_vec(self.raw.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusEntry {
    pub path: GitPath,
    pub previous_path: Option<GitPath>,
    /// Porcelain-v2 index status code.
    pub index_status: char,
    /// Porcelain-v2 worktree status code.
    pub worktree_status: char,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeadState {
    Unborn { branch: String },
    Attached { branch: String, oid: String },
    Detached { oid: String },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RebaseState {
    Apply,
    Merge,
    #[default]
    None,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationState {
    pub merge: bool,
    pub rebase: RebaseState,
    pub cherry_pick: bool,
    pub revert: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SnapshotGuard([u8; 32]);

impl fmt::Debug for SnapshotGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SnapshotGuard(<opaque>)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalSnapshot {
    pub head: HeadState,
    pub upstream: Option<String>,
    pub upstream_oid: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    pub staged: Vec<StatusEntry>,
    pub unstaged: Vec<StatusEntry>,
    pub untracked: Vec<GitPath>,
    pub conflicts: Vec<StatusEntry>,
    pub operation: OperationState,
    pub guard: SnapshotGuard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffTarget {
    Staged,
    Worktree,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffContent {
    Text(String),
    BinaryMetadata,
    MediaMetadata,
    UnsupportedMetadata { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedDiff {
    pub path: GitPath,
    pub target: DiffTarget,
    pub content: DiffContent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteBranchObservation {
    pub remote: String,
    pub branch: String,
    pub oid: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationAction {
    Stage,
    Unstage,
    Commit,
    CreateBranch,
    SwitchBranch,
    Fetch,
    FastForwardPull,
    Push,
    ForcePushWithLease,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MutationReceipt {
    pub action: MutationAction,
    pub precondition: SnapshotGuard,
    /// The caller must install an authoritative post-action snapshot.
    pub refresh_required: bool,
    /// Present for lease-protected pushes and useful when reconciling uncertainty.
    pub expected_remote_oid: Option<String>,
}

impl MutationReceipt {
    fn new(
        action: MutationAction,
        precondition: &SnapshotGuard,
        expected_remote_oid: Option<String>,
    ) -> Self {
        Self {
            action,
            precondition: precondition.clone(),
            refresh_required: true,
            expected_remote_oid,
        }
    }
}

#[derive(Clone)]
pub struct LocalGit {
    root: PathBuf,
    git_dir: PathBuf,
    common_git_dir: PathBuf,
    limits: CommandLimits,
    write_lock: Arc<Mutex<()>>,
}

impl fmt::Debug for LocalGit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalGit")
            .field("root", &self.root)
            .field("git_dir", &self.git_dir)
            .field("common_git_dir", &self.common_git_dir)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

static COMMON_GIT_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

impl LocalGit {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_limits(path, CommandLimits::default())
    }

    pub fn open_with_limits(path: impl AsRef<Path>, limits: CommandLimits) -> Result<Self> {
        if limits.deadline.is_zero() || limits.max_output_bytes == 0 || limits.max_input_bytes == 0
        {
            return Err(LocalGitError::InvalidInput(
                "command limits must be greater than zero",
            ));
        }
        let start = fs::canonicalize(path).map_err(|source| LocalGitError::Io {
            context: "canonicalize attached checkout path",
            source,
        })?;
        let provisional = Self {
            root: start.clone(),
            git_dir: start.clone(),
            common_git_dir: start,
            limits,
            write_lock: Arc::new(Mutex::new(())),
        };
        let root = provisional
            .discover_path("discover worktree root", &["rev-parse", "--show-toplevel"])?;
        let git_dir = provisional.discover_path(
            "discover Git directory",
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        )?;
        let common_git_dir = provisional.discover_path(
            "discover common Git directory",
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let root = canonical_git_path(root, "canonicalize worktree root")?;
        let git_dir = canonical_git_path(git_dir, "canonicalize Git directory")?;
        let common_git_dir =
            canonical_git_path(common_git_dir, "canonicalize common Git directory")?;
        let write_lock = {
            let registry = COMMON_GIT_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
            let mut registry = registry.lock().map_err(|_| LocalGitError::PoisonedLock)?;
            registry
                .entry(common_git_dir.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        Ok(Self {
            root,
            git_dir,
            common_git_dir,
            limits,
            write_lock,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub fn common_git_dir(&self) -> &Path {
        &self.common_git_dir
    }

    pub fn snapshot(&self) -> Result<LocalSnapshot> {
        let status = self.run(
            "read status",
            os_args(&[
                "status",
                "--porcelain=v2",
                "-z",
                "--branch",
                "--untracked-files=all",
            ]),
            None,
            false,
            &[0],
        )?;
        let mut parsed = parse_status(&status)?;
        let operation = self.operation_state();
        // Branch and remote-tracking refs live in the common Git directory and
        // can change through another linked worktree without changing this
        // worktree's porcelain status. Include them in the mutation guard.
        let shared_refs = self.run(
            "read shared refs",
            os_args(&[
                "for-each-ref",
                "--sort=refname",
                "--format=%(refname)%00%(objectname)%00",
                "refs/heads",
                "refs/remotes",
            ]),
            None,
            false,
            &[0],
        )?;
        let upstream_oid = match parsed.upstream.as_deref() {
            Some(upstream) => self.optional_commit_oid(upstream)?,
            None => None,
        };
        let mut hash = Sha256::new();
        hash.update(self.root.as_os_str().as_bytes());
        hash.update([0]);
        hash.update(&status);
        hash.update([0]);
        hash.update(&shared_refs);
        hash.update([
            operation.merge as u8,
            operation.cherry_pick as u8,
            operation.revert as u8,
        ]);
        hash.update([match operation.rebase {
            RebaseState::None => 0,
            RebaseState::Apply => 1,
            RebaseState::Merge => 2,
        }]);
        if let Some(oid) = &upstream_oid {
            hash.update(oid.as_bytes());
        }
        parsed.upstream_oid = upstream_oid;
        parsed.operation = operation;
        parsed.guard = SnapshotGuard(hash.finalize().into());
        Ok(parsed)
    }

    pub fn selected_diff(&self, path: &GitPath, target: DiffTarget) -> Result<SelectedDiff> {
        validate_path(&path.raw)?;
        if is_media_path(&path.raw) {
            return Ok(SelectedDiff {
                path: path.clone(),
                target,
                content: DiffContent::MediaMetadata,
            });
        }
        if target == DiffTarget::Worktree {
            let full_path = self.root.join(path.os_string());
            if let Ok(metadata) = fs::symlink_metadata(&full_path) {
                if !metadata.file_type().is_file() {
                    return Ok(SelectedDiff {
                        path: path.clone(),
                        target,
                        content: DiffContent::UnsupportedMetadata {
                            reason: "local entry is not a regular file".into(),
                        },
                    });
                }
                if metadata.len() > self.limits.max_output_bytes as u64 {
                    return Ok(SelectedDiff {
                        path: path.clone(),
                        target,
                        content: DiffContent::UnsupportedMetadata {
                            reason: "local file exceeds the configured diff bound".into(),
                        },
                    });
                }
            }
        }
        let snapshot = (target == DiffTarget::Worktree)
            .then(|| self.snapshot())
            .transpose()?;
        let untracked = snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .untracked
                .iter()
                .any(|candidate| candidate.raw == path.raw)
        });
        let mut numstat_args = os_args(&["diff", "--no-ext-diff", "--no-textconv", "--numstat"]);
        if target == DiffTarget::Staged {
            numstat_args.push("--cached".into());
        }
        numstat_args.push("--".into());
        numstat_args.push(path.os_string());
        let mut numstat = self.run("classify selected diff", numstat_args, None, false, &[0])?;
        if numstat.is_empty() && untracked {
            let mut args = os_args(&[
                "diff",
                "--no-index",
                "--no-ext-diff",
                "--no-textconv",
                "--numstat",
                "--",
                "/dev/null",
            ]);
            args.push(path.os_string());
            numstat = self.run("classify untracked diff", args, None, false, &[0, 1])?;
        }
        if numstat
            .split(|byte| *byte == b'\n')
            .any(|line| line.starts_with(b"-\t-\t"))
        {
            return Ok(SelectedDiff {
                path: path.clone(),
                target,
                content: DiffContent::BinaryMetadata,
            });
        }

        let mut args = os_args(&["diff", "--no-ext-diff", "--no-textconv", "--full-index"]);
        if target == DiffTarget::Staged {
            args.push("--cached".into());
        }
        args.push("--".into());
        args.push(path.os_string());
        let mut output = self.run("read selected diff", args, None, false, &[0])?;
        // An untracked file is outside ordinary `git diff`; compare it to an
        // empty file without adding it to the index.
        if output.is_empty() && untracked {
            let mut args = os_args(&[
                "diff",
                "--no-index",
                "--no-ext-diff",
                "--no-textconv",
                "--full-index",
                "--",
                "/dev/null",
            ]);
            args.push(path.os_string());
            output = self.run("read untracked diff", args, None, false, &[0, 1])?;
        }
        let content = match String::from_utf8(output) {
            Ok(patch) => DiffContent::Text(patch),
            Err(_) => DiffContent::UnsupportedMetadata {
                reason: "diff is not valid UTF-8".into(),
            },
        };
        Ok(SelectedDiff {
            path: path.clone(),
            target,
            content,
        })
    }

    pub fn observe_remote_branch(
        &self,
        remote: &str,
        branch: &str,
    ) -> Result<RemoteBranchObservation> {
        self.validate_remote(remote)?;
        self.validate_branch(branch)?;
        let remote_ref = format!("refs/heads/{branch}");
        let output = self.run(
            "observe remote branch",
            vec![
                "ls-remote".into(),
                "--refs".into(),
                "--".into(),
                remote.into(),
                remote_ref.clone().into(),
            ],
            None,
            false,
            &[0],
        )?;
        let oid = if output.is_empty() {
            None
        } else {
            let line = output
                .split(|byte| *byte == b'\n')
                .find(|line| !line.is_empty())
                .ok_or(LocalGitError::MalformedOutput("empty ls-remote record"))?;
            let separator = line
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or(LocalGitError::MalformedOutput("invalid ls-remote record"))?;
            let (oid, name_with_tab) = line.split_at(separator);
            let name = &name_with_tab[1..];
            if name != remote_ref.as_bytes() {
                return Err(LocalGitError::MalformedOutput("unexpected ls-remote ref"));
            }
            let oid = std::str::from_utf8(oid)
                .map_err(|_| LocalGitError::MalformedOutput("non-UTF-8 remote object ID"))?;
            validate_oid(oid)?;
            Some(oid.to_owned())
        };
        Ok(RemoteBranchObservation {
            remote: remote.into(),
            branch: branch.into(),
            oid,
        })
    }

    pub fn stage(&self, paths: &[GitPath], guard: &SnapshotGuard) -> Result<MutationReceipt> {
        let input = pathspec_input(paths, self.limits.max_input_bytes)?;
        self.guarded(guard, MutationAction::Stage, None, |_| {
            self.run(
                "stage paths",
                os_args(&[
                    "add",
                    "--all",
                    "--pathspec-from-file=-",
                    "--pathspec-file-nul",
                ]),
                Some(&input),
                true,
                &[0],
            )?;
            Ok(())
        })
    }

    pub fn unstage(&self, paths: &[GitPath], guard: &SnapshotGuard) -> Result<MutationReceipt> {
        let input = pathspec_input(paths, self.limits.max_input_bytes)?;
        self.guarded(guard, MutationAction::Unstage, None, |snapshot| {
            let args = match &snapshot.head {
                HeadState::Unborn { .. } => os_args(&[
                    "rm",
                    "--cached",
                    "--ignore-unmatch",
                    "--pathspec-from-file=-",
                    "--pathspec-file-nul",
                ]),
                _ => os_args(&[
                    "reset",
                    "-q",
                    "HEAD",
                    "--pathspec-from-file=-",
                    "--pathspec-file-nul",
                ]),
            };
            self.run("unstage paths", args, Some(&input), true, &[0])?;
            Ok(())
        })
    }

    pub fn commit(&self, message: &str, guard: &SnapshotGuard) -> Result<MutationReceipt> {
        if message.is_empty()
            || message.as_bytes().contains(&0)
            || message.len() > self.limits.max_input_bytes
        {
            return Err(LocalGitError::InvalidInput(
                "commit message is empty or too large",
            ));
        }
        self.guarded(guard, MutationAction::Commit, None, |_| {
            self.run(
                "commit",
                vec!["commit".into(), "-m".into(), message.into()],
                None,
                true,
                &[0],
            )?;
            Ok(())
        })
    }

    pub fn create_branch(
        &self,
        branch: &str,
        start_oid: Option<&str>,
        guard: &SnapshotGuard,
    ) -> Result<MutationReceipt> {
        self.validate_branch(branch)?;
        if let Some(oid) = start_oid {
            validate_oid(oid)?;
            self.require_commit(oid)?;
        }
        self.guarded(guard, MutationAction::CreateBranch, None, |_| {
            let mut args = vec!["switch".into(), "-c".into(), branch.into()];
            if let Some(oid) = start_oid {
                args.push(oid.into());
            }
            self.run("create branch", args, None, true, &[0])?;
            Ok(())
        })
    }

    pub fn switch_branch(&self, branch: &str, guard: &SnapshotGuard) -> Result<MutationReceipt> {
        self.validate_branch(branch)?;
        self.guarded(guard, MutationAction::SwitchBranch, None, |_| {
            self.run(
                "switch branch",
                vec!["switch".into(), "--".into(), branch.into()],
                None,
                true,
                &[0],
            )?;
            Ok(())
        })
    }

    pub fn fetch(&self, remote: &str, guard: &SnapshotGuard) -> Result<MutationReceipt> {
        self.validate_remote(remote)?;
        self.guarded(guard, MutationAction::Fetch, None, |_| {
            self.run(
                "fetch",
                vec![
                    "fetch".into(),
                    "--no-auto-maintenance".into(),
                    "--".into(),
                    remote.into(),
                ],
                None,
                true,
                &[0],
            )?;
            Ok(())
        })
    }

    pub fn fast_forward_pull(
        &self,
        remote: &str,
        branch: &str,
        guard: &SnapshotGuard,
    ) -> Result<MutationReceipt> {
        self.validate_remote(remote)?;
        self.validate_branch(branch)?;
        let remote_ref = format!("refs/heads/{branch}");
        self.guarded(guard, MutationAction::FastForwardPull, None, |_| {
            self.run(
                "fast-forward pull",
                vec![
                    "pull".into(),
                    "--ff-only".into(),
                    "--no-rebase".into(),
                    "--no-edit".into(),
                    "--".into(),
                    remote.into(),
                    remote_ref.into(),
                ],
                None,
                true,
                &[0],
            )?;
            Ok(())
        })
    }

    pub fn push(
        &self,
        remote: &str,
        branch: &str,
        guard: &SnapshotGuard,
    ) -> Result<MutationReceipt> {
        self.validate_remote(remote)?;
        self.validate_branch(branch)?;
        let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
        self.guarded(guard, MutationAction::Push, None, |_| {
            self.run(
                "push",
                vec![
                    "push".into(),
                    "--porcelain".into(),
                    "--".into(),
                    remote.into(),
                    refspec.into(),
                ],
                None,
                true,
                &[0],
            )?;
            Ok(())
        })
    }

    pub fn force_push_with_lease(
        &self,
        remote: &str,
        branch: &str,
        expected_remote_oid: &str,
        guard: &SnapshotGuard,
    ) -> Result<MutationReceipt> {
        self.validate_remote(remote)?;
        self.validate_branch(branch)?;
        validate_oid(expected_remote_oid)?;
        let remote_ref = format!("refs/heads/{branch}");
        let lease = format!("--force-with-lease={remote_ref}:{expected_remote_oid}");
        let refspec = format!("{remote_ref}:{remote_ref}");
        self.guarded(
            guard,
            MutationAction::ForcePushWithLease,
            Some(expected_remote_oid.to_owned()),
            |_| {
                self.run(
                    "force push with lease",
                    vec![
                        "push".into(),
                        "--porcelain".into(),
                        lease.into(),
                        "--".into(),
                        remote.into(),
                        refspec.into(),
                    ],
                    None,
                    true,
                    &[0],
                )?;
                Ok(())
            },
        )
    }

    fn guarded(
        &self,
        expected: &SnapshotGuard,
        action: MutationAction,
        expected_remote_oid: Option<String>,
        operation: impl FnOnce(&LocalSnapshot) -> Result<()>,
    ) -> Result<MutationReceipt> {
        let _lock = self
            .write_lock
            .lock()
            .map_err(|_| LocalGitError::PoisonedLock)?;
        let actual = self.snapshot()?;
        if &actual.guard != expected {
            return Err(LocalGitError::StaleSnapshot);
        }
        operation(&actual)?;
        Ok(MutationReceipt::new(action, expected, expected_remote_oid))
    }

    fn operation_state(&self) -> OperationState {
        OperationState {
            merge: self.git_dir.join("MERGE_HEAD").is_file(),
            rebase: if self.git_dir.join("rebase-merge").is_dir() {
                RebaseState::Merge
            } else if self.git_dir.join("rebase-apply").is_dir() {
                RebaseState::Apply
            } else {
                RebaseState::None
            },
            cherry_pick: self.git_dir.join("CHERRY_PICK_HEAD").is_file(),
            revert: self.git_dir.join("REVERT_HEAD").is_file(),
        }
    }

    fn has_known_lock(&self) -> bool {
        [
            self.git_dir.join("index.lock"),
            self.git_dir.join("HEAD.lock"),
            self.common_git_dir.join("packed-refs.lock"),
        ]
        .iter()
        .any(|path| path.exists())
    }

    fn discover_path(&self, action: &'static str, args: &[&str]) -> Result<PathBuf> {
        let output = match self.run(action, os_args(args), None, false, &[0]) {
            Ok(output) => output,
            Err(LocalGitError::CommandFailed { .. }) => return Err(LocalGitError::NotAWorktree),
            Err(error) => return Err(error),
        };
        let bytes = strip_one_lf(output);
        if bytes.is_empty() {
            return Err(LocalGitError::NotAWorktree);
        }
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }

    fn optional_commit_oid(&self, revision: &str) -> Result<Option<String>> {
        let expression = format!("{revision}^{{commit}}");
        let result = self.run(
            "read upstream object ID",
            vec![
                "rev-parse".into(),
                "--verify".into(),
                "--end-of-options".into(),
                expression.into(),
            ],
            None,
            false,
            &[0, 1, 128],
        )?;
        let value = String::from_utf8(strip_one_lf(result))
            .map_err(|_| LocalGitError::MalformedOutput("non-UTF-8 object ID"))?;
        if value.is_empty() {
            Ok(None)
        } else {
            validate_oid(&value)?;
            Ok(Some(value))
        }
    }

    fn require_commit(&self, oid: &str) -> Result<()> {
        let output = self.run(
            "validate commit",
            vec!["cat-file".into(), "-t".into(), oid.into()],
            None,
            false,
            &[0],
        )?;
        if strip_one_lf(output) != b"commit" {
            return Err(LocalGitError::InvalidInput("start object is not a commit"));
        }
        Ok(())
    }

    fn validate_branch(&self, branch: &str) -> Result<()> {
        if branch.is_empty()
            || branch.len() > 1024
            || branch.starts_with('-')
            || branch.contains("@{")
            || branch
                .bytes()
                .any(|byte| byte == 0 || byte.is_ascii_control())
        {
            return Err(LocalGitError::InvalidInput("invalid branch name"));
        }
        self.run(
            "validate branch name",
            vec!["check-ref-format".into(), "--branch".into(), branch.into()],
            None,
            false,
            &[0],
        )?;
        Ok(())
    }

    fn validate_remote(&self, remote: &str) -> Result<()> {
        if remote.is_empty()
            || remote.len() > 1024
            || remote.starts_with('-')
            || remote
                .bytes()
                .any(|byte| byte == 0 || byte.is_ascii_control())
        {
            return Err(LocalGitError::InvalidInput("invalid remote name"));
        }
        let remotes = self.run("list remotes", os_args(&["remote"]), None, false, &[0])?;
        let found = remotes
            .split(|byte| *byte == b'\n')
            .any(|name| name == remote.as_bytes());
        if !found {
            return Err(LocalGitError::InvalidInput("unknown remote name"));
        }
        Ok(())
    }

    fn run(
        &self,
        action: &'static str,
        args: Vec<OsString>,
        input: Option<&[u8]>,
        mutation: bool,
        accepted_exit_codes: &[i32],
    ) -> Result<Vec<u8>> {
        if input.is_some_and(|bytes| bytes.len() > self.limits.max_input_bytes) {
            return Err(LocalGitError::InvalidInput(
                "Git input exceeds configured bound",
            ));
        }
        let certainty = if mutation {
            OutcomeCertainty::Uncertain
        } else {
            OutcomeCertainty::Certain
        };
        let mut command = Command::new("git");
        for (name, _) in std::env::vars_os() {
            if name.as_bytes().starts_with(b"GIT_") {
                command.env_remove(name);
            }
        }
        command
            .arg("--no-pager")
            .arg("--literal-pathspecs")
            .args(["-c", "color.ui=false", "-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(&self.root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            // Transport and hook output can contain credentials or private URLs.
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().map_err(|source| LocalGitError::Io {
            context: "start installed Git",
            source,
        })?;
        let input_receiver = if let Some(input) = input {
            let mut stdin = child
                .stdin
                .take()
                .ok_or(LocalGitError::MalformedOutput("missing Git input pipe"))?;
            let input = input.to_vec();
            let (sender, receiver) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(stdin.write_all(&input));
            });
            Some(receiver)
        } else {
            None
        };
        let mut stdout = child
            .stdout
            .take()
            .ok_or(LocalGitError::MalformedOutput("missing Git output pipe"))?;
        let exceeded = Arc::new(AtomicBool::new(false));
        let reader_exceeded = exceeded.clone();
        let max_output = self.limits.max_output_bytes;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> std::io::Result<Vec<u8>> {
                let mut output = Vec::new();
                let mut chunk = [0_u8; 8192];
                loop {
                    let count = stdout.read(&mut chunk)?;
                    if count == 0 {
                        return Ok(output);
                    }
                    if output.len().saturating_add(count) > max_output {
                        reader_exceeded.store(true, Ordering::Release);
                        return Ok(output);
                    }
                    output.extend_from_slice(&chunk[..count]);
                }
            })();
            let _ = sender.send(result);
        });
        let started = Instant::now();
        let mut output = None;
        let mut input_finished = input_receiver.is_none();
        loop {
            if exceeded.load(Ordering::Acquire) {
                kill_process_group(&mut child);
                return Err(LocalGitError::OutputLimit { action, certainty });
            }
            if started.elapsed() >= self.limits.deadline {
                kill_process_group(&mut child);
                return Err(LocalGitError::TimedOut { action, certainty });
            }
            if output.is_none() {
                match receiver.try_recv() {
                    Ok(Ok(bytes)) => output = Some(bytes),
                    Ok(Err(source)) => {
                        kill_process_group(&mut child);
                        return Err(LocalGitError::Io {
                            context: "read installed Git output",
                            source,
                        });
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        kill_process_group(&mut child);
                        return Err(LocalGitError::MalformedOutput("Git output reader stopped"));
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if !input_finished && let Some(receiver) = &input_receiver {
                match receiver.try_recv() {
                    Ok(Ok(())) => input_finished = true,
                    Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                        kill_process_group(&mut child);
                        return Err(LocalGitError::CommandFailed {
                            action,
                            exit_code: None,
                        });
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if let Some(status) = child.try_wait().map_err(|source| LocalGitError::Io {
                context: "wait for installed Git",
                source,
            })? && let Some(output) = output.take()
                && input_finished
            {
                let code = status.code();
                if !code.is_some_and(|code| accepted_exit_codes.contains(&code)) {
                    if mutation && self.has_known_lock() {
                        return Err(LocalGitError::RepositoryLocked { action });
                    }
                    return Err(LocalGitError::CommandFailed {
                        action,
                        exit_code: code,
                    });
                }
                return Ok(output);
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }
}

fn parse_status(raw: &[u8]) -> Result<LocalSnapshot> {
    let mut head_oid = None;
    let mut branch = None;
    let mut upstream = None;
    let mut ahead = 0;
    let mut behind = 0;
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();
    let mut conflicts = Vec::new();
    let records: Vec<&[u8]> = raw.split(|byte| *byte == 0).collect();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.is_empty() {
            continue;
        }
        if let Some(value) = record.strip_prefix(b"# branch.oid ") {
            if value != b"(initial)" {
                let value = text(value, "invalid branch object ID")?;
                validate_oid(value)?;
                head_oid = Some(value.to_owned());
            }
        } else if let Some(value) = record.strip_prefix(b"# branch.head ") {
            branch = Some(text(value, "invalid branch name")?.to_owned());
        } else if let Some(value) = record.strip_prefix(b"# branch.upstream ") {
            upstream = Some(text(value, "invalid upstream name")?.to_owned());
        } else if let Some(value) = record.strip_prefix(b"# branch.ab +") {
            let value = text(value, "invalid ahead/behind record")?;
            let (ahead_text, behind_text) =
                value
                    .split_once(" -")
                    .ok_or(LocalGitError::MalformedOutput(
                        "invalid ahead/behind record",
                    ))?;
            ahead = ahead_text
                .parse()
                .map_err(|_| LocalGitError::MalformedOutput("invalid ahead count"))?;
            behind = behind_text
                .parse()
                .map_err(|_| LocalGitError::MalformedOutput("invalid behind count"))?;
        } else if record.starts_with(b"1 ") {
            let fields = split_fields(record, 9)?;
            let entry = status_entry(fields[1], fields[8], None)?;
            add_entry(entry, &mut staged, &mut unstaged);
        } else if record.starts_with(b"2 ") {
            let fields = split_fields(record, 10)?;
            let previous = records
                .get(index)
                .ok_or(LocalGitError::MalformedOutput("missing rename source path"))?;
            index += 1;
            let entry = status_entry(fields[1], fields[9], Some(previous))?;
            add_entry(entry, &mut staged, &mut unstaged);
        } else if record.starts_with(b"u ") {
            let fields = split_fields(record, 11)?;
            conflicts.push(status_entry(fields[1], fields[10], None)?);
        } else if let Some(path) = record.strip_prefix(b"? ") {
            untracked.push(GitPath::from_raw(path.to_vec())?);
        } else if !record.starts_with(b"! ") {
            return Err(LocalGitError::MalformedOutput(
                "unknown porcelain-v2 record",
            ));
        }
    }
    let branch = branch.ok_or(LocalGitError::MalformedOutput("missing branch head record"))?;
    let head = match (branch.as_str(), head_oid) {
        ("(detached)", Some(oid)) => HeadState::Detached { oid },
        ("(detached)", None) => {
            return Err(LocalGitError::MalformedOutput(
                "detached HEAD has no object ID",
            ));
        }
        (name, Some(oid)) => HeadState::Attached {
            branch: name.to_owned(),
            oid,
        },
        (name, None) => HeadState::Unborn {
            branch: name.to_owned(),
        },
    };
    Ok(LocalSnapshot {
        head,
        upstream,
        upstream_oid: None,
        ahead,
        behind,
        staged,
        unstaged,
        untracked,
        conflicts,
        operation: OperationState::default(),
        guard: SnapshotGuard([0; 32]),
    })
}

fn split_fields(record: &[u8], expected: usize) -> Result<Vec<&[u8]>> {
    let fields: Vec<_> = record.splitn(expected, |byte| *byte == b' ').collect();
    if fields.len() != expected {
        return Err(LocalGitError::MalformedOutput(
            "invalid porcelain-v2 field count",
        ));
    }
    Ok(fields)
}

fn status_entry(xy: &[u8], path: &[u8], previous: Option<&&[u8]>) -> Result<StatusEntry> {
    if xy.len() != 2 {
        return Err(LocalGitError::MalformedOutput(
            "invalid porcelain-v2 XY status",
        ));
    }
    Ok(StatusEntry {
        path: GitPath::from_raw(path.to_vec())?,
        previous_path: previous
            .map(|path| GitPath::from_raw(path.to_vec()))
            .transpose()?,
        index_status: xy[0] as char,
        worktree_status: xy[1] as char,
    })
}

fn add_entry(entry: StatusEntry, staged: &mut Vec<StatusEntry>, unstaged: &mut Vec<StatusEntry>) {
    if entry.index_status != '.' {
        staged.push(entry.clone());
    }
    if entry.worktree_status != '.' {
        unstaged.push(entry);
    }
}

fn validate_path(raw: &[u8]) -> Result<()> {
    if raw.is_empty() || raw.contains(&0) || raw.starts_with(b"/") || raw.ends_with(b"/") {
        return Err(LocalGitError::InvalidInput(
            "invalid repository-relative path",
        ));
    }
    let path = Path::new(OsStr::from_bytes(raw));
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(LocalGitError::InvalidInput("path leaves the worktree"));
    }
    Ok(())
}

fn pathspec_input(paths: &[GitPath], max_bytes: usize) -> Result<Vec<u8>> {
    if paths.is_empty() {
        return Err(LocalGitError::InvalidInput("at least one path is required"));
    }
    let mut input = Vec::new();
    for path in paths {
        validate_path(&path.raw)?;
        if input.len().saturating_add(path.raw.len()).saturating_add(1) > max_bytes {
            return Err(LocalGitError::InvalidInput(
                "path list exceeds configured bound",
            ));
        }
        input.extend_from_slice(&path.raw);
        input.push(0);
    }
    Ok(input)
}

fn validate_oid(oid: &str) -> Result<()> {
    if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(LocalGitError::InvalidInput(
            "expected a full SHA-1 or SHA-256 object ID",
        ));
    }
    Ok(())
}

fn display_path(raw: &[u8]) -> String {
    if let Ok(path) = std::str::from_utf8(raw) {
        return path.to_owned();
    }
    let mut display = String::new();
    for byte in raw {
        match byte {
            b'\n' => display.push_str("\\n"),
            b'\r' => display.push_str("\\r"),
            b'\t' => display.push_str("\\t"),
            b'\\' => display.push_str("\\\\"),
            0x20..=0x7e => display.push(*byte as char),
            _ => display.push_str(&format!("\\x{byte:02x}")),
        }
    }
    display
}

fn is_media_path(path: &[u8]) -> bool {
    const EXTENSIONS: &[&[u8]] = &[
        b".avif", b".bmp", b".gif", b".heic", b".jpeg", b".jpg", b".m4a", b".m4v", b".mov",
        b".mp3", b".mp4", b".mpeg", b".mpg", b".ogg", b".png", b".svg", b".tif", b".tiff", b".wav",
        b".webm", b".webp",
    ];
    let lower: Vec<u8> = path.iter().map(u8::to_ascii_lowercase).collect();
    EXTENSIONS
        .iter()
        .any(|extension| lower.ends_with(extension))
}

fn canonical_git_path(path: PathBuf, context: &'static str) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|source| LocalGitError::Io { context, source })
}

fn strip_one_lf(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    bytes
}

fn os_args(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn text<'a>(bytes: &'a [u8], reason: &'static str) -> Result<&'a str> {
    std::str::from_utf8(bytes).map_err(|_| LocalGitError::MalformedOutput(reason))
}

fn kill_process_group(child: &mut std::process::Child) {
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &format!("-{}", child.id())])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}
