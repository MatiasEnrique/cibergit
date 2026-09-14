//! Per-tab, memory-only Actions Jobs and Log presentation state.

use cibergit::{
    domain::{ActionsAttemptLocator, ActionsJobLog, ActionsJobsSnapshot},
    providers::{ActionsCancellation, ActionsReadError},
};
use std::sync::Arc;

pub(super) const JOBS_PAGE_SIZE: usize = 40;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum CiPane {
    #[default]
    Checks,
    Jobs,
    Log,
}

#[derive(Clone, Debug, Default)]
pub(super) enum MemoryRead<T> {
    #[default]
    Empty,
    Loading {
        historical: Option<T>,
        operation_id: u64,
    },
    Fresh(T),
    Historical(T, String),
    Unavailable {
        historical: Option<T>,
        notice: String,
    },
}

impl<T: Clone> MemoryRead<T> {
    pub(super) fn visible(&self) -> Option<&T> {
        match self {
            Self::Loading { historical, .. } | Self::Unavailable { historical, .. } => {
                historical.as_ref()
            }
            Self::Fresh(value) | Self::Historical(value, _) => Some(value),
            Self::Empty => None,
        }
    }

    fn prior_as_historical(&self) -> Option<T> {
        self.visible().cloned()
    }

    pub(super) fn notice(&self) -> Option<&str> {
        match self {
            Self::Loading { .. } => Some("Loading exact read-only GitHub Actions data…"),
            Self::Historical(_, notice) | Self::Unavailable { notice, .. } => Some(notice),
            Self::Empty | Self::Fresh(_) => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct CiOperation {
    pub(super) generation: u64,
    pub(super) operation_id: u64,
    pub(super) cancellation: ActionsCancellation,
}

#[derive(Clone, Debug, Default)]
pub(super) struct CiReadState {
    pub(super) pane: CiPane,
    pub(super) frozen_locator: Option<ActionsAttemptLocator>,
    pub(super) jobs: MemoryRead<ActionsJobsSnapshot>,
    pub(super) selected_job_id: Option<u64>,
    pub(super) jobs_page: usize,
    pub(super) log: MemoryRead<Arc<ActionsJobLog>>,
    pub(super) generation: u64,
    next_operation_id: u64,
    active: Option<CiOperation>,
}

impl CiReadState {
    pub(super) fn begin_jobs(&mut self, locator: ActionsAttemptLocator) -> CiOperation {
        self.cancel_active();
        let same_locator = self.frozen_locator.as_ref() == Some(&locator);
        self.generation = self.generation.saturating_add(1);
        self.next_operation_id = self.next_operation_id.saturating_add(1);
        let operation = CiOperation {
            generation: self.generation,
            operation_id: self.next_operation_id,
            cancellation: ActionsCancellation::new(),
        };
        self.jobs = MemoryRead::Loading {
            historical: same_locator
                .then(|| self.jobs.prior_as_historical())
                .flatten(),
            operation_id: operation.operation_id,
        };
        self.log = match self.log.prior_as_historical() {
            Some(log) => MemoryRead::Historical(
                log,
                if same_locator {
                    "Previous log is historical until the exact job is revalidated.".into()
                } else {
                    "Detached historical log for the prior exact attempt.".into()
                },
            ),
            None => MemoryRead::Empty,
        };
        self.frozen_locator = Some(locator);
        if !same_locator {
            self.selected_job_id = None;
            self.jobs_page = 0;
        }
        self.pane = CiPane::Jobs;
        self.active = Some(operation.clone());
        operation
    }

    pub(super) fn finish_jobs(
        &mut self,
        operation: &CiOperation,
        result: Result<ActionsJobsSnapshot, ActionsReadError>,
    ) -> bool {
        if !self.owns(operation) {
            return false;
        }
        self.active = None;
        match result {
            Ok(snapshot) if self.frozen_locator.as_ref() == Some(&snapshot.attempt.key.locator) => {
                let prior = self.selected_job_id;
                self.selected_job_id = prior
                    .filter(|id| snapshot.jobs.iter().any(|job| job.id == *id))
                    .or(Some(snapshot.selected_check_job_id));
                self.jobs_page = selected_page(&snapshot, self.selected_job_id);
                self.jobs = MemoryRead::Fresh(snapshot);
            }
            Ok(_) => {
                self.jobs = MemoryRead::Unavailable {
                    historical: self.jobs.prior_as_historical(),
                    notice: "Jobs callback belonged to another exact run attempt.".into(),
                };
                self.selected_job_id = None;
            }
            Err(error) => {
                self.jobs = MemoryRead::Unavailable {
                    historical: self.jobs.prior_as_historical(),
                    notice: error.to_string(),
                };
            }
        }
        true
    }

    pub(super) fn begin_log(&mut self) -> Option<(CiOperation, ActionsJobsSnapshot, u64)> {
        let (snapshot, job_id) = self.log_target()?;
        self.cancel_active();
        self.generation = self.generation.saturating_add(1);
        self.next_operation_id = self.next_operation_id.saturating_add(1);
        let operation = CiOperation {
            generation: self.generation,
            operation_id: self.next_operation_id,
            cancellation: ActionsCancellation::new(),
        };
        self.log = MemoryRead::Loading {
            historical: self.log.prior_as_historical(),
            operation_id: operation.operation_id,
        };
        self.pane = CiPane::Log;
        self.active = Some(operation.clone());
        Some((operation, snapshot, job_id))
    }

    pub(super) fn log_target(&mut self) -> Option<(ActionsJobsSnapshot, u64)> {
        let snapshot = self.jobs.visible()?.clone();
        if self.frozen_locator.as_ref() != Some(&snapshot.attempt.key.locator) {
            self.selected_job_id = None;
            return None;
        }
        let job_id = self.selected_job_id?;
        if !snapshot.jobs.iter().any(|job| job.id == job_id) {
            return None;
        }
        Some((snapshot, job_id))
    }

    pub(super) fn finish_log(
        &mut self,
        operation: &CiOperation,
        result: Result<ActionsJobLog, ActionsReadError>,
    ) -> bool {
        if !self.owns(operation) {
            return false;
        }
        let expected = self.jobs.visible().map(|snapshot| {
            (
                snapshot.attempt.key.locator.clone(),
                snapshot.observation_id,
                self.selected_job_id,
            )
        });
        self.active = None;
        self.log = match result {
            Ok(log)
                if expected
                    .as_ref()
                    .is_some_and(|(locator, observation, selected)| {
                        *locator == log.key.locator
                            && *observation == log.jobs_observation_id
                            && *selected == Some(log.job.id)
                    }) =>
            {
                MemoryRead::Fresh(Arc::new(log))
            }
            Ok(_) => MemoryRead::Unavailable {
                historical: self.log.prior_as_historical(),
                notice: "Log callback belonged to another exact job selection.".into(),
            },
            Err(error) => MemoryRead::Unavailable {
                historical: self.log.prior_as_historical(),
                notice: error.to_string(),
            },
        };
        true
    }

    pub(super) fn owns(&self, operation: &CiOperation) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.generation == operation.generation
                && active.operation_id == operation.operation_id
                && !operation.cancellation.is_cancelled()
        })
    }

    pub(super) fn release(&mut self, operation: &CiOperation) -> bool {
        if !self.owns(operation) {
            return false;
        }
        self.active = None;
        self.retire_loading(operation.operation_id, "Actions read result became stale.");
        true
    }

    pub(super) fn reconcile_locator(&mut self, locator: Option<&ActionsAttemptLocator>) {
        if self.frozen_locator.as_ref() == locator {
            if let MemoryRead::Fresh(snapshot) = &self.jobs {
                self.jobs = MemoryRead::Historical(
                    snapshot.clone(),
                    "Jobs are historical until the exact run attempt is revalidated.".into(),
                );
            }
            if let MemoryRead::Fresh(log) = &self.log {
                self.log = MemoryRead::Historical(
                    log.clone(),
                    "Log is historical until the exact job is revalidated.".into(),
                );
            }
            return;
        }
        self.cancel_active();
        if self.frozen_locator.is_some() {
            self.jobs = self
                .jobs
                .prior_as_historical()
                .map_or(MemoryRead::Empty, |value| {
                    MemoryRead::Historical(
                        value,
                        "Detached historical jobs for the prior exact attempt.".into(),
                    )
                });
            self.log = self
                .log
                .prior_as_historical()
                .map_or(MemoryRead::Empty, |value| {
                    MemoryRead::Historical(
                        value,
                        "Detached historical log for the prior exact job.".into(),
                    )
                });
        }
        self.frozen_locator = locator.cloned();
        self.selected_job_id = None;
        self.jobs_page = 0;
        self.pane = CiPane::Checks;
    }

    pub(super) fn cancel_active(&mut self) {
        if let Some(active) = self.active.take() {
            active.cancellation.cancel();
            self.retire_loading(active.operation_id, "Actions read was cancelled.");
        }
    }

    fn retire_loading(&mut self, operation_id: u64, notice: &str) {
        if matches!(
            &self.jobs,
            MemoryRead::Loading {
                operation_id: current,
                ..
            } if *current == operation_id
        ) {
            self.jobs = MemoryRead::Unavailable {
                historical: self.jobs.prior_as_historical(),
                notice: notice.into(),
            };
        }
        if matches!(
            &self.log,
            MemoryRead::Loading {
                operation_id: current,
                ..
            } if *current == operation_id
        ) {
            self.log = MemoryRead::Unavailable {
                historical: self.log.prior_as_historical(),
                notice: notice.into(),
            };
        }
    }

    pub(super) fn move_job(&mut self, delta: isize) {
        let Some(snapshot) = self.jobs.visible() else {
            return;
        };
        let current = self
            .selected_job_id
            .and_then(|id| snapshot.jobs.iter().position(|job| job.id == id))
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(snapshot.jobs.len().saturating_sub(1));
        let selected = snapshot.jobs.get(next).map(|job| job.id);
        self.select_job(selected);
        self.jobs_page = next / JOBS_PAGE_SIZE;
    }

    pub(super) fn move_job_page(&mut self, delta: isize) {
        let Some(snapshot) = self.jobs.visible() else {
            return;
        };
        let pages = snapshot.jobs.len().div_ceil(JOBS_PAGE_SIZE).max(1);
        self.jobs_page = self.jobs_page.saturating_add_signed(delta).min(pages - 1);
        let selected = snapshot
            .jobs
            .get(self.jobs_page * JOBS_PAGE_SIZE)
            .map(|job| job.id);
        self.select_job(selected);
    }

    pub(super) fn select_job(&mut self, selected: Option<u64>) {
        if self.selected_job_id != selected {
            self.cancel_active();
            if let Some(log) = self.log.prior_as_historical() {
                self.log = MemoryRead::Historical(
                    log,
                    "Detached historical log for the previously selected exact job.".into(),
                );
            }
        }
        self.selected_job_id = selected;
    }
}

impl Drop for CiReadState {
    fn drop(&mut self) {
        self.cancel_active();
    }
}

pub(super) fn jobs_page(total: usize, requested: usize) -> (usize, usize, std::ops::Range<usize>) {
    let pages = total.div_ceil(JOBS_PAGE_SIZE).max(1);
    let page = requested.min(pages - 1);
    let start = page.saturating_mul(JOBS_PAGE_SIZE).min(total);
    let end = start.saturating_add(JOBS_PAGE_SIZE).min(total);
    (page, pages, start..end)
}

fn selected_page(snapshot: &ActionsJobsSnapshot, selected: Option<u64>) -> usize {
    selected
        .and_then(|id| snapshot.jobs.iter().position(|job| job.id == id))
        .map_or(0, |index| index / JOBS_PAGE_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::domain::*;
    use cibergit::providers::ActionsReadErrorCategory;

    fn locator(attempt: u64) -> ActionsAttemptLocator {
        let repo = CheckRepositoryIdentity {
            node_id: "R".into(),
            name_with_owner: "o/r".into(),
        };
        ActionsAttemptLocator {
            account: Account {
                host: "github.com".into(),
                login: "alice".into(),
            },
            base_repository: repo.clone(),
            pull_request_node_id: "PR".into(),
            pull_request_number: 7,
            observed_head_sha: "a".repeat(40),
            head_repository: repo.clone(),
            rollup_commit_sha: "a".repeat(40),
            rollup_repository: repo.clone(),
            check_node_id: "C".into(),
            check_database_id: 9,
            check_commit_sha: "a".repeat(40),
            check_repository: repo.clone(),
            suite: CheckSuiteIdentity {
                node_id: "S".into(),
                database_id: Some(8),
                repository: repo,
                app: None,
            },
            workflow_run: WorkflowRunIdentity {
                node_id: "W".into(),
                database_id: 6,
                run_attempt: attempt,
                run_number: 5,
                event: "pull_request".into(),
                github_url: "https://github.com/o/r/actions/runs/6".into(),
                workflow_node_id: "WF".into(),
                workflow_database_id: 4,
                workflow_name: "CI".into(),
            },
        }
    }

    fn job(id: u64, attempt: u64) -> ActionsJob {
        ActionsJob {
            id,
            node_id: format!("J{id}"),
            run_id: 6,
            run_attempt: attempt,
            head_sha: "a".repeat(40),
            check_run_database_id: id + 100,
            check_run_url: format!("https://api.github.com/repos/o/r/check-runs/{}", id + 100),
            name: format!("job {id}"),
            status: "completed".into(),
            conclusion: Some("success".into()),
            started_at: None,
            completed_at: None,
            api_url: format!("https://api.github.com/repos/o/r/actions/jobs/{id}"),
            html_url: format!("https://github.com/o/r/actions/runs/6/job/{id}"),
            steps: vec![],
        }
    }

    fn snapshot(attempt: u64, observation: u64, ids: &[u64]) -> ActionsJobsSnapshot {
        let locator = locator(attempt);
        let jobs = ids.iter().map(|id| job(*id, attempt)).collect::<Vec<_>>();
        ActionsJobsSnapshot {
            attempt: ActionsRunAttemptObservation {
                key: ActionsAttemptKey {
                    locator,
                    viewer_node_id: "V".into(),
                    viewer_login: "alice".into(),
                },
                status: "completed".into(),
                conclusion: Some("success".into()),
                api_url: "https://api.github.com/run".into(),
                html_url: "https://github.com/run".into(),
                workflow_url: "https://api.github.com/workflow".into(),
                returned_pull_requests: vec![],
                relation: ActionsHeadRelation::Unknown,
                observed_at_unix_ms: 1,
            },
            provider_ordered_job_ids: ids.to_vec(),
            selected_check_job_id: ids[0],
            jobs,
            complete: true,
            observed_at_unix_ms: 1,
            observation_id: observation,
        }
    }

    fn log(snapshot: &ActionsJobsSnapshot, id: u64) -> ActionsJobLog {
        ActionsJobLog {
            key: snapshot.attempt.key.clone(),
            job: snapshot
                .jobs
                .iter()
                .find(|job| job.id == id)
                .unwrap()
                .clone(),
            jobs_observation_id: snapshot.observation_id,
            raw_byte_count: 2,
            line_count: 1,
            sanitized_text: "ok".into(),
            observed_at_unix_ms: 2,
            provenance: ActionsLogProvenance::FreshExactRead,
        }
    }

    #[test]
    fn stale_operation_cannot_clear_or_apply_over_newer_owner() {
        let mut state = CiReadState::default();
        let old = state.begin_jobs(locator(1));
        let current = state.begin_jobs(locator(2));
        assert!(old.cancellation.is_cancelled());
        assert!(!state.finish_jobs(
            &old,
            Err(ActionsReadError::closed(
                ActionsReadErrorCategory::Cancelled
            ))
        ));
        assert!(state.owns(&current));
    }

    #[test]
    fn changed_locator_never_presents_or_dispatches_foreign_jobs() {
        let mut state = CiReadState::default();
        let load_a = state.begin_jobs(locator(1));
        assert!(state.finish_jobs(&load_a, Ok(snapshot(1, 10, &[11]))));
        assert_eq!(state.selected_job_id, Some(11));

        let load_b = state.begin_jobs(locator(2));
        assert!(state.jobs.visible().is_none());
        assert_eq!(state.selected_job_id, None);
        assert!(state.finish_jobs(
            &load_b,
            Err(ActionsReadError::closed(
                ActionsReadErrorCategory::Unavailable,
            )),
        ));
        assert!(state.jobs.visible().is_none());
        assert!(state.begin_log().is_none());
    }

    #[test]
    fn changed_job_selection_cancels_and_fences_the_old_log() {
        let mut state = CiReadState::default();
        let load = state.begin_jobs(locator(1));
        let jobs = snapshot(1, 10, &[11, 12]);
        assert!(state.finish_jobs(&load, Ok(jobs.clone())));
        let (log_operation, _, id) = state.begin_log().unwrap();
        assert_eq!(id, 11);
        state.select_job(Some(12));
        assert!(log_operation.cancellation.is_cancelled());
        assert!(!state.finish_log(&log_operation, Ok(log(&jobs, 11))));
        assert_eq!(state.selected_job_id, Some(12));
        assert!(!matches!(state.log, MemoryRead::Fresh(_)));
    }

    #[test]
    fn owned_release_retires_loading_without_touching_newer_work() {
        let mut state = CiReadState::default();
        let stale = state.begin_jobs(locator(1));
        assert!(state.release(&stale));
        assert!(matches!(state.jobs, MemoryRead::Unavailable { .. }));

        let old = state.begin_jobs(locator(1));
        let current = state.begin_jobs(locator(2));
        assert!(!state.release(&old));
        assert!(state.owns(&current));
        assert!(matches!(
            state.jobs,
            MemoryRead::Loading { operation_id, .. }
                if operation_id == current.operation_id
        ));
    }
}
