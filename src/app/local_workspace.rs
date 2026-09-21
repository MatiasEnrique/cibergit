//! Embeddable, checkout-scoped Local Changes surface.
//!
//! This component deliberately has no `ReviewSession` or provider dependency. The
//! parent keeps the published review pinned and routes an explicit "Local changes"
//! gesture here. All checkout and Git I/O is started on GPUI's background executor.
//! Worktree files are never opened for editing here; use an external editor.

use cibergit::ui::{self, Density, TextRole};
#[path = "local_workspace/conflict_view.rs"]
mod conflict_view;
#[path = "local_workspace/operation_lifecycle.rs"]
mod operation_lifecycle;
#[path = "local_workspace/pr_publish.rs"]
mod pr_publish;
#[path = "local_workspace/rebase_panel.rs"]
mod rebase_panel;

use operation_lifecycle::{ObservationKind, OperationLifecycle};
use pr_publish::PrPublishAttempt;
pub use pr_publish::{PrPublishContext, PrPublishMode, PrPublishPreparation};

use cibergit::{
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
    AnyElement, App, ClickEvent, Context, Div, ElementId, Entity, EventEmitter, KeyDownEvent,
    Render, Rgba, SharedString, Subscription, Window, WindowAppearance, actions, div, prelude::*,
    px, rgba,
};
use gpui_base::Button;
use gpui_base::input::{Editor, EditorState, Input, InputEditorStyle, InputState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CString, c_char, c_int},
    fs::{self, File},
    io::Write,
    ops::Range,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

const CODE_FONT: &str = "Menlo";
/// Width of the Local Changes status column. Fixed so every path in the list
/// starts on the same x position; sized for the longest status word.
const CHANGE_STATUS_COLUMN: f32 = 70.;
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

unsafe extern "C" {
    fn openat(fd: c_int, path: *const c_char, oflag: c_int, ...) -> c_int;
    fn flock(fd: c_int, operation: c_int) -> c_int;
    fn dup(fd: c_int) -> c_int;
    fn __error() -> *mut c_int;
}

actions!(local_workspace, [LocalRefresh, LocalConfirm, LocalCancel]);

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

#[derive(Clone, Debug)]
pub enum LocalWorkspaceEvent {
    LocalSnapshotChanged,
    RemoteBranchObserved(RemoteBranchObservation),
    MaterialActionConfirmationRequested { request_id: u64, summary: String },
    LocalActionFinished { request_id: u64, result: String },
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
    /// Immutable explicit PR-source preparation. This is constructed only by
    /// the fresh provider/Git identity pipeline, never by branch-name inference.
    PublishPrSource {
        preparation: Box<PrPublishPreparation>,
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

    fn requires_exclusive_checkout_lane(&self) -> bool {
        self.changes_checkout() || matches!(self, Self::PublishPrSource { .. })
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
            Self::PublishPrSource { preparation } => preparation.summary(),
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
            Self::PublishPrSource { preparation } => match preparation.mode {
                PrPublishMode::UpToDate => "published-pr-source-noop",
                PrPublishMode::Publish => "publish-pr-source",
                PrPublishMode::RepublishWithLease => "republish-pr-source-with-lease",
            },
            Self::CreateBranch { .. } => "create-branch",
            Self::SwitchBranch { .. } => "switch-branch",
        }
    }
}

#[derive(Clone, Debug)]
struct PrPublishReconciliationEvidence {
    attempt: PrPublishAttempt,
    reconciliation: pr_publish::PrPublishReconciliation,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pr_publish: Option<PrPublishAttempt>,
}

struct Backend {
    git: LocalGit,
    rebase: RebaseStore,
    journal_path: PathBuf,
}

enum BackendState {
    Loading,
    Ready(Box<Backend>),
    Failed(String),
}

pub struct LocalWorkspace {
    context: LocalWorkspaceContext,
    backend: BackendState,
    operations: OperationLifecycle,
    snapshot: Option<LocalSnapshot>,
    selected_diff: Option<SelectedDiff>,
    pr_publish_reconciliation: Option<PrPublishReconciliationEvidence>,
    pr_publish_context: Option<PrPublishContext>,
    pr_publish_preparation: Option<PrPublishPreparation>,
    pr_publish_notice: String,
    pr_publish_details_expanded: bool,
    local_actions_scroll: gpui::ScrollHandle,
    commit_message: Entity<InputState>,
    branch_name: Entity<InputState>,
    rebase: rebase_panel::RebasePanel,
    status: String,
    focused: bool,
    _subscriptions: Vec<Subscription>,
}

impl LocalWorkspace {
    /// Cheap construction only. `LocalGit::open` and the first snapshot both
    /// run after Loading is visible.
    pub fn new(
        context: LocalWorkspaceContext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let colors = palette(context.appearance.dark);
        let commit_message = new_input("Commit message", colors, window, cx);
        let branch_name = new_input("Branch name", colors, window, cx);
        let rebase = rebase_panel::RebasePanel::new(colors, window, cx);
        let mut this = Self {
            context,
            backend: BackendState::Loading,
            operations: OperationLifecycle::default(),
            snapshot: None,
            selected_diff: None,
            pr_publish_reconciliation: None,
            pr_publish_context: None,
            pr_publish_preparation: None,
            pr_publish_notice: String::new(),
            pr_publish_details_expanded: false,
            local_actions_scroll: gpui::ScrollHandle::new(),
            commit_message,
            branch_name,
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
            this.rebase
                .update_appearance(palette(this.context.appearance.dark), cx);
            cx.notify();
        });
        this._subscriptions.extend([activation, appearance]);
        this.start_initialization(window, cx);
        this.start_polling(cx);
        this
    }

    pub fn local_snapshot(&self) -> Option<&LocalSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn remote_observation(&self) -> Option<&RemoteBranchObservation> {
        self.operations.remote_observation()
    }

    pub fn status_message(&self) -> &str {
        &self.status
    }

    pub fn in_flight_action_id(&self) -> Option<u64> {
        self.operations.local_in_flight_id()
    }

    /// Attach or clear explicit PR publication identity without changing the
    /// stable LocalWorkspace constructor used by non-PR callsites.
    pub fn set_pr_publish_context(
        &mut self,
        context: Option<PrPublishContext>,
        cx: &mut Context<Self>,
    ) {
        if !self.operations.idle() {
            self.report_error(
                "PR publication identity cannot change while confirmation, dispatch, or durable reconciliation is active"
                    .into(),
                cx,
            );
            return;
        }
        if let Ok(ticket) = self
            .operations
            .begin_observation(ObservationKind::PrPublish)
        {
            let _ = self.operations.finish_observation(ticket);
        }
        self.pr_publish_reconciliation = None;
        self.pr_publish_context = context;
        self.pr_publish_preparation = None;
        self.pr_publish_notice = if self.pr_publish_context.is_some() {
            "Check the PR’s source branch before publishing local commits.".into()
        } else {
            String::new()
        };
        cx.notify();
    }

    pub fn pr_publish_preparation(&self) -> Option<&PrPublishPreparation> {
        self.pr_publish_preparation.as_ref()
    }

    #[cfg(feature = "ui-smoke")]
    pub fn smoke_scroll_local_actions_to_end(&self) -> f32 {
        let maximum = self.local_actions_scroll.max_offset().y;
        self.local_actions_scroll
            .set_offset(gpui::point(px(0.), -maximum));
        maximum.as_f32()
    }

    pub fn toggle_pr_publish_details(&mut self, cx: &mut Context<Self>) {
        self.pr_publish_details_expanded = !self.pr_publish_details_expanded;
        cx.notify();
    }

    pub fn pr_publish_notice(&self) -> &str {
        &self.pr_publish_notice
    }

    /// Explicit read-only preparation. It freezes provider, checkout, local
    /// branch/OID, effective push endpoint, destination branch/OID, and mode.
    pub fn prepare_pr_publish(&mut self, cx: &mut Context<Self>) {
        let Some(context) = self.pr_publish_context.clone() else {
            self.report_error(
                "This workspace has no selected PR publication identity".into(),
                cx,
            );
            return;
        };
        let BackendState::Ready(backend) = &self.backend else {
            self.report_error("Local workspace is not ready".into(), cx);
            return;
        };
        if !self.operations.idle()
            || self
                .operations
                .observation_pending(ObservationKind::PrPublish)
        {
            self.report_error(
                "PR publication preparation is paused by an active confirmation, action, reconciliation, or rebase transition"
                    .into(),
                cx,
            );
            return;
        }
        let ticket = match self
            .operations
            .begin_observation(ObservationKind::PrPublish)
        {
            Ok(ticket) => ticket,
            Err(error) => {
                self.report_error(error.into(), cx);
                return;
            }
        };
        self.pr_publish_reconciliation = None;
        self.pr_publish_preparation = None;
        self.pr_publish_notice = "Checking the PR source and Git destination…".into();
        let checkout = self.context.checkout.clone();
        let git = backend.git.clone();
        let task =
            cx.background_spawn(async move { pr_publish::prepare(&context, &checkout, &git) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if !this.operations.finish_observation(ticket) {
                    return;
                }
                match result {
                    Ok(preparation) => {
                        this.pr_publish_notice = format!(
                            "{} is ready. Review the local and remote branches below.",
                            preparation.mode.label()
                        );
                        this.pr_publish_preparation = Some(preparation);
                    }
                    Err(error) => {
                        this.pr_publish_preparation = None;
                        this.pr_publish_notice = format!(
                            "PR publication unavailable: {error}. No Git mutation was started."
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn request_prepared_pr_publish(&mut self, cx: &mut Context<Self>) -> Option<u64> {
        let Some(preparation) = self.pr_publish_preparation.clone() else {
            self.report_error(
                "Prepare and inspect the fresh PR source target first".into(),
                cx,
            );
            return None;
        };
        if preparation.mode == PrPublishMode::UpToDate {
            self.report_error(
                "The configured PR source branch already has the attached local OID; no push is needed"
                    .into(),
                cx,
            );
            return None;
        }
        self.request_action(
            LocalAction::PublishPrSource {
                preparation: Box::new(preparation),
            },
            cx,
        )
    }

    fn refresh_pr_publish_after_attempt(&mut self, cx: &mut Context<Self>) {
        let (Some(context), BackendState::Ready(backend), Some(attempt)) = (
            self.pr_publish_context.clone(),
            &self.backend,
            self.operations
                .recovery()
                .and_then(|started| started.pr_publish.clone()),
        ) else {
            return;
        };
        let Ok(ticket) = self
            .operations
            .begin_observation(ObservationKind::PrPublish)
        else {
            return;
        };
        self.pr_publish_reconciliation = None;
        self.pr_publish_preparation = None;
        self.pr_publish_notice =
            "Reconciling fresh provider source and effective push endpoint read-only…".into();
        let checkout = self.context.checkout.clone();
        let git = backend.git.clone();
        let expected_attempt = attempt.clone();
        let task = cx.background_spawn(async move {
            pr_publish::reconcile_attempt(&context, &checkout, &git, &attempt)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .operations
                    .recovery()
                    .and_then(|started| started.pr_publish.as_ref())
                    != Some(&expected_attempt)
                {
                    let _ = this.operations.finish_observation(ticket);
                    this.pr_publish_notice =
                        "A newer durable PR publication attempt replaced this reconciliation; the stale read was ignored"
                            .into();
                    cx.notify();
                    return;
                }
                match result {
                    Ok(reconciliation) => {
                        if !this
                            .operations
                            .finish_publish_reconciliation(ticket, &expected_attempt)
                        {
                            return;
                        }
                        this.pr_publish_notice = format!(
                            "Read-only reconciliation: {}",
                            reconciliation.summary(&expected_attempt)
                        );
                        this.pr_publish_reconciliation = Some(PrPublishReconciliationEvidence {
                            attempt: expected_attempt,
                            reconciliation,
                        });
                    }
                    Err(error) => {
                        if !this.operations.finish_observation(ticket) {
                            return;
                        }
                        this.pr_publish_reconciliation = None;
                        this.pr_publish_notice = format!(
                            "Read-only PR publication reconciliation is incomplete: {error}. No action was replayed."
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
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
        let ticket = match self
            .operations
            .begin_observation(ObservationKind::RemoteBranch)
        {
            Ok(ticket) => ticket,
            Err(error) => {
                self.report_error(error.into(), cx);
                return;
            }
        };
        let git = backend.git.clone();
        let context = self.context.clone();
        let task = cx.background_spawn(async move {
            validate_checkout(&context.checkout, &git)?;
            git.observe_remote_branch(&remote, &branch)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(observation) => {
                    if !this
                        .operations
                        .install_remote_observation(ticket, observation.clone())
                    {
                        return;
                    }
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
                    cx.emit(LocalWorkspaceEvent::RemoteBranchObserved(observation));
                    cx.notify();
                }
                Err(error) => {
                    if this.operations.finish_observation(ticket) {
                        this.report_error(
                            format!("Remote observation failed; no force lease was armed: {error}"),
                            cx,
                        );
                    }
                }
            });
        })
        .detach();
    }

    /// Selects the local diff for a worktree-relative path, when that path is
    /// one of the files Git currently reports as changed. A path with no local
    /// change has no diff to show, so the selection is left untouched.
    pub fn select_relative_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        let raw = path.as_os_str().as_bytes();
        let Some((_, git_path, target)) = self
            .snapshot
            .as_ref()
            .map(local_change_rows)
            .unwrap_or_default()
            .into_iter()
            .find(|(_, candidate, _)| candidate.raw == raw)
        else {
            return;
        };
        self.select_local_diff(git_path, target, cx);
    }

    pub fn select_local_diff(&mut self, path: GitPath, target: DiffTarget, cx: &mut Context<Self>) {
        let BackendState::Ready(backend) = &self.backend else {
            return;
        };
        let Ok(ticket) = self.operations.begin_observation(ObservationKind::Diff) else {
            return;
        };
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
                if !this.operations.finish_observation(ticket) {
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

    pub fn is_ready(&self) -> bool {
        matches!(self.backend, BackendState::Ready(_))
    }

    pub fn request_action(&mut self, action: LocalAction, cx: &mut Context<Self>) -> Option<u64> {
        let BackendState::Ready(_) = &self.backend else {
            self.report_error("Local workspace is not ready".into(), cx);
            return None;
        };
        if let Err(error) = validate_local_action_input(&action) {
            self.report_error(format!("{error}; Git was not started"), cx);
            return None;
        }
        let Some(snapshot) = &self.snapshot else {
            self.report_error("Refresh Local Changes before acting".into(), cx);
            return None;
        };
        let guard = match &action {
            LocalAction::PublishPrSource { preparation } => {
                if self.pr_publish_context.is_none() {
                    self.report_error(
                        "The selected PR publication identity is no longer attached".into(),
                        cx,
                    );
                    return None;
                }
                if &snapshot.guard != preparation.snapshot_guard() {
                    self.pr_publish_preparation = None;
                    self.report_error(
                        "Local Git state changed after publication preparation; prepare again"
                            .into(),
                        cx,
                    );
                    return None;
                }
                preparation.snapshot_guard().clone()
            }
            _ => snapshot.guard.clone(),
        };
        let summary = action.summary();
        let id = match self.operations.admit_local(action, guard) {
            Ok(id) => id,
            Err(error) => {
                self.report_error(error, cx);
                return None;
            }
        };
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
        let BackendState::Ready(backend) = &self.backend else {
            self.report_error("Local workspace is not ready".into(), cx);
            return;
        };
        let dispatch = match self.operations.confirm_local(request_id) {
            Ok(dispatch) => dispatch,
            Err(error) => {
                self.report_error(error, cx);
                return;
            }
        };
        let context = self.context.clone();
        let journal_path = backend.journal_path.clone();
        self.pr_publish_reconciliation = None;
        self.status = format!("Checking and recording {}…", dispatch.started.summary);
        let ticket = dispatch.ticket;
        let action = dispatch.intent.action;
        let guard = dispatch.intent.guard;
        let started = dispatch.started;
        let attempted = started.clone();
        let task = cx.background_spawn(async move {
            run_local_action(&context, &journal_path, &attempted, action, &guard)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((receipt, snapshot)) => {
                        if !this.operations.complete_local_success(ticket, &snapshot) {
                            return;
                        }
                        this.snapshot = Some(snapshot);
                        this.pr_publish_reconciliation = None;
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
                        if started.pr_publish.is_some() {
                            this.prepare_pr_publish(cx);
                        }
                    }
                    Err(LocalActionRunError::NotDispatched {
                        error,
                        journal_preserved: false,
                    }) => {
                        if !this.operations.complete_local_retry(ticket) {
                            return;
                        }
                        this.pr_publish_reconciliation = None;
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
                        let is_pr_publish = started.pr_publish.is_some();
                        if !this.operations.complete_local_recovery(
                            ticket,
                            started,
                            false,
                            is_pr_publish,
                        ) {
                            return;
                        }
                        this.pr_publish_reconciliation = None;
                        this.status = format!(
                            "Git was not started, but its durable intent record was preserved: {error}. Reconcile that exact record before retrying."
                        );
                        cx.emit(LocalWorkspaceEvent::Error(this.status.clone()));
                        if is_pr_publish {
                            this.refresh_pr_publish_after_attempt(cx);
                        }
                        cx.notify();
                    }
                    Err(LocalActionRunError::StartedOrUncertain(error)) => {
                        let is_pr_publish = started.pr_publish.is_some();
                        if !this.operations.complete_local_recovery(
                            ticket,
                            started,
                            true,
                            is_pr_publish,
                        ) {
                            return;
                        }
                        this.pr_publish_reconciliation = None;
                        this.status = format!(
                            "Action may have started: {error}. Authoritative refresh and explicit reconciliation are required before retry."
                        );
                        cx.emit(LocalWorkspaceEvent::Error(this.status.clone()));
                        this.refresh_git(cx);
                        if is_pr_publish {
                            this.refresh_pr_publish_after_attempt(cx);
                        }
                    }
                }
            });
        })
        .detach();
    }

    pub fn cancel_action(&mut self, request_id: u64, cx: &mut Context<Self>) {
        if self.operations.cancel_local(request_id) {
            self.status = "Local action cancelled; Git was not started".into();
            cx.notify();
        } else {
            self.report_error("That cancellation is stale".into(), cx);
        }
    }

    /// Clears restart uncertainty only after the refreshed state has been
    /// inspected. It never retries the action.
    pub fn acknowledge_action_reconciliation(&mut self, cx: &mut Context<Self>) {
        if let Some(attempt) = self
            .operations
            .recovery()
            .and_then(|started| started.pr_publish.as_ref())
        {
            let evidence_matches = self
                .pr_publish_reconciliation
                .as_ref()
                .is_some_and(|evidence| &evidence.attempt == attempt);
            if !evidence_matches {
                self.report_error(
                    "The exact durable PR publication attempt lacks current read-only reconciliation evidence"
                        .into(),
                    cx,
                );
                return;
            }
        }
        let BackendState::Ready(backend) = &self.backend else {
            return;
        };
        let dispatch = match self.operations.begin_reconciliation() {
            Ok(dispatch) => dispatch,
            Err(error) => {
                self.report_error(error, cx);
                return;
            }
        };
        let expected = dispatch.started;
        let ticket = dispatch.ticket;
        let reconciliation_summary = self
            .pr_publish_reconciliation
            .as_ref()
            .map(|evidence| evidence.reconciliation.summary(&evidence.attempt));
        let journal = backend.journal_path.clone();
        let expected_for_clear = expected.clone();
        let task =
            cx.background_spawn(async move { clear_started_action(&journal, &expected_for_clear) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                Ok(true) => {
                    if !this.operations.complete_reconciliation(ticket, true) {
                        return;
                    }
                    this.pr_publish_reconciliation = None;
                    this.status = reconciliation_summary.as_ref().map_or_else(
                        || "Local action reconciled; no action was retried".into(),
                        |summary| {
                            format!(
                                "Observed current state acknowledged; no action was retried. {summary}"
                            )
                        },
                    );
                    cx.notify();
                }
                Ok(false) => {
                    if this.operations.complete_reconciliation(ticket, false) {
                        this.report_error(
                            "The durable action record changed; it was preserved for reconciliation".into(),
                            cx,
                        );
                    }
                }
                Err(error) => {
                    if this.operations.complete_reconciliation(ticket, false) {
                        this.report_error(
                            format!("Cannot persist action reconciliation: {error}"),
                            cx,
                        )
                    }
                }
                }
            });
        })
        .detach();
    }

    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.refresh_git(cx);
        self.observe_rebase(cx);
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
                            let needs_publish_reconciliation = started
                                .as_ref()
                                .is_some_and(|started| started.pr_publish.is_some());
                            this.operations.activate(
                                1,
                                checkout_identity_label(&this.context.checkout),
                                &snapshot,
                                started,
                            );
                            this.snapshot = Some(snapshot);
                            this.pr_publish_reconciliation = None;
                            this.status = if this.operations.recovery().is_some() {
                                "A previously started local action requires authoritative reconciliation"
                                    .into()
                            } else {
                                "Local workspace ready · Review remains pinned and read-only".into()
                            };
                            this.backend = BackendState::Ready(Box::new(backend));
                            this.rebase.install_observed(rebase_operation);
                            cx.emit(LocalWorkspaceEvent::LocalSnapshotChanged);
                            cx.notify();
                            if needs_publish_reconciliation {
                                this.refresh_pr_publish_after_attempt(cx);
                            }
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

    fn refresh_git(&mut self, cx: &mut Context<Self>) {
        let BackendState::Ready(backend) = &self.backend else {
            return;
        };
        let Ok(ticket) = self.operations.begin_observation(ObservationKind::Snapshot) else {
            return;
        };
        let context = self.context.clone();
        let git = backend.git.clone();
        let task = cx.background_spawn(async move {
            validate_checkout(&context.checkout, &git)?;
            git.snapshot().map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(snapshot) => {
                        if !this.operations.install_snapshot_observation(ticket, &snapshot) {
                            return;
                        }
                        let head_changed = this
                            .snapshot
                            .as_ref()
                            .is_some_and(|previous| previous.head != snapshot.head);
                        let publish_stale = this.operations.pending_local().is_none()
                            && this.pr_publish_preparation.as_ref().is_some_and(|preparation| {
                                preparation.snapshot_guard() != &snapshot.guard
                            });
                        if publish_stale {
                            this.pr_publish_preparation = None;
                            this.pr_publish_notice = "Local Git state changed; prepare the PR source target again before publishing."
                                .into();
                        }
                        this.snapshot = Some(snapshot);
                        if head_changed {
                            this.status =
                                "External branch/HEAD change observed; Local Changes refreshed"
                                    .into();
                        }
                        cx.emit(LocalWorkspaceEvent::LocalSnapshotChanged);
                        cx.notify();
                    }
                    Err(error) => {
                        if this.operations.finish_observation(ticket) {
                            this.report_error(
                                format!("Local Changes refresh failed: {error}"),
                                cx,
                            )
                        }
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
}

fn selected_paths_summary(action: &str, paths: &[GitPath]) -> String {
    if let [path] = paths {
        format!("{action} {}", path.display)
    } else {
        format!("{action} {} selected files", paths.len())
    }
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
    let action_root = context
        .data_root
        .join("local-workspace")
        .join("actions")
        .join(checkout_partition(context)?);
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
            git,
            rebase,
            journal_path,
        },
        snapshot,
        started,
        rebase_operation,
    ))
}

fn checkout_partition(context: &LocalWorkspaceContext) -> Result<String, String> {
    let association = &context.checkout.association;
    let checkout = fs::canonicalize(&association.path)
        .map_err(|error| format!("canonicalize checkout for its partition key: {error}"))?;
    let git_dir = fs::canonicalize(&association.git_dir)
        .map_err(|error| format!("canonicalize Git directory for its partition key: {error}"))?;
    let common = fs::canonicalize(&association.common_git_dir).map_err(|error| {
        format!("canonicalize common Git directory for its partition key: {error}")
    })?;
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
        LocalAction::PublishPrSource { preparation }
            if preparation.local_branch.trim().is_empty()
                || preparation.remote_branch.trim().is_empty()
                || preparation.local_oid.trim().is_empty()
                || preparation.expected_remote_oid.trim().is_empty() =>
        {
            Err("Prepared PR publication identity is incomplete".into())
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
        LocalAction::PublishPrSource { preparation } => preparation.dispatch(&git, guard),
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

/// The local workspace's own view of the review palette. The neutral steps
/// here track `crate::app::palette` exactly; they are duplicated rather than
/// shared only because this view is separately owned, so a change to one ramp
/// belongs in both.
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
            canvas: rgba(0x111417ff),
            surface: rgba(0x1b1f24ff),
            sidebar: rgba(0x131619f0),
            elevated: rgba(0x22272dff),
            text: rgba(0xf0f6fcff),
            muted: rgba(0xb7bfc8ff),
            border: rgba(0x373e47ff),
            selected: rgba(0xffffff14),
            accent: rgba(0x4493f8ff),
            green: rgba(0x3fb950ff),
            red: rgba(0xf85149ff),
            amber: rgba(0xd29922ff),
            dark,
        }
    } else {
        LocalPalette {
            canvas: rgba(0xf2f4f7ff),
            surface: rgba(0xffffffff),
            sidebar: rgba(0xf4f6f8f0),
            elevated: rgba(0xeef1f4ff),
            text: rgba(0x1f2328ff),
            muted: rgba(0x59636eff),
            border: rgba(0xccd3dbff),
            selected: rgba(0x0000000f),
            accent: rgba(0x0969daff),
            green: rgba(0x1a7f37ff),
            red: rgba(0xd1242fff),
            amber: rgba(0x9a6700ff),
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

/// Style for the rebase panel's plain-text commit-message editor. No syntax
/// highlighting: this app no longer opens worktree files for editing.
fn editor_style(colors: LocalPalette) -> InputEditorStyle {
    InputEditorStyle {
        foreground: colors.text.into(),
        muted_foreground: colors.muted.into(),
        background: colors.canvas.into(),
        border: colors.border.into(),
        selection: colors.accent.into(),
        caret: colors.text.into(),
        editor_gutter_background: Some(colors.canvas.into()),
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

impl Render for LocalWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = palette(self.context.appearance.dark);

        let rebase_open = self.rebase.open;
        let workspace_content = if rebase_open {
            self.render_rebase_panel(colors, window, cx)
        } else {
            self.render_changes_panel(colors, cx)
        };

        div()
            .id("local-workspace")
            .key_context("LocalWorkspace")
            .size_full()
            .flex()
            .flex_col()
            .font_family(ui::TEXT_FONT)
            .ui_text(TextRole::Body)
            .bg(colors.canvas)
            .text_color(colors.text)
            .on_action(cx.listener(|this, _: &LocalRefresh, _, cx| this.refresh_all(cx)))
            .on_action(cx.listener(|this, _: &LocalConfirm, _, cx| {
                if let Some(id) = this.operations.pending_local().map(|pending| pending.id) {
                    this.confirm_action(id, cx);
                } else if let Some(id) = this.rebase_pending_action_id() {
                    this.confirm_rebase_action(id, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &LocalCancel, _, cx| {
                if let Some(id) = this.operations.pending_local().map(|pending| pending.id) {
                    this.cancel_action(id, cx);
                } else if let Some(id) = this.rebase_pending_action_id() {
                    this.cancel_rebase_action(id, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if this.handle_rebase_key(event, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .h(px(48.))
                    .px(px(ui::PANEL_GUTTER))
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(colors.surface)
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .font_weight(ui::WEIGHT_EMPHASIS)
                            .child(format!("{} · Local workspace", self.context.repository.full_name())),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(ui::GAP_COLUMNS))
                            .child(
                                div()
                                    .ui_text(TextRole::Caption)
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
                    .child(workspace_content),
            )
            .child(
                div()
                    .min_h(px(34.))
                    .px(px(ui::PANEL_GUTTER))
                    .py(px(ui::GAP_GROUP))
                    .bg(colors.surface)
                    .border_t_1()
                    .border_color(colors.border)
                    .ui_text(TextRole::Caption)
                    .text_color(colors.muted)
                    .child(self.status.clone()),
            )
    }
}

impl LocalWorkspace {
    fn render_changes_panel(&mut self, colors: LocalPalette, cx: &mut Context<Self>) -> AnyElement {
        let Some(snapshot) = self.snapshot.clone() else {
            return div()
                .w(px(340.))
                .h_full()
                .bg(colors.sidebar)
                .border_l_1()
                .border_color(colors.border)
                .p(px(ui::PANEL_GUTTER))
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
        // Rows own their horizontal inset so a selected row's background reaches
        // the panel edges instead of floating inside a padded column.
        let mut entries = div()
            .id("local-changes-entries")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(ui::GAP_ICON))
            .py(px(ui::GAP_GROUP));
        for (label, path, target) in local_change_rows(&snapshot) {
            // The selected row is identified by the diff already loaded into the
            // panel, so selection presentation never diverges from the content.
            let selected = self
                .selected_diff
                .as_ref()
                .is_some_and(|diff| diff.path == path && diff.target == target);
            let diff_path = path.clone();
            entries = entries.child(
                div()
                    .id(ElementId::Name(
                        format!("change-{label}-{}", path.display).into(),
                    ))
                    .h(px(ui::CONTROL_HEIGHT))
                    .px(px(ui::CELL_INSET))
                    .flex()
                    .items_center()
                    .gap(px(ui::GAP_GROUP))
                    .rounded(px(ui::CONTROL_RADIUS))
                    .ui_text(TextRole::Body)
                    .when(selected, |row| row.bg(colors.selected))
                    .hover(|row| row.bg(colors.selected))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_local_diff(diff_path.clone(), target, cx)
                    }))
                    // The status column stays a fixed width so the paths below it
                    // align into a single readable column.
                    .child(
                        div()
                            .w(px(CHANGE_STATUS_COLUMN))
                            .flex_none()
                            .text_color(colors.muted)
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(path.display),
                    ),
            );
        }
        let diff = self.selected_diff.as_ref().map(|diff| match &diff.content {
            DiffContent::Text(text) => text.clone(),
            DiffContent::BinaryMetadata => "Binary diff (content not loaded)".into(),
            DiffContent::MediaMetadata => "Media diff (content not loaded)".into(),
            DiffContent::UnsupportedMetadata { reason } => format!("Unsupported diff: {reason}"),
        });
        let pending = self.operations.pending_local().cloned();
        let unresolved = self.operations.recovery().cloned();
        let remote_observation = self.operations.remote_observation().cloned();
        let mut panel = div()
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(colors.sidebar)
            .border_l_1()
            .border_color(colors.border)
            .child(
                div()
                    .px(px(ui::CELL_INSET))
                    .py(px(ui::GAP_GROUP))
                    .border_b_1()
                    .border_color(colors.border)
                    .flex()
                    .flex_col()
                    .gap(px(ui::GAP_ICON))
                    .child(ui::kicker("Local changes").text_color(colors.muted))
                    .child(
                        div()
                            .font_family(CODE_FONT)
                            .ui_text(TextRole::Caption)
                            .child(head),
                    )
                    .child(
                        div()
                            .ui_text(TextRole::Caption)
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
                                .font_family(CODE_FONT)
                                .ui_text(TextRole::Caption)
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
                        .p(px(ui::GAP_GROUP))
                        .font_family(CODE_FONT)
                        .ui_text(TextRole::Caption)
                        .whitespace_normal()
                        .child(diff),
                )
            })
            .child(
                div()
                    .px(px(ui::CONTROL_INSET))
                    .py(px(ui::GAP_GROUP))
                    .border_t_1()
                    .border_color(colors.border)
                    .flex()
                    .flex_col()
                    .gap(px(ui::GAP_GROUP))
                    .child(
                        ui::text_field(colors.elevated, colors.border)
                            .child(Input::new(&self.commit_message)),
                    )
                    .child(
                        ui::text_field(colors.elevated, colors.border)
                            .child(Input::new(&self.branch_name)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(ui::GAP_GROUP))
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
        if self.pr_publish_context.is_some() {
            let preparation = self.pr_publish_preparation.clone();
            let controls_locked = self.operations.controls_locked();
            let mut publish = div()
                .id("pr-source-publish")
                .p(px(ui::GAP_COLUMNS))
                .border_t_1()
                .border_color(colors.border)
                .flex()
                .flex_col()
                .gap(px(ui::GAP_ICON))
                .child(
                    div()
                        .font_weight(ui::WEIGHT_EMPHASIS)
                        .child("Publish to PR"),
                )
                .child(
                    div()
                        .ui_text(TextRole::Caption)
                        .text_color(colors.muted)
                        .whitespace_normal()
                        .child(self.pr_publish_notice.clone()),
                );
            if let Some(preparation) = preparation.clone() {
                publish = publish
                    .child(div().ui_text(TextRole::Caption).whitespace_normal().child(format!(
                        "{} #{} · {}",
                        preparation.selected_repository, preparation.pull_request_number,
                        preparation.selected_account,
                    )))
                    .child(div().ui_text(TextRole::Caption).whitespace_normal().child(format!(
                        "Local: {} ({})", preparation.local_branch,
                        &preparation.local_oid[..12.min(preparation.local_oid.len())],
                    )))
                    .child(div().ui_text(TextRole::Caption).whitespace_normal().child(format!(
                        "To: {} · {}", preparation.source_repository, preparation.remote_branch,
                    )))
                    .child(div().ui_text(TextRole::Caption).text_color(colors.muted).whitespace_normal().child(
                        match preparation.mode {
                            PrPublishMode::UpToDate => "These commits are already published.",
                            PrPublishMode::Publish => "Adds local commits without forcing the remote branch.",
                            PrPublishMode::RepublishWithLease => "Replaces rewritten history only if the remote branch still matches the checked commit.",
                        },
                    ))
                    .child(action_button(
                        if self.pr_publish_details_expanded { "Hide commit details" } else { "Show commit details" },
                        colors,
                        cx.listener(|this, _, _, cx| {
                            this.toggle_pr_publish_details(cx);
                        }),
                    ));
                if self.pr_publish_details_expanded {
                    publish = publish.child(
                        div()
                            .ui_text(TextRole::Caption)
                            .font_family(CODE_FONT)
                            .whitespace_normal()
                            .child(format!(
                                "Source: {}/{}\nLocal commit: {}\nPR head: {}\nRemote {}/{}: {}",
                                preparation.source_host,
                                preparation.source_repository,
                                preparation.local_oid,
                                preparation.provider_head_oid,
                                preparation.destination.remote,
                                preparation.remote_branch,
                                preparation.expected_remote_oid,
                            )),
                    );
                }
            }
            let mut buttons = div().flex().flex_wrap().gap(px(ui::GAP_GROUP));
            if !controls_locked {
                buttons = buttons.child(action_button(
                    if preparation.is_some() {
                        "Refresh target"
                    } else {
                        "Prepare publish"
                    },
                    colors,
                    cx.listener(|this, _, _, cx| this.prepare_pr_publish(cx)),
                ));
                if preparation
                    .as_ref()
                    .is_some_and(|preparation| preparation.mode != PrPublishMode::UpToDate)
                {
                    let label = preparation
                        .as_ref()
                        .map(|preparation| preparation.mode.label())
                        .unwrap_or("Publish");
                    buttons = buttons.child(action_button(
                        label,
                        colors,
                        cx.listener(|this, _, _, cx| {
                            this.request_prepared_pr_publish(cx);
                        }),
                    ));
                }
            }
            publish = publish.child(buttons);
            panel = panel.child(publish);
        }
        if let Some(pending) = pending {
            let id = pending.id;
            panel = panel.child(
                div()
                    .p(px(ui::GAP_COLUMNS))
                    .bg(if colors.dark {
                        rgba(0xbb800926)
                    } else {
                        rgba(0xfff8c5ff)
                    })
                    .border_t_1()
                    .border_color(colors.amber)
                    .flex()
                    .flex_col()
                    .gap(px(ui::GAP_GROUP))
                    .child(
                        div()
                            .font_weight(ui::WEIGHT_EMPHASIS)
                            .child("Confirm Git action"),
                    )
                    .child(
                        div()
                            .ui_text(TextRole::Caption)
                            .child(pending.action.summary()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(ui::GAP_GROUP))
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
            let publish_attempt = started.pr_publish.clone();
            let mut reconciliation = div()
                    .p(px(ui::GAP_COLUMNS))
                    .bg(if colors.dark {
                        rgba(0xf851491a)
                    } else {
                        rgba(0xffebe9ff)
                    })
                    .border_t_1()
                    .border_color(colors.red)
                    .flex()
                    .flex_col()
                    .gap(px(ui::GAP_ICON))
                    .child(div().font_weight(ui::WEIGHT_EMPHASIS).child("Started action needs reconciliation"))
                    .child(div().ui_text(TextRole::Caption).child(started.summary))
                    .child(div().ui_text(TextRole::Caption).child("Refresh and inspect actual state. This control only acknowledges; it never retries."));
            if let Some(attempt) = publish_attempt {
                reconciliation =
                    reconciliation
                        .child(
                            div()
                                .font_family(CODE_FONT)
                                .ui_text(TextRole::Caption)
                                .whitespace_normal()
                                .child(format!(
                                    "request {} · {} {} @ {} -> {}/{} expected {} · config {}",
                                    attempt.request_id,
                                    attempt.mode.label(),
                                    attempt.local_branch,
                                    attempt.local_oid,
                                    attempt.destination_remote,
                                    attempt.remote_branch,
                                    attempt.expected_remote_oid,
                                    attempt.destination_configuration_fingerprint,
                                )),
                        )
                        .child(div().ui_text(TextRole::Caption).whitespace_normal().child(
                            format!(
                                "target {} · selected {}/{} #{}",
                                attempt.destination_repository,
                                attempt.selected_host,
                                attempt.selected_repository,
                                attempt.pull_request_number,
                            ),
                        ));
            }
            reconciliation = reconciliation.child(action_button(
                "Acknowledge observed current state",
                colors,
                cx.listener(|this, _, _, cx| this.acknowledge_action_reconciliation(cx)),
            ));
            panel = panel.child(reconciliation);
        }
        div()
            .id("local-actions-scroll")
            .w(px(360.))
            .min_w(px(300.))
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.local_actions_scroll)
            .child(panel)
            .into_any_element()
    }
}

/// A native Button rather than a clickable div: local actions are the panel's
/// real work, so they are reachable with Tab and activate on Enter and Space.
fn action_button(
    label: impl Into<SharedString>,
    colors: LocalPalette,
    listener: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let label = label.into();
    // This module is also compiled standalone by examples/local_workspace_demo,
    // so it cannot reach the review window's shared presentation trait. The ring
    // is spelled out here and must stay in step with `ControlPresentation` in
    // app.rs: keyboard-only, recoloring the border the control already reserves.
    Button::new(ElementId::Name(format!("action-{}", label).into()))
        .accessibility_label(label.clone())
        .control()
        .focus_visible(move |style| style.border_color(colors.accent).bg(colors.selected))
        .border_1()
        .border_color(colors.border)
        .bg(colors.elevated)
        .hover(|button| button.bg(colors.selected))
        .cursor_pointer()
        // Caption is for help and metadata; a control label uses Label.
        .ui_text(TextRole::Label)
        .flex_none()
        .on_click(listener)
        .child(label)
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
    use cibergit::domain::{PullRequestCheckoutSource, Revision};
    use std::{process::Command, thread, time::Instant};
    use tempfile::TempDir;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_text(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("UTF-8 Git output")
            .trim()
            .to_owned()
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

    #[test]
    fn checkout_partition_separates_two_checkouts() {
        let (_first_temp, first) = checkout_fixture();
        let (_second_temp, mut second) = checkout_fixture();
        second.repository = first.repository.clone();
        second.checkout.association.key = first.checkout.association.key.clone();
        assert_ne!(
            checkout_partition(&first).expect("partition"),
            checkout_partition(&second).expect("partition")
        );
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
            pr_publish: None,
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
            pr_publish: None,
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
            pr_publish: None,
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
            pr_publish: None,
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
            pr_publish: None,
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
            pr_publish: None,
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
            pr_publish: None,
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
    fn confirmed_pr_publish_uses_durable_admission_and_actual_local_bare_push() {
        let (_temporary, context) = checkout_fixture();
        let root = &context.checkout.association.path;
        let source_bare = tempfile::tempdir().expect("source bare");
        git(source_bare.path(), &["init", "--bare", "-q"]);
        git(
            root,
            &[
                "remote",
                "add",
                "fork-source",
                source_bare.path().to_str().expect("path"),
            ],
        );
        let published = git_text(root, &["rev-parse", "HEAD"]);
        git(
            root,
            &[
                "push",
                "-q",
                "fork-source",
                "feature:refs/heads/feature/published",
            ],
        );
        fs::write(root.join("same.txt"), "publish next\n").expect("local change");
        git(root, &["add", "same.txt"]);
        git(root, &["commit", "-m", "publish next"]);
        let backend = LocalGit::open(root).expect("local git");
        let local_oid = match backend.snapshot().expect("snapshot").head {
            HeadState::Attached { oid, .. } => oid,
            other => panic!("attached head required: {other:?}"),
        };
        let source_repository = Repository {
            host: context.repository.host.clone(),
            owner: "fork-owner".into(),
            name: "source-repo".into(),
            account: context.repository.account.clone(),
            local_path: Some(source_bare.path().to_owned()),
        };
        let source = PullRequestCheckoutSource {
            number: 7,
            base_repository: context.repository.clone(),
            source_repository: Some(source_repository),
            source_branch: "feature/published".into(),
            target_branch: "main".into(),
            observed_revision: Revision {
                base_sha: published.clone(),
                head_sha: published.clone(),
            },
        };
        let preparation = pr_publish::prepare_test_source(
            context.repository.clone(),
            7,
            &context.checkout,
            &backend,
            source,
        )
        .expect("prepare explicit publish");
        assert_eq!(preparation.mode, PrPublishMode::Publish);
        let publish_guard = preparation.snapshot_guard().clone();
        let started = StartedAction {
            schema_version: 1,
            request_id: 500,
            kind: "publish-pr-source".into(),
            summary: preparation.summary(),
            checkout_identity: checkout_identity_label(&context.checkout),
            displayed_head: local_oid.clone(),
            expected_remote_oid: Some(published.clone()),
            pr_publish: Some(preparation.attempt(500)),
        };
        let blocked_parent = context.data_root.join("blocked-publish-journal");
        fs::write(&blocked_parent, "not a directory").expect("block journal");
        let blocked = run_local_action(
            &context,
            &blocked_parent.join("started.json"),
            &started,
            LocalAction::PublishPrSource {
                preparation: Box::new(preparation.clone()),
            },
            &publish_guard,
        );
        assert!(matches!(
            blocked,
            Err(LocalActionRunError::NotDispatched {
                journal_preserved: false,
                ..
            })
        ));
        assert_eq!(
            git_text(
                source_bare.path(),
                &["rev-parse", "refs/heads/feature/published"]
            ),
            published,
            "durable admission failure must dispatch zero pushes"
        );

        let uncertain_journal = context.data_root.join("publish-uncertain/started.json");
        let preserved = run_local_action_with_journal(
            &context,
            &uncertain_journal,
            &started,
            LocalAction::PublishPrSource {
                preparation: Box::new(preparation.clone()),
            },
            &publish_guard,
            |path, attempted| {
                write_started_action(path, attempted)?;
                Err(JournalWriteError::InstalledOrUncertain(
                    "injected loss after durable PR publish admission".into(),
                ))
            },
        );
        assert!(matches!(
            preserved,
            Err(LocalActionRunError::NotDispatched {
                journal_preserved: true,
                ..
            })
        ));
        let readback = read_started_action(&uncertain_journal)
            .expect("read durable PR publish attempt")
            .expect("preserved PR publish attempt");
        assert_eq!(readback, started);
        let evidence = PrPublishReconciliationEvidence {
            attempt: readback
                .pr_publish
                .clone()
                .expect("persisted attempt evidence"),
            reconciliation: pr_publish::PrPublishReconciliation {
                outcome: pr_publish::PrPublishReconciliationOutcome::MatchesExpected,
            },
        };
        assert_eq!(
            Some(&evidence.attempt),
            readback.pr_publish.as_ref(),
            "evidence binds the exact durable attempt"
        );
        let mut replacement = readback.clone();
        replacement
            .pr_publish
            .as_mut()
            .expect("replacement attempt")
            .attempt_id += 1;
        assert_ne!(Some(&evidence.attempt), replacement.pr_publish.as_ref());
        assert_eq!(
            readback
                .pr_publish
                .as_ref()
                .expect("full PR publish context")
                .destination_configuration_fingerprint
                .len(),
            64
        );
        assert_eq!(
            git_text(
                source_bare.path(),
                &["rev-parse", "refs/heads/feature/published"]
            ),
            published,
            "preserved admission uncertainty must not guess or replay"
        );
        assert!(
            clear_started_action(&uncertain_journal, &started)
                .expect("exact read-only acknowledgement")
        );

        let journal = context.data_root.join("publish/started.json");
        let (receipt, snapshot) = run_local_action(
            &context,
            &journal,
            &started,
            LocalAction::PublishPrSource {
                preparation: Box::new(preparation),
            },
            &publish_guard,
        )
        .expect("confirmed publish");
        assert_eq!(receipt.action, cibergit::local_git::MutationAction::Push);
        assert!(matches!(snapshot.head, HeadState::Attached { .. }));
        assert_eq!(
            git_text(
                source_bare.path(),
                &["rev-parse", "refs/heads/feature/published"]
            ),
            local_oid
        );
        assert!(!journal.exists(), "exact successful attempt clears journal");
    }
}
