//! Toolbar (outline §6.1) and the inline Open NZB field.

use std::path::PathBuf;

use eframe::egui::{self, Align, Layout, RichText, TextEdit, Ui};

use super::app::{Action, Data, Tab, ViewState};
use super::theme::Palette;
use super::widgets::{icon_button, paint_icon, tool_button, Icon, ToolState};

/// Width below which toolbar labels are hidden (icons + tooltips only).
const LABEL_MIN_WIDTH: f32 = 1180.0;

const NOT_YET: &str =
    "Not available yet: browsing needs a Newznab indexer, which is not configured.";

/// Draws the toolbar.
pub fn show(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    view: &mut ViewState,
    actions: &mut Vec<Action>,
) {
    let labels = ui.ctx().content_rect().width() >= LABEL_MIN_WIDTH;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let t = |ui: &mut Ui, icon, label: &str, state, tip: &str| {
            tool_button(ui, pal, icon, label, labels, state).on_hover_text(tip)
        };

        if t(
            ui,
            Icon::Back,
            "Back",
            ToolState::Unavailable,
            "Back (no browsing history yet)",
        )
        .clicked()
        {
            // Harmless no-op until browse exists.
        }
        if t(
            ui,
            Icon::Home,
            "Home",
            ToolState::Normal,
            "Go to the default tab (Queue)",
        )
        .clicked()
        {
            actions.push(Action::SetTab(Tab::Queue));
        }
        let search_state = if view.tab == Tab::Search {
            ToolState::Active
        } else {
            ToolState::Normal
        };
        if t(
            ui,
            Icon::Search,
            "Search",
            search_state,
            "Open the Search tab",
        )
        .clicked()
        {
            actions.push(Action::FocusSearch);
        }
        t(ui, Icon::List, "My List", ToolState::Unavailable, NOT_YET);
        t(
            ui,
            Icon::Newsgroups,
            "Newsgroups",
            ToolState::Unavailable,
            NOT_YET,
        );
        t(
            ui,
            Icon::AutoSearch,
            "Auto Search",
            ToolState::Unavailable,
            NOT_YET,
        );
        ui.add_space(6.0);
        let open_state = if view.open_nzb.visible {
            ToolState::Active
        } else {
            ToolState::Normal
        };
        if t(
            ui,
            Icon::OpenNzb,
            "Open NZB",
            open_state,
            "Import an NZB file (Ctrl+O)",
        )
        .clicked()
        {
            if view.open_nzb.visible {
                view.open_nzb.visible = false;
                view.open_nzb.error = None;
            } else {
                actions.push(Action::ShowOpenNzb);
            }
        }
        let folder_tip = format!(
            "Open {} in the file manager",
            data.settings.download_dir.display()
        );
        if t(
            ui,
            Icon::Folder,
            "Download Folder",
            ToolState::Normal,
            &folder_tip,
        )
        .clicked()
        {
            actions.push(Action::OpenDownloadFolder);
        }
        let queue_state = if view.tab == Tab::Queue {
            ToolState::Active
        } else {
            ToolState::Normal
        };
        if t(
            ui,
            Icon::Transfers,
            "Transfers",
            queue_state,
            "Show the download queue",
        )
        .clicked()
        {
            actions.push(Action::SetTab(Tab::Queue));
        }
        let paused = data.status.paused;
        let (icon, label, tip) = if paused {
            (Icon::Play, "Resume", "Resume the queue")
        } else {
            (
                Icon::Pause,
                "Pause",
                "Pause the queue after the current job",
            )
        };
        if t(ui, icon, label, ToolState::Normal, tip).clicked() {
            actions.push(if paused {
                Action::Resume
            } else {
                Action::Pause
            });
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if tool_button(ui, pal, Icon::Gear, "Settings", false, ToolState::Normal)
                .on_hover_text("Settings (Ctrl+,)")
                .clicked()
            {
                actions.push(Action::OpenSettings);
            }
            search_box(ui, pal, view, actions);
        });
    });

    if view.open_nzb.visible {
        open_nzb_row(ui, pal, view, actions);
    }
}

fn search_box(ui: &mut Ui, pal: &Palette, view: &mut ViewState, actions: &mut Vec<Action>) {
    let width = (ui.available_width() - 8.0).clamp(120.0, 260.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 28.0), egui::Sense::hover());
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let edit = TextEdit::singleline(&mut view.search_text)
        .id_salt("toolbar-search")
        .hint_text(RichText::new("Search (no indexer configured)").color(pal.muted))
        .desired_width(width - 30.0)
        .margin(egui::Margin {
            left: 26,
            right: 6,
            top: 5,
            bottom: 5,
        });
    let resp = child.add(edit);
    paint_icon(
        ui.painter(),
        egui::Rect::from_center_size(
            egui::pos2(rect.left() + 14.0, rect.center().y),
            egui::vec2(14.0, 14.0),
        ),
        Icon::Search,
        pal.muted,
    );
    if view.focus_search {
        resp.request_focus();
        view.focus_search = false;
    }
    if resp.lost_focus()
        && ui.input(|i| i.key_pressed(egui::Key::Enter))
        && !view.search_text.trim().is_empty()
    {
        actions.push(Action::SetTab(Tab::Search));
    }
    resp.on_hover_text("Usenet search needs a Newznab indexer; none is configured.");
}

fn open_nzb_row(ui: &mut Ui, pal: &Palette, view: &mut ViewState, actions: &mut Vec<Action>) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("NZB file").color(pal.muted));
        let width = (ui.available_width() - 250.0).max(160.0);
        let resp = ui.add(
            TextEdit::singleline(&mut view.open_nzb.path)
                .id_salt("open-nzb-path")
                .hint_text("/path/to/file.nzb")
                .desired_width(width),
        );
        if view.open_nzb.focus {
            resp.request_focus();
            view.open_nzb.focus = false;
        }
        let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if resp.changed() {
            view.open_nzb.error = None;
        }
        let browse = ui.add_enabled(!view.dialog_open, egui::Button::new("Browse…"));
        if browse.clicked() {
            actions.push(Action::BrowseNzb);
        }
        let path = view.open_nzb.path.trim().to_owned();
        let add = ui.add_enabled(
            !path.is_empty(),
            egui::Button::new(RichText::new("Add").strong()),
        );
        if (add.clicked() || submit) && !path.is_empty() {
            actions.push(Action::AddNzb(PathBuf::from(path)));
        }
        if icon_button(ui, pal, Icon::Close, pal.muted, "Close (Esc)").clicked() {
            view.open_nzb.visible = false;
            view.open_nzb.error = None;
        }
    });
    if let Some(err) = &view.open_nzb.error {
        ui.label(RichText::new(err).color(pal.negative));
    }
}
