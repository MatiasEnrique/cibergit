//! PR checkout detection and setup. Published review state is never owned here.
//!
//! The common case needs no input: a worktree already on the pull request's
//! source branch is detected and offered directly. Detection is read-only and
//! offline, and it never records an association on its own — `attach` runs only
//! when the user asks for the Local Changes workspace, because `WorktreeManager`
//! has no way to undo an association once it is written.
use super::ControlPresentation;
use super::local_workspace::{
    LocalWorkspace, LocalWorkspaceAppearance, LocalWorkspaceContext, PrPublishContext,
};
use super::open_with::{self, ExternalApp};
use cibergit::ui::{self, Density, TextRole};
use cibergit::{
    domain::{PullRequest, PullRequestCheckoutSource, Repository, Revision},
    local_git::{LocalGit, WorktreeEntry},
    providers::GithubProvider,
    worktrees::{
        AssociationKey, AttachRequest, CheckoutView, CreateFromLocalRequest,
        ProvisionFromRemoteRequest, ProvisionOutcome, ReconcileOutcome, WorktreeManager,
        propose_local_branch,
    },
};
use gpui::{prelude::*, *};
use gpui_base::Button;
use std::path::PathBuf;

actions!(local_checkout, [OpenLocalChanges, ReturnToReview]);

/// A worktree already sitting on the pull request's source branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BranchCheckout {
    pub path: PathBuf,
    pub head: Option<String>,
}

/// What a read-only probe established about this pull request's local state.
#[derive(Clone, Debug)]
pub(super) enum Detection {
    Probing,
    /// cibergit already records an association for this pull request.
    Associated,
    /// Git reports these worktrees on the source branch. More than one is
    /// possible: `git worktree add --force` overrides the usual refusal, so the
    /// user picks rather than cibergit guessing.
    Found(Vec<BranchCheckout>),
    Missing,
    Failed(String),
}

/// Raised when the user asks for the full Local Changes surface, or when they
/// hand the checkout to an application. The tab owns visibility and the
/// workspace owns preferences, so each decides what to do with the request.
pub(super) enum LocalCheckoutEvent {
    OpenWorkspace,
    /// The user launched this application; its `preference_key` becomes the one
    /// the Local changes control shows.
    Launched(String),
}

impl EventEmitter<LocalCheckoutEvent> for LocalCheckout {}

pub struct LocalCheckout {
    repository: Repository,
    pull: PullRequest,
    revision: Revision,
    data_root: PathBuf,
    requested_path: Option<PathBuf>,
    workspace: Option<Entity<LocalWorkspace>>,
    source: Option<PullRequestCheckoutSource>,
    detection: Detection,
    apps: Vec<ExternalApp>,
    /// Path of the checkout currently associated, for display and for handing
    /// to an external application.
    checkout_path: Option<PathBuf>,
    attach_candidate: Option<CheckoutView>,
    busy: bool,
    notice: String,
    _subscription: Option<Subscription>,
}

impl LocalCheckout {
    pub fn new(
        repository: Repository,
        pull: PullRequest,
        revision: Revision,
        requested_path: Option<PathBuf>,
        data_root: PathBuf,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            repository,
            pull,
            revision,
            data_root,
            requested_path,
            workspace: None,
            source: None,
            detection: Detection::Probing,
            apps: open_with::installed_apps(),
            checkout_path: None,
            attach_candidate: None,
            busy: false,
            notice: String::new(),
            _subscription: None,
        };
        this.detect(cx);
        this
    }

    /// Routes the review's selected file into Local Changes. The path only
    /// selects a local diff; no worktree file is opened for editing.
    pub fn select_relative_path(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        self.requested_path = path.clone();
        if let Some(workspace) = &self.workspace {
            if !workspace.read(cx).is_ready() {
                return;
            }
            self.requested_path = None;
            workspace.update(cx, |workspace, cx| {
                workspace.refresh_all(cx);
                if let Some(path) = path {
                    workspace.select_relative_path(&path, cx);
                }
            });
        }
    }

    /// Content of the popover anchored to the Local changes control. Every row
    /// is a `Button` so it is reachable by Tab and announced as a control.
    pub(super) fn popover_body(
        &mut self,
        colors: super::Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut rows = div()
            .id("local-checkout-popover")
            .w(px(320.))
            .flex()
            .flex_col()
            .p(px(ui::MENU_INSET))
            .gap(px(ui::GAP_ICON))
            .bg(colors.elevated)
            .border_1()
            .border_color(colors.border)
            .rounded(px(ui::POPOVER_RADIUS))
            .text_color(colors.text)
            .ui_text(TextRole::Body);

        match self.detection.clone() {
            Detection::Probing => {
                rows = rows.child(self.hint(
                    format!("Looking for a checkout of {}…", self.pull.source_branch),
                    colors,
                ));
            }
            Detection::Associated => {
                let path = self.checkout_path.clone();
                rows = rows
                    .child(
                        self.hint(
                            path.as_ref()
                                .map(|path| path.display().to_string())
                                .unwrap_or_else(|| self.pull.source_branch.clone()),
                            colors,
                        ),
                    )
                    .children(path.map(|path| self.app_rows(path, colors, cx)))
                    .child(self.open_workspace_row(colors, cx));
            }
            Detection::Found(candidates) => {
                if let [only] = candidates.as_slice() {
                    let path = only.path.clone();
                    rows = rows
                        .child(self.hint(path.display().to_string(), colors))
                        .child(self.app_rows(path.clone(), colors, cx))
                        .child(self.attach_and_open_row(path, colors, cx));
                } else {
                    // `git worktree add --force` allows one branch in several
                    // worktrees, so the user chooses instead of cibergit guessing.
                    rows = rows.child(self.hint(
                        format!(
                            "{} checkouts are on {}",
                            candidates.len(),
                            self.pull.source_branch
                        ),
                        colors,
                    ));
                    for candidate in candidates {
                        let path = candidate.path.clone();
                        rows = rows.child(self.menu_row(
                            format!("pick-{}", path.display()),
                            path.display().to_string(),
                            "folder",
                            colors,
                            cx.listener(move |this, _, _, cx| {
                                this.attach_path(path.clone(), cx);
                                cx.emit(LocalCheckoutEvent::OpenWorkspace);
                            }),
                        ));
                    }
                }
            }
            Detection::Missing => {
                rows = rows
                    .child(self.hint(
                        format!("No local checkout of {}", self.pull.source_branch),
                        colors,
                    ))
                    .child(self.menu_row(
                        "create-pr-checkout".into(),
                        "Create dedicated checkout".into(),
                        "plus",
                        colors,
                        cx.listener(|this, _, _, cx| {
                            this.create(cx);
                            cx.emit(LocalCheckoutEvent::OpenWorkspace);
                        }),
                    ))
                    // An interrupted setup leaves exactly this state: no
                    // association and no worktree. Reconciling is never
                    // automatic because it can adopt a half-created checkout.
                    .child(self.menu_row(
                        "reconcile-pr-checkout".into(),
                        "Reconcile interrupted setup".into(),
                        "alert",
                        colors,
                        cx.listener(|this, _, _, cx| this.reconcile_setup(cx)),
                    ));
            }
            Detection::Failed(error) => {
                rows = rows.child(self.hint(error, colors)).child(self.menu_row(
                    "retry-detection".into(),
                    "Try again".into(),
                    "review",
                    colors,
                    cx.listener(|this, _, _, cx| this.detect(cx)),
                ));
            }
        }

        rows.child(self.menu_row(
            "choose-checkout-folder".into(),
            "Choose a folder…".into(),
            "folder",
            colors,
            cx.listener(|this, _, window, cx| this.choose_folder(window, cx)),
        ))
        .when(self.busy, |rows| {
            rows.child(self.hint("Working…".to_owned(), colors))
        })
        .into_any_element()
    }

    fn hint(&self, text: String, colors: super::Palette) -> Div {
        div()
            .px(px(ui::CELL_INSET))
            .py(px(ui::GAP_ICON))
            .ui_text(TextRole::Caption)
            .text_color(colors.muted)
            .child(text)
    }

    fn menu_row(
        &self,
        id: String,
        label: String,
        icon: &'static str,
        colors: super::Palette,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Button {
        Button::new(SharedString::from(id))
            .h(px(ui::ROW_HEIGHT))
            .px(px(ui::CELL_INSET))
            .flex()
            .items_center()
            .gap(px(ui::GAP_ICON))
            .rounded(px(ui::CONTROL_RADIUS))
            .accessibility_label(label.clone())
            .focus_ring(colors.accent, colors.selected)
            .cursor_pointer()
            .hover(|row| row.bg(colors.selected))
            .child(super::app_icon(icon, colors))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .on_click(handler)
    }

    fn app_rows(&self, path: PathBuf, colors: super::Palette, cx: &mut Context<Self>) -> Div {
        let mut list = div().flex().flex_col().gap(px(ui::GAP_ICON));
        for app in self.apps.clone() {
            let path = path.clone();
            list = list.child(self.menu_row(
                format!("open-in-{}", app.label),
                format!("Open in {}", app.label),
                app.icon,
                colors,
                cx.listener(move |this, _, _, cx| this.open_externally(app, path.clone(), cx)),
            ));
        }
        list
    }

    fn open_workspace_row(&self, colors: super::Palette, cx: &mut Context<Self>) -> Button {
        self.menu_row(
            "open-local-changes".into(),
            "Open Local Changes".into(),
            "review",
            colors,
            cx.listener(|_, _, _, cx| cx.emit(LocalCheckoutEvent::OpenWorkspace)),
        )
    }

    fn attach_and_open_row(
        &self,
        path: PathBuf,
        colors: super::Palette,
        cx: &mut Context<Self>,
    ) -> Button {
        self.menu_row(
            "open-local-changes".into(),
            "Open Local Changes".into(),
            "review",
            colors,
            cx.listener(move |this, _, _, cx| {
                this.attach_path(path.clone(), cx);
                cx.emit(LocalCheckoutEvent::OpenWorkspace);
            }),
        )
    }

    /// Launching blocks briefly, so it runs off the UI thread.
    fn open_externally(&mut self, app: ExternalApp, path: PathBuf, cx: &mut Context<Self>) {
        self.notice = format!("Opening in {}…", app.label);
        // Recorded on the request rather than on success: the choice is the
        // user's either way, and a launch that fails is still the application
        // they want next time.
        cx.emit(LocalCheckoutEvent::Launched(open_with::preference_key(
            &app,
        )));
        let task = cx.background_spawn(async move { open_with::open_in(&app, &path) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.notice = match result {
                    Ok(()) => String::new(),
                    Err(error) => error,
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose the checkout folder".into()),
        });
        let this = cx.weak_entity();
        window
            .spawn(cx, async move |window| {
                let result = paths.await;
                let _ = window.update(|_, cx| {
                    let _ = this.update(cx, |this, cx| {
                        match result {
                            Ok(Ok(Some(paths))) => {
                                if let Some(path) = paths.first() {
                                    this.attach_path(path.clone(), cx);
                                }
                            }
                            // Cancelling leaves the popover exactly as it was.
                            Ok(Ok(None)) => {}
                            _ => this.notice = "Cannot open the folder picker.".into(),
                        }
                        cx.notify();
                    });
                });
            })
            .detach();
    }

    /// Read-only, offline probe. It records nothing: a wrong guess here costs
    /// the user nothing because no association is written until they act.
    pub(super) fn detect(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.workspace.is_some() {
            return;
        }
        self.busy = true;
        self.detection = Detection::Probing;
        self.notice = String::new();
        let root = self.data_root.clone();
        let repository = self.repository.clone();
        let number = self.pull.number;
        let branch = self.pull.source_branch.clone();
        let task = cx.background_spawn(async move {
            let manager = manager(&root)?;
            let key = association_key(&repository, number);
            // An existing association always wins, offline and without probing.
            if let Some(checkout) = manager.reopen(&key).map_err(|e| e.to_string())? {
                return Ok::<_, String>((Some(checkout), Detection::Associated));
            }
            // The branch name comes from the already-loaded pull request, so
            // detection never needs a provider read.
            let Some(clone) = repository.local_path.as_ref() else {
                return Ok((None, Detection::Missing));
            };
            let git = match LocalGit::open(clone) {
                Ok(git) => git,
                // A recorded clone that has since moved is not an error worth
                // blocking on; it simply proves nothing about this branch.
                Err(_) => return Ok((None, Detection::Missing)),
            };
            let matches = branch_checkouts(git.worktrees().map_err(|e| e.to_string())?, &branch);
            Ok((
                None,
                if matches.is_empty() {
                    Detection::Missing
                } else {
                    Detection::Found(matches)
                },
            ))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok((Some(checkout), _)) => {
                        this.detection = Detection::Associated;
                        this.install(checkout, cx);
                    }
                    Ok((None, detection)) => this.detection = detection,
                    Err(error) => this.detection = Detection::Failed(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn install(&mut self, checkout: CheckoutView, cx: &mut Context<Self>) {
        // Entity construction needs a Window. Defer it to the next render; the
        // accepted checkout retains its recorded filesystem identity.
        self.checkout_path = Some(checkout.association.path.clone());
        self.detection = Detection::Associated;
        self.attach_candidate = Some(checkout);
        self.notice = "Checkout ready".into();
        cx.notify();
    }

    /// Creates cibergit's own checkout. The branch is always the collision-free
    /// `cibergit/pr-N-<digest>` name rather than the pull request's source
    /// branch: `create_from_local` refuses a branch that already exists, and a
    /// source branch absent from every worktree may still exist unchecked-out.
    pub(super) fn create(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.workspace.is_some() {
            return;
        }
        let key_for_branch = association_key(&self.repository, self.pull.number);
        let branch = propose_local_branch(&key_for_branch)
            .unwrap_or_else(|_| format!("cibergit/pr-{}", self.pull.number));
        self.busy = true;
        self.notice = "Creating the dedicated checkout at the selected review commit…".into();
        let repository = self.repository.clone();
        let key = association_key(&repository, self.pull.number);
        let root = self.data_root.clone();
        let revision = self.revision.clone();
        // The pull request already carries its source branch, so recording the
        // intended remote branch needs no provider read.
        let intended = Some(self.pull.source_branch.clone());
        let task = cx.background_spawn(async move {
            let manager = manager(&root)?;
            let outcome = if let Some(path) = &repository.local_path {
                manager.create_from_local(CreateFromLocalRequest {
                    key,
                    object_repository: path.clone(),
                    start_oid: revision.head_sha.clone(),
                    local_branch: branch,
                    intended_remote_branch: intended,
                    published_head: Some(revision.head_sha),
                })
            } else {
                manager.provision_from_remote(ProvisionFromRemoteRequest {
                    repository_url: format!(
                        "https://{}/{}.git",
                        repository.host,
                        repository.full_name()
                    ),
                    fetch_ref: format!("refs/pull/{}/head", key.pull_request),
                    key,
                    exact_head_oid: revision.head_sha.clone(),
                    local_branch: branch,
                    intended_remote_branch: intended,
                    published_head: Some(revision.head_sha),
                })
            }
            .map_err(|e| e.to_string())?;
            Ok::<_, String>(match outcome {
                ProvisionOutcome::Created(view) | ProvisionOutcome::Reused(view) => view,
            })
        });
        self.finish_checkout(task, cx);
    }

    fn reconcile_setup(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.workspace.is_some() {
            return;
        }
        self.busy = true;
        self.notice = "Checking the interrupted setup…".into();
        let root = self.data_root.clone();
        let key = association_key(&self.repository, self.pull.number);
        let task = cx.background_spawn(async move {
            manager(&root)?.reconcile(&key).map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(ReconcileOutcome::Completed(checkout)) => this.install(*checkout, cx),
                    Ok(ReconcileOutcome::NoOperation | ReconcileOutcome::Removed) => {
                        this.notice = "No interrupted setup remains. Choose a checkout to continue.".into();
                    }
                    Ok(ReconcileOutcome::PreparedNotStarted(operation) | ReconcileOutcome::Incomplete(operation)) => {
                        this.notice = format!("Setup still needs recovery. Its files are preserved at {}. No action was repeated.", operation.checkout_path.display());
                    }
                    Err(error) => this.notice = format!("Setup could not be reconciled: {error}"),
                }
                cx.notify();
            });
        }).detach();
    }

    /// Verifies `path` and records the association. This is the one place that
    /// writes an association, so it only runs from an explicit user choice.
    pub(super) fn attach_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.busy || self.workspace.is_some() {
            return;
        }
        if !path.is_absolute() {
            self.notice = "Choose an absolute checkout path.".into();
            cx.notify();
            return;
        }
        self.busy = true;
        self.notice = "Verifying this checkout…".into();
        let repository = self.repository.clone();
        let source = self
            .source
            .as_ref()
            .and_then(|s| s.source_repository.clone());
        let key = association_key(&repository, self.pull.number);
        let root = self.data_root.clone();
        let published = self.revision.head_sha.clone();
        let intended = Some(self.pull.source_branch.clone());
        let task = cx.background_spawn(async move {
            let git = LocalGit::open(&path).map_err(|e| e.to_string())?;
            let accepted_local = repository
                .local_path
                .as_ref()
                .and_then(|p| LocalGit::open(p).ok())
                .is_some_and(|accepted| accepted.common_git_dir() == git.common_git_dir());
            if !accepted_local {
                let observed = GithubProvider::new(repository.account.clone())
                    .repository(
                        path.to_str()
                            .ok_or("Checkout path cannot be sent to repository discovery")?,
                    )
                    .map_err(|e| format!("Cannot verify checkout repository: {e:#}"))?;
                if !same_repository(&observed, &repository)
                    && !source
                        .as_ref()
                        .is_some_and(|source| same_repository(&observed, source))
                {
                    return Err("This checkout belongs to a different repository.".into());
                }
            }
            manager(&root)?
                .attach(AttachRequest {
                    key,
                    checkout_path: git.root().to_owned(),
                    expected_common_git_dir: git.common_git_dir().to_owned(),
                    intended_remote_branch: intended,
                    published_head: Some(published),
                })
                .map_err(|e| e.to_string())
        });
        self.finish_checkout(task, cx);
    }

    fn finish_checkout(
        &mut self,
        task: Task<Result<CheckoutView, String>>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(view) => this.install(view, cx),
                    Err(error) => this.notice = format!("Checkout was not opened: {error}. Any interrupted operation remains recorded; no action will be replayed automatically."),
                }
                cx.notify();
            });
        }).detach();
    }
}

#[cfg(feature = "ui-smoke")]
pub(super) fn start_smoke(
    root: WeakEntity<super::Root>,
    output: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    window.spawn(cx, async move |window| {
        let mut local = None;
        let mut pinned = None;
        let mut selected = None;
        let started = std::time::Instant::now();
        while started.elapsed() < std::time::Duration::from_secs(90) {
            let opened = window.update(|window, cx| root.update(cx, |root, cx| {
                let review = &mut root.review;
                let Some(index) = review.active_tab else { return false };
                let Some(session) = &review.tabs[index].session else { return false };
                if session.selected_file().is_none() { return false; }
                // A smoke that provisions must use a temporary data directory.
                if !review.interaction_root.starts_with(std::env::temp_dir())
                    && !review.interaction_root.starts_with("/tmp") { return false; }
                pinned = Some(session.revision().clone());
                selected = session.selected_file().map(cibergit::review::file_key);
                review.open_local_changes(window, cx);
                local = review.tabs[index].local_workspace.clone()
                    .and_then(|view| view.downcast::<LocalCheckout>().ok());
                local.is_some()
            }).unwrap_or(false)).unwrap_or(false);
            if opened { break; }
            window.background_executor().timer(std::time::Duration::from_millis(100)).await;
        }
        let mut created = false;
        let mut ready = false;
        let mut reused = false;
        let started = std::time::Instant::now();
        while started.elapsed() < std::time::Duration::from_secs(240) {
            let Some(local) = &local else { break };
            ready = window.update(|_, cx| local.update(cx, |local, cx| {
                if let Some(workspace) = &local.workspace {
                    reused = !created;
                    return workspace.read(cx).is_ready();
                }
                if !local.busy && local.attach_candidate.is_none() && !created {
                    created = true;
                    local.create(cx);
                }
                false
            })).unwrap_or(false);
            if ready { break; }
            window.background_executor().timer(std::time::Duration::from_millis(100)).await;
        }
        let _ = std::fs::create_dir_all(&output);
        let captured = window.update(|window, _| {
            window.render_to_image().and_then(|image| image.save(output.join("pr-local-checkout.png")).map_err(Into::into)).is_ok()
        }).unwrap_or(false);
        let state = window.update(|_, cx| root.update(cx, |root, cx| {
            let review = &mut root.review;
            let Some(index) = review.active_tab else { return false };
            let tab = &mut review.tabs[index];
            let unchanged = tab.session.as_ref().is_some_and(|session| Some(session.revision()) == pinned.as_ref()
                && session.selected_file().map(cibergit::review::file_key) == selected);
            tab.local_visible = false;
            cx.notify();
            unchanged
        }).unwrap_or(false)).unwrap_or(false);
        window.background_executor().timer(std::time::Duration::from_millis(200)).await;
        let returned = window.update(|window, _| {
            window.render_to_image().and_then(|image| image.save(output.join("returned-published-review.png")).map_err(Into::into)).is_ok()
        }).unwrap_or(false);
        let notice = window.update(|_, cx| local.as_ref().map(|local| local.read(cx).notice.clone())).ok().flatten().unwrap_or_default();
        let ok = ready && captured && returned && state;
        let report = format!("PR local-checkout smoke\npass: {ok}\nlocal workspace ready: {ready}\ncreation requested: {created}\nexisting association reused: {reused}\nlocal scene captured: {captured}\npublished revision and selected file unchanged: {state}\nreturned review captured: {returned}\nnotice: {notice}\nRemote writes: none; local checkout creation only, explicit temporary data root.\nPhysical input/acrylic composition: not established.\n");
        let _ = std::fs::write(output.join("local-checkout-smoke.txt"), report);
        let _ = window.update(|_, cx| cx.quit());
    }).detach();
}

fn manager(root: &std::path::Path) -> Result<WorktreeManager, String> {
    WorktreeManager::open(root.join("worktree-state"), root.join("managed-checkouts"))
        .map_err(|e| e.to_string())
}

fn association_key(repository: &Repository, number: u64) -> AssociationKey {
    AssociationKey {
        provider: "github".into(),
        host: repository.host.clone(),
        account: repository.account.login.clone(),
        repository: repository.full_name(),
        pull_request: number,
    }
}

/// Worktrees sitting on `branch`, in the order Git reported them.
fn branch_checkouts(entries: Vec<WorktreeEntry>, branch: &str) -> Vec<BranchCheckout> {
    entries
        .into_iter()
        .filter(|entry| entry.on_branch(branch))
        .map(|entry| BranchCheckout {
            path: entry.path,
            head: entry.head,
        })
        .collect()
}

fn same_repository(a: &Repository, b: &Repository) -> bool {
    a.host.eq_ignore_ascii_case(&b.host)
        && a.owner.eq_ignore_ascii_case(&b.owner)
        && a.name.eq_ignore_ascii_case(&b.name)
}

impl Render for LocalCheckout {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.workspace.is_none()
            && let Some(checkout) = self.attach_candidate.take()
        {
            let context = LocalWorkspaceContext {
                repository: self.repository.clone(),
                checkout,
                data_root: self.data_root.clone(),
                appearance: LocalWorkspaceAppearance {
                    dark: matches!(
                        window.appearance(),
                        WindowAppearance::Dark | WindowAppearance::VibrantDark
                    ),
                },
            };
            let publish = PrPublishContext::github(self.repository.clone(), self.pull.number);
            let workspace = cx.new(|cx| {
                let mut workspace = LocalWorkspace::new(context, window, cx);
                workspace.set_pr_publish_context(Some(publish), cx);
                workspace
            });
            self._subscription =
                Some(
                    cx.subscribe_in(&workspace, window, |this, workspace, _, _, cx| {
                        if workspace.read(cx).is_ready()
                            && let Some(path) = this.requested_path.take()
                        {
                            workspace.update(cx, |workspace, cx| {
                                workspace.select_relative_path(&path, cx)
                            });
                        }
                    }),
                );
            self.workspace = Some(workspace);
        }
        if let Some(workspace) = &self.workspace {
            return div()
                .size_full()
                .child(workspace.clone())
                .into_any_element();
        }
        let dark = matches!(
            window.appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        );
        let colors = super::palette(dark);
        // Reaching this view without a workspace means an attach or create is
        // still running, or it failed. Choosing a checkout happens in the
        // popover, so this is a status line rather than a form.
        div()
            .id("pr-checkout-status")
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(ui::GAP_GROUP))
            .bg(colors.surface)
            .text_color(colors.muted)
            .ui_text(TextRole::Body)
            .child(if self.notice.is_empty() {
                "Opening the local checkout…".to_owned()
            } else {
                self.notice.clone()
            })
            .when(!self.busy, |view| {
                view.child(
                    Button::new("retry-pr-checkout")
                        .control()
                        .accessibility_label("Try the local checkout again")
                        .border_1()
                        .border_color(colors.border)
                        .focus_ring(colors.accent, colors.selected)
                        .cursor_pointer()
                        .hover(|button| button.bg(colors.selected))
                        .child("Try again")
                        .on_click(cx.listener(|this, _, _, cx| this.detect(cx))),
                )
            })
            .into_any_element()
    }
}
