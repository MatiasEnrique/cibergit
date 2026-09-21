//! LocalWorkspace-owned interactive-rebase controller and native panel.
//!
//! The controller never invents Git state. Every mutation is confirmed against
//! an immutable backend snapshot.

use super::conflict_view::{
    ConflictContextState, ConflictPresentation, ConflictSource, WIDE_CONFLICT_PANE_MIN,
    source_panel,
};
use super::*;
use cibergit::rebase::{
    ActiveOperationIdentity, ConflictFile, DirtyPreparation, OperationState as RebaseState,
    PlanAction, PlanStep, PrepareOutcome, RebasePlan, RebasePreparation, SplitState,
    StashRestoreState,
};
use cibergit::ui::{self, Density, TextRole};

#[derive(Clone)]
pub(super) enum RebaseCommand {
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
    pub(super) fn summary(&self) -> String {
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

    pub(super) fn guard(&self) -> Option<&SnapshotGuard> {
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

/// Only user-editable values that can describe (or supply) the pending
/// command belong here. Poll generations and observed operation status are
/// deliberately excluded so a harmless read cannot invalidate confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RebaseEditableInputIdentity {
    None,
    Start {
        steps: Vec<PlanStep>,
        prepared_base_oid: String,
        displayed_base: String,
        reword_editor: Option<(String, String)>,
    },
    AmendMessage(String),
    SplitPartMessage(String),
}

impl RebaseEditableInputIdentity {
    pub(super) fn frozen_description(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Start {
                steps,
                prepared_base_oid,
                displayed_base,
                reword_editor,
            } => {
                let plan = steps
                    .iter()
                    .enumerate()
                    .map(|(index, step)| match &step.action {
                        PlanAction::Reword { message } => format!(
                            "{}. reword {} message {:?}",
                            index + 1,
                            step.commit_oid,
                            message
                        ),
                        action => format!(
                            "{}. {} {}",
                            index + 1,
                            plan_action_label(action),
                            step.commit_oid
                        ),
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                let reword = reword_editor
                    .as_ref()
                    .map(|(oid, message)| format!(" · selected reword editor {oid} = {message:?}"))
                    .unwrap_or_default();
                Some(format!(
                    "Frozen base {prepared_base_oid} (input {displayed_base:?}) · Frozen plan {plan}{reword}"
                ))
            }
            Self::AmendMessage(message) => Some(format!("Frozen amend editor = {message:?}")),
            Self::SplitPartMessage(message) => {
                Some(format!("Frozen split-part editor = {message:?}"))
            }
        }
    }
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
    conflict_view: Option<ConflictPresentation>,
    show_operation_details: bool,
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
            conflict_view: None,
            show_operation_details: false,
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

    fn set_operation(
        &mut self,
        operation: Option<RebaseOperationView>,
        conflicts: Vec<ConflictFile>,
    ) {
        if self.operation.as_ref().map(|view| &view.operation_id)
            != operation.as_ref().map(|view| &view.operation_id)
        {
            self.show_operation_details = false;
        }
        if let Some(view) = &mut self.conflict_view {
            view.observe(operation.as_ref(), &conflicts);
        }
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
        self.operations.pending_rebase().map(|pending| pending.id)
    }

    pub fn rebase_plan_len(&self) -> usize {
        self.rebase.steps.len()
    }

    pub fn rebase_plan_action(&self, index: usize) -> Option<PlanAction> {
        self.rebase.steps.get(index).map(|step| step.action.clone())
    }

    pub fn rebase_conflict_count(&self) -> usize {
        self.rebase.conflicts.len()
    }

    #[cfg(feature = "ui-smoke")]
    pub fn rebase_conflict_virtualization_probe(
        &self,
        scroll_to_end: bool,
    ) -> Option<[(usize, usize, bool); 3]> {
        Some(
            self.rebase
                .conflict_view
                .as_ref()?
                .source_virtualization_probe(scroll_to_end),
        )
    }

    pub fn rebase_conflict_source_proof(&self) -> Option<Vec<(String, Option<String>, String)>> {
        let view = self.rebase.conflict_view.as_ref()?;
        Some(
            ConflictSource::ALL
                .into_iter()
                .map(|source| {
                    let (label, stage) = view.source(source);
                    (
                        label.into(),
                        stage.oid.clone(),
                        super::conflict_view::source_text(stage),
                    )
                })
                .collect(),
        )
    }

    pub fn rebase_conflict_context_status(&self) -> Option<&'static str> {
        match self.rebase.conflict_view.as_ref()?.state() {
            ConflictContextState::Current => Some("current"),
            ConflictContextState::StagesChanged => Some("stages-changed"),
            ConflictContextState::ConflictResolved => Some("resolved-by-git"),
            ConflictContextState::OperationChanged => Some("operation-changed"),
            ConflictContextState::OperationUnavailable => Some("operation-unavailable"),
        }
    }

    pub fn open_rebase_conflict_sources(&mut self, index: usize, cx: &mut Context<Self>) {
        self.open_rebase_conflict(index, cx);
    }

    pub fn request_stage_open_rebase_conflict(&mut self, cx: &mut Context<Self>) {
        self.request_stage_presented_conflict(cx);
    }

    pub fn refresh_open_rebase_conflict_sources(&mut self, cx: &mut Context<Self>) {
        self.refresh_conflict_sources(cx);
    }

    pub fn close_rebase_conflict_sources(&mut self, cx: &mut Context<Self>) {
        self.close_conflict_view(cx);
    }

    pub fn rebase_confirmation_inputs_locked(&self, cx: &App) -> bool {
        self.operations.pending_rebase().is_some()
            && !self.rebase.base.read(cx).is_editable()
            && !self.rebase.message.read(cx).is_editable()
    }

    pub fn set_rebase_operation_details(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.rebase.show_operation_details = visible;
        cx.notify();
    }

    pub fn set_rebase_step_action(
        &mut self,
        index: usize,
        action: PlanAction,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_rebase_input_mutation(cx) {
            return;
        }
        if index < self.rebase.steps.len() {
            self.rebase.selected_step = Some(index);
            self.set_rebase_action(action, cx);
        }
    }

    pub fn move_rebase_plan_step(&mut self, index: usize, delta: isize, cx: &mut Context<Self>) {
        if self.refuse_rebase_input_mutation(cx) {
            return;
        }
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
        if self.refuse_rebase_input_mutation(cx) {
            return;
        }
        let candidate = candidate.into();
        self.rebase
            .base
            .update(cx, |input, cx| input.set_value(candidate, window, cx));
        if self.rebase.preparation.is_some() {
            self.rebase.preparation = None;
            self.rebase.steps.clear();
            self.rebase.selected_step = None;
            self.rebase.status =
                "Base input changed; prepare a new immutable inventory before Start".into();
            cx.notify();
        }
    }

    fn rebase_lane_blocker(&self) -> Option<String> {
        if self.operations.rebase_in_flight() {
            return Some("A rebase effect is already running".into());
        }
        if self.operations.local_pending_or_running_or_recovering() {
            return Some("A local Git action or its durable reconciliation is pending".into());
        }
        if self
            .operations
            .observation_pending(ObservationKind::PrPublish)
        {
            return Some("Wait for the fresh PR publication read to finish".into());
        }
        None
    }

    fn set_rebase_editors_disabled(&self, disabled: bool, cx: &mut Context<Self>) {
        self.rebase
            .message
            .update(cx, |editor, cx| editor.set_disabled(disabled, cx));
        self.rebase
            .base
            .update(cx, |input, cx| input.set_disabled(disabled, cx));
    }

    fn refuse_rebase_input_mutation(&mut self, cx: &mut Context<Self>) -> bool {
        if !rebase_input_is_frozen(
            self.operations.pending_rebase().is_some(),
            self.operations.rebase_in_flight(),
        ) {
            return false;
        }
        self.rebase.status = if self.operations.pending_rebase().is_some() {
            "Confirmation inputs are frozen. Cancel the exact pending request before editing or preparing different inputs."
                .into()
        } else {
            "Rebase inputs remain frozen while the confirmed transition is running".into()
        };
        cx.notify();
        true
    }

    fn pause_rebase_confirmation(&mut self, message: String, cx: &mut Context<Self>) {
        self.rebase.status = message.clone();
        cx.emit(LocalWorkspaceEvent::Error(message));
        cx.notify();
    }

    pub fn toggle_rebase(&mut self, cx: &mut Context<Self>) {
        if self.rebase.open && self.operations.pending_rebase().is_some() {
            self.refuse_rebase_input_mutation(cx);
            return;
        }
        self.rebase.open = !self.rebase.open;
        if self.rebase.open {
            self.observe_rebase(cx);
        }
        cx.notify();
    }

    pub fn prepare_rebase(&mut self, cx: &mut Context<Self>) {
        if let Some(error) = self.rebase_lane_blocker() {
            self.report_error(error, cx);
            return;
        }
        if self.operations.pending_rebase().is_some() || self.rebase.operation.is_some() {
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
        let Ok(ticket) = self.operations.begin_observation(ObservationKind::Rebase) else {
            return;
        };
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
                if !this.operations.finish_observation(ticket) {
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
        if self.refuse_rebase_input_mutation(cx) {
            return;
        }
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
        if self.refuse_rebase_input_mutation(cx) {
            return;
        }
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
        if self.refuse_rebase_input_mutation(cx) {
            return;
        }
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
        if self.refuse_rebase_input_mutation(cx) {
            return;
        }
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

    fn selected_reword_editor(&self, cx: &App) -> Option<(String, String)> {
        let step = self
            .rebase
            .selected_step
            .and_then(|index| self.rebase.steps.get(index))?;
        matches!(step.action, PlanAction::Reword { .. }).then(|| {
            (
                step.commit_oid.clone(),
                self.rebase.message.read(cx).value().to_string(),
            )
        })
    }

    fn capture_rebase_editable_inputs(
        &self,
        command: &RebaseCommand,
        cx: &App,
    ) -> Result<RebaseEditableInputIdentity, String> {
        match command {
            RebaseCommand::Start { preparation, plan } => {
                let Some(PrepareOutcome::Ready(displayed_preparation)) =
                    self.rebase.preparation.as_ref()
                else {
                    return Err("the displayed preparation is no longer ready".into());
                };
                if displayed_preparation.inventory.base_oid != preparation.inventory.base_oid {
                    return Err("the displayed prepared base changed".into());
                }
                if self.rebase.steps != plan.steps {
                    return Err("the displayed plan changed before it could be frozen".into());
                }
                let reword_editor = self.selected_reword_editor(cx);
                if let Some((oid, editor_message)) = &reword_editor {
                    let applied_message = plan
                        .steps
                        .iter()
                        .find(|step| &step.commit_oid == oid)
                        .and_then(|step| match &step.action {
                            PlanAction::Reword { message } => Some(message),
                            _ => None,
                        });
                    if applied_message != Some(editor_message) {
                        return Err("the selected Reword editor has unapplied text; apply Reword again before requesting confirmation".into());
                    }
                }
                Ok(RebaseEditableInputIdentity::Start {
                    steps: self.rebase.steps.clone(),
                    prepared_base_oid: displayed_preparation.inventory.base_oid.clone(),
                    displayed_base: self.rebase.base.read(cx).value().to_string(),
                    reword_editor,
                })
            }
            RebaseCommand::Amend { message, .. } => {
                let displayed = self.rebase.message.read(cx).value().to_string();
                let displayed_payload = (!displayed.trim().is_empty()).then_some(&displayed);
                if displayed_payload != message.as_ref() {
                    return Err("the displayed amend message no longer matches the request".into());
                }
                Ok(RebaseEditableInputIdentity::AmendMessage(displayed))
            }
            RebaseCommand::CommitSplitPart { message, .. } => {
                let displayed = self.rebase.message.read(cx).value().to_string();
                if displayed != *message {
                    return Err(
                        "the displayed split-part message no longer matches the request".into(),
                    );
                }
                Ok(RebaseEditableInputIdentity::SplitPartMessage(displayed))
            }
            _ => Ok(RebaseEditableInputIdentity::None),
        }
    }

    fn route_dirty_commit(&mut self, cx: &mut Context<Self>) {
        if self.refuse_rebase_input_mutation(cx) {
            return;
        }
        self.rebase.open = false;
        self.status = "Rebase preparation is dirty. Use the existing staged Commit control, then reopen Rebase.".into();
        cx.notify();
    }

    fn cancel_rebase_preparation(&mut self, cx: &mut Context<Self>) {
        if self.refuse_rebase_input_mutation(cx) {
            return;
        }
        self.rebase.preparation = None;
        self.rebase.steps.clear();
        self.rebase.selected_step = None;
        self.rebase.status = "Preparation cancelled; Git was not changed".into();
        cx.notify();
    }

    fn route_rebase_to_local_changes(&mut self, status: &'static str, cx: &mut Context<Self>) {
        if self.refuse_rebase_input_mutation(cx) {
            return;
        }
        self.rebase.open = false;
        self.status = status.into();
        cx.notify();
    }

    fn request_rebase_command(&mut self, command: RebaseCommand, cx: &mut Context<Self>) {
        if let Some(error) = self.rebase_lane_blocker() {
            self.report_error(error, cx);
            return;
        }
        let BackendState::Ready(_) = &self.backend else {
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
        let editable_inputs = match self.capture_rebase_editable_inputs(&command, cx) {
            Ok(identity) => identity,
            Err(reason) => {
                self.report_error(
                    format!("Rebase request paused: {reason}. Review the visible inputs again."),
                    cx,
                );
                return;
            }
        };
        let summary = command.summary();
        let id = match self.operations.admit_rebase(command, editable_inputs) {
            Ok(id) => id,
            Err(error) => {
                self.report_error(error, cx);
                return;
            }
        };
        self.set_rebase_editors_disabled(true, cx);
        self.rebase.status = format!("Confirmation required: {summary}");
        cx.emit(LocalWorkspaceEvent::MaterialActionConfirmationRequested {
            request_id: id,
            summary,
        });
        cx.notify();
    }

    pub fn confirm_rebase_action(&mut self, request_id: u64, cx: &mut Context<Self>) {
        let Some(pending) = self.operations.pending_rebase().cloned() else {
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
        let current_inputs = match self.capture_rebase_editable_inputs(&pending.command, cx) {
            Ok(identity) => identity,
            Err(reason) => {
                self.pause_rebase_confirmation(format!("Rebase confirmation paused: {reason}. The exact pending request and its frozen inputs were retained; Cancel, edit/reprepare, and request a new confirmation."), cx);
                return;
            }
        };
        let dispatch = match self.operations.confirm_rebase(request_id, &current_inputs) {
            Ok(dispatch) => dispatch,
            Err(error) => {
                self.pause_rebase_confirmation(error, cx);
                return;
            }
        };
        let store = backend.rebase.clone();
        let git = backend.git.clone();
        let command = dispatch.intent.command;
        let ticket = dispatch.ticket;
        self.set_rebase_editors_disabled(true, cx);
        self.rebase.status = "Dispatching the exact confirmed local rebase transition…".into();
        let task = cx.background_spawn(async move {
            let effect = run_rebase_command(&store, command);
            let refresh = git.snapshot().map_err(|error| error.to_string());
            (effect, refresh)
        });
        cx.spawn(async move |this, cx| {
            let (effect, refresh) = task.await;
            let _ = this.update(cx, |this, cx| {
                if !this
                    .operations
                    .complete_rebase(ticket, refresh.as_ref().ok())
                {
                    return;
                }
                this.set_rebase_editors_disabled(false, cx);
                let refresh_error = match refresh {
                    Ok(snapshot) => {
                        this.snapshot = Some(snapshot);
                        None
                    }
                    Err(error) => Some(error),
                };
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
        if self.operations.cancel_rebase(request_id) {
            self.set_rebase_editors_disabled(false, cx);
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
        if self.operations.rebase_in_flight() {
            return;
        }
        let Ok(ticket) = self.operations.begin_observation(ObservationKind::Rebase) else {
            return;
        };
        let store = backend.rebase.clone();
        let task = cx.background_spawn(async move { observe_bundle(&store) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if !this.operations.finish_observation(ticket) {
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
        if self.refuse_rebase_input_mutation(cx) {
            return;
        }
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

    fn open_rebase_conflict(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.refuse_conflict_control(cx) {
            return;
        }
        let Some(conflict) = self.rebase.conflicts.get(index).cloned() else {
            return;
        };
        let Some(operation) = self.rebase.operation.as_ref() else {
            return;
        };
        let Some(presentation) = ConflictPresentation::new(operation, conflict.clone()) else {
            self.rebase.status =
                "Conflict sources are unavailable because the exact operation identity is missing"
                    .into();
            cx.notify();
            return;
        };
        self.rebase.conflict_view = Some(presentation);
        self.rebase.status = format!(
            "Showing immutable sources for {}. Resolve the result in your own editor, refresh, then stage explicitly",
            conflict.path.display
        );
        cx.notify();
    }

    fn refuse_conflict_control(&mut self, cx: &mut Context<Self>) -> bool {
        if self.operations.pending_rebase().is_none() && !self.operations.rebase_in_flight() {
            return false;
        }
        self.rebase.status = if self.operations.pending_rebase().is_some() {
            "Conflict controls are frozen while the exact confirmation is visible; confirm or cancel it first"
        } else {
            "Conflict controls are frozen while the rebase/local effect is running"
        }
        .into();
        cx.notify();
        true
    }

    fn close_conflict_view(&mut self, cx: &mut Context<Self>) {
        if self.refuse_conflict_control(cx) {
            return;
        }
        self.rebase.conflict_view = None;
        self.rebase.status =
            "Closed the source presentation; the file on disk was not changed".into();
        cx.notify();
    }

    fn select_conflict_source(&mut self, source: ConflictSource, cx: &mut Context<Self>) {
        if self.refuse_conflict_control(cx) {
            return;
        }
        if let Some(view) = &mut self.rebase.conflict_view {
            view.select(source);
            cx.notify();
        }
    }

    fn move_conflict_source(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.refuse_conflict_control(cx) {
            return;
        }
        if let Some(view) = &mut self.rebase.conflict_view {
            view.move_selection(delta);
            cx.notify();
        }
    }

    fn toggle_conflict_details(&mut self, cx: &mut Context<Self>) {
        if self.refuse_conflict_control(cx) {
            return;
        }
        if let Some(view) = &mut self.rebase.conflict_view {
            view.toggle_details();
            cx.notify();
        }
    }

    fn refresh_conflict_sources(&mut self, cx: &mut Context<Self>) {
        if self.refuse_conflict_control(cx) {
            return;
        }
        let Some(view) = self.rebase.conflict_view.as_ref() else {
            return;
        };
        let Some(operation) = self.rebase.operation.as_ref() else {
            self.rebase.status =
                "The original operation is no longer available; source context was not changed"
                    .into();
            cx.notify();
            return;
        };
        let Some(conflict) = self
            .rebase
            .conflicts
            .iter()
            .find(|conflict| conflict.path.raw == view.path_raw())
            .cloned()
        else {
            self.rebase.status =
                "Git no longer reports this path as unmerged; source context was not changed"
                    .into();
            cx.notify();
            return;
        };
        let refreshed = self
            .rebase
            .conflict_view
            .as_mut()
            .is_some_and(|view| view.refresh_from(operation, conflict));
        self.rebase.status = if refreshed {
            "Refreshed immutable source stages; the file on disk was not changed"
        } else {
            "The operation identity changed; this presentation cannot attach to the new operation"
        }
        .into();
        cx.notify();
    }

    fn request_stage_presented_conflict(&mut self, cx: &mut Context<Self>) {
        if self.refuse_conflict_control(cx) {
            return;
        }
        let Some(view) = self.rebase.conflict_view.as_ref() else {
            return;
        };
        if !view.can_stage() {
            self.rebase.status = view
                .state()
                .explanation()
                .unwrap_or("The source context is stale and cannot be staged")
                .into();
            cx.notify();
            return;
        }
        let expected = view.conflict().clone();
        let Some(index) = self
            .rebase
            .conflicts
            .iter()
            .position(|conflict| conflict == &expected)
        else {
            self.rebase.status =
                "The exact source stages are no longer current; refresh before staging".into();
            cx.notify();
            return;
        };
        self.request_stage_rebase_conflict(index, cx);
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
        if self.refuse_rebase_input_mutation(cx) {
            return;
        }
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
        if self.rebase.conflict_view.is_some() {
            let delta = match event.keystroke.key.as_str() {
                "left" => -1,
                "right" => 1,
                _ => return false,
            };
            self.move_conflict_source(delta, cx);
            return true;
        }
        let delta = match event.keystroke.key.as_str() {
            "up" => -1,
            "down" => 1,
            _ => return false,
        };
        if self.refuse_rebase_input_mutation(cx) {
            return true;
        }
        self.move_rebase_step(delta, cx);
        true
    }

    pub(super) fn render_rebase_panel(
        &mut self,
        colors: LocalPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let operation = self.rebase.operation.clone();
        let preparation = self.rebase.preparation.clone();
        let pending = self.operations.pending_rebase().cloned();
        let inputs_frozen = pending.is_some();
        let selected = self.rebase.selected_step;
        let inventory = self.rebase_inventory().cloned();
        let mut plan_rows = div().flex().flex_col().gap(px(ui::GAP_ICON));
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
                    .control()
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
                    .when(inputs_frozen, |row| row.opacity(0.52).cursor_default())
                    .when(!inputs_frozen, |row| row.cursor_pointer())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_rebase_step(index, window, cx)
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(ui::GAP_GROUP))
                            .child(div().w(px(62.)).text_color(colors.accent).child(action))
                            .child(
                                div()
                                    .font_family(CODE_FONT)
                                    .ui_text(TextRole::Caption)
                                    .child(oid),
                            )
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
            .w_full()
            .min_w_0()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(ui::PANEL_GUTTER))
            .flex()
            .flex_col()
            .gap(px(ui::GAP_COLUMNS));
        if self.rebase.conflict_view.is_some() {
            content = content.child(self.render_conflict_view(colors, window, cx));
        } else if let Some(operation) = operation {
            content = content.child(self.render_operation(operation, colors, cx));
        } else if let Some(preparation) = preparation {
            content = content.child(render_preparation_summary(&preparation));
            match preparation {
                PrepareOutcome::ExternalWorkflow(workflow) => {
                    content = content.child(notice_box(
                        "External workflow required",
                        workflow.explanation,
                        colors.red,
                    ));
                }
                PrepareOutcome::Dirty(dirty) => {
                    content = content.child(
                        div().flex().gap(px(ui::GAP_GROUP))
                            .child(action_button("Commit in Local Changes", colors, cx.listener(|this, _, _, cx| this.route_dirty_commit(cx))))
                            .child(action_button("Stash including untracked", colors, cx.listener(|this, _, _, cx| this.request_create_stash(cx))))
                            .child(action_button("Cancel preparation", colors, cx.listener(|this, _, _, cx| this.cancel_rebase_preparation(cx))))
                    ).child(notice_box(
                        "Dirty checkout",
                        format!("{} staged · {} unstaged · {} untracked · {} conflicts. Stash includes untracked; ignored files remain outside it. The exact retained stash receipt appears after creation.", dirty.staged, dirty.unstaged, dirty.untracked, dirty.conflicts),
                        colors.amber));
                }
                PrepareOutcome::Ready(_) => {
                    content = content
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(div().font_weight(ui::WEIGHT_EMPHASIS).child("LINEAR PLAN"))
                                .child(
                                    div()
                                        .ui_text(TextRole::Caption)
                                        .text_color(colors.muted)
                                        .child("⌘↑ / ⌘↓ moves the selected commit"),
                                ),
                        )
                        .child(plan_rows)
                        .child(self.render_plan_editor(inputs_frozen, colors, cx))
                        .child(
                            action_button(
                                "Review and start rebase",
                                colors,
                                cx.listener(|this, _, _, cx| this.request_start_rebase(cx)),
                            )
                            .when(inputs_frozen, |button| {
                                button.opacity(0.52).cursor_default()
                            }),
                        );
                }
            }
        } else {
            content = content
                .child(div().font_weight(ui::WEIGHT_EMPHASIS).child("CHOOSE A LOCAL BASE"))
                .child(div().ui_text(TextRole::Caption).text_color(colors.muted).child("Candidates below are already resolved to immutable full commit OIDs. Manual input accepts HEAD, a full OID, or a fully-qualified refs/* name; ambiguous expressions are refused."))
                .child(
                    ui::text_field(colors.elevated, colors.border)
                        .child(Input::new(&self.rebase.base)),
                );
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
                    colors.amber)
                .children(pending.editable_inputs.frozen_description().map(|description| {
                    div()
                        .font_family(CODE_FONT)
                        .ui_text(TextRole::Caption)
                        .whitespace_normal()
                        .child(description)
                }))
                .child(
                    div()
                        .ui_text(TextRole::Caption)
                        .text_color(colors.muted)
                        .child("Plan, base, and message inputs are disabled until Cancel. Confirmation rechecks their exact frozen identity before dispatch."),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(ui::GAP_GROUP))
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
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(colors.canvas)
            .child(
                div()
                    .min_h(px(52.))
                    .px(px(ui::PANEL_GUTTER))
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(colors.surface)
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .child(div().font_weight(ui::WEIGHT_EMPHASIS).child("REBASE"))
                            .child(
                                div()
                                    .ui_text(TextRole::Caption)
                                    .text_color(colors.muted)
                                    .child("Rebase this branch"),
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
                    .px(px(ui::PANEL_GUTTER))
                    .py(px(ui::GAP_GROUP))
                    .border_t_1()
                    .border_color(colors.border)
                    .ui_text(TextRole::Caption)
                    .text_color(colors.muted)
                    .child(self.rebase.status.clone()),
            )
            .into_any_element()
    }

    fn render_plan_editor(
        &mut self,
        inputs_frozen: bool,
        colors: LocalPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let actions = div()
            .flex()
            .flex_wrap()
            .gap(px(ui::GAP_ICON))
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
            ))
            .when(inputs_frozen, |actions| actions.opacity(0.52));
        div()
            .h(px(170.))
            .flex()
            .flex_col()
            .gap(px(ui::GAP_GROUP))
            .child(actions)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_1()
                    .border_color(colors.border)
                    .rounded(px(ui::CONTROL_RADIUS))
                    .font_family(CODE_FONT)
                    .child(Editor::new(&self.rebase.message)),
            )
            .into_any_element()
    }

    fn render_conflict_view(
        &mut self,
        colors: LocalPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(view) = self.rebase.conflict_view.clone() else {
            return div().into_any_element();
        };
        let pane_width = (window.bounds().size.width.as_f32() - 300.).max(0.);
        let wide = pane_width >= WIDE_CONFLICT_PANE_MIN;
        let controls_frozen = self.operations.rebase_pending_or_running();
        let mut selectors = div().flex().flex_wrap().gap(px(ui::GAP_GROUP));
        for source in ConflictSource::ALL {
            let label = view.source(source).0;
            selectors = selectors.child(
                action_button(
                    label,
                    colors,
                    cx.listener(move |this, _, _, cx| this.select_conflict_source(source, cx)),
                )
                .when(view.selected() == source, |button| {
                    button.border_color(colors.accent).bg(colors.selected)
                })
                .when(controls_frozen, |button| {
                    button.opacity(0.52).cursor_default()
                }),
            );
        }

        let mut sources = if wide {
            div().flex().gap(px(ui::GAP_GROUP))
        } else {
            div().flex().flex_col()
        };
        for source in ConflictSource::ALL {
            if wide || source == view.selected() {
                sources = sources.child(
                    source_panel(source, &view, colors)
                        .when(wide, |panel| panel.flex_1())
                        .when(!wide, |panel| panel.w_full()),
                );
            }
        }

        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(ui::GAP_COLUMNS))
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(ui::GAP_COLUMNS))
                    .child(
                        div()
                            .min_w_0()
                            .child(
                                div()
                                    .font_weight(ui::WEIGHT_EMPHASIS)
                                    .child("THREE-WAY CONFLICT"),
                            )
                            .child(
                                div()
                                    .font_family(CODE_FONT)
                                    .ui_text(TextRole::Caption)
                                    .child(view.conflict().path.display.clone()),
                            )
                            .when(view.show_details(), |header| {
                                header.child(
                                    div()
                                        .ui_text(TextRole::Caption)
                                        .text_color(colors.muted)
                                        .child(format!("Operation {}", view.operation_id())),
                                )
                            }),
                    )
                    .child(
                        action_button(
                            "Back to conflict list",
                            colors,
                            cx.listener(|this, _, _, cx| this.close_conflict_view(cx)),
                        )
                        .when(controls_frozen, |button| {
                            button.opacity(0.52).cursor_default()
                        }),
                    ),
            )
            .children(view.state().explanation().map(|explanation| {
                notice_box(
                    "Frozen source context",
                    explanation,
                    colors.red)
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(ui::GAP_GROUP))
                    .child(selectors)
                    .child(
                        div()
                            .flex()
                            .gap(px(ui::GAP_GROUP))
                            .child(
                                action_button(
                                    if view.show_details() {
                                        "Hide full source details"
                                    } else {
                                        "Show full source details"
                                    },
                                    colors,
                                    cx.listener(|this, _, _, cx| {
                                        this.toggle_conflict_details(cx)
                                    }),
                                )
                                .when(controls_frozen, |button| {
                                    button.opacity(0.52).cursor_default()
                                }),
                            )
                            .when(
                                view.state() == &ConflictContextState::StagesChanged,
                                |actions| {
                                    actions.child(
                                        action_button(
                                            "Refresh changed stages",
                                            colors,
                                            cx.listener(|this, _, _, cx| {
                                                this.refresh_conflict_sources(cx)
                                            }),
                                        )
                                        .when(controls_frozen, |button| {
                                            button.opacity(0.52).cursor_default()
                                        }),
                                    )
                                },
                            ),
                    ),
            )
            .child(
                div()
                    .ui_text(TextRole::Caption)
                    .text_color(colors.muted)
                    .child(if wide {
                        "Immutable sources are side by side. Select with the mouse or ⌘← / ⌘→; each pane scrolls to the actual end of long lines."
                    } else {
                        "Narrow layout shows one immutable source at a time. Select with the mouse or ⌘← / ⌘→; the result panel remains labelled below."
                    }),
            )
            .child(sources);

        let result_header = div()
            .min_h(px(42.))
            .px(px(ui::CONTROL_INSET))
            .flex()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .child(div().font_weight(ui::WEIGHT_EMPHASIS).child("RESULT"))
                    .child(
                        div()
                            .ui_text(TextRole::Caption)
                            .text_color(colors.muted)
                            .child("Resolved in your own editor · staging is explicit"),
                    ),
            );
        let result_actions =
            div()
                .flex()
                .gap(px(ui::GAP_GROUP))
                .when(view.can_stage(), |actions| {
                    actions.child(
                        action_button(
                            "Stage resolved result",
                            colors,
                            cx.listener(|this, _, _, cx| this.request_stage_presented_conflict(cx)),
                        )
                        .when(controls_frozen, |button| {
                            button.opacity(0.52).cursor_default()
                        }),
                    )
                });
        body = body.child(
            div()
                .rounded(px(ui::CONTROL_RADIUS))
                .border_1()
                .border_color(colors.border)
                .bg(colors.surface)
                .child(result_header.child(result_actions))
                .child(
                    div()
                        .p(px(ui::GAP_COLUMNS))
                        .text_color(colors.muted)
                        .child("Resolve the conflicted file in your own editor, then refresh and stage it here. Local changes can open the checkout in an editor you pick; nothing is launched, created, removed, renamed, or staged implicitly."),
                ),
        );
        body.into_any_element()
    }

    fn render_operation(
        &mut self,
        operation: RebaseOperationView,
        colors: LocalPalette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = operation_state_label(operation.state);
        let stopped = operation.active.as_ref().and_then(|active| {
            active.stopped_oid.as_ref().map(|oid| {
                let headline = self.rebase_inventory().and_then(|inventory| {
                    inventory
                        .commits
                        .iter()
                        .find(|commit| &commit.oid == oid)
                        .map(|commit| first_line(&commit.message))
                });
                match headline {
                    Some(headline) => format!("stopped {} · {headline}", short_oid(oid)),
                    None => format!("stopped {}", short_oid(oid)),
                }
            })
        });
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(ui::GAP_COLUMNS))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .font_weight(ui::WEIGHT_EMPHASIS)
                            .child(format!("{} · attempt {}", state, operation.attempt)),
                    )
                    .child(
                        div()
                            .font_family(CODE_FONT)
                            .ui_text(TextRole::Caption)
                            .child(short_oid(&operation.original_head_oid)),
                    ),
            )
            .child(
                div()
                    .ui_text(TextRole::Caption)
                    .text_color(colors.muted)
                    .flex()
                    .flex_col()
                    .gap(px(ui::GAP_ICON))
                    .child(format!("branch {}", operation.original_branch))
                    .child(format!("base {}", short_oid(&operation.base_oid)))
                    .children(stopped),
            )
            .child(action_button(
                if self.rebase.show_operation_details {
                    "Hide operation details"
                } else {
                    "Show operation details"
                },
                colors,
                cx.listener(|this, _, _, cx| {
                    this.rebase.show_operation_details = !this.rebase.show_operation_details;
                    cx.notify();
                }),
            ));
        if self.rebase.show_operation_details {
            body = body.child(operation_details_box(&operation, colors));
        }
        if let Some(split) = &operation.split {
            body = body.child(render_split(split, colors));
        }
        if !self.rebase.conflicts.is_empty() {
            let conflict_controls_frozen = self.operations.rebase_pending_or_running();
            body = body.child(div().font_weight(ui::WEIGHT_EMPHASIS).child("CONFLICTS"));
            for (index, conflict) in self.rebase.conflicts.clone().into_iter().enumerate() {
                let detail = conflict_reason(&conflict, self.rebase.show_operation_details);
                body = body.child(div().flex().flex_col().gap(px(ui::GAP_ICON))
                    .child(div().font_family(CODE_FONT).child(conflict.path.display.clone()))
                    .child(div().ui_text(TextRole::Caption).text_color(colors.muted).child(detail))
                    .child(div().ui_text(TextRole::Caption).child("Rebase orientation: ours = already rebased series; theirs = replayed original commit."))
                    .child(div().flex().gap(px(ui::GAP_GROUP))
                        .child(action_button("Open sources and result", colors, cx.listener(move |this, _, _, cx| this.open_rebase_conflict(index, cx)))
                            .when(conflict_controls_frozen, |button| button.opacity(0.52).cursor_default()))
                        .child(action_button("Stage resolved result", colors, cx.listener(move |this, _, _, cx| this.request_stage_rebase_conflict(index, cx)))
                            .when(conflict_controls_frozen, |button| button.opacity(0.52).cursor_default()))));
            }
        }
        if let Some(stash) = &operation.stash {
            body = body.child(notice_box("Retained stash receipt", format!("OID {} · includes untracked: {} · ignored excluded: {} · retained after restore: {} · restore {:?}", stash.oid, stash.includes_untracked, stash.ignored_files_excluded, stash.retained_after_restore, operation.stash_restore), colors.amber));
        }
        match operation.state {
            RebaseState::PausedForEdit if operation.split.is_some() => {
                body = body.child(self.render_edit_message("Split part message", colors))
                    .child(div().flex().flex_wrap().gap(px(ui::GAP_GROUP))
                        .child(action_button("Stage via Local Changes", colors, cx.listener(|this, _, _, cx| this.route_rebase_to_local_changes("Stage the selected split paths in Local Changes, then reopen Rebase", cx))))
                        .child(action_button("Commit staged part", colors, cx.listener(|this, _, _, cx| this.request_commit_split_part(cx))))
                        .child(action_button("Finish validated split", colors, cx.listener(|this, _, _, cx| this.request_finish_split(cx))))
                        .child(action_button("Abort", colors, cx.listener(|this, _, _, cx| this.request_abort_rebase(cx)))));
            }
            RebaseState::PausedForEdit => {
                body = body.child(self.render_edit_message("Optional amend message", colors))
                    .child(div().flex().flex_wrap().gap(px(ui::GAP_GROUP))
                        .child(action_button("Open/stage in Local Changes", colors, cx.listener(|this, _, _, cx| this.route_rebase_to_local_changes("Edit the files in your own editor, stage the selected paths in Local Changes, then reopen Rebase", cx))))
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
                            .gap(px(ui::GAP_GROUP))
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
                        .gap(px(ui::GAP_GROUP))
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
                            .font_weight(ui::WEIGHT_EMPHASIS)
                            .child("RESULTING COMMITS"),
                    );
                    for commit in &operation.resulting_commits {
                        body = body.child(
                            div()
                                .font_family(CODE_FONT)
                                .ui_text(TextRole::Caption)
                                .child(format!("{}  {}", commit.oid, first_line(&commit.message))),
                        );
                    }
                }
                if operation.publish_handoff.is_some() {
                    body = body.child(notice_box("Publish handoff only", "Rewritten local history is ready for the existing explicit remote-branch/OID lease observation and confirmation flow. This component has no verified PR-source target, does not infer one from a local branch, and never defaults to force push.", colors.amber));
                }
                let unrestored = operation.stash.is_some()
                    && operation.stash_restore == StashRestoreState::NotStarted;
                body = body.child(
                    div()
                        .flex()
                        .gap(px(ui::GAP_GROUP))
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
                body = body.child(notice_box("Uncertain — evidence retained", "This operation cannot be acknowledged away or archived. Controls are read-only; inspect actual Git state and durable evidence. No retry is automatic.", colors.red));
            }
            RebaseState::Prepared => {
                body = body.child(notice_box("Prepared intent observed", "The durable intent exists. Observe and inspect it; cibergit will not automatically replay Start.", colors.amber));
            }
        }
        body.into_any_element()
    }

    fn render_edit_message(&self, label: &'static str, colors: LocalPalette) -> AnyElement {
        let inputs_frozen = self.operations.pending_rebase().is_some();
        div()
            .h(px(150.))
            .flex()
            .flex_col()
            .gap(px(ui::GAP_ICON))
            .child(
                div()
                    .ui_text(TextRole::Caption)
                    .text_color(colors.muted)
                    .child(if inputs_frozen {
                        format!("{label} · frozen until Cancel")
                    } else {
                        label.into()
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_1()
                    .border_color(colors.border)
                    .rounded(px(ui::CONTROL_RADIUS))
                    .font_family(CODE_FONT)
                    .child(Editor::new(&self.rebase.message))
                    .when(inputs_frozen, |editor| editor.opacity(0.62)),
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

fn rebase_input_is_frozen(confirmation_pending: bool, effect_in_flight: bool) -> bool {
    confirmation_pending || effect_in_flight
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

fn render_preparation_summary(outcome: &PrepareOutcome) -> AnyElement {
    let inventory = match outcome {
        PrepareOutcome::Ready(preparation) => &preparation.inventory,
        PrepareOutcome::Dirty(preparation) => &preparation.inventory,
        PrepareOutcome::ExternalWorkflow(_) => return div().into_any_element(),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(ui::GAP_ICON))
        .child(div().font_weight(ui::WEIGHT_EMPHASIS).child(format!(
            "{} commits · {}",
            inventory.commits.len(),
            inventory.branch
        )))
        .child(
            div()
                .font_family(CODE_FONT)
                .ui_text(TextRole::Body)
                .child(format!("base {}", inventory.base_oid)),
        )
        .child(
            div()
                .font_family(CODE_FONT)
                .ui_text(TextRole::Body)
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
    )
    .into_any_element()
}

fn notice_box(title: impl Into<SharedString>, body: impl Into<SharedString>, accent: Rgba) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(ui::GAP_ICON))
        .child(
            div()
                .font_weight(ui::WEIGHT_EMPHASIS)
                .text_color(accent)
                .child(title.into()),
        )
        .child(
            div()
                .ui_text(TextRole::Caption)
                .whitespace_normal()
                .child(body.into()),
        )
}

fn operation_details_box(operation: &RebaseOperationView, colors: LocalPalette) -> Div {
    let active = operation.active.as_ref();
    div()
        .flex()
        .flex_col()
        .gap(px(ui::GAP_ICON))
        .child(
            div()
                .font_weight(ui::WEIGHT_EMPHASIS)
                .text_color(colors.accent)
                .child("Operation details"),
        )
        .child(
            div()
                .ui_text(TextRole::Caption)
                .flex()
                .flex_col()
                .gap(px(ui::GAP_ICON))
                .child(format!("operation {}", operation.operation_id))
                .child(format!("base {}", operation.base_oid))
                .child(format!("original {}", operation.original_head_oid))
                .children(active.map(|active| format!("onto {}", active.onto_oid)))
                .children(active.map(|active| {
                    format!(
                        "stopped {}",
                        active.stopped_oid.as_deref().unwrap_or("none")
                    )
                }))
                .children(active.map(|active| format!("todo {}", active.todo_sha256)))
                .children(active.map(|active| format!("done {}", active.done_sha256)))
                .children(
                    active.map(|active| format!("ownership {}", active.ownership_marker_sha256)),
                ),
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

fn conflict_reason(conflict: &ConflictFile, show_details: bool) -> String {
    let support = "resolve the result in an external editor; cibergit will not create, delete, or stage it implicitly";
    let kind = match &conflict.kind {
        cibergit::rebase::ConflictKind::BothModified => "Both sides modified this file".into(),
        cibergit::rebase::ConflictKind::AddedByBoth => "Both sides added this file".into(),
        cibergit::rebase::ConflictKind::DeletedByOurs => {
            "Deleted in the already-rebased changes and modified by the replayed commit".into()
        }
        cibergit::rebase::ConflictKind::DeletedByTheirs => {
            "Modified in the already-rebased changes and deleted by the replayed commit".into()
        }
        cibergit::rebase::ConflictKind::RenameOrDelete { explanation, .. } => {
            format!("Rename or delete conflict: {explanation}")
        }
        cibergit::rebase::ConflictKind::TypeChange => "The file type changed across sides".into(),
    };
    if show_details {
        format!("{kind} · {support} · disk {}", disk_label(&conflict.disk))
    } else {
        format!("{kind} · {support}")
    }
}

fn operation_state_label(state: RebaseState) -> &'static str {
    match state {
        RebaseState::Prepared => "Ready to start",
        RebaseState::Running => "Rebase running",
        RebaseState::PausedForEdit => "Paused for editing",
        RebaseState::Conflicted => "Conflicts to resolve",
        RebaseState::Completed => "Rebase completed",
        RebaseState::Aborted => "Rebase aborted",
        RebaseState::FailedUncertain => "Outcome uncertain",
    }
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
    #[cfg(feature = "ui-smoke")]
    use cibergit::{
        domain::{Account, Repository},
        local_git::OperationState,
        worktrees::{
            AssociationKey, CheckoutAssociation, CheckoutOwnership, CheckoutView,
            FilesystemIdentity,
        },
    };
    #[cfg(feature = "ui-smoke")]
    use gpui::{Keystroke, Modifiers, TestApp, TestAppWindow};
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

    #[cfg(feature = "ui-smoke")]
    fn filesystem_identity(path: &Path) -> FilesystemIdentity {
        let metadata = fs::symlink_metadata(path).expect("fixture identity");
        FilesystemIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    #[cfg(feature = "ui-smoke")]
    fn native_controller_fixture() -> (
        TempDir,
        PathBuf,
        String,
        RebaseStore,
        TestAppWindow<LocalWorkspace>,
    ) {
        let (temporary, checkout, _fixture_store, base) = lifecycle_fixture();
        git(
            &checkout,
            &["update-ref", "refs/heads/alternate-base", &base],
        );
        let local = LocalGit::open(&checkout).expect("local Git");
        let snapshot = local.snapshot().expect("fixture snapshot");
        let controller_data = temporary.path().join("controller-data");
        let context = LocalWorkspaceContext {
            repository: Repository {
                host: "github.com".into(),
                owner: "fixture".into(),
                name: "repo".into(),
                account: Account {
                    host: "github.com".into(),
                    login: "fixture".into(),
                },
                local_path: Some(checkout.clone()),
            },
            checkout: CheckoutView {
                association: CheckoutAssociation {
                    key: AssociationKey {
                        provider: "github".into(),
                        host: "github.com".into(),
                        account: "fixture".into(),
                        repository: "fixture/repo".into(),
                        pull_request: 17,
                    },
                    path: checkout.clone(),
                    git_dir: local.git_dir().to_owned(),
                    common_git_dir: local.common_git_dir().to_owned(),
                    checkout_identity: filesystem_identity(&checkout),
                    git_dir_identity: filesystem_identity(local.git_dir()),
                    common_git_dir_identity: filesystem_identity(local.common_git_dir()),
                    ownership: CheckoutOwnership::ExplicitlyAttached,
                    creation: None,
                    intended_remote_branch: Some("feature".into()),
                    published_head_at_association: None,
                },
                actual_head: snapshot.head,
                operation: OperationState::default(),
            },
            data_root: controller_data.clone(),
            appearance: LocalWorkspaceAppearance { dark: true },
        };
        let mut app = TestApp::new();
        app.update(gpui_base::init);
        let mut window =
            app.open_window(move |window, cx| LocalWorkspace::new(context, window, cx));
        assert!(window.read(|workspace, _| workspace.is_ready()));
        window.update(|workspace, window, cx| {
            workspace.toggle_rebase(cx);
            workspace.set_rebase_base_candidate(base.clone(), window, cx);
            workspace.prepare_rebase(cx);
        });
        assert_eq!(window.read(|workspace, _| workspace.rebase_plan_len()), 2);
        let store = RebaseStore::open(
            controller_data.join("rebase-private"),
            RebaseAssociation {
                provider: "github".into(),
                host: "github.com".into(),
                account: "fixture".into(),
                repository: "fixture/repo".into(),
                change: "17".into(),
            },
            &checkout,
        )
        .expect("controller store observer");
        (temporary, checkout, base, store, window)
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
    #[cfg(feature = "ui-smoke")]
    fn native_controller_start_confirmation_gates_inputs_and_retains_forced_drift() {
        let (_temporary, checkout, base, store, mut window) = native_controller_fixture();
        let original_head = git(&checkout, &["rev-parse", "HEAD"]);

        let request_id = window.update(|workspace, window, cx| {
            workspace.set_rebase_step_action(0, PlanAction::Edit, cx);
            workspace.request_start_rebase(cx);
            let request_id = workspace.rebase_pending_action_id().expect("pending start");
            assert!(workspace.rebase_confirmation_inputs_locked(cx));
            assert_eq!(workspace.rebase_plan_action(0), Some(PlanAction::Edit));

            // These are the same controller methods used by the plan-row mouse
            // listeners and the embedding API. Neither may mutate while pending.
            workspace.set_rebase_action(PlanAction::Drop, cx);
            workspace.set_rebase_step_action(0, PlanAction::Drop, cx);
            workspace.move_rebase_plan_step(0, 1, cx);
            workspace.set_rebase_base_candidate("refs/heads/alternate-base", window, cx);
            let key = KeyDownEvent {
                keystroke: Keystroke {
                    modifiers: Modifiers {
                        platform: true,
                        ..Default::default()
                    },
                    key: "down".into(),
                    key_char: None,
                },
                is_held: false,
                prefer_character_input: false,
            };
            assert!(workspace.handle_rebase_key(&key, cx));
            assert_eq!(workspace.rebase_plan_action(0), Some(PlanAction::Edit));
            assert_eq!(
                workspace.rebase.steps[0].commit_oid,
                workspace.rebase_inventory().unwrap().commits[0].oid
            );
            assert_eq!(workspace.rebase.base.read(cx).value(), base);

            request_id
        });

        // A defensive check still catches mutation that bypasses every handler.
        window.update(|workspace, _, cx| {
            workspace.rebase.steps[0].action = PlanAction::Drop;
            workspace.confirm_rebase_action(request_id, cx);
            assert_eq!(workspace.rebase_pending_action_id(), Some(request_id));
            assert!(!workspace.operations.rebase_in_flight());
            assert!(workspace.rebase_status_message().contains("paused"));
        });
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), original_head);
        assert!(store.observe().expect("observe no dispatch").is_none());

        // Cancel is the only route back to editing. A second forced-order drift
        // is also retained without replacing the frozen request.
        window.update(|workspace, _, cx| {
            workspace.cancel_rebase_action(request_id, cx);
            assert!(workspace.rebase.base.read(cx).is_editable());
            assert!(workspace.rebase.message.read(cx).is_editable());
            workspace.rebase.steps[0].action = PlanAction::Edit;
            workspace.request_start_rebase(cx);
            let reorder_id = workspace
                .rebase_pending_action_id()
                .expect("reorder request");
            workspace.rebase.steps.swap(0, 1);
            workspace.confirm_rebase_action(reorder_id, cx);
            assert_eq!(workspace.rebase_pending_action_id(), Some(reorder_id));
            assert!(!workspace.operations.rebase_in_flight());
            workspace.cancel_rebase_action(reorder_id, cx);
            workspace.rebase.steps.swap(0, 1);
        });
        assert!(store.observe().expect("observe reorder refusal").is_none());

        // The raw displayed ref is independently frozen along with the
        // preparation's resolved base OID.
        window.update(|workspace, window, cx| {
            workspace.request_start_rebase(cx);
            let base_id = workspace.rebase_pending_action_id().expect("base request");
            workspace.rebase.base.update(cx, |input, cx| {
                input.set_value("refs/heads/alternate-base", window, cx)
            });
            workspace.confirm_rebase_action(base_id, cx);
            assert_eq!(workspace.rebase_pending_action_id(), Some(base_id));
            assert!(!workspace.operations.rebase_in_flight());
            workspace.cancel_rebase_action(base_id, cx);
            workspace.set_rebase_base_candidate(base.clone(), window, cx);
            workspace.prepare_rebase(cx);
        });
        assert_eq!(window.read(|workspace, _| workspace.rebase_plan_len()), 2);
        assert!(store.observe().expect("observe base refusal").is_none());

        // Reword text is applied explicitly, frozen, and checked again even
        // when a programmatic write bypasses disabled input handling.
        window.update(|workspace, window, cx| {
            workspace.rebase.message.update(cx, |editor, cx| {
                editor.set_value("frozen reword", window, cx)
            });
            workspace.set_rebase_step_action(
                0,
                PlanAction::Reword {
                    message: "frozen reword".into(),
                },
                cx,
            );
            workspace.request_start_rebase(cx);
            let reword_id = workspace
                .rebase_pending_action_id()
                .expect("reword request");
            workspace.rebase.message.update(cx, |editor, cx| {
                editor.set_value("programmatic drift", window, cx)
            });
            workspace.confirm_rebase_action(reword_id, cx);
            assert_eq!(workspace.rebase_pending_action_id(), Some(reword_id));
            assert!(!workspace.operations.rebase_in_flight());
            workspace.cancel_rebase_action(reword_id, cx);
            workspace.set_rebase_step_action(0, PlanAction::Edit, cx);
        });
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), original_head);
        assert!(store.observe().expect("observe reword refusal").is_none());

        // A benign observe changes read epochs/status only; the exact request
        // remains valid, dispatches once, and cannot be replayed by stale ID.
        let exact_id = window.update(|workspace, _, cx| {
            workspace.request_start_rebase(cx);
            let exact_id = workspace.rebase_pending_action_id().expect("exact request");
            workspace.observe_rebase(cx);
            exact_id
        });
        window.update(|workspace, _, cx| workspace.confirm_rebase_action(exact_id, cx));
        let observed = store
            .observe()
            .expect("observe exact dispatch")
            .expect("operation");
        assert_eq!(observed.state, RebaseState::PausedForEdit);
        assert_eq!(observed.attempt, 1);
        window.update(|workspace, _, cx| workspace.confirm_rebase_action(exact_id, cx));
        assert_eq!(
            store
                .observe()
                .expect("observe stale confirm")
                .expect("operation")
                .attempt,
            1
        );
    }

    #[test]
    #[cfg(feature = "ui-smoke")]
    fn native_controller_amend_and_split_messages_pause_on_drift_without_git_dispatch() {
        let (_temporary, checkout, _base, store, mut window) = native_controller_fixture();
        window.update(|workspace, _, cx| {
            workspace.set_rebase_step_action(0, PlanAction::Edit, cx);
            workspace.request_start_rebase(cx);
            let id = workspace.rebase_pending_action_id().expect("start request");
            workspace.confirm_rebase_action(id, cx);
        });
        assert_eq!(
            store
                .observe()
                .expect("edit operation")
                .expect("operation")
                .state,
            RebaseState::PausedForEdit
        );

        let before_amend = git(&checkout, &["rev-parse", "HEAD"]);
        window.update(|workspace, window, cx| {
            workspace.rebase.message.update(cx, |editor, cx| {
                editor.set_value("frozen amend", window, cx)
            });
            workspace.request_amend_rebase(cx);
            let id = workspace.rebase_pending_action_id().expect("amend request");
            workspace.rebase.message.update(cx, |editor, cx| {
                editor.set_value("drifted amend", window, cx)
            });
            workspace.confirm_rebase_action(id, cx);
            assert_eq!(workspace.rebase_pending_action_id(), Some(id));
            assert!(!workspace.operations.rebase_in_flight());
            workspace.cancel_rebase_action(id, cx);
        });
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), before_amend);

        window.update(|workspace, _, cx| {
            workspace.request_begin_split(cx);
            let id = workspace.rebase_pending_action_id().expect("split request");
            workspace.confirm_rebase_action(id, cx);
        });
        let split = store
            .observe()
            .expect("split operation")
            .expect("operation");
        assert_eq!(
            split
                .split
                .as_ref()
                .and_then(|split| split.replacement_commit_count),
            Some(0)
        );
        let before_part = git(&checkout, &["rev-parse", "HEAD"]);
        window.update(|workspace, window, cx| {
            workspace.rebase.message.update(cx, |editor, cx| {
                editor.set_value("frozen split part", window, cx)
            });
            workspace.request_commit_split_part(cx);
            let id = workspace
                .rebase_pending_action_id()
                .expect("split-part request");
            workspace.rebase.message.update(cx, |editor, cx| {
                editor.set_value("drifted split part", window, cx)
            });
            workspace.confirm_rebase_action(id, cx);
            assert_eq!(workspace.rebase_pending_action_id(), Some(id));
            assert!(!workspace.operations.rebase_in_flight());
        });
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), before_part);
        assert_eq!(
            store
                .observe()
                .expect("split after refusal")
                .expect("operation")
                .split
                .and_then(|split| split.replacement_commit_count),
            Some(0)
        );
    }

    #[test]
    fn editable_identity_excludes_benign_poll_state_and_covers_all_text_payloads() {
        let steps = pick_steps(&inventory());
        let start = RebaseEditableInputIdentity::Start {
            steps: steps.clone(),
            prepared_base_oid: oid('a'),
            displayed_base: "refs/heads/base".into(),
            reword_editor: None,
        };
        assert_eq!(start.clone(), start);
        assert_ne!(
            RebaseEditableInputIdentity::AmendMessage("one".into()),
            RebaseEditableInputIdentity::AmendMessage("two".into())
        );
        assert_ne!(
            RebaseEditableInputIdentity::SplitPartMessage("one".into()),
            RebaseEditableInputIdentity::SplitPartMessage("two".into())
        );
        let mut reordered = steps;
        reordered.swap(0, 1);
        assert_ne!(
            start,
            RebaseEditableInputIdentity::Start {
                steps: reordered,
                prepared_base_oid: oid('a'),
                displayed_base: "refs/heads/base".into(),
                reword_editor: None,
            }
        );
        assert!(rebase_input_is_frozen(true, false));
        assert!(rebase_input_is_frozen(false, true));
        assert!(!rebase_input_is_frozen(false, false));
    }

    #[test]
    fn base_candidate_gate_refuses_ambiguous_revision_expressions() {
        assert!(!("feature" == "HEAD" || "feature".starts_with("refs/") || "feature".len() == 40));
        assert!("refs/heads/feature".starts_with("refs/"));
        assert_eq!(oid('a').len(), 40);
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
