use cibergit::{domain::Revision, review::*};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};
use tempfile::TempDir;

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
fn init() -> TempDir {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.name", "Review Test"]);
    git(
        dir.path(),
        &["config", "user.email", "review@example.invalid"],
    );
    dir
}
fn commit(path: &Path) -> String {
    git(path, &["add", "--all"]);
    git(path, &["commit", "-qm", "fixture", "--allow-empty"]);
    git(path, &["rev-parse", "HEAD"])
}

#[test]
fn actual_git_diff_stays_pinned_after_heads_and_worktree_change() {
    let dir = init();
    let path = dir.path();
    fs::write(path.join("file.rs"), "fn old() {}\n").unwrap();
    let base = commit(path);
    fs::write(path.join("file.rs"), "fn new() {\n\t exact();  \n}\n").unwrap();
    let head = commit(path);
    let revision = Revision {
        base_sha: base,
        head_sha: head,
    };
    let before = local_comparison(path, &revision).unwrap();
    let patch = before.files[0].patch.as_ref().unwrap();
    assert!(patch.contains("+\t exact();  \n"));
    assert_eq!(
        (before.files[0].additions, before.files[0].deletions),
        (3, 1)
    );
    assert!(parse_file(&before.files[0]).is_complete());
    fs::write(path.join("file.rs"), "totally newer\n").unwrap();
    commit(path);
    fs::write(path.join("file.rs"), "uncommitted\n").unwrap();
    git(path, &["config", "diff.external", "false"]);
    let after = local_comparison(path, &revision).unwrap();
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(&after).unwrap()
    );
    assert_eq!(
        fs::read_to_string(path.join("file.rs")).unwrap(),
        "uncommitted\n"
    );
}

#[test]
fn all_paths_renames_binary_media_empty_and_no_newline_are_listed() {
    let dir = init();
    let path = dir.path();
    fs::write(path.join("original"), "rename content\n").unwrap();
    fs::write(path.join("deleted"), "delete me\n").unwrap();
    fs::write(path.join("no-newline"), "old").unwrap();
    fs::write(path.join("mode"), "same\n").unwrap();
    let base = commit(path);
    fs::rename(path.join("original"), path.join("renamed\n\t\"file")).unwrap();
    fs::remove_file(path.join("deleted")).unwrap();
    fs::write(path.join("no-newline"), "new").unwrap();
    fs::write(path.join("binary.bin"), b"\0\x01\xff").unwrap();
    // Even textual image content must never enter the patch model.
    fs::write(path.join("graphic.SVG"), "<svg>never load this</svg>\n").unwrap();
    fs::write(path.join("empty"), "").unwrap();
    fs::write(path.join(":(glob)*\tfile\n.rs"), "literal path\n").unwrap();
    fs::set_permissions(path.join("mode"), fs::Permissions::from_mode(0o755)).unwrap();
    let head = commit(path);
    let comparison = local_comparison(
        path,
        &Revision {
            base_sha: base,
            head_sha: head,
        },
    )
    .unwrap();
    assert_eq!(comparison.files.len(), 8);
    assert!(comparison.complete); // Metadata-only binary/media are intentional, not truncation.
    assert!(comparison.notice.is_some());
    let renamed = comparison
        .files
        .iter()
        .find(|f| f.status == "renamed")
        .unwrap();
    assert_eq!(renamed.path, "renamed\n\t\"file");
    assert_eq!(renamed.previous_path.as_deref(), Some("original"));
    assert!(renamed.patch_complete);
    for name in ["binary.bin", "graphic.SVG"] {
        let file = comparison.files.iter().find(|f| f.path == name).unwrap();
        assert!(file.patch.is_none());
        assert!(!file.patch_complete);
    }
    let no_newline = comparison
        .files
        .iter()
        .find(|f| f.path == "no-newline")
        .unwrap();
    let parsed = parse_file(no_newline);
    assert!(parsed.is_complete());
    assert_eq!(
        parsed.hunks[0]
            .lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::NoNewline)
            .count(),
        2
    );
    let mode = comparison.files.iter().find(|f| f.path == "mode").unwrap();
    assert!(
        mode.patch
            .as_ref()
            .unwrap()
            .contains("old mode 100644\nnew mode 100755\n")
    );
    assert!(
        comparison
            .files
            .iter()
            .find(|f| f.path == ":(glob)*\tfile\n.rs")
            .unwrap()
            .patch
            .as_ref()
            .unwrap()
            .contains("+literal path\n")
    );
}

#[test]
fn reused_rename_source_cannot_mix_two_file_patches() {
    let dir = init();
    let path = dir.path();
    fs::write(path.join("old"), "unchanged rename\n").unwrap();
    let base = commit(path);
    fs::rename(path.join("old"), path.join("new")).unwrap();
    fs::write(path.join("old"), "replacement source\n").unwrap();
    let head = commit(path);
    let comparison = local_comparison(
        path,
        &Revision {
            base_sha: base,
            head_sha: head,
        },
    )
    .unwrap();
    assert_eq!(comparison.files.len(), 2);
    for file in &comparison.files {
        assert!(parse_file(file).is_complete(), "{}", file.path);
    }
    assert!(
        comparison
            .files
            .iter()
            .find(|f| f.path == "new")
            .unwrap()
            .patch
            .as_ref()
            .unwrap()
            .lines()
            .all(|l| !l.contains("replacement source"))
    );
}

#[test]
fn full_object_ids_only_and_missing_objects_are_errors() {
    let dir = init();
    let head = commit(dir.path());
    for bad in [
        "HEAD",
        "--output=/tmp/no",
        "abc123",
        "$(touch nope)",
        "gggggggggggggggggggggggggggggggggggggggg",
    ] {
        assert!(
            local_comparison(
                dir.path(),
                &Revision {
                    base_sha: bad.into(),
                    head_sha: head.clone()
                }
            )
            .is_err()
        );
    }
    let missing = "f".repeat(40);
    assert!(
        local_comparison(
            dir.path(),
            &Revision {
                base_sha: head.clone(),
                head_sha: missing.clone()
            }
        )
        .is_err()
    );
    let full = Revision {
        base_sha: head.clone(),
        head_sha: head.clone(),
    };
    let (fallback, metadata) = local_since_last_review(dir.path(), &full, &missing).unwrap();
    assert_eq!(fallback.revision, full);
    assert_eq!(metadata.mode, ComparisonMode::FullPullRequest);
    assert!(metadata.notice.unwrap().contains("unavailable"));
    assert!(metadata.requested_mode.is_some());
    let (_, metadata) = local_since_last_review(dir.path(), &full, &head).unwrap();
    assert!(matches!(
        metadata.mode,
        ComparisonMode::SinceLastReview { .. }
    ));
}

#[test]
fn non_utf8_paths_retain_bytes_and_do_not_collide_with_display_strings() {
    let dir = init();
    let path = dir.path();
    let base = commit(path);
    let raw_name = b"invalid\xff.rs".to_vec();
    // APFS rejects non-UTF8 filenames, but Git trees can contain them.
    let blob = git_input(
        path,
        &["hash-object", "-w", "--stdin"],
        b"invalid name content\n",
    );
    let other = git_input(
        path,
        &["hash-object", "-w", "--stdin"],
        b"different literal name\n",
    );
    let mut records = format!("100644 blob {blob}\t").into_bytes();
    records.extend(&raw_name);
    records.push(0);
    records.extend(format!("100644 blob {other}\tinvalid\\xff.rs\0").as_bytes());
    let tree = git_input(path, &["mktree", "-z"], &records);
    let head = git(
        path,
        &["commit-tree", &tree, "-p", &base, "-m", "raw names"],
    );
    let comparison = local_comparison(
        path,
        &Revision {
            base_sha: base,
            head_sha: head,
        },
    )
    .unwrap();
    assert_eq!(comparison.files.len(), 2);
    let raw = comparison
        .files
        .iter()
        .find(|f| f.raw_path.is_some())
        .unwrap();
    assert_eq!(raw.raw_path.as_ref(), Some(&raw_name));
    assert_eq!(raw.path, "invalid\\xff.rs");
    assert!(
        raw.patch
            .as_ref()
            .unwrap()
            .contains("+invalid name content\n")
    );
    let key = file_key(raw);
    let ordinary = comparison
        .files
        .iter()
        .find(|f| f.raw_path.is_none())
        .unwrap();
    assert_ne!(key, file_key(ordinary));
    let mut session = ReviewSession::new(comparison);
    assert!(session.select_file(&key));
    assert_eq!(
        session.selected_file().unwrap().raw_path.as_ref(),
        Some(&raw_name)
    );
    assert!(session.mark_viewed(&key, true));
    assert!(!session.is_viewed("invalid\\xff.rs"));
}

#[test]
fn unsupported_text_encoding_is_explicit_and_symlinks_are_metadata_only() {
    let dir = init();
    let path = dir.path();
    let base = commit(path);
    fs::write(path.join("latin1.txt"), b"caf\xe9\n").unwrap();
    std::os::unix::fs::symlink("/private/not-read", path.join("link")).unwrap();
    let head = commit(path);
    let comparison = local_comparison(
        path,
        &Revision {
            base_sha: base,
            head_sha: head,
        },
    )
    .unwrap();
    assert_eq!(comparison.files.len(), 2);
    assert!(!comparison.complete);
    assert!(comparison.notice.is_some());
    assert!(
        comparison
            .files
            .iter()
            .all(|f| f.patch.is_none() && !f.patch_complete)
    );
}

fn git_input(path: &Path, args: &[&str], input: &[u8]) -> String {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("git")
        .args(args)
        .current_dir(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[test]
fn oversized_blobs_and_patch_output_keep_all_metadata() {
    let dir = init();
    let path = dir.path();
    fs::write(
        path.join("patch-limit"),
        "a".repeat(MAX_PATCH_BYTES / 2 + 100),
    )
    .unwrap();
    let base = commit(path);
    fs::write(
        path.join("patch-limit"),
        "b".repeat(MAX_PATCH_BYTES / 2 + 100),
    )
    .unwrap();
    fs::write(
        path.join("blob-limit"),
        "x".repeat(MAX_TEXT_BLOB_BYTES as usize + 1),
    )
    .unwrap();
    fs::write(path.join("ordinary"), "still visible\n").unwrap();
    let head = commit(path);
    let comparison = local_comparison(
        path,
        &Revision {
            base_sha: base,
            head_sha: head,
        },
    )
    .unwrap();
    assert_eq!(comparison.files.len(), 3);
    assert!(!comparison.complete);
    assert!(
        comparison
            .notice
            .as_ref()
            .unwrap()
            .contains("size/time limits")
    );
    for name in ["blob-limit", "patch-limit"] {
        let file = comparison.files.iter().find(|f| f.path == name).unwrap();
        assert!(file.patch.is_none());
        assert!(!file.patch_complete);
    }
    assert!(
        comparison
            .files
            .iter()
            .find(|f| f.path == "ordinary")
            .unwrap()
            .patch_complete
    );
}

// Run environment-sensitive checks in an isolated child test process; never mutate
// the parallel test runner's environment.
#[test]
fn ambient_probe() {
    let Some(path) = std::env::var_os("REVIEW_PROBE_PATH") else {
        return;
    };
    let revision = Revision {
        base_sha: std::env::var("REVIEW_PROBE_BASE").unwrap(),
        head_sha: std::env::var("REVIEW_PROBE_HEAD").unwrap(),
    };
    let result = local_comparison(Path::new(&path), &revision);
    if std::env::var_os("REVIEW_PROBE_TIMEOUT").is_some() {
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("time limit"), "{error}");
        assert!(!error.contains("PRIVATE"));
    } else {
        let comparison = result.unwrap();
        assert_eq!(comparison.files.len(), 1);
        assert_eq!(comparison.files[0].path, "right-repo");
    }
}

#[test]
fn ambient_git_variables_cannot_redirect_reader() {
    let right = init();
    let wrong = init();
    let base = commit(right.path());
    fs::write(right.path().join("right-repo"), "right content\n").unwrap();
    let head = commit(right.path());
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "ambient_probe", "--nocapture"])
        .env("REVIEW_PROBE_PATH", right.path())
        .env("REVIEW_PROBE_BASE", &base)
        .env("REVIEW_PROBE_HEAD", &head)
        .env("GIT_DIR", wrong.path().join(".git"))
        .env("GIT_WORK_TREE", wrong.path())
        .env("GIT_INDEX_FILE", wrong.path().join("wrong-index"))
        .env("GIT_OBJECT_DIRECTORY", wrong.path().join("missing-objects"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.bare")
        .env("GIT_CONFIG_VALUE_0", "true")
        .env("GIT_EXTERNAL_DIFF", "/bin/false")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!wrong.path().join("wrong-index").exists());
}

#[test]
fn git_deadline_kills_helpers_and_does_not_expose_stderr() {
    let dir = TempDir::new().unwrap();
    let script = dir.path().join("git");
    fs::write(&script, "#!/bin/sh\nprintf PRIVATE >&2\n/bin/sleep 30\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let start = std::time::Instant::now();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "ambient_probe", "--nocapture"])
        .env("REVIEW_PROBE_PATH", dir.path())
        .env("REVIEW_PROBE_BASE", "a".repeat(40))
        .env("REVIEW_PROBE_HEAD", "b".repeat(40))
        .env("REVIEW_PROBE_TIMEOUT", "1")
        .env("PATH", dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(start.elapsed().as_secs() < 10);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE"));
}

#[test]
fn full_pr_uses_merge_base_and_excludes_unrelated_target_changes() {
    let dir = init();
    let path = dir.path();
    fs::write(path.join("feature"), "old feature\n").unwrap();
    fs::write(path.join("target-only"), "old target\n").unwrap();
    let ancestor = commit(path);
    fs::write(path.join("feature"), "new feature\n").unwrap();
    let head = commit(path);
    git(path, &["checkout", "--detach", &ancestor]);
    fs::write(path.join("target-only"), "new target\n").unwrap();
    let base = commit(path);
    let revision = Revision {
        base_sha: base,
        head_sha: head,
    };
    assert_eq!(local_comparison(path, &revision).unwrap().files.len(), 2);
    let full = local_pr_comparison(path, &revision).unwrap();
    assert_eq!(full.revision, revision);
    assert_eq!(full.files.len(), 1);
    assert_eq!(full.files[0].path, "feature");
    assert!(full.notice.as_ref().unwrap().contains(&ancestor));
    assert!(
        full.files[0]
            .patch
            .as_ref()
            .unwrap()
            .contains("+new feature\n")
    );
    let (fallback, metadata) = local_since_last_review(path, &revision, &"f".repeat(40)).unwrap();
    assert_eq!(fallback.files.len(), 1);
    assert_eq!(fallback.revision, revision);
    assert_eq!(metadata.mode, ComparisonMode::FullPullRequest);
}

#[test]
fn non_utf8_rename_preserves_both_raw_paths() {
    let dir = init();
    let path = dir.path();
    let blob = git_input(path, &["hash-object", "-w", "--stdin"], b"rename me\n");
    let old_bytes = b"old\xff.rs";
    let new_bytes = b"new\xfe.rs";
    let make_tree = |name: &[u8]| {
        let mut record = format!("100644 blob {blob}\t").into_bytes();
        record.extend(name);
        record.push(0);
        git_input(path, &["mktree", "-z"], &record)
    };
    let base = git(path, &["commit-tree", &make_tree(old_bytes), "-m", "old"]);
    let head = git(
        path,
        &[
            "commit-tree",
            &make_tree(new_bytes),
            "-p",
            &base,
            "-m",
            "new",
        ],
    );
    let comparison = local_comparison(
        path,
        &Revision {
            base_sha: base,
            head_sha: head,
        },
    )
    .unwrap();
    assert_eq!(comparison.files.len(), 1);
    let renamed = &comparison.files[0];
    assert_eq!(renamed.status, "renamed");
    assert_eq!(renamed.raw_path.as_deref(), Some(new_bytes.as_slice()));
    assert_eq!(
        renamed.raw_previous_path.as_deref(),
        Some(old_bytes.as_slice())
    );
    assert!(parse_file(renamed).is_complete());
}
