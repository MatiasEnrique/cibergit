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
            Self::Display => (24., 28., FontWeight::MEDIUM),
            Self::Title => (15., 20., FontWeight::MEDIUM),
            Self::Subtitle => (13., 18., FontWeight::MEDIUM),
            Self::Body => (12., 18., FontWeight::NORMAL),
            Self::Label => (12., 16., FontWeight::MEDIUM),
            Self::Caption => (11., 16., FontWeight::NORMAL),
            Self::Kicker => (11., 16., FontWeight::SEMIBOLD),
        }
    }
}

pub trait Density: Styled + Sized {
    fn ui_text(self, role: TextRole) -> Self {
        let (size, line, weight) = role.metrics();
        self.text_size(px(size))
            .line_height(px(line))
            .font_weight(weight)
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
            .font_weight(FontWeight::MEDIUM)
    }
}
impl<T: Styled> Density for T {}

/// GPUI has no letter-spacing style. Separate glyphs give kickers explicit
/// 0.08em tracking while the parent retains one accessible label.
pub fn kicker(text: &str) -> Div {
    let label = text.to_uppercase();
    div().child(
        div()
            .id(SharedString::from(format!("kicker-{label}")))
            .ui_text(TextRole::Kicker)
            .flex()
            .items_center()
            .gap(px(11. * 0.08))
            .aria_label(label.clone())
            .children(
                label
                    .chars()
                    .map(|character| div().child(character.to_string())),
            ),
    )
}
