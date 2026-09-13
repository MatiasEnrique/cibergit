use cibergit::domain::{Account, Comparison, PullRequest, Repository, Revision};
use cibergit::workspace::{Store, WorkspaceState};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

fn repo(login: &str) -> Repository {
    Repository {
        host: "github.com".into(),
        owner: "acme".into(),
        name: "app".into(),
        account: Account {
            host: "github.com".into(),
            login: login.into(),
        },
        local_path: None,
    }
}

fn comparison(base: &str, head: &str) -> Comparison {
    Comparison {
        revision: Revision {
            base_sha: base.into(),
            head_sha: head.into(),
        },
        files: vec![],
        complete: true,
        notice: None,
    }
}

fn json_files(root: &Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension().and_then(|ext| ext.to_str()) == Some("json")
                && path.file_name().and_then(|name| name.to_str()) != Some("workspace.json")
        })
        .collect()
}

#[test]
fn store_root_and_records_are_private() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .save_draft(&repo("one"), 8, "comment", "still typing")
        .unwrap();
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(dir.path()), 0o700);
    let files = json_files(dir.path());
    assert_eq!(files.len(), 1);
    assert_eq!(mode(&files[0]), 0o600);
}

#[test]
fn caches_and_drafts_stay_account_partitioned() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let one = repo("one");
    let two = repo("two");
    let prs = vec![PullRequest {
        number: 4,
        title: "Private change".into(),
        ..Default::default()
    }];
    store.save_pull_requests(&one, &prs).unwrap();
    store.save_draft(&one, 4, "body", "draft-one").unwrap();
    store
        .save_comparison(&one, 4, &comparison("aaa", "bbb"))
        .unwrap();
    assert_eq!(
        store.load_pull_requests(&one).unwrap()[0].title,
        "Private change"
    );
    assert!(store.load_pull_requests(&two).is_err());
    assert_eq!(store.load_draft(&one, 4, "body").unwrap(), "draft-one");
    assert!(store.load_draft(&two, 4, "body").is_err());
    assert_eq!(
        store
            .load_comparison(
                &one,
                4,
                &Revision {
                    base_sha: "aaa".into(),
                    head_sha: "bbb".into()
                }
            )
            .unwrap()
            .revision
            .head_sha,
        "bbb"
    );
    assert!(
        store
            .load_comparison(
                &two,
                4,
                &Revision {
                    base_sha: "aaa".into(),
                    head_sha: "bbb".into()
                }
            )
            .is_err()
    );
}

#[test]
fn comparison_load_rejects_wrong_revision_content() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let repository = repo("one");
    let requested = Revision {
        base_sha: "base-a".into(),
        head_sha: "head-a".into(),
    };
    store
        .save_comparison(&repository, 3, &comparison("base-a", "head-a"))
        .unwrap();
    assert_eq!(
        store
            .load_comparison(&repository, 3, &requested)
            .unwrap()
            .revision,
        requested
    );
    assert!(
        store
            .load_comparison(
                &repository,
                3,
                &Revision {
                    base_sha: "base-a".into(),
                    head_sha: "head-other".into()
                }
            )
            .is_err()
    );
    let files = json_files(dir.path());
    assert_eq!(files.len(), 1);
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&files[0]).unwrap()).unwrap();
    value["revision"]["head_sha"] = serde_json::Value::String("tampered".into());
    fs::write(&files[0], serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    let error = store
        .load_comparison(&repository, 3, &requested)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("does not match the requested base/head"),
        "{error}"
    );
}

#[test]
fn workspace_round_trip_keeps_saved_views_and_tabs() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut state = WorkspaceState::default();
    state.add_repository(repo("one"));
    state.views[0].name = "Needs review".into();
    state.tabs.push(cibergit::workspace::TabState {
        repository_key: repo("one").cache_key(),
        number: 12,
        revision: Revision {
            base_sha: "a".into(),
            head_sha: "b".into(),
        },
        selected_file: Some("src/lib.rs".into()),
        scroll_offset: 24.0,
        diff_mode: "unified".into(),
    });
    state.active_tab = Some(0);
    store.save_workspace(&state).unwrap();
    let loaded = store.load_workspace().unwrap();
    assert_eq!(loaded.views[0].name, "Needs review");
    assert_eq!(loaded.tabs[0].selected_file.as_deref(), Some("src/lib.rs"));
    assert_eq!(loaded.active_tab, Some(0));
}

#[test]
fn malformed_or_future_workspace_data_is_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspace.json");
    fs::write(&path, r#"{"schema_version":2,"keep":"future-bytes"}"#).unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert!(store.load_workspace().is_err());
    let save_error = store
        .save_workspace(&WorkspaceState::default())
        .unwrap_err()
        .to_string();
    assert!(save_error.contains("Refusing to overwrite"), "{save_error}");
    assert!(fs::read_to_string(&path).unwrap().contains("future-bytes"));

    fs::write(&path, "not-json").unwrap();
    assert!(store.load_workspace().is_err());
    assert!(store.save_workspace(&WorkspaceState::default()).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "not-json");
}

#[test]
fn leftover_temp_files_do_not_replace_readable_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut state = WorkspaceState::default();
    state.add_repository(repo("one"));
    store.save_workspace(&state).unwrap();
    fs::write(dir.path().join("workspace.1.2.tmp"), "partial-crash").unwrap();
    let loaded = store.load_workspace().unwrap();
    assert_eq!(loaded.repositories.len(), 1);
}

#[test]
fn missing_workspace_file_loads_defaults_without_creating_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let loaded = store.load_workspace().unwrap();
    assert_eq!(loaded.schema_version, 1);
    assert_eq!(loaded.views[0].name, "All open pull requests");
    assert!(!dir.path().join("workspace.json").exists());
}

#[test]
fn store_refuses_to_write_an_unsupported_in_memory_schema() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let state = WorkspaceState {
        schema_version: 2,
        ..WorkspaceState::default()
    };
    assert!(store.save_workspace(&state).is_err());
    assert!(!dir.path().join("workspace.json").exists());
}
