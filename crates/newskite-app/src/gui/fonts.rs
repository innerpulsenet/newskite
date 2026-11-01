//! The brand faces: Instrument Sans for the interface, JetBrains Mono for what measures
//! (readouts, numbers, IDs, log lines, the masthead label).
//!
//! egui's rasterizer does not apply variable-font axes, so static instances (wght 400 and
//! 600, Instrument Sans at wdth 100) generated with fontTools live in
//! `assets/brand/fonts/static/` (SIL OFL 1.1, `assets/brand/fonts/OFL.txt`). egui's
//! default fonts stay behind them as fallbacks for symbols and emoji.

use std::sync::Arc;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily, FontId};

const SANS_400: &[u8] =
    include_bytes!("../../../../assets/brand/fonts/static/InstrumentSans-Regular.ttf");
const SANS_600: &[u8] =
    include_bytes!("../../../../assets/brand/fonts/static/InstrumentSans-SemiBold.ttf");
const MONO_400: &[u8] =
    include_bytes!("../../../../assets/brand/fonts/static/JetBrainsMono-Regular.ttf");
const MONO_600: &[u8] =
    include_bytes!("../../../../assets/brand/fonts/static/JetBrainsMono-SemiBold.ttf");

/// Family name of Instrument Sans 600 (headings, the filled button, strong labels).
pub const SANS_SEMIBOLD: &str = "instrument-sans-600";
/// Family name of JetBrains Mono 600 (the readout figure).
pub const MONO_SEMIBOLD: &str = "jetbrains-mono-600";

/// Instrument Sans 600.
pub fn semibold() -> FontFamily {
    FontFamily::Name(SANS_SEMIBOLD.into())
}

/// JetBrains Mono 600.
pub fn mono_semibold() -> FontFamily {
    FontFamily::Name(MONO_SEMIBOLD.into())
}

/// Instrument Sans 600 at `size`.
pub fn semibold_id(size: f32) -> FontId {
    FontId::new(size, semibold())
}

/// JetBrains Mono 400 at `size`.
pub fn mono_id(size: f32) -> FontId {
    FontId::monospace(size)
}

/// Font definitions with the brand faces first in every family.
pub fn definitions() -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    for (name, bytes) in [
        ("InstrumentSans-Regular", SANS_400),
        ("InstrumentSans-SemiBold", SANS_600),
        ("JetBrainsMono-Regular", MONO_400),
        ("JetBrainsMono-SemiBold", MONO_600),
    ] {
        defs.font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    }
    let fallback_sans = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let fallback_mono = defs
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let with = |first: &str, rest: &[String]| -> Vec<String> {
        std::iter::once(first.to_owned())
            .chain(rest.iter().cloned())
            .collect()
    };
    defs.families.insert(
        FontFamily::Proportional,
        with("InstrumentSans-Regular", &fallback_sans),
    );
    defs.families.insert(
        FontFamily::Monospace,
        with("JetBrainsMono-Regular", &fallback_mono),
    );
    defs.families
        .insert(semibold(), with("InstrumentSans-SemiBold", &fallback_sans));
    defs.families.insert(
        mono_semibold(),
        with("JetBrainsMono-SemiBold", &fallback_mono),
    );
    defs
}

/// Installs the brand faces on `ctx` (takes effect from the next frame).
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(definitions());
}

/// Whether the brand families are live in `ctx` (fonts set during a frame only apply
/// from the next one; laying out text in a named family before that panics).
pub fn ready(ctx: &egui::Context) -> bool {
    ctx.fonts(|f| {
        let families = &f.definitions().families;
        families.contains_key(&semibold()) && families.contains_key(&mono_semibold())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_faces_lead_every_family() {
        let defs = definitions();
        let first = |f: &FontFamily| defs.families.get(f).and_then(|v| v.first()).cloned();
        assert_eq!(
            first(&FontFamily::Proportional).as_deref(),
            Some("InstrumentSans-Regular")
        );
        assert_eq!(
            first(&FontFamily::Monospace).as_deref(),
            Some("JetBrainsMono-Regular")
        );
        assert_eq!(
            first(&semibold()).as_deref(),
            Some("InstrumentSans-SemiBold")
        );
        assert_eq!(
            first(&mono_semibold()).as_deref(),
            Some("JetBrainsMono-SemiBold")
        );
        // Fallbacks remain for symbols the brand faces lack.
        assert!(defs.families[&FontFamily::Proportional].len() > 1);
    }

    #[test]
    fn faces_parse_and_cover_the_ui_text() {
        let ctx = egui::Context::default();
        install(&ctx);
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            for family in [
                FontFamily::Proportional,
                FontFamily::Monospace,
                semibold(),
                mono_semibold(),
            ] {
                let id = FontId::new(13.0, family);
                let color = ctx.style().visuals.text_color();
                let w = ctx.fonts_mut(|f| {
                    f.layout_no_wrap("No jobs · 12 CONN 1.0 MB/s …".into(), id, color)
                        .size()
                        .x
                });
                assert!(w > 50.0);
            }
        });
    }
}
