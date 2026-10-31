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
async fn log_ring_keeps_the_newest_fifty_newest_first() {
    let tmp = TempDir::new().unwrap();
    let svc = Service::open(ServiceOptions::new(tmp.path().join("db")), tokio::runtime::Handle::current()).unwrap();
    let mut rx = svc.subscribe();
    for i in 0..60 {
        svc.log(if i % 2 == 0 { LogLevel::Info } else { LogLevel::Warn }, format!("line {i}"));
    }
    svc.log(LogLevel::Error, "boom");
    let logs = svc.logs();
    assert_eq!(logs.len(), LOG_CAPACITY);
    assert_eq!(logs[0].text, "boom");
    assert_eq!(logs[0].level, LogLevel::Error);
    assert_eq!(logs[1].text, "line 59");
    assert_eq!(logs[49].text, "line 11");
    assert!(logs.windows(2).all(|w| w[0].ts >= w[1].ts));
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
    let reloaded = crate::Store::open(&tmp.path().join("db")).unwrap().load_settings().unwrap();
    assert_eq!(reloaded, stored);
    assert!(!format!("{reloaded:?}").contains(PASS));
    assert!(svc.logs().iter().all(|l| !l.text.contains(PASS)));

    svc.set_runtime_settings_override(None);
    assert!(!svc.has_runtime_settings_override());
}
