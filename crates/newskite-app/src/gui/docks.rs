//! Bottom docks (outline §6.3): Connection Graph, NNTP Activity, Messages, Your Account.

use eframe::egui::{self, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui};
use newskite_service::{LogLevel, LogLine};
use newskite_types::format;

use super::app::{Action, Data, HISTORY_SECS, LIVE_WINDOW_SECS};
use super::fonts;
use super::theme::Palette;
use super::widgets::{card, kit_button, section_title, ButtonKind, Icon};

/// Width at which the four docks sit side by side.
pub const FOUR_COLUMN_MIN: f32 = 1100.0;
/// Width at which the docks form a 2×2 grid (below: one column).
pub const TWO_COLUMN_MIN: f32 = 960.0;
/// Time buckets of the stored history trace (24 h / 160 = 9 min each).
const HISTORY_POINTS: usize = 160;
/// Vertical room a dock card takes beyond its content height: the 12 px inner margin on
/// both sides and the hairline.
const CARD_CHROME: f32 = 26.0;

/// Dock arrangement for a given window width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockLayout {
    /// Four side by side.
    Row,
    /// Two by two.
    Grid,
    /// One column (scrolls).
    Stack,
}

/// Picks the arrangement for `width` (logical pixels).
pub fn layout_for(width: f32) -> DockLayout {
    if width >= FOUR_COLUMN_MIN {
        DockLayout::Row
    } else if width >= TWO_COLUMN_MIN {
        DockLayout::Grid
    } else {
        DockLayout::Stack
    }
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
    Graph,
    Activity,
    Messages,
    Account,
}

impl Dock {
    const ALL: [Dock; 4] = [Dock::Graph, Dock::Activity, Dock::Messages, Dock::Account];

    fn title(self) -> &'static str {
        match self {
            Dock::Graph => "Connection Graph",
            Dock::Activity => "NNTP Activity",
            Dock::Messages => "Messages",
            Dock::Account => "Your Account",
        }
    }
}

fn dock(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    which: Dock,
    height: f32,
    compact: bool,
    actions: &mut Vec<Action>,
) {
    card(pal).show(ui, |ui| {
        // An exact height: every card of a row lines up, and content that does not fit
        // scrolls inside its card instead of pushing the next row down.
        ui.set_min_height(height);
        ui.set_max_height(height.max(60.0));
        ui.set_width(ui.available_width());
        section_title(ui, pal, which.title());
        ui.add_space(4.0);
        match which {
            Dock::Graph => graph(ui, pal, data),
            Dock::Messages => messages(ui, pal, &data.logs),
            Dock::Activity | Dock::Account => {
                egui::ScrollArea::vertical()
                    .id_salt(("dock-body", which.title()))
                    .auto_shrink([false, false])
                    .max_height(ui.available_height().max(20.0))
                    .show(ui, |ui| match which {
                        Dock::Activity => activity(ui, pal, data, compact, actions),
                        _ => account(ui, pal, data, compact),
                    });
            }
        }
    });
}

/// Content height of each dock card in the 2×2 grid for a dock panel `available`
/// tall: two rows of cards (each with its [`CARD_CHROME`]) and the gap between them.
pub fn grid_card_height(available: f32, gap: f32) -> f32 {
    ((available - 2.0 * CARD_CHROME - gap) / 2.0).max(80.0)
}

/// Draws the four docks in the arrangement that fits the available width.
pub fn show(ui: &mut Ui, pal: &Palette, data: &Data, actions: &mut Vec<Action>) {
    let width = ui.ctx().content_rect().width();
    match layout_for(width) {
        DockLayout::Row => {
            let h = (ui.available_height() - CARD_CHROME).max(80.0);
            ui.columns(4, |cols| {
                for (col, which) in cols.iter_mut().zip(Dock::ALL) {
                    dock(col, pal, data, which, h, false, actions);
                }
            });
        }
        DockLayout::Grid => {
            let gap = ui.spacing().item_spacing.y;
            let h = grid_card_height(ui.available_height(), gap);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for pair in Dock::ALL.chunks(2) {
                        ui.columns(2, |cols| {
                            for (col, which) in cols.iter_mut().zip(pair.iter().copied()) {
                                dock(col, pal, data, which, h, true, actions);
                            }
                        });
                    }
                });
        }
        DockLayout::Stack => {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for which in Dock::ALL {
                        dock(ui, pal, data, which, 110.0, false, actions);
                    }
                });
        }
    }
}

/// Key/value readings in a grid: one pair per row, or two side by side when `compact`
/// (the 2×2 dock grid, where height is short and width is not).
fn readings(
    ui: &mut Ui,
    pal: &Palette,
    id: &str,
    rows: &[(&str, String, egui::Color32)],
    compact: bool,
) {
    let per_row = if compact { 2 } else { 1 };
    egui::Grid::new(id)
        .num_columns(2 * per_row)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            for chunk in rows.chunks(per_row) {
                for (key, value, color) in chunk {
                    ui.label(RichText::new(*key).color(pal.muted));
                    ui.label(
                        RichText::new(value)
                            .font(fonts::mono_id(12.0))
                            .color(*color),
                    );
                }
                ui.end_row();
            }
        });
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn graph(ui: &mut Ui, pal: &Palette, data: &Data) {
    let pending = data.totals.jobs_pending;
    let total = data.jobs.len();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format::speed(data.speed()))
                .font(egui::FontId::new(17.0, fonts::mono_semibold()))
                .color(pal.text),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!("Remaining: {pending} / {total} jobs"))
                    .font(fonts::mono_id(11.0))
                    .color(pal.muted),
            );
        });
    });

    let h = (ui.available_height() - 18.0).clamp(36.0, 160.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::CornerRadius::same(4), pal.sunken);
    let now = now_secs();
    let history = if data.history.is_empty() {
        Vec::new()
    } else {
        let raw: Vec<(i64, u64)> = data.history.iter().map(|s| (s.ts, s.bytes_down)).collect();
        history_buckets(&raw, now, HISTORY_SECS as i64, HISTORY_POINTS)
    };
    let live: Vec<(i64, u64)> = data.live.iter().copied().collect();
    let peak = history
        .iter()
        .copied()
        .chain(live.iter().map(|&(_, v)| v as f32))
        .fold(0.0_f32, f32::max);
    let y_max = if peak > 0.0 { peak * 1.1 } else { 1.0 };
    let inner = rect.shrink2(egui::vec2(3.0, 0.0));

    if history.is_empty() && live.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No throughput history yet",
            egui::FontId::proportional(12.5),
            pal.muted,
        );
    } else {
        // Grid lines only behind data, never behind the empty-state text.
        for f in [0.25, 0.5, 0.75] {
            let y = rect.top() + rect.height() * f;
            painter.line_segment(
                [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
                Stroke::new(1.0, pal.border),
            );
        }
        let hp = index_points(&history, inner, y_max);
        if hp.len() >= 2 {
            painter.extend(Shape::dashed_line(
                &hp,
                Stroke::new(1.3, pal.muted),
                4.0,
                3.0,
            ));
        }
        let lp = time_points(&live, now, LIVE_WINDOW_SECS as i64, inner, y_max);
        if lp.len() >= 2 {
            // Soft area under the live trace, then the trace itself.
            let mut area = Vec::with_capacity(lp.len() * 2);
            for w in lp.windows(2) {
                let (a, b) = (w[0], w[1]);
                area.push(Shape::convex_polygon(
                    vec![
                        a,
                        b,
                        Pos2::new(b.x, rect.bottom()),
                        Pos2::new(a.x, rect.bottom()),
                    ],
                    pal.series.gamma_multiply(0.18),
                    Stroke::NONE,
                ));
            }
            painter.extend(area);
            painter.add(Shape::line(lp, Stroke::new(1.8, pal.series)));
        }
    }
    let summary = if history.is_empty() && live.is_empty() {
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
        "Dashed: stored history, last 24 h ({} samples; idle time at zero)\nSolid: live, last {} min\nPeak: {}",
        data.history.len(),
        LIVE_WINDOW_SECS / 60,
        format::speed(peak as u64)
    ));
    if !(history.is_empty() && live.is_empty()) {
        let caption = if history.is_empty() {
            format!(
                "Peak {}  ·  live, last {} min",
                format::speed(peak as u64),
                LIVE_WINDOW_SECS / 60
            )
        } else {
            format!(
                "Peak {}  ·  dashed = 24 h history",
                format::speed(peak as u64)
            )
        };
        ui.label(
            RichText::new(caption)
                .font(fonts::mono_id(11.0))
                .color(pal.faint),
        );
    }
}

fn activity(ui: &mut Ui, pal: &Palette, data: &Data, compact: bool, actions: &mut Vec<Action>) {
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
        }
        Some(server) => {
            let others = s.usable_servers().len().saturating_sub(1);
            let mut rows = vec![
                ("Server", server.address(), pal.text),
                (
                    "Security",
                    if server.tls {
                        "TLS"
                    } else {
                        "Plain (unencrypted)"
                    }
                    .to_owned(),
                    pal.text,
                ),
                (
                    "Connections",
                    format!(
                        "{} · max {} · using {}",
                        server.connections,
                        s.max_connections,
                        s.effective_connections()
                    ),
                    pal.text,
                ),
                (
                    "Username",
                    if server.username.is_empty() {
                        "(none)".to_owned()
                    } else {
                        server.username.clone()
                    },
                    pal.text,
                ),
                // Real counts from this session's engine events, never estimates.
                (
                    "Articles",
                    format!("{} this session", data.articles_fetched),
                    pal.text,
                ),
                (
                    "Missing",
                    data.articles_missing.to_string(),
                    if data.articles_missing > 0 {
                        pal.warning
                    } else {
                        pal.text
                    },
                ),
            ];
            if others > 0 {
                rows.push((
                    "Fail-over",
                    format!("{others} more server{}", if others == 1 { "" } else { "s" }),
                    pal.text,
                ));
            }
            readings(ui, pal, "nntp-activity", &rows, compact);
        }
    }
}

fn level_color(pal: &Palette, level: LogLevel) -> egui::Color32 {
    match level {
        LogLevel::Info => pal.text,
        LogLevel::Warn => pal.warning,
        LogLevel::Error => pal.negative,
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

fn messages(ui: &mut Ui, pal: &Palette, logs: &[LogLine]) {
    if logs.is_empty() {
        ui.label(RichText::new("No messages yet").color(pal.muted));
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("messages")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for line in logs {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(clock(line.ts))
                            .font(fonts::mono_id(11.0))
                            .color(pal.faint),
                    );
                    ui.label(
                        RichText::new(&line.text)
                            .font(fonts::mono_id(11.5))
                            .color(level_color(pal, line.level)),
                    );
                });
            }
        });
}

fn account(ui: &mut Ui, pal: &Palette, data: &Data, compact: bool) {
    let t = &data.totals;
    let mut rows = vec![
        ("Downloaded", format::bytes(t.downloaded_bytes), pal.text),
        ("Jobs done", t.jobs_done.to_string(), pal.text),
        ("Jobs pending", t.jobs_pending.to_string(), pal.text),
    ];
    if t.jobs_failed > 0 {
        rows.push(("Jobs failed", t.jobs_failed.to_string(), pal.negative));
    }
    rows.push((
        "Max connections",
        data.settings.max_connections.to_string(),
        pal.text,
    ));
    readings(ui, pal, "account", &rows, compact);
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
    fn grid_cards_fit_the_panel() {
        // Two rows of cards plus the gap never exceed the panel.
        for avail in [240.0_f32, 290.0, 320.0] {
            let h = grid_card_height(avail, 8.0);
            assert!(2.0 * (h + CARD_CHROME) + 8.0 <= avail + 0.5, "{avail}: {h}");
        }
    }

    #[test]
    fn dock_layout_breakpoints() {
        assert_eq!(layout_for(1280.0), DockLayout::Row);
        assert_eq!(layout_for(1100.0), DockLayout::Row);
        assert_eq!(layout_for(1000.0), DockLayout::Grid);
        assert_eq!(layout_for(960.0), DockLayout::Grid);
        assert_eq!(layout_for(900.0), DockLayout::Stack);
    }
}
