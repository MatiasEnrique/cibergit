//! Participant avatars for the PR conversation.
//!
//! This is the one place where cibergit resolves remote media, and it is kept
//! deliberately narrow: only GitHub's own avatar host, only pictures the
//! details read already named, never a URL typed into a comment body. A login
//! is asked for once per run; what comes back is written beside the rest of
//! the workspace state so the next launch paints from disk instead of the
//! network.
//!
//! Nothing here blocks a frame. The letter puck stays on screen until bytes
//! arrive, and a login whose picture cannot be fetched keeps it for good.

use gpui::{Image, ImageFormat};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

/// A picture is 64px square; anything this far past that is not one, and is
/// dropped rather than decoded.
const MAX_AVATAR_BYTES: u64 = 512 * 1024;

/// How long a single avatar fetch may take before it is abandoned. A slow
/// avatar is never worth delaying anything else for.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// What is known about one participant's picture.
enum Slot {
    /// Claimed by an in-flight fetch. No second fetch is started for it.
    Pending,
    Ready(Arc<Image>),
    /// Fetched and failed, or fetched and undecodable. Asking again inside one
    /// run would just repeat the failure on every frame.
    Missing,
}

/// Every participant picture this run has asked for, keyed by login.
pub(super) struct AvatarCache {
    /// Where fetched bytes are kept between runs.
    root: PathBuf,
    slots: HashMap<String, Slot>,
}

impl AvatarCache {
    pub(super) fn new(root: PathBuf) -> Self {
        Self {
            root,
            slots: HashMap::new(),
        }
    }

    /// Take ownership of fetching this login's picture, or report that someone
    /// already has. The caller only spawns work when this answers `true`.
    pub(super) fn claim(&mut self, login: &str) -> bool {
        if self.slots.contains_key(login) {
            return false;
        }
        self.slots.insert(login.to_owned(), Slot::Pending);
        true
    }

    /// Record what a fetch came back with, including nothing.
    pub(super) fn finish(&mut self, login: &str, image: Option<Arc<Image>>) {
        let slot = match image {
            Some(image) => Slot::Ready(image),
            None => Slot::Missing,
        };
        self.slots.insert(login.to_owned(), slot);
    }

    /// The pictures on hand right now, for one render pass to look up in.
    /// Cloning is an `Arc` per participant, not per comment.
    pub(super) fn ready(&self) -> HashMap<String, Arc<Image>> {
        self.slots
            .iter()
            .filter_map(|(login, slot)| match slot {
                Slot::Ready(image) => Some((login.clone(), image.clone())),
                Slot::Pending | Slot::Missing => None,
            })
            .collect()
    }

    /// The file this URL's bytes live in. The name is the URL's own digest, so
    /// a participant who changes their picture lands on a different file
    /// rather than reusing a stale one.
    pub(super) fn path_for(&self, url: &str) -> PathBuf {
        self.root.join(format!("{}.avatar", digest(url)))
    }
}

/// Load one avatar: the saved copy when there is one, otherwise the network,
/// which is then saved. Runs on a background thread; returns `None` for every
/// failure, because a missing picture is never worth reporting to the user.
pub(super) fn load(path: &Path, url: &str) -> Option<Arc<Image>> {
    if let Some(image) = std::fs::read(path).ok().and_then(decode) {
        return Some(image);
    }
    let bytes = fetch(url)?;
    let image = decode(bytes.clone())?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // A half-written file would be read back as a corrupt picture forever, so
    // the bytes land under a neighbouring name and are moved into place whole.
    let staging = path.with_extension("partial");
    if std::fs::write(&staging, &bytes).is_ok() {
        let _ = std::fs::rename(&staging, path);
    }
    Some(image)
}

/// Fetch the picture itself. Avatars are public, so this carries no
/// credential: it is a plain anonymous GET, and deliberately not routed
/// through `gh`, which would attach the selected account's token to a request
/// that has no need of one.
fn fetch(url: &str) -> Option<Vec<u8>> {
    let output = Command::new("/usr/bin/curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--location",
            // A redirect is normal here; an unbounded chain of them is not.
            "--max-redirs",
            "3",
            "--proto",
            "=https",
            "--max-time",
            &FETCH_TIMEOUT.as_secs().to_string(),
            "--max-filesize",
            &MAX_AVATAR_BYTES.to_string(),
            "--",
            url,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    (output.status.success() && !output.stdout.is_empty()).then_some(output.stdout)
}

/// Wrap bytes for GPUI, naming the format from the bytes themselves. GitHub
/// serves whatever the participant uploaded, so the file extension and the
/// content type are both worse evidence than the header.
fn decode(bytes: Vec<u8>) -> Option<Arc<Image>> {
    if bytes.len() as u64 > MAX_AVATAR_BYTES {
        return None;
    }
    Some(Arc::new(Image::from_bytes(sniff(&bytes)?, bytes)))
}

/// The image formats GitHub accepts for an avatar, recognised by their magic
/// bytes. Anything else is treated as not a picture.
fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some(ImageFormat::Webp)
    } else {
        None
    }
}

/// A filename-safe digest of a URL.
fn digest(url: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(url.as_bytes());
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_names_the_formats_github_serves_and_refuses_anything_else() {
        assert!(matches!(
            sniff(b"\x89PNG\r\n\x1a\n\x00"),
            Some(ImageFormat::Png)
        ));
        assert!(matches!(
            sniff(&[0xff, 0xd8, 0xff, 0xe0]),
            Some(ImageFormat::Jpeg)
        ));
        assert!(matches!(sniff(b"GIF89a...."), Some(ImageFormat::Gif)));
        assert!(matches!(
            sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some(ImageFormat::Webp)
        ));
        // An SVG would be a remote document, not a picture, and GPUI would
        // render whatever it contains.
        assert!(sniff(b"<svg xmlns='http://www.w3.org/2000/svg'/>").is_none());
        assert!(sniff(b"").is_none());
    }

    #[test]
    fn oversized_bytes_are_never_decoded() {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(MAX_AVATAR_BYTES as usize + 1, 0);
        assert!(decode(bytes).is_none());
    }

    #[test]
    fn each_url_gets_its_own_file_and_the_same_url_gets_the_same_one() {
        let cache = AvatarCache::new(PathBuf::from("/tmp/cibergit-avatars"));
        let first = cache.path_for("https://avatars.githubusercontent.com/u/1?s=64");
        let second = cache.path_for("https://avatars.githubusercontent.com/u/2?s=64");
        assert_ne!(first, second);
        assert_eq!(
            first,
            cache.path_for("https://avatars.githubusercontent.com/u/1?s=64")
        );
        assert!(first.starts_with("/tmp/cibergit-avatars"));
    }

    #[test]
    fn a_login_is_only_claimed_once_and_a_failed_fetch_is_not_retried() {
        let mut cache = AvatarCache::new(PathBuf::from("/tmp/cibergit-avatars"));
        assert!(cache.claim("coderabbitai"));
        assert!(!cache.claim("coderabbitai"));
        assert!(cache.ready().is_empty());
        cache.finish("coderabbitai", None);
        assert!(!cache.claim("coderabbitai"));
        assert!(cache.ready().is_empty());
    }
}
