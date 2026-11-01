//! Center area: tab strip, the Queue tab (outline §6.2b) and the placeholder tabs.

use std::cmp::Ordering;
use std::collections::HashMap;

use eframe::egui::{self, Align, Id, Layout, RichText, Sense, TextEdit, Ui};
use egui_extras::{Column, TableBuilder};
use newskite_service::{Job, JobState};
use newskite_types::{format, Settings};

use super::app::{Action, Data, Tab, ViewState};
use super::brand;
use super::fonts;
use super::motion::Beat;
use super::theme::Palette;
use super::widgets::{
    article_grid, icon_button, kit_button, segment_track_live, spaced, state_badge, status_chip,
    status_chip_width, ButtonKind, Icon, Status, EYEBROW_TRACKING,
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

/// Exact empty-state text of the Queue table.
pub const EMPTY_QUEUE: &str = "No jobs. Import an NZB to begin.";

/// Queue table column widths (logical px). Only Name and Progress give way on narrow
/// windows; Actions keeps room for the "Remove?" Yes/No confirmation and the inline
/// archive-password field.
const ID_W: f32 = 44.0;
const STATE_W: f32 = 112.0;
const SIZE_W: f32 = 84.0;
const ACTIONS_W: f32 = 168.0;
const NAME_MIN: f32 = 140.0;
const PROGRESS_MIN: f32 = 110.0;
const PROGRESS_MAX: f32 = 230.0;

/// Progress column width and the table's minimum width for `available` px with
/// `gap` px between columns. When the minimum exceeds `available` (a widened sidebar),
/// the table scrolls sideways instead of being clipped.
pub fn table_widths(available: f32, gap: f32) -> (f32, f32) {
    let fixed = ID_W + STATE_W + SIZE_W + ACTIONS_W + 5.0 * gap;
    let progress = (available - fixed - NAME_MIN).clamp(PROGRESS_MIN, PROGRESS_MAX);
    (progress, fixed + NAME_MIN + progress)
}

/// Queue sort keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortKey {
    /// Creation time (queue order).
    #[default]
    Date,
    /// Job name, case-insensitive.
    Name,
    /// Total size.
    Size,
    /// State, then queue order.
    State,
}

impl SortKey {
    const ALL: [SortKey; 4] = [SortKey::Date, SortKey::Name, SortKey::Size, SortKey::State];

    fn label(self) -> &'static str {
        match self {
            SortKey::Date => "Date added",
            SortKey::Name => "Name",
            SortKey::Size => "Size",
            SortKey::State => "State",
        }
    }
}

/// Queue tab UI state.
#[derive(Default)]
pub struct QueueState {
    /// Sort key.
    pub sort: SortKey,
    /// Descending order.
    pub descending: bool,
    /// Job awaiting removal confirmation.
    pub confirm_remove: Option<i64>,
    /// Job whose archive password is being edited, with the text typed so far.
    pub password_edit: Option<(i64, String)>,
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

fn state_rank(s: JobState) -> u8 {
    match s {
        JobState::Downloading => 0,
        JobState::Queued => 1,
        JobState::Paused => 2,
        JobState::Failed => 3,
        JobState::Done => 4,
    }
}

/// Jobs in display order.
pub fn sorted(jobs: &[Job], key: SortKey, descending: bool) -> Vec<&Job> {
    let mut v: Vec<&Job> = jobs.iter().collect();
    v.sort_by(|a, b| {
        let o = match key {
            SortKey::Date => a.created_at.cmp(&b.created_at),
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Size => a.bytes_total.cmp(&b.bytes_total),
            SortKey::State => state_rank(a.state).cmp(&state_rank(b.state)),
        };
        let o = if o == Ordering::Equal {
            a.id.cmp(&b.id)
        } else {
            o
        };
        if descending {
            o.reverse()
        } else {
            o
        }
    });
    v
}

/// Connection pill text and whether a server is usable.
pub fn connection_text(settings: &Settings, running: bool) -> (String, bool) {
    match settings.first_usable_server() {
        Some(s) => {
            let label = if running { "Connected" } else { "Ready" };
            (
                format!(
                    "{label} · {} · {} conn",
                    s.address(),
                    settings.effective_connections()
                ),
                true,
            )
        }
        None => ("No server".to_owned(), false),
    }
}

/// Draws the tab strip and the selected tab.
pub fn show_center(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    view: &mut ViewState,
    actions: &mut Vec<Action>,
) {
    let mut selected_rect = None;
    ui.horizontal(|ui| {
        for (tab, label) in [
            (Tab::Search, "Search"),
            (Tab::Latest, "Latest File Groups"),
            (Tab::Queue, "Queue"),
        ] {
            let rect = tab_button(ui, pal, label, view.tab == tab, || {
                actions.push(Action::SetTab(tab))
            });
            if view.tab == tab {
                selected_rect = Some(rect);
            }
        }
        if view.tab == Tab::Queue {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (icon, tip) = if view.queue.descending {
                    (Icon::ChevronDown, "Descending (click for ascending)")
                } else {
                    (Icon::ChevronUp, "Ascending (click for descending)")
                };
                if icon_button(ui, pal, icon, pal.muted, tip).clicked() {
                    view.queue.descending = !view.queue.descending;
                }
                egui::ComboBox::from_id_salt("queue-sort")
                    .selected_text(view.queue.sort.label())
                    .show_ui(ui, |ui| {
                        for k in SortKey::ALL {
                            ui.selectable_value(&mut view.queue.sort, k, k.label());
                        }
                    });
                ui.label(RichText::new("Sort By").color(pal.muted));
            });
        }
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

    match view.tab {
        Tab::Queue => queue_tab(ui, pal, data, view, actions),
        Tab::Search => placeholder(
            ui,
            pal,
            "Search",
            &if view.search_text.trim().is_empty() {
                "Newskite's search is indexer-agnostic: it will query a Newznab indexer you configure. \
                 No indexer is configured, so searching is not available yet.\n\n\
                 NZB import is the primary input: use Open NZB (Ctrl+O) or a watch folder."
                    .to_owned()
            } else {
                format!(
                    "Searching for “{}” needs a Newznab indexer, and none is configured.\n\n\
                     NZB import is the primary input: use Open NZB (Ctrl+O) or a watch folder.",
                    view.search_text.trim()
                )
            },
            actions,
        ),
        Tab::Latest => placeholder(
            ui,
            pal,
            "Latest File Groups",
            "Recent file groups come from an indexer feed (indexer-agnostic, Newznab). \
             None is configured, so this list is empty.\n\n\
             NZB import is the primary input: use Open NZB (Ctrl+O) or a watch folder.",
            actions,
        ),
    }
}

fn tab_button(
    ui: &mut Ui,
    pal: &Palette,
    label: &str,
    selected: bool,
    mut on_click: impl FnMut(),
) -> egui::Rect {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let color = if selected { pal.text } else { pal.muted };
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, color);
    let size = egui::vec2(galley.size().x + 24.0, 30.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.hovered() && !selected {
        ui.painter()
            .rect_filled(rect.shrink(2.0), egui::CornerRadius::same(5), pal.hover);
    }
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, color);
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, label)
    });
    if resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        on_click();
    }
    rect
}

fn placeholder(ui: &mut Ui, pal: &Palette, title: &str, text: &str, actions: &mut Vec<Action>) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        ui.set_max_width(520.0);
        ui.label(RichText::new(title).heading().color(pal.text));
        ui.add_space(6.0);
        ui.label(RichText::new(text).color(pal.muted));
        ui.add_space(12.0);
        if kit_button(
            ui,
            pal,
            ButtonKind::Outline,
            Some(Icon::OpenNzb),
            "Open NZB…",
            true,
        )
        .clicked()
        {
            actions.push(Action::ShowOpenNzb);
        }
    });
}

fn queue_tab(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    view: &mut ViewState,
    actions: &mut Vec<Action>,
) {
    let st = &data.status;
    ui.horizontal(|ui| {
        let has_runnable = data
            .jobs
            .iter()
            .any(|j| matches!(j.state, JobState::Queued | JobState::Paused));
        // The Queue view's one filled call to action.
        let can_start = !st.running && has_runnable;
        if kit_button(
            ui,
            pal,
            ButtonKind::Primary,
            Some(Icon::Play),
            "Start",
            can_start,
        )
        .on_hover_text("Start downloading queued jobs, oldest first")
        .clicked()
            && can_start
        {
            actions.push(Action::Start);
        }
        let can_pause = st.running && !st.paused;
        if kit_button(
            ui,
            pal,
            ButtonKind::Outline,
            Some(Icon::Pause),
            "Pause",
            can_pause,
        )
        .on_hover_text("Stop after the current job finishes")
        .clicked()
            && can_pause
        {
            actions.push(Action::Pause);
        }
        if kit_button(
            ui,
            pal,
            ButtonKind::Outline,
            Some(Icon::Play),
            "Resume",
            st.paused,
        )
        .on_hover_text("Clear the pause and continue")
        .clicked()
            && st.paused
        {
            actions.push(Action::Resume);
        }
        let watch_set = !data.settings.watch_folder.as_os_str().is_empty();
        let scan = kit_button(
            ui,
            pal,
            ButtonKind::Outline,
            Some(Icon::Folder),
            "Scan watch folder",
            watch_set,
        );
        let scan = if watch_set {
            scan.on_hover_text(format!(
                "Import new .nzb files from {}",
                data.settings.watch_folder.display()
            ))
        } else {
            scan.on_hover_text("Set a watch folder in Settings, Downloads first")
        };
        if scan.clicked() && watch_set {
            actions.push(Action::ScanWatch);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (text, ok) = connection_text(&data.settings, st.running);
            let status = if ok { Status::Ok } else { Status::Warn };
            if status_chip_width(ui, &text) <= ui.available_width() {
                status_chip(ui, pal, status, &text);
            } else {
                // Narrow windows: the state word alone, the details on hover.
                let short = text.split(" · ").next().unwrap_or(&text).to_owned();
                status_chip(ui, pal, status, &short).on_hover_text(text);
            }
        });
    });
    ui.add_space(10.0);

    if data.jobs.is_empty() {
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
            ui.add_space(12.0);
            if kit_button(
                ui,
                pal,
                ButtonKind::Outline,
                Some(Icon::OpenNzb),
                "Open NZB…",
                true,
            )
            .clicked()
            {
                actions.push(Action::ShowOpenNzb);
            }
        });
        return;
    }

    let (progress_w, min_width) = table_widths(ui.available_width(), ui.spacing().item_spacing.x);
    if min_width > ui.available_width() + 0.5 {
        egui::ScrollArea::horizontal()
            .id_salt("queue-hscroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_min_width(min_width);
                queue_table(ui, pal, data, view, actions, progress_w);
            });
    } else {
        queue_table(ui, pal, data, view, actions, progress_w);
    }
}

fn queue_table(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    view: &mut ViewState,
    actions: &mut Vec<Action>,
    progress_w: f32,
) {
    let now = ui.input(|i| i.time);
    view.queue.note_finishes(&data.jobs, now);
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
    if !view.queue.flashes.is_empty() || view.queue.highlight.is_some() {
        ui.ctx().request_repaint_after(super::motion::FRAME);
    }
    let jobs = sorted(&data.jobs, view.queue.sort, view.queue.descending);
    let reduced = view.reduced_motion;
    TableBuilder::new(ui)
        .id_salt("queue-table")
        .striped(true)
        .resizable(false)
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::exact(ID_W))
        .column(Column::remainder().at_least(NAME_MIN).clip(true))
        .column(Column::exact(STATE_W))
        .column(Column::exact(SIZE_W))
        .column(Column::exact(progress_w))
        .column(Column::exact(ACTIONS_W))
        .header(24.0, |mut h| {
            for t in ["#", "Name", "State", "Size", "Progress", ""] {
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
        })
        .body(|mut body| {
            for job in jobs {
                body.row(40.0, |mut row| {
                    row.col(|ui| {
                        ui.label(
                            RichText::new(job.id.to_string())
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
                        let flash = if reduced {
                            None
                        } else {
                            view.queue.flash(job.id, now)
                        };
                        let live = live_subline(job, data.shown_done(job), data.speed());
                        name_cell(ui, pal, job, highlight, flash, live);
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
                    row.col(|ui| progress_cell(ui, pal, data, job, progress_w - 8.0, busy));
                    row.col(|ui| actions_cell(ui, pal, job, &mut view.queue, actions));
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
        ui.add(egui::Label::new(RichText::new(&job.name).color(color)).truncate())
            .on_hover_text(job.source_path.display().to_string());
        // While downloading at a measured speed the subline is the live figure (the
        // destination stays on hover); a failed job's subline is its error.
        let (sub, color) = match (&job.error, job.state, live) {
            (Some(e), JobState::Failed, _) => (e.clone(), pal.negative),
            (_, _, Some(l)) => (l, pal.accent_fg),
            _ => (job.dest_dir.display().to_string(), pal.muted),
        };
        ui.add(egui::Label::new(RichText::new(sub).small().color(color)).truncate());
    });
}

fn progress_cell(ui: &mut Ui, pal: &Palette, data: &Data, job: &Job, width: f32, busy: Beat) {
    let done = data.shown_done(job);
    // Segment cells fill in the accent; a failed job's cells take the error color.
    let color = match job.state {
        JobState::Failed => pal.negative,
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

fn actions_cell(
    ui: &mut Ui,
    pal: &Palette,
    job: &Job,
    q: &mut QueueState,
    actions: &mut Vec<Action>,
) {
    if let Some((id, text)) = q.password_edit.as_mut() {
        if *id == job.id {
            let resp = ui.add(
                TextEdit::singleline(text)
                    .password(true)
                    .hint_text("Archive password")
                    .desired_width(80.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.small_button("Save").clicked() || enter {
                let pw = (!text.is_empty()).then(|| text.clone());
                actions.push(Action::SetJobPassword(job.id, pw));
                q.password_edit = None;
            } else if icon_button(ui, pal, Icon::Close, pal.muted, "Cancel").clicked() {
                q.password_edit = None;
            }
            return;
        }
    }
    if q.confirm_remove == Some(job.id) {
        ui.label(RichText::new("Remove?").color(pal.negative));
        if ui.small_button("Yes").clicked() {
            actions.push(Action::RemoveJob(job.id));
            q.confirm_remove = None;
        }
        if ui.small_button("No").clicked() {
            q.confirm_remove = None;
        }
        return;
    }
    if job.state == JobState::Failed
        && icon_button(
            ui,
            pal,
            Icon::Retry,
            pal.muted,
            "Retry (completed files are kept)",
        )
        .clicked()
    {
        actions.push(Action::RetryJob(job.id));
    }
    if job.state != JobState::Done {
        let tip = if job.password.is_some() {
            "Change archive password (one is set)"
        } else {
            "Set archive password"
        };
        let color = if job.password.is_some() {
            pal.text
        } else {
            pal.muted
        };
        if icon_button(ui, pal, Icon::Key, color, tip).clicked() {
            q.password_edit = Some((job.id, String::new()));
        }
    }
    let tip = if job.state == JobState::Downloading {
        "Cancel and remove from the queue"
    } else {
        "Remove from the queue"
    };
    if icon_button(ui, pal, Icon::Close, pal.muted, tip).clicked() {
        q.confirm_remove = Some(job.id);
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
        }
    }

    #[test]
    fn sorting() {
        let jobs = vec![
            job(1, "beta", 30, JobState::Done, 10),
            job(2, "Alpha", 10, JobState::Downloading, 20),
            job(3, "gamma", 20, JobState::Queued, 20),
        ];
        let ids = |v: Vec<&Job>| v.iter().map(|j| j.id).collect::<Vec<_>>();
        assert_eq!(ids(sorted(&jobs, SortKey::Date, false)), vec![1, 2, 3]);
        assert_eq!(ids(sorted(&jobs, SortKey::Date, true)), vec![3, 2, 1]);
        assert_eq!(ids(sorted(&jobs, SortKey::Name, false)), vec![2, 1, 3]);
        assert_eq!(ids(sorted(&jobs, SortKey::Size, false)), vec![2, 3, 1]);
        assert_eq!(ids(sorted(&jobs, SortKey::State, false)), vec![2, 3, 1]);
    }

    #[test]
    fn queue_table_fits_the_minimum_window() {
        // 960 px window, default 220 px sidebar, 10 px panel margins: about 720 px.
        let (progress, min) = table_widths(720.0, 8.0);
        assert!(min <= 720.0 + 0.5, "{min}");
        assert!((PROGRESS_MIN..=PROGRESS_MAX).contains(&progress));
        // Wide windows get the full progress column.
        assert_eq!(table_widths(1400.0, 8.0).0, PROGRESS_MAX);
        // Too narrow (widened sidebar): the minimum is reported so the table can scroll.
        let (p, min) = table_widths(560.0, 8.0);
        assert_eq!(p, PROGRESS_MIN);
        assert!(min > 560.0);
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

    #[test]
    fn connection_pill_text() {
        let mut s = Settings::default();
        s.servers[0].host = String::new();
        assert_eq!(connection_text(&s, false), ("No server".to_owned(), false));
        s.servers[0].host = "news.example.net".into();
        s.servers[0].connections = 6;
        let (t, ok) = connection_text(&s, true);
        assert!(ok);
        assert_eq!(t, "Connected · news.example.net:563 · 6 conn");
        assert!(connection_text(&s, false).0.starts_with("Ready"));
    }
}
