//! Identity-safe presentation state for Git's immutable conflict stages.
//!
//! This module never owns or mutates the editable result. `LocalWorkspace`
//! continues to render the one `DocumentStore`-backed `EditorState`; this state
//! only binds immutable stage content to the exact Git operation that produced it.

use super::*;
use cibergit::rebase::{
    ActiveOperationIdentity, BlobContent, ConflictFile, ConflictStage, StashRestoreState,
};

pub(super) const WIDE_CONFLICT_PANE_MIN: f32 = 820.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConflictSource {
    Base,
    Ours,
    Theirs,
}

impl ConflictSource {
    pub(super) const ALL: [Self; 3] = [Self::Base, Self::Ours, Self::Theirs];

    pub(super) fn move_by(self, delta: isize) -> Self {
        let index = match self {
            Self::Base => 0,
            Self::Ours => 1,
            Self::Theirs => 2,
        };
        let next = (index as isize + delta).rem_euclid(Self::ALL.len() as isize) as usize;
        Self::ALL[next]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ConflictOperationIdentity {
    Rebase {
        operation_id: String,
        active: Box<ActiveOperationIdentity>,
    },
    StashRestore {
        operation_id: String,
    },
}

impl ConflictOperationIdentity {
    fn from_operation(operation: &RebaseOperationView) -> Option<Self> {
        if operation.stash_restore == StashRestoreState::Conflicted {
            Some(Self::StashRestore {
                operation_id: operation.operation_id.clone(),
            })
        } else {
            Some(Self::Rebase {
                operation_id: operation.operation_id.clone(),
                active: Box::new(operation.active.clone()?),
            })
        }
    }

    fn operation_id(&self) -> &str {
        match self {
            Self::Rebase { operation_id, .. } | Self::StashRestore { operation_id } => operation_id,
        }
    }

    fn is_stash_restore(&self) -> bool {
        matches!(self, Self::StashRestore { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConflictContextIdentity {
    operation: ConflictOperationIdentity,
    /// Includes raw path bytes, all stage OIDs/modes/content, and disk generation.
    conflict: ConflictFile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ConflictContextState {
    Current,
    StagesChanged,
    ConflictResolved,
    OperationChanged,
    OperationUnavailable,
}

impl ConflictContextState {
    pub(super) fn explanation(&self) -> Option<&'static str> {
        match self {
            Self::Current => None,
            Self::StagesChanged => Some(
                "Git's source stages or the saved result identity changed. The editable result was preserved, but this frozen context cannot be staged. Refresh the source context after reviewing the change.",
            ),
            Self::ConflictResolved => Some(
                "Git no longer reports this path as unmerged. The editable result was preserved; this frozen context cannot advance or stage anything.",
            ),
            Self::OperationChanged => Some(
                "The rebase or stash-restore identity changed. The editable result was preserved, but this presentation will not attach to the new operation even if the path is the same.",
            ),
            Self::OperationUnavailable => Some(
                "The observed operation ended or became unavailable. The editable result was preserved and this source context is now read-only evidence.",
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct ConflictPresentation {
    identity: ConflictContextIdentity,
    selected: ConflictSource,
    show_details: bool,
    state: ConflictContextState,
}

impl ConflictPresentation {
    pub(super) fn new(operation: &RebaseOperationView, conflict: ConflictFile) -> Option<Self> {
        Some(Self {
            identity: ConflictContextIdentity {
                operation: ConflictOperationIdentity::from_operation(operation)?,
                conflict,
            },
            selected: ConflictSource::Ours,
            show_details: false,
            state: ConflictContextState::Current,
        })
    }

    pub(super) fn conflict(&self) -> &ConflictFile {
        &self.identity.conflict
    }

    pub(super) fn path_raw(&self) -> &[u8] {
        &self.identity.conflict.path.raw
    }

    pub(super) fn operation_id(&self) -> &str {
        self.identity.operation.operation_id()
    }

    pub(super) fn is_stash_restore(&self) -> bool {
        self.identity.operation.is_stash_restore()
    }

    pub(super) fn selected(&self) -> ConflictSource {
        self.selected
    }

    pub(super) fn select(&mut self, source: ConflictSource) {
        self.selected = source;
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        self.selected = self.selected.move_by(delta);
    }

    pub(super) fn show_details(&self) -> bool {
        self.show_details
    }

    pub(super) fn toggle_details(&mut self) {
        self.show_details = !self.show_details;
    }

    pub(super) fn state(&self) -> &ConflictContextState {
        &self.state
    }

    pub(super) fn can_stage(&self) -> bool {
        self.state == ConflictContextState::Current
    }

    pub(super) fn can_refresh_from(
        &self,
        operation: &RebaseOperationView,
        conflict: &ConflictFile,
    ) -> bool {
        let Some(binding) = ConflictOperationIdentity::from_operation(operation) else {
            return false;
        };
        binding == self.identity.operation && conflict.path.raw == self.identity.conflict.path.raw
    }

    /// Rebinds only immutable source context. The result editor is deliberately
    /// absent from this API, making an accidental `set_value` impossible here.
    pub(super) fn refresh_from(
        &mut self,
        operation: &RebaseOperationView,
        conflict: ConflictFile,
    ) -> bool {
        if !self.can_refresh_from(operation, &conflict) {
            return false;
        }
        self.identity.conflict = conflict;
        self.state = ConflictContextState::Current;
        true
    }

    pub(super) fn observe(
        &mut self,
        operation: Option<&RebaseOperationView>,
        conflicts: &[ConflictFile],
    ) {
        let Some(operation) = operation else {
            self.state = ConflictContextState::OperationUnavailable;
            return;
        };
        let Some(binding) = ConflictOperationIdentity::from_operation(operation) else {
            self.state = ConflictContextState::OperationUnavailable;
            return;
        };
        if binding != self.identity.operation {
            self.state = ConflictContextState::OperationChanged;
            return;
        }
        let Some(current) = conflicts
            .iter()
            .find(|conflict| conflict.path.raw == self.identity.conflict.path.raw)
        else {
            self.state = ConflictContextState::ConflictResolved;
            return;
        };
        self.state = if current == &self.identity.conflict {
            ConflictContextState::Current
        } else {
            ConflictContextState::StagesChanged
        };
    }

    pub(super) fn source(&self, source: ConflictSource) -> (&'static str, &ConflictStage) {
        match source {
            ConflictSource::Base => ("Base", &self.identity.conflict.base),
            ConflictSource::Ours if self.is_stash_restore() => {
                ("Current worktree (ours)", &self.identity.conflict.ours)
            }
            ConflictSource::Ours => ("Already rebased (ours)", &self.identity.conflict.ours),
            ConflictSource::Theirs if self.is_stash_restore() => {
                ("Restored stash (theirs)", &self.identity.conflict.theirs)
            }
            ConflictSource::Theirs => ("Replayed commit (theirs)", &self.identity.conflict.theirs),
        }
    }
}

pub(super) fn source_summary(stage: &ConflictStage) -> String {
    let oid = stage
        .oid
        .as_deref()
        .map(short_conflict_oid)
        .unwrap_or_else(|| "missing".into());
    let mode = stage.mode.as_deref().unwrap_or("no mode");
    format!("{oid} · {mode} · {}", content_kind(&stage.content))
}

pub(super) fn source_details(stage: &ConflictStage) -> String {
    format!(
        "object {}\nmode {}\ncontent {}",
        stage.oid.as_deref().unwrap_or("missing stage"),
        stage.mode.as_deref().unwrap_or("missing stage"),
        content_kind(&stage.content)
    )
}

pub(super) fn source_text(stage: &ConflictStage) -> String {
    match &stage.content {
        BlobContent::Utf8(text) => text.clone(),
        BlobContent::Deleted => "<missing stage — this side deleted the path>".into(),
        BlobContent::Binary => "<binary Git object — read-only metadata only>".into(),
        BlobContent::Media => "<media Git object — no decoded preview>".into(),
        BlobContent::Symlink(target) => format!(
            "<symbolic-link target bytes: {}>",
            String::from_utf8_lossy(target)
        ),
        BlobContent::NonUtf8 => "<non-UTF-8 Git object — read-only metadata only>".into(),
        BlobContent::TooLarge { bytes, limit } => {
            format!("<Git object is {bytes} bytes; internal text limit is {limit} bytes>")
        }
    }
}

pub(super) fn source_panel(
    source: ConflictSource,
    label: &'static str,
    stage: &ConflictStage,
    selected: bool,
    show_details: bool,
    colors: LocalPalette,
) -> Div {
    let source_id = match source {
        ConflictSource::Base => "base",
        ConflictSource::Ours => "ours",
        ConflictSource::Theirs => "theirs",
    };
    let lines = source_text(stage)
        .split('\n')
        .enumerate()
        .map(|(index, line)| {
            div()
                .id(ElementId::Name(
                    format!("conflict-source-{source_id}-line-{index}").into(),
                ))
                .whitespace_nowrap()
                .child(if line.is_empty() {
                    " ".into()
                } else {
                    line.to_owned()
                })
        })
        .collect::<Vec<_>>();
    div()
        .min_w_0()
        .h(px(220.))
        .flex()
        .flex_col()
        .rounded_md()
        .border_1()
        .border_color(if selected {
            colors.accent
        } else {
            colors.border
        })
        .bg(colors.surface)
        .child(
            div()
                .min_h(px(48.))
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(colors.border)
                .child(div().font_weight(FontWeight::SEMIBOLD).child(label))
                .child(
                    div()
                        .font_family(CODE_FONT)
                        .text_xs()
                        .text_color(colors.muted)
                        .child(source_summary(stage)),
                ),
        )
        .when(show_details, |panel| {
            panel.child(
                div()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(colors.border)
                    .font_family(CODE_FONT)
                    .text_xs()
                    .text_color(colors.muted)
                    .child(source_details(stage)),
            )
        })
        .child(
            div()
                .id(ElementId::Name(
                    format!("conflict-source-{source_id}-scroll").into(),
                ))
                .flex_1()
                .min_h_0()
                .overflow_x_scroll()
                .overflow_y_scroll()
                .p_2()
                .font_family(CODE_FONT)
                .text_xs()
                .children(lines),
        )
}

fn content_kind(content: &BlobContent) -> String {
    match content {
        BlobContent::Utf8(text) => format!("UTF-8 · {} bytes", text.len()),
        BlobContent::Binary => "binary".into(),
        BlobContent::Media => "media".into(),
        BlobContent::Symlink(target) => format!("symlink · {} target bytes", target.len()),
        BlobContent::NonUtf8 => "non-UTF-8".into(),
        BlobContent::TooLarge { bytes, limit } => {
            format!("too large · {bytes} bytes (limit {limit})")
        }
        BlobContent::Deleted => "deleted / missing stage".into(),
    }
}

fn short_conflict_oid(oid: &str) -> String {
    oid.chars().take(10).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::{
        local_git::GitPath,
        rebase::{ConflictKind, DiskGeneration, OperationState},
    };

    fn oid(value: char) -> String {
        std::iter::repeat_n(value, 40).collect()
    }

    fn active(operation_id: &str, marker: char) -> ActiveOperationIdentity {
        ActiveOperationIdentity {
            operation_id: operation_id.into(),
            head_name: "refs/heads/topic".into(),
            onto_oid: oid('0'),
            original_head_oid: oid('9'),
            stopped_oid: Some(oid(marker)),
            rebase_head_oid: Some(oid(marker)),
            stop_is_edit: false,
            todo_sha256: oid('7'),
            done_sha256: oid('8'),
            ownership_marker_sha256: oid('6'),
        }
    }

    fn operation(operation_id: &str, marker: char) -> RebaseOperationView {
        RebaseOperationView {
            operation_id: operation_id.into(),
            attempt: 1,
            state: OperationState::Conflicted,
            original_branch: "refs/heads/topic".into(),
            original_head_oid: oid('9'),
            base_oid: oid('0'),
            active: Some(active(operation_id, marker)),
            resulting_commits: Vec::new(),
            stash: None,
            stash_restore: StashRestoreState::NotStarted,
            split: None,
            evidence: Vec::new(),
            publish_warning: None,
            publish_handoff: None,
        }
    }

    fn stage(marker: char, text: &str) -> ConflictStage {
        ConflictStage {
            oid: Some(oid(marker)),
            mode: Some("100644".into()),
            content: BlobContent::Utf8(text.into()),
        }
    }

    fn conflict(path: &[u8], base: char, ours: char, theirs: char) -> ConflictFile {
        ConflictFile {
            path: GitPath::from_raw(path.to_vec()).unwrap(),
            kind: ConflictKind::BothModified,
            base: stage(base, "base-token\nbase-long-line-END"),
            ours: stage(ours, "ours-token\nours-long-line-END"),
            theirs: stage(theirs, "theirs-token\ntheirs-long-line-END"),
            disk: DiskGeneration::Regular {
                sha256: oid('d'),
                len: 11,
                device: 1,
                inode: 2,
                modified_seconds: 3,
                modified_nanoseconds: 4,
            },
        }
    }

    #[test]
    fn source_labels_content_and_oids_never_swap() {
        let operation = operation("op-a", '5');
        let conflict = conflict(b"src/raw.txt", '1', '2', '3');
        let view = ConflictPresentation::new(&operation, conflict).unwrap();

        let (base_label, base) = view.source(ConflictSource::Base);
        let (ours_label, ours) = view.source(ConflictSource::Ours);
        let (theirs_label, theirs) = view.source(ConflictSource::Theirs);
        assert_eq!(base_label, "Base");
        assert_eq!(ours_label, "Already rebased (ours)");
        assert_eq!(theirs_label, "Replayed commit (theirs)");
        assert_eq!(base.oid.as_deref(), Some(oid('1').as_str()));
        assert_eq!(ours.oid.as_deref(), Some(oid('2').as_str()));
        assert_eq!(theirs.oid.as_deref(), Some(oid('3').as_str()));
        assert!(source_text(base).contains("base-long-line-END"));
        assert!(source_text(ours).contains("ours-long-line-END"));
        assert!(source_text(theirs).contains("theirs-long-line-END"));
    }

    #[test]
    fn stage_replacement_invalidates_until_explicit_refresh() {
        let operation = operation("op-a", '5');
        let original = conflict(b"same.txt", '1', '2', '3');
        let replacement = conflict(b"same.txt", '1', '2', '4');
        let mut view = ConflictPresentation::new(&operation, original).unwrap();

        view.observe(Some(&operation), std::slice::from_ref(&replacement));
        assert_eq!(view.state(), &ConflictContextState::StagesChanged);
        assert!(!view.can_stage());
        assert_eq!(view.source(ConflictSource::Theirs).1.oid, Some(oid('3')));

        assert!(view.refresh_from(&operation, replacement));
        assert!(view.can_stage());
        assert_eq!(view.source(ConflictSource::Theirs).1.oid, Some(oid('4')));
    }

    #[test]
    fn same_path_new_operation_never_reattaches_old_context() {
        let first = operation("op-a", '5');
        let next = operation("op-b", '5');
        let conflict = conflict(b"same.txt", '1', '2', '3');
        let mut view = ConflictPresentation::new(&first, conflict.clone()).unwrap();

        view.observe(Some(&next), std::slice::from_ref(&conflict));
        assert_eq!(view.state(), &ConflictContextState::OperationChanged);
        assert!(!view.can_stage());
        assert!(!view.refresh_from(&next, conflict));
    }

    #[test]
    fn missing_stage_is_not_rendered_as_empty_text() {
        let stage = ConflictStage {
            oid: None,
            mode: None,
            content: BlobContent::Deleted,
        };
        assert!(source_text(&stage).contains("missing stage"));
        assert!(source_summary(&stage).contains("deleted"));
    }

    #[test]
    fn selection_wraps_for_keyboard_navigation() {
        assert_eq!(ConflictSource::Base.move_by(-1), ConflictSource::Theirs);
        assert_eq!(ConflictSource::Theirs.move_by(1), ConflictSource::Base);
    }
}
