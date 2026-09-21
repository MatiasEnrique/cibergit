//! Explicit "open this checkout in another application" support.
//!
//! cibergit no longer edits worktree files, so the only way to reach source is
//! to hand a path to a real editor. Every launch here is user-initiated: this
//! module never opens anything on its own, and it never creates, removes, or
//! renames the path it is given.
//!
//! Detection probes for installed application bundles instead of asking
//! Spotlight, so it still reports correctly on a machine where indexing is
//! disabled.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// An application the user can send a checkout path to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ExternalApp {
    pub label: &'static str,
    /// `None` uses the system default handler for the path.
    pub bundle_id: Option<&'static str>,
    /// Name understood by `super::sidebar_icon`.
    pub icon: &'static str,
}

/// Candidates, in the order they are offered. A bundle name is only a probe
/// target; the launch itself always goes through the bundle identifier, so a
/// renamed or relocated copy still opens correctly once detected.
const CANDIDATES: &[(&str, &str, &str, &str)] = &[
    // (label, bundle identifier, bundle directory name, icon)
    (
        "VS Code",
        "com.microsoft.VSCode",
        "Visual Studio Code.app",
        "vscode",
    ),
    (
        "VS Code Insiders",
        "com.microsoft.VSCodeInsiders",
        "Visual Studio Code - Insiders.app",
        "vscode",
    ),
    ("VSCodium", "com.vscodium", "VSCodium.app", "vscodium"),
    (
        "Cursor",
        "com.todesktop.230313mzl4w4u92",
        "Cursor.app",
        "cursor",
    ),
    ("Zed", "dev.zed.Zed", "Zed.app", "zed"),
    ("Trae", "com.trae.app", "Trae.app", "trae"),
    ("Xcode", "com.apple.dt.Xcode", "Xcode.app", "edit"),
    (
        "Ghostty",
        "com.mitchellh.ghostty",
        "Ghostty.app",
        "terminal",
    ),
    ("iTerm", "com.googlecode.iterm2", "iTerm.app", "terminal"),
    ("Terminal", "com.apple.Terminal", "Terminal.app", "terminal"),
    ("Finder", "com.apple.finder", "Finder.app", "folder"),
];

/// The identifier a preference stores for `app`. The system default handler has
/// no bundle identifier, so it is recorded as the empty string rather than as
/// "no choice made".
pub(super) fn preference_key(app: &ExternalApp) -> String {
    app.bundle_id.unwrap_or_default().to_owned()
}

/// Icon name for a stored preference, or `None` when the identifier names an
/// application this build does not know about — a preference written by a newer
/// version must not silently draw the wrong brand.
pub(super) fn icon_for_preference(key: &str) -> Option<&'static str> {
    if key.is_empty() {
        return Some("folder");
    }
    CANDIDATES
        .iter()
        .find(|(_, bundle_id, _, _)| *bundle_id == key)
        .map(|(_, _, _, icon)| *icon)
}

/// Label for a stored preference, for the control's accessible name.
pub(super) fn label_for_preference(key: &str) -> Option<&'static str> {
    if key.is_empty() {
        return Some("Default app");
    }
    CANDIDATES
        .iter()
        .find(|(_, bundle_id, _, _)| *bundle_id == key)
        .map(|(label, _, _, _)| *label)
}

fn default_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    roots.extend([
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
        PathBuf::from("/System/Library/CoreServices"),
    ]);
    roots
}

/// Applications actually present, followed by the system default handler.
pub(super) fn installed_apps() -> Vec<ExternalApp> {
    detect(&default_roots())
}

fn detect(roots: &[PathBuf]) -> Vec<ExternalApp> {
    let mut apps: Vec<ExternalApp> = CANDIDATES
        .iter()
        .filter(|(_, _, bundle, _)| roots.iter().any(|root| root.join(bundle).is_dir()))
        .map(|(label, bundle_id, _, icon)| ExternalApp {
            label,
            bundle_id: Some(bundle_id),
            icon,
        })
        .collect();
    apps.push(ExternalApp {
        label: "Default app",
        bundle_id: None,
        icon: "folder",
    });
    apps
}

/// Hands `path` to `app`. Blocking but short; callers run it on the background
/// executor rather than the UI thread.
pub(super) fn open_in(app: &ExternalApp, path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("Refusing to open a relative path".into());
    }
    if !path.is_dir() {
        return Err(format!(
            "{} is no longer a directory; refresh the checkout",
            path.display()
        ));
    }
    let mut command = Command::new("/usr/bin/open");
    if let Some(bundle_id) = app.bundle_id {
        command.arg("-b").arg(bundle_id);
    }
    // `--` keeps a path that begins with `-` from being read as a flag.
    command.arg("--").arg(path);
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("Cannot run open: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    // `open` reports a missing application here, which is worth surfacing
    // verbatim: the probe can go stale if the bundle was removed since.
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if detail.is_empty() {
        format!("{} could not open this checkout", app.label)
    } else {
        detail
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn only_present_bundles_are_offered_and_the_default_always_is() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().to_owned();
        fs::create_dir(root.join("Visual Studio Code.app")).unwrap();
        fs::create_dir(root.join("Zed.app")).unwrap();
        // A file of the right name is not an application bundle.
        fs::write(root.join("Cursor.app"), "not a bundle").unwrap();

        let apps = detect(&[root]);
        let labels: Vec<&str> = apps.iter().map(|app| app.label).collect();
        assert_eq!(labels, ["VS Code", "Zed", "Default app"]);
        assert_eq!(apps[0].bundle_id, Some("com.microsoft.VSCode"));
        assert_eq!(
            apps.last().unwrap().bundle_id,
            None,
            "the default handler carries no bundle identifier"
        );
    }

    #[test]
    fn detection_reports_nothing_but_the_default_when_no_bundle_exists() {
        let directory = TempDir::new().unwrap();
        let apps = detect(&[directory.path().to_owned()]);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].label, "Default app");
    }

    #[test]
    fn relative_and_missing_paths_are_refused_before_launching_anything() {
        let app = ExternalApp {
            label: "VS Code",
            bundle_id: Some("com.microsoft.VSCode"),
            icon: "vscode",
        };
        assert!(open_in(&app, Path::new("relative/path")).is_err());
        let directory = TempDir::new().unwrap();
        let absent = directory.path().join("gone");
        let error = open_in(&app, &absent).unwrap_err();
        assert!(error.contains("no longer a directory"), "{error}");
    }
}
