//! Unit tests that need crate internals. The auto-shutdown tests live here because in
//! this test build the platform power-off command is compiled out (see
//! `shutdown::platform_shutdown`), so they can never power off the machine.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;

use crate::common::*;
use crate::{LogLevel, Service, ServiceEvent, ServiceOptions, LOG_CAPACITY};

/// A service with one queued 64 KiB job against a mock server; `allow_shutdown` and the
/// countdown length are configurable. The hook (if any) is installed before anything runs.
async fn shutdown_fixture(
    allow_shutdown: bool,
    countdown_secs: u32,
    hook: Option<Arc<AtomicUsize>>,
) -> (TempDir, Service, newskite_nntp::testkit::MockServer) {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("sd", "sd.bin", 64 * 1024, 2, 7);
    let mock = mock_for(&[&file], &[], 0).await;
    let opts = ServiceOptions { data_dir: tmp.path().join("db"), allow_shutdown };
    let svc = Service::open(opts, tokio::runtime::Handle::current()).unwrap();
    if let Some(counter) = hook {
        svc.set_shutdown_hook(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }));
    }
    let mut settings = test_settings(tmp.path(), vec![server_for(&mock, 2)]);
    settings.auto_shutdown = true;
    settings.auto_shutdown_countdown_secs = countdown_secs;
    svc.save_settings(settings).unwrap();
    let nzb = write_nzb(&tmp.path().join("nzbs"), "sd.nzb", None, &[&file]);
    svc.add_nzb(&nzb).unwrap();
    (tmp, svc, mock)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_shutdown_never_runs_when_not_allowed() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (_tmp, svc, _mock) = shutdown_fixture(false, 1, Some(counter.clone())).await;
    let mut rx = svc.subscribe();
    assert!(svc.start());
    wait_idle(&svc, 20).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let events = drain(&mut rx);
    assert!(!events.iter().any(|e| matches!(e, ServiceEvent::ShutdownCountdown { .. })));
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    assert!(svc.logs().iter().any(|l| l.text.contains("auto-shutdown is not available")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_shutdown_countdown_can_be_canceled() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (_tmp, svc, _mock) = shutdown_fixture(true, 30, Some(counter.clone())).await;
    let mut rx = svc.subscribe();
    assert!(svc.start());
    wait_event(&mut rx, 20, |e| *e == ServiceEvent::ShutdownCountdown { secs_left: 30 }).await;
    svc.cancel_shutdown();
    wait_event(&mut rx, 2, |e| *e == ServiceEvent::ShutdownCanceled).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let later = drain(&mut rx);
    assert!(
        !later.iter().any(|e| matches!(e, ServiceEvent::ShutdownCountdown { .. })),
        "countdown kept ticking after cancel: {later:?}"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    // Canceling again is a no-op.
    svc.cancel_shutdown();
    assert!(!drain(&mut rx).contains(&ServiceEvent::ShutdownCanceled));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_shutdown_countdown_runs_the_hook_once() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (_tmp, svc, _mock) = shutdown_fixture(true, 2, Some(counter.clone())).await;
    let mut rx = svc.subscribe();
    assert!(svc.start());
    let mut ticks = Vec::new();
    while ticks.last() != Some(&0) {
        if let ServiceEvent::ShutdownCountdown { secs_left } =
            wait_event(&mut rx, 20, |e| matches!(e, ServiceEvent::ShutdownCountdown { .. })).await
        {
            ticks.push(secs_left);
        }
    }
    assert_eq!(ticks, vec![2, 1, 0]);
    for _ in 0..40 {
        if counter.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn platform_command_is_compiled_out_of_unit_tests() {
    let (_tmp, svc, _mock) = shutdown_fixture(true, 0, None).await;
    let mut rx = svc.subscribe();
    assert!(svc.start());
    wait_event(&mut rx, 20, |e| *e == ServiceEvent::ShutdownCountdown { secs_left: 0 }).await;
    let ev = wait_event(&mut rx, 5, |e| matches!(e, ServiceEvent::Error(t) if t.contains("shutdown failed"))).await;
    assert!(matches!(ev, ServiceEvent::Error(t) if t.contains("disabled in unit tests")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starting_the_queue_cancels_the_countdown() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (_tmp, svc, _mock) = shutdown_fixture(true, 30, Some(counter.clone())).await;
    let mut rx = svc.subscribe();
    assert!(svc.start());
    wait_event(&mut rx, 20, |e| matches!(e, ServiceEvent::ShutdownCountdown { .. })).await;
    assert!(svc.start());
    wait_event(&mut rx, 2, |e| *e == ServiceEvent::ShutdownCanceled).await;
    wait_idle(&svc, 5).await;
    // An empty run (nothing processed) must not start a new countdown.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(!drain(&mut rx).iter().any(|e| matches!(e, ServiceEvent::ShutdownCountdown { .. })));
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn log_ring_keeps_the_newest_lines_newest_first_and_writes_the_file() {
    let tmp = TempDir::new().unwrap();
    let svc = Service::open(ServiceOptions::new(tmp.path().join("db")), tokio::runtime::Handle::current()).unwrap();
    let mut rx = svc.subscribe();
    let n = LOG_CAPACITY + 10;
    for i in 0..n {
        svc.log(if i % 2 == 0 { LogLevel::Info } else { LogLevel::Warn }, format!("line {i}"));
    }
    svc.log(LogLevel::Error, "boom");
    let logs = svc.logs();
    assert_eq!(logs.len(), LOG_CAPACITY);
    assert_eq!(logs[0].text, "boom");
    assert_eq!(logs[0].level, LogLevel::Error);
    assert_eq!(logs[1].text, format!("line {}", n - 1));
    assert_eq!(logs[LOG_CAPACITY - 1].text, format!("line {}", n - LOG_CAPACITY + 1));
    assert!(logs.windows(2).all(|w| w[0].ts >= w[1].ts));
    let file = std::fs::read_to_string(svc.log_file_path()).unwrap();
    assert!(file.lines().last().unwrap().ends_with("error boom"), "{}", file.lines().last().unwrap());
    let events = drain(&mut rx);
    assert!(events.contains(&ServiceEvent::JobLog("line 0".into())));
    assert!(events.contains(&ServiceEvent::Error("boom".into())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_settings_override_is_used_but_never_persisted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let file = TestFile::random("ov", "ov.bin", 32 * 1024, 2, 11);
    let mock = mock_for(&[&file], &[], 0).await;
    // Stored settings have no usable server.
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![]));
    let stored = svc.settings();
    assert!(!svc.has_runtime_settings_override());

    let mut over = test_settings(tmp.path(), vec![server_for(&mock, 2)]);
    over.download_dir = tmp.path().join("override-dest");
    svc.set_runtime_settings_override(Some(over));
    assert!(svc.has_runtime_settings_override());
    // The public settings() keeps returning the stored document.
    assert_eq!(svc.settings(), stored);

    let nzb = write_nzb(&tmp.path().join("nzbs"), "ov.nzb", None, &[&file]);
    let job = svc.add_nzb(&nzb).unwrap();
    assert!(job.dest_dir.starts_with(tmp.path().join("override-dest")));
    assert!(svc.start());
    wait_idle(&svc, 20).await;
    assert_eq!(svc.jobs()[0].state, crate::JobState::Done);

    // Nothing of the override reached the database.
    // (The service holds the data directory's single-instance lock, so read through it.)
    let reloaded = svc.inner.store.load_settings().unwrap();
    assert_eq!(reloaded, stored);
    assert!(!format!("{reloaded:?}").contains(PASS));
    assert!(svc.logs().iter().all(|l| !l.text.contains(PASS)));

    svc.set_runtime_settings_override(None);
    assert!(!svc.has_runtime_settings_override());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_speed_limit_is_live_and_saved() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut settings = test_settings(tmp.path(), vec![]);
    settings.speed_limit_kbps = 100;
    let svc = open_service(tmp.path(), settings);
    // The stored limit is in force from the start.
    assert_eq!(svc.speed_limit_bps(), 100 * 1024);
    // Engines hold the same value, so a change reaches a running download.
    let engine_side = svc.inner.speed_limit.clone();

    svc.set_speed_limit_kbps(25 * 1024).unwrap();
    assert_eq!(svc.speed_limit_bps(), 25 * 1024 * 1024);
    assert_eq!(engine_side.get(), 25 * 1024 * 1024);
    assert_eq!(svc.settings().speed_limit_kbps, 25 * 1024, "saved");
    assert!(svc.logs().iter().any(|l| l.text == "speed limit 25.0 MB/s"), "{:?}", svc.logs());

    // A save from the Settings sheet applies at once too.
    let mut s = svc.settings();
    s.speed_limit_kbps = 0;
    svc.save_settings(s).unwrap();
    assert_eq!(engine_side.get(), 0, "unlimited");

    // The headless override wins while set, and the stored value comes back after.
    let mut over = svc.settings();
    over.speed_limit_kbps = 7;
    svc.set_runtime_settings_override(Some(over));
    assert_eq!(engine_side.get(), 7 * 1024);
    svc.set_runtime_settings_override(None);
    assert_eq!(engine_side.get(), 0);
}

/// An older job that was retried must still block a re-import of the same NZB, even when a
/// newer job for that NZB is already done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reimport_is_refused_while_an_older_retried_job_is_pending() {
    use crate::{JobState, ServiceError};
    let tmp = TempDir::new().unwrap();
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![]));
    let file = TestFile::random("d", "d.bin", 1024, 1, 3);
    let nzb = write_nzb(&tmp.path().join("nzbs"), "a.nzb", None, &[&file]);

    let first = svc.add_nzb(&nzb).unwrap();
    svc.inner.store.set_job_state(first.id, JobState::Failed, Some("boom")).unwrap();
    let second = svc.add_nzb(&nzb).unwrap();
    svc.inner.store.set_job_state(second.id, JobState::Done, None).unwrap();
    svc.retry_job(first.id).unwrap();

    assert!(matches!(svc.add_nzb(&nzb), Err(ServiceError::Duplicate(_))));
    let pending = svc.jobs().into_iter().filter(|j| j.state.is_pending()).count();
    assert_eq!(pending, 1);
}

/// Retrying a failed job is refused while another job for the same NZB is pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_is_refused_while_another_job_for_the_nzb_is_pending() {
    use crate::{JobState, ServiceError};
    let tmp = TempDir::new().unwrap();
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![]));
    let file = TestFile::random("r", "r.bin", 1024, 1, 4);
    let nzb = write_nzb(&tmp.path().join("nzbs"), "r.nzb", None, &[&file]);

    let first = svc.add_nzb(&nzb).unwrap();
    svc.inner.store.set_job_state(first.id, JobState::Failed, Some("boom")).unwrap();
    let second = svc.add_nzb(&nzb).unwrap();

    assert!(matches!(svc.retry_job(first.id), Err(ServiceError::Duplicate(_))));
    assert_eq!(svc.inner.store.get_job(first.id).unwrap().unwrap().state, JobState::Failed);

    // Once the other job is no longer pending, the retry goes through.
    svc.inner.store.set_job_state(second.id, JobState::Done, None).unwrap();
    svc.retry_job(first.id).unwrap();
    assert_eq!(svc.inner.store.get_job(first.id).unwrap().unwrap().state, JobState::Queued);
}

/// A second service on the same data directory is refused and leaves the first one's
/// live job alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_service_on_the_same_data_dir_is_refused() {
    use crate::JobState;
    let tmp = TempDir::new().unwrap();
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![]));
    let file = TestFile::random("s", "s.bin", 1024, 1, 5);
    let job = svc.add_nzb(&write_nzb(&tmp.path().join("nzbs"), "s.nzb", None, &[&file])).unwrap();
    svc.inner.store.set_job_state(job.id, JobState::Downloading, None).unwrap();

    let err = Service::open(ServiceOptions::new(tmp.path().join("db")), tokio::runtime::Handle::current())
        .expect_err("a second instance must be refused");
    assert!(err.to_string().contains("another Newskite instance"), "{err}");
    assert_eq!(svc.inner.store.get_job(job.id).unwrap().unwrap().state, JobState::Downloading);
}

/// Settings with `watch` as the (manually scanned) watch folder.
fn watch_settings(root: &std::path::Path, watch: &std::path::Path) -> newskite_types::Settings {
    let mut s = test_settings(root, vec![]);
    s.watch_folder = watch.to_path_buf();
    s
}

/// A job the user removed is not imported again by the watch folder while the same NZB
/// file stays there; a new file under the same name is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removed_job_is_not_reimported_by_the_watch_folder() {
    let tmp = TempDir::new().unwrap();
    let watch = tmp.path().join("watch");
    let svc = open_service(tmp.path(), watch_settings(tmp.path(), &watch));
    let file = TestFile::random("w", "w.bin", 1024, 1, 11);
    let nzb = write_nzb(&watch, "w.nzb", None, &[&file]);

    assert_eq!(svc.scan_watch_folder().unwrap(), vec!["w".to_string()]);
    let job = svc.jobs().remove(0);
    svc.inner.store.set_job_state(job.id, crate::JobState::Done, None).unwrap();
    svc.remove_job(job.id).unwrap();
    assert!(svc.scan_watch_folder().unwrap().is_empty(), "a removed job came back");
    assert!(svc.jobs().is_empty());

    // Also for an NZB from the watch folder that was added by hand and removed while queued.
    let manual = write_nzb(&watch, "m.nzb", None, &[&file]);
    let m = svc.add_nzb(&manual).unwrap();
    svc.remove_job(m.id).unwrap();
    assert!(svc.scan_watch_folder().unwrap().is_empty());

    // Replacing the file with different content is a new NZB and is imported.
    let other = TestFile::random("w2", "w2.bin", 2048, 2, 12);
    std::fs::remove_file(&nzb).unwrap();
    write_nzb(&watch, "w.nzb", None, &[&other]);
    assert_eq!(svc.scan_watch_folder().unwrap(), vec!["w".to_string()]);
}

/// Concurrent watch scans (timer and GUI) and concurrent adds never import one NZB twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_imports_do_not_create_duplicate_jobs() {
    let tmp = TempDir::new().unwrap();
    let watch = tmp.path().join("watch");
    let svc = open_service(tmp.path(), watch_settings(tmp.path(), &watch));
    let files: Vec<TestFile> =
        (0..40).map(|i| TestFile::random(&format!("c{i}"), &format!("c{i}.bin"), 4096, 4, 100 + i)).collect();
    for (i, f) in files.iter().enumerate() {
        write_nzb(&watch, &format!("c{i}.nzb"), None, &[f]);
    }
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let svc = svc.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                svc.scan_watch_folder().unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(svc.jobs().len(), 40);

    let single = write_nzb(&tmp.path().join("nzbs"), "one.nzb", None, &[&files[0]]);
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let svc = svc.clone();
            let barrier = barrier.clone();
            let single = single.clone();
            std::thread::spawn(move || {
                barrier.wait();
                svc.add_nzb(&single).is_ok()
            })
        })
        .collect();
    let added = handles.into_iter().map(|h| h.join().unwrap()).filter(|ok| *ok).count();
    assert_eq!(added, 1);
    assert_eq!(svc.jobs().len(), 41);
}

/// A new job never shares a destination folder with any other job (whatever its state),
/// with a folder differing only in case, or with a folder that already exists on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destination_folder_is_never_shared() {
    use crate::JobState;
    let tmp = TempDir::new().unwrap();
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![]));
    let downloads = tmp.path().join("downloads");
    let file = TestFile::random("d", "d.bin", 1024, 1, 21);
    let nzbs = tmp.path().join("nzbs");

    let a = svc.add_nzb(&write_nzb(&nzbs, "a1.nzb", Some("Release"), &[&file])).unwrap();
    assert_eq!(a.dest_dir, downloads.join("Release"));
    svc.inner.store.set_job_state(a.id, JobState::Failed, Some("missing articles")).unwrap();
    // A repost with the same name while the failed job's files are still there.
    let b = svc.add_nzb(&write_nzb(&nzbs, "a2.nzb", Some("Release"), &[&file])).unwrap();
    assert_eq!(b.dest_dir, downloads.join("Release (2)"));
    svc.inner.store.set_job_state(b.id, JobState::Done, None).unwrap();
    // Differs only in case from a pending job.
    let c = svc.add_nzb(&write_nzb(&nzbs, "a3.nzb", Some("release"), &[&file])).unwrap();
    assert_eq!(c.dest_dir, downloads.join("release (3)"));

    // A folder that exists on disk (a removed job's output, another program's) is skipped.
    std::fs::create_dir_all(downloads.join("Other")).unwrap();
    let d = svc.add_nzb(&write_nzb(&nzbs, "o.nzb", Some("Other"), &[&file])).unwrap();
    assert_eq!(d.dest_dir, downloads.join("Other (2)"));
}

/// An unreadable settings document is reported and survives a save of the defaults the
/// service fell back to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreadable_settings_are_kept_when_defaults_are_saved() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("db");
    let corrupt = r#"{"servers": [{"host": "news.example.com", "port": "563"}]}"#;
    drop(crate::Store::open(&data).unwrap());
    {
        let conn = rusqlite::Connection::open(data.join(newskite_store::DB_FILE_NAME)).unwrap();
        conn.execute("INSERT INTO settings (key, value) VALUES ('settings', ?1)", [corrupt]).unwrap();
    }
    let svc = Service::open(ServiceOptions::new(&data), tokio::runtime::Handle::current()).unwrap();
    assert!(svc.settings_load_error().is_some());
    let shown = svc.settings();
    assert!(shown.servers.iter().all(|s| s.host.is_empty()), "defaults are presented");

    // The user edits the defaults and saves: the original document must not be lost.
    svc.save_settings(shown).unwrap();
    assert!(svc.settings_load_error().is_none());
    let backups = svc.inner.store.settings_backups().unwrap();
    assert_eq!(backups.len(), 1);
    assert_eq!(backups[0].1, corrupt);
    assert!(svc.logs().iter().any(|l| l.level == LogLevel::Warn && l.text.contains(&backups[0].0)));
}

/// Bytes downloaded between two sampler ticks are counted when the run ends or the
/// service shuts down, not dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bytes_of_a_short_run_reach_the_lifetime_total_and_history() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("q", "q.bin", 16 * 1024, 2, 31);
    let mock = mock_for(&[&file], &[], 0).await;
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![server_for(&mock, 2)]));
    svc.add_nzb(&write_nzb(&tmp.path().join("nzbs"), "q.nzb", None, &[&file])).unwrap();
    assert!(svc.start());
    wait_idle(&svc, 20).await;
    svc.shutdown();
    let len = file.data.len() as u64;
    assert_eq!(svc.totals().downloaded_bytes, len);
    let stored: u64 = svc.throughput_history(3600).iter().map(|s| s.bytes_down).sum();
    assert_eq!(stored, len);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_settings_follow_every_write() {
    let tmp = TempDir::new().unwrap();
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![]));
    let stored = || svc.inner.store.load_settings().unwrap();
    // Loaded (and cached) once, then each write path must show through.
    assert_eq!(svc.settings(), stored());
    let mut s = svc.settings();
    s.max_connections = 3;
    s.watch_interval_secs = 77;
    svc.save_settings(s).unwrap();
    assert_eq!(svc.settings().max_connections, 3);
    assert_eq!(svc.inner.settings().watch_interval_secs, 77);
    assert_eq!(svc.settings(), stored());

    svc.set_speed_limit_kbps(512).unwrap();
    assert_eq!(svc.settings().speed_limit_kbps, 512);
    assert_eq!(svc.inner.settings().speed_limit_kbps, 512);

    let mut over = svc.settings();
    over.max_connections = 9;
    svc.set_runtime_settings_override(Some(over));
    assert_eq!(svc.inner.settings().max_connections, 9);
    assert_eq!(svc.settings().max_connections, 3, "the public settings stay the stored ones");
    svc.set_runtime_settings_override(None);
    assert_eq!(svc.inner.settings().max_connections, 3);

    // Saves racing with reads: whatever was saved last is what every reader sees after.
    let threads: Vec<_> = (0..4u64)
        .map(|t| {
            let svc = svc.clone();
            std::thread::spawn(move || {
                for i in 0..25u64 {
                    if t % 2 == 0 {
                        let mut s = svc.settings();
                        s.watch_interval_secs = 1000 + t * 100 + i;
                        svc.save_settings(s).unwrap();
                    } else {
                        let _ = svc.inner.settings();
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(svc.settings(), stored());
    assert_eq!(svc.inner.settings().watch_interval_secs, stored().watch_interval_secs);
}
