//! The throughput graph and the message log.
//!
//! The **throughput graph** opens from the status bar's sparkline: the speed and what is
//! left, the live trace of the last minutes and, under it, the 24 h history on a band of
//! its own, so two time axes never share one plot. The **log** is the Log view: fixed
//! level and time columns so long lines wrap under their own text, a level filter, and
//! long message-id lists folded into a summary that opens in place.
//!
//! A new log line is marked by a tint and an accent bar painted *behind* its text, so the
//! text keeps full contrast from the first frame; the tint eases out over a few seconds, or
//! under reduced motion stays still and then goes. The live marker's pulse runs only while
//! bytes move and stops under reduced motion.

use eframe::egui::{
    self, epaint, Color32, CornerRadius, FontId, Id, Pos2, Rect, RichText, Sense, Shape, Stroke,
    Ui, Vec2,
};
use newskite_service::{Job, JobState, LogLevel, LogLine};
use newskite_types::format;

use super::app::{Action, Data, HISTORY_SECS, LIVE_WINDOW_SECS};
use super::fonts;
use super::queue::eta_secs;
use super::theme::Palette;
use super::motion;
use super::widgets::{paint_icon, segmented, Icon};

/// Cells of the stored history strip (24 h / 48 = 30 min each).
const HISTORY_POINTS: usize = 48;
/// Period of the live marker's pulse and the slot shimmer, seconds (the mark's busy beat).
const PULSE_PERIOD: f64 = 1.2;
/// Frame interval while something in the docks moves.
const MOTION_FRAME: std::time::Duration = std::time::Duration::from_millis(40);
/// Height of the 24 h history strip under the live graph, and the shortest live plot.
const HISTORY_STRIP_H: f32 = 10.0;
const MIN_PLOT_H: f32 = 24.0;
/// Gap between connection slot cells.
const SLOT_GAP: f32 = 2.0;
/// Narrowest slot cell; more connections than fit become proportional cells.
const SLOT_MIN_W: f32 = 4.0;
/// Widths of the level and time columns of the message log.
const LEVEL_COL_W: f32 = 14.0;
const TIME_COL_W: f32 = 58.0;
/// Seconds a new message stays marked.
const MESSAGE_NEW_SECS: f64 = 6.0;
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
    /// The Log view's search text (lines containing it, any case).
    pub log_search: String,
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

pub(crate) fn log_filter_chips(ui: &mut Ui, pal: &Palette, filter: &mut LogFilter) {
    let options: Vec<(LogFilter, &str)> = LogFilter::ALL.iter().map(|f| (*f, f.label())).collect();
    if let Some(f) = segmented(ui, pal, Id::new("log-filter"), &options, *filter) {
        *filter = f;
    }
}

/// `"now"`, `"12 s ago"` or `"3 min 5 s ago"` for a sample `secs` old.
fn sample_age(secs: i64) -> String {
    match secs.max(0) {
        0 => "now".to_owned(),
        s if s < 60 => format!("{s} s ago"),
        s => format!("{} min {} s ago", s / 60, s % 60),
    }
}

pub fn now_secs() -> i64 {
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

/// The throughput readout and graph: speed and what is left, the live plot of the last
/// minutes over the 24 h strip (shown when hovering the status bar's sparkline).
pub fn throughput_graph(ui: &mut Ui, pal: &Palette, data: &Data, reduced_motion: bool) {
    let motion = Motion::new(ui, data, reduced_motion);
    graph(ui, pal, data, motion);
}

fn graph(ui: &mut Ui, pal: &Palette, data: &Data, motion: Motion) {
    // Every gap below is explicit, so the bands fit the card's budget exactly.
    ui.spacing_mut().item_spacing.y = 0.0;
    let speed = data.speed();
    let left = data.bytes_left();
    // The figure counts to each new reading instead of jumping.
    let shown = motion::ease_to(ui.ctx(), Id::new("graph-speed"), speed as f32, 0.5, !motion.animate);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format::speed(shown.round().max(0.0) as u64))
                .font(FontId::new(19.0, fonts::mono_semibold()))
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
    // The scale eases to a new peak, so the trace rescales smoothly.
    let y_target = if live_peak > 0.0 {
        live_peak * 1.12
    } else {
        1.0
    };
    let y_max = motion::ease_to(ui.ctx(), Id::new("graph-scale"), y_target, 0.6, !motion.animate).max(1.0);
    let mut crosshair = false;

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
            painter.add(Shape::line(lp.clone(), Stroke::new(2.0, pal.series)));
            let end = lp[lp.len() - 1];
            if motion.moving {
                let ring = 3.0 + 7.0 * motion.phase;
                painter.circle_stroke(
                    end,
                    ring,
                    Stroke::new(1.5, Palette::fade(pal.series, pulse_alpha(motion.phase))),
                );
            }
            // A soft glow under the head of the trace.
            painter.circle_filled(end, 8.0, Palette::fade(pal.series, 0.12));
            painter.circle_filled(end, 5.0, Palette::fade(pal.series, 0.22));
            painter.circle_filled(end, 3.0, pal.series);
            painter.circle_filled(end, 1.4, pal.sunken);
            // Hover: a crosshair on the nearest sample with its reading.
            let start = now - LIVE_WINDOW_SECS as i64;
            let shown: Vec<(i64, u64)> = live.iter().copied().filter(|(ts, _)| *ts >= start).collect();
            if let (Some(p), true) = (resp.hover_pos(), shown.len() == lp.len()) {
                let nearest = lp
                    .iter()
                    .enumerate()
                    .min_by(|a, b| (a.1.x - p.x).abs().total_cmp(&(b.1.x - p.x).abs()));
                if let Some((i, pt)) = nearest {
                    crosshair = true;
                    painter.vline(pt.x, rect.y_range(), Stroke::new(1.0, pal.border_strong));
                    painter.circle_filled(*pt, 4.5, pal.series);
                    painter.circle_stroke(*pt, 4.5, Stroke::new(1.5, pal.sunken));
                    let (ts, v) = shown[i];
                    let galley = painter.layout_no_wrap(
                        format!("{}  ·  {}", format::speed(v), sample_age(now - ts)),
                        fonts::mono_id(10.5),
                        pal.text,
                    );
                    let size = galley.size() + Vec2::new(14.0, 8.0);
                    let x = if pt.x + 10.0 + size.x <= rect.right() - 2.0 { pt.x + 10.0 } else { pt.x - 10.0 - size.x };
                    let tag = Rect::from_min_size(Pos2::new(x.max(rect.left() + 2.0), rect.top() + 18.0), size);
                    painter.rect(tag, CornerRadius::same(6), pal.raised, Stroke::new(1.0, pal.border_strong), egui::StrokeKind::Inside);
                    painter.galley(tag.min + Vec2::new(7.0, 4.0), galley, pal.text);
                }
            }
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
    // The crosshair answers the hover itself; otherwise explain the graph.
    if !crosshair {
        resp.on_hover_text(format!(
            "Live: last {} min, one point per second\nStrip: last 24 h in {}-minute cells, brighter = faster, idle cells empty ({} samples)\nPeak: {}",
            LIVE_WINDOW_SECS / 60,
            HISTORY_SECS / 60 / HISTORY_POINTS as u64,
            data.history.len(),
            format::speed(peak as u64)
        ));
    }

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

pub fn clock(ts: i64) -> String {
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

/// A log line whose message-id list is folded: `lead` + "N missing articles" + `tail`
/// on the summary line, the listed `ids` (without `<>`) behind a disclosure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdList {
    /// Text before the count ("regret-gf.r01: ").
    pub lead: String,
    /// How many articles the line says are missing.
    pub count: u64,
    /// The ids the line lists (often a preview of the first few).
    pub ids: Vec<String>,
    /// Text after the list ("; trying a PAR2 repair").
    pub tail: String,
}

impl IdList {
    /// The folded line: `"regret-gf.r01: 66 missing articles; trying a PAR2 repair"`.
    pub fn summary(&self) -> String {
        format!(
            "{}{} missing article{}{}",
            self.lead,
            self.count,
            if self.count == 1 { "" } else { "s" },
            self.tail
        )
    }
}

/// Splits a "N missing article(s): <id>, <id>, ..." line (the engine's and the queue's
/// wording) into a summary and its ids. `None` when the line lists fewer than two ids
/// (nothing worth folding) or is not such a line.
pub fn split_id_list(text: &str) -> Option<IdList> {
    let at = text.find(" missing article")?;
    let digits_start = text[..at].rfind(|c: char| !c.is_ascii_digit()).map_or(0, |i| i + 1);
    let count: u64 = text[digits_start..at].parse().ok()?;
    let rest = &text[at + " missing article".len()..];
    let rest = rest.strip_prefix("(s)").or_else(|| rest.strip_prefix('s')).unwrap_or(rest);
    let list = rest.strip_prefix(": ")?;
    let (list, tail) = match list.find("; ") {
        Some(i) => (&list[..i], &list[i..]),
        None => (list, ""),
    };
    let ids: Vec<String> = list
        .split(", ")
        .map(|t| t.trim().trim_start_matches('<').trim_end_matches('>').trim_end_matches(','))
        .filter(|t| t.contains('@') && !t.starts_with("..."))
        .map(str::to_owned)
        .collect();
    if ids.len() < 2 {
        return None;
    }
    Some(IdList { lead: text[..digits_start].to_owned(), count, ids, tail: tail.to_owned() })
}

/// Strength (0…1) of a line's "new" mark `age` seconds after it first showed: eases out
/// over [`MESSAGE_NEW_SECS`] when animating; held at full, then gone, under reduced motion.
pub fn new_mark(age: f64, animate: bool) -> f32 {
    if !age.is_finite() || !(0.0..MESSAGE_NEW_SECS).contains(&age) {
        return 0.0;
    }
    if !animate {
        return 1.0;
    }
    let t = (age / MESSAGE_NEW_SECS) as f32;
    // Hold for the first third, then ease out.
    if t < 1.0 / 3.0 {
        1.0
    } else {
        let u = (t - 1.0 / 3.0) * 1.5;
        (1.0 - u).powi(2)
    }
}

/// What the Log view remembers about one line between frames.
#[derive(Clone, Copy)]
struct LineMemo {
    /// When it first showed (egui time).
    first_seen: f64,
    /// How tall it drew at [`LogMemo::text_w`], once drawn.
    height: Option<f32>,
}

/// [`LineMemo`]s of the lines listed last frame.
#[derive(Clone, Default)]
struct LogMemo {
    text_w: f32,
    lines: std::collections::HashMap<Id, LineMemo>,
}

/// The lines the Log view lists under `filter` and `search`, newest first.
fn visible_lines<'a>(data: &'a Data, filter: LogFilter, search: &str) -> Vec<&'a LogLine> {
    let needle = search.trim().to_lowercase();
    data.logs
        .iter()
        .filter(|l| filter.admits(l.level) && (needle.is_empty() || l.text.to_lowercase().contains(&needle)))
        .collect()
}

/// The listed lines as text, oldest first (what "Copy visible" puts on the clipboard).
pub fn visible_log_text(data: &Data, filter: LogFilter, search: &str) -> String {
    visible_lines(data, filter, search)
        .iter()
        .rev()
        .map(|l| format!("{} {:5} {}\n", format::timestamp_utc(l.ts), l.level.to_string(), l.text))
        .collect()
}

/// Messages, newest first, in three columns: a level mark, the clock, the text (which
/// wraps under itself, never under the clock). Long message-id lists fold behind a
/// disclosure; the newest failure of a job that can be retried on a backup carries that
/// button. A line that arrived after the dock's first frame is marked behind its text.
/// `search` keeps the lines that contain it (any case).
pub(crate) fn messages(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    filter: LogFilter,
    search: &str,
    animate: bool,
    actions: &mut Vec<Action>,
) {
    let logs = &data.logs;
    if logs.is_empty() {
        ui.label(RichText::new("No messages yet").color(pal.muted));
        return;
    }
    let needle = search.trim().to_lowercase();
    let shown = visible_lines(data, filter, search);
    if shown.is_empty() && !needle.is_empty() {
        ui.label(RichText::new(format!("No lines contain \u{201c}{}\u{201d}", search.trim())).color(pal.muted));
        return;
    }
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
    let now = ui.input(|i| i.time);
    // What is remembered about the lines on show (when each first showed, how tall it
    // drew), kept in one place and pruned to the lines still listed.
    let memo_key = Id::new("messages-memo");
    let memo: Option<std::sync::Arc<LogMemo>> = ctx.data(|d| d.get_temp(memo_key));
    let first_frame = memo.is_none();
    let mut memo = memo.map(|m| (*m).clone()).unwrap_or_default();
    let backup_servers = data.settings.usable_servers();
    let has_backup = backup_servers.len() >= 2;
    let jobs: std::collections::HashMap<i64, &Job> = data.jobs.iter().map(|j| (j.id, j)).collect();
    let mut offered: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut marked = false;
    let mut next = LogMemo::default();
    egui::ScrollArea::vertical()
        .id_salt("messages")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            let text_w = (ui.available_width() - LEVEL_COL_W - TIME_COL_W - 12.0).max(40.0);
            // The gutter columns are one text row tall, so glyph, clock and text share a center.
            let row_h = ui.fonts_mut(|f| f.row_height(&fonts::mono_id(12.0))).max(14.0);
            // Heights were measured at this text width; another width wraps anew.
            if (memo.text_w - text_w).abs() > 0.5 {
                memo.lines.values_mut().for_each(|l| l.height = None);
            }
            next.text_w = text_w;
            let clip = ui.clip_rect().expand2(Vec2::new(0.0, 200.0));
            for line in shown {
                let id = Id::new(("logline", line.ts, &line.text));
                // When this line first showed; lines present on the first frame count as old.
                let mut seen = memo.lines.get(&id).copied().unwrap_or(LineMemo {
                    first_seen: if first_frame { f64::NEG_INFINITY } else { now },
                    height: None,
                });
                let mark = new_mark(now - seen.first_seen, animate);
                marked |= mark > 0.0;
                let job = line.job_id.and_then(|jid| jobs.get(&jid).copied());
                let retry = has_backup
                    && line.level != LogLevel::Info
                    && job.is_some_and(|j| j.state == JobState::Failed && !offered.contains(&j.id));
                if retry {
                    offered.extend(job.map(|j| j.id));
                }
                // A line off screen keeps the room it took and is not laid out again.
                if let Some(h) = seen.height {
                    let rect = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), h));
                    if !clip.intersects(rect) {
                        ui.allocate_space(rect.size());
                        next.lines.insert(id, seen);
                        continue;
                    }
                }
                let folded = split_id_list(&line.text);
                // Reserve the backdrop's slot first so it paints under the row.
                let backdrop = ui.painter().add(Shape::Noop);
                let row = ui
                    .horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let (lr, _) = ui.allocate_exact_size(Vec2::new(LEVEL_COL_W, row_h), Sense::hover());
                        let glyph = Rect::from_center_size(lr.center(), Vec2::splat(11.0));
                        match line.level {
                            LogLevel::Info => {
                                ui.painter().circle_filled(glyph.center(), 2.0, pal.faint);
                            }
                            LogLevel::Warn => paint_icon(ui.painter(), glyph, Icon::StatusWarn, pal.warning),
                            LogLevel::Error => paint_icon(ui.painter(), glyph, Icon::StatusErr, pal.negative),
                        }
                        let (tr, _) = ui.allocate_exact_size(Vec2::new(TIME_COL_W, row_h), Sense::hover());
                        ui.painter().text(
                            tr.left_center(),
                            egui::Align2::LEFT_CENTER,
                            clock(line.ts),
                            fonts::mono_id(11.0),
                            if mark > 0.0 { pal.text } else { pal.faint },
                        );
                        ui.vertical(|ui| {
                            ui.set_max_width(text_w);
                            ui.spacing_mut().item_spacing.y = 3.0;
                            message_text(ui, pal, line, job, folded.as_ref(), id, actions);
                            if retry {
                                if let Some(job) = job {
                                    let r = super::serverstrip::strip_button(
                                        ui,
                                        pal,
                                        Some(Icon::RetryBackup),
                                        "Retry on backup",
                                        false,
                                        true,
                                    )
                                    .on_hover_text(super::serverstrip::backup_retry_explainer(&backup_servers));
                                    if r.clicked() {
                                        actions.push(Action::RetryOnBackup(job.id));
                                    }
                                }
                            }
                        });
                    })
                    .response
                    .rect;
                seen.height = Some(row.height());
                next.lines.insert(id, seen);
                if mark > 0.0 {
                    // Across the whole log width, so a short line reads as marked too.
                    let band = Rect::from_min_max(
                        Pos2::new(row.left() - 3.0, row.top() - 1.5),
                        Pos2::new(ui.max_rect().right(), row.bottom() + 1.5),
                    );
                    let bar = Rect::from_min_size(band.min, Vec2::new(2.0, band.height()));
                    ui.painter().set(
                        backdrop,
                        Shape::Vec(vec![
                            Shape::rect_filled(band, CornerRadius::same(3), Palette::fade(pal.accent_soft, 0.85 * mark)),
                            Shape::rect_filled(bar, CornerRadius::same(1), Palette::fade(pal.accent, mark)),
                        ]),
                    );
                }
            }
        });
    ctx.data_mut(|d| d.insert_temp(memo_key, std::sync::Arc::new(next)));
    if marked {
        ctx.request_repaint_after(if animate { MOTION_FRAME } else { std::time::Duration::from_millis(500) });
    }
}

/// The text column of one log line: the line (or its folded summary with a disclosure
/// and, when open, the ids with a copy button). A line about a job opens that job.
fn message_text(
    ui: &mut Ui,
    pal: &Palette,
    line: &LogLine,
    job: Option<&Job>,
    folded: Option<&IdList>,
    id: Id,
    actions: &mut Vec<Action>,
) {
    let color = level_color(pal, line.level);
    let font = fonts::mono_id(12.0);
    let open_key = id.with("open");
    let open = ui.ctx().data(|d| d.get_temp::<bool>(open_key)).unwrap_or(false);
    let text = folded.map_or_else(|| line.text.clone(), IdList::summary);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let label = egui::Label::new(RichText::new(&text).font(font.clone()).color(color)).wrap();
        let resp = match job {
            Some(_) => ui.add(label.sense(Sense::click()).selectable(false)),
            None => ui.add(label),
        };
        if let Some(job) = job {
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("Show {}", job.name));
            if resp.hovered() {
                ui.painter().line_segment(
                    [resp.rect.left_bottom(), resp.rect.right_bottom()],
                    Stroke::new(1.0, Palette::fade(color, 0.5)),
                );
            }
            if resp.clicked() {
                actions.push(Action::ShowJob(job.id));
            }
        }
        if let Some(f) = folded {
            let what = format!("{} {}", if open { "Hide" } else { "Show" }, if f.ids.len() as u64 == f.count { "ids" } else { "first ids" });
            let r = ui.add(
                egui::Button::new(
                    RichText::new(format!("{} {what}", if open { "▾" } else { "▸" }))
                        .font(fonts::mono_id(11.0))
                        .color(pal.accent_fg),
                )
                .frame(false),
            );
            if r.clicked() {
                ui.ctx().data_mut(|d| d.insert_temp(open_key, !open));
            }
        }
    });
    if let (Some(f), true) = (folded, open) {
        egui::Frame::new()
            .fill(pal.sunken)
            .corner_radius(CornerRadius::same(4))
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(14.0, 3.0);
                    for mid in &f.ids {
                        ui.add(egui::Label::new(RichText::new(format!("<{mid}>")).font(fonts::mono_id(11.0)).color(pal.muted)));
                    }
                });
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let note = if (f.ids.len() as u64) < f.count {
                        format!("{} of {} ids are listed in this message", f.ids.len(), f.count)
                    } else {
                        format!("all {} listed", f.count)
                    };
                    ui.label(RichText::new(note).font(fonts::mono_id(10.5)).color(pal.faint));
                    if ui
                        .add(egui::Button::new(RichText::new("Copy ids").font(fonts::mono_id(10.5)).color(pal.accent_fg)).frame(false))
                        .clicked()
                    {
                        let ids: Vec<String> = f.ids.iter().map(|m| format!("<{m}>")).collect();
                        ui.ctx().copy_text(ids.join("\n"));
                    }
                });
            });
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
    fn history_cells_scale_to_the_busiest() {
        assert_eq!(heat_alpha(0.0, 10.0), None, "idle stays a well");
        assert_eq!(heat_alpha(5.0, 0.0), None);
        assert_eq!(heat_alpha(f32::NAN, 10.0), None);
        assert_eq!(heat_alpha(10.0, 10.0), Some(1.0));
        let low = heat_alpha(1.0, 10.0).unwrap();
        assert!((0.3..0.4).contains(&low), "{low}");
    }

    #[test]
    fn id_lists_fold_into_a_summary() {
        let f = split_id_list("regret-gf.r01: 66 missing article(s): <a@x>, <b@x>, <c@x>, ...").unwrap();
        assert_eq!(f.summary(), "regret-gf.r01: 66 missing articles");
        assert_eq!(f.ids, ["a@x", "b@x", "c@x"]);
        let f = split_id_list("m: 7 missing articles: id1@x, id2@x, ... (+5 more); trying a PAR2 repair").unwrap();
        assert_eq!(f.summary(), "m: 7 missing articles; trying a PAR2 repair");
        assert_eq!(f.ids, ["id1@x", "id2@x"]);
        let f = split_id_list("failed m: 2 missing articles: id1@x, id2@x").unwrap();
        assert_eq!((f.count, f.lead.as_str()), (2, "failed m: "));
        // One id, or no list at all: nothing to fold.
        assert_eq!(split_id_list("1 missing article: gone@x"), None);
        assert_eq!(split_id_list("regret-gf.r01: 66 missing articles"), None);
        assert_eq!(split_id_list("started job"), None);
    }

    #[test]
    fn new_mark_holds_then_eases_and_respects_reduced_motion() {
        assert_eq!(new_mark(f64::NEG_INFINITY, true), 0.0);
        assert_eq!(new_mark(-1.0, true), 0.0);
        assert_eq!(new_mark(0.0, true), 1.0);
        assert_eq!(new_mark(1.0, true), 1.0);
        let mid = new_mark(4.0, true);
        assert!(mid > 0.0 && mid < 1.0, "{mid}");
        assert_eq!(new_mark(MESSAGE_NEW_SECS, true), 0.0);
        // Reduced motion: still at full strength until it goes.
        assert_eq!(new_mark(5.9, false), 1.0);
        assert_eq!(new_mark(6.1, false), 0.0);
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
