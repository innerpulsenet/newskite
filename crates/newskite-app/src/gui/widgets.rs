//! Small reusable widgets built on the innerpulse components: toolbar and icon buttons,
//! the kit's buttons (one filled call to action per view, outline otherwise), status
//! chips with a glyph and words, notices, segment tracks, the article grid, cards,
//! eyebrows and the quiet credit. All colors come from the [`Palette`].

use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response,
    RichText, Sense, Stroke, StrokeKind, TextFormat, Ui, Vec2,
};
use newskite_service::JobState;

use super::brand;
use super::fonts;
use super::theme::Palette;

pub use super::icons::{paint_icon, Icon};

/// Letter spacing of mono eyebrows and readouts (≈ 0.12em at 11 px).
pub const EYEBROW_TRACKING: f32 = 1.2;

/// A single-line layout job with letter spacing (egui has no CSS `letter-spacing`).
pub fn spaced(text: &str, font: FontId, color: Color32, tracking: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(
        text,
        0.0,
        TextFormat {
            font_id: font,
            color,
            extra_letter_spacing: tracking,
            ..Default::default()
        },
    );
    job.wrap.max_rows = 1;
    job.halign = Align::LEFT;
    job
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

/// Paints the ember focus ring (the one place ember borders a control).
pub fn focus_ring(ui: &Ui, pal: &Palette, rect: Rect, radius: u8) {
    ui.painter().rect_stroke(
        rect.expand(2.0),
        CornerRadius::same(radius.saturating_add(2)),
        Stroke::new(2.0, pal.focus),
        StrokeKind::Inside,
    );
}

/// A toolbar button: icon with an optional label underneath. Icons are text-2 at rest,
/// text when hovered or current, text-3 when unavailable; never ember.
pub fn tool_button(
    ui: &mut Ui,
    pal: &Palette,
    icon: Icon,
    label: &str,
    show_label: bool,
    state: ToolState,
) -> Response {
    let label_font = FontId::proportional(11.5);
    let size = if show_label {
        let text_w = ui
            .painter()
            .layout_no_wrap(label.to_owned(), label_font.clone(), pal.text)
            .size()
            .x;
        Vec2::new((text_w + 16.0).max(58.0), 50.0)
    } else {
        Vec2::new(36.0, 36.0)
    };
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let pressed = resp.is_pointer_button_down_on();
        let bg = if pressed {
            Some(pal.sunken)
        } else if resp.hovered() || state == ToolState::Active {
            Some(pal.hover)
        } else {
            None
        };
        if let Some(bg) = bg {
            painter.rect_filled(rect, CornerRadius::same(4), bg);
        }
        if resp.has_focus() {
            focus_ring(ui, pal, rect.shrink(2.0), 4);
        }
        let fg = match state {
            ToolState::Unavailable => pal.faint,
            ToolState::Active => pal.text,
            ToolState::Normal if resp.hovered() => pal.text,
            ToolState::Normal => pal.muted,
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
            let text_color = match state {
                ToolState::Unavailable => pal.faint,
                _ => pal.text,
            };
            painter.text(
                Pos2::new(rect.center().x, rect.bottom() - 9.0),
                Align2::CENTER_CENTER,
                label,
                label_font,
                text_color,
            );
        }
        if state == ToolState::Active {
            // The current view: a short accent rule under the icon.
            let y = rect.bottom() - 1.0;
            painter.line_segment(
                [
                    Pos2::new(rect.center().x - 8.0, y),
                    Pos2::new(rect.center().x + 8.0, y),
                ],
                Stroke::new(2.0, pal.accent),
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

/// A small square icon button (sidebar `+`, row `×`, etc.). `color` is the resting
/// tint; hover brightens it to the text color.
pub fn icon_button(
    ui: &mut Ui,
    pal: &Palette,
    icon: Icon,
    color: Color32,
    tooltip: &str,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
    if ui.is_rect_visible(rect) {
        if resp.is_pointer_button_down_on() {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(4), pal.sunken);
        } else if resp.hovered() {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(4), pal.hover);
        }
        if resp.has_focus() {
            focus_ring(ui, pal, rect, 4);
        }
        let fg = if resp.hovered() { pal.text } else { color };
        paint_icon(ui.painter(), rect.shrink(3.0), icon, fg);
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tooltip));
    resp.on_hover_text(tooltip)
}

/// Kinds of kit button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    /// The one filled call to action of the view (the action ember, white text).
    Primary,
    /// The quiet outline: everything else.
    Outline,
    /// Bare: dismiss, back, less.
    Text,
}

/// The kit's button: uppercase Instrument Sans 600 at 12 px with 0.06em tracking, an
/// optional 16 px icon before the label, 30 px tall. The accessible label keeps the
/// sentence-case `text`. A disabled primary drops to the outline (no ember on a control
/// that cannot act).
pub fn kit_button(
    ui: &mut Ui,
    pal: &Palette,
    kind: ButtonKind,
    icon: Option<Icon>,
    text: &str,
    enabled: bool,
) -> Response {
    let kind = if kind == ButtonKind::Primary && !enabled {
        ButtonKind::Outline
    } else {
        kind
    };
    let font = fonts::semibold_id(12.0);
    let probe = ui
        .painter()
        .layout_job(spaced(&text.to_uppercase(), font.clone(), pal.text, 0.72));
    let icon_w = if icon.is_some() { 16.0 + 7.0 } else { 0.0 };
    let pad = if kind == ButtonKind::Text { 6.0 } else { 14.0 };
    let size = Vec2::new(probe.size().x + icon_w + 2.0 * pad, 30.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let hovered = enabled && resp.hovered();
        let pressed = enabled && resp.is_pointer_button_down_on();
        let radius = CornerRadius::same(4);
        let (fill, stroke, fg) = match kind {
            ButtonKind::Primary => {
                let f = if hovered || pressed {
                    pal.action_hover
                } else {
                    pal.action
                };
                (f, Stroke::new(1.0, f), pal.action_text)
            }
            ButtonKind::Outline => {
                let fill = if pressed { pal.sunken } else { pal.transparent };
                let border = if hovered {
                    pal.border_strong
                } else {
                    pal.border
                };
                let fg = if enabled {
                    pal.text
                } else {
                    Palette::fade(pal.text, 0.45)
                };
                (fill, Stroke::new(1.0, border), fg)
            }
            ButtonKind::Text => {
                let fg = if !enabled {
                    Palette::fade(pal.muted, 0.45)
                } else if hovered {
                    pal.text
                } else {
                    pal.muted
                };
                (pal.transparent, Stroke::NONE, fg)
            }
        };
        let painter = ui.painter();
        painter.rect(rect, radius, fill, stroke, StrokeKind::Inside);
        if resp.has_focus() {
            focus_ring(ui, pal, rect, 4);
        }
        let mut x = rect.left() + pad;
        if let Some(icon) = icon {
            paint_icon(
                painter,
                Rect::from_center_size(Pos2::new(x + 8.0, rect.center().y), Vec2::splat(16.0)),
                icon,
                fg,
            );
            x += icon_w;
        }
        let galley = painter.layout_job(spaced(&text.to_uppercase(), font, fg, 0.72));
        painter.galley(
            Pos2::new(x, rect.center().y - galley.size().y / 2.0),
            galley,
            fg,
        );
        if kind == ButtonKind::Text && hovered {
            let y = rect.bottom() - 6.0;
            painter.line_segment(
                [
                    Pos2::new(rect.left() + pad, y),
                    Pos2::new(rect.right() - pad, y),
                ],
                Stroke::new(1.0, pal.border_strong),
            );
        }
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Status of a chip or notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Something finished well.
    Ok,
    /// Needs attention.
    Warn,
    /// Failed.
    Err,
    /// Waiting / not started.
    Idle,
    /// Running now (the product accent; ember stays the mark's).
    Live,
}

impl Status {
    /// Glyph, if the status has one (live has a dot instead).
    pub fn glyph(self) -> Option<Icon> {
        match self {
            Status::Ok => Some(Icon::StatusOk),
            Status::Warn => Some(Icon::StatusWarn),
            Status::Err => Some(Icon::StatusErr),
            Status::Idle => Some(Icon::StatusIdle),
            Status::Live => None,
        }
    }

    /// Color of the glyph, words and outline.
    pub fn color(self, pal: &Palette) -> Color32 {
        match self {
            Status::Ok => pal.positive,
            Status::Warn => pal.warning,
            Status::Err => pal.negative,
            Status::Idle => pal.faint,
            Status::Live => pal.accent_fg,
        }
    }
}

/// Width a [`status_chip`] with `text` takes.
pub fn status_chip_width(ui: &Ui, text: &str) -> f32 {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        FontId::proportional(12.5),
        ui.visuals().text_color(),
    );
    galley.size().x + 8.0 + 12.0 + 7.0 + 9.0
}

/// An outlined status chip: a 1 px outline in the status color, its 12 px glyph and the
/// words (12.5 px sans). Reads without color: the glyph carries the state.
pub fn status_chip(ui: &mut Ui, pal: &Palette, status: Status, text: &str) -> Response {
    let color = status.color(pal);
    let font = FontId::proportional(12.5);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let size = Vec2::new(galley.size().x + 8.0 + 12.0 + 7.0 + 9.0, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_stroke(
            rect,
            CornerRadius::same(3),
            Stroke::new(1.0, color),
            StrokeKind::Inside,
        );
        let glyph_rect = Rect::from_center_size(
            Pos2::new(rect.left() + 14.0, rect.center().y),
            Vec2::splat(12.0),
        );
        match status.glyph() {
            Some(g) => paint_icon(painter, glyph_rect, g, color),
            None => {
                painter.circle_filled(glyph_rect.center(), 3.5, color);
            }
        }
        painter.galley(
            Pos2::new(rect.left() + 27.0, rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text));
    resp
}

/// The chip status of a job state: queued idle, downloading live, done ok, failed err,
/// paused warn.
pub fn job_status(state: JobState) -> Status {
    match state {
        JobState::Queued => Status::Idle,
        JobState::Downloading => Status::Live,
        JobState::Done => Status::Ok,
        JobState::Failed => Status::Err,
        JobState::Paused => Status::Warn,
    }
}

/// Job-state chip (`queued`, `downloading`, ...).
pub fn state_badge(ui: &mut Ui, pal: &Palette, state: JobState) -> Response {
    status_chip(ui, pal, job_status(state), state.as_str())
}

/// A notice: a hairline box on surface 1 with a 2 px rule in the status color down its
/// left edge, the glyph in that color and the words in the text color. `lead` is set in
/// the sans 600 before `text`.
pub fn notice(ui: &mut Ui, pal: &Palette, status: Status, lead: &str, text: &str) -> Response {
    let color = status.color(pal);
    let icon = match status {
        Status::Ok => Icon::StatusOk,
        Status::Warn | Status::Err => Icon::Alert,
        Status::Idle | Status::Live => Icon::Info,
    };
    let inner = egui::Frame::new()
        .fill(pal.raised)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(CornerRadius {
            nw: 0,
            sw: 0,
            ne: 4,
            se: 4,
        })
        .inner_margin(egui::Margin {
            left: 14,
            right: 14,
            top: 10,
            bottom: 10,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                paint_icon(ui.painter(), r, icon, color);
                ui.add_space(2.0);
                let mut job = LayoutJob::default();
                let body = FontId::proportional(12.5);
                if !lead.is_empty() {
                    job.append(
                        lead,
                        0.0,
                        TextFormat::simple(fonts::semibold_id(12.5), pal.text),
                    );
                    job.append(" ", 0.0, TextFormat::simple(body.clone(), pal.text));
                }
                job.append(text, 0.0, TextFormat::simple(body, pal.text));
                job.wrap.max_width = ui.available_width();
                ui.add(egui::Label::new(job).wrap());
            });
        });
    let r = inner.response.rect;
    ui.painter().line_segment(
        [
            Pos2::new(r.left() + 1.0, r.top()),
            Pos2::new(r.left() + 1.0, r.bottom()),
        ],
        Stroke::new(2.0, color),
    );
    inner.response
}

/// An error line: the `ip-s-err` glyph and the words, both in the error color. On paper
/// the error red, the action fill and ember text are all deep reds; the glyph is what
/// marks the line as an error (components.md, Fields), so every error line uses this.
pub fn error_line(ui: &mut Ui, pal: &Palette, text: &str) -> Response {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (r, _) = ui.allocate_exact_size(Vec2::new(12.0, 16.0), Sense::hover());
        paint_icon(
            ui.painter(),
            Rect::from_center_size(r.center(), Vec2::splat(12.0)),
            Icon::StatusErr,
            pal.negative,
        );
        ui.add(egui::Label::new(RichText::new(text).color(pal.negative)).wrap());
    })
    .response
}

/// Height of a segment track (the old progress bar's height).
pub const TRACK_H: f32 = 6.0;
/// Gap between the cells of a segment track.
pub const TRACK_GAP: f32 = 2.0;
/// Most cells a segment track draws.
pub const TRACK_MAX_CELLS: usize = 40;

/// Number of cells a segment track of `width` draws: as many near-square cells as fit,
/// 1 to [`TRACK_MAX_CELLS`].
pub fn track_cells(width: f32) -> usize {
    if !width.is_finite() || width <= 0.0 {
        return 1;
    }
    (((width + TRACK_GAP) / (TRACK_H + TRACK_GAP)).floor() as usize).clamp(1, TRACK_MAX_CELLS)
}

/// How full each of `cells` cells is for `fraction` (clamped; NaN is 0): each cell is an
/// equal slice of the job, so the cells fill left to right and the boundary cell fills
/// partly.
pub fn track_fill(fraction: f32, cells: usize) -> Vec<f32> {
    let f = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = f * cells as f32;
    (0..cells)
        .map(|i| (filled - i as f32).clamp(0.0, 1.0))
        .collect()
}

/// A segment track: a row of small square cells, each a proportional slice of the job's
/// articles, filled to `fraction` in `color`; empty cells are faint wells.
pub fn segment_track(
    ui: &mut Ui,
    pal: &Palette,
    fraction: f32,
    color: Color32,
    width: f32,
) -> Response {
    let width = if width.is_finite() {
        width.max(10.0)
    } else {
        10.0
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, TRACK_H), Sense::hover());
    if ui.is_rect_visible(rect) {
        let cells = track_cells(rect.width());
        let fill = track_fill(fraction, cells);
        let cell_w = (rect.width() - TRACK_GAP * (cells as f32 - 1.0)) / cells as f32;
        let painter = ui.painter();
        let r = CornerRadius::same(1);
        for (i, amount) in fill.iter().enumerate() {
            let x = rect.left() + i as f32 * (cell_w + TRACK_GAP);
            let cell = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(cell_w, TRACK_H));
            if *amount >= 1.0 {
                painter.rect_filled(cell, r, color);
            } else {
                painter.rect_filled(cell, r, pal.well);
                if *amount > 0.0 {
                    let mut part = cell;
                    part.set_width((cell_w * amount).max(1.0));
                    painter.rect_filled(part, r, color);
                }
            }
        }
    }
    resp
}

/// Cells of the empty queue's article grid: `(columns, rows)`.
pub const GRID_SIZE: (usize, usize) = (28, 4);

/// Whether cell `(col, row)` of the article grid is lit. Never: the grid is drawn only
/// while the queue is empty, and there are no articles to show then (a lit cell would
/// read as a count that does not exist).
pub fn grid_lit(_col: usize, _row: usize) -> bool {
    false
}

/// The empty queue's article grid: a few rows of quiet, unlit wells, where articles
/// will land once an NZB is queued. No baseline and no lit cells, so it reads as an
/// empty matrix, not as a chart.
pub fn article_grid(ui: &mut Ui, pal: &Palette) {
    let (cols, rows) = GRID_SIZE;
    let (cell, gap) = (7.0, 3.0);
    let w = cols as f32 * cell + (cols as f32 - 1.0) * gap;
    let h = rows as f32 * cell + (rows as f32 - 1.0) * gap;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter();
    for row in 0..rows {
        for col in 0..cols {
            let min = Pos2::new(
                rect.left() + col as f32 * (cell + gap),
                rect.top() + row as f32 * (cell + gap),
            );
            painter.rect_filled(
                Rect::from_min_size(min, Vec2::splat(cell)),
                CornerRadius::same(1),
                pal.well,
            );
        }
    }
}

/// A raised card frame used for docks and settings sections: surface 1, a hairline,
/// the kit's 8 px radius, no shadow.
pub fn card(pal: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(pal.raised)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
}

/// A mono uppercase eyebrow (11 px, 0.12em, text-3): section and dock titles.
pub fn section_title(ui: &mut Ui, pal: &Palette, text: &str) {
    let job = spaced(
        &text.to_uppercase(),
        fonts::mono_id(11.0),
        pal.faint,
        EYEBROW_TRACKING,
    );
    ui.add(egui::Label::new(job).selectable(false));
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

/// A key/value row inside a two-column grid; the value is a reading, set in mono.
pub fn kv(ui: &mut Ui, pal: &Palette, key: &str, value: impl Into<String>) {
    kv_colored(ui, pal, key, value, pal.text);
}

/// [`kv`] with a value color (e.g. a failure count).
pub fn kv_colored(ui: &mut Ui, pal: &Palette, key: &str, value: impl Into<String>, color: Color32) {
    let value: String = value.into();
    ui.label(RichText::new(key).color(pal.muted));
    ui.label(RichText::new(value).font(fonts::mono_id(12.0)).color(color));
    ui.end_row();
}

/// The innerpulse site the credit links to.
pub const CREDIT_URL: &str = "https://www.innerpulse.net/";

/// The quiet credit: a ~12 px static mark and the word `innerpulse` in muted mono; no
/// pill, no border, no motion. Hover brightens the word; a click is the caller's to
/// handle (it opens [`CREDIT_URL`]).
pub fn credit(ui: &mut Ui, pal: &Palette) -> Response {
    let font = fonts::mono_id(11.5);
    let probe = ui
        .painter()
        .layout_no_wrap("innerpulse".to_owned(), font.clone(), pal.muted);
    let size = Vec2::new(12.0 + 6.0 + probe.size().x, 18.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let fg = if resp.hovered() { pal.text } else { pal.muted };
        let mark = Rect::from_center_size(
            Pos2::new(rect.left() + 6.0, rect.center().y),
            Vec2::splat(12.0),
        );
        brand::paint_mark(ui.painter(), mark, &brand::rest_heights(), pal.mark);
        let galley = ui
            .painter()
            .layout_no_wrap("innerpulse".to_owned(), font, fg);
        ui.painter().galley(
            Pos2::new(rect.left() + 18.0, rect.center().y - galley.size().y / 2.0),
            galley,
            fg,
        );
        if resp.has_focus() {
            focus_ring(ui, pal, rect, 2);
        }
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Link, true, "innerpulse"));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("An innerpulse project · www.innerpulse.net")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_track_cells_and_fill() {
        assert_eq!(track_cells(222.0), 28);
        assert_eq!(track_cells(10_000.0), TRACK_MAX_CELLS);
        assert_eq!(track_cells(0.0), 1);
        assert_eq!(track_cells(f32::NAN), 1);
        assert_eq!(track_fill(0.0, 4), vec![0.0; 4]);
        assert_eq!(track_fill(1.0, 4), vec![1.0; 4]);
        assert_eq!(track_fill(0.5, 4), vec![1.0, 1.0, 0.0, 0.0]);
        let f = track_fill(0.3, 10);
        assert_eq!(f[0..2], [1.0, 1.0]);
        assert!((f[2] - 1.0).abs() < 1e-5 || f[2] < 1.0);
        assert!(f[3..].iter().all(|v| *v < 1e-5));
        assert_eq!(track_fill(f32::NAN, 3), vec![0.0; 3]);
        assert_eq!(track_fill(7.0, 2), vec![1.0; 2]);
        // A tiny fraction still shows in the first cell.
        assert!(track_fill(0.01, 28)[0] > 0.0);
    }

    #[test]
    fn job_states_map_to_chips() {
        assert_eq!(job_status(JobState::Queued), Status::Idle);
        assert_eq!(job_status(JobState::Downloading), Status::Live);
        assert_eq!(job_status(JobState::Done), Status::Ok);
        assert_eq!(job_status(JobState::Failed), Status::Err);
        assert_eq!(job_status(JobState::Paused), Status::Warn);
        for s in [Status::Ok, Status::Warn, Status::Err, Status::Idle] {
            assert!(s.glyph().is_some(), "every status but live carries a glyph");
        }
    }

    #[test]
    fn empty_article_grid_shows_no_data() {
        let (cols, rows) = GRID_SIZE;
        let lit = (0..rows)
            .flat_map(|r| (0..cols).map(move |c| (c, r)))
            .filter(|&(c, r)| grid_lit(c, r))
            .count();
        assert_eq!(lit, 0, "an empty queue has no articles to light");
    }
}
