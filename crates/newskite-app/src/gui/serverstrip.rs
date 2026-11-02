//! Server strip: one quiet line above the status bar for the part of the window that
//! rarely changes. Each usable server in fail-over order (health glyph, address, TLS,
//! role, what it delivered and lacked this session), the connection slots, and at the
//! right end "Retry on backup" for every failed download.
//!
//! Nothing here may overlap: the strip is laid out from measured widths, left to right,
//! and drops detail level by level (counts, roles, ports, then the backups) until it fits;
//! the last level is clipped, never drawn over the button. Every reading keeps its full
//! form in a tooltip.

use eframe::egui::{self, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};
use newskite_service::{Job, JobState};
use newskite_types::{ServerConfig, ServerHealth};

use super::app::{Action, Data};
use super::fonts;
use super::icons::Icon;
use super::theme::Palette;
use super::widgets::{paint_icon, spaced};

/// Height of the strip panel.
pub const STRIP_H: f32 = 30.0;
/// Width of the connection slot gauge.
const GAUGE_W: f32 = 56.0;
const GAUGE_H: f32 = 8.0;
/// Space around a separator hairline.
const SEP_GAP: f32 = 12.0;
/// Height of the strip's buttons.
const BUTTON_H: f32 = 22.0;
/// Errors this recent make a server "struggling" (seconds).
const RECENT_ERROR_SECS: i64 = 300;

/// Strip UI state.
#[derive(Default)]
pub struct StripState {
    /// The retry-all confirmation is showing in place of the server readings.
    pub confirm_retry_all: bool,
}

/// What one server looks like this session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// Not used yet, or delivering without recent errors.
    Ok,
    /// Errors in the last few minutes.
    Errors,
    /// Login refused.
    Refused,
}

/// The health of `h` at unix time `now` (`None`: not used this session).
pub fn health_of(h: Option<&ServerHealth>, now: i64) -> Health {
    match h {
        Some(h) if h.unusable.is_some() => Health::Refused,
        Some(h) if h.last_error.as_ref().is_some_and(|(t, _)| now - t < RECENT_ERROR_SECS) => Health::Errors,
        _ => Health::Ok,
    }
}

/// How much of the strip is spelled out; higher levels drop detail to fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Detail {
    /// Address with port, TLS, role, articles and missing counts.
    Full,
    /// No role tags, no article counts (missing stays).
    NoCounts,
    /// Host names without ports.
    Hosts,
    /// The primary alone, the backups as "+N backup".
    PrimaryOnly,
}

impl Detail {
    pub const ALL: [Detail; 4] = [Detail::Full, Detail::NoCounts, Detail::Hosts, Detail::PrimaryOnly];
}

/// A run of the strip: drawn as one unit, sharing one tooltip.
struct Group {
    pieces: Vec<Piece>,
    /// Accessible name.
    label: String,
    tip: String,
}

enum Piece {
    Text(String, FontId, Color32),
    /// Spaced uppercase eyebrow text.
    Tag(String, Color32),
    Glyph(Icon, Color32),
    Gauge,
    Space(f32),
}

/// `1234567` → `"1,234,567"`.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The failed jobs, oldest finish first (the order a retry-all queues them).
pub fn failed_jobs(data: &Data) -> Vec<&Job> {
    let mut v: Vec<&Job> = data.jobs.iter().filter(|j| j.state == JobState::Failed).collect();
    v.sort_by_key(|j| (j.stats.finished_at, j.id));
    v
}

/// Plain-language account of what a backup retry does, for buttons' tooltips.
pub fn backup_retry_explainer(servers: &[ServerConfig]) -> String {
    match servers {
        [] | [_] => "No fail-over server is configured. Add a second server in Settings › Servers to retry on a backup.".into(),
        [primary, backups @ ..] => {
            let names: Vec<String> = backups.iter().map(ServerConfig::address).collect();
            format!(
                "Downloads the incomplete files again, asking {} first for every article and {} only for what {} lacks. \
                 Completed files are kept. The result is written to the log.",
                names.join(", then "),
                primary.address(),
                if backups.len() == 1 { "it" } else { "they" },
            )
        }
    }
}

fn piece_width(ui: &Ui, p: &Piece) -> f32 {
    match p {
        Piece::Text(t, f, c) => ui.painter().layout_no_wrap(t.clone(), f.clone(), *c).size().x,
        Piece::Tag(t, c) => ui.painter().layout_job(spaced(&t.to_uppercase(), fonts::mono_id(9.0), *c, 0.9)).size().x,
        Piece::Glyph(..) => 12.0,
        Piece::Gauge => GAUGE_W,
        Piece::Space(w) => *w,
    }
}

fn group_width(ui: &Ui, g: &Group) -> f32 {
    g.pieces.iter().map(|p| piece_width(ui, p)).sum()
}

fn row_width(ui: &Ui, groups: &[Group]) -> f32 {
    let seps = groups.len().saturating_sub(1) as f32 * (2.0 * SEP_GAP + 1.0);
    groups.iter().map(|g| group_width(ui, g)).sum::<f32>() + seps
}

fn health_glyph(pal: &Palette, h: Health) -> Piece {
    match h {
        Health::Ok => Piece::Glyph(Icon::StatusOk, pal.positive),
        Health::Errors => Piece::Glyph(Icon::StatusWarn, pal.warning),
        Health::Refused => Piece::Glyph(Icon::StatusErr, pal.negative),
    }
}

fn server_group(pal: &Palette, cfg: &ServerConfig, h: Option<&ServerHealth>, primary: bool, detail: Detail, now: i64) -> Group {
    let health = health_of(h, now);
    let mut pieces = vec![health_glyph(pal, health), Piece::Space(6.0)];
    let name = if detail >= Detail::Hosts { cfg.host.trim().to_owned() } else { cfg.address() };
    pieces.push(Piece::Text(name, fonts::mono_id(11.5), pal.text));
    pieces.push(Piece::Space(7.0));
    pieces.push(if cfg.tls { Piece::Tag("TLS".into(), pal.positive) } else { Piece::Tag("Plain".into(), pal.warning) });
    if detail == Detail::Full {
        pieces.push(Piece::Space(7.0));
        pieces.push(Piece::Tag(if primary { "Primary".into() } else { "Backup".into() }, pal.faint));
    }
    let (articles, missing, errors) = h.map_or((0, 0, 0), |h| (h.articles, h.missing, h.errors));
    let mut counts = Vec::new();
    if detail == Detail::Full && articles > 0 {
        counts.push(Piece::Text(format!("{} art", thousands(articles)), fonts::mono_id(11.0), pal.muted));
    }
    if missing > 0 {
        if !counts.is_empty() {
            counts.push(Piece::Text(" · ".into(), fonts::mono_id(11.0), pal.faint));
        }
        counts.push(Piece::Text(format!("{} missing", thousands(missing)), fonts::mono_id(11.0), pal.warning));
    }
    if errors > 0 && detail <= Detail::NoCounts {
        if !counts.is_empty() {
            counts.push(Piece::Text(" · ".into(), fonts::mono_id(11.0), pal.faint));
        }
        counts.push(Piece::Text(format!("{} err", thousands(errors)), fonts::mono_id(11.0), pal.warning));
    }
    if !counts.is_empty() {
        pieces.push(Piece::Space(10.0));
        pieces.extend(counts);
    }
    let state = match health {
        Health::Ok if h.is_none() => "not used yet this session",
        Health::Ok => "ok",
        Health::Errors => "errors in the last 5 minutes",
        Health::Refused => "login refused",
    };
    let mut tip = format!(
        "{} ({}, {}, user {})\n{state}\n{} articles delivered, {} not on this server, {} errors this session",
        cfg.address(),
        if primary { "primary" } else { "fail-over" },
        if cfg.tls { "TLS" } else { "plain text" },
        if cfg.username.is_empty() { "(none)" } else { cfg.username.as_str() },
        thousands(articles),
        thousands(missing),
        thousands(errors),
    );
    if let Some(h) = h {
        if let Some(why) = &h.unusable {
            tip.push_str(&format!("\nRefused: {why}"));
        }
        if let Some((t, e)) = &h.last_error {
            tip.push_str(&format!("\nLast error at {}: {e}", super::docks::clock(*t)));
        }
    }
    Group { pieces, label: cfg.address(), tip }
}

fn groups_for(pal: &Palette, data: &Data, servers: &[ServerConfig], detail: Detail, now: i64) -> Vec<Group> {
    let health = |cfg: &ServerConfig| data.servers.iter().find(|h| h.address == cfg.address());
    let mut groups = Vec::new();
    for (i, cfg) in servers.iter().enumerate() {
        if i > 0 && detail == Detail::PrimaryOnly {
            let rest = servers.len() - 1;
            let missing: u64 = servers[1..].iter().filter_map(health).map(|h| h.missing).sum();
            let mut pieces = vec![Piece::Text(format!("+{rest} backup"), fonts::mono_id(11.0), pal.muted)];
            if missing > 0 {
                pieces.push(Piece::Text(format!(" · {} missing", thousands(missing)), fonts::mono_id(11.0), pal.warning));
            }
            let names: Vec<String> = servers[1..].iter().map(ServerConfig::address).collect();
            groups.push(Group { pieces, label: format!("+{rest} backup"), tip: format!("Fail-over: {}", names.join(", ")) });
            break;
        }
        groups.push(server_group(pal, cfg, health(cfg), i == 0, detail, now));
    }
    let s = &data.settings;
    let (effective, max) = (s.effective_connections(), s.max_connections.max(1) as usize);
    let mut pieces = Vec::new();
    if detail < Detail::PrimaryOnly {
        pieces.push(Piece::Tag("Conn".into(), pal.faint));
        pieces.push(Piece::Space(8.0));
        pieces.push(Piece::Gauge);
        pieces.push(Piece::Space(8.0));
    }
    pieces.push(Piece::Text(format!("{effective} / {max}"), fonts::mono_id(11.5), pal.text));
    groups.push(Group {
        pieces,
        label: format!("{effective} / {max}"),
        tip: format!("{effective} of {max} connections per run (capped by the servers' own connection counts)"),
    });
    groups
}

/// Paints `groups` left to right from `x` along the strip's center line; the caller's
/// clip keeps the last level from reaching the right-hand controls.
fn paint_groups(ui: &mut Ui, pal: &Palette, data: &Data, groups: Vec<Group>, rect: Rect) {
    let y = rect.center().y;
    let mut x = rect.left();
    let lit = data.status.running && !data.status.paused;
    let painter = ui.painter_at(rect);
    for (gi, g) in groups.into_iter().enumerate() {
        if gi > 0 {
            x += SEP_GAP;
            painter.line_segment([Pos2::new(x, y - 8.0), Pos2::new(x, y + 8.0)], Stroke::new(1.0, pal.border));
            x += 1.0 + SEP_GAP;
        }
        let start = x;
        for p in &g.pieces {
            let w = piece_width(ui, p);
            match p {
                Piece::Text(t, f, c) => {
                    painter.text(Pos2::new(x, y), egui::Align2::LEFT_CENTER, t, f.clone(), *c);
                }
                Piece::Tag(t, c) => {
                    let galley = painter.layout_job(spaced(&t.to_uppercase(), fonts::mono_id(9.0), *c, 0.9));
                    painter.galley(Pos2::new(x, y - galley.size().y / 2.0), galley, *c);
                }
                Piece::Glyph(icon, c) => {
                    paint_icon(&painter, Rect::from_center_size(Pos2::new(x + 6.0, y), Vec2::splat(12.0)), *icon, *c);
                }
                Piece::Gauge => {
                    let s = &data.settings;
                    let (cells, lit_cells) = super::docks::slot_cells(
                        s.max_connections.max(1) as usize,
                        s.effective_connections(),
                        GAUGE_W,
                    );
                    let gap = 1.5;
                    let cw = (GAUGE_W - gap * (cells as f32 - 1.0)) / cells as f32;
                    for i in 0..cells {
                        let cell = Rect::from_min_size(
                            Pos2::new(x + i as f32 * (cw + gap), y - GAUGE_H / 2.0),
                            Vec2::new(cw, GAUGE_H),
                        );
                        let color = if i >= lit_cells {
                            pal.well
                        } else if lit {
                            pal.series
                        } else {
                            pal.muted
                        };
                        painter.rect_filled(cell, CornerRadius::same(1), color);
                    }
                }
                Piece::Space(_) => {}
            }
            x += w;
        }
        let grect = Rect::from_min_max(Pos2::new(start, y - 9.0), Pos2::new(x.min(rect.right()), y + 9.0));
        if grect.width() > 0.0 {
            let r = ui.interact(grect, ui.id().with(("strip-group", gi)), Sense::hover());
            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &g.label));
            r.on_hover_text(g.tip);
        }
    }
}

/// A compact outline button sized for the strip (mono label, optional icon).
pub fn strip_button(ui: &mut Ui, pal: &Palette, icon: Option<Icon>, text: &str, primary: bool, enabled: bool) -> egui::Response {
    let font = fonts::mono_id(11.0);
    let w = strip_button_width(ui, icon.is_some(), text);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, BUTTON_H), if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let hovered = enabled && resp.hovered();
        let (fill, stroke, fg) = match (primary && enabled, hovered) {
            (true, h) => (if h { pal.action_hover } else { pal.action }, pal.action, pal.action_text),
            (false, h) => (
                if resp.is_pointer_button_down_on() { pal.sunken } else { pal.transparent },
                if h { pal.border_strong } else { pal.border },
                if enabled { pal.text } else { Palette::fade(pal.text, 0.45) },
            ),
        };
        let p = ui.painter();
        p.rect(rect, CornerRadius::same(4), fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
        let mut x = rect.left() + 8.0;
        if let Some(icon) = icon {
            paint_icon(p, Rect::from_center_size(Pos2::new(x + 7.0, rect.center().y), Vec2::splat(14.0)), icon, fg);
            x += 14.0 + 6.0;
        }
        p.text(Pos2::new(x, rect.center().y), egui::Align2::LEFT_CENTER, text, font, fg);
        if resp.has_focus() {
            super::widgets::focus_ring(ui, pal, rect, 4);
        }
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, text));
    resp
}

fn strip_button_width(ui: &Ui, icon: bool, text: &str) -> f32 {
    let t = ui.painter().layout_no_wrap(text.to_owned(), fonts::mono_id(11.0), ui.visuals().text_color()).size().x;
    t + 16.0 + if icon { 20.0 } else { 0.0 }
}

/// Draws the strip.
pub fn show(ui: &mut Ui, pal: &Palette, data: &Data, state: &mut StripState, actions: &mut Vec<Action>) {
    let servers = data.settings.usable_servers();
    let failed = failed_jobs(data);
    let has_backup = servers.len() >= 2;
    if failed.is_empty() {
        state.confirm_retry_all = false;
    }
    let full = ui.max_rect();
    let now = super::docks::now_secs();

    if state.confirm_retry_all && has_backup {
        confirm_row(ui, pal, &servers, failed.len(), full, state, actions);
        return;
    }

    // Right end: the retry-all button, when something failed.
    let mut left = full;
    if !failed.is_empty() {
        let label = format!("Retry {} on backup", failed.len());
        let w = strip_button_width(ui, true, &label);
        let r = Rect::from_min_size(Pos2::new(full.right() - w, full.center().y - BUTTON_H / 2.0), Vec2::new(w, BUTTON_H));
        let resp = ui
            .scope_builder(egui::UiBuilder::new().max_rect(r), |ui| strip_button(ui, pal, Some(Icon::RetryBackup), &label, false, has_backup))
            .inner;
        let tip = if has_backup {
            format!(
                "Retry {} failed download{} on the fail-over server. {}",
                failed.len(),
                if failed.len() == 1 { "" } else { "s" },
                backup_retry_explainer(&servers)
            )
        } else {
            backup_retry_explainer(&servers)
        };
        if resp.on_hover_text(tip).clicked() {
            state.confirm_retry_all = true;
        }
        left.max.x = r.left() - SEP_GAP;
    }

    if servers.is_empty() {
        let r = Rect::from_min_size(left.min, Vec2::new(left.width(), full.height()));
        ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| {
            ui.horizontal_centered(|ui| {
                ui.label(egui::RichText::new("No server configured").font(fonts::mono_id(11.5)).color(pal.warning));
                if strip_button(ui, pal, Some(Icon::Gear), "Open settings…", false, true).clicked() {
                    actions.push(Action::OpenSettings);
                }
            });
        });
        return;
    }

    let avail = left.width().max(0.0);
    let mut chosen = Detail::PrimaryOnly;
    for d in Detail::ALL {
        if row_width(ui, &groups_for(pal, data, &servers, d, now)) <= avail {
            chosen = d;
            break;
        }
    }
    let groups = groups_for(pal, data, &servers, chosen, now);
    paint_groups(ui, pal, data, groups, left);
}

/// The retry-all confirmation, in place of the readings: says exactly what will happen.
fn confirm_row(
    ui: &mut Ui,
    pal: &Palette,
    servers: &[ServerConfig],
    n: usize,
    full: Rect,
    state: &mut StripState,
    actions: &mut Vec<Action>,
) {
    let backup = servers[1].address();
    let primary = servers[0].address();
    let jobs = format!("{n} failed download{}", if n == 1 { "" } else { "s" });
    let go = format!("Retry {n} on backup");
    let buttons_w = strip_button_width(ui, true, &go) + strip_button_width(ui, false, "Cancel") + 8.0;
    let text_w = (full.width() - buttons_w - SEP_GAP).max(0.0);
    // The longest sentence that fits.
    let candidates = [
        format!("Retry {jobs}: incomplete files download again from {backup} first; {primary} only fills what it lacks. Completed files are kept."),
        format!("Retry {jobs}: {backup} first, {primary} only for what it lacks. Completed files are kept."),
        format!("Retry {jobs} with {backup} first?"),
        format!("Retry {jobs}?"),
    ];
    let font = fonts::mono_id(11.5);
    let sentence = candidates
        .iter()
        .find(|c| ui.painter().layout_no_wrap((*c).clone(), font.clone(), pal.text).size().x <= text_w)
        .unwrap_or(&candidates[3])
        .clone();
    let painter = ui.painter_at(Rect::from_min_size(full.min, Vec2::new(text_w, full.height())));
    painter.text(Pos2::new(full.left(), full.center().y), egui::Align2::LEFT_CENTER, &sentence, font, pal.text);
    let r = ui.interact(
        Rect::from_min_size(full.min, Vec2::new(text_w, full.height())),
        ui.id().with("retry-all-sentence"),
        Sense::hover(),
    );
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &sentence));
    r.on_hover_text(backup_retry_explainer(servers));
    let br = Rect::from_min_max(Pos2::new(full.right() - buttons_w, full.top()), full.max);
    ui.scope_builder(egui::UiBuilder::new().max_rect(br), |ui| {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if strip_button(ui, pal, Some(Icon::RetryBackup), &go, true, true).clicked() {
                state.confirm_retry_all = false;
                actions.push(Action::RetryAllOnBackup);
            }
            if strip_button(ui, pal, None, "Cancel", false, true).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                state.confirm_retry_all = false;
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }

    #[test]
    fn explainer_names_the_order() {
        let a = ServerConfig { host: "a.example".into(), port: 563, ..Default::default() };
        let b = ServerConfig { host: "b.example".into(), port: 563, ..Default::default() };
        assert!(backup_retry_explainer(std::slice::from_ref(&a)).contains("No fail-over server"));
        let t = backup_retry_explainer(&[a, b]);
        assert!(t.contains("asking b.example:563 first"), "{t}");
        assert!(t.contains("a.example:563 only for what it lacks"), "{t}");
    }

    #[test]
    fn health_levels() {
        let mut h = ServerHealth { address: "x".into(), ..Default::default() };
        assert_eq!(health_of(None, 1000), Health::Ok);
        assert_eq!(health_of(Some(&h), 1000), Health::Ok);
        h.last_error = Some((900, "timeout".into()));
        assert_eq!(health_of(Some(&h), 1000), Health::Errors);
        assert_eq!(health_of(Some(&h), 900 + RECENT_ERROR_SECS), Health::Ok);
        h.unusable = Some("bad password".into());
        assert_eq!(health_of(Some(&h), 1000), Health::Refused);
    }
}
