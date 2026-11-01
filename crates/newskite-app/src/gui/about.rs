//! The About dialog: the live mark at 64 px, the product wordmark, the version, one line
//! of description and the quiet credit. No tagline.

use eframe::egui::{self, Id, Pos2, RichText, Vec2};

use super::app::Action;
use super::brand;
use super::fonts;
use super::icons;
use super::theme::Palette;
use super::widgets::{credit, kit_button, ButtonKind, CREDIT_URL};

/// One-line description shown under the version.
pub const DESCRIPTION: &str = "Usenet (NZB) download client";

/// The version line.
pub fn version_line() -> String {
    format!("Version {}", env!("CARGO_PKG_VERSION"))
}

/// Draws the modal with the mark at `mark` heights. Esc is handled by the app.
pub fn show(ctx: &egui::Context, pal: &Palette, mark: &[f64; 6], actions: &mut Vec<Action>) {
    egui::Modal::new(Id::new("about-modal"))
        .backdrop_color(pal.backdrop)
        .show(ctx, |ui| {
            ui.set_width(320.0);
            ui.vertical_centered(|ui| {
                ui.add_space(12.0);
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(64.0), egui::Sense::hover());
                brand::paint_mark(ui.painter(), r, mark, pal.mark);
                resp.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Image, true, "innerpulse mark")
                });
                ui.add_space(14.0);
                let wm_w = 150.0;
                let (_, _, bw, bh) = icons::WORDMARK_BOX;
                let (wr, wresp) =
                    ui.allocate_exact_size(Vec2::new(wm_w, wm_w * bh / bw), egui::Sense::hover());
                icons::paint_svg(ui.painter(), icons::WORDMARK_URI, wr, pal.text);
                wresp.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "Newskite")
                });
                ui.add_space(10.0);
                ui.label(
                    RichText::new(version_line())
                        .font(fonts::mono_id(12.0))
                        .color(pal.muted),
                );
                ui.add_space(2.0);
                ui.label(RichText::new(DESCRIPTION).color(pal.text));
                ui.add_space(18.0);
                if credit(ui, pal).clicked() {
                    actions.push(Action::OpenUrl(CREDIT_URL.to_owned()));
                }
                ui.add_space(14.0);
                let sep = ui.available_rect_before_wrap();
                ui.painter().line_segment(
                    [
                        Pos2::new(sep.left(), sep.top()),
                        Pos2::new(sep.right(), sep.top()),
                    ],
                    egui::Stroke::new(1.0, pal.border),
                );
                ui.add_space(10.0);
                if kit_button(ui, pal, ButtonKind::Text, None, "Close", true).clicked() {
                    actions.push(Action::CloseAbout);
                }
            });
        });
}
