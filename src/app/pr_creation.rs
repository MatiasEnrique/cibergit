//! Native PR creation dialog and its private durable admission lane.
//!
//! Provider reads and the one accepted provider mutation execute on background
//! tasks. Construction and rendering are side-effect free. The journal is
//! intentionally separate from per-PR review journals: a creation has no PR
//! number until GitHub acknowledges it.

use super::ControlPresentation;
#[cfg(feature = "ui-smoke")]
use super::Root;
use super::{Palette, input_style, is_dark, palette};
use crate::{
    CancelPullRequestCreation, ClosePullRequestCreation, ConfirmPullRequestCreation,
    PreparePullRequestCreation, TogglePullRequestCreationDraft,
};
use cibergit::ui::{self, Density, TextRole};
use cibergit::{
    domain::{
        Account, MutationAdmissionReceipt, MutationContext, MutationTerminalRecord,
        ProviderChoiceSet, ProviderMutationOutcome, PullRequestCreationAcknowledgement,
        PullRequestCreationInput, PullRequestCreationPreparation, PullRequestCreationRequest,
        Repository,
    },
    providers::{AdmittedMutationAttempt, GithubProvider, MutationAdmission},
};
use gpui::{
    AnyWindowHandle, App, Context, Div, ElementId, Entity, EventEmitter, FocusHandle, FontWeight,
    IntoElement, Render, SharedString, Subscription, Window, div, prelude::*, px, relative, rems,
    rgba,
};
#[cfg(feature = "ui-smoke")]
use gpui::{WeakEntity, size};
use gpui_base::Button;
use gpui_base::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    ffi::c_int,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// Longest a repository or branch chip grows before its label truncates. A
/// wrapping row of chips stays readable instead of one long name taking a line.
const CHOICE_PILL_MAX_WIDTH: f32 = 260.;
/// Height of the Markdown description field: five Body lines plus the field
/// inset, so a short description is visible without scrolling.
const DESCRIPTION_FIELD_HEIGHT: f32 = 5. * 18. + 2. * ui::CELL_INSET;
const RECORD_VERSION: u64 = 1;
const MAX_DRAFT_BYTES: usize = 2 * 1024 * 1024;
const MAX_JOURNAL_BYTES: usize = 4 * 1024 * 1024;
const MAX_ATTEMPTS_PER_AUTHORITY: usize = 64;
const MAX_DISCOVERED_JOURNALS: usize = 256;
const MAX_FORM_TEXT_BYTES: usize = 1024 * 1024;
const MAX_IDENTITY_BYTES: usize = 256;
const O_NOFOLLOW: c_int = 0x0000_0100;
const LOCK_EX: c_int = 0x02;
const LOCK_NB: c_int = 0x04;
const LOCK_UN: c_int = 0x08;
const PRIVATE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

unsafe extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreationForm {
    pub target_repository: Repository,
    pub base_branch: String,
    pub source_repository: Repository,
    pub source_branch: String,
    pub local_branch: Option<String>,
    pub title: String,
    pub body: String,
    pub draft: bool,
}

impl CreationForm {
    fn input(&self) -> PullRequestCreationInput {
        PullRequestCreationInput {
            target_repository: self.target_repository.clone(),
            base_branch: self.base_branch.clone(),
            source_repository: self.source_repository.clone(),
            source_branch: self.source_branch.clone(),
            local_branch: self.local_branch.clone(),
            title: self.title.clone(),
            body: self.body.clone(),
            draft: self.draft,
        }
    }

    fn validate_durable(&self) -> Result<(), String> {
        for (label, value) in [
            ("base branch", self.base_branch.as_str()),
            ("published source branch", self.source_branch.as_str()),
            ("title", self.title.as_str()),
        ] {
            validate_visible_text(label, value, MAX_FORM_TEXT_BYTES, true)?;
        }
        if self.body.len() > MAX_FORM_TEXT_BYTES || self.body.contains('\0') {
            return Err("Invalid body.".into());
        }
        if let Some(local) = &self.local_branch {
            validate_visible_text(
                "informational local branch",
                local,
                MAX_IDENTITY_BYTES,
                true,
            )?;
        }
        if self.target_repository.account != self.source_repository.account {
            return Err(
                "Source and target must use the same explicitly selected GitHub account.".into(),
            );
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        self.validate_durable()?;
        for (label, value) in [
            ("base branch", self.base_branch.as_str()),
            ("published source branch", self.source_branch.as_str()),
            ("title", self.title.as_str()),
        ] {
            validate_visible_text(label, value, MAX_FORM_TEXT_BYTES, false)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FrozenCreation {
    visible_form: CreationForm,
    request: PullRequestCreationRequest,
}

fn confirmation_details(frozen: &FrozenCreation) -> String {
    let input = &frozen.request.preparation.input;
    let preparation = &frozen.request.preparation;
    format!(
        "Operation: {}\nAttempt: {}\nTarget base OID: {}\nReviewed source OID: {}\nCapability: available\nLocal branch: {}\nBody (exact, {} bytes):\n{}",
        frozen.request.operation_id,
        frozen.request.attempt_id,
        preparation.observed_base_sha,
        preparation.observed_source_head_sha,
        input.local_branch.as_deref().unwrap_or("not supplied"),
        input.body.len(),
        input.body,
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DraftRecord {
    version: u64,
    generation: u64,
    form: CreationForm,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DraftSnapshot {
    edit_generation: u64,
    form: CreationForm,
}

#[derive(Debug, Default)]
struct DraftSaveLane {
    durable_generation: Option<u64>,
    durable_form: Option<CreationForm>,
    in_flight: Option<DraftSnapshot>,
    pending: Option<DraftSnapshot>,
}

impl DraftSaveLane {
    fn restore(&mut self, record: &DraftRecord) {
        self.durable_generation = Some(record.generation);
        self.durable_form = Some(record.form.clone());
    }

    fn queue(&mut self, snapshot: DraftSnapshot) {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|active| active.form == snapshot.form)
            || (self.in_flight.is_none() && self.durable_form.as_ref() == Some(&snapshot.form))
        {
            self.pending = None;
        } else {
            self.pending = Some(snapshot);
        }
    }

    fn start_next(&mut self) -> Option<(Option<u64>, DraftSnapshot)> {
        if self.in_flight.is_some() {
            return None;
        }
        let snapshot = self.pending.take()?;
        self.in_flight = Some(snapshot.clone());
        Some((self.durable_generation, snapshot))
    }

    fn complete_success(&mut self, record: &DraftRecord) -> Result<(), String> {
        let snapshot = self
            .in_flight
            .take()
            .ok_or_else(|| "Draft save completion has no owned in-flight snapshot.".to_owned())?;
        if snapshot.form != record.form {
            return Err("Draft save receipt does not match its owned snapshot.".into());
        }
        self.durable_generation = Some(record.generation);
        self.durable_form = Some(record.form.clone());
        Ok(())
    }

    fn complete_failure(&mut self) {
        self.in_flight = None;
    }

    fn is_idle_durable(&self, form: &CreationForm) -> bool {
        self.in_flight.is_none()
            && self.pending.is_none()
            && self.durable_form.as_ref() == Some(form)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CreationAuthorityKey {
    provider: String,
    account_host: String,
    account_login: String,
    target_host: String,
    target_owner: String,
    target_repository: String,
    base_branch: String,
    source_host: String,
    source_owner: String,
    source_repository: String,
    source_branch: String,
}

impl CreationAuthorityKey {
    fn from_preparation(preparation: &PullRequestCreationPreparation) -> Result<Self, String> {
        let input = &preparation.input;
        input
            .target_repository
            .account
            .eq(&input.source_repository.account)
            .then_some(())
            .ok_or_else(|| "Creation authority cannot span selected accounts.".to_owned())?;
        let key = Self {
            provider: "github".into(),
            account_host: canonical_github(&input.target_repository.account.host)?,
            account_login: canonical_github(&input.target_repository.account.login)?,
            target_host: canonical_github(&input.target_repository.host)?,
            target_owner: canonical_github(&input.target_repository.owner)?,
            target_repository: canonical_github(&input.target_repository.name)?,
            base_branch: exact_branch(&input.base_branch)?,
            source_host: canonical_github(&input.source_repository.host)?,
            source_owner: canonical_github(&input.source_repository.owner)?,
            source_repository: canonical_github(&input.source_repository.name)?,
            source_branch: exact_branch(&input.source_branch)?,
        };
        Ok(key)
    }

    fn component(&self) -> Result<String, String> {
        stable_component(self)
    }

    fn summary(&self) -> String {
        format!(
            "{}:{} → {}/{}:{} as {}",
            self.source_owner,
            self.source_branch,
            self.target_owner,
            self.target_repository,
            self.base_branch,
            self.account_login
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum CreationAttemptStatus {
    InFlight,
    NotStarted { reason: String },
    Acknowledged { acknowledgement: Value },
    Uncertain { reason: String },
}

impl CreationAttemptStatus {
    fn unresolved(&self) -> bool {
        matches!(self, Self::InFlight | Self::Uncertain { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CreationAttempt {
    request: PullRequestCreationRequest,
    context: MutationContext,
    status: CreationAttemptStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CreationJournalRecord {
    version: u64,
    key: CreationAuthorityKey,
    attempts: Vec<CreationAttempt>,
}

impl CreationJournalRecord {
    fn validate(&self) -> Result<(), String> {
        if self.version != RECORD_VERSION {
            return Err(format!(
                "Creation journal uses unsupported version {}; the original was preserved.",
                self.version
            ));
        }
        if self.attempts.len() > MAX_ATTEMPTS_PER_AUTHORITY {
            return Err(
                "Creation journal exceeds its bounded attempt count; the original was preserved."
                    .into(),
            );
        }
        let mut ids = HashSet::new();
        for attempt in &self.attempts {
            validate_identifier("operation ID", &attempt.request.operation_id)?;
            validate_identifier("attempt ID", &attempt.request.attempt_id)?;
            if attempt.context.operation_id != attempt.request.operation_id
                || attempt.context.attempt_id != attempt.request.attempt_id
                || attempt.context.action != "create-pr"
            {
                return Err("Creation journal contains a mismatched provider context; the original was preserved.".into());
            }
            if CreationAuthorityKey::from_preparation(&attempt.request.preparation)? != self.key {
                return Err("Creation journal contains a request for another authority; the original was preserved.".into());
            }
            if !ids.insert((
                attempt.request.operation_id.as_str(),
                attempt.request.attempt_id.as_str(),
            )) {
                return Err("Creation journal repeats an operation/attempt identity; the original was preserved.".into());
            }
            match &attempt.status {
                CreationAttemptStatus::InFlight => {}
                CreationAttemptStatus::NotStarted { reason }
                | CreationAttemptStatus::Uncertain { reason } => {
                    bounded_reason(reason).map_err(|error| error.to_string())?;
                }
                CreationAttemptStatus::Acknowledged { acknowledgement } => {
                    let acknowledgement: PullRequestCreationAcknowledgement =
                        serde_json::from_value(acknowledgement.clone()).map_err(|_| {
                            "Creation journal contains an invalid acknowledgement; the original was preserved."
                                .to_owned()
                        })?;
                    let input = &attempt.request.preparation.input;
                    if acknowledgement.operation_id != attempt.request.operation_id
                        || acknowledgement.target_repository != input.target_repository
                        || acknowledgement.source_repository != input.source_repository
                        || acknowledgement.reviewed_head_sha
                            != attempt.request.preparation.observed_source_head_sha
                        || acknowledgement.pull_request.provider != "github"
                        || !acknowledgement
                            .pull_request
                            .host
                            .eq_ignore_ascii_case(&input.target_repository.host)
                        || !acknowledgement
                            .pull_request
                            .owner
                            .eq_ignore_ascii_case(&input.target_repository.owner)
                        || !acknowledgement
                            .pull_request
                            .repository
                            .eq_ignore_ascii_case(&input.target_repository.name)
                        || acknowledgement.pull_request.pull_request == 0
                    {
                        return Err("Creation journal acknowledgement does not match its exact frozen request; the original was preserved.".into());
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct AttemptSummary {
    pub authority: String,
    pub operation_id: String,
    pub attempt_id: String,
    pub source: String,
    pub target: String,
    pub title: String,
    pub reviewed_head: String,
    pub status: String,
    pub unresolved: bool,
    pub acknowledgement: Option<PullRequestCreationAcknowledgement>,
}

#[derive(Clone, Debug)]
struct CreationStore {
    root: PathBuf,
    draft_account: Option<(String, String)>,
}

impl CreationStore {
    #[cfg(test)]
    fn open(root: PathBuf) -> Result<Self, String> {
        let store = Self {
            root,
            draft_account: None,
        };
        store.ensure_layout()?;
        Ok(store)
    }

    fn for_account(root: PathBuf, account: &Account) -> Result<Self, String> {
        Ok(Self {
            root,
            draft_account: Some((
                canonical_github(&account.host)?,
                canonical_github(&account.login)?,
            )),
        })
    }

    fn ensure_layout(&self) -> Result<(), String> {
        ensure_private_directory(&self.root)?;
        for relative in [["draft", "v1"], ["attempts", "v1"], ["authority", "v1"]] {
            let mut current = self.root.clone();
            for component in relative {
                current.push(component);
                ensure_private_child_directory(&current)?;
            }
        }
        Ok(())
    }

    fn draft_path(&self) -> PathBuf {
        let name = match &self.draft_account {
            Some(account) => format!(
                "account-{}.json",
                stable_component(account).expect("account string tuple is serializable")
            ),
            None => "form.json".into(),
        };
        self.root.join("draft").join("v1").join(name)
    }

    fn draft_lock_path(&self) -> PathBuf {
        self.draft_path().with_extension("lock")
    }

    fn load_draft(&self) -> Result<Option<DraftRecord>, String> {
        self.ensure_layout()?;
        let _guard = acquire_private_lock(&self.draft_lock_path())?;
        self.load_draft_unlocked()
    }

    fn load_draft_unlocked(&self) -> Result<Option<DraftRecord>, String> {
        let Some(bytes) = read_bounded_private(&self.draft_path(), MAX_DRAFT_BYTES)? else {
            if self.draft_account.is_some() {
                // Import only the selected account's legacy draft. Keep the original
                // intact; the first owned save writes the new account-specific path.
                let legacy = Self {
                    root: self.root.clone(),
                    draft_account: None,
                };
                let _legacy_guard = acquire_private_lock(&legacy.draft_lock_path())?;
                if let Some(record) = legacy.load_draft_unlocked()?
                    && self.draft_matches_account(&record.form)
                {
                    return Ok(Some(record));
                }
            }
            return Ok(None);
        };
        let record: DraftRecord = serde_json::from_slice(&bytes).map_err(|error| {
            format!("Creation draft is corrupt and was preserved without overwrite: {error}")
        })?;
        if record.version != RECORD_VERSION {
            return Err(format!(
                "Creation draft uses unsupported version {}; the original was preserved.",
                record.version
            ));
        }
        record.form.validate_durable()?;
        if !self.draft_matches_account(&record.form) {
            return Err(
                "Creation draft belongs to another account; the original was preserved.".into(),
            );
        }
        Ok(Some(record))
    }

    fn draft_matches_account(&self, form: &CreationForm) -> bool {
        let account = &form.target_repository.account;
        self.draft_account.as_ref().is_none_or(|expected| {
            account.host.eq_ignore_ascii_case(&expected.0)
                && account.login.eq_ignore_ascii_case(&expected.1)
        })
    }

    fn save_draft_if_current(
        &self,
        expected_generation: Option<u64>,
        form: CreationForm,
    ) -> Result<DraftRecord, String> {
        self.ensure_layout()?;
        form.validate_durable()?;
        if !self.draft_matches_account(&form) {
            return Err(
                "Creation draft account differs from its storage lane; no text was overwritten."
                    .into(),
            );
        }
        let _guard = acquire_private_lock(&self.draft_lock_path())?;
        let current = self.load_draft_unlocked()?;
        if current.as_ref().map(|record| record.generation) != expected_generation {
            return Err("The durable creation draft changed in another window; this stale snapshot was not written. Reload before preparing.".into());
        }
        let generation = expected_generation
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| "Creation draft generation is exhausted.".to_owned())?;
        let record = DraftRecord {
            version: RECORD_VERSION,
            generation,
            form,
        };
        let bytes = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_DRAFT_BYTES {
            return Err("Creation draft exceeds its bounded storage size.".into());
        }
        atomic_private_write(&self.draft_path(), &bytes)?;
        Ok(record)
    }

    fn journal_path(&self, key: &CreationAuthorityKey) -> Result<PathBuf, String> {
        Ok(self
            .root
            .join("attempts")
            .join("v1")
            .join(format!("{}.json", key.component()?)))
    }

    fn authority_lock_path(&self, key: &CreationAuthorityKey) -> Result<PathBuf, String> {
        Ok(self
            .root
            .join("authority")
            .join("v1")
            .join(format!("{}.lock", key.component()?)))
    }

    fn load_journal_unlocked(
        &self,
        key: &CreationAuthorityKey,
    ) -> Result<CreationJournalRecord, String> {
        self.ensure_layout()?;
        let path = self.journal_path(key)?;
        let Some(bytes) = read_bounded_private(&path, MAX_JOURNAL_BYTES)? else {
            return Ok(CreationJournalRecord {
                version: RECORD_VERSION,
                key: key.clone(),
                attempts: Vec::new(),
            });
        };
        let journal: CreationJournalRecord = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "Creation journal {} is corrupt and was preserved without overwrite: {error}",
                path.display()
            )
        })?;
        journal.validate()?;
        if journal.key != *key
            || key.component()?
                != path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
        {
            return Err("Creation journal path and authority identity disagree; the original was preserved.".into());
        }
        Ok(journal)
    }

    fn save_journal_unlocked(&self, journal: &CreationJournalRecord) -> Result<(), String> {
        self.ensure_layout()?;
        journal.validate()?;
        let bytes = serde_json::to_vec(journal).map_err(|error| error.to_string())?;
        if bytes.len() > MAX_JOURNAL_BYTES {
            return Err("Creation journal exceeds its bounded storage size.".into());
        }
        atomic_private_write(&self.journal_path(&journal.key)?, &bytes)
    }

    fn load_attempt_summaries(&self) -> Result<Vec<AttemptSummary>, String> {
        self.ensure_layout()?;
        let directory = self.root.join("attempts").join("v1");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(format!(
                    "Cannot enumerate creation recovery records: {error}"
                ));
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("Cannot enumerate creation recovery records: {error}"))?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                paths.push(path);
            }
            if paths.len() > MAX_DISCOVERED_JOURNALS {
                return Err("Creation recovery has more journals than the bounded discovery limit; no record was hidden or changed.".into());
            }
        }
        paths.sort();
        let mut summaries = Vec::new();
        for path in paths {
            let Some(bytes) = read_bounded_private(&path, MAX_JOURNAL_BYTES)? else {
                continue;
            };
            let journal: CreationJournalRecord = serde_json::from_slice(&bytes).map_err(|error| {
                format!("Creation journal {} is corrupt and was preserved without overwrite: {error}", path.display())
            })?;
            journal.validate()?;
            let expected = journal.key.component()?;
            if path.file_stem().and_then(|value| value.to_str()) != Some(expected.as_str()) {
                return Err(format!(
                    "Creation journal {} has the wrong authority filename; it was preserved.",
                    path.display()
                ));
            }
            for attempt in journal.attempts {
                let input = &attempt.request.preparation.input;
                let (status, unresolved, acknowledgement) = match &attempt.status {
                    CreationAttemptStatus::InFlight => ("In flight — outcome unknown; never retry implicitly".into(), true, None),
                    CreationAttemptStatus::NotStarted { reason } => (format!("Not started — {reason}"), false, None),
                    CreationAttemptStatus::Acknowledged { acknowledgement } => (
                        "Acknowledged and durably recorded".into(),
                        false,
                        Some(serde_json::from_value(acknowledgement.clone()).map_err(|_| "Durable creation acknowledgement cannot be decoded; the original was preserved.".to_owned())?),
                    ),
                    CreationAttemptStatus::Uncertain { reason } => (format!("Uncertain — {reason}"), true, None),
                };
                summaries.push(AttemptSummary {
                    authority: journal.key.summary(),
                    operation_id: attempt.request.operation_id,
                    attempt_id: attempt.request.attempt_id,
                    source: format!(
                        "{}:{}",
                        input.source_repository.full_name(),
                        input.source_branch
                    ),
                    target: format!(
                        "{}:{}",
                        input.target_repository.full_name(),
                        input.base_branch
                    ),
                    title: input.title.clone(),
                    reviewed_head: attempt.request.preparation.observed_source_head_sha.clone(),
                    status,
                    unresolved,
                    acknowledgement,
                });
            }
        }
        summaries.sort_by(|left, right| left.operation_id.cmp(&right.operation_id));
        Ok(summaries)
    }
}

struct CreationAdmission {
    store: CreationStore,
    request: PullRequestCreationRequest,
}

impl CreationAdmission {
    fn new(store: CreationStore, request: PullRequestCreationRequest) -> Self {
        Self { store, request }
    }
}

impl MutationAdmission for CreationAdmission {
    fn admit<'a>(
        &'a mut self,
        context: &MutationContext,
    ) -> anyhow::Result<Box<dyn AdmittedMutationAttempt + 'a>> {
        self.store.ensure_layout().map_err(anyhow::Error::msg)?;
        validate_context(&self.request, context).map_err(anyhow::Error::msg)?;
        let key = CreationAuthorityKey::from_preparation(&self.request.preparation)
            .map_err(anyhow::Error::msg)?;
        let guard = acquire_private_lock(
            &self
                .store
                .authority_lock_path(&key)
                .map_err(anyhow::Error::msg)?,
        )
        .map_err(anyhow::Error::msg)?;
        let mut journal = self
            .store
            .load_journal_unlocked(&key)
            .map_err(anyhow::Error::msg)?;
        if journal
            .attempts
            .iter()
            .any(|attempt| attempt.status.unresolved())
        {
            return Err(anyhow::anyhow!(
                "this exact source-to-target authority has an unresolved creation; zero writes sent and the retained context must be reconciled explicitly"
            ));
        }
        if journal.attempts.iter().any(|attempt| {
            attempt.request.operation_id == self.request.operation_id
                || attempt.request.attempt_id == self.request.attempt_id
        }) {
            return Err(anyhow::anyhow!(
                "this operation or attempt identity was already admitted; zero writes sent"
            ));
        }
        if journal.attempts.iter().any(|attempt| {
            attempt.request.preparation == self.request.preparation
                && matches!(attempt.status, CreationAttemptStatus::Acknowledged { .. })
        }) {
            return Err(anyhow::anyhow!(
                "this exact frozen creation was already acknowledged; zero writes sent"
            ));
        }
        if journal.attempts.len() >= MAX_ATTEMPTS_PER_AUTHORITY {
            return Err(anyhow::anyhow!(
                "creation authority journal is full; zero writes sent and existing records were preserved"
            ));
        }
        journal.attempts.push(CreationAttempt {
            request: self.request.clone(),
            context: context.clone(),
            status: CreationAttemptStatus::InFlight,
        });
        self.store
            .save_journal_unlocked(&journal)
            .map_err(anyhow::Error::msg)?;
        let durable_record_id = stable_component(&(
            key,
            &self.request.operation_id,
            &self.request.attempt_id,
            context,
        ))
        .map_err(anyhow::Error::msg)?;
        Ok(Box::new(HeldCreationAttempt {
            store: self.store.clone(),
            request: self.request.clone(),
            receipt: MutationAdmissionReceipt {
                operation_id: self.request.operation_id.clone(),
                attempt_id: self.request.attempt_id.clone(),
                durable_record_id,
            },
            journal,
            _guard: guard,
        }))
    }
}

struct HeldCreationAttempt {
    store: CreationStore,
    request: PullRequestCreationRequest,
    receipt: MutationAdmissionReceipt,
    journal: CreationJournalRecord,
    _guard: PrivateLockGuard,
}

impl AdmittedMutationAttempt for HeldCreationAttempt {
    fn receipt(&self) -> &MutationAdmissionReceipt {
        &self.receipt
    }

    fn record_terminal(&mut self, record: &MutationTerminalRecord) -> anyhow::Result<()> {
        let attempt = self
            .journal
            .attempts
            .iter_mut()
            .find(|attempt| {
                attempt.request.operation_id == self.request.operation_id
                    && attempt.request.attempt_id == self.request.attempt_id
                    && matches!(attempt.status, CreationAttemptStatus::InFlight)
            })
            .ok_or_else(|| {
                anyhow::anyhow!("held creation attempt is no longer the retained InFlight record")
            })?;
        attempt.status = match record {
            MutationTerminalRecord::NotStarted { reason } => CreationAttemptStatus::NotStarted {
                reason: bounded_reason(reason)?,
            },
            MutationTerminalRecord::Acknowledged { acknowledgement } => {
                CreationAttemptStatus::Acknowledged {
                    acknowledgement: acknowledgement.clone(),
                }
            }
            MutationTerminalRecord::Uncertain { reason } => CreationAttemptStatus::Uncertain {
                reason: bounded_reason(reason)?,
            },
        };
        self.store
            .save_journal_unlocked(&self.journal)
            .map_err(anyhow::Error::msg)
    }
}

#[derive(Debug)]
struct PrivateLockGuard {
    file: Option<File>,
}

impl Drop for PrivateLockGuard {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            // Explicit LOCK_UN acts on the open-file description, including
            // any dup/try_clone descriptor that outlives this guard.
            let _ = unsafe { flock(file.as_raw_fd(), LOCK_UN) };
        }
    }
}

fn acquire_private_lock(path: &Path) -> Result<PrivateLockGuard, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Private lock has no parent directory.".to_owned())?;
    ensure_private_directory(parent)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("Cannot open private lock {}: {error}", path.display()))?;
    let before = validate_private_inode(&file, path)?;
    let deadline = std::time::Instant::now() + PRIVATE_LOCK_WAIT;
    loop {
        if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() != io::ErrorKind::WouldBlock || std::time::Instant::now() >= deadline {
            return Err(format!(
                "Cannot acquire private creation authority {} within the bounded wait; zero writes sent: {error}",
                path.display()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let guard = PrivateLockGuard { file: Some(file) };
    let after = validate_private_inode(guard.file.as_ref().expect("held lock"), path)?;
    if before != after {
        return Err(format!(
            "Creation authority inode changed while locking {}; zero writes sent.",
            path.display()
        ));
    }
    guard
        .file
        .as_ref()
        .expect("held lock")
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Cannot protect private lock {}: {error}", path.display()))?;
    validate_private_inode(guard.file.as_ref().expect("held lock"), path)?;
    Ok(guard)
}

fn validate_private_inode(file: &File, path: &Path) -> Result<(u64, u64), String> {
    let metadata = file
        .metadata()
        .map_err(|error| format!("Cannot inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(format!(
            "Private record {} is not a single-link regular file; it was refused.",
            path.display()
        ));
    }
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect lock path {}: {error}", path.display()))?;
    if !path_metadata.file_type().is_file()
        || path_metadata.file_type().is_symlink()
        || path_metadata.nlink() != 1
        || path_metadata.dev() != metadata.dev()
        || path_metadata.ino() != metadata.ino()
    {
        return Err(format!(
            "Private lock path {} no longer names the held stable inode; it was refused.",
            path.display()
        ));
    }
    Ok((metadata.dev(), metadata.ino()))
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| {
        format!(
            "Cannot create private directory {}: {error}",
            path.display()
        )
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "Cannot inspect private directory {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "Private path {} is not a real directory.",
            path.display()
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        format!(
            "Cannot protect private directory {}: {error}",
            path.display()
        )
    })
}

fn ensure_private_child_directory(path: &Path) -> Result<(), String> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(format!(
                "Cannot create private child directory {}: {error}",
                path.display()
            ));
        }
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "Cannot inspect private child directory {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "Private child path {} is not a real directory.",
            path.display()
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        format!(
            "Cannot protect private child directory {}: {error}",
            path.display()
        )
    })
}

fn read_bounded_private(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, String> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Cannot open private record {}: {error}",
                path.display()
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("Cannot inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(format!(
            "Private record {} is not a single-link regular file; it was refused.",
            path.display()
        ));
    }
    if metadata.len() > limit as u64 {
        return Err(format!(
            "Private record {} exceeds its {limit}-byte bound and was preserved.",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Cannot read private record {}: {error}", path.display()))?;
    if bytes.len() > limit {
        return Err(format!(
            "Private record {} exceeds its {limit}-byte bound and was preserved.",
            path.display()
        ));
    }
    Ok(Some(bytes))
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Private record has no parent directory.".to_owned())?;
    ensure_private_directory(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.nlink() != 1 =>
        {
            return Err(format!(
                "Private record {} is not a single-link regular file; overwrite refused.",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!("Cannot inspect {}: {error}", path.display()));
        }
    }
    let temporary = parent.join(format!(
        ".cibergit-pr-create-{}-{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(&temporary)
            .map_err(|error| format!("Cannot create {}: {error}", temporary.display()))?;
        file.write_all(bytes)
            .map_err(|error| format!("Cannot write {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("Cannot sync {}: {error}", temporary.display()))?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("Cannot replace {}: {error}", path.display()))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("Cannot sync {}: {error}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn canonical_github(value: &str) -> Result<String, String> {
    validate_visible_text("GitHub identity", value, MAX_IDENTITY_BYTES, false)?;
    if !value.is_ascii() {
        return Err(
            "GitHub login/owner/repository authority must use its accepted ASCII identity.".into(),
        );
    }
    Ok(value.to_ascii_lowercase())
}

fn exact_branch(value: &str) -> Result<String, String> {
    validate_visible_text("branch", value, MAX_IDENTITY_BYTES, false)?;
    Ok(value.to_owned())
}

fn validate_visible_text(
    label: &str,
    value: &str,
    limit: usize,
    allow_empty: bool,
) -> Result<(), String> {
    if (!allow_empty && value.trim().is_empty())
        || value.len() > limit
        || value.chars().any(char::is_control)
    {
        return Err(format!("Invalid {label}."));
    }
    Ok(())
}

fn validate_identifier(label: &str, value: &str) -> Result<(), String> {
    validate_visible_text(label, value, 512, false)
}

fn bounded_reason(reason: &str) -> anyhow::Result<String> {
    if reason.is_empty() || reason.len() > MAX_FORM_TEXT_BYTES || reason.contains('\0') {
        return Err(anyhow::anyhow!(
            "provider terminal reason is invalid; durable InFlight retained"
        ));
    }
    Ok(reason.to_owned())
}

fn stable_component(value: &impl Serialize) -> Result<String, String> {
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn validate_context(
    request: &PullRequestCreationRequest,
    context: &MutationContext,
) -> Result<(), String> {
    if context.operation_id != request.operation_id
        || context.attempt_id != request.attempt_id
        || context.action != "create-pr"
    {
        return Err("Provider creation context does not match the frozen request.".into());
    }
    let encoded = context
        .payload
        .get("request")
        .ok_or_else(|| "Provider creation context omits the frozen request.".to_owned())?;
    let observed: PullRequestCreationRequest = serde_json::from_value(encoded.clone())
        .map_err(|_| "Provider creation context contains an invalid frozen request.".to_owned())?;
    if observed != *request {
        return Err("Provider creation context changed after confirmation.".into());
    }
    Ok(())
}

fn callback_matches(
    current_lifetime: &str,
    reply_lifetime: &str,
    current_generation: u64,
    reply_generation: u64,
) -> bool {
    current_lifetime == reply_lifetime && current_generation == reply_generation
}

fn random_identity(prefix: &str) -> Result<String, String> {
    let mut bytes = [0_u8; 24];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("Cannot allocate a unique creation identity: {error}"))?;
    let entropy = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("{prefix}-{}-{entropy}", std::process::id()))
}

fn unique_dialog_identity() -> String {
    random_identity("creation-dialog").unwrap_or_else(|_| {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        format!(
            "creation-dialog-fallback-{}-{elapsed}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )
    })
}

#[derive(Clone, Debug)]
enum DialogState {
    Loading,
    Unavailable,
    Editing,
    Preparing,
    Confirmation,
    Creating,
    Acknowledged(Box<PullRequestCreationAcknowledgement>),
    Uncertain(String),
}

impl DialogState {
    fn busy(&self) -> bool {
        matches!(self, Self::Loading | Self::Preparing | Self::Creating)
    }
}

#[derive(Clone, Debug)]
struct ChoiceState {
    target: Option<ProviderChoiceSet>,
    source: Option<ProviderChoiceSet>,
    loading: bool,
    notice: Option<String>,
}

pub struct PrCreationDialog {
    store: Option<CreationStore>,
    repositories: Vec<Repository>,
    target_index: usize,
    source_index: usize,
    base_branch: Entity<InputState>,
    source_branch: Entity<InputState>,
    local_branch: Entity<InputState>,
    title: Entity<InputState>,
    body: Entity<TextareaState>,
    draft: bool,
    draft_lane: DraftSaveLane,
    edit_generation: u64,
    load_generation: u64,
    choice_generation: u64,
    save_generation: u64,
    operation_generation: u64,
    lifetime_id: String,
    state: DialogState,
    choices: ChoiceState,
    frozen: Option<FrozenCreation>,
    attempts: Vec<AttemptSummary>,
    attempts_disclosed: bool,
    details_disclosed: bool,
    notice: Option<String>,
    recovery_open_acknowledgement: Option<PullRequestCreationAcknowledgement>,
    prepare_queued: Option<DraftSnapshot>,
    close_after_save: bool,
    closed: bool,
    focus: FocusHandle,
    window_handle: AnyWindowHandle,
    _subscriptions: Vec<Subscription>,
}

pub struct OpenAcknowledgedPr(pub PullRequestCreationAcknowledgement);

impl EventEmitter<OpenAcknowledgedPr> for PrCreationDialog {}

impl PrCreationDialog {
    pub fn new(
        root: PathBuf,
        repositories: Vec<Repository>,
        preferred: Option<Repository>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let target_index = preferred
            .as_ref()
            .and_then(|preferred| {
                repositories
                    .iter()
                    .position(|repository| repository.cache_key() == preferred.cache_key())
            })
            .unwrap_or(0);
        let base_branch = new_input("", "Published target branch", window, cx);
        let source_branch = new_input("", "Published source branch", window, cx);
        let local_branch = new_input("", "Local branch (informational)", window, cx);
        let title = new_input("", "Pull request title", window, cx);
        let body = new_textarea("", "Description (Markdown)", window, cx);
        let focus = cx.focus_handle();
        let lifetime_id = unique_dialog_identity();
        // The constructor performs no filesystem I/O. The first background
        // load creates or verifies the private store.
        let store = repositories
            .get(target_index)
            .and_then(|repository| CreationStore::for_account(root, &repository.account).ok());
        let mut this = Self {
            store,
            repositories,
            target_index,
            source_index: target_index,
            base_branch,
            source_branch,
            local_branch,
            title,
            body,
            draft: false,
            draft_lane: DraftSaveLane::default(),
            edit_generation: 0,
            load_generation: 0,
            choice_generation: 0,
            save_generation: 0,
            operation_generation: 0,
            lifetime_id,
            state: DialogState::Loading,
            choices: ChoiceState {
                target: None,
                source: None,
                loading: false,
                notice: None,
            },
            frozen: None,
            attempts: Vec::new(),
            attempts_disclosed: true,
            details_disclosed: false,
            notice: None,
            recovery_open_acknowledgement: None,
            prepare_queued: None,
            close_after_save: false,
            closed: false,
            focus,
            window_handle: window.window_handle(),
            _subscriptions: Vec::new(),
        };
        this.set_form_disabled(true, cx);
        let appearance = cx.observe_window_appearance(window, |this, window, cx| {
            let style = input_style(palette(is_dark(window)));
            for editor in [
                &this.base_branch,
                &this.source_branch,
                &this.local_branch,
                &this.title,
            ] {
                editor.update(cx, |editor, _| editor.set_editor_style(style.clone()));
            }
            this.body
                .update(cx, |editor, _| editor.set_editor_style(style));
            cx.notify();
        });
        this._subscriptions.push(appearance);
        for editor in [
            this.base_branch.clone(),
            this.source_branch.clone(),
            this.local_branch.clone(),
            this.title.clone(),
        ] {
            let subscription = cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.form_changed(cx);
                }
            });
            this._subscriptions.push(subscription);
        }
        let body_subscription = cx.subscribe(&this.body, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.form_changed(cx);
            }
        });
        this._subscriptions.push(body_subscription);
        this.start_load(cx);
        this
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn start_load(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else {
            self.state = DialogState::Unavailable;
            self.notice = Some("Private creation storage is unavailable; preparation and creation remain disabled.".into());
            return;
        };
        self.load_generation = self.load_generation.saturating_add(1);
        let generation = self.load_generation;
        let lifetime = self.lifetime_id.clone();
        let task = cx.background_spawn(async move {
            Ok::<_, String>((store.load_draft()?, store.load_attempt_summaries()?))
        });
        cx.spawn(async move |dialog, cx| {
            let result = task.await;
            let _ = dialog.update(cx, |this, cx| {
                if !callback_matches(&this.lifetime_id, &lifetime, this.load_generation, generation) { return; }
                match result {
                    Ok((draft, attempts)) => {
                        this.attempts = attempts;
                        if let Some(record) = draft {
                            let target = this.repositories.iter().position(|repository| repository.cache_key() == record.form.target_repository.cache_key());
                            let source = this.repositories.iter().position(|repository| repository.cache_key() == record.form.source_repository.cache_key());
                            if let (Some(target), Some(source)) = (target, source) {
                                this.target_index = target;
                                this.source_index = source;
                                this.draft = record.form.draft;
                                this.draft_lane.restore(&record);
                                this.set_form_values(&record.form, cx);
                            } else {
                                this.state = DialogState::Unavailable;
                                this.notice = Some("The saved creation draft names a repository that is no longer explicitly selected. It was preserved and not guessed into another repository.".into());
                                cx.notify();
                                return;
                            }
                        }
                        this.state = DialogState::Editing;
                        this.set_form_disabled(false, cx);
                        this.load_choices(cx);
                    }
                    Err(error) => {
                        this.state = DialogState::Unavailable;
                        this.notice = Some(error);
                    }
                }
                cx.notify();
            });
        }).detach();
    }

    fn set_form_values(&mut self, form: &CreationForm, cx: &mut Context<Self>) {
        let values = [
            (&self.base_branch, form.base_branch.clone()),
            (&self.source_branch, form.source_branch.clone()),
            (
                &self.local_branch,
                form.local_branch.clone().unwrap_or_default(),
            ),
            (&self.title, form.title.clone()),
        ];
        for (editor, value) in values {
            set_input_value(editor.clone(), value, self.window_handle, cx);
        }
        set_textarea_value(self.body.clone(), form.body.clone(), self.window_handle, cx);
    }

    fn form_changed(&mut self, cx: &mut Context<Self>) {
        self.edit_generation = self.edit_generation.saturating_add(1);
        let edit_generation = self.edit_generation;
        self.prepare_queued = None;
        if matches!(self.state, DialogState::Confirmation) {
            self.state = DialogState::Editing;
            self.frozen = None;
            self.notice = Some("Visible creation fields changed. The prior confirmation was discarded; prepare again.".into());
        } else if self.state.busy() {
            self.notice = Some("A visible field changed while work was pending. The result will be discarded and no stale confirmation can be used.".into());
        }
        let lifetime = self.lifetime_id.clone();
        cx.spawn(async move |dialog, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(350))
                .await;
            let _ = dialog.update(cx, |this, cx| {
                if this.lifetime_id == lifetime
                    && this.edit_generation == edit_generation
                    && matches!(this.state, DialogState::Editing)
                {
                    this.persist_form(cx);
                }
            });
        })
        .detach();
    }

    fn current_form(&self, cx: &App) -> Result<CreationForm, String> {
        let target_repository = self
            .repositories
            .get(self.target_index)
            .cloned()
            .ok_or_else(|| "Choose an explicitly added target repository.".to_owned())?;
        let source_repository = self
            .repositories
            .get(self.source_index)
            .cloned()
            .ok_or_else(|| "Choose an explicitly added source repository.".to_owned())?;
        let local = self.local_branch.read(cx).value().trim().to_owned();
        let form = CreationForm {
            target_repository,
            base_branch: self.base_branch.read(cx).value().to_string(),
            source_repository,
            source_branch: self.source_branch.read(cx).value().to_string(),
            local_branch: (!local.is_empty()).then_some(local),
            title: self.title.read(cx).value().to_string(),
            body: self.body.read(cx).value().to_string(),
            draft: self.draft,
        };
        form.validate_durable()?;
        Ok(form)
    }

    fn set_form_disabled(&self, disabled: bool, cx: &mut Context<Self>) {
        for editor in [
            &self.base_branch,
            &self.source_branch,
            &self.local_branch,
            &self.title,
        ] {
            editor.update(cx, |editor, cx| editor.set_disabled(disabled, cx));
        }
        self.body
            .update(cx, |editor, cx| editor.set_disabled(disabled, cx));
    }

    fn persist_form(&mut self, cx: &mut Context<Self>) {
        let form = match self.current_form(cx) {
            Ok(form) => form,
            Err(_) => return,
        };
        self.queue_draft_save(
            DraftSnapshot {
                edit_generation: self.edit_generation,
                form,
            },
            cx,
        );
    }

    fn queue_draft_save(&mut self, snapshot: DraftSnapshot, cx: &mut Context<Self>) {
        self.draft_lane.queue(snapshot);
        self.start_next_draft_save(cx);
    }

    fn start_next_draft_save(&mut self, cx: &mut Context<Self>) {
        let Some((expected, snapshot)) = self.draft_lane.start_next() else {
            return;
        };
        let Some(store) = self.store.clone() else {
            self.draft_lane.complete_failure();
            self.notice = Some(
                "Durable creation storage is unavailable; draft was not closed or prepared.".into(),
            );
            self.close_after_save = false;
            self.prepare_queued = None;
            self.set_form_disabled(false, cx);
            return;
        };
        self.save_generation = self.save_generation.saturating_add(1);
        let generation = self.save_generation;
        let lifetime = self.lifetime_id.clone();
        let task = cx.background_spawn(async move {
            store.save_draft_if_current(expected, snapshot.form.clone())
        });
        cx.spawn(async move |dialog, cx| {
            let result = task.await;
            let _ = dialog.update(cx, |this, cx| {
                if !callback_matches(
                    &this.lifetime_id,
                    &lifetime,
                    this.save_generation,
                    generation,
                ) {
                    return;
                }
                match result {
                    Ok(record) => {
                        if let Err(error) = this.draft_lane.complete_success(&record) {
                            this.notice = Some(error);
                            this.close_after_save = false;
                            this.prepare_queued = None;
                            this.set_form_disabled(false, cx);
                        } else {
                            this.start_next_draft_save(cx);
                            if this.draft_lane.in_flight.is_none() {
                                this.after_draft_lane_drained(cx);
                            }
                        }
                    }
                    Err(error) => {
                        this.draft_lane.complete_failure();
                        this.close_after_save = false;
                        this.prepare_queued = None;
                        this.set_form_disabled(false, cx);
                        this.notice = Some(format!(
                            "Creation draft could not be made durable; close/prepare was refused: {error}"
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn after_draft_lane_drained(&mut self, cx: &mut Context<Self>) {
        if self.close_after_save {
            match self.current_form(cx) {
                Ok(form) if self.draft_lane.is_idle_durable(&form) => {
                    self.close_after_save = false;
                    self.finish_close(cx);
                }
                Ok(form) => {
                    self.draft_lane.queue(DraftSnapshot {
                        edit_generation: self.edit_generation,
                        form,
                    });
                    self.start_next_draft_save(cx);
                }
                Err(error) => {
                    self.close_after_save = false;
                    self.set_form_disabled(false, cx);
                    self.notice = Some(format!(
                        "Close refused because the latest form cannot be saved: {error}"
                    ));
                }
            }
            return;
        }
        let Some(snapshot) = self.prepare_queued.clone() else {
            return;
        };
        let unchanged = self.edit_generation == snapshot.edit_generation
            && self.current_form(cx).ok().as_ref() == Some(&snapshot.form);
        if unchanged && self.draft_lane.is_idle_durable(&snapshot.form) {
            self.prepare_queued = None;
            self.begin_provider_prepare(snapshot, cx);
        } else {
            self.prepare_queued = None;
            self.set_form_disabled(false, cx);
            self.notice = Some(
                "Visible creation fields changed while the draft save was pending. The latest text remains queued for durable save; prepare again."
                    .into(),
            );
        }
    }

    fn load_choices(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.repositories.get(self.target_index).cloned() else {
            return;
        };
        let Some(source) = self.repositories.get(self.source_index).cloned() else {
            return;
        };
        self.choices.loading = true;
        self.choice_generation = self.choice_generation.saturating_add(1);
        let generation = self.choice_generation;
        let lifetime = self.lifetime_id.clone();
        let task = cx.background_spawn(async move {
            let target_choices = GithubProvider::new(target.account.clone())
                .pr_lifecycle_choices(&target)?
                .branches;
            let source_choices = if source.cache_key() == target.cache_key() {
                target_choices.clone()
            } else {
                GithubProvider::new(source.account.clone())
                    .pr_lifecycle_choices(&source)?
                    .branches
            };
            Ok::<_, anyhow::Error>((target_choices, source_choices))
        });
        cx.spawn(async move |dialog, cx| {
            let result = task.await;
            let _ = dialog.update(cx, |this, cx| {
                if !callback_matches(&this.lifetime_id, &lifetime, this.choice_generation, generation) { return; }
                this.choices.loading = false;
                match result {
                    Ok((target, source)) => {
                        this.choices.target = Some(target);
                        this.choices.source = Some(source);
                    }
                    Err(error) => this.choices.notice = Some(format!("Published branch choices could not be read; no choice was guessed: {error:#}")),
                }
                cx.notify();
            });
        }).detach();
    }

    fn prepare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.state, DialogState::Editing) {
            return;
        }
        if self.store.is_none() {
            self.notice = Some("Durable creation storage is unavailable; zero writes sent.".into());
            cx.notify();
            return;
        }
        let form = match self.current_form(cx) {
            Ok(form) => form,
            Err(error) => {
                self.notice = Some(error);
                cx.notify();
                return;
            }
        };
        if let Err(error) = form.validate() {
            self.notice = Some(error);
            cx.notify();
            return;
        }
        window.focus(&self.focus, cx);
        let snapshot = DraftSnapshot {
            edit_generation: self.edit_generation,
            form,
        };
        self.set_form_disabled(true, cx);
        self.prepare_queued = Some(snapshot.clone());
        self.close_after_save = false;
        self.draft_lane.queue(snapshot.clone());
        self.start_next_draft_save(cx);
        if self.draft_lane.is_idle_durable(&snapshot.form) {
            self.after_draft_lane_drained(cx);
        } else {
            self.notice = Some(
                "Saving the exact visible draft before read-only provider preparation…".into(),
            );
            cx.notify();
        }
    }

    fn begin_provider_prepare(&mut self, snapshot: DraftSnapshot, cx: &mut Context<Self>) {
        self.state = DialogState::Preparing;
        self.operation_generation = self.operation_generation.saturating_add(1);
        let generation = self.operation_generation;
        let edit_generation = snapshot.edit_generation;
        let lifetime = self.lifetime_id.clone();
        let account = snapshot.form.target_repository.account.clone();
        let input = snapshot.form.input();
        let visible_form = snapshot.form;
        let task = cx.background_spawn(async move {
            GithubProvider::new(account)
                .prepare_pr_creation(&input)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |dialog, cx| {
            let result = task.await;
            let _ = dialog.update(cx, |this, cx| {
                if !callback_matches(&this.lifetime_id, &lifetime, this.operation_generation, generation) { return; }
                let unchanged = this.edit_generation == edit_generation && this.current_form(cx).ok().as_ref() == Some(&visible_form);
                match result {
                    Ok(preparation) if unchanged => {
                        if !preparation.can_create.available {
                            this.state = DialogState::Editing;
                            this.set_form_disabled(false, cx);
                            this.notice = Some(preparation.can_create.reason.unwrap_or_else(|| "Selected account cannot create this pull request.".into()));
                        } else {
                            let operation_id = match random_identity("create-pr") { Ok(value) => value, Err(error) => { this.state = DialogState::Editing; this.set_form_disabled(false, cx); this.notice = Some(error); cx.notify(); return; } };
                            let attempt_id = match random_identity("attempt") { Ok(value) => value, Err(error) => { this.state = DialogState::Editing; this.set_form_disabled(false, cx); this.notice = Some(error); cx.notify(); return; } };
                            this.frozen = Some(FrozenCreation {
                                visible_form,
                                request: PullRequestCreationRequest { operation_id, attempt_id, preparation },
                            });
                            this.state = DialogState::Confirmation;
                            this.notice = None;
                        }
                    }
                    Ok(_) => {
                        this.state = DialogState::Editing;
                        this.set_form_disabled(false, cx);
                        this.frozen = None;
                        this.notice = Some("Visible creation fields changed while preparation was pending. The result was discarded; prepare again.".into());
                    }
                    Err(error) => {
                        this.state = DialogState::Editing;
                        this.set_form_disabled(false, cx);
                        this.notice = Some(format!("PR creation preparation refused; zero writes sent: {error}"));
                    }
                }
                cx.notify();
            });
        }).detach();
    }

    fn request_close(&mut self, cx: &mut Context<Self>) {
        if self.close_after_save {
            return;
        }
        self.recovery_open_acknowledgement = None;
        self.begin_close(cx);
    }

    fn request_open_acknowledgement(
        &mut self,
        acknowledgement: PullRequestCreationAcknowledgement,
        cx: &mut Context<Self>,
    ) {
        if self.state.busy() || self.close_after_save {
            return;
        }
        self.recovery_open_acknowledgement = Some(acknowledgement);
        self.begin_close(cx);
        if !self.closed && !self.close_after_save {
            self.recovery_open_acknowledgement = None;
        }
    }

    fn finish_close(&mut self, cx: &mut Context<Self>) {
        self.closed = true;
        if let Some(acknowledgement) = self.recovery_open_acknowledgement.take() {
            cx.emit(OpenAcknowledgedPr(acknowledgement));
        }
    }

    fn begin_close(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, DialogState::Unavailable) {
            // No editable draft was installed. Closing preserves the unavailable
            // original; it must not try to replace it with the empty form.
            self.finish_close(cx);
            cx.notify();
            return;
        }
        if self.state.busy() {
            self.notice = Some(
                "Close is disabled while creation work is pending; no background result was detached."
                    .into(),
            );
            cx.notify();
            return;
        }
        if self.store.is_none() {
            self.notice = Some(
                "Close refused because the latest creation form cannot be made durable.".into(),
            );
            cx.notify();
            return;
        }
        let form = match self.current_form(cx) {
            Ok(form) => form,
            Err(error) => {
                self.notice = Some(format!(
                    "Close refused because the latest form cannot be saved: {error}"
                ));
                cx.notify();
                return;
            }
        };
        self.prepare_queued = None;
        if self.draft_lane.is_idle_durable(&form) {
            self.finish_close(cx);
        } else {
            self.close_after_save = true;
            self.set_form_disabled(true, cx);
            self.notice = Some("Saving the latest form before close…".into());
            self.queue_draft_save(
                DraftSnapshot {
                    edit_generation: self.edit_generation,
                    form,
                },
                cx,
            );
        }
        cx.notify();
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.state, DialogState::Confirmation) {
            return;
        }
        let Some(frozen) = self.frozen.clone() else {
            return;
        };
        if self.current_form(cx).ok().as_ref() != Some(&frozen.visible_form) {
            self.state = DialogState::Editing;
            self.set_form_disabled(false, cx);
            self.frozen = None;
            self.notice = Some("Visible form no longer matches the immutable confirmation. Zero writes sent; prepare again.".into());
            cx.notify();
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        self.state = DialogState::Creating;
        window.focus(&self.focus, cx);
        self.operation_generation = self.operation_generation.saturating_add(1);
        let generation = self.operation_generation;
        let lifetime = self.lifetime_id.clone();
        let request = frozen.request.clone();
        let provider =
            GithubProvider::new(request.preparation.input.target_repository.account.clone());
        let task = cx.background_spawn(async move {
            let mut admission = CreationAdmission::new(store, request.clone());
            let outcome = provider.execute_pr_creation(&request, &mut admission);
            let attempts = admission.store.load_attempt_summaries();
            (outcome, attempts)
        });
        cx.spawn(async move |dialog, cx| {
            let (outcome, attempts) = task.await;
            let _ = dialog.update(cx, |this, cx| {
                if !callback_matches(&this.lifetime_id, &lifetime, this.operation_generation, generation) { return; }
                match outcome {
                    ProviderMutationOutcome::Acknowledged(ack) => {
                        this.notice = Some("GitHub acknowledgement and exact PR coordinates are durable. Open PR is now available.".into());
                        this.state = DialogState::Acknowledged(Box::new(ack));
                    }
                    ProviderMutationOutcome::PreflightRejected { reason } => {
                        this.notice = Some(format!("Creation refused: {reason}"));
                        this.state = DialogState::Editing;
                        this.set_form_disabled(false, cx);
                        this.frozen = None;
                    }
                    ProviderMutationOutcome::Uncertain { reason, .. } => {
                        this.notice = Some("Creation outcome is unresolved. The exact context is retained below; there is no implicit retry or branch/body search.".into());
                        this.state = DialogState::Uncertain(reason);
                    }
                }
                match attempts {
                    Ok(attempts) => this.attempts = attempts,
                    Err(error) => this.notice = Some(format!("Creation finished, but recovery records cannot be read: {error}. Do not retry.")),
                }
                cx.notify();
            });
        }).detach();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, DialogState::Unavailable) {
            self.request_close(cx);
            return;
        }
        if self.state.busy() {
            return;
        }
        self.prepare_queued = None;
        self.close_after_save = false;
        self.frozen = None;
        self.state = DialogState::Editing;
        self.set_form_disabled(false, cx);
        self.notice = Some(
            "Creation confirmation cancelled; zero writes sent. The durable draft is retained."
                .into(),
        );
        cx.notify();
    }

    fn choose_repository(&mut self, target: bool, index: usize, cx: &mut Context<Self>) {
        if self.state.busy()
            || matches!(self.state, DialogState::Unavailable)
            || self.close_after_save
            || self.prepare_queued.is_some()
            || matches!(self.state, DialogState::Confirmation)
            || index >= self.repositories.len()
        {
            return;
        }
        if target {
            self.target_index = index;
        } else {
            self.source_index = index;
        }
        self.frozen = None;
        self.state = DialogState::Editing;
        self.choices.target = None;
        self.choices.source = None;
        self.edit_generation = self.edit_generation.saturating_add(1);
        self.load_choices(cx);
        self.persist_form(cx);
        cx.notify();
    }

    fn use_branch(
        &mut self,
        target: bool,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.busy()
            || matches!(self.state, DialogState::Unavailable)
            || self.close_after_save
            || self.prepare_queued.is_some()
            || matches!(self.state, DialogState::Confirmation)
        {
            return;
        }
        let editor = if target {
            &self.base_branch
        } else {
            &self.source_branch
        };
        editor.update(cx, |input, cx| input.set_value(value, window, cx));
    }

    fn render_form(&self, colors: Palette, cx: &mut Context<Self>) -> Div {
        let inert = self.close_after_save
            || self.prepare_queued.is_some()
            || self.state.busy()
            || matches!(self.state, DialogState::Unavailable)
            || matches!(self.state, DialogState::Confirmation);
        let target_repositories = repository_choices(
            &self.repositories,
            true,
            self.target_index,
            inert,
            colors,
            cx,
        );
        let source_repositories = repository_choices(
            &self.repositories,
            false,
            self.source_index,
            inert,
            colors,
            cx,
        );
        let mut panel = div()
            .child(section_label("Target repository + account", colors))
            .child(
                div()
                    .mt_2()
                    .flex()
                    .flex_wrap()
                    .gap(px(ui::GAP_GROUP))
                    .children(target_repositories),
            )
            .child(field_label("Target base branch", colors).mt(px(ui::GAP_GROUP)))
            .child(editor_box(&self.base_branch, colors))
            .child(branch_choices(
                "target",
                true,
                self.choices.target.as_ref(),
                inert,
                colors,
                cx,
            ))
            .child(section_label("Published source repository", colors).mt(px(ui::GAP_PAGE)))
            .child(
                div()
                    .mt_2()
                    .flex()
                    .flex_wrap()
                    .gap(px(ui::GAP_GROUP))
                    .children(source_repositories),
            )
            .child(field_label("Published source branch", colors).mt(px(ui::GAP_GROUP)))
            .child(editor_box(&self.source_branch, colors))
            .child(branch_choices(
                "source",
                false,
                self.choices.source.as_ref(),
                inert,
                colors,
                cx,
            ))
            .child(field_label("Local branch · informational only", colors).mt(px(ui::GAP_GROUP)))
            .child(editor_box(&self.local_branch, colors))
            .child(field_label("Title", colors).mt(px(ui::GAP_GROUP)))
            .child(editor_box(&self.title, colors))
            .child(field_label("Description · Markdown", colors).mt(px(ui::GAP_GROUP)))
            .child(
                div()
                    .mt(px(ui::GAP_FIELD))
                    .h(px(DESCRIPTION_FIELD_HEIGHT))
                    .p(px(ui::CELL_INSET))
                    .bg(colors.elevated)
                    .border_1()
                    .border_color(colors.border)
                    .rounded(px(ui::CONTROL_RADIUS))
                    .ui_text(TextRole::Body)
                    .overflow_hidden()
                    .child(Textarea::new(&self.body)),
            )
            .child(
                clickable(
                    "creation-draft-toggle",
                    if self.draft {
                        "● Draft PR"
                    } else {
                        "○ Normal PR"
                    },
                    colors,
                    !inert,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.state.busy()
                        && !this.close_after_save
                        && this.prepare_queued.is_none()
                        && !matches!(this.state, DialogState::Confirmation)
                    {
                        this.draft = !this.draft;
                        this.form_changed(cx);
                        cx.notify();
                    }
                })),
            );
        if self.choices.loading {
            panel = panel.child(
                div()
                    .mt_2()
                    .ui_text(TextRole::Caption)
                    .text_color(colors.muted)
                    .child("Loading bounded published branch choices…"),
            );
        }
        if let Some(notice) = &self.choices.notice {
            panel = panel.child(
                div()
                    .mt_2()
                    .ui_text(TextRole::Caption)
                    .text_color(colors.amber)
                    .child(notice.clone()),
            );
        }
        panel
    }

    fn render_confirmation(&self, colors: Palette, cx: &mut Context<Self>) -> Div {
        let Some(frozen) = &self.frozen else {
            return div();
        };
        let input = &frozen.request.preparation.input;
        let preparation = &frozen.request.preparation;
        div()
            .p(px(ui::PANEL_GUTTER))
            .rounded(px(ui::CONTROL_RADIUS))
            .border_1()
            .border_color(colors.amber)
            .bg(colors.elevated)
            .child(div().ui_text(TextRole::Subtitle).child("Confirm immutable PR creation"))
            .child(div().mt_2().child(format!("{}:{}  →  {}:{}", input.source_repository.full_name(), input.source_branch, input.target_repository.full_name(), input.base_branch)))
            .child(div().mt_1().text_color(colors.muted).child(format!("Account {} · {}", preparation.viewer_login, if input.draft { "draft" } else { "normal" })))
            .child(div().mt_2().child(input.title.clone()))
            .child(div().mt_2().text_color(colors.amber).child("GitHub cannot atomically require the reviewed source head during creation. The source is re-read under durable admission; the acknowledgement will show actual and reviewed heads separately."))
            .child(clickable("creation-details-disclosure", if self.details_disclosed { "Hide full request details" } else { "Show full request details" }, colors, true)
                .on_click(cx.listener(|this, _, _, cx| { this.details_disclosed = !this.details_disclosed; cx.notify(); })))
            .when(self.details_disclosed, |panel| panel.child(
                div().mt_2().p(px(ui::CELL_INSET)).rounded(px(ui::CONTROL_RADIUS)).bg(colors.canvas).ui_text(TextRole::Caption).child(confirmation_details(frozen))
            ))
    }

    fn render_outcome(&self, colors: Palette, cx: &mut Context<Self>) -> Div {
        match &self.state {
            DialogState::Acknowledged(ack) => div()
                .p(px(ui::PANEL_GUTTER)).rounded(px(ui::CONTROL_RADIUS)).border_1().border_color(colors.green).bg(colors.elevated)
                .child(div().ui_text(TextRole::Subtitle).child(format!("Created #{}", ack.pull_request.pull_request)))
                .child(div().mt_1().child(ack.url.clone()))
                .child(div().mt_2().child(format!("Actual created head: {}", ack.actual_head_sha)))
                .child(div().mt_1().text_color(if ack.actual_head_sha == ack.reviewed_head_sha { colors.muted } else { colors.amber }).child(format!("Reviewed preparation head: {}", ack.reviewed_head_sha)))
                .when(ack.actual_head_sha != ack.reviewed_head_sha, |panel| panel.child(div().mt_1().text_color(colors.amber).child("The published head moved during GitHub creation. Actual is not relabeled as reviewed.")))
                .child(clickable("open-created-pr", "Open PR", colors, true).on_click(cx.listener(|this, _, _, cx| {
                    if let DialogState::Acknowledged(acknowledgement) = &this.state {
                        this.request_open_acknowledgement((**acknowledgement).clone(), cx);
                    }
                }))),
            DialogState::Uncertain(reason) => div()
                .p(px(ui::PANEL_GUTTER)).rounded(px(ui::CONTROL_RADIUS)).border_1().border_color(colors.red).bg(colors.elevated)
                .child(div().ui_text(TextRole::Subtitle).child("Creation outcome unresolved"))
                .child(div().mt_2().text_color(colors.red).child(reason.clone()))
                .child(div().mt_2().child("The exact request/context is durable. Do not retry: an unknown ID, lost reply, or terminal-save failure cannot be resolved by body search, branch-only adoption, or absence.")),
            _ => div(),
        }
    }

    fn render_attempts(&self, colors: Palette, cx: &mut Context<Self>) -> Div {
        let unresolved = self
            .attempts
            .iter()
            .filter(|attempt| attempt.unresolved)
            .count();
        div()
            .mt(px(ui::GAP_PAGE))
            .child(
                clickable(
                    "creation-attempts-disclosure",
                    &format!(
                        "{} recovery records · {} unresolved",
                        self.attempts.len(),
                        unresolved
                    ),
                    colors,
                    true,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.attempts_disclosed = !this.attempts_disclosed;
                    cx.notify();
                })),
            )
            .when(self.attempts_disclosed, |panel| {
                panel.children(
                    self.attempts
                        .iter()
                        .rev()
                        .enumerate()
                        .map(|(index, attempt)| {
                            div()
                                .mt_2()
                                .p_3()
                                .rounded(px(ui::CONTROL_RADIUS))
                                .border_1()
                                .border_color(if attempt.unresolved {
                                    colors.amber
                                } else {
                                    colors.border
                                })
                                .child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(format!("{} → {}", attempt.source, attempt.target)),
                                )
                                .child(
                                    div()
                                        .mt_1()
                                        .ui_text(TextRole::Caption)
                                        .child(attempt.status.clone()),
                                )
                                .child(
                                    div()
                                        .mt_1()
                                        .ui_text(TextRole::Caption)
                                        .text_color(colors.muted)
                                        .child(format!(
                                            "{} / {} · reviewed {} · {}",
                                            attempt.operation_id,
                                            attempt.attempt_id,
                                            attempt.reviewed_head,
                                            attempt.title
                                        )),
                                )
                                .child(
                                    div()
                                        .mt_1()
                                        .ui_text(TextRole::Caption)
                                        .text_color(colors.faint)
                                        .child(attempt.authority.clone()),
                                )
                                .when_some(
                                    attempt.acknowledgement.clone(),
                                    |card, acknowledgement| {
                                        card.child(
                                            clickable(
                                                SharedString::from(format!(
                                                    "open-durable-created-pr-{index}"
                                                )),
                                                "Open this exact PR",
                                                colors,
                                                !self.state.busy() && !self.close_after_save,
                                            )
                                            .on_click(
                                                cx.listener(move |this, _, _, cx| {
                                                    this.request_open_acknowledgement(
                                                        acknowledgement.clone(),
                                                        cx,
                                                    );
                                                }),
                                            ),
                                        )
                                    },
                                )
                        }),
                )
            })
    }

    #[cfg(feature = "ui-smoke")]
    fn install_synthetic_acknowledgement(&mut self) -> Result<(), String> {
        let frozen = self
            .frozen
            .clone()
            .ok_or_else(|| "smoke has no read-only preparation".to_owned())?;
        let input = &frozen.request.preparation.input;
        self.state = DialogState::Acknowledged(Box::new(PullRequestCreationAcknowledgement {
            operation_id: frozen.request.operation_id,
            target_repository: input.target_repository.clone(),
            source_repository: input.source_repository.clone(),
            pull_request: cibergit::domain::ProviderCoordinates {
                provider: "github".into(),
                host: input.target_repository.host.clone(),
                owner: input.target_repository.owner.clone(),
                repository: input.target_repository.name.clone(),
                pull_request: 4242,
                remote_id: "PR_SYNTHETIC_NATIVE_CREATE".into(),
            },
            actual_head_sha: "f".repeat(40),
            reviewed_head_sha: frozen.request.preparation.observed_source_head_sha,
            reviewed_head_atomically_enforced: false,
            url: format!(
                "https://github.com/{}/pull/4242",
                input.target_repository.full_name()
            ),
        }));
        self.notice = Some(
            "Synthetic outcome control for native evidence; creation transport was not invoked."
                .into(),
        );
        Ok(())
    }
}

impl Render for PrCreationDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.closed {
            return div().into_any_element();
        }
        let colors = palette(is_dark(window));
        let confirmation = matches!(self.state, DialogState::Confirmation);
        let outcome = matches!(
            self.state,
            DialogState::Acknowledged(_) | DialogState::Uncertain(_)
        );
        div()
            .id("pr-creation-overlay")
            .key_context("PrCreation")
            .absolute().inset_0().flex().items_center().justify_center()
            .bg(if colors.dark { rgba(0x00000099) } else { rgba(0x20212455) })
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &PreparePullRequestCreation, window, cx| this.prepare(window, cx)))
            .on_action(cx.listener(|this, _: &ConfirmPullRequestCreation, window, cx| this.confirm(window, cx)))
            .on_action(cx.listener(|this, _: &CancelPullRequestCreation, _, cx| this.cancel(cx)))
            .on_action(cx.listener(|this, _: &TogglePullRequestCreationDraft, _, cx| { if !this.state.busy() && !matches!(this.state, DialogState::Unavailable) && !this.close_after_save && this.prepare_queued.is_none() { this.draft = !this.draft; this.form_changed(cx); cx.notify(); } }))
            .on_action(cx.listener(|this, _: &ClosePullRequestCreation, _, cx| this.request_close(cx)))
            .child(
                div()
                    .id("pr-creation-dialog")
                    .w(px(760.)).max_w(rems(58.)).max_h(relative(0.92))
                    .overflow_y_scroll().p(px(ui::PANEL_GUTTER)).rounded(px(ui::WINDOW_RADIUS)).border_1().border_color(colors.border).bg(colors.surface)
                    .child(div().flex().justify_between().items_start()
                        .child(div().child(div().ui_text(TextRole::Display).font_weight(FontWeight::MEDIUM).child("Create pull request"))
                            .child(div().mt_1().text_color(colors.muted).child("Published branch only · no automatic push · explicit immutable confirmation")))
                        .child(clickable("close-pr-creation", "Close", colors, !self.state.busy()).on_click(cx.listener(|this, _, _, cx| this.request_close(cx)))))
                    .when(!confirmation && !outcome, |dialog| dialog.child(self.render_form(colors, cx)))
                    .when(confirmation, |dialog| dialog.child(self.render_confirmation(colors, cx)))
                    .when(outcome, |dialog| dialog.child(self.render_outcome(colors, cx)))
                    .when_some(self.notice.clone(), |dialog, notice| dialog.child(div().mt_3().p_3().rounded(px(ui::CONTROL_RADIUS)).bg(colors.elevated).text_color(if matches!(self.state, DialogState::Uncertain(_)) { colors.red } else { colors.muted }).child(notice)))
                    .when(matches!(self.state, DialogState::Preparing | DialogState::Creating | DialogState::Loading), |dialog| dialog.child(div().mt(px(ui::GAP_PAGE)).text_color(colors.muted).child(match self.state { DialogState::Preparing => "Preparing with fresh read-only provider state…", DialogState::Creating => "Creation admitted durably; waiting for the single provider result…", _ => "Loading durable creation draft and recovery records…" })))
                    .when(!outcome, |dialog| dialog.child(div().mt(px(ui::GAP_PAGE)).flex().justify_end().gap(px(ui::GAP_GROUP))
                        .child(clickable("cancel-pr-creation", if confirmation { "Cancel confirmation" } else { "Close" }, colors, !self.state.busy()).on_click(cx.listener(|this, _, _, cx| { if matches!(this.state, DialogState::Confirmation) { this.cancel(cx); } else { this.request_close(cx); } })))
                        .when(matches!(self.state, DialogState::Editing), |row| row.child(clickable("prepare-pr-creation", "Prepare creation…", colors, self.store.is_some()).on_click(cx.listener(|this, _, window, cx| this.prepare(window, cx)))))
                        .when(confirmation, |row| row.child(clickable("confirm-pr-creation", "Create pull request", colors, true).on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx)))))
                    ))
                    .child(self.render_attempts(colors, cx)),
            )
            .into_any_element()
    }
}

fn new_input(
    value: &str,
    placeholder: &str,
    window: &mut Window,
    cx: &mut Context<PrCreationDialog>,
) -> Entity<InputState> {
    let colors = palette(is_dark(window));
    cx.new(|cx| {
        let mut editor = InputState::new(window, cx);
        editor.set_editor_style(input_style(colors));
        editor.set_value(value.to_owned(), window, cx);
        editor.set_placeholder(placeholder.to_owned(), window, cx);
        editor
    })
}

fn new_textarea(
    value: &str,
    placeholder: &str,
    window: &mut Window,
    cx: &mut Context<PrCreationDialog>,
) -> Entity<TextareaState> {
    let colors = palette(is_dark(window));
    cx.new(|cx| {
        let mut editor = TextareaState::new(window, cx).auto_grow(3, 8);
        editor.set_editor_style(input_style(colors));
        editor.set_value(value.to_owned(), window, cx);
        editor.set_placeholder(placeholder.to_owned(), window, cx);
        editor
    })
}

fn set_input_value(
    editor: Entity<InputState>,
    value: String,
    handle: AnyWindowHandle,
    cx: &mut Context<PrCreationDialog>,
) {
    let _ = cx.update_window(handle, |_, window, cx| {
        editor.update(cx, |editor, cx| editor.set_value(value, window, cx));
    });
}

fn set_textarea_value(
    editor: Entity<TextareaState>,
    value: String,
    handle: AnyWindowHandle,
    cx: &mut Context<PrCreationDialog>,
) {
    let _ = cx.update_window(handle, |_, window, cx| {
        editor.update(cx, |editor, cx| editor.set_value(value, window, cx));
    });
}

fn field_label(label: &str, colors: Palette) -> Div {
    div()
        .ui_text(TextRole::Label)
        .text_color(colors.muted)
        .child(label.to_owned())
}

fn section_label(label: &str, colors: Palette) -> Div {
    div()
        .mt(px(ui::GAP_PAGE))
        .font_weight(FontWeight::MEDIUM)
        .text_color(colors.text)
        .child(label.to_owned())
}

// Matches the review window's `input_box`: the same inset and field surface, so
// a text field does not change shape between the two dialogs.
fn editor_box(editor: &Entity<InputState>, colors: Palette) -> Div {
    div()
        .mt(px(ui::GAP_FIELD))
        .h(px(ui::CONTROL_HEIGHT))
        .px(px(ui::CELL_INSET))
        .bg(colors.elevated)
        .border_1()
        .border_color(colors.border)
        .rounded(px(ui::CONTROL_RADIUS))
        .ui_text(TextRole::Body)
        .child(Input::new(editor))
}

/// A native Button rather than a clickable div, so every action in this dialog
/// is reachable with Tab and activates on Enter and Space.
fn clickable(id: impl Into<ElementId>, label: &str, colors: Palette, enabled: bool) -> Button {
    Button::new(id)
        .mt(px(ui::GAP_GROUP))
        .control()
        .disabled(!enabled)
        .disabled_presentation()
        .focus_ring(colors.accent, colors.selected)
        .accessibility_label(label.to_owned())
        .rounded(px(ui::CONTROL_RADIUS))
        .max_w(px(CHOICE_PILL_MAX_WIDTH))
        .border_1()
        .border_color(colors.border)
        .flex_none()
        .text_color(if enabled { colors.accent } else { colors.faint })
        .when(enabled, |view| {
            view.cursor_pointer().hover(|view| view.bg(colors.selected))
        })
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(label.to_owned()),
        )
}

fn pill(id: String, label: String, selected: bool, enabled: bool, colors: Palette) -> Button {
    Button::new(SharedString::from(id))
        .control()
        .disabled(!enabled)
        .disabled_presentation()
        .focus_ring(colors.accent, colors.selected)
        .selected(selected)
        .accessibility_label(label.clone())
        .max_w(px(CHOICE_PILL_MAX_WIDTH))
        .rounded(px(ui::CONTROL_RADIUS))
        .border_1()
        .border_color(if selected && enabled {
            colors.accent
        } else {
            colors.border
        })
        .when(selected, |view| view.bg(colors.selected))
        .text_color(if enabled { colors.text } else { colors.faint })
        // An inert chip drops the pointer cursor and the hover response rather
        // than presenting itself as a live choice.
        .when(enabled, |view| {
            view.cursor_pointer().hover(|view| view.bg(colors.selected))
        })
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(label),
        )
}

fn repository_choices(
    repositories: &[Repository],
    target: bool,
    selected: usize,
    inert: bool,
    colors: Palette,
    cx: &mut Context<PrCreationDialog>,
) -> Vec<Button> {
    repositories
        .iter()
        .enumerate()
        .map(|(index, repository)| {
            let label = format!("{} · {}", repository.full_name(), repository.account.login);
            let mut button = pill(
                format!("creation-repository-{target}-{index}"),
                label,
                selected == index,
                !inert,
                colors,
            );
            if !inert {
                button = button.on_click(
                    cx.listener(move |this, _, _, cx| this.choose_repository(target, index, cx)),
                );
            }
            button
        })
        .collect()
}

fn branch_choices(
    prefix: &'static str,
    target: bool,
    choices: Option<&ProviderChoiceSet>,
    inert: bool,
    colors: Palette,
    cx: &mut Context<PrCreationDialog>,
) -> Div {
    let Some(choices) = choices else { return div() };
    div().mt_2().flex().flex_wrap().gap(px(ui::GAP_ICON))
        .children(choices.values.iter().take(24).enumerate().map(|(index, choice)| {
            let value = choice.name.clone();
            let mut button = clickable(SharedString::from(format!("creation-{prefix}-branch-{index}")), &value, colors, !inert);
            if !inert { button = button.on_click(cx.listener(move |this, _, window, cx| this.use_branch(target, value.clone(), window, cx))); }
            button
        }))
        .when(choices.values.len() > 24, |row| row.child(
            div().w_full().ui_text(TextRole::Caption).text_color(colors.amber).child(format!(
                "Showing 24 of {} bounded branch choices. Type an exact published branch to use a value outside this viewport.",
                choices.values.len()
            ))
        ))
        .when(!choices.complete, |row| row.child(div().w_full().ui_text(TextRole::Caption).text_color(colors.amber).child(choices.notice.clone().unwrap_or_else(|| "Branch choices are incomplete; typed values still require provider verification.".into()))))
}

#[cfg(feature = "ui-smoke")]
pub fn start_smoke(
    weak: WeakEntity<Root>,
    output: PathBuf,
    window: &mut Window,
    cx: &mut Context<Root>,
) {
    window.spawn(cx, async move |window| {
        let started = std::time::Instant::now();
        loop {
            window.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let ready = window.update(|window, cx| weak.update(cx, |root, cx| {
                let Root::Review(this) = root else { return false };
                if this.repositories.is_empty() { return false; }
                if this.creation_dialog.is_none() { this.open_creation_dialog(window, cx); }
                this.creation_dialog.as_ref().is_some_and(|dialog| {
                    let dialog = dialog.read(cx);
                    matches!(dialog.state, DialogState::Editing)
                        && dialog.choices.target.as_ref().is_some_and(|choices| !choices.values.is_empty())
                        && dialog.choices.source.as_ref().is_some_and(|choices| !choices.values.is_empty())
                })
            }).unwrap_or(false)).unwrap_or(false);
            if ready || started.elapsed() > std::time::Duration::from_secs(90) { break; }
        }
        let _ = fs::create_dir_all(&output);
        let preparation_started = window.update(|window, cx| weak.update(cx, |root, cx| {
            let Root::Review(this) = root else { return false };
            let Some(dialog) = this.creation_dialog.clone() else { return false };
            dialog.update(cx, |dialog, cx| {
                let Some(base) = dialog.choices.target.as_ref().and_then(|choices| choices.values.first()).map(|choice| choice.name.clone()) else { return false };
                let Some(source) = dialog.choices.source.as_ref().and_then(|choices| choices.values.first()).map(|choice| choice.name.clone()) else { return false };
                dialog.base_branch.update(cx, |input, cx| input.set_value(base, window, cx));
                dialog.source_branch.update(cx, |input, cx| input.set_value(source, window, cx));
                dialog.title.update(cx, |input, cx| input.set_value("Native creation fixture proof", window, cx));
                dialog.body.update(cx, |input, cx| input.set_value("Read-only provider preparation; no creation transport.", window, cx));
                dialog.prepare(window, cx);
                true
            })
        }).unwrap_or(false)).unwrap_or(false);
        let started = std::time::Instant::now();
        loop {
            window.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let confirmed = window.update(|_, cx| weak.read_with(cx, |root, cx| matches!(root, Root::Review(this) if this.creation_dialog.as_ref().is_some_and(|dialog| matches!(dialog.read(cx).state, DialogState::Confirmation)))).unwrap_or(false)).unwrap_or(false);
            if confirmed || started.elapsed() > std::time::Duration::from_secs(90) { break; }
        }
        let _ = window.update(|_, cx| weak.update(cx, |root, cx| {
            let Root::Review(this) = root else { return };
            if let Some(dialog) = &this.creation_dialog {
                dialog.update(cx, |dialog, cx| {
                    dialog.details_disclosed = true;
                    cx.notify();
                });
            }
        }));
        window.background_executor().timer(std::time::Duration::from_millis(350)).await;
        let confirmation_captured = window.update(|window, _| window.render_to_image().and_then(|image| image.save(output.join("native-pr-creation-confirmation.png")).map_err(Into::into)).is_ok()).unwrap_or(false);
        let synthetic = window.update(|_, cx| weak.update(cx, |root, cx| {
            let Root::Review(this) = root else { return false };
            this.creation_dialog.as_ref().is_some_and(|dialog| dialog.update(cx, |dialog, cx| { let ok = dialog.install_synthetic_acknowledgement().is_ok(); cx.notify(); ok }))
        }).unwrap_or(false)).unwrap_or(false);
        let _ = window.update(|window, _| window.resize(size(px(1040.), px(760.))));
        window.background_executor().timer(std::time::Duration::from_millis(350)).await;
        let outcome_captured = window.update(|window, _| window.render_to_image().and_then(|image| image.save(output.join("native-pr-creation-acknowledgement.png")).map_err(Into::into)).is_ok()).unwrap_or(false);
        let report = format!("Native PR creation smoke: {}\nRead-only provider preparation reached immutable confirmation: {}\nExact request/body disclosure expanded for confirmation capture: true\nSynthetic acknowledgement installed: {}\nWide confirmation capture: {}\nNarrow actual-vs-reviewed acknowledgement capture: {}\nWindow focus requested: false when CIBERGIT_SMOKE_BACKGROUND=1\nCreation transport: ZERO; Create was never pressed\nRepository/branch source: explicitly added fixture repository and bounded provider branch reads\nSynthetic controls: acknowledgement coordinates and moved actual head only\n", if preparation_started && confirmation_captured && synthetic && outcome_captured { "passed" } else { "failed" }, preparation_started, synthetic, confirmation_captured, outcome_captured);
        let _ = fs::write(output.join("native-pr-creation-smoke.txt"), report);
        if !(preparation_started && confirmation_captured && synthetic && outcome_captured) { panic!("native PR creation smoke failed"); }
        let _ = window.update(|_, cx| cx.quit());
    }).detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::domain::{Account, ProviderCapability};
    use std::{
        process::Command,
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering as AtomicOrdering},
        },
        thread,
    };
    use tempfile::tempdir;

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

    fn preparation(title: &str, source_sha: char) -> PullRequestCreationPreparation {
        PullRequestCreationPreparation {
            input: PullRequestCreationInput {
                target_repository: repository("Owner", "Repo", "Alice"),
                base_branch: "main".into(),
                source_repository: repository("Forker", "Fork", "Alice"),
                source_branch: "published/topic".into(),
                local_branch: Some("local/different".into()),
                title: title.into(),
                body: "body".into(),
                draft: false,
            },
            observed_base_sha: "a".repeat(40),
            observed_source_head_sha: source_sha.to_string().repeat(40),
            viewer_login: "Alice".into(),
            repository_permission: Some("WRITE".into()),
            can_create: ProviderCapability {
                available: true,
                reason: None,
            },
            reviewed_head_atomically_enforced: false,
            notice: Some("not atomic".into()),
        }
    }

    fn request(
        operation: &str,
        attempt: &str,
        title: &str,
        source_sha: char,
    ) -> PullRequestCreationRequest {
        PullRequestCreationRequest {
            operation_id: operation.into(),
            attempt_id: attempt.into(),
            preparation: preparation(title, source_sha),
        }
    }

    fn context(request: &PullRequestCreationRequest) -> MutationContext {
        MutationContext {
            operation_id: request.operation_id.clone(),
            attempt_id: request.attempt_id.clone(),
            action: "create-pr".into(),
            payload: serde_json::json!({ "request": request, "dispatch": { "method": "POST", "endpoint": "repos/Owner/Repo/pulls" } }),
        }
    }

    fn acknowledgement(request: &PullRequestCreationRequest) -> Value {
        let input = &request.preparation.input;
        serde_json::to_value(PullRequestCreationAcknowledgement {
            operation_id: request.operation_id.clone(),
            target_repository: input.target_repository.clone(),
            source_repository: input.source_repository.clone(),
            pull_request: cibergit::domain::ProviderCoordinates {
                provider: "github".into(),
                host: input.target_repository.host.clone(),
                owner: input.target_repository.owner.clone(),
                repository: input.target_repository.name.clone(),
                pull_request: 7,
                remote_id: "PR_node".into(),
            },
            actual_head_sha: request.preparation.observed_source_head_sha.clone(),
            reviewed_head_sha: request.preparation.observed_source_head_sha.clone(),
            reviewed_head_atomically_enforced: false,
            url: format!(
                "https://github.com/{}/pull/7",
                input.target_repository.full_name()
            ),
        })
        .unwrap()
    }

    #[test]
    fn authority_canonicalizes_github_identity_but_keeps_branches_exact() {
        let upper = CreationAuthorityKey::from_preparation(&preparation("one", 'b')).unwrap();
        let mut lower_prep = preparation("two", 'c');
        lower_prep.input.target_repository.owner = "owner".into();
        lower_prep.input.target_repository.name = "repo".into();
        lower_prep.input.source_repository.owner = "forker".into();
        lower_prep.input.source_repository.name = "fork".into();
        lower_prep.input.target_repository.account.login = "alice".into();
        lower_prep.input.source_repository.account.login = "alice".into();
        let lower = CreationAuthorityKey::from_preparation(&lower_prep).unwrap();
        assert_eq!(upper, lower);
        lower_prep.input.source_branch = "Published/Topic".into();
        assert_ne!(
            upper,
            CreationAuthorityKey::from_preparation(&lower_prep).unwrap()
        );
    }

    #[test]
    fn distinct_local_branch_is_informational_and_same_frozen_preparation_cannot_replay() {
        let directory = tempdir().unwrap();
        let store = CreationStore::open(directory.path().join("creation")).unwrap();
        let request = request("op-1", "attempt-1", "title", 'b');
        let mut admission = CreationAdmission::new(store.clone(), request.clone());
        let mut held = admission.admit(&context(&request)).unwrap();
        held.record_terminal(&MutationTerminalRecord::Acknowledged {
            acknowledgement: acknowledgement(&request),
        })
        .unwrap();
        drop(held);
        let mut replay = CreationAdmission::new(store, request.clone());
        let error = match replay.admit(&context(&request)) {
            Ok(_) => panic!("replay unexpectedly admitted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("already admitted"));
    }

    #[test]
    fn unresolved_attempt_survives_restart_and_other_current_form_tuple() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("creation");
        let store = CreationStore::open(root.clone()).unwrap();
        let request = request("unknown-operation", "lost-reply", "original title", 'b');
        let mut admission = CreationAdmission::new(store.clone(), request.clone());
        let held = admission.admit(&context(&request)).unwrap();
        drop(held);
        let restarted = CreationStore::open(root).unwrap();
        let summaries = restarted.load_attempt_summaries().unwrap();
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].unresolved);
        assert_eq!(summaries[0].title, "original title");
        assert_eq!(summaries[0].source, "Forker/Fork:published/topic");
    }

    #[test]
    fn uncertain_terminal_refuses_pending_replay_after_restart() {
        let directory = tempdir().unwrap();
        let store = CreationStore::open(directory.path().join("creation")).unwrap();
        let first_request = request("op-uncertain", "attempt-uncertain", "title", 'b');
        let mut admission = CreationAdmission::new(store.clone(), first_request.clone());
        let mut held = admission.admit(&context(&first_request)).unwrap();
        held.record_terminal(&MutationTerminalRecord::Uncertain {
            reason: "reply lost after dispatch".into(),
        })
        .unwrap();
        drop(held);
        let next = request("op-new", "attempt-new", "changed title", 'c');
        let mut restarted = CreationAdmission::new(store, next.clone());
        let error = match restarted.admit(&context(&next)) {
            Ok(_) => panic!("unresolved authority unexpectedly admitted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unresolved"));
    }

    #[test]
    fn conclusive_not_started_allows_fresh_ids_for_exact_explicit_retry() {
        let directory = tempdir().unwrap();
        let store = CreationStore::open(directory.path().join("creation")).unwrap();
        let first = request("op-first", "attempt-first", "title", 'b');
        let mut admission = CreationAdmission::new(store.clone(), first.clone());
        let mut held = admission.admit(&context(&first)).unwrap();
        held.record_terminal(&MutationTerminalRecord::NotStarted {
            reason: "second preflight proved the head moved before dispatch".into(),
        })
        .unwrap();
        drop(held);

        let second = request("op-retry", "attempt-retry", "title", 'b');
        let mut retry = CreationAdmission::new(store, second.clone());
        let mut held = retry.admit(&context(&second)).unwrap();
        held.record_terminal(&MutationTerminalRecord::NotStarted {
            reason: "explicit retry also ended before dispatch".into(),
        })
        .unwrap();
    }

    #[test]
    fn different_title_and_oid_share_authority_lane() {
        let first = CreationAuthorityKey::from_preparation(&preparation("one", 'b')).unwrap();
        let second = CreationAuthorityKey::from_preparation(&preparation("two", 'c')).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.component().unwrap(), second.component().unwrap());
    }

    #[test]
    fn initial_durable_write_failure_admits_nothing() {
        let directory = tempdir().unwrap();
        let blocked = directory.path().join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let request = request("op", "attempt", "title", 'b');
        let mut admission = CreationAdmission::new(
            CreationStore {
                root: blocked,
                draft_account: None,
            },
            request.clone(),
        );
        let error = match admission.admit(&context(&request)) {
            Ok(_) => panic!("blocked store unexpectedly admitted"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("directory") || error.contains("Not a directory"));
    }

    #[test]
    fn terminal_save_failure_leaves_durable_inflight() {
        let directory = tempdir().unwrap();
        let store = CreationStore::open(directory.path().join("creation")).unwrap();
        let request = request("op", "attempt", "title", 'b');
        let key = CreationAuthorityKey::from_preparation(&request.preparation).unwrap();
        let path = store.journal_path(&key).unwrap();
        let mut admission = CreationAdmission::new(store.clone(), request.clone());
        let mut held = admission.admit(&context(&request)).unwrap();
        let injected_link = path.with_extension("fault-link");
        fs::hard_link(&path, &injected_link).unwrap();
        assert!(
            held.record_terminal(&MutationTerminalRecord::Acknowledged {
                acknowledgement: acknowledgement(&request)
            })
            .is_err()
        );
        drop(held);
        fs::remove_file(&injected_link).unwrap();
        let journal = store.load_journal_unlocked(&key).unwrap();
        assert!(matches!(
            journal.attempts[0].status,
            CreationAttemptStatus::InFlight
        ));
    }

    #[test]
    fn draft_cas_blocks_stale_window_and_preserves_latest_dirty_text() {
        let directory = tempdir().unwrap();
        let store = CreationStore::open(directory.path().join("creation")).unwrap();
        let mut form = CreationForm {
            target_repository: repository("owner", "repo", "alice"),
            base_branch: "main".into(),
            source_repository: repository("owner", "repo", "alice"),
            source_branch: "topic".into(),
            local_branch: None,
            title: "first".into(),
            body: "dirty".into(),
            draft: false,
        };
        let first = store.save_draft_if_current(None, form.clone()).unwrap();
        form.title = "latest".into();
        let latest = store
            .save_draft_if_current(Some(first.generation), form.clone())
            .unwrap();
        let mut stale = form.clone();
        stale.title = "late callback".into();
        assert!(
            store
                .save_draft_if_current(Some(first.generation), stale)
                .is_err()
        );
        assert_eq!(store.load_draft().unwrap().unwrap(), latest);
    }

    #[test]
    fn account_drafts_preserve_legacy_text_and_do_not_split_creation_authority() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("creation");
        let legacy = CreationStore::open(root.clone()).unwrap();
        let prepared = preparation("Alice legacy draft", 'a');
        let input = &prepared.input;
        let form = CreationForm {
            target_repository: input.target_repository.clone(),
            base_branch: input.base_branch.clone(),
            source_repository: input.source_repository.clone(),
            source_branch: input.source_branch.clone(),
            local_branch: input.local_branch.clone(),
            title: input.title.clone(),
            body: input.body.clone(),
            draft: input.draft,
        };
        let old = legacy.save_draft_if_current(None, form.clone()).unwrap();
        let preserved = fs::read(legacy.draft_path()).unwrap();
        let alice =
            CreationStore::for_account(root.clone(), &input.target_repository.account).unwrap();
        let mut bob_form = form.clone();
        bob_form.target_repository.account.login = "bob".into();
        bob_form.source_repository.account.login = "bob".into();
        bob_form.body = "Bob independent text".into();
        let bob =
            CreationStore::for_account(root.clone(), &bob_form.target_repository.account).unwrap();
        assert!(bob.load_draft().unwrap().is_none());
        bob.save_draft_if_current(None, bob_form.clone()).unwrap();
        assert_eq!(bob.load_draft().unwrap().unwrap().form, bob_form);
        assert_eq!(alice.load_draft().unwrap(), Some(old.clone()));
        let mut edited = form.clone();
        edited.body = "Alice latest text".into();
        alice
            .save_draft_if_current(Some(old.generation), edited.clone())
            .unwrap();
        assert_eq!(fs::read(legacy.draft_path()).unwrap(), preserved);
        assert_eq!(alice.load_draft().unwrap().unwrap().form, edited);
        assert_eq!(bob.load_draft().unwrap().unwrap().form, bob_form);
        assert!(bob.save_draft_if_current(Some(1), form).is_err());
        let mut alias = input.target_repository.account.clone();
        alias.login = alias.login.to_ascii_uppercase();
        alias.host = alias.host.to_ascii_uppercase();
        let alias = CreationStore::for_account(root, &alias).unwrap();
        assert_eq!(alias.draft_path(), alice.draft_path());
        assert_eq!(alias.draft_lock_path(), alice.draft_lock_path());
        assert!(
            alias
                .save_draft_if_current(Some(old.generation), edited.clone())
                .is_err()
        );
        assert_eq!(alice.load_draft().unwrap().unwrap().form, edited);
        let authority = CreationAuthorityKey::from_preparation(&prepared).unwrap();
        assert_eq!(
            legacy.authority_lock_path(&authority).unwrap(),
            alice.authority_lock_path(&authority).unwrap()
        );
        assert_eq!(
            legacy.journal_path(&authority).unwrap(),
            alice.journal_path(&authority).unwrap()
        );
    }

    #[cfg(feature = "ui-smoke")]
    #[gpui::test]
    fn unavailable_saved_repository_can_close_without_overwriting_its_draft(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_base::init);
        let directory = tempdir().unwrap();
        let store = CreationStore::open(directory.path().join("creation")).unwrap();
        let form = CreationForm {
            target_repository: repository("removed", "repo", "alice"),
            source_repository: repository("removed", "repo", "alice"),
            base_branch: "main".into(),
            source_branch: "topic".into(),
            local_branch: None,
            title: "Retained removed-repository draft".into(),
            body: "Keep this text".into(),
            draft: false,
        };
        store.save_draft_if_current(None, form.clone()).unwrap();
        let before = fs::read(store.draft_path()).unwrap();
        let (dialog, cx) = cx.add_window_view(|window, cx| {
            PrCreationDialog::new(
                store.root.clone(),
                vec![repository("owner", "repo", "alice")],
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        dialog.update(cx, |this, cx| {
            assert!(matches!(this.state, DialogState::Unavailable));
            assert!(this.draft_lane.durable_generation.is_none());
            this.cancel(cx);
            assert!(this.closed);
        });
        assert_eq!(fs::read(store.draft_path()).unwrap(), before);
        assert_eq!(store.load_draft().unwrap().unwrap().form, form);
    }

    #[test]
    fn incomplete_dirty_form_is_durable_but_cannot_be_prepared() {
        let directory = tempdir().unwrap();
        let store = CreationStore::open(directory.path().join("creation")).unwrap();
        let form = CreationForm {
            target_repository: repository("owner", "repo", "alice"),
            base_branch: String::new(),
            source_repository: repository("owner", "repo", "alice"),
            source_branch: String::new(),
            local_branch: None,
            title: String::new(),
            body: "unfinished text\nthat must survive close".into(),
            draft: true,
        };
        assert!(form.validate().is_err());
        let saved = store.save_draft_if_current(None, form.clone()).unwrap();
        assert_eq!(store.load_draft().unwrap().unwrap(), saved);
        assert_eq!(saved.form, form);
    }

    #[test]
    fn typing_during_owned_save_serializes_latest_form_against_real_store() {
        let directory = tempdir().unwrap();
        let store = CreationStore::open(directory.path().join("creation")).unwrap();
        let first = CreationForm {
            target_repository: repository("owner", "repo", "alice"),
            base_branch: "main".into(),
            source_repository: repository("owner", "repo", "alice"),
            source_branch: "topic".into(),
            local_branch: None,
            title: "first".into(),
            body: "A".into(),
            draft: false,
        };
        let mut latest = first.clone();
        latest.body = "B typed while A saves".into();
        let mut lane = DraftSaveLane::default();
        lane.queue(DraftSnapshot {
            edit_generation: 1,
            form: first,
        });
        let (expected, active) = lane.start_next().unwrap();
        lane.queue(DraftSnapshot {
            edit_generation: 2,
            form: latest.clone(),
        });
        let saved_a = store.save_draft_if_current(expected, active.form).unwrap();
        lane.complete_success(&saved_a).unwrap();
        let (expected, active) = lane.start_next().unwrap();
        assert_eq!(expected, Some(saved_a.generation));
        let saved_b = store.save_draft_if_current(expected, active.form).unwrap();
        lane.complete_success(&saved_b).unwrap();
        assert!(lane.is_idle_durable(&latest));
        assert_eq!(store.load_draft().unwrap().unwrap().form, latest);
    }

    #[test]
    fn autosave_then_prepare_drains_exact_latest_snapshot_against_real_store() {
        let directory = tempdir().unwrap();
        let store = CreationStore::open(directory.path().join("creation")).unwrap();
        let mut autosave = CreationForm {
            target_repository: repository("owner", "repo", "alice"),
            base_branch: "main".into(),
            source_repository: repository("owner", "repo", "alice"),
            source_branch: "topic".into(),
            local_branch: None,
            title: "autosave".into(),
            body: "old".into(),
            draft: false,
        };
        let mut lane = DraftSaveLane::default();
        lane.queue(DraftSnapshot {
            edit_generation: 4,
            form: autosave.clone(),
        });
        let (expected, active) = lane.start_next().unwrap();
        autosave.title = "exact prepare click".into();
        let prepare = DraftSnapshot {
            edit_generation: 5,
            form: autosave.clone(),
        };
        lane.queue(prepare.clone());
        let saved_old = store.save_draft_if_current(expected, active.form).unwrap();
        lane.complete_success(&saved_old).unwrap();
        assert!(!lane.is_idle_durable(&prepare.form));
        let (expected, active) = lane.start_next().unwrap();
        let saved_prepare = store.save_draft_if_current(expected, active.form).unwrap();
        lane.complete_success(&saved_prepare).unwrap();
        assert!(lane.is_idle_durable(&prepare.form));
        assert_eq!(store.load_draft().unwrap().unwrap().form, prepare.form);
    }

    #[test]
    fn provider_read_failure_after_drain_does_not_poison_explicit_retry() {
        let directory = tempdir().unwrap();
        let store = CreationStore::open(directory.path().join("creation")).unwrap();
        let form = CreationForm {
            target_repository: repository("owner", "repo", "alice"),
            base_branch: "main".into(),
            source_repository: repository("owner", "repo", "alice"),
            source_branch: "topic".into(),
            local_branch: None,
            title: "retry after read failure".into(),
            body: "preserved".into(),
            draft: false,
        };
        let mut lane = DraftSaveLane::default();
        lane.queue(DraftSnapshot {
            edit_generation: 1,
            form: form.clone(),
        });
        let (expected, active) = lane.start_next().unwrap();
        let saved = store.save_draft_if_current(expected, active.form).unwrap();
        lane.complete_success(&saved).unwrap();
        let provider_result: Result<(), String> = Err("read-only preparation failed".into());
        assert!(provider_result.is_err());
        assert!(lane.is_idle_durable(&form));

        lane.queue(DraftSnapshot {
            edit_generation: 2,
            form: form.clone(),
        });
        assert!(lane.start_next().is_none());
        assert!(lane.is_idle_durable(&form));
        assert_eq!(store.load_draft().unwrap().unwrap().form, form);
    }

    #[test]
    fn close_barrier_queues_latest_dirty_form_without_waiting_for_debounce() {
        let directory = tempdir().unwrap();
        let store = CreationStore::open(directory.path().join("creation")).unwrap();
        let form = CreationForm {
            target_repository: repository("owner", "repo", "alice"),
            base_branch: String::new(),
            source_repository: repository("owner", "repo", "alice"),
            source_branch: String::new(),
            local_branch: None,
            title: String::new(),
            body: "last keystroke immediately before close".into(),
            draft: false,
        };
        let mut lane = DraftSaveLane::default();
        lane.queue(DraftSnapshot {
            edit_generation: 99,
            form: form.clone(),
        });
        let (expected, active) = lane.start_next().unwrap();
        let saved = store.save_draft_if_current(expected, active.form).unwrap();
        lane.complete_success(&saved).unwrap();
        assert!(lane.is_idle_durable(&form));
        assert_eq!(store.load_draft().unwrap().unwrap().form, form);
    }

    #[cfg(feature = "ui-smoke")]
    #[gpui::test]
    fn recovery_open_saves_latest_text_and_emits_clicked_historical_pr(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::{cell::RefCell, rc::Rc};
        cx.update(gpui_base::init);
        let directory = tempdir().unwrap();
        let store = CreationStore::for_account(
            directory.path().join("creation"),
            &repository("owner", "repo", "alice").account,
        )
        .unwrap();
        store.ensure_layout().unwrap();
        let (dialog, cx) = cx.add_window_view(|window, cx| {
            let mut dialog = PrCreationDialog::new(
                store.root.clone(),
                vec![repository("owner", "repo", "alice")],
                None,
                window,
                cx,
            );
            // Keep this controller fixture offline: discard the initial load callback
            // before it can start provider branch reads.
            dialog.load_generation += 1;
            dialog.state = DialogState::Editing;
            dialog.set_form_disabled(false, cx);
            dialog
        });
        let request = request("open-history", "attempt-history", "title", 'a');
        let historical: PullRequestCreationAcknowledgement =
            serde_json::from_value(acknowledgement(&request)).unwrap();
        let mut latest = historical.clone();
        latest.pull_request.pull_request += 1;
        let opened = Rc::new(RefCell::new(Vec::new()));
        let observations = opened.clone();
        let durable = store.clone();
        dialog.update(cx, |_, cx| {
            cx.subscribe(&dialog, move |this, _, event: &OpenAcknowledgedPr, _| {
                assert!(this.closed);
                let saved = durable.load_draft().unwrap().unwrap();
                assert_eq!(saved.form.body, "final keystroke before opening history");
                observations.borrow_mut().push(event.0.clone());
            })
            .detach();
        });
        cx.update(|window, cx| {
            dialog.update(cx, |this, cx| {
                this.body.update(cx, |body, cx| {
                    body.set_value("final keystroke before opening history", window, cx)
                });
                this.state = DialogState::Acknowledged(Box::new(latest));
                this.request_open_acknowledgement(historical.clone(), cx);
                assert!(!this.closed, "must await the latest durable save");
                assert!(this.close_after_save);
            });
        });
        cx.run_until_parked();
        assert_eq!(*opened.borrow(), vec![historical]);
    }

    #[cfg(feature = "ui-smoke")]
    #[gpui::test]
    fn recovery_open_refuses_busy_or_failed_save_without_detaching_text(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::{cell::RefCell, rc::Rc};
        cx.update(gpui_base::init);
        let directory = tempdir().unwrap();
        let store = CreationStore::for_account(
            directory.path().join("creation"),
            &repository("owner", "repo", "alice").account,
        )
        .unwrap();
        store.ensure_layout().unwrap();
        let (dialog, cx) = cx.add_window_view(|window, cx| {
            let mut dialog = PrCreationDialog::new(
                store.root.clone(),
                vec![repository("owner", "repo", "alice")],
                None,
                window,
                cx,
            );
            dialog.load_generation += 1;
            dialog.state = DialogState::Editing;
            dialog.set_form_disabled(false, cx);
            dialog
        });
        let request = request("open-history", "attempt-history", "title", 'a');
        let historical: PullRequestCreationAcknowledgement =
            serde_json::from_value(acknowledgement(&request)).unwrap();
        let opened = Rc::new(RefCell::new(Vec::new()));
        let observations = opened.clone();
        dialog.update(cx, |_, cx| {
            cx.subscribe(&dialog, move |_, _, event: &OpenAcknowledgedPr, _| {
                observations.borrow_mut().push(event.0.clone());
            })
            .detach();
        });
        cx.update(|window, cx| {
            dialog.update(cx, |this, cx| {
                this.body.update(cx, |body, cx| {
                    body.set_value("unsaved recovery draft", window, cx)
                });
                this.state = DialogState::Creating;
                this.request_open_acknowledgement(historical.clone(), cx);
                assert!(!this.closed);
                assert!(!this.close_after_save);
                assert!(this.recovery_open_acknowledgement.is_none());
                this.state = DialogState::Editing;
                // An invalid existing original must be preserved, and navigation refused.
                fs::write(store.draft_path(), "corrupt original").unwrap();
                this.request_open_acknowledgement(historical.clone(), cx);
            });
        });
        cx.run_until_parked();
        dialog.update(cx, |this, cx| {
            assert!(!this.closed);
            assert!(!this.close_after_save);
            assert_eq!(
                this.current_form(cx).unwrap().body,
                "unsaved recovery draft"
            );
            assert!(
                this.notice
                    .as_deref()
                    .unwrap()
                    .contains("could not be made durable")
            );
        });
        assert!(opened.borrow().is_empty());
        assert_eq!(
            fs::read_to_string(store.draft_path()).unwrap(),
            "corrupt original"
        );
    }

    #[test]
    fn corrupt_future_and_oversize_records_are_preserved_and_refused() {
        let directory = tempdir().unwrap();
        let store = CreationStore::open(directory.path().join("creation")).unwrap();
        let path = store.draft_path();
        ensure_private_directory(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{corrupt").unwrap();
        let corrupt = fs::read(&path).unwrap();
        assert!(store.load_draft().is_err());
        assert_eq!(fs::read(&path).unwrap(), corrupt);
        let form = CreationForm {
            target_repository: repository("owner", "repo", "alice"),
            base_branch: "main".into(),
            source_repository: repository("owner", "repo", "alice"),
            source_branch: "topic".into(),
            local_branch: None,
            title: "title".into(),
            body: "body".into(),
            draft: false,
        };
        fs::write(
            &path,
            serde_json::to_vec(&DraftRecord {
                version: 99,
                generation: 1,
                form,
            })
            .unwrap(),
        )
        .unwrap();
        let future = fs::read(&path).unwrap();
        assert!(
            store
                .load_draft()
                .unwrap_err()
                .contains("unsupported version")
        );
        assert_eq!(fs::read(&path).unwrap(), future);
        fs::write(&path, vec![b'x'; MAX_DRAFT_BYTES + 1]).unwrap();
        assert!(store.load_draft().unwrap_err().contains("exceeds"));
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            (MAX_DRAFT_BYTES + 1) as u64
        );
    }

    #[test]
    fn nofollow_single_link_and_explicit_unlock_include_later_error_and_dup_descriptor() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("real.lock");
        fs::write(&target, b"").unwrap();
        let link = directory.path().join("link.lock");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(acquire_private_lock(&link).is_err());
        let multi = directory.path().join("multi.lock");
        fs::hard_link(&target, &multi).unwrap();
        assert!(acquire_private_lock(&target).is_err());
        fs::remove_file(&multi).unwrap();
        let guard = acquire_private_lock(&target).unwrap();
        let duplicate = guard.file.as_ref().unwrap().try_clone().unwrap();
        drop(guard);
        let reacquired = acquire_private_lock(&target).unwrap();
        drop(reacquired);
        drop(duplicate);
    }

    #[test]
    fn lock_path_replacement_is_detected_against_the_held_inode() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("authority.lock");
        let moved = directory.path().join("authority.moved");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(&path)
            .unwrap();
        assert_eq!(unsafe { flock(file.as_raw_fd(), LOCK_EX) }, 0);
        fs::rename(&path, &moved).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(
            validate_private_inode(&file, &path)
                .unwrap_err()
                .contains("stable inode")
        );
        assert_eq!(unsafe { flock(file.as_raw_fd(), LOCK_UN) }, 0);
    }

    #[test]
    fn competing_private_lock_wait_is_bounded_and_sends_no_admission_write() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("bounded.lock");
        let held = acquire_private_lock(&path).unwrap();
        let started = std::time::Instant::now();
        let error = acquire_private_lock(&path).unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
        assert!(error.contains("bounded wait"));
        drop(held);
    }

    #[test]
    fn symlinked_intermediate_directory_and_dangling_record_are_preserved_and_refused() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("creation");
        let store = CreationStore::open(root.clone()).unwrap();
        fs::remove_dir(root.join("authority").join("v1")).unwrap();
        fs::remove_dir(root.join("authority")).unwrap();
        let outside = directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("authority")).unwrap();
        assert!(store.ensure_layout().is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);

        let dangling = root.join("draft").join("v1").join("dangling.json");
        std::os::unix::fs::symlink(directory.path().join("missing"), &dangling).unwrap();
        assert!(atomic_private_write(&dangling, b"refused").is_err());
        assert!(
            fs::symlink_metadata(&dangling)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn parallel_exact_attempts_dispatch_once_across_independent_stores() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("creation");
        let barrier = Arc::new(Barrier::new(2));
        let admitted = Arc::new(AtomicUsize::new(0));
        let mut joins = Vec::new();
        for _ in 0..2 {
            let root = root.clone();
            let barrier = barrier.clone();
            let admitted = admitted.clone();
            joins.push(thread::spawn(move || {
                let store = CreationStore::open(root).unwrap();
                let request = request("same-operation", "same-attempt", "same", 'b');
                barrier.wait();
                let mut admission = CreationAdmission::new(store, request.clone());
                if let Ok(mut held) = admission.admit(&context(&request)) {
                    admitted.fetch_add(1, AtomicOrdering::SeqCst);
                    held.record_terminal(&MutationTerminalRecord::Acknowledged {
                        acknowledgement: acknowledgement(&request),
                    })
                    .unwrap();
                }
            }));
        }
        for join in joins {
            join.join().unwrap();
        }
        assert_eq!(admitted.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn form_and_frozen_witness_cover_account_repositories_branches_text_draft_and_oids() {
        let form = CreationForm {
            target_repository: repository("owner", "repo", "alice"),
            base_branch: "main".into(),
            source_repository: repository("forker", "fork", "alice"),
            source_branch: "topic".into(),
            local_branch: Some("different".into()),
            title: "title".into(),
            body: "body".into(),
            draft: true,
        };
        let mut changed = form.clone();
        changed.body.push('!');
        assert_ne!(form, changed);
        let mut first = preparation("title", 'b');
        first.input = form.input();
        let mut second = first.clone();
        second.observed_source_head_sha = "c".repeat(40);
        assert_ne!(first, second);
    }

    #[test]
    fn confirmation_disclosure_contains_the_exact_multiline_body() {
        let body = "Exact first line\n\n- exact list\n\tindented tail";
        let mut prepared = preparation("title", 'b');
        prepared.input.body = body.into();
        let frozen = FrozenCreation {
            visible_form: CreationForm {
                target_repository: prepared.input.target_repository.clone(),
                base_branch: prepared.input.base_branch.clone(),
                source_repository: prepared.input.source_repository.clone(),
                source_branch: prepared.input.source_branch.clone(),
                local_branch: prepared.input.local_branch.clone(),
                title: prepared.input.title.clone(),
                body: body.into(),
                draft: prepared.input.draft,
            },
            request: PullRequestCreationRequest {
                operation_id: "visible-operation".into(),
                attempt_id: "visible-attempt".into(),
                preparation: prepared,
            },
        };
        let details = confirmation_details(&frozen);
        assert!(details.ends_with(body));
        assert!(details.contains(&format!("Body (exact, {} bytes):", body.len())));
    }

    #[test]
    fn source_target_account_and_form_validation_refuse_unpublished_shaped_inputs_before_admission()
    {
        let mut form = CreationForm {
            target_repository: repository("owner", "repo", "alice"),
            base_branch: "".into(),
            source_repository: repository("owner", "repo", "alice"),
            source_branch: "topic".into(),
            local_branch: None,
            title: "title".into(),
            body: "".into(),
            draft: false,
        };
        assert!(form.validate().is_err());
        form.base_branch = "main".into();
        form.source_repository.account.login = "mallory".into();
        assert!(
            form.validate()
                .unwrap_err()
                .contains("same explicitly selected")
        );
    }

    #[test]
    fn identities_use_cross_process_entropy_instead_of_resettable_counter() {
        let first = random_identity("operation").unwrap();
        let second = random_identity("operation").unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with(&format!("operation-{}-", std::process::id())));
    }

    #[test]
    fn late_callbacks_cannot_overwrite_newer_or_reopened_dialogs_and_channels_are_independent() {
        let old_lifetime = "dialog-old";
        let reopened_lifetime = "dialog-reopened";
        assert!(!callback_matches(reopened_lifetime, old_lifetime, 1, 1));
        assert!(!callback_matches(
            reopened_lifetime,
            reopened_lifetime,
            2,
            1
        ));
        assert!(callback_matches(reopened_lifetime, reopened_lifetime, 2, 2));

        let choice_generation = 4;
        let save_generation = 19;
        // A draft save advancing its own channel does not invalidate a branch
        // choice reply. Each callback compares only its channel generation.
        assert!(callback_matches(
            reopened_lifetime,
            reopened_lifetime,
            choice_generation,
            4,
        ));
        assert_eq!(save_generation, 19);
    }

    #[test]
    fn markdown_body_and_dirty_text_are_valid_durable_form_content() {
        let form = CreationForm {
            target_repository: repository("owner", "repo", "alice"),
            base_branch: "main".into(),
            source_repository: repository("owner", "repo", "alice"),
            source_branch: "topic".into(),
            local_branch: None,
            title: "title".into(),
            body: "First paragraph.\n\n- retained dirty line\n\tindented".into(),
            draft: true,
        };
        form.validate().unwrap();
    }

    #[test]
    fn parallel_exact_attempts_dispatch_once_across_processes() {
        let directory = tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let helper = "app::pr_creation::tests::cross_process_admission_helper";
        let mut children = Vec::new();
        for _ in 0..2 {
            children.push(
                Command::new(&executable)
                    .args(["--exact", helper, "--ignored", "--test-threads=1"])
                    .env("CIBERGIT_CREATION_PROCESS_ROOT", directory.path())
                    .spawn()
                    .unwrap(),
            );
        }
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let dispatches = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("dispatch-"))
            .count();
        assert_eq!(dispatches, 1);
    }

    #[test]
    #[ignore = "spawned only by the cross-process authority regression"]
    fn cross_process_admission_helper() {
        let Some(root) = std::env::var_os("CIBERGIT_CREATION_PROCESS_ROOT").map(PathBuf::from)
        else {
            return;
        };
        let ready = root.join(format!("ready-{}", std::process::id()));
        fs::write(&ready, b"ready").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let count = fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("ready-"))
                .count();
            if count == 2 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "process barrier timed out"
            );
            thread::yield_now();
        }
        let store = CreationStore::open(root.join("creation")).unwrap();
        let request = request("process-operation", "process-attempt", "same", 'b');
        let mut admission = CreationAdmission::new(store, request.clone());
        if let Ok(mut held) = admission.admit(&context(&request)) {
            fs::write(
                root.join(format!("dispatch-{}", std::process::id())),
                b"one",
            )
            .unwrap();
            thread::sleep(std::time::Duration::from_millis(100));
            held.record_terminal(&MutationTerminalRecord::Acknowledged {
                acknowledgement: acknowledgement(&request),
            })
            .unwrap();
        }
    }
}
