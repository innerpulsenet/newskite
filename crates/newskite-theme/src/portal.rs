//! XDG desktop portal reader for `org.freedesktop.appearance` (Linux, `portal` feature).
//!
//! The query runs on a short-lived helper thread and the caller waits at most
//! [`PORTAL_TIMEOUT`], so a missing or wedged session bus can never stall the UI.

use crate::Rgb;
#[cfg(all(target_os = "linux", feature = "portal"))]
use std::time::Duration;

/// Upper bound on how long [`query`] may block the caller.
#[cfg(all(target_os = "linux", feature = "portal"))]
pub(crate) const PORTAL_TIMEOUT: Duration = Duration::from_millis(300);

/// What the portal reported. Either field may be absent.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Appearance {
    /// `Some(true)` = prefer dark, `Some(false)` = prefer light, `None` = no preference.
    pub prefers_dark: Option<bool>,
    /// The user's accent color, if the portal exposes one.
    pub accent: Option<Rgb>,
}

impl Appearance {
    /// True when the portal told us nothing useful.
    pub(crate) fn is_empty(&self) -> bool {
        self.prefers_dark.is_none() && self.accent.is_none()
    }
}

#[cfg_attr(not(all(target_os = "linux", feature = "portal")), allow(dead_code))]
/// Maps the portal `color-scheme` value: 1 = prefer dark, 2 = prefer light, else none.
pub(crate) fn scheme_to_dark(v: u32) -> Option<bool> {
    match v {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

#[cfg_attr(not(all(target_os = "linux", feature = "portal")), allow(dead_code))]
/// Converts the portal `accent-color` `(ddd)` triple. Each channel must be within
/// `0.0..=1.0` (portals use out-of-range values to mean "unset").
pub(crate) fn accent_from_triple(r: f64, g: f64, b: f64) -> Option<Rgb> {
    let ch = |v: f64| -> Option<u8> {
        if v.is_finite() && (0.0..=1.0).contains(&v) {
            // In 0..=255 after scaling, so the cast cannot truncate.
            Some((v * 255.0).round() as u8)
        } else {
            None
        }
    };
    Some(Rgb::new(ch(r)?, ch(g)?, ch(b)?))
}

/// Queries the portal. Returns `None` if the bus or portal is unavailable, the call
/// errors or the timeout elapses.
#[cfg(all(target_os = "linux", feature = "portal"))]
pub(crate) fn query() -> Option<Appearance> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("newskite-theme-portal".into())
        .spawn(move || {
            // The receiver may have given up already; nothing to do then.
            let _ = tx.send(dbus::query_blocking());
        });
    if let Err(e) = spawned {
        tracing::debug!("could not spawn portal thread: {e}");
        return None;
    }
    match rx.recv_timeout(PORTAL_TIMEOUT) {
        Ok(result) => result,
        Err(_) => {
            tracing::debug!("desktop portal did not answer within {PORTAL_TIMEOUT:?}");
            None
        }
    }
}

/// Portal querying is unavailable on this platform or build.
#[cfg(not(all(target_os = "linux", feature = "portal")))]
pub(crate) fn query() -> Option<Appearance> {
    None
}

#[cfg(all(target_os = "linux", feature = "portal"))]
mod dbus {
    use super::{accent_from_triple, scheme_to_dark, Appearance, PORTAL_TIMEOUT};
    use zbus::blocking::Connection;
    use zbus::zvariant::{OwnedValue, Value};

    const DEST: &str = "org.freedesktop.portal.Desktop";
    const PATH: &str = "/org/freedesktop/portal/desktop";
    const IFACE: &str = "org.freedesktop.portal.Settings";
    const NAMESPACE: &str = "org.freedesktop.appearance";

    pub(super) fn query_blocking() -> Option<Appearance> {
        let conn = match zbus::blocking::connection::Builder::session()
            .and_then(|b| b.method_timeout(PORTAL_TIMEOUT).build())
        {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("no D-Bus session bus: {e}");
                return None;
            }
        };
        let scheme = read(&conn, "color-scheme");
        let accent = read(&conn, "accent-color");
        if scheme.is_none() && accent.is_none() {
            return None;
        }
        Some(Appearance {
            prefers_dark: scheme.as_ref().and_then(value_u32).and_then(scheme_to_dark),
            accent: accent.as_ref().and_then(value_accent),
        })
    }

    /// `ReadOne` (portal v2+), falling back to the deprecated `Read` for older portals.
    fn read(conn: &Connection, key: &str) -> Option<OwnedValue> {
        let body = (NAMESPACE, key);
        let msg = match conn.call_method(Some(DEST), PATH, Some(IFACE), "ReadOne", &body) {
            Ok(m) => m,
            Err(e) => {
                tracing::trace!("portal ReadOne({key}) failed: {e}; trying Read");
                conn.call_method(Some(DEST), PATH, Some(IFACE), "Read", &body)
                    .map_err(|e| tracing::trace!("portal Read({key}) failed: {e}"))
                    .ok()?
            }
        };
        msg.body().deserialize::<OwnedValue>().ok()
    }

    /// Unwraps the extra variant layer old `Read` implementations add.
    fn peel<'a>(v: &'a Value<'a>) -> &'a Value<'a> {
        let mut cur = v;
        while let Value::Value(inner) = cur {
            cur = inner;
        }
        cur
    }

    fn value_u32(v: &OwnedValue) -> Option<u32> {
        match peel(v) {
            Value::U32(n) => Some(*n),
            Value::I32(n) => u32::try_from(*n).ok(),
            Value::U8(n) => Some(u32::from(*n)),
            _ => None,
        }
    }

    fn value_accent(v: &OwnedValue) -> Option<crate::Rgb> {
        let Value::Structure(s) = peel(v) else {
            return None;
        };
        let f: Vec<f64> = s
            .fields()
            .iter()
            .map(|f| match peel(f) {
                Value::F64(x) => Some(*x),
                _ => None,
            })
            .collect::<Option<_>>()?;
        match f.as_slice() {
            [r, g, b] => accent_from_triple(*r, *g, *b),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_mapping() {
        assert_eq!(scheme_to_dark(0), None);
        assert_eq!(scheme_to_dark(1), Some(true));
        assert_eq!(scheme_to_dark(2), Some(false));
        assert_eq!(scheme_to_dark(9), None);
    }

    #[test]
    fn accent_triple() {
        assert_eq!(
            accent_from_triple(0.0, 0.5, 1.0),
            Some(Rgb::new(0, 128, 255))
        );
        assert_eq!(accent_from_triple(-1.0, 0.5, 1.0), None);
        assert_eq!(accent_from_triple(0.2, 1.5, 0.3), None);
        assert_eq!(accent_from_triple(f64::NAN, 0.5, 0.3), None);
    }

    #[cfg(all(target_os = "linux", feature = "portal"))]
    #[test]
    #[ignore = "talks to the host session bus; run manually"]
    fn query_live_portal() {
        let start = std::time::Instant::now();
        let got = query();
        println!("portal: {got:?} in {:?}", start.elapsed());
        assert!(start.elapsed() < PORTAL_TIMEOUT + Duration::from_millis(100));
    }

    #[test]
    fn empty_appearance() {
        assert!(Appearance::default().is_empty());
        assert!(!Appearance {
            prefers_dark: Some(false),
            accent: None
        }
        .is_empty());
    }
}
