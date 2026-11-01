//! Bottom docks: Throughput, Details and Log.
//!
//! Each dock is a card under a mono eyebrow. **Throughput** keeps the live trace and the
//! 24 h history on separate bands so two time axes never share one plot, under the speed
//! and what is left to download. **Details** follows the selection: the selected job's
//! facts and per-file progress, or, with nothing selected, the server (connection slots,
//! articles this session) and lifetime totals as stat tiles. **Log** keeps fixed level and
//! time columns so long lines wrap under their own text, with a level filter. Motion (the
//! live marker's pulse, the slot shimmer, a new message's fade) runs only while bytes move
//! and stops under reduced motion.

use eframe::egui::{
    self, epaint, Color32, CornerRadius, FontId, Id, Pos2, Rect, RichText, Sense, Shape, Stroke,
    StrokeKind, Ui, Vec2,
};
use newskite_service::{Job, JobState, LogLevel, LogLine};
use newskite_types::format;

use super::app::{Action, Data, HISTORY_SECS, LIVE_WINDOW_SECS};
use super::fonts;
use super::queue::eta_secs;
use super::theme::Palette;
use super::widgets::{
    card, icon_button, kit_button, paint_icon, section_title, segment_track, spaced, state_badge,
    status_chip, ButtonKind, Icon, Status,
};

/// Width at which the three docks sit side by side (below: one scrolling column).
pub const THREE_COLUMN_MIN: f32 = 900.0;
/// Cells of the stored history strip (24 h / 48 = 30 min each).
const HISTORY_POINTS: usize = 48;
/// Vertical room a dock card takes beyond its content height: the 12 px inner margin on
/// both sides and the hairline.
const CARD_CHROME: f32 = 26.0;
/// Period of the live marker's pulse and the slot shimmer, seconds (the mark's busy beat).
const PULSE_PERIOD: f64 = 1.2;
/// Frame interval while something in the docks moves.
const MOTION_FRAME: std::time::Duration = std::time::Duration::from_millis(40);
/// Height of the 24 h history strip under the live graph, and the shortest live plot.
const HISTORY_STRIP_H: f32 = 10.0;
const MIN_PLOT_H: f32 = 24.0;
/// Height of a stat tile (figure over label) and of its one-line inline form.
const TILE_H: f32 = 38.0;
const INLINE_TILE_H: f32 = 18.0;
/// Height of a connection slot cell and the gap between cells.
const SLOT_H: f32 = 9.0;
const SLOT_GAP: f32 = 2.0;
/// Narrowest slot cell; more connections than fit become proportional cells.
const SLOT_MIN_W: f32 = 4.0;
/// Widths of the level and time columns of the message log.
const LEVEL_COL_W: f32 = 14.0;
const TIME_COL_W: f32 = 58.0;
/// Seconds a new message's highlight takes to fade.
const MESSAGE_FADE_SECS: f32 = 1.6;

/// Dock arrangement for a given window width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockLayout {
    /// Three side by side.
    Row,
    /// One column (scrolls).
    Stack,
}

/// Picks the arrangement for `width` (logical pixels).
pub fn layout_for(width: f32) -> DockLayout {
    if width >= THREE_COLUMN_MIN {
        DockLayout::Row
    } else {
        DockLayout::Stack
    }
}

/// Which log lines the Log dock shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFilter {
    /// Every line.
    #[default]
    All,
    /// Warnings and errors.
    Problems,
    /// Errors only.
    Errors,
}

impl LogFilter {
    const ALL: [LogFilter; 3] = [LogFilter::All, LogFilter::Problems, LogFilter::Errors];

    /// Whether a line of `level` is shown.
    pub fn admits(self, level: LogLevel) -> bool {
        match self {
            LogFilter::All => true,
            LogFilter::Problems => level != LogLevel::Info,
            LogFilter::Errors => level == LogLevel::Error,
        }
    }

    fn label(self) -> &'static str {
        match self {
            LogFilter::All => "All",
            LogFilter::Problems => "Warnings",
            LogFilter::Errors => "Errors",
        }
    }
}

/// Dock UI state.
#[derive(Default)]
pub struct DockState {
    /// The Log dock's level filter.
    pub log_filter: LogFilter,
}

/// Mean of each of at most `max_points` equal chunks of `values` (keeps the shape of a
/// long history while bounding the drawing cost). Always finite.
pub fn downsample(values: &[u64], max_points: usize) -> Vec<f32> {
    if values.is_empty() || max_points == 0 {
        return Vec::new();
    }
    if values.len() <= max_points {
        return values.iter().map(|&v| v as f32).collect();
    }
    let chunk = values.len().div_ceil(max_points);
    values
        .chunks(chunk)
        .map(|c| (c.iter().map(|&v| v as f64).sum::<f64>() / c.len() as f64) as f32)
        .collect()
}

/// Mean throughput (bytes/s) of each of `buckets` equal time slices of the `window_secs`
/// ending at `now`. Samples hold the bytes of one second, keyed by time; seconds without
/// a sample were idle and count as zero, so a short burst stays short and gaps stay
/// visible. Samples outside the window are ignored.
pub fn history_buckets(
    samples: &[(i64, u64)],
    now: i64,
    window_secs: i64,
    buckets: usize,
) -> Vec<f32> {
    if buckets == 0 || window_secs <= 0 {
        return Vec::new();
    }
    let start = now - window_secs;
    let mut sums = vec![0.0_f64; buckets];
    for &(ts, bytes) in samples {
        if ts < start || ts > now {
            continue;
        }
        let i = (((ts - start) as i128 * buckets as i128) / window_secs as i128) as usize;
        sums[i.min(buckets - 1)] += bytes as f64;
    }
    let bucket_secs = window_secs as f64 / buckets as f64;
    sums.into_iter().map(|s| (s / bucket_secs) as f32).collect()
}

fn y_for(v: f32, rect: Rect, y_max: f32) -> f32 {
    let max = if y_max.is_finite() && y_max > 0.0 {
        y_max
    } else {
        1.0
    };
    let f = if v.is_finite() {
        (v / max).clamp(0.0, 1.0)
    } else {
        0.0
    };
    // Keep a 1.5 px margin so a zero trace is still visible above the bottom edge.
    let top = rect.top() + 2.0;
    let bottom = rect.bottom() - 1.5;
    bottom - f * (bottom - top).max(0.0)
}

/// Evenly spaced points spanning `rect`'s width. One value draws a flat line across the
/// whole width (outline: a single stored sample must still draw a visible trace); no
/// values draw nothing.
pub fn index_points(values: &[f32], rect: Rect, y_max: f32) -> Vec<Pos2> {
    match values.len() {
        0 => Vec::new(),
        1 => {
            let y = y_for(values[0], rect, y_max);
            vec![Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)]
        }
        n => values
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let x = rect.left() + rect.width() * (i as f32 / (n - 1) as f32);
                Pos2::new(x, y_for(v, rect, y_max))
            })
            .collect(),
    }
}

/// Points of a live series on a time axis ending at `now` (right edge) and spanning
/// `window_secs`. Older samples are dropped; a single sample becomes a short flat
/// segment running to the right edge so it is visible.
pub fn time_points(
    samples: &[(i64, u64)],
    now: i64,
    window_secs: i64,
    rect: Rect,
    y_max: f32,
) -> Vec<Pos2> {
    let window = window_secs.max(1) as f32;
    let start = now - window_secs.max(1);
    let pts: Vec<Pos2> = samples
        .iter()
        .filter(|(ts, _)| *ts >= start)
        .map(|&(ts, v)| {
            let age = (now - ts).max(0) as f32;
            let x = rect.right() - rect.width() * (age / window).clamp(0.0, 1.0);
            Pos2::new(x, y_for(v as f32, rect, y_max))
        })
        .collect();
    if pts.len() == 1 {
        let p = pts[0];
        let left = p.x.min(rect.right() - 6.0).max(rect.left());
        return vec![Pos2::new(left, p.y), Pos2::new(rect.right(), p.y)];
    }
    pts
}

#[derive(Clone, Copy)]
enum Dock {
    Throughput,
    Details,
    Log,
}

impl Dock {
    const ALL: [Dock; 3] = [Dock::Throughput, Dock::Details, Dock::Log];
}

/// What moves this frame: the clock phase of the busy beat (0…1) and whether anything
/// animates at all (bytes are moving and motion is not reduced).
#[derive(Clone, Copy)]
struct Motion {
    phase: f32,
    moving: bool,
    /// Motion is allowed at all (not reduced).
    animate: bool,
}

impl Motion {
    fn new(ui: &Ui, data: &Data, reduced_motion: bool) -> Self {
        let now = ui.input(|i| i.time);
        let moving = !reduced_motion && data.speed() > 0 && now.is_finite();
        let phase = if moving {
            ((now / PULSE_PERIOD).fract()) as f32
        } else {
            0.0
        };
        Self {
            phase,
            moving,
            animate: !reduced_motion,
        }
    }
}

/// Opacity of a history cell holding `v` bytes/s when the busiest cell holds `peak`:
/// `None` for an idle cell (drawn as an empty well), otherwise 0.3…1 with the busiest at 1.
pub fn heat_alpha(v: f32, peak: f32) -> Option<f32> {
    if !v.is_finite() || !peak.is_finite() || v <= 0.0 || peak <= 0.0 {
        return None;
    }
    Some(0.3 + 0.7 * (v / peak).clamp(0.0, 1.0))
}

/// Phase of a pulse (0…1) → opacity of its ring: bright as it starts, gone as it ends.
pub fn pulse_alpha(phase: f32) -> f32 {
    if !phase.is_finite() {
        return 0.0;
    }
    (1.0 - phase.clamp(0.0, 1.0)).powi(2) * 0.6
}

/// How many slot cells a row of `max` connections gets in `width` px, and how many of
/// them are lit for `effective` connections. Every configured connection gets its own
/// cell while cells stay at least [`SLOT_MIN_W`] wide; past that the row keeps the
/// proportion. Never zero cells; never more lit than cells.
pub fn slot_cells(max: usize, effective: usize, width: f32) -> (usize, usize) {
    let max = max.max(1);
    let fit = if width.is_finite() {
        (((width + SLOT_GAP) / (SLOT_MIN_W + SLOT_GAP)).floor() as usize).max(1)
    } else {
        1
    };
    let cells = max.min(fit);
    let lit = if cells == max {
        effective.min(max)
    } else {
        ((effective.min(max) as f64 * cells as f64) / max as f64).round() as usize
    };
    (cells, lit.min(cells))
}

/// Opacity of lit slot `i` of `n` while bytes move: a soft wave traveling along the row.
pub fn slot_shimmer(i: usize, n: usize, phase: f32) -> f32 {
    let offset = if n > 1 { i as f32 / n as f32 } else { 0.0 };
    let wave = ((phase - offset) * std::f32::consts::TAU).sin();
    0.72 + 0.28 * (0.5 + 0.5 * wave)
}

/// Everything a dock needs to draw.
struct Ctx<'a> {
    pal: &'a Palette,
    data: &'a Data,
    selected: Option<&'a Job>,
    motion: Motion,
}

fn dock(
    ui: &mut Ui,
    c: &Ctx<'_>,
    which: Dock,
    height: f32,
    state: &mut DockState,
    actions: &mut Vec<Action>,
) {
    let pal = c.pal;
    card(pal).show(ui, |ui| {
        // An exact height: every card of a row lines up, and content that does not fit
        // scrolls inside its card instead of pushing the next row down.
        ui.set_min_height(height);
        ui.set_max_height(height.max(60.0));
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.set_min_height(18.0);
            let title = match which {
                Dock::Throughput => "Throughput",
                Dock::Details if c.selected.is_some() => "Details",
                Dock::Details => "Server",
                Dock::Log => "Log",
            };
            section_title(ui, pal, title);
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| match which {
                    Dock::Log => log_filter_chips(ui, pal, &mut state.log_filter),
                    Dock::Details if c.selected.is_some() => {
                        if icon_button(ui, pal, Icon::Close, pal.muted, "Clear the selection (Esc)")
                            .clicked()
                        {
                            actions.push(Action::Select(None));
                        }
                    }
                    _ => {}
                },
            );
        });
        ui.add_space(6.0);
        match which {
            Dock::Throughput => graph(ui, pal, c.data, c.motion),
            Dock::Log => messages(ui, pal, &c.data.logs, state.log_filter, c.motion.animate),
            Dock::Details => {
                egui::ScrollArea::vertical()
                    .id_salt("dock-details")
                    .auto_shrink([false, false])
                    .max_height(ui.available_height().max(20.0))
                    .show(ui, |ui| match c.selected {
                        Some(job) => details(ui, pal, c.data, job, actions),
                        None => activity(ui, pal, c.data, c.motion, actions),
                    });
            }
        }
    });
}

/// Draws the three docks in the arrangement that fits the available width.
pub fn show(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    selected: Option<i64>,
    state: &mut DockState,
    reduced_motion: bool,
    actions: &mut Vec<Action>,
) {
    let motion = Motion::new(ui, data, reduced_motion);
    if motion.moving {
        ui.ctx().request_repaint_after(MOTION_FRAME);
    }
    let c = Ctx {
        pal,
        data,
        selected: selected.and_then(|id| data.jobs.iter().find(|j| j.id == id)),
        motion,
    };
    let width = ui.ctx().content_rect().width();
    match layout_for(width) {
        DockLayout::Row => {
            let h = (ui.available_height() - CARD_CHROME).max(80.0);
            ui.columns(3, |cols| {
                for (col, which) in cols.iter_mut().zip(Dock::ALL) {
                    dock(col, &c, which, h, state, actions);
                }
            });
        }
        DockLayout::Stack => {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for which in Dock::ALL {
                        dock(ui, &c, which, 150.0, state, actions);
                    }
                });
        }
    }
}

fn log_filter_chips(ui: &mut Ui, pal: &Palette, filter: &mut LogFilter) {
    ui.spacing_mut().item_spacing.x = 2.0;
    // Right to left: the last chip is drawn first.
    for f in LogFilter::ALL.iter().rev() {
        let on = *filter == *f;
        let text = RichText::new(f.label())
            .font(fonts::mono_id(10.5))
            .color(if on { pal.text } else { pal.faint });
        let resp = ui.add(egui::Button::selectable(on, text).corner_radius(CornerRadius::same(4)));
        if resp.clicked() {
            *filter = *f;
        }
    }
}

/// A reading for a stat tile.
struct Stat {
    value: String,
    label: &'static str,
    color: Color32,
}

impl Stat {
    fn new(value: impl Into<String>, label: &'static str, color: Color32) -> Self {
        Self {
            value: value.into(),
            label,
            color,
        }
    }
}

/// One stat tile: the figure in the 17 px mono semibold over its label as a small mono
/// eyebrow; `inline` lays the two side by side on one 18 px line for the short cards of
/// the 2×2 grid. Two accessible nodes: `"<label>: <value>"` on the figure and the label
/// itself.
fn tile(ui: &mut Ui, pal: &Palette, stat: &Stat, width: f32, inline: bool) {
    let height = if inline { INLINE_TILE_H } else { TILE_H };
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let value_font = FontId::new(if inline { 13.5 } else { 17.0 }, fonts::mono_semibold());
    let label_job = spaced(
        &stat.label.to_uppercase(),
        fonts::mono_id(9.5),
        pal.faint,
        1.0,
    );
    let painter = ui.painter_at(rect);
    let value_galley = painter.layout_no_wrap(stat.value.clone(), value_font, stat.color);
    let label_galley = painter.layout_job(label_job);
    let (value_rect, label_rect) = if inline {
        let v = Rect::from_min_size(rect.min, Vec2::new(value_galley.size().x, height));
        let l = Rect::from_min_max(Pos2::new(v.right() + 6.0, rect.top()), rect.right_bottom());
        (v, l)
    } else {
        (
            Rect::from_min_size(rect.min, Vec2::new(width, 22.0)),
            Rect::from_min_size(
                Pos2::new(rect.left(), rect.bottom() - 14.0),
                Vec2::new(width, 14.0),
            ),
        )
    };
    if ui.is_rect_visible(rect) {
        painter.galley(
            Pos2::new(
                value_rect.left(),
                value_rect.center().y - value_galley.size().y / 2.0,
            ),
            value_galley,
            stat.color,
        );
        let label_pos = if inline {
            // Baselines align: the eyebrow sits a hair above the figure's baseline.
            Pos2::new(
                label_rect.left(),
                value_rect.center().y - label_galley.size().y / 2.0 + 1.0,
            )
        } else {
            label_rect.left_top()
        };
        painter.galley(label_pos, label_galley, pal.faint);
    }
    let value_resp = ui.interact(
        value_rect,
        ui.id().with(("tile-value", stat.label)),
        Sense::hover(),
    );
    value_resp.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Label,
            true,
            format!("{}: {}", stat.label, stat.value),
        )
    });
    let label_resp = ui.interact(
        label_rect,
        ui.id().with(("tile-label", stat.label)),
        Sense::hover(),
    );
    label_resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, stat.label));
}

/// Stat tiles in rows of `per_row`, sharing the width evenly; inline tiles when
/// `compact` (the 2×2 grid, where height is scarce).
fn tiles(ui: &mut Ui, pal: &Palette, stats: &[Stat], per_row: usize, compact: bool) {
    let per_row = per_row.max(1);
    let gap = 10.0;
    let width = ((ui.available_width() - gap * (per_row as f32 - 1.0)) / per_row as f32).max(40.0);
    for row in stats.chunks(per_row) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for stat in row {
                tile(ui, pal, stat, width, compact);
            }
        });
        ui.add_space(if compact { 2.0 } else { 4.0 });
    }
}

/// A small mono eyebrow with a figure at the right end of the same line.
fn eyebrow_row(ui: &mut Ui, pal: &Palette, label: &str, right: &str, right_color: Color32) {
    ui.horizontal(|ui| {
        let job = spaced(&label.to_uppercase(), fonts::mono_id(9.5), pal.faint, 1.0);
        ui.add(egui::Label::new(job).selectable(false));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(right)
                        .font(fonts::mono_id(11.0))
                        .color(right_color),
                )
                .selectable(false),
            );
        });
    });
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Fills under a polyline with a vertical gradient from `top` at the line to `clear`
/// (the palette's transparent) at `bottom`: a mesh with per-vertex colors, since egui
/// has no gradient brush.
fn gradient_fill(points: &[Pos2], bottom: f32, top: Color32, clear: Color32) -> Shape {
    let mut mesh = epaint::Mesh::default();
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        let i = mesh.vertices.len() as u32;
        mesh.colored_vertex(a, top);
        mesh.colored_vertex(b, top);
        mesh.colored_vertex(Pos2::new(b.x, bottom), clear);
        mesh.colored_vertex(Pos2::new(a.x, bottom), clear);
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i, i + 2, i + 3);
    }
    Shape::mesh(mesh)
}

fn graph(ui: &mut Ui, pal: &Palette, data: &Data, motion: Motion) {
    // Every gap below is explicit, so the bands fit the card's budget exactly.
    ui.spacing_mut().item_spacing.y = 0.0;
    let speed = data.speed();
    let left = data.bytes_left();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format::speed(speed))
                .font(FontId::new(17.0, fonts::mono_semibold()))
                .color(pal.text),
        );
        if speed > 0 {
            // A live dot beside the figure; it breathes with the busy beat.
            let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
            let alpha = if motion.moving {
                0.55 + 0.45 * (motion.phase * std::f32::consts::TAU).cos().abs()
            } else {
                1.0
            };
            ui.painter()
                .circle_filled(r.center(), 3.0, Palette::fade(pal.series, alpha));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let text = match eta_secs(left, speed) {
                _ if left == 0 => "Nothing left".to_owned(),
                Some(eta) if data.status.running => format!(
                    "{} left · {}",
                    format::bytes(left),
                    format::duration_secs(eta)
                ),
                _ => format!("{} left", format::bytes(left)),
            };
            ui.label(
                RichText::new(text)
                    .font(fonts::mono_id(11.0))
                    .color(pal.muted),
            );
        });
    });
    ui.add_space(2.0);

    let now = now_secs();
    let history = if data.history.is_empty() {
        Vec::new()
    } else {
        let raw: Vec<(i64, u64)> = data.history.iter().map(|s| (s.ts, s.bytes_down)).collect();
        history_buckets(&raw, now, HISTORY_SECS as i64, HISTORY_POINTS)
    };
    let live: Vec<(i64, u64)> = data.live.iter().copied().collect();
    let nothing = history.is_empty() && live.is_empty();
    let live_peak = live.iter().map(|&(_, v)| v as f32).fold(0.0_f32, f32::max);
    let history_peak = history.iter().copied().fold(0.0_f32, f32::max);
    let peak = live_peak.max(history_peak);

    // Live plot above, 24 h strip below (when there is history), then a caption when
    // the card is tall enough for one (the tooltip repeats it).
    let strip_h = if history.is_empty() {
        0.0
    } else {
        HISTORY_STRIP_H + 4.0
    };
    // A 2 px safety margin: the card clips whatever its budget does not cover.
    let budget = ui.available_height() - 2.0;
    let caption_h = if budget - strip_h >= MIN_PLOT_H + 18.0 {
        18.0
    } else {
        0.0
    };
    let h = (budget - caption_h - strip_h).clamp(MIN_PLOT_H, 170.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(4), pal.sunken);
    let inner = rect.shrink2(Vec2::new(4.0, 0.0));
    let y_max = if live_peak > 0.0 {
        live_peak * 1.12
    } else {
        1.0
    };

    if nothing {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No throughput history yet",
            FontId::proportional(12.5),
            pal.muted,
        );
    } else {
        let lp = time_points(&live, now, LIVE_WINDOW_SECS as i64, inner, y_max);
        if lp.len() >= 2 {
            for f in [0.25, 0.5, 0.75] {
                let y = rect.top() + rect.height() * f;
                painter.line_segment(
                    [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
                    Stroke::new(1.0, pal.border),
                );
            }
            painter.add(gradient_fill(
                &lp,
                rect.bottom(),
                Palette::fade(pal.series, 0.38),
                pal.transparent,
            ));
            painter.add(Shape::line(lp.clone(), Stroke::new(1.8, pal.series)));
            let end = lp[lp.len() - 1];
            if motion.moving {
                let ring = 3.0 + 7.0 * motion.phase;
                painter.circle_stroke(
                    end,
                    ring,
                    Stroke::new(1.5, Palette::fade(pal.series, pulse_alpha(motion.phase))),
                );
            }
            painter.circle_filled(end, 3.0, pal.series);
            painter.circle_filled(end, 1.4, pal.sunken);
            // Axis corner labels: the scale top-left, the time span along the bottom.
            let corner = fonts::mono_id(9.5);
            painter.text(
                Pos2::new(rect.left() + 4.0, rect.top() + 3.0),
                egui::Align2::LEFT_TOP,
                format::speed(y_max as u64),
                corner.clone(),
                pal.faint,
            );
            painter.text(
                Pos2::new(rect.right() - 4.0, rect.bottom() - 2.0),
                egui::Align2::RIGHT_BOTTOM,
                "now",
                corner.clone(),
                pal.faint,
            );
            painter.text(
                Pos2::new(rect.left() + 4.0, rect.bottom() - 2.0),
                egui::Align2::LEFT_BOTTOM,
                format!("−{} min", LIVE_WINDOW_SECS / 60),
                corner,
                pal.faint,
            );
        } else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!(
                    "Idle · nothing moved in the last {} min",
                    LIVE_WINDOW_SECS / 60
                ),
                FontId::proportional(12.0),
                pal.faint,
            );
        }
    }
    let summary = if nothing {
        "No throughput history yet".to_owned()
    } else {
        format!(
            "Throughput graph, {} stored samples, {} live",
            data.history.len(),
            live.len()
        )
    };
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &summary));
    resp.on_hover_text(format!(
        "Live: last {} min, one point per second\nStrip: last 24 h in {}-minute cells, brighter = faster, idle cells empty ({} samples)\nPeak: {}",
        LIVE_WINDOW_SECS / 60,
        HISTORY_SECS / 60 / HISTORY_POINTS as u64,
        data.history.len(),
        format::speed(peak as u64)
    ));

    if !history.is_empty() {
        ui.add_space(4.0);
        let (strip, sresp) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), HISTORY_STRIP_H),
            Sense::hover(),
        );
        // The segment-cell motif: one cell per 30 minutes, lit in the live color as
        // bright as that half hour was fast; idle cells stay empty wells.
        let painter = ui.painter_at(strip);
        let n = history.len();
        let gap = 2.0;
        let cell_w = ((strip.width() - gap * (n as f32 - 1.0)) / n as f32).max(1.0);
        for (i, &v) in history.iter().enumerate() {
            let x = strip.left() + i as f32 * (cell_w + gap);
            let cell =
                Rect::from_min_size(Pos2::new(x, strip.top()), Vec2::new(cell_w, strip.height()));
            let color = match heat_alpha(v, history_peak) {
                Some(a) => Palette::fade(pal.series, a),
                None => pal.well,
            };
            painter.rect_filled(cell, CornerRadius::same(1), color);
        }
        sresp.on_hover_text(format!(
            "Last 24 h · peak {}",
            format::speed(history_peak as u64)
        ));
    }

    if !nothing && caption_h > 0.0 {
        let caption = if history.is_empty() {
            format!(
                "Peak {}  ·  live, last {} min",
                format::speed(peak as u64),
                LIVE_WINDOW_SECS / 60
            )
        } else {
            format!("Peak {}  ·  strip = 24 h", format::speed(peak as u64))
        };
        ui.add_space(4.0);
        ui.label(
            RichText::new(caption)
                .font(fonts::mono_id(10.5))
                .color(pal.faint),
        );
    }
}

/// The connection slot row: one cell per configured connection (or a proportional row
/// when they do not fit), the effective ones lit. While bytes move the lit cells shimmer
/// in the live series color; idle, they rest in the muted tone.
fn slots(ui: &mut Ui, pal: &Palette, max: usize, effective: usize, active: bool, motion: Motion) {
    let width = ui.available_width();
    let (cells, lit) = slot_cells(max, effective, width);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, SLOT_H), Sense::hover());
    if ui.is_rect_visible(rect) {
        let cell_w = (rect.width() - SLOT_GAP * (cells as f32 - 1.0)) / cells as f32;
        let painter = ui.painter();
        let r = CornerRadius::same(1);
        for i in 0..cells {
            let x = rect.left() + i as f32 * (cell_w + SLOT_GAP);
            let cell = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(cell_w, SLOT_H));
            let color = if i >= lit {
                pal.well
            } else if motion.moving {
                Palette::fade(pal.series, slot_shimmer(i, lit, motion.phase))
            } else if active {
                pal.series
            } else {
                pal.muted
            };
            painter.rect_filled(cell, r, color);
        }
    }
    resp.on_hover_text(format!(
        "{effective} of {max} connections per run{}",
        if cells < max {
            " (proportional row)"
        } else {
            ""
        }
    ));
}

fn activity(ui: &mut Ui, pal: &Palette, data: &Data, motion: Motion, actions: &mut Vec<Action>) {
    let s = &data.settings;
    match s.first_usable_server() {
        None => {
            ui.label(RichText::new("No server configured.").color(pal.warning));
            ui.label(
                RichText::new("Add an NNTP server to start downloading.")
                    .small()
                    .color(pal.muted),
            );
            ui.add_space(4.0);
            if kit_button(
                ui,
                pal,
                ButtonKind::Outline,
                Some(Icon::Gear),
                "Open settings…",
                true,
            )
            .clicked()
            {
                actions.push(Action::OpenSettings);
            }
            ui.add_space(10.0);
            account(ui, pal, data);
        }
        Some(server) => {
            let others = s.usable_servers().len().saturating_sub(1);
            let max = s.max_connections.max(1) as usize;
            let effective = s.effective_connections();
            let active = data.status.running && !data.status.paused;
            let identity = |ui: &mut Ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if server.tls {
                    status_chip(ui, pal, Status::Ok, "TLS");
                } else {
                    status_chip(ui, pal, Status::Warn, "Plain");
                }
                ui.label(
                    RichText::new(if server.username.is_empty() {
                        "(none)".to_owned()
                    } else {
                        server.username.clone()
                    })
                    .font(fonts::mono_id(11.5))
                    .color(pal.muted),
                );
                if others > 0 {
                    ui.label(
                        RichText::new(format!(
                            "+{others} fail-over server{}",
                            if others == 1 { "" } else { "s" }
                        ))
                        .font(fonts::mono_id(11.0))
                        .color(pal.faint),
                    );
                }
            };
            let address = RichText::new(server.address())
                .font(fonts::mono_id(12.5))
                .color(pal.text);
            {
                ui.horizontal_wrapped(|ui| {
                    ui.label(address);
                    ui.add_space(4.0);
                    identity(ui);
                });
                ui.add_space(6.0);
                eyebrow_row(
                    ui,
                    pal,
                    "Connections",
                    &format!("{effective} / {max}"),
                    pal.text,
                );
                ui.add_space(3.0);
                slots(ui, pal, max, effective, active, motion);
                ui.add_space(10.0);
                account(ui, pal, data);
            }
        }
    }
}

fn clock(ts: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => {
            t.format("%H:%M:%S").to_string()
        }
        chrono::LocalResult::None => "--:--:--".to_owned(),
    }
}

fn level_color(pal: &Palette, level: LogLevel) -> Color32 {
    match level {
        LogLevel::Info => pal.text,
        LogLevel::Warn => pal.warning,
        LogLevel::Error => pal.negative,
    }
}

/// Messages, newest first, in three columns: a level mark, the clock, the text (which
/// wraps under itself, never under the clock). A line seen for the first time after the
/// dock's first frame carries an accent band that fades out.
fn messages(ui: &mut Ui, pal: &Palette, logs: &[LogLine], filter: LogFilter, animate: bool) {
    if logs.is_empty() {
        ui.label(RichText::new("No messages yet").color(pal.muted));
        return;
    }
    let shown: Vec<&LogLine> = logs.iter().filter(|l| filter.admits(l.level)).collect();
    if shown.is_empty() {
        ui.label(
            RichText::new(match filter {
                LogFilter::Errors => "No errors",
                _ => "No warnings or errors",
            })
            .color(pal.muted),
        );
        return;
    }
    let ctx = ui.ctx().clone();
    let seen_key = Id::new("messages-seen-once");
    let first_frame = !ctx.data(|d| d.get_temp::<bool>(seen_key)).unwrap_or(false);
    egui::ScrollArea::vertical()
        .id_salt("messages")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            let text_w = (ui.available_width() - LEVEL_COL_W - TIME_COL_W - 10.0).max(40.0);
            for line in shown {
                let id = Id::new(("logline", line.ts, &line.text));
                let seen = ctx.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
                if !seen {
                    ctx.data_mut(|d| d.insert_temp(id, true));
                }
                // Lines present on the first frame were not "new"; later ones fade in.
                let fade = if animate && !first_frame {
                    1.0 - ctx.animate_bool_with_time(id.with("fade"), seen, MESSAGE_FADE_SECS)
                } else {
                    0.0
                };
                let row = ui
                    .horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let (lr, _) =
                            ui.allocate_exact_size(Vec2::new(LEVEL_COL_W, 16.0), Sense::hover());
                        let mark = Rect::from_center_size(lr.center(), Vec2::splat(11.0));
                        match line.level {
                            LogLevel::Info => {
                                ui.painter().circle_filled(mark.center(), 2.0, pal.faint);
                            }
                            LogLevel::Warn => {
                                paint_icon(ui.painter(), mark, Icon::StatusWarn, pal.warning)
                            }
                            LogLevel::Error => {
                                paint_icon(ui.painter(), mark, Icon::StatusErr, pal.negative)
                            }
                        }
                        let (tr, _) =
                            ui.allocate_exact_size(Vec2::new(TIME_COL_W, 16.0), Sense::hover());
                        ui.painter().text(
                            tr.left_center(),
                            egui::Align2::LEFT_CENTER,
                            clock(line.ts),
                            fonts::mono_id(11.0),
                            pal.faint,
                        );
                        ui.scope(|ui| {
                            ui.set_max_width(text_w);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&line.text)
                                        .font(fonts::mono_id(11.5))
                                        .color(level_color(pal, line.level)),
                                )
                                .wrap(),
                            );
                        });
                    })
                    .response
                    .rect;
                if fade > 0.0 {
                    ui.painter().rect_filled(
                        row.expand2(Vec2::new(2.0, 1.0)),
                        CornerRadius::same(3),
                        Palette::fade(pal.accent_soft, fade),
                    );
                    ui.painter().rect_stroke(
                        row.expand2(Vec2::new(2.0, 1.0)),
                        CornerRadius::same(3),
                        Stroke::new(1.0, Palette::fade(pal.accent_fg, fade * 0.6)),
                        StrokeKind::Outside,
                    );
                }
            }
        });
    if first_frame {
        ctx.data_mut(|d| d.insert_temp(seen_key, true));
    }
}

/// One row of stat tiles: this session's articles (real counts from engine events,
/// never estimates) and the lifetime download total. Job counts live on the History tab.
fn account(ui: &mut Ui, pal: &Palette, data: &Data) {
    let stats = [
        Stat::new(data.articles_fetched.to_string(), "Articles", pal.text),
        Stat::new(
            data.articles_missing.to_string(),
            "Missing",
            if data.articles_missing > 0 {
                pal.warning
            } else {
                pal.text
            },
        ),
        Stat::new(
            format::bytes(data.totals.downloaded_bytes),
            "Downloaded",
            pal.text,
        ),
    ];
    tiles(ui, pal, &stats, 3, false);
}

/// The selected job: its facts and per-file progress.
fn details(ui: &mut Ui, pal: &Palette, data: &Data, job: &Job, actions: &mut Vec<Action>) {
    // Nothing in here may widen the card (long paths and names truncate instead).
    ui.set_max_width(ui.available_width());
    ui.horizontal(|ui| {
        state_badge(ui, pal, job.state);
        ui.add(egui::Label::new(RichText::new(&job.name).strong().color(pal.text)).truncate())
            .on_hover_text(&job.name);
    });
    ui.add_space(6.0);
    let done = data.shown_done(job);
    // Key/value rows; each value truncates to the card (full text on hover).
    let fact = |ui: &mut Ui, key: &str, value: RichText, tip: Option<String>| {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::new(64.0, 16.0), Sense::hover());
            ui.painter().text(
                r.left_center(),
                egui::Align2::LEFT_CENTER,
                key,
                FontId::proportional(11.5),
                pal.muted,
            );
            let resp = ui.add(egui::Label::new(value).truncate().sense(Sense::click()));
            match tip {
                Some(t) => resp.on_hover_text(t),
                None => resp,
            }
        })
        .inner
    };
    fact(
        ui,
        "Progress",
        RichText::new(format!(
            "{} of {} ({}%)",
            format::bytes(done),
            format::bytes(job.bytes_total),
            format::percent(done, job.bytes_total)
        ))
        .font(fonts::mono_id(11.5))
        .color(pal.text),
        None,
    );
    if job.state == JobState::Downloading {
        if let Some(eta) = eta_secs(job.bytes_total.saturating_sub(done), data.speed()) {
            fact(
                ui,
                "Time left",
                RichText::new(format!(
                    "{} at {}",
                    format::duration_secs(eta),
                    format::speed(data.speed())
                ))
                .font(fonts::mono_id(11.5))
                .color(pal.accent_fg),
                None,
            );
        }
    }
    let folder = job.dest_dir.display().to_string();
    let open = fact(
        ui,
        "Folder",
        RichText::new(&folder).small().color(pal.accent_fg),
        Some(format!("{folder}\nClick to open in the file manager")),
    );
    if open
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        actions.push(Action::OpenJobFolder(job.id));
    }
    let nzb = job
        .source_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    fact(
        ui,
        "NZB",
        RichText::new(nzb).small().color(pal.text),
        Some(job.source_path.display().to_string()),
    );
    if job.password.is_some() {
        fact(
            ui,
            "Password",
            RichText::new("set").small().color(pal.text),
            None,
        );
    }
    if let (JobState::Failed, Some(e)) = (job.state, &job.error) {
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(e).small().color(pal.negative)).wrap());
    }
    ui.add_space(8.0);
    let files = &data.selected_files;
    let finished = files.iter().filter(|f| f.state == "done").count();
    eyebrow_row(
        ui,
        pal,
        "Files",
        &if files.is_empty() {
            String::new()
        } else {
            format!("{finished} of {} done", files.len())
        },
        pal.muted,
    );
    ui.add_space(4.0);
    if files.is_empty() {
        let text = match job.state {
            JobState::Queued | JobState::Paused => {
                "Per-file progress appears once the download starts."
            }
            _ => "No per-file progress was recorded.",
        };
        ui.label(RichText::new(text).small().color(pal.muted));
        return;
    }
    for f in files {
        let color = match f.state.as_str() {
            "done" => pal.positive,
            "failed" => pal.negative,
            _ => pal.accent,
        };
        ui.horizontal(|ui| {
            let right = format!(
                "{} / {}",
                format::bytes(f.bytes_done),
                format::bytes(f.bytes_total)
            );
            let name_w = (ui.available_width() - 128.0).max(40.0);
            ui.scope(|ui| {
                ui.set_width(name_w);
                ui.add(
                    egui::Label::new(
                        RichText::new(&f.name)
                            .font(fonts::mono_id(11.0))
                            .color(pal.text),
                    )
                    .truncate(),
                )
                .on_hover_text(format!("{} · {}", f.name, f.state));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(right)
                        .font(fonts::mono_id(10.5))
                        .color(pal.muted),
                );
            });
        });
        segment_track(
            ui,
            pal,
            format::fraction(f.bytes_done, f.bytes_total),
            color,
            ui.available_width(),
        );
        ui.add_space(5.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect::from_min_max(Pos2::new(10.0, 20.0), Pos2::new(110.0, 70.0))
    }

    #[test]
    fn no_samples_draw_nothing() {
        assert!(index_points(&[], rect(), 10.0).is_empty());
        assert!(time_points(&[], 1000, 600, rect(), 10.0).is_empty());
        assert!(downsample(&[], 10).is_empty());
    }

    #[test]
    fn one_sample_draws_a_visible_flat_trace() {
        let r = rect();
        let pts = index_points(&[5.0], r, 10.0);
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].x, r.left());
        assert_eq!(pts[1].x, r.right());
        assert_eq!(pts[0].y, pts[1].y);
        assert!(pts[0].y > r.top() && pts[0].y < r.bottom());

        // A single zero sample (y_max 0) still sits inside the rect, not on/below the edge.
        let z = index_points(&[0.0], r, 0.0);
        assert!(z.iter().all(|p| p.y < r.bottom() && p.y.is_finite()));

        let live = time_points(&[(1000, 7)], 1000, 600, r, 10.0);
        assert_eq!(live.len(), 2);
        assert!(live[1].x - live[0].x >= 6.0 - f32::EPSILON);
        assert_eq!(live[0].y, live[1].y);
        assert_eq!(live[1].x, r.right());
    }

    #[test]
    fn geometry_stays_finite_and_inside() {
        let r = rect();
        for y_max in [0.0, -1.0, f32::NAN, f32::INFINITY, 1.0] {
            for p in index_points(&[0.0, f32::NAN, 1e30, 3.0], r, y_max) {
                assert!(p.x.is_finite() && p.y.is_finite());
                assert!(p.x >= r.left() && p.x <= r.right());
                assert!(p.y >= r.top() && p.y <= r.bottom());
            }
        }
        let pts = index_points(&[0.0, 10.0], r, 10.0);
        assert_eq!(pts[0].x, r.left());
        assert_eq!(pts[1].x, r.right());
        assert!(pts[1].y < pts[0].y, "larger values are higher");
    }

    #[test]
    fn live_series_drops_old_samples_and_maps_time() {
        let r = rect();
        let s = [(100, 1), (700, 2), (1000, 3)];
        let pts = time_points(&s, 1000, 600, r, 3.0);
        assert_eq!(pts.len(), 2, "sample older than the window dropped");
        assert!((pts[0].x - (r.right() - r.width() * 0.5)).abs() < 1e-3);
        assert_eq!(pts[1].x, r.right());
    }

    #[test]
    fn downsampling_bounds_points_and_keeps_mean() {
        let v: Vec<u64> = (0..1000).collect();
        let d = downsample(&v, 100);
        assert!(d.len() <= 100);
        assert!((d[0] - 4.5).abs() < 1e-3);
        assert_eq!(downsample(&[1, 2, 3], 10), vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn history_is_bucketed_on_a_time_axis() {
        // A 2-minute burst at the end of a 24 h window fills the last bucket only.
        let now = 100_000;
        let burst: Vec<(i64, u64)> = (0..120).map(|i| (now - 119 + i, 1_000)).collect();
        let b = history_buckets(&burst, now, 86_400, 160);
        assert_eq!(b.len(), 160);
        assert!(
            b[..159].iter().all(|v| *v == 0.0),
            "idle time stays at zero"
        );
        let bucket_secs = 86_400.0 / 160.0;
        assert!((b[159] - 120_000.0 / bucket_secs).abs() < 1e-2);
        // Samples outside the window are ignored; nothing is NaN.
        let old = history_buckets(&[(now - 90_000, 5), (now + 10, 5)], now, 86_400, 160);
        assert!(old.iter().all(|v| *v == 0.0));
        assert!(history_buckets(&[], now, 86_400, 0).is_empty());
        // The first second of the window lands in the first bucket, the last in the last.
        let edges = history_buckets(&[(now - 86_400, 540), (now, 540)], now, 86_400, 160);
        assert!(edges[0] > 0.0 && edges[159] > 0.0);
    }

    #[test]
    fn slot_row_fits_and_keeps_proportion() {
        // Every connection gets a cell while they fit.
        assert_eq!(slot_cells(20, 12, 300.0), (20, 12));
        assert_eq!(slot_cells(1, 1, 300.0), (1, 1));
        // Too many for the width: proportional, never more lit than cells.
        let (cells, lit) = slot_cells(200, 100, 120.0);
        assert!((1..200).contains(&cells));
        assert!(lit <= cells && lit > 0);
        assert_eq!(slot_cells(0, 0, 100.0), (1, 0));
        assert_eq!(slot_cells(4, 9, 100.0), (4, 4), "effective capped at max");
        assert_eq!(slot_cells(4, 2, f32::NAN).0, 1);
    }

    #[test]
    fn motion_curves_stay_in_range() {
        for i in 0..40 {
            let phase = i as f32 / 40.0;
            let a = pulse_alpha(phase);
            assert!((0.0..=0.6).contains(&a));
            for n in [1, 2, 20] {
                for k in 0..n {
                    let s = slot_shimmer(k, n, phase);
                    assert!((0.7..=1.0).contains(&s), "{s}");
                }
            }
        }
        assert_eq!(pulse_alpha(f32::NAN), 0.0);
        assert!(pulse_alpha(0.0) > pulse_alpha(0.5));
        assert_eq!(pulse_alpha(1.0), 0.0);
    }

    #[test]
    fn dock_layout_breakpoints() {
        assert_eq!(layout_for(1280.0), DockLayout::Row);
        assert_eq!(layout_for(960.0), DockLayout::Row);
        assert_eq!(layout_for(900.0), DockLayout::Row);
        assert_eq!(layout_for(899.0), DockLayout::Stack);
    }

    #[test]
    fn history_cells_scale_to_the_busiest() {
        assert_eq!(heat_alpha(0.0, 10.0), None, "idle stays a well");
        assert_eq!(heat_alpha(5.0, 0.0), None);
        assert_eq!(heat_alpha(f32::NAN, 10.0), None);
        assert_eq!(heat_alpha(10.0, 10.0), Some(1.0));
        let low = heat_alpha(1.0, 10.0).unwrap();
        assert!((0.3..0.4).contains(&low), "{low}");
    }

    #[test]
    fn log_filter_levels() {
        assert!(LogFilter::All.admits(LogLevel::Info));
        assert!(!LogFilter::Problems.admits(LogLevel::Info));
        assert!(LogFilter::Problems.admits(LogLevel::Warn));
        assert!(LogFilter::Problems.admits(LogLevel::Error));
        assert!(!LogFilter::Errors.admits(LogLevel::Warn));
        assert!(LogFilter::Errors.admits(LogLevel::Error));
    }
}
