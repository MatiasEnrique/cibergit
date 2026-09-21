//! Owns navigation/session write ordering and admission of a delayed startup restore.
use cibergit::{
    domain::Repository,
    workspace::{
        PersistedComparisonContext, RestoredWorkspaceTab, Store, WorkspaceRestoreNotice,
        WorkspaceRestorePlan, WorkspaceState,
    },
};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

type TabIdentity = (String, u64);

#[derive(Clone, Default)]
struct SaveLane {
    latest: Arc<AtomicU64>,
    state: Arc<Mutex<SaveState>>,
}

#[derive(Default)]
struct SaveState {
    completed: u64,
    failed: Option<String>,
}

impl SaveState {
    fn ensure_writable(&self) -> anyhow::Result<()> {
        if let Some(error) = &self.failed {
            anyhow::bail!("Saving remains stopped after an earlier failure: {error}");
        }
        Ok(())
    }

    fn write(
        &mut self,
        sequence: u64,
        write: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.ensure_writable()?;
        if sequence <= self.completed {
            return Ok(());
        }
        let result = write();
        // Retain the error before releasing the lane, even if its tab closed.
        self.failed = result.as_ref().err().map(|error| format!("{error:#}"));
        if result.is_ok() {
            self.completed = sequence;
        }
        result
    }
}

impl SaveLane {
    fn enqueue(&self) -> u64 {
        self.latest.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn write(
        &self,
        sequence: u64,
        write: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("workspace save lock failed"))?;
        state.ensure_writable()?;
        if self.latest.load(Ordering::Acquire) != sequence {
            return Ok(());
        }
        state.write(sequence, write)
    }
}

#[derive(Default)]
struct SessionLane {
    writes: SaveLane,
    // Keep queued data available to an immediate reopen without retaining all
    // visited comparisons once the background save and any readers finish.
    pending: Option<(u64, Weak<PersistedComparisonContext>)>,
}

/// Drains the preceding session write before reading under the same lock.
/// Merely locking a read is insufficient: it can overtake a queued write that
/// has not started. The captured snapshot lets the read perform that write.
pub(super) struct SessionRead {
    store: Store,
    repository: Repository,
    number: u64,
    lane: SaveLane,
    pending: Option<(u64, Arc<PersistedComparisonContext>)>,
}

impl SessionRead {
    pub fn execute(self) -> anyhow::Result<PersistedComparisonContext> {
        let mut state = self
            .lane
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("review session read lock failed"))?;
        state.ensure_writable()?;
        if let Some((sequence, context)) = self.pending {
            state.write(sequence, || {
                self.store
                    .save_review_context(&self.repository, self.number, &context)
            })?;
        }
        self.store
            .load_review_context(&self.repository, self.number)
    }
}

enum Snapshot {
    Workspace(WorkspaceState),
    Session {
        repository: Repository,
        number: u64,
        context: Arc<PersistedComparisonContext>,
    },
}

/// An owned, single-use write suitable for a background executor.
pub(super) struct WorkspaceWrite {
    store: Store,
    lane: SaveLane,
    sequence: u64,
    snapshot: Snapshot,
}

impl WorkspaceWrite {
    pub fn execute(self) -> anyhow::Result<()> {
        self.lane.write(self.sequence, || match self.snapshot {
            Snapshot::Workspace(state) => self.store.save_workspace(&state),
            Snapshot::Session {
                repository,
                number,
                context,
            } => self
                .store
                .save_review_context(&repository, number, &context),
        })
    }
}

/// Immutable disk work paired with an opaque owner identity. It grants no
/// provider/write authority; completion still validates live repositories.
pub(super) struct WorkspaceRestore {
    owner: Arc<()>,
    store: Store,
    snapshot: WorkspaceState,
}

pub(super) struct LoadedWorkspace {
    owner: Arc<()>,
    plan: WorkspaceRestorePlan,
}

impl WorkspaceRestore {
    pub fn execute(self) -> LoadedWorkspace {
        LoadedWorkspace {
            owner: self.owner,
            plan: self.store.load_workspace_restore(&self.snapshot),
        }
    }
}

enum RestoreState {
    Ready,
    Pending,
    Loading(Arc<()>),
}

/// The GUI only schedules returned work and installs admitted tabs. This owner
/// retains ordering, failure stops, and restore cancellation for its lifetime.
pub(super) struct WorkspacePersistence {
    store: Option<Store>,
    navigation: SaveLane,
    sessions: HashMap<TabIdentity, SessionLane>,
    restore: RestoreState,
}

pub(super) struct AdmittedWorkspace {
    pub tabs: Vec<RestoredWorkspaceTab>,
    pub notices: Vec<WorkspaceRestoreNotice>,
    saved_active: Option<TabIdentity>,
    order: Vec<TabIdentity>,
}

impl AdmittedWorkspace {
    pub fn order(&self, current: &[TabIdentity]) -> Vec<usize> {
        let mut order = Vec::with_capacity(current.len());
        for identity in &self.order {
            if let Some(index) = current
                .iter()
                .enumerate()
                .find(|(index, candidate)| !order.contains(index) && *candidate == identity)
                .map(|(index, _)| index)
            {
                order.push(index);
            }
        }
        for index in 0..current.len() {
            if !order.contains(&index) {
                order.push(index);
            }
        }
        order
    }

    pub fn active(
        &self,
        available: &BTreeSet<TabIdentity>,
        explicit: Option<TabIdentity>,
        prior: Option<TabIdentity>,
    ) -> Option<TabIdentity> {
        explicit
            .filter(|id| available.contains(id))
            .or_else(|| {
                self.saved_active
                    .clone()
                    .filter(|id| available.contains(id))
            })
            .or_else(|| prior.filter(|id| available.contains(id)))
            .or_else(|| {
                self.order
                    .iter()
                    .find(|id| available.contains(*id))
                    .cloned()
            })
            .or_else(|| available.first().cloned())
    }
}

impl WorkspacePersistence {
    pub fn new(store: Option<Store>, workspace: &WorkspaceState) -> Self {
        let pending = store.is_some() && !workspace.tabs.is_empty();
        Self {
            store,
            navigation: SaveLane::default(),
            sessions: HashMap::new(),
            restore: if pending {
                RestoreState::Pending
            } else {
                RestoreState::Ready
            },
        }
    }

    pub fn restore_pending(&self) -> bool {
        !matches!(self.restore, RestoreState::Ready)
    }

    pub fn begin_restore(&mut self, workspace: &WorkspaceState) -> Option<WorkspaceRestore> {
        if !matches!(self.restore, RestoreState::Pending) {
            return None;
        }
        let store = self.store.clone()?;
        let owner = Arc::new(());
        self.restore = RestoreState::Loading(owner.clone());
        Some(WorkspaceRestore {
            owner,
            store,
            snapshot: workspace.clone(),
        })
    }

    pub fn cancel_restore(&mut self) {
        self.restore = RestoreState::Ready;
    }

    pub fn complete_restore(
        &mut self,
        loaded: LoadedWorkspace,
        repositories: &[Repository],
        current: &[TabIdentity],
    ) -> Option<AdmittedWorkspace> {
        let RestoreState::Loading(owner) = &self.restore else {
            return None;
        };
        if !Arc::ptr_eq(owner, &loaded.owner) {
            return None;
        }
        self.restore = RestoreState::Ready;
        let plan = loaded.plan;
        let saved_active = plan
            .active_tab
            .and_then(|i| plan.tabs.get(i))
            .map(|tab| (tab.repository.cache_key(), tab.pull_request.number));
        let mut admitted = AdmittedWorkspace {
            tabs: Vec::new(),
            notices: plan.notices,
            saved_active,
            order: Vec::new(),
        };
        for tab in plan.tabs {
            if !repositories.iter().any(|repo| repo == &tab.repository) {
                admitted.notices.push(WorkspaceRestoreNotice { saved_index: Some(tab.saved_index), message: format!("Saved tab #{} was not restored because its exact repository/account was removed while startup data loaded", tab.pull_request.number) });
                continue;
            }
            let identity = (tab.repository.cache_key(), tab.pull_request.number);
            if admitted.order.contains(&identity) {
                continue;
            }
            admitted.order.push(identity.clone());
            if !current.contains(&identity) {
                admitted.tabs.push(tab);
            }
        }
        Some(admitted)
    }

    pub fn save_workspace(&self, state: &WorkspaceState) -> Option<WorkspaceWrite> {
        if self.restore_pending() {
            return None;
        }
        Some(WorkspaceWrite {
            store: self.store.clone()?,
            lane: self.navigation.clone(),
            sequence: self.navigation.enqueue(),
            snapshot: Snapshot::Workspace(state.clone()),
        })
    }

    pub fn save_session(
        &mut self,
        repository: Repository,
        number: u64,
        context: PersistedComparisonContext,
    ) -> Option<WorkspaceWrite> {
        let store = self.store.clone()?;
        let lane = self
            .sessions
            .entry((repository.cache_key(), number))
            .or_default();
        let context = Arc::new(context);
        let sequence = lane.writes.enqueue();
        lane.pending = Some((sequence, Arc::downgrade(&context)));
        Some(WorkspaceWrite {
            store,
            lane: lane.writes.clone(),
            sequence,
            snapshot: Snapshot::Session {
                repository,
                number,
                context,
            },
        })
    }

    pub fn load_session(&mut self, repository: Repository, number: u64) -> Option<SessionRead> {
        let store = self.store.clone()?;
        let lane = self
            .sessions
            .entry((repository.cache_key(), number))
            .or_default();
        let pending = lane
            .pending
            .as_ref()
            .and_then(|(sequence, context)| context.upgrade().map(|context| (*sequence, context)));
        Some(SessionRead {
            store,
            repository,
            number,
            lane: lane.writes.clone(),
            pending,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::{
        domain::{Account, Comparison, PullRequest, Revision},
        review::ReviewSession,
        workspace::TabState,
    };

    fn repository(login: &str) -> Repository {
        Repository {
            host: "github.com".into(),
            owner: "acme".into(),
            name: "app".into(),
            account: Account {
                host: "github.com".into(),
                login: login.into(),
            },
            local_path: None,
        }
    }

    fn context(head: &str) -> PersistedComparisonContext {
        PersistedComparisonContext::full(ReviewSession::new(Comparison {
            revision: Revision {
                base_sha: "base".into(),
                head_sha: head.into(),
            },
            files: Vec::new(),
            complete: true,
            notice: None,
        }))
    }

    fn saved_workspace(store: &Store) -> WorkspaceState {
        let repository = repository("one");
        let mut workspace = WorkspaceState {
            repositories: vec![repository.clone()],
            ..Default::default()
        };
        for number in [1, 2] {
            let context = context(&format!("head-{number}"));
            store
                .save_review_context(&repository, number, &context)
                .unwrap();
            workspace.tabs.push(TabState {
                repository_key: repository.cache_key(),
                number,
                revision: context.canonical_full_revision,
                pull_request: Some(PullRequest {
                    number,
                    ..Default::default()
                }),
                selected_file: None,
                scroll_offset: 0.0,
                diff_mode: "auto".into(),
            });
        }
        workspace.active_tab = Some(0);
        workspace
    }

    #[test]
    fn queued_navigation_writes_coalesce_before_touching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut workspace = WorkspaceState::default();
        let persistence = WorkspacePersistence::new(Some(store.clone()), &workspace);
        let old = persistence.save_workspace(&workspace).unwrap();
        workspace.views[0].name = "Latest".into();
        persistence
            .save_workspace(&workspace)
            .unwrap()
            .execute()
            .unwrap();
        old.execute().unwrap();
        assert_eq!(store.load_workspace().unwrap().views[0].name, "Latest");
    }

    #[test]
    fn failed_navigation_stops_writes_before_ui_receives_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let workspace = WorkspaceState::default();
        let persistence = WorkspacePersistence::new(Some(store.clone()), &workspace);
        let path = dir.path().join("workspace.json");
        std::fs::write(&path, "unreadable saved data").unwrap();
        assert!(
            persistence
                .save_workspace(&workspace)
                .unwrap()
                .execute()
                .is_err()
        );
        std::fs::remove_file(&path).unwrap();
        let mut repaired = workspace.clone();
        repaired.views[0].name = "Recovered externally".into();
        store.save_workspace(&repaired).unwrap();
        assert!(
            persistence
                .save_workspace(&workspace)
                .unwrap()
                .execute()
                .is_err()
        );
        assert_eq!(
            store.load_workspace().unwrap().views[0].name,
            "Recovered externally"
        );
    }

    #[test]
    fn session_ordering_is_partitioned_by_account_and_pull_request() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut persistence =
            WorkspacePersistence::new(Some(store.clone()), &WorkspaceState::default());
        let old = persistence
            .save_session(repository("one"), 1, context("old"))
            .unwrap();
        let latest = persistence
            .save_session(repository("one"), 1, context("new"))
            .unwrap();
        let other_account = persistence
            .save_session(repository("two"), 1, context("account-two"))
            .unwrap();
        let other_pr = persistence
            .save_session(repository("one"), 2, context("pr-two"))
            .unwrap();
        for write in [latest, other_account, old, other_pr] {
            write.execute().unwrap();
        }
        for (login, number, head) in [
            ("one", 1, "new"),
            ("two", 1, "account-two"),
            ("one", 2, "pr-two"),
        ] {
            assert_eq!(
                store
                    .load_review_context(&repository(login), number)
                    .unwrap()
                    .canonical_full_revision
                    .head_sha,
                head
            );
        }
    }

    #[test]
    fn failed_session_stops_its_lane_but_other_sessions_still_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut persistence =
            WorkspacePersistence::new(Some(store.clone()), &WorkspaceState::default());
        let mut invalid = context("invalid");
        invalid.canonical_full_revision.head_sha = "does-not-match-session".into();
        assert!(
            persistence
                .save_session(repository("one"), 1, invalid)
                .unwrap()
                .execute()
                .is_err()
        );
        assert!(
            persistence
                .save_session(repository("one"), 1, context("later"))
                .unwrap()
                .execute()
                .is_err()
        );
        assert!(store.load_review_context(&repository("one"), 1).is_err());
        persistence
            .save_session(repository("one"), 2, context("independent"))
            .unwrap()
            .execute()
            .unwrap();
        assert_eq!(
            store
                .load_review_context(&repository("one"), 2)
                .unwrap()
                .canonical_full_revision
                .head_sha,
            "independent"
        );
    }

    #[test]
    fn navigation_cancels_delayed_restore_and_releases_navigation_saves() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let workspace = saved_workspace(&store);
        let mut persistence = WorkspacePersistence::new(Some(store), &workspace);
        let restore = persistence.begin_restore(&workspace).unwrap();
        assert!(persistence.begin_restore(&workspace).is_none());
        assert!(persistence.save_workspace(&workspace).is_none());
        persistence.cancel_restore();
        assert!(
            persistence
                .complete_restore(restore.execute(), &workspace.repositories, &[])
                .is_none()
        );
        assert!(persistence.save_workspace(&workspace).is_some());
    }

    #[test]
    fn foreign_restore_does_not_consume_current_workspace_restore() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let workspace = saved_workspace(&store);
        let mut old = WorkspacePersistence::new(Some(store.clone()), &workspace);
        let old_restore = old.begin_restore(&workspace).unwrap();
        let mut current = WorkspacePersistence::new(Some(store), &workspace);
        let own_restore = current.begin_restore(&workspace).unwrap();
        assert!(
            current
                .complete_restore(old_restore.execute(), &workspace.repositories, &[])
                .is_none()
        );
        let admitted = current
            .complete_restore(own_restore.execute(), &workspace.repositories, &[])
            .unwrap();
        assert_eq!(admitted.tabs.len(), 2);
    }

    #[test]
    fn restore_revalidates_repositories_and_does_not_reinstall_existing_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let workspace = saved_workspace(&store);
        let mut persistence = WorkspacePersistence::new(Some(store.clone()), &workspace);
        let loaded = persistence.begin_restore(&workspace).unwrap().execute();
        let admitted = persistence
            .complete_restore(loaded, &[repository("two")], &[])
            .unwrap();
        assert!(admitted.tabs.is_empty());
        assert_eq!(admitted.notices.len(), 2);
        let mut persistence = WorkspacePersistence::new(Some(store), &workspace);
        let loaded = persistence.begin_restore(&workspace).unwrap().execute();
        let id = |number| (repository("one").cache_key(), number);
        let current = vec![id(2), id(99)];
        let admitted = persistence
            .complete_restore(loaded, &workspace.repositories, &current)
            .unwrap();
        assert_eq!(
            admitted
                .tabs
                .iter()
                .map(|tab| tab.pull_request.number)
                .collect::<Vec<_>>(),
            vec![1]
        );
        let installed = vec![id(2), id(99), id(1)];
        assert_eq!(admitted.order(&installed), vec![2, 0, 1]);
        let available = installed.into_iter().collect();
        assert_eq!(
            admitted.active(&available, Some(id(99)), None),
            Some(id(99))
        );
        assert_eq!(admitted.active(&available, None, None), Some(id(1)));
    }

    #[test]
    fn unavailable_saved_slots_survive_restore_and_navigation_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut workspace = saved_workspace(&store);
        workspace.tabs[0].revision.head_sha = "unavailable-saved-pin".into();
        let mut persistence = WorkspacePersistence::new(Some(store.clone()), &workspace);
        let loaded = persistence.begin_restore(&workspace).unwrap().execute();
        let admitted = persistence
            .complete_restore(loaded, &workspace.repositories, &[])
            .unwrap();
        assert_eq!(admitted.tabs.len(), 1);
        let live = vec![workspace.tabs[1].clone()];
        workspace.merge_tabs(
            live,
            &BTreeSet::new(),
            Some((repository("one").cache_key(), 2)),
        );
        persistence
            .save_workspace(&workspace)
            .unwrap()
            .execute()
            .unwrap();
        let saved = store.load_workspace().unwrap();
        assert_eq!(saved.tabs.len(), 2);
        assert_eq!(saved.tabs[0].revision.head_sha, "unavailable-saved-pin");
    }
    #[test]
    fn immediate_reopen_drains_final_save_even_before_its_worker_starts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .save_review_context(&repository("one"), 1, &context("old"))
            .unwrap();
        let mut persistence =
            WorkspacePersistence::new(Some(store.clone()), &WorkspaceState::default());
        let delayed_save = persistence
            .save_session(repository("one"), 1, context("latest"))
            .unwrap();
        let reopened = persistence
            .load_session(repository("one"), 1)
            .unwrap()
            .execute()
            .unwrap();
        assert_eq!(reopened.canonical_full_revision.head_sha, "latest");
        delayed_save.execute().unwrap();
        assert_eq!(
            store
                .load_review_context(&repository("one"), 1)
                .unwrap()
                .canonical_full_revision
                .head_sha,
            "latest"
        );
    }

    #[test]
    fn delayed_read_cannot_overwrite_a_newer_completed_session_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut persistence = WorkspacePersistence::new(Some(store), &WorkspaceState::default());
        let old = persistence
            .save_session(repository("one"), 1, context("old"))
            .unwrap();
        let delayed_read = persistence.load_session(repository("one"), 1).unwrap();
        persistence
            .save_session(repository("one"), 1, context("new"))
            .unwrap()
            .execute()
            .unwrap();
        assert_eq!(
            delayed_read
                .execute()
                .unwrap()
                .canonical_full_revision
                .head_sha,
            "new"
        );
        old.execute().unwrap();
        assert_eq!(
            persistence
                .load_session(repository("one"), 1)
                .unwrap()
                .execute()
                .unwrap()
                .canonical_full_revision
                .head_sha,
            "new"
        );
    }

    #[test]
    fn reopening_reports_failed_final_save_instead_of_installing_older_progress() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .save_review_context(&repository("one"), 1, &context("old"))
            .unwrap();
        let mut persistence =
            WorkspacePersistence::new(Some(store.clone()), &WorkspaceState::default());
        let mut invalid = context("new");
        invalid.canonical_full_revision.head_sha = "invalid".into();
        let delayed_save = persistence
            .save_session(repository("one"), 1, invalid)
            .unwrap();
        assert!(
            persistence
                .load_session(repository("one"), 1)
                .unwrap()
                .execute()
                .is_err()
        );
        assert!(delayed_save.execute().is_err());
        assert!(
            persistence
                .load_session(repository("one"), 1)
                .unwrap()
                .execute()
                .is_err()
        );
        assert_eq!(
            store
                .load_review_context(&repository("one"), 1)
                .unwrap()
                .canonical_full_revision
                .head_sha,
            "old"
        );
    }
}
