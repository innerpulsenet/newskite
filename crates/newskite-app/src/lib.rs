//! Newskite desktop application: the egui GUI and launch-argument handling. The
//! `newskite` binary (`src/main.rs`) is a thin wrapper around this library so the GUI
//! can be exercised headlessly in tests.

pub mod desktop;
pub mod gui;
pub mod launch;

pub use gui::{AppConfig, NewskiteApp, ThemeMode};
