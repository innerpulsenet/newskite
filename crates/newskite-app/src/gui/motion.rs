//! Reduced-motion preference.
//!
//! The brand's motion (the mark's draw-in and beat) stops when the desktop asks for
//! reduced motion. Sources, first match wins:
//!
//! 1. `NEWSKITE_REDUCED_MOTION`: `1`/`true`/`yes`/`on` forces reduced motion, `0`/`false`/
//!    `no`/`off` forces full motion;
//! 2. KDE: `[KDE] AnimationDurationFactor` in `kdeglobals` (0 means animations are off);
//! 3. otherwise full motion.

use std::path::Path;

/// Environment variable that overrides the desktop setting.
pub const ENV: &str = "NEWSKITE_REDUCED_MOTION";

/// Parses an override value; `None` when it is empty or not a recognized word.
pub fn parse_env(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `[KDE] AnimationDurationFactor` from the text of a `kdeglobals` file, if present and
/// numeric.
pub fn kde_animation_factor(ini: &str) -> Option<f32> {
    let mut in_kde = false;
    for raw in ini.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_kde = line == "[KDE]";
            continue;
        }
        if !in_kde {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            // KDE allows localized/immutable suffixes such as `Key[$i]`.
            let key = key.trim();
            let key = key.split('[').next().unwrap_or(key).trim();
            if key == "AnimationDurationFactor" {
                return value.trim().parse::<f32>().ok().filter(|f| f.is_finite());
            }
        }
    }
    None
}

/// Whether reduced motion is wanted, from an override value and a `kdeglobals` text.
pub fn decide(env: Option<&str>, kdeglobals: Option<&str>) -> bool {
    if let Some(forced) = env.and_then(parse_env) {
        return forced;
    }
    kdeglobals
        .and_then(kde_animation_factor)
        .is_some_and(|f| f <= 0.0)
}

/// Reads the environment and the user's `kdeglobals` (missing files mean full motion).
pub fn reduced_motion() -> bool {
    let env = std::env::var(ENV).ok();
    let kde = newskite_theme::kdeglobals_path().and_then(|p| read_small(&p));
    decide(env.as_deref(), kde.as_deref())
}

fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    // kdeglobals is a few KB; refuse anything absurd rather than reading it each poll.
    if meta.len() > 1024 * 1024 {
        return None;
    }
    std::fs::read(path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides() {
        assert_eq!(parse_env("1"), Some(true));
        assert_eq!(parse_env(" TRUE "), Some(true));
        assert_eq!(parse_env("0"), Some(false));
        assert_eq!(parse_env(""), None);
        assert_eq!(parse_env("maybe"), None);
        let kde_off = "[KDE]\nAnimationDurationFactor=0\n";
        assert!(!decide(Some("0"), Some(kde_off)), "env wins");
        assert!(decide(Some("1"), None));
        assert!(
            decide(Some("junk"), Some(kde_off)),
            "unknown env falls through"
        );
    }

    #[test]
    fn kde_factor() {
        let ini = "[General]\nAnimationDurationFactor=0\n[KDE]\nLookAndFeelPackage=x\nAnimationDurationFactor=0.5\n";
        assert_eq!(
            kde_animation_factor(ini),
            Some(0.5),
            "only the [KDE] group counts"
        );
        assert!(!decide(None, Some(ini)));
        assert!(decide(None, Some("[KDE]\nAnimationDurationFactor = 0\n")));
        assert!(decide(None, Some("[KDE]\nAnimationDurationFactor[$i]=0\n")));
        assert!(!decide(None, Some("[KDE]\nAnimationDurationFactor=abc\n")));
        assert!(!decide(None, Some("")));
        assert!(!decide(None, None));
    }
}
