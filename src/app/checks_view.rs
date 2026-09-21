//! Read-only Checks selection and identity presentation.
use cibergit::domain::{ActionsLinkage, CheckKind, CheckShaClass, PullRequestCheck};
use gpui::actions;

pub const CHECKS_PAGE_SIZE: usize = 40;

actions!(
    checks,
    [
        OpenChecks,
        PreviousCheck,
        NextCheck,
        ToggleCheckIdentity,
        PreviousCheckPage,
        NextCheckPage,
        OpenSelectedCheckJobs,
        RefreshSelectedCheckJobs,
        PreviousJob,
        NextJob,
        PreviousJobPage,
        NextJobPage,
        LoadSelectedJobLog,
        ReturnToChecks,
        ReturnToJobs,
    ]
);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChecksSelection {
    pub selected_id: Option<String>,
    pub expanded_id: Option<String>,
    pub page: usize,
}

impl ChecksSelection {
    pub fn reconcile(&mut self, checks: &[PullRequestCheck]) {
        if checks.is_empty() {
            *self = Self::default();
            return;
        }
        let selected = self.selected_id.as_ref().and_then(|id| {
            checks
                .iter()
                .position(|check| check.coordinates.remote_id == *id)
        });
        let selected = selected.unwrap_or(0);
        self.selected_id = Some(checks[selected].coordinates.remote_id.clone());
        if self.expanded_id.as_ref().is_some_and(|id| {
            !checks
                .iter()
                .any(|check| check.coordinates.remote_id == *id)
        }) {
            self.expanded_id = None;
        }
        self.page = selected / CHECKS_PAGE_SIZE;
    }

    pub fn move_selection(&mut self, checks: &[PullRequestCheck], delta: isize) {
        self.reconcile(checks);
        let Some(current) = self.selected_id.as_ref().and_then(|id| {
            checks
                .iter()
                .position(|check| check.coordinates.remote_id == *id)
        }) else {
            return;
        };
        let next = current
            .saturating_add_signed(delta)
            .min(checks.len().saturating_sub(1));
        self.selected_id = Some(checks[next].coordinates.remote_id.clone());
        self.page = next / CHECKS_PAGE_SIZE;
    }

    pub fn move_page(&mut self, checks: &[PullRequestCheck], delta: isize) {
        let (_, pages, _) = checks_page(checks.len(), self.page);
        self.page = self
            .page
            .saturating_add_signed(delta)
            .min(pages.saturating_sub(1));
        let (_, _, range) = checks_page(checks.len(), self.page);
        self.selected_id = checks
            .get(range.start)
            .map(|check| check.coordinates.remote_id.clone());
    }

    pub fn activate(&mut self, id: &str) {
        self.selected_id = Some(id.to_owned());
        if self.expanded_id.as_deref() == Some(id) {
            self.expanded_id = None;
        } else {
            self.expanded_id = Some(id.to_owned());
        }
    }

    pub fn toggle_selected(&mut self) {
        let Some(selected) = self.selected_id.clone() else {
            return;
        };
        self.activate(&selected);
    }
}

pub fn checks_page(total: usize, requested_page: usize) -> (usize, usize, std::ops::Range<usize>) {
    let pages = total.div_ceil(CHECKS_PAGE_SIZE).max(1);
    let page = requested_page.min(pages - 1);
    let start = page.saturating_mul(CHECKS_PAGE_SIZE).min(total);
    let end = start.saturating_add(CHECKS_PAGE_SIZE).min(total);
    (page, pages, start..end)
}

/// What GitHub shows as a check's outcome: the status while it is unfinished,
/// the conclusion once it has one. A check run reports both; a commit status
/// carries its whole outcome in `status` and never has a conclusion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckState {
    Success,
    Failure,
    Pending,
    Skipped,
    Unknown,
}

impl CheckState {
    /// The word GitHub puts beside the icon, and what assistive tech hears.
    pub fn label(self) -> &'static str {
        match self {
            Self::Success => "passed",
            Self::Failure => "failed",
            Self::Pending => "in progress",
            Self::Skipped => "skipped",
            Self::Unknown => "state unknown",
        }
    }
}

pub fn check_state(check: &PullRequestCheck) -> CheckState {
    // An unrecognised value stays Unknown rather than being rounded to the
    // nearest familiar outcome: a check reported in a word this app has never
    // seen is exactly the case where a green tick would be a lie.
    if let Some(conclusion) = check.conclusion.as_deref() {
        return match conclusion.to_ascii_uppercase().as_str() {
            "SUCCESS" => CheckState::Success,
            "FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED" | "ERROR" => {
                CheckState::Failure
            }
            "NEUTRAL" | "SKIPPED" | "CANCELLED" | "STALE" => CheckState::Skipped,
            _ => CheckState::Unknown,
        };
    }
    match check.status.to_ascii_uppercase().as_str() {
        "SUCCESS" => CheckState::Success,
        "FAILURE" | "ERROR" => CheckState::Failure,
        "QUEUED" | "IN_PROGRESS" | "WAITING" | "PENDING" | "REQUESTED" | "EXPECTED" => {
            CheckState::Pending
        }
        _ => CheckState::Unknown,
    }
}

/// How many observed checks sit in each state. This counts the whole observed
/// set, not the visible page, and says nothing about checks GitHub did not
/// return; completeness is reported separately.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ChecksTally {
    pub success: usize,
    pub failure: usize,
    pub pending: usize,
    pub skipped: usize,
    pub unknown: usize,
}

impl ChecksTally {
    pub fn total(self) -> usize {
        self.success + self.failure + self.pending + self.skipped + self.unknown
    }

    /// The state the whole set reads as. A failure outranks unfinished work,
    /// which outranks an unreadable state, which outranks success: the worst
    /// news a reader has to act on comes first.
    pub fn headline(self) -> CheckState {
        if self.failure > 0 {
            CheckState::Failure
        } else if self.pending > 0 {
            CheckState::Pending
        } else if self.unknown > 0 {
            CheckState::Unknown
        } else if self.success > 0 {
            CheckState::Success
        } else {
            CheckState::Skipped
        }
    }

    /// `3 passed · 1 failed`, naming only the states actually present.
    pub fn parts(self) -> Vec<String> {
        [
            (self.failure, "failed"),
            (self.pending, "in progress"),
            (self.success, "passed"),
            (self.skipped, "skipped"),
            (self.unknown, "unknown"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, label)| format!("{count} {label}"))
        .collect()
    }
}

pub fn checks_tally(checks: &[PullRequestCheck]) -> ChecksTally {
    let mut tally = ChecksTally::default();
    for check in checks {
        match check_state(check) {
            CheckState::Success => tally.success += 1,
            CheckState::Failure => tally.failure += 1,
            CheckState::Pending => tally.pending += 1,
            CheckState::Skipped => tally.skipped += 1,
            CheckState::Unknown => tally.unknown += 1,
        }
    }
    tally
}

pub fn kind_label(check: &PullRequestCheck) -> &'static str {
    match (&check.kind, &check.actions_linkage) {
        (CheckKind::CheckRun, ActionsLinkage::Linked(_)) => "GitHub Actions",
        (CheckKind::CheckRun, _) => "Check run",
        (CheckKind::CommitStatus, _) => "Commit status",
    }
}

pub fn required_label(check: &PullRequestCheck) -> &'static str {
    match check.required {
        Some(true) => "Required",
        Some(false) => "Not required",
        None => "Requiredness unknown",
    }
}

pub fn sha_label(check: &PullRequestCheck) -> String {
    let class = match check.sha_class {
        CheckShaClass::Head => "PR head",
        CheckShaClass::MergeCandidate => "merge candidate",
        CheckShaClass::Other => "other commit",
        CheckShaClass::Unknown => "unknown commit",
    };
    check
        .commit_sha
        .as_deref()
        .map(|sha| format!("{} · {class}", &sha[..sha.len().min(8)]))
        .unwrap_or_else(|| class.to_owned())
}

pub fn linkage_label(check: &PullRequestCheck) -> String {
    match (&check.kind, &check.actions_linkage) {
        (CheckKind::CommitStatus, _) => "GitHub Actions linkage does not apply".into(),
        (_, ActionsLinkage::Linked(run)) => format!(
            "{} · run {} · attempt {}",
            run.workflow_name, run.run_number, run.run_attempt
        ),
        (_, ActionsLinkage::NoObservedLink) => "No linked GitHub Actions run observed".into(),
        (_, ActionsLinkage::Unknown) => "GitHub Actions linkage unknown".into(),
    }
}

pub fn identity_fields(check: &PullRequestCheck) -> Vec<(&'static str, String)> {
    let mut fields = vec![
        ("Kind", kind_label(check).into()),
        ("Required", required_label(check).into()),
        (
            "Observed commit",
            check.commit_sha.clone().unwrap_or_else(|| "Unknown".into()),
        ),
        ("Commit relation", sha_label(check)),
        ("Actions linkage", linkage_label(check)),
        ("Check node ID", check.coordinates.remote_id.clone()),
        (
            "Check database ID",
            check
                .database_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "Unknown".into()),
        ),
    ];
    if let Some(repository) = &check.commit_repository {
        fields.push(("Observed repository", repository.name_with_owner.clone()));
    }
    if let Some(suite) = &check.suite {
        fields.push(("Suite node ID", suite.node_id.clone()));
        fields.push((
            "Suite database ID",
            suite
                .database_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "Unknown".into()),
        ));
        fields.push(("Suite repository", suite.repository.name_with_owner.clone()));
        if let Some(app) = &suite.app {
            fields.push(("App", format!("{} ({})", app.name, app.slug)));
            fields.push(("App node ID", app.node_id.clone()));
        } else {
            fields.push(("App", "Unknown".into()));
        }
    } else if check.kind == CheckKind::CheckRun {
        fields.push(("Suite", "Unknown".into()));
    }
    if let ActionsLinkage::Linked(run) = &check.actions_linkage {
        fields.extend([
            ("Workflow run node ID", run.node_id.clone()),
            ("Workflow run database ID", run.database_id.to_string()),
            ("Workflow event", run.event.clone()),
            ("Workflow node ID", run.workflow_node_id.clone()),
            ("Workflow database ID", run.workflow_database_id.to_string()),
            ("GitHub workflow run", run.github_url.clone()),
        ]);
    }
    if let Some(permalink) = &check.github_permalink {
        fields.push(("GitHub check permalink", permalink.clone()));
    }
    if let Some(url) = &check.details_url {
        fields.push(("Integrator URL (display only)", url.clone()));
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::domain::{
        CheckRepositoryIdentity, CheckSuiteIdentity, ProviderCoordinates, WorkflowRunIdentity,
    };

    fn check(index: usize) -> PullRequestCheck {
        PullRequestCheck {
            coordinates: ProviderCoordinates {
                provider: "github".into(),
                host: "github.com".into(),
                owner: "owner".into(),
                repository: "repo".into(),
                pull_request: 7,
                remote_id: format!("CHECK-{index}"),
            },
            kind: CheckKind::CheckRun,
            name: format!("check {index}"),
            status: "COMPLETED".into(),
            conclusion: Some("SUCCESS".into()),
            description: None,
            details_url: Some("https://integrator.test/build".into()),
            github_permalink: Some("https://github.com/owner/repo/runs/123".into()),
            started_at: None,
            completed_at: None,
            required: Some(true),
            database_id: Some(10),
            suite: Some(CheckSuiteIdentity {
                node_id: "SUITE".into(),
                database_id: Some(20),
                repository: CheckRepositoryIdentity {
                    node_id: "REPO".into(),
                    name_with_owner: "owner/repo".into(),
                },
                app: None,
            }),
            commit_sha: Some("a".repeat(40)),
            commit_repository: None,
            sha_class: CheckShaClass::Head,
            actions_linkage: ActionsLinkage::Linked(WorkflowRunIdentity {
                node_id: "RUN".into(),
                database_id: 30,
                run_attempt: 2,
                run_number: 9,
                event: "pull_request".into(),
                github_url: "https://github.com/owner/repo/actions/runs/30".into(),
                workflow_node_id: "WORKFLOW".into(),
                workflow_database_id: 40,
                workflow_name: "CI".into(),
            }),
        }
    }

    /// The mapping is the whole feature: a green tick on a failed check, or on
    /// a word this app has never seen, is worse than no icon at all.
    #[test]
    fn check_state_reads_conclusion_first_and_never_guesses_an_outcome() {
        let with = |status: &str, conclusion: Option<&str>| {
            let mut value = check(1);
            value.status = status.into();
            value.conclusion = conclusion.map(str::to_owned);
            check_state(&value)
        };
        assert_eq!(with("COMPLETED", Some("SUCCESS")), CheckState::Success);
        for failure in ["FAILURE", "TIMED_OUT", "STARTUP_FAILURE", "ACTION_REQUIRED"] {
            assert_eq!(
                with("COMPLETED", Some(failure)),
                CheckState::Failure,
                "{failure}"
            );
        }
        for skipped in ["NEUTRAL", "SKIPPED", "CANCELLED", "STALE"] {
            assert_eq!(
                with("COMPLETED", Some(skipped)),
                CheckState::Skipped,
                "{skipped}"
            );
        }
        // Unfinished check runs carry no conclusion at all.
        for pending in ["QUEUED", "IN_PROGRESS", "WAITING", "REQUESTED"] {
            assert_eq!(with(pending, None), CheckState::Pending, "{pending}");
        }
        // A commit status keeps its whole outcome in `status`.
        assert_eq!(with("SUCCESS", None), CheckState::Success);
        assert_eq!(with("ERROR", None), CheckState::Failure);
        assert_eq!(with("PENDING", None), CheckState::Pending);
        // A conclusion GitHub adds after this was written must not be rounded
        // to the nearest familiar one.
        assert_eq!(with("COMPLETED", Some("EMBARGOED")), CheckState::Unknown);
        assert_eq!(with("SOMETHING_NEW", None), CheckState::Unknown);
    }

    /// The headline is what a reader acts on, so the worst news has to win.
    #[test]
    fn a_tally_leads_with_the_worst_state_present() {
        let state = |status: &str, conclusion: Option<&str>| {
            let mut value = check(1);
            value.status = status.into();
            value.conclusion = conclusion.map(str::to_owned);
            value
        };
        let passed = state("COMPLETED", Some("SUCCESS"));
        let failed = state("COMPLETED", Some("FAILURE"));
        let running = state("IN_PROGRESS", None);
        let skipped = state("COMPLETED", Some("SKIPPED"));

        assert_eq!(checks_tally(&[]).headline(), CheckState::Skipped);
        assert_eq!(
            checks_tally(&[passed.clone(), passed.clone()]).headline(),
            CheckState::Success
        );
        assert_eq!(
            checks_tally(&[passed.clone(), running.clone()]).headline(),
            CheckState::Pending
        );
        assert_eq!(
            checks_tally(&[passed.clone(), running.clone(), failed.clone()]).headline(),
            CheckState::Failure,
            "one failure outranks any amount of good news"
        );

        let tally = checks_tally(&[passed, failed, running, skipped]);
        assert_eq!(tally.total(), 4);
        assert_eq!(
            tally.parts(),
            vec!["1 failed", "1 in progress", "1 passed", "1 skipped"],
            "only the states actually present are named, worst first"
        );
    }

    #[test]
    fn bounded_pages_reach_every_enumerated_check_and_keyboard_crosses_page_boundary() {
        let checks = (0..81).map(check).collect::<Vec<_>>();
        assert_eq!(checks_page(checks.len(), 0).2, 0..40);
        assert_eq!(checks_page(checks.len(), 1).2, 40..80);
        assert_eq!(checks_page(checks.len(), 2).2, 80..81);
        let mut selection = ChecksSelection::default();
        selection.reconcile(&checks);
        selection.move_selection(&checks, 40);
        assert_eq!(selection.page, 1);
        assert_eq!(selection.selected_id.as_deref(), Some("CHECK-40"));
        selection.move_page(&checks, 1);
        assert_eq!(selection.page, 2);
        assert_eq!(selection.selected_id.as_deref(), Some("CHECK-80"));
    }

    #[test]
    fn familiar_labels_keep_github_and_integrator_destinations_distinct() {
        let check = check(1);
        assert_eq!(kind_label(&check), "GitHub Actions");
        assert_eq!(required_label(&check), "Required");
        assert_eq!(sha_label(&check), "aaaaaaaa · PR head");
        let fields = identity_fields(&check);
        assert!(fields.contains(&(
            "GitHub check permalink",
            "https://github.com/owner/repo/runs/123".into()
        )));
        assert!(fields.contains(&(
            "Integrator URL (display only)",
            "https://integrator.test/build".into()
        )));
    }

    #[test]
    fn unlinked_check_run_and_spoofed_name_never_become_actions() {
        let mut check = check(1);
        check.name = "GitHub Actions / CI".into();
        check.actions_linkage = ActionsLinkage::NoObservedLink;
        assert_eq!(kind_label(&check), "Check run");
        assert_eq!(
            linkage_label(&check),
            "No linked GitHub Actions run observed"
        );
        check.actions_linkage = ActionsLinkage::Unknown;
        assert_eq!(kind_label(&check), "Check run");
        assert_eq!(linkage_label(&check), "GitHub Actions linkage unknown");
    }
}
