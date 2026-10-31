//! The egui/eframe desktop window (outline §5–§7).
//!
//! Layout: toolbar (top), bookmarks sidebar (left), tabbed center area, four bottom
//! docks and a status bar. All colors come from desktop theme tokens via
//! [`theme::Palette`]; only [`theme`] converts colors.

pub mod app;
pub mod docks;
pub mod queue;
pub mod settings;
pub mod sidebar;
pub mod statusbar;
pub mod theme;
pub mod toolbar;
pub mod widgets;

pub use app::{Action, AppConfig, NewskiteApp, Tab, ThemeMode, UiMsg};
