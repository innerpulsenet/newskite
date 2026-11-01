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
use std::time::Duration;

use eframe::egui::{self, Id};

/// Frame interval while something moves (40 fps).
pub const FRAME: Duration = Duration::from_millis(25);

/// The product's shared beat: a phase (0…1) over `period` seconds that every live
/// element follows, so the masthead, the graph marker, the slot row, the live chip and
/// the queue sweep breathe together. `moving` is false when nothing should move (not
/// active, or reduced motion), and then the phase rests at zero.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Beat {
    /// Phase of the current cycle, 0…1.
    pub phase: f32,
    /// Something moves this frame.
    pub moving: bool,
    /// Motion is allowed at all (not reduced).
    pub animate: bool,
}

impl Beat {
    /// The beat at egui time `now` for a cycle of `period` seconds; it requests the next
    /// frame while moving.
    pub fn new(ctx: &egui::Context, active: bool, reduced_motion: bool, period: f64) -> Self {
        let now = ctx.input(|i| i.time);
        Self::at(now, active, reduced_motion, period).tick(ctx)
    }

    /// The beat at a given time (pure; for tests).
    pub fn at(now: f64, active: bool, reduced_motion: bool, period: f64) -> Self {
        let moving = active && !reduced_motion && now.is_finite() && period > 0.0;
        let phase = if moving {
            (now / period).rem_euclid(1.0) as f32
        } else {
            0.0
        };
        Self {
            phase,
            moving,
            animate: !reduced_motion,
        }
    }

    fn tick(self, ctx: &egui::Context) -> Self {
        if self.moving {
            ctx.request_repaint_after(FRAME);
        }
        self
    }

    /// A soft 0…1 breath over the cycle (1 at the start, 0 halfway, 1 again).
    pub fn breath(self) -> f32 {
        0.5 + 0.5 * (self.phase * std::f32::consts::TAU).cos()
    }
}

/// A fade from 0 to 1 over `secs` that starts the first time `id` is seen: 0 on that
/// frame, then eased toward 1. Under reduced motion it is 1 at once. Pair with
/// [`fade_reset`] when the thing closes, so it fades in again next time.
pub fn fade_in(ctx: &egui::Context, id: Id, secs: f32, reduced_motion: bool) -> f32 {
    if reduced_motion {
        return 1.0;
    }
    let seen = ctx.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    if !seen {
        ctx.data_mut(|d| d.insert_temp(id, true));
        // Seeds the animation at 0; egui snaps on the first call.
        ctx.animate_bool_with_time(id.with("fade"), false, secs);
        ctx.request_repaint();
        return 0.0;
    }
    ctx.animate_bool_with_time(id.with("fade"), true, secs)
}

/// Forgets a [`fade_in`] so it plays again.
pub fn fade_reset(ctx: &egui::Context, id: Id) {
    ctx.data_mut(|d| {
        d.remove_temp::<bool>(id);
    });
}

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
    fn beat_phase_and_rest() {
        let b = Beat::at(10.3, true, false, 1.2);
        assert!(b.moving && b.animate);
        assert!((0.0..1.0).contains(&b.phase));
        assert!((b.phase - ((10.3_f64 / 1.2).rem_euclid(1.0) as f32)).abs() < 1e-6);
        let rest = Beat::at(10.3, false, false, 1.2);
        assert!(!rest.moving && rest.phase == 0.0 && rest.breath() == 1.0);
        let reduced = Beat::at(10.3, true, true, 1.2);
        assert!(!reduced.moving && !reduced.animate);
        assert!(!Beat::at(f64::NAN, true, false, 1.2).moving);
        assert!(!Beat::at(1.0, true, false, 0.0).moving);
        for i in 0..20 {
            let b = Beat::at(i as f64 * 0.07, true, false, 1.0);
            assert!((0.0..=1.0).contains(&b.breath()));
        }
    }

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
