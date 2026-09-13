#[macro_export]
macro_rules! provider_action_tests {
    () => {
use super::*;
use $crate::{
    domain::{
        Account, ChangedFile, Comparison, MergeAction, MergeExecutionRequest, MergeMethod,
        ProviderMutationOutcome, Repository, ReviewAuxiliaryAction, ReviewAuxiliaryRequest,
        Revision,
    },
    participation::{
        CanonicalPublishedPatch, DiffSide, DraftStore, LineSelection, LoadOutcome,
        ReviewComposition, ReviewEvent, ReviewKey, ReviewOperationStatus, validate_coordinate,
    },
    review::{ComparisonMetadata, ReviewSession, file_key},
};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, time::Duration};
use tempfile::TempDir;

const OLD: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";
const NEW: &str = "3333333333333333333333333333333333333333";
const PATCH: &str = "@@ -10,2 +10,2 @@\n-old\n+new\n keep\n";

fn account(login: &str) -> Account {
    Account {
        host: "github.com".into(),
        login: login.into(),
    }
}

fn repo(login: &str) -> Repository {
    Repository {
        host: "github.com".into(),
        owner: "owner".into(),
        name: "repo".into(),
        account: account(login),
        local_path: None,
    }
}

fn key(login: &str) -> ReviewKey {
    ReviewKey::for_repository("github", &repo(login), 7).unwrap()
}

fn comparison(head: &str) -> Comparison {
    Comparison {
        revision: Revision {
            base_sha: OLD.into(),
            head_sha: head.into(),
        },
        files: vec![ChangedFile {
            path: "src/lib.rs".into(),
            previous_path: None,
            raw_path: None,
            raw_previous_path: None,
            status: "modified".into(),
            additions: 1,
            deletions: 1,
            patch: Some(PATCH.into()),
            patch_complete: true,
        }],
        complete: true,
        notice: None,
    }
}

fn composition_with_draft() -> (ReviewComposition, Comparison, String) {
    let comparison = comparison(HEAD);
    let session = ReviewSession::new(comparison.clone());
    let coordinate = validate_coordinate(
        &session,
        &file_key(&comparison.files[0]),
        LineSelection::single(DiffSide::New, 10),
    )
    .unwrap();
    let mut composition =
        ReviewComposition::new(key("alice"), comparison.revision.clone()).unwrap();
    let id = composition
        .add_draft(coordinate, "frozen comment")
        .unwrap()
        .id
        .clone();
    (composition, comparison, id)
}

fn context(head: &str, state: &str) -> Value {
    json!({"data": {
        "viewer": {"login": "alice"},
        "repository": {"nameWithOwner": "owner/repo", "pullRequest": {
            "id": "PR_node", "number": 7, "url": "https://github.com/owner/repo/pull/7",
            "headRefOid": head, "state": state
        }}
    }})
}

fn review_node(id: &str, login: &str, commit: &str, state: &str) -> Value {
    json!({"data": {"node": {
        "id": id, "state": state, "author": {"login": login}, "commit": {"oid": commit},
        "pullRequest": {"id": "PR_node", "number": 7, "repository": {"nameWithOwner": "owner/repo"}}
    }}})
}

fn merge_response(head: &str, ref_target: &str, state: &str, queued: bool, auto: bool) -> Value {
    json!({"data": {
        "viewer": {"login": "alice"},
        "repository": {
            "nameWithOwner": "owner/repo", "viewerPermission": "WRITE",
            "mergeCommitAllowed": true, "squashMergeAllowed": true,
            "rebaseMergeAllowed": false, "autoMergeAllowed": true,
            "pullRequest": {
                "id": "PR_node", "number": 7, "url": "https://github.com/owner/repo/pull/7",
                "state": state, "isDraft": false, "headRefOid": head,
                "headRefName": "feature", "mergeable": "MERGEABLE",
                "mergeStateStatus": "CLEAN", "reviewDecision": "APPROVED",
                "viewerCanEnableAutoMerge": !auto, "viewerCanDisableAutoMerge": auto,
                "viewerCanMergeAsAdmin": false, "viewerCanDeleteHeadRef": true,
                "headRepository": {"nameWithOwner": "owner/repo"},
                "headRef": {"id": "REF_node", "name": "feature", "target": {"oid": ref_target}},
                "isMergeQueueEnabled": false, "isInMergeQueue": queued,
                "autoMergeRequest": if auto { json!({"enabledAt": "now"}) } else { Value::Null },
                "mergeQueueEntry": if queued { json!({"id": "QUEUE_node"}) } else { Value::Null },
                "statusCheckRollup": {"state": "SUCCESS"},
                "mergeHeadline": "merge title", "mergeBody": "merge body",
                "squashHeadline": "squash title", "squashBody": "squash body",
                "rebaseHeadline": "rebase title", "rebaseBody": "rebase body"
            }
        }
    }})
}

fn step(marker: &str, variables: Value, response: Value) -> Value {
    json!({"marker": marker, "variables": variables, "response": response})
}

fn rest_step(endpoint: &str, variables: Value, response: Value) -> Value {
    json!({"transport":"rest", "endpoint":endpoint, "variables":variables, "response":response})
}

fn fixture(login: &str, steps: Vec<Value>, timeout: Duration) -> (TempDir, GithubProvider) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("steps.json"),
        serde_json::to_vec(&json!({"login": login, "steps": steps})).unwrap(),
    )
    .unwrap();
    let executable = dir.path().join("gh");
    fs::write(&executable, r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
root = pathlib.Path(__file__).parent
config = json.loads((root / 'steps.json').read_text())
args = sys.argv[1:]
for key in ['GITHUB_TOKEN','GH_ENTERPRISE_TOKEN','GITHUB_ENTERPRISE_TOKEN','GH_HOST','GH_REPO','GH_DEBUG','DEBUG','GH_HTTP_UNIX_SOCKET']:
    assert key not in os.environ
assert os.environ.get('GH_PROMPT_DISABLED') == '1'
token = 'private-' + config['login']
if args[:2] == ['auth', 'token']:
    assert args == ['auth','token','--hostname','github.com','--user',config['login']]
    assert 'GH_TOKEN' not in os.environ
    print(token)
    sys.exit(0)
assert os.environ.get('GH_TOKEN') == token
count = root / 'count'
index = int(count.read_text()) if count.exists() else 0
step = config['steps'][index]
if step.get('transport') == 'rest':
    assert args == ['api','--hostname','github.com','--method','PUT','--header','Accept: application/vnd.github+json','--header','X-GitHub-Api-Version: 2026-03-10',step['endpoint'],'--input','-']
    payload = json.load(sys.stdin)
    assert payload == step['variables'], (payload, step['variables'])
else:
    assert args == ['api','--hostname','github.com','--method','POST','--header','Accept: application/vnd.github+json','--header','X-GitHub-Api-Version: 2026-03-10','graphql','--input','-']
    payload = json.load(sys.stdin)
    assert step['marker'] in payload['query']
    query = payload['query']
    if 'mutation EnableAutoMerge' in query:
        assert 'pullRequestId: $pullRequestId, expectedHeadOid: $expectedHeadOid' in query
    if 'mutation EnqueuePull' in query:
        assert 'pullRequestId: $pullRequestId, expectedHeadOid: $expectedHeadOid' in query
    if 'mutation DequeuePull' in query:
        assert 'id: $pullRequestId, clientMutationId: $clientMutationId' in query
        assert 'pullRequestId: $pullRequestId, clientMutationId: $clientMutationId' not in query
    assert payload['variables'] == step['variables'], (payload['variables'], step['variables'])
count.write_text(str(index + 1))
if step.get('delay_ms'): time.sleep(step['delay_ms'] / 1000)
if step.get('fail'): sys.exit(1)
print(json.dumps(step['response']))
"#).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let provider = GithubProvider {
        account: account(login),
        runner: Runner {
            gh: executable,
            timeout,
            ..Runner::default()
        },
    };
    (dir, provider)
}

fn count(dir: &TempDir) -> usize {
    fs::read_to_string(dir.path().join("count"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[test]
fn first_pending_payload_is_durably_dispatched_and_acknowledged() {
    let (mut composition, comparison, draft_id) = composition_with_draft();
    let intent = composition
        .prepare_pending_comment(
            &draft_id,
            CanonicalPublishedPatch::new(&comparison, &ComparisonMetadata::default()).unwrap(),
        )
        .unwrap();
    let expected = json!({
        "pullRequestId": "PR_node", "commitOID": HEAD, "event": Value::Null,
        "body": Value::Null,
        "threads": [{"body":"frozen comment","path":"src/lib.rs","line":10,"side":"RIGHT"}],
        "clientMutationId": intent.operation_id,
    });
    let steps = vec![
        step(
            "query ReviewActionContext",
            json!({"owner":"owner","name":"repo","number":7}),
            context(HEAD, "OPEN"),
        ),
        step(
            "mutation AddReview",
            expected,
            json!({"data":{"addPullRequestReview":{"pullRequestReview":{
                "id":"REVIEW_1","state":"PENDING","commit":{"oid":HEAD},
                "comments":{"nodes":[{"id":"COMMENT_1","body":"frozen comment","pullRequestReview":{"id":"REVIEW_1"}}]}
            }}}}),
        ),
    ];
    let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
    let store_dir = tempfile::tempdir().unwrap();
    let store = DraftStore::open(store_dir.path()).unwrap();
    let result = provider.execute_review_operation(
        &repo("alice"),
        &mut composition,
        &store,
        &intent.operation_id,
        "attempt-1",
    );
    assert!(matches!(result, ProviderMutationOutcome::Acknowledged(_)));
    assert_eq!(count(&dir), 2);
    let LoadOutcome::Loaded(restored) = store.load(&key("alice")).unwrap() else {
        panic!()
    };
    assert!(matches!(
        restored.operations[0].status,
        ReviewOperationStatus::Acknowledged { .. }
    ));
}

#[test]
fn reused_pending_review_adds_one_thread_with_exact_review_id() {
    let (mut composition, comparison, draft_id) = composition_with_draft();
    composition.observed_pending_review_id = Some("REVIEW_1".into());
    let intent = composition
        .prepare_pending_comment(
            &draft_id,
            CanonicalPublishedPatch::new(&comparison, &ComparisonMetadata::default()).unwrap(),
        )
        .unwrap();
    let steps = vec![
        step(
            "query ReviewActionContext",
            json!({"owner":"owner","name":"repo","number":7}),
            context(HEAD, "OPEN"),
        ),
        step(
            "query ReviewIdentity",
            json!({"id":"REVIEW_1"}),
            review_node("REVIEW_1", "alice", HEAD, "PENDING"),
        ),
        step(
            "mutation AddReviewThread",
            json!({"pullRequestReviewId":"REVIEW_1","body":"frozen comment","path":"src/lib.rs","line":10,"side":"RIGHT","clientMutationId":intent.operation_id}),
            json!({"data":{"addPullRequestReviewThread":{"thread":{"comments":{"nodes":[{"id":"COMMENT_2","body":"frozen comment","pullRequestReview":{"id":"REVIEW_1"}}]}}}}}),
        ),
    ];
    let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
    let store_dir = tempfile::tempdir().unwrap();
    let store = DraftStore::open(store_dir.path()).unwrap();
    assert!(matches!(
        provider.execute_review_operation(
            &repo("alice"),
            &mut composition,
            &store,
            &intent.operation_id,
            "attempt-reuse"
        ),
        ProviderMutationOutcome::Acknowledged(_)
    ));
    assert_eq!(count(&dir), 3);
}

#[test]
fn existing_pending_comment_edits_the_exact_provider_comment() {
    let (mut composition, comparison, draft_id) = composition_with_draft();
    let mut intent = composition
        .prepare_pending_comment(
            &draft_id,
            CanonicalPublishedPatch::new(&comparison, &ComparisonMetadata::default()).unwrap(),
        )
        .unwrap();
    intent.pending_review_id = Some("REVIEW_pending".into());
    intent.existing_comment_id = Some("COMMENT_existing".into());
    composition.operations[0].payload = Some(ReviewOperationPayload::PendingComment(intent.clone()));
    let steps = vec![
        step(
            "query ReviewActionContext",
            json!({"owner":"owner","name":"repo","number":7}),
            context(HEAD, "OPEN"),
        ),
        step(
            "query ReviewIdentity",
            json!({"id":"REVIEW_pending"}),
            review_node("REVIEW_pending", "alice", HEAD, "PENDING"),
        ),
        step(
            "query ReviewCommentIdentity",
            json!({"id":"COMMENT_existing"}),
            json!({"data":{"node":{"id":"COMMENT_existing","author":{"login":"alice"},"pullRequestReview":{"id":"REVIEW_pending","state":"PENDING","author":{"login":"alice"},"commit":{"oid":HEAD},"pullRequest":{"id":"PR_node","number":7,"repository":{"nameWithOwner":"owner/repo"}}}}}}),
        ),
        step(
            "mutation UpdateReviewComment",
            json!({"commentId":"COMMENT_existing","body":"frozen comment","clientMutationId":intent.operation_id}),
            json!({"data":{"updatePullRequestReviewComment":{"pullRequestReviewComment":{"id":"COMMENT_existing","body":"frozen comment","pullRequestReview":{"id":"REVIEW_pending"}}}}}),
        ),
    ];
    let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
    let store_dir = tempfile::tempdir().unwrap();
    let store = DraftStore::open(store_dir.path()).unwrap();
    let result = provider.execute_review_operation(
        &repo("alice"),
        &mut composition,
        &store,
        &intent.operation_id,
        "attempt-edit",
    );
    let ProviderMutationOutcome::Acknowledged(ack) = result else {
        panic!()
    };
    assert_eq!(ack.review_id.as_deref(), Some("REVIEW_pending"));
    assert_eq!(ack.comment_id.as_deref(), Some("COMMENT_existing"));
    assert_eq!(count(&dir), 4);
}

#[test]
fn immediate_comment_does_not_submit_or_replace_pending_review() {
    let (mut composition, comparison, draft_id) = composition_with_draft();
    composition.observed_pending_review_id = Some("REVIEW_PENDING".into());
    let intent = composition
        .prepare_immediate_comment(
            &draft_id,
            CanonicalPublishedPatch::new(&comparison, &ComparisonMetadata::default()).unwrap(),
        )
        .unwrap();
    let steps = vec![
        step(
            "query ReviewActionContext",
            json!({"owner":"owner","name":"repo","number":7}),
            context(HEAD, "OPEN"),
        ),
        step(
            "mutation AddReview",
            json!({"pullRequestId":"PR_node","commitOID":HEAD,"event":"COMMENT","body":Value::Null,"threads":[{"body":"frozen comment","path":"src/lib.rs","line":10,"side":"RIGHT"}],"clientMutationId":intent.operation_id}),
            json!({"data":{"addPullRequestReview":{"pullRequestReview":{"id":"REVIEW_IMMEDIATE","state":"COMMENTED","commit":{"oid":HEAD},"comments":{"nodes":[{"id":"COMMENT_IMMEDIATE","body":"frozen comment","pullRequestReview":{"id":"REVIEW_IMMEDIATE"}}]}}}}}),
        ),
    ];
    let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
    let store_dir = tempfile::tempdir().unwrap();
    let store = DraftStore::open(store_dir.path()).unwrap();
    assert!(matches!(
        provider.execute_review_operation(
            &repo("alice"),
            &mut composition,
            &store,
            &intent.operation_id,
            "attempt-immediate"
        ),
        ProviderMutationOutcome::Acknowledged(_)
    ));
    assert_eq!(
        composition.observed_pending_review_id.as_deref(),
        Some("REVIEW_PENDING")
    );
    assert_eq!(count(&dir), 2);
}

#[test]
fn failed_initial_save_dispatches_zero_mutations() {
    let (mut composition, comparison, draft_id) = composition_with_draft();
    let intent = composition
        .prepare_pending_comment(
            &draft_id,
            CanonicalPublishedPatch::new(&comparison, &ComparisonMetadata::default()).unwrap(),
        )
        .unwrap();
    let (dir, provider) = fixture(
        "alice",
        vec![step(
            "query ReviewActionContext",
            json!({"owner":"owner","name":"repo","number":7}),
            context(HEAD, "OPEN"),
        )],
        Duration::from_secs(30),
    );
    let store_dir = tempfile::tempdir().unwrap();
    let store = DraftStore::open(store_dir.path()).unwrap();
    let path = store.record_path(&key("alice")).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"not-json").unwrap();
    let result = provider.execute_review_operation(
        &repo("alice"),
        &mut composition,
        &store,
        &intent.operation_id,
        "attempt-save-fails",
    );
    let ProviderMutationOutcome::PreflightRejected { reason } = result else {
        panic!("expected rejected save")
    };
    assert!(reason.contains("durably save"), "{reason}");
    assert!(count(&dir) <= 1, "no mutation was dispatched");
    assert_eq!(
        composition.operations[0].status,
        ReviewOperationStatus::Prepared
    );
}

#[test]
fn lost_reply_persists_uncertainty_and_restart_never_replays() {
    let (mut composition, comparison, draft_id) = composition_with_draft();
    let intent = composition
        .prepare_pending_comment(
            &draft_id,
            CanonicalPublishedPatch::new(&comparison, &ComparisonMetadata::default()).unwrap(),
        )
        .unwrap();
    let mut mutation = step(
        "mutation AddReview",
        json!({
            "pullRequestId":"PR_node","commitOID":HEAD,"event":Value::Null,"body":Value::Null,
            "threads":[{"body":"frozen comment","path":"src/lib.rs","line":10,"side":"RIGHT"}],
            "clientMutationId":intent.operation_id,
        }),
        json!({}),
    );
    // The fake records the mutation as server-applied, then withholds the reply
    // beyond the bounded client deadline.
    mutation["delay_ms"] = json!(12000);
    let (dir, provider) = fixture(
        "alice",
        vec![
            step(
                "query ReviewActionContext",
                json!({"owner":"owner","name":"repo","number":7}),
                context(HEAD, "OPEN"),
            ),
            mutation,
        ],
        Duration::from_secs(10),
    );
    let store_dir = tempfile::tempdir().unwrap();
    let store = DraftStore::open(store_dir.path()).unwrap();
    let first = provider.execute_review_operation(
        &repo("alice"),
        &mut composition,
        &store,
        &intent.operation_id,
        "attempt-lost",
    );
    assert!(matches!(first, ProviderMutationOutcome::Uncertain { .. }));
    let LoadOutcome::Loaded(mut restored) = store.load(&key("alice")).unwrap() else {
        panic!()
    };
    assert!(matches!(
        restored.operations[0].status,
        ReviewOperationStatus::Uncertain { .. }
    ));
    let second = provider.execute_review_operation(
        &repo("alice"),
        &mut restored,
        &store,
        &intent.operation_id,
        "attempt-replay",
    );
    assert!(matches!(
        second,
        ProviderMutationOutcome::PreflightRejected { .. }
    ));
    assert_eq!(count(&dir), 2);
}

#[test]
fn older_sha_submission_is_explicit_and_exact() {
    let mut composition = ReviewComposition::new(
        key("alice"),
        Revision {
            base_sha: OLD.into(),
            head_sha: HEAD.into(),
        },
    )
    .unwrap();
    let intent = composition
        .prepare_submission(ReviewEvent::Approve, "ship", Some(NEW))
        .unwrap();
    assert!(intent.newer_head_warning.is_some());
    let expected = json!({"pullRequestId":"PR_node","commitOID":HEAD,"event":"APPROVE","body":"ship","threads":Value::Null,"clientMutationId":intent.operation_id});
    let (dir, provider) = fixture(
        "alice",
        vec![
            step(
                "query ReviewActionContext",
                json!({"owner":"owner","name":"repo","number":7}),
                context(NEW, "OPEN"),
            ),
            step(
                "mutation AddReview",
                expected,
                json!({"data":{"addPullRequestReview":{"pullRequestReview":{"id":"REVIEW_OLD","state":"APPROVED","commit":{"oid":HEAD},"comments":null}}}}),
            ),
        ],
        Duration::from_secs(30),
    );
    let store_dir = tempfile::tempdir().unwrap();
    let store = DraftStore::open(store_dir.path()).unwrap();
    let result = provider.execute_review_operation(
        &repo("alice"),
        &mut composition,
        &store,
        &intent.operation_id,
        "attempt-old",
    );
    assert!(matches!(result, ProviderMutationOutcome::Acknowledged(_)));
    assert_eq!(count(&dir), 2);
}

#[test]
fn foreign_pending_review_is_rejected_before_write() {
    let mut composition = ReviewComposition::new(
        key("alice"),
        Revision {
            base_sha: OLD.into(),
            head_sha: HEAD.into(),
        },
    )
    .unwrap();
    composition.observed_pending_review_id = Some("REVIEW_1".into());
    let intent = composition
        .prepare_submission(ReviewEvent::Comment, "summary", None)
        .unwrap();
    let (dir, provider) = fixture(
        "alice",
        vec![
            step(
                "query ReviewActionContext",
                json!({"owner":"owner","name":"repo","number":7}),
                context(HEAD, "OPEN"),
            ),
            step(
                "query ReviewIdentity",
                json!({"id":"REVIEW_1"}),
                review_node("REVIEW_1", "mallory", HEAD, "PENDING"),
            ),
        ],
        Duration::from_secs(30),
    );
    let store_dir = tempfile::tempdir().unwrap();
    let store = DraftStore::open(store_dir.path()).unwrap();
    let result = provider.execute_review_operation(
        &repo("alice"),
        &mut composition,
        &store,
        &intent.operation_id,
        "attempt-foreign",
    );
    assert!(matches!(
        result,
        ProviderMutationOutcome::PreflightRejected { .. }
    ));
    assert_eq!(count(&dir), 2);
}

#[test]
fn submitted_pending_id_is_stale_and_rejected_before_write() {
    let mut composition = ReviewComposition::new(
        key("alice"),
        Revision {
            base_sha: OLD.into(),
            head_sha: HEAD.into(),
        },
    )
    .unwrap();
    composition.observed_pending_review_id = Some("REVIEW_stale".into());
    let intent = composition
        .prepare_submission(ReviewEvent::Comment, "summary", None)
        .unwrap();
    let (dir, provider) = fixture(
        "alice",
        vec![
            step(
                "query ReviewActionContext",
                json!({"owner":"owner","name":"repo","number":7}),
                context(HEAD, "OPEN"),
            ),
            step(
                "query ReviewIdentity",
                json!({"id":"REVIEW_stale"}),
                review_node("REVIEW_stale", "alice", HEAD, "COMMENTED"),
            ),
        ],
        Duration::from_secs(30),
    );
    let store_dir = tempfile::tempdir().unwrap();
    let store = DraftStore::open(store_dir.path()).unwrap();
    assert!(matches!(
        provider.execute_review_operation(
            &repo("alice"),
            &mut composition,
            &store,
            &intent.operation_id,
            "attempt-stale"
        ),
        ProviderMutationOutcome::PreflightRejected { .. }
    ));
    assert_eq!(count(&dir), 2);
}

#[test]
fn pending_import_links_only_provider_reported_review_comments_and_reports_cap() {
    let response = json!({"data":{"repository":{"nameWithOwner":"owner/repo","pullRequest":{"number":7,"reviews":{
        "nodes":[{"id":"REVIEW_1","author":{"login":"alice"},"body":"draft summary","state":"PENDING","submittedAt":null,"commit":{"oid":HEAD},"url":"https://github.com/owner/repo/pull/7#review",
            "comments":{"nodes":[{"id":"COMMENT_1","author":{"login":"alice"},"body":"draft","createdAt":"a","updatedAt":"b","url":"u","path":"src/lib.rs","line":10,"originalLine":null,"startLine":null,"originalStartLine":null,"diffHunk":"@@","outdated":false,"commit":{"oid":HEAD},"originalCommit":{"oid":HEAD}}],"pageInfo":{"hasNextPage":true,"endCursor":"next"}}}],
        "pageInfo":{"hasNextPage":false,"endCursor":null}}}}}});
    let (dir, provider) = fixture(
        "alice",
        vec![step(
            "query PendingReview",
            json!({"owner":"owner","name":"repo","number":7}),
            response,
        )],
        Duration::from_secs(30),
    );
    let pending = provider.pending_review(&repo("alice"), 7).unwrap().unwrap();
    assert_eq!(pending.review.coordinates.remote_id, "REVIEW_1");
    assert_eq!(pending.comments[0].pull_request_review_id, "REVIEW_1");
    assert!(!pending.comments_complete);
    assert_eq!(count(&dir), 1);
}

#[test]
fn concurrent_named_accounts_keep_credentials_and_ids_isolated() {
    let response = |login: &str, review: &str| {
        json!({"data":{"repository":{"nameWithOwner":"owner/repo","pullRequest":{"number":7,"reviews":{
        "nodes":[{"id":review,"author":{"login":login},"body":"","state":"PENDING","submittedAt":null,"commit":{"oid":HEAD},"url":"u","comments":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}],
        "pageInfo":{"hasNextPage":false,"endCursor":null}}}}}})
    };
    let (alice_dir, alice) = fixture(
        "alice",
        vec![step(
            "query PendingReview",
            json!({"owner":"owner","name":"repo","number":7}),
            response("alice", "ALICE_REVIEW"),
        )],
        Duration::from_secs(30),
    );
    let (bob_dir, bob) = fixture(
        "bob",
        vec![step(
            "query PendingReview",
            json!({"owner":"owner","name":"repo","number":7}),
            response("bob", "BOB_REVIEW"),
        )],
        Duration::from_secs(30),
    );
    let alice_task = std::thread::spawn(move || {
        alice
            .pending_review(&repo("alice"), 7)
            .unwrap()
            .unwrap()
            .review
            .coordinates
            .remote_id
    });
    let bob_task = std::thread::spawn(move || {
        bob.pending_review(&repo("bob"), 7)
            .unwrap()
            .unwrap()
            .review
            .coordinates
            .remote_id
    });
    assert_eq!(alice_task.join().unwrap(), "ALICE_REVIEW");
    assert_eq!(bob_task.join().unwrap(), "BOB_REVIEW");
    assert_eq!(count(&alice_dir), 1);
    assert_eq!(count(&bob_dir), 1);
}

#[test]
fn merge_payloads_always_include_expected_head_and_capability_blocks() {
    let (dir, provider) = fixture(
        "alice",
        vec![step(
            "query MergePreparation",
            json!({"owner":"owner","name":"repo","number":7}),
            merge_response(HEAD, HEAD, "OPEN", false, false),
        )],
        Duration::from_secs(30),
    );
    let preparation = provider.prepare_merge(&repo("alice"), 7, HEAD).unwrap();
    assert_eq!(
        preparation.allowed_methods,
        vec![MergeMethod::Merge, MergeMethod::Squash]
    );
    let request = MergeExecutionRequest {
        operation_id: "merge-1".into(),
        attempt_id: "try-1".into(),
        action: MergeAction::EnableAutoMerge {
            method: MergeMethod::Squash,
            commit_title: None,
            commit_message: None,
        },
    };
    let prepared = prepare_merge_mutation(&preparation, &request).unwrap();
    assert_eq!(prepared.variables["expectedHeadOid"], HEAD);
    let unavailable = MergeExecutionRequest {
        operation_id: "merge-2".into(),
        attempt_id: "try-2".into(),
        action: MergeAction::Merge {
            method: MergeMethod::Rebase,
            commit_title: None,
            commit_message: None,
        },
    };
    assert!(prepare_merge_mutation(&preparation, &unavailable).is_err());
    let ordinary = MergeExecutionRequest {
        operation_id: "merge-3".into(),
        attempt_id: "try-3".into(),
        action: MergeAction::Merge {
            method: MergeMethod::Squash,
            commit_title: Some("title".into()),
            commit_message: Some("body".into()),
        },
    };
    let ordinary = prepare_merge_mutation(&preparation, &ordinary).unwrap();
    assert_eq!(ordinary.variables["sha"], HEAD);
    let mut blocked = preparation.clone();
    blocked.blockers.push("check status is FAILURE".into());
    let blocked_request = MergeExecutionRequest {
        operation_id: "merge-blocked".into(),
        attempt_id: "try-blocked".into(),
        action: MergeAction::Merge {
            method: MergeMethod::Merge,
            commit_title: None,
            commit_message: None,
        },
    };
    assert!(prepare_merge_mutation(&blocked, &blocked_request).is_err());
    assert_eq!(count(&dir), 1);
}

#[test]
fn ordinary_merge_sends_one_expected_sha_write_and_reconciles_completion() {
    let preparation = sample_preparation();
    let request = MergeExecutionRequest {
        operation_id: "merge-once".into(),
        attempt_id: "try-merge".into(),
        action: MergeAction::Merge {
            method: MergeMethod::Squash,
            commit_title: Some("title".into()),
            commit_message: Some("body".into()),
        },
    };
    let steps = vec![
        step(
            "query MergePreparation",
            json!({"owner":"owner","name":"repo","number":7}),
            merge_response(HEAD, HEAD, "OPEN", false, false),
        ),
        rest_step(
            "repos/owner/repo/pulls/7/merge",
            json!({"sha":HEAD,"merge_method":"squash","commit_title":"title","commit_message":"body"}),
            json!({"sha":NEW,"merged":true}),
        ),
        step(
            "query MergePreparation",
            json!({"owner":"owner","name":"repo","number":7}),
            merge_response(HEAD, HEAD, "MERGED", false, false),
        ),
    ];
    let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
    let result = provider.execute_merge(&repo("alice"), &preparation, &request);
    let ProviderMutationOutcome::Acknowledged(ack) = result else {
        panic!()
    };
    assert!(ack.completed);
    assert!(ack.merged);
    assert_eq!(ack.merge_commit_sha.as_deref(), Some(NEW));
    assert_eq!(count(&dir), 3);
}

#[test]
fn auto_merge_acknowledgement_is_accepted_but_not_completed() {
    let preparation = sample_preparation();
    let request = MergeExecutionRequest {
        operation_id: "auto-1".into(),
        attempt_id: "try-auto".into(),
        action: MergeAction::EnableAutoMerge {
            method: MergeMethod::Squash,
            commit_title: None,
            commit_message: None,
        },
    };
    let variables = json!({"pullRequestId":"PR_node","expectedHeadOid":HEAD,"mergeMethod":"SQUASH","commitHeadline":Value::Null,"commitBody":Value::Null,"clientMutationId":"auto-1"});
    let steps = vec![
        step(
            "query MergePreparation",
            json!({"owner":"owner","name":"repo","number":7}),
            merge_response(HEAD, HEAD, "OPEN", false, false),
        ),
        step(
            "mutation EnableAutoMerge",
            variables,
            json!({"data":{"enablePullRequestAutoMerge":{"clientMutationId":"auto-1","pullRequest":{"id":"PR_node","state":"OPEN","mergedAt":null,"autoMergeRequest":{"enabledAt":"now"}}}}}),
        ),
        step(
            "query MergePreparation",
            json!({"owner":"owner","name":"repo","number":7}),
            merge_response(HEAD, HEAD, "OPEN", false, true),
        ),
    ];
    let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
    let result = provider.execute_merge(&repo("alice"), &preparation, &request);
    let ProviderMutationOutcome::Acknowledged(ack) = result else {
        panic!()
    };
    assert!(ack.accepted);
    assert!(!ack.completed);
    assert!(!ack.merged);
    assert_eq!(count(&dir), 3);
}

#[test]
fn dequeue_uses_schema_id_field_and_reconciles_without_replay() {
    let mut preparation = sample_preparation();
    preparation.in_merge_queue = true;
    let request = MergeExecutionRequest {
        operation_id: "dequeue-1".into(),
        attempt_id: "try-dequeue".into(),
        action: MergeAction::Dequeue,
    };
    let steps = vec![
        step(
            "query MergePreparation",
            json!({"owner":"owner","name":"repo","number":7}),
            merge_response(HEAD, HEAD, "OPEN", true, false),
        ),
        step(
            "mutation DequeuePull",
            json!({"pullRequestId":"PR_node","clientMutationId":"dequeue-1"}),
            json!({"data":{"dequeuePullRequest":{"clientMutationId":"dequeue-1","mergeQueueEntry":null}}}),
        ),
        step(
            "query MergePreparation",
            json!({"owner":"owner","name":"repo","number":7}),
            merge_response(HEAD, HEAD, "OPEN", false, false),
        ),
    ];
    let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
    let result = provider.execute_merge(&repo("alice"), &preparation, &request);
    let ProviderMutationOutcome::Acknowledged(ack) = result else {
        panic!()
    };
    assert!(ack.accepted);
    assert!(!ack.completed);
    assert_eq!(count(&dir), 3);
}

#[test]
fn moving_head_and_advanced_branch_ref_fail_closed_without_writes() {
    let preparation_provider_steps = vec![step(
        "query MergePreparation",
        json!({"owner":"owner","name":"repo","number":7}),
        merge_response(NEW, NEW, "OPEN", false, false),
    )];
    let (dir, provider) = fixture("alice", preparation_provider_steps, Duration::from_secs(30));
    let mut preparation = sample_preparation();
    let request = MergeExecutionRequest {
        operation_id: "merge-moved".into(),
        attempt_id: "try".into(),
        action: MergeAction::Merge {
            method: MergeMethod::Merge,
            commit_title: None,
            commit_message: None,
        },
    };
    assert!(matches!(
        provider.execute_merge(&repo("alice"), &preparation, &request),
        ProviderMutationOutcome::PreflightRejected { .. }
    ));
    assert_eq!(count(&dir), 1);

    let (dir, provider) = fixture(
        "alice",
        vec![step(
            "query MergePreparation",
            json!({"owner":"owner","name":"repo","number":7}),
            merge_response(HEAD, NEW, "MERGED", false, false),
        )],
        Duration::from_secs(30),
    );
    preparation.state = "MERGED".into();
    let delete = $crate::domain::BranchDeletionRequest {
        operation_id: "delete-1".into(),
        attempt_id: "try".into(),
        expected_merged_head_sha: HEAD.into(),
    };
    assert!(matches!(
        provider.delete_merged_branch(&repo("alice"), &preparation, &delete),
        ProviderMutationOutcome::PreflightRejected { .. }
    ));
    assert_eq!(count(&dir), 1);
}

#[test]
fn source_branch_with_open_descendant_pr_is_not_deletable() {
    let preparation = sample_preparation();
    let delete = $crate::domain::BranchDeletionRequest {
        operation_id: "delete-dependent".into(),
        attempt_id: "try-dependent".into(),
        expected_merged_head_sha: HEAD.into(),
    };
    let steps = vec![
        step(
            "query MergePreparation",
            json!({"owner":"owner","name":"repo","number":7}),
            merge_response(HEAD, HEAD, "MERGED", false, false),
        ),
        step(
            "query DependentPullRequests",
            json!({"owner":"owner","name":"repo","base":"feature"}),
            json!({"data":{"repository":{"nameWithOwner":"owner/repo","pullRequests":{"nodes":[{"number":8}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}),
        ),
    ];
    let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
    let ProviderMutationOutcome::PreflightRejected { reason } =
        provider.delete_merged_branch(&repo("alice"), &preparation, &delete)
    else {
        panic!()
    };
    assert!(reason.contains("descendant"), "{reason}");
    assert_eq!(count(&dir), 2);
}

fn sample_preparation() -> $crate::domain::MergePreparation {
    $crate::domain::MergePreparation {
        pull_request: ProviderCoordinates {
            provider: "github".into(),
            host: "github.com".into(),
            owner: "owner".into(),
            repository: "repo".into(),
            pull_request: 7,
            remote_id: "PR_node".into(),
        },
        pull_request_node_id: "PR_node".into(),
        reviewed_head_sha: HEAD.into(),
        current_head_sha: HEAD.into(),
        head_ref_name: "feature".into(),
        head_ref_node_id: Some("REF_node".into()),
        head_repository: "owner/repo".into(),
        state: "OPEN".into(),
        draft: false,
        mergeable: "MERGEABLE".into(),
        merge_state_status: "CLEAN".into(),
        review_status: "APPROVED".into(),
        check_status: "SUCCESS".into(),
        repository_permission: Some("WRITE".into()),
        allowed_methods: vec![MergeMethod::Merge, MergeMethod::Squash],
        blockers: vec![],
        auto_merge_allowed: true,
        auto_merge_enabled: false,
        can_enable_auto_merge: true,
        can_disable_auto_merge: false,
        merge_queue_required: false,
        in_merge_queue: false,
        viewer_can_merge_as_admin: false,
        viewer_can_delete_head_ref: true,
        preferred_headlines: vec![],
        preferred_bodies: vec![],
    }
}

#[test]
fn auxiliary_reply_payload_keeps_graphql_ids_distinct() {
    let request = ReviewAuxiliaryRequest {
        operation_id: "reply-1".into(),
        attempt_id: "try-1".into(),
        action: ReviewAuxiliaryAction::Reply {
            thread: ProviderCoordinates {
                provider: "github".into(),
                host: "github.com".into(),
                owner: "owner".into(),
                repository: "repo".into(),
                pull_request: 7,
                remote_id: "THREAD_node".into(),
            },
            pending_review: None,
            body: "reply".into(),
        },
    };
    let steps = vec![
        step(
            "query ReviewActionContext",
            json!({"owner":"owner","name":"repo","number":7}),
            context(HEAD, "OPEN"),
        ),
        step(
            "query ReviewThreadIdentity",
            json!({"id":"THREAD_node"}),
            json!({"data":{"node":{"id":"THREAD_node","isResolved":false,"viewerCanReply":true,"viewerCanResolve":true,"viewerCanUnresolve":false,"pullRequest":{"id":"PR_node","number":7,"repository":{"nameWithOwner":"owner/repo"}}}}}),
        ),
        step(
            "mutation ReplyReviewThread",
            json!({"threadId":"THREAD_node","reviewId":Value::Null,"body":"reply","clientMutationId":"reply-1"}),
            json!({"data":{"addPullRequestReviewThreadReply":{"comment":{"id":"COMMENT_reply","pullRequestReview":{"id":"REVIEW_immediate"}}}}}),
        ),
    ];
    let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
    assert!(matches!(
        provider.execute_review_auxiliary(&repo("alice"), 7, &request),
        ProviderMutationOutcome::Acknowledged(_)
    ));
    assert_eq!(count(&dir), 3);
}

#[test]
fn thread_resolution_checks_capability_and_exact_thread_id() {
    let request = ReviewAuxiliaryRequest {
        operation_id: "resolve-1".into(),
        attempt_id: "try-resolve".into(),
        action: ReviewAuxiliaryAction::SetThreadResolved {
            thread: ProviderCoordinates {
                provider: "github".into(),
                host: "github.com".into(),
                owner: "owner".into(),
                repository: "repo".into(),
                pull_request: 7,
                remote_id: "THREAD_node".into(),
            },
            resolved: true,
        },
    };
    let steps = vec![
        step(
            "query ReviewActionContext",
            json!({"owner":"owner","name":"repo","number":7}),
            context(HEAD, "OPEN"),
        ),
        step(
            "query ReviewThreadIdentity",
            json!({"id":"THREAD_node"}),
            json!({"data":{"node":{"id":"THREAD_node","isResolved":false,"viewerCanReply":true,"viewerCanResolve":true,"viewerCanUnresolve":false,"pullRequest":{"id":"PR_node","number":7,"repository":{"nameWithOwner":"owner/repo"}}}}}),
        ),
        step(
            "mutation ResolveReviewThread",
            json!({"threadId":"THREAD_node","clientMutationId":"resolve-1"}),
            json!({"data":{"resolveReviewThread":{"thread":{"id":"THREAD_node","isResolved":true}}}}),
        ),
    ];
    let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
    let result = provider.execute_review_auxiliary(&repo("alice"), 7, &request);
    let ProviderMutationOutcome::Acknowledged(ack) = result else {
        panic!()
    };
    assert_eq!(ack.thread_id.as_deref(), Some("THREAD_node"));
    assert_eq!(ack.resolved, Some(true));
    assert_eq!(count(&dir), 3);
}

#[test]
fn foreign_thread_pr_identity_is_rejected_before_mutation() {
    let request = ReviewAuxiliaryRequest {
        operation_id: "resolve-foreign".into(),
        attempt_id: "try-foreign".into(),
        action: ReviewAuxiliaryAction::SetThreadResolved {
            thread: ProviderCoordinates {
                provider: "github".into(),
                host: "github.com".into(),
                owner: "owner".into(),
                repository: "repo".into(),
                pull_request: 7,
                remote_id: "THREAD_foreign".into(),
            },
            resolved: true,
        },
    };
    let steps = vec![
        step(
            "query ReviewActionContext",
            json!({"owner":"owner","name":"repo","number":7}),
            context(HEAD, "OPEN"),
        ),
        step(
            "query ReviewThreadIdentity",
            json!({"id":"THREAD_foreign"}),
            json!({"data":{"node":{"id":"THREAD_foreign","isResolved":false,"viewerCanReply":true,"viewerCanResolve":true,"viewerCanUnresolve":false,"pullRequest":{"id":"OTHER_PR","number":8,"repository":{"nameWithOwner":"owner/other"}}}}}),
        ),
    ];
    let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
    assert!(matches!(
        provider.execute_review_auxiliary(&repo("alice"), 7, &request),
        ProviderMutationOutcome::PreflightRejected { .. }
    ));
    assert_eq!(count(&dir), 2);
}

#[test]
#[ignore = "uses existing MatiasEnrique gh auth for read-only public cli/cli API probes"]
fn live_public_pending_review_and_merge_preparation_queries() {
    let selected = GithubProvider::accounts()
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.login.eq_ignore_ascii_case("MatiasEnrique"))
        .expect("MatiasEnrique GitHub account is unavailable");
    let provider = GithubProvider::new(selected);
    let repository = provider.repository("cli/cli").unwrap();
    let pull = provider.pull_request(&repository, 14_398).unwrap();
    let pending = provider
        .pending_review(&repository, pull.number)
        .expect("pending_review query failed");
    if let Some(pending) = pending {
        assert_eq!(pending.review.coordinates.pull_request, pull.number);
        assert!(pending.comments.iter().all(|comment| {
            comment.pull_request_review_id == pending.review.coordinates.remote_id
                && comment.comment.coordinates.pull_request == pull.number
        }));
    }
    let preparation = provider
        .prepare_merge(&repository, pull.number, &pull.head_sha)
        .expect("prepare_merge query failed");
    assert_eq!(preparation.pull_request.pull_request, pull.number);
    assert_eq!(preparation.reviewed_head_sha, pull.head_sha);
    assert_eq!(preparation.current_head_sha, pull.head_sha);
}
    };
}
