use super::{Palette, Root};
use anyhow::{Context as _, Result, ensure};
use cibergit::ui::{self, Density, TextRole};
use cibergit::{
    domain::{Account, Repository},
    notifications::{
        DisplayedNotificationSet, NotificationReconcileOutcome, NotificationSnapshot,
        NotificationStore, NotificationStoreLimits, PullRequestUnreadSummary,
    },
    providers::{
        GithubProvider,
        notifications::{
            IncompleteNotificationCandidate, NotificationAlertKind, NotificationConditionalCache,
            NotificationDelay, NotificationPollDirective, NotificationPullRequest,
            NotificationReadLimits, NotificationRepositoryScope, ProviderNotificationBatch,
            ProviderNotificationEvent, RepositoryNotificationCompleteness,
        },
    },
};
use gpui::{Context, Div, SharedString, Stateful, SystemNotification, div, prelude::*, px};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        raw::c_int,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const PAGE_SIZE: usize = 40;
const MAX_REGISTRY_BYTES: usize = 1024 * 1024;
const MAX_RETAINED_TAGS: usize = 2_048;
const MAX_RETAINED_ROUTES: usize = 512;
const MAX_CAS_ATTEMPTS: usize = 4;
const REGISTRY_SCHEMA_VERSION: u32 = 1;
const MAX_ACCOUNTS: usize = 128;
const MAX_IDENTITY_BYTES: usize = 512;
const LOCK_ATTEMPTS: usize = 50;
const O_CLOEXEC: c_int = 0x0100_0000;
const O_NOFOLLOW: c_int = 0x0000_0100;
const LOCK_EX: c_int = 2;
const LOCK_NB: c_int = 4;
const LOCK_UN: c_int = 8;

unsafe extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
    fn fchmod(fd: c_int, mode: u16) -> c_int;
    fn geteuid() -> u32;
}

static NEXT_LIFETIME: AtomicU64 = AtomicU64::new(1);
static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NotificationRoute {
    pub account: Account,
    pub owner: String,
    pub repository: String,
    pub pull_request: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ControllerToken {
    consent_generation: u64,
    lifetime: u64,
    account_key: String,
    selection: String,
    generation: u64,
}

#[derive(Clone)]
pub(super) struct PollWork {
    token: ControllerToken,
    account: Account,
    chunks: Vec<Vec<Repository>>,
    store: NotificationStore,
    cache: NotificationConditionalCache,
}

#[derive(Debug)]
pub(super) struct PollCompletion {
    token: ControllerToken,
    account: Account,
    snapshot: NotificationSnapshot,
    newly_admitted: Vec<ProviderNotificationEvent>,
    error: Option<String>,
    cache: NotificationConditionalCache,
    poll: NotificationPollDirective,
}

impl PollCompletion {
    pub(super) fn account(&self) -> &Account {
        &self.account
    }

    pub(super) fn schedule_key(&self) -> String {
        format!("notifications:{}", account_key(&self.account))
    }

    pub(super) fn failed(&self) -> bool {
        self.error.is_some()
            || self
                .snapshot
                .repository_completeness
                .iter()
                .any(|repository| !repository.complete)
    }
}

impl PollWork {
    pub(super) fn run(self) -> PollCompletion {
        self.run_with(|account, chunk, cache| {
            GithubProvider::new(account.clone()).notification_observations_conditional(
                chunk,
                None,
                NotificationReadLimits::default(),
                cache,
            )
        })
    }

    fn run_with(
        self,
        mut read: impl FnMut(
            &Account,
            &[Repository],
            NotificationConditionalCache,
        )
            -> Result<cibergit::providers::notifications::NotificationObservationRead>,
    ) -> PollCompletion {
        let mut batches = Vec::with_capacity(self.chunks.len());
        let mut failures = Vec::new();
        let mut cache = self.cache;
        let mut poll = NotificationPollDirective::default();
        for (index, chunk) in self.chunks.iter().enumerate() {
            match read(&self.account, chunk, cache.clone()) {
                Ok(read) => {
                    let rate_limited = read.poll.rate_limit.is_some();
                    cache = read.cache;
                    poll.merge(&read.poll);
                    batches.push((index, read.batch));
                    if rate_limited {
                        failures.extend(((index + 1)..self.chunks.len()).map(|deferred| {
                            (
                                deferred,
                                "notification chunk deferred by server rate limit".into(),
                            )
                        }));
                        break;
                    }
                }
                Err(error) => failures.push((index, format!("{error:#}"))),
            }
        }

        let reconcile = combine_batches(&self.account, &self.chunks, batches, &failures)
            .and_then(|batch| self.store.reconcile(&self.account, &batch));
        match reconcile {
            Ok(outcome) => PollCompletion {
                token: self.token,
                account: self.account,
                snapshot: snapshot_from_outcome(&outcome),
                newly_admitted: outcome.newly_admitted_alerts,
                error: (!failures.is_empty()).then(|| format_failures(&failures, self.chunks.len())),
                cache,
                poll,
            },
            Err(error) => PollCompletion {
                token: self.token,
                account: self.account.clone(),
                snapshot: self.store.list_unread(&self.account).unwrap_or_else(|cache_error| {
                    empty_snapshot(format!(
                        "Notification refresh failed ({error:#}); cached unread also unavailable ({cache_error:#})"
                    ))
                }),
                newly_admitted: Vec::new(),
                error: Some(format!(
                    "Notification refresh failed; cached unread retained: {error:#}"
                )),
                cache,
                poll,
            },
        }
    }
}

#[derive(Clone)]
pub(super) struct MarkReadWork {
    token: MarkToken,
    account: Account,
    displayed: DisplayedNotificationSet,
    store: NotificationStore,
}

#[derive(Debug)]
pub(super) struct MarkReadCompletion {
    token: MarkToken,
    account: Account,
    result: Result<NotificationSnapshot, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MarkToken {
    lifetime: u64,
    account_key: String,
    selection: String,
    selection_generation: u64,
    generation: u64,
}

impl MarkReadWork {
    pub(super) fn run(self) -> MarkReadCompletion {
        MarkReadCompletion {
            token: self.token,
            account: self.account.clone(),
            result: self
                .store
                .mark_displayed_read(&self.account, &self.displayed)
                .map_err(|error| format!("{error:#}")),
        }
    }
}

#[derive(Clone)]
pub(super) struct ConsentWork {
    token: ConsentToken,
    account: Account,
    enabled: bool,
    registry: NotificationRegistry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConsentToken {
    lifetime: u64,
    account_key: String,
    generation: u64,
}

#[derive(Debug)]
pub(super) struct ConsentCompletion {
    token: ConsentToken,
    account: Account,
    enabled: bool,
    result: Result<(), String>,
}

impl ConsentWork {
    pub(super) fn run(self) -> ConsentCompletion {
        ConsentCompletion {
            token: self.token,
            account: self.account.clone(),
            enabled: self.enabled,
            result: self
                .registry
                .set_enabled(&self.account, self.enabled)
                .map_err(|error| format!("{error:#}")),
        }
    }
}

#[derive(Clone)]
pub(super) struct AdmissionWork {
    token: AdmissionToken,
    account: Account,
    events: Vec<ProviderNotificationEvent>,
    registry: NotificationRegistry,
}

#[derive(Debug)]
pub(super) struct AdmissionCompletion {
    token: AdmissionToken,
    notifications: Vec<QueuedNotification>,
    result: Result<(), String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AdmissionToken {
    lifetime: u64,
    account_key: String,
    selection: String,
    selection_generation: u64,
    consent_generation: u64,
}

impl AdmissionWork {
    pub(super) fn run(self) -> AdmissionCompletion {
        let result = self
            .registry
            .reserve_for_delivery(&self.account, &self.events);
        match result {
            Ok(notifications) => AdmissionCompletion {
                token: self.token,
                notifications,
                result: Ok(()),
            },
            Err(error) => AdmissionCompletion {
                token: self.token,
                notifications: Vec::new(),
                result: Err(format!("{error:#}")),
            },
        }
    }
}

#[derive(Clone, Debug)]
struct QueuedNotification {
    delivery_lease: Option<Arc<RegistryLock>>,
    account_key: String,
    consent_generation: u64,
    selection: String,
    selection_generation: u64,
    notification: SystemNotification,
    route: NotificationRoute,
}

pub(super) trait NotificationSink {
    fn request(&mut self, notification: SystemNotification);
}

impl<F> NotificationSink for F
where
    F: FnMut(SystemNotification),
{
    fn request(&mut self, notification: SystemNotification) {
        self(notification);
    }
}

#[derive(Clone)]
struct NotificationRuntime {
    store: NotificationStore,
    registry: NotificationRegistry,
    preferences: HashMap<String, bool>,
    routes: HashMap<String, NotificationRoute>,
    route_order: VecDeque<String>,
}

pub(super) struct BootstrapWork {
    lifetime: u64,
    root: PathBuf,
}

pub(super) struct BootstrapCompletion {
    lifetime: u64,
    result: Result<NotificationRuntime, String>,
}

impl BootstrapWork {
    pub(super) fn run(self) -> BootstrapCompletion {
        let result = (|| {
            let store = NotificationStore::open(
                self.root.join("notifications"),
                NotificationStoreLimits::default(),
            )?;
            let registry = NotificationRegistry::open(self.root.join("notification-ui"))?;
            let preferences = registry.preferences()?;
            let routes = registry.routes()?;
            Ok(NotificationRuntime {
                store,
                registry,
                preferences,
                route_order: routes.iter().map(|(tag, _)| tag.clone()).collect(),
                routes: routes.into_iter().collect(),
            })
        })()
        .map_err(|error: anyhow::Error| format!("{error:#}"));
        BootstrapCompletion {
            lifetime: self.lifetime,
            result,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerGateReason {
    PollInterval,
    RateLimit,
}

#[derive(Default)]
struct NotificationServerGate {
    not_before: Option<Instant>,
    suspended: bool,
    reason: Option<ServerGateReason>,
}

impl NotificationServerGate {
    fn allows(&self, now: Instant) -> bool {
        !self.suspended && self.not_before.is_none_or(|deadline| now >= deadline)
    }

    fn apply(
        &mut self,
        delay: &NotificationDelay,
        reason: ServerGateReason,
        now: Instant,
        wall_now: SystemTime,
    ) {
        let deadline = match delay {
            NotificationDelay::Seconds(seconds) => now.checked_add(Duration::from_secs(*seconds)),
            NotificationDelay::UntilUnixSeconds(unix) => wall_now
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|wall| unix.saturating_sub(wall.as_secs()))
                .and_then(|seconds| now.checked_add(Duration::from_secs(seconds))),
            NotificationDelay::Suspend => None,
        };
        let Some(deadline) = deadline else {
            self.suspended = true;
            self.reason = Some(reason);
            return;
        };
        if self.not_before.is_none_or(|current| deadline > current) {
            self.not_before = Some(deadline);
            self.reason = Some(reason);
        }
    }

    fn notice(&self, now: Instant) -> Option<&'static str> {
        let active = self.suspended || self.not_before.is_some_and(|deadline| deadline > now);
        if !active {
            return None;
        }
        match (self.suspended, self.reason) {
            (true, Some(ServerGateReason::PollInterval)) => Some(
                "Notification polling is paused because GitHub returned an unrepresentable polling interval.",
            ),
            (true, Some(ServerGateReason::RateLimit)) => Some(
                "Notification polling is paused because GitHub returned an unrepresentable rate-limit delay.",
            ),
            (false, Some(ServerGateReason::PollInterval)) => {
                Some("Notification refresh is deferred by GitHub's polling interval.")
            }
            (false, Some(ServerGateReason::RateLimit)) => {
                Some("Notification refresh is deferred by GitHub rate limiting.")
            }
            _ => None,
        }
    }
}

#[derive(Default)]
struct AccountViewState {
    account: Option<Account>,
    selection: String,
    selected_repositories: HashSet<NotificationRepositoryScope>,
    generation: u64,
    selection_generation: u64,
    mark_generation: u64,
    in_flight: bool,
    active_poll_generation: Option<u64>,
    snapshot: Option<NotificationSnapshot>,
    stale_notice: Option<String>,
    page: usize,
    displayed: Option<DisplayedNotificationSet>,
    consent_generation: u64,
    consent_enabled: bool,
    consent_transition: bool,
    conditional_cache: NotificationConditionalCache,
    server_gate: NotificationServerGate,
}

pub(super) struct NotificationController {
    lifetime: u64,
    data_root: PathBuf,
    runtime: Option<NotificationRuntime>,
    accounts: BTreeMap<String, AccountViewState>,
    open: bool,
    initialization_error: Option<String>,
    queued: Vec<QueuedNotification>,
    routes: HashMap<String, NotificationRoute>,
    route_order: VecDeque<String>,
}

impl NotificationController {
    pub(super) fn new(data_root: PathBuf) -> Self {
        Self {
            lifetime: NEXT_LIFETIME.fetch_add(1, Ordering::Relaxed),
            data_root,
            runtime: None,
            accounts: BTreeMap::new(),
            open: false,
            initialization_error: None,
            queued: Vec::new(),
            routes: HashMap::new(),
            route_order: VecDeque::new(),
        }
    }

    pub(super) fn begin_bootstrap(&self) -> BootstrapWork {
        BootstrapWork {
            lifetime: self.lifetime,
            root: self.data_root.clone(),
        }
    }

    pub(super) fn complete_bootstrap(&mut self, completion: BootstrapCompletion) -> bool {
        if completion.lifetime != self.lifetime {
            return false;
        }
        match completion.result {
            Ok(runtime) => {
                for (key, enabled) in &runtime.preferences {
                    self.accounts
                        .entry(key.clone())
                        .or_default()
                        .consent_enabled = *enabled;
                }
                self.routes = runtime.routes.clone();
                self.route_order = runtime.route_order.clone();
                self.runtime = Some(runtime);
                self.initialization_error = None;
                true
            }
            Err(error) => {
                self.initialization_error = Some(format!(
                    "Local notification state unavailable; no OS notification requested: {error}"
                ));
                false
            }
        }
    }

    pub(super) fn is_ready(&self) -> bool {
        self.runtime.is_some()
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn toggle_open(&mut self) {
        self.open = !self.open;
        if self.open {
            self.capture_displayed();
        }
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        for state in self.accounts.values_mut() {
            state.displayed = None;
        }
        // Closing is presentation-only; unread state is intentionally unchanged.
    }

    pub(super) fn unread_count(&self) -> usize {
        self.accounts
            .values()
            .filter_map(|state| state.snapshot.as_ref())
            .flat_map(|snapshot| &snapshot.unread_by_pull_request)
            .map(|summary| summary.unread_events.len())
            .sum()
    }

    pub(super) fn unread_for(&self, account: &Account, owner: &str, repo: &str, pr: u64) -> usize {
        self.accounts
            .get(&account_key(account))
            .and_then(|state| state.snapshot.as_ref())
            .and_then(|snapshot| {
                snapshot.unread_by_pull_request.iter().find(|summary| {
                    summary.target.account.eq_ignore_ascii_case(&account.login)
                        && summary.target.owner.eq_ignore_ascii_case(owner)
                        && summary.target.repository.eq_ignore_ascii_case(repo)
                        && summary.target.pull_request == pr
                })
            })
            .map(|summary| summary.unread_events.len())
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(super) fn begin_polls(&mut self, repositories: &[Repository]) -> Vec<PollWork> {
        self.begin_polls_when_at(repositories, Instant::now(), |_| true)
    }

    #[cfg(test)]
    pub(super) fn begin_polls_when(
        &mut self,
        repositories: &[Repository],
        is_due: impl FnMut(&Account) -> bool,
    ) -> Vec<PollWork> {
        self.begin_polls_when_at(repositories, Instant::now(), is_due)
    }

    pub(super) fn begin_polls_when_at(
        &mut self,
        repositories: &[Repository],
        now: Instant,
        mut is_due: impl FnMut(&Account) -> bool,
    ) -> Vec<PollWork> {
        let Some(runtime) = self.runtime.clone() else {
            return Vec::new();
        };
        let mut grouped: BTreeMap<String, (Account, Vec<Repository>)> = BTreeMap::new();
        for repository in repositories {
            let key = account_key(&repository.account);
            grouped
                .entry(key)
                .or_insert_with(|| (repository.account.clone(), Vec::new()))
                .1
                .push(repository.clone());
        }
        let selected_keys: HashSet<String> = grouped.keys().cloned().collect();
        for (key, state) in &mut self.accounts {
            if !selected_keys.contains(key) {
                state.selection.clear();
                state.selected_repositories.clear();
                state.selection_generation = state.selection_generation.saturating_add(1);
                state.generation = state.generation.saturating_add(1);
                state.displayed = None;
                state.snapshot = None;
                state.conditional_cache = NotificationConditionalCache::default();
                self.queued.retain(|queued| queued.account_key != *key);
            }
        }

        let mut work = Vec::new();
        for (key, (account, mut repositories)) in grouped {
            repositories.sort_by_key(Repository::cache_key);
            repositories.dedup_by_key(|repository| repository.cache_key());
            let selection = selection_key(&repositories);
            let state = self.accounts.entry(key.clone()).or_default();
            state.account = Some(account.clone());
            state.consent_enabled = runtime
                .preferences
                .get(&key)
                .copied()
                .unwrap_or(state.consent_enabled);
            state.selected_repositories = repositories.iter().map(repository_scope).collect();
            if state.selection != selection {
                state.selection = selection.clone();
                state.selection_generation = state.selection_generation.saturating_add(1);
                state.generation = state.generation.saturating_add(1);
                state.displayed = None;
                state.snapshot = None;
                state.conditional_cache = NotificationConditionalCache::default();
                self.queued.retain(|queued| queued.account_key != key);
            }
            if state.in_flight
                || repositories.is_empty()
                || !state.server_gate.allows(now)
                || !is_due(&account)
            {
                continue;
            }
            state.generation = state.generation.saturating_add(1);
            state.in_flight = true;
            state.active_poll_generation = Some(state.generation);
            let token = ControllerToken {
                consent_generation: state.consent_generation,
                lifetime: self.lifetime,
                account_key: key,
                selection,
                generation: state.generation,
            };
            work.push(PollWork {
                token,
                account,
                chunks: repositories.chunks(5).map(<[Repository]>::to_vec).collect(),
                store: runtime.store.clone(),
                cache: state.conditional_cache.clone(),
            });
        }
        work
    }

    pub(super) fn release_poll(&mut self, completion: &PollCompletion) -> bool {
        self.release_poll_at(completion, Instant::now())
    }

    pub(super) fn release_poll_at(&mut self, completion: &PollCompletion, now: Instant) -> bool {
        self.release_poll_at_with_wall(completion, now, SystemTime::now())
    }

    fn release_poll_at_with_wall(
        &mut self,
        completion: &PollCompletion,
        now: Instant,
        wall_now: SystemTime,
    ) -> bool {
        let key = account_key(&completion.account);
        let Some(state) = self.accounts.get_mut(&key) else {
            return false;
        };
        if completion.token.lifetime != self.lifetime
            || completion.token.account_key != key
            || state.active_poll_generation != Some(completion.token.generation)
        {
            return false;
        }
        if state.account.as_ref() == Some(&completion.account) {
            if token_matches(&completion.token, self.lifetime, &key, state)
                && let Some(delay) = &completion.poll.x_poll_interval
            {
                state
                    .server_gate
                    .apply(delay, ServerGateReason::PollInterval, now, wall_now);
            }
            if let Some(delay) = &completion.poll.rate_limit {
                state
                    .server_gate
                    .apply(delay, ServerGateReason::RateLimit, now, wall_now);
            }
        }
        state.active_poll_generation = None;
        state.in_flight = false;
        true
    }

    pub(super) fn accepts_poll(&self, completion: &PollCompletion) -> bool {
        let key = account_key(&completion.account);
        self.accounts
            .get(&key)
            .is_some_and(|state| token_matches(&completion.token, self.lifetime, &key, state))
    }

    pub(super) fn complete_poll(
        &mut self,
        mut completion: PollCompletion,
    ) -> Option<AdmissionWork> {
        self.release_poll(&completion);
        let key = account_key(&completion.account);
        let selection = completion.token.selection.clone();
        let (consent_enabled, consent_transition, consent_generation, selection_generation) =
            {
                let state = self.accounts.get_mut(&key)?;
                if !token_matches(&completion.token, self.lifetime, &key, state) {
                    return None;
                }
                state.in_flight = false;
                if state.snapshot.as_ref().is_some_and(|current| {
                    current.state_version > completion.snapshot.state_version
                }) {
                    state.in_flight = false;
                    return None;
                }
                project_snapshot(&mut completion.snapshot, &state.selected_repositories);
                completion.newly_admitted.retain(|event| {
                    selected_target(&event.identity.target, &state.selected_repositories)
                });
                state.conditional_cache = completion.cache;
                state.snapshot = Some(completion.snapshot);
                state.stale_notice = completion.error;
                state.page = state.page.min(max_page(state.snapshot.as_ref()));
                (
                    state.consent_enabled,
                    state.consent_transition,
                    state.consent_generation,
                    state.selection_generation,
                )
            };
        if self.open {
            self.capture_displayed();
        }
        if completion.newly_admitted.is_empty()
            || !consent_enabled
            || consent_transition
            || completion.token.consent_generation != consent_generation
        {
            return None;
        }
        let runtime = self.runtime.as_ref()?;
        Some(AdmissionWork {
            token: AdmissionToken {
                lifetime: self.lifetime,
                account_key: key,
                selection,
                selection_generation,
                consent_generation,
            },
            account: completion.account,
            events: completion.newly_admitted,
            registry: runtime.registry.clone(),
        })
    }

    pub(super) fn complete_admission(&mut self, completion: AdmissionCompletion) -> bool {
        let Some(state) = self.accounts.get_mut(&completion.token.account_key) else {
            return false;
        };
        if completion.token.lifetime != self.lifetime
            || completion.token.selection.is_empty()
            || completion.token.selection != state.selection
            || completion.token.selection_generation != state.selection_generation
            || completion.token.consent_generation != state.consent_generation
            || !state.consent_enabled
            || state.consent_transition
        {
            return false;
        }
        if let Err(error) = completion.result {
            state.stale_notice = Some(format!(
                "Unread state saved, but OS delivery was suppressed because the local tag registry failed: {error}"
            ));
            return false;
        }
        for mut notification in completion.notifications {
            notification.consent_generation = state.consent_generation;
            notification.selection = state.selection.clone();
            notification.selection_generation = state.selection_generation;
            let tag = notification.notification.tag.to_string();
            self.route_order.retain(|existing| existing != &tag);
            self.route_order.push_back(tag.clone());
            self.routes.insert(tag, notification.route.clone());
            while self.route_order.len() > MAX_RETAINED_ROUTES {
                if let Some(expired) = self.route_order.pop_front() {
                    self.routes.remove(&expired);
                }
            }
            self.queued.push(notification);
        }
        true
    }

    fn take_system_notifications(&mut self) -> Vec<QueuedNotification> {
        let queued = std::mem::take(&mut self.queued);
        queued
            .into_iter()
            .filter_map(|queued| {
                let state = self.accounts.get(&queued.account_key)?;
                (state.consent_enabled
                    && !state.consent_transition
                    && state.consent_generation == queued.consent_generation)
                    .then_some(state)
                    .filter(|state| {
                        !queued.selection.is_empty()
                            && state.selection == queued.selection
                            && state.selection_generation == queued.selection_generation
                    })
                    .map(|_| queued)
            })
            .collect()
    }

    pub(super) fn dispatch_to(&mut self, sink: &mut impl NotificationSink) -> usize {
        let notifications = self.take_system_notifications();
        let count = notifications.len();
        for queued in notifications {
            // Admission acquires this lease in the background. Keep it until
            // the platform request returns so another process cannot complete
            // a durable disable between the consent read and this request.
            let QueuedNotification {
                notification,
                delivery_lease,
                ..
            } = queued;
            sink.request(notification);
            drop(delivery_lease);
        }
        count
    }

    pub(super) fn begin_consent(
        &mut self,
        account: &Account,
        enabled: bool,
    ) -> Option<ConsentWork> {
        let runtime = self.runtime.as_ref()?;
        let key = account_key(account);
        let state = self.accounts.entry(key.clone()).or_default();
        if state.consent_transition || state.consent_enabled == enabled {
            return None;
        }
        state.account = Some(account.clone());
        state.consent_generation = state.consent_generation.saturating_add(1);
        state.consent_transition = true;
        self.queued.retain(|queued| queued.account_key != key);
        Some(ConsentWork {
            token: ConsentToken {
                lifetime: self.lifetime,
                account_key: key,
                generation: state.consent_generation,
            },
            account: account.clone(),
            enabled,
            registry: runtime.registry.clone(),
        })
    }

    pub(super) fn begin_toggle_consent(&mut self, account: &Account) -> Option<ConsentWork> {
        let enabled = !self
            .accounts
            .get(&account_key(account))
            .is_some_and(|state| state.consent_enabled);
        self.begin_consent(account, enabled)
    }

    pub(super) fn complete_consent(&mut self, completion: ConsentCompletion) -> bool {
        let key = account_key(&completion.account);
        let Some(state) = self.accounts.get_mut(&key) else {
            return false;
        };
        if completion.token.lifetime != self.lifetime
            || completion.token.account_key != key
            || completion.token.generation != state.consent_generation
        {
            return false;
        }
        state.consent_transition = false;
        match completion.result {
            Ok(()) => {
                state.consent_enabled = completion.enabled;
                state.stale_notice = Some(if completion.enabled {
                    "macOS alerts enabled for future exact events. Permission and delivery remain best effort; no historical catch-up was requested."
                        .into()
                } else {
                    "macOS alerts disabled. Pending UI dispatch was cancelled; in-app unread is unchanged."
                        .into()
                });
                if let Some(runtime) = &mut self.runtime {
                    runtime.preferences.insert(key, completion.enabled);
                }
                true
            }
            Err(error) => {
                state.stale_notice = Some(format!(
                    "Preference was not changed; no OS notification requested: {error}"
                ));
                false
            }
        }
    }

    pub(super) fn begin_mark_displayed(&mut self, account: &Account) -> Option<MarkReadWork> {
        let runtime = self.runtime.as_ref()?;
        let key = account_key(account);
        let state = self.accounts.get_mut(&key)?;
        let displayed = state.displayed.clone()?;
        state.mark_generation = state.mark_generation.saturating_add(1);
        Some(MarkReadWork {
            token: MarkToken {
                lifetime: self.lifetime,
                account_key: key,
                selection: state.selection.clone(),
                selection_generation: state.selection_generation,
                generation: state.mark_generation,
            },
            account: account.clone(),
            displayed,
            store: runtime.store.clone(),
        })
    }

    pub(super) fn complete_mark_displayed(&mut self, completion: MarkReadCompletion) -> bool {
        let key = account_key(&completion.account);
        let Some(state) = self.accounts.get_mut(&key) else {
            return false;
        };
        if completion.token.lifetime != self.lifetime
            || completion.token.account_key != key
            || completion.token.selection != state.selection
            || completion.token.selection_generation != state.selection_generation
            || completion.token.generation != state.mark_generation
        {
            return false;
        }
        match completion.result {
            Ok(mut snapshot) => {
                project_snapshot(&mut snapshot, &state.selected_repositories);
                if state
                    .snapshot
                    .as_ref()
                    .is_none_or(|current| current.state_version <= snapshot.state_version)
                {
                    state.snapshot = Some(snapshot);
                }
                state.displayed = None;
                self.capture_displayed();
                true
            }
            Err(error) => {
                state.stale_notice = Some(format!(
                    "Displayed events remain unread because the exact mark-read save failed: {error}"
                ));
                false
            }
        }
    }

    pub(super) fn route_for_tag(&self, tag: &str) -> Option<NotificationRoute> {
        self.routes.get(tag).cloned()
    }

    pub(super) fn next_page(&mut self, account: &Account) {
        if let Some(state) = self.accounts.get_mut(&account_key(account)) {
            state.page = (state.page + 1).min(max_page(state.snapshot.as_ref()));
        }
        self.capture_displayed();
    }

    pub(super) fn previous_page(&mut self, account: &Account) {
        if let Some(state) = self.accounts.get_mut(&account_key(account)) {
            state.page = state.page.saturating_sub(1);
        }
        self.capture_displayed();
    }

    fn capture_displayed(&mut self) {
        for state in self.accounts.values_mut() {
            let Some(snapshot) = &state.snapshot else {
                state.displayed = None;
                continue;
            };
            let events = flattened_events(snapshot)
                .skip(state.page * PAGE_SIZE)
                .take(PAGE_SIZE)
                .map(|event| event.identity.clone())
                .collect::<Vec<_>>();
            state.displayed = (!events.is_empty()).then_some(DisplayedNotificationSet {
                state_version: snapshot.state_version,
                events,
            });
        }
    }

    pub(super) fn render(&self, colors: Palette, cx: &mut Context<Root>) -> Div {
        let sections = self.accounts.values().filter_map(|state| {
            let account = state.account.clone()?;
            let snapshot = state.snapshot.as_ref();
            let event_count = snapshot.map(unread_count).unwrap_or(0);
            let incomplete_count = snapshot.map(|value| value.incomplete_candidates.len()).unwrap_or(0);
            let account_for_mark = account.clone();
            let account_for_previous = account.clone();
            let account_for_next = account.clone();
            let account_for_consent = account.clone();
            let event_rows = snapshot
                .into_iter()
                .flat_map(flattened_events)
                .skip(state.page * PAGE_SIZE)
                .take(PAGE_SIZE)
                .map(|event| render_event(event, colors, cx))
                .collect::<Vec<_>>();
            let incomplete_rows = snapshot
                .into_iter()
                .flat_map(|value| value.incomplete_candidates.iter())
                .take(12)
                .map(|candidate| render_incomplete(candidate, colors))
                .collect::<Vec<_>>();
            let completeness = snapshot
                .into_iter()
                .flat_map(|value| value.repository_completeness.iter())
                .map(|repository| render_completeness(repository, colors))
                .collect::<Vec<_>>();
            Some(
                div()
                    .mb(px(ui::GAP_PAGE))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(format!(
                                "{} · {} unread",
                                account.login, event_count
                            )))
                            .child(
                                div()
                                    .id(SharedString::from(format!("notification-consent-{}", account.login)))
                                    .control()
                                    .cursor_pointer()
                                    .ui_text(TextRole::Caption)
                                    .text_color(colors.accent)
                                    .child(if state.consent_transition {
                                        "Saving macOS preference…"
                                    } else if state.consent_enabled {
                                        "macOS alerts: on"
                                    } else {
                                        "macOS alerts: off"
                                    })
                                    .on_click(cx.listener(move |root, _, _, cx| {
                                        if let Root::Review(this) = root {
                                            this.set_notification_consent(&account_for_consent, cx);
                                        }
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .mt_1()
                            .ui_text(TextRole::Caption)
                            .text_color(colors.faint)
                            .child("In-app unread is always available. macOS alerts are opt-in and requested best effort; this app receives no permission or delivery receipt."),
                    )
                    .children(completeness)
                    .when_some(state.stale_notice.clone(), |section, notice| {
                        section.child(div().mt_2().ui_text(TextRole::Caption).text_color(colors.amber).child(notice))
                    })
                    .when_some(state.server_gate.notice(Instant::now()), |section, notice| {
                        section.child(div().mt_2().ui_text(TextRole::Caption).text_color(colors.amber).child(notice))
                    })
                    .child(div().mt_3().children(event_rows))
                    .when(event_count == 0, |section| {
                        section.child(div().mt_3().text_color(colors.muted).child("No proven unread events."))
                    })
                    .when(incomplete_count > 0, |section| {
                        section
                            .child(div().mt(px(ui::GAP_PAGE)).ui_text(TextRole::Caption).font_weight(gpui::FontWeight::SEMIBOLD).text_color(colors.amber).child(format!("INCOMPLETE CANDIDATES · {incomplete_count} (not counted unread)")))
                            .children(incomplete_rows)
                    })
                    .child(
                        div()
                            .mt_3()
                            .flex()
                            .gap(px(ui::GAP_COLUMNS))
                            .child(panel_action("Previous", colors).on_click(cx.listener(move |root, _, _, cx| {
                                if let Root::Review(this) = root {
                                    this.notifications.previous_page(&account_for_previous);
                                    cx.notify();
                                }
                            })))
                            .child(div().py_1().ui_text(TextRole::Caption).text_color(colors.faint).child(format!("Page {}", state.page + 1)))
                            .child(panel_action("Next", colors).on_click(cx.listener(move |root, _, _, cx| {
                                if let Root::Review(this) = root {
                                    this.notifications.next_page(&account_for_next);
                                    cx.notify();
                                }
                            })))
                            .child(panel_action("Mark displayed read", colors).on_click(cx.listener(move |root, _, _, cx| {
                                if let Root::Review(this) = root {
                                    this.mark_displayed_notifications(&account_for_mark, cx);
                                }
                            }))),
                    ),
            )
        }).collect::<Vec<_>>();

        div()
            .absolute()
            .inset_0()
            .flex()
            .justify_end()
            .bg(gpui::rgba(0x00000055))
            .child(
                div()
                    .id("notifications-panel")
                    .w(px(520.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .border_l_1()
                    .border_color(colors.border)
                    .bg(colors.surface)
                    .shadow_lg()
                    .child(
                        div()
                            .h(px(48.))
                            .px(px(ui::PANEL_GUTTER))
                            .flex()
                            .items_center()
                            .justify_between()
                            .border_b_1()
                            .border_color(colors.border)
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Unread notifications"),
                            )
                            .child(panel_action("Close", colors).on_click(cx.listener(
                                |root, _, _, cx| {
                                    if let Root::Review(this) = root {
                                        this.notifications.close();
                                        cx.notify();
                                    }
                                },
                            ))),
                    )
                    .child(
                        div()
                            .id("notification-panel-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p(px(ui::PANEL_GUTTER))
                            .when(self.runtime.is_none(), |body| {
                                body.child(div().text_color(colors.muted).child(
                                    self.initialization_error.clone().unwrap_or_else(|| {
                                        "Loading private notification state…".into()
                                    }),
                                ))
                            })
                            .children(sections),
                    ),
            )
    }

    #[cfg(feature = "ui-smoke")]
    pub(super) fn install_smoke_fixture(&mut self) -> Result<String, String> {
        use cibergit::providers::notifications::{
            IncompleteCandidateKind, NotificationEventIdentity, NotificationEventSource,
            NotificationEvidence,
        };
        if self.runtime.is_none() {
            return Err("notification runtime is not ready".into());
        }
        let account = Account {
            host: "github.com".into(),
            login: "smoke-reader".into(),
        };
        let repository = Repository {
            host: "github.com".into(),
            owner: "octo".into(),
            name: "native-notifications".into(),
            account: account.clone(),
            local_path: None,
        };
        let target = NotificationPullRequest {
            provider: "github".into(),
            host: "github.com".into(),
            account: account.login.clone(),
            owner: repository.owner.clone(),
            repository: repository.name.clone(),
            pull_request: 7152,
        };
        let event = ProviderNotificationEvent {
            identity: NotificationEventIdentity {
                target: target.clone(),
                source: NotificationEventSource::Timeline,
                remote_event_id: "native-smoke-review-request-2".into(),
            },
            occurred_at: "2026-09-13T12:15:00Z".into(),
            actor: Some("reviewer".into()),
            alert_kind: NotificationAlertKind::ReviewRequest,
            summary: "A reviewer requested your review on the newly observed event.".into(),
            url: "https://github.com/octo/native-notifications/pull/7152".into(),
            evidence: NotificationEvidence::ReviewRequested {
                timeline_event_id: "native-smoke-review-request-2".into(),
                requested_reviewer: account.login.clone(),
            },
        };
        let partial_repository = Repository {
            name: "partial-repository".into(),
            ..repository.clone()
        };
        let interval_account = Account {
            host: "github.com".into(),
            login: "smoke-poll-reader".into(),
        };
        let interval_repository = Repository {
            host: "github.com".into(),
            owner: "octo".into(),
            name: "poll-interval".into(),
            account: interval_account.clone(),
            local_path: None,
        };
        let selected = [
            repository.clone(),
            partial_repository,
            interval_repository.clone(),
        ];
        let mut work = self.begin_polls_when_at(&selected, Instant::now(), |_| true);
        let interval_token = work
            .iter()
            .find(|work| work.account == interval_account)
            .ok_or("smoke controller did not begin its polling-interval account")?
            .token
            .clone();
        let token = work
            .iter()
            .find(|work| work.account == account)
            .ok_or("smoke controller did not begin its rate-limit account")?
            .token
            .clone();
        work.clear();
        let snapshot = NotificationSnapshot {
            state_version: 2,
            unread_by_pull_request: vec![PullRequestUnreadSummary {
                target: target.clone(),
                unread_events: vec![event.clone()],
                unknown_candidates: Vec::new(),
            }],
            repository_completeness: vec![
                RepositoryNotificationCompleteness {
                    target: repository_scope(&repository),
                    complete: true,
                    reasons: Vec::new(),
                },
                RepositoryNotificationCompleteness {
                    target: NotificationRepositoryScope {
                        repository: "partial-repository".into(),
                        ..repository_scope(&repository)
                    },
                    complete: false,
                    reasons: vec!["Provider evidence page bound reached".into()],
                },
            ],
            incomplete_candidates: vec![IncompleteNotificationCandidate {
                target: Some(target),
                provider_notification_id: Some("candidate-without-proof".into()),
                kind: IncompleteCandidateKind::Mention,
                reason: "Mention evidence unavailable; notification reason/body is not authoritative proof."
                    .into(),
            }],
            notices: Vec::new(),
        };
        let admission = self.complete_poll(PollCompletion {
            token,
            account,
            snapshot,
            newly_admitted: vec![event],
            error: None,
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective {
                x_poll_interval: None,
                rate_limit: Some(NotificationDelay::Seconds(300)),
            },
        });
        if admission.is_some() {
            return Err("default-off smoke unexpectedly admitted an OS alert".into());
        }
        let interval_admission = self.complete_poll(PollCompletion {
            token: interval_token,
            account: interval_account,
            snapshot: NotificationSnapshot {
                state_version: 1,
                unread_by_pull_request: Vec::new(),
                repository_completeness: vec![RepositoryNotificationCompleteness {
                    target: repository_scope(&interval_repository),
                    complete: true,
                    reasons: Vec::new(),
                }],
                incomplete_candidates: Vec::new(),
                notices: Vec::new(),
            },
            newly_admitted: Vec::new(),
            error: None,
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective {
                x_poll_interval: Some(NotificationDelay::Seconds(300)),
                rate_limit: None,
            },
        });
        if interval_admission.is_some() {
            return Err("polling-interval smoke unexpectedly admitted an OS alert".into());
        }
        let mut calls = Vec::new();
        let dispatched = self.dispatch_to(&mut |notification| calls.push(notification));
        if dispatched != 0 || !calls.is_empty() {
            return Err("default-off smoke reached its test sink".into());
        }
        let state = self
            .accounts
            .get(&account_key(&Account {
                host: "github.com".into(),
                login: "smoke-reader".into(),
            }))
            .ok_or("smoke account disappeared")?;
        let snapshot = state.snapshot.as_ref().ok_or("smoke snapshot missing")?;
        if unread_count(snapshot) != 1
            || snapshot.incomplete_candidates.len() != 1
            || snapshot.repository_completeness.len() != 2
        {
            return Err("smoke projection lost expected event or partial evidence".into());
        }
        if !self.open {
            self.toggle_open();
        }
        Ok("Synthetic exact event installed through the real begin/release/complete controller path; separate visible rate-limit and polling-interval notices are active; one incomplete mention candidate remains separate; test sink calls=0; platform calls=0; preference=off."
            .into())
    }
}

fn render_event(
    event: &ProviderNotificationEvent,
    colors: Palette,
    cx: &mut Context<Root>,
) -> Stateful<Div> {
    let target = event.identity.target.clone();
    let label = alert_label(&event.alert_kind);
    div()
        .id(SharedString::from(format!(
            "notification-event-{}",
            stable_event_tag(
                &Account {
                    host: target.host.clone(),
                    login: target.account.clone()
                },
                event
            )
        )))
        .mb_2()
        .p_3()
        .rounded(px(ui::CONTROL_RADIUS))
        .border_1()
        .border_color(colors.border)
        .cursor_pointer()
        .hover(|row| row.bg(colors.selected))
        .child(
            div()
                .flex()
                .justify_between()
                .child(div().font_weight(gpui::FontWeight::MEDIUM).child(label))
                .child(
                    div()
                        .ui_text(TextRole::Caption)
                        .text_color(colors.faint)
                        .child(event.occurred_at.clone()),
                ),
        )
        .child(div().mt_1().child(event.summary.clone()))
        .child(
            div()
                .mt_1()
                .ui_text(TextRole::Caption)
                .text_color(colors.muted)
                .child(format!(
                    "{} · {}/{} #{}{}",
                    target.account,
                    target.owner,
                    target.repository,
                    target.pull_request,
                    event
                        .actor
                        .as_ref()
                        .map(|actor| format!(" · {actor}"))
                        .unwrap_or_default()
                )),
        )
        .on_click(cx.listener(move |root, _, _, cx| {
            if let Root::Review(this) = root {
                this.open_notification_target(&target, cx);
            }
        }))
}

fn render_incomplete(candidate: &IncompleteNotificationCandidate, colors: Palette) -> Div {
    let target = candidate
        .target
        .as_ref()
        .map(|target| {
            format!(
                "{}/{} #{}",
                target.owner, target.repository, target.pull_request
            )
        })
        .unwrap_or_else(|| "Unresolved pull request".into());
    div()
        .mt_2()
        .p_2()
        .rounded(px(ui::CONTROL_RADIUS))
        .bg(colors.elevated)
        .ui_text(TextRole::Caption)
        .child(format!("{target} · {:?}", candidate.kind))
        .child(
            div()
                .mt_1()
                .text_color(colors.muted)
                .child(candidate.reason.clone()),
        )
}

fn render_completeness(repository: &RepositoryNotificationCompleteness, colors: Palette) -> Div {
    div()
        .mt_2()
        .ui_text(TextRole::Caption)
        .text_color(if repository.complete {
            colors.muted
        } else {
            colors.amber
        })
        .child(if repository.complete {
            format!(
                "{}/{} · complete",
                repository.target.owner, repository.target.repository
            )
        } else {
            format!(
                "{}/{} · partial: {}",
                repository.target.owner,
                repository.target.repository,
                repository.reasons.join("; ")
            )
        })
}

fn panel_action(label: &'static str, colors: Palette) -> gpui::Stateful<Div> {
    div()
        .id(SharedString::from(format!("notification-action-{label}")))
        .control()
        .cursor_pointer()
        .ui_text(TextRole::Caption)
        .text_color(colors.accent)
        .hover(|button| button.bg(colors.selected))
        .child(label)
}

fn token_matches(
    token: &ControllerToken,
    lifetime: u64,
    account_key: &str,
    state: &AccountViewState,
) -> bool {
    token.lifetime == lifetime
        && token.account_key == account_key
        && token.selection == state.selection
        && token.generation == state.generation
}

fn combine_batches(
    account: &Account,
    chunks: &[Vec<Repository>],
    batches: Vec<(usize, ProviderNotificationBatch)>,
    failures: &[(usize, String)],
) -> Result<ProviderNotificationBatch> {
    ensure!(!batches.is_empty(), "all bounded repository batches failed");
    let mut observations = Vec::new();
    let mut incomplete_candidates = Vec::new();
    let mut repositories = Vec::new();
    let mut notices = Vec::new();
    let mut observed_at_unix_ms = 0;
    let mut full_snapshot = failures.is_empty();
    let mut complete = failures.is_empty();
    for (index, batch) in batches {
        ensure!(
            batch.account == *account,
            "provider batch changed selected account"
        );
        let expected: HashSet<_> = chunks[index].iter().map(repository_scope).collect();
        let actual: HashSet<_> = batch
            .repositories
            .iter()
            .map(|repo| repo.target.clone())
            .collect();
        ensure!(
            expected == actual,
            "provider batch changed selected repository scope"
        );
        observed_at_unix_ms = observed_at_unix_ms.max(batch.observed_at_unix_ms);
        full_snapshot &= batch.full_snapshot;
        complete &= batch.complete;
        observations.extend(batch.observations);
        incomplete_candidates.extend(batch.incomplete_candidates);
        repositories.extend(batch.repositories);
        notices.extend(batch.notices);
    }
    for (index, error) in failures {
        for repository in &chunks[*index] {
            repositories.push(RepositoryNotificationCompleteness {
                target: repository_scope(repository),
                complete: false,
                reasons: vec![format!("Bounded batch read failed: {error}")],
            });
        }
    }
    if !failures.is_empty() {
        notices.push(format_failures(failures, chunks.len()));
    }
    repositories.sort_by_key(|repository| scope_key(&repository.target));
    incomplete_candidates
        .sort_by_key(|candidate| serde_json::to_string(candidate).unwrap_or_default());
    Ok(ProviderNotificationBatch {
        account: account.clone(),
        observed_at_unix_ms,
        observations,
        incomplete_candidates,
        full_snapshot,
        repositories,
        complete,
        notices,
    })
}

fn selected_target(
    target: &NotificationPullRequest,
    selected: &HashSet<NotificationRepositoryScope>,
) -> bool {
    selected.iter().any(|scope| {
        scope.provider == target.provider
            && scope.host.eq_ignore_ascii_case(&target.host)
            && scope.account.eq_ignore_ascii_case(&target.account)
            && scope.owner.eq_ignore_ascii_case(&target.owner)
            && scope.repository.eq_ignore_ascii_case(&target.repository)
    })
}

fn project_snapshot(
    snapshot: &mut NotificationSnapshot,
    selected: &HashSet<NotificationRepositoryScope>,
) {
    snapshot
        .unread_by_pull_request
        .retain(|summary| selected_target(&summary.target, selected));
    snapshot
        .repository_completeness
        .retain(|repository| selected.contains(&repository.target));
    snapshot.incomplete_candidates.retain(|candidate| {
        candidate
            .target
            .as_ref()
            .is_none_or(|target| selected_target(target, selected))
    });
}

fn snapshot_from_outcome(outcome: &NotificationReconcileOutcome) -> NotificationSnapshot {
    NotificationSnapshot {
        state_version: outcome.state_version,
        unread_by_pull_request: outcome.unread_by_pull_request.clone(),
        repository_completeness: outcome.repository_completeness.clone(),
        incomplete_candidates: outcome.incomplete_candidates.clone(),
        notices: outcome.notices.clone(),
    }
}

fn empty_snapshot(notice: String) -> NotificationSnapshot {
    NotificationSnapshot {
        state_version: 0,
        unread_by_pull_request: Vec::new(),
        repository_completeness: Vec::new(),
        incomplete_candidates: Vec::new(),
        notices: vec![notice],
    }
}

fn format_failures(failures: &[(usize, String)], total: usize) -> String {
    format!(
        "{} of {total} bounded repository batches failed; successful batches were reconciled as partial and cached unread was retained for failures",
        failures.len()
    )
}

fn flattened_events(
    snapshot: &NotificationSnapshot,
) -> impl Iterator<Item = &ProviderNotificationEvent> {
    snapshot
        .unread_by_pull_request
        .iter()
        .flat_map(|summary: &PullRequestUnreadSummary| summary.unread_events.iter())
}

fn unread_count(snapshot: &NotificationSnapshot) -> usize {
    flattened_events(snapshot).count()
}

fn max_page(snapshot: Option<&NotificationSnapshot>) -> usize {
    snapshot.map(unread_count).unwrap_or(0).saturating_sub(1) / PAGE_SIZE
}

fn alert_label(kind: &NotificationAlertKind) -> &'static str {
    match kind {
        NotificationAlertKind::ReviewRequest => "Review requested",
        NotificationAlertKind::Mention => "Proven mention",
        NotificationAlertKind::Reply => "Review-thread reply",
        NotificationAlertKind::FailedCheckOwnPullRequest => "Failed check on your pull request",
    }
}

fn account_key(account: &Account) -> String {
    serde_json::to_string(&(
        account.host.to_ascii_lowercase(),
        account.login.to_ascii_lowercase(),
    ))
    .expect("string tuple")
}

pub(super) fn schedule_key(account: &Account) -> String {
    format!("notifications:{}", account_key(account))
}

fn selection_key(repositories: &[Repository]) -> String {
    let mut keys = repositories
        .iter()
        .map(Repository::cache_key)
        .collect::<Vec<_>>();
    keys.sort();
    let mut digest = Sha256::new();
    for key in keys {
        digest.update(key.as_bytes());
        digest.update([0]);
    }
    format!("{:x}", digest.finalize())
}

fn repository_scope(repository: &Repository) -> NotificationRepositoryScope {
    NotificationRepositoryScope {
        provider: "github".into(),
        host: repository.host.clone(),
        account: repository.account.login.clone(),
        owner: repository.owner.clone(),
        repository: repository.name.clone(),
    }
}

fn scope_key(scope: &NotificationRepositoryScope) -> String {
    serde_json::to_string(scope).expect("serializable scope")
}

fn stable_event_tag(account: &Account, event: &ProviderNotificationEvent) -> String {
    let mut digest = Sha256::new();
    digest.update(account_key(account));
    digest.update([0]);
    digest.update(serde_json::to_vec(&event.identity).expect("serializable event identity"));
    format!("cibergit-notification-{:x}", digest.finalize())
}

fn system_notification(account: &Account, event: &ProviderNotificationEvent) -> SystemNotification {
    SystemNotification {
        tag: stable_event_tag(account, event).into(),
        title: format!(
            "{} · {}/{} #{}",
            alert_label(&event.alert_kind),
            event.identity.target.owner,
            event.identity.target.repository,
            event.identity.target.pull_request
        )
        .into(),
        body: event.summary.clone().into(),
        actions: Vec::new(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RegistryFile {
    schema_version: u32,
    revision: u64,
    #[serde(default)]
    enabled: HashMap<String, bool>,
    #[serde(default)]
    delivered_tags: Vec<String>,
    #[serde(default)]
    routes: Vec<PersistedRoute>,
}

impl Default for RegistryFile {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_VERSION,
            revision: 0,
            enabled: HashMap::new(),
            delivered_tags: Vec::new(),
            routes: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedRoute {
    tag: String,
    account: Account,
    target: NotificationPullRequest,
}

#[derive(Clone, Debug)]
struct NotificationRegistry {
    root: PathBuf,
}

impl NotificationRegistry {
    fn open(root: PathBuf) -> Result<Self> {
        if !root.exists() {
            fs::create_dir_all(&root).context("Cannot create private notification UI directory")?;
        }
        let metadata = fs::symlink_metadata(&root)
            .context("Cannot inspect private notification UI directory")?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Notification UI root must be a real directory"
        );
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .context("Cannot make notification UI directory private")?;
        let registry = Self { root };
        let _lock = RegistryLock::acquire(&registry.lock_path())?;
        let _ = registry.load_unlocked()?;
        Ok(registry)
    }

    fn preferences(&self) -> Result<HashMap<String, bool>> {
        let _lock = RegistryLock::acquire(&self.lock_path())?;
        Ok(self.load_unlocked()?.enabled)
    }

    fn routes(&self) -> Result<Vec<(String, NotificationRoute)>> {
        let _lock = RegistryLock::acquire(&self.lock_path())?;
        Ok(self
            .load_unlocked()?
            .routes
            .into_iter()
            .map(|route| {
                (
                    route.tag,
                    NotificationRoute {
                        account: route.account,
                        owner: route.target.owner,
                        repository: route.target.repository,
                        pull_request: route.target.pull_request,
                    },
                )
            })
            .collect())
    }

    fn set_enabled(&self, account: &Account, enabled: bool) -> Result<()> {
        self.update(|file| {
            file.enabled.insert(account_key(account), enabled);
            Ok(())
        })
    }

    fn reserve_for_delivery(
        &self,
        account: &Account,
        events: &[ProviderNotificationEvent],
    ) -> Result<Vec<QueuedNotification>> {
        let delivery_lease = Arc::new(RegistryLock::acquire(&self.lock_path())?);
        let mut file = self.load_unlocked()?;
        let expected = file.revision;
        let mut queued = Vec::new();
        ensure!(
            file.enabled
                .get(&account_key(account))
                .copied()
                .unwrap_or(false),
            "macOS consent is no longer enabled"
        );
        let mut known: HashSet<String> = file.delivered_tags.iter().cloned().collect();
        for event in events {
            ensure!(
                event
                    .identity
                    .target
                    .account
                    .eq_ignore_ascii_case(&account.login),
                "event account differs from notification consent account"
            );
            let tag = stable_event_tag(account, event);
            if !known.insert(tag.clone()) {
                continue;
            }
            file.delivered_tags.push(tag.clone());
            file.routes.retain(|route| route.tag != tag);
            file.routes.push(PersistedRoute {
                tag: tag.clone(),
                account: account.clone(),
                target: event.identity.target.clone(),
            });
            queued.push(QueuedNotification {
                delivery_lease: Some(delivery_lease.clone()),
                account_key: account_key(account),
                consent_generation: 0,
                selection: String::new(),
                selection_generation: 0,
                notification: system_notification(account, event),
                route: NotificationRoute {
                    account: account.clone(),
                    owner: event.identity.target.owner.clone(),
                    repository: event.identity.target.repository.clone(),
                    pull_request: event.identity.target.pull_request,
                },
            });
        }
        if file.delivered_tags.len() > MAX_RETAINED_TAGS {
            let drain = file.delivered_tags.len() - MAX_RETAINED_TAGS;
            file.delivered_tags.drain(0..drain);
        }
        if file.routes.len() > MAX_RETAINED_ROUTES {
            let drain = file.routes.len() - MAX_RETAINED_ROUTES;
            file.routes.drain(0..drain);
        }
        file.revision = expected
            .checked_add(1)
            .context("Notification registry revision exhausted")?;
        self.save_cas_unlocked(expected, &file)?;
        Ok(queued)
    }

    #[cfg(test)]
    fn reserve_new_events(
        &self,
        account: &Account,
        events: &[ProviderNotificationEvent],
    ) -> Result<Vec<QueuedNotification>> {
        let mut queued = self.reserve_for_delivery(account, events)?;
        for notification in &mut queued {
            notification.delivery_lease = None;
        }
        Ok(queued)
    }

    #[cfg(test)]
    fn route_for_tag(&self, tag: &str) -> Result<Option<NotificationRoute>> {
        ensure!(tag.len() <= 128, "notification tag exceeds local bound");
        let _lock = RegistryLock::acquire(&self.lock_path())?;
        Ok(self
            .load_unlocked()?
            .routes
            .into_iter()
            .find(|route| route.tag == tag)
            .map(|route| NotificationRoute {
                account: route.account,
                owner: route.target.owner,
                repository: route.target.repository,
                pull_request: route.target.pull_request,
            }))
    }

    fn update(&self, mut operation: impl FnMut(&mut RegistryFile) -> Result<()>) -> Result<()> {
        for _ in 0..MAX_CAS_ATTEMPTS {
            let _lock = RegistryLock::acquire(&self.lock_path())?;
            let mut file = self.load_unlocked()?;
            let expected = file.revision;
            operation(&mut file)?;
            file.revision = expected
                .checked_add(1)
                .context("Notification registry revision exhausted")?;
            match self.save_cas_unlocked(expected, &file) {
                Ok(()) => return Ok(()),
                Err(error) if error.to_string().contains("revision changed") => continue,
                Err(error) => return Err(error),
            }
        }
        anyhow::bail!("Notification registry CAS retry bound exhausted")
    }

    fn load_unlocked(&self) -> Result<RegistryFile> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(RegistryFile::default());
        }
        let metadata =
            fs::symlink_metadata(&path).context("Cannot inspect notification registry")?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "Notification registry must not be a link"
        );
        ensure!(
            metadata.len() as usize <= MAX_REGISTRY_BYTES,
            "Notification registry exceeds byte bound"
        );
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(O_CLOEXEC | O_NOFOLLOW)
            .open(&path)
            .context("Cannot open notification registry without following links")?;
        validate_private_file(&file, &path, "notification registry")?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((MAX_REGISTRY_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_REGISTRY_BYTES,
            "Notification registry exceeds byte bound"
        );
        let decoded: RegistryFile =
            serde_json::from_slice(&bytes).context("Cannot decode notification registry")?;
        validate_registry_file(&decoded)?;
        Ok(decoded)
    }

    fn save_cas_unlocked(&self, expected: u64, file: &RegistryFile) -> Result<()> {
        ensure!(
            self.load_unlocked()?.revision == expected,
            "Notification registry revision changed"
        );
        validate_registry_file(file)?;
        let bytes = serde_json::to_vec(file).context("Cannot encode notification registry")?;
        ensure!(
            bytes.len() <= MAX_REGISTRY_BYTES,
            "Notification registry exceeds byte bound"
        );
        let temp = self.root.join(format!(
            ".registry-{}-{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_CLOEXEC | O_NOFOLLOW)
            .open(&temp)
            .context("Cannot create private notification registry replacement")?;
        let write_result = (|| {
            output.write_all(&bytes)?;
            output.sync_all()?;
            fs::rename(&temp, self.state_path())?;
            File::open(&self.root)?.sync_all()?;
            Ok::<_, std::io::Error>(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        write_result.context("Cannot atomically save notification registry")
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("registry.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join("registry.lock")
    }
}

fn validate_registry_file(file: &RegistryFile) -> Result<()> {
    ensure!(
        file.schema_version == REGISTRY_SCHEMA_VERSION,
        "Unsupported notification registry schema version {}",
        file.schema_version
    );
    ensure!(
        file.enabled.len() <= MAX_ACCOUNTS,
        "Notification preference account bound exceeded"
    );
    for key in file.enabled.keys() {
        ensure!(
            key.len() <= MAX_IDENTITY_BYTES * 2,
            "Notification preference identity exceeds byte bound"
        );
        let (host, login): (String, String) =
            serde_json::from_str(key).context("Notification preference identity is malformed")?;
        validate_identity_part(&host, "account host")?;
        validate_identity_part(&login, "account login")?;
    }
    ensure!(
        file.delivered_tags.len() <= MAX_RETAINED_TAGS,
        "Notification tag bound exceeded"
    );
    let tags: HashSet<&str> = file.delivered_tags.iter().map(String::as_str).collect();
    ensure!(
        tags.len() == file.delivered_tags.len(),
        "Notification tags must be unique"
    );
    for tag in &file.delivered_tags {
        validate_tag(tag)?;
    }
    ensure!(
        file.routes.len() <= MAX_RETAINED_ROUTES,
        "Notification route bound exceeded"
    );
    let route_tags: HashSet<&str> = file.routes.iter().map(|route| route.tag.as_str()).collect();
    ensure!(
        route_tags.len() == file.routes.len(),
        "Notification routes must have unique tags"
    );
    for route in &file.routes {
        validate_tag(&route.tag)?;
        ensure!(
            tags.contains(route.tag.as_str()),
            "Notification route lacks retained dedupe tag"
        );
        validate_identity_part(&route.account.host, "route account host")?;
        validate_identity_part(&route.account.login, "route account login")?;
        ensure!(
            route.target.provider == "github",
            "Notification route provider is unsupported"
        );
        validate_identity_part(&route.target.host, "route host")?;
        validate_identity_part(&route.target.account, "route account")?;
        validate_identity_part(&route.target.owner, "route owner")?;
        validate_identity_part(&route.target.repository, "route repository")?;
        ensure!(
            route.target.pull_request > 0,
            "Notification route pull request must be positive"
        );
        ensure!(
            route.account.host.eq_ignore_ascii_case(&route.target.host)
                && route
                    .account
                    .login
                    .eq_ignore_ascii_case(&route.target.account),
            "Notification route account binding is inconsistent"
        );
    }
    Ok(())
}

fn validate_identity_part(value: &str, name: &str) -> Result<()> {
    ensure!(!value.is_empty(), "Notification {name} must not be empty");
    ensure!(
        value.len() <= MAX_IDENTITY_BYTES,
        "Notification {name} exceeds byte bound"
    );
    ensure!(
        !value.chars().any(char::is_control),
        "Notification {name} contains control characters"
    );
    Ok(())
}

fn validate_tag(tag: &str) -> Result<()> {
    ensure!(
        tag.starts_with("cibergit-notification-"),
        "Notification tag has an unknown namespace"
    );
    ensure!(
        tag.len() == "cibergit-notification-".len() + 64,
        "Notification tag has an invalid length"
    );
    ensure!(
        tag["cibergit-notification-".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "Notification tag digest is malformed"
    );
    Ok(())
}

fn validate_private_file(file: &File, path: &Path, name: &str) -> Result<()> {
    let descriptor = validate_file_identity(file, path, name)?;
    ensure!(
        descriptor.permissions().mode() & 0o077 == 0,
        "{name} must be private"
    );
    Ok(())
}

fn validate_file_identity(file: &File, path: &Path, name: &str) -> Result<fs::Metadata> {
    let descriptor = file
        .metadata()
        .with_context(|| format!("Cannot inspect {name} descriptor"))?;
    ensure!(descriptor.is_file(), "{name} must be a regular file");
    ensure!(descriptor.nlink() == 1, "{name} must have exactly one link");
    let expected_uid = unsafe { geteuid() };
    ensure!(
        descriptor.uid() == expected_uid,
        "{name} must be owned by the current user"
    );
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Cannot recheck {name} path identity"))?;
    ensure!(
        !path_metadata.file_type().is_symlink()
            && path_metadata.dev() == descriptor.dev()
            && path_metadata.ino() == descriptor.ino(),
        "{name} path changed while it was open"
    );
    Ok(descriptor)
}

#[derive(Debug)]
struct RegistryLock(File);

impl RegistryLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(O_CLOEXEC | O_NOFOLLOW)
            .open(path)
            .context("Cannot open stable notification registry lock")?;
        validate_file_identity(&file, path, "notification registry lock")?;
        let chmod_result = unsafe { fchmod(file.as_raw_fd(), 0o600) };
        ensure!(
            chmod_result == 0,
            "Cannot make notification registry lock private"
        );
        validate_private_file(&file, path, "notification registry lock")?;
        let mut locked = false;
        for attempt in 0..LOCK_ATTEMPTS {
            if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
                locked = true;
                break;
            }
            let error = std::io::Error::last_os_error();
            ensure!(
                matches!(error.raw_os_error(), Some(11 | 35)),
                "Cannot lock notification registry: {error}"
            );
            if attempt + 1 < LOCK_ATTEMPTS {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        ensure!(locked, "Notification registry lock wait bound exhausted");
        validate_private_file(&file, path, "notification registry lock")?;
        Ok(Self(file))
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = unsafe { flock(self.0.as_raw_fd(), LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::providers::notifications::{
        NotificationEventIdentity, NotificationEventSource, NotificationEvidence,
        ProviderNotificationObservation,
    };

    fn account(login: &str) -> Account {
        Account {
            host: "github.com".into(),
            login: login.into(),
        }
    }

    fn repo(login: &str, index: usize) -> Repository {
        Repository {
            host: "github.com".into(),
            owner: "octo".into(),
            name: format!("repo-{index}"),
            account: account(login),
            local_path: None,
        }
    }

    fn event(login: &str, id: &str, pr: u64) -> ProviderNotificationEvent {
        let target = NotificationPullRequest {
            provider: "github".into(),
            host: "github.com".into(),
            account: login.into(),
            owner: "octo".into(),
            repository: "repo-0".into(),
            pull_request: pr,
        };
        ProviderNotificationEvent {
            identity: NotificationEventIdentity {
                target,
                source: NotificationEventSource::Timeline,
                remote_event_id: id.into(),
            },
            occurred_at: "2026-09-13T10:00:00Z".into(),
            actor: Some("reviewer".into()),
            alert_kind: NotificationAlertKind::ReviewRequest,
            summary: "Review requested".into(),
            url: format!("https://github.com/octo/repo-0/pull/{pr}"),
            evidence: NotificationEvidence::ReviewRequested {
                timeline_event_id: id.into(),
                requested_reviewer: login.into(),
            },
        }
    }

    fn batch(
        login: &str,
        events: Vec<ProviderNotificationEvent>,
        full: bool,
    ) -> ProviderNotificationBatch {
        let repository = repo(login, 0);
        ProviderNotificationBatch {
            account: account(login),
            observed_at_unix_ms: 1,
            observations: vec![ProviderNotificationObservation {
                provider_notification_id: "thread".into(),
                provider_reason: "review_requested".into(),
                notification_updated_at: "2026-09-13T10:00:00Z".into(),
                target: events
                    .first()
                    .map(|event| event.identity.target.clone())
                    .unwrap_or(NotificationPullRequest {
                        provider: "github".into(),
                        host: "github.com".into(),
                        account: login.into(),
                        owner: "octo".into(),
                        repository: "repo-0".into(),
                        pull_request: 7,
                    }),
                events,
            }],
            incomplete_candidates: Vec::new(),
            full_snapshot: full,
            repositories: vec![RepositoryNotificationCompleteness {
                target: repository_scope(&repository),
                complete: true,
                reasons: Vec::new(),
            }],
            complete: true,
            notices: Vec::new(),
        }
    }

    #[test]
    fn five_plus_repositories_are_chunked_without_omission_and_one_lane_per_account() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let repositories = (0..12)
            .map(|index| repo("alice", index))
            .collect::<Vec<_>>();
        let work = controller.begin_polls(&repositories);
        assert_eq!(work.len(), 1);
        assert_eq!(
            work[0].chunks.iter().map(Vec::len).collect::<Vec<_>>(),
            [5, 5, 2]
        );
        assert!(controller.begin_polls(&repositories).is_empty());
    }

    #[test]
    fn rate_limit_stops_later_chunks_and_reaches_poll_completion() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let repositories = (0..6).map(|index| repo("alice", index)).collect::<Vec<_>>();
        let work = controller.begin_polls(&repositories).pop().unwrap();
        assert_eq!(work.chunks.len(), 2);
        let mut calls = 0;
        let completion = work.run_with(|_, chunk, cache| {
            calls += 1;
            assert_eq!(chunk.len(), 5);
            let mut response = batch("alice", Vec::new(), true);
            response.repositories = chunk
                .iter()
                .map(|repository| RepositoryNotificationCompleteness {
                    target: repository_scope(repository),
                    complete: false,
                    reasons: vec!["server rate limit".into()],
                })
                .collect();
            response.complete = false;
            Ok(
                cibergit::providers::notifications::NotificationObservationRead {
                    batch: response,
                    cache,
                    poll: NotificationPollDirective {
                        x_poll_interval: None,
                        rate_limit: Some(NotificationDelay::Seconds(90)),
                    },
                },
            )
        });
        assert_eq!(
            calls, 1,
            "later chunks must not dispatch after rate limiting"
        );
        assert_eq!(
            completion.poll.rate_limit,
            Some(NotificationDelay::Seconds(90))
        );
        assert!(completion.failed());
        assert_eq!(completion.snapshot.repository_completeness.len(), 6);
        assert!(
            completion
                .snapshot
                .repository_completeness
                .iter()
                .all(|repository| !repository.complete)
        );
    }

    #[test]
    fn polling_respects_each_accounts_due_time_and_invalidates_removed_selection() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let repositories = [repo("alice", 0), repo("bob", 0)];
        let mut schedule = cibergit::workspace::PollSchedule::default();
        schedule.failed(&schedule_key(&account("bob")));
        let work = controller.begin_polls_when(&repositories, |account| {
            super::super::poll_due(4, schedule.delay(&schedule_key(account), false, true))
        });
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].account.login, "alice");
        assert!(
            !controller.accounts[&account_key(&account("bob"))]
                .selection
                .is_empty()
        );
        let completion = PollCompletion {
            token: work[0].token.clone(),
            account: account("alice"),
            snapshot: empty_snapshot("old selection".into()),
            newly_admitted: vec![],
            error: None,
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective::default(),
        };
        assert!(controller.accepts_poll(&completion));
        assert!(
            controller
                .begin_polls_when(&[repo("bob", 0)], |_| false)
                .is_empty()
        );
        assert!(!controller.accepts_poll(&completion));
        assert!(
            controller
                .begin_polls_when(&repositories, |_| false)
                .is_empty()
        );
        assert!(
            !controller.accepts_poll(&completion),
            "returning to the same selection cannot revive an old completion"
        );
    }

    #[test]
    fn server_poll_interval_blocks_timer_focus_and_manual_until_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let now = Instant::now();
        let work = controller
            .begin_polls_when_at(&[repo("alice", 0)], now, |_| true)
            .pop()
            .unwrap();
        let completion = PollCompletion {
            token: work.token,
            account: account("alice"),
            snapshot: empty_snapshot(String::new()),
            newly_admitted: vec![],
            error: None,
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective {
                x_poll_interval: Some(NotificationDelay::Seconds(120)),
                rate_limit: None,
            },
        };
        assert!(controller.release_poll_at(&completion, now));
        assert_eq!(
            controller.accounts[&account_key(&account("alice"))]
                .server_gate
                .notice(now),
            Some("Notification refresh is deferred by GitHub's polling interval.")
        );
        for label in ["timer", "focus", "manual"] {
            assert!(
                controller
                    .begin_polls_when_at(
                        &[repo("alice", 0)],
                        now + Duration::from_secs(119),
                        |_| true,
                    )
                    .is_empty(),
                "{label} must not bypass the server gate"
            );
        }
        let next = controller
            .begin_polls_when_at(&[repo("alice", 0)], now + Duration::from_secs(120), |_| {
                true
            })
            .pop()
            .unwrap();
        let overflow = PollCompletion {
            token: next.token,
            account: account("alice"),
            snapshot: empty_snapshot(String::new()),
            newly_admitted: vec![],
            error: None,
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective {
                x_poll_interval: Some(NotificationDelay::Seconds(u64::MAX)),
                rate_limit: None,
            },
        };
        assert!(controller.release_poll_at(&overflow, now + Duration::from_secs(120)));
        assert!(
            controller.accounts[&account_key(&account("alice"))]
                .server_gate
                .notice(now + Duration::from_secs(120))
                .is_some_and(|notice| notice.contains("paused"))
        );
        assert!(
            controller
                .begin_polls_when_at(
                    &[repo("alice", 0)],
                    now + Duration::from_secs(3_600),
                    |_| true
                )
                .is_empty(),
            "an unrepresentable server delay suspends instead of shortening"
        );
    }

    #[test]
    fn stale_selection_applies_only_matching_account_rate_gate() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let now = Instant::now();
        let first = controller
            .begin_polls_when_at(&[repo("alice", 0), repo("bob", 0)], now, |_| true)
            .into_iter()
            .find(|work| work.account.login == "alice")
            .unwrap();
        assert!(
            controller
                .begin_polls_when_at(&[repo("alice", 1), repo("bob", 0)], now, |_| false)
                .is_empty()
        );
        let stale = PollCompletion {
            token: first.token,
            account: account("alice"),
            snapshot: empty_snapshot(String::new()),
            newly_admitted: vec![],
            error: Some("rate limited".into()),
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective {
                x_poll_interval: Some(NotificationDelay::Seconds(600)),
                rate_limit: Some(NotificationDelay::Seconds(90)),
            },
        };
        assert!(controller.release_poll_at(&stale, now));
        assert!(!controller.accepts_poll(&stale));
        assert_eq!(
            controller.accounts[&account_key(&account("alice"))]
                .server_gate
                .notice(now),
            Some("Notification refresh is deferred by GitHub rate limiting.")
        );
        assert!(
            controller
                .begin_polls_when_at(&[repo("alice", 1)], now + Duration::from_secs(89), |_| true)
                .is_empty()
        );
        assert_eq!(
            controller
                .begin_polls_when_at(&[repo("alice", 1)], now + Duration::from_secs(90), |_| true)
                .len(),
            1,
            "stale X-Poll-Interval must not install, but matching rate delay must"
        );
    }

    #[test]
    fn successful_completion_does_not_reset_server_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let now = Instant::now();
        let work = controller
            .begin_polls_when_at(&[repo("alice", 0)], now, |_| true)
            .pop()
            .unwrap();
        let completion = PollCompletion {
            token: work.token,
            account: account("alice"),
            snapshot: empty_snapshot(String::new()),
            newly_admitted: vec![],
            error: None,
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective {
                x_poll_interval: Some(NotificationDelay::Seconds(120)),
                rate_limit: None,
            },
        };
        assert!(controller.complete_poll(completion).is_none());
        assert!(
            controller
                .begin_polls_when_at(&[repo("alice", 0)], now + Duration::from_secs(119), |_| {
                    true
                })
                .is_empty(),
            "a successful completion may clear local backoff but not the server deadline"
        );
    }

    #[test]
    fn expired_absolute_reset_is_zero_and_never_shortens_a_later_floor() {
        let wall_epoch = UNIX_EPOCH + Duration::from_secs(10_000);
        let now = Instant::now();

        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let work = controller.begin_polls(&[repo("alice", 0)]).pop().unwrap();
        let expired = PollCompletion {
            token: work.token,
            account: account("alice"),
            snapshot: empty_snapshot(String::new()),
            newly_admitted: vec![],
            error: Some("rate limited".into()),
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective {
                x_poll_interval: None,
                rate_limit: Some(NotificationDelay::UntilUnixSeconds(10_001)),
            },
        };
        assert!(controller.release_poll_at_with_wall(
            &expired,
            now,
            wall_epoch + Duration::from_secs(2)
        ));
        assert!(
            controller.accounts[&account_key(&account("alice"))]
                .server_gate
                .notice(now)
                .is_none()
        );
        assert_eq!(
            controller
                .begin_polls_when_at(&[repo("alice", 0)], now, |_| true)
                .len(),
            1,
            "a reset that expired during response processing must not suspend polling"
        );

        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let work = controller.begin_polls(&[repo("alice", 0)]).pop().unwrap();
        let mixed = PollCompletion {
            token: work.token,
            account: account("alice"),
            snapshot: empty_snapshot(String::new()),
            newly_admitted: vec![],
            error: Some("rate limited".into()),
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective {
                x_poll_interval: Some(NotificationDelay::Seconds(120)),
                rate_limit: Some(NotificationDelay::UntilUnixSeconds(10_001)),
            },
        };
        assert!(controller.release_poll_at_with_wall(
            &mixed,
            now,
            wall_epoch + Duration::from_secs(2)
        ));
        assert!(
            controller
                .begin_polls_when_at(&[repo("alice", 0)], now + Duration::from_secs(119), |_| {
                    true
                })
                .is_empty()
        );
        assert_eq!(
            controller
                .begin_polls_when_at(&[repo("alice", 0)], now + Duration::from_secs(120), |_| {
                    true
                })
                .len(),
            1,
            "an expired reset must not shorten another server floor"
        );
    }

    #[test]
    fn partial_success_retains_failure_backoff() {
        let mut completion = PollCompletion {
            token: ControllerToken {
                consent_generation: 0,
                lifetime: 1,
                account_key: account_key(&account("alice")),
                selection: String::new(),
                generation: 1,
            },
            account: account("alice"),
            snapshot: empty_snapshot(String::new()),
            newly_admitted: vec![],
            error: None,
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective::default(),
        };
        completion.snapshot.repository_completeness = vec![RepositoryNotificationCompleteness {
            target: repository_scope(&repo("alice", 0)),
            complete: false,
            reasons: vec!["hydration bound reached".into()],
        }];
        assert!(
            completion.failed(),
            "an HTTP success with incomplete evidence must back off"
        );
        completion.snapshot.repository_completeness[0].complete = true;
        assert!(!completion.failed());
    }

    #[test]
    fn removed_repository_is_hidden_from_fresh_poll_and_mark_without_erasing_unread() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let store = controller.runtime.as_ref().unwrap().store.clone();
        let alice = account("alice");
        store
            .reconcile(&alice, &batch("alice", vec![], true))
            .unwrap();
        store
            .reconcile(
                &alice,
                &batch("alice", vec![event("alice", "retained", 7)], true),
            )
            .unwrap();
        let mut selected_batch = batch("alice", vec![], true);
        selected_batch.repositories[0].target = repository_scope(&repo("alice", 1));
        selected_batch.observations.clear();
        store.reconcile(&alice, &selected_batch).unwrap();
        let work = controller.begin_polls(&[repo("alice", 1)]).pop().unwrap();
        let completion = PollCompletion {
            token: work.token,
            account: alice.clone(),
            snapshot: store.list_unread(&alice).unwrap(),
            newly_admitted: vec![],
            error: None,
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective::default(),
        };
        controller.complete_poll(completion);
        assert_eq!(controller.unread_count(), 0);
        controller.toggle_open();
        assert!(controller.begin_mark_displayed(&alice).is_none());
        let state = controller.accounts.get(&account_key(&alice)).unwrap();
        let mark = MarkReadCompletion {
            token: MarkToken {
                lifetime: controller.lifetime,
                account_key: account_key(&alice),
                selection: state.selection.clone(),
                selection_generation: state.selection_generation,
                generation: state.mark_generation,
            },
            account: alice.clone(),
            result: Ok(store.list_unread(&alice).unwrap()),
        };
        assert!(controller.complete_mark_displayed(mark));
        assert_eq!(controller.unread_count(), 0);
        assert_eq!(
            unread_count(&store.list_unread(&alice).unwrap()),
            1,
            "projection must preserve durable unread for re-selection"
        );
    }

    #[test]
    fn mark_completion_cannot_reappear_after_selection_away_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let alice = account("alice");
        controller.begin_polls(&[repo("alice", 0)]);
        let state = controller.accounts.get(&account_key(&alice)).unwrap();
        let completion = MarkReadCompletion {
            token: MarkToken {
                lifetime: controller.lifetime,
                account_key: account_key(&alice),
                selection: state.selection.clone(),
                selection_generation: state.selection_generation,
                generation: state.mark_generation,
            },
            account: alice.clone(),
            result: Ok(empty_snapshot("stale mark".into())),
        };
        controller.begin_polls(&[repo("alice", 1)]);
        controller.begin_polls(&[repo("alice", 0)]);
        assert!(!controller.complete_mark_displayed(completion));
        assert!(controller.accounts[&account_key(&alice)].snapshot.is_none());
    }

    #[test]
    fn selection_change_keeps_the_existing_poll_lane_until_its_completion() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let first = controller.begin_polls(&[repo("alice", 0)]).pop().unwrap();
        assert!(
            controller.begin_polls(&[repo("alice", 1)]).is_empty(),
            "old read still owns the account lane"
        );
        let completion = PollCompletion {
            token: first.token,
            account: account("alice"),
            snapshot: empty_snapshot(String::new()),
            newly_admitted: vec![],
            error: None,
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective::default(),
        };
        assert!(!controller.accepts_poll(&completion));
        assert!(controller.release_poll(&completion));
        let second = controller.begin_polls(&[repo("alice", 1)]).pop().unwrap();
        assert_eq!(second.chunks[0][0].name, "repo-1");
        assert!(
            !controller.release_poll(&completion),
            "duplicate old reply cannot release the replacement lane"
        );
        assert!(controller.begin_polls(&[repo("alice", 1)]).is_empty());
    }

    #[test]
    fn enabling_alerts_during_a_poll_does_not_catch_up_that_poll() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let alice = account("alice");
        let first = controller.begin_polls(&[repo("alice", 0)]).pop().unwrap();
        let enable = controller.begin_consent(&alice, true).unwrap();
        assert!(controller.complete_consent(enable.run()));
        let exact = event("alice", "before-enable", 7);
        let snapshot = NotificationSnapshot {
            state_version: 1,
            unread_by_pull_request: vec![PullRequestUnreadSummary {
                target: exact.identity.target.clone(),
                unread_events: vec![exact.clone()],
                unknown_candidates: vec![],
            }],
            repository_completeness: vec![],
            incomplete_candidates: vec![],
            notices: vec![],
        };
        assert!(
            controller
                .complete_poll(PollCompletion {
                    token: first.token,
                    account: alice.clone(),
                    snapshot: snapshot.clone(),
                    newly_admitted: vec![exact.clone()],
                    error: None,
                    cache: NotificationConditionalCache::default(),
                    poll: NotificationPollDirective::default(),
                })
                .is_none()
        );
        assert_eq!(
            controller.unread_count(),
            1,
            "in-app unread remains independent of OS consent"
        );
        let next = controller.begin_polls(&[repo("alice", 0)]).pop().unwrap();
        assert!(
            controller
                .complete_poll(PollCompletion {
                    token: next.token,
                    account: alice,
                    snapshot,
                    newly_admitted: vec![exact],
                    error: None,
                    cache: NotificationConditionalCache::default(),
                    poll: NotificationPollDirective::default(),
                })
                .is_some()
        );
    }

    #[test]
    fn live_response_routes_expire_at_the_same_bound_as_persisted_routes() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let alice = account("alice");
        let enable = controller.begin_consent(&alice, true).unwrap();
        assert!(controller.complete_consent(enable.run()));
        controller.begin_polls(&[repo("alice", 0)]);
        let state = controller.accounts.get(&account_key(&alice)).unwrap();
        let token = AdmissionToken {
            lifetime: controller.lifetime,
            account_key: account_key(&alice),
            selection: state.selection.clone(),
            selection_generation: state.selection_generation,
            consent_generation: state.consent_generation,
        };
        let events = (0..=MAX_RETAINED_ROUTES)
            .map(|n| event("alice", &n.to_string(), 7))
            .collect::<Vec<_>>();
        let old_tag = stable_event_tag(&alice, &events[0]);
        let last_tag = stable_event_tag(&alice, events.last().unwrap());
        let completion = AdmissionWork {
            token,
            account: alice.clone(),
            events,
            registry: controller.runtime.as_ref().unwrap().registry.clone(),
        }
        .run();
        assert!(controller.complete_admission(completion));
        assert_eq!(controller.routes.len(), MAX_RETAINED_ROUTES);
        assert!(controller.route_for_tag(&old_tag).is_none());
        assert_eq!(controller.route_for_tag(&last_tag).unwrap().pull_request, 7);
        controller.dispatch_to(&mut |_| {});
        let mut restarted = NotificationController::new(dir.path().into());
        assert!(restarted.complete_bootstrap(restarted.begin_bootstrap().run()));
        assert!(restarted.route_for_tag(&old_tag).is_none());
        assert_eq!(
            restarted.route_for_tag(&last_tag),
            controller.route_for_tag(&last_tag)
        );
    }

    #[test]
    #[ignore = "subprocess probe for notification delivery authority"]
    fn notification_consent_writer_probe() {
        let root = std::env::var_os("CIBERGIT_TEST_NOTIFICATION_REGISTRY").expect("probe root");
        let registry = NotificationRegistry {
            root: PathBuf::from(root),
        };
        let result = registry.set_enabled(&account("alice"), false);
        if std::env::var_os("CIBERGIT_TEST_EXPECT_NOTIFICATION_BUSY").is_some() {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("lock wait bound exhausted"),
                "unexpected refusal: {error}"
            );
        } else {
            result.unwrap();
        }
    }

    #[test]
    fn durable_disable_in_another_process_serializes_with_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let alice = account("alice");
        let enable = controller.begin_consent(&alice, true).unwrap();
        assert!(controller.complete_consent(enable.run()));
        let poll = controller.begin_polls(&[repo("alice", 0)]).pop().unwrap();
        let exact = event("alice", "delivery", 7);
        let admission = controller
            .complete_poll(PollCompletion {
                token: poll.token,
                account: alice.clone(),
                snapshot: empty_snapshot(String::new()),
                newly_admitted: vec![exact.clone()],
                error: None,
                cache: NotificationConditionalCache::default(),
                poll: NotificationPollDirective::default(),
            })
            .unwrap();
        assert!(controller.complete_admission(admission.run()));
        let registry = controller.runtime.as_ref().unwrap().registry.clone();
        let probe = |expect_busy: bool| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "app::notifications_view::tests::notification_consent_writer_probe",
                    "--ignored",
                    "--nocapture",
                ])
                .env("CIBERGIT_TEST_NOTIFICATION_REGISTRY", &registry.root);
            if expect_busy {
                command.env("CIBERGIT_TEST_EXPECT_NOTIFICATION_BUSY", "1");
            } else {
                command.env_remove("CIBERGIT_TEST_EXPECT_NOTIFICATION_BUSY");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "probe failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        probe(true);
        let mut calls = 0;
        assert_eq!(
            controller.dispatch_to(&mut |_| {
                calls += 1;
                probe(true);
            }),
            1,
            "lease must span the sink request"
        );
        assert_eq!(calls, 1);
        probe(false);
        assert!(
            registry.reserve_for_delivery(&alice, &[exact]).is_err(),
            "successful external disable prevents a stale controller from reserving another request"
        );
    }

    #[test]
    fn registry_is_default_off_private_stable_and_deduplicates_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let registry = NotificationRegistry::open(dir.path().join("ui")).unwrap();
        assert!(
            !registry
                .preferences()
                .unwrap()
                .get(&account_key(&account("alice")))
                .copied()
                .unwrap_or(false)
        );
        registry.set_enabled(&account("alice"), true).unwrap();
        let first = registry
            .reserve_new_events(&account("alice"), &[event("alice", "1", 7)])
            .unwrap();
        assert_eq!(first.len(), 1);
        let reopened = NotificationRegistry::open(dir.path().join("ui")).unwrap();
        assert!(
            reopened
                .reserve_new_events(&account("alice"), &[event("alice", "1", 7)])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reopened
                .route_for_tag(&first[0].notification.tag)
                .unwrap()
                .unwrap()
                .pull_request,
            7
        );
        let mode = fs::metadata(dir.path().join("ui/registry.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn real_store_baseline_new_event_exact_display_mark_and_restart_are_lossless() {
        let dir = tempfile::tempdir().unwrap();
        let store = NotificationStore::open(
            dir.path().join("notifications"),
            NotificationStoreLimits::default(),
        )
        .unwrap();
        let alice = account("alice");
        assert!(
            store
                .reconcile(&alice, &batch("alice", vec![event("alice", "1", 7)], true))
                .unwrap()
                .newly_admitted_alerts
                .is_empty()
        );
        let later = store
            .reconcile(
                &alice,
                &batch(
                    "alice",
                    vec![event("alice", "1", 7), event("alice", "2", 7)],
                    true,
                ),
            )
            .unwrap();
        assert_eq!(later.newly_admitted_alerts.len(), 1);
        let displayed = DisplayedNotificationSet {
            state_version: later.state_version,
            events: vec![later.newly_admitted_alerts[0].identity.clone()],
        };
        let concurrent = store
            .reconcile(
                &alice,
                &batch(
                    "alice",
                    vec![
                        event("alice", "1", 7),
                        event("alice", "2", 7),
                        event("alice", "3", 7),
                    ],
                    true,
                ),
            )
            .unwrap();
        assert_eq!(concurrent.newly_admitted_alerts.len(), 1);
        let marked = store.mark_displayed_read(&alice, &displayed).unwrap();
        assert_eq!(unread_count(&marked), 1);
        let reopened = NotificationStore::open(
            dir.path().join("notifications"),
            NotificationStoreLimits::default(),
        )
        .unwrap();
        assert_eq!(unread_count(&reopened.list_unread(&alice).unwrap()), 1);
        assert!(
            reopened
                .reconcile(
                    &alice,
                    &batch(
                        "alice",
                        vec![
                            event("alice", "1", 7),
                            event("alice", "2", 7),
                            event("alice", "3", 7)
                        ],
                        true
                    )
                )
                .unwrap()
                .newly_admitted_alerts
                .is_empty()
        );
    }

    #[test]
    fn stale_tokens_consent_disable_and_unknown_tags_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let alice = account("alice");
        let enable = controller.begin_consent(&alice, true).unwrap();
        assert!(controller.complete_consent(enable.run()));
        let stale = AdmissionToken {
            lifetime: controller.lifetime,
            account_key: account_key(&alice),
            selection: "removed".into(),
            selection_generation: 0,
            consent_generation: 0,
        };
        assert!(!controller.complete_admission(AdmissionCompletion {
            token: stale,
            notifications: Vec::new(),
            result: Ok(())
        }));
        let disable = controller.begin_consent(&alice, false).unwrap();
        assert!(controller.take_system_notifications().is_empty());
        assert!(controller.complete_consent(disable.run()));
        assert!(controller.route_for_tag("unknown").is_none());
    }

    #[test]
    fn account_and_selection_identity_are_part_of_callback_authority() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let first = controller.begin_polls(&[repo("alice", 0)]).pop().unwrap();
        let old_token = first.token.clone();
        controller
            .accounts
            .get_mut(&account_key(&account("alice")))
            .unwrap()
            .selection = selection_key(&[repo("alice", 1)]);
        let completion = PollCompletion {
            token: old_token,
            account: account("alice"),
            snapshot: NotificationSnapshot {
                state_version: 2,
                unread_by_pull_request: vec![PullRequestUnreadSummary {
                    target: event("alice", "1", 7).identity.target,
                    unread_events: vec![event("alice", "1", 7)],
                    unknown_candidates: Vec::new(),
                }],
                repository_completeness: Vec::new(),
                incomplete_candidates: Vec::new(),
                notices: vec!["stale".into()],
            },
            newly_admitted: vec![event("alice", "1", 7)],
            error: None,
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective::default(),
        };
        assert!(controller.complete_poll(completion).is_none());
        assert_eq!(controller.unread_count(), 0);
    }

    #[test]
    fn consent_is_captured_and_current_before_test_sink_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let alice = account("alice");

        let enable = controller.begin_consent(&alice, true).unwrap();
        assert!(controller.complete_consent(enable.run()));
        let configured_selection = selection_key(&[repo("alice", 0)]);
        let state = controller.accounts.get_mut(&account_key(&alice)).unwrap();
        state.selection = configured_selection.clone();
        state.selection_generation = 1;
        let consent_generation = state.consent_generation;
        let late_work = AdmissionWork {
            token: AdmissionToken {
                lifetime: controller.lifetime,
                account_key: account_key(&alice),
                selection: configured_selection.clone(),
                selection_generation: 1,
                consent_generation,
            },
            account: alice.clone(),
            events: vec![event("alice", "late", 7)],
            registry: controller.runtime.as_ref().unwrap().registry.clone(),
        };
        let late_completion = late_work.run();
        let disable = controller.begin_consent(&alice, false).unwrap();
        assert!(!controller.complete_admission(late_completion));
        assert!(controller.complete_consent(disable.run()));
        let mut calls = Vec::new();
        assert_eq!(
            controller.dispatch_to(&mut |notification| calls.push(notification)),
            0
        );
        assert!(calls.is_empty());

        let reenable = controller.begin_consent(&alice, true).unwrap();
        assert!(controller.complete_consent(reenable.run()));
        let state = controller.accounts.get(&account_key(&alice)).unwrap();
        let exact_event = event("alice", "fresh", 9);
        let expected_tag = stable_event_tag(&alice, &exact_event);
        let completion = AdmissionWork {
            token: AdmissionToken {
                lifetime: controller.lifetime,
                account_key: account_key(&alice),
                selection: configured_selection,
                selection_generation: state.selection_generation,
                consent_generation: state.consent_generation,
            },
            account: alice.clone(),
            events: vec![exact_event],
            registry: controller.runtime.as_ref().unwrap().registry.clone(),
        }
        .run();
        assert!(controller.complete_admission(completion));
        assert_eq!(
            controller.dispatch_to(&mut |notification| calls.push(notification)),
            1
        );
        assert_eq!(calls[0].tag.as_ref(), expected_tag);
        assert_eq!(
            controller
                .route_for_tag(&expected_tag)
                .unwrap()
                .pull_request,
            9
        );
    }

    #[test]
    fn partial_chunk_failure_is_disclosed_without_full_baseline_claim() {
        let alice = account("alice");
        let chunks = vec![vec![repo("alice", 0)], vec![repo("alice", 1)]];
        let combined = combine_batches(
            &alice,
            &chunks,
            vec![(0, batch("alice", vec![event("alice", "1", 7)], true))],
            &[(1, "offline".into())],
        )
        .unwrap();
        assert!(!combined.full_snapshot);
        assert!(!combined.complete);
        assert_eq!(combined.repositories.len(), 2);
        assert!(combined.repositories.iter().any(|repository| {
            repository.target.repository == "repo-1"
                && !repository.complete
                && repository
                    .reasons
                    .iter()
                    .any(|reason| reason.contains("offline"))
        }));
    }

    #[test]
    fn older_mark_completion_cannot_replace_a_newer_displayed_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let alice = account("alice");
        let repository = repo("alice", 0);
        let selection = selection_key(std::slice::from_ref(&repository));
        let key = account_key(&alice);
        controller.accounts.insert(
            key.clone(),
            AccountViewState {
                account: Some(alice.clone()),
                selection: selection.clone(),
                mark_generation: 4,
                snapshot: Some(NotificationSnapshot {
                    state_version: 9,
                    unread_by_pull_request: vec![PullRequestUnreadSummary {
                        target: event("alice", "new", 7).identity.target,
                        unread_events: vec![event("alice", "new", 7)],
                        unknown_candidates: Vec::new(),
                    }],
                    repository_completeness: Vec::new(),
                    incomplete_candidates: Vec::new(),
                    notices: Vec::new(),
                }),
                ..Default::default()
            },
        );
        let completion = MarkReadCompletion {
            token: MarkToken {
                lifetime: controller.lifetime,
                account_key: key,
                selection,
                selection_generation: 0,
                generation: 4,
            },
            account: alice,
            result: Ok(NotificationSnapshot {
                state_version: 8,
                unread_by_pull_request: Vec::new(),
                repository_completeness: Vec::new(),
                incomplete_candidates: Vec::new(),
                notices: Vec::new(),
            }),
        };
        assert!(controller.complete_mark_displayed(completion));
        assert_eq!(controller.unread_count(), 1);
        assert_eq!(
            controller
                .accounts
                .values()
                .next()
                .unwrap()
                .snapshot
                .as_ref()
                .unwrap()
                .state_version,
            9
        );
    }

    #[test]
    fn repository_removal_while_admission_is_in_flight_suppresses_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let alice = account("alice");
        let enable = controller.begin_consent(&alice, true).unwrap();
        assert!(controller.complete_consent(enable.run()));
        let selection = selection_key(&[repo("alice", 0)]);
        let state = controller.accounts.get_mut(&account_key(&alice)).unwrap();
        state.selection = selection.clone();
        state.selection_generation = 7;
        let work = AdmissionWork {
            token: AdmissionToken {
                lifetime: controller.lifetime,
                account_key: account_key(&alice),
                selection,
                selection_generation: 7,
                consent_generation: state.consent_generation,
            },
            account: alice,
            events: vec![event("alice", "removed", 7)],
            registry: controller.runtime.as_ref().unwrap().registry.clone(),
        };
        let completion = work.run();
        assert!(controller.begin_polls(&[]).is_empty());
        assert!(!controller.complete_admission(completion));
        let mut calls = Vec::new();
        assert_eq!(
            controller.dispatch_to(&mut |notification| calls.push(notification)),
            0
        );
        assert!(calls.is_empty());
    }

    #[test]
    fn delayed_poll_completion_cannot_restore_snapshot_older_than_mark_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut controller = NotificationController::new(dir.path().into());
        assert!(controller.complete_bootstrap(controller.begin_bootstrap().run()));
        let alice = account("alice");
        let selection = selection_key(&[repo("alice", 0)]);
        let key = account_key(&alice);
        controller.accounts.insert(
            key.clone(),
            AccountViewState {
                account: Some(alice.clone()),
                selection: selection.clone(),
                generation: 3,
                in_flight: true,
                snapshot: Some(NotificationSnapshot {
                    state_version: 12,
                    unread_by_pull_request: Vec::new(),
                    repository_completeness: Vec::new(),
                    incomplete_candidates: Vec::new(),
                    notices: vec!["mark-read completed".into()],
                }),
                ..Default::default()
            },
        );
        let completion = PollCompletion {
            token: ControllerToken {
                consent_generation: 0,
                lifetime: controller.lifetime,
                account_key: key,
                selection,
                generation: 3,
            },
            account: alice,
            snapshot: NotificationSnapshot {
                state_version: 11,
                unread_by_pull_request: vec![PullRequestUnreadSummary {
                    target: event("alice", "old", 7).identity.target,
                    unread_events: vec![event("alice", "old", 7)],
                    unknown_candidates: Vec::new(),
                }],
                repository_completeness: Vec::new(),
                incomplete_candidates: Vec::new(),
                notices: Vec::new(),
            },
            newly_admitted: vec![event("alice", "old", 7)],
            error: None,
            cache: NotificationConditionalCache::default(),
            poll: NotificationPollDirective::default(),
        };
        assert!(controller.complete_poll(completion).is_none());
        let state = controller.accounts.values().next().unwrap();
        assert_eq!(state.snapshot.as_ref().unwrap().state_version, 12);
        assert_eq!(controller.unread_count(), 0);
        assert!(!state.in_flight);
    }

    #[test]
    fn corrupt_and_future_registry_records_are_preserved_and_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ui");
        let registry = NotificationRegistry::open(root.clone()).unwrap();
        registry.set_enabled(&account("alice"), true).unwrap();
        let path = root.join("registry.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["schema_version"] = serde_json::json!(REGISTRY_SCHEMA_VERSION + 1);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let future = fs::read(&path).unwrap();
        assert!(registry.set_enabled(&account("alice"), false).is_err());
        assert_eq!(fs::read(&path).unwrap(), future);

        fs::write(&path, b"{not-json").unwrap();
        let corrupt = fs::read(&path).unwrap();
        assert!(registry.set_enabled(&account("alice"), false).is_err());
        assert_eq!(fs::read(&path).unwrap(), corrupt);
    }

    #[test]
    fn registry_lock_is_private_nofollow_single_link_and_bounded() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ui");
        fs::create_dir(&root).unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, root.join("registry.lock")).unwrap();
        assert!(NotificationRegistry::open(root.clone()).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        fs::remove_file(root.join("registry.lock")).unwrap();

        let registry = NotificationRegistry::open(root.clone()).unwrap();
        let first = RegistryLock::acquire(&registry.lock_path()).unwrap();
        let started = std::time::Instant::now();
        let error = RegistryLock::acquire(&registry.lock_path()).err().unwrap();
        assert!(error.to_string().contains("wait bound exhausted"));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        drop(first);
        let second = RegistryLock::acquire(&registry.lock_path()).unwrap();
        drop(second);
        let metadata = fs::metadata(registry.lock_path()).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        fs::hard_link(registry.lock_path(), root.join("second-link")).unwrap();
        assert!(RegistryLock::acquire(&registry.lock_path()).is_err());
    }
}
