//! The queue's state and the pieces its views share (see [`super::center`]): queue order
//! and summaries, the start/pause toggle of a job, the row controls (move, password,
//! folder, remove, with their inline confirmations), the context menu, the name filter
//! and the empty queue. Queue rows list pending jobs in the order a run downloads them;
//! History lists finished jobs newest first.

use std::collections::HashMap;

use eframe::egui::{self, Align, Layout, RichText, Sense, TextEdit, Ui};
use newskite_service::{Job, JobMove, JobState};
use newskite_types::format;

use super::app::{Action, Data, Tab, ViewState};
use super::brand;
use super::fonts;
use super::motion::Beat;
use super::theme::Palette;
use super::widgets::{
    article_grid, compact_button, icon_button, icon_button_enabled, keycap, kit_button,
    paint_icon, ButtonKind, Icon,
};

/// Seconds a row's finish (done or failed) wash takes to fade.
const FLASH_SECS: f64 = 1.6;
/// Seconds left at the current speed for `remaining` bytes, if a speed is known.
pub fn eta_secs(remaining: u64, speed_bps: u64) -> Option<u64> {
    if speed_bps == 0 {
        return None;
    }
    Some(remaining.div_ceil(speed_bps))
}

/// Exact empty-state text of the Queue tab.
pub const EMPTY_QUEUE: &str = "The queue is empty. Add an NZB to begin.";
/// Exact empty-state text of the Queue tab before any server is set up.
pub const EMPTY_FIRST_RUN: &str = "Set up your news server to begin.";
/// Exact empty-state text of the History tab.
pub const EMPTY_HISTORY: &str = "Nothing has finished yet.";

/// Width of the name filter field.
const FILTER_W: f32 = 220.0;

/// Center-area UI state.
#[derive(Default)]
pub struct QueueState {
    /// The selected job (its details show in the detail pane).
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
    /// The job whose password editor already took the keyboard (it asks once, on opening).
    pub password_focused: Option<i64>,
    /// The password being typed shows in clear.
    pub password_shown: bool,
    /// Height of the row measured last: what a row off screen is taken to be.
    pub row_h: Option<f32>,
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
    /// The rows on screen, top to bottom, as last drawn (what ↑/↓ walk through).
    pub visible: Vec<i64>,
    /// Scroll the selected row into view next frame (it was chosen from the keyboard).
    pub reveal: bool,
    /// The queue row being dragged to a new place, if one is.
    pub drag: Option<i64>,
    /// The queue rows on screen this frame, top to bottom, with their rectangles (where
    /// a dragged row may be dropped).
    pub row_rects: Vec<(i64, egui::Rect)>,
}

/// Where a dragged row would be dropped: the number of other rows (in `rows`, top to
/// bottom, the dragged one left out) whose middle lies above `y`, so the drop goes before
/// the row at that index, or after the last row when `y` is below them all.
pub fn drop_index(rows: &[(i64, egui::Rect)], dragged: i64, y: f32) -> usize {
    rows.iter().filter(|(id, r)| *id != dragged && r.center().y < y).count()
}

/// The queue order after dropping `dragged` at `at` (see [`drop_index`]) among `visible`,
/// the rows on screen top to bottom: `order` with `dragged` moved before the row that was
/// at `at`, or after the last row on screen when `at` is past them all. Rows a filter
/// hides keep their places next to their visible neighbors. `None` when nothing moves.
pub fn dropped_order(order: &[i64], visible: &[i64], dragged: i64, at: usize) -> Option<Vec<i64>> {
    if !order.contains(&dragged) {
        return None;
    }
    let others: Vec<i64> = visible.iter().copied().filter(|&id| id != dragged).collect();
    let mut new: Vec<i64> = order.iter().copied().filter(|&id| id != dragged).collect();
    let place = match others.get(at) {
        Some(before) => new.iter().position(|id| id == before)?,
        // Past the last row on screen; alone on screen, there is nowhere else to go.
        None => new.iter().position(|id| Some(id) == others.last())? + 1,
    };
    new.insert(place, dragged);
    (new != order).then_some(new)
}

/// A key that moves the selection through the rows on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Up,
    Down,
    Home,
    End,
}

/// The row `nav` selects among `rows` (top to bottom): ↑/↓ step from the selection, or
/// start at the bottom/top when nothing on screen is selected; Home/End jump to the ends.
/// `None` when nothing changes.
pub fn step_selection(rows: &[i64], selected: Option<i64>, nav: Nav) -> Option<i64> {
    let at = selected.and_then(|id| rows.iter().position(|&r| r == id));
    let next = match (nav, at) {
        (Nav::Home, _) => rows.first(),
        (Nav::End, _) => rows.last(),
        (Nav::Up, Some(i)) => rows.get(i.saturating_sub(1)),
        (Nav::Down, Some(i)) => rows.get(i + 1),
        (Nav::Up, None) => rows.last(),
        (Nav::Down, None) => rows.first(),
    };
    next.copied().filter(|&id| Some(id) != selected)
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
        // Every job was just inserted, so only a shorter list can leave stale entries (a
        // per-frame scan of jobs × jobs would cost milliseconds on a long history).
        if self.last_states.len() != jobs.len() {
            let ids: std::collections::HashSet<i64> = jobs.iter().map(|j| j.id).collect();
            self.last_states.retain(|id, _| ids.contains(id));
        }
        let states = &self.last_states;
        self.flashes.retain(|id, (_, at)| states.contains_key(id) && now - *at < FLASH_SECS);
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
pub(crate) fn toggle_look(data: &Data, job: &Job) -> Option<(Icon, &'static str)> {
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

pub(crate) fn filter_box(ui: &mut Ui, pal: &Palette, view: &mut ViewState) {
    search_field(
        ui,
        pal,
        &mut view.filter,
        &mut view.queue.focus_filter,
        "job-filter",
        "Filter by name",
        &format!("Show only jobs whose name contains this text ({}+F)", super::widgets::CMD_KEY),
    );
}

/// A search field with its magnifier: `focus` asks for the keyboard (Ctrl+F), Esc in it
/// clears it.
pub(crate) fn search_field(
    ui: &mut Ui,
    pal: &Palette,
    text: &mut String,
    focus: &mut bool,
    salt: &str,
    hint: &str,
    tip: &str,
) {
    let width = FILTER_W.min(ui.available_width() - 8.0).max(120.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 28.0), Sense::hover());
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let resp = child.add(
        TextEdit::singleline(text)
            .id_salt(salt)
            .hint_text(RichText::new(hint).color(pal.muted))
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
    if *focus {
        resp.request_focus();
        *focus = false;
    }
    if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        text.clear();
    }
    resp.on_hover_text(tip);
}

pub(crate) fn empty_queue(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    view: &ViewState,
    actions: &mut Vec<Action>,
) {
    ui.add_space(24.0);
    // A drop zone: a dashed, rounded frame whose dashes drift slowly around it while the
    // pointer is over it (an idle window stays still, and costs nothing).
    let finished = data.finished_count();
    let first_run = data.settings.first_usable_server().is_none();
    let zone_w = (ui.available_width() - 48.0).clamp(280.0, 580.0);
    let zone_h = if finished > 0 { 286.0 } else { 256.0 };
    let zone = egui::Rect::from_center_size(
        egui::pos2(ui.max_rect().center().x, ui.cursor().top() + zone_h / 2.0),
        egui::vec2(zone_w, zone_h),
    );
    ui.allocate_rect(zone, Sense::hover());
    let ambient = zone_beat(ui, zone, view.reduced_motion);
    ui.painter().rect_filled(zone, egui::CornerRadius::same(16), Palette::fade(pal.raised, 0.55));
    let drift = if ambient.moving { ambient.phase * 22.0 } else { 0.0 };
    ui.painter().extend(egui::Shape::dashed_line_with_offset(
        &rounded_outline(zone, 16.0),
        egui::Stroke::new(1.5, pal.border_strong),
        &[8.0],
        &[6.0],
        drift,
    ));
    let mut inner = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(zone.shrink2(egui::vec2(24.0, 28.0)))
            .layout(Layout::top_down(Align::Center)),
    );
    let ui = &mut inner;
    {
        article_grid(ui, pal, ambient.moving.then_some(ambient.phase));
        ui.add_space(20.0);
        let cmd = super::widgets::CMD_KEY;
        let browse_tip = format!("Choose NZB files ({cmd}+O)");
        if first_run {
            // Nothing can download yet: the server comes first, the NZB second.
            ui.label(RichText::new(EMPTY_FIRST_RUN).font(fonts::semibold_id(15.5)).color(pal.text));
            ui.add_space(5.0);
            ui.label(
                RichText::new("Newskite downloads from your Usenet provider. Add its address and sign-in, then an NZB.")
                    .small()
                    .color(pal.muted),
            );
            ui.add_space(16.0);
            let w = kit_width(ui, pal, Some(Icon::Gear), "Set up server…") + 8.0 + kit_width(ui, pal, Some(Icon::OpenNzb), "Add NZB…");
            ui.allocate_ui_with_layout(egui::vec2(w, 30.0), Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if kit_button(ui, pal, ButtonKind::Primary, Some(Icon::Gear), "Set up server…", true)
                    .on_hover_text(format!("Open Settings ({cmd}+,)"))
                    .clicked()
                {
                    actions.push(Action::OpenSettings);
                }
                if kit_button(ui, pal, ButtonKind::Outline, Some(Icon::OpenNzb), "Add NZB…", !view.dialog_open)
                    .on_hover_text(browse_tip.as_str())
                    .clicked()
                {
                    actions.push(Action::BrowseNzb);
                }
            });
        } else {
            ui.label(RichText::new(EMPTY_QUEUE).font(fonts::semibold_id(15.5)).color(pal.text));
            ui.add_space(5.0);
            ui.label(
                RichText::new("Drop .nzb files onto the window, or set a watch folder in Settings.")
                    .small()
                    .color(pal.muted),
            );
            ui.add_space(16.0);
            if kit_button(ui, pal, ButtonKind::Primary, Some(Icon::OpenNzb), "Add NZB…", !view.dialog_open)
                .on_hover_text(browse_tip.as_str())
                .clicked()
            {
                actions.push(Action::BrowseNzb);
            }
        }
        ui.add_space(14.0);
        // Keyboard hints, centered under the call to action.
        let hints: [(&[&str], &str); 2] = [(&[cmd, "O"], "browse"), (&[cmd, "V"], "paste copied files")];
        let font = egui::TextStyle::Small.resolve(ui.style());
        let row_w: f32 = hints
            .iter()
            .map(|(keys, word)| {
                keys.iter().map(|k| (k.len() as f32 * 6.5 + 10.0).max(20.0) + 3.0).sum::<f32>()
                    + 4.0
                    + ui.painter().layout_no_wrap((*word).to_owned(), font.clone(), pal.faint).size().x
            })
            .sum::<f32>()
            + 22.0;
        ui.allocate_ui_with_layout(egui::vec2(row_w, 22.0), Layout::left_to_right(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            for (i, (keys, word)) in hints.iter().enumerate() {
                if i > 0 {
                    ui.add_space(16.0);
                }
                for k in *keys {
                    keycap(ui, pal, k);
                }
                ui.add_space(2.0);
                ui.label(RichText::new(*word).small().color(pal.faint));
            }
        });
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
    }
}

/// The empty queue's ambient beat: its clock only runs while the pointer is over `zone`,
/// so the dashes drift on from where they stopped and an idle window asks for no frames.
fn zone_beat(ui: &Ui, zone: egui::Rect, reduced_motion: bool) -> Beat {
    let id = egui::Id::new("empty-zone-clock");
    let mut clock = ui.data(|d| d.get_temp::<f64>(id)).unwrap_or(0.0);
    if !reduced_motion && ui.rect_contains_pointer(zone) {
        clock += f64::from(ui.input(|i| i.stable_dt).min(0.1));
        ui.data_mut(|d| d.insert_temp(id, clock));
        ui.ctx().request_repaint_after(super::motion::AMBIENT_FRAME);
    }
    Beat::at(clock, true, reduced_motion, brand::PERIOD_AMBIENT * 2.0)
}

/// Width a [`kit_button`] with this icon and label takes.
fn kit_width(ui: &Ui, pal: &Palette, icon: Option<Icon>, text: &str) -> f32 {
    let font = fonts::semibold_id(12.0);
    let label = ui.painter().layout_job(super::widgets::spaced(&text.to_uppercase(), font, pal.text, 0.72)).size().x;
    label + if icon.is_some() { 23.0 } else { 0.0 } + 28.0
}

/// Points around `rect` with corners of `radius`, closed (for dashed outlines).
pub(crate) fn rounded_outline(rect: egui::Rect, radius: f32) -> Vec<egui::Pos2> {
    const STEPS: usize = 6;
    let r = radius.min(rect.width() / 2.0).min(rect.height() / 2.0);
    let corners = [
        (egui::pos2(rect.right() - r, rect.top() + r), -90.0f32),
        (egui::pos2(rect.right() - r, rect.bottom() - r), 0.0),
        (egui::pos2(rect.left() + r, rect.bottom() - r), 90.0),
        (egui::pos2(rect.left() + r, rect.top() + r), 180.0),
    ];
    let mut pts = vec![egui::pos2(rect.left() + r, rect.top())];
    for (c, start) in corners {
        for s in 0..=STEPS {
            let a = (start + 90.0 * s as f32 / STEPS as f32).to_radians();
            pts.push(c + egui::vec2(a.cos(), a.sin()) * r);
        }
    }
    pts.push(pts[0]);
    pts
}

/// What an empty filtered list says: the search that matched nothing, or the state
/// filter that has no jobs.
pub fn no_match_text(filter: &str, list: super::center::ListFilter) -> String {
    use super::center::ListFilter;
    let f = filter.trim();
    match (f.is_empty(), list) {
        (true, ListFilter::Active) => "No active jobs.".to_owned(),
        (true, ListFilter::Held) => "No held jobs.".to_owned(),
        (true, ListFilter::All) => "No jobs.".to_owned(),
        (false, ListFilter::Active) => format!("No active jobs match “{f}”."),
        (false, ListFilter::Held) => format!("No held jobs match “{f}”."),
        (false, ListFilter::All) => format!("No jobs match “{f}”."),
    }
}

/// The empty filtered list: what matched nothing, and a way back to every job.
pub(crate) fn no_match(ui: &mut Ui, pal: &Palette, view: &mut ViewState, list: super::center::ListFilter) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(no_match_text(&view.filter, list)).size(14.0).color(pal.muted));
        ui.add_space(6.0);
        let label = if list == super::center::ListFilter::All { "Clear filter" } else { "Show all jobs" };
        if kit_button(ui, pal, ButtonKind::Text, None, label, true).clicked() {
            view.filter.clear();
            view.list_filter = super::center::ListFilter::All;
        }
    });
}

/// `path` with the home folder written as `~` (sublines show many folders under it).
pub fn home_relative(path: &std::path::Path) -> String {
    match std::env::home_dir().and_then(|h| path.strip_prefix(&h).ok().map(|r| r.to_path_buf())) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
}

/// `"8m 12s · 25.3 MB/s · extracted"` for a finished job (measured figures only).
pub fn finished_subline(job: &Job) -> String {
    let st = &job.stats;
    let total_ms = st.download_ms + st.post_ms;
    let mut parts = vec![format::duration_short(total_ms / 1000)];
    if let Some(s) = st.avg_speed(job.bytes_done) {
        parts.push(format::speed(s));
    }
    if let Some(o) = &st.outcome {
        if o.extracted_to.is_some() {
            parts.push("extracted".into());
        }
    }
    parts.join("  ·  ")
}

/// The word under a history row's chip: what made the result what it is.
pub(crate) fn result_qualifier(pal: &Palette, job: &Job) -> Option<(String, egui::Color32)> {
    let o = job.stats.outcome.as_ref()?;
    match job.state {
        JobState::Failed => o.failure.map(|k| (k.label().to_string(), pal.negative)),
        _ if o.repaired() => Some(("repaired".into(), pal.warning)),
        _ if o.extracted_to.is_some() => Some(("extracted".into(), pal.muted)),
        _ if o.verify.as_ref().is_some_and(|v| v.state == newskite_service::StepState::Ok) => {
            Some(("verified".into(), pal.muted))
        }
        _ => None,
    }
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
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
pub(crate) fn inline_editors(
    ui: &mut Ui,
    pal: &Palette,
    job: &Job,
    q: &mut super::queue::QueueState,
    actions: &mut Vec<Action>,
) -> bool {
    if q.password_edit.is_none() {
        q.password_focused = None;
        q.password_shown = false;
    }
    let take_focus = q.password_focused != Some(job.id);
    let shown = &mut q.password_shown;
    if let Some((id, text)) = q.password_edit.as_mut() {
        if *id == job.id {
            q.password_focused = Some(job.id);
            // Right to left, so the field takes the box's width after Save and Cancel.
            let (save, cancel, enter) = ui
                .allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 24.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let cancel = icon_button(ui, pal, Icon::Close, pal.muted, "Cancel").clicked();
                        let save = compact_button(ui, pal, ButtonKind::Outline, "Save", true).clicked();
                        if compact_button(ui, pal, ButtonKind::Text, if *shown { "Hide" } else { "Show" }, true)
                            .on_hover_text(if *shown { "Hide the password" } else { "Show the password to check it" })
                            .clicked()
                        {
                            *shown = !*shown;
                        }
                        let resp = ui.add(
                            TextEdit::singleline(text)
                                .password(!*shown)
                                .hint_text("Archive password")
                                .desired_width(ui.available_width()),
                        );
                        if take_focus {
                            resp.request_focus();
                        }
                        (save, cancel, resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    },
                )
                .inner;
            if save || enter {
                let pw = (!text.is_empty()).then(|| text.clone());
                actions.push(Action::SetJobPassword(job.id, pw));
                q.password_edit = None;
            } else if cancel {
                q.password_edit = None;
            }
            return true;
        }
    }
    if q.confirm_delete == Some(job.id) {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new("Delete files?").color(pal.negative))
            .on_hover_text(format!("Permanently delete {} and remove the job", job.dest_dir.display()));
        if compact_button(ui, pal, ButtonKind::Danger, "Yes", true).clicked() {
            actions.push(Action::DeleteJobFiles(job.id));
            q.confirm_delete = None;
        }
        if compact_button(ui, pal, ButtonKind::Text, "No", true).clicked() {
            q.confirm_delete = None;
        }
        return true;
    }
    if q.confirm_remove == Some(job.id) {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new("Remove?").color(pal.negative));
        if compact_button(ui, pal, ButtonKind::Danger, "Yes", true).clicked() {
            actions.push(Action::RemoveJob(job.id));
            q.confirm_remove = None;
        }
        if compact_button(ui, pal, ButtonKind::Text, "No", true).clicked() {
            q.confirm_remove = None;
        }
        return true;
    }
    false
}

pub(crate) fn controls_cell(
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
    let up = icon_button_enabled(
        ui,
        pal,
        Icon::ChevronUp,
        pal.muted,
        if first { "Already first in the queue" } else { "Move up (Alt+↑)" },
        !first,
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

pub(crate) fn history_actions(
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
        let servers = data.settings.usable_servers();
        let tip = format!("Retry on the backup server: {}", super::serverstrip::backup_retry_explainer(&servers));
        let ok = servers.len() >= 2;
        let r = icon_button_enabled(ui, pal, Icon::RetryBackup, if ok { pal.accent_fg } else { pal.muted }, &tip, ok);
        if r.clicked() && servers.len() >= 2 {
            actions.push(Action::RetryOnBackup(job.id));
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
pub(crate) fn job_menu(
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

    #[test]
    fn a_dropped_row_lands_before_its_target_and_hidden_rows_keep_their_place() {
        let order = [1, 2, 3, 4, 5];
        // Every row on screen: drop before a row, or after the last one.
        assert_eq!(dropped_order(&order, &order, 4, 0), Some(vec![4, 1, 2, 3, 5]));
        assert_eq!(dropped_order(&order, &order, 1, 1), Some(vec![2, 1, 3, 4, 5]), "before the second other row");
        assert_eq!(dropped_order(&order, &order, 1, 4), Some(vec![2, 3, 4, 5, 1]), "past the end");
        assert_eq!(dropped_order(&order, &order, 3, 2), None, "dropped where it was");
        assert_eq!(dropped_order(&order, &order, 9, 0), None, "not in the queue");
        // A filter hides 2 and 4: dropping 5 before 3 puts it between 2 and 3.
        assert_eq!(dropped_order(&order, &[1, 3, 5], 5, 1), Some(vec![1, 2, 5, 3, 4]));
        // Dropped after the last visible row (3): right behind it, before the hidden 4.
        assert_eq!(dropped_order(&order, &[1, 3], 1, 1), Some(vec![2, 3, 1, 4, 5]));
        assert_eq!(dropped_order(&order, &[1, 3, 5], 1, 2), Some(vec![2, 3, 4, 5, 1]), "behind the last on screen");
        assert_eq!(dropped_order(&order, &[2], 2, 0), None, "alone on screen");
        let r = |top: f32| (0, egui::Rect::from_min_size(egui::pos2(0.0, top), egui::vec2(10.0, 10.0)));
        let rows: Vec<(i64, egui::Rect)> = [(1, r(0.0).1), (2, r(10.0).1), (3, r(20.0).1)].to_vec();
        assert_eq!(drop_index(&rows, 2, 3.0), 0);
        assert_eq!(drop_index(&rows, 2, 7.0), 1, "below the first row's middle");
        assert_eq!(drop_index(&rows, 2, 27.0), 2, "below every other row");
        assert_eq!(drop_index(&rows, 9, 100.0), 3);
    }

    #[test]
    fn arrow_keys_walk_the_rows_on_screen() {
        let rows = [7, 3, 9];
        assert_eq!(step_selection(&rows, None, Nav::Down), Some(7), "from the top");
        assert_eq!(step_selection(&rows, None, Nav::Up), Some(9), "from the bottom");
        assert_eq!(step_selection(&rows, Some(7), Nav::Down), Some(3));
        assert_eq!(step_selection(&rows, Some(3), Nav::Up), Some(7));
        assert_eq!(step_selection(&rows, Some(9), Nav::Down), None, "stays at the end");
        assert_eq!(step_selection(&rows, Some(7), Nav::Up), None, "stays at the top");
        assert_eq!(step_selection(&rows, Some(3), Nav::End), Some(9));
        assert_eq!(step_selection(&rows, Some(3), Nav::Home), Some(7));
        // A selection that is not on screen (filtered out) starts over.
        assert_eq!(step_selection(&rows, Some(42), Nav::Down), Some(7));
        assert_eq!(step_selection(&[], Some(1), Nav::Down), None);
    }

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
