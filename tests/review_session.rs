use cibergit::{
    domain::{ChangedFile, Comparison, Revision},
    review::*,
};

fn revision(base: char, head: char) -> Revision {
    Revision {
        base_sha: base.to_string().repeat(40),
        head_sha: head.to_string().repeat(40),
    }
}
fn file(path: &str, text: &str) -> ChangedFile {
    ChangedFile {
        raw_path: None,
        raw_previous_path: None,
        path: path.into(),
        previous_path: None,
        status: "modified".into(),
        additions: 1,
        deletions: 1,
        patch: Some(format!("@@ -1 +1 @@\n-old\n+{text}\n")),
        patch_complete: true,
    }
}
fn comparison(revision: Revision, files: Vec<ChangedFile>) -> Comparison {
    Comparison {
        revision,
        files,
        complete: true,
        notice: None,
    }
}

#[test]
fn incoming_commits_do_not_move_code_and_advance_rejects_stale_load() {
    let initial = comparison(revision('a', 'b'), vec![file("a", "one")]);
    let mut tab = ReviewSession::new(initial.clone());
    tab.observe_revision(revision('a', 'c'));
    assert_eq!(tab.revision(), &initial.revision);
    assert_eq!(tab.submission_revision(), &initial.revision);
    assert!(tab.requires_advance_before_merge());
    let stale = comparison(revision('a', 'c'), vec![file("a", "two")]);
    tab.observe_revision(revision('a', 'd'));
    assert!(tab.advance(stale).is_err());
    assert_eq!(tab.revision(), &initial.revision);
    tab.advance(comparison(revision('a', 'd'), vec![file("a", "three")]))
        .unwrap();
    assert_eq!(tab.revision(), &revision('a', 'd'));
    assert!(tab.available_revision().is_none());
    assert!(!tab.requires_advance_before_merge());
}

#[test]
fn viewed_marks_carry_only_for_unchanged_complete_files() {
    let mut binary = file("blob", "");
    binary.patch = None;
    binary.patch_complete = false;
    let files = vec![
        file("same", "one"),
        file("edit", "two"),
        file("gone", "gone"),
        binary.clone(),
    ];
    let mut tab = ReviewSession::new(comparison(revision('a', 'b'), files));
    for path in ["same", "edit", "gone", "blob"] {
        assert!(tab.mark_viewed(path, true));
    }
    let original_mark = tab.viewed_file("same").unwrap().clone();
    tab.select_file("edit");
    tab.set_scroll_position(234.5);
    tab.set_diff_mode(DiffMode::Unified);
    tab.observe_revision(revision('a', 'c'));
    tab.advance(comparison(
        revision('a', 'c'),
        vec![file("same", "one"), file("edit", "changed"), binary],
    ))
    .unwrap();
    assert!(tab.is_viewed("same"));
    assert_eq!(
        tab.viewed_file("same").unwrap().revision,
        original_mark.revision
    );
    for path in ["edit", "gone", "blob"] {
        assert!(!tab.is_viewed(path));
    }
    assert_eq!(tab.selected_file().unwrap().path, "edit");
    assert_eq!(tab.scroll_position(), 234.5);
    assert_eq!(tab.diff_mode().resolve(true), DiffMode::Unified);
    assert!(tab.mark_viewed("same", false));
    assert!(!tab.is_viewed("same"));
    assert!(!tab.mark_viewed("missing", true));
}

#[test]
fn separate_tabs_navigation_and_serialized_preferences() {
    let snapshot = comparison(
        revision('a', 'b'),
        vec![file("one", "one"), file("two", "two")],
    );
    let mut first = ReviewSession::new(snapshot.clone());
    let second = ReviewSession::new(snapshot);
    assert!(!first.previous_file());
    assert!(first.next_file());
    assert!(!first.next_file());
    first.set_scroll_position(52.0);
    first.set_diff_mode(DiffMode::SideBySide);
    first.mark_viewed("two", true);
    first.observe_revision(revision('a', 'c'));
    assert_eq!(second.selected_file().unwrap().path, "one");
    assert!(!second.is_viewed("two"));
    assert!(second.available_revision().is_none());
    assert_eq!(second.scroll_position(), 0.0);
    let restored: ReviewSession =
        serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
    assert_eq!(restored.selected_file().unwrap().path, "two");
    assert_eq!(restored.scroll_position(), 52.0);
    assert_eq!(restored.diff_mode().resolve(false), DiffMode::SideBySide);
    assert!(restored.is_viewed("two"));
    assert_eq!(DiffMode::Auto.resolve(true), DiffMode::SideBySide);
    assert_eq!(DiffMode::Auto.resolve(false), DiffMode::Unified);
}

#[test]
fn comparison_selection_and_empty_file_lists() {
    let mut tab = ReviewSession::new(comparison(revision('a', 'b'), vec![]));
    assert!(tab.selected_file().is_none());
    assert!(!tab.next_file());
    assert!(!tab.previous_file());
    let metadata = ComparisonMetadata {
        mode: ComparisonMode::CommitRange,
        requested_mode: None,
        notice: None,
    };
    tab.select_comparison(
        comparison(revision('b', 'c'), vec![file("new", "new")]),
        metadata.clone(),
    );
    assert_eq!(tab.metadata(), &metadata);
    assert_eq!(tab.selected_file().unwrap().path, "new");
    assert!(!tab.select_file("missing"));
    assert_eq!(tab.selected_file().unwrap().path, "new");
}

#[test]
fn horizontal_scroll_is_file_scoped_and_old_records_restore_at_start() {
    let mut session = ReviewSession::new(comparison(
        revision('a', 'b'),
        vec![file("one", "1"), file("two", "2")],
    ));
    session.set_horizontal_scroll_position(120.5);
    session.select_file("two");
    assert_eq!(session.horizontal_scroll_position(), 0.);
    session.set_horizontal_scroll_position(640.);
    session.set_horizontal_scroll_position(f32::NAN);
    session.set_horizontal_scroll_position(-1.);
    assert_eq!(session.horizontal_scroll_position(), 640.);
    session.select_file("one");
    let encoded = serde_json::to_value(&session).unwrap();
    let restored: ReviewSession = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(restored.horizontal_scroll_position(), 120.5);
    let mut legacy = encoded;
    legacy
        .as_object_mut()
        .unwrap()
        .remove("horizontal_scroll_positions");
    let old: ReviewSession = serde_json::from_value(legacy).unwrap();
    assert_eq!(old.horizontal_scroll_position(), 0.);
}
