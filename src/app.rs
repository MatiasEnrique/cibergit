use crate::{
    CloseTab, CycleDiffMode, NextFile, OpenRepositorySetup, PreviousFile, Refresh, Save,
    ToggleInspector, TogglePalette,
};
use cibergit::{
    domain::{PullRequest, PullRequestDetails, Repository, Revision},
    providers::GithubProvider,
    review::{
        AlignedRow, DiffLine, DiffLineKind, DiffMode, ParsedDiff, PatchStatus, ReviewSession,
        file_key, load_local_file, local_pr_inventory, parse_file,
    },
    workspace::{
        GroupBy, PersonalFilter, PollSchedule, Store, TabState, WorkspaceState, group_path,
    },
};
use gpui::{prelude::*, *};
use gpui_base::input::{Editor, EditorState, Input, InputEditorStyle, InputState};
use std::{
    cmp::Reverse,
    ops::Range,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

const UI_FONT: &str = "IBM Plex Sans";
const CODE_FONT: &str = "Menlo";

#[derive(Clone, Debug, Default)]
pub enum LaunchMode {
    #[default]
    Review,
    Edit(PathBuf),
}

#[derive(Clone, Debug, Default)]
pub struct Startup {
    pub mode: LaunchMode,
    pub repository: Option<String>,
    pub account: Option<String>,
    pub pull_request: Option<u64>,
    pub data_dir: Option<PathBuf>,
}

pub enum Root {
    Review(ReviewWorkspace),
    Editor(EditorWorkspace),
}

impl Root {
    pub fn review(window: &mut Window, cx: &mut Context<Self>, startup: Startup) -> Self {
        Self::Review(ReviewWorkspace::new(window, cx, startup))
    }

    pub fn editor(window: &mut Window, cx: &mut Context<Self>, path: PathBuf) -> Self {
        Self::Editor(EditorWorkspace::new(window, cx, path))
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self {
            Self::Review(workspace) => workspace.render(window, cx).into_any_element(),
            Self::Editor(editor) => editor.render(window, cx).into_any_element(),
        }
    }
}

#[derive(Clone, Copy)]
struct Palette {
    canvas: Rgba,
    surface: Rgba,
    sidebar: Rgba,
    elevated: Rgba,
    text: Rgba,
    muted: Rgba,
    faint: Rgba,
    border: Rgba,
    selected: Rgba,
    accent: Rgba,
    green: Rgba,
    red: Rgba,
    amber: Rgba,
    dark: bool,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            canvas: rgba(0x18191bff),
            surface: rgba(0x202124ff),
            sidebar: rgba(0x17181abe),
            elevated: rgba(0x292b2fff),
            text: rgba(0xf1f2f3ff),
            muted: rgba(0xa8abb1ff),
            faint: rgba(0x777b83ff),
            border: rgba(0x36383dff),
            selected: rgba(0xffffff13),
            accent: rgba(0x8ab4f8ff),
            green: rgba(0x70c995ff),
            red: rgba(0xf28b82ff),
            amber: rgba(0xf7c873ff),
            dark,
        }
    } else {
        Palette {
            canvas: rgba(0xfafaf9ff),
            surface: rgba(0xffffffff),
            sidebar: rgba(0xf0f0eecc),
            elevated: rgba(0xf2f2f0ff),
            text: rgba(0x202124ff),
            muted: rgba(0x64676cff),
            faint: rgba(0x8b8e93ff),
            border: rgba(0xdedfdcff),
            selected: rgba(0x0000000a),
            accent: rgba(0x245eaaff),
            green: rgba(0x18794eff),
            red: rgba(0xc2352aff),
            amber: rgba(0x986a12ff),
            dark,
        }
    }
}

fn is_dark(window: &Window) -> bool {
    matches!(
        window.appearance(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

fn input_style(colors: Palette) -> InputEditorStyle {
    InputEditorStyle {
        foreground: colors.text.into(),
        muted_foreground: colors.muted.into(),
        background: colors.elevated.into(),
        editor_gutter_background: Some(colors.elevated.into()),
        ..Default::default()
    }
}

fn new_input(
    value: impl Into<String>,
    placeholder: &'static str,
    window: &mut Window,
    cx: &mut Context<Root>,
) -> Entity<InputState> {
    let value = value.into();
    let colors = palette(is_dark(window));
    cx.new(|cx| {
        let mut editor = InputState::new(window, cx);
        editor.set_editor_style(input_style(colors));
        editor.set_value(value, window, cx);
        editor.set_placeholder(placeholder, window, cx);
        editor
    })
}

#[derive(Clone)]
enum LoadState {
    Loading(String),
    Ready,
    Cached(String),
    Error(String),
}

impl LoadState {
    fn notice(&self) -> Option<String> {
        match self {
            Self::Loading(value) | Self::Cached(value) | Self::Error(value) => Some(value.clone()),
            Self::Ready => None,
        }
    }
}

struct RepoRuntime {
    repository: Repository,
    pull_requests: Vec<PullRequest>,
    state: LoadState,
    generation: u64,
}

struct ReviewTab {
    repository: Repository,
    pull_request: PullRequest,
    session: Option<ReviewSession>,
    state: LoadState,
    generation: u64,
    metadata_generation: u64,
    diff_rows: Vec<DiffRow>,
    diff_scroll: UniformListScrollHandle,
    inspector_section: InspectorSection,
    local_inventory: bool,
    session_persistence_error: Option<String>,
    details: Option<PullRequestDetails>,
    details_state: LoadState,
    details_generation: u64,
}

#[derive(Clone)]
enum DiffRow {
    Hunk(String),
    Unified(DiffLine),
    Split(AlignedRow),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InspectorSection {
    Overview,
    Activity,
    Checks,
}

pub struct ReviewWorkspace {
    store: Option<Store>,
    workspace: WorkspaceState,
    persistence_error: Option<String>,
    accounts: Vec<cibergit::domain::Account>,
    accounts_state: LoadState,
    repositories: Vec<RepoRuntime>,
    tabs: Vec<ReviewTab>,
    active_tab: Option<usize>,
    setup_open: bool,
    command_palette: bool,
    inspector_open: bool,
    focused: bool,
    wide: bool,
    focus: FocusHandle,
    query: Entity<InputState>,
    repository_input: Entity<InputState>,
    pr_input: Entity<InputState>,
    selected_account: usize,
    status: String,
    schedule: PollSchedule,
    startup_pr: Option<u64>,
    session_save_latest: Arc<AtomicU64>,
    session_save_lock: Arc<Mutex<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ReviewWorkspace {
    fn new(window: &mut Window, cx: &mut Context<Root>, startup: Startup) -> Self {
        let store_result = startup
            .data_dir
            .map(Store::open)
            .unwrap_or_else(Store::open_default);
        let (store, workspace, persistence_error) = match store_result {
            Ok(store) => match store.load_workspace() {
                Ok(workspace) => (Some(store), workspace, None),
                Err(error) => (
                    Some(store),
                    WorkspaceState::default(),
                    Some(format!(
                        "Workspace data is unreadable and will not be overwritten: {error:#}"
                    )),
                ),
            },
            Err(error) => (
                None,
                WorkspaceState::default(),
                Some(format!("Cannot open workspace data: {error:#}")),
            ),
        };
        let query = new_input(
            workspace.view().filter.search,
            "Search pull requests",
            window,
            cx,
        );
        let repository_input = new_input(
            startup.repository.clone().unwrap_or_default(),
            "owner/name, URL, or local folder",
            window,
            cx,
        );
        let pr_input = new_input(
            startup
                .pull_request
                .map(|number| number.to_string())
                .unwrap_or_default(),
            "Pull request number",
            window,
            cx,
        );
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let repositories = workspace
            .repositories
            .iter()
            .cloned()
            .map(|repository| {
                let cached = store
                    .as_ref()
                    .and_then(|store| store.load_pull_requests(&repository).ok());
                RepoRuntime {
                    repository,
                    pull_requests: cached.clone().unwrap_or_default(),
                    state: if cached.is_some() {
                        LoadState::Cached("Cached PR list · refreshing…".into())
                    } else {
                        LoadState::Loading("Loading pull requests…".into())
                    },
                    generation: 0,
                }
            })
            .collect();
        let mut this = Self {
            store,
            workspace,
            persistence_error,
            accounts: Vec::new(),
            accounts_state: LoadState::Loading("Discovering gh accounts…".into()),
            repositories,
            tabs: Vec::new(),
            active_tab: None,
            setup_open: startup.repository.is_none(),
            command_palette: false,
            inspector_open: true,
            focused: window.is_window_active(),
            wide: window.bounds().size.width > px(1180.),
            focus,
            query,
            repository_input,
            pr_input,
            selected_account: 0,
            status: "Read-only review workspace".into(),
            schedule: PollSchedule::default(),
            startup_pr: startup.pull_request,
            session_save_latest: Arc::new(AtomicU64::new(0)),
            session_save_lock: Arc::new(Mutex::new(())),
            _subscriptions: Vec::new(),
        };
        let activation = cx.observe_window_activation(window, |root, window, cx| {
            let Root::Review(this) = root else { return };
            this.focused = window.is_window_active();
            if this.focused {
                this.refresh_all(cx);
                this.refresh_active(cx);
            }
            cx.notify();
        });
        let appearance = cx.observe_window_appearance(window, |root, window, cx| {
            let Root::Review(this) = root else { return };
            let style = input_style(palette(is_dark(window)));
            for editor in [&this.query, &this.repository_input, &this.pr_input] {
                editor.update(cx, |editor, _| editor.set_editor_style(style.clone()));
            }
            cx.notify();
        });
        let bounds = cx.observe_window_bounds(window, |root, window, cx| {
            let Root::Review(this) = root else { return };
            let wide = window.bounds().size.width > px(1180.);
            if wide != this.wide {
                this.wide = wide;
                if let Some(index) = this.active_tab
                    && this.tabs[index]
                        .session
                        .as_ref()
                        .is_some_and(|session| session.diff_mode() == DiffMode::Auto)
                {
                    this.rebuild_diff(index, wide);
                }
                cx.notify();
            }
        });
        this._subscriptions.extend([activation, appearance, bounds]);
        this.discover_accounts(startup.account, cx);
        for index in 0..this.repositories.len() {
            this.refresh_repository(index, cx);
        }
        this.start_polling(cx);
        this.start_smoke(window, cx);
        this
    }

    fn discover_accounts(&mut self, requested: Option<String>, cx: &mut Context<Root>) {
        let task = cx.background_spawn(async { GithubProvider::accounts() });
        cx.spawn(async move |root, cx| {
            let result = task.await;
            let _ = root.update(cx, |root, cx| {
                let Root::Review(this) = root else { return };
                match result {
                    Ok(mut accounts) => {
                        if let Some(login) = requested.as_ref()
                            && !accounts.iter().any(|account| account.login == *login)
                        {
                            accounts.push(cibergit::domain::Account {
                                host: "github.com".into(),
                                login: login.clone(),
                            });
                        }
                        this.accounts = accounts;
                        this.selected_account = requested
                            .as_ref()
                            .and_then(|login| {
                                this.accounts
                                    .iter()
                                    .position(|account| account.login == *login)
                            })
                            .unwrap_or(0);
                        this.accounts_state = LoadState::Ready;
                        if requested.is_some()
                            && !this.repository_input.read(cx).value().trim().is_empty()
                        {
                            this.add_repository(cx);
                        }
                    }
                    Err(error) => {
                        if let Some(login) = requested {
                            this.accounts = vec![cibergit::domain::Account {
                                host: "github.com".into(),
                                login,
                            }];
                            this.accounts_state = LoadState::Cached(
                                "Explicit account selected; gh discovery unavailable".into(),
                            );
                            this.add_repository(cx);
                        } else {
                            this.accounts_state =
                                LoadState::Error(format!("Cannot discover gh accounts: {error:#}"));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start_polling(&mut self, cx: &mut Context<Root>) {
        cx.spawn(async move |root, cx| {
            let mut tick = 0u64;
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(15))
                    .await;
                if root
                    .update(cx, |root, cx| {
                        let Root::Review(this) = root else { return };
                        tick += 1;
                        if this.focused || tick.is_multiple_of(4) {
                            this.refresh_active(cx);
                        }
                        if (this.focused && tick.is_multiple_of(4)) || tick.is_multiple_of(16) {
                            this.refresh_all(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    #[cfg(feature = "ui-smoke")]
    fn start_smoke(&mut self, window: &mut Window, cx: &mut Context<Root>) {
        let Some(output) = std::env::var_os("CIBERGIT_SMOKE_DIR").map(PathBuf::from) else {
            return;
        };
        let weak = cx.weak_entity();
        window
            .spawn(cx, async move |window| {
                let started = std::time::Instant::now();
                loop {
                    window
                        .background_executor()
                        .timer(Duration::from_millis(250))
                        .await;
                    let ready = window
                        .update(|_, cx| {
                            weak.read_with(cx, |root, _| {
                                matches!(root, Root::Review(this) if this.tabs.iter().any(|tab| tab.session.is_some()))
                            })
                            .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    if ready || started.elapsed() > Duration::from_secs(90) {
                        break;
                    }
                }
                let _ = std::fs::create_dir_all(&output);
                let _ = window.update(|window, cx| {
                    let details = weak
                        .read_with(cx, |root, _| {
                            if let Root::Review(this) = root
                                && let Some(tab) =
                                    this.active_tab.and_then(|index| this.tabs.get(index))
                                && let Some(session) = &tab.session
                            {
                                return format!(
                                    "Real read complete\nRepository: {}\nPR: #{} {}\nBranches: {} -> {}\nRevision: {}\nFiles: {}\nSelected: {}\nRemote writes: none\n",
                                    tab.repository.full_name(),
                                    tab.pull_request.number,
                                    tab.pull_request.title,
                                    tab.pull_request.source_branch,
                                    tab.pull_request.target_branch,
                                    session.revision().head_sha,
                                    session.comparison().files.len(),
                                    session
                                        .selected_file()
                                        .map(|file| file.path.as_str())
                                        .unwrap_or("none")
                                );
                            }
                            "Smoke timed out before a real PR comparison loaded.\n".into()
                        })
                        .unwrap_or_else(|_| "Smoke view unavailable.\n".into());
                    let captured = window
                        .render_to_image()
                        .and_then(|image| {
                            image
                                .save(output.join("native-pr-review.png"))
                                .map_err(Into::into)
                        })
                        .is_ok();
                    let report = format!(
                        "{details}Scene capture: {}\nNative backdrop blending: not established by in-process capture\n",
                        if captured {
                            "native-pr-review.png"
                        } else {
                            "failed"
                        }
                    );
                    let _ = std::fs::write(output.join("native-pr-smoke.txt"), report);
                    cx.quit();
                });
            })
            .detach();
    }

    #[cfg(not(feature = "ui-smoke"))]
    fn start_smoke(&mut self, _: &mut Window, _: &mut Context<Root>) {}

    fn add_repository(&mut self, cx: &mut Context<Root>) {
        let input = self.repository_input.read(cx).value().trim().to_owned();
        let Some(account) = self.accounts.get(self.selected_account).cloned() else {
            self.status = "Choose a discovered GitHub account".into();
            cx.notify();
            return;
        };
        if input.is_empty() {
            self.status = "Enter owner/name, a GitHub URL, or a local folder".into();
            cx.notify();
            return;
        }
        self.status = format!("Resolving {input} as {}…", account.login);
        let task =
            cx.background_spawn(async move { GithubProvider::new(account).repository(&input) });
        cx.spawn(async move |root, cx| {
            let result = task.await;
            let _ = root.update(cx, |root, cx| {
                let Root::Review(this) = root else { return };
                match result {
                    Ok(repository) => {
                        let key = repository.cache_key();
                        let existing = this
                            .repositories
                            .iter()
                            .position(|runtime| runtime.repository.cache_key() == key);
                        let index = if let Some(index) = existing {
                            index
                        } else {
                            this.workspace.add_repository(repository.clone());
                            this.repositories.push(RepoRuntime {
                                repository: repository.clone(),
                                pull_requests: Vec::new(),
                                state: LoadState::Loading("Loading pull requests…".into()),
                                generation: 0,
                            });
                            this.save_workspace();
                            this.repositories.len() - 1
                        };
                        this.refresh_repository(index, cx);
                        if let Some(number) = this.startup_pr.take() {
                            this.open_pr(index, number, cx);
                        }
                        this.status = format!(
                            "Added {} for {}",
                            repository.full_name(),
                            repository.account.login
                        );
                        this.setup_open = false;
                    }
                    Err(error) => {
                        this.status = format!("Cannot add repository: {error:#}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn save_workspace(&mut self) {
        if self.persistence_error.is_some() {
            return;
        }
        self.workspace.tabs = self
            .tabs
            .iter()
            .filter_map(|tab| {
                tab.session.as_ref().map(|session| TabState {
                    repository_key: tab.repository.cache_key(),
                    number: tab.pull_request.number,
                    revision: session.revision().clone(),
                    selected_file: session.selected_file().map(file_key),
                    scroll_offset: session.scroll_position(),
                    diff_mode: match session.diff_mode() {
                        DiffMode::Auto => "auto",
                        DiffMode::Unified => "unified",
                        DiffMode::SideBySide => "side-by-side",
                    }
                    .into(),
                })
            })
            .collect();
        self.workspace.active_tab = self.active_tab;
        if let Some(store) = &self.store
            && let Err(error) = store.save_workspace(&self.workspace)
        {
            self.persistence_error = Some(format!("Workspace was not saved: {error:#}"));
        }
    }

    fn persist_session(&mut self, index: usize, cx: &mut Context<Root>) {
        let Some(store) = self.store.clone() else {
            return;
        };
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.session_persistence_error.is_some() {
            return;
        }
        let Some(session) = tab.session.clone() else {
            return;
        };
        let repository = tab.repository.clone();
        let number = tab.pull_request.number;
        let key = repository.cache_key();
        let latest = self.session_save_latest.clone();
        let lock = self.session_save_lock.clone();
        let sequence = latest.fetch_add(1, Ordering::AcqRel) + 1;
        let task = cx.background_spawn(async move {
            let _guard = lock
                .lock()
                .map_err(|_| anyhow::anyhow!("review session save lock failed"))?;
            if latest.load(Ordering::Acquire) != sequence {
                return Ok(());
            }
            store.save_review_session(&repository, number, &session)
        });
        cx.spawn(async move |root, cx| {
            let result = task.await;
            let _ = root.update(cx, |root, cx| {
                let Root::Review(this) = root else { return };
                if let Err(error) = result
                    && let Some(tab) = this.tabs.iter_mut().find(|tab| {
                        tab.repository.cache_key() == key && tab.pull_request.number == number
                    })
                {
                    tab.session_persistence_error = Some(format!(
                        "Review progress was not saved; existing data was preserved: {error:#}"
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn load_selected_local_file(&mut self, index: usize, cx: &mut Context<Root>) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        let Some(path) = tab.repository.local_path.clone() else {
            return;
        };
        let Some(session) = tab.session.as_ref() else {
            return;
        };
        let Some(file) = session.selected_file() else {
            return;
        };
        if file.patch.is_some() {
            self.persist_session(index, cx);
            return;
        }
        tab.generation += 1;
        let generation = tab.generation;
        let repository_key = tab.repository.cache_key();
        let number = tab.pull_request.number;
        let revision = session.revision().clone();
        let selected_key = file_key(file);
        tab.state = LoadState::Loading("Loading selected local file…".into());
        let request_revision = revision.clone();
        let request_key = selected_key.clone();
        let task = cx.background_spawn(async move {
            load_local_file(&path, &request_revision, &request_key, true)
        });
        cx.spawn(async move |root, cx| {
            let result = task.await;
            let _ = root.update(cx, |root, cx| {
                let Root::Review(this) = root else { return };
                let Some(tab_index) = this.tabs.iter().position(|tab| {
                    tab.repository.cache_key() == repository_key
                        && tab.pull_request.number == number
                        && tab.generation == generation
                        && tab.local_inventory
                }) else {
                    return;
                };
                match result {
                    Ok(file) => {
                        let installed =
                            this.tabs[tab_index]
                                .session
                                .as_mut()
                                .is_some_and(|session| {
                                    session.install_file_patch(&revision, file).is_ok()
                                });
                        if installed {
                            this.tabs[tab_index].state = LoadState::Ready;
                            this.rebuild_diff(tab_index, this.wide);
                            this.persist_session(tab_index, cx);
                        }
                    }
                    Err(error) => {
                        this.tabs[tab_index].state =
                            LoadState::Error(format!("Selected local file unavailable: {error:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_all(&mut self, cx: &mut Context<Root>) {
        for index in 0..self.repositories.len() {
            self.refresh_repository(index, cx);
        }
    }

    fn refresh_repository(&mut self, index: usize, cx: &mut Context<Root>) {
        let Some(runtime) = self.repositories.get_mut(index) else {
            return;
        };
        runtime.generation += 1;
        let generation = runtime.generation;
        let repository = runtime.repository.clone();
        let repo_key = repository.cache_key();
        if runtime.pull_requests.is_empty() {
            runtime.state = LoadState::Loading("Loading pull requests…".into());
        }
        let provider = GithubProvider::new(repository.account.clone());
        let requested_state = {
            let state = self.workspace.view().filter.state;
            if state.is_empty() {
                "open".to_owned()
            } else {
                state
            }
        };
        let task = cx.background_spawn(async move {
            provider.list_pull_requests(&repository, &requested_state)
        });
        cx.spawn(async move |root, cx| {
            let result = task.await;
            let _ = root.update(cx, |root, cx| {
                let Root::Review(this) = root else { return };
                let Some(runtime) = this.repositories.iter_mut().find(|runtime| {
                    runtime.repository.cache_key() == repo_key && runtime.generation == generation
                }) else {
                    return;
                };
                match result {
                    Ok(pull_requests) => {
                        if let Some(store) = &this.store {
                            let _ = store.save_pull_requests(&runtime.repository, &pull_requests);
                        }
                        runtime.pull_requests = pull_requests;
                        runtime.state = LoadState::Ready;
                        this.schedule.succeeded(&format!("sidebar:{repo_key}"));
                    }
                    Err(error) => {
                        this.schedule.failed(&format!("sidebar:{repo_key}"));
                        runtime.state = if runtime.pull_requests.is_empty() {
                            LoadState::Error(format!("Refresh failed: {error:#}"))
                        } else {
                            LoadState::Cached(format!("Offline/cache · {error:#}"))
                        };
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_pr(&mut self, repo_index: usize, number: u64, cx: &mut Context<Root>) {
        let Some(repository) = self
            .repositories
            .get(repo_index)
            .map(|runtime| runtime.repository.clone())
        else {
            return;
        };
        if let Some(index) = self.tabs.iter().position(|tab| {
            tab.repository.cache_key() == repository.cache_key()
                && tab.pull_request.number == number
        }) {
            self.active_tab = Some(index);
            self.setup_open = false;
            cx.notify();
            return;
        }
        if let Some(pull_request) = self.repositories[repo_index]
            .pull_requests
            .iter()
            .find(|pull_request| pull_request.number == number)
            .cloned()
        {
            self.install_tab(repository, pull_request, cx);
            return;
        }
        self.status = format!("Loading #{} from {}…", number, repository.full_name());
        let key = repository.cache_key();
        let request_repo = repository.clone();
        let task = cx.background_spawn(async move {
            GithubProvider::new(request_repo.account.clone()).pull_request(&request_repo, number)
        });
        cx.spawn(async move |root, cx| {
            let result = task.await;
            let _ = root.update(cx, |root, cx| {
                let Root::Review(this) = root else { return };
                match result {
                    Ok(pull_request) => {
                        if let Some(runtime) = this
                            .repositories
                            .iter_mut()
                            .find(|runtime| runtime.repository.cache_key() == key)
                            && !runtime
                                .pull_requests
                                .iter()
                                .any(|listed| listed.number == pull_request.number)
                        {
                            runtime.pull_requests.push(pull_request.clone());
                        }
                        let Some(repository) = this
                            .repositories
                            .iter()
                            .find(|runtime| runtime.repository.cache_key() == key)
                            .map(|runtime| runtime.repository.clone())
                        else {
                            return;
                        };
                        this.install_tab(repository, pull_request, cx);
                    }
                    Err(error) => {
                        this.status = format!("Cannot load PR #{number}: {error:#}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn install_tab(
        &mut self,
        repository: Repository,
        pull_request: PullRequest,
        cx: &mut Context<Root>,
    ) {
        let revision = pull_request.revision();
        let restored = self
            .store
            .as_ref()
            .map(|store| store.load_review_session(&repository, pull_request.number));
        let (session, state, session_persistence_error) = match restored {
            Some(Ok(session)) => (
                Some(session),
                LoadState::Cached("Restored pinned review session · refreshing metadata".into()),
                None,
            ),
            Some(Err(error))
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                (
                    None,
                    LoadState::Loading("Loading immutable comparison…".into()),
                    None,
                )
            }
            Some(Err(error)) => (
                None,
                LoadState::Loading("Loading immutable comparison…".into()),
                Some(format!(
                    "Saved review session is unreadable and will not be overwritten: {error:#}"
                )),
            ),
            None => (
                None,
                LoadState::Loading("Loading immutable comparison…".into()),
                None,
            ),
        };
        let local_inventory = repository.local_path.is_some()
            && session.as_ref().is_some_and(|session| {
                session
                    .comparison()
                    .files
                    .iter()
                    .any(|file| file.patch.is_none())
            });
        self.tabs.push(ReviewTab {
            repository,
            pull_request,
            session,
            state,
            generation: 0,
            metadata_generation: 0,
            diff_rows: Vec::new(),
            diff_scroll: UniformListScrollHandle::new(),
            inspector_section: InspectorSection::Overview,
            local_inventory,
            session_persistence_error,
            details: None,
            details_state: LoadState::Loading("Loading PR details…".into()),
            details_generation: 0,
        });
        let index = self.tabs.len() - 1;
        self.active_tab = Some(index);
        self.setup_open = false;
        if self.tabs[index].session.is_some() {
            self.rebuild_diff(index, false);
            if self.tabs[index].local_inventory
                && self.tabs[index]
                    .session
                    .as_ref()
                    .and_then(|session| session.selected_file())
                    .is_some_and(|file| file.patch.is_none())
            {
                self.load_selected_local_file(index, cx);
            }
            self.refresh_active(cx);
        } else {
            self.load_comparison(index, revision, false, cx);
            self.refresh_details(index, cx);
        }
    }

    fn load_comparison(
        &mut self,
        index: usize,
        revision: Revision,
        advancing: bool,
        cx: &mut Context<Root>,
    ) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        tab.generation += 1;
        let generation = tab.generation;
        let repository = tab.repository.clone();
        let number = tab.pull_request.number;
        let key = repository.cache_key();
        tab.state = LoadState::Loading(
            if advancing {
                "Loading newer revision…"
            } else {
                "Loading immutable comparison…"
            }
            .into(),
        );
        let store = self.store.clone();
        let task = cx.background_spawn(async move {
            let provider = GithubProvider::new(repository.account.clone());
            if let Some(path) = repository.local_path.as_deref()
                && let Ok(comparison) = local_pr_inventory(path, &revision)
            {
                return Ok((comparison, false, true));
            }
            match provider.comparison(&repository, number, &revision) {
                Ok(comparison) => Ok((comparison, false, false)),
                Err(error) => match store
                    .as_ref()
                    .and_then(|store| store.load_comparison(&repository, number, &revision).ok())
                {
                    Some(cached) => Ok((cached, true, false)),
                    None => Err(error),
                },
            }
        });
        cx.spawn(async move |root, cx| {
            let result = task.await;
            let _ = root.update(cx, |root, cx| {
                let Root::Review(this) = root else { return };
                let Some(tab_index) = this.tabs.iter().position(|tab| {
                    tab.repository.cache_key() == key
                        && tab.pull_request.number == number
                        && tab.generation == generation
                }) else {
                    return;
                };
                match result {
                    Ok((comparison, cached, local_inventory)) => {
                        if !cached && let Some(store) = &this.store {
                            let _ = store.save_comparison(
                                &this.tabs[tab_index].repository,
                                number,
                                &comparison,
                            );
                        }
                        if advancing {
                            if let Some(session) = &mut this.tabs[tab_index].session
                                && let Err(error) = session.advance(comparison)
                            {
                                this.tabs[tab_index].state =
                                    LoadState::Error(format!("Cannot advance: {error:#}"));
                                cx.notify();
                                return;
                            }
                        } else {
                            this.tabs[tab_index].session = Some(ReviewSession::new(comparison));
                        }
                        this.tabs[tab_index].state = if cached {
                            LoadState::Cached("Offline · immutable comparison from cache".into())
                        } else {
                            LoadState::Ready
                        };
                        this.tabs[tab_index].local_inventory = local_inventory;
                        this.rebuild_diff(tab_index, false);
                        this.schedule.succeeded(&format!("pr:{key}:{number}"));
                        this.save_workspace();
                        if local_inventory {
                            this.load_selected_local_file(tab_index, cx);
                        } else {
                            this.persist_session(tab_index, cx);
                        }
                    }
                    Err(error) => {
                        this.tabs[tab_index].state =
                            LoadState::Error(format!("Comparison unavailable: {error:#}"));
                        this.schedule.failed(&format!("pr:{key}:{number}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_active(&mut self, cx: &mut Context<Root>) {
        let Some(index) = self.active_tab else { return };
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        tab.metadata_generation += 1;
        let generation = tab.metadata_generation;
        let repository = tab.repository.clone();
        let number = tab.pull_request.number;
        let key = repository.cache_key();
        let task = cx.background_spawn(async move {
            GithubProvider::new(repository.account.clone()).pull_request(&repository, number)
        });
        cx.spawn(async move |root, cx| {
            let result = task.await;
            let _ = root.update(cx, |root, cx| {
                let Root::Review(this) = root else { return };
                let Some(tab) = this.tabs.iter_mut().find(|tab| {
                    tab.repository.cache_key() == key
                        && tab.pull_request.number == number
                        && tab.metadata_generation == generation
                }) else {
                    return;
                };
                match result {
                    Ok(pull_request) => {
                        if let Some(session) = &mut tab.session {
                            session.observe_revision(pull_request.revision());
                        }
                        tab.pull_request = pull_request;
                        this.schedule.succeeded(&format!("pr:{key}:{number}"));
                    }
                    Err(error) => {
                        this.status = format!(
                            "Metadata refresh failed; displayed revision unchanged: {error:#}"
                        );
                        this.schedule.failed(&format!("pr:{key}:{number}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        self.refresh_details(index, cx);
    }

    fn refresh_details(&mut self, index: usize, cx: &mut Context<Root>) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        tab.details_generation += 1;
        let generation = tab.details_generation;
        let repository = tab.repository.clone();
        let number = tab.pull_request.number;
        let key = repository.cache_key();
        if tab.details.is_none() {
            tab.details_state = LoadState::Loading("Loading PR details…".into());
        }
        let task = cx.background_spawn(async move {
            GithubProvider::new(repository.account.clone()).details(&repository, number)
        });
        cx.spawn(async move |root, cx| {
            let result = task.await;
            let _ = root.update(cx, |root, cx| {
                let Root::Review(this) = root else { return };
                let Some(tab) = this.tabs.iter_mut().find(|tab| {
                    tab.repository.cache_key() == key
                        && tab.pull_request.number == number
                        && tab.details_generation == generation
                }) else {
                    return;
                };
                match result {
                    Ok(details) => {
                        tab.details = Some(details);
                        tab.details_state = LoadState::Ready;
                    }
                    Err(error) => {
                        tab.details_state = if tab.details.is_some() {
                            LoadState::Cached(format!("Details refresh failed · {error:#}"))
                        } else {
                            LoadState::Error(format!("PR details unavailable · {error:#}"))
                        };
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn rebuild_diff(&mut self, index: usize, wide: bool) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        let Some(session) = &tab.session else { return };
        let Some(file) = session.selected_file() else {
            tab.diff_rows.clear();
            return;
        };
        let scroll_position = session.scroll_position();
        tab.diff_rows = build_rows(parse_file(file), session.diff_mode().resolve(wide));
        tab.diff_scroll = UniformListScrollHandle::new();
        tab.diff_scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-scroll_position)));
    }

    fn capture_scroll(&mut self, index: usize) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        let position = -tab.diff_scroll.0.borrow().base_handle.offset().y.as_f32();
        if let Some(session) = &mut tab.session {
            session.set_scroll_position(position);
        }
    }

    fn select_file(&mut self, key: &str, wide: bool, cx: &mut Context<Root>) {
        let Some(index) = self.active_tab else { return };
        self.capture_scroll(index);
        if self.tabs[index]
            .session
            .as_mut()
            .is_some_and(|session| session.select_file(key))
        {
            self.rebuild_diff(index, wide);
            self.save_workspace();
            if self.tabs[index].local_inventory
                && self.tabs[index]
                    .session
                    .as_ref()
                    .and_then(|session| session.selected_file())
                    .is_some_and(|file| file.patch.is_none())
            {
                self.load_selected_local_file(index, cx);
                return;
            }
            self.persist_session(index, cx);
        }
    }

    fn navigate_file(&mut self, next: bool, window: &mut Window, cx: &mut Context<Root>) {
        let Some(index) = self.active_tab else { return };
        self.capture_scroll(index);
        let changed = self.tabs[index].session.as_mut().is_some_and(|session| {
            if next {
                session.next_file()
            } else {
                session.previous_file()
            }
        });
        if changed {
            self.rebuild_diff(index, window.bounds().size.width > px(1180.));
            self.save_workspace();
            if self.tabs[index].local_inventory
                && self.tabs[index]
                    .session
                    .as_ref()
                    .and_then(|session| session.selected_file())
                    .is_some_and(|file| file.patch.is_none())
            {
                self.load_selected_local_file(index, cx);
            } else {
                self.persist_session(index, cx);
            }
            cx.notify();
        }
    }

    fn next_file(&mut self, _: &NextFile, window: &mut Window, cx: &mut Context<Root>) {
        self.navigate_file(true, window, cx);
    }

    fn previous_file(&mut self, _: &PreviousFile, window: &mut Window, cx: &mut Context<Root>) {
        self.navigate_file(false, window, cx);
    }

    fn refresh(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Root>) {
        self.refresh_all(cx);
        self.refresh_active(cx);
    }

    fn close_tab(&mut self, _: &CloseTab, _: &mut Window, cx: &mut Context<Root>) {
        if let Some(index) = self.active_tab.take() {
            self.capture_scroll(index);
            self.persist_session(index, cx);
            self.tabs.remove(index);
            self.active_tab = (!self.tabs.is_empty()).then(|| index.min(self.tabs.len() - 1));
            self.save_workspace();
            cx.notify();
        }
    }

    fn cycle_diff(&mut self, _: &CycleDiffMode, window: &mut Window, cx: &mut Context<Root>) {
        let Some(index) = self.active_tab else { return };
        self.capture_scroll(index);
        if let Some(session) = &mut self.tabs[index].session {
            session.set_diff_mode(match session.diff_mode() {
                DiffMode::Auto => DiffMode::Unified,
                DiffMode::Unified => DiffMode::SideBySide,
                DiffMode::SideBySide => DiffMode::Auto,
            });
            self.rebuild_diff(index, window.bounds().size.width > px(1180.));
            self.save_workspace();
            self.persist_session(index, cx);
            cx.notify();
        }
    }

    fn toggle_viewed(&mut self, key: &str, cx: &mut Context<Root>) {
        let Some(index) = self.active_tab else { return };
        if let Some(session) = &mut self.tabs[index].session {
            let viewed = !session.is_viewed(key);
            if session.mark_viewed(key, viewed) {
                self.save_workspace();
                self.persist_session(index, cx);
                cx.notify();
            }
        }
    }

    fn apply_filter(&mut self, personal: PersonalFilter, cx: &mut Context<Root>) {
        let mut view = self.workspace.view();
        view.filter.search = self.query.read(cx).value().trim().to_owned();
        view.filter.personal = personal;
        self.workspace.views[self.workspace.selected_view] = view;
        self.save_workspace();
        cx.notify();
    }

    fn cycle_group(&mut self, cx: &mut Context<Root>) {
        let mut view = self.workspace.view();
        view.groups = match view.groups.first() {
            Some(GroupBy::Repository) => vec![GroupBy::TargetBranch],
            Some(GroupBy::TargetBranch) => vec![GroupBy::SourceBranch],
            Some(GroupBy::SourceBranch) => vec![GroupBy::Stack],
            _ => vec![GroupBy::Repository],
        };
        self.workspace.views[self.workspace.selected_view] = view;
        self.save_workspace();
        cx.notify();
    }

    fn cycle_state(&mut self, cx: &mut Context<Root>) {
        let mut view = self.workspace.view();
        view.filter.state = match view.filter.state.as_str() {
            "" | "open" => "closed",
            "closed" => "merged",
            "merged" => "all",
            _ => "open",
        }
        .into();
        self.workspace.views[self.workspace.selected_view] = view;
        self.save_workspace();
        self.refresh_all(cx);
        cx.notify();
    }

    fn save_current_view(&mut self, cx: &mut Context<Root>) {
        let mut view = self.workspace.view();
        let state = if view.filter.state.is_empty() {
            "Open"
        } else {
            &view.filter.state
        };
        view.name = format!(
            "{} · {} · {}",
            state,
            group_label(view.groups.first()),
            self.workspace.views.len()
        );
        self.workspace.views.push(view);
        self.workspace.selected_view = self.workspace.views.len() - 1;
        self.save_workspace();
        self.status = "Saved current filter and grouping".into();
        cx.notify();
    }

    fn select_view(&mut self, index: usize, window: &mut Window, cx: &mut Context<Root>) {
        if index >= self.workspace.views.len() {
            return;
        }
        self.workspace.selected_view = index;
        let search = self.workspace.view().filter.search;
        self.query
            .update(cx, |query, cx| query.set_value(search, window, cx));
        self.save_workspace();
        self.refresh_all(cx);
        cx.notify();
    }

    fn advance_revision(&mut self, cx: &mut Context<Root>) {
        let Some(index) = self.active_tab else { return };
        let revision = self.tabs[index]
            .session
            .as_ref()
            .and_then(|session| session.available_revision().cloned());
        if let Some(revision) = revision {
            self.load_comparison(index, revision, true, cx);
        }
    }

    fn render(&mut self, window: &mut Window, cx: &mut Context<Root>) -> impl IntoElement {
        let colors = palette(is_dark(window));
        div()
            .id("review-workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(|root, action: &Refresh, window, cx| {
                if let Root::Review(this) = root {
                    this.refresh(action, window, cx)
                }
            }))
            .on_action(cx.listener(|root, action: &NextFile, window, cx| {
                if let Root::Review(this) = root {
                    this.next_file(action, window, cx)
                }
            }))
            .on_action(cx.listener(|root, action: &PreviousFile, window, cx| {
                if let Root::Review(this) = root {
                    this.previous_file(action, window, cx)
                }
            }))
            .on_action(cx.listener(|root, action: &CloseTab, window, cx| {
                if let Root::Review(this) = root {
                    this.close_tab(action, window, cx)
                }
            }))
            .on_action(cx.listener(|root, _: &TogglePalette, _, cx| {
                if let Root::Review(this) = root {
                    this.command_palette = !this.command_palette;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|root, _: &ToggleInspector, _, cx| {
                if let Root::Review(this) = root {
                    this.inspector_open = !this.inspector_open;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|root, action: &CycleDiffMode, window, cx| {
                if let Root::Review(this) = root {
                    this.cycle_diff(action, window, cx)
                }
            }))
            .on_action(cx.listener(|root, _: &OpenRepositorySetup, _, cx| {
                if let Root::Review(this) = root {
                    this.setup_open = true;
                    this.command_palette = false;
                    cx.notify();
                }
            }))
            .size_full()
            .flex()
            .font_family(UI_FONT)
            .text_size(px(13.))
            .text_color(colors.text)
            .bg(rgba(0x00000000))
            .child(self.render_sidebar(colors, cx))
            .child(self.render_main(colors, window, cx))
            .when(self.command_palette, |root| {
                root.child(self.render_palette(colors, cx))
            })
    }

    fn render_sidebar(&self, colors: Palette, cx: &mut Context<Root>) -> impl IntoElement {
        let view = self.workspace.view();
        let group = group_label(view.groups.first());
        let state = if view.filter.state.is_empty() {
            "open"
        } else {
            &view.filter.state
        };
        let saved_views = self
            .workspace
            .views
            .iter()
            .enumerate()
            .map(|(index, saved)| {
                side_control(&saved.name, self.workspace.selected_view == index, colors)
                    .id(SharedString::from(format!("saved-view-{index}")))
                    .on_click(cx.listener(move |root, _, window, cx| {
                        if let Root::Review(this) = root {
                            this.select_view(index, window, cx)
                        }
                    }))
            });
        let participating_incomplete = view.filter.personal == PersonalFilter::Participating
            && self.repositories.iter().any(|runtime| {
                runtime
                    .pull_requests
                    .iter()
                    .any(|pull_request| !pull_request.participants_complete)
            });
        let mut rows = Vec::new();
        for (repo_index, runtime) in self.repositories.iter().enumerate() {
            let active_number = self
                .active_tab
                .and_then(|index| self.tabs.get(index))
                .filter(|tab| tab.repository.cache_key() == runtime.repository.cache_key())
                .map(|tab| tab.pull_request.number);
            let mut filtered: Vec<_> = runtime
                .pull_requests
                .iter()
                .filter(|pull_request| {
                    view.filter
                        .matches(pull_request, &runtime.repository.account.login)
                })
                .cloned()
                .collect();
            filtered.sort_by_key(|pull_request| {
                (
                    pull_request.number != active_number.unwrap_or_default(),
                    Reverse(pull_request.number),
                )
            });
            let label = filtered
                .first()
                .map(|pull_request| {
                    group_path(
                        &runtime.repository,
                        pull_request,
                        &view.groups,
                        &runtime.pull_requests,
                    )
                    .join(" / ")
                })
                .unwrap_or_else(|| {
                    format!(
                        "{} · {}",
                        runtime.repository.full_name(),
                        runtime.repository.account.login
                    )
                });
            rows.push(
                div()
                    .px_3()
                    .pt_3()
                    .pb_1()
                    .text_xs()
                    .text_color(colors.muted)
                    .child(label)
                    .into_any_element(),
            );
            for pull_request in filtered {
                let selected = self
                    .active_tab
                    .and_then(|index| self.tabs.get(index))
                    .is_some_and(|tab| {
                        tab.repository.cache_key() == runtime.repository.cache_key()
                            && tab.pull_request.number == pull_request.number
                    });
                let number = pull_request.number;
                rows.push(
                    div()
                        .id(SharedString::from(format!("pr-{repo_index}-{number}")))
                        .mx_2()
                        .my_px()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .cursor_pointer()
                        .when(selected, |row| row.bg(colors.selected))
                        .hover(|row| row.bg(colors.selected))
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(div().text_color(colors.faint).child(format!("#{number}")))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(pull_request.title),
                                ),
                        )
                        .child(
                            div()
                                .mt_1()
                                .text_xs()
                                .text_color(colors.muted)
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(pull_request.source_branch),
                        )
                        .on_click(cx.listener(move |root, _, _, cx| {
                            if let Root::Review(this) = root {
                                this.open_pr(repo_index, number, cx)
                            }
                        }))
                        .into_any_element(),
                );
            }
            if let Some(notice) = runtime.state.notice() {
                rows.push(
                    div()
                        .px_5()
                        .py_1()
                        .text_xs()
                        .text_color(colors.faint)
                        .child(notice)
                        .into_any_element(),
                );
            }
        }
        div()
            .w(px(292.))
            .min_w(px(250.))
            .h_full()
            .flex()
            .flex_col()
            .bg(colors.sidebar)
            .border_r_1()
            .border_color(colors.border)
            .child(
                div()
                    .h(px(48.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("cibergit"))
                    .child(div().text_xs().text_color(colors.faint).child("READ ONLY")),
            )
            .child(
                div().px_3().pb_2().child(
                    div()
                        .h(px(32.))
                        .px_2()
                        .rounded_md()
                        .bg(colors.elevated)
                        .border_1()
                        .border_color(colors.border)
                        .font_family(UI_FONT)
                        .child(Input::new(&self.query)),
                ),
            )
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_xs()
                    .child(
                        div()
                            .id("cycle-state")
                            .cursor_pointer()
                            .text_color(colors.accent)
                            .child(format!("State: {state}"))
                            .on_click(cx.listener(|root, _, _, cx| {
                                if let Root::Review(this) = root { this.cycle_state(cx) }
                            })),
                    )
                    .child(
                        div()
                            .id("save-view")
                            .cursor_pointer()
                            .text_color(colors.accent)
                            .child("Save view")
                            .on_click(cx.listener(|root, _, _, cx| {
                                if let Root::Review(this) = root { this.save_current_view(cx) }
                            })),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .children(saved_views),
            )
            .when(participating_incomplete, |sidebar| {
                sidebar.child(
                    div()
                        .px_3()
                        .pt_2()
                        .text_xs()
                        .text_color(colors.amber)
                        .child("Participating results may be incomplete; known participants still match."),
                )
            })
            .child(
                div()
                    .px_3()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .child(
                        side_control(
                            "All open",
                            view.filter.personal == PersonalFilter::All,
                            colors,
                        )
                        .id("filter-all")
                        .on_click(cx.listener(|root, _, _, cx| {
                            if let Root::Review(this) = root {
                                this.apply_filter(PersonalFilter::All, cx)
                            }
                        })),
                    )
                    .child(
                        side_control(
                            "Needs review",
                            view.filter.personal == PersonalFilter::ReviewRequested,
                            colors,
                        )
                        .id("filter-review")
                        .on_click(cx.listener(|root, _, _, cx| {
                            if let Root::Review(this) = root {
                                this.apply_filter(PersonalFilter::ReviewRequested, cx)
                            }
                        })),
                    )
                    .child(
                        side_control("Mine", view.filter.personal == PersonalFilter::Own, colors)
                            .id("filter-mine")
                            .on_click(cx.listener(|root, _, _, cx| {
                                if let Root::Review(this) = root {
                                    this.apply_filter(PersonalFilter::Own, cx)
                                }
                            })),
                    )
                    .child(
                        side_control(
                            "Participating",
                            view.filter.personal == PersonalFilter::Participating,
                            colors,
                        )
                        .id("filter-participating")
                        .on_click(cx.listener(|root, _, _, cx| {
                            if let Root::Review(this) = root {
                                this.apply_filter(PersonalFilter::Participating, cx)
                            }
                        })),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_xs()
                    .text_color(colors.muted)
                    .child(format!("Group by {group}"))
                    .child(
                        div()
                            .id("cycle-group")
                            .cursor_pointer()
                            .text_color(colors.accent)
                            .child("Change")
                            .on_click(cx.listener(|root, _, _, cx| {
                                if let Root::Review(this) = root {
                                    this.cycle_group(cx)
                                }
                            })),
                    ),
            )
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                div()
                    .px_3()
                    .py_3()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .id("add-repository")
                            .h(px(32.))
                            .px_3()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .bg(colors.elevated)
                            .cursor_pointer()
                            .hover(|button| button.bg(colors.selected))
                            .child("＋ Add repository  ⌘O")
                            .on_click(cx.listener(|root, _, _, cx| {
                                if let Root::Review(this) = root {
                                    this.setup_open = true;
                                    cx.notify();
                                }
                            })),
                    ),
            )
    }

    fn render_main(
        &self,
        colors: Palette,
        window: &Window,
        cx: &mut Context<Root>,
    ) -> impl IntoElement {
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(colors.canvas)
            .child(self.render_tabs(colors, cx))
            .child(if self.setup_open {
                self.render_setup(colors, cx).into_any_element()
            } else if let Some(index) = self.active_tab {
                self.render_review(index, colors, window, cx)
                    .into_any_element()
            } else {
                self.render_empty(colors, cx).into_any_element()
            })
            .child(self.render_status(colors))
    }

    fn render_tabs(&self, colors: Palette, cx: &mut Context<Root>) -> impl IntoElement {
        let tabs = self.tabs.iter().enumerate().map(|(index, tab)| {
            div()
                .id(SharedString::from(format!("tab-{index}")))
                .h(px(42.))
                .px_3()
                .flex()
                .items_center()
                .gap_2()
                .border_r_1()
                .border_color(colors.border)
                .cursor_pointer()
                .when(self.active_tab == Some(index), |tab| tab.bg(colors.surface))
                .child(
                    div()
                        .max_w(px(220.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(format!(
                            "{}  #{}",
                            tab.repository.name, tab.pull_request.number
                        )),
                )
                .on_click(cx.listener(move |root, _, _, cx| {
                    if let Root::Review(this) = root {
                        this.active_tab = Some(index);
                        this.setup_open = false;
                        cx.notify();
                    }
                }))
        });
        div()
            .h(px(42.))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.canvas)
            .children(tabs)
    }

    fn render_setup(&self, colors: Palette, cx: &mut Context<Root>) -> impl IntoElement {
        let accounts = self.accounts.iter().enumerate().map(|(index, account)| {
            side_control(&account.login, self.selected_account == index, colors)
                .id(SharedString::from(format!("account-{index}")))
                .on_click(cx.listener(move |root, _, _, cx| {
                    if let Root::Review(this) = root {
                        this.selected_account = index;
                        cx.notify();
                    }
                }))
        });
        let account_status = self
            .accounts_state
            .notice()
            .unwrap_or_else(|| "Choose the gh identity used only for this repository".into());
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .child(
                div()
                    .w(px(560.))
                    .p_6()
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.surface)
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Add a repository"),
                    )
                    .child(
                        div()
                            .mt_1()
                            .text_color(colors.muted)
                            .child("Review without cloning. Existing local folders work too."),
                    )
                    .child(field_label("REPOSITORY", colors).mt_5())
                    .child(input_box(&self.repository_input, colors))
                    .child(field_label("GITHUB ACCOUNT", colors).mt_4())
                    .child(div().mt_2().flex().flex_wrap().gap_2().children(accounts))
                    .child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(colors.faint)
                            .child(account_status),
                    )
                    .child(field_label("OPEN PR NUMBER (OPTIONAL)", colors).mt_4())
                    .child(div().w(px(160.)).child(input_box(&self.pr_input, colors)))
                    .child(
                        div()
                            .mt_5()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .id("cancel-setup")
                                    .px_4()
                                    .py_2()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .child("Cancel")
                                    .on_click(cx.listener(|root, _, _, cx| {
                                        if let Root::Review(this) = root {
                                            this.setup_open = false;
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                div()
                                    .id("confirm-add-repository")
                                    .px_4()
                                    .py_2()
                                    .rounded_md()
                                    .bg(colors.text)
                                    .text_color(colors.canvas)
                                    .cursor_pointer()
                                    .child("Add repository")
                                    .on_click(cx.listener(|root, _, _, cx| {
                                        if let Root::Review(this) = root {
                                            this.startup_pr =
                                                this.pr_input.read(cx).value().trim().parse().ok();
                                            this.add_repository(cx);
                                        }
                                    })),
                            ),
                    ),
            )
    }

    fn render_empty(&self, colors: Palette, cx: &mut Context<Root>) -> impl IntoElement {
        div().flex_1().flex().items_center().justify_center().child(
            div()
                .text_center()
                .child(
                    div()
                        .text_size(px(20.))
                        .font_weight(FontWeight::MEDIUM)
                        .child("Repository-to-PR review"),
                )
                .child(
                    div()
                        .mt_2()
                        .text_color(colors.muted)
                        .child("Choose an open pull request or add another repository."),
                )
                .child(
                    div()
                        .id("empty-add")
                        .mt_5()
                        .mx_auto()
                        .px_4()
                        .py_2()
                        .rounded_md()
                        .bg(colors.elevated)
                        .cursor_pointer()
                        .child("Add repository")
                        .on_click(cx.listener(|root, _, _, cx| {
                            if let Root::Review(this) = root {
                                this.setup_open = true;
                                cx.notify();
                            }
                        })),
                ),
        )
    }

    fn render_review(
        &self,
        index: usize,
        colors: Palette,
        window: &Window,
        cx: &mut Context<Root>,
    ) -> impl IntoElement {
        let tab = &self.tabs[index];
        let session = tab.session.as_ref();
        let revision = session
            .map(|session| session.revision().head_sha.as_str())
            .map(|sha| &sha[..sha.len().min(8)])
            .unwrap_or("loading");
        let newer = session
            .and_then(|session| session.available_revision())
            .is_some();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(colors.surface)
            .child(
                div()
                    .px_5()
                    .py_3()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(colors.elevated)
                                    .text_xs()
                                    .child(format!("#{}", tab.pull_request.number)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(16.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(tab.pull_request.title.clone()),
                            )
                            .child(
                                div()
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(colors.border)
                                    .child("Review"),
                            )
                            .child(
                                div()
                                    .px_3()
                                    .py_1()
                                    .text_color(colors.muted)
                                    .child("Published revision"),
                            ),
                    )
                    .child(
                        div()
                            .mt_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_sm()
                            .text_color(colors.muted)
                            .child(format!(
                                "{}  →  {}",
                                tab.pull_request.source_branch, tab.pull_request.target_branch
                            ))
                            .child("·")
                            .child(format!("Full pull request · {revision}"))
                            .when(newer, |row| {
                                row.child(
                                    div()
                                        .id("advance-revision")
                                        .ml_2()
                                        .px_2()
                                        .py_1()
                                        .rounded_md()
                                        .bg(colors.amber)
                                        .text_color(colors.canvas)
                                        .cursor_pointer()
                                        .child("New head available · Advance manually")
                                        .on_click(cx.listener(|root, _, _, cx| {
                                            if let Root::Review(this) = root {
                                                this.advance_revision(cx)
                                            }
                                        })),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("diff-mode")
                                    .cursor_pointer()
                                    .text_color(colors.accent)
                                    .child(
                                        session
                                            .map(|session| match session.diff_mode() {
                                                DiffMode::Auto => "Diff: Auto",
                                                DiffMode::Unified => "Diff: Unified",
                                                DiffMode::SideBySide => "Diff: Side by side",
                                            })
                                            .unwrap_or("Diff: Auto"),
                                    )
                                    .on_click(cx.listener(|root, _, window, cx| {
                                        if let Root::Review(this) = root {
                                            this.cycle_diff(&CycleDiffMode, window, cx)
                                        }
                                    })),
                            )
                            .child(
                                div()
                                    .id("toggle-inspector")
                                    .cursor_pointer()
                                    .text_color(colors.accent)
                                    .child(if self.inspector_open {
                                        "Hide details"
                                    } else {
                                        "Show details"
                                    })
                                    .on_click(cx.listener(|root, _, _, cx| {
                                        if let Root::Review(this) = root {
                                            this.inspector_open = !this.inspector_open;
                                            cx.notify();
                                        }
                                    })),
                            ),
                    ),
            )
            .when_some(tab.state.notice(), |view, notice| {
                view.child(
                    div()
                        .px_5()
                        .py_2()
                        .bg(colors.elevated)
                        .text_color(colors.muted)
                        .child(notice),
                )
            })
            .when_some(
                session.and_then(|session| session.comparison().notice.clone()),
                |view, notice| {
                    view.child(
                        div()
                            .px_5()
                            .py_2()
                            .bg(colors.elevated)
                            .text_color(colors.amber)
                            .child(notice),
                    )
                },
            )
            .when_some(tab.session_persistence_error.clone(), |view, notice| {
                view.child(
                    div()
                        .px_5()
                        .py_2()
                        .bg(colors.elevated)
                        .text_color(colors.red)
                        .child(notice),
                )
            })
            .child(
                div()
                    .id("file-scroll")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_files(index, colors, window, cx))
                    .child(self.render_diff(index, colors))
                    .when(self.inspector_open, |body| {
                        body.child(self.render_inspector(index, colors, cx))
                    }),
            )
    }

    fn render_files(
        &self,
        index: usize,
        colors: Palette,
        window: &Window,
        cx: &mut Context<Root>,
    ) -> impl IntoElement {
        let tab = &self.tabs[index];
        let items: Vec<_> = tab
            .session
            .as_ref()
            .map(|session| {
                session
                    .comparison()
                    .files
                    .iter()
                    .map(|file| {
                        let key = file_key(file);
                        let selected = session
                            .selected_file()
                            .is_some_and(|selected| file_key(selected) == key);
                        let viewed = session.is_viewed(&key);
                        (
                            key,
                            file.path.clone(),
                            file.status.clone(),
                            file.patch
                                .as_ref()
                                .map(|_| (file.additions, file.deletions)),
                            selected,
                            viewed,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let wide = window.bounds().size.width > px(1180.);
        div()
            .w(px(250.))
            .min_w(px(210.))
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(colors.border)
            .bg(colors.canvas)
            .child(
                div()
                    .h(px(38.))
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(format!("Files  {}", items.len()))
                    .child(div().text_xs().text_color(colors.muted).child("⌘[  ⌘]")),
            )
            .child(
                div()
                    .id("changed-files-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(items.into_iter().map(
                        |(key, path, status, stats, selected, viewed)| {
                            let click_key = key.clone();
                            let viewed_key = key.clone();
                            div()
                                .id(SharedString::from(format!("file-{key}")))
                                .px_3()
                                .py_2()
                                .cursor_pointer()
                                .when(selected, |row| row.bg(colors.selected))
                                .hover(|row| row.bg(colors.selected))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(px(22.))
                                                .flex_none()
                                                .text_center()
                                                .text_xs()
                                                .text_color(colors.faint)
                                                .child(file_status_badge(&status)),
                                        )
                                        .child(
                                            div()
                                                .id(SharedString::from(format!("viewed-{key}")))
                                                .w(px(20.))
                                                .flex_none()
                                                .text_center()
                                                .text_color(if viewed {
                                                    colors.green
                                                } else {
                                                    colors.faint
                                                })
                                                .cursor_pointer()
                                                .child(if viewed { "✓" } else { "○" })
                                                .on_click(cx.listener(move |root, _, _, cx| {
                                                    if let Root::Review(this) = root {
                                                        this.toggle_viewed(&viewed_key, cx)
                                                    }
                                                })),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .overflow_hidden()
                                                .text_ellipsis()
                                                .child(path),
                                        ),
                                )
                                .child(
                                    div().pl_6().mt_1().flex().gap_2().text_xs().children(
                                        stats
                                            .map(|(additions, deletions)| {
                                                vec![
                                                    div()
                                                        .text_color(colors.green)
                                                        .child(format!("+{additions}")),
                                                    div()
                                                        .text_color(colors.red)
                                                        .child(format!("−{deletions}")),
                                                ]
                                            })
                                            .unwrap_or_else(|| {
                                                vec![
                                                    div()
                                                        .text_color(colors.faint)
                                                        .child("stats on load"),
                                                ]
                                            }),
                                    ),
                                )
                                .on_click(cx.listener(move |root, _, _, cx| {
                                    if let Root::Review(this) = root {
                                        this.select_file(&click_key, wide, cx);
                                        cx.notify();
                                    }
                                }))
                        },
                    )),
            )
    }

    fn render_diff(&self, index: usize, colors: Palette) -> impl IntoElement {
        let tab = &self.tabs[index];
        let header = tab
            .session
            .as_ref()
            .and_then(|session| session.selected_file())
            .map(|file| {
                if file.patch.is_some() {
                    format!("{}   +{} −{}", file.path, file.additions, file.deletions)
                } else {
                    format!("{}   metadata only", file.path)
                }
            })
            .unwrap_or_else(|| "Select a changed file".into());
        let rows = tab.diff_rows.clone();
        let count = rows.len();
        let scroll = tab.diff_scroll.clone();
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(colors.surface)
            .child(
                div()
                    .h(px(38.))
                    .px_4()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(colors.border)
                    .font_family(CODE_FONT)
                    .text_sm()
                    .child(header),
            )
            .child(if count == 0 {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(colors.muted)
                    .child("No text patch is available. Binary and media content is never loaded.")
                    .into_any_element()
            } else {
                uniform_list("diff-rows", count, move |range: Range<usize>, _, _| {
                    range
                        .map(|index| render_diff_row(&rows[index], colors))
                        .collect::<Vec<_>>()
                })
                .track_scroll(&scroll)
                .flex_1()
                .min_h_0()
                .into_any_element()
            })
    }

    fn render_inspector(
        &self,
        index: usize,
        colors: Palette,
        cx: &mut Context<Root>,
    ) -> impl IntoElement {
        let tab = &self.tabs[index];
        let current = tab.inspector_section;
        let section = |name: &'static str, value: InspectorSection| {
            side_control(name, current == value, colors)
                .id(SharedString::from(format!("inspector-{name}")))
                .on_click(cx.listener(move |root, _, _, cx| {
                    if let Root::Review(this) = root
                        && let Some(index) = this.active_tab
                    {
                        this.tabs[index].inspector_section = value;
                        cx.notify();
                    }
                }))
        };
        let content =
            match current {
                InspectorSection::Overview => {
                    let mut fields = vec![
                        detail("Author", &tab.pull_request.author, colors),
                        detail("State", &tab.pull_request.state, colors),
                        detail(
                            "Review",
                            empty_unknown(&tab.pull_request.review_status),
                            colors,
                        ),
                        detail(
                            "Labels",
                            if tab.pull_request.labels.is_empty() {
                                "None".into()
                            } else {
                                tab.pull_request.labels.join(", ")
                            },
                            colors,
                        ),
                    ];
                    if let Some(details) = &tab.details {
                        fields.push(detail(
                            "Requested reviewers",
                            if details.requested_reviewers.is_empty() {
                                "None".into()
                            } else {
                                details.requested_reviewers.join(", ")
                            },
                            colors,
                        ));
                        fields.push(detail(
                            "Merge state",
                            format!(
                                "{} · {}",
                                details.merge_eligibility.mergeable,
                                details.merge_eligibility.merge_state_status
                            ),
                            colors,
                        ));
                        if !details.body.trim().is_empty() {
                            fields.push(detail("Description", details.body.clone(), colors));
                        }
                    }
                    div().children(fields).into_any_element()
                }
                InspectorSection::Activity => {
                    let mut activity = Vec::new();
                    if let Some(details) = &tab.details {
                        for comment in details.issue_comments.iter().take(20) {
                            activity.push(activity_item(
                                comment.author.as_deref().unwrap_or("Unknown author"),
                                &comment.body,
                                &comment.created_at,
                                colors,
                            ));
                        }
                        for review in details.reviews.iter().take(20) {
                            activity.push(activity_item(
                                review.author.as_deref().unwrap_or("Unknown reviewer"),
                                if review.body.is_empty() {
                                    &review.state
                                } else {
                                    &review.body
                                },
                                review.submitted_at.as_deref().unwrap_or("Pending"),
                                colors,
                            ));
                        }
                        if !details.activity_complete {
                            activity.push(div().text_color(colors.amber).child(
                                "Activity is incomplete; GitHub response limits were reached.",
                            ));
                        }
                    }
                    if activity.is_empty() {
                        activity.push(
                            div()
                                .text_color(colors.muted)
                                .child("No activity returned for this pull request."),
                        );
                    }
                    div().children(activity).into_any_element()
                }
                InspectorSection::Checks => {
                    let mut checks = vec![detail(
                        "Status",
                        empty_unknown(&tab.pull_request.check_status),
                        colors,
                    )];
                    if let Some(details) = &tab.details {
                        for check in details.checks.iter().take(40) {
                            checks.push(div().mb_3().child(check.name.clone()).child(
                                div().text_xs().text_color(colors.muted).child(format!(
                                            "{}{}",
                                            check.status,
                                            check
                                                .conclusion
                                                .as_ref()
                                                .map(|value| format!(" · {value}"))
                                                .unwrap_or_default()
                                        )),
                            ));
                        }
                        if !details.checks_complete {
                            checks.push(
                                div()
                                    .text_color(colors.amber)
                                    .child("Check results are incomplete."),
                            );
                        }
                    }
                    div().children(checks).into_any_element()
                }
            };
        div()
            .w(px(274.))
            .min_w(px(240.))
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(colors.border)
            .bg(colors.canvas)
            .child(
                div()
                    .h(px(38.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(section("Overview", InspectorSection::Overview))
                    .child(section("Activity", InspectorSection::Activity))
                    .child(section("Checks", InspectorSection::Checks)),
            )
            .when_some(tab.details_state.notice(), |panel, notice| {
                panel.child(
                    div()
                        .px_4()
                        .py_2()
                        .text_xs()
                        .text_color(colors.muted)
                        .child(notice),
                )
            })
            .child(
                div()
                    .id("inspector-scroll")
                    .p_4()
                    .overflow_y_scroll()
                    .child(content),
            )
    }

    fn render_status(&self, colors: Palette) -> impl IntoElement {
        div()
            .h(px(28.))
            .px_4()
            .flex()
            .items_center()
            .gap_3()
            .border_t_1()
            .border_color(colors.border)
            .bg(colors.canvas)
            .text_xs()
            .text_color(colors.muted)
            .child(if self.focused {
                "Online reads · polling active"
            } else {
                "Inactive · polling slowed"
            })
            .child("·")
            .child(self.status.clone())
            .when_some(self.persistence_error.clone(), |bar, error| {
                bar.child("·")
                    .child(div().text_color(colors.red).child(error))
            })
    }

    fn render_palette(&self, colors: Palette, cx: &mut Context<Root>) -> impl IntoElement {
        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .pt_24()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(520.))
                    .p_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.surface)
                    .shadow_lg()
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .text_xs()
                            .text_color(colors.muted)
                            .child("COMMANDS"),
                    )
                    .child(
                        command_row("Refresh repository and active PR", "⌘R", colors)
                            .id("command-refresh")
                            .cursor_pointer()
                            .hover(|row| row.bg(colors.selected))
                            .on_click(cx.listener(|root, _, window, cx| {
                                if let Root::Review(this) = root {
                                    this.refresh(&Refresh, window, cx);
                                    this.command_palette = false;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        command_row("Next changed file", "⌘]", colors)
                            .id("command-next")
                            .cursor_pointer()
                            .hover(|row| row.bg(colors.selected))
                            .on_click(cx.listener(|root, _, window, cx| {
                                if let Root::Review(this) = root {
                                    this.next_file(&NextFile, window, cx);
                                    this.command_palette = false;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        command_row("Previous changed file", "⌘[", colors)
                            .id("command-previous")
                            .cursor_pointer()
                            .hover(|row| row.bg(colors.selected))
                            .on_click(cx.listener(|root, _, window, cx| {
                                if let Root::Review(this) = root {
                                    this.previous_file(&PreviousFile, window, cx);
                                    this.command_palette = false;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        command_row("Cycle diff mode", "⇧⌘D", colors)
                            .id("command-diff")
                            .cursor_pointer()
                            .hover(|row| row.bg(colors.selected))
                            .on_click(cx.listener(|root, _, window, cx| {
                                if let Root::Review(this) = root {
                                    this.cycle_diff(&CycleDiffMode, window, cx);
                                    this.command_palette = false;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        command_row("Toggle PR details", "⇧⌘I", colors)
                            .id("command-details")
                            .cursor_pointer()
                            .hover(|row| row.bg(colors.selected))
                            .on_click(cx.listener(|root, _, _, cx| {
                                if let Root::Review(this) = root {
                                    this.inspector_open = !this.inspector_open;
                                    this.command_palette = false;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        command_row("Add repository", "⌘O", colors)
                            .id("command-add")
                            .cursor_pointer()
                            .hover(|row| row.bg(colors.selected))
                            .on_click(cx.listener(|root, _, _, cx| {
                                if let Root::Review(this) = root {
                                    this.setup_open = true;
                                    this.command_palette = false;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        div()
                            .id("close-palette")
                            .mt_2()
                            .px_3()
                            .py_2()
                            .text_color(colors.accent)
                            .cursor_pointer()
                            .child("Close palette  ⇧⌘P")
                            .on_click(cx.listener(|root, _, _, cx| {
                                if let Root::Review(this) = root {
                                    this.command_palette = false;
                                    cx.notify();
                                }
                            })),
                    ),
            )
    }
}

fn field_label(label: &str, colors: Palette) -> Div {
    div()
        .text_xs()
        .text_color(colors.muted)
        .child(label.to_owned())
}

fn input_box(editor: &Entity<InputState>, colors: Palette) -> Div {
    div()
        .mt_2()
        .h(px(36.))
        .px_2()
        .rounded_md()
        .bg(colors.elevated)
        .border_1()
        .border_color(colors.border)
        .font_family(UI_FONT)
        .child(Input::new(editor))
}

fn file_status_badge(status: &str) -> &'static str {
    match status.to_ascii_lowercase().as_str() {
        "added" | "a" => "A",
        "removed" | "deleted" | "d" => "D",
        "renamed" | "r" => "R",
        "copied" | "c" => "C",
        "changed" | "modified" | "m" => "M",
        _ => "·",
    }
}

fn side_control(label: &str, selected: bool, colors: Palette) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_xs()
        .when(selected, |item| {
            item.bg(colors.selected).text_color(colors.text)
        })
        .when(!selected, |item| item.text_color(colors.muted))
        .child(label.to_owned())
}

fn command_row(label: &str, shortcut: &str, colors: Palette) -> Div {
    div()
        .px_3()
        .py_2()
        .flex()
        .justify_between()
        .rounded_md()
        .child(label.to_owned())
        .child(div().text_color(colors.muted).child(shortcut.to_owned()))
}

fn detail(label: &str, value: impl Into<String>, colors: Palette) -> Div {
    div()
        .mb_4()
        .child(
            div()
                .text_xs()
                .text_color(colors.muted)
                .child(label.to_owned()),
        )
        .child(div().mt_1().child(value.into()))
}

fn activity_item(author: &str, body: &str, timestamp: &str, colors: Palette) -> Div {
    div()
        .mb_4()
        .child(
            div()
                .flex()
                .justify_between()
                .text_xs()
                .child(author.to_owned())
                .child(
                    div()
                        .ml_2()
                        .text_color(colors.faint)
                        .child(timestamp.to_owned()),
                ),
        )
        .child(div().mt_1().text_color(colors.muted).child(body.to_owned()))
}

fn empty_unknown(value: &str) -> String {
    if value.is_empty() {
        "UNKNOWN".into()
    } else {
        value.into()
    }
}

fn group_label(group: Option<&GroupBy>) -> &'static str {
    match group {
        Some(GroupBy::Repository) => "Repository",
        Some(GroupBy::TargetBranch) => "Target branch",
        Some(GroupBy::SourceBranch) => "Source branch",
        Some(GroupBy::SourcePrefix(_)) => "Source prefix",
        Some(GroupBy::Stack) => "Stack",
        None => "None",
    }
}

fn build_rows(diff: ParsedDiff, mode: DiffMode) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    for hunk in diff.hunks {
        rows.push(DiffRow::Hunk(hunk.header.clone()));
        match mode {
            DiffMode::SideBySide => {
                rows.extend(hunk.aligned_rows().into_iter().map(DiffRow::Split))
            }
            _ => rows.extend(hunk.lines.into_iter().map(DiffRow::Unified)),
        }
    }
    match diff.status {
        PatchStatus::Complete => {}
        PatchStatus::Truncated { reason } | PatchStatus::Unsupported { reason } => {
            rows.push(DiffRow::Hunk(format!("Notice: {reason}")))
        }
    }
    rows
}

fn render_diff_row(row: &DiffRow, colors: Palette) -> AnyElement {
    match row {
        DiffRow::Hunk(header) => div()
            .h(px(26.))
            .px_3()
            .flex()
            .items_center()
            .bg(colors.elevated)
            .text_color(colors.accent)
            .font_family(CODE_FONT)
            .text_xs()
            .child(header.clone())
            .into_any_element(),
        DiffRow::Unified(line) => {
            let (background, foreground, marker) = line_colors(line.kind, colors);
            div()
                .h(px(24.))
                .flex()
                .items_center()
                .bg(background)
                .font_family(CODE_FONT)
                .text_xs()
                .child(line_number(line.old_line, colors))
                .child(line_number(line.new_line, colors))
                .child(div().w(px(18.)).text_color(foreground).child(marker))
                .child(
                    div()
                        .flex_1()
                        .whitespace_nowrap()
                        .text_color(foreground)
                        .child(line.text.clone()),
                )
                .into_any_element()
        }
        DiffRow::Split(row) => div()
            .h(px(24.))
            .flex()
            .font_family(CODE_FONT)
            .text_xs()
            .child(split_cell(row.old.as_ref(), true, colors))
            .child(split_cell(row.new.as_ref(), false, colors))
            .into_any_element(),
    }
}

fn line_number(number: Option<u64>, colors: Palette) -> Div {
    div()
        .w(px(48.))
        .px_2()
        .text_right()
        .text_color(colors.faint)
        .child(number.map(|number| number.to_string()).unwrap_or_default())
}

fn line_colors(kind: DiffLineKind, colors: Palette) -> (Rgba, Rgba, &'static str) {
    match kind {
        DiffLineKind::Addition => (
            if colors.dark {
                rgba(0x14382588)
            } else {
                rgba(0xdff3e7ff)
            },
            colors.green,
            "+",
        ),
        DiffLineKind::Deletion => (
            if colors.dark {
                rgba(0x411f2188)
            } else {
                rgba(0xf9e2e0ff)
            },
            colors.red,
            "−",
        ),
        DiffLineKind::Context => (colors.surface, colors.text, " "),
        DiffLineKind::NoNewline => (colors.elevated, colors.muted, "↳"),
    }
}

fn split_cell(line: Option<&DiffLine>, old: bool, colors: Palette) -> Div {
    let Some(line) = line else {
        return div()
            .w_1_2()
            .h_full()
            .bg(colors.elevated)
            .border_r_1()
            .border_color(colors.border);
    };
    let (background, foreground, marker) = line_colors(line.kind, colors);
    let number = if old { line.old_line } else { line.new_line };
    div()
        .w_1_2()
        .h_full()
        .flex()
        .items_center()
        .bg(background)
        .border_r_1()
        .border_color(colors.border)
        .child(line_number(number, colors))
        .child(div().w(px(18.)).text_color(foreground).child(marker))
        .child(
            div()
                .flex_1()
                .whitespace_nowrap()
                .overflow_hidden()
                .text_color(foreground)
                .child(line.text.clone()),
        )
}

pub struct EditorWorkspace {
    editor: Entity<EditorState>,
    path: PathBuf,
    disk_base: Option<String>,
    message: String,
}

impl EditorWorkspace {
    fn new(window: &mut Window, cx: &mut Context<Root>, path: PathBuf) -> Self {
        let loaded = std::fs::read_to_string(&path);
        let (disk_base, text, message) = match loaded {
            Ok(text) => (Some(text.clone()), text, "Local editor · ⌘S to save".into()),
            Err(error) => (None, String::new(), format!("Cannot open file: {error}")),
        };
        let colors = palette(is_dark(window));
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx);
            state.set_editor_style(InputEditorStyle {
                foreground: colors.text.into(),
                muted_foreground: colors.muted.into(),
                background: colors.canvas.into(),
                editor_gutter_background: Some(colors.canvas.into()),
                ..Default::default()
            });
            state.set_value(text, window, cx);
            state.focus(window, cx);
            state
        });
        Self {
            editor,
            path,
            disk_base,
            message,
        }
    }

    fn save(&mut self, cx: &mut Context<Root>) {
        let Some(base) = &self.disk_base else {
            self.message = "File is unavailable; buffer preserved".into();
            cx.notify();
            return;
        };
        let text = self.editor.read(cx).value().to_string();
        self.message = match std::fs::read_to_string(&self.path) {
            Ok(current) if current == *base => match std::fs::write(&self.path, &text) {
                Ok(()) => {
                    self.disk_base = Some(text);
                    "Saved".into()
                }
                Err(error) => format!("Save failed: {error}"),
            },
            Ok(_) => {
                "File changed on disk; unsaved text preserved. Reconciliation is pending.".into()
            }
            Err(error) => format!("Cannot verify disk contents: {error}"),
        };
        cx.notify();
    }

    fn render(&mut self, window: &mut Window, cx: &mut Context<Root>) -> impl IntoElement {
        let colors = palette(is_dark(window));
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.canvas)
            .font_family(UI_FONT)
            .text_color(colors.text)
            .on_action(cx.listener(|root, _: &Save, _, cx| {
                if let Root::Editor(this) = root {
                    this.save(cx)
                }
            }))
            .child(
                div()
                    .h(px(52.))
                    .px_5()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("cibergit · Local editor"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(colors.muted)
                            .child(self.path.display().to_string()),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px_5()
                    .font_family(CODE_FONT)
                    .child(Editor::new(&self.editor)),
            )
            .child(
                div()
                    .h(px(36.))
                    .px_5()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(colors.border)
                    .text_sm()
                    .child(self.message.clone())
                    .child("Save  ⌘S"),
            )
    }
}
