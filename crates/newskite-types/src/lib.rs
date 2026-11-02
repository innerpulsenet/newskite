//! Shared types for every Newskite crate: settings, server config, engine events and
//! human-readable formatting helpers. This crate has no I/O beyond resolving default
//! directories, so every other crate (and the UI) can depend on it cheaply.

pub mod config;
pub mod events;
pub mod format;
pub mod outcome;

pub use config::{PostProcessSettings, ServerConfig, Settings};
pub use events::EngineEvent;
pub use outcome::{FailureKind, JobOutcome, ServerHealth, StepRecord, StepState};

/// Application name used for data/config directories and window titles.
pub const APP_NAME: &str = "Newskite";
/// Reverse-DNS application id (desktop file, D-Bus, AppStream).
pub const APP_ID: &str = "net.innerpulse.Newskite";
