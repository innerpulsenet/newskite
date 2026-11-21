//! The queue's actions shared by the header and the sidebar: what the queue control does
//! now, the masthead's server readout, and the inline Add NZB field.

use eframe::egui::{self, RichText, TextEdit, Ui};
use newskite_service::JobState;
use newskite_types::Settings;

use super::app::{Action, Data, ViewState};
use super::theme::Palette;
use super::widgets::{
    compact_button, error_line, icon_button, ButtonKind, Icon,
};

/// Longest server address the readout shows before eliding.
const READOUT_HOST_MAX: usize = 26;

/// The masthead's mono readout: true facts about the connection Newskite would use,
/// e.g. `NNTP · NEWS.EXAMPLE.NET:563 TLS · 12 CONN`, or `NO SERVER`.
pub fn masthead_readout(settings: &Settings) -> String {
    match settings.first_usable_server() {
        None => "NO SERVER".to_owned(),
        Some(s) => {
            let addr = s.address();
            let addr: String = if addr.chars().count() > READOUT_HOST_MAX {
                let cut: String = addr.chars().take(READOUT_HOST_MAX - 1).collect();
                format!("{cut}…")
            } else {
                addr
            };
            format!(
                "NNTP · {} {} · {} CONN",
                addr,
                if s.tls { "TLS" } else { "PLAIN" },
                settings.effective_connections()
            )
            .to_uppercase()
        }
    }
}

/// The global queue control: what it does now, its icon, label and tooltip, and
/// whether it can act.
pub fn queue_control(data: &Data) -> (Action, Icon, &'static str, &'static str, bool) {
    let st = &data.status;
    if st.paused {
        (
            Action::Resume,
            Icon::Play,
            "Resume",
            "Resume the queue",
            true,
        )
    } else if st.running {
        (
            Action::Pause,
            Icon::Pause,
            "Pause",
            "Pause the queue now (the current download resumes first)",
            true,
        )
    } else {
        let queued = data.jobs.iter().any(|j| j.state == JobState::Queued);
        let server = data.settings.first_usable_server().is_some();
        let tip = if !queued {
            "Nothing queued to start (held jobs start from their row)"
        } else if !server {
            "Add a server in Settings before starting"
        } else {
            "Download every queued job, in queue order"
        };
        (Action::Start, Icon::Play, "Start", tip, queued && server)
    }
}

pub(crate) fn open_nzb_row(ui: &mut Ui, pal: &Palette, view: &mut ViewState, actions: &mut Vec<Action>) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("NZB file").color(pal.muted));
        let width = (ui.available_width() - 250.0).max(160.0);
        let resp = ui.add(
            TextEdit::singleline(&mut view.open_nzb.path)
                .id_salt("open-nzb-path")
                .hint_text(if cfg!(windows) { r"C:\path\to\file.nzb" } else { "/path/to/file.nzb" })
                .desired_width(width),
        );
        if view.open_nzb.focus {
            resp.request_focus();
            view.open_nzb.focus = false;
        }
        view.open_nzb.has_focus = resp.has_focus();
        let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if resp.changed() {
            view.open_nzb.error = None;
        }
        if compact_button(ui, pal, ButtonKind::Outline, "Browse…", !view.dialog_open).clicked() {
            actions.push(Action::BrowseNzb);
        }
        let text = view.open_nzb.path.trim().to_owned();
        let add = compact_button(ui, pal, ButtonKind::Outline, "Add", !text.is_empty());
        if (add.clicked() || submit) && !text.is_empty() {
            // Pasted file:// URIs (as file managers copy them) and ~ paths work too.
            match crate::launch::path_from_text(&text) {
                Ok(path) => actions.push(Action::AddNzb(path)),
                Err(e) => view.open_nzb.error = Some(e),
            }
        }
        if icon_button(ui, pal, Icon::Close, pal.muted, "Close (Esc)").clicked() {
            view.open_nzb.visible = false;
            view.open_nzb.error = None;
        }
    });
    if view.open_nzb.importing > 0 {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new("Importing…").color(pal.muted));
        });
    }
    if let Some(err) = &view.open_nzb.error {
        error_line(ui, pal, err);
    }
}
