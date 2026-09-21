//! State for the Pull requests page: one repository's pull-request index, open
//! and closed, newest activity first.
//!
//! This is a browsing surface, not a review one. It holds summaries, which
//! carry no revision, so nothing here can pin or advance a comparison; opening
//! a row hands its number to the ordinary open path, which reads the full pull
//! request. Opening the page therefore never disturbs a pull request tab.
//!
//! Pages are read on demand rather than enumerated. The sidebar's read walks
//! every page and hydrates each one over GraphQL, which is right for a
//! repository's open pull requests and wrong for its whole history.
//!
//! Replies are matched against a token rather than an index. Retargeting the
//! repository, switching the state, and reloading all supersede whatever is in
//! flight; a superseded reply is dropped rather than appended to a list that is
//! now about something else.

use super::LoadState;
use cibergit::domain::{Account, PullRequestSummary, PullRequestSummaryPage};

/// Which of the repository's pull requests the page is asking for.
///
/// These are GitHub's own index tabs. `Closed` includes merged pull requests,
/// as GitHub's does — each row's state tells them apart, and a client-side
/// "merged only" filter would leave paging claiming more results while showing
/// none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum BrowseState {
    #[default]
    Open,
    Closed,
    All,
}

impl BrowseState {
    pub fn query(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Closed => "Closed",
            Self::All => "All",
        }
    }

    pub const ALL: [Self; 3] = [Self::Open, Self::Closed, Self::All];
}

/// Identifies one page read. A reply that does not match the controller's
/// current identity is discarded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BrowseToken {
    repository_key: String,
    account: Account,
    state: BrowseState,
    page: usize,
    generation: u64,
}

impl BrowseToken {
    pub fn state(&self) -> BrowseState {
        self.state
    }

    pub fn page(&self) -> usize {
        self.page
    }
}

pub(crate) struct PullRequestBrowser {
    repository_key: String,
    account: Account,
    generation: u64,
    pub state: BrowseState,
    pub load: LoadState,
    /// Every summary read so far, in the order the provider returned them.
    pub summaries: Vec<PullRequestSummary>,
    /// The page a Load more press would ask for.
    pub next_page: usize,
    /// The last page came back full, so another may exist.
    pub has_more: bool,
    /// A Load more read is outstanding, as opposed to a first-page read, which
    /// `load` already describes.
    pub appending: bool,
    /// Narrows what is already loaded. This is not a repository-wide search;
    /// GitHub's index endpoint has no text query.
    pub filter: String,
}

impl PullRequestBrowser {
    pub fn new(repository_key: String, account: Account) -> Self {
        Self {
            repository_key,
            account,
            generation: 0,
            state: BrowseState::default(),
            load: LoadState::Loading("Loading pull requests…".into()),
            summaries: Vec::new(),
            next_page: 1,
            has_more: false,
            appending: false,
            filter: String::new(),
        }
    }

    pub fn repository_key(&self) -> &str {
        &self.repository_key
    }

    /// Point the page at another repository. Returns whether anything changed,
    /// so the caller only spends a read when it did.
    pub fn retarget(&mut self, repository_key: String, account: Account) -> bool {
        if self.repository_key == repository_key && self.account == account {
            return false;
        }
        self.repository_key = repository_key;
        self.account = account;
        self.reset();
        true
    }

    pub fn set_state(&mut self, state: BrowseState) -> bool {
        if self.state == state {
            return false;
        }
        self.state = state;
        self.reset();
        true
    }

    /// Discard what is loaded and invalidate anything in flight. The filter
    /// survives: it describes what the reader is looking for, not which page
    /// they were on.
    fn reset(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.summaries.clear();
        self.next_page = 1;
        self.has_more = false;
        self.appending = false;
        self.load = LoadState::Loading("Loading pull requests…".into());
    }

    /// Begin a read. `append` asks for the next page and keeps what is on
    /// screen; otherwise the page reloads from the first one.
    pub fn begin_read(&mut self, append: bool) -> BrowseToken {
        if append {
            self.appending = true;
        } else {
            self.generation = self.generation.saturating_add(1);
            self.next_page = 1;
            self.appending = false;
            if self.summaries.is_empty() {
                self.load = LoadState::Loading("Loading pull requests…".into());
            }
        }
        BrowseToken {
            repository_key: self.repository_key.clone(),
            account: self.account.clone(),
            state: self.state,
            page: self.next_page,
            generation: self.generation,
        }
    }

    pub fn accepts(&self, token: &BrowseToken) -> bool {
        token.repository_key == self.repository_key
            && token.account == self.account
            && token.state == self.state
            && token.generation == self.generation
    }

    pub fn install(&mut self, page: PullRequestSummaryPage) {
        if page.page <= 1 {
            self.summaries.clear();
        }
        self.summaries.extend(page.summaries);
        self.next_page = page.page.saturating_add(1);
        self.has_more = page.has_more;
        self.appending = false;
        self.load = LoadState::Ready;
    }

    pub fn fail(&mut self, message: String) {
        self.appending = false;
        self.load = if self.summaries.is_empty() {
            LoadState::Error(message)
        } else {
            LoadState::Cached(message)
        };
    }

    pub fn set_filter(&mut self, filter: String) -> bool {
        if self.filter == filter {
            return false;
        }
        self.filter = filter;
        true
    }

    /// Which loaded rows the filter leaves, as positions rather than clones.
    /// The list renderer runs on every frame and looks each row up by position,
    /// so nothing here copies a summary to decide whether to paint it.
    ///
    /// The matched fields are the sidebar's, so the same typing finds the same
    /// pull request in both places.
    pub fn visible_indices(&self) -> Vec<usize> {
        let needle = self.filter.trim().to_lowercase();
        if needle.is_empty() {
            return (0..self.summaries.len()).collect();
        }
        self.summaries
            .iter()
            .enumerate()
            .filter(|(_, summary)| {
                format!(
                    "{} #{} {} {} {}",
                    summary.title,
                    summary.number,
                    summary.author,
                    summary.source_branch,
                    summary.target_branch
                )
                .to_lowercase()
                .contains(&needle)
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub fn summary(&self, index: usize) -> Option<&PullRequestSummary> {
        self.summaries.get(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(login: &str) -> Account {
        Account {
            host: "github.com".into(),
            login: login.into(),
        }
    }

    fn browser() -> PullRequestBrowser {
        PullRequestBrowser::new("github.com/acme/workspace".into(), account("ada"))
    }

    /// What the list would paint, in the order it would paint it.
    fn visible_numbers(browser: &PullRequestBrowser) -> Vec<u64> {
        browser
            .visible_indices()
            .into_iter()
            .map(|index| browser.summary(index).expect("a visible row exists").number)
            .collect()
    }

    fn summary(number: u64, title: &str, state: &str) -> PullRequestSummary {
        PullRequestSummary {
            number,
            title: title.into(),
            author: "ada".into(),
            source_branch: "feature".into(),
            target_branch: "main".into(),
            labels: Vec::new(),
            draft: false,
            state: state.into(),
            updated_at: "2026-09-12T12:00:00Z".into(),
            url: format!("https://github.com/acme/workspace/pull/{number}"),
        }
    }

    fn page(page: usize, summaries: Vec<PullRequestSummary>, has_more: bool) -> PullRequestSummaryPage {
        PullRequestSummaryPage {
            summaries,
            page,
            has_more,
        }
    }

    #[test]
    fn appending_keeps_order_and_advances_the_page() {
        let mut browser = browser();
        let first = browser.begin_read(false);
        assert_eq!(first.page(), 1);
        browser.install(page(1, vec![summary(9, "newest", "MERGED")], true));
        assert!(browser.has_more);

        let second = browser.begin_read(true);
        assert_eq!(second.page(), 2);
        assert!(browser.appending);
        browser.install(page(2, vec![summary(4, "older", "CLOSED")], false));

        let numbers = visible_numbers(&browser);
        assert_eq!(numbers, [9, 4]);
        assert_eq!(browser.next_page, 3);
        assert!(!browser.has_more && !browser.appending);
        assert!(matches!(browser.load, LoadState::Ready));
    }

    #[test]
    fn switching_state_clears_rows_and_refuses_the_reply_in_flight() {
        let mut browser = browser();
        let stale = browser.begin_read(false);
        browser.install(page(1, vec![summary(9, "open one", "OPEN")], true));

        assert!(browser.set_state(BrowseState::Closed));
        assert!(browser.summaries.is_empty());
        assert_eq!(browser.next_page, 1);
        assert!(!browser.accepts(&stale), "a superseded reply must be dropped");

        let fresh = browser.begin_read(false);
        assert_eq!(fresh.state(), BrowseState::Closed);
        assert!(browser.accepts(&fresh));
        assert!(!browser.set_state(BrowseState::Closed), "no read for a no-op");
    }

    #[test]
    fn retargeting_another_repository_refuses_the_previous_reply() {
        let mut browser = browser();
        let stale = browser.begin_read(false);
        browser.install(page(1, vec![summary(9, "theirs", "OPEN")], false));

        assert!(browser.retarget("github.com/acme/other".into(), account("ada")));
        assert!(!browser.accepts(&stale));
        assert!(browser.summaries.is_empty());
        assert!(!browser.retarget("github.com/acme/other".into(), account("ada")));
    }

    #[test]
    fn a_failed_first_page_errors_and_a_failed_later_page_keeps_what_is_loaded() {
        let mut browser = browser();
        browser.begin_read(false);
        browser.fail("GitHub read unavailable".into());
        assert!(matches!(browser.load, LoadState::Error(_)));

        browser.install(page(1, vec![summary(9, "kept", "OPEN")], true));
        browser.begin_read(true);
        browser.fail("GitHub read unavailable".into());
        assert!(matches!(browser.load, LoadState::Cached(_)));
        assert_eq!(visible_numbers(&browser), [9], "loaded rows survive a failure");
        assert!(!browser.appending);
    }

    #[test]
    fn the_filter_narrows_loaded_rows_by_title_number_author_and_branch() {
        let mut browser = browser();
        browser.begin_read(false);
        let mut other = summary(4, "Unrelated", "CLOSED");
        other.author = "grace".into();
        other.source_branch = "hotfix".into();
        browser.install(page(1, vec![summary(9, "Compact the chrome", "MERGED"), other], false));

        for (needle, expected) in [
            ("chrome", vec![9]),
            ("CHROME", vec![9]),
            ("#4", vec![4]),
            ("grace", vec![4]),
            ("hotfix", vec![4]),
            ("main", vec![9, 4]),
            ("nothing here", vec![]),
        ] {
            browser.set_filter(needle.into());
            let numbers = visible_numbers(&browser);
            assert_eq!(numbers, expected, "filtering by {needle}");
        }

        browser.set_filter("  ".into());
        assert_eq!(visible_numbers(&browser).len(), 2, "a blank filter narrows nothing");
    }

    #[test]
    fn the_filter_survives_a_state_switch() {
        let mut browser = browser();
        browser.set_filter("chrome".into());
        browser.set_state(BrowseState::All);
        assert_eq!(browser.filter, "chrome");
    }
}
