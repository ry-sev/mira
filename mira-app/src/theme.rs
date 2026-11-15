//! The window's look: its colors, type scale, and fonts.
//!
//! Every raw color in the crate is defined here, and nowhere else. Panels are
//! neutral graphite and carry no color of their own; color is information.
//! Each site owns a stable hue derived from its registrable domain, so one ad
//! network reads as one color across every view.

use gpui_kit::Hsla;
use gpui_kit::Rems;
use gpui_kit::rems;
use gpui_kit::rgb;
use std::sync::LazyLock;

/// The palette, shared by every view.
#[derive(Debug, Clone, Copy)]
pub struct Colors {
    /// Behind everything.
    pub void: Hsla,

    /// Recessed fields: inputs, readouts, tracks.
    pub well: Hsla,

    /// The main content area.
    pub panel: Hsla,

    /// Bars and side panels.
    pub panel_2: Hsla,

    /// Raised controls.
    pub raised: Hsla,

    /// A row or control under the pointer.
    pub hover: Hsla,

    /// The selected row.
    pub select: Hsla,

    /// Dividers between regions.
    pub line: Hsla,

    /// Dividers within a region.
    pub line_soft: Hsla,

    /// Primary text.
    pub ink: Hsla,

    /// Secondary text.
    pub ink_2: Hsla,

    /// Labels and metadata.
    pub ink_3: Hsla,

    /// Text that recedes: counts, separators, placeholders.
    pub ink_4: Hsla,

    /// Keyboard focus.
    pub focus: Hsla,

    /// Success: 2xx statuses, a listening proxy.
    pub ok: Hsla,

    /// Caution.
    pub warn: Hsla,

    /// Failure: errors, 4xx and 5xx statuses.
    pub err: Hsla,

    /// Redirects.
    pub redirect: Hsla,

    /// The accent: the active view, latched toggles, the brand mark.
    pub live: Hsla,
}

static COLORS: LazyLock<Colors> = LazyLock::new(|| Colors {
    void: rgb(0x0b0c0d).into(),
    well: rgb(0x101113).into(),
    panel: rgb(0x151618).into(),
    panel_2: rgb(0x1a1b1e).into(),
    raised: rgb(0x212327).into(),
    hover: rgb(0x25272c).into(),
    select: rgb(0x2c3039).into(),
    line: rgb(0x2a2c31).into(),
    line_soft: rgb(0x1f2125).into(),
    ink: rgb(0xe8e6e1).into(),
    ink_2: rgb(0xa5a5a1).into(),
    ink_3: rgb(0x707172).into(),
    ink_4: rgb(0x4b4c4f).into(),
    focus: rgb(0x7fb7ff).into(),
    ok: rgb(0x67d98f).into(),
    warn: rgb(0xf2c14e).into(),
    err: rgb(0xff5d55).into(),
    redirect: rgb(0x46abef).into(),
    live: rgb(0xf25c54).into(),
});

/// The palette.
pub fn colors() -> &'static Colors {
    &COLORS
}

/// The hues sites are drawn in.
static SITE_PALETTE: LazyLock<[Hsla; 14]> = LazyLock::new(|| {
    [
        rgb(0xf25c54).into(), // coral
        rgb(0xf4a03a).into(), // amber
        rgb(0xecc94b).into(), // saffron
        rgb(0xb5d65a).into(), // lime
        rgb(0x55c98b).into(), // jade
        rgb(0x38c3bd).into(), // teal
        rgb(0x46abef).into(), // azure
        rgb(0x6f87f5).into(), // indigo
        rgb(0x9f7af5).into(), // violet
        rgb(0xd76cd1).into(), // orchid
        rgb(0xf27aa0).into(), // rose
        rgb(0xcf9a6a).into(), // clay
        rgb(0x8fb4a4).into(), // sage
        rgb(0xa3a8c7).into(), // haze
    ]
});

/// The color of the site `site`, the same in every view and every run.
pub fn site_color(site: &str) -> Hsla {
    if site.is_empty() {
        return colors().ink_3;
    }
    let palette = &*SITE_PALETTE;
    let index = usize::try_from(fnv1a(site)).unwrap_or_default() % palette.len();
    palette[index]
}

/// The phases of a request's timing, in the order they happen.
pub struct PhaseColors {
    pub blocked: Hsla,
    pub connect: Hsla,
    pub ssl: Hsla,
    pub send: Hsla,
    pub wait: Hsla,
    pub receive: Hsla,
}

static PHASES: LazyLock<PhaseColors> = LazyLock::new(|| PhaseColors {
    blocked: rgb(0x4b4c4f).into(),
    connect: rgb(0xf4a03a).into(),
    ssl: rgb(0xd76cd1).into(),
    send: rgb(0x6f87f5).into(),
    wait: rgb(0x55c98b).into(),
    receive: rgb(0x46abef).into(),
});

/// The colors of the timing phases.
pub fn phase_colors() -> &'static PhaseColors {
    &PHASES
}

/// 32-bit FNV-1a: stable across runs and platforms, unlike the standard
/// library's hasher, so a site keeps its color.
fn fnv1a(text: &str) -> u32 {
    text.bytes().fold(0x811c_9dc5, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

/// The type scale. Relative to the rem, so the whole window zooms together.
pub mod text {
    use super::Rems;
    use super::rems;

    /// Tags and tiny counts: 10 px at the default rem.
    pub const MICRO: Rems = rems(0.625);

    /// Labels and dense rows: 11 px.
    pub const LABEL: Rems = rems(0.6875);

    /// Controls and ordinary text: 13 px.
    pub const UI: Rems = rems(0.8125);

    /// Reading text: 14 px.
    pub const BODY: Rems = rems(0.875);
}

/// The proportional font.
pub const SANS: &str = ".SystemUIFont";

/// The monospaced font, for URLs, headers, bodies, and numbers.
#[cfg(target_os = "macos")]
pub const MONO: &str = "Menlo";

/// The monospaced font, for URLs, headers, bodies, and numbers.
#[cfg(target_os = "windows")]
pub const MONO: &str = "Consolas";

/// The monospaced font, for URLs, headers, bodies, and numbers.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const MONO: &str = "DejaVu Sans Mono";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_site_colors_are_stable_and_spread() {
        assert_eq!(site_color("example.com"), site_color("example.com"));
        let distinct: std::collections::HashSet<_> = ["a.com", "b.com", "c.com", "d.com", "e.com"]
            .iter()
            .map(|site| format!("{:?}", site_color(site)))
            .collect();
        assert!(distinct.len() > 1, "different sites get different hues");
    }

    #[test]
    fn test_fnv1a_matches_the_viewer() {
        // The viewer's `hash("")` and `hash("a")`.
        assert_eq!(fnv1a(""), 0x811c_9dc5);
        assert_eq!(fnv1a("a"), 0xe40c_292c);
    }
}
