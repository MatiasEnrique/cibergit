//! Conditional REST representations for the bounded general synchronization slice.
//!
//! Only the sidebar PR enumeration and single-PR metadata body are retained. GraphQL
//! hydration remains live and is never representation-cache authority.

use super::{
    ApiPullRequest, GithubProvider, MAX_PR_PAGES, PAGE_SIZE, Session,
    conditional::{self, ConditionalGet, RestValidators},
};
use crate::domain::{PullRequest, Repository};
use anyhow::{Result, bail, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

pub use super::conditional::{
    BoundedDelay as GeneralReadDelay, GeneralReadFailureKind,
    RestPollDirective as GeneralReadDirective,
};

pub const GENERAL_READ_CACHE_MAX_ENTRIES: usize = 256;
pub const GENERAL_READ_CACHE_MAX_BYTES: usize = 16 * 1024 * 1024;
pub const GENERAL_READ_CACHE_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
pub const GENERAL_READ_CACHE_MAX_LIST_ITEMS: usize = PAGE_SIZE;

const ACCEPT: &str = "application/vnd.github+json";
const API_VERSION: &str = "2026-03-10";

#[derive(Clone, Debug, Default)]
pub struct GeneralReadCache {
    entries: BTreeMap<GeneralRestKey, CachedGeneralRestBody>,
}

#[derive(Debug)]
pub struct GeneralReadOutcome<T> {
    result: std::result::Result<T, String>,
    cache: Option<GeneralReadCache>,
    poll: GeneralReadDirective,
    failure: Option<GeneralReadFailureKind>,
}

impl<T> GeneralReadOutcome<T> {
    pub fn result(&self) -> &std::result::Result<T, String> {
        &self.result
    }

    pub fn poll(&self) -> &GeneralReadDirective {
        &self.poll
    }

    pub fn failure_kind(&self) -> Option<GeneralReadFailureKind> {
        self.failure
    }

    pub fn into_parts(
        self,
    ) -> (
        std::result::Result<T, String>,
        Option<GeneralReadCache>,
        GeneralReadDirective,
        Option<GeneralReadFailureKind>,
    ) {
        (self.result, self.cache, self.poll, self.failure)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct GeneralRestKey {
    provider: &'static str,
    host: String,
    account: String,
    owner: String,
    repository: String,
    method: &'static str,
    path: String,
    canonical_query: String,
    page: Option<usize>,
    requested_state: Option<String>,
    pull_request: Option<u64>,
    since: Option<String>,
    accept: &'static str,
    api_version: &'static str,
}

#[derive(Clone, Debug)]
struct CachedGeneralRestBody {
    body: CachedBody,
    validators: RestValidators,
    body_bytes: usize,
    retained_bytes: usize,
}

#[derive(Clone, Debug)]
enum CachedBody {
    PullList(Vec<ApiPullRequest>),
    Pull(Box<ApiPullRequest>),
}

impl GithubProvider {
    /// Execute an unconditional general read with bounded included-response
    /// metadata. A rate response halts later nested calls in this operation.
    pub fn general_read<T>(
        &self,
        operation: impl FnOnce(&Self) -> Result<T>,
    ) -> GeneralReadOutcome<T> {
        tracked(None, || operation(self))
    }

    /// Conditional REST sidebar enumeration with live GraphQL hydration.
    pub fn list_pull_requests_conditional(
        &self,
        repo: &Repository,
        state: &str,
        cache: GeneralReadCache,
    ) -> GeneralReadOutcome<Vec<PullRequest>> {
        tracked(Some(cache.clone()), || {
            let (value, cache) = self.list_pull_requests_conditional_impl(repo, state, cache)?;
            Ok((value, cache))
        })
        .flatten_cache()
    }

    /// Conditional REST single-PR metadata with live GraphQL hydration.
    pub fn pull_request_conditional(
        &self,
        repo: &Repository,
        number: u64,
        cache: GeneralReadCache,
    ) -> GeneralReadOutcome<PullRequest> {
        tracked(Some(cache.clone()), || {
            let (value, cache) = self.pull_request_conditional_impl(repo, number, cache)?;
            Ok((value, cache))
        })
        .flatten_cache()
    }

    fn list_pull_requests_conditional_impl(
        &self,
        repo: &Repository,
        state: &str,
        mut cache: GeneralReadCache,
    ) -> Result<(Vec<PullRequest>, GeneralReadCache)> {
        self.validate_repo(repo)?;
        let state = state.to_ascii_lowercase();
        ensure!(
            ["open", "closed", "merged", "all"].contains(&state.as_str()),
            "Unsupported PR state"
        );
        let api_state = if state == "merged" { "closed" } else { &state };
        let mut session = Session::new(self);
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        let mut staged = Vec::new();
        for page in 1..=MAX_PR_PAGES {
            let endpoint = format!(
                "repos/{}/pulls?state={api_state}&sort=created&direction=asc&per_page={PAGE_SIZE}&page={page}",
                repo.full_name()
            );
            let key = list_key(self, repo, &state, api_state, page);
            let cached = cache.entries.get(&key).cloned();
            let response = session.get_conditional::<Vec<ApiPullRequest>>(
                &endpoint,
                cached.as_ref().map(|entry| &entry.validators),
            )?;
            let (pulls, validators, body_bytes) = match response {
                ConditionalGet::Modified { value, metadata } => {
                    (value, metadata.validators, metadata.body_bytes)
                }
                ConditionalGet::NotModified { metadata } => {
                    let entry = cached.ok_or_else(|| {
                        anyhow::anyhow!(
                            "Conditional PR enumeration had no exact retained response body"
                        )
                    })?;
                    let CachedBody::PullList(value) = entry.body else {
                        bail!("Conditional PR enumeration cache representation mismatch")
                    };
                    (
                        value,
                        entry
                            .validators
                            .merged_after_not_modified(&metadata.validators),
                        entry.body_bytes,
                    )
                }
            };
            ensure!(pulls.len() <= PAGE_SIZE, "Invalid PR pagination response");
            let last = pulls.len() < PAGE_SIZE;
            let retained = cacheable(
                &key,
                CachedBody::PullList(pulls.clone()),
                validators,
                body_bytes,
                pulls.len(),
            );
            let mut batch = Vec::with_capacity(pulls.len());
            for pull in pulls {
                pull.validate(repo, None)?;
                ensure!(
                    seen.insert(pull.number),
                    "PR list changed during pagination; refresh to retry"
                );
                batch.push(pull.into_domain());
            }
            session.hydrate_metadata(repo, &mut batch)?;
            for pull in batch {
                if state != "merged" || pull.state == "MERGED" {
                    result.push(pull);
                }
            }
            staged.push((key, retained));
            if last {
                for (key, entry) in staged {
                    install(&mut cache, key, entry);
                }
                prune_list_tail(&mut cache, self, repo, &state, api_state, page);
                return Ok((result, cache));
            }
        }
        bail!("PR pagination limit reached; list is incomplete")
    }

    fn pull_request_conditional_impl(
        &self,
        repo: &Repository,
        number: u64,
        mut cache: GeneralReadCache,
    ) -> Result<(PullRequest, GeneralReadCache)> {
        self.validate_repo(repo)?;
        ensure!(number > 0, "PR number must be positive");
        let endpoint = format!("repos/{}/pulls/{number}", repo.full_name());
        let key = pull_key(self, repo, number);
        let cached = cache.entries.get(&key).cloned();
        let mut session = Session::new(self);
        let response = session.get_conditional::<ApiPullRequest>(
            &endpoint,
            cached.as_ref().map(|entry| &entry.validators),
        )?;
        let (pull, validators, body_bytes) = match response {
            ConditionalGet::Modified { value, metadata } => {
                (value, metadata.validators, metadata.body_bytes)
            }
            ConditionalGet::NotModified { metadata } => {
                let entry = cached.ok_or_else(|| {
                    anyhow::anyhow!("Conditional PR metadata had no exact retained response body")
                })?;
                let CachedBody::Pull(value) = entry.body else {
                    bail!("Conditional PR metadata cache representation mismatch")
                };
                (
                    *value,
                    entry
                        .validators
                        .merged_after_not_modified(&metadata.validators),
                    entry.body_bytes,
                )
            }
        };
        pull.validate(repo, Some(number))?;
        let retained = cacheable(
            &key,
            CachedBody::Pull(Box::new(pull.clone())),
            validators,
            body_bytes,
            1,
        );
        let mut pulls = vec![pull.into_domain()];
        session.hydrate_metadata(repo, &mut pulls)?;
        install(&mut cache, key, retained);
        Ok((pulls.pop().expect("one PR"), cache))
    }
}

impl<T> GeneralReadOutcome<(T, GeneralReadCache)> {
    fn flatten_cache(self) -> GeneralReadOutcome<T> {
        match self.result {
            Ok((value, cache)) => GeneralReadOutcome {
                result: Ok(value),
                cache: Some(cache),
                poll: self.poll,
                failure: self.failure,
            },
            Err(error) => GeneralReadOutcome {
                result: Err(error),
                cache: None,
                poll: self.poll,
                failure: self.failure,
            },
        }
    }
}

fn tracked<T>(
    success_cache: Option<GeneralReadCache>,
    operation: impl FnOnce() -> Result<T>,
) -> GeneralReadOutcome<T> {
    let (result, poll, tracked_failure) = conditional::with_general_read_tracker(operation);
    let mut result = result.map_err(|error| format!("{error:#}"));
    let failure = if poll.rate_limit.is_some() {
        result = Err("GitHub read was stopped by server rate limiting".into());
        Some(GeneralReadFailureKind::RateLimited)
    } else if result.is_ok() {
        None
    } else {
        tracked_failure.or(Some(GeneralReadFailureKind::Incomplete))
    };
    GeneralReadOutcome {
        result,
        cache: result_is_ok_cache(&failure, success_cache),
        poll,
        failure,
    }
}

fn result_is_ok_cache(
    failure: &Option<GeneralReadFailureKind>,
    cache: Option<GeneralReadCache>,
) -> Option<GeneralReadCache> {
    failure.is_none().then_some(cache).flatten()
}

fn list_key(
    provider: &GithubProvider,
    repo: &Repository,
    requested_state: &str,
    api_state: &str,
    page: usize,
) -> GeneralRestKey {
    GeneralRestKey {
        provider: "github",
        host: provider.account.host.to_ascii_lowercase(),
        account: provider.account.login.to_ascii_lowercase(),
        owner: repo.owner.to_ascii_lowercase(),
        repository: repo.name.to_ascii_lowercase(),
        method: "GET",
        path: format!("/repos/{}/pulls", repo.full_name()),
        canonical_query: format!(
            "direction=asc&page={page}&per_page={PAGE_SIZE}&sort=created&state={api_state}"
        ),
        page: Some(page),
        requested_state: Some(requested_state.to_owned()),
        pull_request: None,
        since: None,
        accept: ACCEPT,
        api_version: API_VERSION,
    }
}

fn pull_key(provider: &GithubProvider, repo: &Repository, number: u64) -> GeneralRestKey {
    GeneralRestKey {
        provider: "github",
        host: provider.account.host.to_ascii_lowercase(),
        account: provider.account.login.to_ascii_lowercase(),
        owner: repo.owner.to_ascii_lowercase(),
        repository: repo.name.to_ascii_lowercase(),
        method: "GET",
        path: format!("/repos/{}/pulls/{number}", repo.full_name()),
        canonical_query: String::new(),
        page: None,
        requested_state: None,
        pull_request: Some(number),
        since: None,
        accept: ACCEPT,
        api_version: API_VERSION,
    }
}

fn cacheable(
    key: &GeneralRestKey,
    body: CachedBody,
    validators: RestValidators,
    body_bytes: usize,
    item_count: usize,
) -> Option<CachedGeneralRestBody> {
    if validators.is_empty()
        || body_bytes > GENERAL_READ_CACHE_MAX_BODY_BYTES
        || item_count > GENERAL_READ_CACHE_MAX_LIST_ITEMS
    {
        return None;
    }
    let retained_bytes = body_bytes
        .checked_add(serde_json::to_vec(key).ok()?.len())?
        .checked_add(validators.etag.as_ref().map_or(0, String::len))?
        .checked_add(validators.last_modified.as_ref().map_or(0, String::len))?;
    (retained_bytes <= GENERAL_READ_CACHE_MAX_BODY_BYTES).then_some(CachedGeneralRestBody {
        body,
        validators,
        body_bytes,
        retained_bytes,
    })
}

fn install(
    cache: &mut GeneralReadCache,
    key: GeneralRestKey,
    entry: Option<CachedGeneralRestBody>,
) {
    cache.entries.remove(&key);
    if let Some(entry) = entry {
        cache.entries.insert(key, entry);
    }
    while cache.entries.len() > GENERAL_READ_CACHE_MAX_ENTRIES
        || cache
            .entries
            .values()
            .map(|entry| entry.retained_bytes)
            .sum::<usize>()
            > GENERAL_READ_CACHE_MAX_BYTES
    {
        let Some(key) = cache.entries.keys().next_back().cloned() else {
            break;
        };
        cache.entries.remove(&key);
    }
}

fn prune_list_tail(
    cache: &mut GeneralReadCache,
    provider: &GithubProvider,
    repo: &Repository,
    requested_state: &str,
    api_state: &str,
    terminal_page: usize,
) {
    let identity = list_key(provider, repo, requested_state, api_state, terminal_page);
    cache.entries.retain(|key, _| {
        key.page.is_none_or(|page| page <= terminal_page)
            || key.provider != identity.provider
            || key.host != identity.host
            || key.account != identity.account
            || key.owner != identity.owner
            || key.repository != identity.repository
            || key.method != identity.method
            || key.path != identity.path
            || key.requested_state != identity.requested_state
            || key.since != identity.since
            || key.accept != identity.accept
            || key.api_version != identity.api_version
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Account;
    use crate::providers::Runner;
    use serde_json::{Map, Value, json};
    use std::{fs, os::unix::fs::PermissionsExt, time::Duration};
    use tempfile::TempDir;

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

    fn pull(number: u64, state: &str, merged: bool) -> Value {
        json!({
            "number": number,
            "title": format!("pull {number}"),
            "body": "body",
            "user": {"login": "author"},
            "state": state,
            "draft": false,
            "merged_at": merged.then_some("2026-09-13T12:00:00Z"),
            "html_url": format!("https://github.com/owner/repo/pull/{number}"),
            "base": {"sha": "1".repeat(40), "ref": "main", "repo": {"name": "repo", "owner": {"login": "owner"}}},
            "head": {"sha": "2".repeat(40), "ref": format!("topic-{number}"), "repo": {"name": "repo", "owner": {"login": "owner"}}},
            "requested_reviewers": [],
            "requested_teams": [],
            "assignees": [],
            "labels": [],
            "changed_files": 1,
            "updated_at": "2026-09-13T12:00:00Z"
        })
    }

    fn metadata(numbers: &[u64]) -> Value {
        let mut repository = Map::new();
        repository.insert("nameWithOwner".into(), json!("owner/repo"));
        for (index, number) in numbers.iter().enumerate() {
            repository.insert(
                format!("pr{index}"),
                json!({
                    "number": number,
                    "url": format!("https://github.com/owner/repo/pull/{number}"),
                    "reviewDecision": null,
                    "statusCheckRollup": null,
                    "comments": {"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}},
                    "reviews": {"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}},
                    "reviewRequests": {"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}},
                    "assignees": {"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}}
                }),
            );
        }
        json!({"data": {"repository": Value::Object(repository)}})
    }

    fn rest(endpoint: &str, status: u16, etag: Option<&str>, body: Option<Value>) -> Value {
        json!({
            "kind": "rest",
            "endpoint": endpoint,
            "status": status,
            "etag": etag,
            "body": body,
        })
    }

    fn graphql(status: u16, headers: Value, body: Option<Value>) -> Value {
        json!({
            "kind": "graphql",
            "status": status,
            "headers": headers,
            "body": body,
        })
    }

    fn fixture(login: &str, steps: Vec<Value>) -> (TempDir, GithubProvider) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("fixture.json"),
            serde_json::to_vec(&json!({"login": login, "steps": steps})).unwrap(),
        )
        .unwrap();
        let executable = dir.path().join("gh");
        fs::write(
            &executable,
            r#"#!/usr/bin/python3
import json, os, pathlib, sys
root = pathlib.Path(__file__).parent
fixture = json.loads((root / 'fixture.json').read_text())
args = sys.argv[1:]
if args[:2] == ['auth', 'token']:
    assert args == ['auth', 'token', '--hostname', 'github.com', '--user', fixture['login']]
    assert 'GH_TOKEN' not in os.environ
    print('private-' + fixture['login'])
    sys.exit(0)
assert os.environ.get('GH_TOKEN') == 'private-' + fixture['login']
count_path = root / 'count'
index = int(count_path.read_text()) if count_path.exists() else 0
step = fixture['steps'][index]
count_path.write_text(str(index + 1))
headers = [args[i + 1] for i, value in enumerate(args[:-1]) if value == '--header']
if step['kind'] == 'rest':
    assert args[:2] == ['api', '--include']
    assert args[-1] == step['endpoint'], (args, step)
    expected = step.get('if_none_match')
    actual = [value for value in headers if value.startswith('If-None-Match:')]
    assert actual == ([] if expected is None else ['If-None-Match: ' + expected]), (actual, expected)
else:
    assert args == ['api', '--include', '--hostname', 'github.com', '--method', 'POST', '--header', 'Accept: application/vnd.github+json', '--header', 'X-GitHub-Api-Version: 2026-03-10', 'graphql', '--input', '-'], args
    payload = json.load(sys.stdin)
    assert payload['query'].lstrip().startswith('query ')
status = step['status']
sys.stdout.write('HTTP/2 %d fixture\r\n' % status)
if step.get('etag') is not None:
    sys.stdout.write('ETag: %s\r\n' % step['etag'])
for name, value in step.get('headers', {}).items():
    sys.stdout.write('%s: %s\r\n' % (name, value))
sys.stdout.write('\r\n')
if step.get('body') is not None:
    sys.stdout.write(json.dumps(step['body']))
sys.exit(0 if status in [200, 304] else 1)
"#,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let provider = GithubProvider {
            account: account(login),
            runner: Runner {
                gh: executable,
                timeout: Duration::from_secs(10),
                ..Runner::default()
            },
        };
        (dir, provider)
    }

    fn calls(dir: &TempDir) -> usize {
        fs::read_to_string(dir.path().join("count"))
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    }

    #[test]
    fn exact_200_then_304_reuses_retained_pr_body_and_rehydrates_graphql() {
        let endpoint = "repos/owner/repo/pulls/7";
        let mut not_modified = rest(endpoint, 304, Some("\"v1\""), None);
        not_modified["if_none_match"] = json!("\"v1\"");
        let (dir, provider) = fixture(
            "alice",
            vec![
                rest(endpoint, 200, Some("\"v1\""), Some(pull(7, "open", false))),
                graphql(200, json!({}), Some(metadata(&[7]))),
                not_modified,
                graphql(200, json!({}), Some(metadata(&[7]))),
            ],
        );
        let (first, cache, _, failure) = provider
            .pull_request_conditional(&repo("alice"), 7, GeneralReadCache::default())
            .into_parts();
        assert_eq!(first.unwrap().number, 7);
        assert_eq!(failure, None);
        let (second, cache, _, failure) = provider
            .pull_request_conditional(&repo("alice"), 7, cache.unwrap())
            .into_parts();
        assert_eq!(second.unwrap().title, "pull 7");
        assert!(cache.is_some());
        assert_eq!(failure, None);
        assert_eq!(calls(&dir), 4);
    }

    #[test]
    fn sidebar_200_then_304_retains_exact_page_and_rehydrates_graphql() {
        let endpoint = format!(
            "repos/owner/repo/pulls?state=open&sort=created&direction=asc&per_page={PAGE_SIZE}&page=1"
        );
        let mut not_modified = rest(&endpoint, 304, Some("\"list-v1\""), None);
        not_modified["if_none_match"] = json!("\"list-v1\"");
        let (dir, provider) = fixture(
            "alice",
            vec![
                rest(
                    &endpoint,
                    200,
                    Some("\"list-v1\""),
                    Some(json!([pull(7, "open", false)])),
                ),
                graphql(200, json!({}), Some(metadata(&[7]))),
                not_modified,
                graphql(200, json!({}), Some(metadata(&[7]))),
            ],
        );
        let (first, cache, _, _) = provider
            .list_pull_requests_conditional(&repo("alice"), "open", GeneralReadCache::default())
            .into_parts();
        assert_eq!(first.unwrap()[0].number, 7);
        let (second, cache, _, failure) = provider
            .list_pull_requests_conditional(&repo("alice"), "open", cache.unwrap())
            .into_parts();
        assert_eq!(second.unwrap()[0].title, "pull 7");
        assert!(cache.is_some());
        assert_eq!(failure, None);
        assert_eq!(calls(&dir), 4);
    }

    #[test]
    fn not_modified_without_exact_body_is_rejected_without_hydration() {
        let endpoint = "repos/owner/repo/pulls/7";
        let (dir, provider) = fixture("alice", vec![rest(endpoint, 304, Some("\"orphan\""), None)]);
        let (result, cache, _, failure) = provider
            .pull_request_conditional(&repo("alice"), 7, GeneralReadCache::default())
            .into_parts();
        assert!(result.is_err());
        assert!(cache.is_none());
        assert_eq!(failure, Some(GeneralReadFailureKind::Incomplete));
        assert_eq!(calls(&dir), 1, "missing body must stop before GraphQL");
    }

    #[test]
    fn cache_identity_separates_account_state_page_and_pull_request() {
        let alice = GithubProvider::new(account("alice"));
        let bob = GithubProvider::new(account("bob"));
        assert_ne!(
            list_key(&alice, &repo("alice"), "open", "open", 1),
            list_key(&alice, &repo("alice"), "merged", "closed", 1)
        );
        assert_ne!(
            list_key(&alice, &repo("alice"), "open", "open", 1),
            list_key(&alice, &repo("alice"), "open", "open", 2)
        );
        assert_ne!(
            list_key(&alice, &repo("alice"), "open", "open", 1),
            list_key(&bob, &repo("bob"), "open", "open", 1)
        );
        assert_ne!(
            pull_key(&alice, &repo("alice"), 7),
            pull_key(&alice, &repo("alice"), 8)
        );
    }

    #[test]
    fn graphql_rate_response_stops_unstarted_sidebar_pages_and_returns_no_cache() {
        let endpoint = format!(
            "repos/owner/repo/pulls?state=open&sort=created&direction=asc&per_page={PAGE_SIZE}&page=1"
        );
        let body = Value::Array(
            (1..=PAGE_SIZE as u64)
                .map(|n| pull(n, "open", false))
                .collect(),
        );
        let (dir, provider) = fixture(
            "alice",
            vec![
                rest(&endpoint, 200, Some("\"page-1\""), Some(body)),
                graphql(
                    429,
                    json!({"Retry-After": "120", "X-RateLimit-Remaining": "0"}),
                    Some(json!({"message": "withheld"})),
                ),
            ],
        );
        let (result, cache, poll, failure) = provider
            .list_pull_requests_conditional(&repo("alice"), "open", GeneralReadCache::default())
            .into_parts();
        assert!(result.is_err());
        assert!(
            cache.is_none(),
            "failed chain must not return cache authority"
        );
        assert_eq!(failure, Some(GeneralReadFailureKind::RateLimited));
        assert_eq!(poll.rate_limit, Some(GeneralReadDelay::Seconds(120)));
        assert_eq!(calls(&dir), 2, "page two must remain unstarted");
    }

    #[test]
    fn malformed_rate_metadata_preserves_conservative_floor() {
        let (dir, provider) = fixture(
            "alice",
            vec![graphql(
                429,
                json!({"Retry-After": "not-decimal"}),
                Some(json!({"message": "withheld"})),
            )],
        );
        let outcome = provider.general_read(|provider| {
            Session::new(provider).graphql::<Value>("query Test { viewer { login } }", json!({}))
        });
        let (result, _, poll, failure) = outcome.into_parts();
        assert!(result.is_err());
        assert_eq!(failure, Some(GeneralReadFailureKind::RateLimited));
        assert_eq!(poll.rate_limit, Some(GeneralReadDelay::Seconds(60)));
        assert_eq!(calls(&dir), 1);
    }

    #[test]
    fn graphql_http_200_exhaustion_records_floor_and_stops_caught_nested_work() {
        let (dir, provider) = fixture(
            "alice",
            vec![graphql(
                200,
                json!({
                    "Retry-After": "75",
                    "X-RateLimit-Remaining": "0",
                    "X-RateLimit-Reset": "9999999999"
                }),
                Some(json!({
                    "data": null,
                    "errors": [{"type": "RATE_LIMITED", "message": "withheld"}]
                })),
            )],
        );
        let outcome = provider.general_read(|provider| {
            let mut session = Session::new(provider);
            let _ = session.graphql::<Value>("query First { viewer { login } }", json!({}));
            let _ = session.get::<Value>("repos/owner/repo");
            Ok(())
        });
        let (result, _, poll, failure) = outcome.into_parts();
        assert!(result.is_err());
        assert_eq!(failure, Some(GeneralReadFailureKind::RateLimited));
        assert_eq!(poll.rate_limit, Some(GeneralReadDelay::Seconds(75)));
        assert_eq!(
            calls(&dir),
            1,
            "caught error must not permit nested dispatch"
        );
    }

    #[test]
    fn malformed_graphql_exhaustion_metadata_keeps_conservative_floor() {
        let (dir, provider) = fixture(
            "alice",
            vec![graphql(
                200,
                json!({
                    "X-RateLimit-Remaining": "0",
                    "X-RateLimit-Reset": "not-decimal"
                }),
                Some(json!({
                    "data": null,
                    "errors": [{"type": "RATE_LIMITED", "message": "withheld"}]
                })),
            )],
        );
        let outcome = provider.general_read(|provider| {
            let mut session = Session::new(provider);
            let _ = session.graphql::<Value>("query First { viewer { login } }", json!({}));
            let _ = session.get::<Value>("repos/owner/repo");
            Ok(())
        });
        let (result, _, poll, failure) = outcome.into_parts();
        assert!(result.is_err());
        assert_eq!(failure, Some(GeneralReadFailureKind::RateLimited));
        assert_eq!(poll.rate_limit, Some(GeneralReadDelay::Seconds(60)));
        assert_eq!(calls(&dir), 1);
    }

    #[test]
    fn rest_poll_floor_is_recorded_before_aggregate_byte_rejection() {
        let endpoint = "repos/owner/repo";
        let mut response = rest(endpoint, 200, None, Some(json!({"name": "repo"})));
        response["headers"] = json!({"X-Poll-Interval": "45"});
        let (dir, provider) = fixture("alice", vec![response]);
        let outcome = provider.general_read(|provider| {
            let mut session = Session::new(provider);
            session.bytes = super::super::MAX_OPERATION_BYTES;
            session.get::<Value>(endpoint)
        });
        let (result, _, poll, failure) = outcome.into_parts();
        assert!(result.is_err());
        assert_eq!(failure, Some(GeneralReadFailureKind::Incomplete));
        assert_eq!(poll.x_poll_interval, Some(GeneralReadDelay::Seconds(45)));
        assert_eq!(calls(&dir), 1);
    }

    #[test]
    fn graphql_exhaustion_floor_is_recorded_before_aggregate_byte_rejection() {
        let (dir, provider) = fixture(
            "alice",
            vec![graphql(
                200,
                json!({"Retry-After": "75", "X-RateLimit-Remaining": "0"}),
                Some(json!({
                    "data": null,
                    "errors": [{"type": "RATE_LIMITED", "message": "withheld"}]
                })),
            )],
        );
        let outcome = provider.general_read(|provider| {
            let mut session = Session::new(provider);
            session.bytes = super::super::MAX_OPERATION_BYTES;
            session.graphql::<Value>("query First { viewer { login } }", json!({}))
        });
        let (result, _, poll, failure) = outcome.into_parts();
        assert!(result.is_err());
        assert_eq!(failure, Some(GeneralReadFailureKind::RateLimited));
        assert_eq!(poll.rate_limit, Some(GeneralReadDelay::Seconds(75)));
        assert_eq!(calls(&dir), 1);
    }

    #[test]
    fn cache_limits_bound_entries_total_bytes_and_single_body_size() {
        let provider = GithubProvider::new(account("alice"));
        let repository = repo("alice");
        let value: ApiPullRequest = serde_json::from_value(pull(7, "open", false)).unwrap();
        let validators = RestValidators {
            etag: Some("\"bounded\"".into()),
            last_modified: None,
        };
        assert!(
            cacheable(
                &pull_key(&provider, &repository, 7),
                CachedBody::Pull(Box::new(value.clone())),
                validators.clone(),
                GENERAL_READ_CACHE_MAX_BODY_BYTES + 1,
                1,
            )
            .is_none()
        );

        let mut cache = GeneralReadCache::default();
        for page in 1..=GENERAL_READ_CACHE_MAX_ENTRIES + 1 {
            install(
                &mut cache,
                list_key(&provider, &repository, "open", "open", page),
                Some(CachedGeneralRestBody {
                    body: CachedBody::Pull(Box::new(value.clone())),
                    validators: validators.clone(),
                    body_bytes: 1,
                    retained_bytes: 1,
                }),
            );
        }
        assert_eq!(cache.entries.len(), GENERAL_READ_CACHE_MAX_ENTRIES);

        let mut bytes_cache = GeneralReadCache::default();
        for number in 1..=20 {
            install(
                &mut bytes_cache,
                pull_key(&provider, &repository, number),
                Some(CachedGeneralRestBody {
                    body: CachedBody::Pull(Box::new(value.clone())),
                    validators: validators.clone(),
                    body_bytes: 1024 * 1024,
                    retained_bytes: 1024 * 1024,
                }),
            );
        }
        assert!(
            bytes_cache
                .entries
                .values()
                .map(|entry| entry.retained_bytes)
                .sum::<usize>()
                <= GENERAL_READ_CACHE_MAX_BYTES
        );
    }
}
