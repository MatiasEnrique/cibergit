//! PR-tab-owned controller and durable personal relationships for the read-only
//! net remaining stack view.

use super::{DiffRow, build_rows, diff_content_width};
use cibergit::{
    domain::{Account, Repository},
    providers::GithubProvider,
    review::{DiffMode, ReviewSession},
    stacks::{
        EffectiveBoundary, NativeStackAvailability, NativeStackRead,
        PERSONAL_CORRECTIONS_SCHEMA_VERSION, PersonalStackCorrections, StackEdge,
        StackEdgeProvenance, StackLayer, StackNetSelection, StackPullRequestId, StackRepository,
        StackResolution, resolve_stack, select_local_stack_net,
    },
};
use gpui::{ListAlignment, ListState, ScrollHandle, px};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::c_int,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const CORRECTION_BYTES_LIMIT: usize = 512 * 1024;
const O_CLOEXEC: c_int = 0x0100_0000;
// Darwin O_NOFOLLOW protects the final path component. Parent directories are
// app-owned and checked as real directories before the lock or record opens.
const O_NOFOLLOW: c_int = 0x0000_0100;
const LOCK_UN: c_int = 0x08;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static CONTROLLER_SEQUENCE: AtomicU64 = AtomicU64::new(1);

unsafe extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StackRequestToken {
    controller_instance: u64,
    repository_key: String,
    account: Account,
    selected_pull_request: u64,
    generation: u64,
}

impl StackRequestToken {
    pub fn repository_key(&self) -> &str {
        &self.repository_key
    }

    pub fn selected_pull_request(&self) -> u64 {
        self.selected_pull_request
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CorrectionSnapshot {
    pub corrections: PersonalStackCorrections,
    digest: String,
}

#[derive(Clone, Debug)]
pub(crate) struct StackCorrectionWrite {
    token: StackRequestToken,
    store: PersonalCorrectionStore,
    repository: Repository,
    expected_digest: String,
    replacement: PersonalStackCorrections,
    success: String,
}

impl StackCorrectionWrite {
    pub fn execute(self) -> StackCorrectionOutcome {
        let result = self
            .store
            .compare_and_swap(&self.repository, &self.expected_digest, self.replacement)
            .map(|()| self.success);
        StackCorrectionOutcome {
            token: self.token,
            result,
        }
    }
}

pub(crate) struct StackCorrectionOutcome {
    pub token: StackRequestToken,
    pub result: Result<String, String>,
}

#[derive(Clone, Debug)]
pub(crate) struct LoadedStack {
    pub native: NativeStackRead,
    pub candidates: Vec<StackLayer>,
    pub resolution: StackResolution,
    pub selection: StackNetSelection,
    pub corrections: CorrectionSnapshot,
}

#[derive(Clone, Debug)]
pub(crate) enum StackLoadState {
    Idle,
    Loading(String),
    Ready,
    Unavailable(String),
}

impl StackLoadState {
    pub fn notice(&self) -> Option<&str> {
        match self {
            Self::Loading(notice) | Self::Unavailable(notice) => Some(notice),
            Self::Idle | Self::Ready => None,
        }
    }
}

pub(crate) struct StackViewController {
    controller_instance: u64,
    repository_key: String,
    account: Account,
    selected_pull_request: u64,
    generation: u64,
    pub visible: bool,
    pub state: StackLoadState,
    pub loaded: Option<LoadedStack>,
    pub session: Option<ReviewSession>,
    pub diff_rows: Vec<DiffRow>,
    pub diff_scroll: ListState,
    pub horizontal: ScrollHandle,
    pub diff_content_width: f32,
    pub file_scroll: gpui::UniformListScrollHandle,
    pub relationship_scroll: gpui::UniformListScrollHandle,
    pub correction_scroll: gpui::UniformListScrollHandle,
    pub correction_editor_open: bool,
    pub narrow_relationships_open: bool,
    pub feedback: Option<String>,
    store: PersonalCorrectionStore,
}

impl StackViewController {
    pub fn new(data_root: PathBuf, repository: &Repository, selected_pull_request: u64) -> Self {
        Self {
            controller_instance: CONTROLLER_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            repository_key: repository.cache_key(),
            account: repository.account.clone(),
            selected_pull_request,
            generation: 0,
            visible: false,
            state: StackLoadState::Idle,
            loaded: None,
            session: None,
            diff_rows: Vec::new(),
            diff_scroll: ListState::new(0, ListAlignment::Top, px(480.)),
            horizontal: ScrollHandle::new(),
            diff_content_width: 0.,
            file_scroll: gpui::UniformListScrollHandle::new(),
            relationship_scroll: gpui::UniformListScrollHandle::new(),
            correction_scroll: gpui::UniformListScrollHandle::new(),
            correction_editor_open: false,
            narrow_relationships_open: false,
            feedback: None,
            store: PersonalCorrectionStore::new(data_root),
        }
    }

    pub fn begin_refresh(&mut self, repository: &Repository) -> StackRequestToken {
        self.visible = true;
        self.generation = self.generation.saturating_add(1);
        self.state = StackLoadState::Loading("Reading stack relationships…".into());
        StackRequestToken {
            controller_instance: self.controller_instance,
            repository_key: repository.cache_key(),
            account: repository.account.clone(),
            selected_pull_request: self.selected_pull_request,
            generation: self.generation,
        }
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.generation = self.generation.saturating_add(1);
    }

    pub fn accepts(&self, token: &StackRequestToken) -> bool {
        self.visible
            && self.controller_instance == token.controller_instance
            && self.repository_key == token.repository_key
            && self.account == token.account
            && self.selected_pull_request == token.selected_pull_request
            && self.generation == token.generation
    }

    pub fn current_token(&self) -> Option<StackRequestToken> {
        self.visible.then(|| StackRequestToken {
            controller_instance: self.controller_instance,
            repository_key: self.repository_key.clone(),
            account: self.account.clone(),
            selected_pull_request: self.selected_pull_request,
            generation: self.generation,
        })
    }

    pub fn accept(
        &mut self,
        token: &StackRequestToken,
        result: Result<LoadedStack, String>,
        wide: bool,
    ) -> bool {
        if !self.accepts(token) {
            return false;
        }
        match result {
            Ok(loaded) => {
                let previous_head = self
                    .session
                    .as_ref()
                    .map(|session| session.revision().head_sha.clone());
                let next_head = loaded
                    .selection
                    .comparison
                    .as_ref()
                    .map(|comparison| comparison.revision.head_sha.clone());
                self.feedback = previous_head
                    .zip(next_head.as_ref())
                    .filter(|(previous, next)| previous != *next)
                    .map(|(previous, next)| {
                        format!(
                            "Explicit refresh replaced stack head {} with {}.",
                            short_sha(&previous),
                            short_sha(next)
                        )
                    });
                self.session = loaded.selection.comparison.clone().map(ReviewSession::new);
                self.loaded = Some(loaded);
                self.state = StackLoadState::Ready;
                self.rebuild(wide);
            }
            Err(error) => {
                self.state = StackLoadState::Unavailable(error);
            }
        }
        true
    }

    pub fn observe_selected_revision(&mut self, head_sha: &str) {
        let Some(rendered) = self.session.as_ref().map(|session| session.revision()) else {
            return;
        };
        if rendered.head_sha != head_sha {
            self.feedback = Some(format!(
                "Newer published code is available at {}; refresh Stack explicitly to replace the rendered pair.",
                short_sha(head_sha)
            ));
        }
    }

    pub fn rebuild(&mut self, wide: bool) {
        let Some(session) = self.session.as_ref() else {
            self.diff_rows.clear();
            self.diff_content_width = 0.;
            return;
        };
        let Some(file) = session.selected_file() else {
            self.diff_rows.clear();
            self.diff_content_width = 0.;
            return;
        };
        let mode = match session.diff_mode() {
            DiffMode::Auto if wide => DiffMode::SideBySide,
            DiffMode::Auto => DiffMode::Unified,
            mode => mode,
        };
        self.diff_rows = build_rows(cibergit::review::parse_file(file), mode);
        self.diff_content_width = diff_content_width(&self.diff_rows, mode);
        self.diff_scroll = ListState::new(
            self.diff_rows.len(),
            ListAlignment::Top,
            px(session.scroll_position()),
        );
        self.horizontal.set_offset(gpui::point(
            px(-session.horizontal_scroll_position()),
            px(0.),
        ));
    }

    pub fn capture_scroll(&mut self) {
        let vertical = self.diff_scroll.scroll_px_offset_for_scrollbar().y.as_f32();
        let horizontal = (-self.horizontal.offset().x.as_f32()).max(0.);
        if let Some(session) = self.session.as_mut() {
            session.set_scroll_position(vertical);
            session.set_horizontal_scroll_position(horizontal);
        }
    }

    pub fn select_file(&mut self, key: &str, wide: bool) -> bool {
        self.capture_scroll();
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        if !session.select_file(key) {
            return false;
        }
        self.rebuild(wide);
        true
    }

    pub fn navigate_file(&mut self, next: bool, wide: bool) -> bool {
        self.capture_scroll();
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        let changed = if next {
            session.next_file()
        } else {
            session.previous_file()
        };
        if changed {
            self.rebuild(wide);
        }
        changed
    }

    pub fn cycle_diff(&mut self, wide: bool) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        session.set_diff_mode(match session.diff_mode() {
            DiffMode::Auto => DiffMode::Unified,
            DiffMode::Unified => DiffMode::SideBySide,
            DiffMode::SideBySide => DiffMode::Auto,
        });
        self.rebuild(wide);
    }

    pub fn correction_choices(&self) -> Vec<StackPullRequestId> {
        let Some(loaded) = &self.loaded else {
            return Vec::new();
        };
        loaded
            .resolution
            .layers
            .iter()
            .map(|layer| layer.id.clone())
            .filter(|id| id.number != self.selected_pull_request)
            .take(cibergit::stacks::MAX_STACK_LAYERS)
            .collect()
    }

    pub fn prepare_set_selected_parent(
        &mut self,
        repository: &Repository,
        parent: StackPullRequestId,
    ) -> Result<StackCorrectionWrite, String> {
        let loaded = self
            .loaded
            .as_ref()
            .ok_or_else(|| "Load Stack before correcting a relationship.".to_owned())?;
        let child = StackPullRequestId {
            repository: StackRepository::from_repository(repository),
            number: self.selected_pull_request,
        };
        let mut replacement = loaded.corrections.corrections.clone();
        replacement.edges.retain(|edge| edge.child != child);
        replacement.edges.push(StackEdge {
            parent,
            child: child.clone(),
            provenance: StackEdgeProvenance::Personal,
        });
        resolve_stack(
            repository,
            &child,
            &loaded.candidates,
            &loaded.native,
            Some(&replacement),
        )
        .map_err(|error| format!("Personal relationship was not saved: {error:#}"))?;
        self.prepare_correction_write(
            repository,
            loaded.corrections.digest.clone(),
            replacement,
            "Personal relationship saved locally · refreshing Stack",
        )
    }

    pub fn prepare_remove_selected_parent(
        &mut self,
        repository: &Repository,
    ) -> Result<StackCorrectionWrite, String> {
        let loaded = self
            .loaded
            .as_ref()
            .ok_or_else(|| "Load Stack before removing a relationship.".to_owned())?;
        let child = StackPullRequestId {
            repository: StackRepository::from_repository(repository),
            number: self.selected_pull_request,
        };
        let mut replacement = loaded.corrections.corrections.clone();
        replacement.edges.retain(|edge| edge.child != child);
        self.prepare_correction_write(
            repository,
            loaded.corrections.digest.clone(),
            replacement,
            "Personal relationship removed locally · refreshing Stack",
        )
    }

    pub fn prepare_reset_corrections(
        &mut self,
        repository: &Repository,
    ) -> Result<StackCorrectionWrite, String> {
        let loaded = self
            .loaded
            .as_ref()
            .ok_or_else(|| "Load Stack before resetting relationships.".to_owned())?;
        self.prepare_correction_write(
            repository,
            loaded.corrections.digest.clone(),
            empty_corrections(repository),
            "Personal relationships reset locally · refreshing Stack",
        )
    }

    fn prepare_correction_write(
        &mut self,
        repository: &Repository,
        expected_digest: String,
        replacement: PersonalStackCorrections,
        success: &str,
    ) -> Result<StackCorrectionWrite, String> {
        if !self.visible
            || self.repository_key != repository.cache_key()
            || self.account != repository.account
        {
            return Err("Stack changed before the personal relationship could be saved.".into());
        }
        // A correction request supersedes every outstanding refresh and lazy
        // patch read. Only the durable CAS result for this exact controller
        // lifetime may trigger the subsequent refresh.
        self.generation = self.generation.saturating_add(1);
        self.feedback = Some("Saving personal relationship locally…".into());
        Ok(StackCorrectionWrite {
            token: self
                .current_token()
                .expect("visible controller has a current request token"),
            store: self.store.clone(),
            repository: repository.clone(),
            expected_digest,
            replacement,
            success: success.into(),
        })
    }

    pub fn accept_correction(
        &mut self,
        token: &StackRequestToken,
        result: &Result<String, String>,
    ) -> bool {
        if !self.accepts(token) {
            return false;
        }
        self.feedback = Some(match result {
            Ok(message) => message.clone(),
            Err(error) => error.clone(),
        });
        true
    }

    pub fn store(&self) -> PersonalCorrectionStore {
        self.store.clone()
    }
}

pub(crate) fn load_stack(
    provider: &GithubProvider,
    repository: &Repository,
    selected_pull_request: u64,
    store: &PersonalCorrectionStore,
) -> Result<LoadedStack, String> {
    let native = provider
        .native_stack(repository, selected_pull_request)
        .map_err(|error| format!("Native stack read failed: {error:#}"))?;
    // Complete native membership is independently authoritative and must not
    // be blocked by a large or unavailable repository-wide inference read.
    let candidates = if native.availability == NativeStackAvailability::Complete {
        native.layers.clone()
    } else {
        provider.stack_candidates(repository).map_err(|error| {
            let native_notice = native
                .notice
                .as_deref()
                .unwrap_or("Native stack membership is unavailable.");
            format!("{native_notice} Inferred relationships are unavailable: {error:#}")
        })?
    };
    let corrections = store.load(repository)?;
    let selected = StackPullRequestId {
        repository: StackRepository::from_repository(repository),
        number: selected_pull_request,
    };
    let resolution = resolve_stack(
        repository,
        &selected,
        &candidates,
        &native,
        Some(&corrections.corrections),
    )
    .map_err(|error| format!("Stack relationships are unavailable: {error:#}"))?;
    let tip = unique_linear_tip(&resolution, &selected)?;
    let selection = if let Some(path) = repository.local_path.as_deref() {
        select_local_stack_net(path, &resolution, &tip)
    } else {
        provider.select_stack_net(repository, &resolution, &tip)
    }
    .map_err(|error| format!("Net remaining comparison is unavailable: {error:#}"))?;
    Ok(LoadedStack {
        native,
        candidates,
        resolution,
        selection,
        corrections,
    })
}

fn unique_linear_tip(
    resolution: &StackResolution,
    selected: &StackPullRequestId,
) -> Result<StackPullRequestId, String> {
    let ids: HashSet<_> = resolution
        .layers
        .iter()
        .map(|layer| layer.id.clone())
        .collect();
    if !ids.contains(selected) {
        return Err("Selected pull request is absent from the resolved stack.".into());
    }
    let mut adjacent: HashMap<StackPullRequestId, Vec<StackPullRequestId>> = HashMap::new();
    let mut children: HashMap<StackPullRequestId, Vec<StackPullRequestId>> = HashMap::new();
    for edge in &resolution.edges {
        adjacent
            .entry(edge.parent.clone())
            .or_default()
            .push(edge.child.clone());
        adjacent
            .entry(edge.child.clone())
            .or_default()
            .push(edge.parent.clone());
        children
            .entry(edge.parent.clone())
            .or_default()
            .push(edge.child.clone());
    }
    let mut component = HashSet::new();
    let mut pending = VecDeque::from([selected.clone()]);
    while let Some(id) = pending.pop_front() {
        if !component.insert(id.clone()) {
            continue;
        }
        pending.extend(adjacent.get(&id).into_iter().flatten().cloned());
    }
    if component
        .iter()
        .any(|id| children.get(id).is_some_and(|children| children.len() > 1))
    {
        return Err(
            "This stack has sibling paths. Choosing one tip is pending a product decision; no path was selected."
                .into(),
        );
    }
    let tips = component
        .iter()
        .filter(|id| {
            children
                .get(*id)
                .is_none_or(|children| children.iter().all(|child| !component.contains(child)))
        })
        .cloned()
        .collect::<Vec<_>>();
    if tips.len() != 1 {
        return Err(
            "This stack needs an explicit tip selection. That multiple-tip choice is not available yet."
                .into(),
        );
    }
    Ok(tips[0].clone())
}

#[derive(Clone, Debug)]
pub(crate) struct PersonalCorrectionStore {
    root: PathBuf,
}

impl PersonalCorrectionStore {
    pub fn new(data_root: PathBuf) -> Self {
        Self {
            root: data_root.join("stack-corrections").join("v1"),
        }
    }

    pub fn load(&self, repository: &Repository) -> Result<CorrectionSnapshot, String> {
        let bytes = read_bounded(&self.record_path(repository)?, CORRECTION_BYTES_LIMIT)?;
        let corrections = match bytes.as_deref() {
            None => empty_corrections(repository),
            Some(bytes) => serde_json::from_slice::<PersonalStackCorrections>(bytes).map_err(
                |error| {
                    format!(
                        "Saved personal stack relationships are unreadable and were preserved: {error}"
                    )
                },
            )?,
        };
        if corrections.schema_version != PERSONAL_CORRECTIONS_SCHEMA_VERSION {
            return Err(format!(
                "Saved personal stack relationship schema {} is newer or unsupported; the original was preserved.",
                corrections.schema_version
            ));
        }
        if corrections.repository_key != repository.cache_key()
            || corrections.account != repository.account
        {
            return Err(
                "Saved personal stack relationships belong to a different account or repository; the original was preserved."
                    .into(),
            );
        }
        if corrections.edges.len() > cibergit::stacks::MAX_STACK_LAYERS {
            return Err("Saved personal stack relationships exceed the bounded limit.".into());
        }
        Ok(CorrectionSnapshot {
            digest: digest(bytes.as_deref().unwrap_or_default()),
            corrections,
        })
    }

    pub fn compare_and_swap(
        &self,
        repository: &Repository,
        expected_digest: &str,
        replacement: PersonalStackCorrections,
    ) -> Result<(), String> {
        if replacement.schema_version != PERSONAL_CORRECTIONS_SCHEMA_VERSION
            || replacement.repository_key != repository.cache_key()
            || replacement.account != repository.account
            || replacement.edges.len() > cibergit::stacks::MAX_STACK_LAYERS
        {
            return Err("Invalid personal stack relationship partition or bound.".into());
        }
        let record = self.record_path(repository)?;
        let lock = self.lock_path(repository)?;
        with_lock(&lock, || {
            let current = read_bounded(&record, CORRECTION_BYTES_LIMIT)?;
            if digest(current.as_deref().unwrap_or_default()) != expected_digest {
                return Err(
                    "Personal stack relationships changed in another window; the stale edit was not saved. Reload Stack before editing."
                        .into(),
                );
            }
            if let Some(bytes) = &current {
                let parsed = serde_json::from_slice::<PersonalStackCorrections>(bytes).map_err(
                    |error| {
                        format!(
                            "Saved personal stack relationships are unreadable and were preserved: {error}"
                        )
                    },
                )?;
                if parsed.schema_version != PERSONAL_CORRECTIONS_SCHEMA_VERSION {
                    return Err(
                        "A future personal stack relationship record was preserved; it was not overwritten."
                            .into(),
                    );
                }
            }
            let bytes = serde_json::to_vec_pretty(&replacement)
                .map_err(|error| format!("Cannot encode personal relationships: {error}"))?;
            if bytes.len() > CORRECTION_BYTES_LIMIT {
                return Err("Personal stack relationships exceed the byte bound.".into());
            }
            atomic_private_write(&record, &bytes)
        })
    }

    fn record_path(&self, repository: &Repository) -> Result<PathBuf, String> {
        Ok(self.root.join(format!("{}.json", partition(repository)?)))
    }

    fn lock_path(&self, repository: &Repository) -> Result<PathBuf, String> {
        Ok(self.root.join(format!("{}.lock", partition(repository)?)))
    }
}

fn empty_corrections(repository: &Repository) -> PersonalStackCorrections {
    PersonalStackCorrections {
        schema_version: PERSONAL_CORRECTIONS_SCHEMA_VERSION,
        repository_key: repository.cache_key(),
        account: repository.account.clone(),
        edges: Vec::new(),
    }
}

fn partition(repository: &Repository) -> Result<String, String> {
    let encoded = serde_json::to_vec(&(
        repository.cache_key(),
        repository.account.host.as_str(),
        repository.account.login.as_str(),
    ))
    .map_err(|error| format!("Cannot encode relationship partition: {error}"))?;
    Ok(digest(&encoded))
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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
            "Private relationship directory {} is not a real directory.",
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

fn open_private_lock(path: &Path) -> Result<File, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Private relationship lock has no parent.".to_owned())?;
    ensure_private_directory(parent)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(O_CLOEXEC | O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("Cannot open private relationship lock: {error}"))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("Cannot inspect opened relationship lock: {error}"))?;
    let named = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect named relationship lock: {error}"))?;
    if !opened.file_type().is_file()
        || named.file_type().is_symlink()
        || opened.dev() != named.dev()
        || opened.ino() != named.ino()
        || opened.nlink() != 1
    {
        return Err("Private relationship lock is not one stable regular link.".into());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Cannot protect private relationship lock: {error}"))?;
    Ok(file)
}

fn with_lock<T>(path: &Path, operation: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let file = open_private_lock(path)?;
    file.lock()
        .map_err(|error| format!("Cannot lock personal stack relationships: {error}"))?;
    let result = operation();
    let unlock = unsafe { flock(file.as_raw_fd(), LOCK_UN) };
    if unlock != 0 {
        let error = std::io::Error::last_os_error();
        return match result {
            Ok(_) => Err(format!(
                "Cannot unlock personal stack relationships: {error}"
            )),
            Err(primary) => Err(format!(
                "{primary} Additionally, the relationship lock could not be explicitly unlocked: {error}"
            )),
        };
    }
    result
}

fn read_bounded(path: &Path, limit: usize) -> Result<Option<Vec<u8>>, String> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(O_CLOEXEC | O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Cannot read {}: {error}", path.display())),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("Cannot inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(format!(
            "{} is not one private regular link",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    if bytes.len() > limit {
        return Err(format!("{} exceeds its {limit}-byte bound", path.display()));
    }
    Ok(Some(bytes))
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Private relationship record has no parent.".to_owned())?;
    ensure_private_directory(parent)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".cibergit-stack-{}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_CLOEXEC | O_NOFOLLOW)
            .open(&temporary)
            .map_err(|error| format!("Cannot create {}: {error}", temporary.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("Cannot write {}: {error}", temporary.display()))?;
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

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

pub(crate) fn provenance_label(provenance: StackEdgeProvenance) -> &'static str {
    match provenance {
        StackEdgeProvenance::Native => "Native",
        StackEdgeProvenance::Inferred => "Inferred",
        StackEdgeProvenance::Personal => "Personal",
    }
}

pub(crate) fn boundary_label(boundary: &EffectiveBoundary) -> String {
    match boundary {
        EffectiveBoundary::Proven { sha, .. } => {
            format!("Net remaining from {}", short_sha(sha))
        }
        EffectiveBoundary::AllMerged => "All layers are merged".into(),
        EffectiveBoundary::Unavailable { reason } => format!("Unavailable · {reason}"),
    }
}

#[cfg(feature = "ui-smoke")]
pub(crate) fn synthetic_stack_smoke(
    data_root: PathBuf,
) -> Result<
    (
        Repository,
        cibergit::domain::PullRequest,
        StackViewController,
        String,
    ),
    String,
> {
    use std::process::Command;

    let repository_path = std::env::temp_dir().join(format!(
        "cibergit-native-stack-smoke-{}",
        std::process::id()
    ));
    if repository_path.exists() {
        fs::remove_dir_all(&repository_path)
            .map_err(|error| format!("Cannot replace disposable stack fixture: {error}"))?;
    }
    fs::create_dir_all(&repository_path)
        .map_err(|error| format!("Cannot create disposable stack fixture: {error}"))?;
    let git = |arguments: &[&str]| -> Result<String, String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repository_path)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .map_err(|error| format!("Cannot run disposable Git fixture: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "Disposable Git fixture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    git(&["init", "-b", "main"])?;
    fs::write(repository_path.join("README.md"), "stack smoke base\n")
        .map_err(|error| error.to_string())?;
    git(&["add", "README.md"])?;
    git(&[
        "-c",
        "user.name=cibergit smoke",
        "-c",
        "user.email=smoke@example.invalid",
        "commit",
        "-m",
        "base",
    ])?;
    let base = git(&["rev-parse", "HEAD"])?;
    git(&["checkout", "-b", "lower"])?;
    fs::write(repository_path.join("lower.txt"), "lower layer once\n")
        .map_err(|error| error.to_string())?;
    git(&["add", "lower.txt"])?;
    git(&[
        "-c",
        "user.name=cibergit smoke",
        "-c",
        "user.email=smoke@example.invalid",
        "commit",
        "-m",
        "lower",
    ])?;
    let lower = git(&["rev-parse", "HEAD"])?;
    git(&["checkout", "-b", "upper"])?;
    let long = format!(
        "pub const STACK_LONG_LINE: &str = \"{}STACK_END_91C7\";\n",
        "linear-stack-".repeat(180)
    );
    fs::write(repository_path.join("stack.rs"), long).map_err(|error| error.to_string())?;
    fs::write(
        repository_path.join("image.bin"),
        [0_u8, 159, 146, 150, 0, 255],
    )
    .map_err(|error| error.to_string())?;
    git(&["add", "stack.rs", "image.bin"])?;
    git(&[
        "-c",
        "user.name=cibergit smoke",
        "-c",
        "user.email=smoke@example.invalid",
        "commit",
        "-m",
        "upper",
    ])?;
    let upper = git(&["rev-parse", "HEAD"])?;

    let repository = Repository {
        host: "github.com".into(),
        owner: "cibergit-smoke".into(),
        name: "linear-stack".into(),
        account: Account {
            host: "github.com".into(),
            login: "synthetic-read-only".into(),
        },
        local_path: Some(repository_path),
    };
    let stack_repository = StackRepository::from_repository(&repository);
    let layer = |number, source: &str, target: &str, base_sha: &str, head_sha: &str| StackLayer {
        id: StackPullRequestId {
            repository: stack_repository.clone(),
            number,
        },
        native_entry_id: None,
        native_position: None,
        source: cibergit::stacks::StackRef {
            repository: Some(stack_repository.clone()),
            branch: source.into(),
            oid: head_sha.into(),
        },
        target: cibergit::stacks::StackRef {
            repository: Some(stack_repository.clone()),
            branch: target.into(),
            oid: base_sha.into(),
        },
        revision: cibergit::domain::Revision {
            base_sha: base_sha.into(),
            head_sha: head_sha.into(),
        },
        state: cibergit::stacks::StackLayerState::Open,
        merged_commit_oid: None,
    };
    let layers = vec![
        layer(41, "lower", "main", &base, &lower),
        layer(42, "upper", "lower", &lower, &upper),
    ];
    let selected = layers[1].id.clone();
    let native = NativeStackRead::not_member();
    let store = PersonalCorrectionStore::new(data_root.clone());
    let first_corrections = store.load(&repository)?;
    let first_resolution = resolve_stack(
        &repository,
        &selected,
        &layers,
        &native,
        Some(&first_corrections.corrections),
    )
    .map_err(|error| error.to_string())?;
    let first_tip = unique_linear_tip(&first_resolution, &selected)?;
    let first_selection = select_local_stack_net(
        repository.local_path.as_deref().expect("fixture path"),
        &first_resolution,
        &first_tip,
    )
    .map_err(|error| error.to_string())?;
    let mut controller = StackViewController::new(data_root, &repository, selected.number);
    let first_token = controller.begin_refresh(&repository);
    controller.accept(
        &first_token,
        Ok(LoadedStack {
            native: native.clone(),
            candidates: layers.clone(),
            resolution: first_resolution,
            selection: first_selection,
            corrections: first_corrections,
        }),
        true,
    );
    controller
        .prepare_set_selected_parent(&repository, layers[0].id.clone())?
        .execute()
        .result?;
    let corrections = store.load(&repository)?;
    let resolution = resolve_stack(
        &repository,
        &selected,
        &layers,
        &native,
        Some(&corrections.corrections),
    )
    .map_err(|error| error.to_string())?;
    if !resolution
        .edges
        .iter()
        .any(|edge| edge.provenance == StackEdgeProvenance::Personal)
    {
        return Err("Synthetic correction did not remain visibly Personal.".into());
    }
    let tip = unique_linear_tip(&resolution, &selected)?;
    let selection = select_local_stack_net(
        repository.local_path.as_deref().expect("fixture path"),
        &resolution,
        &tip,
    )
    .map_err(|error| error.to_string())?;
    let token = controller.begin_refresh(&repository);
    controller.accept(
        &token,
        Ok(LoadedStack {
            native,
            candidates: layers,
            resolution,
            selection,
            corrections,
        }),
        true,
    );
    let pull_request = cibergit::domain::PullRequest {
        number: 42,
        title: "Synthetic linear stack, net remaining".into(),
        source_branch: "upper".into(),
        target_branch: "lower".into(),
        state: "OPEN".into(),
        base_sha: lower.clone(),
        head_sha: upper.clone(),
        url: "https://github.com/cibergit-smoke/linear-stack/pull/42".into(),
        ..Default::default()
    };
    let report = format!(
        "Synthetic temporary Git proof: base={base}, lower={lower}, upper={upper}\nNet files include lower.txt and stack.rs once; image.bin remains metadata-only.\nPersonal correction persisted and read back with Personal provenance.\n"
    );
    Ok((repository, pull_request, controller, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cibergit::domain::{ChangedFile, Comparison, Revision};
    use tempfile::tempdir;

    fn repository(root: Option<PathBuf>, login: &str) -> Repository {
        Repository {
            host: "github.com".into(),
            owner: "owner".into(),
            name: "repo".into(),
            account: Account {
                host: "github.com".into(),
                login: login.into(),
            },
            local_path: root,
        }
    }

    fn token_controller(root: &Path) -> (Repository, StackViewController) {
        let repo = repository(None, "alice");
        let controller = StackViewController::new(root.to_owned(), &repo, 2);
        (repo, controller)
    }

    fn loaded(repo: &Repository, store: &PersonalCorrectionStore, head: char) -> LoadedStack {
        let stack_repo = StackRepository::from_repository(repo);
        let selected = StackPullRequestId {
            repository: stack_repo.clone(),
            number: 2,
        };
        let revision = Revision {
            base_sha: "a".repeat(40),
            head_sha: head.to_string().repeat(40),
        };
        let comparison = Comparison {
            revision: revision.clone(),
            files: vec![ChangedFile {
                path: "src/long.rs".into(),
                previous_path: None,
                raw_path: None,
                raw_previous_path: None,
                status: "modified".into(),
                additions: 1,
                deletions: 1,
                patch: Some("@@ -1 +1 @@\n-old\n+new".into()),
                patch_complete: true,
            }],
            complete: true,
            notice: None,
        };
        let layer = StackLayer {
            id: selected.clone(),
            native_entry_id: None,
            native_position: None,
            source: cibergit::stacks::StackRef {
                repository: Some(stack_repo.clone()),
                branch: "feature".into(),
                oid: revision.head_sha.clone(),
            },
            target: cibergit::stacks::StackRef {
                repository: Some(stack_repo),
                branch: "main".into(),
                oid: revision.base_sha.clone(),
            },
            revision,
            state: cibergit::stacks::StackLayerState::Open,
            merged_commit_oid: None,
        };
        LoadedStack {
            native: NativeStackRead::not_member(),
            candidates: vec![layer.clone()],
            resolution: StackResolution {
                repository_key: repo.cache_key(),
                selected: selected.clone(),
                layers: vec![layer.clone()],
                edges: Vec::new(),
                native: None,
                complete: true,
                notice: None,
            },
            selection: StackNetSelection {
                selected_tip: selected,
                frozen_layers: vec![layer],
                frozen_edges: Vec::new(),
                effective_boundary: EffectiveBoundary::Proven {
                    sha: "a".repeat(40),
                    source: cibergit::stacks::EffectiveBoundarySource::RootTarget {
                        layer: StackPullRequestId {
                            repository: StackRepository::from_repository(repo),
                            number: 2,
                        },
                    },
                    proof: cibergit::stacks::BoundaryProof::LocalDirectAncestor,
                },
                comparison: Some(comparison),
            },
            corrections: store.load(repo).unwrap(),
        }
    }

    #[test]
    fn stale_reply_close_reopen_and_identity_changes_are_ignored() {
        let root = tempdir().unwrap();
        let (repo, mut controller) = token_controller(root.path());
        let old = controller.begin_refresh(&repo);
        controller.close();
        let current = controller.begin_refresh(&repo);
        let store = controller.store();
        assert!(!controller.accept(&old, Ok(loaded(&repo, &store, 'b')), true));
        assert!(controller.session.is_none());
        assert!(controller.accept(&current, Ok(loaded(&repo, &store, 'c')), true));
        assert_eq!(
            controller.session.as_ref().unwrap().revision().head_sha,
            "c".repeat(40)
        );

        let mut replacement = StackViewController::new(root.path().to_owned(), &repo, 2);
        let replacement_token = replacement.begin_refresh(&repo);
        assert!(!replacement.accept(&current, Ok(loaded(&repo, &store, 'd')), true));
        assert!(replacement.session.is_none());
        assert!(replacement.accept(&replacement_token, Ok(loaded(&repo, &store, 'e')), true));
        assert_eq!(
            replacement.session.as_ref().unwrap().revision().head_sha,
            "e".repeat(40)
        );

        let mut wrong = replacement_token;
        wrong.account.login = "bob".into();
        assert!(!replacement.accept(&wrong, Ok(loaded(&repo, &store, 'f')), true));
        assert_eq!(
            replacement.session.as_ref().unwrap().revision().head_sha,
            "e".repeat(40)
        );
    }

    #[test]
    fn correction_request_invalidates_reads_and_admits_only_its_background_result() {
        let root = tempdir().unwrap();
        let (repo, mut controller) = token_controller(root.path());
        let refresh = controller.begin_refresh(&repo);
        let store = controller.store();
        assert!(controller.accept(&refresh, Ok(loaded(&repo, &store, 'b')), true));
        let lazy = controller.current_token().unwrap();

        let write = controller.prepare_reset_corrections(&repo).unwrap();
        assert!(!controller.accepts(&lazy));
        assert!(controller.accepts(&write.token));

        let outcome = std::thread::spawn(move || write.execute()).join().unwrap();
        assert!(outcome.result.is_ok());
        assert!(controller.accept_correction(&outcome.token, &outcome.result));
    }

    #[test]
    fn branched_component_never_chooses_a_tip() {
        let repo = repository(None, "alice");
        let ids = (1..=3)
            .map(|number| StackPullRequestId {
                repository: StackRepository::from_repository(&repo),
                number,
            })
            .collect::<Vec<_>>();
        let resolution = StackResolution {
            repository_key: repo.cache_key(),
            selected: ids[0].clone(),
            layers: ids
                .iter()
                .map(|id| StackLayer {
                    id: id.clone(),
                    native_entry_id: None,
                    native_position: None,
                    source: cibergit::stacks::StackRef {
                        repository: Some(id.repository.clone()),
                        branch: format!("b{}", id.number),
                        oid: format!("{:040x}", id.number + 10),
                    },
                    target: cibergit::stacks::StackRef {
                        repository: Some(id.repository.clone()),
                        branch: "main".into(),
                        oid: format!("{:040x}", id.number),
                    },
                    revision: Revision {
                        base_sha: format!("{:040x}", id.number),
                        head_sha: format!("{:040x}", id.number + 10),
                    },
                    state: cibergit::stacks::StackLayerState::Open,
                    merged_commit_oid: None,
                })
                .collect(),
            edges: vec![
                StackEdge {
                    parent: ids[0].clone(),
                    child: ids[1].clone(),
                    provenance: StackEdgeProvenance::Native,
                },
                StackEdge {
                    parent: ids[0].clone(),
                    child: ids[2].clone(),
                    provenance: StackEdgeProvenance::Native,
                },
            ],
            native: None,
            complete: true,
            notice: None,
        };
        let error = unique_linear_tip(&resolution, &ids[0]).unwrap_err();
        assert!(error.contains("pending a product decision"));
    }

    #[test]
    fn correction_store_preserves_future_and_rejects_stale_cas() {
        let root = tempdir().unwrap();
        let repo = repository(None, "alice");
        let store = PersonalCorrectionStore::new(root.path().to_owned());
        let initial = store.load(&repo).unwrap();
        let mut first = initial.corrections.clone();
        first.edges = Vec::new();
        store
            .compare_and_swap(&repo, &initial.digest, first.clone())
            .unwrap();
        assert!(
            store
                .compare_and_swap(&repo, &initial.digest, first)
                .unwrap_err()
                .contains("changed in another window")
        );

        let record = store.record_path(&repo).unwrap();
        let before = fs::read(&record).unwrap();
        let mut future: PersonalStackCorrections = serde_json::from_slice(&before).unwrap();
        future.schema_version = PERSONAL_CORRECTIONS_SCHEMA_VERSION + 1;
        fs::write(&record, serde_json::to_vec(&future).unwrap()).unwrap();
        let future_bytes = fs::read(&record).unwrap();
        assert!(
            store
                .load(&repo)
                .unwrap_err()
                .contains("newer or unsupported")
        );
        assert_eq!(fs::read(&record).unwrap(), future_bytes);
    }

    #[test]
    fn wrong_account_partition_is_not_reused() {
        let root = tempdir().unwrap();
        let alice = repository(None, "alice");
        let bob = repository(None, "bob");
        let store = PersonalCorrectionStore::new(root.path().to_owned());
        let alice_snapshot = store.load(&alice).unwrap();
        store
            .compare_and_swap(&alice, &alice_snapshot.digest, alice_snapshot.corrections)
            .unwrap();
        assert_ne!(
            store.record_path(&alice).unwrap(),
            store.record_path(&bob).unwrap()
        );
        assert!(store.load(&bob).unwrap().corrections.edges.is_empty());
    }

    #[test]
    fn concurrent_cas_allows_exactly_one_writer() {
        use std::sync::{Arc, Barrier};

        let root = tempdir().unwrap();
        let repo = repository(None, "alice");
        let store = PersonalCorrectionStore::new(root.path().to_owned());
        let snapshot = store.load(&repo).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let handles = (0..2)
            .map(|_| {
                let store = store.clone();
                let repo = repo.clone();
                let expected = snapshot.digest.clone();
                let replacement = snapshot.corrections.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.compare_and_swap(&repo, &expected, replacement)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| result
                    .as_ref()
                    .is_err_and(|error| error.contains("changed in another window")))
                .count(),
            1
        );
    }

    #[test]
    fn correction_lock_rejects_symlink_and_multiple_links() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let repo = repository(None, "alice");
        let store = PersonalCorrectionStore::new(root.path().to_owned());
        let snapshot = store.load(&repo).unwrap();
        let lock = store.lock_path(&repo).unwrap();
        ensure_private_directory(lock.parent().unwrap()).unwrap();
        let target = root.path().join("outside.lock");
        fs::write(&target, b"").unwrap();
        symlink(&target, &lock).unwrap();
        assert!(
            store
                .compare_and_swap(&repo, &snapshot.digest, snapshot.corrections.clone())
                .unwrap_err()
                .contains("Cannot open private relationship lock")
        );
        fs::remove_file(&lock).unwrap();
        fs::hard_link(&target, &lock).unwrap();
        assert!(
            store
                .compare_and_swap(&repo, &snapshot.digest, snapshot.corrections)
                .unwrap_err()
                .contains("not one stable regular link")
        );
    }
}
