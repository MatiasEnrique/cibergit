use cibergit::{
    domain::{PullRequest, Repository},
    workspace::{Filter, GroupBy, PersonalFilter, SavedView, WorkspaceState, group_path},
};
use std::{cmp::Reverse, rc::Rc, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GroupKind {
    Repository,
    TargetBranch,
    SourceBranch,
    Stack,
}

impl GroupKind {
    const ALL: [Self; 4] = [
        Self::Repository,
        Self::TargetBranch,
        Self::SourceBranch,
        Self::Stack,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Repository => "Repository",
            Self::TargetBranch => "Target branch",
            Self::SourceBranch => "Source branch",
            Self::Stack => "Stack",
        }
    }

    fn matches(self, group: &GroupBy) -> bool {
        matches!(
            (self, group),
            (Self::Repository, GroupBy::Repository)
                | (Self::TargetBranch, GroupBy::TargetBranch)
                | (
                    Self::SourceBranch,
                    GroupBy::SourceBranch | GroupBy::SourcePrefix(_)
                )
                | (Self::Stack, GroupBy::Stack)
        )
    }

    fn from_group(group: &GroupBy) -> Self {
        match group {
            GroupBy::Repository => Self::Repository,
            GroupBy::TargetBranch => Self::TargetBranch,
            GroupBy::SourceBranch | GroupBy::SourcePrefix(_) => Self::SourceBranch,
            GroupBy::Stack => Self::Stack,
        }
    }

    fn group(self) -> GroupBy {
        match self {
            Self::Repository => GroupBy::Repository,
            Self::TargetBranch => GroupBy::TargetBranch,
            Self::SourceBranch => GroupBy::SourceBranch,
            Self::Stack => GroupBy::Stack,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct ViewEditorController {
    draft: Option<SavedView>,
}

impl ViewEditorController {
    pub(super) fn new() -> Self {
        Self { draft: None }
    }

    pub(super) fn is_open(&self) -> bool {
        self.draft.is_some()
    }

    pub(super) fn begin(&mut self, workspace: &WorkspaceState) {
        self.draft = Some(workspace.view());
    }

    pub(super) fn cancel(&mut self) {
        self.draft = None;
    }

    pub(super) fn draft(&self) -> Option<&SavedView> {
        self.draft.as_ref()
    }

    pub(super) fn set_personal(&mut self, personal: PersonalFilter) {
        if let Some(draft) = &mut self.draft {
            draft.filter.personal = personal;
        }
    }

    pub(super) fn set_draft_state(&mut self, draft_state: Option<bool>) {
        if let Some(draft) = &mut self.draft {
            draft.filter.draft = draft_state;
        }
    }

    pub(super) fn set_pr_state(&mut self, state: impl Into<String>) {
        if let Some(draft) = &mut self.draft {
            draft.filter.state = state.into();
        }
    }

    pub(super) fn add_group(&mut self) {
        let Some(draft) = &mut self.draft else { return };
        if let Some(kind) = GroupKind::ALL
            .into_iter()
            .find(|kind| !draft.groups.iter().any(|group| kind.matches(group)))
        {
            draft.groups.push(kind.group());
        }
    }

    pub(super) fn remove_group(&mut self, index: usize) {
        if let Some(draft) = &mut self.draft
            && index < draft.groups.len()
        {
            draft.groups.remove(index);
        }
    }

    pub(super) fn move_group(&mut self, index: usize, offset: isize) {
        let Some(draft) = &mut self.draft else { return };
        let Some(target) = index.checked_add_signed(offset) else {
            return;
        };
        if index < draft.groups.len() && target < draft.groups.len() {
            draft.groups.swap(index, target);
        }
    }

    pub(super) fn cycle_group(&mut self, index: usize, prefix: &str) {
        let Some(draft) = &mut self.draft else { return };
        let Some(group) = draft.groups.get(index) else {
            return;
        };
        let current = GroupKind::from_group(group);
        let current_index = GroupKind::ALL
            .iter()
            .position(|kind| *kind == current)
            .unwrap_or_default();
        for step in 1..=GroupKind::ALL.len() {
            let candidate = GroupKind::ALL[(current_index + step) % GroupKind::ALL.len()];
            if !draft
                .groups
                .iter()
                .enumerate()
                .any(|(other_index, group)| other_index != index && candidate.matches(group))
            {
                draft.groups[index] =
                    if candidate == GroupKind::SourceBranch && !prefix.trim().is_empty() {
                        GroupBy::SourcePrefix(prefix.trim().to_owned())
                    } else {
                        candidate.group()
                    };
                break;
            }
        }
    }

    pub(super) fn set_source_group_prefix(&mut self, index: usize, prefix: Option<&str>) {
        let Some(draft) = &mut self.draft else { return };
        let Some(group) = draft.groups.get_mut(index) else {
            return;
        };
        if !matches!(group, GroupBy::SourceBranch | GroupBy::SourcePrefix(_)) {
            return;
        }
        *group = match prefix.map(str::trim) {
            Some(prefix) => GroupBy::SourcePrefix(prefix.to_owned()),
            None => GroupBy::SourceBranch,
        };
    }

    pub(super) fn replace_filter(&mut self, filter: Filter) {
        if let Some(draft) = &mut self.draft {
            draft.filter = filter;
        }
    }

    #[cfg(feature = "ui-smoke")]
    pub(super) fn replace_groups(&mut self, groups: Vec<GroupBy>) {
        if let Some(draft) = &mut self.draft {
            draft.groups = groups;
        }
    }

    pub(super) fn apply(
        &mut self,
        workspace: &mut WorkspaceState,
        name: &str,
    ) -> Result<(), String> {
        let mut draft = self
            .draft
            .clone()
            .ok_or_else(|| "No view changes are open".to_owned())?;
        draft.name = valid_name(name)?;
        validate_groups(&draft.groups)?;
        normalize_workspace(workspace);
        workspace.views[workspace.selected_view] = draft;
        self.draft = None;
        Ok(())
    }

    pub(super) fn save_as(
        &mut self,
        workspace: &mut WorkspaceState,
        name: &str,
    ) -> Result<(), String> {
        let mut draft = self
            .draft
            .clone()
            .ok_or_else(|| "No view changes are open".to_owned())?;
        draft.name = valid_name(name)?;
        validate_groups(&draft.groups)?;
        normalize_workspace(workspace);
        workspace.views.push(draft);
        workspace.selected_view = workspace.views.len() - 1;
        self.draft = None;
        Ok(())
    }

    pub(super) fn delete_selected(&mut self, workspace: &mut WorkspaceState) {
        normalize_workspace(workspace);
        if workspace.views.len() == 1 {
            workspace.views[0] = SavedView::default();
            workspace.selected_view = 0;
        } else {
            workspace.views.remove(workspace.selected_view);
            workspace.selected_view = workspace.selected_view.min(workspace.views.len() - 1);
        }
        self.draft = None;
    }
}

fn valid_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        Err("Give this view a name".into())
    } else {
        Ok(name.to_owned())
    }
}

fn validate_groups(groups: &[GroupBy]) -> Result<(), String> {
    for (index, group) in groups.iter().enumerate() {
        let kind = GroupKind::from_group(group);
        if groups[..index].iter().any(|other| kind.matches(other)) {
            return Err(format!("{} can only appear once", kind.label()));
        }
        if matches!(group, GroupBy::SourcePrefix(prefix) if prefix.trim().is_empty()) {
            return Err("Enter a source branch prefix".into());
        }
    }
    Ok(())
}

fn normalize_workspace(workspace: &mut WorkspaceState) {
    if workspace.views.is_empty() {
        workspace.views.push(SavedView::default());
    }
    workspace.selected_view = workspace.selected_view.min(workspace.views.len() - 1);
}

pub(super) struct RepositoryPulls<'a> {
    pub(super) index: usize,
    pub(super) repository: &'a Repository,
    pub(super) pull_requests: &'a [PullRequest],
}

pub(super) enum SidebarRow {
    Group {
        depth: usize,
        label: String,
    },
    Pull {
        repository_index: usize,
        repository_key: String,
        pull_request: Box<PullRequest>,
    },
}

/// Keeps the last immutable inventory alive so pointer identity cannot be reused.
#[derive(Default)]
pub(super) struct SidebarCache {
    view: Option<SavedView>,
    inventories: Vec<(Repository, Arc<Vec<PullRequest>>)>,
    rows: Rc<[SidebarRow]>,
    pub participating_incomplete: bool,
}

impl SidebarCache {
    pub fn rows_for(
        &mut self,
        repositories: &[(Repository, Arc<Vec<PullRequest>>)],
        view: &SavedView,
    ) -> Rc<[SidebarRow]> {
        if self.view.as_ref() != Some(view)
            || self.inventories.len() != repositories.len()
            || self
                .inventories
                .iter()
                .zip(repositories)
                .any(|((old_repo, old), (repo, pulls))| {
                    old_repo != repo || !Arc::ptr_eq(old, pulls)
                })
        {
            let inventories = repositories
                .iter()
                .enumerate()
                .map(|(index, (repository, pulls))| RepositoryPulls {
                    index,
                    repository,
                    pull_requests: pulls,
                })
                .collect::<Vec<_>>();
            self.rows = compose_sidebar_rows(&inventories, view).into();
            self.participating_incomplete = view.filter.personal == PersonalFilter::Participating
                && repositories
                    .iter()
                    .any(|(_, pulls)| pulls.iter().any(|pull| !pull.participants_complete));
            self.inventories = repositories.to_vec();
            self.view = Some(view.clone());
        }
        self.rows.clone()
    }
}

struct GroupedPull<'a> {
    repository_index: usize,
    repository_key: String,
    pull_request: &'a PullRequest,
    path: Vec<String>,
}

pub(super) fn compose_sidebar_rows(
    repositories: &[RepositoryPulls<'_>],
    view: &SavedView,
) -> Vec<SidebarRow> {
    let mut pulls = repositories
        .iter()
        .flat_map(|runtime| {
            runtime
                .pull_requests
                .iter()
                .filter(move |pull_request| {
                    view.filter
                        .matches(pull_request, &runtime.repository.account.login)
                })
                .map(move |pull_request| GroupedPull {
                    repository_index: runtime.index,
                    repository_key: runtime.repository.cache_key(),
                    pull_request,
                    path: group_path(
                        runtime.repository,
                        pull_request,
                        &view.groups,
                        runtime.pull_requests,
                    )
                    .into_iter()
                    .map(|label| {
                        if label.trim().is_empty() {
                            "Not set".to_owned()
                        } else {
                            label
                        }
                    })
                    .collect(),
                })
        })
        .collect::<Vec<_>>();
    pulls.sort_by_cached_key(|pull| {
        (
            pull.path
                .iter()
                .map(|part| part.to_lowercase())
                .collect::<Vec<_>>(),
            pull.path.clone(),
            pull.repository_key.to_lowercase(),
            Reverse(pull.pull_request.number),
        )
    });

    let mut rows = Vec::new();
    let mut previous_path: Vec<String> = Vec::new();
    for pull in pulls {
        let common = previous_path
            .iter()
            .zip(&pull.path)
            .take_while(|(left, right)| left == right)
            .count();
        for (depth, label) in pull.path.iter().enumerate().skip(common) {
            rows.push(SidebarRow::Group {
                depth,
                label: label.clone(),
            });
        }
        previous_path = pull.path;
        rows.push(SidebarRow::Pull {
            repository_index: pull.repository_index,
            repository_key: pull.repository_key,
            pull_request: Box::new(pull.pull_request.clone()),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::domain::Account;

    fn repo(name: &str, login: &str) -> Repository {
        Repository {
            host: "github.com".into(),
            owner: "acme".into(),
            name: name.into(),
            account: Account {
                host: "github.com".into(),
                login: login.into(),
            },
            local_path: None,
        }
    }

    fn pr(number: u64, source: &str, target: &str) -> PullRequest {
        PullRequest {
            number,
            title: format!("Pull {number}"),
            source_branch: source.into(),
            target_branch: target.into(),
            author: "ada".into(),
            reviewers: vec!["reviewer".into(), "ada".into()],
            assignees: vec!["owner".into()],
            labels: vec!["bug".into()],
            participants: vec!["participant".into()],
            participants_complete: true,
            draft: false,
            state: "OPEN".into(),
            review_status: "APPROVED".into(),
            check_status: "PASSING".into(),
            ..Default::default()
        }
    }

    #[test]
    fn sidebar_cache_reuses_inventory_and_invalidates_same_length_refresh_and_account() {
        let mut inventories = vec![(
            repo("one", "maya"),
            Arc::new(vec![PullRequest {
                number: 7,
                title: "Original".into(),
                state: "OPEN".into(),
                ..Default::default()
            }]),
        )];
        let mut cache = SidebarCache::default();
        let mut view = SavedView::default();
        let original = cache.rows_for(&inventories, &view);
        assert!(Rc::ptr_eq(&original, &cache.rows_for(&inventories, &view)));
        Arc::make_mut(&mut inventories[0].1)[0].title = "Replacement".into();
        let updated = cache.rows_for(&inventories, &view);
        assert!(!Rc::ptr_eq(&original, &updated));
        assert!(
            matches!(&updated[1], SidebarRow::Pull { pull_request, .. } if pull_request.title == "Replacement")
        );
        inventories[0].0.account.login = "other".into();
        let account_changed = cache.rows_for(&inventories, &view);
        assert!(!Rc::ptr_eq(&updated, &account_changed));
        view.filter.search = "No match".into();
        assert!(cache.rows_for(&inventories, &view).is_empty());
    }

    #[test]
    fn case_distinct_git_branches_keep_separate_groups_and_exact_filters() {
        let repository = repo("one", "ada");
        let pulls = [
            pr(1, "Feature/parser", "Main"),
            pr(2, "feature/parser", "main"),
        ];
        let repositories = [RepositoryPulls {
            index: 0,
            repository: &repository,
            pull_requests: &pulls,
        }];
        let view = SavedView {
            groups: vec![GroupBy::TargetBranch],
            ..Default::default()
        };
        let rows = compose_sidebar_rows(&repositories, &view);
        let groups = rows
            .iter()
            .filter_map(|row| match row {
                SidebarRow::Group { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(groups, ["Main", "main"]);
        let exact = SavedView {
            filter: Filter {
                source_branch: "Feature/parser".into(),
                target_branch: "Main".into(),
                ..Default::default()
            },
            ..view
        };
        let numbers = compose_sidebar_rows(&repositories, &exact)
            .into_iter()
            .filter_map(|row| match row {
                SidebarRow::Pull { pull_request, .. } => Some(pull_request.number),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(numbers, [1]);
    }

    #[test]
    fn cancel_is_isolated_and_apply_commits_the_draft() {
        let mut workspace = WorkspaceState::default();
        let original = workspace.view();
        let mut controller = ViewEditorController::new();
        controller.begin(&workspace);
        controller.set_pr_state("closed");
        controller.add_group();
        controller.cancel();
        assert_eq!(workspace.view().filter.state, original.filter.state);
        assert_eq!(workspace.view().groups.len(), original.groups.len());

        controller.begin(&workspace);
        controller.set_pr_state("all");
        controller.add_group();
        controller.apply(&mut workspace, "Across targets").unwrap();
        assert_eq!(workspace.view().name, "Across targets");
        assert_eq!(workspace.view().filter.state, "all");
        assert_eq!(workspace.view().groups.len(), 2);
    }

    #[test]
    fn filter_matrix_is_composed_at_the_controller_boundary() {
        let repository = repo("one", "ada");
        let pull = pr(42, "feature/parser", "main");
        let cases = [
            (
                "search",
                Filter {
                    search: "#42".into(),
                    ..Default::default()
                },
            ),
            (
                "author",
                Filter {
                    author: "ADA".into(),
                    ..Default::default()
                },
            ),
            (
                "reviewer",
                Filter {
                    reviewer: "reviewer".into(),
                    ..Default::default()
                },
            ),
            (
                "assignee",
                Filter {
                    assignee: "owner".into(),
                    ..Default::default()
                },
            ),
            (
                "label",
                Filter {
                    label: "bug".into(),
                    ..Default::default()
                },
            ),
            (
                "draft",
                Filter {
                    draft: Some(false),
                    ..Default::default()
                },
            ),
            (
                "review",
                Filter {
                    review_status: "approved".into(),
                    ..Default::default()
                },
            ),
            (
                "checks",
                Filter {
                    check_status: "passing".into(),
                    ..Default::default()
                },
            ),
            (
                "target",
                Filter {
                    target_branch: "main".into(),
                    ..Default::default()
                },
            ),
            (
                "source",
                Filter {
                    source_branch: "feature/parser".into(),
                    ..Default::default()
                },
            ),
            (
                "state",
                Filter {
                    state: "open".into(),
                    ..Default::default()
                },
            ),
            (
                "own",
                Filter {
                    personal: PersonalFilter::Own,
                    ..Default::default()
                },
            ),
            (
                "requested",
                Filter {
                    personal: PersonalFilter::ReviewRequested,
                    ..Default::default()
                },
            ),
            (
                "participating",
                Filter {
                    personal: PersonalFilter::Participating,
                    ..Default::default()
                },
            ),
        ];
        for (name, filter) in cases {
            let view = SavedView {
                name: name.into(),
                filter,
                groups: Vec::new(),
            };
            assert_eq!(
                compose_sidebar_rows(
                    &[RepositoryPulls {
                        index: 0,
                        repository: &repository,
                        pull_requests: std::slice::from_ref(&pull),
                    }],
                    &view
                )
                .len(),
                1,
                "{name} should match"
            );
        }
        let rejected = SavedView {
            name: "No match".into(),
            filter: Filter {
                label: "enhancement".into(),
                ..Default::default()
            },
            groups: Vec::new(),
        };
        assert!(
            compose_sidebar_rows(
                &[RepositoryPulls {
                    index: 0,
                    repository: &repository,
                    pull_requests: std::slice::from_ref(&pull),
                }],
                &rejected
            )
            .is_empty()
        );
    }

    #[test]
    fn target_then_repository_groups_globally_and_prefixes_sources() {
        let first_repo = repo("one", "one-user");
        let second_repo = repo("two", "two-user");
        let first_prs = vec![pr(1, "team/alpha", "main"), pr(2, "misc", "release")];
        let second_prs = vec![pr(1, "team/beta", "main")];
        let view = SavedView {
            name: "Global".into(),
            filter: Filter::default(),
            groups: vec![
                GroupBy::TargetBranch,
                GroupBy::Repository,
                GroupBy::SourcePrefix("team/".into()),
            ],
        };
        let rows = compose_sidebar_rows(
            &[
                RepositoryPulls {
                    index: 0,
                    repository: &first_repo,
                    pull_requests: &first_prs,
                },
                RepositoryPulls {
                    index: 1,
                    repository: &second_repo,
                    pull_requests: &second_prs,
                },
            ],
            &view,
        );
        let headings = rows
            .iter()
            .filter_map(|row| match row {
                SidebarRow::Group { depth, label } => Some((*depth, label.as_str())),
                SidebarRow::Pull { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(headings[0], (0, "main"));
        assert!(
            headings
                .iter()
                .filter(|(depth, label)| *depth == 0 && *label == "main")
                .count()
                == 1
        );
        assert!(headings.contains(&(2, "team/")));
        assert!(headings.contains(&(2, "Other branches")));
    }

    #[test]
    fn named_views_keep_a_valid_selection_and_restart() {
        let directory = tempfile::tempdir().unwrap();
        let store = cibergit::workspace::Store::open(directory.path()).unwrap();
        let mut workspace = WorkspaceState::default();
        let mut controller = ViewEditorController::new();
        controller.begin(&workspace);
        controller.set_pr_state("all");
        controller.save_as(&mut workspace, "Everything").unwrap();
        store.save_workspace(&workspace).unwrap();
        let restored = store.load_workspace().unwrap();
        assert_eq!(restored.selected_view, 1);
        assert_eq!(restored.view().name, "Everything");
        assert_eq!(restored.view().filter.state, "all");

        controller.delete_selected(&mut workspace);
        assert_eq!(workspace.selected_view, 0);
        controller.delete_selected(&mut workspace);
        assert_eq!(workspace.views.len(), 1);
        assert_eq!(workspace.selected_view, 0);
        assert!(!workspace.view().name.is_empty());
    }
}
