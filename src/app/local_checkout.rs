//! Explicit PR checkout setup. Published review state is never owned here.
use super::local_workspace::{
    LocalWorkspace, LocalWorkspaceAppearance, LocalWorkspaceContext, PrPublishContext,
};
use cibergit::{
    domain::{PullRequest, PullRequestCheckoutSource, Repository, Revision},
    local_git::LocalGit,
    providers::GithubProvider,
    worktrees::{
        AssociationKey, AttachRequest, CheckoutView, CreateFromLocalRequest,
        ProvisionFromRemoteRequest, ProvisionOutcome, ReconcileOutcome, WorktreeManager,
        propose_local_branch,
    },
};
use gpui::{prelude::*, *};
use gpui_base::input::{Input, InputState};
use std::path::PathBuf;

actions!(local_checkout, [EditLocally, ReturnToReview]);

pub struct LocalCheckout {
    repository: Repository,
    pull: PullRequest,
    revision: Revision,
    data_root: PathBuf,
    requested_path: Option<PathBuf>,
    workspace: Option<Entity<LocalWorkspace>>,
    source: Option<PullRequestCheckoutSource>,
    path_input: Entity<InputState>,
    branch_input: Entity<InputState>,
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let key = association_key(&repository, pull.number);
        let branch =
            propose_local_branch(&key).unwrap_or_else(|_| format!("cibergit/pr-{}", pull.number));
        let path_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Existing checkout path"));
        let branch_input = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("Local branch name");
            input.set_value(branch, window, cx);
            input
        });
        let mut this = Self {
            repository,
            pull,
            revision,
            data_root,
            requested_path,
            workspace: None,
            source: None,
            path_input,
            branch_input,
            attach_candidate: None,
            busy: false,
            notice: String::new(),
            _subscription: None,
        };
        this.reopen(cx);
        this
    }

    pub fn open_relative_path(
        &mut self,
        path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.requested_path = path.clone();
        if let Some(workspace) = &self.workspace {
            if !workspace.read(cx).is_ready() {
                return;
            }
            self.requested_path = None;
            workspace.update(cx, |workspace, cx| {
                workspace.refresh_all(cx);
                if let Some(path) = path {
                    workspace.open_relative_path(path, window, cx);
                }
            });
        }
    }

    fn reopen(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.notice = "Checking the saved checkout…".into();
        let root = self.data_root.clone();
        let repository = self.repository.clone();
        let number = self.pull.number;
        let task = cx.background_spawn(async move {
            let manager = manager(&root)?;
            let checkout = manager
                .reopen(&association_key(&repository, number))
                .map_err(|e| e.to_string())?;
            // An associated checkout can reopen offline without an API read.
            let source = if checkout.is_none() {
                Some(
                    GithubProvider::new(repository.account.clone())
                        .checkout_source(&repository, number)
                        .map_err(|e| format!("Source metadata unavailable: {e:#}")),
                )
            } else {
                None
            };
            Ok::<_, String>((checkout, source))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok((Some(checkout), _)) => this.install(checkout, cx),
                    Ok((None, source)) => {
                        this.notice = match source {
                            Some(Ok(source)) => {
                                this.source = Some(source);
                                "Choose where to edit this PR.".into()
                            }
                            Some(Err(error)) => error,
                            None => "Choose where to edit this PR.".into(),
                        };
                    }
                    Err(error) => this.notice = format!("Saved checkout needs attention: {error}"),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn install(&mut self, checkout: CheckoutView, cx: &mut Context<Self>) {
        // Entity construction needs a Window. Defer it to the next render; the
        // accepted checkout retains its recorded filesystem identity.
        self.attach_candidate = Some(checkout);
        self.notice = "Checkout ready".into();
        cx.notify();
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.workspace.is_some() {
            return;
        }
        let branch = self.branch_input.read(cx).value().to_string();
        if branch.trim().is_empty() {
            self.notice = "Enter a local branch name.".into();
            cx.notify();
            return;
        }
        self.busy = true;
        self.notice = "Creating the dedicated checkout at the selected review commit…".into();
        let repository = self.repository.clone();
        let key = association_key(&repository, self.pull.number);
        let root = self.data_root.clone();
        let revision = self.revision.clone();
        let intended = self.source.as_ref().map(|s| s.source_branch.clone());
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

    fn inspect_attachment(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.workspace.is_some() {
            return;
        }
        let path = PathBuf::from(self.path_input.read(cx).value().to_string());
        if !path.is_absolute() {
            self.notice = "Enter an absolute checkout path.".into();
            cx.notify();
            return;
        }
        self.busy = true;
        self.notice = "Verifying this existing checkout…".into();
        let repository = self.repository.clone();
        let source = self
            .source
            .as_ref()
            .and_then(|s| s.source_repository.clone());
        let key = association_key(&repository, self.pull.number);
        let root = self.data_root.clone();
        let published = self.revision.head_sha.clone();
        let intended = self.source.as_ref().map(|s| s.source_branch.clone());
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
                let super::Root::Review(review) = root else { return false };
                let Some(index) = review.active_tab else { return false };
                let Some(session) = &review.tabs[index].session else { return false };
                if session.selected_file().is_none() { return false; }
                // A smoke that provisions must use a temporary data directory.
                if !review.interaction_root.starts_with(std::env::temp_dir())
                    && !review.interaction_root.starts_with("/tmp") { return false; }
                pinned = Some(session.revision().clone());
                selected = session.selected_file().map(cibergit::review::file_key);
                review.edit_locally(window, cx);
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
                    return workspace.read(cx).is_ready() && workspace.read(cx).active_path().is_some();
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
            let super::Root::Review(review) = root else { return false };
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
        let report = format!("PR local-checkout smoke\npass: {ok}\nlocal editor ready with selected file: {ready}\ncreation requested: {created}\nexisting association reused: {reused}\nlocal scene captured: {captured}\npublished revision and selected file unchanged: {state}\nreturned review captured: {returned}\nnotice: {notice}\nRemote writes: none; local checkout creation only, explicit temporary data root.\nPhysical input/acrylic composition: not established.\n");
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
                    cx.subscribe_in(&workspace, window, |this, workspace, _, window, cx| {
                        if workspace.read(cx).is_ready()
                            && let Some(path) = this.requested_path.take()
                        {
                            workspace.update(cx, |workspace, cx| {
                                workspace.open_relative_path(path, window, cx)
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
        div().size_full().flex().flex_col().p_6().gap_4().bg(colors.surface).text_color(colors.text)
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Edit this pull request locally"))
            .child(format!("{} #{} · Review commit {}", self.repository.full_name(), self.pull.number, &self.revision.head_sha[..self.revision.head_sha.len().min(12)]))
            .when_some(self.source.as_ref(), |view, source| view.child(format!("PR source: {} · {}",
                source.source_repository.as_ref().map(Repository::full_name).unwrap_or_else(|| "repository unavailable".into()), source.source_branch)))
            .child(div().text_color(colors.muted).child("A dedicated checkout keeps local edits separate. Existing checkouts are attached only when you choose them. Closing this tab keeps the checkout and recovery files."))
            .child(div().h(px(36.)).flex_shrink_0().child(Input::new(&self.branch_input)))
            .child(div().id("create-pr-checkout").px_3().py_2().rounded_md().bg(colors.selected).cursor_pointer()
                .child(if self.busy { "Working…" } else { "Create dedicated checkout" })
                .on_click(cx.listener(|this, _, _, cx| this.create(cx))))
            .child(div().h(px(36.)).flex_shrink_0().child(Input::new(&self.path_input)))
            .child(div().id("attach-pr-checkout").px_3().py_2().rounded_md().bg(colors.selected).cursor_pointer()
                .child("Verify and attach existing checkout")
                .on_click(cx.listener(|this, _, _, cx| this.inspect_attachment(cx))))
            .child(div().id("reconcile-pr-checkout").px_3().py_2().rounded_md().cursor_pointer()
                .child("Reconcile interrupted setup")
                .on_click(cx.listener(|this, _, _, cx| this.reconcile_setup(cx))))
            .child(div().text_sm().text_color(colors.muted).child(self.notice.clone()))
            .into_any_element()
    }
}
