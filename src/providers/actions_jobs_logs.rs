//! Exact, read-only GitHub Actions attempt jobs and bounded plain-text job logs.
//!
//! Jobs and logs are intentionally memory-only. Signed storage locations and
//! credentials are ephemeral transport inputs and never enter diagnostics.

use super::{
    API_VERSION, GithubProvider, RunnerFailureKind, Session, conditional, terminate_process_group,
};
use crate::domain::{
    ActionsAttemptKey, ActionsAttemptLocator, ActionsHeadRelation, ActionsJob, ActionsJobLog,
    ActionsJobStep, ActionsJobsSnapshot, ActionsLinkage, ActionsLogProvenance,
    ActionsPullRequestIdentity, ActionsRunAttemptObservation, CheckKind, CheckRepositoryIdentity,
    CheckShaClass, PullRequestCheck, PullRequestDetails, Repository,
};
use serde::Deserialize;
use std::{
    collections::HashSet,
    fmt,
    io::{Read, Write},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(super) const MAX_ACTIONS_JSON_BYTES: usize = 8 * 1024 * 1024;
const JOBS_PAGE_SIZE: usize = 100;
const MAX_JOB_PAGES: usize = 10;
const MAX_JOBS: usize = 1_000;
const MAX_STEPS_PER_JOB: usize = 256;
const MAX_STEPS: usize = 10_000;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_HEADER_FIELDS: usize = 128;
const MAX_LOCATION_BYTES: usize = 8 * 1024;
const MAX_SIGNED_PATH_QUERY_BYTES: usize = 16 * 1024;
const MAX_LOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_LOG_LINES: usize = 200_000;
const MAX_LOG_LINE_BYTES: usize = 256 * 1024;
const MAX_STORAGE_REDIRECTS: usize = 2;
const JOBS_OPERATION_TIMEOUT: Duration = Duration::from_secs(180);
const LOG_OPERATION_TIMEOUT: Duration = Duration::from_secs(90);
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);
const CURL_API_BODY_LIMIT: usize = 65_536;
const CURL_LOG_OUTPUT_LIMIT: usize = MAX_LOG_BYTES + MAX_HEADER_BYTES + 1;

static NEXT_OBSERVATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default)]
pub struct ActionsCancellation(Arc<AtomicBool>);

impl ActionsCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn shared(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionsReadErrorCategory {
    Credential,
    Permission,
    RateLimited,
    Unavailable,
    NotReady,
    UnsupportedStorageHost,
    MovedIdentity,
    TooLarge,
    TimedOut,
    Cancelled,
    InvalidResponse,
    Transport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionsReadError {
    category: ActionsReadErrorCategory,
    stage: &'static str,
}

impl ActionsReadError {
    fn new(category: ActionsReadErrorCategory, stage: &'static str) -> Self {
        Self { category, stage }
    }

    pub fn category(&self) -> ActionsReadErrorCategory {
        self.category
    }

    #[doc(hidden)]
    pub fn closed(category: ActionsReadErrorCategory) -> Self {
        Self::new(category, "Actions read")
    }
}

impl fmt::Display for ActionsReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let category = match self.category {
            ActionsReadErrorCategory::Credential => {
                "selected GitHub credential is unavailable or invalid"
            }
            ActionsReadErrorCategory::Permission => {
                "selected account cannot read this Actions resource"
            }
            ActionsReadErrorCategory::RateLimited => "refresh was deferred by GitHub rate limiting",
            ActionsReadErrorCategory::Unavailable => {
                "resource is unavailable, missing, expired, or hidden"
            }
            ActionsReadErrorCategory::NotReady => "log is not available yet for this exact job",
            ActionsReadErrorCategory::UnsupportedStorageHost => {
                "GitHub returned an unsupported log storage location"
            }
            ActionsReadErrorCategory::MovedIdentity => {
                "GitHub changed the run or job identity during the bounded read"
            }
            ActionsReadErrorCategory::TooLarge => "response exceeded a bounded read limit",
            ActionsReadErrorCategory::TimedOut => "bounded read timed out",
            ActionsReadErrorCategory::Cancelled => "bounded read was cancelled",
            ActionsReadErrorCategory::InvalidResponse => {
                "GitHub returned an invalid or incomplete response"
            }
            ActionsReadErrorCategory::Transport => "read-only transport failed",
        };
        write!(formatter, "{}: {category}", self.stage)
    }
}

impl std::error::Error for ActionsReadError {}

type ActionsResult<T> = Result<T, ActionsReadError>;

impl GithubProvider {
    /// Freeze the complete locally observed tuple. This does not perform I/O;
    /// the server-side viewer is added only by `read_actions_jobs`.
    pub fn actions_attempt_locator(
        &self,
        repo: &Repository,
        details: &PullRequestDetails,
        check: &PullRequestCheck,
    ) -> ActionsResult<ActionsAttemptLocator> {
        self.validate_repo(repo).map_err(|_| invalid("admission"))?;
        if !details.checks_complete
            || details.number == 0
            || details.number != check.coordinates.pull_request
            || check.kind != CheckKind::CheckRun
            || !coordinates_match(repo, details.number, check)
        {
            return Err(invalid("admission"));
        }
        let ActionsLinkage::Linked(workflow_run) = &check.actions_linkage else {
            return Err(invalid("admission"));
        };
        let base_repository = required_repository(details.base_repository.as_ref())?;
        if base_repository.name_with_owner != repo.full_name() {
            return Err(moved("admission"));
        }
        validate_workflow(workflow_run, &base_repository)?;
        let pull_request_node_id = required_node(details.pull_request_node_id.as_deref())?;
        let observed_head_sha = required_sha(details.observed_head_sha.as_deref())?;
        let head_repository = required_repository(details.head_repository.as_ref())?;
        let rollup_commit_sha = required_sha(details.rollup_commit_sha.as_deref())?;
        let rollup_repository = required_repository(details.rollup_repository.as_ref())?;
        let check_database_id = positive_graphql_id(check.database_id)?;
        let check_commit_sha = required_sha(check.commit_sha.as_deref())?;
        let check_repository = required_repository(check.commit_repository.as_ref())?;
        let suite = check.suite.clone().ok_or_else(|| invalid("admission"))?;
        required_node(Some(&suite.node_id))?;
        positive_graphql_id(suite.database_id)?;
        required_repository(Some(&suite.repository))?;
        if suite.repository != check_repository
            || !matches!(
                check.sha_class,
                CheckShaClass::Head | CheckShaClass::MergeCandidate
            )
            || (check.sha_class == CheckShaClass::Head && check_commit_sha != observed_head_sha)
            || (check.sha_class == CheckShaClass::MergeCandidate
                && check_commit_sha != rollup_commit_sha)
        {
            return Err(moved("admission"));
        }
        Ok(ActionsAttemptLocator {
            account: repo.account.clone(),
            base_repository,
            pull_request_node_id,
            pull_request_number: details.number,
            observed_head_sha,
            head_repository,
            rollup_commit_sha,
            rollup_repository,
            check_node_id: required_node(Some(&check.coordinates.remote_id))?,
            check_database_id,
            check_commit_sha,
            check_repository,
            suite,
            workflow_run: workflow_run.clone(),
        })
    }

    pub fn read_actions_jobs(
        &self,
        repo: &Repository,
        locator: &ActionsAttemptLocator,
        current_pull_request_head: &str,
        cancellation: &ActionsCancellation,
    ) -> ActionsResult<ActionsJobsSnapshot> {
        if conditional::active_general_read_tracker().is_none() {
            return Err(invalid("account read admission"));
        }
        self.validate_repo(repo).map_err(|_| invalid("jobs"))?;
        if locator.account != self.account || locator.account != repo.account {
            return Err(moved("jobs"));
        }
        let deadline = Instant::now()
            .checked_add(JOBS_OPERATION_TIMEOUT)
            .ok_or_else(|| timed_out("jobs"))?;
        check_operation(cancellation, deadline, "jobs")?;
        let mut session = Session::new_actions_until(self, cancellation.shared(), deadline);
        let viewer: ApiViewer = modified(session.get_conditional("user", None), "viewer identity")?;
        if !viewer.login.eq_ignore_ascii_case(&self.account.login)
            || viewer.login != self.account.login
        {
            return Err(moved("viewer identity"));
        }
        let key = ActionsAttemptKey {
            locator: locator.clone(),
            viewer_node_id: required_node(Some(&viewer.node_id))?,
            viewer_login: bounded_text(&viewer.login, "viewer identity")?,
        };
        let run_endpoint = run_attempt_endpoint(repo, locator);
        let first_run: ApiRunAttempt =
            modified(session.get_conditional(&run_endpoint, None), "run attempt")?;
        let observed_at = now_unix_ms()?;
        let attempt = validate_run(
            repo,
            &key,
            current_pull_request_head,
            first_run,
            observed_at,
        )?;
        check_operation(cancellation, deadline, "run attempt")?;
        let initial_run_fingerprint = run_fingerprint(&attempt);

        let mut page_records = Vec::new();
        let mut jobs = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut seen_nodes = HashSet::new();
        let mut total_count = None;
        let mut step_count = 0usize;
        for page in 1..=MAX_JOB_PAGES {
            check_cancel(cancellation, "jobs")?;
            let endpoint = jobs_endpoint(repo, locator, page);
            let response = session
                .get_conditional::<ApiJobsPage>(&endpoint, None)
                .map_err(|error| map_rest(error, "jobs page"))?;
            let conditional::ConditionalGet::Modified { value, metadata } = response else {
                return Err(invalid("jobs page"));
            };
            if value.jobs.len() > JOBS_PAGE_SIZE || value.total_count > MAX_JOBS {
                return Err(too_large("jobs page"));
            }
            if total_count
                .replace(value.total_count)
                .is_some_and(|prior| prior != value.total_count)
            {
                return Err(moved("jobs pagination"));
            }
            let mut page_jobs = Vec::with_capacity(value.jobs.len());
            for api_job in value.jobs {
                let job = map_job(repo, locator, api_job)?;
                if !seen_ids.insert(job.id) || !seen_nodes.insert(job.node_id.clone()) {
                    return Err(moved("jobs pagination"));
                }
                step_count = step_count
                    .checked_add(job.steps.len())
                    .ok_or_else(|| too_large("job steps"))?;
                if step_count > MAX_STEPS {
                    return Err(too_large("job steps"));
                }
                page_jobs.push(job);
            }
            check_operation(cancellation, deadline, "jobs page")?;
            let expected_total = total_count.unwrap_or_default();
            let more = jobs.len() + page_jobs.len() < expected_total;
            validate_link(metadata.link.as_deref(), repo, locator, page, more)?;
            if more && page_jobs.len() < JOBS_PAGE_SIZE {
                return Err(moved("jobs pagination"));
            }
            let identities = page_jobs.iter().map(job_fingerprint).collect::<Vec<_>>();
            jobs.extend(page_jobs);
            page_records.push(PageRecord {
                page,
                validators: metadata.validators,
                identities,
            });
            if jobs.len() == expected_total {
                break;
            }
            if jobs.len() > expected_total || page == MAX_JOB_PAGES {
                return Err(too_large("jobs pagination"));
            }
        }
        if jobs.len() != total_count.unwrap_or_default() {
            return Err(moved("jobs pagination"));
        }

        let mut offset = 0usize;
        let mut final_step_count = 0usize;
        for record in &page_records {
            check_cancel(cancellation, "jobs revalidation")?;
            let endpoint = jobs_endpoint(repo, locator, record.page);
            let response = session
                .get_conditional::<ApiJobsPage>(
                    &endpoint,
                    (!record.validators.is_empty()).then_some(&record.validators),
                )
                .map_err(|error| map_rest(error, "jobs revalidation"))?;
            match response {
                conditional::ConditionalGet::NotModified { .. } => {
                    final_step_count = add_steps(
                        final_step_count,
                        &jobs[offset..offset + record.identities.len()],
                    )?;
                }
                conditional::ConditionalGet::Modified { value, metadata } => {
                    if value.total_count != total_count.unwrap_or_default()
                        || value.jobs.len() != record.identities.len()
                    {
                        return Err(moved("jobs revalidation"));
                    }
                    let mut refreshed = Vec::with_capacity(value.jobs.len());
                    for api_job in value.jobs {
                        refreshed.push(map_job(repo, locator, api_job)?);
                    }
                    let identities = refreshed.iter().map(job_fingerprint).collect::<Vec<_>>();
                    if identities != record.identities {
                        return Err(moved("jobs revalidation"));
                    }
                    final_step_count = add_steps(final_step_count, &refreshed)?;
                    let more = offset + refreshed.len() < total_count.unwrap_or_default();
                    validate_link(metadata.link.as_deref(), repo, locator, record.page, more)?;
                    jobs[offset..offset + refreshed.len()].clone_from_slice(&refreshed);
                }
            }
            offset += record.identities.len();
            check_operation(cancellation, deadline, "jobs revalidation")?;
        }
        let final_run: ApiRunAttempt = modified(
            session.get_conditional(&run_endpoint, None),
            "run revalidation",
        )?;
        let final_attempt = validate_run(
            repo,
            &key,
            current_pull_request_head,
            final_run,
            observed_at,
        )?;
        if run_fingerprint(&final_attempt) != initial_run_fingerprint {
            return Err(moved("run revalidation"));
        }
        check_operation(cancellation, deadline, "run revalidation")?;
        let matches = jobs
            .iter()
            .filter(|job| job.check_run_database_id == locator.check_database_id)
            .map(|job| job.id)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(moved("selected check job join"));
        }
        let provider_ordered_job_ids = jobs.iter().map(|job| job.id).collect();
        jobs.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
                .then(left.id.cmp(&right.id))
        });
        check_operation(cancellation, deadline, "jobs install")?;
        Ok(ActionsJobsSnapshot {
            attempt: final_attempt,
            provider_ordered_job_ids,
            jobs,
            selected_check_job_id: matches[0],
            complete: true,
            observed_at_unix_ms: observed_at,
            observation_id: NEXT_OBSERVATION.fetch_add(1, Ordering::Relaxed),
        })
    }

    pub fn read_actions_job_log(
        &self,
        repo: &Repository,
        snapshot: &ActionsJobsSnapshot,
        selected_job_id: u64,
        cancellation: &ActionsCancellation,
    ) -> ActionsResult<ActionsJobLog> {
        if conditional::active_general_read_tracker().is_none() {
            return Err(invalid("account read admission"));
        }
        let started = Instant::now();
        let deadline = started
            .checked_add(LOG_OPERATION_TIMEOUT)
            .ok_or_else(|| timed_out("job log"))?;
        self.validate_repo(repo).map_err(|_| invalid("job log"))?;
        if !snapshot.complete
            || snapshot.attempt.key.locator.account != self.account
            || snapshot.attempt.key.locator.account != repo.account
        {
            return Err(moved("job log"));
        }
        let selected = snapshot
            .jobs
            .iter()
            .find(|job| job.id == selected_job_id)
            .cloned()
            .ok_or_else(|| moved("job selection"))?;
        check_cancel(cancellation, "job log")?;
        let mut session = Session::new_actions_until(self, cancellation.shared(), deadline);
        let viewer: ApiViewer = modified(session.get_conditional("user", None), "viewer identity")?;
        if viewer.login != snapshot.attempt.key.viewer_login
            || viewer.node_id != snapshot.attempt.key.viewer_node_id
        {
            return Err(moved("viewer identity"));
        }
        let job_endpoint = format!(
            "repos/{}/actions/jobs/{}",
            repo.full_name(),
            selected_job_id
        );
        let refreshed = map_job(
            repo,
            &snapshot.attempt.key.locator,
            modified(
                session.get_conditional(&job_endpoint, None),
                "job revalidation",
            )?,
        )?;
        if job_fingerprint(&refreshed) != job_fingerprint(&selected) {
            return Err(moved("job revalidation"));
        }
        if started.elapsed() >= LOG_OPERATION_TIMEOUT {
            return Err(timed_out("job log"));
        }

        let mut token = resolve_token(self, cancellation, deadline)?;
        let api_url = format!(
            "https://api.github.com/repos/{}/actions/jobs/{selected_job_id}/logs",
            repo.full_name()
        );
        let mut config = curl_config(&api_url, Some(&token))?;
        token.fill(0);
        let api_output = run_curl(
            &self.runner.curl,
            CurlKind::Api,
            &mut config,
            CURL_API_BODY_LIMIT + MAX_HEADER_BYTES + 1,
            cancellation,
            started,
        )?;
        conditional::record_general_poll_from_included_prefix(&api_output);
        let api_response = parse_http_response(api_output, CURL_API_BODY_LIMIT, "log redirect")?;
        if api_response.status != 302 {
            return Err(status_error(
                api_response.status,
                api_response.poll.rate_limit.is_some(),
                "log redirect",
            ));
        }
        let mut secret = SecretUrl::parse(
            api_response
                .single_header("location")?
                .ok_or_else(|| invalid("log redirect"))?,
        )?;
        drop(api_response);

        let mut redirects = 0usize;
        let raw_body = loop {
            check_cancel(cancellation, "log storage")?;
            if started.elapsed() >= LOG_OPERATION_TIMEOUT {
                return Err(timed_out("log storage"));
            }
            let mut config = curl_config(secret.expose(), None)?;
            secret.clear();
            let output = run_curl(
                &self.runner.curl,
                CurlKind::Storage,
                &mut config,
                CURL_LOG_OUTPUT_LIMIT,
                cancellation,
                started,
            )?;
            let response = parse_http_response(output, MAX_LOG_BYTES, "log storage")?;
            check_operation(cancellation, deadline, "log storage")?;
            match response.status {
                200 => {
                    validate_content_type(response.single_header("content-type")?)?;
                    if let Some(length) = response.single_header("content-length")? {
                        let declared = length
                            .parse::<usize>()
                            .map_err(|_| invalid("log storage"))?;
                        if declared != response.body.len() {
                            return Err(invalid("log storage"));
                        }
                    }
                    break response.body;
                }
                301 | 302 | 303 | 307 | 308 if redirects < MAX_STORAGE_REDIRECTS => {
                    secret = SecretUrl::parse(
                        response
                            .single_header("location")?
                            .ok_or_else(|| invalid("log storage redirect"))?,
                    )?;
                    redirects += 1;
                }
                301 | 302 | 303 | 307 | 308 => {
                    return Err(invalid("log storage redirect"));
                }
                status => return Err(status_error(status, false, "log storage")),
            }
        };
        let (sanitized_text, line_count) = sanitize_log(&raw_body)?;
        check_operation(cancellation, deadline, "job log install")?;
        Ok(ActionsJobLog {
            key: snapshot.attempt.key.clone(),
            job: refreshed,
            jobs_observation_id: snapshot.observation_id,
            raw_byte_count: raw_body.len(),
            line_count,
            sanitized_text,
            observed_at_unix_ms: now_unix_ms()?,
            provenance: ActionsLogProvenance::FreshExactRead,
        })
    }
}

fn coordinates_match(repo: &Repository, number: u64, check: &PullRequestCheck) -> bool {
    check.coordinates.provider == "github"
        && check.coordinates.host == repo.host
        && check.coordinates.owner == repo.owner
        && check.coordinates.repository == repo.name
        && check.coordinates.pull_request == number
        && !check.coordinates.remote_id.is_empty()
}

fn required_repository(
    value: Option<&CheckRepositoryIdentity>,
) -> ActionsResult<CheckRepositoryIdentity> {
    let value = value.ok_or_else(|| invalid("admission"))?;
    required_node(Some(&value.node_id))?;
    bounded_text(&value.name_with_owner, "admission")?;
    Ok(value.clone())
}

fn required_node(value: Option<&str>) -> ActionsResult<String> {
    let value = value.ok_or_else(|| invalid("identity"))?;
    if value.is_empty()
        || value.len() > 1_024
        || value.contains('\0')
        || value.chars().any(char::is_whitespace)
    {
        return Err(invalid("identity"));
    }
    Ok(value.to_owned())
}

fn required_sha(value: Option<&str>) -> ActionsResult<String> {
    let value = value.ok_or_else(|| invalid("identity"))?;
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("identity"));
    }
    Ok(value.to_ascii_lowercase())
}

fn positive_graphql_id(value: Option<u64>) -> ActionsResult<u64> {
    let value = value.ok_or_else(|| invalid("identity"))?;
    if value == 0 || value > i32::MAX as u64 {
        return Err(invalid("identity"));
    }
    Ok(value)
}

fn validate_workflow(
    value: &crate::domain::WorkflowRunIdentity,
    repository: &CheckRepositoryIdentity,
) -> ActionsResult<()> {
    required_node(Some(&value.node_id))?;
    required_node(Some(&value.workflow_node_id))?;
    positive_graphql_id(Some(value.database_id))?;
    positive_graphql_id(Some(value.workflow_database_id))?;
    if value.run_attempt == 0 || value.run_number == 0 {
        return Err(invalid("admission"));
    }
    bounded_text(&value.event, "admission")?;
    bounded_text(&value.workflow_name, "admission")?;
    let expected = format!(
        "https://github.com/{}/actions/runs/{}",
        repository.name_with_owner, value.database_id
    );
    if value.github_url != expected {
        return Err(moved("admission"));
    }
    Ok(())
}

fn bounded_text(value: &str, stage: &'static str) -> ActionsResult<String> {
    if value.is_empty()
        || value.len() > MAX_TEXT_BYTES
        || value.contains('\0')
        || value.chars().any(|character| character == '\r')
    {
        return Err(invalid(stage));
    }
    Ok(value.to_owned())
}

fn run_attempt_endpoint(repo: &Repository, locator: &ActionsAttemptLocator) -> String {
    format!(
        "repos/{}/actions/runs/{}/attempts/{}",
        repo.full_name(),
        locator.workflow_run.database_id,
        locator.workflow_run.run_attempt
    )
}

fn jobs_endpoint(repo: &Repository, locator: &ActionsAttemptLocator, page: usize) -> String {
    format!(
        "repos/{}/actions/runs/{}/attempts/{}/jobs?per_page={JOBS_PAGE_SIZE}&page={page}",
        repo.full_name(),
        locator.workflow_run.database_id,
        locator.workflow_run.run_attempt
    )
}

#[derive(Deserialize)]
struct ApiViewer {
    login: String,
    node_id: String,
}

#[derive(Clone, Deserialize)]
struct ApiRepositoryIdentity {
    node_id: String,
    full_name: String,
}

#[derive(Deserialize)]
struct ApiPullBranch {
    sha: String,
    repo: ApiRepositoryIdentity,
}

#[derive(Deserialize)]
struct ApiRunPullRequest {
    number: u64,
    base: ApiPullBranch,
    head: ApiPullBranch,
}

#[derive(Deserialize)]
struct ApiRunAttempt {
    id: u64,
    node_id: String,
    run_attempt: u64,
    run_number: u64,
    event: String,
    status: String,
    conclusion: Option<String>,
    workflow_id: u64,
    check_suite_id: u64,
    check_suite_node_id: String,
    head_sha: String,
    url: String,
    html_url: String,
    workflow_url: String,
    repository: ApiRepositoryIdentity,
    head_repository: Option<ApiRepositoryIdentity>,
    #[serde(default)]
    pull_requests: Vec<ApiRunPullRequest>,
}

#[derive(Deserialize)]
struct ApiJobsPage {
    total_count: usize,
    jobs: Vec<ApiJob>,
}

#[derive(Deserialize)]
struct ApiJobStep {
    name: String,
    status: String,
    conclusion: Option<String>,
    number: u64,
    started_at: Option<String>,
    completed_at: Option<String>,
}

#[derive(Deserialize)]
struct ApiJob {
    id: u64,
    node_id: String,
    run_id: u64,
    run_attempt: u64,
    head_sha: String,
    check_run_url: String,
    name: String,
    status: String,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    url: String,
    html_url: String,
    #[serde(default)]
    steps: Vec<ApiJobStep>,
}

fn validate_run(
    repo: &Repository,
    key: &ActionsAttemptKey,
    current_head: &str,
    value: ApiRunAttempt,
    observed_at: u64,
) -> ActionsResult<ActionsRunAttemptObservation> {
    let locator = &key.locator;
    let expected_run_url = format!(
        "https://api.github.com/repos/{}/actions/runs/{}/attempts/{}",
        repo.full_name(),
        locator.workflow_run.database_id,
        locator.workflow_run.run_attempt
    );
    let expected_workflow_url = format!(
        "https://api.github.com/repos/{}/actions/workflows/{}",
        repo.full_name(),
        locator.workflow_run.workflow_database_id
    );
    let expected_html = &locator.workflow_run.github_url;
    let expected_attempt_html = format!(
        "{expected_html}/attempts/{}",
        locator.workflow_run.run_attempt
    );
    if value.id != locator.workflow_run.database_id
        || value.node_id != locator.workflow_run.node_id
        || value.run_attempt != locator.workflow_run.run_attempt
        || value.run_number != locator.workflow_run.run_number
        || value.event != locator.workflow_run.event
        || value.workflow_id != locator.workflow_run.workflow_database_id
        || value.check_suite_id != positive_graphql_id(locator.suite.database_id)?
        || value.check_suite_node_id != locator.suite.node_id
        || required_sha(Some(&value.head_sha))? != locator.check_commit_sha
        || value.url != expected_run_url
        || (value.html_url != *expected_html && value.html_url != expected_attempt_html)
        || value.workflow_url != expected_workflow_url
        || value.repository.node_id != locator.base_repository.node_id
        || value.repository.full_name != locator.base_repository.name_with_owner
    {
        return Err(moved("run attempt"));
    }
    let Some(head_repository) = value.head_repository else {
        return Err(invalid("run attempt"));
    };
    if head_repository.node_id != locator.head_repository.node_id
        || head_repository.full_name != locator.head_repository.name_with_owner
    {
        return Err(moved("run attempt"));
    }
    let mut returned = Vec::with_capacity(value.pull_requests.len());
    let mut selected_relation = ActionsHeadRelation::Unknown;
    for pull in value.pull_requests {
        let identity = ActionsPullRequestIdentity {
            number: pull.number,
            base_repository: api_repository(pull.base.repo)?,
            base_sha: required_sha(Some(&pull.base.sha))?,
            head_repository: api_repository(pull.head.repo)?,
            head_sha: required_sha(Some(&pull.head.sha))?,
        };
        if identity.number == locator.pull_request_number {
            if identity.base_repository != locator.base_repository
                || identity.head_repository != locator.head_repository
                || identity.head_sha != locator.observed_head_sha
            {
                return Err(moved("run pull request"));
            }
            selected_relation = if current_head.is_empty() {
                ActionsHeadRelation::Unknown
            } else if current_head.eq_ignore_ascii_case(&locator.observed_head_sha) {
                ActionsHeadRelation::CurrentHead
            } else {
                ActionsHeadRelation::HistoricalHead
            };
        }
        returned.push(identity);
    }
    Ok(ActionsRunAttemptObservation {
        key: key.clone(),
        status: bounded_text(&value.status, "run attempt")?,
        conclusion: value
            .conclusion
            .map(|value| bounded_text(&value, "run attempt"))
            .transpose()?,
        api_url: value.url,
        html_url: value.html_url,
        workflow_url: value.workflow_url,
        returned_pull_requests: returned,
        relation: selected_relation,
        observed_at_unix_ms: observed_at,
    })
}

fn api_repository(value: ApiRepositoryIdentity) -> ActionsResult<CheckRepositoryIdentity> {
    Ok(CheckRepositoryIdentity {
        node_id: required_node(Some(&value.node_id))?,
        name_with_owner: bounded_text(&value.full_name, "run pull request")?,
    })
}

fn map_job(
    repo: &Repository,
    locator: &ActionsAttemptLocator,
    value: ApiJob,
) -> ActionsResult<ActionsJob> {
    if value.id == 0
        || value.run_id != locator.workflow_run.database_id
        || value.run_attempt != locator.workflow_run.run_attempt
        || required_sha(Some(&value.head_sha))? != locator.check_commit_sha
    {
        return Err(moved("job identity"));
    }
    if value.steps.len() > MAX_STEPS_PER_JOB {
        return Err(too_large("job steps"));
    }
    let expected_api = format!(
        "https://api.github.com/repos/{}/actions/jobs/{}",
        repo.full_name(),
        value.id
    );
    if value.url != expected_api {
        return Err(moved("job identity"));
    }
    let check_run_database_id = parse_check_run_url(repo, &value.check_run_url)?;
    let steps = value
        .steps
        .into_iter()
        .map(|step| {
            if step.number == 0 {
                return Err(invalid("job step"));
            }
            Ok(ActionsJobStep {
                number: step.number,
                name: bounded_text(&step.name, "job step")?,
                status: bounded_text(&step.status, "job step")?,
                conclusion: step
                    .conclusion
                    .map(|value| bounded_text(&value, "job step"))
                    .transpose()?,
                started_at: step
                    .started_at
                    .map(|value| bounded_text(&value, "job step"))
                    .transpose()?,
                completed_at: step
                    .completed_at
                    .map(|value| bounded_text(&value, "job step"))
                    .transpose()?,
            })
        })
        .collect::<ActionsResult<Vec<_>>>()?;
    Ok(ActionsJob {
        id: value.id,
        node_id: required_node(Some(&value.node_id))?,
        run_id: value.run_id,
        run_attempt: value.run_attempt,
        head_sha: value.head_sha.to_ascii_lowercase(),
        check_run_database_id,
        check_run_url: value.check_run_url,
        name: bounded_text(&value.name, "job")?,
        status: bounded_text(&value.status, "job")?,
        conclusion: value
            .conclusion
            .map(|value| bounded_text(&value, "job"))
            .transpose()?,
        started_at: value
            .started_at
            .map(|value| bounded_text(&value, "job"))
            .transpose()?,
        completed_at: value
            .completed_at
            .map(|value| bounded_text(&value, "job"))
            .transpose()?,
        api_url: value.url,
        html_url: bounded_text(&value.html_url, "job")?,
        steps,
    })
}

fn parse_check_run_url(repo: &Repository, value: &str) -> ActionsResult<u64> {
    let prefix = format!(
        "https://api.github.com/repos/{}/check-runs/",
        repo.full_name()
    );
    let suffix = value
        .strip_prefix(&prefix)
        .filter(|suffix| !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| moved("check run URL"))?;
    let id = suffix
        .parse::<u64>()
        .map_err(|_| invalid("check run URL"))?;
    positive_graphql_id(Some(id))
}

type JobFingerprint = (u64, String, u64, u64, String, u64, String, String);

fn job_fingerprint(job: &ActionsJob) -> JobFingerprint {
    (
        job.id,
        job.node_id.clone(),
        job.run_id,
        job.run_attempt,
        job.head_sha.clone(),
        job.check_run_database_id,
        job.api_url.clone(),
        job.check_run_url.clone(),
    )
}

fn add_steps(prior: usize, jobs: &[ActionsJob]) -> ActionsResult<usize> {
    let count = jobs.iter().try_fold(prior, |count, job| {
        count
            .checked_add(job.steps.len())
            .ok_or_else(|| too_large("job steps"))
    })?;
    if count > MAX_STEPS {
        Err(too_large("job steps"))
    } else {
        Ok(count)
    }
}

type RunFingerprint = (
    ActionsAttemptKey,
    String,
    String,
    String,
    Vec<ActionsPullRequestIdentity>,
);

fn run_fingerprint(value: &ActionsRunAttemptObservation) -> RunFingerprint {
    (
        value.key.clone(),
        value.api_url.clone(),
        value.html_url.clone(),
        value.workflow_url.clone(),
        value.returned_pull_requests.clone(),
    )
}

struct PageRecord {
    page: usize,
    validators: conditional::RestValidators,
    identities: Vec<JobFingerprint>,
}

fn validate_link(
    value: Option<&str>,
    repo: &Repository,
    locator: &ActionsAttemptLocator,
    page: usize,
    more: bool,
) -> ActionsResult<()> {
    let next = value.and_then(|value| {
        value.split(',').find_map(|part| {
            let part = part.trim();
            let (url, relation) = part.split_once(';')?;
            (relation.trim() == "rel=\"next\"").then_some(url.trim())
        })
    });
    match (more, next) {
        (false, None) => Ok(()),
        (true, Some(next)) => {
            let expected = format!(
                "<https://api.github.com/repos/{}/actions/runs/{}/attempts/{}/jobs?per_page={JOBS_PAGE_SIZE}&page={}>",
                repo.full_name(),
                locator.workflow_run.database_id,
                locator.workflow_run.run_attempt,
                page + 1
            );
            if next == expected {
                Ok(())
            } else {
                Err(moved("jobs pagination link"))
            }
        }
        _ => Err(moved("jobs pagination link")),
    }
}

fn modified<T>(
    response: Result<conditional::ConditionalGet<T>, conditional::RestReadError>,
    stage: &'static str,
) -> ActionsResult<T> {
    match response.map_err(|error| map_rest(error, stage))? {
        conditional::ConditionalGet::Modified { value, .. } => Ok(value),
        conditional::ConditionalGet::NotModified { .. } => Err(invalid(stage)),
    }
}

fn map_rest(error: conditional::RestReadError, stage: &'static str) -> ActionsReadError {
    let rate = error.poll().rate_limit.is_some();
    match error.http_status() {
        Some(status) => status_error(status, rate, stage),
        None if rate => ActionsReadError::new(ActionsReadErrorCategory::RateLimited, stage),
        None => ActionsReadError::new(
            match error.class() {
                conditional::RestReadErrorClass::Credential => ActionsReadErrorCategory::Credential,
                conditional::RestReadErrorClass::Cancelled => ActionsReadErrorCategory::Cancelled,
                conditional::RestReadErrorClass::TimedOut => ActionsReadErrorCategory::TimedOut,
                conditional::RestReadErrorClass::OperationLimit => {
                    ActionsReadErrorCategory::TooLarge
                }
                conditional::RestReadErrorClass::InvalidResponse => {
                    ActionsReadErrorCategory::InvalidResponse
                }
                conditional::RestReadErrorClass::Transport
                | conditional::RestReadErrorClass::HttpFailure
                | conditional::RestReadErrorClass::RateDeferred => {
                    ActionsReadErrorCategory::Transport
                }
            },
            stage,
        ),
    }
}

fn status_error(status: u16, rate: bool, stage: &'static str) -> ActionsReadError {
    let category = if rate || status == 429 {
        ActionsReadErrorCategory::RateLimited
    } else {
        match status {
            401 => ActionsReadErrorCategory::Credential,
            403 => ActionsReadErrorCategory::Permission,
            404 | 410 => ActionsReadErrorCategory::Unavailable,
            409 | 422 => ActionsReadErrorCategory::NotReady,
            _ => ActionsReadErrorCategory::InvalidResponse,
        }
    };
    ActionsReadError::new(category, stage)
}

struct SecretUrl(Vec<u8>);

impl SecretUrl {
    fn parse(value: &str) -> ActionsResult<Self> {
        if value.is_empty()
            || value.len() > MAX_LOCATION_BYTES
            || value.contains('#')
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return Err(invalid("log storage location"));
        }
        let rest = value
            .strip_prefix("https://")
            .ok_or_else(|| invalid("log storage location"))?;
        let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        let path_query = &rest[authority_end..];
        if authority.is_empty()
            || authority.contains('@')
            || authority.contains(':')
            || authority.starts_with('[')
            || path_query.len() > MAX_SIGNED_PATH_QUERY_BYTES
            || authority != authority.to_ascii_lowercase()
            || !valid_dns_name(authority)
            || !allowed_storage_host(authority)
        {
            return Err(ActionsReadError::new(
                ActionsReadErrorCategory::UnsupportedStorageHost,
                "log storage location",
            ));
        }
        Ok(Self(value.as_bytes().to_vec()))
    }

    fn expose(&self) -> &[u8] {
        &self.0
    }

    fn clear(&mut self) {
        self.0.fill(0);
    }
}

impl Drop for SecretUrl {
    fn drop(&mut self) {
        self.clear();
    }
}

impl fmt::Display for SecretUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted signed URL]")
    }
}

impl fmt::Debug for SecretUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretUrl([redacted])")
    }
}

struct SensitiveBuffer(Vec<u8>);

impl Drop for SensitiveBuffer {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

fn valid_dns_name(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

fn allowed_storage_host(host: &str) -> bool {
    ["actions.githubusercontent.com", "blob.core.windows.net"]
        .into_iter()
        .any(|suffix| {
            host.len() > suffix.len()
                && host.ends_with(suffix)
                && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
        })
}

fn resolve_token(
    provider: &GithubProvider,
    cancellation: &ActionsCancellation,
    deadline: Instant,
) -> ActionsResult<Vec<u8>> {
    let mut command = provider.runner.gh_command();
    command.args([
        "auth",
        "token",
        "--hostname",
        &provider.account.host,
        "--user",
        &provider.account.login,
    ]);
    let output = provider
        .runner
        .run_with_status_maybe_cancelled(
            command,
            "resolve selected GitHub credential",
            Some(&cancellation.0),
            Some(deadline),
        )
        .map_err(|error| {
            ActionsReadError::new(
                match error.kind {
                    RunnerFailureKind::Cancelled => ActionsReadErrorCategory::Cancelled,
                    RunnerFailureKind::TimedOut => ActionsReadErrorCategory::TimedOut,
                    RunnerFailureKind::Start
                    | RunnerFailureKind::Io
                    | RunnerFailureKind::OutputLimit => ActionsReadErrorCategory::Credential,
                },
                "log credential",
            )
        })?;
    if !output.status.success() {
        return Err(ActionsReadError::new(
            ActionsReadErrorCategory::Credential,
            "log credential",
        ));
    }
    let output = output.stdout;
    let start = output
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(output.len());
    let end = output
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |position| position + 1);
    let token = output[start..end].to_vec();
    if token.is_empty()
        || token.len() > 4_096
        || token
            .iter()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(ActionsReadError::new(
            ActionsReadErrorCategory::Credential,
            "log credential",
        ));
    }
    Ok(token)
}

fn curl_config(url: impl AsRef<[u8]>, token: Option<&[u8]>) -> ActionsResult<Vec<u8>> {
    let url = url.as_ref();
    if url.is_empty() || url.len() > MAX_SIGNED_PATH_QUERY_BYTES + 512 {
        return Err(invalid("curl input"));
    }
    let mut config = Vec::with_capacity(url.len() + token.map_or(0, |token| token.len()) + 64);
    config.extend_from_slice(b"url = \"");
    curl_escape(url, &mut config)?;
    config.extend_from_slice(b"\"\n");
    if let Some(token) = token {
        config.extend_from_slice(b"header = \"Authorization: Bearer ");
        curl_escape(token, &mut config)?;
        config.extend_from_slice(b"\"\n");
    }
    Ok(config)
}

fn curl_escape(value: &[u8], output: &mut Vec<u8>) -> ActionsResult<()> {
    for byte in value {
        match byte {
            b'\\' | b'\"' => {
                output.push(b'\\');
                output.push(*byte);
            }
            byte if byte.is_ascii_control() => return Err(invalid("curl input")),
            byte => output.push(*byte),
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum CurlKind {
    Api,
    Storage,
}

fn run_curl(
    executable: &std::path::Path,
    kind: CurlKind,
    config: &mut Vec<u8>,
    output_limit: usize,
    cancellation: &ActionsCancellation,
    operation_started: Instant,
) -> ActionsResult<Vec<u8>> {
    check_cancel(cancellation, "curl transport")?;
    if operation_started.elapsed() >= LOG_OPERATION_TIMEOUT {
        return Err(timed_out("curl transport"));
    }
    let mut command = Command::new(executable);
    command.args([
        "-q",
        "--config",
        "-",
        "--silent",
        "--show-error",
        "--include",
        "--request",
        "GET",
        "--no-location",
        "--no-netrc",
        "--disallow-username-in-url",
        "--proto",
        "=https",
        "--noproxy",
        "*",
        "--connect-timeout",
        "10",
        "--max-time",
        "30",
        "--header",
        "Accept-Encoding: identity",
        "--max-filesize",
    ]);
    match kind {
        CurlKind::Api => {
            command.arg(CURL_API_BODY_LIMIT.to_string()).args([
                "--header",
                "Accept: application/vnd.github+json",
                "--header",
                API_VERSION,
            ]);
        }
        CurlKind::Storage => {
            command.arg(MAX_LOG_BYTES.to_string());
        }
    }
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let child_started = Instant::now();
    let mut child = command.spawn().map_err(|_| {
        ActionsReadError::new(ActionsReadErrorCategory::Transport, "curl transport")
    })?;
    let Some(mut stdin) = child.stdin.take() else {
        terminate_process_group(&mut child);
        config.fill(0);
        return Err(ActionsReadError::new(
            ActionsReadErrorCategory::Transport,
            "curl transport",
        ));
    };
    let mut private_input = SensitiveBuffer(std::mem::take(config));
    let (tx, rx) = mpsc::channel();
    let input_tx = tx.clone();
    if thread::Builder::new()
        .name("actions-curl-input".into())
        .spawn(move || {
            let result = stdin.write_all(&private_input.0);
            private_input.0.fill(0);
            let _ = input_tx.send(CurlPipeEvent::Input(result));
        })
        .is_err()
    {
        terminate_process_group(&mut child);
        return Err(ActionsReadError::new(
            ActionsReadErrorCategory::Transport,
            "curl transport",
        ));
    }
    let Some(stdout) = child.stdout.take() else {
        terminate_process_group(&mut child);
        return Err(ActionsReadError::new(
            ActionsReadErrorCategory::Transport,
            "curl transport",
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_process_group(&mut child);
        return Err(ActionsReadError::new(
            ActionsReadErrorCategory::Transport,
            "curl transport",
        ));
    };
    for (is_stdout, mut pipe) in [
        (true, Box::new(stdout) as Box<dyn Read + Send>),
        (false, Box::new(stderr) as Box<dyn Read + Send>),
    ] {
        let output_tx = tx.clone();
        if thread::Builder::new()
            .name("actions-curl-output".into())
            .spawn(move || {
                let mut bytes = Vec::new();
                let mut prefix_sent = false;
                let result = loop {
                    let mut chunk = [0u8; 8 * 1024];
                    match pipe.read(&mut chunk) {
                        Ok(0) => {
                            if is_stdout && matches!(kind, CurlKind::Api) && !prefix_sent {
                                let _ = output_tx.send(CurlPipeEvent::ApiPrefix(
                                    bytes[..bytes.len().min(MAX_HEADER_BYTES)].to_vec(),
                                ));
                            }
                            break Ok(bytes);
                        }
                        Ok(read) => {
                            let remaining =
                                output_limit.saturating_add(1).saturating_sub(bytes.len());
                            bytes.extend_from_slice(&chunk[..read.min(remaining)]);
                            if is_stdout && matches!(kind, CurlKind::Api) && !prefix_sent {
                                let prefix = bytes[..bytes.len().min(MAX_HEADER_BYTES)].to_vec();
                                if find_header_end(&bytes).is_some()
                                    || bytes.len() >= MAX_HEADER_BYTES
                                {
                                    prefix_sent = true;
                                    let _ = output_tx.send(CurlPipeEvent::ApiPrefix(prefix));
                                } else {
                                    let _ = output_tx.send(CurlPipeEvent::ApiProgress(prefix));
                                }
                            }
                            if bytes.len() > output_limit {
                                break Ok(bytes);
                            }
                        }
                        Err(error) => {
                            if is_stdout && matches!(kind, CurlKind::Api) && !prefix_sent {
                                let _ = output_tx.send(CurlPipeEvent::ApiPrefix(
                                    bytes[..bytes.len().min(MAX_HEADER_BYTES)].to_vec(),
                                ));
                            }
                            break Err(error);
                        }
                    }
                };
                let _ = output_tx.send(CurlPipeEvent::Output(is_stdout, result));
            })
            .is_err()
        {
            terminate_process_group(&mut child);
            return Err(ActionsReadError::new(
                ActionsReadErrorCategory::Transport,
                "curl transport",
            ));
        }
    }
    drop(tx);
    let mut stdout = None;
    let mut input_done = false;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut status = None;
    let mut api_prefix = Vec::new();
    loop {
        while let Ok(event) = rx.try_recv() {
            match event {
                CurlPipeEvent::Input(result) => {
                    if result.is_err() {
                        terminate_curl_with_poll(&mut child, &rx, kind, &mut api_prefix);
                        return Err(ActionsReadError::new(
                            ActionsReadErrorCategory::Transport,
                            "curl transport",
                        ));
                    }
                    input_done = true;
                }
                CurlPipeEvent::Output(is_stdout, result) => {
                    let bytes = match result {
                        Ok(bytes) => bytes,
                        Err(_) => {
                            terminate_curl_with_poll(&mut child, &rx, kind, &mut api_prefix);
                            return Err(ActionsReadError::new(
                                ActionsReadErrorCategory::Transport,
                                "curl transport",
                            ));
                        }
                    };
                    if bytes.len() > output_limit {
                        if is_stdout && matches!(kind, CurlKind::Api) {
                            api_prefix = bytes[..bytes.len().min(MAX_HEADER_BYTES)].to_vec();
                        }
                        terminate_curl_with_poll(&mut child, &rx, kind, &mut api_prefix);
                        return Err(too_large("curl transport"));
                    }
                    if is_stdout {
                        stdout = Some(bytes);
                        stdout_done = true;
                    } else {
                        stderr_done = true;
                    }
                }
                CurlPipeEvent::ApiPrefix(prefix) => {
                    api_prefix = prefix.clone();
                    conditional::record_general_poll_from_included_prefix(&prefix);
                }
                CurlPipeEvent::ApiProgress(prefix) => api_prefix = prefix,
            }
        }
        if cancellation.is_cancelled() {
            terminate_curl_with_poll(&mut child, &rx, kind, &mut api_prefix);
            return Err(cancelled("curl transport"));
        }
        if operation_started.elapsed() >= LOG_OPERATION_TIMEOUT {
            terminate_curl_with_poll(&mut child, &rx, kind, &mut api_prefix);
            return Err(timed_out("curl transport"));
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(current)) => status = Some(current),
                Ok(None) => {}
                Err(_) => {
                    terminate_curl_with_poll(&mut child, &rx, kind, &mut api_prefix);
                    return Err(ActionsReadError::new(
                        ActionsReadErrorCategory::Transport,
                        "curl transport",
                    ));
                }
            }
        }
        if let Some(status) = status
            && input_done
            && stdout_done
            && stderr_done
        {
            if !status.success() {
                return Err(if status.code() == Some(63) {
                    too_large("curl transport")
                } else {
                    ActionsReadError::new(ActionsReadErrorCategory::Transport, "curl transport")
                });
            }
            return Ok(stdout.unwrap_or_default());
        }
        if child_started.elapsed() >= CHILD_TIMEOUT {
            terminate_curl_with_poll(&mut child, &rx, kind, &mut api_prefix);
            return Err(timed_out("curl transport"));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

enum CurlPipeEvent {
    Input(std::io::Result<()>),
    Output(bool, std::io::Result<Vec<u8>>),
    ApiPrefix(Vec<u8>),
    ApiProgress(Vec<u8>),
}

fn terminate_curl_with_poll(
    child: &mut Child,
    receiver: &mpsc::Receiver<CurlPipeEvent>,
    kind: CurlKind,
    prefix: &mut Vec<u8>,
) {
    terminate_process_group(child);
    if !matches!(kind, CurlKind::Api) {
        return;
    }
    let drain_until = Instant::now().checked_add(Duration::from_millis(50));
    loop {
        match receiver.try_recv() {
            Ok(CurlPipeEvent::ApiPrefix(value) | CurlPipeEvent::ApiProgress(value)) => {
                *prefix = value;
            }
            Ok(CurlPipeEvent::Output(true, Ok(value))) => {
                *prefix = value[..value.len().min(MAX_HEADER_BYTES)].to_vec();
            }
            Ok(_) => {}
            Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty)
                if drain_until.is_some_and(|deadline| Instant::now() < deadline) =>
            {
                thread::sleep(Duration::from_millis(1));
            }
            Err(mpsc::TryRecvError::Empty) => break,
        }
    }
    conditional::record_general_poll_from_included_prefix(prefix);
}

struct ParsedHttp {
    status: u16,
    fields: Vec<(String, String)>,
    body: Vec<u8>,
    poll: conditional::RestPollDirective,
}

impl ParsedHttp {
    fn single_header(&self, name: &str) -> ActionsResult<Option<&str>> {
        let mut found = None;
        for (_, value) in self.fields.iter().filter(|(field, _)| field == name) {
            if found.is_some_and(|prior| prior != value) {
                return Err(invalid("response headers"));
            }
            found = Some(value.as_str());
        }
        Ok(found)
    }
}

fn parse_http_response(
    output: Vec<u8>,
    body_limit: usize,
    stage: &'static str,
) -> ActionsResult<ParsedHttp> {
    let (header_end, delimiter) = find_header_end(&output).ok_or_else(|| invalid(stage))?;
    if header_end > MAX_HEADER_BYTES {
        return Err(too_large(stage));
    }
    let header = &output[..header_end];
    let body = &output[header_end + delimiter..];
    if body.len() > body_limit {
        return Err(too_large(stage));
    }
    let mut lines = header.split(|byte| *byte == b'\n');
    let status_line = lines.next().ok_or_else(|| invalid(stage))?;
    let status_line = status_line.strip_suffix(b"\r").unwrap_or(status_line);
    let status_text = std::str::from_utf8(status_line).map_err(|_| invalid(stage))?;
    let mut parts = status_text.split_ascii_whitespace();
    let version = parts.next().unwrap_or_default();
    let code = parts.next().unwrap_or_default();
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1" | "HTTP/2" | "HTTP/2.0")
        || code.len() != 3
        || !code.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid(stage));
    }
    let status = code.parse::<u16>().map_err(|_| invalid(stage))?;
    let mut fields = Vec::new();
    for line in lines {
        if fields.len() == MAX_HEADER_FIELDS {
            return Err(too_large(stage));
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line.starts_with(b" ") || line.starts_with(b"\t") || line.contains(&0)
        {
            return Err(invalid(stage));
        }
        let line = std::str::from_utf8(line).map_err(|_| invalid(stage))?;
        let (name, value) = line.split_once(':').ok_or_else(|| invalid(stage))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(invalid(stage));
        }
        fields.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let poll = conditional::general_poll_from_included_prefix(&output[..header_end + delimiter]);
    Ok(ParsedHttp {
        status,
        fields,
        body: body.to_vec(),
        poll,
    })
}

fn find_header_end(output: &[u8]) -> Option<(usize, usize)> {
    output
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .map(|position| (position, 4))
        .or_else(|| {
            output
                .windows(2)
                .position(|bytes| bytes == b"\n\n")
                .map(|position| (position, 2))
        })
}

fn validate_content_type(value: Option<&str>) -> ActionsResult<()> {
    let Some(value) = value else { return Ok(()) };
    let media_type = value.split(';').next().unwrap_or_default().trim();
    if matches!(media_type, "text/plain" | "application/octet-stream") {
        Ok(())
    } else {
        Err(invalid("log content type"))
    }
}

fn sanitize_log(raw: &[u8]) -> ActionsResult<(String, usize)> {
    if raw.len() > MAX_LOG_BYTES {
        return Err(too_large("job log"));
    }
    if raw.contains(&0) {
        return Err(invalid("job log"));
    }
    let decoded = String::from_utf8_lossy(raw);
    let mut output = String::with_capacity(decoded.len().min(MAX_LOG_BYTES));
    let mut line_bytes = 0usize;
    let mut line_count: usize = 0;
    let mut chars = decoded.chars().peekable();
    while let Some(character) = chars.next() {
        let character = if character == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            '\n'
        } else {
            character
        };
        if character == '\n' {
            line_bytes = 0;
            line_count = line_count.saturating_add(1);
            if line_count > MAX_LOG_LINES {
                return Err(too_large("job log lines"));
            }
            output.push('\n');
            continue;
        }
        let visible = if character == '\t'
            || !(character <= '\u{001f}' || ('\u{007f}'..='\u{009f}').contains(&character))
        {
            character
        } else {
            '\u{fffd}'
        };
        line_bytes = line_bytes.saturating_add(visible.len_utf8());
        if line_bytes > MAX_LOG_LINE_BYTES {
            return Err(too_large("job log line"));
        }
        if output.len().saturating_add(visible.len_utf8()) > MAX_LOG_BYTES * 3 {
            return Err(too_large("job log decode"));
        }
        output.push(visible);
    }
    if !output.is_empty() && !output.ends_with('\n') {
        line_count = line_count.saturating_add(1);
    }
    if line_count > MAX_LOG_LINES {
        return Err(too_large("job log lines"));
    }
    Ok((output, line_count))
}

fn check_cancel(cancellation: &ActionsCancellation, stage: &'static str) -> ActionsResult<()> {
    if cancellation.is_cancelled() {
        Err(cancelled(stage))
    } else {
        Ok(())
    }
}

fn check_operation(
    cancellation: &ActionsCancellation,
    deadline: Instant,
    stage: &'static str,
) -> ActionsResult<()> {
    check_cancel(cancellation, stage)?;
    if Instant::now() >= deadline {
        Err(timed_out(stage))
    } else {
        Ok(())
    }
}

fn now_unix_ms() -> ActionsResult<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("observation clock"))?
        .as_millis();
    u64::try_from(millis).map_err(|_| invalid("observation clock"))
}

fn invalid(stage: &'static str) -> ActionsReadError {
    ActionsReadError::new(ActionsReadErrorCategory::InvalidResponse, stage)
}

fn moved(stage: &'static str) -> ActionsReadError {
    ActionsReadError::new(ActionsReadErrorCategory::MovedIdentity, stage)
}

fn too_large(stage: &'static str) -> ActionsReadError {
    ActionsReadError::new(ActionsReadErrorCategory::TooLarge, stage)
}

fn timed_out(stage: &'static str) -> ActionsReadError {
    ActionsReadError::new(ActionsReadErrorCategory::TimedOut, stage)
}

fn cancelled(stage: &'static str) -> ActionsReadError {
    ActionsReadError::new(ActionsReadErrorCategory::Cancelled, stage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Account, ActionsLinkage, CheckSuiteIdentity, ProviderCoordinates, WorkflowRunIdentity,
    };
    use serde_json::json;
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};
    use tempfile::TempDir;

    fn executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn repository() -> Repository {
        Repository {
            host: "github.com".into(),
            owner: "owner".into(),
            name: "repo".into(),
            account: Account {
                host: "github.com".into(),
                login: "alice".into(),
            },
            local_path: None,
        }
    }

    fn details_and_check() -> (PullRequestDetails, PullRequestCheck) {
        let identity = CheckRepositoryIdentity {
            node_id: "REPO_node".into(),
            name_with_owner: "owner/repo".into(),
        };
        let check = PullRequestCheck {
            coordinates: ProviderCoordinates {
                provider: "github".into(),
                host: "github.com".into(),
                owner: "owner".into(),
                repository: "repo".into(),
                pull_request: 7,
                remote_id: "CHECK_node".into(),
            },
            kind: CheckKind::CheckRun,
            name: "CI".into(),
            status: "COMPLETED".into(),
            conclusion: Some("SUCCESS".into()),
            description: None,
            details_url: Some("https://untrusted.example/build".into()),
            github_permalink: Some("https://github.com/owner/repo/runs/9".into()),
            started_at: None,
            completed_at: None,
            required: Some(true),
            database_id: Some(9),
            suite: Some(CheckSuiteIdentity {
                node_id: "SUITE_node".into(),
                database_id: Some(8),
                repository: identity.clone(),
                app: None,
            }),
            commit_sha: Some("a".repeat(40)),
            commit_repository: Some(identity.clone()),
            sha_class: CheckShaClass::Head,
            actions_linkage: ActionsLinkage::Linked(WorkflowRunIdentity {
                node_id: "RUN_node".into(),
                database_id: 6,
                run_attempt: 2,
                run_number: 5,
                event: "pull_request".into(),
                github_url: "https://github.com/owner/repo/actions/runs/6".into(),
                workflow_node_id: "WORKFLOW_node".into(),
                workflow_database_id: 4,
                workflow_name: "CI".into(),
            }),
        };
        let details = PullRequestDetails {
            number: 7,
            pull_request_node_id: Some("PR_node".into()),
            base_repository: Some(identity.clone()),
            observed_head_sha: Some("a".repeat(40)),
            rollup_commit_sha: Some("b".repeat(40)),
            potential_merge_commit_sha: None,
            head_repository: Some(identity.clone()),
            rollup_repository: Some(identity.clone()),
            potential_merge_commit_repository: None,
            body: String::new(),
            requested_reviewers: vec![],
            labels: vec![],
            assignees: vec![],
            merge_eligibility: crate::domain::MergeEligibility {
                state: "OPEN".into(),
                draft: false,
                mergeable: "MERGEABLE".into(),
                merge_state_status: "CLEAN".into(),
                review_status: "APPROVED".into(),
                check_status: "SUCCESS".into(),
                maintainer_can_modify: false,
                can_rebase: false,
                can_update_branch: false,
                auto_merge_enabled: false,
                in_merge_queue: false,
            },
            issue_comments: vec![],
            reviews: vec![],
            review_threads: vec![],
            reactions: vec![],
            checks: vec![check.clone()],
            participant_avatars: Default::default(),
            activity_complete: true,
            checks_complete: true,
            notice: None,
        };
        (details, check)
    }

    fn included_json(value: &serde_json::Value, headers: &str) -> Vec<u8> {
        let mut output = format!("HTTP/1.1 200 OK\r\n{headers}\r\n").into_bytes();
        output.extend(serde_json::to_vec(value).unwrap());
        output
    }

    #[test]
    fn exact_attempt_enumerates_101_jobs_revalidates_every_page_and_uses_get_only() {
        let temp = TempDir::new().unwrap();
        let user = temp.path().join("user");
        let run = temp.path().join("run");
        let page1 = temp.path().join("page1");
        let page2 = temp.path().join("page2");
        let ledger = temp.path().join("ledger");
        fs::write(
            &user,
            included_json(
                &json!({"login":"alice","node_id":"VIEWER_node"}),
                "ETag: \"u\"\r\n",
            ),
        )
        .unwrap();
        fs::write(
            &run,
            included_json(
                &json!({
                    "id":6,"node_id":"RUN_node","run_attempt":2,"run_number":5,
                    "event":"pull_request","status":"completed","conclusion":"success",
                    "workflow_id":4,"check_suite_id":8,"check_suite_node_id":"SUITE_node",
                    "head_sha":"a".repeat(40),
                    "url":"https://api.github.com/repos/owner/repo/actions/runs/6/attempts/2",
                    "html_url":"https://github.com/owner/repo/actions/runs/6/attempts/2",
                    "workflow_url":"https://api.github.com/repos/owner/repo/actions/workflows/4",
                    "repository":{"node_id":"REPO_node","full_name":"owner/repo"},
                    "head_repository":{"node_id":"REPO_node","full_name":"owner/repo"},
                    "pull_requests":[]
                }),
                "ETag: \"r\"\r\n",
            ),
        )
        .unwrap();
        let job = |id: u64, check_id: u64| {
            json!({
                "id":id,"node_id":format!("JOB_{id}"),"run_id":6,"run_attempt":2,
                "head_sha":"a".repeat(40),
                "check_run_url":format!("https://api.github.com/repos/owner/repo/check-runs/{check_id}"),
                "name":format!("job {id:04}"),"status":"completed","conclusion":"success",
                "started_at":"2026-09-14T00:00:00Z","completed_at":"2026-09-14T00:01:00Z",
                "url":format!("https://api.github.com/repos/owner/repo/actions/jobs/{id}"),
                "html_url":format!("https://github.com/owner/repo/actions/runs/6/job/{id}"),
                "steps":[]
            })
        };
        let first = (0..100)
            .map(|index| job(1_000 + index, if index == 40 { 9 } else { 10_000 + index }))
            .collect::<Vec<_>>();
        let link = "Link: <https://api.github.com/repos/owner/repo/actions/runs/6/attempts/2/jobs?per_page=100&page=2>; rel=\"next\"\r\nETag: \"p1\"\r\n";
        fs::write(
            &page1,
            included_json(&json!({"total_count":101,"jobs":first}), link),
        )
        .unwrap();
        fs::write(
            &page2,
            included_json(
                &json!({"total_count":101,"jobs":[job(2_000, 20_000)]}),
                "ETag: \"p2\"\r\n",
            ),
        )
        .unwrap();
        let gh = temp.path().join("gh");
        executable(
            &gh,
            &format!(
                "#!/bin/sh\ncase \"$1 $2\" in\n  'auth token') printf '%s\\n' 'fixture-token'; exit 0;;\nesac\nprintf '%s\\n' \"$*\" >> '{}'\nendpoint=''\nfor arg do endpoint=\"$arg\"; done\ncase \"$*\" in\n  *If-None-Match*) printf 'HTTP/1.1 304 Not Modified\\r\\nETag: \"same\"\\r\\n\\r\\n';;\n  *'&page=2'*) /bin/cat '{}';;\n  *'&page=1'*) /bin/cat '{}';;\n  *' user') /bin/cat '{}';;\n  *) /bin/cat '{}';;\nesac\n",
                ledger.display(),
                page2.display(),
                page1.display(),
                user.display(),
                run.display()
            ),
        );
        let repo = repository();
        let provider =
            GithubProvider::synthetic_with_gh(repo.account.clone(), gh, Duration::from_secs(2));
        let (details, check) = details_and_check();
        let locator = provider
            .actions_attempt_locator(&repo, &details, &check)
            .unwrap();
        let outcome = provider.general_read(|provider| {
            provider
                .read_actions_jobs(
                    &repo,
                    &locator,
                    &"c".repeat(40),
                    &ActionsCancellation::new(),
                )
                .map_err(anyhow::Error::new)
        });
        let snapshot = outcome.result().as_ref().unwrap();
        assert_eq!(snapshot.jobs.len(), 101);
        assert_eq!(snapshot.selected_check_job_id, 1_040);
        assert_ne!(snapshot.selected_check_job_id, locator.check_database_id);
        assert_eq!(snapshot.attempt.relation, ActionsHeadRelation::Unknown);
        assert!(snapshot.attempt.returned_pull_requests.is_empty());
        assert_eq!(snapshot.provider_ordered_job_ids[40], 1_040);
        let ledger = fs::read_to_string(ledger).unwrap();
        assert_eq!(ledger.lines().count(), 7);
        assert!(ledger.lines().all(|line| line.contains("--method GET")));
        for forbidden in ["POST", "PUT", "PATCH", "DELETE", "untrusted.example"] {
            assert!(!ledger.contains(forbidden));
        }
    }

    fn fake_curl(temp: &TempDir, body: &str) -> std::path::PathBuf {
        let path = temp.path().join("curl");
        executable(&path, body);
        path
    }

    #[test]
    fn fake_child_nonzero_retains_api_rate_floor_without_exposing_stderr() {
        let temp = TempDir::new().unwrap();
        let curl = fake_curl(
            &temp,
            "#!/bin/sh\n/bin/cat >/dev/null\nprintf 'HTTP/1.1 429 Too Many Requests\\r\\nRetry-After: 17\\r\\nX-Poll-Interval: 3\\r\\n\\r\\n'\nprintf '%s' 'signed-query-SECRET' >&2\nexit 63\n",
        );
        let mut config = curl_config(b"https://api.github.com/x", Some(b"fixture-token")).unwrap();
        let cancellation = ActionsCancellation::new();
        let (result, poll, _) = conditional::with_general_read_tracker(|| {
            run_curl(
                &curl,
                CurlKind::Api,
                &mut config,
                1024,
                &cancellation,
                Instant::now(),
            )
        });
        let error = result.unwrap_err();
        assert_eq!(error.category(), ActionsReadErrorCategory::TooLarge);
        assert_eq!(
            poll.rate_limit,
            Some(conditional::BoundedDelay::Seconds(17))
        );
        assert_eq!(
            poll.x_poll_interval,
            Some(conditional::BoundedDelay::Seconds(3))
        );
        assert!(!error.to_string().contains("SECRET"));
    }

    #[test]
    fn fake_child_cancel_terminates_group_after_header_floor_is_observed() {
        let temp = TempDir::new().unwrap();
        let ready = temp.path().join("ready");
        let curl = fake_curl(
            &temp,
            &format!(
                "#!/bin/sh\n/bin/cat >/dev/null\nprintf 'HTTP/1.1 429 Too Many Requests\\r\\nRetry-After: 19\\r\\n'\nprintf ready > '{}'\n/bin/sleep 30\n",
                ready.display()
            ),
        );
        let cancellation = ActionsCancellation::new();
        let child_cancel = cancellation.clone();
        let handle = thread::spawn(move || {
            let mut config =
                curl_config(b"https://api.github.com/x", Some(b"fixture-token")).unwrap();
            conditional::with_general_read_tracker(|| {
                run_curl(
                    &curl,
                    CurlKind::Api,
                    &mut config,
                    1024,
                    &child_cancel,
                    Instant::now(),
                )
            })
        });
        let wait_started = Instant::now();
        while !ready.exists() && wait_started.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(ready.exists());
        cancellation.cancel();
        let (result, poll, _) = handle.join().unwrap();
        assert_eq!(
            result.unwrap_err().category(),
            ActionsReadErrorCategory::Cancelled
        );
        assert_eq!(
            poll.rate_limit,
            Some(conditional::BoundedDelay::Seconds(19))
        );
    }

    #[test]
    fn fake_child_stdout_overflow_discards_output_and_reaps() {
        let temp = TempDir::new().unwrap();
        let curl = fake_curl(
            &temp,
            "#!/bin/sh\n/bin/cat >/dev/null\nprintf 'HTTP/1.1 200 OK\\r\\n\\r\\n'\ni=0; while [ $i -lt 256 ]; do printf x; i=$((i + 1)); done\n",
        );
        let mut config = curl_config(b"https://a.blob.core.windows.net/log", None).unwrap();
        let result = run_curl(
            &curl,
            CurlKind::Storage,
            &mut config,
            64,
            &ActionsCancellation::new(),
            Instant::now(),
        );
        assert_eq!(
            result.unwrap_err().category(),
            ActionsReadErrorCategory::TooLarge
        );
    }

    #[test]
    fn fake_child_stderr_never_steers_api_scheduling() {
        let temp = TempDir::new().unwrap();
        let curl = fake_curl(
            &temp,
            "#!/bin/sh\n/bin/cat >/dev/null\nprintf 'HTTP/1.1 200 OK\\r\\n\\r\\n{}'\nprintf 'HTTP/1.1 429 Too Many Requests\\r\\nRetry-After: 777\\r\\n\\r\\n' >&2\n",
        );
        let mut config = curl_config(b"https://api.github.com/x", Some(b"fixture-token")).unwrap();
        let (result, poll, _) = conditional::with_general_read_tracker(|| {
            run_curl(
                &curl,
                CurlKind::Api,
                &mut config,
                32,
                &ActionsCancellation::new(),
                Instant::now(),
            )
        });
        assert!(result.is_err());
        assert_eq!(poll.rate_limit, None);
        assert_eq!(poll.x_poll_interval, None);
    }

    #[test]
    fn fake_storage_child_receives_credential_free_stdin_and_fixed_headers() {
        let temp = TempDir::new().unwrap();
        let argv = temp.path().join("argv");
        let input = temp.path().join("input");
        let environment = temp.path().join("environment");
        let ssl_key_log = temp.path().join("must-not-exist-ssl-key-log");
        let curl = fake_curl(
            &temp,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\n/bin/cat > '{}'\n/usr/bin/env > '{}'\nprintf 'HTTP/1.1 200 OK\\r\\nContent-Type: text/plain\\r\\nContent-Length: 2\\r\\n\\r\\nok'\n",
                argv.display(),
                input.display(),
                environment.display()
            ),
        );
        let mut config = curl_config(
            b"https://account.blob.core.windows.net/log?signed=SECRET",
            None,
        )
        .unwrap();
        // SAFETY: this non-secret sentinel only discriminates the child
        // environment boundary and is removed immediately after the call.
        unsafe { std::env::set_var("SSLKEYLOGFILE", &ssl_key_log) };
        let output = run_curl(
            &curl,
            CurlKind::Storage,
            &mut config,
            1024,
            &ActionsCancellation::new(),
            Instant::now(),
        )
        .unwrap();
        unsafe { std::env::remove_var("SSLKEYLOGFILE") };
        assert!(config.is_empty());
        assert!(parse_http_response(output, 2, "test").is_ok());
        let argv = fs::read_to_string(argv).unwrap();
        for forbidden in [
            "Authorization",
            "Cookie",
            "Referer",
            "application/vnd.github+json",
            "X-GitHub-Api-Version",
            "SECRET",
        ] {
            assert!(!argv.contains(forbidden), "argv leaked {forbidden}");
        }
        assert!(argv.starts_with("-q --config -"));
        let environment = fs::read_to_string(environment).unwrap();
        assert!(!environment.contains("SSLKEYLOGFILE"));
        assert!(!environment.contains("GH_TOKEN="));
        assert!(!ssl_key_log.exists());
        let input = fs::read_to_string(input).unwrap();
        assert!(input.starts_with("url = \"https://account.blob.core.windows.net/"));
        assert!(!input.contains("Authorization"));
    }

    #[test]
    fn spoofed_display_fields_cannot_repair_an_incomplete_attempt_tuple() {
        let repo = repository();
        let provider = GithubProvider::new(repo.account.clone());
        let (mut details, mut check) = details_and_check();
        details.pull_request_node_id = None;
        check.name = "GitHub Actions".into();
        check.details_url = Some("https://github.com/owner/repo/actions/runs/6".into());
        check.github_permalink = Some("https://github.com/owner/repo/actions/runs/6".into());
        assert_eq!(
            provider
                .actions_attempt_locator(&repo, &details, &check)
                .unwrap_err()
                .category(),
            ActionsReadErrorCategory::InvalidResponse
        );
    }

    #[test]
    fn workflow_url_is_canonical_to_the_frozen_repository_and_run() {
        let repo = repository();
        let provider = GithubProvider::new(repo.account.clone());
        for url in [
            "https://github.com/other/repo/actions/runs/6",
            "https://github.com/owner/repo/actions/runs/7",
            "https://github.com/owner/repo/actions/runs/6/attempts/2",
            "https://github.com/owner/repo/actions/runs/6?check_suite_focus=true",
        ] {
            let (details, mut check) = details_and_check();
            let ActionsLinkage::Linked(workflow) = &mut check.actions_linkage else {
                panic!()
            };
            workflow.github_url = url.into();
            assert_eq!(
                provider
                    .actions_attempt_locator(&repo, &details, &check)
                    .unwrap_err()
                    .category(),
                ActionsReadErrorCategory::MovedIdentity,
                "accepted {url}"
            );
        }
        let (details, check) = details_and_check();
        assert!(
            provider
                .actions_attempt_locator(&repo, &details, &check)
                .is_ok()
        );
    }

    #[test]
    fn selected_pr_relation_requires_an_exact_returned_association() {
        let repo = repository();
        let provider = GithubProvider::new(repo.account.clone());
        let (details, check) = details_and_check();
        let locator = provider
            .actions_attempt_locator(&repo, &details, &check)
            .unwrap();
        let key = ActionsAttemptKey {
            locator: locator.clone(),
            viewer_node_id: "VIEWER_node".into(),
            viewer_login: "alice".into(),
        };
        let run = |pull_requests| ApiRunAttempt {
            id: 6,
            node_id: "RUN_node".into(),
            run_attempt: 2,
            run_number: 5,
            event: "pull_request".into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
            workflow_id: 4,
            check_suite_id: 8,
            check_suite_node_id: "SUITE_node".into(),
            head_sha: "a".repeat(40),
            url: "https://api.github.com/repos/owner/repo/actions/runs/6/attempts/2".into(),
            html_url: "https://github.com/owner/repo/actions/runs/6/attempts/2".into(),
            workflow_url: "https://api.github.com/repos/owner/repo/actions/workflows/4".into(),
            repository: ApiRepositoryIdentity {
                node_id: "REPO_node".into(),
                full_name: "owner/repo".into(),
            },
            head_repository: Some(ApiRepositoryIdentity {
                node_id: "REPO_node".into(),
                full_name: "owner/repo".into(),
            }),
            pull_requests,
        };
        assert_eq!(
            validate_run(&repo, &key, &"a".repeat(40), run(vec![]), 1)
                .unwrap()
                .relation,
            ActionsHeadRelation::Unknown
        );
        let omitted = ApiRunPullRequest {
            number: 8,
            base: ApiPullBranch {
                sha: "b".repeat(40),
                repo: ApiRepositoryIdentity {
                    node_id: "OTHER_BASE".into(),
                    full_name: "owner/repo".into(),
                },
            },
            head: ApiPullBranch {
                sha: "c".repeat(40),
                repo: ApiRepositoryIdentity {
                    node_id: "OTHER_HEAD".into(),
                    full_name: "fork/repo".into(),
                },
            },
        };
        assert_eq!(
            validate_run(&repo, &key, &"a".repeat(40), run(vec![omitted]), 1)
                .unwrap()
                .relation,
            ActionsHeadRelation::Unknown
        );
        let selected = ApiRunPullRequest {
            number: 7,
            base: ApiPullBranch {
                sha: "b".repeat(40),
                repo: ApiRepositoryIdentity {
                    node_id: "REPO_node".into(),
                    full_name: "owner/repo".into(),
                },
            },
            head: ApiPullBranch {
                sha: "a".repeat(40),
                repo: ApiRepositoryIdentity {
                    node_id: "REPO_node".into(),
                    full_name: "owner/repo".into(),
                },
            },
        };
        assert_eq!(
            validate_run(&repo, &key, &"c".repeat(40), run(vec![selected]), 1)
                .unwrap()
                .relation,
            ActionsHeadRelation::HistoricalHead
        );
    }

    #[test]
    fn same_identity_revalidation_cannot_replace_snapshot_with_too_many_steps() {
        let make_job = |id: u64, step_count: usize| ActionsJob {
            id,
            node_id: format!("JOB_{id}"),
            run_id: 6,
            run_attempt: 2,
            head_sha: "a".repeat(40),
            check_run_database_id: id + 10,
            check_run_url: format!(
                "https://api.github.com/repos/owner/repo/check-runs/{}",
                id + 10
            ),
            name: format!("job {id}"),
            status: "completed".into(),
            conclusion: Some("success".into()),
            started_at: None,
            completed_at: None,
            api_url: format!("https://api.github.com/repos/owner/repo/actions/jobs/{id}"),
            html_url: format!("https://github.com/owner/repo/actions/runs/6/job/{id}"),
            steps: (0..step_count)
                .map(|number| ActionsJobStep {
                    number: number as u64 + 1,
                    name: "step".into(),
                    status: "completed".into(),
                    conclusion: Some("success".into()),
                    started_at: None,
                    completed_at: None,
                })
                .collect(),
        };
        let initial = (1..=40).map(|id| make_job(id, 1)).collect::<Vec<_>>();
        let refreshed = (1..=40).map(|id| make_job(id, 256)).collect::<Vec<_>>();
        assert_eq!(
            initial.iter().map(job_fingerprint).collect::<Vec<_>>(),
            refreshed.iter().map(job_fingerprint).collect::<Vec<_>>()
        );
        assert!(add_steps(0, &initial).is_ok());
        assert_eq!(
            add_steps(0, &refreshed).unwrap_err().category(),
            ActionsReadErrorCategory::TooLarge
        );
    }

    #[test]
    fn storage_host_uses_a_strict_subdomain_boundary_and_secret_is_redacted() {
        for value in [
            "https://results.actions.githubusercontent.com/path?sig=SECRET",
            "https://account.blob.core.windows.net/container/log?sig=SECRET",
        ] {
            let secret = SecretUrl::parse(value).unwrap();
            assert_eq!(format!("{secret}"), "[redacted signed URL]");
            assert_eq!(format!("{secret:?}"), "SecretUrl([redacted])");
            assert!(!format!("{secret:?}").contains("SECRET"));
        }
        for value in [
            "http://results.actions.githubusercontent.com/path",
            "https://actions.githubusercontent.com/path",
            "https://actions.githubusercontent.com.evil.test/path",
            "https://evilactions.githubusercontent.com/path",
            "https://127.0.0.1/path",
            "https://user@results.actions.githubusercontent.com/path",
            "https://results.actions.githubusercontent.com:443/path",
            "https://RESULTS.actions.githubusercontent.com/path",
        ] {
            assert!(SecretUrl::parse(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn curl_config_escapes_quotes_and_never_places_secret_in_fixed_arguments() {
        let config = curl_config(
            b"https://a.blob.core.windows.net/p?q=\\\"secret",
            Some(b"tok\\\"en"),
        )
        .unwrap();
        let text = String::from_utf8(config).unwrap();
        assert!(text.contains("q=\\\\\\\"secret"));
        assert!(text.contains("Bearer tok\\\\\\\"en"));
    }

    #[test]
    fn log_credential_child_timeout_remains_typed() {
        let temp = TempDir::new().unwrap();
        let gh = temp.path().join("gh");
        executable(&gh, "#!/bin/sh\n/bin/sleep 5\n");
        let repo = repository();
        let provider =
            GithubProvider::synthetic_with_gh(repo.account, gh, Duration::from_millis(80));
        assert_eq!(
            resolve_token(
                &provider,
                &ActionsCancellation::new(),
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap_err()
            .category(),
            ActionsReadErrorCategory::TimedOut
        );
    }

    #[test]
    fn plain_text_normalizes_newlines_and_neutralizes_controls() {
        let (text, lines) = sanitize_log(b"one\r\ntwo\rthree\x1b[31m\tend\n").unwrap();
        assert_eq!(text, "one\ntwo\nthree\u{fffd}[31m\tend\n");
        assert_eq!(lines, 3);
        assert!(sanitize_log(b"bad\0body").is_err());
        assert_eq!(sanitize_log(b"").unwrap().1, 0);
        assert_eq!(sanitize_log(b"\n").unwrap().1, 1);
        assert_eq!(sanitize_log(b"\nA").unwrap().1, 2);
        assert_eq!(sanitize_log(b"A\n").unwrap().1, 1);
        assert_eq!(sanitize_log(b"\r\nA\r").unwrap().1, 2);
        assert_eq!(
            sanitize_log(&vec![b'\n'; MAX_LOG_LINES]).unwrap().1,
            MAX_LOG_LINES
        );
        assert_eq!(
            sanitize_log(&vec![b'\n'; MAX_LOG_LINES + 1])
                .unwrap_err()
                .category(),
            ActionsReadErrorCategory::TooLarge
        );
    }

    #[test]
    fn actions_error_adapter_preserves_closed_non_http_categories() {
        let cases = [
            (
                conditional::RestReadError::credential(),
                ActionsReadErrorCategory::Credential,
            ),
            (
                conditional::RestReadError::cancelled(),
                ActionsReadErrorCategory::Cancelled,
            ),
            (
                conditional::RestReadError::timed_out(),
                ActionsReadErrorCategory::TimedOut,
            ),
            (
                conditional::RestReadError::operation_limit(),
                ActionsReadErrorCategory::TooLarge,
            ),
            (
                conditional::RestReadError::invalid_body(Default::default()),
                ActionsReadErrorCategory::InvalidResponse,
            ),
            (
                conditional::RestReadError::transport(),
                ActionsReadErrorCategory::Transport,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(map_rest(error, "test").category(), expected);
        }
    }

    #[test]
    fn expired_deadline_and_cancellation_block_install() {
        assert_eq!(
            check_operation(
                &ActionsCancellation::new(),
                Instant::now() - Duration::from_millis(1),
                "install",
            )
            .unwrap_err()
            .category(),
            ActionsReadErrorCategory::TimedOut
        );
        let cancellation = ActionsCancellation::new();
        cancellation.cancel();
        assert_eq!(
            check_operation(
                &cancellation,
                Instant::now() + Duration::from_secs(1),
                "install",
            )
            .unwrap_err()
            .category(),
            ActionsReadErrorCategory::Cancelled
        );
    }

    #[test]
    fn duplicate_or_conflicting_location_is_rejected_without_exposure() {
        let output = b"HTTP/1.1 302 Found\r\nLocation: https://a.blob.core.windows.net/a?SECRET\r\nLocation: https://b.blob.core.windows.net/b?SECRET\r\n\r\n".to_vec();
        let response = parse_http_response(output, 0, "test").unwrap();
        let error = response.single_header("location").unwrap_err();
        assert!(!error.to_string().contains("SECRET"));
    }
}
