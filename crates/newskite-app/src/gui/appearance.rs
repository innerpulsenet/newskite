//! The Appearance choice (Settings › Appearance): wear one of the innerpulse themes
//! (carbon, the studio's ground, until the user chooses otherwise) or follow the desktop.
//!
//! `Settings` belongs to `newskite-types` and is shared with the headless CLI, so the
//! choice is a GUI preference stored on its own: a small JSON document,
//! `<data dir>/appearance.json`, next to the database (`{"theme": "innerpulse-ink"}`,
//! `"innerpulse-aluminum"`, `"innerpulse-warm"`, `"innerpulse-paper"` or
//! `"follow-desktop"`). A missing or
//! unreadable file means nothing was chosen: carbon, on Linux and Windows alike.

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
    /// Brushed steel, the dark ground (the kit's ink, carbon): the brushed plate on cool
    /// blacks, steel text and the violet accent. Stored as `innerpulse-ink`.
    Ink,
    /// Bare aluminum, the light ground of the same panel: a pale brushed plate, pale
    /// wells and caps written in ink, with the same dark screens, lamps and lit keys.
    Aluminum,
    /// The kit's optional warm ink: the same materials on the brown-black ground.
    Warm,
    /// innerpulse paper: the warm off-white ground with the deep violet.
    Paper,
}

impl Appearance {
    /// Every choice, in menu order.
    pub const ALL: [Appearance; 5] = [
        Appearance::Ink,
        Appearance::Aluminum,
        Appearance::Warm,
        Appearance::Paper,
        Appearance::FollowDesktop,
    ];

    /// What a window wears until its user chooses: carbon.
    pub const UNCHOSEN: Appearance = Appearance::Ink;

    /// The label shown in Settings.
    pub fn label(self) -> &'static str {
        match self {
            Appearance::FollowDesktop => "Follow desktop",
            Appearance::Ink => "Steel",
            Appearance::Aluminum => "Aluminum",
            Appearance::Warm => "Warm ink",
            Appearance::Paper => "Paper",
        }
    }

    /// The stored key.
    pub fn key(self) -> &'static str {
        match self {
            Appearance::FollowDesktop => "follow-desktop",
            Appearance::Ink => "innerpulse-ink",
            Appearance::Aluminum => "innerpulse-aluminum",
            Appearance::Warm => "innerpulse-warm",
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
            Appearance::Aluminum => brand_tokens::aluminum(),
            Appearance::Warm => brand_tokens::warm(),
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

/// Reads the choice from `path` ([`Appearance::UNCHOSEN`] when missing or unreadable).
pub fn load(path: &Path) -> Appearance {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| Appearance::from_json(&t))
        .unwrap_or(Appearance::UNCHOSEN)
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
        assert_eq!(load(&p), Appearance::Ink, "missing file: carbon");
        assert!(!load_reduce_motion(&p), "missing file");
        save(&p, Appearance::FollowDesktop, false).expect("save");
        assert_eq!(load(&p), Appearance::FollowDesktop, "a stored choice is kept");
        save(&p, Appearance::Paper, false).expect("save");
        assert_eq!(load(&p), Appearance::Paper);
        assert!(!load_reduce_motion(&p));
        save(&p, Appearance::Ink, true).expect("save");
        assert_eq!(load(&p), Appearance::Ink);
        assert!(load_reduce_motion(&p));
        std::fs::write(&p, "garbage").expect("write");
        assert_eq!(load(&p), Appearance::Ink, "unreadable file");
    }

    #[test]
    fn resolves_to_the_brand_sets() {
        let desktop = newskite_theme::fallback(false);
        assert_eq!(Appearance::FollowDesktop.resolve(&desktop), desktop);
        assert!(Appearance::Ink.resolve(&desktop).dark);
        assert!(Appearance::Warm.resolve(&desktop).dark);
        assert_ne!(Appearance::Warm.resolve(&desktop), Appearance::Ink.resolve(&desktop));
        assert!(!Appearance::Paper.resolve(&desktop).dark);
        assert_eq!(
            Appearance::Ink.resolve(&desktop).scheme_name.as_deref(),
            Some(brand_tokens::INK_NAME)
        );
    }
}
