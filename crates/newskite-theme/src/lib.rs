//! Desktop color tokens for Newskite.
//!
//! The UI never hard-codes a color: it asks this crate for a [`Theme`], whose [`Tokens`]
//! are resolved from the desktop in this order:
//!
//! 1. KDE `kdeglobals` (`[Colors:Window]`, `[Colors:Selection]`, `[Colors:View]`,
//!    `[General] AccentColor`/`ColorScheme`), see [`from_kdeglobals_str`];
//! 2. on Linux with the default `portal` feature, the XDG desktop portal
//!    (`org.freedesktop.appearance` `color-scheme` and `accent-color`);
//! 3. a built-in light or dark palette, see [`fallback`].
//!
//! Literal color values exist **only** in this crate (the fallback palette and the
//! fallback status hues). [`ThemeWatcher`] re-resolves when the desktop scheme changes.

mod color;
mod ini;
mod portal;
mod watcher;

use std::path::{Path, PathBuf};

pub use color::{ParseColorError, Rgb};
pub use watcher::ThemeWatcher;

use color::{BLACK, WHITE};
use ini::KConfig;

/// Errors from reading theme sources from disk.
#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    /// The file exists but could not be read.
    #[error("failed to read {path}: {source}")]
    Io {
        /// The file that failed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// The semantic color tokens every UI surface draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tokens {
    /// Window/panel background.
    pub surface: Rgb,
    /// Slightly elevated surfaces: cards, popups, hovered rows.
    pub surface_raised: Rgb,
    /// Recessed surfaces: text fields, table bodies, wells.
    pub surface_sunken: Rgb,
    /// Primary text.
    pub text: Rgb,
    /// Secondary text (captions, hints, inactive labels).
    pub text_muted: Rgb,
    /// Accent / selection color.
    pub accent: Rgb,
    /// Text drawn on top of `accent` (contrast >= 4.5 whenever possible).
    pub accent_text: Rgb,
    /// Separators and outlines.
    pub border: Rgb,
    /// Success state (contrast >= 3 against `surface`).
    pub positive: Rgb,
    /// Warning state (contrast >= 3 against `surface`).
    pub warning: Rgb,
    /// Error state (contrast >= 3 against `surface`).
    pub negative: Rgb,
}

impl Tokens {
    /// Iterates `(token_name, color)` pairs in declaration order, for diagnostics.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, Rgb)> {
        [
            ("surface", self.surface),
            ("surface_raised", self.surface_raised),
            ("surface_sunken", self.surface_sunken),
            ("text", self.text),
            ("text_muted", self.text_muted),
            ("accent", self.accent),
            ("accent_text", self.accent_text),
            ("border", self.border),
            ("positive", self.positive),
            ("warning", self.warning),
            ("negative", self.negative),
        ]
        .into_iter()
    }
}

/// Where a [`Theme`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeSource {
    /// KDE `kdeglobals`.
    KdeGlobals,
    /// XDG desktop portal (`org.freedesktop.appearance`).
    Portal,
    /// The built-in palette.
    Fallback,
}

/// A resolved desktop theme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// The color tokens.
    pub tokens: Tokens,
    /// Whether this is a dark scheme (light text on a dark background).
    pub dark: bool,
    /// Which source produced it.
    pub source: ThemeSource,
    /// The desktop's scheme name when known (`[General] ColorScheme`).
    pub scheme_name: Option<String>,
}

/// The built-in palettes. The only literal palette colors in the code base.
mod palette {
    use crate::Rgb;

    pub(crate) const DARK_BG: Rgb = Rgb::new(0x20, 0x23, 0x26);
    pub(crate) const DARK_FG: Rgb = Rgb::new(0xfc, 0xfc, 0xfc);
    pub(crate) const LIGHT_BG: Rgb = Rgb::new(0xef, 0xf0, 0xf1);
    pub(crate) const LIGHT_FG: Rgb = Rgb::new(0x23, 0x26, 0x29);
    pub(crate) const ACCENT: Rgb = Rgb::new(0x3d, 0xae, 0xe9);

    /// Status hues tuned for dark surfaces.
    pub(crate) const DARK_POSITIVE: Rgb = Rgb::new(0x3c, 0xc8, 0x7a);
    pub(crate) const DARK_WARNING: Rgb = Rgb::new(0xf6, 0x9a, 0x3c);
    pub(crate) const DARK_NEGATIVE: Rgb = Rgb::new(0xf0, 0x60, 0x6c);
    /// Status hues tuned for light surfaces.
    pub(crate) const LIGHT_POSITIVE: Rgb = Rgb::new(0x1d, 0x7a, 0x43);
    pub(crate) const LIGHT_WARNING: Rgb = Rgb::new(0xa8, 0x55, 0x00);
    pub(crate) const LIGHT_NEGATIVE: Rgb = Rgb::new(0xc0, 0x2b, 0x3a);
}

/// Minimum WCAG contrast for `accent_text` on `accent`.
const ACCENT_TEXT_CONTRAST: f32 = 4.5;
/// Minimum WCAG contrast for status colors on `surface` (non-text UI / large text).
const STATUS_CONTRAST: f32 = 3.0;

/// Decides light vs. dark: a scheme is dark when its background is darker than its
/// foreground. If the two are (nearly) equally bright, falls back to background
/// luminance < 0.5.
fn is_dark(bg: Rgb, fg: Rgb) -> bool {
    let (lb, lf) = (bg.relative_luminance(), fg.relative_luminance());
    if (lb - lf).abs() > 0.05 {
        lb < lf
    } else {
        lb < 0.5
    }
}

/// Picks the text color for `accent`: the better of `fg`/`bg` if it reaches 4.5:1,
/// otherwise black or white if that does, otherwise whichever contrasts most.
fn pick_accent_text(accent: Rgb, bg: Rgb, fg: Rgb) -> Rgb {
    let best = |a: Rgb, b: Rgb| {
        if accent.contrast(a) >= accent.contrast(b) {
            a
        } else {
            b
        }
    };
    let scheme = best(fg, bg);
    if accent.contrast(scheme) >= ACCENT_TEXT_CONTRAST {
        return scheme;
    }
    let bw = best(BLACK, WHITE);
    if accent.contrast(bw) >= ACCENT_TEXT_CONTRAST {
        return bw;
    }
    best(scheme, bw)
}

/// Derives the full token set from a background, foreground and accent color, plus
/// optional desktop-provided status colors.
///
/// * `surface = bg`, `text = fg`, `accent` as given.
/// * Dark schemes: `surface_raised` is `bg` 6% toward `fg`, `surface_sunken` is `bg` 25%
///   toward black. Light schemes: `surface_raised` is `bg` 60% toward white,
///   `surface_sunken` is `bg` 5% toward `fg`.
/// * `text_muted` is `fg` 40% toward `bg`; `border` is `fg` 80% toward `bg`.
/// * `accent_text` reaches 4.5:1 against `accent` whenever any candidate can.
/// * Status colors use the given values, else built-in hues for the scheme's
///   brightness, and are nudged until they reach 3:1 against `surface`.
pub fn derive(
    bg: Rgb,
    fg: Rgb,
    accent: Rgb,
    positive: Option<Rgb>,
    warning: Option<Rgb>,
    negative: Option<Rgb>,
) -> Tokens {
    let dark = is_dark(bg, fg);
    let (surface_raised, surface_sunken) = if dark {
        (bg.mix(fg, 0.06), bg.mix(BLACK, 0.25))
    } else {
        (bg.mix(WHITE, 0.6), bg.mix(fg, 0.05))
    };
    let (dp, dw, dn) = if dark {
        (
            palette::DARK_POSITIVE,
            palette::DARK_WARNING,
            palette::DARK_NEGATIVE,
        )
    } else {
        (
            palette::LIGHT_POSITIVE,
            palette::LIGHT_WARNING,
            palette::LIGHT_NEGATIVE,
        )
    };
    let status =
        |given: Option<Rgb>, hue: Rgb| given.unwrap_or(hue).ensure_contrast(bg, STATUS_CONTRAST);
    Tokens {
        surface: bg,
        surface_raised,
        surface_sunken,
        text: fg,
        text_muted: fg.mix(bg, 0.4),
        accent,
        accent_text: pick_accent_text(accent, bg, fg),
        border: fg.mix(bg, 0.8),
        positive: status(positive, dp),
        warning: status(warning, dw),
        negative: status(negative, dn),
    }
}

/// The built-in light or dark theme (source [`ThemeSource::Fallback`]).
pub fn fallback(dark: bool) -> Theme {
    let (bg, fg) = if dark {
        (palette::DARK_BG, palette::DARK_FG)
    } else {
        (palette::LIGHT_BG, palette::LIGHT_FG)
    };
    Theme {
        tokens: derive(bg, fg, palette::ACCENT, None, None, None),
        dark,
        source: ThemeSource::Fallback,
        scheme_name: None,
    }
}

/// Parses a color in either KDE `r,g,b` or hex syntax.
fn parse_any(s: &str) -> Option<Rgb> {
    Rgb::parse_kde(s).or_else(|| Rgb::parse_hex(s))
}

/// Builds a theme from the text of a `kdeglobals` file.
///
/// Requires `[Colors:Window] BackgroundNormal` and `ForegroundNormal`; returns `None`
/// if either is missing or unparseable. The accent is `[General] AccentColor` when set,
/// else `[Colors:Selection] BackgroundNormal`, else the built-in accent. Status colors
/// come from `[Colors:View]` (then `[Colors:Window]`) `ForegroundPositive/Neutral/Negative`.
pub fn from_kdeglobals_str(ini: &str) -> Option<Theme> {
    let cfg = KConfig::parse(ini);
    let color = |group: &str, key: &str| cfg.get(group, key).and_then(parse_any);
    let bg = color("Colors:Window", "BackgroundNormal")?;
    let fg = color("Colors:Window", "ForegroundNormal")?;
    let accent = color("General", "AccentColor")
        .or_else(|| color("Colors:Selection", "BackgroundNormal"))
        .unwrap_or(palette::ACCENT);
    let status = |key: &str| color("Colors:View", key).or_else(|| color("Colors:Window", key));
    let tokens = derive(
        bg,
        fg,
        accent,
        status("ForegroundPositive"),
        status("ForegroundNeutral"),
        status("ForegroundNegative"),
    );
    let scheme_name = cfg
        .get("General", "ColorScheme")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Some(Theme {
        tokens,
        dark: is_dark(bg, fg),
        source: ThemeSource::KdeGlobals,
        scheme_name,
    })
}

/// Reads and parses a `kdeglobals` file. `Ok(None)` if the file does not exist or has
/// no usable window colors.
pub fn from_kdeglobals_path(path: &Path) -> Result<Option<Theme>, ThemeError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(from_kdeglobals_str(&String::from_utf8_lossy(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ThemeError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Location of the user's `kdeglobals`: `$XDG_CONFIG_HOME/kdeglobals` when that is set
/// to an absolute path, else `~/.config/kdeglobals`. `None` if no home directory is known.
pub fn kdeglobals_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let p = PathBuf::from(xdg);
        if p.is_absolute() {
            return Some(p.join("kdeglobals"));
        }
    }
    let base = directories::BaseDirs::new()?;
    Some(base.home_dir().join(".config").join("kdeglobals"))
}

/// Resolves the desktop theme with no outside hint. See [`resolve_with_hint`].
pub fn resolve() -> Theme {
    resolve_with_hint(None)
}

/// Resolves the desktop theme: kdeglobals → portal (Linux) → fallback.
///
/// `prefers_dark` is the caller's own guess at the system preference (e.g. the GUI
/// toolkit's detected theme). It is used when the portal reports no light/dark
/// preference and for the fallback palette; `None` means light.
pub fn resolve_with_hint(prefers_dark: Option<bool>) -> Theme {
    resolve_from(&Sources {
        kdeglobals: kdeglobals_path(),
        portal: true,
        prefers_dark,
    })
}

/// Inputs for one resolution pass (separated so the watcher and tests can redirect them).
#[derive(Debug, Clone)]
pub(crate) struct Sources {
    pub kdeglobals: Option<PathBuf>,
    pub portal: bool,
    pub prefers_dark: Option<bool>,
}

pub(crate) fn resolve_from(src: &Sources) -> Theme {
    resolve_detailed(src).0
}

/// [`resolve_from`], also saying whether the portal was asked and gave no answer (absent,
/// slow or empty), so a watcher can keep an earlier portal theme through a late reply.
pub(crate) fn resolve_detailed(src: &Sources) -> (Theme, bool) {
    if let Some(path) = &src.kdeglobals {
        match from_kdeglobals_path(path) {
            Ok(Some(theme)) => return (theme, false),
            Ok(None) => {}
            Err(e) => tracing::warn!("ignoring kdeglobals: {e}"),
        }
    }
    let hint = src.prefers_dark.unwrap_or(false);
    if src.portal {
        if let Some(app) = portal::query().filter(|a| !a.is_empty()) {
            return (theme_from_portal(app, hint), false);
        }
        return (fallback(hint), true);
    }
    (fallback(hint), false)
}

fn theme_from_portal(app: portal::Appearance, hint: bool) -> Theme {
    let dark = app.prefers_dark.unwrap_or(hint);
    let mut theme = fallback(dark);
    if let Some(accent) = app.accent {
        let (bg, fg) = (theme.tokens.surface, theme.tokens.text);
        theme.tokens.accent = accent;
        theme.tokens.accent_text = pick_accent_text(accent, bg, fg);
    }
    theme.source = ThemeSource::Portal;
    theme
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_palettes_are_sane() {
        for dark in [true, false] {
            let t = fallback(dark);
            assert_eq!(t.dark, dark);
            assert_eq!(t.source, ThemeSource::Fallback);
            let k = t.tokens;
            assert!(k.text.contrast(k.surface) >= 7.0);
            assert!(k.text_muted.contrast(k.surface) >= 3.0);
            assert!(k.accent_text.contrast(k.accent) >= 4.5);
            for c in [k.positive, k.warning, k.negative] {
                assert!(c.contrast(k.surface) >= 3.0, "{c} on {}", k.surface);
            }
        }
    }

    #[test]
    fn dark_detection() {
        assert!(is_dark(Rgb::new(32, 35, 38), Rgb::new(252, 252, 252)));
        assert!(!is_dark(Rgb::new(239, 240, 241), Rgb::new(35, 38, 41)));
        // Mid-gray background with black text is light, even though luminance < 0.5.
        assert!(!is_dark(Rgb::new(160, 160, 160), BLACK));
        // Degenerate: fg == bg falls back to luminance.
        assert!(is_dark(Rgb::new(20, 20, 20), Rgb::new(20, 20, 20)));
        assert!(!is_dark(Rgb::new(240, 240, 240), Rgb::new(240, 240, 240)));
    }

    #[test]
    fn surfaces_order() {
        let d = derive(
            palette::DARK_BG,
            palette::DARK_FG,
            palette::ACCENT,
            None,
            None,
            None,
        );
        assert!(d.surface_raised.relative_luminance() > d.surface.relative_luminance());
        assert!(d.surface_sunken.relative_luminance() < d.surface.relative_luminance());
        let l = derive(
            palette::LIGHT_BG,
            palette::LIGHT_FG,
            palette::ACCENT,
            None,
            None,
            None,
        );
        assert!(l.surface_raised.relative_luminance() > l.surface.relative_luminance());
        assert!(l.surface_sunken.relative_luminance() < l.surface.relative_luminance());
        assert_eq!(l.text_muted, palette::LIGHT_FG.mix(palette::LIGHT_BG, 0.4));
        assert_eq!(l.border, palette::LIGHT_FG.mix(palette::LIGHT_BG, 0.8));
    }

    #[test]
    fn accent_text_prefers_scheme_colors_then_black_white() {
        let bg = Rgb::new(32, 35, 38);
        let fg = Rgb::new(252, 252, 252);
        // Light accent on a dark scheme: dark bg works as text.
        assert_eq!(pick_accent_text(Rgb::new(61, 174, 233), bg, fg), bg);
        // Mid accent where neither fg nor bg qualifies falls to black/white.
        let gray_bg = Rgb::new(120, 120, 120);
        let gray_fg = Rgb::new(140, 140, 140);
        let accent = Rgb::new(0, 0, 160);
        let t = pick_accent_text(accent, gray_bg, gray_fg);
        assert_eq!(t, WHITE);
        // Every 8-bit gray accent gets readable text.
        for v in (0..=255u8).step_by(5) {
            let a = Rgb::new(v, v, v);
            let t = pick_accent_text(a, gray_bg, gray_fg);
            assert!(a.contrast(t) >= 4.5, "accent {a} text {t}");
        }
    }

    #[test]
    fn given_status_colors_are_kept_when_legible() {
        let pos = Rgb::new(39, 174, 96);
        let k = derive(
            palette::DARK_BG,
            palette::DARK_FG,
            palette::ACCENT,
            Some(pos),
            None,
            None,
        );
        assert_eq!(k.positive, pos);
        // An illegible desktop value is nudged until legible.
        let bad = Rgb::new(40, 40, 40);
        let k = derive(
            palette::DARK_BG,
            palette::DARK_FG,
            palette::ACCENT,
            None,
            None,
            Some(bad),
        );
        assert_ne!(k.negative, bad);
        assert!(k.negative.contrast(k.surface) >= 3.0);
    }

    #[test]
    fn tokens_iter_lists_all() {
        let t = fallback(true).tokens;
        let v: Vec<_> = t.iter().collect();
        assert_eq!(v.len(), 11);
        assert_eq!(v[0], ("surface", t.surface));
        assert_eq!(v[10], ("negative", t.negative));
        let names: std::collections::HashSet<_> = v.iter().map(|(n, _)| *n).collect();
        assert_eq!(names.len(), 11);
    }

    #[test]
    fn portal_theme_uses_preference_and_accent() {
        let app = portal::Appearance {
            prefers_dark: Some(true),
            accent: Some(Rgb::new(200, 30, 30)),
        };
        let t = theme_from_portal(app, false);
        assert!(t.dark);
        assert_eq!(t.source, ThemeSource::Portal);
        assert_eq!(t.tokens.accent, Rgb::new(200, 30, 30));
        assert!(t.tokens.accent_text.contrast(t.tokens.accent) >= 4.5);
        let t = theme_from_portal(
            portal::Appearance {
                prefers_dark: None,
                accent: None,
            },
            true,
        );
        assert!(t.dark);
    }

    #[test]
    fn missing_kdeglobals_file_is_none() {
        let p = std::env::temp_dir().join("newskite-theme-definitely-missing-kdeglobals");
        assert!(matches!(from_kdeglobals_path(&p), Ok(None)));
        let src = Sources {
            kdeglobals: Some(p),
            portal: false,
            prefers_dark: Some(true),
        };
        assert_eq!(resolve_from(&src), fallback(true));
    }
}
