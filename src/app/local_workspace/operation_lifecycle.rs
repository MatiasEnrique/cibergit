//! Checkout-scoped ownership of local mutation and observation lifecycles.
//!
//! The GPUI shell starts work, but this module is the authority for which
//! frozen intent may run, which observation is current, and whether recovery
//! has enough authoritative evidence to be acknowledged.

use super::rebase_panel::{RebaseCommand, RebaseEditableInputIdentity};
use super::{
    LocalAction, LocalSnapshot, RemoteBranchObservation, SnapshotGuard, StartedAction, head_label,
    operation_is_active,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ObservationKind {
    Snapshot,
    Diff,
    RemoteBranch,
    PrPublish,
    Rebase,
}

impl ObservationKind {
    const COUNT: usize = 5;

    const fn index(self) -> usize {
        match self {
            Self::Snapshot => 0,
            Self::Diff => 1,
            Self::RemoteBranch => 2,
            Self::PrPublish => 3,
            Self::Rebase => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ObservationTicket {
    kind: ObservationKind,
    generation: u64,
    checkout_epoch: u64,
    operation_version: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DispatchTicket {
    request_id: u64,
    checkout_epoch: u64,
    operation_version: u64,
    lane: Lane,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
    Local,
    Rebase,
    Reconciliation,
}

#[derive(Clone)]
pub(super) struct FrozenLocalIntent {
    pub(super) id: u64,
    pub(super) action: LocalAction,
    pub(super) guard: SnapshotGuard,
    checkout_epoch: u64,
}

#[derive(Clone)]
pub(super) struct FrozenRebaseIntent {
    pub(super) id: u64,
    pub(super) command: RebaseCommand,
    pub(super) editable_inputs: RebaseEditableInputIdentity,
    checkout_epoch: u64,
}

#[derive(Clone)]
enum FrozenIntent {
    Local(FrozenLocalIntent),
    Rebase(Box<FrozenRebaseIntent>),
}

impl FrozenIntent {
    fn id(&self) -> u64 {
        match self {
            Self::Local(intent) => intent.id,
            Self::Rebase(intent) => intent.id,
        }
    }

    fn lane(&self) -> Lane {
        match self {
            Self::Local(_) => Lane::Local,
            Self::Rebase(_) => Lane::Rebase,
        }
    }
}

#[derive(Clone)]
struct RunningIntent {
    frozen: FrozenIntent,
    ticket: DispatchTicket,
}

#[derive(Clone)]
struct RecoveryState {
    started: StartedAction,
    authoritative_refresh_required: bool,
    publish_refresh_required: bool,
}

#[derive(Clone)]
enum Phase {
    Idle,
    AwaitingConfirmation(FrozenIntent),
    Running(RunningIntent),
    AwaitingAuthoritativeSnapshot,
    Recovery(RecoveryState),
    Acknowledging {
        recovery: RecoveryState,
        ticket: DispatchTicket,
    },
}

#[derive(Clone)]
struct AuthoritativeSnapshot {
    guard: SnapshotGuard,
    displayed_head: String,
    operation_active: bool,
}

pub(super) struct LocalDispatch {
    pub(super) ticket: DispatchTicket,
    pub(super) intent: FrozenLocalIntent,
    pub(super) started: StartedAction,
}

pub(super) struct RebaseDispatch {
    pub(super) ticket: DispatchTicket,
    pub(super) intent: FrozenRebaseIntent,
}

pub(super) struct ReconciliationDispatch {
    pub(super) ticket: DispatchTicket,
    pub(super) started: StartedAction,
}

pub(super) struct OperationLifecycle {
    checkout_epoch: Option<u64>,
    checkout_identity: String,
    next_request_id: u64,
    operation_version: u64,
    phase: Phase,
    observation_generations: [u64; ObservationKind::COUNT],
    observations_in_flight: [bool; ObservationKind::COUNT],
    snapshot: Option<AuthoritativeSnapshot>,
    remote_observation: Option<RemoteBranchObservation>,
}

impl Default for OperationLifecycle {
    fn default() -> Self {
        Self {
            checkout_epoch: None,
            checkout_identity: String::new(),
            next_request_id: 1,
            operation_version: 0,
            phase: Phase::Idle,
            observation_generations: [0; ObservationKind::COUNT],
            observations_in_flight: [false; ObservationKind::COUNT],
            snapshot: None,
            remote_observation: None,
        }
    }
}

impl OperationLifecycle {
    pub(super) fn activate(
        &mut self,
        checkout_epoch: u64,
        checkout_identity: String,
        snapshot: &LocalSnapshot,
        started: Option<StartedAction>,
    ) {
        self.checkout_epoch = Some(checkout_epoch);
        self.checkout_identity = checkout_identity;
        self.advance_operation_version();
        self.observation_generations = [0; ObservationKind::COUNT];
        self.observations_in_flight = [false; ObservationKind::COUNT];
        self.record_snapshot(snapshot);
        self.phase = started.map_or(Phase::Idle, |started| {
            let publish_refresh_required = started.pr_publish.is_some();
            Phase::Recovery(RecoveryState {
                started,
                authoritative_refresh_required: false,
                publish_refresh_required,
            })
        });
    }

    pub(super) fn begin_observation(
        &mut self,
        kind: ObservationKind,
    ) -> Result<ObservationTicket, &'static str> {
        let Some(checkout_epoch) = self.checkout_epoch else {
            return Err("Local workspace is not ready");
        };
        let index = kind.index();
        if kind == ObservationKind::RemoteBranch {
            self.remote_observation = None;
        }
        self.observation_generations[index] = self.observation_generations[index].wrapping_add(1);
        self.observations_in_flight[index] = true;
        Ok(ObservationTicket {
            kind,
            generation: self.observation_generations[index],
            checkout_epoch,
            operation_version: self.operation_version,
        })
    }

    pub(super) fn finish_observation(&mut self, ticket: ObservationTicket) -> bool {
        if !self.observation_is_current(ticket) {
            return false;
        }
        self.observations_in_flight[ticket.kind.index()] = false;
        true
    }

    pub(super) fn install_snapshot_observation(
        &mut self,
        ticket: ObservationTicket,
        snapshot: &LocalSnapshot,
    ) -> bool {
        if ticket.kind != ObservationKind::Snapshot || !self.finish_observation(ticket) {
            return false;
        }
        self.record_snapshot(snapshot);
        if let Phase::Recovery(recovery) = &mut self.phase {
            recovery.authoritative_refresh_required = false;
        } else if matches!(self.phase, Phase::AwaitingAuthoritativeSnapshot) {
            self.phase = Phase::Idle;
        }
        true
    }

    pub(super) fn install_remote_observation(
        &mut self,
        ticket: ObservationTicket,
        observation: RemoteBranchObservation,
    ) -> bool {
        if ticket.kind != ObservationKind::RemoteBranch || !self.finish_observation(ticket) {
            return false;
        }
        self.remote_observation = Some(observation);
        true
    }

    pub(super) fn finish_publish_reconciliation(
        &mut self,
        ticket: ObservationTicket,
        attempt: &super::pr_publish::PrPublishAttempt,
    ) -> bool {
        if ticket.kind != ObservationKind::PrPublish || !self.finish_observation(ticket) {
            return false;
        }
        let Phase::Recovery(recovery) = &mut self.phase else {
            return false;
        };
        if recovery.started.pr_publish.as_ref() != Some(attempt) {
            return false;
        }
        recovery.publish_refresh_required = false;
        true
    }

    pub(super) fn observation_pending(&self, kind: ObservationKind) -> bool {
        self.observations_in_flight[kind.index()]
    }

    pub(super) fn remote_observation(&self) -> Option<&RemoteBranchObservation> {
        self.remote_observation.as_ref()
    }

    pub(super) fn admit_local(
        &mut self,
        action: LocalAction,
        guard: SnapshotGuard,
    ) -> Result<u64, String> {
        self.require_admission_lane(Lane::Local)?;
        let Some(epoch) = self.checkout_epoch else {
            return Err("Local workspace is not ready".into());
        };
        let Some(snapshot) = &self.snapshot else {
            return Err("Refresh Local Changes before acting".into());
        };
        if snapshot.guard != guard {
            return Err("Local Git state changed; refresh and review the action again".into());
        }
        if action.requires_exclusive_checkout_lane() && snapshot.operation_active {
            return Err(
                "Branch/pull action paused while a merge, rebase, cherry-pick, or revert is active"
                    .into(),
            );
        }
        if let LocalAction::ForcePushWithLease {
            remote,
            branch,
            observed_remote_oid,
        } = &action
            && !super::remote_observation_matches(
                self.remote_observation.as_ref(),
                remote,
                branch,
                observed_remote_oid,
            )
        {
            return Err(
                "Force-with-lease paused: observe and inspect this exact remote branch/OID first"
                    .into(),
            );
        }
        let id = self.allocate_request_id();
        self.phase = Phase::AwaitingConfirmation(FrozenIntent::Local(FrozenLocalIntent {
            id,
            action,
            guard,
            checkout_epoch: epoch,
        }));
        Ok(id)
    }

    pub(super) fn confirm_local(&mut self, request_id: u64) -> Result<LocalDispatch, String> {
        let Phase::AwaitingConfirmation(FrozenIntent::Local(intent)) = &self.phase else {
            return Err("There is no local action awaiting confirmation".into());
        };
        if intent.id != request_id {
            return Err("That confirmation is stale; review the current action".into());
        }
        self.validate_frozen_local(intent)?;
        let intent = intent.clone();
        let snapshot = self
            .snapshot
            .as_ref()
            .ok_or_else(|| "Refresh Local Changes before acting".to_string())?;
        let started = StartedAction {
            schema_version: 1,
            request_id,
            kind: intent.action.journal_kind().into(),
            summary: intent.action.summary(),
            checkout_identity: self.checkout_identity.clone(),
            displayed_head: snapshot.displayed_head.clone(),
            expected_remote_oid: match &intent.action {
                LocalAction::ForcePushWithLease {
                    observed_remote_oid,
                    ..
                } => Some(observed_remote_oid.clone()),
                LocalAction::PublishPrSource { preparation } => {
                    Some(preparation.expected_remote_oid.clone())
                }
                _ => None,
            },
            pr_publish: match &intent.action {
                LocalAction::PublishPrSource { preparation } => {
                    Some(preparation.attempt(request_id))
                }
                _ => None,
            },
        };
        let ticket = self.start_running(FrozenIntent::Local(intent.clone()));
        Ok(LocalDispatch {
            ticket,
            intent,
            started,
        })
    }

    pub(super) fn complete_local_success(
        &mut self,
        ticket: DispatchTicket,
        snapshot: &LocalSnapshot,
    ) -> bool {
        if !self.running_matches(ticket, Lane::Local) {
            return false;
        }
        self.advance_operation_version();
        self.phase = Phase::Idle;
        self.remote_observation = None;
        self.record_snapshot(snapshot);
        true
    }

    pub(super) fn complete_local_retry(&mut self, ticket: DispatchTicket) -> bool {
        let Some(running) = self.take_running(ticket, Lane::Local) else {
            return false;
        };
        self.advance_operation_version();
        self.phase = Phase::AwaitingConfirmation(running.frozen);
        true
    }

    pub(super) fn complete_local_recovery(
        &mut self,
        ticket: DispatchTicket,
        started: StartedAction,
        authoritative_refresh_required: bool,
        publish_refresh_required: bool,
    ) -> bool {
        if !self.running_matches(ticket, Lane::Local) {
            return false;
        }
        self.advance_operation_version();
        self.remote_observation = None;
        self.phase = Phase::Recovery(RecoveryState {
            started,
            authoritative_refresh_required,
            publish_refresh_required,
        });
        true
    }

    pub(super) fn cancel_local(&mut self, request_id: u64) -> bool {
        if matches!(
            &self.phase,
            Phase::AwaitingConfirmation(FrozenIntent::Local(intent)) if intent.id == request_id
        ) {
            self.phase = Phase::Idle;
            self.advance_operation_version();
            true
        } else {
            false
        }
    }

    pub(super) fn admit_rebase(
        &mut self,
        command: RebaseCommand,
        editable_inputs: RebaseEditableInputIdentity,
    ) -> Result<u64, String> {
        self.require_admission_lane(Lane::Rebase)?;
        let Some(epoch) = self.checkout_epoch else {
            return Err("Local workspace is not ready".into());
        };
        if let Some(guard) = command.guard()
            && self.snapshot.as_ref().map(|snapshot| &snapshot.guard) != Some(guard)
        {
            return Err(
                "Rebase request paused: the affected Git snapshot changed; refresh and review again"
                    .into(),
            );
        }
        let id = self.allocate_request_id();
        self.phase =
            Phase::AwaitingConfirmation(FrozenIntent::Rebase(Box::new(FrozenRebaseIntent {
                id,
                command,
                editable_inputs,
                checkout_epoch: epoch,
            })));
        Ok(id)
    }

    pub(super) fn confirm_rebase(
        &mut self,
        request_id: u64,
        current_inputs: &RebaseEditableInputIdentity,
    ) -> Result<RebaseDispatch, String> {
        let Phase::AwaitingConfirmation(FrozenIntent::Rebase(intent)) = &self.phase else {
            return Err("There is no rebase action awaiting confirmation".into());
        };
        if intent.id != request_id {
            return Err("That rebase confirmation is stale".into());
        }
        let epoch = self
            .checkout_epoch
            .ok_or_else(|| "Local workspace is not ready".to_string())?;
        if intent.checkout_epoch != epoch {
            return Err("Rebase confirmation paused: checkout identity changed".into());
        }
        if &intent.editable_inputs != current_inputs {
            return Err("Rebase confirmation paused: the displayed plan, base, or message input drifted from the frozen request. Nothing was dispatched; the exact pending request was retained. Cancel, edit/reprepare, and request a new confirmation.".into());
        }
        if let Some(guard) = intent.command.guard()
            && self.snapshot.as_ref().map(|snapshot| &snapshot.guard) != Some(guard)
        {
            return Err("Rebase confirmation paused: the Git snapshot changed. The pending confirmation was retained.".into());
        }
        let intent = intent.as_ref().clone();
        let ticket = self.start_running(FrozenIntent::Rebase(Box::new(intent.clone())));
        Ok(RebaseDispatch { ticket, intent })
    }

    pub(super) fn complete_rebase(
        &mut self,
        ticket: DispatchTicket,
        snapshot: Option<&LocalSnapshot>,
    ) -> bool {
        if !self.running_matches(ticket, Lane::Rebase) {
            return false;
        }
        self.advance_operation_version();
        self.remote_observation = None;
        self.phase = if snapshot.is_some() {
            Phase::Idle
        } else {
            Phase::AwaitingAuthoritativeSnapshot
        };
        if let Some(snapshot) = snapshot {
            self.record_snapshot(snapshot);
        }
        true
    }

    pub(super) fn cancel_rebase(&mut self, request_id: u64) -> bool {
        if matches!(
            &self.phase,
            Phase::AwaitingConfirmation(FrozenIntent::Rebase(intent)) if intent.id == request_id
        ) {
            self.phase = Phase::Idle;
            self.advance_operation_version();
            true
        } else {
            false
        }
    }

    pub(super) fn begin_reconciliation(&mut self) -> Result<ReconciliationDispatch, String> {
        let Phase::Recovery(recovery) = &self.phase else {
            return Err("There is no started action to reconcile".into());
        };
        if recovery.authoritative_refresh_required {
            return Err(
                "Wait for a successful authoritative Git refresh before acknowledging".into(),
            );
        }
        if recovery.publish_refresh_required {
            return Err("Wait for a successful fresh provider and effective push-endpoint reconciliation before acknowledging".into());
        }
        if self.snapshot.is_none() {
            return Err("Refresh authoritative Git state before reconciling".into());
        }
        let recovery = recovery.clone();
        self.advance_operation_version();
        let ticket = DispatchTicket {
            request_id: recovery.started.request_id,
            checkout_epoch: self.checkout_epoch.unwrap_or_default(),
            operation_version: self.operation_version,
            lane: Lane::Reconciliation,
        };
        self.phase = Phase::Acknowledging {
            recovery: recovery.clone(),
            ticket,
        };
        Ok(ReconciliationDispatch {
            ticket,
            started: recovery.started,
        })
    }

    pub(super) fn complete_reconciliation(
        &mut self,
        ticket: DispatchTicket,
        cleared: bool,
    ) -> bool {
        let Phase::Acknowledging {
            recovery,
            ticket: active,
        } = &self.phase
        else {
            return false;
        };
        if *active != ticket {
            return false;
        }
        let recovery = recovery.clone();
        self.advance_operation_version();
        self.phase = if cleared {
            Phase::Idle
        } else {
            Phase::Recovery(recovery)
        };
        true
    }

    pub(super) fn pending_local(&self) -> Option<&FrozenLocalIntent> {
        match &self.phase {
            Phase::AwaitingConfirmation(FrozenIntent::Local(intent)) => Some(intent),
            _ => None,
        }
    }

    pub(super) fn pending_rebase(&self) -> Option<&FrozenRebaseIntent> {
        match &self.phase {
            Phase::AwaitingConfirmation(FrozenIntent::Rebase(intent)) => Some(intent),
            _ => None,
        }
    }

    pub(super) fn recovery(&self) -> Option<&StartedAction> {
        match &self.phase {
            Phase::Recovery(recovery) | Phase::Acknowledging { recovery, .. } => {
                Some(&recovery.started)
            }
            _ => None,
        }
    }

    pub(super) fn local_in_flight_id(&self) -> Option<u64> {
        match &self.phase {
            Phase::Running(running) if running.ticket.lane == Lane::Local => {
                Some(running.ticket.request_id)
            }
            _ => None,
        }
    }

    pub(super) fn rebase_in_flight(&self) -> bool {
        matches!(&self.phase, Phase::Running(running) if running.ticket.lane == Lane::Rebase)
    }

    pub(super) fn rebase_pending_or_running(&self) -> bool {
        self.pending_rebase().is_some() || self.rebase_in_flight()
    }

    pub(super) fn local_pending_or_running_or_recovering(&self) -> bool {
        self.pending_local().is_some()
            || self.local_in_flight_id().is_some()
            || matches!(self.phase, Phase::Recovery(_) | Phase::Acknowledging { .. })
    }

    pub(super) fn acknowledging(&self) -> bool {
        matches!(self.phase, Phase::Acknowledging { .. })
    }

    pub(super) fn idle(&self) -> bool {
        matches!(self.phase, Phase::Idle)
    }

    pub(super) fn controls_locked(&self) -> bool {
        !self.idle() || self.observation_pending(ObservationKind::PrPublish)
    }

    fn require_admission_lane(&self, requested: Lane) -> Result<(), String> {
        if self.observation_pending(ObservationKind::PrPublish) {
            return Err("Wait for the fresh PR publication read to finish".into());
        }
        match &self.phase {
            Phase::Idle => Ok(()),
            Phase::AwaitingConfirmation(frozen) if frozen.lane() == Lane::Rebase => {
                if requested == Lane::Local {
                    Err(
                        "A rebase transition or confirmation is active; finish or cancel it first"
                            .into(),
                    )
                } else {
                    Err("A rebase confirmation is already pending".into())
                }
            }
            Phase::Running(running) if running.ticket.lane == Lane::Rebase => {
                if requested == Lane::Local {
                    Err(
                        "A rebase transition or confirmation is active; finish or cancel it first"
                            .into(),
                    )
                } else {
                    Err("A rebase effect is already running".into())
                }
            }
            Phase::AwaitingConfirmation(_) => {
                if requested == Lane::Rebase {
                    Err("A local Git action or its durable reconciliation is pending".into())
                } else {
                    Err(
                        "Reconcile or cancel the current local action before starting another"
                            .into(),
                    )
                }
            }
            Phase::Running(_) | Phase::Acknowledging { .. } => {
                Err("A local action or its durable reconciliation is still running".into())
            }
            Phase::Recovery(_) => {
                Err("Reconcile the current local action before starting another".into())
            }
            Phase::AwaitingAuthoritativeSnapshot => Err(
                "Wait for a successful authoritative Git refresh before starting another action"
                    .into(),
            ),
        }
    }

    fn validate_frozen_local(&self, intent: &FrozenLocalIntent) -> Result<(), String> {
        let epoch = self
            .checkout_epoch
            .ok_or_else(|| "Local workspace is not ready".to_string())?;
        if intent.checkout_epoch != epoch {
            return Err("Checkout identity changed; refresh before retrying".into());
        }
        let snapshot = self
            .snapshot
            .as_ref()
            .ok_or_else(|| "Refresh Local Changes before acting".to_string())?;
        if snapshot.guard != intent.guard {
            return Err(
                "Confirmation paused: authoritative Git state changed; the exact pending request was retained"
                    .into(),
            );
        }
        if intent.action.requires_exclusive_checkout_lane() && snapshot.operation_active {
            return Err(
                "Confirmation paused: authoritative Git operation state is incompatible".into(),
            );
        }
        if let LocalAction::ForcePushWithLease {
            remote,
            branch,
            observed_remote_oid,
        } = &intent.action
            && !super::remote_observation_matches(
                self.remote_observation.as_ref(),
                remote,
                branch,
                observed_remote_oid,
            )
        {
            return Err(
                "Confirmation paused: the authoritative remote observation changed; observe and review the exact lease again"
                    .into(),
            );
        }
        Ok(())
    }

    fn start_running(&mut self, frozen: FrozenIntent) -> DispatchTicket {
        self.advance_operation_version();
        let ticket = DispatchTicket {
            request_id: frozen.id(),
            checkout_epoch: self.checkout_epoch.unwrap_or_default(),
            operation_version: self.operation_version,
            lane: frozen.lane(),
        };
        self.phase = Phase::Running(RunningIntent { frozen, ticket });
        ticket
    }

    fn running_matches(&self, ticket: DispatchTicket, lane: Lane) -> bool {
        matches!(
            &self.phase,
            Phase::Running(running) if running.ticket == ticket && running.ticket.lane == lane
        )
    }

    fn take_running(&self, ticket: DispatchTicket, lane: Lane) -> Option<RunningIntent> {
        match &self.phase {
            Phase::Running(running) if running.ticket == ticket && running.ticket.lane == lane => {
                Some(running.clone())
            }
            _ => None,
        }
    }

    fn observation_is_current(&self, ticket: ObservationTicket) -> bool {
        self.checkout_epoch == Some(ticket.checkout_epoch)
            && self.operation_version == ticket.operation_version
            && self.observation_generations[ticket.kind.index()] == ticket.generation
            && self.observations_in_flight[ticket.kind.index()]
    }

    fn advance_operation_version(&mut self) {
        self.operation_version = self.operation_version.wrapping_add(1);
        self.observations_in_flight = [false; ObservationKind::COUNT];
    }

    fn record_snapshot(&mut self, snapshot: &LocalSnapshot) {
        self.snapshot = Some(AuthoritativeSnapshot {
            guard: snapshot.guard.clone(),
            displayed_head: head_label(&snapshot.head),
            operation_active: operation_is_active(&snapshot.operation),
        });
    }

    fn allocate_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        id
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use cibergit::local_git::LocalGit;
    use tempfile::TempDir;

    use super::*;

    fn git(root: &std::path::Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn lifecycle_fixture() -> (TempDir, LocalSnapshot, OperationLifecycle) {
        let temporary = tempfile::tempdir().expect("tempdir");
        git(temporary.path(), &["init", "-q"]);
        git(temporary.path(), &["config", "user.name", "Lifecycle Test"]);
        git(
            temporary.path(),
            &["config", "user.email", "lifecycle@example.test"],
        );
        std::fs::write(temporary.path().join("tracked.txt"), "base\n").expect("fixture file");
        git(temporary.path(), &["add", "tracked.txt"]);
        git(temporary.path(), &["commit", "-qm", "base"]);
        let snapshot = LocalGit::open(temporary.path())
            .expect("local git")
            .snapshot()
            .expect("snapshot");
        let mut lifecycle = OperationLifecycle::default();
        lifecycle.activate(7, "fixture-checkout".into(), &snapshot, None);
        (temporary, snapshot, lifecycle)
    }

    fn commit_action(message: &str) -> LocalAction {
        LocalAction::Commit {
            message: message.into(),
        }
    }

    #[test]
    fn stale_completion_cannot_release_a_newer_running_operation() {
        let (_temporary, snapshot, mut lifecycle) = lifecycle_fixture();
        let first = lifecycle
            .admit_local(commit_action("first"), snapshot.guard.clone())
            .expect("first admission");
        let first_dispatch = lifecycle.confirm_local(first).expect("first dispatch");
        assert!(lifecycle.complete_local_retry(first_dispatch.ticket));
        assert!(lifecycle.cancel_local(first));
        let second = lifecycle
            .admit_local(commit_action("second"), snapshot.guard.clone())
            .expect("second admission");
        let second_dispatch = lifecycle.confirm_local(second).expect("second dispatch");

        assert!(!lifecycle.complete_local_retry(first_dispatch.ticket));
        assert_eq!(
            lifecycle.local_in_flight_id(),
            Some(second_dispatch.intent.id)
        );
    }

    #[test]
    fn confirmation_rejects_a_changed_authoritative_force_lease() {
        let (_temporary, snapshot, mut lifecycle) = lifecycle_fixture();
        let observed = "1111111111111111111111111111111111111111";
        let first_read = lifecycle
            .begin_observation(ObservationKind::RemoteBranch)
            .expect("remote read");
        assert!(lifecycle.install_remote_observation(
            first_read,
            RemoteBranchObservation {
                remote: "origin".into(),
                branch: "main".into(),
                oid: Some(observed.into()),
            },
        ));
        let request = lifecycle
            .admit_local(
                LocalAction::ForcePushWithLease {
                    remote: "origin".into(),
                    branch: "main".into(),
                    observed_remote_oid: observed.into(),
                },
                snapshot.guard.clone(),
            )
            .expect("lease admission");
        let changed_read = lifecycle
            .begin_observation(ObservationKind::RemoteBranch)
            .expect("changed remote read");
        assert!(lifecycle.install_remote_observation(
            changed_read,
            RemoteBranchObservation {
                remote: "origin".into(),
                branch: "main".into(),
                oid: Some("2222222222222222222222222222222222222222".into()),
            },
        ));

        assert!(lifecycle.confirm_local(request).is_err());
        assert_eq!(
            lifecycle.pending_local().map(|intent| intent.id),
            Some(request)
        );
    }

    #[test]
    fn uncertain_completion_requires_a_new_authoritative_snapshot_before_reconciliation() {
        let (_temporary, snapshot, mut lifecycle) = lifecycle_fixture();
        let request = lifecycle
            .admit_local(commit_action("uncertain"), snapshot.guard.clone())
            .expect("admission");
        let dispatch = lifecycle.confirm_local(request).expect("dispatch");
        assert!(lifecycle.complete_local_recovery(dispatch.ticket, dispatch.started, true, false,));
        assert!(lifecycle.begin_reconciliation().is_err());
        let refresh = lifecycle
            .begin_observation(ObservationKind::Snapshot)
            .expect("refresh");
        assert!(lifecycle.install_snapshot_observation(refresh, &snapshot));

        assert!(lifecycle.begin_reconciliation().is_ok());
    }

    #[test]
    fn superseded_observation_cannot_become_authoritative() {
        let (_temporary, _snapshot, mut lifecycle) = lifecycle_fixture();
        let delayed = lifecycle
            .begin_observation(ObservationKind::Diff)
            .expect("delayed read");
        let current = lifecycle
            .begin_observation(ObservationKind::Diff)
            .expect("current read");

        assert!(!lifecycle.finish_observation(delayed));
        assert!(lifecycle.finish_observation(current));
    }

    #[test]
    fn pending_confirmation_excludes_a_second_admission() {
        let (_temporary, snapshot, mut lifecycle) = lifecycle_fixture();
        lifecycle
            .admit_local(commit_action("first"), snapshot.guard.clone())
            .expect("first admission");

        assert!(
            lifecycle
                .admit_local(commit_action("second"), snapshot.guard)
                .is_err()
        );
    }

    #[test]
    fn rebase_without_post_effect_snapshot_keeps_the_checkout_lane_closed() {
        let (_temporary, snapshot, mut lifecycle) = lifecycle_fixture();
        let request = lifecycle
            .admit_rebase(
                RebaseCommand::Retire {
                    operation_id: "operation".into(),
                },
                RebaseEditableInputIdentity::None,
            )
            .expect("rebase admission");
        let dispatch = lifecycle
            .confirm_rebase(request, &RebaseEditableInputIdentity::None)
            .expect("rebase dispatch");
        assert!(lifecycle.complete_rebase(dispatch.ticket, None));
        assert!(
            lifecycle
                .admit_local(commit_action("blocked"), snapshot.guard.clone())
                .is_err()
        );
        let refresh = lifecycle
            .begin_observation(ObservationKind::Snapshot)
            .expect("authoritative refresh");
        assert!(lifecycle.install_snapshot_observation(refresh, &snapshot));

        assert!(
            lifecycle
                .admit_local(commit_action("allowed"), snapshot.guard)
                .is_ok()
        );
    }

    #[test]
    fn operation_transition_retires_invalidated_reads_without_releasing_newer_reads() {
        let (_temporary, snapshot, mut lifecycle) = lifecycle_fixture();
        let request = lifecycle
            .admit_local(commit_action("recover"), snapshot.guard.clone())
            .expect("admission");
        let dispatch = lifecycle.confirm_local(request).expect("dispatch");
        assert!(
            lifecycle.complete_local_recovery(dispatch.ticket, dispatch.started, false, false,)
        );
        let invalidated_publish = lifecycle
            .begin_observation(ObservationKind::PrPublish)
            .expect("publish reconciliation read");
        let invalidated_rebase = lifecycle
            .begin_observation(ObservationKind::Rebase)
            .expect("rebase observation");

        let reconciliation = lifecycle
            .begin_reconciliation()
            .expect("begin durable reconciliation");
        assert!(
            !lifecycle.observation_pending(ObservationKind::PrPublish)
                && !lifecycle.observation_pending(ObservationKind::Rebase),
            "the transition must retire every read invalidated by its new operation version"
        );
        assert!(lifecycle.complete_reconciliation(reconciliation.ticket, true));
        assert!(lifecycle.idle() && !lifecycle.controls_locked());

        let current_publish = lifecycle
            .begin_observation(ObservationKind::PrPublish)
            .expect("new publish read");
        let current_rebase = lifecycle
            .begin_observation(ObservationKind::Rebase)
            .expect("new rebase read");
        assert!(!lifecycle.finish_observation(invalidated_publish));
        assert!(!lifecycle.finish_observation(invalidated_rebase));
        assert!(
            lifecycle.observation_pending(ObservationKind::PrPublish)
                && lifecycle.observation_pending(ObservationKind::Rebase),
            "stale tickets must not release newer observations of the same kinds"
        );
        assert!(lifecycle.finish_observation(current_publish));
        assert!(lifecycle.finish_observation(current_rebase));
        assert!(
            lifecycle
                .admit_local(commit_action("unblocked"), snapshot.guard)
                .is_ok()
        );
    }
}
