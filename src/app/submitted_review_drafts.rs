//! Private recovery storage for unsent submitted-review summary text.
//!
//! Records contain local draft history only. In particular, the durable source
//! deliberately cannot represent viewer capability or a prepared confirmation.

use anyhow::{Context as _, Result, bail};
use cibergit::domain::{ProviderCoordinates, PullRequestReview, Repository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CString, OsString, c_char, c_int, c_uint, c_void},
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::{
        ffi::OsStringExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};

const SCHEMA_VERSION: u32 = 1;
pub(super) const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_RECORD_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DRAFTS_PER_PULL_REQUEST: usize = 32;
const MAX_ROOT_ENTRIES: usize = 256;
const MAX_ROOT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IDENTITY_BYTES: usize = 512;
const LOCK_ATTEMPTS: usize = 25;
const LOCK_RETRY: Duration = Duration::from_millis(8);
// Darwin values. The native app is macOS-only; these calls fail closed and
// keep every locked operation relative to the accepted root descriptor.
const O_RDONLY: c_int = 0;
const O_RDWR: c_int = 2;
const O_NONBLOCK: c_int = 0x0000_0004;
const O_CREAT: c_int = 0x0000_0200;
const O_EXCL: c_int = 0x0000_0800;
const O_DIRECTORY: c_int = 0x0010_0000;
const O_CLOEXEC: c_int = 0x0100_0000;
const O_NOFOLLOW_ANY: c_int = 0x2000_0000;
const RENAME_SWAP: c_uint = 0x0000_0002;
const RENAME_EXCL: c_uint = 0x0000_0004;
const RENAME_NOFOLLOW_ANY: c_uint = 0x0000_0010;
const RECORD_PREFIX: &str = "v1-";
const RECORD_SUFFIX: &str = ".json";
const LOCK_NAME: &str = ".submitted-review-drafts.lock";

static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" {
    fn geteuid() -> u32;
    fn openat(fd: c_int, path: *const c_char, oflag: c_int, ...) -> c_int;
    fn renameatx_np(
        from_fd: c_int,
        from: *const c_char,
        to_fd: c_int,
        to: *const c_char,
        flags: c_uint,
    ) -> c_int;
    fn unlinkat(fd: c_int, path: *const c_char, flag: c_int) -> c_int;
    fn fdopendir(fd: c_int) -> *mut c_void;
    fn readdir(directory: *mut c_void) -> *mut DarwinDirent;
    fn closedir(directory: *mut c_void) -> c_int;
    fn __error() -> *mut c_int;
}

#[repr(C)]
struct DarwinDirent {
    d_ino: u64,
    d_seekoff: u64,
    d_reclen: u16,
    d_namlen: u16,
    d_type: u8,
    d_name: [c_char; 1024],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SubmittedSummaryDraft {
    pub review: PullRequestReview,
    pub body: String,
}

pub(super) fn same_review_coordinates(
    left: &ProviderCoordinates,
    right: &ProviderCoordinates,
) -> bool {
    left.provider.eq_ignore_ascii_case(&right.provider)
        && left.host.eq_ignore_ascii_case(&right.host)
        && left.owner.eq_ignore_ascii_case(&right.owner)
        && left.repository.eq_ignore_ascii_case(&right.repository)
        && left.pull_request == right.pull_request
        && left.remote_id == right.remote_id
}

pub(super) fn same_draft_history(
    left: &SubmittedSummaryDraft,
    right: &SubmittedSummaryDraft,
) -> bool {
    same_review_coordinates(&left.review.coordinates, &right.review.coordinates)
        && left.review.author == right.review.author
        && left.review.body == right.review.body
        && left.review.state == right.review.state
        && left.review.submitted_at == right.review.submitted_at
        && left.review.commit_sha == right.review.commit_sha
        && left.body == right.body
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct DraftSnapshot {
    /// `None` means that no record has ever existed. An empty durable record is
    /// retained as a tombstone with `Some(generation)` to prevent ABA.
    pub generation: Option<u64>,
    pub active_review: Option<ProviderCoordinates>,
    pub drafts: Vec<SubmittedSummaryDraft>,
}

#[derive(Clone, Debug)]
pub(super) struct SubmittedDraftSave {
    snapshot: DraftSnapshot,
    expected_generation: Option<u64>,
    operation_generation: u64,
}

impl SubmittedDraftSave {
    pub fn snapshot(&self) -> &DraftSnapshot {
        &self.snapshot
    }

    pub fn expected_generation(&self) -> Option<u64> {
        self.expected_generation
    }

    pub fn operation_generation(&self) -> u64 {
        self.operation_generation
    }
}

#[derive(Clone, Debug)]
pub(super) struct SubmittedDraftClear {
    captured: SubmittedSummaryDraft,
    expected_generation: Option<u64>,
    operation_generation: u64,
}

impl SubmittedDraftClear {
    pub fn captured(&self) -> &SubmittedSummaryDraft {
        &self.captured
    }

    pub fn expected_generation(&self) -> Option<u64> {
        self.expected_generation
    }

    pub fn operation_generation(&self) -> u64 {
        self.operation_generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SubmittedDraftCloseDisposition {
    Safe,
    Save,
    WaitForOperation,
    RefuseRecovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SubmittedDraftLoadPhase {
    Loading,
    Ready,
    Failed,
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SubmittedDraftActivity {
    Idle,
    Saving,
    Clearing,
}

#[derive(Clone, Debug)]
enum SubmittedDraftLoadState {
    Loading,
    Ready,
    Failed,
    Conflict { disk: DraftSnapshot },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SubmittedDraftLoadAttempt {
    generation: u64,
    edit_generation: u64,
}

impl SubmittedDraftLoadAttempt {
    pub fn generation(self) -> u64 {
        self.generation
    }

    pub fn edit_generation(self) -> u64 {
        self.edit_generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SubmittedDraftLoadCompletion {
    Restored { recovered: usize, save_queued: bool },
    Conflict,
    Failed,
}

/// Owns the durable-save protocol state independently of widgets and focus.
/// The editor supplies current text snapshots; this state records exactly one
/// active save/clear, one coalesced successor, and the last durable generation.
#[derive(Clone, Debug, Default)]
struct SubmittedDraftPersistence {
    durable: DraftSnapshot,
    in_flight: Option<SubmittedDraftSave>,
    pending: Option<DraftSnapshot>,
    pending_clear: Option<SubmittedSummaryDraft>,
    clear_in_flight: Option<SubmittedDraftClear>,
    close_after_save: bool,
    persistence_error: Option<String>,
    operation_generation: u64,
}

impl SubmittedDraftPersistence {
    fn queue_snapshot(&mut self, snapshot: DraftSnapshot) {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|active| active.snapshot.same_contents(&snapshot))
            || (self.in_flight.is_none() && self.durable.same_contents(&snapshot))
        {
            self.pending = None;
        } else {
            self.pending = Some(snapshot);
        }
    }

    fn start_next(&mut self, ready: bool) -> Option<SubmittedDraftSave> {
        if self.in_flight.is_some()
            || !ready
            || self.clear_in_flight.is_some()
            || self.pending_clear.is_some()
        {
            return None;
        }
        let snapshot = self.pending.take()?;
        self.operation_generation = self.operation_generation.saturating_add(1);
        let next = SubmittedDraftSave {
            snapshot,
            expected_generation: self.durable.generation,
            operation_generation: self.operation_generation,
        };
        self.in_flight = Some(next.clone());
        Some(next)
    }

    fn queue_clear(&mut self, captured: SubmittedSummaryDraft) {
        self.pending_clear = Some(captured);
    }

    fn is_current_durable(&self, ready: bool, current: &DraftSnapshot) -> bool {
        ready
            && self.in_flight.is_none()
            && self.pending.is_none()
            && self.clear_in_flight.is_none()
            && self.pending_clear.is_none()
            && self.persistence_error.is_none()
            && current.same_contents(&self.durable)
    }

    fn close_disposition(
        &self,
        ready: bool,
        has_recovered_drafts: bool,
        current: &DraftSnapshot,
    ) -> SubmittedDraftCloseDisposition {
        if self.clear_in_flight.is_some()
            || self.pending_clear.is_some()
            || self.in_flight.is_some()
        {
            SubmittedDraftCloseDisposition::WaitForOperation
        } else if !ready && has_recovered_drafts {
            SubmittedDraftCloseDisposition::RefuseRecovery
        } else if ready && !self.is_current_durable(ready, current) {
            SubmittedDraftCloseDisposition::Save
        } else {
            SubmittedDraftCloseDisposition::Safe
        }
    }

    fn complete_save(
        &mut self,
        operation_generation: u64,
        current: DraftSnapshot,
        result: Result<DraftSnapshot, String>,
    ) -> Option<Result<(), String>> {
        let active = self.in_flight.as_ref()?;
        if active.operation_generation != operation_generation {
            return None;
        }
        let active = self.in_flight.take()?;
        Some(match result {
            Ok(saved) if saved.same_contents(&active.snapshot) => {
                self.durable = saved;
                self.persistence_error = None;
                if self.pending.is_none() && !current.same_contents(&self.durable) {
                    self.queue_snapshot(current);
                }
                Ok(())
            }
            Ok(_) => {
                self.pending = None;
                Err(self.record_failure(
                    "Submitted-review draft was not made durable; text remains open and close was refused: Submitted-review draft save receipt did not match its exact snapshot."
                        .into(),
                ))
            }
            Err(error) => {
                self.pending = None;
                Err(self.record_failure(format!(
                    "Submitted-review draft was not made durable; text remains open and close was refused: {error}"
                )))
            }
        })
    }

    fn start_clear(
        &mut self,
        write_in_flight: bool,
    ) -> Option<Result<SubmittedDraftClear, String>> {
        if write_in_flight || self.clear_in_flight.is_some() || self.in_flight.is_some() {
            return None;
        }
        let captured = self.pending_clear.take()?;
        if !self
            .durable
            .drafts
            .iter()
            .any(|draft| same_draft_history(draft, &captured))
        {
            return Some(Err(self.record_failure(
                "Remote edit was acknowledged, but the durable local predecessor changed before clear. It was preserved and no automatic clear or replay was attempted."
                    .into(),
            )));
        }
        self.operation_generation = self.operation_generation.saturating_add(1);
        let clear = SubmittedDraftClear {
            captured,
            expected_generation: self.durable.generation,
            operation_generation: self.operation_generation,
        };
        self.clear_in_flight = Some(clear.clone());
        Some(Ok(clear))
    }

    fn complete_clear(
        &mut self,
        operation_generation: u64,
        result: Result<DraftSnapshot, String>,
    ) -> Option<Result<(SubmittedSummaryDraft, DraftSnapshot), String>> {
        let active = self.clear_in_flight.as_ref()?;
        if active.operation_generation != operation_generation {
            return None;
        }
        let active = self.clear_in_flight.take()?;
        Some(match result {
            Ok(cleared) => {
                // Any queued snapshot predates this exact tombstone receipt.
                // Rebuild from the editor's current text after applying the
                // clear so an old pending save cannot resurrect the capture.
                self.pending = None;
                self.durable = cleared.clone();
                self.persistence_error = None;
                Ok((active.captured, cleared))
            }
            Err(error) => Err(self.record_failure(format!(
                "Remote edit was acknowledged, but its exact local draft could not be durably cleared; newer or foreign text was preserved: {error}"
            ))),
        })
    }

    fn record_failure(&mut self, message: String) -> String {
        self.close_after_save = false;
        self.persistence_error = Some(message.clone());
        message
    }

    fn activity(&self) -> SubmittedDraftActivity {
        if self.clear_in_flight.is_some() || self.pending_clear.is_some() {
            SubmittedDraftActivity::Clearing
        } else if self.in_flight.is_some() || self.pending.is_some() {
            SubmittedDraftActivity::Saving
        } else {
            SubmittedDraftActivity::Idle
        }
    }
}

/// Owns the complete submitted-summary draft lifecycle. GPUI supplies current
/// text and schedules the returned save/clear jobs; it never mutates durable,
/// in-flight, generation, recovery, or close-barrier state directly.
#[derive(Clone, Debug)]
pub(super) struct SubmittedSummaryEditor {
    active_review: Option<ProviderCoordinates>,
    drafts: Vec<SubmittedSummaryDraft>,
    load_state: SubmittedDraftLoadState,
    load_generation: u64,
    edit_generation: u64,
    persistence: SubmittedDraftPersistence,
}

impl Default for SubmittedSummaryEditor {
    fn default() -> Self {
        Self {
            active_review: None,
            drafts: Vec::new(),
            load_state: SubmittedDraftLoadState::Loading,
            load_generation: 0,
            edit_generation: 0,
            persistence: SubmittedDraftPersistence::default(),
        }
    }
}

impl SubmittedSummaryEditor {
    pub fn active_draft(&self) -> Option<&SubmittedSummaryDraft> {
        let active = self.active_review.as_ref()?;
        self.drafts
            .iter()
            .find(|draft| same_review_coordinates(&draft.review.coordinates, active))
    }

    fn active_draft_mut(&mut self) -> Option<&mut SubmittedSummaryDraft> {
        let active = self.active_review.as_ref()?;
        self.drafts
            .iter_mut()
            .find(|draft| same_review_coordinates(&draft.review.coordinates, active))
    }

    pub fn draft_for(&self, coordinates: &ProviderCoordinates) -> Option<&SubmittedSummaryDraft> {
        self.drafts
            .iter()
            .find(|draft| same_review_coordinates(&draft.review.coordinates, coordinates))
    }

    pub fn is_active(&self, coordinates: &ProviderCoordinates) -> bool {
        self.active_review
            .as_ref()
            .is_some_and(|active| same_review_coordinates(active, coordinates))
    }

    #[cfg(test)]
    pub fn active_coordinates(&self) -> Option<&ProviderCoordinates> {
        self.active_review.as_ref()
    }

    pub fn drafts(&self) -> &[SubmittedSummaryDraft] {
        &self.drafts
    }

    pub fn store_active_body(&mut self, body: String) -> bool {
        if let Some(draft) = self.active_draft_mut()
            && draft.body != body
        {
            draft.body = body;
            self.edit_generation = self.edit_generation.saturating_add(1);
            return true;
        }
        false
    }

    /// Selects an exact review draft. Existing text is retained; a newer fresh
    /// source tuple replaces only the expected source used by preflight.
    pub fn begin(&mut self, fresh: PullRequestReview) -> (String, bool, bool) {
        let coordinates = fresh.coordinates.clone();
        let existing = self
            .drafts
            .iter_mut()
            .find(|draft| same_review_coordinates(&draft.review.coordinates, &coordinates));
        let (body, source_refreshed, resumed) = if let Some(draft) = existing {
            let source_refreshed = draft.review != fresh;
            draft.review = fresh;
            (draft.body.clone(), source_refreshed, true)
        } else {
            let body = fresh.body.clone();
            self.drafts.push(SubmittedSummaryDraft {
                review: fresh,
                body: body.clone(),
            });
            (body, false, false)
        };
        self.active_review = Some(coordinates);
        self.edit_generation = self.edit_generation.saturating_add(1);
        (body, source_refreshed, resumed)
    }

    fn clear(&mut self, coordinates: &ProviderCoordinates) {
        self.drafts
            .retain(|draft| !same_review_coordinates(&draft.review.coordinates, coordinates));
        if self
            .active_review
            .as_ref()
            .is_some_and(|active| same_review_coordinates(active, coordinates))
        {
            self.active_review = None;
        }
        self.edit_generation = self.edit_generation.saturating_add(1);
    }

    pub fn current_snapshot(&self) -> DraftSnapshot {
        DraftSnapshot {
            generation: self.persistence.durable.generation,
            active_review: self.active_review.clone(),
            drafts: self.drafts.clone(),
        }
    }

    pub fn load_phase(&self) -> SubmittedDraftLoadPhase {
        match self.load_state {
            SubmittedDraftLoadState::Loading => SubmittedDraftLoadPhase::Loading,
            SubmittedDraftLoadState::Ready => SubmittedDraftLoadPhase::Ready,
            SubmittedDraftLoadState::Failed => SubmittedDraftLoadPhase::Failed,
            SubmittedDraftLoadState::Conflict { .. } => SubmittedDraftLoadPhase::Conflict,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.load_phase() == SubmittedDraftLoadPhase::Ready
    }

    pub fn is_current_durable(&self) -> bool {
        self.persistence
            .is_current_durable(self.is_ready(), &self.current_snapshot())
    }

    pub fn close_after_save(&self) -> bool {
        self.persistence.close_after_save
    }

    pub fn persistence_error(&self) -> Option<&str> {
        self.persistence.persistence_error.as_deref()
    }

    pub fn activity(&self) -> SubmittedDraftActivity {
        self.persistence.activity()
    }

    pub fn has_active_operation(&self) -> bool {
        self.persistence.in_flight.is_some() || self.persistence.clear_in_flight.is_some()
    }

    pub fn has_clear_work(&self) -> bool {
        self.persistence.clear_in_flight.is_some() || self.persistence.pending_clear.is_some()
    }

    pub fn request_close(&mut self) -> SubmittedDraftCloseDisposition {
        let current = self.current_snapshot();
        let disposition =
            self.persistence
                .close_disposition(self.is_ready(), !self.drafts.is_empty(), &current);
        match disposition {
            SubmittedDraftCloseDisposition::WaitForOperation => {
                self.persistence.close_after_save = true;
            }
            SubmittedDraftCloseDisposition::Save => {
                self.persistence.close_after_save = true;
                self.persistence.persistence_error = None;
                self.persistence.queue_snapshot(current);
            }
            SubmittedDraftCloseDisposition::Safe
            | SubmittedDraftCloseDisposition::RefuseRecovery => {}
        }
        disposition
    }

    pub fn start_load(&mut self) -> SubmittedDraftLoadAttempt {
        self.load_generation = self.load_generation.saturating_add(1);
        self.load_state = SubmittedDraftLoadState::Loading;
        SubmittedDraftLoadAttempt {
            generation: self.load_generation,
            edit_generation: self.edit_generation,
        }
    }

    pub fn load_generation(&self) -> u64 {
        self.load_generation
    }

    pub fn edit_generation(&self) -> u64 {
        self.edit_generation
    }

    pub fn may_retry_load(&self) -> bool {
        !self.has_active_operation()
    }

    pub fn complete_load(
        &mut self,
        captured_edit_generation: u64,
        result: Result<DraftSnapshot, String>,
    ) -> SubmittedDraftLoadCompletion {
        let mut disk = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.load_state = SubmittedDraftLoadState::Failed;
                self.persistence.record_failure(format!(
                    "Submitted-review draft recovery failed; the original was preserved and editing is disabled: {error}"
                ));
                return SubmittedDraftLoadCompletion::Failed;
            }
        };
        let recovered = disk.drafts.len();
        // Restoring selection is history, not authorization. The user must
        // explicitly choose Edit against fresh Activity before an input opens.
        disk.active_review = None;
        if self.edit_generation == captured_edit_generation && self.drafts.is_empty() {
            self.active_review = None;
            self.drafts = disk.drafts.clone();
            self.persistence.durable = disk;
            self.load_state = SubmittedDraftLoadState::Ready;
            self.persistence.persistence_error = None;
            return SubmittedDraftLoadCompletion::Restored {
                recovered,
                save_queued: false,
            };
        }

        let same_review_conflict = self.drafts.iter().any(|local| {
            disk.drafts.iter().any(|saved| {
                same_review_coordinates(&saved.review.coordinates, &local.review.coordinates)
                    && !same_draft_history(saved, local)
            })
        });
        if same_review_conflict {
            self.load_state = SubmittedDraftLoadState::Conflict { disk };
            self.persistence.persistence_error = Some(
                "A saved draft for this same review loaded after local text. Both versions are preserved; choose which text to keep."
                    .into(),
            );
            return SubmittedDraftLoadCompletion::Conflict;
        }

        for saved in &disk.drafts {
            if !self.drafts.iter().any(|local| {
                same_review_coordinates(&local.review.coordinates, &saved.review.coordinates)
            }) {
                self.drafts.push(saved.clone());
            }
        }
        self.persistence.durable = disk;
        self.load_state = SubmittedDraftLoadState::Ready;
        self.persistence.persistence_error = None;
        self.queue_current();
        SubmittedDraftLoadCompletion::Restored {
            recovered,
            save_queued: self.persistence.pending.is_some(),
        }
    }

    pub fn retry_save(&mut self) -> bool {
        if !self.is_ready() || self.has_active_operation() || self.has_clear_work() {
            return false;
        }
        self.persistence.persistence_error = None;
        self.queue_current();
        true
    }

    pub fn queue_current(&mut self) {
        self.persistence.queue_snapshot(self.current_snapshot());
    }

    pub fn start_next_save(&mut self) -> Option<SubmittedDraftSave> {
        self.persistence.start_next(self.is_ready())
    }

    pub fn complete_save(
        &mut self,
        operation_generation: u64,
        result: Result<DraftSnapshot, String>,
    ) -> Option<Result<(), String>> {
        self.persistence
            .complete_save(operation_generation, self.current_snapshot(), result)
    }

    pub fn queue_clear(&mut self, captured: SubmittedSummaryDraft) {
        self.persistence.queue_clear(captured);
    }

    pub fn start_clear(
        &mut self,
        write_in_flight: bool,
    ) -> Option<Result<SubmittedDraftClear, String>> {
        self.persistence.start_clear(write_in_flight)
    }

    pub fn complete_clear(
        &mut self,
        operation_generation: u64,
        result: Result<DraftSnapshot, String>,
    ) -> Option<Result<bool, String>> {
        let completion = self
            .persistence
            .complete_clear(operation_generation, result)?;
        Some(completion.map(|(captured, _cleared)| {
            let unchanged = self
                .drafts
                .iter()
                .any(|draft| same_draft_history(draft, &captured));
            if unchanged {
                self.clear(&captured.review.coordinates);
            }
            if !self
                .current_snapshot()
                .same_contents(&self.persistence.durable)
            {
                self.queue_current();
            }
            unchanged
        }))
    }

    pub fn resolve_conflict_use_saved(&mut self) -> bool {
        let SubmittedDraftLoadState::Conflict { disk } = self.load_state.clone() else {
            return false;
        };
        let mut replaced_active = false;
        for saved in &disk.drafts {
            if let Some(position) = self.drafts.iter().position(|local| {
                same_review_coordinates(&local.review.coordinates, &saved.review.coordinates)
            }) {
                if !same_draft_history(&self.drafts[position], saved) {
                    replaced_active |= self.active_review.as_ref().is_some_and(|active| {
                        same_review_coordinates(active, &saved.review.coordinates)
                    });
                    self.drafts[position] = saved.clone();
                }
            } else {
                self.drafts.push(saved.clone());
            }
        }
        if replaced_active {
            self.active_review = None;
        }
        self.persistence.durable = disk;
        self.load_state = SubmittedDraftLoadState::Ready;
        self.persistence.persistence_error = None;
        self.queue_current();
        true
    }

    pub fn resolve_conflict_keep_current(&mut self) -> bool {
        let SubmittedDraftLoadState::Conflict { mut disk } = self.load_state.clone() else {
            return false;
        };
        disk.active_review = None;
        for saved in &disk.drafts {
            if !self.drafts.iter().any(|local| {
                same_review_coordinates(&local.review.coordinates, &saved.review.coordinates)
            }) {
                self.drafts.push(saved.clone());
            }
        }
        self.persistence.durable = disk;
        self.load_state = SubmittedDraftLoadState::Ready;
        self.persistence.persistence_error = None;
        self.queue_current();
        true
    }

    pub fn conflict(&self) -> Option<&DraftSnapshot> {
        match &self.load_state {
            SubmittedDraftLoadState::Conflict { disk } => Some(disk),
            _ => None,
        }
    }

    pub fn unavailable_reason(&self) -> Option<&'static str> {
        match self.load_state {
            SubmittedDraftLoadState::Loading => {
                Some("Local submitted-review drafts are still loading.")
            }
            SubmittedDraftLoadState::Conflict { .. } => {
                Some("Resolve the two preserved same-review draft versions first.")
            }
            SubmittedDraftLoadState::Failed => {
                Some("Local submitted-review draft recovery failed; retry it before editing.")
            }
            SubmittedDraftLoadState::Ready => None,
        }
    }

    #[cfg(test)]
    pub fn pending_snapshot(&self) -> Option<&DraftSnapshot> {
        self.persistence.pending.as_ref()
    }

    #[cfg(test)]
    pub fn durable_snapshot(&self) -> &DraftSnapshot {
        &self.persistence.durable
    }

    #[cfg(any(test, feature = "ui-smoke"))]
    pub fn mark_current_durable(&mut self, clear_selection: bool) {
        if clear_selection {
            self.active_review = None;
        }
        self.persistence.durable = self.current_snapshot();
        self.persistence.persistence_error = None;
    }

    #[cfg(test)]
    pub fn assume_loaded_for_test(&mut self) {
        self.load_state = SubmittedDraftLoadState::Ready;
    }
}

impl DraftSnapshot {
    pub fn same_contents(&self, other: &Self) -> bool {
        let active_matches = match (&self.active_review, &other.active_review) {
            (Some(left), Some(right)) => same_review_coordinates(left, right),
            (None, None) => true,
            _ => false,
        };
        active_matches
            && self.drafts.len() == other.drafts.len()
            && self
                .drafts
                .iter()
                .zip(&other.drafts)
                .all(|(left, right)| same_draft_history(left, right))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PullRequestIdentity {
    provider: String,
    host: String,
    account_host: String,
    account_login: String,
    owner: String,
    repository: String,
    pull_request: u64,
}

impl PullRequestIdentity {
    fn new(repository: &Repository, pull_request: u64) -> Result<Self> {
        if pull_request == 0 {
            bail!("submitted-review draft pull request must be nonzero");
        }
        Ok(Self {
            provider: "github".into(),
            host: canonical("repository host", &repository.host)?,
            account_host: canonical("account host", &repository.account.host)?,
            account_login: canonical("account login", &repository.account.login)?,
            owner: canonical("repository owner", &repository.owner)?,
            repository: canonical("repository name", &repository.name)?,
            pull_request,
        })
    }

    fn validate_coordinates(&self, coordinates: &ProviderCoordinates) -> Result<()> {
        if canonical("provider", &coordinates.provider)? != self.provider
            || canonical("review host", &coordinates.host)? != self.host
            || canonical("review owner", &coordinates.owner)? != self.owner
            || canonical("review repository", &coordinates.repository)? != self.repository
            || coordinates.pull_request != self.pull_request
        {
            bail!("submitted-review draft coordinates do not match their exact repository/PR");
        }
        validate_exact(
            "review ID",
            &coordinates.remote_id,
            MAX_IDENTITY_BYTES,
            false,
        )
    }

    fn filename(&self) -> String {
        let encoded = serde_json::to_vec(self).expect("draft identity is serializable");
        format!(
            "{RECORD_PREFIX}{}{RECORD_SUFFIX}",
            hex(&Sha256::digest(encoded))
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSource {
    remote_id: String,
    author: Option<String>,
    body: String,
    state: String,
    submitted_at: Option<String>,
    commit_sha: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDraft {
    source: StoredSource,
    body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftRecord {
    schema_version: u32,
    identity: PullRequestIdentity,
    generation: u64,
    active_review_id: Option<String>,
    drafts: Vec<StoredDraft>,
}

struct ReadRecord {
    record: DraftRecord,
    metadata: fs::Metadata,
}

struct AtomicWriteHooks<AfterCreate, BeforeInstall, AfterErrorSync> {
    after_create: AfterCreate,
    before_install: BeforeInstall,
    after_error_sync: AfterErrorSync,
}

impl DraftRecord {
    fn validate(&self, expected: &PullRequestIdentity) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            bail!(
                "submitted-review draft uses future or unsupported schema {}; original preserved",
                self.schema_version
            );
        }
        if &self.identity != expected {
            bail!("submitted-review draft identity mismatch; original preserved");
        }
        if self.generation == 0 {
            bail!("submitted-review draft has an invalid zero generation; original preserved");
        }
        if self.drafts.len() > MAX_DRAFTS_PER_PULL_REQUEST {
            bail!("submitted-review draft exceeds the 32-review bound; original preserved");
        }
        if let Some(active) = &self.active_review_id {
            validate_exact("active review ID", active, MAX_IDENTITY_BYTES, false)?;
            if !self
                .drafts
                .iter()
                .any(|draft| draft.source.remote_id == *active)
            {
                bail!("submitted-review draft active review is absent; original preserved");
            }
        }
        let mut ids = std::collections::HashSet::new();
        for draft in &self.drafts {
            validate_stored_draft(draft)?;
            if !ids.insert(draft.source.remote_id.as_str()) {
                bail!("submitted-review draft repeats a review ID; original preserved");
            }
        }
        Ok(())
    }

    fn into_snapshot(self) -> DraftSnapshot {
        let coordinates = |remote_id: String| ProviderCoordinates {
            provider: self.identity.provider.clone(),
            host: self.identity.host.clone(),
            owner: self.identity.owner.clone(),
            repository: self.identity.repository.clone(),
            pull_request: self.identity.pull_request,
            remote_id,
        };
        let active_review = self.active_review_id.map(&coordinates);
        let drafts = self
            .drafts
            .into_iter()
            .map(|draft| {
                let review = PullRequestReview {
                    coordinates: coordinates(draft.source.remote_id),
                    author: draft.source.author,
                    body: draft.source.body,
                    state: draft.source.state,
                    submitted_at: draft.source.submitted_at,
                    commit_sha: draft.source.commit_sha,
                    // Durable source history never grants fresh viewer authority.
                    edit_summary_capability: None,
                    dismissal_capability: None,
                    url: String::new(),
                };
                SubmittedSummaryDraft {
                    review,
                    body: draft.body,
                }
            })
            .collect();
        DraftSnapshot {
            generation: Some(self.generation),
            active_review,
            drafts,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct SubmittedReviewDraftStore {
    root: PathBuf,
}

impl SubmittedReviewDraftStore {
    pub fn new(data_root: PathBuf) -> Self {
        Self {
            root: data_root.join("submitted-review-drafts-v1"),
        }
    }

    pub fn load(&self, repository: &Repository, pull_request: u64) -> Result<DraftSnapshot> {
        let identity = PullRequestIdentity::new(repository, pull_request)?;
        if !self.ensure_root(false)? {
            return Ok(DraftSnapshot::default());
        }
        self.with_lock(false, |root| {
            Ok(self
                .read_record(root, &identity)?
                .map(|record| record.record.into_snapshot())
                .unwrap_or_default())
        })
    }

    pub fn save_if_current(
        &self,
        repository: &Repository,
        pull_request: u64,
        expected_generation: Option<u64>,
        active_review: Option<&ProviderCoordinates>,
        drafts: &[SubmittedSummaryDraft],
    ) -> Result<DraftSnapshot> {
        let identity = PullRequestIdentity::new(repository, pull_request)?;
        let record = record_from_memory(&identity, expected_generation, active_review, drafts)?;
        let bytes = serde_json::to_vec(&record).context("encode submitted-review draft")?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            bail!("submitted-review draft exceeds its 2 MiB record bound; text remains unsaved");
        }
        self.with_lock(true, |root| {
            let current = self.read_record(root, &identity)?;
            if current.as_ref().map(|read| read.record.generation) != expected_generation {
                bail!(
                    "durable submitted-review draft changed in another editor; stale save refused"
                );
            }
            self.atomic_write(
                root,
                &identity.filename(),
                current.as_ref().map(|read| &read.metadata),
                &bytes,
            )?;
            Ok(record.into_snapshot())
        })
    }

    pub fn clear_if_current(
        &self,
        repository: &Repository,
        pull_request: u64,
        expected_generation: Option<u64>,
        review: &ProviderCoordinates,
        expected_body: &str,
    ) -> Result<DraftSnapshot> {
        let identity = PullRequestIdentity::new(repository, pull_request)?;
        identity.validate_coordinates(review)?;
        validate_exact("submitted-review body", expected_body, MAX_BODY_BYTES, true)?;
        self.with_lock(true, |root| {
            let current = self.read_record(root, &identity)?;
            if current.as_ref().map(|read| read.record.generation) != expected_generation {
                bail!(
                    "durable submitted-review draft changed in another editor; stale clear refused"
                );
            }
            let current = current.context(
                "durable submitted-review draft is absent; exact acknowledged text was not cleared",
            )?;
            let metadata = current.metadata;
            let mut current = current.record;
            let position = current
                .drafts
                .iter()
                .position(|draft| {
                    draft.source.remote_id == review.remote_id && draft.body == expected_body
                })
                .context(
                    "durable submitted-review body differs from the acknowledged text; clear refused",
                )?;
            current.drafts.remove(position);
            if current.active_review_id.as_deref() == Some(&review.remote_id) {
                current.active_review_id = None;
            }
            current.generation = current
                .generation
                .checked_add(1)
                .context("submitted-review draft generation exhausted")?;
            // Even an empty record is written durably. It is the tombstone that
            // prevents a pre-clear writer from succeeding after delete/recreate.
            let bytes = serde_json::to_vec(&current).context("encode submitted-review tombstone")?;
            self.atomic_write(root, &identity.filename(), Some(&metadata), &bytes)?;
            Ok(current.into_snapshot())
        })
    }

    fn read_record(
        &self,
        root: &File,
        identity: &PullRequestIdentity,
    ) -> Result<Option<ReadRecord>> {
        let Some((bytes, metadata)) =
            read_bounded_private_at(root, &identity.filename(), MAX_RECORD_BYTES)?
        else {
            return Ok(None);
        };
        let record: DraftRecord = serde_json::from_slice(&bytes)
            .context("submitted-review draft is corrupt; original preserved")?;
        record.validate(identity)?;
        Ok(Some(ReadRecord { record, metadata }))
    }

    fn with_lock<T>(
        &self,
        create_root: bool,
        operation: impl FnOnce(&File) -> Result<T>,
    ) -> Result<T> {
        self.with_lock_after(create_root, |_| Ok(()), operation)
    }

    fn with_lock_after<T>(
        &self,
        create_root: bool,
        after_lock: impl FnOnce(&File) -> Result<()>,
        operation: impl FnOnce(&File) -> Result<T>,
    ) -> Result<T> {
        if !self.ensure_root(create_root)? {
            bail!("submitted-review draft root is absent");
        }
        let root_descriptor = open_private_directory(&self.root)?;
        let root_lock = root_descriptor
            .try_clone()
            .context("clone submitted-review draft root descriptor for stable lock")?;
        let mut root_guard = AdvisoryLock::acquire(root_lock)?;
        let result = (|| {
            validate_directory_descriptor(&self.root, &root_descriptor)?;
            let before_lock = root_occupancy(&root_descriptor)?;
            if !before_lock.lock_present && before_lock.entries == MAX_ROOT_ENTRIES {
                bail!(
                    "submitted-review draft root cannot admit its persistent lock within the 256-entry bound"
                );
            }
            let lock_file = open_private_file_at(&root_descriptor, LOCK_NAME, true, 0)?
                .expect("create=true always returns a submitted-review draft lock descriptor");
            let mut guard = AdvisoryLock::acquire_immediate(lock_file)?;
            let operation_result = (|| {
                guard.validate_at(&root_descriptor)?;
                let held = root_occupancy(&root_descriptor)?;
                if !held.lock_present {
                    bail!("submitted-review draft lock disappeared after acquisition");
                }
                after_lock(&root_descriptor).and_then(|()| operation(&root_descriptor))
            })();
            let lock_check = guard.validate_at(&root_descriptor);
            let unlock = guard.unlock();
            match (operation_result, lock_check, unlock) {
                (Ok(value), Ok(()), Ok(())) => Ok(value),
                (Err(error), _, _) => Err(error),
                (Ok(_), Err(error), _) => Err(error),
                (Ok(_), Ok(()), Err(error)) => {
                    Err(error).context("unlock submitted-review draft entry lock")
                }
            }
        })();
        let root_check = validate_directory_descriptor(&self.root, &root_descriptor);
        let root_unlock = root_guard.unlock();
        match (result, root_check, root_unlock) {
            (Ok(value), Ok(()), Ok(())) => Ok(value),
            (Err(error), _, _) => Err(error),
            (Ok(_), Err(error), _) => Err(error),
            (Ok(_), Ok(()), Err(error)) => {
                Err(error).context("unlock submitted-review draft root lock")
            }
        }
    }

    fn ensure_root(&self, create: bool) -> Result<bool> {
        match fs::symlink_metadata(&self.root) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound && !create => return Ok(false),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                match builder.create(&self.root) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(error).context("create submitted-review draft root");
                    }
                }
            }
            Err(error) => return Err(error).context("inspect submitted-review draft root"),
        }
        validate_private_directory(&self.root)?;
        Ok(true)
    }

    fn atomic_write(
        &self,
        root: &File,
        name: &str,
        expected_target: Option<&fs::Metadata>,
        bytes: &[u8],
    ) -> Result<()> {
        self.atomic_write_after_check(
            root,
            name,
            expected_target,
            bytes,
            AtomicWriteHooks {
                after_create: |_: &File, _: &str| Ok(()),
                before_install: || Ok(()),
                after_error_sync: || Ok(()),
            },
        )
    }

    fn atomic_write_after_check(
        &self,
        root: &File,
        name: &str,
        expected_target: Option<&fs::Metadata>,
        bytes: &[u8],
        hooks: AtomicWriteHooks<
            impl FnOnce(&File, &str) -> Result<()>,
            impl FnOnce() -> Result<()>,
            impl FnOnce() -> Result<()>,
        >,
    ) -> Result<()> {
        let AtomicWriteHooks {
            after_create,
            before_install,
            after_error_sync,
        } = hooks;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            bail!("submitted-review draft atomic write exceeds its record bound");
        }
        let occupancy = root_occupancy(root)?;
        if occupancy.entries == MAX_ROOT_ENTRIES
            || occupancy
                .bytes
                .checked_add(bytes.len() as u64)
                .is_none_or(|peak| peak > MAX_ROOT_BYTES)
        {
            bail!(
                "submitted-review draft root cannot admit an atomic temp within its configured peak bounds"
            );
        }
        let current_target = open_private_file_at(root, name, false, MAX_RECORD_BYTES)?;
        match (expected_target, current_target.as_ref()) {
            (Some(expected), Some(current)) if same_snapshot(expected, &current.metadata()?) => {}
            (None, None) => {}
            _ => bail!(
                "submitted-review draft target changed after its CAS read; original preserved"
            ),
        }
        let nonce = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
        let temporary = format!(".tmp-{}-{nonce}", std::process::id());
        let mut created_identity = None;
        let mut directory_mutated = false;
        let result = (|| {
            let mut file = create_private_file_at(root, &temporary)?;
            directory_mutated = true;
            after_create(root, &temporary)?;
            let metadata = file
                .metadata()
                .context("capture submitted-review draft temp identity")?;
            created_identity = Some((metadata.dev(), metadata.ino()));
            validate_open_file_at(root, &temporary, &file, None, MAX_RECORD_BYTES)?;
            file.write_all(bytes)
                .context("write submitted-review draft temp")?;
            file.sync_all()
                .context("fsync submitted-review draft temp")?;
            validate_open_file_at(root, &temporary, &file, Some(&metadata), MAX_RECORD_BYTES)?;
            before_install()?;
            if let Some(expected) = expected_target {
                exchange_at(root, &temporary, name)?;
                directory_mutated = true;
                let displaced = open_private_file_at(root, &temporary, false, MAX_RECORD_BYTES)?
                    .context("displaced submitted-review draft target disappeared")?;
                let displaced_metadata = displaced.metadata()?;
                if !same_snapshot(expected, &displaced_metadata) {
                    let rollback_name =
                        open_private_file_at(root, &temporary, false, MAX_RECORD_BYTES)?
                            .context("displaced target disappeared before rollback")?;
                    if !same_snapshot(&displaced_metadata, &rollback_name.metadata()?) {
                        bail!(
                            "displaced submitted-review draft changed before rollback; preserved for recovery"
                        );
                    }
                    exchange_at(root, &temporary, name).context(
                        "restore submitted-review draft target after conditional install failed",
                    )?;
                    directory_mutated = true;
                    bail!(
                        "submitted-review draft target changed before conditional install; replacement preserved"
                    );
                }
                directory_mutated |= remove_exact_temp_at(
                    root,
                    &temporary,
                    displaced_metadata.dev(),
                    displaced_metadata.ino(),
                )?;
            } else {
                install_exclusive_at(root, &temporary, name)?;
                directory_mutated = true;
            }
            validate_open_file_at(root, name, &file, Some(&metadata), MAX_RECORD_BYTES)?;
            root.sync_all()
                .context("fsync submitted-review draft root descriptor")
        })();
        let Err(error) = result else {
            return Ok(());
        };
        let mut recovery_failures = Vec::new();
        if let Some((device, inode)) = created_identity {
            match remove_exact_temp_at(root, &temporary, device, inode) {
                Ok(removed) => directory_mutated |= removed,
                Err(cleanup_error) => recovery_failures
                    .push(format!("exact temporary cleanup failed: {cleanup_error:#}")),
            }
        }
        if directory_mutated {
            match root
                .sync_all()
                .context("fsync submitted-review draft root after failed atomic install")
            {
                Ok(()) => {
                    if let Err(observer_error) = after_error_sync() {
                        recovery_failures.push(format!(
                            "post-recovery durability check failed: {observer_error:#}"
                        ));
                    }
                }
                Err(sync_error) => recovery_failures.push(format!("{sync_error:#}")),
            }
        }
        if recovery_failures.is_empty() {
            Err(error)
        } else {
            Err(error).context(format!(
                "submitted-review draft recovery also failed: {}",
                recovery_failures.join("; ")
            ))
        }
    }
}

fn record_from_memory(
    identity: &PullRequestIdentity,
    expected_generation: Option<u64>,
    active_review: Option<&ProviderCoordinates>,
    drafts: &[SubmittedSummaryDraft],
) -> Result<DraftRecord> {
    if drafts.len() > MAX_DRAFTS_PER_PULL_REQUEST {
        bail!("submitted-review draft exceeds its 32-review bound; text remains unsaved");
    }
    let mut ids = std::collections::HashSet::new();
    let stored = drafts
        .iter()
        .map(|draft| {
            identity.validate_coordinates(&draft.review.coordinates)?;
            validate_exact(
                "review ID",
                &draft.review.coordinates.remote_id,
                MAX_IDENTITY_BYTES,
                false,
            )?;
            validate_exact("submitted-review body", &draft.body, MAX_BODY_BYTES, true)?;
            validate_exact(
                "source review body",
                &draft.review.body,
                MAX_BODY_BYTES,
                true,
            )?;
            if !ids.insert(draft.review.coordinates.remote_id.as_str()) {
                bail!("submitted-review draft repeats a review ID");
            }
            let source = StoredSource {
                remote_id: draft.review.coordinates.remote_id.clone(),
                author: draft.review.author.clone(),
                body: draft.review.body.clone(),
                state: draft.review.state.clone(),
                submitted_at: draft.review.submitted_at.clone(),
                commit_sha: draft.review.commit_sha.clone(),
            };
            let value = StoredDraft {
                source,
                body: draft.body.clone(),
            };
            validate_stored_draft(&value)?;
            Ok(value)
        })
        .collect::<Result<Vec<_>>>()?;
    let active_review_id = active_review
        .map(|active| {
            identity.validate_coordinates(active)?;
            if !stored
                .iter()
                .any(|draft| draft.source.remote_id == active.remote_id)
            {
                bail!("active submitted-review draft is absent from the saved collection");
            }
            Ok(active.remote_id.clone())
        })
        .transpose()?;
    Ok(DraftRecord {
        schema_version: SCHEMA_VERSION,
        identity: identity.clone(),
        generation: expected_generation
            .unwrap_or(0)
            .checked_add(1)
            .context("submitted-review draft generation exhausted")?,
        active_review_id,
        drafts: stored,
    })
}

fn validate_stored_draft(draft: &StoredDraft) -> Result<()> {
    validate_exact(
        "review ID",
        &draft.source.remote_id,
        MAX_IDENTITY_BYTES,
        false,
    )?;
    validate_exact(
        "source author",
        draft.source.author.as_deref().unwrap_or(""),
        MAX_IDENTITY_BYTES,
        true,
    )?;
    validate_exact(
        "source review body",
        &draft.source.body,
        MAX_BODY_BYTES,
        true,
    )?;
    validate_exact(
        "source state",
        &draft.source.state,
        MAX_IDENTITY_BYTES,
        false,
    )?;
    validate_exact(
        "source submission time",
        draft.source.submitted_at.as_deref().unwrap_or(""),
        MAX_IDENTITY_BYTES,
        true,
    )?;
    validate_exact(
        "source commit",
        draft.source.commit_sha.as_deref().unwrap_or(""),
        MAX_IDENTITY_BYTES,
        true,
    )?;
    validate_exact("submitted-review body", &draft.body, MAX_BODY_BYTES, true)
}

fn canonical(label: &str, value: &str) -> Result<String> {
    validate_exact(label, value, MAX_IDENTITY_BYTES, false)?;
    if !value.is_ascii() {
        bail!("{label} must use its canonical ASCII identity");
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_exact(label: &str, value: &str, max: usize, allow_empty: bool) -> Result<()> {
    if (!allow_empty && value.is_empty())
        || value.len() > max
        || value.contains('\0')
        || (max == MAX_IDENTITY_BYTES && value.chars().any(char::is_control))
    {
        bail!("invalid {label}");
    }
    Ok(())
}

fn read_bounded_private_at(
    root: &File,
    name: &str,
    max: u64,
) -> Result<Option<(Vec<u8>, fs::Metadata)>> {
    let Some(mut file) = open_private_file_at(root, name, false, max)? else {
        return Ok(None);
    };
    let initial = file
        .metadata()
        .context("inspect initial submitted-review draft descriptor")?;
    let mut bytes = Vec::with_capacity(initial.len().min(max) as usize);
    Read::by_ref(&mut file)
        .take(max + 1)
        .read_to_end(&mut bytes)
        .context("read submitted-review draft record")?;
    if bytes.len() as u64 > max {
        bail!("submitted-review draft record exceeds its 2 MiB bound; original preserved");
    }
    let final_descriptor = file
        .metadata()
        .context("post-check submitted-review draft descriptor")?;
    validate_metadata(&final_descriptor, max)?;
    let current = open_private_file_at(root, name, false, max)?
        .context("submitted-review draft record disappeared during bounded read")?;
    let current_descriptor = current
        .metadata()
        .context("post-check current submitted-review draft descriptor")?;
    if !same_snapshot(&initial, &final_descriptor) || !same_snapshot(&initial, &current_descriptor)
    {
        bail!("submitted-review draft record changed during bounded read; original preserved");
    }
    Ok(Some((bytes, final_descriptor)))
}

fn c_name(name: &str) -> Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        bail!("invalid submitted-review draft root entry name");
    }
    CString::new(name).context("submitted-review draft entry name contains NUL")
}

fn open_private_file_at(root: &File, name: &str, create: bool, max: u64) -> Result<Option<File>> {
    let name = c_name(name)?;
    let mut flags =
        if create { O_RDWR } else { O_RDONLY } | O_NONBLOCK | O_CLOEXEC | O_NOFOLLOW_ANY;
    if create {
        flags |= O_CREAT;
    }
    let descriptor = unsafe { openat(root.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if !create && error.kind() == ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).context("open private submitted-review draft entry relative to root");
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    validate_metadata(&file.metadata()?, max)?;
    Ok(Some(file))
}

fn create_private_file_at(root: &File, name: &str) -> Result<File> {
    let name = c_name(name)?;
    let descriptor = unsafe {
        openat(
            root.as_raw_fd(),
            name.as_ptr(),
            O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW_ANY,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .context("create private submitted-review draft temp relative to root");
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    Ok(file)
}

fn validate_open_file_at(
    root: &File,
    name: &str,
    file: &File,
    expected: Option<&fs::Metadata>,
    max: u64,
) -> Result<()> {
    let descriptor = file
        .metadata()
        .context("inspect submitted-review draft descriptor")?;
    validate_metadata(&descriptor, max)?;
    let current = open_private_file_at(root, name, false, max)?
        .context("submitted-review draft entry disappeared")?;
    let current = current.metadata()?;
    if !same_identity(&descriptor, &current)
        || expected.is_some_and(|expected| !same_identity(expected, &descriptor))
    {
        bail!("submitted-review draft descriptor identity mismatch");
    }
    Ok(())
}

fn validate_metadata(metadata: &fs::Metadata, max: u64) -> Result<()> {
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("refuse non-regular submitted-review draft entry");
    }
    if metadata.nlink() != 1 {
        bail!("refuse hard-linked submitted-review draft entry");
    }
    // SAFETY: geteuid has no preconditions and retains no pointers.
    if metadata.uid() != unsafe { geteuid() } {
        bail!("refuse submitted-review draft entry owned by another user");
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("refuse submitted-review draft entry with non-private permissions");
    }
    if metadata.len() > max {
        bail!("refuse oversized submitted-review draft entry");
    }
    Ok(())
}

fn validate_private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("inspect submitted-review draft root")?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!("refuse non-directory submitted-review draft root");
    }
    // SAFETY: geteuid has no preconditions and retains no pointers.
    if metadata.uid() != unsafe { geteuid() } {
        bail!("refuse submitted-review draft root owned by another user");
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("refuse submitted-review draft root with non-private permissions");
    }
    Ok(())
}

fn open_private_directory(path: &Path) -> Result<File> {
    let initial = fs::symlink_metadata(path).context("inspect submitted-review draft root")?;
    validate_private_directory(path)?;
    let canonical = fs::canonicalize(path).context("canonicalize submitted-review draft root")?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW_ANY)
        .open(&canonical)
        .context("open submitted-review draft root descriptor without following links")?;
    validate_directory_descriptor(path, &file)?;
    let descriptor = file
        .metadata()
        .context("inspect submitted-review draft root descriptor")?;
    if !same_identity(&initial, &descriptor) {
        bail!("submitted-review draft root changed while opening");
    }
    Ok(file)
}

fn validate_directory_descriptor(path: &Path, descriptor: &File) -> Result<()> {
    let opened = descriptor
        .metadata()
        .context("inspect submitted-review draft root descriptor")?;
    let current = fs::symlink_metadata(path).context("reinspect submitted-review draft root")?;
    validate_private_directory(path)?;
    if !same_identity(&opened, &current) {
        bail!("submitted-review draft root identity changed");
    }
    Ok(())
}

fn same_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn same_snapshot(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_identity(left, right)
        && left.len() == right.len()
        && left.nlink() == right.nlink()
        && left.uid() == right.uid()
        && left.mode() == right.mode()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
}

fn rename_with_flags(root: &File, from: &str, to: &str, flags: c_uint) -> Result<()> {
    let from = c_name(from)?;
    let to = c_name(to)?;
    if unsafe {
        renameatx_np(
            root.as_raw_fd(),
            from.as_ptr(),
            root.as_raw_fd(),
            to.as_ptr(),
            flags | RENAME_NOFOLLOW_ANY,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .context("install submitted-review draft record relative to root");
    }
    Ok(())
}

fn exchange_at(root: &File, from: &str, to: &str) -> Result<()> {
    rename_with_flags(root, from, to, RENAME_SWAP)
}

fn install_exclusive_at(root: &File, from: &str, to: &str) -> Result<()> {
    rename_with_flags(root, from, to, RENAME_EXCL)
}

fn remove_exact_temp_at(root: &File, name: &str, device: u64, inode: u64) -> Result<bool> {
    let Some(file) = open_private_file_at(root, name, false, MAX_RECORD_BYTES)? else {
        return Ok(false);
    };
    let metadata = file
        .metadata()
        .context("inspect submitted-review draft temp for cleanup")?;
    if metadata.dev() != device || metadata.ino() != inode || metadata.nlink() != 1 {
        bail!("submitted-review draft temp identity changed; cleanup refused");
    }
    let name = c_name(name)?;
    if unsafe { unlinkat(root.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("remove exact submitted-review draft temp relative to root");
    }
    Ok(true)
}

#[derive(Clone, Copy, Debug)]
struct RootOccupancy {
    entries: usize,
    bytes: u64,
    lock_present: bool,
}

struct DirectoryStream(*mut c_void);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            closedir(self.0);
        }
    }
}

fn root_occupancy(root: &File) -> Result<RootOccupancy> {
    let current = CString::new(".").expect("static directory name");
    let directory = unsafe {
        openat(
            root.as_raw_fd(),
            current.as_ptr(),
            O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW_ANY,
        )
    };
    if directory < 0 {
        return Err(std::io::Error::last_os_error())
            .context("reopen submitted-review draft root descriptor for bounded scan");
    }
    let stream = unsafe { fdopendir(directory) };
    if stream.is_null() {
        unsafe {
            drop(File::from_raw_fd(directory));
        }
        return Err(std::io::Error::last_os_error())
            .context("open submitted-review draft root directory stream");
    }
    let stream = DirectoryStream(stream);
    let mut occupancy = RootOccupancy {
        entries: 0,
        bytes: 0,
        lock_present: false,
    };
    loop {
        unsafe {
            *__error() = 0;
        }
        let entry = unsafe { readdir(stream.0) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error().unwrap_or(0) == 0 {
                return Ok(occupancy);
            }
            return Err(error).context("scan submitted-review draft root descriptor");
        }
        let entry = unsafe { &*entry };
        let length = usize::from(entry.d_namlen).min(entry.d_name.len());
        let name =
            unsafe { std::slice::from_raw_parts(entry.d_name.as_ptr().cast::<u8>(), length) };
        if name == b"." || name == b".." {
            continue;
        }
        occupancy.entries = occupancy.entries.saturating_add(1);
        if occupancy.entries > MAX_ROOT_ENTRIES {
            bail!("submitted-review draft root exceeds its 256-entry bound");
        }
        let name = OsString::from_vec(name.to_vec());
        let name = name.into_string().map_err(|_| {
            anyhow::anyhow!("submitted-review draft root contains a non-UTF-8 entry")
        })?;
        let file = open_private_file_at(root, &name, false, u64::MAX)?
            .context("submitted-review draft root entry disappeared during bounded scan")?;
        let metadata = file.metadata()?;
        occupancy.bytes = occupancy
            .bytes
            .checked_add(metadata.len())
            .context("submitted-review draft root byte count overflow")?;
        if occupancy.bytes > MAX_ROOT_BYTES {
            bail!("submitted-review draft root exceeds its 64 MiB bound");
        }
        occupancy.lock_present |= name == LOCK_NAME;
    }
}

struct AdvisoryLock {
    file: Option<File>,
}

impl AdvisoryLock {
    fn acquire(file: File) -> Result<Self> {
        for attempt in 0..LOCK_ATTEMPTS {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file: Some(file) }),
                Err(fs::TryLockError::WouldBlock) if attempt + 1 < LOCK_ATTEMPTS => {
                    thread::sleep(LOCK_RETRY);
                }
                Err(fs::TryLockError::WouldBlock) => {
                    bail!("submitted-review draft lock contention exceeded 200 ms");
                }
                Err(fs::TryLockError::Error(error)) => {
                    return Err(error).context("lock submitted-review draft store");
                }
            }
        }
        unreachable!("bounded lock loop always returns")
    }

    fn acquire_immediate(file: File) -> Result<Self> {
        match file.try_lock() {
            Ok(()) => Ok(Self { file: Some(file) }),
            Err(fs::TryLockError::WouldBlock) => {
                bail!("submitted-review draft entry lock is held by another process")
            }
            Err(fs::TryLockError::Error(error)) => {
                Err(error).context("lock submitted-review draft entry")
            }
        }
    }

    fn validate_at(&self, root: &File) -> Result<()> {
        let file = self
            .file
            .as_ref()
            .context("submitted-review draft lock descriptor is absent")?;
        validate_open_file_at(root, LOCK_NAME, file, None, 0)
    }

    fn unlock(&mut self) -> Result<()> {
        if let Some(file) = self.file.take() {
            file.unlock()
                .context("explicitly unlock submitted-review draft store")?;
        }
        Ok(())
    }
}

impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::domain::{Account, SubmittedReviewEditCapability};
    use tempfile::tempdir;

    fn repository(login: &str) -> Repository {
        Repository {
            host: "GitHub.COM".into(),
            owner: "Octo".into(),
            name: "Repo".into(),
            account: Account {
                host: "github.com".into(),
                login: login.into(),
            },
            local_path: None,
        }
    }

    fn review(
        repository: &Repository,
        id: &str,
        source: &str,
        body: &str,
    ) -> SubmittedSummaryDraft {
        SubmittedSummaryDraft {
            review: PullRequestReview {
                coordinates: ProviderCoordinates {
                    provider: "github".into(),
                    host: repository.host.clone(),
                    owner: repository.owner.clone(),
                    repository: repository.name.clone(),
                    pull_request: 7,
                    remote_id: id.into(),
                },
                author: Some(repository.account.login.clone()),
                body: source.into(),
                state: "COMMENTED".into(),
                submitted_at: Some("2026-09-13T12:00:00Z".into()),
                commit_sha: Some("1".repeat(40)),
                edit_summary_capability: Some(SubmittedReviewEditCapability {
                    viewer_did_author: true,
                    viewer_can_update: true,
                    viewer_cannot_update_reasons: Vec::new(),
                }),
                dismissal_capability: None,
                url: "https://example.invalid/review".into(),
            },
            body: body.into(),
        }
    }

    #[test]
    fn persistence_serializes_saves_and_rejects_a_stale_receipt() {
        let repository = repository("alice");
        let first = review(&repository, "REVIEW_A", "source A", "first body");
        let mut newer = first.clone();
        newer.body = "newer body".into();
        let first_snapshot = DraftSnapshot {
            generation: None,
            active_review: Some(first.review.coordinates.clone()),
            drafts: vec![first],
        };
        let newer_snapshot = DraftSnapshot {
            generation: None,
            active_review: Some(newer.review.coordinates.clone()),
            drafts: vec![newer],
        };
        let mut persistence = SubmittedDraftPersistence::default();

        persistence.queue_snapshot(first_snapshot.clone());
        let active = persistence.start_next(true).unwrap();
        persistence.queue_snapshot(newer_snapshot.clone());
        assert!(persistence.start_next(true).is_none());

        let mut saved = active.snapshot.clone();
        saved.generation = Some(1);
        persistence
            .complete_save(
                active.operation_generation(),
                newer_snapshot.clone(),
                Ok(saved),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            persistence.pending.as_ref().unwrap().drafts[0].body,
            "newer body"
        );

        let stale = persistence.start_next(true).unwrap();
        assert_eq!(
            persistence.complete_save(
                stale.operation_generation(),
                first_snapshot,
                Ok(DraftSnapshot::default()),
            ),
            Some(Err(
                "Submitted-review draft was not made durable; text remains open and close was refused: Submitted-review draft save receipt did not match its exact snapshot."
                    .into()
            ))
        );
        assert!(persistence.in_flight.is_none());
        assert!(persistence.pending.is_none());
    }

    #[test]
    fn editor_requires_owned_clear_generation_and_preserves_newer_text() {
        let repository = repository("alice");
        let draft = review(&repository, "REVIEW_A", "source A", "sent body");
        let mut editor = SubmittedSummaryEditor::default();
        editor.assume_loaded_for_test();
        editor.begin(draft.review.clone());
        editor.store_active_body(draft.body.clone());
        let durable = DraftSnapshot {
            generation: Some(12),
            active_review: Some(draft.review.coordinates.clone()),
            drafts: vec![draft.clone()],
        };
        editor.complete_load(editor.edit_generation(), Ok(durable));

        assert_eq!(
            editor.complete_clear(1, Ok(DraftSnapshot::default())),
            None,
            "a completion without an owned clear is stale, not an admission"
        );
        editor.queue_clear(draft);
        let clear = editor.start_clear(false).unwrap().unwrap();
        editor.queue_current();
        editor.store_active_body("newer typed body".into());
        let tombstone = DraftSnapshot {
            generation: Some(13),
            active_review: None,
            drafts: Vec::new(),
        };
        assert_eq!(
            editor.complete_clear(clear.operation_generation() + 1, Ok(tombstone.clone())),
            None,
            "a stale callback cannot consume the owned clear"
        );
        assert!(editor.has_active_operation());
        assert_eq!(
            editor.complete_clear(clear.operation_generation(), Ok(tombstone)),
            Some(Ok(false))
        );
        assert_eq!(editor.active_draft().unwrap().body, "newer typed body");
        assert_eq!(editor.durable_snapshot().generation, Some(13));
        assert_eq!(
            editor.pending_snapshot().unwrap().drafts[0].body,
            "newer typed body"
        );
        assert!(editor.start_clear(false).is_none());
    }

    #[test]
    fn close_save_failure_is_recorded_and_reopens_the_barrier() {
        let repository = repository("alice");
        let draft = review(&repository, "REVIEW_A", "source A", "must remain");
        let mut editor = SubmittedSummaryEditor::default();
        editor.assume_loaded_for_test();
        editor.begin(draft.review);
        editor.store_active_body(draft.body);

        assert_eq!(editor.request_close(), SubmittedDraftCloseDisposition::Save);
        assert!(editor.close_after_save());
        let save = editor.start_next_save().unwrap();
        let error = editor
            .complete_save(
                save.operation_generation(),
                Err("injected CAS failure".into()),
            )
            .unwrap()
            .unwrap_err();
        assert_eq!(editor.persistence_error(), Some(error.as_str()));
        assert!(!editor.close_after_save());
        assert_eq!(editor.active_draft().unwrap().body, "must remain");
        assert_eq!(editor.request_close(), SubmittedDraftCloseDisposition::Save);
    }

    #[test]
    fn exact_identity_account_and_review_lanes_survive_restart_including_empty_body() {
        let directory = tempdir().unwrap();
        let alice = repository("Alice");
        let bob = repository("bob");
        let store = SubmittedReviewDraftStore::new(directory.path().to_owned());
        let a = review(&alice, "REVIEW_A", "source A", "typed A");
        let b = review(&alice, "REVIEW_B", "source B", "");
        let saved = store
            .save_if_current(
                &alice,
                7,
                None,
                Some(&a.review.coordinates),
                &[a.clone(), b.clone()],
            )
            .unwrap();
        assert_eq!(saved.drafts.len(), 2);
        assert_eq!(saved.drafts[0].body, "typed A");
        assert_eq!(saved.drafts[1].body, "");
        assert!(same_review_coordinates(
            &saved.drafts[0].review.coordinates,
            &a.review.coordinates
        ));
        assert!(
            saved
                .drafts
                .iter()
                .all(|draft| draft.review.edit_summary_capability.is_none())
        );
        assert!(store.load(&bob, 7).unwrap().drafts.is_empty());
        let bob_draft = review(&bob, "REVIEW_A", "bob source", "bob text");
        store
            .save_if_current(
                &bob,
                7,
                None,
                Some(&bob_draft.review.coordinates),
                std::slice::from_ref(&bob_draft),
            )
            .unwrap();

        let restarted = SubmittedReviewDraftStore::new(directory.path().to_owned());
        let alice_loaded = restarted.load(&repository("ALICE"), 7).unwrap();
        assert_eq!(alice_loaded.drafts.len(), 2);
        assert_eq!(alice_loaded.drafts[0].body, "typed A");
        assert_eq!(alice_loaded.drafts[1].body, "");
        assert!(
            alice_loaded
                .drafts
                .iter()
                .all(|draft| draft.review.edit_summary_capability.is_none())
        );
        let bob_loaded = restarted.load(&bob, 7).unwrap();
        assert_eq!(bob_loaded.drafts.len(), 1);
        assert_eq!(bob_loaded.drafts[0].body, bob_draft.body);
        assert!(same_review_coordinates(
            &bob_loaded.drafts[0].review.coordinates,
            &bob_draft.review.coordinates
        ));
    }

    #[test]
    fn configured_case_alias_resumes_same_exact_opaque_review_identity() {
        let directory = tempdir().unwrap();
        let mixed = repository("Alice");
        let store = SubmittedReviewDraftStore::new(directory.path().to_owned());
        let saved = review(&mixed, "Review_CaseSensitive", "old", "typed once");
        store
            .save_if_current(
                &mixed,
                7,
                None,
                Some(&saved.review.coordinates),
                std::slice::from_ref(&saved),
            )
            .unwrap();
        let mut alias = repository("ALICE");
        alias.host = "github.com".into();
        alias.owner = "octo".into();
        alias.name = "repo".into();
        let loaded = store.load(&alias, 7).unwrap();
        let mut fresh = review(&mixed, "Review_CaseSensitive", "new", "new");
        fresh.review.coordinates.host = "GITHUB.com".into();
        assert!(same_review_coordinates(
            &loaded.drafts[0].review.coordinates,
            &fresh.review.coordinates
        ));
        fresh.review.coordinates.remote_id = "review_casesensitive".into();
        assert!(!same_review_coordinates(
            &loaded.drafts[0].review.coordinates,
            &fresh.review.coordinates
        ));
    }

    #[test]
    fn two_store_stale_save_and_clear_cannot_erase_or_resurrect_text() {
        let directory = tempdir().unwrap();
        let repository = repository("alice");
        let first_store = SubmittedReviewDraftStore::new(directory.path().to_owned());
        let second_store = first_store.clone();
        let original = review(&repository, "REVIEW_A", "source", "first");
        let first = first_store
            .save_if_current(
                &repository,
                7,
                None,
                Some(&original.review.coordinates),
                std::slice::from_ref(&original),
            )
            .unwrap();
        let stale_generation = first.generation;
        let mut newer = original.clone();
        newer.body = "newer editor".into();
        let newest = second_store
            .save_if_current(
                &repository,
                7,
                first.generation,
                Some(&newer.review.coordinates),
                std::slice::from_ref(&newer),
            )
            .unwrap();
        let mut stale = original.clone();
        stale.body = "stale callback".into();
        assert!(
            first_store
                .save_if_current(
                    &repository,
                    7,
                    stale_generation,
                    Some(&stale.review.coordinates),
                    std::slice::from_ref(&stale)
                )
                .is_err()
        );
        assert!(
            first_store
                .clear_if_current(
                    &repository,
                    7,
                    stale_generation,
                    &original.review.coordinates,
                    "first"
                )
                .is_err()
        );
        assert_eq!(first_store.load(&repository, 7).unwrap(), newest);

        let cleared = second_store
            .clear_if_current(
                &repository,
                7,
                newest.generation,
                &newer.review.coordinates,
                "newer editor",
            )
            .unwrap();
        assert!(cleared.drafts.is_empty());
        assert!(cleared.generation.unwrap() > newest.generation.unwrap());
        assert!(
            first_store
                .save_if_current(
                    &repository,
                    7,
                    newest.generation,
                    Some(&stale.review.coordinates),
                    std::slice::from_ref(&stale)
                )
                .is_err()
        );
        assert!(first_store.load(&repository, 7).unwrap().drafts.is_empty());
    }

    #[test]
    fn corrupt_future_symlink_and_hardlink_records_are_preserved() {
        let directory = tempdir().unwrap();
        let repository = repository("alice");
        let store = SubmittedReviewDraftStore::new(directory.path().to_owned());
        let draft = review(&repository, "REVIEW_A", "source", "typed");
        let saved = store
            .save_if_current(&repository, 7, None, None, std::slice::from_ref(&draft))
            .unwrap();
        let identity = PullRequestIdentity::new(&repository, 7).unwrap();
        let path = store.root.join(identity.filename());

        fs::write(&path, b"{corrupt").unwrap();
        let corrupt = fs::read(&path).unwrap();
        assert!(store.load(&repository, 7).is_err());
        assert!(
            store
                .save_if_current(
                    &repository,
                    7,
                    saved.generation,
                    None,
                    std::slice::from_ref(&draft)
                )
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), corrupt);

        let mut future = record_from_memory(
            &identity,
            saved.generation,
            None,
            std::slice::from_ref(&draft),
        )
        .unwrap();
        future.schema_version = 99;
        fs::write(&path, serde_json::to_vec(&future).unwrap()).unwrap();
        let future_bytes = fs::read(&path).unwrap();
        assert!(store.load(&repository, 7).is_err());
        assert!(
            store
                .save_if_current(
                    &repository,
                    7,
                    saved.generation,
                    None,
                    std::slice::from_ref(&draft)
                )
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), future_bytes);

        fs::remove_file(&path).unwrap();
        let outside = directory.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(store.load(&repository, 7).is_err());
        assert!(
            store
                .save_if_current(&repository, 7, None, None, std::slice::from_ref(&draft))
                .is_err()
        );
        assert_eq!(fs::read_link(&path).unwrap(), outside);
        fs::remove_file(&path).unwrap();
        fs::hard_link(&outside, &path).unwrap();
        assert!(store.load(&repository, 7).is_err());
        assert!(
            store
                .save_if_current(&repository, 7, None, None, std::slice::from_ref(&draft))
                .is_err()
        );
        assert_eq!(fs::read(&outside).unwrap(), b"outside");

        fs::remove_file(&path).unwrap();
        fs::write(&path, b"foreign-readable preserved bytes").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let non_private = fs::read(&path).unwrap();
        assert!(store.load(&repository, 7).is_err());
        assert!(
            store
                .save_if_current(&repository, 7, None, None, std::slice::from_ref(&draft))
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), non_private);
    }

    #[test]
    fn exact_clear_preserves_the_other_review_and_private_layout() {
        let directory = tempdir().unwrap();
        let repository = repository("alice");
        let store = SubmittedReviewDraftStore::new(directory.path().to_owned());
        let a = review(&repository, "REVIEW_A", "source A", "typed A");
        let b = review(&repository, "REVIEW_B", "source B", "typed B");
        let saved = store
            .save_if_current(
                &repository,
                7,
                None,
                Some(&a.review.coordinates),
                &[a.clone(), b.clone()],
            )
            .unwrap();
        let cleared = store
            .clear_if_current(
                &repository,
                7,
                saved.generation,
                &a.review.coordinates,
                "typed A",
            )
            .unwrap();
        assert_eq!(cleared.drafts.len(), 1);
        assert_eq!(cleared.drafts[0].body, "typed B");
        assert!(same_review_coordinates(
            &cleared.drafts[0].review.coordinates,
            &b.review.coordinates
        ));
        assert!(cleared.active_review.is_none());
        assert_eq!(
            fs::symlink_metadata(&store.root)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let identity = PullRequestIdentity::new(&repository, 7).unwrap();
        assert_eq!(
            fs::symlink_metadata(store.root.join(identity.filename()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn root_replacement_cannot_redirect_locked_atomic_write() {
        let directory = tempdir().unwrap();
        let repository = repository("alice");
        let store = SubmittedReviewDraftStore::new(directory.path().to_owned());
        let original = review(&repository, "REVIEW_A", "source", "original");
        let saved = store
            .save_if_current(
                &repository,
                7,
                None,
                Some(&original.review.coordinates),
                std::slice::from_ref(&original),
            )
            .unwrap();
        let mut updated = original.clone();
        updated.body = "descriptor anchored update".into();
        let identity = PullRequestIdentity::new(&repository, 7).unwrap();
        let record = record_from_memory(
            &identity,
            saved.generation,
            Some(&updated.review.coordinates),
            std::slice::from_ref(&updated),
        )
        .unwrap();
        let bytes = serde_json::to_vec(&record).unwrap();
        let moved = directory.path().join("moved-root");
        let escape = directory.path().join("escape-root");
        fs::DirBuilder::new().mode(0o700).create(&escape).unwrap();

        let result = store.with_lock_after(
            false,
            |_| {
                fs::rename(&store.root, &moved)?;
                std::os::unix::fs::symlink(&escape, &store.root)?;
                Ok(())
            },
            |root| {
                let current = store.read_record(root, &identity)?;
                assert_eq!(
                    current.as_ref().map(|record| record.record.generation),
                    saved.generation
                );
                store.atomic_write(
                    root,
                    &identity.filename(),
                    current.as_ref().map(|record| &record.metadata),
                    &bytes,
                )
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read_dir(&escape).unwrap().count(), 0);
        let installed: DraftRecord =
            serde_json::from_slice(&fs::read(moved.join(identity.filename())).unwrap()).unwrap();
        assert_eq!(installed.drafts[0].body, "descriptor anchored update");
        assert!(fs::read_dir(&moved).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp-")
        }));
    }

    #[test]
    fn bounded_load_refuses_overfull_root_and_exact_lock_admission_boundary() {
        let directory = tempdir().unwrap();
        let repository = repository("alice");
        let store = SubmittedReviewDraftStore::new(directory.path().to_owned());
        let draft = review(&repository, "REVIEW_A", "source", "typed");
        store
            .save_if_current(&repository, 7, None, None, std::slice::from_ref(&draft))
            .unwrap();
        fs::remove_file(store.root.join(LOCK_NAME)).unwrap();
        for index in 0..(MAX_ROOT_ENTRIES - 1) {
            let filler = store.root.join(format!("filler-{index:03}"));
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(filler)
                .unwrap();
        }
        assert_eq!(fs::read_dir(&store.root).unwrap().count(), MAX_ROOT_ENTRIES);
        assert!(store.load(&repository, 7).is_err());
        assert!(!store.root.join(LOCK_NAME).exists());

        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(store.root.join("one-too-many"))
            .unwrap();
        assert!(store.load(&repository, 7).is_err());
        assert!(!store.root.join(LOCK_NAME).exists());

        let bytes_directory = tempdir().unwrap();
        let bytes_store = SubmittedReviewDraftStore::new(bytes_directory.path().to_owned());
        bytes_store
            .save_if_current(&repository, 7, None, None, std::slice::from_ref(&draft))
            .unwrap();
        let oversized = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(bytes_store.root.join("oversized-preserved"))
            .unwrap();
        oversized.set_len(MAX_ROOT_BYTES).unwrap();
        assert!(bytes_store.load(&repository, 7).is_err());
        assert_eq!(oversized.metadata().unwrap().len(), MAX_ROOT_BYTES);
    }

    #[test]
    fn target_replacement_after_cas_read_is_preserved_by_conditional_install() {
        let directory = tempdir().unwrap();
        let repository = repository("alice");
        let store = SubmittedReviewDraftStore::new(directory.path().to_owned());
        let original = review(&repository, "REVIEW_A", "source", "original");
        let saved = store
            .save_if_current(
                &repository,
                7,
                None,
                Some(&original.review.coordinates),
                std::slice::from_ref(&original),
            )
            .unwrap();
        let identity = PullRequestIdentity::new(&repository, 7).unwrap();
        let mut update = original.clone();
        update.body = "candidate update".into();
        let candidate = serde_json::to_vec(
            &record_from_memory(
                &identity,
                saved.generation,
                Some(&update.review.coordinates),
                std::slice::from_ref(&update),
            )
            .unwrap(),
        )
        .unwrap();
        let target = store.root.join(identity.filename());
        let preserved_original = store.root.join("preserved-original");
        let replacement = b"corrupt replacement must survive".to_vec();
        let recovery_was_durably_observed = std::cell::Cell::new(false);

        let result = store.with_lock(false, |root| {
            let current = store.read_record(root, &identity)?.unwrap();
            store.atomic_write_after_check(
                root,
                &identity.filename(),
                Some(&current.metadata),
                &candidate,
                AtomicWriteHooks {
                    after_create: |_: &File, _: &str| Ok(()),
                    before_install: || {
                        fs::rename(&target, &preserved_original)?;
                        let mut file = OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .mode(0o600)
                            .open(&target)?;
                        file.write_all(&replacement)?;
                        file.sync_all()?;
                        Ok(())
                    },
                    after_error_sync: || {
                        assert_eq!(fs::read(&target)?, replacement);
                        assert!(fs::read_dir(&store.root)?.all(|entry| {
                            !entry
                                .unwrap()
                                .file_name()
                                .to_string_lossy()
                                .starts_with(".tmp-")
                        }));
                        recovery_was_durably_observed.set(true);
                        Ok(())
                    },
                },
            )
        });
        assert!(result.is_err());
        assert!(recovery_was_durably_observed.get());
        assert_eq!(fs::read(&target).unwrap(), replacement);
        let original_record: DraftRecord =
            serde_json::from_slice(&fs::read(preserved_original).unwrap()).unwrap();
        assert_eq!(original_record.drafts[0].body, "original");
    }

    #[test]
    fn submitted_post_open_pre_metadata_failure_syncs_retained_temp() {
        let directory = tempdir().unwrap();
        let repository = repository("alice");
        let store = SubmittedReviewDraftStore::new(directory.path().to_owned());
        let identity = PullRequestIdentity::new(&repository, 7).unwrap();
        assert!(store.ensure_root(true).unwrap());
        let retained_name = std::cell::RefCell::new(None::<String>);
        let recovery_was_durably_observed = std::cell::Cell::new(false);

        let result = store.with_lock(false, |root| {
            store.atomic_write_after_check(
                root,
                &identity.filename(),
                None,
                b"forced candidate",
                AtomicWriteHooks {
                    after_create: |_: &File, temporary: &str| {
                        retained_name.replace(Some(temporary.to_owned()));
                        bail!("forced post-open metadata failure")
                    },
                    before_install: || unreachable!("post-open failure stops before install"),
                    after_error_sync: || {
                        let temporary = retained_name
                            .borrow()
                            .clone()
                            .expect("post-create hook captured candidate name");
                        let metadata = fs::symlink_metadata(store.root.join(temporary))?;
                        assert!(metadata.is_file());
                        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
                        recovery_was_durably_observed.set(true);
                        Ok(())
                    },
                },
            )
        });
        let error = format!("{:#}", result.unwrap_err());
        assert!(
            error.contains("forced post-open metadata failure"),
            "{error}"
        );
        assert!(recovery_was_durably_observed.get());
    }
}
