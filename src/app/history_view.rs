//! State for the History page: one repository's commit graph, and the diff of
//! whichever commit is selected.
//!
//! This owns its own comparison, file selection and scroll positions, the way
//! `stack_view` does. Opening History never disturbs a pull request tab's
//! pinned comparison, review draft or local checkout, and History has no write
//! action of any kind.
//!
//! Replies are matched against a token rather than an index. A history read,
//! a commit's diff and a lazy patch load can all land after the user has
//! changed repository, narrowed the scope, or selected another commit; each
//! one is dropped rather than mixed into what is now on screen.

use super::{DiffRow, LoadState, build_rows, diff_content_width};
use cibergit::{
    domain::Account,
    history::{
        CommitGraph, GraphRow, HistoryCommit, HistoryScope, RefKind, RepositoryHistory, lay_out,
    },
    review::{DiffMode, ReviewSession},
};
use gpui::{ListAlignment, ListState, ScrollHandle, px};
use std::rc::Rc;

/// How many branch chips the scope row will show. Past this the row stops
/// being a way to choose and starts being a wall of names.
const MAX_SCOPE_CHIPS: usize = 12;

/// Identifies one history read. A reply that does not match the controller's
/// current identity is discarded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryRequestToken {
    repository_key: String,
    account: Account,
    scope: HistoryScope,
    generation: u64,
}

/// Identifies one commit's diff read, which is superseded by selecting another
/// commit as well as by everything that supersedes the history itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryDiffToken {
    repository_key: String,
    sha: String,
    generation: u64,
}

pub(crate) struct HistoryController {
    repository_key: String,
    account: Account,
    generation: u64,
    diff_generation: u64,
    pub scope: HistoryScope,
    pub state: LoadState,
    pub history: Option<RepositoryHistory>,
    /// How wide the gutter has to be, and whether the graph had to fold lanes
    /// together to fit it.
    pub lane_count: usize,
    pub overflowed: bool,
    /// The commits and their rows, held behind an `Rc` so a render can hand
    /// them to the list closure with a pointer copy. Cloning five hundred
    /// commits on every frame is what this exists to avoid.
    commits: Rc<Vec<HistoryCommit>>,
    rows: Rc<Vec<GraphRow>>,
    /// The exact commit, not its row. A refresh that reorders or shortens the
    /// history must not silently move the selection to a different commit.
    pub selected: Option<String>,
    pub commit_scroll: gpui::UniformListScrollHandle,
    pub file_scroll: gpui::UniformListScrollHandle,
    pub diff_state: LoadState,
    pub session: Option<ReviewSession>,
    pub diff_rows: Vec<DiffRow>,
    pub diff_scroll: ListState,
    pub horizontal: ScrollHandle,
    pub diff_content_width: f32,
    pub feedback: Option<String>,
}

impl HistoryController {
    pub fn new(repository_key: String, account: Account) -> Self {
        Self {
            repository_key,
            account,
            generation: 0,
            diff_generation: 0,
            scope: HistoryScope::AllRefs,
            state: LoadState::Loading("Reading this repository's history…".into()),
            history: None,
            lane_count: 0,
            overflowed: false,
            commits: Rc::new(Vec::new()),
            rows: Rc::new(Vec::new()),
            selected: None,
            commit_scroll: gpui::UniformListScrollHandle::new(),
            file_scroll: gpui::UniformListScrollHandle::new(),
            diff_state: LoadState::Ready,
            session: None,
            diff_rows: Vec::new(),
            diff_scroll: ListState::new(0, ListAlignment::Top, px(480.)),
            horizontal: ScrollHandle::new(),
            diff_content_width: 0.,
            feedback: None,
        }
    }

    pub fn repository_key(&self) -> &str {
        &self.repository_key
    }

    pub fn commits(&self) -> &Rc<Vec<HistoryCommit>> {
        &self.commits
    }

    pub fn rows(&self) -> &Rc<Vec<GraphRow>> {
        &self.rows
    }

    pub fn selected_commit(&self) -> Option<&HistoryCommit> {
        let sha = self.selected.as_deref()?;
        self.commits.iter().find(|commit| commit.sha == sha)
    }

    /// The branches worth offering as a scope, newest-first as the history
    /// itself is ordered, so the chips lead with what is being worked on.
    pub fn scope_choices(&self) -> Vec<String> {
        let mut names = Vec::new();
        for commit in self.commits.iter() {
            for label in &commit.refs {
                if matches!(label.kind, RefKind::Head | RefKind::LocalBranch)
                    && !names.contains(&label.name)
                {
                    names.push(label.name.clone());
                }
            }
            if names.len() >= MAX_SCOPE_CHIPS {
                break;
            }
        }
        names.truncate(MAX_SCOPE_CHIPS);
        names
    }

    /// Point this page at another repository. Everything loaded belongs to the
    /// previous one, so none of it is carried across.
    pub fn retarget(&mut self, repository_key: String, account: Account) -> bool {
        if self.repository_key == repository_key && self.account == account {
            return false;
        }
        *self = Self::new(repository_key, account);
        true
    }

    pub fn set_scope(&mut self, scope: HistoryScope) -> bool {
        if self.scope == scope {
            return false;
        }
        self.scope = scope;
        // The commits are about to be replaced. Everything derived from them
        // goes with them; the selection is re-resolved by sha once the new
        // history lands, so a commit present in both scopes stays selected.
        self.history = None;
        self.clear_graph();
        self.state = LoadState::Loading("Reading this repository's history…".into());
        true
    }

    pub fn begin_read(&mut self, generation: u64) -> HistoryRequestToken {
        self.generation = generation;
        self.state = LoadState::Loading("Reading this repository's history…".into());
        HistoryRequestToken {
            repository_key: self.repository_key.clone(),
            account: self.account.clone(),
            scope: self.scope.clone(),
            generation,
        }
    }

    pub fn accepts(&self, token: &HistoryRequestToken) -> bool {
        self.repository_key == token.repository_key
            && self.account == token.account
            && self.scope == token.scope
            && self.generation == token.generation
    }

    /// Install a completed history read. The selection is kept only if the
    /// exact commit is still present.
    pub fn install(&mut self, history: RepositoryHistory) {
        let CommitGraph {
            rows,
            lane_count,
            overflowed,
        } = lay_out(&history.commits);
        self.rows = Rc::new(rows);
        self.lane_count = lane_count;
        self.overflowed = overflowed;
        self.commits = Rc::new(history.commits.clone());
        self.state = match &history.notice {
            Some(notice) => LoadState::Cached(notice.clone()),
            None => LoadState::Ready,
        };
        let still_present = self
            .selected
            .as_ref()
            .is_some_and(|sha| history.commits.iter().any(|commit| &commit.sha == sha));
        if !still_present {
            self.selected = None;
            self.clear_diff();
        }
        self.history = Some(history);
    }

    pub fn fail(&mut self, error: String) {
        self.state = LoadState::Error(error);
        self.history = None;
        self.clear_graph();
        self.selected = None;
        self.clear_diff();
    }

    fn clear_graph(&mut self) {
        self.commits = Rc::new(Vec::new());
        self.rows = Rc::new(Vec::new());
        self.lane_count = 0;
        self.overflowed = false;
    }

    /// Select a commit. Returns false when it was already selected, so the
    /// caller does not start a second read for the same diff.
    pub fn select(&mut self, sha: &str) -> bool {
        if self.selected.as_deref() == Some(sha) {
            return false;
        }
        self.selected = Some(sha.to_owned());
        self.clear_diff();
        true
    }

    pub fn begin_diff(&mut self, generation: u64) -> Option<HistoryDiffToken> {
        let sha = self.selected.clone()?;
        self.diff_generation = generation;
        self.diff_state = LoadState::Loading("Loading this commit's changes…".into());
        Some(HistoryDiffToken {
            repository_key: self.repository_key.clone(),
            sha,
            generation,
        })
    }

    pub fn accepts_diff(&self, token: &HistoryDiffToken) -> bool {
        self.repository_key == token.repository_key
            && self.selected.as_deref() == Some(token.sha.as_str())
            && self.diff_generation == token.generation
    }

    pub fn install_diff(&mut self, session: ReviewSession, wide: bool) {
        self.session = Some(session);
        self.diff_state = LoadState::Ready;
        self.feedback = None;
        self.rebuild(wide);
    }

    pub fn fail_diff(&mut self, error: String) {
        self.clear_diff();
        self.diff_state = LoadState::Error(error);
    }

    fn clear_diff(&mut self) {
        self.session = None;
        self.diff_rows.clear();
        self.diff_content_width = 0.;
        self.diff_scroll = ListState::new(0, ListAlignment::Top, px(480.));
        self.horizontal = ScrollHandle::new();
        self.diff_state = LoadState::Ready;
        self.feedback = None;
    }

    pub fn select_file(&mut self, key: &str, wide: bool) -> bool {
        self.capture_scroll();
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        if !session.select_file(key) {
            return false;
        }
        self.rebuild(wide);
        true
    }

    /// Rebuild the rendered diff rows from the selected file. The row list and
    /// its scroll state are replaced wholesale, and the per-file offsets the
    /// session remembers are restored onto the new handles.
    pub fn rebuild(&mut self, wide: bool) {
        let Some(file) = self.session.as_ref().and_then(ReviewSession::selected_file) else {
            self.diff_rows.clear();
            self.diff_content_width = 0.;
            return;
        };
        let mode = match self
            .session
            .as_ref()
            .expect("session is present")
            .diff_mode()
        {
            DiffMode::Auto if wide => DiffMode::SideBySide,
            DiffMode::Auto => DiffMode::Unified,
            mode => mode,
        };
        self.diff_rows = build_rows(cibergit::review::parse_file(file), mode);
        self.diff_content_width = diff_content_width(&self.diff_rows, mode);
        let session = self.session.as_ref().expect("session is present");
        self.diff_scroll = ListState::new(
            self.diff_rows.len(),
            ListAlignment::Top,
            px(session.scroll_position()),
        );
        self.horizontal = ScrollHandle::new();
        self.horizontal.set_offset(gpui::point(
            px(-session.horizontal_scroll_position()),
            px(0.),
        ));
    }

    pub fn capture_scroll(&mut self) {
        let vertical = self.diff_scroll.scroll_px_offset_for_scrollbar().y.as_f32();
        let horizontal = (-self.horizontal.offset().x.as_f32()).max(0.);
        if let Some(session) = self.session.as_mut() {
            session.set_scroll_position(vertical);
            session.set_horizontal_scroll_position(horizontal);
        }
    }
}
