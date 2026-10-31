//! Bottom docks (outline §6.3): Connection Graph, NNTP Activity, Messages, Your Account.

use eframe::egui::{self, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui};
use newskite_service::{LogLevel, LogLine};
use newskite_types::format;

use super::app::{Action, Data, LIVE_WINDOW_SECS};
use super::theme::Palette;
use super::widgets::{card, kv, section_title};

/// Width at which the four docks sit side by side.
pub const FOUR_COLUMN_MIN: f32 = 1100.0;
/// Width at which the docks form a 2×2 grid (below: one column).
pub const TWO_COLUMN_MIN: f32 = 960.0;
/// Maximum number of points drawn for the stored history trace.
const HISTORY_POINTS: usize = 160;

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
    actions: &mut Vec<Action>,
) {
    card(pal).show(ui, |ui| {
        ui.set_min_height(height);
        ui.set_max_height(height.max(60.0));
        ui.set_width(ui.available_width());
        section_title(ui, pal, which.title());
        ui.add_space(4.0);
        match which {
            Dock::Graph => graph(ui, pal, data),
            Dock::Activity => activity(ui, pal, data, actions),
            Dock::Messages => messages(ui, pal, &data.logs),
            Dock::Account => account(ui, pal, data),
        }
    });
}

/// Draws the four docks in the arrangement that fits the available width.
pub fn show(ui: &mut Ui, pal: &Palette, data: &Data, actions: &mut Vec<Action>) {
    let width = ui.ctx().content_rect().width();
    match layout_for(width) {
        DockLayout::Row => {
            let h = (ui.available_height() - 22.0).max(80.0);
            ui.columns(4, |cols| {
                for (col, which) in cols.iter_mut().zip(Dock::ALL) {
                    dock(col, pal, data, which, h, actions);
                }
            });
        }
        DockLayout::Grid => {
            let h = ((ui.available_height() - 40.0) / 2.0).max(120.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for pair in Dock::ALL.chunks(2) {
                        ui.columns(2, |cols| {
                            for (col, which) in cols.iter_mut().zip(pair.iter().copied()) {
                                dock(col, pal, data, which, h, actions);
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
                        dock(ui, pal, data, which, 110.0, actions);
                    }
                });
        }
    }
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
                .size(17.0)
                .strong()
                .color(pal.accent_fg),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!("Remaining: {pending} / {total} jobs"))
                    .small()
                    .color(pal.muted),
            );
        });
    });

    let h = (ui.available_height() - 18.0).clamp(36.0, 160.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::CornerRadius::same(4), pal.sunken);
    let history_raw: Vec<u64> = data.history.iter().map(|s| s.bytes_down).collect();
    let history = downsample(&history_raw, HISTORY_POINTS);
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
            egui::FontId::proportional(12.0),
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
        let lp = time_points(&live, now_secs(), LIVE_WINDOW_SECS as i64, inner, y_max);
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
                    pal.accent.gamma_multiply(0.18),
                    Stroke::NONE,
                ));
            }
            painter.extend(area);
            painter.add(Shape::line(lp, Stroke::new(1.8, pal.accent)));
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
        "Dashed: stored history (last 24 h, {} samples)\nSolid: live, last {} min\nPeak: {}",
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
        ui.label(RichText::new(caption).small().color(pal.muted));
    }
}

fn activity(ui: &mut Ui, pal: &Palette, data: &Data, actions: &mut Vec<Action>) {
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
            if ui.button("Open Settings…").clicked() {
                actions.push(Action::OpenSettings);
            }
        }
        Some(server) => {
            egui::Grid::new("nntp-activity")
                .num_columns(2)
                .spacing([10.0, 4.0])
                .show(ui, |ui| {
                    kv(ui, pal, "Server", server.address());
                    kv(
                        ui,
                        pal,
                        "Security",
                        if server.tls {
                            "TLS"
                        } else {
                            "Plain (unencrypted)"
                        },
                    );
                    kv(
                        ui,
                        pal,
                        "Connections",
                        format!(
                            "{} · max {} · using {}",
                            server.connections,
                            s.max_connections,
                            s.effective_connections()
                        ),
                    );
                    kv(
                        ui,
                        pal,
                        "Username",
                        if server.username.is_empty() {
                            "(none)".to_owned()
                        } else {
                            server.username.clone()
                        },
                    );
                    let others = s.usable_servers().len().saturating_sub(1);
                    if others > 0 {
                        kv(
                            ui,
                            pal,
                            "Fail-over",
                            format!("{others} more server{}", if others == 1 { "" } else { "s" }),
                        );
                    }
                });
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
                            .monospace()
                            .size(11.0)
                            .color(pal.muted),
                    );
                    ui.label(
                        RichText::new(&line.text)
                            .size(12.0)
                            .color(level_color(pal, line.level)),
                    );
                });
            }
        });
}

fn account(ui: &mut Ui, pal: &Palette, data: &Data) {
    let t = &data.totals;
    egui::Grid::new("account")
        .num_columns(2)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            kv(ui, pal, "Downloaded", format::bytes(t.downloaded_bytes));
            kv(ui, pal, "Jobs done", t.jobs_done.to_string());
            kv(ui, pal, "Jobs pending", t.jobs_pending.to_string());
            if t.jobs_failed > 0 {
                ui.label(RichText::new("Jobs failed").color(pal.muted));
                ui.label(RichText::new(t.jobs_failed.to_string()).color(pal.negative));
                ui.end_row();
            }
            kv(
                ui,
                pal,
                "Max connections",
                data.settings.max_connections.to_string(),
            );
        });
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
    fn dock_layout_breakpoints() {
        assert_eq!(layout_for(1280.0), DockLayout::Row);
        assert_eq!(layout_for(1100.0), DockLayout::Row);
        assert_eq!(layout_for(1000.0), DockLayout::Grid);
        assert_eq!(layout_for(960.0), DockLayout::Grid);
        assert_eq!(layout_for(900.0), DockLayout::Stack);
    }
}
