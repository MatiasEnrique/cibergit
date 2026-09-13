use cibergit::{domain::ChangedFile, review::*};

#[test]
fn exact_whitespace_coordinates_markers_and_layouts() {
    let patch = "@@ -3,3 +3,4 @@ fn main()\n \t keep \r\n-old\n-\tlast  \n\\ No newline at end of file\n+new\n+extra\n+\tlast  \n\\ No newline at end of file\n";
    let parsed = parse_patch(patch);
    assert_eq!(parsed.status, PatchStatus::Complete);
    let hunk = &parsed.hunks[0];
    assert_eq!(hunk.header, "@@ -3,3 +3,4 @@ fn main()");
    let lines = hunk.unified_rows();
    assert_eq!(lines[0].text, "\t keep \r");
    assert_eq!((lines[0].old_line, lines[0].new_line), (Some(3), Some(3)));
    assert_eq!((lines[2].old_line, lines[2].new_line), (Some(5), None));
    assert_eq!(lines[2].text, "\tlast  ");
    assert_eq!(lines[3].kind, DiffLineKind::NoNewline);
    assert_eq!((lines[3].old_line, lines[3].new_line), (None, None));
    assert_eq!(lines[6].new_line, Some(6));
    let rows = hunk.aligned_rows();
    assert_eq!(rows[0].old.as_ref().unwrap().text, "\t keep \r");
    assert_eq!(rows[1].old.as_ref().unwrap().text, "old");
    assert_eq!(rows[1].new.as_ref().unwrap().text, "new");
    assert_eq!(
        rows.last().unwrap().new.as_ref().unwrap().kind,
        DiffLineKind::NoNewline
    );
    assert!(rows.last().unwrap().old.is_none());
}

#[test]
fn omitted_counts_zero_ranges_and_multiple_hunks() {
    let parsed = parse_patch("@@ -0,0 +1 @@\n+first\n@@ -5 +6 @@ name\n-last\n+final\n");
    assert!(parsed.is_complete());
    assert_eq!(parsed.hunks.len(), 2);
    assert_eq!(parsed.hunks[0].lines[0].new_line, Some(1));
    assert_eq!(parsed.hunks[1].lines[0].old_line, Some(5));
    let deleted = parse_patch("@@ -1 +0,0 @@\n-one\n");
    assert!(deleted.is_complete());
    assert!(deleted.hunks[0].aligned_rows()[0].new.is_none());
}

#[test]
fn truncation_and_unsupported_data_never_claim_completion() {
    for patch in [
        "@@ -1,2 +1,2 @@\n one\n",
        "@@ -1 +1 @@\n-old\n+new",
        "@@ -1,2 +1,2 @@\n one\n@@ -4 +4 @@\n four\n",
    ] {
        assert!(
            matches!(parse_patch(patch).status, PatchStatus::Truncated { .. }),
            "{patch:?}"
        );
    }
    for patch in [
        "@@@ -1 -1 +1 @@@\n",
        "GIT binary patch\n",
        "Binary files a/a and b/a differ\n",
        "@@ -1 +1 @@\n same\n+extra\n",
        "@@ -1 +1 @@\n\\ No newline at end of file\n",
        "@@ -18446744073709551615,2 +1 @@\n",
        "@@ -0 +1 @@\n",
        "not a patch\n",
    ] {
        assert!(
            matches!(parse_patch(patch).status, PatchStatus::Unsupported { .. }),
            "{patch:?}"
        );
    }
    let file = ChangedFile {
        raw_path: None,
        raw_previous_path: None,
        path: "a".into(),
        previous_path: None,
        status: "modified".into(),
        additions: 1,
        deletions: 1,
        patch: Some("@@ -1 +1 @@\n-old\n+new\n".into()),
        patch_complete: false,
    };
    assert!(matches!(
        parse_file(&file).status,
        PatchStatus::Truncated { .. }
    ));
    assert!(matches!(
        parse_file(&ChangedFile {
            patch: None,
            ..file
        })
        .status,
        PatchStatus::Unsupported { .. }
    ));
}

#[test]
fn context_eof_marker_is_rendered_on_both_sides() {
    let parsed = parse_patch("@@ -1 +1 @@\n same\n\\ No newline at end of file\n");
    let rows = parsed.hunks[0].aligned_rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].old.as_ref().unwrap().kind, DiffLineKind::NoNewline);
    assert_eq!(rows[1].new.as_ref().unwrap().kind, DiffLineKind::NoNewline);
}

#[test]
fn whole_missing_hunks_are_detected_against_file_statistics() {
    let file = ChangedFile {
        path: "partial".into(),
        raw_path: None,
        raw_previous_path: None,
        previous_path: None,
        status: "modified".into(),
        additions: 2,
        deletions: 1,
        patch: Some("@@ -1 +1 @@\n-old\n+new\n".into()),
        patch_complete: true,
    };
    assert!(matches!(
        parse_file(&file).status,
        PatchStatus::Truncated { .. }
    ));
    assert!(matches!(
        parse_file(&ChangedFile {
            patch: Some(String::new()),
            ..file
        })
        .status,
        PatchStatus::Truncated { .. }
    ));
}
