use cibergit::document::{
    ConflictKind, DiskState, DocumentError, DocumentLimits, DocumentStatus, DocumentStore,
    ReconcileOutcome, RecoveryScope, RecoveryStatus, RefreshOutcome, SaveOutcome, TargetIssue,
};
use std::{
    ffi::{CString, OsString, c_void},
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        macos::fs::MetadataExt as MacMetadataExt,
        raw::{c_char, c_int, c_uint},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

unsafe extern "C" {
    fn fsetxattr(
        fd: c_int,
        name: *const c_char,
        value: *const c_void,
        size: usize,
        position: c_uint,
        options: c_int,
    ) -> c_int;
    fn fgetxattr(
        fd: c_int,
        name: *const c_char,
        value: *mut c_void,
        size: usize,
        position: c_uint,
        options: c_int,
    ) -> isize;
}

fn set_xattr(path: &Path, name: &str, value: &[u8]) {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let name = CString::new(name).unwrap();
    let result = unsafe {
        fsetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    assert_eq!(
        result,
        0,
        "fsetxattr failed: {}",
        std::io::Error::last_os_error()
    );
}

fn get_xattr(path: &Path, name: &str) -> Vec<u8> {
    let file = File::open(path).unwrap();
    let name = CString::new(name).unwrap();
    let size = unsafe {
        fgetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            std::ptr::null_mut(),
            0,
            0,
            0,
        )
    };
    assert!(
        size >= 0,
        "fgetxattr size failed: {}",
        std::io::Error::last_os_error()
    );
    let mut value = vec![0_u8; size as usize];
    if value.is_empty() {
        return value;
    }
    let read = unsafe {
        fgetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    assert!(
        read >= 0,
        "fgetxattr failed: {}",
        std::io::Error::last_os_error()
    );
    value.truncate(read as usize);
    value
}

fn acl_entries(path: &Path) -> Vec<String> {
    let output = Command::new("ls").arg("-le").arg(path).output().unwrap();
    assert!(
        output.status.success(),
        "ls -le failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .skip(1)
        .map(str::trim)
        .map(str::to_owned)
        .collect()
}

struct Fixture {
    _temp: TempDir,
    worktree: PathBuf,
    recovery: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("worktree");
        let recovery = temp.path().join("recovery");
        fs::create_dir(&worktree).unwrap();
        Self {
            _temp: temp,
            worktree,
            recovery,
        }
    }

    fn write(&self, relative: impl AsRef<Path>, text: impl AsRef<[u8]>) {
        let path = self.worktree.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, text).unwrap();
    }

    fn store(&self) -> DocumentStore {
        DocumentStore::new(
            &self.worktree,
            &self.recovery,
            RecoveryScope::new("account-1", "repository-1"),
            DocumentLimits::default(),
        )
        .unwrap()
    }
}

#[test]
fn clean_external_edit_reloads_authoritative_disk() {
    let fixture = Fixture::new();
    fixture.write("src/main.rs", "old\n");
    let mut document = fixture.store().open("src/main.rs").unwrap();

    fixture.write("src/main.rs", "external\n");

    assert_eq!(document.refresh().unwrap(), RefreshOutcome::Reloaded);
    assert_eq!(document.buffer(), "external\n");
    assert_eq!(document.base().text, "external\n");
    assert_eq!(document.status(), DocumentStatus::Clean);
}

#[test]
fn dirty_external_edit_preserves_base_buffer_and_disk_until_reconciled() {
    let fixture = Fixture::new();
    fixture.write("notes.txt", "base\n");
    let mut document = fixture.store().open("notes.txt").unwrap();
    document.set_buffer("mine\n").unwrap();
    fixture.write("notes.txt", "theirs\n");

    assert_eq!(document.refresh().unwrap(), RefreshOutcome::Conflict);
    let conflict = document.conflict().unwrap();
    assert_eq!(conflict.kind, ConflictKind::ExternalEdit);
    assert_eq!(conflict.base.text, "base\n");
    assert_eq!(conflict.buffer, "mine\n");
    assert!(matches!(&conflict.external, DiskState::Present(disk) if disk.text == "theirs\n"));

    let expected_disk = match document.disk() {
        DiskState::Present(snapshot) => snapshot.version.clone(),
        other => panic!("expected present disk, got {other:?}"),
    };
    assert_eq!(
        document.reconcile(&expected_disk, "merged\n").unwrap(),
        ReconcileOutcome::Applied
    );
    assert_eq!(document.base().text, "theirs\n");
    assert_eq!(document.buffer(), "merged\n");
    assert_eq!(document.status(), DocumentStatus::Dirty);
    assert!(matches!(
        document.save().unwrap(),
        SaveOutcome::Saved { .. }
    ));
    assert_eq!(
        fs::read_to_string(fixture.worktree.join("notes.txt")).unwrap(),
        "merged\n"
    );
}

#[test]
fn explicit_reload_discards_dirty_buffer_only_after_safe_reread() {
    let fixture = Fixture::new();
    fixture.write("reload.txt", "base\n");
    let mut document = fixture.store().open("reload.txt").unwrap();
    document.set_buffer("mine\n").unwrap();
    let recovery_path = document.recovery_path().to_owned();
    fixture.write("reload.txt", "theirs\n");
    assert_eq!(document.refresh().unwrap(), RefreshOutcome::Conflict);

    document.reload_from_disk().unwrap();
    assert_eq!(document.buffer(), "theirs\n");
    assert_eq!(document.status(), DocumentStatus::Clean);
    assert!(!recovery_path.exists());
    drop(document);
    assert_eq!(
        fixture.store().open("reload.txt").unwrap().buffer(),
        "theirs\n"
    );
}

#[test]
fn stale_reconciliation_preserves_merge_draft_and_requires_new_comparison() {
    let fixture = Fixture::new();
    fixture.write("stale.txt", "A\n");
    let mut document = fixture.store().open("stale.txt").unwrap();
    document.set_buffer("B\n").unwrap();
    fixture.write("stale.txt", "C\n");
    assert_eq!(document.refresh().unwrap(), RefreshOutcome::Conflict);
    let compared_c = match document.disk() {
        DiskState::Present(snapshot) => snapshot.version.clone(),
        other => panic!("expected C on disk, got {other:?}"),
    };

    fixture.write("stale.txt", "D\n");
    assert_eq!(
        document
            .reconcile(&compared_c, "merge of B and C\n")
            .unwrap(),
        ReconcileOutcome::Stale
    );

    assert_eq!(document.buffer(), "merge of B and C\n");
    assert!(document.is_dirty());
    assert!(matches!(document.disk(), DiskState::Present(snapshot) if snapshot.text == "D\n"));
    assert_eq!(
        fs::read_to_string(fixture.worktree.join("stale.txt")).unwrap(),
        "D\n"
    );
    assert!(matches!(
        document.save().unwrap(),
        SaveOutcome::Blocked {
            status: DocumentStatus::Conflict
        }
    ));
    drop(document);
    let restarted = fixture.store().open("stale.txt").unwrap();
    assert_eq!(restarted.buffer(), "merge of B and C\n");
    assert!(matches!(restarted.disk(), DiskState::Present(snapshot) if snapshot.text == "D\n"));
}

#[test]
fn undo_to_old_base_keeps_conflict_recoverable_until_explicit_resolution() {
    let fixture = Fixture::new();
    fixture.write("undo-reconcile.txt", "A\n");
    let recovery_path = {
        let mut document = fixture.store().open("undo-reconcile.txt").unwrap();
        document.set_buffer("B\n").unwrap();
        fixture.write("undo-reconcile.txt", "C\n");
        assert_eq!(document.refresh().unwrap(), RefreshOutcome::Conflict);

        document.set_buffer("A\n").unwrap();
        assert!(document.is_dirty());
        assert_eq!(document.status(), DocumentStatus::Conflict);
        assert_eq!(document.refresh().unwrap(), RefreshOutcome::Conflict);
        assert!(matches!(
            document.save().unwrap(),
            SaveOutcome::Blocked {
                status: DocumentStatus::Conflict
            }
        ));
        document.recovery_path().to_owned()
    };
    assert!(recovery_path.exists());

    let mut restarted = fixture.store().open("undo-reconcile.txt").unwrap();
    assert_eq!(restarted.buffer(), "A\n");
    assert_eq!(restarted.base().text, "A\n");
    assert!(restarted.is_dirty());
    assert_eq!(restarted.status(), DocumentStatus::Conflict);
    let compared_c = match restarted.disk() {
        DiskState::Present(snapshot) => snapshot.version.clone(),
        other => panic!("expected C on disk, got {other:?}"),
    };
    assert_eq!(
        restarted.reconcile(&compared_c, "A\n").unwrap(),
        ReconcileOutcome::Applied
    );
    assert!(restarted.conflict().is_none());
    assert!(restarted.is_dirty());

    fixture.write("undo-reload.txt", "A\n");
    let mut reload = fixture.store().open("undo-reload.txt").unwrap();
    reload.set_buffer("B\n").unwrap();
    fixture.write("undo-reload.txt", "C\n");
    assert_eq!(reload.refresh().unwrap(), RefreshOutcome::Conflict);
    reload.set_buffer("A\n").unwrap();
    reload.reload_from_disk().unwrap();
    assert_eq!(reload.buffer(), "C\n");
    assert_eq!(reload.status(), DocumentStatus::Clean);
    assert!(!reload.is_dirty());
}

#[test]
fn delayed_external_edit_at_atomic_save_boundary_is_retained_and_reported() {
    let fixture = Fixture::new();
    fixture.write("race.txt", "base\n");
    let mut document = fixture.store().open("race.txt").unwrap();
    document.set_buffer("editor\n").unwrap();

    let outcome = document
        .save_with_hook(|path| fs::write(path, "external-at-boundary\n"))
        .unwrap();

    let SaveOutcome::ConflictRetained {
        retained_external,
        target_contains_buffer,
    } = outcome
    else {
        panic!("expected retained save conflict, got {outcome:?}");
    };
    assert!(target_contains_buffer);
    assert_eq!(
        fs::read_to_string(&retained_external).unwrap(),
        "external-at-boundary\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.worktree.join("race.txt")).unwrap(),
        "editor\n"
    );
    let conflict = document.conflict().unwrap();
    assert_eq!(conflict.kind, ConflictKind::SaveRace);
    assert_eq!(conflict.base.text, "base\n");
    assert_eq!(conflict.buffer, "editor\n");
    assert!(
        matches!(&conflict.external, DiskState::Present(disk) if disk.text == "external-at-boundary\n")
    );
    assert!(
        retained_external.starts_with(fs::canonicalize(&fixture.recovery).unwrap()),
        "retained path was {} instead of recovery root {}",
        retained_external.display(),
        fixture.recovery.display()
    );
}

#[test]
fn save_preserves_xattrs_resource_fork_permissions_flags_and_ownership() {
    let fixture = Fixture::new();
    fixture.write("metadata.txt", "base\n");
    let path = fixture.worktree.join("metadata.txt");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o754)).unwrap();
    set_xattr(&path, "com.cibergit.fixture", b"benign attribute");
    set_xattr(&path, "com.apple.ResourceFork", b"small resource fork");
    let status = Command::new("chflags")
        .arg("hidden")
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    let before = fs::metadata(&path).unwrap();

    let mut document = fixture.store().open("metadata.txt").unwrap();
    document.set_buffer("saved\n").unwrap();
    assert!(matches!(
        document.save().unwrap(),
        SaveOutcome::Saved { .. }
    ));

    let after = fs::metadata(&path).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "saved\n");
    assert_eq!(
        get_xattr(&path, "com.cibergit.fixture"),
        b"benign attribute"
    );
    assert_eq!(
        get_xattr(&path, "com.apple.ResourceFork"),
        b"small resource fork"
    );
    assert_eq!(after.mode() & 0o7777, before.mode() & 0o7777);
    assert_eq!(after.uid(), before.uid());
    assert_eq!(after.gid(), before.gid());
    assert_eq!(after.st_flags(), before.st_flags());
}

#[test]
fn save_preserves_extended_acl_entries() {
    let fixture = Fixture::new();
    fixture.write("acl.txt", "base\n");
    let path = fixture.worktree.join("acl.txt");
    let output = Command::new("chmod")
        .args(["+a", "everyone allow read"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "chmod +a failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let before = acl_entries(&path);
    assert!(!before.is_empty());

    let mut document = fixture.store().open("acl.txt").unwrap();
    document.set_buffer("saved\n").unwrap();
    assert!(matches!(
        document.save().unwrap(),
        SaveOutcome::Saved { .. }
    ));

    assert_eq!(acl_entries(&path), before);
}

#[test]
fn metadata_only_external_change_after_baseline_is_authoritative() {
    let fixture = Fixture::new();
    fixture.write("metadata-refresh.txt", "same text\n");
    let path = fixture.worktree.join("metadata-refresh.txt");
    let mut document = fixture.store().open("metadata-refresh.txt").unwrap();
    let before = document.base().version.metadata_sha256.clone();

    set_xattr(&path, "com.cibergit.fixture", b"external metadata");

    assert_eq!(document.refresh().unwrap(), RefreshOutcome::Reloaded);
    assert_eq!(document.buffer(), "same text\n");
    assert_ne!(document.base().version.metadata_sha256, before);
    assert_eq!(document.status(), DocumentStatus::Clean);
}

#[test]
fn dirty_save_blocks_on_metadata_only_external_change() {
    let fixture = Fixture::new();
    fixture.write("metadata-dirty.txt", "base\n");
    let path = fixture.worktree.join("metadata-dirty.txt");
    let mut document = fixture.store().open("metadata-dirty.txt").unwrap();
    document.set_buffer("editor\n").unwrap();

    set_xattr(&path, "com.cibergit.fixture", b"external metadata");

    assert!(matches!(
        document.save().unwrap(),
        SaveOutcome::Blocked {
            status: DocumentStatus::Conflict
        }
    ));
    assert_eq!(fs::read_to_string(&path).unwrap(), "base\n");
    assert_eq!(
        get_xattr(&path, "com.cibergit.fixture"),
        b"external metadata"
    );
    assert_eq!(document.buffer(), "editor\n");
}

#[test]
fn reconcile_rejects_metadata_only_advance_after_displayed_version() {
    let fixture = Fixture::new();
    fixture.write("metadata-reconcile.txt", "base\n");
    let path = fixture.worktree.join("metadata-reconcile.txt");
    let mut document = fixture.store().open("metadata-reconcile.txt").unwrap();
    document.set_buffer("editor\n").unwrap();
    set_xattr(&path, "com.cibergit.fixture", b"displayed metadata");
    assert_eq!(document.refresh().unwrap(), RefreshOutcome::Conflict);
    let displayed = match document.disk() {
        DiskState::Present(snapshot) => snapshot.version.clone(),
        other => panic!("expected displayed metadata version, got {other:?}"),
    };

    set_xattr(&path, "com.cibergit.fixture", b"newer metadata");

    assert_eq!(
        document.reconcile(&displayed, "merge draft\n").unwrap(),
        ReconcileOutcome::Stale
    );
    assert_eq!(document.buffer(), "merge draft\n");
    assert_eq!(get_xattr(&path, "com.cibergit.fixture"), b"newer metadata");
    assert!(matches!(
        document.save().unwrap(),
        SaveOutcome::Blocked {
            status: DocumentStatus::Conflict
        }
    ));
}

#[test]
fn metadata_only_change_at_exchange_boundary_is_retained_as_a_save_race() {
    let fixture = Fixture::new();
    fixture.write("metadata-race.txt", "base\n");
    let path = fixture.worktree.join("metadata-race.txt");
    set_xattr(&path, "com.cibergit.fixture", b"baseline metadata");
    let mut document = fixture.store().open("metadata-race.txt").unwrap();
    document.set_buffer("editor\n").unwrap();

    let outcome = document
        .save_with_hook(|path| {
            set_xattr(path, "com.cibergit.fixture", b"boundary metadata");
            Ok(())
        })
        .unwrap();

    let SaveOutcome::ConflictRetained {
        retained_external,
        target_contains_buffer,
    } = outcome
    else {
        panic!("expected retained metadata race, got {outcome:?}");
    };
    assert!(target_contains_buffer);
    assert_eq!(
        get_xattr(&retained_external, "com.cibergit.fixture"),
        b"boundary metadata"
    );
    assert_eq!(
        get_xattr(&path, "com.cibergit.fixture"),
        b"baseline metadata"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "editor\n");
    assert_eq!(document.conflict().unwrap().kind, ConflictKind::SaveRace);
}

#[test]
fn save_immediately_revalidates_a_replaced_path() {
    let fixture = Fixture::new();
    fixture.write("replace-before-save.txt", "base\n");
    let path = fixture.worktree.join("replace-before-save.txt");
    let mut document = fixture.store().open("replace-before-save.txt").unwrap();
    document.set_buffer("editor\n").unwrap();
    fs::rename(&path, fixture.worktree.join("moved-original.txt")).unwrap();
    fs::write(&path, "replacement\n").unwrap();

    assert!(matches!(
        document.save().unwrap(),
        SaveOutcome::Blocked {
            status: DocumentStatus::Conflict
        }
    ));
    assert_eq!(fs::read_to_string(path).unwrap(), "replacement\n");
    assert_eq!(document.buffer(), "editor\n");
}

#[test]
fn writer_holding_displaced_inode_finishes_into_retained_previous_file() {
    let fixture = Fixture::new();
    fixture.write("held.txt", "base\n");
    let path = fixture.worktree.join("held.txt");
    let mut held_writer = OpenOptions::new().write(true).open(&path).unwrap();
    let mut document = fixture.store().open("held.txt").unwrap();
    document.set_buffer("editor\n").unwrap();

    let SaveOutcome::Saved {
        retained_previous, ..
    } = document.save().unwrap()
    else {
        panic!("expected a verified save");
    };
    held_writer.set_len(0).unwrap();
    held_writer.write_all(b"late held-fd edit\n").unwrap();
    held_writer.sync_all().unwrap();

    assert_eq!(fs::read_to_string(path).unwrap(), "editor\n");
    assert_eq!(
        fs::read_to_string(retained_previous).unwrap(),
        "late held-fd edit\n"
    );
}

#[test]
fn delete_replacement_and_symlink_are_observed_without_losing_text() {
    let fixture = Fixture::new();
    fixture.write("tracked.txt", "base\n");
    let mut document = fixture.store().open("tracked.txt").unwrap();
    document.set_buffer("unsaved\n").unwrap();

    fs::remove_file(fixture.worktree.join("tracked.txt")).unwrap();
    assert_eq!(document.refresh().unwrap(), RefreshOutcome::Missing);
    assert_eq!(document.buffer(), "unsaved\n");
    assert_eq!(document.status(), DocumentStatus::Missing);

    fixture.write("tracked.txt", "replacement\n");
    assert_eq!(document.refresh().unwrap(), RefreshOutcome::Conflict);
    assert_eq!(document.buffer(), "unsaved\n");

    fs::remove_file(fixture.worktree.join("tracked.txt")).unwrap();
    let outside = fixture._temp.path().join("outside.txt");
    fs::write(&outside, "outside\n").unwrap();
    std::os::unix::fs::symlink(&outside, fixture.worktree.join("tracked.txt")).unwrap();
    assert_eq!(document.refresh().unwrap(), RefreshOutcome::Unsafe);
    assert!(matches!(
        document.disk(),
        DiskState::Unsafe(TargetIssue::SymlinkOrEscape)
    ));
    assert_eq!(fs::read_to_string(outside).unwrap(), "outside\n");
    assert_eq!(document.buffer(), "unsaved\n");
}

#[test]
fn clean_path_replacement_reloads_even_when_text_is_identical() {
    let fixture = Fixture::new();
    fixture.write("replace.txt", "same\n");
    let mut document = fixture.store().open("replace.txt").unwrap();
    let old_inode = document.base().version.inode;
    fs::rename(
        fixture.worktree.join("replace.txt"),
        fixture.worktree.join("old.txt"),
    )
    .unwrap();
    fixture.write("replace.txt", "same\n");

    assert_eq!(document.refresh().unwrap(), RefreshOutcome::Reloaded);
    assert_ne!(document.base().version.inode, old_inode);
    assert_eq!(document.status(), DocumentStatus::Clean);
}

#[test]
fn read_only_target_is_not_bypassed_by_replacement_save() {
    let fixture = Fixture::new();
    fixture.write("readonly.txt", "base\n");
    let path = fixture.worktree.join("readonly.txt");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    let mut document = fixture.store().open("readonly.txt").unwrap();
    document.set_buffer("unsaved\n").unwrap();

    let error = document.save().unwrap_err();
    assert!(matches!(
        error,
        DocumentError::UnsafeTarget(TargetIssue::PermissionDenied)
    ));
    assert_eq!(fs::read_to_string(&path).unwrap(), "base\n");
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn restart_restores_private_scoped_unsaved_text_and_detects_new_disk() {
    let fixture = Fixture::new();
    fixture.write("draft.txt", "base\n");
    let recovery_path = {
        let mut document = fixture.store().open("draft.txt").unwrap();
        document.set_buffer("unsaved\n").unwrap();
        document.recovery_path().to_owned()
    };

    assert_eq!(
        fs::metadata(&fixture.recovery)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&recovery_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fixture.write("draft.txt", "external\n");

    let document = fixture.store().open("draft.txt").unwrap();
    assert_eq!(document.buffer(), "unsaved\n");
    assert_eq!(document.base().text, "base\n");
    assert_eq!(document.status(), DocumentStatus::Conflict);
    assert!(matches!(
        document.recovery_status(),
        RecoveryStatus::Restored { .. }
    ));
}

#[test]
fn recovery_is_partitioned_by_account_repository_and_path() {
    let fixture = Fixture::new();
    fixture.write("same.txt", "disk\n");
    fixture.write("other.txt", "other disk\n");
    let store_a = fixture.store();
    let mut document = store_a.open("same.txt").unwrap();
    document.set_buffer("account A draft\n").unwrap();
    drop(document);

    let account_b = DocumentStore::new(
        &fixture.worktree,
        &fixture.recovery,
        RecoveryScope::new("account-2", "repository-1"),
        DocumentLimits::default(),
    )
    .unwrap();
    assert_eq!(account_b.open("same.txt").unwrap().buffer(), "disk\n");

    let repository_b = DocumentStore::new(
        &fixture.worktree,
        &fixture.recovery,
        RecoveryScope::new("account-1", "repository-2"),
        DocumentLimits::default(),
    )
    .unwrap();
    assert_eq!(repository_b.open("same.txt").unwrap().buffer(), "disk\n");
    assert_eq!(store_a.open("other.txt").unwrap().buffer(), "other disk\n");
}

#[test]
fn corrupt_recovery_is_reported_preserved_and_never_overwritten() {
    let fixture = Fixture::new();
    fixture.write("draft.txt", "base\n");
    let recovery_path = {
        let mut document = fixture.store().open("draft.txt").unwrap();
        document.set_buffer("first draft\n").unwrap();
        document.recovery_path().to_owned()
    };
    let corrupt = b"{not valid recovery";
    fs::write(&recovery_path, corrupt).unwrap();

    let mut reopened = fixture.store().open("draft.txt").unwrap();
    assert!(matches!(
        reopened.recovery_status(),
        RecoveryStatus::Corrupt { .. }
    ));
    assert_eq!(reopened.status(), DocumentStatus::RecoveryCorrupt);
    assert!(matches!(
        reopened.set_buffer("second draft\n"),
        Err(DocumentError::CorruptRecovery { .. })
    ));
    assert_eq!(fs::read(recovery_path).unwrap(), corrupt);
}

#[test]
fn unusual_filename_bytes_round_trip_and_traversal_is_rejected() {
    let fixture = Fixture::new();
    // Darwin rejects ill-formed UTF-8 at the filesystem API boundary, but the
    // backend never line-parses or lossily converts platform-supported bytes.
    let relative = PathBuf::from(OsString::from_vec(b"odd-\n\t\"-name.txt".to_vec()));
    fixture.write(&relative, "base\n");
    let mut document = fixture.store().open(&relative).unwrap();
    assert_eq!(
        document.relative_path().as_os_str().as_bytes(),
        relative.as_os_str().as_bytes()
    );
    document.set_buffer("saved\n").unwrap();
    assert!(matches!(
        document.save().unwrap(),
        SaveOutcome::Saved { .. }
    ));
    assert_eq!(
        fs::read_to_string(fixture.worktree.join(relative)).unwrap(),
        "saved\n"
    );

    let result = fixture.store().open("../outside.txt");
    assert!(matches!(result, Err(DocumentError::InvalidRelativePath)));
}

#[test]
fn symlink_and_oversized_initial_targets_are_rejected() {
    let fixture = Fixture::new();
    let outside = fixture._temp.path().join("outside.txt");
    fs::write(&outside, "outside\n").unwrap();
    std::os::unix::fs::symlink(&outside, fixture.worktree.join("link.txt")).unwrap();
    assert!(matches!(
        fixture.store().open("link.txt"),
        Err(DocumentError::UnsafeTarget(TargetIssue::SymlinkOrEscape))
    ));

    fixture.write("large.txt", b"12345");
    let store = DocumentStore::new(
        &fixture.worktree,
        fixture._temp.path().join("small-recovery"),
        RecoveryScope::new("account", "repo"),
        DocumentLimits {
            max_file_bytes: 4,
            max_buffer_bytes: 4,
        },
    )
    .unwrap();
    assert!(matches!(
        store.open("large.txt"),
        Err(DocumentError::UnsafeTarget(TargetIssue::TooLarge { .. }))
    ));

    fs::create_dir(fixture.worktree.join("directory.txt")).unwrap();
    assert!(matches!(
        fixture.store().open("directory.txt"),
        Err(DocumentError::UnsafeTarget(_))
    ));

    let hardlink_source = fixture._temp.path().join("hardlink-source.txt");
    fs::write(&hardlink_source, "aliased outside root\n").unwrap();
    fs::hard_link(&hardlink_source, fixture.worktree.join("hardlink.txt")).unwrap();
    assert!(matches!(
        fixture.store().open("hardlink.txt"),
        Err(DocumentError::UnsafeTarget(_))
    ));
}
