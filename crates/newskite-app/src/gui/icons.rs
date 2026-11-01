//! UI icons and the product wordmark, rendered from SVG with the `egui_extras` loader.
//!
//! The icon set is the innerpulse kit's (`assets/brand/kit/ui-icons/`): a 24 grid, one 2 px
//! stroke with round caps and joins, no fills, `currentColor`. Where the kit has no
//! matching icon (Back, Home, My List, Newsgroups, Open NZB, folder, key, chevron up) the
//! icon below is drawn in the same grammar. Every SVG is registered with its
//! `currentColor` replaced by white, so painting it with a tint gives exactly the tint;
//! each paint rasterizes at the target's physical pixel size, so icons stay crisp at any
//! scale factor.

use eframe::egui::{
    self, emath::GuiRounding, load::SizeHint, load::TexturePoll, Color32, ImageSource, Pos2, Rect,
    TextureOptions,
};

/// Every icon the GUI paints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    /// Left arrow (drawn here).
    Back,
    /// House (drawn here).
    Home,
    /// Magnifier (kit `search`).
    Search,
    /// Bulleted list (drawn here).
    List,
    /// Two speech bubbles (drawn here).
    Newsgroups,
    /// Circular arrow (kit `refresh`), for Auto Search.
    AutoSearch,
    /// Document with a plus (drawn here).
    OpenNzb,
    /// Folder (drawn here).
    Folder,
    /// Arrow into a tray (kit `download`).
    Transfers,
    /// Two bars (kit `pause`).
    Pause,
    /// Triangle (kit `run`).
    Play,
    /// Faders with ring knobs (kit `settings`).
    Gear,
    /// Plus sign (kit `plus`).
    Plus,
    /// Diagonal cross (kit `close`).
    Close,
    /// Circular arrow (kit `refresh`), for Retry.
    Retry,
    /// Key (drawn here).
    Key,
    /// Downward chevron (kit `chevron-down`).
    ChevronDown,
    /// Upward chevron (drawn here).
    ChevronUp,
    /// Rightward chevron (kit `chevron-right`).
    ChevronRight,
    /// Triangle with a bang (kit `alert`).
    Alert,
    /// Circled i (kit `info`).
    Info,
    /// Box with an arrow out (kit `external`).
    External,
    /// Status glyph: check (kit `status-ok`, 12 grid).
    StatusOk,
    /// Status glyph: bar and dot (kit `status-warn`).
    StatusWarn,
    /// Status glyph: cross (kit `status-err`).
    StatusErr,
    /// Status glyph: dash (kit `status-idle`).
    StatusIdle,
}

impl Icon {
    /// Every icon (for registration and tests).
    pub const ALL: [Icon; 26] = [
        Icon::Back,
        Icon::Home,
        Icon::Search,
        Icon::List,
        Icon::Newsgroups,
        Icon::AutoSearch,
        Icon::OpenNzb,
        Icon::Folder,
        Icon::Transfers,
        Icon::Pause,
        Icon::Play,
        Icon::Gear,
        Icon::Plus,
        Icon::Close,
        Icon::Retry,
        Icon::Key,
        Icon::ChevronDown,
        Icon::ChevronUp,
        Icon::ChevronRight,
        Icon::Alert,
        Icon::Info,
        Icon::External,
        Icon::StatusOk,
        Icon::StatusWarn,
        Icon::StatusErr,
        Icon::StatusIdle,
    ];

    /// The `bytes://` URI the icon is registered under.
    pub fn uri(self) -> &'static str {
        match self {
            Icon::Back => "bytes://newskite/icon/back.svg",
            Icon::Home => "bytes://newskite/icon/home.svg",
            Icon::Search => "bytes://newskite/icon/search.svg",
            Icon::List => "bytes://newskite/icon/list.svg",
            Icon::Newsgroups => "bytes://newskite/icon/newsgroups.svg",
            Icon::AutoSearch | Icon::Retry => "bytes://newskite/icon/refresh.svg",
            Icon::OpenNzb => "bytes://newskite/icon/open-nzb.svg",
            Icon::Folder => "bytes://newskite/icon/folder.svg",
            Icon::Transfers => "bytes://newskite/icon/download.svg",
            Icon::Pause => "bytes://newskite/icon/pause.svg",
            Icon::Play => "bytes://newskite/icon/run.svg",
            Icon::Gear => "bytes://newskite/icon/settings.svg",
            Icon::Plus => "bytes://newskite/icon/plus.svg",
            Icon::Close => "bytes://newskite/icon/close.svg",
            Icon::Key => "bytes://newskite/icon/key.svg",
            Icon::ChevronDown => "bytes://newskite/icon/chevron-down.svg",
            Icon::ChevronUp => "bytes://newskite/icon/chevron-up.svg",
            Icon::ChevronRight => "bytes://newskite/icon/chevron-right.svg",
            Icon::Alert => "bytes://newskite/icon/alert.svg",
            Icon::Info => "bytes://newskite/icon/info.svg",
            Icon::External => "bytes://newskite/icon/external.svg",
            Icon::StatusOk => "bytes://newskite/icon/status-ok.svg",
            Icon::StatusWarn => "bytes://newskite/icon/status-warn.svg",
            Icon::StatusErr => "bytes://newskite/icon/status-err.svg",
            Icon::StatusIdle => "bytes://newskite/icon/status-idle.svg",
        }
    }

    /// The SVG source (with `currentColor`).
    pub fn svg(self) -> std::borrow::Cow<'static, str> {
        use std::borrow::Cow;
        macro_rules! kit {
            ($name:literal) => {
                Cow::Borrowed(include_str!(concat!(
                    "../../../../assets/brand/kit/ui-icons/",
                    $name,
                    ".svg"
                )))
            };
        }
        match self {
            Icon::Back => drawn("M19.5 12h-15M10.5 6l-6 6 6 6"),
            Icon::Home => drawn("M3.5 11.5L12 4l8.5 7.5M6 9.5V20h12V9.5M10 20v-5.5h4V20"),
            Icon::List => {
                drawn("M9.5 6.5h10.5M9.5 12h10.5M9.5 17.5h10.5M4.5 6.5v.1M4.5 12v.1M4.5 17.5v.1")
            }
            Icon::Newsgroups => {
                drawn("M3.5 4.5h11v8.5h-6l-3 3v-3h-2zM14.5 8.5h6v9h-2v2.5l-3-2.5h-5V13")
            }
            Icon::OpenNzb => drawn("M13.5 3.5H6v17h12V8zM13.5 3.5V8H18M12 11.5v6M9 14.5h6"),
            Icon::Folder => drawn("M3.5 5.5h6l2 2.5h9v11.5h-17z"),
            Icon::Key => drawn(
                "M10.5 12a3.5 3.5 0 1 1-7 0a3.5 3.5 0 1 1 7 0M10.5 12h10M17.5 12v3M20.5 12v2.5",
            ),
            Icon::ChevronUp => drawn("M6 14.5l6-6 6 6"),
            Icon::Search => kit!("search"),
            Icon::AutoSearch | Icon::Retry => kit!("refresh"),
            Icon::Transfers => kit!("download"),
            Icon::Pause => kit!("pause"),
            Icon::Play => kit!("run"),
            Icon::Gear => kit!("settings"),
            Icon::Plus => kit!("plus"),
            Icon::Close => kit!("close"),
            Icon::ChevronDown => kit!("chevron-down"),
            Icon::ChevronRight => kit!("chevron-right"),
            Icon::Alert => kit!("alert"),
            Icon::Info => kit!("info"),
            Icon::External => kit!("external"),
            Icon::StatusOk => kit!("status-ok"),
            Icon::StatusWarn => kit!("status-warn"),
            Icon::StatusErr => kit!("status-err"),
            Icon::StatusIdle => kit!("status-idle"),
        }
    }
}

/// An icon drawn here in the kit's grammar: the kit's own SVG header around `d`.
fn drawn(d: &str) -> std::borrow::Cow<'static, str> {
    std::borrow::Cow::Owned(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" viewBox=\"0 0 24 24\" \
         aria-hidden=\"true\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" \
         stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"{d}\"/></svg>"
    ))
}

/// The product wordmark's `bytes://` URI.
pub const WORDMARK_URI: &str = "bytes://newskite/wordmark.svg";
/// The wordmark's tight bounds in its own units (`x` 0…694.4; `y` from the top of the
/// `k`/`t` ascenders, −75, to just under the baseline, 1.5). The x-height runs from
/// −50.5 to 0.
pub const WORDMARK_BOX: (f32, f32, f32, f32) = (0.0, -75.0, 694.4, 76.5);
/// The x-height center, as a fraction of [`WORDMARK_BOX`]'s height from its top.
pub const WORDMARK_XHEIGHT_CENTER: f32 = (75.0 - 25.25) / 76.5;

const WORDMARK_SRC: &str = include_str!("../../../../assets/brand/product/newskite-wordmark.svg");
const WORDMARK_VIEWBOX: &str = "viewBox=\"0 -92.5 694.4 120.0\"";

/// The wordmark SVG cropped to [`WORDMARK_BOX`] and filled white (for tinting). `None` if
/// the asset no longer has the expected view box or fill.
pub fn wordmark_svg() -> Option<String> {
    if !WORDMARK_SRC.contains(WORDMARK_VIEWBOX) || !WORDMARK_SRC.contains("currentColor") {
        return None;
    }
    let (x, y, w, h) = WORDMARK_BOX;
    Some(
        WORDMARK_SRC
            .replace(WORDMARK_VIEWBOX, &format!("viewBox=\"{x} {y} {w} {h}\""))
            .replace("currentColor", "#ffffff"),
    )
}

fn whitened(svg: &str) -> Vec<u8> {
    svg.replace("currentColor", "#ffffff").into_bytes()
}

/// Installs the image loaders and registers every icon and the wordmark. Idempotent.
pub fn install(ctx: &egui::Context) {
    egui_extras::install_image_loaders(ctx);
    for icon in Icon::ALL {
        ctx.include_bytes(icon.uri(), whitened(&icon.svg()));
    }
    if let Some(svg) = wordmark_svg() {
        ctx.include_bytes(WORDMARK_URI, svg.into_bytes());
    }
}

/// Paints a registered SVG into `rect`, tinted with `color`. Nothing is painted if the
/// image is not registered (e.g. in a bare test context) or `rect` is empty.
pub fn paint_svg(painter: &egui::Painter, uri: &'static str, rect: Rect, color: Color32) {
    let ctx = painter.ctx();
    let ppp = ctx.pixels_per_point();
    let rect = rect.round_to_pixels(ppp);
    let px = rect.size() * ppp;
    if px.x.is_nan() || px.y.is_nan() || px.x < 1.0 || px.y < 1.0 {
        return;
    }
    let hint = SizeHint::Size {
        width: px.x.round() as u32,
        height: px.y.round() as u32,
        maintain_aspect_ratio: false,
    };
    if let Ok(TexturePoll::Ready { texture }) =
        ImageSource::Uri(uri.into()).load(ctx, TextureOptions::LINEAR, hint)
    {
        painter.image(
            texture.id,
            rect,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            color,
        );
    }
}

/// Paints `icon` inside `rect` (should be square) in `color`.
pub fn paint_icon(painter: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    paint_svg(painter, icon.uri(), rect, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_follows_the_kit_grammar() {
        for icon in Icon::ALL {
            let svg = icon.svg();
            assert!(svg.contains("fill=\"none\""), "{icon:?}");
            assert!(svg.contains("stroke=\"currentColor\""), "{icon:?}");
            assert!(svg.contains("stroke-linecap=\"round\""), "{icon:?}");
            assert!(svg.contains("stroke-linejoin=\"round\""), "{icon:?}");
            let grid = if icon.uri().contains("status-") {
                "0 0 12 12"
            } else {
                "0 0 24 24"
            };
            assert!(svg.contains(grid), "{icon:?}");
            assert!(!whitened(&svg).windows(12).any(|w| w == b"currentColor"));
        }
    }

    #[test]
    fn every_icon_and_the_wordmark_rasterize() {
        let opts = resvg::usvg::Options::default();
        for icon in Icon::ALL {
            let img = egui_extras::image::load_svg_bytes_with_size(
                &whitened(&icon.svg()),
                SizeHint::Size {
                    width: 20,
                    height: 20,
                    maintain_aspect_ratio: false,
                },
                &opts,
            )
            .unwrap_or_else(|e| panic!("{icon:?}: {e}"));
            assert!(
                img.pixels.iter().any(|p| p.a() > 0),
                "{icon:?} paints something"
            );
        }
        let wm = wordmark_svg().expect("wordmark asset has the expected view box");
        let img = egui_extras::image::load_svg_bytes_with_size(
            wm.as_bytes(),
            SizeHint::Height(15),
            &opts,
        )
        .expect("wordmark rasterizes");
        assert_eq!(img.size[1], 15);
        assert!(img.size[0] > 120 && img.size[0] < 150, "{:?}", img.size);
        // The crop keeps every glyph: the outer columns and rows are not empty.
        let (w, h) = (img.size[0], img.size[1]);
        let alpha = |x: usize, y: usize| img.pixels[y * w + x].a();
        assert!((0..h).any(|y| alpha(0, y) > 0 || alpha(1, y) > 0));
        assert!((0..h).any(|y| alpha(w - 1, y) > 0 || alpha(w - 2, y) > 0));
        assert!((0..w).any(|x| alpha(x, 0) > 0 || alpha(x, 1) > 0));
    }

    #[test]
    fn painting_without_loaders_is_harmless() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                for icon in Icon::ALL {
                    paint_icon(
                        ui.painter(),
                        Rect::from_min_size(Pos2::ZERO, egui::Vec2::splat(20.0)),
                        icon,
                        ui.visuals().text_color(),
                    );
                    paint_icon(ui.painter(), Rect::NOTHING, icon, ui.visuals().text_color());
                }
            });
        });
    }
}
