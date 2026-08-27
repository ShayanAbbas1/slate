//! Slate's theme.
//!
//! Two rules hold this module together:
//!
//! 1. **Numbers drive layout, colours are paint.** Layout constants live in
//!    [`layout`] and never mix with colour tokens, so a spacing change cannot
//!    accidentally become a colour change.
//! 2. **Contrast is checked, not assumed.** Every text token is asserted
//!    against its background in the tests below. A palette edit that breaks
//!    WCAG AA fails the build.
//!
//! Dark and light are designed separately rather than mirrored, but they share
//! one elevation rule: **the closer a surface is to the data, the more light it
//! gets.** Results are the brightest plane, the editor sits one tone behind
//! them, and chrome recedes furthest — never near-black anywhere, which reads
//! as a hole rather than a surface. Tone steps, not hairlines, are what
//! separate the planes; borders are reserved for floating overlays. The one
//! structural seam, the sidebar edge, is drawn by its drag handle.
//!
//! The window is vibrant, so every plane is also a glass: see [`Theme::frost`].

pub mod color;

use color::{Oklch, Rgba, Srgb};
use gpui_component::{
    ThemeMode,
    highlighter::{HighlightTheme, HighlightThemeStyle, SyntaxColors},
};
use serde_json::json;
use std::sync::Arc;

/// Layout scale. Deliberately tiny — four spacing values and three radii.
/// An arbitrary one-off pixel value in a component is a code review failure.
pub mod layout {
    pub const SPACE_XS: f32 = 4.0;
    pub const SPACE_SM: f32 = 8.0;
    pub const SPACE_MD: f32 = 12.0;
    pub const SPACE_LG: f32 = 16.0;

    /// Type scale. Body is 13, not gpui-component's 16: a database client is a
    /// dense surface, and the library default reads as a demo blown up for a
    /// projector. `XS` is for the uppercase section labels only, which is why it
    /// is allowed to sit below the readable body minimum.
    pub const TEXT_XS: f32 = 11.0;
    pub const TEXT_SM: f32 = 12.0;
    pub const TEXT_MD: f32 = 13.0;
    pub const TEXT_LG: f32 = 16.0;

    /// One icon size everywhere. Icons here label rows and buttons; nothing in
    /// Slate is an illustration, so a second size would only be decoration.
    pub const ICON_SIZE: f32 = 14.0;

    pub const RADIUS_CONTROL: f32 = 6.0;
    pub const RADIUS_PANEL: f32 = 10.0;
    pub const RADIUS_LARGE: f32 = 16.0;

    pub const TITLEBAR_HEIGHT: f32 = 38.0;
    /// Where the titlebar's own content can start without colliding with the
    /// platform's window buttons, which are drawn over it.
    pub const TITLEBAR_LEADING_INSET: f32 = 78.0;
    pub const STATUS_HEIGHT: f32 = 24.0;
    pub const TAB_HEIGHT: f32 = 34.0;
    /// A tab is a chip inside the strip, so it gets a chip height rather than
    /// the full bar.
    pub const TAB_CHIP_HEIGHT: f32 = 26.0;
    pub const SWITCHER_HEIGHT: f32 = 40.0;
    pub const EDITOR_EMPTY_HEIGHT: f32 = 680.0;
    pub const EDITOR_DEFAULT_HEIGHT: f32 = 420.0;
    pub const EDITOR_MIN_HEIGHT: f32 = 120.0;
    pub const EDITOR_MAX_HEIGHT: f32 = 720.0;
    pub const RESULTS_EMPTY_HEIGHT: f32 = 100.0;
    pub const RESULTS_DEFAULT_HEIGHT: f32 = 360.0;
    pub const RESULTS_MIN_HEIGHT: f32 = 100.0;
    /// The row inspector beside the grid. Fixed: it is read, not worked in, and
    /// a second draggable split inside the results pane is a handle nobody
    /// asked for.
    pub const INSPECTOR_WIDTH: f32 = 300.0;
    pub const SIDEBAR_DEFAULT_WIDTH: f32 = 256.0;
    pub const SIDEBAR_MIN_WIDTH: f32 = 180.0;
    pub const SIDEBAR_MAX_WIDTH: f32 = 480.0;
    pub const DIALOG_WIDTH: f32 = 420.0;
    /// The palette. Wide enough for a schema-qualified name and its kind, and
    /// capped so a catalog of thousands scrolls rather than filling the window.
    pub const PALETTE_WIDTH: f32 = 520.0;
    pub const PALETTE_MAX_HEIGHT: f32 = 360.0;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Appearance {
    #[default]
    Dark,
    Light,
}

impl gpui::Global for Theme {}

impl Default for Theme {
    fn default() -> Self {
        Self::all()[0]
    }
}

/// Read the active theme. Panics unless [`Theme::apply_to_components`] and
/// `cx.set_global` have run, which `main` does before opening the window.
pub fn theme(cx: &gpui::App) -> &Theme {
    cx.global::<Theme>()
}

impl From<Srgb> for gpui::Hsla {
    fn from(c: Srgb) -> Self {
        c.opaque().into()
    }
}

impl From<Rgba> for gpui::Hsla {
    fn from(c: Rgba) -> Self {
        gpui::Rgba {
            r: c.r,
            g: c.g,
            b: c.b,
            a: c.a,
        }
        .into()
    }
}

// `bg()` takes Into<Fill> rather than Into<Hsla>, so tokens need this to be
// passed directly instead of converted at every call site.
impl From<Srgb> for gpui::Fill {
    fn from(c: Srgb) -> Self {
        gpui::Hsla::from(c).into()
    }
}

impl From<Rgba> for gpui::Fill {
    fn from(c: Rgba) -> Self {
        gpui::Hsla::from(c).into()
    }
}

/// Hue for the neutral ramp. A trace of blue reads as "considered"; a true
/// grey reads as unfinished. The chroma is low enough that it never looks tinted.
const NEUTRAL_HUE: f32 = 260.0;
const NEUTRAL_CHROMA: f32 = 0.004;

/// Hairlines need proportionally more alpha on light backgrounds than dark ones
/// to stay visible without turning into a hard stroke.
const HAIRLINE_DARK: f32 = 0.09;
const HAIRLINE_LIGHT: f32 = 0.12;

/// The glass, for the themes that ask for it. The window background is the
/// blurred desktop, so each plane is a tint over it rather than a fill, and the
/// elevation rule reads a second way: the closer a surface is to the data, the
/// less it lets through.
///
/// The tints stack — [`Theme::frost`] is painted once by the window root and
/// everything else sits on it — so these are what shows over the frost, not
/// what reaches the desktop.
///
/// They are close together, and that is the whole trick. What a plane
/// transmits is not a look, it is how much it *moves* — a surface at 22% is
/// restated by whatever the window happens to be sitting on, so a sidebar that
/// transmits twice what the editor does is not one step lighter, it is a
/// shifting bright patch beside a stable dark one. Stacked, the transmissions
/// multiply: chrome is `1 - FROST`, and the planes over it are that again
/// times their own. Keeping the three within a few points of each other is
/// what lets the tone ramp, rather than the desktop, say which plane is which.
const FROST_ALPHA: f32 = 0.86;
const PANEL_ALPHA: f32 = 0.35;
const DATA_ALPHA: f32 = 0.55;

fn neutral(lightness: f32) -> Srgb {
    Oklch::new(lightness, NEUTRAL_CHROMA, NEUTRAL_HUE).to_srgb()
}

const WHITE: Srgb = Srgb::new(1.0, 1.0, 1.0);
const BLACK: Srgb = Srgb::new(0.0, 0.0, 0.0);
const TRANSPARENT: Rgba = Rgba {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 0.0,
};

/// Every colour Slate paints. Flat fields, not nested groups — a token you have
/// to go looking for gets duplicated instead of reused.
///
/// A new theme is one constructor returning this struct plus one entry in
/// [`Theme::all`]. Nothing else in the app names a theme, so anything that fills
/// every field in is already fully supported — including its contrast tests.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    pub appearance: Appearance,
    /// Whether this theme paints on the blurred desktop rather than on itself.
    /// A glass theme's planes are tints, its window background is vibrant, and
    /// its palette has to be built for that — see [`Theme::glass`].
    pub is_glass: bool,

    /// The content plane, and the brightest tone: the results. The data is
    /// what Slate exists to show, so it gets the most light.
    pub bg: Srgb,
    /// One step behind `bg`: the editor's page and the active tab — the
    /// prompt, not the answer. Three planes rather than two because an editor
    /// over a grid over a sidebar is three surfaces, and two tones make one of
    /// the boundaries invisible.
    pub panel: Srgb,
    /// Chrome, furthest back: sidebar, titlebar, status bar, table headers.
    pub surface: Srgb,
    /// The floating plane: the connection switcher and anything else that sits
    /// over chrome. One step past `surface` in dark themes; in light themes it
    /// stays white and earns its elevation from a border and shadow instead,
    /// because "lighter than white" does not exist.
    pub overlay: Srgb,

    pub element_hover: Rgba,
    pub element_active: Rgba,

    /// A control that must read as pressable at rest: buttons. A solid tone of
    /// its own rather than a wash, because a wash only reads as a button once
    /// the pointer is already on it. Deliberately neutral — a coloured button
    /// shouts in an interface that is otherwise tone-on-tone.
    pub control: Srgb,

    pub border: Rgba,
    pub border_strong: Rgba,

    pub text: Srgb,
    pub text_muted: Srgb,
    pub text_faint: Srgb,

    pub accent: Srgb,
    pub on_accent: Srgb,
    pub selection: Rgba,
    pub cursor: Srgb,
    /// A grid cell the user has changed and not yet applied. A wash under the
    /// value rather than a colour on it, so the value still reads as the value —
    /// and a hue of its own, because the accent wash next to it already means
    /// "selected".
    pub edited: Rgba,

    pub danger: Srgb,
    pub success: Srgb,

    pub syntax_comment: Srgb,
    pub syntax_keyword: Srgb,
    pub syntax_string: Srgb,
    pub syntax_number: Srgb,
    pub syntax_function: Srgb,
    pub syntax_type: Srgb,
    pub syntax_variable: Srgb,
    pub syntax_operator: Srgb,
}

impl Theme {
    /// Every theme Slate ships, in the order the switcher cycles them. The
    /// first is the default.
    pub fn all() -> [Self; 3] {
        [Self::glass(), Self::dark(), Self::light()]
    }

    /// The theme after this one, by name. Falls back to the default, so a theme
    /// deleted from `all` cannot strand the app on a name that no longer exists.
    pub fn next(self) -> Self {
        let themes = Self::all();
        let index = themes
            .iter()
            .position(|theme| theme.name == self.name)
            .map(|index| (index + 1) % themes.len())
            .unwrap_or(0);
        themes[index]
    }

    /// The window frost: the chrome tone over the blurred desktop, and the
    /// plane every other surface is layered on. Chrome — sidebar, titlebar, tab
    /// strip, status bar — paints nothing of its own and is this.
    pub fn frost(self) -> Rgba {
        self.surface.alpha(self.tint(FROST_ALPHA))
    }

    /// The editor's page, as a tint over [`Theme::frost`].
    pub fn panel_glass(self) -> Rgba {
        self.panel.alpha(self.tint(PANEL_ALPHA))
    }

    /// The results plane, as a tint over [`Theme::frost`]. The most solid of the
    /// three: a dense grid read against a moving wallpaper is not read at all.
    pub fn data_glass(self) -> Rgba {
        self.bg.alpha(self.tint(DATA_ALPHA))
    }

    /// An opaque theme has no desktop behind it to let through, so every tint
    /// collapses to its own fill and the planes paint exactly as they did
    /// before glass existed. One gate here rather than a branch at each of the
    /// nine call sites.
    fn tint(self, alpha: f32) -> f32 {
        if self.is_glass { alpha } else { 1.0 }
    }

    /// What the window's own background is. A glass theme sits on the blurred
    /// desktop; every other theme paints its own chrome and would only show the
    /// raw desktop through the gaps.
    pub fn window_background(self) -> gpui::WindowBackgroundAppearance {
        if self.is_glass {
            gpui::WindowBackgroundAppearance::Blurred
        } else {
            gpui::WindowBackgroundAppearance::Opaque
        }
    }

    pub fn apply_to_components(self, cx: &mut gpui::App) {
        let component = gpui_component::Theme::global_mut(cx);
        component.shadow = false;
        component.radius = gpui::px(layout::RADIUS_CONTROL);
        component.radius_lg = gpui::px(layout::RADIUS_LARGE);
        component.font_size = gpui::px(layout::TEXT_MD);
        component.mono_font_size = gpui::px(layout::TEXT_MD);
        component.font_family = ".ZedSans".into();
        component.mono_font_family = ".ZedMono".into();

        // The one plane painted below Slate's own tree, by `Root`. It is the
        // frost, so removing a `bg` from a chrome element uncovers glass rather
        // than a hole.
        component.colors.background = self.frost().into();
        component.colors.foreground = self.text.into();
        component.colors.input = self.border.into();
        component.colors.border = self.border.into();
        component.colors.caret = self.cursor.into();
        component.colors.selection = self.selection.into();
        component.colors.ring = self.accent.into();
        component.colors.muted = self.surface.into();
        component.colors.muted_foreground = self.text_muted.into();
        component.colors.popover = self.overlay.into();
        component.colors.popover_foreground = self.text.into();
        // Both button variants are the same neutral: a Slate button is a grey
        // that steps visibly brighter under the pointer, never a colour.
        // Colour is reserved for state (selection, danger), not for controls.
        let control_hover = self.element_active.flatten(self.control);
        let control_active = self.element_active.flatten(control_hover);
        component.colors.primary = self.control.into();
        component.colors.primary_foreground = self.text.into();
        component.colors.primary_hover = control_hover.into();
        component.colors.primary_active = control_active.into();
        component.colors.secondary = self.control.into();
        component.colors.secondary_foreground = self.text.into();
        component.colors.secondary_hover = control_hover.into();
        component.colors.secondary_active = control_active.into();
        // What a ghost button washes with on hover -- the tab pair lives on it.
        component.colors.accent = self.element_hover.flatten(self.panel).into();
        component.colors.accent_foreground = self.text.into();
        component.colors.danger = self.danger.into();
        component.colors.danger_foreground = self.on_accent.into();
        component.colors.danger_hover = self.element_hover.flatten(self.danger).into();
        component.colors.danger_active = self.element_active.flatten(self.danger).into();
        component.colors.success = self.success.into();
        component.colors.success_foreground = self.on_accent.into();
        component.colors.success_hover = self.element_hover.flatten(self.success).into();
        component.colors.success_active = self.element_active.flatten(self.success).into();
        component.colors.list = self.surface.into();
        component.colors.list_even = self.surface.into();
        component.colors.list_head = self.surface.into();
        component.colors.list_hover = self.element_hover.into();
        component.colors.list_active = self.selection.into();
        component.colors.list_active_border = self.accent.into();
        component.colors.scrollbar = self.panel.into();
        component.colors.scrollbar_thumb = self.border_strong.into();
        component.colors.scrollbar_thumb_hover = self.element_active.into();
        // The results pane already paints `data_glass` behind the grid, and a
        // second tint over it would only stack toward opaque.
        component.colors.table = TRANSPARENT.into();
        component.colors.table_active = self.selection.into();
        component.colors.table_active_border = self.accent.into();
        component.colors.table_even = self.element_hover.into();
        component.colors.table_head = self.panel.into();
        component.colors.table_head_foreground = self.text_muted.into();
        component.colors.table_hover = self.element_hover.into();
        // Stripes carry the rows; a hairline under every row as well is the
        // grid equivalent of ruled paper under print.
        component.colors.table_row_border = TRANSPARENT.into();
        component.highlight_theme = self.highlight_theme();
    }

    /// Built through serde because `ThemeStyle`'s fields are private and it has
    /// no constructor — deserialization is the only way to make one from here.
    fn highlight_theme(self) -> Arc<HighlightTheme> {
        let style = |color: Srgb| json!({ "color": color.hex() });
        let syntax: SyntaxColors = serde_json::from_value(json!({
            "attribute": style(self.syntax_variable),
            "boolean": style(self.syntax_number),
            "comment": style(self.syntax_comment),
            "comment_doc": style(self.syntax_comment),
            "constant": style(self.syntax_number),
            "constructor": style(self.syntax_type),
            "embedded": style(self.text),
            "emphasis": style(self.text),
            "emphasis.strong": style(self.text),
            "enum": style(self.syntax_type),
            "function": style(self.syntax_function),
            "hint": style(self.text_muted),
            "keyword": style(self.syntax_keyword),
            "label": style(self.syntax_variable),
            "link_text": style(self.syntax_function),
            "link_uri": style(self.syntax_string),
            "number": style(self.syntax_number),
            "operator": style(self.syntax_operator),
            "predictive": style(self.text_faint),
            "preproc": style(self.syntax_keyword),
            "primary": style(self.text),
            "property": style(self.syntax_variable),
            "punctuation": style(self.syntax_operator),
            "punctuation.bracket": style(self.syntax_operator),
            "punctuation.delimiter": style(self.syntax_operator),
            "punctuation.list_marker": style(self.syntax_operator),
            "punctuation.special": style(self.syntax_keyword),
            "string": style(self.syntax_string),
            "string.escape": style(self.syntax_number),
            "string.regex": style(self.syntax_string),
            "string.special": style(self.syntax_string),
            "string.special.symbol": style(self.syntax_string),
            "tag": style(self.syntax_keyword),
            "tag.doctype": style(self.syntax_keyword),
            "text.literal": style(self.syntax_string),
            "title": style(self.syntax_function),
            "type": style(self.syntax_type),
            "variable": style(self.syntax_variable),
            "variable.special": style(self.syntax_keyword),
            "variant": style(self.syntax_type)
        }))
        // Unstyled syntax is a bad afternoon; a window that will not open is a
        // worse one. A gpui-component bump that renames a key must not be able
        // to stop the app from starting -- the test below is what catches it.
        .unwrap_or_default();

        Arc::new(HighlightTheme {
            name: "Slate".into(),
            appearance: match self.appearance {
                Appearance::Dark => ThemeMode::Dark,
                Appearance::Light => ThemeMode::Light,
            },
            style: HighlightThemeStyle {
                // Transparent, not the editor's tone. The highlighter fills the
                // gutter and the ghost line with this, and it fills them over a
                // page that has already painted itself -- so naming a colour
                // here only repaints the plane, at full opacity, on top of the
                // tint that was supposed to be showing. That is invisible in an
                // opaque theme and is the whole editor in a glass one.
                editor_background: Some(TRANSPARENT.into()),
                editor_foreground: Some(self.text.into()),
                editor_active_line: Some(self.element_hover.into()),
                editor_line_number: Some(self.text_faint.into()),
                editor_active_line_number: Some(self.text_muted.into()),
                status: Default::default(),
                syntax,
            },
        })
    }

    /// The vibrant theme, and the default: the window background is the blurred
    /// desktop and every plane is a tint over it rather than a fill.
    ///
    /// Near-black where [`Theme::dark`] is deliberately grey, which reverses
    /// that theme's one rule for a reason. A dark plane reads as a void only
    /// when there is nothing behind it; over vibrancy there is a whole desktop
    /// behind it, and near-black is what turns that into smoked glass. A mid
    /// grey turns it into dirt — the wallpaper's own light lands in the same
    /// band as the tone and the two never resolve into either one.
    ///
    /// The steps between the planes are half of dark's for the same reason:
    /// they are read through a moving backdrop, and tone that survives on an
    /// opaque page reads as patchiness on a transparent one. What separates the
    /// planes here is mostly how much they let through — see [`FROST_ALPHA`].
    pub fn glass() -> Self {
        Self {
            name: "Slate Glass",
            is_glass: true,

            // Chrome goes near-black and stays there — it is the plane with
            // nothing to read on it, so it can afford to be mostly desktop.
            // The two that carry text climb back out, because a tone below the
            // frost's own composite reads as a hole punched in the window: the
            // frost carries the desktop's light, and a tint darker than that
            // subtracts it.
            bg: neutral(0.275),
            panel: neutral(0.215),
            surface: neutral(0.130),
            overlay: neutral(0.300),

            control: neutral(0.340),

            ..Self::dark()
        }
    }

    /// The tones run chrome → editor → results, dark grey to lighter grey:
    /// the answer gets the light, the prompt sits a step behind it. Near-black
    /// is deliberately absent: a plane at 4% lightness reads as a void — which
    /// holds for a page that paints itself, and is exactly what [`Theme::glass`]
    /// gets to ignore.
    pub fn dark() -> Self {
        Self {
            name: "Slate Dark",
            appearance: Appearance::Dark,
            is_glass: false,

            bg: neutral(0.300),
            panel: neutral(0.260),
            surface: neutral(0.220),
            overlay: neutral(0.350),

            element_hover: WHITE.alpha(0.05),
            element_active: WHITE.alpha(0.09),

            control: neutral(0.380),

            border: WHITE.alpha(HAIRLINE_DARK),
            border_strong: WHITE.alpha(0.16),

            // Not a pure white. The last few percent of lightness reads as
            // glare rather than crispness, and a dense result grid is where
            // that gets tiring.
            text: neutral(0.93),
            text_muted: neutral(0.76),
            text_faint: neutral(0.62),

            accent: Oklch::new(0.68, 0.15, 250.0).to_srgb(),
            on_accent: neutral(0.14),
            selection: Oklch::new(0.68, 0.15, 250.0).to_srgb().alpha(0.28),
            cursor: Oklch::new(0.72, 0.14, 250.0).to_srgb(),
            edited: Oklch::new(0.60, 0.15, 75.0).to_srgb().alpha(0.32),

            danger: Oklch::new(0.70, 0.19, 25.0).to_srgb(),
            success: Oklch::new(0.72, 0.15, 150.0).to_srgb(),

            syntax_comment: neutral(0.68),
            syntax_keyword: Oklch::new(0.78, 0.13, 300.0).to_srgb(),
            syntax_string: Oklch::new(0.78, 0.13, 150.0).to_srgb(),
            syntax_number: Oklch::new(0.82, 0.12, 75.0).to_srgb(),
            syntax_function: Oklch::new(0.78, 0.12, 250.0).to_srgb(),
            syntax_type: Oklch::new(0.80, 0.10, 205.0).to_srgb(),
            syntax_variable: neutral(0.90),
            syntax_operator: neutral(0.74),
        }
    }

    pub fn light() -> Self {
        Self {
            name: "Slate Light",
            appearance: Appearance::Light,
            is_glass: false,

            bg: WHITE,
            panel: neutral(0.972),
            surface: neutral(0.940),
            overlay: WHITE,

            element_hover: BLACK.alpha(0.04),
            element_active: BLACK.alpha(0.08),

            control: neutral(0.920),

            border: BLACK.alpha(HAIRLINE_LIGHT),
            border_strong: BLACK.alpha(0.20),

            // Soft ink, not near-black: it keeps the two appearances in the
            // same contrast neighbourhood now that the dark page is grey.
            text: neutral(0.26),
            text_muted: neutral(0.45),
            text_faint: neutral(0.58),

            accent: Oklch::new(0.52, 0.17, 250.0).to_srgb(),
            on_accent: WHITE,
            selection: Oklch::new(0.52, 0.17, 250.0).to_srgb().alpha(0.20),
            cursor: Oklch::new(0.48, 0.18, 250.0).to_srgb(),
            edited: Oklch::new(0.80, 0.14, 75.0).to_srgb().alpha(0.30),

            danger: Oklch::new(0.52, 0.20, 25.0).to_srgb(),
            success: Oklch::new(0.52, 0.15, 150.0).to_srgb(),

            syntax_comment: neutral(0.46),
            syntax_keyword: Oklch::new(0.48, 0.16, 300.0).to_srgb(),
            syntax_string: Oklch::new(0.44, 0.14, 150.0).to_srgb(),
            syntax_number: Oklch::new(0.48, 0.15, 65.0).to_srgb(),
            syntax_function: Oklch::new(0.46, 0.16, 250.0).to_srgb(),
            syntax_type: Oklch::new(0.44, 0.12, 205.0).to_srgb(),
            syntax_variable: neutral(0.28),
            syntax_operator: neutral(0.40),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::color::contrast_ratio;
    use super::*;

    /// WCAG AA: 4.5 for body text, 3.0 for large text and UI components.
    /// AAA: 7.0. Slate holds body text to AAA because a result grid is dense.
    const AAA_TEXT: f32 = 7.0;
    const AA_TEXT: f32 = 4.5;
    const AA_LARGE: f32 = 3.0;

    fn check(theme: Theme, name: &str, fg: Srgb, bg: Srgb, minimum: f32) {
        let ratio = contrast_ratio(fg, bg);
        assert!(
            ratio >= minimum,
            "{}: {name}: contrast {ratio:.2} is below the {minimum:.1} floor",
            theme.name
        );
    }

    #[test]
    fn syntax_tokens_clear_wcag_in_every_theme() {
        for theme in Theme::all() {
            for (name, token) in [
                ("comment", theme.syntax_comment),
                ("keyword", theme.syntax_keyword),
                ("string", theme.syntax_string),
                ("number", theme.syntax_number),
                ("function", theme.syntax_function),
                ("type", theme.syntax_type),
                ("variable", theme.syntax_variable),
                ("operator", theme.syntax_operator),
            ] {
                // Against the editor's page, which is where SQL is read.
                check(theme, name, token, theme.panel, AA_TEXT);
            }
        }
    }

    #[test]
    fn every_highlighter_category_has_a_slate_style() {
        for theme in Theme::all() {
            let syntax = serde_json::to_value(&theme.highlight_theme().style.syntax).unwrap();
            let styles = syntax.as_object().unwrap();
            let missing = styles
                .iter()
                .filter_map(|(name, style)| style.is_null().then_some(name.as_str()))
                .collect::<Vec<_>>();
            assert_eq!(styles.len(), 40);
            assert!(
                missing.is_empty(),
                "highlighter categories fell back to the component theme: {missing:?}"
            );
        }
    }

    #[test]
    fn text_contrast_clears_wcag_in_every_theme() {
        for t in Theme::all() {
            check(t, "text on bg", t.text, t.bg, AAA_TEXT);
            check(t, "text on panel", t.text, t.panel, AAA_TEXT);
            check(t, "text on surface", t.text, t.surface, AAA_TEXT);
            check(t, "muted on bg", t.text_muted, t.bg, AA_TEXT);
            check(t, "muted on panel", t.text_muted, t.panel, AA_TEXT);
            check(t, "muted on surface", t.text_muted, t.surface, AA_TEXT);
            check(t, "text on overlay", t.text, t.overlay, AAA_TEXT);
            check(t, "muted on overlay", t.text_muted, t.overlay, AA_TEXT);
            check(t, "faint on bg", t.text_faint, t.bg, AA_LARGE);
            check(t, "accent on bg", t.accent, t.bg, AA_LARGE);
            // Query errors are written in `danger` at body size, not as a badge.
            check(t, "danger on bg", t.danger, t.bg, AA_TEXT);
            check(t, "danger on panel", t.danger, t.panel, AA_TEXT);
            check(t, "success on surface", t.success, t.surface, AA_LARGE);
            check(t, "on_accent over accent", t.on_accent, t.accent, AA_LARGE);
            // Button labels are body-size UI text on the control tone.
            check(t, "text on control", t.text, t.control, AA_TEXT);
            // A changed cell is still a cell in the dense grid: the wash marks
            // it, it does not get to make the value harder to read.
            check(
                t,
                "text on an edited cell",
                t.text,
                t.edited.flatten(t.bg),
                AAA_TEXT,
            );
        }
    }

    #[test]
    fn themes_are_comparable_not_mirrored() {
        // Every theme should land in the same contrast neighbourhood, so
        // switching does not make one of them feel washed out next to another.
        let ratios = Theme::all().map(|theme| contrast_ratio(theme.text, theme.bg));
        let spread = ratios.iter().cloned().fold(f32::MIN, f32::max)
            - ratios.iter().cloned().fold(f32::MAX, f32::min);
        assert!(spread < 6.0, "themes drifted apart: {ratios:?}");
    }

    #[test]
    fn elevation_runs_the_right_way_in_every_theme() {
        // One rule for both appearances: the closer to the data, the brighter.
        // Results over the editor's page over chrome — never a hole.
        for theme in Theme::all() {
            assert!(
                theme.bg.relative_luminance() > theme.panel.relative_luminance(),
                "{}: results must sit brighter than the editor's page",
                theme.name
            );
            assert!(
                theme.panel.relative_luminance() > theme.surface.relative_luminance(),
                "{}: the editor's page must sit brighter than chrome",
                theme.name
            );
        }
    }

    #[test]
    fn the_three_planes_are_told_apart_at_a_glance() {
        // The complaint this exists to catch: an editor, a grid and a sidebar
        // all within a few sRGB levels of each other read as one black
        // rectangle. Measured in levels rather than contrast ratio, because at
        // near-black the ratio's flare term compresses every step into noise —
        // #0a and #16 differ by 12 levels and score 1.09.
        //
        // Opaque themes only, and not for want of trying: a glass theme has no
        // fixed answer to grade. Its planes are tints over a frost that is
        // itself transparent, so a plane's step over the one behind it is
        // `alpha * (tint - frost)` — and `frost` moves with the wallpaper. Over
        // a dark desktop the planes compress toward each other; over a bright
        // one the step inverts and the editor lands darker than the sidebar it
        // is supposed to sit in front of. No palette fixes that, because the
        // term that flips is the desktop. It is what vibrancy costs.
        //
        // What still holds for glass is checked elsewhere: the tone ramp runs
        // the right way in `elevation_runs_the_right_way_in_every_theme`, and
        // every text token clears WCAG against its raw tint, with no credit for
        // whatever light the wallpaper happens to add.
        let level = |c: Srgb| (c.r + c.g + c.b) / 3.0 * 255.0;
        for t in Theme::all().into_iter().filter(|t| !t.is_glass) {
            for (name, near, far) in [
                ("bg to panel", t.bg, t.panel),
                ("panel to surface", t.panel, t.surface),
            ] {
                let step = (level(near) - level(far)).abs();
                assert!(
                    step >= 8.0,
                    "{} {name}: {step:.1} levels is not a visible step",
                    t.name
                );
            }
        }
    }

    #[test]
    fn hairlines_are_visible_but_soft() {
        for t in Theme::all() {
            let border = t.border.flatten(t.bg);
            let ratio = contrast_ratio(border, t.bg);
            assert!(ratio > 1.10, "{} border is invisible: {ratio:.3}", t.name);
            assert!(
                ratio < 2.20,
                "{} border reads as a stroke: {ratio:.3}",
                t.name
            );
        }
    }

    #[test]
    fn the_switcher_visits_every_theme_and_comes_back() {
        let mut theme = Theme::default();
        let mut seen = vec![theme.name];
        for _ in 1..Theme::all().len() {
            theme = theme.next();
            assert!(!seen.contains(&theme.name), "{} repeated", theme.name);
            seen.push(theme.name);
        }
        assert_eq!(theme.next().name, Theme::default().name);
    }
}
