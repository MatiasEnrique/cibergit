use cibergit::{
    domain::{
        MergeAcknowledgement, MergeExecutionRequest, MergeMethod, MergePreparation,
        MutationContext, PendingReviewSnapshot, ProviderMutationOutcome, PullRequestDetails,
        Repository, ReviewAuxiliaryAcknowledgement, ReviewAuxiliaryRequest, ReviewThread,
    },
    participation::{
        CanonicalPublishedPatch, DraftCoordinate, DraftStore, LineSelection, LoadOutcome,
        ReviewComposition, ReviewEvent, ReviewKey,
    },
    review::{ReviewSession, file_key},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const JOURNAL_VERSION: u64 = 1;
const MAX_JOURNAL_BYTES: usize = 512 * 1024;
const MAX_JOURNAL_OPERATIONS: usize = 96;
const MAX_JOURNAL_TEXT_BYTES: usize = 64 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static ATTEMPT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct ComposerState {
    pub coordinate: DraftCoordinate,
    pub draft_id: Option<String>,
    pub body: String,
    pub durable: bool,
    pub notice: Option<String>,
}

#[derive(Clone, Debug)]
pub enum ControllerLoad {
    Ready(ReviewInteractionController),
    RecoveryRequired(String),
}

#[derive(Clone, Debug)]
pub struct ReviewInteractionController {
    pub composition: ReviewComposition,
    pub store: DraftStore,
    pub authority: ReviewStateAuthority,
    pub durable_composition: Option<ReviewComposition>,
    pub composer: Option<ComposerState>,
    pub pending_review: Option<PendingReviewSnapshot>,
    pub pending_complete: bool,
    pub notice: Option<String>,
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
        let authority = ReviewStateAuthority::open(root.join("state-authority"), key)?;
        Ok(ControllerLoad::Ready(Self {
            composition,
            store,
            authority,
            durable_composition,
            composer: None,
            pending_review: None,
            pending_complete: true,
            notice: None,
        }))
    }

    pub fn select_line(
        &mut self,
        session: &ReviewSession,
        selection: LineSelection,
    ) -> Result<(), String> {
        let selected = session
            .selected_file()
            .ok_or_else(|| "Select a text file before starting a discussion.".to_owned())?;
        let coordinate =
            cibergit::participation::validate_coordinate(session, &file_key(selected), selection)
                .map_err(|error| error.to_string())?;
        let existing = self.composition.drafts.iter().find(|draft| {
            draft.coordinate == coordinate
                && draft.disposition == cibergit::participation::DraftDisposition::Pending
        });
        self.composer = Some(match existing {
            Some(draft) => ComposerState {
                coordinate,
                draft_id: Some(draft.id.clone()),
                body: draft.body.clone(),
                durable: true,
                notice: None,
            },
            None => ComposerState {
                coordinate,
                draft_id: None,
                body: String::new(),
                durable: false,
                notice: Some(
                    "Bound to the displayed revision. Text becomes durable after its first local save."
                        .into(),
                ),
            },
        });
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
        let composer = ComposerState {
            coordinate: draft.coordinate.clone(),
            draft_id: Some(draft.id.clone()),
            body: draft.body.clone(),
            durable: true,
            notice: Some(if draft.remote.is_some() {
                "Editing the linked pending comment locally; Add to pending review is an explicit update."
                    .into()
            } else {
                "Reopened local pending text.".into()
            }),
        };
        self.composer = Some(composer.clone());
        Ok(composer)
    }

    /// Apply the text in memory. Callers persist the returned composition away
    /// from the UI thread and then report the save result through
    /// `finish_composer_save`.
    pub fn stage_composer_text(&mut self, body: String) -> Result<ReviewComposition, String> {
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
        Ok(self.composition.clone())
    }

    pub fn finish_composer_save(
        &mut self,
        saved: &ReviewComposition,
        draft_id: &str,
        saved_body: &str,
        result: Result<(), String>,
    ) {
        let Some(composer) = self.composer.as_mut() else {
            return;
        };
        if composer.draft_id.as_deref() != Some(draft_id) || composer.body != saved_body {
            return;
        }
        match result {
            Ok(()) => {
                self.durable_composition = Some(saved.clone());
                composer.durable = true;
                composer.notice = Some("Saved locally for restart and offline recovery.".into());
            }
            Err(error) => {
                composer.durable = false;
                composer.notice = Some(format!(
                    "Local save failed; text remains only in this open window: {error}"
                ));
            }
        }
    }

    pub fn prepare_pending(&mut self, session: &ReviewSession) -> Result<String, String> {
        let draft_id = self.saved_composer_id()?;
        let canonical = CanonicalPublishedPatch::new(session.comparison(), session.metadata())
            .map_err(|error| error.to_string())?;
        let intent = self
            .composition
            .prepare_pending_comment(&draft_id, canonical)
            .map_err(|error| error.to_string())?;
        Ok(intent.operation_id)
    }

    pub fn prepare_immediate(&mut self, session: &ReviewSession) -> Result<String, String> {
        let draft_id = self.saved_composer_id()?;
        let canonical = CanonicalPublishedPatch::new(session.comparison(), session.metadata())
            .map_err(|error| error.to_string())?;
        let intent = self
            .composition
            .prepare_immediate_comment(&draft_id, canonical)
            .map_err(|error| error.to_string())?;
        Ok(intent.operation_id)
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
        Ok(intent.operation_id)
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
    }

    pub fn unresolved_operations(&self) -> usize {
        self.composition
            .operations_requiring_reconciliation()
            .count()
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
}

#[derive(Clone, Debug)]
pub struct ReviewStateAuthority {
    root: PathBuf,
    key: ReviewKey,
}

impl ReviewStateAuthority {
    pub fn open(root: PathBuf, key: ReviewKey) -> Result<Self, String> {
        ensure_private_directory(&root)?;
        Ok(Self { root, key })
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
            let result = execute();
            let durable = load_composition(store, &self.key)?;
            Ok((result, durable))
        })
    }

    fn with_lock<T>(&self, action: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let lock = open_private_lock(&self.lock_path()?)?;
        lock.lock()
            .map_err(|error| format!("Cannot lock review state authority: {error}"))?;
        action()
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
            .join("v1")
            .join(account)
            .join(repository)
            .join(format!("pr-{}.lock", self.key.pull_request)))
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

fn place_thread(session: &ReviewSession, thread: &ReviewThread) -> Result<InlineAnchor, String> {
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
    let (line, exact_commit) = if !thread.outdated {
        (
            thread.line,
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
    let key = file_key(file);
    cibergit::participation::validate_coordinate(session, &key, LineSelection::single(side, line))
        .map_err(|error| format!("The provider anchor is not selectable in this patch: {error}"))?;
    Ok(InlineAnchor {
        file_key: key,
        side,
        line,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum JournalRequest {
    Auxiliary(ReviewAuxiliaryRequest),
    Merge {
        preparation: MergePreparation,
        request: MergeExecutionRequest,
    },
}

impl JournalRequest {
    fn operation_and_attempt(&self) -> (&str, &str) {
        match self {
            Self::Auxiliary(request) => (&request.operation_id, &request.attempt_id),
            Self::Merge { request, .. } => (&request.operation_id, &request.attempt_id),
        }
    }

    fn mutation_context(&self) -> MutationContext {
        let (operation_id, attempt_id) = self.operation_and_attempt();
        let action = match self {
            Self::Auxiliary(request) => match &request.action {
                cibergit::domain::ReviewAuxiliaryAction::UpdatePendingSummary { .. } => {
                    "update pending review summary"
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
        };
        MutationContext {
            operation_id: operation_id.to_owned(),
            attempt_id: attempt_id.to_owned(),
            action: action.into(),
            payload: serde_json::to_value(self)
                .unwrap_or_else(|_| serde_json::json!({"serialization": "failed"})),
        }
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
}

impl ActionJournal {
    pub fn open(root: &Path, key: ReviewKey) -> Result<Self, String> {
        ensure_private_directory(root)?;
        Ok(Self {
            root: root.to_owned(),
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
        let lock = match open_private_lock(&match self.lock_path() {
            Ok(path) => path,
            Err(reason) => {
                return ProviderMutationOutcome::PreflightRejected {
                    reason: format!(
                        "Could not resolve the caller journal lock; zero writes sent: {reason}"
                    ),
                };
            }
        })
        .and_then(|file| {
            file.lock()
                .map_err(|error| format!("Cannot lock action journal: {error}"))?;
            Ok(file)
        }) {
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

    pub fn mark_not_applied(
        &self,
        operation_id: &str,
        attempt_id: &str,
        evidence: String,
    ) -> Result<(), String> {
        let lock = open_private_lock(&self.lock_path()?)?;
        lock.lock()
            .map_err(|error| format!("Cannot lock action journal: {error}"))?;
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
        let lock = open_private_lock(&self.lock_path()?)?;
        lock.lock()
            .map_err(|error| format!("Cannot lock action journal: {error}"))?;
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
        let lock = open_private_lock(&self.lock_path()?)?;
        lock.lock()
            .map_err(|error| format!("Cannot lock action journal: {error}"))?;
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
            .join("v1")
            .join(account)
            .join(repository)
            .join(format!("pr-{}.json", self.key.pull_request)))
    }

    fn lock_path(&self) -> Result<PathBuf, String> {
        Ok(self.path()?.with_extension("lock"))
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
        JournalRequest::Auxiliary(request.clone()),
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
            preparation: preparation.clone(),
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
        Account, ChangedFile, Comparison, MergeEligibility, ProviderCoordinates, PullRequestReview,
        ReviewComment, Revision,
    };
    use cibergit::participation::{DiffSide, ReviewOperationStatus};
    use std::{
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
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
        let journal = ActionJournal {
            root: blocked,
            key: review_key(),
        };
        let calls = AtomicUsize::new(0);
        let outcome: ProviderMutationOutcome<()> = journal.dispatch(
            JournalRequest::Auxiliary(auxiliary_request()),
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
            JournalRequest::Auxiliary(request.clone()),
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
            JournalRequest::Auxiliary(ReviewAuxiliaryRequest {
                operation_id: "aux-2".into(),
                attempt_id: "attempt-2".into(),
                action: request.action,
            }),
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
            JournalRequest::Auxiliary(request.clone()),
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
                    JournalRequest::Auxiliary(request),
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
    fn concurrent_terminal_reconciliation_cannot_overwrite_first_outcome() {
        let directory = tempdir().unwrap();
        let journal = ActionJournal::open(directory.path(), review_key()).unwrap();
        let request = auxiliary_request();
        let _ = journal.dispatch(
            JournalRequest::Auxiliary(request.clone()),
            || ProviderMutationOutcome::<()>::Uncertain {
                context: JournalRequest::Auxiliary(request.clone()).mutation_context(),
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
