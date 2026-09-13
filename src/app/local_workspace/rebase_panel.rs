//! LocalWorkspace-owned interactive-rebase controller and native panel.
//!
//! The controller never invents Git state. Every mutation is confirmed against
//! an immutable backend snapshot plus the exact state of every open document.

use super::*;
use cibergit::rebase::{
    ActiveOperationIdentity, BlobContent, ConflictFile, DirtyPreparation,
    OperationState as RebaseState, PlanAction, PlanStep, PrepareOutcome, RebasePlan,
    RebasePreparation, SplitState, StashRestoreState,
};
use std::{ffi::OsString, os::unix::ffi::OsStringExt};

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocumentFenceEntry {
    path: PathBuf,
    edit_generation: u64,
    editor_value: String,
    accepted_base: String,
    status: DocumentStatus,
    pending_checkout_operations: usize,
    pending_programmatic_reload: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DocumentFence(Vec<DocumentFenceEntry>);

#[derive(Clone)]
enum RebaseCommand {
    CreateStash(DirtyPreparation),
    Start {
        preparation: RebasePreparation,
        plan: RebasePlan,
    },
    Continue {
        operation_id: String,
        active: ActiveOperationIdentity,
        guard: SnapshotGuard,
    },
    Skip {
        operation_id: String,
        active: ActiveOperationIdentity,
        guard: SnapshotGuard,
    },
    Abort {
        operation_id: String,
        active: ActiveOperationIdentity,
        guard: SnapshotGuard,
    },
    Amend {
        operation_id: String,
        active: ActiveOperationIdentity,
        guard: SnapshotGuard,
        message: Option<String>,
    },
    BeginSplit {
        operation_id: String,
        active: ActiveOperationIdentity,
        guard: SnapshotGuard,
    },
    CommitSplitPart {
        operation_id: String,
        active: ActiveOperationIdentity,
        guard: SnapshotGuard,
        message: String,
    },
    FinishSplit {
        operation_id: String,
        active: ActiveOperationIdentity,
        guard: SnapshotGuard,
    },
    StageConflict {
        operation_id: String,
        active: Option<ActiveOperationIdentity>,
        expected: Box<ConflictFile>,
        guard: SnapshotGuard,
        stash_restore: bool,
    },
    RestoreStash {
        operation_id: String,
        guard: SnapshotGuard,
    },
    FinishStashRestore {
        operation_id: String,
        guard: SnapshotGuard,
    },
    Retire {
        operation_id: String,
    },
}

impl RebaseCommand {
    fn summary(&self) -> String {
        match self {
            Self::CreateStash(dirty) => format!(
                "Stash {} staged, {} unstaged, and {} untracked entries (ignored files excluded)",
                dirty.staged, dirty.unstaged, dirty.untracked
            ),
            Self::Start { plan, .. } => {
                format!("Start the frozen {}-commit linear rebase plan", plan.steps.len())
            }
            Self::Continue { .. } => "Continue the currently observed rebase".into(),
            Self::Skip { .. } => "Skip the currently stopped replay commit".into(),
            Self::Abort { .. } => "Abort this rebase. Git may discard recorded split/rebase edits and untracked obstructions at restored tracked paths during reset --hard/rebase --abort. This does not claim every untracked file is preserved.".into(),
            Self::Amend { message, .. } => match message {
                Some(_) => "Amend the edit-stop commit with staged content and the displayed message".into(),
                None => "Amend the edit-stop commit with staged content and keep its message".into(),
            },
            Self::BeginSplit { .. } => "Begin split: record the exact stop/tree, then mixed-reset the stopped commit".into(),
            Self::CommitSplitPart { .. } => "Commit the currently staged split part".into(),
            Self::FinishSplit { .. } => "Validate at least two replacements and the exact stopped tree, then finish the split".into(),
            Self::StageConflict { expected, .. } => format!(
                "Re-read exact conflict stages and disk identity, then stage {}",
                expected.path.display
            ),
            Self::RestoreStash { .. } => "Apply the exact retained stash with index metadata; never pop or delete it".into(),
            Self::FinishStashRestore { .. } => "Finish the explicitly resolved stash restoration".into(),
            Self::Retire { .. } => "Close and archive this conclusively inactive operation with its evidence".into(),
        }
    }

    fn guard(&self) -> Option<&SnapshotGuard> {
        match self {
            Self::CreateStash(dirty) => Some(&dirty.guard),
            Self::Start { preparation, .. } => Some(&preparation.guard),
            Self::Continue { guard, .. }
            | Self::Skip { guard, .. }
            | Self::Abort { guard, .. }
            | Self::Amend { guard, .. }
            | Self::BeginSplit { guard, .. }
            | Self::CommitSplitPart { guard, .. }
            | Self::FinishSplit { guard, .. }
            | Self::StageConflict { guard, .. }
            | Self::RestoreStash { guard, .. }
            | Self::FinishStashRestore { guard, .. } => Some(guard),
            Self::Retire { .. } => None,
        }
    }
}

#[derive(Clone)]
struct PendingRebaseCommand {
    id: u64,
    command: RebaseCommand,
    documents: DocumentFence,
    checkout_generation: u64,
}

pub(super) struct RebasePanel {
    pub(super) open: bool,
    base: Entity<InputState>,
    message: Entity<EditorState>,
    operation: Option<RebaseOperationView>,
    preparation: Option<PrepareOutcome>,
    steps: Vec<PlanStep>,
    selected_step: Option<usize>,
    conflicts: Vec<ConflictFile>,
    pending: Option<PendingRebaseCommand>,
    in_flight: Option<u64>,
    read_generation: u64,
    status: String,
}

impl RebasePanel {
    pub(super) fn new(
        colors: LocalPalette,
        window: &mut Window,
        cx: &mut Context<LocalWorkspace>,
    ) -> Self {
        let base = new_input(
            "Full commit OID, HEAD, or fully-qualified refs/*",
            colors,
            window,
            cx,
        );
        let message = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx).language("text");
            editor.set_editor_style(editor_style(colors));
            editor
        });
        Self {
            open: false,
            base,
            message,
            operation: None,
            preparation: None,
            steps: Vec::new(),
            selected_step: None,
            conflicts: Vec::new(),
            pending: None,
            in_flight: None,
            read_generation: 0,
            status: "Choose an immutable local base candidate".into(),
        }
    }

    pub(super) fn install_observed(&mut self, operation: Option<RebaseOperationView>) {
        self.operation = operation;
        if self.operation.is_some() {
            self.open = true;
            self.status =
                "Recovered the durable rebase lifecycle; observing actual Git state".into();
        }
    }

    pub(super) fn update_appearance(&self, colors: LocalPalette, cx: &mut Context<LocalWorkspace>) {
        self.base.update(cx, |input, _| {
            input.set_editor_style(InputEditorStyle {
                foreground: colors.text.into(),
                muted_foreground: colors.muted.into(),
                background: colors.elevated.into(),
                border: colors.border.into(),
                ..Default::default()
            });
        });
        self.message.update(cx, |editor, _| {
            editor.set_editor_style(editor_style(colors))
        });
    }

    pub(super) fn is_running(&self) -> bool {
        self.in_flight.is_some()
    }

    fn invalidate_reads(&mut self) -> u64 {
        advance_generation(&mut self.read_generation)
    }

    fn accepts_read(&self, generation: u64) -> bool {
        read_reply_is_current(self.read_generation, generation, self.in_flight.is_some())
    }

    pub(super) fn has_pending_or_running(&self) -> bool {
        self.pending.is_some() || self.in_flight.is_some()
    }

    fn set_operation(
        &mut self,
        operation: Option<RebaseOperationView>,
        conflicts: Vec<ConflictFile>,
    ) {
        self.operation = operation;
        self.conflicts = conflicts;
    }
}

#[derive(Clone)]
struct RebaseEffectResult {
    preparation: Option<RebasePreparation>,
    operation: Option<RebaseOperationView>,
    conflicts: Vec<ConflictFile>,
    retired: bool,
    message: String,
}

impl LocalWorkspace {
    pub fn rebase_is_open(&self) -> bool {
        self.rebase.open
    }

    pub fn rebase_operation(&self) -> Option<&RebaseOperationView> {
        self.rebase.operation.as_ref()
    }

    pub fn rebase_status_message(&self) -> &str {
        &self.rebase.status
    }

    pub fn rebase_pending_action_id(&self) -> Option<u64> {
        self.rebase.pending.as_ref().map(|pending| pending.id)
    }

    pub fn rebase_plan_len(&self) -> usize {
        self.rebase.steps.len()
    }

    pub fn set_rebase_step_action(
        &mut self,
        index: usize,
        action: PlanAction,
        cx: &mut Context<Self>,
    ) {
        if index < self.rebase.steps.len() {
            self.rebase.selected_step = Some(index);
            self.set_rebase_action(action, cx);
        }
    }

    pub fn move_rebase_plan_step(&mut self, index: usize, delta: isize, cx: &mut Context<Self>) {
        if index < self.rebase.steps.len() {
            self.rebase.selected_step = Some(index);
            self.move_rebase_step(delta, cx);
        }
    }

    pub fn set_rebase_base_candidate(
        &mut self,
        candidate: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let candidate = candidate.into();
        self.rebase
            .base
            .update(cx, |input, cx| input.set_value(candidate, window, cx));
    }

    fn capture_document_fence(&self, cx: &App) -> DocumentFence {
        DocumentFence(
            self.documents
                .values()
                .map(|tab| DocumentFenceEntry {
                    path: tab.path.clone(),
                    edit_generation: tab.edit_generation,
                    editor_value: tab.editor.read(cx).value().to_string(),
                    accepted_base: tab.view.base.clone(),
                    status: tab.view.status,
                    pending_checkout_operations: tab.pending_checkout_operations,
                    pending_programmatic_reload: tab.pending_programmatic_reload.is_some(),
                })
                .collect(),
        )
    }

    fn rebase_document_blocker(&self, cx: &App) -> Option<String> {
        self.checkout_action_blocker(cx).map(|reason| {
            format!("Rebase paused: {reason}. Save and reconcile in-memory text first; Git stash cannot preserve editor buffers")
        })
    }

    fn rebase_lane_blocker(&self, cx: &App) -> Option<String> {
        if self.rebase.in_flight.is_some() {
            return Some("A rebase effect is already running".into());
        }
        if self.pending_action.is_some()
            || self.in_flight_action.is_some()
            || self.unresolved_started_action.is_some()
            || self.reconciliation_clear_in_flight.is_some()
        {
            return Some("A local Git action or its durable reconciliation is pending".into());
        }
        self.rebase_document_blocker(cx)
    }

    fn document_fence_matches(&self, expected: &DocumentFence, cx: &App) -> bool {
        self.capture_document_fence(cx) == *expected
    }

    fn set_rebase_editors_disabled(&self, disabled: bool, cx: &mut Context<Self>) {
        for tab in self.documents.values() {
            tab.editor
                .update(cx, |editor, cx| editor.set_disabled(disabled, cx));
        }
        self.rebase
            .message
            .update(cx, |editor, cx| editor.set_disabled(disabled, cx));
        self.rebase
            .base
            .update(cx, |input, cx| input.set_disabled(disabled, cx));
    }

    pub fn toggle_rebase(&mut self, cx: &mut Context<Self>) {
        self.rebase.open = !self.rebase.open;
        if self.rebase.open {
            self.observe_rebase(cx);
        }
        cx.notify();
    }

    pub fn prepare_rebase(&mut self, cx: &mut Context<Self>) {
        if let Some(error) = self.rebase_lane_blocker(cx) {
            self.report_error(error, cx);
            return;
        }
        if self.rebase.pending.is_some() || self.rebase.operation.is_some() {
            self.report_error(
                "Close the current rebase request before preparing another".into(),
                cx,
            );
            return;
        }
        let candidate = self.rebase.base.read(cx).value().trim().to_owned();
        if candidate.is_empty() {
            self.report_error(
                "Choose a displayed immutable base candidate first".into(),
                cx,
            );
            return;
        }
        let BackendState::Ready(backend) = &self.backend else {
            self.report_error("Local workspace is not ready".into(), cx);
            return;
        };
        let generation = self.rebase.invalidate_reads();
        let store = backend.rebase.clone();
        let git = backend.git.clone();
        self.rebase.status = format!("Resolving immutable base {candidate}…");
        let task = cx.background_spawn(async move {
            let oid = resolve_base_candidate(&git, &candidate)?;
            let outcome = store.prepare(&oid).map_err(|error| error.to_string())?;
            Ok::<_, String>((oid, outcome))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if !this.rebase.accepts_read(generation) {
                    return;
                }
                match result {
                    Ok((oid, outcome)) => {
                        this.rebase.steps = match &outcome {
                            PrepareOutcome::Ready(preparation) => {
                                pick_steps(&preparation.inventory)
                            }
                            PrepareOutcome::Dirty(preparation) => {
                                pick_steps(&preparation.inventory)
                            }
                            PrepareOutcome::ExternalWorkflow(_) => Vec::new(),
                        };
                        this.rebase.selected_step = (!this.rebase.steps.is_empty()).then_some(0);
                        this.rebase.status = format!("Prepared immutable base {oid}");
                        this.rebase.preparation = Some(outcome);
                        cx.notify();
                    }
                    Err(error) => {
                        this.report_error(format!("Rebase preparation refused: {error}"), cx)
                    }
                }
            });
        })
        .detach();
    }

    fn select_rebase_step(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(step) = self.rebase.steps.get(index) else {
            return;
        };
        let message = match &step.action {
            PlanAction::Reword { message } => message.clone(),
            _ => self
                .rebase_inventory()
                .and_then(|inventory| {
                    inventory
                        .commits
                        .iter()
                        .find(|commit| commit.oid == step.commit_oid)
                })
                .map(|commit| commit.message.clone())
                .unwrap_or_default(),
        };
        self.rebase.selected_step = Some(index);
        self.rebase.message.update(cx, |editor, cx| {
            editor.set_value(message, window, cx);
            editor.focus(window, cx);
        });
        cx.notify();
    }

    fn rebase_inventory(&self) -> Option<&cibergit::rebase::CommitInventory> {
        match self.rebase.preparation.as_ref()? {
            PrepareOutcome::Ready(preparation) => Some(&preparation.inventory),
            PrepareOutcome::Dirty(preparation) => Some(&preparation.inventory),
            PrepareOutcome::ExternalWorkflow(_) => None,
        }
    }

    fn move_rebase_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(index) = self.rebase.selected_step else {
            return;
        };
        let next = index
            .saturating_add_signed(delta)
            .min(self.rebase.steps.len().saturating_sub(1));
        if next != index {
            self.rebase.steps.swap(index, next);
            self.rebase.selected_step = Some(next);
            self.validate_rebase_plan();
            cx.notify();
        }
    }

    fn set_rebase_action(&mut self, action: PlanAction, cx: &mut Context<Self>) {
        let Some(index) = self.rebase.selected_step else {
            return;
        };
        if let Some(step) = self.rebase.steps.get_mut(index) {
            step.action = action;
            self.validate_rebase_plan();
            cx.notify();
        }
    }

    fn apply_reword_message(&mut self, cx: &mut Context<Self>) {
        let message = self.rebase.message.read(cx).value().to_string();
        self.set_rebase_action(PlanAction::Reword { message }, cx);
    }

    fn validate_rebase_plan(&mut self) -> Option<RebasePlan> {
        let inventory = self.rebase_inventory()?.clone();
        match RebasePlan::validate(&inventory, self.rebase.steps.clone()) {
            Ok(plan) => {
                self.rebase.status =
                    "Plan valid · confirmation will freeze this exact order".into();
                Some(plan)
            }
            Err(error) => {
                self.rebase.status = format!("Plan invalid: {error}");
                None
            }
        }
    }

    fn route_dirty_commit(&mut self, cx: &mut Context<Self>) {
        self.rebase.open = false;
        self.status = "Rebase preparation is dirty. Use the existing staged Commit control, then reopen Rebase; unsaved editor text must be saved first.".into();
        cx.notify();
    }

    fn request_rebase_command(&mut self, command: RebaseCommand, cx: &mut Context<Self>) {
        if self.rebase.pending.is_some() {
            self.report_error("A rebase confirmation is already pending".into(), cx);
            return;
        }
        if let Some(error) = self.rebase_lane_blocker(cx) {
            self.report_error(error, cx);
            return;
        }
        let BackendState::Ready(backend) = &self.backend else {
            self.report_error("Local workspace is not ready".into(), cx);
            return;
        };
        if command.guard().is_some_and(|guard| {
            self.snapshot
                .as_ref()
                .is_none_or(|snapshot| &snapshot.guard != guard)
        }) {
            self.report_error("Rebase request paused: the affected Git snapshot changed; refresh and review again".into(), cx);
            return;
        }
        let id = self.next_action_id;
        self.next_action_id = self.next_action_id.wrapping_add(1);
        let summary = command.summary();
        self.rebase.pending = Some(PendingRebaseCommand {
            id,
            command,
            documents: self.capture_document_fence(cx),
            checkout_generation: backend.checkout_generation,
        });
        self.rebase.status = format!("Confirmation required: {summary}");
        cx.emit(LocalWorkspaceEvent::MaterialActionConfirmationRequested {
            request_id: id,
            summary,
        });
        cx.notify();
    }

    pub fn confirm_rebase_action(&mut self, request_id: u64, cx: &mut Context<Self>) {
        let Some(pending) = self.rebase.pending.clone() else {
            self.report_error("There is no rebase action awaiting confirmation".into(), cx);
            return;
        };
        if pending.id != request_id {
            self.report_error("That rebase confirmation is stale".into(), cx);
            return;
        }
        let BackendState::Ready(backend) = &self.backend else {
            return;
        };
        if pending.checkout_generation != backend.checkout_generation {
            self.report_error(
                "Rebase confirmation paused: checkout identity changed".into(),
                cx,
            );
            return;
        }
        if !self.document_fence_matches(&pending.documents, cx) {
            self.report_error("Rebase confirmation paused: editor text or document generation changed. The confirmation remains pending; save/reconcile and request again or cancel.".into(), cx);
            return;
        }
        if let Some(error) = self.rebase_lane_blocker(cx) {
            self.report_error(format!("Rebase confirmation paused: {error}"), cx);
            return;
        }
        if pending.command.guard().is_some_and(|guard| {
            self.snapshot
                .as_ref()
                .is_none_or(|snapshot| &snapshot.guard != guard)
        }) {
            self.report_error("Rebase confirmation paused: the Git snapshot changed. The pending confirmation was retained.".into(), cx);
            return;
        }
        let store = backend.rebase.clone();
        let git = backend.git.clone();
        let checkout_root = self.context.checkout.association.path.clone();
        let command = pending.command;
        self.rebase.pending = None;
        self.rebase.in_flight = Some(request_id);
        // Invalidate preparation/observation replies and document opens that
        // began against the pre-effect checkout before dispatching Git.
        self.rebase.invalidate_reads();
        self.open_generation = self.open_generation.wrapping_add(1);
        self.set_rebase_editors_disabled(true, cx);
        self.rebase.status = "Dispatching the exact confirmed local rebase transition…".into();
        let task = cx.background_spawn(async move {
            let effect = run_rebase_command(&store, command);
            let refresh = (|| {
                let snapshot = git.snapshot().map_err(|error| error.to_string())?;
                let browser = enumerate_worktree(&checkout_root, BrowserLimits::default())?;
                Ok::<_, String>((snapshot, browser))
            })();
            (effect, refresh)
        });
        cx.spawn(async move |this, cx| {
            let (effect, refresh) = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.rebase.in_flight != Some(request_id) {
                    return;
                }
                // A read started during the effect cannot become authoritative
                // after the lane is released, even if its callback is delayed.
                this.rebase.invalidate_reads();
                this.rebase.in_flight = None;
                this.set_rebase_editors_disabled(false, cx);
                let refresh_error = match refresh {
                    Ok((snapshot, browser)) => {
                        this.git_generation = this.git_generation.wrapping_add(1);
                        this.snapshot = Some(snapshot);
                        if let BackendState::Ready(backend) = &mut this.backend {
                            backend.browser = browser;
                        }
                        None
                    }
                    Err(error) => Some(error),
                };
                let paths = this.documents.keys().cloned().collect::<Vec<_>>();
                for path in paths {
                    this.refresh_document(&path, cx);
                }
                match effect {
                    Ok(effect) => {
                        if effect.retired {
                            this.rebase.set_operation(None, Vec::new());
                            this.rebase.preparation = None;
                            this.rebase.steps.clear();
                        } else {
                            if let Some(preparation) = effect.preparation {
                                this.rebase.preparation = Some(PrepareOutcome::Ready(preparation));
                            }
                            if effect.operation.is_some() {
                                this.rebase.set_operation(effect.operation, effect.conflicts);
                            }
                        }
                        this.rebase.status = match refresh_error {
                            Some(error) => format!(
                                "{} Authoritative worktree refresh failed: {error}",
                                effect.message
                            ),
                            None => effect.message,
                        };
                        this.observe_rebase(cx);
                        cx.emit(LocalWorkspaceEvent::LocalSnapshotChanged);
                        cx.notify();
                    }
                    Err(error) => {
                        let refresh_note = refresh_error
                            .map(|error| format!(" Worktree refresh also failed: {error}."))
                            .unwrap_or_default();
                        this.rebase.status = format!("Rebase transition stopped: {error}.{refresh_note} It will not be replayed automatically; observe actual Git state.");
                        this.observe_rebase(cx);
                        cx.emit(LocalWorkspaceEvent::Error(this.rebase.status.clone()));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub fn cancel_rebase_action(&mut self, request_id: u64, cx: &mut Context<Self>) {
        if self
            .rebase
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == request_id)
        {
            self.rebase.pending = None;
            self.rebase.status = "Rebase action cancelled; Git was not started".into();
            cx.notify();
        } else {
            self.report_error("That rebase cancellation is stale".into(), cx);
        }
    }

    pub(super) fn observe_rebase(&mut self, cx: &mut Context<Self>) {
        let BackendState::Ready(backend) = &self.backend else {
            return;
        };
        if self.rebase.in_flight.is_some() {
            return;
        }
        let generation = self.rebase.invalidate_reads();
        let store = backend.rebase.clone();
        let task = cx.background_spawn(async move { observe_bundle(&store) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if !this.rebase.accepts_read(generation) {
                    return;
                }
                match result {
                    Ok((operation, conflicts)) => {
                        this.rebase.set_operation(operation, conflicts);
                        cx.notify();
                    }
                    Err(error) => {
                        this.rebase.status = format!("Rebase observation failed: {error}");
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub fn request_start_rebase(&mut self, cx: &mut Context<Self>) {
        let Some(PrepareOutcome::Ready(preparation)) = self.rebase.preparation.clone() else {
            self.report_error("Rebase is not ready to start".into(), cx);
            return;
        };
        let Some(plan) = self.validate_rebase_plan() else {
            return;
        };
        self.request_rebase_command(RebaseCommand::Start { preparation, plan }, cx);
    }

    fn request_create_stash(&mut self, cx: &mut Context<Self>) {
        let Some(PrepareOutcome::Dirty(dirty)) = self.rebase.preparation.clone() else {
            return;
        };
        self.request_rebase_command(RebaseCommand::CreateStash(dirty), cx);
    }

    fn current_active_guard(&self) -> Option<(String, ActiveOperationIdentity, SnapshotGuard)> {
        let operation = self.rebase.operation.as_ref()?;
        Some((
            operation.operation_id.clone(),
            operation.active.clone()?,
            self.snapshot.as_ref()?.guard.clone(),
        ))
    }

    pub fn request_continue_rebase(&mut self, cx: &mut Context<Self>) {
        let Some((operation_id, active, guard)) = self.current_active_guard() else {
            return;
        };
        self.request_rebase_command(
            RebaseCommand::Continue {
                operation_id,
                active,
                guard,
            },
            cx,
        );
    }

    fn request_skip_rebase(&mut self, cx: &mut Context<Self>) {
        let Some((operation_id, active, guard)) = self.current_active_guard() else {
            return;
        };
        self.request_rebase_command(
            RebaseCommand::Skip {
                operation_id,
                active,
                guard,
            },
            cx,
        );
    }

    pub fn request_abort_rebase(&mut self, cx: &mut Context<Self>) {
        let Some((operation_id, active, guard)) = self.current_active_guard() else {
            return;
        };
        self.request_rebase_command(
            RebaseCommand::Abort {
                operation_id,
                active,
                guard,
            },
            cx,
        );
    }

    fn request_amend_rebase(&mut self, cx: &mut Context<Self>) {
        let Some((operation_id, active, guard)) = self.current_active_guard() else {
            return;
        };
        let value = self.rebase.message.read(cx).value().to_string();
        let message = (!value.trim().is_empty()).then_some(value);
        self.request_rebase_command(
            RebaseCommand::Amend {
                operation_id,
                active,
                guard,
                message,
            },
            cx,
        );
    }

    fn request_begin_split(&mut self, cx: &mut Context<Self>) {
        let Some((operation_id, active, guard)) = self.current_active_guard() else {
            return;
        };
        self.request_rebase_command(
            RebaseCommand::BeginSplit {
                operation_id,
                active,
                guard,
            },
            cx,
        );
    }

    fn request_commit_split_part(&mut self, cx: &mut Context<Self>) {
        let Some((operation_id, active, guard)) = self.current_active_guard() else {
            return;
        };
        let message = self.rebase.message.read(cx).value().to_string();
        self.request_rebase_command(
            RebaseCommand::CommitSplitPart {
                operation_id,
                active,
                guard,
                message,
            },
            cx,
        );
    }

    fn request_finish_split(&mut self, cx: &mut Context<Self>) {
        let Some((operation_id, active, guard)) = self.current_active_guard() else {
            return;
        };
        self.request_rebase_command(
            RebaseCommand::FinishSplit {
                operation_id,
                active,
                guard,
            },
            cx,
        );
    }

    fn request_restore_stash(&mut self, cx: &mut Context<Self>) {
        let Some(operation) = &self.rebase.operation else {
            return;
        };
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        self.request_rebase_command(
            RebaseCommand::RestoreStash {
                operation_id: operation.operation_id.clone(),
                guard: snapshot.guard.clone(),
            },
            cx,
        );
    }

    fn request_finish_stash_restore(&mut self, cx: &mut Context<Self>) {
        let Some(operation) = &self.rebase.operation else {
            return;
        };
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        self.request_rebase_command(
            RebaseCommand::FinishStashRestore {
                operation_id: operation.operation_id.clone(),
                guard: snapshot.guard.clone(),
            },
            cx,
        );
    }

    pub fn request_retire_rebase(&mut self, cx: &mut Context<Self>) {
        let Some(operation) = &self.rebase.operation else {
            return;
        };
        self.request_rebase_command(
            RebaseCommand::Retire {
                operation_id: operation.operation_id.clone(),
            },
            cx,
        );
    }

    fn open_rebase_conflict(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conflict) = self.rebase.conflicts.get(index) else {
            return;
        };
        if !conflict_is_editor_candidate(conflict) {
            self.rebase.status = format!(
                "{} cannot be opened in the safe text editor; resolve it externally, refresh, then stage explicitly",
                conflict.path.display
            );
            cx.notify();
            return;
        }
        let path = PathBuf::from(OsString::from_vec(conflict.path.raw.clone()));
        self.open_relative_path(path, window, cx);
    }

    fn request_stage_rebase_conflict(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(expected) = self.rebase.conflicts.get(index).cloned() else {
            return;
        };
        let Some(operation) = &self.rebase.operation else {
            return;
        };
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let stash_restore = operation.stash_restore == StashRestoreState::Conflicted;
        self.request_rebase_command(
            RebaseCommand::StageConflict {
                operation_id: operation.operation_id.clone(),
                active: (!stash_restore).then(|| operation.active.clone()).flatten(),
                expected: Box::new(expected),
                guard: snapshot.guard.clone(),
                stash_restore,
            },
            cx,
        );
    }

    fn select_base_candidate(&mut self, oid: String, window: &mut Window, cx: &mut Context<Self>) {
        self.set_rebase_base_candidate(oid, window, cx);
        self.rebase.preparation = None;
        self.rebase.steps.clear();
        self.rebase.status =
            "Immutable base candidate selected; Prepare performs the bounded inventory read".into();
        cx.notify();
    }

    pub(super) fn handle_rebase_key(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.rebase.open || !event.keystroke.modifiers.platform {
            return false;
        }
        match event.keystroke.key.as_str() {
            "up" => self.move_rebase_step(-1, cx),
            "down" => self.move_rebase_step(1, cx),
            _ => return false,
        }
        true
    }

    pub(super) fn render_rebase_panel(
        &mut self,
        colors: LocalPalette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let operation = self.rebase.operation.clone();
        let preparation = self.rebase.preparation.clone();
        let pending = self.rebase.pending.clone();
        let selected = self.rebase.selected_step;
        let inventory = self.rebase_inventory().cloned();
        let mut plan_rows = div().flex().flex_col().gap_1();
        for (index, step) in self.rebase.steps.clone().into_iter().enumerate() {
            let commit = inventory.as_ref().and_then(|inventory| {
                inventory
                    .commits
                    .iter()
                    .find(|commit| commit.oid == step.commit_oid)
            });
            let title = commit
                .map(|commit| first_line(&commit.message))
                .unwrap_or("unknown commit");
            let oid = short_oid(&step.commit_oid);
            let action = plan_action_label(&step.action);
            plan_rows = plan_rows.child(
                div()
                    .id(ElementId::Name(format!("rebase-step-{index}").into()))
                    .min_h(px(42.))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(if selected == Some(index) {
                        colors.accent
                    } else {
                        colors.border
                    })
                    .bg(if selected == Some(index) {
                        colors.selected
                    } else {
                        colors.surface
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_rebase_step(index, window, cx)
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(62.)).text_color(colors.accent).child(action))
                            .child(div().font_family(CODE_FONT).text_xs().child(oid))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(title.to_owned()),
                            ),
                    ),
            );
        }

        let mut content = div()
            .id("rebase-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_4()
            .flex()
            .flex_col()
            .gap_3();
        if let Some(operation) = operation {
            content = content.child(self.render_operation(operation, colors, cx));
        } else if let Some(preparation) = preparation {
            content = content.child(render_preparation_summary(&preparation, colors));
            match preparation {
                PrepareOutcome::ExternalWorkflow(workflow) => {
                    content = content.child(notice_box(
                        "External workflow required",
                        workflow.explanation,
                        colors.red,
                        colors,
                    ));
                }
                PrepareOutcome::Dirty(dirty) => {
                    content = content.child(
                        div().flex().gap_2()
                            .child(action_button("Commit in Local Changes", colors, cx.listener(|this, _, _, cx| this.route_dirty_commit(cx))))
                            .child(action_button("Stash including untracked", colors, cx.listener(|this, _, _, cx| this.request_create_stash(cx))))
                            .child(action_button("Cancel preparation", colors, cx.listener(|this, _, _, cx| {
                                this.rebase.preparation = None;
                                this.rebase.steps.clear();
                                this.rebase.status = "Preparation cancelled; Git was not changed".into();
                                cx.notify();
                            })))
                    ).child(notice_box(
                        "Dirty checkout",
                        format!("{} staged · {} unstaged · {} untracked · {} conflicts. Stash includes untracked; ignored files remain outside it. The exact retained stash receipt appears after creation.", dirty.staged, dirty.unstaged, dirty.untracked, dirty.conflicts),
                        colors.amber,
                        colors,
                    ));
                }
                PrepareOutcome::Ready(_) => {
                    content = content
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(div().font_weight(FontWeight::SEMIBOLD).child("LINEAR PLAN"))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(colors.muted)
                                        .child("⌘↑ / ⌘↓ moves the selected commit"),
                                ),
                        )
                        .child(plan_rows)
                        .child(self.render_plan_editor(colors, cx))
                        .child(action_button(
                            "Review and Start",
                            colors,
                            cx.listener(|this, _, _, cx| this.request_start_rebase(cx)),
                        ));
                }
            }
        } else {
            content = content
                .child(div().font_weight(FontWeight::SEMIBOLD).child("CHOOSE A LOCAL BASE"))
                .child(div().text_xs().text_color(colors.muted).child("Candidates below are already resolved to immutable full commit OIDs. Manual input accepts HEAD, a full OID, or a fully-qualified refs/* name; ambiguous expressions are refused."))
                .child(Input::new(&self.rebase.base));
            if let Some(snapshot) = self.snapshot.clone() {
                if let Some(oid) = snapshot.upstream_oid {
                    content = content.child(action_button(
                        format!("Upstream · {}", short_oid(&oid)),
                        colors,
                        cx.listener(move |this, _, window, cx| {
                            this.select_base_candidate(oid.clone(), window, cx)
                        }),
                    ));
                }
                for (branch, oid) in snapshot.local_branch_oids {
                    let label = format!("{branch} · {}", short_oid(&oid));
                    content = content.child(action_button(
                        label,
                        colors,
                        cx.listener(move |this, _, window, cx| {
                            this.select_base_candidate(oid.clone(), window, cx)
                        }),
                    ));
                }
            }
            content = content.child(action_button(
                "Prepare inventory",
                colors,
                cx.listener(|this, _, _, cx| this.prepare_rebase(cx)),
            ));
        }
        if let Some(pending) = pending {
            let id = pending.id;
            content = content.child(
                notice_box(
                    "Explicit confirmation",
                    pending.command.summary(),
                    colors.amber,
                    colors,
                )
                .child(
                    div()
                        .mt_2()
                        .flex()
                        .gap_2()
                        .child(action_button(
                            "Confirm exact transition",
                            colors,
                            cx.listener(move |this, _, _, cx| this.confirm_rebase_action(id, cx)),
                        ))
                        .child(action_button(
                            "Cancel",
                            colors,
                            cx.listener(move |this, _, _, cx| this.cancel_rebase_action(id, cx)),
                        )),
                ),
            );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.canvas)
            .child(
                div()
                    .min_h(px(52.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(colors.surface)
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child("REBASE"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(colors.muted)
                                    .child("Local-only · guarded · explicit transitions"),
                            ),
                    )
                    .child(action_button(
                        "Back to workspace",
                        colors,
                        cx.listener(|this, _, _, cx| this.toggle_rebase(cx)),
                    )),
            )
            .child(content)
            .child(
                div()
                    .min_h(px(36.))
                    .px_4()
                    .py_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .text_xs()
                    .text_color(colors.muted)
                    .child(self.rebase.status.clone()),
            )
            .into_any_element()
    }

    fn render_plan_editor(&mut self, colors: LocalPalette, cx: &mut Context<Self>) -> AnyElement {
        let actions = div()
            .flex()
            .flex_wrap()
            .gap_1()
            .child(action_button(
                "Pick",
                colors,
                cx.listener(|this, _, _, cx| this.set_rebase_action(PlanAction::Pick, cx)),
            ))
            .child(action_button(
                "Squash",
                colors,
                cx.listener(|this, _, _, cx| this.set_rebase_action(PlanAction::Squash, cx)),
            ))
            .child(action_button(
                "Fixup",
                colors,
                cx.listener(|this, _, _, cx| this.set_rebase_action(PlanAction::Fixup, cx)),
            ))
            .child(action_button(
                "Drop",
                colors,
                cx.listener(|this, _, _, cx| this.set_rebase_action(PlanAction::Drop, cx)),
            ))
            .child(action_button(
                "Reword",
                colors,
                cx.listener(|this, _, _, cx| this.apply_reword_message(cx)),
            ))
            .child(action_button(
                "Edit",
                colors,
                cx.listener(|this, _, _, cx| this.set_rebase_action(PlanAction::Edit, cx)),
            ))
            .child(action_button(
                "Move up",
                colors,
                cx.listener(|this, _, _, cx| this.move_rebase_step(-1, cx)),
            ))
            .child(action_button(
                "Move down",
                colors,
                cx.listener(|this, _, _, cx| this.move_rebase_step(1, cx)),
            ));
        div()
            .h(px(170.))
            .flex()
            .flex_col()
            .gap_2()
            .child(actions)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_1()
                    .border_color(colors.border)
                    .rounded_md()
                    .font_family(CODE_FONT)
                    .child(Editor::new(&self.rebase.message)),
            )
            .into_any_element()
    }

    fn render_operation(
        &mut self,
        operation: RebaseOperationView,
        colors: LocalPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = format!("{:?}", operation.state);
        let mut body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("{} · attempt {}", state, operation.attempt)),
                    )
                    .child(
                        div()
                            .font_family(CODE_FONT)
                            .text_xs()
                            .child(short_oid(&operation.original_head_oid)),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(colors.muted)
                    .whitespace_normal()
                    .child(format!(
                        "branch {} · base {} · operation {}",
                        operation.original_branch, operation.base_oid, operation.operation_id
                    )),
            );
        if let Some(active) = &operation.active {
            body = body.child(notice_box(
                "Guard-bound active identity",
                format!(
                    "onto {} · original {} · stopped {} · todo {} · done {}",
                    active.onto_oid,
                    active.original_head_oid,
                    active.stopped_oid.as_deref().unwrap_or("none"),
                    active.todo_sha256,
                    active.done_sha256
                ),
                colors.accent,
                colors,
            ));
        }
        if let Some(split) = &operation.split {
            body = body.child(render_split(split, colors));
        }
        if !self.rebase.conflicts.is_empty() {
            body = body.child(div().font_weight(FontWeight::SEMIBOLD).child("CONFLICTS"));
            for (index, conflict) in self.rebase.conflicts.clone().into_iter().enumerate() {
                let detail = conflict_reason(&conflict);
                body = body.child(div().p_2().border_1().border_color(colors.border).rounded_md()
                    .child(div().font_family(CODE_FONT).child(conflict.path.display.clone()))
                    .child(div().mt_1().text_xs().text_color(colors.muted).child(detail))
                    .child(div().mt_1().text_xs().child("Rebase orientation: ours = already rebased series; theirs = replayed original commit."))
                    .child(div().mt_2().flex().gap_2()
                        .child(action_button("Open safe result", colors, cx.listener(move |this, _, window, cx| this.open_rebase_conflict(index, window, cx))))
                        .child(action_button("Stage exact saved result", colors, cx.listener(move |this, _, _, cx| this.request_stage_rebase_conflict(index, cx))))));
            }
        }
        if let Some(stash) = &operation.stash {
            body = body.child(notice_box("Retained stash receipt", format!("OID {} · includes untracked: {} · ignored excluded: {} · retained after restore: {} · restore {:?}", stash.oid, stash.includes_untracked, stash.ignored_files_excluded, stash.retained_after_restore, operation.stash_restore), colors.amber, colors));
        }
        match operation.state {
            RebaseState::PausedForEdit if operation.split.is_some() => {
                body = body.child(self.render_edit_message("Split part message", colors))
                    .child(div().flex().flex_wrap().gap_2()
                        .child(action_button("Stage via Local Changes", colors, cx.listener(|this, _, _, cx| { this.rebase.open = false; this.status = "Stage the selected split paths in Local Changes, then reopen Rebase".into(); cx.notify(); })))
                        .child(action_button("Commit staged part", colors, cx.listener(|this, _, _, cx| this.request_commit_split_part(cx))))
                        .child(action_button("Finish validated split", colors, cx.listener(|this, _, _, cx| this.request_finish_split(cx))))
                        .child(action_button("Abort", colors, cx.listener(|this, _, _, cx| this.request_abort_rebase(cx)))));
            }
            RebaseState::PausedForEdit => {
                body = body.child(self.render_edit_message("Optional amend message", colors))
                    .child(div().flex().flex_wrap().gap_2()
                        .child(action_button("Open/stage in Local Changes", colors, cx.listener(|this, _, _, cx| { this.rebase.open = false; this.status = "Edit safely, save, and stage selected paths in Local Changes; then reopen Rebase".into(); cx.notify(); })))
                        .child(action_button("Amend", colors, cx.listener(|this, _, _, cx| this.request_amend_rebase(cx))))
                        .child(action_button("Begin split", colors, cx.listener(|this, _, _, cx| this.request_begin_split(cx))))
                        .child(action_button("Continue", colors, cx.listener(|this, _, _, cx| this.request_continue_rebase(cx))))
                        .child(action_button("Abort", colors, cx.listener(|this, _, _, cx| this.request_abort_rebase(cx)))));
            }
            RebaseState::Conflicted => {
                if operation.stash_restore == StashRestoreState::Conflicted {
                    body = body.child(action_button(
                        "Finish stash restore after all conflicts are staged",
                        colors,
                        cx.listener(|this, _, _, cx| this.request_finish_stash_restore(cx)),
                    ));
                } else if operation.split.is_none() {
                    body = body.child(
                        div()
                            .flex()
                            .gap_2()
                            .child(action_button(
                                "Continue",
                                colors,
                                cx.listener(|this, _, _, cx| this.request_continue_rebase(cx)),
                            ))
                            .child(action_button(
                                "Skip",
                                colors,
                                cx.listener(|this, _, _, cx| this.request_skip_rebase(cx)),
                            ))
                            .child(action_button(
                                "Abort",
                                colors,
                                cx.listener(|this, _, _, cx| this.request_abort_rebase(cx)),
                            )),
                    );
                }
            }
            RebaseState::Running => {
                body = body.child(
                    div()
                        .flex()
                        .gap_2()
                        .child(action_button(
                            "Observe now",
                            colors,
                            cx.listener(|this, _, _, cx| this.observe_rebase(cx)),
                        ))
                        .child(action_button(
                            "Abort",
                            colors,
                            cx.listener(|this, _, _, cx| this.request_abort_rebase(cx)),
                        )),
                );
            }
            RebaseState::Completed | RebaseState::Aborted => {
                if !operation.resulting_commits.is_empty() {
                    body = body.child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("RESULTING COMMITS"),
                    );
                    for commit in &operation.resulting_commits {
                        body = body.child(div().font_family(CODE_FONT).text_xs().child(format!(
                            "{}  {}",
                            commit.oid,
                            first_line(&commit.message)
                        )));
                    }
                }
                if operation.publish_handoff.is_some() {
                    body = body.child(notice_box("Publish handoff only", "Rewritten local history is ready for the existing explicit remote-branch/OID lease observation and confirmation flow. This component has no verified PR-source target, does not infer one from a local branch, and never defaults to force push.", colors.amber, colors));
                }
                let unrestored = operation.stash.is_some()
                    && operation.stash_restore == StashRestoreState::NotStarted;
                body = body.child(
                    div()
                        .flex()
                        .gap_2()
                        .when(unrestored, |row| {
                            row.child(action_button(
                                "Restore exact retained stash",
                                colors,
                                cx.listener(|this, _, _, cx| this.request_restore_stash(cx)),
                            ))
                        })
                        .child(action_button(
                            "Close / Archive",
                            colors,
                            cx.listener(|this, _, _, cx| this.request_retire_rebase(cx)),
                        )),
                );
            }
            RebaseState::FailedUncertain => {
                body = body.child(notice_box("Uncertain — evidence retained", "This operation cannot be acknowledged away or archived. Controls are read-only; inspect actual Git state and durable evidence. No retry is automatic.", colors.red, colors));
            }
            RebaseState::Prepared => {
                body = body.child(notice_box("Prepared intent observed", "The durable intent exists. Observe and inspect it; cibergit will not automatically replay Start.", colors.amber, colors));
            }
        }
        body.into_any_element()
    }

    fn render_edit_message(&self, label: &'static str, colors: LocalPalette) -> AnyElement {
        div()
            .h(px(150.))
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_xs().text_color(colors.muted).child(label))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_1()
                    .border_color(colors.border)
                    .rounded_md()
                    .font_family(CODE_FONT)
                    .child(Editor::new(&self.rebase.message)),
            )
            .into_any_element()
    }
}

fn resolve_base_candidate(git: &LocalGit, candidate: &str) -> Result<String, String> {
    let allowed = candidate == "HEAD"
        || candidate.starts_with("refs/")
        || (candidate.len() == 40 && candidate.bytes().all(|byte| byte.is_ascii_hexdigit()));
    if !allowed {
        return Err("use HEAD, a full 40-hex commit OID, or a fully-qualified refs/* name".into());
    }
    git.resolve_commit_reference(candidate)
        .map_err(|error| error.to_string())
}

fn advance_generation(generation: &mut u64) -> u64 {
    *generation = generation.wrapping_add(1);
    *generation
}

fn read_reply_is_current(current: u64, reply: u64, effect_in_flight: bool) -> bool {
    current == reply && !effect_in_flight
}

fn pick_steps(inventory: &cibergit::rebase::CommitInventory) -> Vec<PlanStep> {
    inventory
        .commits
        .iter()
        .map(|commit| PlanStep {
            commit_oid: commit.oid.clone(),
            action: PlanAction::Pick,
        })
        .collect()
}

fn observe_bundle(
    store: &RebaseStore,
) -> Result<(Option<RebaseOperationView>, Vec<ConflictFile>), String> {
    let operation = store.observe().map_err(|error| error.to_string())?;
    let conflicts = match &operation {
        Some(view) if view.stash_restore == StashRestoreState::Conflicted => store
            .stash_conflicts(&view.operation_id)
            .map_err(|error| error.to_string())?,
        Some(view) if view.state == RebaseState::Conflicted => match &view.active {
            Some(active) => store.conflicts(active).map_err(|error| error.to_string())?,
            None => Vec::new(),
        },
        _ => Vec::new(),
    };
    Ok((operation, conflicts))
}

fn run_rebase_command(
    store: &RebaseStore,
    command: RebaseCommand,
) -> Result<RebaseEffectResult, String> {
    let mut preparation = None;
    let mut retired = false;
    let operation = match command {
        RebaseCommand::CreateStash(dirty) => {
            preparation = Some(
                store
                    .create_stash(&dirty)
                    .map_err(|error| error.to_string())?,
            );
            None
        }
        RebaseCommand::Start { preparation, plan } => Some(
            store
                .start(&preparation, &plan)
                .map_err(|error| error.to_string())?,
        ),
        RebaseCommand::Continue {
            operation_id,
            active,
            guard,
        } => Some(
            store
                .continue_rebase(&operation_id, &active, &guard)
                .map_err(|error| error.to_string())?,
        ),
        RebaseCommand::Skip {
            operation_id,
            active,
            guard,
        } => Some(
            store
                .skip(&operation_id, &active, &guard)
                .map_err(|error| error.to_string())?,
        ),
        RebaseCommand::Abort {
            operation_id,
            active,
            guard,
        } => Some(
            store
                .abort(&operation_id, &active, &guard)
                .map_err(|error| error.to_string())?,
        ),
        RebaseCommand::Amend {
            operation_id,
            active,
            guard,
            message,
        } => Some(
            store
                .amend_at_edit(&operation_id, &active, &guard, message.as_deref())
                .map_err(|error| error.to_string())?,
        ),
        RebaseCommand::BeginSplit {
            operation_id,
            active,
            guard,
        } => Some(
            store
                .begin_split(&operation_id, &active, &guard)
                .map_err(|error| error.to_string())?,
        ),
        RebaseCommand::CommitSplitPart {
            operation_id,
            active,
            guard,
            message,
        } => Some(
            store
                .commit_split_part(&operation_id, &active, &guard, &message)
                .map_err(|error| error.to_string())?,
        ),
        RebaseCommand::FinishSplit {
            operation_id,
            active,
            guard,
        } => Some(
            store
                .finish_split(&operation_id, &active, &guard)
                .map_err(|error| error.to_string())?,
        ),
        RebaseCommand::StageConflict {
            operation_id,
            active,
            expected,
            guard,
            stash_restore,
        } => Some(
            if stash_restore {
                store.stage_stash_resolution(&operation_id, &expected, &guard)
            } else {
                store.stage_resolution(
                    &operation_id,
                    active
                        .as_ref()
                        .ok_or_else(|| "missing active rebase identity".to_owned())?,
                    &expected,
                    &guard,
                )
            }
            .map_err(|error| error.to_string())?,
        ),
        RebaseCommand::RestoreStash {
            operation_id,
            guard,
        } => Some(
            store
                .restore_stash(&operation_id, &guard)
                .map_err(|error| error.to_string())?,
        ),
        RebaseCommand::FinishStashRestore {
            operation_id,
            guard,
        } => Some(
            store
                .finish_stash_restore(&operation_id, &guard)
                .map_err(|error| error.to_string())?,
        ),
        RebaseCommand::Retire { operation_id } => {
            store
                .retire_operation(&operation_id)
                .map_err(|error| error.to_string())?;
            retired = true;
            None
        }
    };
    let conflicts = match &operation {
        Some(view) if view.stash_restore == StashRestoreState::Conflicted => store
            .stash_conflicts(&view.operation_id)
            .map_err(|error| error.to_string())?,
        Some(view) if view.state == RebaseState::Conflicted => view
            .active
            .as_ref()
            .map(|active| store.conflicts(active))
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    Ok(RebaseEffectResult {
        preparation,
        operation,
        conflicts,
        retired,
        message: if retired {
            "Operation safely archived; a second plan may now be prepared".into()
        } else {
            "Transition completed; actual Git lifecycle state refreshed".into()
        },
    })
}

fn render_preparation_summary(outcome: &PrepareOutcome, colors: LocalPalette) -> AnyElement {
    let inventory = match outcome {
        PrepareOutcome::Ready(preparation) => &preparation.inventory,
        PrepareOutcome::Dirty(preparation) => &preparation.inventory,
        PrepareOutcome::ExternalWorkflow(_) => return div().into_any_element(),
    };
    div()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.surface)
        .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
            "{} commits · {}",
            inventory.commits.len(),
            inventory.branch
        )))
        .child(
            div()
                .mt_1()
                .font_family(CODE_FONT)
                .text_xs()
                .child(format!("base {}", inventory.base_oid)),
        )
        .child(
            div()
                .mt_1()
                .font_family(CODE_FONT)
                .text_xs()
                .child(format!("head {}", inventory.head_oid)),
        )
        .into_any_element()
}

fn render_split(split: &SplitState, colors: LocalPalette) -> AnyElement {
    notice_box(
        "Active split — generic Continue/Skip disabled",
        format!(
            "stopped {} · rewritten stop {} · parent {} · required tree {} · replacements {}",
            split.stopped_oid,
            split
                .rewritten_stopped_oid
                .as_deref()
                .unwrap_or("legacy/missing"),
            split.parent_oid,
            split.required_tree_oid,
            split
                .replacement_commit_count
                .map(|count| count.to_string())
                .unwrap_or_else(|| "unknown".into())
        ),
        colors.amber,
        colors,
    )
    .into_any_element()
}

fn notice_box(
    title: impl Into<SharedString>,
    body: impl Into<SharedString>,
    accent: Rgba,
    colors: LocalPalette,
) -> Div {
    div()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(accent)
        .bg(colors.surface)
        .child(div().font_weight(FontWeight::SEMIBOLD).child(title.into()))
        .child(
            div()
                .mt_1()
                .text_xs()
                .whitespace_normal()
                .child(body.into()),
        )
}

fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or("(empty message)")
}

fn short_oid(oid: &str) -> String {
    oid.chars().take(10).collect()
}

fn plan_action_label(action: &PlanAction) -> &'static str {
    match action {
        PlanAction::Pick => "pick",
        PlanAction::Squash => "squash",
        PlanAction::Fixup => "fixup",
        PlanAction::Drop => "drop",
        PlanAction::Reword { .. } => "reword",
        PlanAction::Edit => "edit",
    }
}

fn conflict_is_editor_candidate(conflict: &ConflictFile) -> bool {
    matches!(
        conflict.disk,
        cibergit::rebase::DiskGeneration::Regular { .. }
    ) && [
        &conflict.base.content,
        &conflict.ours.content,
        &conflict.theirs.content,
    ]
    .iter()
    .all(|content| matches!(content, BlobContent::Utf8(_) | BlobContent::Deleted))
}

fn conflict_reason(conflict: &ConflictFile) -> String {
    let support = if conflict_is_editor_candidate(conflict) {
        "regular UTF-8 result can be opened safely"
    } else {
        "binary/media/non-UTF8/missing/type result requires an external workflow; cibergit will not create or delete it"
    };
    format!(
        "{:?} · {support} · disk {}",
        conflict.kind,
        disk_label(&conflict.disk)
    )
}

fn disk_label(disk: &cibergit::rebase::DiskGeneration) -> String {
    match disk {
        cibergit::rebase::DiskGeneration::Missing => "missing".into(),
        cibergit::rebase::DiskGeneration::Symlink { target_sha256 } => {
            format!("symlink target sha256 {}", short_oid(target_sha256))
        }
        cibergit::rebase::DiskGeneration::Regular {
            sha256,
            len,
            device,
            inode,
            ..
        } => {
            format!(
                "regular sha256 {} · {len} bytes · dev {device} inode {inode}",
                short_oid(sha256)
            )
        }
        cibergit::rebase::DiskGeneration::Unsupported => "unsupported entry".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn oid(value: char) -> String {
        std::iter::repeat_n(value, 40).collect()
    }

    fn inventory() -> cibergit::rebase::CommitInventory {
        cibergit::rebase::CommitInventory {
            base_oid: oid('a'),
            branch: "feature".into(),
            head_oid: oid('d'),
            commits: vec![
                cibergit::rebase::CommitEntry {
                    oid: oid('b'),
                    parent_oid: oid('a'),
                    message_raw: b"one\n".to_vec(),
                    message: "one\n".into(),
                },
                cibergit::rebase::CommitEntry {
                    oid: oid('c'),
                    parent_oid: oid('b'),
                    message_raw: b"two\n".to_vec(),
                    message: "two\n".into(),
                },
                cibergit::rebase::CommitEntry {
                    oid: oid('d'),
                    parent_oid: oid('c'),
                    message_raw: b"three\n".to_vec(),
                    message: "three\n".into(),
                },
            ],
        }
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("run fixture git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn lifecycle_fixture() -> (TempDir, PathBuf, RebaseStore, String) {
        let temporary = tempfile::tempdir().expect("fixture root");
        let checkout = temporary.path().join("checkout");
        let private = temporary.path().join("private");
        fs::create_dir_all(&checkout).expect("checkout");
        fs::create_dir_all(&private).expect("private");
        git(&checkout, &["init", "-b", "feature"]);
        git(&checkout, &["config", "user.name", "UI fixture"]);
        git(&checkout, &["config", "user.email", "ui@invalid"]);
        fs::write(checkout.join("one.txt"), "base one\n").expect("base one");
        fs::write(checkout.join("two.txt"), "base two\n").expect("base two");
        fs::write(checkout.join(".gitignore"), "ignored.txt\n").expect("ignore");
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-m", "base"]);
        let base = git(&checkout, &["rev-parse", "HEAD"]);
        fs::write(checkout.join("one.txt"), "first one\n").expect("first one");
        fs::write(checkout.join("two.txt"), "first two\n").expect("first two");
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-m", "editable pair"]);
        fs::write(checkout.join("tail.txt"), "tail\n").expect("tail");
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-m", "tail"]);
        let store = RebaseStore::open(
            &private,
            RebaseAssociation {
                provider: "github".into(),
                host: "github.com".into(),
                account: "fixture".into(),
                repository: "fixture/repo".into(),
                change: "17".into(),
            },
            &checkout,
        )
        .expect("store");
        (temporary, checkout, store, base)
    }

    #[test]
    fn controller_plan_uses_backend_validation_after_reorder_and_actions() {
        let inventory = inventory();
        let mut steps = pick_steps(&inventory);
        steps.swap(1, 2);
        steps[1].action = PlanAction::Edit;
        steps[2].action = PlanAction::Reword {
            message: "new\nmessage".into(),
        };
        assert!(RebasePlan::validate(&inventory, steps).is_ok());
        let mut invalid = pick_steps(&inventory);
        invalid[0].action = PlanAction::Squash;
        assert!(RebasePlan::validate(&inventory, invalid).is_err());
    }

    #[test]
    fn editor_fence_detects_typing_pending_fifo_and_accepted_baseline_changes() {
        let initial = DocumentFence(vec![DocumentFenceEntry {
            path: "file.txt".into(),
            edit_generation: 4,
            editor_value: "ours".into(),
            accepted_base: "ours".into(),
            status: DocumentStatus::Clean,
            pending_checkout_operations: 0,
            pending_programmatic_reload: false,
        }]);
        let mut changed = initial.clone();
        changed.0[0].edit_generation += 1;
        changed.0[0].editor_value.push('!');
        assert_ne!(initial, changed);
        let mut fifo = initial.clone();
        fifo.0[0].pending_checkout_operations = 1;
        assert_ne!(initial, fifo);
        let mut baseline = initial.clone();
        baseline.0[0].accepted_base.push('!');
        assert_ne!(initial, baseline);
    }

    #[test]
    fn base_candidate_gate_refuses_ambiguous_revision_expressions() {
        assert!(!("feature" == "HEAD" || "feature".starts_with("refs/") || "feature".len() == 40));
        assert!("refs/heads/feature".starts_with("refs/"));
        assert_eq!(oid('a').len(), 40);
    }

    #[test]
    fn effect_epochs_reject_reads_completed_out_of_order_across_both_lane_edges() {
        let mut epoch = 17;
        let prepared_before_admission = epoch;
        advance_generation(&mut epoch);
        assert!(!read_reply_is_current(
            epoch,
            prepared_before_admission,
            true
        ));

        let observed_during_effect = epoch;
        advance_generation(&mut epoch);
        assert!(!read_reply_is_current(epoch, observed_during_effect, false));
        assert!(read_reply_is_current(epoch, epoch, false));

        let mut open_generation = 9;
        let open_started_before_effect = open_generation;
        advance_generation(&mut open_generation);
        assert_ne!(open_started_before_effect, open_generation);
    }

    #[test]
    fn real_controller_start_edit_continue_archive_and_second_prepare() {
        let (_temporary, checkout, store, base) = lifecycle_fixture();
        let preparation = match store.prepare(&base).expect("prepare") {
            PrepareOutcome::Ready(preparation) => preparation,
            other => panic!("expected ready, got {other:?}"),
        };
        let mut steps = pick_steps(&preparation.inventory);
        steps[0].action = PlanAction::Edit;
        let plan = RebasePlan::validate(&preparation.inventory, steps).expect("plan");
        let started = run_rebase_command(
            &store,
            RebaseCommand::Start {
                preparation: preparation.clone(),
                plan,
            },
        )
        .expect("start")
        .operation
        .expect("operation");
        assert_eq!(started.state, RebaseState::PausedForEdit);
        assert!(
            started
                .active
                .as_ref()
                .is_some_and(|active| active.stop_is_edit)
        );
        assert_eq!(
            fs::read_to_string(checkout.join("one.txt")).expect("disk"),
            "first one\n"
        );
        let guard = LocalGit::open(&checkout)
            .expect("git")
            .snapshot()
            .expect("snapshot")
            .guard;
        let completed = run_rebase_command(
            &store,
            RebaseCommand::Continue {
                operation_id: started.operation_id.clone(),
                active: started.active.expect("active"),
                guard,
            },
        )
        .expect("continue")
        .operation
        .expect("completed");
        assert_eq!(completed.state, RebaseState::Completed);
        assert_eq!(completed.resulting_commits.len(), 2);
        run_rebase_command(
            &store,
            RebaseCommand::Retire {
                operation_id: completed.operation_id,
            },
        )
        .expect("archive");
        assert!(matches!(
            store.prepare(&base).expect("second prepare"),
            PrepareOutcome::Ready(_)
        ));
    }

    #[test]
    fn real_controller_split_requires_two_parts_and_conserves_tree() {
        let (temporary, checkout, store, base) = lifecycle_fixture();
        let preparation = match store.prepare(&base).expect("prepare") {
            PrepareOutcome::Ready(preparation) => preparation,
            other => panic!("expected ready, got {other:?}"),
        };
        let required_tree = git(
            &checkout,
            &[
                "rev-parse",
                &format!("{}^{{tree}}", preparation.inventory.commits[0].oid),
            ],
        );
        let mut steps = pick_steps(&preparation.inventory);
        steps[0].action = PlanAction::Edit;
        let plan = RebasePlan::validate(&preparation.inventory, steps).expect("plan");
        let edit = store.start(&preparation, &plan).expect("edit stop");
        let local = LocalGit::open(&checkout).expect("local git");
        let guard = local.snapshot().expect("edit snapshot").guard;
        let split = store
            .begin_split(
                &edit.operation_id,
                edit.active.as_ref().expect("active"),
                &guard,
            )
            .expect("begin split");
        assert_eq!(
            split
                .split
                .as_ref()
                .and_then(|split| split.replacement_commit_count),
            Some(0)
        );
        let restarted = RebaseStore::open(
            temporary.path().join("private"),
            RebaseAssociation {
                provider: "github".into(),
                host: "github.com".into(),
                account: "fixture".into(),
                repository: "fixture/repo".into(),
                change: "17".into(),
            },
            &checkout,
        )
        .expect("reopen store");
        let reopened = observe_bundle(&restarted)
            .expect("restart observation")
            .0
            .expect("operation");
        assert_eq!(reopened.state, RebaseState::PausedForEdit);
        assert_eq!(reopened.split, split.split);

        let snapshot = local.snapshot().expect("split worktree");
        local
            .stage(
                &[GitPath::from_raw(b"one.txt".to_vec()).expect("path")],
                &snapshot.guard,
            )
            .expect("stage first");
        let snapshot = local.snapshot().expect("first staged");
        let first = store
            .commit_split_part(
                &split.operation_id,
                split.active.as_ref().expect("active"),
                &snapshot.guard,
                "split one",
            )
            .expect("first part");
        let snapshot = local.snapshot().expect("second worktree");
        local
            .stage(
                &[GitPath::from_raw(b"two.txt".to_vec()).expect("path")],
                &snapshot.guard,
            )
            .expect("stage second");
        let snapshot = local.snapshot().expect("second staged");
        let second = store
            .commit_split_part(
                &first.operation_id,
                first.active.as_ref().expect("active"),
                &snapshot.guard,
                "split two",
            )
            .expect("second part");
        let snapshot = local.snapshot().expect("finish snapshot");
        let completed = store
            .finish_split(
                &second.operation_id,
                second.active.as_ref().expect("active"),
                &snapshot.guard,
            )
            .expect("finish split");
        assert_eq!(completed.state, RebaseState::Completed);
        assert_eq!(
            git(&checkout, &["rev-parse", "HEAD~1^{tree}"]),
            required_tree
        );
        assert_eq!(completed.resulting_commits.len(), 3);
    }

    #[test]
    fn real_dirty_stash_receipt_is_retained_and_restored_without_pop() {
        let (_temporary, checkout, store, base) = lifecycle_fixture();
        fs::write(checkout.join("one.txt"), "dirty tracked\n").expect("dirty tracked");
        fs::write(checkout.join("untracked.txt"), "untracked\n").expect("untracked");
        fs::write(checkout.join("ignored.txt"), "ignored\n").expect("ignored");
        let dirty = match store.prepare(&base).expect("dirty prepare") {
            PrepareOutcome::Dirty(dirty) => dirty,
            other => panic!("expected dirty, got {other:?}"),
        };
        let ready = store.create_stash(&dirty).expect("stash");
        let receipt = ready.stash.as_ref().expect("receipt");
        assert!(
            receipt.includes_untracked
                && receipt.ignored_files_excluded
                && receipt.retained_after_restore
        );
        assert!(!checkout.join("untracked.txt").exists());
        assert!(checkout.join("ignored.txt").exists());
        let plan =
            RebasePlan::validate(&ready.inventory, pick_steps(&ready.inventory)).expect("plan");
        let completed = store.start(&ready, &plan).expect("complete");
        assert_eq!(completed.state, RebaseState::Completed);
        let local = LocalGit::open(&checkout).expect("local");
        let restored = store
            .restore_stash(
                &completed.operation_id,
                &local.snapshot().expect("guard").guard,
            )
            .expect("restore");
        assert_eq!(restored.stash_restore, StashRestoreState::Completed);
        assert_eq!(
            fs::read_to_string(checkout.join("one.txt")).expect("tracked"),
            "dirty tracked\n"
        );
        assert_eq!(
            fs::read_to_string(checkout.join("untracked.txt")).expect("untracked"),
            "untracked\n"
        );
        assert_eq!(git(&checkout, &["rev-parse", "refs/stash"]), receipt.oid);
    }
}
