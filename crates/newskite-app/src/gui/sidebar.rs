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
    let rect = Rect::from_min_max(Pos2::new(ui.max_rect().left(), top), Pos2::new(ui.max_rect().right(), bottom));
    if disk_meter(ui, pal, data, rect, rail).clicked() {
        actions.push(Action::OpenDownloadFolder);
    }
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
            icons::paint_svg(painter, icons::WORDMARK_URI, wm, pal.text);
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
        if e.current {
            painter.rect_filled(slot, CornerRadius::same(8), pal.hover);
        } else if hover > 0.0 {
            painter.rect_filled(slot, CornerRadius::same(8), Palette::fade(pal.hover, 0.6 * hover));
        }
        if resp.has_focus() {
            focus_ring(ui, pal, slot, 8);
        }
        let fg = if e.current { pal.text } else { pal.muted.lerp_to_gamma(pal.text, hover) };
        if rail {
            let icon = Rect::from_center_size(slot.center(), Vec2::splat(18.0));
            paint_icon(painter, icon, e.icon, fg);
            if let Some(n) = e.count.as_ref().filter(|n| n.as_str() != "0") {
                // A pill over the icon's top-right corner, cut out of it by a ring in the
                // color behind, so the numerals never sit on the icon's strokes.
                let n = if n.parse::<u64>().is_ok_and(|v| v > 99) { "99+" } else { n.as_str() };
                let (tint, c) = if e.count_warn { (pal.warning_soft, pal.warning) } else { (pal.sunken, fg) };
                let galley = painter.layout_no_wrap(n.to_owned(), fonts::mono_id(9.5), c);
                let size = Vec2::new((galley.size().x + 7.0).max(13.0), 13.0);
                let right = (icon.right() - 4.0 + size.x).min(slot.right() - 2.0);
                let pill = Rect::from_min_size(Pos2::new(right - size.x, slot.top() + 2.0), size);
                let behind = if e.current { pal.hover } else { pal.raised.lerp_to_gamma(pal.hover, 0.6 * hover) };
                let round = CornerRadius::same(7);
                painter.rect_filled(pill.expand(1.5), round, behind);
                painter.rect_filled(pill, round, tint);
                painter.galley(pill.center() - galley.size() / 2.0, galley, c);
            }
        } else {
            paint_icon(painter, Rect::from_center_size(Pos2::new(slot.left() + 20.0, slot.center().y), Vec2::splat(16.0)), e.icon, fg);
            let font = if e.current { fonts::semibold_id(13.5) } else { FontId::proportional(13.5) };
            painter.text(Pos2::new(slot.left() + 38.0, slot.center().y), Align2::LEFT_CENTER, e.label, font, fg);
            if let Some(n) = &e.count {
                let c = if e.count_warn { pal.warning } else if e.current { pal.text } else { pal.muted };
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
