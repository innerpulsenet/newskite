//! Number formatting used everywhere a figure reaches a human. Never yields NaN/inf text.

/// `0 B`, `512 B`, `1.4 KB`, `2.0 MB`, `3.21 GB` (binary units, 1 KB = 1024 B).
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else if v >= 10.0 || i < 3 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// Throughput: `0 B/s`, `1.4 MB/s`.
pub fn speed(bytes_per_sec: u64) -> String {
    format!("{}/s", bytes(bytes_per_sec))
}

/// Integer percentage 0–100 of `done/total`; 0 when total is 0. Clamped.
pub fn percent(done: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    ((done.min(total) as u128 * 100) / total as u128) as u8
}

/// Fraction 0.0–1.0 for progress bars; 0.0 when total is 0. Always finite.
pub fn fraction(done: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    (done.min(total) as f64 / total as f64) as f32
}

/// `0:42`, `12:05`, `1:02:03`.
pub fn duration_secs(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Compact human duration: `45s`, `8m 12s`, `2h 05m`, `3d 4h`.
pub fn duration_short(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60, secs % 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// How long ago `then` was, relative to `now` (unix seconds): `just now`, `5 min ago`,
/// `3 h ago`, `yesterday`, `4 days ago`.
pub fn ago(then: i64, now: i64) -> String {
    let d = (now - then).max(0);
    match d {
        0..=44 => "just now".into(),
        45..=3599 => format!("{} min ago", (d + 30) / 60),
        3600..=86_399 => format!("{} h ago", d / 3600),
        86_400..=172_799 => "yesterday".into(),
        _ => format!("{} days ago", d / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_durations_and_ago() {
        assert_eq!(duration_short(45), "45s");
        assert_eq!(duration_short(492), "8m 12s");
        assert_eq!(duration_short(7500), "2h 05m");
        assert_eq!(duration_short(3 * 86400 + 4 * 3600), "3d 4h");
        assert_eq!(ago(100, 110), "just now");
        assert_eq!(ago(0, 300), "5 min ago");
        assert_eq!(ago(0, 3 * 3600 + 5), "3 h ago");
        assert_eq!(ago(0, 90_000), "yesterday");
        assert_eq!(ago(0, 5 * 86_400), "5 days ago");
    }

    #[test]
    fn formats() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.0 KB");
        assert_eq!(bytes(1_468_006), "1.4 MB");
        assert_eq!(speed(0), "0 B/s");
        assert_eq!(percent(5, 0), 0);
        assert_eq!(percent(50, 100), 50);
        assert_eq!(percent(500, 100), 100);
        assert_eq!(fraction(1, 0), 0.0);
        assert_eq!(duration_secs(3723), "1:02:03");
        assert_eq!(duration_secs(42), "0:42");
    }
}
