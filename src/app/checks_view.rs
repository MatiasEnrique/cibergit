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
