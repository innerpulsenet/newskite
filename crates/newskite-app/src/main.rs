//! `newskite`: the desktop Usenet (NZB) client.
//!
//! * `newskite --headless ...` runs the headless CLI (`newskite_cli`).
//! * Otherwise the window opens; positional `.nzb` paths, `file://` and local `nzb://`
//!   URIs are queued at launch.

use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use eframe::egui;
use newskite_app::gui::{appearance, fonts, theme};
use newskite_app::launch::{self, LaunchItem, Mode};
use newskite_app::{AppConfig, NewskiteApp};
use newskite_service::{LogLevel, Service, ServiceError, ServiceOptions, StoreError};
use newskite_types::config::default_data_dir;

fn main() -> ExitCode {
    match launch::parse_args(std::env::args_os()) {
        Mode::Headless(args) => {
            newskite_cli::run_headless(newskite_cli::HeadlessArgs::parse_from(args))
        }
        Mode::Help => {
            println!("{}", launch::USAGE);
            ExitCode::SUCCESS
        }
        Mode::Version => {
            println!("newskite {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Mode::Usage(msg) => {
            eprintln!("newskite: {msg}\n\n{}", launch::USAGE);
            ExitCode::from(2)
        }
        Mode::Gui(items) => run_gui(items),
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("warn,newskite=info,newskite_service=info")
    });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Shows a fatal start-up error (the main window has not opened) in a small window of
/// its own and on stderr.
fn fatal(message: &str) -> ExitCode {
    eprintln!("newskite: {message}");
    show_message("Newskite cannot start", message, true);
    ExitCode::FAILURE
}

/// The app icon (the innerpulse ink tile and mark).
const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/brand/app-icon-256.png");

/// Another process holds the data directory: say so in a small window (no panic, no
/// half-open second window) and exit.
fn already_open(data_dir: &std::path::Path, items: &[LaunchItem]) -> ExitCode {
    let message = launch::instance_conflict_message(data_dir, items);
    eprintln!("newskite: {}", message.replace("\n\n", " "));
    show_message("Newskite is already open", &message, false);
    ExitCode::FAILURE
}

/// Shows a message before (or instead of) the main window. The message is drawn in a
/// small window of our own, so it needs nothing beyond what the app needs; only if that
/// window cannot open is a desktop dialog tool tried.
fn show_message(title: &str, message: &str, error: bool) {
    if !message_window(title, message) {
        native_dialog(title, message, error);
    }
}

/// A desktop dialog for when no window of our own can open: kdialog on KDE, zenity
/// (rfd's backend) elsewhere. Only tools that are installed are tried, so a missing one
/// never swallows the message silently; stderr always has it.
fn native_dialog(title: &str, message: &str, error: bool) {
    let on_path = |tool: &str| {
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
    };
    if on_path("kdialog") {
        let shown = std::process::Command::new("kdialog")
            .arg("--title")
            .arg(title)
            .arg(if error { "--error" } else { "--msgbox" })
            .arg(message)
            .status();
        if shown.is_ok() {
            return;
        }
    }
    if on_path("zenity") {
        let _ = rfd::MessageDialog::new()
            .set_level(if error {
                rfd::MessageLevel::Error
            } else {
                rfd::MessageLevel::Info
            })
            .set_title(title)
            .set_description(message)
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }
}

/// The message window: the title, the text and an OK button, in the desktop theme.
/// Returns whether it could be shown.
fn message_window(title: &str, message: &str) -> bool {
    struct MessageApp {
        title: String,
        message: String,
    }
    impl eframe::App for MessageApp {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            let close =
                ctx.input(|i| i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Escape));
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(20)),
                )
                .show(ctx, |ui| {
                    ui.label(egui::RichText::new(&self.title).heading().strong());
                    ui.add_space(10.0);
                    ui.add(egui::Label::new(&self.message).wrap());
                    ui.add_space(16.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        let ok = ui.add(egui::Button::new("OK").min_size(egui::vec2(72.0, 28.0)));
                        ok.request_focus();
                        if ok.clicked() || close {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
        }
    }
    let lines = message.lines().count().max(1) + message.len() / 60;
    let height = (150.0 + 18.0 * lines as f32).clamp(190.0, 420.0);
    let mut viewport = egui::ViewportBuilder::default()
        .with_app_id(newskite_types::APP_ID)
        .with_title(title)
        .with_inner_size([480.0, height])
        .with_resizable(false);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(APP_ICON_PNG) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let native = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let app = MessageApp {
        title: title.to_owned(),
        message: message.to_owned(),
    };
    eframe::run_native(
        title,
        native,
        Box::new(move |cc| {
            let theme = newskite_theme::resolve_with_hint(
                cc.egui_ctx.system_theme().map(|t| t == egui::Theme::Dark),
            );
            fonts::install(&cc.egui_ctx);
            theme::apply(&cc.egui_ctx, &theme);
            Ok(Box::new(app))
        }),
    )
    .is_ok()
}

/// winit's Wayland backend delivers no file drag and drop (only its X11 backend does), so
/// on a Wayland session with XWayland available the window runs through X11 and NZB files
/// can be dropped onto it. `NEWSKITE_WAYLAND=1` keeps the native Wayland backend (no
/// drops; Ctrl+V of copied files and Add NZB still work). The X11 window keeps the app id
/// as its WM_CLASS, so the taskbar still matches the desktop entry.
#[cfg(all(unix, not(target_os = "macos")))]
fn x11_for_drag_and_drop() -> Option<eframe::EventLoopBuilderHook> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
    let x11 = std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty());
    let opt_out = std::env::var("NEWSKITE_WAYLAND").is_ok_and(|v| v.trim() == "1");
    if !wayland || !x11 || opt_out {
        return None;
    }
    tracing::info!("using the X11 backend (XWayland) so NZB files can be dropped on the window");
    Some(Box::new(|builder| {
        use winit::platform::x11::EventLoopBuilderExtX11 as _;
        builder.with_x11();
    }))
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn x11_for_drag_and_drop() -> Option<eframe::EventLoopBuilderHook> {
    None
}

fn run_gui(items: Vec<LaunchItem>) -> ExitCode {
    init_tracing();

    // The runtime lives for the whole process, alongside eframe's event loop on the
    // main thread; the service spawns its queue runner and timers onto it.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("newskite-rt")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return fatal(&format!("cannot start the async runtime: {e}")),
    };

    let data_dir = default_data_dir();
    let opts = ServiceOptions {
        data_dir: data_dir.clone(),
        allow_shutdown: true,
    };
    let service = match Service::open(opts, rt.handle().clone()) {
        Ok(s) => s,
        Err(ServiceError::Store(StoreError::Invalid(msg)))
            if launch::is_instance_conflict(&msg) =>
        {
            return already_open(&data_dir, &items);
        }
        Err(e) => {
            return fatal(&format!(
                "cannot open the database in {}: {e}",
                default_data_dir().display()
            ))
        }
    };

    // Queue launch arguments before the window shows (errors land in the message log).
    let mut imported = 0usize;
    for item in items {
        match item {
            LaunchItem::Nzb(path) => {
                if service.add_nzb(&path).is_ok() {
                    imported += 1;
                }
            }
            LaunchItem::Unsupported { arg, reason } => {
                service.log(LogLevel::Warn, format!("cannot open {arg}: {reason}"));
            }
        }
    }
    if imported > 0 {
        tracing::info!("queued {imported} NZB(s) from the command line");
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_app_id(newskite_types::APP_ID)
        .with_title("Newskite")
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([960.0, 600.0]);
    match eframe::icon_data::from_png_bytes(APP_ICON_PNG) {
        Ok(icon) => viewport = viewport.with_icon(std::sync::Arc::new(icon)),
        Err(e) => tracing::warn!("cannot decode the window icon: {e}"),
    }
    let native = eframe::NativeOptions {
        viewport,
        event_loop_builder: x11_for_drag_and_drop(),
        ..Default::default()
    };
    let appearance_file = data_dir.join(appearance::FILE_NAME);

    let app_service = service.clone();
    let handle = rt.handle().clone();
    let result = eframe::run_native(
        "Newskite",
        native,
        Box::new(move |cc| {
            Ok(Box::new(NewskiteApp::new(
                cc.egui_ctx.clone(),
                app_service,
                handle,
                AppConfig {
                    appearance_file: Some(appearance_file),
                    ..AppConfig::default()
                },
            )))
        }),
    );

    service.shutdown();
    rt.shutdown_timeout(Duration::from_secs(3));

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // The windowing system just failed: a window of our own cannot open now.
            let message = format!("cannot open the window: {e}");
            eprintln!("newskite: {message}");
            native_dialog("Newskite cannot start", &message, true);
            ExitCode::FAILURE
        }
    }
}
