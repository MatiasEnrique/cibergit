//! Building only the part of a long scrolling page a reader can see.
//!
//! The conversation is a plain scrolling column rather than a virtual list, and
//! it has to be: its rows are a description, a composer, recovery notices,
//! review cards, threads and comments, and no two of them are the same shape or
//! the same height. GPUI redraws the whole window for every scroll wheel tick,
//! so that column used to lay out, shape and paint every comment on the page
//! for each of those frames. One coding agent's review is thousands of shaped
//! lines on its own, and a conversation holds twenty of them.
//!
//! This keeps the column and takes the cost out of it. Every row reports where
//! it landed and how tall it was during prepaint. On the next frame a row whose
//! remembered band lies well outside the viewport is replaced by a spacer of
//! its own last height, so the page keeps its exact length and the scrollbar
//! its exact position while only the rows near the reader are built.
//!
//! A row is built whenever its height is not known, so nothing is ever left out
//! for never having been measured; the first frame of a page builds all of it.

use gpui::{
    App, Bounds, Div, ParentElement, Pixels, ScrollHandle, SharedString, Styled, Window, div, px,
};
use gpui_base::ElementExt as _;
use std::{cell::RefCell, collections::HashMap, rc::Rc};

/// How far past the viewport a row is still built. Revealing a row should not
/// be the first time its prose is shaped, so the band extends by about a screen
/// in each direction and the work happens before the reader arrives.
const OVERDRAW_PIXELS: f32 = 900.;
const OVERDRAW: Pixels = px(OVERDRAW_PIXELS);

/// Where one row sat in the page, and how tall it was there. `top` is measured
/// from the top of the scrolled content rather than from the window, so it
/// stays true as the page scrolls under it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Band {
    top: Pixels,
    height: Pixels,
}

/// Whether a row's remembered band is far enough from the viewport to stand in
/// as a spacer. `scrolled` is the distance from the top of the content to the
/// top of the viewport.
fn outside_window(band: Band, scrolled: Pixels, viewport_height: Pixels) -> bool {
    band.top >= scrolled + viewport_height + OVERDRAW
        || band.top + band.height <= scrolled - OVERDRAW
}

#[derive(Default)]
struct Measured {
    /// The page width the last redraw built against. A resize rewraps every
    /// body, so the remembered heights stop describing anything and go.
    ///
    /// This is deliberately the width read while building rows, not the one
    /// read while measuring them: the first redraw of a window runs before the
    /// scrolling page has been laid out at all, and a zero read there would
    /// otherwise look like a resize on the next frame and discard everything
    /// the first frame had just measured.
    width: Option<Pixels>,
    rows: HashMap<SharedString, Band>,
    /// How many rows have been built rather than stood in for, over the life of
    /// this page. Only tests read it, and they read it to establish that a
    /// redraw stops doing the work the first one did.
    built: usize,
}

/// The remembered geometry of one scrolling page.
#[derive(Clone, Default)]
pub(super) struct PageWindow(Rc<RefCell<Measured>>);

impl PageWindow {
    /// Wrap one page row, giving back either the row itself or a spacer of its
    /// last measured height.
    ///
    /// `key` identifies the row across frames and must name what the row shows,
    /// not merely its position: the same page position holds a different
    /// comment in a different pull request, and a height remembered for one
    /// would be wrong for the other.
    pub(super) fn row(&self, key: SharedString, scroll: &ScrollHandle, row: Div) -> Div {
        let viewport = scroll.bounds();
        // GPUI keeps the scroll distance as a negative offset.
        let scrolled = -scroll.offset().y;
        let laid_out = viewport.size.width > px(0.) && viewport.size.height > px(0.);
        let band = laid_out
            .then(|| {
                let mut measured = self.0.borrow_mut();
                // Learning the width for the first time is not a resize.
                if measured
                    .width
                    .replace(viewport.size.width)
                    .is_some_and(|previous| previous != viewport.size.width)
                {
                    measured.rows.clear();
                }
                measured.rows.get(&key).copied()
            })
            .flatten();

        let record = {
            let state = self.0.clone();
            let scroll = scroll.clone();
            let key = key.clone();
            move |bounds: Bounds<Pixels>, _: &mut Window, _: &mut App| {
                let viewport = scroll.bounds();
                let band = Band {
                    top: bounds.origin.y - viewport.origin.y - scroll.offset().y,
                    height: bounds.size.height,
                };
                state.borrow_mut().rows.insert(key, band);
            }
        };

        // The wrapper carries no padding of its own, so what prepaint measures
        // is exactly the box the page laid out, spacer or row alike. It is a
        // column so the row it holds stretches across it, the way the row did
        // when the page held it directly.
        let wrapper = div().w_full().min_w_0().flex().flex_col();
        match band.filter(|band| outside_window(*band, scrolled, viewport.size.height)) {
            // A spacer still reports where it is: rows above it can change
            // height, and this one has to notice that it moved.
            Some(band) => wrapper.h(band.height),
            None => {
                self.0.borrow_mut().built += 1;
                wrapper.child(row)
            }
        }
        .on_prepaint(record)
    }

    /// How many rows this page has built, rather than stood in for, since it
    /// was created.
    #[cfg(test)]
    pub(super) fn built_rows(&self) -> usize {
        self.0.borrow().built
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWPORT_PIXELS: f32 = 900.;
    const VIEWPORT: Pixels = px(VIEWPORT_PIXELS);

    /// The reader is ten thousand pixels down a long conversation.
    const SCROLLED: f32 = 10_000.;

    fn band(top: f32, height: f32) -> Band {
        Band {
            top: px(top),
            height: px(height),
        }
    }

    #[test]
    fn a_row_in_the_viewport_is_inside_the_window() {
        assert!(!outside_window(
            band(SCROLLED, 400.),
            px(SCROLLED),
            VIEWPORT
        ));
    }

    #[test]
    fn a_row_within_the_overdraw_is_still_inside_the_window() {
        // Above the viewport, with its foot one pixel inside the leading
        // overdraw: its prose is shaped before the reader scrolls back to it.
        let above = band(SCROLLED - OVERDRAW_PIXELS - 399., 400.);
        assert!(!outside_window(above, px(SCROLLED), VIEWPORT));
        // Below the viewport, by the same margin.
        let below = band(SCROLLED + VIEWPORT_PIXELS + OVERDRAW_PIXELS - 1., 400.);
        assert!(!outside_window(below, px(SCROLLED), VIEWPORT));
    }

    #[test]
    fn a_row_past_the_overdraw_becomes_a_spacer() {
        let above = band(SCROLLED - OVERDRAW_PIXELS - 400., 400.);
        assert!(outside_window(above, px(SCROLLED), VIEWPORT));
        let below = band(SCROLLED + VIEWPORT_PIXELS + OVERDRAW_PIXELS, 400.);
        assert!(outside_window(below, px(SCROLLED), VIEWPORT));
    }

    /// The first redraw of a window runs before the scrolling page has been
    /// laid out. Reading a zero width there and calling the next frame a resize
    /// would discard every height the first frame measured, which is exactly
    /// the set the second frame needs.
    #[test]
    fn an_unlaid_out_page_records_no_width_to_be_resized_from() {
        let window = PageWindow::default();
        let _ = window.row("comment-1".into(), &ScrollHandle::new(), div());
        assert_eq!(window.0.borrow().width, None);
        assert_eq!(window.built_rows(), 1, "an unmeasured row must be built");
    }
}
