//! The detail pane: everything about the selected job in one place, beside the list.
//!
//! * **Header**: a progress ring, the name, the folder and what the job is doing.
//! * **Problem card** for failed jobs: what went wrong (by [`FailureKind`]), the full
//!   reason (selectable, with Copy), what to do, and the matching buttons.
//! * **Lifecycle**: Added → Download → Verify → Repair → Extract → Clean up → result,
//!   each step marked done, live (with its fraction), failed, skipped or waiting, with
//!   its duration and a one-line detail. Built by [`timeline`] from stored outcome data
//!   and the live post-processing stage, never from guesses.
//! * **Numbers**: size, average speed, time spent, articles, repaired slices; a number is
//!   shown only when it was measured.
//! * **Article map** and **files**: every article of the job as recorded by its runs, and
//!   one lane of cells per file with `n / N` downloaded (per-file byte progress when the
//!   job has no map yet).
//! * **Log** tab: the job's own log, which survives restarts.

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use newskite_service::{FailureKind, Job, JobState, PostProgress, PostStage, StepRecord, StepState};
use newskite_types::format;

use super::app::{Action, Data};
use super::fonts;
use super::icons::{paint_icon, Icon};
use super::queue::eta_secs;
use super::theme::Palette;
use super::widgets::{compact_button, icon_button, kit_button, progress_ring, section_title, segment_track, ButtonKind};

/// Width of the side panel when it first opens.
pub const PANEL_DEFAULT_W: f32 = 400.0;
/// Narrowest the side panel may be dragged.
pub const PANEL_MIN_W: f32 = 340.0;
/// Window width from which the inspector is a side panel.
pub const SIDE_PANEL_MIN_WINDOW: f32 = 1100.0;

/// Which inspector tab is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InspectorTab {
    #[default]
    Overview,
    Log,
}

/// Inspector UI state.
#[derive(Default)]
pub struct InspectorState {
    pub tab: InspectorTab,
    /// The archive password typed into a failed job's problem card, with that job's id.
    pub password: Option<(i64, String)>,
    /// Whether that password shows in clear.
    pub show_password: bool,
}

/// How a lifecycle step is marked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mark {
    Done,
    /// Running now; fraction done when known.
    Live(Option<f32>),
    /// Finished with something worth a look (missing articles PAR2 made up for, a pause).
    Warn,
    Failed,
    Skipped,
    Waiting,
}

/// One row of the lifecycle.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub title: &'static str,
    pub mark: Mark,
    pub detail: String,
    /// Duration or time, right-aligned.
    pub time: Option<String>,
}

impl Step {
    fn new(title: &'static str, mark: Mark, detail: impl Into<String>) -> Self {
        Self { title, mark, detail: detail.into(), time: None }
    }

    fn at(mut self, time: Option<String>) -> Self {
        self.time = time;
        self
    }
}

fn ms_text(ms: u64) -> Option<String> {
    (ms > 0).then(|| if ms < 1000 { format!("{ms} ms") } else { format::duration_short(ms / 1000) })
}

fn from_record(title: &'static str, r: &StepRecord) -> Step {
    let mark = match r.state {
        StepState::Ok | StepState::Repaired => Mark::Done,
        StepState::Skipped => Mark::Skipped,
        StepState::Failed => Mark::Failed,
    };
    Step::new(title, mark, r.detail.clone()).at(ms_text(r.ms))
}

const POST_STEPS: [(PostStage, &str); 4] = [
    (PostStage::Verify, "Verify"),
    (PostStage::Repair, "Repair"),
    (PostStage::Extract, "Extract"),
    (PostStage::Cleanup, "Clean up"),
];

/// The lifecycle of `job`. `post` is the live post-processing stage (only for this job),
/// `done` the bytes shown as downloaded and `speed` the current speed (0 = unknown).
pub fn timeline(job: &Job, post: Option<&PostProgress>, done: u64, speed: u64, now: i64) -> Vec<Step> {
    let st = &job.stats;
    let outcome = st.outcome.as_ref();
    let mut v = vec![Step::new("Added", Mark::Done, format!("Added {}", format::ago(job.created_at, now)))];

    // Download.
    let avg = st.avg_speed(job.bytes_done).map(|s| format!(" · avg {}", format::speed(s))).unwrap_or_default();
    let dl_time = ms_text(st.download_ms);
    let download_failed = matches!(
        outcome.and_then(|o| o.failure),
        Some(FailureKind::Auth | FailureKind::Connection | FailureKind::Disk | FailureKind::Nzb | FailureKind::Unrepairable)
    ) || (outcome.is_some_and(|o| o.failure == Some(FailureKind::MissingArticles) && o.verify.is_none()));
    let dl = match job.state {
        JobState::Queued if st.started_at.is_none() => Step::new("Download", Mark::Waiting, "Waiting in the queue"),
        JobState::Queued => Step::new(
            "Download",
            Mark::Waiting,
            format!("Resumes at {}% when its turn comes", format::percent(done, job.bytes_total)),
        ),
        JobState::Paused => Step::new(
            "Download",
            Mark::Warn,
            format!("Held at {}% · resume it to continue", format::percent(done, job.bytes_total)),
        ),
        JobState::Downloading if post.is_none() => {
            let frac = format::fraction(done, job.bytes_total);
            let mut d = format!("{} of {}", format::bytes(done), format::bytes(job.bytes_total));
            if speed > 0 {
                d.push_str(&format!(" · {}", format::speed(speed)));
                if let Some(eta) = eta_secs(job.bytes_total.saturating_sub(done), speed) {
                    d.push_str(&format!(" · {} left", format::duration_short(eta)));
                }
            }
            Step::new("Download", Mark::Live(Some(frac)), d).at(dl_time)
        }
        JobState::Downloading => {
            Step::new("Download", Mark::Done, format!("{}{avg}", format::bytes(job.bytes_total))).at(dl_time)
        }
        JobState::Done | JobState::Failed if download_failed => {
            let d = match outcome.and_then(|o| o.failure) {
                Some(FailureKind::MissingArticles) => {
                    format!("{} article(s) missing on every server", st.articles_missing)
                }
                Some(FailureKind::Unrepairable) => match outcome.and_then(|o| o.repair_outlook) {
                    Some(r) => format!("Stopped early: {} damaged slices, at most {} recovery blocks", r.needed, r.available),
                    None => "Stopped early: too few recovery blocks".into(),
                },
                Some(k) => k.label().to_string(),
                None => "failed".into(),
            };
            Step::new("Download", Mark::Failed, d).at(dl_time)
        }
        JobState::Done | JobState::Failed if st.articles_missing > 0 => Step::new(
            "Download",
            Mark::Warn,
            format!(
                "{} of {} articles missing · handed to PAR2{avg}",
                st.articles_missing,
                st.articles_total
            ),
        )
        .at(dl_time),
        JobState::Done | JobState::Failed => {
            let articles = if st.articles_total > 0 { format!(" · {} articles", st.articles_total) } else { String::new() };
            Step::new("Download", Mark::Done, format!("{}{articles}{avg}", format::bytes(job.bytes_total))).at(dl_time)
        }
    };
    v.push(dl);

    // Post-processing.
    match (job.state, post, outcome) {
        (JobState::Downloading, Some(p), _) => {
            let cur = POST_STEPS.iter().position(|(s, _)| *s == p.stage).unwrap_or(0);
            for (i, (_, title)) in POST_STEPS.iter().enumerate() {
                v.push(match i.cmp(&cur) {
                    std::cmp::Ordering::Less if *title == "Repair" => Step::new(title, Mark::Skipped, "Not needed"),
                    std::cmp::Ordering::Less => Step::new(title, Mark::Done, ""),
                    std::cmp::Ordering::Equal => {
                        Step::new(title, Mark::Live(p.permille.map(|m| m as f32 / 1000.0)), p.detail.clone())
                    }
                    std::cmp::Ordering::Greater => Step::new(title, Mark::Waiting, ""),
                });
            }
        }
        (JobState::Done | JobState::Failed, _, Some(o)) if o.verify.is_some() || o.extract.is_some() => {
            v.push(o.verify.as_ref().map_or(Step::new("Verify", Mark::Skipped, "Not run"), |r| from_record("Verify", r)));
            v.push(o.repair.as_ref().map_or_else(
                || {
                    let verified = o.verify.as_ref().is_some_and(|r| r.state == StepState::Ok);
                    Step::new("Repair", Mark::Skipped, if verified { "Not needed: every file is correct" } else { "Not run" })
                },
                |r| from_record("Repair", r),
            ));
            v.push(o.extract.as_ref().map_or(Step::new("Extract", Mark::Skipped, "Not run"), |r| from_record("Extract", r)));
            v.push(o.cleanup.as_ref().map_or(Step::new("Clean up", Mark::Skipped, "Not run"), |r| from_record("Clean up", r)));
        }
        (JobState::Done | JobState::Failed, _, _) => {
            let why = if download_failed {
                "Not reached"
            } else if st.finished_at.is_none() {
                // Finished before this version recorded outcomes.
                "Not recorded"
            } else {
                "Post-processing was off"
            };
            for (_, title) in POST_STEPS {
                v.push(Step::new(title, Mark::Skipped, why));
            }
        }
        _ => {
            for (_, title) in POST_STEPS {
                v.push(Step::new(title, Mark::Waiting, ""));
            }
        }
    }

    // Result.
    let when = st.finished_at.map(|t| format::ago(t, now));
    v.push(match job.state {
        JobState::Done => {
            let repaired = outcome.is_some_and(|o| o.repaired());
            let d = match (repaired, outcome.and_then(|o| o.extracted_to.as_ref())) {
                (true, _) => "Complete · PAR2 repaired the damage".to_string(),
                (false, Some(_)) => "Complete · extracted".to_string(),
                _ => "Complete".to_string(),
            };
            Step::new("Done", Mark::Done, d).at(when)
        }
        JobState::Failed => Step::new(
            "Failed",
            Mark::Failed,
            outcome.and_then(|o| o.failure).map_or("See the problem above".into(), |k| capitalize(k.label())),
        )
        .at(when),
        _ => Step::new("Done", Mark::Waiting, ""),
    });
    v
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// The numbers worth showing for `job` (label, value); only measured ones.
pub fn facts(job: &Job, done: u64) -> Vec<(&'static str, String)> {
    let st = &job.stats;
    let mut v = vec![("Size", format::bytes(job.bytes_total))];
    if job.state.is_pending() {
        v.push(("Downloaded", format!("{} ({}%)", format::bytes(done), format::percent(done, job.bytes_total))));
    }
    if let Some(s) = st.avg_speed(job.bytes_done) {
        v.push(("Average speed", format::speed(s)));
    }
    if st.download_ms >= 1000 {
        v.push(("Downloading", format::duration_short(st.download_ms / 1000)));
    }
    if st.post_ms >= 1000 {
        v.push(("Post-processing", format::duration_short(st.post_ms / 1000)));
    }
    if st.articles_total > 0 {
        v.push(("Articles", st.articles_total.to_string()));
    }
    if st.articles_missing > 0 {
        v.push(("Missing", st.articles_missing.to_string()));
    }
    if let Some(o) = &st.outcome {
        if o.slices_rebuilt > 0 {
            v.push(("Slices rebuilt", o.slices_rebuilt.to_string()));
        }
        if o.files_removed > 0 {
            v.push(("Cleaned up", format!("{} files", o.files_removed)));
        }
    }
    v
}

/// The inspector body for `job`. `compact` drops the header (the dock has its own).
pub fn show(ui: &mut Ui, pal: &Palette, data: &Data, job: &Job, state: &mut InspectorState, reduced: bool, actions: &mut Vec<Action>) {
    ui.set_max_width(ui.available_width());
    header(ui, pal, data, job, reduced, actions);
    ui.add_space(10.0);
    tabs(ui, pal, data, job, state);
    ui.add_space(8.0);
    egui::ScrollArea::vertical()
        .id_salt(("inspector", state.tab as u8))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_max_width(ui.available_width());
            match state.tab {
                InspectorTab::Overview => overview(ui, pal, data, job, state, reduced, actions),
                InspectorTab::Log => log(ui, pal, data, actions),
            }
        });
}

fn header(ui: &mut Ui, pal: &Palette, data: &Data, job: &Job, reduced: bool, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        let (word, color) = super::center::state_word(pal, data, job);
        ui.label(RichText::new(word).font(fonts::semibold_id(13.0)).color(color));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icon_button(ui, pal, Icon::Close, pal.muted, "Close (Esc)").clicked() {
                actions.push(Action::Select(None));
            }
            if icon_button(ui, pal, Icon::Folder, pal.muted, "Open the download folder").clicked() {
                actions.push(Action::OpenJobFolder(job.id));
            }
        });
    });
    ui.add_space(10.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        // How far the job got, as a ring that glides to each new figure.
        let done = data.shown_done(job);
        let target = if job.state == JobState::Done { 1.0 } else { format::fraction(done, job.bytes_total) };
        let f = super::motion::ease_to(ui.ctx(), egui::Id::new(("inspector-ring", job.id)), target, 0.6, reduced);
        let color = match job.state {
            JobState::Done => pal.positive,
            JobState::Failed => pal.negative,
            JobState::Paused => pal.faint,
            _ => pal.accent,
        };
        let (r, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
        progress_ring(ui.painter(), pal, r.center(), 26.0, 6.0, f, color);
        match job.state {
            JobState::Done => paint_icon(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(18.0)), Icon::StatusOk, color),
            JobState::Failed => paint_icon(ui.painter(), Rect::from_center_size(r.center(), Vec2::splat(18.0)), Icon::StatusErr, color),
            _ => {
                ui.painter().text(
                    r.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{:.0}%", (f * 100.0).floor()),
                    fonts::mono_id(13.0),
                    pal.text,
                );
            }
        }
        ui.vertical(|ui| {
            ui.set_max_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.add_space(10.0);
            ui.add(egui::Label::new(RichText::new(&job.name).font(fonts::semibold_id(17.0)).color(pal.text)).wrap());
            ui.add(egui::Label::new(RichText::new(super::queue::home_relative(&job.dest_dir)).small().color(pal.muted)).truncate())
                .on_hover_text(format!("{}\nNZB: {}", job.dest_dir.display(), job.source_path.display()));
        });
    });
}

fn tabs(ui: &mut Ui, pal: &Palette, data: &Data, job: &Job, state: &mut InspectorState) {
    let mut current = None;
    let row = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 16.0;
        let failed = data.selected_files.iter().filter(|f| f.state == "failed").count();
        let warn = data.selected_log.iter().filter(|l| l.level != "info").count();
        let items = [
            (InspectorTab::Overview, "Overview".to_string(), (failed > 0).then_some(pal.negative)),
            (InspectorTab::Log, format!("Log {}", data.selected_log.len()), (warn > 0).then_some(pal.warning)),
        ];
        for (tab, label, dot) in items {
            let on = state.tab == tab;
            let resp = ui.add(
                egui::Label::new(RichText::new(&label).color(if on { pal.text } else { pal.muted }))
                    .sense(Sense::click())
                    .selectable(false),
            );
            let r = resp.rect;
            if on {
                current = Some(r);
            }
            if let Some(c) = dot {
                ui.painter().circle_filled(Pos2::new(r.right() + 5.0, r.top() + 3.0), 2.5, c);
            }
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                state.tab = tab;
            }
        }
        let _ = job;
    });
    let r = row.response.rect;
    ui.painter().line_segment(
        [Pos2::new(r.left(), r.bottom() + 5.0), Pos2::new(r.left() + ui.available_width(), r.bottom() + 5.0)],
        Stroke::new(1.0, pal.border),
    );
    if let Some(on) = current {
        // The accent rule glides to the chosen tab.
        let secs = ui.style().animation_time;
        let left = ui.ctx().animate_value_with_time(egui::Id::new("inspector-tab-l"), on.left(), secs);
        let right = ui.ctx().animate_value_with_time(egui::Id::new("inspector-tab-r"), on.right(), secs);
        let rule = Rect::from_x_y_ranges(left..=right, (r.bottom() + 3.0)..=(r.bottom() + 5.5));
        ui.painter().rect_filled(rule, CornerRadius::same(1), pal.accent);
    }
    ui.add_space(4.0);
}

fn overview(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    job: &Job,
    state: &mut InspectorState,
    reduced: bool,
    actions: &mut Vec<Action>,
) {
    let now = now_secs();
    let done = data.shown_done(job);
    if job.state == JobState::Failed {
        problem(ui, pal, data, job, state, actions);
        ui.add_space(12.0);
    }
    let post = data.status.post.as_ref().filter(|p| p.job_id == job.id);
    let speed = if job.state == JobState::Downloading { data.speed() } else { 0 };
    section_title(ui, pal, "Lifecycle");
    ui.add_space(6.0);
    let steps = timeline(job, post, done, speed, now);
    lifecycle(ui, pal, &steps, reduced);
    ui.add_space(12.0);

    // The repair budget, once there is something measured to show (or while it is being
    // measured).
    if let Some(o) = data.outlook(job).filter(|o| o.known || o.no_par2 || job.state == JobState::Downloading) {
        super::repair::section(ui, pal, egui::Id::new(("inspector-repair", job.id)), &o, pal.raised, reduced);
        ui.add_space(12.0);
    }

    // Every article of the job as recorded by its runs, then one lane per file.
    let held = job.state == JobState::Paused;
    match data.article_maps.get(&job.id).filter(|m| !m.is_empty()) {
        Some(map) => {
            let w = ui.available_width();
            let size = Vec2::new(w, super::articles::fit_height(map.len(), w, 132.0));
            let per = super::articles::per_cell(map, size);
            ui.horizontal(|ui| {
                section_title(ui, pal, "Article map");
                if per > 1 {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(format!("1 cell = {per} articles")).font(fonts::mono_id(10.5)).color(pal.faint));
                    });
                }
            });
            ui.add_space(6.0);
            super::articles::grid(ui, pal, egui::Id::new(("inspector-articles", job.id)), map, size, held, pal.raised, reduced);
            ui.add_space(6.0);
            ui.label(RichText::new(super::articles::summary(&map.counts())).font(fonts::mono_id(11.0)).color(pal.muted));
            super::articles::legend(ui, pal, pal.raised);
            ui.add_space(12.0);
            section_title(ui, pal, "Files");
            ui.add_space(6.0);
            super::articles::lanes(ui, pal, map, held, pal.raised);
            ui.add_space(12.0);
        }
        None => {
            section_title(ui, pal, "Files");
            ui.add_space(6.0);
            files(ui, pal, data);
            ui.add_space(12.0);
        }
    }

    section_title(ui, pal, "Numbers");
    ui.add_space(6.0);
    let facts = facts(job, done);
    // Value over label, two per row (like the dock tiles).
    let col_w = (ui.available_width() - 12.0) / 2.0;
    for pair in facts.chunks(2) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            for (k, v) in pair {
                ui.allocate_ui(Vec2::new(col_w, 34.0), |ui| {
                    ui.set_width(col_w);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.add(egui::Label::new(RichText::new(v).font(fonts::mono_id(13.0)).color(pal.text)).truncate());
                        ui.label(RichText::new(k.to_uppercase()).font(fonts::mono_id(9.5)).color(pal.faint));
                    });
                });
            }
        });
        ui.add_space(4.0);
    }
    if let Some(s) = job.stats.outcome.as_ref().and_then(|o| o.par2_summary.as_ref()) {
        ui.add_space(6.0);
        ui.add(egui::Label::new(RichText::new(format!("PAR2: {s}")).small().color(pal.muted)).wrap());
    }
    ui.add_space(12.0);

    section_title(ui, pal, "Where");
    ui.add_space(6.0);
    place(ui, pal, "Folder", &job.dest_dir, Some(Action::OpenJobFolder(job.id)), actions);
    if let Some(x) = job.stats.outcome.as_ref().and_then(|o| o.extracted_to.as_ref()) {
        place(ui, pal, "Extracted", x, Some(Action::OpenPath(x.clone())), actions);
    }
    place(ui, pal, "NZB", &job.source_path, Some(Action::RevealPath(job.source_path.clone())), actions);
    if job.password.is_some() {
        ui.label(RichText::new("An archive password is set for this job.").small().color(pal.muted));
    }
}

fn place(ui: &mut Ui, pal: &Palette, key: &str, path: &std::path::Path, open: Option<Action>, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(64.0, 16.0), Sense::hover());
        ui.painter().text(r.left_center(), egui::Align2::LEFT_CENTER, key, egui::FontId::proportional(11.5), pal.muted);
        let full = path.display().to_string();
        // A source file is named and shown in its folder on click (the full path is on
        // hover); folders show in full and open on click.
        let reveal = matches!(open, Some(Action::RevealPath(_)));
        let text = if reveal {
            path.file_name().map_or(full.clone(), |n| n.to_string_lossy().into_owned())
        } else {
            full.clone()
        };
        let color = if open.is_some() { pal.accent_fg } else { pal.text };
        let resp = ui.add(egui::Label::new(RichText::new(&text).small().color(color)).truncate().sense(Sense::click()));
        let tip = match &open {
            Some(_) if reveal => format!("{full}\nClick to show it in the file manager"),
            Some(_) => format!("{full}\nClick to open"),
            None => full.clone(),
        };
        let resp = resp.on_hover_text(tip);
        if let Some(a) = open {
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                actions.push(a);
            }
        }
    });
}

/// The problem card of a failed job. A password or extraction failure gets a password
/// field in the card itself; Retry (or Enter in the field) saves what was typed first.
fn problem(ui: &mut Ui, pal: &Palette, data: &Data, job: &Job, state: &mut InspectorState, actions: &mut Vec<Action>) {
    let kind = job.stats.outcome.as_ref().and_then(|o| o.failure);
    let reason = job.error.clone().unwrap_or_else(|| "No reason was recorded.".into());
    let frame = egui::Frame::new()
        .fill(Palette::fade(pal.negative_soft, 0.6))
        .stroke(Stroke::new(1.0, Palette::fade(pal.negative, 0.5)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::same(12));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
            paint_icon(ui.painter(), r, Icon::Alert, pal.negative);
            let title = kind.map_or("This download failed".to_string(), |k| capitalize(k.label()));
            ui.label(RichText::new(title).font(fonts::semibold_id(13.0)).color(pal.text));
        });
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(&reason).font(fonts::mono_id(11.5)).color(pal.negative)).wrap().selectable(true));
        ui.add_space(6.0);
        ui.add(egui::Label::new(RichText::new(kind.unwrap_or(FailureKind::Other).hint()).small().color(pal.text)).wrap());
        ui.add_space(8.0);
        let asks_password = matches!(kind, Some(FailureKind::Password | FailureKind::Extract));
        let entered = if asks_password {
            let entered = password_field(ui, pal, job, state);
            ui.add_space(8.0);
            entered
        } else {
            false
        };
        let typed = state
            .password
            .as_ref()
            .filter(|(id, text)| asks_password && *id == job.id && !text.is_empty())
            .map(|(_, text)| text.clone());
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let tip = if typed.is_some() { "Save the password and download again" } else { "Download again" };
            let retry = kit_button(ui, pal, ButtonKind::Outline, Some(Icon::Retry), "Retry", true).on_hover_text(tip);
            if retry.clicked() || (entered && typed.is_some()) {
                if let Some(pw) = typed.clone() {
                    actions.push(Action::SetJobPassword(job.id, Some(pw)));
                    state.password = None;
                }
                actions.push(Action::RetryJob(job.id));
            }
            // Missing articles and server trouble are what a fail-over server can fix.
            if matches!(
                kind,
                None | Some(
                    FailureKind::MissingArticles
                        | FailureKind::Unrepairable
                        | FailureKind::Connection
                        | FailureKind::Auth
                        | FailureKind::Repair
                )
            ) {
                let servers = data.settings.usable_servers();
                let ok = servers.len() >= 2;
                let r = kit_button(ui, pal, ButtonKind::Outline, Some(Icon::RetryBackup), "Retry on backup", ok);
                if r.on_hover_text(super::serverstrip::backup_retry_explainer(&servers)).clicked() && ok {
                    actions.push(Action::RetryOnBackup(job.id));
                }
            }
            if matches!(kind, Some(FailureKind::Auth | FailureKind::Connection))
                && kit_button(ui, pal, ButtonKind::Text, Some(Icon::Gear), "Servers…", true).clicked()
            {
                actions.push(Action::OpenSettings);
            }
            if kit_button(ui, pal, ButtonKind::Text, None, "Copy error", true).clicked() {
                ui.ctx().copy_text(reason.clone());
            }
        });
    });
}

/// The archive password field of the problem card, the width of the card, with a toggle
/// to show what was typed. Returns whether Enter was pressed in it.
fn password_field(ui: &mut Ui, pal: &Palette, job: &Job, state: &mut InspectorState) -> bool {
    // A draft belongs to one job: another job's card starts empty and hidden.
    if state.password.as_ref().is_none_or(|(id, _)| *id != job.id) {
        state.password = Some((job.id, String::new()));
        state.show_password = false;
    }
    let InspectorState { password: Some((_, text)), show_password, .. } = state else {
        return false;
    };
    // Right to left, so the field takes whatever the toggle leaves.
    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 24.0), Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if compact_button(ui, pal, ButtonKind::Text, if *show_password { "Hide" } else { "Show" }, true).clicked() {
            *show_password = !*show_password;
        }
        let hint = if job.password.is_some() { "Another archive password" } else { "Archive password" };
        let resp = ui.add(
            egui::TextEdit::singleline(text)
                .id_salt(("inspector-password", job.id))
                .password(!*show_password)
                .hint_text(hint)
                .desired_width(ui.available_width()),
        );
        resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
    })
    .inner
}

/// The vertical lifecycle: a node per step on a rail, title and time on one line, the
/// detail under it.
fn lifecycle(ui: &mut Ui, pal: &Palette, steps: &[Step], reduced: bool) {
    const GUTTER: f32 = 22.0;
    let t = ui.input(|i| i.time);
    let mut centers: Vec<(Pos2, Color32)> = Vec::new();
    for step in steps {
        let color = match step.mark {
            Mark::Done => pal.positive,
            Mark::Live(_) => pal.accent_fg,
            Mark::Warn => pal.warning,
            Mark::Failed => pal.negative,
            Mark::Skipped | Mark::Waiting => pal.faint,
        };
        let resp = ui.horizontal_top(|ui| {
            let (gutter, _) = ui.allocate_exact_size(Vec2::new(GUTTER, 18.0), Sense::hover());
            let c = Pos2::new(gutter.center().x, gutter.top() + 9.0);
            node(ui, pal, c, step.mark, color, if reduced { None } else { Some(t) });
            centers.push((c, color));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.horizontal(|ui| {
                    let title_color = match step.mark {
                        Mark::Skipped | Mark::Waiting => pal.muted,
                        _ => pal.text,
                    };
                    ui.label(RichText::new(step.title).font(fonts::semibold_id(12.5)).color(title_color));
                    if let Some(time) = &step.time {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(RichText::new(time).font(fonts::mono_id(11.0)).color(pal.muted));
                        });
                    }
                });
                if !step.detail.is_empty() {
                    let dc = match step.mark {
                        Mark::Failed => pal.negative,
                        Mark::Warn => pal.warning,
                        Mark::Skipped | Mark::Waiting => pal.faint,
                        _ => pal.muted,
                    };
                    ui.add(egui::Label::new(RichText::new(&step.detail).small().color(dc)).wrap());
                }
                if let Mark::Live(Some(f)) = step.mark {
                    ui.add_space(2.0);
                    segment_track(ui, pal, f, pal.accent, ui.available_width().min(260.0));
                }
            });
        });
        let _ = resp;
        ui.add_space(6.0);
    }
    // The rail between nodes, drawn after so it sits behind nothing it should not.
    for w in centers.windows(2) {
        let (a, ca) = w[0];
        let (b, _) = w[1];
        let color = if ca == pal.faint { pal.border } else { Palette::fade(ca, 0.5) };
        ui.painter().line_segment([Pos2::new(a.x, a.y + 8.0), Pos2::new(b.x, b.y - 8.0)], Stroke::new(1.5, color));
    }
}

fn node(ui: &Ui, pal: &Palette, c: Pos2, mark: Mark, color: Color32, time: Option<f64>) {
    let p = ui.painter();
    match mark {
        Mark::Done => {
            p.circle_filled(c, 6.0, color);
            paint_icon(p, Rect::from_center_size(c, Vec2::splat(10.0)), Icon::StatusOk, pal.surface);
        }
        Mark::Failed => {
            p.circle_filled(c, 6.0, color);
            paint_icon(p, Rect::from_center_size(c, Vec2::splat(10.0)), Icon::StatusErr, pal.surface);
        }
        Mark::Warn => {
            p.circle_filled(c, 6.0, color);
            paint_icon(p, Rect::from_center_size(c, Vec2::splat(10.0)), Icon::StatusWarn, pal.surface);
        }
        Mark::Live(_) => {
            let breath = time.map_or(1.0, |t| (0.5 + 0.5 * (t * std::f64::consts::TAU / 1.6).sin()) as f32);
            p.circle_stroke(c, 6.0 + 3.0 * (1.0 - breath), Stroke::new(1.0, Palette::fade(color, 0.5 * breath)));
            p.circle_filled(c, 4.5, color);
            if time.is_some() {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
            }
        }
        Mark::Skipped => {
            p.circle_stroke(c, 5.0, Stroke::new(1.0, color));
            p.line_segment([Pos2::new(c.x - 2.5, c.y), Pos2::new(c.x + 2.5, c.y)], Stroke::new(1.0, color));
        }
        Mark::Waiting => {
            p.circle_stroke(c, 5.0, Stroke::new(1.0, color));
        }
    }
}

fn files(ui: &mut Ui, pal: &Palette, data: &Data) {
    let files = &data.selected_files;
    if files.is_empty() {
        ui.label(RichText::new("Per-file progress appears once the download starts.").small().color(pal.muted));
        return;
    }
    let mut sorted: Vec<_> = files.iter().collect();
    // Problems first, then in progress, then done; by name within each.
    sorted.sort_by_key(|f| {
        (
            match f.state.as_str() {
                "failed" => 0,
                "downloading" => 1,
                "done" => 3,
                _ => 2,
            },
            f.name.clone(),
        )
    });
    let done = files.iter().filter(|f| f.state == "done").count();
    let failed = files.iter().filter(|f| f.state == "failed").count();
    let mut summary = format!("{done} of {} done", files.len());
    if failed > 0 {
        summary.push_str(&format!(" · {failed} incomplete"));
    }
    ui.label(RichText::new(summary).small().color(if failed > 0 { pal.warning } else { pal.muted }));
    ui.add_space(6.0);
    for f in sorted {
        let (color, status) = match f.state.as_str() {
            "done" => (pal.positive, "done"),
            "failed" => (pal.negative, "incomplete"),
            "downloading" => (pal.accent, "downloading"),
            other => (pal.muted, other),
        };
        // One file: its name with the figures on the same line, its track right under.
        ui.scope(|ui| {
            ui.spacing_mut().interact_size.y = 16.0;
            ui.spacing_mut().item_spacing.y = 5.0;
            let figures = format!("{} / {}", format::bytes(f.bytes_done), format::bytes(f.bytes_total));
            ui.horizontal(|ui| {
                let fig = ui.painter().layout_no_wrap(figures.clone(), fonts::mono_id(10.5), pal.muted);
                let name_w = (ui.available_width() - fig.size().x - 12.0).max(40.0);
                ui.allocate_ui_with_layout(Vec2::new(name_w, 16.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(&f.name).font(fonts::mono_id(11.0)).color(pal.text))
                            .truncate(),
                    )
                    .on_hover_text(format!("{} · {status}", f.name));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(figures).font(fonts::mono_id(10.5)).color(pal.muted));
                });
            });
            segment_track(ui, pal, format::fraction(f.bytes_done, f.bytes_total), color, ui.available_width());
        });
        ui.add_space(12.0);
    }
}

fn log(ui: &mut Ui, pal: &Palette, data: &Data, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let text: String = data
            .selected_log
            .iter()
            .map(|l| format!("{} {:5} {}\n", format::timestamp_utc(l.ts), l.level, l.text))
            .collect();
        if kit_button(ui, pal, ButtonKind::Text, None, "Copy log", !text.is_empty()).clicked() {
            ui.ctx().copy_text(text);
        }
        if kit_button(ui, pal, ButtonKind::Text, Some(Icon::External), "Open log file", true)
            .on_hover_text("The full application log, in the data folder")
            .clicked()
        {
            actions.push(Action::OpenLogFile);
        }
    });
    ui.add_space(6.0);
    if data.selected_log.is_empty() {
        ui.label(RichText::new("Nothing logged for this job yet.").small().color(pal.muted));
        return;
    }
    for l in data.selected_log.iter().rev() {
        let color = match l.level.as_str() {
            "error" => pal.negative,
            "warn" => pal.warning,
            _ => pal.text,
        };
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(RichText::new(clock(l.ts)).font(fonts::mono_id(10.5)).color(pal.faint));
            ui.add(egui::Label::new(RichText::new(&l.text).small().color(color)).wrap().selectable(true));
        });
    }
}

/// Local `HH:MM:SS` (as the message dock shows times).
fn clock(ts: i64) -> String {
    super::docks::clock(ts)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use newskite_service::{JobOutcome, JobStats};
    use std::path::PathBuf;

    fn job(state: JobState, stats: JobStats) -> Job {
        Job {
            id: 1,
            name: "x".into(),
            state,
            bytes_total: 1000,
            bytes_done: if state == JobState::Done { 1000 } else { 400 },
            dest_dir: PathBuf::from("/d/x"),
            source_path: PathBuf::from("/n/x.nzb"),
            created_at: 0,
            error: None,
            password: None,
            stats,
        }
    }

    fn marks(v: &[Step]) -> Vec<Mark> {
        v.iter().map(|s| s.mark).collect()
    }

    #[test]
    fn queued_job_waits() {
        let v = timeline(&job(JobState::Queued, JobStats::default()), None, 0, 0, 100);
        assert_eq!(v.len(), 7);
        assert_eq!(v[0].mark, Mark::Done);
        assert!(v[1..].iter().all(|s| s.mark == Mark::Waiting));
        assert_eq!(v[1].detail, "Waiting in the queue");
    }

    #[test]
    fn downloading_shows_fraction_speed_and_eta() {
        let v = timeline(&job(JobState::Downloading, JobStats::default()), None, 400, 100, 100);
        assert_eq!(v[1].mark, Mark::Live(Some(0.4)));
        assert!(v[1].detail.contains("6s left"), "{}", v[1].detail);
    }

    #[test]
    fn post_stage_marks_earlier_steps() {
        let p = PostProgress { job_id: 1, stage: PostStage::Extract, permille: Some(500), detail: "Extracting".into() };
        let v = timeline(&job(JobState::Downloading, JobStats::default()), Some(&p), 1000, 0, 0);
        assert_eq!(
            marks(&v),
            vec![Mark::Done, Mark::Done, Mark::Done, Mark::Skipped, Mark::Live(Some(0.5)), Mark::Waiting, Mark::Waiting]
        );
    }

    #[test]
    fn repaired_job_reads_from_the_outcome() {
        let rec = |state, detail: &str| Some(StepRecord { state, detail: detail.into(), ms: 1500 });
        let stats = JobStats {
            started_at: Some(10),
            finished_at: Some(50),
            download_ms: 30_000,
            articles_total: 40,
            articles_missing: 2,
            outcome: Some(Box::new(JobOutcome {
                verify: rec(StepState::Ok, "1 file: 1 damaged"),
                repair: rec(StepState::Repaired, "rebuilt 2 slices"),
                extract: rec(StepState::Skipped, "skipped (no archives found)"),
                cleanup: rec(StepState::Ok, "removed 3 files"),
                slices_rebuilt: 2,
                articles_repaired: 2,
                ..Default::default()
            })),
            ..Default::default()
        };
        let v = timeline(&job(JobState::Done, stats), None, 1000, 0, 110);
        assert_eq!(
            marks(&v),
            vec![Mark::Done, Mark::Warn, Mark::Done, Mark::Done, Mark::Skipped, Mark::Done, Mark::Done]
        );
        assert!(v[1].detail.contains("2 of 40 articles missing"));
        assert_eq!(v[3].time.as_deref(), Some("1s"));
        assert_eq!(v[6].detail, "Complete · PAR2 repaired the damage");
        assert_eq!(v[6].time.as_deref(), Some("1 min ago"));
    }

    #[test]
    fn login_failure_stops_at_download() {
        let stats = JobStats {
            finished_at: Some(5),
            outcome: Some(Box::new(JobOutcome { failure: Some(FailureKind::Auth), ..Default::default() })),
            ..Default::default()
        };
        let v = timeline(&job(JobState::Failed, stats), None, 0, 0, 10);
        assert_eq!(v[1].mark, Mark::Failed);
        assert!(v[2..6].iter().all(|s| s.mark == Mark::Skipped && s.detail == "Not reached"));
        assert_eq!(v[6].detail, "Login refused");
    }

    #[test]
    fn facts_only_show_measured_numbers() {
        let f = facts(&job(JobState::Queued, JobStats::default()), 0);
        assert_eq!(f.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec!["Size", "Downloaded"]);
        let stats = JobStats { download_ms: 2000, articles_total: 9, ..Default::default() };
        let f = facts(&job(JobState::Done, stats), 1000);
        assert!(f.iter().any(|(k, v)| *k == "Average speed" && v == &format::speed(500)));
    }
}
