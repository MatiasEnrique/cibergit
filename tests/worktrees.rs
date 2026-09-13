use cibergit::{
    local_git::{HeadState, LocalGit, LocalGitError},
    worktrees::*,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::mpsc,
    time::{Duration, Instant},
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

fn init_at(path: &Path) -> String {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "Worktree Test"]);
    git(path, &["config", "user.email", "worktree@example.invalid"]);
    fs::write(path.join("tracked.txt"), "base\n").unwrap();
    git(path, &["add", "tracked.txt"]);
    git(path, &["commit", "-qm", "base"]);
    git(path, &["rev-parse", "HEAD"])
}

struct Fixture {
    _temp: TempDir,
    source: PathBuf,
    state: PathBuf,
    managed: PathBuf,
    head: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let state = temp.path().join("state");
        let managed = temp.path().join("managed");
        fs::create_dir(&state).unwrap();
        fs::create_dir(&managed).unwrap();
        let head = init_at(&source);
        Self {
            _temp: temp,
            source,
            state,
            managed,
            head,
        }
    }

    fn manager(&self) -> WorktreeManager {
        WorktreeManager::open(&self.state, &self.managed).unwrap()
    }

    fn request(&self, number: u64, branch: &str) -> CreateFromLocalRequest {
        CreateFromLocalRequest {
            key: key("one", number),
            object_repository: self.source.clone(),
            start_oid: self.head.clone(),
            local_branch: branch.into(),
            intended_remote_branch: Some("contributor/topic".into()),
            published_head: Some(self.head.clone()),
        }
    }
}

fn key(account: &str, pull_request: u64) -> AssociationKey {
    AssociationKey {
        provider: "github".into(),
        host: "github.example".into(),
        account: account.into(),
        repository: "repository-42".into(),
        pull_request,
    }
}

fn created(outcome: ProvisionOutcome) -> CheckoutView {
    match outcome {
        ProvisionOutcome::Created(view) => view,
        ProvisionOutcome::Reused(_) => panic!("expected new managed checkout"),
    }
}

fn managed_path(managed: &Path, key: &AssociationKey) -> PathBuf {
    let scope = format!(
        "{}\n{}\n{}\n{}\n{}",
        key.provider, key.host, key.account, key.repository, key.pull_request
    );
    managed
        .canonicalize()
        .unwrap()
        .join("worktrees")
        .join(format!("{:x}", Sha256::digest(scope)))
}

#[test]
fn explicit_attach_is_read_only_and_reopen_observes_external_head_changes() {
    let fixture = Fixture::new();
    fs::write(fixture.source.join("tracked.txt"), "edited\n").unwrap();
    fs::write(fixture.source.join("untracked.txt"), "keep\n").unwrap();
    git(&fixture.source, &["add", "tracked.txt"]);
    git(
        &fixture.source,
        &["remote", "add", "upstream", "ssh://example.invalid/repo"],
    );
    let before_status = git(&fixture.source, &["status", "--porcelain=v2"]);
    let before_remote = git(&fixture.source, &["remote", "get-url", "upstream"]);
    let common = LocalGit::open(&fixture.source)
        .unwrap()
        .common_git_dir()
        .to_path_buf();
    let manager = fixture.manager();
    let view = manager
        .attach(AttachRequest {
            key: key("one", 1),
            checkout_path: fixture.source.clone(),
            expected_common_git_dir: common,
            intended_remote_branch: Some("owner/source".into()),
            published_head: Some(fixture.head.clone()),
        })
        .unwrap();
    assert_eq!(
        view.association.ownership,
        CheckoutOwnership::ExplicitlyAttached
    );
    assert_eq!(
        git(&fixture.source, &["status", "--porcelain=v2"]),
        before_status
    );
    assert_eq!(
        git(&fixture.source, &["remote", "get-url", "upstream"]),
        before_remote
    );

    git(&fixture.source, &["reset", "--hard", "-q"]);
    fs::remove_file(fixture.source.join("untracked.txt")).unwrap();
    git(&fixture.source, &["checkout", "--detach", "-q", "HEAD"]);
    let reopened = manager.reopen(&key("one", 1)).unwrap().unwrap();
    assert!(matches!(reopened.actual_head, HeadState::Detached { .. }));
    assert_eq!(reopened.association.path, view.association.path);
    assert_eq!(reopened.association.git_dir, view.association.git_dir);
}

#[test]
fn attach_requires_explicit_repository_identity_and_stays_account_partitioned() {
    let fixture = Fixture::new();
    let other = fixture._temp.path().join("other");
    init_at(&other);
    let manager = fixture.manager();
    let wrong_common = LocalGit::open(&other)
        .unwrap()
        .common_git_dir()
        .to_path_buf();
    assert!(matches!(
        manager.attach(AttachRequest {
            key: key("one", 2),
            checkout_path: fixture.source.clone(),
            expected_common_git_dir: wrong_common,
            intended_remote_branch: None,
            published_head: Some(fixture.head.clone()),
        }),
        Err(WorktreeError::InvalidInput(_))
    ));
    let common = LocalGit::open(&fixture.source)
        .unwrap()
        .common_git_dir()
        .to_path_buf();
    manager
        .attach(AttachRequest {
            key: key("one", 2),
            checkout_path: fixture.source.clone(),
            expected_common_git_dir: common.clone(),
            intended_remote_branch: None,
            published_head: Some(fixture.head.clone()),
        })
        .unwrap();
    assert!(manager.reopen(&key("two", 2)).unwrap().is_none());
    assert!(matches!(
        manager.attach(AttachRequest {
            key: key("one", 2),
            checkout_path: other,
            expected_common_git_dir: common,
            intended_remote_branch: None,
            published_head: Some(fixture.head.clone()),
        }),
        Err(WorktreeError::InvalidInput(_)) | Err(WorktreeError::AssociationConflict)
    ));
}

#[test]
fn creates_exact_persistent_branch_and_reuses_without_advancing_it() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let proposed = propose_local_branch(&key("one", 3)).unwrap();
    assert!(proposed.starts_with("cibergit/pr-3-"));
    let first = created(
        manager
            .create_from_local(fixture.request(3, "cibergit/pr-3"))
            .unwrap(),
    );
    assert_eq!(
        first.association.path,
        managed_path(&fixture.managed, &key("one", 3))
    );
    assert_eq!(
        first.association.creation.as_ref().unwrap().local_branch,
        "cibergit/pr-3"
    );
    assert_eq!(
        first.association.intended_remote_branch.as_deref(),
        Some("contributor/topic")
    );
    assert!(matches!(
        first.actual_head,
        HeadState::Attached { ref branch, ref oid }
            if branch == "cibergit/pr-3" && oid == &fixture.head
    ));

    fs::write(first.association.path.join("tracked.txt"), "local edit\n").unwrap();
    git(
        &first.association.path,
        &["checkout", "-q", "-b", "external/pr-3"],
    );
    git(&first.association.path, &["commit", "-qam", "outside"]);
    let outside = git(&first.association.path, &["rev-parse", "HEAD"]);
    let mut revisit = fixture.request(3, "different-proposal");
    revisit.intended_remote_branch = Some("different/remote".into());
    let reused = match manager.create_from_local(revisit).unwrap() {
        ProvisionOutcome::Reused(view) => view,
        ProvisionOutcome::Created(_) => panic!("association was recreated"),
    };
    assert!(matches!(
        reused.actual_head,
        HeadState::Attached { ref branch, ref oid }
            if branch == "external/pr-3" && oid == &outside
    ));
    assert_eq!(
        reused.association.creation.as_ref().unwrap().local_branch,
        "cibergit/pr-3"
    );
    assert_eq!(
        reused.association.intended_remote_branch.as_deref(),
        Some("contributor/topic")
    );
}

#[test]
fn create_refuses_occupied_branch_existing_path_and_missing_exact_object() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    git(&fixture.source, &["branch", "occupied"]);
    assert!(matches!(
        manager.create_from_local(fixture.request(4, "occupied")),
        Err(WorktreeError::InvalidInput(_))
    ));
    let missing = "f".repeat(40);
    let mut missing_request = fixture.request(5, "missing-object");
    missing_request.start_oid = missing;
    assert!(manager.create_from_local(missing_request).is_err());
    let occupied_key = key("one", 6);
    let path = managed_path(&fixture.managed, &occupied_key);
    fs::create_dir(&path).unwrap();
    let request = fixture.request(6, "path-occupied");
    assert!(matches!(
        manager.create_from_local(request),
        Err(WorktreeError::InvalidInput(_))
    ));
    assert!(
        !git(&fixture.source, &["branch", "--list", "path-occupied"]).contains("path-occupied")
    );
}

#[test]
fn remote_provision_uses_local_transport_and_verifies_exact_fetched_head() {
    let fixture = Fixture::new();
    let remote = fixture._temp.path().join("remote.git");
    git(
        fixture._temp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            fixture.source.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    let manager = fixture.manager();
    let credential_url = ProvisionFromRemoteRequest {
        key: key("one", 70),
        repository_url: "https://secret@github.example/owner/repo.git".into(),
        fetch_ref: "refs/heads/main".into(),
        exact_head_oid: fixture.head.clone(),
        local_branch: "cibergit/pr-70".into(),
        intended_remote_branch: None,
        published_head: Some(fixture.head.clone()),
    };
    assert!(matches!(
        manager.provision_from_remote(credential_url),
        Err(WorktreeError::InvalidInput(_))
    ));
    assert!(!managed_path(&fixture.managed, &key("one", 70)).exists());
    let request = ProvisionFromRemoteRequest {
        key: key("one", 7),
        repository_url: remote.to_string_lossy().into_owned(),
        fetch_ref: "refs/heads/main".into(),
        exact_head_oid: fixture.head.clone(),
        local_branch: "cibergit/pr-7".into(),
        intended_remote_branch: Some("fork/pr-7".into()),
        published_head: Some(fixture.head.clone()),
    };
    let view = created(manager.provision_from_remote(request).unwrap());
    assert_eq!(
        git(&view.association.path, &["rev-parse", "HEAD"]),
        fixture.head
    );
    let object_repository = &view
        .association
        .creation
        .as_ref()
        .unwrap()
        .object_repository;
    assert!(
        object_repository.starts_with(fixture.managed.canonicalize().unwrap().join("repositories"))
    );
    assert_eq!(
        git(object_repository, &["remote", "get-url", "origin"]),
        remote.to_string_lossy()
    );

    let mismatch = ProvisionFromRemoteRequest {
        key: key("one", 8),
        repository_url: remote.to_string_lossy().into_owned(),
        fetch_ref: "refs/heads/main".into(),
        exact_head_oid: "a".repeat(40),
        local_branch: "cibergit/pr-8".into(),
        intended_remote_branch: None,
        published_head: Some(fixture.head.clone()),
    };
    assert!(matches!(
        manager.provision_from_remote(mismatch),
        Err(WorktreeError::Uncertain { .. })
    ));
    let incomplete = managed_path(&fixture.managed, &key("one", 8));
    assert!(incomplete.is_dir());
    assert!(!incomplete.join(".git").exists());
    assert!(matches!(
        manager.reconcile(&key("one", 8)).unwrap(),
        ReconcileOutcome::Incomplete(_)
    ));
}

#[test]
fn corrupt_and_future_store_data_are_preserved_and_symlinks_are_refused() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    created(
        manager
            .create_from_local(fixture.request(9, "cibergit/pr-9"))
            .unwrap(),
    );
    let store = fixture.state.join("worktree-associations.json");
    let valid = fs::read(&store).unwrap();
    assert_eq!(
        fs::metadata(&store).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&fixture.state).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let mut future: serde_json::Value = serde_json::from_slice(&valid).unwrap();
    future["schema_version"] = 99.into();
    for bytes in [
        b"broken json".to_vec(),
        serde_json::to_vec(&future).unwrap(),
    ] {
        fs::write(&store, &bytes).unwrap();
        assert!(WorktreeManager::open(&fixture.state, &fixture.managed).is_err());
        assert_eq!(fs::read(&store).unwrap(), bytes);
    }
    let oversized = vec![b'x'; 2 * 1024 * 1024 + 1];
    fs::write(&store, &oversized).unwrap();
    assert!(WorktreeManager::open(&fixture.state, &fixture.managed).is_err());
    assert_eq!(fs::metadata(&store).unwrap().len(), oversized.len() as u64);
    fs::write(&store, valid).unwrap();
    let link = fixture._temp.path().join("state-link");
    std::os::unix::fs::symlink(&fixture.state, &link).unwrap();
    assert!(WorktreeManager::open(&link, fixture._temp.path().join("managed-2")).is_err());
}

#[test]
fn stale_preparation_is_reported_without_git_mutation() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let claims = fixture.managed.join("claims");
    fs::set_permissions(&claims, fs::Permissions::from_mode(0o500)).unwrap();
    let result = manager.create_from_local(fixture.request(10, "cibergit/pr-10"));
    fs::set_permissions(&claims, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
    let outcome = manager.reconcile(&key("one", 10)).unwrap();
    assert!(matches!(outcome, ReconcileOutcome::PreparedNotStarted(_)));
    assert!(!managed_path(&fixture.managed, &key("one", 10)).exists());
    assert!(git(&fixture.source, &["branch", "--list", "cibergit/pr-10"]).is_empty());
}

#[test]
fn lost_store_ack_after_git_creation_reconciles_without_replay() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let sentinel = fixture._temp.path().join("hook-entered");
    let release = fixture._temp.path().join("hook-release");
    let hook = fixture.source.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\n: > '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\n",
            sentinel.display(),
            release.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let worker_manager = manager.clone();
    let request = fixture.request(11, "cibergit/pr-11");
    let worker = std::thread::spawn(move || worker_manager.create_from_local(request));
    wait_for(&sentinel);
    fs::set_permissions(&fixture.state, fs::Permissions::from_mode(0o500)).unwrap();
    fs::write(&release, "continue").unwrap();
    let result = worker.join().unwrap();
    fs::set_permissions(&fixture.state, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(result, Err(WorktreeError::Uncertain { .. })));
    assert!(managed_path(&fixture.managed, &key("one", 11)).is_dir());
    let reconciled = manager.reconcile(&key("one", 11)).unwrap();
    assert!(matches!(reconciled, ReconcileOutcome::Completed(_)));
    let branches = git(&fixture.source, &["branch", "--list", "cibergit/pr-11"]);
    assert_eq!(branches.matches("cibergit/pr-11").count(), 1);
}

#[test]
fn reconciliation_refuses_a_recreated_path_even_with_matching_git_identity() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let sentinel = fixture._temp.path().join("replace-hook-entered");
    let release = fixture._temp.path().join("replace-hook-release");
    let hook = fixture.source.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\n: > '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\n",
            sentinel.display(),
            release.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let worker_manager = manager.clone();
    let request = fixture.request(13, "cibergit/pr-13");
    let worker = std::thread::spawn(move || worker_manager.create_from_local(request));
    wait_for(&sentinel);
    fs::set_permissions(&fixture.state, fs::Permissions::from_mode(0o500)).unwrap();
    fs::write(&release, "continue").unwrap();
    let result = worker.join().unwrap();
    fs::set_permissions(&fixture.state, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(result, Err(WorktreeError::Uncertain { .. })));
    let path = managed_path(&fixture.managed, &key("one", 13));
    git(
        &fixture.source,
        &["worktree", "remove", "--force", path.to_str().unwrap()],
    );
    git(
        &fixture.source,
        &[
            "worktree",
            "add",
            "-q",
            path.to_str().unwrap(),
            "cibergit/pr-13",
        ],
    );
    assert!(matches!(
        manager.reconcile(&key("one", 13)).unwrap(),
        ReconcileOutcome::Incomplete(_)
    ));
    assert!(manager.reopen(&key("one", 13)).unwrap().is_none());
}

#[test]
fn linked_worktree_mutations_share_the_local_git_common_directory_lock() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let sentinel = fixture._temp.path().join("lock-hook-entered");
    let release = fixture._temp.path().join("lock-hook-release");
    let hook = fixture.source.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\n: > '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\n",
            sentinel.display(),
            release.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let local = LocalGit::open(&fixture.source).unwrap();
    fs::write(fixture.source.join("pending.txt"), "pending\n").unwrap();
    let snapshot = local.snapshot().unwrap();
    let create_manager = manager.clone();
    let request = fixture.request(12, "cibergit/pr-12");
    let creator = std::thread::spawn(move || create_manager.create_from_local(request));
    wait_for(&sentinel);
    let (sender, receiver) = mpsc::channel();
    let mutator = std::thread::spawn(move || {
        let result = local.stage(
            &[cibergit::local_git::GitPath::from_raw(b"pending.txt".to_vec()).unwrap()],
            &snapshot.guard,
        );
        sender.send(result).unwrap();
    });
    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    fs::write(&release, "continue").unwrap();
    created(creator.join().unwrap().unwrap());
    assert!(matches!(
        receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
        Err(LocalGitError::StaleSnapshot)
    ));
    mutator.join().unwrap();
}

#[test]
fn cleanup_preserves_branch_and_refuses_all_local_work_and_attached_checkouts() {
    let fixture = Fixture::new();
    let manager = fixture.manager();

    let safe = created(
        manager
            .create_from_local(fixture.request(20, "cibergit/pr-20"))
            .unwrap(),
    );
    assert_eq!(
        manager
            .cleanup_eligibility(&key("one", 20), Some(&fixture.head))
            .unwrap(),
        CleanupEligibility::Eligible
    );
    assert_eq!(
        manager
            .remove_managed(&key("one", 20), Some(&fixture.head))
            .unwrap(),
        RemovalOutcome::Removed
    );
    assert!(!safe.association.path.exists());
    assert!(!git(&fixture.source, &["branch", "--list", "cibergit/pr-20"]).is_empty());
    assert!(manager.reopen(&key("one", 20)).unwrap().is_none());

    let untracked = created(
        manager
            .create_from_local(fixture.request(21, "cibergit/pr-21"))
            .unwrap(),
    );
    fs::write(untracked.association.path.join("new.txt"), "keep").unwrap();
    assert_eq!(
        manager
            .cleanup_eligibility(&key("one", 21), Some(&fixture.head))
            .unwrap(),
        CleanupEligibility::Ineligible(CleanupBlocker::UntrackedFiles)
    );

    let ignored = created(
        manager
            .create_from_local(fixture.request(22, "cibergit/pr-22"))
            .unwrap(),
    );
    fs::write(ignored.association.path.join(".gitignore"), "secret.log\n").unwrap();
    git(&ignored.association.path, &["add", ".gitignore"]);
    git(&ignored.association.path, &["commit", "-qm", "ignore rule"]);
    let ignored_head = git(&ignored.association.path, &["rev-parse", "HEAD"]);
    fs::write(ignored.association.path.join("secret.log"), "keep").unwrap();
    assert_eq!(
        manager
            .cleanup_eligibility(&key("one", 22), Some(&ignored_head))
            .unwrap(),
        CleanupEligibility::Ineligible(CleanupBlocker::IgnoredFiles)
    );

    let unpublished = created(
        manager
            .create_from_local(fixture.request(23, "cibergit/pr-23"))
            .unwrap(),
    );
    fs::write(
        unpublished.association.path.join("tracked.txt"),
        "committed\n",
    )
    .unwrap();
    git(&unpublished.association.path, &["add", "tracked.txt"]);
    git(
        &unpublished.association.path,
        &["commit", "-qm", "local only"],
    );
    assert_eq!(
        manager
            .cleanup_eligibility(&key("one", 23), Some(&fixture.head))
            .unwrap(),
        CleanupEligibility::Ineligible(CleanupBlocker::CommitsNotContainedInPublishedHead)
    );

    let staged = created(
        manager
            .create_from_local(fixture.request(26, "cibergit/pr-26"))
            .unwrap(),
    );
    fs::write(staged.association.path.join("tracked.txt"), "staged\n").unwrap();
    git(&staged.association.path, &["add", "tracked.txt"]);
    assert_eq!(
        manager
            .cleanup_eligibility(&key("one", 26), Some(&fixture.head))
            .unwrap(),
        CleanupEligibility::Ineligible(CleanupBlocker::StagedChanges)
    );

    let unstaged = created(
        manager
            .create_from_local(fixture.request(27, "cibergit/pr-27"))
            .unwrap(),
    );
    fs::write(unstaged.association.path.join("tracked.txt"), "unstaged\n").unwrap();
    assert_eq!(
        manager
            .cleanup_eligibility(&key("one", 27), Some(&fixture.head))
            .unwrap(),
        CleanupEligibility::Ineligible(CleanupBlocker::UnstagedChanges)
    );

    let detached = created(
        manager
            .create_from_local(fixture.request(24, "cibergit/pr-24"))
            .unwrap(),
    );
    git(&detached.association.path, &["checkout", "--detach", "-q"]);
    assert_eq!(
        manager
            .cleanup_eligibility(&key("one", 24), Some(&fixture.head))
            .unwrap(),
        CleanupEligibility::Ineligible(CleanupBlocker::DetachedHead)
    );

    let attached_key = key("one", 25);
    let common = LocalGit::open(&fixture.source)
        .unwrap()
        .common_git_dir()
        .to_path_buf();
    manager
        .attach(AttachRequest {
            key: attached_key.clone(),
            checkout_path: fixture.source.clone(),
            expected_common_git_dir: common,
            intended_remote_branch: None,
            published_head: Some(fixture.head.clone()),
        })
        .unwrap();
    assert_eq!(
        manager
            .cleanup_eligibility(&attached_key, Some(&fixture.head))
            .unwrap(),
        CleanupEligibility::Ineligible(CleanupBlocker::NotManaged)
    );
    assert!(matches!(
        manager.remove_managed(&attached_key, Some(&fixture.head)),
        Err(WorktreeError::CleanupRefused(CleanupBlocker::NotManaged))
    ));
    assert!(fixture.source.exists());
}

#[test]
fn cleanup_refuses_conflict_operation_and_unknown_publication() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let view = created(
        manager
            .create_from_local(fixture.request(30, "cibergit/pr-30"))
            .unwrap(),
    );
    assert_eq!(
        manager.cleanup_eligibility(&key("one", 30), None).unwrap(),
        CleanupEligibility::Ineligible(CleanupBlocker::UnknownPublishedHead)
    );
    git(
        &fixture.source,
        &["checkout", "-q", "-b", "other", &fixture.head],
    );
    fs::write(fixture.source.join("tracked.txt"), "source side\n").unwrap();
    git(&fixture.source, &["commit", "-qam", "source side"]);
    fs::write(view.association.path.join("tracked.txt"), "worktree side\n").unwrap();
    git(&view.association.path, &["commit", "-qam", "worktree side"]);
    let output = git_output(&view.association.path, &["merge", "--no-edit", "other"]);
    assert!(!output.status.success());
    assert_eq!(
        manager
            .cleanup_eligibility(&key("one", 30), Some(&fixture.head))
            .unwrap(),
        CleanupEligibility::Ineligible(CleanupBlocker::Conflicts)
    );

    let operation = created(
        manager
            .create_from_local(fixture.request(32, "cibergit/pr-32"))
            .unwrap(),
    );
    fs::write(
        operation.association.git_dir.join("MERGE_HEAD"),
        &fixture.head,
    )
    .unwrap();
    assert_eq!(
        manager
            .cleanup_eligibility(&key("one", 32), Some(&fixture.head))
            .unwrap(),
        CleanupEligibility::Ineligible(CleanupBlocker::GitOperation)
    );
}

#[test]
fn replacement_checkout_at_managed_path_is_never_silently_adopted() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let view = created(
        manager
            .create_from_local(fixture.request(31, "cibergit/pr-31"))
            .unwrap(),
    );
    git(
        &fixture.source,
        &[
            "worktree",
            "remove",
            "--force",
            view.association.path.to_str().unwrap(),
        ],
    );
    init_at(&view.association.path);
    fs::write(
        view.association.path.join("replacement-only.txt"),
        "preserve\n",
    )
    .unwrap();
    assert!(manager.reopen(&key("one", 31)).is_err());
    assert_eq!(
        manager
            .cleanup_eligibility(&key("one", 31), Some(&fixture.head))
            .unwrap(),
        CleanupEligibility::Ineligible(CleanupBlocker::IdentityChanged)
    );
    assert!(matches!(
        manager.remove_managed(&key("one", 31), Some(&fixture.head)),
        Err(WorktreeError::CleanupRefused(
            CleanupBlocker::IdentityChanged
        ))
    ));
    assert_eq!(
        fs::read_to_string(view.association.path.join("replacement-only.txt")).unwrap(),
        "preserve\n"
    );
}

#[test]
fn attached_checkout_replacement_at_identical_paths_is_refused_without_mutation() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let association_key = key("one", 33);
    let original_common = LocalGit::open(&fixture.source)
        .unwrap()
        .common_git_dir()
        .to_path_buf();
    manager
        .attach(AttachRequest {
            key: association_key.clone(),
            checkout_path: fixture.source.clone(),
            expected_common_git_dir: original_common,
            intended_remote_branch: Some("owner/original".into()),
            published_head: Some(fixture.head.clone()),
        })
        .unwrap();
    let original_aside = fixture._temp.path().join("source-original-aside");
    fs::rename(&fixture.source, &original_aside).unwrap();
    let replacement_head = init_at(&fixture.source);
    fs::write(fixture.source.join("replacement-only.txt"), "preserve\n").unwrap();
    let replacement_common = LocalGit::open(&fixture.source)
        .unwrap()
        .common_git_dir()
        .to_path_buf();

    assert!(manager.reopen(&association_key).is_err());
    assert!(matches!(
        manager.attach(AttachRequest {
            key: association_key.clone(),
            checkout_path: fixture.source.clone(),
            expected_common_git_dir: replacement_common,
            intended_remote_branch: Some("owner/replacement".into()),
            published_head: Some(replacement_head),
        }),
        Err(WorktreeError::AssociationConflict)
    ));
    assert!(matches!(
        manager.remove_managed(&association_key, Some(&fixture.head)),
        Err(WorktreeError::CleanupRefused(CleanupBlocker::NotManaged))
    ));
    assert_eq!(
        fs::read_to_string(fixture.source.join("replacement-only.txt")).unwrap(),
        "preserve\n"
    );
    assert!(original_aside.join("tracked.txt").exists());
}

fn wait_for(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
