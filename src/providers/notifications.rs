//! Bounded, read-only notification evidence for explicitly selected repositories.
//!
//! GitHub's notification `reason` is deliberately only a discovery hint. It is
//! sticky per thread, and `ci_activity` only says a viewer-triggered workflow
//! completed. Classification below therefore requires immutable event evidence
//! bound to the selected account, repository, and pull request. See:
//! <https://docs.github.com/en/rest/activity/notifications>
//! <https://docs.github.com/en/rest/issues/timeline>
//! <https://docs.github.com/en/rest/pulls/comments>
//! <https://docs.github.com/en/rest/checks/runs>

use super::{GithubProvider, PAGE_SIZE, Session, validate_sha};
use crate::domain::{Account, Repository};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_NOTIFICATION_REPOSITORIES: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationReadLimits {
    pub max_notification_pages: usize,
    pub max_notifications: usize,
    pub max_candidate_hydrations: usize,
    pub max_timeline_pages: usize,
    pub max_review_comment_pages: usize,
    pub max_check_pages: usize,
}

impl Default for NotificationReadLimits {
    fn default() -> Self {
        Self {
            max_notification_pages: 4,
            max_notifications: 400,
            max_candidate_hydrations: 40,
            max_timeline_pages: 4,
            max_review_comment_pages: 4,
            max_check_pages: 4,
        }
    }
}

impl NotificationReadLimits {
    fn validate(&self) -> Result<()> {
        for (name, value, maximum) in [
            ("notification pages", self.max_notification_pages, 10),
            ("notifications", self.max_notifications, 1_000),
            ("candidate hydrations", self.max_candidate_hydrations, 100),
            ("timeline pages", self.max_timeline_pages, 10),
            ("review-comment pages", self.max_review_comment_pages, 10),
            ("check pages", self.max_check_pages, 10),
        ] {
            ensure!(value > 0 && value <= maximum, "Invalid {name} bound");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NotificationPullRequest {
    pub provider: String,
    pub host: String,
    pub account: String,
    pub owner: String,
    pub repository: String,
    pub pull_request: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationEventSource {
    Timeline,
    ReviewComment,
    CheckRun,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NotificationEventIdentity {
    pub target: NotificationPullRequest,
    pub source: NotificationEventSource,
    pub remote_event_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationAlertKind {
    ReviewRequest,
    Mention,
    Reply,
    FailedCheckOwnPullRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotificationEvidence {
    /// Native `mentioned` timeline events identify the body-mention recipient
    /// in `actor`; this does not infer a mention from comment text or a sticky
    /// notification reason, and does not identify the person who wrote it.
    MentionedInBody {
        timeline_event_id: String,
        mentioned_user: String,
    },
    ReviewRequested {
        timeline_event_id: String,
        requested_reviewer: String,
    },
    ReviewThreadReply {
        comment_id: String,
        parent_comment_id: String,
        parent_author: String,
    },
    FailedCheck {
        check_run_id: String,
        head_sha: String,
        pull_request_author: String,
        conclusion: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderNotificationEvent {
    pub identity: NotificationEventIdentity,
    pub occurred_at: String,
    pub actor: Option<String>,
    pub alert_kind: NotificationAlertKind,
    pub summary: String,
    pub url: String,
    pub evidence: NotificationEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteCandidateKind {
    ReviewRequest,
    Mention,
    Reply,
    FailedCheckOwnPullRequest,
    PullRequestNotification,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncompleteNotificationCandidate {
    pub target: Option<NotificationPullRequest>,
    pub provider_notification_id: Option<String>,
    pub kind: IncompleteCandidateKind,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderNotificationObservation {
    /// Notification-thread identity is intentionally separate from event IDs.
    pub provider_notification_id: String,
    pub provider_reason: String,
    pub notification_updated_at: String,
    pub target: NotificationPullRequest,
    pub events: Vec<ProviderNotificationEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderNotificationBatch {
    pub account: Account,
    pub observed_at_unix_ms: u64,
    pub observations: Vec<ProviderNotificationObservation>,
    pub incomplete_candidates: Vec<IncompleteNotificationCandidate>,
    /// True only when this was an unfiltered all-history enumeration. A
    /// `since` overlap read can update an existing baseline but cannot create one.
    pub full_snapshot: bool,
    pub repositories: Vec<RepositoryNotificationCompleteness>,
    pub complete: bool,
    pub notices: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryNotificationCompleteness {
    pub target: NotificationRepositoryScope,
    pub complete: bool,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NotificationRepositoryScope {
    pub provider: String,
    pub host: String,
    pub account: String,
    pub owner: String,
    pub repository: String,
}

impl GithubProvider {
    /// Read notification candidates for one to five explicitly selected repos.
    ///
    /// This method issues GET-only GitHub API calls and never marks a provider
    /// notification read. `since` is an optional caller-owned overlap cursor;
    /// it must be an RFC3339-looking UTC timestamp and is not persisted here.
    pub fn notification_observations(
        &self,
        repositories: &[Repository],
        since: Option<&str>,
        limits: NotificationReadLimits,
    ) -> Result<ProviderNotificationBatch> {
        limits.validate()?;
        ensure!(
            !repositories.is_empty() && repositories.len() <= MAX_NOTIFICATION_REPOSITORIES,
            "Notifications require one to five explicitly selected repositories"
        );
        if let Some(since) = since {
            validate_timestamp(since)?;
        }
        let mut selected = HashMap::new();
        for repository in repositories {
            self.validate_repo(repository)?;
            let key = repository.full_name().to_ascii_lowercase();
            ensure!(
                selected.insert(key, repository).is_none(),
                "Duplicate selected notification repository"
            );
        }

        let mut repository_completeness: HashMap<String, RepositoryNotificationCompleteness> =
            selected
                .iter()
                .map(|(key, repo)| {
                    (
                        key.clone(),
                        RepositoryNotificationCompleteness {
                            target: scope(self, repo),
                            complete: true,
                            reasons: Vec::new(),
                        },
                    )
                })
                .collect();
        let mut session = Session::new(self);
        let mut raw = Vec::new();
        let mut complete = true;
        let mut notices = Vec::new();
        let mut incomplete = Vec::new();
        let mut seen_notifications = HashSet::new();
        let mut pages_read = 0usize;
        'repositories: for (repository_index, repository) in repositories.iter().enumerate() {
            if pages_read == limits.max_notification_pages {
                complete = false;
                notice(
                    &mut notices,
                    "Notification page bound reached; omitted candidates are unknown.",
                );
                mark_selected_incomplete(
                    &mut repository_completeness,
                    &repositories[repository_index..],
                    "global notification page bound reached",
                );
                break;
            }
            if raw.len() == limits.max_notifications {
                complete = false;
                notice(
                    &mut notices,
                    "Notification item bound reached; omitted candidates are unknown.",
                );
                mark_selected_incomplete(
                    &mut repository_completeness,
                    &repositories[repository_index..],
                    "global notification item bound reached",
                );
                break;
            }

            let key = repository.full_name().to_ascii_lowercase();
            let mut repository_page = 1usize;
            loop {
                pages_read += 1;
                let suffix = since.map_or_else(String::new, |value| format!("&since={value}"));
                let endpoint = format!(
                    "repos/{}/notifications?all=true&participating=false&per_page={PAGE_SIZE}&page={repository_page}{suffix}",
                    repository.full_name()
                );
                let page_items: Vec<ApiNotification> = match session.get(&endpoint) {
                    Ok(items) => items,
                    Err(error) => {
                        complete = false;
                        mark_repo_incomplete(
                            &mut repository_completeness,
                            &key,
                            "notification enumeration read failed",
                        );
                        notice(
                            &mut notices,
                            "A repository notification read failed; its candidate set is unknown.",
                        );
                        incomplete.push(IncompleteNotificationCandidate {
                            target: None,
                            provider_notification_id: None,
                            kind: IncompleteCandidateKind::PullRequestNotification,
                            reason: format!(
                                "notification enumeration read failed for {}: {error}",
                                repository.full_name()
                            ),
                        });
                        continue 'repositories;
                    }
                };
                if page_items.len() > PAGE_SIZE {
                    complete = false;
                    mark_repo_incomplete(
                        &mut repository_completeness,
                        &key,
                        "notification enumeration returned an invalid page",
                    );
                    incomplete.push(IncompleteNotificationCandidate {
                        target: None,
                        provider_notification_id: None,
                        kind: IncompleteCandidateKind::PullRequestNotification,
                        reason: format!(
                            "notification enumeration returned more than {PAGE_SIZE} items for {}",
                            repository.full_name()
                        ),
                    });
                    continue 'repositories;
                }
                let last = page_items.len() < PAGE_SIZE;
                let mut omitted_from_page = false;
                for item in page_items {
                    ensure!(
                        seen_notifications.insert(item.id.clone()),
                        "Notifications changed during pagination; refresh to retry"
                    );
                    if raw.len() == limits.max_notifications {
                        omitted_from_page = true;
                        break;
                    }
                    raw.push((key.clone(), item));
                }
                if raw.len() == limits.max_notifications {
                    let incomplete_from = if omitted_from_page || !last {
                        repository_index
                    } else {
                        repository_index + 1
                    };
                    if incomplete_from < repositories.len() {
                        complete = false;
                        notice(
                            &mut notices,
                            "Notification item bound reached; omitted candidates are unknown.",
                        );
                        mark_selected_incomplete(
                            &mut repository_completeness,
                            &repositories[incomplete_from..],
                            "global notification item bound reached",
                        );
                    }
                    break 'repositories;
                }
                if last {
                    break;
                }
                if pages_read == limits.max_notification_pages {
                    complete = false;
                    notice(
                        &mut notices,
                        "Notification page bound reached; omitted candidates are unknown.",
                    );
                    mark_selected_incomplete(
                        &mut repository_completeness,
                        &repositories[repository_index..],
                        "global notification page bound reached",
                    );
                    break 'repositories;
                }
                repository_page += 1;
            }
        }

        let mut observations = Vec::new();
        let mut hydrated = 0usize;
        for (key, notification) in raw {
            let repository = selected
                .get(&key)
                .copied()
                .expect("enumerated repository was selected");
            if let Err(reason) = notification.validate(repository) {
                complete = false;
                mark_repo_incomplete(
                    &mut repository_completeness,
                    &key,
                    "notification identity validation failed",
                );
                incomplete.push(incomplete_notification(
                    &notification,
                    None,
                    reason.to_string(),
                ));
                continue;
            }
            if notification.subject.kind != "PullRequest" {
                if known_non_pull_subject(&notification.subject.kind) {
                    continue;
                }
                complete = false;
                mark_repo_incomplete(
                    &mut repository_completeness,
                    &key,
                    "notification subject type was unknown",
                );
                incomplete.push(incomplete_notification(
                    &notification,
                    None,
                    "Unknown notification subject type; PR candidate completeness is unknown"
                        .into(),
                ));
                continue;
            }
            let number = match parse_pull_subject_url(repository, &notification.subject) {
                Ok(number) => number,
                Err(reason) => {
                    complete = false;
                    mark_repo_incomplete(
                        &mut repository_completeness,
                        &key,
                        "pull-request subject identity was invalid",
                    );
                    incomplete.push(incomplete_notification(
                        &notification,
                        None,
                        reason.to_string(),
                    ));
                    continue;
                }
            };
            let target = target(self, repository, number);
            if hydrated == limits.max_candidate_hydrations {
                complete = false;
                mark_repo_incomplete(
                    &mut repository_completeness,
                    &key,
                    "candidate hydration bound reached",
                );
                incomplete.push(IncompleteNotificationCandidate {
                    target: Some(target),
                    provider_notification_id: Some(notification.id),
                    kind: IncompleteCandidateKind::PullRequestNotification,
                    reason: "candidate hydration bound reached; event kind remains unknown".into(),
                });
                continue;
            }
            hydrated += 1;
            let mut events = Vec::new();
            let hydration = hydrate_candidate(
                &mut session,
                self,
                repository,
                number,
                &notification,
                &limits,
                &mut events,
                &mut incomplete,
            );
            match hydration {
                Ok(true) => {}
                Ok(false) => {
                    complete = false;
                    mark_repo_incomplete(
                        &mut repository_completeness,
                        &key,
                        "candidate evidence was capped or incomplete",
                    );
                }
                Err(error) => {
                    complete = false;
                    mark_repo_incomplete(
                        &mut repository_completeness,
                        &key,
                        "candidate evidence read failed",
                    );
                    incomplete.push(IncompleteNotificationCandidate {
                        target: Some(target.clone()),
                        provider_notification_id: Some(notification.id.clone()),
                        kind: IncompleteCandidateKind::PullRequestNotification,
                        reason: format!("candidate evidence read failed: {error}"),
                    });
                }
            }
            events.sort_by(|left, right| {
                left.identity
                    .remote_event_id
                    .cmp(&right.identity.remote_event_id)
            });
            observations.push(ProviderNotificationObservation {
                provider_notification_id: notification.id,
                provider_reason: notification.reason,
                notification_updated_at: notification.updated_at,
                target,
                events,
            });
        }
        observations.sort_by(|left, right| {
            left.provider_notification_id
                .cmp(&right.provider_notification_id)
        });
        let observed_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("System clock is before Unix epoch")?
            .as_millis()
            .try_into()
            .context("System timestamp is outside supported range")?;
        let mut repositories: Vec<_> = repository_completeness.into_values().collect();
        repositories.sort_by_key(|value| scope_key(&value.target));
        Ok(ProviderNotificationBatch {
            account: self.account.clone(),
            observed_at_unix_ms,
            observations,
            incomplete_candidates: incomplete,
            full_snapshot: since.is_none(),
            repositories,
            complete,
            notices,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn hydrate_candidate(
    session: &mut Session<'_>,
    provider: &GithubProvider,
    repo: &Repository,
    number: u64,
    notification: &ApiNotification,
    limits: &NotificationReadLimits,
    events: &mut Vec<ProviderNotificationEvent>,
    incomplete: &mut Vec<IncompleteNotificationCandidate>,
) -> Result<bool> {
    let exact_target = target(provider, repo, number);
    let pr: ApiNotificationPull =
        session.get(&format!("repos/{}/pulls/{number}", repo.full_name()))?;
    pr.validate(repo, number)?;

    let timeline_complete = read_timeline(
        session,
        provider,
        repo,
        number,
        notification,
        limits.max_timeline_pages,
        events,
        incomplete,
    )?;
    let replies_complete = read_review_replies(
        session,
        provider,
        repo,
        number,
        notification,
        limits.max_review_comment_pages,
        events,
        incomplete,
    )?;
    let checks_complete = read_failed_checks(
        session,
        provider,
        repo,
        number,
        notification,
        &pr,
        limits.max_check_pages,
        events,
        incomplete,
    )?;

    if matches!(notification.reason.as_str(), "mention" | "team_mention") {
        incomplete.push(IncompleteNotificationCandidate {
            target: Some(exact_target),
            provider_notification_id: Some(notification.id.clone()),
            kind: IncompleteCandidateKind::Mention,
            reason: "Comment/team mention evidence remains unavailable from sticky notification reasons or body text; any exact native PR-body mention events are classified separately".into(),
        });
    }
    Ok(timeline_complete && replies_complete && checks_complete)
}

#[allow(clippy::too_many_arguments)]
fn read_timeline(
    session: &mut Session<'_>,
    provider: &GithubProvider,
    repo: &Repository,
    number: u64,
    notification: &ApiNotification,
    max_pages: usize,
    events: &mut Vec<ProviderNotificationEvent>,
    incomplete: &mut Vec<IncompleteNotificationCandidate>,
) -> Result<bool> {
    let target = target(provider, repo, number);
    let mut seen = HashSet::new();
    for page in 1..=max_pages {
        let values: Vec<ApiTimelineEvent> = session.get(&format!(
            "repos/{}/issues/{number}/timeline?per_page={PAGE_SIZE}&page={page}",
            repo.full_name()
        ))?;
        ensure!(values.len() <= PAGE_SIZE, "Invalid timeline page size");
        let last = values.len() < PAGE_SIZE;
        for value in values {
            if value.event.as_deref() == Some("mentioned") {
                let id = value
                    .id
                    .context("mentioned event omitted immutable numeric ID")?;
                ensure!(id > 0, "Invalid mentioned event identity");
                ensure!(
                    seen.insert(id.to_string()),
                    "Timeline changed during pagination; refresh to retry"
                );
                if let Some(event) = prove_body_mention(provider, repo, number, value)? {
                    events.push(event);
                }
                continue;
            }
            if value.event.as_deref() != Some("review_requested") {
                continue;
            }
            let Some(id) = value.id.map(|id| id.to_string()).or(value.node_id.clone()) else {
                incomplete.push(candidate(
                    &target,
                    notification,
                    IncompleteCandidateKind::ReviewRequest,
                    "review_requested event omitted immutable identity",
                ));
                continue;
            };
            ensure!(
                seen.insert(id.clone()),
                "Timeline changed during pagination; refresh to retry"
            );
            let requested = value
                .requested_reviewer
                .as_ref()
                .map(|user| user.login.as_str());
            if requested.map(|login| login.eq_ignore_ascii_case(&provider.account.login))
                != Some(true)
            {
                continue;
            }
            let Some(created_at) = value.created_at.as_deref() else {
                incomplete.push(candidate(
                    &target,
                    notification,
                    IncompleteCandidateKind::ReviewRequest,
                    "review_requested event omitted immutable timestamp",
                ));
                continue;
            };
            validate_timestamp(created_at)?;
            let Some(actor) = value.actor.as_ref().map(|actor| actor.login.clone()) else {
                incomplete.push(candidate(
                    &target,
                    notification,
                    IncompleteCandidateKind::ReviewRequest,
                    "review_requested event omitted actor identity",
                ));
                continue;
            };
            validate_login(&actor)?;
            events.push(ProviderNotificationEvent {
                identity: NotificationEventIdentity {
                    target: target.clone(),
                    source: NotificationEventSource::Timeline,
                    remote_event_id: id.clone(),
                },
                occurred_at: created_at.into(),
                actor: Some(actor),
                alert_kind: NotificationAlertKind::ReviewRequest,
                summary: "Review requested".into(),
                url: pull_url(repo, number),
                evidence: NotificationEvidence::ReviewRequested {
                    timeline_event_id: id,
                    requested_reviewer: provider.account.login.clone(),
                },
            });
        }
        if last {
            return Ok(true);
        }
        if page == max_pages {
            incomplete.push(candidate(
                &target,
                notification,
                IncompleteCandidateKind::ReviewRequest,
                "timeline page bound reached; review-request and body-mention absence is unknown",
            ));
            return Ok(false);
        }
    }
    Ok(true)
}

/// GitHub documents `mentioned` as the actor being mentioned in an issue or
/// PR body: https://docs.github.com/en/rest/using-the-rest-api/issue-event-types#mentioned
/// The enclosing exact PR timeline and this event's repository URL bind the
/// event. No comment-body search, reason matching or author inference is used.
fn prove_body_mention(
    provider: &GithubProvider,
    repo: &Repository,
    number: u64,
    event: ApiTimelineEvent,
) -> Result<Option<ProviderNotificationEvent>> {
    let recipient = event
        .actor
        .context("mentioned event omitted recipient identity")?
        .login;
    validate_login(&recipient)?;
    if !recipient.eq_ignore_ascii_case(&provider.account.login) {
        return Ok(None);
    }
    let id = event
        .id
        .context("mentioned event omitted immutable numeric ID")?;
    ensure!(id > 0, "Invalid mentioned event identity");
    let (event_repository, event_id) = event
        .url
        .as_deref()
        .and_then(|url| url.strip_prefix("https://api.github.com/repos/"))
        .and_then(|path| path.split_once("/issues/events/"))
        .context("Mentioned event URL is not a canonical GitHub issue-event URL")?;
    ensure!(
        event_repository.eq_ignore_ascii_case(&repo.full_name()) && event_id == id.to_string(),
        "Mentioned event URL does not bind the selected repository and event ID"
    );
    let occurred_at = event
        .created_at
        .context("mentioned event omitted timestamp")?;
    validate_timestamp(&occurred_at)?;
    Ok(Some(ProviderNotificationEvent {
        identity: NotificationEventIdentity {
            target: target(provider, repo, number),
            source: NotificationEventSource::Timeline,
            remote_event_id: id.to_string(),
        },
        occurred_at,
        // `actor` is the recipient for this event type, not its author.
        actor: None,
        alert_kind: NotificationAlertKind::Mention,
        summary: "You were mentioned in the pull request body".into(),
        url: pull_url(repo, number),
        evidence: NotificationEvidence::MentionedInBody {
            timeline_event_id: id.to_string(),
            mentioned_user: provider.account.login.clone(),
        },
    }))
}

#[allow(clippy::too_many_arguments)]
fn read_review_replies(
    session: &mut Session<'_>,
    provider: &GithubProvider,
    repo: &Repository,
    number: u64,
    notification: &ApiNotification,
    max_pages: usize,
    events: &mut Vec<ProviderNotificationEvent>,
    incomplete: &mut Vec<IncompleteNotificationCandidate>,
) -> Result<bool> {
    let target = target(provider, repo, number);
    let mut comments = Vec::new();
    let mut ids = HashSet::new();
    let mut complete = true;
    for page in 1..=max_pages {
        let values: Vec<ApiReviewComment> = session.get(&format!(
            "repos/{}/pulls/{number}/comments?sort=created&direction=asc&per_page={PAGE_SIZE}&page={page}",
            repo.full_name()
        ))?;
        ensure!(
            values.len() <= PAGE_SIZE,
            "Invalid review-comment page size"
        );
        let last = values.len() < PAGE_SIZE;
        for value in values {
            value.validate(repo, number)?;
            ensure!(
                ids.insert(value.id),
                "Review comments changed during pagination; refresh to retry"
            );
            comments.push(value);
        }
        if last {
            break;
        }
        if page == max_pages {
            complete = false;
            incomplete.push(candidate(
                &target,
                notification,
                IncompleteCandidateKind::Reply,
                "review-comment page bound reached; reply absence is unknown",
            ));
        }
    }
    let by_id: HashMap<u64, &ApiReviewComment> = comments
        .iter()
        .map(|comment| (comment.id, comment))
        .collect();
    for comment in comments
        .iter()
        .filter(|comment| comment.in_reply_to_id.is_some())
    {
        let parent_id = comment.in_reply_to_id.expect("filtered");
        let Some(parent) = by_id.get(&parent_id) else {
            incomplete.push(candidate(
                &target,
                notification,
                IncompleteCandidateKind::Reply,
                "reply parent was outside the bounded response; thread ownership is unknown",
            ));
            continue;
        };
        if !parent
            .user
            .login
            .eq_ignore_ascii_case(&provider.account.login)
        {
            continue;
        }
        validate_timestamp(&comment.created_at)?;
        validate_login(&comment.user.login)?;
        events.push(ProviderNotificationEvent {
            identity: NotificationEventIdentity {
                target: target.clone(),
                source: NotificationEventSource::ReviewComment,
                remote_event_id: comment.id.to_string(),
            },
            occurred_at: comment.created_at.clone(),
            actor: Some(comment.user.login.clone()),
            alert_kind: NotificationAlertKind::Reply,
            summary: "Reply to your review thread".into(),
            url: pull_url(repo, number),
            evidence: NotificationEvidence::ReviewThreadReply {
                comment_id: comment.id.to_string(),
                parent_comment_id: parent_id.to_string(),
                parent_author: parent.user.login.clone(),
            },
        });
    }
    Ok(complete)
}

#[allow(clippy::too_many_arguments)]
fn read_failed_checks(
    session: &mut Session<'_>,
    provider: &GithubProvider,
    repo: &Repository,
    number: u64,
    notification: &ApiNotification,
    pr: &ApiNotificationPull,
    max_pages: usize,
    events: &mut Vec<ProviderNotificationEvent>,
    incomplete: &mut Vec<IncompleteNotificationCandidate>,
) -> Result<bool> {
    if !pr.user.login.eq_ignore_ascii_case(&provider.account.login) {
        return Ok(true);
    }
    let target = target(provider, repo, number);
    let mut seen = HashSet::new();
    for page in 1..=max_pages {
        let response: ApiCheckRuns = session.get(&format!(
            "repos/{}/commits/{}/check-runs?filter=latest&per_page={PAGE_SIZE}&page={page}",
            repo.full_name(),
            pr.head.sha
        ))?;
        ensure!(
            response.check_runs.len() <= PAGE_SIZE,
            "Invalid check-run page size"
        );
        let page_len = response.check_runs.len();
        let last = page_len < PAGE_SIZE;
        for run in response.check_runs {
            ensure!(
                seen.insert(run.id),
                "Check runs changed during pagination; refresh to retry"
            );
            if run.status != "completed" {
                continue;
            }
            if matches!(run.conclusion.as_deref(), Some("action_required")) {
                incomplete.push(candidate(&target, notification, IncompleteCandidateKind::FailedCheckOwnPullRequest, "check conclusion action_required is not an actual failure/timed_out result; no failed-check alert was classified"));
                continue;
            }
            if !matches!(run.conclusion.as_deref(), Some("failure" | "timed_out")) {
                continue;
            }
            let exact_pull_url = format!(
                "https://api.github.com/repos/{}/pulls/{number}",
                repo.full_name()
            );
            if !run
                .pull_requests
                .iter()
                .any(|pull| pull.number == number && pull.url == exact_pull_url)
            {
                incomplete.push(candidate(&target, notification, IncompleteCandidateKind::FailedCheckOwnPullRequest, "failed check did not carry exact pull-request binding; no alert was classified"));
                continue;
            }
            let Some(completed_at) = run.completed_at.as_deref() else {
                incomplete.push(candidate(
                    &target,
                    notification,
                    IncompleteCandidateKind::FailedCheckOwnPullRequest,
                    "completed failed check omitted completion timestamp",
                ));
                continue;
            };
            validate_timestamp(completed_at)?;
            validate_summary(&run.name)?;
            let conclusion = run.conclusion.clone().expect("matched");
            let event_version = format!("{}:{completed_at}:{conclusion}", run.id);
            events.push(ProviderNotificationEvent {
                identity: NotificationEventIdentity {
                    target: target.clone(),
                    source: NotificationEventSource::CheckRun,
                    remote_event_id: event_version,
                },
                occurred_at: completed_at.into(),
                actor: None,
                alert_kind: NotificationAlertKind::FailedCheckOwnPullRequest,
                summary: format!("Check failed: {}", run.name),
                url: pull_url(repo, number),
                evidence: NotificationEvidence::FailedCheck {
                    check_run_id: run.id.to_string(),
                    head_sha: pr.head.sha.clone(),
                    pull_request_author: pr.user.login.clone(),
                    conclusion,
                },
            });
        }
        if last {
            if seen.len() < response.total_count {
                incomplete.push(candidate(
                    &target,
                    notification,
                    IncompleteCandidateKind::FailedCheckOwnPullRequest,
                    "check-run response total exceeded returned bounded items; absence is unknown",
                ));
                return Ok(false);
            }
            return Ok(true);
        }
        if page == max_pages {
            incomplete.push(candidate(
                &target,
                notification,
                IncompleteCandidateKind::FailedCheckOwnPullRequest,
                "check-run page bound reached; failed-check absence is unknown",
            ));
            return Ok(false);
        }
    }
    Ok(true)
}

fn target(provider: &GithubProvider, repo: &Repository, number: u64) -> NotificationPullRequest {
    NotificationPullRequest {
        provider: "github".into(),
        host: repo.host.clone(),
        account: provider.account.login.clone(),
        owner: repo.owner.clone(),
        repository: repo.name.clone(),
        pull_request: number,
    }
}

fn scope(provider: &GithubProvider, repo: &Repository) -> NotificationRepositoryScope {
    NotificationRepositoryScope {
        provider: "github".into(),
        host: repo.host.clone(),
        account: provider.account.login.clone(),
        owner: repo.owner.clone(),
        repository: repo.name.clone(),
    }
}

fn scope_key(scope: &NotificationRepositoryScope) -> String {
    format!(
        "{}\0{}\0{}\0{}\0{}",
        scope.provider.to_ascii_lowercase(),
        scope.host.to_ascii_lowercase(),
        scope.account.to_ascii_lowercase(),
        scope.owner.to_ascii_lowercase(),
        scope.repository.to_ascii_lowercase()
    )
}

fn mark_repo_incomplete(
    values: &mut HashMap<String, RepositoryNotificationCompleteness>,
    key: &str,
    reason: &str,
) {
    if let Some(value) = values.get_mut(key) {
        value.complete = false;
        if !value.reasons.iter().any(|known| known == reason) {
            value.reasons.push(reason.into());
        }
    }
}

fn mark_selected_incomplete(
    values: &mut HashMap<String, RepositoryNotificationCompleteness>,
    repositories: &[Repository],
    reason: &str,
) {
    for repository in repositories {
        mark_repo_incomplete(values, &repository.full_name().to_ascii_lowercase(), reason);
    }
}

fn candidate(
    target: &NotificationPullRequest,
    notification: &ApiNotification,
    kind: IncompleteCandidateKind,
    reason: &str,
) -> IncompleteNotificationCandidate {
    IncompleteNotificationCandidate {
        target: Some(target.clone()),
        provider_notification_id: Some(notification.id.clone()),
        kind,
        reason: reason.into(),
    }
}

fn incomplete_notification(
    notification: &ApiNotification,
    target: Option<NotificationPullRequest>,
    reason: String,
) -> IncompleteNotificationCandidate {
    IncompleteNotificationCandidate {
        target,
        provider_notification_id: Some(notification.id.clone()),
        kind: IncompleteCandidateKind::PullRequestNotification,
        reason,
    }
}

fn notice(notices: &mut Vec<String>, value: &str) {
    if !notices.iter().any(|known| known == value) {
        notices.push(value.into());
    }
}

fn pull_url(repo: &Repository, number: u64) -> String {
    format!("https://github.com/{}/pull/{number}", repo.full_name())
}

fn validate_login(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 100
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "Invalid GitHub login"
    );
    Ok(())
}

fn validate_summary(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control),
        "Invalid or oversized notification summary"
    );
    Ok(())
}

fn validate_timestamp(value: &str) -> Result<()> {
    ensure!(
        value.len() >= 20
            && value.len() <= 40
            && value.ends_with('Z')
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit()
                    || matches!(byte, b'-' | b':' | b'.' | b'T' | b'Z')),
        "Invalid notification timestamp"
    );
    Ok(())
}

fn known_non_pull_subject(kind: &str) -> bool {
    matches!(
        kind,
        "Issue"
            | "CheckSuite"
            | "Commit"
            | "Release"
            | "Discussion"
            | "RepositoryInvitation"
            | "RepositoryVulnerabilityAlert"
    )
}

fn parse_pull_subject_url(repo: &Repository, subject: &ApiNotificationSubject) -> Result<u64> {
    ensure!(
        subject.kind == "PullRequest",
        "Notification subject is not a pull request"
    );
    let prefix = format!("https://api.github.com/repos/{}/pulls/", repo.full_name());
    let suffix = subject
        .url
        .strip_prefix(&prefix)
        .context("Notification subject URL does not match the selected repository")?;
    ensure!(
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()),
        "Invalid pull-request subject URL"
    );
    let number: u64 = suffix.parse().context("Invalid pull-request number")?;
    ensure!(number > 0, "Invalid pull-request number");
    if let Some(url) = &subject.latest_comment_url {
        let issue_prefix = format!(
            "https://api.github.com/repos/{}/issues/comments/",
            repo.full_name()
        );
        let review_prefix = format!(
            "https://api.github.com/repos/{}/pulls/comments/",
            repo.full_name()
        );
        ensure!(
            url.starts_with(&issue_prefix) || url.starts_with(&review_prefix),
            "Notification latest-comment URL is foreign or malformed"
        );
    }
    Ok(number)
}

#[derive(Deserialize)]
struct ApiNotification {
    id: String,
    reason: String,
    updated_at: String,
    subject: ApiNotificationSubject,
    repository: ApiNotificationRepository,
}

impl ApiNotification {
    fn validate(&self, repo: &Repository) -> Result<()> {
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 128
                && self
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'),
            "Invalid provider notification ID"
        );
        ensure!(
            !self.reason.is_empty()
                && self.reason.len() <= 64
                && self
                    .reason
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
            "Invalid notification reason"
        );
        validate_timestamp(&self.updated_at)?;
        ensure!(
            self.repository
                .full_name
                .eq_ignore_ascii_case(&repo.full_name()),
            "Notification repository identity mismatch"
        );
        ensure!(
            self.repository.html_url == format!("https://github.com/{}", repo.full_name()),
            "Notification repository URL mismatch"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
struct ApiNotificationSubject {
    #[serde(rename = "type")]
    kind: String,
    url: String,
    latest_comment_url: Option<String>,
}

#[derive(Deserialize)]
struct ApiNotificationRepository {
    full_name: String,
    html_url: String,
}

#[derive(Deserialize)]
struct ApiUser {
    login: String,
}

#[derive(Deserialize)]
struct ApiNotificationPull {
    number: u64,
    url: String,
    html_url: String,
    user: ApiUser,
    head: ApiHead,
}

impl ApiNotificationPull {
    fn validate(&self, repo: &Repository, number: u64) -> Result<()> {
        ensure!(self.number == number, "Pull-request number mismatch");
        ensure!(
            self.url
                == format!(
                    "https://api.github.com/repos/{}/pulls/{number}",
                    repo.full_name()
                ),
            "Pull-request API URL mismatch"
        );
        ensure!(
            self.html_url == pull_url(repo, number),
            "Pull-request HTML URL mismatch"
        );
        validate_login(&self.user.login)?;
        validate_sha(&self.head.sha)?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct ApiHead {
    sha: String,
}

#[derive(Deserialize)]
struct ApiTimelineEvent {
    id: Option<u64>,
    url: Option<String>,
    node_id: Option<String>,
    event: Option<String>,
    actor: Option<ApiUser>,
    requested_reviewer: Option<ApiUser>,
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct ApiReviewComment {
    id: u64,
    url: String,
    pull_request_url: String,
    user: ApiUser,
    created_at: String,
    in_reply_to_id: Option<u64>,
}

impl ApiReviewComment {
    fn validate(&self, repo: &Repository, number: u64) -> Result<()> {
        ensure!(self.id > 0, "Invalid review-comment ID");
        ensure!(
            self.url
                == format!(
                    "https://api.github.com/repos/{}/pulls/comments/{}",
                    repo.full_name(),
                    self.id
                ),
            "Review-comment URL mismatch"
        );
        ensure!(
            self.pull_request_url
                == format!(
                    "https://api.github.com/repos/{}/pulls/{number}",
                    repo.full_name()
                ),
            "Review-comment pull-request URL mismatch"
        );
        validate_login(&self.user.login)?;
        validate_timestamp(&self.created_at)?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct ApiCheckRuns {
    total_count: usize,
    check_runs: Vec<ApiCheckRun>,
}

#[derive(Deserialize)]
struct ApiCheckRun {
    id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    completed_at: Option<String>,
    #[serde(default)]
    pull_requests: Vec<ApiCheckPull>,
}

#[derive(Deserialize)]
struct ApiCheckPull {
    number: u64,
    url: String,
}

#[cfg(test)]
#[path = "../../tests/notifications.rs"]
mod notifications_fixture;

#[cfg(test)]
mod tests {
    crate::notification_provider_tests!();
}
