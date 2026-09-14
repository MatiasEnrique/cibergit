use anyhow::{Context as _, Result, bail};
use cibergit::domain::{
    ActionsLinkage, CheckKind, CheckRepositoryIdentity, CheckShaClass, ProviderCoordinates,
    PullRequestDetails, Repository,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const SCHEMA_VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CONTROL_BYTES: u64 = 128 * 1024;
const MAX_OWNED_RECORDS: usize = 64;
const MAX_ROOT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ROOT_ENTRIES: usize = 256;
const CAS_SLOT_COUNT: usize = 256;
const LOCK_ATTEMPTS: usize = 25;
const LOCK_RETRY: Duration = Duration::from_millis(8);
const FUTURE_SKEW: Duration = Duration::from_secs(5 * 60);
const O_NOFOLLOW: i32 = 0x0000_0100;
const RECORD_PREFIX: &str = "v1-";
const RECORD_SUFFIX: &str = ".json";
const LOCK_NAME: &str = ".collaboration-cache.lock";
const CONTROL_NAME: &str = ".collaboration-cache-cas-v1.json";

static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" {
    fn geteuid() -> u32;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheIdentity {
    provider: String,
    host: String,
    account_host: String,
    account_login: String,
    owner: String,
    repository: String,
    pull_request: u64,
}

impl CacheIdentity {
    fn new(repository: &Repository, pull_request: u64) -> Self {
        Self {
            provider: "github".into(),
            host: repository.host.clone(),
            account_host: repository.account.host.clone(),
            account_login: repository.account.login.clone(),
            owner: repository.owner.clone(),
            repository: repository.name.clone(),
            pull_request,
        }
    }

    fn digest(&self) -> [u8; 32] {
        let encoded = serde_json::to_vec(self).expect("cache identity is serializable");
        Sha256::digest(encoded).into()
    }

    fn filename(&self) -> String {
        format!("{RECORD_PREFIX}{}{RECORD_SUFFIX}", hex(&self.digest()))
    }

    fn matches_coordinates(&self, coordinates: &ProviderCoordinates) -> bool {
        coordinates.provider == self.provider
            && coordinates.host == self.host
            && coordinates.owner == self.owner
            && coordinates.repository == self.repository
            && coordinates.pull_request == self.pull_request
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheRecord {
    schema_version: u32,
    identity: CacheIdentity,
    observed_at_unix_ms: u64,
    request_reservation: u64,
    details: PullRequestDetails,
}

#[derive(Clone, Debug)]
pub(super) struct CachedObservation {
    pub details: PullRequestDetails,
    pub observed_at_unix_ms: u64,
}

#[derive(Clone, Debug)]
pub(super) struct PreparedWrite {
    identity: CacheIdentity,
    identity_digest: [u8; 32],
    reservation: u64,
    predecessor: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CasSlot {
    identity_sha256: String,
    reservation: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CasControl {
    schema_version: u32,
    next_reservation: u64,
    slots: Vec<Option<CasSlot>>,
}

impl Default for CasControl {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            next_reservation: 0,
            slots: vec![None; CAS_SLOT_COUNT],
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct CollaborationCache {
    root: PathBuf,
}

impl CollaborationCache {
    pub fn new(data_root: PathBuf) -> Self {
        Self {
            root: data_root.join("collaboration-cache-v1"),
        }
    }

    pub fn load(
        &self,
        repository: &Repository,
        pull_request: u64,
    ) -> Result<Option<CachedObservation>> {
        match fs::symlink_metadata(&self.root) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("inspect collaboration cache root"),
        }
        let identity = CacheIdentity::new(repository, pull_request);
        self.with_lock(false, || {
            let Some(read) = self.read_record(&identity)? else {
                return Ok(None);
            };
            Ok(Some(CachedObservation {
                details: read.record.details,
                observed_at_unix_ms: read.record.observed_at_unix_ms,
            }))
        })
    }

    /// Allocate a durable pre-request reservation. A later request for this key,
    /// or a conservative slot collision, invalidates this permit even if the
    /// payload record is later removed and the path becomes absent again.
    pub fn prepare_write(
        &self,
        repository: &Repository,
        pull_request: u64,
    ) -> Result<PreparedWrite> {
        let identity = CacheIdentity::new(repository, pull_request);
        self.with_lock(true, || {
            let predecessor = self.read_record(&identity)?.map(|read| read.digest);
            let mut control = match self.read_control()? {
                Some(control) => control,
                None if self.control_can_initialize()? => CasControl::default(),
                None => bail!(
                    "collaboration cache CAS control is absent from an existing cache; refusing to reset reservations"
                ),
            };
            control.next_reservation = control
                .next_reservation
                .checked_add(1)
                .context("collaboration cache reservation counter exhausted")?;
            let reservation = control.next_reservation;
            let identity_digest = identity.digest();
            let identity_hex = hex(&identity_digest);
            let slot = control
                .slots
                .iter()
                .position(|slot| {
                    slot.as_ref()
                        .is_some_and(|slot| slot.identity_sha256 == identity_hex)
                })
                .or_else(|| control.slots.iter().position(Option::is_none))
                .or_else(|| {
                    control
                        .slots
                        .iter()
                        .enumerate()
                        .filter_map(|(index, slot)| {
                            slot.as_ref().map(|slot| (index, slot.reservation))
                        })
                        .min_by_key(|(_, reservation)| *reservation)
                        .map(|(index, _)| index)
                })
                .context("collaboration cache CAS table has no replaceable slot")?;
            control.slots[slot] = Some(CasSlot {
                identity_sha256: identity_hex,
                reservation,
            });
            self.write_control(&control, Some(&identity.filename()))?;
            Ok(PreparedWrite {
                identity,
                identity_digest,
                reservation,
                predecessor,
            })
        })
    }

    pub fn save(
        &self,
        prepared: PreparedWrite,
        details: PullRequestDetails,
        observed_at_unix_ms: u64,
    ) -> Result<()> {
        validate_observed_at(observed_at_unix_ms)?;
        validate_details(&prepared.identity, &details)?;
        let record = CacheRecord {
            schema_version: SCHEMA_VERSION,
            identity: prepared.identity.clone(),
            observed_at_unix_ms,
            request_reservation: prepared.reservation,
            details,
        };
        let encoded = serde_json::to_vec(&record).context("encode collaboration cache record")?;
        if encoded.len() as u64 > MAX_RECORD_BYTES {
            bail!("collaboration cache record exceeds the 4 MiB bound");
        }

        self.with_lock(true, || {
            let control = self
                .read_control()?
                .context("collaboration cache CAS control is missing")?;
            let identity_hex = hex(&prepared.identity_digest);
            let expected_slot = control.slots.iter().flatten().find(|slot| {
                slot.identity_sha256 == identity_hex && slot.reservation == prepared.reservation
            });
            if expected_slot.is_none() {
                bail!("collaboration cache write lost its durable request reservation");
            }
            let current = self
                .read_record(&prepared.identity)?
                .map(|read| read.digest);
            if current != prepared.predecessor {
                bail!("collaboration cache write lost its predecessor CAS");
            }
            self.retain_for_write(&prepared.identity, encoded.len() as u64)?;
            self.atomic_write(&prepared.identity.filename(), &encoded, MAX_RECORD_BYTES)
        })
    }

    fn with_lock<T>(&self, create_root: bool, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        self.ensure_root(create_root)?;
        let root_descriptor = open_private_directory(&self.root)?;
        let lock_path = self.root.join(LOCK_NAME);
        let lock_file = open_private_file(&lock_path, true)?;
        let mut guard = AdvisoryLock::acquire(lock_file)?;
        guard.validate_path(&lock_path)?;
        let result = operation();
        let root_check = validate_directory_descriptor(&self.root, &root_descriptor);
        let lock_check = guard.validate_path(&lock_path);
        let unlock = guard.unlock();
        match (result, root_check, lock_check, unlock) {
            (Ok(value), Ok(()), Ok(()), Ok(())) => Ok(value),
            (Err(error), _, _, _) => Err(error),
            (Ok(_), Err(error), _, _) | (Ok(_), Ok(()), Err(error), _) => Err(error),
            (Ok(_), Ok(()), Ok(()), Err(error)) => Err(error).context("unlock collaboration cache"),
        }
    }

    fn ensure_root(&self, create: bool) -> Result<()> {
        if !self.root.exists() {
            if !create {
                return Ok(());
            }
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&self.root) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).context("create collaboration cache root"),
            }
        }
        validate_private_directory(&self.root)
    }

    fn read_record(&self, identity: &CacheIdentity) -> Result<Option<ReadRecord>> {
        let path = self.root.join(identity.filename());
        let Some(bytes) = read_bounded_private(&path, MAX_RECORD_BYTES)? else {
            return Ok(None);
        };
        let digest = Sha256::digest(&bytes).into();
        let record: CacheRecord = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "refuse corrupt collaboration cache entry {}",
                identity.filename()
            )
        })?;
        validate_record(&record, identity)?;
        Ok(Some(ReadRecord { record, digest }))
    }

    fn read_control(&self) -> Result<Option<CasControl>> {
        let path = self.root.join(CONTROL_NAME);
        let Some(bytes) = read_bounded_private(&path, MAX_CONTROL_BYTES)? else {
            return Ok(None);
        };
        let control: CasControl = serde_json::from_slice(&bytes)
            .context("refuse corrupt collaboration cache CAS control")?;
        if control.schema_version != SCHEMA_VERSION {
            bail!("refuse future or unsupported collaboration cache CAS schema");
        }
        if control.slots.len() != CAS_SLOT_COUNT {
            bail!("refuse collaboration cache CAS control with invalid slot count");
        }
        for slot in control.slots.iter().flatten() {
            if slot.identity_sha256.len() != 64
                || !slot
                    .identity_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || slot.reservation == 0
                || slot.reservation > control.next_reservation
            {
                bail!("refuse invalid collaboration cache CAS slot");
            }
        }
        Ok(Some(control))
    }

    fn write_control(&self, control: &CasControl, protected_record: Option<&str>) -> Result<()> {
        let encoded =
            serde_json::to_vec(control).context("encode collaboration cache CAS control")?;
        if encoded.len() as u64 > MAX_CONTROL_BYTES {
            bail!("collaboration cache CAS control exceeds its bound");
        }
        self.retain_for_atomic_write(CONTROL_NAME, encoded.len() as u64, false, protected_record)?;
        self.atomic_write(CONTROL_NAME, &encoded, MAX_CONTROL_BYTES)
    }

    fn retain_for_write(&self, target: &CacheIdentity, incoming: u64) -> Result<()> {
        let filename = target.filename();
        self.retain_for_atomic_write(&filename, incoming, true, Some(&filename))
    }

    fn retain_for_atomic_write(
        &self,
        target_name: &str,
        incoming: u64,
        adds_owned_record: bool,
        protected_record: Option<&str>,
    ) -> Result<()> {
        let mut entries = Vec::new();
        let mut root_bytes = 0_u64;
        for (position, entry) in fs::read_dir(&self.root)
            .context("scan collaboration cache root")?
            .enumerate()
        {
            if position >= MAX_ROOT_ENTRIES {
                bail!("collaboration cache root exceeds the 256-entry scan bound");
            }
            let entry = entry.context("read collaboration cache directory entry")?;
            let metadata = fs::symlink_metadata(entry.path())
                .context("inspect collaboration cache directory entry")?;
            if metadata.file_type().is_dir()
                || (!metadata.file_type().is_file() && !metadata.file_type().is_symlink())
            {
                bail!(
                    "collaboration cache root contains an unsupported entry whose occupancy cannot be bounded; preserving it and refusing the write"
                );
            }
            root_bytes = root_bytes
                .checked_add(metadata.len())
                .context("collaboration cache root byte count overflow")?;
            entries.push((entry.path(), metadata));
        }

        let target_path = self.root.join(target_name);
        let protected_path = protected_record.map(|name| self.root.join(name));
        let mut owned = Vec::new();
        for (path, metadata) in &entries {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !is_record_name(name) {
                continue;
            }
            // Only an entry that passes the complete safe read, schema,
            // filename/identity, timestamp and coordinate validation is owned.
            if let Ok(Some(read)) = read_any_record(path, metadata) {
                owned.push((
                    path.clone(),
                    metadata.len(),
                    read.record.observed_at_unix_ms,
                ));
            }
        }

        let target_is_owned = owned.iter().any(|(path, _, _)| path == &target_path);
        let mut final_items = owned.len() + usize::from(adds_owned_record && !target_is_owned);
        let mut peak_bytes = root_bytes
            .checked_add(incoming)
            .context("collaboration cache root byte count overflow")?;
        let mut peak_entries = entries.len().saturating_add(1);
        owned.sort_by_key(|(_, _, observed)| *observed);
        let mut removals = Vec::new();
        for (path, size, _) in owned {
            if final_items <= MAX_OWNED_RECORDS
                && peak_bytes <= MAX_ROOT_BYTES
                && peak_entries <= MAX_ROOT_ENTRIES
            {
                break;
            }
            if path == target_path || protected_path.as_ref() == Some(&path) {
                continue;
            }
            removals.push(path);
            peak_bytes = peak_bytes.saturating_sub(size);
            peak_entries = peak_entries.saturating_sub(1);
            final_items = final_items.saturating_sub(1);
        }
        // The new temp coexists with the current target until rename. Count that
        // peak explicitly; target_size is intentionally not subtracted.
        if final_items > MAX_OWNED_RECORDS
            || peak_bytes > MAX_ROOT_BYTES
            || peak_entries > MAX_ROOT_ENTRIES
        {
            bail!("safe collaboration cache retention cannot prove configured bounds");
        }
        for path in removals {
            let metadata = fs::symlink_metadata(&path).context("recheck retained cache entry")?;
            if read_any_record(&path, &metadata)?.is_none() {
                bail!("collaboration cache retention lost ownership proof");
            }
            fs::remove_file(&path).context("remove validated disposable cache entry")?;
        }
        Ok(())
    }

    fn control_can_initialize(&self) -> Result<bool> {
        let mut count = 0_usize;
        for entry in fs::read_dir(&self.root).context("inspect fresh collaboration cache root")? {
            count += 1;
            if count > MAX_ROOT_ENTRIES {
                return Ok(false);
            }
            let entry = entry.context("read fresh collaboration cache entry")?;
            if entry.file_name().to_str() != Some(LOCK_NAME) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn atomic_write(&self, name: &str, bytes: &[u8], bound: u64) -> Result<()> {
        if bytes.len() as u64 > bound {
            bail!("collaboration cache atomic write exceeds its bound");
        }
        let nonce = TEMP_NONCE.fetch_add(1, Ordering::Relaxed);
        let temp_name = format!(".tmp-{}-{nonce}", std::process::id());
        self.atomic_write_with_temp(name, bytes, bound, &temp_name)
    }

    fn atomic_write_with_temp(
        &self,
        name: &str,
        bytes: &[u8],
        bound: u64,
        temp_name: &str,
    ) -> Result<()> {
        if bytes.len() as u64 > bound {
            bail!("collaboration cache atomic write exceeds its bound");
        }
        let temp_path = self.root.join(temp_name);
        let target_path = self.root.join(name);
        let mut created_identity = None;
        let result = (|| {
            let mut options = OpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(O_NOFOLLOW);
            let mut file = options
                .open(&temp_path)
                .context("create private collaboration cache temp")?;
            validate_open_file(&temp_path, &file, None)?;
            let metadata = file
                .metadata()
                .context("capture collaboration cache temp identity")?;
            created_identity = Some((metadata.dev(), metadata.ino()));
            file.write_all(bytes)
                .context("write collaboration cache temp")?;
            file.sync_all().context("fsync collaboration cache temp")?;
            drop(file);
            fs::rename(&temp_path, &target_path).context("install collaboration cache record")?;
            sync_directory(&self.root)
        })();
        if result.is_err()
            && let Some((device, inode)) = created_identity
        {
            let _ = remove_exact_temp(&temp_path, device, inode);
        }
        result
    }
}

struct ReadRecord {
    record: CacheRecord,
    digest: [u8; 32],
}

struct AdvisoryLock {
    file: Option<File>,
}

impl AdvisoryLock {
    fn acquire(file: File) -> Result<Self> {
        for attempt in 0..LOCK_ATTEMPTS {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file: Some(file) }),
                Err(fs::TryLockError::WouldBlock) if attempt + 1 < LOCK_ATTEMPTS => {
                    thread::sleep(LOCK_RETRY);
                }
                Err(fs::TryLockError::WouldBlock) => {
                    bail!("collaboration cache lock contention exceeded 200 ms");
                }
                Err(fs::TryLockError::Error(error)) => {
                    return Err(error).context("lock collaboration cache");
                }
            }
        }
        unreachable!("bounded lock loop always returns")
    }

    fn unlock(&mut self) -> Result<()> {
        if let Some(file) = self.file.take() {
            file.unlock()
                .context("explicitly unlock collaboration cache")?;
        }
        Ok(())
    }

    fn validate_path(&self, path: &Path) -> Result<()> {
        let file = self
            .file
            .as_ref()
            .context("collaboration cache lock descriptor is absent")?;
        validate_open_file(path, file, None)
    }
}

impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

fn validate_record(record: &CacheRecord, expected: &CacheIdentity) -> Result<()> {
    if record.schema_version != SCHEMA_VERSION {
        bail!("refuse future or unsupported collaboration cache schema");
    }
    if &record.identity != expected {
        bail!("refuse collaboration cache identity mismatch");
    }
    if record.request_reservation == 0 {
        bail!("refuse collaboration cache record without a request reservation");
    }
    validate_observed_at(record.observed_at_unix_ms)?;
    validate_details(expected, &record.details)
}

fn validate_details(identity: &CacheIdentity, details: &PullRequestDetails) -> Result<()> {
    if details.number != identity.pull_request {
        bail!("refuse collaboration cache payload for another pull request");
    }
    for coordinates in details
        .issue_comments
        .iter()
        .map(|value| &value.coordinates)
        .chain(details.reviews.iter().map(|value| &value.coordinates))
        .chain(
            details
                .review_threads
                .iter()
                .map(|value| &value.coordinates),
        )
        .chain(
            details
                .review_threads
                .iter()
                .flat_map(|thread| thread.comments.iter().map(|value| &value.coordinates)),
        )
        .chain(details.checks.iter().map(|value| &value.coordinates))
    {
        if !identity.matches_coordinates(coordinates) {
            bail!("refuse collaboration cache payload with mismatched provider coordinates");
        }
    }
    for reaction in &details.reactions {
        if !identity.matches_coordinates(&reaction.pull_request)
            || !identity.matches_coordinates(&reaction.subject)
            || reaction
                .parent_review
                .as_ref()
                .is_some_and(|parent| !identity.matches_coordinates(parent))
        {
            bail!("refuse collaboration cache payload with mismatched reaction coordinates");
        }
        match (reaction.kind, &reaction.parent_review) {
            (cibergit::domain::ReactableKind::PullRequest, None)
                if reaction.subject == reaction.pull_request => {}
            (cibergit::domain::ReactableKind::PullRequestReviewComment, Some(_)) => {}
            (cibergit::domain::ReactableKind::IssueComment, None)
            | (cibergit::domain::ReactableKind::PullRequestReview, None) => {}
            _ => bail!("refuse collaboration cache payload with invalid reaction parent shape"),
        }
    }
    validate_check_identities(identity, details)?;
    Ok(())
}

fn validate_check_identities(identity: &CacheIdentity, details: &PullRequestDetails) -> Result<()> {
    if let Some(node_id) = &details.pull_request_node_id {
        validate_cached_node_id(node_id)?;
    }
    for sha in [
        details.observed_head_sha.as_deref(),
        details.rollup_commit_sha.as_deref(),
        details.potential_merge_commit_sha.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_cached_sha(sha)?;
    }
    for repository in [
        details.base_repository.as_ref(),
        details.head_repository.as_ref(),
        details.rollup_repository.as_ref(),
        details.potential_merge_commit_repository.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_cached_repository(repository)?;
    }
    if let Some(base) = &details.base_repository
        && !base
            .name_with_owner
            .eq_ignore_ascii_case(&format!("{}/{}", identity.owner, identity.repository))
    {
        bail!("refuse collaboration cache payload with foreign base repository identity");
    }
    if details.pull_request_node_id.is_some() != details.base_repository.is_some() {
        bail!("refuse collaboration cache payload with incomplete PR/base identity");
    }
    if details.rollup_commit_sha.is_some() != details.rollup_repository.is_some() {
        bail!("refuse collaboration cache payload with incomplete rollup commit identity");
    }
    if details.potential_merge_commit_sha.is_some()
        != details.potential_merge_commit_repository.is_some()
    {
        bail!("refuse collaboration cache payload with incomplete merge-candidate identity");
    }
    if let Some(repository) = &details.rollup_repository {
        let base_matches = details.base_repository.as_ref().map_or_else(
            || {
                repository
                    .name_with_owner
                    .eq_ignore_ascii_case(&format!("{}/{}", identity.owner, identity.repository))
            },
            |base| same_cached_repository(base, repository),
        );
        let head_matches = details
            .head_repository
            .as_ref()
            .is_some_and(|head| same_cached_repository(head, repository));
        if !base_matches && !head_matches {
            bail!("refuse collaboration cache payload with foreign rollup repository");
        }
    }
    if let Some(repository) = &details.potential_merge_commit_repository {
        let base_matches = details.base_repository.as_ref().map_or_else(
            || {
                repository
                    .name_with_owner
                    .eq_ignore_ascii_case(&format!("{}/{}", identity.owner, identity.repository))
            },
            |base| same_cached_repository(base, repository),
        );
        if !base_matches {
            bail!("refuse collaboration cache payload with foreign merge-candidate repository");
        }
    }
    for check in &details.checks {
        if let Some(sha) = &check.commit_sha {
            validate_cached_sha(sha)?;
        }
        let expected_class = match check.commit_sha.as_deref() {
            Some(sha) if details.observed_head_sha.as_deref() == Some(sha) => CheckShaClass::Head,
            Some(sha) if details.potential_merge_commit_sha.as_deref() == Some(sha) => {
                CheckShaClass::MergeCandidate
            }
            Some(_) => CheckShaClass::Other,
            None => CheckShaClass::Unknown,
        };
        if check.sha_class != expected_class {
            bail!("refuse collaboration cache payload with inconsistent check SHA class");
        }
        if let Some(repository) = &check.commit_repository {
            validate_cached_check_origin(identity, details, repository)?;
        }
        if let Some(suite) = &check.suite {
            validate_cached_check_origin(identity, details, &suite.repository)?;
            validate_cached_graphql_database_id(suite.database_id)?;
            if check.kind != CheckKind::CheckRun
                || check.commit_repository.as_ref() != Some(&suite.repository)
            {
                bail!("refuse collaboration cache payload with inconsistent check-suite origin");
            }
            if let Some(app) = &suite.app {
                validate_cached_node_id(&app.node_id)?;
                validate_cached_text(&app.name)?;
                validate_cached_text(&app.slug)?;
            }
        }
        validate_cached_graphql_database_id(check.database_id)?;
        match (&check.kind, &check.actions_linkage) {
            (CheckKind::CommitStatus, ActionsLinkage::Linked(_)) => {
                bail!("refuse collaboration cache status context with Actions identity")
            }
            (_, ActionsLinkage::Linked(run)) => {
                if check.suite.is_none() {
                    bail!("refuse collaboration cache Actions identity without a check suite");
                }
                validate_cached_node_id(&run.node_id)?;
                validate_cached_node_id(&run.workflow_node_id)?;
                validate_cached_graphql_database_id(Some(run.database_id))?;
                validate_cached_graphql_database_id(Some(run.workflow_database_id))?;
                validate_cached_graphql_database_id(Some(run.run_attempt))?;
                validate_cached_graphql_database_id(Some(run.run_number))?;
                validate_cached_text(&run.event)?;
                validate_cached_text(&run.workflow_name)?;
                if !run.github_url.starts_with("https://github.com/")
                    || !valid_cached_uri(&run.github_url)
                {
                    bail!("refuse collaboration cache payload with invalid GitHub run URL");
                }
            }
            _ => {}
        }
        if check.kind == CheckKind::CommitStatus
            && (check.database_id.is_some()
                || check.suite.is_some()
                || check.github_permalink.is_some())
        {
            bail!("refuse collaboration cache status context with CheckRun identity");
        }
        if let Some(permalink) = &check.github_permalink
            && (!permalink.starts_with("https://github.com/") || !valid_cached_uri(permalink))
        {
            bail!("refuse collaboration cache payload with invalid GitHub check permalink");
        }
        if let Some(url) = &check.details_url
            && !valid_cached_uri(url)
        {
            bail!("refuse collaboration cache payload with invalid display-only integrator URL");
        }
    }
    Ok(())
}

fn validate_cached_check_origin(
    identity: &CacheIdentity,
    details: &PullRequestDetails,
    repository: &CheckRepositoryIdentity,
) -> Result<()> {
    validate_cached_repository(repository)?;
    let base_name = format!("{}/{}", identity.owner, identity.repository);
    let base_allowed = details.base_repository.as_ref().map_or_else(
        || repository.name_with_owner.eq_ignore_ascii_case(&base_name),
        |base| same_cached_repository(base, repository),
    );
    let allowed = base_allowed
        || details
            .head_repository
            .as_ref()
            .is_some_and(|candidate| same_cached_repository(candidate, repository))
        || details
            .rollup_repository
            .as_ref()
            .is_some_and(|candidate| same_cached_repository(candidate, repository));
    if !allowed {
        bail!("refuse collaboration cache payload with foreign nested check repository");
    }
    Ok(())
}

fn same_cached_repository(left: &CheckRepositoryIdentity, right: &CheckRepositoryIdentity) -> bool {
    left.node_id == right.node_id
        && left
            .name_with_owner
            .eq_ignore_ascii_case(&right.name_with_owner)
}

fn validate_cached_repository(repository: &CheckRepositoryIdentity) -> Result<()> {
    validate_cached_node_id(&repository.node_id)?;
    let Some((owner, name)) = repository.name_with_owner.split_once('/') else {
        bail!("refuse collaboration cache payload with invalid repository coordinates");
    };
    if owner.is_empty()
        || name.is_empty()
        || owner.len() > 100
        || name.len() > 100
        || owner.contains('/')
        || name.contains('/')
        || owner.chars().any(char::is_whitespace)
        || name.chars().any(char::is_whitespace)
    {
        bail!("refuse collaboration cache payload with invalid repository coordinates");
    }
    Ok(())
}

fn validate_cached_sha(sha: &str) -> Result<()> {
    if sha.len() != 40
        || !sha
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("refuse collaboration cache payload with invalid exact check SHA");
    }
    Ok(())
}

fn validate_cached_node_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 1024 || id.contains('\0') || id.chars().any(char::is_whitespace)
    {
        bail!("refuse collaboration cache payload with invalid opaque check node ID");
    }
    Ok(())
}

fn validate_cached_graphql_database_id(id: Option<u64>) -> Result<()> {
    if id.is_some_and(|id| id == 0 || id > i32::MAX as u64) {
        bail!("refuse collaboration cache payload with out-of-range GraphQL database ID");
    }
    Ok(())
}

fn validate_cached_text(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        bail!("refuse collaboration cache payload with invalid check identity text");
    }
    Ok(())
}

fn valid_cached_uri(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 16 * 1024
        && !value.chars().any(char::is_control)
        && !value.chars().any(char::is_whitespace)
}

fn validate_observed_at(observed_at_unix_ms: u64) -> Result<()> {
    if observed_at_unix_ms == 0 {
        bail!("refuse collaboration cache record without an observation time");
    }
    let now = now_unix_ms()?;
    let allowance = FUTURE_SKEW.as_millis() as u64;
    if observed_at_unix_ms > now.saturating_add(allowance) {
        bail!("refuse collaboration cache record observed implausibly in the future");
    }
    Ok(())
}

pub(super) fn now_unix_ms() -> Result<u64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    u64::try_from(duration.as_millis()).context("current timestamp exceeds u64 milliseconds")
}

fn read_any_record(path: &Path, initial: &fs::Metadata) -> Result<Option<ReadRecord>> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    if !is_record_name(name) {
        return Ok(None);
    }
    validate_metadata(initial, MAX_RECORD_BYTES)?;
    let Some(bytes) = read_bounded_private(path, MAX_RECORD_BYTES)? else {
        return Ok(None);
    };
    let record: CacheRecord = serde_json::from_slice(&bytes).context("parse owned cache entry")?;
    if record.identity.filename() != name {
        bail!("cache record filename does not match its identity");
    }
    validate_record(&record, &record.identity)?;
    Ok(Some(ReadRecord {
        digest: Sha256::digest(&bytes).into(),
        record,
    }))
}

fn read_bounded_private(path: &Path, max: u64) -> Result<Option<Vec<u8>>> {
    let initial = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect collaboration cache entry"),
    };
    validate_metadata(&initial, max)?;
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(O_NOFOLLOW);
    let mut file = options
        .open(path)
        .context("open collaboration cache entry nofollow")?;
    validate_open_file(path, &file, Some(&initial))?;
    let mut bytes = Vec::with_capacity(initial.len().min(max) as usize);
    Read::by_ref(&mut file)
        .take(max + 1)
        .read_to_end(&mut bytes)
        .context("read bounded collaboration cache entry")?;
    if bytes.len() as u64 > max {
        bail!("refuse oversized collaboration cache entry");
    }
    let final_descriptor = file
        .metadata()
        .context("post-check collaboration cache descriptor")?;
    validate_metadata(&final_descriptor, max)?;
    let final_path = fs::symlink_metadata(path).context("post-check collaboration cache entry")?;
    validate_metadata(&final_path, max)?;
    if !same_snapshot(&initial, &final_descriptor) || !same_snapshot(&initial, &final_path) {
        bail!("collaboration cache entry changed during read");
    }
    Ok(Some(bytes))
}

fn open_private_file(path: &Path, create: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW);
    if create {
        options.create(true);
    }
    let file = options
        .open(path)
        .context("open private collaboration cache file")?;
    validate_open_file(path, &file, None)?;
    Ok(file)
}

fn validate_open_file(path: &Path, file: &File, expected: Option<&fs::Metadata>) -> Result<()> {
    let descriptor = file
        .metadata()
        .context("inspect collaboration cache descriptor")?;
    validate_metadata(&descriptor, u64::MAX)?;
    let path_metadata = fs::symlink_metadata(path).context("inspect collaboration cache path")?;
    validate_metadata(&path_metadata, u64::MAX)?;
    if !same_identity(&descriptor, &path_metadata)
        || expected.is_some_and(|expected| !same_identity(expected, &descriptor))
    {
        bail!("collaboration cache descriptor identity mismatch");
    }
    Ok(())
}

fn validate_metadata(metadata: &fs::Metadata, max: u64) -> Result<()> {
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("refuse non-regular collaboration cache entry");
    }
    if metadata.nlink() != 1 {
        bail!("refuse hard-linked collaboration cache entry");
    }
    // SAFETY: geteuid has no preconditions and does not retain pointers.
    if metadata.uid() != unsafe { geteuid() } {
        bail!("refuse collaboration cache entry owned by another user");
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("refuse collaboration cache entry with non-private permissions");
    }
    if metadata.len() > max {
        bail!("refuse oversized collaboration cache entry");
    }
    Ok(())
}

fn validate_private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("inspect collaboration cache root")?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!("refuse non-directory collaboration cache root");
    }
    // SAFETY: geteuid has no preconditions and does not retain pointers.
    if metadata.uid() != unsafe { geteuid() } {
        bail!("refuse collaboration cache root owned by another user");
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("refuse collaboration cache root with non-private permissions");
    }
    Ok(())
}

fn open_private_directory(path: &Path) -> Result<File> {
    let initial = fs::symlink_metadata(path).context("inspect collaboration cache root")?;
    validate_private_directory(path)?;
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(O_NOFOLLOW);
    let directory = options
        .open(path)
        .context("open collaboration cache root nofollow")?;
    let descriptor = directory
        .metadata()
        .context("inspect collaboration cache root descriptor")?;
    if !descriptor.is_dir() || !same_identity(&initial, &descriptor) {
        bail!("collaboration cache root descriptor identity mismatch");
    }
    Ok(directory)
}

fn validate_directory_descriptor(path: &Path, descriptor: &File) -> Result<()> {
    validate_private_directory(path)?;
    let current = fs::symlink_metadata(path).context("post-check collaboration cache root")?;
    let held = descriptor
        .metadata()
        .context("post-check collaboration cache root descriptor")?;
    if !held.is_dir() || !same_identity(&current, &held) {
        bail!("collaboration cache root changed while locked");
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    let directory = File::open(path).context("open collaboration cache root for fsync")?;
    let descriptor = directory
        .metadata()
        .context("inspect collaboration cache root descriptor")?;
    let current = fs::symlink_metadata(path).context("post-check collaboration cache root")?;
    if !same_identity(&descriptor, &current) || !descriptor.is_dir() {
        bail!("collaboration cache root changed during write");
    }
    directory
        .sync_all()
        .context("fsync collaboration cache root")
}

fn remove_exact_temp(path: &Path, device: u64, inode: u64) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspect collaboration cache temp"),
    };
    validate_metadata(&metadata, MAX_RECORD_BYTES.max(MAX_CONTROL_BYTES))?;
    if metadata.dev() != device || metadata.ino() != inode {
        bail!("collaboration cache temp identity changed; preserving it");
    }
    fs::remove_file(path).context("remove owned collaboration cache temp")
}

fn same_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn same_snapshot(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_identity(left, right)
        && left.len() == right.len()
        && left.nlink() == right.nlink()
        && left.uid() == right.uid()
        && left.mode() == right.mode()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
}

fn is_record_name(name: &str) -> bool {
    let Some(digest) = name
        .strip_prefix(RECORD_PREFIX)
        .and_then(|value| value.strip_suffix(RECORD_SUFFIX))
    else {
        return false;
    };
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::domain::{
        Account, CheckKind, CheckRepositoryIdentity, CheckSuiteIdentity, IssueComment,
        MergeEligibility, PullRequestCheck, PullRequestReview, ReactableKind, ReactionSnapshot,
        ReactionSubjectSnapshot, ReviewComment, ReviewThread,
    };
    use std::{env, os::unix::fs::symlink, process::Command, time::Instant};

    fn repository(owner: &str, name: &str, login: &str) -> Repository {
        Repository {
            host: "github.com".into(),
            owner: owner.into(),
            name: name.into(),
            account: Account {
                host: "github.com".into(),
                login: login.into(),
            },
            local_path: None,
        }
    }

    fn coordinates(repository: &Repository, number: u64, remote_id: &str) -> ProviderCoordinates {
        ProviderCoordinates {
            provider: "github".into(),
            host: repository.host.clone(),
            owner: repository.owner.clone(),
            repository: repository.name.clone(),
            pull_request: number,
            remote_id: remote_id.into(),
        }
    }

    fn details(repository: &Repository, number: u64, marker: &str) -> PullRequestDetails {
        PullRequestDetails {
            number,
            pull_request_node_id: None,
            base_repository: None,
            observed_head_sha: None,
            rollup_commit_sha: None,
            potential_merge_commit_sha: None,
            head_repository: None,
            rollup_repository: None,
            potential_merge_commit_repository: None,
            body: format!("Overview {marker}"),
            requested_reviewers: vec!["reviewer".into()],
            labels: vec!["cache".into()],
            assignees: vec!["assignee".into()],
            merge_eligibility: MergeEligibility {
                state: "OPEN".into(),
                draft: false,
                mergeable: "MERGEABLE".into(),
                merge_state_status: "CLEAN".into(),
                review_status: "APPROVED".into(),
                check_status: "PASSING".into(),
                maintainer_can_modify: true,
                can_rebase: true,
                can_update_branch: false,
                auto_merge_enabled: false,
                in_merge_queue: false,
            },
            issue_comments: vec![IssueComment {
                coordinates: coordinates(repository, number, "issue-1"),
                author: Some("reader".into()),
                body: format!("Issue discussion {marker}"),
                created_at: "2026-09-13T12:00:00Z".into(),
                updated_at: "2026-09-13T12:00:00Z".into(),
                url: "https://example.test/issue-1".into(),
            }],
            reviews: vec![PullRequestReview {
                coordinates: coordinates(repository, number, "review-1"),
                author: Some("reviewer".into()),
                body: format!("Review discussion {marker}"),
                state: "APPROVED".into(),
                submitted_at: Some("2026-09-13T12:01:00Z".into()),
                commit_sha: Some("a".repeat(40)),
                edit_summary_capability: None,
                dismissal_capability: None,
                url: "https://example.test/review-1".into(),
            }],
            review_threads: vec![ReviewThread {
                coordinates: coordinates(repository, number, "thread-1"),
                path: "src/lib.rs".into(),
                subject: cibergit::domain::ReviewSubject::Line,
                line: Some(2),
                original_line: Some(2),
                start_line: Some(2),
                original_start_line: Some(2),
                side: Some("RIGHT".into()),
                start_side: Some("RIGHT".into()),
                resolved: false,
                outdated: false,
                comments: vec![ReviewComment {
                    coordinates: coordinates(repository, number, "comment-1"),
                    author: Some("reviewer".into()),
                    body: format!("Inline discussion {marker}"),
                    created_at: "2026-09-13T12:02:00Z".into(),
                    updated_at: "2026-09-13T12:02:00Z".into(),
                    url: "https://example.test/comment-1".into(),
                    path: "src/lib.rs".into(),
                    subject: cibergit::domain::ReviewSubject::Line,
                    line: Some(2),
                    original_line: Some(2),
                    start_line: Some(2),
                    original_start_line: Some(2),
                    side: Some("RIGHT".into()),
                    diff_hunk: "@@ -1,2 +1,2 @@".into(),
                    commit_sha: Some("a".repeat(40)),
                    original_commit_sha: Some("a".repeat(40)),
                    outdated: false,
                }],
                comments_complete: false,
            }],
            reactions: Vec::new(),
            checks: vec![PullRequestCheck {
                coordinates: coordinates(repository, number, "check-1"),
                kind: CheckKind::CheckRun,
                name: format!("Check {marker}"),
                status: "COMPLETED".into(),
                conclusion: Some("SUCCESS".into()),
                description: Some("cached check".into()),
                details_url: Some("https://example.test/check-1".into()),
                github_permalink: None,
                started_at: Some("2026-09-13T12:00:00Z".into()),
                completed_at: Some("2026-09-13T12:03:00Z".into()),
                required: Some(true),
                database_id: None,
                suite: None,
                commit_sha: None,
                commit_repository: None,
                sha_class: CheckShaClass::Unknown,
                actions_linkage: ActionsLinkage::Unknown,
            }],
            activity_complete: false,
            checks_complete: false,
            notice: Some("Original partial-evidence notice".into()),
        }
    }

    fn save(
        cache: &CollaborationCache,
        repository: &Repository,
        number: u64,
        value: &PullRequestDetails,
    ) {
        let prepared = cache.prepare_write(repository, number).unwrap();
        cache
            .save(prepared, value.clone(), now_unix_ms().unwrap())
            .unwrap();
    }

    fn private_write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn exact_payload_round_trip_preserves_partial_evidence_and_isolates_identity() {
        let root = tempfile::tempdir().unwrap();
        let cache = CollaborationCache::new(root.path().to_owned());
        let alice = repository("octo", "one", "alice");
        let bob = repository("octo", "one", "bob");
        let other_repo = repository("octo", "two", "alice");
        let alice_details = details(&alice, 7, "alice-one");
        let bob_details = details(&bob, 7, "bob-one");
        let other_details = details(&other_repo, 7, "alice-two");

        let alice_permit = cache.prepare_write(&alice, 7).unwrap();
        let bob_permit = cache.prepare_write(&bob, 7).unwrap();
        let other_permit = cache.prepare_write(&other_repo, 7).unwrap();
        let observed = now_unix_ms().unwrap();
        cache
            .save(alice_permit, alice_details.clone(), observed)
            .unwrap();
        cache
            .save(bob_permit, bob_details.clone(), observed)
            .unwrap();
        cache
            .save(other_permit, other_details.clone(), observed)
            .unwrap();

        assert_eq!(
            cache.load(&alice, 7).unwrap().unwrap().details,
            alice_details
        );
        assert_eq!(cache.load(&bob, 7).unwrap().unwrap().details, bob_details);
        assert_eq!(
            cache.load(&other_repo, 7).unwrap().unwrap().details,
            other_details
        );
        assert!(cache.load(&alice, 8).unwrap().is_none());
        let names = fs::read_dir(&cache.root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(names.iter().all(|name| {
            !name.contains("alice") && !name.contains("bob") && !name.contains("octo")
        }));
    }

    #[test]
    fn wrong_coordinates_are_never_written() {
        let root = tempfile::tempdir().unwrap();
        let cache = CollaborationCache::new(root.path().to_owned());
        let repo = repository("octo", "one", "alice");
        let mut wrong = details(&repo, 7, "wrong");
        wrong.checks[0].coordinates.repository = "another".into();
        let prepared = cache.prepare_write(&repo, 7).unwrap();
        assert!(cache.save(prepared, wrong, now_unix_ms().unwrap()).is_err());
        assert!(cache.load(&repo, 7).unwrap().is_none());
    }

    #[test]
    fn foreign_cached_pr_reaction_is_rejected_and_preserved() {
        let root = tempfile::tempdir().unwrap();
        let cache = CollaborationCache::new(root.path().to_owned());
        let repo = repository("octo", "one", "alice");
        let mut value = details(&repo, 7, "foreign-reaction");
        let pr = coordinates(&repo, 7, "PR_7");
        value.reactions.push(ReactionSubjectSnapshot {
            kind: ReactableKind::PullRequest,
            pull_request: pr.clone(),
            subject: pr,
            parent_review: None,
            content: "cached PR body".into(),
            reactions: ReactionSnapshot {
                groups: Vec::new(),
                complete: false,
            },
            fresh_capability: None,
        });
        save(&cache, &repo, 7, &value);
        let record_path = cache.root.join(CacheIdentity::new(&repo, 7).filename());
        let mut record: CacheRecord =
            serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
        record.details.reactions[0].pull_request.pull_request = 8;
        record.details.reactions[0].subject.pull_request = 8;
        let foreign = serde_json::to_vec(&record).unwrap();
        private_write(&record_path, &foreign);

        assert!(cache.load(&repo, 7).is_err());
        assert_eq!(fs::read(record_path).unwrap(), foreign);
    }

    #[test]
    fn foreign_nested_check_origin_is_rejected_and_preserved() {
        let root = tempfile::tempdir().unwrap();
        let cache = CollaborationCache::new(root.path().to_owned());
        let repo = repository("octo", "one", "alice");
        let base = CheckRepositoryIdentity {
            node_id: "R-base".into(),
            name_with_owner: "octo/one".into(),
        };
        let mut value = details(&repo, 7, "check-origin");
        value.pull_request_node_id = Some("PR-current".into());
        value.base_repository = Some(base.clone());
        value.observed_head_sha = Some("a".repeat(40));
        value.head_repository = Some(base.clone());
        value.rollup_commit_sha = Some("a".repeat(40));
        value.rollup_repository = Some(base.clone());
        value.checks[0].commit_sha = Some("a".repeat(40));
        value.checks[0].commit_repository = Some(base.clone());
        value.checks[0].sha_class = CheckShaClass::Head;
        value.checks[0].suite = Some(CheckSuiteIdentity {
            node_id: "SUITE-current".into(),
            database_id: Some(10),
            repository: base,
            app: None,
        });
        save(&cache, &repo, 7, &value);
        let record_path = cache.root.join(CacheIdentity::new(&repo, 7).filename());
        let mut record: CacheRecord =
            serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
        let foreign = CheckRepositoryIdentity {
            node_id: "R-foreign".into(),
            name_with_owner: "mallory/other".into(),
        };
        record.details.rollup_repository = Some(foreign.clone());
        record.details.checks[0].commit_repository = Some(foreign.clone());
        record.details.checks[0].suite.as_mut().unwrap().repository = foreign;
        let foreign_bytes = serde_json::to_vec(&record).unwrap();
        private_write(&record_path, &foreign_bytes);

        assert!(cache.load(&repo, 7).is_err());
        assert_eq!(fs::read(record_path).unwrap(), foreign_bytes);
    }

    #[test]
    fn later_reservation_wins_reverse_completion_and_blocks_absent_aba() {
        let root = tempfile::tempdir().unwrap();
        let cache = CollaborationCache::new(root.path().to_owned());
        let repo = repository("octo", "one", "alice");
        let older = cache.prepare_write(&repo, 7).unwrap();
        let newer = cache.prepare_write(&repo, 7).unwrap();
        let observed = now_unix_ms().unwrap();

        assert!(
            cache
                .save(older, details(&repo, 7, "older"), observed)
                .is_err()
        );
        cache
            .save(newer, details(&repo, 7, "newer"), observed)
            .unwrap();
        let record_path = cache.root.join(CacheIdentity::new(&repo, 7).filename());
        fs::remove_file(&record_path).unwrap();

        let stale_absent = PreparedWrite {
            identity: CacheIdentity::new(&repo, 7),
            identity_digest: CacheIdentity::new(&repo, 7).digest(),
            reservation: 1,
            predecessor: None,
        };
        assert!(
            cache
                .save(stale_absent, details(&repo, 7, "resurrected"), observed)
                .is_err()
        );
        assert!(!record_path.exists());
    }

    #[test]
    fn independent_identities_keep_associative_reservations() {
        let root = tempfile::tempdir().unwrap();
        let cache = CollaborationCache::new(root.path().to_owned());
        let first = repository("octo", "one", "alice");
        let second = repository("octo", "two", "alice");
        let first_permit = cache.prepare_write(&first, 7).unwrap();
        let second_permit = cache.prepare_write(&second, 7).unwrap();
        let observed = now_unix_ms().unwrap();
        cache
            .save(second_permit, details(&second, 7, "second"), observed)
            .unwrap();
        cache
            .save(first_permit, details(&first, 7, "first"), observed)
            .unwrap();
    }

    #[test]
    fn absent_control_in_existing_cache_fails_closed_and_preserves_payload() {
        let root = tempfile::tempdir().unwrap();
        let cache = CollaborationCache::new(root.path().to_owned());
        let repo = repository("octo", "one", "alice");
        save(&cache, &repo, 7, &details(&repo, 7, "existing"));
        let record_path = cache.root.join(CacheIdentity::new(&repo, 7).filename());
        let before = fs::read(&record_path).unwrap();
        fs::remove_file(cache.root.join(CONTROL_NAME)).unwrap();
        assert!(cache.prepare_write(&repo, 7).is_err());
        assert_eq!(fs::read(record_path).unwrap(), before);
    }

    #[test]
    fn corrupt_future_and_implausible_time_records_are_refused_and_preserved() {
        for defect in ["corrupt", "future-schema", "future-time"] {
            let root = tempfile::tempdir().unwrap();
            let cache = CollaborationCache::new(root.path().to_owned());
            let repo = repository("octo", "one", "alice");
            save(&cache, &repo, 7, &details(&repo, 7, defect));
            let path = cache.root.join(CacheIdentity::new(&repo, 7).filename());
            let mut bytes = fs::read(&path).unwrap();
            match defect {
                "corrupt" => bytes = b"{not-json".to_vec(),
                "future-schema" => {
                    let mut record: CacheRecord = serde_json::from_slice(&bytes).unwrap();
                    record.schema_version = SCHEMA_VERSION + 1;
                    bytes = serde_json::to_vec(&record).unwrap();
                }
                "future-time" => {
                    let mut record: CacheRecord = serde_json::from_slice(&bytes).unwrap();
                    record.observed_at_unix_ms = now_unix_ms().unwrap() + 10 * 60 * 1_000;
                    bytes = serde_json::to_vec(&record).unwrap();
                }
                _ => unreachable!(),
            }
            private_write(&path, &bytes);
            assert!(cache.load(&repo, 7).is_err());
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn oversize_symlink_and_hardlink_records_are_refused_and_preserved() {
        for defect in ["oversize", "symlink", "hardlink"] {
            let root = tempfile::tempdir().unwrap();
            let cache = CollaborationCache::new(root.path().to_owned());
            let repo = repository("octo", "one", "alice");
            let _permit = cache.prepare_write(&repo, 7).unwrap();
            let path = cache.root.join(CacheIdentity::new(&repo, 7).filename());
            match defect {
                "oversize" => private_write(&path, &vec![b'x'; MAX_RECORD_BYTES as usize + 1]),
                "symlink" => {
                    let target = root.path().join("outside");
                    private_write(&target, b"do not follow");
                    symlink(&target, &path).unwrap();
                }
                "hardlink" => {
                    let target = root.path().join("outside");
                    private_write(&target, b"do not unlink");
                    fs::hard_link(&target, &path).unwrap();
                }
                _ => unreachable!(),
            }
            let before = fs::symlink_metadata(&path).unwrap();
            assert!(cache.load(&repo, 7).is_err());
            let after = fs::symlink_metadata(&path).unwrap();
            assert!(same_identity(&before, &after));
            assert!(path.exists() || defect == "symlink");
        }
    }

    #[test]
    fn unsupported_subdirectory_is_preserved_and_blocks_unproved_retention() {
        let root = tempfile::tempdir().unwrap();
        let cache = CollaborationCache::new(root.path().to_owned());
        let repo = repository("octo", "one", "alice");
        let prepared = cache.prepare_write(&repo, 7).unwrap();
        let unknown = cache.root.join("unknown-directory");
        fs::create_dir(&unknown).unwrap();
        assert!(
            cache
                .save(
                    prepared,
                    details(&repo, 7, "blocked"),
                    now_unix_ms().unwrap()
                )
                .is_err()
        );
        assert!(unknown.is_dir());
    }

    #[test]
    fn retention_over_sixty_four_removes_only_valid_owned_records() {
        let root = tempfile::tempdir().unwrap();
        let cache = CollaborationCache::new(root.path().to_owned());
        let repo = repository("octo", "retention", "alice");
        let _initialize = cache.prepare_write(&repo, 1).unwrap();

        let corrupt_identity = CacheIdentity::new(&repo, 800);
        let corrupt_path = cache.root.join(corrupt_identity.filename());
        private_write(&corrupt_path, b"{corrupt");

        let future_identity = CacheIdentity::new(&repo, 801);
        let future_path = cache.root.join(future_identity.filename());
        let future = CacheRecord {
            schema_version: SCHEMA_VERSION + 1,
            identity: future_identity,
            observed_at_unix_ms: now_unix_ms().unwrap(),
            request_reservation: 1,
            details: details(&repo, 801, "future"),
        };
        private_write(&future_path, &serde_json::to_vec(&future).unwrap());

        let linked_identity = CacheIdentity::new(&repo, 802);
        let linked_path = cache.root.join(linked_identity.filename());
        let linked_source = root.path().join("linked-source");
        private_write(&linked_source, b"linked foreign fixture");
        fs::hard_link(&linked_source, &linked_path).unwrap();

        let unknown_path = cache.root.join("unknown-preserved.bin");
        private_write(&unknown_path, b"unknown foreign fixture");
        let preserved = [
            (&corrupt_path, fs::read(&corrupt_path).unwrap()),
            (&future_path, fs::read(&future_path).unwrap()),
            (&linked_path, fs::read(&linked_path).unwrap()),
            (&unknown_path, fs::read(&unknown_path).unwrap()),
        ];

        let base_time = now_unix_ms().unwrap().saturating_sub(10_000);
        for number in 1..=65 {
            let permit = cache.prepare_write(&repo, number).unwrap();
            cache
                .save(
                    permit,
                    details(&repo, number, &format!("owned-{number}")),
                    base_time + number,
                )
                .unwrap();
        }

        let valid_owned = fs::read_dir(&cache.root)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let metadata = fs::symlink_metadata(entry.path()).ok()?;
                read_any_record(&entry.path(), &metadata).ok().flatten()
            })
            .count();
        assert_eq!(valid_owned, MAX_OWNED_RECORDS);
        assert!(cache.load(&repo, 1).unwrap().is_none());
        assert_eq!(
            cache.load(&repo, 65).unwrap().unwrap().details.body,
            "Overview owned-65"
        );
        assert!(cache.root.join(CONTROL_NAME).is_file());
        assert!(cache.root.join(LOCK_NAME).is_file());
        for (path, bytes) in preserved {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        assert_eq!(fs::metadata(&linked_source).unwrap().nlink(), 2);
    }

    #[test]
    fn byte_and_entry_pressure_without_safe_victims_refuses_write() {
        for pressure in ["bytes", "entries"] {
            let root = tempfile::tempdir().unwrap();
            let cache = CollaborationCache::new(root.path().to_owned());
            let repo = repository("octo", pressure, "alice");
            let permit = cache.prepare_write(&repo, 7).unwrap();
            if pressure == "bytes" {
                let path = cache.root.join("unknown-large.bin");
                let file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .unwrap();
                file.set_len(MAX_ROOT_BYTES).unwrap();
                drop(file);
                assert!(
                    cache
                        .save(permit, details(&repo, 7, pressure), now_unix_ms().unwrap())
                        .is_err()
                );
                assert_eq!(fs::metadata(path).unwrap().len(), MAX_ROOT_BYTES);
            } else {
                let existing = fs::read_dir(&cache.root).unwrap().count();
                for index in existing..MAX_ROOT_ENTRIES {
                    private_write(&cache.root.join(format!("unknown-{index:03}")), b"preserve");
                }
                assert_eq!(fs::read_dir(&cache.root).unwrap().count(), MAX_ROOT_ENTRIES);
                assert!(
                    cache
                        .save(permit, details(&repo, 7, pressure), now_unix_ms().unwrap())
                        .is_err()
                );
                assert_eq!(fs::read_dir(&cache.root).unwrap().count(), MAX_ROOT_ENTRIES);
            }
        }
    }

    #[test]
    fn temp_name_collision_preserves_preexisting_bytes() {
        let root = tempfile::tempdir().unwrap();
        let cache = CollaborationCache::new(root.path().to_owned());
        let repo = repository("octo", "one", "alice");
        let _permit = cache.prepare_write(&repo, 7).unwrap();
        let name = ".tmp-collision";
        let path = cache.root.join(name);
        private_write(&path, b"preexisting crash artifact");
        assert!(
            cache
                .atomic_write_with_temp("probe", b"new", MAX_RECORD_BYTES, name)
                .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), b"preexisting crash artifact");
    }

    #[test]
    fn bounded_lock_contention_and_explicit_unlock() {
        let root = tempfile::tempdir().unwrap();
        let cache = CollaborationCache::new(root.path().to_owned());
        let repo = repository("octo", "one", "alice");
        let _first = cache.prepare_write(&repo, 7).unwrap();
        let lock = open_private_file(&cache.root.join(LOCK_NAME), false).unwrap();
        lock.lock().unwrap();
        let started = Instant::now();
        let error = cache.prepare_write(&repo, 7).unwrap_err();
        assert!(error.to_string().contains("contention"));
        assert!(started.elapsed() < Duration::from_secs(2));
        lock.unlock().unwrap();
        let _second = cache.prepare_write(&repo, 7).unwrap();

        let unlocked = open_private_file(&cache.root.join(LOCK_NAME), false).unwrap();
        unlocked.try_lock().unwrap();
        unlocked.unlock().unwrap();
    }

    #[test]
    fn process_death_releases_the_advisory_lock() {
        let root = tempfile::tempdir().unwrap();
        let cache = CollaborationCache::new(root.path().to_owned());
        let repo = repository("octo", "one", "alice");
        let _first = cache.prepare_write(&repo, 7).unwrap();
        let status = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "app::collaboration_cache::tests::process_death_lock_holder",
                "--nocapture",
            ])
            .env("CIBERGIT_CACHE_LOCK_CHILD", &cache.root)
            .status()
            .unwrap();
        assert!(status.success());
        let _after_death = cache.prepare_write(&repo, 7).unwrap();
    }

    #[test]
    fn process_death_lock_holder() {
        let Some(root) = env::var_os("CIBERGIT_CACHE_LOCK_CHILD") else {
            return;
        };
        let lock = open_private_file(&PathBuf::from(root).join(LOCK_NAME), false).unwrap();
        lock.lock().unwrap();
        std::process::exit(0);
    }
}
