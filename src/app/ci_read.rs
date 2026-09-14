//! Per-tab, memory-only Actions Jobs and Log presentation state.

use cibergit::{
    domain::{ActionsAttemptLocator, ActionsJobLog, ActionsJobsSnapshot},
    providers::{ActionsCancellation, ActionsReadError},
};

pub(super) const JOBS_PAGE_SIZE: usize = 40;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum CiPane {
    #[default]
    Checks,
    Jobs,
    Log,
}

#[derive(Clone, Debug)]
pub(super) enum MemoryRead<T> {
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

impl<T> Default for MemoryRead<T> {
    fn default() -> Self {
        Self::Empty
    }
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
    pub(super) log: MemoryRead<ActionsJobLog>,
    pub(super) generation: u64,
    next_operation_id: u64,
    active: Option<CiOperation>,
}

impl CiReadState {
    pub(super) fn begin_jobs(&mut self, locator: ActionsAttemptLocator) -> CiOperation {
        self.cancel_active();
        self.generation = self.generation.saturating_add(1);
        self.next_operation_id = self.next_operation_id.saturating_add(1);
        let operation = CiOperation {
            generation: self.generation,
            operation_id: self.next_operation_id,
            cancellation: ActionsCancellation::new(),
        };
        self.jobs = MemoryRead::Loading {
            historical: self.jobs.prior_as_historical(),
            operation_id: operation.operation_id,
        };
        self.log = match self.log.prior_as_historical() {
            Some(log) => MemoryRead::Historical(
                log,
                "Previous log is historical until the exact job is revalidated.".into(),
            ),
            None => MemoryRead::Empty,
        };
        self.frozen_locator = Some(locator);
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
            Ok(snapshot) => {
                let prior = self.selected_job_id;
                self.selected_job_id = prior
                    .filter(|id| snapshot.jobs.iter().any(|job| job.id == *id))
                    .or(Some(snapshot.selected_check_job_id));
                self.jobs_page = selected_page(&snapshot, self.selected_job_id);
                self.jobs = MemoryRead::Fresh(snapshot);
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
        let snapshot = self.jobs.visible()?.clone();
        let job_id = self.selected_job_id?;
        if !snapshot.jobs.iter().any(|job| job.id == job_id) {
            return None;
        }
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

    pub(super) fn finish_log(
        &mut self,
        operation: &CiOperation,
        result: Result<ActionsJobLog, ActionsReadError>,
    ) -> bool {
        if !self.owns(operation) {
            return false;
        }
        self.active = None;
        self.log = match result {
            Ok(log) => MemoryRead::Fresh(log),
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
        self.selected_job_id = snapshot.jobs.get(next).map(|job| job.id);
        self.jobs_page = next / JOBS_PAGE_SIZE;
    }

    pub(super) fn move_job_page(&mut self, delta: isize) {
        let Some(snapshot) = self.jobs.visible() else {
            return;
        };
        let pages = snapshot.jobs.len().div_ceil(JOBS_PAGE_SIZE).max(1);
        self.jobs_page = self.jobs_page.saturating_add_signed(delta).min(pages - 1);
        self.selected_job_id = snapshot
            .jobs
            .get(self.jobs_page * JOBS_PAGE_SIZE)
            .map(|job| job.id);
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
}
