//! Shared metrics for cibergit's dense desktop UI. See docs/ui-density.md.
use gpui::{prelude::*, *};

pub const GAP_ICON: f32 = 4.;
pub const GAP_FIELD: f32 = 6.;
pub const GAP_GROUP: f32 = 8.;
pub const GAP_COLUMNS: f32 = 12.;
pub const PANEL_GUTTER: f32 = 20.;
pub const GAP_PAGE: f32 = 24.;
pub const ROW_HEIGHT: f32 = 28.;
pub const TWO_LINE_ROW: f32 = 44.;
pub const CONTROL_HEIGHT: f32 = 28.;
pub const BUTTON_XS: f32 = 24.;
/// A row of chips — page tabs, the Compare bar, any segmented choice. The chip
/// is painted at `BUTTON_XS`; the row is the pointer target around it.
pub const CHIP_ROW_HEIGHT: f32 = 36.;
/// How far a chip row hangs left of its container's content edge: the painted
/// chip's own inset plus its focus-ring border. This is the one negative
/// margin in the app, and it is an optical correction rather than spacing — it
/// makes a resting chip label start on the same gutter as the prose above and
/// below it, which is what the eye reads as the page edge.
pub const CHIP_BLEED: f32 = GAP_GROUP + 1.;
pub const BUTTON_LG: f32 = 36.;
pub const CONTROL_RADIUS: f32 = 10.;
pub const CONTROL_INSET: f32 = 10.;
pub const ICON_SIZE: f32 = 14.;
pub const CELL_INSET: f32 = 10.;
pub const MENU_INSET: f32 = 6.;
pub const BADGE_HEIGHT: f32 = 20.;
pub const BADGE_RADIUS: f32 = 6.;
pub const BADGE_INSET: f32 = 8.;
pub const WINDOW_RADIUS: f32 = 12.;
pub const POPOVER_RADIUS: f32 = 10.;
pub const DESKTOP_HIT: f32 = 40.;
pub const TOUCH_HIT: f32 = 44.;

/// The type ladder runs above the stock weights: at 11-13px over translucent
/// chrome, Regular goes thin and grey. Running text stays at Medium, because
/// paragraphs of Semibold turn the panels into one grey slab; the step up goes
/// to the roles that are meant to carry it. Every role and every local emphasis
/// reads from these three so the whole app moves together. Bold is the ceiling
/// — IBM Plex Sans ships no heavier face — so emphasis and strong share a
/// weight and the kicker's tracking is what tells them apart.
pub const WEIGHT_TEXT: FontWeight = FontWeight::MEDIUM;
pub const WEIGHT_EMPHASIS: FontWeight = FontWeight::BOLD;
pub const WEIGHT_STRONG: FontWeight = FontWeight::BOLD;

/// Two families, split by job. IBM Plex Sans holds the headings: its flat
/// terminals and narrow set give the app its voice at display sizes. DM Sans
/// holds everything you actually read — at the 11-13px the rails, tables and
/// diffs live at, Plex turns cold and cramped, and DM Sans stays open.
pub const TITLE_FONT: &str = "IBM Plex Sans";
pub const TEXT_FONT: &str = "DM Sans";

#[derive(Clone, Copy)]
pub enum TextRole {
    Display,
    Title,
    Subtitle,
    Body,
    Label,
    Caption,
    Kicker,
}
impl TextRole {
    pub const fn metrics(self) -> (f32, f32, FontWeight) {
        match self {
            Self::Display => (24., 28., WEIGHT_EMPHASIS),
            Self::Title => (15., 20., WEIGHT_EMPHASIS),
            Self::Subtitle => (13., 18., WEIGHT_EMPHASIS),
            Self::Body => (12., 18., WEIGHT_TEXT),
            Self::Label => (12., 16., WEIGHT_EMPHASIS),
            Self::Caption => (11., 16., WEIGHT_TEXT),
            Self::Kicker => (11., 16., WEIGHT_STRONG),
        }
    }

    /// Only the headings name a face. The reading roles deliberately name none
    /// so they inherit: the window root sets `TEXT_FONT`, and a code surface
    /// that already set a monospace family keeps it. Three dozen chains read
    /// `.font_family(CODE_FONT).ui_text(TextRole::Body)` in that order, so a
    /// family named here would silently undo every one of them.
    pub const fn family(self) -> Option<&'static str> {
        match self {
            Self::Display | Self::Title | Self::Subtitle | Self::Kicker => Some(TITLE_FONT),
            Self::Body | Self::Label | Self::Caption => None,
        }
    }
}

pub trait Density: Styled + Sized {
    fn ui_text(self, role: TextRole) -> Self {
        let (size, line, weight) = role.metrics();
        let styled = self
            .text_size(px(size))
            .line_height(px(line))
            .font_weight(weight);
        match role.family() {
            Some(family) => styled.font_family(family),
            None => styled,
        }
    }
    fn control(self) -> Self {
        self.h(px(CONTROL_HEIGHT))
            .px(px(CONTROL_INSET))
            .py_0()
            .rounded(px(CONTROL_RADIUS))
            .flex()
            .items_center()
            .ui_text(TextRole::Label)
    }
    fn badge(self) -> Self {
        self.h(px(BADGE_HEIGHT))
            .px(px(BADGE_INSET))
            .py_0()
            .rounded(px(BADGE_RADIUS))
            .flex()
            .items_center()
            .ui_text(TextRole::Caption)
            .font_weight(WEIGHT_EMPHASIS)
    }
}
impl<T: Styled> Density for T {}

/// Tracking, in em, for the roles that can carry it. GPUI has no
/// letter-spacing style, so tracking costs one element per glyph: only short,
/// single-line labels that never wrap, truncate, or get selected use it.
pub const TRACKING_KICKER: f32 = 0.08;
pub const TRACKING_LABEL: f32 = 0.01;

/// Lays `text` out one glyph at a time so the gap between them stands in for
/// letter-spacing. The parent keeps a single accessible label.
pub fn tracked(text: &str, role: TextRole, em: f32) -> Div {
    let label = SharedString::from(text.to_string());
    let (size, _, _) = role.metrics();
    div().child(
        div()
            .id(SharedString::from(format!("tracked-{label}")))
            .ui_text(role)
            .flex()
            .items_center()
            .gap(px(size * em))
            .aria_label(label.clone())
            .children(label.chars().map(|character| {
                div().child(if character == ' ' {
                    // A lone space collapses in a flex row; hold it open.
                    SharedString::from("\u{00a0}")
                } else {
                    SharedString::from(character.to_string())
                })
            })),
    )
}

/// The palette-free half of the block vocabulary. `src/app/layout.rs` wraps
/// these with the review window's `Palette`; they live here so the standalone
/// `LocalWorkspace` harness, which mounts its module outside the application
/// crate root, builds the same shapes from its own colours.
///
/// The ladder is four steps, and every vertical relationship in the app is one
/// of them:
///
/// | Builder | Gap | What it separates |
/// | --- | ---: | --- |
/// | `page` | 24 | sections on a page |
/// | `block` | 8 | the items inside one section |
/// | `lines` / `row` | 4 | a run of caption lines; items on one line |
/// | field | 6 | a label from the control it names |
///
/// A container declares one `gap` and its children carry no margin. A margin
/// hung on each child restates the same relationship once per child, so it
/// drifts the moment anyone edits the list.
/// A scrolling page: the gutter around it, and the rhythm between its sections.
pub fn page() -> Div {
    div()
        .px(px(PANEL_GUTTER))
        .py(px(PANEL_GUTTER))
        .flex()
        .flex_col()
        .gap(px(GAP_PAGE))
}

/// A run of items that belong to one thought — a heading and its caption, a
/// statement and its detail, a stack of controls.
pub fn block() -> Div {
    div().flex().flex_col().gap(px(GAP_GROUP))
}

/// The tightest vertical run: consecutive caption lines that are really one
/// paragraph broken across elements. Anything looser makes them read as
/// separate claims.
pub fn lines() -> Div {
    div().flex().flex_col().gap(px(GAP_ICON))
}

/// Things that sit on one line: an icon and its label, a row of chips, a byline.
pub fn row() -> Div {
    div().flex().items_center().gap(px(GAP_ICON))
}

/// A single-line text field, tinted by the surface it sits on.
///
/// The box is a `control`, which is what puts the text on the control's centre
/// line. Six hand-built copies of this chain each fixed a 28px height and then
/// never centred anything inside it, so every caret in the app hung from the
/// top edge of its box with ten pixels of dead space under it; two of them had
/// no horizontal inset either and ran the text into the border.
pub fn text_field(fill: Rgba, border: Rgba) -> Div {
    div()
        .control()
        .font_family(TEXT_FONT)
        .ui_text(TextRole::Body)
        .bg(fill)
        .border_1()
        .border_color(border)
}

/// A multi-line text field, sized by how many lines of body text it shows
/// rather than by a pixel height chosen per call site — eleven call sites had
/// picked five different heights between them. Its inset matches the
/// single-line field's, so a caret starts on the same vertical line in both.
pub fn text_area(shown_lines: u16, fill: Rgba, border: Rgba) -> Div {
    let (_, line_height, _) = TextRole::Body.metrics();
    div()
        .h(px(line_height * f32::from(shown_lines)
            + CELL_INSET * 2.
            + 2.))
        .p(px(CELL_INSET))
        .rounded(px(CONTROL_RADIUS))
        .font_family(TEXT_FONT)
        .ui_text(TextRole::Body)
        .bg(fill)
        .border_1()
        .border_color(border)
        .overflow_hidden()
}

/// A row of chips. Every chip row in the app is this one, so two rows stacked
/// — the page tabs over the Compare bar — share a gutter, a pitch and a height
/// instead of each inventing its own.
///
/// It hangs `CHIP_BLEED` left of its container's content edge, so place it in a
/// container that already carries the page gutter and let it bleed back out.
pub fn chip_row() -> Div {
    div()
        .ml(px(-CHIP_BLEED))
        .h(px(CHIP_ROW_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .flex_wrap()
        .gap(px(GAP_ICON))
}

/// A full-bleed strip across a pane: a load error, a stale read, a warning
/// about the comparison. It spans the pane's gutter rather than sitting inside
/// it, so it reads as a condition of the pane and not as an item in it.
///
/// The tint is the whole signal. These strips used to carry a raised fill
/// behind the text as well, which said "this is a box" a beat before the colour
/// said "this is a warning" — and stacked two or three deep, as they do on a
/// stack page, the bands turned the top of the pane into a striped header.
pub fn notice(text: impl Into<SharedString>, tint: Rgba) -> Div {
    div()
        .px(px(PANEL_GUTTER))
        .py(px(GAP_GROUP))
        .ui_text(TextRole::Body)
        .text_color(tint)
        .child(text.into())
}

/// A hairline. Structure inside content — a table header, a diff's columns —
/// keeps its rules; chrome is separated by space instead.
pub fn rule(color: Rgba) -> Div {
    div().h(px(1.)).w_full().flex_none().bg(color)
}

/// An all-caps kicker at the widest tracking in the ladder.
pub fn kicker(text: &str) -> Div {
    tracked(&text.to_uppercase(), TextRole::Kicker, TRACKING_KICKER)
}
