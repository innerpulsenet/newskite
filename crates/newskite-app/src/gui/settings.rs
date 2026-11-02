//! Settings sheet (outline §6.5, F8): a modal editing a cloned [`Settings`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::egui::{self, Align, DragValue, Id, Layout, RichText, TextEdit, Ui};
use newskite_types::{ServerConfig, Settings};

use super::app::Action;
use super::appearance::Appearance;
use super::theme::Palette;
use super::widgets::{card, error_line, kit_button, notice, section_title, ButtonKind, Status};
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
    let max_h = (ctx.content_rect().height() - 210.0).max(220.0);
    let width = (ctx.content_rect().width() - 80.0).clamp(560.0, 820.0);
    let t = super::motion::fade_in(ctx, fade_id(), 0.16, reduced_motion);
    egui::Modal::new(Id::new("settings-modal"))
        .backdrop_color(Palette::fade(pal.shadow, t))
        .show(ctx, |ui| {
            ui.set_opacity(t);
            ui.set_width(width);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Settings").heading().strong().color(pal.text));
            });
            ui.add_space(6.0);
            if let Some(e) = &draft.load_error {
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
                ui.add_space(8.0);
            }
            egui::ScrollArea::vertical()
                .max_height(max_h)
                .min_scrolled_height(max_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    connection_section(ui, pal, draft, actions);
                    ui.add_space(8.0);
                    downloads_section(ui, pal, draft, dialog_open, actions);
                    ui.add_space(8.0);
                    performance_section(ui, pal, &mut draft.settings);
                    ui.add_space(8.0);
                    postprocess_section(ui, pal, &mut draft.settings);
                    ui.add_space(8.0);
                    automation_section(ui, pal, &mut draft.settings);
                    ui.add_space(8.0);
                    appearance_section(ui, pal, &mut draft.appearance);
                });

            let errors = validate(&draft.edited());
            let result = draft.result();
            ui.add_space(8.0);
            for e in &errors {
                error_line(ui, pal, e);
            }
            if let Some(e) = &draft.save_error {
                error_line(ui, pal, &format!("Saving failed: {e}"));
            }
            ui.separator();
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
        });
}

fn section(ui: &mut Ui, pal: &Palette, title: &str, body: impl FnOnce(&mut Ui)) {
    section_title(ui, pal, title);
    ui.add_space(2.0);
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
                    if ui
                        .button("Remove")
                        .on_hover_text("Remove this server")
                        .clicked()
                    {
                        remove = Some(i);
                    }
                    let running = draft.tests.get(&row_id) == Some(&TestState::Running);
                    let can_test = !srv.host.trim().is_empty() && !running;
                    if ui
                        .add_enabled(
                            can_test,
                            egui::Button::new(if running { "Testing…" } else { "Test" }),
                        )
                        .on_hover_text("Connect, authenticate and send DATE; shows the raw replies")
                        .clicked()
                    {
                        draft.tests.insert(row_id, TestState::Running);
                        let mut cfg = srv.clone();
                        cfg.host = cfg.host.trim().to_owned();
                        actions.push(Action::TestServer(row_id, Box::new(cfg)));
                    }
                    ui.checkbox(&mut srv.enabled, "Enabled");
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
                        if ui.checkbox(&mut srv.tls, "TLS").changed() {
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
        if ui.button("Add server").clicked() {
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
    label: &str,
    text: &mut String,
    hint: &str,
    target: FolderTarget,
    dialog_open: bool,
    actions: &mut Vec<Action>,
) {
    ui.label(label);
    ui.horizontal(|ui| {
        ui.add(
            TextEdit::singleline(text)
                .hint_text(hint)
                .desired_width(ui.available_width() - 90.0),
        );
        if ui
            .add_enabled(!dialog_open, egui::Button::new("Browse…"))
            .clicked()
        {
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
                    "Download folder",
                    &mut draft.download_dir,
                    "~/Downloads/Newskite",
                    FolderTarget::Download,
                    dialog_open,
                    actions,
                );
                folder_row(
                    ui,
                    "Watch folder",
                    &mut draft.watch_folder,
                    "(none)",
                    FolderTarget::Watch,
                    dialog_open,
                    actions,
                );
                let s = &mut draft.settings;
                ui.label("");
                ui.checkbox(&mut s.watch_enabled, "Watch folder enabled");
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
                ui.checkbox(
                    &mut s.watch_auto_start,
                    "Start the queue after a watch-folder import",
                );
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
        ui.checkbox(&mut p.enabled, "Post-process finished jobs");
        ui.add_enabled_ui(p.enabled, |ui| {
            ui.checkbox(&mut p.repair, "Verify and repair with PAR2");
            ui.checkbox(&mut p.extract, "Extract archives (7z, rar, zip)");
            ui.checkbox(&mut p.cleanup, "Delete archives after a successful extraction");
            ui.checkbox(
                &mut p.remove_leftovers,
                "Remove leftover files (.par2, repair backups, .sfv, .nfo) once verified",
            )
            .on_hover_text(
                "Runs when PAR2 verified or repaired the download and nothing failed to extract. \
                 Other files (samples, subtitles) are kept.",
            );
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
        ui.checkbox(&mut s.auto_shutdown, "Shut down when queue finishes");
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
            ui.label("Theme");
            ui.add_space(8.0);
            for a in Appearance::ALL {
                ui.radio_value(appearance, a, a.label());
                ui.add_space(10.0);
            }
        });
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
