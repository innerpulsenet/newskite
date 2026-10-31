//! Integration tests: parsing real-world kdeglobals files.

use newskite_theme::{derive, fallback, from_kdeglobals_str, Rgb, ThemeSource};

const DARK: &str = include_str!("fixtures/breeze-dark.kdeglobals");
const LIGHT: &str = include_str!("fixtures/breeze-light.kdeglobals");

#[test]
fn breeze_dark() {
    let t = from_kdeglobals_str(DARK).expect("breeze dark parses");
    assert!(t.dark);
    assert_eq!(t.source, ThemeSource::KdeGlobals);
    assert_eq!(t.scheme_name.as_deref(), Some("BreezeDark"));
    let k = t.tokens;
    assert_eq!(k.surface, Rgb::new(32, 35, 38));
    assert_eq!(k.surface.hex(), "#202326");
    assert_eq!(k.text, Rgb::new(252, 252, 252));
    assert_eq!(k.accent, Rgb::new(61, 174, 233));
    assert_eq!(k.accent.hex(), "#3daee9");
    assert!(k.accent_text.contrast(k.accent) >= 4.5);
    // Breeze Dark status colors are already legible on the window background.
    assert_eq!(k.positive, Rgb::new(39, 174, 96));
    assert_eq!(k.warning, Rgb::new(246, 116, 0));
    assert_eq!(k.negative, Rgb::new(218, 68, 83));
    assert!(k.surface_raised.relative_luminance() > k.surface.relative_luminance());
    assert!(k.surface_sunken.relative_luminance() < k.surface.relative_luminance());
}

#[test]
fn breeze_light() {
    let t = from_kdeglobals_str(LIGHT).expect("breeze light parses");
    assert!(!t.dark);
    // `[General][$i]` and `Name[$e]` flag suffixes are tolerated.
    assert_eq!(t.scheme_name.as_deref(), Some("Breeze"));
    let k = t.tokens;
    assert_eq!(k.surface, Rgb::new(239, 240, 241));
    assert_eq!(k.text, Rgb::new(35, 38, 41));
    assert_eq!(k.accent, Rgb::new(61, 174, 233));
    assert_eq!(
        k.accent_text,
        Rgb::new(35, 38, 41),
        "dark text on the light-blue accent"
    );
    assert!(k.accent_text.contrast(k.accent) >= 4.5);
    // The Breeze positive green is below 3:1 on the light window color, so it is darkened.
    for c in [k.positive, k.warning, k.negative] {
        assert!(c.contrast(k.surface) >= 3.0, "{c} on {}", k.surface);
    }
    assert_ne!(k.positive, Rgb::new(39, 174, 96));
    assert!(k.text_muted.contrast(k.surface) >= 3.0);
}

#[test]
fn accent_color_override() {
    let ini = format!("{DARK}\n[General]\nAccentColor=233,100,61\n");
    let t = from_kdeglobals_str(&ini).expect("parses");
    assert_eq!(t.tokens.accent, Rgb::new(233, 100, 61));
    assert!(t.tokens.accent_text.contrast(t.tokens.accent) >= 4.5);
    // Other [General] keys from earlier in the file survive the merge.
    assert_eq!(t.scheme_name.as_deref(), Some("BreezeDark"));

    let hex = format!("{LIGHT}\n[General]\nAccentColor=#8e44ad\n");
    let t = from_kdeglobals_str(&hex).expect("parses");
    assert_eq!(t.tokens.accent, Rgb::new(0x8e, 0x44, 0xad));

    // An unparseable override falls back to the selection color.
    let bad = format!("{DARK}\n[General]\nAccentColor=not-a-color\n");
    assert_eq!(
        from_kdeglobals_str(&bad).map(|t| t.tokens.accent),
        Some(Rgb::new(61, 174, 233))
    );
}

#[test]
fn missing_sections_yield_none() {
    assert!(from_kdeglobals_str("").is_none());
    assert!(from_kdeglobals_str("[General]\nColorScheme=BreezeDark\n").is_none());
    assert!(from_kdeglobals_str("[Colors:Window]\nBackgroundNormal=32,35,38\n").is_none());
    assert!(from_kdeglobals_str("[Colors:Window]\nForegroundNormal=252,252,252\n").is_none());
    assert!(from_kdeglobals_str(
        "[Colors:Window]\nBackgroundNormal=oops\nForegroundNormal=1,2,3\n"
    )
    .is_none());
    // Window colors under a different group do not count.
    assert!(from_kdeglobals_str(
        "[Colors:View]\nBackgroundNormal=1,1,1\nForegroundNormal=200,200,200\n"
    )
    .is_none());
}

#[test]
fn minimal_file_uses_defaults() {
    let t = from_kdeglobals_str(
        "[Colors:Window]\nBackgroundNormal=10,10,10\nForegroundNormal=230,230,230\n",
    )
    .expect("minimal parses");
    assert!(t.dark);
    assert_eq!(t.scheme_name, None);
    // No selection/accent: built-in accent; no status colors: built-in dark hues.
    assert_eq!(t.tokens.accent, fallback(true).tokens.accent);
    let fb = derive(
        Rgb::new(10, 10, 10),
        Rgb::new(230, 230, 230),
        t.tokens.accent,
        None,
        None,
        None,
    );
    assert_eq!(t.tokens, fb);
}

#[test]
fn view_status_colors_take_precedence_over_window() {
    let ini = "[Colors:Window]\nBackgroundNormal=32,35,38\nForegroundNormal=252,252,252\nForegroundPositive=1,200,1\n\
               [Colors:View]\nForegroundPositive=100,220,100\n";
    let t = from_kdeglobals_str(ini).expect("parses");
    assert_eq!(t.tokens.positive, Rgb::new(100, 220, 100));
}

#[test]
fn crlf_and_comments() {
    let ini = DARK.replace('\n', "\r\n");
    let t = from_kdeglobals_str(&format!("# leading comment\r\n{ini}")).expect("parses");
    assert_eq!(t.tokens.surface, Rgb::new(32, 35, 38));
}
