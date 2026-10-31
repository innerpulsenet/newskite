//! Settings sheet (outline §6.5, F8): a modal editing a cloned [`Settings`].

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui::{self, Align, DragValue, Id, Layout, RichText, TextEdit, Ui};
use newskite_types::{ServerConfig, Settings};

use super::app::Action;
use super::theme::Palette;
use super::widgets::{card, section_title};

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
    /// on the right row after rows are removed.
    pub row_ids: Vec<u64>,
    next_row_id: u64,
    /// Test state per row id.
    pub tests: HashMap<u64, TestState>,
    /// Download folder as text.
    pub download_dir: String,
    /// Watch folder as text.
    pub watch_folder: String,
    /// Error from the last save attempt.
    pub save_error: Option<String>,
}

impl SettingsDraft {
    /// Starts editing a copy of `settings` (normalized: at least one server row).
    pub fn new(settings: Settings) -> Self {
        let settings = settings.normalized();
        let n = settings.servers.len() as u64;
        Self {
            download_dir: settings.download_dir.display().to_string(),
            watch_folder: settings.watch_folder.display().to_string(),
            settings,
            row_ids: (0..n).collect(),
            next_row_id: n,
            tests: HashMap::new(),
            save_error: None,
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

    /// Records a test result for a row (ignored if the row was removed).
    pub fn set_test_result(&mut self, row_id: u64, result: Result<String, String>) {
        if self.row_ids.contains(&row_id) {
            self.tests.insert(row_id, TestState::Done(result));
        }
    }

    /// Appends an empty server row.
    pub fn add_server(&mut self) {
        self.settings.servers.push(ServerConfig::default());
        self.row_ids.push(self.next_row_id);
        self.next_row_id += 1;
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

    /// The settings that Save would persist (folder texts applied, trimmed).
    pub fn result(&self) -> Settings {
        let mut s = self.settings.clone();
        s.download_dir = PathBuf::from(self.download_dir.trim());
        s.watch_folder = PathBuf::from(self.watch_folder.trim());
        for srv in &mut s.servers {
            srv.host = srv.host.trim().to_owned();
            srv.username = srv.username.trim().to_owned();
        }
        s.normalized()
    }
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
    }
    if s.watch_enabled && s.watch_folder.as_os_str().is_empty() {
        errs.push("Choose a watch folder or turn the watch folder off.".to_owned());
    }
    errs
}

/// Draws the modal. Esc is handled by the app (it closes without saving).
pub fn show(
    ctx: &egui::Context,
    pal: &Palette,
    draft: &mut SettingsDraft,
    dialog_open: bool,
    actions: &mut Vec<Action>,
) {
    let max_h = (ctx.content_rect().height() - 210.0).max(220.0);
    let width = (ctx.content_rect().width() - 80.0).clamp(560.0, 820.0);
    egui::Modal::new(Id::new("settings-modal"))
        .backdrop_color(pal.shadow)
        .show(ctx, |ui| {
            ui.set_width(width);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Settings").heading().strong().color(pal.text));
            });
            ui.add_space(6.0);
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
                });

            let result = draft.result();
            let errors = validate(&result);
            ui.add_space(8.0);
            for e in &errors {
                ui.label(RichText::new(e).color(pal.negative));
            }
            if let Some(e) = &draft.save_error {
                ui.label(RichText::new(format!("Saving failed: {e}")).color(pal.negative));
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
                    let save = ui.add_enabled(
                        errors.is_empty(),
                        egui::Button::new(RichText::new("Save").strong()),
                    );
                    if save.clicked() {
                        actions.push(Action::SaveSettings(Box::new(result)));
                    }
                    if ui.button("Cancel").clicked() {
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
                    let (text, color) = match r {
                        Ok(t) => (t.as_str(), pal.text),
                        Err(e) => (e.as_str(), pal.negative),
                    };
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
                            ui.label(RichText::new(text).monospace().color(color));
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
            ui.checkbox(
                &mut p.cleanup,
                "Delete archives and parity files after a successful extraction",
            );
        });
        ui.label(
            RichText::new(
                "Uses par2, 7z, unrar and unzip when installed; missing tools are skipped.",
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
                    RichText::new("cancellable from the banner")
                        .small()
                        .color(pal.muted),
                );
            });
        });
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
        assert_eq!(d.row_ids, vec![0, 1, 2]);
        d.tests.insert(1, TestState::Running);
        d.remove_server(1);
        assert_eq!(d.row_ids, vec![0, 2]);
        d.set_test_result(1, Ok("gone".into()));
        assert!(
            !d.tests.contains_key(&1),
            "result for a removed row is dropped"
        );
        d.set_test_result(2, Ok("200 hi".into()));
        assert_eq!(d.tests.get(&2), Some(&TestState::Done(Ok("200 hi".into()))));
        d.remove_server(0);
        d.remove_server(0);
        assert_eq!(d.settings.servers.len(), 1, "never zero rows");
        assert!(!d.settings.servers[0].enabled);
        d.remove_server(99); // out of range is ignored
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
