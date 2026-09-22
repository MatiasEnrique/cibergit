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

/// How far ahead of the viewport a row is still built. Revealing a row should
/// not be the first time its prose is shaped, so the band runs on past the edge
/// the reader is moving towards and the work happens before they arrive.
const LEAD_PIXELS: f32 = 700.;
const LEAD: Pixels = px(LEAD_PIXELS);

/// How far behind the viewport a row is still built.
///
/// The band used to run a full screen out in both directions, which laid out
/// about four screens to show one. Behind the reader there is nothing to shape
/// before they arrive — they have been there — so that side only has to cover
/// the moment a scroll turns around, and the lead is spent where they are
/// actually going.
const TRAIL_PIXELS: f32 = 200.;
const TRAIL: Pixels = px(TRAIL_PIXELS);

/// Which way the page last moved under the reader. The band leads this way.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Travel {
    #[default]
    Down,
    Up,
}

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
fn outside_window(band: Band, scrolled: Pixels, viewport_height: Pixels, travel: Travel) -> bool {
    let (over, under) = match travel {
        Travel::Down => (TRAIL, LEAD),
        Travel::Up => (LEAD, TRAIL),
    };
    band.top >= scrolled + viewport_height + under || band.top + band.height <= scrolled - over
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
    /// Where the page stood the last time it moved, and which way it went.
    /// Every row of one redraw reads the same scroll distance, so the first of
    /// them notices the move and the rest agree with it.
    scrolled: Pixels,
    travel: Travel,
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
        let mut travel = Travel::default();
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
                if scrolled != measured.scrolled {
                    measured.travel = if scrolled > measured.scrolled {
                        Travel::Down
                    } else {
                        Travel::Up
                    };
                    measured.scrolled = scrolled;
                }
                travel = measured.travel;
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
        match band.filter(|band| outside_window(*band, scrolled, viewport.size.height, travel)) {
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
            VIEWPORT,
            Travel::Down
        ));
    }

    #[test]
    fn a_row_within_the_lead_is_still_inside_the_window() {
        // Below the viewport, with its head one pixel inside the lead: its
        // prose is shaped before the reader scrolling down reaches it.
        let below = band(SCROLLED + VIEWPORT_PIXELS + LEAD_PIXELS - 1., 400.);
        assert!(!outside_window(below, px(SCROLLED), VIEWPORT, Travel::Down));
        // Above the viewport, for a reader scrolling back up.
        let above = band(SCROLLED - LEAD_PIXELS - 399., 400.);
        assert!(!outside_window(above, px(SCROLLED), VIEWPORT, Travel::Up));
    }

    #[test]
    fn a_row_past_the_lead_becomes_a_spacer() {
        let below = band(SCROLLED + VIEWPORT_PIXELS + LEAD_PIXELS, 400.);
        assert!(outside_window(below, px(SCROLLED), VIEWPORT, Travel::Down));
        let above = band(SCROLLED - LEAD_PIXELS - 400., 400.);
        assert!(outside_window(above, px(SCROLLED), VIEWPORT, Travel::Up));
    }

    /// The side the reader is moving away from only has to cover a scroll
    /// turning around, so it is built out by much less than the side they are
    /// moving towards. A row that would sit inside the lead stands in as a
    /// spacer once it is behind.
    #[test]
    fn a_row_behind_the_reader_is_kept_only_to_the_trail() {
        let behind = band(SCROLLED - TRAIL_PIXELS - 399., 400.);
        assert!(!outside_window(
            behind,
            px(SCROLLED),
            VIEWPORT,
            Travel::Down
        ));
        let further = band(SCROLLED - TRAIL_PIXELS - 400., 400.);
        assert!(outside_window(
            further,
            px(SCROLLED),
            VIEWPORT,
            Travel::Down
        ));
        assert!(
            !outside_window(further, px(SCROLLED), VIEWPORT, Travel::Up),
            "the same row is built once the reader turns back towards it"
        );
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
