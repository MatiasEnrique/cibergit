//! Durable, guarded single-branch linear interactive rebase backend.
//!
//! Git's checkout, index, refs, and rebase control files remain authoritative.
//! The private record supplies intent and transition evidence; it never grants
//! permission to replay an operation after Git may have started.

use crate::local_git::{
    GitPath, HeadState, LocalGit, LocalGitError, LocalSnapshot, RebaseState, SnapshotGuard,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::{OsStr, OsString},
    fmt,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::fd::AsRawFd,
    os::raw::c_int,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, TryLockError,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const RECORD_VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 4 * 1024 * 1024;
const MAX_COMMITS: usize = 4_096;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_INVENTORY_MESSAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SCOPE_TEXT_BYTES: usize = 4_096;
const CONFLICT_BLOB_LIMIT: u64 = 2 * 1024 * 1024;
const PRIVATE_DIR_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const JOURNAL_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const JOURNAL_LOCK_RETRY: Duration = Duration::from_millis(10);

// Darwin flock(2) values. The application target is macOS; LOCK_NB keeps every
// acquisition bounded in userspace and the held descriptor makes crash release
// an operating-system responsibility.
const LOCK_EX: c_int = 0x02;
const LOCK_NB: c_int = 0x04;

unsafe extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
}

static UNIQUE_ID: AtomicU64 = AtomicU64::new(1);
static RECORD_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

struct RecordAuthorityGuard<'a> {
    _process_guard: MutexGuard<'a, ()>,
    _descriptor: File,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebaseAssociation {
    pub provider: String,
    pub host: String,
    pub account: String,
    pub repository: String,
    pub change: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitEntry {
    pub oid: String,
    pub parent_oid: String,
    /// Lossless commit-message bytes. `message` is only the UI representation.
    pub message_raw: Vec<u8>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInventory {
    pub base_oid: String,
    pub branch: String,
    pub head_oid: String,
    /// Oldest to newest.
    pub commits: Vec<CommitEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnsupportedHistory {
    ContainsMerge {
        commit_oid: String,
        parent_count: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalWorkflow {
    pub reason: UnsupportedHistory,
    pub explanation: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirtyChoice {
    Commit,
    StashIncludingUntracked,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirtyPreparation {
    pub operation_id: String,
    pub inventory: CommitInventory,
    pub guard: SnapshotGuard,
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub conflicts: usize,
    pub choices: [DirtyChoice; 3],
    pub stash_includes_untracked: bool,
    pub ignored_files_remain_outside_stash: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebasePreparation {
    pub operation_id: String,
    pub inventory: CommitInventory,
    pub guard: SnapshotGuard,
    pub stash: Option<StashReceipt>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrepareOutcome {
    Ready(RebasePreparation),
    Dirty(DirtyPreparation),
    ExternalWorkflow(ExternalWorkflow),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanAction {
    Pick,
    Squash,
    Fixup,
    Drop,
    Reword { message: String },
    Edit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub commit_oid: String,
    pub action: PlanAction,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebasePlan {
    pub inventory_base_oid: String,
    pub inventory_head_oid: String,
    pub steps: Vec<PlanStep>,
}

impl RebasePlan {
    pub fn validate(inventory: &CommitInventory, steps: Vec<PlanStep>) -> Result<Self> {
        if steps.len() != inventory.commits.len() {
            return Err(RebaseError::InvalidPlan(
                "the plan must mention every inventoried commit exactly once".into(),
            ));
        }
        let members: BTreeSet<&str> = inventory
            .commits
            .iter()
            .map(|commit| commit.oid.as_str())
            .collect();
        let mut seen = BTreeSet::new();
        let mut prior_fold_target = false;
        for step in &steps {
            validate_full_oid(&step.commit_oid)?;
            if !members.contains(step.commit_oid.as_str()) {
                return Err(RebaseError::InvalidPlan(
                    "the plan contains a commit outside the immutable inventory".into(),
                ));
            }
            if !seen.insert(step.commit_oid.as_str()) {
                return Err(RebaseError::InvalidPlan(
                    "the plan contains a duplicate commit".into(),
                ));
            }
            if let PlanAction::Reword { message } = &step.action {
                validate_message(message)?;
            }
            match step.action {
                PlanAction::Squash | PlanAction::Fixup if !prior_fold_target => {
                    return Err(RebaseError::InvalidPlan(
                        "squash/fixup requires an earlier non-dropped commit".into(),
                    ));
                }
                PlanAction::Drop => {}
                _ => prior_fold_target = true,
            }
        }
        Ok(Self {
            inventory_base_oid: inventory.base_oid.clone(),
            inventory_head_oid: inventory.head_oid.clone(),
            steps,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationState {
    Prepared,
    Running,
    PausedForEdit,
    Conflicted,
    Completed,
    Aborted,
    FailedUncertain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveOperationIdentity {
    pub operation_id: String,
    pub head_name: String,
    pub onto_oid: String,
    pub original_head_oid: String,
    pub stopped_oid: Option<String>,
    pub rebase_head_oid: Option<String>,
    pub stop_is_edit: bool,
    pub todo_sha256: String,
    pub done_sha256: String,
    pub ownership_marker_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionEvidence {
    pub observed_head: HeadState,
    pub observed_rebase: RebaseState,
    pub dispatch_proof: bool,
    pub owned_active_marker: bool,
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationView {
    pub operation_id: String,
    pub attempt: u64,
    pub state: OperationState,
    pub original_branch: String,
    pub original_head_oid: String,
    pub base_oid: String,
    pub active: Option<ActiveOperationIdentity>,
    pub resulting_commits: Vec<CommitEntry>,
    pub stash: Option<StashReceipt>,
    pub stash_restore: StashRestoreState,
    pub evidence: Vec<TransitionEvidence>,
    pub publish_warning: Option<String>,
    pub publish_handoff: Option<PublishHandoff>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishHandoff {
    pub branch: String,
    pub original_local_head_oid: String,
    pub rewritten_local_head_oid: String,
    /// The caller must independently supply the expected remote OID to the
    /// existing immutable-OID force-with-lease API.
    pub requires_explicit_expected_remote_oid: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashReceipt {
    pub oid: String,
    pub includes_untracked: bool,
    pub ignored_files_excluded: bool,
    /// `git stash apply --index <oid>` retains the stash ref/object.
    pub retained_after_restore: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StashRestoreState {
    NotStarted,
    Running,
    Conflicted,
    Completed,
    FailedUncertain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiskGeneration {
    Missing,
    Symlink {
        target_sha256: String,
    },
    Regular {
        sha256: String,
        len: u64,
        device: u64,
        inode: u64,
        modified_seconds: i64,
        modified_nanoseconds: i64,
    },
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictKind {
    BothModified,
    AddedByBoth,
    DeletedByOurs,
    DeletedByTheirs,
    RenameOrDelete {
        related_paths: Vec<GitPath>,
        explanation: String,
    },
    TypeChange,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobContent {
    Utf8(String),
    Binary,
    Media,
    Symlink(Vec<u8>),
    NonUtf8,
    TooLarge { bytes: u64, limit: u64 },
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictStage {
    pub oid: Option<String>,
    pub mode: Option<String>,
    pub content: BlobContent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictFile {
    pub path: GitPath,
    pub kind: ConflictKind,
    pub base: ConflictStage,
    /// During rebase, Git defines ours as the rebased series and theirs as the
    /// replayed original commit (the opposite of many users' intuition).
    pub ours: ConflictStage,
    pub theirs: ConflictStage,
    pub disk: DiskGeneration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FilesystemIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ScopeIdentity {
    association: RebaseAssociation,
    checkout: PathBuf,
    git_dir: PathBuf,
    common_git_dir: PathBuf,
    checkout_identity: FilesystemIdentity,
    git_dir_identity: FilesystemIdentity,
    common_git_dir_identity: FilesystemIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum AttemptAction {
    None,
    CreateStash,
    Start,
    Continue,
    Skip,
    Abort,
    RestoreStash,
    Amend,
    BeginSplit,
    CommitSplit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SplitRecord {
    stopped_oid: String,
    parent_oid: String,
    required_tree_oid: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StoredRecord {
    operation_id: String,
    attempt: u64,
    state: OperationState,
    scope: ScopeIdentity,
    inventory: CommitInventory,
    plan: Option<RebasePlan>,
    action: AttemptAction,
    dispatch_acknowledged: bool,
    stash_before_oid: Option<String>,
    stash_expected_untracked: bool,
    stash: Option<StashReceipt>,
    stash_restore: StashRestoreState,
    split: Option<SplitRecord>,
    resulting_commits: Vec<CommitEntry>,
    evidence: Vec<TransitionEvidence>,
}

#[derive(Serialize, Deserialize)]
struct RecordEnvelope {
    version: u32,
    payload: serde_json::Value,
    checksum: String,
}

#[derive(Clone)]
pub struct RebaseStore {
    git: LocalGit,
    scope: ScopeIdentity,
    partition_dir: PathBuf,
    record_path: PathBuf,
    record_lock: Arc<Mutex<()>>,
}

impl fmt::Debug for RebaseStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RebaseStore")
            .field("checkout", &self.scope.checkout)
            .field("partition_dir", &self.partition_dir)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum RebaseError {
    Git(LocalGitError),
    Io {
        context: &'static str,
        source: std::io::Error,
    },
    InvalidInput(String),
    InvalidPlan(String),
    UnsupportedState(String),
    PendingOperation(String),
    MissingOperation,
    CorruptRecord(String),
    JournalBusy,
    FutureRecord {
        version: u32,
    },
    IdentityChanged(String),
    StaleOperation,
    PostStartUncertain {
        action: &'static str,
        detail: String,
    },
}

impl fmt::Display for RebaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git(error) => error.fmt(formatter),
            Self::Io { context, .. } => formatter.write_str(context),
            Self::InvalidInput(reason) => write!(formatter, "invalid rebase input: {reason}"),
            Self::InvalidPlan(reason) => write!(formatter, "invalid rebase plan: {reason}"),
            Self::UnsupportedState(reason) => {
                write!(formatter, "unsupported rebase state: {reason}")
            }
            Self::PendingOperation(id) => {
                write!(formatter, "rebase operation {id} requires reconciliation")
            }
            Self::MissingOperation => formatter.write_str("no durable rebase operation exists"),
            Self::CorruptRecord(reason) => write!(
                formatter,
                "rebase record is corrupt and was preserved: {reason}"
            ),
            Self::JournalBusy => formatter.write_str(
                "the rebase journal is busy in another process; retry after its bounded operation finishes",
            ),
            Self::FutureRecord { version } => write!(
                formatter,
                "rebase record version {version} is newer and was preserved"
            ),
            Self::IdentityChanged(reason) => {
                write!(formatter, "checkout identity changed: {reason}")
            }
            Self::StaleOperation => {
                formatter.write_str("the displayed active rebase identity is stale")
            }
            Self::PostStartUncertain { action, detail } => write!(
                formatter,
                "{action} may have partially taken effect; reconcile before retrying: {detail}"
            ),
        }
    }
}

impl std::error::Error for RebaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Git(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<LocalGitError> for RebaseError {
    fn from(error: LocalGitError) -> Self {
        Self::Git(error)
    }
}

pub type Result<T> = std::result::Result<T, RebaseError>;

impl RebaseStore {
    pub fn open(
        private_root: impl AsRef<Path>,
        association: RebaseAssociation,
        checkout: impl AsRef<Path>,
    ) -> Result<Self> {
        validate_association(&association)?;
        let private_root = private_root.as_ref();
        if !private_root.is_absolute() {
            return Err(RebaseError::InvalidInput(
                "private root must be absolute".into(),
            ));
        }
        create_private_dir(private_root)?;
        let private_root =
            fs::canonicalize(private_root).map_err(io("canonicalize private root"))?;
        let git = LocalGit::open(checkout)?;
        Self::from_local_git(private_root, association, git)
    }

    pub fn from_local_git(
        private_root: impl AsRef<Path>,
        association: RebaseAssociation,
        git: LocalGit,
    ) -> Result<Self> {
        validate_association(&association)?;
        let private_root = private_root.as_ref();
        if !private_root.is_absolute() {
            return Err(RebaseError::InvalidInput(
                "private root must be absolute".into(),
            ));
        }
        create_private_dir(private_root)?;
        let private_root =
            fs::canonicalize(private_root).map_err(io("canonicalize private root"))?;
        let scope = ScopeIdentity {
            association,
            checkout: git.root().to_path_buf(),
            git_dir: git.git_dir().to_path_buf(),
            common_git_dir: git.common_git_dir().to_path_buf(),
            checkout_identity: filesystem_identity(git.root())?,
            git_dir_identity: filesystem_identity(git.git_dir())?,
            common_git_dir_identity: filesystem_identity(git.common_git_dir())?,
        };
        let encoded = serde_json::to_vec(&scope)
            .map_err(|_| RebaseError::InvalidInput("scope cannot be encoded".into()))?;
        let partition = hex_digest(&encoded);
        let partition_dir = private_root.join("rebase").join(partition);
        create_private_dir(&private_root.join("rebase"))?;
        create_private_dir(&partition_dir)?;
        let record_path = partition_dir.join("operation.json");
        let record_lock = {
            let registry = RECORD_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
            let mut registry = registry.lock().map_err(|_| {
                RebaseError::CorruptRecord("record lock registry is poisoned".into())
            })?;
            registry
                .entry(record_path.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let store = Self {
            git,
            scope,
            partition_dir,
            record_path,
            record_lock,
        };
        {
            let _record_guard = store.lock_record()?;
            store.reject_replaced_checkout_records(&private_root.join("rebase"))?;
            if store.record_path.exists() {
                let _ = store.load_record()?;
            }
        }
        Ok(store)
    }

    pub fn prepare(&self, base_oid: &str) -> Result<PrepareOutcome> {
        let _record_guard = self.lock_record()?;
        validate_full_oid(base_oid)?;
        if let Some(record) = self.try_load_record()? {
            return Err(RebaseError::PendingOperation(record.operation_id));
        }
        self.verify_scope_identity()?;
        let snapshot = self.git.snapshot()?;
        if snapshot.operation.rebase != RebaseState::None
            || snapshot.operation.merge
            || snapshot.operation.cherry_pick
            || snapshot.operation.revert
        {
            return Err(RebaseError::UnsupportedState(
                "another Git operation is already active".into(),
            ));
        }
        let inventory = self.inventory(base_oid, &snapshot)?;
        if let Some(merge) = inventory.commits.iter().find_map(|commit| {
            let parents = self.parents(&commit.oid).ok()?;
            (parents.len() != 1).then_some((commit.oid.clone(), parents.len()))
        }) {
            return Ok(PrepareOutcome::ExternalWorkflow(ExternalWorkflow {
                reason: UnsupportedHistory::ContainsMerge {
                    commit_oid: merge.0.clone(),
                    parent_count: merge.1,
                },
                explanation: format!(
                    "commit {} has {} parents; use an external merge-preserving workflow because cibergit will not flatten it",
                    merge.0, merge.1
                ),
            }));
        }
        let operation_id = new_operation_id(&self.scope, &inventory);
        let dirty = !snapshot.staged.is_empty()
            || !snapshot.unstaged.is_empty()
            || !snapshot.untracked.is_empty()
            || !snapshot.conflicts.is_empty();
        if dirty {
            return Ok(PrepareOutcome::Dirty(DirtyPreparation {
                operation_id,
                inventory,
                guard: snapshot.guard,
                staged: snapshot.staged.len(),
                unstaged: snapshot.unstaged.len(),
                untracked: snapshot.untracked.len(),
                conflicts: snapshot.conflicts.len(),
                choices: [
                    DirtyChoice::Commit,
                    DirtyChoice::StashIncludingUntracked,
                    DirtyChoice::Cancel,
                ],
                stash_includes_untracked: true,
                ignored_files_remain_outside_stash: true,
            }));
        }
        Ok(PrepareOutcome::Ready(RebasePreparation {
            operation_id,
            inventory,
            guard: snapshot.guard,
            stash: None,
        }))
    }

    pub fn create_stash(&self, preparation: &DirtyPreparation) -> Result<RebasePreparation> {
        let _record_guard = self.lock_record()?;
        self.create_stash_locked(preparation, || {})
    }

    #[cfg(test)]
    #[allow(dead_code)] // Called by the path-included integration test module.
    pub(crate) fn create_stash_with_post_command_hook(
        &self,
        preparation: &DirtyPreparation,
        after_command: impl FnOnce(),
    ) -> Result<RebasePreparation> {
        let _record_guard = self.lock_record()?;
        self.create_stash_locked(preparation, after_command)
    }

    fn create_stash_locked(
        &self,
        preparation: &DirtyPreparation,
        after_command: impl FnOnce(),
    ) -> Result<RebasePreparation> {
        if let Some(record) = self.try_load_record()? {
            return Err(RebaseError::PendingOperation(record.operation_id));
        }
        if preparation.conflicts != 0 {
            return Err(RebaseError::UnsupportedState(
                "unmerged entries must be resolved before creating a stash".into(),
            ));
        }
        let mut record = self.new_record(&preparation.operation_id, &preparation.inventory);
        record.attempt = 1;
        record.action = AttemptAction::CreateStash;
        record.stash_before_oid = self.optional_ref_oid("refs/stash")?;
        // `--include-untracked` creates the third, root-parented stash commit
        // even when the current untracked set is empty on installed Git 2.50.1.
        record.stash_expected_untracked = true;
        self.save_record(&record)?;
        let command_result = self
            .git
            .with_guarded_worktree(&preparation.guard, |actual| {
                require_clean_operation_and_identity(actual, &preparation.inventory)?;
                self.git.run_worktree_command(
                    "create explicit rebase stash",
                    vec![
                        // Apple Git 2.50.1 leaves untracked files in place when
                        // `stash -u` inherits the runner's `--literal-pathspecs`.
                        // This command has no pathspecs, so cancel that global flag.
                        "--no-literal-pathspecs".into(),
                        "stash".into(),
                        "push".into(),
                        "--include-untracked".into(),
                        "--message".into(),
                        stash_marker(&preparation.operation_id).into(),
                    ],
                    true,
                    &[0],
                )
            });
        after_command();
        let current = self.optional_ref_oid("refs/stash")?;
        let oid = match command_result {
            Ok(_) => {
                match self.acknowledged_stash_oid(&record)? {
                    Some(oid) => oid,
                    None => {
                        record.state = OperationState::FailedUncertain;
                        record.evidence.push(self.evidence(
                        "no unique stash object matched this operation nonce and original HEAD"
                            .into(),
                    )?);
                        self.save_record(&record)?;
                        return Err(RebaseError::PostStartUncertain {
                        action: "stash creation",
                        detail: "the exact resulting stash object could not be bound to the operation"
                            .into(),
                    });
                    }
                }
            }
            Err(error)
                if is_certain_prestart_error(&error) && current == record.stash_before_oid =>
            {
                record.action = AttemptAction::None;
                record.evidence.push(
                    self.evidence(
                        "stash creation was certainly refused before Git could change the checkout"
                            .into(),
                    )?,
                );
                self.save_record(&record)?;
                return Err(RebaseError::Git(error));
            }
            Err(error) => {
                record.state = OperationState::FailedUncertain;
                record
                    .evidence
                    .push(self.evidence(format!("stash acknowledgement was ambiguous: {error}"))?);
                self.save_record(&record)?;
                return Err(RebaseError::PostStartUncertain {
                    action: "stash creation",
                    detail: "the exact resulting stash object could not be acknowledged".into(),
                });
            }
        };
        validate_full_oid(&oid)?;
        let receipt = StashReceipt {
            oid,
            includes_untracked: true,
            ignored_files_excluded: true,
            retained_after_restore: true,
        };
        record.action = AttemptAction::None;
        record.stash = Some(receipt.clone());
        record.state = OperationState::Prepared;
        record
            .evidence
            .push(self.evidence("exact stash object acknowledged".into())?);
        self.save_record(&record)?;
        let snapshot = self.git.snapshot()?;
        if !snapshot.staged.is_empty()
            || !snapshot.unstaged.is_empty()
            || !snapshot.untracked.is_empty()
            || !snapshot.conflicts.is_empty()
        {
            return Err(RebaseError::PostStartUncertain {
                action: "stash creation",
                detail: format!(
                    "Git acknowledged a stash but the checkout did not become clean (staged={}, unstaged={}, untracked={:?}, conflicts={})",
                    snapshot.staged.len(),
                    snapshot.unstaged.len(),
                    snapshot.untracked,
                    snapshot.conflicts.len()
                ),
            });
        }
        Ok(RebasePreparation {
            operation_id: preparation.operation_id.clone(),
            inventory: preparation.inventory.clone(),
            guard: snapshot.guard,
            stash: Some(receipt),
        })
    }

    pub fn start(
        &self,
        preparation: &RebasePreparation,
        plan: &RebasePlan,
    ) -> Result<OperationView> {
        let _record_guard = self.lock_record()?;
        if plan.inventory_base_oid != preparation.inventory.base_oid
            || plan.inventory_head_oid != preparation.inventory.head_oid
        {
            return Err(RebaseError::InvalidPlan(
                "plan inventory identity is stale".into(),
            ));
        }
        let checked = RebasePlan::validate(&preparation.inventory, plan.steps.clone())?;
        let mut record = match self.try_load_record()? {
            Some(existing)
                if existing.operation_id == preparation.operation_id
                    && existing.state == OperationState::Prepared
                    && existing.plan.is_none()
                    && existing.action == AttemptAction::None =>
            {
                existing
            }
            Some(existing) => return Err(RebaseError::PendingOperation(existing.operation_id)),
            None => self.new_record(&preparation.operation_id, &preparation.inventory),
        };
        if record.stash != preparation.stash {
            return Err(RebaseError::InvalidInput(
                "stash preparation identity is stale".into(),
            ));
        }
        record.attempt += 1;
        record.action = AttemptAction::Start;
        record.plan = Some(checked.clone());
        record.state = OperationState::Prepared;
        record.dispatch_acknowledged = false;
        self.save_record(&record)?;
        self.write_helpers(&record.operation_id, &checked)?;

        let helpers = self.helper_paths(&record.operation_id);
        let environment = self.helper_environment(&record.operation_id, &helpers);
        let editor = shell_quote(&helpers.sequence_editor);
        let message_editor = shell_quote(&helpers.message_editor);
        let command_result = self
            .git
            .with_guarded_worktree(&preparation.guard, |actual| {
                require_pristine_start(actual, &preparation.inventory)?;
                self.git.run_worktree_command_with_input_and_env(
                    "start interactive rebase",
                    vec![
                        "-c".into(),
                        format!("sequence.editor={editor}").into(),
                        "-c".into(),
                        format!("core.editor={message_editor}").into(),
                        "rebase".into(),
                        "--interactive".into(),
                        "--force-rebase".into(),
                        "--reapply-cherry-picks".into(),
                        "--empty=keep".into(),
                        "--no-autostash".into(),
                        "--no-update-refs".into(),
                        "--no-fork-point".into(),
                        "--onto".into(),
                        preparation.inventory.base_oid.clone().into(),
                        preparation.inventory.base_oid.clone().into(),
                    ],
                    None,
                    &environment,
                    true,
                    &[0],
                )
            });
        let proof = self.dispatch_proof_matches(&record.operation_id)?;
        record.dispatch_acknowledged = command_result.is_ok() || proof;
        record.state = OperationState::Running;
        record.evidence.push(self.evidence(format!(
            "interactive rebase returned; dispatch proof={proof}"
        ))?);
        self.save_record(&record)?;
        self.reconcile_after_command("interactive rebase start", command_result.err())
    }

    pub fn observe(&self) -> Result<Option<OperationView>> {
        let _record_guard = self.lock_record()?;
        self.observe_locked()
    }

    fn observe_locked(&self) -> Result<Option<OperationView>> {
        let Some(mut record) = self.try_load_record()? else {
            return Ok(None);
        };
        self.reconcile_record(&mut record)?;
        self.save_record(&record)?;
        Ok(Some(view(
            &record,
            self.active_identity(&record.operation_id)?,
        )))
    }

    pub fn continue_rebase(
        &self,
        operation_id: &str,
        expected_active: &ActiveOperationIdentity,
        guard: &SnapshotGuard,
    ) -> Result<OperationView> {
        let _record_guard = self.lock_record()?;
        self.control(
            operation_id,
            expected_active,
            guard,
            AttemptAction::Continue,
            "continue rebase",
            "--continue",
        )
    }

    pub fn skip(
        &self,
        operation_id: &str,
        expected_active: &ActiveOperationIdentity,
        guard: &SnapshotGuard,
    ) -> Result<OperationView> {
        let _record_guard = self.lock_record()?;
        self.control(
            operation_id,
            expected_active,
            guard,
            AttemptAction::Skip,
            "skip rebase commit",
            "--skip",
        )
    }

    pub fn abort(
        &self,
        operation_id: &str,
        expected_active: &ActiveOperationIdentity,
        guard: &SnapshotGuard,
    ) -> Result<OperationView> {
        let _record_guard = self.lock_record()?;
        self.control(
            operation_id,
            expected_active,
            guard,
            AttemptAction::Abort,
            "abort rebase",
            "--abort",
        )
    }

    pub fn amend_at_edit(
        &self,
        operation_id: &str,
        expected_active: &ActiveOperationIdentity,
        guard: &SnapshotGuard,
        message: Option<&str>,
    ) -> Result<OperationView> {
        let _record_guard = self.lock_record()?;
        if let Some(message) = message {
            validate_message(message)?;
        }
        let mut record = self.require_record(operation_id)?;
        self.require_owned_active(&record, expected_active)?;
        if record.state != OperationState::PausedForEdit || record.split.is_some() {
            return Err(RebaseError::UnsupportedState(
                "amend is only available at an ordinary edit stop".into(),
            ));
        }
        record.attempt += 1;
        record.action = AttemptAction::Amend;
        self.save_record(&record)?;
        let message_path = self
            .partition_dir
            .join(format!("amend-{}.txt", record.attempt));
        if let Some(message) = message {
            write_private_file(&message_path, message.as_bytes(), false)?;
        }
        let result = self.git.with_guarded_worktree(guard, |_| {
            self.require_active_identity(expected_active)?;
            let mut args: Vec<OsString> = vec!["commit".into(), "--amend".into()];
            if message.is_some() {
                args.extend(["-F".into(), message_path.as_os_str().to_owned()]);
            } else {
                args.push("--no-edit".into());
            }
            self.git
                .run_worktree_command("amend rebase edit stop", args, true, &[0])
        });
        record.action = AttemptAction::None;
        record
            .evidence
            .push(self.evidence("edit-stop amendment dispatched".into())?);
        self.save_record(&record)?;
        self.reconcile_after_command("amend edit stop", result.err())
    }

    pub fn begin_split(
        &self,
        operation_id: &str,
        expected_active: &ActiveOperationIdentity,
        guard: &SnapshotGuard,
    ) -> Result<OperationView> {
        let _record_guard = self.lock_record()?;
        let mut record = self.require_record(operation_id)?;
        self.require_owned_active(&record, expected_active)?;
        if record.state != OperationState::PausedForEdit || record.split.is_some() {
            return Err(RebaseError::UnsupportedState(
                "split is only available once at an edit stop".into(),
            ));
        }
        record.attempt += 1;
        record.action = AttemptAction::BeginSplit;
        self.save_record(&record)?;
        let split = self.git.with_guarded_worktree(guard, |actual| {
            require_no_local_changes(actual, true)?;
            self.require_active_identity(expected_active)?;
            let parent_oid = self.rev_parse("HEAD^")?;
            let required_tree_oid = self.rev_parse("HEAD^{tree}")?;
            let stopped_oid =
                expected_active
                    .stopped_oid
                    .clone()
                    .ok_or(LocalGitError::MalformedOutput(
                        "edit stop has no stopped object ID",
                    ))?;
            let split = SplitRecord {
                stopped_oid,
                parent_oid,
                required_tree_oid,
            };
            // This transition evidence is durable before reset. If reset starts
            // but acknowledgement is lost, restart cannot repeat it blindly.
            record.split = Some(split.clone());
            record.evidence.push(TransitionEvidence {
                observed_head: actual.head.clone(),
                observed_rebase: actual.operation.rebase,
                dispatch_proof: true,
                owned_active_marker: true,
                note: "split stopped object, parent, and required tree recorded before reset"
                    .into(),
            });
            self.save_record(&record)
                .map_err(|_| LocalGitError::MalformedOutput("persist split transition"))?;
            self.git.run_worktree_command(
                "begin edit-stop split",
                vec!["reset".into(), "--mixed".into(), "HEAD^".into()],
                true,
                &[0],
            )?;
            Ok(split)
        });
        match split {
            Ok(split) => {
                record.split = Some(split);
                record.action = AttemptAction::None;
                record.evidence.push(self.evidence(
                    "split reset preserved the stopped commit as unstaged content".into(),
                )?);
                self.save_record(&record)?;
                self.observe_locked()?.ok_or(RebaseError::MissingOperation)
            }
            Err(error) if is_certain_prestart_error(&error) => {
                record.action = AttemptAction::None;
                record.evidence.push(
                    self.evidence("split transition was certainly refused before reset".into())?,
                );
                self.save_record(&record)?;
                Err(RebaseError::Git(error))
            }
            Err(error) => self.fail_after_start(record, "begin split", error.to_string()),
        }
    }

    pub fn commit_split_part(
        &self,
        operation_id: &str,
        expected_active: &ActiveOperationIdentity,
        guard: &SnapshotGuard,
        message: &str,
    ) -> Result<OperationView> {
        let _record_guard = self.lock_record()?;
        validate_message(message)?;
        let mut record = self.require_record(operation_id)?;
        self.require_owned_active(&record, expected_active)?;
        if record.state != OperationState::PausedForEdit || record.split.is_none() {
            return Err(RebaseError::UnsupportedState(
                "split has not been started".into(),
            ));
        }
        record.attempt += 1;
        record.action = AttemptAction::CommitSplit;
        self.save_record(&record)?;
        let message_path = self
            .partition_dir
            .join(format!("split-{}.txt", record.attempt));
        write_private_file(&message_path, message.as_bytes(), false)?;
        let result = self.git.with_guarded_worktree(guard, |actual| {
            self.require_active_identity(expected_active)?;
            if actual.staged.is_empty() || !actual.conflicts.is_empty() {
                return Err(LocalGitError::InvalidInput(
                    "split commit requires staged changes and no conflicts",
                ));
            }
            self.git.run_worktree_command(
                "commit split part",
                vec![
                    "commit".into(),
                    "-F".into(),
                    message_path.as_os_str().to_owned(),
                ],
                true,
                &[0],
            )
        });
        record.action = AttemptAction::None;
        record
            .evidence
            .push(self.evidence("explicit split commit dispatched".into())?);
        self.save_record(&record)?;
        self.reconcile_after_command("commit split part", result.err())
    }

    pub fn finish_split(
        &self,
        operation_id: &str,
        expected_active: &ActiveOperationIdentity,
        guard: &SnapshotGuard,
    ) -> Result<OperationView> {
        let _record_guard = self.lock_record()?;
        let record = self.require_record(operation_id)?;
        let split = record
            .split
            .clone()
            .ok_or_else(|| RebaseError::UnsupportedState("split has not been started".into()))?;
        self.git.with_guarded_worktree(guard, |actual| {
            require_no_local_changes(actual, true)?;
            self.require_active_identity(expected_active)?;
            let actual_tree = self.rev_parse("HEAD^{tree}")?;
            if actual_tree != split.required_tree_oid {
                return Err(LocalGitError::InvalidInput(
                    "split commits do not reproduce the stopped commit tree",
                ));
            }
            let count = self.first_parent_count(&split.parent_oid, "HEAD")?;
            if count < 2 {
                return Err(LocalGitError::InvalidInput(
                    "split requires at least two replacement commits",
                ));
            }
            Ok(())
        })?;
        self.control(
            operation_id,
            expected_active,
            guard,
            AttemptAction::Continue,
            "continue rebase",
            "--continue",
        )
    }

    pub fn conflicts(
        &self,
        expected_active: &ActiveOperationIdentity,
    ) -> Result<Vec<ConflictFile>> {
        let _record_guard = self.lock_record()?;
        let record = self.require_record(&expected_active.operation_id)?;
        self.require_owned_active(&record, expected_active)?;
        self.read_conflicts()
    }

    pub fn stage_resolution(
        &self,
        operation_id: &str,
        expected_active: &ActiveOperationIdentity,
        expected: &ConflictFile,
        guard: &SnapshotGuard,
    ) -> Result<OperationView> {
        let _record_guard = self.lock_record()?;
        let record = self.require_record(operation_id)?;
        self.require_owned_active(&record, expected_active)?;
        self.git.with_guarded_worktree(guard, |_| {
            self.require_active_identity(expected_active)?;
            let current = self
                .read_conflicts()
                .map_err(|_| LocalGitError::StaleSnapshot)?;
            let candidate = current
                .iter()
                .find(|item| item.path.raw == expected.path.raw)
                .ok_or(LocalGitError::StaleSnapshot)?;
            if candidate != expected {
                return Err(LocalGitError::StaleSnapshot);
            }
            let mut input = expected.path.raw.clone();
            input.push(0);
            self.git.run_worktree_command_with_input_and_env(
                "stage one conflict resolution",
                vec![
                    "add".into(),
                    "--pathspec-from-file=-".into(),
                    "--pathspec-file-nul".into(),
                ],
                Some(&input),
                &[],
                true,
                &[0],
            )
        })?;
        self.observe_locked()?.ok_or(RebaseError::MissingOperation)
    }

    /// Exact conflict stages left by an explicit stash restoration. Unlike a
    /// rebase conflict, there is deliberately no active rebase identity.
    pub fn stash_conflicts(&self, operation_id: &str) -> Result<Vec<ConflictFile>> {
        let _record_guard = self.lock_record()?;
        let record = self.require_record(operation_id)?;
        if record.stash_restore != StashRestoreState::Conflicted {
            return Err(RebaseError::UnsupportedState(
                "the recorded stash restoration is not conflicted".into(),
            ));
        }
        if self.git.snapshot()?.operation.rebase != RebaseState::None {
            return Err(RebaseError::UnsupportedState(
                "a rebase operation is active while restoring a stash".into(),
            ));
        }
        self.read_conflicts()
    }

    pub fn stage_stash_resolution(
        &self,
        operation_id: &str,
        expected: &ConflictFile,
        guard: &SnapshotGuard,
    ) -> Result<OperationView> {
        let _record_guard = self.lock_record()?;
        let record = self.require_record(operation_id)?;
        if record.stash_restore != StashRestoreState::Conflicted {
            return Err(RebaseError::UnsupportedState(
                "the recorded stash restoration is not conflicted".into(),
            ));
        }
        self.git.with_guarded_worktree(guard, |snapshot| {
            if snapshot.operation.rebase != RebaseState::None {
                return Err(LocalGitError::StaleSnapshot);
            }
            let current = self
                .read_conflicts()
                .map_err(|_| LocalGitError::StaleSnapshot)?;
            let candidate = current
                .iter()
                .find(|item| item.path.raw == expected.path.raw)
                .ok_or(LocalGitError::StaleSnapshot)?;
            if candidate != expected {
                return Err(LocalGitError::StaleSnapshot);
            }
            let mut input = expected.path.raw.clone();
            input.push(0);
            self.git.run_worktree_command_with_input_and_env(
                "stage one stash conflict resolution",
                vec![
                    "add".into(),
                    "--pathspec-from-file=-".into(),
                    "--pathspec-file-nul".into(),
                ],
                Some(&input),
                &[],
                true,
                &[0],
            )
        })?;
        Ok(view(&record, None))
    }

    pub fn finish_stash_restore(
        &self,
        operation_id: &str,
        guard: &SnapshotGuard,
    ) -> Result<OperationView> {
        let _record_guard = self.lock_record()?;
        let mut record = self.require_record(operation_id)?;
        if record.stash_restore != StashRestoreState::Conflicted {
            return Err(RebaseError::UnsupportedState(
                "the recorded stash restoration is not conflicted".into(),
            ));
        }
        self.git.with_guarded_worktree(guard, |snapshot| {
            if snapshot.operation.rebase != RebaseState::None || !snapshot.conflicts.is_empty() {
                return Err(LocalGitError::StaleSnapshot);
            }
            Ok(())
        })?;
        record.stash_restore = StashRestoreState::Completed;
        record.evidence.push(self.evidence(
            "stash conflicts were explicitly resolved; exact stash object remains retained".into(),
        )?);
        self.save_record(&record)?;
        Ok(view(&record, None))
    }

    pub fn restore_stash(
        &self,
        operation_id: &str,
        guard: &SnapshotGuard,
    ) -> Result<OperationView> {
        let _record_guard = self.lock_record()?;
        let mut record = self.require_record(operation_id)?;
        if !matches!(
            record.state,
            OperationState::Completed | OperationState::Aborted
        ) {
            return Err(RebaseError::UnsupportedState(
                "stash restore is only available after completion or abort".into(),
            ));
        }
        let stash = record.stash.clone().ok_or_else(|| {
            RebaseError::UnsupportedState("this operation has no recorded stash".into())
        })?;
        if record.stash_restore != StashRestoreState::NotStarted {
            return Err(RebaseError::UnsupportedState(
                "stash restore was already dispatched and will not be replayed".into(),
            ));
        }
        record.attempt += 1;
        record.action = AttemptAction::RestoreStash;
        record.stash_restore = StashRestoreState::Running;
        self.save_record(&record)?;
        let result = self.git.with_guarded_worktree(guard, |actual| {
            require_no_local_changes(actual, true)?;
            self.git.run_worktree_command(
                "restore exact rebase stash",
                vec![
                    "stash".into(),
                    "apply".into(),
                    "--index".into(),
                    stash.oid.clone().into(),
                ],
                true,
                &[0],
            )
        });
        let snapshot = self.git.snapshot()?;
        record.action = AttemptAction::None;
        if !snapshot.conflicts.is_empty() {
            record.stash_restore = StashRestoreState::Conflicted;
            record.evidence.push(self.evidence(
                "exact stash application stopped with conflicts; stash retained".into(),
            )?);
            self.save_record(&record)?;
            return Ok(view(&record, None));
        }
        if let Err(error) = result {
            record.stash_restore = StashRestoreState::FailedUncertain;
            record.state = OperationState::FailedUncertain;
            record.evidence.push(self.evidence(
                "exact stash application outcome is uncertain; retry forbidden".into(),
            )?);
            self.save_record(&record)?;
            return Err(RebaseError::PostStartUncertain {
                action: "stash restoration",
                detail: error.to_string(),
            });
        }
        record.stash_restore = StashRestoreState::Completed;
        record
            .evidence
            .push(self.evidence("exact stash object restored successfully and retained".into())?);
        self.save_record(&record)?;
        Ok(view(&record, None))
    }

    fn control(
        &self,
        operation_id: &str,
        expected_active: &ActiveOperationIdentity,
        guard: &SnapshotGuard,
        action: AttemptAction,
        label: &'static str,
        flag: &'static str,
    ) -> Result<OperationView> {
        let mut record = self.require_record(operation_id)?;
        self.require_owned_active(&record, expected_active)?;
        record.attempt += 1;
        record.action = action;
        self.save_record(&record)?;
        let helpers = self.helper_paths(operation_id);
        let environment = self.helper_environment(operation_id, &helpers);
        let editor = shell_quote(&helpers.message_editor);
        let result = self.git.with_guarded_worktree(guard, |_| {
            self.require_active_identity(expected_active)?;
            self.git.run_worktree_command_with_input_and_env(
                label,
                vec![
                    "-c".into(),
                    format!("core.editor={editor}").into(),
                    "rebase".into(),
                    flag.into(),
                ],
                None,
                &environment,
                true,
                &[0],
            )
        });
        record.action = AttemptAction::None;
        record
            .evidence
            .push(self.evidence(format!("{label} returned"))?);
        self.save_record(&record)?;
        self.reconcile_after_command(label, result.err())
    }

    fn reconcile_after_command(
        &self,
        action: &'static str,
        error: Option<LocalGitError>,
    ) -> Result<OperationView> {
        let Some(mut record) = self.try_load_record()? else {
            return Err(RebaseError::MissingOperation);
        };
        self.reconcile_record(&mut record)?;
        self.save_record(&record)?;
        let view = view(&record, self.active_identity(&record.operation_id)?);
        if let Some(error) = error {
            if is_certain_prestart_error(&error) {
                return Err(RebaseError::Git(error));
            }
            if view.state == OperationState::FailedUncertain
                || view.state == OperationState::Prepared
            {
                record.state = OperationState::FailedUncertain;
                record.evidence.push(
                    self.evidence(format!("{action} returned an uncertain post-start error"))?,
                );
                self.save_record(&record)?;
                return Err(RebaseError::PostStartUncertain {
                    action,
                    detail: error.to_string(),
                });
            }
        }
        Ok(view)
    }

    fn reconcile_record(&self, record: &mut StoredRecord) -> Result<()> {
        if record.scope != self.scope {
            return Err(RebaseError::IdentityChanged(
                "stored scope does not match this checkout".into(),
            ));
        }
        if let Err(error) = self.verify_scope_identity() {
            record.state = OperationState::FailedUncertain;
            record.evidence.push(TransitionEvidence {
                observed_head: HeadState::Detached {
                    oid: record.inventory.head_oid.clone(),
                },
                observed_rebase: RebaseState::None,
                dispatch_proof: self
                    .dispatch_proof_matches(&record.operation_id)
                    .unwrap_or(false),
                owned_active_marker: false,
                note: error.to_string(),
            });
            return Ok(());
        }
        if record.state == OperationState::FailedUncertain {
            return Ok(());
        }
        let previous_state = record.state;
        let snapshot = self.git.snapshot()?;
        let proof = self.dispatch_proof_matches(&record.operation_id)?;
        let active = self.active_identity(&record.operation_id)?;
        if snapshot.operation.rebase != RebaseState::None {
            let Some(active) = active else {
                record.state = OperationState::FailedUncertain;
                record.evidence.push(self.evidence(
                    "active Git rebase lacks this operation's ownership marker".into(),
                )?);
                return Ok(());
            };
            if !proof || !active_matches_record(&active, record) {
                record.state = OperationState::FailedUncertain;
                record
                    .evidence
                    .push(self.evidence("active Git rebase is unrelated or replaced".into())?);
                return Ok(());
            }
            if snapshot.local_branch_oids.get(&record.inventory.branch)
                != Some(&record.inventory.head_oid)
            {
                record.state = OperationState::FailedUncertain;
                record
                    .evidence
                    .push(self.evidence("branch ref moved externally during the rebase".into())?);
                return Ok(());
            }
            record.state = if !snapshot.conflicts.is_empty() {
                OperationState::Conflicted
            } else if active.stopped_oid.is_some() && active.stop_is_edit {
                OperationState::PausedForEdit
            } else {
                OperationState::Running
            };
            return Ok(());
        }
        if matches!(
            record.action,
            AttemptAction::CreateStash | AttemptAction::RestoreStash
        ) {
            record.state = OperationState::FailedUncertain;
            if record.action == AttemptAction::RestoreStash {
                record.stash_restore = StashRestoreState::FailedUncertain;
            }
            record.evidence.push(self.evidence(
                "saved stash intent cannot establish whether Git ran; replay forbidden".into(),
            )?);
            return Ok(());
        }
        if !proof {
            // A saved intent is not proof Git ran.
            record.state = OperationState::Prepared;
            return Ok(());
        }
        match &snapshot.head {
            HeadState::Attached { branch, oid } if branch == &record.inventory.branch => {
                if oid == &record.inventory.head_oid {
                    record.state = OperationState::Aborted;
                } else if self.is_ancestor(&record.inventory.base_oid, oid)? {
                    record.state = OperationState::Completed;
                    record.resulting_commits = self
                        .inventory_at(&record.inventory.base_oid, branch, oid)?
                        .commits;
                } else {
                    record.state = OperationState::FailedUncertain;
                }
            }
            _ => record.state = OperationState::FailedUncertain,
        }
        if record.state == OperationState::Completed && previous_state != OperationState::Completed
        {
            record.evidence.push(self.evidence(
                "Git rebase markers disappeared and rewritten branch ancestry was verified".into(),
            )?);
        }
        Ok(())
    }

    fn inventory(&self, base_oid: &str, snapshot: &LocalSnapshot) -> Result<CommitInventory> {
        let (branch, head_oid) = match &snapshot.head {
            HeadState::Attached { branch, oid } => (branch.clone(), oid.clone()),
            HeadState::Detached { .. } => {
                return Err(RebaseError::UnsupportedState(
                    "detached HEAD cannot identify the branch to rewrite".into(),
                ));
            }
            HeadState::Unborn { .. } => {
                return Err(RebaseError::UnsupportedState(
                    "unborn branches have no commits to rewrite".into(),
                ));
            }
        };
        self.require_exact_commit(base_oid)?;
        if base_oid == head_oid {
            return Err(RebaseError::UnsupportedState(
                "the base equals HEAD; the rebase inventory would be empty".into(),
            ));
        }
        if !self.is_ancestor(base_oid, &head_oid)? {
            return Err(RebaseError::UnsupportedState(
                "the explicit base is not an ancestor of attached branch HEAD".into(),
            ));
        }
        let inventory = self.inventory_at(base_oid, &branch, &head_oid)?;
        if inventory.commits.is_empty() {
            return Err(RebaseError::UnsupportedState(
                "the rebase inventory is empty".into(),
            ));
        }
        Ok(inventory)
    }

    fn inventory_at(
        &self,
        base_oid: &str,
        branch: &str,
        head_oid: &str,
    ) -> Result<CommitInventory> {
        let range = format!("{base_oid}..{head_oid}");
        let raw = self.git.run_worktree_command(
            "inventory linear rebase commits",
            vec![
                "rev-list".into(),
                "--reverse".into(),
                "--parents".into(),
                range.into(),
            ],
            false,
            &[0],
        )?;
        let lines: Vec<&[u8]> = raw
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        if lines.len() > MAX_COMMITS {
            return Err(RebaseError::UnsupportedState(
                "rebase inventory exceeds the commit bound".into(),
            ));
        }
        let mut commits = Vec::with_capacity(lines.len());
        let mut aggregate = 0usize;
        for line in lines {
            let fields: Vec<&[u8]> = line.split(|byte| *byte == b' ').collect();
            if fields.len() < 2 {
                return Err(RebaseError::UnsupportedState(
                    "root commits require an external workflow".into(),
                ));
            }
            let oid = ascii_oid(fields[0])?;
            let parents: Vec<String> = fields[1..]
                .iter()
                .map(|field| ascii_oid(field))
                .collect::<Result<_>>()?;
            if parents.len() != 1 {
                return Ok(CommitInventory {
                    base_oid: base_oid.into(),
                    branch: branch.into(),
                    head_oid: head_oid.into(),
                    commits: vec![CommitEntry {
                        oid,
                        parent_oid: parents.join(" "),
                        message_raw: Vec::new(),
                        message: String::new(),
                    }],
                });
            }
            let message_raw = self.git.run_worktree_command(
                "read inventoried commit message",
                vec![
                    "show".into(),
                    "-s".into(),
                    "--format=%B".into(),
                    oid.clone().into(),
                ],
                false,
                &[0],
            )?;
            aggregate = aggregate.saturating_add(message_raw.len());
            if aggregate > MAX_INVENTORY_MESSAGE_BYTES {
                return Err(RebaseError::UnsupportedState(
                    "commit messages exceed the inventory bound".into(),
                ));
            }
            commits.push(CommitEntry {
                oid,
                parent_oid: parents[0].clone(),
                message: String::from_utf8_lossy(&message_raw)
                    .trim_end_matches('\n')
                    .to_owned(),
                message_raw,
            });
        }
        Ok(CommitInventory {
            base_oid: base_oid.into(),
            branch: branch.into(),
            head_oid: head_oid.into(),
            commits,
        })
    }

    fn parents(&self, oid: &str) -> Result<Vec<String>> {
        let raw = self.git.run_worktree_command(
            "read commit parents",
            vec![
                "rev-list".into(),
                "--parents".into(),
                "-n".into(),
                "1".into(),
                oid.into(),
            ],
            false,
            &[0],
        )?;
        let mut fields = raw.split(|byte| byte.is_ascii_whitespace());
        let _ = fields.next();
        fields
            .filter(|field| !field.is_empty())
            .map(ascii_oid)
            .collect()
    }

    fn new_record(&self, operation_id: &str, inventory: &CommitInventory) -> StoredRecord {
        StoredRecord {
            operation_id: operation_id.into(),
            attempt: 0,
            state: OperationState::Prepared,
            scope: self.scope.clone(),
            inventory: inventory.clone(),
            plan: None,
            action: AttemptAction::None,
            dispatch_acknowledged: false,
            stash_before_oid: None,
            stash_expected_untracked: false,
            stash: None,
            stash_restore: StashRestoreState::NotStarted,
            split: None,
            resulting_commits: Vec::new(),
            evidence: Vec::new(),
        }
    }

    fn require_record(&self, operation_id: &str) -> Result<StoredRecord> {
        let record = self.load_record()?;
        if record.operation_id != operation_id {
            return Err(RebaseError::StaleOperation);
        }
        Ok(record)
    }

    fn try_load_record(&self) -> Result<Option<StoredRecord>> {
        match fs::metadata(&self.record_path) {
            Ok(_) => self.load_record().map(Some),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(source) => Err(RebaseError::Io {
                context: "inspect rebase record",
                source,
            }),
        }
    }

    fn load_record(&self) -> Result<StoredRecord> {
        let metadata =
            fs::symlink_metadata(&self.record_path).map_err(io("inspect rebase record"))?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_RECORD_BYTES {
            return Err(RebaseError::CorruptRecord(
                "record is not a bounded regular file".into(),
            ));
        }
        let bytes = fs::read(&self.record_path).map_err(io("read rebase record"))?;
        let envelope: RecordEnvelope = serde_json::from_slice(&bytes)
            .map_err(|error| RebaseError::CorruptRecord(error.to_string()))?;
        if envelope.version > RECORD_VERSION {
            return Err(RebaseError::FutureRecord {
                version: envelope.version,
            });
        }
        if envelope.version != RECORD_VERSION {
            return Err(RebaseError::CorruptRecord(
                "unsupported legacy record".into(),
            ));
        }
        let payload = serde_json::to_vec(&envelope.payload)
            .map_err(|error| RebaseError::CorruptRecord(error.to_string()))?;
        if hex_digest(&payload) != envelope.checksum {
            return Err(RebaseError::CorruptRecord("checksum mismatch".into()));
        }
        let record: StoredRecord = serde_json::from_value(envelope.payload)
            .map_err(|error| RebaseError::CorruptRecord(error.to_string()))?;
        validate_record(&record)?;
        Ok(record)
    }

    fn save_record(&self, record: &StoredRecord) -> Result<()> {
        validate_record(record)?;
        let payload = serde_json::to_value(record)
            .map_err(|error| RebaseError::CorruptRecord(error.to_string()))?;
        let canonical = serde_json::to_vec(&payload)
            .map_err(|error| RebaseError::CorruptRecord(error.to_string()))?;
        let envelope = RecordEnvelope {
            version: RECORD_VERSION,
            checksum: hex_digest(&canonical),
            payload,
        };
        let bytes = serde_json::to_vec(&envelope)
            .map_err(|error| RebaseError::CorruptRecord(error.to_string()))?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(RebaseError::InvalidInput(
                "rebase record exceeds its bound".into(),
            ));
        }
        atomic_private_write(&self.record_path, &bytes)
    }

    fn helper_paths(&self, operation_id: &str) -> HelperPaths {
        let root = self.partition_dir.join(format!("operation-{operation_id}"));
        HelperPaths {
            todo: root.join("todo"),
            messages: root.join("messages"),
            sequence_editor: root.join("sequence editor 'safe'.sh"),
            message_editor: root.join("message editor 'safe'.sh"),
            proof: root.join("dispatch-proof"),
            root,
        }
    }

    fn write_helpers(&self, operation_id: &str, plan: &RebasePlan) -> Result<()> {
        let paths = self.helper_paths(operation_id);
        create_private_dir(&paths.root)?;
        create_private_dir(&paths.messages)?;
        let mut todo = Vec::new();
        for step in &plan.steps {
            let verb = match step.action {
                PlanAction::Pick => "pick",
                PlanAction::Squash => "squash",
                PlanAction::Fixup => "fixup",
                PlanAction::Drop => "drop",
                PlanAction::Reword { .. } => "reword",
                PlanAction::Edit => "edit",
            };
            todo.extend_from_slice(verb.as_bytes());
            todo.push(b' ');
            todo.extend_from_slice(step.commit_oid.as_bytes());
            todo.push(b'\n');
            if let PlanAction::Reword { ref message } = step.action {
                write_private_file(
                    &paths.messages.join(&step.commit_oid),
                    message.as_bytes(),
                    false,
                )?;
            }
        }
        write_private_file(&paths.todo, &todo, false)?;
        let sequence = b"#!/bin/sh\nset -eu\ntest \"$#\" -eq 1\ncp \"$CIBERGIT_TODO_FILE\" \"$1\"\nprintf '%s\\n' \"$CIBERGIT_OPERATION_ID\" > \"$CIBERGIT_DISPATCH_PROOF.tmp\"\nmv \"$CIBERGIT_DISPATCH_PROOF.tmp\" \"$CIBERGIT_DISPATCH_PROOF\"\nprintf '%s\\n' \"$CIBERGIT_OPERATION_ID\" > \"$CIBERGIT_GIT_DIR/rebase-merge/cibergit-operation.tmp\"\nmv \"$CIBERGIT_GIT_DIR/rebase-merge/cibergit-operation.tmp\" \"$CIBERGIT_GIT_DIR/rebase-merge/cibergit-operation\"\n";
        let message = b"#!/bin/sh\nset -eu\ntest \"$#\" -eq 1\ngitdir=$(git rev-parse --git-path rebase-merge)\noid=\nwhile IFS=' ' read -r action candidate rest; do oid=$candidate; done < \"$gitdir/done\"\ncase \"$oid\" in *[!0-9a-fA-F]*|'') exit 0;; esac\nsource=$CIBERGIT_MESSAGES_DIR/$oid\nif test -f \"$source\"; then cp \"$source\" \"$1\"; fi\n";
        write_private_file(&paths.sequence_editor, sequence, true)?;
        write_private_file(&paths.message_editor, message, true)?;
        Ok(())
    }

    fn helper_environment(
        &self,
        operation_id: &str,
        paths: &HelperPaths,
    ) -> Vec<(OsString, OsString)> {
        vec![
            (
                "GIT_SEQUENCE_EDITOR".into(),
                shell_quote(&paths.sequence_editor).into(),
            ),
            (
                "GIT_EDITOR".into(),
                shell_quote(&paths.message_editor).into(),
            ),
            ("CIBERGIT_OPERATION_ID".into(), operation_id.into()),
            (
                "CIBERGIT_TODO_FILE".into(),
                paths.todo.as_os_str().to_owned(),
            ),
            (
                "CIBERGIT_MESSAGES_DIR".into(),
                paths.messages.as_os_str().to_owned(),
            ),
            (
                "CIBERGIT_DISPATCH_PROOF".into(),
                paths.proof.as_os_str().to_owned(),
            ),
            (
                "CIBERGIT_GIT_DIR".into(),
                self.scope.git_dir.as_os_str().to_owned(),
            ),
        ]
    }

    fn dispatch_proof_matches(&self, operation_id: &str) -> Result<bool> {
        read_small_optional(&self.helper_paths(operation_id).proof, 4096)
            .map(|value| value.is_some_and(|bytes| trim_lf(&bytes) == operation_id.as_bytes()))
    }

    fn active_identity(&self, operation_id: &str) -> Result<Option<ActiveOperationIdentity>> {
        let snapshot = self.git.snapshot()?;
        if snapshot.operation.rebase == RebaseState::None {
            return Ok(None);
        }
        let dir = match snapshot.operation.rebase {
            RebaseState::Merge => self.scope.git_dir.join("rebase-merge"),
            RebaseState::Apply => self.scope.git_dir.join("rebase-apply"),
            RebaseState::None => unreachable!(),
        };
        let marker = read_small_optional(&dir.join("cibergit-operation"), 4096)?;
        let Some(marker) = marker.filter(|bytes| trim_lf(bytes) == operation_id.as_bytes()) else {
            return Ok(None);
        };
        let head_name = required_text_file(&dir.join("head-name"))?;
        let onto_oid = required_oid_file(&dir.join("onto"))?;
        let original_head_oid = required_oid_file(&dir.join("orig-head"))?;
        let stopped_oid = optional_oid_file(&dir.join("stopped-sha"))?;
        let rebase_head_oid = self.optional_ref_oid("REBASE_HEAD")?;
        let done = read_small_optional(&dir.join("done"), MAX_RECORD_BYTES)?.unwrap_or_default();
        let stop_is_edit = done
            .split(|byte| *byte == b'\n')
            .rev()
            .find(|line| !line.is_empty())
            .is_some_and(|line| line.starts_with(b"edit ") || line.starts_with(b"e "));
        Ok(Some(ActiveOperationIdentity {
            operation_id: operation_id.into(),
            head_name,
            onto_oid,
            original_head_oid,
            stopped_oid,
            rebase_head_oid,
            stop_is_edit,
            todo_sha256: digest_optional_file(&dir.join("git-rebase-todo"))?,
            done_sha256: hex_digest(&done),
            ownership_marker_sha256: hex_digest(&marker),
        }))
    }

    fn require_owned_active(
        &self,
        record: &StoredRecord,
        expected: &ActiveOperationIdentity,
    ) -> Result<()> {
        if expected.operation_id != record.operation_id || !active_matches_record(expected, record)
        {
            return Err(RebaseError::StaleOperation);
        }
        Ok(self.require_active_identity(expected)?)
    }

    fn require_active_identity(
        &self,
        expected: &ActiveOperationIdentity,
    ) -> std::result::Result<(), LocalGitError> {
        let actual = self
            .active_identity(&expected.operation_id)
            .map_err(|_| LocalGitError::StaleSnapshot)?;
        if actual.as_ref() != Some(expected) {
            return Err(LocalGitError::StaleSnapshot);
        }
        Ok(())
    }

    fn read_conflicts(&self) -> Result<Vec<ConflictFile>> {
        let raw = self.git.run_worktree_command(
            "read exact conflict stages",
            vec![
                "ls-files".into(),
                "--unmerged".into(),
                "--stage".into(),
                "-z".into(),
            ],
            false,
            &[0],
        )?;
        let mut grouped: BTreeMap<Vec<u8>, [Option<RawStage>; 3]> = BTreeMap::new();
        for item in raw.split(|byte| *byte == 0).filter(|item| !item.is_empty()) {
            let tab = item.iter().position(|byte| *byte == b'\t').ok_or_else(|| {
                RebaseError::InvalidInput("malformed ls-files stage output".into())
            })?;
            let meta = std::str::from_utf8(&item[..tab]).map_err(|_| {
                RebaseError::InvalidInput("non-UTF-8 conflict stage metadata".into())
            })?;
            let fields: Vec<&str> = meta.split(' ').collect();
            if fields.len() != 3 {
                return Err(RebaseError::InvalidInput(
                    "malformed conflict stage metadata".into(),
                ));
            }
            let stage: usize = fields[2]
                .parse()
                .map_err(|_| RebaseError::InvalidInput("invalid conflict stage number".into()))?;
            if !(1..=3).contains(&stage) {
                return Err(RebaseError::InvalidInput(
                    "invalid conflict stage number".into(),
                ));
            }
            validate_full_oid(fields[1])?;
            grouped.entry(item[tab + 1..].to_vec()).or_default()[stage - 1] = Some(RawStage {
                mode: fields[0].into(),
                oid: fields[1].into(),
            });
        }
        let all = grouped.clone();
        let mut result = Vec::new();
        for (raw_path, stages) in grouped {
            let path = GitPath::from_raw(raw_path.clone())?;
            let related: Vec<GitPath> = all
                .iter()
                .filter(|(other, other_stages)| {
                    **other != raw_path
                        && stages.iter().flatten().any(|stage| {
                            other_stages
                                .iter()
                                .flatten()
                                .any(|candidate| candidate.oid == stage.oid)
                        })
                })
                .map(|(other, _)| GitPath::from_raw(other.clone()))
                .collect::<std::result::Result<_, _>>()?;
            let modes: BTreeSet<&str> = stages
                .iter()
                .flatten()
                .map(|stage| stage.mode.as_str())
                .collect();
            let kind = if !related.is_empty() {
                ConflictKind::RenameOrDelete { related_paths: related, explanation: "Git exposes rename conflicts as path-level unmerged stages; related paths share a stage object. Missing stages are deletions.".into() }
            } else if modes.len() > 1 {
                ConflictKind::TypeChange
            } else {
                match (stages[0].is_some(), stages[1].is_some(), stages[2].is_some()) {
                (true, true, true) => ConflictKind::BothModified,
                (false, true, true) => ConflictKind::AddedByBoth,
                (true, false, true) => ConflictKind::DeletedByOurs,
                (true, true, false) => ConflictKind::DeletedByTheirs,
                _ => ConflictKind::RenameOrDelete { related_paths: Vec::new(), explanation: "an unusual unmerged stage combination represents a rename, delete, or directory/file conflict".into() },
            }
            };
            let [base, ours, theirs] = stages;
            result.push(ConflictFile {
                kind,
                path: path.clone(),
                base: self.conflict_stage(base, &raw_path)?,
                ours: self.conflict_stage(ours, &raw_path)?,
                theirs: self.conflict_stage(theirs, &raw_path)?,
                disk: disk_generation(&self.scope.checkout.join(OsStr::from_bytes(&raw_path)))?,
            });
        }
        Ok(result)
    }

    fn conflict_stage(&self, stage: Option<RawStage>, raw_path: &[u8]) -> Result<ConflictStage> {
        let Some(stage) = stage else {
            return Ok(ConflictStage {
                oid: None,
                mode: None,
                content: BlobContent::Deleted,
            });
        };
        let RawStage { mode, oid } = stage;
        let size_raw = self.git.run_worktree_command(
            "read conflict blob size",
            vec!["cat-file".into(), "-s".into(), oid.clone().into()],
            false,
            &[0],
        )?;
        let size: u64 = std::str::from_utf8(trim_lf(&size_raw))
            .map_err(|_| RebaseError::InvalidInput("invalid blob size".into()))?
            .parse()
            .map_err(|_| RebaseError::InvalidInput("invalid blob size".into()))?;
        let content = if size > CONFLICT_BLOB_LIMIT {
            BlobContent::TooLarge {
                bytes: size,
                limit: CONFLICT_BLOB_LIMIT,
            }
        } else {
            let bytes = self.git.run_worktree_command(
                "read conflict blob",
                vec!["cat-file".into(), "blob".into(), oid.clone().into()],
                false,
                &[0],
            )?;
            if mode == "120000" {
                BlobContent::Symlink(bytes)
            } else if is_media_path(raw_path) {
                BlobContent::Media
            } else if bytes.contains(&0) {
                BlobContent::Binary
            } else {
                match String::from_utf8(bytes) {
                    Ok(text) => BlobContent::Utf8(text),
                    Err(_) => BlobContent::NonUtf8,
                }
            }
        };
        Ok(ConflictStage {
            oid: Some(oid),
            mode: Some(mode),
            content,
        })
    }

    fn verify_scope_identity(&self) -> Result<()> {
        for (label, path, expected) in [
            (
                "checkout",
                &self.scope.checkout,
                &self.scope.checkout_identity,
            ),
            (
                "Git directory",
                &self.scope.git_dir,
                &self.scope.git_dir_identity,
            ),
            (
                "common Git directory",
                &self.scope.common_git_dir,
                &self.scope.common_git_dir_identity,
            ),
        ] {
            if filesystem_identity(path)? != *expected {
                return Err(RebaseError::IdentityChanged(format!(
                    "{label} device/inode no longer matches"
                )));
            }
        }
        let reopened = LocalGit::open(&self.scope.checkout)?;
        if reopened.root() != self.scope.checkout
            || reopened.git_dir() != self.scope.git_dir
            || reopened.common_git_dir() != self.scope.common_git_dir
        {
            return Err(RebaseError::IdentityChanged(
                "canonical Git paths no longer match".into(),
            ));
        }
        Ok(())
    }

    fn require_exact_commit(&self, oid: &str) -> Result<()> {
        let kind = self.git.run_worktree_command(
            "validate explicit rebase base",
            vec!["cat-file".into(), "-t".into(), oid.into()],
            false,
            &[0],
        )?;
        if trim_lf(&kind) != b"commit" {
            return Err(RebaseError::InvalidInput(
                "base object is not a commit".into(),
            ));
        }
        Ok(())
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        match self.git.run_worktree_command(
            "validate rebase ancestry",
            vec![
                "merge-base".into(),
                "--is-ancestor".into(),
                ancestor.into(),
                descendant.into(),
            ],
            false,
            &[0],
        ) {
            Ok(_) => Ok(true),
            Err(LocalGitError::CommandFailed {
                exit_code: Some(1), ..
            }) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn optional_ref_oid(&self, reference: &str) -> Result<Option<String>> {
        let result = self.git.run_worktree_command(
            "read optional Git object",
            vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                reference.into(),
            ],
            false,
            &[0, 1],
        )?;
        if result.is_empty() {
            return Ok(None);
        }
        let oid = ascii_oid(trim_lf(&result))?;
        Ok(Some(oid))
    }

    fn acknowledged_stash_oid(&self, record: &StoredRecord) -> Result<Option<String>> {
        let marker = stash_marker(&record.operation_id);
        let raw = self.git.run_worktree_command(
            "bind exact rebase stash from reflog",
            vec![
                "reflog".into(),
                "show".into(),
                "-z".into(),
                "--format=%H%x00%gs%x00".into(),
                "refs/stash".into(),
            ],
            false,
            &[0],
        )?;
        let fields: Vec<&[u8]> = raw
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .collect();
        if !fields.len().is_multiple_of(2) {
            return Err(RebaseError::InvalidInput(
                "malformed stash reflog evidence".into(),
            ));
        }
        let mut matches = Vec::new();
        for pair in fields.chunks_exact(2) {
            let oid = ascii_oid(pair[0])?;
            if Some(&oid) == record.stash_before_oid.as_ref()
                || !pair[1].ends_with(marker.as_bytes())
            {
                continue;
            }
            let parents = self.parents(&oid)?;
            let expected_parent_count = if record.stash_expected_untracked {
                3
            } else {
                2
            };
            if parents.len() != expected_parent_count
                || parents.first() != Some(&record.inventory.head_oid)
            {
                continue;
            }
            let index_parents = self.parents(&parents[1])?;
            if index_parents.as_slice() != [record.inventory.head_oid.as_str()] {
                continue;
            }
            if record.stash_expected_untracked && !self.parents(&parents[2])?.is_empty() {
                continue;
            }
            let message = self.git.run_worktree_command(
                "verify exact rebase stash message",
                vec![
                    "show".into(),
                    "-s".into(),
                    "--format=%B".into(),
                    oid.clone().into(),
                ],
                false,
                &[0],
            )?;
            if trim_lf(&message).ends_with(marker.as_bytes()) {
                matches.push(oid);
            }
        }
        if matches.len() == 1 {
            Ok(matches.pop())
        } else {
            Ok(None)
        }
    }

    fn rev_parse(&self, expression: &str) -> std::result::Result<String, LocalGitError> {
        let raw = self.git.run_worktree_command(
            "read exact rebase object",
            vec!["rev-parse".into(), "--verify".into(), expression.into()],
            false,
            &[0],
        )?;
        let text = std::str::from_utf8(trim_lf(&raw))
            .map_err(|_| LocalGitError::MalformedOutput("non-UTF-8 object ID"))?
            .to_owned();
        if !matches!(text.len(), 40 | 64) || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(LocalGitError::MalformedOutput("invalid object ID"));
        }
        Ok(text)
    }

    fn first_parent_count(
        &self,
        from: &str,
        to: &str,
    ) -> std::result::Result<usize, LocalGitError> {
        let range = format!("{from}..{to}");
        let raw = self.git.run_worktree_command(
            "count split replacement commits",
            vec![
                "rev-list".into(),
                "--first-parent".into(),
                "--count".into(),
                range.into(),
            ],
            false,
            &[0],
        )?;
        std::str::from_utf8(trim_lf(&raw))
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or(LocalGitError::MalformedOutput("invalid commit count"))
    }

    fn evidence(&self, note: String) -> Result<TransitionEvidence> {
        let snapshot = self.git.snapshot()?;
        let active = self.active_identity_for_any_marker()?;
        Ok(TransitionEvidence {
            observed_head: snapshot.head,
            observed_rebase: snapshot.operation.rebase,
            dispatch_proof: self.try_load_record()?.is_some_and(|record| {
                self.dispatch_proof_matches(&record.operation_id)
                    .unwrap_or(false)
            }),
            owned_active_marker: active,
            note,
        })
    }

    fn active_identity_for_any_marker(&self) -> Result<bool> {
        Ok(read_small_optional(
            &self.scope.git_dir.join("rebase-merge/cibergit-operation"),
            4096,
        )?
        .is_some())
    }

    fn fail_after_start(
        &self,
        mut record: StoredRecord,
        action: &'static str,
        detail: String,
    ) -> Result<OperationView> {
        record.state = OperationState::FailedUncertain;
        record
            .evidence
            .push(self.evidence(format!("{action} outcome is uncertain"))?);
        self.save_record(&record)?;
        Err(RebaseError::PostStartUncertain { action, detail })
    }

    fn lock_record(&self) -> Result<RecordAuthorityGuard<'_>> {
        let started = Instant::now();
        let process_guard = loop {
            match self.record_lock.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::Poisoned(_)) => {
                    return Err(RebaseError::CorruptRecord(
                        "operation record lock is poisoned".into(),
                    ));
                }
                Err(TryLockError::WouldBlock) => {
                    if started.elapsed() >= JOURNAL_LOCK_TIMEOUT {
                        return Err(RebaseError::JournalBusy);
                    }
                    thread::sleep(JOURNAL_LOCK_RETRY);
                }
            }
        };
        let descriptor = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(PRIVATE_FILE_MODE)
            .open(self.partition_dir.join("operation.lock"))
            .map_err(io("open rebase journal lock"))?;
        loop {
            // SAFETY: descriptor is open for the guard's lifetime and flock does
            // not retain a userspace pointer. LOCK_NB prevents kernel blocking.
            if unsafe { flock(descriptor.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
                return Ok(RecordAuthorityGuard {
                    _process_guard: process_guard,
                    _descriptor: descriptor,
                });
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != ErrorKind::WouldBlock {
                return Err(RebaseError::Io {
                    context: "acquire rebase journal lock",
                    source: error,
                });
            }
            if started.elapsed() >= JOURNAL_LOCK_TIMEOUT {
                return Err(RebaseError::JournalBusy);
            }
            thread::sleep(JOURNAL_LOCK_RETRY);
        }
    }

    #[cfg(test)]
    #[allow(dead_code)] // Called by the path-included integration test module.
    pub(crate) fn hold_record_lock_for_test(&self, acquired: &Path, release: &Path) -> Result<()> {
        let _record_guard = self.lock_record()?;
        fs::write(acquired, b"acquired").map_err(io("write journal test marker"))?;
        let started = Instant::now();
        while !release.exists() {
            if started.elapsed() >= Duration::from_secs(10) {
                return Err(RebaseError::JournalBusy);
            }
            thread::sleep(JOURNAL_LOCK_RETRY);
        }
        Ok(())
    }

    fn reject_replaced_checkout_records(&self, rebase_root: &Path) -> Result<()> {
        let entries = fs::read_dir(rebase_root).map_err(io("scan rebase record partitions"))?;
        for entry in entries {
            let entry = entry.map_err(io("scan rebase record partition"))?;
            let candidate = entry.path().join("operation.json");
            if candidate == self.record_path || !candidate.exists() {
                continue;
            }
            let Ok(bytes) = fs::read(&candidate) else {
                continue;
            };
            if bytes.len() as u64 > MAX_RECORD_BYTES {
                continue;
            }
            let Ok(envelope) = serde_json::from_slice::<RecordEnvelope>(&bytes) else {
                continue;
            };
            if envelope.version != RECORD_VERSION {
                continue;
            }
            let Ok(canonical) = serde_json::to_vec(&envelope.payload) else {
                continue;
            };
            if hex_digest(&canonical) != envelope.checksum {
                continue;
            }
            let Ok(record) = serde_json::from_value::<StoredRecord>(envelope.payload) else {
                continue;
            };
            if record.scope.association == self.scope.association
                && record.scope.checkout == self.scope.checkout
                && record.scope != self.scope
            {
                return Err(RebaseError::IdentityChanged(
                    "a durable record belongs to a replaced checkout at the same path".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
struct RawStage {
    mode: String,
    oid: String,
}

#[derive(Debug)]
struct HelperPaths {
    root: PathBuf,
    todo: PathBuf,
    messages: PathBuf,
    sequence_editor: PathBuf,
    message_editor: PathBuf,
    proof: PathBuf,
}

fn view(record: &StoredRecord, active: Option<ActiveOperationIdentity>) -> OperationView {
    let publish_handoff = (record.state == OperationState::Completed).then(|| PublishHandoff {
        branch: record.inventory.branch.clone(),
        original_local_head_oid: record.inventory.head_oid.clone(),
        rewritten_local_head_oid: record
            .resulting_commits
            .last()
            .map_or_else(|| record.inventory.base_oid.clone(), |tip| tip.oid.clone()),
        requires_explicit_expected_remote_oid: true,
    });
    OperationView {
        operation_id: record.operation_id.clone(), attempt: record.attempt, state: record.state,
        original_branch: record.inventory.branch.clone(), original_head_oid: record.inventory.head_oid.clone(),
        base_oid: record.inventory.base_oid.clone(), active, resulting_commits: record.resulting_commits.clone(),
        stash: record.stash.clone(), evidence: record.evidence.clone(),
        stash_restore: record.stash_restore,
        publish_warning: (record.state == OperationState::Completed).then(|| "History was rewritten locally. Publishing is a separate explicit immutable-OID force-with-lease action; descendant branch repair is not included.".into()),
        publish_handoff,
    }
}

fn require_clean_operation_and_identity(
    snapshot: &LocalSnapshot,
    inventory: &CommitInventory,
) -> std::result::Result<(), LocalGitError> {
    if snapshot.operation.rebase != RebaseState::None
        || snapshot.operation.merge
        || snapshot.operation.cherry_pick
        || snapshot.operation.revert
    {
        return Err(LocalGitError::InvalidInput(
            "another Git operation is active",
        ));
    }
    match &snapshot.head {
        HeadState::Attached { branch, oid }
            if branch == &inventory.branch && oid == &inventory.head_oid =>
        {
            Ok(())
        }
        _ => Err(LocalGitError::StaleSnapshot),
    }
}

fn require_pristine_start(
    snapshot: &LocalSnapshot,
    inventory: &CommitInventory,
) -> std::result::Result<(), LocalGitError> {
    require_clean_operation_and_identity(snapshot, inventory)?;
    require_no_local_changes(snapshot, true)
}

fn require_no_local_changes(
    snapshot: &LocalSnapshot,
    include_untracked: bool,
) -> std::result::Result<(), LocalGitError> {
    if !snapshot.staged.is_empty()
        || !snapshot.unstaged.is_empty()
        || !snapshot.conflicts.is_empty()
        || (include_untracked && !snapshot.untracked.is_empty())
    {
        return Err(LocalGitError::InvalidInput(
            "worktree/index must be clean for this transition",
        ));
    }
    Ok(())
}

fn is_certain_prestart_error(error: &LocalGitError) -> bool {
    matches!(
        error,
        LocalGitError::Io { .. }
            | LocalGitError::NotAWorktree
            | LocalGitError::InvalidInput(_)
            | LocalGitError::StaleSnapshot
            | LocalGitError::RepositoryLocked { .. }
            | LocalGitError::CommandFailed { .. }
            | LocalGitError::SnapshotContentLimit { .. }
            | LocalGitError::UnsupportedSnapshotEntry
            | LocalGitError::MalformedOutput(_)
            | LocalGitError::PoisonedLock
    )
}

fn active_matches_record(active: &ActiveOperationIdentity, record: &StoredRecord) -> bool {
    active.operation_id == record.operation_id
        && active.head_name == format!("refs/heads/{}", record.inventory.branch)
        && active.onto_oid == record.inventory.base_oid
        && active.original_head_oid == record.inventory.head_oid
}

fn validate_association(scope: &RebaseAssociation) -> Result<()> {
    for (label, value) in [
        ("provider", &scope.provider),
        ("host", &scope.host),
        ("account", &scope.account),
        ("repository", &scope.repository),
        ("change", &scope.change),
    ] {
        if value.is_empty()
            || value.len() > MAX_SCOPE_TEXT_BYTES
            || value
                .bytes()
                .any(|byte| byte == 0 || byte.is_ascii_control())
        {
            return Err(RebaseError::InvalidInput(format!(
                "invalid association {label}"
            )));
        }
    }
    Ok(())
}

fn validate_record(record: &StoredRecord) -> Result<()> {
    if record.operation_id.is_empty()
        || record.operation_id.len() > 128
        || !record
            .operation_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(RebaseError::CorruptRecord("invalid operation ID".into()));
    }
    if record.inventory.commits.is_empty()
        || record.inventory.commits.len() > MAX_COMMITS
        || record.evidence.len() > MAX_COMMITS
    {
        return Err(RebaseError::CorruptRecord(
            "record collection exceeds its bound".into(),
        ));
    }
    validate_association(&record.scope.association)
        .map_err(|error| RebaseError::CorruptRecord(error.to_string()))?;
    for path in [
        &record.scope.checkout,
        &record.scope.git_dir,
        &record.scope.common_git_dir,
    ] {
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(RebaseError::CorruptRecord(
                "record contains an invalid identity path".into(),
            ));
        }
    }
    Ok(())
}

fn validate_full_oid(oid: &str) -> Result<()> {
    if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RebaseError::InvalidInput(
            "expected a full SHA-1 or SHA-256 object ID".into(),
        ));
    }
    Ok(())
}

fn validate_message(message: &str) -> Result<()> {
    if message.is_empty() || message.len() > MAX_MESSAGE_BYTES || message.as_bytes().contains(&0) {
        return Err(RebaseError::InvalidPlan(
            "commit message must be non-empty, NUL-free, and bounded".into(),
        ));
    }
    Ok(())
}

fn ascii_oid(raw: &[u8]) -> Result<String> {
    let oid = std::str::from_utf8(raw)
        .map_err(|_| RebaseError::InvalidInput("non-UTF-8 object ID".into()))?
        .to_owned();
    validate_full_oid(&oid)?;
    Ok(oid)
}

fn filesystem_identity(path: &Path) -> Result<FilesystemIdentity> {
    let metadata = fs::symlink_metadata(path).map_err(io("inspect filesystem identity"))?;
    if metadata.file_type().is_symlink() {
        return Err(RebaseError::InvalidInput(
            "identity path must not be a symlink".into(),
        ));
    }
    Ok(FilesystemIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn disk_generation(path: &Path) -> Result<DiskGeneration> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(DiskGeneration::Missing),
        Err(source) => {
            return Err(RebaseError::Io {
                context: "inspect conflicted path",
                source,
            });
        }
    };
    if before.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(io("read conflicted symlink"))?;
        return Ok(DiskGeneration::Symlink {
            target_sha256: hex_digest(target.as_os_str().as_bytes()),
        });
    }
    if !before.file_type().is_file() {
        return Ok(DiskGeneration::Unsupported);
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x2000_0000 | 0x0000_0004)
        .open(path)
        .map_err(io("open conflicted file without following symlinks"))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = file.read(&mut buffer).map_err(io("read conflicted file"))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(io("reinspect conflicted file"))?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
    {
        return Err(RebaseError::Git(LocalGitError::StaleSnapshot));
    }
    Ok(DiskGeneration::Regular {
        sha256: format!("{:x}", hash.finalize()),
        len: after.len(),
        device: after.dev(),
        inode: after.ino(),
        modified_seconds: after.mtime(),
        modified_nanoseconds: after.mtime_nsec(),
    })
}

fn new_operation_id(scope: &ScopeIdentity, inventory: &CommitInventory) -> String {
    let mut hash = Sha256::new();
    hash.update(serde_json::to_vec(scope).unwrap_or_default());
    hash.update(inventory.head_oid.as_bytes());
    hash.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    hash.update(std::process::id().to_le_bytes());
    hash.update(UNIQUE_ID.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    format!("{:x}", hash.finalize())
}

fn stash_marker(operation_id: &str) -> String {
    format!("cibergit-rebase:{operation_id}")
}

fn create_private_dir(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(RebaseError::Io {
                context: "create private rebase directory",
                source,
            });
        }
    }
    let metadata = fs::symlink_metadata(path).map_err(io("inspect private rebase directory"))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(RebaseError::InvalidInput(
            "private rebase path is not a directory".into(),
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIR_MODE))
        .map_err(io("protect private rebase directory"))
}

fn write_private_file(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(if executable { 0o700 } else { PRIVATE_FILE_MODE });
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            return Err(RebaseError::PendingOperation(path.display().to_string()));
        }
        Err(source) => {
            return Err(RebaseError::Io {
                context: "create private helper file",
                source,
            });
        }
    };
    file.write_all(bytes)
        .map_err(io("write private helper file"))?;
    file.sync_all().map_err(io("sync private helper file"))
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| RebaseError::InvalidInput("record has no parent".into()))?;
    let name = format!(
        ".operation-{}-{}.tmp",
        std::process::id(),
        UNIQUE_ID.fetch_add(1, Ordering::Relaxed)
    );
    let temporary = parent.join(name);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(&temporary)
        .map_err(io("create atomic rebase record"))?;
    file.write_all(bytes)
        .map_err(io("write atomic rebase record"))?;
    file.sync_all().map_err(io("sync atomic rebase record"))?;
    fs::rename(&temporary, path).map_err(io("install atomic rebase record"))?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(io("sync rebase record directory"))
}

fn read_small_optional(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RebaseError::Io {
                context: "inspect rebase control file",
                source,
            });
        }
    };
    if !metadata.file_type().is_file() || metadata.len() > limit {
        return Err(RebaseError::InvalidInput(
            "rebase control file is not a bounded regular file".into(),
        ));
    }
    fs::read(path)
        .map(Some)
        .map_err(io("read rebase control file"))
}

fn required_text_file(path: &Path) -> Result<String> {
    let bytes = read_small_optional(path, 64 * 1024)?.ok_or_else(|| {
        RebaseError::InvalidInput("required rebase control file is missing".into())
    })?;
    String::from_utf8(trim_lf(&bytes).to_vec())
        .map_err(|_| RebaseError::InvalidInput("rebase control file is not UTF-8".into()))
}

fn required_oid_file(path: &Path) -> Result<String> {
    let value = required_text_file(path)?;
    validate_full_oid(&value)?;
    Ok(value)
}
fn optional_oid_file(path: &Path) -> Result<Option<String>> {
    read_small_optional(path, 4096)?
        .map(|bytes| ascii_oid(trim_lf(&bytes)))
        .transpose()
}
fn digest_optional_file(path: &Path) -> Result<String> {
    Ok(read_small_optional(path, MAX_RECORD_BYTES)?
        .map_or_else(|| hex_digest(b"missing"), |bytes| hex_digest(&bytes)))
}
fn trim_lf(mut bytes: &[u8]) -> &[u8] {
    if bytes.last() == Some(&b'\n') {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}
fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn shell_quote(path: &Path) -> String {
    let raw = path.as_os_str().as_bytes();
    let mut quoted = String::from("'");
    for &byte in raw {
        if byte == b'\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(byte as char);
        }
    }
    quoted.push('\'');
    quoted
}

fn is_media_path(path: &[u8]) -> bool {
    const EXTENSIONS: &[&[u8]] = &[
        b".avif", b".bmp", b".gif", b".heic", b".jpeg", b".jpg", b".mov", b".mp3", b".mp4",
        b".png", b".svg", b".tif", b".tiff", b".wav", b".webm", b".webp",
    ];
    let lower: Vec<u8> = path.iter().map(u8::to_ascii_lowercase).collect();
    EXTENSIONS
        .iter()
        .any(|extension| lower.ends_with(extension))
}

fn io(context: &'static str) -> impl FnOnce(std::io::Error) -> RebaseError {
    move |source| RebaseError::Io { context, source }
}
