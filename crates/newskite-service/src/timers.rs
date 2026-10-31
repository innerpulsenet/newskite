//! Background timers: the 1 Hz throughput sampler and the watch-folder scanner.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use newskite_store::ThroughputSample;
use tokio::time::MissedTickBehavior;

use crate::{lock, now_secs, runner, Inner, LogLevel, ServiceError, ServiceEvent, SAMPLE_RETENTION_SECS};

/// Sampler ticks between two prunes of old throughput samples (one hour).
const PRUNE_EVERY_TICKS: u64 = 3600;

/// Starts the 1 Hz throughput sampler. It holds only a weak reference to the service and
/// stops with [`crate::Service::shutdown`] or when the service is dropped.
pub(crate) fn spawn_sampler(inner: &Arc<Inner>) {
    let weak = Arc::downgrade(inner);
    let token = inner.shutdown.clone();
    let progress = inner.progress.clone();
    inner.rt.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await; // the first tick completes immediately
        let mut last = progress.bytes_down();
        let mut ticks: u64 = 0;
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = interval.tick() => {}
            }
            let Some(inner) = weak.upgrade() else { break };
            let now = progress.bytes_down();
            let delta = now.saturating_sub(last);
            last = now;
            sample(&inner, delta, ticks.is_multiple_of(PRUNE_EVERY_TICKS));
            ticks = ticks.wrapping_add(1);
        }
    });
}

/// One sampler tick: publish the speed, store a sample when bytes moved or a run is
/// active, add to the lifetime counter, and prune old samples when `prune` is set.
fn sample(inner: &Inner, delta: u64, prune: bool) {
    inner.speed_bps.store(delta, Ordering::Relaxed);
    inner.emit(ServiceEvent::Throughput { bytes_per_sec: delta });
    let running = inner.lock_state().running;
    let ts = now_secs();
    let mut result = Ok(());
    if delta > 0 || running {
        result = inner.store.add_sample(&ThroughputSample { ts, bytes_up: 0, bytes_down: delta });
    }
    if delta > 0 && result.is_ok() {
        result = inner.store.add_downloaded_bytes(delta);
    }
    if prune && result.is_ok() {
        result = inner.store.prune_samples_before(ts.saturating_sub(SAMPLE_RETENTION_SECS)).map(|_| ());
    }
    match result {
        Ok(()) => inner.sampler_error_logged.store(false, Ordering::Relaxed),
        Err(e) => {
            // Log once per failure streak; the sampler runs every second.
            if !inner.sampler_error_logged.swap(true, Ordering::Relaxed) {
                inner.log(LogLevel::Warn, format!("could not store throughput: {e}"));
            }
        }
    }
}

/// Starts the watch-folder timer. Settings are re-read every second; while
/// `watch_enabled` and `watch_interval_secs > 0`, the folder is scanned right away and then
/// every interval.
pub(crate) fn spawn_watch_timer(inner: &Arc<Inner>) {
    let weak = Arc::downgrade(inner);
    let token = inner.shutdown.clone();
    inner.rt.spawn(async move {
        // Seconds since the last scan; `None` = scan on the next enabled tick.
        let mut since_scan: Option<u64> = None;
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            let Some(inner) = weak.upgrade() else { break };
            let settings = inner.settings();
            let enabled = settings.watch_enabled
                && settings.watch_interval_secs > 0
                && !settings.watch_folder.as_os_str().is_empty();
            if !enabled {
                since_scan = None;
                continue;
            }
            let due = match since_scan {
                None => true,
                Some(s) => s + 1 >= settings.watch_interval_secs,
            };
            if due {
                since_scan = Some(0);
                // Errors are logged by the scan itself.
                let _ = scan_watch_folder(&inner, false);
            } else {
                since_scan = since_scan.map(|s| s + 1);
            }
        }
    });
}

/// Scans the watch folder. `manual` scans log every outcome; timer scans log only imports
/// and problems not reported before.
pub(crate) fn scan_watch_folder(inner: &Arc<Inner>, manual: bool) -> Result<Vec<String>, ServiceError> {
    let settings = inner.settings();
    let folder = settings.watch_folder.clone();
    if folder.as_os_str().is_empty() {
        if manual {
            inner.log(LogLevel::Info, "watch folder is not configured");
        }
        return Ok(Vec::new());
    }
    let entries = match std::fs::read_dir(&folder) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if manual {
                inner.log(LogLevel::Warn, format!("watch folder {} does not exist", folder.display()));
            }
            return Ok(Vec::new());
        }
        Err(e) => {
            if manual {
                inner.log(LogLevel::Error, format!("could not read watch folder {}: {e}", folder.display()));
            }
            return Err(e.into());
        }
    };

    let mut candidates: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("nzb")))
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()))
        .collect();
    candidates.sort();

    let mut imported = Vec::new();
    for path in candidates {
        let Ok(canonical) = std::fs::canonicalize(&path) else { continue };
        match inner.store.job_by_source(&canonical) {
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(e) => {
                inner.log(LogLevel::Error, format!("could not check {}: {e}", path.display()));
                return Err(e.into());
            }
        }
        match inner.add_nzb_inner(&canonical) {
            Ok(job) => imported.push(job.name),
            Err(e) => {
                let mtime = std::fs::metadata(&canonical).and_then(|m| m.modified()).ok();
                let fresh = lock(&inner.watch_rejected).insert((canonical.clone(), mtime));
                if manual || fresh {
                    inner.log(LogLevel::Warn, format!("watch folder: could not import {}: {e}", path.display()));
                }
            }
        }
    }

    if imported.is_empty() {
        if manual {
            inner.log(LogLevel::Info, "watch folder: no new NZB files");
        }
        return Ok(imported);
    }
    inner.log(
        LogLevel::Info,
        format!(
            "watch folder: imported {} NZB{}",
            imported.len(),
            if imported.len() == 1 { "" } else { "s" }
        ),
    );
    inner.emit(ServiceEvent::WatchImported(imported.clone()));
    if settings.watch_auto_start && !inner.lock_state().paused {
        runner::start(inner);
    }
    Ok(imported)
}
