//! Private, account-partitioned local unread state.
//!
//! The store never changes GitHub notification state and has no network write
//! surface. Initial baseline authority is tracked per repository, so adding a
//! repository later cannot turn its history into a toast storm.

use crate::domain::Account;
use crate::providers::notifications::{
    IncompleteNotificationCandidate, NotificationEventIdentity, NotificationPullRequest,
    NotificationRepositoryScope, ProviderNotificationBatch, ProviderNotificationEvent,
    RepositoryNotificationCompleteness,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        raw::c_int,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const FORMAT_VERSION: u32 = 1;
const O_CLOEXEC: c_int = 0x0100_0000;
const O_NOFOLLOW: c_int = 0x0000_0100;
const LOCK_EX: c_int = 2;
const LOCK_NB: c_int = 4;
const LOCK_UN: c_int = 8;

unsafe extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationStoreLimits {
    pub max_state_bytes: usize,
    pub max_events: usize,
    pub max_repositories: usize,
    pub max_text_bytes: usize,
}

impl Default for NotificationStoreLimits {
    fn default() -> Self {
        Self {
            max_state_bytes: 4 * 1024 * 1024,
            max_events: 8_192,
            max_repositories: 128,
            max_text_bytes: 1_024,
        }
    }
}

impl NotificationStoreLimits {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.max_state_bytes >= 4_096 && self.max_state_bytes <= 64 * 1024 * 1024,
            "Invalid notification state byte bound"
        );
        ensure!(
            self.max_events > 0 && self.max_events <= 100_000,
            "Invalid notification event bound"
        );
        ensure!(
            self.max_repositories > 0 && self.max_repositories <= 1_024,
            "Invalid notification repository bound"
        );
        ensure!(
            self.max_text_bytes >= 64 && self.max_text_bytes <= 64 * 1024,
            "Invalid notification text bound"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayedNotificationSet {
    pub state_version: u64,
    pub events: Vec<NotificationEventIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestUnreadSummary {
    pub target: NotificationPullRequest,
    pub unread_events: Vec<ProviderNotificationEvent>,
    pub unknown_candidates: Vec<IncompleteNotificationCandidate>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationSnapshot {
    pub state_version: u64,
    pub unread_by_pull_request: Vec<PullRequestUnreadSummary>,
    pub repository_completeness: Vec<RepositoryNotificationCompleteness>,
    pub incomplete_candidates: Vec<IncompleteNotificationCandidate>,
    pub notices: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationReconcileOutcome {
    pub state_version: u64,
    pub newly_admitted_alerts: Vec<ProviderNotificationEvent>,
    pub unread_by_pull_request: Vec<PullRequestUnreadSummary>,
    pub repository_completeness: Vec<RepositoryNotificationCompleteness>,
    pub incomplete_candidates: Vec<IncompleteNotificationCandidate>,
    pub notices: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct NotificationStore {
    root: PathBuf,
    limits: NotificationStoreLimits,
}

impl NotificationStore {
    pub fn open(root: impl Into<PathBuf>, limits: NotificationStoreLimits) -> Result<Self> {
        limits.validate()?;
        let root = root.into();
        if !root.exists() {
            fs::create_dir_all(&root).context("Cannot create private notification directory")?;
        }
        let metadata =
            fs::symlink_metadata(&root).context("Cannot inspect notification directory")?;
        ensure!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "Notification store root must be a real directory"
        );
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .context("Cannot make notification directory private")?;
        Ok(Self { root, limits })
    }

    pub fn reconcile(
        &self,
        account: &Account,
        batch: &ProviderNotificationBatch,
    ) -> Result<NotificationReconcileOutcome> {
        validate_account(account)?;
        ensure!(
            &batch.account == account,
            "Provider batch belongs to another selected account"
        );
        self.with_lock(account, || {
            let mut state = self.load_state(account)?;
            if state.account.is_none() {
                state.account = Some(account.clone());
            }
            validate_batch(batch, &self.limits)?;
            let declared: HashSet<String> = batch.repositories.iter().map(|repo| scope_key(&repo.target)).collect();
            let mut incoming = Vec::new();
            for observation in &batch.observations {
                ensure!(declared.contains(&scope_key(&scope_from_target(&observation.target))), "Observation repository was not declared by the bounded batch");
                for event in &observation.events {
                    ensure!(event.identity.target == observation.target, "Event target differs from its provider observation");
                    incoming.push(event.clone());
                }
            }
            let incoming_ids: HashSet<String> = incoming.iter().map(event_key).collect();
            ensure!(incoming_ids.len() == incoming.len(), "Provider batch contains duplicate event identities");

            let mut changed = false;
            let mut newly_admitted = Vec::new();
            for event in incoming {
                let key = event_key(&event);
                let fingerprint = fingerprint(&event)?;
                if let Some(existing) = state.events.iter().find(|known| known.key == key) {
                    ensure!(existing.fingerprint == fingerprint && existing.event == event, "Provider returned inconsistent payload for an existing immutable event identity");
                    continue;
                }
                ensure!(state.events.len() < self.limits.max_events, "Notification retention bound cannot preserve dedupe/read evidence; reconciliation refused");
                let repo_key = scope_key(&scope_from_target(&event.identity.target));
                let baseline_exists = state.baselines.iter().any(|known| known == &repo_key);
                let admitted_version = state.state_version.checked_add(1).context("Notification state version exhausted")?;
                state.events.push(StoredEvent {
                    key,
                    fingerprint,
                    event: event.clone(),
                    unread: baseline_exists,
                    admitted_version,
                });
                if baseline_exists { newly_admitted.push(event); }
                changed = true;
            }

            for repository in &batch.repositories {
                let key = scope_key(&repository.target);
                if batch.full_snapshot && repository.complete && !state.baselines.iter().any(|known| known == &key) {
                    ensure!(state.baselines.len() < self.limits.max_repositories, "Notification baseline bound reached");
                    state.baselines.push(key);
                    changed = true;
                }
            }
            if state.repository_completeness != batch.repositories {
                state.repository_completeness = batch.repositories.clone();
                changed = true;
            }
            if state.incomplete_candidates != batch.incomplete_candidates {
                state.incomplete_candidates = batch.incomplete_candidates.clone();
                changed = true;
            }
            if state.notices != batch.notices {
                state.notices = batch.notices.clone();
                changed = true;
            }
            if changed {
                state.state_version = state.state_version.checked_add(1).context("Notification state version exhausted")?;
                self.save_state(account, &state)?;
            }
            let snapshot = snapshot_from_state(&state);
            Ok(NotificationReconcileOutcome {
                state_version: state.state_version,
                newly_admitted_alerts: newly_admitted,
                unread_by_pull_request: snapshot.unread_by_pull_request,
                repository_completeness: snapshot.repository_completeness,
                incomplete_candidates: snapshot.incomplete_candidates,
                notices: snapshot.notices,
            })
        })
    }

    pub fn list_unread(&self, account: &Account) -> Result<NotificationSnapshot> {
        validate_account(account)?;
        self.with_lock(account, || {
            Ok(snapshot_from_state(&self.load_state(account)?))
        })
    }

    pub fn mark_displayed_read(
        &self,
        account: &Account,
        displayed: &DisplayedNotificationSet,
    ) -> Result<NotificationSnapshot> {
        validate_account(account)?;
        ensure!(
            displayed.state_version > 0,
            "Displayed state version must be positive"
        );
        let requested: HashSet<String> = displayed.events.iter().map(identity_key).collect();
        ensure!(
            requested.len() == displayed.events.len(),
            "Displayed event identities must be unique"
        );
        self.with_lock(account, || {
            let mut state = self.load_state(account)?;
            ensure!(
                displayed.state_version <= state.state_version,
                "Displayed state version is from the future"
            );
            let mut found = HashSet::new();
            let mut changed = false;
            for event in &mut state.events {
                if requested.contains(&event.key) {
                    ensure!(
                        event.admitted_version <= displayed.state_version,
                        "Displayed set includes an event admitted after its version"
                    );
                    found.insert(event.key.clone());
                    if event.unread {
                        event.unread = false;
                        changed = true;
                    }
                }
            }
            ensure!(
                found == requested,
                "Displayed set contains an unknown or foreign event identity"
            );
            if changed {
                state.state_version = state
                    .state_version
                    .checked_add(1)
                    .context("Notification state version exhausted")?;
                self.save_state(account, &state)?;
            }
            Ok(snapshot_from_state(&state))
        })
    }

    fn with_lock<T>(&self, account: &Account, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let _guard = StoreLock::acquire(&self.lock_path(account))?;
        operation()
    }

    fn load_state(&self, account: &Account) -> Result<PersistedState> {
        let path = self.state_path(account);
        if !path.exists() {
            return Ok(PersistedState::default());
        }
        let file = open_nofollow_read(&path)?;
        validate_private_regular(&file, "notification state")?;
        let size: usize = file
            .metadata()?
            .len()
            .try_into()
            .context("Notification state is too large")?;
        ensure!(
            size <= self.limits.max_state_bytes,
            "Notification state exceeds configured byte bound; original preserved"
        );
        let mut bytes = Vec::with_capacity(size);
        file.take(self.limits.max_state_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .context("Cannot read notification state")?;
        ensure!(
            bytes.len() <= self.limits.max_state_bytes,
            "Notification state exceeds configured byte bound; original preserved"
        );
        let state: PersistedState = serde_json::from_slice(&bytes)
            .context("Notification state is corrupt; original preserved")?;
        ensure!(
            state.format_version == FORMAT_VERSION,
            "Notification state has a future or unsupported format; original preserved"
        );
        validate_persisted(&state, account, &self.limits)?;
        Ok(state)
    }

    fn save_state(&self, account: &Account, state: &PersistedState) -> Result<()> {
        validate_persisted(state, account, &self.limits)?;
        let bytes = serde_json::to_vec(state).context("Cannot encode notification state")?;
        ensure!(
            bytes.len() <= self.limits.max_state_bytes,
            "Notification state byte bound reached; prior state preserved"
        );
        let destination = self.state_path(account);
        if destination.exists() {
            let current = open_nofollow_read(&destination)?;
            validate_private_regular(&current, "notification state")?;
        }
        let mut temporary = destination.as_os_str().to_os_string();
        temporary.push(format!(".tmp.{}.{}", std::process::id(), unique_suffix()?));
        let temporary = PathBuf::from(temporary);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_CLOEXEC | O_NOFOLLOW)
            .open(&temporary)
            .context("Cannot create notification state temporary")?;
        let result = (|| -> Result<()> {
            file.write_all(&bytes)
                .context("Cannot write notification state temporary")?;
            file.sync_all()
                .context("Cannot sync notification state temporary")?;
            fs::rename(&temporary, &destination)
                .context("Cannot atomically replace notification state")?;
            sync_directory(&self.root)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn state_path(&self, account: &Account) -> PathBuf {
        self.root.join(format!("{}.json", account_digest(account)))
    }
    fn lock_path(&self, account: &Account) -> PathBuf {
        self.root.join(format!("{}.lock", account_digest(account)))
    }
}

#[derive(Serialize, Deserialize)]
struct PersistedState {
    format_version: u32,
    state_version: u64,
    account: Option<Account>,
    baselines: Vec<String>,
    events: Vec<StoredEvent>,
    #[serde(default)]
    repository_completeness: Vec<RepositoryNotificationCompleteness>,
    #[serde(default)]
    incomplete_candidates: Vec<IncompleteNotificationCandidate>,
    #[serde(default)]
    notices: Vec<String>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            state_version: 0,
            account: None,
            baselines: Vec::new(),
            events: Vec::new(),
            repository_completeness: Vec::new(),
            incomplete_candidates: Vec::new(),
            notices: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredEvent {
    key: String,
    fingerprint: String,
    event: ProviderNotificationEvent,
    unread: bool,
    admitted_version: u64,
}

struct StoreLock {
    file: File,
    acquired: bool,
}

impl StoreLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(O_CLOEXEC | O_NOFOLLOW)
            .open(path)
            .context("Cannot open private notification lock")?;
        validate_private_regular(&file, "notification lock")?;
        let result = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
        ensure!(
            result == 0,
            "Notification state is locked by another process"
        );
        Ok(Self {
            file,
            acquired: true,
        })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        if self.acquired {
            let _ = unsafe { flock(self.file.as_raw_fd(), LOCK_UN) };
            self.acquired = false;
        }
    }
}

fn validate_batch(
    batch: &ProviderNotificationBatch,
    limits: &NotificationStoreLimits,
) -> Result<()> {
    ensure!(
        !batch.repositories.is_empty() && batch.repositories.len() <= limits.max_repositories,
        "Provider batch repository bound exceeded"
    );
    ensure!(
        batch.observations.len() <= 1_000,
        "Provider batch observation bound exceeded"
    );
    ensure!(
        batch.incomplete_candidates.len() <= 10_000,
        "Provider batch incomplete-candidate bound exceeded"
    );
    ensure!(
        batch.complete
            == batch
                .repositories
                .iter()
                .all(|repository| repository.complete),
        "Provider batch and per-repository completeness disagree"
    );
    let mut repository_keys = HashSet::new();
    for repository in &batch.repositories {
        validate_scope(&repository.target, &batch.account)?;
        ensure!(
            repository_keys.insert(scope_key(&repository.target)),
            "Provider batch contains duplicate repository scopes"
        );
        ensure!(
            repository.reasons.len() <= 100,
            "Too many notification completeness reasons"
        );
        for reason in &repository.reasons {
            validate_text(
                reason,
                limits.max_text_bytes,
                "notification completeness reason",
            )?;
        }
    }
    for candidate in &batch.incomplete_candidates {
        validate_candidate(candidate, &batch.account, limits)?;
        if let Some(target) = &candidate.target {
            ensure!(
                repository_keys.contains(&scope_key(&scope_from_target(target))),
                "Incomplete candidate repository was not declared by the bounded batch"
            );
        }
    }
    for notice in &batch.notices {
        validate_text(notice, limits.max_text_bytes, "notification notice")?;
    }
    for observation in &batch.observations {
        validate_target(&observation.target, &batch.account)?;
        validate_text(
            &observation.provider_notification_id,
            128,
            "provider notification ID",
        )?;
        validate_text(&observation.provider_reason, 64, "provider reason")?;
        validate_text(
            &observation.notification_updated_at,
            40,
            "provider timestamp",
        )?;
        for event in &observation.events {
            validate_event(event, &batch.account, limits)?;
        }
    }
    Ok(())
}

fn validate_persisted(
    state: &PersistedState,
    account: &Account,
    limits: &NotificationStoreLimits,
) -> Result<()> {
    ensure!(
        state.events.len() <= limits.max_events
            && state.baselines.len() <= limits.max_repositories
            && state.repository_completeness.len() <= limits.max_repositories
            && state.incomplete_candidates.len() <= 10_000
            && state.notices.len() <= 1_000,
        "Persisted notification bounds exceeded"
    );
    if let Some(stored) = &state.account {
        ensure!(
            stored == account,
            "Notification state belongs to another account"
        );
    }
    let mut keys = HashSet::new();
    for event in &state.events {
        validate_event(&event.event, account, limits)?;
        ensure!(
            event.key == event_key(&event.event),
            "Persisted notification key mismatch"
        );
        ensure!(
            event.fingerprint == fingerprint(&event.event)?,
            "Persisted notification fingerprint mismatch"
        );
        ensure!(
            keys.insert(event.key.clone()),
            "Duplicate persisted notification identity"
        );
        ensure!(
            event.admitted_version <= state.state_version,
            "Persisted notification version mismatch"
        );
    }
    let baseline_set: HashSet<_> = state.baselines.iter().collect();
    ensure!(
        baseline_set.len() == state.baselines.len(),
        "Duplicate notification repository baseline"
    );
    let mut repository_keys = HashSet::new();
    for repository in &state.repository_completeness {
        validate_scope(&repository.target, account)?;
        ensure!(
            repository_keys.insert(scope_key(&repository.target)),
            "Duplicate persisted repository completeness"
        );
        ensure!(
            repository.reasons.len() <= 100,
            "Too many notification completeness reasons"
        );
        for reason in &repository.reasons {
            validate_text(
                reason,
                limits.max_text_bytes,
                "notification completeness reason",
            )?;
        }
    }
    for candidate in &state.incomplete_candidates {
        validate_candidate(candidate, account, limits)?;
    }
    for notice in &state.notices {
        validate_text(notice, limits.max_text_bytes, "notification notice")?;
    }
    Ok(())
}

fn validate_candidate(
    candidate: &IncompleteNotificationCandidate,
    account: &Account,
    limits: &NotificationStoreLimits,
) -> Result<()> {
    if let Some(target) = &candidate.target {
        validate_target(target, account)?;
    }
    if let Some(id) = &candidate.provider_notification_id {
        validate_text(id, 128, "candidate provider notification ID")?;
    }
    validate_text(
        &candidate.reason,
        limits.max_text_bytes,
        "incomplete candidate reason",
    )
}

fn validate_event(
    event: &ProviderNotificationEvent,
    account: &Account,
    limits: &NotificationStoreLimits,
) -> Result<()> {
    validate_target(&event.identity.target, account)?;
    validate_text(
        &event.identity.remote_event_id,
        limits.max_text_bytes,
        "remote event identity",
    )?;
    validate_text(&event.occurred_at, 40, "event timestamp")?;
    if let Some(actor) = &event.actor {
        validate_text(actor, 100, "event actor")?;
    }
    validate_text(&event.summary, limits.max_text_bytes, "event summary")?;
    validate_text(&event.url, limits.max_text_bytes, "event URL")?;
    let expected_url = format!(
        "https://{}/{}/{}/pull/{}",
        event.identity.target.host,
        event.identity.target.owner,
        event.identity.target.repository,
        event.identity.target.pull_request
    );
    ensure!(
        event.url == expected_url,
        "Notification event URL is not the exact target pull request"
    );
    Ok(())
}

fn validate_target(target: &NotificationPullRequest, account: &Account) -> Result<()> {
    ensure!(
        target.provider == "github"
            && target.host == account.host
            && target.account == account.login,
        "Notification target belongs to another provider, host, or account"
    );
    validate_text(&target.owner, 100, "repository owner")?;
    validate_text(&target.repository, 100, "repository name")?;
    ensure!(
        target.pull_request > 0,
        "Invalid notification pull-request number"
    );
    Ok(())
}

fn validate_scope(scope: &NotificationRepositoryScope, account: &Account) -> Result<()> {
    ensure!(
        scope.provider == "github" && scope.host == account.host && scope.account == account.login,
        "Notification repository scope belongs to another provider, host, or account"
    );
    validate_text(&scope.owner, 100, "repository owner")?;
    validate_text(&scope.repository, 100, "repository name")?;
    Ok(())
}

fn validate_account(account: &Account) -> Result<()> {
    ensure!(
        account.host == "github.com",
        "Notification store supports GitHub.com accounts only"
    );
    validate_text(&account.login, 100, "account login")
}

fn validate_text(value: &str, max: usize, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= max
            && !value.contains('\0')
            && !value
                .chars()
                .any(|value| value.is_control() && !matches!(value, '\n' | '\t')),
        "Invalid or oversized {label}"
    );
    Ok(())
}

fn snapshot_from_state(state: &PersistedState) -> NotificationSnapshot {
    let mut grouped: HashMap<NotificationPullRequest, Vec<ProviderNotificationEvent>> =
        HashMap::new();
    for stored in state.events.iter().filter(|stored| stored.unread) {
        grouped
            .entry(stored.event.identity.target.clone())
            .or_default()
            .push(stored.event.clone());
    }
    let mut unknown_by_target: HashMap<
        NotificationPullRequest,
        Vec<IncompleteNotificationCandidate>,
    > = HashMap::new();
    for candidate in &state.incomplete_candidates {
        if let Some(target) = &candidate.target {
            unknown_by_target
                .entry(target.clone())
                .or_default()
                .push(candidate.clone());
            grouped.entry(target.clone()).or_default();
        }
    }
    let mut unread_by_pull_request: Vec<_> = grouped
        .into_iter()
        .map(|(target, mut unread_events)| {
            unread_events.sort_by(|left, right| {
                left.occurred_at
                    .cmp(&right.occurred_at)
                    .then_with(|| event_key(left).cmp(&event_key(right)))
            });
            PullRequestUnreadSummary {
                unknown_candidates: unknown_by_target.remove(&target).unwrap_or_default(),
                target,
                unread_events,
            }
        })
        .collect();
    unread_by_pull_request.sort_by_key(|value| target_key(&value.target));
    NotificationSnapshot {
        state_version: state.state_version,
        unread_by_pull_request,
        repository_completeness: state.repository_completeness.clone(),
        incomplete_candidates: state.incomplete_candidates.clone(),
        notices: state.notices.clone(),
    }
}

fn scope_from_target(target: &NotificationPullRequest) -> NotificationRepositoryScope {
    NotificationRepositoryScope {
        provider: target.provider.clone(),
        host: target.host.clone(),
        account: target.account.clone(),
        owner: target.owner.clone(),
        repository: target.repository.clone(),
    }
}

fn target_key(target: &NotificationPullRequest) -> String {
    format!(
        "{}\0{}",
        scope_key(&scope_from_target(target)),
        target.pull_request
    )
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
fn identity_key(identity: &NotificationEventIdentity) -> String {
    serde_json::to_string(identity).expect("serializable identity")
}
fn event_key(event: &ProviderNotificationEvent) -> String {
    identity_key(&event.identity)
}

fn fingerprint(event: &ProviderNotificationEvent) -> Result<String> {
    let bytes =
        serde_json::to_vec(event).context("Cannot encode notification event fingerprint")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn account_digest(account: &Account) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            format!(
                "{}\0{}",
                account.host.to_ascii_lowercase(),
                account.login.to_ascii_lowercase()
            )
            .as_bytes()
        )
    )
}

fn open_nofollow_read(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_CLOEXEC | O_NOFOLLOW)
        .open(path)
        .context("Cannot open private notification file")
}

fn validate_private_regular(file: &File, label: &str) -> Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("Cannot inspect {label}"))?;
    ensure!(
        metadata.file_type().is_file() && metadata.nlink() == 1,
        "Private {label} must be a single-link regular file"
    );
    ensure!(
        metadata.mode() & 0o077 == 0,
        "Private {label} permissions are too broad"
    );
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?
        .sync_all()
        .context("Cannot sync notification directory")
}

fn unique_suffix() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before Unix epoch")?
        .as_nanos())
}

#[cfg(test)]
mod tests {
    crate::notification_store_tests!();
}
