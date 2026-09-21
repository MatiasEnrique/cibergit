//! `local_history` against real repositories built by real Git.
//!
//! The lane algorithm is unit-tested inside `src/history.rs` over hand-built
//! commits. What this file proves is the other half: that what Git actually
//! prints parses back into those commits — the record framing, the decoration
//! format, merges, roots, and the truncation bound.

use cibergit::comparisons::InventoryAvailability;
use cibergit::history::{
    HistoryScope, MAX_HISTORY_COMMITS, RefKind, RepositoryHistory, lay_out, local_history,
};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "History Test")
        .env("GIT_AUTHOR_EMAIL", "history@example.invalid")
        .env("GIT_COMMITTER_NAME", "History Test")
        .env("GIT_COMMITTER_EMAIL", "history@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn init() -> TempDir {
    let directory = TempDir::new().unwrap();
    git(directory.path(), &["init", "-q", "-b", "main"]);
    git(directory.path(), &["config", "user.name", "History Test"]);
    git(
        directory.path(),
        &["config", "user.email", "history@example.invalid"],
    );
    directory
}

fn commit(path: &Path, message: &str) -> String {
    git(path, &["commit", "-q", "--allow-empty", "-m", message]);
    git(path, &["rev-parse", "HEAD"])
}

fn all(path: &Path) -> RepositoryHistory {
    local_history(path, &HistoryScope::AllRefs).unwrap()
}

fn headline(history: &RepositoryHistory, sha: &str) -> String {
    history
        .commits
        .iter()
        .find(|commit| commit.sha == sha)
        .unwrap_or_else(|| panic!("{sha} is missing from the history"))
        .message_headline
        .clone()
}

#[test]
fn a_linear_history_reads_back_in_order_with_its_parents() {
    let repository = init();
    let path = repository.path();
    let first = commit(path, "first");
    let second = commit(path, "second");
    let third = commit(path, "third");

    let history = all(path);
    assert_eq!(history.availability, InventoryAvailability::Complete);
    assert_eq!(history.notice, None);
    let shas: Vec<_> = history
        .commits
        .iter()
        .map(|commit| commit.sha.clone())
        .collect();
    assert_eq!(
        shas,
        vec![third.clone(), second.clone(), first.clone()],
        "history reads newest first"
    );
    assert_eq!(headline(&history, &third), "third");

    assert_eq!(history.commits[0].parent_shas, vec![second.clone()]);
    assert_eq!(history.commits[1].parent_shas, vec![first.clone()]);
    assert!(
        history.commits[2].parent_shas.is_empty(),
        "the root commit has no parents"
    );
    assert!(!history.commits[2].is_merge());
    assert_eq!(history.commits[2].first_parent(), None);
    assert_eq!(history.commits[0].short_sha(), &third[..7]);
    assert_eq!(history.commits[0].author_name, "History Test");
}

#[test]
fn a_merge_carries_both_parents_in_git_order() {
    let repository = init();
    let path = repository.path();
    let base = commit(path, "base");
    let mainline = commit(path, "mainline");
    git(path, &["checkout", "-q", "-b", "side", &base]);
    let side = commit(path, "side work");
    git(path, &["checkout", "-q", "main"]);
    git(
        path,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    let merge = git(path, &["rev-parse", "HEAD"]);

    let history = all(path);
    let merged = history
        .commits
        .iter()
        .find(|commit| commit.sha == merge)
        .expect("the merge is in the history");
    assert!(merged.is_merge());
    assert_eq!(
        merged.parent_shas,
        vec![mainline.clone(), side.clone()],
        "the first parent is the branch merged into"
    );
    assert_eq!(
        merged.first_parent(),
        Some(mainline.as_str()),
        "a merge is diffed against the branch it landed on"
    );

    // The graph over a real merge is the shape the unit tests describe.
    let graph = lay_out(&history.commits);
    assert_eq!(graph.rows.len(), history.commits.len());
    assert_eq!(graph.lane_count, 2, "a side branch needs a second lane");
    assert!(graph.rows[0].merge);
}

#[test]
fn all_refs_reaches_commits_that_no_branch_head_descends_from() {
    let repository = init();
    let path = repository.path();
    commit(path, "on main");
    git(path, &["checkout", "-q", "--orphan", "island"]);
    let island = commit(path, "on island");
    git(path, &["checkout", "-q", "main"]);

    let every = all(path);
    assert!(
        every.commits.iter().any(|commit| commit.sha == island),
        "an unrelated branch is still part of the repository's history"
    );

    let only_main = local_history(path, &HistoryScope::Ref("main".into())).unwrap();
    assert!(
        !only_main.commits.iter().any(|commit| commit.sha == island),
        "narrowing to one ref must actually narrow the read"
    );
}

#[test]
fn decorations_name_the_checked_out_branch_other_branches_and_tags() {
    let repository = init();
    let path = repository.path();
    let first = commit(path, "first");
    git(path, &["tag", "v1.0"]);
    git(path, &["branch", "shipped"]);
    let second = commit(path, "second");

    let history = all(path);
    let tip = history
        .commits
        .iter()
        .find(|commit| commit.sha == second)
        .unwrap();
    assert_eq!(
        tip.refs
            .iter()
            .map(|label| (label.name.as_str(), label.kind))
            .collect::<Vec<_>>(),
        vec![("main", RefKind::Head)],
        "the checked-out branch is marked as HEAD's"
    );

    let tagged = history
        .commits
        .iter()
        .find(|commit| commit.sha == first)
        .unwrap();
    let mut labels: Vec<_> = tagged
        .refs
        .iter()
        .map(|label| (label.name.as_str(), label.kind))
        .collect();
    labels.sort_by_key(|(name, _)| *name);
    assert_eq!(
        labels,
        vec![("shipped", RefKind::LocalBranch), ("v1.0", RefKind::Tag)]
    );
}

#[test]
fn a_history_longer_than_the_limit_is_cut_and_reports_that_it_was() {
    let repository = init();
    let path = repository.path();
    // One past the limit is the smallest history that proves the bound, and
    // each commit is empty, so this stays fast.
    for index in 0..=MAX_HISTORY_COMMITS {
        commit(path, &format!("commit {index}"));
    }

    let history = all(path);
    assert_eq!(history.commits.len(), MAX_HISTORY_COMMITS);
    assert_eq!(history.availability, InventoryAvailability::Incomplete);
    assert!(
        history
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("longer")),
        "a cut history has to say it was cut: {:?}",
        history.notice
    );
    assert_eq!(
        history.commits[0].message_headline,
        format!("commit {MAX_HISTORY_COMMITS}"),
        "the cut keeps the newest commits, not the oldest"
    );
}

#[test]
fn a_history_exactly_at_the_limit_is_complete() {
    let repository = init();
    let path = repository.path();
    for index in 0..MAX_HISTORY_COMMITS {
        commit(path, &format!("commit {index}"));
    }

    let history = all(path);
    assert_eq!(history.commits.len(), MAX_HISTORY_COMMITS);
    assert_eq!(
        history.availability,
        InventoryAvailability::Complete,
        "a history that ends exactly on the limit was not truncated"
    );
    assert_eq!(history.notice, None);
}

#[test]
fn an_empty_repository_reads_as_an_empty_history() {
    let repository = init();
    let history = all(repository.path());
    assert!(history.commits.is_empty());
    assert_eq!(history.availability, InventoryAvailability::Complete);
    assert!(lay_out(&history.commits).rows.is_empty());
}

#[test]
fn a_subject_containing_the_field_separator_cannot_break_the_records() {
    let repository = init();
    let path = repository.path();
    // Newlines, quotes and a literal "\0" spelling are the shapes most likely
    // to be mistaken for framing by a parser that trusts its input.
    commit(path, "subject with \"quotes\" and a \\0 spelling");
    let awkward = commit(path, "trailing separator\ttab");

    let history = all(path);
    assert_eq!(history.commits.len(), 2);
    assert_eq!(headline(&history, &awkward), "trailing separator\ttab");
}

#[test]
fn a_ref_name_that_would_become_an_option_is_refused_before_git_runs() {
    let repository = init();
    let path = repository.path();
    commit(path, "first");
    for name in ["--all", "-n", "main..main", "main^"] {
        assert!(
            local_history(path, &HistoryScope::Ref(name.into())).is_err(),
            "{name:?} must never reach the command line"
        );
    }
}

#[test]
fn a_root_commit_diffs_against_the_empty_tree_as_the_addition_of_its_files() {
    let repository = init();
    let path = repository.path();
    std::fs::write(path.join("first.txt"), "hello\n").unwrap();
    std::fs::write(path.join("second.txt"), "world\n").unwrap();
    git(path, &["add", "--all"]);
    let root = commit(path, "root");

    // What the History page does for a commit with no parent: the empty tree
    // stands in for the missing parent, so the commit reads as the addition of
    // everything in it.
    let base = cibergit::review::empty_tree_oid(path).unwrap();
    let revision = cibergit::domain::Revision {
        base_sha: base,
        head_sha: root,
    };
    let comparison = cibergit::review::local_inventory(path, &revision).unwrap();
    let mut paths: Vec<_> = comparison
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["first.txt", "second.txt"]);

    // And the lazy patch load reaches the same pair, so the diff actually fills in.
    let key = cibergit::review::file_key(&comparison.files[0]);
    let loaded = cibergit::review::load_local_file(path, &revision, &key, false).unwrap();
    assert!(
        loaded
            .patch
            .as_deref()
            .is_some_and(|patch| patch.contains('+')),
        "a root commit's file has to load as an addition"
    );
}

#[test]
fn a_non_empty_tree_is_still_refused_as_a_diff_base() {
    let repository = init();
    let path = repository.path();
    std::fs::write(path.join("first.txt"), "hello\n").unwrap();
    git(path, &["add", "--all"]);
    let root = commit(path, "root");
    let tree = git(path, &["rev-parse", "HEAD^{tree}"]);

    let revision = cibergit::domain::Revision {
        base_sha: tree,
        head_sha: root,
    };
    assert!(
        cibergit::review::local_inventory(path, &revision).is_err(),
        "only the empty tree is admitted as a stand-in for a missing parent"
    );
}
