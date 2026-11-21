//! Status bar (outline §6.4).

use eframe::egui::{self, Align, Color32, Layout, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use newskite_service::JobState;
use newskite_types::format;

use super::app::{Action, Data};
use super::brand;
use super::motion::Beat;
use super::theme::Palette;
use super::widgets::{credit, Status, CREDIT_URL};

/// Free space below which the reading turns to the warning color.
pub const LOW_DISK: u64 = 2 * 1024 * 1024 * 1024;

/// The right-hand reading: free space on the download folder's filesystem (when known).
/// `true` when the free space is short of what the queue still needs or below
/// [`LOW_DISK`].
pub fn disk_reading(data: &Data) -> (String, bool) {
    match data.disk_free {
        Some(free) => {
            let left = data.bytes_left();
            let low = free < LOW_DISK || free < left;
            (format!("{} free", format::bytes(free)), low)
        }
        None => ("free space unknown".into(), false),
    }
}

/// Fraction of the bytes of every unfinished job that is already down (0…1; 0 when
/// nothing is pending). The status bar's hairline while a run is active.
pub fn pending_fraction(data: &Data) -> f32 {
    let (done, total) =
        data.jobs
            .iter()
            .filter(|j| j.state != JobState::Done)
            .fold((0u64, 0u64), |(d, t), j| {
                (
                    d.saturating_add(data.shown_done(j)),
                    t.saturating_add(j.bytes_total),
                )
            });
    format::fraction(done, total)
}

/// What the status pill shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pill {
    /// A server is configured and the queue is idle.
    Online,
    /// A run is active (speed text).
    Downloading(String),
    /// Pause requested while a job is still downloading (speed text): the pause is
    /// cooperative and takes effect once the current job finishes.
    Pausing(String),
    /// Paused, and no job is downloading.
    Paused,
    /// The last run ended with an error.
    Error(String),
    /// No usable server.
    NoServer,
}

impl Pill {
    /// Label text.
    pub fn text(&self) -> String {
        match self {
            Pill::Online => "Online".into(),
            // The speed is the center reading's.
            Pill::Downloading(_) => "Downloading".into(),
            Pill::Pausing(_) => "Pausing".into(),
            Pill::Paused => "Paused".into(),
            Pill::Error(e) => short_cause(e),
            Pill::NoServer => "No server".into(),
        }
    }
}

/// Longest error cause shown in the chip; the full text is in its tooltip.
const CAUSE_CHARS: usize = 48;

/// The chip words for an error: what happened, on one line, elided to
/// [`CAUSE_CHARS`] (the brand's chips say what happened, never just "Error").
pub fn short_cause(error: &str) -> String {
    let line = error
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.is_empty() {
        return "Failed".into();
    }
    if line.chars().count() <= CAUSE_CHARS {
        return line.to_owned();
    }
    let cut: String = line.chars().take(CAUSE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// Derives the pill from the current data. Pause is cooperative: while the current job
/// still downloads, the pill keeps the speed and says the pause is pending.
pub fn pill_state(data: &Data) -> Pill {
    let st = &data.status;
    if st.running && st.paused {
        Pill::Pausing(format::speed(data.speed()))
    } else if st.running {
        Pill::Downloading(format::speed(data.speed()))
    } else if st.paused {
        Pill::Paused
    } else if let Some(e) = &st.last_error {
        Pill::Error(e.clone())
    } else if data.settings.first_usable_server().is_none() {
        Pill::NoServer
    } else {
        Pill::Online
    }
}

impl Pill {
    /// The chip status: online ok, downloading live, paused and no server warn, error err.
    pub fn status(&self) -> Status {
        match self {
            Pill::Online => Status::Ok,
            Pill::Downloading(_) | Pill::Pausing(_) => Status::Live,
            Pill::Paused | Pill::NoServer => Status::Warn,
            Pill::Error(_) => Status::Err,
        }
    }
}

/// The left reading, e.g. `"2 servers · 20 TLS"`: the usable servers and the connections
/// a run opens, and how they connect. `None` without a usable server.
pub fn servers_reading(data: &Data) -> Option<String> {
    let usable = data.settings.usable_servers();
    if usable.is_empty() {
        return None;
    }
    let tls = usable.iter().filter(|s| s.tls).count();
    let conn = data.settings.effective_connections();
    let connections = if tls == usable.len() {
        format!("{conn} TLS")
    } else if tls == 0 {
        format!("{conn} plain")
    } else {
        format!("{conn} conn, TLS + plain")
    };
    let n = usable.len();
    Some(format!("{n} server{}  ·  {connections}", if n == 1 { "" } else { "s" }))
}

/// The fastest second on record: the live window and the stored history (both one sample
/// per second).
pub fn peak_speed(data: &Data) -> u64 {
    let live = data.live.iter().map(|&(_, v)| v).max().unwrap_or(0);
    live.max(data.history_peak)
}

/// The center reading: what the queue is doing (`Downloading` and the speed, `Pausing`,
/// `Paused` or `Idle`) and the fastest second on record, once there is one. How much is
/// left and the time left are the queue header's.
pub fn activity_reading(data: &Data) -> String {
    let st = &data.status;
    let head = if st.running {
        let word = if st.paused { "Pausing" } else { "Downloading" };
        format!("{word}  ·  {}", format::speed(data.speed()))
    } else if st.paused {
        "Paused".to_owned()
    } else {
        "Idle".to_owned()
    };
    // A limit in force says so in place of the record second.
    match (data.settings.speed_limit_bps(), peak_speed(data)) {
        (0, 0) => head,
        (0, peak) => format!("{head}  ·  peak {}", format::speed(peak)),
        (limit, _) => format!("{head}  ·  limit {}", format::speed(limit)),
    }
}

/// The speed limits offered from the status bar, in KiB/s (0 = unlimited).
pub const SPEED_PRESETS: [u64; 6] = [0, 50 * 1024, 25 * 1024, 10 * 1024, 5 * 1024, 1024];

/// The status bar's speed menu: the presets (the one in force checked), the current
/// limit when it is none of them, and a field for any other in MB/s.
fn speed_menu(ui: &mut Ui, pal: &Palette, data: &Data, actions: &mut Vec<Action>) {
    ui.set_min_width(170.0);
    ui.label(RichText::new("Speed limit").small().color(pal.muted));
    let current = data.settings.speed_limit_kbps;
    let label = |kbps: u64| if kbps == 0 { "Unlimited".to_owned() } else { format::speed(kbps.saturating_mul(1024)) };
    for kbps in SPEED_PRESETS {
        if ui.add(egui::Button::selectable(current == kbps, label(kbps))).clicked() {
            if current != kbps {
                actions.push(Action::SetSpeedLimit(kbps));
            }
            ui.close();
        }
    }
    if !SPEED_PRESETS.contains(&current) {
        let _ = ui.add(egui::Button::selectable(true, label(current)));
    }
    ui.separator();
    ui.label(RichText::new("Other limit").small().color(pal.muted));
    // The draft lives in memory while the menu is open; it starts at the limit in force.
    let id = egui::Id::new("speed-menu-other");
    let mut mbps: f64 = ui
        .data(|d| d.get_temp(id))
        .unwrap_or_else(|| if current == 0 { 20.0 } else { current as f64 / 1024.0 });
    let mut apply = false;
    ui.horizontal(|ui| {
        // Menus draw their widgets frameless; the field and its button keep a frame here
        // so they read as editable.
        let v = &mut ui.style_mut().visuals;
        v.button_frame = true;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, pal.border_strong);
        v.widgets.inactive.weak_bg_fill = pal.well;
        v.widgets.inactive.bg_fill = pal.well;
        let r = ui.add(
            egui::DragValue::new(&mut mbps)
                .range(0.1..=10_000.0)
                .speed(0.5)
                .max_decimals(1)
                .suffix(" MB/s"),
        );
        apply = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        apply |= ui.button("Set").clicked();
    });
    ui.data_mut(|d| d.insert_temp(id, mbps));
    if apply {
        let kbps = custom_limit_kbps(mbps);
        if kbps != current {
            actions.push(Action::SetSpeedLimit(kbps));
        }
        ui.data_mut(|d| d.remove::<f64>(id));
        ui.close();
    }
}

/// A limit typed in MB/s as KiB/s (the unit the setting keeps), never below 1 KiB/s.
pub fn custom_limit_kbps(mbps: f64) -> u64 {
    (mbps * 1024.0).round().max(1.0) as u64
}

/// The servers' health this session, for the left reading's hover: one block per usable
/// server with its role, what it delivered and its last error.
fn server_health_tip(ui: &mut Ui, pal: &Palette, data: &Data) {
    let now = super::docks::now_secs();
    let servers = data.settings.usable_servers();
    if servers.is_empty() {
        ui.label("No server is set up. Add one in Settings.");
        return;
    }
    for (i, cfg) in servers.iter().enumerate() {
        if i > 0 {
            ui.add_space(6.0);
        }
        let h = data.servers.iter().find(|h| h.address == cfg.address());
        let (glyph, color) = match super::serverstrip::health_of(h, now) {
            super::serverstrip::Health::Ok => ("✔", pal.positive),
            super::serverstrip::Health::Errors => ("!", pal.warning),
            super::serverstrip::Health::Refused => ("✖", pal.negative),
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(glyph).color(color));
            ui.label(RichText::new(cfg.address()).font(super::fonts::mono_id(12.0)).color(pal.text));
            ui.label(
                RichText::new(format!("{}  ·  {}", if i == 0 { "primary" } else { "backup" }, if cfg.tls { "TLS" } else { "plain" }))
                    .small()
                    .color(pal.muted),
            );
        });
        let line = match h {
            Some(h) => format!(
                "{} articles  ·  {} not on server  ·  {} errors",
                super::serverstrip::thousands(h.articles),
                super::serverstrip::thousands(h.missing),
                super::serverstrip::thousands(h.errors)
            ),
            None => "not used yet this session".to_owned(),
        };
        ui.label(RichText::new(line).small().color(pal.muted));
        if let Some(why) = h.and_then(|h| h.unusable.as_ref()) {
            ui.label(RichText::new(format!("refused: {why}")).small().color(pal.negative));
        } else if let Some((_, e)) = h.and_then(|h| h.last_error.as_ref()) {
            ui.label(RichText::new(format!("last error: {e}")).small().color(pal.warning));
        }
    }
}

/// Width of the status bar's sparkline.
const SPARK_W: f32 = 120.0;
/// Width of the throughput graph shown from the sparkline.
const GRAPH_POPUP_W: f32 = 340.0;

/// The series the sparkline draws: the live window when it has a trace, else the stored
/// history in 30-minute cells.
fn spark_values(data: &Data) -> Vec<f32> {
    if data.live.len() >= 2 {
        return data.live.iter().map(|&(_, v)| v as f32).collect();
    }
    data.history_cells.clone()
}

/// Cells the stored day is folded into for the sparkline (30 minutes each).
pub const SPARK_CELLS: usize = 48;

/// A small zigzag (the activity glyph): `color` strokes in `rect`.
fn paint_pulse(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let (w, h) = (rect.width() / 2.0, rect.height() / 2.0);
    let pts = [
        Pos2::new(c.x - w, c.y),
        Pos2::new(c.x - w * 0.45, c.y),
        Pos2::new(c.x - w * 0.2, c.y - h),
        Pos2::new(c.x + w * 0.15, c.y + h),
        Pos2::new(c.x + w * 0.4, c.y),
        Pos2::new(c.x + w, c.y),
    ];
    painter.add(egui::Shape::line(pts.to_vec(), Stroke::new(1.3, color)));
}

/// A drive glyph for the free-space reading.
fn paint_drive(painter: &egui::Painter, rect: Rect, color: Color32) {
    let body = Rect::from_center_size(rect.center(), Vec2::new(rect.width(), rect.height() * 0.62));
    painter.rect_stroke(body, egui::CornerRadius::same(2), Stroke::new(1.2, color), egui::StrokeKind::Inside);
    painter.circle_filled(Pos2::new(body.right() - 3.0, body.center().y), 1.1, color);
}

/// Draws the status bar. Left: the state chip (its live dot breathing with the busy beat)
/// and the servers a run uses. Center: the activity glyph and reading and a sparkline of
/// the throughput, whose hover shows the full graph with the 24 h strip. Right: the free
/// space on the download folder's filesystem (only with the sidebar a `rail`: the wide
/// sidebar has its own disk meter) and the quiet innerpulse credit. While a run is active
/// a 2 px accent hairline along the top edge shows how much of the pending bytes is down.
pub fn show(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    rail: bool,
    reduced_motion: bool,
    actions: &mut Vec<Action>,
) {
    let st = &data.status;
    let beat = Beat::new(
        ui.ctx(),
        st.running && data.speed() > 0,
        reduced_motion,
        brand::PERIOD_BUSY,
    );
    let bar = ui.max_rect();
    if st.running {
        let f = super::motion::ease_to(
            ui.ctx(),
            egui::Id::new("statusbar-pending"),
            pending_fraction(data),
            0.6,
            reduced_motion,
        );
        let y = bar.top() - 2.0;
        ui.painter().line_segment(
            [
                Pos2::new(bar.left() - 10.0, y),
                Pos2::new(bar.right() + 10.0, y),
            ],
            Stroke::new(2.0, pal.well),
        );
        if f > 0.0 {
            let x = bar.left() - 10.0 + (bar.width() + 20.0) * f;
            ui.painter().line_segment(
                [Pos2::new(bar.left() - 10.0, y), Pos2::new(x, y)],
                Stroke::new(2.0, pal.accent),
            );
        }
    }
    let mono = super::fonts::mono_id(11.0);
    let reading = activity_reading(data);
    let reading_w = ui
        .painter()
        .layout_no_wrap(reading.clone(), mono.clone(), pal.muted)
        .size()
        .x;
    // The center group: glyph, reading, gap, sparkline.
    let center_w = 14.0 + 6.0 + reading_w + 12.0 + SPARK_W;
    ui.horizontal_centered(|ui| {
        let state = pill_state(data);
        let color = state.status().color(pal);
        let (dot, dresp) = ui.allocate_exact_size(Vec2::new(14.0, 14.0), Sense::hover());
        match beat.moving.then(|| beat.breath()) {
            Some(b) => {
                ui.painter().circle_stroke(dot.center(), 4.0 + 3.0 * (1.0 - b), Stroke::new(1.0, Palette::fade(color, 0.45 * b)));
                ui.painter().circle_filled(dot.center(), 4.0, Palette::fade(color, 0.6 + 0.4 * b));
            }
            None => {
                ui.painter().circle_filled(dot.center(), 4.0, color);
            }
        }
        let state_text = state.text();
        dresp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &state_text));
        ui.add_space(4.0);
        let left_text = match &state {
            Pill::Error(e) => RichText::new(short_cause(e)).font(mono.clone()).color(pal.negative),
            Pill::NoServer => RichText::new("No server").font(mono.clone()).color(pal.warning),
            _ => RichText::new(servers_reading(data).unwrap_or_default()).font(mono.clone()).color(pal.muted),
        };
        let r = ui.add(egui::Label::new(left_text).sense(Sense::click()).selectable(false));
        if state == Pill::NoServer {
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Open Settings").clicked() {
                actions.push(Action::OpenSettings);
            }
        } else if let Pill::Error(e) = &state {
            // The chip opens the job that failed most recently.
            let failed = data
                .jobs
                .iter()
                .filter(|j| j.state == JobState::Failed)
                .max_by_key(|j| (j.stats.finished_at, j.id));
            match failed {
                Some(job) => {
                    let r = r
                        .interact(Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(format!("{e}\nClick to show {}", job.name));
                    if r.clicked() {
                        actions.push(Action::ShowJob(job.id));
                    }
                }
                None => {
                    r.on_hover_text(e);
                }
            }
        } else {
            r.on_hover_ui(|ui| server_health_tip(ui, pal, data));
        }

        // Center the activity group in the bar when there is room, else follow on.
        let gap = (bar.center().x - center_w / 2.0 - ui.cursor().left()).max(16.0);
        ui.add_space(gap);
        let moving = st.running && data.speed() > 0;
        let (glyph, _) = ui.allocate_exact_size(Vec2::new(14.0, 12.0), Sense::hover());
        paint_pulse(ui.painter(), glyph, if moving { pal.accent_fg } else { pal.faint });
        // The reading opens the speed limit menu; it applies to running downloads at once.
        let limit = data.settings.speed_limit_bps();
        let tip = if limit == 0 {
            "No speed limit. Click to set one".to_owned()
        } else {
            format!("Downloads are limited to {}. Click to change", format::speed(limit))
        };
        let r = ui
            .add(egui::Label::new(RichText::new(reading).font(mono.clone()).color(if moving { pal.text } else { pal.muted })).sense(Sense::click()).selectable(false))
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(tip);
        // Clicks inside stay in the menu, so the limit field can be edited.
        egui::Popup::menu(&r)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| speed_menu(ui, pal, data, actions));
        ui.add_space(6.0);
        sparkline(ui, pal, data, reduced_motion);

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if credit(ui, pal).clicked() {
                actions.push(Action::OpenUrl(CREDIT_URL.to_owned()));
            }
            if !rail {
                return;
            }
            ui.add_space(14.0);
            let (text, low) = disk_reading(data);
            let color = if low { pal.warning } else { pal.muted };
            let r = ui.label(RichText::new(text).font(mono.clone()).color(color));
            let (drive, _) = ui.allocate_exact_size(Vec2::new(14.0, 12.0), Sense::hover());
            paint_drive(ui.painter(), drive, color);
            let dir = data.settings.download_dir.display().to_string();
            let total = format!("{} downloaded in total", format::bytes(data.totals.downloaded_bytes));
            if low {
                r.on_hover_text(format!(
                    "Low on space in {dir}: the queue still needs {}\n{total}",
                    format::bytes(data.bytes_left())
                ));
            } else {
                r.on_hover_text(format!("Free space on the filesystem of {dir}\n{total}"));
            }
        });
    });
}

/// The throughput sparkline: the live window (or the stored history) as a soft area
/// under a thin line, scaled to its own peak. Hovering it previews the full graph; a
/// click pins the graph open, where its crosshair and hour cells can be pointed at.
fn sparkline(ui: &mut Ui, pal: &Palette, data: &Data, reduced_motion: bool) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(SPARK_W, 16.0), Sense::click());
    let values = spark_values(data);
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect.expand(1.0));
        if values.len() >= 2 {
            let peak = values.iter().copied().fold(0.0_f32, f32::max).max(1.0);
            let pts: Vec<Pos2> = values
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let x = rect.left() + rect.width() * i as f32 / (values.len() - 1) as f32;
                    let y = rect.bottom() - 1.0 - (rect.height() - 2.0) * (v / peak).clamp(0.0, 1.0);
                    Pos2::new(x, y)
                })
                .collect();
            let mut mesh = egui::epaint::Mesh::default();
            let top = Palette::fade(pal.series, 0.32);
            for w in pts.windows(2) {
                let base = mesh.vertices.len() as u32;
                mesh.colored_vertex(w[0], top);
                mesh.colored_vertex(w[1], top);
                mesh.colored_vertex(Pos2::new(w[1].x, rect.bottom()), pal.transparent);
                mesh.colored_vertex(Pos2::new(w[0].x, rect.bottom()), pal.transparent);
                mesh.add_triangle(base, base + 1, base + 2);
                mesh.add_triangle(base, base + 2, base + 3);
            }
            painter.add(egui::Shape::mesh(mesh));
            painter.add(egui::Shape::line(pts, Stroke::new(1.2, pal.series)));
        } else {
            painter.hline(rect.x_range(), rect.bottom() - 1.0, Stroke::new(1.0, pal.border));
        }
    }
    // Idle seconds (0 B/s samples) are not throughput history.
    let summary = if data.history.is_empty() && data.live.iter().all(|&(_, v)| v == 0) {
        "No throughput history yet".to_owned()
    } else {
        format!(
            "Throughput graph, {} stored samples, {} live",
            data.history.len(),
            data.live.len()
        )
    };
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &summary));
    let graph = |ui: &mut Ui| {
        ui.set_width(GRAPH_POPUP_W);
        ui.allocate_ui(Vec2::new(GRAPH_POPUP_W, 190.0), |ui| {
            ui.set_min_height(190.0);
            super::docks::throughput_graph(ui, pal, data, reduced_motion);
        });
    };
    // A tooltip closes as the pointer leaves the sparkline, so the preview only shows the
    // graph; the pinned popup is the one to point into.
    let pinned = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let resp = if pinned {
        resp
    } else {
        resp.on_hover_ui(|ui| {
            graph(ui);
            ui.label(RichText::new("Click to pin and inspect").small().color(pal.muted));
        })
    };
    egui::Popup::from_toggle_button_response(&resp)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .align(egui::RectAlign::TOP_END)
        .show(graph);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_limit_is_kept_in_kib() {
        assert_eq!(custom_limit_kbps(20.0), 20 * 1024);
        assert_eq!(custom_limit_kbps(2.5), 2560);
        // Never rounds down to 0, which would mean unlimited.
        assert_eq!(custom_limit_kbps(0.0001), 1);
    }

    #[test]
    fn pill_priorities() {
        let mut d = Data::default();
        d.settings.servers[0].host = String::new();
        assert_eq!(pill_state(&d), Pill::NoServer);
        d.settings.servers[0].host = "h".into();
        assert_eq!(pill_state(&d), Pill::Online);
        d.status.last_error = Some("auth failed".into());
        assert_eq!(pill_state(&d), Pill::Error("auth failed".into()));
        d.status.running = true;
        d.status.speed_bps = 2048;
        // The chip names the state; the speed is the center reading's.
        assert_eq!(pill_state(&d).text(), "Downloading");
        assert_eq!(activity_reading(&d), "Downloading  ·  2.0 KB/s");
        // Pause requested while a job still downloads: still live.
        d.status.paused = true;
        assert_eq!(pill_state(&d), Pill::Pausing("2.0 KB/s".into()));
        assert_eq!(pill_state(&d).status(), Status::Live);
        assert_eq!(pill_state(&d).text(), "Pausing");
        // The run has stopped: paused.
        d.status.running = false;
        assert_eq!(pill_state(&d), Pill::Paused);
    }

    #[test]
    fn pending_fraction_counts_unfinished_jobs_only() {
        use newskite_service::Job;
        use std::path::PathBuf;
        let job = |id, state, done, total| Job {
            id,
            name: String::new(),
            state,
            bytes_total: total,
            bytes_done: done,
            dest_dir: PathBuf::new(),
            source_path: PathBuf::new(),
            created_at: 0,
            error: None,
            password: None,
            stats: Default::default(),
        };
        let mut d = Data::default();
        assert_eq!(pending_fraction(&d), 0.0);
        d.jobs = vec![
            job(1, JobState::Done, 100, 100),
            job(2, JobState::Downloading, 25, 100),
            job(3, JobState::Queued, 0, 100),
        ];
        assert!((pending_fraction(&d) - 0.125).abs() < 1e-6);
    }

    #[test]
    fn disk_reading_warns_when_the_queue_does_not_fit() {
        let mut d = Data::default();
        d.totals.downloaded_bytes = 2048;
        assert_eq!(disk_reading(&d), ("free space unknown".to_owned(), false));
        d.disk_free = Some(100 * 1024 * 1024 * 1024);
        let (t, low) = disk_reading(&d);
        assert!(t.starts_with("100 GB free"), "{t}");
        assert!(!low);
        d.disk_free = Some(LOW_DISK - 1);
        assert!(disk_reading(&d).1);
    }

    #[test]
    fn readings_say_what_runs_and_how_fast() {
        let mut d = Data::default();
        assert_eq!(activity_reading(&d), "Idle");
        d.live.push_back((1, 6_400_000));
        d.live.push_back((2, 1_000));
        assert_eq!(activity_reading(&d), format!("Idle  ·  peak {}", format::speed(6_400_000)));
        d.status.running = true;
        // Running: the speed now, and still the peak on record.
        d.live.push_back((3, 0));
        assert_eq!(activity_reading(&d), format!("Downloading  ·  0 B/s  ·  peak {}", format::speed(6_400_000)));
        d.status.paused = true;
        assert!(activity_reading(&d).starts_with("Pausing  ·  "));
        d.status.running = false;
        assert!(activity_reading(&d).starts_with("Paused  ·  peak"));
        // A limit in force replaces the record second.
        d.settings.speed_limit_kbps = 25 * 1024;
        assert_eq!(activity_reading(&d), "Paused  ·  limit 25.0 MB/s");
        d.settings.speed_limit_kbps = 0;

        d.settings.servers[0].host = String::new();
        assert_eq!(servers_reading(&d), None);
        d.settings.servers[0].host = "a".into();
        d.settings.servers[0].tls = true;
        let r = servers_reading(&d).unwrap();
        assert!(r.starts_with("1 server  ·  ") && r.ends_with(" TLS"), "{r}");
        let mut plain = d.settings.servers[0].clone();
        plain.host = "b".into();
        plain.tls = false;
        d.settings.servers.push(plain);
        let r = servers_reading(&d).unwrap();
        assert!(r.starts_with("2 servers") && r.ends_with("conn, TLS + plain"), "{r}");
    }

    #[test]
    fn error_chip_says_what_happened() {
        assert_eq!(
            Pill::Error("auth failed · 481".into()).text(),
            "auth failed · 481"
        );
        assert_eq!(
            Pill::Error("\n  first line\nsecond".into()).text(),
            "first line"
        );
        assert_eq!(Pill::Error(String::new()).text(), "Failed");
        let long = "x".repeat(200);
        let t = Pill::Error(long).text();
        assert_eq!(t.chars().count(), CAUSE_CHARS);
        assert!(t.ends_with('…'));
        assert_ne!(Pill::Error("boom".into()).text(), "Error");
    }
}
