//! Center area: tab strip, the Queue tab (outline §6.2b) and the placeholder tabs.

use std::cmp::Ordering;

use eframe::egui::{self, Align, Layout, RichText, Sense, TextEdit, Ui};
use egui_extras::{Column, TableBuilder};
use newskite_service::{Job, JobState};
use newskite_types::{format, Settings};

use super::app::{Action, Data, Tab, ViewState};
use super::theme::Palette;
use super::widgets::{icon_button, icon_text_button, pill, progress_bar, state_badge, Icon};

/// Exact empty-state text of the Queue table.
pub const EMPTY_QUEUE: &str = "No jobs. Import an NZB to begin.";

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
    ui.horizontal(|ui| {
        for (tab, label) in [
            (Tab::Search, "Search"),
            (Tab::Latest, "Latest File Groups"),
            (Tab::Queue, "Queue"),
        ] {
            tab_button(ui, pal, label, view.tab == tab, || {
                actions.push(Action::SetTab(tab))
            });
        }
        if view.tab == Tab::Queue {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (icon, tip) = if view.queue.descending {
                    (Icon::ChevronDown, "Descending (click for ascending)")
                } else {
                    (Icon::ChevronUp, "Ascending (click for descending)")
                };
                if icon_button(ui, pal, icon, pal.text, tip).clicked() {
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

fn tab_button(ui: &mut Ui, pal: &Palette, label: &str, selected: bool, mut on_click: impl FnMut()) {
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
    if selected {
        let y = rect.bottom() - 1.5;
        ui.painter().line_segment(
            [
                egui::pos2(rect.left() + 6.0, y),
                egui::pos2(rect.right() - 6.0, y),
            ],
            egui::Stroke::new(3.0, pal.accent),
        );
    }
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, label)
    });
    if resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        on_click();
    }
}

fn placeholder(ui: &mut Ui, pal: &Palette, title: &str, text: &str, actions: &mut Vec<Action>) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        ui.set_max_width(520.0);
        ui.label(RichText::new(title).heading().color(pal.text));
        ui.add_space(6.0);
        ui.label(RichText::new(text).color(pal.muted));
        ui.add_space(12.0);
        if ui.button("Open NZB…").clicked() {
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
        if icon_text_button(ui, pal, Icon::Play, "Start", !st.running && has_runnable)
            .on_hover_text("Start downloading queued jobs, oldest first")
            .clicked()
        {
            actions.push(Action::Start);
        }
        if icon_text_button(ui, pal, Icon::Pause, "Pause", st.running && !st.paused)
            .on_hover_text("Stop after the current job finishes")
            .clicked()
        {
            actions.push(Action::Pause);
        }
        if icon_text_button(ui, pal, Icon::Play, "Resume", st.paused)
            .on_hover_text("Clear the pause and continue")
            .clicked()
        {
            actions.push(Action::Resume);
        }
        let watch_set = !data.settings.watch_folder.as_os_str().is_empty();
        let scan = icon_text_button(ui, pal, Icon::Folder, "Scan watch folder", watch_set);
        let scan = if watch_set {
            scan.on_hover_text(format!(
                "Import new .nzb files from {}",
                data.settings.watch_folder.display()
            ))
        } else {
            scan.on_hover_text("Set a watch folder in Settings, Downloads first")
        };
        if scan.clicked() {
            actions.push(Action::ScanWatch);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let (text, ok) = connection_text(&data.settings, st.running);
            let (fg, bg) = if ok {
                (pal.positive, pal.positive_soft)
            } else {
                (pal.warning, pal.warning_soft)
            };
            pill(ui, &text, fg, bg);
        });
    });
    ui.add_space(8.0);

    if data.jobs.is_empty() {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(EMPTY_QUEUE).size(14.0).color(pal.muted));
            ui.add_space(8.0);
            if ui.button("Open NZB…").clicked() {
                actions.push(Action::ShowOpenNzb);
            }
        });
        return;
    }

    let jobs = sorted(&data.jobs, view.queue.sort, view.queue.descending);
    let progress_w = 230.0;
    TableBuilder::new(ui)
        .id_salt("queue-table")
        .striped(true)
        .resizable(false)
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::exact(44.0))
        .column(Column::remainder().at_least(160.0).clip(true))
        .column(Column::exact(100.0))
        .column(Column::exact(84.0))
        .column(Column::exact(progress_w))
        .column(Column::exact(150.0))
        .header(24.0, |mut h| {
            for t in ["#", "Name", "State", "Size", "Progress", ""] {
                h.col(|ui| {
                    ui.label(RichText::new(t).small().strong().color(pal.muted));
                });
            }
        })
        .body(|mut body| {
            for job in jobs {
                body.row(40.0, |mut row| {
                    row.col(|ui| {
                        ui.label(
                            RichText::new(job.id.to_string())
                                .monospace()
                                .color(pal.muted),
                        );
                    });
                    row.col(|ui| name_cell(ui, pal, job, view.queue.highlight == Some(job.id)));
                    row.col(|ui| {
                        let r = state_badge(ui, pal, job.state);
                        if let Some(e) = &job.error {
                            r.on_hover_text(e);
                        }
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(format::bytes(job.bytes_total)).color(pal.text));
                    });
                    row.col(|ui| progress_cell(ui, pal, data, job, progress_w - 8.0));
                    row.col(|ui| actions_cell(ui, pal, job, &mut view.queue, actions));
                });
            }
        });
}

fn name_cell(ui: &mut Ui, pal: &Palette, job: &Job, highlight: bool) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        let color = if highlight { pal.accent_fg } else { pal.text };
        ui.add(egui::Label::new(RichText::new(&job.name).color(color)).truncate())
            .on_hover_text(job.source_path.display().to_string());
        let (sub, color) = match (&job.error, job.state) {
            (Some(e), JobState::Failed) => (e.clone(), pal.negative),
            _ => (job.dest_dir.display().to_string(), pal.muted),
        };
        ui.add(egui::Label::new(RichText::new(sub).small().color(color)).truncate());
    });
}

fn progress_cell(ui: &mut Ui, pal: &Palette, data: &Data, job: &Job, width: f32) {
    let done = data.shown_done(job);
    let color = match job.state {
        JobState::Done => pal.positive,
        JobState::Failed => pal.negative,
        JobState::Paused => pal.warning,
        _ => pal.accent,
    };
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 3.0;
        progress_bar(
            ui,
            pal,
            format::fraction(done, job.bytes_total),
            color,
            width,
        );
        ui.label(
            RichText::new(format!(
                "{}%  ·  {} / {}",
                format::percent(done, job.bytes_total),
                format::bytes(done),
                format::bytes(job.bytes_total)
            ))
            .small()
            .color(pal.muted),
        );
    });
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
            pal.text,
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
            pal.accent_fg
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
    if icon_button(ui, pal, Icon::Close, pal.negative, tip).clicked() {
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
