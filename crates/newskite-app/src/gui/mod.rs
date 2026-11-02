//! The egui/eframe desktop window (outline §5–§7), branded with the innerpulse kit.
//!
//! Layout, a quiet list: the sidebar (mark, views, watch folder, settings, disk meter) on
//! the left, the header and job list in the center, the detail pane of the selected job on
//! the right, then the server strip and the status bar. All colors come from theme tokens
//! via [`theme::Palette`]; only [`theme`] converts colors and only it and [`brand_tokens`]
//! hold literal color values.

pub mod about;
pub mod app;
pub mod appearance;
pub mod articles;
pub mod brand;
pub mod brand_tokens;
pub mod center;
pub mod disk;
pub mod docks;
pub mod events;
pub mod fonts;
pub mod icons;
pub mod inspector;
pub mod motion;
pub mod queue;
pub mod serverstrip;
pub mod settings;
pub mod sidebar;
pub mod statusbar;
pub mod theme;
pub mod toast;
pub mod toolbar;
pub mod widgets;

pub use app::{Action, AppConfig, NewskiteApp, Tab, ThemeMode, UiMsg};
pub use appearance::Appearance;
