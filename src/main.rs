#![recursion_limit = "512"]

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("cibergit V1 supports macOS on Apple Silicon only");

mod app;

use app::{LaunchMode, Startup};
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
            "--edit" => {
                startup.mode = LaunchMode::Edit(PathBuf::from(next_value(&mut args, "--edit")?))
            }
            "--help" | "-h" => {
                println!(
                    "cibergit [--repo OWNER/NAME|URL|FOLDER --account LOGIN --pr NUMBER] \
                     [--data-dir PATH] [--edit PATH]\n\n\
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
    let fonts = vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-Medium.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-SemiBold.ttf").as_slice()),
    ];
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        eprintln!("Cannot load bundled IBM Plex Sans: {error:#}");
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
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
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
                window_background: WindowBackgroundAppearance::Blurred,
                ..Default::default()
            },
            move |window, cx| match &startup.mode {
                LaunchMode::Review => cx.new(|cx| app::Root::review(window, cx, startup.clone())),
                LaunchMode::Edit(path) => cx.new(|cx| app::Root::editor(window, cx, path.clone())),
            },
        )
        .expect("open native window");
        if !background_smoke {
            cx.activate(true);
        }
    });
}
