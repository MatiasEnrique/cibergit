//! The review window's half of the block vocabulary.
//!
//! `cibergit::ui` owns the numbers and the palette-free builders — `page`,
//! `block`, `lines`, `row`, `text_field`, `text_area`, `notice`, `rule`. Those
//! live in the library because the standalone `LocalWorkspace` harness mounts
//! its module outside this crate root and has to build the same shapes from its
//! own colours. What is here is the half that needs this window's `Palette`:
//! the titled section, the labelled field, and the tinted wrappers around the
//! two text fields.
//!
//! Between them they are the layer that did not exist before. Every block used
//! to re-derive its geometry from the raw tokens, so the same relationship came
//! out 4px in one place and 8px two lines below — nothing said which token a
//! relationship was supposed to take. A screen now names the relationship and
//! the spacing follows. See `ui::page` for the ladder and the one rule that
//! keeps it from drifting: a container declares one `gap`, and its children
//! carry no margin.
//!
//! There are no cards here, and that is the point. A fill plus a radius plus a
//! box inset around content is a container the reader has to parse before
//! reaching what is inside it, and a page of them reads as a dashboard. Blocks
//! separate by space and announce themselves with a kicker. Fills are for
//! things you can point at: controls, chips, selection, and the surfaces that
//! genuinely float — dialogs, popovers, menus.

pub(super) use cibergit::ui::{block, chip_row, lines, notice, page, row};

use super::{ControlPresentation, Palette, field_label};
use cibergit::ui::{self, Density, TextRole};
use gpui::{prelude::*, *};
use gpui_base::Button;

/// A titled region of a page. The kicker is the whole of its chrome: no fill,
/// no border, no radius, no inset. Tracked uppercase at 11px reads as a heading
/// at a glance without taking the weight of a real title, which is what lets
/// the sections stack without a box each.
pub(super) fn section(title: &str, colors: Palette) -> Div {
    ui::block().child(ui::kicker(title).text_color(colors.faint))
}

/// A labelled control. The label is close enough to its control to belong to
/// it and no closer, which is the one relationship `GAP_FIELD` exists for.
pub(super) fn field(label: &str, colors: Palette) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(ui::GAP_FIELD))
        .child(field_label(label, colors))
}

/// One chip in a `chip_row`: a page tab, a Compare-bar mode, any segmented
/// choice. A trailing `count` is rendered in the faint colour rather than
/// repeating the label's weight.
///
/// The chip is painted small and the Button around it reserves the whole row as
/// its pointer target, the way sidebar icons do. It is a Button and not a
/// styled `div` because that is what makes it a real tab stop with a focus ring
/// that answers Enter and Space — the Compare bar's four chips were plain divs
/// and had none of that, while the page tabs directly above them did.
///
/// The caller adds the click handler and, where it applies, the disabled
/// presentation.
///
/// `id` must be unique among the chips in one row. Siblings sharing an element
/// id share the element's identity, and a press then lands on whichever of them
/// the framework reached first — the row still paints correctly and simply
/// stops answering. Rows whose chips come from a list have to vary it.
pub(super) fn chip(
    id: impl Into<SharedString>,
    label: &str,
    count: Option<usize>,
    selected: bool,
    colors: Palette,
) -> Button {
    let id: SharedString = id.into();
    let group = id.clone();
    let hover = id.clone();
    let selector = id.clone();
    let spoken = match count {
        Some(count) => format!("{label}  {count}"),
        None => label.to_owned(),
    };
    let label = label.to_owned();
    Button::new(id)
        .debug_selector(move || selector.to_string())
        .group(group)
        .h_full()
        .px_0()
        .py_0()
        .flex()
        .items_center()
        .rounded(px(ui::BADGE_RADIUS))
        .border_1()
        .border_color(rgba(0x00000000))
        .focus_ring(colors.accent, colors.selected)
        .cursor_pointer()
        .selected(selected)
        .accessibility_label(format!(
            "{spoken}{}",
            if selected { ", selected" } else { "" }
        ))
        .child(
            div()
                .h(px(ui::BUTTON_XS))
                .px(px(ui::GAP_GROUP))
                .flex()
                .items_center()
                .gap(px(ui::GAP_FIELD))
                .rounded(px(ui::BADGE_RADIUS))
                .ui_text(TextRole::Caption)
                .font_weight(ui::WEIGHT_EMPHASIS)
                .text_color(if selected { colors.text } else { colors.muted })
                .when(selected, |chip| chip.bg(colors.elevated))
                .when(!selected, |chip| {
                    chip.group_hover(hover, |chip| chip.bg(colors.selected))
                })
                .child(label)
                .when_some(count, |chip, count| {
                    chip.child(div().text_color(colors.faint).child(count.to_string()))
                }),
        )
}

/// The single-line text field on an ordinary panel.
pub(super) fn text_field(colors: Palette) -> Div {
    ui::text_field(colors.elevated, colors.border)
}

/// The multi-line text field on an ordinary panel.
pub(super) fn text_area(shown_lines: u16, colors: Palette) -> Div {
    ui::text_area(shown_lines, colors.elevated, colors.border)
}

/// A hairline. Structure inside content — a table header, a diff's columns —
/// keeps its rules; chrome is separated by space instead.
pub(super) fn rule(colors: Palette) -> Div {
    ui::rule(colors.border)
}
