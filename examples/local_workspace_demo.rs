//! Standalone native evidence harness for the embeddable LocalWorkspace entity.
//! It creates only temporary repositories and uses an explicit private data root.

#![allow(dead_code)]
#![recursion_limit = "512"]

#[path = "../src/app/local_workspace.rs"]
mod local_workspace;

use cibergit::{
    document::DocumentStatus,
    domain::{Account, Repository},
    local_git::{GitPath, LocalGit, OperationState},
    rebase::{OperationState as RebaseState, PlanAction},
    worktrees::{
        AssociationKey, CheckoutAssociation, CheckoutOwnership, CheckoutView, FilesystemIdentity,
    },
};
use gpui::{
    AppContext as _, Bounds, Focusable, KeyBinding, TitlebarOptions, WindowBackgroundAppearance,
    WindowBounds, WindowOptions, px, size,
};
use local_workspace::{
    LocalAction, LocalFind, LocalRefresh, LocalReplace, LocalSave, LocalWorkspace,
    LocalWorkspaceAppearance, LocalWorkspaceContext,
};
use std::{
    borrow::Cow,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};

fn run_git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("run temporary Git command");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn identity(path: &Path) -> FilesystemIdentity {
    let metadata = fs::symlink_metadata(path).expect("temporary identity");
    FilesystemIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn fixture(base: &Path) -> (Repository, CheckoutView, PathBuf, String) {
    let checkout = base.join("checkout");
    let data = base.join("data");
    fs::create_dir_all(checkout.join("src")).expect("fixture directories");
    fs::create_dir_all(&data).expect("explicit data directory");
    fs::write(
        checkout.join("src/lib.rs"),
        "// unchanged file visible in quick-open\n\npub struct LocalDemo {\n    pub ready: bool,\n}\n",
    )
    .expect("fixture source");
    fs::write(checkout.join("README.md"), "# Local workspace demo\n").expect("fixture readme");
    fs::write(checkout.join("preview.png"), b"not decoded").expect("fixture media");
    run_git(&checkout, &["init", "-b", "feature/local-ui"]);
    run_git(&checkout, &["config", "user.name", "cibergit demo"]);
    run_git(&checkout, &["config", "user.email", "demo@invalid"]);
    run_git(&checkout, &["add", "."]);
    run_git(&checkout, &["commit", "-m", "base fixture"]);
    let base_oid = Command::new("git")
        .current_dir(&checkout)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("read base")
        .stdout;
    let base_oid = String::from_utf8(base_oid)
        .expect("base UTF-8")
        .trim()
        .to_owned();
    fs::write(checkout.join("workflow.txt"), "first replay\n").expect("first replay");
    run_git(&checkout, &["add", "."]);
    run_git(&checkout, &["commit", "-m", "first replay"]);
    fs::write(checkout.join("workflow.txt"), "second replay\n").expect("second replay");
    run_git(&checkout, &["add", "."]);
    run_git(&checkout, &["commit", "-m", "second replay"]);
    fs::write(
        checkout.join("src/lib.rs"),
        "// unchanged file visible in quick-open\n\npub struct LocalDemo {\n    pub ready: bool,\n    pub lifecycle: bool,\n}\n",
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
    (repository, checkout_view, data, base_oid)
}

fn load_fonts(cx: &gpui::App) {
    let fonts = vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-Medium.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-SemiBold.ttf").as_slice()),
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
    let (repository, checkout, data_root, rebase_base_oid) = fixture(&base);
    let checkout_root = checkout.association.path.clone();
    println!(
        "temporary checkout: {}",
        checkout.association.path.display()
    );
    println!("explicit data root: {}", data_root.display());
    gpui_platform::application().run(move |cx| {
        gpui_base::init(cx);
        load_fonts(cx);
        cx.bind_keys([
            KeyBinding::new("cmd-s", LocalSave, Some("LocalWorkspace")),
            KeyBinding::new("cmd-r", LocalRefresh, Some("LocalWorkspace")),
            KeyBinding::new("cmd-f", LocalFind, Some("LocalWorkspace")),
            KeyBinding::new("cmd-alt-f", LocalReplace, Some("LocalWorkspace")),
        ]);
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
                    LocalWorkspace::new(
                        LocalWorkspaceContext {
                            repository,
                            checkout,
                            data_root,
                            appearance: LocalWorkspaceAppearance { dark },
                        },
                        window,
                        cx,
                    )
                });
                if let Some(output) = std::env::var_os("CIBERGIT_LOCAL_WORKSPACE_SMOKE_DIR") {
                    start_smoke(
                        workspace.downgrade(),
                        checkout_root.clone(),
                        rebase_base_oid.clone(),
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
    checkout: PathBuf,
    rebase_base_oid: String,
    output: PathBuf,
    dark: bool,
    window: &mut gpui::Window,
    cx: &mut gpui::App,
) {
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

            let start_edit_requested = window
                .update(|_, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.set_rebase_step_action(0, PlanAction::Edit, cx);
                            workspace.request_start_rebase(cx);
                            let Some(id) = workspace.rebase_pending_action_id() else {
                                return false;
                            };
                            workspace.confirm_rebase_action(id, cx);
                            true
                        })
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let edit_at = std::time::Instant::now();
            let edit_ready = loop {
                window.background_executor().timer(std::time::Duration::from_millis(50)).await;
                let ready = window.update(|_, cx| workspace.read_with(cx, |workspace, _| {
                    workspace.rebase_operation().is_some_and(|operation| operation.state == RebaseState::PausedForEdit)
                }).unwrap_or(false)).unwrap_or(false);
                if ready || edit_at.elapsed() > std::time::Duration::from_secs(20) { break ready; }
            };
            let _ = window.update(|window, _| window.resize(size(px(1680.), px(900.))));
            let edit_capture = window.update(|window, _| {
                window.render_to_image().and_then(|image| image.save(output.join(if dark { "rebase-edit-dark-wide.png" } else { "rebase-edit-light-wide.png" })).map_err(Into::into)).is_ok()
            }).unwrap_or(false);

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
                }).unwrap_or(false)).unwrap_or(false);
                if ready || conflict_at.elapsed() > std::time::Duration::from_secs(20) { break ready; }
            };
            let _ = window.update(|window, _| window.resize(size(px(1040.), px(760.))));
            let conflict_capture = window.update(|window, _| {
                window.render_to_image().and_then(|image| image.save(output.join(if dark { "rebase-conflict-dark-narrow.png" } else { "rebase-conflict-light-narrow.png" })).map_err(Into::into)).is_ok()
            }).unwrap_or(false);
            let abort_requested = window.update(|_, cx| workspace.update(cx, |workspace, cx| {
                workspace.request_abort_rebase(cx);
                let Some(id) = workspace.rebase_pending_action_id() else { return false; };
                workspace.confirm_rebase_action(id, cx);
                true
            }).unwrap_or(false)).unwrap_or(false);
            let abort_at = std::time::Instant::now();
            loop {
                window.background_executor().timer(std::time::Duration::from_millis(50)).await;
                let done = window.update(|_, cx| workspace.read_with(cx, |workspace, _| {
                    workspace.rebase_operation().is_some_and(|operation| operation.state == RebaseState::Aborted)
                }).unwrap_or(false)).unwrap_or(false);
                if done || abort_at.elapsed() > std::time::Duration::from_secs(20) { break; }
            }
            window.background_executor().spawn({
                let readme = checkout.join("README.md");
                async move {
                    fs::write(readme, "# Local workspace demo\n\nStaged only in the disposable smoke repository.\n")
                        .expect("create local change for stage smoke");
                }
            }).await;
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
            let opened = window
                .update(|window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.open_relative_path("src/lib.rs", window, cx);
                        })
                        .is_ok()
                })
                .unwrap_or(false);
            let opened_at = std::time::Instant::now();
            loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let has_editor = window
                    .update(|_, cx| {
                        workspace
                            .read_with(cx, |workspace, _| workspace.active_editor().is_some())
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if has_editor || opened_at.elapsed() > std::time::Duration::from_secs(20) {
                    break;
                }
            }
            let edited = window
                .update(|window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            let Some(editor) = workspace.active_editor() else {
                                return false;
                            };
                            editor.update(cx, |editor, cx| {
                                editor.focus(window, cx);
                                editor.insert("// highlighted local edit\n", window, cx);
                                editor.open_search(true, cx);
                                editor.set_search_query("ready", true, cx);
                                assert_eq!(
                                    editor.replace_all_search_matches("verified", window, cx),
                                    1
                                );
                            });
                            true
                        })
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            for action in [
                &gpui_base::input::Undo as &dyn gpui::Action,
                &gpui_base::input::Redo as &dyn gpui::Action,
            ] {
                let _ = window.update(|window, cx| {
                    let focus = workspace
                        .read_with(cx, |workspace, cx| {
                            workspace
                                .active_editor()
                                .map(|editor| editor.read(cx).focus_handle(cx).clone())
                        })
                        .ok()
                        .flatten();
                    if let Some(focus) = focus {
                        focus.dispatch_action(action, window, cx);
                    }
                });
            }
            let _ = window.update(|_, cx| {
                let _ = workspace.update(cx, |workspace, cx| workspace.save_active(cx));
            });
            let save_started = std::time::Instant::now();
            loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let clean = window
                    .update(|_, cx| {
                        workspace
                            .read_with(cx, |workspace, _| {
                                workspace.active_document_status() == Some(DocumentStatus::Clean)
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if clean || save_started.elapsed() > std::time::Duration::from_secs(20) {
                    break;
                }
            }
            let readback = window
                .background_executor()
                .spawn({
                    let path = checkout.join("src/lib.rs");
                    async move { fs::read_to_string(path).unwrap_or_default() }
                })
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
            let clean_refresh_confirmation_started = window
                .update(|_, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.refresh_all(cx);
                            let Some(request_id) = workspace.request_action(
                                LocalAction::SwitchBranch {
                                    branch: "feature/local-ui".into(),
                                },
                                cx,
                            ) else {
                                return false;
                            };
                            workspace.confirm_action(request_id, cx);
                            workspace.in_flight_action_id() == Some(request_id)
                        })
                        .unwrap_or(false)
                })
                .unwrap_or(false);
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
                                workspace.in_flight_action_id().is_none()
                                    && workspace.status_message().contains("Completed")
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
            let save_in_flight_confirmation_paused = window
                .update(|window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            let Some(request_id) = workspace.request_action(
                                LocalAction::SwitchBranch {
                                    branch: "feature/local-ui".into(),
                                },
                                cx,
                            ) else {
                                return false;
                            };
                            let Some(editor) = workspace.active_editor() else {
                                return false;
                            };
                            editor.update(cx, |editor, cx| {
                                editor.insert("// save-in-flight guard\n", window, cx)
                            });
                            workspace.save_active(cx);
                            workspace.confirm_action(request_id, cx);
                            let paused = workspace.in_flight_action_id().is_none()
                                && workspace
                                    .status_message()
                                    .contains("Confirmation paused");
                            workspace.cancel_action(request_id, cx);
                            paused && workspace.status_message().contains("cancelled")
                        })
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let save_guard_at = std::time::Instant::now();
            loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                let clean = window
                    .update(|_, cx| {
                        workspace
                            .read_with(cx, |workspace, _| {
                                workspace.active_document_status() == Some(DocumentStatus::Clean)
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if clean || save_guard_at.elapsed() > std::time::Duration::from_secs(20) {
                    break;
                }
            }
            let checkout_action_edit_race_paused = window
                .update(|window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            let request = workspace.request_action(
                                LocalAction::SwitchBranch {
                                    branch: "feature".into(),
                                },
                                cx,
                            );
                            let Some(editor) = workspace.active_editor() else {
                                return false;
                            };
                            editor.update(cx, |editor, cx| {
                                editor.insert("// unsaved ours\n", window, cx)
                            });
                            let Some(request_id) = request else {
                                return false;
                            };
                            workspace.confirm_action(request_id, cx);
                            let paused_and_retained = workspace.in_flight_action_id().is_none()
                                && workspace
                                    .status_message()
                                    .contains("Confirmation paused");
                            workspace.cancel_action(request_id, cx);
                            paused_and_retained
                                && workspace.status_message().contains("cancelled")
                        })
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            window
                .background_executor()
                .spawn({
                    let path = checkout.join("src/lib.rs");
                    async move {
                        let mut disk = fs::read_to_string(&path).expect("smoke disk read");
                        disk.push_str("// external disk edit\n");
                        fs::write(path, disk).expect("smoke external edit");
                    }
                })
                .await;
            let document_conflict_requested = window
                .update(|_, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.refresh_all(cx);
                        })
                        .is_ok()
                })
                .unwrap_or(false);
            let conflict_at = std::time::Instant::now();
            loop {
                window
                    .background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let conflict = window
                    .update(|_, cx| {
                        workspace
                            .read_with(cx, |workspace, _| {
                                workspace.active_document_status() == Some(DocumentStatus::Conflict)
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if conflict || conflict_at.elapsed() > std::time::Duration::from_secs(20) {
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
                "Local workspace native smoke\nappearance: {}\nfocus option: false; cx.activate: not called\nrebase prepare requested: {}\npopulated three-commit plan ready: {}\nplan normal capture: {}\nedit plan Start requested through confirmation: {}\nPausedForEdit observed: {}\nedit wide capture: {}\nexplicit Continue requested through confirmation: {}\nCompleted observed: {}\nresult normal capture: {}\nsafe archive requested and second prepare enabled: {}\nreordered conflict plan prepared: {}\nconflicting Start requested through confirmation: {}\nConflicted observed from real Git: {}\nconflict narrow capture: {}\nexplicit Abort requested through confirmation: {}\nunchanged-file open: {}\nsyntax edit/find-replace/undo-redo/save dispatched: {}\nsave readback contains highlighted edit: {}\nhighlighted editor capture: {}\nempty-message prestart refused with zero in-flight Git: {}\nclean checkout confirmation dispatched while no-op refresh pending: {}\nclean refresh-time confirmation completed authoritatively: {}\nsave-in-flight checkout confirmation paused and retained until cancel: {}\nimmediate edit/persist versus checkout confirmation paused and retained until cancel: {}\nexternal dirty conflict requested: {}\nconflict visible: {}\nmaterial action confirmation visible: {}\nin-flight acknowledgement and second action refused: {}\nconfirmed temporary-repository stage completed and refreshed: {}\nLocal Changes refreshed independently; ReviewSession imported/mutated: false\nconflict/confirmation scene capture: {}\nphysical input and desktop acrylic: not established by own-scene capture\n",
                if dark { "dark" } else { "light" },
                prepare_requested,
                plan_ready,
                plan_capture,
                start_edit_requested,
                edit_ready,
                edit_capture,
                continue_requested,
                rebase_completed,
                result_capture,
                archived,
                conflict_requested,
                conflict_started,
                rebase_conflict,
                conflict_capture,
                abort_requested,
                opened,
                edited,
                readback.contains("highlighted local edit") && readback.contains("verified"),
                highlighted_capture,
                invalid_prestart_refused,
                clean_refresh_confirmation_started,
                clean_refresh_confirmation_finished,
                save_in_flight_confirmation_paused,
                checkout_action_edit_race_paused,
                checkout_action_edit_race_paused && document_conflict_requested,
                window
                    .update(|_, cx| workspace.read_with(cx, |workspace, _| workspace.active_document_status() == Some(DocumentStatus::Conflict)).unwrap_or(false))
                    .unwrap_or(false),
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
