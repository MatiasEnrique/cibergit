use crate::{
    CloseTab, CycleDiffMode, DetailsNarrower, DetailsWider, DiffScrollEnd, DiffScrollHome,
    DiffScrollLeft, DiffScrollRight, FileTreeActivate, FileTreeDown, FileTreeLeft,
    FileTreeNarrower, FileTreeRight, FileTreeUp, FileTreeWider, NextFile, OpenRepositorySetup,
    PreviousFile, Refresh, ResetLayout, Save, SidebarNarrower, SidebarWider, ToggleFileTree,
    ToggleInspector, TogglePalette, ToggleSidebar,
};
mod file_tree;
mod view_editor;

use cibergit::{
    domain::{PullRequest, PullRequestDetails, Repository, Revision},
    providers::GithubProvider,
    review::{
        AlignedRow, DiffLine, DiffLineKind, DiffMode, ParsedDiff, PatchStatus, ReviewSession,
        file_key, load_local_file, local_pr_inventory, parse_file,
    },
    workspace::{Filter, GroupBy, PersonalFilter, PollSchedule, Store, TabState, WorkspaceState},
};
use file_tree::{FileTree, TreeRowKind};
use gpui::{prelude::*, *};
use gpui_base::{
    Scrollbar, TextView, TextViewStyle,
    input::{Editor, EditorState, Input, InputEditorStyle, InputState},
};
use std::{
    cell::Cell,
    collections::HashMap,
    ops::Range,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use view_editor::{RepositoryPulls, SidebarRow, ViewEditorController, compose_sidebar_rows};

const UI_FONT: &str = "IBM Plex Sans";
const CODE_FONT: &str = "Menlo";
const DEFAULT_SIDEBAR_WIDTH: f32 = 292.;
const DEFAULT_FILE_TREE_WIDTH: f32 = 250.;
const DEFAULT_DETAILS_WIDTH: f32 = 274.;
const MIN_SIDEBAR_WIDTH: f32 = 220.;
const MIN_FILE_TREE_WIDTH: f32 = 180.;
const MIN_DETAILS_WIDTH: f32 = 220.;
const MAX_PANEL_WIDTH: f32 = 460.;
const COLLAPSED_PANEL_WIDTH: f32 = 34.;
const SPLITTER_WIDTH: f32 = 6.;
const MIN_SPLIT_DIFF_WIDTH: f32 = 560.;
const PANEL_KEYBOARD_STEP: f32 = 16.;
// Menlo at the diff's 12px text size advances about 7.225px per ASCII cell on
// the pinned renderer. Round upward; Unicode is conservatively two cells.
const DIFF_CELL_WIDTH: f32 = 7.23;
const DIFF_FIXED_COLUMNS: f32 = 122.;
#[cfg(feature = "ui-smoke")]
const SPLIT_GUTTER_WIDTH: f32 = 66.;
const EXCEPTIONAL_LINE_CHUNK_BYTES: usize = 2_048;
#[cfg(feature = "ui-smoke")]
const SMOKE_LONG_LINE_TOKEN: &str = "CIBERGIT_LONG_LINE_END_7F3A";
#[cfg(feature = "ui-smoke")]
const SMOKE_OLD_LINE_START: &str = "CIBERGIT_OLD_START_2A6D";
#[cfg(feature = "ui-smoke")]
const SMOKE_OLD_LINE_END: &str = "CIBERGIT_OLD_END_9C41";
#[cfg(feature = "ui-smoke")]
const SMOKE_NEW_LINE_START: &str = "CIBERGIT_NEW_START_51B8";
#[cfg(feature = "ui-smoke")]
const SMOKE_NEW_LINE_END: &str = "CIBERGIT_NEW_END_E73F";

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
    Review(Box<ReviewWorkspace>),
    Editor(EditorWorkspace),
}

impl Root {
    pub fn review(window: &mut Window, cx: &mut Context<Self>, startup: Startup) -> Self {
        Self::Review(Box::new(ReviewWorkspace::new(window, cx, startup)))
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
            muted: rgba(0xb8bbc1ff),
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
            sidebar: rgba(0xf8f8f7b5),
            elevated: rgba(0xf2f2f0ff),
            text: rgba(0x202124ff),
            muted: rgba(0x56595eff),
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

struct ViewEditorInputs {
    name: Entity<InputState>,
    search: Entity<InputState>,
    author: Entity<InputState>,
    reviewer: Entity<InputState>,
    assignee: Entity<InputState>,
    label: Entity<InputState>,
    review_status: Entity<InputState>,
    check_status: Entity<InputState>,
    target_branch: Entity<InputState>,
    source_branch: Entity<InputState>,
    source_prefix: Entity<InputState>,
}

impl ViewEditorInputs {
    fn new(
        view: &cibergit::workspace::SavedView,
        window: &mut Window,
        cx: &mut Context<Root>,
    ) -> Self {
        let prefix = view
            .groups
            .iter()
            .find_map(|group| match group {
                GroupBy::SourcePrefix(prefix) => Some(prefix.clone()),
                _ => None,
            })
            .unwrap_or_default();
        Self {
            name: new_input(&view.name, "View name", window, cx),
            search: new_input(&view.filter.search, "Title, number, or branch", window, cx),
            author: new_input(&view.filter.author, "Author login", window, cx),
            reviewer: new_input(&view.filter.reviewer, "Requested reviewer", window, cx),
            assignee: new_input(&view.filter.assignee, "Assignee", window, cx),
            label: new_input(&view.filter.label, "Label", window, cx),
            review_status: new_input(&view.filter.review_status, "e.g. approved", window, cx),
            check_status: new_input(&view.filter.check_status, "e.g. passing", window, cx),
            target_branch: new_input(&view.filter.target_branch, "Target branch", window, cx),
            source_branch: new_input(
                &view.filter.source_branch,
                "Exact source branch",
                window,
                cx,
            ),
            source_prefix: new_input(prefix, "e.g. feature/", window, cx),
        }
    }

    fn all(&self) -> [&Entity<InputState>; 11] {
        [
            &self.name,
            &self.search,
            &self.author,
            &self.reviewer,
            &self.assignee,
            &self.label,
            &self.review_status,
            &self.check_status,
            &self.target_branch,
            &self.source_branch,
            &self.source_prefix,
        ]
    }

    fn set_view(
        &self,
        view: &cibergit::workspace::SavedView,
        window: &mut Window,
        cx: &mut Context<Root>,
    ) {
        let prefix = view
            .groups
            .iter()
            .find_map(|group| match group {
                GroupBy::SourcePrefix(prefix) => Some(prefix.as_str()),
                _ => None,
            })
            .unwrap_or_default();
        for (editor, value) in [
            (&self.name, view.name.as_str()),
            (&self.search, view.filter.search.as_str()),
            (&self.author, view.filter.author.as_str()),
            (&self.reviewer, view.filter.reviewer.as_str()),
            (&self.assignee, view.filter.assignee.as_str()),
            (&self.label, view.filter.label.as_str()),
            (&self.review_status, view.filter.review_status.as_str()),
            (&self.check_status, view.filter.check_status.as_str()),
            (&self.target_branch, view.filter.target_branch.as_str()),
            (&self.source_branch, view.filter.source_branch.as_str()),
            (&self.source_prefix, prefix),
        ] {
            editor.update(cx, |editor, cx| editor.set_value(value, window, cx));
        }
    }

    fn value(editor: &Entity<InputState>, cx: &Context<Root>) -> String {
        editor.read(cx).value().trim().to_owned()
    }

    fn filter(&self, base: &Filter, cx: &Context<Root>) -> Filter {
        Filter {
            search: Self::value(&self.search, cx),
            author: Self::value(&self.author, cx),
            reviewer: Self::value(&self.reviewer, cx),
            assignee: Self::value(&self.assignee, cx),
            label: Self::value(&self.label, cx),
            draft: base.draft,
            review_status: Self::value(&self.review_status, cx),
            check_status: Self::value(&self.check_status, cx),
            target_branch: Self::value(&self.target_branch, cx),
            source_branch: Self::value(&self.source_branch, cx),
            personal: base.personal.clone(),
            state: base.state.clone(),
        }
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanelKind {
    Sidebar,
    FileTree,
    Details,
}

#[derive(Clone, Debug)]
struct PanelLayout {
    sidebar_width: f32,
    file_tree_width: f32,
    details_width: f32,
    sidebar_collapsed: bool,
    file_tree_collapsed: bool,
}

impl Default for PanelLayout {
    fn default() -> Self {
        Self {
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            file_tree_width: DEFAULT_FILE_TREE_WIDTH,
            details_width: DEFAULT_DETAILS_WIDTH,
            sidebar_collapsed: false,
            file_tree_collapsed: false,
        }
    }
}

impl PanelLayout {
    fn width(&self, panel: PanelKind, inspector_open: bool) -> f32 {
        match panel {
            PanelKind::Sidebar if self.sidebar_collapsed => COLLAPSED_PANEL_WIDTH,
            PanelKind::Sidebar => self.sidebar_width,
            PanelKind::FileTree if self.file_tree_collapsed => COLLAPSED_PANEL_WIDTH,
            PanelKind::FileTree => self.file_tree_width,
            PanelKind::Details if !inspector_open => 0.,
            PanelKind::Details => self.details_width,
        }
    }

    fn adjust(&mut self, panel: PanelKind, delta: f32) {
        let (width, collapsed, minimum) = match panel {
            PanelKind::Sidebar => (
                &mut self.sidebar_width,
                &mut self.sidebar_collapsed,
                MIN_SIDEBAR_WIDTH,
            ),
            PanelKind::FileTree => (
                &mut self.file_tree_width,
                &mut self.file_tree_collapsed,
                MIN_FILE_TREE_WIDTH,
            ),
            PanelKind::Details => {
                self.details_width =
                    (self.details_width + delta).clamp(MIN_DETAILS_WIDTH, MAX_PANEL_WIDTH);
                return;
            }
        };
        if *collapsed && delta > 0. {
            *collapsed = false;
        }
        *width = (*width + delta).clamp(minimum, MAX_PANEL_WIDTH);
    }
}

fn resolved_panel_widths_for(
    layout: &PanelLayout,
    inspector_open: bool,
    window_width: f32,
) -> (f32, f32, f32) {
    let mut sidebar = layout.width(PanelKind::Sidebar, inspector_open);
    let mut tree = layout.width(PanelKind::FileTree, inspector_open);
    let mut details = layout.width(PanelKind::Details, inspector_open);
    let splitter_count = 2. + if inspector_open { 1. } else { 0. };
    let panel_budget = (window_width - 360. - splitter_count * SPLITTER_WIDTH).max(0.);
    let minimum_sidebar = if layout.sidebar_collapsed {
        COLLAPSED_PANEL_WIDTH
    } else {
        MIN_SIDEBAR_WIDTH
    };
    let minimum_tree = if layout.file_tree_collapsed {
        COLLAPSED_PANEL_WIDTH
    } else {
        MIN_FILE_TREE_WIDTH
    };
    let minimum_details = if inspector_open {
        MIN_DETAILS_WIDTH
    } else {
        0.
    };
    let mut excess = (sidebar + tree + details - panel_budget).max(0.);
    for (width, minimum) in [
        (&mut details, minimum_details),
        (&mut sidebar, minimum_sidebar),
        (&mut tree, minimum_tree),
    ] {
        let reduction = excess.min((*width - minimum).max(0.));
        *width -= reduction;
        excess -= reduction;
    }
    (sidebar, tree, details)
}

fn available_diff_width_for(layout: &PanelLayout, inspector_open: bool, window_width: f32) -> f32 {
    let (sidebar, tree, details) = resolved_panel_widths_for(layout, inspector_open, window_width);
    let splitter_count = 2. + if inspector_open { 1. } else { 0. };
    (window_width - sidebar - tree - details - splitter_count * SPLITTER_WIDTH).max(0.)
}

#[derive(Clone)]
struct PanelResizeDrag {
    panel: PanelKind,
    start_width: f32,
    start_x: Rc<Cell<Option<f32>>>,
}

struct SplitterDragPreview;

impl Render for SplitterDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w(px(2.)).h(px(24.)).bg(rgba(0x6fa8ffff))
    }
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
    diff_horizontal: ScrollHandle,
    horizontal_positions: HashMap<String, f32>,
    diff_content_width: f32,
    file_tree: FileTree,
    file_tree_scroll: UniformListScrollHandle,
    inspector_section: InspectorSection,
    local_inventory: bool,
    session_persistence_error: Option<String>,
    details: Option<PullRequestDetails>,
    details_state: LoadState,
    details_generation: u64,
}

#[cfg(feature = "ui-smoke")]
struct SmokeExpectation {
    repository: Repository,
    number: u64,
    file_key: String,
    viewed: bool,
}

#[cfg(feature = "ui-smoke")]
struct SmokeActions {
    primary_number: u64,
    report: String,
    expectations: Vec<SmokeExpectation>,
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
    file_tree_focus: FocusHandle,
    diff_focus: FocusHandle,
    panel_layout: PanelLayout,
    view_editor_scroll: ScrollHandle,
    query: Entity<InputState>,
    view_editor: ViewEditorController,
    view_inputs: ViewEditorInputs,
    repository_input: Entity<InputState>,
    pr_input: Entity<InputState>,
    selected_account: usize,
    status: String,
    schedule: PollSchedule,
    startup_pr: Option<u64>,
    session_save_latest: HashMap<String, Arc<AtomicU64>>,
    session_save_locks: HashMap<String, Arc<Mutex<()>>>,
    _subscriptions: Vec<Subscription>,
}

impl ReviewWorkspace {
    fn resolved_panel_widths(&self, window: &Window) -> (f32, f32, f32) {
        resolved_panel_widths_for(
            &self.panel_layout,
            self.inspector_open,
            window.bounds().size.width.as_f32(),
        )
    }

    fn available_diff_width(&self, window: &Window) -> f32 {
        available_diff_width_for(
            &self.panel_layout,
            self.inspector_open,
            window.bounds().size.width.as_f32(),
        )
    }

    fn refresh_auto_layout(&mut self, window: &Window) {
        let wide = self.available_diff_width(window) >= MIN_SPLIT_DIFF_WIDTH;
        if wide == self.wide {
            return;
        }
        self.wide = wide;
        if let Some(index) = self.active_tab
            && self.tabs[index]
                .session
                .as_ref()
                .is_some_and(|session| session.diff_mode() == DiffMode::Auto)
        {
            self.capture_scroll(index);
            self.rebuild_diff(index, wide);
        }
    }

    fn reset_layout(&mut self, window: &Window, cx: &mut Context<Root>) {
        self.panel_layout = PanelLayout::default();
        self.inspector_open = true;
        self.refresh_auto_layout(window);
        self.status = "Panel layout reset".into();
        cx.notify();
    }

    fn adjust_panel(
        &mut self,
        panel: PanelKind,
        delta: f32,
        window: &Window,
        cx: &mut Context<Root>,
    ) {
        if panel == PanelKind::Details && !self.inspector_open {
            self.inspector_open = true;
        }
        self.panel_layout.adjust(panel, delta);
        self.refresh_auto_layout(window);
        cx.notify();
    }

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
        let view_inputs = ViewEditorInputs::new(&workspace.view(), window, cx);
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
        let file_tree_focus = cx.focus_handle();
        let diff_focus = cx.focus_handle();
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
            wide: false,
            focus,
            file_tree_focus,
            diff_focus,
            panel_layout: PanelLayout::default(),
            view_editor_scroll: ScrollHandle::new(),
            query,
            view_editor: ViewEditorController::new(),
            view_inputs,
            repository_input,
            pr_input,
            selected_account: 0,
            status: "Read-only review workspace".into(),
            schedule: PollSchedule::default(),
            startup_pr: startup.pull_request,
            session_save_latest: HashMap::new(),
            session_save_locks: HashMap::new(),
            _subscriptions: Vec::new(),
        };
        this.wide = this.available_diff_width(window) >= MIN_SPLIT_DIFF_WIDTH;
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
            for editor in [&this.query, &this.repository_input, &this.pr_input]
                .into_iter()
                .chain(this.view_inputs.all())
            {
                editor.update(cx, |editor, _| editor.set_editor_style(style.clone()));
            }
            cx.notify();
        });
        let bounds = cx.observe_window_bounds(window, |root, window, cx| {
            let Root::Review(this) = root else { return };
            let wide = this.available_diff_width(window) >= MIN_SPLIT_DIFF_WIDTH;
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
                        let active_due = this.active_tab.is_some_and(|index| {
                            let tab = &this.tabs[index];
                            let key = format!(
                                "pr:{}:{}",
                                tab.repository.cache_key(),
                                tab.pull_request.number
                            );
                            poll_due(tick, this.schedule.delay(&key, true, this.focused))
                        });
                        if active_due {
                            this.refresh_active(cx);
                        }
                        let repositories = this
                            .repositories
                            .iter()
                            .enumerate()
                            .filter_map(|(index, runtime)| {
                                let key = format!("sidebar:{}", runtime.repository.cache_key());
                                poll_due(tick, this.schedule.delay(&key, false, this.focused))
                                    .then_some(index)
                            })
                            .collect::<Vec<_>>();
                        for index in repositories {
                            this.refresh_repository(index, cx);
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
        let second_pr = std::env::var("CIBERGIT_SMOKE_SECOND_PR")
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        let expect_restore = std::env::var_os("CIBERGIT_SMOKE_EXPECT_RESTORE").is_some();
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
                                matches!(root, Root::Review(this) if this.smoke_ready())
                            })
                            .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    if ready || started.elapsed() > Duration::from_secs(90) {
                        break;
                    }
                }
                let _ = std::fs::create_dir_all(&output);
                let split_review_captured = if expect_restore {
                    true
                } else {
                    let _ = window.update(|window, cx| {
                        let _ = weak.update(cx, |root, cx| {
                            if let Root::Review(this) = root {
                                this.panel_layout.sidebar_collapsed = true;
                                this.panel_layout.file_tree_collapsed = true;
                                this.inspector_open = false;
                                this.refresh_auto_layout(window);
                                cx.notify();
                            }
                        });
                    });
                    window
                        .background_executor()
                        .timer(Duration::from_millis(300))
                        .await;
                    let captured =
                    window
                        .update(|window, cx| {
                            let split = weak
                                .read_with(cx, |root, _| {
                                    matches!(root, Root::Review(this) if this.active_tab.is_some_and(|index| this.tabs[index].diff_rows.iter().any(|row| matches!(row, DiffRow::Split(_)))))
                                })
                                .unwrap_or(false);
                            split
                                && window
                                    .render_to_image()
                                    .and_then(|image| {
                                        image
                                            .save(output.join("native-pr-review-split.png"))
                                            .map_err(Into::into)
                                    })
                                    .is_ok()
                        })
                        .unwrap_or(false);
                    let _ = window.update(|window, cx| {
                        let _ = weak.update(cx, |root, cx| {
                            if let Root::Review(this) = root {
                                this.panel_layout = PanelLayout::default();
                                this.inspector_open = true;
                                this.refresh_auto_layout(window);
                                cx.notify();
                            }
                        });
                    });
                    window
                        .background_executor()
                        .timer(Duration::from_millis(300))
                        .await;
                    captured
                };
                let auto_layout_verified = if expect_restore {
                    true
                } else {
                    let _ = window.update(|window, _| {
                        window.resize(size(px(1040.), px(720.)));
                    });
                    window
                        .background_executor()
                        .timer(Duration::from_millis(300))
                        .await;
                    let narrow = window
                        .update(|_, cx| {
                            weak.read_with(cx, |root, _| {
                                matches!(root, Root::Review(this) if this.active_tab.is_some_and(|index| {
                                    !this.wide
                                        && this.tabs[index].diff_horizontal.bounds().size.width
                                            < px(MIN_SPLIT_DIFF_WIDTH)
                                        && this.tabs[index].session.as_ref().is_some_and(|session| session.diff_mode() == DiffMode::Auto)
                                        && this.tabs[index].diff_rows.iter().all(|row| !matches!(row, DiffRow::Split(_)))
                                }))
                            })
                            .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    let _ = window.update(|window, _| {
                        window.resize(size(px(1440.), px(900.)));
                    });
                    window
                        .background_executor()
                        .timer(Duration::from_millis(300))
                        .await;
                    let wide = window
                        .update(|_, cx| {
                            weak.read_with(cx, |root, _| {
                                matches!(root, Root::Review(this) if this.active_tab.is_some_and(|index| {
                                    this.wide
                                        && effective_diff_viewport_width(&this.tabs[index].diff_rows, &this.tabs[index].diff_horizontal)
                                            >= MIN_SPLIT_DIFF_WIDTH
                                        && this.tabs[index].session.as_ref().is_some_and(|session| session.diff_mode() == DiffMode::Auto)
                                        && this.tabs[index].diff_rows.iter().any(|row| matches!(row, DiffRow::Split(_)))
                                }))
                            })
                            .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    narrow && wide
                };
                let actions = window
                    .update(|window, cx| {
                        weak.update(cx, |root, cx| {
                            let Root::Review(this) = root else {
                                return Err("smoke started outside review workspace".to_owned());
                            };
                            this.run_primary_smoke_actions(
                                second_pr,
                                expect_restore,
                                auto_layout_verified,
                                window,
                                cx,
                            )
                        })
                        .unwrap_or_else(|error| Err(format!("smoke entity unavailable: {error:#}")))
                    })
                    .unwrap_or_else(|error| Err(format!("smoke window unavailable: {error:#}")));

                if actions.is_ok() && second_pr.is_some() {
                    let second_started = std::time::Instant::now();
                    loop {
                        window
                            .background_executor()
                            .timer(Duration::from_millis(250))
                            .await;
                        let ready = window
                            .update(|_, cx| {
                                weak.read_with(cx, |root, _| {
                                    matches!(root, Root::Review(this) if this.tabs.len() >= 2 && this.smoke_ready())
                                })
                                .unwrap_or(false)
                            })
                            .unwrap_or(false);
                        if ready || second_started.elapsed() > Duration::from_secs(90) {
                            break;
                        }
                    }
                }

                let actions = match actions {
                    Ok(mut actions) => window
                        .update(|window, cx| {
                            weak.update(cx, |root, cx| {
                                let Root::Review(this) = root else {
                                    return Err("smoke left review workspace".to_owned());
                                };
                                this.run_tab_smoke_actions(
                                    second_pr,
                                    &mut actions,
                                    window,
                                    cx,
                                )?;
                                Ok(actions)
                            })
                            .unwrap_or_else(|error| {
                                Err(format!("smoke entity unavailable: {error:#}"))
                            })
                        })
                        .unwrap_or_else(|error| Err(format!("smoke window unavailable: {error:#}"))),
                    Err(error) => Err(error),
                };

                window
                    .background_executor()
                    .timer(Duration::from_millis(750))
                    .await;
                let review_captured = window
                    .update(|window, _| {
                        window
                            .render_to_image()
                            .and_then(|image| {
                                image
                                    .save(output.join("native-pr-review.png"))
                                    .map_err(Into::into)
                            })
                            .is_ok()
                    })
                    .unwrap_or(false);
                let long_line_installed = actions.is_ok()
                    && window
                        .update(|_, cx| {
                            weak.update(cx, |root, cx| {
                                let Root::Review(this) = root else {
                                    return Err("smoke left review workspace".to_owned());
                                };
                                this.install_long_line_smoke(DiffMode::Unified, cx)
                            })
                            .unwrap_or_else(|error| {
                                Err(format!("smoke entity unavailable: {error:#}"))
                            })
                        })
                        .unwrap_or_else(|_| Err("smoke window unavailable".to_owned()))
                        .is_ok();
                window
                    .background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let long_line_maximum = if long_line_installed {
                    window
                        .update(|_, cx| {
                            weak.update(cx, |root, cx| {
                                let Root::Review(this) = root else {
                                    return Err("smoke left review workspace".to_owned());
                                };
                                this.scroll_long_line_smoke_to_end(cx)
                            })
                            .unwrap_or_else(|error| {
                                Err(format!("smoke entity unavailable: {error:#}"))
                            })
                        })
                        .unwrap_or_else(|_| Err("smoke window unavailable".to_owned()))
                } else {
                    Err("long-line fixture was not installed".to_owned())
                };
                window
                    .background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let long_line_verified = long_line_maximum.as_ref().is_ok_and(|maximum| {
                    window
                        .update(|_, cx| {
                            weak.read_with(cx, |root, _| {
                                let Root::Review(this) = root else {
                                    return Err("smoke left review workspace".to_owned());
                                };
                                this.validate_long_line_smoke(*maximum, DiffMode::Unified)
                            })
                            .unwrap_or_else(|error| {
                                Err(format!("smoke entity unavailable: {error:#}"))
                            })
                        })
                        .unwrap_or_else(|_| Err("smoke window unavailable".to_owned()))
                        .is_ok()
                });
                let long_line_captured = long_line_verified
                    && window
                        .update(|window, _| {
                            window
                                .render_to_image()
                                .and_then(|image| {
                                    image
                                        .save(output.join("native-long-line-end.png"))
                                        .map_err(Into::into)
                                })
                                .is_ok()
                        })
                        .unwrap_or(false);
                let split_long_line_installed = actions.is_ok()
                    && window
                        .update(|_, cx| {
                            weak.update(cx, |root, cx| {
                                let Root::Review(this) = root else {
                                    return Err("smoke left review workspace".to_owned());
                                };
                                this.install_long_line_smoke(DiffMode::SideBySide, cx)
                            })
                            .unwrap_or_else(|error| {
                                Err(format!("smoke entity unavailable: {error:#}"))
                            })
                        })
                        .unwrap_or_else(|_| Err("smoke window unavailable".to_owned()))
                        .is_ok();
                window
                    .background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let split_long_line_start_verified = split_long_line_installed
                    && window
                        .update(|_, cx| {
                            weak.read_with(cx, |root, _| {
                                let Root::Review(this) = root else {
                                    return Err("smoke left review workspace".to_owned());
                                };
                                this.validate_split_long_line_start()
                            })
                            .unwrap_or_else(|error| {
                                Err(format!("smoke entity unavailable: {error:#}"))
                            })
                        })
                        .unwrap_or_else(|_| Err("smoke window unavailable".to_owned()))
                        .is_ok();
                let split_long_line_start_captured = split_long_line_start_verified
                    && window
                        .update(|window, _| {
                            window
                                .render_to_image()
                                .and_then(|image| {
                                    image
                                        .save(output.join("native-long-line-start-split.png"))
                                        .map_err(Into::into)
                                })
                                .is_ok()
                        })
                        .unwrap_or(false);
                let split_long_line_maximum = if split_long_line_installed {
                    window
                        .update(|_, cx| {
                            weak.update(cx, |root, cx| {
                                let Root::Review(this) = root else {
                                    return Err("smoke left review workspace".to_owned());
                                };
                                this.scroll_long_line_smoke_to_end(cx)
                            })
                            .unwrap_or_else(|error| {
                                Err(format!("smoke entity unavailable: {error:#}"))
                            })
                        })
                        .unwrap_or_else(|_| Err("smoke window unavailable".to_owned()))
                } else {
                    Err("split long-line fixture was not installed".to_owned())
                };
                window
                    .background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let split_long_line_verified =
                    split_long_line_maximum.as_ref().is_ok_and(|maximum| {
                        window
                            .update(|_, cx| {
                                weak.read_with(cx, |root, _| {
                                    let Root::Review(this) = root else {
                                        return Err("smoke left review workspace".to_owned());
                                    };
                                    this.validate_long_line_smoke(*maximum, DiffMode::SideBySide)
                                })
                                .unwrap_or_else(|error| {
                                    Err(format!("smoke entity unavailable: {error:#}"))
                                })
                            })
                            .unwrap_or_else(|_| Err("smoke window unavailable".to_owned()))
                            .is_ok()
                    });
                let split_long_line_captured = split_long_line_verified
                    && window
                        .update(|window, _| {
                            window
                                .render_to_image()
                                .and_then(|image| {
                                    image
                                        .save(output.join("native-long-line-end-split.png"))
                                        .map_err(Into::into)
                                })
                                .is_ok()
                        })
                        .unwrap_or(false);
                let _ = window.update(|_, cx| {
                    let _ = weak.update(cx, |root, cx| {
                        if let Root::Review(this) = root {
                            this.restore_real_diff_after_long_line(cx);
                        }
                    });
                });
                window
                    .background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let editor_opened = window
                    .update(|window, cx| {
                        weak.update(cx, |root, cx| {
                            let Root::Review(this) = root else {
                                return false;
                            };
                            this.open_view_editor(window, cx);
                            true
                        })
                        .unwrap_or(false)
                    })
                    .unwrap_or(false);
                window
                    .background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let filter_editor_captured = editor_opened
                    && window
                        .update(|window, _| {
                            window
                                .render_to_image()
                                .and_then(|image| {
                                    image
                                        .save(output.join("native-view-editor-filters.png"))
                                        .map_err(Into::into)
                                })
                                .is_ok()
                        })
                        .unwrap_or(false);
                let editor_scrolled = window
                    .update(|_, cx| {
                        weak.update(cx, |root, cx| {
                            let Root::Review(this) = root else {
                                return false;
                            };
                            this.view_editor_scroll.scroll_to_bottom();
                            cx.notify();
                            true
                        })
                        .unwrap_or(false)
                    })
                    .unwrap_or(false);
                window
                    .background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let _ = window.update(|window, cx| {
                    let validation = actions.and_then(|mut actions| {
                        weak.update(cx, |root, _| {
                            let Root::Review(this) = root else {
                                return Err("smoke left review workspace".to_owned());
                            };
                            this.validate_smoke_persistence(&actions.expectations)?;
                            actions.report.push_str(
                                "Persistence: selected/viewed state read back after queued two-tab saves\n",
                            );
                            actions.report.push_str(&format!(
                                "Long-line source end: {SMOKE_LONG_LINE_TOKEN}\nUnified horizontal maximum offset: {}px\nUnified far-end native render state: {}\nSplit start sentinels: OLD={SMOKE_OLD_LINE_START}, NEW={SMOKE_NEW_LINE_START}\nSplit start native render state: {}\nSplit end sentinels: OLD={SMOKE_OLD_LINE_END}, NEW={SMOKE_NEW_LINE_END}\nSplit horizontal maximum offset: {}px\nSplit far-end native render state: {}\n",
                                long_line_maximum.as_ref().copied().unwrap_or_default(),
                                if long_line_verified { "passed" } else { "failed" },
                                if split_long_line_start_verified { "passed" } else { "failed" },
                                split_long_line_maximum.as_ref().copied().unwrap_or_default(),
                                if split_long_line_verified { "passed" } else { "failed" },
                            ));
                            Ok(actions)
                        })
                        .unwrap_or_else(|error| {
                            Err(format!("smoke entity unavailable: {error:#}"))
                        })
                    });
                    let group_editor_captured = editor_scrolled
                        && window
                            .render_to_image()
                            .and_then(|image| {
                                image
                                    .save(output.join("native-view-editor-groups.png"))
                                    .map_err(Into::into)
                            })
                            .is_ok();
                    let passed = validation.is_ok()
                        && split_review_captured
                        && review_captured
                        && long_line_captured
                        && split_long_line_start_captured
                        && split_long_line_captured
                        && filter_editor_captured
                        && group_editor_captured;
                    let details = validation
                        .map(|actions| actions.report)
                        .unwrap_or_else(|error| format!("Smoke failed: {error}\n"));
                    let report = format!(
                        "{details}Programmatic native actions: {}\nInitial split review scene capture: {}\nReview scene capture: {}\nUnified long-line end scene capture: {}\nSplit long-line start scene capture: {}\nSplit long-line end scene capture: {}\nFilter editor scene capture: {}\nGrouping editor scene capture: {}\nNative backdrop blending and physical input: not established by in-process capture\nRemote writes: none\n",
                        if passed { "passed" } else { "failed" },
                        if expect_restore {
                            "covered by fresh light run"
                        } else if split_review_captured {
                            "native-pr-review-split.png"
                        } else {
                            "failed"
                        },
                        if review_captured {
                            "native-pr-review.png"
                        } else {
                            "failed"
                        },
                        if long_line_captured {
                            "native-long-line-end.png"
                        } else {
                            "failed"
                        },
                        if split_long_line_start_captured {
                            "native-long-line-start-split.png"
                        } else {
                            "failed"
                        },
                        if split_long_line_captured {
                            "native-long-line-end-split.png"
                        } else {
                            "failed"
                        },
                        if filter_editor_captured {
                            "native-view-editor-filters.png"
                        } else {
                            "failed"
                        },
                        if group_editor_captured {
                            "native-view-editor-groups.png"
                        } else {
                            "failed"
                        }
                    );
                    let _ = std::fs::write(output.join("native-pr-smoke.txt"), report);
                    if !passed {
                        panic!("native UI smoke assertions failed");
                    }
                    cx.quit();
                });
            })
            .detach();
    }

    #[cfg(feature = "ui-smoke")]
    fn smoke_ready(&self) -> bool {
        let repositories_finished = self
            .repositories
            .iter()
            .all(|runtime| !matches!(runtime.state, LoadState::Loading(_)));
        let tabs_finished = self.tabs.iter().all(|tab| {
            tab.session.is_some()
                && !matches!(tab.state, LoadState::Loading(_))
                && !matches!(tab.details_state, LoadState::Loading(_))
        });
        repositories_finished && !self.tabs.is_empty() && tabs_finished
    }

    #[cfg(feature = "ui-smoke")]
    fn install_long_line_smoke(
        &mut self,
        mode: DiffMode,
        cx: &mut Context<Root>,
    ) -> Result<(), String> {
        let index = self
            .active_tab
            .ok_or_else(|| "long-line smoke has no active tab".to_owned())?;
        let text = format!(
            "LONG-LINE-BEGIN {} {SMOKE_LONG_LINE_TOKEN}",
            "0123456789abcdef".repeat(256)
        );
        let line = DiffLine {
            kind: DiffLineKind::Addition,
            old_line: None,
            new_line: Some(1),
            text,
        };
        let rows = vec![
            DiffRow::Hunk(format!("Native {mode:?} long-line reachability fixture")),
            match mode {
                DiffMode::SideBySide => {
                    let shared = "0123456789abcdef".repeat(256);
                    DiffRow::Split(AlignedRow {
                        old: Some(DiffLine {
                            kind: DiffLineKind::Deletion,
                            old_line: Some(1),
                            new_line: None,
                            text: format!("{SMOKE_OLD_LINE_START} {shared} {SMOKE_OLD_LINE_END}"),
                        }),
                        new: Some(DiffLine {
                            kind: DiffLineKind::Addition,
                            old_line: None,
                            new_line: Some(1),
                            text: format!("{SMOKE_NEW_LINE_START} {shared} {SMOKE_NEW_LINE_END}"),
                        }),
                    })
                }
                DiffMode::Auto | DiffMode::Unified => DiffRow::Unified(line),
            },
        ];
        if let Some(session) = &mut self.tabs[index].session {
            session.set_diff_mode(mode);
        }
        self.tabs[index].diff_content_width = diff_content_width(&rows, mode);
        self.tabs[index].diff_rows = rows;
        self.tabs[index].diff_scroll = UniformListScrollHandle::new();
        self.tabs[index].diff_horizontal = ScrollHandle::new();
        self.tabs[index]
            .diff_scroll
            .scroll_to_item(1, ScrollStrategy::Nearest);
        cx.notify();
        Ok(())
    }

    #[cfg(feature = "ui-smoke")]
    fn scroll_long_line_smoke_to_end(&mut self, cx: &mut Context<Root>) -> Result<f32, String> {
        let index = self
            .active_tab
            .ok_or_else(|| "long-line smoke has no active tab".to_owned())?;
        let handle = &self.tabs[index].diff_horizontal;
        let maximum = handle.max_offset().x.as_f32();
        if maximum < 1_000. {
            return Err(format!(
                "long-line viewport did not expose meaningful horizontal overflow ({maximum}px)"
            ));
        }
        handle.set_offset(point(px(-maximum), px(0.)));
        cx.notify();
        Ok(maximum)
    }

    #[cfg(feature = "ui-smoke")]
    fn validate_long_line_smoke(&self, maximum: f32, mode: DiffMode) -> Result<(), String> {
        let index = self
            .active_tab
            .ok_or_else(|| "long-line smoke has no active tab".to_owned())?;
        let tab = &self.tabs[index];
        if tab
            .session
            .as_ref()
            .is_none_or(|session| session.diff_mode() != mode)
        {
            return Err(format!("long-line scene did not expose {mode:?} mode"));
        }
        let rendered_source_has_token = tab.diff_rows.iter().any(|row| match (mode, row) {
            (DiffMode::Unified, DiffRow::Unified(line)) => {
                line.text.ends_with(SMOKE_LONG_LINE_TOKEN)
            }
            (DiffMode::SideBySide, DiffRow::Split(row)) => {
                row.old
                    .as_ref()
                    .is_some_and(|line| line.text.ends_with(SMOKE_OLD_LINE_END))
                    && row
                        .new
                        .as_ref()
                        .is_some_and(|line| line.text.ends_with(SMOKE_NEW_LINE_END))
            }
            _ => false,
        });
        let offset = tab.diff_horizontal.offset().x.as_f32();
        if !rendered_source_has_token || (offset + maximum).abs() > 1. {
            return Err(format!(
                "long-line far-end render state is invalid (token={rendered_source_has_token}, offset={offset}, maximum={maximum})"
            ));
        }
        Ok(())
    }

    #[cfg(feature = "ui-smoke")]
    fn validate_split_long_line_start(&self) -> Result<(), String> {
        let index = self
            .active_tab
            .ok_or_else(|| "split long-line smoke has no active tab".to_owned())?;
        let tab = &self.tabs[index];
        if tab.diff_horizontal.offset().x.as_f32().abs() > 1. {
            return Err("split long-line fixture did not begin at its left edge".to_owned());
        }
        let sentinels_are_side_specific = tab.diff_rows.iter().any(|row| {
            let DiffRow::Split(row) = row else {
                return false;
            };
            row.old
                .as_ref()
                .is_some_and(|line| line.text.starts_with(SMOKE_OLD_LINE_START))
                && row
                    .new
                    .as_ref()
                    .is_some_and(|line| line.text.starts_with(SMOKE_NEW_LINE_START))
        });
        if !sentinels_are_side_specific {
            return Err("split long-line start sentinels lost OLD/NEW identity".to_owned());
        }
        Ok(())
    }

    #[cfg(feature = "ui-smoke")]
    fn restore_real_diff_after_long_line(&mut self, cx: &mut Context<Root>) {
        if let Some(index) = self.active_tab {
            if let Some(session) = &mut self.tabs[index].session {
                session.set_diff_mode(DiffMode::Unified);
            }
            self.rebuild_diff(index, self.wide);
            cx.notify();
        }
    }

    #[cfg(feature = "ui-smoke")]
    fn run_primary_smoke_actions(
        &mut self,
        second_pr: Option<u64>,
        expect_restore: bool,
        auto_layout_verified: bool,
        window: &mut Window,
        cx: &mut Context<Root>,
    ) -> Result<SmokeActions, String> {
        let index = self
            .active_tab
            .ok_or_else(|| "no active pull request".to_owned())?;
        let tab = &self.tabs[index];
        let session = tab
            .session
            .as_ref()
            .ok_or_else(|| "active comparison is not loaded".to_owned())?;
        let restored =
            matches!(&tab.state, LoadState::Cached(notice) if notice.contains("Restored"));
        if expect_restore && !restored {
            return Err("expected a persisted review session to restore".to_owned());
        }
        let original_key = session
            .selected_file()
            .map(file_key)
            .ok_or_else(|| "comparison has no selected file".to_owned())?;
        let file_count = session.comparison().files.len();
        if file_count < 2 {
            return Err("native navigation smoke requires a multi-file pull request".to_owned());
        }
        let repository = tab.repository.clone();
        let number = tab.pull_request.number;
        let title = tab.pull_request.title.clone();
        let source_branch = tab.pull_request.source_branch.clone();
        let target_branch = tab.pull_request.target_branch.clone();
        let branches = format!("{} -> {}", source_branch, target_branch);
        let revision = session.revision().head_sha.clone();
        let initial_mode = session.diff_mode();
        let initial_rows_are_split = tab
            .diff_rows
            .iter()
            .any(|row| matches!(row, DiffRow::Split(_)));
        let actual_diff_width = effective_diff_viewport_width(&tab.diff_rows, &tab.diff_horizontal);
        let calculated_diff_width = self.available_diff_width(window);
        if actual_diff_width <= 0.
            || (actual_diff_width - calculated_diff_width).abs() > SPLITTER_WIDTH + 2.
        {
            return Err(format!(
                "actual diff viewport {actual_diff_width}px differs from pane calculation {calculated_diff_width}px"
            ));
        }
        if !expect_restore
            && (initial_mode != DiffMode::Auto || !self.wide || !initial_rows_are_split)
        {
            return Err(format!(
                "initial wide Auto layout was not split (pane {actual_diff_width}px)"
            ));
        }
        if expect_restore && initial_mode == DiffMode::Auto {
            return Err("explicit diff mode did not restore with the tab".into());
        }
        if !auto_layout_verified {
            return Err(
                "native Auto layout did not switch wide split -> narrow unified -> wide split"
                    .into(),
            );
        }
        if number == 14130
            && !tab
                .details
                .as_ref()
                .is_some_and(|details| details.body.contains("Issue fields are not currently"))
        {
            return Err("cli/cli#14130 Markdown overlap probe text is absent".into());
        }
        let next_expected_key = session
            .comparison()
            .files
            .iter()
            .position(|file| file_key(file) == original_key)
            .and_then(|position| session.comparison().files.get(position + 1))
            .map(file_key)
            .ok_or_else(|| "selected file has no next file for tree reveal smoke".to_owned())?;
        let saved_view_report =
            self.exercise_saved_view_smoke(&source_branch, &target_branch, expect_restore)?;

        if self.tabs[index]
            .file_tree
            .collapse_ancestor_of(&next_expected_key)
            .is_some()
            && self.tabs[index]
                .file_tree
                .file_is_visible(&next_expected_key)
        {
            return Err("collapsed tree ancestor left its changed file visible".into());
        }
        self.next_file(&NextFile, window, cx);
        let next_key = self.tabs[index]
            .session
            .as_ref()
            .and_then(ReviewSession::selected_file)
            .map(file_key)
            .ok_or_else(|| "next-file handler cleared selection".to_owned())?;
        if next_key == original_key {
            return Err("next-file handler did not advance selection".to_owned());
        }
        if next_key != next_expected_key || !self.tabs[index].file_tree.file_is_visible(&next_key) {
            return Err("next-file navigation did not expand and reveal its tree ancestors".into());
        }
        self.previous_file(&PreviousFile, window, cx);
        let returned_key = self.tabs[index]
            .session
            .as_ref()
            .and_then(ReviewSession::selected_file)
            .map(file_key)
            .ok_or_else(|| "previous-file handler cleared selection".to_owned())?;
        if returned_key != original_key {
            return Err("previous-file handler did not restore selection".to_owned());
        }

        let default_tree_width = self.panel_layout.file_tree_width;
        self.adjust_panel(PanelKind::FileTree, PANEL_KEYBOARD_STEP, window, cx);
        if self.panel_layout.file_tree_width <= default_tree_width {
            return Err("programmatic file-tree splitter adjustment had no effect".into());
        }
        self.reset_layout(window, cx);
        if self.panel_layout.file_tree_width != DEFAULT_FILE_TREE_WIDTH {
            return Err("reset-layout action did not restore file-tree width".into());
        }

        if self.tabs[index]
            .session
            .as_ref()
            .is_some_and(|session| session.diff_mode() == DiffMode::Auto)
        {
            self.cycle_diff(&CycleDiffMode, window, cx);
        }
        window.resize(size(px(1040.), px(720.)));
        if self.tabs[index]
            .session
            .as_ref()
            .is_none_or(|session| session.diff_mode() == DiffMode::Auto)
        {
            return Err("diff mode was not explicit before resize".to_owned());
        }

        if let Some(second) = second_pr
            && second != number
        {
            let repo_index = self
                .repositories
                .iter()
                .position(|runtime| runtime.repository.cache_key() == repository.cache_key())
                .ok_or_else(|| "active repository is absent from sidebar".to_owned())?;
            self.open_pr(repo_index, second, cx);
        }

        Ok(SmokeActions {
            primary_number: number,
            report: format!(
                "Real read complete\nRepository: {}\nPR: #{number} {title}\nBranches: {branches}\nRevision: {revision}\nFiles: {file_count}\nSelected: {}\nSidebar and details reads: settled\n{saved_view_report}\nRestart restore observed: {restored}\nInitial actual diff pane: {actual_diff_width}px ({initial_mode:?})\nInitial wide Auto split: {}\nNarrow Auto unified: {}\nCollapsed-directory next/previous reveal: {original_key} -> {next_key} -> {returned_key}\nProgrammatic splitter adjustment and Reset layout: passed\ncli/cli#14130 Markdown probe source: {}\nExplicit diff mode survived narrow resize\n",
                repository.full_name(),
                self.tabs[index]
                    .session
                    .as_ref()
                    .and_then(ReviewSession::selected_file)
                    .map(|file| file.path.as_str())
                    .unwrap_or("none"),
                !expect_restore,
                if expect_restore {
                    "covered by fresh light run"
                } else if auto_layout_verified {
                    "passed"
                } else {
                    "failed"
                },
                number == 14130,
            ),
            expectations: Vec::new(),
        })
    }

    #[cfg(feature = "ui-smoke")]
    fn exercise_saved_view_smoke(
        &mut self,
        source_branch: &str,
        target_branch: &str,
        expect_restore: bool,
    ) -> Result<String, String> {
        const NAME: &str = "Smoke · target → repository → source prefix";
        let prefix = source_branch
            .split_once('/')
            .map(|(first, _)| format!("{first}/"))
            .unwrap_or_else(|| source_branch.to_owned());
        if !expect_restore {
            self.view_editor.begin(&self.workspace);
            self.view_editor.replace_filter(Filter {
                source_branch: source_branch.to_owned(),
                state: "open".into(),
                ..Default::default()
            });
            self.view_editor.replace_groups(vec![
                GroupBy::TargetBranch,
                GroupBy::Repository,
                GroupBy::SourcePrefix(prefix.clone()),
            ]);
            self.view_editor.save_as(&mut self.workspace, NAME)?;
            self.save_workspace();
        }
        let view = self.workspace.view();
        if view.name != NAME
            || view.filter.source_branch != source_branch
            || view.filter.state != "open"
            || view.groups
                != vec![
                    GroupBy::TargetBranch,
                    GroupBy::Repository,
                    GroupBy::SourcePrefix(prefix.clone()),
                ]
        {
            return Err("saved composed view did not restore exactly".into());
        }
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| "saved-view smoke requires an isolated persistent store".to_owned())?;
        let restored = store
            .load_workspace()
            .map_err(|error| format!("cannot read saved view back: {error:#}"))?;
        if restored.view().name != NAME
            || restored.view().filter.source_branch != source_branch
            || restored.view().groups != view.groups
        {
            return Err("saved composed view read-back differs from active view".into());
        }
        Ok(format!(
            "Saved view: {NAME}\nExact source filter: {source_branch}\nGlobal grouping: {target_branch} → repository → prefix {prefix}\nSaved-view restart/read-back: passed"
        ))
    }

    #[cfg(feature = "ui-smoke")]
    fn run_tab_smoke_actions(
        &mut self,
        second_pr: Option<u64>,
        actions: &mut SmokeActions,
        window: &mut Window,
        cx: &mut Context<Root>,
    ) -> Result<(), String> {
        let primary_number = actions.primary_number;
        let primary = self
            .tabs
            .iter()
            .position(|tab| tab.pull_request.number == primary_number)
            .unwrap_or(0);
        let original_key = self.tabs[primary]
            .session
            .as_ref()
            .and_then(ReviewSession::selected_file)
            .map(file_key)
            .ok_or_else(|| "primary tab has no selected file".to_owned())?;

        let mut indices = vec![primary];
        if let Some(second) = second_pr
            && second != self.tabs[primary].pull_request.number
        {
            let secondary = self
                .tabs
                .iter()
                .position(|tab| tab.pull_request.number == second)
                .ok_or_else(|| format!("second PR #{second} did not load"))?;
            self.activate_tab(secondary, cx);
            self.activate_tab(primary, cx);
            let restored_key = self.tabs[primary]
                .session
                .as_ref()
                .and_then(ReviewSession::selected_file)
                .map(file_key)
                .ok_or_else(|| "tab switch cleared primary selection".to_owned())?;
            if restored_key != original_key {
                return Err("tab switch did not restore primary selection".to_owned());
            }
            indices.push(secondary);
            actions
                .report
                .push_str(&format!("Tab switch/restore: passed with PR #{second}\n"));
        }

        for index in indices {
            self.activate_tab(index, cx);
            let key = self.tabs[index]
                .session
                .as_ref()
                .and_then(ReviewSession::selected_file)
                .map(file_key)
                .ok_or_else(|| "tab has no selected file".to_owned())?;
            let previous = self.tabs[index]
                .session
                .as_ref()
                .is_some_and(|session| session.is_viewed(&key));
            self.toggle_viewed(&key, cx);
            actions.expectations.push(SmokeExpectation {
                repository: self.tabs[index].repository.clone(),
                number: self.tabs[index].pull_request.number,
                file_key: key,
                viewed: !previous,
            });
        }
        self.activate_tab(primary, cx);
        window.resize(size(px(1440.), px(900.)));
        if self.tabs[primary]
            .session
            .as_ref()
            .is_none_or(|session| session.diff_mode() == DiffMode::Auto)
        {
            return Err("explicit diff mode was lost after wide resize".to_owned());
        }
        self.save_workspace();
        Ok(())
    }

    #[cfg(feature = "ui-smoke")]
    fn validate_smoke_persistence(&self, expectations: &[SmokeExpectation]) -> Result<(), String> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| "smoke requires an isolated persistent store".to_owned())?;
        for expected in expectations {
            let session = store
                .load_review_session(&expected.repository, expected.number)
                .map_err(|error| format!("cannot reload PR #{}: {error:#}", expected.number))?;
            if session.is_viewed(&expected.file_key) != expected.viewed {
                return Err(format!(
                    "queued save for PR #{} did not preserve its latest viewed state",
                    expected.number
                ));
            }
        }
        Ok(())
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
        let active_identity = self.active_tab.and_then(|index| {
            self.tabs
                .get(index)
                .map(|tab| (tab.repository.cache_key(), tab.pull_request.number))
        });
        let mut active_tab = None;
        let mut tabs = Vec::new();
        for tab in &self.tabs {
            if let Some(session) = &tab.session {
                if active_identity.as_ref().is_some_and(|(key, number)| {
                    *key == tab.repository.cache_key() && *number == tab.pull_request.number
                }) {
                    active_tab = Some(tabs.len());
                }
                tabs.push(TabState {
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
                });
            }
        }
        self.workspace.tabs = tabs;
        self.workspace.active_tab = active_tab;
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
        let save_key = format!("{key}\n{number}");
        let latest = self
            .session_save_latest
            .entry(save_key.clone())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone();
        let lock = self
            .session_save_locks
            .entry(save_key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
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

    fn activate_tab(&mut self, index: usize, cx: &mut Context<Root>) {
        if index < self.tabs.len() {
            if let Some(previous) = self.active_tab {
                self.capture_scroll(previous);
            }
            self.active_tab = Some(index);
            if let Some(key) = self.tabs[index]
                .session
                .as_ref()
                .and_then(ReviewSession::selected_file)
                .map(file_key)
                && let Some(row) = self.tabs[index].file_tree.reveal_file(&key)
            {
                self.tabs[index]
                    .file_tree_scroll
                    .scroll_to_item(row, gpui::ScrollStrategy::Nearest);
            }
            self.setup_open = false;
            cx.notify();
        }
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
        let file_tree = session
            .as_ref()
            .map(|session| FileTree::new(&session.comparison().files))
            .unwrap_or_default();
        self.tabs.push(ReviewTab {
            repository,
            pull_request,
            session,
            state,
            generation: 0,
            metadata_generation: 0,
            diff_rows: Vec::new(),
            diff_scroll: UniformListScrollHandle::new(),
            diff_horizontal: ScrollHandle::new(),
            horizontal_positions: HashMap::new(),
            diff_content_width: 0.,
            file_tree,
            file_tree_scroll: UniformListScrollHandle::new(),
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
            self.rebuild_diff(index, self.wide);
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
                        if let Some((files, selected)) =
                            this.tabs[tab_index].session.as_ref().map(|session| {
                                (
                                    session.comparison().files.clone(),
                                    session.selected_file().map(file_key),
                                )
                            })
                        {
                            this.tabs[tab_index].file_tree.sync(&files);
                            if let Some(selected) = selected
                                && let Some(row) =
                                    this.tabs[tab_index].file_tree.reveal_file(&selected)
                            {
                                this.tabs[tab_index]
                                    .file_tree_scroll
                                    .scroll_to_item(row, ScrollStrategy::Nearest);
                            }
                        }
                        this.tabs[tab_index].state = if cached {
                            LoadState::Cached("Offline · immutable comparison from cache".into())
                        } else {
                            LoadState::Ready
                        };
                        this.tabs[tab_index].local_inventory = local_inventory;
                        this.rebuild_diff(tab_index, this.wide);
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
        let selected_key = file_key(file);
        let scroll_position = session.scroll_position();
        let horizontal_position = tab
            .horizontal_positions
            .get(&selected_key)
            .copied()
            .unwrap_or(0.);
        let resolved_mode = session.diff_mode().resolve(wide);
        tab.diff_rows = build_rows(parse_file(file), resolved_mode);
        tab.diff_content_width = diff_content_width(&tab.diff_rows, resolved_mode);
        tab.diff_scroll = UniformListScrollHandle::new();
        tab.diff_scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-scroll_position)));
        tab.diff_horizontal = ScrollHandle::new();
        tab.diff_horizontal
            .set_offset(point(px(-horizontal_position), px(0.)));
    }

    fn capture_scroll(&mut self, index: usize) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        let position = -tab.diff_scroll.0.borrow().base_handle.offset().y.as_f32();
        if let Some(session) = &mut tab.session {
            session.set_scroll_position(position);
            if let Some(key) = session.selected_file().map(file_key) {
                let horizontal = (-tab.diff_horizontal.offset().x.as_f32()).max(0.);
                tab.horizontal_positions.insert(key, horizontal);
            }
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
            if let Some(row) = self.tabs[index].file_tree.reveal_file(key) {
                self.tabs[index]
                    .file_tree_scroll
                    .scroll_to_item(row, ScrollStrategy::Nearest);
            }
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

    fn navigate_file(&mut self, next: bool, _window: &mut Window, cx: &mut Context<Root>) {
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
            if let Some(key) = self.tabs[index]
                .session
                .as_ref()
                .and_then(ReviewSession::selected_file)
                .map(file_key)
                && let Some(row) = self.tabs[index].file_tree.reveal_file(&key)
            {
                self.tabs[index]
                    .file_tree_scroll
                    .scroll_to_item(row, ScrollStrategy::Nearest);
            }
            self.rebuild_diff(index, self.wide);
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

    fn cycle_diff(&mut self, _: &CycleDiffMode, _window: &mut Window, cx: &mut Context<Root>) {
        let Some(index) = self.active_tab else { return };
        self.capture_scroll(index);
        if let Some(session) = &mut self.tabs[index].session {
            session.set_diff_mode(match session.diff_mode() {
                DiffMode::Auto => DiffMode::Unified,
                DiffMode::Unified => DiffMode::SideBySide,
                DiffMode::SideBySide => DiffMode::Auto,
            });
            self.rebuild_diff(index, self.wide);
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

    fn move_file_tree_cursor(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Root>) {
        let Some(index) = self.active_tab else { return };
        if let Some(key) = self.tabs[index].file_tree.move_cursor(delta) {
            self.select_file(&key, self.wide, cx);
        }
        if let Some(row) = self.tabs[index].file_tree.cursor_index() {
            self.tabs[index]
                .file_tree_scroll
                .scroll_to_item(row, ScrollStrategy::Nearest);
        }
        window.focus(&self.file_tree_focus, cx);
        cx.notify();
    }

    fn file_tree_left(&mut self, window: &mut Window, cx: &mut Context<Root>) {
        let Some(index) = self.active_tab else { return };
        self.tabs[index].file_tree.left();
        if let Some(row) = self.tabs[index].file_tree.cursor_index() {
            self.tabs[index]
                .file_tree_scroll
                .scroll_to_item(row, ScrollStrategy::Nearest);
        }
        window.focus(&self.file_tree_focus, cx);
        cx.notify();
    }

    fn file_tree_right(&mut self, window: &mut Window, cx: &mut Context<Root>) {
        let Some(index) = self.active_tab else { return };
        if let Some(key) = self.tabs[index].file_tree.right() {
            self.select_file(&key, self.wide, cx);
        }
        if let Some(row) = self.tabs[index].file_tree.cursor_index() {
            self.tabs[index]
                .file_tree_scroll
                .scroll_to_item(row, ScrollStrategy::Nearest);
        }
        window.focus(&self.file_tree_focus, cx);
        cx.notify();
    }

    fn activate_file_tree(&mut self, window: &mut Window, cx: &mut Context<Root>) {
        let Some(index) = self.active_tab else { return };
        if let Some(key) = self.tabs[index].file_tree.activate_cursor() {
            self.select_file(&key, self.wide, cx);
        }
        window.focus(&self.file_tree_focus, cx);
        cx.notify();
    }

    fn scroll_diff_horizontally(&mut self, amount: Option<f32>, cx: &mut Context<Root>) {
        let Some(index) = self.active_tab else { return };
        let handle = &self.tabs[index].diff_horizontal;
        let current = handle.offset();
        let maximum = handle.max_offset().x.as_f32();
        let target = match amount {
            Some(amount) => (current.x.as_f32() - amount).clamp(-maximum, 0.),
            None => -maximum,
        };
        handle.set_offset(point(px(target), px(0.)));
        cx.notify();
    }

    fn apply_filter(&mut self, personal: PersonalFilter, cx: &mut Context<Root>) {
        let mut view = self.workspace.view();
        view.filter.search = self.query.read(cx).value().trim().to_owned();
        view.filter.personal = personal;
        self.workspace.views[self.workspace.selected_view] = view;
        self.save_workspace();
        cx.notify();
    }

    fn open_view_editor(&mut self, window: &mut Window, cx: &mut Context<Root>) {
        let view = self.workspace.view();
        self.view_editor.begin(&self.workspace);
        self.view_editor_scroll.set_offset(point(px(0.), px(0.)));
        self.view_inputs.set_view(&view, window, cx);
        self.command_palette = false;
        self.view_inputs
            .name
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        cx.notify();
    }

    fn sync_view_editor_inputs(&mut self, cx: &Context<Root>) {
        let Some(draft) = self.view_editor.draft() else {
            return;
        };
        let filter = self.view_inputs.filter(&draft.filter, cx);
        let prefix = ViewEditorInputs::value(&self.view_inputs.source_prefix, cx);
        let source_index = draft
            .groups
            .iter()
            .position(|group| matches!(group, GroupBy::SourceBranch | GroupBy::SourcePrefix(_)));
        self.view_editor.replace_filter(filter);
        if let Some(index) = source_index {
            let uses_prefix = self
                .view_editor
                .draft()
                .and_then(|view| view.groups.get(index))
                .is_some_and(|group| matches!(group, GroupBy::SourcePrefix(_)));
            if uses_prefix {
                self.view_editor
                    .set_source_group_prefix(index, Some(&prefix));
            }
        }
    }

    fn commit_view_editor(&mut self, save_as: bool, window: &mut Window, cx: &mut Context<Root>) {
        let old_state = self.workspace.view().filter.state;
        self.sync_view_editor_inputs(cx);
        let name = ViewEditorInputs::value(&self.view_inputs.name, cx);
        let result = if save_as {
            self.view_editor.save_as(&mut self.workspace, &name)
        } else {
            self.view_editor.apply(&mut self.workspace, &name)
        };
        match result {
            Ok(()) => {
                let view = self.workspace.view();
                self.query.update(cx, |query, cx| {
                    query.set_value(view.filter.search.clone(), window, cx)
                });
                self.save_workspace();
                if old_state != view.filter.state {
                    self.refresh_all(cx);
                }
                self.status = if save_as {
                    format!("Saved view “{}”", view.name)
                } else {
                    format!("Applied view “{}”", view.name)
                };
                self.focus.focus(window, cx);
            }
            Err(error) => {
                self.status = error;
            }
        }
        cx.notify();
    }

    fn cancel_view_editor(&mut self, window: &mut Window, cx: &mut Context<Root>) {
        self.view_editor.cancel();
        self.status = "View changes cancelled".into();
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn delete_current_view(&mut self, window: &mut Window, cx: &mut Context<Root>) {
        let old_state = self.workspace.view().filter.state;
        self.view_editor.delete_selected(&mut self.workspace);
        let view = self.workspace.view();
        self.query.update(cx, |query, cx| {
            query.set_value(view.filter.search.clone(), window, cx)
        });
        self.save_workspace();
        if old_state != view.filter.state {
            self.refresh_all(cx);
        }
        self.status = "Deleted saved view; a usable view remains selected".into();
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn select_view(&mut self, index: usize, window: &mut Window, cx: &mut Context<Root>) {
        if index >= self.workspace.views.len() {
            return;
        }
        self.workspace.selected_view = index;
        self.view_editor.cancel();
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
            .on_action(cx.listener(|root, _: &Save, window, cx| {
                if let Root::Review(this) = root
                    && this.view_editor.is_open()
                {
                    this.commit_view_editor(false, window, cx);
                }
            }))
            .on_action(cx.listener(|root, _: &TogglePalette, _, cx| {
                if let Root::Review(this) = root {
                    this.command_palette = !this.command_palette;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|root, _: &ToggleInspector, window, cx| {
                if let Root::Review(this) = root {
                    this.inspector_open = !this.inspector_open;
                    this.refresh_auto_layout(window);
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
            .on_action(cx.listener(|root, _: &FileTreeUp, window, cx| {
                if let Root::Review(this) = root {
                    this.move_file_tree_cursor(-1, window, cx);
                }
            }))
            .on_action(cx.listener(|root, _: &FileTreeDown, window, cx| {
                if let Root::Review(this) = root {
                    this.move_file_tree_cursor(1, window, cx);
                }
            }))
            .on_action(cx.listener(|root, _: &FileTreeLeft, window, cx| {
                if let Root::Review(this) = root {
                    this.file_tree_left(window, cx);
                }
            }))
            .on_action(cx.listener(|root, _: &FileTreeRight, window, cx| {
                if let Root::Review(this) = root {
                    this.file_tree_right(window, cx);
                }
            }))
            .on_action(cx.listener(|root, _: &FileTreeActivate, window, cx| {
                if let Root::Review(this) = root {
                    this.activate_file_tree(window, cx);
                }
            }))
            .on_action(cx.listener(|root, _: &ToggleSidebar, window, cx| {
                if let Root::Review(this) = root {
                    this.panel_layout.sidebar_collapsed = !this.panel_layout.sidebar_collapsed;
                    this.refresh_auto_layout(window);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|root, _: &ToggleFileTree, window, cx| {
                if let Root::Review(this) = root {
                    this.panel_layout.file_tree_collapsed = !this.panel_layout.file_tree_collapsed;
                    this.refresh_auto_layout(window);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|root, _: &SidebarNarrower, window, cx| {
                if let Root::Review(this) = root {
                    this.adjust_panel(PanelKind::Sidebar, -PANEL_KEYBOARD_STEP, window, cx);
                }
            }))
            .on_action(cx.listener(|root, _: &SidebarWider, window, cx| {
                if let Root::Review(this) = root {
                    this.adjust_panel(PanelKind::Sidebar, PANEL_KEYBOARD_STEP, window, cx);
                }
            }))
            .on_action(cx.listener(|root, _: &FileTreeNarrower, window, cx| {
                if let Root::Review(this) = root {
                    this.adjust_panel(PanelKind::FileTree, -PANEL_KEYBOARD_STEP, window, cx);
                }
            }))
            .on_action(cx.listener(|root, _: &FileTreeWider, window, cx| {
                if let Root::Review(this) = root {
                    this.adjust_panel(PanelKind::FileTree, PANEL_KEYBOARD_STEP, window, cx);
                }
            }))
            .on_action(cx.listener(|root, _: &DetailsNarrower, window, cx| {
                if let Root::Review(this) = root {
                    this.adjust_panel(PanelKind::Details, -PANEL_KEYBOARD_STEP, window, cx);
                }
            }))
            .on_action(cx.listener(|root, _: &DetailsWider, window, cx| {
                if let Root::Review(this) = root {
                    this.adjust_panel(PanelKind::Details, PANEL_KEYBOARD_STEP, window, cx);
                }
            }))
            .on_action(cx.listener(|root, _: &ResetLayout, window, cx| {
                if let Root::Review(this) = root {
                    this.reset_layout(window, cx);
                }
            }))
            .on_action(cx.listener(|root, _: &DiffScrollLeft, _, cx| {
                if let Root::Review(this) = root {
                    this.scroll_diff_horizontally(Some(-96.), cx);
                }
            }))
            .on_action(cx.listener(|root, _: &DiffScrollRight, _, cx| {
                if let Root::Review(this) = root {
                    this.scroll_diff_horizontally(Some(96.), cx);
                }
            }))
            .on_action(cx.listener(|root, _: &DiffScrollHome, _, cx| {
                if let Root::Review(this) = root
                    && let Some(index) = this.active_tab
                {
                    this.tabs[index]
                        .diff_horizontal
                        .set_offset(point(px(0.), px(0.)));
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|root, _: &DiffScrollEnd, _, cx| {
                if let Root::Review(this) = root {
                    this.scroll_diff_horizontally(None, cx);
                }
            }))
            .on_key_down(cx.listener(|root, event: &KeyDownEvent, window, cx| {
                if let Root::Review(this) = root
                    && this.view_editor.is_open()
                    && event.keystroke.key == "escape"
                {
                    this.cancel_view_editor(window, cx);
                    cx.stop_propagation();
                }
            }))
            .size_full()
            .flex()
            .font_family(UI_FONT)
            .text_size(px(13.))
            .text_color(colors.text)
            .bg(rgba(0x00000000))
            .child(self.render_sidebar(colors, window, cx))
            .child(self.render_splitter(PanelKind::Sidebar, colors, window, cx))
            .child(self.render_main(colors, window, cx))
            .when(self.command_palette, |root| {
                root.child(self.render_palette(colors, cx))
            })
            .when(self.view_editor.is_open(), |root| {
                root.child(self.render_view_editor(colors, cx))
            })
    }

    fn render_splitter(
        &self,
        panel: PanelKind,
        colors: Palette,
        window: &Window,
        cx: &mut Context<Root>,
    ) -> impl IntoElement {
        let (sidebar, tree, details) = self.resolved_panel_widths(window);
        let start_width = match panel {
            PanelKind::Sidebar => sidebar,
            PanelKind::FileTree => tree,
            PanelKind::Details => details,
        };
        let drag = PanelResizeDrag {
            panel,
            start_width,
            start_x: Rc::new(Cell::new(None)),
        };
        div()
            .id(SharedString::from(format!("splitter-{panel:?}")))
            .w(px(SPLITTER_WIDTH))
            .h_full()
            .flex_none()
            .cursor_move()
            .bg(colors.canvas)
            .border_l_1()
            .border_r_1()
            .border_color(colors.border)
            .hover(|splitter| splitter.bg(colors.selected))
            .on_drag(drag, |drag, position, _, cx| {
                drag.start_x.set(Some(position.x.as_f32()));
                cx.new(|_| SplitterDragPreview)
            })
            .on_drag_move(cx.listener(
                |root, event: &DragMoveEvent<PanelResizeDrag>, window, cx| {
                    let Root::Review(this) = root else { return };
                    let drag = event.drag(cx);
                    let Some(start_x) = drag.start_x.get() else {
                        return;
                    };
                    let mut delta = event.event.position.x.as_f32() - start_x;
                    if drag.panel == PanelKind::Details {
                        delta = -delta;
                    }
                    let width = (drag.start_width + delta).clamp(
                        match drag.panel {
                            PanelKind::Sidebar => MIN_SIDEBAR_WIDTH,
                            PanelKind::FileTree => MIN_FILE_TREE_WIDTH,
                            PanelKind::Details => MIN_DETAILS_WIDTH,
                        },
                        MAX_PANEL_WIDTH,
                    );
                    match drag.panel {
                        PanelKind::Sidebar => {
                            this.panel_layout.sidebar_width = width;
                            this.panel_layout.sidebar_collapsed = false;
                        }
                        PanelKind::FileTree => {
                            this.panel_layout.file_tree_width = width;
                            this.panel_layout.file_tree_collapsed = false;
                        }
                        PanelKind::Details => {
                            this.panel_layout.details_width = width;
                            this.inspector_open = true;
                        }
                    }
                    this.refresh_auto_layout(window);
                    cx.notify();
                },
            ))
    }

    fn render_sidebar(&self, colors: Palette, window: &Window, cx: &mut Context<Root>) -> Div {
        let (sidebar_width, _, _) = self.resolved_panel_widths(window);
        if self.panel_layout.sidebar_collapsed {
            return div()
                .w(px(sidebar_width))
                .h_full()
                .flex()
                .flex_col()
                .items_center()
                .bg(colors.sidebar)
                .child(
                    div()
                        .id("restore-sidebar")
                        .mt_3()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .text_color(colors.accent)
                        .child("›")
                        .on_click(cx.listener(|root, _, window, cx| {
                            if let Root::Review(this) = root {
                                this.panel_layout.sidebar_collapsed = false;
                                this.refresh_auto_layout(window);
                                cx.notify();
                            }
                        })),
                );
        }
        let view = self.workspace.view();
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
        let inventories = self
            .repositories
            .iter()
            .enumerate()
            .map(|(index, runtime)| RepositoryPulls {
                index,
                repository: &runtime.repository,
                pull_requests: &runtime.pull_requests,
            })
            .collect::<Vec<_>>();
        let mut rows = Vec::new();
        for row in compose_sidebar_rows(&inventories, &view) {
            match row {
                SidebarRow::Group { depth, label } => rows.push(
                    div()
                        .pl(px(12. + depth as f32 * 14.))
                        .pr_3()
                        .pt(if depth == 0 { px(10.) } else { px(4.) })
                        .pb_1()
                        .text_xs()
                        .font_weight(if depth == 0 {
                            FontWeight::MEDIUM
                        } else {
                            FontWeight::NORMAL
                        })
                        .text_color(if depth == 0 {
                            colors.muted
                        } else {
                            colors.faint
                        })
                        .child(if depth == 0 {
                            label
                        } else {
                            format!("↳ {label}")
                        })
                        .into_any_element(),
                ),
                SidebarRow::Pull {
                    repository_index,
                    repository_key,
                    pull_request,
                } => {
                    let pull_request = *pull_request;
                    let selected = self
                        .active_tab
                        .and_then(|index| self.tabs.get(index))
                        .is_some_and(|tab| {
                            tab.repository.cache_key() == repository_key
                                && tab.pull_request.number == pull_request.number
                        });
                    let number = pull_request.number;
                    rows.push(
                        div()
                            .id(SharedString::from(format!(
                                "pr-{repository_index}-{number}"
                            )))
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
                                    .child(
                                        div().text_color(colors.faint).child(format!("#{number}")),
                                    )
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
                                    this.open_pr(repository_index, number, cx)
                                }
                            }))
                            .into_any_element(),
                    );
                }
            }
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .px_5()
                    .py_4()
                    .text_xs()
                    .text_color(colors.faint)
                    .child("No pull requests match this view.")
                    .into_any_element(),
            );
        }
        for runtime in &self.repositories {
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
            .w(px(sidebar_width))
            .min_w(px(sidebar_width))
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
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_xs().text_color(colors.faint).child("READ ONLY"))
                            .child(
                                div()
                                    .id("collapse-sidebar")
                                    .cursor_pointer()
                                    .text_color(colors.accent)
                                    .child("‹")
                                    .on_click(cx.listener(|root, _, window, cx| {
                                        if let Root::Review(this) = root {
                                            this.panel_layout.sidebar_collapsed = true;
                                            this.refresh_auto_layout(window);
                                            cx.notify();
                                        }
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .child(
                        div()
                            .h(px(32.))
                            .px_2()
                            .rounded_md()
                            .bg(colors.elevated)
                            .border_1()
                            .border_color(colors.border)
                            .font_family(UI_FONT)
                            .child(Input::new(&self.query)),
                    )
                    .child(
                        div()
                            .id("apply-sidebar-search")
                            .pt_1()
                            .text_xs()
                            .text_color(colors.accent)
                            .cursor_pointer()
                            .child("Apply search")
                            .on_click(cx.listener(|root, _, _, cx| {
                                if let Root::Review(this) = root {
                                    let personal = this.workspace.view().filter.personal;
                                    this.apply_filter(personal, cx);
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
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .font_weight(FontWeight::MEDIUM)
                            .child(view.name.clone()),
                    )
                    .child(
                        div()
                            .id("edit-view")
                            .cursor_pointer()
                            .text_color(colors.accent)
                            .child("Edit view…")
                            .on_click(cx.listener(|root, _, window, cx| {
                                if let Root::Review(this) = root {
                                    this.open_view_editor(window, cx)
                                }
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
                    .pb_1()
                    .text_xs()
                    .text_color(colors.faint)
                    .child(view_summary(&view)),
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

    fn render_view_editor(&self, colors: Palette, cx: &mut Context<Root>) -> impl IntoElement {
        let draft = self.view_editor.draft().cloned().unwrap_or_default();
        let prefix = ViewEditorInputs::value(&self.view_inputs.source_prefix, cx);
        let groups = draft
            .groups
            .iter()
            .enumerate()
            .map(|(index, group)| {
                let is_source = matches!(group, GroupBy::SourceBranch | GroupBy::SourcePrefix(_));
                let uses_prefix = matches!(group, GroupBy::SourcePrefix(_));
                div()
                    .id(SharedString::from(format!("view-group-{index}")))
                    .py_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(22.))
                                    .text_xs()
                                    .text_color(colors.faint)
                                    .child(format!("{}", index + 1)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(group_label(Some(group))),
                            )
                            .child(
                                small_action("Change", colors)
                                    .id(SharedString::from(format!("change-view-group-{index}")))
                                    .on_click(cx.listener(move |root, _, _, cx| {
                                        if let Root::Review(this) = root {
                                            let prefix = ViewEditorInputs::value(
                                                &this.view_inputs.source_prefix,
                                                cx,
                                            );
                                            this.view_editor.cycle_group(index, &prefix);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                small_action("↑", colors)
                                    .id(SharedString::from(format!("move-up-view-group-{index}")))
                                    .on_click(cx.listener(move |root, _, _, cx| {
                                        if let Root::Review(this) = root {
                                            this.view_editor.move_group(index, -1);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                small_action("↓", colors)
                                    .id(SharedString::from(format!("move-down-view-group-{index}")))
                                    .on_click(cx.listener(move |root, _, _, cx| {
                                        if let Root::Review(this) = root {
                                            this.view_editor.move_group(index, 1);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                small_action("Remove", colors)
                                    .id(SharedString::from(format!("remove-view-group-{index}")))
                                    .on_click(cx.listener(move |root, _, _, cx| {
                                        if let Root::Review(this) = root {
                                            this.view_editor.remove_group(index);
                                            cx.notify();
                                        }
                                    })),
                            ),
                    )
                    .when(is_source, |row| {
                        row.child(
                            div()
                                .ml(px(30.))
                                .mt_2()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    side_control("Exact", !uses_prefix, colors)
                                        .id(SharedString::from(format!(
                                            "source-group-exact-{index}"
                                        )))
                                        .on_click(cx.listener(move |root, _, _, cx| {
                                            if let Root::Review(this) = root {
                                                this.view_editor
                                                    .set_source_group_prefix(index, None);
                                                cx.notify();
                                            }
                                        })),
                                )
                                .child(
                                    side_control("Prefix", uses_prefix, colors)
                                        .id(SharedString::from(format!(
                                            "source-group-prefix-{index}"
                                        )))
                                        .on_click(cx.listener(move |root, _, _, cx| {
                                            if let Root::Review(this) = root {
                                                let prefix = ViewEditorInputs::value(
                                                    &this.view_inputs.source_prefix,
                                                    cx,
                                                );
                                                this.view_editor
                                                    .set_source_group_prefix(index, Some(&prefix));
                                                cx.notify();
                                            }
                                        })),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .h(px(32.))
                                        .px_2()
                                        .rounded_md()
                                        .border_1()
                                        .border_color(colors.border)
                                        .bg(colors.elevated)
                                        .child(Input::new(&self.view_inputs.source_prefix)),
                                ),
                        )
                    })
            })
            .collect::<Vec<_>>();

        let state = if draft.filter.state.is_empty() {
            "open"
        } else {
            draft.filter.state.as_str()
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .bg(rgba(0x00000066))
            .child(
                div()
                    .id("view-editor")
                    .w(px(760.))
                    .max_h_full()
                    .flex()
                    .flex_col()
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.surface)
                    .shadow_lg()
                    .child(
                        div()
                            .px_5()
                            .py_4()
                            .border_b_1()
                            .border_color(colors.border)
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Edit sidebar view"),
                            )
                            .child(div().mt_1().text_xs().text_color(colors.muted).child(
                                "Changes stay in this editor until you apply or save them.",
                            )),
                    )
                    .child(
                        div()
                            .id("view-editor-scroll")
                            .track_scroll(&self.view_editor_scroll)
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px_5()
                            .py_4()
                            .child(editor_field("View name", &self.view_inputs.name, colors))
                            .child(section_label("FILTERS", colors))
                            .child(
                                div()
                                    .flex()
                                    .gap_3()
                                    .child(editor_field("Search", &self.view_inputs.search, colors))
                                    .child(editor_field(
                                        "Author",
                                        &self.view_inputs.author,
                                        colors,
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_3()
                                    .child(editor_field(
                                        "Requested reviewer",
                                        &self.view_inputs.reviewer,
                                        colors,
                                    ))
                                    .child(editor_field(
                                        "Assignee",
                                        &self.view_inputs.assignee,
                                        colors,
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_3()
                                    .child(editor_field("Label", &self.view_inputs.label, colors))
                                    .child(editor_field(
                                        "Review status",
                                        &self.view_inputs.review_status,
                                        colors,
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_3()
                                    .child(editor_field(
                                        "Checks",
                                        &self.view_inputs.check_status,
                                        colors,
                                    ))
                                    .child(editor_field(
                                        "Target branch",
                                        &self.view_inputs.target_branch,
                                        colors,
                                    )),
                            )
                            .child(editor_field(
                                "Exact source branch",
                                &self.view_inputs.source_branch,
                                colors,
                            ))
                            .child(choice_row(
                                "Pull request state",
                                &[
                                    ("Open", state.eq_ignore_ascii_case("open")),
                                    ("Closed", state.eq_ignore_ascii_case("closed")),
                                    ("Merged", state.eq_ignore_ascii_case("merged")),
                                    ("All", state.eq_ignore_ascii_case("all")),
                                ],
                                colors,
                                cx,
                                |this, index| {
                                    this.view_editor
                                        .set_pr_state(["open", "closed", "merged", "all"][index]);
                                },
                            ))
                            .child(choice_row(
                                "Draft status",
                                &[
                                    ("All", draft.filter.draft.is_none()),
                                    ("Draft", draft.filter.draft == Some(true)),
                                    ("Ready", draft.filter.draft == Some(false)),
                                ],
                                colors,
                                cx,
                                |this, index| {
                                    this.view_editor
                                        .set_draft_state([None, Some(true), Some(false)][index]);
                                },
                            ))
                            .child(choice_row(
                                "Relationship",
                                &[
                                    ("All", draft.filter.personal == PersonalFilter::All),
                                    (
                                        "Needs review",
                                        draft.filter.personal == PersonalFilter::ReviewRequested,
                                    ),
                                    ("Mine", draft.filter.personal == PersonalFilter::Own),
                                    (
                                        "Participating",
                                        draft.filter.personal == PersonalFilter::Participating,
                                    ),
                                ],
                                colors,
                                cx,
                                |this, index| {
                                    this.view_editor.set_personal(
                                        [
                                            PersonalFilter::All,
                                            PersonalFilter::ReviewRequested,
                                            PersonalFilter::Own,
                                            PersonalFilter::Participating,
                                        ][index]
                                            .clone(),
                                    );
                                },
                            ))
                            .child(section_label("GROUPING ORDER", colors))
                            .children(groups)
                            .child(
                                small_action("＋ Add grouping level", colors)
                                    .id("add-view-group")
                                    .mt_2()
                                    .on_click(cx.listener(|root, _, _, cx| {
                                        if let Root::Review(this) = root {
                                            this.view_editor.add_group();
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                div()
                                    .mt_3()
                                    .text_xs()
                                    .text_color(colors.muted)
                                    .child(format!(
                                        "Source prefix preview: {}",
                                        if prefix.is_empty() {
                                            "not set"
                                        } else {
                                            &prefix
                                        }
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .px_5()
                            .py_3()
                            .flex()
                            .items_center()
                            .justify_between()
                            .border_t_1()
                            .border_color(colors.border)
                            .child(
                                modal_button("Delete view", false, colors)
                                    .id("delete-view")
                                    .on_click(cx.listener(|root, _, window, cx| {
                                        if let Root::Review(this) = root {
                                            this.delete_current_view(window, cx);
                                        }
                                    })),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        modal_button("Cancel", false, colors)
                                            .id("cancel-view-editor")
                                            .on_click(cx.listener(|root, _, window, cx| {
                                                if let Root::Review(this) = root {
                                                    this.cancel_view_editor(window, cx);
                                                }
                                            })),
                                    )
                                    .child(
                                        modal_button("Save as new", false, colors)
                                            .id("save-new-view")
                                            .on_click(cx.listener(|root, _, window, cx| {
                                                if let Root::Review(this) = root {
                                                    this.commit_view_editor(true, window, cx);
                                                }
                                            })),
                                    )
                                    .child(
                                        modal_button("Apply  ⌘S", true, colors)
                                            .id("apply-view-editor")
                                            .on_click(cx.listener(|root, _, window, cx| {
                                                if let Root::Review(this) = root {
                                                    this.commit_view_editor(false, window, cx);
                                                }
                                            })),
                                    ),
                            ),
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
                        this.activate_tab(index, cx);
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
                                    .on_click(cx.listener(|root, _, window, cx| {
                                        if let Root::Review(this) = root {
                                            this.inspector_open = !this.inspector_open;
                                            this.refresh_auto_layout(window);
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
                    .child(self.render_splitter(PanelKind::FileTree, colors, window, cx))
                    .child(self.render_diff(index, colors, cx))
                    .when(self.inspector_open, |body| {
                        body.child(self.render_splitter(PanelKind::Details, colors, window, cx))
                            .child(self.render_inspector(index, colors, window, cx))
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
        let rows = tab.file_tree.rows();
        let count = rows.len();
        let selected_key = tab
            .session
            .as_ref()
            .and_then(ReviewSession::selected_file)
            .map(file_key);
        let viewed = tab
            .session
            .as_ref()
            .map(|session| {
                session
                    .comparison()
                    .files
                    .iter()
                    .map(|file| {
                        let key = file_key(file);
                        (key.clone(), session.is_viewed(&key))
                    })
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let scroll = tab.file_tree_scroll.clone();
        let cursor = tab.file_tree.cursor_index();
        let local_inventory = tab.local_inventory;
        let root = cx.entity();
        let focus = self.file_tree_focus.clone();
        let click_focus = focus.clone();
        let (_, tree_width, _) = self.resolved_panel_widths(window);
        if self.panel_layout.file_tree_collapsed {
            return div()
                .w(px(tree_width))
                .h_full()
                .flex()
                .flex_col()
                .items_center()
                .bg(colors.canvas)
                .child(
                    div()
                        .id("restore-file-tree")
                        .mt_3()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .text_color(colors.accent)
                        .child("›")
                        .on_click(cx.listener(|root, _, window, cx| {
                            if let Root::Review(this) = root {
                                this.panel_layout.file_tree_collapsed = false;
                                this.refresh_auto_layout(window);
                                cx.notify();
                            }
                        })),
                );
        }
        div()
            .w(px(tree_width))
            .min_w(px(tree_width))
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
                    .child(format!(
                        "Files  {}",
                        tab.session
                            .as_ref()
                            .map(|session| session.comparison().files.len())
                            .unwrap_or(0)
                    ))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .text_xs()
                            .text_color(colors.muted)
                            .child("⌘[  ⌘]")
                            .child(
                                div()
                                    .id("collapse-file-tree")
                                    .cursor_pointer()
                                    .text_color(colors.accent)
                                    .child("‹")
                                    .on_click(cx.listener(|root, _, window, cx| {
                                        if let Root::Review(this) = root {
                                            this.panel_layout.file_tree_collapsed = true;
                                            this.refresh_auto_layout(window);
                                            cx.notify();
                                        }
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .id("changed-files-scroll")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .track_focus(&focus)
                    .key_context("FileTree")
                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        window.focus(&click_focus, cx);
                    })
                    .child(
                        uniform_list(
                            SharedString::from(format!("changed-file-tree-{index}")),
                            count,
                            move |range: Range<usize>, _, _| {
                                range
                                    .map(|row_index| {
                                        let row = rows[row_index].clone();
                                        let row_identity = FileTree::row_identity(&row);
                                        let row_root = root.clone();
                                        let row_focus = focus.clone();
                                        let selected = row.file_key().is_some_and(|key| {
                                            selected_key.as_deref() == Some(key)
                                        });
                                        let is_cursor = cursor == Some(row_index);
                                        let mut item = div()
                                            .id(SharedString::from(format!("tree-{row_identity}")))
                                            .h(px(28.))
                                            .pl(px(8. + row.depth as f32 * 14.))
                                            .pr_2()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .cursor_pointer()
                                            .when(selected, |row| row.bg(colors.selected))
                                            .when(is_cursor && !selected, |row| {
                                                row.border_1().border_color(colors.accent)
                                            })
                                            .hover(|row| row.bg(colors.selected));
                                        match row.kind {
                                            TreeRowKind::Directory {
                                                directory_key,
                                                expanded,
                                                raw,
                                            } => {
                                                let click_root = row_root.clone();
                                                let click_identity = row_identity.clone();
                                                item = item
                                                    .child(
                                                        div()
                                                            .w(px(14.))
                                                            .text_color(colors.faint)
                                                            .child(if expanded {
                                                                "▾"
                                                            } else {
                                                                "▸"
                                                            }),
                                                    )
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .overflow_hidden()
                                                            .text_ellipsis()
                                                            .child(row.label),
                                                    )
                                                    .when(raw, |row| {
                                                        row.child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(colors.amber)
                                                                .child("RAW"),
                                                        )
                                                    })
                                                    .on_click(move |_, window, cx| {
                                                        window.focus(&row_focus, cx);
                                                        click_root.update(cx, |root, cx| {
                                                            if let Root::Review(this) = root
                                                                && let Some(index) = this.active_tab
                                                            {
                                                                this.tabs[index]
                                                                    .file_tree
                                                                    .set_cursor(
                                                                        click_identity.clone(),
                                                                    );
                                                                this.tabs[index]
                                                                    .file_tree
                                                                    .toggle_directory(
                                                                        &directory_key,
                                                                    );
                                                                cx.notify();
                                                            }
                                                        });
                                                    });
                                            }
                                            TreeRowKind::File {
                                                file_key,
                                                status,
                                                additions,
                                                deletions,
                                                patch_available,
                                                rename_from,
                                                raw,
                                            } => {
                                                let viewed_state =
                                                    viewed.get(&file_key).copied().unwrap_or(false);
                                                let viewed_key = file_key.clone();
                                                let viewed_root = row_root.clone();
                                                let click_key = file_key.clone();
                                                let click_identity = row_identity.clone();
                                                let display_label = rename_from
                                                    .as_deref()
                                                    .and_then(|previous| {
                                                        previous.rsplit('/').next()
                                                    })
                                                    .filter(|previous| *previous != row.label)
                                                    .map(|previous| {
                                                        format!("{previous} → {}", row.label)
                                                    })
                                                    .unwrap_or(row.label);
                                                item = item
                                                    .child(
                                                        div()
                                                            .w(px(18.))
                                                            .flex_none()
                                                            .text_center()
                                                            .text_xs()
                                                            .text_color(colors.faint)
                                                            .child(file_status_badge(&status)),
                                                    )
                                                    .child(
                                                        div()
                                                            .id(SharedString::from(format!(
                                                                "viewed-{file_key}"
                                                            )))
                                                            .w(px(18.))
                                                            .flex_none()
                                                            .text_center()
                                                            .text_color(if viewed_state {
                                                                colors.green
                                                            } else {
                                                                colors.faint
                                                            })
                                                            .child(if viewed_state {
                                                                "✓"
                                                            } else {
                                                                "○"
                                                            })
                                                            .on_click(move |_, _, cx| {
                                                                viewed_root.update(
                                                                    cx,
                                                                    |root, cx| {
                                                                        if let Root::Review(this) =
                                                                            root
                                                                        {
                                                                            this.toggle_viewed(
                                                                                &viewed_key,
                                                                                cx,
                                                                            );
                                                                        }
                                                                    },
                                                                );
                                                            }),
                                                    )
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .overflow_hidden()
                                                            .text_ellipsis()
                                                            .child(display_label),
                                                    )
                                                    .when(raw, |row| {
                                                        row.child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(colors.amber)
                                                                .child("RAW"),
                                                        )
                                                    })
                                                    .child(
                                                        div()
                                                            .ml_1()
                                                            .flex()
                                                            .gap_1()
                                                            .text_xs()
                                                            .when(patch_available, |stats| {
                                                                stats
                                                                    .child(
                                                                        div()
                                                                            .text_color(
                                                                                colors.green,
                                                                            )
                                                                            .child(format!(
                                                                                "+{additions}"
                                                                            )),
                                                                    )
                                                                    .child(
                                                                        div()
                                                                            .text_color(colors.red)
                                                                            .child(format!(
                                                                                "−{deletions}"
                                                                            )),
                                                                    )
                                                            })
                                                            .when(!patch_available, |stats| {
                                                                stats
                                                                    .text_color(colors.faint)
                                                                    .child(if local_inventory {
                                                                        "on select"
                                                                    } else {
                                                                        "metadata"
                                                                    })
                                                            }),
                                                    )
                                                    .on_click(move |_, window, cx| {
                                                        window.focus(&row_focus, cx);
                                                        row_root.update(cx, |root, cx| {
                                                            if let Root::Review(this) = root
                                                                && let Some(index) = this.active_tab
                                                            {
                                                                this.tabs[index]
                                                                    .file_tree
                                                                    .set_cursor(
                                                                        click_identity.clone(),
                                                                    );
                                                                this.select_file(
                                                                    &click_key, this.wide, cx,
                                                                );
                                                                cx.notify();
                                                            }
                                                        });
                                                    });
                                            }
                                        }
                                        item
                                    })
                                    .collect::<Vec<_>>()
                            },
                        )
                        .track_scroll(&scroll)
                        .w_full()
                        .h_full(),
                    )
                    .child(
                        div().absolute().inset_0().child(
                            Scrollbar::vertical(&scroll)
                                .id(SharedString::from(format!("file-tree-scrollbar-{index}")))
                                .viewport_from_layout(),
                        ),
                    ),
            )
    }

    fn render_diff(
        &self,
        index: usize,
        colors: Palette,
        _cx: &mut Context<Root>,
    ) -> impl IntoElement {
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
        let split_mode = rows.iter().any(|row| matches!(row, DiffRow::Split(_)));
        let scroll = tab.diff_scroll.clone();
        let horizontal = tab.diff_horizontal.clone();
        let content_width = tab.diff_content_width.max(1.);
        let split_text_width = split_text_content_width(&rows);
        let focus = self.diff_focus.clone();
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
            .when(split_mode, |pane| {
                pane.child(
                    div()
                        .h(px(24.))
                        .flex()
                        .font_family(CODE_FONT)
                        .text_xs()
                        .text_color(colors.muted)
                        .bg(colors.elevated)
                        .border_b_1()
                        .border_color(colors.border)
                        .child(div().w_1_2().px_3().child("OLD"))
                        .child(
                            div()
                                .w_1_2()
                                .px_3()
                                .border_l_1()
                                .border_color(colors.border)
                                .child("NEW"),
                        ),
                )
            })
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
                let body = div()
                    .id(SharedString::from(format!("diff-horizontal-{index}")))
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .track_focus(&focus)
                    .key_context("DiffPane")
                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        window.focus(&focus, cx);
                    });
                if split_mode {
                    let row_horizontal = horizontal.clone();
                    body.child(
                        uniform_list(
                            SharedString::from(format!("diff-rows-{index}")),
                            count,
                            move |range: Range<usize>, _, _| {
                                range
                                    .map(|index| {
                                        render_split_diff_row(
                                            &rows[index],
                                            colors,
                                            &row_horizontal,
                                            split_text_width,
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            },
                        )
                        .track_scroll(&scroll)
                        .w_full()
                        .h_full(),
                    )
                    .child(diff_horizontal_scrollbar(index, &horizontal))
                    .into_any_element()
                } else {
                    body.overflow_x_scroll()
                        .restrict_scroll_to_axis()
                        .track_scroll(&horizontal)
                        .child(
                            uniform_list(
                                SharedString::from(format!("diff-rows-{index}")),
                                count,
                                move |range: Range<usize>, _, _| {
                                    range
                                        .map(|index| render_diff_row(&rows[index], colors))
                                        .collect::<Vec<_>>()
                                },
                            )
                            .track_scroll(&scroll)
                            .w(px(content_width))
                            .h_full(),
                        )
                        .child(diff_horizontal_scrollbar(index, &horizontal))
                        .into_any_element()
                }
            })
    }

    fn render_inspector(
        &self,
        index: usize,
        colors: Palette,
        window: &Window,
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
        let content = match current {
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
                        fields.push(markdown_detail(
                            format!("pr-description-{}", tab.pull_request.number),
                            "Description",
                            &details.body,
                            colors,
                        ));
                    }
                }
                div().children(fields).into_any_element()
            }
            InspectorSection::Activity => {
                let mut activity = Vec::new();
                if let Some(details) = &tab.details {
                    for (position, comment) in details.issue_comments.iter().take(20).enumerate() {
                        activity.push(activity_item(
                            format!("issue-comment-{position}"),
                            comment.author.as_deref().unwrap_or("Unknown author"),
                            &comment.body,
                            &comment.created_at,
                            colors,
                        ));
                    }
                    for (position, review) in details.reviews.iter().take(20).enumerate() {
                        activity.push(activity_item(
                            format!("review-{position}"),
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
                        activity.push(
                            div().text_color(colors.amber).child(
                                "Activity is incomplete; GitHub response limits were reached.",
                            ),
                        );
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
        let (_, _, details_width) = self.resolved_panel_widths(window);
        div()
            .w(px(details_width))
            .min_w(px(details_width))
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
                            .on_click(cx.listener(|root, _, window, cx| {
                                if let Root::Review(this) = root {
                                    this.inspector_open = !this.inspector_open;
                                    this.refresh_auto_layout(window);
                                    this.command_palette = false;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        command_row("Reset panel layout", "⌃⌥0", colors)
                            .id("command-reset-layout")
                            .cursor_pointer()
                            .hover(|row| row.bg(colors.selected))
                            .on_click(cx.listener(|root, _, window, cx| {
                                if let Root::Review(this) = root {
                                    this.reset_layout(window, cx);
                                    this.command_palette = false;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        command_row("Edit sidebar filters and grouping", "⌘S to apply", colors)
                            .id("command-edit-view")
                            .cursor_pointer()
                            .hover(|row| row.bg(colors.selected))
                            .on_click(cx.listener(|root, _, window, cx| {
                                if let Root::Review(this) = root {
                                    this.open_view_editor(window, cx);
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

fn editor_field(label: &str, editor: &Entity<InputState>, colors: Palette) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .mb_3()
        .child(field_label(label, colors))
        .child(input_box(editor, colors))
}

fn section_label(label: &str, colors: Palette) -> Div {
    div()
        .mt_3()
        .mb_2()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(colors.faint)
        .child(label.to_owned())
}

fn small_action(label: &str, colors: Palette) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .text_color(colors.accent)
        .cursor_pointer()
        .hover(|button| button.bg(colors.selected))
        .child(label.to_owned())
}

fn modal_button(label: &str, primary: bool, colors: Palette) -> Div {
    div()
        .h(px(32.))
        .px_3()
        .flex()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(if primary {
            colors.accent
        } else {
            colors.border
        })
        .bg(if primary {
            colors.selected
        } else {
            colors.elevated
        })
        .text_color(if primary { colors.accent } else { colors.text })
        .cursor_pointer()
        .hover(|button| button.bg(colors.selected))
        .child(label.to_owned())
}

fn choice_row(
    label: &str,
    choices: &[(&'static str, bool)],
    colors: Palette,
    cx: &mut Context<Root>,
    select: fn(&mut ReviewWorkspace, usize),
) -> Div {
    let controls = choices
        .iter()
        .enumerate()
        .map(|(index, (label, selected))| {
            side_control(label, *selected, colors)
                .id(SharedString::from(format!(
                    "view-choice-{}-{index}",
                    label.to_ascii_lowercase().replace(' ', "-")
                )))
                .on_click(cx.listener(move |root, _, _, cx| {
                    if let Root::Review(this) = root {
                        select(this, index);
                        cx.notify();
                    }
                }))
        });
    div()
        .mb_3()
        .child(field_label(label, colors))
        .child(div().mt_2().flex().flex_wrap().gap_1().children(controls))
}

fn view_summary(view: &cibergit::workspace::SavedView) -> String {
    let state = if view.filter.state.is_empty() {
        "open"
    } else {
        view.filter.state.as_str()
    };
    let groups = if view.groups.is_empty() {
        "no grouping".to_owned()
    } else {
        view.groups
            .iter()
            .map(|group| group_label(Some(group)))
            .collect::<Vec<_>>()
            .join(" → ")
    };
    format!("{state} · {groups}")
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

fn markdown_detail(id: String, label: &str, body: &str, colors: Palette) -> Div {
    div()
        .mb_4()
        .child(
            div()
                .text_xs()
                .text_color(colors.muted)
                .child(label.to_owned()),
        )
        .child(
            div()
                .mt_1()
                .child(markdown_text(id, body, colors).text_size(px(12.))),
        )
}

fn activity_item(id: String, author: &str, body: &str, timestamp: &str, colors: Palette) -> Div {
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
        .child(
            div()
                .mt_1()
                .child(markdown_text(id, body, colors).text_size(px(12.))),
        )
}

fn markdown_text(id: String, source: &str, colors: Palette) -> TextView {
    TextView::markdown(SharedString::from(id), media_free_markdown(source))
        .style(
            TextViewStyle::default()
                .with_foreground(colors.muted.into())
                .with_muted_foreground(colors.faint.into())
                .with_link(colors.accent.into())
                .with_code_background(colors.elevated.into())
                .with_border(colors.border.into())
                .with_heading_base_font_size(px(12.))
                .with_heading_font_size(|level, base| match level {
                    1 => base * 1.5,
                    2 => base * 1.35,
                    3 => base * 1.2,
                    _ => base,
                })
                .with_dark(colors.dark),
        )
        // gpui-base's inline flow measures each wrapped row from the inherited
        // window line height. Own that metric here so a narrow panel cannot
        // combine 12px body text and independently-sized headings on a 13px row.
        .text_size(px(12.))
        .line_height(px(20.))
        .selectable(true)
}

/// Keep GitHub prose readable without allowing rich text to resolve media URIs.
fn media_free_markdown(source: &str) -> String {
    let mut without_comments = String::with_capacity(source.len());
    let mut remaining = source;
    while let Some(start) = remaining.find("<!--") {
        without_comments.push_str(&remaining[..start]);
        let after_start = &remaining[start + 4..];
        if let Some(end) = after_start.find("-->") {
            remaining = &after_start[end + 3..];
        } else {
            remaining = "";
            break;
        }
    }
    without_comments.push_str(remaining);
    let escaped_inline_code = escape_inline_code_delimiters(&without_comments);
    escaped_inline_code
        .replace("![", "[Image: ")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The pinned TextView switches an entire paragraph to its fragment-based
/// InlineFlow whenever it contains inline-code marks. At narrow widths that
/// flow advances some wrapped fragments from their pre-wrap origin, making
/// adjacent words paint on top of each other. Preserve the literal backticks
/// and selectable source text while keeping ordinary paragraphs on TextView's
/// stable single-Inline layout. Fenced code blocks remain fenced.
fn escape_inline_code_delimiters(source: &str) -> String {
    let mut result = String::with_capacity(source.len());
    let mut characters = source.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '`' {
            result.push(character);
            continue;
        }
        let mut run = 1usize;
        while characters.next_if_eq(&'`').is_some() {
            run += 1;
        }
        if run >= 3 {
            result.extend(std::iter::repeat_n('`', run));
        } else {
            for _ in 0..run {
                result.push('\\');
                result.push('`');
            }
        }
    }
    result
}

fn poll_due(tick: u64, delay: Duration) -> bool {
    let periods = (delay.as_secs() / 15).max(1);
    tick.is_multiple_of(periods)
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

fn display_columns(text: &str) -> usize {
    let mut columns = 0usize;
    for character in text.chars() {
        columns += match character {
            '\t' => 4 - columns % 4,
            '\u{0000}'..='\u{001f}' | '\u{007f}' => 1,
            character if character.is_ascii() => 1,
            _ => 2,
        };
    }
    columns
}

fn diff_content_width(rows: &[DiffRow], mode: DiffMode) -> f32 {
    let maximum = rows
        .iter()
        .map(|row| match row {
            DiffRow::Hunk(header) => 24. + display_columns(header) as f32 * DIFF_CELL_WIDTH,
            DiffRow::Unified(line) => {
                DIFF_FIXED_COLUMNS + display_columns(&line.text) as f32 * DIFF_CELL_WIDTH
            }
            DiffRow::Split(row) => {
                let old = row
                    .old
                    .as_ref()
                    .map(|line| display_columns(&line.text))
                    .unwrap_or(0);
                let new = row
                    .new
                    .as_ref()
                    .map(|line| display_columns(&line.text))
                    .unwrap_or(0);
                2. * (76. + old.max(new) as f32 * DIFF_CELL_WIDTH)
            }
        })
        .fold(0f32, f32::max);
    let minimum = match mode {
        DiffMode::SideBySide => MIN_SPLIT_DIFF_WIDTH,
        DiffMode::Auto | DiffMode::Unified => 420.,
    };
    maximum.max(minimum)
}

fn split_text_content_width(rows: &[DiffRow]) -> f32 {
    rows.iter()
        .filter_map(|row| match row {
            DiffRow::Split(row) => Some(
                row.old
                    .iter()
                    .chain(row.new.iter())
                    .map(|line| display_columns(&line.text) as f32 * DIFF_CELL_WIDTH)
                    .fold(0f32, f32::max),
            ),
            DiffRow::Hunk(_) | DiffRow::Unified(_) => None,
        })
        .fold(1f32, f32::max)
}

#[cfg(feature = "ui-smoke")]
fn effective_diff_viewport_width(rows: &[DiffRow], horizontal: &ScrollHandle) -> f32 {
    let source_viewport = horizontal.bounds().size.width.as_f32();
    if rows.iter().any(|row| matches!(row, DiffRow::Split(_))) {
        2. * (source_viewport + SPLIT_GUTTER_WIDTH)
    } else {
        source_viewport
    }
}

/// Tabs are expanded to stable four-column stops. Exceptionally long lines are
/// split at UTF-8 boundaries so no single text shaping request grows without a
/// bound; every chunk remains in the same horizontal row and stays reachable.
fn line_text_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut columns = 0usize;
    for character in text.chars() {
        let expansion = if character == '\t' {
            " ".repeat(4 - columns % 4)
        } else if character.is_control() {
            "�".to_owned()
        } else {
            character.to_string()
        };
        let width = if character == '\t' {
            expansion.len()
        } else if character.is_ascii() {
            1
        } else {
            2
        };
        if !chunk.is_empty()
            && chunk.len().saturating_add(expansion.len()) > EXCEPTIONAL_LINE_CHUNK_BYTES
        {
            chunks.push(std::mem::take(&mut chunk));
        }
        chunk.push_str(&expansion);
        columns += width;
    }
    if !chunk.is_empty() || chunks.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

fn line_text(text: &str, foreground: Rgba) -> Div {
    div()
        .flex()
        .items_center()
        .whitespace_nowrap()
        .text_color(foreground)
        .children(
            line_text_chunks(text)
                .into_iter()
                .map(|chunk| div().flex_none().whitespace_nowrap().child(chunk)),
        )
}

fn render_diff_row(row: &DiffRow, colors: Palette) -> AnyElement {
    match row {
        DiffRow::Hunk(header) => div()
            .h(px(26.))
            .w_full()
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
                .w_full()
                .flex()
                .items_center()
                .bg(background)
                .font_family(CODE_FONT)
                .text_xs()
                .child(line_number(line.old_line, colors))
                .child(line_number(line.new_line, colors))
                .child(div().w(px(18.)).text_color(foreground).child(marker))
                .child(line_text(&line.text, foreground))
                .into_any_element()
        }
        DiffRow::Split(row) => div()
            .h(px(24.))
            .w_full()
            .flex()
            .font_family(CODE_FONT)
            .text_xs()
            .child(split_cell(row.old.as_ref(), true, colors))
            .child(split_cell(row.new.as_ref(), false, colors))
            .into_any_element(),
    }
}

fn diff_horizontal_scrollbar(index: usize, horizontal: &ScrollHandle) -> Div {
    div()
        .absolute()
        .left_0()
        .right_0()
        .bottom_0()
        .h(px(12.))
        .child(
            Scrollbar::horizontal(horizontal)
                .id(SharedString::from(format!("diff-scrollbar-{index}")))
                .viewport_from_layout(),
        )
}

fn render_split_diff_row(
    row: &DiffRow,
    colors: Palette,
    horizontal: &ScrollHandle,
    text_width: f32,
) -> AnyElement {
    match row {
        DiffRow::Hunk(header) => div()
            .h(px(26.))
            .w_full()
            .px_3()
            .flex()
            .items_center()
            .overflow_hidden()
            .bg(colors.elevated)
            .text_color(colors.accent)
            .font_family(CODE_FONT)
            .text_xs()
            .child(header.clone())
            .into_any_element(),
        DiffRow::Split(row) => div()
            .h(px(24.))
            .w_full()
            .flex()
            .overflow_hidden()
            .font_family(CODE_FONT)
            .text_xs()
            .child(split_cell_scrolled(
                row.old.as_ref(),
                true,
                colors,
                horizontal,
                text_width,
            ))
            .child(split_cell_scrolled(
                row.new.as_ref(),
                false,
                colors,
                horizontal,
                text_width,
            ))
            .into_any_element(),
        DiffRow::Unified(_) => render_diff_row(row, colors),
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
        .child(line_text(&line.text, foreground))
}

fn split_cell_scrolled(
    line: Option<&DiffLine>,
    old: bool,
    colors: Palette,
    horizontal: &ScrollHandle,
    text_width: f32,
) -> Div {
    let (background, foreground, marker, number, text) = match line {
        Some(line) => {
            let (background, foreground, marker) = line_colors(line.kind, colors);
            (
                background,
                foreground,
                marker,
                if old { line.old_line } else { line.new_line },
                line.text.as_str(),
            )
        }
        None => (colors.elevated, colors.muted, " ", None, ""),
    };
    div()
        .w_1_2()
        .h_full()
        .min_w_0()
        .flex()
        .items_center()
        .overflow_hidden()
        .bg(background)
        .border_r_1()
        .border_color(colors.border)
        .child(line_number(number, colors))
        .child(
            div()
                .w(px(18.))
                .flex_none()
                .text_color(foreground)
                .child(marker),
        )
        .child(
            div()
                .id(if old {
                    "split-old-source"
                } else {
                    "split-new-source"
                })
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_x_scroll()
                .restrict_scroll_to_axis()
                .track_scroll(horizontal)
                .child(line_text(text, foreground).w(px(text_width)).h_full()),
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

#[cfg(test)]
mod layout_tests {
    use super::{
        COLLAPSED_PANEL_WIDTH, DEFAULT_SIDEBAR_WIDTH, DiffLine, DiffLineKind, DiffMode, DiffRow,
        EXCEPTIONAL_LINE_CHUNK_BYTES, MAX_PANEL_WIDTH, MIN_DETAILS_WIDTH, MIN_FILE_TREE_WIDTH,
        MIN_SIDEBAR_WIDTH, MIN_SPLIT_DIFF_WIDTH, PanelKind, PanelLayout, available_diff_width_for,
        diff_content_width, display_columns, line_text_chunks, media_free_markdown,
        resolved_panel_widths_for,
    };

    #[test]
    fn auto_uses_remaining_diff_pane_instead_of_whole_window() {
        let layout = PanelLayout::default();
        let wide_diff = available_diff_width_for(&layout, true, 1440.);
        assert_eq!(wide_diff, 606.);
        assert!(wide_diff >= MIN_SPLIT_DIFF_WIDTH);

        let narrow_diff = available_diff_width_for(&layout, true, 1180.);
        assert_eq!(narrow_diff, 360.);
        assert!(narrow_diff < MIN_SPLIT_DIFF_WIDTH);
        let (sidebar, tree, details) = resolved_panel_widths_for(&layout, true, 1040.);
        assert!(sidebar >= MIN_SIDEBAR_WIDTH);
        assert!(tree >= MIN_FILE_TREE_WIDTH);
        assert!(details >= MIN_DETAILS_WIDTH);
        assert_eq!(available_diff_width_for(&layout, true, 1040.), 360.);
    }

    #[test]
    fn panel_keyboard_adjustment_collapse_and_reset_are_bounded() {
        let mut layout = PanelLayout::default();
        layout.adjust(PanelKind::Sidebar, -10_000.);
        layout.adjust(PanelKind::FileTree, 10_000.);
        layout.adjust(PanelKind::Details, -10_000.);
        assert_eq!(layout.sidebar_width, MIN_SIDEBAR_WIDTH);
        assert_eq!(layout.file_tree_width, MAX_PANEL_WIDTH);
        assert_eq!(layout.details_width, MIN_DETAILS_WIDTH);
        layout.sidebar_collapsed = true;
        assert_eq!(
            layout.width(PanelKind::Sidebar, true),
            COLLAPSED_PANEL_WIDTH
        );
        layout = PanelLayout::default();
        assert_eq!(layout.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
        assert!(!layout.sidebar_collapsed);
    }

    #[test]
    fn long_line_width_accounts_for_tabs_and_unicode_without_clipping() {
        assert_eq!(display_columns("a\tb"), 5);
        assert_eq!(display_columns("a界b"), 4);
        let token = "CIBERGIT_LONG_LINE_END_7F3A";
        let text = format!("{}\t界{token}", "x".repeat(20_000));
        let chunks = line_text_chunks(&text);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| { chunk.len() <= EXCEPTIONAL_LINE_CHUNK_BYTES + char::MAX_LEN_UTF8 })
        );
        assert!(chunks.last().is_some_and(|chunk| chunk.ends_with(token)));
        let rows = vec![DiffRow::Unified(DiffLine {
            kind: DiffLineKind::Addition,
            old_line: None,
            new_line: Some(1),
            text,
        })];
        assert!(diff_content_width(&rows, DiffMode::Unified) > 140_000.);
    }

    #[test]
    fn markdown_probe_preserves_text_and_disables_media_resolution() {
        let source = "### Description\n\nIssue fields are not currently available through `gh`.\n\n```sh\ngh issue view\n```\n\n![unsafe](https://example.test/a.png)";
        let safe = media_free_markdown(source);
        assert!(safe.contains("Issue fields are not currently available"));
        assert!(safe.contains("\\`gh\\`"));
        assert!(safe.contains("```sh"));
        assert!(safe.contains("[Image: unsafe]"));
        assert!(!safe.contains("!["));
    }
}
