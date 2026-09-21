//! The window's translucency is AppKit's own material, not a painted grey.
//!
//! One material spans the whole window, so the sidebar, the tab strip, the
//! status line and the margin around the main section are all the same surface
//! rather than a translucent column beside an opaque field. The main section's
//! content frame is the only thing that paints over it, which is what makes the
//! frame read as floating on the window instead of filling it.
//!
//! macOS 26 draws glass with `NSGlassEffectView`, which refracts and tints what
//! is behind it rather than merely blurring it; `NSVisualEffectView` remains for
//! an older system and for anyone who prefers the frosted look. Either view is
//! installed underneath GPUI's Metal layer, so every surface that wants it
//! shows it by painting no fill of its own.
//!
//! GPUI's `WindowBackgroundAppearance::Blurred` is deliberately unused: it
//! spreads one Selection-material `NSVisualEffectView` across the whole window
//! and then strips that view's tint and saturation, which blurs without reading
//! as glass.

use cibergit::workspace::SidebarMaterial;
use gpui::Window;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{MainThreadMarker, MainThreadOnly, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView, NSWindowOrderingMode,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::cell::RefCell;

/// `NSGlassEffectViewStyle`. Regular lays a grey scrim under the glass for
/// legibility, which is most of what a sidebar over a quiet desktop ends up
/// showing; Clear keeps the refraction and drops the scrim.
const GLASS_STYLE_REGULAR: isize = 0;
const GLASS_STYLE_CLEAR: isize = 1;

thread_local! {
    /// One window, one material view. The application opens a single window and
    /// every call below belongs to the main thread that renders it. The
    /// requested material is kept beside the view so a changed preference
    /// replaces it instead of stacking a second one behind the first.
    static SIDEBAR: RefCell<Option<(SidebarMaterial, Retained<NSView>)>> =
        const { RefCell::new(None) };
}

/// Installs `material` behind the sidebar, sizes it to `width`, and reports
/// whether a material is in place.
///
/// A `false` answer obliges the caller to paint its own background: the window
/// is transparent, so an uncovered column would show the desktop unblurred.
/// Installs the window's material if it is missing, replaces it when the
/// preference changes, and answers whether one is present. Idempotent, so every
/// surface that has to know whether to stay unpainted can just ask.
pub fn sync_window(window: &Window, material: SidebarMaterial) -> bool {
    SIDEBAR.with(|sidebar| {
        let mut sidebar = sidebar.borrow_mut();
        if sidebar
            .as_ref()
            .is_some_and(|(installed, _)| *installed != material)
            && let Some((_, previous)) = sidebar.take()
        {
            previous.removeFromSuperview();
        }
        if material == SidebarMaterial::Solid {
            return false;
        }
        if sidebar.is_none() {
            *sidebar = install(window, material).map(|view| (material, view));
        }
        // Both axes follow the content view through the autoresizing mask, so a
        // resize needs no restated geometry at all.
        sidebar.is_some()
    })
}

fn install(window: &Window, material: SidebarMaterial) -> Option<Retained<NSView>> {
    let main_thread = MainThreadMarker::new()?;
    // `Window` has its own inherent `window_handle`, which answers with GPUI's
    // handle rather than the platform one this needs.
    let handle = HasWindowHandle::window_handle(window).ok().or_else(|| {
        report("no platform window handle; the sidebar keeps its painted fill");
        None
    })?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        report("window is not an AppKit window; the sidebar keeps its painted fill");
        return None;
    };
    // SAFETY: the handle is this window's live `NSView`, owned by the main
    // thread proven above, and it is only borrowed for this call.
    let rendered = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let content = rendered.window()?.contentView()?;

    let style = match material {
        SidebarMaterial::ClearGlass => Some(GLASS_STYLE_CLEAR),
        SidebarMaterial::TintedGlass => Some(GLASS_STYLE_REGULAR),
        SidebarMaterial::Frosted | SidebarMaterial::Solid => None,
    };
    let installed = style
        .and_then(|style| liquid_glass(&content, style))
        .unwrap_or_else(|| {
            if style.is_some() {
                report("NSGlassEffectView is unavailable; falling back to the frosted material");
            }
            frosted(&content, main_thread)
        });
    installed.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewHeightSizable | NSAutoresizingMaskOptions::ViewWidthSizable,
    );
    content.addSubview_positioned_relativeTo(&installed, NSWindowOrderingMode::Below, None);
    Some(installed)
}

/// macOS 26's glass. The class is looked up by name because the pinned AppKit
/// bindings predate it.
fn liquid_glass(content: &NSView, style: isize) -> Option<Retained<NSView>> {
    let class = AnyClass::get(c"NSGlassEffectView")?;
    // SAFETY: `NSGlassEffectView` is an `NSView` subclass, so it answers
    // `initWithFrame:` and every `NSView` message the caller then sends it.
    unsafe {
        let allocated: Allocated<AnyObject> = msg_send![class, alloc];
        let glass: Retained<AnyObject> = msg_send![allocated, initWithFrame: content.bounds()];
        let _: () = msg_send![&*glass, setStyle: style];
        // The material runs flush into the window's own corners, which already
        // clip it; a radius of its own would cut a second, inset shape.
        let _: () = msg_send![&*glass, setCornerRadius: 0.0f64];
        report(if style == GLASS_STYLE_CLEAR {
            "installed NSGlassEffectView, clear style"
        } else {
            "installed NSGlassEffectView, tinted style"
        });
        Some(Retained::cast_unchecked::<NSView>(glass))
    }
}

/// The pre-26 material, and what a system that has no glass falls back to.
fn frosted(content: &NSView, main_thread: MainThreadMarker) -> Retained<NSView> {
    let frosted =
        NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(main_thread), content.bounds());
    frosted.setMaterial(NSVisualEffectMaterial::Sidebar);
    frosted.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    // The material dims with the window, the way every native sidebar does.
    frosted.setState(NSVisualEffectState::FollowsWindowActiveState);
    report("installed NSVisualEffectView, sidebar material");
    // SAFETY: `NSVisualEffectView` is an `NSView` subclass.
    unsafe { Retained::cast_unchecked::<NSView>(frosted) }
}

/// The material is composited by the window server behind the Metal layer, so
/// no scene capture can show which one was installed. This says so in words.
fn report(outcome: &str) {
    if std::env::var_os("CIBERGIT_GLASS_REPORT").is_some() {
        eprintln!("sidebar material: {outcome}");
    }
}
