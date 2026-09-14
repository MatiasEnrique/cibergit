use cibergit::{
    domain::{
        MergeAcknowledgement, MergeExecutionRequest, MergeMethod, MergePreparation,
        MutationAdmissionReceipt, MutationContext, MutationTerminalRecord, PendingReviewSnapshot,
        ProviderCoordinates, ProviderMutationOutcome, PullRequestDetails,
        PullRequestDiscussionRequest, PullRequestLifecycleRequest, Repository,
        ReviewAuxiliaryAcknowledgement, ReviewAuxiliaryRequest, ReviewComment, ReviewThread,
    },
    participation::{
        CanonicalPublishedPatch, DraftCoordinate, DraftStore, LineSelection, LoadOutcome,
        PublishedFile, PublishedPosition, ReviewComposition, ReviewEvent, ReviewKey,
        ReviewOperation, ReviewOperationPayload, ReviewOperationStatus, ReviewOperationTarget,
        map_file_to_canonical_published, map_to_canonical_published, validate_coordinate,
    },
    providers::{AdmittedMutationAttempt, MutationAdmission},
    review::{ReviewSession, file_key},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    ffi::c_int,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const JOURNAL_VERSION: u64 = 1;
const MAX_JOURNAL_BYTES: usize = 512 * 1024;
const MAX_JOURNAL_OPERATIONS: usize = 96;
const MAX_JOURNAL_TEXT_BYTES: usize = 64 * 1024;
const LOCK_UN: c_int = 0x08;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static ATTEMPT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

unsafe extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
}

#[derive(Clone, Debug)]
pub struct ComposerState {
    pub coordinate: DraftCoordinate,
    /// Untouched witness from a narrower displayed pair. Persisted review
    /// drafts always retain the independently validated canonical coordinate.
    pub source_coordinate: Option<DraftCoordinate>,
    pub draft_id: Option<String>,
    pub body: String,
    pub durable: bool,
    pub notice: Option<String>,
}

#[derive(Clone, Debug)]
pub struct FileComposerState {
    pub target: PublishedFile,
    pub draft_id: Option<String>,
    pub body: String,
    pub durable: bool,
    pub notice: Option<String>,
}

#[derive(Clone, Debug)]
pub enum ControllerLoad {
    Ready(Box<ReviewInteractionController>),
    RecoveryRequired(String),
}

#[derive(Clone, Debug)]
pub struct ReviewInteractionController {
    pub composition: ReviewComposition,
    pub store: DraftStore,
    pub authority: ReviewStateAuthority,
    pub durable_composition: Option<ReviewComposition>,
    undurable_drafts: HashSet<String>,
    pub composer: Option<ComposerState>,
    pub file_composer: Option<FileComposerState>,
    pub pending_review: Option<PendingReviewSnapshot>,
    pub pending_complete: bool,
    pub notice: Option<String>,
    pub reconciliation_results: Vec<ReviewReconciliationItem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewReconciliationItem {
    pub operation_id: String,
    pub attempt_id: String,
    pub concise_request: String,
    pub frozen_request: String,
    pub outcome: ReviewReconciliationOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewReconciliationOutcome {
    Reconciled(String),
    Unresolved(String),
}

#[derive(Clone, Debug)]
pub struct ReviewReconciliationReport {
    pub composition: ReviewComposition,
    pub details: PullRequestDetails,
    pub pending: Option<PendingReviewSnapshot>,
    pub items: Vec<ReviewReconciliationItem>,
}

impl ReviewReconciliationReport {
    pub fn resolved(&self) -> usize {
        self.items
            .iter()
            .filter(|item| matches!(item.outcome, ReviewReconciliationOutcome::Reconciled(_)))
            .count()
    }

    pub fn unresolved(&self) -> usize {
        self.items.len() - self.resolved()
    }
}

impl ReviewInteractionController {
    pub fn load(
        root: &Path,
        repository: &Repository,
        pull_request: u64,
        session: &ReviewSession,
    ) -> Result<ControllerLoad, String> {
        let key = ReviewKey::for_repository("github", repository, pull_request)
            .map_err(|error| error.to_string())?;
        let store = DraftStore::open(root.join("drafts")).map_err(|error| error.to_string())?;
        let (composition, durable_composition) = match store
            .load(&key)
            .map_err(|error| error.to_string())?
        {
            LoadOutcome::Missing => (
                ReviewComposition::new(key.clone(), session.revision().clone())
                    .map_err(|error| error.to_string())?,
                None,
            ),
            LoadOutcome::Loaded(composition) => {
                if composition.reviewed_revision != *session.revision() {
                    return Ok(ControllerLoad::RecoveryRequired(format!(
                        "Unfinished review text targets {} and was preserved. Display that revision before editing it; the record was not overwritten by {}.",
                        short_sha(&composition.reviewed_revision.head_sha),
                        short_sha(&session.revision().head_sha)
                    )));
                }
                let composition = *composition;
                (composition.clone(), Some(composition))
            }
            LoadOutcome::Corrupt { path, reason } => {
                return Ok(ControllerLoad::RecoveryRequired(format!(
                    "Review recovery is unreadable at {} and was preserved: {reason}",
                    path.display()
                )));
            }
            LoadOutcome::FutureVersion { path, version } => {
                return Ok(ControllerLoad::RecoveryRequired(format!(
                    "Review recovery at {} uses newer format {version} and was preserved.",
                    path.display()
                )));
            }
        };
        let authority = ReviewStateAuthority::open(root.to_owned(), key)?;
        Ok(ControllerLoad::Ready(Box::new(Self {
            composition,
            store,
            authority,
            durable_composition,
            undurable_drafts: HashSet::new(),
            composer: None,
            file_composer: None,
            pending_review: None,
            pending_complete: true,
            notice: None,
            reconciliation_results: Vec::new(),
        })))
    }

    pub fn select_line_with_canonical(
        &mut self,
        displayed: &ReviewSession,
        canonical: &ReviewSession,
        selection: LineSelection,
    ) -> Result<(), String> {
        self.require_writable_display(displayed, canonical)?;
        if self
            .file_composer
            .as_ref()
            .is_some_and(|composer| !composer.durable)
        {
            return Err("Save the open file-level draft before changing comment targets.".into());
        }
        let selected = displayed
            .selected_file()
            .ok_or_else(|| "Select a text file before starting a discussion.".to_owned())?;
        let source_coordinate = validate_coordinate(displayed, &file_key(selected), selection)
            .map_err(|error| error.to_string())?;
        let published = map_to_canonical_published(
            &source_coordinate,
            CanonicalPublishedPatch::new(canonical.comparison(), canonical.metadata())
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("This selected line is read-only: {error}. Return to Full PR or choose a safely mappable NEW-side line."))?;
        let canonical_file = canonical
            .comparison()
            .files
            .iter()
            .find(|file| file.path == published.path)
            .ok_or_else(|| {
                "The mapped file is absent from the retained full pull request.".to_owned()
            })?;
        let coordinate = validate_coordinate(
            canonical,
            &file_key(canonical_file),
            LineSelection {
                side: published.side,
                start_line: published.start_line.unwrap_or(published.line),
                line: published.line,
            },
        )
        .map_err(|error| {
            format!("The canonical full patch cannot independently validate this line: {error}")
        })?;
        if published.commit_sha != coordinate.reviewed_revision.head_sha
            || published.path != coordinate.path
        {
            return Err("Canonical re-anchor identity changed during validation.".into());
        }
        let existing = self.composition.drafts.iter().find(|draft| {
            draft.coordinate == coordinate
                && draft.disposition == cibergit::participation::DraftDisposition::Pending
        });
        self.composer = Some(match existing {
            Some(draft) => ComposerState {
                coordinate,
                source_coordinate: (source_coordinate != draft.coordinate)
                    .then_some(source_coordinate),
                draft_id: Some(draft.id.clone()),
                body: draft.body.clone(),
                durable: !self.undurable_drafts.contains(&draft.id),
                notice: self
                    .undurable_drafts
                    .contains(&draft.id)
                    .then(|| "The latest text has not finished saving locally.".into()),
            },
            None => ComposerState {
                coordinate,
                source_coordinate: (source_coordinate.reviewed_revision
                    != self.composition.reviewed_revision)
                    .then_some(source_coordinate),
                draft_id: None,
                body: String::new(),
                durable: false,
                notice: Some(
                    "Bound to the displayed revision. Text becomes durable after its first local save."
                        .into(),
                ),
            },
        });
        self.file_composer = None;
        Ok(())
    }

    pub fn select_file_with_canonical(
        &mut self,
        displayed: &ReviewSession,
        canonical: &ReviewSession,
    ) -> Result<(), String> {
        self.require_writable_display(displayed, canonical)?;
        if self
            .composer
            .as_ref()
            .is_some_and(|composer| !composer.durable)
        {
            return Err("Save the open inline draft before changing comment targets.".into());
        }
        let selected = displayed
            .selected_file()
            .ok_or_else(|| "Select a file before starting a file-level discussion.".to_owned())?;
        let target = map_file_to_canonical_published(
            displayed,
            &file_key(selected),
            CanonicalPublishedPatch::new(canonical.comparison(), canonical.metadata())
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("This file target is read-only: {error}."))?;
        let existing = self.composition.file_drafts.iter().find(|draft| {
            draft.target == target
                && draft.disposition == cibergit::participation::DraftDisposition::Pending
                && draft.remote.is_none()
        });
        self.file_composer = Some(match existing {
            Some(draft) => FileComposerState {
                target,
                draft_id: Some(draft.id.clone()),
                body: draft.body.clone(),
                durable: !self.undurable_drafts.contains(&draft.id),
                notice: Some(if self.undurable_drafts.contains(&draft.id) {
                    "The latest text has not finished saving locally.".into()
                } else {
                    "Reopened local file-level text.".into()
                }),
            },
            None => FileComposerState {
                target,
                draft_id: None,
                body: String::new(),
                durable: false,
                notice: Some(
                    "Bound to this exact canonical file. Text becomes durable after its first local save."
                        .into(),
                ),
            },
        });
        self.composer = None;
        Ok(())
    }

    #[allow(dead_code)] // Retained for focused controller tests and full-session callers.
    pub fn select_line(
        &mut self,
        session: &ReviewSession,
        selection: LineSelection,
    ) -> Result<(), String> {
        self.select_line_with_canonical(session, session, selection)
    }

    fn require_writable_display(
        &self,
        displayed: &ReviewSession,
        canonical: &ReviewSession,
    ) -> Result<(), String> {
        if canonical.revision() != &self.composition.reviewed_revision {
            return Err(format!(
                "Unfinished review text targets {} and was preserved. Return to that Full PR snapshot before editing or submitting.",
                short_sha(&self.composition.reviewed_revision.head_sha)
            ));
        }
        if displayed.revision().head_sha != canonical.revision().head_sha {
            return Err(format!(
                "This comparison ends at {}, older than the reviewed head {}. Return to Full PR or choose a range ending at the current head to comment.",
                short_sha(&displayed.revision().head_sha),
                short_sha(&canonical.revision().head_sha)
            ));
        }
        Ok(())
    }

    pub fn reopen_draft(&mut self, draft_id: &str) -> Result<ComposerState, String> {
        let draft = self
            .composition
            .draft(draft_id)
            .ok_or_else(|| "The linked local draft no longer exists.".to_owned())?;
        if draft.disposition != cibergit::participation::DraftDisposition::Pending {
            return Err(
                "Submitted and immediately posted comments are historical; editing them does not republish."
                    .into(),
            );
        }
        let durable = !self.undurable_drafts.contains(&draft.id);
        let composer = ComposerState {
            coordinate: draft.coordinate.clone(),
            source_coordinate: None,
            draft_id: Some(draft.id.clone()),
            body: draft.body.clone(),
            durable,
            notice: Some(if !durable {
                "The latest text has not finished saving locally.".into()
            } else if draft.remote.is_some() {
                "Editing the linked pending comment locally; Add to pending review is an explicit update."
                    .into()
            } else {
                "Reopened local pending text.".into()
            }),
        };
        self.composer = Some(composer.clone());
        self.file_composer = None;
        Ok(composer)
    }

    pub fn reopen_file_draft(&mut self, draft_id: &str) -> Result<FileComposerState, String> {
        let draft = self
            .composition
            .file_draft(draft_id)
            .ok_or_else(|| "The linked local file draft no longer exists.".to_owned())?;
        if draft.disposition != cibergit::participation::DraftDisposition::Pending
            || draft.remote.is_some()
        {
            return Err("Published file comments are historical; create a new file draft.".into());
        }
        let durable = !self.undurable_drafts.contains(&draft.id);
        let composer = FileComposerState {
            target: draft.target.clone(),
            draft_id: Some(draft.id.clone()),
            body: draft.body.clone(),
            durable,
            notice: Some(if durable {
                "Reopened local file-level text.".into()
            } else {
                "The latest text has not finished saving locally.".into()
            }),
        };
        self.file_composer = Some(composer.clone());
        self.composer = None;
        Ok(composer)
    }

    /// Apply the text in memory. Callers persist the returned composition away
    /// from the UI thread and then report the save result through
    /// `finish_composer_save`.
    pub fn stage_composer_text(&mut self, body: String) -> Result<ReviewComposition, String> {
        if let Some(composer) = self.file_composer.as_mut() {
            composer.body = body.clone();
            composer.durable = false;
            composer.notice = Some("Saving local recovery…".into());
            if let Some(draft_id) = composer.draft_id.as_deref() {
                self.composition
                    .edit_file_draft(draft_id, body)
                    .map_err(|error| error.to_string())?;
            } else {
                let draft = self
                    .composition
                    .add_file_draft(composer.target.clone(), body)
                    .map_err(|error| error.to_string())?;
                composer.draft_id = Some(draft.id.clone());
            }
            if let Some(draft_id) = &composer.draft_id {
                self.undurable_drafts.insert(draft_id.clone());
            }
            return Ok(self.composition.clone());
        }
        let composer = self
            .composer
            .as_mut()
            .ok_or_else(|| "No inline composer is open.".to_owned())?;
        composer.body = body.clone();
        composer.durable = false;
        composer.notice = Some("Saving local recovery…".into());
        if let Some(draft_id) = composer.draft_id.as_deref() {
            self.composition
                .edit_draft(draft_id, body)
                .map_err(|error| error.to_string())?;
        } else {
            let draft = self
                .composition
                .add_draft(composer.coordinate.clone(), body)
                .map_err(|error| error.to_string())?;
            composer.draft_id = Some(draft.id.clone());
        }
        if let Some(draft_id) = &composer.draft_id {
            self.undurable_drafts.insert(draft_id.clone());
        }
        Ok(self.composition.clone())
    }

    pub fn finish_composer_save(
        &mut self,
        saved: &ReviewComposition,
        draft_id: &str,
        saved_body: &str,
        result: Result<(), String>,
    ) {
        if result.is_ok() {
            self.durable_composition = Some(saved.clone());
            self.undurable_drafts.remove(draft_id);
        }
        if let Some(composer) = self.file_composer.as_mut()
            && composer.draft_id.as_deref() == Some(draft_id)
            && composer.body == saved_body
        {
            match result {
                Ok(()) => {
                    composer.durable = true;
                    composer.notice =
                        Some("Saved locally for restart and offline recovery.".into());
                }
                Err(ref error) => {
                    self.undurable_drafts.insert(draft_id.into());
                    composer.durable = false;
                    composer.notice = Some(format!(
                        "Local save failed; text remains only in this open window: {error}"
                    ));
                }
            }
            return;
        }
        let Some(composer) = self.composer.as_mut() else {
            return;
        };
        if composer.draft_id.as_deref() != Some(draft_id) || composer.body != saved_body {
            return;
        }
        match result {
            Ok(()) => {
                composer.durable = true;
                composer.notice = Some("Saved locally for restart and offline recovery.".into());
            }
            Err(error) => {
                self.undurable_drafts.insert(draft_id.into());
                composer.durable = false;
                composer.notice = Some(format!(
                    "Local save failed; text remains only in this open window: {error}"
                ));
            }
        }
    }

    pub fn prepare_pending_with_canonical(
        &mut self,
        displayed: &ReviewSession,
        canonical_session: &ReviewSession,
    ) -> Result<String, String> {
        self.require_writable_display(displayed, canonical_session)?;
        let draft_id = self.saved_composer_id()?;
        let canonical = CanonicalPublishedPatch::new(
            canonical_session.comparison(),
            canonical_session.metadata(),
        )
        .map_err(|error| error.to_string())?;
        let intent = self
            .composition
            .prepare_pending_comment(&draft_id, canonical)
            .map_err(|error| error.to_string())?;
        self.reconciliation_results.clear();
        Ok(intent.operation_id)
    }

    #[allow(dead_code)] // Retained for focused controller tests and full-session callers.
    pub fn prepare_pending(&mut self, session: &ReviewSession) -> Result<String, String> {
        self.prepare_pending_with_canonical(session, session)
    }

    pub fn prepare_pending_file_comment(
        &mut self,
        source: &cibergit::domain::PendingFileCommentSource,
    ) -> Result<String, String> {
        let draft_id = self.saved_file_composer_id()?;
        let intent = self
            .composition
            .prepare_pending_file_comment(&draft_id, source)
            .map_err(|error| error.to_string())?;
        self.reconciliation_results.clear();
        Ok(intent.operation_id)
    }

    pub fn prepare_immediate_with_canonical(
        &mut self,
        displayed: &ReviewSession,
        canonical_session: &ReviewSession,
    ) -> Result<String, String> {
        self.require_writable_display(displayed, canonical_session)?;
        let draft_id = self.saved_composer_id()?;
        let canonical = CanonicalPublishedPatch::new(
            canonical_session.comparison(),
            canonical_session.metadata(),
        )
        .map_err(|error| error.to_string())?;
        let intent = self
            .composition
            .prepare_immediate_comment(&draft_id, canonical)
            .map_err(|error| error.to_string())?;
        self.reconciliation_results.clear();
        Ok(intent.operation_id)
    }

    #[allow(dead_code)] // Retained for focused controller tests and full-session callers.
    pub fn prepare_immediate(&mut self, session: &ReviewSession) -> Result<String, String> {
        self.prepare_immediate_with_canonical(session, session)
    }

    pub fn prepare_submission(
        &mut self,
        event: ReviewEvent,
        body: String,
        current_head: Option<&str>,
    ) -> Result<String, String> {
        let intent = self
            .composition
            .prepare_submission(event, body, current_head)
            .map_err(|error| error.to_string())?;
        self.reconciliation_results.clear();
        Ok(intent.operation_id)
    }

    pub fn prepare_submission_with_displayed(
        &mut self,
        displayed: &ReviewSession,
        canonical: &ReviewSession,
        event: ReviewEvent,
        body: String,
        current_head: Option<&str>,
    ) -> Result<String, String> {
        self.require_writable_display(displayed, canonical)?;
        self.prepare_submission(event, body, current_head)
    }

    pub fn reconcile_details(&mut self, details: &PullRequestDetails) -> Result<(), String> {
        let report = self
            .composition
            .reconcile_remote_pending(details)
            .map_err(|error| error.to_string())?;
        self.pending_complete = details.activity_complete;
        if !details.activity_complete {
            self.notice = Some(
                "Pending-review linkage is partial; unlinked historical comments were not imported."
                    .into(),
            );
        } else if report.linked_comments_missing > 0 {
            self.notice = Some(format!(
                "{} linked pending comment(s) were absent from this read; local text was preserved.",
                report.linked_comments_missing
            ));
        }
        Ok(())
    }

    pub fn install_pending_snapshot(&mut self, snapshot: Option<PendingReviewSnapshot>) {
        self.pending_complete = snapshot
            .as_ref()
            .is_none_or(|snapshot| snapshot.comments_complete);
        self.pending_review = snapshot;
    }

    pub fn pending_count(&self) -> usize {
        self.composition
            .drafts
            .iter()
            .filter(|draft| draft.disposition == cibergit::participation::DraftDisposition::Pending)
            .count()
            + self
                .composition
                .file_drafts
                .iter()
                .filter(|draft| {
                    draft.disposition == cibergit::participation::DraftDisposition::Pending
                })
                .count()
    }

    pub fn unresolved_operations(&self) -> usize {
        self.composition
            .operations_requiring_reconciliation()
            .count()
    }

    pub fn unresolved_operation_descriptions(&self) -> Vec<String> {
        self.composition
            .operations_requiring_reconciliation()
            .map(|operation| {
                let attempt = operation_attempt(operation).unwrap_or("missing-attempt-id");
                let reason = match &operation.status {
                    ReviewOperationStatus::Uncertain { reason, .. } => reason.as_str(),
                    ReviewOperationStatus::InFlight { .. } => {
                        "The process stopped before a durable provider outcome was recorded."
                    }
                    _ => unreachable!("iterator only returns unresolved operations"),
                };
                format!(
                    "{} / attempt {} · {} · {reason}",
                    operation.id,
                    attempt,
                    frozen_request_summary(operation)
                )
            })
            .collect()
    }

    pub fn unresolved_operation_details(&self) -> Vec<(String, String)> {
        self.composition
            .operations_requiring_reconciliation()
            .zip(self.unresolved_operation_descriptions())
            .map(|(operation, description)| (operation.id.clone(), description))
            .collect()
    }

    pub fn unresolved_operation_summaries(&self) -> Vec<(String, String)> {
        self.composition
            .operations_requiring_reconciliation()
            .map(|operation| {
                let reason = match &operation.status {
                    ReviewOperationStatus::Uncertain { .. } => {
                        "Outcome unknown; use read-only reconciliation before retry."
                    }
                    ReviewOperationStatus::InFlight { .. } => {
                        "Outcome was not recorded; use read-only reconciliation before retry."
                    }
                    _ => unreachable!("iterator only returns unresolved operations"),
                };
                (
                    operation.id.clone(),
                    format!("{} · {reason}", concise_request_summary(operation)),
                )
            })
            .collect()
    }

    fn saved_composer_id(&self) -> Result<String, String> {
        let composer = self
            .composer
            .as_ref()
            .ok_or_else(|| "No inline composer is open.".to_owned())?;
        if !composer.durable {
            return Err(
                "Save the local text successfully before starting a remote operation.".into(),
            );
        }
        composer
            .draft_id
            .clone()
            .ok_or_else(|| "The composer has no durable draft identity.".into())
    }

    fn saved_file_composer_id(&self) -> Result<String, String> {
        let composer = self
            .file_composer
            .as_ref()
            .ok_or_else(|| "No file-level composer is open.".to_owned())?;
        if !composer.durable {
            return Err(
                "Save the local file-level text successfully before starting a remote operation."
                    .into(),
            );
        }
        composer
            .draft_id
            .clone()
            .ok_or_else(|| "The file-level composer has no durable draft identity.".into())
    }
}

/// The one cross-process authority lane for every provider mutation targeting
/// an account/repository/pull-request tuple. Durable journals decide whether a
/// later operation is admissible; this OS lock prevents their critical
/// sections and provider dispatches from racing across app processes.
#[derive(Clone, Debug)]
struct TargetMutationAuthority {
    root: PathBuf,
    key: ReviewKey,
}

impl TargetMutationAuthority {
    fn new(root: PathBuf, key: ReviewKey) -> Result<Self, String> {
        ensure_private_directory(&root)?;
        Ok(Self { root, key })
    }

    fn acquire(&self) -> Result<TargetMutationGuard, String> {
        let file = open_private_lock(&self.lock_path()?)?;
        file.lock()
            .map_err(|error| format!("Cannot lock target mutation authority: {error}"))?;
        Ok(TargetMutationGuard { file: Some(file) })
    }

    fn lock_path(&self) -> Result<PathBuf, String> {
        let account = stable_component(&(
            self.key.account.host.as_str(),
            self.key.account.login.as_str(),
        ))?;
        let repository = stable_component(&(
            self.key.provider.as_str(),
            self.key.host.as_str(),
            self.key.owner.as_str(),
            self.key.repository.as_str(),
        ))?;
        Ok(self
            .root
            .join("target-authority")
            .join("v1")
            .join(account)
            .join(repository)
            .join(format!("pr-{}.lock", self.key.pull_request)))
    }
}

#[derive(Debug)]
struct TargetMutationGuard {
    file: Option<File>,
}

impl Drop for TargetMutationGuard {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            // BSD flock belongs to the open-file description. Closing this
            // descriptor alone would not release it if a fork/dup still held
            // the description, so explicitly unlock before close on every
            // normal and error exit.
            let _ = unsafe { flock(file.as_raw_fd(), LOCK_UN) };
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReviewStateAuthority {
    root: PathBuf,
    key: ReviewKey,
    target: TargetMutationAuthority,
}

impl ReviewStateAuthority {
    pub fn open(root: PathBuf, key: ReviewKey) -> Result<Self, String> {
        Ok(Self {
            target: TargetMutationAuthority::new(root.clone(), key.clone())?,
            root,
            key,
        })
    }

    pub fn save_if_current(
        &self,
        store: &DraftStore,
        expected: Option<&ReviewComposition>,
        replacement: &ReviewComposition,
    ) -> Result<(), String> {
        self.with_lock(|| {
            let current = load_composition(store, &self.key)?;
            if current.as_ref() != expected {
                return Err(
                    "Durable review state changed in another completion or window; the stale clone was not saved. Reload before editing."
                        .into(),
                );
            }
            store.save(replacement).map_err(|error| error.to_string())
        })
    }

    pub fn execute_if_current<T>(
        &self,
        store: &DraftStore,
        expected: Option<&ReviewComposition>,
        execute: impl FnOnce() -> T,
    ) -> Result<(T, Option<ReviewComposition>), String> {
        self.with_lock(|| {
            let current = load_composition(store, &self.key)?;
            if current.as_ref() != expected {
                return Err(
                    "Durable review state changed before dispatch; zero writes sent and the stale preparation was not saved."
                        .into(),
                );
            }
            self.refuse_unresolved_action_journal()?;
            let result = execute();
            let durable = load_composition(store, &self.key)?;
            Ok((result, durable))
        })
    }

    fn refuse_unresolved_action_journal(&self) -> Result<(), String> {
        let journal = ActionJournal::open(&self.root, self.key.clone())?;
        if journal.operations_unlocked()?.iter().any(|operation| {
            matches!(
                operation.status,
                JournalStatus::InFlight | JournalStatus::Uncertain { .. }
            )
        }) {
            return Err(
                "Another target mutation has an unresolved durable outcome; zero review writes sent. Reconcile that exact attempt first."
                    .into(),
            );
        }
        Ok(())
    }

    /// Reconcile started review-composition operations with fresh read-only
    /// provider state while holding the same per-key authority as dispatch.
    /// A resolved result is returned only after the replacement is durable.
    pub fn reconcile_if_current(
        &self,
        store: &DraftStore,
        expected: Option<&ReviewComposition>,
        repository: &Repository,
        pull_request: u64,
        read: impl FnOnce() -> Result<(PullRequestDetails, Option<PendingReviewSnapshot>), String>,
    ) -> Result<ReviewReconciliationReport, String> {
        self.with_lock(|| {
            let current = load_composition(store, &self.key)?;
            if current.as_ref() != expected {
                return Err(
                    "Durable review state changed before reconciliation; the stale read was rejected and no outcome was changed."
                        .into(),
                );
            }
            let current = current.ok_or_else(|| {
                "Durable review state disappeared before reconciliation; no outcome was changed."
                    .to_owned()
            })?;
            let (details, pending) = read()?;
            let (composition, items) = reconcile_review_operations(
                &current,
                repository,
                pull_request,
                &details,
                pending.as_ref(),
            );
            if items.iter().any(|item| {
                matches!(item.outcome, ReviewReconciliationOutcome::Reconciled(_))
            }) {
                store.save(&composition).map_err(|error| {
                    format!(
                        "Observed review success was not recorded because the durable save failed; the operation remains frozen: {error}"
                    )
                })?;
            }
            Ok(ReviewReconciliationReport {
                composition,
                details,
                pending,
                items,
            })
        })
    }

    fn with_lock<T>(&self, action: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let _lock = self.target.acquire()?;
        action()
    }
}

fn reconcile_review_operations(
    current: &ReviewComposition,
    repository: &Repository,
    pull_request: u64,
    details: &PullRequestDetails,
    pending: Option<&PendingReviewSnapshot>,
) -> (ReviewComposition, Vec<ReviewReconciliationItem>) {
    let mut replacement = current.clone();
    let unresolved = current
        .operations_requiring_reconciliation()
        .cloned()
        .collect::<Vec<_>>();
    let mut items = Vec::with_capacity(unresolved.len());
    for operation in unresolved {
        let attempt_id = operation_attempt(&operation)
            .unwrap_or("missing-attempt-id")
            .to_owned();
        let concise_request = concise_request_summary(&operation);
        let frozen_request = frozen_request_summary(&operation);
        let observation = observe_review_operation(
            current,
            &operation,
            repository,
            pull_request,
            details,
            pending,
        );
        let outcome = match observation {
            Ok(ObservedReviewSuccess::Comment {
                review_id,
                comment_id,
                body,
                evidence,
            }) => match replacement.reconcile_observed_comment_success(
                &operation.id,
                review_id,
                comment_id,
                body,
            ) {
                Ok(()) => ReviewReconciliationOutcome::Reconciled(evidence),
                Err(error) => ReviewReconciliationOutcome::Unresolved(format!(
                    "Exact provider evidence could not be applied to local recovery: {error}"
                )),
            },
            Ok(ObservedReviewSuccess::Submission {
                review_id,
                evidence,
            }) => match replacement.reconcile_observed_submission_success(&operation.id, review_id)
            {
                Ok(()) => ReviewReconciliationOutcome::Reconciled(evidence),
                Err(error) => ReviewReconciliationOutcome::Unresolved(format!(
                    "Exact provider evidence could not be applied to local recovery: {error}"
                )),
            },
            Err(reason) => ReviewReconciliationOutcome::Unresolved(reason),
        };
        items.push(ReviewReconciliationItem {
            operation_id: operation.id,
            attempt_id,
            concise_request,
            frozen_request,
            outcome,
        });
    }
    (replacement, items)
}

enum ObservedReviewSuccess {
    Comment {
        review_id: Option<String>,
        comment_id: String,
        body: String,
        evidence: String,
    },
    Submission {
        review_id: String,
        evidence: String,
    },
}

fn observe_review_operation(
    composition: &ReviewComposition,
    operation: &ReviewOperation,
    repository: &Repository,
    pull_request: u64,
    details: &PullRequestDetails,
    pending: Option<&PendingReviewSnapshot>,
) -> Result<ObservedReviewSuccess, String> {
    let expected_key = ReviewKey::for_repository("github", repository, pull_request)
        .map_err(|error| format!("Local review identity is invalid: {error}"))?;
    if composition.key != expected_key || details.number != pull_request {
        return Err(
            "The fresh read does not identify the exact selected account/repository/pull request."
                .into(),
        );
    }
    if !details.activity_complete {
        return Err(
            "The fresh activity read is incomplete or truncated; exact outcome proof is unavailable."
                .into(),
        );
    }
    let payload = operation.payload.as_ref().ok_or_else(|| {
        "The frozen operation predates exact-payload recovery and cannot be proven by a read."
            .to_owned()
    })?;
    match payload {
        ReviewOperationPayload::PendingComment(intent) => {
            if operation.target
                != (ReviewOperationTarget::SynchronizePendingComment {
                    draft_id: intent.draft_id.clone(),
                })
                || intent.operation_id != operation.id
                || intent.key != expected_key
            {
                return Err(
                    "The frozen comment operation identity or target is inconsistent.".into(),
                );
            }
            let comment_id = intent.existing_comment_id.as_deref().ok_or_else(|| {
                "GitHub does not preserve this local attempt ID on a newly created comment. Without a returned remote comment ID, even identical body and position are ambiguous and remain unresolved."
                    .to_owned()
            })?;
            let review_id = intent.pending_review_id.as_deref().ok_or_else(|| {
                "The frozen comment edit has no exact parent pending-review ID; no identity proof is available."
                    .to_owned()
            })?;
            let pending = pending.ok_or_else(|| {
                "The selected-account read returned no pending review. Absence alone cannot prove whether the edit applied or was later deleted."
                    .to_owned()
            })?;
            validate_pending_review_identity(
                pending,
                repository,
                pull_request,
                review_id,
                &intent.position.commit_sha,
            )?;
            if !pending.comments_complete {
                return Err(
                    "The pending-review comment read is incomplete; absence or a partial match cannot prove this attempt."
                        .into(),
                );
            }
            let candidates = pending
                .comments
                .iter()
                .filter(|linked| {
                    linked.pull_request_review_id == review_id
                        && linked.comment.coordinates.remote_id == comment_id
                })
                .collect::<Vec<_>>();
            let [linked] = candidates.as_slice() else {
                return Err(if candidates.is_empty() {
                    "The complete pending-review read did not contain the exact known comment ID. Absence alone does not prove NotApplied after possible external deletion."
                        .into()
                } else {
                    "The provider read returned duplicate objects for the exact comment ID; identity is ambiguous."
                        .into()
                });
            };
            validate_pending_comment_identity(
                &linked.comment,
                repository,
                pull_request,
                comment_id,
                &intent.body,
                &intent.position,
            )?;
            let thread_comment = validate_thread_comment_identity(
                details,
                repository,
                pull_request,
                comment_id,
                &intent.body,
                &intent.position,
            )?;
            Ok(ObservedReviewSuccess::Comment {
                review_id: Some(review_id.to_owned()),
                comment_id: comment_id.to_owned(),
                body: thread_comment.body.clone(),
                evidence: format!(
                    "Fresh complete selected-account pending-review and activity-thread reads joined exact review {review_id} and comment {comment_id}; both matched author, head, path, range, and body, and the unique complete current thread proved side and start-side."
                ),
            })
        }
        ReviewOperationPayload::PendingFileComment(intent) => {
            if operation.target
                != (ReviewOperationTarget::SynchronizePendingFileComment {
                    draft_id: intent.draft_id.clone(),
                })
                || intent.operation_id != operation.id
                || intent.key != expected_key
            {
                return Err(
                    "The frozen file-comment operation identity or target is inconsistent.".into(),
                );
            }
            Err(
                "GitHub does not preserve this local attempt ID on the newly created file-level thread. Without the returned new thread and comment IDs, body, path, time, or list position cannot establish identity; the attempt remains unresolved and will not replay."
                    .into(),
            )
        }
        ReviewOperationPayload::ImmediateComment(intent) => {
            if operation.target
                != (ReviewOperationTarget::PostImmediateComment {
                    draft_id: intent.draft_id.clone(),
                })
                || intent.operation_id != operation.id
                || intent.key != expected_key
            {
                return Err(
                    "The frozen immediate-comment operation identity or target is inconsistent."
                        .into(),
                );
            }
            Err(
                "GitHub does not preserve this local attempt ID and the frozen immediate-comment request has no returned remote comment ID. Similar or identical activity cannot establish identity."
                    .into(),
            )
        }
        ReviewOperationPayload::Submission(intent) => {
            if operation.target
                != (ReviewOperationTarget::SubmitReview {
                    event: intent.event.clone(),
                })
                || intent.operation_id != operation.id
                || intent.key != expected_key
            {
                return Err(
                    "The frozen submission operation identity or target is inconsistent.".into(),
                );
            }
            let review_id = intent.pending_review_id.as_deref().ok_or_else(|| {
                "The frozen submission has no exact pending-review ID. A terminal review with similar content cannot be correlated to this attempt."
                    .to_owned()
            })?;
            if !details.activity_complete {
                return Err(
                    "The review activity read is incomplete; a terminal review cannot be proven from a truncated page."
                        .into(),
                );
            }
            if pending.is_some_and(|snapshot| snapshot.review.coordinates.remote_id == review_id) {
                return Err(
                    "The exact review is still reported as pending, so terminal submission success is not established."
                        .into(),
                );
            }
            if let Some(pending) = pending {
                validate_coordinates(
                    &pending.review.coordinates,
                    repository,
                    pull_request,
                    &pending.review.coordinates.remote_id,
                    "selected-account pending review",
                )?;
                validate_selected_author(
                    pending.review.author.as_deref(),
                    repository,
                    "selected-account pending review",
                )?;
            }
            let candidates = details
                .reviews
                .iter()
                .filter(|review| review.coordinates.remote_id == review_id)
                .collect::<Vec<_>>();
            let [review] = candidates.as_slice() else {
                return Err(if candidates.is_empty() {
                    "The complete activity read did not contain the exact known review ID. Absence alone cannot prove NotApplied after external deletion or retention changes."
                        .into()
                } else {
                    "The provider read returned duplicate objects for the exact review ID; identity is ambiguous."
                        .into()
                });
            };
            validate_coordinates(
                &review.coordinates,
                repository,
                pull_request,
                review_id,
                "review",
            )?;
            validate_selected_author(review.author.as_deref(), repository, "review")?;
            let expected_state = match intent.event {
                ReviewEvent::Comment => "COMMENTED",
                ReviewEvent::Approve => "APPROVED",
                ReviewEvent::RequestChanges => "CHANGES_REQUESTED",
            };
            if review.state != expected_state
                || review.submitted_at.is_none()
                || review.commit_sha.as_deref() != Some(intent.reviewed_commit_sha.as_str())
                || review.body != intent.body
            {
                return Err(format!(
                    "The exact review ID was observed, but event, terminal timestamp, reviewed head, or body differs from the frozen submission payload (expected {expected_state} at {}).",
                    intent.reviewed_commit_sha
                ));
            }
            Ok(ObservedReviewSuccess::Submission {
                review_id: review_id.to_owned(),
                evidence: format!(
                    "Fresh complete activity matched exact terminal review {review_id}, selected author, event {expected_state}, reviewed head, and body."
                ),
            })
        }
    }
}

fn validate_pending_review_identity(
    pending: &PendingReviewSnapshot,
    repository: &Repository,
    pull_request: u64,
    review_id: &str,
    reviewed_head: &str,
) -> Result<(), String> {
    validate_coordinates(
        &pending.review.coordinates,
        repository,
        pull_request,
        review_id,
        "pending review",
    )?;
    validate_selected_author(
        pending.review.author.as_deref(),
        repository,
        "pending review",
    )?;
    if pending.review.state != "PENDING"
        || pending.review.submitted_at.is_some()
        || pending.review.commit_sha.as_deref() != Some(reviewed_head)
    {
        return Err(
            "The exact parent review is not authoritatively pending on the frozen reviewed head."
                .into(),
        );
    }
    Ok(())
}

fn validate_pending_comment_identity(
    comment: &ReviewComment,
    repository: &Repository,
    pull_request: u64,
    comment_id: &str,
    body: &str,
    position: &PublishedPosition,
) -> Result<(), String> {
    validate_coordinates(
        &comment.coordinates,
        repository,
        pull_request,
        comment_id,
        "comment",
    )?;
    validate_selected_author(comment.author.as_deref(), repository, "comment")?;
    let side = match position.side {
        cibergit::participation::DiffSide::Old => "LEFT",
        cibergit::participation::DiffSide::New => "RIGHT",
    };
    if comment.outdated
        || comment.body != body
        || comment.path != position.path
        || comment
            .side
            .as_deref()
            .is_some_and(|observed| observed != side)
        || comment.line != Some(position.line)
        || comment.start_line != position.start_line
        || comment.commit_sha.as_deref() != Some(position.commit_sha.as_str())
    {
        return Err(
            "The pending-review read contained the exact comment ID, but author, head, path, range, body, or current-anchor state differs from the frozen edit payload."
                .into(),
        );
    }
    Ok(())
}

fn validate_thread_comment_identity<'a>(
    details: &'a PullRequestDetails,
    repository: &Repository,
    pull_request: u64,
    comment_id: &str,
    body: &str,
    position: &PublishedPosition,
) -> Result<&'a ReviewComment, String> {
    let candidates = details
        .review_threads
        .iter()
        .filter(|thread| {
            thread
                .comments
                .iter()
                .any(|comment| comment.coordinates.remote_id == comment_id)
        })
        .collect::<Vec<_>>();
    let [thread] = candidates.as_slice() else {
        return Err(if candidates.is_empty() {
            "The fresh complete activity read did not contain a review thread for the exact known comment ID; pending-review data alone does not carry authoritative diff-side evidence."
                .into()
        } else {
            "The fresh activity read associated the exact known comment ID with multiple review threads; identity is ambiguous."
                .into()
        });
    };
    validate_coordinates(
        &thread.coordinates,
        repository,
        pull_request,
        &thread.coordinates.remote_id,
        "review thread",
    )?;
    if !thread.comments_complete {
        return Err(
            "The exact comment appears in a review thread with incomplete comments; the join cannot prove a unique current placement."
                .into(),
        );
    }
    let side = match position.side {
        cibergit::participation::DiffSide::Old => "LEFT",
        cibergit::participation::DiffSide::New => "RIGHT",
    };
    let expected_start_side = position.start_line.map(|_| side);
    if thread.outdated
        || thread.path != position.path
        || thread.side.as_deref() != Some(side)
        || thread.start_side.as_deref() != expected_start_side
        || thread.line != Some(position.line)
        || thread.start_line != position.start_line
    {
        return Err(
            "The unique review thread for the exact comment ID has a different current path, end side/line, start side/line, or outdated state than the frozen edit payload."
                .into(),
        );
    }
    let comments = thread
        .comments
        .iter()
        .filter(|comment| comment.coordinates.remote_id == comment_id)
        .collect::<Vec<_>>();
    let [comment] = comments.as_slice() else {
        return Err(
            "The unique review thread returned duplicate entries for the exact comment ID; identity is ambiguous."
                .into(),
        );
    };
    validate_coordinates(
        &comment.coordinates,
        repository,
        pull_request,
        comment_id,
        "thread comment",
    )?;
    validate_selected_author(comment.author.as_deref(), repository, "thread comment")?;
    if comment.outdated
        || comment.body != body
        || comment.path != position.path
        || comment.side.as_deref() != Some(side)
        || comment.line != Some(position.line)
        || comment.start_line != position.start_line
        || comment.commit_sha.as_deref() != Some(position.commit_sha.as_str())
    {
        return Err(
            "The exact thread comment disagrees with the pending-review read or frozen author, head, path, side, range, body, or current-anchor state."
                .into(),
        );
    }
    Ok(comment)
}

fn validate_coordinates(
    coordinates: &ProviderCoordinates,
    repository: &Repository,
    pull_request: u64,
    remote_id: &str,
    kind: &str,
) -> Result<(), String> {
    if coordinates.provider != "github"
        || coordinates.host != repository.host
        || !coordinates.owner.eq_ignore_ascii_case(&repository.owner)
        || !coordinates
            .repository
            .eq_ignore_ascii_case(&repository.name)
        || coordinates.pull_request != pull_request
        || coordinates.remote_id != remote_id
    {
        return Err(format!(
            "The observed {kind} coordinates do not match the exact provider/repository/pull request/object ID."
        ));
    }
    Ok(())
}

fn validate_selected_author(
    author: Option<&str>,
    repository: &Repository,
    kind: &str,
) -> Result<(), String> {
    if !author.is_some_and(|author| author.eq_ignore_ascii_case(&repository.account.login)) {
        return Err(format!(
            "The observed {kind} author does not exactly match selected account {}.",
            repository.account.login
        ));
    }
    Ok(())
}

fn operation_attempt(operation: &ReviewOperation) -> Option<&str> {
    match &operation.status {
        ReviewOperationStatus::InFlight { attempt_id }
        | ReviewOperationStatus::Uncertain { attempt_id, .. } => Some(attempt_id),
        _ => None,
    }
}

fn concise_request_summary(operation: &ReviewOperation) -> String {
    match operation.payload.as_ref() {
        Some(ReviewOperationPayload::PendingComment(intent)) => format!(
            "Update pending comment · {}:{}",
            intent.position.path, intent.position.line
        ),
        Some(ReviewOperationPayload::ImmediateComment(intent)) => format!(
            "Publish comment · {}:{}",
            intent.position.path, intent.position.line
        ),
        Some(ReviewOperationPayload::PendingFileComment(intent)) => {
            let path = match &intent.target {
                cibergit::participation::ReviewCommentTarget::File(file) => file.path.as_str(),
                cibergit::participation::ReviewCommentTarget::Line(_) => "invalid-line-target",
            };
            format!("Add pending file comment · {path}")
        }
        Some(ReviewOperationPayload::Submission(_)) => "Submit pending review".into(),
        None => "Recover earlier review action".into(),
    }
}

fn frozen_position_summary(position: &cibergit::participation::PublishedPosition) -> String {
    let side = match position.side {
        cibergit::participation::DiffSide::Old => "LEFT",
        cibergit::participation::DiffSide::New => "RIGHT",
    };
    match position.start_line {
        Some(start) if start != position.line => {
            format!("{} {side} lines {start}–{}", position.path, position.line)
        }
        _ => format!("{} {side} line {}", position.path, position.line),
    }
}

fn frozen_request_summary(operation: &ReviewOperation) -> String {
    match operation.payload.as_ref() {
        Some(ReviewOperationPayload::PendingComment(intent)) => format!(
            "pending comment draft {} · review {} · comment {} · head {} · {} · body {:?}",
            intent.draft_id,
            intent.pending_review_id.as_deref().unwrap_or("unknown"),
            intent.existing_comment_id.as_deref().unwrap_or("unknown"),
            intent.position.commit_sha,
            frozen_position_summary(&intent.position),
            intent.body
        ),
        Some(ReviewOperationPayload::ImmediateComment(intent)) => format!(
            "immediate comment draft {} · remote comment unknown · head {} · {} · body {:?}",
            intent.draft_id,
            intent.position.commit_sha,
            frozen_position_summary(&intent.position),
            intent.body
        ),
        Some(ReviewOperationPayload::PendingFileComment(intent)) => {
            let target = match &intent.target {
                cibergit::participation::ReviewCommentTarget::File(file) => format!(
                    "file {} · base {} · head {}",
                    file.path, file.base_sha, file.commit_sha
                ),
                cibergit::participation::ReviewCommentTarget::Line(_) => {
                    "invalid line target".into()
                }
            };
            format!(
                "pending file comment draft {} · review {} · PR {} · {} · body {:?}",
                intent.draft_id,
                intent.pending.pending_review.remote_id,
                intent.pending.pull_request.remote_id,
                target,
                intent.body
            )
        }
        Some(ReviewOperationPayload::Submission(intent)) => format!(
            "submit {:?} · review {} · head {} · body {:?}",
            intent.event,
            intent.pending_review_id.as_deref().unwrap_or("unknown"),
            intent.reviewed_commit_sha,
            intent.body
        ),
        None => format!(
            "legacy frozen target {:?} · exact payload unavailable",
            operation.target
        ),
    }
}

fn load_composition(
    store: &DraftStore,
    key: &ReviewKey,
) -> Result<Option<ReviewComposition>, String> {
    match store.load(key).map_err(|error| error.to_string())? {
        LoadOutcome::Missing => Ok(None),
        LoadOutcome::Loaded(composition) => Ok(Some(*composition)),
        LoadOutcome::Corrupt { path, reason } => Err(format!(
            "Review recovery at {} is corrupt and was preserved: {reason}",
            path.display()
        )),
        LoadOutcome::FutureVersion { path, version } => Err(format!(
            "Review recovery at {} uses future version {version} and was preserved.",
            path.display()
        )),
    }
}

#[derive(Clone, Debug)]
pub struct InlineThread {
    pub thread: ReviewThread,
    pub anchor: Option<InlineAnchor>,
    pub unplaced_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineAnchor {
    pub file_key: String,
    pub side: cibergit::participation::DiffSide,
    pub start_line: u64,
    pub line: u64,
}

pub fn place_threads(session: &ReviewSession, details: &PullRequestDetails) -> Vec<InlineThread> {
    details
        .review_threads
        .iter()
        .cloned()
        .map(|thread| match place_thread(session, &thread) {
            Ok(anchor) => InlineThread {
                thread,
                anchor: Some(anchor),
                unplaced_reason: None,
            },
            Err(reason) => InlineThread {
                thread,
                anchor: None,
                unplaced_reason: Some(reason),
            },
        })
        .collect()
}

pub fn place_threads_with_canonical(
    displayed: &ReviewSession,
    canonical: &ReviewSession,
    details: &PullRequestDetails,
) -> Vec<InlineThread> {
    if displayed.revision() == canonical.revision() {
        return place_threads(canonical, details);
    }
    details
        .review_threads
        .iter()
        .cloned()
        .map(|thread| {
            let result = (|| {
                if displayed.revision().head_sha != canonical.revision().head_sha {
                    return Err(
                        "The selected comparison ends before the canonical reviewed head; return to Full PR to place this thread."
                            .to_owned(),
                    );
                }
                let canonical_anchor = place_thread(canonical, &thread)?;
                let displayed_file = displayed
                    .comparison()
                    .files
                    .iter()
                    .find(|file| file_key(file) == canonical_anchor.file_key)
                    .ok_or_else(|| {
                        "The thread file is absent from the selected comparison.".to_owned()
                    })?;
                let source = validate_coordinate(
                    displayed,
                    &file_key(displayed_file),
                    LineSelection {
                        side: canonical_anchor.side,
                        start_line: canonical_anchor.start_line,
                        line: canonical_anchor.line,
                    },
                )
                .map_err(|error| {
                    format!("The thread is not selectable in this direct pair: {error}")
                })?;
                let mapped = map_to_canonical_published(
                    &source,
                    CanonicalPublishedPatch::new(canonical.comparison(), canonical.metadata())
                        .map_err(|error| error.to_string())?,
                )
                .map_err(|error| format!("The thread cannot be proven against Full PR: {error}"))?;
                if mapped.path != displayed_file.path
                    || mapped.side != canonical_anchor.side
                    || mapped.start_line.unwrap_or(mapped.line) != canonical_anchor.start_line
                    || mapped.line != canonical_anchor.line
                {
                    return Err("The selected and canonical thread anchors differ.".into());
                }
                Ok(canonical_anchor)
            })();
            match result {
                Ok(anchor) => InlineThread {
                    thread,
                    anchor: Some(anchor),
                    unplaced_reason: None,
                },
                Err(reason) => InlineThread {
                    thread,
                    anchor: None,
                    unplaced_reason: Some(reason),
                },
            }
        })
        .collect()
}

fn place_thread(session: &ReviewSession, thread: &ReviewThread) -> Result<InlineAnchor, String> {
    match thread.subject {
        cibergit::domain::ReviewSubject::File => {
            return Err(
                "File-level discussion targets the whole file and is shown in Activity, never at a fabricated inline position."
                    .into(),
            );
        }
        cibergit::domain::ReviewSubject::Unknown => {
            return Err(
                "Provider subject provenance is unknown; this cached/legacy discussion is read-only."
                    .into(),
            );
        }
        cibergit::domain::ReviewSubject::Line => {}
    }
    if thread
        .comments
        .iter()
        .any(|comment| comment.subject != cibergit::domain::ReviewSubject::Line)
    {
        return Err("Thread/comment subject provenance disagrees; discussion is read-only.".into());
    }
    let file = session
        .comparison()
        .files
        .iter()
        .find(|file| file.path == thread.path)
        .ok_or_else(|| "File is not present in the displayed immutable comparison.".to_owned())?;
    if file.raw_path.is_some() {
        return Err(
            "The provider anchor uses a raw non-UTF-8 path; it is read-only and cannot be placed safely."
                .into(),
        );
    }
    let side = match thread.side.as_deref() {
        Some("LEFT") => cibergit::participation::DiffSide::Old,
        Some("RIGHT") => cibergit::participation::DiffSide::New,
        _ => return Err("The provider did not return an OLD/NEW side.".into()),
    };
    let newest = thread.comments.last();
    let (line, start_line, exact_commit) = if !thread.outdated {
        (
            thread.line,
            thread.start_line,
            newest.and_then(|comment| comment.commit_sha.as_deref()),
        )
    } else {
        if side == cibergit::participation::DiffSide::Old {
            return Err(
                "Outdated OLD-side anchors stay unplaced because equivalent base blobs cannot be inferred."
                    .into(),
            );
        }
        (
            thread.original_line,
            thread.original_start_line,
            newest.and_then(|comment| comment.original_commit_sha.as_deref()),
        )
    };
    let exact_commit = exact_commit.ok_or_else(|| {
        "The provider omitted the anchor commit; no revision equivalence was inferred.".to_owned()
    })?;
    if exact_commit != session.revision().head_sha {
        return Err(format!(
            "Anchor belongs to commit {}, while the displayed review is {}.",
            short_sha(exact_commit),
            short_sha(&session.revision().head_sha)
        ));
    }
    let line = line.ok_or_else(|| "The provider did not return a usable line.".to_owned())?;
    let start_line = start_line.unwrap_or(line);
    let expected_side = match side {
        cibergit::participation::DiffSide::Old => "LEFT",
        cibergit::participation::DiffSide::New => "RIGHT",
    };
    if thread
        .start_side
        .as_deref()
        .is_some_and(|candidate| candidate != expected_side)
    {
        return Err(
            "The provider returned a cross-side range, which cannot be mapped safely.".into(),
        );
    }
    let key = file_key(file);
    cibergit::participation::validate_coordinate(
        session,
        &key,
        LineSelection {
            side,
            start_line,
            line,
        },
    )
    .map_err(|error| format!("The provider anchor is not selectable in this patch: {error}"))?;
    Ok(InlineAnchor {
        file_key: key,
        side,
        start_line,
        line,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum JournalRequest {
    Auxiliary(Box<ReviewAuxiliaryRequest>),
    Merge {
        preparation: Box<MergePreparation>,
        request: MergeExecutionRequest,
    },
    Lifecycle(Box<PullRequestLifecycleRequest>),
    Discussion(Box<PullRequestDiscussionRequest>),
}

impl JournalRequest {
    fn operation_and_attempt(&self) -> (&str, &str) {
        match self {
            Self::Auxiliary(request) => (&request.operation_id, &request.attempt_id),
            Self::Merge { request, .. } => (&request.operation_id, &request.attempt_id),
            Self::Lifecycle(request) => (&request.operation_id, &request.attempt_id),
            Self::Discussion(request) => (&request.operation_id, &request.attempt_id),
        }
    }

    fn mutation_context(&self) -> MutationContext {
        let (operation_id, attempt_id) = self.operation_and_attempt();
        let action = match self {
            Self::Auxiliary(request) => match &request.action {
                cibergit::domain::ReviewAuxiliaryAction::UpdatePendingSummary { .. } => {
                    "update pending review summary"
                }
                cibergit::domain::ReviewAuxiliaryAction::UpdateSubmittedSummary { .. } => {
                    "update submitted review summary"
                }
                cibergit::domain::ReviewAuxiliaryAction::DeletePendingComment { .. } => {
                    "delete pending review comment"
                }
                cibergit::domain::ReviewAuxiliaryAction::CancelPendingReview { .. } => {
                    "cancel pending review"
                }
                cibergit::domain::ReviewAuxiliaryAction::Reply { .. } => "reply to review thread",
                cibergit::domain::ReviewAuxiliaryAction::SetThreadResolved {
                    resolved: true,
                    ..
                } => "resolve review thread",
                cibergit::domain::ReviewAuxiliaryAction::SetThreadResolved {
                    resolved: false,
                    ..
                } => "unresolve review thread",
            },
            Self::Merge { request, .. } => match request.action {
                cibergit::domain::MergeAction::Merge { .. } => "merge pull request",
                cibergit::domain::MergeAction::EnableAutoMerge { .. } => "enable auto-merge",
                cibergit::domain::MergeAction::DisableAutoMerge => "disable auto-merge",
                cibergit::domain::MergeAction::Enqueue => "enqueue pull request",
                cibergit::domain::MergeAction::Dequeue => "dequeue pull request",
            },
            Self::Lifecycle(request) => match &request.action {
                cibergit::domain::PullRequestLifecycleAction::UpdateTitle { .. } => {
                    "update-pr-title"
                }
                cibergit::domain::PullRequestLifecycleAction::UpdateBody { .. } => "update-pr-body",
                cibergit::domain::PullRequestLifecycleAction::UpdateBaseBranch { .. } => {
                    "update-pr-base"
                }
                cibergit::domain::PullRequestLifecycleAction::Close => "close-pr",
                cibergit::domain::PullRequestLifecycleAction::Reopen => "reopen-pr",
                cibergit::domain::PullRequestLifecycleAction::ConvertToDraft => {
                    "convert-pr-to-draft"
                }
                cibergit::domain::PullRequestLifecycleAction::MarkReadyForReview => "mark-pr-ready",
                cibergit::domain::PullRequestLifecycleAction::AddReviewer(_) => "add-pr-reviewer",
                cibergit::domain::PullRequestLifecycleAction::RemoveReviewer(_) => {
                    "remove-pr-reviewer"
                }
                cibergit::domain::PullRequestLifecycleAction::AddLabel(_) => "add-pr-label",
                cibergit::domain::PullRequestLifecycleAction::RemoveLabel(_) => "remove-pr-label",
                cibergit::domain::PullRequestLifecycleAction::AddAssignee(_) => "add-pr-assignee",
                cibergit::domain::PullRequestLifecycleAction::RemoveAssignee(_) => {
                    "remove-pr-assignee"
                }
            },
            Self::Discussion(request) => match request.action {
                cibergit::domain::PullRequestDiscussionAction::Create { .. } => {
                    "create-pr-discussion-comment"
                }
                cibergit::domain::PullRequestDiscussionAction::Edit { .. } => {
                    "edit-pr-discussion-comment"
                }
                cibergit::domain::PullRequestDiscussionAction::Delete { .. } => {
                    "delete-pr-discussion-comment"
                }
            },
        };
        let payload = match self {
            Self::Lifecycle(request) => serde_json::json!({
                "request": request,
                "dispatch": {"transport": "journal-context"},
            }),
            Self::Discussion(request) => serde_json::json!({
                "request": request,
                "dispatch": {"transport": "journal-context"},
            }),
            _ => serde_json::to_value(self)
                .unwrap_or_else(|_| serde_json::json!({"serialization": "failed"})),
        };
        MutationContext {
            operation_id: operation_id.to_owned(),
            attempt_id: attempt_id.to_owned(),
            action: action.into(),
            payload,
        }
    }

    fn validate_provider_context(&self, context: &MutationContext) -> Result<(), String> {
        let expected = self.mutation_context();
        if context.operation_id != expected.operation_id
            || context.attempt_id != expected.attempt_id
            || context.action != expected.action
        {
            return Err(
                "provider mutation identity differs from the frozen journal request".into(),
            );
        }
        let expected_request = expected.payload.get("request").ok_or_else(|| {
            "journal request is not supported by held provider admission".to_owned()
        })?;
        if context.payload.get("request") != Some(expected_request) {
            return Err("provider mutation payload differs from the frozen journal request".into());
        }
        if context.payload.get("dispatch").is_none() {
            return Err("provider mutation context omitted its exact dispatch payload".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum JournalStatus {
    InFlight,
    Uncertain {
        reason: String,
    },
    Acknowledged {
        accepted: bool,
        completed: bool,
        summary: String,
    },
    NotApplied {
        evidence: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalOperation {
    pub request: JournalRequest,
    pub status: JournalStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct JournalRecord {
    version: u64,
    key: ReviewKey,
    operations: Vec<JournalOperation>,
}

#[derive(Clone, Debug)]
enum JournalLoad {
    Ready(Vec<JournalOperation>),
    RecoveryRequired(String),
}

#[derive(Clone, Debug)]
pub struct ActionJournal {
    root: PathBuf,
    key: ReviewKey,
    target: TargetMutationAuthority,
}

impl ActionJournal {
    pub fn open(root: &Path, key: ReviewKey) -> Result<Self, String> {
        ensure_private_directory(root)?;
        Ok(Self {
            root: root.to_owned(),
            target: TargetMutationAuthority::new(root.to_owned(), key.clone())?,
            key,
        })
    }

    fn load_unlocked(&self) -> Result<JournalLoad, String> {
        let path = self.path()?;
        let bytes = match read_bounded(&path, MAX_JOURNAL_BYTES) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(JournalLoad::Ready(Vec::new())),
            Err(error) => return Err(error),
        };
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(error) => {
                return Ok(JournalLoad::RecoveryRequired(format!(
                    "Action journal {} is corrupt and was preserved: {error}",
                    path.display()
                )));
            }
        };
        let Some(version) = value.get("version").and_then(serde_json::Value::as_u64) else {
            return Ok(JournalLoad::RecoveryRequired(format!(
                "Action journal {} has no version and was preserved.",
                path.display()
            )));
        };
        if version != JOURNAL_VERSION {
            return Ok(JournalLoad::RecoveryRequired(format!(
                "Action journal {} uses unsupported version {version} and was preserved.",
                path.display()
            )));
        }
        let record: JournalRecord = match serde_json::from_value(value) {
            Ok(record) => record,
            Err(error) => {
                return Ok(JournalLoad::RecoveryRequired(format!(
                    "Action journal {} is invalid and was preserved: {error}",
                    path.display()
                )));
            }
        };
        if record.key != self.key || !journal_is_bounded(&record.operations) {
            return Ok(JournalLoad::RecoveryRequired(format!(
                "Action journal {} failed identity or bounds validation and was preserved.",
                path.display()
            )));
        }
        Ok(JournalLoad::Ready(record.operations))
    }

    /// Must complete before the provider closure is invoked. If this save
    /// fails, the closure is not called and therefore zero writes are sent.
    pub fn dispatch<T>(
        &self,
        request: JournalRequest,
        execute: impl FnOnce() -> ProviderMutationOutcome<T>,
        acknowledged: impl FnOnce(&T) -> (bool, bool, String),
    ) -> ProviderMutationOutcome<T> {
        let lock = match self.target.acquire() {
            Ok(lock) => lock,
            Err(reason) => {
                return ProviderMutationOutcome::PreflightRejected {
                    reason: format!(
                        "Could not lock the caller journal before dispatch; zero writes sent: {reason}"
                    ),
                };
            }
        };
        if let Err(reason) = self.record_in_flight_unlocked(request.clone()) {
            return ProviderMutationOutcome::PreflightRejected {
                reason: format!(
                    "Could not durably journal the exact request before dispatch; zero writes sent: {reason}"
                ),
            };
        }
        let outcome = execute();
        let update = match &outcome {
            ProviderMutationOutcome::PreflightRejected { reason } => JournalStatus::NotApplied {
                evidence: format!("Provider preflight rejected before mutation dispatch: {reason}"),
            },
            ProviderMutationOutcome::Uncertain { reason, .. } => JournalStatus::Uncertain {
                reason: reason.clone(),
            },
            ProviderMutationOutcome::Acknowledged(value) => {
                let (accepted, completed, summary) = acknowledged(value);
                JournalStatus::Acknowledged {
                    accepted,
                    completed,
                    summary,
                }
            }
        };
        if let Err(error) = self.update_status_unlocked(&request, update) {
            return match outcome {
                ProviderMutationOutcome::Uncertain { context, reason } => {
                    ProviderMutationOutcome::Uncertain {
                        context,
                        reason: format!("{reason}; journal update also failed: {error}"),
                    }
                }
                ProviderMutationOutcome::Acknowledged(_) => ProviderMutationOutcome::Uncertain {
                    context: request.mutation_context(),
                    reason: format!(
                        "The provider acknowledged the write, but its durable journal outcome could not be saved: {error}. Reconcile by authoritative read; do not replay blindly."
                    ),
                },
                ProviderMutationOutcome::PreflightRejected { reason } => {
                    ProviderMutationOutcome::PreflightRejected {
                        reason: format!(
                            "{reason}; the no-write outcome could not be saved to the caller journal: {error}"
                        ),
                    }
                }
            };
        }
        drop(lock);
        outcome
    }

    #[allow(dead_code)] // Retained for future explicit evidence-selection; no safe automatic negative proof exists today.
    pub fn mark_not_applied(
        &self,
        operation_id: &str,
        attempt_id: &str,
        evidence: String,
    ) -> Result<(), String> {
        let _lock = self.target.acquire()?;
        let operation = self
            .operations_unlocked()?
            .into_iter()
            .find(|operation| {
                operation.request.operation_and_attempt() == (operation_id, attempt_id)
            })
            .ok_or_else(|| "Journal operation does not exist.".to_owned())?;
        if !matches!(
            operation.status,
            JournalStatus::InFlight | JournalStatus::Uncertain { .. }
        ) {
            return Err("Journal operation already has a terminal outcome.".into());
        }
        self.update_status_unlocked(&operation.request, JournalStatus::NotApplied { evidence })
    }

    pub fn mark_acknowledged(
        &self,
        operation_id: &str,
        attempt_id: &str,
        completed: bool,
        summary: String,
    ) -> Result<(), String> {
        let _lock = self.target.acquire()?;
        let operation = self
            .operations_unlocked()?
            .into_iter()
            .find(|operation| {
                operation.request.operation_and_attempt() == (operation_id, attempt_id)
            })
            .ok_or_else(|| "Journal operation does not exist.".to_owned())?;
        if !matches!(
            operation.status,
            JournalStatus::InFlight | JournalStatus::Uncertain { .. }
        ) {
            return Err("Journal operation already has a terminal outcome.".into());
        }
        self.update_status_unlocked(
            &operation.request,
            JournalStatus::Acknowledged {
                accepted: true,
                completed,
                summary,
            },
        )
    }

    pub fn operations(&self) -> Result<Vec<JournalOperation>, String> {
        let _lock = self.target.acquire()?;
        self.operations_unlocked()
    }

    fn operations_unlocked(&self) -> Result<Vec<JournalOperation>, String> {
        match self.load_unlocked()? {
            JournalLoad::Ready(operations) => Ok(operations),
            JournalLoad::RecoveryRequired(reason) => Err(reason),
        }
    }

    fn record_in_flight_unlocked(&self, request: JournalRequest) -> Result<(), String> {
        let mut operations = self.operations_unlocked()?;
        if operations.iter().any(|operation| {
            matches!(
                operation.status,
                JournalStatus::InFlight | JournalStatus::Uncertain { .. }
            )
        }) {
            return Err(
                "Another auxiliary or merge attempt still needs outcome reconciliation.".into(),
            );
        }
        if self.review_operations_unresolved()? {
            return Err(
                "A review mutation still needs exact outcome reconciliation; no target mutation was admitted."
                    .into(),
            );
        }
        let identity = request.operation_and_attempt();
        if operations
            .iter()
            .any(|operation| operation.request.operation_and_attempt() == identity)
        {
            return Err("This exact request attempt is already journaled.".into());
        }
        operations.push(JournalOperation {
            request,
            status: JournalStatus::InFlight,
        });
        if operations.len() > MAX_JOURNAL_OPERATIONS {
            let removable = operations
                .iter()
                .position(|operation| {
                    matches!(
                        operation.status,
                        JournalStatus::Acknowledged { .. } | JournalStatus::NotApplied { .. }
                    )
                })
                .ok_or_else(|| {
                    "Action journal reached its unresolved-operation bound.".to_owned()
                })?;
            operations.remove(removable);
        }
        self.save(&operations)
    }

    fn update_status_unlocked(
        &self,
        request: &JournalRequest,
        status: JournalStatus,
    ) -> Result<(), String> {
        match &status {
            JournalStatus::Uncertain { reason }
            | JournalStatus::NotApplied { evidence: reason }
                if reason.is_empty() || reason.len() > MAX_JOURNAL_TEXT_BYTES =>
            {
                return Err("Journal outcome explanation is empty or too large.".into());
            }
            JournalStatus::Acknowledged { summary, .. }
                if summary.is_empty() || summary.len() > MAX_JOURNAL_TEXT_BYTES =>
            {
                return Err("Journal acknowledgement summary is empty or too large.".into());
            }
            _ => {}
        }
        let mut operations = self.operations_unlocked()?;
        let identity = request.operation_and_attempt();
        let operation = operations
            .iter_mut()
            .find(|operation| operation.request.operation_and_attempt() == identity)
            .ok_or_else(|| "Journal operation disappeared before outcome save.".to_owned())?;
        if operation.request != *request {
            return Err("Journal request payload changed for an existing attempt.".into());
        }
        operation.status = status;
        self.save(&operations)
    }

    fn save(&self, operations: &[JournalOperation]) -> Result<(), String> {
        if !journal_is_bounded(operations) {
            return Err("Action journal exceeds its bounds.".into());
        }
        let record = JournalRecord {
            version: JOURNAL_VERSION,
            key: self.key.clone(),
            operations: operations.to_vec(),
        };
        let bytes = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_JOURNAL_BYTES {
            return Err(format!("Action journal exceeds {MAX_JOURNAL_BYTES} bytes."));
        }
        atomic_private_write(&self.path()?, &bytes)
    }

    fn path(&self) -> Result<PathBuf, String> {
        let account = stable_component(&(
            self.key.account.host.as_str(),
            self.key.account.login.as_str(),
        ))?;
        let repository = stable_component(&(
            self.key.provider.as_str(),
            self.key.host.as_str(),
            self.key.owner.as_str(),
            self.key.repository.as_str(),
        ))?;
        Ok(self
            .root
            .join("action-journal")
            .join("v1")
            .join(account)
            .join(repository)
            .join(format!("pr-{}.json", self.key.pull_request)))
    }

    fn review_operations_unresolved(&self) -> Result<bool, String> {
        let store =
            DraftStore::open(self.root.join("drafts")).map_err(|error| error.to_string())?;
        Ok(
            load_composition(&store, &self.key)?.is_some_and(|composition| {
                composition
                    .operations_requiring_reconciliation()
                    .next()
                    .is_some()
            }),
        )
    }

    pub fn admission(&mut self, request: JournalRequest) -> JournalAdmission<'_> {
        JournalAdmission {
            journal: self,
            request,
        }
    }
}

/// Adapter between the provider's held `MutationAdmission` contract and the
/// app's one shared per-target action journal.
pub struct JournalAdmission<'a> {
    journal: &'a mut ActionJournal,
    request: JournalRequest,
}

impl MutationAdmission for JournalAdmission<'_> {
    fn admit<'a>(
        &'a mut self,
        context: &MutationContext,
    ) -> anyhow::Result<Box<dyn AdmittedMutationAttempt + 'a>> {
        self.request
            .validate_provider_context(context)
            .map_err(anyhow::Error::msg)?;
        let guard = self.journal.target.acquire().map_err(anyhow::Error::msg)?;
        self.journal
            .record_in_flight_unlocked(self.request.clone())
            .map_err(anyhow::Error::msg)?;
        let (operation_id, attempt_id) = self.request.operation_and_attempt();
        let operation_id = operation_id.to_owned();
        let attempt_id = attempt_id.to_owned();
        let durable_record_id = stable_component(&(
            self.journal.key.provider.as_str(),
            self.journal.key.host.as_str(),
            self.journal.key.owner.as_str(),
            self.journal.key.repository.as_str(),
            self.journal.key.pull_request,
            self.journal.key.account.host.as_str(),
            self.journal.key.account.login.as_str(),
            operation_id.as_str(),
            attempt_id.as_str(),
            context,
        ))
        .map_err(anyhow::Error::msg)?;
        Ok(Box::new(HeldJournalAttempt {
            journal: self.journal.clone(),
            request: self.request.clone(),
            receipt: MutationAdmissionReceipt {
                operation_id,
                attempt_id,
                durable_record_id,
            },
            _guard: guard,
        }))
    }
}

struct HeldJournalAttempt {
    journal: ActionJournal,
    request: JournalRequest,
    receipt: MutationAdmissionReceipt,
    _guard: TargetMutationGuard,
}

impl AdmittedMutationAttempt for HeldJournalAttempt {
    fn receipt(&self) -> &MutationAdmissionReceipt {
        &self.receipt
    }

    fn record_terminal(&mut self, record: &MutationTerminalRecord) -> anyhow::Result<()> {
        let status = match record {
            MutationTerminalRecord::NotStarted { reason } => JournalStatus::NotApplied {
                evidence: format!("Provider second preflight dispatched zero writes: {reason}"),
            },
            MutationTerminalRecord::Acknowledged { acknowledgement } => {
                let encoded = serde_json::to_string(acknowledgement)?;
                JournalStatus::Acknowledged {
                    accepted: true,
                    completed: true,
                    summary: format!("Durable provider acknowledgement: {encoded}"),
                }
            }
            MutationTerminalRecord::Uncertain { reason } => JournalStatus::Uncertain {
                reason: reason.clone(),
            },
        };
        self.journal
            .update_status_unlocked(&self.request, status)
            .map_err(anyhow::Error::msg)
    }
}

pub fn dispatch_auxiliary(
    journal: &ActionJournal,
    provider: &cibergit::providers::GithubProvider,
    repository: &Repository,
    pull_request: u64,
    request: &ReviewAuxiliaryRequest,
) -> ProviderMutationOutcome<ReviewAuxiliaryAcknowledgement> {
    journal.dispatch(
        JournalRequest::Auxiliary(Box::new(request.clone())),
        || provider.execute_review_auxiliary(repository, pull_request, request),
        |ack| {
            (
                true,
                true,
                format!(
                    "review={:?}, comment={:?}, thread={:?}, resolved={:?}",
                    ack.review_id, ack.comment_id, ack.thread_id, ack.resolved
                ),
            )
        },
    )
}

pub fn dispatch_merge(
    journal: &ActionJournal,
    provider: &cibergit::providers::GithubProvider,
    repository: &Repository,
    preparation: &MergePreparation,
    request: &MergeExecutionRequest,
) -> ProviderMutationOutcome<MergeAcknowledgement> {
    journal.dispatch(
        JournalRequest::Merge {
            preparation: Box::new(preparation.clone()),
            request: request.clone(),
        },
        || provider.execute_merge(repository, preparation, request),
        |ack| {
            (
                ack.accepted,
                ack.completed,
                format!(
                    "accepted={}, completed={}, merged={}, commit={:?}",
                    ack.accepted, ack.completed, ack.merged, ack.merge_commit_sha
                ),
            )
        },
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct MergePreferenceRecord {
    version: u64,
    method: MergeMethod,
}

pub fn load_merge_preference(
    root: &Path,
    repository: &Repository,
) -> Result<Option<MergeMethod>, String> {
    let path = merge_preference_path(root, repository)?;
    let Some(bytes) = read_bounded(&path, 16 * 1024)? else {
        return Ok(None);
    };
    let record: MergePreferenceRecord = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "Merge preference {} is unreadable and was preserved: {error}",
            path.display()
        )
    })?;
    if record.version != JOURNAL_VERSION {
        return Err(format!(
            "Merge preference {} uses unsupported version {} and was preserved.",
            path.display(),
            record.version
        ));
    }
    Ok(Some(record.method))
}

pub fn save_merge_preference(
    root: &Path,
    repository: &Repository,
    method: MergeMethod,
) -> Result<(), String> {
    let path = merge_preference_path(root, repository)?;
    let bytes = serde_json::to_vec(&MergePreferenceRecord {
        version: JOURNAL_VERSION,
        method,
    })
    .map_err(|error| error.to_string())?;
    atomic_private_write(&path, &bytes)
}

pub fn next_attempt_id(operation_id: &str) -> String {
    format!(
        "{operation_id}-attempt-{}-{}",
        std::process::id(),
        ATTEMPT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn merge_preference_path(root: &Path, repository: &Repository) -> Result<PathBuf, String> {
    let component = stable_component(&(
        repository.host.as_str(),
        repository.owner.as_str(),
        repository.name.as_str(),
        repository.account.host.as_str(),
        repository.account.login.as_str(),
    ))?;
    Ok(root
        .join("merge-preferences")
        .join(format!("{component}.json")))
}

fn journal_is_bounded(operations: &[JournalOperation]) -> bool {
    operations.len() <= MAX_JOURNAL_OPERATIONS
        && operations.iter().all(|operation| {
            let (operation_id, attempt_id) = operation.request.operation_and_attempt();
            !operation_id.is_empty()
                && operation_id.len() <= 512
                && !attempt_id.is_empty()
                && attempt_id.len() <= 512
                && match &operation.status {
                    JournalStatus::Uncertain { reason } => {
                        !reason.is_empty() && reason.len() <= MAX_JOURNAL_TEXT_BYTES
                    }
                    JournalStatus::Acknowledged { summary, .. } => {
                        !summary.is_empty() && summary.len() <= MAX_JOURNAL_TEXT_BYTES
                    }
                    JournalStatus::NotApplied { evidence } => {
                        !evidence.is_empty() && evidence.len() <= MAX_JOURNAL_TEXT_BYTES
                    }
                    JournalStatus::InFlight => true,
                }
        })
}

fn stable_component(value: &impl Serialize) -> Result<String, String> {
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| {
        format!(
            "Cannot create private directory {}: {error}",
            path.display()
        )
    })?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        format!(
            "Cannot protect private directory {}: {error}",
            path.display()
        )
    })
}

fn open_private_lock(path: &Path) -> Result<File, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Private lock has no parent directory.".to_owned())?;
    ensure_private_directory(parent)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("Cannot open private lock {}: {error}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Cannot protect private lock {}: {error}", path.display()))?;
    Ok(file)
}

fn read_bounded(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, String> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Cannot open {}: {error}", path.display())),
    };
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    if bytes.len() > limit {
        return Err(format!("{} exceeds its {limit}-byte bound", path.display()));
    }
    Ok(Some(bytes))
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Private record has no parent directory.".to_owned())?;
    ensure_private_directory(parent)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".cibergit-{}-{sequence}.tmp", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| format!("Cannot create {}: {error}", temporary.display()))?;
        file.write_all(bytes)
            .map_err(|error| format!("Cannot write {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("Cannot sync {}: {error}", temporary.display()))?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("Cannot replace {}: {error}", path.display()))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("Cannot sync {}: {error}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::domain::{
        Account, ChangedFile, Comparison, LinkedReviewComment, MergeEligibility,
        PendingFileCommentSource, ProviderCoordinates, PullRequestLifecycleAction,
        PullRequestMutationTarget, PullRequestReview, ReviewComment, Revision,
    };
    use cibergit::participation::{
        DiffSide, RemoteDraftIds, ReviewCommentTarget, ReviewOperationStatus,
    };
    use std::{
        process::Command,
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };
    use tempfile::tempdir;

    fn repository() -> Repository {
        Repository {
            host: "github.com".into(),
            owner: "octo".into(),
            name: "repo".into(),
            account: Account {
                host: "github.com".into(),
                login: "reader".into(),
            },
            local_path: None,
        }
    }

    fn session() -> ReviewSession {
        ReviewSession::new(Comparison {
            revision: Revision {
                base_sha: "1111111".into(),
                head_sha: "2222222".into(),
            },
            files: vec![ChangedFile {
                path: "src/lib.rs".into(),
                previous_path: None,
                raw_path: None,
                raw_previous_path: None,
                status: "modified".into(),
                additions: 1,
                deletions: 1,
                patch: Some("@@ -1,2 +1,2 @@\n-old\n+new\n same".into()),
                patch_complete: true,
            }],
            complete: true,
            notice: None,
        })
    }

    fn review_key() -> ReviewKey {
        ReviewKey::for_repository("github", &repository(), 7).unwrap()
    }

    fn pending_file_source() -> PendingFileCommentSource {
        PendingFileCommentSource {
            viewer_login: "reader".into(),
            repository: repository(),
            pull_request: ProviderCoordinates {
                provider: "github".into(),
                host: "github.com".into(),
                owner: "octo".into(),
                repository: "repo".into(),
                pull_request: 7,
                remote_id: "PR_node".into(),
            },
            pull_request_state: "OPEN".into(),
            current_base_sha: "1111111".into(),
            current_head_sha: "2222222".into(),
            review: ProviderCoordinates {
                provider: "github".into(),
                host: "github.com".into(),
                owner: "octo".into(),
                repository: "repo".into(),
                pull_request: 7,
                remote_id: "REVIEW_pending".into(),
            },
            review_author: "reader".into(),
            review_commit_sha: "2222222".into(),
        }
    }

    #[test]
    fn direct_pair_comments_are_reanchored_only_after_canonical_proof() {
        let root = tempdir().unwrap();
        let canonical = session();
        let mut direct_comparison = canonical.comparison().clone();
        direct_comparison.revision.base_sha = "9999999".into();
        let mut displayed = ReviewSession::new(direct_comparison.clone());
        displayed.select_comparison(
            direct_comparison,
            cibergit::review::ComparisonMetadata {
                mode: cibergit::review::ComparisonMode::Commit {
                    sha: "2222222".into(),
                },
                requested_mode: None,
                notice: None,
            },
        );
        let mut controller =
            match ReviewInteractionController::load(root.path(), &repository(), 7, &canonical)
                .unwrap()
            {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };

        controller
            .select_line_with_canonical(
                &displayed,
                &canonical,
                LineSelection::single(DiffSide::New, 1),
            )
            .unwrap();
        let composer = controller.composer.as_ref().unwrap();
        assert_eq!(composer.coordinate.reviewed_revision, *canonical.revision());
        assert_eq!(
            composer
                .source_coordinate
                .as_ref()
                .unwrap()
                .reviewed_revision,
            *displayed.revision()
        );
        controller.stage_composer_text("mapped".into()).unwrap();

        let old_error = controller
            .select_line_with_canonical(
                &displayed,
                &canonical,
                LineSelection::single(DiffSide::Old, 1),
            )
            .unwrap_err();
        assert!(old_error.contains("OLD-side"), "{old_error}");

        let mut older_comparison = displayed.comparison().clone();
        older_comparison.revision.head_sha = "3333333".into();
        let older = ReviewSession::new(older_comparison);
        let older_error = controller
            .select_line_with_canonical(&older, &canonical, LineSelection::single(DiffSide::New, 1))
            .unwrap_err();
        assert!(
            older_error.contains("older than the reviewed head"),
            "{older_error}"
        );
    }

    #[test]
    fn file_draft_restarts_on_exact_target_and_prepares_only_fresh_pending_source() {
        let root = tempdir().unwrap();
        let session = session();
        let mut controller =
            match ReviewInteractionController::load(root.path(), &repository(), 7, &session)
                .unwrap()
            {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };
        controller
            .select_file_with_canonical(&session, &session)
            .unwrap();
        let saved = controller
            .stage_composer_text("Whole-file rationale".into())
            .unwrap();
        controller.store.save(&saved).unwrap();
        let draft_id = controller
            .file_composer
            .as_ref()
            .and_then(|composer| composer.draft_id.clone())
            .unwrap();
        let target = controller.file_composer.as_ref().unwrap().target.clone();
        controller.finish_composer_save(&saved, &draft_id, "Whole-file rationale", Ok(()));
        drop(controller);

        let mut restarted =
            match ReviewInteractionController::load(root.path(), &repository(), 7, &session)
                .unwrap()
            {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };
        assert!(restarted.file_composer.is_none());
        let reopened = restarted.reopen_file_draft(&draft_id).unwrap();
        assert!(reopened.durable);
        assert_eq!(reopened.body, "Whole-file rationale");
        assert_eq!(reopened.target, target);

        let operation_id = restarted
            .prepare_pending_file_comment(&pending_file_source())
            .unwrap();
        let operation = restarted
            .composition
            .operations
            .iter()
            .find(|operation| operation.id == operation_id)
            .unwrap();
        let Some(ReviewOperationPayload::PendingFileComment(intent)) = &operation.payload else {
            panic!("file draft must produce its distinct frozen payload")
        };
        assert_eq!(intent.body, "Whole-file rationale");
        assert_eq!(intent.target, ReviewCommentTarget::File(target));
        assert_eq!(intent.pending.pending_review.remote_id, "REVIEW_pending");

        restarted
            .composition
            .cancel_prepared(&operation_id)
            .unwrap();
        let mut changed = pending_file_source();
        changed.current_head_sha = "different-head".into();
        let error = restarted
            .prepare_pending_file_comment(&changed)
            .unwrap_err();
        assert!(
            error.contains("canonical file") || error.contains("identity"),
            "{error}"
        );
    }

    fn auxiliary_request() -> ReviewAuxiliaryRequest {
        ReviewAuxiliaryRequest {
            operation_id: "aux-1".into(),
            attempt_id: "attempt-1".into(),
            action: cibergit::domain::ReviewAuxiliaryAction::SetThreadResolved {
                thread: ProviderCoordinates {
                    provider: "github".into(),
                    host: "github.com".into(),
                    owner: "octo".into(),
                    repository: "repo".into(),
                    pull_request: 7,
                    remote_id: "thread-1".into(),
                },
                resolved: true,
            },
        }
    }

    fn submitted_edit_request() -> ReviewAuxiliaryRequest {
        ReviewAuxiliaryRequest {
            operation_id: "submitted-edit-1".into(),
            attempt_id: "submitted-edit-attempt-1".into(),
            action: cibergit::domain::ReviewAuxiliaryAction::UpdateSubmittedSummary {
                review: coordinates("REVIEW_owned"),
                selected_author: "reader".into(),
                submitted_state: "APPROVED".into(),
                submitted_commit_sha: "1".repeat(40),
                expected_body: "historical body before edit".into(),
                body: "requested body after edit".into(),
            },
        }
    }

    #[test]
    fn submitted_edit_journal_freezes_distinct_historical_request() {
        let directory = tempdir().unwrap();
        let journal = ActionJournal::open(directory.path(), review_key()).unwrap();
        let request = submitted_edit_request();
        let frozen = JournalRequest::Auxiliary(Box::new(request.clone()));
        let context = frozen.mutation_context();
        assert_eq!(context.action, "update submitted review summary");
        let encoded = serde_json::to_string(&context.payload).unwrap();
        assert!(encoded.contains("historical body before edit"));
        assert!(encoded.contains("requested body after edit"));
        assert!(encoded.contains("REVIEW_owned"));

        let outcome = journal.dispatch(
            frozen,
            || ProviderMutationOutcome::<()>::Uncertain {
                context,
                reason: "provider acknowledgement was malformed".into(),
            },
            |_| (true, true, "unexpected".into()),
        );
        assert!(matches!(outcome, ProviderMutationOutcome::Uncertain { .. }));
        let restored = ActionJournal::open(directory.path(), review_key())
            .unwrap()
            .operations()
            .unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(
            restored[0].request,
            JournalRequest::Auxiliary(Box::new(request))
        );
        assert!(matches!(
            restored[0].status,
            JournalStatus::Uncertain { .. }
        ));
    }

    fn lifecycle_request() -> PullRequestLifecycleRequest {
        PullRequestLifecycleRequest {
            operation_id: "lifecycle-1".into(),
            attempt_id: "lifecycle-attempt-1".into(),
            target: PullRequestMutationTarget {
                repository: repository(),
                pull_request: coordinates("PR_7"),
                observed_updated_at: "2026-09-13T00:00:00Z".into(),
                observed_state: "OPEN".into(),
                observed_head_sha: "2222222".into(),
            },
            action: PullRequestLifecycleAction::Close,
        }
    }

    fn coordinates(remote_id: &str) -> ProviderCoordinates {
        ProviderCoordinates {
            provider: "github".into(),
            host: "github.com".into(),
            owner: "octo".into(),
            repository: "repo".into(),
            pull_request: 7,
            remote_id: remote_id.into(),
        }
    }

    fn details(reviews: Vec<PullRequestReview>, complete: bool) -> PullRequestDetails {
        PullRequestDetails {
            number: 7,
            body: String::new(),
            requested_reviewers: Vec::new(),
            labels: Vec::new(),
            assignees: Vec::new(),
            merge_eligibility: MergeEligibility {
                state: "OPEN".into(),
                draft: false,
                mergeable: "MERGEABLE".into(),
                merge_state_status: "CLEAN".into(),
                review_status: String::new(),
                check_status: String::new(),
                maintainer_can_modify: true,
                can_rebase: true,
                can_update_branch: true,
                auto_merge_enabled: false,
                in_merge_queue: false,
            },
            issue_comments: Vec::new(),
            reviews,
            review_threads: Vec::new(),
            checks: Vec::new(),
            activity_complete: complete,
            checks_complete: true,
            notice: None,
        }
    }

    fn pending_snapshot(
        review_id: &str,
        comment_id: &str,
        body: &str,
        complete: bool,
    ) -> PendingReviewSnapshot {
        PendingReviewSnapshot {
            review: PullRequestReview {
                coordinates: coordinates(review_id),
                author: Some("reader".into()),
                body: String::new(),
                state: "PENDING".into(),
                submitted_at: None,
                commit_sha: Some("2222222".into()),
                edit_summary_capability: None,
                url: String::new(),
            },
            comments: vec![LinkedReviewComment {
                pull_request_review_id: review_id.into(),
                comment: ReviewComment {
                    coordinates: coordinates(comment_id),
                    author: Some("reader".into()),
                    body: body.into(),
                    created_at: "now".into(),
                    updated_at: "now".into(),
                    url: String::new(),
                    path: "src/lib.rs".into(),
                    subject: cibergit::domain::ReviewSubject::Line,
                    line: Some(1),
                    original_line: Some(1),
                    start_line: None,
                    original_start_line: None,
                    side: None,
                    diff_hunk: String::new(),
                    commit_sha: Some("2222222".into()),
                    original_commit_sha: Some("2222222".into()),
                    outdated: false,
                },
            }],
            comments_complete: complete,
            file_comment_source: None,
        }
    }

    fn review_thread(comment_id: &str, body: &str, complete: bool) -> ReviewThread {
        ReviewThread {
            coordinates: coordinates("thread-1"),
            path: "src/lib.rs".into(),
            subject: cibergit::domain::ReviewSubject::Line,
            line: Some(1),
            original_line: Some(1),
            start_line: None,
            original_start_line: None,
            side: Some("RIGHT".into()),
            start_side: None,
            resolved: false,
            outdated: false,
            comments: vec![ReviewComment {
                coordinates: coordinates(comment_id),
                author: Some("reader".into()),
                body: body.into(),
                created_at: "now".into(),
                updated_at: "now".into(),
                url: String::new(),
                path: "src/lib.rs".into(),
                subject: cibergit::domain::ReviewSubject::Line,
                line: Some(1),
                original_line: Some(1),
                start_line: None,
                original_start_line: None,
                side: Some("RIGHT".into()),
                diff_hunk: String::new(),
                commit_sha: Some("2222222".into()),
                original_commit_sha: Some("2222222".into()),
                outdated: false,
            }],
            comments_complete: complete,
        }
    }

    fn details_with_thread(thread: ReviewThread, complete: bool) -> PullRequestDetails {
        let mut details = details(Vec::new(), complete);
        details.review_threads.push(thread);
        details
    }

    fn uncertain_comment_controller(
        root: &Path,
        existing_id: Option<&str>,
    ) -> (Box<ReviewInteractionController>, String) {
        let session = session();
        let mut controller =
            match ReviewInteractionController::load(root, &repository(), 7, &session).unwrap() {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };
        controller
            .select_line(&session, LineSelection::single(DiffSide::New, 1))
            .unwrap();
        let snapshot = controller
            .stage_composer_text("frozen edit body".into())
            .unwrap();
        controller.store.save(&snapshot).unwrap();
        let draft_id = controller
            .composer
            .as_ref()
            .unwrap()
            .draft_id
            .clone()
            .unwrap();
        controller.finish_composer_save(&snapshot, &draft_id, "frozen edit body", Ok(()));
        if let Some(comment_id) = existing_id {
            let draft = controller
                .composition
                .drafts
                .iter_mut()
                .find(|draft| draft.id == draft_id)
                .unwrap();
            draft.remote = Some(RemoteDraftIds {
                review_id: Some("pending-1".into()),
                comment_id: comment_id.into(),
            });
            draft.dirty = true;
        }
        controller.store.save(&controller.composition).unwrap();
        controller.durable_composition = Some(controller.composition.clone());
        let operation_id = controller.prepare_pending(&session).unwrap();
        controller
            .composition
            .mark_in_flight(&operation_id, "attempt-1")
            .unwrap();
        controller
            .composition
            .mark_uncertain(&operation_id, "server reply was lost")
            .unwrap();
        controller.store.save(&controller.composition).unwrap();
        controller.durable_composition = Some(controller.composition.clone());
        (controller, operation_id)
    }

    fn uncertain_submission_controller(root: &Path) -> (Box<ReviewInteractionController>, String) {
        let session = session();
        let mut controller =
            match ReviewInteractionController::load(root, &repository(), 7, &session).unwrap() {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };
        controller.composition.observed_pending_review_id = Some("pending-1".into());
        controller.store.save(&controller.composition).unwrap();
        controller.durable_composition = Some(controller.composition.clone());
        let operation_id = controller
            .prepare_submission(
                ReviewEvent::Approve,
                "frozen review body".into(),
                Some("2222222"),
            )
            .unwrap();
        controller
            .composition
            .mark_in_flight(&operation_id, "attempt-submit")
            .unwrap();
        controller
            .composition
            .mark_uncertain(&operation_id, "server reply was lost")
            .unwrap();
        controller.store.save(&controller.composition).unwrap();
        controller.durable_composition = Some(controller.composition.clone());
        (controller, operation_id)
    }

    fn terminal_review() -> PullRequestReview {
        PullRequestReview {
            coordinates: coordinates("pending-1"),
            author: Some("reader".into()),
            body: "frozen review body".into(),
            state: "APPROVED".into(),
            submitted_at: Some("now".into()),
            commit_sha: Some("2222222".into()),
            edit_summary_capability: None,
            url: String::new(),
        }
    }

    #[test]
    fn draft_text_is_revision_bound_and_survives_restart() {
        let directory = tempdir().unwrap();
        let session = session();
        let mut controller =
            match ReviewInteractionController::load(directory.path(), &repository(), 7, &session)
                .unwrap()
            {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };
        controller
            .select_line(&session, LineSelection::single(DiffSide::New, 1))
            .unwrap();
        let snapshot = controller
            .stage_composer_text("A precise multiline\ncomment".into())
            .unwrap();
        controller.store.save(&snapshot).unwrap();
        let draft_id = controller
            .composer
            .as_ref()
            .unwrap()
            .draft_id
            .clone()
            .unwrap();
        controller.finish_composer_save(
            &snapshot,
            &draft_id,
            "A precise multiline\ncomment",
            Ok(()),
        );
        let restored =
            match ReviewInteractionController::load(directory.path(), &repository(), 7, &session)
                .unwrap()
            {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };
        assert_eq!(
            restored.composition.drafts[0].body,
            "A precise multiline\ncomment"
        );
        assert_eq!(
            restored.composition.drafts[0].coordinate.reviewed_revision,
            *session.revision()
        );
    }

    #[test]
    fn reselecting_an_unsaved_draft_never_claims_it_is_durable() {
        let directory = tempdir().unwrap();
        let session = session();
        let mut controller =
            match ReviewInteractionController::load(directory.path(), &repository(), 7, &session)
                .unwrap()
            {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };
        let selection = LineSelection::single(DiffSide::New, 1);
        controller.select_line(&session, selection).unwrap();
        controller
            .stage_composer_text("not on disk".into())
            .unwrap();
        controller.composer = None;
        controller.select_line(&session, selection).unwrap();
        assert!(!controller.composer.as_ref().unwrap().durable);
        assert!(
            controller
                .prepare_pending(&session)
                .unwrap_err()
                .contains("Save")
        );
        assert!(matches!(
            controller.store.load(&controller.composition.key).unwrap(),
            LoadOutcome::Missing
        ));
    }

    #[test]
    fn pending_immediate_and_submission_freeze_distinct_exact_payloads() {
        let directory = tempdir().unwrap();
        let session = session();
        let mut controller =
            match ReviewInteractionController::load(directory.path(), &repository(), 7, &session)
                .unwrap()
            {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };
        controller
            .select_line(
                &session,
                LineSelection {
                    side: DiffSide::New,
                    start_line: 1,
                    line: 2,
                },
            )
            .unwrap();
        let snapshot = controller
            .stage_composer_text("Keep this exact payload".into())
            .unwrap();
        controller.store.save(&snapshot).unwrap();
        let draft_id = controller
            .composer
            .as_ref()
            .unwrap()
            .draft_id
            .clone()
            .unwrap();
        controller.finish_composer_save(&snapshot, &draft_id, "Keep this exact payload", Ok(()));

        let pending_id = controller.prepare_pending(&session).unwrap();
        let pending = controller
            .composition
            .operations
            .iter()
            .find(|operation| operation.id == pending_id)
            .unwrap();
        let Some(cibergit::participation::ReviewOperationPayload::PendingComment(intent)) =
            &pending.payload
        else {
            panic!("pending action must freeze a pending-comment payload");
        };
        assert_eq!(intent.body, "Keep this exact payload");
        assert_eq!(intent.position.commit_sha, session.revision().head_sha);
        assert_eq!(intent.position.start_line, Some(1));
        assert_eq!(intent.position.line, 2);
        controller.composition.cancel_prepared(&pending_id).unwrap();

        let immediate_id = controller.prepare_immediate(&session).unwrap();
        assert!(matches!(
            controller
                .composition
                .operations
                .iter()
                .find(|operation| operation.id == immediate_id)
                .and_then(|operation| operation.payload.as_ref()),
            Some(cibergit::participation::ReviewOperationPayload::ImmediateComment(_))
        ));
        controller
            .composition
            .cancel_prepared(&immediate_id)
            .unwrap();

        let draft = controller
            .composition
            .drafts
            .iter_mut()
            .find(|draft| draft.id == draft_id)
            .unwrap();
        draft.dirty = false;
        draft.remote = Some(cibergit::participation::RemoteDraftIds {
            review_id: Some("pending-review-1".into()),
            comment_id: "comment-1".into(),
        });
        let submission_id = controller
            .prepare_submission(
                ReviewEvent::RequestChanges,
                "Summary stays on the reviewed head".into(),
                Some("3333333"),
            )
            .unwrap();
        let submission = controller
            .composition
            .operations
            .iter()
            .find(|operation| operation.id == submission_id)
            .and_then(|operation| operation.payload.as_ref())
            .unwrap();
        let cibergit::participation::ReviewOperationPayload::Submission(intent) = submission else {
            panic!("submit action must freeze a submission payload");
        };
        assert_eq!(intent.reviewed_commit_sha, session.revision().head_sha);
        assert!(intent.newer_head_warning.as_deref().is_some_and(|warning| {
            warning.contains("3333333") && warning.contains(&session.revision().head_sha)
        }));
    }

    #[test]
    fn initial_journal_failure_sends_zero_writes() {
        let directory = tempdir().unwrap();
        let blocked = directory.path().join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let key = review_key();
        let journal = ActionJournal {
            root: blocked.clone(),
            key: key.clone(),
            target: TargetMutationAuthority { root: blocked, key },
        };
        let calls = AtomicUsize::new(0);
        let outcome: ProviderMutationOutcome<()> = journal.dispatch(
            JournalRequest::Auxiliary(Box::new(auxiliary_request())),
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                ProviderMutationOutcome::Acknowledged(())
            },
            |_| (true, true, "done".into()),
        );
        assert!(matches!(
            outcome,
            ProviderMutationOutcome::PreflightRejected { .. }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn uncertain_attempt_survives_restart_and_is_not_replayed() {
        let directory = tempdir().unwrap();
        let journal = ActionJournal::open(directory.path(), review_key()).unwrap();
        let request = auxiliary_request();
        let calls = AtomicUsize::new(0);
        let outcome = journal.dispatch(
            JournalRequest::Auxiliary(Box::new(request.clone())),
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                ProviderMutationOutcome::<()>::Uncertain {
                    context: cibergit::domain::MutationContext {
                        operation_id: request.operation_id.clone(),
                        attempt_id: request.attempt_id.clone(),
                        action: "resolve".into(),
                        payload: serde_json::json!({"thread": "thread-1"}),
                    },
                    reason: "server success; reply lost".into(),
                }
            },
            |_| (true, true, "done".into()),
        );
        assert!(matches!(outcome, ProviderMutationOutcome::Uncertain { .. }));
        let restored = ActionJournal::open(directory.path(), review_key()).unwrap();
        let operations = restored.operations().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            operations[0].status,
            JournalStatus::Uncertain { .. }
        ));
        let second = AtomicUsize::new(0);
        let result = restored.dispatch(
            JournalRequest::Auxiliary(Box::new(ReviewAuxiliaryRequest {
                operation_id: "aux-2".into(),
                attempt_id: "attempt-2".into(),
                action: request.action,
            })),
            || {
                second.fetch_add(1, Ordering::SeqCst);
                ProviderMutationOutcome::Acknowledged(())
            },
            |_| (true, true, "done".into()),
        );
        assert!(matches!(
            result,
            ProviderMutationOutcome::PreflightRejected { .. }
        ));
        assert_eq!(second.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn acknowledged_write_with_failed_outcome_save_becomes_uncertain() {
        let directory = tempdir().unwrap();
        let journal = ActionJournal::open(directory.path(), review_key()).unwrap();
        let request = auxiliary_request();
        let path = journal.path().unwrap();
        let partition = path.parent().unwrap().to_owned();
        let displaced = partition.with_extension("displaced-for-test");
        let outcome = journal.dispatch(
            JournalRequest::Auxiliary(Box::new(request.clone())),
            || {
                fs::rename(&partition, &displaced).unwrap();
                fs::write(&partition, b"blocks journal partition").unwrap();
                ProviderMutationOutcome::Acknowledged(())
            },
            |_| (true, true, "provider acknowledged exact request".into()),
        );
        fs::remove_file(&partition).unwrap();
        fs::rename(&displaced, &partition).unwrap();
        let ProviderMutationOutcome::Uncertain { context, reason } = outcome else {
            panic!("an acknowledged write without a durable outcome must be uncertain");
        };
        assert_eq!(context.operation_id, request.operation_id);
        assert_eq!(context.attempt_id, request.attempt_id);
        assert!(reason.contains("acknowledged the write"));
        let operations = journal.operations().unwrap();
        assert!(matches!(operations[0].status, JournalStatus::InFlight));
    }

    #[test]
    fn concurrent_identical_journal_attempt_dispatches_exactly_once() {
        let directory = tempdir().unwrap();
        let journal = ActionJournal::open(directory.path(), review_key()).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let journal = journal.clone();
            let request = auxiliary_request();
            let barrier = barrier.clone();
            let calls = calls.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                journal.dispatch(
                    JournalRequest::Auxiliary(Box::new(request)),
                    || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(30));
                        ProviderMutationOutcome::Acknowledged(())
                    },
                    |_| (true, true, "exact request acknowledged".into()),
                )
            }));
        }
        barrier.wait();
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, ProviderMutationOutcome::Acknowledged(())))
                .count(),
            1
        );
        assert_eq!(journal.operations().unwrap().len(), 1);
    }

    #[test]
    fn explicit_target_guard_unlocks_while_duplicate_descriptor_remains_open() {
        let directory = tempdir().unwrap();
        let authority =
            TargetMutationAuthority::new(directory.path().to_owned(), review_key()).unwrap();
        let guard = authority.acquire().unwrap();
        let duplicate = guard.file.as_ref().unwrap().try_clone().unwrap();
        drop(guard);
        drop(authority.acquire().expect(
            "Drop must explicitly unlock even while a duplicate open-file description remains",
        ));
        drop(duplicate);
    }

    #[test]
    fn unresolved_review_blocks_lifecycle_admission_before_dispatch() {
        let directory = tempdir().unwrap();
        let (_controller, _) = uncertain_comment_controller(directory.path(), None);
        let request = lifecycle_request();
        let journal = ActionJournal::open(directory.path(), review_key()).unwrap();
        let mut owned = journal.clone();
        let mut admission = owned.admission(JournalRequest::Lifecycle(Box::new(request.clone())));
        let error = admission
            .admit(&JournalRequest::Lifecycle(Box::new(request)).mutation_context())
            .err()
            .expect("unresolved review must refuse lifecycle admission");
        assert!(error.to_string().contains("review mutation"));
        assert!(journal.operations().unwrap().is_empty());
    }

    #[test]
    fn unresolved_lifecycle_blocks_review_and_merge_families() {
        let directory = tempdir().unwrap();
        let request = lifecycle_request();
        let mut journal = ActionJournal::open(directory.path(), review_key()).unwrap();
        let frozen = JournalRequest::Lifecycle(Box::new(request.clone()));
        {
            let mut admission = journal.admission(frozen.clone());
            let mut held = admission.admit(&frozen.mutation_context()).unwrap();
            held.record_terminal(&MutationTerminalRecord::Uncertain {
                reason: "provider reply lost".into(),
            })
            .unwrap();
        }

        let controller =
            match ReviewInteractionController::load(directory.path(), &repository(), 7, &session())
                .unwrap()
            {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };
        let review_error = controller
            .authority
            .execute_if_current(
                &controller.store,
                controller.durable_composition.as_ref(),
                || panic!("unresolved lifecycle must reject before review dispatch"),
            )
            .unwrap_err();
        assert!(review_error.contains("unresolved durable outcome"));

        let calls = AtomicUsize::new(0);
        let merge_family = journal.dispatch(
            JournalRequest::Auxiliary(Box::new(auxiliary_request())),
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                ProviderMutationOutcome::Acknowledged(())
            },
            |_| (true, true, "unexpected".into()),
        );
        assert!(matches!(
            merge_family,
            ProviderMutationOutcome::PreflightRejected { .. }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn duplicate_lifecycle_window_attempt_is_admitted_exactly_once() {
        let directory = tempdir().unwrap();
        let root = directory.path().to_owned();
        let barrier = Arc::new(Barrier::new(3));
        let admitted = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let root = root.clone();
            let barrier = barrier.clone();
            let admitted = admitted.clone();
            workers.push(thread::spawn(move || {
                let request = lifecycle_request();
                let frozen = JournalRequest::Lifecycle(Box::new(request));
                let mut journal = ActionJournal::open(&root, review_key()).unwrap();
                let mut admission = journal.admission(frozen.clone());
                barrier.wait();
                match admission.admit(&frozen.mutation_context()) {
                    Ok(mut held) => {
                        admitted.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(30));
                        held.record_terminal(&MutationTerminalRecord::Acknowledged {
                            acknowledgement: serde_json::json!({"exact": true}),
                        })
                        .unwrap();
                        true
                    }
                    Err(_) => false,
                }
            }));
        }
        barrier.wait();
        let accepted = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|accepted| *accepted)
            .count();
        assert_eq!(accepted, 1);
        assert_eq!(admitted.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn duplicate_lifecycle_process_is_refused_after_first_terminal_tombstone() {
        let directory = tempdir().unwrap();
        let ready = directory.path().join("helper-ready");
        let request = lifecycle_request();
        let frozen = JournalRequest::Lifecycle(Box::new(request));
        let mut journal = ActionJournal::open(directory.path(), review_key()).unwrap();
        let mut admission = journal.admission(frozen.clone());
        let mut held = admission.admit(&frozen.mutation_context()).unwrap();
        let mut helper = Command::new(std::env::current_exe().unwrap())
            .arg("app::review_interactions::tests::lifecycle_admission_helper_process")
            .arg("--ignored")
            .arg("--exact")
            .env("CIBERGIT_TEST_LIFECYCLE_ROOT", directory.path())
            .env("CIBERGIT_TEST_LIFECYCLE_READY", &ready)
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !ready.exists() && started.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "helper reached cross-process admission");
        held.record_terminal(&MutationTerminalRecord::Acknowledged {
            acknowledgement: serde_json::json!({"first_process": true}),
        })
        .unwrap();
        drop(held);
        assert!(helper.wait().unwrap().success());
        assert_eq!(journal.operations().unwrap().len(), 1);
    }

    #[test]
    #[ignore = "subprocess helper for exact cross-process lifecycle admission"]
    fn lifecycle_admission_helper_process() {
        let Some(root) = std::env::var_os("CIBERGIT_TEST_LIFECYCLE_ROOT") else {
            return;
        };
        let ready = std::env::var_os("CIBERGIT_TEST_LIFECYCLE_READY").unwrap();
        let request = lifecycle_request();
        let frozen = JournalRequest::Lifecycle(Box::new(request));
        let mut journal = ActionJournal::open(Path::new(&root), review_key()).unwrap();
        fs::write(ready, b"ready").unwrap();
        let mut admission = journal.admission(frozen.clone());
        let error = admission
            .admit(&frozen.mutation_context())
            .err()
            .expect("first process terminal tombstone must refuse duplicate");
        assert!(error.to_string().contains("already journaled"));
    }

    #[test]
    fn lifecycle_initial_save_failure_has_no_admitted_dispatch() {
        let directory = tempdir().unwrap();
        let mut journal = ActionJournal::open(directory.path(), review_key()).unwrap();
        fs::write(
            directory.path().join("action-journal"),
            b"blocks journal tree",
        )
        .unwrap();
        let request = lifecycle_request();
        let frozen = JournalRequest::Lifecycle(Box::new(request));
        let calls = AtomicUsize::new(0);
        let mut admission = journal.admission(frozen.clone());
        if admission.admit(&frozen.mutation_context()).is_ok() {
            calls.fetch_add(1, Ordering::SeqCst);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn lifecycle_terminal_save_failure_retains_inflight_and_refuses_restart_replay() {
        let directory = tempdir().unwrap();
        let request = lifecycle_request();
        let frozen = JournalRequest::Lifecycle(Box::new(request));
        let mut journal = ActionJournal::open(directory.path(), review_key()).unwrap();
        let path = journal.path().unwrap();
        {
            let mut admission = journal.admission(frozen.clone());
            let mut held = admission.admit(&frozen.mutation_context()).unwrap();
            let partition = path.parent().unwrap().to_owned();
            let displaced = partition.with_extension("terminal-save-backup");
            fs::rename(&partition, &displaced).unwrap();
            fs::write(&partition, b"blocks terminal record").unwrap();
            assert!(
                held.record_terminal(&MutationTerminalRecord::Acknowledged {
                    acknowledgement: serde_json::json!({"provider": "accepted"}),
                })
                .is_err()
            );
            fs::remove_file(&partition).unwrap();
            fs::rename(&displaced, &partition).unwrap();
        }
        assert!(matches!(
            journal.operations().unwrap()[0].status,
            JournalStatus::InFlight
        ));
        let mut restarted = ActionJournal::open(directory.path(), review_key()).unwrap();
        let mut admission = restarted.admission(frozen.clone());
        assert!(admission.admit(&frozen.mutation_context()).is_err());
    }

    #[test]
    fn corrupt_and_future_action_journals_are_preserved() {
        for bytes in [b"{".as_slice(), br#"{"version":99}"#.as_slice()] {
            let directory = tempdir().unwrap();
            let journal = ActionJournal::open(directory.path(), review_key()).unwrap();
            let path = journal.path().unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
            let error = journal.operations().unwrap_err();
            assert!(error.contains("preserved"));
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn concurrent_terminal_reconciliation_cannot_overwrite_first_outcome() {
        let directory = tempdir().unwrap();
        let journal = ActionJournal::open(directory.path(), review_key()).unwrap();
        let request = auxiliary_request();
        let _ = journal.dispatch(
            JournalRequest::Auxiliary(Box::new(request.clone())),
            || ProviderMutationOutcome::<()>::Uncertain {
                context: JournalRequest::Auxiliary(Box::new(request.clone())).mutation_context(),
                reason: "lost reply".into(),
            },
            |_| (true, true, "unused".into()),
        );
        let barrier = Arc::new(Barrier::new(3));
        let acknowledged = {
            let journal = journal.clone();
            let barrier = barrier.clone();
            let operation_id = request.operation_id.clone();
            let attempt_id = request.attempt_id.clone();
            thread::spawn(move || {
                barrier.wait();
                journal.mark_acknowledged(
                    &operation_id,
                    &attempt_id,
                    true,
                    "authoritative success".into(),
                )
            })
        };
        let not_applied = {
            let journal = journal.clone();
            let barrier = barrier.clone();
            let operation_id = request.operation_id.clone();
            let attempt_id = request.attempt_id.clone();
            thread::spawn(move || {
                barrier.wait();
                journal.mark_not_applied(&operation_id, &attempt_id, "authoritative absence".into())
            })
        };
        barrier.wait();
        let results = [acknowledged.join().unwrap(), not_applied.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let operations = journal.operations().unwrap();
        assert!(matches!(
            operations[0].status,
            JournalStatus::Acknowledged { .. } | JournalStatus::NotApplied { .. }
        ));
    }

    #[test]
    fn cross_controller_cas_preserves_newer_text_and_retired_ids() {
        let directory = tempdir().unwrap();
        let session = session();
        let mut first =
            match ReviewInteractionController::load(directory.path(), &repository(), 7, &session)
                .unwrap()
            {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };
        let mut stale = first.clone();
        first
            .select_line(&session, LineSelection::single(DiffSide::New, 1))
            .unwrap();
        let mut first_snapshot = first.stage_composer_text("newer text".into()).unwrap();
        first_snapshot
            .retired_review_ids
            .insert("retired-review".into());
        first
            .authority
            .save_if_current(
                &first.store,
                first.durable_composition.as_ref(),
                &first_snapshot,
            )
            .unwrap();

        stale
            .select_line(&session, LineSelection::single(DiffSide::New, 1))
            .unwrap();
        let stale_snapshot = stale.stage_composer_text("stale overwrite".into()).unwrap();
        let error = stale
            .authority
            .save_if_current(
                &stale.store,
                stale.durable_composition.as_ref(),
                &stale_snapshot,
            )
            .unwrap_err();
        assert!(error.contains("stale clone was not saved"));
        let durable = load_composition(&first.store, &first.composition.key)
            .unwrap()
            .unwrap();
        assert_eq!(durable.drafts[0].body, "newer text");
        assert!(durable.retired_review_ids.contains("retired-review"));
    }

    #[test]
    fn production_shaped_exact_existing_comment_edit_reconciles_durably() {
        let directory = tempdir().unwrap();
        let (mut controller, operation_id) =
            uncertain_comment_controller(directory.path(), Some("comment-1"));
        let operation = controller
            .composition
            .operations
            .iter_mut()
            .find(|operation| operation.id == operation_id)
            .unwrap();
        operation.status = ReviewOperationStatus::InFlight {
            attempt_id: "attempt-1".into(),
        };
        controller
            .composition
            .edit_draft("draft-1", "newer unsent local text")
            .unwrap();
        controller
            .composition
            .retired_review_ids
            .insert("older-retired-review".into());
        controller.store.save(&controller.composition).unwrap();
        controller.durable_composition = Some(controller.composition.clone());
        let reads = AtomicUsize::new(0);
        let provider_mutations = AtomicUsize::new(0);
        let pending = pending_snapshot("pending-1", "comment-1", "frozen edit body", true);
        assert_eq!(
            pending.comments[0].comment.side, None,
            "GithubProvider pending-review comments do not carry diff-side metadata"
        );
        let report = controller
            .authority
            .reconcile_if_current(
                &controller.store,
                controller.durable_composition.as_ref(),
                &repository(),
                7,
                || {
                    reads.fetch_add(1, Ordering::SeqCst);
                    Ok((
                        details_with_thread(
                            review_thread("comment-1", "frozen edit body", true),
                            true,
                        ),
                        Some(pending),
                    ))
                },
            )
            .unwrap();
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert_eq!(provider_mutations.load(Ordering::SeqCst), 0);
        assert_eq!(report.resolved(), 1);
        assert_eq!(report.unresolved(), 0);
        assert!(
            report
                .composition
                .retired_review_ids
                .contains("older-retired-review")
        );
        let draft = report.composition.draft("draft-1").unwrap();
        assert_eq!(draft.body, "newer unsent local text");
        assert_eq!(
            draft.observed_remote_body.as_deref(),
            Some("frozen edit body")
        );
        assert!(draft.dirty);
        assert_eq!(
            draft
                .remote
                .as_ref()
                .map(|remote| remote.comment_id.as_str()),
            Some("comment-1")
        );

        let mut restored =
            match ReviewInteractionController::load(directory.path(), &repository(), 7, &session())
                .unwrap()
            {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };
        assert_eq!(restored.unresolved_operations(), 0);
        restored.reopen_draft("draft-1").unwrap();
        let prepared = restored.prepare_pending(&session()).unwrap();
        assert!(
            restored
                .composition
                .operations
                .iter()
                .any(|operation| operation.id == prepared
                    && operation.status == ReviewOperationStatus::Prepared)
        );
        assert_eq!(provider_mutations.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn known_comment_thread_join_rejects_missing_duplicate_partial_and_wrong_range() {
        let directory = tempdir().unwrap();
        let (controller, _) = uncertain_comment_controller(directory.path(), Some("comment-1"));
        let expected = controller.durable_composition.as_ref();
        let pending = pending_snapshot("pending-1", "comment-1", "frozen edit body", true);
        let mut cases = Vec::new();

        cases.push((details(Vec::new(), true), "did not contain a review thread"));

        let mut wrong_end_side = review_thread("comment-1", "frozen edit body", true);
        wrong_end_side.side = Some("LEFT".into());
        cases.push((details_with_thread(wrong_end_side, true), "end side/line"));

        let mut wrong_end_line = review_thread("comment-1", "frozen edit body", true);
        wrong_end_line.line = Some(2);
        cases.push((details_with_thread(wrong_end_line, true), "end side/line"));

        let mut wrong_start_side = review_thread("comment-1", "frozen edit body", true);
        wrong_start_side.start_side = Some("RIGHT".into());
        cases.push((
            details_with_thread(wrong_start_side, true),
            "start side/line",
        ));

        cases.push((
            details_with_thread(review_thread("comment-1", "frozen edit body", false), true),
            "incomplete comments",
        ));

        let mut duplicate =
            details_with_thread(review_thread("comment-1", "frozen edit body", true), true);
        let mut second = review_thread("comment-1", "frozen edit body", true);
        second.coordinates.remote_id = "thread-2".into();
        duplicate.review_threads.push(second);
        cases.push((duplicate, "multiple review threads"));

        let mut missing_thread_side = review_thread("comment-1", "frozen edit body", true);
        missing_thread_side.comments[0].side = None;
        cases.push((
            details_with_thread(missing_thread_side, true),
            "thread comment disagrees",
        ));

        for (details, expected_reason) in cases {
            let report = controller
                .authority
                .reconcile_if_current(&controller.store, expected, &repository(), 7, || {
                    Ok((details, Some(pending.clone())))
                })
                .unwrap();
            assert_eq!(report.resolved(), 0);
            let ReviewReconciliationOutcome::Unresolved(reason) = &report.items[0].outcome else {
                panic!("invalid thread evidence must remain unresolved")
            };
            assert!(
                reason.contains(expected_reason),
                "expected {expected_reason:?} in {reason:?}"
            );
        }
    }

    #[test]
    fn exact_pending_review_submission_reconciles_terminal_event_head_body_and_ids() {
        let directory = tempdir().unwrap();
        let (controller, operation_id) = uncertain_submission_controller(directory.path());
        let report = controller
            .authority
            .reconcile_if_current(
                &controller.store,
                controller.durable_composition.as_ref(),
                &repository(),
                7,
                || Ok((details(vec![terminal_review()], true), None)),
            )
            .unwrap();
        assert_eq!(report.resolved(), 1);
        assert!(report.composition.retired_review_ids.contains("pending-1"));
        assert!(matches!(
            report
                .composition
                .operations
                .iter()
                .find(|operation| operation.id == operation_id)
                .map(|operation| &operation.status),
            Some(ReviewOperationStatus::Acknowledged {
                remote_review_id: Some(review_id),
                remote_comment_id: None,
            }) if review_id == "pending-1"
        ));
        let restored =
            match ReviewInteractionController::load(directory.path(), &repository(), 7, &session())
                .unwrap()
            {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };
        assert_eq!(restored.unresolved_operations(), 0);
        assert!(
            restored
                .composition
                .retired_review_ids
                .contains("pending-1")
        );
    }

    #[test]
    fn github_identity_names_accept_canonical_case_without_weakening_remote_ids() {
        let directory = tempdir().unwrap();
        let (controller, operation_id) =
            uncertain_comment_controller(directory.path(), Some("comment-1"));
        let mut pending = pending_snapshot("pending-1", "comment-1", "frozen edit body", true);
        pending.review.coordinates.owner = "OCTO".into();
        pending.review.coordinates.repository = "Repo".into();
        pending.review.author = Some("READER".into());
        pending.comments[0].comment.coordinates.owner = "Octo".into();
        pending.comments[0].comment.coordinates.repository = "REPO".into();
        pending.comments[0].comment.author = Some("Reader".into());
        let mut thread = review_thread("comment-1", "frozen edit body", true);
        thread.coordinates.owner = "Octo".into();
        thread.coordinates.repository = "REPO".into();
        thread.comments[0].coordinates.owner = "OCTO".into();
        thread.comments[0].coordinates.repository = "Repo".into();
        thread.comments[0].author = Some("READER".into());
        let report = controller
            .authority
            .reconcile_if_current(
                &controller.store,
                controller.durable_composition.as_ref(),
                &repository(),
                7,
                || Ok((details_with_thread(thread, true), Some(pending))),
            )
            .unwrap();
        assert_eq!(report.resolved(), 1);
        assert_eq!(report.items[0].operation_id, operation_id);
        assert_eq!(
            report
                .composition
                .draft("draft-1")
                .and_then(|draft| draft.remote.as_ref())
                .map(|remote| remote.comment_id.as_str()),
            Some("comment-1"),
            "remote object IDs remain exact"
        );
    }

    #[test]
    fn comment_reconciliation_rejects_heuristics_mismatches_partial_reads_and_other_account() {
        let directory = tempdir().unwrap();
        let (controller, _) = uncertain_comment_controller(directory.path(), Some("comment-1"));
        let expected = controller.durable_composition.as_ref();
        let mut cases = Vec::new();
        let valid_thread = review_thread("comment-1", "frozen edit body", true);

        let mut wrong_id =
            pending_snapshot("pending-1", "unrelated-comment", "frozen edit body", true);
        cases.push((
            repository(),
            wrong_id.clone(),
            valid_thread.clone(),
            "exact known comment ID",
        ));
        wrong_id.comments[0].comment.coordinates.remote_id = "comment-1".into();

        let mut wrong_author = wrong_id.clone();
        wrong_author.comments[0].comment.author = Some("other-user".into());
        cases.push((repository(), wrong_author, valid_thread.clone(), "author"));

        let mut wrong_repo = wrong_id.clone();
        wrong_repo.comments[0].comment.coordinates.repository = "other-repo".into();
        cases.push((
            repository(),
            wrong_repo,
            valid_thread.clone(),
            "coordinates",
        ));

        let mut wrong_head = wrong_id.clone();
        wrong_head.comments[0].comment.commit_sha = Some("different-head".into());
        cases.push((repository(), wrong_head, valid_thread.clone(), "differs"));

        let mut wrong_body = wrong_id.clone();
        wrong_body.comments[0].comment.body = "different body".into();
        cases.push((repository(), wrong_body, valid_thread.clone(), "differs"));

        let mut incomplete = wrong_id.clone();
        incomplete.comments_complete = false;
        cases.push((repository(), incomplete, valid_thread.clone(), "incomplete"));

        let mut other_account = repository();
        other_account.account.login = "other-user".into();
        cases.push((other_account, wrong_id, valid_thread, "selected account"));

        for (repository, pending, thread, expected_reason) in cases {
            let report = controller
                .authority
                .reconcile_if_current(&controller.store, expected, &repository, 7, || {
                    Ok((details_with_thread(thread, true), Some(pending)))
                })
                .unwrap();
            assert_eq!(report.resolved(), 0);
            assert_eq!(report.unresolved(), 1);
            let ReviewReconciliationOutcome::Unresolved(reason) = &report.items[0].outcome else {
                panic!("mismatch must remain unresolved")
            };
            assert!(
                reason.contains(expected_reason),
                "expected {expected_reason:?} in {reason:?}"
            );
        }

        let truncated = controller
            .authority
            .reconcile_if_current(&controller.store, expected, &repository(), 7, || {
                Ok((
                    details_with_thread(
                        review_thread("comment-1", "frozen edit body", true),
                        false,
                    ),
                    Some(pending_snapshot(
                        "pending-1",
                        "comment-1",
                        "frozen edit body",
                        true,
                    )),
                ))
            })
            .unwrap();
        let ReviewReconciliationOutcome::Unresolved(reason) = &truncated.items[0].outcome else {
            panic!("truncated activity must remain unresolved")
        };
        assert!(reason.contains("incomplete or truncated"));

        let ambiguous_directory = tempdir().unwrap();
        let (ambiguous, _) = uncertain_comment_controller(ambiguous_directory.path(), None);
        let report = ambiguous
            .authority
            .reconcile_if_current(
                &ambiguous.store,
                ambiguous.durable_composition.as_ref(),
                &repository(),
                7,
                || {
                    Ok((
                        details(Vec::new(), true),
                        Some(pending_snapshot(
                            "pending-1",
                            "unrelated-identical",
                            "frozen edit body",
                            true,
                        )),
                    ))
                },
            )
            .unwrap();
        let ReviewReconciliationOutcome::Unresolved(reason) = &report.items[0].outcome else {
            panic!("new comment without a remote ID must remain unresolved")
        };
        assert!(reason.contains("even identical body and position are ambiguous"));
    }

    #[test]
    fn submission_reconciliation_rejects_id_author_repo_event_head_body_and_truncation() {
        let directory = tempdir().unwrap();
        let (controller, _) = uncertain_submission_controller(directory.path());
        let expected = controller.durable_composition.as_ref();
        let mut cases = Vec::new();

        let mut wrong_id = terminal_review();
        wrong_id.coordinates.remote_id = "different-review".into();
        cases.push((details(vec![wrong_id], true), "exact known review ID"));

        let mut wrong_author = terminal_review();
        wrong_author.author = Some("other-user".into());
        cases.push((details(vec![wrong_author], true), "author"));

        let mut wrong_repo = terminal_review();
        wrong_repo.coordinates.repository = "other-repo".into();
        cases.push((details(vec![wrong_repo], true), "coordinates"));

        let mut wrong_event = terminal_review();
        wrong_event.state = "COMMENTED".into();
        cases.push((details(vec![wrong_event], true), "differs"));

        let mut wrong_head = terminal_review();
        wrong_head.commit_sha = Some("different-head".into());
        cases.push((details(vec![wrong_head], true), "differs"));

        let mut wrong_body = terminal_review();
        wrong_body.body = "different body".into();
        cases.push((details(vec![wrong_body], true), "differs"));

        cases.push((details(vec![terminal_review()], false), "incomplete"));

        for (details, expected_reason) in cases {
            let report = controller
                .authority
                .reconcile_if_current(&controller.store, expected, &repository(), 7, || {
                    Ok((details, None))
                })
                .unwrap();
            assert_eq!(report.resolved(), 0);
            let ReviewReconciliationOutcome::Unresolved(reason) = &report.items[0].outcome else {
                panic!("mismatch must remain unresolved")
            };
            assert!(reason.contains(expected_reason), "{reason}");
        }
    }

    #[test]
    fn stale_controller_and_save_failure_leave_uncertain_state_frozen() {
        let directory = tempdir().unwrap();
        let (controller, _) = uncertain_comment_controller(directory.path(), Some("comment-1"));
        let stale = controller.durable_composition.clone();
        let mut newer = stale.clone().unwrap();
        newer.edit_draft("draft-1", "newer durable text").unwrap();
        let operation = newer
            .operations
            .iter_mut()
            .find(|operation| operation.status.requires_reconciliation())
            .unwrap();
        operation.status = ReviewOperationStatus::Uncertain {
            attempt_id: "newer-attempt".into(),
            reason: "newer controller owns this attempt".into(),
        };
        controller.store.save(&newer).unwrap();
        let stale_error = controller
            .authority
            .reconcile_if_current(&controller.store, stale.as_ref(), &repository(), 7, || {
                panic!("stale CAS must reject before provider reads")
            })
            .unwrap_err();
        assert!(stale_error.contains("stale read was rejected"));

        let path = controller.store.record_path(&newer.key).unwrap();
        let parent = path.parent().unwrap().to_owned();
        let displaced = parent.with_extension("save-failure-backup");
        let save_error = controller
            .authority
            .reconcile_if_current(&controller.store, Some(&newer), &repository(), 7, || {
                fs::rename(&parent, &displaced).unwrap();
                fs::write(&parent, b"block recovery partition").unwrap();
                Ok((
                    details_with_thread(review_thread("comment-1", "frozen edit body", true), true),
                    Some(pending_snapshot(
                        "pending-1",
                        "comment-1",
                        "frozen edit body",
                        true,
                    )),
                ))
            })
            .unwrap_err();
        fs::remove_file(&parent).unwrap();
        fs::rename(&displaced, &parent).unwrap();
        assert!(save_error.contains("operation remains frozen"));
        let durable = load_composition(&controller.store, &newer.key)
            .unwrap()
            .unwrap();
        assert_eq!(durable.draft("draft-1").unwrap().body, "newer durable text");
        assert_eq!(durable.operations_requiring_reconciliation().count(), 1);
    }

    #[test]
    fn draft_store_reconciliation_never_resolves_the_auxiliary_journal() {
        let directory = tempdir().unwrap();
        let journal = ActionJournal::open(&directory.path().join("journal"), review_key()).unwrap();
        let request = auxiliary_request();
        let _ = journal.dispatch(
            JournalRequest::Auxiliary(Box::new(request.clone())),
            || ProviderMutationOutcome::<()>::Uncertain {
                context: JournalRequest::Auxiliary(Box::new(request.clone())).mutation_context(),
                reason: "lost reply".into(),
            },
            |_| (true, true, "unused".into()),
        );
        let (controller, _) = uncertain_comment_controller(directory.path(), Some("comment-1"));
        let report = controller
            .authority
            .reconcile_if_current(
                &controller.store,
                controller.durable_composition.as_ref(),
                &repository(),
                7,
                || {
                    Ok((
                        details_with_thread(
                            review_thread("comment-1", "frozen edit body", true),
                            true,
                        ),
                        Some(pending_snapshot(
                            "pending-1",
                            "comment-1",
                            "frozen edit body",
                            true,
                        )),
                    ))
                },
            )
            .unwrap();
        assert_eq!(report.resolved(), 1);
        assert!(matches!(
            journal.operations().unwrap()[0].status,
            JournalStatus::Uncertain { .. }
        ));
    }

    #[test]
    fn exact_current_thread_places_and_outdated_old_side_does_not() {
        let session = session();
        let comment = ReviewComment {
            coordinates: ProviderCoordinates {
                provider: "github".into(),
                host: "github.com".into(),
                owner: "octo".into(),
                repository: "repo".into(),
                pull_request: 7,
                remote_id: "comment-1".into(),
            },
            author: Some("reviewer".into()),
            body: "Please explain this.".into(),
            created_at: "now".into(),
            updated_at: "now".into(),
            url: String::new(),
            path: "src/lib.rs".into(),
            subject: cibergit::domain::ReviewSubject::Line,
            line: Some(1),
            original_line: Some(1),
            start_line: None,
            original_start_line: None,
            side: Some("RIGHT".into()),
            diff_hunk: String::new(),
            commit_sha: Some("2222222".into()),
            original_commit_sha: Some("2222222".into()),
            outdated: false,
        };
        let current = ReviewThread {
            coordinates: ProviderCoordinates {
                remote_id: "thread-1".into(),
                ..comment.coordinates.clone()
            },
            path: "src/lib.rs".into(),
            subject: cibergit::domain::ReviewSubject::Line,
            line: Some(1),
            original_line: Some(1),
            start_line: None,
            original_start_line: None,
            side: Some("RIGHT".into()),
            start_side: None,
            resolved: false,
            outdated: false,
            comments: vec![comment.clone()],
            comments_complete: true,
        };
        assert_eq!(place_thread(&session, &current).unwrap().line, 1);
        let file_level = ReviewThread {
            subject: cibergit::domain::ReviewSubject::File,
            comments: vec![ReviewComment {
                subject: cibergit::domain::ReviewSubject::File,
                line: None,
                ..comment.clone()
            }],
            line: None,
            side: None,
            ..current.clone()
        };
        assert!(
            place_thread(&session, &file_level)
                .unwrap_err()
                .contains("whole file")
        );
        let unknown = ReviewThread {
            subject: cibergit::domain::ReviewSubject::Unknown,
            ..current.clone()
        };
        assert!(
            place_thread(&session, &unknown)
                .unwrap_err()
                .contains("read-only")
        );
        let range = ReviewThread {
            line: Some(2),
            start_line: Some(1),
            start_side: Some("RIGHT".into()),
            ..current.clone()
        };
        let range_anchor = place_thread(&session, &range).unwrap();
        assert_eq!((range_anchor.start_line, range_anchor.line), (1, 2));
        let cross_side = ReviewThread {
            start_side: Some("LEFT".into()),
            ..range
        };
        assert!(
            place_thread(&session, &cross_side)
                .unwrap_err()
                .contains("cross-side")
        );
        let old = ReviewThread {
            side: Some("LEFT".into()),
            outdated: true,
            comments: vec![ReviewComment {
                outdated: true,
                ..comment
            }],
            ..current
        };
        assert!(
            place_thread(&session, &old)
                .unwrap_err()
                .contains("OLD-side")
        );
    }

    #[test]
    fn remote_pending_reconciliation_does_not_advance_session() {
        let directory = tempdir().unwrap();
        let session = session();
        let mut controller =
            match ReviewInteractionController::load(directory.path(), &repository(), 7, &session)
                .unwrap()
            {
                ControllerLoad::Ready(controller) => controller,
                ControllerLoad::RecoveryRequired(reason) => panic!("{reason}"),
            };
        let displayed = session.revision().clone();
        let details = PullRequestDetails {
            number: 7,
            body: String::new(),
            requested_reviewers: Vec::new(),
            labels: Vec::new(),
            assignees: Vec::new(),
            merge_eligibility: MergeEligibility {
                state: "OPEN".into(),
                draft: false,
                mergeable: "MERGEABLE".into(),
                merge_state_status: "CLEAN".into(),
                review_status: String::new(),
                check_status: String::new(),
                maintainer_can_modify: true,
                can_rebase: true,
                can_update_branch: true,
                auto_merge_enabled: false,
                in_merge_queue: false,
            },
            issue_comments: Vec::new(),
            reviews: vec![PullRequestReview {
                coordinates: ProviderCoordinates {
                    provider: "github".into(),
                    host: "github.com".into(),
                    owner: "octo".into(),
                    repository: "repo".into(),
                    pull_request: 7,
                    remote_id: "pending-1".into(),
                },
                author: Some("reader".into()),
                body: String::new(),
                state: "PENDING".into(),
                submitted_at: None,
                commit_sha: Some("2222222".into()),
                edit_summary_capability: None,
                url: String::new(),
            }],
            review_threads: Vec::new(),
            checks: Vec::new(),
            activity_complete: true,
            checks_complete: true,
            notice: None,
        };
        controller.reconcile_details(&details).unwrap();
        assert_eq!(controller.composition.reviewed_revision, displayed);
        assert_eq!(
            controller.composition.observed_pending_review_id.as_deref(),
            Some("pending-1")
        );
        assert_eq!(controller.unresolved_operations(), 0);
        assert!(
            controller
                .composition
                .operations
                .iter()
                .all(|op| !matches!(op.status, ReviewOperationStatus::InFlight { .. }))
        );
    }
}
