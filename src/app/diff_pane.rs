//! The rendered half of a diff pane: the rows, the metrics measured from them,
//! and the two scroll handles that carry them.
//!
//! History and Stack each held their own copy of these five fields and their own
//! copy of the four methods that maintain them, and the copies had already
//! drifted: one rebuilt its horizontal handle and the other reused it, and both
//! restored their vertical offset through an argument that does not mean what
//! the call site assumed (see `rebuild`). A pane is one thing, so it is one
//! type, and the surfaces that show a diff differ in what they put *around* it
//! rather than in how it is built.
//!
//! What is deliberately not here: which comparison or file is selected.
//! `ReviewSession` owns that lifecycle and the per-file offsets; this module
//! turns its immutable patch data into rows and prepares their rendering state
//! without deciding what the reader should show.

use super::{DIFF_CELL_WIDTH, InlineThread};
pub(crate) use cibergit::workspace::ReadingMode;
use cibergit::{
    participation::DiffSide,
    review::{AlignedRow, DiffLine, DiffMode, ParsedDiff, PatchStatus, ReviewSession},
};
use gpui::{ListAlignment, ListState, ScrollHandle, point, px};
use std::{collections::HashSet, ops::Range, rc::Rc};

/// One file's run of rows inside a streamed comparison.
///
/// This is the index that makes Stream navigable: it answers "which file is
/// this row in" for the cursor and the sticky header, "where does this file
/// start" for the tree and the ruler, and it carries the file's own horizontal
/// scroll so one minified line cannot make every other file scrollable to the
/// same absurd width.
#[derive(Clone)]
pub(crate) struct FileSpan {
    pub key: String,
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
    /// Rows belonging to this file, the `FileHeader` included.
    pub rows: Range<usize>,
    pub horizontal: ScrollHandle,
    pub text_width: f32,
}

/// How far past the viewport the list builds rows. One screen of a long diff,
/// which is what keeps a fast scroll from painting an empty band before the
/// rows catch up.
const DIFF_OVERDRAW: f32 = 480.;

/// One materialized row in either an interactive or read-only comparison.
#[derive(Clone)]
pub(crate) enum DiffRow {
    FileHeader {
        key: String,
        path: String,
        additions: u64,
        deletions: u64,
        loaded: bool,
        collapsed: bool,
    },
    Hunk(String),
    Unified(DiffLine),
    Split(AlignedRow),
    Thread(Box<InlineThread>),
    Composer {
        side: DiffSide,
        start_line: u64,
        line: u64,
        canonical_reanchored: bool,
    },
}

/// Build display rows from a parsed patch without acquiring UI state.
pub(crate) fn build_rows(diff: ParsedDiff, mode: DiffMode) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    for hunk in diff.hunks {
        rows.push(DiffRow::Hunk(hunk.header.clone()));
        match mode {
            DiffMode::SideBySide => {
                rows.extend(hunk.aligned_rows().into_iter().map(DiffRow::Split));
            }
            _ => rows.extend(hunk.lines.into_iter().map(DiffRow::Unified)),
        }
    }
    match diff.status {
        PatchStatus::Complete => {}
        PatchStatus::Truncated { reason } | PatchStatus::Unsupported { reason } => {
            rows.push(DiffRow::Hunk(format!("Notice: {reason}")));
        }
    }
    rows
}

pub(crate) fn display_columns(text: &str) -> usize {
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

pub(crate) fn diff_text_metrics(rows: &[DiffRow]) -> (bool, f32) {
    let split = rows.iter().any(|row| matches!(row, DiffRow::Split(_)));
    let text_width = rows
        .iter()
        .filter_map(|row| match row {
            DiffRow::Split(row) if split => Some(
                row.old
                    .iter()
                    .chain(row.new.iter())
                    .map(|line| display_columns(&line.text) as f32 * DIFF_CELL_WIDTH)
                    .fold(0f32, f32::max),
            ),
            DiffRow::Unified(line) if !split => {
                Some(display_columns(&line.text) as f32 * DIFF_CELL_WIDTH)
            }
            _ => None,
        })
        .fold(1f32, f32::max);
    (split, text_width)
}

pub(crate) struct DiffPaneState {
    /// Behind an `Rc` so a render hands the list closure a pointer copy. The
    /// virtualization regression pins this: rebuilding the vector inside
    /// `render` instead of here fails its `Rc::ptr_eq` across redraws.
    pub rows: Rc<Vec<DiffRow>>,
    pub split: bool,
    /// The scroll content width of a row's source column — at least the longest
    /// line, or the column has nothing to scroll.
    pub text_width: f32,
    pub vertical: ListState,
    pub horizontal: ScrollHandle,
    /// One entry per file when streaming, empty when showing one file.
    /// Whether this is empty is what the pane means by "which mode am I in";
    /// there is no second flag to disagree with it.
    pub spans: Rc<Vec<FileSpan>>,
    /// Files whose rows are folded away, by key.
    ///
    /// Held here rather than rebuilt from the rows, because it has to outlive
    /// them: switching diff mode, loading a patch, or posting a comment all
    /// rebuild the stream, and a fold the reader set by hand must survive all
    /// three. `clear` drops it, because that is the comparison going away.
    pub collapsed: HashSet<String>,
    /// Which row the keyboard is on, or `None` before it has been placed.
    ///
    /// An index into `rows` rather than a line number or a file offset: every
    /// jump this pane offers — a line, a hunk header, a comment thread — is a
    /// row, so one index answers all of them and none of them can point at a
    /// row that is not there. It is cleared whenever the rows are replaced,
    /// because an index into a vector that no longer exists is the one way
    /// this could silently land somewhere wrong.
    pub cursor: Option<usize>,
}

impl DiffPaneState {
    pub fn new() -> Self {
        Self {
            rows: Rc::default(),
            split: false,
            text_width: 1.,
            vertical: ListState::new(0, ListAlignment::Top, px(DIFF_OVERDRAW)),
            horizontal: ScrollHandle::new(),
            spans: Rc::default(),
            collapsed: HashSet::new(),
            cursor: None,
        }
    }

    /// Fold or unfold one file. The caller rebuilds; this only records intent.
    pub fn toggle_file(&mut self, key: &str) -> bool {
        if !self.collapsed.remove(key) {
            self.collapsed.insert(key.to_owned());
        }
        true
    }

    /// True when every file that could be folded already is. This is what the
    /// one button reads to decide which way it points: a mixed scroll collapses
    /// first, and only an entirely folded one offers to open again.
    pub fn all_collapsed(&self) -> bool {
        !self.spans.is_empty()
            && self
                .spans
                .iter()
                .all(|span| self.collapsed.contains(&span.key))
    }

    /// Fold or unfold the whole comparison.
    pub fn set_all_collapsed(&mut self, collapsed: bool) {
        if collapsed {
            self.collapsed = self.spans.iter().map(|span| span.key.clone()).collect();
        } else {
            self.collapsed.clear();
        }
    }

    /// The horizontal handle and content width a row scrolls with: its own
    /// file's when streaming, the pane's when showing one file.
    pub fn scroll_context(&self, row: usize) -> (ScrollHandle, f32) {
        match self.span_of(row) {
            Some(span) => (span.horizontal.clone(), span.text_width),
            None => (self.horizontal.clone(), self.text_width),
        }
    }

    /// The same lookup, captured once so a list closure can answer it per row
    /// without borrowing the pane. A render builds one of these and clones it
    /// into the closure; the handles inside are reference-counted, so the
    /// clone costs one refcount per file rather than one per row.
    pub fn scroll_contexts(&self) -> ScrollContexts {
        ScrollContexts {
            spans: self.spans.clone(),
            fallback: (self.horizontal.clone(), self.text_width),
        }
    }

    /// The file a row belongs to. The spans are contiguous and ordered, so
    /// this is a binary search rather than a scan of every file on every row.
    pub fn span_of(&self, row: usize) -> Option<&FileSpan> {
        let index = self
            .spans
            .binary_search_by(|span| {
                if span.rows.end <= row {
                    std::cmp::Ordering::Less
                } else if span.rows.start > row {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()?;
        self.spans.get(index)
    }

    /// Where a file's rows begin, for the tree and the ruler to jump to.
    pub fn span_for_key(&self, key: &str) -> Option<&FileSpan> {
        self.spans.iter().find(|span| span.key == key)
    }

    /// Bring a file's first row to the top of the viewport.
    pub fn reveal_file(&mut self, key: &str) -> bool {
        let Some(start) = self.span_for_key(key).map(|span| span.rows.start) else {
            return false;
        };
        self.vertical.scroll_to_reveal_item(start);
        self.cursor = Some(start);
        true
    }

    /// The file showing at the top of the viewport, which is the one a sticky
    /// header names.
    pub fn leading_file(&self) -> Option<&FileSpan> {
        let top = self.vertical.logical_scroll_top().item_ix;
        self.span_of(top).or_else(|| self.spans.last())
    }

    /// Drop the rendered diff. The session behind it is untouched: clearing is
    /// about what is on screen, not about what the reader has selected or
    /// marked viewed.
    pub fn clear(&mut self) {
        self.rows = Rc::default();
        self.split = false;
        self.text_width = 1.;
        self.vertical = ListState::new(0, ListAlignment::Top, px(DIFF_OVERDRAW));
        self.horizontal = ScrollHandle::new();
        self.spans = Rc::default();
        self.collapsed.clear();
        self.cursor = None;
    }

    /// Take a prepared row vector: measure it, rebuild the list state around
    /// it, and restore the offsets the session remembers for the file it came
    /// from.
    ///
    /// The review pane builds its own rows, because it interleaves comment
    /// threads and a composer among them, and hands them here. History and
    /// Stack reach the same place through `rebuild`.
    ///
    /// The vertical restore is two steps, and has to be. `ListState::new` takes
    /// `overdraw` as its third argument, not an initial offset — History and
    /// Stack both passed the saved position there, so their vertical restore
    /// silently did nothing and their overdraw budget varied with how far down
    /// the reader had been. Build with a fixed overdraw, then scroll.
    pub fn install(&mut self, rows: Vec<DiffRow>, session: &ReviewSession) {
        self.spans = Rc::default();
        self.rows = Rc::new(rows);
        self.cursor = None;
        (self.split, self.text_width) = diff_text_metrics(&self.rows);
        self.vertical = ListState::new(self.rows.len(), ListAlignment::Top, px(DIFF_OVERDRAW));
        let vertical = session.scroll_position();
        if vertical > 0. {
            self.vertical.scroll_by(px(vertical));
        }
        // A fresh handle rather than an offset on the old one: `bounds()` is
        // whatever element registered last, and a stale width from the file
        // just closed is what `effective_diff_viewport_width` would then read.
        self.horizontal = ScrollHandle::new();
        self.horizontal
            .set_offset(point(px(-session.horizontal_scroll_position()), px(0.)));
    }

    /// Take a streamed comparison: every file's rows in one vector, with the
    /// index that says where each file's run begins and ends.
    ///
    /// The vertical offset is not restored here. A session remembers a scroll
    /// position per file, and those positions describe a pane showing one file;
    /// applying one to a scroll that spans the whole comparison would land
    /// somewhere unrelated. Stream opens at the selected file instead, which is
    /// the same intent expressed in the units this mode actually has.
    pub fn install_stream(&mut self, rows: Vec<DiffRow>, spans: Vec<FileSpan>, split: bool) {
        self.rows = Rc::new(rows);
        self.spans = Rc::new(spans);
        self.split = split;
        self.text_width = 1.;
        self.horizontal = ScrollHandle::new();
        self.cursor = None;
        self.vertical = ListState::new(self.rows.len(), ListAlignment::Top, px(DIFF_OVERDRAW));
    }

    /// True when this pane is showing the whole comparison rather than one file.
    pub fn streaming(&self) -> bool {
        !self.spans.is_empty()
    }

    /// Rebuild the rows for the session's selected file. The read-only panes
    /// show the patch and nothing else, so this is the whole of their work.
    pub fn rebuild(&mut self, session: &ReviewSession, wide: bool) {
        let Some(file) = session.selected_file() else {
            self.clear();
            return;
        };
        let mode = session.diff_mode().resolve(wide);
        self.install(
            build_rows(cibergit::review::parse_file(file), mode),
            session,
        );
    }

    /// Put the cursor on `row` and bring it into view. Out-of-range indices
    /// are refused rather than clamped: every caller derives its target from
    /// `rows`, so one that is past the end is a bug, not a long scroll.
    fn place_cursor(&mut self, row: usize) -> bool {
        if row >= self.rows.len() {
            return false;
        }
        self.cursor = Some(row);
        self.vertical.scroll_to_reveal_item(row);
        true
    }

    /// Step the cursor by whole rows. The first move from nowhere lands on the
    /// top row going down and the bottom row going up, so either key starts
    /// reading without a click first.
    pub fn move_cursor(&mut self, delta: isize) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        let last = self.rows.len() - 1;
        let target = match self.cursor {
            Some(current) => current.saturating_add_signed(delta).min(last),
            None if delta >= 0 => 0,
            None => last,
        };
        if self.cursor == Some(target) {
            return false;
        }
        self.place_cursor(target)
    }

    /// Move to the next or previous row the predicate accepts — the hunk
    /// headers and the comment threads are both found this way. Stops at the
    /// last match rather than wrapping: wrapping past the end of a file reads
    /// as a jump to somewhere unrelated.
    pub fn jump(&mut self, forward: bool, accept: impl Fn(&DiffRow) -> bool) -> bool {
        let rows = self.rows.clone();
        let found = if forward {
            let from = self.cursor.map_or(0, |current| current + 1);
            rows.iter()
                .enumerate()
                .skip(from)
                .find(|(_, row)| accept(row))
                .map(|(index, _)| index)
        } else {
            let upto = self.cursor.unwrap_or(0);
            rows.iter()
                .enumerate()
                .take(upto)
                .rfind(|(_, row)| accept(row))
                .map(|(index, _)| index)
        };
        found.is_some_and(|row| self.place_cursor(row))
    }

    /// Both ends of the rows currently loaded.
    pub fn cursor_to_start(&mut self) -> bool {
        !self.rows.is_empty() && self.place_cursor(0)
    }

    pub fn cursor_to_end(&mut self) -> bool {
        let last = self.rows.len().checked_sub(1);
        last.is_some_and(|last| self.place_cursor(last))
    }

    /// Write both offsets back to the session, which holds them per file.
    pub fn capture_into(&self, session: &mut ReviewSession) {
        let vertical = self.vertical.scroll_px_offset_for_scrollbar().y.as_f32();
        let horizontal = (-self.horizontal.offset().x.as_f32()).max(0.);
        session.set_scroll_position(vertical);
        session.set_horizontal_scroll_position(horizontal);
    }
}

/// Per-row horizontal scroll, resolved without the pane.
#[derive(Clone)]
pub(crate) struct ScrollContexts {
    spans: Rc<Vec<FileSpan>>,
    fallback: (ScrollHandle, f32),
}

impl ScrollContexts {
    pub fn for_row(&self, row: usize) -> (&ScrollHandle, f32) {
        let found = self
            .spans
            .binary_search_by(|span| {
                if span.rows.end <= row {
                    std::cmp::Ordering::Less
                } else if span.rows.start > row {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()
            .and_then(|index| self.spans.get(index));
        match found {
            Some(span) => (&span.horizontal, span.text_width),
            None => (&self.fallback.0, self.fallback.1),
        }
    }
}
