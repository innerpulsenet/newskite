//! The innerpulse brand token sets: the one place (besides [`super::theme`]) where the
//! GUI holds literal color values.
//!
//! Values are copied from the innerpulse brand kit (`innerpulse-tokens.css`, and
//! `innerpulse-warm.css` for the optional warm ink): the ink and paper grounds with their
//! three surfaces and three text steps, the desktop-family violet accent (phosphor on ink,
//! the deep step on paper), the status colors and the ember signals (the mark, the action
//! fill and focus). Ink is carbon since the kit of 2026-11-05: the neutral cool-grey ground
//! with steel text. The two ink sets also carry a [`MetalSet`]: the plate the window is cut
//! from and the smoked glass of its keys, the materials the studio's desktop apps share.
//! [`ink`], [`warm`] and [`paper`] build ordinary [`newskite_theme::Theme`]s, so the rest of
//! the GUI treats them like a desktop theme.

use newskite_theme::{Rgb, Theme, ThemeSource, Tokens};

/// One brand ground: everything a [`Theme`] needs plus the steps `Tokens` has no slot for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrandSet {
    /// Whether this is the dark (ink) set.
    pub dark: bool,
    /// The ground behind everything.
    pub ground: Rgb,
    /// Surface 1 (cards, toolbar, status bar).
    pub surface_1: Rgb,
    /// Surface 2 (inputs, wells).
    pub surface_2: Rgb,
    /// Surface 3 (pressed rows, skeletons).
    pub surface_3: Rgb,
    /// Primary text.
    pub text: Rgb,
    /// Secondary text.
    pub text_2: Rgb,
    /// Tertiary text (the 12 px floor on every surface).
    pub text_3: Rgb,
    /// The hairline (`--innerpulse-line-2`) as an opaque color on the ground.
    pub line: Rgb,
    /// The family accent (desktop and native: violet).
    pub accent: Rgb,
    /// Text drawn on the accent.
    pub accent_text: Rgb,
    /// Status: ok.
    pub ok: Rgb,
    /// Status: warning.
    pub warn: Rgb,
    /// Status: error.
    pub err: Rgb,
    /// The window's materials, on the grounds that have them (the two inks).
    pub metal: Option<MetalSet>,
}

/// The materials of an ink ground: one plate the whole window is cut from, with dark glass
/// let into it. Carbon's plate is brushed and its wordmark is steel; the warm plate is
/// plain, since gunmetal tones would fight the warm ground.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetalSet {
    /// The plate.
    pub plate: Rgb,
    /// A hairline that reads on the plate and on the glass (the steel text at 16% over
    /// the plate).
    pub line: Rgb,
    /// A key's smoked glass: at rest (top, foot), then held down (top, foot).
    pub key: [Rgb; 4],
    /// The steel ramp of the wordmark, top to foot: bright above the crease, dark through
    /// it, a lighter band below. `None` paints the wordmark in the text color.
    pub steel: Option<[Rgb; 5]>,
    /// Whether the plate carries a brushed grain.
    pub brushed: bool,
}

/// The ink set: carbon (`#0b0d10` ground, steel text, the violet phosphor).
pub const INK: BrandSet = BrandSet {
    dark: true,
    ground: Rgb::new(0x0b, 0x0d, 0x10),
    surface_1: Rgb::new(0x11, 0x13, 0x17),
    surface_2: Rgb::new(0x17, 0x1a, 0x1f),
    surface_3: Rgb::new(0x1f, 0x23, 0x29),
    text: Rgb::new(0xe3, 0xe6, 0xe9),
    text_2: Rgb::new(0xb9, 0xbd, 0xc3),
    text_3: Rgb::new(0x8f, 0x94, 0x9b),
    // rgba(227, 230, 233, .16) over the ground.
    line: Rgb::new(0x2e, 0x30, 0x33),
    accent: Rgb::new(0xb3, 0x8c, 0xff),
    accent_text: Rgb::new(0x0b, 0x0d, 0x10),
    ok: Rgb::new(0x7d, 0xff, 0xb0),
    warn: Rgb::new(0xff, 0xb3, 0x47),
    err: Rgb::new(0xff, 0x9a, 0x8f),
    metal: Some(MetalSet {
        plate: Rgb::new(0x26, 0x2a, 0x30),
        line: Rgb::new(0x44, 0x48, 0x4e),
        key: [
            Rgb::new(0x1b, 0x1e, 0x23),
            Rgb::new(0x0e, 0x10, 0x13),
            Rgb::new(0x0c, 0x0d, 0x10),
            Rgb::new(0x14, 0x16, 0x1a),
        ],
        steel: Some([
            Rgb::new(0xf4, 0xf6, 0xf8),
            Rgb::new(0xcf, 0xd3, 0xd8),
            Rgb::new(0x8b, 0x90, 0x97),
            Rgb::new(0xd5, 0xd9, 0xde),
            Rgb::new(0x9d, 0xa2, 0xa9),
        ]),
        brushed: true,
    }),
};

/// The optional warm ink set (`#0a0908` ground, warm greys), the brown-black ground of the
/// 2026-10-25 kit. Never mixed with carbon on one screen.
pub const WARM: BrandSet = BrandSet {
    dark: true,
    ground: Rgb::new(0x0a, 0x09, 0x08),
    surface_1: Rgb::new(0x10, 0x0e, 0x0c),
    surface_2: Rgb::new(0x17, 0x14, 0x0f),
    surface_3: Rgb::new(0x1f, 0x1b, 0x17),
    text: Rgb::new(0xec, 0xe7, 0xdf),
    text_2: Rgb::new(0xb0, 0xa9, 0x9f),
    text_3: Rgb::new(0x8e, 0x87, 0x7d),
    // rgba(244, 241, 236, .16) over the ground.
    line: Rgb::new(0x2f, 0x2e, 0x2c),
    accent: Rgb::new(0xb3, 0x8c, 0xff),
    accent_text: Rgb::new(0x0a, 0x09, 0x08),
    ok: Rgb::new(0x7d, 0xff, 0xb0),
    warn: Rgb::new(0xff, 0xb3, 0x47),
    err: Rgb::new(0xff, 0x9a, 0x8f),
    metal: Some(MetalSet {
        plate: Rgb::new(0x21, 0x1b, 0x14),
        line: Rgb::new(0x42, 0x3c, 0x34),
        key: [
            Rgb::new(0x22, 0x1c, 0x15),
            Rgb::new(0x13, 0x10, 0x0b),
            Rgb::new(0x11, 0x0d, 0x09),
            Rgb::new(0x1a, 0x15, 0x0f),
        ],
        steel: None,
        brushed: false,
    }),
};

/// The paper set (`#f4f1ec` ground, the deep violet step).
pub const PAPER: BrandSet = BrandSet {
    dark: false,
    ground: Rgb::new(0xf4, 0xf1, 0xec),
    surface_1: Rgb::new(0xef, 0xeb, 0xe4),
    surface_2: Rgb::new(0xe9, 0xe4, 0xdb),
    surface_3: Rgb::new(0xe0, 0xda, 0xd0),
    text: Rgb::new(0x14, 0x17, 0x1c),
    text_2: Rgb::new(0x45, 0x4b, 0x56),
    text_3: Rgb::new(0x57, 0x5e, 0x6b),
    // rgba(20, 23, 28, .2) over the ground.
    line: Rgb::new(0xc7, 0xc5, 0xc2),
    accent: Rgb::new(0x6a, 0x3f, 0xd0),
    accent_text: Rgb::new(0xff, 0xff, 0xff),
    ok: Rgb::new(0x0d, 0x6d, 0x3d),
    warn: Rgb::new(0x8b, 0x51, 0x09),
    err: Rgb::new(0xa3, 0x22, 0x1a),
    metal: None,
};

/// Ember signals. They are the same for every theme the window can wear (the desktop's
/// included): the mark, the one filled call to action, and focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ember {
    /// The mark on dark grounds (`#f43a2a`).
    pub mark_on_dark: Rgb,
    /// The mark on light grounds (`#cf2a1c`).
    pub mark_on_light: Rgb,
    /// The action fill of the one filled button (`#d12c1e`, 5.1:1 with white).
    pub action: Rgb,
    /// The action fill on hover / press (`#bd2417`).
    pub action_hover: Rgb,
    /// Text on the action fill.
    pub action_text: Rgb,
    /// Ember as a small signal (focus ring) on dark grounds.
    pub pulse_on_dark: Rgb,
    /// Ember as a small signal on light grounds (`#b42418`, 5.8:1 on paper).
    pub pulse_on_light: Rgb,
}

/// The ember constants.
pub const EMBER: Ember = Ember {
    mark_on_dark: Rgb::new(0xf4, 0x3a, 0x2a),
    mark_on_light: Rgb::new(0xcf, 0x2a, 0x1c),
    action: Rgb::new(0xd1, 0x2c, 0x1e),
    action_hover: Rgb::new(0xbd, 0x24, 0x17),
    action_text: Rgb::new(0xff, 0xff, 0xff),
    pulse_on_dark: Rgb::new(0xf4, 0x3a, 0x2a),
    pulse_on_light: Rgb::new(0xb4, 0x24, 0x18),
};

/// The kit's categorical chart slots (`--innerpulse-chart-1` … `-7`) for light grounds:
/// cyan, coral, violet, amber, blue, mint, magenta. Identity colors for series and user
/// labels; never status.
pub const CHART_LIGHT: [Rgb; 7] = [
    Rgb::new(0x00, 0x75, 0x97),
    Rgb::new(0xc0, 0x42, 0x1c),
    Rgb::new(0x6a, 0x3f, 0xd0),
    Rgb::new(0x93, 0x56, 0x0a),
    Rgb::new(0x28, 0x5f, 0xcd),
    Rgb::new(0x0e, 0x78, 0x43),
    Rgb::new(0xa6, 0x2f, 0x85),
];

/// The categorical chart slots for dark grounds (the phosphor hues at OKLCH L ≈ .66).
pub const CHART_DARK: [Rgb; 7] = [
    Rgb::new(0x00, 0xa3, 0xb8),
    Rgb::new(0xe1, 0x66, 0x4a),
    Rgb::new(0x86, 0x5c, 0xe7),
    Rgb::new(0xc9, 0x7d, 0x00),
    Rgb::new(0x62, 0x8d, 0xea),
    Rgb::new(0x00, 0xad, 0x5f),
    Rgb::new(0xd8, 0x5c, 0xad),
];

/// The chart slots for a light or dark ground.
pub fn chart_set(dark: bool) -> [Rgb; 7] {
    if dark {
        CHART_DARK
    } else {
        CHART_LIGHT
    }
}

/// The product's single-series chart color: the family accent (violet) at its chart
/// step, slot 3 (charts.md §2: the phosphors are for text and dots, not series).
pub const CHART_SERIES_SLOT: usize = 3;

/// Scheme names the brand themes carry (shown in diagnostics).
pub const INK_NAME: &str = "innerpulse carbon";
/// See [`INK_NAME`].
pub const WARM_NAME: &str = "innerpulse warm";
/// See [`INK_NAME`].
pub const PAPER_NAME: &str = "innerpulse paper";

impl BrandSet {
    /// The set as desktop-style tokens. On paper the ground is the window surface,
    /// surface 1 the raised surface (cards) and surface 2 the sunken one (inputs, wells).
    /// On a ground with materials the window surface is the plate, and what was raised is
    /// glass let into it: cards are surface 1 and wells the ground itself.
    pub fn tokens(&self) -> Tokens {
        let (surface, raised, sunken, line) = match &self.metal {
            Some(m) => (m.plate, self.surface_1, self.ground, m.line),
            None => (self.ground, self.surface_1, self.surface_2, self.line),
        };
        Tokens {
            surface,
            surface_raised: raised,
            surface_sunken: sunken,
            text: self.text,
            text_muted: self.text_2,
            accent: self.accent,
            accent_text: self.accent_text,
            border: line,
            positive: self.ok,
            warning: self.warn,
            negative: self.err,
        }
    }

    /// The set as a [`Theme`] named [`INK_NAME`], [`WARM_NAME`] or [`PAPER_NAME`].
    pub fn theme(&self) -> Theme {
        let name = if !self.dark {
            PAPER_NAME
        } else if self.ground == WARM.ground {
            WARM_NAME
        } else {
            INK_NAME
        };
        Theme {
            tokens: self.tokens(),
            dark: self.dark,
            source: ThemeSource::Fallback,
            scheme_name: Some(name.to_owned()),
        }
    }
}

/// The innerpulse ink theme (carbon).
pub fn ink() -> Theme {
    INK.theme()
}

/// The optional warm ink theme.
pub fn warm() -> Theme {
    WARM.theme()
}

/// The innerpulse paper theme.
pub fn paper() -> Theme {
    PAPER.theme()
}

/// The brand set a theme was built from, if it is one of the brand themes.
pub fn brand_set_of(theme: &Theme) -> Option<&'static BrandSet> {
    [&INK, &WARM, &PAPER]
        .into_iter()
        .find(|s| s.dark == theme.dark && s.tokens() == theme.tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hairlines_match_the_kit_alpha() {
        // line-2 is a translucent text color; the opaque value is that alpha over the ground.
        let ink_line = INK.ground.mix(INK.text, 0.16);
        let warm_line = WARM.ground.mix(Rgb::new(244, 241, 236), 0.16);
        let paper_line = PAPER.ground.mix(Rgb::new(20, 23, 28), 0.2);
        let plates = [&INK, &WARM].map(|s| {
            let m = s.metal.expect("the inks have materials");
            (m.line, m.plate.mix(s.text, 0.16))
        });
        for (got, want) in [(INK.line, ink_line), (WARM.line, warm_line), (PAPER.line, paper_line)].into_iter().chain(plates) {
            for (a, b) in [(got.r, want.r), (got.g, want.g), (got.b, want.b)] {
                assert!(a.abs_diff(b) <= 1, "{got:?} vs {want:?}");
            }
        }
    }

    #[test]
    fn brand_text_is_readable_on_every_surface() {
        for set in [INK, WARM, PAPER] {
            let plate = set.metal.map(|m| m.plate);
            for surface in [set.ground, set.surface_1, set.surface_2, set.surface_3].into_iter().chain(plate) {
                for text in [set.text, set.text_2, set.text_3, set.accent] {
                    assert!(
                        text.contrast(surface) >= 4.5,
                        "{text:?} on {surface:?}: {}",
                        text.contrast(surface)
                    );
                }
                for status in [set.ok, set.warn, set.err] {
                    assert!(status.contrast(surface) >= 4.5, "{status:?} on {surface:?}");
                }
            }
            assert!(set.accent_text.contrast(set.accent) >= 4.5);
        }
        assert!(EMBER.action_text.contrast(EMBER.action) >= 5.0);
        assert!(EMBER.action_text.contrast(EMBER.action_hover) >= 5.0);
    }

    #[test]
    fn chart_series_is_the_violet_chart_step() {
        assert_eq!(
            CHART_DARK[CHART_SERIES_SLOT - 1],
            Rgb::new(0x86, 0x5c, 0xe7)
        );
        // On paper the chart step is the accent itself.
        assert_eq!(CHART_LIGHT[CHART_SERIES_SLOT - 1], PAPER.accent);
        assert_ne!(CHART_DARK[CHART_SERIES_SLOT - 1], INK.accent);
    }

    #[test]
    fn brand_themes_are_recognized() {
        assert_eq!(brand_set_of(&ink()), Some(&INK));
        assert_eq!(brand_set_of(&warm()), Some(&WARM));
        assert_eq!(brand_set_of(&paper()), Some(&PAPER));
        assert_ne!(ink().scheme_name, warm().scheme_name);
        assert_eq!(brand_set_of(&newskite_theme::fallback(true)), None);
        assert!(ink().dark && warm().dark && !paper().dark);
    }
}
