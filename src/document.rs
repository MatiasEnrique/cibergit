//! Bounded, recovery-backed editing for existing UTF-8 worktree files.
//!
//! Filesystem notifications are only hints. Call [`Document::refresh`] on a
//! notification or focus change, and trust the state returned by this module.
//!
//! Save exchanges the checked pathname with a synced candidate atomically on
//! Darwin and retains the displaced inode. That closes the silent-unlink case
//! for a writer which already holds the old file descriptor, but it is not a
//! transaction with arbitrary external tools: they may still rename or write
//! either pathname after any observation. Every uncertain boundary is returned
//! explicitly and is never repaired with a destructive rollback.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        raw::{c_char, c_int, c_uint},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

const RECOVERY_SCHEMA: u32 = 1;
const MAX_CONFIGURED_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_BYTES: usize = 8 * 1024 * 1024;
const PRIVATE_DIR_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

// Darwin values from <sys/fcntl.h> and <sys/stdio.h>. cibergit V1 targets
// macOS on Apple Silicon only; refusing an unavailable primitive is safer than
// silently falling back to a replacing rename.
const O_RDONLY: c_int = 0;
const O_WRONLY: c_int = 1;
const O_NONBLOCK: c_int = 0x0000_0004;
const O_CREAT: c_int = 0x0000_0200;
const O_EXCL: c_int = 0x0000_0800;
const O_RESOLVE_BENEATH: c_int = 0x0000_1000;
const O_UNIQUE: c_int = 0x0000_2000;
const O_DIRECTORY: c_int = 0x0010_0000;
const O_CLOEXEC: c_int = 0x0100_0000;
const O_NOFOLLOW_ANY: c_int = 0x2000_0000;
const RENAME_SWAP: c_uint = 0x0000_0002;
const RENAME_EXCL: c_uint = 0x0000_0004;
const RENAME_NOFOLLOW_ANY: c_uint = 0x0000_0010;
const RENAME_RESOLVE_BENEATH: c_uint = 0x0000_0020;

unsafe extern "C" {
    fn openat(fd: c_int, path: *const c_char, oflag: c_int, ...) -> c_int;
    fn renameatx_np(
        from_fd: c_int,
        from: *const c_char,
        to_fd: c_int,
        to: *const c_char,
        flags: c_uint,
    ) -> c_int;
    fn renamex_np(from: *const c_char, to: *const c_char, flags: c_uint) -> c_int;
}

static UNIQUE_NAME: AtomicU64 = AtomicU64::new(1);

/// Hard limits for one editor document and its unsaved buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentLimits {
    pub max_file_bytes: usize,
    pub max_buffer_bytes: usize,
}

impl Default for DocumentLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_BYTES,
            max_buffer_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// Stable, non-secret identifiers used to partition private recovery data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryScope {
    pub account_id: String,
    pub repository_id: String,
}

impl RecoveryScope {
    pub fn new(account_id: impl Into<String>, repository_id: impl Into<String>) -> Self {
        Self {
            account_id: account_id.into(),
            repository_id: repository_id.into(),
        }
    }
}

/// A content fingerprint plus the identity of the file that supplied it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiskVersion {
    pub sha256: String,
    pub len: u64,
    pub device: u64,
    pub inode: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
    pub mode: u32,
}

impl DiskVersion {
    pub fn is_same_generation(&self, other: &Self) -> bool {
        self.sha256 == other.sha256
            && self.len == other.len
            && self.device == other.device
            && self.inode == other.inode
            && self.mode == other.mode
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiskSnapshot {
    pub text: String,
    pub version: DiskVersion,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TargetIssue {
    SymlinkOrEscape,
    NonRegular,
    MultipleHardLinks,
    InvalidUtf8,
    TooLarge { bytes: u64, limit: usize },
    PermissionDenied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiskState {
    Present(DiskSnapshot),
    Missing,
    Unsafe(TargetIssue),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictKind {
    ExternalEdit,
    Missing,
    Unsafe,
    SaveRace,
}

/// All versions needed by a reconciliation UI. For a save race, `external`
/// is the entry atomically displaced at the save boundary and `current` is a
/// fresh observation of the path after that boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictState {
    pub kind: ConflictKind,
    pub base: DiskSnapshot,
    pub buffer: String,
    pub external: DiskState,
    pub current: DiskState,
    pub retained_external: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryStatus {
    None,
    Restored { path: PathBuf },
    Corrupt { path: PathBuf, reason: String },
    Stale { path: PathBuf, reason: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentStatus {
    Clean,
    Dirty,
    Conflict,
    Missing,
    Unsafe,
    RecoveryCorrupt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshOutcome {
    Unchanged,
    Reloaded,
    Conflict,
    Missing,
    Unsafe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconcileOutcome {
    Applied,
    /// Disk advanced after the caller's comparison was displayed. The merged
    /// text was preserved as the dirty buffer, but was not adopted against the
    /// newer disk baseline.
    Stale,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SaveOutcome {
    Unchanged,
    Saved {
        version: DiskVersion,
        /// The previous live inode is retained so a writer that already held
        /// it cannot finish into an unlinked file unnoticed.
        retained_previous: PathBuf,
    },
    Blocked {
        status: DocumentStatus,
    },
    /// The swap completed, but the displaced version was not the checked
    /// baseline. The competing bytes are retained at `retained_external`.
    ConflictRetained {
        retained_external: PathBuf,
        target_contains_buffer: bool,
    },
    /// The swap completed and this module deliberately did not roll it back,
    /// because a rollback could overwrite a newer external change.
    CommittedButUncertain {
        retained_path: Option<PathBuf>,
        reason: String,
    },
}

#[derive(Debug)]
pub enum DocumentError {
    InvalidLimits,
    InvalidRelativePath,
    InvalidWorktreeRoot(String),
    InvalidRecoveryStore(String),
    MissingTarget,
    UnsafeTarget(TargetIssue),
    BufferTooLarge {
        bytes: usize,
        limit: usize,
    },
    UnstableRead,
    CorruptRecovery {
        path: PathBuf,
        reason: String,
    },
    Io {
        action: &'static str,
        source: io::Error,
    },
    Serialization(String),
    AtomicExchange(io::Error),
}

impl fmt::Display for DocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => write!(f, "document limits must be between 1 byte and 64 MiB"),
            Self::InvalidRelativePath => write!(
                f,
                "document path must be a non-empty relative path without parent traversal"
            ),
            Self::InvalidWorktreeRoot(message) => write!(f, "invalid worktree root: {message}"),
            Self::InvalidRecoveryStore(message) => write!(f, "invalid recovery store: {message}"),
            Self::MissingTarget => write!(f, "document target is missing"),
            Self::UnsafeTarget(issue) => write!(f, "unsafe document target: {issue:?}"),
            Self::BufferTooLarge { bytes, limit } => {
                write!(f, "editor buffer is {bytes} bytes; limit is {limit}")
            }
            Self::UnstableRead => {
                write!(f, "document changed while it was being read; retry refresh")
            }
            Self::CorruptRecovery { path, reason } => write!(
                f,
                "recovery data at {} is corrupt and was preserved: {reason}",
                path.display()
            ),
            Self::Io { action, source } => write!(f, "{action}: {source}"),
            Self::Serialization(message) => write!(f, "recovery serialization failed: {message}"),
            Self::AtomicExchange(source) => write!(
                f,
                "safe atomic save exchange is unavailable or failed: {source}"
            ),
        }
    }
}

impl std::error::Error for DocumentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::AtomicExchange(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct DocumentStore {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    root: PathBuf,
    root_fd: File,
    recovery: RecoveryStore,
    limits: DocumentLimits,
}

#[derive(Clone)]
struct RecoveryStore {
    root: PathBuf,
    scope_hash: String,
    limits: DocumentLimits,
}

pub struct Document {
    store: Arc<StoreInner>,
    relative_path: PathBuf,
    recovery_path: PathBuf,
    base: DiskSnapshot,
    buffer: String,
    disk: DiskState,
    conflict: Option<ConflictState>,
    recovery: RecoveryStatus,
}

impl DocumentStore {
    pub fn new(
        worktree_root: impl AsRef<Path>,
        recovery_root: impl AsRef<Path>,
        scope: RecoveryScope,
        limits: DocumentLimits,
    ) -> Result<Self, DocumentError> {
        if limits.max_file_bytes == 0
            || limits.max_buffer_bytes == 0
            || limits.max_file_bytes > MAX_CONFIGURED_BYTES
            || limits.max_buffer_bytes > MAX_CONFIGURED_BYTES
        {
            return Err(DocumentError::InvalidLimits);
        }
        if scope.account_id.is_empty() || scope.repository_id.is_empty() {
            return Err(DocumentError::InvalidRecoveryStore(
                "account and repository identifiers must be non-empty".into(),
            ));
        }

        let root = fs::canonicalize(worktree_root.as_ref())
            .map_err(|error| DocumentError::InvalidWorktreeRoot(error.to_string()))?;
        let metadata = fs::metadata(&root)
            .map_err(|error| DocumentError::InvalidWorktreeRoot(error.to_string()))?;
        if !metadata.is_dir() {
            return Err(DocumentError::InvalidWorktreeRoot("not a directory".into()));
        }
        let root_fd = OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_CLOEXEC)
            .open(&root)
            .map_err(|error| DocumentError::InvalidWorktreeRoot(error.to_string()))?;

        let recovery = RecoveryStore::new(recovery_root.as_ref(), scope, limits)?;
        if recovery.root.starts_with(&root) {
            return Err(DocumentError::InvalidRecoveryStore(
                "private recovery must be outside the supplied worktree".into(),
            ));
        }
        Ok(Self {
            inner: Arc::new(StoreInner {
                root,
                root_fd,
                recovery,
                limits,
            }),
        })
    }

    pub fn open(&self, relative_path: impl AsRef<Path>) -> Result<Document, DocumentError> {
        let relative_path = validate_relative_path(relative_path.as_ref())?;
        let recovery_path = self.inner.recovery.path_for(&relative_path);
        let loaded = self.inner.recovery.load(&relative_path)?;
        let disk = self.inner.read_disk(&relative_path)?;

        match loaded {
            RecoveryLoad::Valid { payload, path } => {
                let conflict = conflict_for(&payload.base, &payload.buffer, &disk, None, None);
                Ok(Document {
                    store: self.inner.clone(),
                    relative_path,
                    recovery_path,
                    base: payload.base,
                    buffer: payload.buffer,
                    disk,
                    conflict,
                    recovery: RecoveryStatus::Restored { path },
                })
            }
            RecoveryLoad::Missing => {
                let DiskState::Present(snapshot) = disk else {
                    return Err(issue_for_initial_disk(disk));
                };
                Ok(Document {
                    store: self.inner.clone(),
                    relative_path,
                    recovery_path,
                    base: snapshot.clone(),
                    buffer: snapshot.text.clone(),
                    disk: DiskState::Present(snapshot),
                    conflict: None,
                    recovery: RecoveryStatus::None,
                })
            }
            RecoveryLoad::Corrupt { path, reason } => {
                let DiskState::Present(snapshot) = disk else {
                    return Err(DocumentError::CorruptRecovery { path, reason });
                };
                Ok(Document {
                    store: self.inner.clone(),
                    relative_path,
                    recovery_path,
                    base: snapshot.clone(),
                    buffer: snapshot.text.clone(),
                    disk: DiskState::Present(snapshot),
                    conflict: None,
                    recovery: RecoveryStatus::Corrupt { path, reason },
                })
            }
        }
    }
}

impl Document {
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }
    pub fn absolute_path(&self) -> PathBuf {
        self.store.root.join(&self.relative_path)
    }
    pub fn recovery_path(&self) -> &Path {
        &self.recovery_path
    }
    pub fn buffer(&self) -> &str {
        &self.buffer
    }
    pub fn base(&self) -> &DiskSnapshot {
        &self.base
    }
    pub fn disk(&self) -> &DiskState {
        &self.disk
    }
    pub fn conflict(&self) -> Option<&ConflictState> {
        self.conflict.as_ref()
    }
    pub fn recovery_status(&self) -> &RecoveryStatus {
        &self.recovery
    }
    pub fn is_dirty(&self) -> bool {
        self.buffer != self.base.text
    }

    pub fn status(&self) -> DocumentStatus {
        if matches!(
            self.recovery,
            RecoveryStatus::Corrupt { .. } | RecoveryStatus::Stale { .. }
        ) {
            return DocumentStatus::RecoveryCorrupt;
        }
        if let Some(conflict) = &self.conflict {
            return match conflict.kind {
                ConflictKind::Missing => DocumentStatus::Missing,
                ConflictKind::Unsafe => DocumentStatus::Unsafe,
                ConflictKind::ExternalEdit | ConflictKind::SaveRace => DocumentStatus::Conflict,
            };
        }
        if self.is_dirty() {
            DocumentStatus::Dirty
        } else {
            DocumentStatus::Clean
        }
    }

    /// Replaces the UTF-8 editor buffer and durably records unsaved text before
    /// publishing the new in-memory value.
    pub fn set_buffer(&mut self, text: impl Into<String>) -> Result<(), DocumentError> {
        self.require_writable_recovery()?;
        let text = text.into();
        if text.len() > self.store.limits.max_buffer_bytes {
            return Err(DocumentError::BufferTooLarge {
                bytes: text.len(),
                limit: self.store.limits.max_buffer_bytes,
            });
        }
        if text == self.base.text {
            self.store.recovery.clear(&self.relative_path)?;
            self.recovery = RecoveryStatus::None;
        } else {
            self.store
                .recovery
                .write(&self.relative_path, &self.base, &text)?;
            self.recovery = RecoveryStatus::Restored {
                path: self.recovery_path.clone(),
            };
        }
        self.buffer = text;
        if let Some(conflict) = &mut self.conflict {
            conflict.buffer = self.buffer.clone();
        }
        Ok(())
    }

    /// Re-reads authoritative disk state. A clean buffer reloads; a dirty
    /// buffer retains its base and text and gains a three-way conflict.
    pub fn refresh(&mut self) -> Result<RefreshOutcome, DocumentError> {
        let disk = self.store.read_disk(&self.relative_path)?;
        let was_same = disk_matches_base(&disk, &self.base);
        self.disk = disk.clone();

        if self.is_dirty() {
            if was_same {
                self.conflict = None;
                return Ok(RefreshOutcome::Unchanged);
            }
            self.conflict = conflict_for(&self.base, &self.buffer, &disk, None, None);
            return Ok(outcome_for_disk(&disk, RefreshOutcome::Conflict));
        }

        match disk {
            DiskState::Present(snapshot) => {
                if self.base.version.is_same_generation(&snapshot.version) {
                    self.conflict = None;
                    Ok(RefreshOutcome::Unchanged)
                } else {
                    self.base = snapshot.clone();
                    self.buffer = snapshot.text.clone();
                    self.disk = DiskState::Present(snapshot);
                    self.conflict = None;
                    Ok(RefreshOutcome::Reloaded)
                }
            }
            missing_or_unsafe => {
                self.conflict =
                    conflict_for(&self.base, &self.buffer, &missing_or_unsafe, None, None);
                self.disk = missing_or_unsafe.clone();
                Ok(outcome_for_disk(
                    &missing_or_unsafe,
                    RefreshOutcome::Conflict,
                ))
            }
        }
    }

    /// Explicitly discards the buffer in favor of a currently safe disk file.
    pub fn reload_from_disk(&mut self) -> Result<(), DocumentError> {
        self.require_writable_recovery()?;
        let disk = self.store.read_disk(&self.relative_path)?;
        let DiskState::Present(snapshot) = disk else {
            return Err(issue_for_initial_disk(disk));
        };
        self.store.recovery.clear(&self.relative_path)?;
        self.base = snapshot.clone();
        self.buffer = snapshot.text.clone();
        self.disk = DiskState::Present(snapshot);
        self.conflict = None;
        self.recovery = RecoveryStatus::None;
        Ok(())
    }

    /// Applies text merged against the exact disk generation the caller
    /// displayed. If disk advanced again, the proposed merge is durably kept
    /// as the dirty buffer but the newer disk version is not silently adopted.
    pub fn reconcile(
        &mut self,
        expected_disk: &DiskVersion,
        merged_text: impl Into<String>,
    ) -> Result<ReconcileOutcome, DocumentError> {
        self.require_writable_recovery()?;
        let merged_text = merged_text.into();
        if merged_text.len() > self.store.limits.max_buffer_bytes {
            return Err(DocumentError::BufferTooLarge {
                bytes: merged_text.len(),
                limit: self.store.limits.max_buffer_bytes,
            });
        }
        let disk = self.store.read_disk(&self.relative_path)?;
        let displayed_matches = matches!(
            &self.disk,
            DiskState::Present(displayed)
                if displayed.version.is_same_generation(expected_disk)
        );
        let latest_matches = matches!(
            &disk,
            DiskState::Present(latest)
                if latest.version.is_same_generation(expected_disk)
        );
        if !displayed_matches || !latest_matches {
            self.store
                .recovery
                .write(&self.relative_path, &self.base, &merged_text)?;
            self.buffer = merged_text;
            self.disk = disk.clone();
            self.conflict = conflict_for(&self.base, &self.buffer, &disk, None, None);
            self.recovery = RecoveryStatus::Restored {
                path: self.recovery_path.clone(),
            };
            return Ok(ReconcileOutcome::Stale);
        }
        let DiskState::Present(snapshot) = disk else {
            unreachable!("latest_matches requires a present disk snapshot")
        };
        if merged_text == snapshot.text {
            self.store.recovery.clear(&self.relative_path)?;
            self.recovery = RecoveryStatus::None;
        } else {
            self.store
                .recovery
                .write(&self.relative_path, &snapshot, &merged_text)?;
            self.recovery = RecoveryStatus::Restored {
                path: self.recovery_path.clone(),
            };
        }
        self.base = snapshot.clone();
        self.buffer = merged_text;
        self.disk = DiskState::Present(snapshot);
        self.conflict = None;
        Ok(ReconcileOutcome::Applied)
    }

    pub fn save(&mut self) -> Result<SaveOutcome, DocumentError> {
        self.save_with_hook(|_| Ok(()))
    }

    /// The hook runs after the replacement has been written and synced but
    /// immediately before the atomic exchange. It exists for deterministic
    /// boundary tests and should not be used for routine application work.
    #[doc(hidden)]
    pub fn save_with_hook<F>(&mut self, hook: F) -> Result<SaveOutcome, DocumentError>
    where
        F: FnOnce(&Path) -> io::Result<()>,
    {
        self.require_writable_recovery()?;
        let _ = self.refresh()?;
        if !self.is_dirty() {
            return Ok(SaveOutcome::Unchanged);
        }
        if self.conflict.is_some() {
            return Ok(SaveOutcome::Blocked {
                status: self.status(),
            });
        }
        self.store.validate_writable(&self.relative_path)?;

        let expected = self.base.clone();
        let (temp_relative, mut temp_file) = self
            .store
            .create_save_temp(&self.relative_path, expected.version.mode)?;
        if let Err(error) = temp_file
            .write_all(self.buffer.as_bytes())
            .and_then(|_| temp_file.sync_all())
        {
            self.store.remove_temp(&temp_relative);
            return Err(DocumentError::Io {
                action: "write and sync save candidate",
                source: error,
            });
        }
        drop(temp_file);

        if let Err(error) = hook(&self.absolute_path()) {
            self.store.remove_temp(&temp_relative);
            return Err(DocumentError::Io {
                action: "run save-boundary hook",
                source: error,
            });
        }

        if let Err(error) = self.store.exchange(&temp_relative, &self.relative_path) {
            self.store.remove_temp(&temp_relative);
            let _ = self.refresh();
            if self.conflict.is_some() {
                return Ok(SaveOutcome::Blocked {
                    status: self.status(),
                });
            }
            return Err(DocumentError::AtomicExchange(error));
        }
        if let Err(error) = self.store.sync_target_parent(&self.relative_path) {
            return Ok(SaveOutcome::CommittedButUncertain {
                retained_path: Some(self.store.root.join(&temp_relative)),
                reason: format!(
                    "atomic exchange completed but its directory did not sync: {error}"
                ),
            });
        }

        self.finish_exchanged_save(temp_relative, expected)
    }

    fn finish_exchanged_save(
        &mut self,
        displaced_relative: PathBuf,
        expected: DiskSnapshot,
    ) -> Result<SaveOutcome, DocumentError> {
        let displaced_absolute = self.store.root.join(&displaced_relative);
        let displaced = match self.store.read_raw(&displaced_relative) {
            Ok(value) => value,
            Err(error) => {
                return Ok(SaveOutcome::CommittedButUncertain {
                    retained_path: Some(displaced_absolute),
                    reason: format!("could not verify displaced file: {error}"),
                });
            }
        };

        let RawDiskState::Present { bytes, version } = displaced else {
            return Ok(SaveOutcome::CommittedButUncertain {
                retained_path: Some(displaced_absolute),
                reason: "atomic exchange displaced a missing or unsafe entry".into(),
            });
        };

        if !expected.version.is_same_generation(&version) {
            let retained = self
                .store
                .recovery
                .retain_displaced_inode(&self.relative_path, &displaced_absolute, "conflict")
                .unwrap_or_else(|_| displaced_absolute.clone());
            let external = raw_to_disk(bytes, version, self.store.limits.max_file_bytes);
            let current = self
                .store
                .read_disk(&self.relative_path)
                .unwrap_or(DiskState::Unsafe(TargetIssue::NonRegular));
            let target_contains_buffer =
                matches!(&current, DiskState::Present(snapshot) if snapshot.text == self.buffer);
            self.disk = current.clone();
            self.conflict = Some(ConflictState {
                kind: ConflictKind::SaveRace,
                base: self.base.clone(),
                buffer: self.buffer.clone(),
                external,
                current,
                retained_external: Some(retained.clone()),
            });
            return Ok(SaveOutcome::ConflictRetained {
                retained_external: retained,
                target_contains_buffer,
            });
        }

        let current = match self.store.read_disk(&self.relative_path) {
            Ok(current) => current,
            Err(error) => {
                return Ok(SaveOutcome::CommittedButUncertain {
                    retained_path: Some(displaced_absolute),
                    reason: format!("could not verify saved path: {error}"),
                });
            }
        };

        let DiskState::Present(saved) = &current else {
            self.disk = current.clone();
            self.conflict = conflict_for(&self.base, &self.buffer, &current, None, None);
            return Ok(SaveOutcome::CommittedButUncertain {
                retained_path: Some(displaced_absolute),
                reason: "saved path changed again before post-save verification".into(),
            });
        };

        if saved.text != self.buffer {
            let retained = self
                .store
                .recovery
                .retain_displaced_inode(&self.relative_path, &displaced_absolute, "previous")
                .unwrap_or(displaced_absolute);
            self.disk = current.clone();
            self.conflict = conflict_for(&self.base, &self.buffer, &current, None, None);
            return Ok(SaveOutcome::CommittedButUncertain {
                retained_path: Some(retained),
                reason: "the path changed again after the atomic exchange; the editor buffer remains in recovery".into(),
            });
        }

        let retained_previous = self
            .store
            .recovery
            .retain_displaced_inode(&self.relative_path, &displaced_absolute, "previous")
            .unwrap_or(displaced_absolute);
        if let Err(error) = self.store.recovery.clear(&self.relative_path) {
            self.base = saved.clone();
            self.disk = current;
            self.conflict = None;
            self.recovery = RecoveryStatus::Stale {
                path: self.recovery_path.clone(),
                reason: error.to_string(),
            };
            return Ok(SaveOutcome::CommittedButUncertain {
                retained_path: Some(retained_previous),
                reason: error.to_string(),
            });
        }
        let saved = saved.clone();
        self.base = saved.clone();
        self.disk = current;
        self.conflict = None;
        self.recovery = RecoveryStatus::None;
        Ok(SaveOutcome::Saved {
            version: saved.version,
            retained_previous,
        })
    }

    fn require_writable_recovery(&self) -> Result<(), DocumentError> {
        match &self.recovery {
            RecoveryStatus::Corrupt { path, reason } | RecoveryStatus::Stale { path, reason } => {
                Err(DocumentError::CorruptRecovery {
                    path: path.clone(),
                    reason: reason.clone(),
                })
            }
            RecoveryStatus::None | RecoveryStatus::Restored { .. } => Ok(()),
        }
    }
}

impl StoreInner {
    fn open_relative(&self, relative: &Path, flags: c_int, mode: Option<u32>) -> io::Result<File> {
        let path = path_cstring(relative)?;
        let fd = unsafe {
            match mode {
                Some(mode) => openat(self.root_fd.as_raw_fd(), path.as_ptr(), flags, mode),
                None => openat(self.root_fd.as_raw_fd(), path.as_ptr(), flags),
            }
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn read_raw(&self, relative: &Path) -> Result<RawDiskState, DocumentError> {
        let flags =
            O_RDONLY | O_NONBLOCK | O_CLOEXEC | O_NOFOLLOW_ANY | O_RESOLVE_BENEATH | O_UNIQUE;
        let mut file = match self.open_relative(relative, flags, None) {
            Ok(file) => file,
            Err(error) => return classify_open_error(error),
        };
        let before = file.metadata().map_err(|source| DocumentError::Io {
            action: "inspect document",
            source,
        })?;
        if !before.file_type().is_file() {
            return Ok(RawDiskState::Unsafe(TargetIssue::NonRegular));
        }
        if before.nlink() != 1 {
            return Ok(RawDiskState::Unsafe(TargetIssue::MultipleHardLinks));
        }
        if before.len() > self.limits.max_file_bytes as u64 {
            return Ok(RawDiskState::Unsafe(TargetIssue::TooLarge {
                bytes: before.len(),
                limit: self.limits.max_file_bytes,
            }));
        }
        let mut bytes = Vec::with_capacity(before.len() as usize);
        Read::by_ref(&mut file)
            .take(self.limits.max_file_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| DocumentError::Io {
                action: "read document",
                source,
            })?;
        if bytes.len() > self.limits.max_file_bytes {
            return Ok(RawDiskState::Unsafe(TargetIssue::TooLarge {
                bytes: bytes.len() as u64,
                limit: self.limits.max_file_bytes,
            }));
        }
        let after = file.metadata().map_err(|source| DocumentError::Io {
            action: "reinspect document",
            source,
        })?;
        if metadata_changed_during_read(&before, &after) {
            return Err(DocumentError::UnstableRead);
        }
        Ok(RawDiskState::Present {
            version: disk_version(&after, &bytes),
            bytes,
        })
    }

    fn read_disk(&self, relative: &Path) -> Result<DiskState, DocumentError> {
        Ok(match self.read_raw(relative)? {
            RawDiskState::Present { bytes, version } => {
                raw_to_disk(bytes, version, self.limits.max_file_bytes)
            }
            RawDiskState::Missing => DiskState::Missing,
            RawDiskState::Unsafe(issue) => DiskState::Unsafe(issue),
        })
    }

    fn validate_writable(&self, relative: &Path) -> Result<(), DocumentError> {
        let flags = O_WRONLY | O_CLOEXEC | O_NOFOLLOW_ANY | O_RESOLVE_BENEATH | O_UNIQUE;
        match self.open_relative(relative, flags, None) {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                Err(DocumentError::UnsafeTarget(TargetIssue::PermissionDenied))
            }
            Err(error) if matches!(error.raw_os_error(), Some(2)) => {
                Err(DocumentError::UnsafeTarget(TargetIssue::NonRegular))
            }
            Err(error) if matches!(error.raw_os_error(), Some(62) | Some(107)) => {
                Err(DocumentError::UnsafeTarget(TargetIssue::SymlinkOrEscape))
            }
            Err(source) => Err(DocumentError::Io {
                action: "verify document is writable",
                source,
            }),
        }
    }

    fn create_save_temp(
        &self,
        relative: &Path,
        mode: u32,
    ) -> Result<(PathBuf, File), DocumentError> {
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        for _ in 0..32 {
            let leaf = format!(
                ".cibergit-save-{}-{}",
                std::process::id(),
                UNIQUE_NAME.fetch_add(1, Ordering::Relaxed)
            );
            let candidate = if parent.as_os_str().is_empty() {
                PathBuf::from(leaf)
            } else {
                parent.join(leaf)
            };
            let flags =
                O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW_ANY | O_RESOLVE_BENEATH;
            match self.open_relative(&candidate, flags, Some(PRIVATE_FILE_MODE)) {
                Ok(file) => {
                    file.set_permissions(fs::Permissions::from_mode(mode & 0o7777))
                        .map_err(|source| DocumentError::Io {
                            action: "copy document permissions",
                            source,
                        })?;
                    return Ok((candidate, file));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(DocumentError::Io {
                        action: "create save candidate",
                        source,
                    });
                }
            }
        }
        Err(DocumentError::Io {
            action: "create unique save candidate",
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "temporary-name attempts exhausted",
            ),
        })
    }

    fn exchange(&self, from: &Path, to: &Path) -> io::Result<()> {
        let from = path_cstring(from)?;
        let to = path_cstring(to)?;
        let result = unsafe {
            renameatx_np(
                self.root_fd.as_raw_fd(),
                from.as_ptr(),
                self.root_fd.as_raw_fd(),
                to.as_ptr(),
                RENAME_SWAP | RENAME_NOFOLLOW_ANY | RENAME_RESOLVE_BENEATH,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn remove_temp(&self, relative: &Path) {
        // Best effort only. After a completed exchange, a failed cleanup leaves
        // the displaced original recoverable rather than risking a rollback.
        let _ = fs::remove_file(self.root.join(relative));
    }

    fn sync_target_parent(&self, relative: &Path) -> Result<(), DocumentError> {
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let path = if parent.as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.root.join(parent)
        };
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| DocumentError::Io {
                action: "sync saved document directory",
                source,
            })
    }
}

impl RecoveryStore {
    fn new(
        root: &Path,
        scope: RecoveryScope,
        limits: DocumentLimits,
    ) -> Result<Self, DocumentError> {
        ensure_private_dir(root)?;
        let root = fs::canonicalize(root).map_err(|source| DocumentError::Io {
            action: "canonicalize recovery directory",
            source,
        })?;
        let scope_hash =
            hash_fields(&[scope.account_id.as_bytes(), scope.repository_id.as_bytes()]);
        let version_root = root.join(format!("v{RECOVERY_SCHEMA}")).join(&scope_hash);
        ensure_private_dir(&version_root)?;
        Ok(Self {
            root: version_root,
            scope_hash,
            limits,
        })
    }

    fn path_for(&self, relative: &Path) -> PathBuf {
        self.root.join(format!(
            "{}.json",
            hash_fields(&[relative.as_os_str().as_bytes()])
        ))
    }

    fn load(&self, relative: &Path) -> Result<RecoveryLoad, DocumentError> {
        let path = self.path_for(relative);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(RecoveryLoad::Missing);
            }
            Err(source) => {
                return Err(DocumentError::Io {
                    action: "read document recovery",
                    source,
                });
            }
        };
        let corrupt = |reason: String| RecoveryLoad::Corrupt {
            path: path.clone(),
            reason,
        };
        if bytes.len()
            > self
                .limits
                .max_file_bytes
                .saturating_add(self.limits.max_buffer_bytes)
                .saturating_mul(2)
                .saturating_add(64 * 1024)
        {
            return Ok(corrupt("record exceeds bounded recovery size".into()));
        }
        let record: RecoveryRecord = match serde_json::from_slice(&bytes) {
            Ok(record) => record,
            Err(error) => return Ok(corrupt(format!("invalid JSON: {error}"))),
        };
        let encoded = serde_json::to_vec(&record.payload)
            .map_err(|error| DocumentError::Serialization(error.to_string()))?;
        if record.checksum != hex_digest(&encoded) {
            return Ok(corrupt("checksum mismatch".into()));
        }
        if record.payload.schema != RECOVERY_SCHEMA {
            return Ok(corrupt("unsupported schema".into()));
        }
        if record.payload.scope_hash != self.scope_hash {
            return Ok(corrupt("scope mismatch".into()));
        }
        if record.payload.path_hex != hex_bytes(relative.as_os_str().as_bytes()) {
            return Ok(corrupt("path mismatch".into()));
        }
        if record.payload.base.text.len() > self.limits.max_file_bytes
            || record.payload.buffer.len() > self.limits.max_buffer_bytes
        {
            return Ok(corrupt("record text exceeds configured limit".into()));
        }
        Ok(RecoveryLoad::Valid {
            payload: record.payload,
            path,
        })
    }

    fn write(
        &self,
        relative: &Path,
        base: &DiskSnapshot,
        buffer: &str,
    ) -> Result<(), DocumentError> {
        if let RecoveryLoad::Corrupt { path, reason } = self.load(relative)? {
            return Err(DocumentError::CorruptRecovery { path, reason });
        }
        let payload = RecoveryPayload {
            schema: RECOVERY_SCHEMA,
            scope_hash: self.scope_hash.clone(),
            path_hex: hex_bytes(relative.as_os_str().as_bytes()),
            base: base.clone(),
            buffer: buffer.to_owned(),
        };
        let encoded_payload = serde_json::to_vec(&payload)
            .map_err(|error| DocumentError::Serialization(error.to_string()))?;
        let record = RecoveryRecord {
            checksum: hex_digest(&encoded_payload),
            payload,
        };
        let bytes = serde_json::to_vec(&record)
            .map_err(|error| DocumentError::Serialization(error.to_string()))?;
        atomic_private_write(&self.path_for(relative), &bytes)
    }

    fn clear(&self, relative: &Path) -> Result<(), DocumentError> {
        match self.load(relative)? {
            RecoveryLoad::Missing => Ok(()),
            RecoveryLoad::Corrupt { path, reason } => {
                Err(DocumentError::CorruptRecovery { path, reason })
            }
            RecoveryLoad::Valid { path, .. } => {
                fs::remove_file(path).map_err(|source| DocumentError::Io {
                    action: "remove completed document recovery",
                    source,
                })?;
                sync_directory(&self.root)
            }
        }
    }

    fn retain_displaced_inode(
        &self,
        relative: &Path,
        source: &Path,
        category: &str,
    ) -> Result<PathBuf, DocumentError> {
        let document_dir = self.root.join(format!(
            "{}-displaced",
            hash_fields(&[relative.as_os_str().as_bytes()])
        ));
        ensure_private_dir(&document_dir)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..32 {
            let path = document_dir.join(format!(
                "{category}-{stamp}-{}.bin",
                UNIQUE_NAME.fetch_add(1, Ordering::Relaxed)
            ));
            let source_c = path_cstring(source).map_err(|source| DocumentError::Io {
                action: "encode displaced-file source path",
                source,
            })?;
            let path_c = path_cstring(&path).map_err(|source| DocumentError::Io {
                action: "encode displaced-file recovery path",
                source,
            })?;
            let renamed = unsafe {
                renamex_np(
                    source_c.as_ptr(),
                    path_c.as_ptr(),
                    RENAME_EXCL | RENAME_NOFOLLOW_ANY,
                )
            };
            if renamed == 0 {
                // The path is useful even if a directory sync reports an
                // error. Returning an error here would make the caller report
                // the now-stale source path after the inode already moved.
                let _ = sync_directory(&document_dir);
                if let Some(source_parent) = source.parent() {
                    let _ = sync_directory(source_parent);
                }
                return Ok(path);
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::AlreadyExists => continue,
                _ => {
                    return Err(DocumentError::Io {
                        action: "retain displaced file inode in private recovery",
                        source: error,
                    });
                }
            }
        }
        Err(DocumentError::Io {
            action: "create unique displaced-file archive",
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "archive-name attempts exhausted",
            ),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct RecoveryRecord {
    payload: RecoveryPayload,
    checksum: String,
}

#[derive(Serialize, Deserialize)]
struct RecoveryPayload {
    schema: u32,
    scope_hash: String,
    path_hex: String,
    base: DiskSnapshot,
    buffer: String,
}

enum RecoveryLoad {
    Missing,
    Valid {
        payload: RecoveryPayload,
        path: PathBuf,
    },
    Corrupt {
        path: PathBuf,
        reason: String,
    },
}

enum RawDiskState {
    Present {
        bytes: Vec<u8>,
        version: DiskVersion,
    },
    Missing,
    Unsafe(TargetIssue),
}

fn validate_relative_path(path: &Path) -> Result<PathBuf, DocumentError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(DocumentError::InvalidRelativePath);
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(DocumentError::InvalidRelativePath);
    }
    Ok(path.to_path_buf())
}

fn path_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn classify_open_error(error: io::Error) -> Result<RawDiskState, DocumentError> {
    match error.raw_os_error() {
        Some(2) => Ok(RawDiskState::Missing),
        Some(13) => Ok(RawDiskState::Unsafe(TargetIssue::PermissionDenied)),
        Some(20) => Ok(RawDiskState::Unsafe(TargetIssue::NonRegular)),
        Some(31) => Ok(RawDiskState::Unsafe(TargetIssue::MultipleHardLinks)),
        Some(62) | Some(107) => Ok(RawDiskState::Unsafe(TargetIssue::SymlinkOrEscape)),
        _ => Err(DocumentError::Io {
            action: "open document without following links or escaping root",
            source: error,
        }),
    }
}

fn raw_to_disk(bytes: Vec<u8>, version: DiskVersion, limit: usize) -> DiskState {
    if bytes.len() > limit {
        return DiskState::Unsafe(TargetIssue::TooLarge {
            bytes: bytes.len() as u64,
            limit,
        });
    }
    match String::from_utf8(bytes) {
        Ok(text) => DiskState::Present(DiskSnapshot { text, version }),
        Err(_) => DiskState::Unsafe(TargetIssue::InvalidUtf8),
    }
}

fn disk_version(metadata: &fs::Metadata, bytes: &[u8]) -> DiskVersion {
    DiskVersion {
        sha256: hex_digest(bytes),
        len: bytes.len() as u64,
        device: metadata.dev(),
        inode: metadata.ino(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        mode: metadata.mode(),
    }
}

fn metadata_changed_during_read(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.mode() != after.mode()
}

fn disk_matches_base(disk: &DiskState, base: &DiskSnapshot) -> bool {
    matches!(disk, DiskState::Present(snapshot) if snapshot.version.is_same_generation(&base.version))
}

fn conflict_for(
    base: &DiskSnapshot,
    buffer: &str,
    external: &DiskState,
    retained_external: Option<PathBuf>,
    current: Option<DiskState>,
) -> Option<ConflictState> {
    if disk_matches_base(external, base) {
        return None;
    }
    let kind = match external {
        DiskState::Present(_) => ConflictKind::ExternalEdit,
        DiskState::Missing => ConflictKind::Missing,
        DiskState::Unsafe(_) => ConflictKind::Unsafe,
    };
    Some(ConflictState {
        kind,
        base: base.clone(),
        buffer: buffer.to_owned(),
        external: external.clone(),
        current: current.unwrap_or_else(|| external.clone()),
        retained_external,
    })
}

fn outcome_for_disk(disk: &DiskState, present: RefreshOutcome) -> RefreshOutcome {
    match disk {
        DiskState::Present(_) => present,
        DiskState::Missing => RefreshOutcome::Missing,
        DiskState::Unsafe(_) => RefreshOutcome::Unsafe,
    }
}

fn issue_for_initial_disk(disk: DiskState) -> DocumentError {
    match disk {
        DiskState::Present(_) => unreachable!(),
        DiskState::Missing => DocumentError::MissingTarget,
        DiskState::Unsafe(issue) => DocumentError::UnsafeTarget(issue),
    }
}

fn ensure_private_dir(path: &Path) -> Result<(), DocumentError> {
    if !path.exists() {
        fs::create_dir_all(path).map_err(|source| DocumentError::Io {
            action: "create private recovery directory",
            source,
        })?;
    }
    let metadata = fs::symlink_metadata(path).map_err(|source| DocumentError::Io {
        action: "inspect private recovery directory",
        source,
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(DocumentError::Io {
            action: "validate private recovery directory",
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "recovery path is not a real directory",
            ),
        });
    }
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIR_MODE)).map_err(|source| {
        DocumentError::Io {
            action: "make recovery directory private",
            source,
        }
    })
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<(), DocumentError> {
    let parent = path.parent().expect("recovery file has parent");
    for _ in 0..32 {
        let temp = parent.join(format!(
            ".recovery-{}-{}",
            std::process::id(),
            UNIQUE_NAME.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(PRIVATE_FILE_MODE);
        match options.open(&temp) {
            Ok(mut file) => {
                if let Err(source) = file.write_all(bytes).and_then(|_| file.sync_all()) {
                    let _ = fs::remove_file(&temp);
                    return Err(DocumentError::Io {
                        action: "write and sync document recovery",
                        source,
                    });
                }
                if let Err(source) = fs::rename(&temp, path) {
                    let _ = fs::remove_file(&temp);
                    return Err(DocumentError::Io {
                        action: "publish document recovery atomically",
                        source,
                    });
                }
                return sync_directory(parent);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(DocumentError::Io {
                    action: "create recovery candidate",
                    source,
                });
            }
        }
    }
    Err(DocumentError::Io {
        action: "create unique recovery candidate",
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "temporary-name attempts exhausted",
        ),
    })
}

fn sync_directory(path: &Path) -> Result<(), DocumentError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| DocumentError::Io {
            action: "sync recovery directory",
            source,
        })
}

fn hash_fields(fields: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for field in fields {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field);
    }
    hex_bytes(&hash.finalize())
}

fn hex_digest(bytes: &[u8]) -> String {
    hex_bytes(&Sha256::digest(bytes))
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}
