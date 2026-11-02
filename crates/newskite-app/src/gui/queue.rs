//! Center area: the Queue and History tabs.
//!
//! The Queue tab lists pending jobs (queued, downloading, held) in the order a run
//! downloads them; every row has its own start/pause toggle, a menu (move, password,
//! folder, remove) and a context menu with the same entries. History lists finished jobs
//! newest first. A click selects a row; its details show in the Details dock.

use std::collections::HashMap;

use eframe::egui::{self, Align, Id, Layout, RichText, Sense, TextEdit, Ui};
use egui_extras::{Column, TableBuilder};
use newskite_service::{Job, JobMove, JobState};
use newskite_types::format;

use super::app::{Action, Data, Tab, ViewState};
use super::brand;
use super::fonts;
use super::motion::Beat;
use super::theme::Palette;
use super::widgets::{
    article_grid, icon_button, kit_button, paint_icon, segment_track_live, spaced, state_badge,
    ButtonKind, Icon, EYEBROW_TRACKING,
};

/// Seconds the just-imported job's name stays in the accent before settling.
const HIGHLIGHT_SECS: f64 = 2.5;
/// Seconds a row's finish (done or failed) wash takes to fade.
const FLASH_SECS: f64 = 1.6;
/// Seconds the tab underline takes to slide to a newly selected tab.
const TAB_SLIDE_SECS: f32 = 0.18;

/// Seconds left at the current speed for `remaining` bytes, if a speed is known.
pub fn eta_secs(remaining: u64, speed_bps: u64) -> Option<u64> {
    if speed_bps == 0 {
        return None;
    }
    Some(remaining.div_ceil(speed_bps))
}

/// Exact empty-state text of the Queue tab.
pub const EMPTY_QUEUE: &str = "The queue is empty. Add an NZB to begin.";
/// Exact empty-state text of the History tab.
pub const EMPTY_HISTORY: &str = "Nothing has finished yet.";

/// Queue table column widths (logical px). Only Name and Progress give way on narrow
/// windows; Controls keeps room for the "Remove?" Yes/No confirmation and the inline
/// archive-password field.
const POS_W: f32 = 30.0;
const STATE_W: f32 = 108.0;
const SIZE_W: f32 = 78.0;
const CONTROLS_W: f32 = 160.0;
const NAME_MIN: f32 = 150.0;
const PROGRESS_MIN: f32 = 110.0;
const PROGRESS_MAX: f32 = 240.0;
/// History table: actions column.
const HISTORY_ACTIONS_W: f32 = 160.0;
/// Width of the name filter field.
const FILTER_W: f32 = 220.0;

/// Progress column width and the table's minimum width for `available` px with
/// `gap` px between columns. When the minimum exceeds `available`, the table scrolls
/// sideways instead of being clipped.
pub fn table_widths(available: f32, gap: f32) -> (f32, f32) {
    let fixed = POS_W + STATE_W + SIZE_W + CONTROLS_W + 5.0 * gap;
    let progress = (available - fixed - NAME_MIN).clamp(PROGRESS_MIN, PROGRESS_MAX);
    (progress, fixed + NAME_MIN + progress)
}

/// Center-area UI state.
#[derive(Default)]
pub struct QueueState {
    /// The selected job (its details show in the Details dock).
    pub selected: Option<i64>,
    /// Job awaiting removal confirmation.
    pub confirm_remove: Option<i64>,
    /// Finished job whose folder deletion awaits confirmation.
    pub confirm_delete: Option<i64>,
    /// "Clear history" awaiting confirmation.
    pub confirm_clear: bool,
    /// "Delete files too" (from the Clear history confirmation) awaiting confirmation.
    pub confirm_clear_files: bool,
    /// Job whose archive password is being edited, with the text typed so far.
    pub password_edit: Option<(i64, String)>,
    /// Focus the filter field next frame (Ctrl+F).
    pub focus_filter: bool,
    /// Most recently imported job (briefly highlighted).
    pub highlight: Option<i64>,
    /// When the highlight began (egui time), set on its first frame.
    pub highlight_at: Option<f64>,
    /// Each job's state as last drawn, to notice finishes.
    pub last_states: HashMap<i64, JobState>,
    /// Rows that just finished: the state reached and when (egui time).
    pub flashes: HashMap<i64, (JobState, f64)>,
}

impl QueueState {
    /// Notices jobs that reached done or failed since the last frame and starts their
    /// wash; forgets rows that are gone. Pure bookkeeping, so tests can drive it.
    pub fn note_finishes(&mut self, jobs: &[Job], now: f64) {
        for j in jobs {
            let was = self.last_states.insert(j.id, j.state);
            if let Some(was) = was {
                if was != j.state && matches!(j.state, JobState::Done | JobState::Failed) {
                    self.flashes.insert(j.id, (j.state, now));
                }
            }
        }
        self.last_states
            .retain(|id, _| jobs.iter().any(|j| j.id == *id));
        self.flashes
            .retain(|id, (_, at)| jobs.iter().any(|j| j.id == *id) && now - *at < FLASH_SECS);
    }

    /// 1 → 0 over [`FLASH_SECS`] for a row that just finished, with its state.
    pub fn flash(&self, id: i64, now: f64) -> Option<(JobState, f32)> {
        let (state, at) = self.flashes.get(&id)?;
        let t = ((now - at) / FLASH_SECS).clamp(0.0, 1.0) as f32;
        Some((*state, 1.0 - t))
    }
}

/// `true` when `name` matches the filter text (case-insensitive substring; an empty
/// filter matches everything).
pub fn matches_filter(name: &str, filter: &str) -> bool {
    let f = filter.trim();
    f.is_empty() || name.to_lowercase().contains(&f.to_lowercase())
}

/// The row's start/pause toggle: what it does for `job` right now, if anything.
///
/// A downloading job pauses (held); a queued job that the active run will download is
/// held; a queued job outside any run, or a held one, starts on its own (the rest of the
/// queue stays put); a failed job is retried. Done jobs have no toggle.
pub fn toggle_action(data: &Data, job: &Job) -> Option<Action> {
    match job.state {
        JobState::Downloading => Some(Action::PauseJob(job.id)),
        JobState::Queued if data.in_run(job) => Some(Action::PauseJob(job.id)),
        JobState::Queued | JobState::Paused => Some(Action::StartJob(job.id)),
        JobState::Failed => Some(Action::RetryJob(job.id)),
        JobState::Done => None,
    }
}

/// Icon and tooltip of the toggle for `job` (see [`toggle_action`]).
fn toggle_look(data: &Data, job: &Job) -> Option<(Icon, &'static str)> {
    Some(match job.state {
        JobState::Downloading => (
            Icon::Pause,
            "Pause this download (completed files are kept)",
        ),
        JobState::Queued if data.in_run(job) => (Icon::Pause, "Hold: skip this job for now"),
        JobState::Queued => (
            Icon::Play,
            "Download this job now (the rest of the queue stays put)",
        ),
        JobState::Paused => (Icon::Play, "Resume this job"),
        JobState::Failed => (Icon::Retry, "Retry (completed files are kept)"),
        JobState::Done => return None,
    })
}

/// The one-line summary over the queue table: count, bytes left and, with a measured
/// speed, the time left. Never an estimate without a speed.
pub fn queue_summary(data: &Data) -> String {
    let jobs = data.queue();
    if jobs.is_empty() {
        return "Nothing queued".to_owned();
    }
    let n = jobs.len();
    let held = jobs.iter().filter(|j| j.state == JobState::Paused).count();
    let left = data.bytes_left();
    let mut parts = vec![
        format!("{n} job{}", if n == 1 { "" } else { "s" }),
        format!("{} to go", format::bytes(left)),
    ];
    let speed = data.speed();
    if data.status.running && speed > 0 {
        if let Some(eta) = eta_secs(left, speed) {
            parts.push(format!(
                "about {} at {}",
                format::duration_secs(eta),
                format::speed(speed)
            ));
        }
    } else if data.status.paused {
        parts.push("queue paused".to_owned());
    }
    if held > 0 {
        parts.push(format!("{held} held"));
    }
    parts.join("  ·  ")
}

/// Bytes down and bytes in total over every pending job (the queue's own track).
pub fn queue_bytes(data: &Data) -> (u64, u64) {
    data.queue().iter().fold((0u64, 0u64), |(d, t), j| {
        (
            d.saturating_add(data.shown_done(j)),
            t.saturating_add(j.bytes_total),
        )
    })
}

/// Draws the tab strip, the filter and the selected tab.
pub fn show_center(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    view: &mut ViewState,
    actions: &mut Vec<Action>,
) {
    let queue_n = data.queue().len();
    let history_n = data.history().len();
    let mut selected_rect = None;
    ui.horizontal(|ui| {
        for (tab, label, n) in [
            (Tab::Queue, "Queue", queue_n),
            (Tab::History, "History", history_n),
        ] {
            let rect = tab_button(ui, pal, label, n, view.tab == tab, || {
                actions.push(Action::SetTab(tab))
            });
            if view.tab == tab {
                selected_rect = Some(rect);
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            filter_box(ui, pal, view);
        });
    });
    if let Some(rect) = selected_rect {
        // The accent rule slides from the previous tab to the new one.
        let (left, right) = if view.reduced_motion {
            (rect.left() + 6.0, rect.right() - 6.0)
        } else {
            let ctx = ui.ctx();
            (
                ctx.animate_value_with_time(
                    Id::new("tab-rule-left"),
                    rect.left() + 6.0,
                    TAB_SLIDE_SECS,
                ),
                ctx.animate_value_with_time(
                    Id::new("tab-rule-right"),
                    rect.right() - 6.0,
                    TAB_SLIDE_SECS,
                ),
            )
        };
        let y = rect.bottom() - 1.5;
        ui.painter().line_segment(
            [egui::pos2(left, y), egui::pos2(right, y)],
            egui::Stroke::new(3.0, pal.accent),
        );
    }
    let r = ui.available_rect_before_wrap();
    ui.painter().line_segment(
        [r.left_top(), r.right_top()],
        egui::Stroke::new(1.0, pal.border),
    );
    ui.add_space(8.0);

    let now = ui.input(|i| i.time);
    view.queue.note_finishes(&data.jobs, now);
    match view.tab {
        Tab::Queue => queue_tab(ui, pal, data, view, actions),
        Tab::History => history_tab(ui, pal, data, view, actions),
    }
}

fn tab_button(
    ui: &mut Ui,
    pal: &Palette,
    label: &str,
    count: usize,
    selected: bool,
    mut on_click: impl FnMut(),
) -> egui::Rect {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let color = if selected { pal.text } else { pal.muted };
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, color);
    let count_text = count.to_string();
    let count_galley =
        ui.painter()
            .layout_no_wrap(count_text.clone(), fonts::mono_id(11.0), pal.muted);
    let pill_w = count_galley.size().x + 12.0;
    let size = egui::vec2(galley.size().x + 6.0 + pill_w + 24.0, 30.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.hovered() && !selected {
        ui.painter()
            .rect_filled(rect.shrink(2.0), egui::CornerRadius::same(5), pal.hover);
    }
    if resp.has_focus() {
        super::widgets::focus_ring(ui, pal, rect.shrink(2.0), 5);
    }
    let x = rect.left() + 12.0;
    ui.painter().galley(
        egui::pos2(x, rect.center().y - galley.size().y / 2.0),
        galley.clone(),
        color,
    );
    let pill = egui::Rect::from_min_size(
        egui::pos2(x + galley.size().x + 6.0, rect.center().y - 9.0),
        egui::vec2(pill_w, 18.0),
    );
    ui.painter()
        .rect_filled(pill, egui::CornerRadius::same(9), pal.sunken);
    ui.painter().galley(
        pill.center() - count_galley.size() / 2.0,
        count_galley,
        pal.muted,
    );
    let name = format!("{label} ({count})");
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &name)
    });
    if resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        on_click();
    }
    rect
}

fn filter_box(ui: &mut Ui, pal: &Palette, view: &mut ViewState) {
    let width = FILTER_W.min(ui.available_width() - 8.0).max(120.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 28.0), Sense::hover());
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let resp = child.add(
        TextEdit::singleline(&mut view.filter)
            .id_salt("job-filter")
            .hint_text(RichText::new("Filter by name").color(pal.muted))
            .desired_width(width - 30.0)
            .margin(egui::Margin {
                left: 26,
                right: 6,
                top: 5,
                bottom: 5,
            }),
    );
    paint_icon(
        ui.painter(),
        egui::Rect::from_center_size(
            egui::pos2(rect.left() + 14.0, rect.center().y),
            egui::vec2(14.0, 14.0),
        ),
        Icon::Search,
        pal.faint,
    );
    if view.queue.focus_filter {
        resp.request_focus();
        view.queue.focus_filter = false;
    }
    if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        view.filter.clear();
    }
    resp.on_hover_text("Show only jobs whose name contains this text (Ctrl+F)");
}

fn summary_row(ui: &mut Ui, pal: &Palette, text: &str, right: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        ui.label(
            RichText::new(text)
                .font(fonts::mono_id(12.0))
                .color(pal.muted),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
    ui.add_space(6.0);
}

fn queue_tab(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    view: &mut ViewState,
    actions: &mut Vec<Action>,
) {
    let jobs: Vec<&Job> = data
        .queue()
        .into_iter()
        .filter(|j| matches_filter(&j.name, &view.filter))
        .collect();
    let all = data.queue();
    if all.is_empty() {
        empty_queue(ui, pal, data, view, actions);
        return;
    }
    // A run started for single jobs leaves the rest queued: offer the whole queue.
    let outside = data.status.run_scope.is_some()
        && all
            .iter()
            .any(|j| j.state == JobState::Queued && !data.in_run(j));
    let (done, total) = queue_bytes(data);
    let busy = Beat::new(
        ui.ctx(),
        data.status.running && data.speed() > 0,
        view.reduced_motion,
        brand::PERIOD_BUSY * 2.0,
    );
    summary_row(ui, pal, &queue_summary(data), |ui| {
        if outside
            && kit_button(
                ui,
                pal,
                ButtonKind::Outline,
                Some(Icon::Play),
                "Start all",
                true,
            )
            .on_hover_text("Download every queued job, in queue order")
            .clicked()
        {
            actions.push(Action::Start);
        }
        // The whole queue as one segment track: every pending byte, done ones lit.
        if total > 0 {
            let width = (ui.available_width() * 0.4).clamp(120.0, 280.0);
            let sweep = busy.moving.then_some(busy.phase);
            segment_track_live(
                ui,
                pal,
                format::fraction(done, total),
                pal.accent,
                width,
                sweep,
            )
            .on_hover_text(format!(
                "Queue: {}% · {} of {} downloaded",
                format::percent(done, total),
                format::bytes(done),
                format::bytes(total)
            ));
        }
    });
    if jobs.is_empty() {
        no_match(ui, pal, &view.filter);
        return;
    }
    let (progress_w, min_width) = table_widths(ui.available_width(), ui.spacing().item_spacing.x);
    if min_width > ui.available_width() + 0.5 {
        egui::ScrollArea::horizontal()
            .id_salt("queue-hscroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_min_width(min_width);
                queue_table(ui, pal, data, &jobs, view, actions, progress_w);
            });
    } else {
        queue_table(ui, pal, data, &jobs, view, actions, progress_w);
    }
}

fn empty_queue(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    view: &ViewState,
    actions: &mut Vec<Action>,
) {
    let ambient = Beat::new(
        ui.ctx(),
        true,
        view.reduced_motion,
        brand::PERIOD_AMBIENT * 2.0,
    );
    ui.add_space(36.0);
    ui.vertical_centered(|ui| {
        article_grid(ui, pal, ambient.moving.then_some(ambient.phase));
        ui.add_space(18.0);
        ui.label(RichText::new(EMPTY_QUEUE).size(14.0).color(pal.muted));
        ui.add_space(4.0);
        ui.label(
            RichText::new("Drop .nzb files onto the window, or set a watch folder in Settings.")
                .small()
                .color(pal.faint),
        );
        ui.add_space(12.0);
        if kit_button(
            ui,
            pal,
            ButtonKind::Primary,
            Some(Icon::OpenNzb),
            "Add NZB…",
            true,
        )
        .on_hover_text("Import an NZB file (Ctrl+O)")
        .clicked()
        {
            actions.push(Action::ShowOpenNzb);
        }
        let finished = data.history().len();
        if finished > 0 {
            ui.add_space(8.0);
            if kit_button(
                ui,
                pal,
                ButtonKind::Text,
                None,
                &format!("See {finished} finished in History"),
                true,
            )
            .clicked()
            {
                actions.push(Action::SetTab(Tab::History));
            }
        }
    });
}

fn no_match(ui: &mut Ui, pal: &Palette, filter: &str) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(format!("No jobs match “{}”.", filter.trim()))
                .size(14.0)
                .color(pal.muted),
        );
    });
}

/// The eyebrow header row of a job table.
fn header_labels(h: &mut egui_extras::TableRow<'_, '_>, pal: &Palette, labels: &[&str]) {
    for t in labels {
        h.col(|ui| {
            let job = spaced(
                &t.to_uppercase(),
                fonts::mono_id(11.0),
                pal.faint,
                EYEBROW_TRACKING,
            );
            ui.add(egui::Label::new(job).selectable(false));
        });
    }
}

fn queue_table(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    jobs: &[&Job],
    view: &mut ViewState,
    actions: &mut Vec<Action>,
    progress_w: f32,
) {
    let now = ui.input(|i| i.time);
    if view.queue.highlight.is_some() && view.queue.highlight_at.is_none() {
        view.queue.highlight_at = Some(now);
    }
    if let (Some(_), Some(at)) = (view.queue.highlight, view.queue.highlight_at) {
        if now - at >= HIGHLIGHT_SECS {
            view.queue.highlight = None;
            view.queue.highlight_at = None;
        }
    }
    let busy = Beat::new(
        ui.ctx(),
        data.status.running && data.speed() > 0,
        view.reduced_motion,
        brand::PERIOD_BUSY * 2.0,
    );
    if view.queue.highlight.is_some() {
        ui.ctx().request_repaint_after(super::motion::FRAME);
    }
    let reduced = view.reduced_motion;
    let order = &data.queue_order;
    let last = order.len().saturating_sub(1);
    TableBuilder::new(ui)
        .id_salt("queue-table")
        .striped(true)
        .resizable(false)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::exact(POS_W))
        .column(Column::remainder().at_least(NAME_MIN).clip(true))
        .column(Column::exact(STATE_W))
        .column(Column::exact(SIZE_W))
        .column(Column::exact(progress_w))
        .column(Column::exact(CONTROLS_W))
        .header(24.0, |mut h| {
            header_labels(&mut h, pal, &["#", "Name", "State", "Size", "Progress", ""])
        })
        .body(|mut body| {
            for &job in jobs {
                let pos = order.iter().position(|&id| id == job.id);
                body.row(42.0, |mut row| {
                    row.set_selected(view.queue.selected == Some(job.id));
                    row.col(|ui| {
                        ui.label(
                            RichText::new(pos.map_or("–".to_owned(), |p| (p + 1).to_string()))
                                .font(fonts::mono_id(12.0))
                                .color(pal.faint),
                        );
                    });
                    row.col(|ui| {
                        let highlight = if view.queue.highlight == Some(job.id) {
                            // Accent at first, settling to the text color over the span.
                            let at = view.queue.highlight_at.unwrap_or(now);
                            let t = ((now - at) / HIGHLIGHT_SECS).clamp(0.0, 1.0) as f32;
                            Some(if reduced { 0.0 } else { t })
                        } else {
                            None
                        };
                        let live = live_subline(job, data.shown_done(job), data.speed());
                        name_cell(ui, pal, job, highlight, None, live);
                    });
                    row.col(|ui| {
                        let r = state_badge(ui, pal, job.state);
                        if job.state == JobState::Paused {
                            r.on_hover_text("Held: queue runs skip it until you start it");
                        } else if job.state == JobState::Queued && !data.in_run(job) {
                            r.on_hover_text("Waiting for Start");
                        }
                    });
                    row.col(|ui| {
                        ui.label(
                            RichText::new(format::bytes(job.bytes_total))
                                .font(fonts::mono_id(12.0))
                                .color(pal.text),
                        );
                    });
                    row.col(|ui| progress_cell(ui, pal, data, job, progress_w - 8.0, busy));
                    row.col(|ui| controls_cell(ui, pal, data, job, &mut view.queue, actions));
                    let resp = row.response();
                    if resp.clicked() {
                        actions.push(Action::Select(Some(job.id)));
                    }
                    let place = pos.map(|p| (p == 0, p == last));
                    resp.context_menu(|ui| {
                        job_menu(ui, data, job, place, &mut view.queue, actions)
                    });
                });
            }
        });
}

fn history_tab(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    view: &mut ViewState,
    actions: &mut Vec<Action>,
) {
    let all = data.history();
    if all.is_empty() {
        ui.add_space(36.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(EMPTY_HISTORY).size(14.0).color(pal.muted));
            ui.add_space(4.0);
            ui.label(
                RichText::new("Finished and failed downloads are listed here.")
                    .small()
                    .color(pal.faint),
            );
        });
        return;
    }
    let failed = all.iter().filter(|j| j.state == JobState::Failed).count();
    let text = if failed > 0 {
        format!("{} finished  ·  {failed} failed", all.len() - failed)
    } else {
        format!("{} finished", all.len())
    };
    let q = &mut view.queue;
    summary_row(ui, pal, &text, |ui| {
        let n = all.len();
        let s = if n == 1 { "" } else { "s" };
        if q.confirm_clear_files {
            if kit_button(ui, pal, ButtonKind::Text, None, "Cancel", true).clicked() {
                q.confirm_clear_files = false;
            }
            if kit_button(ui, pal, ButtonKind::Outline, Some(Icon::Trash), "Delete", true).clicked() {
                actions.push(Action::ClearHistoryAndFiles);
                q.confirm_clear_files = false;
            }
            ui.label(
                RichText::new(format!(
                    "Permanently delete the download folder{s} of {n} job{s}? This cannot be undone."
                ))
                .color(pal.negative),
            );
        } else if q.confirm_clear {
            if kit_button(ui, pal, ButtonKind::Text, None, "Cancel", true).clicked() {
                q.confirm_clear = false;
            }
            if kit_button(ui, pal, ButtonKind::Text, Some(Icon::Trash), "Delete files too…", true)
                .on_hover_text("Also delete each job's download folder from disk")
                .clicked()
            {
                q.confirm_clear = false;
                q.confirm_clear_files = true;
            }
            if kit_button(ui, pal, ButtonKind::Outline, None, "Clear", true).clicked() {
                actions.push(Action::ClearHistory);
                q.confirm_clear = false;
            }
            ui.label(
                RichText::new(format!(
                    "Remove {} job{} from the list? Files stay on disk.",
                    all.len(),
                    if all.len() == 1 { "" } else { "s" }
                ))
                .color(pal.text),
            );
        } else if kit_button(
            ui,
            pal,
            ButtonKind::Outline,
            Some(Icon::Close),
            "Clear history",
            true,
        )
        .on_hover_text("Remove finished jobs from the list (files stay on disk)")
        .clicked()
        {
            q.confirm_clear = true;
        }
    });
    let jobs: Vec<&Job> = all
        .into_iter()
        .filter(|j| matches_filter(&j.name, &view.filter))
        .collect();
    if jobs.is_empty() {
        no_match(ui, pal, &view.filter);
        return;
    }
    let now = ui.input(|i| i.time);
    if !view.queue.flashes.is_empty() {
        ui.ctx().request_repaint_after(super::motion::FRAME);
    }
    let reduced = view.reduced_motion;
    TableBuilder::new(ui)
        .id_salt("history-table")
        .striped(true)
        .resizable(false)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::remainder().at_least(NAME_MIN).clip(true))
        .column(Column::exact(STATE_W))
        .column(Column::exact(SIZE_W))
        .column(Column::exact(HISTORY_ACTIONS_W))
        .header(24.0, |mut h| {
            header_labels(&mut h, pal, &["Name", "Result", "Size", ""])
        })
        .body(|mut body| {
            for job in jobs {
                body.row(42.0, |mut row| {
                    row.set_selected(view.queue.selected == Some(job.id));
                    row.col(|ui| {
                        let flash = if reduced {
                            None
                        } else {
                            view.queue.flash(job.id, now)
                        };
                        name_cell(ui, pal, job, None, flash, None);
                    });
                    row.col(|ui| {
                        let r = state_badge(ui, pal, job.state);
                        if let Some(e) = &job.error {
                            r.on_hover_text(e);
                        }
                    });
                    row.col(|ui| {
                        ui.label(
                            RichText::new(format::bytes(job.bytes_total))
                                .font(fonts::mono_id(12.0))
                                .color(pal.text),
                        );
                    });
                    row.col(|ui| history_actions(ui, pal, job, &mut view.queue, actions));
                    let resp = row.response();
                    if resp.double_clicked() {
                        actions.push(Action::OpenJobFolder(job.id));
                    } else if resp.clicked() {
                        actions.push(Action::Select(Some(job.id)));
                    }
                    resp.context_menu(|ui| job_menu(ui, data, job, None, &mut view.queue, actions));
                });
            }
        });
}

/// The name and its subline. `highlight` is the just-imported fade (0 = full accent, 1 =
/// settled); `flash` a finish wash (state, 1 → 0) painted under the cell.
fn name_cell(
    ui: &mut Ui,
    pal: &Palette,
    job: &Job,
    highlight: Option<f32>,
    flash: Option<(JobState, f32)>,
    live: Option<String>,
) {
    if let Some((state, strength)) = flash {
        let tint = match state {
            JobState::Failed => pal.negative_soft,
            _ => pal.positive_soft,
        };
        let r = ui.available_rect_before_wrap();
        ui.painter().rect_filled(
            r.expand2(egui::vec2(6.0, 2.0)),
            egui::CornerRadius::same(4),
            Palette::fade(tint, strength),
        );
    }
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        let color = match highlight {
            Some(t) => pal.accent_fg.lerp_to_gamma(pal.text, t),
            None => pal.text,
        };
        ui.add(
            egui::Label::new(RichText::new(&job.name).color(color))
                .truncate()
                .selectable(false),
        )
        .on_hover_text(job.source_path.display().to_string());
        // While downloading at a measured speed the subline is the live figure (the
        // destination stays on hover); a failed job's subline is its error.
        let (sub, color) = match (&job.error, job.state, live) {
            (Some(e), JobState::Failed, _) => (e.clone(), pal.negative),
            (_, _, Some(l)) => (l, pal.accent_fg),
            _ => (job.dest_dir.display().to_string(), pal.muted),
        };
        ui.add(
            egui::Label::new(RichText::new(sub).small().color(color))
                .truncate()
                .selectable(false),
        );
    });
}

fn progress_cell(ui: &mut Ui, pal: &Palette, data: &Data, job: &Job, width: f32, busy: Beat) {
    // After the download: the post-processing stage replaces the byte figures.
    if let Some(p) = data.status.post.as_ref().filter(|p| p.job_id == job.id) {
        let fraction = p.permille.map(|m| m as f32 / 1000.0);
        let color = match p.stage {
            newskite_service::PostStage::Repair => pal.warning,
            _ => pal.accent,
        };
        let sweep = (fraction.is_none() && busy.moving).then_some(busy.phase);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            segment_track_live(ui, pal, fraction.unwrap_or(1.0), color, width, sweep);
            let head = match fraction {
                Some(f) => format!("{} {:.0}%", p.stage.label(), f * 100.0),
                None => p.stage.label().to_string(),
            };
            ui.add(
                egui::Label::new(
                    RichText::new(format!("{head}  ·  {}", p.detail))
                        .font(fonts::mono_id(11.0))
                        .color(pal.muted),
                )
                .truncate(),
            )
            .on_hover_text(&p.detail);
        });
        return;
    }
    let done = data.shown_done(job);
    // Segment cells fill in the accent; a held job's cells rest in the muted tone.
    let color = match job.state {
        JobState::Failed => pal.negative,
        JobState::Paused => pal.muted,
        _ => pal.accent,
    };
    let downloading = job.state == JobState::Downloading;
    let sweep = (downloading && busy.moving).then_some(busy.phase);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        segment_track_live(
            ui,
            pal,
            format::fraction(done, job.bytes_total),
            color,
            width,
            sweep,
        );
        ui.label(
            RichText::new(format!(
                "{}%  ·  {} / {}",
                format::percent(done, job.bytes_total),
                format::bytes(done),
                format::bytes(job.bytes_total)
            ))
            .font(fonts::mono_id(11.0))
            .color(pal.muted),
        );
    });
}

/// The live subline of a downloading job: the measured speed and, from it, the time
/// left for the bytes still missing. `None` without a measured speed (never a guess).
pub fn live_subline(job: &Job, done: u64, speed_bps: u64) -> Option<String> {
    if job.state != JobState::Downloading || speed_bps == 0 {
        return None;
    }
    let speed = format::speed(speed_bps);
    if job.bytes_total == 0 {
        return Some(speed);
    }
    let eta = eta_secs(job.bytes_total.saturating_sub(done), speed_bps)?;
    Some(format!("{speed} · {} left", format::duration_secs(eta)))
}

/// The inline editors that take over a row's controls cell (password, remove
/// confirmation). Returns `true` when one was drawn.
fn inline_editors(
    ui: &mut Ui,
    pal: &Palette,
    job: &Job,
    q: &mut super::queue::QueueState,
    actions: &mut Vec<Action>,
) -> bool {
    if let Some((id, text)) = q.password_edit.as_mut() {
        if *id == job.id {
            let resp = ui.add(
                TextEdit::singleline(text)
                    .password(true)
                    .hint_text("Archive password")
                    .desired_width(76.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.small_button("Save").clicked() || enter {
                let pw = (!text.is_empty()).then(|| text.clone());
                actions.push(Action::SetJobPassword(job.id, pw));
                q.password_edit = None;
            } else if icon_button(ui, pal, Icon::Close, pal.muted, "Cancel").clicked() {
                q.password_edit = None;
            }
            return true;
        }
    }
    if q.confirm_delete == Some(job.id) {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new("Delete files?").color(pal.negative))
            .on_hover_text(format!("Permanently delete {} and remove the job", job.dest_dir.display()));
        if ui.small_button("Yes").clicked() {
            actions.push(Action::DeleteJobFiles(job.id));
            q.confirm_delete = None;
        }
        if ui.small_button("No").clicked() {
            q.confirm_delete = None;
        }
        return true;
    }
    if q.confirm_remove == Some(job.id) {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new("Remove?").color(pal.negative));
        if ui.small_button("Yes").clicked() {
            actions.push(Action::RemoveJob(job.id));
            q.confirm_remove = None;
        }
        if ui.small_button("No").clicked() {
            q.confirm_remove = None;
        }
        return true;
    }
    false
}

fn controls_cell(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    job: &Job,
    q: &mut QueueState,
    actions: &mut Vec<Action>,
) {
    if inline_editors(ui, pal, job, q, actions) {
        return;
    }
    ui.spacing_mut().item_spacing.x = 4.0;
    if let (Some(action), Some((icon, tip))) = (toggle_action(data, job), toggle_look(data, job)) {
        let color = if icon == Icon::Play {
            pal.accent_fg
        } else {
            pal.text
        };
        if icon_button(ui, pal, icon, color, tip).clicked() {
            actions.push(action);
        }
    }
    let order = &data.queue_order;
    let place = order
        .iter()
        .position(|&id| id == job.id)
        .map(|p| (p == 0, p + 1 == order.len()));
    let first = place.is_none_or(|(f, _)| f);
    let up = icon_button(
        ui,
        pal,
        Icon::ChevronUp,
        if first {
            Palette::fade(pal.muted, 0.4)
        } else {
            pal.muted
        },
        "Move up (Alt+↑)",
    );
    if up.clicked() && !first {
        actions.push(Action::MoveJob(job.id, JobMove::Up));
    }
    let menu = icon_button(ui, pal, Icon::More, pal.muted, "More actions");
    egui::Popup::menu(&menu).show(|ui| job_menu(ui, data, job, place, q, actions));
    let tip = if job.state == JobState::Downloading {
        "Cancel and remove from the queue (Del)"
    } else {
        "Remove from the queue (Del)"
    };
    if icon_button(ui, pal, Icon::Close, pal.muted, tip).clicked() {
        q.confirm_remove = Some(job.id);
    }
}

fn history_actions(
    ui: &mut Ui,
    pal: &Palette,
    job: &Job,
    q: &mut QueueState,
    actions: &mut Vec<Action>,
) {
    if inline_editors(ui, pal, job, q, actions) {
        return;
    }
    ui.spacing_mut().item_spacing.x = 4.0;
    if icon_button(ui, pal, Icon::Folder, pal.muted, "Open the download folder").clicked() {
        actions.push(Action::OpenJobFolder(job.id));
    }
    if job.state == JobState::Failed {
        if icon_button(
            ui,
            pal,
            Icon::Retry,
            pal.accent_fg,
            "Retry (completed files are kept)",
        )
        .clicked()
        {
            actions.push(Action::RetryJob(job.id));
        }
        let (color, tip) = if job.password.is_some() {
            (pal.text, "Change archive password (one is set)")
        } else {
            (pal.muted, "Set archive password")
        };
        if icon_button(ui, pal, Icon::Key, color, tip).clicked() {
            q.password_edit = Some((job.id, String::new()));
        }
    }
    if icon_button(ui, pal, Icon::Trash, pal.muted, "Delete files and remove from the list").clicked() {
        q.confirm_delete = Some(job.id);
    }
    if icon_button(ui, pal, Icon::Close, pal.muted, "Remove from the list (files stay on disk)").clicked() {
        q.confirm_remove = Some(job.id);
    }
}

/// The row menu (⋯ button and right click). `place` is `(first, last)` in the queue
/// order for pending jobs.
fn job_menu(
    ui: &mut Ui,
    data: &Data,
    job: &Job,
    place: Option<(bool, bool)>,
    q: &mut QueueState,
    actions: &mut Vec<Action>,
) {
    ui.set_min_width(190.0);
    let mut item = |ui: &mut Ui, enabled: bool, text: &str, action: Action| {
        if ui.add_enabled(enabled, egui::Button::new(text)).clicked() {
            actions.push(action);
            ui.close();
        }
    };
    if let Some(a) = toggle_action(data, job) {
        let text = match (&a, job.state) {
            (Action::PauseJob(_), JobState::Downloading) => "Pause",
            (Action::PauseJob(_), _) => "Hold",
            (Action::StartJob(_), JobState::Paused) => "Resume",
            (Action::StartJob(_), _) => "Download now",
            _ => "Retry",
        };
        item(ui, true, text, a);
    }
    if let Some((first, last)) = place {
        ui.separator();
        item(
            ui,
            !first,
            "Move to top",
            Action::MoveJob(job.id, JobMove::Top),
        );
        item(ui, !first, "Move up", Action::MoveJob(job.id, JobMove::Up));
        item(
            ui,
            !last,
            "Move down",
            Action::MoveJob(job.id, JobMove::Down),
        );
        item(
            ui,
            !last,
            "Move to bottom",
            Action::MoveJob(job.id, JobMove::Bottom),
        );
    }
    ui.separator();
    item(
        ui,
        true,
        "Open download folder",
        Action::OpenJobFolder(job.id),
    );
    if job.state != JobState::Done {
        let text = if job.password.is_some() {
            "Change archive password…"
        } else {
            "Set archive password…"
        };
        if ui.button(text).clicked() {
            q.password_edit = Some((job.id, String::new()));
            ui.close();
        }
    }
    ui.separator();
    let text = if job.state.is_pending() {
        "Remove from queue…"
    } else {
        "Remove from list…"
    };
    if ui.button(text).clicked() {
        q.confirm_remove = Some(job.id);
        ui.close();
    }
    if !job.state.is_pending() && ui.button("Delete files and remove…").clicked() {
        q.confirm_delete = Some(job.id);
        ui.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn job(id: i64, name: &str, size: u64, state: JobState, created: i64) -> Job {
        Job {
            id,
            name: name.into(),
            state,
            bytes_total: size,
            bytes_done: 0,
            dest_dir: PathBuf::new(),
            source_path: PathBuf::new(),
            created_at: created,
            error: None,
            password: None,
            stats: Default::default(),
        }
    }

    #[test]
    fn queue_follows_the_stored_order_and_history_is_newest_first() {
        let mut d = Data {
            jobs: vec![
                job(1, "beta", 30, JobState::Done, 10),
                job(2, "Alpha", 10, JobState::Downloading, 20),
                job(3, "gamma", 20, JobState::Queued, 20),
                job(4, "delta", 20, JobState::Failed, 30),
                job(5, "eps", 20, JobState::Paused, 40),
            ],
            queue_order: vec![3, 5, 2],
            ..Data::default()
        };
        let ids = |v: Vec<&Job>| v.iter().map(|j| j.id).collect::<Vec<_>>();
        assert_eq!(ids(d.queue()), vec![3, 5, 2]);
        assert_eq!(ids(d.history()), vec![4, 1]);
        // Jobs missing from the order (just imported) go last.
        d.queue_order = vec![5];
        assert_eq!(ids(d.queue()), vec![5, 2, 3]);
    }

    #[test]
    fn filter_is_a_case_insensitive_substring() {
        assert!(matches_filter("Debian.12.Netinst", ""));
        assert!(matches_filter("Debian.12.Netinst", "  debian "));
        assert!(matches_filter("Debian.12.Netinst", "NETINST"));
        assert!(!matches_filter("Debian.12.Netinst", "ubuntu"));
    }

    #[test]
    fn toggle_depends_on_state_and_run_scope() {
        let mut d = Data::default();
        let queued = job(1, "a", 10, JobState::Queued, 0);
        let held = job(2, "b", 10, JobState::Paused, 0);
        let active = job(3, "c", 10, JobState::Downloading, 0);
        let failed = job(4, "d", 10, JobState::Failed, 0);
        let done = job(5, "e", 10, JobState::Done, 0);
        // Idle: queued and held jobs start on their own.
        assert_eq!(toggle_action(&d, &queued), Some(Action::StartJob(1)));
        assert_eq!(toggle_action(&d, &held), Some(Action::StartJob(2)));
        assert_eq!(toggle_action(&d, &failed), Some(Action::RetryJob(4)));
        assert_eq!(toggle_action(&d, &done), None);
        // A run of the whole queue: queued jobs can be held, the active one paused.
        d.status.running = true;
        assert_eq!(toggle_action(&d, &queued), Some(Action::PauseJob(1)));
        assert_eq!(toggle_action(&d, &active), Some(Action::PauseJob(3)));
        // A run limited to other jobs: this one can still be started.
        d.status.run_scope = Some(vec![3]);
        assert_eq!(toggle_action(&d, &queued), Some(Action::StartJob(1)));
    }

    #[test]
    fn summary_counts_bytes_and_only_estimates_with_a_speed() {
        let mut d = Data::default();
        assert_eq!(queue_summary(&d), "Nothing queued");
        let mut a = job(1, "a", 1000, JobState::Downloading, 0);
        a.bytes_done = 400;
        d.jobs = vec![
            a,
            job(2, "b", 1000, JobState::Paused, 0),
            job(3, "c", 5, JobState::Done, 0),
        ];
        let s = queue_summary(&d);
        assert!(s.starts_with("2 jobs  ·  1.6 KB to go"), "{s}");
        assert!(s.ends_with("1 held"), "{s}");
        assert!(!s.contains("about"), "no speed, no estimate: {s}");
        d.status.running = true;
        d.status.speed_bps = 100;
        assert!(queue_summary(&d).contains("about 0:16 at 100 B/s"));
    }

    #[test]
    fn queue_track_counts_pending_bytes_only() {
        let mut a = job(1, "a", 100, JobState::Downloading, 0);
        a.bytes_done = 50;
        let d = Data {
            jobs: vec![
                a,
                job(2, "b", 100, JobState::Paused, 0),
                job(3, "c", 900, JobState::Done, 0),
            ],
            ..Data::default()
        };
        assert_eq!(queue_bytes(&d), (50, 200));
        assert_eq!(queue_bytes(&Data::default()), (0, 0));
    }

    #[test]
    fn queue_table_fits_the_minimum_window() {
        // 960 px window, 14 px panel margins: about 930 px.
        let (progress, min) = table_widths(930.0, 8.0);
        assert!(min <= 930.0 + 0.5, "{min}");
        assert!((PROGRESS_MIN..=PROGRESS_MAX).contains(&progress));
        // Wide windows get the full progress column.
        assert_eq!(table_widths(1600.0, 8.0).0, PROGRESS_MAX);
        // Too narrow: the minimum is reported so the table can scroll.
        let (p, min) = table_widths(500.0, 8.0);
        assert_eq!(p, PROGRESS_MIN);
        assert!(min > 500.0);
    }

    #[test]
    fn live_subline_only_with_a_measured_speed() {
        let mut j = job(1, "a", 1000, JobState::Downloading, 0);
        assert_eq!(live_subline(&j, 500, 0), None);
        assert_eq!(
            live_subline(&j, 500, 100).as_deref(),
            Some("100 B/s · 0:05 left")
        );
        j.bytes_total = 0;
        assert_eq!(live_subline(&j, 0, 100).as_deref(), Some("100 B/s"));
        j.state = JobState::Queued;
        assert_eq!(live_subline(&j, 0, 100), None);
    }

    #[test]
    fn eta_needs_a_speed() {
        assert_eq!(eta_secs(1000, 0), None);
        assert_eq!(eta_secs(1000, 100), Some(10));
        assert_eq!(eta_secs(1001, 100), Some(11), "rounds up");
        assert_eq!(eta_secs(0, 100), Some(0));
    }

    #[test]
    fn finishes_flash_once_and_fade() {
        let mut q = QueueState::default();
        let mut jobs = vec![job(1, "a", 10, JobState::Downloading, 0)];
        q.note_finishes(&jobs, 0.0);
        assert!(q.flash(1, 0.0).is_none(), "no flash on first sight");
        jobs[0].state = JobState::Done;
        q.note_finishes(&jobs, 1.0);
        let (state, s) = q.flash(1, 1.0).expect("flash");
        assert_eq!(state, JobState::Done);
        assert!((s - 1.0).abs() < 1e-6);
        let (_, mid) = q.flash(1, 1.0 + FLASH_SECS / 2.0).expect("fading");
        assert!(mid < 1.0 && mid > 0.0);
        q.note_finishes(&jobs, 1.0 + FLASH_SECS + 0.1);
        assert!(q.flash(1, 5.0).is_none(), "expired");
        // A row that vanishes is forgotten.
        q.note_finishes(&[], 9.0);
        assert!(q.last_states.is_empty());
    }
}
