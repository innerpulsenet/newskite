//! [`ThemeWatcher`]: cheap polling for desktop scheme changes (re-theme without restart).

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use crate::{kdeglobals_path, resolve_detailed, resolve_from, Sources, Theme, ThemeSource};

/// Default minimum time between two real checks in [`ThemeWatcher::poll_changed`].
const DEFAULT_MIN_INTERVAL: Duration = Duration::from_secs(1);
/// How often to re-ask the desktop portal when kdeglobals is not the active source
/// (the portal has no file to stat, so it is re-queried on this slower cadence).
const PORTAL_REFRESH: Duration = Duration::from_secs(10);
/// Consecutive unanswered portal refreshes before a portal theme gives way to the
/// fallback palette. A portal that is still activating (login) or slow on a loaded host
/// misses a refresh now and then; flipping the window to the fallback and back each time
/// would flicker the accent and scheme.
const PORTAL_MISS_LIMIT: u32 = 3;

/// A resolution running on a helper thread: the theme and whether the portal missed.
type Pending = Receiver<(Theme, bool)>;

/// Identity of the kdeglobals file at one point in time. `None` = file absent.
type Stamp = Option<(Option<SystemTime>, u64)>;

fn stamp(path: Option<&PathBuf>) -> Stamp {
    let meta = std::fs::metadata(path?).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

/// Watches the desktop theme. Call [`poll_changed`](Self::poll_changed) from the UI
/// loop as often as you like; it does real work at most once per second, and then
/// only a `stat` of kdeglobals unless the file changed (or the portal is due a
/// refresh when kdeglobals is not in use).
///
/// When the portal is consulted, re-resolution runs on a helper thread (a D-Bus round
/// trip must not stall the caller's frame); its result is returned by a later
/// `poll_changed` call.
#[derive(Debug)]
pub struct ThemeWatcher {
    sources: Sources,
    stamp: Stamp,
    theme: Theme,
    last_poll: Option<Instant>,
    last_resolve: Instant,
    min_interval: Duration,
    force: bool,
    pending: Option<Pending>,
    portal_misses: u32,
}

impl Default for ThemeWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ThemeWatcher {
    /// Resolves the current theme immediately (as [`crate::resolve`]) and starts watching.
    pub fn new() -> Self {
        Self::from_sources(Sources {
            kdeglobals: kdeglobals_path(),
            portal: true,
            prefers_dark: None,
        })
    }

    /// Like [`new`](Self::new) but with the caller's light/dark guess (see
    /// [`crate::resolve_with_hint`]).
    pub fn with_hint(prefers_dark: Option<bool>) -> Self {
        Self::from_sources(Sources {
            kdeglobals: kdeglobals_path(),
            portal: true,
            prefers_dark,
        })
    }

    /// Watches a specific kdeglobals-format file and never consults the portal. When the
    /// file is missing or unusable the theme is `fallback(prefers_dark.unwrap_or(false))`.
    pub fn with_path(path: impl Into<PathBuf>, prefers_dark: Option<bool>) -> Self {
        Self::from_sources(Sources {
            kdeglobals: Some(path.into()),
            portal: false,
            prefers_dark,
        })
    }

    fn from_sources(sources: Sources) -> Self {
        let stamp = stamp(sources.kdeglobals.as_ref());
        let theme = resolve_from(&sources);
        Self {
            sources,
            stamp,
            theme,
            last_poll: None,
            last_resolve: Instant::now(),
            min_interval: DEFAULT_MIN_INTERVAL,
            force: false,
            pending: None,
            portal_misses: 0,
        }
    }

    /// The most recently resolved theme.
    pub fn current(&self) -> &Theme {
        &self.theme
    }

    /// Changes the minimum interval between real checks (default 1 s). Mainly for tests.
    pub fn set_min_interval(&mut self, interval: Duration) {
        self.min_interval = interval;
    }

    /// Updates the caller's light/dark guess; the next eligible poll re-resolves.
    pub fn set_prefers_dark_hint(&mut self, prefers_dark: Option<bool>) {
        if self.sources.prefers_dark != prefers_dark {
            self.sources.prefers_dark = prefers_dark;
            self.force = true;
        }
    }

    /// Returns `Some(theme)` only when the resolved tokens (or light/dark, source or
    /// scheme name) actually changed since the last returned/initial theme.
    pub fn poll_changed(&mut self) -> Option<Theme> {
        // A background resolution finished: settle it before starting another.
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok((theme, portal_missed)) => {
                    self.pending = None;
                    return self.accept(theme, portal_missed);
                }
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => self.pending = None,
            }
        }

        let now = Instant::now();
        if let Some(last) = self.last_poll {
            if now.duration_since(last) < self.min_interval {
                return None;
            }
        }
        self.last_poll = Some(now);

        let new_stamp = stamp(self.sources.kdeglobals.as_ref());
        let file_changed = new_stamp != self.stamp;
        let portal_due = self.sources.portal
            && self.theme.source != ThemeSource::KdeGlobals
            && now.duration_since(self.last_resolve) >= PORTAL_REFRESH;
        if !(file_changed || portal_due || self.force) {
            return None;
        }
        // Stamp before reading: a write racing with the read is caught next poll.
        self.stamp = new_stamp;
        self.force = false;
        self.last_resolve = now;

        if self.sources.portal {
            let (tx, rx) = mpsc::sync_channel(1);
            let sources = self.sources.clone();
            let spawned = std::thread::Builder::new()
                .name("newskite-theme-resolve".into())
                .spawn(move || {
                    let _ = tx.send(resolve_detailed(&sources));
                });
            match spawned {
                Ok(_) => {
                    self.pending = Some(rx);
                    return None;
                }
                Err(e) => tracing::debug!("could not spawn the theme thread: {e}"),
            }
        }
        let (theme, portal_missed) = resolve_detailed(&self.sources);
        self.accept(theme, portal_missed)
    }

    /// Adopts a freshly resolved theme, except that a portal theme survives a few
    /// unanswered refreshes (see [`PORTAL_MISS_LIMIT`]).
    fn accept(&mut self, theme: Theme, portal_missed: bool) -> Option<Theme> {
        if portal_missed && self.theme.source == ThemeSource::Portal {
            self.portal_misses += 1;
            if self.portal_misses < PORTAL_MISS_LIMIT {
                tracing::debug!(
                    misses = self.portal_misses,
                    "portal did not answer; keeping its theme"
                );
                return None;
            }
        }
        self.portal_misses = 0;
        if theme == self.theme {
            return None;
        }
        tracing::debug!(source = ?theme.source, dark = theme.dark, "desktop theme changed");
        self.theme = theme.clone();
        Some(theme)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn portal_theme() -> Theme {
        let mut t = crate::fallback(true);
        t.source = ThemeSource::Portal;
        t.tokens.accent = crate::Rgb::new(0x9b, 0x59, 0xd0);
        t
    }

    #[test]
    fn a_late_portal_keeps_its_theme_for_a_while() {
        let mut w = ThemeWatcher::with_path("/nonexistent/newskite-kdeglobals", Some(true));
        w.theme = portal_theme();
        let fallback = crate::fallback(true);
        for _ in 1..PORTAL_MISS_LIMIT {
            assert_eq!(
                w.accept(fallback.clone(), true),
                None,
                "one late reply never flips"
            );
        }
        assert_eq!(w.theme, portal_theme());
        // The portal answering again resets the count.
        assert_eq!(w.accept(portal_theme(), false), None);
        assert_eq!(w.portal_misses, 0);
        for _ in 1..PORTAL_MISS_LIMIT {
            assert_eq!(w.accept(fallback.clone(), true), None);
        }
        // Gone for good: the fallback takes over.
        assert_eq!(w.accept(fallback.clone(), true), Some(fallback.clone()));
        assert_eq!(w.theme.source, ThemeSource::Fallback);
    }

    #[test]
    fn a_real_change_is_adopted_at_once() {
        let mut w = ThemeWatcher::with_path("/nonexistent/newskite-kdeglobals", Some(true));
        w.theme = portal_theme();
        let light = crate::fallback(false);
        assert_eq!(w.accept(light.clone(), false), Some(light));
    }
}
