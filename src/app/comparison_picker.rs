use cibergit::{
    comparisons::{
        CommitInventory, CommitInventoryEntry, ComparisonRequest, InventoryAvailability,
    },
    domain::Revision,
    review::ReviewSession,
    workspace::{MAX_PERSISTED_COMPARISON_SESSIONS, PersistedComparisonSession},
};

pub fn remember_request_progress(
    saved: &mut Vec<PersistedComparisonSession>,
    request: ComparisonRequest,
    session: ReviewSession,
    local_file_load: Option<cibergit::comparisons::LocalFileLoadPlan>,
) {
    if matches!(request, ComparisonRequest::FullPullRequest) {
        return;
    }
    saved.retain(|entry| entry.request != request);
    saved.push(PersistedComparisonSession {
        request,
        session,
        local_file_load,
    });
    if saved.len() > MAX_PERSISTED_COMPARISON_SESSIONS {
        let excess = saved.len() - MAX_PERSISTED_COMPARISON_SESSIONS;
        saved.drain(..excess);
    }
}

pub fn saved_request_progress(
    saved: &[PersistedComparisonSession],
    request: &ComparisonRequest,
) -> Option<PersistedComparisonSession> {
    saved
        .iter()
        .find(|entry| entry.request == *request)
        .cloned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerMode {
    Full,
    Commit,
    Range,
    SinceReview,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestToken {
    pub repository_key: String,
    pub pull_request: u64,
    pub canonical_full_revision: Revision,
    pub generation: u64,
}

impl RequestToken {
    pub fn matches(
        &self,
        repository_key: &str,
        pull_request: u64,
        canonical_full_revision: &Revision,
        generation: u64,
    ) -> bool {
        self.repository_key == repository_key
            && self.pull_request == pull_request
            && self.canonical_full_revision == *canonical_full_revision
            && self.generation == generation
    }
}

#[derive(Clone, Debug)]
pub struct ComparisonPicker {
    pub request: ComparisonRequest,
    pub inventory: Option<CommitInventory>,
    pub inventory_loading: bool,
    pub expanded: bool,
    pub revision_details_expanded: bool,
    pub editing_mode: PickerMode,
    pub range_first: Option<usize>,
    pub range_last: Option<usize>,
    pub notice: Option<String>,
}

impl ComparisonPicker {
    pub fn new(request: ComparisonRequest) -> Self {
        let (range_first, range_last) = match &request {
            ComparisonRequest::CommitRange { .. } => (None, None),
            _ => (None, None),
        };
        Self {
            editing_mode: match &request {
                ComparisonRequest::FullPullRequest => PickerMode::Full,
                ComparisonRequest::Commit { .. } => PickerMode::Commit,
                ComparisonRequest::CommitRange { .. } => PickerMode::Range,
                ComparisonRequest::SinceLastReview { .. } => PickerMode::SinceReview,
            },
            request,
            inventory: None,
            inventory_loading: false,
            expanded: false,
            revision_details_expanded: false,
            range_first,
            range_last,
            notice: None,
        }
    }

    pub fn mode(&self) -> PickerMode {
        match self.request {
            ComparisonRequest::FullPullRequest => PickerMode::Full,
            ComparisonRequest::Commit { .. } => PickerMode::Commit,
            ComparisonRequest::CommitRange { .. } => PickerMode::Range,
            ComparisonRequest::SinceLastReview { .. } => PickerMode::SinceReview,
        }
    }

    pub fn install_inventory(&mut self, canonical: &Revision, inventory: CommitInventory) -> bool {
        if inventory.full_revision != *canonical {
            return false;
        }
        self.inventory_loading = false;
        self.notice = inventory.notice.clone();
        self.inventory = Some(inventory);
        self.sync_range_indices();
        true
    }

    pub fn inventory_ready(&self) -> bool {
        self.inventory
            .as_ref()
            .is_some_and(|inventory| inventory.availability == InventoryAvailability::Complete)
    }

    pub fn inventory_reason(&self) -> String {
        if self.inventory_loading {
            return "Loading commits…".into();
        }
        match &self.inventory {
            Some(inventory) if inventory.availability == InventoryAvailability::Complete => {
                format!("{} commits", inventory.commits.len())
            }
            Some(inventory) => inventory.notice.clone().unwrap_or_else(|| {
                "The commit list is incomplete; commit and range selection are disabled.".into()
            }),
            None => {
                "The commit list is unavailable; commit and range selection are disabled.".into()
            }
        }
    }

    pub fn commits(&self) -> &[CommitInventoryEntry] {
        self.inventory
            .as_ref()
            .map(|inventory| inventory.commits.as_slice())
            .unwrap_or_default()
    }

    pub fn choose_commit(&mut self, index: usize) -> Result<ComparisonRequest, String> {
        self.require_complete()?;
        let commit = self
            .commits()
            .get(index)
            .ok_or_else(|| "The selected commit is outside the fixed inventory.".to_owned())?;
        if commit.parent_shas.len() != 1 {
            return Err(
                "Root and merge commits cannot be selected as an individual comparison.".into(),
            );
        }
        let request = ComparisonRequest::Commit {
            sha: commit.sha.clone(),
        };
        self.expanded = false;
        self.range_first = None;
        self.range_last = None;
        Ok(request)
    }

    /// First activation fixes the start endpoint. The second activation fixes
    /// the end and returns a contiguous request; reversing endpoints is refused.
    pub fn choose_range_endpoint(
        &mut self,
        index: usize,
    ) -> Result<Option<ComparisonRequest>, String> {
        self.require_complete()?;
        if index >= self.commits().len() {
            return Err("The selected endpoint is outside the fixed inventory.".into());
        }
        if self.range_first.is_none() || self.range_last.is_some() {
            self.range_first = Some(index);
            self.range_last = None;
            self.notice = Some(
                "Range start selected. Choose the same or a later contiguous commit as the end."
                    .into(),
            );
            return Ok(None);
        }
        let first = self.range_first.expect("checked above");
        if index < first {
            return Err("The range end must not precede its start.".into());
        }
        let selected = &self.commits()[first..=index];
        if selected.iter().any(|commit| commit.parent_shas.len() != 1)
            || selected
                .windows(2)
                .any(|pair| pair[1].parent_shas[0] != pair[0].sha)
        {
            return Err(
                "The endpoints do not form a contiguous first-parent range without merges.".into(),
            );
        }
        let request = ComparisonRequest::CommitRange {
            first_sha: selected[0].sha.clone(),
            last_sha: selected.last().expect("non-empty range").sha.clone(),
        };
        self.range_last = Some(index);
        self.expanded = false;
        self.notice = None;
        Ok(Some(request))
    }

    pub fn select_request(&mut self, request: ComparisonRequest) {
        self.editing_mode = match &request {
            ComparisonRequest::FullPullRequest => PickerMode::Full,
            ComparisonRequest::Commit { .. } => PickerMode::Commit,
            ComparisonRequest::CommitRange { .. } => PickerMode::Range,
            ComparisonRequest::SinceLastReview { .. } => PickerMode::SinceReview,
        };
        self.request = request;
        self.sync_range_indices();
    }

    pub fn begin_endpoint_selection(&mut self, mode: PickerMode) -> Result<(), String> {
        if !matches!(mode, PickerMode::Commit | PickerMode::Range) {
            return Err("This mode has no commit endpoints.".into());
        }
        self.require_complete()?;
        self.editing_mode = mode;
        self.expanded = true;
        if mode == PickerMode::Range {
            self.range_first = None;
            self.range_last = None;
            self.notice = Some("Choose the first commit in the contiguous range.".into());
        }
        Ok(())
    }

    pub fn request_label(&self) -> String {
        match &self.request {
            ComparisonRequest::FullPullRequest => "Full pull request".into(),
            ComparisonRequest::Commit { sha } => format!("Commit {}", short_sha(sha)),
            ComparisonRequest::CommitRange {
                first_sha,
                last_sha,
            } => format!("Range {}…{}", short_sha(first_sha), short_sha(last_sha)),
            ComparisonRequest::SinceLastReview { .. } => "Since last review".into(),
        }
    }

    fn require_complete(&self) -> Result<(), String> {
        if self.inventory_ready() {
            Ok(())
        } else {
            Err(self.inventory_reason())
        }
    }

    fn sync_range_indices(&mut self) {
        let ComparisonRequest::CommitRange {
            first_sha,
            last_sha,
        } = &self.request
        else {
            return;
        };
        self.range_first = self
            .commits()
            .iter()
            .position(|commit| commit.sha == *first_sha);
        self.range_last = self
            .commits()
            .iter()
            .position(|commit| commit.sha == *last_sha);
    }
}

pub fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::{
        domain::{ChangedFile, Comparison},
        review::{ComparisonMetadata, ComparisonMode},
    };

    fn inventory() -> CommitInventory {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let c = "c".repeat(40);
        CommitInventory {
            full_revision: Revision {
                base_sha: "0".repeat(40),
                head_sha: c.clone(),
            },
            commits: vec![
                CommitInventoryEntry {
                    sha: a.clone(),
                    parent_shas: vec!["0".repeat(40)],
                    message_headline: "first".into(),
                    authored_at: "2026-01-01T00:00:00Z".into(),
                    committed_at: "2026-01-01T00:00:00Z".into(),
                },
                CommitInventoryEntry {
                    sha: b.clone(),
                    parent_shas: vec![a],
                    message_headline: "second".into(),
                    authored_at: "2026-01-02T00:00:00Z".into(),
                    committed_at: "2026-01-02T00:00:00Z".into(),
                },
                CommitInventoryEntry {
                    sha: c,
                    parent_shas: vec![b],
                    message_headline: "third".into(),
                    authored_at: "2026-01-03T00:00:00Z".into(),
                    committed_at: "2026-01-03T00:00:00Z".into(),
                },
            ],
            availability: InventoryAvailability::Complete,
            notice: None,
        }
    }

    fn progress_session(revision: Revision, path: &str, mode: ComparisonMode) -> ReviewSession {
        let comparison = Comparison {
            revision,
            files: vec![ChangedFile {
                path: path.into(),
                previous_path: None,
                raw_path: None,
                raw_previous_path: None,
                status: "modified".into(),
                additions: 1,
                deletions: 1,
                patch: Some("@@ -1 +1 @@\n-old\n+new\n".into()),
                patch_complete: true,
            }],
            complete: true,
            notice: None,
        };
        let mut session = ReviewSession::new(comparison.clone());
        session.select_comparison(
            comparison,
            ComparisonMetadata {
                mode,
                requested_mode: None,
                notice: None,
            },
        );
        session
    }

    #[test]
    fn individual_and_range_endpoints_use_the_complete_fixed_inventory() {
        let mut picker = ComparisonPicker::new(ComparisonRequest::FullPullRequest);
        let inventory = inventory();
        assert!(picker.install_inventory(&inventory.full_revision.clone(), inventory));
        let commit = picker.choose_commit(1).unwrap();
        assert!(matches!(commit, ComparisonRequest::Commit { .. }));
        assert!(picker.choose_range_endpoint(0).unwrap().is_none());
        let range = picker.choose_range_endpoint(2).unwrap().unwrap();
        assert!(matches!(range, ComparisonRequest::CommitRange { .. }));
    }

    #[test]
    fn incomplete_inventory_never_exposes_a_selectable_prefix() {
        let mut picker = ComparisonPicker::new(ComparisonRequest::FullPullRequest);
        let mut inventory = inventory();
        inventory.availability = InventoryAvailability::Incomplete;
        inventory.notice = Some("capped".into());
        let revision = inventory.full_revision.clone();
        picker.install_inventory(&revision, inventory);
        assert_eq!(picker.choose_commit(0).unwrap_err(), "capped");
        assert!(picker.choose_range_endpoint(0).is_err());
    }

    #[test]
    fn request_tokens_isolate_tabs_and_reject_stale_or_advanced_results() {
        let revision = inventory().full_revision;
        let token = RequestToken {
            repository_key: "github.com/acme/app/account".into(),
            pull_request: 7,
            canonical_full_revision: revision.clone(),
            generation: 41,
        };
        assert!(token.matches("github.com/acme/app/account", 7, &revision, 41));
        assert!(!token.matches("github.com/acme/app/account", 8, &revision, 41));
        assert!(!token.matches("github.com/acme/app/account", 7, &revision, 42));
        let advanced = Revision {
            base_sha: revision.base_sha.clone(),
            head_sha: "d".repeat(40),
        };
        assert!(!token.matches("github.com/acme/app/account", 7, &advanced, 41));
    }

    #[test]
    fn pending_inventory_survives_since_and_full_request_generation_changes() {
        let inventory = inventory();
        let canonical = inventory.full_revision.clone();
        let inventory_token = RequestToken {
            repository_key: "github.com/acme/app/account".into(),
            pull_request: 7,
            canonical_full_revision: canonical.clone(),
            generation: 51,
        };
        let mut picker = ComparisonPicker::new(ComparisonRequest::FullPullRequest);
        picker.inventory_loading = true;
        picker.select_request(ComparisonRequest::SinceLastReview {
            baseline: cibergit::comparisons::BaselineResolution::Unavailable(
                cibergit::comparisons::BaselineUnavailableReason::NoSubmittedReview,
            ),
        });
        picker.select_request(ComparisonRequest::FullPullRequest);

        // Selection requests use another generation. The inventory reply is
        // still accepted against its independent canonical/tab generation.
        assert!(inventory_token.matches("github.com/acme/app/account", 7, &canonical, 51,));
        assert!(picker.install_inventory(&canonical, inventory));
        assert!(!picker.inventory_loading);
        assert!(picker.inventory_ready());
    }

    #[test]
    fn returning_to_a_request_restores_its_own_viewed_and_scroll_progress() {
        let request_a = ComparisonRequest::Commit {
            sha: "a".repeat(40),
        };
        let mut session_a = progress_session(
            Revision {
                base_sha: "0".repeat(40),
                head_sha: "a".repeat(40),
            },
            "a.rs",
            ComparisonMode::Commit {
                sha: "a".repeat(40),
            },
        );
        session_a.mark_viewed("a.rs", true);
        session_a.set_scroll_position(41.0);

        let request_b = ComparisonRequest::CommitRange {
            first_sha: "a".repeat(40),
            last_sha: "b".repeat(40),
        };
        let mut session_b = progress_session(
            Revision {
                base_sha: "0".repeat(40),
                head_sha: "b".repeat(40),
            },
            "b.rs",
            ComparisonMode::CommitRange,
        );
        session_b.set_scroll_position(92.0);

        let mut saved = Vec::new();
        remember_request_progress(&mut saved, request_a.clone(), session_a, None);
        remember_request_progress(&mut saved, request_b, session_b, None);
        let restored = saved_request_progress(&saved, &request_a).unwrap().session;
        assert!(restored.is_viewed("a.rs"));
        assert_eq!(restored.scroll_position(), 41.0);
        assert!(!restored.is_viewed("b.rs"));
    }
}
