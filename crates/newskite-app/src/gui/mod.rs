//! The egui/eframe desktop window (outline §5–§7), branded with the innerpulse kit.
//!
//! Layout: toolbar with the masthead (top), bookmarks sidebar (left), tabbed center
//! area, four bottom docks and a status bar. All colors come from theme tokens via
//! [`theme::Palette`]; only [`theme`] converts colors and only it and [`brand_tokens`]
//! hold literal color values.

pub mod about;
pub mod app;
pub mod appearance;
pub mod brand;
pub mod brand_tokens;
pub mod docks;
pub mod events;
pub mod fonts;
pub mod icons;
pub mod motion;
pub mod queue;
pub mod settings;
pub mod sidebar;
pub mod statusbar;
pub mod theme;
pub mod toolbar;
pub mod widgets;

pub use app::{Action, AppConfig, NewskiteApp, Tab, ThemeMode, UiMsg};
pub use appearance::Appearance;
