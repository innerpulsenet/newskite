//! The center of the window, a quiet list: a header with the view's title, a one-line
//! summary, the queue filter and the actions that move the queue, then the view itself.
//! The queue is a list of rows (name, state, figure, folder or reason, and a thin strip of
//! the job's articles), with what finished in the last day under it; History lists finished jobs
//! the same way; Log shows the message log. A click selects a row (the detail pane opens
//! beside the list); every row keeps its controls and its context menu.

use eframe::egui::{self, Align, Id, Layout, Margin, Rect, RichText, Sense, Shape, Ui, Vec2};
use newskite_service::{Job, JobState};
use newskite_types::format;

use super::app::{Action, Data, Tab, ViewState};
use super::articles;
use super::docks;
use super::fonts;
use super::queue::{
    controls_cell, empty_queue, filter_box, finished_subline, history_actions, home_relative, job_menu,
    live_subline, matches_filter, no_match, now_unix, queue_summary, result_qualifier, search_field, EMPTY_HISTORY,
};
use super::theme::Palette;
use super::toolbar::{open_nzb_row, queue_control};
use super::widgets::{kit_button, section_title, segmented, tool_button, ButtonKind, Icon, ToolState};

/// Which queued jobs the list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListFilter {
    #[default]
    All,
    /// Downloading and queued jobs.
    Active,
    /// Jobs held by the user.
    Held,
}

impl ListFilter {
    pub const ALL: [ListFilter; 3] = [ListFilter::All, ListFilter::Active, ListFilter::Held];

    pub fn label(self) -> &'static str {
        match self {
            ListFilter::All => "All",
            ListFilter::Active => "Active",
            ListFilter::Held => "Held",
        }
    }

    /// Whether a job in `state` is listed.
    pub fn admits(self, state: JobState) -> bool {
        match self {
            ListFilter::All => true,
            ListFilter::Active => matches!(state, JobState::Downloading | JobState::Queued),
            ListFilter::Held => state == JobState::Paused,
        }
    }
}

/// Width kept for a row's controls (six icon buttons, or an inline confirmation).
const CONTROLS_W: f32 = 178.0;
/// Width of the controls box while an archive password is typed in the row.
const PASSWORD_EDIT_W: f32 = 320.0;
/// A row's padding.
const ROW_PAD: Margin = Margin { left: 18, right: 12, top: 12, bottom: 12 };
/// Seconds the just-imported job's name stays in the accent before settling.
const HIGHLIGHT_SECS: f64 = 2.5;
/// Finished jobs listed under the queue, and how recent they must be (seconds).
const RECENT_MAX: usize = 5;
const RECENT_SECS: i64 = 24 * 3600;
/// Header width below which the actions go on their own line.
const HEADER_WRAP_W: f32 = 760.0;

/// What a row shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Queue,
    Finished,
}

/// Draws the header and the current view.
pub fn show(ui: &mut Ui, pal: &Palette, data: &Data, view: &mut ViewState, actions: &mut Vec<Action>) {
    // Filled again as the lists draw (the Log view has none).
    view.queue.visible.clear();
    header(ui, pal, data, view, actions);
    if view.open_nzb.visible {
        open_nzb_row(ui, pal, view, actions);
    }
    ui.add_space(14.0);
    let now = ui.input(|i| i.time);
    view.queue.note_finishes(&data.jobs, now);
    match view.tab {
        Tab::Log => {
            docks::messages(ui, pal, data, view.docks.log_filter, &view.docks.log_search, !view.reduced_motion, actions);
        }
        tab => {
            egui::ScrollArea::vertical().id_salt(("center", tab as u8)).auto_shrink([false, false]).show(ui, |ui| {
                ui.set_width(ui.available_width());
                match tab {
                    Tab::History => history_view(ui, pal, data, view, actions),
                    _ => queue_view(ui, pal, data, view, actions),
                }
            });
        }
    }
}

/// `"1 finished  ·  1 failed"`.
pub fn history_summary(data: &Data) -> String {
    let all = data.history();
    let failed = all.iter().filter(|j| j.state == JobState::Failed).count();
    if failed > 0 {
        format!("{} finished  ·  {failed} failed", all.len() - failed)
    } else {
        format!("{} finished", all.len())
    }
}

fn header(ui: &mut Ui, pal: &Palette, data: &Data, view: &mut ViewState, actions: &mut Vec<Action>) {
    let (title, summary) = match view.tab {
        Tab::Queue => ("Queue", queue_summary(data)),
        Tab::History => ("History", history_summary(data)),
        Tab::Log => {
            let n = data.logs.len();
            ("Log", format!("{n} line{}  ·  newest first", if n == 1 { "" } else { "s" }))
        }
    };
    let wrap = ui.available_width() < HEADER_WRAP_W;
    let titles = |ui: &mut Ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.label(RichText::new(title).font(fonts::semibold_id(24.0)).color(pal.text));
            ui.add(egui::Label::new(RichText::new(summary.clone()).font(fonts::mono_id(12.5)).color(pal.muted)).truncate());
        });
    };
    if wrap {
        titles(ui);
        ui.add_space(8.0);
        ui.horizontal(|ui| header_actions(ui, pal, data, view, actions));
    } else {
        ui.horizontal(|ui| {
            titles(ui);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| header_actions(ui, pal, data, view, actions));
        });
    }
}

/// One header action, drawn into the header row.
type HeaderItem<'a> = Box<dyn FnOnce(&mut Ui, &mut ViewState, &mut Vec<Action>) + 'a>;

/// The header's actions, right to left: Add NZB (left to the empty queue's own call to
/// action when that shows), the queue control, the filter and the name search (on the
/// Log view: its level filter).
fn header_actions(ui: &mut Ui, pal: &Palette, data: &Data, view: &mut ViewState, actions: &mut Vec<Action>) {
    ui.spacing_mut().item_spacing.x = 8.0;
    let rtl = ui.layout().prefer_right_to_left();
    let mut items: Vec<HeaderItem<'_>> = Vec::new();
    if !(view.tab == Tab::Queue && data.queue().is_empty()) {
        items.push(Box::new(|ui, view, actions| {
            let r = kit_button(ui, pal, ButtonKind::Primary, Some(Icon::Plus), "Add NZB", !view.dialog_open)
                .on_hover_text("Choose NZB files (Ctrl+O), or drop .nzb files onto the window");
            if r.clicked() {
                actions.push(Action::BrowseNzb);
            }
        }));
    }
    items.push(Box::new(|ui, _view, actions| {
        let (action, icon, label, tip, enabled) = queue_control(data);
        let state = if enabled { ToolState::Accent } else { ToolState::Unavailable };
        if tool_button(ui, pal, icon, label, true, state).on_hover_text(tip).clicked() && enabled {
            actions.push(action);
        }
    }));
    match view.tab {
        Tab::Log => {
            items.push(Box::new(|ui, view, _actions| docks::log_filter_chips(ui, pal, &mut view.docks.log_filter)));
            items.push(Box::new(|ui, view, _actions| {
                search_field(
                    ui,
                    pal,
                    &mut view.docks.log_search,
                    &mut view.queue.focus_filter,
                    "log-search",
                    "Search the log",
                    "Show only lines that contain this text (Ctrl+F)",
                )
            }));
        }
        tab => {
            if tab == Tab::Queue {
                // A run started for single jobs leaves the rest queued: offer the whole queue.
                let outside = data.status.run_scope.is_some()
                    && data.queue().iter().any(|j| j.state == JobState::Queued && !data.in_run(j));
                if outside {
                    items.push(Box::new(|ui, _view, actions| {
                        if kit_button(ui, pal, ButtonKind::Outline, Some(Icon::Play), "Start all", true)
                            .on_hover_text("Download every queued job, in queue order")
                            .clicked()
                        {
                            actions.push(Action::Start);
                        }
                    }));
                }
                items.push(Box::new(|ui, view, _actions| {
                    let options: Vec<(ListFilter, &str)> = ListFilter::ALL.iter().map(|f| (*f, f.label())).collect();
                    if let Some(f) = segmented(ui, pal, Id::new("list-filter"), &options, view.list_filter) {
                        view.list_filter = f;
                    }
                }));
            }
            items.push(Box::new(|ui, view, _actions| filter_box(ui, pal, view)));
        }
    }
    // Right to left in the header row; left to right when wrapped onto their own line.
    if !rtl {
        items.reverse();
    }
    for item in items {
        item(ui, view, actions);
    }
}

fn queue_view(ui: &mut Ui, pal: &Palette, data: &Data, view: &mut ViewState, actions: &mut Vec<Action>) {
    let all = data.queue();
    if all.is_empty() {
        empty_queue(ui, pal, data, view, actions);
    } else {
        let jobs: Vec<&Job> = all
            .into_iter()
            .filter(|j| matches_filter(&j.name, &view.filter) && view.list_filter.admits(j.state))
            .collect();
        if jobs.is_empty() {
            no_match(ui, pal, &view.filter);
        } else {
            list(ui, pal, data, &jobs, view, actions, RowKind::Queue);
        }
    }
    // What finished in the last day, newest first (a rolling window, so the list does not
    // empty itself at midnight).
    let since = now_unix() - RECENT_SECS;
    let recent: Vec<&Job> = data
        .history()
        .into_iter()
        .filter(|j| j.stats.finished_at.is_some_and(|t| t >= since))
        .filter(|j| matches_filter(&j.name, &view.filter))
        .take(RECENT_MAX)
        .collect();
    if !recent.is_empty() {
        ui.add_space(20.0);
        section_title(ui, pal, "Recently finished");
        ui.add_space(6.0);
        list(ui, pal, data, &recent, view, actions, RowKind::Finished);
    }
}

fn history_view(ui: &mut Ui, pal: &Palette, data: &Data, view: &mut ViewState, actions: &mut Vec<Action>) {
    let all = data.history();
    if all.is_empty() {
        ui.add_space(36.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(EMPTY_HISTORY).size(14.0).color(pal.muted));
            ui.add_space(4.0);
            ui.label(RichText::new("Finished and failed downloads are listed here.").small().color(pal.faint));
        });
        return;
    }
    clear_history_row(ui, pal, all.len(), view, actions);
    ui.add_space(8.0);
    let jobs: Vec<&Job> = all.into_iter().filter(|j| matches_filter(&j.name, &view.filter)).collect();
    if jobs.is_empty() {
        no_match(ui, pal, &view.filter);
        return;
    }
    list(ui, pal, data, &jobs, view, actions, RowKind::Finished);
}

/// Clear history, with its inline confirmations (files stay on disk unless the second
/// confirmation says otherwise).
fn clear_history_row(ui: &mut Ui, pal: &Palette, n: usize, view: &mut ViewState, actions: &mut Vec<Action>) {
    let q = &mut view.queue;
    let s = if n == 1 { "" } else { "s" };
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if q.confirm_clear_files {
                if kit_button(ui, pal, ButtonKind::Text, None, "Cancel", true).clicked() {
                    q.confirm_clear_files = false;
                }
                if kit_button(ui, pal, ButtonKind::Danger, Some(Icon::Trash), "Delete", true).clicked() {
                    actions.push(Action::ClearHistoryAndFiles);
                    q.confirm_clear_files = false;
                }
                ui.label(
                    RichText::new(format!("Permanently delete the download folder{s} of {n} job{s}? This cannot be undone."))
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
                ui.label(RichText::new(format!("Remove {n} job{s} from the list? Files stay on disk.")).color(pal.text));
            } else if kit_button(ui, pal, ButtonKind::Outline, Some(Icon::Close), "Clear history", true)
                .on_hover_text("Remove finished jobs from the list (files stay on disk)")
                .clicked()
            {
                q.confirm_clear = true;
            }
        });
    });
}

/// The rows of `jobs` in one rounded container.
fn list(ui: &mut Ui, pal: &Palette, data: &Data, jobs: &[&Job], view: &mut ViewState, actions: &mut Vec<Action>, kind: RowKind) {
    let now = ui.input(|i| i.time);
    if view.queue.highlight.is_some() && view.queue.highlight_at.is_none() {
        view.queue.highlight_at = Some(now);
    }
    if let (Some(_), Some(at)) = (view.queue.highlight, view.queue.highlight_at) {
        if now - at >= HIGHLIGHT_SECS {
            view.queue.highlight = None;
            view.queue.highlight_at = None;
        } else {
            ui.ctx().request_repaint_after(super::motion::FRAME);
        }
    }
    if !view.queue.flashes.is_empty() {
        ui.ctx().request_repaint_after(super::motion::FRAME);
    }
    egui::Frame::new()
        .fill(pal.raised)
        .stroke(egui::Stroke::new(1.0, pal.border))
        .corner_radius(egui::CornerRadius::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            let last = jobs.len().saturating_sub(1);
            view.queue.visible.extend(jobs.iter().map(|j| j.id));
            for (i, &job) in jobs.iter().enumerate() {
                row(ui, pal, data, job, view, actions, kind, i == last);
            }
        });
}

/// The state word of a row and its color: what the job is doing, in plain words.
pub fn state_word(pal: &Palette, data: &Data, job: &Job) -> (String, egui::Color32) {
    if let Some(p) = data.status.post.as_ref().filter(|p| p.job_id == job.id) {
        let color = if p.stage == newskite_service::PostStage::Repair { pal.warning } else { pal.accent_fg };
        return (p.stage.label().to_string(), color);
    }
    match job.state {
        JobState::Downloading => ("Downloading".into(), pal.accent_fg),
        JobState::Queued => ("Queued".into(), pal.muted),
        JobState::Paused => ("Held".into(), pal.warning),
        JobState::Done => ("Done".into(), pal.positive),
        JobState::Failed => ("Failed".into(), pal.negative),
    }
}

#[allow(clippy::too_many_arguments)]
fn row(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    job: &Job,
    view: &mut ViewState,
    actions: &mut Vec<Action>,
    kind: RowKind,
    last: bool,
) {
    let id = Id::new(("job-row", kind as u8, job.id));
    let width = ui.available_width();
    // The row's own click area goes first, so the buttons drawn on it take their clicks;
    // its height is the one the row had last frame.
    let h = ui.ctx().data(|d| d.get_temp::<f32>(id)).unwrap_or(80.0);
    let rect = Rect::from_min_size(ui.cursor().min, Vec2::new(width, h));
    let resp = ui.interact(rect, id, Sense::click());
    let slot = ui.painter().add(Shape::Noop);
    let held = job.state == JobState::Paused;
    let now = ui.input(|i| i.time);
    // On the selection tint, secondary text is lifted toward the text color so it keeps
    // 4.5:1 there too.
    let selected = view.queue.selected == Some(job.id);
    let quiet = if selected { pal.muted.lerp_to_gamma(pal.text, 0.5) } else { pal.muted };

    let inner = ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(Align::Min), |ui| {
        egui::Frame::new().inner_margin(ROW_PAD).show(ui, |ui| {
            let w = width - f32::from(ROW_PAD.left) - f32::from(ROW_PAD.right);
            ui.set_width(w);
            ui.spacing_mut().item_spacing.y = 6.0;
            first_line(ui, pal, data, job, view, actions, kind, w, now, quiet);
            subline(ui, pal, data, job, kind, quiet);
            ui.add_space(2.0);
            let pct = format::percent(data.shown_done(job), job.bytes_total);
            match data.strips.get(&job.id) {
                Some(cells) => {
                    let label = format!("Articles of {}: {pct}% downloaded", job.name);
                    articles::strip(ui, pal, cells, held, w, pal.raised, &label);
                }
                None => {
                    let f = if job.state == JobState::Done { 1.0 } else { format::fraction(data.shown_done(job), job.bytes_total) };
                    articles::progress_line(ui, pal, f, held, w);
                }
            }
        });
    });
    let real = inner.response.rect;
    if selected && view.queue.reveal {
        ui.scroll_to_rect(real, None);
        view.queue.reveal = false;
    }
    if (real.height() - h).abs() > 0.5 {
        ui.ctx().data_mut(|d| d.insert_temp(id, real.height()));
        ui.ctx().request_repaint();
    }

    // Background: hover wash, selection tint with its accent rail, the finish wash, and a
    // hairline under every row but the last.
    let ctx = ui.ctx();
    let hover = ctx.animate_bool(id.with("hover"), resp.contains_pointer());
    let sel = ctx.animate_bool(id.with("selected"), selected);
    let mut shapes = Vec::new();
    if sel > 0.0 {
        // Half the soft accent: secondary text on it keeps 4.5:1; the rail marks it.
        shapes.push(Shape::rect_filled(real, 0.0, Palette::fade(pal.accent_soft, 0.5 * sel)));
    }
    if hover > 0.0 {
        shapes.push(Shape::rect_filled(real, 0.0, Palette::fade(pal.hover, hover * if selected { 0.5 } else { 1.0 })));
    }
    if !view.reduced_motion {
        if let Some((state, strength)) = view.queue.flash(job.id, now) {
            let tint = if state == JobState::Failed { pal.negative_soft } else { pal.positive_soft };
            shapes.push(Shape::rect_filled(real, 0.0, Palette::fade(tint, strength)));
        }
    }
    if sel > 0.0 {
        let rail = Rect::from_min_size(real.min, Vec2::new(3.0, real.height()));
        shapes.push(Shape::rect_filled(rail, 0.0, Palette::fade(pal.accent, sel)));
    }
    if !last {
        shapes.push(Shape::hline(real.x_range(), real.bottom() - 0.5, egui::Stroke::new(1.0, pal.border)));
    }
    ui.painter().set(slot, Shape::Vec(shapes));

    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if kind == RowKind::Finished && resp.double_clicked() {
        actions.push(Action::OpenJobFolder(job.id));
    } else if resp.clicked() {
        actions.push(Action::Select(Some(job.id)));
    }
    let place = data
        .queue_order
        .iter()
        .position(|&i| i == job.id)
        .map(|p| (p == 0, p + 1 == data.queue_order.len()));
    resp.context_menu(|ui| job_menu(ui, data, job, if kind == RowKind::Queue { place } else { None }, &mut view.queue, actions));
}

/// Name, state word, figure and the row's controls.
#[allow(clippy::too_many_arguments)]
fn first_line(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    job: &Job,
    view: &mut ViewState,
    actions: &mut Vec<Action>,
    kind: RowKind,
    w: f32,
    now: f64,
    quiet: egui::Color32,
) {
    let (word, word_color) = state_word(pal, data, job);
    let word_color = if word_color == pal.muted { quiet } else { word_color };
    let done = data.shown_done(job);
    // The figure: progress and speed while queued or downloading; size and the finish
    // time once finished.
    let mut figures: Vec<(String, Option<String>)> = Vec::new();
    match kind {
        RowKind::Queue => {
            if let Some(p) = data.status.post.as_ref().filter(|p| p.job_id == job.id) {
                if let Some(m) = p.permille {
                    figures.push((format!("{}%", m / 10), None));
                }
            } else if let Some(live) = live_subline(job, done, data.speed()) {
                figures.push((format!("{}%", format::percent(done, job.bytes_total)), None));
                figures.push((live, None));
            } else if done > 0 {
                figures.push((
                    format!(
                        "{}%  ·  {} / {}",
                        format::percent(done, job.bytes_total),
                        format::bytes(done),
                        format::bytes(job.bytes_total)
                    ),
                    None,
                ));
            } else {
                figures.push((format::bytes(job.bytes_total), None));
            }
        }
        RowKind::Finished => match job.stats.finished_at {
            // Relative ("58 min ago"), the exact local and UTC times on hover.
            Some(t) => figures.push((
                format!("{}  ·  {}", format::bytes(job.bytes_total), format::ago(t, now_unix())),
                Some(format!("Finished at {} · {}", clock_hm(t), format::timestamp_utc(t))),
            )),
            None => figures.push((format::bytes(job.bytes_total), None)),
        },
    }
    // While an archive password is typed in the row, its editor takes the figure's room.
    let controls_w = if view.queue.password_edit.as_ref().is_some_and(|(id, _)| *id == job.id) {
        figures.clear();
        PASSWORD_EDIT_W
    } else {
        CONTROLS_W
    };
    let mono = fonts::mono_id(12.0);
    let word_font = fonts::semibold_id(13.0);
    let measure = |t: &str, f: &egui::FontId| ui.painter().layout_no_wrap(t.to_owned(), f.clone(), pal.text).size().x;
    let mid_w = measure(&word, &word_font)
        + figures.iter().map(|(t, _)| measure(t, &mono) + 12.0).sum::<f32>()
        + 16.0;
    let name_w = (w - controls_w - mid_w - 8.0).max(90.0);
    ui.horizontal(|ui| {
        ui.set_min_height(24.0);
        ui.spacing_mut().item_spacing.x = 0.0;
        let highlight = (view.queue.highlight == Some(job.id)).then(|| {
            let at = view.queue.highlight_at.unwrap_or(now);
            if view.reduced_motion { 1.0 } else { ((now - at) / HIGHLIGHT_SECS).clamp(0.0, 1.0) as f32 }
        });
        let name_color = match highlight {
            Some(t) => pal.accent_fg.lerp_to_gamma(pal.text, t),
            None => pal.text,
        };
        ui.allocate_ui_with_layout(Vec2::new(name_w, 24.0), Layout::left_to_right(Align::Center), |ui| {
            ui.add(
                egui::Label::new(RichText::new(&job.name).font(fonts::semibold_id(14.5)).color(name_color))
                    .truncate()
                    .selectable(false),
            )
            .on_hover_text(job.source_path.display().to_string());
        });
        ui.add_space(12.0);
        let r = ui.add(egui::Label::new(RichText::new(&word).font(word_font.clone()).color(word_color)).selectable(false));
        if job.state == JobState::Paused {
            r.on_hover_text("Held: queue runs skip it until you start it");
        } else if job.state == JobState::Queued && !data.in_run(job) {
            r.on_hover_text("Waiting for Start");
        }
        for (text, tip) in figures {
            ui.add_space(12.0);
            let r = ui.add(egui::Label::new(RichText::new(text).font(mono.clone()).color(quiet)).selectable(false));
            if let Some(tip) = tip {
                r.on_hover_text(tip);
            }
        }
        // The controls sit at the right end, in a box of their own.
        let room = ui.available_width() - controls_w;
        if room > 0.0 {
            ui.add_space(room);
        }
        ui.allocate_ui_with_layout(Vec2::new(controls_w.min(ui.available_width()), 24.0), Layout::left_to_right(Align::Center), |ui| {
            // Laid out left to right after a spacer, so the buttons end flush right (an
            // inline confirmation takes the whole box).
            let buttons = match kind {
                RowKind::Queue => 4.0,
                RowKind::Finished => if job.state == JobState::Failed { 6.0 } else { 3.0 },
            };
            let editing = view.queue.password_edit.as_ref().is_some_and(|(id, _)| *id == job.id)
                || view.queue.confirm_remove == Some(job.id)
                || view.queue.confirm_delete == Some(job.id);
            if !editing {
                let used = buttons * super::widgets::ICON_BUTTON + (buttons - 1.0) * 4.0;
                ui.add_space((ui.available_width() - used).max(0.0));
            }
            match kind {
                RowKind::Queue => controls_cell(ui, pal, data, job, &mut view.queue, actions),
                RowKind::Finished => history_actions(ui, pal, data, job, &mut view.queue, actions),
            }
        });
    });
}

/// `"10:42"`: the local time of `ts`, to the minute.
fn clock_hm(ts: i64) -> String {
    let full = docks::clock(ts);
    full.get(..5).unwrap_or(&full).to_owned()
}

/// The second line: the folder, why a job is held, the failure, or how a finished job went.
fn subline(ui: &mut Ui, pal: &Palette, data: &Data, job: &Job, kind: RowKind, quiet: egui::Color32) {
    let (text, color) = if let Some(p) = data.status.post.as_ref().filter(|p| p.job_id == job.id) {
        (p.detail.clone(), quiet)
    } else {
        match (&job.error, job.state, kind) {
            (Some(e), JobState::Failed, _) => (e.clone(), pal.negative),
            (_, JobState::Paused, _) => ("Held: queue runs skip it until you start it".to_owned(), quiet),
            (_, JobState::Done, RowKind::Finished) => {
                let mut s = if job.stats.download_ms >= 1000 { finished_subline(job) } else { home_relative(&job.dest_dir) };
                if let Some((q, _)) = result_qualifier(pal, job) {
                    if !s.contains(&q) {
                        s.push_str(&format!("  ·  {q}"));
                    }
                }
                (s, quiet)
            }
            _ => (home_relative(&job.dest_dir), quiet),
        }
    };
    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(color)).truncate().selectable(false));
}
