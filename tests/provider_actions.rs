#[macro_export]
macro_rules! provider_action_tests {
    () => {
use super::*;
use $crate::{
    domain::{
        Account, ChangedFile, Comparison, MergeAction, MergeExecutionRequest, MergeMethod,
        PendingFileCommentSource, PendingFileReviewAbsence, ProviderCoordinates,
        ProviderMutationOutcome, Repository, ReviewAuxiliaryAction, ReviewAuxiliaryRequest,
        ReviewSubject, Revision,
    },
    participation::{
        CanonicalPublishedPatch, DiffSide, DraftStore, LineSelection, LoadOutcome,
        ReviewComposition, ReviewEvent, ReviewKey, ReviewOperationStatus,
        map_file_to_canonical_published, validate_coordinate,
    },
    review::{ComparisonMetadata, ReviewSession, file_key},
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
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

fn coordinates(remote_id: &str) -> ProviderCoordinates {
    ProviderCoordinates {
        provider: "github".into(),
        host: "github.com".into(),
        owner: "owner".into(),
        repository: "repo".into(),
        pull_request: 7,
        remote_id: remote_id.into(),
    }
}

fn composition_with_file_draft() -> (ReviewComposition, PendingFileCommentSource, String) {
    let mut canonical = comparison(HEAD);
    canonical.files[0].previous_path = Some("src/old-lib.rs".into());
    canonical.files[0].patch = None;
    canonical.files[0].patch_complete = false;
    let session = ReviewSession::new(canonical.clone());
    let file = map_file_to_canonical_published(
        &session,
        &file_key(&canonical.files[0]),
        CanonicalPublishedPatch::new(&canonical, &ComparisonMetadata::default()).unwrap(),
    )
    .unwrap();
    let mut composition = ReviewComposition::new(key("alice"), canonical.revision).unwrap();
    let draft_id = composition
        .add_file_draft(file, "exact whole-file comment")
        .unwrap()
        .id
        .clone();
    let source = PendingFileCommentSource {
        viewer_login: "alice".into(),
        repository: repo("alice"),
        pull_request: coordinates("PR_node"),
        pull_request_state: "OPEN".into(),
        current_base_sha: OLD.into(),
        current_head_sha: HEAD.into(),
        review: coordinates("REVIEW_pending"),
        review_author: "alice".into(),
        review_commit_sha: HEAD.into(),
    };
    (composition, source, draft_id)
}

fn pending_file_preflight(head: &str, author: &str, review_id: &str) -> Value {
    json!({"data": {
        "viewer": {"login": "alice"},
        "repository": {"nameWithOwner": "owner/repo", "pullRequest": {
            "id": "PR_node", "number": 7,
            "url": "https://github.com/owner/repo/pull/7", "state": "OPEN",
            "baseRefOid": OLD, "headRefOid": head,
            "reviews": {"nodes": [{
                "id": review_id, "state": "PENDING", "submittedAt": null,
                "author": {"login": author}, "commit": {"oid": HEAD}
            }], "pageInfo": {"hasNextPage": false, "endCursor": null}}
        }}
    }})
}

fn pending_review_read(nodes: Vec<Value>) -> Value {
    json!({"data": {
        "viewer": {"login": "alice"},
        "repository": {"nameWithOwner": "owner/repo", "pullRequest": {
            "id": "PR_node", "number": 7,
            "url": "https://github.com/owner/repo/pull/7", "state": "OPEN",
            "baseRefOid": OLD, "headRefOid": HEAD,
            "reviews": {"nodes": nodes,
                "pageInfo": {"hasNextPage": false, "endCursor": null}}
        }}
    }})
}

fn pending_review_row(id: &str, author: Value) -> Value {
    json!({
        "id": id, "author": author, "body": "", "state": "PENDING",
        "submittedAt": null, "commit": {"oid": HEAD},
        "url": format!("https://github.com/owner/repo/pull/7#pullrequestreview-{id}"),
        "comments": {"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}}
    })
}

fn pending_review_create_ack(operation_id: &str, review_id: &str, author: &str) -> Value {
    json!({"data": {"createEmptyPendingReview": {
        "clientMutationId": operation_id,
        "pullRequestReview": {
            "id": review_id, "state": "PENDING", "submittedAt": null,
            "author": {"login": author}, "commit": {"oid": HEAD},
            "pullRequest": {"id": "PR_node", "number": 7,
                "repository": {"nameWithOwner": "owner/repo"}}
        }
    }}})
}

fn pending_start_intent() -> $crate::participation::PendingFileReviewStartIntent {
    let (composition, _, draft_id) = composition_with_file_draft();
    composition
        .prepare_pending_file_review_start(
            &draft_id,
            &PendingFileReviewAbsence {
                viewer_login: "alice".into(),
                repository: repo("alice"),
                pull_request: coordinates("PR_node"),
                pull_request_state: "OPEN".into(),
                current_base_sha: OLD.into(),
                current_head_sha: HEAD.into(),
            },
            "pending-start-flow-1".into(),
            "pending-start-create-1".into(),
            "pending-start-thread-1".into(),
        )
        .unwrap()
}

fn pending_file_ack(operation_id: &str, subject: &str, body: &str, review_id: &str) -> Value {
    json!({"data": {"addPendingFileReviewThread": {
        "clientMutationId": operation_id,
        "thread": {
            "id": "THREAD_new", "path": "src/lib.rs", "subjectType": subject,
            "comments": {"totalCount": 1, "nodes": [{
                "id": "COMMENT_new", "body": body, "path": "src/lib.rs",
                "subjectType": subject, "author": {"login": "alice"},
                "pullRequestReview": {
                    "id": review_id, "state": "PENDING", "submittedAt": null,
                    "author": {"login": "alice"}, "commit": {"oid": HEAD},
                    "pullRequest": {"id": "PR_node", "number": 7,
                        "repository": {"nameWithOwner": "owner/repo"}}
                }
            }], "pageInfo": {"hasNextPage": false, "endCursor": null}}
        }
    }}})
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

fn submitted_review_node(
    id: &str,
    login: &str,
    commit: &str,
    state: &str,
    body: &str,
) -> Value {
    json!({"data": {"node": {
        "id": id, "body": body, "state": state,
        "submittedAt": "2026-09-13T12:00:00Z",
        "author": {"login": login}, "commit": {"oid": commit},
        "viewerDidAuthor": true, "viewerCanUpdate": true,
        "viewerCannotUpdateReasons": [],
        "pullRequest": {"id": "PR_node", "number": 7, "repository": {"nameWithOwner": "owner/repo"}}
    }}})
}

fn submitted_edit_request() -> ReviewAuxiliaryRequest {
    ReviewAuxiliaryRequest {
        operation_id: "submitted-edit-1".into(),
        attempt_id: "submitted-edit-attempt-1".into(),
        action: ReviewAuxiliaryAction::UpdateSubmittedSummary {
            review: ProviderCoordinates {
                provider: "github".into(),
                host: "github.com".into(),
                owner: "owner".into(),
                repository: "repo".into(),
                pull_request: 7,
                remote_id: "REVIEW_submitted".into(),
            },
            selected_author: "alice".into(),
            submitted_state: "APPROVED".into(),
            submitted_commit_sha: OLD.into(),
            expected_body: "before".into(),
            body: String::new(),
        },
    }
}

fn submitted_edit_ack(
    operation_id: &str,
    id: &str,
    login: &str,
    commit: &str,
    state: &str,
    body: &str,
) -> Value {
    json!({"data": {"updateSubmittedPullRequestReview": {
        "clientMutationId": operation_id,
        "pullRequestReview": {
            "id": id, "body": body, "state": state,
            "author": {"login": login}, "commit": {"oid": commit},
            "pullRequest": {"id": "PR_node", "number": 7, "repository": {"nameWithOwner": "owner/repo"}}
        }
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
import json, os, pathlib, stat, sys, time
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
    assert payload == step['variables']
else:
    assert args == ['api','--hostname','github.com','--method','POST','--header','Accept: application/vnd.github+json','--header','X-GitHub-Api-Version: 2026-03-10','graphql','--input','-']
    payload = json.load(sys.stdin)
    assert step['marker'] in payload['query']
    query = ' '.join(payload['query'].split())
    if 'mutation AddReview(' in query:
        assert 'addPullRequestReview(input: { pullRequestId: $pullRequestId, commitOID: $commitOID, event: $event, body: $body, threads: $threads, clientMutationId: $clientMutationId })' in query
    if 'mutation AddReviewThread(' in query:
        assert 'addPullRequestReviewThread(input: { pullRequestReviewId: $pullRequestReviewId, body: $body, path: $path, line: $line, side: $side, startLine: $startLine, startSide: $startSide, clientMutationId: $clientMutationId })' in query
    if 'mutation AddPendingFileReviewThread(' in query:
        assert 'subjectType: $subjectType' in query
        mutation_input = query.split('addPendingFileReviewThread: addPullRequestReviewThread(input:', 1)[1].split('})', 1)[0]
        for forbidden in ['line:', 'side:', 'startLine:', 'startSide:', 'pullRequestId:', 'event:']:
            assert forbidden not in mutation_input
    if 'mutation CreateEmptyPendingReview(' in query:
        assert 'createEmptyPendingReview: addPullRequestReview(input:' in query
        assert 'commitOID: $commitOID' in query
        assert 'clientMutationId pullRequestReview { id state submittedAt author { login } commit { oid } pullRequest { id number repository { nameWithOwner } } }' in query
        assert payload['variables']['event'] is None
        assert payload['variables']['body'] is None
        assert payload['variables']['threads'] is None
    if 'mutation UpdateReviewComment(' in query:
        assert 'updatePullRequestReviewComment(input: { pullRequestReviewCommentId: $commentId, body: $body, clientMutationId: $clientMutationId })' in query
    if 'mutation SubmitReview(' in query:
        assert 'submitPullRequestReview(input: { pullRequestReviewId: $reviewId, event: $event, body: $body, clientMutationId: $clientMutationId })' in query
    if 'mutation UpdatePendingReview(' in query:
        assert 'updatePullRequestReview(input: { pullRequestReviewId: $reviewId, body: $body, clientMutationId: $clientMutationId })' in query
    if 'mutation UpdateSubmittedReviewSummary(' in query:
        assert 'updateSubmittedPullRequestReview: updatePullRequestReview(input: { pullRequestReviewId: $reviewId, body: $body, clientMutationId: $clientMutationId })' in query
        assert 'clientMutationId pullRequestReview { id body state author { login } commit { oid } pullRequest { id number repository { nameWithOwner } } }' in query
        assert 'event:' not in query
    if 'query ReviewIdentity(' in query:
        assert 'id body state submittedAt author { login } commit { oid } viewerDidAuthor viewerCanUpdate viewerCannotUpdateReasons' in query
    if 'mutation DeletePendingReviewComment(' in query:
        assert 'deletePullRequestReviewComment(input: { id: $commentId, clientMutationId: $clientMutationId })' in query
        assert 'pullRequestReviewCommentId: $commentId' not in query
    if 'mutation CancelPendingReview(' in query:
        assert 'deletePullRequestReview(input: { pullRequestReviewId: $reviewId, clientMutationId: $clientMutationId })' in query
    if 'mutation ReplyReviewThread(' in query:
        assert 'addPullRequestReviewThreadReply(input: { pullRequestReviewThreadId: $threadId, pullRequestReviewId: $reviewId, body: $body, clientMutationId: $clientMutationId })' in query
    if 'mutation ResolveReviewThread(' in query:
        assert 'resolveReviewThread(input: { threadId: $threadId, clientMutationId: $clientMutationId })' in query
    if 'mutation UnresolveReviewThread(' in query:
        assert 'unresolveReviewThread(input: { threadId: $threadId, clientMutationId: $clientMutationId })' in query
    if 'mutation EnableAutoMerge' in query:
        assert 'pullRequestId: $pullRequestId, expectedHeadOid: $expectedHeadOid' in query
    if 'mutation EnqueuePull' in query:
        assert 'pullRequestId: $pullRequestId, expectedHeadOid: $expectedHeadOid' in query
    if 'mutation DequeuePull' in query:
        assert 'id: $pullRequestId, clientMutationId: $clientMutationId' in query
        assert 'pullRequestId: $pullRequestId, clientMutationId: $clientMutationId' not in query
    if 'query PendingReview' in query:
        assert 'pullRequestReview { id }' in query
    assert payload['variables'] == step['variables']
    if query.startswith('mutation '):
        mutations = root / 'mutations'
        mutation_count = int(mutations.read_text()) if mutations.exists() else 0
        mutations.write_text(str(mutation_count + 1))
count.write_text(str(index + 1))
if step.get('immutable_path'):
    os.chflags(step['immutable_path'], stat.UF_IMMUTABLE)
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

fn no_read_fixture(timeout: Duration) -> (TempDir, GithubProvider) {
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("gh");
    fs::write(
        &executable,
        r#"#!/bin/sh
root=${0%/*}
if [ "$1" = "auth" ] && [ "$2" = "token" ]; then
    [ -z "$GH_TOKEN" ] || exit 20
    printf '%s\n' 'private-alice'
    exit 0
fi
[ "$GH_TOKEN" = "private-alice" ] || exit 21
if [ ! -f "$root/count" ]; then
    /bin/cat >/dev/null
    printf '%s' 1 >"$root/count"
    printf '%s\n' '{"data":{"viewer":{"login":"alice"},"repository":{"nameWithOwner":"owner/repo","pullRequest":{"id":"PR_node","number":7,"url":"https://github.com/owner/repo/pull/7","headRefOid":"2222222222222222222222222222222222222222","state":"OPEN"}}}}'
    exit 0
fi
printf '%s' 2 >"$root/count"
printf '%s\n' $$ >"$CIBERGIT_PROVIDER_NO_READ_GROUP_PATH"
/bin/sleep 30 <&0 >&1 2>&2 &
helper=$!
printf '%s\n' "$helper" >"$CIBERGIT_PROVIDER_NO_READ_HELPER_PATH"
kill -0 "$helper" || exit 22
: >"$CIBERGIT_PROVIDER_NO_READ_STARTED_PATH"
/bin/sleep 30
"#,
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let provider = GithubProvider {
        account: account("alice"),
        runner: Runner {
            gh: executable,
            timeout: Duration::from_secs(30),
            input_timeout: Some(timeout),
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

fn recorded_pid(path: &Path) -> u32 {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("missing process record {}: {error}", path.display()))
        .trim()
        .parse()
        .unwrap_or_else(|error| panic!("invalid process record {}: {error}", path.display()))
}

fn process_state(pid: u32) -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    if !output.status.success() {
        return None;
    }
    let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!state.is_empty()).then_some(state)
}

fn assert_processes_not_live_after_grace(label: &str, pids: &[u32]) {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let live = pids
            .iter()
            .filter_map(|pid| {
                process_state(*pid)
                    .filter(|state| !state.starts_with('Z'))
                    .map(|state| (*pid, state))
            })
            .collect::<Vec<_>>();
        if live.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{label} processes remained live after bounded grace: {live:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
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
fn exact_pending_file_comment_uses_file_subject_without_fake_line_and_saves_ack() {
    let (mut composition, source, draft_id) = composition_with_file_draft();
    let intent = composition
        .prepare_pending_file_comment(&draft_id, &source)
        .unwrap();
    let variables = json!({
        "pullRequestReviewId": "REVIEW_pending",
        "body": "exact whole-file comment",
        "path": "src/lib.rs",
        "subjectType": "FILE",
        "clientMutationId": intent.operation_id,
    });
    let (dir, provider) = fixture(
        "alice",
        vec![
            step(
                "query PendingFileCommentPreflight",
                json!({"owner":"owner","name":"repo","number":7}),
                pending_file_preflight(HEAD, "alice", "REVIEW_pending"),
            ),
            step(
                "mutation AddPendingFileReviewThread",
                variables,
                pending_file_ack(
                    &intent.operation_id,
                    "FILE",
                    "exact whole-file comment",
                    "REVIEW_pending",
                ),
            ),
        ],
        Duration::from_secs(30),
    );
    let store_dir = tempfile::tempdir().unwrap();
    let store = DraftStore::open(store_dir.path()).unwrap();
    let outcome = provider.execute_review_operation(
        &repo("alice"),
        &mut composition,
        &store,
        &intent.operation_id,
        "file-attempt-1",
    );
    let ProviderMutationOutcome::Acknowledged(ack) = outcome else {
        panic!("exact file acknowledgement was not accepted")
    };
    assert_eq!(ack.review_id.as_deref(), Some("REVIEW_pending"));
    assert_eq!(ack.thread_id.as_deref(), Some("THREAD_new"));
    assert_eq!(ack.comment_id.as_deref(), Some("COMMENT_new"));
    assert_eq!(count(&dir), 2);
    let LoadOutcome::Loaded(restored) = store.load(&key("alice")).unwrap() else {
        panic!("file operation was not durable")
    };
    let draft = restored.file_draft(&draft_id).unwrap();
    assert!(!draft.dirty);
    assert_eq!(
        draft.remote.as_ref().map(|remote| remote.comment_id.as_str()),
        Some("COMMENT_new")
    );
    assert!(matches!(
        restored.operations[0].status,
        ReviewOperationStatus::Acknowledged { .. }
    ));
}

#[test]
fn pending_file_comment_rejects_changed_head_foreign_or_missing_pending_before_write() {
    for response in [
        pending_file_preflight(NEW, "alice", "REVIEW_pending"),
        pending_file_preflight(HEAD, "mallory", "REVIEW_pending"),
        json!({"data": {
            "viewer": {"login": "alice"},
            "repository": {"nameWithOwner": "owner/repo", "pullRequest": {
                "id": "PR_node", "number": 7,
                "url": "https://github.com/owner/repo/pull/7", "state": "OPEN",
                "baseRefOid": OLD, "headRefOid": HEAD,
                "reviews": {"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}}
            }}
        }}),
    ] {
        let (mut composition, source, draft_id) = composition_with_file_draft();
        let intent = composition
            .prepare_pending_file_comment(&draft_id, &source)
            .unwrap();
        let (dir, provider) = fixture(
            "alice",
            vec![step(
                "query PendingFileCommentPreflight",
                json!({"owner":"owner","name":"repo","number":7}),
                response,
            )],
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
                "file-preflight-reject"
            ),
            ProviderMutationOutcome::PreflightRejected { .. }
        ));
        assert_eq!(count(&dir), 1, "a mutation must not follow failed preflight");
        assert_eq!(composition.file_draft(&draft_id).unwrap().body, "exact whole-file comment");
    }
}

#[test]
fn pending_file_ack_mismatch_is_uncertain_and_cannot_replay() {
    for acknowledgement in [
        pending_file_ack("wrong-operation", "FILE", "exact whole-file comment", "REVIEW_pending"),
        pending_file_ack("operation-1", "LINE", "exact whole-file comment", "REVIEW_pending"),
        pending_file_ack("operation-1", "FILE", "different body", "REVIEW_pending"),
        pending_file_ack("operation-1", "FILE", "exact whole-file comment", "REVIEW_other"),
    ] {
        let (mut composition, source, draft_id) = composition_with_file_draft();
        let intent = composition
            .prepare_pending_file_comment(&draft_id, &source)
            .unwrap();
        let (dir, provider) = fixture(
            "alice",
            vec![
                step(
                    "query PendingFileCommentPreflight",
                    json!({"owner":"owner","name":"repo","number":7}),
                    pending_file_preflight(HEAD, "alice", "REVIEW_pending"),
                ),
                step(
                    "mutation AddPendingFileReviewThread",
                    json!({
                        "pullRequestReviewId":"REVIEW_pending",
                        "body":"exact whole-file comment",
                        "path":"src/lib.rs",
                        "subjectType":"FILE",
                        "clientMutationId":intent.operation_id,
                    }),
                    acknowledgement,
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
                "file-uncertain"
            ),
            ProviderMutationOutcome::Uncertain { .. }
        ));
        assert_eq!(count(&dir), 2);
        assert!(matches!(
            provider.execute_review_operation(
                &repo("alice"),
                &mut composition,
                &store,
                &intent.operation_id,
                "file-replay"
            ),
            ProviderMutationOutcome::PreflightRejected { .. }
        ));
        assert_eq!(count(&dir), 2, "uncertain file writes must never replay");
        assert!(composition.file_draft(&draft_id).unwrap().remote.is_none());
    }
}

#[test]
fn pending_review_start_uses_two_ordered_exact_id_writes() {
    let intent = pending_start_intent();
    let create_variables = json!({
        "pullRequestId": "PR_node",
        "commitOID": HEAD,
        "event": null,
        "body": null,
        "threads": null,
        "clientMutationId": "pending-start-create-1",
    });
    let thread_variables = json!({
        "pullRequestReviewId": "REVIEW_created",
        "body": "exact whole-file comment",
        "path": "src/lib.rs",
        "subjectType": "FILE",
        "clientMutationId": "pending-start-thread-1",
    });
    let (transport_fixture, provider) = fixture(
        "alice",
        vec![
            step(
                "query PendingReview",
                json!({"owner":"owner","name":"repo","number":7}),
                pending_review_read(vec![]),
            ),
            step(
                "mutation CreateEmptyPendingReview",
                create_variables,
                pending_review_create_ack(
                    "pending-start-create-1",
                    "REVIEW_created",
                    "alice",
                ),
            ),
            step(
                "query PendingFileCommentPreflight",
                json!({"owner":"owner","name":"repo","number":7}),
                pending_file_preflight(HEAD, "alice", "REVIEW_created"),
            ),
            step(
                "mutation AddPendingFileReviewThread",
                thread_variables,
                pending_file_ack(
                    "pending-start-thread-1",
                    "FILE",
                    "exact whole-file comment",
                    "REVIEW_created",
                ),
            ),
        ],
        Duration::from_secs(2),
    );
    let prepared = provider
        .prepare_pending_review_start_create(&repo("alice"), &intent)
        .unwrap();
    let creation = match provider.dispatch_pending_review_start_create(
        prepared,
        "create-attempt-1",
    ) {
        ProviderMutationOutcome::Acknowledged(ack) => ack,
        other => panic!("unexpected create outcome: {other:?}"),
    };
    assert_eq!(creation.review.remote_id, "REVIEW_created");
    let prepared = provider
        .prepare_pending_review_start_thread(&repo("alice"), &intent, &creation)
        .unwrap();
    let thread = match provider.dispatch_pending_review_start_thread(
        prepared,
        "thread-attempt-1",
    ) {
        ProviderMutationOutcome::Acknowledged(ack) => ack,
        other => panic!("unexpected FILE outcome: {other:?}"),
    };
    assert_eq!(thread.review_id.as_deref(), Some("REVIEW_created"));
    assert_eq!(thread.thread_id.as_deref(), Some("THREAD_new"));
    assert_eq!(thread.comment_id.as_deref(), Some("COMMENT_new"));
    assert_eq!(
        fs::read_to_string(transport_fixture.path().join("mutations")).unwrap(),
        "2"
    );
}

#[test]
fn pending_review_absence_requires_every_pending_row_to_be_classifiable() {
    for (name, nodes, absence_expected) in [
        ("empty", vec![], true),
        (
            "known-other-author",
            vec![pending_review_row(
                "REVIEW_other",
                json!({"login":"bob"}),
            )],
            true,
        ),
        (
            "null-author",
            vec![pending_review_row("REVIEW_unknown", Value::Null)],
            false,
        ),
    ] {
        let (fixture, provider) = fixture(
            "alice",
            vec![step(
                "query PendingReview",
                json!({"owner":"owner","name":"repo","number":7}),
                pending_review_read(nodes),
            )],
            Duration::from_secs(2),
        );
        let observation = provider
            .pending_review_observation(&repo("alice"), 7)
            .unwrap();
        assert_eq!(
            observation.absence.is_some(),
            absence_expected,
            "{name}"
        );
        assert!(!fixture.path().join("mutations").exists(), "{name}");
    }

    let intent = pending_start_intent();
    let (null_author_fixture, provider) = fixture(
        "alice",
        vec![step(
            "query PendingReview",
            json!({"owner":"owner","name":"repo","number":7}),
            pending_review_read(vec![pending_review_row(
                "REVIEW_unknown",
                Value::Null,
            )]),
        )],
        Duration::from_secs(2),
    );
    assert!(
        provider
            .prepare_pending_review_start_create(&repo("alice"), &intent)
            .is_err()
    );
    assert!(!null_author_fixture.path().join("mutations").exists());

    for (name, replacement) in [("missing-pr-id", Value::Null), ("empty-pr-id", json!(""))] {
        let mut response = pending_review_read(vec![]);
        response["data"]["repository"]["pullRequest"]["id"] = replacement;
        let (fixture, provider) = fixture(
            "alice",
            vec![step(
                "query PendingReview",
                json!({"owner":"owner","name":"repo","number":7}),
                response,
            )],
            Duration::from_secs(2),
        );
        let observation = provider
            .pending_review_observation(&repo("alice"), 7)
            .unwrap();
        assert!(observation.absence.is_none(), "{name}");
        assert!(!fixture.path().join("mutations").exists(), "{name}");
    }
}

#[test]
fn pending_review_start_rejects_frozen_identity_and_body_before_transport() {
    let (fixture, provider) = fixture("alice", vec![], Duration::from_secs(2));
    let mut wrong_account = pending_start_intent();
    wrong_account.selected_author = "mallory".into();
    assert!(
        provider
            .prepare_pending_review_start_create(&repo("alice"), &wrong_account)
            .is_err()
    );
    let mut empty_body = pending_start_intent();
    empty_body.body.clear();
    assert!(
        provider
            .prepare_pending_review_start_create(&repo("alice"), &empty_body)
            .is_err()
    );
    assert!(!fixture.path().join("count").exists());
    assert!(!fixture.path().join("mutations").exists());
}

#[test]
fn moved_head_between_stages_leaves_exact_created_review_and_sends_one_write() {
    let intent = pending_start_intent();
    let (fixture, provider) = fixture(
        "alice",
        vec![
            step(
                "query PendingReview",
                json!({"owner":"owner","name":"repo","number":7}),
                pending_review_read(vec![]),
            ),
            step(
                "mutation CreateEmptyPendingReview",
                json!({
                    "pullRequestId":"PR_node", "commitOID":HEAD,
                    "event":null, "body":null, "threads":null,
                    "clientMutationId":"pending-start-create-1"
                }),
                pending_review_create_ack(
                    "pending-start-create-1",
                    "REVIEW_created",
                    "alice",
                ),
            ),
            step(
                "query PendingFileCommentPreflight",
                json!({"owner":"owner","name":"repo","number":7}),
                pending_file_preflight(NEW, "alice", "REVIEW_created"),
            ),
        ],
        Duration::from_secs(2),
    );
    let prepared = provider
        .prepare_pending_review_start_create(&repo("alice"), &intent)
        .unwrap();
    let creation = match provider.dispatch_pending_review_start_create(
        prepared,
        "create-attempt",
    ) {
        ProviderMutationOutcome::Acknowledged(ack) => ack,
        other => panic!("unexpected create outcome: {other:?}"),
    };
    assert!(
        provider
            .prepare_pending_review_start_thread(&repo("alice"), &intent, &creation)
            .is_err()
    );
    assert_eq!(creation.review.remote_id, "REVIEW_created");
    assert_eq!(fs::read_to_string(fixture.path().join("mutations")).unwrap(), "1");
    assert_eq!(fs::read_to_string(fixture.path().join("count")).unwrap(), "3");
}

#[test]
fn pending_review_create_rich_ack_mismatches_are_uncertain_and_never_reach_file_stage() {
    let mut cases = Vec::new();
    let mut wrong_operation = pending_review_create_ack(
        "pending-start-create-1",
        "REVIEW_created",
        "alice",
    );
    wrong_operation["data"]["createEmptyPendingReview"]["clientMutationId"] =
        json!("another-operation");
    cases.push(("operation", wrong_operation));
    let mut wrong_author = pending_review_create_ack(
        "pending-start-create-1",
        "REVIEW_created",
        "alice",
    );
    wrong_author["data"]["createEmptyPendingReview"]["pullRequestReview"]["author"]["login"] =
        json!("mallory");
    cases.push(("author", wrong_author));
    let mut wrong_state = pending_review_create_ack(
        "pending-start-create-1",
        "REVIEW_created",
        "alice",
    );
    wrong_state["data"]["createEmptyPendingReview"]["pullRequestReview"]["state"] =
        json!("COMMENTED");
    cases.push(("state", wrong_state));
    let mut submitted = pending_review_create_ack(
        "pending-start-create-1",
        "REVIEW_created",
        "alice",
    );
    submitted["data"]["createEmptyPendingReview"]["pullRequestReview"]["submittedAt"] =
        json!("2026-09-14T12:00:00Z");
    cases.push(("submitted", submitted));
    let mut wrong_commit = pending_review_create_ack(
        "pending-start-create-1",
        "REVIEW_created",
        "alice",
    );
    wrong_commit["data"]["createEmptyPendingReview"]["pullRequestReview"]["commit"]["oid"] =
        json!(NEW);
    cases.push(("commit", wrong_commit));
    let mut wrong_pr = pending_review_create_ack(
        "pending-start-create-1",
        "REVIEW_created",
        "alice",
    );
    wrong_pr["data"]["createEmptyPendingReview"]["pullRequestReview"]["pullRequest"]["id"] =
        json!("PR_other");
    cases.push(("pull-request", wrong_pr));
    let mut wrong_repo = pending_review_create_ack(
        "pending-start-create-1",
        "REVIEW_created",
        "alice",
    );
    wrong_repo["data"]["createEmptyPendingReview"]["pullRequestReview"]["pullRequest"]
        ["repository"]["nameWithOwner"] = json!("other/repo");
    cases.push(("repository", wrong_repo));
    let mut empty_review_id = pending_review_create_ack(
        "pending-start-create-1",
        "REVIEW_created",
        "alice",
    );
    empty_review_id["data"]["createEmptyPendingReview"]["pullRequestReview"]["id"] = json!("");
    cases.push(("review-id", empty_review_id));
    let mut review_id_is_pull_id = pending_review_create_ack(
        "pending-start-create-1",
        "REVIEW_created",
        "alice",
    );
    review_id_is_pull_id["data"]["createEmptyPendingReview"]["pullRequestReview"]["id"] =
        json!("PR_node");
    cases.push(("review-id-is-pull-id", review_id_is_pull_id));
    let mut missing_author = pending_review_create_ack(
        "pending-start-create-1",
        "REVIEW_created",
        "alice",
    );
    missing_author["data"]["createEmptyPendingReview"]["pullRequestReview"]["author"] =
        Value::Null;
    cases.push(("missing-author", missing_author));
    let mut missing_commit = pending_review_create_ack(
        "pending-start-create-1",
        "REVIEW_created",
        "alice",
    );
    missing_commit["data"]["createEmptyPendingReview"]["pullRequestReview"]["commit"] =
        Value::Null;
    cases.push(("missing-commit", missing_commit));
    let mut wrong_number = pending_review_create_ack(
        "pending-start-create-1",
        "REVIEW_created",
        "alice",
    );
    wrong_number["data"]["createEmptyPendingReview"]["pullRequestReview"]["pullRequest"]
        ["number"] = json!(8);
    cases.push(("pull-request-number", wrong_number));

    for (name, response) in cases {
        let intent = pending_start_intent();
        let (fixture, provider) = fixture(
            "alice",
            vec![
                step(
                    "query PendingReview",
                    json!({"owner":"owner","name":"repo","number":7}),
                    pending_review_read(vec![]),
                ),
                step(
                    "mutation CreateEmptyPendingReview",
                    json!({
                        "pullRequestId":"PR_node", "commitOID":HEAD,
                        "event":null, "body":null, "threads":null,
                        "clientMutationId":"pending-start-create-1"
                    }),
                    response,
                ),
            ],
            Duration::from_secs(2),
        );
        let prepared = provider
            .prepare_pending_review_start_create(&repo("alice"), &intent)
            .unwrap();
        assert!(matches!(
            provider.dispatch_pending_review_start_create(prepared, "create-attempt"),
            ProviderMutationOutcome::Uncertain { .. }
        ), "{name}");
        assert_eq!(
            fs::read_to_string(fixture.path().join("mutations")).unwrap(),
            "1",
            "{name}"
        );
        assert_eq!(
            fs::read_to_string(fixture.path().join("count")).unwrap(),
            "2",
            "{name}: no stage-2 preflight or write"
        );
    }
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
            json!({"data":{"node":{"id":"COMMENT_existing","author":{"login":"alice"},"subjectType":"LINE","pullRequestReview":{"id":"REVIEW_pending","state":"PENDING","author":{"login":"alice"},"commit":{"oid":HEAD},"pullRequest":{"id":"PR_node","number":7,"repository":{"nameWithOwner":"owner/repo"}}}}}}),
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
fn reused_pending_acknowledgement_must_match_requested_review() {
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
            json!({"data":{"addPullRequestReviewThread":{"thread":{"comments":{"nodes":[{"id":"COMMENT_2","pullRequestReview":{"id":"REVIEW_OTHER"}}]}}}}}),
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
            "attempt-wrong-parent"
        ),
        ProviderMutationOutcome::Uncertain { .. }
    ));
    assert!(matches!(
        composition.operations[0].status,
        ReviewOperationStatus::Uncertain { .. }
    ));
    assert!(composition.acknowledged_pending_review_id.is_none());
    assert_eq!(
        composition.observed_pending_review_id.as_deref(),
        Some("REVIEW_1")
    );
    assert_eq!(count(&dir), 3);
}

#[test]
fn updated_comment_acknowledgement_binds_comment_and_review_ids() {
    for (ack_comment, ack_review) in [
        ("COMMENT_OTHER", "REVIEW_pending"),
        ("COMMENT_existing", "REVIEW_OTHER"),
    ] {
        let (mut composition, comparison, draft_id) = composition_with_draft();
        let mut intent = composition
            .prepare_pending_comment(
                &draft_id,
                CanonicalPublishedPatch::new(&comparison, &ComparisonMetadata::default()).unwrap(),
            )
            .unwrap();
        intent.pending_review_id = Some("REVIEW_pending".into());
        intent.existing_comment_id = Some("COMMENT_existing".into());
        composition.operations[0].payload =
            Some(ReviewOperationPayload::PendingComment(intent.clone()));
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
                json!({"data":{"node":{"id":"COMMENT_existing","author":{"login":"alice"},"subjectType":"LINE","pullRequestReview":{"id":"REVIEW_pending","state":"PENDING","author":{"login":"alice"},"commit":{"oid":HEAD},"pullRequest":{"id":"PR_node","number":7,"repository":{"nameWithOwner":"owner/repo"}}}}}}),
            ),
            step(
                "mutation UpdateReviewComment",
                json!({"commentId":"COMMENT_existing","body":"frozen comment","clientMutationId":intent.operation_id}),
                json!({"data":{"updatePullRequestReviewComment":{"pullRequestReviewComment":{"id":ack_comment,"pullRequestReview":{"id":ack_review}}}}}),
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
                "attempt-mismatched-edit"
            ),
            ProviderMutationOutcome::Uncertain { .. }
        ));
        assert!(composition.acknowledged_pending_review_id.is_none());
        assert_eq!(count(&dir), 4);
    }
}

#[test]
fn submitted_review_acknowledgement_binds_existing_review_id() {
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
        .prepare_submission(ReviewEvent::Approve, "ship", None)
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
            "mutation SubmitReview",
            json!({"reviewId":"REVIEW_1","event":"APPROVE","body":"ship","clientMutationId":intent.operation_id}),
            json!({"data":{"submitPullRequestReview":{"pullRequestReview":{"id":"REVIEW_OTHER"}}}}),
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
            "attempt-wrong-submit"
        ),
        ProviderMutationOutcome::Uncertain { .. }
    ));
    assert_eq!(
        composition.observed_pending_review_id.as_deref(),
        Some("REVIEW_1")
    );
    assert_eq!(count(&dir), 3);
}

#[test]
fn new_review_acknowledgement_requires_valid_consistent_nodes() {
    let responses = [
        json!({"data":{"addPullRequestReview":{"pullRequestReview":null}}}),
        json!({"data":{"addPullRequestReview":{"pullRequestReview":{"id":"","comments":{"nodes":[{"id":"COMMENT_1","pullRequestReview":{"id":""}}]}}}}}),
        json!({"data":{"addPullRequestReview":{"pullRequestReview":{"id":"REVIEW_1","comments":{"nodes":[{"id":"","pullRequestReview":{"id":"REVIEW_1"}}]}}}}}),
        json!({"data":{"addPullRequestReview":{"pullRequestReview":{"id":"REVIEW_1","comments":{"nodes":[{"id":"COMMENT_1","pullRequestReview":{"id":"REVIEW_OTHER"}}]}}}}}),
    ];
    for response in responses {
        let (mut composition, comparison, draft_id) = composition_with_draft();
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
                "mutation AddReview",
                json!({"pullRequestId":"PR_node","commitOID":HEAD,"event":Value::Null,"body":Value::Null,"threads":[{"body":"frozen comment","path":"src/lib.rs","line":10,"side":"RIGHT"}],"clientMutationId":intent.operation_id}),
                response,
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
                "attempt-invalid-new-review"
            ),
            ProviderMutationOutcome::Uncertain { .. }
        ));
        assert!(composition.acknowledged_pending_review_id.is_none());
        assert_eq!(count(&dir), 2);
    }
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
fn acknowledgement_save_failure_restores_in_flight_memory_and_refuses_replay() {
    let (mut composition, comparison, draft_id) = composition_with_draft();
    let intent = composition
        .prepare_pending_comment(
            &draft_id,
            CanonicalPublishedPatch::new(&comparison, &ComparisonMetadata::default()).unwrap(),
        )
        .unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let store = DraftStore::open(store_dir.path()).unwrap();
    let record = store.record_path(&key("alice")).unwrap();
    let mut mutation = step(
        "mutation AddReview",
        json!({
            "pullRequestId":"PR_node","commitOID":HEAD,"event":Value::Null,"body":Value::Null,
            "threads":[{"body":"frozen comment","path":"src/lib.rs","line":10,"side":"RIGHT"}],
            "clientMutationId":intent.operation_id,
        }),
        json!({"data":{"addPullRequestReview":{"pullRequestReview":{"id":"REVIEW_1","comments":{"nodes":[{"id":"COMMENT_1","pullRequestReview":{"id":"REVIEW_1"}}]}}}}}),
    );
    mutation["immutable_path"] = json!(record.clone());
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
        Duration::from_secs(30),
    );
    let result = provider.execute_review_operation(
        &repo("alice"),
        &mut composition,
        &store,
        &intent.operation_id,
        "attempt-ack-save-fails",
    );
    assert!(
        Command::new("/usr/bin/chflags")
            .args(["nouchg", record.to_str().unwrap()])
            .status()
            .unwrap()
            .success()
    );
    assert!(matches!(result, ProviderMutationOutcome::Uncertain { .. }));
    assert!(matches!(
        composition.operations[0].status,
        ReviewOperationStatus::InFlight { .. }
    ));
    assert_eq!(composition.drafts[0].body, "frozen comment");
    assert!(composition.acknowledged_pending_review_id.is_none());
    assert!(
        composition
            .prepare_submission(ReviewEvent::Comment, "new operation", None)
            .is_err()
    );
    let LoadOutcome::Loaded(mut restored) = store.load(&key("alice")).unwrap() else {
        panic!()
    };
    assert!(matches!(
        restored.operations[0].status,
        ReviewOperationStatus::InFlight { .. }
    ));
    assert!(matches!(
        provider.execute_review_operation(
            &repo("alice"),
            &mut restored,
            &store,
            &intent.operation_id,
            "attempt-must-not-replay"
        ),
        ProviderMutationOutcome::PreflightRejected { .. }
    ));
    assert_eq!(count(&dir), 2);
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
#[ignore = "run in isolation; an outer subprocess enforces the 3-second regression deadline"]
fn no_read_input_and_pipe_descendants_are_bounded_by_an_outer_deadline() {
    const CHILD_MARKER: &str = "CIBERGIT_PROVIDER_NO_READ_CHILD";
    const NO_READ_GROUP_PATH: &str = "CIBERGIT_PROVIDER_NO_READ_GROUP_PATH";
    const NO_READ_HELPER_PATH: &str = "CIBERGIT_PROVIDER_NO_READ_HELPER_PATH";
    const NO_READ_STARTED_PATH: &str = "CIBERGIT_PROVIDER_NO_READ_STARTED_PATH";
    const EARLY_EXIT_GROUP_PATH: &str = "CIBERGIT_PROVIDER_EARLY_EXIT_GROUP_PATH";
    const EARLY_EXIT_HELPER_PATH: &str = "CIBERGIT_PROVIDER_EARLY_EXIT_HELPER_PATH";
    const EARLY_EXIT_STARTED_PATH: &str = "CIBERGIT_PROVIDER_EARLY_EXIT_STARTED_PATH";
    if std::env::var_os(CHILD_MARKER).is_some() {
        let no_read_group_path = std::env::var_os(NO_READ_GROUP_PATH).unwrap();
        let no_read_helper_path = std::env::var_os(NO_READ_HELPER_PATH).unwrap();
        let no_read_started_path = std::env::var_os(NO_READ_STARTED_PATH).unwrap();
        let early_exit_group_path = std::env::var_os(EARLY_EXIT_GROUP_PATH).unwrap();
        let early_exit_helper_path = std::env::var_os(EARLY_EXIT_HELPER_PATH).unwrap();
        let early_exit_started_path = std::env::var_os(EARLY_EXIT_STARTED_PATH).unwrap();
        let (mut composition, comparison, draft_id) = composition_with_draft();
        composition
            .edit_draft(&draft_id, "x".repeat(64 * 1024))
            .unwrap();
        let intent = composition
            .prepare_pending_comment(
                &draft_id,
                CanonicalPublishedPatch::new(&comparison, &ComparisonMetadata::default()).unwrap(),
            )
            .unwrap();
        let (dir, provider) = no_read_fixture(Duration::from_millis(200));
        let store_dir = tempfile::tempdir().unwrap();
        let store = DraftStore::open(store_dir.path()).unwrap();
        let started = Instant::now();
        match provider.execute_review_operation(
            &repo("alice"),
            &mut composition,
            &store,
            &intent.operation_id,
            "attempt-no-read",
        ) {
            ProviderMutationOutcome::Uncertain { .. } => {}
            ProviderMutationOutcome::PreflightRejected { reason } => {
                panic!("large bounded mutation was rejected before dispatch: {reason}")
            }
            ProviderMutationOutcome::Acknowledged(_) => {
                panic!("blocked mutation unexpectedly returned an acknowledgement")
            }
        }
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(
            Path::new(&no_read_started_path).is_file(),
            "no-read shell/helper did not report startup"
        );
        let no_read_group = recorded_pid(Path::new(&no_read_group_path));
        let no_read_helper = recorded_pid(Path::new(&no_read_helper_path));
        assert_ne!(no_read_group, no_read_helper);
        assert_processes_not_live_after_grace(
            "no-read shell/helper",
            &[no_read_group, no_read_helper],
        );
        let LoadOutcome::Loaded(mut restored) = store.load(&key("alice")).unwrap() else {
            panic!()
        };
        assert!(matches!(
            restored.operations[0].status,
            ReviewOperationStatus::Uncertain { .. }
        ));
        let dispatch_count = count(&dir);
        assert!((1..=2).contains(&dispatch_count));
        assert!(matches!(
            provider.execute_review_operation(
                &repo("alice"),
                &mut restored,
                &store,
                &intent.operation_id,
                "attempt-no-read-replay"
            ),
            ProviderMutationOutcome::PreflightRejected { .. }
        ));
        assert_eq!(count(&dir), dispatch_count);

        let runner = Runner {
            timeout: Duration::from_secs(2),
            input_timeout: Some(Duration::from_millis(200)),
            ..Runner::default()
        };
        let mut command = Command::new("/bin/sh");
        command
            .env("EARLY_EXIT_GROUP_PATH", &early_exit_group_path)
            .env("EARLY_EXIT_HELPER_PATH", &early_exit_helper_path)
            .env("EARLY_EXIT_STARTED_PATH", &early_exit_started_path)
            .args([
                "-c",
                r#"printf '%s\n' $$ >"$EARLY_EXIT_GROUP_PATH"
/bin/sleep 30 <&0 >&1 2>&2 &
helper=$!
printf '%s\n' "$helper" >"$EARLY_EXIT_HELPER_PATH"
kill -0 "$helper" || exit 22
: >"$EARLY_EXIT_STARTED_PATH"
exit 7"#,
            ]);
        let started = Instant::now();
        let sensitive_input = "do-not-expose-input".repeat(16 * 1024);
        let error = runner
            .run_with_input(
                command,
                "test early input failure",
                sensitive_input.as_bytes(),
            )
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!error.contains("do-not-expose-input"));
        assert!(
            Path::new(&early_exit_started_path).is_file(),
            "early-exit shell/helper did not report startup"
        );
        let early_exit_group = recorded_pid(Path::new(&early_exit_group_path));
        let early_exit_helper = recorded_pid(Path::new(&early_exit_helper_path));
        assert_ne!(early_exit_group, early_exit_helper);
        assert_processes_not_live_after_grace(
            "early-exit shell/helper",
            &[early_exit_group, early_exit_helper],
        );
        return;
    }

    let outer = tempfile::tempdir().unwrap();
    let no_read_group_path = outer.path().join("no-read-group");
    let no_read_helper_path = outer.path().join("no-read-helper");
    let no_read_started_path = outer.path().join("no-read-started");
    let early_exit_group_path = outer.path().join("early-exit-group");
    let early_exit_helper_path = outer.path().join("early-exit-helper");
    let early_exit_started_path = outer.path().join("early-exit-started");
    let group_paths = [&no_read_group_path, &early_exit_group_path];
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .arg("no_read_input_and_pipe_descendants_are_bounded_by_an_outer_deadline")
        .arg("--ignored")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_MARKER, "1")
        .env(NO_READ_GROUP_PATH, &no_read_group_path)
        .env(NO_READ_HELPER_PATH, &no_read_helper_path)
        .env(NO_READ_STARTED_PATH, &no_read_started_path)
        .env(EARLY_EXIT_GROUP_PATH, &early_exit_group_path)
        .env(EARLY_EXIT_HELPER_PATH, &early_exit_helper_path)
        .env(EARLY_EXIT_STARTED_PATH, &early_exit_started_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // The nested test emits only local assertion diagnostics. Subprocess
        // stderr from gh remains captured and withheld by Runner.
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = child.spawn().unwrap();
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            if !status.success() {
                for path in group_paths {
                    if path.is_file() {
                        terminate_process_group_id(recorded_pid(path));
                    }
                }
            }
            assert!(status.success(), "bounded transport subprocess failed");
            break;
        }
        if started.elapsed() >= Duration::from_secs(3) {
            for path in group_paths {
                if path.is_file() {
                    terminate_process_group_id(recorded_pid(path));
                }
            }
            terminate_process_group(&mut child);
            panic!("bounded transport subprocess exceeded its outer deadline");
        }
        thread::sleep(Duration::from_millis(10));
    }
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
            "comments":{"nodes":[{"id":"COMMENT_1","author":{"login":"alice"},"body":"draft","createdAt":"a","updatedAt":"b","url":"u","path":"src/lib.rs","line":10,"originalLine":null,"startLine":null,"originalStartLine":null,"diffHunk":"@@","outdated":false,"commit":{"oid":HEAD},"originalCommit":{"oid":HEAD},"pullRequestReview":{"id":"REVIEW_1"}}],"pageInfo":{"hasNextPage":true,"endCursor":"next"}}}],
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
fn complete_pending_import_exposes_fresh_file_source_and_explicit_subject() {
    let response: Value = serde_json::from_str(&format!(
        r#"{{"data":{{"viewer":{{"login":"alice"}},"repository":{{"nameWithOwner":"owner/repo","pullRequest":{{"id":"PR_node","number":7,"url":"https://github.com/owner/repo/pull/7","state":"OPEN","baseRefOid":"{OLD}","headRefOid":"{HEAD}","reviews":{{"nodes":[{{"id":"REVIEW_1","author":{{"login":"alice"}},"body":"draft summary","state":"PENDING","submittedAt":null,"commit":{{"oid":"{HEAD}"}},"url":"u","comments":{{"nodes":[{{"id":"COMMENT_1","author":{{"login":"alice"}},"body":"whole file","createdAt":"a","updatedAt":"b","url":"u","path":"src/lib.rs","subjectType":"FILE","line":null,"originalLine":null,"startLine":null,"originalStartLine":null,"diffHunk":"","outdated":false,"commit":{{"oid":"{HEAD}"}},"originalCommit":{{"oid":"{HEAD}"}},"pullRequestReview":{{"id":"REVIEW_1"}}}}],"pageInfo":{{"hasNextPage":false,"endCursor":null}}}}}}],"pageInfo":{{"hasNextPage":false,"endCursor":null}}}}}}}}}}}}"#
    ))
    .unwrap();
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
    assert_eq!(pending.comments[0].comment.subject, ReviewSubject::File);
    let source = pending.file_comment_source.expect("fresh complete source");
    assert_eq!(source.pull_request.remote_id, "PR_node");
    assert_eq!(source.review.remote_id, "REVIEW_1");
    assert_eq!(source.current_base_sha, OLD);
    assert_eq!(source.current_head_sha, HEAD);
    assert_eq!(count(&dir), 1);
}

#[test]
fn pending_import_excludes_missing_or_mismatched_comment_parents() {
    let comment = |id: &str, parent: Value| {
        json!({
            "id":id,"author":{"login":"alice"},"body":"draft","createdAt":"a",
            "updatedAt":"b","url":"u","path":"src/lib.rs","line":10,
            "originalLine":null,"startLine":null,"originalStartLine":null,
            "diffHunk":"@@","outdated":false,"commit":{"oid":HEAD},
            "originalCommit":{"oid":HEAD},"pullRequestReview":parent
        })
    };
    let response = json!({"data":{"repository":{"nameWithOwner":"owner/repo","pullRequest":{"number":7,"reviews":{
        "nodes":[{"id":"REVIEW_1","author":{"login":"alice"},"body":"draft summary","state":"PENDING","submittedAt":null,"commit":{"oid":HEAD},"url":"u",
            "comments":{"nodes":[
                comment("COMMENT_OK", json!({"id":"REVIEW_1"})),
                comment("COMMENT_OTHER", json!({"id":"REVIEW_OTHER"})),
                comment("COMMENT_MISSING", Value::Null)
            ],"pageInfo":{"hasNextPage":false,"endCursor":null}}}],
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
    assert_eq!(pending.comments.len(), 1);
    assert_eq!(
        pending.comments[0].comment.coordinates.remote_id,
        "COMMENT_OK"
    );
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
fn submitted_summary_edit_sends_one_exact_mutation_for_older_review_commit() {
    let request = submitted_edit_request();
    let steps = vec![
        step(
            "query ReviewActionContext",
            json!({"owner":"owner","name":"repo","number":7}),
            context(HEAD, "CLOSED"),
        ),
        step(
            "query ReviewIdentity",
            json!({"id":"REVIEW_submitted"}),
            submitted_review_node("REVIEW_submitted", "alice", OLD, "APPROVED", "before"),
        ),
        step(
            "mutation UpdateSubmittedReviewSummary",
            json!({"reviewId":"REVIEW_submitted","body":"","clientMutationId":"submitted-edit-1"}),
            submitted_edit_ack(
                "submitted-edit-1",
                "REVIEW_submitted",
                "alice",
                OLD,
                "APPROVED",
                "",
            ),
        ),
    ];
    let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
    let ProviderMutationOutcome::Acknowledged(ack) =
        provider.execute_review_auxiliary(&repo("alice"), 7, &request)
    else {
        panic!("exact submitted edit must be acknowledged")
    };
    assert_eq!(ack.operation_id, "submitted-edit-1");
    assert_eq!(ack.review_id.as_deref(), Some("REVIEW_submitted"));
    assert_eq!(count(&dir), 3);
}

#[test]
fn unknown_pending_comment_subject_is_rejected_before_delete_transport() {
    for subject in [Value::Null, json!("FUTURE_SUBJECT")] {
        let request = ReviewAuxiliaryRequest {
            operation_id: "delete-unknown-subject".into(),
            attempt_id: "attempt-delete-unknown".into(),
            action: ReviewAuxiliaryAction::DeletePendingComment {
                review: coordinates("REVIEW_pending"),
                comment: coordinates("COMMENT_unknown"),
            },
        };
        let comment = json!({
            "data": {"node": {
                "id": "COMMENT_unknown",
                "author": {"login": "alice"},
                "subjectType": subject,
                "pullRequestReview": {
                    "id": "REVIEW_pending",
                    "state": "PENDING",
                    "author": {"login": "alice"},
                    "commit": {"oid": HEAD},
                    "pullRequest": {
                        "id": "PR_node",
                        "number": 7,
                        "repository": {"nameWithOwner": "owner/repo"}
                    }
                }
            }}
        });
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
                    json!({"id":"REVIEW_pending"}),
                    review_node("REVIEW_pending", "alice", HEAD, "PENDING"),
                ),
                step(
                    "query ReviewCommentIdentity",
                    json!({"id":"COMMENT_unknown"}),
                    comment,
                ),
            ],
            Duration::from_secs(30),
        );
        let outcome = provider.execute_review_auxiliary(&repo("alice"), 7, &request);
        let ProviderMutationOutcome::PreflightRejected { reason } = outcome else {
            panic!("unknown subject must be rejected before delete mutation")
        };
        assert!(reason.contains("subject"), "{reason}");
        assert_eq!(count(&dir), 3, "no mutation transport may be dispatched");
    }
}

#[test]
fn submitted_summary_edit_rejects_stale_or_unproven_targets_before_mutation() {
    let base = submitted_review_node(
        "REVIEW_submitted",
        "alice",
        OLD,
        "APPROVED",
        "before",
    );
    let mut cases = Vec::new();

    let mut foreign_author = base.clone();
    foreign_author["data"]["node"]["author"]["login"] = json!("bob");
    cases.push(("foreign author", context(HEAD, "OPEN"), foreign_author));

    let mut incapable = base.clone();
    incapable["data"]["node"]["viewerCanUpdate"] = json!(false);
    incapable["data"]["node"]["viewerCannotUpdateReasons"] = json!(["DENIED"]);
    cases.push(("capability false", context(HEAD, "MERGED"), incapable));

    let mut missing_capability = base.clone();
    missing_capability["data"]["node"]
        .as_object_mut()
        .unwrap()
        .remove("viewerCanUpdate");
    cases.push((
        "capability missing",
        context(HEAD, "OPEN"),
        missing_capability,
    ));

    let mut viewer_not_author = base.clone();
    viewer_not_author["data"]["node"]["viewerDidAuthor"] = json!(false);
    cases.push((
        "viewer did not author",
        context(HEAD, "OPEN"),
        viewer_not_author,
    ));

    let mut wrong_parent = base.clone();
    wrong_parent["data"]["node"]["pullRequest"]["id"] = json!("PR_other");
    cases.push(("wrong parent", context(HEAD, "OPEN"), wrong_parent));

    let mut pending = base.clone();
    pending["data"]["node"]["state"] = json!("PENDING");
    pending["data"]["node"]["submittedAt"] = Value::Null;
    cases.push(("pending state", context(HEAD, "OPEN"), pending));

    let mut dismissed = base.clone();
    dismissed["data"]["node"]["state"] = json!("DISMISSED");
    cases.push(("dismissed state", context(HEAD, "OPEN"), dismissed));

    let mut changed_body = base.clone();
    changed_body["data"]["node"]["body"] = json!("changed elsewhere");
    cases.push(("changed prior body", context(HEAD, "OPEN"), changed_body));

    let mut changed_commit = base.clone();
    changed_commit["data"]["node"]["commit"]["oid"] = json!(NEW);
    cases.push(("changed review commit", context(HEAD, "OPEN"), changed_commit));

    let mut missing_submission = base.clone();
    missing_submission["data"]["node"]["submittedAt"] = Value::Null;
    cases.push((
        "missing submitted time",
        context(HEAD, "OPEN"),
        missing_submission,
    ));

    let mut partial = base;
    partial["errors"] = json!([{"message":"capability unavailable"}]);
    cases.push(("partial review read", context(HEAD, "OPEN"), partial));

    for (name, action_context, review) in cases {
        let steps = vec![
            step(
                "query ReviewActionContext",
                json!({"owner":"owner","name":"repo","number":7}),
                action_context,
            ),
            step(
                "query ReviewIdentity",
                json!({"id":"REVIEW_submitted"}),
                review,
            ),
        ];
        let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
        let outcome = provider.execute_review_auxiliary(
            &repo("alice"),
            7,
            &submitted_edit_request(),
        );
        assert!(
            matches!(outcome, ProviderMutationOutcome::PreflightRejected { .. }),
            "{name}: {outcome:?}"
        );
        assert_eq!(count(&dir), 2, "{name}");
    }

    let mut viewer_mismatch = context(HEAD, "OPEN");
    viewer_mismatch["data"]["viewer"]["login"] = json!("bob");
    let (dir, provider) = fixture(
        "alice",
        vec![step(
            "query ReviewActionContext",
            json!({"owner":"owner","name":"repo","number":7}),
            viewer_mismatch,
        )],
        Duration::from_secs(30),
    );
    assert!(matches!(
        provider.execute_review_auxiliary(&repo("alice"), 7, &submitted_edit_request()),
        ProviderMutationOutcome::PreflightRejected { .. }
    ));
    assert_eq!(count(&dir), 1);

    let mut wrong_selected_author = submitted_edit_request();
    let ReviewAuxiliaryAction::UpdateSubmittedSummary {
        selected_author, ..
    } = &mut wrong_selected_author.action
    else {
        unreachable!()
    };
    *selected_author = "bob".into();
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
                json!({"id":"REVIEW_submitted"}),
                submitted_review_node(
                    "REVIEW_submitted",
                    "alice",
                    OLD,
                    "APPROVED",
                    "before",
                ),
            ),
        ],
        Duration::from_secs(30),
    );
    assert!(matches!(
        provider.execute_review_auxiliary(&repo("alice"), 7, &wrong_selected_author),
        ProviderMutationOutcome::PreflightRejected { .. }
    ));
    assert_eq!(count(&dir), 2);

    let mut wrong_coordinates = submitted_edit_request();
    let ReviewAuxiliaryAction::UpdateSubmittedSummary { review, .. } =
        &mut wrong_coordinates.action
    else {
        unreachable!()
    };
    review.repository = "other".into();
    let (dir, provider) = fixture(
        "alice",
        vec![step(
            "query ReviewActionContext",
            json!({"owner":"owner","name":"repo","number":7}),
            context(HEAD, "OPEN"),
        )],
        Duration::from_secs(30),
    );
    assert!(matches!(
        provider.execute_review_auxiliary(&repo("alice"), 7, &wrong_coordinates),
        ProviderMutationOutcome::PreflightRejected { .. }
    ));
    assert_eq!(count(&dir), 1);
}

#[test]
fn submitted_summary_edit_requires_exact_acknowledgement_tuple() {
    let exact = submitted_edit_ack(
        "submitted-edit-1",
        "REVIEW_submitted",
        "alice",
        OLD,
        "APPROVED",
        "",
    );
    let mut cases = Vec::new();
    let mut wrong_operation = exact.clone();
    wrong_operation["data"]["updateSubmittedPullRequestReview"]["clientMutationId"] =
        json!("other-operation");
    cases.push(("operation", wrong_operation));
    let mut wrong_id = exact.clone();
    wrong_id["data"]["updateSubmittedPullRequestReview"]["pullRequestReview"]["id"] =
        json!("REVIEW_other");
    cases.push(("review ID", wrong_id));
    let mut wrong_body = exact.clone();
    wrong_body["data"]["updateSubmittedPullRequestReview"]["pullRequestReview"]["body"] =
        json!("not empty");
    cases.push(("body", wrong_body));
    let mut wrong_state = exact.clone();
    wrong_state["data"]["updateSubmittedPullRequestReview"]["pullRequestReview"]["state"] =
        json!("COMMENTED");
    cases.push(("state", wrong_state));
    let mut wrong_author = exact.clone();
    wrong_author["data"]["updateSubmittedPullRequestReview"]["pullRequestReview"]["author"]
        ["login"] = json!("bob");
    cases.push(("author", wrong_author));
    let mut wrong_commit = exact.clone();
    wrong_commit["data"]["updateSubmittedPullRequestReview"]["pullRequestReview"]["commit"]
        ["oid"] = json!(NEW);
    cases.push(("commit", wrong_commit));
    let mut wrong_parent = exact.clone();
    wrong_parent["data"]["updateSubmittedPullRequestReview"]["pullRequestReview"]
        ["pullRequest"]["repository"]["nameWithOwner"] = json!("owner/other");
    cases.push(("parent", wrong_parent));
    let mut malformed = exact;
    malformed["data"]["updateSubmittedPullRequestReview"]["pullRequestReview"]
        .as_object_mut()
        .unwrap()
        .remove("body");
    cases.push(("malformed", malformed));

    for (name, acknowledgement) in cases {
        let steps = vec![
            step(
                "query ReviewActionContext",
                json!({"owner":"owner","name":"repo","number":7}),
                context(HEAD, "OPEN"),
            ),
            step(
                "query ReviewIdentity",
                json!({"id":"REVIEW_submitted"}),
                submitted_review_node(
                    "REVIEW_submitted",
                    "alice",
                    OLD,
                    "APPROVED",
                    "before",
                ),
            ),
            step(
                "mutation UpdateSubmittedReviewSummary",
                json!({"reviewId":"REVIEW_submitted","body":"","clientMutationId":"submitted-edit-1"}),
                acknowledgement,
            ),
        ];
        let (dir, provider) = fixture("alice", steps, Duration::from_secs(30));
        assert!(
            matches!(
                provider.execute_review_auxiliary(&repo("alice"), 7, &submitted_edit_request()),
                ProviderMutationOutcome::Uncertain { .. }
            ),
            "{name} acknowledgement must remain uncertain"
        );
        assert_eq!(count(&dir), 3, "{name}");
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
