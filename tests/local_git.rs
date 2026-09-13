use cibergit::local_git::*;
use std::{
    ffi::OsStr,
    fs,
    io::Write,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::Path,
    process::{Command, Output, Stdio},
    time::Duration,
};
use tempfile::TempDir;

fn git_output(path: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap()
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = git_output(path, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git_input(path: &Path, args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn init() -> TempDir {
    let directory = TempDir::new().unwrap();
    git(directory.path(), &["init", "-q", "-b", "main"]);
    configure_identity(directory.path());
    directory
}

fn configure_identity(path: &Path) {
    git(path, &["config", "user.name", "Local Git Test"]);
    git(path, &["config", "user.email", "local-git@example.invalid"]);
}

fn commit_all(path: &Path, message: &str) -> String {
    git(path, &["add", "--all"]);
    git(path, &["commit", "-qm", message, "--allow-empty"]);
    git(path, &["rev-parse", "HEAD"])
}

fn path(raw: &[u8]) -> GitPath {
    GitPath::from_raw(raw.to_vec()).unwrap()
}

#[test]
fn discovers_unborn_checkout_and_reports_raw_status_and_selected_diffs() {
    let directory = init();
    let backend = LocalGit::open(directory.path().join(".")).unwrap();
    let initial = backend.snapshot().unwrap();
    assert!(matches!(
        initial.head,
        HeadState::Unborn { ref branch } if branch == "main"
    ));
    assert_eq!(backend.root(), directory.path().canonicalize().unwrap());
    assert_eq!(backend.git_dir(), backend.common_git_dir());

    let odd = b":(glob)* tab\tline\n.txt";
    fs::write(directory.path().join(OsStr::from_bytes(odd)), "first\n").unwrap();
    let snapshot = backend.snapshot().unwrap();
    assert_eq!(snapshot.untracked[0].raw, odd);
    assert_eq!(snapshot.untracked[0].display, ":(glob)* tab\tline\n.txt");
    let diff = backend
        .selected_diff(&snapshot.untracked[0], DiffTarget::Worktree)
        .unwrap();
    assert!(matches!(diff.content, DiffContent::Text(ref patch) if patch.contains("+first")));

    let receipt = backend.stage(&[path(odd)], &snapshot.guard).unwrap();
    assert_eq!(receipt.action, MutationAction::Stage);
    assert!(receipt.refresh_required);
    let staged = backend.snapshot().unwrap();
    assert_eq!(staged.staged[0].path.raw, odd);
    let staged_diff = backend
        .selected_diff(&path(odd), DiffTarget::Staged)
        .unwrap();
    assert!(
        matches!(staged_diff.content, DiffContent::Text(ref patch) if patch.contains("+first"))
    );

    backend.unstage(&[path(odd)], &staged.guard).unwrap();
    let unstaged = backend.snapshot().unwrap();
    assert!(unstaged.staged.is_empty());
    assert_eq!(unstaged.untracked[0].raw, odd);

    fs::write(directory.path().join("image.png"), b"not decoded").unwrap();
    fs::write(directory.path().join("binary.dat"), b"\0\xff").unwrap();
    assert!(matches!(
        backend
            .selected_diff(&path(b"image.png"), DiffTarget::Worktree)
            .unwrap()
            .content,
        DiffContent::MediaMetadata
    ));
    assert!(matches!(
        backend
            .selected_diff(&path(b"binary.dat"), DiffTarget::Worktree)
            .unwrap()
            .content,
        DiffContent::BinaryMetadata
    ));
}

#[test]
fn stage_unstage_and_commit_preserve_unrelated_changes_and_rename_paths() {
    let directory = init();
    let root = directory.path();
    fs::write(root.join("a.txt"), "base a\n").unwrap();
    fs::write(root.join("b.txt"), "base b\n").unwrap();
    fs::write(root.join("rename-me"), "rename\n").unwrap();
    commit_all(root, "base");
    fs::write(root.join("a.txt"), "changed a\n").unwrap();
    fs::write(root.join("b.txt"), "changed b\n").unwrap();
    fs::rename(root.join("rename-me"), root.join("renamed\t\nfile")).unwrap();

    let backend = LocalGit::open(root).unwrap();
    let before = backend.snapshot().unwrap();
    backend.stage(&[path(b"a.txt")], &before.guard).unwrap();
    let after_stage = backend.snapshot().unwrap();
    assert!(
        after_stage
            .staged
            .iter()
            .any(|entry| entry.path.raw == b"a.txt")
    );
    assert!(
        after_stage
            .unstaged
            .iter()
            .any(|entry| entry.path.raw == b"b.txt")
    );
    assert!(
        after_stage
            .unstaged
            .iter()
            .any(|entry| entry.path.raw == b"rename-me")
    );

    backend
        .unstage(&[path(b"a.txt")], &after_stage.guard)
        .unwrap();
    let after_unstage = backend.snapshot().unwrap();
    assert!(after_unstage.staged.is_empty());
    backend
        .stage(&[path(b"a.txt")], &after_unstage.guard)
        .unwrap();
    let ready = backend.snapshot().unwrap();
    backend.commit("only a", &ready.guard).unwrap();
    let after_commit = backend.snapshot().unwrap();
    assert!(after_commit.staged.is_empty());
    assert!(
        after_commit
            .unstaged
            .iter()
            .any(|entry| entry.path.raw == b"b.txt")
    );
    assert!(
        after_commit
            .unstaged
            .iter()
            .any(|entry| entry.path.raw == b"rename-me")
    );

    let rename_guard = after_commit.guard.clone();
    backend
        .stage(
            &[path(b"rename-me"), path(b"renamed\t\nfile")],
            &rename_guard,
        )
        .unwrap();
    let renamed = backend.snapshot().unwrap();
    let entry = renamed
        .staged
        .iter()
        .find(|entry| entry.previous_path.is_some())
        .unwrap();
    assert_eq!(entry.path.raw, b"renamed\t\nfile");
    assert_eq!(entry.previous_path.as_ref().unwrap().raw, b"rename-me");
}

#[test]
fn linked_worktrees_share_common_directory_and_stale_guards_reject_external_changes() {
    let directory = init();
    fs::write(directory.path().join("file"), "base\n").unwrap();
    commit_all(directory.path(), "base");
    git(directory.path(), &["branch", "linked"]);
    let linked_parent = TempDir::new().unwrap();
    let linked = linked_parent.path().join("worktree");
    git(
        directory.path(),
        &["worktree", "add", "-q", linked.to_str().unwrap(), "linked"],
    );
    let main = LocalGit::open(directory.path()).unwrap();
    let other = LocalGit::open(&linked).unwrap();
    assert_ne!(main.git_dir(), other.git_dir());
    assert_eq!(main.common_git_dir(), other.common_git_dir());

    fs::write(directory.path().join("external"), "one\n").unwrap();
    fs::write(directory.path().join("wanted"), "two\n").unwrap();
    let displayed = main.snapshot().unwrap();
    git(directory.path(), &["add", "external"]);
    let error = main
        .stage(&[path(b"wanted")], &displayed.guard)
        .unwrap_err();
    assert!(matches!(error, LocalGitError::StaleSnapshot));
    let actual = main.snapshot().unwrap();
    assert!(
        actual
            .staged
            .iter()
            .any(|entry| entry.path.raw == b"external")
    );
    assert!(actual.untracked.iter().any(|entry| entry.raw == b"wanted"));

    let before_ref_change = other.snapshot().unwrap();
    git(directory.path(), &["branch", "external-ref"]);
    let error = other
        .stage(&[path(b"missing")], &before_ref_change.guard)
        .unwrap_err();
    assert!(matches!(error, LocalGitError::StaleSnapshot));
}

#[test]
fn instances_for_one_common_git_directory_serialize_incompatible_writes() {
    let directory = init();
    let root = directory.path();
    fs::write(root.join("tracked"), "base\n").unwrap();
    commit_all(root, "base");
    fs::write(root.join("tracked"), "staged\n").unwrap();
    fs::write(root.join("later"), "later\n").unwrap();
    git(root, &["add", "tracked"]);
    let hook = root.join(".git/hooks/pre-commit");
    fs::write(&hook, "#!/bin/sh\nsleep 0.3\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let committing = LocalGit::open(root).unwrap();
    let staging = LocalGit::open(root).unwrap();
    let guard = committing.snapshot().unwrap().guard;
    let commit_guard = guard.clone();
    let thread = std::thread::spawn(move || committing.commit("serialized", &commit_guard));
    std::thread::sleep(Duration::from_millis(50));
    let started = std::time::Instant::now();
    let stage_error = staging.stage(&[path(b"later")], &guard).unwrap_err();
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert!(matches!(stage_error, LocalGitError::StaleSnapshot));
    assert!(thread.join().unwrap().is_ok());
    assert!(
        staging
            .snapshot()
            .unwrap()
            .untracked
            .iter()
            .any(|entry| entry.raw == b"later")
    );
}

#[test]
fn git_index_lock_failure_is_reported_without_removing_the_lock() {
    let directory = init();
    fs::write(directory.path().join("file"), "base\n").unwrap();
    commit_all(directory.path(), "base");
    fs::write(directory.path().join("file"), "changed\n").unwrap();
    let backend = LocalGit::open(directory.path()).unwrap();
    let snapshot = backend.snapshot().unwrap();
    let lock = backend.git_dir().join("index.lock");
    fs::write(&lock, "external lock").unwrap();
    let error = backend
        .stage(&[path(b"file")], &snapshot.guard)
        .unwrap_err();
    assert!(matches!(
        error,
        LocalGitError::RepositoryLocked {
            action: "stage paths",
        }
    ));
    assert!(lock.exists());
    assert!(backend.snapshot().unwrap().staged.is_empty());
}

#[test]
fn branch_actions_use_real_git_dirty_refusal_and_operation_state() {
    let directory = init();
    let root = directory.path();
    fs::write(root.join("file"), "base\n").unwrap();
    let base = commit_all(root, "base");
    let backend = LocalGit::open(root).unwrap();
    let snapshot = backend.snapshot().unwrap();
    backend
        .create_branch("other", Some(&base), &snapshot.guard)
        .unwrap();
    fs::write(root.join("file"), "other\n").unwrap();
    commit_all(root, "other");
    let snapshot = backend.snapshot().unwrap();
    backend.switch_branch("main", &snapshot.guard).unwrap();
    fs::write(root.join("file"), "dirty main\n").unwrap();
    let dirty = backend.snapshot().unwrap();
    let error = backend.switch_branch("other", &dirty.guard).unwrap_err();
    assert!(matches!(
        error,
        LocalGitError::CommandFailed {
            action: "switch branch",
            ..
        }
    ));
    assert!(matches!(
        backend.snapshot().unwrap().head,
        HeadState::Attached { ref branch, .. } if branch == "main"
    ));

    git(root, &["restore", "file"]);
    fs::write(root.join("file"), "main\n").unwrap();
    commit_all(root, "main");
    let merge = git_output(root, &["merge", "other"]);
    assert!(!merge.status.success());
    let conflicted = backend.snapshot().unwrap();
    assert!(conflicted.operation.merge);
    assert_eq!(conflicted.conflicts.len(), 1);
    assert_eq!(conflicted.conflicts[0].path.raw, b"file");
}

#[test]
fn actual_rebase_and_cherry_pick_states_are_read_from_git_metadata() {
    let directory = init();
    let root = directory.path();
    fs::write(root.join("file"), "base\n").unwrap();
    commit_all(root, "base");
    git(root, &["switch", "-qc", "topic"]);
    fs::write(root.join("file"), "topic\n").unwrap();
    let topic_commit = commit_all(root, "topic");
    git(root, &["switch", "-q", "main"]);
    fs::write(root.join("file"), "main\n").unwrap();
    commit_all(root, "main");

    let rebase = git_output(root, &["rebase", "main", "topic"]);
    assert!(!rebase.status.success());
    let backend = LocalGit::open(root).unwrap();
    let rebasing = backend.snapshot().unwrap();
    assert_ne!(rebasing.operation.rebase, RebaseState::None);
    assert_eq!(rebasing.conflicts.len(), 1);
    git(root, &["rebase", "--abort"]);

    git(root, &["switch", "-q", "main"]);
    let cherry_pick = git_output(root, &["cherry-pick", &topic_commit]);
    assert!(!cherry_pick.status.success());
    let picking = backend.snapshot().unwrap();
    assert!(picking.operation.cherry_pick);
    assert_eq!(picking.conflicts.len(), 1);
    git(root, &["cherry-pick", "--abort"]);
}

#[test]
fn local_bare_remote_fetch_ff_pull_push_and_explicit_lease_rejection() {
    let directory = init();
    let root = directory.path();
    fs::write(root.join("file"), "base\n").unwrap();
    commit_all(root, "base");
    let bare = TempDir::new().unwrap();
    git(bare.path(), &["init", "--bare", "-q"]);
    git(
        root,
        &["remote", "add", "origin", bare.path().to_str().unwrap()],
    );
    let backend = LocalGit::open(root).unwrap();
    let snapshot = backend.snapshot().unwrap();
    backend.push("origin", "main", &snapshot.guard).unwrap();
    git(root, &["branch", "--set-upstream-to=origin/main", "main"]);

    let peer_parent = TempDir::new().unwrap();
    let peer = peer_parent.path().join("peer");
    git(
        peer_parent.path(),
        &[
            "clone",
            "-q",
            bare.path().to_str().unwrap(),
            peer.to_str().unwrap(),
        ],
    );
    configure_identity(&peer);
    git(&peer, &["switch", "-q", "main"]);
    fs::write(peer.join("file"), "peer one\n").unwrap();
    let peer_one = commit_all(&peer, "peer one");
    git(&peer, &["push", "-q", "origin", "main"]);

    let before_fetch = backend.snapshot().unwrap();
    backend.fetch("origin", &before_fetch.guard).unwrap();
    let fetched = backend.snapshot().unwrap();
    assert_eq!(fetched.behind, 1);
    assert_eq!(fetched.upstream_oid.as_deref(), Some(peer_one.as_str()));
    backend
        .fast_forward_pull("origin", "main", &fetched.guard)
        .unwrap();
    assert!(matches!(
        backend.snapshot().unwrap().head,
        HeadState::Attached { ref oid, .. } if oid == &peer_one
    ));

    fs::write(root.join("local"), "local\n").unwrap();
    let dirty = backend.snapshot().unwrap();
    backend.stage(&[path(b"local")], &dirty.guard).unwrap();
    let staged = backend.snapshot().unwrap();
    backend.commit("local", &staged.guard).unwrap();
    let committed = backend.snapshot().unwrap();
    backend.push("origin", "main", &committed.guard).unwrap();
    let remote_after_local = backend
        .observe_remote_branch("origin", "main")
        .unwrap()
        .oid
        .unwrap();

    git(&peer, &["pull", "-q", "--ff-only"]);
    fs::write(peer.join("peer"), "peer two\n").unwrap();
    commit_all(&peer, "peer two");
    git(&peer, &["push", "-q", "origin", "main"]);
    fs::write(root.join("file"), "rewritten locally\n").unwrap();
    commit_all(root, "local divergent");
    let lease_guard = backend.snapshot().unwrap();
    let error = backend
        .force_push_with_lease("origin", "main", &remote_after_local, &lease_guard.guard)
        .unwrap_err();
    assert!(matches!(
        error,
        LocalGitError::CommandFailed {
            action: "force push with lease",
            ..
        }
    ));
    let observed = backend.observe_remote_branch("origin", "main").unwrap();
    assert_ne!(observed.oid.as_deref(), Some(remote_after_local.as_str()));
}

#[test]
fn non_utf8_index_path_is_preserved_even_when_filesystem_cannot_create_it() {
    let directory = init();
    let root = directory.path();
    commit_all(root, "base");
    let oid = String::from_utf8(git_input(root, &["hash-object", "-w", "--stdin"], b"raw\n"))
        .unwrap()
        .trim()
        .to_owned();
    let raw_path = b"non-utf8-\xff.txt";
    let mut index_record = format!("100644 {oid}\t").into_bytes();
    index_record.extend_from_slice(raw_path);
    index_record.push(0);
    git_input(root, &["update-index", "-z", "--index-info"], &index_record);
    let snapshot = LocalGit::open(root).unwrap().snapshot().unwrap();
    let entry = snapshot
        .staged
        .iter()
        .find(|entry| entry.path.raw == raw_path)
        .unwrap();
    assert_eq!(entry.path.raw, raw_path);
    assert_eq!(entry.path.display, "non-utf8-\\xff.txt");
}

#[test]
fn mutation_timeout_is_explicitly_uncertain_and_kills_hook_process_group() {
    let directory = init();
    let root = directory.path();
    fs::write(root.join("file"), "base\n").unwrap();
    commit_all(root, "base");
    fs::write(root.join("file"), "changed\n").unwrap();
    git(root, &["add", "file"]);
    let hook = root.join(".git/hooks/pre-commit");
    fs::write(&hook, "#!/bin/sh\nsleep 5\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let backend = LocalGit::open_with_limits(
        root,
        CommandLimits {
            deadline: Duration::from_millis(100),
            max_output_bytes: 1024 * 1024,
            max_input_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    let snapshot = backend.snapshot().unwrap();
    let started = std::time::Instant::now();
    let error = backend
        .commit("will time out", &snapshot.guard)
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(matches!(
        error,
        LocalGitError::TimedOut {
            action: "commit",
            certainty: OutcomeCertainty::Uncertain,
        }
    ));
}

#[test]
fn ref_remote_path_and_oid_validation_rejects_option_or_revision_injection() {
    let directory = init();
    fs::write(directory.path().join("file"), "base\n").unwrap();
    commit_all(directory.path(), "base");
    let backend = LocalGit::open(directory.path()).unwrap();
    let guard = backend.snapshot().unwrap().guard;
    assert!(matches!(
        backend.switch_branch("--detach", &guard),
        Err(LocalGitError::InvalidInput(_))
    ));
    assert!(matches!(
        backend.create_branch("@{-1}", None, &guard),
        Err(LocalGitError::InvalidInput(_))
    ));
    assert!(matches!(
        backend.fetch("--all", &guard),
        Err(LocalGitError::InvalidInput(_))
    ));
    assert!(GitPath::from_raw(b"../outside".to_vec()).is_err());
    assert!(matches!(
        backend.force_push_with_lease("origin", "main", "HEAD", &guard),
        Err(LocalGitError::InvalidInput(_))
    ));
}
