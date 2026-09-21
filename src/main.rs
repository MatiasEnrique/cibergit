#![recursion_limit = "512"]

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("cibergit V1 supports macOS on Apple Silicon only");

mod app;
mod glass;

use app::Startup;
use gpui::{
    AppContext as _, Bounds, KeyBinding, TitlebarOptions, WindowBackgroundAppearance, WindowBounds,
    WindowOptions, actions, px, size,
};
use std::{borrow::Cow, path::PathBuf};

fn next_value<I: Iterator<Item = std::ffi::OsString>>(
    args: &mut I,
    name: &str,
) -> Result<std::ffi::OsString, String> {
    args.next()
        .ok_or_else(|| format!("{name} requires a value"))
}

actions!(
    cibergit,
    [
        Save,
        Quit,
        Refresh,
        NextFile,
        PreviousFile,
        CloseTab,
        TogglePalette,
        ToggleInspector,
        CycleDiffMode,
        OpenRepositorySetup,
        OpenSettings,
        OpenHistory,
        OpenPullRequestBrowser,
        ComposeInlineComment,
        SaveReviewDraft,
        AddPendingComment,
        PostImmediateComment,
        SubmitReview,
        MergePullRequest,
        ToggleComparisonPicker,
        SelectFullComparison,
        SelectSinceLastReview,
        SelectPreviousComparisonCommit,
        SelectNextComparisonCommit,
        EditPrMetadata,
        ApplyPrMetadata,
        NewPrDiscussion,
        ApplyPrDiscussion,
        ConfirmPrMutation,
        CancelPrMutation,
        OpenStackView,
        RefreshStackView,
        ToggleStackRelationships,
        SelectNextStackTip,
        ReturnToPullRequest,
    ]
);
actions!(
    cibergit,
    [
        OpenPullRequestCreation,
        PreparePullRequestCreation,
        ConfirmPullRequestCreation,
        CancelPullRequestCreation,
        ClosePullRequestCreation,
        TogglePullRequestCreationDraft,
    ]
);
actions!(
    cibergit,
    [
        FileTreeUp,
        FileTreeDown,
        FileTreeLeft,
        FileTreeRight,
        FileTreeActivate,
        DiffScrollLeft,
        DiffScrollRight,
        DiffScrollHome,
        DiffScrollEnd,
    ]
);
// Reading a diff with the keyboard. The cursor is a row, so every one of these
// is a move to another row; the pane brings whichever row it lands on into
// view. Modifier-based rather than single letters, because the diff pane also
// answers `c` and a bare-letter scheme would fight the composer it opens.
actions!(
    cibergit,
    [
        DiffCursorDown,
        DiffCursorUp,
        DiffNextHunk,
        DiffPreviousHunk,
        DiffNextThread,
        DiffPreviousThread,
        DiffCursorToStart,
        DiffCursorToEnd,
        MarkViewedAndAdvance,
        ToggleAllFileSections,
    ]
);
actions!(
    cibergit,
    [
        ToggleSidebar,
        ToggleFileTree,
        SidebarNarrower,
        SidebarWider,
        FileTreeNarrower,
        FileTreeWider,
        DetailsNarrower,
        DetailsWider,
        ResetLayout,
    ]
);

fn parse_startup() -> Result<Startup, String> {
    let mut args = std::env::args_os().skip(1);
    let mut startup = Startup::default();
    while let Some(argument) = args.next() {
        let flag = argument.to_string_lossy();
        match flag.as_ref() {
            "--repo" => {
                startup.repository = Some(next_value(&mut args, "--repo")?.to_string_lossy().into())
            }
            "--account" => {
                startup.account = Some(next_value(&mut args, "--account")?.to_string_lossy().into())
            }
            "--pr" => {
                let number = next_value(&mut args, "--pr")?;
                startup.pull_request = Some(
                    number
                        .to_string_lossy()
                        .parse()
                        .map_err(|_| "--pr requires a positive integer".to_owned())?,
                );
            }
            "--data-dir" => {
                startup.data_dir = Some(PathBuf::from(next_value(&mut args, "--data-dir")?))
            }
            "--help" | "-h" => {
                println!(
                    "cibergit [--repo OWNER/NAME|URL|FOLDER --account LOGIN --pr NUMBER] \
                     [--data-dir PATH]\n\n\
                     CIBERGIT_DATA_DIR also overrides the personal workspace directory."
                );
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument: {flag}")),
        }
    }
    if startup.data_dir.is_none() {
        startup.data_dir = std::env::var_os("CIBERGIT_DATA_DIR").map(PathBuf::from);
    }
    #[cfg(feature = "ui-smoke")]
    if std::env::var_os("CIBERGIT_SMOKE_DIR").is_some()
        && (std::env::var_os("CIBERGIT_SMOKE_ACTIONS_JOBS_LOGS").is_some()
            || std::env::var_os("CIBERGIT_SMOKE_SIDEBAR").is_some()
            || std::env::var_os("CIBERGIT_SMOKE_PR_LAYOUT").is_some()
            || std::env::var_os("CIBERGIT_SMOKE_HISTORY").is_some()
            || std::env::var_os("CIBERGIT_SMOKE_STACK_TIPS").is_some())
    {
        startup.provider_reads_disabled = true;
    }
    if startup.pull_request.is_some() && startup.repository.is_none() {
        return Err("--pr requires --repo".into());
    }
    if startup.repository.is_some() && startup.account.is_none() {
        return Err(
            "--repo startup requires --account (the setup screen can discover accounts)".into(),
        );
    }
    Ok(startup)
}

fn load_interface_fonts(cx: &gpui::App) {
    // Three weights per family, which is the whole ladder in src/ui.rs: Regular
    // reads, Medium emphasises, SemiBold is the ceiling. Upstream revisions and
    // checksums are in assets/fonts/manifest.json.
    let fonts = vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/Geist-Regular.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/Geist-Medium.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/Geist-SemiBold.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/Inter-Regular.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/Inter-Medium.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/Inter-SemiBold.ttf").as_slice()),
    ];
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        eprintln!("Cannot load bundled interface fonts: {error:#}");
    }
}

fn main() {
    let startup = parse_startup().unwrap_or_else(|error| {
        eprintln!("cibergit: {error}\nRun cibergit --help for usage.");
        std::process::exit(2);
    });
    let background_smoke =
        cfg!(feature = "ui-smoke") && std::env::var_os("CIBERGIT_SMOKE_BACKGROUND").is_some();
    gpui_platform::application().run(move |cx| {
        gpui_base::init(cx);
        load_interface_fonts(cx);
        cx.set_app_identity("dev.cibergit.cibergit", "cibergit");
        #[cfg(feature = "ui-smoke")]
        match std::env::var("CIBERGIT_SMOKE_APPEARANCE").as_deref() {
            Ok("dark") => cx.set_window_appearance(Some(gpui::WindowAppearance::Dark)),
            Ok("light") => cx.set_window_appearance(Some(gpui::WindowAppearance::Light)),
            _ => {}
        }
        cx.bind_keys([
            KeyBinding::new("cmd-s", Save, None),
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-r", Refresh, None),
            KeyBinding::new("cmd-]", NextFile, None),
            KeyBinding::new("cmd-[", PreviousFile, None),
            KeyBinding::new("cmd-w", CloseTab, None),
            KeyBinding::new("cmd-shift-p", TogglePalette, None),
            KeyBinding::new("cmd-shift-i", ToggleInspector, None),
            KeyBinding::new("cmd-shift-d", CycleDiffMode, None),
            KeyBinding::new("cmd-o", OpenRepositorySetup, None),
            KeyBinding::new("cmd-,", OpenSettings, None),
            KeyBinding::new("cmd-shift-h", OpenHistory, None),
            KeyBinding::new("cmd-shift-l", OpenPullRequestBrowser, None),
            KeyBinding::new("cmd-shift-n", OpenPullRequestCreation, None),
            KeyBinding::new("cmd-enter", PreparePullRequestCreation, Some("PrCreation")),
            KeyBinding::new(
                "cmd-shift-enter",
                ConfirmPullRequestCreation,
                Some("PrCreation"),
            ),
            KeyBinding::new("escape", CancelPullRequestCreation, Some("PrCreation")),
            KeyBinding::new("cmd-escape", ClosePullRequestCreation, Some("PrCreation")),
            KeyBinding::new("cmd-d", TogglePullRequestCreationDraft, Some("PrCreation")),
            KeyBinding::new("c", ComposeInlineComment, Some("DiffPane")),
            KeyBinding::new("cmd-enter", SaveReviewDraft, Some("ReviewComposer")),
            KeyBinding::new("cmd-shift-enter", AddPendingComment, Some("ReviewComposer")),
            KeyBinding::new(
                "cmd-alt-enter",
                PostImmediateComment,
                Some("ReviewComposer"),
            ),
            KeyBinding::new("cmd-shift-r", SubmitReview, None),
            KeyBinding::new("cmd-shift-m", MergePullRequest, None),
            KeyBinding::new("cmd-alt-k", ToggleComparisonPicker, None),
            KeyBinding::new("cmd-alt-1", SelectFullComparison, None),
            KeyBinding::new("cmd-alt-4", SelectSinceLastReview, None),
            KeyBinding::new("cmd-alt-[", SelectPreviousComparisonCommit, None),
            KeyBinding::new("cmd-alt-]", SelectNextComparisonCommit, None),
            KeyBinding::new("ctrl-alt-m", EditPrMetadata, None),
            KeyBinding::new("ctrl-alt-enter", ApplyPrMetadata, None),
            KeyBinding::new("ctrl-alt-c", NewPrDiscussion, None),
            KeyBinding::new("ctrl-alt-shift-enter", ApplyPrDiscussion, None),
            KeyBinding::new("ctrl-alt-shift-m", ConfirmPrMutation, None),
            KeyBinding::new("ctrl-alt-escape", CancelPrMutation, None),
            KeyBinding::new("cmd-shift-s", OpenStackView, None),
            KeyBinding::new("cmd-alt-r", RefreshStackView, Some("StackView")),
            KeyBinding::new("cmd-alt-l", ToggleStackRelationships, Some("StackView")),
            KeyBinding::new("cmd-alt-t", SelectNextStackTip, Some("StackView")),
            KeyBinding::new("escape", ReturnToPullRequest, Some("StackView")),
            KeyBinding::new("up", FileTreeUp, Some("FileTree")),
            KeyBinding::new("down", FileTreeDown, Some("FileTree")),
            KeyBinding::new("left", FileTreeLeft, Some("FileTree")),
            KeyBinding::new("right", FileTreeRight, Some("FileTree")),
            KeyBinding::new("enter", FileTreeActivate, Some("FileTree")),
            KeyBinding::new("cmd-shift-b", ToggleSidebar, None),
            KeyBinding::new("cmd-shift-f", ToggleFileTree, None),
            KeyBinding::new("ctrl-alt-left", SidebarNarrower, None),
            KeyBinding::new("ctrl-alt-right", SidebarWider, None),
            KeyBinding::new("ctrl-alt-shift-left", FileTreeNarrower, None),
            KeyBinding::new("ctrl-alt-shift-right", FileTreeWider, None),
            KeyBinding::new("ctrl-cmd-alt-left", DetailsNarrower, None),
            KeyBinding::new("ctrl-cmd-alt-right", DetailsWider, None),
            KeyBinding::new("ctrl-alt-0", ResetLayout, None),
            KeyBinding::new("left", DiffScrollLeft, Some("DiffPane")),
            KeyBinding::new("right", DiffScrollRight, Some("DiffPane")),
            KeyBinding::new("home", DiffScrollHome, Some("DiffPane")),
            KeyBinding::new("end", DiffScrollEnd, Some("DiffPane")),
            // Bare arrows stay with the list's own scrolling; the cursor takes
            // the command pair, hunks the option pair, and threads both.
            KeyBinding::new("cmd-down", DiffCursorDown, Some("DiffPane")),
            KeyBinding::new("cmd-up", DiffCursorUp, Some("DiffPane")),
            KeyBinding::new("alt-down", DiffNextHunk, Some("DiffPane")),
            KeyBinding::new("alt-up", DiffPreviousHunk, Some("DiffPane")),
            KeyBinding::new("cmd-alt-down", DiffNextThread, Some("DiffPane")),
            KeyBinding::new("cmd-alt-up", DiffPreviousThread, Some("DiffPane")),
            KeyBinding::new("cmd-home", DiffCursorToStart, Some("DiffPane")),
            KeyBinding::new("cmd-end", DiffCursorToEnd, Some("DiffPane")),
            KeyBinding::new("cmd-shift-v", MarkViewedAndAdvance, Some("DiffPane")),
            // Folding is a property of the whole scroll, so it is not
            // scoped to the pane: the button on the bar and this key do
            // the same thing from wherever focus happens to be.
            KeyBinding::new("cmd-shift-j", ToggleAllFileSections, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        let root_slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let root_for_window = root_slot.clone();
        cx.open_window(
            WindowOptions {
                focus: !background_smoke,
                titlebar: Some(TitlebarOptions {
                    title: Some("cibergit".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1440.), px(900.)),
                    cx,
                ))),
                window_min_size: Some(size(px(1040.), px(620.))),
                // The sidebar carries its own AppKit material (src/glass.rs), so
                // the window only has to stop painting behind it.
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            move |window, cx| {
                let root = cx.new(|cx| app::Root::review(window, cx, startup.clone()));
                *root_for_window.borrow_mut() = Some(root.downgrade());
                root
            },
        )
        .expect("open native window");
        cx.on_system_notification_response(move |response, cx| {
            let Some(root) = root_slot.borrow().clone() else {
                return;
            };
            let _ = root.update(cx, |root, cx| {
                root.system_notification_response(&response.tag, cx)
            });
        });
        if !background_smoke {
            cx.activate(true);
        }
    });
}
