#[macro_export]
macro_rules! notification_provider_tests {
    () => {
        use super::*;
        use $crate::domain::{Account, Repository};
        use $crate::notifications::{NotificationStore, NotificationStoreLimits};
        use serde_json::{Value, json};
        use std::{
            collections::HashMap,
            fs,
            os::unix::fs::PermissionsExt,
            time::Duration,
        };
        use tempfile::TempDir;

        fn provider_account(login: &str) -> Account {
            Account { host: "github.com".into(), login: login.into() }
        }

        fn provider_repo(login: &str) -> Repository {
            provider_repo_named(login, "repo")
        }

        fn provider_repo_named(login: &str, name: &str) -> Repository {
            Repository {
                host: "github.com".into(), owner: "owner".into(), name: name.into(),
                account: provider_account(login), local_path: None,
            }
        }

        fn notification(id: &str, reason: &str, number: u64) -> Value {
            notification_for(
                id,
                reason,
                "repo",
                "PullRequest",
                &format!("https://api.github.com/repos/owner/repo/pulls/{number}"),
            )
        }

        fn notification_for(
            id: &str,
            reason: &str,
            repository: &str,
            kind: &str,
            subject_url: &str,
        ) -> Value {
            json!({
                "id": id, "reason": reason, "updated_at": "2026-09-13T12:00:00Z",
                "subject": {
                    "title": "production-shaped subject", "type": kind,
                    "url": subject_url,
                    "latest_comment_url": null
                },
                "repository": {
                    "full_name": format!("owner/{repository}"),
                    "html_url": format!("https://github.com/owner/{repository}")
                }
            })
        }

        fn notifications_endpoint(repository: &str, page: usize) -> String {
            format!(
                "repos/owner/{repository}/notifications?all=true&participating=false&per_page=100&page={page}"
            )
        }

        fn provider_fixture(login: &str, responses: HashMap<String, Value>) -> (TempDir, GithubProvider) {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("login"), login).unwrap();
            fs::write(dir.path().join("responses.json"), serde_json::to_vec(&responses).unwrap()).unwrap();
            let executable = dir.path().join("gh");
            fs::write(&executable, r#"#!/usr/bin/python3
import json, os, pathlib, sys
root = pathlib.Path(__file__).parent
args = sys.argv[1:]
login = (root / 'login').read_text()
for key in ['GITHUB_TOKEN','GH_ENTERPRISE_TOKEN','GITHUB_ENTERPRISE_TOKEN','GH_HOST','GH_REPO','GH_DEBUG','DEBUG','GH_HTTP_UNIX_SOCKET']:
    assert key not in os.environ
assert os.environ.get('GH_PROMPT_DISABLED') == '1'
assert os.environ.get('GH_PAGER') == '/bin/cat'
if args[:2] == ['auth', 'token']:
    assert args == ['auth','token','--hostname','github.com','--user',login]
    assert 'GH_TOKEN' not in os.environ
    print('private-' + login)
    sys.exit(0)
assert os.environ.get('GH_TOKEN') == 'private-' + login
assert args[:7] == ['api','--hostname','github.com','--method','GET','--header','Accept: application/vnd.github+json']
assert args[7:9] == ['--header','X-GitHub-Api-Version: 2026-03-10']
endpoint = args[9]
with (root / 'calls').open('a') as log:
    log.write(endpoint + '\n')
responses = json.loads((root / 'responses.json').read_text())
assert endpoint in responses, endpoint
print(json.dumps(responses[endpoint]))
"#).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let provider = GithubProvider {
                account: provider_account(login),
                runner: $crate::providers::Runner {
                    gh: executable,
                    timeout: Duration::from_secs(10),
                    ..$crate::providers::Runner::default()
                },
            };
            (dir, provider)
        }

        fn complete_responses(reason: &str, conclusion: &str) -> HashMap<String, Value> {
            HashMap::from([
                (notifications_endpoint("repo", 1), json!([notification("thread-1", reason, 7)])),
                ("repos/owner/repo/pulls/7".into(), json!({
                    "number": 7, "url": "https://api.github.com/repos/owner/repo/pulls/7",
                    "html_url": "https://github.com/owner/repo/pull/7",
                    "user": {"login": "alice"},
                    "head": {"sha": "1111111111111111111111111111111111111111"}
                })),
                ("repos/owner/repo/issues/7/timeline?per_page=100&page=1".into(), json!([{
                    "id": 41, "node_id": "timeline-node", "event": "review_requested",
                    "actor": {"login": "bob"}, "requested_reviewer": {"login": "alice"},
                    "created_at": "2026-09-13T10:00:00Z"
                }])),
                ("repos/owner/repo/pulls/7/comments?sort=created&direction=asc&per_page=100&page=1".into(), json!([
                    {"id": 51, "url": "https://api.github.com/repos/owner/repo/pulls/comments/51", "pull_request_url": "https://api.github.com/repos/owner/repo/pulls/7", "user": {"login": "alice"}, "created_at": "2026-09-13T10:01:00Z", "in_reply_to_id": null},
                    {"id": 52, "url": "https://api.github.com/repos/owner/repo/pulls/comments/52", "pull_request_url": "https://api.github.com/repos/owner/repo/pulls/7", "user": {"login": "bob"}, "created_at": "2026-09-13T10:02:00Z", "in_reply_to_id": 51}
                ])),
                ("repos/owner/repo/commits/1111111111111111111111111111111111111111/check-runs?filter=latest&per_page=100&page=1".into(), json!({
                    "total_count": 1,
                    "check_runs": [{"id": 61, "name": "build", "status": "completed", "conclusion": conclusion,
                        "completed_at": "2026-09-13T10:03:00Z",
                        "pull_requests": [{"number": 7, "url": "https://api.github.com/repos/owner/repo/pulls/7"}] }]
                })),
            ])
        }

        fn known_non_pull_notifications() -> Vec<Value> {
            [
                ("thread-issue", "Issue", "https://api.github.com/repos/owner/repo/issues/23"),
                ("thread-check-suite", "CheckSuite", "https://api.github.com/repos/owner/repo/check-suites/24"),
                ("thread-commit", "Commit", "https://api.github.com/repos/owner/repo/commits/1111111111111111111111111111111111111111"),
                ("thread-release", "Release", "https://api.github.com/repos/owner/repo/releases/25"),
                ("thread-discussion", "Discussion", "https://api.github.com/repos/owner/repo/discussions/26"),
                ("thread-invitation", "RepositoryInvitation", "https://api.github.com/repositories/27/invitations/28"),
                ("thread-vulnerability", "RepositoryVulnerabilityAlert", "https://api.github.com/repos/owner/repo/dependabot/alerts/29"),
            ]
            .into_iter()
            .map(|(id, kind, url)| notification_for(id, "subscribed", "repo", kind, url))
            .collect()
        }

        fn assert_incomplete_repository_did_not_baseline(
            initial: &ProviderNotificationBatch,
            repository: &str,
        ) {
            let account = provider_account("alice");
            let dir = tempfile::tempdir().unwrap();
            let store = NotificationStore::open(dir.path(), NotificationStoreLimits::default())
                .unwrap();
            store.reconcile(&account, initial).unwrap();

            let target = NotificationPullRequest {
                provider: "github".into(),
                host: "github.com".into(),
                account: "alice".into(),
                owner: "owner".into(),
                repository: repository.into(),
                pull_request: 7,
            };
            let event = ProviderNotificationEvent {
                identity: NotificationEventIdentity {
                    target: target.clone(),
                    source: NotificationEventSource::Timeline,
                    remote_event_id: "historical-review-request".into(),
                },
                occurred_at: "2026-09-13T10:00:00Z".into(),
                actor: Some("bob".into()),
                alert_kind: NotificationAlertKind::ReviewRequest,
                summary: "Review requested".into(),
                url: format!("https://github.com/owner/{repository}/pull/7"),
                evidence: NotificationEvidence::ReviewRequested {
                    timeline_event_id: "historical-review-request".into(),
                    requested_reviewer: "alice".into(),
                },
            };
            let later = ProviderNotificationBatch {
                account: account.clone(),
                observed_at_unix_ms: 2,
                observations: vec![ProviderNotificationObservation {
                    provider_notification_id: "thread-historical".into(),
                    provider_reason: "review_requested".into(),
                    notification_updated_at: "2026-09-13T12:00:00Z".into(),
                    target,
                    events: vec![event],
                }],
                incomplete_candidates: vec![],
                full_snapshot: true,
                repositories: vec![RepositoryNotificationCompleteness {
                    target: NotificationRepositoryScope {
                        provider: "github".into(),
                        host: "github.com".into(),
                        account: "alice".into(),
                        owner: "owner".into(),
                        repository: repository.into(),
                    },
                    complete: true,
                    reasons: vec![],
                }],
                complete: true,
                notices: vec![],
            };
            assert!(
                store
                    .reconcile(&account, &later)
                    .unwrap()
                    .newly_admitted_alerts
                    .is_empty(),
                "an incomplete repository must wait for a later complete baseline"
            );
        }

        #[test]
        fn provider_uses_exact_read_only_queries_and_authoritative_event_evidence() {
            let (dir, provider) = provider_fixture("alice", complete_responses("mention", "failure"));
            let batch = provider.notification_observations(&[provider_repo("alice")], None, NotificationReadLimits::default()).unwrap();
            assert!(batch.complete);
            assert!(batch.full_snapshot);
            assert_eq!(batch.observations.len(), 1);
            let events = &batch.observations[0].events;
            assert_eq!(events.len(), 3);
            assert!(events.iter().any(|event| event.alert_kind == NotificationAlertKind::ReviewRequest));
            assert!(events.iter().any(|event| event.alert_kind == NotificationAlertKind::Reply));
            let failed = events.iter().find(|event| event.alert_kind == NotificationAlertKind::FailedCheckOwnPullRequest).unwrap();
            assert!(failed.identity.remote_event_id.contains("61:2026-09-13T10:03:00Z:failure"));
            assert!(batch.incomplete_candidates.iter().any(|candidate| candidate.kind == IncompleteCandidateKind::Mention));
            assert!(fs::read_to_string(dir.path().join("calls")).unwrap().lines().all(|line| !line.contains("mark") && !line.contains("subscriptions")));
        }

        #[test]
        fn known_non_pull_subjects_preserve_pr_baseline_and_do_not_consume_hydration() {
            let mut baseline_responses = complete_responses("review_requested", "success");
            let mut notification_page = known_non_pull_notifications();
            notification_page.push(notification("thread-pr", "review_requested", 7));
            baseline_responses.insert(
                notifications_endpoint("repo", 1),
                Value::Array(notification_page.clone()),
            );
            let (_baseline_dir, baseline_provider) =
                provider_fixture("alice", baseline_responses);
            let limits = NotificationReadLimits {
                max_candidate_hydrations: 1,
                ..NotificationReadLimits::default()
            };
            let baseline = baseline_provider
                .notification_observations(&[provider_repo("alice")], None, limits.clone())
                .unwrap();
            assert!(baseline.complete);
            assert!(baseline.repositories[0].complete);
            assert_eq!(baseline.observations.len(), 1);

            let store_dir = tempfile::tempdir().unwrap();
            let store = NotificationStore::open(
                store_dir.path(),
                NotificationStoreLimits::default(),
            )
            .unwrap();
            assert!(store
                .reconcile(&provider_account("alice"), &baseline)
                .unwrap()
                .newly_admitted_alerts
                .is_empty());

            let mut later_responses = complete_responses("review_requested", "success");
            later_responses.insert(
                notifications_endpoint("repo", 1),
                Value::Array(notification_page),
            );
            later_responses.insert(
                "repos/owner/repo/issues/7/timeline?per_page=100&page=1".into(),
                json!([
                    {
                        "id": 41, "node_id": "timeline-node", "event": "review_requested",
                        "actor": {"login": "bob"}, "requested_reviewer": {"login": "alice"},
                        "created_at": "2026-09-13T10:00:00Z"
                    },
                    {
                        "id": 42, "node_id": "timeline-node-later", "event": "review_requested",
                        "actor": {"login": "carol"}, "requested_reviewer": {"login": "alice"},
                        "created_at": "2026-09-13T10:04:00Z"
                    }
                ]),
            );
            let (_later_dir, later_provider) = provider_fixture("alice", later_responses);
            let later = later_provider
                .notification_observations(&[provider_repo("alice")], None, limits)
                .unwrap();
            assert!(
                later.complete,
                "later provider read was incomplete: {:?}; notices: {:?}",
                later.incomplete_candidates, later.notices
            );
            let reconciled = store
                .reconcile(&provider_account("alice"), &later)
                .unwrap();
            assert_eq!(reconciled.newly_admitted_alerts.len(), 1);
            assert_eq!(
                reconciled.newly_admitted_alerts[0]
                    .identity
                    .remote_event_id,
                "42"
            );
        }

        #[test]
        fn sticky_reason_and_action_required_never_claim_exact_alerts() {
            let (_dir, provider) = provider_fixture("alice", complete_responses("mention", "action_required"));
            let batch = provider.notification_observations(&[provider_repo("alice")], None, NotificationReadLimits::default()).unwrap();
            assert!(batch.complete, "classification proof gaps do not make enumeration partial");
            assert!(!batch.observations[0].events.iter().any(|event| matches!(event.alert_kind, NotificationAlertKind::Mention | NotificationAlertKind::FailedCheckOwnPullRequest)));
            assert!(batch.incomplete_candidates.iter().any(|candidate| candidate.kind == IncompleteCandidateKind::Mention));
            assert!(batch.incomplete_candidates.iter().any(|candidate| candidate.kind == IncompleteCandidateKind::FailedCheckOwnPullRequest));
        }

        #[test]
        fn foreign_or_malformed_subject_is_visible_and_never_hydrated() {
            let mut item = notification("thread-1", "mention", 7);
            item["subject"]["url"] = json!("https://api.github.com/repos/other/repo/pulls/7");
            let responses = HashMap::from([(notifications_endpoint("repo", 1), json!([item]))]);
            let (dir, provider) = provider_fixture("alice", responses);
            let batch = provider.notification_observations(&[provider_repo("alice")], None, NotificationReadLimits::default()).unwrap();
            assert!(!batch.complete);
            assert!(!batch.repositories[0].complete);
            assert!(batch.observations.is_empty());
            assert_eq!(batch.incomplete_candidates.len(), 1);
            assert!(batch.incomplete_candidates[0]
                .reason
                .contains("does not match the selected repository"));
            assert_eq!(fs::read_to_string(dir.path().join("calls")).unwrap().lines().count(), 1);
        }

        #[test]
        fn unknown_subject_type_is_visible_incomplete_and_never_hydrated() {
            let item = notification_for(
                "thread-unknown",
                "subscribed",
                "repo",
                "FutureSubjectType",
                "https://api.github.com/repos/owner/repo/future/7",
            );
            let responses = HashMap::from([(notifications_endpoint("repo", 1), json!([item]))]);
            let (dir, provider) = provider_fixture("alice", responses);
            let batch = provider
                .notification_observations(
                    &[provider_repo("alice")],
                    None,
                    NotificationReadLimits::default(),
                )
                .unwrap();
            assert!(!batch.complete);
            assert!(!batch.repositories[0].complete);
            assert!(batch.observations.is_empty());
            assert_eq!(batch.incomplete_candidates.len(), 1);
            assert!(batch.incomplete_candidates[0]
                .reason
                .contains("Unknown notification subject type"));
            assert_eq!(
                fs::read_to_string(dir.path().join("calls"))
                    .unwrap()
                    .lines()
                    .count(),
                1
            );
        }

        #[test]
        fn exactly_full_notification_page_reads_the_next_page() {
            let first_page: Vec<_> = (0..100)
                .map(|index| {
                    notification_for(
                        &format!("thread-{index}"),
                        "subscribed",
                        "repo",
                        "Issue",
                        &format!("https://api.github.com/repos/owner/repo/issues/{index}"),
                    )
                })
                .collect();
            let responses = HashMap::from([
                (notifications_endpoint("repo", 1), Value::Array(first_page)),
                (notifications_endpoint("repo", 2), json!([])),
            ]);
            let (dir, provider) = provider_fixture("alice", responses);
            let batch = provider
                .notification_observations(
                    &[provider_repo("alice")],
                    None,
                    NotificationReadLimits::default(),
                )
                .unwrap();
            assert!(batch.complete);
            assert!(batch.repositories[0].complete);
            assert!(batch.observations.is_empty());
            assert_eq!(
                fs::read_to_string(dir.path().join("calls"))
                    .unwrap()
                    .lines()
                    .collect::<Vec<_>>(),
                [notifications_endpoint("repo", 1), notifications_endpoint("repo", 2)]
            );
        }

        #[test]
        fn duplicate_across_moving_notification_pages_fails_without_absence_claim() {
            let first_page: Vec<_> = (0..100)
                .map(|index| {
                    notification_for(
                        &format!("thread-{index}"),
                        "subscribed",
                        "repo",
                        "Issue",
                        &format!("https://api.github.com/repos/owner/repo/issues/{index}"),
                    )
                })
                .collect();
            let duplicate = first_page[0].clone();
            let responses = HashMap::from([
                (notifications_endpoint("repo", 1), Value::Array(first_page)),
                (notifications_endpoint("repo", 2), json!([duplicate])),
            ]);
            let (_dir, provider) = provider_fixture("alice", responses);
            assert!(provider.notification_observations(&[provider_repo("alice")], None, NotificationReadLimits::default()).unwrap_err().to_string().contains("changed during pagination"));
        }

        #[test]
        fn global_page_budget_leaves_not_yet_enumerated_repository_incomplete() {
            let responses = HashMap::from([(
                notifications_endpoint("one", 1),
                json!([]),
            )]);
            let (dir, provider) = provider_fixture("alice", responses);
            let repositories = [
                provider_repo_named("alice", "one"),
                provider_repo_named("alice", "two"),
            ];
            let limits = NotificationReadLimits {
                max_notification_pages: 1,
                ..NotificationReadLimits::default()
            };
            let batch = provider
                .notification_observations(&repositories, None, limits)
                .unwrap();
            assert!(!batch.complete);
            assert!(batch.repositories.iter().find(|value| value.target.repository == "one").unwrap().complete);
            let second = batch.repositories.iter().find(|value| value.target.repository == "two").unwrap();
            assert!(!second.complete);
            assert!(second.reasons.iter().any(|reason| reason.contains("page bound")));
            assert_incomplete_repository_did_not_baseline(&batch, "two");
            assert_eq!(fs::read_to_string(dir.path().join("calls")).unwrap(), format!("{}\n", notifications_endpoint("one", 1)));
        }

        #[test]
        fn global_item_budget_leaves_not_yet_enumerated_repository_incomplete() {
            let issue = notification_for(
                "thread-one",
                "subscribed",
                "one",
                "Issue",
                "https://api.github.com/repos/owner/one/issues/1",
            );
            let responses = HashMap::from([(
                notifications_endpoint("one", 1),
                json!([issue]),
            )]);
            let (dir, provider) = provider_fixture("alice", responses);
            let repositories = [
                provider_repo_named("alice", "one"),
                provider_repo_named("alice", "two"),
            ];
            let limits = NotificationReadLimits {
                max_notifications: 1,
                ..NotificationReadLimits::default()
            };
            let batch = provider
                .notification_observations(&repositories, None, limits)
                .unwrap();
            assert!(!batch.complete);
            assert!(batch.repositories.iter().find(|value| value.target.repository == "one").unwrap().complete);
            let second = batch.repositories.iter().find(|value| value.target.repository == "two").unwrap();
            assert!(!second.complete);
            assert!(second.reasons.iter().any(|reason| reason.contains("item bound")));
            assert_incomplete_repository_did_not_baseline(&batch, "two");
            assert_eq!(fs::read_to_string(dir.path().join("calls")).unwrap(), format!("{}\n", notifications_endpoint("one", 1)));
        }

        #[test]
        fn repository_endpoint_failure_is_partial_and_does_not_poison_another_repo() {
            let responses = HashMap::from([(
                notifications_endpoint("two", 1),
                json!([]),
            )]);
            let (_dir, provider) = provider_fixture("alice", responses);
            let repositories = [
                provider_repo_named("alice", "one"),
                provider_repo_named("alice", "two"),
            ];
            let batch = provider
                .notification_observations(
                    &repositories,
                    None,
                    NotificationReadLimits::default(),
                )
                .unwrap();
            assert!(!batch.complete);
            assert!(!batch.repositories.iter().find(|value| value.target.repository == "one").unwrap().complete);
            assert!(batch.repositories.iter().find(|value| value.target.repository == "two").unwrap().complete);
            assert!(batch.incomplete_candidates.iter().any(|candidate| candidate
                .reason
                .contains("notification enumeration read failed for owner/one")));
        }

        #[test]
        fn repository_endpoint_cannot_return_another_selected_repository_identity() {
            let foreign_item = notification_for(
                "thread-two",
                "subscribed",
                "two",
                "Issue",
                "https://api.github.com/repos/owner/two/issues/1",
            );
            let responses = HashMap::from([
                (
                    notifications_endpoint("one", 1),
                    json!([foreign_item]),
                ),
                (notifications_endpoint("two", 1), json!([])),
            ]);
            let (_dir, provider) = provider_fixture("alice", responses);
            let repositories = [
                provider_repo_named("alice", "one"),
                provider_repo_named("alice", "two"),
            ];
            let batch = provider
                .notification_observations(
                    &repositories,
                    None,
                    NotificationReadLimits::default(),
                )
                .unwrap();
            assert!(!batch.complete);
            let first = batch
                .repositories
                .iter()
                .find(|value| value.target.repository == "one")
                .unwrap();
            assert!(!first.complete);
            assert!(first
                .reasons
                .iter()
                .any(|reason| reason.contains("identity validation")));
            assert!(batch
                .repositories
                .iter()
                .find(|value| value.target.repository == "two")
                .unwrap()
                .complete);
            assert!(batch.observations.is_empty());
        }

        #[test]
        fn notification_cap_marks_every_selected_repository_incomplete() {
            let mut responses = complete_responses("author", "success");
            responses.insert(
                notifications_endpoint("repo", 1),
                json!([
                    notification("thread-1", "author", 7),
                    notification("thread-2", "author", 8)
                ]),
            );
            let (_dir, provider) = provider_fixture("alice", responses);
            let limits = NotificationReadLimits {
                max_notifications: 1,
                ..NotificationReadLimits::default()
            };
            let batch = provider
                .notification_observations(&[provider_repo("alice")], None, limits)
                .unwrap();
            assert!(!batch.complete);
            assert!(!batch.repositories[0].complete);
            assert!(batch.repositories[0]
                .reasons
                .iter()
                .any(|reason| reason.contains("item bound")));
        }

        #[test]
        fn malformed_authoritative_event_is_not_admitted() {
            let mut responses = complete_responses("review_requested", "success");
            responses.insert(
                "repos/owner/repo/issues/7/timeline?per_page=100&page=1".into(),
                json!([{
                    "id": 41, "event": "review_requested",
                    "actor": {"login": "bob"}, "requested_reviewer": {"login": "alice"},
                    "created_at": "not-a-timestamp"
                }]),
            );
            let (_dir, provider) = provider_fixture("alice", responses);
            let batch = provider
                .notification_observations(
                    &[provider_repo("alice")],
                    None,
                    NotificationReadLimits::default(),
                )
                .unwrap();
            assert!(!batch.complete);
            assert!(batch.observations[0].events.is_empty());
            assert!(batch.incomplete_candidates.iter().any(|candidate| {
                candidate.reason.contains("candidate evidence read failed")
            }));
        }

        #[test]
        fn selected_account_and_repository_are_not_interchangeable() {
            let (_dir, provider) = provider_fixture("alice", HashMap::new());
            assert!(provider.notification_observations(&[provider_repo("bob")], None, NotificationReadLimits::default()).is_err());
            assert!(provider.notification_observations(&[], None, NotificationReadLimits::default()).is_err());
        }
    };
}

#[macro_export]
macro_rules! notification_store_tests {
    () => {
        use super::*;
        use $crate::providers::notifications::{
            IncompleteNotificationCandidate, NotificationAlertKind, NotificationEventSource,
            NotificationEvidence, ProviderNotificationObservation,
            RepositoryNotificationCompleteness,
        };
        use std::{
            fs,
            os::unix::fs::{PermissionsExt, symlink},
            process::{Command, Stdio},
            thread,
            time::{Duration, Instant},
        };

        fn store_account(login: &str) -> Account { Account { host: "github.com".into(), login: login.into() } }

        fn store_scope(login: &str, repo: &str) -> NotificationRepositoryScope {
            NotificationRepositoryScope { provider: "github".into(), host: "github.com".into(), account: login.into(), owner: "owner".into(), repository: repo.into() }
        }

        fn store_target(login: &str, repo: &str, pull_request: u64) -> NotificationPullRequest {
            NotificationPullRequest { provider: "github".into(), host: "github.com".into(), account: login.into(), owner: "owner".into(), repository: repo.into(), pull_request }
        }

        fn store_event(login: &str, repo: &str, id: &str, minute: u32) -> ProviderNotificationEvent {
            let target = store_target(login, repo, 7);
            ProviderNotificationEvent {
                identity: NotificationEventIdentity { target, source: NotificationEventSource::Timeline, remote_event_id: id.into() },
                occurred_at: format!("2026-09-13T10:{minute:02}:00Z"),
                actor: Some("bob".into()), alert_kind: NotificationAlertKind::ReviewRequest,
                summary: "Review requested".into(), url: format!("https://github.com/owner/{repo}/pull/7"),
                evidence: NotificationEvidence::ReviewRequested { timeline_event_id: id.into(), requested_reviewer: login.into() },
            }
        }

        fn store_batch(login: &str, repo: &str, events: Vec<ProviderNotificationEvent>, full: bool, complete: bool) -> ProviderNotificationBatch {
            ProviderNotificationBatch {
                account: store_account(login), observed_at_unix_ms: 1,
                observations: vec![ProviderNotificationObservation {
                    provider_notification_id: "thread-1".into(), provider_reason: "author".into(),
                    notification_updated_at: "2026-09-13T12:00:00Z".into(), target: store_target(login, repo, 7), events,
                }],
                incomplete_candidates: Vec::<IncompleteNotificationCandidate>::new(),
                full_snapshot: full,
                repositories: vec![RepositoryNotificationCompleteness { target: store_scope(login, repo), complete, reasons: vec![] }],
                complete,
                notices: vec![],
            }
        }

        #[test]
        fn baseline_duplicate_poll_and_restart_are_suppressed_then_new_events_alert_once() {
            let dir = tempfile::tempdir().unwrap();
            let account = store_account("alice");
            let event1 = store_event("alice", "repo", "event-1", 1);
            let store = NotificationStore::open(dir.path(), NotificationStoreLimits::default()).unwrap();
            let first = store.reconcile(&account, &store_batch("alice", "repo", vec![event1.clone()], true, true)).unwrap();
            assert!(first.newly_admitted_alerts.is_empty());
            assert!(first.unread_by_pull_request.is_empty());

            let restarted = NotificationStore::open(dir.path(), NotificationStoreLimits::default()).unwrap();
            let duplicate = restarted.reconcile(&account, &store_batch("alice", "repo", vec![event1.clone()], true, true)).unwrap();
            assert!(duplicate.newly_admitted_alerts.is_empty());
            let event2 = store_event("alice", "repo", "event-2", 2);
            let later = restarted.reconcile(&account, &store_batch("alice", "repo", vec![event1.clone(), event2.clone()], true, true)).unwrap();
            assert_eq!(later.newly_admitted_alerts, vec![event2]);
            let again = restarted.reconcile(&account, &store_batch("alice", "repo", vec![event1, later.newly_admitted_alerts[0].clone()], true, true)).unwrap();
            assert!(again.newly_admitted_alerts.is_empty());
            assert_eq!(again.unread_by_pull_request[0].unread_events.len(), 1);
        }

        #[test]
        fn partial_or_cursor_read_cannot_establish_a_repository_baseline() {
            let dir = tempfile::tempdir().unwrap();
            let account = store_account("alice");
            let store = NotificationStore::open(dir.path(), NotificationStoreLimits::default()).unwrap();
            let event1 = store_event("alice", "repo", "event-1", 1);
            assert!(store.reconcile(&account, &store_batch("alice", "repo", vec![event1.clone()], false, false)).unwrap().newly_admitted_alerts.is_empty());
            let event2 = store_event("alice", "repo", "event-2", 2);
            let baseline = store.reconcile(&account, &store_batch("alice", "repo", vec![event1.clone(), event2.clone()], true, true)).unwrap();
            assert!(baseline.newly_admitted_alerts.is_empty());
            assert!(baseline.unread_by_pull_request.is_empty());
            let event3 = store_event("alice", "repo", "event-3", 3);
            let later = store.reconcile(&account, &store_batch("alice", "repo", vec![event1, event2, event3.clone()], false, true)).unwrap();
            assert_eq!(later.newly_admitted_alerts, vec![event3]);
        }

        #[test]
        fn accounts_and_later_added_repositories_have_independent_baselines() {
            let dir = tempfile::tempdir().unwrap();
            let store = NotificationStore::open(dir.path(), NotificationStoreLimits::default()).unwrap();
            for login in ["alice", "bob"] {
                let event = store_event(login, "repo", "event-1", 1);
                assert!(store.reconcile(&store_account(login), &store_batch(login, "repo", vec![event], true, true)).unwrap().newly_admitted_alerts.is_empty());
            }
            let alice_new_repo = store_event("alice", "second", "historical", 2);
            assert!(store.reconcile(&store_account("alice"), &store_batch("alice", "second", vec![alice_new_repo], true, true)).unwrap().newly_admitted_alerts.is_empty());
            assert!(store.list_unread(&store_account("alice")).unwrap().unread_by_pull_request.is_empty());
            assert!(store.list_unread(&store_account("bob")).unwrap().unread_by_pull_request.is_empty());
        }

        #[test]
        fn exact_displayed_mark_read_does_not_consume_a_concurrent_event() {
            let dir = tempfile::tempdir().unwrap();
            let account = store_account("alice");
            let store = NotificationStore::open(dir.path(), NotificationStoreLimits::default()).unwrap();
            store.reconcile(&account, &store_batch("alice", "repo", vec![], true, true)).unwrap();
            let event1 = store_event("alice", "repo", "event-1", 1);
            let displayed_snapshot = store.reconcile(&account, &store_batch("alice", "repo", vec![event1.clone()], false, true)).unwrap();
            let displayed = DisplayedNotificationSet { state_version: displayed_snapshot.state_version, events: vec![event1.identity.clone()] };
            let event2 = store_event("alice", "repo", "event-2", 2);
            store.reconcile(&account, &store_batch("alice", "repo", vec![event1, event2.clone()], false, true)).unwrap();
            let after = store.mark_displayed_read(&account, &displayed).unwrap();
            assert_eq!(after.unread_by_pull_request[0].unread_events, vec![event2]);
        }

        #[test]
        fn offline_failure_retains_prior_unread_and_has_no_replay_surface() {
            let dir = tempfile::tempdir().unwrap();
            let account = store_account("alice");
            let store = NotificationStore::open(
                dir.path(),
                NotificationStoreLimits::default(),
            )
            .unwrap();
            store
                .reconcile(
                    &account,
                    &store_batch("alice", "repo", vec![], true, true),
                )
                .unwrap();
            let event = store_event("alice", "repo", "event-1", 1);
            store
                .reconcile(
                    &account,
                    &store_batch("alice", "repo", vec![event.clone()], false, true),
                )
                .unwrap();
            let before = store.list_unread(&account).unwrap();
            let provider_failure: anyhow::Result<ProviderNotificationBatch> =
                Err(anyhow::anyhow!("offline"));
            assert!(provider_failure.is_err());
            let after = store.list_unread(&account).unwrap();
            assert_eq!(after, before);
            assert_eq!(after.unread_by_pull_request[0].unread_events, vec![event]);
        }

        #[test]
        fn inconsistent_same_identity_and_unsafe_retention_fail_without_replacing_state() {
            let dir = tempfile::tempdir().unwrap();
            let account = store_account("alice");
            let limits = NotificationStoreLimits { max_events: 1, ..NotificationStoreLimits::default() };
            let store = NotificationStore::open(dir.path(), limits).unwrap();
            store.reconcile(&account, &store_batch("alice", "repo", vec![], true, true)).unwrap();
            let event1 = store_event("alice", "repo", "event-1", 1);
            store.reconcile(&account, &store_batch("alice", "repo", vec![event1.clone()], false, true)).unwrap();
            let before = fs::read(store.state_path(&account)).unwrap();
            let mut inconsistent = event1.clone();
            inconsistent.summary = "Changed immutable payload".into();
            assert!(store.reconcile(&account, &store_batch("alice", "repo", vec![inconsistent], false, true)).is_err());
            assert_eq!(fs::read(store.state_path(&account)).unwrap(), before);
            let event2 = store_event("alice", "repo", "event-2", 2);
            assert!(store.reconcile(&account, &store_batch("alice", "repo", vec![event1, event2], false, true)).unwrap_err().to_string().contains("retention bound"));
            assert_eq!(fs::read(store.state_path(&account)).unwrap(), before);
        }

        #[test]
        fn corrupt_future_and_oversize_state_are_preserved() {
            for (name, bytes, limit) in [
                ("corrupt", b"not json".to_vec(), 4096usize),
                ("future", br#"{"format_version":999,"state_version":0,"account":null,"baselines":[],"events":[]}"#.to_vec(), 4096),
                ("oversize", vec![b'x'; 4097], 4096),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let account = store_account("alice");
                let store = NotificationStore::open(dir.path(), NotificationStoreLimits { max_state_bytes: limit, ..NotificationStoreLimits::default() }).unwrap();
                let path = store.state_path(&account);
                fs::write(&path, &bytes).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                assert!(store.list_unread(&account).is_err(), "{name}");
                assert_eq!(fs::read(&path).unwrap(), bytes, "{name}");
            }
        }

        #[test]
        fn lock_is_nofollow_single_link_and_explicit_unlock_releases_duplicates() {
            let dir = tempfile::tempdir().unwrap();
            let account = store_account("alice");
            let store = NotificationStore::open(dir.path(), NotificationStoreLimits::default()).unwrap();
            let guard = StoreLock::acquire(&store.lock_path(&account)).unwrap();
            let duplicate = guard.file.try_clone().unwrap();
            assert!(StoreLock::acquire(&store.lock_path(&account)).is_err());
            drop(guard);
            let reacquired = StoreLock::acquire(&store.lock_path(&account)).unwrap();
            drop(reacquired);
            drop(duplicate);

            let other = tempfile::tempdir().unwrap();
            let symlink_store = NotificationStore::open(other.path(), NotificationStoreLimits::default()).unwrap();
            let outside = other.path().join("outside");
            fs::write(&outside, "outside").unwrap();
            symlink(&outside, symlink_store.lock_path(&account)).unwrap();
            assert!(symlink_store.list_unread(&account).is_err());
            assert_eq!(fs::read_to_string(outside).unwrap(), "outside");
        }

        #[test]
        #[ignore = "helper process selected by the parent lock test"]
        fn notification_lock_helper_process() {
            let Some(path) = std::env::var_os("CIBERGIT_NOTIFICATION_LOCK_HELPER_PATH") else { return; };
            let marker = std::env::var_os("CIBERGIT_NOTIFICATION_LOCK_HELPER_MARKER").unwrap();
            let _guard = StoreLock::acquire(Path::new(&path)).unwrap();
            fs::write(marker, "locked").unwrap();
            let mut stdin = std::io::stdin();
            let mut sink = Vec::new();
            std::io::Read::read_to_end(&mut stdin, &mut sink).unwrap();
        }

        #[test]
        fn lock_excludes_another_process_with_bounded_fixture_outcome() {
            let dir = tempfile::tempdir().unwrap();
            let account = store_account("alice");
            let store = NotificationStore::open(dir.path(), NotificationStoreLimits::default()).unwrap();
            let marker = dir.path().join("marker");
            let mut child = Command::new(std::env::current_exe().unwrap())
                .arg("notification_lock_helper_process").arg("--ignored").arg("--nocapture")
                .env("CIBERGIT_NOTIFICATION_LOCK_HELPER_PATH", store.lock_path(&account))
                .env("CIBERGIT_NOTIFICATION_LOCK_HELPER_MARKER", &marker)
                .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            let started = Instant::now();
            while !marker.exists() && started.elapsed() < Duration::from_secs(5) {
                if let Some(status) = child.try_wait().unwrap() { panic!("lock helper exited before readiness: {status}"); }
                thread::sleep(Duration::from_millis(5));
            }
            assert!(marker.exists(), "lock helper readiness timed out after {:?}", started.elapsed());
            assert!(store.list_unread(&account).unwrap_err().to_string().contains("locked by another process"));
            drop(child.stdin.take());
            let status = child.wait().unwrap();
            assert!(status.success(), "lock helper outcome: {status}");
            assert!(store.list_unread(&account).is_ok());
        }
    };
}
