//! Immutable published comparisons. Call Git readers on a worker, never the UI thread.
use crate::domain::{ChangedFile, Comparison, Revision};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    io::Read,
    os::unix::ffi::OsStringExt,
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffMode {
    Auto,
    Unified,
    SideBySide,
}
impl DiffMode {
    /// The layout owns its width threshold; explicit choices survive resizing.
    pub fn resolve(self, wide: bool) -> Self {
        match self {
            Self::Auto if wide => Self::SideBySide,
            Self::Auto => Self::Unified,
            explicit => explicit,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComparisonMode {
    FullPullRequest,
    Commit { sha: String },
    CommitRange,
    SinceLastReview { reviewed_head_sha: String },
}
impl ComparisonMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::FullPullRequest => "Full pull request",
            Self::Commit { .. } => "Individual commit",
            Self::CommitRange => "Commit range",
            Self::SinceLastReview { .. } => "Changes since last review",
        }
    }
}

/// Keep a requested mode visible when the reader has fallen back to a full diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComparisonMetadata {
    pub mode: ComparisonMode,
    pub requested_mode: Option<ComparisonMode>,
    pub notice: Option<String>,
}
impl Default for ComparisonMetadata {
    fn default() -> Self {
        Self {
            mode: ComparisonMode::FullPullRequest,
            requested_mode: None,
            notice: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewedFile {
    pub revision: Revision,
    pub fingerprint: String,
}

static READER_INSTANCE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn next_reader_instance() -> u64 {
    READER_INSTANCE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

/// Opaque authority to install one background-loaded patch into one live reader.
///
/// The request captures reader lifetime as well as comparison and file identity.
/// A result issued before another file selection, comparison replacement, clone,
/// close/reopen, or newer request is rejected by `ReviewSession::accept_file_patch`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LazyPatchRequest {
    reader_instance: u64,
    generation: u64,
    revision: Revision,
    file_key: String,
    path: String,
    previous_path: Option<String>,
    raw_path: Option<Vec<u8>>,
    raw_previous_path: Option<Vec<u8>>,
    status: String,
}

impl LazyPatchRequest {
    pub fn revision(&self) -> &Revision {
        &self.revision
    }

    pub fn file_key(&self) -> &str {
        &self.file_key
    }
}

/// One instance per PR tab. Collaboration refreshes have no access to the selected
/// snapshot; a new revision is only installed by an explicit user action.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReviewSession {
    comparison: Arc<Comparison>,
    #[serde(skip)]
    file_indices: Arc<OnceLock<HashMap<String, usize>>>,
    available_revision: Option<Revision>,
    selected_file: Option<String>,
    diff_mode: DiffMode,
    metadata: ComparisonMetadata,
    viewed: HashMap<String, ViewedFile>,
    scroll_positions: HashMap<String, f32>,
    #[serde(default)]
    horizontal_scroll_positions: HashMap<String, f32>,
    #[serde(skip, default = "next_reader_instance")]
    reader_instance: u64,
    #[serde(skip)]
    lazy_patch_generation: u64,
}

impl Clone for ReviewSession {
    fn clone(&self) -> Self {
        Self {
            comparison: Arc::clone(&self.comparison),
            file_indices: Arc::clone(&self.file_indices),
            available_revision: self.available_revision.clone(),
            selected_file: self.selected_file.clone(),
            diff_mode: self.diff_mode,
            metadata: self.metadata.clone(),
            viewed: self.viewed.clone(),
            scroll_positions: self.scroll_positions.clone(),
            horizontal_scroll_positions: self.horizontal_scroll_positions.clone(),
            reader_instance: next_reader_instance(),
            lazy_patch_generation: 0,
        }
    }
}
impl ReviewSession {
    pub fn new(comparison: Comparison) -> Self {
        let selected_file = comparison.files.first().map(file_key);
        Self {
            comparison: Arc::new(comparison),
            file_indices: Arc::default(),
            available_revision: None,
            selected_file,
            // A review opens unified. Auto, which follows the window width, and
            // side-by-side remain a keystroke away and are persisted per PR.
            diff_mode: DiffMode::Unified,
            metadata: ComparisonMetadata::default(),
            viewed: HashMap::new(),
            scroll_positions: HashMap::new(),
            horizontal_scroll_positions: HashMap::new(),
            reader_instance: next_reader_instance(),
            lazy_patch_generation: 0,
        }
    }
    pub fn comparison(&self) -> &Comparison {
        &self.comparison
    }
    /// Share the immutable file inventory without copying patch contents for a view.
    pub fn shared_comparison(&self) -> Arc<Comparison> {
        Arc::clone(&self.comparison)
    }
    pub fn revision(&self) -> &Revision {
        &self.comparison.revision
    }
    pub fn available_revision(&self) -> Option<&Revision> {
        self.available_revision.as_ref()
    }
    pub fn metadata(&self) -> &ComparisonMetadata {
        &self.metadata
    }
    pub fn diff_mode(&self) -> DiffMode {
        self.diff_mode
    }
    pub fn set_diff_mode(&mut self, mode: DiffMode) {
        self.diff_mode = mode;
    }
    /// Receiving comments/checks does not call this method and cannot replace code.
    pub fn observe_revision(&mut self, revision: Revision) {
        self.available_revision = (revision != self.comparison.revision).then_some(revision);
    }
    /// A caller must still refresh provider merge eligibility before any remote write.
    pub fn requires_advance_before_merge(&self) -> bool {
        self.available_revision.is_some()
    }
    /// Reviews target exactly the revision displayed, even when newer code exists.
    pub fn submission_revision(&self) -> &Revision {
        self.revision()
    }
    fn file_index(&self, key: &str) -> Option<usize> {
        self.file_indices
            .get_or_init(|| {
                let mut indices = HashMap::with_capacity(self.comparison.files.len());
                for (index, file) in self.comparison.files.iter().enumerate() {
                    indices.entry(file_key(file)).or_insert(index);
                }
                indices
            })
            .get(key)
            .copied()
    }
    pub fn selected_file(&self) -> Option<&ChangedFile> {
        let index = self.file_index(self.selected_file.as_deref()?)?;
        self.comparison.files.get(index)
    }
    pub fn select_file(&mut self, path: &str) -> bool {
        if self.file_index(path).is_some() {
            if self.selected_file.as_deref() != Some(path) {
                self.selected_file = Some(path.to_owned());
                self.invalidate_lazy_patch_requests();
            }
            true
        } else {
            false
        }
    }
    /// Navigation is bounded (no wrap); false means no selection changed.
    pub fn next_file(&mut self) -> bool {
        self.navigate(1)
    }
    pub fn previous_file(&mut self) -> bool {
        self.navigate(-1)
    }
    fn navigate(&mut self, direction: isize) -> bool {
        let Some(index) = self
            .selected_file
            .as_deref()
            .and_then(|key| self.file_index(key))
        else {
            return false;
        };
        let Some(next) = index.checked_add_signed(direction) else {
            return false;
        };
        let Some(file) = self.comparison.files.get(next) else {
            return false;
        };
        self.selected_file = Some(file_key(file));
        self.invalidate_lazy_patch_requests();
        true
    }
    pub fn set_scroll_position(&mut self, position: f32) {
        if position.is_finite()
            && position >= 0.0
            && let Some(path) = &self.selected_file
        {
            self.scroll_positions.insert(path.clone(), position);
        }
    }
    pub fn scroll_position(&self) -> f32 {
        self.selected_file
            .as_ref()
            .and_then(|p| self.scroll_positions.get(p))
            .copied()
            .unwrap_or(0.0)
    }
    /// Horizontal source position is scoped to this comparison and raw-safe file identity.
    pub fn set_horizontal_scroll_position(&mut self, position: f32) {
        if position.is_finite()
            && position >= 0.0
            && let Some(path) = &self.selected_file
        {
            self.horizontal_scroll_positions
                .insert(path.clone(), position);
        }
    }
    pub fn horizontal_scroll_position(&self) -> f32 {
        self.selected_file
            .as_ref()
            .and_then(|path| self.horizontal_scroll_positions.get(path))
            .copied()
            .filter(|position| position.is_finite() && *position >= 0.0)
            .unwrap_or(0.0)
    }
    pub fn mark_viewed(&mut self, path: &str, viewed: bool) -> bool {
        let Some(file) = self.comparison.files.iter().find(|f| file_key(f) == path) else {
            return false;
        };
        if viewed {
            self.viewed.insert(
                path.to_owned(),
                ViewedFile {
                    revision: self.comparison.revision.clone(),
                    fingerprint: file_fingerprint(file),
                },
            );
        } else {
            self.viewed.remove(path);
        }
        true
    }
    pub fn viewed_file(&self, path: &str) -> Option<&ViewedFile> {
        self.viewed.get(path)
    }
    pub fn is_viewed(&self, path: &str) -> bool {
        self.viewed.contains_key(path)
    }

    /// Issue the only identity a background patch result may be accepted with.
    /// Starting another request supersedes the previous request for this reader.
    pub fn begin_file_patch(&mut self, key: &str) -> Result<LazyPatchRequest> {
        let index = self
            .file_index(key)
            .context("file patch request does not match the displayed comparison")?;
        let file = &self.comparison.files[index];
        self.lazy_patch_generation = self.lazy_patch_generation.saturating_add(1);
        Ok(LazyPatchRequest {
            reader_instance: self.reader_instance,
            generation: self.lazy_patch_generation,
            revision: self.comparison.revision.clone(),
            file_key: file_key(file),
            path: file.path.clone(),
            previous_path: file.previous_path.clone(),
            raw_path: file.raw_path.clone(),
            raw_previous_path: file.raw_previous_path.clone(),
            status: file.status.clone(),
        })
    }

    /// Whether this reader lifetime and comparison can still accept `request`.
    /// Result metadata is checked by `accept_file_patch` before mutation.
    pub fn accepts_file_patch(&self, request: &LazyPatchRequest) -> bool {
        self.reader_instance == request.reader_instance
            && self.lazy_patch_generation == request.generation
            && self.comparison.revision == request.revision
            && self.file_index(&request.file_key).is_some_and(|index| {
                let file = &self.comparison.files[index];
                file.path == request.path
                    && file.previous_path == request.previous_path
                    && file.raw_path == request.raw_path
                    && file.raw_previous_path == request.raw_previous_path
                    && file.status == request.status
            })
    }

    /// Install a background result only when the issuing reader is still current.
    pub fn accept_file_patch(
        &mut self,
        request: &LazyPatchRequest,
        file: ChangedFile,
    ) -> Result<()> {
        ensure!(
            self.accepts_file_patch(request),
            "file patch request is no longer current"
        );
        ensure!(
            file_key(&file) == request.file_key
                && file.path == request.path
                && file.previous_path == request.previous_path
                && file.raw_path == request.raw_path
                && file.raw_previous_path == request.raw_previous_path
                && file.status == request.status,
            "file patch result identity differs from its request"
        );
        self.install_file_patch(&request.revision, file)
    }

    fn invalidate_lazy_patch_requests(&mut self) {
        self.lazy_patch_generation = self.lazy_patch_generation.saturating_add(1);
    }
    /// Install one lazily loaded file without changing the published snapshot or
    /// navigation. A load for a replaced snapshot, or for different metadata, is
    /// rejected instead of being applied to whichever file is currently selected.
    pub fn install_file_patch(
        &mut self,
        expected_revision: &Revision,
        file: ChangedFile,
    ) -> Result<()> {
        ensure!(
            expected_revision == &self.comparison.revision,
            "file patch does not match the displayed revision"
        );
        let key = file_key(&file);
        let index = self
            .comparison
            .files
            .iter()
            .position(|existing| file_key(existing) == key)
            .context("file patch does not match a file in the displayed comparison")?;
        let existing = &self.comparison.files[index];
        ensure!(
            existing.path == file.path
                && existing.previous_path == file.previous_path
                && existing.raw_path == file.raw_path
                && existing.raw_previous_path == file.raw_previous_path
                && existing.status == file.status,
            "file patch identity differs from the displayed file"
        );
        if self
            .viewed
            .get(&key)
            .is_some_and(|viewed| viewed.fingerprint != file_fingerprint(&file))
        {
            self.viewed.remove(&key);
        }
        Arc::make_mut(&mut self.comparison).files[index] = file;
        self.invalidate_lazy_patch_requests();
        Ok(())
    }
    /// Reject an outdated background load if another revision was observed meanwhile.
    pub fn advance(&mut self, comparison: Comparison) -> Result<()> {
        ensure!(
            self.available_revision.as_ref() == Some(&comparison.revision),
            "comparison does not match the available revision"
        );
        self.install(comparison);
        self.available_revision = None;
        Ok(())
    }
    /// Explicit full/commit/range/since-review selection, separate from polling.
    pub fn select_comparison(&mut self, comparison: Comparison, metadata: ComparisonMetadata) {
        self.install(comparison);
        self.metadata = metadata;
    }
    fn install(&mut self, comparison: Comparison) {
        self.invalidate_lazy_patch_requests();
        let same_revision = comparison.revision == self.comparison.revision;
        self.viewed.retain(|path, viewed| {
            comparison
                .files
                .iter()
                .find(|f| file_key(f) == *path)
                .is_some_and(|file| {
                    let unchanged = viewed.fingerprint == file_fingerprint(file);
                    // Missing/binary/truncated patches cannot establish unchanged content.
                    unchanged && (same_revision || parse_file(file).is_complete())
                })
        });
        self.scroll_positions
            .retain(|path, _| comparison.files.iter().any(|f| file_key(f) == *path));
        self.horizontal_scroll_positions
            .retain(|path, _| comparison.files.iter().any(|f| file_key(f) == *path));
        if !comparison
            .files
            .iter()
            .any(|f| Some(&file_key(f)) == self.selected_file.as_ref())
        {
            self.selected_file = comparison.files.first().map(file_key);
        }
        self.comparison = Arc::new(comparison);
        self.file_indices = Arc::default();
    }
}

/// Length-delimited serialization includes paths, rename/status metadata and exact
/// patch bytes. The revision lives in ViewedFile, allowing unchanged files to carry.
pub fn file_fingerprint(file: &ChangedFile) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(file).expect("serializable file"))
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Addition,
    Deletion,
    NoNewline,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<u64>,
    pub new_line: Option<u64>,
    /// Only the patch prefix is removed. Tabs, spaces and carriage returns survive.
    pub text: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub header: String,
    pub old_start: u64,
    pub old_count: u64,
    pub new_start: u64,
    pub new_count: u64,
    pub lines: Vec<DiffLine>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlignedRow {
    pub old: Option<DiffLine>,
    pub new: Option<DiffLine>,
}
impl DiffHunk {
    pub fn unified_rows(&self) -> &[DiffLine] {
        &self.lines
    }
    /// Pair deletion/addition runs by position, preserving empty cells and markers.
    pub fn aligned_rows(&self) -> Vec<AlignedRow> {
        let mut rows = Vec::new();
        let mut old = Vec::new();
        let mut new = Vec::new();
        let flush =
            |rows: &mut Vec<AlignedRow>, old: &mut Vec<DiffLine>, new: &mut Vec<DiffLine>| {
                let mut left = old.drain(..);
                let mut right = new.drain(..);
                loop {
                    let old = left.next();
                    let new = right.next();
                    if old.is_none() && new.is_none() {
                        break;
                    }
                    rows.push(AlignedRow { old, new });
                }
            };
        let mut preceding = DiffLineKind::Context;
        for line in &self.lines {
            match line.kind {
                DiffLineKind::Context => {
                    flush(&mut rows, &mut old, &mut new);
                    rows.push(AlignedRow {
                        old: Some(line.clone()),
                        new: Some(line.clone()),
                    });
                }
                DiffLineKind::Deletion => old.push(line.clone()),
                DiffLineKind::Addition => new.push(line.clone()),
                DiffLineKind::NoNewline => match preceding {
                    DiffLineKind::Deletion => old.push(line.clone()),
                    DiffLineKind::Addition => new.push(line.clone()),
                    _ => rows.push(AlignedRow {
                        old: Some(line.clone()),
                        new: Some(line.clone()),
                    }),
                },
            }
            if line.kind != DiffLineKind::NoNewline {
                preceding = line.kind;
            }
        }
        flush(&mut rows, &mut old, &mut new);
        rows
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchStatus {
    Complete,
    Truncated { reason: String },
    Unsupported { reason: String },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedDiff {
    pub hunks: Vec<DiffHunk>,
    pub status: PatchStatus,
}
impl ParsedDiff {
    pub fn is_complete(&self) -> bool {
        self.status == PatchStatus::Complete
    }
}

/// Parse one standard unified file patch, with or without Git file headers.
/// Hunk counts are verified; malformed/combined/binary patches are never complete.
/// GitHub JSON patch strings may omit the final line separator. Completeness
/// follows declared hunk counts; the transport must reject truncated responses.
pub fn parse_patch(patch: &str) -> ParsedDiff {
    let mut parsed = ParsedDiff {
        hunks: Vec::new(),
        status: PatchStatus::Complete,
    };
    let mut old_used = 0;
    let mut new_used = 0;
    let mut file_headers = 0;
    let mut text_headers = false;
    let fail = |mut parsed: ParsedDiff, reason: &str, unsupported: bool| {
        parsed.status = if unsupported {
            PatchStatus::Unsupported {
                reason: reason.into(),
            }
        } else {
            PatchStatus::Truncated {
                reason: reason.into(),
            }
        };
        parsed
    };
    for line in patch.split_terminator('\n') {
        if parsed.hunks.is_empty() && (line.starts_with("--- ") || line.starts_with("+++ ")) {
            text_headers = true;
        }
        if line.starts_with("@@@")
            || line.starts_with("diff --cc ")
            || line.starts_with("diff --combined ")
            || line == "GIT binary patch"
            || line.starts_with("Binary files ")
        {
            return fail(
                parsed,
                "Binary or combined diff is not a supported text patch",
                true,
            );
        }
        if line.starts_with("@@ ") {
            if parsed
                .hunks
                .last()
                .is_some_and(|h| old_used != h.old_count || new_used != h.new_count)
            {
                return fail(parsed, "Hunk ends before its declared line counts", false);
            }
            let Some((old_start, old_count, new_start, new_count)) = parse_header(line) else {
                return fail(parsed, "Malformed hunk header", true);
            };
            parsed.hunks.push(DiffHunk {
                header: line.into(),
                old_start,
                old_count,
                new_start,
                new_count,
                lines: Vec::new(),
            });
            old_used = 0;
            new_used = 0;
            continue;
        }
        if let Some(hunk) = parsed.hunks.last_mut() {
            if line == "\\ No newline at end of file" {
                if hunk
                    .lines
                    .last()
                    .is_none_or(|l| l.kind == DiffLineKind::NoNewline)
                {
                    return fail(
                        parsed,
                        "No-newline marker has no preceding source line",
                        true,
                    );
                }
                hunk.lines.push(DiffLine {
                    kind: DiffLineKind::NoNewline,
                    old_line: None,
                    new_line: None,
                    text: line.into(),
                });
                continue;
            }
            let (kind, takes_old, takes_new) = match line.as_bytes().first() {
                Some(b' ') => (DiffLineKind::Context, true, true),
                Some(b'-') => (DiffLineKind::Deletion, true, false),
                Some(b'+') => (DiffLineKind::Addition, false, true),
                _ => return fail(parsed, "Unexpected data inside a hunk", true),
            };
            if (takes_old && old_used >= hunk.old_count)
                || (takes_new && new_used >= hunk.new_count)
            {
                return fail(parsed, "Hunk contains more lines than declared", true);
            }
            let old_line = takes_old.then_some(hunk.old_start + old_used);
            let new_line = takes_new.then_some(hunk.new_start + new_used);
            old_used += u64::from(takes_old);
            new_used += u64::from(takes_new);
            hunk.lines.push(DiffLine {
                kind,
                old_line,
                new_line,
                text: line[1..].into(),
            });
        } else if line.starts_with("diff --git ") {
            file_headers += 1;
            if file_headers > 1 {
                return fail(parsed, "Expected one file patch", true);
            }
        } else if !(line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("old mode ")
            || line.starts_with("new mode ")
            || line.starts_with("new file mode ")
            || line.starts_with("deleted file mode ")
            || line.starts_with("similarity index ")
            || line.starts_with("dissimilarity index ")
            || line.starts_with("rename from ")
            || line.starts_with("rename to ")
            || line.starts_with("copy from ")
            || line.starts_with("copy to "))
        {
            return fail(parsed, "Unrecognized patch metadata", true);
        }
    }
    if (text_headers && parsed.hunks.is_empty())
        || parsed
            .hunks
            .last()
            .is_some_and(|h| old_used != h.old_count || new_used != h.new_count)
    {
        return fail(parsed, "Patch ends before the complete hunk", false);
    }
    parsed
}
fn parse_header(line: &str) -> Option<(u64, u64, u64, u64)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, suffix) = rest.split_once(" @@")?;
    if !suffix.is_empty() && !suffix.starts_with(' ') {
        return None;
    }
    let range = |value: &str| -> Option<(u64, u64)> {
        let (start, count) = value.split_once(',').unwrap_or((value, "1"));
        if !start.bytes().all(|c| c.is_ascii_digit()) || !count.bytes().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        let start: u64 = start.parse().ok()?;
        let count: u64 = count.parse().ok()?;
        start.checked_add(count)?;
        if count > 0 && start == 0 {
            return None;
        }
        Some((start, count))
    };
    let (old_start, old_count) = range(old)?;
    let (new_start, new_count) = range(new)?;
    Some((old_start, old_count, new_start, new_count))
}
pub fn parse_file(file: &ChangedFile) -> ParsedDiff {
    let Some(patch) = &file.patch else {
        return ParsedDiff { hunks: Vec::new(), status: PatchStatus::Unsupported { reason: "Text patch unavailable (binary, media, unsupported encoding, size/time limit, or omitted by source)".into() } };
    };
    let mut parsed = parse_patch(patch);
    if parsed.is_complete() {
        let additions = parsed
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == DiffLineKind::Addition)
            .count() as u64;
        let deletions = parsed
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == DiffLineKind::Deletion)
            .count() as u64;
        if !file.patch_complete || additions != file.additions || deletions != file.deletions {
            parsed.status = PatchStatus::Truncated {
                reason: "Source omitted patch lines or did not provide a complete patch".into(),
            };
        }
    }
    parsed
}

/// Resource limits are implementation safeguards, not product performance gates.
pub const MAX_TEXT_BLOB_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_PATCH_BYTES: usize = 2 * 1024 * 1024;
const MAX_GIT_METADATA_BYTES: usize = 32 * 1024 * 1024;
const GIT_READ_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct GitReadBound;
impl std::fmt::Display for GitReadBound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Local Git read exceeded its output or time limit")
    }
}
impl std::error::Error for GitReadBound {}

fn git<S: AsRef<OsStr>>(path: &Path, args: &[S]) -> Result<Vec<u8>> {
    git_bounded(path, args, MAX_GIT_METADATA_BYTES)
}
fn git_bounded<S: AsRef<OsStr>>(path: &Path, args: &[S], limit: usize) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    // Ambient variables must not redirect this explicit repository or inject config.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .arg("--no-pager")
        .args([
            "-c",
            "core.quotePath=true",
            "-c",
            "diff.ignoreSubmodules=none",
            "-c",
            "diff.suppressBlankEmpty=false",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .current_dir(path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command.spawn().context("Start local Git reader")?;
    let mut stdout = child.stdout.take().context("Missing Git output pipe")?;
    let exceeded = Arc::new(AtomicBool::new(false));
    let reader_exceeded = exceeded.clone();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> std::io::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            let mut chunk = [0; 8192];
            loop {
                let count = stdout.read(&mut chunk)?;
                if count == 0 {
                    return Ok(bytes);
                }
                if bytes.len() + count > limit {
                    reader_exceeded.store(true, Ordering::Release);
                    return Ok(bytes);
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
        })();
        let _ = sender.send(result);
    });
    let started = Instant::now();
    let mut bytes = None;
    loop {
        if exceeded.load(Ordering::Acquire) || started.elapsed() >= GIT_READ_DEADLINE {
            // Kill the process group too, in case Git spawned a helper retaining stdout.
            let _ = Command::new("/bin/kill")
                .args(["-KILL", &format!("-{}", child.id())])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = child.kill();
            let _ = child.wait();
            return Err(GitReadBound.into());
        }
        if bytes.is_none() {
            match receiver.try_recv() {
                Ok(result) => bytes = Some(result),
                Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("Local Git output reader stopped");
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(status) = child.try_wait().context("Wait for local Git reader")?
            && let Some(bytes) = bytes
        {
            ensure!(!exceeded.load(Ordering::Acquire), GitReadBound);
            // Stderr may contain private content; never capture or return it.
            ensure!(
                status.success(),
                "Local Git read failed (exit {:?})",
                status.code()
            );
            return bytes.context("Read local Git output");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
/// Accept full SHA-1 or SHA-256 IDs only, never refs, abbreviations or revision syntax.
pub fn validate_object_id(oid: &str) -> Result<()> {
    ensure!(
        (oid.len() == 40 || oid.len() == 64) && oid.bytes().all(|c| c.is_ascii_hexdigit()),
        "Expected a full immutable Git object ID"
    );
    Ok(())
}
/// The base a root commit's diff is taken against.
///
/// A root commit has no parent, so there is no commit to compare it with. Git
/// always provides the empty tree, and asks it be addressed by OID; the OID
/// depends on the repository's hash algorithm, so it is resolved here rather
/// than hard-coded to the SHA-1 spelling.
pub fn empty_tree_oid(path: &Path) -> Result<String> {
    let oid = git(path, &["hash-object", "-t", "tree", "/dev/null"])?;
    let oid = std::str::from_utf8(&oid)
        .context("Invalid empty tree object ID")?
        .trim()
        .to_owned();
    validate_object_id(&oid)?;
    Ok(oid)
}

/// The base endpoint of a diff. Ordinarily a commit, but the empty tree is
/// admitted so a root commit can be shown as the addition of every file in it.
/// The head endpoint is never relaxed this way.
fn validate_diff_base(path: &Path, oid: &str) -> Result<()> {
    validate_object_id(oid)?;
    let kind = git(path, &["cat-file", "-t", oid])
        .with_context(|| format!("Revision object {oid} unavailable locally"))?;
    ensure!(
        kind == b"commit\n" || (kind == b"tree\n" && oid == empty_tree_oid(path)?),
        "Revision object {oid} is not a commit"
    );
    Ok(())
}

fn validate_commit(path: &Path, oid: &str) -> Result<()> {
    validate_object_id(oid)?;
    let kind = git(path, &["cat-file", "-t", oid])
        .with_context(|| format!("Revision object {oid} unavailable locally"))?;
    ensure!(kind == b"commit\n", "Revision object {oid} is not a commit");
    Ok(())
}
struct RawFile {
    old_mode: String,
    new_mode: String,
    old_oid: String,
    new_oid: String,
    status: String,
    path: String,
    previous_path: Option<String>,
    raw_path: Option<Vec<u8>>,
    raw_previous_path: Option<Vec<u8>>,
    path_bytes: Vec<u8>,
}
fn parse_raw(raw: &[u8]) -> Result<Vec<RawFile>> {
    let mut fields = raw.split(|b| *b == 0);
    let mut files = Vec::new();
    while let Some(header) = fields.next() {
        if header.is_empty() {
            ensure!(fields.next().is_none(), "Unexpected empty Git raw record");
            break;
        }
        let header = std::str::from_utf8(header).context("Invalid Git raw metadata")?;
        let parts: Vec<_> = header
            .strip_prefix(':')
            .context("Missing Git raw prefix")?
            .split(' ')
            .collect();
        ensure!(parts.len() == 5, "Invalid Git raw fields");
        let read_path = |bytes: Option<&[u8]>| -> Result<Vec<u8>> {
            let bytes = bytes.context("Missing NUL-delimited Git path")?;
            ensure!(!bytes.is_empty(), "Empty Git path");
            Ok(bytes.to_vec())
        };
        let first = read_path(fields.next())?;
        let (path_bytes, previous_bytes) = if parts[4].starts_with(['R', 'C']) {
            (read_path(fields.next())?, Some(first))
        } else {
            (first, None)
        };
        let (path, raw_path) = display_path(&path_bytes);
        let (previous_path, raw_previous_path) = match previous_bytes {
            Some(bytes) => {
                let (display, raw) = display_path(&bytes);
                (Some(display), raw)
            }
            None => (None, None),
        };
        validate_object_id(parts[2])?;
        validate_object_id(parts[3])?;
        files.push(RawFile {
            old_mode: parts[0].into(),
            new_mode: parts[1].into(),
            old_oid: parts[2].into(),
            new_oid: parts[3].into(),
            status: parts[4].into(),
            path,
            previous_path,
            raw_path,
            raw_previous_path,
            path_bytes,
        });
    }
    Ok(files)
}

fn enumerate_local_files(path: &Path, revision: &Revision) -> Result<Vec<RawFile>> {
    validate_diff_base(path, &revision.base_sha)?;
    validate_commit(path, &revision.head_sha)?;
    let common = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-relative",
        "--ignore-submodules=none",
        "--find-renames",
        "--no-abbrev",
        "--no-renames",
    ];
    // Explicit rename detection overrides both configuration and the initial default.
    let mut args = common.to_vec();
    args.extend([
        "--find-renames=50%",
        "--raw",
        "-z",
        &revision.base_sha,
        &revision.head_sha,
        "--",
    ]);
    parse_raw(&git(path, &args)?)
}

fn changed_file_metadata(file: &RawFile) -> ChangedFile {
    let status = match file.status.as_bytes()[0] {
        b'A' => "added",
        b'D' => "removed",
        b'M' => "modified",
        b'R' => "renamed",
        b'C' => "copied",
        b'T' => "changed",
        _ => "unsupported",
    };
    ChangedFile {
        path: file.path.clone(),
        raw_path: file.raw_path.clone(),
        raw_previous_path: file.raw_previous_path.clone(),
        previous_path: file.previous_path.clone(),
        status: status.into(),
        additions: 0,
        deletions: 0,
        patch: None,
        patch_complete: false,
    }
}

struct LoadedLocalFile {
    file: ChangedFile,
    complete: bool,
}

fn is_regular_mode(mode: &str) -> bool {
    mode == "000000" || mode.starts_with("100")
}

/// Read statistics and a patch for exactly one already-enumerated raw record.
/// A metadata-only result for media, binary or non-regular files is intentional.
fn hydrate_local_file(path: &Path, revision: &Revision, raw: &RawFile) -> Result<LoadedLocalFile> {
    let mut changed = changed_file_metadata(raw);
    let metadata_only = is_media_path(&String::from_utf8_lossy(&raw.path_bytes))
        || raw.previous_path.as_deref().is_some_and(is_media_path)
        || !is_regular_mode(&raw.old_mode)
        || !is_regular_mode(&raw.new_mode);
    if metadata_only {
        return Ok(LoadedLocalFile {
            file: changed,
            complete: true,
        });
    }

    let mut oversized = false;
    for oid in [&raw.old_oid, &raw.new_oid] {
        if oid.bytes().all(|c| c == b'0') {
            continue;
        }
        match git(path, &["cat-file", "-s", oid]) {
            Ok(size) => {
                let size: u64 = std::str::from_utf8(&size)?
                    .trim()
                    .parse()
                    .context("Invalid Git object size")?;
                oversized |= size > MAX_TEXT_BLOB_BYTES;
            }
            Err(error) if error.is::<GitReadBound>() => oversized = true,
            Err(error) => return Err(error),
        }
    }
    if oversized {
        return Ok(LoadedLocalFile {
            file: changed,
            complete: false,
        });
    }

    // Two existing blobs provide an unambiguous per-file diff even when a
    // rename source was reused by another changed file in this comparison.
    let both_exist =
        !raw.old_oid.bytes().all(|c| c == b'0') && !raw.new_oid.bytes().all(|c| c == b'0');
    let mut literal_bytes = b":(literal)".to_vec();
    literal_bytes.extend(&raw.path_bytes);
    let literal = OsString::from_vec(literal_bytes);
    let endpoints: Vec<OsString> = if both_exist {
        vec![
            raw.old_oid.as_str().into(),
            raw.new_oid.as_str().into(),
            "--".into(),
        ]
    } else {
        vec![
            revision.base_sha.as_str().into(),
            revision.head_sha.as_str().into(),
            "--".into(),
            literal,
        ]
    };
    let common = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-relative",
        "--no-renames",
        "--ignore-submodules=none",
        "--diff-algorithm=myers",
        "--no-indent-heuristic",
        "--unified=3",
        "--inter-hunk-context=0",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ];
    // Numstat detects binary data without capturing any blob/media bytes.
    let mut diff_args: Vec<OsString> = common.into_iter().map(OsString::from).collect();
    diff_args.push("--numstat".into());
    diff_args.push("-z".into());
    diff_args.extend(endpoints.iter().cloned());
    let stats = match git(path, &diff_args) {
        Ok(stats) => stats,
        Err(error) if error.is::<GitReadBound>() => {
            return Ok(LoadedLocalFile {
                file: changed,
                complete: false,
            });
        }
        Err(error) => return Err(error),
    };
    let stats = stats.split(|b| *b == 0).next().unwrap_or_default();
    let mut counts = stats.splitn(3, |b| *b == b'\t');
    let added = counts.next().unwrap_or_default();
    let removed = counts.next().unwrap_or_default();
    if added == b"-" || removed == b"-" {
        return Ok(LoadedLocalFile {
            file: changed,
            complete: true,
        });
    }
    if !stats.is_empty() {
        changed.additions = std::str::from_utf8(added)?
            .parse()
            .context("Invalid Git additions")?;
        changed.deletions = std::str::from_utf8(removed)?
            .parse()
            .context("Invalid Git deletions")?;
    }

    diff_args.truncate(diff_args.len() - endpoints.len() - 2);
    diff_args.push("--patch".into());
    diff_args.extend(endpoints);
    let patch = match git_bounded(path, &diff_args, MAX_PATCH_BYTES) {
        Ok(patch) => patch,
        Err(error) if error.is::<GitReadBound>() => {
            return Ok(LoadedLocalFile {
                file: changed,
                complete: false,
            });
        }
        Err(error) => return Err(error),
    };
    match String::from_utf8(patch) {
        Ok(mut patch) => {
            // Blob diffs lack file mode metadata. Preserve it for viewed fingerprints.
            if both_exist && raw.old_mode != raw.new_mode {
                patch = format!(
                    "old mode {}\nnew mode {}\n{patch}",
                    raw.old_mode, raw.new_mode
                );
            }
            changed.patch_complete = parse_patch(&patch).is_complete();
            let complete = changed.patch_complete;
            changed.patch = Some(patch);
            Ok(LoadedLocalFile {
                file: changed,
                complete,
            })
        }
        Err(_) => Ok(LoadedLocalFile {
            file: changed,
            complete: false,
        }),
    }
}

/// Enumerate direct-tree metadata without reading text statistics or patches.
/// Zero counts in this snapshot are placeholders; the notice makes that explicit.
pub fn local_inventory(path: &Path, revision: &Revision) -> Result<Comparison> {
    let files = enumerate_local_files(path, revision)?;
    Ok(Comparison {
        revision: revision.clone(),
        files: files.iter().map(changed_file_metadata).collect(),
        complete: true,
        notice: Some("All changed files are listed. Text patches and diff statistics are not loaded; select a file to load its diff.".into()),
    })
}

/// Enumeration and statistics use NUL records. Media, binary files and gitlinks
/// stay listed without content. Unsupported encodings are explicit, never lossy.
/// Missing objects fail instead of falling through to moving branches/worktree data.
pub fn local_comparison(path: &Path, revision: &Revision) -> Result<Comparison> {
    let files = enumerate_local_files(path, revision)?;
    let mut comparison = Comparison {
        revision: revision.clone(),
        files: Vec::with_capacity(files.len()),
        complete: true,
        notice: None,
    };
    for raw in &files {
        let loaded = hydrate_local_file(path, revision, raw)?;
        comparison.complete &= loaded.complete;
        comparison.files.push(loaded.file);
    }
    if comparison.files.iter().any(|file| !file.patch_complete) {
        comparison.notice = Some("All changed files are listed. Text patches are unavailable for media, binary, non-regular files, unsupported encodings, size/time limits or incomplete diffs.".into());
    }
    Ok(comparison)
}

fn local_pr_effective_revision(path: &Path, revision: &Revision) -> Result<Revision> {
    validate_commit(path, &revision.base_sha)?;
    validate_commit(path, &revision.head_sha)?;
    let bases = git(
        path,
        &[
            "merge-base",
            "--all",
            &revision.base_sha,
            &revision.head_sha,
        ],
    )?;
    let bases: Vec<_> = std::str::from_utf8(&bases)?.split_whitespace().collect();
    ensure!(
        bases.len() == 1,
        "Full PR comparison needs one unambiguous merge-base"
    );
    let effective_base = bases[0];
    validate_object_id(effective_base)?;
    Ok(Revision {
        base_sha: effective_base.into(),
        head_sha: revision.head_sha.clone(),
    })
}

fn publish_pr_revision(mut comparison: Comparison, revision: &Revision) -> Comparison {
    let effective_base = comparison.revision.base_sha.clone();
    comparison.revision = revision.clone();
    if effective_base != revision.base_sha {
        let notice = format!("Full pull request diff uses effective merge-base {effective_base}.");
        comparison.notice = Some(match comparison.notice {
            Some(existing) => format!("{notice} {existing}"),
            None => notice,
        });
    }
    comparison
}

/// Full-PR metadata inventory using the effective merge-base to head, while
/// preserving the caller's selected base/head identity in the published snapshot.
pub fn local_pr_inventory(path: &Path, revision: &Revision) -> Result<Comparison> {
    let effective = local_pr_effective_revision(path, revision)?;
    Ok(publish_pr_revision(
        local_inventory(path, &effective)?,
        revision,
    ))
}

/// Full-PR semantics match a three-dot comparison: effective merge-base to head.
/// The selected published base/head identity remains the caller's pinned revision.
pub fn local_pr_comparison(path: &Path, revision: &Revision) -> Result<Comparison> {
    let effective = local_pr_effective_revision(path, revision)?;
    Ok(publish_pr_revision(
        local_comparison(path, &effective)?,
        revision,
    ))
}

/// Load only the selected file from the caller's pinned revision. The raw file key
/// is matched against NUL-delimited Git metadata; it is never used as a path.
pub fn load_local_file(
    path: &Path,
    revision: &Revision,
    requested_file_key: &str,
    full_pr: bool,
) -> Result<ChangedFile> {
    let effective = if full_pr {
        local_pr_effective_revision(path, revision)?
    } else {
        revision.clone()
    };
    let files = enumerate_local_files(path, &effective)?;
    let raw = files
        .iter()
        .find(|raw| file_key(&changed_file_metadata(raw)) == requested_file_key)
        .context("Selected file is not present in the requested comparison")?;
    Ok(hydrate_local_file(path, &effective, raw)?.file)
}

/// Extension exclusion is deliberately conservative, including text-based images.
pub fn is_media_path(path: &str) -> bool {
    let extension = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "avif"
            | "heic"
            | "heif"
            | "bmp"
            | "tif"
            | "tiff"
            | "ico"
            | "icns"
            | "svg"
            | "psd"
            | "raw"
            | "mp4"
            | "mov"
            | "m4v"
            | "webm"
            | "avi"
            | "mkv"
            | "mpeg"
            | "mpg"
            | "3gp"
            | "ogv"
            | "mp3"
            | "wav"
            | "ogg"
            | "flac"
            | "aac"
            | "m4a"
    )
}

/// Missing previous review objects have an explicit full-PR fallback. Other errors
/// (including a missing selected head/base) remain errors, rather than stale success.
pub fn local_since_last_review(
    path: &Path,
    full_revision: &Revision,
    reviewed_head_sha: &str,
) -> Result<(Comparison, ComparisonMetadata)> {
    validate_object_id(reviewed_head_sha)?;
    validate_commit(path, &full_revision.base_sha)?;
    validate_commit(path, &full_revision.head_sha)?;
    let requested = ComparisonMode::SinceLastReview {
        reviewed_head_sha: reviewed_head_sha.into(),
    };
    match git(path, &["cat-file", "-t", reviewed_head_sha]) {
        Ok(kind) if kind == b"commit\n" => {
            let revision = Revision {
                base_sha: reviewed_head_sha.into(),
                head_sha: full_revision.head_sha.clone(),
            };
            Ok((
                local_comparison(path, &revision)?,
                ComparisonMetadata {
                    mode: requested,
                    requested_mode: None,
                    notice: None,
                },
            ))
        }
        Ok(_) => bail!("Previous review object is not a commit"),
        Err(_) => {
            let comparison = local_pr_comparison(path, full_revision)?;
            Ok((comparison, ComparisonMetadata { mode: ComparisonMode::FullPullRequest, requested_mode: Some(requested), notice: Some("The previous reviewed commit is unavailable locally; showing the full pull request diff.".into()) }))
        }
    }
}

/// Display invalid UTF-8 using byte escapes; operations retain the original bytes.
fn display_path(bytes: &[u8]) -> (String, Option<Vec<u8>>) {
    match std::str::from_utf8(bytes) {
        Ok(path) => (path.into(), None),
        Err(_) => {
            let mut display = String::new();
            for byte in bytes {
                match byte {
                    b' '..=b'~' if *byte != b'\\' => display.push(*byte as char),
                    _ => display.push_str(&format!("\\x{byte:02x}")),
                }
            }
            (display, Some(bytes.to_vec()))
        }
    }
}

/// Stable selection identity. The NUL namespace cannot collide with a Git path.
/// Callers use the display path for labels and this key for file actions.
pub fn file_key(file: &ChangedFile) -> String {
    match &file.raw_path {
        Some(bytes) => format!(
            "\0raw:{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ),
        None => file.path.clone(),
    }
}
