//! Persistent PR-to-checkout associations and conservative managed-worktree cleanup.
//!
//! Associations are local application state, while Git remains authoritative for
//! the checkout's current branch, HEAD, index, files, and operation state.

use crate::local_git::{
    CommandLimits, HeadState, LocalGit, LocalGitError, OperationState, RebaseState,
    run_installed_git_at, run_installed_git_at_with_limits,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::c_int,
    fmt, fs,
    io::{ErrorKind, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SCHEMA_VERSION: u32 = 1;
const STORE_FILE: &str = "worktree-associations.json";
const MAX_STORE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_RECORDS: usize = 4_096;
const MAX_TEXT_BYTES: usize = 4_096;
const STORE_LOCK_FILE: &str = "worktree-associations.lock";
const STORE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
// Darwin constants; the application supports macOS only.
const O_NONBLOCK: c_int = 0x0004;
const O_CLOEXEC: c_int = 0x0100_0000;
const O_NOFOLLOW_ANY: c_int = 0x2000_0000;
const LOCK_EX: c_int = 0x02;
const LOCK_NB: c_int = 0x04;

unsafe extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
    fn geteuid() -> u32;
}

struct StoreAuthorityGuard<'a> {
    _process_guard: MutexGuard<'a, ()>,
    // The persistent lock inode is never unlinked. Closing the descriptor also
    // releases authority on process death, without stale PID-file recovery.
    _descriptor: fs::File,
}
// A full initial public clone exceeded the local-action 30s limit in native
// validation. Network setup remains bounded and never runs on the UI thread.
const NETWORK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AssociationKey {
    pub provider: String,
    pub host: String,
    pub account: String,
    pub repository: String,
    pub pull_request: u64,
}

impl AssociationKey {
    fn scope(&self) -> String {
        format!(
            "{}\n{}\n{}\n{}\n{}",
            self.provider, self.host, self.account, self.repository, self.pull_request
        )
    }

    fn validate(&self) -> Result<()> {
        for (label, value) in [
            ("provider", &self.provider),
            ("host", &self.host),
            ("account", &self.account),
            ("repository", &self.repository),
        ] {
            validate_text(value, label)?;
        }
        if self.pull_request == 0 {
            return Err(WorktreeError::InvalidInput(
                "pull request number must be greater than zero",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckoutOwnership {
    ExplicitlyAttached,
    AppManaged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemIdentity {
    pub device: u64,
    pub inode: u64,
}

impl FilesystemIdentity {
    fn read(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path).map_err(io("inspect filesystem identity"))?;
        if metadata.file_type().is_symlink() {
            return Err(WorktreeError::InvalidInput(
                "Git identity path must not be a symlink",
            ));
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreationContext {
    pub start_oid: String,
    pub local_branch: String,
    pub intended_remote_branch: Option<String>,
    pub published_head_at_creation: Option<String>,
    /// Existing user object repository or an application-owned no-checkout clone.
    pub object_repository: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutAssociation {
    pub key: AssociationKey,
    pub path: PathBuf,
    pub git_dir: PathBuf,
    pub common_git_dir: PathBuf,
    pub checkout_identity: FilesystemIdentity,
    pub git_dir_identity: FilesystemIdentity,
    pub common_git_dir_identity: FilesystemIdentity,
    pub ownership: CheckoutOwnership,
    pub creation: Option<CreationContext>,
    pub intended_remote_branch: Option<String>,
    pub published_head_at_association: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutView {
    pub association: CheckoutAssociation,
    /// Fresh installed-Git observation. It is never restored from creation state.
    pub actual_head: HeadState,
    pub operation: OperationState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachRequest {
    pub key: AssociationKey,
    pub checkout_path: PathBuf,
    /// Explicit repository binding discovered by the caller, then independently
    /// canonicalized and verified here.
    pub expected_common_git_dir: PathBuf,
    pub intended_remote_branch: Option<String>,
    pub published_head: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateFromLocalRequest {
    pub key: AssociationKey,
    pub object_repository: PathBuf,
    pub start_oid: String,
    pub local_branch: String,
    pub intended_remote_branch: Option<String>,
    pub published_head: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisionFromRemoteRequest {
    pub key: AssociationKey,
    pub repository_url: String,
    pub fetch_ref: String,
    pub exact_head_oid: String,
    pub local_branch: String,
    pub intended_remote_branch: Option<String>,
    pub published_head: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionOutcome {
    Created(CheckoutView),
    Reused(CheckoutView),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationAction {
    Create,
    Provision,
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UncertainOperation {
    pub operation_id: String,
    pub action: OperationAction,
    pub checkout_path: PathBuf,
    pub object_repository: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileOutcome {
    NoOperation,
    /// Preparation was durable but Git was never started. Reconciliation never
    /// promotes or retries it automatically.
    PreparedNotStarted(UncertainOperation),
    Completed(Box<CheckoutView>),
    Removed,
    Incomplete(UncertainOperation),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupBlocker {
    NotManaged,
    UnknownPublishedHead,
    IdentityChanged,
    DetachedHead,
    StagedChanges,
    UnstagedChanges,
    UntrackedFiles,
    IgnoredFiles,
    Conflicts,
    GitOperation,
    PublishedHeadMissing,
    CommitsNotContainedInPublishedHead,
    OperationPending,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupEligibility {
    Eligible,
    Ineligible(CleanupBlocker),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemovalOutcome {
    Removed,
}

#[derive(Debug)]
pub enum WorktreeError {
    InvalidInput(&'static str),
    Io {
        context: &'static str,
        source: std::io::Error,
    },
    LocalGit(LocalGitError),
    Store(String),
    AssociationConflict,
    AssociationNotFound,
    OperationPending(UncertainOperation),
    CleanupRefused(CleanupBlocker),
    /// Git was started or succeeded but the durable acknowledgement was not
    /// completed. The caller must reconcile before any explicit retry.
    Uncertain {
        operation: UncertainOperation,
        source: Option<Box<WorktreeError>>,
    },
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(reason) => write!(formatter, "invalid worktree input: {reason}"),
            Self::Io { context, .. } => formatter.write_str(context),
            Self::LocalGit(error) => write!(formatter, "{error}"),
            Self::Store(message) => formatter.write_str(message),
            Self::AssociationConflict => formatter.write_str(
                "a different checkout is already associated with this account and pull request",
            ),
            Self::AssociationNotFound => formatter.write_str("checkout association not found"),
            Self::OperationPending(_) => {
                formatter.write_str("a prior worktree operation requires reconciliation")
            }
            Self::CleanupRefused(blocker) => {
                write!(formatter, "managed worktree cleanup refused: {blocker:?}")
            }
            Self::Uncertain { operation, .. } => write!(
                formatter,
                "worktree {:?} operation {} has an uncertain outcome; reconcile before retrying",
                operation.action, operation.operation_id
            ),
        }
    }
}

impl std::error::Error for WorktreeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::LocalGit(source) => Some(source),
            Self::Uncertain {
                source: Some(source),
                ..
            } => Some(source),
            _ => None,
        }
    }
}

impl From<LocalGitError> for WorktreeError {
    fn from(value: LocalGitError) -> Self {
        Self::LocalGit(value)
    }
}

pub type Result<T> = std::result::Result<T, WorktreeError>;

/// Deterministic collision-resistant application proposal. Callers may instead
/// supply any explicit branch name that passes Git validation.
pub fn propose_local_branch(key: &AssociationKey) -> Result<String> {
    key.validate()?;
    let digest = storage_key(key);
    Ok(format!(
        "cibergit/pr-{}-{}",
        key.pull_request,
        &digest[..12]
    ))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum OperationPhase {
    Prepared,
    Started,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredOperation {
    id: String,
    key: AssociationKey,
    action: OperationAction,
    phase: OperationPhase,
    checkout_path: PathBuf,
    object_repository: Option<PathBuf>,
    expected_common_git_dir: Option<PathBuf>,
    #[serde(default)]
    managed_repository: Option<StoredManagedRepository>,
    #[serde(default)]
    repository_device: Option<u64>,
    #[serde(default)]
    repository_inode: Option<u64>,
    checkout_device: Option<u64>,
    checkout_inode: Option<u64>,
    start_oid: Option<String>,
    local_branch: Option<String>,
    intended_remote_branch: Option<String>,
    published_head: Option<String>,
}

impl StoredOperation {
    fn report(&self) -> UncertainOperation {
        UncertainOperation {
            operation_id: self.id.clone(),
            action: self.action.clone(),
            checkout_path: self.checkout_path.clone(),
            object_repository: self.object_repository.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StoredManagedRepository {
    provider: String,
    host: String,
    account: String,
    repository: String,
    origin_url: String,
    root: PathBuf,
    git_dir: PathBuf,
    common_git_dir: PathBuf,
    root_identity: FilesystemIdentity,
    git_dir_identity: FilesystemIdentity,
    common_git_dir_identity: FilesystemIdentity,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredState {
    schema_version: u32,
    associations: BTreeMap<String, CheckoutAssociation>,
    operations: BTreeMap<String, StoredOperation>,
    #[serde(default)]
    managed_repositories: BTreeMap<String, StoredManagedRepository>,
}

impl Default for StoredState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            associations: BTreeMap::new(),
            operations: BTreeMap::new(),
            managed_repositories: BTreeMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct WorktreeManager {
    managed_root: PathBuf,
    store_path: PathBuf,
    store_lock: Arc<Mutex<()>>,
}

static STORE_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

impl WorktreeManager {
    pub fn open(store_root: impl AsRef<Path>, managed_root: impl AsRef<Path>) -> Result<Self> {
        let store_root = prepare_private_root(store_root.as_ref(), "prepare worktree store")?;
        let managed_root =
            prepare_private_root(managed_root.as_ref(), "prepare managed worktree root")?;
        if store_root == managed_root
            || store_root.starts_with(&managed_root)
            || managed_root.starts_with(&store_root)
        {
            return Err(WorktreeError::InvalidInput(
                "store and managed roots must be separate",
            ));
        }
        if LocalGit::open(&managed_root).is_ok() {
            return Err(WorktreeError::InvalidInput(
                "managed root must be outside an existing Git checkout",
            ));
        }
        for child in ["worktrees", "repositories", "claims"] {
            let path = managed_root.join(child);
            fs::create_dir(&path)
                .or_else(|error| {
                    if error.kind() == ErrorKind::AlreadyExists {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
                .map_err(io("prepare managed worktree directory"))?;
            reject_symlink_or_non_directory(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(|source| {
                WorktreeError::Io {
                    context: "make managed worktree directory private",
                    source,
                }
            })?;
        }
        let store_path = store_root.join(STORE_FILE);
        if store_path
            .try_exists()
            .map_err(io("inspect worktree store"))?
        {
            reject_symlink_or_non_file(&store_path)?;
            let _ = load_state(&store_path)?;
        }
        let store_lock = {
            let registry = STORE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
            let mut registry = registry
                .lock()
                .map_err(|_| WorktreeError::Store("worktree store lock is unavailable".into()))?;
            registry
                .entry(store_path.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        Ok(Self {
            managed_root,
            store_path,
            store_lock,
        })
    }

    pub fn attach(&self, request: AttachRequest) -> Result<CheckoutView> {
        validate_request_context(
            &request.key,
            request.intended_remote_branch.as_deref(),
            request.published_head.as_deref(),
        )?;
        reject_symlink_or_non_directory(&request.checkout_path)?;
        let checkout = LocalGit::open(&request.checkout_path)?;
        let expected_common = canonical_non_symlink_directory(&request.expected_common_git_dir)?;
        if checkout.common_git_dir() != expected_common {
            return Err(WorktreeError::InvalidInput(
                "attached checkout does not match the explicit repository binding",
            ));
        }
        let _guard = self.lock_store()?;
        let mut state = load_state(&self.store_path)?;
        self.require_no_operation(&state, &request.key)?;
        let storage_key = storage_key(&request.key);
        if let Some(existing) = state.associations.get(&storage_key) {
            if existing.path != checkout.root()
                || existing.git_dir != checkout.git_dir()
                || existing.common_git_dir != checkout.common_git_dir()
                || existing.checkout_identity != FilesystemIdentity::read(checkout.root())?
                || existing.git_dir_identity != FilesystemIdentity::read(checkout.git_dir())?
                || existing.common_git_dir_identity
                    != FilesystemIdentity::read(checkout.common_git_dir())?
            {
                return Err(WorktreeError::AssociationConflict);
            }
            return revalidate_association(existing);
        }
        let association = CheckoutAssociation {
            key: request.key,
            path: checkout.root().to_path_buf(),
            git_dir: checkout.git_dir().to_path_buf(),
            common_git_dir: checkout.common_git_dir().to_path_buf(),
            checkout_identity: FilesystemIdentity::read(checkout.root())?,
            git_dir_identity: FilesystemIdentity::read(checkout.git_dir())?,
            common_git_dir_identity: FilesystemIdentity::read(checkout.common_git_dir())?,
            ownership: CheckoutOwnership::ExplicitlyAttached,
            creation: None,
            intended_remote_branch: request.intended_remote_branch,
            published_head_at_association: request.published_head,
        };
        let _ = revalidate_association(&association)?;
        state.associations.insert(storage_key, association.clone());
        save_state(&self.store_path, &state)?;
        revalidate_association(&association)
    }

    pub fn reopen(&self, key: &AssociationKey) -> Result<Option<CheckoutView>> {
        key.validate()?;
        let _guard = self.lock_store()?;
        let state = load_state(&self.store_path)?;
        let Some(association) = state.associations.get(&storage_key(key)) else {
            return Ok(None);
        };
        if association.key != *key {
            return Err(WorktreeError::Store(
                "stored association scope does not match its partition".into(),
            ));
        }
        revalidate_association(association).map(Some)
    }

    pub fn create_from_local(&self, request: CreateFromLocalRequest) -> Result<ProvisionOutcome> {
        self.create_from_local_inner(request, false)
    }

    pub fn provision_from_remote(
        &self,
        request: ProvisionFromRemoteRequest,
    ) -> Result<ProvisionOutcome> {
        validate_request_context(
            &request.key,
            request.intended_remote_branch.as_deref(),
            request.published_head.as_deref(),
        )?;
        validate_oid(&request.exact_head_oid)?;
        validate_branch_text(&request.local_branch)?;
        validate_fetch_ref(&request.fetch_ref)?;
        validate_repository_url(&request.repository_url)?;
        validate_ref_with_git(
            &self.managed_root,
            &request.local_branch,
            &request.fetch_ref,
        )?;
        let _guard = self.lock_store()?;
        let mut state = load_state(&self.store_path)?;
        if let Some(existing) = state.associations.get(&storage_key(&request.key)) {
            return revalidate_association(existing).map(ProvisionOutcome::Reused);
        }
        self.require_no_operation(&state, &request.key)?;

        let repository_path = self
            .managed_root
            .join("repositories")
            .join(format!("{}.repo", repository_storage_key(&request.key)));
        let checkout_path = self.checkout_path(&request.key);
        require_missing_path(&checkout_path)?;
        let (existing_repository, existing_provenance) = if path_exists_no_follow(&repository_path)?
        {
            reject_symlink_or_non_directory(&repository_path)?;
            let repository = LocalGit::open(&repository_path)?;
            let Some(provenance) = state_proves_managed_repository(
                &state,
                &request.key,
                &request.repository_url,
                &repository_path,
                &repository,
            )?
            else {
                return Err(WorktreeError::InvalidInput(
                    "existing repository path is not proven application-owned",
                ));
            };
            state
                .managed_repositories
                .insert(repository_storage_key(&request.key), provenance.clone());
            (Some(repository), Some(provenance))
        } else {
            (None, None)
        };
        let operation = StoredOperation {
            id: operation_id(&request.key, OperationAction::Provision),
            key: request.key.clone(),
            action: OperationAction::Provision,
            phase: OperationPhase::Prepared,
            checkout_path: checkout_path.clone(),
            object_repository: Some(repository_path.clone()),
            expected_common_git_dir: existing_repository
                .as_ref()
                .map(|repository| repository.common_git_dir().to_path_buf()),
            managed_repository: existing_provenance.clone(),
            repository_device: existing_provenance
                .as_ref()
                .map(|provenance| provenance.root_identity.device),
            repository_inode: existing_provenance
                .as_ref()
                .map(|provenance| provenance.root_identity.inode),
            checkout_device: None,
            checkout_inode: None,
            start_oid: Some(request.exact_head_oid.clone()),
            local_branch: Some(request.local_branch.clone()),
            intended_remote_branch: request.intended_remote_branch.clone(),
            published_head: request.published_head.clone(),
        };
        state
            .operations
            .insert(storage_key(&request.key), operation.clone());
        save_state(&self.store_path, &state)?;
        self.create_claim(&operation)?;
        let mut operation = operation;
        let (device, inode) = self.reserve_checkout_path(&operation.checkout_path)?;
        operation.checkout_device = Some(device);
        operation.checkout_inode = Some(inode);
        if existing_repository.is_none() {
            let identity = self.reserve_repository_path(&repository_path)?;
            operation.repository_device = Some(identity.device);
            operation.repository_inode = Some(identity.inode);
        }
        state
            .operations
            .insert(storage_key(&request.key), operation.clone());
        save_state(&self.store_path, &state)?;
        operation.phase = OperationPhase::Started;
        state
            .operations
            .insert(storage_key(&request.key), operation.clone());
        if let Err(error) = save_state(&self.store_path, &state) {
            return Err(uncertain(operation, error));
        }

        let repository = match existing_repository {
            Some(repository) => repository,
            None => {
                let clone_result = run_installed_git_at_with_limits(
                    &self.managed_root,
                    "create application-owned local repository",
                    vec![
                        "clone".into(),
                        "--no-checkout".into(),
                        "--origin".into(),
                        "origin".into(),
                        "--".into(),
                        request.repository_url.clone().into(),
                        repository_path.as_os_str().to_owned(),
                    ],
                    true,
                    &[0],
                    CommandLimits {
                        deadline: NETWORK_DEADLINE,
                        ..CommandLimits::default()
                    },
                );
                if let Err(error) = clone_result {
                    return Err(uncertain(operation, WorktreeError::LocalGit(error)));
                }
                match operation_repository_identity_matches(&operation) {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(uncertain(
                            operation,
                            WorktreeError::InvalidInput(
                                "managed repository reservation changed during clone",
                            ),
                        ));
                    }
                    Err(error) => return Err(uncertain(operation, error)),
                }
                match LocalGit::open(&repository_path) {
                    Ok(repository) => repository,
                    Err(error) => return Err(uncertain(operation, error.into())),
                }
            }
        };
        let provenance = if let Some(provenance) = operation.managed_repository.clone() {
            match managed_repository_matches(
                &provenance,
                &request.key,
                &request.repository_url,
                &repository_path,
                &repository,
            ) {
                Ok(true) => provenance,
                Ok(false) => {
                    return Err(uncertain(
                        operation,
                        WorktreeError::InvalidInput(
                            "application-owned repository identity changed before Git started",
                        ),
                    ));
                }
                Err(error) => return Err(uncertain(operation, error)),
            }
        } else {
            match capture_managed_repository(&request.key, &request.repository_url, &repository) {
                Ok(provenance) => provenance,
                Err(error) => return Err(uncertain(operation, error)),
            }
        };
        if !operation_repository_provenance_matches(&operation, &provenance) {
            return Err(uncertain(
                operation,
                WorktreeError::InvalidInput(
                    "managed repository reservation changed before provenance was recorded",
                ),
            ));
        }
        operation.expected_common_git_dir = Some(repository.common_git_dir().to_path_buf());
        operation.managed_repository = Some(provenance.clone());
        state
            .managed_repositories
            .insert(repository_storage_key(&request.key), provenance.clone());
        state
            .operations
            .insert(storage_key(&request.key), operation.clone());
        if let Err(error) = save_state(&self.store_path, &state) {
            return Err(uncertain(operation, error));
        }
        let git_guard = match repository.lock_common_git() {
            Ok(guard) => guard,
            Err(error) => return Err(uncertain(operation, error.into())),
        };
        match managed_repository_matches(
            &provenance,
            &request.key,
            &request.repository_url,
            &repository_path,
            &repository,
        ) {
            Ok(true) => {}
            Ok(false) => {
                return Err(uncertain(
                    operation,
                    WorktreeError::InvalidInput(
                        "application-owned repository identity changed before Git started",
                    ),
                ));
            }
            Err(error) => return Err(uncertain(operation, error)),
        }
        let synthetic_ref = format!("refs/cibergit/fetched/{}", storage_key(&request.key));
        let refspec = format!("+{}:{synthetic_ref}", request.fetch_ref);
        if let Err(error) = run_installed_git_at_with_limits(
            repository.root(),
            "fetch exact pull request object",
            vec![
                "fetch".into(),
                "--no-tags".into(),
                "--no-write-fetch-head".into(),
                "--no-auto-maintenance".into(),
                "--".into(),
                "origin".into(),
                refspec.into(),
            ],
            true,
            &[0],
            CommandLimits {
                deadline: NETWORK_DEADLINE,
                ..CommandLimits::default()
            },
        ) {
            return Err(uncertain(operation, error.into()));
        }
        match resolve_commit(&repository, &synthetic_ref) {
            Ok(actual) if actual == request.exact_head_oid => {}
            Ok(_) => {
                return Err(uncertain(
                    operation,
                    WorktreeError::InvalidInput(
                        "fetched pull request ref did not match the exact requested head",
                    ),
                ));
            }
            Err(error) => return Err(uncertain(operation, error)),
        }
        if let Err(error) = validate_creation_locked(
            &repository,
            &request.exact_head_oid,
            &request.local_branch,
            &checkout_path,
            Some((device, inode)),
        ) {
            return Err(uncertain(operation, error));
        }
        if let Err(error) = add_worktree_locked(
            &repository,
            &checkout_path,
            &request.local_branch,
            &request.exact_head_oid,
        ) {
            return Err(uncertain(operation, error));
        }
        drop(git_guard);
        self.finish_creation(
            &mut state,
            operation,
            request.key,
            repository_path,
            request.exact_head_oid,
            request.local_branch,
            request.intended_remote_branch,
            request.published_head,
        )
    }

    pub fn reconcile(&self, key: &AssociationKey) -> Result<ReconcileOutcome> {
        key.validate()?;
        let _guard = self.lock_store()?;
        let mut state = load_state(&self.store_path)?;
        let storage_key = storage_key(key);
        let Some(operation) = state.operations.get(&storage_key).cloned() else {
            return Ok(ReconcileOutcome::NoOperation);
        };
        if operation.key != *key {
            return Err(WorktreeError::Store(
                "stored operation scope does not match its partition".into(),
            ));
        }
        if operation.phase == OperationPhase::Prepared {
            return Ok(ReconcileOutcome::PreparedNotStarted(operation.report()));
        }
        match operation.action {
            OperationAction::Create | OperationAction::Provision => {
                let Ok(checkout) = LocalGit::open(&operation.checkout_path) else {
                    return Ok(ReconcileOutcome::Incomplete(operation.report()));
                };
                if !self.is_managed_checkout_path(checkout.root())
                    || operation
                        .expected_common_git_dir
                        .as_ref()
                        .is_some_and(|expected| expected != checkout.common_git_dir())
                    || !operation_checkout_identity_matches(&operation)?
                {
                    return Ok(ReconcileOutcome::Incomplete(operation.report()));
                }
                let Some(start_oid) = operation.start_oid.clone() else {
                    return Ok(ReconcileOutcome::Incomplete(operation.report()));
                };
                let Some(local_branch) = operation.local_branch.clone() else {
                    return Ok(ReconcileOutcome::Incomplete(operation.report()));
                };
                let snapshot = checkout.snapshot()?;
                if !matches!(snapshot.head, HeadState::Attached { ref branch, ref oid } if branch == &local_branch && oid == &start_oid)
                {
                    return Ok(ReconcileOutcome::Incomplete(operation.report()));
                }
                let Some(object_repository) = operation.object_repository.clone() else {
                    return Ok(ReconcileOutcome::Incomplete(operation.report()));
                };
                if !operation_repository_identity_matches(&operation)? {
                    return Ok(ReconcileOutcome::Incomplete(operation.report()));
                }
                let managed_provenance = if operation.action == OperationAction::Provision {
                    let Some(provenance) = operation.managed_repository.clone() else {
                        return Ok(ReconcileOutcome::Incomplete(operation.report()));
                    };
                    let Ok(repository) = LocalGit::open(&object_repository) else {
                        return Ok(ReconcileOutcome::Incomplete(operation.report()));
                    };
                    if !managed_repository_matches(
                        &provenance,
                        &operation.key,
                        &provenance.origin_url,
                        &object_repository,
                        &repository,
                    )? {
                        return Ok(ReconcileOutcome::Incomplete(operation.report()));
                    }
                    Some(provenance)
                } else {
                    None
                };
                if !worktree_record_contains(&object_repository, checkout.root())? {
                    return Ok(ReconcileOutcome::Incomplete(operation.report()));
                }
                let association = managed_association(
                    key.clone(),
                    &checkout,
                    CreationContext {
                        start_oid,
                        local_branch,
                        intended_remote_branch: operation.intended_remote_branch.clone(),
                        published_head_at_creation: operation.published_head.clone(),
                        object_repository,
                    },
                )?;
                let view = revalidate_association(&association)?;
                state
                    .associations
                    .insert(storage_key.clone(), association.clone());
                if let Some(provenance) = managed_provenance {
                    state
                        .managed_repositories
                        .insert(repository_storage_key(key), provenance);
                }
                state.operations.remove(&storage_key);
                save_state(&self.store_path, &state)?;
                self.remove_claim(&operation)?;
                Ok(ReconcileOutcome::Completed(Box::new(view)))
            }
            OperationAction::Remove => {
                if path_exists_no_follow(&operation.checkout_path)? {
                    return Ok(ReconcileOutcome::Incomplete(operation.report()));
                }
                if let Some(repository_path) = &operation.object_repository
                    && worktree_record_contains(repository_path, &operation.checkout_path)?
                {
                    return Ok(ReconcileOutcome::Incomplete(operation.report()));
                }
                state.associations.remove(&storage_key);
                state.operations.remove(&storage_key);
                save_state(&self.store_path, &state)?;
                self.remove_claim(&operation)?;
                Ok(ReconcileOutcome::Removed)
            }
        }
    }

    /// `exact_known_published_head` is an immutable head from authoritative
    /// provider state; callers must not substitute local HEAD or a tracking ref.
    pub fn cleanup_eligibility(
        &self,
        key: &AssociationKey,
        exact_known_published_head: Option<&str>,
    ) -> Result<CleanupEligibility> {
        key.validate()?;
        let _guard = self.lock_store()?;
        let state = load_state(&self.store_path)?;
        if state.operations.contains_key(&storage_key(key)) {
            return Ok(CleanupEligibility::Ineligible(
                CleanupBlocker::OperationPending,
            ));
        }
        let association = state
            .associations
            .get(&storage_key(key))
            .ok_or(WorktreeError::AssociationNotFound)?;
        cleanup_eligibility(association, exact_known_published_head)
    }

    /// The published head has the same authoritative-provider contract as
    /// [`Self::cleanup_eligibility`]. This action never infers publication from
    /// local Git state.
    pub fn remove_managed(
        &self,
        key: &AssociationKey,
        exact_known_published_head: Option<&str>,
    ) -> Result<RemovalOutcome> {
        key.validate()?;
        let _guard = self.lock_store()?;
        let mut state = load_state(&self.store_path)?;
        self.require_no_operation(&state, key)?;
        let storage_key = storage_key(key);
        let association = state
            .associations
            .get(&storage_key)
            .cloned()
            .ok_or(WorktreeError::AssociationNotFound)?;
        if let CleanupEligibility::Ineligible(blocker) =
            cleanup_eligibility(&association, exact_known_published_head)?
        {
            return Err(WorktreeError::CleanupRefused(blocker));
        }
        let creation = association
            .creation
            .as_ref()
            .ok_or(WorktreeError::CleanupRefused(CleanupBlocker::NotManaged))?;
        let repository = LocalGit::open(&creation.object_repository)?;
        if repository.common_git_dir() != association.common_git_dir {
            return Err(WorktreeError::CleanupRefused(
                CleanupBlocker::IdentityChanged,
            ));
        }
        let managed_provenance = if self.is_managed_repository_path(repository.root()) {
            let origin_url = read_origin_url(&repository)?;
            let Some(provenance) = state_proves_managed_repository(
                &state,
                key,
                &origin_url,
                &creation.object_repository,
                &repository,
            )?
            else {
                return Err(WorktreeError::CleanupRefused(
                    CleanupBlocker::IdentityChanged,
                ));
            };
            Some(provenance)
        } else {
            None
        };
        let git_guard = repository.lock_common_git()?;
        if let Some(provenance) = &managed_provenance
            && !managed_repository_matches(
                provenance,
                key,
                &provenance.origin_url,
                &creation.object_repository,
                &repository,
            )?
        {
            return Err(WorktreeError::CleanupRefused(
                CleanupBlocker::IdentityChanged,
            ));
        }
        if let CleanupEligibility::Ineligible(blocker) =
            cleanup_eligibility(&association, exact_known_published_head)?
        {
            return Err(WorktreeError::CleanupRefused(blocker));
        }
        let mut operation = StoredOperation {
            id: operation_id(key, OperationAction::Remove),
            key: key.clone(),
            action: OperationAction::Remove,
            phase: OperationPhase::Prepared,
            checkout_path: association.path.clone(),
            object_repository: Some(creation.object_repository.clone()),
            expected_common_git_dir: Some(association.common_git_dir.clone()),
            managed_repository: managed_provenance.clone(),
            repository_device: managed_provenance
                .as_ref()
                .map(|provenance| provenance.root_identity.device),
            repository_inode: managed_provenance
                .as_ref()
                .map(|provenance| provenance.root_identity.inode),
            checkout_device: Some(association.checkout_identity.device),
            checkout_inode: Some(association.checkout_identity.inode),
            start_oid: None,
            local_branch: None,
            intended_remote_branch: association.intended_remote_branch.clone(),
            published_head: exact_known_published_head.map(str::to_owned),
        };
        if let Some(provenance) = &managed_provenance {
            state
                .managed_repositories
                .insert(repository_storage_key(key), provenance.clone());
        }
        state
            .operations
            .insert(storage_key.clone(), operation.clone());
        save_state(&self.store_path, &state)?;
        operation.phase = OperationPhase::Started;
        state
            .operations
            .insert(storage_key.clone(), operation.clone());
        if let Err(error) = save_state(&self.store_path, &state) {
            return Err(uncertain(operation, error));
        }
        if let Some(provenance) = &managed_provenance {
            match managed_repository_matches(
                provenance,
                key,
                &provenance.origin_url,
                &creation.object_repository,
                &repository,
            ) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(uncertain(
                        operation,
                        WorktreeError::CleanupRefused(CleanupBlocker::IdentityChanged),
                    ));
                }
                Err(error) => return Err(uncertain(operation, error)),
            }
        }
        if let CleanupEligibility::Ineligible(blocker) =
            cleanup_eligibility(&association, exact_known_published_head)?
        {
            return Err(uncertain(operation, WorktreeError::CleanupRefused(blocker)));
        }
        if let Err(error) = repository.run_worktree_command(
            "remove managed worktree",
            vec![
                "worktree".into(),
                "remove".into(),
                "--".into(),
                association.path.as_os_str().to_owned(),
            ],
            true,
            &[0],
        ) {
            return Err(uncertain(operation, error.into()));
        }
        drop(git_guard);
        state.associations.remove(&storage_key);
        state.operations.remove(&storage_key);
        if let Err(error) = save_state(&self.store_path, &state) {
            return Err(uncertain(operation, error));
        }
        self.remove_claim(&operation)?;
        Ok(RemovalOutcome::Removed)
    }

    fn create_from_local_inner(
        &self,
        request: CreateFromLocalRequest,
        allow_managed_source: bool,
    ) -> Result<ProvisionOutcome> {
        validate_request_context(
            &request.key,
            request.intended_remote_branch.as_deref(),
            request.published_head.as_deref(),
        )?;
        validate_oid(&request.start_oid)?;
        validate_branch_text(&request.local_branch)?;
        reject_symlink_or_non_directory(&request.object_repository)?;
        let repository = LocalGit::open(&request.object_repository)?;
        if !allow_managed_source
            && (self.managed_root.starts_with(repository.root())
                || repository.root().starts_with(&self.managed_root))
        {
            return Err(WorktreeError::InvalidInput(
                "managed worktree root must be outside the source checkout",
            ));
        }
        let _guard = self.lock_store()?;
        let mut state = load_state(&self.store_path)?;
        if let Some(existing) = state.associations.get(&storage_key(&request.key)) {
            return revalidate_association(existing).map(ProvisionOutcome::Reused);
        }
        self.require_no_operation(&state, &request.key)?;
        let checkout_path = self.checkout_path(&request.key);
        require_missing_path(&checkout_path)?;
        let git_guard = repository.lock_common_git()?;
        validate_creation_locked(
            &repository,
            &request.start_oid,
            &request.local_branch,
            &checkout_path,
            None,
        )?;
        let repository_identity = FilesystemIdentity::read(repository.root())?;
        let mut operation = StoredOperation {
            id: operation_id(&request.key, OperationAction::Create),
            key: request.key.clone(),
            action: OperationAction::Create,
            phase: OperationPhase::Prepared,
            checkout_path: checkout_path.clone(),
            object_repository: Some(repository.root().to_path_buf()),
            expected_common_git_dir: Some(repository.common_git_dir().to_path_buf()),
            managed_repository: None,
            repository_device: Some(repository_identity.device),
            repository_inode: Some(repository_identity.inode),
            checkout_device: None,
            checkout_inode: None,
            start_oid: Some(request.start_oid.clone()),
            local_branch: Some(request.local_branch.clone()),
            intended_remote_branch: request.intended_remote_branch.clone(),
            published_head: request.published_head.clone(),
        };
        state
            .operations
            .insert(storage_key(&request.key), operation.clone());
        save_state(&self.store_path, &state)?;
        self.create_claim(&operation)?;
        let (device, inode) = self.reserve_checkout_path(&operation.checkout_path)?;
        operation.checkout_device = Some(device);
        operation.checkout_inode = Some(inode);
        state
            .operations
            .insert(storage_key(&request.key), operation.clone());
        save_state(&self.store_path, &state)?;
        operation.phase = OperationPhase::Started;
        state
            .operations
            .insert(storage_key(&request.key), operation.clone());
        if let Err(error) = save_state(&self.store_path, &state) {
            return Err(uncertain(operation, error));
        }
        if let Err(error) = validate_creation_locked(
            &repository,
            &request.start_oid,
            &request.local_branch,
            &checkout_path,
            Some((device, inode)),
        ) {
            return Err(uncertain(operation, error));
        }
        if let Err(error) = add_worktree_locked(
            &repository,
            &checkout_path,
            &request.local_branch,
            &request.start_oid,
        ) {
            return Err(uncertain(operation, error));
        }
        drop(git_guard);
        self.finish_creation(
            &mut state,
            operation,
            request.key,
            repository.root().to_path_buf(),
            request.start_oid,
            request.local_branch,
            request.intended_remote_branch,
            request.published_head,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_creation(
        &self,
        state: &mut StoredState,
        operation: StoredOperation,
        key: AssociationKey,
        object_repository: PathBuf,
        start_oid: String,
        local_branch: String,
        intended_remote_branch: Option<String>,
        published_head: Option<String>,
    ) -> Result<ProvisionOutcome> {
        let checkout = match LocalGit::open(&operation.checkout_path) {
            Ok(checkout) => checkout,
            Err(error) => return Err(uncertain(operation, error.into())),
        };
        if operation
            .expected_common_git_dir
            .as_ref()
            .is_some_and(|expected| expected != checkout.common_git_dir())
        {
            return Err(uncertain(
                operation,
                WorktreeError::InvalidInput("created worktree has an unexpected Git identity"),
            ));
        }
        let snapshot = match checkout.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => return Err(uncertain(operation, error.into())),
        };
        if !matches!(snapshot.head, HeadState::Attached { ref branch, ref oid } if branch == &local_branch && oid == &start_oid)
        {
            return Err(uncertain(
                operation,
                WorktreeError::InvalidInput("created worktree did not retain its requested start"),
            ));
        }
        match operation_repository_identity_matches(&operation) {
            Ok(true) => {}
            Ok(false) => {
                return Err(uncertain(
                    operation,
                    WorktreeError::InvalidInput(
                        "created worktree belongs to an unexpected object repository",
                    ),
                ));
            }
            Err(error) => return Err(uncertain(operation, error)),
        }
        match worktree_record_contains(&object_repository, checkout.root()) {
            Ok(true) => {}
            Ok(false) => {
                return Err(uncertain(
                    operation,
                    WorktreeError::InvalidInput(
                        "created checkout is missing from Git worktree records",
                    ),
                ));
            }
            Err(error) => return Err(uncertain(operation, error)),
        }
        let association = match managed_association(
            key.clone(),
            &checkout,
            CreationContext {
                start_oid,
                local_branch,
                intended_remote_branch,
                published_head_at_creation: published_head,
                object_repository,
            },
        ) {
            Ok(association) => association,
            Err(error) => return Err(uncertain(operation, error)),
        };
        let view = match revalidate_association(&association) {
            Ok(view) => view,
            Err(error) => return Err(uncertain(operation, error)),
        };
        let storage_key = storage_key(&key);
        state
            .associations
            .insert(storage_key.clone(), association.clone());
        state.operations.remove(&storage_key);
        if let Err(error) = save_state(&self.store_path, state) {
            return Err(uncertain(operation, error));
        }
        self.remove_claim(&operation)?;
        Ok(ProvisionOutcome::Created(view))
    }

    fn checkout_path(&self, key: &AssociationKey) -> PathBuf {
        self.managed_root.join("worktrees").join(storage_key(key))
    }

    fn is_managed_checkout_path(&self, path: &Path) -> bool {
        path.parent() == Some(self.managed_root.join("worktrees").as_path())
    }

    fn is_managed_repository_path(&self, path: &Path) -> bool {
        path.parent() == Some(self.managed_root.join("repositories").as_path())
    }

    fn reserve_repository_path(&self, path: &Path) -> Result<FilesystemIdentity> {
        fs::create_dir(path).map_err(io("reserve managed repository directory"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(io("make managed repository directory private"))?;
        sync_directory(path.parent().expect("repository path has parent"))?;
        let identity = FilesystemIdentity::read(path)?;
        if fs::read_dir(path)
            .map_err(io("inspect managed repository reservation"))?
            .next()
            .is_some()
        {
            return Err(WorktreeError::InvalidInput(
                "managed repository reservation is not empty",
            ));
        }
        Ok(identity)
    }

    fn reserve_checkout_path(&self, path: &Path) -> Result<(u64, u64)> {
        fs::create_dir(path).map_err(io("reserve managed checkout directory"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(io("make managed checkout directory private"))?;
        sync_directory(path.parent().expect("checkout path has parent"))?;
        let metadata =
            fs::symlink_metadata(path).map_err(io("inspect managed checkout reservation"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(WorktreeError::InvalidInput(
                "managed checkout reservation is not a directory",
            ));
        }
        Ok((metadata.dev(), metadata.ino()))
    }

    fn require_no_operation(&self, state: &StoredState, key: &AssociationKey) -> Result<()> {
        if let Some(operation) = state.operations.get(&storage_key(key)) {
            return Err(WorktreeError::OperationPending(operation.report()));
        }
        Ok(())
    }

    fn lock_store(&self) -> Result<StoreAuthorityGuard<'_>> {
        let started = Instant::now();
        let busy = || WorktreeError::Store("worktree store is busy in another operation".into());
        let process_guard = loop {
            match self.store_lock.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::Poisoned(_)) => {
                    return Err(WorktreeError::Store(
                        "worktree store lock is unavailable".into(),
                    ));
                }
                Err(TryLockError::WouldBlock) => {
                    if started.elapsed() >= STORE_LOCK_TIMEOUT {
                        return Err(busy());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        let path = self.store_path.with_file_name(STORE_LOCK_FILE);
        let descriptor = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(O_NONBLOCK | O_CLOEXEC | O_NOFOLLOW_ANY)
            .open(&path)
            .map_err(io("open worktree store authority"))?;
        let metadata = descriptor
            .metadata()
            .map_err(io("inspect worktree store authority"))?;
        // SAFETY: geteuid takes no pointers and reports this process's identity.
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { geteuid() }
            || metadata.mode() & 0o777 != 0o600
        {
            return Err(WorktreeError::Store(
                "worktree store authority must be a private single-link regular file".into(),
            ));
        }
        loop {
            // SAFETY: the open descriptor remains owned by the returned guard;
            // LOCK_NB makes contention subject to the userspace deadline.
            if unsafe { flock(descriptor.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
                let current =
                    fs::symlink_metadata(&path).map_err(io("recheck worktree store authority"))?;
                if current.file_type().is_symlink()
                    || current.dev() != metadata.dev()
                    || current.ino() != metadata.ino()
                    || current.nlink() != 1
                {
                    return Err(WorktreeError::Store(
                        "worktree store authority identity changed".into(),
                    ));
                }
                return Ok(StoreAuthorityGuard {
                    _process_guard: process_guard,
                    _descriptor: descriptor,
                });
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != ErrorKind::WouldBlock && error.kind() != ErrorKind::Interrupted {
                return Err(WorktreeError::Io {
                    context: "acquire worktree store authority",
                    source: error,
                });
            }
            if started.elapsed() >= STORE_LOCK_TIMEOUT {
                return Err(busy());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn claim_path(&self, operation: &StoredOperation) -> PathBuf {
        self.managed_root
            .join("claims")
            .join(format!("{}.json", operation.id))
    }

    fn create_claim(&self, operation: &StoredOperation) -> Result<()> {
        let path = self.claim_path(operation);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(io("reserve managed worktree path"))?;
        file.write_all(
            serde_json::to_string(&(&operation.id, &operation.key, &operation.checkout_path))
                .map_err(|error| WorktreeError::Store(error.to_string()))?
                .as_bytes(),
        )
        .map_err(io("write managed worktree claim"))?;
        file.sync_all().map_err(io("sync managed worktree claim"))?;
        sync_directory(path.parent().expect("claim has parent"))?;
        Ok(())
    }

    fn remove_claim(&self, operation: &StoredOperation) -> Result<()> {
        let path = self.claim_path(operation);
        match fs::remove_file(&path) {
            Ok(()) => sync_directory(path.parent().expect("claim has parent")),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(source) => Err(WorktreeError::Io {
                context: "remove managed worktree claim",
                source,
            }),
        }
    }
}

fn managed_association(
    key: AssociationKey,
    checkout: &LocalGit,
    creation: CreationContext,
) -> Result<CheckoutAssociation> {
    Ok(CheckoutAssociation {
        key,
        path: checkout.root().to_path_buf(),
        git_dir: checkout.git_dir().to_path_buf(),
        common_git_dir: checkout.common_git_dir().to_path_buf(),
        checkout_identity: FilesystemIdentity::read(checkout.root())?,
        git_dir_identity: FilesystemIdentity::read(checkout.git_dir())?,
        common_git_dir_identity: FilesystemIdentity::read(checkout.common_git_dir())?,
        ownership: CheckoutOwnership::AppManaged,
        intended_remote_branch: creation.intended_remote_branch.clone(),
        published_head_at_association: creation.published_head_at_creation.clone(),
        creation: Some(creation),
    })
}

fn revalidate_association(association: &CheckoutAssociation) -> Result<CheckoutView> {
    let checkout = open_revalidated_association(association)?;
    let snapshot = checkout.snapshot()?;
    validate_association_identity(association, &checkout)?;
    Ok(CheckoutView {
        association: association.clone(),
        actual_head: snapshot.head,
        operation: snapshot.operation,
    })
}

fn open_revalidated_association(association: &CheckoutAssociation) -> Result<LocalGit> {
    reject_symlink_or_non_directory(&association.path)?;
    let checkout = LocalGit::open(&association.path)?;
    validate_association_identity(association, &checkout)?;
    Ok(checkout)
}

fn validate_association_identity(
    association: &CheckoutAssociation,
    checkout: &LocalGit,
) -> Result<()> {
    if checkout.root() != association.path
        || checkout.git_dir() != association.git_dir
        || checkout.common_git_dir() != association.common_git_dir
        || FilesystemIdentity::read(checkout.root())? != association.checkout_identity
        || FilesystemIdentity::read(checkout.git_dir())? != association.git_dir_identity
        || FilesystemIdentity::read(checkout.common_git_dir())?
            != association.common_git_dir_identity
    {
        return Err(WorktreeError::Store(
            "saved checkout path now resolves to a different Git identity".into(),
        ));
    }
    Ok(())
}

fn cleanup_eligibility(
    association: &CheckoutAssociation,
    exact_known_published_head: Option<&str>,
) -> Result<CleanupEligibility> {
    if association.ownership != CheckoutOwnership::AppManaged || association.creation.is_none() {
        return Ok(CleanupEligibility::Ineligible(CleanupBlocker::NotManaged));
    }
    let Some(published_head) = exact_known_published_head else {
        return Ok(CleanupEligibility::Ineligible(
            CleanupBlocker::UnknownPublishedHead,
        ));
    };
    if validate_oid(published_head).is_err() {
        return Ok(CleanupEligibility::Ineligible(
            CleanupBlocker::UnknownPublishedHead,
        ));
    }
    let repository = match open_revalidated_association(association) {
        Ok(repository) => repository,
        Err(_) => {
            return Ok(CleanupEligibility::Ineligible(
                CleanupBlocker::IdentityChanged,
            ));
        }
    };
    let snapshot = repository.snapshot()?;
    if !snapshot.conflicts.is_empty() {
        return Ok(CleanupEligibility::Ineligible(CleanupBlocker::Conflicts));
    }
    if snapshot.operation.merge
        || snapshot.operation.rebase != RebaseState::None
        || snapshot.operation.cherry_pick
        || snapshot.operation.revert
    {
        return Ok(CleanupEligibility::Ineligible(CleanupBlocker::GitOperation));
    }
    if !snapshot.staged.is_empty() {
        return Ok(CleanupEligibility::Ineligible(
            CleanupBlocker::StagedChanges,
        ));
    }
    if !snapshot.unstaged.is_empty() {
        return Ok(CleanupEligibility::Ineligible(
            CleanupBlocker::UnstagedChanges,
        ));
    }
    if !snapshot.untracked.is_empty() {
        return Ok(CleanupEligibility::Ineligible(
            CleanupBlocker::UntrackedFiles,
        ));
    }
    let ignored = repository.run_worktree_command(
        "inspect ignored files",
        vec![
            "ls-files".into(),
            "-z".into(),
            "--others".into(),
            "--ignored".into(),
            "--exclude-standard".into(),
        ],
        false,
        &[0],
    )?;
    if !ignored.is_empty() {
        return Ok(CleanupEligibility::Ineligible(CleanupBlocker::IgnoredFiles));
    }
    let current_oid = match snapshot.head {
        HeadState::Attached { oid, .. } => oid,
        HeadState::Detached { .. } | HeadState::Unborn { .. } => {
            return Ok(CleanupEligibility::Ineligible(CleanupBlocker::DetachedHead));
        }
    };
    if resolve_commit(&repository, published_head).is_err() {
        return Ok(CleanupEligibility::Ineligible(
            CleanupBlocker::PublishedHeadMissing,
        ));
    }
    let result = repository.run_worktree_command(
        "prove local commits are published",
        vec![
            "merge-base".into(),
            "--is-ancestor".into(),
            current_oid.clone().into(),
            published_head.into(),
        ],
        false,
        &[0, 1],
    )?;
    // merge-base --is-ancestor has no output; ask rev-list for a stable
    // behavioral answer rather than inferring its accepted exit code.
    let unpublished = repository.run_worktree_command(
        "inspect unpublished commits",
        vec![
            "rev-list".into(),
            "--max-count=1".into(),
            format!("{published_head}..{current_oid}").into(),
        ],
        false,
        &[0],
    )?;
    let _ = result;
    if !unpublished.is_empty() {
        return Ok(CleanupEligibility::Ineligible(
            CleanupBlocker::CommitsNotContainedInPublishedHead,
        ));
    }
    if validate_association_identity(association, &repository).is_err() {
        return Ok(CleanupEligibility::Ineligible(
            CleanupBlocker::IdentityChanged,
        ));
    }
    Ok(CleanupEligibility::Eligible)
}

fn validate_creation_locked(
    repository: &LocalGit,
    start_oid: &str,
    local_branch: &str,
    checkout_path: &Path,
    reserved_identity: Option<(u64, u64)>,
) -> Result<()> {
    validate_oid(start_oid)?;
    validate_branch_with_git(repository, local_branch)?;
    if let Some((device, inode)) = reserved_identity {
        let metadata = fs::symlink_metadata(checkout_path)
            .map_err(io("revalidate managed checkout reservation"))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.dev() != device
            || metadata.ino() != inode
            || fs::read_dir(checkout_path)
                .map_err(io("inspect managed checkout reservation"))?
                .next()
                .is_some()
        {
            return Err(WorktreeError::InvalidInput(
                "managed checkout reservation changed before Git started",
            ));
        }
    } else {
        require_missing_path(checkout_path)?;
    }
    let object_type = repository.run_worktree_command(
        "validate exact worktree start",
        vec!["cat-file".into(), "-t".into(), start_oid.into()],
        false,
        &[0],
    )?;
    if strip_lf(&object_type) != b"commit" {
        return Err(WorktreeError::InvalidInput(
            "worktree start object is not a commit",
        ));
    }
    let branch_ref = format!("refs/heads/{local_branch}");
    let branch = repository.run_worktree_command(
        "check local branch availability",
        vec![
            "show-ref".into(),
            "--verify".into(),
            "--quiet".into(),
            branch_ref.into(),
        ],
        false,
        &[0, 1],
    );
    match branch {
        Ok(_) => {
            // show-ref produces no output, so explicitly resolve to distinguish
            // exit 0 from the accepted absent exit 1.
            if resolve_optional_ref(repository, &format!("refs/heads/{local_branch}"))?.is_some() {
                return Err(WorktreeError::InvalidInput(
                    "requested local branch already exists",
                ));
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn add_worktree_locked(
    repository: &LocalGit,
    checkout_path: &Path,
    local_branch: &str,
    start_oid: &str,
) -> Result<()> {
    repository
        .run_worktree_command(
            "create persistent worktree",
            vec![
                "worktree".into(),
                "add".into(),
                "--no-track".into(),
                "-b".into(),
                local_branch.into(),
                "--".into(),
                checkout_path.as_os_str().to_owned(),
                start_oid.into(),
            ],
            true,
            &[0],
        )
        .map(|_| ())
        .map_err(Into::into)
}

fn validate_branch_with_git(repository: &LocalGit, branch: &str) -> Result<()> {
    validate_branch_text(branch)?;
    repository
        .run_worktree_command(
            "validate worktree branch name",
            vec!["check-ref-format".into(), "--branch".into(), branch.into()],
            false,
            &[0],
        )
        .map(|_| ())
        .map_err(Into::into)
}

fn resolve_commit(repository: &LocalGit, revision: &str) -> Result<String> {
    let output = repository.run_worktree_command(
        "resolve exact commit",
        vec![
            "rev-parse".into(),
            "--verify".into(),
            "--end-of-options".into(),
            format!("{revision}^{{commit}}").into(),
        ],
        false,
        &[0],
    )?;
    let oid = String::from_utf8(strip_lf(&output).to_vec())
        .map_err(|_| WorktreeError::Store("Git returned a non-UTF-8 object ID".into()))?;
    validate_oid(&oid)?;
    Ok(oid)
}

fn resolve_optional_ref(repository: &LocalGit, reference: &str) -> Result<Option<String>> {
    let output = repository.run_worktree_command(
        "resolve optional local branch",
        vec![
            "for-each-ref".into(),
            "--format=%(objectname)".into(),
            "--count=1".into(),
            reference.into(),
        ],
        false,
        &[0],
    )?;
    let value = strip_lf(&output);
    if value.is_empty() {
        Ok(None)
    } else {
        let oid = String::from_utf8(value.to_vec())
            .map_err(|_| WorktreeError::Store("Git returned a non-UTF-8 object ID".into()))?;
        validate_oid(&oid)?;
        Ok(Some(oid))
    }
}

fn read_origin_url(repository: &LocalGit) -> Result<String> {
    let output = repository.run_worktree_command(
        "verify application-owned repository origin",
        vec!["config".into(), "--get".into(), "remote.origin.url".into()],
        false,
        &[0],
    )?;
    String::from_utf8(strip_lf(&output).to_vec())
        .map_err(|_| WorktreeError::Store("Git returned a non-UTF-8 origin URL".into()))
}

fn state_proves_managed_repository(
    state: &StoredState,
    key: &AssociationKey,
    origin_url: &str,
    repository_path: &Path,
    repository: &LocalGit,
) -> Result<Option<StoredManagedRepository>> {
    let partition = repository_storage_key(key);
    if let Some(provenance) = state.managed_repositories.get(&partition) {
        return if managed_repository_matches(
            provenance,
            key,
            origin_url,
            repository_path,
            repository,
        )? {
            Ok(Some(provenance.clone()))
        } else {
            Ok(None)
        };
    }

    for operation in state.operations.values().filter(|operation| {
        operation.action == OperationAction::Provision
            && operation.object_repository.as_deref() == Some(repository_path)
    }) {
        let Some(provenance) = operation.managed_repository.as_ref() else {
            continue;
        };
        return if managed_repository_matches(
            provenance,
            key,
            origin_url,
            repository_path,
            repository,
        )? {
            Ok(Some(provenance.clone()))
        } else {
            Ok(None)
        };
    }

    // Compatibility for state written before repository provenance became a
    // first-class record. The durable managed association and its saved common
    // directory identity are the proof; the managed path alone is not.
    let common_identity = FilesystemIdentity::read(repository.common_git_dir())?;
    let association_proves = state.associations.values().any(|association| {
        association.ownership == CheckoutOwnership::AppManaged
            && same_repository_scope(&association.key, key)
            && association
                .creation
                .as_ref()
                .is_some_and(|creation| creation.object_repository == repository_path)
            && association.common_git_dir == repository.common_git_dir()
            && association.common_git_dir_identity == common_identity
    });
    if association_proves {
        capture_managed_repository(key, origin_url, repository).map(Some)
    } else {
        Ok(None)
    }
}

fn capture_managed_repository(
    key: &AssociationKey,
    origin_url: &str,
    repository: &LocalGit,
) -> Result<StoredManagedRepository> {
    if read_origin_url(repository)? != origin_url {
        return Err(WorktreeError::InvalidInput(
            "application-owned repository has a different origin URL",
        ));
    }
    Ok(StoredManagedRepository {
        provider: key.provider.clone(),
        host: key.host.clone(),
        account: key.account.clone(),
        repository: key.repository.clone(),
        origin_url: origin_url.to_owned(),
        root: repository.root().to_path_buf(),
        git_dir: repository.git_dir().to_path_buf(),
        common_git_dir: repository.common_git_dir().to_path_buf(),
        root_identity: FilesystemIdentity::read(repository.root())?,
        git_dir_identity: FilesystemIdentity::read(repository.git_dir())?,
        common_git_dir_identity: FilesystemIdentity::read(repository.common_git_dir())?,
    })
}

fn managed_repository_matches(
    provenance: &StoredManagedRepository,
    key: &AssociationKey,
    origin_url: &str,
    repository_path: &Path,
    repository: &LocalGit,
) -> Result<bool> {
    Ok(provenance.provider == key.provider
        && provenance.host == key.host
        && provenance.account == key.account
        && provenance.repository == key.repository
        && provenance.origin_url == origin_url
        && provenance.root == repository_path
        && provenance.root == repository.root()
        && provenance.git_dir == repository.git_dir()
        && provenance.common_git_dir == repository.common_git_dir()
        && provenance.root_identity == FilesystemIdentity::read(repository.root())?
        && provenance.git_dir_identity == FilesystemIdentity::read(repository.git_dir())?
        && provenance.common_git_dir_identity
            == FilesystemIdentity::read(repository.common_git_dir())?
        && read_origin_url(repository)? == origin_url)
}

fn same_repository_scope(left: &AssociationKey, right: &AssociationKey) -> bool {
    left.provider == right.provider
        && left.host == right.host
        && left.account == right.account
        && left.repository == right.repository
}

fn worktree_record_contains(repository_path: &Path, checkout_path: &Path) -> Result<bool> {
    let repository = match LocalGit::open(repository_path) {
        Ok(repository) => repository,
        Err(_) => return Ok(false),
    };
    let output = repository.run_worktree_command(
        "inspect Git worktree records",
        vec![
            "worktree".into(),
            "list".into(),
            "--porcelain".into(),
            "-z".into(),
        ],
        false,
        &[0],
    )?;
    let expected = checkout_path.as_os_str().as_encoded_bytes();
    Ok(output
        .split(|byte| *byte == 0)
        .filter_map(|record| record.strip_prefix(b"worktree "))
        .any(|path| path == expected))
}

fn operation_checkout_identity_matches(operation: &StoredOperation) -> Result<bool> {
    let (Some(expected_device), Some(expected_inode)) =
        (operation.checkout_device, operation.checkout_inode)
    else {
        return Ok(false);
    };
    let metadata = fs::symlink_metadata(&operation.checkout_path)
        .map_err(io("revalidate managed checkout ownership"))?;
    Ok(!metadata.file_type().is_symlink()
        && metadata.is_dir()
        && metadata.dev() == expected_device
        && metadata.ino() == expected_inode)
}

fn operation_repository_identity_matches(operation: &StoredOperation) -> Result<bool> {
    let (Some(repository_path), Some(expected_device), Some(expected_inode)) = (
        operation.object_repository.as_ref(),
        operation.repository_device,
        operation.repository_inode,
    ) else {
        return Ok(false);
    };
    let metadata = fs::symlink_metadata(repository_path)
        .map_err(io("revalidate managed repository ownership"))?;
    Ok(!metadata.file_type().is_symlink()
        && metadata.is_dir()
        && metadata.dev() == expected_device
        && metadata.ino() == expected_inode)
}

fn operation_repository_provenance_matches(
    operation: &StoredOperation,
    provenance: &StoredManagedRepository,
) -> bool {
    operation.object_repository.as_ref() == Some(&provenance.root)
        && operation.repository_device == Some(provenance.root_identity.device)
        && operation.repository_inode == Some(provenance.root_identity.inode)
}

fn validate_request_context(
    key: &AssociationKey,
    intended_remote_branch: Option<&str>,
    published_head: Option<&str>,
) -> Result<()> {
    key.validate()?;
    if let Some(branch) = intended_remote_branch {
        validate_branch_text(branch)?;
    }
    if let Some(oid) = published_head {
        validate_oid(oid)?;
    }
    Ok(())
}

fn validate_text(value: &str, _label: &'static str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_TEXT_BYTES
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(WorktreeError::InvalidInput(
            "identity fields must be bounded, non-empty text",
        ));
    }
    Ok(())
}

fn validate_oid(oid: &str) -> Result<()> {
    if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(WorktreeError::InvalidInput(
            "object ID must be a full hexadecimal Git object ID",
        ));
    }
    Ok(())
}

fn validate_branch_text(branch: &str) -> Result<()> {
    if branch.is_empty()
        || branch.len() > 1_024
        || branch.starts_with('-')
        || branch.contains("@{")
        || branch
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(WorktreeError::InvalidInput("invalid local branch name"));
    }
    Ok(())
}

fn validate_fetch_ref(reference: &str) -> Result<()> {
    if !reference.starts_with("refs/")
        || reference.len() > 2_048
        || reference.starts_with('-')
        || reference
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(WorktreeError::InvalidInput(
            "fetch ref must be an explicit bounded refs/ name",
        ));
    }
    Ok(())
}

fn validate_ref_with_git(current_dir: &Path, branch: &str, fetch_ref: &str) -> Result<()> {
    run_installed_git_at(
        current_dir,
        "validate proposed local branch",
        vec!["check-ref-format".into(), "--branch".into(), branch.into()],
        false,
        &[0],
    )?;
    run_installed_git_at(
        current_dir,
        "validate requested fetch ref",
        vec!["check-ref-format".into(), fetch_ref.into()],
        false,
        &[0],
    )?;
    Ok(())
}

fn validate_repository_url(url: &str) -> Result<()> {
    if url.is_empty()
        || url.len() > MAX_TEXT_BYTES
        || url.bytes().any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(WorktreeError::InvalidInput("invalid repository URL"));
    }
    if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "https" | "ssh" | "file") {
            return Err(WorktreeError::InvalidInput(
                "unsupported repository URL scheme",
            ));
        }
        let authority = rest.split('/').next().unwrap_or_default();
        if let Some((user, _)) = authority.split_once('@')
            && (scheme != "ssh" || user != "git")
        {
            return Err(WorktreeError::InvalidInput(
                "repository URLs must not contain credentials",
            ));
        }
    } else if let Some((user, _)) = url.split_once('@')
        && user != "git"
    {
        return Err(WorktreeError::InvalidInput(
            "repository URLs must not contain credentials",
        ));
    }
    Ok(())
}

fn storage_key(key: &AssociationKey) -> String {
    format!("{:x}", Sha256::digest(key.scope()))
}

fn repository_storage_key(key: &AssociationKey) -> String {
    repository_storage_key_parts(&key.provider, &key.host, &key.account, &key.repository)
}

fn repository_storage_key_parts(
    provider: &str,
    host: &str,
    account: &str,
    repository: &str,
) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!(
            "{}\n{}\n{}\n{}",
            provider, host, account, repository
        ))
    )
}

fn operation_id(key: &AssociationKey, action: OperationAction) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:x}",
        Sha256::digest(format!(
            "{}\n{action:?}\n{}\n{stamp}",
            key.scope(),
            std::process::id()
        ))
    )
}

fn load_state(path: &Path) -> Result<StoredState> {
    if !path.try_exists().map_err(io("inspect worktree store"))? {
        return Ok(StoredState::default());
    }
    reject_symlink_or_non_file(path)?;
    let metadata = fs::metadata(path).map_err(io("inspect worktree store"))?;
    if metadata.len() > MAX_STORE_BYTES {
        return Err(WorktreeError::Store(
            "worktree association store exceeds its size limit".into(),
        ));
    }
    let bytes = fs::read(path).map_err(io("read worktree store"))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| WorktreeError::Store(format!("unreadable worktree store: {error}")))?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| WorktreeError::Store("worktree store is missing schema_version".into()))?;
    if version != u64::from(SCHEMA_VERSION) {
        return Err(WorktreeError::Store(
            "worktree store was written by an unsupported application version".into(),
        ));
    }
    let state: StoredState = serde_json::from_value(value)
        .map_err(|error| WorktreeError::Store(format!("invalid worktree store: {error}")))?;
    validate_state(&state)?;
    Ok(state)
}

fn validate_state(state: &StoredState) -> Result<()> {
    if state.schema_version != SCHEMA_VERSION
        || state.associations.len() > MAX_RECORDS
        || state.operations.len() > MAX_RECORDS
        || state.managed_repositories.len() > MAX_RECORDS
    {
        return Err(WorktreeError::Store(
            "worktree store violates schema bounds".into(),
        ));
    }
    for (partition, association) in &state.associations {
        association.key.validate()?;
        if partition != &storage_key(&association.key) {
            return Err(WorktreeError::Store(
                "worktree association is stored in the wrong partition".into(),
            ));
        }
        validate_stored_absolute_path(&association.path)?;
        validate_stored_absolute_path(&association.git_dir)?;
        validate_stored_absolute_path(&association.common_git_dir)?;
        if let Some(creation) = &association.creation {
            validate_oid(&creation.start_oid)?;
            validate_branch_text(&creation.local_branch)?;
            validate_stored_absolute_path(&creation.object_repository)?;
        }
        if (association.ownership == CheckoutOwnership::AppManaged)
            != association.creation.is_some()
        {
            return Err(WorktreeError::Store(
                "worktree ownership and creation context disagree".into(),
            ));
        }
        if let Some(branch) = &association.intended_remote_branch {
            validate_branch_text(branch)?;
        }
        if let Some(oid) = &association.published_head_at_association {
            validate_oid(oid)?;
        }
    }
    for (partition, operation) in &state.operations {
        operation.key.validate()?;
        if partition != &storage_key(&operation.key) {
            return Err(WorktreeError::Store(
                "worktree operation is stored in the wrong partition".into(),
            ));
        }
        validate_stored_absolute_path(&operation.checkout_path)?;
        if let Some(path) = &operation.object_repository {
            validate_stored_absolute_path(path)?;
        }
        if let Some(provenance) = &operation.managed_repository {
            validate_stored_managed_repository(provenance)?;
            if !same_repository_scope_fields(provenance, &operation.key)
                || operation.object_repository.as_ref() != Some(&provenance.root)
                || operation.expected_common_git_dir.as_ref() != Some(&provenance.common_git_dir)
                || !operation_repository_provenance_matches(operation, provenance)
                || !matches!(
                    operation.action,
                    OperationAction::Provision | OperationAction::Remove
                )
            {
                return Err(WorktreeError::Store(
                    "worktree operation has inconsistent repository provenance".into(),
                ));
            }
        }
        if operation.repository_device.is_some() != operation.repository_inode.is_some() {
            return Err(WorktreeError::Store(
                "worktree operation has an incomplete repository identity".into(),
            ));
        }
        if operation.checkout_device.is_some() != operation.checkout_inode.is_some() {
            return Err(WorktreeError::Store(
                "worktree operation has an incomplete path identity".into(),
            ));
        }
        if let Some(oid) = &operation.start_oid {
            validate_oid(oid)?;
        }
        if let Some(branch) = &operation.local_branch {
            validate_branch_text(branch)?;
        }
        if let Some(branch) = &operation.intended_remote_branch {
            validate_branch_text(branch)?;
        }
        if let Some(oid) = &operation.published_head {
            validate_oid(oid)?;
        }
    }
    for (partition, provenance) in &state.managed_repositories {
        validate_stored_managed_repository(provenance)?;
        if partition
            != &repository_storage_key_parts(
                &provenance.provider,
                &provenance.host,
                &provenance.account,
                &provenance.repository,
            )
        {
            return Err(WorktreeError::Store(
                "managed repository is stored in the wrong partition".into(),
            ));
        }
    }
    Ok(())
}

fn validate_stored_managed_repository(provenance: &StoredManagedRepository) -> Result<()> {
    for value in [
        &provenance.provider,
        &provenance.host,
        &provenance.account,
        &provenance.repository,
    ] {
        validate_text(value, "managed repository identity")?;
    }
    validate_repository_url(&provenance.origin_url)?;
    validate_stored_absolute_path(&provenance.root)?;
    validate_stored_absolute_path(&provenance.git_dir)?;
    validate_stored_absolute_path(&provenance.common_git_dir)
}

fn same_repository_scope_fields(
    provenance: &StoredManagedRepository,
    key: &AssociationKey,
) -> bool {
    provenance.provider == key.provider
        && provenance.host == key.host
        && provenance.account == key.account
        && provenance.repository == key.repository
}

fn save_state(path: &Path, state: &StoredState) -> Result<()> {
    validate_state(state)?;
    if path.try_exists().map_err(io("inspect worktree store"))? {
        let _ = load_state(path)?;
    }
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| WorktreeError::Store(error.to_string()))?;
    if bytes.len() as u64 > MAX_STORE_BYTES {
        return Err(WorktreeError::Store(
            "worktree association store exceeds its size limit".into(),
        ));
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| WorktreeError::Store(error.to_string()))?
        .as_nanos();
    let temp = path.with_extension(format!("{}.{}.tmp", std::process::id(), stamp));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(io("create temporary worktree store"))?;
        file.write_all(&bytes)
            .map_err(io("write temporary worktree store"))?;
        file.sync_all()
            .map_err(io("sync temporary worktree store"))?;
        fs::rename(&temp, path).map_err(io("replace worktree store"))?;
        sync_directory(path.parent().expect("store path has parent"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn prepare_private_root(path: &Path, context: &'static str) -> Result<PathBuf> {
    validate_lexical_absolute(path)?;
    if path_exists_no_follow(path)? {
        reject_symlink_or_non_directory(path)?;
    } else {
        let parent = path.parent().ok_or(WorktreeError::InvalidInput(
            "private root must have an existing parent",
        ))?;
        reject_symlink_or_non_directory(parent)?;
        fs::create_dir(path).map_err(|source| WorktreeError::Io { context, source })?;
        reject_symlink_or_non_directory(path)?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| WorktreeError::Io { context, source })?;
    fs::canonicalize(path).map_err(|source| WorktreeError::Io { context, source })
}

fn canonical_non_symlink_directory(path: &Path) -> Result<PathBuf> {
    reject_symlink_or_non_directory(path)?;
    fs::canonicalize(path).map_err(io("canonicalize repository binding"))
}

fn reject_symlink_or_non_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(io("inspect path"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorktreeError::InvalidInput(
            "path must be an existing non-symlink directory",
        ));
    }
    Ok(())
}

fn reject_symlink_or_non_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(io("inspect state file"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(WorktreeError::Store(
            "worktree state path is not a regular non-symlink file".into(),
        ));
    }
    Ok(())
}

fn validate_lexical_absolute(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(WorktreeError::InvalidInput(
            "paths must be absolute without traversal",
        ));
    }
    Ok(())
}

fn validate_stored_absolute_path(path: &Path) -> Result<()> {
    validate_lexical_absolute(path)
        .map_err(|error| WorktreeError::Store(format!("unsafe stored path: {error}")))
}

fn require_missing_path(path: &Path) -> Result<()> {
    validate_lexical_absolute(path)?;
    if path_exists_no_follow(path)? {
        return Err(WorktreeError::InvalidInput(
            "managed checkout path already exists",
        ));
    }
    Ok(())
}

fn path_exists_no_follow(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(source) => Err(WorktreeError::Io {
            context: "inspect path without following symlinks",
            source,
        }),
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io("sync state directory"))
}

fn strip_lf(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}

fn io(context: &'static str) -> impl FnOnce(std::io::Error) -> WorktreeError {
    move |source| WorktreeError::Io { context, source }
}

fn uncertain(operation: StoredOperation, error: WorktreeError) -> WorktreeError {
    WorktreeError::Uncertain {
        operation: operation.report(),
        source: Some(Box::new(error)),
    }
}
