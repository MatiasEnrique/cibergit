use cibergit::{
    comparisons::*,
    domain::{Account, ProviderCoordinates, PullRequestReview, Repository, Revision},
    review::{ComparisonMode, file_key, load_local_file},
};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};
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
    git(dir.path(), &["config", "user.name", "Comparison Test"]);
    git(
        dir.path(),
        &["config", "user.email", "comparison@example.invalid"],
    );
    dir
}

fn commit(path: &Path, message: &str) -> String {
    git(path, &["add", "--all"]);
    git(path, &["commit", "-qm", message, "--allow-empty"]);
    git(path, &["rev-parse", "HEAD"])
}

fn linear_fixture() -> (TempDir, Revision, Vec<String>) {
    let dir = init();
    let path = dir.path();
    fs::write(path.join("one.txt"), "base\n").unwrap();
    let base = commit(path, "base");
    fs::write(path.join("one.txt"), "first\n").unwrap();
    let first = commit(path, "first");
    fs::write(path.join("two.txt"), "second\n").unwrap();
    let second = commit(path, "second");
    fs::write(path.join("one.txt"), "third\n").unwrap();
    let third = commit(path, "third");
    (
        dir,
        Revision {
            base_sha: base,
            head_sha: third.clone(),
        },
        vec![first, second, third],
    )
}

#[test]
fn full_commit_and_contiguous_range_keep_canonical_revision_separate() {
    let (dir, full, commits) = linear_fixture();
    let inventory = local_commit_inventory(dir.path(), &full).unwrap();
    assert_eq!(inventory.availability, InventoryAvailability::Complete);
    assert_eq!(
        inventory
            .commits
            .iter()
            .map(|commit| commit.sha.as_str())
            .collect::<Vec<_>>(),
        commits.iter().map(String::as_str).collect::<Vec<_>>()
    );

    let full_selection = select_local_comparison(
        dir.path(),
        &full,
        &inventory,
        ComparisonRequest::FullPullRequest,
        true,
    )
    .unwrap();
    assert_eq!(full_selection.full_revision, full);
    assert_eq!(full_selection.comparison.revision, full);
    assert!(full_selection.local_file_load.unwrap().full_pr);
    assert!(
        full_selection
            .comparison
            .files
            .iter()
            .all(|file| file.patch.is_none())
    );

    let individual = select_local_comparison(
        dir.path(),
        &full,
        &inventory,
        ComparisonRequest::Commit {
            sha: commits[1].clone(),
        },
        false,
    )
    .unwrap();
    assert_eq!(individual.full_revision, full);
    assert_eq!(individual.comparison.revision.base_sha, commits[0]);
    assert_eq!(individual.comparison.revision.head_sha, commits[1]);
    assert_eq!(
        individual.metadata.mode,
        ComparisonMode::Commit {
            sha: commits[1].clone()
        }
    );
    assert_eq!(individual.comparison.files.len(), 1);
    assert_eq!(individual.comparison.files[0].path, "two.txt");

    let range = select_local_comparison(
        dir.path(),
        &full,
        &inventory,
        ComparisonRequest::CommitRange {
            first_sha: commits[0].clone(),
            last_sha: commits[1].clone(),
        },
        true,
    )
    .unwrap();
    assert_eq!(range.full_revision, full);
    assert_eq!(range.comparison.revision.base_sha, full.base_sha);
    assert_eq!(range.comparison.revision.head_sha, commits[1]);
    assert_eq!(range.metadata.mode, ComparisonMode::CommitRange);
    let plan = range.local_file_load.unwrap();
    assert!(!plan.full_pr);
    assert_eq!(plan.revision, range.comparison.revision);
    assert!(
        range
            .comparison
            .files
            .iter()
            .all(|file| file.patch.is_none())
    );
    let selected = load_local_file(
        dir.path(),
        &plan.revision,
        &file_key(&range.comparison.files[0]),
        plan.full_pr,
    )
    .unwrap();
    assert!(selected.patch.is_some());
    assert!(
        range
            .comparison
            .files
            .iter()
            .all(|file| file.patch.is_none()),
        "lazy aggregate must not be incidentally hydrated"
    );
}

#[test]
fn fixed_inventory_and_selection_ignore_new_head_and_worktree() {
    let (dir, full, commits) = linear_fixture();
    let inventory = local_commit_inventory(dir.path(), &full).unwrap();
    fs::write(dir.path().join("one.txt"), "new remote-like head\n").unwrap();
    let newer = commit(dir.path(), "newer");
    fs::write(dir.path().join("one.txt"), "uncommitted worktree\n").unwrap();

    let selected = select_local_comparison(
        dir.path(),
        &full,
        &inventory,
        ComparisonRequest::Commit {
            sha: commits[2].clone(),
        },
        false,
    )
    .unwrap();
    assert_eq!(selected.full_revision.head_sha, commits[2]);
    assert_eq!(selected.comparison.revision.head_sha, commits[2]);
    let patch = selected.comparison.files[0].patch.as_deref().unwrap();
    assert!(patch.contains("+third"));
    assert!(!patch.contains("new remote-like head"));
    assert!(!patch.contains("uncommitted worktree"));
    assert_ne!(newer, selected.full_revision.head_sha);
    assert_eq!(
        fs::read_to_string(dir.path().join("one.txt")).unwrap(),
        "uncommitted worktree\n"
    );
}

#[test]
fn since_review_is_direct_after_divergence_and_missing_history_falls_back() {
    let dir = init();
    let path = dir.path();
    fs::write(path.join("value"), "base\n").unwrap();
    let base = commit(path, "base");
    fs::write(path.join("value"), "reviewed\n").unwrap();
    let reviewed = commit(path, "reviewed");
    git(path, &["checkout", "-q", "--detach", &base]);
    fs::write(path.join("value"), "rewritten\n").unwrap();
    let rewritten = commit(path, "rewritten");
    let full = Revision {
        base_sha: base,
        head_sha: rewritten.clone(),
    };
    let inventory = local_commit_inventory(path, &full).unwrap();
    let baseline = BaselineResolution::Found(ReviewBaseline {
        reviewed_head_sha: reviewed.clone(),
        completed_at: "2026-09-13T12:00:00Z".into(),
        source: ReviewBaselineSource::SubmittedReview {
            review_id: "R1".into(),
        },
    });
    let direct = select_local_comparison(
        path,
        &full,
        &inventory,
        ComparisonRequest::SinceLastReview { baseline },
        false,
    )
    .unwrap();
    assert_eq!(direct.full_revision, full);
    assert_eq!(direct.comparison.revision.base_sha, reviewed);
    assert_eq!(direct.comparison.revision.head_sha, rewritten);
    assert!(matches!(
        direct.metadata.mode,
        ComparisonMode::SinceLastReview { .. }
    ));
    let patch = direct.comparison.files[0].patch.as_deref().unwrap();
    assert!(patch.contains("-reviewed"));
    assert!(patch.contains("+rewritten"));

    let missing = "f".repeat(40);
    let fallback = select_local_comparison(
        path,
        &full,
        &inventory,
        ComparisonRequest::SinceLastReview {
            baseline: BaselineResolution::Found(ReviewBaseline {
                reviewed_head_sha: missing.clone(),
                completed_at: "2026-09-13T12:00:00Z".into(),
                source: ReviewBaselineSource::AcceptedLocalCompletion,
            }),
        },
        true,
    )
    .unwrap();
    assert_eq!(fallback.comparison.revision, full);
    assert_eq!(fallback.metadata.mode, ComparisonMode::FullPullRequest);
    assert_eq!(
        fallback.metadata.requested_mode,
        Some(ComparisonMode::SinceLastReview {
            reviewed_head_sha: missing
        })
    );
    assert!(fallback.metadata.notice.unwrap().contains("unavailable"));
    assert!(fallback.local_file_load.unwrap().full_pr);
}

#[test]
fn incomplete_inventory_and_ambiguous_or_noncontiguous_commits_cannot_select() {
    let (dir, full, commits) = linear_fixture();
    let mut inventory = local_commit_inventory(dir.path(), &full).unwrap();
    inventory.availability = InventoryAvailability::Incomplete;
    assert!(
        select_local_comparison(
            dir.path(),
            &full,
            &inventory,
            ComparisonRequest::FullPullRequest,
            true,
        )
        .is_ok(),
        "full PR remains available without commit-selector inventory"
    );
    assert!(
        select_local_comparison(
            dir.path(),
            &full,
            &inventory,
            ComparisonRequest::Commit {
                sha: commits[0].clone()
            },
            false,
        )
        .unwrap_err()
        .to_string()
        .contains("not complete")
    );

    inventory.availability = InventoryAvailability::Complete;
    inventory.commits[1].parent_shas = vec![full.base_sha.clone()];
    assert!(
        select_local_comparison(
            dir.path(),
            &full,
            &inventory,
            ComparisonRequest::CommitRange {
                first_sha: commits[0].clone(),
                last_sha: commits[1].clone(),
            },
            false,
        )
        .unwrap_err()
        .to_string()
        .contains("contiguous")
    );
    inventory.commits[0].parent_shas.push("e".repeat(40));
    assert!(
        select_local_comparison(
            dir.path(),
            &full,
            &inventory,
            ComparisonRequest::Commit {
                sha: commits[0].clone()
            },
            false,
        )
        .unwrap_err()
        .to_string()
        .contains("root and merge")
    );
}

fn repo(login: &str) -> Repository {
    Repository {
        host: "github.com".into(),
        owner: "Owner".into(),
        name: "Repo".into(),
        account: Account {
            host: "github.com".into(),
            login: login.into(),
        },
        local_path: None,
    }
}

fn review(
    login: &str,
    state: &str,
    submitted: Option<&str>,
    sha: Option<&str>,
) -> PullRequestReview {
    PullRequestReview {
        coordinates: ProviderCoordinates {
            provider: "github".into(),
            host: "github.com".into(),
            owner: "Owner".into(),
            repository: "Repo".into(),
            pull_request: 7,
            remote_id: format!("review-{login}-{state}"),
        },
        author: Some(login.into()),
        body: String::new(),
        state: state.into(),
        submitted_at: submitted.map(str::to_owned),
        commit_sha: sha.map(str::to_owned),
        edit_summary_capability: None,
        dismissal_capability: None,
        url: String::new(),
    }
}

#[test]
fn baseline_is_selected_account_explicit_submission_or_durable_completion_only() {
    let repository = repo("Alice");
    let old = "1".repeat(40);
    let latest = "2".repeat(40);
    let other = "3".repeat(40);
    let pending = "4".repeat(40);
    let reviews = vec![
        review(
            "alice",
            "APPROVED",
            Some("2026-09-13T10:00:00Z"),
            Some(&old),
        ),
        review(
            "ALICE",
            "COMMENTED",
            Some("2026-09-13T11:00:00Z"),
            Some(&latest),
        ),
        review(
            "bob",
            "APPROVED",
            Some("2026-09-13T12:00:00Z"),
            Some(&other),
        ),
        review("Alice", "PENDING", None, Some(&pending)),
    ];
    let resolved = resolve_review_baseline(&repository, 7, &reviews, true, None);
    let BaselineResolution::Found(resolved) = resolved else {
        panic!("expected baseline")
    };
    assert_eq!(resolved.reviewed_head_sha, latest);
    assert!(matches!(
        resolved.source,
        ReviewBaselineSource::SubmittedReview { .. }
    ));

    let local = LocalReviewCompletion {
        completion_id: "accepted-local-1".into(),
        repository_key: repository.cache_key(),
        account: repository.account.clone(),
        pull_request: 7,
        reviewed_head_sha: "5".repeat(40),
        completed_at: "2026-09-13T13:00:00Z".into(),
    };
    let resolved = resolve_review_baseline(&repository, 7, &reviews, true, Some(&local));
    assert!(matches!(
        resolved,
        BaselineResolution::Found(ReviewBaseline {
            source: ReviewBaselineSource::AcceptedLocalCompletion,
            ..
        })
    ));
    assert_eq!(
        resolve_review_baseline(&repository, 7, &reviews, false, Some(&local)),
        BaselineResolution::Unavailable(BaselineUnavailableReason::ReviewActivityIncomplete)
    );
    assert_eq!(
        resolve_review_baseline(&repository, 7, &[], true, None),
        BaselineResolution::Unavailable(BaselineUnavailableReason::NoSubmittedReview)
    );
}

#[test]
fn lazy_inventory_retains_binary_and_media_without_loading_content() {
    let dir = init();
    fs::write(dir.path().join("base"), "base\n").unwrap();
    let base = commit(dir.path(), "base");
    fs::write(dir.path().join("graphic.svg"), "<svg>secret</svg>\n").unwrap();
    fs::write(dir.path().join("binary.bin"), b"\0binary-private\xff").unwrap();
    let head = commit(dir.path(), "media and binary");
    let full = Revision {
        base_sha: base,
        head_sha: head,
    };
    let inventory = local_commit_inventory(dir.path(), &full).unwrap();
    let selected = select_local_comparison(
        dir.path(),
        &full,
        &inventory,
        ComparisonRequest::FullPullRequest,
        true,
    )
    .unwrap();
    assert_eq!(selected.comparison.files.len(), 2);
    assert!(selected.comparison.files.iter().all(|file| {
        file.patch.is_none() && !file.patch_complete && file.additions == 0 && file.deletions == 0
    }));
    assert!(
        selected
            .comparison
            .files
            .iter()
            .any(|file| file.path == "graphic.svg")
    );
    assert!(
        selected
            .comparison
            .files
            .iter()
            .any(|file| file.path == "binary.bin")
    );
}

#[test]
fn raw_path_identity_survives_selector_inventory_and_lazy_load() {
    let dir = init();
    let base = commit(dir.path(), "empty base");
    let blob = git_input(
        dir.path(),
        &["hash-object", "-w", "--stdin"],
        b"raw-path-content\n",
    );
    let raw_name = b"invalid\xff.rs".to_vec();
    let mut tree_record = format!("100644 blob {blob}\t").into_bytes();
    tree_record.extend(&raw_name);
    tree_record.push(0);
    let tree = git_input(dir.path(), &["mktree", "-z"], &tree_record);
    let head = git(
        dir.path(),
        &["commit-tree", &tree, "-p", &base, "-m", "raw path"],
    );
    let full = Revision {
        base_sha: base,
        head_sha: head,
    };
    let inventory = local_commit_inventory(dir.path(), &full).unwrap();
    let selection = select_local_comparison(
        dir.path(),
        &full,
        &inventory,
        ComparisonRequest::FullPullRequest,
        true,
    )
    .unwrap();
    let file = selection.comparison.files.first().unwrap();
    assert_eq!(file.raw_path.as_ref(), Some(&raw_name));
    let key = file_key(file);
    assert!(key.starts_with("\0raw:"));
    let plan = selection.local_file_load.unwrap();
    let loaded = load_local_file(dir.path(), &plan.revision, &key, plan.full_pr).unwrap();
    assert_eq!(loaded.raw_path.as_ref(), Some(&raw_name));
    assert!(
        loaded
            .patch
            .as_deref()
            .unwrap()
            .contains("+raw-path-content")
    );
}
