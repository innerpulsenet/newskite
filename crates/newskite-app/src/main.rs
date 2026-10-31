//! `newskite`: the desktop Usenet (NZB) client.
//!
//! * `newskite --headless ...` runs the headless CLI (`newskite_cli`).
//! * Otherwise the window opens; positional `.nzb` paths, `file://` and local `nzb://`
//!   URIs are queued at launch.

use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use eframe::egui;
use newskite_app::launch::{self, LaunchItem, Mode};
use newskite_app::{AppConfig, NewskiteApp};
use newskite_service::{LogLevel, Service, ServiceOptions};
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

/// Shows a fatal start-up error natively (the window cannot open) and on stderr.
fn fatal(message: &str) -> ExitCode {
    eprintln!("newskite: {message}");
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Newskite cannot start")
        .set_description(message)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
    ExitCode::FAILURE
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

    let opts = ServiceOptions {
        data_dir: default_data_dir(),
        allow_shutdown: true,
    };
    let service = match Service::open(opts, rt.handle().clone()) {
        Ok(s) => s,
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

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(newskite_types::APP_ID)
            .with_title("Newskite")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };

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
                AppConfig::default(),
            )))
        }),
    );

    service.shutdown();
    rt.shutdown_timeout(Duration::from_secs(3));

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fatal(&format!("cannot open the window: {e}")),
    }
}
