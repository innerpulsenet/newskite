//! Settings sheet (outline §6.5, F8): a modal editing a cloned [`Settings`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui::{self, Align, DragValue, Id, Layout, RichText, TextEdit, Ui};
use newskite_types::{ServerConfig, Settings};

use super::app::Action;
use super::appearance::Appearance;
use super::fonts;
use super::theme::Palette;
use super::widgets::{
    card, compact_button, dialog_frame, error_line, icon_button, kit_button, notice,
    section_title, toggle, ButtonKind, Icon, Status,
};
use crate::launch::expand_tilde;

/// Server row ids are unique for the whole process, not per sheet: a server "Test" can
/// outlive the sheet that started it (it runs up to the test timeout), and its late
/// result must not land on a row of a sheet opened afterwards.
static NEXT_ROW_ID: AtomicU64 = AtomicU64::new(0);

fn new_row_id() -> u64 {
    NEXT_ROW_ID.fetch_add(1, Ordering::Relaxed)
}

/// Which settings field a folder picker fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderTarget {
    /// `download_dir`.
    Download,
    /// `watch_folder`.
    Watch,
}

impl FolderTarget {
    /// Dialog title.
    pub fn title(self) -> &'static str {
        match self {
            FolderTarget::Download => "Choose download folder",
            FolderTarget::Watch => "Choose watch folder",
        }
    }
}

/// State of a server "Test".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestState {
    /// Waiting for the reply.
    Running,
    /// Raw transcript (`Ok`) or the error text (`Err`).
    Done(Result<String, String>),
}

/// The settings being edited plus per-row UI state.
pub struct SettingsDraft {
    /// Edited copy; discarded on Cancel.
    pub settings: Settings,
    /// Stable ids of server rows (parallel to `settings.servers`) so test results land
    /// on the right row after rows are removed. Unique across sheets (see
    /// [`NEXT_ROW_ID`]).
    pub row_ids: Vec<u64>,
    /// Test state per row id.
    pub tests: HashMap<u64, TestState>,
    /// Download folder as text.
    pub download_dir: String,
    /// Watch folder as text.
    pub watch_folder: String,
    /// Error from the last save attempt.
    pub save_error: Option<String>,
    /// Settings › Appearance (applied on Save).
    pub appearance: Appearance,
    /// Why the stored settings could not be read, shown as a notice at the top.
    pub load_error: Option<String>,
}

impl SettingsDraft {
    /// Starts editing a copy of `settings` (normalized: at least one server row).
    pub fn new(settings: Settings) -> Self {
        let settings = settings.normalized();
        let row_ids = settings.servers.iter().map(|_| new_row_id()).collect();
        Self {
            download_dir: settings.download_dir.display().to_string(),
            watch_folder: settings.watch_folder.display().to_string(),
            settings,
            row_ids,
            tests: HashMap::new(),
            save_error: None,
            appearance: Appearance::default(),
            load_error: None,
        }
    }

    /// Current text of a folder field.
    pub fn folder(&self, target: FolderTarget) -> PathBuf {
        PathBuf::from(match target {
            FolderTarget::Download => &self.download_dir,
            FolderTarget::Watch => &self.watch_folder,
        })
    }

    /// Fills a folder field (from a picker).
    pub fn set_folder(&mut self, target: FolderTarget, path: PathBuf) {
        let text = path.display().to_string();
        match target {
            FolderTarget::Download => self.download_dir = text,
            FolderTarget::Watch => self.watch_folder = text,
        }
    }

    /// Records a test result for a row. Ignored unless this sheet started a test on that
    /// row that is still waiting (the row was removed, or the result belongs to a test
    /// from a sheet that has since closed).
    pub fn set_test_result(&mut self, row_id: u64, result: Result<String, String>) {
        if self.row_ids.contains(&row_id) && self.tests.get(&row_id) == Some(&TestState::Running) {
            self.tests.insert(row_id, TestState::Done(result));
        }
    }

    /// Appends an empty server row.
    pub fn add_server(&mut self) {
        self.settings.servers.push(ServerConfig::default());
        self.row_ids.push(new_row_id());
    }

    /// Removes a row; the last row is replaced by an empty disabled one so at least one
    /// row always exists.
    pub fn remove_server(&mut self, index: usize) {
        if index >= self.settings.servers.len() {
            return;
        }
        self.settings.servers.remove(index);
        let id = self.row_ids.remove(index);
        self.tests.remove(&id);
        if self.settings.servers.is_empty() {
            self.add_server();
            if let Some(s) = self.settings.servers.first_mut() {
                s.enabled = false;
            }
        }
    }

    /// The settings that Save would persist (folder texts applied with `~` expanded,
    /// trimmed, normalized).
    pub fn result(&self) -> Settings {
        self.edited().normalized()
    }

    /// The edited settings before normalization: what [`validate`] checks, so a value
    /// normalization would silently change is reported instead.
    pub fn edited(&self) -> Settings {
        let folder = |text: &str| {
            let text = text.trim();
            if text.is_empty() {
                PathBuf::new()
            } else {
                expand_tilde(text)
            }
        };
        let mut s = self.settings.clone();
        s.download_dir = folder(&self.download_dir);
        s.watch_folder = folder(&self.watch_folder);
        for srv in &mut s.servers {
            srv.host = srv.host.trim().to_owned();
            srv.username = srv.username.trim().to_owned();
        }
        s
    }
}

/// Whether `host` looks like `name:port` (one colon). Bare IPv6 literals have several
/// colons and are left alone; bracketed ones are written without the port.
fn host_has_port(host: &str) -> bool {
    host.matches(':').count() == 1 || (host.starts_with('[') && host.contains("]:"))
}

/// Validation problems that block saving (empty = valid).
pub fn validate(s: &Settings) -> Vec<String> {
    let mut errs = Vec::new();
    for (i, srv) in s.servers.iter().enumerate() {
        let n = i + 1;
        if srv.port == 0 {
            errs.push(format!("Server {n}: port must be between 1 and 65535."));
        }
        if srv.connections == 0 {
            errs.push(format!("Server {n}: connections must be at least 1."));
        }
        let host = srv.host.trim();
        if host.contains("://") || host.contains('/') || host.chars().any(char::is_whitespace) {
            errs.push(format!(
                "Server {n}: enter a bare host name (no scheme, path or spaces)."
            ));
        } else if host_has_port(host) {
            errs.push(format!(
                "Server {n}: put the port in the Port field, not after the host name."
            ));
        }
        if srv.enabled && host.is_empty() && s.servers.len() > 1 {
            errs.push(format!(
                "Server {n}: enabled servers need a host (or disable the row)."
            ));
        }
    }
    if s.max_connections == 0 {
        errs.push("Max connections must be at least 1.".to_owned());
    }
    if s.download_dir.as_os_str().is_empty() {
        errs.push("Download folder is required.".to_owned());
    } else if !s.download_dir.is_absolute() {
        errs.push("Download folder must be a full path (starting with / or ~).".to_owned());
    }
    if s.watch_enabled && s.watch_folder.as_os_str().is_empty() {
        errs.push("Choose a watch folder or turn the watch folder off.".to_owned());
    } else if !s.watch_folder.as_os_str().is_empty() && !s.watch_folder.is_absolute() {
        errs.push("Watch folder must be a full path (starting with / or ~).".to_owned());
    }
    errs
}

/// Draws the modal. Esc is handled by the app (it closes without saving).
/// Key of the sheet's fade-in (reset by the app while it is closed).
pub fn fade_id() -> Id {
    Id::new("settings-fade")
}

pub fn show(
    ctx: &egui::Context,
    pal: &Palette,
    draft: &mut SettingsDraft,
    dialog_open: bool,
    reduced_motion: bool,
    actions: &mut Vec<Action>,
) {
    let max_h = (ctx.content_rect().height() - 230.0).max(220.0);
    let width = (ctx.content_rect().width() - 80.0).clamp(600.0, 900.0);
    let t = super::motion::fade_in(ctx, fade_id(), 0.2, reduced_motion);
    let eased = super::motion::ease_out(t);
    let id = Id::new("settings-modal");
    egui::Modal::new(id)
        // The sheet rises a little as it fades in.
        .area(egui::Modal::default_area(id).anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, (1.0 - eased) * 18.0)))
        .frame(dialog_frame(pal))
        .backdrop_color(Palette::fade(pal.backdrop, t))
        .show(ctx, |ui| {
            ui.set_opacity(t);
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::Frame::new().inner_margin(egui::Margin { left: 22, right: 14, top: 16, bottom: 14 }).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Settings").font(fonts::semibold_id(18.0)).color(pal.text));
                        ui.add_space(2.0);
                        ui.label(RichText::new("Servers, folders, speed and what happens after a download").small().color(pal.muted));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        if icon_button(ui, pal, Icon::Close, pal.muted, "Close without saving (Esc)").clicked() {
                            actions.push(Action::CloseSettings);
                        }
                    });
                });
            });
            hairline(ui, pal);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                section_nav(ui, pal, max_h);
                let r = ui.available_rect_before_wrap();
                ui.painter().vline(r.left(), r.top()..=(r.top() + max_h), egui::Stroke::new(1.0, pal.border));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
                    sheet_body(ui, pal, draft, dialog_open, max_h, actions);
                });
            });
            hairline(ui, pal);
            egui::Frame::new().inner_margin(egui::Margin { left: 22, right: 16, top: 12, bottom: 14 }).show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
                footer(ui, pal, draft, actions);
            });
        });
}

/// A full-width hairline between the sheet's header, body and footer.
fn hairline(ui: &mut Ui, pal: &Palette) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(r.x_range(), r.center().y, egui::Stroke::new(1.0, pal.border));
}

/// The sheet's sections, in order (titles double as the navigation entries).
const SECTIONS: [(&str, Icon); 6] = [
    ("Connection", Icon::Transfers),
    ("Downloads", Icon::Folder),
    ("Performance", Icon::Gear),
    ("Post-processing", Icon::Retry),
    ("Automation", Icon::Play),
    ("Appearance", Icon::Info),
];

/// Temp-data keys: the section the reader is in (scroll spy), a pending jump, and where
/// each section's title landed this frame.
fn nav_active_id() -> Id {
    Id::new("settings-nav-active")
}
fn nav_jump_id() -> Id {
    Id::new("settings-nav-jump")
}
fn section_top_id(title: &str) -> Id {
    Id::new(("settings-section-top", title))
}

/// The navigation rail: one entry per section; the one being read is marked, and a
/// click scrolls its section to the top.
fn section_nav(ui: &mut Ui, pal: &Palette, height: f32) {
    let active = ui.ctx().data(|d| d.get_temp::<usize>(nav_active_id())).unwrap_or(0);
    let (rail, _) = ui.allocate_exact_size(egui::vec2(184.0, height), egui::Sense::hover());
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rail.shrink2(egui::vec2(10.0, 12.0))).layout(Layout::top_down(Align::Min)));
    let ui = &mut child;
    ui.spacing_mut().item_spacing.y = 2.0;
    // The marker sits under the entries; its shape is filled in once they are placed.
    let marker = ui.painter().add(egui::Shape::Noop);
    let mut slots = Vec::with_capacity(SECTIONS.len());
    for (i, (title, icon)) in SECTIONS.iter().enumerate() {
        let (r, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), egui::Sense::click());
        slots.push(r);
        let hover = ui.ctx().animate_bool(resp.id.with("hover"), resp.hovered() && i != active);
        if hover > 0.0 {
            ui.painter().rect_filled(r, egui::CornerRadius::same(7), Palette::fade(pal.hover, hover));
        }
        let on = i == active;
        let fg = if on { pal.text } else { pal.muted.lerp_to_gamma(pal.text, hover) };
        super::widgets::paint_icon(
            ui.painter(),
            egui::Rect::from_center_size(egui::pos2(r.left() + 20.0, r.center().y), egui::vec2(16.0, 16.0)),
            *icon,
            if on { pal.accent_fg } else { fg },
        );
        let font = if on { fonts::semibold_id(13.0) } else { egui::FontId::proportional(13.0) };
        ui.painter().text(egui::pos2(r.left() + 38.0, r.center().y), egui::Align2::LEFT_CENTER, *title, font, fg);
        if resp.has_focus() {
            super::widgets::focus_ring(ui, pal, r, 7);
        }
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, on, format!("Go to {title}")));
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            ui.ctx().data_mut(|d| {
                d.insert_temp(nav_jump_id(), i);
                d.insert_temp(nav_active_id(), i);
            });
        }
    }
    // The marker glides to the active entry.
    if let Some(slot) = slots.get(active) {
        let secs = ui.style().animation_time;
        let top = ui.ctx().animate_value_with_time(Id::new("settings-nav-marker"), slot.top(), secs);
        let pill = egui::Rect::from_min_size(egui::pos2(slot.left(), top), slot.size());
        let rail = egui::Rect::from_center_size(egui::pos2(pill.left() + 2.0, pill.center().y), egui::vec2(3.0, 16.0));
        ui.painter().set(
            marker,
            egui::Shape::Vec(vec![
                egui::Shape::rect_filled(pill, egui::CornerRadius::same(7), pal.accent_soft),
                egui::Shape::rect_filled(rail, egui::CornerRadius::same(2), pal.accent),
            ]),
        );
    }
}

/// The scrolling sections; afterwards the scroll spy picks the section being read.
fn sheet_body(
    ui: &mut Ui,
    pal: &Palette,
    draft: &mut SettingsDraft,
    dialog_open: bool,
    max_h: f32,
    actions: &mut Vec<Action>,
) {
    if let Some(e) = &draft.load_error {
        egui::Frame::new().inner_margin(egui::Margin { left: 20, right: 18, top: 14, bottom: 0 }).show(ui, |ui| {
            notice(
                ui,
                pal,
                Status::Warn,
                "The stored settings could not be read.",
                &format!(
                    "Defaults are shown below; saving replaces the stored settings and keeps \
                     the unreadable document in the database as a backup. ({e})"
                ),
            );
        });
    }
    let out = egui::ScrollArea::vertical()
        .id_salt("settings-body")
        .max_height(max_h)
        .min_scrolled_height(max_h)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Frame::new().inner_margin(egui::Margin { left: 20, right: 18, top: 14, bottom: 18 }).show(ui, |ui| {
                connection_section(ui, pal, draft, actions);
                ui.add_space(14.0);
                downloads_section(ui, pal, draft, dialog_open, actions);
                ui.add_space(14.0);
                performance_section(ui, pal, &mut draft.settings);
                ui.add_space(14.0);
                postprocess_section(ui, pal, &mut draft.settings);
                ui.add_space(14.0);
                automation_section(ui, pal, &mut draft.settings);
                ui.add_space(14.0);
                appearance_section(ui, pal, &mut draft.appearance);
            });
        });
    // Scroll spy: the last section whose title has passed the top of the view (the
    // last one once the end is reached).
    let view = out.inner_rect;
    let at_end = out.state.offset.y + view.height() >= out.content_size.y - 2.0 && out.state.offset.y > 0.0;
    let ctx = ui.ctx().clone();
    let jumping = ctx.data(|d| d.get_temp::<usize>(nav_jump_id())).is_some();
    if !jumping {
        let active = if at_end {
            SECTIONS.len() - 1
        } else {
            SECTIONS
                .iter()
                .enumerate()
                .filter(|(_, (title, _))| {
                    ctx.data(|d| d.get_temp::<f32>(section_top_id(title))).is_some_and(|y| y <= view.top() + 40.0)
                })
                .map(|(i, _)| i)
                .next_back()
                .unwrap_or(0)
        };
        ctx.data_mut(|d| d.insert_temp(nav_active_id(), active));
    }
}

/// Validation messages, the storage note and Cancel / Save.
fn footer(ui: &mut Ui, pal: &Palette, draft: &mut SettingsDraft, actions: &mut Vec<Action>) {
    let errors = validate(&draft.edited());
    let result = draft.result();
    ui.add_space(8.0);
    for e in &errors {
        error_line(ui, pal, e);
    }
    if let Some(e) = &draft.save_error {
        error_line(ui, pal, &format!("Saving failed: {e}"));
    }
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(
                "Passwords are stored unencrypted in the local settings database.",
            )
            .small()
            .color(pal.muted),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // The sheet's one filled call to action.
            let save = kit_button(
                ui,
                pal,
                ButtonKind::Primary,
                None,
                "Save",
                errors.is_empty(),
            );
            if save.clicked() && errors.is_empty() {
                // The app applies the sheet's appearance only once the save
                // succeeds, so a failed save changes nothing.
                actions.push(Action::SaveSettings(Box::new(result)));
            }
            if kit_button(ui, pal, ButtonKind::Outline, None, "Cancel", true).clicked() {
                actions.push(Action::CloseSettings);
            }
        });
    });
}

fn section(ui: &mut Ui, pal: &Palette, title: &str, body: impl FnOnce(&mut Ui)) {
    // A pending jump from the navigation rail lands here.
    let index = SECTIONS.iter().position(|(t, _)| *t == title);
    let jump = ui.ctx().data(|d| d.get_temp::<usize>(nav_jump_id()));
    if index.is_some() && jump == index {
        ui.scroll_to_cursor(Some(Align::TOP));
        ui.ctx().data_mut(|d| d.remove_temp::<usize>(nav_jump_id()));
    }
    let top = ui.cursor().top();
    ui.ctx().data_mut(|d| d.insert_temp(section_top_id(title), top));
    section_title(ui, pal, title);
    ui.add_space(4.0);
    card(pal).show(ui, |ui| {
        ui.set_width(ui.available_width());
        body(ui);
    });
}

fn connection_section(
    ui: &mut Ui,
    pal: &Palette,
    draft: &mut SettingsDraft,
    actions: &mut Vec<Action>,
) {
    section(ui, pal, "Connection", |ui| {
        let mut remove = None;
        let count = draft.settings.servers.len();
        for i in 0..count {
            let row_id = draft.row_ids[i];
            if i > 0 {
                ui.separator();
            }
            let srv = &mut draft.settings.servers[i];
            ui.horizontal(|ui| {
                let title = if srv.host.trim().is_empty() {
                    format!("Server {}", i + 1)
                } else {
                    format!("Server {} · {}", i + 1, srv.address())
                };
                ui.label(RichText::new(title).strong().color(pal.text));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if compact_button(ui, pal, ButtonKind::Outline, "Remove", true)
                        .on_hover_text("Remove this server")
                        .clicked()
                    {
                        remove = Some(i);
                    }
                    let running = draft.tests.get(&row_id) == Some(&TestState::Running);
                    let can_test = !srv.host.trim().is_empty() && !running;
                    if compact_button(ui, pal, ButtonKind::Outline, if running { "Testing…" } else { "Test" }, can_test)
                        .on_hover_text("Connect, authenticate and send DATE; shows the raw replies")
                        .clicked()
                    {
                        draft.tests.insert(row_id, TestState::Running);
                        let mut cfg = srv.clone();
                        cfg.host = cfg.host.trim().to_owned();
                        actions.push(Action::TestServer(row_id, Box::new(cfg)));
                    }
                    toggle(ui, pal, &mut srv.enabled, "Enabled");
                });
            });
            egui::Grid::new(("server-grid", row_id))
                .num_columns(4)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Host");
                    ui.add_sized(
                        [220.0, 22.0],
                        TextEdit::singleline(&mut srv.host).hint_text("news.example.net"),
                    );
                    ui.label("Port");
                    ui.horizontal(|ui| {
                        ui.add(DragValue::new(&mut srv.port).range(1..=65535));
                        if toggle(ui, pal, &mut srv.tls, "TLS").changed() {
                            // Follow the usual port when it is still the default of the other mode.
                            if srv.tls && srv.port == 119 {
                                srv.port = 563;
                            } else if !srv.tls && srv.port == 563 {
                                srv.port = 119;
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Username");
                    ui.add_sized([220.0, 22.0], TextEdit::singleline(&mut srv.username));
                    ui.label("Password");
                    ui.add_sized(
                        [160.0, 22.0],
                        TextEdit::singleline(&mut srv.password).password(true),
                    );
                    ui.end_row();
                    ui.label("Connections");
                    ui.add(DragValue::new(&mut srv.connections).range(1..=100));
                    ui.label("Priority");
                    ui.add(DragValue::new(&mut srv.priority).range(-100..=100))
                        .on_hover_text(
                        "Lower is tried first for fail-over; the first enabled row always leads",
                    );
                    ui.end_row();
                });
            match draft.tests.get(&row_id) {
                Some(TestState::Running) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Testing…").color(pal.muted));
                    });
                }
                Some(TestState::Done(r)) => {
                    egui::Frame::new()
                        .fill(pal.sunken)
                        .stroke(egui::Stroke::new(
                            1.0,
                            if r.is_ok() {
                                pal.positive
                            } else {
                                pal.negative
                            },
                        ))
                        .corner_radius(egui::CornerRadius::same(4))
                        .inner_margin(egui::Margin::same(6))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            match r {
                                Ok(t) => {
                                    ui.label(RichText::new(t).monospace().color(pal.text));
                                }
                                Err(e) => {
                                    error_line(ui, pal, e);
                                }
                            }
                        });
                }
                None => {}
            }
        }
        if let Some(i) = remove {
            draft.remove_server(i);
        }
        ui.add_space(4.0);
        if kit_button(ui, pal, ButtonKind::Outline, Some(Icon::Plus), "Add server", true).clicked() {
            draft.add_server();
        }
        ui.label(
            RichText::new(
                "Downloads use the first enabled server with a host; the others are fail-over.",
            )
            .small()
            .color(pal.muted),
        );
    });
}

fn folder_row(
    ui: &mut Ui,
    pal: &Palette,
    text: &mut String,
    target: FolderTarget,
    dialog_open: bool,
    actions: &mut Vec<Action>,
) {
    let (label, hint) = match target {
        FolderTarget::Download => ("Download folder", "~/Downloads/Newskite"),
        FolderTarget::Watch => ("Watch folder", "(none)"),
    };
    ui.label(label);
    ui.horizontal(|ui| {
        ui.add(
            TextEdit::singleline(text)
                .hint_text(hint)
                .desired_width(ui.available_width() - 90.0),
        );
        if compact_button(ui, pal, ButtonKind::Outline, "Browse…", !dialog_open).clicked() {
            actions.push(Action::BrowseFolder(target));
        }
    });
    ui.end_row();
}

fn downloads_section(
    ui: &mut Ui,
    pal: &Palette,
    draft: &mut SettingsDraft,
    dialog_open: bool,
    actions: &mut Vec<Action>,
) {
    section(ui, pal, "Downloads", |ui| {
        egui::Grid::new("downloads-grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                folder_row(
                    ui,
                    pal,
                    &mut draft.download_dir,
                    FolderTarget::Download,
                    dialog_open,
                    actions,
                );
                folder_row(
                    ui,
                    pal,
                    &mut draft.watch_folder,
                    FolderTarget::Watch,
                    dialog_open,
                    actions,
                );
                let s = &mut draft.settings;
                ui.label("");
                toggle(ui, pal, &mut s.watch_enabled, "Watch folder enabled");
                ui.end_row();
                ui.label("Scan interval");
                ui.horizontal(|ui| {
                    ui.add(
                        DragValue::new(&mut s.watch_interval_secs)
                            .range(0..=86_400)
                            .suffix(" s"),
                    );
                    ui.label(
                        RichText::new("0 = manual scans only")
                            .small()
                            .color(pal.muted),
                    );
                });
                ui.end_row();
                ui.label("");
                toggle(ui, pal, &mut s.watch_auto_start, "Start the queue after a watch-folder import");
                ui.end_row();
            });
    });
}

fn performance_section(ui: &mut Ui, pal: &Palette, s: &mut Settings) {
    section(ui, pal, "Performance", |ui| {
        egui::Grid::new("performance-grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Max connections");
                ui.add(DragValue::new(&mut s.max_connections).range(1..=200));
                ui.end_row();
                ui.label("Speed limit (KiB/s, 0 = unlimited)");
                ui.add(
                    DragValue::new(&mut s.speed_limit_kbps)
                        .range(0..=10_000_000)
                        .speed(16),
                );
                ui.end_row();
                ui.label("Retries per article");
                ui.add(DragValue::new(&mut s.retries).range(0..=10));
                ui.end_row();
            });
        ui.label(
            RichText::new("Changes apply from the next job.")
                .small()
                .color(pal.muted),
        );
    });
}

fn postprocess_section(ui: &mut Ui, pal: &Palette, s: &mut Settings) {
    section(ui, pal, "Post-processing", |ui| {
        let p = &mut s.postprocess;
        toggle(ui, pal, &mut p.enabled, "Post-process finished jobs");
        ui.add_enabled_ui(p.enabled, |ui| {
            ui.indent("postprocess-steps", |ui| {
                toggle(ui, pal, &mut p.repair, "Verify and repair with PAR2");
                toggle(ui, pal, &mut p.extract, "Extract archives (7z, rar, zip)");
                toggle(ui, pal, &mut p.cleanup, "Delete archives after a successful extraction");
                toggle(
                    ui,
                    pal,
                    &mut p.remove_leftovers,
                    "Remove leftover files (.par2, repair backups, .sfv, .nfo) once verified",
                )
            .on_hover_text(
                "Runs when PAR2 verified or repaired the download and nothing failed to extract. \
                 Other files (samples, subtitles) are kept.",
                );
            });
        });
        ui.label(
            RichText::new(
                "PAR2 repair and RAR, 7z and zip extraction are built in; no external tools are needed.",
            )
            .small()
            .color(pal.muted),
        );
    });
}

fn automation_section(ui: &mut Ui, pal: &Palette, s: &mut Settings) {
    section(ui, pal, "Automation", |ui| {
        toggle(ui, pal, &mut s.auto_shutdown, "Shut down when queue finishes");
        ui.add_enabled_ui(s.auto_shutdown, |ui| {
            ui.horizontal(|ui| {
                ui.label("Countdown");
                ui.add(
                    DragValue::new(&mut s.auto_shutdown_countdown_secs)
                        .range(10..=600)
                        .suffix(" s"),
                );
                ui.label(
                    RichText::new("cancelable from the banner")
                        .small()
                        .color(pal.muted),
                );
            });
        });
    });
}

fn appearance_section(ui: &mut Ui, pal: &Palette, appearance: &mut Appearance) {
    section(ui, pal, "Appearance", |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            for a in Appearance::ALL {
                if theme_swatch(ui, pal, a, *appearance == a).clicked() {
                    *appearance = a;
                }
            }
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Follow desktop uses your desktop color scheme and follows its changes. \
                 The innerpulse themes are the studio's ink and paper grounds with the violet accent.",
            )
            .small()
            .color(pal.muted),
        );
    });
}

/// A theme choice as a card: a miniature window in that theme's colors (the desktop
/// option shows ink and paper side by side, since it follows whichever the desktop
/// uses) above its name; the chosen card is ringed in the accent.
fn theme_swatch(ui: &mut Ui, pal: &Palette, choice: Appearance, selected: bool) -> egui::Response {
    let size = egui::vec2(148.0, 112.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool(resp.id.with("hover"), resp.hovered());
        let on = ui.ctx().animate_bool(resp.id.with("on"), selected);
        let painter = ui.painter();
        let preview = egui::Rect::from_min_size(rect.min, egui::vec2(size.x, 82.0));
        let ink = Palette::from_theme(&super::brand_tokens::ink());
        let paper = Palette::from_theme(&super::brand_tokens::paper());
        match choice {
            Appearance::Ink => mini_window(painter, &ink, preview),
            Appearance::Paper => mini_window(painter, &paper, preview),
            Appearance::FollowDesktop => {
                let (l, r) = preview.split_left_right_at_fraction(0.5);
                mini_window(&painter.with_clip_rect(l), &paper, preview);
                mini_window(&painter.with_clip_rect(r), &ink, preview);
            }
        }
        let ring = pal.border_strong.lerp_to_gamma(pal.accent, on);
        painter.rect_stroke(
            preview,
            egui::CornerRadius::same(10),
            egui::Stroke::new(1.0 + 1.5 * on.max(hover * 0.4), ring),
            egui::StrokeKind::Outside,
        );
        let font = if selected { fonts::semibold_id(12.5) } else { egui::FontId::proportional(12.5) };
        painter.text(
            egui::pos2(rect.center().x, preview.bottom() + 15.0),
            egui::Align2::CENTER_CENTER,
            choice.label(),
            font,
            if selected { pal.text } else { pal.muted },
        );
        if resp.has_focus() {
            super::widgets::focus_ring(ui, pal, preview, 10);
        }
    }
    let label = choice.label();
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, selected, label));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A tiny window in `p`'s colors: toolbar, two rows with text and a segment track.
fn mini_window(painter: &egui::Painter, p: &Palette, r: egui::Rect) {
    use egui::{pos2, vec2, CornerRadius, Rect};
    painter.rect_filled(r, CornerRadius::same(10), p.surface);
    let bar = Rect::from_min_size(r.min, vec2(r.width(), 18.0));
    painter.rect_filled(bar, CornerRadius { nw: 10, ne: 10, sw: 0, se: 0 }, p.raised);
    for (i, h) in [6.0f32, 10.0, 8.0].iter().enumerate() {
        let x = r.left() + 10.0 + i as f32 * 3.0;
        painter.rect_filled(Rect::from_center_size(pos2(x, bar.center().y), vec2(1.6, *h)), CornerRadius::ZERO, p.mark);
    }
    painter.rect_filled(Rect::from_min_size(pos2(r.right() - 34.0, bar.top() + 5.0), vec2(24.0, 8.0)), CornerRadius::same(4), p.accent_soft);
    for row in 0..2 {
        let y = bar.bottom() + 12.0 + row as f32 * 26.0;
        painter.rect_filled(Rect::from_min_size(pos2(r.left() + 10.0, y), vec2(54.0, 5.0)), CornerRadius::same(2), p.text);
        painter.rect_filled(Rect::from_min_size(pos2(r.left() + 10.0, y + 9.0), vec2(36.0, 4.0)), CornerRadius::same(2), p.muted);
        let track = Rect::from_min_size(pos2(r.left() + 74.0, y + 3.0), vec2(r.width() - 86.0, 4.0));
        super::widgets::paint_segments(painter, p, track, if row == 0 { 0.62 } else { 0.25 }, p.accent, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_least_one_row_and_stable_ids() {
        let mut s = Settings::default();
        s.servers.clear();
        let mut d = SettingsDraft::new(s);
        assert_eq!(d.settings.servers.len(), 1, "normalized to one row");
        d.add_server();
        d.add_server();
        let ids = d.row_ids.clone();
        assert_eq!(ids.len(), 3);
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "distinct, increasing");
        d.tests.insert(ids[1], TestState::Running);
        d.remove_server(1);
        assert_eq!(d.row_ids, vec![ids[0], ids[2]]);
        d.set_test_result(ids[1], Ok("gone".into()));
        assert!(
            !d.tests.contains_key(&ids[1]),
            "result for a removed row is dropped"
        );
        d.set_test_result(ids[2], Ok("never asked".into()));
        assert!(
            !d.tests.contains_key(&ids[2]),
            "no test was started on that row"
        );
        d.tests.insert(ids[2], TestState::Running);
        d.set_test_result(ids[2], Ok("200 hi".into()));
        assert_eq!(
            d.tests.get(&ids[2]),
            Some(&TestState::Done(Ok("200 hi".into())))
        );
        d.remove_server(0);
        d.remove_server(0);
        assert_eq!(d.settings.servers.len(), 1, "never zero rows");
        assert!(!d.settings.servers[0].enabled);
        d.remove_server(99); // out of range is ignored
    }

    #[test]
    fn a_late_test_result_never_lands_on_a_reopened_sheet() {
        let mut first = SettingsDraft::new(Settings::default());
        let row = first.row_ids[0];
        first.tests.insert(row, TestState::Running);
        drop(first); // Cancel while the test runs.
        let mut second = SettingsDraft::new(Settings::default());
        assert!(!second.row_ids.contains(&row), "row ids are not reused");
        second.settings.servers[0].host = "b.example.net".into();
        second.set_test_result(row, Err("connection timed out".into()));
        assert!(second.tests.is_empty(), "{:?}", second.tests);
    }

    #[test]
    fn host_with_port_relative_folders_and_zero_connections_are_rejected() {
        let mut s = Settings {
            download_dir: PathBuf::from("/tmp/dl"),
            ..Default::default()
        };
        s.servers[0].host = "news.example.net:563".into();
        assert!(
            validate(&s).iter().any(|e| e.contains("Port field")),
            "{:?}",
            validate(&s)
        );
        for ok in ["news.example.net", "2001:db8::1", "::1", "10.0.0.1"] {
            s.servers[0].host = ok.into();
            assert!(validate(&s).is_empty(), "{ok}: {:?}", validate(&s));
        }
        s.servers[0].host = "[2001:db8::1]:563".into();
        assert!(!validate(&s).is_empty());
        s.servers[0].host = "news.example.net".into();

        s.download_dir = PathBuf::from("Downloads/usenet");
        assert!(validate(&s).iter().any(|e| e.contains("full path")));
        s.download_dir = PathBuf::from("/tmp/dl");
        s.watch_folder = PathBuf::from("nzbs");
        assert!(validate(&s).iter().any(|e| e.contains("Watch folder")));
        s.watch_folder = PathBuf::new();

        // The sheet validates before normalizing, so 0 is reported rather than changed.
        let mut d = SettingsDraft::new(s);
        d.download_dir = "/tmp/dl".into();
        d.settings.max_connections = 0;
        assert!(validate(&d.edited())
            .iter()
            .any(|e| e.contains("Max connections")));
        assert_eq!(
            d.result().max_connections.max(1),
            d.result().max_connections
        );
    }

    #[test]
    fn tilde_folders_expand_to_home() {
        let Some(home) = crate::launch::home_dir() else {
            return;
        };
        let mut d = SettingsDraft::new(Settings::default());
        d.download_dir = " ~/Downloads/usenet ".into();
        d.watch_folder = "~".into();
        let r = d.result();
        assert_eq!(r.download_dir, home.join("Downloads/usenet"));
        assert_eq!(r.watch_folder, home);
        assert!(validate(&d.edited())
            .iter()
            .all(|e| !e.contains("full path")));
    }

    #[test]
    fn validation() {
        let mut s = Settings::default();
        s.servers[0].host = "news.example.net".into();
        assert!(validate(&s).is_empty(), "{:?}", validate(&s));
        s.servers[0].port = 0;
        s.servers[0].connections = 0;
        let e = validate(&s);
        assert!(e.iter().any(|e| e.contains("port")));
        assert!(e.iter().any(|e| e.contains("connections")));
        s.servers[0].port = 563;
        s.servers[0].connections = 4;
        s.servers[0].host = "nntps://news.example.net".into();
        assert_eq!(validate(&s).len(), 1);
        s.servers[0].host = "news.example.net".into();
        s.watch_enabled = true;
        s.watch_folder = PathBuf::new();
        assert_eq!(validate(&s).len(), 1);
        s.watch_enabled = false;
        s.download_dir = PathBuf::new();
        assert_eq!(validate(&s).len(), 1);
    }

    #[test]
    fn result_applies_folder_text_and_trims() {
        let mut d = SettingsDraft::new(Settings::default());
        d.download_dir = "  /tmp/dl ".into();
        d.set_folder(FolderTarget::Watch, PathBuf::from("/tmp/watch"));
        d.settings.servers[0].host = " news.example.net ".into();
        let r = d.result();
        assert_eq!(r.download_dir, PathBuf::from("/tmp/dl"));
        assert_eq!(r.watch_folder, PathBuf::from("/tmp/watch"));
        assert_eq!(r.servers[0].host, "news.example.net");
        assert_eq!(d.folder(FolderTarget::Watch), PathBuf::from("/tmp/watch"));
    }
}
