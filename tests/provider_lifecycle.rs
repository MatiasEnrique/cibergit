#[macro_export]
macro_rules! provider_lifecycle_tests {
    () => {
        use super::*;
        use $crate::domain::{
            Account, MutationAdmissionReceipt, MutationContext, MutationTerminalRecord,
            ProviderCoordinates, ProviderMutationOutcome, PullRequestCreationInput,
            PullRequestCreationRequest, PullRequestDiscussionAction, PullRequestDiscussionRequest,
            PullRequestLifecycleAction, PullRequestLifecycleRequest, PullRequestMutationTarget,
            PullRequestReviewer, Repository,
        };
        use serde_json::{Value, json};
        use std::{
            cell::RefCell,
            collections::HashSet,
            fs,
            os::unix::fs::PermissionsExt,
            rc::Rc,
            sync::{Mutex, MutexGuard, OnceLock},
            time::Duration,
        };
        use tempfile::TempDir;

        const BASE: &str = "1111111111111111111111111111111111111111";
        const HEAD: &str = "2222222222222222222222222222222222222222";
        const NEW_HEAD: &str = "3333333333333333333333333333333333333333";

        fn lifecycle_test_lock() -> MutexGuard<'static, ()> {
            static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
            LOCK.get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn lifecycle_account(login: &str) -> Account {
            Account {
                host: "github.com".into(),
                login: login.into(),
            }
        }

        fn lifecycle_repo(login: &str) -> Repository {
            Repository {
                host: "github.com".into(),
                owner: "owner".into(),
                name: "repo".into(),
                account: lifecycle_account(login),
                local_path: None,
            }
        }

        fn source_repo(login: &str) -> Repository {
            Repository {
                host: "github.com".into(),
                owner: "forker".into(),
                name: "fork".into(),
                account: lifecycle_account(login),
                local_path: None,
            }
        }

        #[allow(clippy::too_many_arguments)]
        fn lifecycle_response(
            login: &str,
            state: &str,
            draft: bool,
            title: &str,
            body: &str,
            base: &str,
            reviewers: &[(&str, &str)],
            labels: &[&str],
            assignees: &[&str],
        ) -> Value {
            let requests: Vec<Value> = reviewers
                .iter()
                .map(|(kind, name)| {
                    if *kind == "USER" {
                        json!({"requestedReviewer":{"login":name}})
                    } else {
                        json!({"requestedReviewer":{"slug":name}})
                    }
                })
                .collect();
            json!({"data": {
                "viewer": {"login": login},
                "repository": {
                    "nameWithOwner": "owner/repo",
                    "viewerPermission": "WRITE",
                    "pullRequest": {
                        "id": "PR_node", "number": 7,
                        "url": "https://github.com/owner/repo/pull/7",
                        "updatedAt": "2026-09-13T12:00:00Z", "state": state,
                        "headRefOid": HEAD, "title": title, "body": body,
                        "baseRefName": base, "isDraft": draft, "locked": false,
                        "viewerCanUpdate": true, "viewerCanClose": state == "OPEN",
                        "viewerCanReopen": state == "CLOSED", "viewerCanLabel": true,
                        "viewerCanAssign": true,
                        "reviewRequests": {"nodes": requests, "pageInfo":{"hasNextPage":false,"endCursor":null}},
                        "labels": {"nodes": labels.iter().map(|name| json!({"name":name})).collect::<Vec<_>>(), "pageInfo":{"hasNextPage":false,"endCursor":null}},
                        "assignees": {"nodes": assignees.iter().map(|login| json!({"login":login})).collect::<Vec<_>>(), "pageInfo":{"hasNextPage":false,"endCursor":null}}
                    }
                }
            }})
        }

        fn lifecycle_step(response: Value) -> Value {
            json!({
                "transport":"graphql", "marker":"query PullRequestLifecycle(",
                "variables":{"owner":"owner","name":"repo","number":7},
                "response":response
            })
        }

        fn graphql_step(marker: &str, variables: Value, response: Value, write: bool) -> Value {
            json!({
                "transport":"graphql", "marker":marker, "variables":variables,
                "response":response, "write":write
            })
        }

        fn rest_step(
            method: &str,
            endpoint: &str,
            variables: Value,
            response: Value,
            write: bool,
        ) -> Value {
            json!({
                "transport":"rest", "method":method, "endpoint":endpoint,
                "variables":variables, "response":response, "write":write
            })
        }

        fn get_step(endpoint: &str, response: Value) -> Value {
            json!({"transport":"get","endpoint":endpoint,"response":response})
        }

        fn update_graphql_step(
            field: &str,
            variables: Value,
            fixture_before: Value,
            fixture_after: Value,
            response: Value,
        ) -> Value {
            json!({
                "transport":"graphql", "marker":"mutation UpdatePullRequestMetadata(",
                "variables":variables, "response":response, "write":true,
                "update_field":field, "fixture_before":fixture_before,
                "fixture_after":fixture_after,
            })
        }

        fn failing_write(marker: &str, variables: Value) -> Value {
            json!({
                "transport":"graphql", "marker":marker, "variables":variables,
                "write":true, "fail":true
            })
        }

        fn lifecycle_fixture(login: &str, steps: Vec<Value>) -> (TempDir, GithubProvider) {
            let dir = tempfile::tempdir().unwrap();
            fs::write(
                dir.path().join("steps.json"),
                serde_json::to_vec(&json!({"login":login,"steps":steps})).unwrap(),
            )
            .unwrap();
            let executable = dir.path().join("gh");
            fs::write(
                &executable,
                r#"#!/usr/bin/python3
import json, os, pathlib, re, sys
root = pathlib.Path(__file__).parent
config = json.loads((root / 'steps.json').read_text())
args = sys.argv[1:]
for key in ['GITHUB_TOKEN','GH_ENTERPRISE_TOKEN','GITHUB_ENTERPRISE_TOKEN','GH_HOST','GH_REPO','GH_DEBUG','DEBUG','GH_HTTP_UNIX_SOCKET']:
    assert key not in os.environ
assert os.environ.get('GH_PROMPT_DISABLED') == '1'
assert os.environ.get('GH_PAGER') == '/bin/cat'
assert os.environ.get('PAGER') == '/bin/cat'
token = 'private-' + config['login']
if args[:2] == ['auth','token']:
    assert args == ['auth','token','--hostname','github.com','--user',config['login']]
    assert 'GH_TOKEN' not in os.environ
    print(token)
    sys.exit(0)
assert os.environ.get('GH_TOKEN') == token
count_path = root / 'count'
index = int(count_path.read_text()) if count_path.exists() else 0
assert index < len(config['steps']), (index, args)
step = config['steps'][index]
count_path.write_text(str(index + 1))
transport = step['transport']
if transport == 'get':
    assert args == ['api','--hostname','github.com','--method','GET','--header','Accept: application/vnd.github+json','--header','X-GitHub-Api-Version: 2026-03-10',step['endpoint']]
elif transport == 'graphql':
    assert args == ['api','--hostname','github.com','--method','POST','--header','Accept: application/vnd.github+json','--header','X-GitHub-Api-Version: 2026-03-10','graphql','--input','-']
    payload = json.load(sys.stdin)
    query = ' '.join(payload['query'].split())
    assert step['marker'] in payload['query']
    assert payload['variables'] == step['variables']
    if 'mutation UpdatePullRequestMetadata(' in query:
        update_field = step['update_field']
        match = re.search(r'updatePullRequest\(input: \{([^}]*)\}\)', query)
        assert match is not None
        assignments = re.findall(r'(\w+): \$(\w+)', match.group(1))
        expected = [('pullRequestId', 'pullRequestId'), (update_field, update_field), ('clientMutationId', 'clientMutationId')]
        assert assignments == expected, assignments
        assert set(payload['variables']) == {name for name, _ in expected}
        for candidate in ['title', 'body', 'baseRefName']:
            assert (f'${candidate}:' in query) == (candidate == update_field)
        applied = dict(step['fixture_before'])
        applied[update_field] = payload['variables'][update_field]
        assert applied == step['fixture_after'], applied
    if 'mutation ConvertPullRequestToDraftLifecycle(' in query:
        assert 'convertPullRequestToDraft(input: { pullRequestId: $pullRequestId, clientMutationId: $clientMutationId })' in query
    if 'mutation MarkPullRequestReadyForReviewLifecycle(' in query:
        assert 'markPullRequestReadyForReview(input: { pullRequestId: $pullRequestId, clientMutationId: $clientMutationId })' in query
    if 'mutation AddTopLevelPullRequestComment(' in query:
        assert 'addComment(input: { subjectId: $subjectId, body: $body, clientMutationId: $clientMutationId })' in query
        assert 'addPullRequestReview' not in query
    if 'mutation UpdateTopLevelPullRequestComment(' in query:
        assert 'updateIssueComment(input: { id: $commentId, body: $body, clientMutationId: $clientMutationId })' in query
        assert 'updatePullRequestReviewComment' not in query
    if 'mutation DeleteTopLevelPullRequestComment(' in query:
        assert 'deleteIssueComment(input: { id: $commentId, clientMutationId: $clientMutationId })' in query
        assert 'deletePullRequestReviewComment' not in query
else:
    assert transport == 'rest'
    assert args == ['api','--hostname','github.com','--method',step['method'],'--header','Accept: application/vnd.github+json','--header','X-GitHub-Api-Version: 2026-03-10',step['endpoint'],'--input','-']
    payload = json.load(sys.stdin)
    assert payload == step['variables']
if step.get('write'):
    writes = root / 'writes'
    writes.write_text(str(int(writes.read_text()) + 1 if writes.exists() else 1))
if step.get('fail'):
    sys.exit(1)
if 'raw' in step:
    sys.stdout.write(step['raw'])
else:
    print(json.dumps(step['response']))
"#,
            )
            .unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let provider = GithubProvider {
                account: lifecycle_account(login),
                runner: Runner {
                    gh: executable,
                    // Ordinary fixture startup must tolerate the full parallel suite.
                    // Dedicated transport tests retain their short deadline assertions.
                    timeout: Duration::from_secs(30),
                    ..Runner::default()
                },
            };
            (dir, provider)
        }

        fn file_count(dir: &TempDir, name: &str) -> usize {
            fs::read_to_string(dir.path().join(name))
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0)
        }

        fn mutation_target(state: &str) -> PullRequestMutationTarget {
            PullRequestMutationTarget {
                repository: lifecycle_repo("alice"),
                pull_request: ProviderCoordinates {
                    provider: "github".into(),
                    host: "github.com".into(),
                    owner: "owner".into(),
                    repository: "repo".into(),
                    pull_request: 7,
                    remote_id: "PR_node".into(),
                },
                observed_updated_at: "2026-09-13T12:00:00Z".into(),
                observed_state: state.into(),
                observed_head_sha: HEAD.into(),
            }
        }

        #[derive(Default)]
        struct JournalState {
            inflight_targets: HashSet<String>,
            seen_attempts: HashSet<String>,
            contexts: Vec<MutationContext>,
            records: Vec<MutationTerminalRecord>,
        }

        struct FakeAdmission {
            state: Rc<RefCell<JournalState>>,
            reject: bool,
            mismatch: bool,
            terminal_fail: bool,
        }

        impl FakeAdmission {
            fn new() -> Self {
                Self {
                    state: Rc::new(RefCell::new(JournalState::default())),
                    reject: false,
                    mismatch: false,
                    terminal_fail: false,
                }
            }

            fn sibling(&self) -> Self {
                Self {
                    state: self.state.clone(),
                    reject: self.reject,
                    mismatch: self.mismatch,
                    terminal_fail: self.terminal_fail,
                }
            }
        }

        struct FakeAttempt {
            state: Rc<RefCell<JournalState>>,
            key: String,
            attempt_key: String,
            receipt: MutationAdmissionReceipt,
            terminal_fail: bool,
        }

        fn authority_key(context: &MutationContext) -> String {
            context
                .payload
                .pointer("/request/target/pull_request")
                .or_else(|| {
                    context
                        .payload
                        .pointer("/request/preparation/input/target_repository")
                })
                .map(Value::to_string)
                .unwrap_or_else(|| context.action.clone())
        }

        impl MutationAdmission for FakeAdmission {
            fn admit<'a>(
                &'a mut self,
                context: &MutationContext,
            ) -> anyhow::Result<Box<dyn AdmittedMutationAttempt + 'a>> {
                if self.reject {
                    anyhow::bail!("journal unavailable");
                }
                let key = authority_key(context);
                let attempt_key = format!("{}:{}", context.operation_id, context.attempt_id);
                let mut state = self.state.borrow_mut();
                if state.seen_attempts.contains(&attempt_key)
                    || !state.inflight_targets.insert(key.clone())
                {
                    anyhow::bail!("attempt replay or target already has an admitted attempt");
                }
                state.contexts.push(context.clone());
                drop(state);
                Ok(Box::new(FakeAttempt {
                    state: self.state.clone(),
                    key,
                    attempt_key,
                    receipt: MutationAdmissionReceipt {
                        operation_id: if self.mismatch {
                            "wrong-operation".into()
                        } else {
                            context.operation_id.clone()
                        },
                        attempt_id: context.attempt_id.clone(),
                        durable_record_id: "durable-1".into(),
                    },
                    terminal_fail: self.terminal_fail,
                }))
            }
        }

        impl AdmittedMutationAttempt for FakeAttempt {
            fn receipt(&self) -> &MutationAdmissionReceipt {
                &self.receipt
            }

            fn record_terminal(&mut self, record: &MutationTerminalRecord) -> anyhow::Result<()> {
                if self.terminal_fail {
                    anyhow::bail!("terminal fsync failed");
                }
                let mut state = self.state.borrow_mut();
                state.records.push(record.clone());
                state.seen_attempts.insert(self.attempt_key.clone());
                if !matches!(record, MutationTerminalRecord::Uncertain { .. }) {
                    state.inflight_targets.remove(&self.key);
                }
                Ok(())
            }
        }

        fn title_request() -> PullRequestLifecycleRequest {
            PullRequestLifecycleRequest {
                operation_id: "op-title".into(),
                attempt_id: "attempt-1".into(),
                target: mutation_target("OPEN"),
                action: PullRequestLifecycleAction::UpdateTitle {
                    observed: "old title".into(),
                    value: "new title".into(),
                },
            }
        }

        fn lifecycle_ack(field: &str, operation: &str, id: &str) -> Value {
            json!({"data":{field:{"clientMutationId":operation,"pullRequest":{"id":id}}}})
        }

        fn issue_ack(assignees: &[&str]) -> Value {
            json!({
                "number":7,
                "html_url":"https://github.com/owner/repo/issues/7",
                "pull_request":{
                    "url":"https://api.github.com/repos/owner/repo/pulls/7",
                    "html_url":"https://github.com/owner/repo/pull/7",
                    "diff_url":"https://github.com/owner/repo/pull/7.diff",
                    "patch_url":"https://github.com/owner/repo/pull/7.patch",
                    "merged_at":null
                },
                "assignees":assignees.iter().map(|login| json!({"login":login})).collect::<Vec<_>>()
            })
        }

        #[test]
        fn lifecycle_snapshot_binds_account_node_and_reports_incomplete_values() {
            let _serial = lifecycle_test_lock();
            let mut response = lifecycle_response(
                "alice",
                "OPEN",
                false,
                "title",
                "body",
                "main",
                &[("USER", "bob")],
                &["bug"],
                &["alice"],
            );
            response["errors"] = json!([{"message":"one field unavailable"}]);
            response["data"]["repository"]["pullRequest"]["reviewRequests"]["pageInfo"]
                ["hasNextPage"] = json!(true);
            let (_dir, provider) = lifecycle_fixture("alice", vec![lifecycle_step(response)]);
            let snapshot = provider
                .pr_lifecycle_snapshot(&lifecycle_repo("alice"), 7)
                .unwrap();
            assert_eq!(snapshot.pull_request.remote_id, "PR_node");
            assert_eq!(snapshot.viewer_login, "alice");
            assert!(!snapshot.values_complete);
            assert!(!snapshot.capabilities_complete);
            assert!(snapshot.notice.unwrap().contains("partial"));
        }

        #[test]
        fn repository_choices_are_bounded_and_fail_independently() {
            let _serial = lifecycle_test_lock();
            let steps = vec![
                get_step("repos/owner/repo/branches?per_page=100&page=1", json!([{"name":"main"}])),
                get_step("repos/owner/repo/labels?per_page=100&page=1", json!([{"name":"bug","node_id":"LABEL_1"}])),
                get_step("repos/owner/repo/assignees?per_page=100&page=1", json!([{"login":"alice","node_id":"USER_1"}])),
                get_step("repos/owner/repo/collaborators?affiliation=all&per_page=100&page=1", json!([{"login":"bob","node_id":"USER_2"}])),
                json!({"transport":"get","endpoint":"repos/owner/repo/teams?per_page=100&page=1","fail":true}),
            ];
            let (_dir, provider) = lifecycle_fixture("alice", steps);
            let choices = provider
                .pr_lifecycle_choices(&lifecycle_repo("alice"))
                .unwrap();
            assert_eq!(
                choices.labels.values.first().and_then(|value| value.remote_id.as_deref()),
                Some("LABEL_1"),
                "{:?}",
                choices.labels
            );
            assert!(choices.branches.complete);
            assert!(!choices.reviewer_teams.complete);
            assert!(choices.reviewer_teams.notice.is_some());
        }

        #[test]
        fn title_update_preserves_unrelated_fields_and_records_before_success() {
            let _serial = lifecycle_test_lock();
            let initial = lifecycle_response("alice", "OPEN", false, "old title", "body-a", "main", &[], &[], &[]);
            let under_lock = lifecycle_response("alice", "OPEN", false, "old title", "body-b", "main", &[], &[], &[]);
            let observed = lifecycle_response("alice", "OPEN", false, "new title", "body-b", "main", &[], &[], &[]);
            let variables = json!({
                "pullRequestId":"PR_node","title":"new title","clientMutationId":"op-title"
            });
            let steps = vec![
                lifecycle_step(initial),
                lifecycle_step(under_lock),
                update_graphql_step(
                    "title",
                    variables,
                    json!({"title":"old title","body":"body-b","baseRefName":"main"}),
                    json!({"title":"new title","body":"body-b","baseRefName":"main"}),
                    lifecycle_ack("updatePullRequest", "op-title", "PR_node"),
                ),
                lifecycle_step(observed),
            ];
            let (dir, provider) = lifecycle_fixture("alice", steps);
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_lifecycle(
                &lifecycle_repo("alice"),
                &title_request(),
                &mut admission,
            );
            assert!(matches!(result, ProviderMutationOutcome::Acknowledged(_)));
            assert_eq!(file_count(&dir, "writes"), 1);
            let state = admission.state.borrow();
            assert_eq!(state.contexts.len(), 1);
            assert_eq!(state.records.len(), 1);
            assert!(matches!(state.records[0], MutationTerminalRecord::Acknowledged { .. }));
            let dispatched = state.contexts[0]
                .payload
                .pointer("/dispatch/variables")
                .and_then(Value::as_object)
                .unwrap();
            assert_eq!(dispatched.len(), 3);
            assert!(!dispatched.contains_key("body"));
            assert!(!dispatched.contains_key("baseRefName"));
        }

        #[test]
        fn body_clear_and_base_update_each_send_only_the_requested_delta() {
            let _serial = lifecycle_test_lock();
            let body_request = PullRequestLifecycleRequest {
                operation_id: "op-body".into(),
                attempt_id: "attempt-body".into(),
                target: mutation_target("OPEN"),
                action: PullRequestLifecycleAction::UpdateBody {
                    observed: "old body".into(),
                    value: String::new(),
                },
            };
            let body_steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "old body", "main", &[], &[], &[])),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "old body", "main", &[], &[], &[])),
                update_graphql_step(
                    "body",
                    json!({"pullRequestId":"PR_node","body":"","clientMutationId":"op-body"}),
                    json!({"title":"title","body":"old body","baseRefName":"main"}),
                    json!({"title":"title","body":"","baseRefName":"main"}),
                    lifecycle_ack("updatePullRequest", "op-body", "PR_node"),
                ),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "", "main", &[], &[], &[])),
            ];
            let (body_dir, body_provider) = lifecycle_fixture("alice", body_steps);
            let mut body_admission = FakeAdmission::new();
            assert!(matches!(
                body_provider.execute_pr_lifecycle(
                    &lifecycle_repo("alice"),
                    &body_request,
                    &mut body_admission,
                ),
                ProviderMutationOutcome::Acknowledged(_)
            ));
            assert_eq!(file_count(&body_dir, "writes"), 1);

            let base_request = PullRequestLifecycleRequest {
                operation_id: "op-base".into(),
                attempt_id: "attempt-base".into(),
                target: mutation_target("OPEN"),
                action: PullRequestLifecycleAction::UpdateBaseBranch {
                    observed: "main".into(),
                    value: "release".into(),
                },
            };
            let base_steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                update_graphql_step(
                    "baseRefName",
                    json!({"pullRequestId":"PR_node","baseRefName":"release","clientMutationId":"op-base"}),
                    json!({"title":"title","body":"body","baseRefName":"main"}),
                    json!({"title":"title","body":"body","baseRefName":"release"}),
                    lifecycle_ack("updatePullRequest", "op-base", "PR_node"),
                ),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "release", &[], &[], &[])),
            ];
            let (base_dir, base_provider) = lifecycle_fixture("alice", base_steps);
            let mut base_admission = FakeAdmission::new();
            assert!(matches!(
                base_provider.execute_pr_lifecycle(
                    &lifecycle_repo("alice"),
                    &base_request,
                    &mut base_admission,
                ),
                ProviderMutationOutcome::Acknowledged(_)
            ));
            assert_eq!(file_count(&base_dir, "writes"), 1);
        }

        #[test]
        fn stale_affected_field_refuses_before_admission_and_dispatch() {
            let _serial = lifecycle_test_lock();
            let steps = vec![lifecycle_step(lifecycle_response(
                "alice", "OPEN", false, "someone changed it", "body", "main", &[], &[], &[],
            ))];
            let (dir, provider) = lifecycle_fixture("alice", steps);
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_lifecycle(
                &lifecycle_repo("alice"),
                &title_request(),
                &mut admission,
            );
            assert!(matches!(result, ProviderMutationOutcome::PreflightRejected { .. }));
            assert_eq!(file_count(&dir, "writes"), 0);
            assert!(admission.state.borrow().contexts.is_empty());
        }

        #[test]
        fn post_admission_change_records_not_started_and_dispatches_zero_writes() {
            let _serial = lifecycle_test_lock();
            let steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "old title", "body", "main", &[], &[], &[])),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "moved title", "body", "main", &[], &[], &[])),
            ];
            let (dir, provider) = lifecycle_fixture("alice", steps);
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_lifecycle(
                &lifecycle_repo("alice"),
                &title_request(),
                &mut admission,
            );
            assert!(matches!(result, ProviderMutationOutcome::PreflightRejected { .. }));
            assert_eq!(file_count(&dir, "writes"), 0);
            let records = admission.state.borrow().records.clone();
            assert_eq!(records.len(), 1, "{records:?}");
            assert!(matches!(records[0], MutationTerminalRecord::NotStarted { .. }), "{records:?}");
        }

        #[test]
        fn label_and_team_reviewer_use_delta_endpoints() {
            let _serial = lifecycle_test_lock();
            let label_request = PullRequestLifecycleRequest {
                operation_id: "op-label".into(),
                attempt_id: "attempt-label".into(),
                target: mutation_target("OPEN"),
                action: PullRequestLifecycleAction::AddLabel("bug".into()),
            };
            let label_steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                rest_step("POST", "repos/owner/repo/issues/7/labels", json!({"labels":["bug"]}), json!([{"name":"bug"}]), true),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &["bug"], &[])),
            ];
            let (label_dir, label_provider) = lifecycle_fixture("alice", label_steps);
            let mut label_admission = FakeAdmission::new();
            assert!(matches!(
                label_provider.execute_pr_lifecycle(&lifecycle_repo("alice"), &label_request, &mut label_admission),
                ProviderMutationOutcome::Acknowledged(_)
            ));
            assert_eq!(file_count(&label_dir, "writes"), 1);

            let reviewer = PullRequestReviewer { kind:"TEAM".into(), name:"core".into() };
            let reviewer_request = PullRequestLifecycleRequest {
                operation_id: "op-reviewer".into(),
                attempt_id: "attempt-reviewer".into(),
                target: mutation_target("OPEN"),
                action: PullRequestLifecycleAction::RemoveReviewer(reviewer),
            };
            let reviewer_steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[("TEAM","core")], &[], &[])),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[("TEAM","core")], &[], &[])),
                rest_step("DELETE", "repos/owner/repo/pulls/7/requested_reviewers", json!({"reviewers":[],"team_reviewers":["core"]}), json!({"number":7,"html_url":"https://github.com/owner/repo/pull/7","requested_reviewers":[],"requested_teams":[]}), true),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
            ];
            let (reviewer_dir, reviewer_provider) = lifecycle_fixture("alice", reviewer_steps);
            let mut reviewer_admission = FakeAdmission::new();
            assert!(matches!(
                reviewer_provider.execute_pr_lifecycle(&lifecycle_repo("alice"), &reviewer_request, &mut reviewer_admission),
                ProviderMutationOutcome::Acknowledged(_)
            ));
            assert_eq!(file_count(&reviewer_dir, "writes"), 1);
        }

        #[test]
        fn merged_pull_request_is_distinct_and_cannot_reopen_or_change_draft() {
            let _serial = lifecycle_test_lock();
            let merged = lifecycle_response("alice", "MERGED", false, "title", "body", "main", &[], &[], &[]);
            let (dir, provider) = lifecycle_fixture("alice", vec![lifecycle_step(merged)]);
            let mut request = PullRequestLifecycleRequest {
                operation_id: "op-reopen".into(),
                attempt_id: "attempt-reopen".into(),
                target: mutation_target("MERGED"),
                action: PullRequestLifecycleAction::Reopen,
            };
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_lifecycle(&lifecycle_repo("alice"), &request, &mut admission);
            assert!(matches!(result, ProviderMutationOutcome::PreflightRejected { .. }));
            assert_eq!(file_count(&dir, "writes"), 0);
            request.action = PullRequestLifecycleAction::ConvertToDraft;
        }

        #[test]
        fn unavailable_reviewer_permission_is_reasoned_and_dispatches_zero_writes() {
            let _serial = lifecycle_test_lock();
            let mut response = lifecycle_response(
                "alice", "OPEN", false, "title", "body", "main", &[], &[], &[],
            );
            response["data"]["repository"]["viewerPermission"] = json!("READ");
            let (dir, provider) = lifecycle_fixture("alice", vec![lifecycle_step(response)]);
            let request = PullRequestLifecycleRequest {
                operation_id:"op-reviewer".into(), attempt_id:"attempt-reviewer".into(),
                target:mutation_target("OPEN"),
                action:PullRequestLifecycleAction::AddReviewer(PullRequestReviewer { kind:"USER".into(), name:"bob".into() }),
            };
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_lifecycle(&lifecycle_repo("alice"), &request, &mut admission);
            let ProviderMutationOutcome::PreflightRejected { reason } = result else { panic!("expected refusal") };
            assert!(reason.contains("permission"));
            assert_eq!(file_count(&dir, "writes"), 0);
            assert!(admission.state.borrow().contexts.is_empty());
        }

        #[test]
        fn admission_failure_and_receipt_mismatch_dispatch_zero_writes() {
            let _serial = lifecycle_test_lock();
            for mismatch in [false, true] {
                let steps = vec![
                    lifecycle_step(lifecycle_response("alice", "OPEN", false, "old title", "body", "main", &[], &[], &[])),
                    if mismatch {
                        json!({"transport":"graphql","marker":"unused","variables":{},"response":{}})
                    } else {
                        json!({"transport":"graphql","marker":"unused","variables":{},"response":{}})
                    },
                ];
                let (dir, provider) = lifecycle_fixture("alice", steps);
                let mut admission = FakeAdmission::new();
                admission.reject = !mismatch;
                admission.mismatch = mismatch;
                let result = provider.execute_pr_lifecycle(&lifecycle_repo("alice"), &title_request(), &mut admission);
                assert!(matches!(result, ProviderMutationOutcome::PreflightRejected { .. }));
                assert_eq!(file_count(&dir, "writes"), 0);
                assert_eq!(file_count(&dir, "count"), 1);
            }
        }

        #[test]
        fn held_target_authority_refuses_a_second_attempt() {
            let _serial = lifecycle_test_lock();
            let context = MutationContext {
                operation_id: "held-op".into(),
                attempt_id: "held-attempt".into(),
                action: "update-pr-title".into(),
                payload: json!({"request":{"target":{"pull_request":mutation_target("OPEN").pull_request}}}),
            };
            let first = FakeAdmission::new();
            let mut holder = first.sibling();
            let _guard = holder.admit(&context).unwrap();
            let steps = vec![lifecycle_step(lifecycle_response("alice", "OPEN", false, "old title", "body", "main", &[], &[], &[]))];
            let (dir, provider) = lifecycle_fixture("alice", steps);
            let mut contender = first.sibling();
            let result = provider.execute_pr_lifecycle(&lifecycle_repo("alice"), &title_request(), &mut contender);
            assert!(matches!(result, ProviderMutationOutcome::PreflightRejected { .. }));
            assert_eq!(file_count(&dir, "writes"), 0);
        }

        #[test]
        fn terminal_attempt_is_tombstoned_against_restart_replay() {
            let _serial = lifecycle_test_lock();
            let context = MutationContext {
                operation_id: "done-op".into(),
                attempt_id: "done-attempt".into(),
                action: "update-pr-title".into(),
                payload: json!({"request":{"target":{"pull_request":mutation_target("OPEN").pull_request}}}),
            };
            let admission = FakeAdmission::new();
            let mut writer = admission.sibling();
            let mut guard = writer.admit(&context).unwrap();
            guard
                .record_terminal(&MutationTerminalRecord::Acknowledged {
                    acknowledgement: json!({"ok":true}),
                })
                .unwrap();
            drop(guard);
            let mut restarted = admission.sibling();
            assert!(restarted.admit(&context).is_err());
        }

        #[test]
        fn acknowledged_write_with_terminal_save_failure_is_uncertain_and_not_replayable() {
            let _serial = lifecycle_test_lock();
            let steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "old title", "body", "main", &[], &[], &[])),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "old title", "body", "main", &[], &[], &[])),
                update_graphql_step(
                    "title",
                    json!({"pullRequestId":"PR_node","title":"new title","clientMutationId":"op-title"}),
                    json!({"title":"old title","body":"body","baseRefName":"main"}),
                    json!({"title":"new title","body":"body","baseRefName":"main"}),
                    lifecycle_ack("updatePullRequest", "op-title", "PR_node"),
                ),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "new title", "body", "main", &[], &[], &[])),
            ];
            let (dir, provider) = lifecycle_fixture("alice", steps);
            let mut admission = FakeAdmission::new();
            admission.terminal_fail = true;
            let result = provider.execute_pr_lifecycle(&lifecycle_repo("alice"), &title_request(), &mut admission);
            let ProviderMutationOutcome::Uncertain { context, reason } = result else { panic!("expected uncertainty, received {result:?}") };
            assert_eq!(context.operation_id, "op-title");
            assert!(reason.contains("durable InFlight retained"));
            assert_eq!(file_count(&dir, "writes"), 1);
            assert_eq!(admission.state.borrow().inflight_targets.len(), 1);
        }

        #[test]
        fn lost_comment_create_reply_is_uncertain_with_exact_parent_and_one_dispatch() {
            let _serial = lifecycle_test_lock();
            let request = PullRequestDiscussionRequest {
                operation_id: "op-comment".into(),
                attempt_id: "attempt-comment".into(),
                target: mutation_target("OPEN"),
                action: PullRequestDiscussionAction::Create { body:"hello".into() },
            };
            let variables = json!({"subjectId":"PR_node","body":"hello","clientMutationId":"op-comment"});
            let steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                failing_write("mutation AddTopLevelPullRequestComment(", variables),
            ];
            let (dir, provider) = lifecycle_fixture("alice", steps);
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_discussion(&lifecycle_repo("alice"), &request, &mut admission);
            let ProviderMutationOutcome::Uncertain { context, .. } = result else { panic!("expected uncertainty") };
            assert_eq!(file_count(&dir, "writes"), 1);
            assert_eq!(context.payload.pointer("/dispatch/variables/subjectId").and_then(Value::as_str), Some("PR_node"));
            assert!(matches!(admission.state.borrow().records.as_slice(), [MutationTerminalRecord::Uncertain { .. }]));
        }

        fn comment_ack_node(id: &str, number: u64, author: &str, body: &str) -> Value {
            json!({
                "id":id,"body":body,"createdAt":"2026-09-13T10:00:00Z",
                "updatedAt":"2026-09-13T11:00:00Z","url":format!("https://GITHUB.com/OWNER/REPO/pull/{number}#issuecomment-1"),
                "author":{"login":author},
                "repository":{"nameWithOwner":"OWNER/REPO"},
                "pullRequest":{"id":"PR_node","number":number,
                    "url":format!("https://GITHUB.com/OWNER/REPO/pull/{number}"),
                    "repository":{"nameWithOwner":"OWNER/REPO"}}
            })
        }

        fn comment_response(id: &str, number: u64, author: &str, body: &str) -> Value {
            let mut node = comment_ack_node(id, number, author, body);
            node["viewerCanUpdate"] = json!(true);
            node["viewerCanDelete"] = json!(true);
            json!({"data":{"viewer":{"login":"alice"},"node":node}})
        }

        #[test]
        fn production_shaped_comment_create_and_edit_acknowledgements_need_no_capabilities() {
            let _serial = lifecycle_test_lock();
            let create_request = PullRequestDiscussionRequest {
                operation_id: "op-create-comment".into(),
                attempt_id: "attempt-create-comment".into(),
                target: mutation_target("OPEN"),
                action: PullRequestDiscussionAction::Create { body: "hello".into() },
            };
            let create_ack = json!({"data":{"addComment":{
                "clientMutationId":"op-create-comment",
                "subject":{"id":"PR_node","number":7,
                    "url":"https://github.com/owner/repo/pull/7",
                    "repository":{"nameWithOwner":"owner/repo"}},
                "commentEdge":{"node":comment_ack_node("COMMENT_2", 7, "alice", "hello")}
            }}});
            assert!(create_ack.pointer("/data/addComment/commentEdge/node/viewerCanUpdate").is_none());
            assert!(create_ack.pointer("/data/addComment/commentEdge/node/viewerCanDelete").is_none());
            let create_steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                graphql_step(
                    "mutation AddTopLevelPullRequestComment(",
                    json!({"subjectId":"PR_node","body":"hello","clientMutationId":"op-create-comment"}),
                    create_ack,
                    true,
                ),
            ];
            let (create_dir, create_provider) = lifecycle_fixture("alice", create_steps);
            let mut create_admission = FakeAdmission::new();
            let ProviderMutationOutcome::Acknowledged(create) = create_provider
                .execute_pr_discussion(
                    &lifecycle_repo("alice"),
                    &create_request,
                    &mut create_admission,
                )
            else {
                panic!("expected create acknowledgement");
            };
            assert_eq!(create.comment.remote_id, "COMMENT_2");
            assert_eq!(file_count(&create_dir, "writes"), 1);

            let edit_request = PullRequestDiscussionRequest {
                operation_id: "op-edit-comment".into(),
                attempt_id: "attempt-edit-comment".into(),
                target: mutation_target("OPEN"),
                action: PullRequestDiscussionAction::Edit {
                    comment: ProviderCoordinates { remote_id:"COMMENT_1".into(), ..mutation_target("OPEN").pull_request },
                    selected_author: "alice".into(), observed_body: "old".into(),
                    observed_updated_at: "2026-09-13T11:00:00Z".into(), body: "new".into(),
                },
            };
            let edit_ack = json!({"data":{"updateIssueComment":{
                "clientMutationId":"op-edit-comment",
                "issueComment":comment_ack_node("COMMENT_1", 7, "alice", "new")
            }}});
            assert!(edit_ack.pointer("/data/updateIssueComment/issueComment/viewerCanUpdate").is_none());
            assert!(edit_ack.pointer("/data/updateIssueComment/issueComment/viewerCanDelete").is_none());
            let edit_steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                graphql_step("query TopLevelPullRequestComment", json!({"id":"COMMENT_1"}), comment_response("COMMENT_1", 7, "alice", "old"), false),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                graphql_step("query TopLevelPullRequestComment", json!({"id":"COMMENT_1"}), comment_response("COMMENT_1", 7, "alice", "old"), false),
                graphql_step(
                    "mutation UpdateTopLevelPullRequestComment(",
                    json!({"commentId":"COMMENT_1","body":"new","clientMutationId":"op-edit-comment"}),
                    edit_ack,
                    true,
                ),
            ];
            let (edit_dir, edit_provider) = lifecycle_fixture("alice", edit_steps);
            let mut edit_admission = FakeAdmission::new();
            assert!(matches!(
                edit_provider.execute_pr_discussion(
                    &lifecycle_repo("alice"),
                    &edit_request,
                    &mut edit_admission,
                ),
                ProviderMutationOutcome::Acknowledged(_)
            ));
            assert_eq!(file_count(&edit_dir, "writes"), 1);
        }

        #[test]
        fn missing_comment_preflight_capability_refuses_before_admission_or_write() {
            let _serial = lifecycle_test_lock();
            for missing in ["viewerCanUpdate", "viewerCanDelete"] {
                let action = if missing == "viewerCanUpdate" {
                    PullRequestDiscussionAction::Edit {
                        comment: ProviderCoordinates { remote_id:"COMMENT_1".into(), ..mutation_target("OPEN").pull_request },
                        selected_author:"alice".into(), observed_body:"old".into(),
                        observed_updated_at:"2026-09-13T11:00:00Z".into(), body:"new".into(),
                    }
                } else {
                    PullRequestDiscussionAction::Delete {
                        comment: ProviderCoordinates { remote_id:"COMMENT_1".into(), ..mutation_target("OPEN").pull_request },
                        selected_author:"alice".into(), observed_body:"old".into(),
                        observed_updated_at:"2026-09-13T11:00:00Z".into(),
                    }
                };
                let request = PullRequestDiscussionRequest {
                    operation_id: format!("op-missing-{missing}"),
                    attempt_id: format!("attempt-missing-{missing}"),
                    target: mutation_target("OPEN"),
                    action,
                };
                let mut response = comment_response("COMMENT_1", 7, "alice", "old");
                response["data"]["node"].as_object_mut().unwrap().remove(missing);
                let steps = vec![
                    lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                    graphql_step("query TopLevelPullRequestComment", json!({"id":"COMMENT_1"}), response, false),
                ];
                let (dir, provider) = lifecycle_fixture("alice", steps);
                let mut admission = FakeAdmission::new();
                assert!(matches!(
                    provider.execute_pr_discussion(
                        &lifecycle_repo("alice"),
                        &request,
                        &mut admission,
                    ),
                    ProviderMutationOutcome::PreflightRejected { .. }
                ));
                assert_eq!(file_count(&dir, "writes"), 0);
                assert!(admission.state.borrow().contexts.is_empty());
            }
        }

        #[test]
        fn comment_wrong_parent_or_author_refuses_without_admission() {
            let _serial = lifecycle_test_lock();
            let request = PullRequestDiscussionRequest {
                operation_id: "op-edit".into(),
                attempt_id: "attempt-edit".into(),
                target: mutation_target("OPEN"),
                action: PullRequestDiscussionAction::Edit {
                    comment: ProviderCoordinates { remote_id:"COMMENT_1".into(), ..mutation_target("OPEN").pull_request },
                    selected_author:"alice".into(), observed_body:"old".into(),
                    observed_updated_at:"2026-09-13T11:00:00Z".into(), body:"new".into(),
                },
            };
            let steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                graphql_step("query TopLevelPullRequestComment", json!({"id":"COMMENT_1"}), comment_response("COMMENT_1", 8, "alice", "old"), false),
            ];
            let (dir, provider) = lifecycle_fixture("alice", steps);
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_discussion(&lifecycle_repo("alice"), &request, &mut admission);
            assert!(matches!(result, ProviderMutationOutcome::PreflightRejected { .. }));
            assert_eq!(file_count(&dir, "writes"), 0);
            assert!(admission.state.borrow().contexts.is_empty());

            let author_steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                graphql_step("query TopLevelPullRequestComment", json!({"id":"COMMENT_1"}), comment_response("COMMENT_1", 7, "bob", "old"), false),
            ];
            let (author_dir, author_provider) = lifecycle_fixture("alice", author_steps);
            let mut author_admission = FakeAdmission::new();
            let author_result = author_provider.execute_pr_discussion(
                &lifecycle_repo("alice"),
                &request,
                &mut author_admission,
            );
            assert!(matches!(author_result, ProviderMutationOutcome::PreflightRejected { .. }));
            assert_eq!(file_count(&author_dir, "writes"), 0);
            assert!(author_admission.state.borrow().contexts.is_empty());

            let mut wrong_owner = comment_response("COMMENT_1", 7, "alice", "old");
            wrong_owner["data"]["node"]["url"] =
                json!("https://github.com/other/repo/pull/7#issuecomment-1");
            wrong_owner["data"]["node"]["pullRequest"]["url"] =
                json!("https://github.com/other/repo/pull/7");
            let url_steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                graphql_step("query TopLevelPullRequestComment", json!({"id":"COMMENT_1"}), wrong_owner, false),
            ];
            let (url_dir, url_provider) = lifecycle_fixture("alice", url_steps);
            let mut url_admission = FakeAdmission::new();
            let url_result = url_provider.execute_pr_discussion(
                &lifecycle_repo("alice"),
                &request,
                &mut url_admission,
            );
            assert!(matches!(url_result, ProviderMutationOutcome::PreflightRejected { .. }));
            assert_eq!(file_count(&url_dir, "writes"), 0);
            assert!(url_admission.state.borrow().contexts.is_empty());
        }

        #[test]
        fn mismatched_comment_acknowledgement_is_uncertain() {
            let _serial = lifecycle_test_lock();
            let request = PullRequestDiscussionRequest {
                operation_id: "op-edit".into(),
                attempt_id: "attempt-edit".into(),
                target: mutation_target("OPEN"),
                action: PullRequestDiscussionAction::Edit {
                    comment: ProviderCoordinates { remote_id:"COMMENT_1".into(), ..mutation_target("OPEN").pull_request },
                    selected_author:"alice".into(), observed_body:"old".into(),
                    observed_updated_at:"2026-09-13T11:00:00Z".into(), body:"new".into(),
                },
            };
            let mutation_response = json!({"data":{"updateIssueComment":{
                "clientMutationId":"op-edit","issueComment":comment_ack_node("COMMENT_OTHER",7,"alice","new")
            }}});
            let steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                graphql_step("query TopLevelPullRequestComment", json!({"id":"COMMENT_1"}), comment_response("COMMENT_1",7,"alice","old"), false),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                graphql_step("query TopLevelPullRequestComment", json!({"id":"COMMENT_1"}), comment_response("COMMENT_1",7,"alice","old"), false),
                graphql_step("mutation UpdateTopLevelPullRequestComment(", json!({"commentId":"COMMENT_1","body":"new","clientMutationId":"op-edit"}), mutation_response, true),
            ];
            let (dir, provider) = lifecycle_fixture("alice", steps);
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_discussion(&lifecycle_repo("alice"), &request, &mut admission);
            assert!(matches!(result, ProviderMutationOutcome::Uncertain { .. }), "{result:?}");
            assert_eq!(file_count(&dir, "writes"), 1);
        }

        #[test]
        fn exact_top_level_comment_delete_uses_issue_comment_schema() {
            let _serial = lifecycle_test_lock();
            let comment = ProviderCoordinates {
                remote_id: "COMMENT_1".into(),
                ..mutation_target("OPEN").pull_request
            };
            let request = PullRequestDiscussionRequest {
                operation_id: "op-delete".into(),
                attempt_id: "attempt-delete".into(),
                target: mutation_target("OPEN"),
                action: PullRequestDiscussionAction::Delete {
                    comment: comment.clone(), selected_author:"alice".into(),
                    observed_body:"old".into(), observed_updated_at:"2026-09-13T11:00:00Z".into(),
                },
            };
            let steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                graphql_step("query TopLevelPullRequestComment", json!({"id":"COMMENT_1"}), comment_response("COMMENT_1",7,"alice","old"), false),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                graphql_step("query TopLevelPullRequestComment", json!({"id":"COMMENT_1"}), comment_response("COMMENT_1",7,"alice","old"), false),
                graphql_step("mutation DeleteTopLevelPullRequestComment(", json!({"commentId":"COMMENT_1","clientMutationId":"op-delete"}), json!({"data":{"deleteIssueComment":{"clientMutationId":"op-delete"}}}), true),
            ];
            let (dir, provider) = lifecycle_fixture("alice", steps);
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_discussion(&lifecycle_repo("alice"), &request, &mut admission);
            let ProviderMutationOutcome::Acknowledged(ack) = result else { panic!("expected acknowledgement") };
            assert_eq!(ack.comment, comment);
            assert!(ack.deleted);
            assert_eq!(file_count(&dir, "writes"), 1);
        }

        #[test]
        fn exact_comment_absence_remains_inconclusive() {
            let _serial = lifecycle_test_lock();
            let steps = vec![graphql_step(
                "query TopLevelPullRequestComment",
                json!({"id":"COMMENT_missing"}),
                json!({"data":{"viewer":{"login":"alice"},"node":null}}),
                false,
            )];
            let (_dir, provider) = lifecycle_fixture("alice", steps);
            let coordinates = ProviderCoordinates {
                remote_id:"COMMENT_missing".into(), ..mutation_target("OPEN").pull_request
            };
            assert!(matches!(
                provider.reconcile_pr_comment(&lifecycle_repo("alice"), 7, &coordinates),
                $crate::domain::ProviderReadEvidence::Inconclusive { .. }
            ));
        }

        #[test]
        fn malformed_lifecycle_acknowledgement_is_uncertain_after_one_dispatch() {
            let _serial = lifecycle_test_lock();
            let steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "old title", "body", "main", &[], &[], &[])),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "old title", "body", "main", &[], &[], &[])),
                update_graphql_step(
                    "title",
                    json!({"pullRequestId":"PR_node","title":"new title","clientMutationId":"op-title"}),
                    json!({"title":"old title","body":"body","baseRefName":"main"}),
                    json!({"title":"new title","body":"body","baseRefName":"main"}),
                    json!({"data":{}}),
                ),
            ];
            let (dir, provider) = lifecycle_fixture("alice", steps);
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_lifecycle(&lifecycle_repo("alice"), &title_request(), &mut admission);
            assert!(matches!(result, ProviderMutationOutcome::Uncertain { .. }));
            assert_eq!(file_count(&dir, "writes"), 1);
        }

        #[test]
        fn mismatched_rest_delta_acknowledgement_is_uncertain() {
            let _serial = lifecycle_test_lock();
            let request = PullRequestLifecycleRequest {
                operation_id:"op-label-mismatch".into(), attempt_id:"attempt-label-mismatch".into(),
                target:mutation_target("OPEN"), action:PullRequestLifecycleAction::AddLabel("bug".into()),
            };
            let steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                rest_step("POST", "repos/owner/repo/issues/7/labels", json!({"labels":["bug"]}), json!([{"name":"other"}]), true),
            ];
            let (dir, provider) = lifecycle_fixture("alice", steps);
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_lifecycle(&lifecycle_repo("alice"), &request, &mut admission);
            assert!(matches!(result, ProviderMutationOutcome::Uncertain { .. }));
            assert_eq!(file_count(&dir, "writes"), 1);
        }

        #[test]
        fn draft_transition_uses_primary_schema_and_assignee_acks_use_issue_shape() {
            let _serial = lifecycle_test_lock();
            let draft_request = PullRequestLifecycleRequest {
                operation_id:"op-draft".into(), attempt_id:"attempt-draft".into(),
                target:mutation_target("OPEN"), action:PullRequestLifecycleAction::ConvertToDraft,
            };
            let draft_steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                graphql_step("mutation ConvertPullRequestToDraftLifecycle(", json!({"pullRequestId":"PR_node","clientMutationId":"op-draft"}), lifecycle_ack("convertPullRequestToDraft", "op-draft", "PR_node"), true),
                lifecycle_step(lifecycle_response("alice", "OPEN", true, "title", "body", "main", &[], &[], &[])),
            ];
            let (draft_dir, draft_provider) = lifecycle_fixture("alice", draft_steps);
            let mut draft_admission = FakeAdmission::new();
            assert!(matches!(draft_provider.execute_pr_lifecycle(&lifecycle_repo("alice"), &draft_request, &mut draft_admission), ProviderMutationOutcome::Acknowledged(_)));
            assert_eq!(file_count(&draft_dir, "writes"), 1);

            let assignee_request = PullRequestLifecycleRequest {
                operation_id:"op-assignee".into(), attempt_id:"attempt-assignee".into(),
                target:mutation_target("OPEN"), action:PullRequestLifecycleAction::AddAssignee("bob".into()),
            };
            let assignee_steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                rest_step("POST", "repos/owner/repo/issues/7/assignees", json!({"assignees":["bob"]}), issue_ack(&["bob"]), true),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &["bob"])),
            ];
            let (assignee_dir, assignee_provider) = lifecycle_fixture("alice", assignee_steps);
            let mut assignee_admission = FakeAdmission::new();
            assert!(matches!(assignee_provider.execute_pr_lifecycle(&lifecycle_repo("alice"), &assignee_request, &mut assignee_admission), ProviderMutationOutcome::Acknowledged(_)));
            assert_eq!(file_count(&assignee_dir, "writes"), 1);

            let remove_request = PullRequestLifecycleRequest {
                operation_id:"op-remove-assignee".into(), attempt_id:"attempt-remove-assignee".into(),
                target:mutation_target("OPEN"), action:PullRequestLifecycleAction::RemoveAssignee("bob".into()),
            };
            let remove_steps = vec![
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &["bob"])),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &["bob"])),
                rest_step("DELETE", "repos/owner/repo/issues/7/assignees", json!({"assignees":["bob"]}), issue_ack(&[]), true),
                lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
            ];
            let (remove_dir, remove_provider) = lifecycle_fixture("alice", remove_steps);
            let mut remove_admission = FakeAdmission::new();
            assert!(matches!(remove_provider.execute_pr_lifecycle(&lifecycle_repo("alice"), &remove_request, &mut remove_admission), ProviderMutationOutcome::Acknowledged(_)));
            assert_eq!(file_count(&remove_dir, "writes"), 1);
        }

        #[test]
        fn assignee_ack_rejects_wrong_issue_or_pull_target_after_one_dispatch() {
            let _serial = lifecycle_test_lock();
            for (suffix, mutate) in [
                ("issue-repo", 0_u8),
                ("number", 1),
                ("missing-pull", 2),
                ("pull-repo", 3),
                ("pull-number", 4),
            ] {
                let request = PullRequestLifecycleRequest {
                    operation_id:format!("op-assignee-{suffix}"),
                    attempt_id:format!("attempt-assignee-{suffix}"),
                    target:mutation_target("OPEN"),
                    action:PullRequestLifecycleAction::AddAssignee("bob".into()),
                };
                let mut ack = issue_ack(&["bob"]);
                match mutate {
                    0 => ack["html_url"] = json!("https://github.com/other/repo/issues/7"),
                    1 => ack["number"] = json!(8),
                    2 => { ack.as_object_mut().unwrap().remove("pull_request"); }
                    3 => ack["pull_request"]["html_url"] = json!("https://github.com/other/repo/pull/7"),
                    4 => ack["pull_request"]["html_url"] = json!("https://github.com/owner/repo/pull/8"),
                    _ => unreachable!(),
                }
                let steps = vec![
                    lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                    lifecycle_step(lifecycle_response("alice", "OPEN", false, "title", "body", "main", &[], &[], &[])),
                    rest_step("POST", "repos/owner/repo/issues/7/assignees", json!({"assignees":["bob"]}), ack, true),
                ];
                let (dir, provider) = lifecycle_fixture("alice", steps);
                let mut admission = FakeAdmission::new();
                assert!(matches!(
                    provider.execute_pr_lifecycle(
                        &lifecycle_repo("alice"),
                        &request,
                        &mut admission,
                    ),
                    ProviderMutationOutcome::Uncertain { .. }
                ));
                assert_eq!(file_count(&dir, "writes"), 1);
            }
        }

        #[test]
        fn wrong_lifecycle_target_node_refuses_before_any_provider_read() {
            let _serial = lifecycle_test_lock();
            let (dir, provider) = lifecycle_fixture("alice", vec![]);
            let mut request = title_request();
            request.target.pull_request.owner = "other".into();
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_lifecycle(&lifecycle_repo("alice"), &request, &mut admission);
            assert!(matches!(result, ProviderMutationOutcome::PreflightRejected { .. }));
            assert_eq!(file_count(&dir, "count"), 0);
            assert_eq!(file_count(&dir, "writes"), 0);
        }

        #[test]
        fn simultaneous_named_accounts_keep_credentials_and_attempts_isolated() {
            let _serial = lifecycle_test_lock();
            let account_steps = |login: &str, operation: &str| {
                vec![
                    lifecycle_step(lifecycle_response(login, "OPEN", false, "old title", "body", "main", &[], &[], &[])),
                    lifecycle_step(lifecycle_response(login, "OPEN", false, "old title", "body", "main", &[], &[], &[])),
                    update_graphql_step(
                        "title",
                        json!({"pullRequestId":"PR_node","title":"new title","clientMutationId":operation}),
                        json!({"title":"old title","body":"body","baseRefName":"main"}),
                        json!({"title":"new title","body":"body","baseRefName":"main"}),
                        lifecycle_ack("updatePullRequest", operation, "PR_node"),
                    ),
                    lifecycle_step(lifecycle_response(login, "OPEN", false, "new title", "body", "main", &[], &[], &[])),
                ]
            };
            let (alice_dir, alice_provider) = lifecycle_fixture("alice", account_steps("alice", "op-alice"));
            let (bob_dir, bob_provider) = lifecycle_fixture("bob", account_steps("bob", "op-bob"));
            let run = |provider: GithubProvider, login: &'static str, operation: &'static str| {
                std::thread::spawn(move || {
                    let mut request = title_request();
                    request.operation_id = operation.into();
                    request.attempt_id = format!("attempt-{login}");
                    request.target.repository = lifecycle_repo(login);
                    request.target.pull_request.owner = "owner".into();
                    let mut admission = FakeAdmission::new();
                    provider.execute_pr_lifecycle(&lifecycle_repo(login), &request, &mut admission)
                })
            };
            let alice = run(alice_provider, "alice", "op-alice");
            let bob = run(bob_provider, "bob", "op-bob");
            assert!(matches!(alice.join().unwrap(), ProviderMutationOutcome::Acknowledged(_)));
            assert!(matches!(bob.join().unwrap(), ProviderMutationOutcome::Acknowledged(_)));
            assert_eq!(file_count(&alice_dir, "writes"), 1);
            assert_eq!(file_count(&bob_dir, "writes"), 1);
        }

        /// Explicit opt-in public read. No mutation query is sent.
        #[test]
        #[ignore = "uses existing gh auth and public cli/cli API reads"]
        fn live_public_pull_request_lifecycle_read_only() {
            let account = GithubProvider::accounts().unwrap().remove(0);
            let provider = GithubProvider::new(account);
            let repository = provider.repository("cli/cli").unwrap();
            let snapshot = provider.pr_lifecycle_snapshot(&repository, 14398).unwrap();
            assert_eq!(snapshot.repository.full_name(), "cli/cli");
            assert_eq!(snapshot.pull_request.pull_request, 14398);
            assert!(!snapshot.pull_request.remote_id.is_empty());
            assert!(matches!(snapshot.state.as_str(), "OPEN" | "CLOSED" | "MERGED"));
            assert_eq!(snapshot.head_sha.len(), 40);
        }

        fn creation_repo(full_name: &str, pull: bool, push: bool, parent: Option<&str>) -> Value {
            let (owner, name) = full_name.split_once('/').unwrap();
            json!({
                "name":name,"owner":{"login":owner},"full_name":full_name,
                "permissions":{"admin":false,"maintain":false,"push":push,"triage":false,"pull":pull},
                "parent":parent.map(|value| json!({"full_name":value})),"source":null
            })
        }

        fn creation_read_steps(head: &str) -> Vec<Value> {
            vec![
                get_step("user", json!({"login":"alice"})),
                get_step("repos/owner/repo", creation_repo("owner/repo", true, false, None)),
                get_step("repos/forker/fork", creation_repo("forker/fork", true, true, Some("owner/repo"))),
                get_step("repos/owner/repo/git/ref/heads/main", json!({"ref":"refs/heads/main","object":{"sha":BASE}})),
                get_step("repos/forker/fork/git/ref/heads/feature", json!({"ref":"refs/heads/feature","object":{"sha":head}})),
            ]
        }

        fn creation_input() -> PullRequestCreationInput {
            PullRequestCreationInput {
                target_repository: lifecycle_repo("alice"), base_branch:"main".into(),
                source_repository: source_repo("alice"), source_branch:"feature".into(),
                local_branch:Some("cibergit/worktree-17".into()), title:"new PR".into(),
                body:"body".into(), draft:true,
            }
        }

        fn creation_execution_steps(html_url: &str) -> Vec<Value> {
            let mut steps = creation_read_steps(HEAD);
            steps.extend(creation_read_steps(HEAD));
            steps.extend(creation_read_steps(HEAD));
            steps.push(rest_step(
                "POST", "repos/owner/repo/pulls",
                json!({"title":"new PR","body":"body","base":"main","head":"forker:feature","draft":true,"head_repo":"fork"}),
                json!({
                    "node_id":"PR_created","number":9,"html_url":html_url,
                    "state":"open","title":"new PR","body":"body","draft":true,
                    "base":{"sha":BASE,"ref":"main","repo":{"name":"repo","owner":{"login":"owner"}}},
                    "head":{"sha":NEW_HEAD,"ref":"feature","repo":{"name":"fork","owner":{"login":"forker"}}}
                }), true,
            ));
            steps
        }

        #[test]
        fn creation_preparation_preserves_explicit_local_source_distinction_and_rejects_moving_head() {
            let _serial = lifecycle_test_lock();
            let mut steps = creation_read_steps(HEAD);
            steps.extend(creation_read_steps(NEW_HEAD));
            let (dir, provider) = lifecycle_fixture("alice", steps);
            let preparation = provider.prepare_pr_creation(&creation_input()).unwrap();
            assert_eq!(preparation.input.source_branch, "feature");
            assert_eq!(preparation.input.local_branch.as_deref(), Some("cibergit/worktree-17"));
            assert!(!preparation.reviewed_head_atomically_enforced);
            let request = PullRequestCreationRequest { operation_id:"op-create".into(), attempt_id:"attempt-create".into(), preparation };
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_creation(&request, &mut admission);
            assert!(matches!(result, ProviderMutationOutcome::PreflightRejected { .. }));
            assert_eq!(file_count(&dir, "writes"), 0);
            assert!(admission.state.borrow().contexts.is_empty());
        }

        #[test]
        fn creation_rejects_unverified_cross_repository_source() {
            let _serial = lifecycle_test_lock();
            let steps = vec![
                get_step("user", json!({"login":"alice"})),
                get_step("repos/owner/repo", creation_repo("owner/repo", true, false, None)),
                get_step("repos/forker/fork", creation_repo("forker/fork", true, true, None)),
            ];
            let (dir, provider) = lifecycle_fixture("alice", steps);
            assert!(provider.prepare_pr_creation(&creation_input()).is_err());
            assert_eq!(file_count(&dir, "writes"), 0);

            let (invalid_dir, invalid_provider) = lifecycle_fixture("alice", vec![]);
            let mut invalid = creation_input();
            invalid.source_branch = "feature..moved".into();
            assert!(invalid_provider.prepare_pr_creation(&invalid).is_err());
            assert_eq!(file_count(&invalid_dir, "count"), 0);
        }

        #[test]
        fn mixed_case_creation_ack_records_actual_head_without_claiming_atomic_enforcement() {
            let _serial = lifecycle_test_lock();
            let steps = creation_execution_steps("https://GITHUB.com/OWNER/REPO/pull/9");
            let (dir, provider) = lifecycle_fixture("alice", steps);
            let preparation = provider.prepare_pr_creation(&creation_input()).unwrap();
            let request = PullRequestCreationRequest { operation_id:"op-create".into(), attempt_id:"attempt-create".into(), preparation };
            let mut admission = FakeAdmission::new();
            let result = provider.execute_pr_creation(&request, &mut admission);
            let ProviderMutationOutcome::Acknowledged(ack) = result else { panic!("expected acknowledgement") };
            assert_eq!(ack.reviewed_head_sha, HEAD);
            assert_eq!(ack.actual_head_sha, NEW_HEAD);
            assert!(!ack.reviewed_head_atomically_enforced);
            assert_eq!(file_count(&dir, "writes"), 1);
            let context = &admission.state.borrow().contexts[0];
            assert!(context.payload.pointer("/dispatch/variables/expected_head_sha").is_none());
        }

        #[test]
        fn creation_ack_refuses_wrong_url_owner_and_number_after_one_dispatch() {
            let _serial = lifecycle_test_lock();
            for (suffix, url) in [
                ("owner", "https://github.com/other/repo/pull/9"),
                ("number", "https://github.com/owner/repo/pull/10"),
            ] {
                let (dir, provider) = lifecycle_fixture("alice", creation_execution_steps(url));
                let preparation = provider.prepare_pr_creation(&creation_input()).unwrap();
                let request = PullRequestCreationRequest {
                    operation_id: format!("op-create-{suffix}"),
                    attempt_id: format!("attempt-create-{suffix}"),
                    preparation,
                };
                let mut admission = FakeAdmission::new();
                let result = provider.execute_pr_creation(&request, &mut admission);
                assert!(matches!(result, ProviderMutationOutcome::Uncertain { .. }), "{result:?}");
                assert_eq!(file_count(&dir, "writes"), 1);
                assert!(matches!(admission.state.borrow().records.as_slice(), [MutationTerminalRecord::Uncertain { .. }]));
            }
        }
    };
}
