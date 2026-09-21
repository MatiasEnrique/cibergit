//! Standalone native evidence harness for the embeddable LocalWorkspace entity.
//! It creates only temporary repositories and uses an explicit private data root.

#![allow(dead_code)]
#![recursion_limit = "512"]

#[path = "../src/app/local_workspace.rs"]
mod local_workspace;

use cibergit::{
    domain::{Account, PullRequestCheckoutSource, Repository, Revision},
    local_git::{GitPath, HeadState, LocalGit, OperationState},
    rebase::{OperationState as RebaseState, PlanAction},
    worktrees::{
        AssociationKey, CheckoutAssociation, CheckoutOwnership, CheckoutView, FilesystemIdentity,
    },
};
use gpui::{
    AppContext as _, Bounds, KeyBinding, TitlebarOptions, WindowBackgroundAppearance, WindowBounds,
    WindowOptions, px, size,
};
use local_workspace::{
    LocalAction, LocalRefresh, LocalWorkspace, LocalWorkspaceAppearance, LocalWorkspaceContext,
    PrPublishContext,
};
use std::{
    borrow::Cow,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};

struct SmokeFixture {
    checkout: PathBuf,
    data_root: PathBuf,
    rebase_base_oid: String,
    publish_bare: PathBuf,
}

fn run_git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run temporary Git command");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn read_git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run temporary Git read");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("UTF-8 Git read")
        .trim()
        .to_owned()
}

fn identity(path: &Path) -> FilesystemIdentity {
    let metadata = fs::symlink_metadata(path).expect("temporary identity");
    FilesystemIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn retained_started_action(data_root: &Path) -> String {
    let actions = data_root.join("local-workspace/actions");
    let Ok(entries) = fs::read_dir(&actions) else {
        return format!("none under {}", actions.display());
    };
    for entry in entries.flatten() {
        let path = entry.path().join("started-local-action.json");
        if path.is_file() {
            return match fs::read_to_string(&path) {
                Ok(payload) => format!("{} :: {}", path.display(), payload.trim()),
                Err(error) => format!("{} :: unreadable: {error}", path.display()),
            };
        }
    }
    format!("none under {}", actions.display())
}

fn fixture(
    base: &Path,
) -> (
    Repository,
    CheckoutView,
    PathBuf,
    String,
    PullRequestCheckoutSource,
) {
    let checkout = base.join("checkout");
    let data = base.join("data");
    fs::create_dir_all(checkout.join("src")).expect("fixture directories");
    fs::create_dir_all(&data).expect("explicit data directory");
    fs::write(
        checkout.join("src/lib.rs"),
        "// unchanged tracked file\n\npub struct LocalDemo {\n    pub ready: bool,\n}\n",
    )
    .expect("fixture source");
    fs::write(checkout.join("README.md"), "# Local workspace demo\n").expect("fixture readme");
    fs::write(checkout.join("preview.png"), b"not decoded").expect("fixture media");
    run_git(&checkout, &["init", "-b", "feature/local-ui"]);
    run_git(&checkout, &["config", "user.name", "cibergit demo"]);
    run_git(&checkout, &["config", "user.email", "demo@invalid"]);
    run_git(&checkout, &["add", "."]);
    run_git(&checkout, &["commit", "-m", "base fixture"]);
    let base_oid = read_git(&checkout, &["rev-parse", "HEAD"]);
    fs::write(
        checkout.join("workflow.txt"),
        conflict_source_fixture("first"),
    )
    .expect("first replay");
    run_git(&checkout, &["add", "."]);
    run_git(&checkout, &["commit", "-m", "first replay"]);
    fs::write(
        checkout.join("workflow.txt"),
        conflict_source_fixture("second"),
    )
    .expect("second replay");
    run_git(&checkout, &["add", "."]);
    run_git(&checkout, &["commit", "-m", "second replay"]);
    fs::write(
        checkout.join("src/lib.rs"),
        "// unchanged tracked file\n\npub struct LocalDemo {\n    pub ready: bool,\n    pub lifecycle: bool,\n}\n",
    )
    .expect("tail source");
    run_git(&checkout, &["add", "."]);
    run_git(&checkout, &["commit", "-m", "tail source"]);
    let git = LocalGit::open(&checkout).expect("open temporary checkout");
    let snapshot = git.snapshot().expect("temporary snapshot");
    let checkout_root = git.root().to_owned();
    let git_dir = git.git_dir().to_owned();
    let common_git_dir = git.common_git_dir().to_owned();
    let repository = Repository {
        host: "github.com".into(),
        owner: "fixture".into(),
        name: "local-workspace".into(),
        account: Account {
            host: "github.com".into(),
            login: "demo".into(),
        },
        local_path: Some(checkout_root.clone()),
    };
    let publish_bare = base.join("pr-source.git");
    fs::create_dir_all(&publish_bare).expect("publish bare directory");
    run_git(&publish_bare, &["init", "--bare"]);
    run_git(
        &checkout,
        &[
            "remote",
            "add",
            "fork-source",
            publish_bare.to_str().expect("UTF-8 temporary path"),
        ],
    );
    run_git(
        &checkout,
        &[
            "push",
            "fork-source",
            &format!("{base_oid}:refs/heads/feature/published"),
        ],
    );
    let publish_source = PullRequestCheckoutSource {
        number: 42,
        base_repository: repository.clone(),
        source_repository: Some(Repository {
            host: "github.com".into(),
            owner: "fixture-fork".into(),
            name: "local-workspace-source".into(),
            account: repository.account.clone(),
            local_path: Some(publish_bare.clone()),
        }),
        source_branch: "feature/published".into(),
        target_branch: "main".into(),
        observed_revision: Revision {
            base_sha: base_oid.clone(),
            head_sha: base_oid.clone(),
        },
    };
    let checkout_view = CheckoutView {
        association: CheckoutAssociation {
            key: AssociationKey {
                provider: "github".into(),
                host: "github.com".into(),
                account: "demo".into(),
                repository: "fixture/local-workspace".into(),
                pull_request: 42,
            },
            path: checkout_root.clone(),
            git_dir: git_dir.clone(),
            common_git_dir: common_git_dir.clone(),
            checkout_identity: identity(&checkout_root),
            git_dir_identity: identity(&git_dir),
            common_git_dir_identity: identity(&common_git_dir),
            ownership: CheckoutOwnership::ExplicitlyAttached,
            creation: None,
            intended_remote_branch: Some("feature/local-ui".into()),
            published_head_at_association: None,
        },
        actual_head: snapshot.head,
        operation: OperationState::default(),
    };
    (repository, checkout_view, data, base_oid, publish_source)
}

fn conflict_source_fixture(prefix: &str) -> String {
    let mut text = format!("{prefix}-source-token\n");
    for row in 0..5000 {
        text.push_str(&format!("{prefix} source line {row}\n"));
    }
    text.push_str(&format!(
        "{}{prefix}-end-of-long-line-END\n",
        "long source ".repeat(100)
    ));
    text
}

fn load_fonts(cx: &gpui::App) {
    let fonts = vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/Geist-Regular.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/Geist-Medium.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/Geist-SemiBold.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/Inter-Regular.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/Inter-Medium.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/Inter-SemiBold.ttf").as_slice()),
    ];
    cx.text_system().add_fonts(fonts).expect("load UI fonts");
}

fn main() {
    let base = std::env::var_os("CIBERGIT_LOCAL_WORKSPACE_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            tempfile::Builder::new()
                .prefix("cibergit-local-workspace-demo-")
                .tempdir()
                .expect("temporary demo root")
                .keep()
        });
    let (repository, checkout, data_root, rebase_base_oid, publish_source) = fixture(&base);
    let publish_context =
        PrPublishContext::fixed_for_smoke(repository.clone(), 42, publish_source.clone());
    let checkout_root = checkout.association.path.clone();
    let smoke_data_root = data_root.clone();
    println!(
        "temporary checkout: {}",
        checkout.association.path.display()
    );
    println!("explicit data root: {}", data_root.display());
    gpui_platform::application().run(move |cx| {
        gpui_base::init(cx);
        load_fonts(cx);
        cx.bind_keys([KeyBinding::new(
            "cmd-r",
            LocalRefresh,
            Some("LocalWorkspace"),
        )]);
        let dark = std::env::var("CIBERGIT_LOCAL_WORKSPACE_APPEARANCE")
            .map(|value| value != "light")
            .unwrap_or(true);
        cx.set_window_appearance(Some(if dark {
            gpui::WindowAppearance::Dark
        } else {
            gpui::WindowAppearance::Light
        }));
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("cibergit · Local workspace component".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1440.), px(900.)),
                    cx,
                ))),
                window_background: WindowBackgroundAppearance::Blurred,
                focus: false,
                ..Default::default()
            },
            move |window, cx| {
                let workspace = cx.new(|cx| {
                    let mut workspace = LocalWorkspace::new(
                        LocalWorkspaceContext {
                            repository,
                            checkout,
                            data_root,
                            appearance: LocalWorkspaceAppearance { dark },
                        },
                        window,
                        cx,
                    );
                    workspace.set_pr_publish_context(Some(publish_context), cx);
                    workspace
                });
                if let Some(output) = std::env::var_os("CIBERGIT_LOCAL_WORKSPACE_SMOKE_DIR") {
                    start_smoke(
                        workspace.downgrade(),
                        SmokeFixture {
                            checkout: checkout_root.clone(),
                            data_root: smoke_data_root.clone(),
                            rebase_base_oid: rebase_base_oid.clone(),
                            publish_bare: publish_source
                                .source_repository
                                .as_ref()
                                .and_then(|repository| repository.local_path.clone())
                                .expect("temporary publish bare"),
                        },
                        PathBuf::from(output),
                        dark,
                        window,
                        cx,
                    );
                }
                workspace
            },
        )
        .expect("open local workspace component window");
        // Intentionally no cx.activate(): background smoke must not steal focus.
    });
}

fn start_smoke(
    workspace: gpui::WeakEntity<LocalWorkspace>,
    fixture: SmokeFixture,
    output: PathBuf,
    dark: bool,
    window: &mut gpui::Window,
    cx: &mut gpui::App,
) {
    let SmokeFixture {
        checkout,
        data_root,
        rebase_base_oid,
        publish_bare,
    } = fixture;
    window
        .spawn(cx, async move |window| {
            let started = std::time::Instant::now();
            loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let ready = window
                    .update(|_, cx| workspace.read_with(cx, |workspace, _| workspace.is_ready()).unwrap_or(false))
                    .unwrap_or(false);
                if ready || started.elapsed() > std::time::Duration::from_secs(20) {
                    break;
                }
            }
            window
                .background_executor()
                .spawn({
                    let output = output.clone();
                    async move { fs::create_dir_all(output).expect("prepare smoke evidence") }
                })
                .await;

            let publish_prepare_requested = window
                .update(|_, cx| {
                    workspace
                        .update(cx, |workspace, cx| workspace.prepare_pr_publish(cx))
                        .is_ok()
                })
                .unwrap_or(false);
            let publish_prepare_at = std::time::Instant::now();
            let publish_prepared = loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                let prepared = window
                    .update(|_, cx| {
                        workspace
                            .read_with(cx, |workspace, _| {
                                workspace.pr_publish_preparation().is_some()
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if prepared || publish_prepare_at.elapsed() > std::time::Duration::from_secs(20) {
                    break prepared;
                }
            };
            let _ = window.update(|window, _| window.resize(size(px(1440.), px(900.))));
            window.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let publish_wide_size = window.update(|window, _| window.viewport_size()).unwrap();
            assert_eq!(publish_wide_size, size(px(1440.), px(900.)), "publish wide viewport did not resize");
            let publish_ready_wide_capture = window
                .update(|window, _| {
                    window
                        .render_to_image()
                        .and_then(|image| {
                            image
                                .save(output.join(if dark {
                                    "pr-publish-ready-dark-wide.png"
                                } else {
                                    "pr-publish-ready-light-wide.png"
                                }))
                                .map_err(Into::into)
                        })
                        .is_ok()
                })
                .unwrap_or(false);
            let publish_local_oid = window
                .update(|_, cx| {
                    workspace
                        .read_with(cx, |workspace, _| {
                            workspace
                                .pr_publish_preparation()
                                .map(|preparation| preparation.local_oid.clone())
                        })
                        .ok()
                        .flatten()
                })
                .ok()
                .flatten();
            let publish_request = window
                .update(|_, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.request_prepared_pr_publish(cx)
                        })
                        .ok()
                        .flatten()
                })
                .ok()
                .flatten();
            let publish_confirmation_wide_capture = window
                .update(|window, _| {
                    window
                        .render_to_image()
                        .and_then(|image| {
                            image
                                .save(output.join(if dark {
                                    "pr-publish-confirm-dark-wide.png"
                                } else {
                                    "pr-publish-confirm-light-wide.png"
                                }))
                                .map_err(Into::into)
                        })
                        .is_ok()
                })
                .unwrap_or(false);
            let _ = window.update(|window, _| window.resize(size(px(1040.), px(820.))));
            window.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let publish_narrow_size = window.update(|window, _| window.viewport_size()).unwrap();
            assert_eq!(publish_narrow_size, size(px(1040.), px(820.)), "publish narrow viewport did not resize");
            fs::write(output.join("publish-viewport-proof.txt"), format!("wide: {publish_wide_size:?}\nnarrow: {publish_narrow_size:?}\nactual viewport sizes asserted before capture\n")).unwrap();
            let publish_confirmation_narrow_capture = window
                .update(|window, _| {
                    window
                        .render_to_image()
                        .and_then(|image| {
                            image
                                .save(output.join(if dark {
                                    "pr-publish-confirm-dark-narrow.png"
                                } else {
                                    "pr-publish-confirm-light-narrow.png"
                                }))
                                .map_err(Into::into)
                        })
                        .is_ok()
                })
                .unwrap_or(false);
            let _ = window.update(|_, cx| workspace.update(cx, |workspace, cx| {
                workspace.toggle_pr_publish_details(cx);
            }));
            window.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let actions_scroll_max = window.update(|_, cx| workspace.update(cx, |workspace, cx| {
                let maximum = workspace.smoke_scroll_local_actions_to_end();
                cx.notify();
                maximum
            })).unwrap().unwrap();
            assert!(actions_scroll_max > 0., "expanded details must expose a scrollable confirmation");
            fs::write(output.join("publish-expanded-scroll-proof.txt"), format!("maximum vertical offset: {actions_scroll_max}\nscrolled to end before expanded capture\n")).unwrap();
            window.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let publish_details_capture = window.update(|window, _| {
                window.render_to_image().and_then(|image| image.save(output.join(if dark {
                    "pr-publish-details-dark-narrow.png"
                } else {
                    "pr-publish-details-light-narrow.png"
                })).map_err(Into::into)).is_ok()
            }).unwrap_or(false);
            assert!(publish_details_capture, "expanded exact publish details capture failed");
            let _ = window.update(|_, cx| workspace.update(cx, |workspace, cx| {
                workspace.toggle_pr_publish_details(cx);
            }));
            let publish_dispatched = window
                .update(|_, cx| {
                    let Some(request_id) = publish_request else {
                        return false;
                    };
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.confirm_action(request_id, cx);
                            workspace.in_flight_action_id() == Some(request_id)
                        })
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let publish_at = std::time::Instant::now();
            let publish_completed = loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                let completed = window
                    .update(|_, cx| {
                        workspace
                            .read_with(cx, |workspace, _| {
                                publish_dispatched
                                    && workspace.in_flight_action_id().is_none()
                                    && workspace.status_message().contains("Completed Push")
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if completed || publish_at.elapsed() > std::time::Duration::from_secs(20) {
                    break completed;
                }
            };
            let published_oid = window
                .background_executor()
                .spawn({
                    let publish_bare = publish_bare.clone();
                    async move {
                        read_git(
                            &publish_bare,
                            &["rev-parse", "refs/heads/feature/published"],
                        )
                    }
                })
                .await;
            let publish_exact_readback = publish_local_oid.as_deref() == Some(&published_oid);
            let _ = window.update(|window, _| window.resize(size(px(1440.), px(900.))));

            let prepare_requested = window
                .update(|window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.toggle_rebase(cx);
                            workspace.set_rebase_base_candidate(
                                rebase_base_oid.clone(),
                                window,
                                cx,
                            );
                            workspace.prepare_rebase(cx);
                        })
                        .is_ok()
                })
                .unwrap_or(false);
            let plan_at = std::time::Instant::now();
            let plan_ready = loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                let ready = window
                    .update(|_, cx| {
                        workspace
                            .read_with(cx, |workspace, _| workspace.rebase_plan_len() == 3)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if ready || plan_at.elapsed() > std::time::Duration::from_secs(20) {
                    break ready;
                }
            };
            let _ = window.update(|window, _| window.resize(size(px(1440.), px(900.))));
            let plan_capture = window
                .update(|window, _| {
                    window
                        .render_to_image()
                        .and_then(|image| {
                            image
                                .save(output.join(if dark {
                                    "rebase-plan-dark-normal.png"
                                } else {
                                    "rebase-plan-light-normal.png"
                                }))
                                .map_err(Into::into)
                        })
                        .is_ok()
                })
                .unwrap_or(false);

            let pending_start = window
                .update(|_, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.set_rebase_step_action(0, PlanAction::Edit, cx);
                            workspace.request_start_rebase(cx);
                            let id = workspace.rebase_pending_action_id()?;
                            let inputs_locked = workspace.rebase_confirmation_inputs_locked(cx);
                            workspace.set_rebase_step_action(0, PlanAction::Drop, cx);
                            let blocked_drop_preserved_edit =
                                workspace.rebase_plan_action(0) == Some(PlanAction::Edit);
                            Some((id, inputs_locked, blocked_drop_preserved_edit))
                        })
                        .unwrap_or(None)
                })
                .unwrap_or(None);
            window
                .background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
            let pending_confirmation_capture = window
                .update(|window, _| {
                    window
                        .render_to_image()
                        .and_then(|image| {
                            image
                                .save(output.join(if dark {
                                    "rebase-confirmation-dark-normal.png"
                                } else {
                                    "rebase-confirmation-light-normal.png"
                                }))
                                .map_err(Into::into)
                        })
                        .is_ok()
                })
                .unwrap_or(false);
            let (start_edit_requested, cancel_restored_editing) = window
                .update(|_, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            let Some((id, _, _)) = pending_start else {
                                return (false, false);
                            };
                            workspace.cancel_rebase_action(id, cx);
                            let cancel_restored_editing =
                                !workspace.rebase_confirmation_inputs_locked(cx);
                            workspace.set_rebase_step_action(0, PlanAction::Drop, cx);
                            let edit_after_cancel =
                                workspace.rebase_plan_action(0) == Some(PlanAction::Drop);
                            workspace.set_rebase_step_action(0, PlanAction::Edit, cx);
                            workspace.request_start_rebase(cx);
                            let Some(reprepared_id) = workspace.rebase_pending_action_id() else {
                                return (false, cancel_restored_editing && edit_after_cancel);
                            };
                            workspace.confirm_rebase_action(reprepared_id, cx);
                            (true, cancel_restored_editing && edit_after_cancel)
                        })
                        .unwrap_or((false, false))
                })
                .unwrap_or((false, false));
            let edit_at = std::time::Instant::now();
            let edit_ready = loop {
                window.background_executor().timer(std::time::Duration::from_millis(50)).await;
                let ready = window.update(|_, cx| workspace.read_with(cx, |workspace, _| {
                    workspace.rebase_operation().is_some_and(|operation| operation.state == RebaseState::PausedForEdit)
                }).unwrap_or(false)).unwrap_or(false);
                if ready || edit_at.elapsed() > std::time::Duration::from_secs(20) { break ready; }
            };
            let _ = window.update(|window, _| window.resize(size(px(1680.), px(900.))));
            window
                .background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let edit_capture = window.update(|window, _| {
                window.render_to_image().and_then(|image| image.save(output.join(if dark { "rebase-edit-dark-wide.png" } else { "rebase-edit-light-wide.png" })).map_err(Into::into)).is_ok()
            }).unwrap_or(false);
            let edit_details_visible = window
                .update(|_, cx| {
                    workspace
                    .update(cx, |workspace, cx| {
                        workspace.set_rebase_operation_details(true, cx)
                    })
                    .is_ok()
                })
                .unwrap_or(false);
            window
                .background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
            let edit_details_capture = edit_details_visible
                && window
                    .update(|window, _| {
                        window
                            .render_to_image()
                            .and_then(|image| {
                                image
                                    .save(output.join(if dark {
                                        "rebase-operation-details-dark-wide.png"
                                    } else {
                                        "rebase-operation-details-light-wide.png"
                                    }))
                                    .map_err(Into::into)
                            })
                            .is_ok()
                    })
                    .unwrap_or(false);
            let _ = window.update(|_, cx| {
                let _ = workspace.update(cx, |workspace, cx| {
                    workspace.set_rebase_operation_details(false, cx)
                });
            });

            let continue_requested = window.update(|_, cx| workspace.update(cx, |workspace, cx| {
                workspace.request_continue_rebase(cx);
                let Some(id) = workspace.rebase_pending_action_id() else { return false; };
                workspace.confirm_rebase_action(id, cx);
                true
            }).unwrap_or(false)).unwrap_or(false);
            let completed_at = std::time::Instant::now();
            let rebase_completed = loop {
                window.background_executor().timer(std::time::Duration::from_millis(50)).await;
                let ready = window.update(|_, cx| workspace.read_with(cx, |workspace, _| {
                    workspace.rebase_operation().is_some_and(|operation| operation.state == RebaseState::Completed)
                }).unwrap_or(false)).unwrap_or(false);
                if ready || completed_at.elapsed() > std::time::Duration::from_secs(20) { break ready; }
            };
            let _ = window.update(|window, _| window.resize(size(px(1440.), px(900.))));
            window
                .background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let result_capture = window.update(|window, _| {
                window.render_to_image().and_then(|image| image.save(output.join(if dark { "rebase-result-dark-normal.png" } else { "rebase-result-light-normal.png" })).map_err(Into::into)).is_ok()
            }).unwrap_or(false);

            let archived = window.update(|_, cx| workspace.update(cx, |workspace, cx| {
                workspace.request_retire_rebase(cx);
                let Some(id) = workspace.rebase_pending_action_id() else { return false; };
                workspace.confirm_rebase_action(id, cx);
                true
            }).unwrap_or(false)).unwrap_or(false);
            let archived_at = std::time::Instant::now();
            loop {
                window.background_executor().timer(std::time::Duration::from_millis(50)).await;
                let done = window.update(|_, cx| workspace.read_with(cx, |workspace, _| workspace.rebase_operation().is_none()).unwrap_or(false)).unwrap_or(false);
                if done || archived_at.elapsed() > std::time::Duration::from_secs(20) { break; }
            }

            let conflict_requested = window.update(|_, cx| workspace.update(cx, |workspace, cx| {
                workspace.prepare_rebase(cx);
                true
            }).unwrap_or(false)).unwrap_or(false);
            let second_plan_at = std::time::Instant::now();
            loop {
                window.background_executor().timer(std::time::Duration::from_millis(50)).await;
                let ready = window.update(|_, cx| workspace.read_with(cx, |workspace, _| workspace.rebase_plan_len() == 3).unwrap_or(false)).unwrap_or(false);
                if ready || second_plan_at.elapsed() > std::time::Duration::from_secs(20) { break; }
            }
            let conflict_started = window.update(|_, cx| workspace.update(cx, |workspace, cx| {
                workspace.move_rebase_plan_step(1, -1, cx);
                workspace.set_rebase_step_action(1, PlanAction::Drop, cx);
                workspace.request_start_rebase(cx);
                let Some(id) = workspace.rebase_pending_action_id() else { return false; };
                workspace.confirm_rebase_action(id, cx);
                true
            }).unwrap_or(false)).unwrap_or(false);
            let conflict_at = std::time::Instant::now();
            let rebase_conflict = loop {
                window.background_executor().timer(std::time::Duration::from_millis(50)).await;
                let ready = window.update(|_, cx| workspace.read_with(cx, |workspace, _| {
                    workspace.rebase_operation().is_some_and(|operation| operation.state == RebaseState::Conflicted)
                        && workspace.rebase_conflict_count() == 1
                }).unwrap_or(false)).unwrap_or(false);
                if ready || conflict_at.elapsed() > std::time::Duration::from_secs(20) { break ready; }
            };
            let _ = window.update(|window, _| window.resize(size(px(1040.), px(760.))));
            window
                .background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let conflict_list_capture = window.update(|window, _| {
                window.render_to_image().and_then(|image| image.save(output.join(if dark { "rebase-conflict-dark-narrow.png" } else { "rebase-conflict-light-narrow.png" })).map_err(Into::into)).is_ok()
            }).unwrap_or(false);

            let conflict_opened = window.update(|_, cx| workspace.update(cx, |workspace, cx| {
                workspace.open_rebase_conflict_sources(0, cx);
                true
            }).unwrap_or(false)).unwrap_or(false);
            let conflict_open_at = std::time::Instant::now();
            let conflict_sources_ready = loop {
                window.background_executor().timer(std::time::Duration::from_millis(50)).await;
                let ready = window.update(|_, cx| workspace.read_with(cx, |workspace, _| {
                    workspace.rebase_conflict_source_proof().is_some()
                }).unwrap_or(false)).unwrap_or(false);
                if ready || conflict_open_at.elapsed() > std::time::Duration::from_secs(20) { break ready; }
            };
            let source_identity_proof = window.update(|_, cx| workspace.read_with(cx, |workspace, _| {
                let Some(proof) = workspace.rebase_conflict_source_proof() else { return false; };
                let labels = proof.iter().map(|(label, _, _)| label.as_str()).collect::<Vec<_>>();
                let oids = proof.iter().filter_map(|(_, oid, _)| oid.as_ref()).collect::<std::collections::BTreeSet<_>>();
                let contents = proof.iter().map(|(_, _, text)| text.as_str()).collect::<std::collections::BTreeSet<_>>();
                labels == ["Base", "Already rebased (ours)", "Replayed commit (theirs)"]
                    && oids.len() >= 2
                    && contents.len() == 3
                    && contents.iter().any(|text| text.contains("first-end-of-long-line-END"))
                    && contents.iter().any(|text| text.contains("second-end-of-long-line-END"))
            }).unwrap_or(false)).unwrap_or(false);
            let _ = window.update(|window, _| window.resize(size(px(1680.), px(940.))));
            window.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let conflict_sources_wide_capture = window.update(|window, _| {
                window.render_to_image().and_then(|image| image.save(output.join(if dark { "rebase-conflict-sources-dark-wide.png" } else { "rebase-conflict-sources-light-wide.png" })).map_err(Into::into)).is_ok()
            }).unwrap_or(false);
            let _ = window.update(|_, cx| workspace.update(cx, |workspace, cx| {
                workspace.rebase_conflict_virtualization_probe(true);
                cx.notify();
            }));
            window.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let source_virtualization = window.update(|window, cx| {
                let capture = window.render_to_image().and_then(|image| image.save(output.join(if dark { "rebase-conflict-source-end-dark.png" } else { "rebase-conflict-source-end-light.png" })).map_err(Into::into)).is_ok();
                let probes = workspace.read_with(cx, |workspace, _| workspace.rebase_conflict_virtualization_probe(false)).unwrap().unwrap();
                let valid = capture && probes.iter().filter(|(count, _, _)| *count > 5000).count() == 2
                    && probes.iter().all(|(_, batch, end)| *batch > 0 && *batch <= 64 && *end);
                fs::write(output.join("source-virtualization.txt"), format!("all source rows reachable with bounded visible rendering: {valid}\n(count, largest rendered batch, at vertical and horizontal end): {probes:?}\n")).unwrap();
                valid
            }).unwrap_or(false);
            assert!(source_virtualization, "native conflict source virtualization/end proof failed");
            let _ = window.update(|window, _| window.resize(size(px(1040.), px(820.))));
            window.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let conflict_sources_narrow_capture = window.update(|window, _| {
                window.render_to_image().and_then(|image| image.save(output.join(if dark { "rebase-conflict-sources-dark-narrow.png" } else { "rebase-conflict-sources-light-narrow.png" })).map_err(Into::into)).is_ok()
            }).unwrap_or(false);

            // The conflicted result is resolved outside cibergit: the app shows
            // the immutable stages and stages the result, but never writes it.
            window.background_executor().spawn({
                let path = checkout.join("workflow.txt");
                async move {
                    fs::write(path, "resolved-result-token\nresult-long-line-END\n")
                        .expect("external conflict resolution")
                }
            }).await;
            let result_readback = window.background_executor().spawn({
                let path = checkout.join("workflow.txt");
                async move { fs::read_to_string(path).unwrap_or_default() }
            }).await;
            let result_external_resolution = result_readback == "resolved-result-token\nresult-long-line-END\n";
            let _ = window.update(|_, cx| { let _ = workspace.update(cx, |workspace, cx| workspace.refresh_all(cx)); });
            let saved_context_at = std::time::Instant::now();
            loop {
                window.background_executor().timer(std::time::Duration::from_millis(50)).await;
                let changed = window.update(|_, cx| workspace.read_with(cx, |workspace, _| {
                    workspace.rebase_conflict_context_status() == Some("stages-changed")
                }).unwrap_or(false)).unwrap_or(false);
                if changed || saved_context_at.elapsed() > std::time::Duration::from_secs(20) { break; }
            }
            let result_identity_refreshed = window.update(|_, cx| workspace.update(cx, |workspace, cx| {
                workspace.refresh_open_rebase_conflict_sources(cx);
                workspace.rebase_conflict_context_status() == Some("current")
            }).unwrap_or(false)).unwrap_or(false);
            let stage_requested = window.update(|_, cx| workspace.update(cx, |workspace, cx| {
                workspace.request_stage_open_rebase_conflict(cx);
                let Some(id) = workspace.rebase_pending_action_id() else { return false; };
                workspace.confirm_rebase_action(id, cx);
                true
            }).unwrap_or(false)).unwrap_or(false);
            let stage_at = std::time::Instant::now();
            let staged_and_proven_resolved = loop {
                window.background_executor().timer(std::time::Duration::from_millis(50)).await;
                let resolved = window.update(|_, cx| workspace.read_with(cx, |workspace, _| {
                    workspace.rebase_conflict_count() == 0
                        && workspace.rebase_conflict_context_status() == Some("resolved-by-git")
                }).unwrap_or(false)).unwrap_or(false);
                if resolved || stage_at.elapsed() > std::time::Duration::from_secs(20) { break resolved; }
            };
            let _ = window.update(|window, _| window.resize(size(px(1440.), px(900.))));
            window.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let resolved_capture = window.update(|window, _| {
                window.render_to_image().and_then(|image| image.save(output.join(if dark { "rebase-conflict-resolved-dark.png" } else { "rebase-conflict-resolved-light.png" })).map_err(Into::into)).is_ok()
            }).unwrap_or(false);
            let continue_after_resolution = window.update(|_, cx| workspace.update(cx, |workspace, cx| {
                workspace.close_rebase_conflict_sources(cx);
                workspace.request_continue_rebase(cx);
                let Some(id) = workspace.rebase_pending_action_id() else { return false; };
                workspace.confirm_rebase_action(id, cx);
                true
            }).unwrap_or(false)).unwrap_or(false);
            let conflict_continue_at = std::time::Instant::now();
            let conflict_rebase_completed = loop {
                window.background_executor().timer(std::time::Duration::from_millis(50)).await;
                let done = window.update(|_, cx| workspace.read_with(cx, |workspace, _| {
                    workspace.rebase_operation().is_some_and(|operation| operation.state == RebaseState::Completed)
                }).unwrap_or(false)).unwrap_or(false);
                if done || conflict_continue_at.elapsed() > std::time::Duration::from_secs(20) { break done; }
            };
            let completed_result_persisted = window.background_executor().spawn({
                let path = checkout.join("workflow.txt");
                async move { fs::read_to_string(path).unwrap_or_default() }
            }).await == "resolved-result-token\nresult-long-line-END\n";
            let post_rebase_guard = window.background_executor().spawn({
                let checkout = checkout.clone();
                async move {
                    LocalGit::open(checkout)
                        .expect("post-rebase local git")
                        .snapshot()
                        .expect("post-rebase snapshot")
                        .guard
                }
            }).await;
            let _ = window.update(|_, cx| {
                let _ = workspace.update(cx, |workspace, cx| workspace.refresh_all(cx));
            });
            let post_rebase_refresh_at = std::time::Instant::now();
            loop {
                window.background_executor().timer(std::time::Duration::from_millis(50)).await;
                let refreshed = window.update(|_, cx| workspace.read_with(cx, |workspace, _| {
                    workspace.local_snapshot().is_some_and(|snapshot| snapshot.guard == post_rebase_guard)
                }).unwrap_or(false)).unwrap_or(false);
                if refreshed || post_rebase_refresh_at.elapsed() > std::time::Duration::from_secs(20) { break; }
            }
            let _ = window.update(|_, cx| { let _ = workspace.update(cx, |workspace, cx| workspace.toggle_rebase(cx)); });
            let _ = window.update(|window, _| window.resize(size(px(1440.), px(900.))));
            // An external tool edits a tracked file; cibergit only observes it.
            window
                .background_executor()
                .spawn({
                    let checkout = checkout.clone();
                    async move {
                        let path = checkout.join("src/lib.rs");
                        let mut text = fs::read_to_string(&path).expect("read smoke source");
                        text.insert_str(0, "// highlighted local edit\n");
                        let text = text.replace("ready", "verified");
                        fs::write(&path, text).expect("external smoke edit");
                        run_git(&checkout, &["add", "--", "src/lib.rs"]);
                        run_git(&checkout, &["commit", "-m", "record external smoke fixture"]);
                    }
                })
                .await;
            let readback = window
                .background_executor()
                .spawn({
                    let path = checkout.join("src/lib.rs");
                    async move { fs::read_to_string(path).unwrap_or_default() }
                })
                .await;
            let _ = window.update(|_, cx| {
                let _ = workspace.update(cx, |workspace, cx| workspace.refresh_all(cx));
            });
            window
                .background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let highlighted_capture = window
                .update(|window, _| {
                    window
                        .render_to_image()
                        .and_then(|image| {
                            image
                                .save(output.join(if dark {
                                    "highlighted-editor-dark.png"
                                } else {
                                    "highlighted-editor-light.png"
                                }))
                                .map_err(Into::into)
                        })
                        .is_ok()
                })
                .unwrap_or(false);
            let invalid_prestart_refused = window
                .update(|_, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace
                                .request_action(
                                    LocalAction::Commit {
                                        message: "   ".into(),
                                    },
                                    cx,
                                )
                                .is_none()
                                && workspace.in_flight_action_id().is_none()
                                && workspace.status_message().contains("Git was not started")
                        })
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let _ = window.update(|_, cx| {
                let _ = workspace.update(cx, |workspace, cx| workspace.refresh_all(cx));
            });
            let expected_guard = window
                .background_executor()
                .spawn({
                    let checkout = checkout.clone();
                    async move {
                        LocalGit::open(checkout)
                            .expect("open smoke checkout for guard")
                            .snapshot()
                            .expect("read smoke guard")
                            .guard
                    }
                })
                .await;
            let guard_at = std::time::Instant::now();
            loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                let current = window
                    .update(|_, cx| {
                        workspace
                            .read_with(cx, |workspace, _| {
                                workspace
                                    .local_snapshot()
                                    .is_some_and(|snapshot| snapshot.guard == expected_guard)
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if current || guard_at.elapsed() > std::time::Duration::from_secs(20) {
                    break;
                }
            }
            let clean_create_request = window
                .update(|_, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.refresh_all(cx);
                            let request_id = workspace.request_action(
                                LocalAction::CreateBranch {
                                    branch: "smoke-clean-checkout".into(),
                                    start_oid: None,
                                },
                                cx,
                            )?;
                            workspace.confirm_action(request_id, cx);
                            Some((
                                request_id,
                                workspace.in_flight_action_id() == Some(request_id),
                            ))
                        })
                        .unwrap_or(None)
                })
                .unwrap_or(None);
            let clean_refresh_confirmation_started = clean_create_request
                .is_some_and(|(_, dispatched)| dispatched);
            let clean_confirmation_at = std::time::Instant::now();
            let clean_refresh_confirmation_finished = loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                let finished = window
                    .update(|_, cx| {
                        workspace
                            .read_with(cx, |workspace, _| {
                                clean_refresh_confirmation_started
                                    && workspace.in_flight_action_id().is_none()
                                    && workspace.status_message().contains("Completed")
                                    && workspace.local_snapshot().is_some_and(|snapshot| {
                                        matches!(
                                            &snapshot.head,
                                            HeadState::Attached { branch, .. }
                                                if branch == "smoke-clean-checkout"
                                        )
                                    })
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if finished
                    || !clean_refresh_confirmation_started
                    || clean_confirmation_at.elapsed() > std::time::Duration::from_secs(20)
                {
                    break finished;
                }
            };
            let clean_create_status = window
                .update(|_, cx| {
                    workspace
                        .read_with(cx, |workspace, _| workspace.status_message().to_owned())
                        .unwrap_or_else(|error| format!("workspace read failed: {error}"))
                })
                .unwrap_or_else(|error| format!("window update failed: {error}"));
            if !clean_refresh_confirmation_finished {
                let journal = window
                    .background_executor()
                    .spawn({
                        let data_root = data_root.clone();
                        async move { retained_started_action(&data_root) }
                    })
                    .await;
                let report = format!(
                    "Local workspace native smoke\nappearance: {}\nfocus option: false; cx.activate: not called\nCreateBranch request outcome: {:?}\nCreateBranch dispatched: {}\nCreateBranch completed: {}\nCreateBranch terminal status/error: {}\nretained started-action journal: {}\nsmoke stopped after failed CreateBranch; SwitchBranch and dependent local-action probes were not run\n",
                    if dark { "dark" } else { "light" },
                    clean_create_request.map(|(request_id, _)| request_id),
                    clean_refresh_confirmation_started,
                    clean_refresh_confirmation_finished,
                    clean_create_status,
                    journal,
                );
                window
                    .background_executor()
                    .spawn({
                        let report_path = output.join(if dark {
                            "smoke-dark.txt"
                        } else {
                            "smoke-light.txt"
                        });
                        async move { fs::write(report_path, report).expect("write failed smoke report") }
                    })
                    .await;
                let _ = window.update(|_, cx| cx.quit());
                return;
            }

            let clean_switch_request = window
                .update(|_, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            let request_id = workspace.request_action(
                                LocalAction::SwitchBranch {
                                    branch: "feature/local-ui".into(),
                                },
                                cx,
                            )?;
                            workspace.confirm_action(request_id, cx);
                            Some((
                                request_id,
                                workspace.in_flight_action_id() == Some(request_id),
                            ))
                        })
                        .unwrap_or(None)
                })
                .unwrap_or(None);
            let clean_switch_dispatched =
                clean_switch_request.is_some_and(|(_, dispatched)| dispatched);
            let clean_switch_at = std::time::Instant::now();
            let clean_switch_finished = loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                let finished = window
                    .update(|_, cx| {
                        workspace
                            .read_with(cx, |workspace, _| {
                                clean_switch_dispatched
                                    && workspace.in_flight_action_id().is_none()
                                    && workspace.status_message().contains("Completed")
                                    && workspace.local_snapshot().is_some_and(|snapshot| {
                                        matches!(
                                            &snapshot.head,
                                            HeadState::Attached { branch, .. }
                                                if branch == "feature/local-ui"
                                        )
                                    })
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if finished
                    || !clean_switch_dispatched
                    || clean_switch_at.elapsed() > std::time::Duration::from_secs(20)
                {
                    break finished;
                }
            };
            let clean_switch_status = window
                .update(|_, cx| {
                    workspace
                        .read_with(cx, |workspace, _| workspace.status_message().to_owned())
                        .unwrap_or_else(|error| format!("workspace read failed: {error}"))
                })
                .unwrap_or_else(|error| format!("window update failed: {error}"));
            if !clean_switch_finished {
                let journal = window
                    .background_executor()
                    .spawn({
                        let data_root = data_root.clone();
                        async move { retained_started_action(&data_root) }
                    })
                    .await;
                let report = format!(
                    "Local workspace native smoke\nappearance: {}\nfocus option: false; cx.activate: not called\nCreateBranch request outcome: {:?}\nCreateBranch dispatched: {}\nCreateBranch completed: {}\nCreateBranch terminal status/error: {}\nSwitchBranch request outcome: {:?}\nSwitchBranch dispatched: {}\nSwitchBranch completed: {}\nSwitchBranch terminal status/error: {}\nretained started-action journal: {}\nsmoke stopped after failed clean SwitchBranch; same-current and dependent local-action probes were not run\n",
                    if dark { "dark" } else { "light" },
                    clean_create_request.map(|(request_id, _)| request_id),
                    clean_refresh_confirmation_started,
                    clean_refresh_confirmation_finished,
                    clean_create_status,
                    clean_switch_request.map(|(request_id, _)| request_id),
                    clean_switch_dispatched,
                    clean_switch_finished,
                    clean_switch_status,
                    journal,
                );
                window
                    .background_executor()
                    .spawn({
                        let report_path = output.join(if dark {
                            "smoke-dark.txt"
                        } else {
                            "smoke-light.txt"
                        });
                        async move { fs::write(report_path, report).expect("write failed smoke report") }
                    })
                    .await;
                let _ = window.update(|_, cx| cx.quit());
                return;
            }

            let same_branch_request = window
                .update(|_, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            let request_id = workspace.request_action(
                                LocalAction::SwitchBranch {
                                    branch: "feature/local-ui".into(),
                                },
                                cx,
                            )?;
                            workspace.confirm_action(request_id, cx);
                            Some((
                                request_id,
                                workspace.in_flight_action_id() == Some(request_id),
                            ))
                        })
                        .unwrap_or(None)
                })
                .unwrap_or(None);
            let same_branch_dispatched =
                same_branch_request.is_some_and(|(_, dispatched)| dispatched);
            let same_branch_at = std::time::Instant::now();
            let same_branch_finished = loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                let finished = window
                    .update(|_, cx| {
                        workspace
                            .read_with(cx, |workspace, _| {
                                same_branch_dispatched
                                    && workspace.in_flight_action_id().is_none()
                                    && workspace.status_message().contains("Completed")
                                    && workspace.local_snapshot().is_some_and(|snapshot| {
                                        matches!(
                                            &snapshot.head,
                                            HeadState::Attached { branch, .. }
                                                if branch == "feature/local-ui"
                                        )
                                    })
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if finished
                    || !same_branch_dispatched
                    || same_branch_at.elapsed() > std::time::Duration::from_secs(20)
                {
                    break finished;
                }
            };
            let same_branch_status = window
                .update(|_, cx| {
                    workspace
                        .read_with(cx, |workspace, _| workspace.status_message().to_owned())
                        .unwrap_or_else(|error| format!("workspace read failed: {error}"))
                })
                .unwrap_or_else(|error| format!("window update failed: {error}"));
            if !same_branch_finished {
                let journal = window
                    .background_executor()
                    .spawn({
                        let data_root = data_root.clone();
                        async move { retained_started_action(&data_root) }
                    })
                    .await;
                let report = format!(
                    "Local workspace native smoke\nappearance: {}\nfocus option: false; cx.activate: not called\nCreateBranch request outcome: {:?}\nCreateBranch dispatched: {}\nCreateBranch completed: {}\nCreateBranch terminal status/error: {}\nSwitchBranch request outcome: {:?}\nSwitchBranch dispatched: {}\nSwitchBranch completed: {}\nSwitchBranch terminal status/error: {}\nsame-current SwitchBranch request outcome: {:?}\nsame-current SwitchBranch dispatched: {}\nsame-current SwitchBranch completed: {}\nsame-current SwitchBranch terminal status/error: {}\nretained started-action journal: {}\nsmoke stopped after failed same-current SwitchBranch; dependent local-action probes were not run\n",
                    if dark { "dark" } else { "light" },
                    clean_create_request.map(|(request_id, _)| request_id),
                    clean_refresh_confirmation_started,
                    clean_refresh_confirmation_finished,
                    clean_create_status,
                    clean_switch_request.map(|(request_id, _)| request_id),
                    clean_switch_dispatched,
                    clean_switch_finished,
                    clean_switch_status,
                    same_branch_request.map(|(request_id, _)| request_id),
                    same_branch_dispatched,
                    same_branch_finished,
                    same_branch_status,
                    journal,
                );
                window
                    .background_executor()
                    .spawn({
                        let report_path = output.join(if dark {
                            "smoke-dark.txt"
                        } else {
                            "smoke-light.txt"
                        });
                        async move { fs::write(report_path, report).expect("write failed smoke report") }
                    })
                    .await;
                let _ = window.update(|_, cx| cx.quit());
                return;
            }
            window.background_executor().spawn({
                let readme = checkout.join("README.md");
                async move {
                    fs::write(readme, "# Local workspace demo\n\nStaged only in the disposable smoke repository.\n")
                    .expect("create local change for stage smoke");
                }
            }).await;
            let staged_fixture_guard = window
                .background_executor()
                .spawn({
                    let checkout = checkout.clone();
                    async move {
                        LocalGit::open(checkout)
                            .expect("open stage fixture checkout")
                            .snapshot()
                            .expect("read stage fixture guard")
                            .guard
                    }
                })
                .await;
            let _ = window.update(|_, cx| {
                let _ = workspace.update(cx, |workspace, cx| workspace.refresh_all(cx));
            });
            let stage_refresh_at = std::time::Instant::now();
            loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                let refreshed = window
                    .update(|_, cx| {
                        workspace
                            .read_with(cx, |workspace, _| {
                                workspace.local_snapshot().is_some_and(|snapshot| {
                                    snapshot.guard == staged_fixture_guard
                                })
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if refreshed || stage_refresh_at.elapsed() > std::time::Duration::from_secs(20) {
                    break;
                }
            }
            let confirmation = window
                .update(|_, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.request_action(
                                LocalAction::Stage(vec![
                                    GitPath::from_raw(b"README.md".to_vec()).expect("smoke path"),
                                ]),
                                cx,
                            )
                        })
                        .ok()
                        .flatten()
                })
                .unwrap_or(None);
            window
                .background_executor()
                .timer(std::time::Duration::from_millis(400))
                .await;
            let capture = window
                .update(|window, _| {
                    window
                        .render_to_image()
                        .and_then(|image| {
                            image
                                .save(output.join(if dark {
                                    "local-workspace-dark.png"
                                } else {
                                    "local-workspace-light.png"
                                }))
                                .map_err(Into::into)
                        })
                        .is_ok()
                })
                .unwrap_or(false);
            let controller_running_gate = confirmation.is_some_and(|request_id| {
                window
                    .update(|_, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                workspace.confirm_action(request_id, cx);
                                let marked_running =
                                    workspace.in_flight_action_id() == Some(request_id);
                                workspace.acknowledge_action_reconciliation(cx);
                                let acknowledgement_refused = workspace
                                    .status_message()
                                    .contains("still running");
                                let second_action = workspace.request_action(
                                    LocalAction::Stage(vec![
                                        GitPath::from_raw(b"README.md".to_vec())
                                            .expect("second smoke path"),
                                    ]),
                                    cx,
                                );
                                marked_running
                                    && acknowledgement_refused
                                    && second_action.is_none()
                                    && workspace
                                        .status_message()
                                        .contains("still running")
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            });
            let action_started = std::time::Instant::now();
            let action_finished = loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                let finished = window
                    .update(|_, cx| {
                        workspace
                            .read_with(cx, |workspace, _| {
                                workspace.in_flight_action_id().is_none()
                                    && workspace.local_snapshot().is_some_and(|snapshot| {
                                        snapshot.staged.iter().any(|entry| {
                                            entry.path.raw.as_slice() == b"README.md"
                                        })
                                    })
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if finished || action_started.elapsed() > std::time::Duration::from_secs(20) {
                    break finished;
                }
            };
            let report = format!(
                "Local workspace native smoke\nappearance: {}\nfocus option: false; cx.activate: not called\nPR source prepare requested: {}\nfresh fake-provider target prepared: {}\npublish target wide capture: {}\nimmutable publish confirmation requested: {:?}\npublish confirmation wide capture: {}\npublish confirmation narrow capture: {}\nnon-force publish dispatched: {}\nnon-force publish completed: {}\nlocal-bare exact target readback: {}\nrebase prepare requested: {}\npopulated three-commit plan ready: {}\nplan normal capture: {}\npending plan/message inputs disabled: {}\npending Drop handler preserved exact frozen Edit: {}\nfrozen plan/confirmation capture: {}\nCancel restored editing and explicit edit/reprepare: {}\nedit plan Start requested through confirmation: {}\nPausedForEdit observed: {}\nedit wide capture: {}\nexpanded operation details capture: {}\nexplicit Continue requested through confirmation: {}\nCompleted observed: {}\nresult normal capture: {}\nsafe archive requested and second prepare enabled: {}\nreordered single-conflict plan prepared: {}\nconflicting Start requested through confirmation: {}\nConflicted observed from real Git with one exact path: {}\nconflict list narrow capture: {}\nconflict source presentation opened: {}\nimmutable source proof ready: {}\nlabels, distinct source content, and exact OIDs pinned: {}\nthree source panes wide capture: {}\nexplicit source-selection narrow capture: {}\nconflicted result resolved in an external editor with exact readback: {}\nsaved result identity explicitly refreshed without replacing buffer: {}\nexact stage command requested through confirmation: {}\nGit proved the presented conflict resolved: {}\nresolved/stale-evidence capture: {}\nexplicit Continue after resolution requested: {}\nconflict rebase completed: {}\ncompleted checkout retained exact result: {}\nexternal edit observed in readback: {}\nexternal-edit capture: {}\nempty-message prestart refused with zero in-flight Git: {}\nCreateBranch request outcome: {:?}\nCreateBranch dispatched while no-op refresh pending: {}\nCreateBranch completed authoritatively: {}\nCreateBranch terminal status/error: {}\nSwitchBranch request outcome: {:?}\nSwitchBranch dispatched: {}\nSwitchBranch completed authoritatively: {}\nSwitchBranch terminal status/error: {}\nsame-current SwitchBranch request outcome: {:?}\nsame-current SwitchBranch dispatched: {}\nsame-current SwitchBranch completed authoritatively: {}\nsame-current SwitchBranch terminal status/error: {}\nmaterial action confirmation visible: {}\nin-flight acknowledgement and second action refused: {}\nconfirmed temporary-repository stage completed and refreshed: {}\nLocal Changes refreshed independently; ReviewSession imported/mutated: false\nconflict/confirmation scene capture: {}\nphysical input and desktop acrylic: not established by own-scene capture\n",
                if dark { "dark" } else { "light" },
                publish_prepare_requested,
                publish_prepared,
                publish_ready_wide_capture,
                publish_request,
                publish_confirmation_wide_capture,
                publish_confirmation_narrow_capture,
                publish_dispatched,
                publish_completed,
                publish_exact_readback,
                prepare_requested,
                plan_ready,
                plan_capture,
                pending_start.is_some_and(|(_, locked, _)| locked),
                pending_start.is_some_and(|(_, _, preserved)| preserved),
                pending_confirmation_capture,
                cancel_restored_editing,
                start_edit_requested,
                edit_ready,
                edit_capture,
                edit_details_capture,
                continue_requested,
                rebase_completed,
                result_capture,
                archived,
                conflict_requested,
                conflict_started,
                rebase_conflict,
                conflict_list_capture,
                conflict_opened,
                conflict_sources_ready,
                source_identity_proof,
                conflict_sources_wide_capture,
                conflict_sources_narrow_capture,
                result_external_resolution,
                result_identity_refreshed,
                stage_requested,
                staged_and_proven_resolved,
                resolved_capture,
                continue_after_resolution,
                conflict_rebase_completed,
                completed_result_persisted,
                readback.contains("highlighted local edit") && readback.contains("verified"),
                highlighted_capture,
                invalid_prestart_refused,
                clean_create_request.map(|(request_id, _)| request_id),
                clean_refresh_confirmation_started,
                clean_refresh_confirmation_finished,
                clean_create_status,
                clean_switch_request.map(|(request_id, _)| request_id),
                clean_switch_dispatched,
                clean_switch_finished,
                clean_switch_status,
                same_branch_request.map(|(request_id, _)| request_id),
                same_branch_dispatched,
                same_branch_finished,
                same_branch_status,
                confirmation.is_some(),
                controller_running_gate,
                action_finished,
                capture,
            );
            window
                .background_executor()
                .spawn({
                    let report_path = output.join(if dark {
                        "smoke-dark.txt"
                    } else {
                        "smoke-light.txt"
                    });
                    async move { fs::write(report_path, report).expect("write smoke report") }
                })
                .await;
            let _ = window.update(|_, cx| cx.quit());
        })
        .detach();
}
