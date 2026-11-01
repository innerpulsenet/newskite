//! Desktop color tokens → egui visuals.
//!
//! This is the **only** module of the GUI that turns color values into [`Color32`]s
//! (the brand's literal token values live beside it in [`super::brand_tokens`]). Every
//! other module receives a [`Palette`] (all derived from [`Tokens`] and the brand's ember
//! constants) and must not construct colors itself; a unit test enforces that.

use std::str::FromStr;

use eframe::egui::{
    self, style::Selection, style::WidgetVisuals, style::Widgets, Color32, CornerRadius,
    FontFamily, FontId, Shadow, Stroke, TextStyle, Visuals,
};
use newskite_theme::{Rgb, Theme, Tokens};

use super::brand_tokens::{brand_set_of, chart_set, CHART_SERIES_SLOT, EMBER};
use super::fonts;

/// Converts a token color to an opaque egui color.
pub fn to_color32(c: Rgb) -> Color32 {
    Color32::from_rgb(c.r, c.g, c.b)
}

/// Parses a user-supplied color (`#rrggbb`, `#rgb` or `r,g,b`), e.g. a bookmark color.
/// Returns `None` for anything unparsable so callers can fall back to a token color.
pub fn parse_user_color(s: &str) -> Option<Color32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Rgb::from_str(s).ok().map(to_color32)
}

/// Moves `fg` toward `toward` in 10% steps until it reaches `min_contrast` against
/// `bg` (or `toward` itself is reached). Keeps hues recognizable while readable.
fn readable_on(fg: Rgb, bg: Rgb, toward: Rgb, min_contrast: f32) -> Rgb {
    let mut c = fg;
    for step in 0..=10u8 {
        c = fg.mix(toward, f32::from(step) / 10.0);
        if c.contrast(bg) >= min_contrast {
            break;
        }
    }
    c
}

/// Picks whichever of `a`/`b` contrasts more with `bg`.
fn best_contrast(bg: Rgb, a: Rgb, b: Rgb) -> Rgb {
    if a.contrast(bg) >= b.contrast(bg) {
        a
    } else {
        b
    }
}

/// Every color the GUI paints with, derived from the desktop [`Tokens`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    /// Whether the scheme is dark.
    pub dark: bool,
    /// Window / panel background.
    pub surface: Color32,
    /// Cards, docks, raised controls.
    pub raised: Color32,
    /// Inputs, graph backgrounds, tracks.
    pub sunken: Color32,
    /// Primary text.
    pub text: Color32,
    /// Secondary text.
    pub muted: Color32,
    /// Tertiary text (captions, disabled icons); the brand's text-3 on its own themes.
    pub faint: Color32,
    /// Accent (selection, live series, primary actions).
    pub accent: Color32,
    /// Text drawn on top of `accent`.
    pub accent_text: Color32,
    /// Accent adjusted for readability as text/strokes on the surface.
    pub accent_fg: Color32,
    /// Accent tint used for soft backgrounds (badges, banners).
    pub accent_soft: Color32,
    /// Hairlines and frames.
    pub border: Color32,
    /// Stronger frame (hover, focus).
    pub border_strong: Color32,
    /// Hover background of flat buttons.
    pub hover: Color32,
    /// Success state.
    pub positive: Color32,
    /// Tint behind success badges.
    pub positive_soft: Color32,
    /// Warning state.
    pub warning: Color32,
    /// Tint behind warning badges and banners.
    pub warning_soft: Color32,
    /// Error state.
    pub negative: Color32,
    /// Tint behind error badges.
    pub negative_soft: Color32,
    /// Window shadow.
    pub shadow: Color32,
    /// Modal backdrop.
    pub backdrop: Color32,
    /// An empty cell of a segment track or the article grid: a quiet accent-tinted well.
    pub well: Color32,
    /// The innerpulse mark: ember on dark grounds, the deep ember on light ones.
    pub mark: Color32,
    /// Fill of the one filled call to action per view (the same in every theme).
    pub action: Color32,
    /// Its hover / pressed fill.
    pub action_hover: Color32,
    /// Text on the action fill.
    pub action_text: Color32,
    /// Ember as a small signal: focus rings.
    pub focus: Color32,
    /// Fully transparent (for "no fill").
    pub transparent: Color32,
    /// The kit's categorical chart slots 1–7 for this ground (series and user labels;
    /// never status).
    pub chart: [Color32; 7],
    /// The single live series of the throughput graph: the family accent at its chart
    /// step on the brand themes, the desktop accent otherwise.
    pub series: Color32,
}

impl Palette {
    /// Derives the palette from desktop tokens.
    pub fn from_tokens(t: &Tokens, dark: bool) -> Self {
        let soft = |c: Rgb, amount: f32| to_color32(t.surface_raised.mix(c, amount));
        let accent_fg = readable_on(t.accent, t.surface, t.text, 4.5);
        // Shadows: darker than any surface. Mixing the sunken surface toward the
        // darker of text/surface keeps them derived from tokens in both schemes.
        let shadow_base = if dark { t.surface_sunken } else { t.text };
        let shadow_alpha: u8 = if dark { 150 } else { 50 };
        let backdrop_alpha: u8 = if dark { 153 } else { 110 };
        Self {
            dark,
            surface: to_color32(t.surface),
            raised: to_color32(t.surface_raised),
            sunken: to_color32(t.surface_sunken),
            text: to_color32(t.text),
            muted: to_color32(t.text_muted),
            faint: to_color32(t.text_muted.mix(t.surface, 0.22)),
            accent: to_color32(t.accent),
            accent_text: to_color32(t.accent_text),
            accent_fg: to_color32(accent_fg),
            accent_soft: soft(t.accent, if dark { 0.28 } else { 0.18 }),
            border: to_color32(t.border),
            border_strong: to_color32(t.border.mix(t.text, 0.35)),
            hover: to_color32(t.surface_raised.mix(t.text, 0.09)),
            positive: to_color32(t.positive),
            positive_soft: soft(t.positive, if dark { 0.22 } else { 0.16 }),
            warning: to_color32(t.warning),
            warning_soft: soft(t.warning, if dark { 0.22 } else { 0.16 }),
            negative: to_color32(t.negative),
            negative_soft: soft(t.negative, if dark { 0.22 } else { 0.16 }),
            shadow: Color32::from_rgba_unmultiplied(
                shadow_base.r,
                shadow_base.g,
                shadow_base.b,
                shadow_alpha,
            ),
            backdrop: Color32::from_rgba_unmultiplied(
                shadow_base.r,
                shadow_base.g,
                shadow_base.b,
                backdrop_alpha,
            ),
            well: to_color32(t.surface.mix(t.accent, if dark { 0.2 } else { 0.16 })),
            mark: to_color32(if dark {
                EMBER.mark_on_dark
            } else {
                EMBER.mark_on_light
            }),
            action: to_color32(EMBER.action),
            action_hover: to_color32(EMBER.action_hover),
            action_text: to_color32(EMBER.action_text),
            focus: to_color32(if dark {
                EMBER.pulse_on_dark
            } else {
                EMBER.pulse_on_light
            }),
            transparent: Color32::TRANSPARENT,
            chart: chart_set(dark).map(to_color32),
            series: to_color32(t.accent),
        }
    }

    /// The palette of a resolved theme. The two brand themes take their exact tertiary
    /// text step from the kit; desktop themes derive it.
    pub fn from_theme(theme: &Theme) -> Self {
        let mut p = Self::from_tokens(&theme.tokens, theme.dark);
        if let Some(set) = brand_set_of(theme) {
            p.faint = to_color32(set.text_3);
            p.series = p.chart[CHART_SERIES_SLOT - 1];
        }
        p
    }

    /// `color` at `alpha` (0…1) of its opacity, for disabled and pressed states.
    pub fn fade(color: Color32, alpha: f32) -> Color32 {
        color.gamma_multiply(alpha.clamp(0.0, 1.0))
    }
}

/// Builds egui [`Visuals`] from desktop tokens: every surface, stroke and text color
/// egui uses is overridden, so nothing of egui's built-in palette leaks through.
pub fn visuals(t: &Tokens, dark: bool) -> Visuals {
    let p = Palette::from_tokens(t, dark);
    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    // The kit's one radius is 4 px; cards and dialogs take twice that.
    let radius = CornerRadius::same(4);

    v.dark_mode = dark;
    v.override_text_color = None;
    v.weak_text_color = Some(p.muted);
    v.hyperlink_color = p.accent_fg;
    v.faint_bg_color = to_color32(t.surface.mix(t.text, 0.035));
    v.extreme_bg_color = p.sunken;
    v.text_edit_bg_color = Some(p.sunken);
    v.code_bg_color = p.sunken;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.negative;

    v.panel_fill = p.surface;
    v.window_fill = p.raised;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    // Dialogs: surface 1 and a hairline, no shadow (the backdrop separates them).
    v.window_shadow = Shadow::NONE;
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: p.shadow,
    };
    // Tables opt in to striping explicitly; key/value grids stay clean.
    v.striped = false;

    let sel_bg = t
        .accent
        .mix(t.surface_sunken, if dark { 0.35 } else { 0.25 });
    let sel_fg = best_contrast(sel_bg, t.text, t.accent_text);
    v.selection = Selection {
        bg_fill: to_color32(sel_bg),
        stroke: Stroke::new(1.0, to_color32(sel_fg)),
    };
    v.text_cursor.stroke = Stroke::new(2.0, p.accent_fg);

    let button = t.surface_raised.mix(t.text, if dark { 0.06 } else { 0.03 });
    let widget =
        |bg: Color32, weak: Color32, stroke: Stroke, fg: Stroke, expansion: f32| WidgetVisuals {
            bg_fill: bg,
            weak_bg_fill: weak,
            bg_stroke: stroke,
            corner_radius: radius,
            fg_stroke: fg,
            expansion,
        };
    v.widgets = Widgets {
        noninteractive: widget(
            p.surface,
            p.surface,
            Stroke::new(1.0, p.border),
            Stroke::new(1.0, p.text),
            0.0,
        ),
        inactive: widget(
            p.sunken,
            to_color32(button),
            Stroke::new(1.0, p.border),
            Stroke::new(1.0, p.text),
            0.0,
        ),
        hovered: widget(
            to_color32(t.surface_sunken.mix(t.text, 0.08)),
            p.hover,
            Stroke::new(1.0, p.border_strong),
            Stroke::new(1.5, p.text),
            1.0,
        ),
        active: widget(
            to_color32(t.surface_sunken.mix(t.text, 0.14)),
            to_color32(t.surface_raised.mix(t.text, 0.16)),
            Stroke::new(1.0, p.accent),
            Stroke::new(2.0, p.text),
            1.0,
        ),
        open: widget(
            p.sunken,
            to_color32(t.surface_raised.mix(t.text, 0.08)),
            Stroke::new(1.0, p.border_strong),
            Stroke::new(1.0, p.text),
            0.0,
        ),
    };
    v
}

/// Applies a theme to the context (both egui theme slots, so OS light/dark flips in
/// egui never swap in a non-token palette) and returns the matching [`Palette`].
/// Fonts are installed separately ([`fonts::install`]).
pub fn apply(ctx: &egui::Context, theme: &Theme) -> Palette {
    let vis = visuals(&theme.tokens, theme.dark);
    for slot in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.set_visuals_of(slot, vis.clone());
        ctx.style_mut_of(slot, tune_style);
    }
    ctx.set_theme(if theme.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });
    Palette::from_theme(theme)
}

/// Spacing and type scale shared by both schemes: Instrument Sans 13.5 px body,
/// headings in its 600 weight, JetBrains Mono for monospace; nothing below 11 px.
fn tune_style(style: &mut egui::Style) {
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    style.spacing.interact_size.y = 24.0;
    style.spacing.window_margin = egui::Margin::same(16);
    style.text_styles = [
        (TextStyle::Heading, fonts::semibold_id(17.0)),
        (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(13.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(11.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(12.0, FontFamily::Monospace),
        ),
    ]
    .into();
}

#[cfg(test)]
mod tests {
    use super::*;
    use newskite_theme::fallback;

    #[test]
    fn visuals_come_from_tokens() {
        for dark in [true, false] {
            let theme = fallback(dark);
            let t = &theme.tokens;
            let v = visuals(t, dark);
            assert_eq!(v.dark_mode, dark);
            assert_eq!(v.panel_fill, to_color32(t.surface));
            assert_eq!(v.window_fill, to_color32(t.surface_raised));
            assert_eq!(v.extreme_bg_color, to_color32(t.surface_sunken));
            assert_eq!(v.window_stroke.color, to_color32(t.border));
            assert_eq!(v.widgets.noninteractive.fg_stroke.color, to_color32(t.text));
            assert_eq!(v.widgets.inactive.fg_stroke.color, to_color32(t.text));
            assert_eq!(
                v.widgets.noninteractive.bg_stroke.color,
                to_color32(t.border)
            );
            assert_eq!(v.weak_text_color, Some(to_color32(t.text_muted)));
            assert_eq!(v.warn_fg_color, to_color32(t.warning));
            assert_eq!(v.error_fg_color, to_color32(t.negative));
            assert_eq!(v.widgets.active.bg_stroke.color, to_color32(t.accent));
        }
    }

    #[test]
    fn switching_scheme_changes_every_surface_and_text_color() {
        let d = visuals(&fallback(true).tokens, true);
        let l = visuals(&fallback(false).tokens, false);
        assert_ne!(d.panel_fill, l.panel_fill);
        assert_ne!(d.window_fill, l.window_fill);
        assert_ne!(d.extreme_bg_color, l.extreme_bg_color);
        assert_ne!(d.faint_bg_color, l.faint_bg_color);
        assert_ne!(
            d.widgets.noninteractive.fg_stroke.color,
            l.widgets.noninteractive.fg_stroke.color
        );
        assert_ne!(d.weak_text_color, l.weak_text_color);
        assert_ne!(
            d.widgets.inactive.weak_bg_fill,
            l.widgets.inactive.weak_bg_fill
        );
        assert_ne!(
            d.widgets.hovered.weak_bg_fill,
            l.widgets.hovered.weak_bg_fill
        );
        assert_ne!(d.selection.bg_fill, l.selection.bg_fill);
    }

    #[test]
    fn palette_text_colors_are_readable() {
        for dark in [true, false] {
            let theme = fallback(dark);
            let t = &theme.tokens;
            // The readable accent reaches 4.5:1 unless the tokens make that impossible.
            let fg = readable_on(t.accent, t.surface, t.text, 4.5);
            assert!(
                fg.contrast(t.surface) >= 4.5,
                "dark={dark}: {}",
                fg.contrast(t.surface)
            );
            let v = visuals(t, dark);
            let sel = v.selection.bg_fill;
            let sel_rgb = Rgb::new(sel.r(), sel.g(), sel.b());
            let fg = v.selection.stroke.color;
            let fg_rgb = Rgb::new(fg.r(), fg.g(), fg.b());
            assert!(fg_rgb.contrast(sel_rgb) >= 3.0, "dark={dark}");
        }
    }

    #[test]
    fn apply_sets_both_slots_and_preference() {
        let ctx = egui::Context::default();
        let theme = fallback(true);
        let p = apply(&ctx, &theme);
        assert!(p.dark);
        assert_eq!(
            ctx.style_of(egui::Theme::Light).visuals.panel_fill,
            p.surface
        );
        assert_eq!(
            ctx.style_of(egui::Theme::Dark).visuals.panel_fill,
            p.surface
        );
        let light = fallback(false);
        let p2 = apply(&ctx, &light);
        assert_ne!(p.surface, p2.surface);
        assert_eq!(
            ctx.style_of(egui::Theme::Dark).visuals.panel_fill,
            p2.surface
        );
    }

    #[test]
    fn user_colors_parse_or_fall_back() {
        assert_eq!(
            parse_user_color("#ff0000"),
            Some(to_color32(Rgb::new(255, 0, 0)))
        );
        assert_eq!(
            parse_user_color(" 0,128,255 "),
            Some(to_color32(Rgb::new(0, 128, 255)))
        );
        assert_eq!(parse_user_color(""), None);
        assert_eq!(parse_user_color("not a color"), None);
    }

    /// F10 / acceptance: no literal colors in UI code outside this module and the brand
    /// token file.
    #[test]
    fn no_literal_colors_outside_theme_module() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/gui");
        let banned = [
            "Color32::from_rgb",
            "Color32::from_rgba",
            "Color32::from_gray",
            "Color32::from_hex",
            "Color32::from_black_alpha",
            "Color32::from_white_alpha",
            "Color32::TRANSPARENT",
            "Color32::RED",
            "Color32::GREEN",
            "Color32::BLUE",
            "Color32::WHITE",
            "Color32::BLACK",
            "Color32::YELLOW",
            "Color32::GOLD",
            "Color32::KHAKI",
            "Color32::BROWN",
            "Color32::GRAY",
            "Color32::LIGHT_",
            "Color32::DARK_",
            "Color32::PLACEHOLDER",
            "Color32::ORANGE",
            "Color32::MAGENTA",
            "Rgb::new(",
            "Rgba::from",
            "Hsva::new",
        ];
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("src/gui exists") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // The two places allowed to hold or convert literal colors.
            if matches!(
                path.file_name().and_then(|n| n.to_str()),
                Some("theme.rs" | "brand_tokens.rs")
            ) {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("readable source");
            for pat in banned {
                assert!(
                    !src.contains(pat),
                    "{} contains literal color `{pat}`",
                    path.display()
                );
            }
            checked += 1;
        }
        assert!(checked >= 8, "expected the gui modules, found {checked}");
        // main.rs and lib.rs must not paint either.
        for f in ["src/main.rs", "src/lib.rs", "src/launch.rs"] {
            let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(f);
            if let Ok(src) = std::fs::read_to_string(&p) {
                for pat in banned {
                    assert!(!src.contains(pat), "{f} contains literal color `{pat}`");
                }
            }
        }
    }
}
