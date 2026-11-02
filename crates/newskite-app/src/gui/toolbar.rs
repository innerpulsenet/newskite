//! Toolbar with the masthead: Add NZB, the queue control, Scan Folder, Downloads and
//! Settings, and the inline Add NZB field.

use eframe::egui::{self, Align, Layout, Pos2, Rect, RichText, Sense, Stroke, TextEdit, Ui, Vec2};
use newskite_service::JobState;
use newskite_types::Settings;

use super::app::{Action, Data, ViewState};
use super::brand;
use super::fonts;
use super::icons;
use super::theme::Palette;
use super::widgets::{error_line, icon_button, spaced, tool_button, Icon, ToolState};

/// Width below which toolbar labels are hidden (icons + tooltips only).
const LABEL_MIN_WIDTH: f32 = 900.0;
/// Side of the masthead mark (whole-pixel bars at 1× and 2×).
pub const MASTHEAD_MARK: f32 = 24.0;
/// Width of the masthead wordmark (≈ 18.5 px Syne: the mark is 1.3× its font size).
const WORDMARK_W: f32 = 128.0;
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

/// The masthead: the live mark beside the product wordmark, the readout under the
/// wordmark. A click opens About.
fn masthead(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    view: &ViewState,
    row_h: f32,
) -> egui::Response {
    let readout = masthead_readout(&data.settings);
    let job = spaced(&readout, fonts::mono_id(11.0), pal.muted, 0.66);
    let galley = ui.painter().layout_job(job);
    let (_, _, bw, bh) = icons::WORDMARK_BOX;
    let wm_h = WORDMARK_W * bh / bw;
    let gap = 9.0;
    let block_w = WORDMARK_W.max(galley.size().x);
    let size = Vec2::new(MASTHEAD_MARK + gap + block_w + 4.0, row_h);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if resp.has_focus() {
            super::widgets::focus_ring(ui, pal, rect.shrink(2.0), 4);
        }
        let mark = Rect::from_center_size(
            Pos2::new(rect.left() + MASTHEAD_MARK / 2.0, rect.center().y),
            Vec2::splat(MASTHEAD_MARK),
        );
        brand::paint_mark(painter, mark, &view.masthead_mark, pal.mark);
        let block_h = wm_h + 5.0 + galley.size().y;
        let top = (rect.center().y - block_h / 2.0).round();
        let x = rect.left() + MASTHEAD_MARK + gap;
        let wm = Rect::from_min_size(Pos2::new(x, top), Vec2::new(WORDMARK_W, wm_h));
        icons::paint_svg(painter, icons::WORDMARK_URI, wm, pal.text);
        painter.galley(Pos2::new(x, top + wm_h + 5.0), galley, pal.muted);
    }
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            true,
            format!("Newskite · {readout}"),
        )
    });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("About Newskite")
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
        let runnable = data.jobs.iter().any(|j| j.state == JobState::Queued);
        let tip = if runnable {
            "Download every queued job, in queue order"
        } else {
            "Nothing queued to start (held jobs start from their row)"
        };
        (Action::Start, Icon::Play, "Start", tip, runnable)
    }
}

/// Draws the toolbar.
pub fn show(
    ui: &mut Ui,
    pal: &Palette,
    data: &Data,
    view: &mut ViewState,
    actions: &mut Vec<Action>,
) {
    let labels = ui.ctx().content_rect().width() >= LABEL_MIN_WIDTH;
    let row_h = if labels { 44.0 } else { 36.0 };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if masthead(ui, pal, data, view, row_h).clicked() {
            actions.push(Action::OpenAbout);
        }
        // A hairline between the signature and the controls.
        let (sep, _) = ui.allocate_exact_size(Vec2::new(21.0, row_h), Sense::hover());
        let x = sep.center().x.round() + 0.5;
        ui.painter().line_segment(
            [
                Pos2::new(x, sep.top() + row_h * 0.22),
                Pos2::new(x, sep.bottom() - row_h * 0.22),
            ],
            Stroke::new(1.0, pal.border),
        );
        let t = |ui: &mut Ui, icon, label: &str, state, tip: &str| {
            tool_button(ui, pal, icon, label, labels, state).on_hover_text(tip)
        };

        let open_state = if view.open_nzb.visible {
            ToolState::Active
        } else {
            ToolState::Normal
        };
        if t(
            ui,
            Icon::OpenNzb,
            "Add NZB",
            open_state,
            "Import an NZB file (Ctrl+O), or drop .nzb files onto the window",
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
        let (action, icon, label, tip, enabled) = queue_control(data);
        // The control that moves the queue leads the row.
        let state = if enabled {
            ToolState::Accent
        } else {
            ToolState::Unavailable
        };
        if t(ui, icon, label, state, tip).clicked() && enabled {
            actions.push(action);
        }
        let watch = &data.settings.watch_folder;
        if !watch.as_os_str().is_empty() {
            let (state, tip) = if view.scanning {
                (ToolState::Active, "Scanning the watch folder…".to_owned())
            } else {
                (
                    ToolState::Normal,
                    format!("Import new .nzb files from {} now", watch.display()),
                )
            };
            if t(ui, Icon::Scan, "Scan Folder", state, &tip).clicked() && !view.scanning {
                actions.push(Action::ScanWatch);
            }
        }
        let folder_tip = format!(
            "Open {} in the file manager",
            data.settings.download_dir.display()
        );
        if t(
            ui,
            Icon::Folder,
            "Downloads",
            ToolState::Normal,
            &folder_tip,
        )
        .clicked()
        {
            actions.push(Action::OpenDownloadFolder);
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if tool_button(ui, pal, Icon::Gear, "Settings", labels, ToolState::Normal)
                .on_hover_text("Settings (Ctrl+,)")
                .clicked()
            {
                actions.push(Action::OpenSettings);
            }
        });
    });

    if view.open_nzb.visible {
        open_nzb_row(ui, pal, view, actions);
    }
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
        let text = view.open_nzb.path.trim().to_owned();
        let add = ui.add_enabled(
            !text.is_empty(),
            egui::Button::new(RichText::new("Add").strong()),
        );
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
