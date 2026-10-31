//! Small reusable widgets: vector icons, toolbar buttons, pills, state badges, progress
//! bars, cards and empty states. All colors come from the [`Palette`].

use std::f32::consts::TAU;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, RichText, Sense, Shape,
    Stroke, StrokeKind, Ui, Vec2,
};
use newskite_service::JobState;

use super::theme::Palette;

/// Vector icons drawn with the painter (no image assets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    /// Left arrow.
    Back,
    /// House.
    Home,
    /// Magnifier.
    Search,
    /// Bulleted list.
    List,
    /// Two speech bubbles.
    Newsgroups,
    /// Circular arrow around a dot.
    AutoSearch,
    /// Document with a plus.
    OpenNzb,
    /// Folder.
    Folder,
    /// Arrow into a tray.
    Transfers,
    /// Two bars.
    Pause,
    /// Triangle.
    Play,
    /// Cog wheel.
    Gear,
    /// Plus sign.
    Plus,
    /// Diagonal cross.
    Close,
    /// Circular arrow.
    Retry,
    /// Key.
    Key,
    /// Downward chevron.
    ChevronDown,
    /// Upward chevron.
    ChevronUp,
    /// Rightward chevron.
    ChevronRight,
}

/// Stroke width of icons at the 20 px reference size.
const ICON_STROKE: f32 = 1.6;

/// Points on a circular arc (angles in radians, 0 = +x, clockwise on screen).
fn arc(center: Pos2, radius: f32, from: f32, to: f32, segments: usize) -> Vec<Pos2> {
    let n = segments.max(2);
    (0..=n)
        .map(|i| {
            let a = from + (to - from) * (i as f32 / n as f32);
            center + Vec2::new(a.cos(), a.sin()) * radius
        })
        .collect()
}

/// Paints `icon` inside `rect` (should be square) with `color`.
pub fn paint_icon(painter: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = rect.width().min(rect.height());
    let origin = rect.center() - Vec2::splat(s / 2.0);
    let p = |x: f32, y: f32| origin + Vec2::new(x * s, y * s);
    let stroke = Stroke::new(ICON_STROKE * (s / 20.0).max(0.75), color);
    let line = |pts: Vec<Pos2>| painter.add(Shape::line(pts, stroke));
    let closed = |pts: Vec<Pos2>| painter.add(Shape::closed_line(pts, stroke));

    match icon {
        Icon::Back => {
            line(vec![p(0.18, 0.5), p(0.82, 0.5)]);
            line(vec![p(0.44, 0.24), p(0.18, 0.5), p(0.44, 0.76)]);
        }
        Icon::Home => {
            line(vec![p(0.12, 0.5), p(0.5, 0.16), p(0.88, 0.5)]);
            line(vec![
                p(0.24, 0.4),
                p(0.24, 0.86),
                p(0.76, 0.86),
                p(0.76, 0.4),
            ]);
            line(vec![
                p(0.42, 0.86),
                p(0.42, 0.62),
                p(0.58, 0.62),
                p(0.58, 0.86),
            ]);
        }
        Icon::Search => {
            painter.circle_stroke(p(0.43, 0.43), 0.25 * s, stroke);
            line(vec![p(0.62, 0.62), p(0.86, 0.86)]);
        }
        Icon::List => {
            for y in [0.26, 0.5, 0.74] {
                painter.circle_filled(p(0.2, y), 0.055 * s, color);
                line(vec![p(0.34, y), p(0.86, y)]);
            }
        }
        Icon::Newsgroups => {
            let r = CornerRadius::same((0.1 * s) as u8);
            painter.rect_stroke(
                Rect::from_min_max(p(0.1, 0.16), p(0.66, 0.54)),
                r,
                stroke,
                StrokeKind::Middle,
            );
            line(vec![p(0.22, 0.54), p(0.2, 0.68), p(0.36, 0.54)]);
            line(vec![
                p(0.74, 0.36),
                p(0.9, 0.36),
                p(0.9, 0.72),
                p(0.82, 0.72),
                p(0.84, 0.86),
                p(0.68, 0.72),
                p(0.4, 0.72),
                p(0.4, 0.62),
            ]);
        }
        Icon::AutoSearch => {
            let c = p(0.5, 0.5);
            line(arc(c, 0.32 * s, -0.25 * TAU, 0.55 * TAU, 24));
            // Arrow head at the arc start (top).
            line(vec![p(0.38, 0.06), p(0.5, 0.18), p(0.38, 0.3)]);
            painter.circle_filled(c, 0.08 * s, color);
        }
        Icon::OpenNzb => {
            closed(vec![
                p(0.22, 0.1),
                p(0.6, 0.1),
                p(0.8, 0.3),
                p(0.8, 0.9),
                p(0.22, 0.9),
            ]);
            line(vec![p(0.6, 0.1), p(0.6, 0.3), p(0.8, 0.3)]);
            line(vec![p(0.51, 0.44), p(0.51, 0.76)]);
            line(vec![p(0.35, 0.6), p(0.67, 0.6)]);
        }
        Icon::Folder => {
            closed(vec![
                p(0.1, 0.24),
                p(0.38, 0.24),
                p(0.47, 0.34),
                p(0.9, 0.34),
                p(0.9, 0.82),
                p(0.1, 0.82),
            ]);
            line(vec![p(0.1, 0.44), p(0.9, 0.44)]);
        }
        Icon::Transfers => {
            line(vec![p(0.5, 0.12), p(0.5, 0.6)]);
            line(vec![p(0.31, 0.42), p(0.5, 0.61), p(0.69, 0.42)]);
            line(vec![
                p(0.16, 0.6),
                p(0.16, 0.86),
                p(0.84, 0.86),
                p(0.84, 0.6),
            ]);
        }
        Icon::Pause => {
            let r = CornerRadius::same((0.04 * s) as u8);
            painter.rect_filled(Rect::from_min_max(p(0.27, 0.2), p(0.42, 0.8)), r, color);
            painter.rect_filled(Rect::from_min_max(p(0.58, 0.2), p(0.73, 0.8)), r, color);
        }
        Icon::Play => {
            painter.add(Shape::convex_polygon(
                vec![p(0.3, 0.18), p(0.82, 0.5), p(0.3, 0.82)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Gear => {
            let c = p(0.5, 0.5);
            painter.circle_stroke(c, 0.12 * s, stroke);
            painter.circle_stroke(c, 0.27 * s, stroke);
            for i in 0..8 {
                let a = i as f32 / 8.0 * TAU;
                let d = Vec2::new(a.cos(), a.sin());
                painter.line_segment(
                    [c + d * 0.27 * s, c + d * 0.4 * s],
                    Stroke::new(stroke.width * 1.6, color),
                );
            }
        }
        Icon::Plus => {
            line(vec![p(0.5, 0.18), p(0.5, 0.82)]);
            line(vec![p(0.18, 0.5), p(0.82, 0.5)]);
        }
        Icon::Close => {
            line(vec![p(0.24, 0.24), p(0.76, 0.76)]);
            line(vec![p(0.76, 0.24), p(0.24, 0.76)]);
        }
        Icon::Retry => {
            let c = p(0.5, 0.52);
            line(arc(c, 0.3 * s, -0.2 * TAU, 0.7 * TAU, 24));
            let tip = c + Vec2::new((-0.2 * TAU).cos(), (-0.2 * TAU).sin()) * 0.3 * s;
            line(vec![
                tip + Vec2::new(-0.2 * s, -0.02 * s),
                tip,
                tip + Vec2::new(0.0, 0.2 * s),
            ]);
        }
        Icon::Key => {
            painter.circle_stroke(p(0.3, 0.5), 0.15 * s, stroke);
            line(vec![p(0.45, 0.5), p(0.88, 0.5)]);
            line(vec![p(0.72, 0.5), p(0.72, 0.66)]);
            line(vec![p(0.84, 0.5), p(0.84, 0.62)]);
        }
        Icon::ChevronDown => {
            line(vec![p(0.26, 0.38), p(0.5, 0.62), p(0.74, 0.38)]);
        }
        Icon::ChevronUp => {
            line(vec![p(0.26, 0.62), p(0.5, 0.38), p(0.74, 0.62)]);
        }
        Icon::ChevronRight => {
            line(vec![p(0.38, 0.26), p(0.62, 0.5), p(0.38, 0.74)]);
        }
    }
}

/// How a toolbar button is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolState {
    /// A working action.
    Normal,
    /// The action reflects the current view (e.g. the Queue tab is showing).
    Active,
    /// Visible for layout parity but not implemented yet; clicking is harmless.
    Unavailable,
}

/// A toolbar button: icon with an optional label underneath.
pub fn tool_button(
    ui: &mut Ui,
    pal: &Palette,
    icon: Icon,
    label: &str,
    show_label: bool,
    state: ToolState,
) -> Response {
    let label_font = FontId::proportional(11.0);
    let size = if show_label {
        let text_w = ui
            .painter()
            .layout_no_wrap(label.to_owned(), label_font.clone(), pal.text)
            .size()
            .x;
        Vec2::new((text_w + 14.0).max(58.0), 50.0)
    } else {
        Vec2::new(36.0, 36.0)
    };
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let bg = if resp.is_pointer_button_down_on() {
            Some(pal.accent_soft)
        } else if resp.hovered() || state == ToolState::Active {
            Some(pal.hover)
        } else {
            None
        };
        if let Some(bg) = bg {
            painter.rect_filled(rect, CornerRadius::same(6), bg);
        }
        if resp.has_focus() {
            painter.rect_stroke(
                rect,
                CornerRadius::same(6),
                Stroke::new(1.0, pal.accent),
                StrokeKind::Inside,
            );
        }
        let fg = match state {
            ToolState::Unavailable => pal.muted,
            ToolState::Active => pal.accent_fg,
            ToolState::Normal => pal.text,
        };
        let icon_rect = if show_label {
            Rect::from_center_size(
                Pos2::new(rect.center().x, rect.top() + 17.0),
                Vec2::splat(20.0),
            )
        } else {
            Rect::from_center_size(rect.center(), Vec2::splat(20.0))
        };
        paint_icon(painter, icon_rect, icon, fg);
        if show_label {
            let text_color = if state == ToolState::Unavailable {
                pal.muted
            } else {
                pal.text
            };
            painter.text(
                Pos2::new(rect.center().x, rect.bottom() - 9.0),
                Align2::CENTER_CENTER,
                label,
                label_font,
                text_color,
            );
        }
    }
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            state != ToolState::Unavailable,
            label,
        )
    });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small square icon button (sidebar `+`, row `×`, etc.).
pub fn icon_button(
    ui: &mut Ui,
    pal: &Palette,
    icon: Icon,
    color: Color32,
    tooltip: &str,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
    if ui.is_rect_visible(rect) {
        if resp.hovered() {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(4), pal.hover);
        }
        paint_icon(ui.painter(), rect.shrink(4.0), icon, color);
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tooltip));
    resp.on_hover_text(tooltip)
}

/// A button with a leading icon and text.
pub fn icon_text_button(
    ui: &mut Ui,
    pal: &Palette,
    icon: Icon,
    text: &str,
    enabled: bool,
) -> Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, pal.text);
    let size = Vec2::new(galley.size().x + 40.0, 26.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let vis = ui.style().interact(&resp);
        let fill = if enabled {
            vis.weak_bg_fill
        } else {
            pal.surface
        };
        ui.painter().rect(
            rect.expand(vis.expansion),
            CornerRadius::same(5),
            fill,
            Stroke::new(1.0, pal.border),
            StrokeKind::Inside,
        );
        let fg = if enabled { pal.text } else { pal.muted };
        let icon_rect = Rect::from_center_size(
            Pos2::new(rect.left() + 16.0, rect.center().y),
            Vec2::splat(14.0),
        );
        paint_icon(ui.painter(), icon_rect, icon, fg);
        ui.painter().galley(
            Pos2::new(rect.left() + 28.0, rect.center().y - galley.size().y / 2.0),
            galley,
            fg,
        );
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
    resp
}

/// A rounded pill with a leading status dot.
pub fn pill(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) -> Response {
    let font = FontId::proportional(12.0);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, fg);
    let size = Vec2::new(galley.size().x + 28.0, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(11), bg);
        painter.circle_filled(Pos2::new(rect.left() + 11.0, rect.center().y), 3.5, fg);
        painter.galley(
            Pos2::new(rect.left() + 19.0, rect.center().y - galley.size().y / 2.0),
            galley,
            fg,
        );
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    resp
}

/// Foreground/background colors of a job-state badge.
pub fn state_colors(pal: &Palette, state: JobState) -> (Color32, Color32) {
    match state {
        JobState::Queued => (pal.muted, pal.sunken),
        JobState::Downloading => (pal.accent_fg, pal.accent_soft),
        JobState::Done => (pal.positive, pal.positive_soft),
        JobState::Failed => (pal.negative, pal.negative_soft),
        JobState::Paused => (pal.warning, pal.warning_soft),
    }
}

/// Job-state badge (`queued`, `downloading`, ...).
pub fn state_badge(ui: &mut Ui, pal: &Palette, state: JobState) -> Response {
    let (fg, bg) = state_colors(pal, state);
    let font = FontId::proportional(11.5);
    let galley = ui
        .painter()
        .layout_no_wrap(state.as_str().to_owned(), font, fg);
    let size = Vec2::new(galley.size().x + 16.0, 20.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, CornerRadius::same(4), bg);
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, fg);
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, state.as_str()));
    resp
}

/// A thin progress bar. `fraction` is clamped to 0..=1 and NaN is treated as 0.
pub fn progress_bar(
    ui: &mut Ui,
    pal: &Palette,
    fraction: f32,
    color: Color32,
    width: f32,
) -> Response {
    let f = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width.max(10.0), 6.0), Sense::hover());
    if ui.is_rect_visible(rect) {
        let r = CornerRadius::same(3);
        ui.painter().rect_filled(rect, r, pal.sunken);
        ui.painter()
            .rect_stroke(rect, r, Stroke::new(1.0, pal.border), StrokeKind::Inside);
        if f > 0.0 {
            let mut fill = rect;
            fill.set_width((rect.width() * f).max(4.0));
            ui.painter().rect_filled(fill, r, color);
        }
    }
    resp
}

/// A raised card frame used for docks and settings sections.
pub fn card(pal: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(pal.raised)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10))
}

/// Small uppercase section title.
pub fn section_title(ui: &mut Ui, pal: &Palette, text: &str) {
    ui.label(
        RichText::new(text.to_uppercase())
            .size(11.0)
            .strong()
            .color(pal.muted),
    );
}

/// Centered, readable empty-state text.
pub fn empty_state(ui: &mut Ui, pal: &Palette, text: &str) {
    ui.add_space(8.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(text).color(pal.muted));
    });
    ui.add_space(8.0);
}

/// A link-styled label (accent text, underline on hover).
pub fn link_label(ui: &mut Ui, pal: &Palette, text: &str) -> Response {
    let resp =
        ui.add(egui::Label::new(RichText::new(text).color(pal.accent_fg)).sense(Sense::click()));
    if resp.hovered() {
        let r = resp.rect;
        ui.painter().line_segment(
            [r.left_bottom(), r.right_bottom()],
            Stroke::new(1.0, pal.accent_fg),
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A key/value row inside a two-column grid.
pub fn kv(ui: &mut Ui, pal: &Palette, key: &str, value: impl Into<String>) {
    let value: String = value.into();
    ui.label(RichText::new(key).color(pal.muted));
    ui.label(RichText::new(value).color(pal.text));
    ui.end_row();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arc_endpoints() {
        let pts = arc(Pos2::ZERO, 1.0, 0.0, TAU / 4.0, 4);
        assert_eq!(pts.len(), 5);
        assert!((pts[0].x - 1.0).abs() < 1e-5 && pts[0].y.abs() < 1e-5);
        assert!(pts[4].x.abs() < 1e-5 && (pts[4].y - 1.0).abs() < 1e-5);
        assert_eq!(arc(Pos2::ZERO, 1.0, 0.0, 1.0, 0).len(), 3);
    }

    #[test]
    fn every_icon_paints_without_panicking() {
        let ctx = egui::Context::default();
        let pal = Palette::from_theme(&newskite_theme::fallback(true));
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let painter = ui.painter();
                for icon in [
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
                ] {
                    paint_icon(
                        painter,
                        Rect::from_min_size(Pos2::ZERO, Vec2::splat(20.0)),
                        icon,
                        pal.text,
                    );
                    // Degenerate sizes must not panic either.
                    paint_icon(
                        painter,
                        Rect::from_min_size(Pos2::ZERO, Vec2::ZERO),
                        icon,
                        pal.text,
                    );
                }
            });
        });
    }
}
