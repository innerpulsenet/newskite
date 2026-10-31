//! Integration tests for ThemeWatcher against a temporary kdeglobals file.

use std::path::PathBuf;
use std::time::Duration;

use newskite_theme::{fallback, ThemeSource, ThemeWatcher};

const DARK: &str = include_str!("fixtures/breeze-dark.kdeglobals");
const LIGHT: &str = include_str!("fixtures/breeze-light.kdeglobals");

struct TempFile(PathBuf);

impl TempFile {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        Self(std::env::temp_dir().join(format!(
            "newskite-theme-{tag}-{}-{nanos}",
            std::process::id()
        )))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn detects_changes_and_ignores_no_ops() {
    let f = TempFile::new("watch");
    let mut w = ThemeWatcher::with_path(&f.0, Some(false));
    w.set_min_interval(Duration::ZERO);
    assert_eq!(w.current(), &fallback(false));
    assert_eq!(w.poll_changed(), None, "nothing changed yet");

    std::fs::write(&f.0, DARK).unwrap();
    let t = w.poll_changed().expect("dark scheme detected");
    assert!(t.dark);
    assert_eq!(t.source, ThemeSource::KdeGlobals);
    assert_eq!(w.poll_changed(), None, "unchanged file");

    // Rewritten with different bytes but identical colors: no change reported.
    std::fs::write(&f.0, format!("{DARK}\n# touched\n")).unwrap();
    assert_eq!(w.poll_changed(), None);

    std::fs::write(&f.0, LIGHT).unwrap();
    let t = w.poll_changed().expect("light scheme detected");
    assert!(!t.dark);
    assert_eq!(w.current(), &t);

    std::fs::remove_file(&f.0).unwrap();
    let t = w.poll_changed().expect("back to fallback");
    assert_eq!(t, fallback(false));
}

#[test]
fn rate_limited() {
    let f = TempFile::new("rate");
    let mut w = ThemeWatcher::with_path(&f.0, None);
    w.set_min_interval(Duration::from_secs(3600));
    assert_eq!(w.poll_changed(), None); // first poll consumes the window
    std::fs::write(&f.0, DARK).unwrap();
    assert_eq!(w.poll_changed(), None, "throttled despite the change");
    w.set_min_interval(Duration::ZERO);
    assert!(
        w.poll_changed().is_some(),
        "picked up once the throttle allows"
    );
}

#[test]
fn hint_change_re_resolves() {
    let f = TempFile::new("hint");
    let mut w = ThemeWatcher::with_path(&f.0, Some(false));
    w.set_min_interval(Duration::ZERO);
    assert!(!w.current().dark);
    w.set_prefers_dark_hint(Some(true));
    let t = w.poll_changed().expect("hint flips fallback");
    assert!(t.dark);
    assert_eq!(w.poll_changed(), None);
}

#[test]
fn default_interval_is_one_second() {
    let f = TempFile::new("default");
    let mut w = ThemeWatcher::with_path(&f.0, None);
    assert_eq!(w.poll_changed(), None);
    std::fs::write(&f.0, DARK).unwrap();
    assert_eq!(w.poll_changed(), None, "within the 1 s window");
    std::thread::sleep(Duration::from_millis(1100));
    assert!(w.poll_changed().is_some());
}
