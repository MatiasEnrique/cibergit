#[macro_export]
macro_rules! provider_reaction_tests {
    () => {
        use super::*;
        use $crate::domain::{
            Account, FreshReactionCapability, MutationAdmissionReceipt, MutationContext,
            MutationTerminalRecord, ProviderCoordinates, ProviderMutationOutcome, ReactableKind,
            ReactionAction, ReactionContent, ReactionGroupSnapshot, ReactionIntent,
            ReactionSnapshot, ReactionSubjectSnapshot, Repository, SelectedViewer,
        };
        use $crate::providers::{AdmittedMutationAttempt, MutationAdmission};
        use anyhow::Result;
        use serde_json::{Value, json};
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            sync::{Arc, Mutex},
            time::Duration,
        };
        use tempfile::TempDir;

        fn reaction_account(login: &str) -> Account {
            Account {
                host: "github.com".into(),
                login: login.into(),
            }
        }

        fn reaction_repo(login: &str) -> Repository {
            Repository {
                host: "github.com".into(),
                owner: "owner".into(),
                name: "repo".into(),
                account: reaction_account(login),
                local_path: None,
            }
        }

        fn reaction_coordinates(id: &str) -> ProviderCoordinates {
            ProviderCoordinates {
                provider: "github".into(),
                host: "github.com".into(),
                owner: "owner".into(),
                repository: "repo".into(),
                pull_request: 7,
                remote_id: id.into(),
            }
        }

        fn subject_id(kind: ReactableKind) -> &'static str {
            match kind {
                ReactableKind::PullRequest => "PR_node",
                ReactableKind::PullRequestReview => "REVIEW_node",
                ReactableKind::IssueComment => "ISSUE_COMMENT_node",
                ReactableKind::PullRequestReviewComment => "REVIEW_COMMENT_node",
            }
        }

        fn subject_body(kind: ReactableKind) -> &'static str {
            match kind {
                ReactableKind::PullRequest => "PR body",
                ReactableKind::PullRequestReview => "Review body",
                ReactableKind::IssueComment => "Discussion body",
                ReactableKind::PullRequestReviewComment => "Inline body",
            }
        }

        fn reaction_groups(selected: ReactionContent, present: bool) -> Vec<ReactionGroupSnapshot> {
            ReactionContent::ALL
                .into_iter()
                .map(|content| ReactionGroupSnapshot {
                    content,
                    count: u64::from(content == selected && present),
                    viewer_has_reacted: content == selected && present,
                })
                .collect()
        }

        fn displayed_subject(
            kind: ReactableKind,
            content: ReactionContent,
            present: bool,
        ) -> ReactionSubjectSnapshot {
            ReactionSubjectSnapshot {
                kind,
                pull_request: reaction_coordinates("PR_node"),
                subject: reaction_coordinates(subject_id(kind)),
                parent_review: (kind == ReactableKind::PullRequestReviewComment)
                    .then(|| reaction_coordinates("REVIEW_parent")),
                content: subject_body(kind).into(),
                reactions: ReactionSnapshot {
                    groups: reaction_groups(content, present),
                    complete: true,
                },
                fresh_capability: Some(FreshReactionCapability {
                    viewer: SelectedViewer {
                        node_id: "USER_alice".into(),
                        login: "alice".into(),
                    },
                    viewer_can_react: true,
                }),
            }
        }

        fn target_node(
            kind: ReactableKind,
            content: ReactionContent,
            present: bool,
            own_id: &str,
        ) -> Value {
            let mut node = json!({
                "__typename": kind.graphql_name(),
                "id": subject_id(kind),
                "body": subject_body(kind),
                "viewerCanReact": true,
                "reactions": {
                    "viewerHasReacted": present,
                    "nodes": if present { vec![json!({
                        "id": own_id,
                        "content": content.graphql_name(),
                        "user": {"id":"USER_alice","login":"alice"},
                        "reactable": {"__typename":kind.graphql_name(),"id":subject_id(kind)}
                    })] } else { Vec::<Value>::new() },
                    "pageInfo": {"hasNextPage":false,"endCursor":null}
                }
            });
            match kind {
                ReactableKind::PullRequest => {
                    node["number"] = json!(7);
                    node["repository"] = json!({"nameWithOwner":"owner/repo"});
                }
                ReactableKind::PullRequestReview | ReactableKind::IssueComment => {
                    node["pullRequest"] = json!({
                        "id":"PR_node","number":7,
                        "repository":{"nameWithOwner":"owner/repo"}
                    });
                }
                ReactableKind::PullRequestReviewComment => {
                    node["pullRequestReview"] = json!({
                        "id":"REVIEW_parent",
                        "pullRequest":{"id":"PR_node","number":7,
                            "repository":{"nameWithOwner":"owner/repo"}}
                    });
                }
            }
            node
        }

        fn target_response(
            kind: ReactableKind,
            content: ReactionContent,
            present: bool,
            own_id: &str,
        ) -> Value {
            json!({"data":{
                "viewer":{"id":"USER_alice","login":"alice"},
                "node":target_node(kind, content, present, own_id)
            }})
        }

        fn mutation_response(
            kind: ReactableKind,
            content: ReactionContent,
            add: bool,
            operation_id: &str,
            reaction_id: &str,
        ) -> Value {
            let field = if add { "addReaction" } else { "removeReaction" };
            json!({"data":{(field):{
                "clientMutationId":operation_id,
                "reaction":{
                    "id":reaction_id,"content":content.graphql_name(),
                    "user":{"id":"USER_alice","login":"alice"},
                    "reactable":{"__typename":kind.graphql_name(),"id":subject_id(kind)}
                },
                "subject":target_node(kind, content, add, reaction_id)
            }}})
        }

        fn reaction_step(marker: &str, variables: Value, response: Value) -> Value {
            json!({"marker":marker,"variables":variables,"response":response})
        }

        fn reaction_fixture(steps: Vec<Value>) -> (TempDir, GithubProvider) {
            let dir = tempfile::tempdir().unwrap();
            fs::write(
                dir.path().join("steps.json"),
                serde_json::to_vec(&json!({"steps":steps})).unwrap(),
            )
            .unwrap();
            let executable = dir.path().join("gh");
            fs::write(
                &executable,
                r#"#!/usr/bin/python3
import json, os, pathlib, sys
root = pathlib.Path(__file__).parent
steps = json.loads((root / 'steps.json').read_text())['steps']
args = sys.argv[1:]
if args[:2] == ['auth','token']:
    assert args == ['auth','token','--hostname','github.com','--user','alice']
    assert 'GH_TOKEN' not in os.environ
    print('private-alice')
    sys.exit(0)
assert os.environ.get('GH_TOKEN') == 'private-alice'
assert args == ['api','--hostname','github.com','--method','POST','--header','Accept: application/vnd.github+json','--header','X-GitHub-Api-Version: 2026-03-10','graphql','--input','-']
count = root / 'count'
index = int(count.read_text()) if count.exists() else 0
step = steps[index]
payload = json.load(sys.stdin)
assert step['marker'] in payload['query']
assert payload['variables'] == step['variables']
if 'mutation AddReaction(' in payload['query']:
    assert 'addReaction(input:' in payload['query']
    assert 'removeReaction' not in payload['query']
if 'mutation RemoveReaction(' in payload['query']:
    assert 'removeReaction(input:' in payload['query']
    assert 'addReaction' not in payload['query']
assert '/reactions' not in ' '.join(args)
count.write_text(str(index + 1))
if step.get('fail'):
    print('synthetic lost transport', file=sys.stderr)
    sys.exit(1)
print(json.dumps(step['response']))
"#,
            )
            .unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let provider = GithubProvider {
                account: reaction_account("alice"),
                runner: Runner {
                    gh: executable,
                    timeout: Duration::from_secs(30),
                    ..Runner::default()
                },
            };
            (dir, provider)
        }

        fn reaction_count(dir: &TempDir) -> usize {
            fs::read_to_string(dir.path().join("count"))
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0)
        }

        #[derive(Clone, Default)]
        struct AdmissionState {
            admitted: usize,
            records: Vec<MutationTerminalRecord>,
        }

        struct TestAdmission {
            state: Arc<Mutex<AdmissionState>>,
            fail_admit: bool,
            fail_terminal: bool,
        }

        struct TestAttempt {
            receipt: MutationAdmissionReceipt,
            state: Arc<Mutex<AdmissionState>>,
            fail_terminal: bool,
        }

        impl MutationAdmission for TestAdmission {
            fn admit<'a>(
                &'a mut self,
                context: &MutationContext,
            ) -> Result<Box<dyn AdmittedMutationAttempt + 'a>> {
                if self.fail_admit {
                    anyhow::bail!("synthetic journal save failure");
                }
                self.state.lock().unwrap().admitted += 1;
                Ok(Box::new(TestAttempt {
                    receipt: MutationAdmissionReceipt {
                        operation_id: context.operation_id.clone(),
                        attempt_id: context.attempt_id.clone(),
                        durable_record_id: "durable-reaction-record".into(),
                    },
                    state: self.state.clone(),
                    fail_terminal: self.fail_terminal,
                }))
            }
        }

        impl AdmittedMutationAttempt for TestAttempt {
            fn receipt(&self) -> &MutationAdmissionReceipt {
                &self.receipt
            }

            fn record_terminal(&mut self, record: &MutationTerminalRecord) -> Result<()> {
                if self.fail_terminal {
                    anyhow::bail!("synthetic terminal save failure");
                }
                self.state.lock().unwrap().records.push(record.clone());
                Ok(())
            }
        }

        fn admission() -> TestAdmission {
            TestAdmission {
                state: Arc::new(Mutex::new(AdmissionState::default())),
                fail_admit: false,
                fail_terminal: false,
            }
        }

        #[test]
        fn all_four_subjects_and_all_contents_use_one_exact_graphql_add() {
            for kind in [
                ReactableKind::PullRequest,
                ReactableKind::PullRequestReview,
                ReactableKind::IssueComment,
                ReactableKind::PullRequestReviewComment,
            ] {
                for content in ReactionContent::ALL {
                    let operation_id = format!("add-{:?}-{}", kind, content.graphql_name());
                    let variables = json!({
                        "id":subject_id(kind),"content":content.graphql_name(),"after":null
                    });
                    let mutation_variables = json!({
                        "subjectId":subject_id(kind),"content":content.graphql_name(),
                        "clientMutationId":operation_id
                    });
                    let steps = vec![
                        reaction_step(
                            "query ReactionTarget(",
                            variables.clone(),
                            target_response(kind, content, false, "unused"),
                        ),
                        reaction_step(
                            "query ReactionTarget(",
                            variables,
                            target_response(kind, content, false, "unused"),
                        ),
                        reaction_step(
                            "mutation AddReaction(",
                            mutation_variables,
                            mutation_response(
                                kind,
                                content,
                                true,
                                &operation_id,
                                "REACTION_new",
                            ),
                        ),
                    ];
                    let (dir, provider) = reaction_fixture(steps);
                    let request = provider
                        .prepare_reaction(
                            &reaction_repo("alice"),
                            7,
                            &displayed_subject(kind, content, false),
                            content,
                            ReactionIntent::Add,
                            operation_id.clone(),
                            "attempt-add".into(),
                        )
                        .unwrap();
                    let mut admission = admission();
                    let ProviderMutationOutcome::Acknowledged(ack) = provider.execute_reaction(
                        &reaction_repo("alice"),
                        &request,
                        &mut admission,
                    ) else {
                        panic!("{kind:?} {} add was not acknowledged", content.graphql_name())
                    };
                    assert_eq!(ack.operation_id, operation_id);
                    assert_eq!(ack.reaction_id, "REACTION_new");
                    assert!(ack.present);
                    assert_eq!(ack.target.kind, kind);
                    assert_eq!(reaction_count(&dir), 3);
                    let state = admission.state.lock().unwrap().clone();
                    assert_eq!(state.admitted, 1);
                    assert!(matches!(
                        state.records.as_slice(),
                        [MutationTerminalRecord::Acknowledged { .. }]
                    ));
                }
            }
        }

        #[test]
        fn remove_requires_same_exact_own_id_twice_and_sends_one_mutation() {
            let content = ReactionContent::Heart;
            let target_variables = json!({"id":"REVIEW_COMMENT_node","content":"HEART","after":null});
            let steps = vec![
                reaction_step(
                    "query ReactionTarget(",
                    target_variables.clone(),
                    target_response(
                        ReactableKind::PullRequestReviewComment,
                        content,
                        true,
                        "REACTION_existing",
                    ),
                ),
                reaction_step(
                    "query ReactionTarget(",
                    target_variables,
                    target_response(
                        ReactableKind::PullRequestReviewComment,
                        content,
                        true,
                        "REACTION_existing",
                    ),
                ),
                reaction_step(
                    "mutation RemoveReaction(",
                    json!({"subjectId":"REVIEW_COMMENT_node","content":"HEART","clientMutationId":"remove-heart"}),
                    mutation_response(
                        ReactableKind::PullRequestReviewComment,
                        content,
                        false,
                        "remove-heart",
                        "REACTION_existing",
                    ),
                ),
            ];
            let (dir, provider) = reaction_fixture(steps);
            let request = provider
                .prepare_reaction(
                    &reaction_repo("alice"),
                    7,
                    &displayed_subject(
                        ReactableKind::PullRequestReviewComment,
                        content,
                        true,
                    ),
                    content,
                    ReactionIntent::Remove,
                    "remove-heart".into(),
                    "attempt-remove-heart".into(),
                )
                .unwrap();
            assert!(matches!(
                request.action,
                ReactionAction::Remove { ref existing_reaction_id }
                    if existing_reaction_id == "REACTION_existing"
            ));
            let mut admission = admission();
            let ProviderMutationOutcome::Acknowledged(ack) = provider.execute_reaction(
                &reaction_repo("alice"),
                &request,
                &mut admission,
            ) else {
                panic!("exact removal was not acknowledged")
            };
            assert!(!ack.present);
            assert_eq!(ack.reaction_id, "REACTION_existing");
            assert_eq!(reaction_count(&dir), 3);
        }

        #[test]
        fn changed_remove_id_after_admission_sends_zero_mutations() {
            let content = ReactionContent::Eyes;
            let variables = json!({"id":"ISSUE_COMMENT_node","content":"EYES","after":null});
            let steps = vec![
                reaction_step(
                    "query ReactionTarget(",
                    variables.clone(),
                    target_response(
                        ReactableKind::IssueComment,
                        content,
                        true,
                        "REACTION_old",
                    ),
                ),
                reaction_step(
                    "query ReactionTarget(",
                    variables,
                    target_response(
                        ReactableKind::IssueComment,
                        content,
                        true,
                        "REACTION_replacement",
                    ),
                ),
            ];
            let (dir, provider) = reaction_fixture(steps);
            let request = provider
                .prepare_reaction(
                    &reaction_repo("alice"),
                    7,
                    &displayed_subject(ReactableKind::IssueComment, content, true),
                    content,
                    ReactionIntent::Remove,
                    "remove-eyes".into(),
                    "attempt-remove-eyes".into(),
                )
                .unwrap();
            let mut admission = admission();
            assert!(matches!(
                provider.execute_reaction(&reaction_repo("alice"), &request, &mut admission),
                ProviderMutationOutcome::PreflightRejected { .. }
            ));
            assert_eq!(reaction_count(&dir), 2);
            assert!(matches!(
                admission.state.lock().unwrap().records.as_slice(),
                [MutationTerminalRecord::NotStarted { .. }]
            ));
        }

        #[test]
        fn changed_add_capability_after_admission_sends_zero_mutations() {
            let content = ReactionContent::Rocket;
            let variables = json!({"id":"PR_node","content":"ROCKET","after":null});
            let mut denied =
                target_response(ReactableKind::PullRequest, content, false, "unused");
            denied["data"]["node"]["viewerCanReact"] = json!(false);
            let (dir, provider) = reaction_fixture(vec![
                reaction_step(
                    "query ReactionTarget(",
                    variables.clone(),
                    target_response(ReactableKind::PullRequest, content, false, "unused"),
                ),
                reaction_step("query ReactionTarget(", variables, denied),
            ]);
            let request = provider
                .prepare_reaction(
                    &reaction_repo("alice"),
                    7,
                    &displayed_subject(ReactableKind::PullRequest, content, false),
                    content,
                    ReactionIntent::Add,
                    "changed-capability".into(),
                    "changed-capability-attempt".into(),
                )
                .unwrap();
            let mut admission = admission();
            assert!(matches!(
                provider.execute_reaction(&reaction_repo("alice"), &request, &mut admission),
                ProviderMutationOutcome::PreflightRejected { .. }
            ));
            assert_eq!(reaction_count(&dir), 2);
            assert!(matches!(
                admission.state.lock().unwrap().records.as_slice(),
                [MutationTerminalRecord::NotStarted { .. }]
            ));
        }

        #[test]
        fn unavailable_ambiguous_and_already_desired_states_send_zero_mutations() {
            let content = ReactionContent::ThumbsDown;
            let (dir, provider) = reaction_fixture(Vec::new());

            let mut denied = displayed_subject(ReactableKind::PullRequest, content, false);
            denied.fresh_capability.as_mut().unwrap().viewer_can_react = false;
            assert!(provider
                .prepare_reaction(
                    &reaction_repo("alice"),
                    7,
                    &denied,
                    content,
                    ReactionIntent::Add,
                    "denied".into(),
                    "denied-attempt".into(),
                )
                .is_err());
            let mut partial = displayed_subject(ReactableKind::PullRequest, content, false);
            partial.reactions.complete = false;
            assert!(provider
                .prepare_reaction(
                    &reaction_repo("alice"),
                    7,
                    &partial,
                    content,
                    ReactionIntent::Add,
                    "partial".into(),
                    "partial-attempt".into(),
                )
                .is_err());
            assert!(provider
                .prepare_reaction(
                    &reaction_repo("alice"),
                    7,
                    &displayed_subject(ReactableKind::PullRequest, content, true),
                    content,
                    ReactionIntent::Add,
                    "already-present".into(),
                    "already-present-attempt".into(),
                )
                .is_err());
            assert!(provider
                .prepare_reaction(
                    &reaction_repo("alice"),
                    7,
                    &displayed_subject(ReactableKind::PullRequest, content, false),
                    content,
                    ReactionIntent::Remove,
                    "already-absent".into(),
                    "already-absent-attempt".into(),
                )
                .is_err());
            assert!(provider
                .prepare_reaction(
                    &reaction_repo("bob"),
                    7,
                    &displayed_subject(ReactableKind::PullRequest, content, false),
                    content,
                    ReactionIntent::Add,
                    "wrong-account".into(),
                    "wrong-account-attempt".into(),
                )
                .is_err());
            assert_eq!(reaction_count(&dir), 0);

            let variables = json!({"id":"PR_node","content":"THUMBS_DOWN","after":null});
            let mut capability_false = target_response(
                ReactableKind::PullRequest,
                content,
                false,
                "unused",
            );
            capability_false["data"]["node"]["viewerCanReact"] = json!(false);
            let (dir, provider) = reaction_fixture(vec![reaction_step(
                "query ReactionTarget(",
                variables,
                capability_false,
            )]);
            assert!(provider
                .prepare_reaction(
                    &reaction_repo("alice"),
                    7,
                    &displayed_subject(ReactableKind::PullRequest, content, false),
                    content,
                    ReactionIntent::Add,
                    "target-denied".into(),
                    "target-denied-attempt".into(),
                )
                .is_err());
            assert_eq!(reaction_count(&dir), 1);
        }

        #[test]
        fn displayed_capability_false_rejects_before_targeted_read() {
            let content = ReactionContent::ThumbsDown;
            let variables = json!({"id":"PR_node","content":"THUMBS_DOWN","after":null});
            let (dir, provider) = reaction_fixture(vec![reaction_step(
                "query ReactionTarget(",
                variables,
                target_response(
                    ReactableKind::PullRequest,
                    content,
                    false,
                    "unused",
                ),
            )]);
            let mut denied = displayed_subject(ReactableKind::PullRequest, content, false);
            denied.fresh_capability.as_mut().unwrap().viewer_can_react = false;
            assert!(provider
                .prepare_reaction(
                    &reaction_repo("alice"),
                    7,
                    &denied,
                    content,
                    ReactionIntent::Add,
                    "displayed-denied".into(),
                    "displayed-denied-attempt".into(),
                )
                .is_err());
            assert_eq!(reaction_count(&dir), 0);
        }

        #[test]
        fn targeted_identity_partial_and_pagination_evidence_fail_closed() {
            let content = ReactionContent::Hooray;
            let variables = json!({"id":"ISSUE_COMMENT_node","content":"HOORAY","after":null});
            for defect in [
                "viewer-node",
                "viewer-login",
                "id",
                "body",
                "type",
                "parent",
                "repository",
                "partial",
                "duplicate-own",
            ] {
                let mut response = target_response(
                    ReactableKind::IssueComment,
                    content,
                    true,
                    "REACTION_own",
                );
                match defect {
                    "viewer-node" => response["data"]["viewer"]["id"] = json!("USER_other"),
                    "viewer-login" => response["data"]["viewer"]["login"] = json!("bob"),
                    "id" => response["data"]["node"]["id"] = json!("OTHER_SUBJECT"),
                    "body" => response["data"]["node"]["body"] = json!("changed body"),
                    "type" => {
                        response["data"]["node"]["__typename"] =
                            json!("PullRequestReview")
                    }
                    "parent" => {
                        response["data"]["node"]["pullRequest"]["number"] = json!(8)
                    }
                    "repository" => {
                        response["data"]["node"]["pullRequest"]["repository"]
                            ["nameWithOwner"] = json!("other/repo")
                    }
                    "partial" => {
                        response["errors"] = json!([{"message":"withheld reaction field"}])
                    }
                    "duplicate-own" => {
                        let duplicate = response["data"]["node"]["reactions"]["nodes"][0]
                            .clone();
                        response["data"]["node"]["reactions"]["nodes"] =
                            json!([duplicate.clone(), duplicate]);
                    }
                    _ => unreachable!(),
                }
                let (dir, provider) = reaction_fixture(vec![reaction_step(
                    "query ReactionTarget(",
                    variables.clone(),
                    response,
                )]);
                assert!(provider
                    .prepare_reaction(
                        &reaction_repo("alice"),
                        7,
                        &displayed_subject(ReactableKind::IssueComment, content, true),
                        content,
                        ReactionIntent::Remove,
                        format!("defect-{defect}"),
                        format!("defect-{defect}-attempt"),
                    )
                    .is_err());
                assert_eq!(reaction_count(&dir), 1, "{defect}");
            }

            let mut first = target_response(
                ReactableKind::IssueComment,
                content,
                true,
                "REACTION_foreign",
            );
            first["data"]["node"]["reactions"]["nodes"][0]["user"] =
                json!({"id":"USER_bob","login":"bob"});
            first["data"]["node"]["reactions"]["pageInfo"] =
                json!({"hasNextPage":true,"endCursor":"next-own"});
            let second = target_response(
                ReactableKind::IssueComment,
                content,
                true,
                "REACTION_own",
            );
            let (dir, provider) = reaction_fixture(vec![
                reaction_step("query ReactionTarget(", variables, first),
                reaction_step(
                    "query ReactionTarget(",
                    json!({"id":"ISSUE_COMMENT_node","content":"HOORAY","after":"next-own"}),
                    second,
                ),
            ]);
            let request = provider
                .prepare_reaction(
                    &reaction_repo("alice"),
                    7,
                    &displayed_subject(ReactableKind::IssueComment, content, true),
                    content,
                    ReactionIntent::Remove,
                    "paged-remove".into(),
                    "paged-remove-attempt".into(),
                )
                .unwrap();
            assert!(matches!(
                request.action,
                ReactionAction::Remove { existing_reaction_id } if existing_reaction_id == "REACTION_own"
            ));
            assert_eq!(reaction_count(&dir), 2);
        }

        #[test]
        fn cached_capability_and_failed_durable_admission_send_zero_mutations() {
            let mut cached = displayed_subject(
                ReactableKind::PullRequest,
                ReactionContent::ThumbsUp,
                false,
            );
            cached.fresh_capability = None;
            let (dir, provider) = reaction_fixture(Vec::new());
            assert!(provider
                .prepare_reaction(
                    &reaction_repo("alice"),
                    7,
                    &cached,
                    ReactionContent::ThumbsUp,
                    ReactionIntent::Add,
                    "cached-add".into(),
                    "cached-attempt".into(),
                )
                .is_err());
            assert_eq!(reaction_count(&dir), 0);

            let content = ReactionContent::Rocket;
            let variables = json!({"id":"PR_node","content":"ROCKET","after":null});
            let (dir, provider) = reaction_fixture(vec![reaction_step(
                "query ReactionTarget(",
                variables,
                target_response(ReactableKind::PullRequest, content, false, "unused"),
            )]);
            let request = provider
                .prepare_reaction(
                    &reaction_repo("alice"),
                    7,
                    &displayed_subject(ReactableKind::PullRequest, content, false),
                    content,
                    ReactionIntent::Add,
                    "admission-fail".into(),
                    "admission-fail-attempt".into(),
                )
                .unwrap();
            let mut admission = admission();
            admission.fail_admit = true;
            assert!(matches!(
                provider.execute_reaction(&reaction_repo("alice"), &request, &mut admission),
                ProviderMutationOutcome::PreflightRejected { .. }
            ));
            assert_eq!(reaction_count(&dir), 1);
        }

        #[test]
        fn wrong_complete_ack_is_uncertain_after_one_mutation_and_terminal_failure_stays_uncertain() {
            for fail_terminal in [false, true] {
                let content = ReactionContent::Laugh;
                let variables = json!({"id":"REVIEW_node","content":"LAUGH","after":null});
                let mut wrong = mutation_response(
                    ReactableKind::PullRequestReview,
                    content,
                    true,
                    "add-laugh",
                    "REACTION_new",
                );
                if !fail_terminal {
                    wrong["data"]["addReaction"]["reaction"]["user"]["id"] =
                        json!("USER_other");
                }
                let steps = vec![
                    reaction_step(
                        "query ReactionTarget(",
                        variables.clone(),
                        target_response(
                            ReactableKind::PullRequestReview,
                            content,
                            false,
                            "unused",
                        ),
                    ),
                    reaction_step(
                        "query ReactionTarget(",
                        variables,
                        target_response(
                            ReactableKind::PullRequestReview,
                            content,
                            false,
                            "unused",
                        ),
                    ),
                    reaction_step(
                        "mutation AddReaction(",
                        json!({"subjectId":"REVIEW_node","content":"LAUGH","clientMutationId":"add-laugh"}),
                        wrong,
                    ),
                ];
                let (dir, provider) = reaction_fixture(steps);
                let request = provider
                    .prepare_reaction(
                        &reaction_repo("alice"),
                        7,
                        &displayed_subject(
                            ReactableKind::PullRequestReview,
                            content,
                            false,
                        ),
                        content,
                        ReactionIntent::Add,
                        "add-laugh".into(),
                        format!("attempt-{fail_terminal}"),
                    )
                    .unwrap();
                let mut admission = admission();
                admission.fail_terminal = fail_terminal;
                assert!(matches!(
                    provider.execute_reaction(&reaction_repo("alice"), &request, &mut admission),
                    ProviderMutationOutcome::Uncertain { .. }
                ));
                assert_eq!(reaction_count(&dir), 3);
            }
        }

        #[test]
        fn every_wrong_or_partial_ack_and_lost_transport_is_uncertain_without_retry() {
            let content = ReactionContent::Confused;
            for defect in [
                "operation",
                "reaction-id",
                "content",
                "viewer",
                "reactable",
                "subject",
                "parent",
                "final-state",
                "graphql-partial",
                "lost-transport",
            ] {
                let variables =
                    json!({"id":"REVIEW_COMMENT_node","content":"CONFUSED","after":null});
                let mut ack = mutation_response(
                    ReactableKind::PullRequestReviewComment,
                    content,
                    true,
                    "ack-matrix",
                    "REACTION_new",
                );
                match defect {
                    "operation" => {
                        ack["data"]["addReaction"]["clientMutationId"] = json!("other")
                    }
                    "reaction-id" => {
                        ack["data"]["addReaction"]["reaction"]["id"] = json!("")
                    }
                    "content" => {
                        ack["data"]["addReaction"]["reaction"]["content"] = json!("HEART")
                    }
                    "viewer" => {
                        ack["data"]["addReaction"]["reaction"]["user"]["login"] =
                            json!("bob")
                    }
                    "reactable" => {
                        ack["data"]["addReaction"]["reaction"]["reactable"]["id"] =
                            json!("OTHER")
                    }
                    "subject" => {
                        ack["data"]["addReaction"]["subject"]["id"] = json!("OTHER")
                    }
                    "parent" => {
                        ack["data"]["addReaction"]["subject"]["pullRequestReview"]["id"] =
                            json!("OTHER_REVIEW")
                    }
                    "final-state" => {
                        ack["data"]["addReaction"]["subject"]["reactions"]
                            ["viewerHasReacted"] = json!(false)
                    }
                    "graphql-partial" => {
                        ack["errors"] = json!([{"message":"post-dispatch partial response"}])
                    }
                    "lost-transport" => {}
                    _ => unreachable!(),
                }
                let mutation = if defect == "lost-transport" {
                    json!({
                        "marker":"mutation AddReaction(",
                        "variables":{"subjectId":"REVIEW_COMMENT_node","content":"CONFUSED","clientMutationId":"ack-matrix"},
                        "response":ack,
                        "fail":true
                    })
                } else {
                    reaction_step(
                        "mutation AddReaction(",
                        json!({"subjectId":"REVIEW_COMMENT_node","content":"CONFUSED","clientMutationId":"ack-matrix"}),
                        ack,
                    )
                };
                let steps = vec![
                    reaction_step(
                        "query ReactionTarget(",
                        variables.clone(),
                        target_response(
                            ReactableKind::PullRequestReviewComment,
                            content,
                            false,
                            "unused",
                        ),
                    ),
                    reaction_step(
                        "query ReactionTarget(",
                        variables,
                        target_response(
                            ReactableKind::PullRequestReviewComment,
                            content,
                            false,
                            "unused",
                        ),
                    ),
                    mutation,
                ];
                let (dir, provider) = reaction_fixture(steps);
                let request = provider
                    .prepare_reaction(
                        &reaction_repo("alice"),
                        7,
                        &displayed_subject(
                            ReactableKind::PullRequestReviewComment,
                            content,
                            false,
                        ),
                        content,
                        ReactionIntent::Add,
                        "ack-matrix".into(),
                        format!("ack-matrix-{defect}"),
                    )
                    .unwrap();
                let mut admission = admission();
                assert!(matches!(
                    provider.execute_reaction(&reaction_repo("alice"), &request, &mut admission),
                    ProviderMutationOutcome::Uncertain { .. }
                ));
                assert_eq!(reaction_count(&dir), 3, "{defect}");
                assert!(matches!(
                    admission.state.lock().unwrap().records.as_slice(),
                    [MutationTerminalRecord::Uncertain { .. }]
                ));
            }
        }

        #[test]
        fn remove_race_returning_a_replacement_id_is_uncertain_after_one_write() {
            let content = ReactionContent::Heart;
            let variables = json!({"id":"REVIEW_node","content":"HEART","after":null});
            let steps = vec![
                reaction_step(
                    "query ReactionTarget(",
                    variables.clone(),
                    target_response(
                        ReactableKind::PullRequestReview,
                        content,
                        true,
                        "REACTION_old",
                    ),
                ),
                reaction_step(
                    "query ReactionTarget(",
                    variables,
                    target_response(
                        ReactableKind::PullRequestReview,
                        content,
                        true,
                        "REACTION_old",
                    ),
                ),
                reaction_step(
                    "mutation RemoveReaction(",
                    json!({"subjectId":"REVIEW_node","content":"HEART","clientMutationId":"remove-race"}),
                    mutation_response(
                        ReactableKind::PullRequestReview,
                        content,
                        false,
                        "remove-race",
                        "REACTION_replacement",
                    ),
                ),
            ];
            let (dir, provider) = reaction_fixture(steps);
            let request = provider
                .prepare_reaction(
                    &reaction_repo("alice"),
                    7,
                    &displayed_subject(ReactableKind::PullRequestReview, content, true),
                    content,
                    ReactionIntent::Remove,
                    "remove-race".into(),
                    "remove-race-attempt".into(),
                )
                .unwrap();
            let mut admission = admission();
            assert!(matches!(
                provider.execute_reaction(&reaction_repo("alice"), &request, &mut admission),
                ProviderMutationOutcome::Uncertain { .. }
            ));
            assert_eq!(reaction_count(&dir), 3);
        }
    };
}
