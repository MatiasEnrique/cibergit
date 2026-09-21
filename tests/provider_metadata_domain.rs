use cibergit::domain::{
    Account, CheckKind, MergeEligibility, ProviderCoordinates, PullRequest, PullRequestCheck,
    PullRequestDetails, PullRequestSummary, PullRequestSummaryPage, Repository,
};
use cibergit::providers::GithubProvider;
use serde_json::json;

#[test]
fn cached_pull_requests_gain_conservative_participant_defaults() {
    let old = json!({
        "number": 7,
        "title": "cached",
        "body": "",
        "source_branch": "feature",
        "target_branch": "main",
        "author": "author",
        "reviewers": [],
        "assignees": [],
        "labels": [],
        "draft": false,
        "state": "OPEN",
        "review_status": "UNKNOWN",
        "check_status": "UNKNOWN",
        "base_sha": "base",
        "head_sha": "head",
        "url": "https://github.com/owner/repo/pull/7"
    });
    let pull: PullRequest = serde_json::from_value(old).unwrap();
    assert!(pull.participants.is_empty());
    assert!(!pull.participants_complete);
    assert_eq!(pull.participants_notice, None);
}

#[test]
fn details_types_are_provider_independent_and_revision_free() {
    let coordinates = ProviderCoordinates {
        provider: "github".into(),
        host: "github.com".into(),
        owner: "owner".into(),
        repository: "repo".into(),
        pull_request: 7,
        remote_id: "check-id".into(),
    };
    let details = PullRequestDetails {
        number: 7,
        pull_request_node_id: None,
        base_repository: None,
        observed_head_sha: None,
        rollup_commit_sha: None,
        potential_merge_commit_sha: None,
        head_repository: None,
        rollup_repository: None,
        potential_merge_commit_repository: None,
        body: "current collaboration body".into(),
        requested_reviewers: vec!["reviewer".into()],
        labels: vec!["bug".into()],
        assignees: vec!["assignee".into()],
        merge_eligibility: MergeEligibility {
            state: "OPEN".into(),
            draft: false,
            mergeable: "MERGEABLE".into(),
            merge_state_status: "CLEAN".into(),
            review_status: "Approved".into(),
            check_status: "Passing".into(),
            maintainer_can_modify: true,
            can_rebase: true,
            can_update_branch: false,
            auto_merge_enabled: false,
            in_merge_queue: false,
        },
        issue_comments: vec![],
        reviews: vec![],
        review_threads: vec![],
        reactions: vec![],
        checks: vec![PullRequestCheck {
            coordinates,
            kind: CheckKind::CheckRun,
            name: "build".into(),
            status: "COMPLETED".into(),
            conclusion: Some("SUCCESS".into()),
            description: None,
            details_url: None,
            github_permalink: None,
            started_at: None,
            completed_at: None,
            required: Some(true),
            database_id: None,
            suite: None,
            commit_sha: None,
            commit_repository: None,
            sha_class: Default::default(),
            actions_linkage: Default::default(),
        }],
        activity_complete: true,
        checks_complete: true,
        notice: None,
    };
    let encoded = serde_json::to_value(details).unwrap();
    assert!(encoded.get("revision").is_none());
    assert!(encoded.get("base_sha").is_none());
    assert!(encoded.get("head_sha").is_none());

    let account = Account {
        host: "github.com".into(),
        login: "selected-account".into(),
    };
    let repo = Repository {
        host: "github.com".into(),
        owner: "owner".into(),
        name: "repo".into(),
        account: account.clone(),
        local_path: None,
    };
    let provider = GithubProvider::new(account);
    let call: fn(&GithubProvider, &Repository, u64) -> anyhow::Result<PullRequestDetails> =
        GithubProvider::details;
    let _ = (provider, repo, call);
}

#[test]
fn pull_request_summaries_are_provider_independent_and_revision_free() {
    let summary: PullRequestSummary = serde_json::from_value(json!({
        "number": 7,
        "title": "Compact the chrome above the diff",
        "author": "ada",
        "source_branch": "feature",
        "target_branch": "main",
        "labels": ["ui"],
        "draft": false,
        "state": "MERGED",
        "updated_at": "2026-09-12T12:00:00Z",
        "url": "https://github.com/owner/repo/pull/7"
    }))
    .unwrap();
    assert_eq!(summary.state, "MERGED");
    assert_eq!(summary.labels, ["ui"]);

    // A summary lists a pull request; it can never stand in for one being
    // reviewed, so it carries no revision to pin a comparison with.
    let encoded = serde_json::to_value(&summary).unwrap();
    for absent in ["revision", "base_sha", "head_sha", "body"] {
        assert!(encoded.get(absent).is_none(), "summary carries {absent}");
    }

    let account = Account {
        host: "github.com".into(),
        login: "selected-account".into(),
    };
    let repo = Repository {
        host: "github.com".into(),
        owner: "owner".into(),
        name: "repo".into(),
        account: account.clone(),
        local_path: None,
    };
    let provider = GithubProvider::new(account);
    let call: fn(
        &GithubProvider,
        &Repository,
        &str,
        usize,
    ) -> anyhow::Result<PullRequestSummaryPage> = GithubProvider::list_pull_request_summaries;
    let _ = (provider, repo, call);
}
