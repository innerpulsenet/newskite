//! The left sidebar: the live mark and the wordmark (About), the views (Queue, History,
//! Log) with their counts, the watch folder and Settings, and the download disk's free
//! space as a meter at the bottom (a click opens the download folder). Below
//! [`RAIL_BELOW`] it narrows to a rail of icons with the same entries.

use eframe::egui::{self, Align2, CornerRadius, FontId, Pos2, Rect, Sense, Ui, Vec2};
use newskite_service::LogLevel;
use newskite_types::format;

use super::app::{Action, Data, Tab, ViewState};
use super::brand;
use super::fonts;
use super::icons;
use super::theme::Palette;
use super::toolbar::masthead_readout;
use super::widgets::{focus_ring, paint_icon, Icon};

/// Width of the sidebar, and of the rail it becomes on narrow windows.
pub const WIDE_W: f32 = 212.0;
pub const RAIL_W: f32 = 60.0;
/// Window width below which the sidebar is a rail.
pub const RAIL_BELOW: f32 = 1100.0;
/// Height of a sidebar entry.
const ITEM_H: f32 = 40.0;
/// Side of the mark beside the wordmark.
const MARK: f32 = 24.0;
/// Width of the wordmark.
const WORDMARK_W: f32 = 112.0;

/// One entry of the sidebar.
struct Entry {
    icon: Icon,
    label: &'static str,
    count: Option<String>,
    /// The count reads as a warning (log problems).
    count_warn: bool,
    current: bool,
    tip: String,
    action: Action,
}

/// Draws the sidebar (`rail` on narrow windows).
pub fn show(ui: &mut Ui, pal: &Palette, data: &Data, view: &ViewState, rail: bool, actions: &mut Vec<Action>) {
    if brand_button(ui, pal, data, view, rail).clicked() {
        actions.push(Action::OpenAbout);
    }
    ui.add_space(14.0);
    let problems = data.logs.iter().filter(|l| l.level != LogLevel::Info).count();
    let watch = &data.settings.watch_folder;
    let watch_set = !watch.as_os_str().is_empty();
    let entries = [
        Entry {
            icon: Icon::Transfers,
            label: "Queue",
            count: Some(data.queue().len().to_string()),
            count_warn: false,
            current: view.tab == Tab::Queue,
            tip: "Jobs waiting, downloading or held".into(),
            action: Action::SetTab(Tab::Queue),
        },
        Entry {
            icon: Icon::StatusOk,
            label: "History",
            count: Some(data.finished_count().to_string()),
            count_warn: false,
            current: view.tab == Tab::History,
            tip: "Finished and failed jobs".into(),
            action: Action::SetTab(Tab::History),
        },
        Entry {
            icon: Icon::Info,
            label: "Log",
            count: (problems > 0).then(|| problems.to_string()),
            count_warn: true,
            current: view.tab == Tab::Log,
            tip: if problems > 0 {
                format!("Messages, newest first; {problems} need{} a look (warnings and errors)", if problems == 1 { "s" } else { "" })
            } else {
                "Messages, newest first".into()
            },
            action: Action::SetTab(Tab::Log),
        },
        Entry {
            icon: Icon::Scan,
            label: "Watch folder",
            count: None,
            count_warn: false,
            current: false,
            tip: if view.scanning {
                "Scanning the watch folder…".into()
            } else if watch_set {
                format!("Import new .nzb files from {} now", watch.display())
            } else {
                "No watch folder is set; choose one in Settings".into()
            },
            action: if watch_set { Action::ScanWatch } else { Action::OpenSettings },
        },
        Entry {
            icon: Icon::Gear,
            label: "Settings",
            count: None,
            count_warn: false,
            current: false,
            tip: format!("Settings ({}+,)", super::widgets::CMD_KEY),
            action: Action::OpenSettings,
        },
    ];
    for e in entries {
        if entry(ui, pal, &e, rail) && !(matches!(e.action, Action::ScanWatch) && view.scanning) {
            actions.push(e.action);
        }
    }
    // The disk meter sits at the bottom of the wide sidebar; beside the rail the status
    // bar has the reading, and a bare bar would say nothing.
    if rail {
        return;
    }
    let bottom = ui.max_rect().bottom();
    let meter_h = if rail { 40.0 } else { 46.0 };
    let top = (bottom - meter_h).max(ui.cursor().top() + 8.0);
    // With materials the wide sidebar carries the instrument's display: throughput in
    // seven-segment figures, where there is room for it between the views and the meter.
    if let (Some(m), false) = (&pal.metal, rail) {
        let lcd_top = top - 14.0 - LCD_H;
        if lcd_top >= ui.cursor().top() + 12.0 {
            let r = Rect::from_min_size(Pos2::new(ui.max_rect().left() + 4.0, lcd_top), Vec2::new(ui.max_rect().width() - 8.0, LCD_H));
            throughput_lcd(ui, pal, m, data, r);
        }
    }
    let rect = Rect::from_min_max(Pos2::new(ui.max_rect().left(), top), Pos2::new(ui.max_rect().right(), bottom));
    if disk_meter(ui, pal, data, rect, rail).clicked() {
        actions.push(Action::OpenDownloadFolder);
    }
}

/// Height of the throughput display.
const LCD_H: f32 = 86.0;

/// The throughput display's figures and unit for `bps` bytes per second: four places,
/// blank-padded, in the units the rest of the window uses (`"  2.4"` `MB/s`, `" 512"` `KB/s`).
pub fn lcd_speed(bps: u64) -> (String, &'static str) {
    const UNITS: [&str; 6] = ["KB/s", "MB/s", "GB/s", "TB/s", "PB/s", "EB/s"];
    if bps < 1024 {
        return (format!("{bps:>4}"), "B/s");
    }
    let mut v = bps as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    // One decimal while it fits in four places.
    if (v * 10.0).round() / 10.0 >= 100.0 {
        (format!("{v:>4.0}"), UNITS[i])
    } else {
        (format!("{v:>5.1}"), UNITS[i])
    }
}

/// The instrument's display, let into the plate: the current download speed in
/// seven-segment figures with its unit, annunciators for what the queue is doing (RX
/// while it runs, HOLD while it is paused), and how much is left to fetch. Every figure
/// is the same one the status bar and the header give in words.
fn throughput_lcd(ui: &mut Ui, pal: &Palette, m: &super::theme::Metal, data: &Data, r: Rect) {
    let resp = ui.interact(r, ui.id().with("throughput-lcd"), Sense::hover());
    let paused = data.status.paused;
    // A job reported as downloading counts as running, as its row shows it.
    let running = data.status.running || data.jobs.iter().any(|j| j.state == newskite_service::JobState::Downloading);
    let bps = if running { data.speed() } else { 0 };
    let left = format::bytes(data.bytes_left());
    if ui.is_rect_visible(r) {
        let painter = ui.painter();
        let lit = m.lcd_lit;
        let dim = super::theme::Metal::lerp(m.lcd_glass, lit, 0.62);
        super::metal::lcd_back(painter, m, r, if running { 1.0 } else { 0.35 });
        let inner = r.shrink2(Vec2::new(10.0, 8.0));
        // Annunciators along the top, the unit at its right end.
        let tag_font = FontId::new(10.0, fonts::mono_semibold());
        let mut x = inner.left();
        for (word, on, color) in [("RX", running && !paused, lit), ("HOLD", paused, m.glow(pal.warning))] {
            let g = painter.layout_no_wrap(word.to_owned(), tag_font.clone(), lit);
            let tag = Rect::from_min_size(Pos2::new(x, inner.top()), Vec2::new(g.size().x + 10.0, 15.0));
            super::metal::lcd_tag(painter, m, tag, g, color, on);
            x = tag.right() + 6.0;
        }
        let (figures, unit) = lcd_speed(bps);
        painter.text(Pos2::new(inner.right(), inner.top() + 7.5), Align2::RIGHT_CENTER, unit, fonts::mono_id(10.5), dim);
        // The figures, right-aligned as a meter's are; dimmer while nothing moves.
        let h = 30.0;
        let w = super::metal::lcd_digits_width(&figures, h);
        let o = Pos2::new(inner.right() - w - h * 0.07, inner.top() + 21.0);
        super::metal::lcd_digits(painter, o, &figures, h, if bps > 0 { lit } else { dim });
        // What is left to fetch.
        let foot = inner.bottom() - 5.0;
        painter.text(Pos2::new(inner.left(), foot), Align2::LEFT_CENTER, "TO GO", fonts::mono_id(10.0), dim);
        painter.text(Pos2::new(inner.right(), foot), Align2::RIGHT_CENTER, &left, FontId::new(11.0, fonts::mono_semibold()), lit);
        super::metal::lcd_glass(painter, r);
    }
    let label = format!("Throughput display: {}, {left} to go", format::speed(bps));
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
    resp.on_hover_text(if paused {
        "The queue is paused"
    } else if running {
        "Download speed now, and how much the queue still has to fetch"
    } else {
        "Nothing is downloading"
    });
}

/// The mark (beating while Newskite works) and, when wide, the wordmark.
fn brand_button(ui: &mut Ui, pal: &Palette, data: &Data, view: &ViewState, rail: bool) -> egui::Response {
    let readout = masthead_readout(&data.settings);
    let (_, _, bw, bh) = icons::WORDMARK_BOX;
    let wm_h = WORDMARK_W * bh / bw;
    let size = Vec2::new(ui.available_width(), 36.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let x = if rail { rect.center().x - MARK / 2.0 } else { rect.left() + 10.0 };
        let mark = Rect::from_min_size(Pos2::new(x, rect.center().y - MARK / 2.0), Vec2::splat(MARK));
        brand::paint_mark(painter, mark, &view.masthead_mark, pal.mark);
        if !rail {
            let wm = Rect::from_min_size(
                Pos2::new(mark.right() + 9.0, (rect.center().y - wm_h / 2.0).round()),
                Vec2::new(WORDMARK_W, wm_h),
            );
            match &pal.metal {
                // The chrome lockup: the flat mark beside the wordmark in brushed steel.
                Some(m) => super::metal::steel_svg(painter, pal, m, icons::WORDMARK_URI, wm),
                None => icons::paint_svg(painter, icons::WORDMARK_URI, wm, pal.text),
            }
        }
        if resp.has_focus() {
            focus_ring(ui, pal, rect, 6);
        }
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Newskite · {readout}")));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("About Newskite\n{readout}"))
}

/// One entry: its row (or icon on the rail), current marked with the hover tint, a
/// count at the right end. Returns whether it was clicked.
fn entry(ui: &mut Ui, pal: &Palette, e: &Entry, rail: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ITEM_H), Sense::click());
    let accessible = match &e.count {
        Some(n) if !e.count_warn => format!("{} ({n})", e.label),
        Some(n) => format!("{} ({n} warnings or errors)", e.label),
        None => e.label.to_string(),
    };
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool(resp.id.with("hover"), resp.hovered() && !e.current);
        let painter = ui.painter();
        // On the rail the slot is a square, with room in its corner for the count.
        let slot = rect.shrink2(Vec2::new(if rail { 2.0 } else { 0.0 }, 2.0));
        if let Some(m) = &pal.metal {
            // The views are keys: the current one latched and lit, the others rising out
            // of the plate under the pointer.
            let k = super::metal::KeyState { hover: 1.0, on: if e.current { 1.0 } else { 0.0 }, press: 0.0, color: pal.accent };
            if e.current {
                super::metal::key_body(painter, m, slot.shrink(1.0), 4, k);
            } else if hover > 0.0 {
                let mut p = painter.clone();
                p.multiply_opacity(hover);
                super::metal::key_body(&p, m, slot.shrink(1.0), 4, k);
            }
        } else if e.current {
            painter.rect_filled(slot, CornerRadius::same(8), pal.hover);
        } else if hover > 0.0 {
            painter.rect_filled(slot, CornerRadius::same(8), Palette::fade(pal.hover, 0.6 * hover));
        }
        if resp.has_focus() {
            focus_ring(ui, pal, slot, 8);
        }
        // On aluminum the current view's key is lit glass on a pale plate: what is written
        // on it takes the key's light, not the plate's ink.
        let glass = pal.metal.filter(|m| m.light && e.current);
        let fg = match &glass {
            Some(m) => super::metal::key_text(pal, m, super::metal::KeyState { hover: 0.0, on: 1.0, press: 0.0, color: pal.accent }, true),
            None if e.current => pal.text,
            None => pal.muted.lerp_to_gamma(pal.text, hover),
        };
        let warn = glass.map_or(pal.warning, |m| m.glow(pal.warning));
        if rail {
            let icon = Rect::from_center_size(slot.center(), Vec2::splat(18.0));
            paint_icon(painter, icon, e.icon, fg);
            if let Some(n) = e.count.as_ref().filter(|n| n.as_str() != "0") {
                // A pill over the icon's top-right corner, cut out of it by a ring in the
                // color behind, so the numerals never sit on the icon's strokes.
                let n = if n.parse::<u64>().is_ok_and(|v| v > 99) { "99+" } else { n.as_str() };
                let (tint, c) = match (&glass, e.count_warn) {
                    (Some(m), true) => (m.glass[1], warn),
                    (Some(m), false) => (m.glass[1], fg),
                    (None, true) => (pal.warning_soft, pal.warning),
                    (None, false) => (pal.metal.filter(|m| m.light).map_or(pal.sunken, |m| m.rail), fg),
                };
                let galley = painter.layout_no_wrap(n.to_owned(), fonts::mono_id(9.5), c);
                let size = Vec2::new((galley.size().x + 7.0).max(13.0), 13.0);
                let right = (icon.right() - 4.0 + size.x).min(slot.right() - 2.0);
                let pill = Rect::from_min_size(Pos2::new(right - size.x, slot.top() + 2.0), size);
                let behind = match &glass {
                    Some(m) => m.glass[0],
                    None if e.current => pal.hover,
                    // With materials the rail is the plate itself, and a key rises under
                    // the pointer.
                    None => match &pal.metal {
                        Some(m) => pal.surface.lerp_to_gamma(m.key[0], hover),
                        None => pal.raised.lerp_to_gamma(pal.hover, 0.6 * hover),
                    },
                };
                let round = CornerRadius::same(7);
                painter.rect_filled(pill.expand(1.5), round, behind);
                painter.rect_filled(pill, round, tint);
                painter.galley(pill.center() - galley.size() / 2.0, galley, c);
            }
        } else {
            paint_icon(painter, Rect::from_center_size(Pos2::new(slot.left() + 20.0, slot.center().y), Vec2::splat(16.0)), e.icon, fg);
            let font = if e.current { fonts::semibold_id(13.5) } else { FontId::proportional(13.5) };
            painter.text(Pos2::new(slot.left() + 38.0, slot.center().y), Align2::LEFT_CENTER, e.label, font, fg);
            if let Some(n) = e.count.as_ref().filter(|n| n.as_str() != "0") {
                let c = if e.count_warn { warn } else if e.current { fg } else { pal.muted };
                painter.text(Pos2::new(slot.right() - 12.0, slot.center().y), Align2::RIGHT_CENTER, n, fonts::mono_id(12.0), c);
            }
        }
    }
    let current = e.current;
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, current, &accessible));
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let resp = if rail { resp.on_hover_text(format!("{accessible}\n{}", e.tip)) } else { resp.on_hover_text(&e.tip) };
    resp.clicked()
}

/// The download disk: free space in words and a meter of how full the disk is. Turns to
/// the warning color when space is short of what the queue needs.
fn disk_meter(ui: &mut Ui, pal: &Palette, data: &Data, rect: Rect, rail: bool) -> egui::Response {
    let resp = ui.interact(rect, ui.id().with("disk-meter"), Sense::click());
    let (text, low) = super::statusbar::disk_reading(data);
    let color = if low { pal.warning } else { pal.muted };
    let used = match (data.disk_free, data.disk_total) {
        (Some(free), Some(total)) if total > 0 => Some(1.0 - free as f32 / total as f32),
        _ => None,
    };
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let inner = rect.shrink2(Vec2::new(if rail { 10.0 } else { 12.0 }, 6.0));
        if !rail {
            painter.text(inner.left_top(), Align2::LEFT_TOP, "Downloads disk", FontId::proportional(12.0), pal.muted);
            painter.text(inner.right_top(), Align2::RIGHT_TOP, &text, fonts::mono_id(11.5), color);
        }
        let bar = Rect::from_min_size(Pos2::new(inner.left(), inner.bottom() - 6.0), Vec2::new(inner.width(), 6.0));
        painter.rect_filled(bar, CornerRadius::same(3), pal.well);
        if let Some(u) = used {
            let mut fill = bar;
            fill.set_width((bar.width() * u.clamp(0.0, 1.0)).max(3.0));
            painter.rect_filled(fill, CornerRadius::same(3), if low { pal.warning } else { pal.faint });
        }
    }
    let dir = data.settings.download_dir.display().to_string();
    let total = format!("{} downloaded in total", format::bytes(data.totals.downloaded_bytes));
    let tip = match data.disk_total {
        Some(t) => format!("{text} of {} on the disk of {dir}\n{total}\nClick to open the download folder", format::bytes(t)),
        None => format!("{text} on the disk of {dir}\n{total}\nClick to open the download folder"),
    };
    let label = format!("Downloads disk: {text}");
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_display_keeps_four_places_in_the_window_units() {
        assert_eq!(lcd_speed(0), ("   0".to_owned(), "B/s"));
        assert_eq!(lcd_speed(1023), ("1023".to_owned(), "B/s"));
        assert_eq!(lcd_speed(1024), ("  1.0".to_owned(), "KB/s"));
        assert_eq!(lcd_speed(1024 * 1024 * 24 + 419_430), (" 24.4".to_owned(), "MB/s"));
        // 99.96 would round to five places with a decimal: it drops the decimal instead.
        assert_eq!(lcd_speed((99.96 * 1024.0 * 1024.0) as u64), (" 100".to_owned(), "MB/s"));
        assert_eq!(lcd_speed(512 * 1024 * 1024), (" 512".to_owned(), "MB/s"));
        for bps in [0, 1, 999, 1024, 1_500_000, 123_456_789, u64::MAX] {
            let (figures, _) = lcd_speed(bps);
            assert_eq!(figures.chars().filter(|c| *c != '.').count(), 4, "{figures:?}");
        }
    }
}
