use cibergit::{domain::Revision, review::*};
use std::{fs, path::Path, process::Command};
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
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn git_input(path: &Path, args: &[&str], input: &[u8]) -> String {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn init() -> TempDir {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.name", "Lazy Review Test"]);
    git(
        dir.path(),
        &["config", "user.email", "lazy-review@example.invalid"],
    );
    dir
}

fn commit(path: &Path) -> String {
    git(path, &["add", "--all"]);
    git(path, &["commit", "-qm", "fixture", "--allow-empty"]);
    git(path, &["rev-parse", "HEAD"])
}

#[test]
fn inventory_omits_every_patch_and_selected_load_has_real_statistics() {
    let dir = init();
    let path = dir.path();
    for index in 0..40 {
        fs::write(path.join(format!("file-{index:02}.txt")), "old\n").unwrap();
    }
    let base = commit(path);
    for index in 0..40 {
        fs::write(
            path.join(format!("file-{index:02}.txt")),
            format!("new {index}\nextra\n"),
        )
        .unwrap();
    }
    fs::write(path.join("graphic.svg"), "<svg>must stay metadata</svg>\n").unwrap();
    fs::write(path.join("binary.bin"), b"\0private binary bytes\xff").unwrap();
    let head = commit(path);
    let revision = Revision {
        base_sha: base,
        head_sha: head,
    };

    let inventory = local_inventory(path, &revision).unwrap();
    assert_eq!(inventory.files.len(), 42);
    assert!(inventory.complete);
    assert!(inventory.files.iter().all(|file| {
        file.patch.is_none() && !file.patch_complete && file.additions == 0 && file.deletions == 0
    }));
    let notice = inventory.notice.as_deref().unwrap();
    assert!(notice.contains("not loaded"));
    assert!(notice.contains("diff statistics"));

    let selected = load_local_file(path, &revision, "file-17.txt", false).unwrap();
    assert_eq!((selected.additions, selected.deletions), (2, 1));
    assert!(selected.patch_complete);
    assert!(selected.patch.as_deref().unwrap().contains("+new 17\n"));
    assert!(
        inventory.files.iter().all(|file| file.patch.is_none()),
        "loading one file must not mutate or materialize the inventory"
    );

    for key in ["graphic.svg", "binary.bin"] {
        let metadata = load_local_file(path, &revision, key, false).unwrap();
        assert_eq!(metadata.path, key);
        assert!(metadata.patch.is_none());
        assert!(!metadata.patch_complete);
        assert_eq!((metadata.additions, metadata.deletions), (0, 0));
    }
}

#[test]
fn selected_load_is_pinned_when_head_and_worktree_advance() {
    let dir = init();
    let path = dir.path();
    fs::write(path.join("selected.rs"), "old\n").unwrap();
    let base = commit(path);
    fs::write(path.join("selected.rs"), "requested\nsecond\n").unwrap();
    let requested_head = commit(path);
    let revision = Revision {
        base_sha: base,
        head_sha: requested_head,
    };

    fs::write(path.join("selected.rs"), "newer commit\n").unwrap();
    commit(path);
    fs::write(path.join("selected.rs"), "uncommitted worktree\n").unwrap();

    let selected = load_local_file(path, &revision, "selected.rs", false).unwrap();
    let patch = selected.patch.as_deref().unwrap();
    assert!(patch.contains("+requested\n"));
    assert!(patch.contains("+second\n"));
    assert!(!patch.contains("newer commit"));
    assert!(!patch.contains("uncommitted worktree"));
    assert_eq!((selected.additions, selected.deletions), (2, 1));
    assert_eq!(
        fs::read_to_string(path.join("selected.rs")).unwrap(),
        "uncommitted worktree\n"
    );
}

#[test]
fn raw_file_keys_select_exact_tree_bytes() {
    let dir = init();
    let path = dir.path();
    let base = commit(path);
    let raw_name = b"invalid\xff.rs".to_vec();
    let raw_blob = git_input(path, &["hash-object", "-w", "--stdin"], b"raw selected\n");
    let display_blob = git_input(
        path,
        &["hash-object", "-w", "--stdin"],
        b"display selected\n",
    );
    let mut records = format!("100644 blob {raw_blob}\t").into_bytes();
    records.extend(&raw_name);
    records.push(0);
    records.extend(format!("100644 blob {display_blob}\tinvalid\\xff.rs\0").as_bytes());
    let tree = git_input(path, &["mktree", "-z"], &records);
    let head = git(path, &["commit-tree", &tree, "-p", &base, "-m", "raw tree"]);
    let revision = Revision {
        base_sha: base,
        head_sha: head,
    };

    let inventory = local_inventory(path, &revision).unwrap();
    let raw = inventory
        .files
        .iter()
        .find(|file| file.raw_path.is_some())
        .unwrap();
    let raw_key = file_key(raw);
    assert_ne!(raw_key, "invalid\\xff.rs");
    let loaded = load_local_file(path, &revision, &raw_key, false).unwrap();
    assert_eq!(loaded.raw_path.as_ref(), Some(&raw_name));
    assert!(loaded.patch.as_deref().unwrap().contains("+raw selected\n"));
    assert!(
        !loaded
            .patch
            .as_deref()
            .unwrap()
            .contains("display selected")
    );

    let display = load_local_file(path, &revision, "invalid\\xff.rs", false).unwrap();
    assert!(
        display
            .patch
            .as_deref()
            .unwrap()
            .contains("+display selected\n")
    );
}

#[test]
fn session_installs_only_matching_current_snapshot_file() {
    let dir = init();
    let path = dir.path();
    fs::write(path.join("one"), "old one\n").unwrap();
    fs::write(path.join("two"), "old two\n").unwrap();
    let base = commit(path);
    fs::write(path.join("one"), "new one\n").unwrap();
    fs::write(path.join("two"), "new two\n").unwrap();
    let head = commit(path);
    let revision = Revision {
        base_sha: base,
        head_sha: head,
    };
    let inventory = local_inventory(path, &revision).unwrap();
    let mut session = ReviewSession::new(inventory);
    assert!(session.select_file("two"));
    assert!(session.mark_viewed("one", true));
    let selected_before = file_key(session.selected_file().unwrap());
    let loaded = load_local_file(path, &revision, "one", false).unwrap();

    let stale = Revision {
        base_sha: "a".repeat(40),
        head_sha: "b".repeat(40),
    };
    assert!(session.install_file_patch(&stale, loaded.clone()).is_err());
    assert!(session.comparison().files[0].patch.is_none());

    let mut wrong_identity = loaded.clone();
    wrong_identity.status = "renamed".into();
    assert!(
        session
            .install_file_patch(&revision, wrong_identity)
            .is_err()
    );
    let mut absent = loaded.clone();
    absent.path = "absent".into();
    assert!(session.install_file_patch(&revision, absent).is_err());

    session.install_file_patch(&revision, loaded).unwrap();
    assert_eq!(session.revision(), &revision);
    assert_eq!(file_key(session.selected_file().unwrap()), selected_before);
    assert!(session.comparison().files[0].patch_complete);
    assert!(session.comparison().files[1].patch.is_none());
    assert!(!session.is_viewed("one"));
}

#[test]
fn full_pr_inventory_and_selected_load_share_merge_base_semantics() {
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

    let inventory = local_pr_inventory(path, &revision).unwrap();
    assert_eq!(inventory.revision, revision);
    assert_eq!(inventory.files.len(), 1);
    assert_eq!(inventory.files[0].path, "feature");
    assert!(inventory.files[0].patch.is_none());
    let notice = inventory.notice.as_deref().unwrap();
    assert!(notice.contains(&ancestor));
    assert!(notice.contains("not loaded"));

    let selected = load_local_file(path, &revision, "feature", true).unwrap();
    assert!(
        selected
            .patch
            .as_deref()
            .unwrap()
            .contains("+new feature\n")
    );
    assert_eq!((selected.additions, selected.deletions), (1, 1));
    assert!(
        load_local_file(path, &revision, "target-only", true).is_err(),
        "target-only changes are outside the full PR merge-base diff"
    );
}
