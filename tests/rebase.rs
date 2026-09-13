#![allow(dead_code)] // Source modules are included because the parent owns final lib export.

#[path = "../src/local_git.rs"]
mod local_git;
#[path = "../src/rebase.rs"]
mod rebase;

use local_git::{CommandLimits, GitPath, LocalGit};
use rebase::{
    BlobContent, OperationState, PlanAction, PlanStep, PrepareOutcome, RebaseAssociation,
    RebaseError, RebasePlan, RebaseStore, StashRestoreState, UnsupportedHistory,
};
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Repo {
    temp: TempDir,
    root: PathBuf,
    state: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout with 'quotes'");
        fs::create_dir(&root).unwrap();
        run_ok(&root, ["init", "-b", "topic"]);
        run_ok(&root, ["config", "user.name", "Rebase Test"]);
        run_ok(&root, ["config", "user.email", "rebase@example.invalid"]);
        let state = temp.path().join("private state with 'quotes'");
        Self { temp, root, state }
    }

    fn write(&self, path: &str, contents: &str) {
        let path = self.root.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn write_bytes(&self, path: &str, contents: &[u8]) {
        fs::write(self.root.join(path), contents).unwrap();
    }

    fn commit(&self, message: &str) -> String {
        run_ok(&self.root, ["add", "--all"]);
        run_ok(&self.root, ["commit", "-m", message]);
        self.oid("HEAD")
    }

    fn oid(&self, revision: &str) -> String {
        text(run_ok(&self.root, ["rev-parse", revision]))
            .trim()
            .into()
    }

    fn store(&self) -> RebaseStore {
        RebaseStore::open(&self.state, association(), &self.root).unwrap()
    }

    fn snapshot(&self) -> local_git::LocalSnapshot {
        LocalGit::open(&self.root).unwrap().snapshot().unwrap()
    }
}

fn association() -> RebaseAssociation {
    RebaseAssociation {
        provider: "github".into(),
        host: "github.com".into(),
        account: "test".into(),
        repository: "owner/repo".into(),
        change: "42".into(),
    }
}

fn run(root: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Output {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    command.args(args);
    command.output().unwrap()
}

fn run_ok(root: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Vec<u8> {
    let output = run(root, args);
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap()
}

fn ready(store: &RebaseStore, base: &str) -> rebase::RebasePreparation {
    match store.prepare(base).unwrap() {
        PrepareOutcome::Ready(preparation) => preparation,
        other => panic!("expected ready preparation, got {other:?}"),
    }
}

fn steps(inventory: &rebase::CommitInventory, actions: Vec<(usize, PlanAction)>) -> RebasePlan {
    RebasePlan::validate(
        inventory,
        actions
            .into_iter()
            .map(|(index, action)| PlanStep {
                commit_oid: inventory.commits[index].oid.clone(),
                action,
            })
            .collect(),
    )
    .unwrap()
}

#[test]
fn all_non_edit_plan_actions_rewrite_exact_tree_without_publishing() {
    let repo = Repo::new();
    repo.write("base", "base\n");
    let base = repo.commit("base");
    for (path, message) in [
        ("a", "one"),
        ("b", "two"),
        ("c", "three"),
        ("d", "drop me"),
        ("e", "five"),
    ] {
        repo.write(path, message);
        repo.commit(message);
    }
    let bare = repo.temp.path().join("remote.git");
    run_ok(repo.temp.path(), ["init", "--bare", bare.to_str().unwrap()]);
    run_ok(
        &repo.root,
        ["remote", "add", "origin", bare.to_str().unwrap()],
    );
    run_ok(&repo.root, ["push", "origin", "topic"]);
    let published = text(run_ok(&bare, ["rev-parse", "refs/heads/topic"]))
        .trim()
        .to_owned();

    let store = repo.store();
    let preparation = ready(&store, &base);
    let plan = steps(
        &preparation.inventory,
        vec![
            (1, PlanAction::Pick),
            (0, PlanAction::Squash),
            (2, PlanAction::Fixup),
            (3, PlanAction::Drop),
            (
                4,
                PlanAction::Reword {
                    message: "safe reword\nexec touch SHOULD_NOT_EXIST".into(),
                },
            ),
        ],
    );
    let view = store.start(&preparation, &plan).unwrap();
    assert_eq!(view.state, OperationState::Completed);
    assert!(view.publish_warning.is_some());
    assert!(!repo.root.join("SHOULD_NOT_EXIST").exists());
    assert_eq!(fs::read_to_string(repo.root.join("a")).unwrap(), "one");
    assert_eq!(fs::read_to_string(repo.root.join("b")).unwrap(), "two");
    assert_eq!(fs::read_to_string(repo.root.join("c")).unwrap(), "three");
    assert_eq!(fs::read_to_string(repo.root.join("e")).unwrap(), "five");
    assert!(!repo.root.join("d").exists());
    assert!(
        text(run_ok(&repo.root, ["log", "-1", "--format=%B"]))
            .starts_with("safe reword\nexec touch SHOULD_NOT_EXIST")
    );
    assert_eq!(
        text(run_ok(&bare, ["rev-parse", "refs/heads/topic"])).trim(),
        published
    );
    assert!(
        run(&repo.root, ["merge-base", "--is-ancestor", &base, "HEAD"])
            .status
            .success()
    );
}

#[test]
fn terminal_retirement_archives_evidence_and_allows_a_second_prepare() {
    let repo = Repo::new();
    repo.write("base", "base\n");
    let base = repo.commit("base");
    repo.write("topic", "topic\n");
    repo.commit("topic");
    let store = repo.store();
    let preparation = ready(&store, &base);
    let plan = steps(
        &preparation.inventory,
        vec![(
            0,
            PlanAction::Reword {
                message: "rewritten for retirement".into(),
            },
        )],
    );
    let completed = store.start(&preparation, &plan).unwrap();
    assert_eq!(completed.state, OperationState::Completed);
    assert!(matches!(
        store.prepare(&base).unwrap_err(),
        RebaseError::PendingOperation(_)
    ));

    let archived = store.retire_operation(&completed.operation_id).unwrap();
    assert_eq!(archived.operation_id, completed.operation_id);
    assert_eq!(archived.state, OperationState::Completed);
    assert!(archived.stash.is_none());
    let record = walk_record(&repo.state);
    assert!(!record.exists());
    let archived_path = record
        .parent()
        .unwrap()
        .join("archive")
        .join(&archived.archive_file_name);
    let archived_bytes = fs::read(&archived_path).unwrap();
    assert!(
        archived_bytes
            .windows(completed.operation_id.len())
            .any(|window| window == completed.operation_id.as_bytes())
    );

    let second = ready(&store, &base);
    assert_ne!(second.operation_id, completed.operation_id);
}

#[test]
fn plan_validation_rejects_missing_duplicate_foreign_and_invalid_fold_inputs() {
    let repo = Repo::new();
    repo.write("base", "base");
    let base = repo.commit("base");
    repo.write("a", "a");
    repo.commit("a");
    repo.write("b", "b");
    repo.commit("b");
    let preparation = ready(&repo.store(), &base);
    let commits = &preparation.inventory.commits;
    for candidate in [
        vec![PlanStep {
            commit_oid: commits[0].oid.clone(),
            action: PlanAction::Pick,
        }],
        vec![
            PlanStep {
                commit_oid: commits[0].oid.clone(),
                action: PlanAction::Pick,
            },
            PlanStep {
                commit_oid: commits[0].oid.clone(),
                action: PlanAction::Drop,
            },
        ],
        vec![
            PlanStep {
                commit_oid: base.clone(),
                action: PlanAction::Pick,
            },
            PlanStep {
                commit_oid: commits[1].oid.clone(),
                action: PlanAction::Pick,
            },
        ],
        vec![
            PlanStep {
                commit_oid: commits[0].oid.clone(),
                action: PlanAction::Squash,
            },
            PlanStep {
                commit_oid: commits[1].oid.clone(),
                action: PlanAction::Pick,
            },
        ],
        vec![
            PlanStep {
                commit_oid: commits[0].oid.clone(),
                action: PlanAction::Reword {
                    message: String::new(),
                },
            },
            PlanStep {
                commit_oid: commits[1].oid.clone(),
                action: PlanAction::Pick,
            },
        ],
    ] {
        assert!(RebasePlan::validate(&preparation.inventory, candidate).is_err());
    }
    let injected_oid = format!("{} exec touch pwned", commits[0].oid);
    assert!(
        RebasePlan::validate(
            &preparation.inventory,
            vec![
                PlanStep {
                    commit_oid: injected_oid,
                    action: PlanAction::Pick
                },
                PlanStep {
                    commit_oid: commits[1].oid.clone(),
                    action: PlanAction::Pick
                },
            ]
        )
        .is_err()
    );
}

#[test]
fn merge_dirty_detached_and_empty_histories_are_explicit() {
    let repo = Repo::new();
    repo.write("base", "base");
    let base = repo.commit("base");
    run_ok(&repo.root, ["switch", "-c", "side"]);
    repo.write("side", "side");
    repo.commit("side");
    run_ok(&repo.root, ["switch", "topic"]);
    repo.write("topic", "topic");
    repo.commit("topic");
    run_ok(&repo.root, ["merge", "--no-ff", "side", "-m", "merge"]);
    match repo.store().prepare(&base).unwrap() {
        PrepareOutcome::ExternalWorkflow(flow) => assert!(matches!(
            flow.reason,
            UnsupportedHistory::ContainsMerge {
                parent_count: 2,
                ..
            }
        )),
        other => panic!("expected merge refusal, got {other:?}"),
    }

    let detached = Repo::new();
    detached.write("x", "x");
    let detached_base = detached.commit("base");
    detached.write("y", "y");
    detached.commit("tip");
    run_ok(&detached.root, ["checkout", "--detach"]);
    assert!(
        detached
            .store()
            .prepare(&detached_base)
            .unwrap_err()
            .to_string()
            .contains("detached")
    );

    let empty = Repo::new();
    empty.write("x", "x");
    let only = empty.commit("only");
    assert!(
        empty
            .store()
            .prepare(&only)
            .unwrap_err()
            .to_string()
            .contains("empty")
    );

    let unborn = Repo::new();
    assert!(
        unborn
            .store()
            .prepare(&"0".repeat(40))
            .unwrap_err()
            .to_string()
            .contains("unborn")
    );
}

fn conflicting_reorder() -> (
    Repo,
    String,
    RebaseStore,
    rebase::RebasePreparation,
    RebasePlan,
) {
    let repo = Repo::new();
    repo.write("file", "zero\n");
    let base = repo.commit("base");
    repo.write("file", "one\n");
    repo.commit("one");
    repo.write("file", "two\n");
    repo.commit("two");
    let store = repo.store();
    let preparation = ready(&store, &base);
    let plan = steps(
        &preparation.inventory,
        vec![(1, PlanAction::Pick), (0, PlanAction::Drop)],
    );
    (repo, base, store, preparation, plan)
}

#[test]
fn conflict_exposes_exact_stages_and_guarded_resolution_then_continue() {
    let (repo, base, store, preparation, plan) = conflicting_reorder();
    let mut view = store.start(&preparation, &plan).unwrap();
    assert_eq!(view.state, OperationState::Conflicted);
    let active = view.active.clone().unwrap();
    let conflicts = store.conflicts(&active).unwrap();
    assert_eq!(conflicts.len(), 1);
    let conflict = &conflicts[0];
    assert!(
        conflict.base.oid.is_some() && conflict.ours.oid.is_some() && conflict.theirs.oid.is_some()
    );
    assert!(matches!(conflict.base.content, BlobContent::Utf8(_)));
    repo.write("file", "two\n");
    let guard = repo.snapshot().guard;
    assert!(
        store
            .stage_resolution(&view.operation_id, &active, conflict, &guard)
            .is_err()
    );
    // Refresh both the disk/index generation and guard, then stage only this path.
    let refreshed = store.conflicts(&active).unwrap();
    view = store
        .stage_resolution(
            &view.operation_id,
            &active,
            &refreshed[0],
            &repo.snapshot().guard,
        )
        .unwrap();
    assert_eq!(view.state, OperationState::Running);
    view = store
        .continue_rebase(
            &view.operation_id,
            view.active.as_ref().unwrap(),
            &repo.snapshot().guard,
        )
        .unwrap();
    assert_eq!(view.state, OperationState::Completed);
    assert!(
        run(&repo.root, ["merge-base", "--is-ancestor", &base, "HEAD"])
            .status
            .success()
    );
}

fn byte_conflict(path: &str) -> (Repo, RebaseStore, rebase::OperationView) {
    let repo = Repo::new();
    repo.write_bytes(path, &[0, 1]);
    let base = repo.commit("base");
    repo.write_bytes(path, &[0, 2]);
    repo.commit("one");
    repo.write_bytes(path, &[0, 3]);
    repo.commit("two");
    let store = repo.store();
    let preparation = ready(&store, &base);
    let plan = steps(
        &preparation.inventory,
        vec![(1, PlanAction::Pick), (0, PlanAction::Drop)],
    );
    let view = store.start(&preparation, &plan).unwrap();
    (repo, store, view)
}

#[test]
fn binary_media_symlink_and_delete_conflicts_are_explained_without_text_editing() {
    let (_repo, store, view) = byte_conflict("opaque.bin");
    let conflicts = store.conflicts(view.active.as_ref().unwrap()).unwrap();
    assert!(matches!(conflicts[0].base.content, BlobContent::Binary));
    assert!(matches!(conflicts[0].ours.content, BlobContent::Binary));
    assert!(matches!(conflicts[0].theirs.content, BlobContent::Binary));

    let (_repo, store, view) = byte_conflict("image.png");
    let conflicts = store.conflicts(view.active.as_ref().unwrap()).unwrap();
    assert!(matches!(conflicts[0].base.content, BlobContent::Media));

    let deleted = Repo::new();
    deleted.write("file", "base\n");
    let base = deleted.commit("base");
    deleted.write("file", "modified\n");
    deleted.commit("modify");
    fs::remove_file(deleted.root.join("file")).unwrap();
    deleted.commit("delete");
    let store = deleted.store();
    let preparation = ready(&store, &base);
    let plan = steps(
        &preparation.inventory,
        vec![(1, PlanAction::Pick), (0, PlanAction::Drop)],
    );
    let view = store.start(&preparation, &plan).unwrap();
    let conflicts = store.conflicts(view.active.as_ref().unwrap()).unwrap();
    assert!(matches!(
        conflicts[0].kind,
        rebase::ConflictKind::DeletedByOurs
            | rebase::ConflictKind::DeletedByTheirs
            | rebase::ConflictKind::RenameOrDelete { .. }
    ));

    use std::os::unix::fs::symlink;
    let linked = Repo::new();
    symlink("base-target", linked.root.join("link")).unwrap();
    let base = linked.commit("base");
    fs::remove_file(linked.root.join("link")).unwrap();
    symlink("one-target", linked.root.join("link")).unwrap();
    linked.commit("one");
    fs::remove_file(linked.root.join("link")).unwrap();
    symlink("two-target", linked.root.join("link")).unwrap();
    linked.commit("two");
    let store = linked.store();
    let preparation = ready(&store, &base);
    let plan = steps(
        &preparation.inventory,
        vec![(1, PlanAction::Pick), (0, PlanAction::Drop)],
    );
    let view = store.start(&preparation, &plan).unwrap();
    let conflicts = store.conflicts(view.active.as_ref().unwrap()).unwrap();
    assert!(matches!(conflicts[0].base.content, BlobContent::Symlink(_)));
}

#[test]
fn continue_skip_abort_and_external_transitions_follow_git_state() {
    // Continue with an external editor resolution and backend staging.
    let (repo, _, store, preparation, plan) = conflicting_reorder();
    let view = store.start(&preparation, &plan).unwrap();
    let active = view.active.clone().unwrap();
    repo.write("file", "two\n");
    run_ok(&repo.root, ["add", "file"]);
    let guard = repo.snapshot().guard;
    let completed = store
        .continue_rebase(&view.operation_id, &active, &guard)
        .unwrap();
    assert_eq!(completed.state, OperationState::Completed);
    assert_eq!(fs::read_to_string(repo.root.join("file")).unwrap(), "two\n");

    let (repo, _, store, preparation, plan) = conflicting_reorder();
    let view = store.start(&preparation, &plan).unwrap();
    let skipped = store
        .skip(
            &view.operation_id,
            view.active.as_ref().unwrap(),
            &repo.snapshot().guard,
        )
        .unwrap();
    assert_eq!(skipped.state, OperationState::Completed);
    assert_eq!(
        fs::read_to_string(repo.root.join("file")).unwrap(),
        "zero\n"
    );

    let (repo, _, store, preparation, plan) = conflicting_reorder();
    let original = preparation.inventory.head_oid.clone();
    let view = store.start(&preparation, &plan).unwrap();
    let aborted = store
        .abort(
            &view.operation_id,
            view.active.as_ref().unwrap(),
            &repo.snapshot().guard,
        )
        .unwrap();
    assert_eq!(aborted.state, OperationState::Aborted);
    assert_eq!(repo.oid("HEAD"), original);

    let (repo, _, store, preparation, plan) = conflicting_reorder();
    let _view = store.start(&preparation, &plan).unwrap();
    run_ok(&repo.root, ["rebase", "--abort"]);
    assert_eq!(
        store.observe().unwrap().unwrap().state,
        OperationState::Aborted
    );
    assert_eq!(repo.oid("HEAD"), preparation.inventory.head_oid);
}

#[test]
fn edit_amend_and_real_two_commit_split_conserve_trees() {
    let amend = Repo::new();
    amend.write("base", "base");
    let base = amend.commit("base");
    amend.write("file", "original\n");
    amend.commit("edit me");
    let store = amend.store();
    let preparation = ready(&store, &base);
    let plan = steps(&preparation.inventory, vec![(0, PlanAction::Edit)]);
    let view = store.start(&preparation, &plan).unwrap();
    assert_eq!(view.state, OperationState::PausedForEdit);
    amend.write("file", "amended\n");
    run_ok(&amend.root, ["add", "file"]);
    let amended = store
        .amend_at_edit(
            &view.operation_id,
            view.active.as_ref().unwrap(),
            &amend.snapshot().guard,
            Some("amended message"),
        )
        .unwrap();
    let finished = store
        .continue_rebase(
            &view.operation_id,
            amended.active.as_ref().unwrap(),
            &amend.snapshot().guard,
        )
        .unwrap();
    assert_eq!(finished.state, OperationState::Completed);
    assert_eq!(
        text(run_ok(&amend.root, ["log", "-1", "--format=%s"])).trim(),
        "amended message"
    );

    let split = Repo::new();
    split.write("base", "base");
    let base = split.commit("base");
    split.write("a", "a");
    split.write("b", "b");
    split.commit("split me");
    let expected_tree = split.oid("HEAD^{tree}");
    let store = split.store();
    let preparation = ready(&store, &base);
    let plan = steps(&preparation.inventory, vec![(0, PlanAction::Edit)]);
    let view = store.start(&preparation, &plan).unwrap();
    split.write("unrelated", "preserve me");
    assert!(
        store
            .begin_split(
                &view.operation_id,
                view.active.as_ref().unwrap(),
                &split.snapshot().guard,
            )
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(split.root.join("unrelated")).unwrap(),
        "preserve me"
    );
    fs::remove_file(split.root.join("unrelated")).unwrap();
    let view = store
        .begin_split(
            &view.operation_id,
            view.active.as_ref().unwrap(),
            &split.snapshot().guard,
        )
        .unwrap();
    let split_state = view.split.as_ref().unwrap();
    assert_eq!(
        split_state.stopped_oid,
        preparation.inventory.commits[0].oid
    );
    assert!(split_state.rewritten_stopped_oid.is_some());
    assert_eq!(split_state.required_tree_oid, expected_tree);
    assert_eq!(split_state.replacement_commit_count, Some(0));
    let record_path = walk_record(&split.state);
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    envelope["payload"]["action"] = "BeginSplit".into();
    let payload = serde_json::to_vec(&envelope["payload"]).unwrap();
    envelope["checksum"] = format!("{:x}", Sha256::digest(payload)).into();
    fs::write(&record_path, serde_json::to_vec(&envelope).unwrap()).unwrap();
    assert!(
        store
            .retire_operation(&view.operation_id)
            .unwrap_err()
            .to_string()
            .contains("active Git operation")
    );
    let head_after_reset = split.oid("HEAD");
    assert!(
        store
            .continue_rebase(
                &view.operation_id,
                view.active.as_ref().unwrap(),
                &split.snapshot().guard,
            )
            .unwrap_err()
            .to_string()
            .contains("finish_split")
    );
    assert!(
        store
            .skip(
                &view.operation_id,
                view.active.as_ref().unwrap(),
                &split.snapshot().guard,
            )
            .unwrap_err()
            .to_string()
            .contains("finish_split")
    );
    assert_eq!(split.oid("HEAD"), head_after_reset);
    assert!(
        store
            .finish_split(
                &view.operation_id,
                view.active.as_ref().unwrap(),
                &split.snapshot().guard,
            )
            .is_err()
    );
    drop(store);
    let store = split.store();
    let view = store.observe().unwrap().unwrap();
    assert_eq!(view.state, OperationState::PausedForEdit);
    assert_eq!(
        view.split.as_ref().unwrap().replacement_commit_count,
        Some(0)
    );
    run_ok(&split.root, ["add", "a"]);
    store
        .commit_split_part(
            &view.operation_id,
            view.active.as_ref().unwrap(),
            &split.snapshot().guard,
            "part a",
        )
        .unwrap();
    drop(store);
    let store = split.store();
    let view = store.observe().unwrap().unwrap();
    assert_eq!(
        view.split.as_ref().unwrap().replacement_commit_count,
        Some(1)
    );
    run_ok(&split.root, ["add", "b"]);
    let view = store
        .commit_split_part(
            &view.operation_id,
            view.active.as_ref().unwrap(),
            &split.snapshot().guard,
            "part b",
        )
        .unwrap();
    let completed = store
        .finish_split(
            &view.operation_id,
            view.active.as_ref().unwrap(),
            &split.snapshot().guard,
        )
        .unwrap();
    assert_eq!(completed.state, OperationState::Completed);
    assert_eq!(split.oid("HEAD^{tree}"), expected_tree);
    assert_eq!(
        text(run_ok(
            &split.root,
            ["rev-list", "--count", &format!("{base}..HEAD")]
        ))
        .trim(),
        "2"
    );

    let changed = Repo::new();
    changed.write("base", "base");
    let base = changed.commit("base");
    changed.write("file", "edit");
    changed.commit("edit");
    let store = changed.store();
    let preparation = ready(&store, &base);
    let plan = steps(&preparation.inventory, vec![(0, PlanAction::Edit)]);
    let edit = store.start(&preparation, &plan).unwrap();
    run_ok(
        &changed.root,
        ["commit", "--allow-empty", "-m", "external edit-stop commit"],
    );
    assert!(
        store
            .begin_split(
                &edit.operation_id,
                edit.active.as_ref().unwrap(),
                &changed.snapshot().guard,
            )
            .is_err()
    );
    assert_eq!(
        store.observe().unwrap().unwrap().state,
        OperationState::FailedUncertain
    );

    let aborted = Repo::new();
    aborted.write("base", "base");
    let base = aborted.commit("base");
    aborted.write("file", "edit");
    aborted.commit("edit");
    let store = aborted.store();
    let preparation = ready(&store, &base);
    let plan = steps(&preparation.inventory, vec![(0, PlanAction::Edit)]);
    let edit = store.start(&preparation, &plan).unwrap();
    let split_view = store
        .begin_split(
            &edit.operation_id,
            edit.active.as_ref().unwrap(),
            &aborted.snapshot().guard,
        )
        .unwrap();
    let aborted_view = store
        .abort(
            &split_view.operation_id,
            split_view.active.as_ref().unwrap(),
            &aborted.snapshot().guard,
        )
        .unwrap();
    assert_eq!(aborted_view.state, OperationState::Aborted);
    assert!(aborted_view.split.is_none());
}

#[test]
fn exact_stash_restore_ignores_newer_stash_and_preserves_untracked() {
    let repo = Repo::new();
    repo.write("file", "base\n");
    repo.write(".gitignore", "ignored\n");
    let base = repo.commit("base");
    repo.write("topic", "topic\n");
    repo.commit("topic");
    repo.write("file", "dirty\n");
    repo.write("untracked", "saved\n");
    repo.write("ignored", "outside stash\n");
    let store = repo.store();
    let dirty = match store.prepare(&base).unwrap() {
        PrepareOutcome::Dirty(value) => value,
        other => panic!("{other:?}"),
    };
    assert!(dirty.stash_includes_untracked && dirty.ignored_files_remain_outside_stash);
    let preparation = store.create_stash(&dirty).unwrap();
    let saved_oid = preparation.stash.as_ref().unwrap().oid.clone();
    let plan = steps(
        &preparation.inventory,
        vec![(
            0,
            PlanAction::Reword {
                message: "rewritten".into(),
            },
        )],
    );
    let completed = store.start(&preparation, &plan).unwrap();
    assert_eq!(completed.state, OperationState::Completed);
    assert_eq!(
        fs::read_to_string(repo.root.join("file")).unwrap(),
        "base\n"
    );
    assert!(!repo.root.join("untracked").exists());
    assert_eq!(
        fs::read_to_string(repo.root.join("ignored")).unwrap(),
        "outside stash\n"
    );
    repo.write("file", "newer stash\n");
    run_ok(&repo.root, ["stash", "push", "-m", "newer"]);
    let newer = repo.oid("refs/stash");
    assert_ne!(newer, saved_oid);
    assert!(
        store
            .retire_operation(&completed.operation_id)
            .unwrap_err()
            .to_string()
            .contains("undisposed stash")
    );
    let restored = store
        .restore_stash(&completed.operation_id, &repo.snapshot().guard)
        .unwrap();
    assert_eq!(restored.stash_restore, StashRestoreState::Completed);
    assert_eq!(
        fs::read_to_string(repo.root.join("file")).unwrap(),
        "dirty\n"
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("untracked")).unwrap(),
        "saved\n"
    );
    assert_eq!(repo.oid("refs/stash"), newer);
    assert!(
        run(&repo.root, ["cat-file", "-e", &saved_oid])
            .status
            .success()
    );
    let archived = store.retire_operation(&completed.operation_id).unwrap();
    assert_eq!(archived.stash.as_ref().unwrap().oid, saved_oid);
    assert_eq!(archived.stash_restore, StashRestoreState::Completed);
    let archive = walk_record(&repo.state)
        .parent()
        .unwrap()
        .join("archive")
        .join(archived.archive_file_name);
    assert!(fs::read_to_string(archive).unwrap().contains(&saved_oid));
    assert!(matches!(
        store.prepare(&base).unwrap(),
        PrepareOutcome::Dirty(_)
    ));
}

#[test]
fn stash_acknowledgement_selects_own_object_when_competitor_moves_ref() {
    let repo = Repo::new();
    repo.write("file", "base\n");
    let base = repo.commit("base");
    repo.write("topic", "topic\n");
    repo.commit("topic");
    repo.write("file", "own dirty state\n");
    repo.write("own-untracked", "own untracked state\n");
    let store = repo.store();
    let dirty = match store.prepare(&base).unwrap() {
        PrepareOutcome::Dirty(value) => value,
        other => panic!("{other:?}"),
    };

    let preparation = store
        .create_stash_with_post_command_hook(&dirty, || {
            repo.write("file", "competitor dirty state\n");
            run_ok(&repo.root, ["stash", "push", "-m", "competitor"]);
        })
        .unwrap();
    let own_oid = preparation.stash.as_ref().unwrap().oid.clone();
    let competitor_oid = repo.oid("refs/stash");
    assert_ne!(own_oid, competitor_oid);
    assert!(
        text(run_ok(&repo.root, ["show", "-s", "--format=%B", &own_oid]))
            .contains(&format!("cibergit-rebase:{}", preparation.operation_id))
    );
    assert_eq!(
        repo.oid(&format!("{own_oid}^1")),
        preparation.inventory.head_oid
    );

    let plan = steps(
        &preparation.inventory,
        vec![(
            0,
            PlanAction::Reword {
                message: "competitor stash rebase".into(),
            },
        )],
    );
    let completed = store.start(&preparation, &plan).unwrap();
    store
        .restore_stash(&completed.operation_id, &repo.snapshot().guard)
        .unwrap();
    assert_eq!(
        fs::read_to_string(repo.root.join("file")).unwrap(),
        "own dirty state\n"
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("own-untracked")).unwrap(),
        "own untracked state\n"
    );
    assert_eq!(repo.oid("refs/stash"), competitor_oid);
}

#[test]
fn duplicate_stash_nonce_is_uncertain_and_never_selects_competitor() {
    let repo = Repo::new();
    repo.write("file", "base\n");
    let base = repo.commit("base");
    repo.write("topic", "topic\n");
    repo.commit("topic");
    repo.write("file", "own dirty state\n");
    let store = repo.store();
    let dirty = match store.prepare(&base).unwrap() {
        PrepareOutcome::Dirty(value) => value,
        other => panic!("{other:?}"),
    };
    let marker = format!("cibergit-rebase:{}", dirty.operation_id);
    let error = store
        .create_stash_with_post_command_hook(&dirty, || {
            repo.write("file", "competing dirty state\n");
            run_ok(&repo.root, ["stash", "push", "-u", "-m", &marker]);
        })
        .unwrap_err();
    assert!(error.to_string().contains("partially"));
    assert_eq!(
        store.observe().unwrap().unwrap().state,
        OperationState::FailedUncertain
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("file")).unwrap(),
        "base\n"
    );
}

#[test]
fn stash_restore_conflict_is_explicit_and_stash_is_retained() {
    let repo = Repo::new();
    repo.write("file", "base\n");
    let base = repo.commit("base");
    repo.write("file", "topic\n");
    repo.commit("topic");
    repo.write("file", "dirty based on topic\n");
    let store = repo.store();
    let dirty = match store.prepare(&base).unwrap() {
        PrepareOutcome::Dirty(value) => value,
        other => panic!("{other:?}"),
    };
    let preparation = store.create_stash(&dirty).unwrap();
    let stash = preparation.stash.as_ref().unwrap().oid.clone();
    let plan = steps(&preparation.inventory, vec![(0, PlanAction::Drop)]);
    let completed = store.start(&preparation, &plan).unwrap();
    let restored = store
        .restore_stash(&completed.operation_id, &repo.snapshot().guard)
        .unwrap();
    assert_eq!(restored.stash_restore, StashRestoreState::Conflicted);
    assert!(!repo.snapshot().conflicts.is_empty());
    assert!(
        store
            .retire_operation(&completed.operation_id)
            .unwrap_err()
            .to_string()
            .contains("active split or stash restoration")
    );
    let conflicts = store.stash_conflicts(&completed.operation_id).unwrap();
    assert_eq!(conflicts.len(), 1);
    repo.write("file", "dirty based on topic\n");
    let refreshed = store.stash_conflicts(&completed.operation_id).unwrap();
    store
        .stage_stash_resolution(
            &completed.operation_id,
            &refreshed[0],
            &repo.snapshot().guard,
        )
        .unwrap();
    let finished = store
        .finish_stash_restore(&completed.operation_id, &repo.snapshot().guard)
        .unwrap();
    assert_eq!(finished.stash_restore, StashRestoreState::Completed);
    assert!(run(&repo.root, ["cat-file", "-e", &stash]).status.success());
}

#[test]
fn interrupted_restore_after_completion_is_uncertain_and_retains_receipt() {
    let repo = Repo::new();
    repo.write("file", "base\n");
    let base = repo.commit("base");
    repo.write("topic", "topic\n");
    repo.commit("topic");
    repo.write("file", "dirty\n");
    let store = repo.store();
    let dirty = match store.prepare(&base).unwrap() {
        PrepareOutcome::Dirty(value) => value,
        other => panic!("{other:?}"),
    };
    let preparation = store.create_stash(&dirty).unwrap();
    let stash_oid = preparation.stash.as_ref().unwrap().oid.clone();
    let plan = steps(
        &preparation.inventory,
        vec![(
            0,
            PlanAction::Reword {
                message: "completed before interrupted restore".into(),
            },
        )],
    );
    let completed = store.start(&preparation, &plan).unwrap();
    store
        .persist_restore_stash_intent_for_test(&completed.operation_id)
        .unwrap();
    drop(store);

    let reopened = repo.store();
    let observed = reopened.observe().unwrap().unwrap();
    assert_eq!(observed.state, OperationState::FailedUncertain);
    assert_eq!(observed.stash_restore, StashRestoreState::FailedUncertain);
    assert_eq!(observed.stash.as_ref().unwrap().oid, stash_oid);
    assert!(
        run(&repo.root, ["cat-file", "-e", &stash_oid])
            .status
            .success()
    );
    assert!(
        reopened
            .retire_operation(&observed.operation_id)
            .unwrap_err()
            .to_string()
            .contains("authoritative concrete outcome proof")
    );
    assert_eq!(
        reopened.observe().unwrap().unwrap().state,
        OperationState::FailedUncertain
    );
}

#[test]
fn restart_external_continue_and_same_path_replacement_are_detected() {
    let repo = Repo::new();
    repo.write("base", "base");
    let base = repo.commit("base");
    repo.write("a", "a");
    repo.commit("edit");
    let store = repo.store();
    let preparation = ready(&store, &base);
    let plan = steps(&preparation.inventory, vec![(0, PlanAction::Edit)]);
    let _view = store.start(&preparation, &plan).unwrap();
    drop(store);
    let reopened = repo.store();
    assert_eq!(
        reopened.observe().unwrap().unwrap().state,
        OperationState::PausedForEdit
    );
    run_ok(
        &repo.root,
        ["-c", "core.editor=true", "rebase", "--continue"],
    );
    assert_eq!(
        reopened.observe().unwrap().unwrap().state,
        OperationState::Completed
    );

    let old = repo.temp.path().join("old checkout");
    fs::rename(&repo.root, &old).unwrap();
    fs::create_dir(&repo.root).unwrap();
    run_ok(&repo.root, ["init", "-b", "topic"]);
    run_ok(&repo.root, ["config", "user.name", "Replacement"]);
    run_ok(
        &repo.root,
        ["config", "user.email", "replacement@example.invalid"],
    );
    repo.write("replacement", "replacement");
    repo.commit("replacement");
    assert!(
        RebaseStore::open(&repo.state, association(), &repo.root)
            .unwrap_err()
            .to_string()
            .contains("replaced checkout")
    );
}

#[test]
fn stale_dirty_content_locks_ref_movement_and_timeout_never_replay() {
    let repo = Repo::new();
    repo.write("base", "base");
    let base = repo.commit("base");
    repo.write("a", "a");
    repo.commit("a");
    repo.write("dirty", "one");
    let store = repo.store();
    let dirty = match store.prepare(&base).unwrap() {
        PrepareOutcome::Dirty(value) => value,
        other => panic!("{other:?}"),
    };
    repo.write("dirty", "two");
    assert!(
        store
            .create_stash(&dirty)
            .unwrap_err()
            .to_string()
            .contains("changed since")
    );
    assert!(repo.root.join("dirty").exists());
    assert!(store.observe().unwrap().is_none());
    assert!(matches!(
        store.prepare(&base).unwrap(),
        PrepareOutcome::Dirty(_)
    ));

    let locked = Repo::new();
    locked.write("base", "base");
    let base = locked.commit("base");
    locked.write("a", "a");
    locked.commit("a");
    let store = locked.store();
    let preparation = ready(&store, &base);
    let plan = steps(
        &preparation.inventory,
        vec![(
            0,
            PlanAction::Reword {
                message: "index lock retry".into(),
            },
        )],
    );
    fs::write(locked.root.join(".git/index.lock"), "lock").unwrap();
    assert!(
        store
            .start(&preparation, &plan)
            .unwrap_err()
            .to_string()
            .contains("blocked")
    );
    fs::remove_file(locked.root.join(".git/index.lock")).unwrap();
    assert!(store.observe().unwrap().is_none());
    assert_eq!(locked.oid("HEAD"), preparation.inventory.head_oid);
    assert_eq!(
        store.start(&preparation, &plan).unwrap().state,
        OperationState::Completed
    );

    let moved = Repo::new();
    moved.write("base", "base");
    let base = moved.commit("base");
    moved.write("a", "a");
    moved.commit("a");
    let store = moved.store();
    let preparation = ready(&store, &base);
    let plan = steps(&preparation.inventory, vec![(0, PlanAction::Pick)]);
    run_ok(&moved.root, ["commit", "--allow-empty", "-m", "external"]);
    assert!(
        store
            .start(&preparation, &plan)
            .unwrap_err()
            .to_string()
            .contains("changed since")
    );
    assert!(store.observe().unwrap().is_none());
    assert_eq!(moved.oid("HEAD"), moved.oid("refs/heads/topic"));
    assert!(matches!(
        store.prepare(&base).unwrap(),
        PrepareOutcome::Ready(_)
    ));

    let timed = Repo::new();
    timed.write("base", "base");
    let base = timed.commit("base");
    timed.write("a", "a");
    timed.commit("a");
    let hooks = timed.root.join(".git/hooks");
    let hook = hooks.join("pre-rebase");
    fs::write(&hook, "#!/bin/sh\nsleep 2\n").unwrap();
    let mut permissions = fs::metadata(&hook).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o700);
    fs::set_permissions(&hook, permissions).unwrap();
    let git = LocalGit::open_with_limits(
        &timed.root,
        CommandLimits {
            deadline: Duration::from_millis(50),
            max_output_bytes: 1024 * 1024,
            max_input_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    let store = RebaseStore::from_local_git(&timed.state, association(), git).unwrap();
    let preparation = ready(&store, &base);
    let plan = steps(&preparation.inventory, vec![(0, PlanAction::Pick)]);
    assert!(
        store
            .start(&preparation, &plan)
            .unwrap_err()
            .to_string()
            .contains("partially")
    );
    assert_eq!(
        store.observe().unwrap().unwrap().state,
        OperationState::FailedUncertain
    );
    assert_eq!(timed.oid("HEAD"), preparation.inventory.head_oid);
    let uncertain_id = store.observe().unwrap().unwrap().operation_id;
    assert!(
        store
            .retire_operation(&uncertain_id)
            .unwrap_err()
            .to_string()
            .contains("authoritative concrete outcome proof")
    );
}

#[test]
fn active_branch_ref_movement_and_external_branch_switch_are_not_blessed() {
    let repo = Repo::new();
    repo.write("base", "base");
    let base = repo.commit("base");
    repo.write("a", "a");
    repo.commit("edit");
    let store = repo.store();
    let preparation = ready(&store, &base);
    let plan = steps(&preparation.inventory, vec![(0, PlanAction::Edit)]);
    store.start(&preparation, &plan).unwrap();
    run_ok(&repo.root, ["update-ref", "refs/heads/topic", &base]);
    assert_eq!(
        store.observe().unwrap().unwrap().state,
        OperationState::FailedUncertain
    );

    let switched = Repo::new();
    switched.write("base", "base");
    let base = switched.commit("base");
    switched.write("a", "a");
    switched.commit("pick");
    run_ok(&switched.root, ["branch", "other", &base]);
    let store = switched.store();
    let preparation = ready(&store, &base);
    let plan = steps(&preparation.inventory, vec![(0, PlanAction::Pick)]);
    assert_eq!(
        store.start(&preparation, &plan).unwrap().state,
        OperationState::Completed
    );
    run_ok(&switched.root, ["switch", "other"]);
    assert_eq!(
        store.observe().unwrap().unwrap().state,
        OperationState::FailedUncertain
    );

    let replaced = Repo::new();
    replaced.write("base", "base");
    let base = replaced.commit("base");
    replaced.write("a", "a");
    replaced.commit("edit");
    let store = replaced.store();
    let preparation = ready(&store, &base);
    let plan = steps(&preparation.inventory, vec![(0, PlanAction::Edit)]);
    store.start(&preparation, &plan).unwrap();
    fs::write(
        replaced.root.join(".git/rebase-merge/cibergit-operation"),
        "unrelated\n",
    )
    .unwrap();
    assert_eq!(
        store.observe().unwrap().unwrap().state,
        OperationState::FailedUncertain
    );
}

#[test]
fn raw_non_utf8_conflict_path_and_stage_ids_remain_addressable() {
    use std::os::unix::ffi::OsStringExt;
    let repo = Repo::new();
    let raw_name = std::ffi::OsString::from_vec(b"f\nname".to_vec());
    fs::write(repo.root.join(&raw_name), [0xfe]).unwrap();
    let base = repo.commit("base");
    fs::write(repo.root.join(&raw_name), [0xfd]).unwrap();
    repo.commit("one");
    fs::write(repo.root.join(&raw_name), [0xfc]).unwrap();
    repo.commit("two");
    let store = repo.store();
    let preparation = ready(&store, &base);
    let plan = steps(
        &preparation.inventory,
        vec![(1, PlanAction::Pick), (0, PlanAction::Drop)],
    );
    let view = store.start(&preparation, &plan).unwrap();
    let conflicts = store.conflicts(view.active.as_ref().unwrap()).unwrap();
    assert_eq!(conflicts[0].path.raw, b"f\nname");
    assert!(matches!(conflicts[0].theirs.content, BlobContent::NonUtf8));
    let raw = GitPath::from_raw(vec![b'x', 0xff]).unwrap();
    assert!(raw.display.contains("\\xff"));
    let os = std::ffi::OsString::from_vec(raw.raw);
    use std::os::unix::ffi::OsStrExt;
    assert_eq!(os.as_os_str().as_bytes(), &[b'x', 0xff]);
}

#[test]
fn record_manifest_checksum_shape_is_stable_for_lost_ack_reconciliation() {
    let repo = Repo::new();
    repo.write("base", "base");
    let base = repo.commit("base");
    repo.write("a", "a");
    repo.commit("edit");
    let store = repo.store();
    let preparation = ready(&store, &base);
    let plan = steps(&preparation.inventory, vec![(0, PlanAction::Edit)]);
    let view = store.start(&preparation, &plan).unwrap();
    let record = walk_record(&repo.state);
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&fs::read(&record).unwrap()).unwrap();
    envelope["payload"]["dispatch_acknowledged"] = false.into();
    let payload = serde_json::to_vec(&envelope["payload"]).unwrap();
    envelope["checksum"] = format!("{:x}", Sha256::digest(payload)).into();
    fs::write(&record, serde_json::to_vec(&envelope).unwrap()).unwrap();
    drop(store);
    let observed = repo.store().observe().unwrap().unwrap();
    assert_eq!(observed.operation_id, view.operation_id);
    assert_eq!(observed.state, OperationState::PausedForEdit);
}

#[test]
fn journal_authority_serializes_separate_processes() {
    const CHILD: &str = "CIBERGIT_REBASE_JOURNAL_TEST_CHILD";
    if env::var_os(CHILD).is_some() {
        let state = PathBuf::from(env::var_os("CIBERGIT_REBASE_TEST_STATE").unwrap());
        let root = PathBuf::from(env::var_os("CIBERGIT_REBASE_TEST_ROOT").unwrap());
        let acquired = PathBuf::from(env::var_os("CIBERGIT_REBASE_TEST_ACQUIRED").unwrap());
        assert!(matches!(
            RebaseStore::open(state, association(), root),
            Err(RebaseError::JournalBusy)
        ));
        fs::write(acquired, b"journal busy observed").unwrap();
        return;
    }

    let repo = Repo::new();
    repo.write("base", "base\n");
    let base = repo.commit("base");
    repo.write("topic", "topic\n");
    repo.commit("topic");
    let store = repo.store();
    let parent_acquired = repo.temp.path().join("parent acquired");
    let release = repo.temp.path().join("release parent");
    let child_acquired = repo.temp.path().join("child acquired");
    let holder = store.clone();
    let held_path = parent_acquired.clone();
    let release_path = release.clone();
    let holder_thread = thread::spawn(move || {
        holder
            .hold_record_lock_for_test(&held_path, &release_path)
            .unwrap();
    });
    wait_for_path(&parent_acquired);

    let started = Instant::now();
    let mut child = Command::new(env::current_exe().unwrap())
        .arg("--exact")
        .arg("journal_authority_serializes_separate_processes")
        .arg("--nocapture")
        .env(CHILD, "1")
        .env("CIBERGIT_REBASE_TEST_STATE", &repo.state)
        .env("CIBERGIT_REBASE_TEST_ROOT", &repo.root)
        .env("CIBERGIT_REBASE_TEST_ACQUIRED", &child_acquired)
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(250));
    assert!(!child_acquired.exists());
    assert!(child.try_wait().unwrap().is_none());

    let status = child.wait().unwrap();
    assert!(status.success());
    assert!(started.elapsed() >= Duration::from_secs(5));
    wait_for_path(&child_acquired);
    fs::write(&release, b"release").unwrap();
    holder_thread.join().unwrap();
    assert!(matches!(
        store.prepare(&base).unwrap(),
        PrepareOutcome::Ready(_)
    ));
}

#[test]
fn corrupt_and_future_records_are_refused_without_replacement() {
    let repo = Repo::new();
    repo.write("base", "base");
    let base = repo.commit("base");
    repo.write("a", "a");
    repo.commit("edit");
    let store = repo.store();
    let preparation = ready(&store, &base);
    let plan = steps(&preparation.inventory, vec![(0, PlanAction::Edit)]);
    store.start(&preparation, &plan).unwrap();
    let record = walk_record(&repo.state);

    let corrupt = b"{ definitely not valid json".to_vec();
    fs::write(&record, &corrupt).unwrap();
    assert!(
        RebaseStore::open(&repo.state, association(), &repo.root)
            .unwrap_err()
            .to_string()
            .contains("corrupt")
    );
    assert_eq!(fs::read(&record).unwrap(), corrupt);

    let future = br#"{"version":999,"payload":{},"checksum":"future"}"#.to_vec();
    fs::write(&record, &future).unwrap();
    assert!(
        RebaseStore::open(&repo.state, association(), &repo.root)
            .unwrap_err()
            .to_string()
            .contains("newer")
    );
    assert_eq!(fs::read(&record).unwrap(), future);
}

fn walk_record(root: &Path) -> PathBuf {
    fs::read_dir(root.join("rebase"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("operation.json")
}

fn wait_for_path(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timed out waiting for {path:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
