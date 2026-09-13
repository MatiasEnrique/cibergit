//! Pure local review composition, coordinate validation, and durable recovery.
//!
//! This module prepares provider writes but never executes them. In particular,
//! restoring an in-flight or uncertain operation does not make it replayable.

use crate::{
    domain::{
        Account, ChangedFile, Comparison, ProviderCoordinates, PullRequestDetails, Repository,
        Revision,
    },
    review::{
        ComparisonMetadata, ComparisonMode, DiffLineKind, ReviewSession, file_key, parse_file,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub const MAX_DRAFT_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_REVIEW_BODY_BYTES: usize = 64 * 1024;
pub const MAX_DRAFTS: usize = 256;
pub const MAX_OPERATIONS: usize = 512;
pub const MAX_COMMENT_RANGE_LINES: u64 = 100;
pub const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;
const FORMAT_VERSION: u64 = 1;
const MAX_IDENTITY_BYTES: usize = 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum ParticipationError {
    InvalidIdentity(&'static str),
    DraftNotFound(String),
    OperationNotFound(String),
    InvalidCoordinate(String),
    Mapping(MappingIssue),
    TextTooLarge {
        field: &'static str,
        limit: usize,
    },
    TooManyDrafts {
        limit: usize,
    },
    TooManyOperations {
        limit: usize,
    },
    UnsynchronizedDrafts {
        count: usize,
    },
    OperationNeedsReconciliation(String),
    OperationState(String),
    RemoteState(String),
    RecordTooLarge {
        limit: usize,
    },
    RecoveryRequired {
        path: PathBuf,
        reason: String,
    },
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Serialize(serde_json::Error),
}

impl fmt::Display for ParticipationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity(field) => {
                write!(f, "review identity field `{field}` is empty or too long")
            }
            Self::DraftNotFound(id) => write!(f, "review draft `{id}` does not exist"),
            Self::OperationNotFound(id) => write!(f, "review operation `{id}` does not exist"),
            Self::InvalidCoordinate(reason) => write!(f, "invalid review coordinate: {reason}"),
            Self::Mapping(issue) => write!(
                f,
                "comment cannot be mapped to the canonical published patch: {issue}"
            ),
            Self::TextTooLarge { field, limit } => {
                write!(f, "{field} exceeds the {limit}-byte limit")
            }
            Self::TooManyDrafts { limit } => write!(f, "review has more than {limit} local drafts"),
            Self::TooManyOperations { limit } => {
                write!(f, "review has more than {limit} retained operations")
            }
            Self::UnsynchronizedDrafts { count } => write!(
                f,
                "{count} pending local draft(s) must be synchronized before submission"
            ),
            Self::OperationNeedsReconciliation(id) => write!(
                f,
                "review operation `{id}` has an in-flight or uncertain outcome that must be reconciled before another publish attempt"
            ),
            Self::OperationState(reason) | Self::RemoteState(reason) => f.write_str(reason),
            Self::RecordTooLarge { limit } => {
                write!(f, "review recovery record exceeds the {limit}-byte limit")
            }
            Self::RecoveryRequired { path, reason } => write!(
                f,
                "review recovery file `{}` must be preserved and reconciled before saving: {reason}",
                path.display()
            ),
            Self::Io {
                action,
                path,
                source,
            } => write!(f, "could not {action} `{}`: {source}", path.display()),
            Self::Serialize(source) => {
                write!(f, "could not serialize review recovery record: {source}")
            }
        }
    }
}

impl Error for ParticipationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Serialize(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReviewKey {
    pub provider: String,
    pub host: String,
    pub owner: String,
    pub repository: String,
    pub account: Account,
    pub pull_request: u64,
}

impl ReviewKey {
    pub fn for_repository(
        provider: impl Into<String>,
        repository: &Repository,
        pull_request: u64,
    ) -> Result<Self, ParticipationError> {
        let key = Self {
            provider: provider.into(),
            host: repository.host.clone(),
            owner: repository.owner.clone(),
            repository: repository.name.clone(),
            account: repository.account.clone(),
            pull_request,
        };
        key.validate()?;
        Ok(key)
    }

    pub fn validate(&self) -> Result<(), ParticipationError> {
        for (name, value) in [
            ("provider", self.provider.as_str()),
            ("host", self.host.as_str()),
            ("owner", self.owner.as_str()),
            ("repository", self.repository.as_str()),
            ("account.host", self.account.host.as_str()),
            ("account.login", self.account.login.as_str()),
        ] {
            if value.is_empty() || value.len() > MAX_IDENTITY_BYTES || value.contains('\0') {
                return Err(ParticipationError::InvalidIdentity(name));
            }
        }
        if self.pull_request == 0 {
            return Err(ParticipationError::InvalidIdentity("pull_request"));
        }
        Ok(())
    }

    fn matches(&self, coordinates: &ProviderCoordinates) -> bool {
        self.provider == coordinates.provider
            && self.host == coordinates.host
            && self.owner == coordinates.owner
            && self.repository == coordinates.repository
            && self.pull_request == coordinates.pull_request
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DiffSide {
    Old,
    New,
}

impl DiffSide {
    pub fn provider_name(self) -> &'static str {
        match self {
            Self::Old => "LEFT",
            Self::New => "RIGHT",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineSelection {
    pub side: DiffSide,
    /// The inclusive first line. Use the same value as `line` for one line.
    pub start_line: u64,
    /// The inclusive final line and provider anchor.
    pub line: u64,
}

impl LineSelection {
    pub fn single(side: DiffSide, line: u64) -> Self {
        Self {
            side,
            start_line: line,
            line,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoordinateLineKind {
    Context,
    Addition,
    Deletion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineAnchor {
    pub line: u64,
    pub kind: CoordinateLineKind,
    pub text: String,
}

/// A local comment anchor. It retains the active comparison revision and exact
/// file bytes even when no provider-safe UTF-8 path can be produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftCoordinate {
    pub reviewed_revision: Revision,
    pub file_key: String,
    pub path: String,
    pub previous_path: Option<String>,
    pub raw_path: Option<Vec<u8>>,
    pub raw_previous_path: Option<Vec<u8>>,
    pub side: DiffSide,
    pub start_line: u64,
    pub line: u64,
    pub anchors: Vec<LineAnchor>,
}

/// Validate a selectable context/changed line against the parsed comparison
/// currently pinned by the session. No line-offset inference is performed.
pub fn validate_coordinate(
    session: &ReviewSession,
    selected_file_key: &str,
    selection: LineSelection,
) -> Result<DraftCoordinate, ParticipationError> {
    validate_line_range(selection)?;
    let file = session
        .comparison()
        .files
        .iter()
        .find(|file| file_key(file) == selected_file_key)
        .ok_or_else(|| {
            ParticipationError::InvalidCoordinate(
                "the selected file is not in the pinned comparison".into(),
            )
        })?;
    let anchors = anchors_for(file, selection).map_err(ParticipationError::InvalidCoordinate)?;
    Ok(DraftCoordinate {
        reviewed_revision: session.revision().clone(),
        file_key: file_key(file),
        path: file.path.clone(),
        previous_path: file.previous_path.clone(),
        raw_path: file.raw_path.clone(),
        raw_previous_path: file.raw_previous_path.clone(),
        side: selection.side,
        start_line: selection.start_line,
        line: selection.line,
        anchors,
    })
}

fn validate_line_range(selection: LineSelection) -> Result<(), ParticipationError> {
    if selection.start_line == 0 || selection.line == 0 || selection.start_line > selection.line {
        return Err(ParticipationError::InvalidCoordinate(
            "the inclusive line range must be positive and ordered".into(),
        ));
    }
    let count = selection.line - selection.start_line + 1;
    if count > MAX_COMMENT_RANGE_LINES {
        return Err(ParticipationError::InvalidCoordinate(format!(
            "a comment range may contain at most {MAX_COMMENT_RANGE_LINES} lines"
        )));
    }
    Ok(())
}

fn anchors_for(file: &ChangedFile, selection: LineSelection) -> Result<Vec<LineAnchor>, String> {
    let parsed = parse_file(file);
    if !parsed.is_complete() {
        return Err("the selected file does not have a complete parsed text patch".into());
    }
    let mut anchors = Vec::new();
    for wanted in selection.start_line..=selection.line {
        let mut found = None;
        for line in parsed.hunks.iter().flat_map(|hunk| &hunk.lines) {
            let (number, selectable, kind) = match (selection.side, line.kind) {
                (DiffSide::Old, DiffLineKind::Context) => {
                    (line.old_line, true, CoordinateLineKind::Context)
                }
                (DiffSide::New, DiffLineKind::Context) => {
                    (line.new_line, true, CoordinateLineKind::Context)
                }
                (DiffSide::Old, DiffLineKind::Deletion) => {
                    (line.old_line, true, CoordinateLineKind::Deletion)
                }
                (DiffSide::New, DiffLineKind::Addition) => {
                    (line.new_line, true, CoordinateLineKind::Addition)
                }
                _ => (None, false, CoordinateLineKind::Context),
            };
            if selectable && number == Some(wanted) {
                if found.is_some() {
                    return Err(format!("line {wanted} appears more than once in the patch"));
                }
                found = Some(LineAnchor {
                    line: wanted,
                    kind,
                    text: line.text.clone(),
                });
            }
        }
        anchors.push(found.ok_or_else(|| {
            format!(
                "line {wanted} is not selectable on the {} side",
                selection.side.provider_name()
            )
        })?);
    }
    Ok(anchors)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MappingIssue {
    InvalidStoredCoordinate,
    NotCanonicalPublishedPatch,
    OutdatedRevision {
        reviewed_head: String,
        canonical_head: String,
    },
    OldSideBaseMismatch {
        reviewed_base: String,
        canonical_base: String,
    },
    FileMissing,
    FileIdentityChanged,
    RawPathUnsupported,
    CanonicalPatchIncomplete,
    LineMissing {
        side: DiffSide,
        line: u64,
    },
    LineChanged {
        side: DiffSide,
        line: u64,
    },
}

impl fmt::Display for MappingIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidStoredCoordinate => {
                f.write_str("the stored local coordinate failed its integrity checks")
            }
            Self::NotCanonicalPublishedPatch => {
                f.write_str("the supplied comparison is not marked as the full pull request")
            }
            Self::OutdatedRevision {
                reviewed_head,
                canonical_head,
            } => write!(
                f,
                "reviewed head {reviewed_head} differs from canonical head {canonical_head}"
            ),
            Self::OldSideBaseMismatch {
                reviewed_base,
                canonical_base,
            } => write!(
                f,
                "OLD-side reviewed base {reviewed_base} differs from canonical base {canonical_base}"
            ),
            Self::FileMissing => f.write_str("the file is absent"),
            Self::FileIdentityChanged => f.write_str("the raw-safe file identity changed"),
            Self::RawPathUnsupported => {
                f.write_str("the exact path is not valid UTF-8 for the provider API")
            }
            Self::CanonicalPatchIncomplete => {
                f.write_str("the canonical file patch is incomplete or unsupported")
            }
            Self::LineMissing { side, line } => write!(
                f,
                "line {line} is absent on the {} side",
                side.provider_name()
            ),
            Self::LineChanged { side, line } => write!(
                f,
                "line {line} changed on the {} side",
                side.provider_name()
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedPosition {
    pub commit_sha: String,
    pub path: String,
    pub side: DiffSide,
    pub line: u64,
    pub start_line: Option<u64>,
    pub start_side: Option<DiffSide>,
}

/// A comparison explicitly identified as the full published PR patch. The
/// marker prevents accidentally validating against the active commit/range view.
#[derive(Clone, Copy, Debug)]
pub struct CanonicalPublishedPatch<'a> {
    comparison: &'a Comparison,
}

impl<'a> CanonicalPublishedPatch<'a> {
    pub fn new(
        comparison: &'a Comparison,
        metadata: &ComparisonMetadata,
    ) -> Result<Self, MappingIssue> {
        if metadata.mode != ComparisonMode::FullPullRequest {
            return Err(MappingIssue::NotCanonicalPublishedPatch);
        }
        Ok(Self { comparison })
    }

    pub fn comparison(self) -> &'a Comparison {
        self.comparison
    }
}

/// Verify an active full/commit/range/since-review coordinate against the
/// canonical published PR patch at the same immutable head. A different base is
/// accepted only when every exact anchor is present and unchanged.
pub fn map_to_canonical_published(
    coordinate: &DraftCoordinate,
    canonical: CanonicalPublishedPatch<'_>,
) -> Result<PublishedPosition, MappingIssue> {
    let canonical = canonical.comparison();
    if !coordinate_has_valid_shape(coordinate) {
        return Err(MappingIssue::InvalidStoredCoordinate);
    }
    if coordinate.reviewed_revision.head_sha != canonical.revision.head_sha {
        return Err(MappingIssue::OutdatedRevision {
            reviewed_head: coordinate.reviewed_revision.head_sha.clone(),
            canonical_head: canonical.revision.head_sha.clone(),
        });
    }
    // Equal OLD-side line/text in different base blobs does not establish the
    // same original coordinate. Without exact old-blob identity, fail closed.
    if coordinate.side == DiffSide::Old
        && coordinate.reviewed_revision.base_sha != canonical.revision.base_sha
    {
        return Err(MappingIssue::OldSideBaseMismatch {
            reviewed_base: coordinate.reviewed_revision.base_sha.clone(),
            canonical_base: canonical.revision.base_sha.clone(),
        });
    }
    let file = canonical
        .files
        .iter()
        .find(|file| file_key(file) == coordinate.file_key)
        .ok_or(MappingIssue::FileMissing)?;
    if file.path != coordinate.path
        || file.previous_path != coordinate.previous_path
        || file.raw_path != coordinate.raw_path
        || file.raw_previous_path != coordinate.raw_previous_path
    {
        return Err(MappingIssue::FileIdentityChanged);
    }
    if coordinate.raw_path.is_some() {
        return Err(MappingIssue::RawPathUnsupported);
    }
    if !parse_file(file).is_complete() {
        return Err(MappingIssue::CanonicalPatchIncomplete);
    }
    for expected in &coordinate.anchors {
        let actual = anchors_for(file, LineSelection::single(coordinate.side, expected.line))
            .map_err(|_| MappingIssue::LineMissing {
                side: coordinate.side,
                line: expected.line,
            })?
            .pop()
            .expect("one requested anchor");
        if actual.kind != expected.kind || actual.text != expected.text {
            return Err(MappingIssue::LineChanged {
                side: coordinate.side,
                line: expected.line,
            });
        }
    }
    let multiline = coordinate.start_line != coordinate.line;
    Ok(PublishedPosition {
        commit_sha: coordinate.reviewed_revision.head_sha.clone(),
        path: coordinate.path.clone(),
        side: coordinate.side,
        line: coordinate.line,
        start_line: multiline.then_some(coordinate.start_line),
        start_side: multiline.then_some(coordinate.side),
    })
}

fn coordinate_has_valid_shape(coordinate: &DraftCoordinate) -> bool {
    let selection = LineSelection {
        side: coordinate.side,
        start_line: coordinate.start_line,
        line: coordinate.line,
    };
    let Ok(()) = validate_line_range(selection) else {
        return false;
    };
    let expected_count = (coordinate.line - coordinate.start_line + 1) as usize;
    if coordinate.anchors.len() != expected_count {
        return false;
    }
    let expected_key = match &coordinate.raw_path {
        Some(bytes) => format!(
            "\0raw:{}",
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
        None => coordinate.path.clone(),
    };
    coordinate.file_key == expected_key
        && coordinate
            .anchors
            .iter()
            .zip(coordinate.start_line..=coordinate.line)
            .all(|(anchor, line)| {
                anchor.line == line
                    && matches!(
                        (coordinate.side, anchor.kind),
                        (
                            DiffSide::Old,
                            CoordinateLineKind::Context | CoordinateLineKind::Deletion
                        ) | (
                            DiffSide::New,
                            CoordinateLineKind::Context | CoordinateLineKind::Addition
                        )
                    )
            })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DraftDisposition {
    Pending,
    PostedImmediately,
    Submitted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDraftIds {
    pub review_id: Option<String>,
    pub comment_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalDraft {
    pub id: String,
    pub coordinate: DraftCoordinate,
    pub body: String,
    pub dirty: bool,
    pub disposition: DraftDisposition,
    /// Acknowledged provider identity is separate from editable local text.
    pub remote: Option<RemoteDraftIds>,
    /// Last observed provider text; a dirty local body is never overwritten by it.
    pub observed_remote_body: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewOperationTarget {
    SynchronizePendingComment { draft_id: String },
    PostImmediateComment { draft_id: String },
    SubmitReview { event: ReviewEvent },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewOperationStatus {
    Prepared,
    InFlight {
        attempt_id: String,
    },
    Uncertain {
        attempt_id: String,
        reason: String,
    },
    /// An explicit provider read established that this attempt did not apply.
    /// Preparing a later retry remains a separate user action.
    NotApplied {
        evidence: String,
    },
    Acknowledged {
        remote_review_id: Option<String>,
        remote_comment_id: Option<String>,
    },
}

impl ReviewOperationStatus {
    pub fn requires_reconciliation(&self) -> bool {
        matches!(self, Self::InFlight { .. } | Self::Uncertain { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewOperation {
    pub id: String,
    pub target: ReviewOperationTarget,
    pub status: ReviewOperationStatus,
    /// Exact dispatched data survives later edits and restart. Legacy records
    /// without a payload remain readable but cannot start an execution.
    #[serde(default)]
    pub payload: Option<ReviewOperationPayload>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewOperationPayload {
    PendingComment(PendingCommentIntent),
    ImmediateComment(ImmediateCommentIntent),
    Submission(SubmissionIntent),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingCommentIntent {
    pub operation_id: String,
    pub key: ReviewKey,
    pub draft_id: String,
    pub body: String,
    pub position: PublishedPosition,
    pub pending_review_id: Option<String>,
    pub existing_comment_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImmediateCommentIntent {
    pub operation_id: String,
    pub key: ReviewKey,
    pub draft_id: String,
    pub body: String,
    pub position: PublishedPosition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionIntent {
    pub operation_id: String,
    pub key: ReviewKey,
    pub reviewed_commit_sha: String,
    pub pending_review_id: Option<String>,
    pub event: ReviewEvent,
    pub body: String,
    pub newer_head_warning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComposition {
    pub key: ReviewKey,
    pub reviewed_revision: Revision,
    pub drafts: Vec<LocalDraft>,
    pub operations: Vec<ReviewOperation>,
    /// Last pending review identity observed for this account. Absence on a later
    /// refresh does not infer that the review was submitted.
    pub observed_pending_review_id: Option<String>,
    /// Provider identity returned by an acknowledged local operation.
    pub acknowledged_pending_review_id: Option<String>,
    next_draft_number: u64,
    next_operation_number: u64,
}

impl ReviewComposition {
    pub fn new(key: ReviewKey, reviewed_revision: Revision) -> Result<Self, ParticipationError> {
        key.validate()?;
        validate_revision(&reviewed_revision)?;
        Ok(Self {
            key,
            reviewed_revision,
            drafts: Vec::new(),
            operations: Vec::new(),
            observed_pending_review_id: None,
            acknowledged_pending_review_id: None,
            next_draft_number: 1,
            next_operation_number: 1,
        })
    }

    /// Adds a local pending-review draft. Immediate posting is never the default.
    pub fn add_draft(
        &mut self,
        coordinate: DraftCoordinate,
        body: impl Into<String>,
    ) -> Result<&LocalDraft, ParticipationError> {
        if self.drafts.len() >= MAX_DRAFTS {
            return Err(ParticipationError::TooManyDrafts { limit: MAX_DRAFTS });
        }
        if coordinate.reviewed_revision != self.reviewed_revision {
            return Err(ParticipationError::InvalidCoordinate(
                "the draft revision differs from the review composition".into(),
            ));
        }
        if !coordinate_has_valid_shape(&coordinate) {
            return Err(ParticipationError::InvalidCoordinate(
                "the draft coordinate failed its integrity checks".into(),
            ));
        }
        let body = body.into();
        validate_text("draft text", &body, MAX_DRAFT_TEXT_BYTES)?;
        let id = format!("draft-{}", self.next_draft_number);
        self.next_draft_number = self.next_draft_number.checked_add(1).ok_or_else(|| {
            ParticipationError::OperationState("local draft identifier space is exhausted".into())
        })?;
        self.drafts.push(LocalDraft {
            id,
            coordinate,
            body,
            dirty: true,
            disposition: DraftDisposition::Pending,
            remote: None,
            observed_remote_body: None,
        });
        Ok(self.drafts.last().expect("just pushed"))
    }

    pub fn edit_draft(
        &mut self,
        id: &str,
        body: impl Into<String>,
    ) -> Result<(), ParticipationError> {
        let body = body.into();
        validate_text("draft text", &body, MAX_DRAFT_TEXT_BYTES)?;
        let draft = self.draft_mut(id)?;
        if draft.body != body {
            draft.body = body;
            draft.dirty = true;
        }
        Ok(())
    }

    pub fn draft(&self, id: &str) -> Option<&LocalDraft> {
        self.drafts.iter().find(|draft| draft.id == id)
    }

    fn draft_mut(&mut self, id: &str) -> Result<&mut LocalDraft, ParticipationError> {
        self.drafts
            .iter_mut()
            .find(|draft| draft.id == id)
            .ok_or_else(|| ParticipationError::DraftNotFound(id.into()))
    }

    pub fn operations_requiring_reconciliation(&self) -> impl Iterator<Item = &ReviewOperation> {
        self.operations
            .iter()
            .filter(|operation| operation.status.requires_reconciliation())
    }

    pub fn prepare_pending_comment(
        &mut self,
        draft_id: &str,
        canonical: CanonicalPublishedPatch<'_>,
    ) -> Result<PendingCommentIntent, ParticipationError> {
        self.ensure_target_available(|_| true)?;
        let draft = self
            .draft(draft_id)
            .ok_or_else(|| ParticipationError::DraftNotFound(draft_id.into()))?;
        if draft.disposition != DraftDisposition::Pending {
            return Err(ParticipationError::OperationState(
                "a completed comment is historical; create a new draft for another comment".into(),
            ));
        }
        validate_nonempty_text("draft text", &draft.body, MAX_DRAFT_TEXT_BYTES)?;
        let position = map_to_canonical_published(&draft.coordinate, canonical)
            .map_err(ParticipationError::Mapping)?;
        let body = draft.body.clone();
        let existing_comment_id = draft
            .remote
            .as_ref()
            .map(|remote| remote.comment_id.clone());
        let pending_review_id = draft
            .remote
            .as_ref()
            .and_then(|remote| remote.review_id.clone())
            .or_else(|| self.observed_pending_review_id.clone())
            .or_else(|| self.acknowledged_pending_review_id.clone());
        let operation_id =
            self.push_operation(ReviewOperationTarget::SynchronizePendingComment {
                draft_id: draft_id.into(),
            })?;
        let intent = PendingCommentIntent {
            operation_id,
            key: self.key.clone(),
            draft_id: draft_id.into(),
            body,
            position,
            pending_review_id,
            existing_comment_id,
        };
        self.operation_mut(&intent.operation_id)?.payload =
            Some(ReviewOperationPayload::PendingComment(intent.clone()));
        Ok(intent)
    }

    pub fn prepare_immediate_comment(
        &mut self,
        draft_id: &str,
        canonical: CanonicalPublishedPatch<'_>,
    ) -> Result<ImmediateCommentIntent, ParticipationError> {
        self.ensure_target_available(|_| true)?;
        let draft = self
            .draft(draft_id)
            .ok_or_else(|| ParticipationError::DraftNotFound(draft_id.into()))?;
        if draft.remote.is_some() {
            return Err(ParticipationError::OperationState(
                "a comment already attached to a pending review cannot be posted immediately"
                    .into(),
            ));
        }
        if draft.disposition != DraftDisposition::Pending {
            return Err(ParticipationError::OperationState(
                "a completed comment is historical; create a new draft for another comment".into(),
            ));
        }
        validate_nonempty_text("draft text", &draft.body, MAX_DRAFT_TEXT_BYTES)?;
        let position = map_to_canonical_published(&draft.coordinate, canonical)
            .map_err(ParticipationError::Mapping)?;
        let body = draft.body.clone();
        let operation_id = self.push_operation(ReviewOperationTarget::PostImmediateComment {
            draft_id: draft_id.into(),
        })?;
        let intent = ImmediateCommentIntent {
            operation_id,
            key: self.key.clone(),
            draft_id: draft_id.into(),
            body,
            position,
        };
        self.operation_mut(&intent.operation_id)?.payload =
            Some(ReviewOperationPayload::ImmediateComment(intent.clone()));
        Ok(intent)
    }

    pub fn prepare_submission(
        &mut self,
        event: ReviewEvent,
        body: impl Into<String>,
        currently_available_head: Option<&str>,
    ) -> Result<SubmissionIntent, ParticipationError> {
        self.ensure_target_available(|_| true)?;
        let unsynchronized = self
            .drafts
            .iter()
            .filter(|draft| {
                draft.disposition == DraftDisposition::Pending
                    && (draft.dirty || draft.remote.is_none())
            })
            .count();
        if unsynchronized > 0 {
            return Err(ParticipationError::UnsynchronizedDrafts {
                count: unsynchronized,
            });
        }
        let body = body.into();
        validate_text("review body", &body, MAX_REVIEW_BODY_BYTES)?;
        let warning = currently_available_head
            .filter(|head| *head != self.reviewed_revision.head_sha)
            .map(|head| format!(
                "Newer commit {head} is available; this review targets the displayed commit {}.",
                self.reviewed_revision.head_sha
            ));
        let operation_id = self.push_operation(ReviewOperationTarget::SubmitReview {
            event: event.clone(),
        })?;
        let intent = SubmissionIntent {
            operation_id,
            key: self.key.clone(),
            reviewed_commit_sha: self.reviewed_revision.head_sha.clone(),
            pending_review_id: self
                .observed_pending_review_id
                .clone()
                .or_else(|| self.acknowledged_pending_review_id.clone()),
            event,
            body,
            newer_head_warning: warning,
        };
        self.operation_mut(&intent.operation_id)?.payload =
            Some(ReviewOperationPayload::Submission(intent.clone()));
        Ok(intent)
    }

    fn ensure_target_available(
        &self,
        matches_target: impl Fn(&ReviewOperationTarget) -> bool,
    ) -> Result<(), ParticipationError> {
        if let Some(operation) = self.operations.iter().find(|operation| {
            matches_target(&operation.target)
                && matches!(
                    operation.status,
                    ReviewOperationStatus::Prepared
                        | ReviewOperationStatus::InFlight { .. }
                        | ReviewOperationStatus::Uncertain { .. }
                )
        }) {
            return if operation.status.requires_reconciliation() {
                Err(ParticipationError::OperationNeedsReconciliation(
                    operation.id.clone(),
                ))
            } else {
                Err(ParticipationError::OperationState(format!(
                    "review operation `{}` is already prepared",
                    operation.id
                )))
            };
        }
        Ok(())
    }

    /// Discard a locally prepared intent before any executor has started it.
    pub fn cancel_prepared(&mut self, operation_id: &str) -> Result<(), ParticipationError> {
        let index = self
            .operations
            .iter()
            .position(|operation| operation.id == operation_id)
            .ok_or_else(|| ParticipationError::OperationNotFound(operation_id.into()))?;
        if self.operations[index].status != ReviewOperationStatus::Prepared {
            return Err(ParticipationError::OperationState(format!(
                "review operation `{operation_id}` has already started"
            )));
        }
        self.operations.remove(index);
        Ok(())
    }

    fn push_operation(
        &mut self,
        target: ReviewOperationTarget,
    ) -> Result<String, ParticipationError> {
        if self.operations.len() >= MAX_OPERATIONS {
            return Err(ParticipationError::TooManyOperations {
                limit: MAX_OPERATIONS,
            });
        }
        let id = format!("operation-{}", self.next_operation_number);
        self.next_operation_number =
            self.next_operation_number.checked_add(1).ok_or_else(|| {
                ParticipationError::OperationState(
                    "local operation identifier space is exhausted".into(),
                )
            })?;
        self.operations.push(ReviewOperation {
            id: id.clone(),
            target,
            status: ReviewOperationStatus::Prepared,
            payload: None,
        });
        Ok(id)
    }

    pub fn mark_in_flight(
        &mut self,
        operation_id: &str,
        attempt_id: impl Into<String>,
    ) -> Result<(), ParticipationError> {
        let attempt_id = attempt_id.into();
        validate_nonempty_id("attempt_id", &attempt_id)?;
        let operation = self.operation_mut(operation_id)?;
        if operation.status != ReviewOperationStatus::Prepared || operation.payload.is_none() {
            return Err(ParticipationError::OperationState(format!(
                "review operation `{operation_id}` has no executable prepared payload"
            )));
        }
        operation.status = ReviewOperationStatus::InFlight { attempt_id };
        Ok(())
    }

    pub fn mark_uncertain(
        &mut self,
        operation_id: &str,
        reason: impl Into<String>,
    ) -> Result<(), ParticipationError> {
        let reason = reason.into();
        validate_nonempty_text("uncertain outcome reason", &reason, MAX_DRAFT_TEXT_BYTES)?;
        let operation = self.operation_mut(operation_id)?;
        let ReviewOperationStatus::InFlight { attempt_id } = &operation.status else {
            return Err(ParticipationError::OperationState(format!(
                "review operation `{operation_id}` is not in flight"
            )));
        };
        operation.status = ReviewOperationStatus::Uncertain {
            attempt_id: attempt_id.clone(),
            reason,
        };
        Ok(())
    }

    /// Record an explicit provider read proving that a started attempt did not
    /// take effect. This resolves uncertainty but never prepares a retry.
    pub fn reconcile_observed_not_applied(
        &mut self,
        operation_id: &str,
        evidence: impl Into<String>,
    ) -> Result<(), ParticipationError> {
        let evidence = evidence.into();
        validate_nonempty_text(
            "not-applied reconciliation evidence",
            &evidence,
            MAX_DRAFT_TEXT_BYTES,
        )?;
        let operation = self.operation_mut(operation_id)?;
        if !matches!(
            operation.status,
            ReviewOperationStatus::InFlight { .. } | ReviewOperationStatus::Uncertain { .. }
        ) {
            return Err(ParticipationError::OperationState(format!(
                "review operation `{operation_id}` has no started attempt to reconcile"
            )));
        }
        operation.status = ReviewOperationStatus::NotApplied { evidence };
        Ok(())
    }

    /// Reconcile provider-observed success. This is deliberately separate from
    /// timeout handling, so a timeout can never be treated as rejection/retry.
    pub fn reconcile_observed_comment_success(
        &mut self,
        operation_id: &str,
        remote_review_id: Option<String>,
        remote_comment_id: String,
        remote_body: String,
    ) -> Result<(), ParticipationError> {
        validate_nonempty_id("remote_comment_id", &remote_comment_id)?;
        if let Some(id) = &remote_review_id {
            validate_nonempty_id("remote_review_id", id)?;
        }
        validate_text("remote comment body", &remote_body, MAX_DRAFT_TEXT_BYTES)?;
        let target = self
            .operations
            .iter()
            .find(|operation| operation.id == operation_id)
            .ok_or_else(|| ParticipationError::OperationNotFound(operation_id.into()))?
            .target
            .clone();
        let (draft_id, disposition) = match target {
            ReviewOperationTarget::SynchronizePendingComment { draft_id } => {
                (draft_id, DraftDisposition::Pending)
            }
            ReviewOperationTarget::PostImmediateComment { draft_id } => {
                (draft_id, DraftDisposition::PostedImmediately)
            }
            ReviewOperationTarget::SubmitReview { .. } => {
                return Err(ParticipationError::OperationState(
                    "comment success cannot acknowledge a submission operation".into(),
                ));
            }
        };
        if disposition == DraftDisposition::Pending && remote_review_id.is_none() {
            return Err(ParticipationError::RemoteState(
                "a synchronized pending comment must identify its remote pending review".into(),
            ));
        }
        {
            let operation = self.operation_mut(operation_id)?;
            if !matches!(
                operation.status,
                ReviewOperationStatus::InFlight { .. } | ReviewOperationStatus::Uncertain { .. }
            ) {
                return Err(ParticipationError::OperationState(format!(
                    "review operation `{operation_id}` has no started attempt to reconcile"
                )));
            }
            operation.status = ReviewOperationStatus::Acknowledged {
                remote_review_id: remote_review_id.clone(),
                remote_comment_id: Some(remote_comment_id.clone()),
            };
        }
        let draft = self.draft_mut(&draft_id)?;
        draft.remote = Some(RemoteDraftIds {
            review_id: remote_review_id.clone(),
            comment_id: remote_comment_id,
        });
        draft.observed_remote_body = Some(remote_body.clone());
        if draft.body == remote_body {
            draft.dirty = false;
        }
        draft.disposition = disposition;
        if disposition == DraftDisposition::Pending
            && let Some(id) = remote_review_id
        {
            self.acknowledged_pending_review_id = Some(id);
        }
        Ok(())
    }

    pub fn reconcile_observed_submission_success(
        &mut self,
        operation_id: &str,
        remote_review_id: String,
    ) -> Result<(), ParticipationError> {
        validate_nonempty_id("remote_review_id", &remote_review_id)?;
        let operation = self.operation_mut(operation_id)?;
        if !matches!(operation.target, ReviewOperationTarget::SubmitReview { .. }) {
            return Err(ParticipationError::OperationState(
                "submission success cannot acknowledge a comment operation".into(),
            ));
        }
        if !matches!(
            operation.status,
            ReviewOperationStatus::InFlight { .. } | ReviewOperationStatus::Uncertain { .. }
        ) {
            return Err(ParticipationError::OperationState(format!(
                "review operation `{operation_id}` has no started attempt to reconcile"
            )));
        }
        if let Some(ReviewOperationPayload::Submission(intent)) = &operation.payload
            && let Some(expected) = &intent.pending_review_id
            && expected != &remote_review_id
        {
            return Err(ParticipationError::RemoteState(
                "submission acknowledgement identifies a different pending review".into(),
            ));
        }
        operation.status = ReviewOperationStatus::Acknowledged {
            remote_review_id: Some(remote_review_id.clone()),
            remote_comment_id: None,
        };
        self.retire_pending_review(&remote_review_id);
        Ok(())
    }

    fn retire_pending_review(&mut self, remote_review_id: &str) {
        for draft in &mut self.drafts {
            if draft.disposition == DraftDisposition::Pending
                && draft
                    .remote
                    .as_ref()
                    .and_then(|remote| remote.review_id.as_deref())
                    == Some(remote_review_id)
            {
                if draft.dirty {
                    // Edits made while submission ran stay as a new local draft.
                    draft.remote = None;
                    draft.observed_remote_body = None;
                } else {
                    draft.disposition = DraftDisposition::Submitted;
                }
            }
        }
        if self.observed_pending_review_id.as_deref() == Some(remote_review_id) {
            self.observed_pending_review_id = None;
        }
        if self.acknowledged_pending_review_id.as_deref() == Some(remote_review_id) {
            self.acknowledged_pending_review_id = None;
        }
    }

    fn operation_mut(&mut self, id: &str) -> Result<&mut ReviewOperation, ParticipationError> {
        self.operations
            .iter_mut()
            .find(|operation| operation.id == id)
            .ok_or_else(|| ParticipationError::OperationNotFound(id.into()))
    }

    /// Reconcile typed provider activity without touching the pinned session.
    /// Only already-acknowledged comment IDs are eligible to update clean text;
    /// unrelated remote comments are not guessed to belong to a pending review.
    pub fn reconcile_remote_pending(
        &mut self,
        details: &PullRequestDetails,
    ) -> Result<ReconcileReport, ParticipationError> {
        if details.number != self.key.pull_request {
            return Err(ParticipationError::RemoteState(
                "remote details belong to another pull request".into(),
            ));
        }
        let pending: Vec<_> = details
            .reviews
            .iter()
            .filter(|review| {
                review.state == "PENDING"
                    && review
                        .author
                        .as_deref()
                        .is_some_and(|author| author.eq_ignore_ascii_case(&self.key.account.login))
                    && self.key.matches(&review.coordinates)
            })
            .collect();
        if pending.len() > 1 {
            return Err(ParticipationError::RemoteState(
                "provider returned more than one pending review for the selected account".into(),
            ));
        }
        let completed: Vec<_> = details
            .reviews
            .iter()
            .filter(|review| {
                matches!(
                    review.state.as_str(),
                    "APPROVED" | "CHANGES_REQUESTED" | "COMMENTED" | "DISMISSED"
                ) && review
                    .author
                    .as_deref()
                    .is_some_and(|author| author.eq_ignore_ascii_case(&self.key.account.login))
                    && self.key.matches(&review.coordinates)
            })
            .map(|review| review.coordinates.remote_id.clone())
            .collect();
        for remote_id in completed {
            self.retire_pending_review(&remote_id);
        }
        let previous = self.observed_pending_review_id.clone();
        self.observed_pending_review_id = pending
            .first()
            .map(|review| review.coordinates.remote_id.clone())
            .or_else(|| {
                (!details.activity_complete)
                    .then(|| previous.clone())
                    .flatten()
            });

        let mut report = ReconcileReport {
            pending_review_changed: previous != self.observed_pending_review_id,
            clean_drafts_updated: 0,
            dirty_drafts_preserved: 0,
            linked_comments_missing: 0,
        };
        for draft in &mut self.drafts {
            let Some(remote) = &draft.remote else {
                continue;
            };
            let observed = details
                .review_threads
                .iter()
                .flat_map(|thread| &thread.comments)
                .find(|comment| {
                    comment.coordinates.remote_id == remote.comment_id
                        && self.key.matches(&comment.coordinates)
                });
            let Some(observed) = observed else {
                report.linked_comments_missing += 1;
                continue;
            };
            validate_text("remote comment body", &observed.body, MAX_DRAFT_TEXT_BYTES)?;
            draft.observed_remote_body = Some(observed.body.clone());
            if draft.dirty {
                report.dirty_drafts_preserved += 1;
            } else if draft.body != observed.body {
                draft.body = observed.body.clone();
                report.clean_drafts_updated += 1;
            }
        }
        Ok(report)
    }

    fn validate_bounds(&self) -> Result<(), ParticipationError> {
        self.key.validate()?;
        validate_revision(&self.reviewed_revision)?;
        if self.drafts.len() > MAX_DRAFTS {
            return Err(ParticipationError::TooManyDrafts { limit: MAX_DRAFTS });
        }
        if self.operations.len() > MAX_OPERATIONS {
            return Err(ParticipationError::TooManyOperations {
                limit: MAX_OPERATIONS,
            });
        }
        let mut draft_ids = HashSet::new();
        for draft in &self.drafts {
            validate_nonempty_id("draft_id", &draft.id)?;
            draft
                .id
                .strip_prefix("draft-")
                .and_then(|number| number.parse::<u64>().ok())
                .filter(|number| *number > 0 && *number < self.next_draft_number)
                .ok_or_else(|| {
                    ParticipationError::OperationState(
                        "review recovery record contains an invalid local draft counter".into(),
                    )
                })?;
            if !draft_ids.insert(draft.id.as_str()) {
                return Err(ParticipationError::OperationState(
                    "review recovery record contains duplicate draft IDs".into(),
                ));
            }
            if draft.coordinate.reviewed_revision != self.reviewed_revision
                || !coordinate_has_valid_shape(&draft.coordinate)
            {
                return Err(ParticipationError::InvalidCoordinate(
                    "a recovered draft coordinate failed its integrity checks".into(),
                ));
            }
            validate_text("draft text", &draft.body, MAX_DRAFT_TEXT_BYTES)?;
            if let Some(body) = &draft.observed_remote_body {
                validate_text("remote comment body", body, MAX_DRAFT_TEXT_BYTES)?;
            }
            if let Some(remote) = &draft.remote {
                validate_nonempty_id("remote_comment_id", &remote.comment_id)?;
                if let Some(id) = &remote.review_id {
                    validate_nonempty_id("remote_review_id", id)?;
                }
            }
        }
        let mut operation_ids = HashSet::new();
        for operation in &self.operations {
            if let Some(payload) = &operation.payload {
                let (id, key, target, body, head, position) = match payload {
                    ReviewOperationPayload::PendingComment(intent) => (
                        &intent.operation_id,
                        &intent.key,
                        ReviewOperationTarget::SynchronizePendingComment {
                            draft_id: intent.draft_id.clone(),
                        },
                        &intent.body,
                        &intent.position.commit_sha,
                        Some(&intent.position),
                    ),
                    ReviewOperationPayload::ImmediateComment(intent) => (
                        &intent.operation_id,
                        &intent.key,
                        ReviewOperationTarget::PostImmediateComment {
                            draft_id: intent.draft_id.clone(),
                        },
                        &intent.body,
                        &intent.position.commit_sha,
                        Some(&intent.position),
                    ),
                    ReviewOperationPayload::Submission(intent) => (
                        &intent.operation_id,
                        &intent.key,
                        ReviewOperationTarget::SubmitReview {
                            event: intent.event.clone(),
                        },
                        &intent.body,
                        &intent.reviewed_commit_sha,
                        None,
                    ),
                };
                if id != &operation.id
                    || key != &self.key
                    || target != operation.target
                    || head != &self.reviewed_revision.head_sha
                {
                    return Err(ParticipationError::OperationState(
                        "stored operation payload does not match its review identity or target"
                            .into(),
                    ));
                }
                validate_text("operation body", body, MAX_REVIEW_BODY_BYTES)?;
                if let Some(position) = position {
                    validate_nonempty_text("comment body", body, MAX_DRAFT_TEXT_BYTES)?;
                    validate_nonempty_id("comment path", &position.path)?;
                    validate_line_range(LineSelection {
                        side: position.side,
                        start_line: position.start_line.unwrap_or(position.line),
                        line: position.line,
                    })?;
                    if position
                        .start_side
                        .is_some_and(|side| side != position.side)
                    {
                        return Err(ParticipationError::InvalidCoordinate(
                            "stored operation range crosses diff sides".into(),
                        ));
                    }
                }
            }
            validate_nonempty_id("operation_id", &operation.id)?;
            operation
                .id
                .strip_prefix("operation-")
                .and_then(|number| number.parse::<u64>().ok())
                .filter(|number| *number > 0 && *number < self.next_operation_number)
                .ok_or_else(|| {
                    ParticipationError::OperationState(
                        "review recovery record contains an invalid local operation counter".into(),
                    )
                })?;
            if !operation_ids.insert(operation.id.as_str()) {
                return Err(ParticipationError::OperationState(
                    "review recovery record contains duplicate operation IDs".into(),
                ));
            }
            match &operation.target {
                ReviewOperationTarget::SynchronizePendingComment { draft_id }
                | ReviewOperationTarget::PostImmediateComment { draft_id } => {
                    if !draft_ids.contains(draft_id.as_str()) {
                        return Err(ParticipationError::OperationState(format!(
                            "review operation `{}` refers to a missing draft",
                            operation.id
                        )));
                    }
                }
                ReviewOperationTarget::SubmitReview { .. } => {}
            }
            match &operation.status {
                ReviewOperationStatus::InFlight { attempt_id }
                | ReviewOperationStatus::Uncertain { attempt_id, .. } => {
                    validate_nonempty_id("attempt_id", attempt_id)?;
                }
                ReviewOperationStatus::Acknowledged {
                    remote_review_id,
                    remote_comment_id,
                } => {
                    if let Some(id) = remote_review_id {
                        validate_nonempty_id("remote_review_id", id)?;
                    }
                    if let Some(id) = remote_comment_id {
                        validate_nonempty_id("remote_comment_id", id)?;
                    }
                }
                ReviewOperationStatus::Prepared | ReviewOperationStatus::NotApplied { .. } => {}
            }
            if let ReviewOperationStatus::Uncertain { reason, .. } = &operation.status {
                validate_text("uncertain outcome reason", reason, MAX_DRAFT_TEXT_BYTES)?;
            }
            if let ReviewOperationStatus::NotApplied { evidence } = &operation.status {
                validate_nonempty_text(
                    "not-applied reconciliation evidence",
                    evidence,
                    MAX_DRAFT_TEXT_BYTES,
                )?;
            }
        }
        if self.next_draft_number == 0 || self.next_operation_number == 0 {
            return Err(ParticipationError::OperationState(
                "review recovery record contains an exhausted local identifier counter".into(),
            ));
        }
        for id in [
            self.observed_pending_review_id.as_deref(),
            self.acknowledged_pending_review_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            validate_nonempty_id("remote_review_id", id)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub pending_review_changed: bool,
    pub clean_drafts_updated: usize,
    pub dirty_drafts_preserved: usize,
    pub linked_comments_missing: usize,
}

fn validate_text(field: &'static str, text: &str, limit: usize) -> Result<(), ParticipationError> {
    if text.len() > limit {
        Err(ParticipationError::TextTooLarge { field, limit })
    } else {
        Ok(())
    }
}

fn validate_nonempty_text(
    field: &'static str,
    text: &str,
    limit: usize,
) -> Result<(), ParticipationError> {
    if text.is_empty() {
        Err(ParticipationError::OperationState(format!(
            "{field} must not be empty"
        )))
    } else {
        validate_text(field, text, limit)
    }
}

fn validate_nonempty_id(field: &'static str, value: &str) -> Result<(), ParticipationError> {
    if value.is_empty() || value.len() > MAX_IDENTITY_BYTES || value.contains('\0') {
        Err(ParticipationError::InvalidIdentity(field))
    } else {
        Ok(())
    }
}

fn validate_revision(revision: &Revision) -> Result<(), ParticipationError> {
    validate_nonempty_id("revision.base_sha", &revision.base_sha)?;
    validate_nonempty_id("revision.head_sha", &revision.head_sha)
}

#[derive(Serialize, Deserialize)]
struct StoredRecord {
    version: u64,
    state: ReviewComposition,
}

#[derive(Debug)]
pub enum LoadOutcome {
    Missing,
    Loaded(Box<ReviewComposition>),
    /// The original file remains untouched for recovery or migration.
    Corrupt {
        path: PathBuf,
        reason: String,
    },
    /// The original file remains untouched for a newer application version.
    FutureVersion {
        path: PathBuf,
        version: u64,
    },
}

#[derive(Clone, Debug)]
pub struct DraftStore {
    root: PathBuf,
}

impl DraftStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ParticipationError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|source| {
            io_error("create private review recovery directory", &root, source)
        })?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(|source| {
            io_error(
                "set private review recovery directory permissions",
                &root,
                source,
            )
        })?;
        Ok(Self { root })
    }

    pub fn record_path(&self, key: &ReviewKey) -> Result<PathBuf, ParticipationError> {
        key.validate()?;
        let account = stable_component(&(key.account.host.as_str(), key.account.login.as_str()))?;
        let repository = stable_component(&(
            key.provider.as_str(),
            key.host.as_str(),
            key.owner.as_str(),
            key.repository.as_str(),
        ))?;
        Ok(self
            .root
            .join("v1")
            .join(account)
            .join(repository)
            .join(format!("pr-{}.json", key.pull_request)))
    }

    pub fn save(&self, state: &ReviewComposition) -> Result<(), ParticipationError> {
        state.validate_bounds()?;
        let record = StoredRecord {
            version: FORMAT_VERSION,
            state: state.clone(),
        };
        let bytes = serde_json::to_vec(&record).map_err(ParticipationError::Serialize)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(ParticipationError::RecordTooLarge {
                limit: MAX_RECORD_BYTES,
            });
        }
        let path = self.record_path(&state.key)?;
        match self.load(&state.key)? {
            LoadOutcome::Missing | LoadOutcome::Loaded(_) => {}
            LoadOutcome::Corrupt { path, reason } => {
                return Err(ParticipationError::RecoveryRequired { path, reason });
            }
            LoadOutcome::FutureVersion { path, version } => {
                return Err(ParticipationError::RecoveryRequired {
                    path,
                    reason: format!("record version {version} is newer than {FORMAT_VERSION}"),
                });
            }
        }
        let parent = path.parent().expect("record path has parent");
        fs::create_dir_all(parent)
            .map_err(|source| io_error("create review recovery partition", parent, source))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|source| {
            io_error("set review recovery partition permissions", parent, source)
        })?;

        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".review-{}-{sequence}.tmp", std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|source| {
                    io_error("create atomic review recovery file", &temporary, source)
                })?;
            file.write_all(&bytes)
                .map_err(|source| io_error("write review recovery file", &temporary, source))?;
            file.sync_all()
                .map_err(|source| io_error("sync review recovery file", &temporary, source))?;
            fs::rename(&temporary, &path)
                .map_err(|source| io_error("replace review recovery file", &path, source))?;
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|source| io_error("sync review recovery directory", parent, source))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn load(&self, key: &ReviewKey) -> Result<LoadOutcome, ParticipationError> {
        let path = self.record_path(key)?;
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(LoadOutcome::Missing);
            }
            Err(source) => return Err(io_error("open review recovery file", &path, source)),
        };
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_RECORD_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|source| io_error("read review recovery file", &path, source))?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Ok(LoadOutcome::Corrupt {
                path,
                reason: format!("record exceeds the {MAX_RECORD_BYTES}-byte limit"),
            });
        }
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(source) => {
                return Ok(LoadOutcome::Corrupt {
                    path,
                    reason: source.to_string(),
                });
            }
        };
        let Some(version) = value.get("version").and_then(serde_json::Value::as_u64) else {
            return Ok(LoadOutcome::Corrupt {
                path,
                reason: "record has no numeric version".into(),
            });
        };
        if version > FORMAT_VERSION {
            return Ok(LoadOutcome::FutureVersion { path, version });
        }
        if version != FORMAT_VERSION {
            return Ok(LoadOutcome::Corrupt {
                path,
                reason: format!("unsupported old record version {version}"),
            });
        }
        let record: StoredRecord = match serde_json::from_value(value) {
            Ok(record) => record,
            Err(source) => {
                return Ok(LoadOutcome::Corrupt {
                    path,
                    reason: source.to_string(),
                });
            }
        };
        if record.state.key != *key {
            return Ok(LoadOutcome::Corrupt {
                path,
                reason: "record identity does not match its account/repository/PR partition".into(),
            });
        }
        if let Err(source) = record.state.validate_bounds() {
            return Ok(LoadOutcome::Corrupt {
                path,
                reason: source.to_string(),
            });
        }
        Ok(LoadOutcome::Loaded(Box::new(record.state)))
    }
}

fn stable_component(value: &impl Serialize) -> Result<String, ParticipationError> {
    let bytes = serde_json::to_vec(value).map_err(ParticipationError::Serialize)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn io_error(action: &'static str, path: &Path, source: io::Error) -> ParticipationError {
    ParticipationError::Io {
        action,
        path: path.to_owned(),
        source,
    }
}
