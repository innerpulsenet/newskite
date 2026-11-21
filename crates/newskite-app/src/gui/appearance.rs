//! The Appearance choice (Settings › Appearance): follow the desktop (the default, outline
//! F10) or wear one of the two innerpulse themes.
//!
//! `Settings` belongs to `newskite-types` and is shared with the headless CLI, so the
//! choice is a GUI preference stored on its own: a small JSON document,
//! `<data dir>/appearance.json`, next to the database (`{"theme": "follow-desktop"}`,
//! `"innerpulse-ink"` or `"innerpulse-paper"`). A missing or unreadable file means
//! "follow desktop".

use std::path::Path;

use newskite_theme::Theme;

use super::brand_tokens;

/// File name of the appearance document inside the data directory.
pub const FILE_NAME: &str = "appearance.json";

/// Where the window's colors come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Appearance {
    /// The desktop color scheme (kdeglobals / portal / fallback), live.
    #[default]
    FollowDesktop,
    /// innerpulse ink: the warm near-black ground with the violet accent.
    Ink,
    /// innerpulse paper: the warm off-white ground with the deep violet.
    Paper,
}

impl Appearance {
    /// Every choice, in menu order.
    pub const ALL: [Appearance; 3] = [
        Appearance::FollowDesktop,
        Appearance::Ink,
        Appearance::Paper,
    ];

    /// The label shown in Settings.
    pub fn label(self) -> &'static str {
        match self {
            Appearance::FollowDesktop => "Follow desktop",
            Appearance::Ink => "innerpulse ink",
            Appearance::Paper => "innerpulse paper",
        }
    }

    /// The stored key.
    pub fn key(self) -> &'static str {
        match self {
            Appearance::FollowDesktop => "follow-desktop",
            Appearance::Ink => "innerpulse-ink",
            Appearance::Paper => "innerpulse-paper",
        }
    }

    /// Parses a stored key.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.key() == key)
    }

    /// The theme to wear given the desktop's current one.
    pub fn resolve(self, desktop: &Theme) -> Theme {
        match self {
            Appearance::FollowDesktop => desktop.clone(),
            Appearance::Ink => brand_tokens::ink(),
            Appearance::Paper => brand_tokens::paper(),
        }
    }

    /// The JSON document for this choice.
    pub fn to_json(self) -> String {
        serde_json::json!({ "theme": self.key() }).to_string()
    }

    /// Parses a JSON document; `None` if it is not one this version understands.
    pub fn from_json(text: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        Self::from_key(v.get("theme")?.as_str()?)
    }
}

/// Reads the choice from `path` (follow desktop when missing or unreadable).
pub fn load(path: &Path) -> Appearance {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| Appearance::from_json(&t))
        .unwrap_or_default()
}

/// Whether the document at `path` asks for reduced motion (`"reduce_motion": true`;
/// `false` when missing or unreadable). The desktop's own preference is read elsewhere
/// (see [`super::motion`]); either one is enough to still the window.
pub fn load_reduce_motion(path: &Path) -> bool {
    std::fs::read_to_string(path).ok().is_some_and(|t| reduce_motion_from_json(&t))
}

fn reduce_motion_from_json(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("reduce_motion")?.as_bool())
        .unwrap_or(false)
}

/// The JSON document for a theme choice and the Reduce motion switch.
pub fn document(a: Appearance, reduce_motion: bool) -> String {
    if reduce_motion {
        serde_json::json!({ "theme": a.key(), "reduce_motion": true }).to_string()
    } else {
        a.to_json()
    }
}

/// Writes the choices to `path` atomically (temp file + rename).
pub fn save(path: &Path, a: Appearance, reduce_motion: bool) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, document(a, reduce_motion))?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_defaults() {
        for a in Appearance::ALL {
            assert_eq!(Appearance::from_json(&a.to_json()), Some(a));
        }
        assert_eq!(Appearance::from_json("{}"), None);
        assert_eq!(Appearance::from_json("{\"theme\":\"neon\"}"), None);
        assert_eq!(Appearance::from_json("not json"), None);
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("sub").join(FILE_NAME);
        assert_eq!(load(&p), Appearance::FollowDesktop, "missing file");
        assert!(!load_reduce_motion(&p), "missing file");
        save(&p, Appearance::Paper, false).expect("save");
        assert_eq!(load(&p), Appearance::Paper);
        assert!(!load_reduce_motion(&p));
        save(&p, Appearance::Ink, true).expect("save");
        assert_eq!(load(&p), Appearance::Ink);
        assert!(load_reduce_motion(&p));
        std::fs::write(&p, "garbage").expect("write");
        assert_eq!(load(&p), Appearance::FollowDesktop, "unreadable file");
    }

    #[test]
    fn resolves_to_the_brand_sets() {
        let desktop = newskite_theme::fallback(false);
        assert_eq!(Appearance::FollowDesktop.resolve(&desktop), desktop);
        assert!(Appearance::Ink.resolve(&desktop).dark);
        assert!(!Appearance::Paper.resolve(&desktop).dark);
        assert_eq!(
            Appearance::Ink.resolve(&desktop).scheme_name.as_deref(),
            Some(brand_tokens::INK_NAME)
        );
    }
}
