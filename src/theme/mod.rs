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
//! Dark and light are designed separately rather than mirrored. In dark, the
//! content plane is the darkest surface and chrome sits one step lighter; in
//! light, the content plane is white and chrome sits one step darker. Inverting
//! lightness would put the elevation the wrong way round.

pub mod color;

use color::{Oklch, Rgba, Srgb};

/// Layout scale. Deliberately tiny — four spacing values and three radii.
/// An arbitrary one-off pixel value in a component is a code review failure.
pub mod layout {
    pub const SPACE_XS: f32 = 4.0;
    pub const SPACE_SM: f32 = 8.0;
    pub const SPACE_MD: f32 = 12.0;
    pub const SPACE_LG: f32 = 16.0;

    pub const RADIUS_CONTROL: f32 = 6.0;
    pub const RADIUS_PANEL: f32 = 10.0;
    pub const RADIUS_LARGE: f32 = 16.0;

    pub const TITLEBAR_HEIGHT: f32 = 38.0;
    pub const STATUS_HEIGHT: f32 = 24.0;
    pub const SIDEBAR_DEFAULT_WIDTH: f32 = 256.0;
    pub const SIDEBAR_MIN_WIDTH: f32 = 180.0;
    pub const SIDEBAR_MAX_WIDTH: f32 = 420.0;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Appearance {
    #[default]
    Dark,
    Light,
}

impl gpui::Global for Theme {}

/// Read the active theme. Panics if [`Theme::install`] has not run, which can
/// only happen if a view renders before app setup.
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

fn neutral(lightness: f32) -> Srgb {
    Oklch::new(lightness, NEUTRAL_CHROMA, NEUTRAL_HUE).to_srgb()
}

const WHITE: Srgb = Srgb::new(1.0, 1.0, 1.0);
const BLACK: Srgb = Srgb::new(0.0, 0.0, 0.0);

/// Every colour Slate paints. Flat fields, not nested groups — a token you have
/// to go looking for gets duplicated instead of reused.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub appearance: Appearance,

    /// The content plane. Result grids and editors sit on this.
    pub bg: Srgb,
    /// Chrome one step from `bg`: sidebar, tab strip, status bar.
    pub surface: Srgb,
    /// Popovers, dialogs, the value inspector.
    pub surface_raised: Srgb,

    pub element_hover: Rgba,
    pub element_active: Rgba,

    pub border: Rgba,
    pub border_strong: Rgba,

    pub text: Srgb,
    pub text_muted: Srgb,
    pub text_faint: Srgb,

    pub accent: Srgb,
    pub on_accent: Srgb,
    pub selection: Rgba,
    pub cursor: Srgb,

    pub danger: Srgb,
    pub warning: Srgb,
    pub success: Srgb,
}

impl Theme {
    pub fn new(appearance: Appearance) -> Self {
        match appearance {
            Appearance::Dark => Self::dark(),
            Appearance::Light => Self::light(),
        }
    }

    pub fn dark() -> Self {
        Self {
            appearance: Appearance::Dark,

            bg: neutral(0.155),
            surface: neutral(0.195),
            surface_raised: neutral(0.235),

            element_hover: WHITE.alpha(0.05),
            element_active: WHITE.alpha(0.09),

            border: WHITE.alpha(HAIRLINE_DARK),
            border_strong: WHITE.alpha(0.16),

            text: neutral(0.96),
            text_muted: neutral(0.74),
            text_faint: neutral(0.58),

            accent: Oklch::new(0.68, 0.15, 250.0).to_srgb(),
            on_accent: neutral(0.14),
            selection: Oklch::new(0.68, 0.15, 250.0).to_srgb().alpha(0.28),
            cursor: Oklch::new(0.72, 0.14, 250.0).to_srgb(),

            danger: Oklch::new(0.68, 0.19, 25.0).to_srgb(),
            warning: Oklch::new(0.80, 0.13, 85.0).to_srgb(),
            success: Oklch::new(0.72, 0.15, 150.0).to_srgb(),
        }
    }

    pub fn light() -> Self {
        Self {
            appearance: Appearance::Light,

            bg: WHITE,
            surface: neutral(0.975),
            surface_raised: WHITE,

            element_hover: BLACK.alpha(0.04),
            element_active: BLACK.alpha(0.08),

            border: BLACK.alpha(HAIRLINE_LIGHT),
            border_strong: BLACK.alpha(0.20),

            text: neutral(0.22),
            text_muted: neutral(0.45),
            text_faint: neutral(0.58),

            accent: Oklch::new(0.52, 0.17, 250.0).to_srgb(),
            on_accent: WHITE,
            selection: Oklch::new(0.52, 0.17, 250.0).to_srgb().alpha(0.20),
            cursor: Oklch::new(0.48, 0.18, 250.0).to_srgb(),

            danger: Oklch::new(0.52, 0.20, 25.0).to_srgb(),
            warning: Oklch::new(0.58, 0.14, 75.0).to_srgb(),
            success: Oklch::new(0.52, 0.15, 150.0).to_srgb(),
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

    fn check(name: &str, fg: Srgb, bg: Srgb, minimum: f32) {
        let ratio = contrast_ratio(fg, bg);
        assert!(
            ratio >= minimum,
            "{name}: contrast {ratio:.2} is below the {minimum:.1} floor"
        );
    }

    #[test]
    fn dark_text_contrast_clears_wcag() {
        let t = Theme::dark();
        check("dark text on bg", t.text, t.bg, AAA_TEXT);
        check("dark text on surface", t.text, t.surface, AAA_TEXT);
        check("dark muted on bg", t.text_muted, t.bg, AA_TEXT);
        check("dark faint on bg", t.text_faint, t.bg, AA_LARGE);
        check("dark accent on bg", t.accent, t.bg, AA_LARGE);
        check("dark danger on bg", t.danger, t.bg, AA_LARGE);
        check("dark on_accent over accent", t.on_accent, t.accent, AA_LARGE);
    }

    #[test]
    fn light_text_contrast_clears_wcag() {
        let t = Theme::light();
        check("light text on bg", t.text, t.bg, AAA_TEXT);
        check("light text on surface", t.text, t.surface, AAA_TEXT);
        check("light muted on bg", t.text_muted, t.bg, AA_TEXT);
        check("light faint on bg", t.text_faint, t.bg, AA_LARGE);
        check("light accent on bg", t.accent, t.bg, AA_LARGE);
        check("light danger on bg", t.danger, t.bg, AA_LARGE);
        check("light on_accent over accent", t.on_accent, t.accent, AA_LARGE);
    }

    #[test]
    fn appearances_are_comparable_not_mirrored() {
        // Both appearances should land in the same contrast neighbourhood, so
        // switching does not make one of them feel washed out.
        let (dark, light) = (Theme::dark(), Theme::light());
        let d = contrast_ratio(dark.text, dark.bg);
        let l = contrast_ratio(light.text, light.bg);
        assert!(
            (d - l).abs() < 6.0,
            "appearances drifted apart: dark {d:.2} vs light {l:.2}"
        );
    }

    #[test]
    fn elevation_runs_the_right_way_in_each_appearance() {
        let dark = Theme::dark();
        assert!(
            dark.surface.relative_luminance() > dark.bg.relative_luminance(),
            "dark chrome must sit lighter than the content plane"
        );

        let light = Theme::light();
        assert!(
            light.surface.relative_luminance() < light.bg.relative_luminance(),
            "light chrome must sit darker than the content plane"
        );
    }

    #[test]
    fn hairlines_are_visible_but_soft() {
        for t in [Theme::dark(), Theme::light()] {
            let border = t.border.flatten(t.bg);
            let ratio = contrast_ratio(border, t.bg);
            assert!(ratio > 1.10, "{:?} border is invisible: {ratio:.3}", t.appearance);
            assert!(ratio < 2.20, "{:?} border reads as a stroke: {ratio:.3}", t.appearance);
        }
    }
}
