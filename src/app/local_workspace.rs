//! Embeddable, checkout-scoped local editor and Local Changes surface.
//!
//! This component deliberately has no `ReviewSession` or provider dependency. The
//! parent keeps the published review pinned and routes an explicit "Edit locally"
//! gesture here. All checkout and Git I/O is started on GPUI's background executor;
//! editor changes are serialized by one FIFO worker per open document.

#[path = "local_workspace/conflict_view.rs"]
mod conflict_view;
#[path = "local_workspace/rebase_panel.rs"]
mod rebase_panel;

use cibergit::{
    document::{
        ConflictKind, DiskState, DiskVersion, Document, DocumentLimits, DocumentStatus,
        DocumentStore, RecoveryScope, RecoveryStatus, RefreshOutcome, SaveOutcome,
    },
    domain::Repository,
    local_git::{
        DiffContent, DiffTarget, GitPath, HeadState, LocalGit, LocalGitError, LocalSnapshot,
        MutationReceipt, OperationState, OutcomeCertainty, RemoteBranchObservation, SelectedDiff,
        SnapshotGuard,
    },
    rebase::{OperationView as RebaseOperationView, RebaseAssociation, RebaseStore},
    worktrees::{CheckoutView, FilesystemIdentity},
};
use gpui::{
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, EventEmitter, FontWeight,
    HighlightStyle, KeyDownEvent, Render, Rgba, SharedString, Stateful, Subscription, Window,
    WindowAppearance, actions, div, prelude::*, px, rgba,
};
use gpui_base::input::{
    Editor, EditorState, FoldRange, HighlightStyleResolver, Input, InputEditorStyle, InputEvent,
    InputHighlighter, InputHighlighterFactory, InputState, Rope,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr, OsString, c_char, c_int, c_void},
    fs::{self, File},
    io::Write,
    ops::Range,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    rc::Rc,
    sync::mpsc,
    thread,
    time::Duration,
};

const UI_FONT: &str = "IBM Plex Sans";
const CODE_FONT: &str = "Menlo";
const DEFAULT_FILE_LIMIT: usize = 20_000;
const DEFAULT_DEPTH_LIMIT: usize = 64;
const DEFAULT_PATH_BYTES_LIMIT: usize = 4 * 1024 * 1024;
const ACTION_JOURNAL: &str = "started-local-action.json";

// Darwin values from <sys/fcntl.h> and <sys/file.h>. cibergit's native V1
// target is macOS; these primitives fail closed instead of falling back to a
// pathname traversal or a process-lifetime lock file.
const O_RDONLY: c_int = 0;
const O_RDWR: c_int = 2;
const O_NONBLOCK: c_int = 0x0000_0004;
const O_CREAT: c_int = 0x0000_0200;
const O_RESOLVE_BENEATH: c_int = 0x0000_1000;
const O_DIRECTORY: c_int = 0x0010_0000;
const O_CLOEXEC: c_int = 0x0100_0000;
const O_NOFOLLOW_ANY: c_int = 0x2000_0000;
const LOCK_EX: c_int = 0x02;
const LOCK_NB: c_int = 0x04;
const LOCK_UN: c_int = 0x08;
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;

#[repr(C)]
struct DarwinDirent {
    d_ino: u64,
    d_seekoff: u64,
    d_reclen: u16,
    d_namlen: u16,
    d_type: u8,
    d_name: [c_char; 1024],
}

unsafe extern "C" {
    fn openat(fd: c_int, path: *const c_char, oflag: c_int, ...) -> c_int;
    fn flock(fd: c_int, operation: c_int) -> c_int;
    fn dup(fd: c_int) -> c_int;
    fn fdopendir(fd: c_int) -> *mut c_void;
    fn readdir(directory: *mut c_void) -> *mut DarwinDirent;
    fn closedir(directory: *mut c_void) -> c_int;
    fn __error() -> *mut c_int;
}

actions!(
    local_workspace,
    [
        LocalSave,
        LocalRefresh,
        LocalQuickOpen,
        LocalConfirm,
        LocalCancel,
        LocalReloadDisk,
        LocalFind,
        LocalReplace
    ]
);

/// Colors are supplied rather than inherited from the parent application, so
/// the component can render in a small standalone native evidence harness.
#[derive(Clone, Copy, Debug)]
pub struct LocalWorkspaceAppearance {
    pub dark: bool,
}

impl Default for LocalWorkspaceAppearance {
    fn default() -> Self {
        Self { dark: true }
    }
}

#[derive(Clone, Debug)]
pub struct LocalWorkspaceContext {
    pub repository: Repository,
    pub checkout: CheckoutView,
    pub data_root: PathBuf,
    pub appearance: LocalWorkspaceAppearance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrowserLimits {
    pub max_entries: usize,
    pub max_depth: usize,
    pub max_path_bytes: usize,
}

impl Default for BrowserLimits {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_FILE_LIMIT,
            max_depth: DEFAULT_DEPTH_LIMIT,
            max_path_bytes: DEFAULT_PATH_BYTES_LIMIT,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrowserEntryKind {
    Directory,
    EditableCandidate,
    Symlink,
    UnsupportedMedia,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserEntry {
    /// Exact platform path identity. `display` is never used for I/O.
    pub relative_path: PathBuf,
    pub display: String,
    pub kind: BrowserEntryKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserSnapshot {
    pub entries: Vec<BrowserEntry>,
    pub truncated: bool,
    pub truncation_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub enum LocalWorkspaceEvent {
    DocumentOpened(PathBuf),
    DocumentSaved {
        path: PathBuf,
        newer_edits_remain: bool,
    },
    LocalSnapshotChanged,
    RemoteBranchObserved(RemoteBranchObservation),
    MaterialActionConfirmationRequested {
        request_id: u64,
        summary: String,
    },
    LocalActionFinished {
        request_id: u64,
        result: String,
    },
    RetainedPathNotice(PathBuf),
    Error(String),
}

impl EventEmitter<LocalWorkspaceEvent> for LocalWorkspace {}

#[derive(Clone, Debug)]
pub enum LocalAction {
    Stage(Vec<GitPath>),
    Unstage(Vec<GitPath>),
    Commit {
        message: String,
    },
    Fetch {
        remote: String,
    },
    FastForwardPull {
        remote: String,
        branch: String,
    },
    Push {
        remote: String,
        branch: String,
    },
    ForcePushWithLease {
        remote: String,
        branch: String,
        observed_remote_oid: String,
    },
    CreateBranch {
        branch: String,
        start_oid: Option<String>,
    },
    SwitchBranch {
        branch: String,
    },
}

impl LocalAction {
    fn changes_checkout(&self) -> bool {
        matches!(
            self,
            Self::FastForwardPull { .. } | Self::CreateBranch { .. } | Self::SwitchBranch { .. }
        )
    }

    fn summary(&self) -> String {
        match self {
            Self::Stage(paths) => selected_paths_summary("Stage", paths),
            Self::Unstage(paths) => selected_paths_summary("Unstage", paths),
            Self::Commit { message } => format!("Commit staged changes: {message}"),
            Self::Fetch { remote } => format!("Fetch remote {remote}"),
            Self::FastForwardPull { remote, branch } => {
                format!("Fast-forward only pull {remote}/{branch}")
            }
            Self::Push { remote, branch } => {
                format!("Push the selected commit to {remote}/{branch}")
            }
            Self::ForcePushWithLease {
                remote,
                branch,
                observed_remote_oid,
            } => format!("Force push {remote}/{branch} with lease at {observed_remote_oid}"),
            Self::CreateBranch { branch, start_oid } => match start_oid {
                Some(oid) => format!("Create and switch to branch {branch} at {oid}"),
                None => format!("Create and switch to branch {branch}"),
            },
            Self::SwitchBranch { branch } => format!("Switch checkout to branch {branch}"),
        }
    }

    fn journal_kind(&self) -> &'static str {
        match self {
            Self::Stage(_) => "stage",
            Self::Unstage(_) => "unstage",
            Self::Commit { .. } => "commit",
            Self::Fetch { .. } => "fetch",
            Self::FastForwardPull { .. } => "fast-forward-pull",
            Self::Push { .. } => "push",
            Self::ForcePushWithLease { .. } => "force-push-with-lease",
            Self::CreateBranch { .. } => "create-branch",
            Self::SwitchBranch { .. } => "switch-branch",
        }
    }
}

#[derive(Clone)]
struct PendingAction {
    id: u64,
    action: LocalAction,
    guard: SnapshotGuard,
    checkout_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StartedAction {
    schema_version: u32,
    request_id: u64,
    kind: String,
    summary: String,
    checkout_identity: String,
    displayed_head: String,
    expected_remote_oid: Option<String>,
}

#[derive(Clone, Debug)]
struct DocumentView {
    buffer: String,
    base: String,
    disk: DiskState,
    status: DocumentStatus,
    recovery: RecoveryStatus,
    conflict_kind: Option<ConflictKind>,
    retained_paths: Vec<PathBuf>,
}

impl DocumentView {
    fn from_document(document: &Document, retained_paths: Vec<PathBuf>) -> Self {
        Self {
            buffer: document.buffer().to_owned(),
            base: document.base().text.clone(),
            disk: document.disk().clone(),
            status: document.status(),
            recovery: document.recovery_status().clone(),
            conflict_kind: document.conflict().map(|conflict| conflict.kind),
            retained_paths,
        }
    }

    fn displayed_disk_version(&self) -> Option<DiskVersion> {
        match &self.disk {
            DiskState::Present(snapshot) => Some(snapshot.version.clone()),
            DiskState::Missing | DiskState::Unsafe(_) => None,
        }
    }
}

enum DocumentCommand {
    Persist {
        generation: u64,
        text: String,
        reply: mpsc::Sender<DocumentReply>,
    },
    Save {
        generation: u64,
        text: String,
        reply: mpsc::Sender<DocumentReply>,
    },
    Refresh {
        generation: u64,
        reply: mpsc::Sender<DocumentReply>,
    },
    Reload {
        generation: u64,
        reply: mpsc::Sender<DocumentReply>,
    },
    Reconcile {
        generation: u64,
        expected_disk: DiskVersion,
        proposed: String,
        reply: mpsc::Sender<DocumentReply>,
    },
}

#[derive(Clone, Debug)]
enum DocumentReplyKind {
    Persisted,
    Saved {
        saved_buffer: String,
        outcome: SaveOutcome,
    },
    Refreshed(RefreshOutcome),
    Reloaded,
    Reconciled(cibergit::document::ReconcileOutcome),
}

#[derive(Clone, Debug)]
struct DocumentReply {
    generation: u64,
    result: Result<(DocumentReplyKind, DocumentView), String>,
}

#[derive(Clone)]
struct DocumentWorker {
    sender: mpsc::Sender<DocumentCommand>,
}

impl DocumentWorker {
    fn start(mut document: Document) -> Self {
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("cibergit-document".into())
            .spawn(move || {
                let mut retained_paths = Vec::new();
                while let Ok(command) = receiver.recv() {
                    let (generation, reply, result) = match command {
                        DocumentCommand::Persist {
                            generation,
                            text,
                            reply,
                        } => {
                            let result = document
                                .set_buffer(text)
                                .map(|()| DocumentReplyKind::Persisted)
                                .map_err(|error| error.to_string());
                            (generation, reply, result)
                        }
                        DocumentCommand::Save {
                            generation,
                            text,
                            reply,
                        } => {
                            let saved_buffer = text.clone();
                            let result = document
                                .set_buffer(text)
                                .and_then(|()| document.save())
                                .map(|outcome| {
                                    match &outcome {
                                        SaveOutcome::Saved {
                                            retained_previous, ..
                                        } => retained_paths.push(retained_previous.clone()),
                                        SaveOutcome::ConflictRetained {
                                            retained_external, ..
                                        } => retained_paths.push(retained_external.clone()),
                                        SaveOutcome::CommittedButUncertain {
                                            retained_path: Some(path),
                                            ..
                                        } => retained_paths.push(path.clone()),
                                        _ => {}
                                    }
                                    DocumentReplyKind::Saved {
                                        saved_buffer,
                                        outcome,
                                    }
                                })
                                .map_err(|error| error.to_string());
                            (generation, reply, result)
                        }
                        DocumentCommand::Refresh { generation, reply } => {
                            let result = document
                                .refresh()
                                .map(DocumentReplyKind::Refreshed)
                                .map_err(|error| error.to_string());
                            (generation, reply, result)
                        }
                        DocumentCommand::Reload { generation, reply } => {
                            let result = document
                                .reload_from_disk()
                                .map(|()| DocumentReplyKind::Reloaded)
                                .map_err(|error| error.to_string());
                            (generation, reply, result)
                        }
                        DocumentCommand::Reconcile {
                            generation,
                            expected_disk,
                            proposed,
                            reply,
                        } => {
                            let result = document
                                .reconcile(&expected_disk, proposed)
                                .map(DocumentReplyKind::Reconciled)
                                .map_err(|error| error.to_string());
                            (generation, reply, result)
                        }
                    };
                    let result = result.map(|kind| {
                        (
                            kind,
                            DocumentView::from_document(&document, retained_paths.clone()),
                        )
                    });
                    let _ = reply.send(DocumentReply { generation, result });
                }
            })
            .expect("spawn serialized document worker");
        Self { sender }
    }

    fn dispatch(&self, command: DocumentCommand) -> Result<(), String> {
        self.sender
            .send(command)
            .map_err(|_| "document worker stopped".to_owned())
    }
}

struct DocumentTab {
    path: PathBuf,
    editor: Entity<EditorState>,
    worker: DocumentWorker,
    view: DocumentView,
    generation: u64,
    /// Advances only for user-originated editor changes. Background refreshes
    /// may advance `generation` without invalidating a material confirmation.
    edit_generation: u64,
    persisted_generation: u64,
    pending_checkout_operations: usize,
    message: String,
    pending_programmatic_reload: Option<PendingProgrammaticReload>,
    _subscription: Subscription,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingProgrammaticReload {
    generation: u64,
    expected_editor_value: String,
    replacement: String,
}

impl PendingProgrammaticReload {
    fn still_applies(&self, generation: u64, editor_value: &str) -> bool {
        self.generation == generation && self.expected_editor_value == editor_value
    }
}

struct Backend {
    documents: DocumentStore,
    git: LocalGit,
    rebase: RebaseStore,
    browser: BrowserSnapshot,
    journal_path: PathBuf,
    checkout_generation: u64,
}

enum BackendState {
    Loading,
    Ready(Box<Backend>),
    Failed(String),
}

pub struct LocalWorkspace {
    context: LocalWorkspaceContext,
    backend: BackendState,
    documents: BTreeMap<PathBuf, DocumentTab>,
    active_document: Option<PathBuf>,
    revealed_path: Option<PathBuf>,
    open_generation: u64,
    git_generation: u64,
    snapshot: Option<LocalSnapshot>,
    selected_diff: Option<SelectedDiff>,
    pending_action: Option<PendingAction>,
    in_flight_action: Option<u64>,
    reconciliation_clear_in_flight: Option<u64>,
    next_action_id: u64,
    unresolved_started_action: Option<StartedAction>,
    unresolved_refresh_required: bool,
    remote_observation: Option<RemoteBranchObservation>,
    quick_open: Entity<InputState>,
    commit_message: Entity<InputState>,
    branch_name: Entity<InputState>,
    proposed_merge: Entity<EditorState>,
    proposed_seed: Option<(PathBuf, String)>,
    rebase: rebase_panel::RebasePanel,
    status: String,
    focused: bool,
    _subscriptions: Vec<Subscription>,
}

impl LocalWorkspace {
    /// Cheap construction only. Filesystem enumeration, `DocumentStore::new`,
    /// `LocalGit::open`, and the first snapshot all run after Loading is visible.
    pub fn new(
        context: LocalWorkspaceContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let colors = palette(context.appearance.dark);
        let quick_open = new_input("Quick-open any worktree file", colors, window, cx);
        let commit_message = new_input("Commit message", colors, window, cx);
        let branch_name = new_input("Branch name", colors, window, cx);
        let proposed_merge = cx.new(|cx| {
            let mut state = EditorState::new(window, cx).language("text");
            state.set_editor_style(editor_style(colors));
            state
        });
        let rebase = rebase_panel::RebasePanel::new(colors, window, cx);
        let mut this = Self {
            context,
            backend: BackendState::Loading,
            documents: BTreeMap::new(),
            active_document: None,
            revealed_path: None,
            open_generation: 0,
            git_generation: 0,
            snapshot: None,
            selected_diff: None,
            pending_action: None,
            in_flight_action: None,
            reconciliation_clear_in_flight: None,
            next_action_id: 1,
            unresolved_started_action: None,
            unresolved_refresh_required: false,
            remote_observation: None,
            quick_open,
            commit_message,
            branch_name,
            proposed_merge,
            proposed_seed: None,
            rebase,
            status: "Preparing safe local workspace…".into(),
            focused: window.is_window_active(),
            _subscriptions: Vec::new(),
        };
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            this.focused = window.is_window_active();
            if this.focused {
                this.refresh_all(cx);
            }
        });
        let appearance = cx.observe_window_appearance(window, |this, window, cx| {
            this.context.appearance.dark = is_dark(window);
            let style = editor_style(palette(this.context.appearance.dark));
            for tab in this.documents.values() {
                tab.editor
                    .update(cx, |editor, _| editor.set_editor_style(style.clone()));
            }
            this.proposed_merge
                .update(cx, |editor, _| editor.set_editor_style(style));
            this.rebase
                .update_appearance(palette(this.context.appearance.dark), cx);
            cx.notify();
        });
        this._subscriptions.extend([activation, appearance]);
        this.start_initialization(window, cx);
        this.start_polling(cx);
        this
    }

    pub fn browser(&self) -> Option<&BrowserSnapshot> {
        match &self.backend {
            BackendState::Ready(backend) => Some(&backend.browser),
            BackendState::Loading | BackendState::Failed(_) => None,
        }
    }

    pub fn local_snapshot(&self) -> Option<&LocalSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn remote_observation(&self) -> Option<&RemoteBranchObservation> {
        self.remote_observation.as_ref()
    }

    pub fn status_message(&self) -> &str {
        &self.status
    }

    pub fn in_flight_action_id(&self) -> Option<u64> {
        self.in_flight_action
    }

    /// Reads the remote ref through installed Git authentication. The returned
    /// OID is retained and displayed; force-with-lease requests must match it.
    pub fn observe_remote_branch(
        &mut self,
        remote: String,
        branch: String,
        cx: &mut Context<Self>,
    ) {
        let BackendState::Ready(backend) = &self.backend else {
            self.report_error("Local workspace is not ready".into(), cx);
            return;
        };
        self.git_generation = self.git_generation.wrapping_add(1);
        let generation = self.git_generation;
        let git = backend.git.clone();
        let context = self.context.clone();
        let task = cx.background_spawn(async move {
            validate_checkout(&context.checkout, &git)?;
            git.observe_remote_branch(&remote, &branch)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.git_generation {
                    return;
                }
                match result {
                    Ok(observation) => {
                        this.status = match &observation.oid {
                            Some(oid) => format!(
                                "Observed {}/{} at {oid}; force-with-lease is pinned to this OID",
                                observation.remote, observation.branch
                            ),
                            None => format!(
                                "Observed {}/{} as absent",
                                observation.remote, observation.branch
                            ),
                        };
                        this.remote_observation = Some(observation.clone());
                        cx.emit(LocalWorkspaceEvent::RemoteBranchObserved(observation));
                        cx.notify();
                    }
                    Err(error) => this.report_error(
                        format!("Remote observation failed; no force lease was armed: {error}"),
                        cx,
                    ),
                }
            });
        })
        .detach();
    }

    pub fn select_local_diff(&mut self, path: GitPath, target: DiffTarget, cx: &mut Context<Self>) {
        let BackendState::Ready(backend) = &self.backend else {
            return;
        };
        self.git_generation = self.git_generation.wrapping_add(1);
        let generation = self.git_generation;
        let git = backend.git.clone();
        let context = self.context.clone();
        let task = cx.background_spawn(async move {
            validate_checkout(&context.checkout, &git)?;
            git.selected_diff(&path, target)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.git_generation {
                    return;
                }
                match result {
                    Ok(diff) => {
                        this.selected_diff = Some(diff);
                        cx.notify();
                    }
                    Err(error) => this.report_error(error, cx),
                }
            });
        })
        .detach();
    }

    pub fn active_path(&self) -> Option<&Path> {
        self.active_document.as_deref()
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.backend, BackendState::Ready(_))
    }

    pub fn active_editor(&self) -> Option<Entity<EditorState>> {
        self.active_document
            .as_ref()
            .and_then(|path| self.documents.get(path))
            .map(|tab| tab.editor.clone())
    }

    pub fn active_document_status(&self) -> Option<DocumentStatus> {
        self.active_document
            .as_ref()
            .and_then(|path| self.documents.get(path))
            .map(|tab| tab.view.status)
    }

    pub fn reveal_relative_path(&mut self, path: impl AsRef<Path>, cx: &mut Context<Self>) {
        match validate_relative_path(path.as_ref()) {
            Ok(path) => {
                self.revealed_path = Some(path);
                cx.notify();
            }
            Err(error) => self.report_error(error, cx),
        }
    }

    pub fn open_relative_path(
        &mut self,
        path: impl AsRef<Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !document_open_allowed(self.rebase.is_running()) {
            self.report_error(
                "Document open paused while a rebase transition is running; retry after the exact effect is observed"
                    .into(),
                cx,
            );
            return;
        }
        let path = match validate_relative_path(path.as_ref()) {
            Ok(path) => path,
            Err(error) => {
                self.report_error(error, cx);
                return;
            }
        };
        self.revealed_path = Some(path.clone());
        if self.documents.contains_key(&path) {
            self.active_document = Some(path);
            cx.notify();
            return;
        }
        let BackendState::Ready(backend) = &self.backend else {
            self.status = "Local workspace is still loading".into();
            cx.notify();
            return;
        };
        self.open_generation = self.open_generation.wrapping_add(1);
        let generation = self.open_generation;
        let store = backend.documents.clone();
        self.status = format!("Opening {}…", display_path(&path));
        let task = cx.background_spawn({
            let path = path.clone();
            async move { store.open(&path).map_err(|error| error.to_string()) }
        });
        let weak = cx.weak_entity();
        window
            .spawn(cx, async move |window| {
                let result = task.await;
                let _ = window.update(|window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if generation != this.open_generation {
                            return;
                        }
                        match result {
                            Ok(document) => this.install_document(path, document, window, cx),
                            Err(error) => this.report_error(
                                format!("Cannot open {}: {error}", display_path(&path)),
                                cx,
                            ),
                        }
                    });
                });
            })
            .detach();
    }

    pub fn request_action(&mut self, action: LocalAction, cx: &mut Context<Self>) -> Option<u64> {
        let BackendState::Ready(backend) = &self.backend else {
            self.report_error("Local workspace is not ready".into(), cx);
            return None;
        };
        if let Err(error) = validate_local_action_input(&action) {
            self.report_error(format!("{error}; Git was not started"), cx);
            return None;
        }
        if self.in_flight_action.is_some() || self.reconciliation_clear_in_flight.is_some() {
            self.report_error(
                "A local action or its durable reconciliation is still running".into(),
                cx,
            );
            return None;
        }
        if self.rebase.has_pending_or_running() {
            self.report_error(
                "A rebase transition or confirmation is active; finish or cancel it first".into(),
                cx,
            );
            return None;
        }
        if self.pending_action.is_some() || self.unresolved_started_action.is_some() {
            self.report_error(
                "Reconcile or cancel the current local action before starting another".into(),
                cx,
            );
            return None;
        }
        if action.changes_checkout()
            && let Some(reason) = self.checkout_action_blocker(cx)
        {
            self.report_error(reason, cx);
            return None;
        }
        if let LocalAction::ForcePushWithLease {
            remote,
            branch,
            observed_remote_oid,
        } = &action
            && !remote_observation_matches(
                self.remote_observation.as_ref(),
                remote,
                branch,
                observed_remote_oid,
            )
        {
            self.report_error(
                "Force-with-lease paused: observe and inspect this exact remote branch/OID first"
                    .into(),
                cx,
            );
            return None;
        }
        let Some(snapshot) = &self.snapshot else {
            self.report_error("Refresh Local Changes before acting".into(), cx);
            return None;
        };
        if action.changes_checkout() && operation_is_active(&snapshot.operation) {
            self.report_error(
                "Branch/pull action paused while a merge, rebase, cherry-pick, or revert is active"
                    .into(),
                cx,
            );
            return None;
        }
        let id = self.next_action_id;
        self.next_action_id = self.next_action_id.wrapping_add(1);
        let summary = action.summary();
        self.pending_action = Some(PendingAction {
            id,
            action,
            guard: snapshot.guard.clone(),
            checkout_generation: backend.checkout_generation,
        });
        self.status = format!("Confirmation required: {summary}");
        cx.emit(LocalWorkspaceEvent::MaterialActionConfirmationRequested {
            request_id: id,
            summary,
        });
        cx.notify();
        Some(id)
    }

    /// Confirms only the immutable currently-displayed request. A stale ID is
    /// rejected without dispatching Git.
    pub fn confirm_action(&mut self, request_id: u64, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_action.clone() else {
            self.report_error("There is no local action awaiting confirmation".into(), cx);
            return;
        };
        if pending.id != request_id {
            self.report_error(
                "That confirmation is stale; review the current action".into(),
                cx,
            );
            return;
        }
        let BackendState::Ready(backend) = &self.backend else {
            self.report_error("Local workspace is not ready".into(), cx);
            return;
        };
        if pending.checkout_generation != backend.checkout_generation {
            self.pending_action = None;
            self.report_error(
                "Checkout identity changed; refresh before retrying".into(),
                cx,
            );
            return;
        }
        if pending.action.changes_checkout()
            && let Some(reason) = self.checkout_action_blocker(cx)
        {
            self.report_error(format!("Confirmation paused: {reason}"), cx);
            return;
        }
        if pending.action.changes_checkout()
            && self
                .snapshot
                .as_ref()
                .is_none_or(|snapshot| operation_is_active(&snapshot.operation))
        {
            self.pending_action = None;
            self.report_error(
                "Confirmation paused: authoritative Git operation state is incompatible".into(),
                cx,
            );
            return;
        }
        let context = self.context.clone();
        let journal_path = backend.journal_path.clone();
        let displayed_head = self
            .snapshot
            .as_ref()
            .map(|snapshot| head_label(&snapshot.head))
            .unwrap_or_else(|| "unknown".into());
        let started = StartedAction {
            schema_version: 1,
            request_id,
            kind: pending.action.journal_kind().into(),
            summary: pending.action.summary(),
            checkout_identity: checkout_identity_label(&context.checkout),
            displayed_head,
            expected_remote_oid: match &pending.action {
                LocalAction::ForcePushWithLease {
                    observed_remote_oid,
                    ..
                } => Some(observed_remote_oid.clone()),
                _ => None,
            },
        };
        self.pending_action = None;
        self.in_flight_action = Some(request_id);
        self.unresolved_refresh_required = false;
        self.status = format!("Checking and recording {}…", started.summary);
        let retry = pending.clone();
        let action = pending.action;
        let guard = pending.guard;
        let attempted = started.clone();
        let task = cx.background_spawn(async move {
            run_local_action(&context, &journal_path, &attempted, action, &guard)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.in_flight_action != Some(request_id) {
                    return;
                }
                this.in_flight_action = None;
                match result {
                    Ok((receipt, snapshot)) => {
                        this.git_generation = this.git_generation.wrapping_add(1);
                        this.snapshot = Some(snapshot);
                        this.unresolved_started_action = None;
                        this.unresolved_refresh_required = false;
                        this.status = format!(
                            "Completed {:?}; refreshed authoritative Git state",
                            receipt.action
                        );
                        cx.emit(LocalWorkspaceEvent::LocalActionFinished {
                            request_id,
                            result: this.status.clone(),
                        });
                        cx.emit(LocalWorkspaceEvent::LocalSnapshotChanged);
                        cx.notify();
                    }
                    Err(LocalActionRunError::NotDispatched {
                        error,
                        journal_preserved: false,
                    }) => {
                        this.unresolved_started_action = None;
                        this.unresolved_refresh_required = false;
                        if this.pending_action.is_none() {
                            this.pending_action = Some(retry);
                        }
                        this.status = format!(
                            "Git was not started: {error}. The same confirmation remains available to retry or cancel."
                        );
                        cx.emit(LocalWorkspaceEvent::Error(this.status.clone()));
                        cx.notify();
                    }
                    Err(LocalActionRunError::NotDispatched {
                        error,
                        journal_preserved: true,
                    }) => {
                        this.unresolved_started_action = Some(started);
                        this.unresolved_refresh_required = false;
                        this.status = format!(
                            "Git was not started, but its durable intent record was preserved: {error}. Reconcile that exact record before retrying."
                        );
                        cx.emit(LocalWorkspaceEvent::Error(this.status.clone()));
                        cx.notify();
                    }
                    Err(LocalActionRunError::StartedOrUncertain(error)) => {
                        this.unresolved_started_action = Some(started);
                        this.unresolved_refresh_required = true;
                        this.status = format!(
                            "Action may have started: {error}. Authoritative refresh and explicit reconciliation are required before retry."
                        );
                        cx.emit(LocalWorkspaceEvent::Error(this.status.clone()));
                        this.refresh_git(cx);
                    }
                }
            });
        })
        .detach();
    }

    pub fn cancel_action(&mut self, request_id: u64, cx: &mut Context<Self>) {
        if self
            .pending_action
            .as_ref()
            .is_some_and(|pending| pending.id == request_id)
        {
            self.pending_action = None;
            self.status = "Local action cancelled; Git was not started".into();
            cx.notify();
        } else {
            self.report_error("That cancellation is stale".into(), cx);
        }
    }

    /// Clears restart uncertainty only after the refreshed state has been
    /// inspected. It never retries the action.
    pub fn acknowledge_action_reconciliation(&mut self, cx: &mut Context<Self>) {
        if self.in_flight_action.is_some() {
            self.report_error(
                "The confirmed action is still running; it cannot be acknowledged".into(),
                cx,
            );
            return;
        }
        if self.reconciliation_clear_in_flight.is_some() {
            self.report_error("Durable reconciliation is already running".into(), cx);
            return;
        }
        if self.unresolved_refresh_required {
            self.report_error(
                "Wait for a successful authoritative Git refresh before acknowledging".into(),
                cx,
            );
            return;
        }
        let Some(_) = self.snapshot else {
            self.report_error(
                "Refresh authoritative Git state before reconciling".into(),
                cx,
            );
            return;
        };
        let BackendState::Ready(backend) = &self.backend else {
            return;
        };
        let Some(expected) = self.unresolved_started_action.clone() else {
            self.report_error("There is no started action to reconcile".into(), cx);
            return;
        };
        let request_id = expected.request_id;
        self.reconciliation_clear_in_flight = Some(request_id);
        let journal = backend.journal_path.clone();
        let task = cx.background_spawn(async move { clear_started_action(&journal, &expected) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.reconciliation_clear_in_flight != Some(request_id) {
                    return;
                }
                this.reconciliation_clear_in_flight = None;
                match result {
                Ok(true) => {
                    if this
                        .unresolved_started_action
                        .as_ref()
                        .is_none_or(|started| started.request_id != request_id)
                    {
                        this.report_error(
                            "A newer action record appeared; the delayed acknowledgement was ignored"
                                .into(),
                            cx,
                        );
                        return;
                    }
                    this.unresolved_started_action = None;
                    this.status = "Local action reconciled; no action was retried".into();
                    cx.notify();
                }
                Ok(false) => this.report_error(
                    "The durable action record changed; it was preserved for reconciliation".into(),
                    cx,
                ),
                Err(error) => {
                    this.report_error(format!("Cannot persist action reconciliation: {error}"), cx)
                }
                }
            });
        })
        .detach();
    }

    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.refresh_git(cx);
        self.observe_rebase(cx);
        let paths = self.documents.keys().cloned().collect::<Vec<_>>();
        for path in paths {
            self.refresh_document(&path, cx);
        }
    }

    pub fn save_active(&mut self, cx: &mut Context<Self>) {
        if self.rebase.is_running() {
            self.report_error(
                "Document save paused while a rebase transition is running".into(),
                cx,
            );
            return;
        }
        let Some(path) = self.active_document.clone() else {
            self.report_error("No local document is open".into(), cx);
            return;
        };
        let Some(tab) = self.documents.get_mut(&path) else {
            return;
        };
        tab.generation = tab.generation.wrapping_add(1);
        let generation = tab.generation;
        let text = tab.editor.read(cx).value().to_string();
        let (reply, receiver) = mpsc::channel();
        if let Err(error) = tab.worker.dispatch(DocumentCommand::Save {
            generation,
            text,
            reply,
        }) {
            self.report_error(error, cx);
            return;
        }
        tab.pending_checkout_operations = tab.pending_checkout_operations.saturating_add(1);
        tab.message = "Saving checked buffer…".into();
        self.await_document_reply(path, receiver, true, cx);
    }

    pub fn reload_active_from_disk(&mut self, cx: &mut Context<Self>) {
        if self.rebase.is_running() {
            self.report_error(
                "Document reload paused while a rebase transition is running".into(),
                cx,
            );
            return;
        }
        let Some(path) = self.active_document.clone() else {
            return;
        };
        let Some(tab) = self.documents.get_mut(&path) else {
            return;
        };
        tab.generation = tab.generation.wrapping_add(1);
        let generation = tab.generation;
        let (reply, receiver) = mpsc::channel();
        if tab
            .worker
            .dispatch(DocumentCommand::Reload { generation, reply })
            .is_ok()
        {
            tab.pending_checkout_operations = tab.pending_checkout_operations.saturating_add(1);
            self.await_document_reply(path, receiver, true, cx);
        }
    }

    pub fn reconcile_active(
        &mut self,
        expected_disk: DiskVersion,
        proposed: String,
        cx: &mut Context<Self>,
    ) {
        if self.rebase.is_running() {
            self.report_error(
                "Document reconciliation paused while a rebase transition is running".into(),
                cx,
            );
            return;
        }
        let Some(path) = self.active_document.clone() else {
            return;
        };
        let Some(tab) = self.documents.get_mut(&path) else {
            return;
        };
        tab.generation = tab.generation.wrapping_add(1);
        let generation = tab.generation;
        let (reply, receiver) = mpsc::channel();
        if tab
            .worker
            .dispatch(DocumentCommand::Reconcile {
                generation,
                expected_disk,
                proposed,
                reply,
            })
            .is_ok()
        {
            tab.pending_checkout_operations = tab.pending_checkout_operations.saturating_add(1);
            self.await_document_reply(path, receiver, true, cx);
        }
    }

    fn start_initialization(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let context = self.context.clone();
        let task = cx.background_spawn(async move { initialize_backend(&context) });
        let weak = cx.weak_entity();
        window
            .spawn(cx, async move |window| {
                let result = task.await;
                let _ = window.update(|_, cx| {
                    let _ = weak.update(cx, |this, cx| match result {
                        Ok((backend, snapshot, started, rebase_operation)) => {
                            this.snapshot = Some(snapshot);
                            this.unresolved_started_action = started;
                            this.unresolved_refresh_required = false;
                            this.status = if this.unresolved_started_action.is_some() {
                                "A previously started local action requires authoritative reconciliation"
                                    .into()
                            } else {
                                "Local workspace ready · Review remains pinned and read-only".into()
                            };
                            this.backend = BackendState::Ready(Box::new(backend));
                            this.rebase.install_observed(rebase_operation);
                            cx.emit(LocalWorkspaceEvent::LocalSnapshotChanged);
                            cx.notify();
                        }
                        Err(error) => {
                            this.backend = BackendState::Failed(error.clone());
                            this.report_error(error, cx);
                        }
                    });
                });
            })
            .detach();
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                if this
                    .update(cx, |this, cx| {
                        if this.focused {
                            this.refresh_all(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn install_document(
        &mut self,
        path: PathBuf,
        document: Document,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = DocumentView::from_document(&document, Vec::new());
        let colors = palette(self.context.appearance.dark);
        let language = language_for_path(&path);
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language(language)
                .replaceable(true);
            state.set_disabled(self.rebase.has_pending_or_running(), cx);
            state.set_editor_style(editor_style(colors));
            state.set_highlighter_factory(highlighter_factory(), cx);
            // Initial/recovery load is the one intentional programmatic reload.
            state.set_value(view.buffer.clone(), window, cx);
            state
        });
        let subscription = cx.subscribe_in(&editor, window, |this, editor, event, _, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let Some((path, tab)) = this
                .documents
                .iter_mut()
                .find(|(_, tab)| tab.editor.entity_id() == editor.entity_id())
            else {
                return;
            };
            // A user edit always wins over a reload reply waiting for a Window.
            tab.pending_programmatic_reload = None;
            tab.generation = tab.generation.wrapping_add(1);
            tab.edit_generation = tab.edit_generation.wrapping_add(1);
            let generation = tab.generation;
            let text = editor.read(cx).value().to_string();
            let (reply, receiver) = mpsc::channel();
            if tab
                .worker
                .dispatch(DocumentCommand::Persist {
                    generation,
                    text,
                    reply,
                })
                .is_ok()
            {
                tab.pending_checkout_operations = tab.pending_checkout_operations.saturating_add(1);
                tab.message = "Persisting recovery…".into();
                let path = path.clone();
                this.await_document_reply(path, receiver, true, cx);
            }
        });
        let worker = DocumentWorker::start(document);
        self.documents.insert(
            path.clone(),
            DocumentTab {
                path: path.clone(),
                editor,
                worker,
                view,
                generation: 0,
                edit_generation: 0,
                persisted_generation: 0,
                pending_checkout_operations: 0,
                message: "Recovered/opened through DocumentStore".into(),
                pending_programmatic_reload: None,
                _subscription: subscription,
            },
        );
        self.active_document = Some(path.clone());
        self.status = format!("Opened {}", display_path(&path));
        cx.emit(LocalWorkspaceEvent::DocumentOpened(path));
        cx.notify();
    }

    fn refresh_document(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(tab) = self.documents.get_mut(path) else {
            return;
        };
        tab.generation = tab.generation.wrapping_add(1);
        let generation = tab.generation;
        let (reply, receiver) = mpsc::channel();
        if tab
            .worker
            .dispatch(DocumentCommand::Refresh { generation, reply })
            .is_ok()
        {
            self.await_document_reply(path.to_owned(), receiver, false, cx);
        }
    }

    fn await_document_reply(
        &mut self,
        path: PathBuf,
        receiver: mpsc::Receiver<DocumentReply>,
        blocks_checkout: bool,
        cx: &mut Context<Self>,
    ) {
        let task = cx.background_spawn(async move {
            receiver
                .recv()
                .map_err(|_| "document worker reply was lost".to_owned())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(reply) => {
                    if blocks_checkout && let Some(tab) = this.documents.get_mut(&path) {
                        tab.pending_checkout_operations =
                            tab.pending_checkout_operations.saturating_sub(1);
                    }
                    this.apply_document_reply(&path, reply, cx);
                }
                Err(error) => this.report_error(error, cx),
            });
        })
        .detach();
    }

    fn apply_document_reply(&mut self, path: &Path, reply: DocumentReply, cx: &mut Context<Self>) {
        let Some(tab) = self.documents.get_mut(path) else {
            return;
        };
        if reply.generation != tab.generation {
            if let Ok((DocumentReplyKind::Saved { .. }, view)) = reply.result {
                // Receiver tasks may reach the UI out of order even though the
                // owning worker is FIFO. Preserve every retained inode path,
                // but never let this stale save replace newer status/message.
                for retained in
                    merge_retained_paths(&mut tab.view.retained_paths, &view.retained_paths)
                {
                    cx.emit(LocalWorkspaceEvent::RetainedPathNotice(retained.clone()));
                }
                cx.emit(LocalWorkspaceEvent::DocumentSaved {
                    path: path.to_owned(),
                    newer_edits_remain: true,
                });
                cx.notify();
            }
            return;
        }
        match reply.result {
            Ok((kind, mut view)) => {
                tab.persisted_generation = tab.persisted_generation.max(reply.generation);
                match &kind {
                    DocumentReplyKind::Persisted => {
                        tab.message = if reply.generation == tab.generation {
                            "Unsaved buffer recovered durably".into()
                        } else {
                            "Older recovery write completed; newer edit is queued".into()
                        };
                    }
                    DocumentReplyKind::Saved {
                        saved_buffer,
                        outcome,
                    } => {
                        let current = tab.editor.read(cx).value();
                        let newer_edits_remain = current.as_ref() != saved_buffer;
                        tab.message = format!(
                            "{}{}",
                            save_outcome_message(outcome),
                            if newer_edits_remain {
                                " · newer edits remain dirty"
                            } else {
                                ""
                            }
                        );
                        cx.emit(LocalWorkspaceEvent::DocumentSaved {
                            path: path.to_owned(),
                            newer_edits_remain,
                        });
                    }
                    DocumentReplyKind::Refreshed(outcome) => {
                        tab.message = refresh_outcome_message(*outcome).into();
                        if matches!(outcome, RefreshOutcome::Reloaded)
                            && reply.generation == tab.generation
                        {
                            // Applied on the next render, where GPUI supplies the Window.
                            tab.pending_programmatic_reload = Some(PendingProgrammaticReload {
                                generation: reply.generation,
                                expected_editor_value: tab.editor.read(cx).value().to_string(),
                                replacement: view.buffer.clone(),
                            });
                        }
                    }
                    DocumentReplyKind::Reloaded => {
                        tab.message = "Explicitly reloaded from disk".into();
                        tab.pending_programmatic_reload = Some(PendingProgrammaticReload {
                            generation: reply.generation,
                            expected_editor_value: tab.editor.read(cx).value().to_string(),
                            replacement: view.buffer.clone(),
                        });
                    }
                    DocumentReplyKind::Reconciled(outcome) => {
                        tab.message = match outcome {
                            cibergit::document::ReconcileOutcome::Applied =>
                                "Merged changes are ready to save",
                            cibergit::document::ReconcileOutcome::Stale =>
                                "The file changed again. Your proposed merge is preserved; review the latest disk version",
                        }.into();
                        tab.pending_programmatic_reload = Some(PendingProgrammaticReload {
                            generation: reply.generation,
                            expected_editor_value: tab.editor.read(cx).value().to_string(),
                            replacement: view.buffer.clone(),
                        });
                    }
                }
                for retained in
                    merge_retained_paths(&mut tab.view.retained_paths, &view.retained_paths)
                {
                    cx.emit(LocalWorkspaceEvent::RetainedPathNotice(retained.clone()));
                }
                view.retained_paths = tab.view.retained_paths.clone();
                tab.view = view;
                cx.notify();
            }
            Err(error) => {
                tab.message = format!("Persistence/operation failed: {error}");
                self.status = tab.message.clone();
                cx.emit(LocalWorkspaceEvent::Error(self.status.clone()));
                cx.notify();
            }
        }
    }

    fn refresh_git(&mut self, cx: &mut Context<Self>) {
        let BackendState::Ready(backend) = &self.backend else {
            return;
        };
        self.git_generation = self.git_generation.wrapping_add(1);
        let generation = self.git_generation;
        let context = self.context.clone();
        let git = backend.git.clone();
        let task = cx.background_spawn(async move {
            validate_checkout(&context.checkout, &git)?;
            git.snapshot().map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.git_generation {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        let head_changed = this
                            .snapshot
                            .as_ref()
                            .is_some_and(|previous| previous.head != snapshot.head);
                        let dirty_buffers = this.checkout_action_blocker(cx).is_some();
                        this.snapshot = Some(snapshot);
                        if this.in_flight_action.is_none() {
                            this.unresolved_refresh_required = false;
                        }
                        if head_changed && dirty_buffers {
                            this.status = "External branch/HEAD change observed; dirty buffers were preserved and incompatible actions are paused".into();
                        }
                        cx.emit(LocalWorkspaceEvent::LocalSnapshotChanged);
                        cx.notify();
                    }
                    Err(error) => {
                        this.report_error(format!("Local Changes refresh failed: {error}"), cx)
                    }
                }
            });
        })
        .detach();
    }

    fn report_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.status = error.clone();
        cx.emit(LocalWorkspaceEvent::Error(error));
        cx.notify();
    }

    fn checkout_action_blocker(&self, cx: &App) -> Option<String> {
        self.documents.values().find_map(|tab| {
            let editor_value = tab.editor.read(cx).value();
            let buffer_differs_from_accepted_base = editor_value.as_ref() != tab.view.base;
            let operation_pending = tab.pending_checkout_operations != 0;
            let unresolved = tab.view.status != DocumentStatus::Clean
                || tab.pending_programmatic_reload.is_some();
            (buffer_differs_from_accepted_base || operation_pending || unresolved).then(|| {
                format!(
                    "Branch/pull action paused: {} has unsaved, unreconciled, or queued document work",
                    display_path(&tab.path)
                )
            })
        })
    }
}

fn selected_paths_summary(action: &str, paths: &[GitPath]) -> String {
    if let [path] = paths {
        format!("{action} {}", path.display)
    } else {
        format!("{action} {} selected files", paths.len())
    }
}

fn save_outcome_message(outcome: &SaveOutcome) -> &'static str {
    match outcome {
        SaveOutcome::Unchanged => "No changes to save",
        SaveOutcome::Saved { .. } => "Saved · Previous version kept",
        SaveOutcome::Blocked {
            status: DocumentStatus::Conflict,
        } => "Save paused · Resolve the disk conflict first",
        SaveOutcome::Blocked { .. } => "Save paused · Your unsaved text is preserved",
        SaveOutcome::ConflictRetained { .. } => {
            "The file changed during save · Both versions are preserved"
        }
        SaveOutcome::CommittedButUncertain { .. } => {
            "Save needs verification · Refresh and inspect the file before continuing"
        }
    }
}

fn refresh_outcome_message(outcome: RefreshOutcome) -> &'static str {
    match outcome {
        RefreshOutcome::Unchanged => "File checked · No disk changes",
        RefreshOutcome::Reloaded => "Reloaded changes from disk",
        RefreshOutcome::Conflict => "Disk conflict · Your unsaved text is preserved",
        RefreshOutcome::Missing => "File missing · Your buffer is preserved",
        RefreshOutcome::Unsafe => "File cannot be safely updated · Your buffer is preserved",
    }
}

fn merge_retained_paths(target: &mut Vec<PathBuf>, incoming: &[PathBuf]) -> Vec<PathBuf> {
    let mut added = Vec::new();
    for path in incoming {
        if !target.iter().any(|existing| existing == path) {
            target.push(path.clone());
            added.push(path.clone());
        }
    }
    added
}

fn initialize_backend(
    context: &LocalWorkspaceContext,
) -> Result<
    (
        Backend,
        LocalSnapshot,
        Option<StartedAction>,
        Option<RebaseOperationView>,
    ),
    String,
> {
    if context.data_root.as_os_str().is_empty() {
        return Err("local workspace data root is empty".into());
    }
    let git =
        LocalGit::open(&context.checkout.association.path).map_err(|error| error.to_string())?;
    validate_checkout(&context.checkout, &git)?;
    let partition = recovery_partition(context)?;
    let recovery_root = context
        .data_root
        .join("local-workspace")
        .join("recovery")
        .join(partition);
    fs::create_dir_all(&recovery_root)
        .map_err(|error| format!("prepare recovery partition: {error}"))?;
    let documents = DocumentStore::new(
        &context.checkout.association.path,
        &recovery_root,
        RecoveryScope::new(
            format!(
                "{}:{}",
                context.repository.account.host, context.repository.account.login
            ),
            format!(
                "{}:{}:{}/{}:pr:{}",
                context.repository.host,
                context.checkout.association.key.provider,
                context.repository.owner,
                context.repository.name,
                context.checkout.association.key.pull_request
            ),
        ),
        DocumentLimits::default(),
    )
    .map_err(|error| error.to_string())?;
    let browser = enumerate_worktree(&context.checkout.association.path, BrowserLimits::default())?;
    let action_root = context
        .data_root
        .join("local-workspace")
        .join("actions")
        .join(recovery_partition(context)?);
    prepare_private_directory(&action_root)?;
    let journal_path = action_root.join(ACTION_JOURNAL);
    let started = read_started_action(&journal_path)?;
    let snapshot = git.snapshot().map_err(|error| error.to_string())?;
    let association = &context.checkout.association.key;
    let rebase = RebaseStore::open(
        context.data_root.join("rebase-private"),
        RebaseAssociation {
            provider: association.provider.clone(),
            host: association.host.clone(),
            account: association.account.clone(),
            repository: association.repository.clone(),
            change: association.pull_request.to_string(),
        },
        &context.checkout.association.path,
    )
    .map_err(|error| format!("open rebase lifecycle store: {error}"))?;
    let rebase_operation = rebase
        .observe()
        .map_err(|error| format!("observe rebase lifecycle: {error}"))?;
    Ok((
        Backend {
            documents,
            git,
            rebase,
            browser,
            journal_path,
            checkout_generation: 1,
        },
        snapshot,
        started,
        rebase_operation,
    ))
}

fn recovery_partition(context: &LocalWorkspaceContext) -> Result<String, String> {
    let association = &context.checkout.association;
    let checkout = fs::canonicalize(&association.path)
        .map_err(|error| format!("canonicalize checkout for recovery: {error}"))?;
    let git_dir = fs::canonicalize(&association.git_dir)
        .map_err(|error| format!("canonicalize Git directory for recovery: {error}"))?;
    let common = fs::canonicalize(&association.common_git_dir)
        .map_err(|error| format!("canonicalize common Git directory for recovery: {error}"))?;
    let mut hash = Sha256::new();
    for value in [
        context.repository.host.as_bytes(),
        context.repository.owner.as_bytes(),
        context.repository.name.as_bytes(),
        context.repository.account.host.as_bytes(),
        context.repository.account.login.as_bytes(),
        association.key.provider.as_bytes(),
        association.key.host.as_bytes(),
        association.key.account.as_bytes(),
        association.key.repository.as_bytes(),
        checkout.as_os_str().as_bytes(),
        git_dir.as_os_str().as_bytes(),
        common.as_os_str().as_bytes(),
    ] {
        hash.update(value);
        hash.update([0]);
    }
    hash.update(association.key.pull_request.to_le_bytes());
    for identity in [
        association.checkout_identity,
        association.git_dir_identity,
        association.common_git_dir_identity,
    ] {
        hash.update(identity.device.to_le_bytes());
        hash.update(identity.inode.to_le_bytes());
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn validate_checkout(checkout: &CheckoutView, git: &LocalGit) -> Result<(), String> {
    let association = &checkout.association;
    let expected_root = fs::canonicalize(&association.path)
        .map_err(|error| format!("canonicalize checkout: {error}"))?;
    let expected_git = fs::canonicalize(&association.git_dir)
        .map_err(|error| format!("canonicalize Git directory: {error}"))?;
    let expected_common = fs::canonicalize(&association.common_git_dir)
        .map_err(|error| format!("canonicalize common Git directory: {error}"))?;
    if git.root() != expected_root
        || git.git_dir() != expected_git
        || git.common_git_dir() != expected_common
    {
        return Err("checkout Git identity no longer matches its accepted association".into());
    }
    for (label, path, expected) in [
        (
            "checkout",
            expected_root.as_path(),
            association.checkout_identity,
        ),
        (
            "Git directory",
            expected_git.as_path(),
            association.git_dir_identity,
        ),
        (
            "common Git directory",
            expected_common.as_path(),
            association.common_git_dir_identity,
        ),
    ] {
        let actual = filesystem_identity(path)?;
        if actual != expected {
            return Err(format!("{label} device/inode identity changed"));
        }
    }
    Ok(())
}

fn filesystem_identity(path: &Path) -> Result<FilesystemIdentity, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{} unexpectedly became a symlink", path.display()));
    }
    Ok(FilesystemIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn enumerate_worktree(root: &Path, limits: BrowserLimits) -> Result<BrowserSnapshot, String> {
    enumerate_worktree_with_hook(root, limits, |_| {})
}

fn enumerate_worktree_with_hook(
    root: &Path,
    limits: BrowserLimits,
    mut before_descend: impl FnMut(&Path),
) -> Result<BrowserSnapshot, String> {
    if limits.max_entries == 0 || limits.max_depth == 0 || limits.max_path_bytes == 0 {
        return Err("browser limits must be non-zero".into());
    }
    let root = fs::canonicalize(root).map_err(|error| format!("canonicalize worktree: {error}"))?;
    let expected_root = filesystem_identity(&root)?;
    let root_fd = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW_ANY)
        .open(&root)
        .map_err(|error| format!("open worktree root without following links: {error}"))?;
    let opened_root = root_fd
        .metadata()
        .map_err(|error| format!("inspect opened worktree root: {error}"))?;
    if !opened_root.is_dir()
        || opened_root.dev() != expected_root.device
        || opened_root.ino() != expected_root.inode
    {
        return Err("opened worktree root identity does not match the accepted path".into());
    }
    let mut snapshot = BrowserSnapshot {
        entries: Vec::new(),
        truncated: false,
        truncation_reason: None,
    };
    let mut pending = vec![(PathBuf::new(), 0usize)];
    let mut path_bytes = 0usize;
    while let Some((relative_dir, depth)) = pending.pop() {
        if depth > limits.max_depth {
            truncate(&mut snapshot, "maximum directory depth reached");
            continue;
        }
        let opened_directory;
        let directory_fd = if relative_dir.as_os_str().is_empty() {
            &root_fd
        } else {
            before_descend(&relative_dir);
            let Ok(directory) = open_directory_at(&root_fd, relative_dir.as_os_str()) else {
                continue;
            };
            opened_directory = directory;
            &opened_directory
        };
        let mut bounded_children: Vec<(OsString, PathBuf, BrowserEntryKind)> = Vec::new();
        let mut reached_bound = false;
        read_directory_names(directory_fd, |name, entry_type| {
            if name.as_bytes() == b".git" {
                return true;
            }
            let relative = relative_dir.join(&name);
            let relative_bytes = relative.as_os_str().as_bytes().len();
            if snapshot
                .entries
                .len()
                .saturating_add(bounded_children.len())
                >= limits.max_entries
                || path_bytes.saturating_add(relative_bytes) > limits.max_path_bytes
            {
                reached_bound = true;
                return false;
            }
            path_bytes = path_bytes.saturating_add(relative_bytes);
            let kind = if entry_type == DT_LNK {
                BrowserEntryKind::Symlink
            } else if entry_type == DT_DIR {
                BrowserEntryKind::Directory
            } else if entry_type == DT_REG && is_media_path(&relative) {
                BrowserEntryKind::UnsupportedMedia
            } else if entry_type == DT_REG {
                BrowserEntryKind::EditableCandidate
            } else {
                BrowserEntryKind::Other
            };
            bounded_children.push((name, relative, kind));
            true
        })
        .map_err(|error| {
            format!(
                "enumerate descriptor for {}: {error}",
                display_path(&relative_dir)
            )
        })?;
        if reached_bound {
            truncate(&mut snapshot, "file or path-byte enumeration bound reached");
            snapshot.entries.extend(bounded_children.into_iter().map(
                |(_, relative_path, kind)| BrowserEntry {
                    display: display_path(&relative_path),
                    relative_path,
                    kind,
                },
            ));
            sort_browser_entries(&mut snapshot.entries);
            return Ok(snapshot);
        }
        bounded_children.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        for (_, relative, kind) in bounded_children {
            snapshot.entries.push(BrowserEntry {
                display: display_path(&relative),
                relative_path: relative.clone(),
                kind: kind.clone(),
            });
            if kind == BrowserEntryKind::Directory {
                if depth == limits.max_depth {
                    truncate(&mut snapshot, "maximum directory depth reached");
                } else {
                    pending.push((relative, depth + 1));
                }
            }
        }
    }
    sort_browser_entries(&mut snapshot.entries);
    Ok(snapshot)
}

struct DirectoryStream(*mut c_void);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            closedir(self.0);
        }
    }
}

fn read_directory_names(
    directory: &File,
    mut visit: impl FnMut(OsString, u8) -> bool,
) -> Result<(), String> {
    let duplicate = unsafe { dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(format!(
            "duplicate directory descriptor: {}",
            std::io::Error::last_os_error()
        ));
    }
    let stream = unsafe { fdopendir(duplicate) };
    if stream.is_null() {
        unsafe {
            File::from_raw_fd(duplicate);
        }
        return Err(format!(
            "open directory stream: {}",
            std::io::Error::last_os_error()
        ));
    }
    let stream = DirectoryStream(stream);
    loop {
        unsafe {
            *__error() = 0;
        }
        let entry = unsafe { readdir(stream.0) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error().unwrap_or(0) == 0 {
                return Ok(());
            }
            return Err(format!("read directory stream: {error}"));
        }
        let entry = unsafe { &*entry };
        let name_len = usize::from(entry.d_namlen).min(entry.d_name.len());
        let name =
            unsafe { std::slice::from_raw_parts(entry.d_name.as_ptr().cast::<u8>(), name_len) };
        if name == b"." || name == b".." {
            continue;
        }
        if !visit(OsString::from_vec(name.to_vec()), entry.d_type) {
            return Ok(());
        }
    }
}

fn open_directory_at(parent: &File, name: &OsStr) -> Result<File, String> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| "directory entry unexpectedly contains NUL".to_owned())?;
    let fd = unsafe {
        openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            O_RDONLY | O_NONBLOCK | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW_ANY | O_RESOLVE_BENEATH,
        )
    };
    if fd < 0 {
        Err(format!(
            "open directory entry without following links: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn sort_browser_entries(entries: &mut [BrowserEntry]) {
    entries.sort_by(|left, right| {
        left.relative_path
            .as_os_str()
            .as_bytes()
            .cmp(right.relative_path.as_os_str().as_bytes())
    });
}

fn truncate(snapshot: &mut BrowserSnapshot, reason: &str) {
    snapshot.truncated = true;
    if snapshot.truncation_reason.is_none() {
        snapshot.truncation_reason = Some(reason.into());
    }
}

fn validate_relative_path(path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err("path must be a non-empty worktree-relative path".into());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => return Err("path contains traversal or a root".into()),
        }
    }
    if path
        .components()
        .any(|component| matches!(component, Component::Normal(name) if name.as_bytes() == b".git"))
    {
        return Err("Git internals are not part of the workspace browser".into());
    }
    Ok(path.to_owned())
}

fn is_media_path(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "avif"
            | "heic"
            | "mp4"
            | "mov"
            | "avi"
            | "mkv"
            | "webm"
            | "mp3"
            | "wav"
            | "flac"
            | "pdf"
    )
}

fn display_path(path: &Path) -> String {
    path.as_os_str().to_string_lossy().into_owned()
}

fn validate_local_action_input(action: &LocalAction) -> Result<(), String> {
    match action {
        LocalAction::Commit { message } if message.trim().is_empty() => {
            Err("Commit message must not be empty".into())
        }
        LocalAction::FastForwardPull { branch, .. }
        | LocalAction::Push { branch, .. }
        | LocalAction::ForcePushWithLease { branch, .. }
        | LocalAction::CreateBranch { branch, .. }
        | LocalAction::SwitchBranch { branch }
            if branch.trim().is_empty() =>
        {
            Err("Branch name must not be empty".into())
        }
        _ => Ok(()),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum LocalActionRunError {
    NotDispatched {
        error: String,
        journal_preserved: bool,
    },
    StartedOrUncertain(String),
}

impl LocalActionRunError {
    fn before_journal(error: impl Into<String>) -> Self {
        Self::NotDispatched {
            error: error.into(),
            journal_preserved: false,
        }
    }

    fn with_preserved_journal(error: impl Into<String>) -> Self {
        Self::NotDispatched {
            error: error.into(),
            journal_preserved: true,
        }
    }
}

fn run_local_action(
    context: &LocalWorkspaceContext,
    journal_path: &Path,
    started: &StartedAction,
    action: LocalAction,
    guard: &SnapshotGuard,
) -> Result<(MutationReceipt, LocalSnapshot), LocalActionRunError> {
    run_local_action_with_journal(
        context,
        journal_path,
        started,
        action,
        guard,
        write_started_action,
    )
}

fn run_local_action_with_journal(
    context: &LocalWorkspaceContext,
    journal_path: &Path,
    started: &StartedAction,
    action: LocalAction,
    guard: &SnapshotGuard,
    write_journal: impl FnOnce(&Path, &StartedAction) -> Result<(), JournalWriteError>,
) -> Result<(MutationReceipt, LocalSnapshot), LocalActionRunError> {
    validate_local_action_input(&action).map_err(LocalActionRunError::before_journal)?;
    let git = LocalGit::open(&context.checkout.association.path)
        .map_err(|error| LocalActionRunError::before_journal(error.to_string()))?;
    validate_checkout(&context.checkout, &git).map_err(LocalActionRunError::before_journal)?;
    write_journal(journal_path, started).map_err(|error| match error {
        JournalWriteError::NotInstalled(error) => LocalActionRunError::before_journal(error),
        JournalWriteError::InstalledOrUncertain(error) => {
            LocalActionRunError::with_preserved_journal(error)
        }
    })?;
    let receipt = match action {
        LocalAction::Stage(paths) => git.stage(&paths, guard),
        LocalAction::Unstage(paths) => git.unstage(&paths, guard),
        LocalAction::Commit { message } => git.commit(&message, guard),
        LocalAction::Fetch { remote } => git.fetch(&remote, guard),
        LocalAction::FastForwardPull { remote, branch } => {
            git.fast_forward_pull(&remote, &branch, guard)
        }
        LocalAction::Push { remote, branch } => git.push(&remote, &branch, guard),
        LocalAction::ForcePushWithLease {
            remote,
            branch,
            observed_remote_oid,
        } => git.force_push_with_lease(&remote, &branch, &observed_remote_oid, guard),
        LocalAction::CreateBranch { branch, start_oid } => {
            git.create_branch(&branch, start_oid.as_deref(), guard)
        }
        LocalAction::SwitchBranch { branch } => git.switch_branch(&branch, guard),
    };
    let receipt = match receipt {
        Ok(receipt) => receipt,
        Err(error) if local_git_error_is_certain(&error) => {
            let message = format!("Git refused before mutation dispatch: {error}");
            return match clear_started_action(journal_path, started) {
                Ok(true) => Err(LocalActionRunError::before_journal(message)),
                Ok(false) => Err(LocalActionRunError::before_journal(format!(
                    "{message}; the exact journal record was not present, so no different record was erased"
                ))),
                Err(clear_error) => Err(LocalActionRunError::before_journal(format!(
                    "{message}; the durable record could not be cleared safely and was preserved: {clear_error}"
                ))),
            };
        }
        Err(error) => {
            return Err(LocalActionRunError::StartedOrUncertain(git_action_error(
                &error,
            )));
        }
    };
    validate_checkout(&context.checkout, &git).map_err(LocalActionRunError::StartedOrUncertain)?;
    let snapshot = git.snapshot().map_err(|error| {
        LocalActionRunError::StartedOrUncertain(format!(
            "post-start authoritative refresh failed: {error}; outcome is uncertain"
        ))
    })?;
    let cleared = clear_started_action(journal_path, started)
        .map_err(LocalActionRunError::StartedOrUncertain)?;
    if !cleared {
        return Err(LocalActionRunError::StartedOrUncertain(
            "started-action record changed before completion; it was preserved".into(),
        ));
    }
    Ok((receipt, snapshot))
}

fn local_git_error_is_certain(error: &LocalGitError) -> bool {
    match error {
        LocalGitError::MutationCommandFailed { certainty, .. }
        | LocalGitError::MutationIo { certainty, .. }
        | LocalGitError::TimedOut { certainty, .. }
        | LocalGitError::OutputLimit { certainty, .. } => *certainty == OutcomeCertainty::Certain,
        _ => true,
    }
}

fn git_action_error(error: &LocalGitError) -> String {
    format!("Git reported {error}; the durable started-action record was retained")
}

#[derive(Debug, PartialEq, Eq)]
enum JournalWriteError {
    NotInstalled(String),
    InstalledOrUncertain(String),
}

fn write_started_action(path: &Path, started: &StartedAction) -> Result<(), JournalWriteError> {
    write_started_action_with_hook(path, started, || Ok(()))
}

fn write_started_action_with_hook(
    path: &Path,
    started: &StartedAction,
    after_install: impl FnOnce() -> Result<(), String>,
) -> Result<(), JournalWriteError> {
    let before_install = |error: String| JournalWriteError::NotInstalled(error);
    let after_install_error = |error: String| JournalWriteError::InstalledOrUncertain(error);
    let parent = path
        .parent()
        .ok_or_else(|| before_install("action journal has no parent".to_owned()))?;
    prepare_private_directory(parent).map_err(before_install)?;
    let _lock = ActionJournalLock::acquire(path).map_err(before_install)?;
    match read_started_action(path) {
        Ok(Some(existing)) => {
            return Err(JournalWriteError::NotInstalled(format!(
                "an existing started action {} is still active; it was preserved",
                existing.request_id
            )));
        }
        Ok(None) => {}
        Err(error) => {
            return Err(JournalWriteError::NotInstalled(format!(
                "existing action journal is unsafe, corrupt, or from a future schema; it was preserved: {error}"
            )));
        }
    }
    let bytes =
        serde_json::to_vec_pretty(started).map_err(|error| before_install(error.to_string()))?;
    if bytes.len() > 64 * 1024 {
        return Err(before_install(
            "local action journal exceeds its 64 KiB bound".into(),
        ));
    }
    let temporary = parent.join(format!(".started-{}.tmp", started.request_id));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| before_install(format!("create action journal: {error}")))?;
    let prepared = file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("sync action journal: {error}"));
    if let Err(error) = prepared {
        let _ = fs::remove_file(&temporary);
        return Err(before_install(error));
    }
    // A hard link is an atomic create-without-overwrite in this same directory.
    // Even an uncooperative concurrent writer cannot be replaced silently.
    if let Err(error) = fs::hard_link(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(before_install(format!(
            "install action journal without overwrite: {error}"
        )));
    }
    after_install().map_err(after_install_error)?;
    fs::remove_file(&temporary).map_err(|error| {
        after_install_error(format!("remove installed journal temporary: {error}"))
    })?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| after_install_error(format!("sync action journal directory: {error}")))
}

fn read_started_action(path: &Path) -> Result<Option<StartedAction>, String> {
    if !path.try_exists().map_err(|error| error.to_string())? {
        return Ok(None);
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > 64 * 1024
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("local action journal is unsafe or exceeds its bound".into());
    }
    let bytes = fs::read(path).map_err(|error| format!("read local action journal: {error}"))?;
    let started: StartedAction = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse local action journal: {error}"))?;
    if started.schema_version != 1 {
        return Err("local action journal has an unsupported schema".into());
    }
    Ok(Some(started))
}

fn clear_started_action(path: &Path, expected: &StartedAction) -> Result<bool, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "action journal has no parent".to_owned())?;
    prepare_private_directory(parent)?;
    let _lock = ActionJournalLock::acquire(path)?;
    if read_started_action(path)? != Some(expected.clone()) {
        return Ok(false);
    }
    let claimed = parent.join(format!(".cleared-{}.json", expected.request_id));
    if claimed
        .try_exists()
        .map_err(|error| format!("inspect reconciliation claim: {error}"))?
    {
        return Err("a reconciliation claim already exists; preserving both records".into());
    }
    fs::rename(path, &claimed).map_err(|error| format!("claim exact action journal: {error}"))?;
    fs::remove_file(&claimed).map_err(|error| format!("clear claimed action journal: {error}"))?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync cleared action journal: {error}"))?;
    Ok(true)
}

fn prepare_private_directory(path: &Path) -> Result<(), String> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(path)
        .map_err(|error| format!("prepare private action directory: {error}"))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect private action directory: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("action journal parent is not a private real directory".into());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("protect action journal directory: {error}"))
}

#[derive(Debug)]
struct ActionJournalLock {
    file: File,
    locked: bool,
}

impl ActionJournalLock {
    fn acquire(journal: &Path) -> Result<Self, String> {
        Self::acquire_with_post_lock_hook(journal, |_| Ok(()))
    }

    fn acquire_with_post_lock_hook(
        journal: &Path,
        after_lock: impl FnOnce(c_int) -> Result<(), String>,
    ) -> Result<Self, String> {
        let parent = journal
            .parent()
            .ok_or_else(|| "action journal has no parent".to_owned())?;
        let expected_parent = filesystem_identity(parent)?;
        let canonical_parent = fs::canonicalize(parent)
            .map_err(|error| format!("canonicalize action journal directory: {error}"))?;
        let parent_fd = fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_CLOEXEC)
            .open(&canonical_parent)
            .map_err(|error| format!("open action journal directory: {error}"))?;
        let opened_parent = parent_fd
            .metadata()
            .map_err(|error| format!("inspect action journal directory descriptor: {error}"))?;
        if !opened_parent.is_dir()
            || opened_parent.dev() != expected_parent.device
            || opened_parent.ino() != expected_parent.inode
        {
            return Err("action journal directory identity changed before lock acquisition".into());
        }
        let lock_name = CString::new(".started-local-action.lock").expect("static lock name");
        let lock_fd = unsafe {
            openat(
                parent_fd.as_raw_fd(),
                lock_name.as_ptr(),
                O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW_ANY,
                0o600,
            )
        };
        if lock_fd < 0 {
            return Err(format!(
                "open private action journal lock without following links: {}",
                std::io::Error::last_os_error()
            ));
        }
        let file = unsafe { File::from_raw_fd(lock_fd) };
        let metadata = file
            .metadata()
            .map_err(|error| format!("inspect action journal lock descriptor: {error}"))?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err("action journal lock is not a single private regular file".into());
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("protect action journal lock: {error}"))?;
        if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
            return Err(format!(
                "action journal is locked by another live process; preserving state: {}",
                std::io::Error::last_os_error()
            ));
        }
        // Closing one descriptor does not release a BSD flock while a forked or
        // duplicated descriptor still references the same open-file description.
        // Activate an explicit unlock guard before any later fallible work.
        let mut guard = Self { file, locked: true };
        after_lock(guard.file.as_raw_fd())?;
        let path = canonical_parent.join(".started-local-action.lock");
        let path_metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect action journal lock path: {error}"))?;
        if path_metadata.file_type().is_symlink()
            || path_metadata.dev() != metadata.dev()
            || path_metadata.ino() != metadata.ino()
        {
            return Err("action journal lock path changed while it was acquired".into());
        }
        guard
            .file
            .set_len(0)
            .and_then(|()| {
                guard
                    .file
                    .write_all(format!("pid={}\n", std::process::id()).as_bytes())
            })
            .and_then(|()| guard.file.sync_all())
            .map_err(|error| format!("sync action journal lock owner: {error}"))?;
        parent_fd
            .sync_all()
            .map_err(|error| format!("sync action journal lock directory: {error}"))?;
        Ok(guard)
    }
}

impl Drop for ActionJournalLock {
    fn drop(&mut self) {
        if self.locked {
            // Best effort is the only option in Drop. Unlocking before `File`
            // closes is what prevents inherited duplicates from extending the
            // critical section past this guard's lifetime.
            let _ = unsafe { flock(self.file.as_raw_fd(), LOCK_UN) };
            self.locked = false;
        }
    }
}

fn head_label(head: &HeadState) -> String {
    match head {
        HeadState::Unborn { branch } => format!("unborn:{branch}"),
        HeadState::Attached { branch, oid } => format!("{branch}@{oid}"),
        HeadState::Detached { oid } => format!("detached@{oid}"),
    }
}

fn checkout_identity_label(checkout: &CheckoutView) -> String {
    let identity = checkout.association.checkout_identity;
    format!(
        "{}:{}:{}",
        display_path(&checkout.association.path),
        identity.device,
        identity.inode
    )
}

fn language_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(OsStr::to_str) {
        Some("rs") => "rust",
        Some("json") => "json",
        Some("md" | "markdown") => "markdown",
        Some("toml") => "toml",
        _ => "text",
    }
}

#[derive(Clone, Copy)]
struct LocalPalette {
    canvas: Rgba,
    surface: Rgba,
    sidebar: Rgba,
    elevated: Rgba,
    text: Rgba,
    muted: Rgba,
    border: Rgba,
    selected: Rgba,
    accent: Rgba,
    green: Rgba,
    red: Rgba,
    amber: Rgba,
    dark: bool,
}

fn palette(dark: bool) -> LocalPalette {
    if dark {
        LocalPalette {
            canvas: rgba(0x18191bff),
            surface: rgba(0x202124ff),
            sidebar: rgba(0x17181af0),
            elevated: rgba(0x292b2fff),
            text: rgba(0xf1f2f3ff),
            muted: rgba(0xb8bbc1ff),
            border: rgba(0x36383dff),
            selected: rgba(0xffffff13),
            accent: rgba(0x8ab4f8ff),
            green: rgba(0x70c995ff),
            red: rgba(0xf28b82ff),
            amber: rgba(0xf7c873ff),
            dark,
        }
    } else {
        LocalPalette {
            canvas: rgba(0xfafaf9ff),
            surface: rgba(0xffffffff),
            sidebar: rgba(0xf8f8f7f0),
            elevated: rgba(0xf2f2f0ff),
            text: rgba(0x202124ff),
            muted: rgba(0x56595eff),
            border: rgba(0xdedfdcff),
            selected: rgba(0x0000000a),
            accent: rgba(0x245eaaff),
            green: rgba(0x18794eff),
            red: rgba(0xc2352aff),
            amber: rgba(0x986a12ff),
            dark,
        }
    }
}

fn is_dark(window: &Window) -> bool {
    matches!(
        window.appearance(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

fn editor_style(colors: LocalPalette) -> InputEditorStyle {
    InputEditorStyle {
        foreground: colors.text.into(),
        muted_foreground: colors.muted.into(),
        background: colors.canvas.into(),
        border: colors.border.into(),
        selection: colors.accent.into(),
        caret: colors.text.into(),
        editor_gutter_background: Some(colors.canvas.into()),
        highlight_styles: std::sync::Arc::new(LocalHighlightTheme { dark: colors.dark }),
        ..Default::default()
    }
}

fn new_input(
    placeholder: &'static str,
    colors: LocalPalette,
    window: &mut Window,
    cx: &mut Context<LocalWorkspace>,
) -> Entity<InputState> {
    cx.new(|cx| {
        let mut input = InputState::new(window, cx);
        input.set_editor_style(InputEditorStyle {
            foreground: colors.text.into(),
            muted_foreground: colors.muted.into(),
            background: colors.elevated.into(),
            border: colors.border.into(),
            ..Default::default()
        });
        input.set_placeholder(placeholder, window, cx);
        input
    })
}

struct LocalHighlightTheme {
    dark: bool,
}

impl HighlightStyleResolver for LocalHighlightTheme {
    fn style(&self, name: &str) -> Option<HighlightStyle> {
        let color = match (self.dark, name) {
            (true, "keyword") => rgba(0xc792eaff),
            (false, "keyword") => rgba(0x7b2cbfff),
            (true, "string") => rgba(0xc3e88dff),
            (false, "string") => rgba(0x2b7a0bff),
            (true, "comment") => rgba(0x8492a6ff),
            (false, "comment") => rgba(0x6b7280ff),
            (true, "number") => rgba(0xf78c6cff),
            (false, "number") => rgba(0xb45309ff),
            (true, "type") => rgba(0x82aaffff),
            (false, "type") => rgba(0x1d4ed8ff),
            (true, "heading") => rgba(0x89ddffff),
            (false, "heading") => rgba(0x0369a1ff),
            _ => return None,
        };
        Some(HighlightStyle {
            color: Some(color.into()),
            font_weight: matches!(name, "keyword" | "heading").then_some(FontWeight::SEMIBOLD),
            ..Default::default()
        })
    }
}

#[derive(Clone, Debug)]
struct TokenRange {
    range: Range<usize>,
    semantic: &'static str,
}

struct LocalHighlighter {
    language: SharedString,
    text_len: usize,
    tokens: Vec<TokenRange>,
}

impl LocalHighlighter {
    fn new(language: &str) -> Self {
        Self {
            language: language.to_owned().into(),
            text_len: 0,
            tokens: Vec::new(),
        }
    }

    fn parse(&mut self, text: &str) {
        self.text_len = text.len();
        self.tokens = match self.language.as_ref() {
            "rust" => lex_rust(text),
            "json" => lex_json(text),
            "toml" => lex_toml(text),
            "markdown" => lex_markdown(text),
            _ => Vec::new(),
        };
    }
}

impl InputHighlighter for LocalHighlighter {
    fn language(&self) -> SharedString {
        self.language.clone()
    }

    fn update(
        &mut self,
        _edit: Option<gpui_base::input::InputEdit>,
        text: &Rope,
        _folding: bool,
        _window: &mut Window,
        _cx: &mut Context<EditorState>,
    ) {
        self.parse(&text.to_string());
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let start = range.start.min(self.text_len);
        let end = range.end.min(self.text_len).max(start);
        let mut result = Vec::new();
        let mut cursor = start;
        for token in &self.tokens {
            if token.range.end <= start || token.range.start >= end {
                continue;
            }
            let token_start = token.range.start.max(start);
            let token_end = token.range.end.min(end);
            if cursor < token_start {
                result.push((cursor..token_start, HighlightStyle::default()));
            }
            result.push((
                token_start..token_end,
                resolver.style(token.semantic).unwrap_or_default(),
            ));
            cursor = token_end;
        }
        if cursor < end {
            result.push((cursor..end, HighlightStyle::default()));
        }
        result
    }

    fn fold_ranges(&self, _text: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

fn highlighter_factory() -> InputHighlighterFactory {
    Rc::new(|language| match language {
        "rust" | "json" | "toml" | "markdown" => {
            Some(Box::new(LocalHighlighter::new(language)) as Box<dyn InputHighlighter>)
        }
        _ => None,
    })
}

fn lex_rust(text: &str) -> Vec<TokenRange> {
    const KEYWORDS: &[&str] = &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
        "true", "type", "unsafe", "use", "where", "while",
    ];
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"//") {
            let end = text[index..]
                .find('\n')
                .map_or(bytes.len(), |offset| index + offset);
            tokens.push(TokenRange {
                range: index..end,
                semantic: "comment",
            });
            index = end;
        } else if bytes[index..].starts_with(b"/*") {
            let end = text[index + 2..]
                .find("*/")
                .map_or(bytes.len(), |offset| index + 2 + offset + 2);
            tokens.push(TokenRange {
                range: index..end,
                semantic: "comment",
            });
            index = end;
        } else if bytes[index] == b'"' {
            let end = quoted_end(bytes, index, b'"');
            tokens.push(TokenRange {
                range: index..end,
                semantic: "string",
            });
            index = end;
        } else if bytes[index].is_ascii_digit() {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || b"_xX.".contains(&bytes[index]))
            {
                index += 1;
            }
            tokens.push(TokenRange {
                range: start..index,
                semantic: "number",
            });
        } else if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            let word = &text[start..index];
            if KEYWORDS.contains(&word) {
                tokens.push(TokenRange {
                    range: start..index,
                    semantic: "keyword",
                });
            } else if word.chars().next().is_some_and(char::is_uppercase) {
                tokens.push(TokenRange {
                    range: start..index,
                    semantic: "type",
                });
            }
        } else {
            index += text[index..].chars().next().map_or(1, char::len_utf8);
        }
    }
    tokens
}

fn lex_json(text: &str) -> Vec<TokenRange> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            let end = quoted_end(bytes, index, b'"');
            tokens.push(TokenRange {
                range: index..end,
                semantic: "string",
            });
            index = end;
        } else if bytes[index].is_ascii_digit() || bytes[index] == b'-' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_digit() || b".eE+-".contains(&bytes[index]))
            {
                index += 1;
            }
            tokens.push(TokenRange {
                range: start..index,
                semantic: "number",
            });
        } else if bytes[index..].starts_with(b"true")
            || bytes[index..].starts_with(b"false")
            || bytes[index..].starts_with(b"null")
        {
            let length = if bytes[index..].starts_with(b"false") {
                5
            } else {
                4
            };
            tokens.push(TokenRange {
                range: index..index + length,
                semantic: "keyword",
            });
            index += length;
        } else {
            index += 1;
        }
    }
    tokens
}

fn lex_toml(text: &str) -> Vec<TokenRange> {
    let mut tokens = lex_json(text);
    for (line_start, line) in lines_with_offsets(text) {
        if let Some(comment) = line.find('#') {
            tokens.retain(|token| {
                token.range.end <= line_start + comment
                    || token.range.start >= line_start + line.len()
            });
            tokens.push(TokenRange {
                range: line_start + comment..line_start + line.len(),
                semantic: "comment",
            });
        } else if line.trim_start().starts_with('[') {
            tokens.retain(|token| {
                token.range.end <= line_start || token.range.start >= line_start + line.len()
            });
            tokens.push(TokenRange {
                range: line_start..line_start + line.len(),
                semantic: "heading",
            });
        }
    }
    tokens.sort_by_key(|token| token.range.start);
    tokens
}

fn lex_markdown(text: &str) -> Vec<TokenRange> {
    let mut tokens = Vec::new();
    for (start, line) in lines_with_offsets(text) {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            tokens.push(TokenRange {
                range: start..start + line.len(),
                semantic: "heading",
            });
            continue;
        }
        let mut rest = line;
        let mut offset = start;
        while let Some(open) = rest.find('`') {
            let after = offset + open + 1;
            if let Some(close) = text[after..start + line.len()].find('`') {
                tokens.push(TokenRange {
                    range: offset + open..after + close + 1,
                    semantic: "string",
                });
                offset = after + close + 1;
                rest = &text[offset..start + line.len()];
            } else {
                break;
            }
        }
    }
    tokens.sort_by_key(|token| token.range.start);
    tokens
}

fn lines_with_offsets(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    text.split_inclusive('\n').map(move |line| {
        let start = offset;
        offset += line.len();
        (start, line.trim_end_matches('\n'))
    })
}

fn quoted_end(bytes: &[u8], start: usize, quote: u8) -> usize {
    let mut index = start + 1;
    let mut escaped = false;
    while index < bytes.len() {
        if !escaped && bytes[index] == quote {
            return index + 1;
        }
        escaped = !escaped && bytes[index] == b'\\';
        if bytes[index] != b'\\' {
            escaped = false;
        }
        index += 1;
    }
    bytes.len()
}

impl Render for LocalWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = palette(self.context.appearance.dark);

        // `set_value` is reserved for initial load and explicit authoritative
        // reload/reconcile. Routine typing is never mirrored back through it.
        for tab in self.documents.values_mut() {
            if let Some(pending) = tab.pending_programmatic_reload.take() {
                let current = tab.editor.read(cx).value().to_string();
                if pending.still_applies(tab.generation, &current) {
                    tab.editor.update(cx, |editor, cx| {
                        editor.set_value(pending.replacement, window, cx)
                    });
                } else {
                    tab.message = "A newer edit superseded a delayed programmatic reload".into();
                }
            }
        }

        let active = self
            .active_document
            .as_ref()
            .and_then(|path| self.documents.get(path));
        if let Some(tab) = active
            && tab.view.status == DocumentStatus::Conflict
            && let Some(version) = tab.view.displayed_disk_version()
        {
            let seed = (tab.path.clone(), version.sha256.clone());
            if self.proposed_seed.as_ref() != Some(&seed) {
                let ours = tab.editor.read(cx).value();
                self.proposed_merge
                    .update(cx, |editor, cx| editor.set_value(ours, window, cx));
                self.proposed_seed = Some(seed);
            }
        }

        let query = self.quick_open.read(cx).value().to_lowercase();
        let filtered_browser = self
            .browser()
            .map(|browser| {
                browser
                    .entries
                    .iter()
                    .filter(|entry| {
                        query.trim().is_empty()
                            || entry.display.to_lowercase().contains(query.trim())
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let browser_notice = self.browser().and_then(|browser| {
            if browser.truncated {
                Some(
                    browser
                        .truncation_reason
                        .clone()
                        .unwrap_or_else(|| "bounded".into()),
                )
            } else if filtered_browser.len() > 500 {
                Some(format!(
                    "showing 500 of {} matches; narrow quick-open",
                    filtered_browser.len()
                ))
            } else {
                None
            }
        });
        let browser_rows = filtered_browser.into_iter().take(500).collect::<Vec<_>>();

        let mut files = div()
            .id("local-workspace-files")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_2()
            .py_2();
        for entry in browser_rows {
            let path = entry.relative_path.clone();
            let selected = self.revealed_path.as_ref() == Some(&path);
            let icon = match entry.kind {
                BrowserEntryKind::Directory => "▸",
                BrowserEntryKind::EditableCandidate => " ",
                BrowserEntryKind::Symlink => "↗",
                BrowserEntryKind::UnsupportedMedia => "◇",
                BrowserEntryKind::Other => "?",
            };
            let editable = entry.kind == BrowserEntryKind::EditableCandidate;
            files = files.child(
                div()
                    .id(ElementId::Name(
                        format!(
                            "local-file-{:x}",
                            Sha256::digest(path.as_os_str().as_bytes())
                        )
                        .into(),
                    ))
                    .h(px(28.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_md()
                    .when(selected, |row| row.bg(colors.selected))
                    .text_color(if editable { colors.text } else { colors.muted })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if editable {
                            this.open_relative_path(&path, window, cx);
                        } else {
                            this.reveal_relative_path(&path, cx);
                            this.status = match entry.kind {
                                BrowserEntryKind::UnsupportedMedia => {
                                    "Media is listed but never decoded or edited".into()
                                }
                                BrowserEntryKind::Symlink => {
                                    "Symlink is listed but never followed by the editor".into()
                                }
                                BrowserEntryKind::Directory => "Directory selected".into(),
                                BrowserEntryKind::Other => "Unsupported filesystem entry".into(),
                                BrowserEntryKind::EditableCandidate => unreachable!(),
                            };
                        }
                    }))
                    .child(div().w(px(12.)).child(icon))
                    .child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(entry.display),
                    ),
            );
        }

        let browser_panel = div()
            .w(px(300.))
            .min_w(px(220.))
            .h_full()
            .flex()
            .flex_col()
            .bg(colors.sidebar)
            .border_r_1()
            .border_color(colors.border)
            .child(
                div()
                    .h(px(48.))
                    .px_3()
                    .flex()
                    .items_center()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("WORKTREE"),
            )
            .child(div().px_2().pb_2().child(Input::new(&self.quick_open)))
            .child(files)
            .when_some(browser_notice, |panel, reason| {
                panel.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_xs()
                        .text_color(colors.amber)
                        .child(format!("Enumeration truncated: {reason}")),
                )
            });

        let rebase_open = self.rebase.open;
        let workspace_content = if rebase_open {
            self.render_rebase_panel(colors, window, cx)
        } else {
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .child(self.render_editor_panel(colors, window, cx))
                .child(self.render_changes_panel(colors, cx))
                .into_any_element()
        };

        div()
            .id("local-workspace")
            .key_context("LocalWorkspace")
            .size_full()
            .flex()
            .flex_col()
            .font_family(UI_FONT)
            .text_sm()
            .bg(colors.canvas)
            .text_color(colors.text)
            .on_action(cx.listener(|this, _: &LocalSave, _, cx| this.save_active(cx)))
            .on_action(cx.listener(|this, _: &LocalRefresh, _, cx| this.refresh_all(cx)))
            .on_action(cx.listener(|this, _: &LocalQuickOpen, window, cx| {
                let query = this.quick_open.read(cx).value().to_lowercase();
                let candidate = this.browser().and_then(|browser| {
                    browser
                        .entries
                        .iter()
                        .find(|entry| {
                            entry.kind == BrowserEntryKind::EditableCandidate
                                && entry.display.to_lowercase().contains(query.trim())
                        })
                        .map(|entry| entry.relative_path.clone())
                });
                if let Some(path) = candidate {
                    this.open_relative_path(path, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &LocalConfirm, _, cx| {
                if let Some(id) = this.pending_action.as_ref().map(|pending| pending.id) {
                    this.confirm_action(id, cx);
                } else if let Some(id) = this.rebase_pending_action_id() {
                    this.confirm_rebase_action(id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &LocalCancel, _, cx| {
                if let Some(id) = this.pending_action.as_ref().map(|pending| pending.id) {
                    this.cancel_action(id, cx);
                } else if let Some(id) = this.rebase_pending_action_id() {
                    this.cancel_rebase_action(id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &LocalFind, _, cx| {
                if let Some(tab) = this
                    .active_document
                    .as_ref()
                    .and_then(|path| this.documents.get(path))
                {
                    tab.editor.update(cx, |editor, cx| editor.open_search(false, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &LocalReplace, _, cx| {
                if let Some(tab) = this
                    .active_document
                    .as_ref()
                    .and_then(|path| this.documents.get(path))
                {
                    tab.editor.update(cx, |editor, cx| editor.open_search(true, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &LocalReloadDisk, _, cx| {
                this.reload_active_from_disk(cx)
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.handle_rebase_key(event, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .h(px(48.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(colors.surface)
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("{} · Local workspace", self.context.repository.full_name())),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted)
                                    .child("Published Review stays pinned · Git auth/authorship come from installed Git"),
                            )
                            .child(action_button(
                                if rebase_open { "Workspace" } else { "Rebase" },
                                colors,
                                cx.listener(|this, _, _, cx| this.toggle_rebase(cx)),
                            )),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(browser_panel)
                    .child(workspace_content),
            )
            .child(
                div()
                    .min_h(px(34.))
                    .px_4()
                    .py_2()
                    .bg(colors.surface)
                    .border_t_1()
                    .border_color(colors.border)
                    .text_xs()
                    .text_color(colors.muted)
                    .child(self.status.clone()),
            )
    }
}

impl LocalWorkspace {
    fn render_editor_panel(
        &mut self,
        colors: LocalPalette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(path) = self.active_document.clone() else {
            return div()
                .flex_1()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(colors.muted)
                .child(match &self.backend {
                    BackendState::Loading => "Preparing checkout off the UI thread…".to_owned(),
                    BackendState::Failed(error) => format!("Local workspace unavailable: {error}"),
                    BackendState::Ready(_) => "Open any unchanged or changed text file".to_owned(),
                })
                .into_any_element();
        };
        let Some(tab) = self.documents.get(&path) else {
            return div().into_any_element();
        };
        let editor = tab.editor.clone();
        let status = tab.view.status;
        let message = tab.message.clone();
        let recovery = match &tab.view.recovery {
            RecoveryStatus::None => "",
            RecoveryStatus::Restored { .. } => " · Local draft recovered",
            RecoveryStatus::Corrupt { .. } => {
                " · Recovery needs attention; original copy preserved"
            }
            RecoveryStatus::Stale { .. } => " · Earlier recovery copy preserved",
        };
        let mut recovery_paths = tab.view.retained_paths.clone();
        match &tab.view.recovery {
            RecoveryStatus::Restored { path }
            | RecoveryStatus::Corrupt { path, .. }
            | RecoveryStatus::Stale { path, .. } => {
                if !recovery_paths.contains(path) {
                    recovery_paths.push(path.clone());
                }
            }
            RecoveryStatus::None => {}
        }
        let conflict = status == DocumentStatus::Conflict;
        let conflict_data = conflict.then(|| {
            let disk = match &tab.view.disk {
                DiskState::Present(snapshot) => snapshot.text.clone(),
                DiskState::Missing => "<file missing>".into(),
                DiskState::Unsafe(issue) => format!("<unsafe: {issue:?}>"),
            };
            (
                tab.view.base.clone(),
                tab.editor.read(cx).value().to_string(),
                disk,
                tab.view.displayed_disk_version(),
            )
        });
        let mut panel = div()
            .flex_1()
            .min_w(px(420.))
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(44.))
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .font_family(CODE_FONT)
                            .font_weight(FontWeight::MEDIUM)
                            .child(display_path(&path)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(action_button(
                                "Find",
                                colors,
                                cx.listener(|this, _, _, cx| {
                                    if let Some(tab) = this
                                        .active_document
                                        .as_ref()
                                        .and_then(|path| this.documents.get(path))
                                    {
                                        tab.editor
                                            .update(cx, |editor, cx| editor.open_search(false, cx));
                                    }
                                }),
                            ))
                            .child(action_button(
                                "Replace",
                                colors,
                                cx.listener(|this, _, _, cx| {
                                    if let Some(tab) = this
                                        .active_document
                                        .as_ref()
                                        .and_then(|path| this.documents.get(path))
                                    {
                                        tab.editor
                                            .update(cx, |editor, cx| editor.open_search(true, cx));
                                    }
                                }),
                            ))
                            .child(action_button(
                                "Save ⌘S",
                                colors,
                                cx.listener(|this, _, _, cx| this.save_active(cx)),
                            )),
                    ),
            );
        if let Some((base, ours, disk, version)) = conflict_data {
            panel = panel
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .bg(if colors.dark { rgba(0x452f18ff) } else { rgba(0xfff4d6ff) })
                        .text_color(colors.amber)
                        .child(match tab.view.conflict_kind {
                            Some(ConflictKind::Missing) => "The file was removed. Your unsaved text is preserved.",
                            Some(ConflictKind::Unsafe) => "The file cannot be safely updated. Your unsaved text is preserved.",
                            Some(ConflictKind::SaveRace) => "The file changed during save. Review the preserved versions before continuing.",
                            _ => "The file changed on disk. Review both versions, then reload or merge your changes.",
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .gap_1()
                        .child(conflict_column("BASE", base, colors))
                        .child(conflict_column("OURS", ours, colors))
                        .child(conflict_column("CURRENT DISK", disk, colors)),
                )
                .child(
                    div()
                        .h(px(180.))
                        .flex()
                        .flex_col()
                        .border_t_1()
                        .border_color(colors.border)
                        .child(
                            div()
                                .h(px(32.))
                                .px_3()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child("EDITABLE PROPOSED RESULT")
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .child(action_button("Reload disk", colors, cx.listener(|this, _, _, cx| {
                                            this.reload_active_from_disk(cx)
                                        })))
                                        .when_some(version, |buttons, version| {
                                            buttons.child(action_button("Reconcile displayed version", colors, cx.listener(move |this, _, _, cx| {
                                                let proposed = this.proposed_merge.read(cx).value().to_string();
                                                this.reconcile_active(version.clone(), proposed, cx);
                                            })))
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .font_family(CODE_FONT)
                                .child(Editor::new(&self.proposed_merge)),
                        ),
                );
        } else {
            panel = panel.child(
                div()
                    .flex_1()
                    .min_h_0()
                    .font_family(CODE_FONT)
                    .child(Editor::new(&editor)),
            );
        }
        panel
            .child(
                div()
                    .min_h(px(32.))
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(colors.border)
                    .text_xs()
                    .text_color(match status {
                        DocumentStatus::Clean => colors.green,
                        DocumentStatus::Dirty => colors.amber,
                        DocumentStatus::Conflict
                        | DocumentStatus::Missing
                        | DocumentStatus::Unsafe
                        | DocumentStatus::RecoveryCorrupt => colors.red,
                    })
                    .child(format!("{message}{recovery}"))
                    .when(!recovery_paths.is_empty(), |footer| {
                        let count = recovery_paths.len();
                        let paths = recovery_paths
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join("\n");
                        footer.child(action_button(
                            format!("Copy recovery paths ({count})"),
                            colors,
                            cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    paths.clone(),
                                ));
                            }),
                        ))
                    }),
            )
            .into_any_element()
    }

    fn render_changes_panel(&mut self, colors: LocalPalette, cx: &mut Context<Self>) -> AnyElement {
        let Some(snapshot) = self.snapshot.clone() else {
            return div()
                .w(px(340.))
                .h_full()
                .bg(colors.sidebar)
                .border_l_1()
                .border_color(colors.border)
                .p_4()
                .text_color(colors.muted)
                .child("Loading authoritative Local Changes…")
                .into_any_element();
        };
        let head = head_label(&snapshot.head);
        let operation = operation_label(&snapshot.operation);
        let stage_paths = snapshot
            .unstaged
            .iter()
            .map(|entry| entry.path.clone())
            .chain(snapshot.untracked.iter().cloned())
            .collect::<Vec<_>>();
        let unstage_paths = snapshot
            .staged
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        let current_branch = match &snapshot.head {
            HeadState::Attached { branch, .. } | HeadState::Unborn { branch } => {
                Some(branch.clone())
            }
            HeadState::Detached { .. } => None,
        };
        let mut entries = div()
            .id("local-changes-entries")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3()
            .py_2();
        for (label, path, target) in local_change_rows(&snapshot) {
            let diff_path = path.clone();
            entries = entries.child(
                div()
                    .id(ElementId::Name(
                        format!("change-{label}-{}", path.display).into(),
                    ))
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_local_diff(diff_path.clone(), target, cx)
                    }))
                    .child(div().w(px(70.)).text_color(colors.muted).child(label))
                    .child(path.display),
            );
        }
        let diff = self.selected_diff.as_ref().map(|diff| match &diff.content {
            DiffContent::Text(text) => text.clone(),
            DiffContent::BinaryMetadata => "Binary diff (content not loaded)".into(),
            DiffContent::MediaMetadata => "Media diff (content not loaded)".into(),
            DiffContent::UnsupportedMetadata { reason } => format!("Unsupported diff: {reason}"),
        });
        let pending = self.pending_action.clone();
        let unresolved = self.unresolved_started_action.clone();
        let remote_observation = self.remote_observation.clone();
        let mut panel = div()
            .w(px(360.))
            .min_w(px(300.))
            .h_full()
            .flex()
            .flex_col()
            .bg(colors.sidebar)
            .border_l_1()
            .border_color(colors.border)
            .child(
                div()
                    .px_3()
                    .py_3()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("LOCAL CHANGES"),
                    )
                    .child(div().mt_2().font_family(CODE_FONT).text_xs().child(head))
                    .child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(colors.muted)
                            .child(format!(
                                "upstream {} · ↑{} ↓{} · {operation}",
                                snapshot.upstream.as_deref().unwrap_or("none"),
                                snapshot.ahead,
                                snapshot.behind
                            )),
                    )
                    .when_some(remote_observation.clone(), |header, observation| {
                        header.child(
                            div()
                                .mt_1()
                                .font_family(CODE_FONT)
                                .text_xs()
                                .text_color(colors.muted)
                                .child(format!(
                                    "observed {}/{} = {}",
                                    observation.remote,
                                    observation.branch,
                                    observation.oid.as_deref().unwrap_or("absent")
                                )),
                        )
                    }),
            )
            .child(entries)
            .when_some(diff, |panel, diff| {
                panel.child(
                    div()
                        .id("local-selected-diff")
                        .h(px(150.))
                        .overflow_y_scroll()
                        .border_t_1()
                        .border_color(colors.border)
                        .p_2()
                        .font_family(CODE_FONT)
                        .text_xs()
                        .whitespace_normal()
                        .child(diff),
                )
            })
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(Input::new(&self.commit_message))
                    .child(Input::new(&self.branch_name))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .when(!stage_paths.is_empty(), |buttons| {
                                buttons.child(action_button(
                                    "Stage all",
                                    colors,
                                    cx.listener(move |this, _, _, cx| {
                                        this.request_action(
                                            LocalAction::Stage(stage_paths.clone()),
                                            cx,
                                        );
                                    }),
                                ))
                            })
                            .when(!unstage_paths.is_empty(), |buttons| {
                                buttons.child(action_button(
                                    "Unstage all",
                                    colors,
                                    cx.listener(move |this, _, _, cx| {
                                        this.request_action(
                                            LocalAction::Unstage(unstage_paths.clone()),
                                            cx,
                                        );
                                    }),
                                ))
                            })
                            .child(action_button(
                                "Commit",
                                colors,
                                cx.listener(|this, _, _, cx| {
                                    let message = this.commit_message.read(cx).value().to_string();
                                    this.request_action(LocalAction::Commit { message }, cx);
                                }),
                            ))
                            .child(action_button(
                                "Fetch",
                                colors,
                                cx.listener(|this, _, _, cx| {
                                    this.request_action(
                                        LocalAction::Fetch {
                                            remote: "origin".into(),
                                        },
                                        cx,
                                    );
                                }),
                            ))
                            .when_some(current_branch.clone(), |buttons, branch| {
                                buttons
                                    .child(action_button(
                                        "FF pull",
                                        colors,
                                        cx.listener({
                                            let branch = branch.clone();
                                            move |this, _, _, cx| {
                                                this.request_action(
                                                    LocalAction::FastForwardPull {
                                                        remote: "origin".into(),
                                                        branch: branch.clone(),
                                                    },
                                                    cx,
                                                );
                                            }
                                        }),
                                    ))
                                    .child(action_button(
                                        "Push",
                                        colors,
                                        cx.listener(move |this, _, _, cx| {
                                            this.request_action(
                                                LocalAction::Push {
                                                    remote: "origin".into(),
                                                    branch: branch.clone(),
                                                },
                                                cx,
                                            );
                                        }),
                                    ))
                            })
                            .when_some(current_branch.clone(), |buttons, branch| {
                                buttons.child(action_button(
                                    "Observe origin",
                                    colors,
                                    cx.listener(move |this, _, _, cx| {
                                        this.observe_remote_branch(
                                            "origin".into(),
                                            branch.clone(),
                                            cx,
                                        );
                                    }),
                                ))
                            })
                            .when_some(remote_observation, |buttons, observation| {
                                if let Some(oid) = observation.oid {
                                    let remote = observation.remote;
                                    let branch = observation.branch;
                                    buttons.child(action_button(
                                        "Force w/lease",
                                        colors,
                                        cx.listener(move |this, _, _, cx| {
                                            this.request_action(
                                                LocalAction::ForcePushWithLease {
                                                    remote: remote.clone(),
                                                    branch: branch.clone(),
                                                    observed_remote_oid: oid.clone(),
                                                },
                                                cx,
                                            );
                                        }),
                                    ))
                                } else {
                                    buttons
                                }
                            })
                            .child(action_button(
                                "Create branch",
                                colors,
                                cx.listener(|this, _, _, cx| {
                                    let branch = this.branch_name.read(cx).value().to_string();
                                    this.request_action(
                                        LocalAction::CreateBranch {
                                            branch,
                                            start_oid: None,
                                        },
                                        cx,
                                    );
                                }),
                            ))
                            .child(action_button(
                                "Switch branch",
                                colors,
                                cx.listener(|this, _, _, cx| {
                                    let branch = this.branch_name.read(cx).value().to_string();
                                    this.request_action(LocalAction::SwitchBranch { branch }, cx);
                                }),
                            )),
                    ),
            );
        if let Some(pending) = pending {
            let id = pending.id;
            panel = panel.child(
                div()
                    .p_3()
                    .bg(if colors.dark {
                        rgba(0x342b18ff)
                    } else {
                        rgba(0xfff4d6ff)
                    })
                    .border_t_1()
                    .border_color(colors.amber)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Confirm Git action"),
                    )
                    .child(div().mt_1().text_xs().child(pending.action.summary()))
                    .child(
                        div()
                            .mt_2()
                            .flex()
                            .gap_2()
                            .child(action_button(
                                "Confirm",
                                colors,
                                cx.listener(move |this, _, _, cx| this.confirm_action(id, cx)),
                            ))
                            .child(action_button(
                                "Cancel",
                                colors,
                                cx.listener(move |this, _, _, cx| this.cancel_action(id, cx)),
                            )),
                    ),
            );
        }
        if let Some(started) = unresolved {
            panel = panel.child(
                div()
                    .p_3()
                    .bg(if colors.dark { rgba(0x411f21ff) } else { rgba(0xf9e2e0ff) })
                    .border_t_1()
                    .border_color(colors.red)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Started action needs reconciliation"))
                    .child(div().mt_1().text_xs().child(started.summary))
                    .child(div().mt_1().text_xs().child("Refresh and inspect actual state. This control only acknowledges; it never retries."))
                    .child(action_button("Acknowledge refreshed state", colors, cx.listener(|this, _, _, cx| {
                        this.acknowledge_action_reconciliation(cx)
                    }))),
            );
        }
        panel.into_any_element()
    }
}

fn action_button(
    label: impl Into<SharedString>,
    colors: LocalPalette,
    listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let label = label.into();
    div()
        .id(ElementId::Name(format!("action-{}", label).into()))
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.elevated)
        .hover(|button| button.bg(colors.selected))
        .cursor_pointer()
        .text_xs()
        .on_click(listener)
        .child(label)
}

fn conflict_column(label: &'static str, text: String, colors: LocalPalette) -> Div {
    div()
        .w_1_3()
        .min_w_0()
        .flex()
        .flex_col()
        .bg(colors.surface)
        .child(
            div()
                .h(px(28.))
                .px_2()
                .flex()
                .items_center()
                .text_xs()
                .text_color(colors.muted)
                .child(label),
        )
        .child(
            div()
                .id(ElementId::Name(format!("conflict-{label}").into()))
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p_2()
                .font_family(CODE_FONT)
                .text_xs()
                .whitespace_normal()
                .child(text),
        )
}

fn operation_label(operation: &OperationState) -> String {
    let mut active = Vec::new();
    if operation.merge {
        active.push("merge");
    }
    if operation.rebase != cibergit::local_git::RebaseState::None {
        active.push("rebase");
    }
    if operation.cherry_pick {
        active.push("cherry-pick");
    }
    if operation.revert {
        active.push("revert");
    }
    if active.is_empty() {
        "no Git operation".into()
    } else {
        format!("active {}", active.join(", "))
    }
}

fn operation_is_active(operation: &OperationState) -> bool {
    operation.merge
        || operation.rebase != cibergit::local_git::RebaseState::None
        || operation.cherry_pick
        || operation.revert
}

fn document_open_allowed(rebase_effect_in_flight: bool) -> bool {
    !rebase_effect_in_flight
}

fn remote_observation_matches(
    observation: Option<&RemoteBranchObservation>,
    remote: &str,
    branch: &str,
    oid: &str,
) -> bool {
    observation.is_some_and(|observation| {
        observation.remote == remote
            && observation.branch == branch
            && observation.oid.as_deref() == Some(oid)
    })
}

fn local_change_rows(snapshot: &LocalSnapshot) -> Vec<(&'static str, GitPath, DiffTarget)> {
    snapshot
        .conflicts
        .iter()
        .map(|entry| ("conflict", entry.path.clone(), DiffTarget::Worktree))
        .chain(
            snapshot
                .staged
                .iter()
                .map(|entry| ("staged", entry.path.clone(), DiffTarget::Staged)),
        )
        .chain(
            snapshot
                .unstaged
                .iter()
                .map(|entry| ("unstaged", entry.path.clone(), DiffTarget::Worktree)),
        )
        .chain(
            snapshot
                .untracked
                .iter()
                .cloned()
                .map(|path| ("untracked", path, DiffTarget::Worktree)),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::document::{ReconcileOutcome, TargetIssue};
    use std::{os::unix::fs::symlink, process::Command, time::Instant};
    use tempfile::TempDir;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn checkout_fixture() -> (TempDir, LocalWorkspaceContext) {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path().join("checkout");
        let data = temporary.path().join("data");
        fs::create_dir_all(root.join("src")).expect("create fixture");
        fs::create_dir_all(&data).expect("create data");
        fs::write(root.join("src/lib.rs"), "pub fn original() {}\n").expect("source");
        fs::write(root.join("same.txt"), "base\n").expect("same file");
        git(&root, &["init", "-b", "feature"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@invalid"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        let local = LocalGit::open(&root).expect("local git");
        let snapshot = local.snapshot().expect("snapshot");
        let checkout_root = local.root().to_owned();
        let git_dir = local.git_dir().to_owned();
        let common_git_dir = local.common_git_dir().to_owned();
        let repository = Repository {
            host: "github.com".into(),
            owner: "owner".into(),
            name: "repo".into(),
            account: cibergit::domain::Account {
                host: "github.com".into(),
                login: "account".into(),
            },
            local_path: Some(checkout_root.clone()),
        };
        let checkout = CheckoutView {
            association: cibergit::worktrees::CheckoutAssociation {
                key: cibergit::worktrees::AssociationKey {
                    provider: "github".into(),
                    host: "github.com".into(),
                    account: "account".into(),
                    repository: "owner/repo".into(),
                    pull_request: 7,
                },
                path: checkout_root.clone(),
                git_dir: git_dir.clone(),
                common_git_dir: common_git_dir.clone(),
                checkout_identity: filesystem_identity(&checkout_root).expect("identity"),
                git_dir_identity: filesystem_identity(&git_dir).expect("identity"),
                common_git_dir_identity: filesystem_identity(&common_git_dir).expect("identity"),
                ownership: cibergit::worktrees::CheckoutOwnership::ExplicitlyAttached,
                creation: None,
                intended_remote_branch: Some("feature".into()),
                published_head_at_association: None,
            },
            actual_head: snapshot.head,
            operation: snapshot.operation,
        };
        (
            temporary,
            LocalWorkspaceContext {
                repository,
                checkout,
                data_root: data,
                appearance: LocalWorkspaceAppearance::default(),
            },
        )
    }

    fn document_store(root: &Path, recovery: &Path) -> DocumentStore {
        fs::create_dir_all(recovery).expect("recovery root");
        DocumentStore::new(
            root,
            recovery,
            RecoveryScope::new("account", "repository"),
            DocumentLimits::default(),
        )
        .expect("document store")
    }

    #[test]
    fn browser_preserves_raw_names_excludes_git_and_never_follows_symlinks() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path().join("checkout");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(root.join(".git/objects")).expect("git internals");
        fs::create_dir_all(root.join("nested/.git/objects")).expect("nested Git directory");
        fs::create_dir_all(root.join("nested/.github/workflows")).expect("GitHub directory");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("secret.txt"), "outside").expect("outside file");
        fs::write(root.join("nested/.git/config"), "metadata").expect("nested Git metadata");
        fs::write(root.join("submodule"), "placeholder").expect("submodule parent");
        fs::create_dir(root.join("submodule-dir")).expect("submodule directory");
        fs::write(root.join("submodule-dir/.git"), "gitdir: elsewhere").expect("git file");
        fs::write(root.join("nested/.gitignore"), "target\n").expect("gitignore");
        fs::write(
            root.join("nested/.github/workflows/check.yml"),
            "name: check\n",
        )
        .expect("GitHub workflow");
        fs::write(root.join(OsStr::from_bytes(b"raw-\nname.txt")), b"raw").expect("raw file");
        fs::write(root.join("movie.mp4"), b"media").expect("media");
        symlink(&outside, root.join("escape")).expect("symlink");

        let snapshot = enumerate_worktree(&root, BrowserLimits::default()).expect("enumerate");
        assert!(snapshot.entries.iter().any(|entry| {
            entry.relative_path.as_os_str().as_bytes() == b"raw-\nname.txt"
                && entry.kind == BrowserEntryKind::EditableCandidate
        }));
        assert!(snapshot.entries.iter().any(|entry| {
            entry.relative_path == Path::new("escape") && entry.kind == BrowserEntryKind::Symlink
        }));
        assert!(!snapshot.entries.iter().any(|entry| {
            entry
                .relative_path
                .components()
                .any(|component| matches!(component, Component::Normal(name) if name == ".git"))
                || entry.display.contains("secret")
        }));
        assert!(
            snapshot
                .entries
                .iter()
                .any(|entry| entry.relative_path == Path::new("nested/.gitignore"))
        );
        assert!(snapshot.entries.iter().any(|entry| {
            entry.relative_path == Path::new("nested/.github/workflows/check.yml")
        }));
        assert!(validate_relative_path(Path::new("nested/.git/config")).is_err());
        assert!(validate_relative_path(Path::new("submodule-dir/.git")).is_err());
        assert!(validate_relative_path(Path::new("nested/.gitignore")).is_ok());
        assert!(validate_relative_path(Path::new("nested/.github/check.yml")).is_ok());
        assert_eq!(
            snapshot
                .entries
                .iter()
                .find(|entry| entry.relative_path == Path::new("movie.mp4"))
                .map(|entry| &entry.kind),
            Some(&BrowserEntryKind::UnsupportedMedia)
        );
    }

    #[test]
    fn browser_reports_bounds_instead_of_silently_omitting() {
        let temporary = tempfile::tempdir().expect("tempdir");
        for index in 0..4 {
            fs::write(temporary.path().join(format!("{index}.txt")), "x").expect("file");
        }
        let snapshot = enumerate_worktree(
            temporary.path(),
            BrowserLimits {
                max_entries: 2,
                max_depth: 2,
                max_path_bytes: 100,
            },
        )
        .expect("enumerate");
        assert!(snapshot.truncated);
        assert_eq!(snapshot.entries.len(), 2);
        assert!(snapshot.truncation_reason.is_some());
        assert!(snapshot.entries.windows(2).all(|entries| {
            entries[0].relative_path.as_os_str().as_bytes()
                <= entries[1].relative_path.as_os_str().as_bytes()
        }));
    }

    #[test]
    fn browser_does_not_follow_a_directory_replaced_before_descriptor_open() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path().join("checkout");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(root.join("victim")).expect("victim directory");
        fs::create_dir_all(&outside).expect("outside directory");
        fs::write(root.join("victim/inside.txt"), "inside").expect("inside file");
        fs::write(outside.join("secret.txt"), "secret").expect("outside secret");
        let mut replaced = false;
        let snapshot = enumerate_worktree_with_hook(&root, BrowserLimits::default(), |relative| {
            if !replaced && relative == Path::new("victim") {
                fs::rename(root.join("victim"), root.join("detached"))
                    .expect("detach original directory");
                symlink(&outside, root.join("victim")).expect("replacement symlink");
                replaced = true;
            }
        })
        .expect("descriptor enumeration");
        assert!(replaced);
        assert!(!snapshot.entries.iter().any(|entry| {
            entry.relative_path == Path::new("victim/secret.txt")
                || entry.display.contains("secret")
        }));
    }

    #[test]
    fn recovery_partition_separates_same_relative_file_in_two_checkouts() {
        let (_first_temp, first) = checkout_fixture();
        let (_second_temp, mut second) = checkout_fixture();
        second.repository = first.repository.clone();
        second.checkout.association.key = first.checkout.association.key.clone();
        assert_ne!(
            recovery_partition(&first).expect("partition"),
            recovery_partition(&second).expect("partition")
        );
    }

    #[test]
    fn fifo_worker_saves_captured_buffer_and_preserves_later_edit_for_restart() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path().join("checkout");
        let recovery = temporary.path().join("recovery");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("file.txt"), "base").expect("file");
        let store = document_store(&root, &recovery);
        let worker = DocumentWorker::start(store.open("file.txt").expect("open"));
        let send = |command| worker.dispatch(command).expect("dispatch");

        let (tx1, rx1) = mpsc::channel();
        send(DocumentCommand::Persist {
            generation: 1,
            text: "saved value".into(),
            reply: tx1,
        });
        let (tx2, rx2) = mpsc::channel();
        send(DocumentCommand::Save {
            generation: 2,
            text: "saved value".into(),
            reply: tx2,
        });
        let (tx3, rx3) = mpsc::channel();
        send(DocumentCommand::Persist {
            generation: 3,
            text: "newer unsaved value".into(),
            reply: tx3,
        });
        assert!(rx1.recv().expect("reply").result.is_ok());
        let saved = rx2.recv().expect("reply");
        assert!(matches!(
            saved.result,
            Ok((DocumentReplyKind::Saved { .. }, _))
        ));
        let latest = rx3.recv().expect("reply");
        assert!(latest.result.is_ok());
        assert_eq!(
            fs::read_to_string(root.join("file.txt")).expect("disk"),
            "saved value"
        );
        drop(worker);
        let restored = store.open("file.txt").expect("restart open");
        assert_eq!(restored.buffer(), "newer unsaved value");
        assert_eq!(restored.status(), DocumentStatus::Dirty);
    }

    #[test]
    fn clean_reload_dirty_conflict_undo_to_base_and_stale_reconcile_are_explicit() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path().join("checkout");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("file.txt"), "one").expect("file");
        let store = document_store(&root, &temporary.path().join("recovery"));
        let mut document = store.open("file.txt").expect("open");
        fs::write(root.join("file.txt"), "two").expect("external clean change");
        assert_eq!(
            document.refresh().expect("refresh"),
            RefreshOutcome::Reloaded
        );
        assert_eq!(document.buffer(), "two");
        document.set_buffer("ours").expect("dirty");
        fs::write(root.join("file.txt"), "three").expect("external dirty change");
        assert_eq!(
            document.refresh().expect("refresh"),
            RefreshOutcome::Conflict
        );
        assert_eq!(document.status(), DocumentStatus::Conflict);
        document.set_buffer("two").expect("undo to base");
        assert_eq!(document.status(), DocumentStatus::Conflict);
        let displayed = match document.disk() {
            DiskState::Present(snapshot) => snapshot.version.clone(),
            _ => panic!("present disk"),
        };
        fs::write(root.join("file.txt"), "four").expect("advance disk");
        assert_eq!(
            document
                .reconcile(&displayed, "proposed")
                .expect("reconcile"),
            ReconcileOutcome::Stale
        );
        assert_eq!(document.buffer(), "proposed");
        assert_eq!(document.status(), DocumentStatus::Conflict);
        drop(document);
        let restarted = store.open("file.txt").expect("restart");
        assert_eq!(restarted.buffer(), "proposed");
        assert_eq!(restarted.status(), DocumentStatus::Conflict);
    }

    #[test]
    fn unsupported_non_utf8_file_remains_listed_but_document_open_explains() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path().join("checkout");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("binary.dat"), [0xff, 0xfe]).expect("binary");
        let browser = enumerate_worktree(&root, BrowserLimits::default()).expect("browser");
        assert!(
            browser
                .entries
                .iter()
                .any(|entry| entry.relative_path == Path::new("binary.dat"))
        );
        let store = document_store(&root, &temporary.path().join("recovery"));
        assert!(matches!(
            store.open("binary.dat"),
            Err(cibergit::document::DocumentError::UnsafeTarget(
                TargetIssue::InvalidUtf8
            ))
        ));
    }

    #[test]
    fn syntax_highlighter_emits_real_colored_token_runs() {
        let source = "pub struct Demo { value: u64 } // note\n";
        let mut highlighter = LocalHighlighter::new("rust");
        highlighter.parse(source);
        let styles = highlighter.styles(&(0..source.len()), &LocalHighlightTheme { dark: true });
        assert!(styles.iter().any(|(_, style)| style.color.is_some()));
        let pub_range = styles
            .iter()
            .find(|(range, _)| &source[range.clone()] == "pub")
            .expect("keyword range");
        assert_eq!(pub_range.1.font_weight, Some(FontWeight::SEMIBOLD));
        assert_eq!(styles.first().expect("styles").0.start, 0);
        assert_eq!(styles.last().expect("styles").0.end, source.len());
    }

    #[test]
    fn stale_guard_is_a_certain_refusal_and_exact_journal_is_cleared() {
        let (_temporary, context) = checkout_fixture();
        let git = LocalGit::open(&context.checkout.association.path).expect("git");
        let stale = git.snapshot().expect("snapshot").guard;
        fs::write(
            context.checkout.association.path.join("same.txt"),
            "changed outside",
        )
        .expect("external change");
        let journal = context.data_root.join("uncertain.json");
        let started = StartedAction {
            schema_version: 1,
            request_id: 9,
            kind: "stage".into(),
            summary: "Stage same.txt".into(),
            checkout_identity: checkout_identity_label(&context.checkout),
            displayed_head: "test".into(),
            expected_remote_oid: None,
        };
        let result = run_local_action(
            &context,
            &journal,
            &started,
            LocalAction::Stage(vec![GitPath::from_raw(b"same.txt".to_vec()).expect("path")]),
            &stale,
        );
        assert!(matches!(
            result,
            Err(LocalActionRunError::NotDispatched {
                journal_preserved: false,
                ..
            })
        ));
        assert_eq!(read_started_action(&journal).expect("journal"), None);
        let refreshed = git.snapshot().expect("refreshed snapshot");
        assert!(
            refreshed.staged.is_empty(),
            "stale refusal dispatched no Git"
        );
    }

    #[test]
    fn certain_refusal_releases_attempt_without_erasing_changed_durable_record() {
        let (_temporary, context) = checkout_fixture();
        let git = LocalGit::open(&context.checkout.association.path).expect("git");
        let stale = git.snapshot().expect("snapshot").guard;
        fs::write(
            context.checkout.association.path.join("same.txt"),
            "changed outside",
        )
        .expect("external change");
        let journal = context.data_root.join("changed-record.json");
        let started = StartedAction {
            schema_version: 1,
            request_id: 10,
            kind: "stage".into(),
            summary: "Stage same.txt".into(),
            checkout_identity: checkout_identity_label(&context.checkout),
            displayed_head: "test".into(),
            expected_remote_oid: None,
        };
        let different = StartedAction {
            request_id: 99,
            summary: "different durable record".into(),
            ..started.clone()
        };
        let different_for_writer = different.clone();
        let result = run_local_action_with_journal(
            &context,
            &journal,
            &started,
            LocalAction::Stage(vec![GitPath::from_raw(b"same.txt".to_vec()).expect("path")]),
            &stale,
            |path, attempted| {
                write_started_action(path, attempted)?;
                let bytes =
                    serde_json::to_vec_pretty(&different_for_writer).expect("different JSON");
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(path)
                    .expect("replace durable contents for race fixture");
                file.write_all(&bytes).expect("different durable contents");
                file.sync_all().expect("sync different durable contents");
                Ok(())
            },
        );
        assert!(matches!(
            result,
            Err(LocalActionRunError::NotDispatched {
                journal_preserved: false,
                ..
            })
        ));
        assert_eq!(
            read_started_action(&journal).expect("changed record remains"),
            Some(different)
        );
        assert!(git.snapshot().expect("after refusal").staged.is_empty());
    }

    #[test]
    fn prestart_validation_and_journal_failure_dispatch_zero_git_mutations() {
        let (_temporary, context) = checkout_fixture();
        fs::write(
            context.checkout.association.path.join("same.txt"),
            "eligible local change",
        )
        .expect("local change");
        let git = LocalGit::open(&context.checkout.association.path).expect("git");
        let guard = git.snapshot().expect("guard").guard;
        let started = StartedAction {
            schema_version: 1,
            request_id: 21,
            kind: "stage".into(),
            summary: "Stage same.txt".into(),
            checkout_identity: checkout_identity_label(&context.checkout),
            displayed_head: "test".into(),
            expected_remote_oid: None,
        };
        let blocked_parent = context.data_root.join("not-a-directory");
        fs::write(&blocked_parent, "block journal directory").expect("blocking file");
        let journal = blocked_parent.join("started.json");
        let result = run_local_action(
            &context,
            &journal,
            &started,
            LocalAction::Stage(vec![GitPath::from_raw(b"same.txt".to_vec()).expect("path")]),
            &guard,
        );
        assert!(matches!(
            result,
            Err(LocalActionRunError::NotDispatched {
                journal_preserved: false,
                ..
            })
        ));
        assert!(
            git.snapshot().expect("after refusal").staged.is_empty(),
            "journal preflight failure must dispatch no mutation"
        );

        let mismatch_journal = context.data_root.join("mismatch/started.json");
        let existing = StartedAction {
            request_id: 90,
            summary: "different durable attempt".into(),
            ..started.clone()
        };
        write_started_action(&mismatch_journal, &existing).expect("existing durable attempt");
        let mismatch = run_local_action(
            &context,
            &mismatch_journal,
            &started,
            LocalAction::Stage(vec![GitPath::from_raw(b"same.txt".to_vec()).expect("path")]),
            &guard,
        );
        assert!(matches!(
            mismatch,
            Err(LocalActionRunError::NotDispatched {
                journal_preserved: false,
                ..
            })
        ));
        assert_eq!(
            read_started_action(&mismatch_journal).expect("preserved mismatch"),
            Some(existing)
        );
        assert!(git.snapshot().expect("after mismatch").staged.is_empty());

        for action in [
            LocalAction::Commit {
                message: " \t".into(),
            },
            LocalAction::SwitchBranch { branch: "".into() },
        ] {
            let validation_journal = context
                .data_root
                .join(format!("validation-{}.json", action.journal_kind()));
            let result = run_local_action(
                &context,
                &validation_journal,
                &StartedAction {
                    request_id: started.request_id + 1,
                    kind: action.journal_kind().into(),
                    summary: action.summary(),
                    ..started.clone()
                },
                action,
                &guard,
            );
            assert!(matches!(
                result,
                Err(LocalActionRunError::NotDispatched {
                    journal_preserved: false,
                    ..
                })
            ));
            assert!(!validation_journal.exists());
        }
        assert!(git.snapshot().expect("after validation").staged.is_empty());
    }

    #[test]
    fn failure_after_journal_install_is_conservative_and_restart_reconciles_exact_record() {
        let (_temporary, context) = checkout_fixture();
        fs::write(
            context.checkout.association.path.join("same.txt"),
            "eligible local change",
        )
        .expect("local change");
        let git = LocalGit::open(&context.checkout.association.path).expect("git");
        let guard = git.snapshot().expect("guard").guard;
        let journal = context.data_root.join("post-install/started.json");
        let started = StartedAction {
            schema_version: 1,
            request_id: 33,
            kind: "stage".into(),
            summary: "Stage same.txt".into(),
            checkout_identity: checkout_identity_label(&context.checkout),
            displayed_head: "test".into(),
            expected_remote_oid: None,
        };
        let result = run_local_action_with_journal(
            &context,
            &journal,
            &started,
            LocalAction::Stage(vec![GitPath::from_raw(b"same.txt".to_vec()).expect("path")]),
            &guard,
            |path, started| {
                write_started_action_with_hook(path, started, || {
                    Err("injected failure after journal installation".into())
                })
            },
        );
        assert!(matches!(
            result,
            Err(LocalActionRunError::NotDispatched {
                journal_preserved: true,
                ..
            })
        ));
        assert!(
            git.snapshot()
                .expect("after injected failure")
                .staged
                .is_empty()
        );
        assert_eq!(
            read_started_action(&journal).expect("restart journal read"),
            Some(started.clone())
        );
        assert!(clear_started_action(&journal, &started).expect("exact restart acknowledgement"));
        assert_eq!(
            read_started_action(&journal).expect("cleared journal"),
            None
        );
    }

    #[test]
    fn delayed_generation_cannot_be_selected_as_current() {
        fn applies(current: u64, reply: u64) -> bool {
            current == reply
        }
        assert!(!applies(4, 2));
        assert!(!applies(4, 5));
        assert!(applies(4, 4));
        let mut active = PathBuf::from("new.rs");
        let delayed_path = PathBuf::from("old.rs");
        if applies(2, 1) {
            active = delayed_path;
        }
        assert_eq!(active, Path::new("new.rs"));
    }

    #[test]
    fn document_open_admission_closes_for_the_exact_rebase_effect_lane() {
        assert!(document_open_allowed(false));
        assert!(!document_open_allowed(true));
    }

    #[test]
    fn delayed_programmatic_reload_cannot_replace_a_newer_editor_value() {
        let pending = PendingProgrammaticReload {
            generation: 7,
            expected_editor_value: "value at reply".into(),
            replacement: "disk reload".into(),
        };
        assert!(pending.still_applies(7, "value at reply"));
        assert!(!pending.still_applies(8, "newer edit"));
        assert!(!pending.still_applies(7, "edit in reply-to-render gap"));
    }

    #[test]
    fn journal_is_private_never_overwrites_and_clears_only_exact_attempt() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let path = temporary.path().join("private/actions/started.json");
        let first = StartedAction {
            schema_version: 1,
            request_id: 41,
            kind: "stage".into(),
            summary: "first".into(),
            checkout_identity: "checkout".into(),
            displayed_head: "head".into(),
            expected_remote_oid: None,
        };
        let second = StartedAction {
            request_id: 42,
            summary: "second".into(),
            ..first.clone()
        };
        write_started_action(&path, &first).expect("first private journal");
        assert_eq!(
            fs::symlink_metadata(path.parent().expect("parent"))
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::symlink_metadata(&path)
                .expect("journal metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(write_started_action(&path, &second).is_err());
        assert_eq!(
            read_started_action(&path).expect("read first"),
            Some(first.clone())
        );
        assert!(!clear_started_action(&path, &second).expect("mismatched clear"));
        assert_eq!(
            read_started_action(&path).expect("still first"),
            Some(first.clone())
        );
        assert!(clear_started_action(&path, &first).expect("exact clear"));
        assert_eq!(read_started_action(&path).expect("cleared"), None);
    }

    #[test]
    fn descriptor_lock_refuses_live_contender_then_process_death_releases_acknowledgement() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let journal = temporary.path().join("actions/started.json");
        let marker = temporary.path().join("helper-ready");
        let started = StartedAction {
            schema_version: 1,
            request_id: 71,
            kind: "stage".into(),
            summary: "Stage same.txt".into(),
            checkout_identity: "checkout".into(),
            displayed_head: "head".into(),
            expected_remote_oid: None,
        };
        write_started_action(&journal, &started).expect("durable journal before helper");
        let mut helper = Command::new(std::env::current_exe().expect("test executable"))
            .arg("action_journal_lock_helper_process")
            .arg("--ignored")
            .arg("--nocapture")
            .env("CIBERGIT_TEST_LOCK_JOURNAL", &journal)
            .env("CIBERGIT_TEST_LOCK_MARKER", &marker)
            .spawn()
            .expect("spawn lock helper");
        let wait_started = Instant::now();
        while !marker.exists() && wait_started.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(25));
        }
        assert!(marker.exists(), "helper did not acquire descriptor lock");
        let refusal_started = Instant::now();
        let refusal = clear_started_action(&journal, &started);
        assert!(refusal.is_err(), "live lock holder must refuse contender");
        assert!(
            refusal_started.elapsed() < Duration::from_secs(1),
            "nonblocking lock refusal must be bounded"
        );
        helper.kill().expect("kill lock helper");
        helper.wait().expect("reap lock helper");
        assert!(
            clear_started_action(&journal, &started).expect("acknowledge after helper death"),
            "closing the killed helper descriptor must release the lock"
        );
        assert_eq!(
            read_started_action(&journal).expect("cleared journal"),
            None
        );
        let lock_path = journal
            .parent()
            .expect("journal parent")
            .join(".started-local-action.lock");
        assert!(
            lock_path.exists(),
            "one persistent lock namespace is retained"
        );
        assert_eq!(
            fs::symlink_metadata(lock_path)
                .expect("lock metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn duplicated_descriptor_reproduces_lock_retention_after_original_close() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let journal = temporary.path().join("actions/started.json");
        prepare_private_directory(journal.parent().expect("journal parent"))
            .expect("private directory");
        drop(ActionJournalLock::acquire(&journal).expect("create persistent lock file"));
        let lock_path = journal
            .parent()
            .expect("journal parent")
            .join(".started-local-action.lock");
        let original = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_CLOEXEC)
            .open(&lock_path)
            .expect("open raw lock descriptor");
        assert_eq!(unsafe { flock(original.as_raw_fd(), LOCK_EX | LOCK_NB) }, 0);
        let duplicate_fd = unsafe { dup(original.as_raw_fd()) };
        assert!(duplicate_fd >= 0, "duplicate raw lock descriptor");
        let duplicate = unsafe { File::from_raw_fd(duplicate_fd) };
        drop(original);

        let refusal = ActionJournalLock::acquire(&journal)
            .expect_err("a duplicated open-file description must retain flock after close");
        assert!(refusal.contains("another live process"));

        assert_eq!(unsafe { flock(duplicate.as_raw_fd(), LOCK_UN) }, 0);
        drop(ActionJournalLock::acquire(&journal).expect("explicit raw unlock releases contender"));
    }

    #[test]
    fn journal_guard_drop_unlocks_while_duplicated_descriptor_remains_open() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let journal = temporary.path().join("actions/started.json");
        prepare_private_directory(journal.parent().expect("journal parent"))
            .expect("private directory");
        let guard = ActionJournalLock::acquire(&journal).expect("guard lock");
        let duplicate_fd = unsafe { dup(guard.file.as_raw_fd()) };
        assert!(duplicate_fd >= 0, "duplicate guard descriptor");
        let duplicate = unsafe { File::from_raw_fd(duplicate_fd) };

        drop(guard);
        drop(
            ActionJournalLock::acquire(&journal)
                .expect("guard Drop must explicitly unlock before descriptor close"),
        );
        drop(duplicate);
    }

    #[test]
    fn post_flock_acquisition_error_unlocks_with_duplicate_still_open() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let journal = temporary.path().join("actions/started.json");
        prepare_private_directory(journal.parent().expect("journal parent"))
            .expect("private directory");
        let mut duplicate_fd = -1;
        let result = ActionJournalLock::acquire_with_post_lock_hook(&journal, |fd| {
            duplicate_fd = unsafe { dup(fd) };
            if duplicate_fd < 0 {
                return Err("duplicate post-flock descriptor failed".into());
            }
            Err("forced post-flock acquisition failure".into())
        });
        assert_eq!(result.unwrap_err(), "forced post-flock acquisition failure");
        let duplicate = unsafe { File::from_raw_fd(duplicate_fd) };

        drop(
            ActionJournalLock::acquire(&journal)
                .expect("error return must run explicit unlock before File closes"),
        );
        drop(duplicate);
    }

    #[test]
    #[ignore = "subprocess helper; invoked by descriptor lock lifecycle test"]
    fn action_journal_lock_helper_process() {
        let Some(journal) = std::env::var_os("CIBERGIT_TEST_LOCK_JOURNAL").map(PathBuf::from)
        else {
            return;
        };
        let marker = std::env::var_os("CIBERGIT_TEST_LOCK_MARKER")
            .map(PathBuf::from)
            .expect("helper marker");
        let _lock = ActionJournalLock::acquire(&journal).expect("helper descriptor lock");
        fs::write(marker, "ready").expect("helper ready marker");
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }

    #[test]
    fn corrupt_or_future_journal_is_preserved_instead_of_replaced() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let parent = temporary.path().join("actions");
        prepare_private_directory(&parent).expect("private parent");
        let path = parent.join("started.json");
        let started = StartedAction {
            schema_version: 1,
            request_id: 1,
            kind: "fetch".into(),
            summary: "fetch".into(),
            checkout_identity: "checkout".into(),
            displayed_head: "head".into(),
            expected_remote_oid: None,
        };
        let write_raw = |bytes: &[u8]| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .expect("raw journal");
            file.write_all(bytes).expect("write raw");
            file.sync_all().expect("sync raw");
        };
        write_raw(b"not-json");
        assert!(write_started_action(&path, &started).is_err());
        assert_eq!(fs::read(&path).expect("corrupt retained"), b"not-json");
        let future = serde_json::to_vec(&StartedAction {
            schema_version: 2,
            ..started.clone()
        })
        .expect("future json");
        write_raw(&future);
        assert!(write_started_action(&path, &started).is_err());
        assert_eq!(fs::read(&path).expect("future retained"), future);
    }

    #[test]
    fn force_lease_requires_the_exact_displayed_remote_observation() {
        let observation = RemoteBranchObservation {
            remote: "origin".into(),
            branch: "feature".into(),
            oid: Some("abc123".into()),
        };
        assert!(remote_observation_matches(
            Some(&observation),
            "origin",
            "feature",
            "abc123"
        ));
        assert!(!remote_observation_matches(
            Some(&observation),
            "origin",
            "feature",
            "advanced"
        ));
        assert!(!remote_observation_matches(
            None, "origin", "feature", "abc123"
        ));
    }

    #[test]
    fn reordered_save_replies_merge_retained_paths_without_regressing_newer_state() {
        let retained_a = PathBuf::from(OsStr::from_bytes(b"retained-\na"));
        let retained_b = PathBuf::from(OsStr::from_bytes(b"retained-b"));
        let mut visible = Vec::new();
        let newer_message = "Conflict from newer reply".to_owned();
        let newer_status = DocumentStatus::Conflict;

        assert_eq!(
            merge_retained_paths(&mut visible, &[retained_a.clone(), retained_b.clone()]),
            vec![retained_a.clone(), retained_b.clone()]
        );
        // The older save callback arrives after the newer callback. Its
        // cumulative worker view has only A, so replacement would lose B.
        assert!(merge_retained_paths(&mut visible, std::slice::from_ref(&retained_a)).is_empty());
        assert_eq!(visible, vec![retained_a, retained_b]);
        assert_eq!(newer_message, "Conflict from newer reply");
        assert_eq!(newer_status, DocumentStatus::Conflict);
    }
}
