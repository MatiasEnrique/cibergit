use cibergit::domain::{Account, PullRequest, Repository};
use cibergit::workspace::{Filter, GroupBy, PersonalFilter, PollSchedule, group_path};
use std::time::Duration;

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

fn pr(number: u64, source: &str, target: &str) -> PullRequest {
    PullRequest {
        number,
        title: format!("Change {number}"),
        source_branch: source.into(),
        target_branch: target.into(),
        state: "OPEN".into(),
        ..Default::default()
    }
}

fn stack_label(item: &PullRequest, prs: &[PullRequest]) -> String {
    group_path(&repo("me"), item, &[GroupBy::Stack], prs)
        .into_iter()
        .next()
        .unwrap()
}

#[test]
fn default_filter_is_all_open_and_closed_remain_searchable() {
    let open = PullRequest {
        state: "OPEN".into(),
        title: "Open work".into(),
        number: 1,
        source_branch: "feature/open".into(),
        ..Default::default()
    };
    let closed = PullRequest {
        state: "CLOSED".into(),
        title: "Closed work".into(),
        number: 2,
        source_branch: "feature/closed".into(),
        ..Default::default()
    };
    let merged = PullRequest {
        state: "MERGED".into(),
        title: "Merged work".into(),
        number: 3,
        source_branch: "feature/merged".into(),
        ..Default::default()
    };
    assert!(Filter::default().matches(&open, "me"));
    assert!(!Filter::default().matches(&closed, "me"));
    assert!(!Filter::default().matches(&merged, "me"));
    let all = Filter {
        state: "all".into(),
        ..Default::default()
    };
    assert!(all.matches(&open, "me") && all.matches(&closed, "me") && all.matches(&merged, "me"));
    assert!(
        Filter {
            state: "closed".into(),
            search: "closed".into(),
            ..Default::default()
        }
        .matches(&closed, "me")
    );
    assert!(
        Filter {
            state: "merged".into(),
            search: "#3".into(),
            ..Default::default()
        }
        .matches(&merged, "me")
    );
}

#[test]
fn search_covers_title_number_and_both_branches() {
    let item = PullRequest {
        title: "Rewrite parser".into(),
        number: 11,
        source_branch: "feat/parser".into(),
        target_branch: "release/1.2".into(),
        state: "OPEN".into(),
        ..Default::default()
    };
    for needle in ["rewrite", "#11", "feat/parser", "release/1.2"] {
        assert!(
            Filter {
                search: needle.into(),
                ..Default::default()
            }
            .matches(&item, "me"),
            "search {needle}"
        );
    }
    assert!(
        !Filter {
            search: "unrelated".into(),
            ..Default::default()
        }
        .matches(&item, "me")
    );
}

#[test]
fn attribute_filters_and_personal_identity_are_case_insensitive() {
    let item = PullRequest {
        title: "Ship it".into(),
        number: 4,
        author: "Ada".into(),
        reviewers: vec!["Bea".into()],
        assignees: vec!["Cyd".into()],
        labels: vec!["Bug".into()],
        draft: true,
        review_status: "ChangesRequested".into(),
        check_status: "Failing".into(),
        source_branch: "feat/bug".into(),
        target_branch: "Main".into(),
        state: "OPEN".into(),
        ..Default::default()
    };
    let matching = Filter {
        author: "ada".into(),
        reviewer: "bea".into(),
        assignee: "cyd".into(),
        label: "bug".into(),
        draft: Some(true),
        review_status: "changesrequested".into(),
        check_status: "failing".into(),
        source_branch: "FEAT/bug".into(),
        target_branch: "main".into(),
        ..Default::default()
    };
    assert!(matching.matches(&item, "ada"));
    assert!(
        !Filter {
            draft: Some(false),
            ..Default::default()
        }
        .matches(&item, "ada")
    );
    assert!(
        Filter {
            personal: PersonalFilter::ReviewRequested,
            ..Default::default()
        }
        .matches(&item, "bea")
    );
    assert!(
        Filter {
            personal: PersonalFilter::Own,
            ..Default::default()
        }
        .matches(&item, "ADA")
    );
    assert!(
        Filter {
            personal: PersonalFilter::Participating,
            ..Default::default()
        }
        .matches(&item, "cyd")
    );
    assert!(
        !Filter {
            personal: PersonalFilter::Participating,
            ..Default::default()
        }
        .matches(&item, "other")
    );
    assert!(
        !Filter {
            personal: PersonalFilter::Own,
            ..Default::default()
        }
        .matches(&item, "")
    );
}

#[test]
fn source_prefix_and_ordered_groups_compose() {
    let repository = repo("me");
    let feature = pr(1, "feat/login", "main");
    let other = pr(2, "hotfix", "main");
    let groups = [
        GroupBy::Repository,
        GroupBy::TargetBranch,
        GroupBy::SourcePrefix("feat/".into()),
        GroupBy::SourceBranch,
    ];
    assert_eq!(
        group_path(
            &repository,
            &feature,
            &groups,
            &[feature.clone(), other.clone()]
        ),
        vec![
            "acme/app · me".to_string(),
            "main".into(),
            "feat/".into(),
            "feat/login".into(),
        ]
    );
    assert_eq!(
        group_path(&repository, &other, &groups, &[feature, other.clone()]),
        vec![
            "acme/app · me".to_string(),
            "main".into(),
            "Other branches".into(),
            "hotfix".into(),
        ]
    );
}

#[test]
fn stack_group_follows_inferred_chain_to_the_same_root() {
    let root = pr(1, "layer-a", "main");
    let middle = pr(2, "layer-b", "layer-a");
    let tip = pr(3, "layer-c", "layer-b");
    let prs = vec![root.clone(), middle.clone(), tip.clone()];
    let expected = "layer-a (inferred)";
    assert_eq!(stack_label(&root, &prs), expected);
    assert_eq!(stack_label(&middle, &prs), expected);
    assert_eq!(stack_label(&tip, &prs), expected);
}

#[test]
fn stack_group_keeps_multiple_tips_on_the_inferred_root_without_a_merge_policy() {
    let root = pr(1, "layer-a", "main");
    let left = pr(2, "layer-b", "layer-a");
    let right = pr(3, "layer-c", "layer-a");
    let prs = vec![root.clone(), left.clone(), right.clone()];
    let expected = "layer-a (inferred)";
    assert_eq!(stack_label(&root, &prs), expected);
    assert_eq!(stack_label(&left, &prs), expected);
    assert_eq!(stack_label(&right, &prs), expected);
}

#[test]
fn stack_group_flags_cycles_and_ambiguous_parents() {
    let cyclic_a = pr(1, "a", "b");
    let cyclic_b = pr(2, "b", "a");
    let cyclic = vec![cyclic_a.clone(), cyclic_b.clone()];
    assert_eq!(stack_label(&cyclic_a, &cyclic), "Cyclic stack");
    assert_eq!(stack_label(&cyclic_b, &cyclic), "Cyclic stack");
    assert_eq!(stack_label(&pr(9, "loop", "loop"), &[]), "Cyclic stack");

    let one = pr(10, "topic", "main");
    let two = pr(11, "topic", "develop");
    let child = pr(12, "wip", "topic");
    let ambiguous = vec![one, two, child.clone()];
    assert_eq!(stack_label(&child, &ambiguous), "Ambiguous stack");
}

#[test]
fn poll_backoff_is_bounded_and_independent_per_key() {
    let mut schedule = PollSchedule::default();
    assert_eq!(schedule.delay("pr", true, true), Duration::from_secs(15));
    assert_eq!(
        schedule.delay("sidebar", false, true),
        Duration::from_secs(60)
    );
    assert_eq!(schedule.delay("pr", true, false), Duration::from_secs(60));
    for _ in 0..8 {
        schedule.failed("pr");
    }
    assert_eq!(
        schedule.delay("pr", true, true),
        Duration::from_secs(15 * 32)
    );
    assert_eq!(
        schedule.delay("pr", true, false),
        Duration::from_secs(15 * 32 * 4)
    );
    assert_eq!(
        schedule.delay("sidebar", false, true),
        Duration::from_secs(60)
    );
    schedule.succeeded("pr");
    assert_eq!(schedule.delay("pr", true, true), Duration::from_secs(15));
}
