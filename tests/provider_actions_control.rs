#[macro_export]
macro_rules! provider_actions_control_tests {
    () => {
        use super::*;
        use $crate::domain::{
            Account, ActionsAttemptLocator, ActionsRunControlAcknowledgement,
            ActionsRunControlAction, ActionsRunControlAuthority, ActionsRunControlRequest,
            CheckRepositoryIdentity, CheckSuiteIdentity, MutationAdmissionReceipt, MutationContext,
            MutationTerminalRecord, ProviderMutationOutcome, ProviderReadEvidence, Repository,
            WorkflowRunIdentity,
        };
        use $crate::providers::{ActionsRunControlDispatch, AdmittedMutationAttempt, MutationAdmission};
        use anyhow::Result;
        use serde_json::{Value, json};
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            sync::{Arc, Mutex},
            time::Duration,
        };
        use tempfile::TempDir;

        const CONTROL_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        fn control_account() -> Account {
            Account {
                host: "github.com".into(),
                login: "alice".into(),
            }
        }

        fn control_repo() -> Repository {
            Repository {
                host: "github.com".into(),
                owner: "owner".into(),
                name: "repo".into(),
                account: control_account(),
                local_path: None,
            }
        }

        fn control_repository_identity() -> CheckRepositoryIdentity {
            CheckRepositoryIdentity {
                node_id: "REPO_node".into(),
                name_with_owner: "owner/repo".into(),
            }
        }

        fn control_locator() -> ActionsAttemptLocator {
            let repository = control_repository_identity();
            ActionsAttemptLocator {
                account: control_account(),
                base_repository: repository.clone(),
                pull_request_node_id: "PR_node".into(),
                pull_request_number: 7,
                observed_head_sha: CONTROL_SHA.into(),
                head_repository: repository.clone(),
                rollup_commit_sha: CONTROL_SHA.into(),
                rollup_repository: repository.clone(),
                check_node_id: "CHECK_node".into(),
                check_database_id: 9,
                check_commit_sha: CONTROL_SHA.into(),
                check_repository: repository.clone(),
                suite: CheckSuiteIdentity {
                    node_id: "SUITE_node".into(),
                    database_id: Some(8),
                    repository,
                    app: None,
                },
                workflow_run: WorkflowRunIdentity {
                    node_id: "RUN_node".into(),
                    database_id: 6,
                    run_attempt: 2,
                    run_number: 5,
                    event: "pull_request".into(),
                    github_url: "https://github.com/owner/repo/actions/runs/6".into(),
                    workflow_node_id: "WORKFLOW_node".into(),
                    workflow_database_id: 4,
                    workflow_name: "CI".into(),
                },
            }
        }

        fn viewer_body() -> Value {
            json!({"login":"alice","node_id":"VIEWER_node"})
        }

        fn repository_body(push: bool) -> Value {
            json!({
                "node_id":"REPO_node","full_name":"owner/repo","archived":false,
                "permissions":{"admin":false,"maintain":false,"push":push,"triage":true,"pull":true}
            })
        }

        fn run_body(attempt: u64, status: &str, conclusion: Option<&str>) -> Value {
            json!({
                "id":6,"node_id":"RUN_node","run_attempt":attempt,"run_number":5,
                "event":"pull_request","status":status,"conclusion":conclusion,
                "workflow_id":4,"check_suite_id":8,"check_suite_node_id":"SUITE_node",
                "head_sha":CONTROL_SHA,
                "url":"https://api.github.com/repos/owner/repo/actions/runs/6",
                "html_url":"https://github.com/owner/repo/actions/runs/6",
                "workflow_url":"https://api.github.com/repos/owner/repo/actions/workflows/4",
                "repository":{"node_id":"REPO_node","full_name":"owner/repo"},
                "head_repository":{"node_id":"REPO_node","full_name":"owner/repo"}
            })
        }

        fn get_args(endpoint: &str) -> Value {
            json!([
                "api","--hostname","github.com","--method","GET",
                "--header","Accept: application/vnd.github+json",
                "--header","X-GitHub-Api-Version: 2026-03-10",
                endpoint
            ])
        }

        fn post_args(segment: &str) -> Value {
            json!([
                "api","--hostname","github.com","--method","POST",
                "--header","Accept: application/vnd.github+json",
                "--header","X-GitHub-Api-Version: 2026-03-10",
                "--include",
                format!("repos/owner/repo/actions/runs/6/{segment}"),
                "--input","-"
            ])
        }

        fn get_step(endpoint: &str, body: Value) -> Value {
            json!({"args": get_args(endpoint), "stdout": body.to_string()})
        }

        /// One complete observation triple: viewer, fresh repository
        /// permission, and the exact current run.
        fn observation_steps(push: bool, attempt: u64, status: &str, conclusion: Option<&str>) -> Vec<Value> {
            vec![
                get_step("user", viewer_body()),
                get_step("repos/owner/repo", repository_body(push)),
                get_step(
                    "repos/owner/repo/actions/runs/6",
                    run_body(attempt, status, conclusion),
                ),
            ]
        }

        fn post_step(segment: &str, stdout: &str, exit: i32) -> Value {
            json!({"args": post_args(segment), "stdout": stdout, "exit": exit})
        }

        fn accepted_response(status: u16) -> String {
            let reason = if status == 201 { "Created" } else { "Accepted" };
            format!("HTTP/2.0 {status} {reason}\r\nx-ratelimit-remaining: 4999\r\n\r\n")
        }

        fn control_fixture(steps: Vec<Value>) -> (TempDir, GithubProvider) {
            control_fixture_with(steps, Duration::from_secs(30), 16 * 1024 * 1024)
        }

        fn control_fixture_with(
            steps: Vec<Value>,
            timeout: Duration,
            output_limit: usize,
        ) -> (TempDir, GithubProvider) {
            let directory = tempfile::tempdir().unwrap();
            fs::write(
                directory.path().join("plan.json"),
                serde_json::to_vec(&json!({"steps": steps})).unwrap(),
            )
            .unwrap();
            let executable = directory.path().join("gh");
            fs::write(
                &executable,
                r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
root = pathlib.Path(__file__).parent
steps = json.loads((root / 'plan.json').read_text())['steps']
args = sys.argv[1:]
if args[:2] == ['auth', 'token']:
    assert args == ['auth','token','--hostname','github.com','--user','alice'], args
    assert 'GH_TOKEN' not in os.environ
    print('private-alice')
    sys.exit(0)
assert os.environ.get('GH_TOKEN') == 'private-alice'
count = root / 'count'
index = int(count.read_text()) if count.exists() else 0
assert index < len(steps), 'unexpected extra provider request: %r' % (args,)
step = steps[index]
count.write_text(str(index + 1))
assert args == step['args'], 'expected %r, got %r' % (step['args'], args)
if args[4] == 'POST':
    body = sys.stdin.read()
    assert json.loads(body) == {}, body
if step.get('stderr_headers'):
    sys.stderr.write(step['stderr_headers'])
    sys.stderr.flush()
    sys.stdout.write(step.get('stdout', ''))
    sys.stdout.flush()
    sys.exit(1)
if step.get('fail_process'):
    print('synthetic lost transport', file=sys.stderr)
    sys.exit(2)
sys.stdout.write(step['stdout'])
sys.stdout.flush()
if step.get('hang_seconds'):
    time.sleep(step['hang_seconds'])
if step.get('overflow_bytes'):
    sys.stdout.write('y' * step['overflow_bytes'])
    sys.stdout.flush()
    time.sleep(step.get('overflow_hang_seconds', 0))
sys.exit(step.get('exit', 0))
"#,
            )
            .unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let provider = GithubProvider {
                account: control_account(),
                runner: Runner {
                    gh: executable,
                    timeout,
                    output_limit,
                    ..Runner::default()
                },
            };
            (directory, provider)
        }

        fn control_count(directory: &TempDir) -> usize {
            fs::read_to_string(directory.path().join("count"))
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0)
        }

        #[derive(Clone, Default)]
        struct ControlAdmissionState {
            admitted: usize,
            terminal: Vec<MutationTerminalRecord>,
            contexts: Vec<MutationContext>,
        }

        struct ControlAdmission {
            state: Arc<Mutex<ControlAdmissionState>>,
            fail_terminal: bool,
            reject_replay: bool,
        }

        struct ControlAttempt {
            receipt: MutationAdmissionReceipt,
            state: Arc<Mutex<ControlAdmissionState>>,
            fail_terminal: bool,
        }

        impl MutationAdmission for ControlAdmission {
            fn admit<'a>(
                &'a mut self,
                context: &MutationContext,
            ) -> Result<Box<dyn AdmittedMutationAttempt + 'a>> {
                if self.reject_replay && self.state.lock().unwrap().admitted > 0 {
                    anyhow::bail!("synthetic durable InFlight replay barrier");
                }
                let mut state = self.state.lock().unwrap();
                state.admitted += 1;
                state.contexts.push(context.clone());
                drop(state);
                Ok(Box::new(ControlAttempt {
                    receipt: MutationAdmissionReceipt {
                        operation_id: context.operation_id.clone(),
                        attempt_id: context.attempt_id.clone(),
                        durable_record_id: "durable-actions-control-record".into(),
                    },
                    state: self.state.clone(),
                    fail_terminal: self.fail_terminal,
                }))
            }
        }

        impl AdmittedMutationAttempt for ControlAttempt {
            fn receipt(&self) -> &MutationAdmissionReceipt {
                &self.receipt
            }

            fn record_terminal(&mut self, record: &MutationTerminalRecord) -> Result<()> {
                if self.fail_terminal {
                    anyhow::bail!("synthetic terminal save failure");
                }
                self.state.lock().unwrap().terminal.push(record.clone());
                Ok(())
            }
        }

        fn control_admission() -> ControlAdmission {
            ControlAdmission {
                state: Arc::new(Mutex::new(ControlAdmissionState::default())),
                fail_terminal: false,
                reject_replay: false,
            }
        }

        fn prepared(
            provider: &GithubProvider,
            action: ActionsRunControlAction,
        ) -> std::result::Result<ActionsRunControlRequest, String> {
            provider.prepare_actions_run_control(
                &control_repo(),
                &control_locator(),
                action,
                "control-op".into(),
                "control-attempt".into(),
            )
        }

        #[test]
        fn rerun_all_jobs_sends_one_documented_post_and_reports_acceptance_only() {
            let mut steps = observation_steps(true, 2, "completed", Some("failure"));
            steps.extend(observation_steps(true, 2, "completed", Some("failure")));
            steps.push(post_step("rerun", &accepted_response(201), 0));
            steps.push(get_step(
                "repos/owner/repo/actions/runs/6",
                run_body(3, "in_progress", None),
            ));
            let (directory, provider) = control_fixture(steps);
            let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            assert_eq!(request.preparation.method, "POST");
            assert_eq!(
                request.preparation.path,
                "repos/owner/repo/actions/runs/6/rerun"
            );
            assert_eq!(request.preparation.body, json!({}));
            assert_eq!(request.preparation.observation.target.run_attempt, 2);
            assert_eq!(
                request.preparation.observation.authority,
                ActionsRunControlAuthority::Available
            );

            let mut admission = control_admission();
            let state = admission.state.clone();
            let dispatch =
                provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
            let ProviderMutationOutcome::Acknowledged(ack) = dispatch.outcome else {
                panic!("expected an acknowledgement: {:?}", dispatch.outcome);
            };
            assert_eq!(ack.accepted_status, 201);
            assert_eq!(ack.action, ActionsRunControlAction::RerunAllJobs);
            assert!(matches!(
                ack.observed_after,
                ProviderReadEvidence::Observed(ref progress) if progress.run_attempt == 3
            ));
            assert_eq!(control_count(&directory), 8);
            let state = state.lock().unwrap();
            assert_eq!(state.admitted, 1);
            assert!(matches!(
                state.terminal.as_slice(),
                [MutationTerminalRecord::Acknowledged { .. }]
            ));
            // The one frozen context backs admission and the exact dispatch.
            let context = state.contexts.first().unwrap();
            assert_eq!(context.action, "rerun-actions-run-all-jobs");
            assert_eq!(
                context.payload["dispatch"]["path"],
                json!("repos/owner/repo/actions/runs/6/rerun")
            );
            assert_eq!(context.payload["dispatch"]["method"], json!("POST"));
            assert_eq!(context.payload["dispatch"]["body"], json!({}));
        }

        #[test]
        fn cancel_uses_the_documented_202_and_its_own_path() {
            let mut steps = observation_steps(true, 2, "in_progress", None);
            steps.extend(observation_steps(true, 2, "in_progress", None));
            steps.push(post_step("cancel", &accepted_response(202), 0));
            steps.push(get_step(
                "repos/owner/repo/actions/runs/6",
                run_body(2, "in_progress", None),
            ));
            let (_directory, provider) = control_fixture(steps);
            let request = prepared(&provider, ActionsRunControlAction::CancelRun).unwrap();
            assert_eq!(
                request.preparation.path,
                "repos/owner/repo/actions/runs/6/cancel"
            );
            let mut admission = control_admission();
            let dispatch =
                provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
            let ProviderMutationOutcome::Acknowledged(ack) = dispatch.outcome else {
                panic!("expected an acknowledgement: {:?}", dispatch.outcome);
            };
            assert_eq!(ack.accepted_status, 202);
            // Acceptance is not cancellation: the follow-up read still shows
            // the run in progress and that is reported as observed, not proof.
            assert!(matches!(
                ack.observed_after,
                ProviderReadEvidence::Observed(ref progress) if progress.run_status == "in_progress"
            ));
        }

        #[test]
        fn a_documented_accepted_status_for_the_other_action_is_never_accepted() {
            let mut steps = observation_steps(true, 2, "in_progress", None);
            steps.extend(observation_steps(true, 2, "in_progress", None));
            // 201 is the re-run status; cancel documents 202 only.
            steps.push(post_step("cancel", &accepted_response(201), 0));
            let (_directory, provider) = control_fixture(steps);
            let request = prepared(&provider, ActionsRunControlAction::CancelRun).unwrap();
            let mut admission = control_admission();
            let state = admission.state.clone();
            let dispatch =
                provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
            let ProviderMutationOutcome::Uncertain { reason, .. } = dispatch.outcome else {
                panic!("expected an uncertain outcome: {:?}", dispatch.outcome);
            };
            assert!(reason.contains("201"));
            assert!(reason.contains("202"));
            assert!(matches!(
                state.lock().unwrap().terminal.as_slice(),
                [MutationTerminalRecord::Uncertain { .. }]
            ));
        }

        #[test]
        fn action_and_status_combinations_are_refused_before_any_write() {
            let (directory, provider) =
                control_fixture(observation_steps(true, 2, "in_progress", None));
            let error = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap_err();
            assert!(error.contains("in_progress"));
            assert_eq!(control_count(&directory), 3);

            let (_directory, provider) =
                control_fixture(observation_steps(true, 2, "completed", Some("success")));
            let error = prepared(&provider, ActionsRunControlAction::RerunFailedJobs).unwrap_err();
            assert!(error.contains("no failed job"));

            let (_directory, provider) =
                control_fixture(observation_steps(true, 2, "completed", Some("failure")));
            let error = prepared(&provider, ActionsRunControlAction::CancelRun).unwrap_err();
            assert!(error.contains("completed"));
        }

        #[test]
        fn a_later_attempt_refuses_preparation_instead_of_retargeting_it() {
            let (_directory, provider) =
                control_fixture(observation_steps(true, 4, "completed", Some("failure")));
            let error = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap_err();
            assert!(error.contains("historical"));
            assert!(error.contains("current attempt is 4"));
        }

        #[test]
        fn absent_write_permission_refuses_and_unknown_permission_is_disclosed() {
            let mut denied = observation_steps(false, 2, "completed", Some("failure"));
            denied[1] = get_step("repos/owner/repo", repository_body(false));
            let (_directory, provider) = control_fixture(denied);
            let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            assert!(matches!(
                request.preparation.observation.authority,
                ActionsRunControlAuthority::Unavailable { .. }
            ));
            let mut admission = control_admission();
            let state = admission.state.clone();
            let dispatch =
                provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
            assert!(matches!(
                dispatch.outcome,
                ProviderMutationOutcome::PreflightRejected { .. }
            ));
            // The refusal happened before durable admission, so no journal
            // authority was taken and nothing needs reconciliation.
            assert_eq!(state.lock().unwrap().admitted, 0);

            let mut unknown = observation_steps(true, 2, "completed", Some("failure"));
            unknown[1] = get_step(
                "repos/owner/repo",
                json!({"node_id":"REPO_node","full_name":"owner/repo","archived":false}),
            );
            let (_directory, provider) = control_fixture(unknown);
            let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            assert!(matches!(
                request.preparation.observation.authority,
                ActionsRunControlAuthority::Unknown { .. }
            ));
            assert!(
                request
                    .preparation
                    .notices
                    .iter()
                    .any(|notice| notice.contains("GitHub decides authorization"))
            );
        }

        #[test]
        fn an_archived_repository_is_unavailable_rather_than_unknown() {
            let mut steps = observation_steps(true, 2, "completed", Some("failure"));
            steps[1] = get_step(
                "repos/owner/repo",
                json!({
                    "node_id":"REPO_node","full_name":"owner/repo","archived":true,
                    "permissions":{"admin":true,"maintain":true,"push":true}
                }),
            );
            let (_directory, provider) = control_fixture(steps);
            let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            assert!(matches!(
                request.preparation.observation.authority,
                ActionsRunControlAuthority::Unavailable { .. }
            ));
        }

        #[test]
        fn a_run_that_moves_after_admission_dispatches_zero_writes() {
            let mut steps = observation_steps(true, 2, "completed", Some("failure"));
            steps.extend(observation_steps(true, 3, "in_progress", None));
            let (directory, provider) = control_fixture(steps);
            let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            let mut admission = control_admission();
            let state = admission.state.clone();
            let dispatch =
                provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
            let ProviderMutationOutcome::PreflightRejected { reason } = dispatch.outcome else {
                panic!("expected a rejection: {:?}", dispatch.outcome);
            };
            assert!(reason.contains("dispatched zero writes"));
            // The POST step was never reached.
            assert_eq!(control_count(&directory), 6);
            let state = state.lock().unwrap();
            assert_eq!(state.admitted, 1);
            assert!(matches!(
                state.terminal.as_slice(),
                [MutationTerminalRecord::NotStarted { .. }]
            ));
        }

        /// One complete observation triple whose run body is mutated by the
        /// caller, so a second read can differ in exactly one field.
        fn mutated_observation_steps(mutate: impl FnOnce(&mut Value)) -> Vec<Value> {
            let mut run = run_body(2, "completed", Some("failure"));
            mutate(&mut run);
            vec![
                get_step("user", viewer_body()),
                get_step("repos/owner/repo", repository_body(true)),
                get_step("repos/owner/repo/actions/runs/6", run),
            ]
        }

        /// The second read must re-observe every field the run endpoint
        /// returns. Each case moves exactly one of them.
        #[test]
        fn one_moved_second_read_field_at_a_time_dispatches_zero_writes() {
            let cases: Vec<(&str, Box<dyn FnOnce(&mut Value)>)> = vec![
                (
                    "head repository node",
                    Box::new(|run: &mut Value| {
                        run["head_repository"]["node_id"] = json!("OTHER_REPO_node")
                    }),
                ),
                (
                    "head repository name",
                    Box::new(|run: &mut Value| {
                        run["head_repository"]["full_name"] = json!("other/fork")
                    }),
                ),
                (
                    "absent head repository",
                    Box::new(|run: &mut Value| run["head_repository"] = Value::Null),
                ),
                (
                    "workflow URL",
                    Box::new(|run: &mut Value| {
                        run["workflow_url"] =
                            json!("https://api.github.com/repos/owner/repo/actions/workflows/99")
                    }),
                ),
                (
                    "run API URL",
                    Box::new(|run: &mut Value| {
                        run["url"] = json!("https://api.github.com/repos/owner/repo/actions/runs/99")
                    }),
                ),
                (
                    "run HTML URL",
                    Box::new(|run: &mut Value| {
                        run["html_url"] = json!("https://github.com/owner/repo/actions/runs/99")
                    }),
                ),
                (
                    "check suite database ID",
                    Box::new(|run: &mut Value| run["check_suite_id"] = json!(88)),
                ),
                (
                    "check suite node",
                    Box::new(|run: &mut Value| run["check_suite_node_id"] = json!("OTHER_SUITE")),
                ),
                (
                    "workflow database ID",
                    Box::new(|run: &mut Value| run["workflow_id"] = json!(44)),
                ),
                (
                    "run number",
                    Box::new(|run: &mut Value| run["run_number"] = json!(55)),
                ),
                (
                    "run event",
                    Box::new(|run: &mut Value| run["event"] = json!("push")),
                ),
                (
                    "run node",
                    Box::new(|run: &mut Value| run["node_id"] = json!("OTHER_RUN_node")),
                ),
            ];
            for (field, mutate) in cases {
                let mut steps = observation_steps(true, 2, "completed", Some("failure"));
                steps.extend(mutated_observation_steps(mutate));
                let (directory, provider) = control_fixture(steps);
                let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs)
                    .unwrap_or_else(|error| panic!("{field}: preparation failed: {error}"));
                let mut admission = control_admission();
                let state = admission.state.clone();
                let dispatch =
                    provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
                assert!(
                    matches!(
                        dispatch.outcome,
                        ProviderMutationOutcome::PreflightRejected { .. }
                    ),
                    "a moved {field} was not refused: {:?}",
                    dispatch.outcome
                );
                // The POST step was never reached.
                assert_eq!(control_count(&directory), 6, "{field} reached the transport");
                assert!(matches!(
                    state.lock().unwrap().terminal.as_slice(),
                    [MutationTerminalRecord::NotStarted { .. }]
                ));
            }
        }

        /// Scheduling evidence lives in the bounded leading header block, so it
        /// must survive a response this classifier will not read.
        #[test]
        fn an_unclassifiable_response_still_installs_the_server_rate_floor() {
            for body_bytes in [70 * 1024usize, 200 * 1024] {
                let mut steps = observation_steps(true, 2, "completed", Some("failure"));
                steps.extend(observation_steps(true, 2, "completed", Some("failure")));
                steps.push(post_step(
                    "rerun",
                    &format!(
                        "HTTP/2.0 429 Too Many Requests\r\nretry-after: 120\r\n\r\n{}",
                        "x".repeat(body_bytes)
                    ),
                    1,
                ));
                let (_directory, provider) = control_fixture(steps);
                let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
                let mut admission = control_admission();
                let state = admission.state.clone();
                let dispatch =
                    provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
                assert!(
                    matches!(dispatch.outcome, ProviderMutationOutcome::Uncertain { .. }),
                    "{body_bytes} byte body must stay uncertain: {:?}",
                    dispatch.outcome
                );
                assert_eq!(
                    dispatch.directive.rate_limit,
                    Some(GeneralReadDelay::Seconds(120)),
                    "a {body_bytes} byte body suppressed a complete leading rate directive"
                );
                assert!(matches!(
                    state.lock().unwrap().terminal.as_slice(),
                    [MutationTerminalRecord::Uncertain { .. }]
                ));
            }
        }

        const RATE_HEADERS: &str = "HTTP/2.0 429 Too Many Requests\r\nretry-after: 120\r\n\r\n";

        fn rate_floor_case(post: Value, timeout: Duration, output_limit: usize) -> ActionsRunControlDispatch {
            let mut steps = observation_steps(true, 2, "completed", Some("failure"));
            steps.extend(observation_steps(true, 2, "completed", Some("failure")));
            steps.push(post);
            let (_directory, provider) = control_fixture_with(steps, timeout, output_limit);
            let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            let mut admission = control_admission();
            provider.execute_actions_run_control(&control_repo(), &request, &mut admission)
        }

        /// A transport that starts, sends complete leading headers, then never
        /// finishes must still install the floor the server already stated.
        #[test]
        fn complete_leading_headers_survive_a_transport_timeout() {
            let mut post = post_step("rerun", RATE_HEADERS, 0);
            post["hang_seconds"] = json!(30);
            let dispatch = rate_floor_case(post, Duration::from_millis(150), 16 * 1024 * 1024);
            assert!(
                matches!(dispatch.outcome, ProviderMutationOutcome::Uncertain { .. }),
                "a started transport must stay uncertain: {:?}",
                dispatch.outcome
            );
            assert_eq!(
                dispatch.directive.rate_limit,
                Some(GeneralReadDelay::Seconds(120)),
                "a timeout discarded complete leading rate headers"
            );
        }

        /// The same holds when the child overflows the runner's output bound
        /// after its headers.
        #[test]
        fn complete_leading_headers_survive_an_output_overflow() {
            let mut post = post_step("rerun", RATE_HEADERS, 0);
            post["overflow_bytes"] = json!(64 * 1024);
            let dispatch = rate_floor_case(post, Duration::from_secs(10), 4 * 1024);
            assert!(
                matches!(dispatch.outcome, ProviderMutationOutcome::Uncertain { .. }),
                "an overflowing transport must stay uncertain: {:?}",
                dispatch.outcome
            );
            assert_eq!(
                dispatch.directive.rate_limit,
                Some(GeneralReadDelay::Seconds(120)),
                "an output overflow discarded complete leading rate headers"
            );
        }

        /// Child stderr is never scheduling evidence, however convincing it
        /// looks. Only stdout headers may install a floor.
        #[test]
        fn a_stderr_header_lookalike_never_installs_a_floor() {
            let mut post = post_step("rerun", "", 1);
            post["stderr_headers"] = json!(RATE_HEADERS);
            let dispatch = rate_floor_case(post, Duration::from_secs(10), 16 * 1024 * 1024);
            assert!(
                matches!(dispatch.outcome, ProviderMutationOutcome::Uncertain { .. }),
                "expected an uncertain outcome: {:?}",
                dispatch.outcome
            );
            assert_eq!(
                dispatch.directive.rate_limit, None,
                "child stderr installed an account floor"
            );
            assert_eq!(dispatch.directive.x_poll_interval, None);
        }

        /// The prefix capture is exercised directly, because wrapping a whole
        /// dispatch in a tracker scope would also divert its preflight reads
        /// onto the conditional entrypoint and prove nothing about capture.
        fn hanging_header_child(headers: &str, to_stderr: bool) -> (TempDir, Runner) {
            let directory = tempfile::tempdir().unwrap();
            let executable = directory.path().join("child");
            let stream = if to_stderr { "stderr" } else { "stdout" };
            fs::write(
                &executable,
                format!(
                    "#!/usr/bin/python3\nimport sys, time\nsys.{stream}.write({headers:?})\nsys.{stream}.flush()\ntime.sleep(30)\n"
                ),
            )
            .unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let runner = Runner {
                gh: executable,
                timeout: Duration::from_millis(150),
                ..Runner::default()
            };
            (directory, runner)
        }

        #[test]
        fn mutation_prefix_capture_never_touches_an_active_general_collector() {
            let (_directory, runner) = hanging_header_child(RATE_HEADERS, false);
            let command = std::process::Command::new(&runner.gh);
            let (result, collector, failure) = conditional::with_general_read_tracker(|| {
                runner.run_mutation_with_input(command, b"{}")
            });
            let failure_output = result.err().expect("the hanging child must time out");
            // The caller receives the server's own floor.
            assert_eq!(
                conditional::parse_mutation_response(&failure_output.header_prefix)
                    .poll
                    .rate_limit,
                Some(GeneralReadDelay::Seconds(120))
            );
            // The active general collector is untouched.
            assert_eq!(collector, GeneralReadDirective::default());
            assert!(failure.is_none());
        }

        /// The conditional read path must keep recording into the collector.
        #[test]
        fn conditional_prefix_capture_still_records_into_the_general_collector() {
            let (_directory, runner) = hanging_header_child(RATE_HEADERS, false);
            let mut command = std::process::Command::new(&runner.gh);
            let (result, collector, _) = conditional::with_general_read_tracker(|| {
                runner.run_inner_maybe_cancelled(
                    &mut command,
                    "GitHub conditional read request",
                    None,
                    None,
                    None,
                )
            });
            assert!(result.is_err());
            assert_eq!(
                collector.rate_limit,
                Some(GeneralReadDelay::Seconds(120)),
                "the conditional read path stopped recording its floor"
            );
        }

        /// Child stderr is never scheduling evidence at the capture layer
        /// either, so no prefix is retained from it at all.
        #[test]
        fn mutation_prefix_capture_retains_nothing_from_child_stderr() {
            let (_directory, runner) = hanging_header_child(RATE_HEADERS, true);
            let command = std::process::Command::new(&runner.gh);
            let failure = runner
                .run_mutation_with_input(command, b"{}")
                .err()
                .expect("the hanging child must time out");
            assert!(
                failure.header_prefix.is_empty(),
                "child stderr was retained as a header prefix"
            );
        }

        /// A single pipe read can span the blank line, so the retained prefix
        /// must be cut at the first delimiter.
        #[test]
        fn a_retained_prefix_is_the_header_block_only() {
            let crlf = b"HTTP/2.0 429 x\r\nretry-after: 5\r\n\r\nBODYBODY";
            assert_eq!(
                header_block_only(crlf),
                b"HTTP/2.0 429 x\r\nretry-after: 5\r\n\r\n".to_vec()
            );
            let lf = b"HTTP/2.0 429 x\nretry-after: 5\n\nBODYBODY";
            assert_eq!(
                header_block_only(lf),
                b"HTTP/2.0 429 x\nretry-after: 5\n\n".to_vec()
            );
            // A delimiter that has not arrived yet leaves an incomplete header
            // block, which is still header-only.
            let partial = b"HTTP/2.0 429 x\r\nretry-af";
            assert_eq!(header_block_only(partial), partial.to_vec());
            // The earliest delimiter wins, so a bare LF pair inside a CRLF
            // stream cannot leak the bytes after it.
            let mixed = b"HTTP/2.0 429 x\n\nBODY\r\n\r\nMORE";
            assert_eq!(header_block_only(mixed), b"HTTP/2.0 429 x\n\n".to_vec());
            assert!(header_block_only(b"").is_empty());
        }

        #[test]
        fn a_refusal_status_after_send_is_uncertain_and_never_recorded_not_applied() {
            // Only 403 and 429 carry a rate directive; no other refusal status
            // may invent an account floor.
            for (status, floors) in [
                (403u16, true),
                (429, true),
                (404, false),
                (409, false),
                (422, false),
                (500, false),
            ] {
                let mut steps = observation_steps(true, 2, "completed", Some("failure"));
                steps.extend(observation_steps(true, 2, "completed", Some("failure")));
                steps.push(post_step(
                    "rerun",
                    &format!(
                        "HTTP/2.0 {status} Refused\r\nretry-after: 60\r\n\r\n{{\"message\":\"no\"}}"
                    ),
                    1,
                ));
                let (_directory, provider) = control_fixture(steps);
                let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
                let mut admission = control_admission();
                let state = admission.state.clone();
                let dispatch =
                    provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
                let ProviderMutationOutcome::Uncertain { reason, .. } = dispatch.outcome else {
                    panic!("status {status} must stay uncertain: {:?}", dispatch.outcome);
                };
                assert!(reason.contains("crossed the transport"));
                let state = state.lock().unwrap();
                assert!(
                    matches!(
                        state.terminal.as_slice(),
                        [MutationTerminalRecord::Uncertain { .. }]
                    ),
                    "status {status} must never record NotStarted"
                );
                // Safely parsed server pacing survives the error status, and
                // only the statuses that actually carry one install a floor.
                assert_eq!(
                    dispatch.directive.rate_limit,
                    floors.then_some(GeneralReadDelay::Seconds(60)),
                    "status {status} produced the wrong account floor"
                );
                assert!(dispatch.directive.x_poll_interval.is_none());
            }
        }

        #[test]
        fn an_unframed_or_lost_response_stays_uncertain() {
            let mut steps = observation_steps(true, 2, "completed", Some("failure"));
            steps.extend(observation_steps(true, 2, "completed", Some("failure")));
            steps.push(post_step("rerun", "not an http response", 0));
            let (_directory, provider) = control_fixture(steps);
            let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            let mut admission = control_admission();
            let dispatch =
                provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
            let ProviderMutationOutcome::Uncertain { reason, .. } = dispatch.outcome else {
                panic!("expected an uncertain outcome: {:?}", dispatch.outcome);
            };
            assert!(reason.contains("could not be framed"));

            let mut steps = observation_steps(true, 2, "completed", Some("failure"));
            steps.extend(observation_steps(true, 2, "completed", Some("failure")));
            let mut lost = post_step("rerun", "", 0);
            lost["fail_process"] = json!(true);
            steps.push(lost);
            let (_directory, provider) = control_fixture(steps);
            let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            let mut admission = control_admission();
            let dispatch =
                provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
            assert!(matches!(
                dispatch.outcome,
                ProviderMutationOutcome::Uncertain { .. }
            ));
        }

        #[test]
        fn a_documented_status_with_an_unexpected_body_is_uncertain() {
            let mut steps = observation_steps(true, 2, "completed", Some("failure"));
            steps.extend(observation_steps(true, 2, "completed", Some("failure")));
            steps.push(post_step(
                "rerun",
                "HTTP/2.0 201 Created\r\n\r\n{\"message\":\"unexpected\"}",
                0,
            ));
            let (_directory, provider) = control_fixture(steps);
            let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            let mut admission = control_admission();
            let dispatch =
                provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
            let ProviderMutationOutcome::Uncertain { reason, .. } = dispatch.outcome else {
                panic!("expected an uncertain outcome: {:?}", dispatch.outcome);
            };
            assert!(reason.contains("unexpected body shape"));
        }

        #[test]
        fn a_failed_terminal_save_keeps_the_attempt_uncertain_with_inflight_retained() {
            let mut steps = observation_steps(true, 2, "completed", Some("failure"));
            steps.extend(observation_steps(true, 2, "completed", Some("failure")));
            steps.push(post_step("rerun", &accepted_response(201), 0));
            steps.push(get_step(
                "repos/owner/repo/actions/runs/6",
                run_body(3, "in_progress", None),
            ));
            let (_directory, provider) = control_fixture(steps);
            let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            let mut admission = control_admission();
            admission.fail_terminal = true;
            let dispatch =
                provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
            let ProviderMutationOutcome::Uncertain { reason, .. } = dispatch.outcome else {
                panic!("expected an uncertain outcome: {:?}", dispatch.outcome);
            };
            assert!(reason.contains("InFlight authority was retained"));
        }

        #[test]
        fn a_failed_not_started_save_is_uncertain_rather_than_a_clean_rejection() {
            let mut steps = observation_steps(true, 2, "completed", Some("failure"));
            steps.extend(observation_steps(true, 3, "in_progress", None));
            let (_directory, provider) = control_fixture(steps);
            let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            let mut admission = control_admission();
            admission.fail_terminal = true;
            let dispatch =
                provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
            let ProviderMutationOutcome::Uncertain { reason, .. } = dispatch.outcome else {
                panic!("expected an uncertain outcome: {:?}", dispatch.outcome);
            };
            assert!(reason.contains("dispatched zero writes"));
            assert!(reason.contains("InFlight authority was retained"));
        }

        #[test]
        fn a_replayed_attempt_is_refused_by_durable_admission_before_any_read() {
            let mut steps = observation_steps(true, 2, "completed", Some("failure"));
            steps.extend(observation_steps(true, 2, "completed", Some("failure")));
            steps.push(post_step("rerun", &accepted_response(201), 0));
            steps.push(get_step(
                "repos/owner/repo/actions/runs/6",
                run_body(3, "in_progress", None),
            ));
            let (directory, provider) = control_fixture(steps);
            let request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            let mut admission = control_admission();
            admission.reject_replay = true;
            assert!(matches!(
                provider
                    .execute_actions_run_control(&control_repo(), &request, &mut admission)
                    .outcome,
                ProviderMutationOutcome::Acknowledged(_)
            ));
            let sent = control_count(&directory);
            let replay =
                provider.execute_actions_run_control(&control_repo(), &request, &mut admission);
            assert!(matches!(
                replay.outcome,
                ProviderMutationOutcome::PreflightRejected { .. }
            ));
            assert_eq!(control_count(&directory), sent);
        }

        #[test]
        fn a_foreign_account_repository_or_body_is_refused_without_admission() {
            let mut steps = Vec::new();
            for _ in 0..3 {
                steps.extend(observation_steps(true, 2, "completed", Some("failure")));
            }
            let (_directory, provider) = control_fixture(steps);
            let mut request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            request.preparation.body = json!({"enable_debug_logging": true});
            let mut admission = control_admission();
            let state = admission.state.clone();
            assert!(matches!(
                provider
                    .execute_actions_run_control(&control_repo(), &request, &mut admission)
                    .outcome,
                ProviderMutationOutcome::PreflightRejected { .. }
            ));

            let mut request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            request.preparation.path = "repos/owner/repo/actions/runs/6/cancel".into();
            assert!(matches!(
                provider
                    .execute_actions_run_control(&control_repo(), &request, &mut admission)
                    .outcome,
                ProviderMutationOutcome::PreflightRejected { .. }
            ));

            let mut request = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap();
            request.preparation.observation.target.account.login = "mallory".into();
            assert!(matches!(
                provider
                    .execute_actions_run_control(&control_repo(), &request, &mut admission)
                    .outcome,
                ProviderMutationOutcome::PreflightRejected { .. }
            ));
            assert_eq!(state.lock().unwrap().admitted, 0);
        }

        #[test]
        fn a_moved_run_identity_never_prepares_a_control() {
            let mut steps = observation_steps(true, 2, "completed", Some("failure"));
            let mut moved = run_body(2, "completed", Some("failure"));
            moved["node_id"] = json!("OTHER_RUN_node");
            steps[2] = get_step("repos/owner/repo/actions/runs/6", moved);
            let (_directory, provider) = control_fixture(steps);
            let error = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap_err();
            assert!(error.contains("moved"));
        }

        #[test]
        fn a_credential_that_resolves_to_another_account_never_prepares_a_control() {
            let mut steps = observation_steps(true, 2, "completed", Some("failure"));
            steps[0] = get_step("user", json!({"login":"mallory","node_id":"OTHER_node"}));
            let (_directory, provider) = control_fixture(steps);
            let error = prepared(&provider, ActionsRunControlAction::RerunAllJobs).unwrap_err();
            assert!(error.contains("another account"));
        }

        #[test]
        fn reconciliation_reads_only_and_reports_exact_observed_movement() {
            let (directory, provider) = control_fixture(vec![get_step(
                "repos/owner/repo/actions/runs/6",
                run_body(3, "in_progress", None),
            )]);
            let target = ActionsRunControlAcknowledgement {
                operation_id: "op".into(),
                action: ActionsRunControlAction::RerunAllJobs,
                target: {
                    let mut steps = observation_steps(true, 2, "completed", Some("failure"));
                    steps.clear();
                    let (_temporary, fixture_provider) =
                        control_fixture(observation_steps(true, 2, "completed", Some("failure")));
                    prepared(&fixture_provider, ActionsRunControlAction::RerunAllJobs)
                        .unwrap()
                        .preparation
                        .observation
                        .target
                },
                accepted_status: 201,
                observed_after: ProviderReadEvidence::Inconclusive {
                    reason: "unused".into(),
                },
            };
            let evidence = provider.reconcile_actions_run_control(&control_repo(), &target.target);
            assert!(matches!(
                evidence,
                ProviderReadEvidence::Observed(ref progress) if progress.run_attempt == 3
            ));
            assert_eq!(control_count(&directory), 1);
        }
    };
}
