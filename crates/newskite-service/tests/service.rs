//! Integration tests for the queue service against the in-process mock NNTP server.
//!
//! None of these tests enables `allow_shutdown`; the auto-shutdown tests are unit tests in
//! `src/tests.rs`, where the platform power-off command is compiled out.

mod common;

use std::path::Path;
use std::time::Duration;

use common::*;
use newskite_service::{
    JobFile, JobMove, JobState, LogLevel, NewBookmark, ServiceError, ServiceEvent, Store, NO_SERVER_MESSAGE,
};
use newskite_types::{EngineEvent, ServerConfig};
use tempfile::TempDir;

fn read(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_nzb_happy_path_malformed_and_duplicate() {
    let tmp = TempDir::new().unwrap();
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![]));
    let mut rx = svc.subscribe();
    let a = TestFile::random("a", "alpha.bin", 3000, 3, 1);
    let b = TestFile::random("b", "beta.bin", 2000, 2, 2);
    let nzbs = tmp.path().join("nzbs");
    let path = write_nzb(&nzbs, "release.nzb", Some("My Release"), &[&a, &b]);

    let job = svc.add_nzb(&path).unwrap();
    assert_eq!(job.name, "My Release");
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.bytes_done, 0);
    let expected_total: u64 = a.articles.iter().chain(&b.articles).map(|(_, b)| b.len() as u64).sum();
    assert_eq!(job.bytes_total, expected_total);
    assert_eq!(job.source_path, std::fs::canonicalize(&path).unwrap());
    assert_eq!(job.dest_dir, tmp.path().join("downloads").join("My Release"));
    assert!(drain(&mut rx).contains(&ServiceEvent::JobsChanged));
    assert!(svc.logs()[0].text.starts_with("queued My Release (2 files,"));

    // Same file (also through a non-canonical path) is a duplicate while pending.
    let dotted = nzbs.join(".").join("release.nzb");
    assert!(matches!(svc.add_nzb(&dotted), Err(ServiceError::Duplicate(n)) if n == "My Release"));

    // Malformed and unreadable NZBs create no row and are logged.
    let bad = nzbs.join("bad.nzb");
    std::fs::write(&bad, "<nzb><file subject=\"x\"><segments>").unwrap();
    assert!(matches!(svc.add_nzb(&bad), Err(ServiceError::Nzb(_))));
    let empty = nzbs.join("empty.nzb");
    std::fs::write(&empty, "<?xml version=\"1.0\"?><nzb></nzb>").unwrap();
    assert!(matches!(svc.add_nzb(&empty), Err(ServiceError::Nzb(_))));
    assert!(matches!(svc.add_nzb(&nzbs.join("missing.nzb")), Err(ServiceError::Io(_))));
    assert_eq!(svc.jobs().len(), 1);
    assert_eq!(svc.logs()[0].level, LogLevel::Error);

    // Another NZB with the same job name gets its own folder.
    let other = write_nzb(&nzbs, "other.nzb", Some("My Release"), &[&a]);
    let job2 = svc.add_nzb(&other).unwrap();
    assert_eq!(job2.dest_dir, tmp.path().join("downloads").join("My Release (2)"));
    assert_eq!(svc.totals().jobs_pending, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_run_downloads_byte_exact_and_samples_throughput() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("big", "big.bin", 2 * 1024 * 1024, 8, 42);
    let mock = mock_for(&[&file], &[], 0).await;
    let mut settings = test_settings(tmp.path(), vec![server_for(&mock, 4)]);
    settings.postprocess.enabled = true; // nothing to repair or extract: both steps skip
    let svc = open_service(tmp.path(), settings);
    let mut rx = svc.subscribe();
    let nzb = write_nzb(&tmp.path().join("nzbs"), "big.nzb", None, &[&file]);
    let job = svc.add_nzb(&nzb).unwrap();
    assert_eq!(job.name, "big");

    assert!(svc.start());
    wait_idle(&svc, 30).await;

    let out = job.dest_dir.join("big.bin");
    assert_eq!(sha256(&read(&out)), sha256(&file.data));
    assert!(!job.dest_dir.join("big.bin.nkpart").exists());
    let done = svc.jobs().into_iter().find(|j| j.id == job.id).unwrap();
    assert_eq!(done.state, JobState::Done, "error: {:?}", done.error);
    assert_eq!(done.bytes_done, done.bytes_total);
    assert_eq!(done.error, None);
    assert_eq!(
        svc.job_files(job.id),
        vec![JobFile {
            job_id: job.id,
            name: "big.bin".into(),
            bytes_total: file.data.len() as u64,
            bytes_done: file.data.len() as u64,
            state: "done".into()
        }]
    );

    let events = drain(&mut rx);
    assert!(events.contains(&ServiceEvent::QueueIdle));
    assert!(events.iter().any(|e| matches!(e,
        ServiceEvent::Engine { job_id, event: EngineEvent::FileDone { name, .. } } if *job_id == job.id && name == "big.bin")));
    let segment_events =
        events.iter().filter(|e| matches!(e, ServiceEvent::Engine { event: EngineEvent::SegmentDone { .. }, .. })).count();
    assert_eq!(segment_events, 8);
    let logs: Vec<String> = svc.logs().into_iter().map(|l| l.text).collect();
    assert!(logs.iter().any(|l| l.starts_with("completed big")), "{logs:?}");
    assert!(logs.iter().any(|l| l.starts_with("post-processing big: par2 skipped")), "{logs:?}");
    // One log line per file, never per segment.
    assert!(logs.len() < 30, "{logs:?}");
    assert!(!logs.iter().any(|l| l.contains(PASS)));

    // The 1 Hz sampler stores the bytes and the lifetime counter.
    let want = file.data.len() as u64;
    let mut ok = false;
    for _ in 0..40 {
        let hist: u64 = svc.throughput_history(3600).iter().map(|s| s.bytes_down).sum();
        if hist == want && svc.totals().downloaded_bytes == want {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok, "throughput history/totals never reached {want}");
    assert!(events.iter().any(|e| matches!(e, ServiceEvent::Throughput { .. })) || {
        // The run may finish before the first tick; wait for one now.
        wait_event(&mut rx, 3, |e| matches!(e, ServiceEvent::Throughput { .. })).await;
        true
    });
    let totals = svc.totals();
    assert_eq!((totals.jobs_done, totals.jobs_pending, totals.jobs_failed), (1, 0, 0));
    assert!(!svc.status().running);
    assert_eq!(svc.status().last_error, None);

    // A finished job's NZB may be added again (it is no longer pending).
    assert!(svc.add_nzb(&nzb).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("s", "s.bin", 64 * 1024, 4, 3);
    let mock = mock_for(&[&file], &[], 100).await;
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![server_for(&mock, 1)]));
    svc.add_nzb(&write_nzb(&tmp.path().join("n"), "s.nzb", None, &[&file])).unwrap();
    assert!(svc.start());
    assert!(!svc.start());
    assert!(svc.status().running);
    wait_idle(&svc, 20).await;
    assert_eq!(svc.jobs()[0].state, JobState::Done);
    // A new run may start once the previous one ended (and ends at once: nothing to do).
    assert!(svc.start());
    wait_idle(&svc, 5).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_jobs_runs_only_the_listed_jobs() {
    let tmp = TempDir::new().unwrap();
    let f1 = TestFile::random("o1", "old.bin", 16 * 1024, 2, 31);
    let f2 = TestFile::random("o2", "mine.bin", 16 * 1024, 3, 32);
    let mock = mock_for(&[&f1, &f2], &[], 0).await;
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![server_for(&mock, 1)]));
    let dir = tmp.path().join("n");
    let older = svc.add_nzb(&write_nzb(&dir, "old.nzb", None, &[&f1])).unwrap();
    let mine = svc.add_nzb(&write_nzb(&dir, "mine.nzb", None, &[&f2])).unwrap();

    assert!(svc.start_jobs(&[mine.id]));
    assert!(!svc.start_jobs(&[mine.id]), "a run is already active");
    wait_idle(&svc, 20).await;
    let state = |id| svc.jobs().into_iter().find(|j| j.id == id).unwrap().state;
    assert_eq!(state(mine.id), JobState::Done);
    assert_eq!(state(older.id), JobState::Queued, "an unlisted job is left alone");
    assert_eq!(mock.body_requests(), 3);

    // An empty or already finished list ends the run at once; a plain start runs the rest.
    assert!(svc.start_jobs(&[]));
    wait_idle(&svc, 5).await;
    assert_eq!(state(older.id), JobState::Queued);
    assert!(svc.start());
    wait_idle(&svc, 20).await;
    assert_eq!(state(older.id), JobState::Done);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_stops_before_next_job_and_resume_finishes() {
    let tmp = TempDir::new().unwrap();
    let f1 = TestFile::random("p1", "one.bin", 40 * 1024, 4, 11);
    let f2 = TestFile::random("p2", "two.bin", 40 * 1024, 4, 12);
    let mock = mock_for(&[&f1, &f2], &[], 150).await;
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![server_for(&mock, 1)]));
    let dir = tmp.path().join("n");
    let j1 = svc.add_nzb(&write_nzb(&dir, "one.nzb", None, &[&f1])).unwrap();
    let j2 = svc.add_nzb(&write_nzb(&dir, "two.nzb", None, &[&f2])).unwrap();
    let mut rx = svc.subscribe();

    assert!(svc.start());
    wait_event(&mut rx, 10, |e| matches!(e, ServiceEvent::Engine { job_id, .. } if *job_id == j1.id)).await;
    svc.pause();
    wait_event(&mut rx, 1, |e| *e == ServiceEvent::Paused).await;
    assert!(svc.status().paused);
    wait_idle(&svc, 20).await;

    let jobs = svc.jobs();
    assert_eq!(jobs[0].state, JobState::Done);
    assert_eq!(jobs[1].state, JobState::Queued);
    assert_eq!(jobs[1].bytes_done, 0);
    assert!(!svc.status().running);
    assert!(svc.logs().iter().any(|l| l.text == "queue paused"));

    svc.resume();
    wait_event(&mut rx, 1, |e| *e == ServiceEvent::Resumed).await;
    wait_idle(&svc, 20).await;
    assert_eq!(svc.jobs()[1].state, JobState::Done);
    assert_eq!(sha256(&read(&j2.dest_dir.join("two.bin"))), sha256(&f2.data));
    assert!(!svc.status().paused);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn holding_the_active_job_moves_on_and_start_job_brings_it_back() {
    let tmp = TempDir::new().unwrap();
    let slow = TestFile::random("h1", "slow.bin", 80 * 1024, 8, 61);
    let next = TestFile::random("h2", "next.bin", 8 * 1024, 1, 62);
    let mock = mock_for(&[&slow, &next], &[], 300).await;
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![server_for(&mock, 1)]));
    let dir = tmp.path().join("n");
    let j1 = svc.add_nzb(&write_nzb(&dir, "slow.nzb", None, &[&slow])).unwrap();
    let j2 = svc.add_nzb(&write_nzb(&dir, "next.nzb", None, &[&next])).unwrap();
    let mut rx = svc.subscribe();
    let state = |id| svc.jobs().into_iter().find(|j| j.id == id).unwrap().state;

    assert!(svc.start());
    wait_event(&mut rx, 10, |e| {
        matches!(e, ServiceEvent::Engine { job_id, event: EngineEvent::FileStarted { .. } } if *job_id == j1.id)
    })
    .await;
    svc.pause_job(j1.id).unwrap();
    wait_idle(&svc, 10).await;
    assert_eq!(state(j1.id), JobState::Paused, "held, not failed");
    assert_eq!(state(j2.id), JobState::Done, "the run moved on");
    assert_eq!(svc.status().last_error, None);

    // A plain start skips the held job; start_job runs just it.
    assert!(svc.start());
    wait_idle(&svc, 5).await;
    assert_eq!(state(j1.id), JobState::Paused);
    svc.start_job(j1.id).unwrap();
    wait_idle(&svc, 20).await;
    assert_eq!(state(j1.id), JobState::Done);
    assert_eq!(sha256(&read(&j1.dest_dir.join("slow.bin"))), sha256(&slow.data));
    assert!(svc.pause_job(j1.id).is_err(), "finished jobs cannot be held");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_job_leaves_the_rest_and_widens_an_active_run() {
    let tmp = TempDir::new().unwrap();
    let f1 = TestFile::random("s1", "one.bin", 40 * 1024, 4, 71);
    let f2 = TestFile::random("s2", "two.bin", 8 * 1024, 1, 72);
    let f3 = TestFile::random("s3", "three.bin", 8 * 1024, 1, 73);
    let mock = mock_for(&[&f1, &f2, &f3], &[], 150).await;
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![server_for(&mock, 1)]));
    let dir = tmp.path().join("n");
    let j1 = svc.add_nzb(&write_nzb(&dir, "one.nzb", None, &[&f1])).unwrap();
    let j2 = svc.add_nzb(&write_nzb(&dir, "two.nzb", None, &[&f2])).unwrap();
    let j3 = svc.add_nzb(&write_nzb(&dir, "three.nzb", None, &[&f3])).unwrap();
    let mut rx = svc.subscribe();
    let state = |id| svc.jobs().into_iter().find(|j| j.id == id).unwrap().state;

    svc.start_job(j1.id).unwrap();
    wait_event(&mut rx, 10, |e| matches!(e, ServiceEvent::Engine { job_id, .. } if *job_id == j1.id)).await;
    assert_eq!(svc.status().run_scope, Some(vec![j1.id]));
    // Starting another job while the limited run is active adds it to the run.
    svc.start_job(j3.id).unwrap();
    wait_idle(&svc, 20).await;
    assert_eq!(state(j1.id), JobState::Done);
    assert_eq!(state(j3.id), JobState::Done);
    assert_eq!(state(j2.id), JobState::Queued, "never started");
    assert_eq!(svc.status().run_scope, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_now_interrupts_and_resume_continues_the_same_job_first() {
    let tmp = TempDir::new().unwrap();
    let slow = TestFile::random("n1", "slow.bin", 80 * 1024, 8, 81);
    let next = TestFile::random("n2", "next.bin", 8 * 1024, 1, 82);
    let mock = mock_for(&[&slow, &next], &[], 300).await;
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![server_for(&mock, 1)]));
    let dir = tmp.path().join("n");
    let j1 = svc.add_nzb(&write_nzb(&dir, "slow.nzb", None, &[&slow])).unwrap();
    let j2 = svc.add_nzb(&write_nzb(&dir, "next.nzb", None, &[&next])).unwrap();
    let mut rx = svc.subscribe();
    let state = |id| svc.jobs().into_iter().find(|j| j.id == id).unwrap().state;

    assert!(svc.start());
    wait_event(&mut rx, 10, |e| {
        matches!(e, ServiceEvent::Engine { job_id, event: EngineEvent::FileStarted { .. } } if *job_id == j1.id)
    })
    .await;
    let t0 = std::time::Instant::now();
    svc.pause_now();
    wait_idle(&svc, 10).await;
    assert!(t0.elapsed() < Duration::from_secs(4), "pause took {:?}", t0.elapsed());
    assert_eq!(state(j1.id), JobState::Queued, "interrupted, resumable");
    assert_eq!(state(j2.id), JobState::Queued, "the next job did not start");
    assert!(svc.status().paused);

    svc.resume();
    wait_idle(&svc, 30).await;
    assert_eq!(state(j1.id), JobState::Done);
    assert_eq!(state(j2.id), JobState::Done);
    assert_eq!(sha256(&read(&j1.dest_dir.join("slow.bin"))), sha256(&slow.data));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_job_reorders_the_queue_and_clear_history_keeps_pending_jobs() {
    let tmp = TempDir::new().unwrap();
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![]));
    let dir = tmp.path().join("n");
    let f = |tag: &str, seed| TestFile::random(tag, &format!("{tag}.bin"), 1024, 1, seed);
    let (fa, fb, fc) = (f("a", 1), f("b", 2), f("c", 3));
    let a = svc.add_nzb(&write_nzb(&dir, "a.nzb", None, &[&fa])).unwrap();
    let b = svc.add_nzb(&write_nzb(&dir, "b.nzb", None, &[&fb])).unwrap();
    let c = svc.add_nzb(&write_nzb(&dir, "c.nzb", None, &[&fc])).unwrap();
    assert_eq!(svc.queue_order(), vec![a.id, b.id, c.id]);

    svc.move_job(c.id, JobMove::Top).unwrap();
    assert_eq!(svc.queue_order(), vec![c.id, a.id, b.id]);
    svc.move_job(c.id, JobMove::Down).unwrap();
    assert_eq!(svc.queue_order(), vec![a.id, c.id, b.id]);
    svc.move_job(a.id, JobMove::Up).unwrap();
    assert_eq!(svc.queue_order(), vec![a.id, c.id, b.id], "already first");
    svc.move_job(a.id, JobMove::Bottom).unwrap();
    assert_eq!(svc.queue_order(), vec![c.id, b.id, a.id]);

    svc.pause_job(b.id).unwrap();
    assert_eq!(svc.queue_order(), vec![c.id, b.id, a.id], "held jobs keep their place");
    assert_eq!(svc.clear_history(), 0);
    assert_eq!(svc.jobs().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_the_active_job_cancels_it_and_the_run_continues() {
    let tmp = TempDir::new().unwrap();
    let slow = TestFile::random("r1", "slow.bin", 80 * 1024, 8, 21);
    let next = TestFile::random("r2", "next.bin", 8 * 1024, 1, 22);
    let mock = mock_for(&[&slow, &next], &[], 300).await;
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![server_for(&mock, 1)]));
    let dir = tmp.path().join("n");
    let j1 = svc.add_nzb(&write_nzb(&dir, "slow.nzb", None, &[&slow])).unwrap();
    let j2 = svc.add_nzb(&write_nzb(&dir, "next.nzb", None, &[&next])).unwrap();
    let mut rx = svc.subscribe();

    assert!(svc.start());
    wait_event(&mut rx, 10, |e| {
        matches!(e, ServiceEvent::Engine { job_id, event: EngineEvent::FileStarted { .. } } if *job_id == j1.id)
    })
    .await;
    assert_eq!(svc.status().active_job, Some(j1.id));
    let t0 = std::time::Instant::now();
    svc.remove_job(j1.id).unwrap();
    wait_idle(&svc, 10).await;
    assert!(t0.elapsed() < Duration::from_secs(4), "cancel took {:?}", t0.elapsed());

    let jobs = svc.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, j2.id);
    assert_eq!(jobs[0].state, JobState::Done);
    assert!(svc.job_files(j1.id).is_empty());
    assert!(!j1.dest_dir.join("slow.bin").exists());
    assert_eq!(svc.status().last_error, None);
    assert!(matches!(svc.remove_job(j1.id), Err(ServiceError::NotFound(id)) if id == j1.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_article_fails_the_job_with_the_id_and_retry_requeues() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("m", "m.bin", 30 * 1024, 3, 31);
    let gone = file.articles[1].0.clone();
    let mock = mock_for(&[&file], &[gone.as_str()], 0).await;
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![server_for(&mock, 2)]));
    let job = svc.add_nzb(&write_nzb(&tmp.path().join("n"), "m.nzb", None, &[&file])).unwrap();

    assert!(svc.start());
    wait_idle(&svc, 20).await;
    let failed = svc.jobs().remove(0);
    assert_eq!(failed.state, JobState::Failed);
    let err = failed.error.clone().unwrap();
    assert_eq!(err, format!("1 missing article: {gone}"));
    assert!(!job.dest_dir.join("m.bin").exists(), "an incomplete file must not look complete");
    assert!(job.dest_dir.join("m.bin.nkpart").exists());
    assert_eq!(svc.job_files(job.id)[0].state, "failed");
    assert!(svc.status().last_error.unwrap().contains(&gone));

    svc.retry_job(job.id).unwrap();
    let requeued = svc.jobs().remove(0);
    assert_eq!(requeued.state, JobState::Queued);
    assert_eq!(requeued.error, None);
    assert!(matches!(svc.retry_job(job.id), Err(ServiceError::Other(_))));
    assert!(matches!(svc.retry_job(9999), Err(ServiceError::NotFound(9999))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_server_leaves_the_job_queued_with_a_single_error() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("ns", "ns.bin", 1024, 1, 41);
    let disabled = ServerConfig { host: "news.invalid".into(), enabled: false, ..Default::default() };
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![disabled]));
    svc.add_nzb(&write_nzb(&tmp.path().join("n"), "ns.nzb", None, &[&file])).unwrap();
    let mut rx = svc.subscribe();

    assert!(svc.start());
    wait_idle(&svc, 5).await;
    assert_eq!(svc.jobs()[0].state, JobState::Queued);
    assert_eq!(svc.jobs()[0].error, None);
    let errors: Vec<_> = svc.logs().into_iter().filter(|l| l.level == LogLevel::Error).collect();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].text, NO_SERVER_MESSAGE);
    assert_eq!(svc.status().last_error.as_deref(), Some(NO_SERVER_MESSAGE));
    let events = drain(&mut rx);
    assert!(events.contains(&ServiceEvent::Error(NO_SERVER_MESSAGE.into())));
    assert!(events.contains(&ServiceEvent::QueueIdle));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_failure_fails_the_job_and_stops_the_run() {
    let tmp = TempDir::new().unwrap();
    let f1 = TestFile::random("af1", "a1.bin", 1024, 1, 51);
    let f2 = TestFile::random("af2", "a2.bin", 1024, 1, 52);
    let mock = mock_for(&[&f1, &f2], &[], 0).await;
    let mut server = server_for(&mock, 2);
    server.password = "wrong".into();
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![server]));
    let dir = tmp.path().join("n");
    svc.add_nzb(&write_nzb(&dir, "a1.nzb", None, &[&f1])).unwrap();
    svc.add_nzb(&write_nzb(&dir, "a2.nzb", None, &[&f2])).unwrap();

    assert!(svc.start());
    wait_idle(&svc, 10).await;
    let jobs = svc.jobs();
    assert_eq!(jobs[0].state, JobState::Failed);
    assert!(jobs[0].error.as_deref().unwrap().starts_with("authentication failed"), "{:?}", jobs[0].error);
    assert_eq!(jobs[1].state, JobState::Queued, "the run must stop after an auth failure");
    assert_eq!(mock.body_requests(), 0);
    assert!(!svc.logs().iter().any(|l| l.text.contains("wrong")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_resumes_an_interrupted_job_and_skips_done_files() {
    let tmp = TempDir::new().unwrap();
    let f1 = TestFile::random("rs1", "first.bin", 50 * 1024, 4, 61);
    let f2 = TestFile::random("rs2", "second.bin", 50 * 1024, 5, 62);
    let mock = mock_for(&[&f1, &f2], &[], 0).await;
    let settings = test_settings(tmp.path(), vec![server_for(&mock, 2)]);
    let nzb = std::fs::canonicalize(write_nzb(&tmp.path().join("n"), "rs.nzb", None, &[&f1, &f2])).unwrap();
    let dest = tmp.path().join("downloads").join("rs");

    // A previous process: job left `downloading` with the first file complete on disk.
    let job_id = {
        let store = Store::open(&tmp.path().join("db")).unwrap();
        store.save_settings(&settings).unwrap();
        let total: u64 = f1.articles.iter().chain(&f2.articles).map(|(_, b)| b.len() as u64).sum();
        let job = store
            .insert_job(&newskite_store::NewJob {
                name: "rs".into(),
                bytes_total: total,
                dest_dir: dest.clone(),
                source_path: nzb.clone(),
                password: None,
            })
            .unwrap();
        store.set_job_state(job.id, JobState::Downloading, None).unwrap();
        store.set_job_progress(job.id, f1.data.len() as u64).unwrap();
        store
            .upsert_job_file(&JobFile {
                job_id: job.id,
                name: "first.bin".into(),
                bytes_total: f1.data.len() as u64,
                bytes_done: f1.data.len() as u64,
                state: "done".into(),
            })
            .unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("first.bin"), &f1.data).unwrap();
        job.id
    };

    let svc = open_service(tmp.path(), settings);
    let job = svc.jobs().remove(0);
    assert_eq!(job.id, job_id);
    assert_eq!(job.state, JobState::Queued, "interrupted job must be reset to queued");
    assert_eq!(job.bytes_done, f1.data.len() as u64, "progress survives the restart");
    assert!(svc.logs().iter().any(|l| l.text == "1 interrupted job will resume"));

    assert!(svc.start());
    wait_idle(&svc, 20).await;
    let job = svc.jobs().remove(0);
    assert_eq!(job.state, JobState::Done, "{:?}", job.error);
    assert_eq!(job.bytes_done, job.bytes_total);
    assert_eq!(mock.body_requests(), f2.articles.len(), "only the unfinished file is fetched");
    assert_eq!(sha256(&read(&dest.join("second.bin"))), sha256(&f2.data));
    assert_eq!(sha256(&read(&dest.join("first.bin"))), sha256(&f1.data));
    let files = svc.job_files(job_id);
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f.state == "done"));
}

/// A file recorded as `done` whose data did not survive (shorter on disk than recorded, as
/// after a power loss) must be downloaded again, not skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_redownloads_a_done_file_that_is_truncated_on_disk() {
    let tmp = TempDir::new().unwrap();
    let f1 = TestFile::random("tr1", "first.bin", 50 * 1024, 4, 63);
    let f2 = TestFile::random("tr2", "second.bin", 50 * 1024, 5, 64);
    let mock = mock_for(&[&f1, &f2], &[], 0).await;
    let settings = test_settings(tmp.path(), vec![server_for(&mock, 2)]);
    let nzb = std::fs::canonicalize(write_nzb(&tmp.path().join("n"), "tr.nzb", None, &[&f1, &f2])).unwrap();
    let dest = tmp.path().join("downloads").join("tr");

    let job_id = {
        let store = Store::open(&tmp.path().join("db")).unwrap();
        store.save_settings(&settings).unwrap();
        let total: u64 = f1.articles.iter().chain(&f2.articles).map(|(_, b)| b.len() as u64).sum();
        let job = store
            .insert_job(&newskite_store::NewJob {
                name: "tr".into(),
                bytes_total: total,
                dest_dir: dest.clone(),
                source_path: nzb.clone(),
                password: None,
            })
            .unwrap();
        store.set_job_state(job.id, JobState::Downloading, None).unwrap();
        store
            .upsert_job_file(&JobFile {
                job_id: job.id,
                name: "first.bin".into(),
                bytes_total: f1.data.len() as u64,
                bytes_done: f1.data.len() as u64,
                state: "done".into(),
            })
            .unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("first.bin"), &f1.data[..1000]).unwrap();
        job.id
    };

    let svc = open_service(tmp.path(), settings);
    assert!(svc.start());
    wait_idle(&svc, 20).await;
    let job = svc.jobs().remove(0);
    assert_eq!(job.state, JobState::Done, "{:?}", job.error);
    assert_eq!(mock.body_requests(), f1.articles.len() + f2.articles.len(), "the truncated file is fetched again");
    assert_eq!(sha256(&read(&dest.join("first.bin"))), sha256(&f1.data));
    assert_eq!(sha256(&read(&dest.join("second.bin"))), sha256(&f2.data));
    let mut names: Vec<String> = std::fs::read_dir(&dest).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, vec!["first.bin", "second.bin"], "no numbered duplicate");
    assert!(svc.logs().iter().any(|l| l.text.contains("first.bin") && l.text.contains("again")));
    assert!(svc.job_files(job_id).iter().all(|f| f.state == "done"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_interrupts_the_active_job_and_leaves_it_resumable() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("sh", "sh.bin", 80 * 1024, 8, 71);
    let mock = mock_for(&[&file], &[], 300).await;
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![server_for(&mock, 1)]));
    let job = svc.add_nzb(&write_nzb(&tmp.path().join("n"), "sh.nzb", None, &[&file])).unwrap();
    let mut rx = svc.subscribe();
    assert!(svc.start());
    wait_event(&mut rx, 10, |e| matches!(e, ServiceEvent::Engine { event: EngineEvent::FileStarted { .. }, .. })).await;
    svc.shutdown();
    wait_idle(&svc, 5).await;
    let j = svc.jobs().remove(0);
    assert_eq!(j.id, job.id);
    assert_eq!(j.state, JobState::Queued);
    assert_eq!(j.error, None);
    assert!(!svc.start(), "a stopped service does not start runs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_folder_scan_imports_once() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("w", "w.bin", 1024, 1, 81);
    let watch = tmp.path().join("watch");
    // An NZB imported earlier by hand (now finished) is not imported again. It is recorded
    // before the service opens, because the service holds the data directory's lock.
    // (The watch folder must not exist yet, so the canonical path is built by hand; the
    // file itself is written below.)
    let manual = std::fs::canonicalize(tmp.path()).unwrap().join("watch").join("gamma.nzb");
    {
        let store = Store::open(&tmp.path().join("db")).unwrap();
        let g = store
            .insert_job(&newskite_store::NewJob {
                name: "gamma".into(),
                bytes_total: 1024,
                dest_dir: tmp.path().join("downloads").join("gamma"),
                source_path: manual,
                password: None,
            })
            .unwrap();
        store.set_job_state(g.id, JobState::Done, None).unwrap();
    }
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![]));

    // Not configured, then missing: no error, a log line, nothing imported.
    assert!(svc.scan_watch_folder().unwrap().is_empty());
    assert_eq!(svc.logs()[0].text, "watch folder is not configured");
    let mut s = svc.settings();
    s.watch_folder = watch.clone();
    svc.save_settings(s).unwrap();
    assert!(svc.scan_watch_folder().unwrap().is_empty());
    assert!(svc.logs()[0].text.contains("does not exist"));

    write_nzb(&watch, "alpha.nzb", None, &[&file]);
    write_nzb(&watch, "Beta.NZB", None, &[&file]);
    write_nzb(&watch.join("sub"), "nested.nzb", None, &[&file]);
    std::fs::write(watch.join("notes.txt"), "not an nzb").unwrap();
    std::fs::write(watch.join("broken.nzb"), "<nzb><file").unwrap();
    write_nzb(&watch, "gamma.nzb", None, &[&file]);

    let mut rx = svc.subscribe();
    let names = svc.scan_watch_folder().unwrap();
    assert_eq!(names, vec!["Beta".to_string(), "alpha".to_string()]);
    assert!(drain(&mut rx).contains(&ServiceEvent::WatchImported(names.clone())));
    assert!(svc.logs().iter().any(|l| l.level == LogLevel::Warn && l.text.contains("broken.nzb")));
    assert_eq!(svc.jobs().len(), 3);
    assert!(!svc.status().running, "no auto-start unless configured");

    assert!(svc.scan_watch_folder().unwrap().is_empty());
    assert_eq!(svc.jobs().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_timer_imports_and_auto_starts() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("wt", "wt.bin", 16 * 1024, 2, 91);
    let mock = mock_for(&[&file], &[], 0).await;
    let watch = tmp.path().join("watch");
    write_nzb(&watch, "timed.nzb", None, &[&file]);
    let mut settings = test_settings(tmp.path(), vec![server_for(&mock, 2)]);
    settings.watch_folder = watch;
    settings.watch_enabled = true;
    settings.watch_interval_secs = 1;
    settings.watch_auto_start = true;
    let svc = open_service(tmp.path(), settings);
    let mut rx = svc.subscribe();
    wait_event(&mut rx, 10, |e| matches!(e, ServiceEvent::WatchImported(n) if n == &vec!["timed".to_string()])).await;
    wait_event(&mut rx, 20, |e| *e == ServiceEvent::QueueIdle).await;
    let jobs = svc.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, JobState::Done, "{:?}", jobs[0].error);
    // Later timer ticks do not import it again.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(svc.jobs().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_bookmarks_passwords_and_server_test() {
    let tmp = TempDir::new().unwrap();
    let mock = mock_for(&[], &[], 0).await;
    let svc = open_service(tmp.path(), test_settings(tmp.path(), vec![server_for(&mock, 3)]));

    let s = svc.settings();
    assert_eq!(s.first_usable_server().unwrap().port, mock.addr.port());

    let transcript = svc.test_server(server_for(&mock, 1)).await.unwrap();
    assert!(!transcript.contains(PASS), "{transcript}");
    assert!(transcript.contains("DATE"), "{transcript}");
    let mut bad = server_for(&mock, 1);
    bad.password = "nope".into();
    assert!(svc.test_server(bad).await.is_err());
    assert!(svc.test_server(ServerConfig::default()).await.is_err());
    assert!(!svc.logs().iter().any(|l| l.text.contains(PASS) || l.text.contains("nope")));

    let parent = svc
        .add_bookmark(NewBookmark { parent_id: None, name: "Linux".into(), newsgroup: None, color: Some("#336699".into()) })
        .unwrap();
    let child = svc
        .add_bookmark(NewBookmark {
            parent_id: Some(parent.id),
            name: "ISOs".into(),
            newsgroup: Some("alt.binaries.linux".into()),
            color: None,
        })
        .unwrap();
    let mut renamed = child.clone();
    renamed.name = "Images".into();
    svc.update_bookmark(&renamed).unwrap();
    assert_eq!(svc.bookmarks().iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), vec!["Linux", "Images"]);
    svc.remove_bookmark(parent.id).unwrap();
    assert!(svc.bookmarks().is_empty());
    assert!(svc.remove_bookmark(parent.id).is_err());

    let file = TestFile::random("pw", "pw.bin", 100, 1, 5);
    let job = svc.add_nzb(&write_nzb(&tmp.path().join("n"), "pw.nzb", None, &[&file])).unwrap();
    svc.set_job_password(job.id, Some("archive-secret".into())).unwrap();
    assert_eq!(svc.jobs()[0].password.as_deref(), Some("archive-secret"));
    svc.set_job_password(job.id, Some(String::new())).unwrap();
    assert_eq!(svc.jobs()[0].password, None);
    assert!(matches!(svc.set_job_password(424242, None), Err(ServiceError::NotFound(424242))));
    assert!(!svc.logs().iter().any(|l| l.text.contains("archive-secret")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_article_is_repaired_by_par2_and_files_can_be_deleted() {
    let tmp = TempDir::new().unwrap();
    let data = common::random_bytes(40 * 1024, 77);
    // PAR2 data for the payload, posted alongside it.
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("m.bin"), &data).unwrap();
    let written = newskite_par2::create(&src, &["m.bin"], 2048, 8, &src.join("m")).unwrap();
    let file = TestFile::new("r", "m.bin", data.clone(), 4);
    let par2: Vec<TestFile> = written
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            TestFile::new(&format!("p{i}"), &name, std::fs::read(p).unwrap(), 1)
        })
        .collect();
    let gone = file.articles[2].0.clone();
    let mut all: Vec<&TestFile> = vec![&file];
    all.extend(par2.iter());
    let mock = mock_for(&all, &[gone.as_str()], 0).await;
    let mut settings = test_settings(tmp.path(), vec![server_for(&mock, 2)]);
    settings.postprocess = newskite_types::PostProcessSettings::default();
    let svc = open_service(tmp.path(), settings);
    let job = svc.add_nzb(&write_nzb(&tmp.path().join("n"), "m.nzb", None, &all)).unwrap();
    let mut rx = svc.subscribe();

    assert!(svc.start());
    wait_idle(&svc, 30).await;
    let done = svc.jobs().remove(0);
    let logs: Vec<String> = svc.logs().into_iter().map(|l| l.text).collect();
    assert_eq!(done.state, JobState::Done, "{:?}\n{logs:#?}", done.error);
    assert_eq!(std::fs::read(job.dest_dir.join("m.bin")).unwrap(), data);
    assert!(!job.dest_dir.join("m.bin.nkpart").exists());
    // Leftovers are gone once verified.
    assert!(!job.dest_dir.join("m.par2").exists());
    assert!(logs.iter().any(|l| l.contains("PAR2 repaired 1 missing article")), "{logs:#?}");
    let events = drain(&mut rx);
    assert!(events.iter().any(|e| matches!(e, ServiceEvent::PostProgress(p) if p.stage == newskite_service::PostStage::Repair)));
    assert_eq!(svc.status().post, None);

    // Delete files: the folder goes, the job leaves the list.
    let deleted = svc.delete_job_files(job.id).unwrap();
    assert_eq!(deleted.files, 1);
    assert!(!job.dest_dir.exists());
    assert!(svc.jobs().is_empty());
    assert!(tmp.path().join("downloads").exists(), "the download folder itself stays");
}
