//! Per-tab state and fencing for explicit GitHub Actions run controls.
//!
//! Re-run all jobs, re-run failed jobs, and normal cancel share this tab's one
//! durable per-target authority with every other mutation family. Nothing here
//! reads through the automatic read scheduler, and nothing here retries.

use super::review_interactions::{ActionJournal, JournalRequest};
use cibergit::{
    domain::{
        ActionsAttemptLocator, ActionsRunControlAcknowledgement, ActionsRunControlAction,
        ActionsRunControlAuthority, ActionsRunControlProgress, ActionsRunControlRequest,
        ProviderMutationOutcome, ProviderReadEvidence, Repository,
    },
    providers::{ActionsRunControlDispatch, GithubProvider},
};

/// Every Actions control offered by the Checks pane. The list is fixed: the
/// fresh preparation read decides which one GitHub can accept, so no control is
/// enabled or hidden from cached display state.
pub(super) const RUN_CONTROLS: [ActionsRunControlAction; 3] = [
    ActionsRunControlAction::RerunAllJobs,
    ActionsRunControlAction::RerunFailedJobs,
    ActionsRunControlAction::CancelRun,
];

/// The one Actions control operation a tab currently owns busy state for.
///
/// Ownership deliberately excludes every display value. A completion must be
/// able to release its own busy state and report its outcome even after the
/// selected check, the details generation, or the confirmation generation has
/// moved; otherwise a dispatched attempt could leave the tab permanently busy
/// and an Uncertain outcome could be silently dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ActionsControlOwnership {
    pub(super) workspace_instance: u64,
    pub(super) tab_instance: u64,
    pub(super) repository_key: String,
    pub(super) pull_request: u64,
    pub(super) operation_id: String,
    pub(super) attempt_id: String,
}

/// The display state a prepared control was bound to. It gates whether a
/// confirmation may be installed or confirmed, never whether an operation may
/// release itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ActionsControlDisplay {
    pub(super) details_generation: u64,
    pub(super) selected_check_id: Option<String>,
    pub(super) locator: Option<ActionsAttemptLocator>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct CiActionsState {
    /// Bumped whenever a prepared control stops being trustworthy.
    pub(super) confirmation_generation: u64,
    pub(super) in_flight: Option<ActionsControlOwnership>,
}

impl CiActionsState {
    pub(super) fn invalidate(&mut self) -> u64 {
        self.confirmation_generation = self
            .confirmation_generation
            .checked_add(1)
            .expect("Actions control confirmation generation overflow");
        self.confirmation_generation
    }

    /// Release the busy state for exactly this operation. A different owner,
    /// or no owner, releases nothing.
    pub(super) fn release(&mut self, ownership: &ActionsControlOwnership) -> bool {
        if self.in_flight.as_ref() == Some(ownership) {
            self.in_flight = None;
            return true;
        }
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ActionsControlPreparationToken {
    pub(super) ownership: ActionsControlOwnership,
    pub(super) display: ActionsControlDisplay,
    pub(super) generation: u64,
    pub(super) action: ActionsRunControlAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ActionsControlConfirmationToken {
    pub(super) ownership: ActionsControlOwnership,
    pub(super) display: ActionsControlDisplay,
    pub(super) generation: u64,
    pub(super) request: ActionsRunControlRequest,
}

/// Send exactly one control under this target's durable action journal.
pub(super) fn dispatch_actions_run_control(
    journal: &mut ActionJournal,
    provider: &GithubProvider,
    repository: &Repository,
    request: &ActionsRunControlRequest,
) -> ActionsRunControlDispatch {
    let mut admission =
        journal.admission(JournalRequest::ActionsRunControl(Box::new(request.clone())));
    provider.execute_actions_run_control(repository, request, &mut admission)
}

/// User-facing copy for a completed dispatch.
///
/// An accepted status is reported as GitHub accepting the request. It is never
/// reported as a started attempt or a cancelled run.
pub(super) fn completion_status(
    action: ActionsRunControlAction,
    outcome: &ProviderMutationOutcome<ActionsRunControlAcknowledgement>,
) -> String {
    match outcome {
        ProviderMutationOutcome::Acknowledged(ack) => {
            let observed = match &ack.observed_after {
                ProviderReadEvidence::Observed(progress) => format!(
                    "A later read saw attempt {} · {}{}. GitHub does not attribute that state to this request.",
                    progress.run_attempt,
                    progress.run_status,
                    progress
                        .run_conclusion
                        .as_deref()
                        .map(|value| format!(" · {value}"))
                        .unwrap_or_default()
                ),
                ProviderReadEvidence::Inconclusive { reason } => {
                    format!("The follow-up read was inconclusive: {reason}")
                }
            };
            format!(
                "GitHub accepted the {} request for run {} with status {}. Acceptance is not proof that a new attempt started or that the run is cancelled. {observed}",
                action.label().to_lowercase(),
                ack.target.run_database_id,
                ack.accepted_status
            )
        }
        ProviderMutationOutcome::PreflightRejected { reason } => format!(
            "{} was not sent; zero writes left this application: {reason}",
            action.label()
        ),
        ProviderMutationOutcome::Uncertain { reason, .. } => format!(
            "{} is unresolved and frozen against replay. GitHub offers no per-request correlation, so this attempt must be reconciled by an explicit read: {reason}",
            action.label()
        ),
    }
}

/// Read-only reconciliation copy for one unresolved control.
pub(super) fn reconciliation_summary(
    action: ActionsRunControlAction,
    frozen_attempt: u64,
    frozen_status: &str,
    evidence: &ProviderReadEvidence<ActionsRunControlProgress>,
) -> Option<(bool, String)> {
    let ProviderReadEvidence::Observed(progress) = evidence else {
        return None;
    };
    match action {
        ActionsRunControlAction::RerunAllJobs | ActionsRunControlAction::RerunFailedJobs => {
            (progress.run_attempt > frozen_attempt).then(|| {
                (
                    true,
                    format!(
                        "A later attempt {} exists for this run; the frozen attempt was {frozen_attempt}. GitHub does not attribute an attempt to a specific re-run request.",
                        progress.run_attempt
                    ),
                )
            })
        }
        ActionsRunControlAction::CancelRun => (progress.run_attempt == frozen_attempt
            && progress.run_status == "completed"
            && progress.run_conclusion.as_deref() == Some("cancelled"))
        .then(|| {
            (
                true,
                format!(
                    "The frozen attempt {frozen_attempt} is now completed with conclusion cancelled; it was {frozen_status} when the request was frozen. A cancellation cannot be attributed to a specific request."
                ),
            )
        }),
    }
}

/// Confirmation copy for the exact authority evidence behind one control.
pub(super) fn authority_summary(authority: &ActionsRunControlAuthority) -> &str {
    match authority {
        ActionsRunControlAuthority::Available => {
            "Available: a fresh repository read returned write permission."
        }
        ActionsRunControlAuthority::Unknown { reason }
        | ActionsRunControlAuthority::Unavailable { reason } => reason.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::domain::{Account, ActionsRunControlTarget};

    fn ownership(operation: &str) -> ActionsControlOwnership {
        ActionsControlOwnership {
            workspace_instance: 1,
            tab_instance: 2,
            repository_key: "github/owner/repo".into(),
            pull_request: 7,
            operation_id: operation.into(),
            attempt_id: format!("{operation}-attempt"),
        }
    }

    fn target(attempt: u64) -> ActionsRunControlTarget {
        ActionsRunControlTarget {
            account: Account {
                host: "github.com".into(),
                login: "alice".into(),
            },
            repository_node_id: "REPO_node".into(),
            repository_name_with_owner: "owner/repo".into(),
            pull_request_number: 7,
            pull_request_node_id: "PR_node".into(),
            check_node_id: "CHECK_node".into(),
            check_database_id: 9,
            check_suite_node_id: "SUITE_node".into(),
            check_suite_database_id: 8,
            workflow_node_id: "WORKFLOW_node".into(),
            workflow_database_id: 4,
            workflow_name: "CI".into(),
            run_node_id: "RUN_node".into(),
            run_database_id: 6,
            run_number: 5,
            run_attempt: attempt,
            run_event: "pull_request".into(),
            run_head_sha: "a".repeat(40),
            run_html_url: "https://github.com/owner/repo/actions/runs/6".into(),
            run_api_url: "https://api.github.com/repos/owner/repo/actions/runs/6".into(),
            workflow_url: "https://api.github.com/repos/owner/repo/actions/workflows/4".into(),
            head_repository_node_id: "REPO_node".into(),
            head_repository_name_with_owner: "owner/repo".into(),
        }
    }

    #[test]
    fn only_the_owning_operation_releases_busy_state() {
        let mut state = CiActionsState::default();
        let mine = ownership("rerun");
        let other = ownership("cancel");
        state.in_flight = Some(mine.clone());
        // A different operation must never release someone else's busy state.
        assert!(!state.release(&other));
        assert_eq!(state.in_flight, Some(mine.clone()));
        assert!(state.release(&mine));
        assert!(state.in_flight.is_none());
        // Releasing again must not release a later owner.
        state.in_flight = Some(other.clone());
        assert!(!state.release(&mine));
        assert_eq!(state.in_flight, Some(other));
    }

    #[test]
    fn ownership_carries_no_display_value() {
        // Ownership holds no details generation, selected check, locator, or
        // confirmation generation, so a moved display can never hide an
        // operation from its own completion and leak busy state.
        let first = ownership("rerun");
        assert_eq!(first, ownership("rerun"));
        let mut other_attempt = first.clone();
        other_attempt.attempt_id = "rerun-attempt-2".into();
        assert_ne!(first, other_attempt);
        let mut other_tab = first.clone();
        other_tab.tab_instance += 1;
        assert_ne!(first, other_tab);
    }

    #[test]
    fn invalidation_is_monotonic() {
        let mut state = CiActionsState::default();
        assert_eq!(state.invalidate(), 1);
        assert_eq!(state.invalidate(), 2);
        assert_eq!(state.confirmation_generation, 2);
    }

    #[test]
    fn acceptance_copy_never_claims_a_started_attempt_or_a_cancelled_run() {
        let ack = ActionsRunControlAcknowledgement {
            operation_id: "op".into(),
            action: ActionsRunControlAction::CancelRun,
            target: target(2),
            accepted_status: 202,
            observed_after: ProviderReadEvidence::Observed(ActionsRunControlProgress {
                run_attempt: 2,
                run_status: "in_progress".into(),
                run_conclusion: None,
            }),
        };
        let status = completion_status(
            ActionsRunControlAction::CancelRun,
            &ProviderMutationOutcome::Acknowledged(ack),
        );
        assert!(status.contains("GitHub accepted"));
        assert!(status.contains("not proof"));
        assert!(!status.contains("cancelled the run"));
    }

    #[test]
    fn an_uncertain_outcome_is_frozen_against_replay_in_its_copy() {
        let status = completion_status(
            ActionsRunControlAction::RerunFailedJobs,
            &ProviderMutationOutcome::<ActionsRunControlAcknowledgement>::Uncertain {
                context: cibergit::domain::MutationContext {
                    operation_id: "op".into(),
                    attempt_id: "attempt".into(),
                    action: "rerun-actions-run-failed-jobs".into(),
                    payload: serde_json::json!({}),
                },
                reason: "GitHub returned status 502".into(),
            },
        );
        assert!(status.contains("frozen against replay"));
        assert!(status.contains("502"));
    }

    #[test]
    fn reconciliation_requires_positive_movement_and_never_claims_attribution() {
        let unchanged = ProviderReadEvidence::Observed(ActionsRunControlProgress {
            run_attempt: 2,
            run_status: "completed".into(),
            run_conclusion: Some("failure".into()),
        });
        assert!(
            reconciliation_summary(
                ActionsRunControlAction::RerunAllJobs,
                2,
                "completed",
                &unchanged
            )
            .is_none()
        );
        let advanced = ProviderReadEvidence::Observed(ActionsRunControlProgress {
            run_attempt: 3,
            run_status: "in_progress".into(),
            run_conclusion: None,
        });
        let (resolved, summary) = reconciliation_summary(
            ActionsRunControlAction::RerunAllJobs,
            2,
            "completed",
            &advanced,
        )
        .expect("a later attempt is positive evidence");
        assert!(resolved);
        assert!(summary.contains("does not attribute"));

        let inconclusive = ProviderReadEvidence::Inconclusive {
            reason: "read failed".into(),
        };
        assert!(
            reconciliation_summary(
                ActionsRunControlAction::CancelRun,
                2,
                "in_progress",
                &inconclusive
            )
            .is_none()
        );
    }

    #[test]
    fn cancel_reconciliation_requires_the_frozen_attempt_and_a_cancelled_conclusion() {
        let other_attempt = ProviderReadEvidence::Observed(ActionsRunControlProgress {
            run_attempt: 3,
            run_status: "completed".into(),
            run_conclusion: Some("cancelled".into()),
        });
        assert!(
            reconciliation_summary(
                ActionsRunControlAction::CancelRun,
                2,
                "in_progress",
                &other_attempt
            )
            .is_none()
        );
        let exact = ProviderReadEvidence::Observed(ActionsRunControlProgress {
            run_attempt: 2,
            run_status: "completed".into(),
            run_conclusion: Some("cancelled".into()),
        });
        assert!(
            reconciliation_summary(ActionsRunControlAction::CancelRun, 2, "in_progress", &exact)
                .is_some()
        );
    }
}
