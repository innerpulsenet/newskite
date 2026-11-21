//! Background timers: the 1 Hz throughput sampler and the watch-folder scanner.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use newskite_store::ThroughputSample;
use tokio::time::MissedTickBehavior;

use crate::{lock, now_secs, runner, Fingerprint, Inner, LogLevel, ServiceError, ServiceEvent, SAMPLE_RETENTION_SECS};

/// Sampler ticks between two prunes of old throughput samples (one hour).
const PRUNE_EVERY_TICKS: u64 = 3600;

/// Starts the 1 Hz throughput sampler. It holds only a weak reference to the service and
/// stops with [`crate::Service::shutdown`] or when the service is dropped.
pub(crate) fn spawn_sampler(inner: &Arc<Inner>) {
    let weak = Arc::downgrade(inner);
    let token = inner.shutdown.clone();
    inner.rt.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await; // the first tick completes immediately
        let mut ticks: u64 = 0;
        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    // Record what moved since the last tick (the job interrupted by the
                    // shutdown may have added bytes after Service::shutdown flushed).
                    if let Some(inner) = weak.upgrade() {
                        flush_throughput(&inner);
                    }
                    break;
                }
                _ = interval.tick() => {}
            }
            let Some(inner) = weak.upgrade() else { break };
            let delta = take_delta(&inner);
            sample(&inner, delta, ticks.is_multiple_of(PRUNE_EVERY_TICKS));
            ticks = ticks.wrapping_add(1);
        }
    });
}

/// Bytes downloaded since the last call (marks them as recorded).
fn take_delta(inner: &Inner) -> u64 {
    let mut sampled = lock(&inner.sampled_down);
    let now = inner.progress.bytes_down();
    let delta = now.saturating_sub(*sampled);
    *sampled = now.max(*sampled);
    delta
}

/// Writes the bytes downloaded since the last sampler tick to the current second of the
/// throughput history and to the lifetime total, without publishing a speed. Called when
/// a run ends and at shutdown, so a run shorter than a tick (or the tail of the last one)
/// is not lost.
pub(crate) fn flush_throughput(inner: &Inner) {
    let delta = take_delta(inner);
    if delta == 0 {
        return;
    }
    let result = inner.store.record_throughput(&ThroughputSample { ts: now_secs(), bytes_up: 0, bytes_down: delta }, true);
    if let Err(e) = result {
        if !inner.sampler_error_logged.swap(true, Ordering::Relaxed) {
            inner.log(LogLevel::Warn, format!("could not store throughput: {e}"));
        }
    }
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
        // The sample and the lifetime total in one write.
        result = inner.store.record_throughput(&ThroughputSample { ts, bytes_up: 0, bytes_down: delta }, delta > 0);
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

/// Starts the watch-folder timer. While `watch_enabled` and `watch_interval_secs > 0` (and
/// a folder is set), the folder is scanned right away and then every interval, on a
/// blocking thread. In between it sleeps until the next scan is due or the settings
/// change, so a disabled watch folder costs nothing and a change applies at once.
pub(crate) fn spawn_watch_timer(inner: &Arc<Inner>) {
    let weak = Arc::downgrade(inner);
    let token = inner.shutdown.clone();
    let mut changed = inner.settings_changed.subscribe();
    inner.rt.spawn(async move {
        // The first scan waits a moment after startup, as it always has.
        tokio::select! {
            _ = token.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        // When the last scan ended; `None` = scan as soon as the watch folder is enabled.
        let mut last_scan: Option<Instant> = None;
        loop {
            // Seen before reading, so a change made after this read wakes the wait below.
            changed.borrow_and_update();
            let Some(inner) = weak.upgrade() else { break };
            let settings = inner.settings();
            let enabled = settings.watch_enabled
                && settings.watch_interval_secs > 0
                && !settings.watch_folder.as_os_str().is_empty();
            let wait = if enabled {
                let interval = Duration::from_secs(settings.watch_interval_secs);
                match last_scan.map(|t| t.elapsed()) {
                    Some(since) if since < interval => Some(interval - since),
                    _ => {
                        // Errors are logged by the scan itself.
                        let scan = inner.clone();
                        let _ = tokio::task::spawn_blocking(move || scan_watch_folder(&scan, false)).await;
                        last_scan = Some(Instant::now());
                        continue;
                    }
                }
            } else {
                last_scan = None;
                None
            };
            drop((inner, settings));
            let due = async {
                match wait {
                    Some(w) => tokio::time::sleep(w).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = token.cancelled() => break,
                r = changed.changed() => if r.is_err() { break },
                _ = due => {}
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

    // With the (link-following) metadata read to find the files, which is also the
    // fingerprint of the canonical path.
    let mut candidates: Vec<(PathBuf, Fingerprint)> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("nzb")))
        .filter_map(|p| {
            let meta = std::fs::metadata(&p).ok().filter(std::fs::Metadata::is_file)?;
            Some((p, Fingerprint::from_meta(&meta)))
        })
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    let mut imported = Vec::new();
    for (path, fp) in candidates {
        let Ok(canonical) = std::fs::canonicalize(&path) else { continue };
        // Cheap check first, so known files are not parsed every interval; import_nzb
        // checks again under the import lock.
        match inner.watch_source_known(&canonical, fp) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(e) => {
                inner.log(LogLevel::Error, format!("could not check {}: {e}", path.display()));
                return Err(e.into());
            }
        }
        match inner.import_nzb(&canonical, Some(fp)) {
            Ok(Some(job)) => imported.push(job.name),
            Ok(None) => {}
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
