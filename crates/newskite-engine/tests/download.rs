//! End-to-end engine tests over real loopback TCP against the testkit mock server.

mod common;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use common::*;
use newskite_engine::EngineError;
use newskite_nntp::testkit::{MockConfig, MockServer};
use newskite_types::EngineEvent;
use tokio_util::sync::CancellationToken;

const MIB: usize = 1024 * 1024;

/// Acceptance (outline §12): 2 MiB random file, 8 yEnc parts, auth, real TCP, SHA-256 equal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptance_two_mib_eight_parts_with_auth() {
    let src = TestFile::new("acc", "payload.bin", random_bytes(2 * MIB, 0xACCE_7713), 8);
    assert_eq!(src.articles.len(), 8);
    let server = mock(articles_of([&src]), true).await;
    let dir = tempfile::tempdir().unwrap();

    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, true, 4)], options(4)).await;
    let report = out.result.expect("download succeeds");

    let path = dir.path().join("payload.bin");
    let written = std::fs::read(&path).unwrap();
    assert_eq!(written.len(), src.data.len());
    assert_eq!(sha256(&written), sha256(&src.data), "byte-exact assembly");
    assert_eq!(listing(dir.path()), vec!["payload.bin"], "no .nkpart left behind");

    assert_eq!(report.files.len(), 1);
    assert_eq!(report.files[0].name, "payload.bin");
    assert_eq!(report.files[0].path, path);
    assert_eq!(report.files[0].bytes, (2 * MIB) as u64);
    assert!(report.files[0].missing.is_empty());
    assert_eq!(report.files[0].crc_errors, 0);
    assert_eq!(report.bytes_written, (2 * MIB) as u64);
    assert_eq!(out.progress.bytes_down(), (2 * MIB) as u64);
    assert_eq!(out.progress.segments_done(), 8);
    assert_eq!(server.body_requests(), 8);
    assert!(server.connections_accepted() <= 4, "reuses at most 4 connections");

    // Event order: Layout, FileStarted, 8 x SegmentDone (done reaching 8), FileDone.
    let file_events: Vec<&EngineEvent> =
        out.events.iter().filter(|e| !matches!(e, EngineEvent::Log(_) | EngineEvent::Warn(_) | EngineEvent::Repair(_))).collect();
    assert!(matches!(file_events[0], EngineEvent::Layout { files }
        if files.len() == 1 && files[0].segments == 8 && !files[0].complete));
    assert!(matches!(file_events[1], EngineEvent::FileStarted { name, segments: 8 } if name == "payload.bin"));
    let (dones, mut places): (Vec<usize>, Vec<(usize, usize)>) = file_events[2..10]
        .iter()
        .map(|e| match e {
            EngineEvent::SegmentDone { done, total: 8, file, index, .. } => (*done, (*file, *index)),
            other => panic!("unexpected {other:?}"),
        })
        .unzip();
    assert_eq!(dones, (1..=8).collect::<Vec<_>>());
    // Every article reported once, at its own place in the layout.
    places.sort();
    assert_eq!(places, (0..8).map(|i| (0, i)).collect::<Vec<_>>());
    assert!(matches!(file_events[10], EngineEvent::FileDone { bytes, .. } if *bytes == (2 * MIB) as u64));
    assert_eq!(file_events.len(), 11);
    // No PAR2 in the job: once the file's first article showed it is not PAR2, there is
    // nothing to repair with.
    let last_outlook = out.events.iter().rev().find_map(|e| match e {
        EngineEvent::Repair(o) => Some(*o),
        _ => None,
    });
    assert!(last_outlook.is_some_and(|o| o.no_par2 && !o.known));

    let logs = logs(&out.events);
    assert!(logs.iter().any(|l| l.starts_with("Starting download: 1 file(s)")));
    assert!(logs.iter().any(|l| l.starts_with("Completed payload.bin")));
    assert!(logs.iter().all(|l| !l.contains(PASS)), "password never logged");
}

/// Three files, segments mapped in reverse offset order and served with delay so parts land
/// out of order; every file must still be byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_file_out_of_order() {
    let files = [
        TestFile::new("a", "alpha.bin", random_bytes(300_000, 1), 5),
        TestFile::new("b", "beta.bin", random_bytes(123_457, 2), 4),
        TestFile::new("c", "gamma.dat", random_bytes(64_001, 3), 3),
    ];
    let server = MockServer::start(MockConfig { articles: articles_of(&files), delay_ms: 30, ..Default::default() }).await;
    let nzb = nzb_of(
        files
            .iter()
            .map(|f| {
                let rev: Vec<usize> = (0..f.articles.len()).rev().collect();
                f.nzb_file(Some(&rev))
            })
            .collect(),
    );
    let dir = tempfile::tempdir().unwrap();
    let out = run(nzb, dir.path(), vec![server_for(&server, false, 6)], options(6)).await;
    let report = out.result.expect("download succeeds");
    for f in &files {
        let got = std::fs::read(dir.path().join(&f.name)).unwrap();
        assert_eq!(sha256(&got), sha256(&f.data), "{}", f.name);
    }
    assert_eq!(listing(dir.path()), vec!["alpha.bin", "beta.bin", "gamma.dat"]);
    let names: Vec<&str> = report.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["alpha.bin", "beta.bin", "gamma.dat"]);
    let done = out.events.iter().filter(|e| matches!(e, EngineEvent::FileDone { .. })).count();
    assert_eq!(done, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_article_fails_job_and_keeps_part_file() {
    let src = TestFile::new("m", "movie.mkv", random_bytes(200_000, 9), 4);
    let missing_id = src.articles[2].0.clone();
    let mut articles = articles_of([&src]);
    articles.remove(&missing_id);
    let server = mock(articles, false).await;
    let dir = tempfile::tempdir().unwrap();

    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 2)], options(2)).await;
    match out.result {
        Err(EngineError::MissingArticles { ids }) => assert_eq!(ids, vec![missing_id.clone()]),
        other => panic!("expected MissingArticles, got {other:?}"),
    }
    assert!(!dir.path().join("movie.mkv").exists(), "never presented as complete");
    assert!(dir.path().join("movie.mkv.nkpart").exists(), "partial kept");
    assert!(out.events.iter().any(
        |e| matches!(e, EngineEvent::FileFailed { name, missing } if name == "movie.mkv" && missing == &vec![missing_id.clone()])
    ));
    assert!(!out.events.iter().any(|e| matches!(e, EngineEvent::FileDone { .. })));
    // The missing article is reported at its place, and only there.
    let missing_at: Vec<usize> = out
        .events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::SegmentMissing { file: 0, index } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(missing_at.len(), 1);
    assert!(!out.events.iter().any(|e| matches!(e, EngineEvent::SegmentDone { index, .. } if *index == missing_at[0])));
    let logs = logs(&out.events);
    assert!(logs.iter().any(|l| l.contains("1 missing article(s)") && l.contains(&missing_id)));
    // Missing on the only server: asked exactly once.
    assert_eq!(server.body_requests(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_article_missing_reports_without_creating_files() {
    let src = TestFile::new("gone", "gone.bin", random_bytes(10_000, 4), 2);
    let server = mock(HashMap::new(), false).await;
    let dir = tempfile::tempdir().unwrap();
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 2)], options(2)).await;
    match out.result {
        Err(EngineError::MissingArticles { ids }) => assert_eq!(ids.len(), 2),
        other => panic!("expected MissingArticles, got {other:?}"),
    }
    assert!(listing(dir.path()).is_empty());
    let started = out.events.iter().position(|e| matches!(e, EngineEvent::FileStarted { .. })).unwrap();
    let failed = out.events.iter().position(|e| matches!(e, EngineEvent::FileFailed { .. })).unwrap();
    assert!(started < failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failover_to_second_server_for_missing_article() {
    let src = TestFile::new("fo", "fail.over", random_bytes(150_000, 5), 3);
    let lacking = src.articles[1].0.clone();
    let mut partial = articles_of([&src]);
    partial.remove(&lacking);
    let a = mock(partial, true).await;
    let b = mock(articles_of([&src]), false).await;
    let dir = tempfile::tempdir().unwrap();

    let out = run(
        nzb_of(vec![src.nzb_file(None)]),
        dir.path(),
        vec![server_for(&a, true, 2), server_for(&b, false, 2)],
        options(2),
    )
    .await;
    out.result.expect("fail-over succeeds");
    let got = std::fs::read(dir.path().join("fail.over")).unwrap();
    assert_eq!(sha256(&got), sha256(&src.data));
    assert_eq!(a.body_requests(), 3);
    assert_eq!(b.body_requests(), 1, "only the lacking article comes from B");
    let logs = logs(&out.events);
    assert!(logs.iter().any(|l| l.contains(&lacking) && l.contains("not on")), "{logs:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_auth_on_only_server_fails_fast() {
    let src = TestFile::new("auth", "secret.bin", random_bytes(50_000, 6), 4);
    let server = mock(articles_of([&src]), true).await;
    let mut cfg = server_for(&server, true, 4);
    cfg.password = "wrong-password".into();
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![cfg], options(4)).await;
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    match out.result {
        Err(EngineError::AuthFailed(msg)) => assert!(!msg.contains("wrong-password")),
        other => panic!("expected AuthFailed, got {other:?}"),
    }
    assert_eq!(server.body_requests(), 0, "no work after auth failure");
    assert!(logs(&out.events).iter().all(|l| !l.contains("wrong-password")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_auth_on_first_server_uses_second() {
    let src = TestFile::new("auth2", "second.bin", random_bytes(40_000, 7), 2);
    let a = mock(articles_of([&src]), true).await;
    let b = mock(articles_of([&src]), false).await;
    let mut bad = server_for(&a, true, 2);
    bad.password = "nope".into();
    let dir = tempfile::tempdir().unwrap();
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![bad, server_for(&b, false, 2)], options(2)).await;
    out.result.expect("second server used");
    assert_eq!(a.body_requests(), 0);
    assert_eq!(b.body_requests(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_connection_failures_are_retried() {
    let src = TestFile::new("tr", "flaky.bin", random_bytes(100_000, 8), 4);
    let server = MockServer::start(MockConfig {
        articles: articles_of([&src]),
        fail_first_n_connections: 2,
        ..Default::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 4)], options(4)).await;
    out.result.expect("retries recover");
    let got = std::fs::read(dir.path().join("flaky.bin")).unwrap();
    assert_eq!(sha256(&got), sha256(&src.data));
    assert!(server.connections_accepted() >= 3);
    assert!(logs(&out.events).iter().any(|l| l.contains("retrying in")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unreachable_server_gives_connect_error_after_retries() {
    // Bind then drop a listener to get a port that refuses connections.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let src = TestFile::new("down", "down.bin", random_bytes(1000, 10), 1);
    let cfg = newskite_types::ServerConfig {
        host: "127.0.0.1".into(),
        port,
        tls: false,
        connections: 2,
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(2);
    opts.retries = 1;
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![cfg], opts).await;
    assert!(matches!(out.result, Err(EngineError::Connect(_))), "{:?}", out.result);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_mid_download_returns_promptly() {
    let src = TestFile::new("cx", "slow.bin", random_bytes(400_000, 11), 8);
    let server =
        MockServer::start(MockConfig { articles: articles_of([&src]), delay_ms: 400, ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(600)).await;
        c2.cancel();
    });
    let started = Instant::now();
    let out = run_with_cancel(
        nzb_of(vec![src.nzb_file(None)]),
        dir.path(),
        vec![server_for(&server, false, 2)],
        options(2),
        cancel,
    )
    .await;
    let elapsed = started.elapsed();
    assert!(matches!(out.result, Err(EngineError::Cancelled)), "{:?}", out.result);
    assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
    assert!(dir.path().join("slow.bin.nkpart").exists(), "partial kept for resume");
    assert!(!dir.path().join("slow.bin").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn already_cancelled_token_does_nothing() {
    let src = TestFile::new("pre", "pre.bin", random_bytes(1000, 12), 1);
    let server = mock(articles_of([&src]), false).await;
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let out = run_with_cancel(
        nzb_of(vec![src.nzb_file(None)]),
        dir.path(),
        vec![server_for(&server, false, 1)],
        options(1),
        cancel,
    )
    .await;
    assert!(matches!(out.result, Err(EngineError::Cancelled)));
    assert_eq!(server.body_requests(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_skips_completed_files() {
    let done = TestFile::new("s1", "done.bin", random_bytes(80_000, 13), 4);
    let todo = TestFile::new("s2", "todo.bin", random_bytes(60_000, 14), 3);
    let server = mock(articles_of([&done, &todo]), false).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("done.bin"), &done.data).unwrap();

    let mut opts = options(3);
    opts.skip_files = HashSet::from(["done.bin".to_string()]);
    let out = run(nzb_of(vec![done.nzb_file(None), todo.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 3)], opts).await;
    let report = out.result.expect("resume succeeds");
    assert_eq!(server.body_requests(), 3, "only todo.bin fetched");
    assert_eq!(report.files[0].name, "done.bin");
    assert_eq!(report.files[0].bytes, 80_000);
    assert_eq!(sha256(&std::fs::read(dir.path().join("todo.bin")).unwrap()), sha256(&todo.data));
    assert_eq!(listing(dir.path()), vec!["done.bin", "todo.bin"]);
    assert!(logs(&out.events).iter().any(|l| l.contains("done.bin: already complete")));
    assert!(!out.events.iter().any(|e| matches!(e, EngineEvent::FileStarted { name, .. } if name == "done.bin")));
    // The layout says which file is already complete.
    assert!(out.events.iter().any(|e| matches!(e, EngineEvent::Layout { files }
        if files.iter().map(|f| (f.segments, f.complete)).collect::<Vec<_>>() == vec![(4, true), (3, false)])));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_skips_when_yenc_name_differs_from_subject() {
    // The subject says "label.bin" but the yEnc name is "real.bin", which is complete.
    let src = TestFile::new("y", "real.bin", random_bytes(40_000, 15), 4);
    let mut nf = src.nzb_file(None);
    nf.subject = "\"label.bin\" yEnc (1/4)".into();
    let server = mock(articles_of([&src]), false).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("real.bin"), &src.data).unwrap();
    let mut opts = options(1);
    opts.skip_files = HashSet::from(["real.bin".to_string()]);
    let out = run(nzb_of(vec![nf]), dir.path(), vec![server_for(&server, false, 1)], opts).await;
    let report = out.result.expect("skip after first part");
    assert_eq!(server.body_requests(), 1, "only the first part was needed to learn the name");
    assert_eq!(report.files[0].name, "real.bin");
    assert_eq!(listing(dir.path()), vec!["real.bin"]);
    // Found complete by content: every article of it counts as done.
    assert!(out.events.iter().any(|e| matches!(e, EngineEvent::FileComplete { file: 0 })));
}

/// First articles are requested ahead before their files' outputs are decided. Files that
/// then turn out complete from an earlier run cost exactly their first article, and every
/// reply still reaches the segment it belongs to (the last file downloads intact).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_by_first_article_with_pipelined_first_articles() {
    let done: Vec<TestFile> =
        (0..4).map(|i| TestFile::new(&format!("r{i}"), &format!("real{i}.bin"), random_bytes(30_000, 40 + i), 3)).collect();
    let todo = TestFile::new("rt", "todo.bin", random_bytes(30_000, 49), 3);
    let server = mock(articles_of(done.iter().chain([&todo])), false).await;
    let dir = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    let mut opts = options(1);
    for (i, f) in done.iter().enumerate() {
        std::fs::write(dir.path().join(&f.name), &f.data).unwrap();
        opts.skip_files.insert(f.name.clone());
        let mut nf = f.nzb_file(None);
        nf.subject = format!("\"label{i}.bin\" yEnc (1/3)");
        files.push(nf);
    }
    files.push(todo.nzb_file(None));
    let out = run(nzb_of(files), dir.path(), vec![server_for(&server, false, 1)], opts).await;
    let report = out.result.expect("resume succeeds");
    assert_eq!(server.body_requests(), 4 + 3, "one article per complete file, then todo.bin");
    assert_eq!(report.files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["real0.bin", "real1.bin", "real2.bin", "real3.bin", "todo.bin"]);
    assert_eq!(sha256(&std::fs::read(dir.path().join("todo.bin")).unwrap()), sha256(&todo.data));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn existing_file_not_in_skip_list_gets_numbered_name() {
    let src = TestFile::new("ex", "clash.bin", random_bytes(30_000, 16), 2);
    let server = mock(articles_of([&src]), false).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("clash.bin"), b"older unrelated file").unwrap();
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 2)], options(2)).await;
    let report = out.result.unwrap();
    assert_eq!(report.files[0].name, "clash (2).bin");
    assert_eq!(std::fs::read(dir.path().join("clash.bin")).unwrap(), b"older unrelated file");
    assert_eq!(sha256(&std::fs::read(dir.path().join("clash (2).bin")).unwrap()), sha256(&src.data));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_files_with_same_name_are_deduplicated() {
    let one = TestFile::new("d1", "same.bin", random_bytes(20_000, 17), 2);
    let two = TestFile::new("d2", "same.bin", random_bytes(25_000, 18), 2);
    let server = mock(articles_of([&one, &two]), false).await;
    let dir = tempfile::tempdir().unwrap();
    let out = run(nzb_of(vec![one.nzb_file(None), two.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 2)], options(2)).await;
    let report = out.result.unwrap();
    assert_eq!(listing(dir.path()), vec!["same (2).bin", "same.bin"]);
    for (f, r) in [&one, &two].iter().zip(&report.files) {
        assert_eq!(sha256(&std::fs::read(&r.path).unwrap()), sha256(&f.data));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn path_traversal_in_yenc_name_stays_inside_dest() {
    let src = TestFile::new("pt", "../../escape/evil.bin", random_bytes(20_000, 19), 2);
    let server = mock(articles_of([&src]), false).await;
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("dest");
    let out = run(nzb_of(vec![src.nzb_file(None)]), &dest, vec![server_for(&server, false, 2)], options(2)).await;
    let report = out.result.unwrap();
    assert_eq!(report.files[0].path.parent().unwrap(), dest.as_path());
    assert_eq!(listing(&dest), vec!["evil.bin"]);
    assert_eq!(listing(root.path()), vec!["dest"], "nothing written outside dest_dir");
    assert_eq!(sha256(&std::fs::read(dest.join("evil.bin")).unwrap()), sha256(&src.data));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn speed_limit_is_respected() {
    let src = TestFile::new("sl", "limited.bin", random_bytes(512 * 1024, 20), 8);
    let server = mock(articles_of([&src]), false).await;
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(4);
    opts.speed_limit = newskite_engine::SpeedLimit::new(256 * 1024);
    let started = Instant::now();
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 4)], opts).await;
    let elapsed = started.elapsed();
    out.result.unwrap();
    assert!(elapsed >= Duration::from_millis(1500), "too fast: {elapsed:?}");
    assert!(elapsed < Duration::from_secs(6), "too slow: {elapsed:?}");
    assert_eq!(sha256(&std::fs::read(dir.path().join("limited.bin")).unwrap()), sha256(&src.data));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_article_is_missing_and_crc_mismatch_is_counted() {
    let src = TestFile::new("cr", "crc.bin", random_bytes(30_000, 21), 3);
    let mut articles = articles_of([&src]);
    // Article 1: not yEnc at all -> undecodable -> missing.
    articles.insert(src.articles[0].0.clone(), b"this is not a yEnc article\r\n".to_vec());
    // Article 2: one changed data byte -> CRC mismatch, data kept.
    corrupt_one_data_byte(articles.get_mut(&src.articles[1].0).unwrap());
    let server = mock(articles, false).await;
    let dir = tempfile::tempdir().unwrap();
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 1)], options(1)).await;
    match out.result {
        Err(EngineError::MissingArticles { ids }) => assert_eq!(ids, vec![src.articles[0].0.clone()]),
        other => panic!("expected MissingArticles, got {other:?}"),
    }
    assert!(logs(&out.events).iter().any(|l| l.contains("could not be decoded")));
    assert!(dir.path().join("crc.bin.nkpart").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crc_mismatch_only_still_completes() {
    let src = TestFile::new("cr2", "crc2.bin", random_bytes(30_000, 22), 3);
    let mut articles = articles_of([&src]);
    corrupt_one_data_byte(articles.get_mut(&src.articles[2].0).unwrap());
    let server = mock(articles, false).await;
    let dir = tempfile::tempdir().unwrap();
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 1)], options(1)).await;
    let report = out.result.expect("CRC errors keep data");
    assert_eq!(report.files[0].crc_errors, 1);
    assert!(logs(&out.events).iter().any(|l| l.contains("CRC")));
}

#[tokio::test]
async fn no_usable_server_is_an_error() {
    let src = TestFile::new("ns", "x.bin", vec![1, 2, 3], 1);
    let dir = tempfile::tempdir().unwrap();
    let disabled = newskite_types::ServerConfig { host: "h".into(), enabled: false, ..Default::default() };
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![disabled], options(1)).await;
    assert!(matches!(out.result, Err(EngineError::NoServer)));
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), Vec::new(), options(1)).await;
    assert!(matches!(out.result, Err(EngineError::NoServer)));
}

/// One connection, six 10 KB articles and a link that takes ~100 ms per body, downloaded
/// once with 100 ms of simulated one-way latency per reply and once without. Fetched one
/// after another, every article pays the latency (at least 600 ms more in all); with the
/// next request sent while the current body streams, it is paid only for the first article
/// of a file whose output is decided (~200 ms more). Comparing the two runs cancels out
/// what the machine adds (timer granularity, a busy CI runner), which a fixed limit on the
/// latency run alone did not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipelining_hides_reply_latency() {
    async fn download(latency_ms: u64) -> Duration {
        let src = TestFile::new("pl", "pipe.bin", random_bytes(60_000, 24), 6);
        let server = MockServer::start(MockConfig {
            articles: articles_of([&src]),
            latency_ms,
            bandwidth_bps: 100_000,
            ..Default::default()
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 1)], options(1)).await;
        let elapsed = started.elapsed();
        out.result.expect("download succeeds");
        assert_eq!(sha256(&std::fs::read(dir.path().join("pipe.bin")).unwrap()), sha256(&src.data));
        assert_eq!(server.body_requests(), 6);
        elapsed
    }
    // One stalled run (a slow flush, a descheduled runner) is not the engine's doing:
    // the pair is timed up to three times and the best one counts.
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let without = download(0).await;
        let with = download(100).await;
        best = best.min(with.saturating_sub(without));
        if best < Duration::from_millis(400) {
            return;
        }
    }
    panic!("latency added {best:?} at best: articles were not pipelined");
}

/// The same comparison for six files of two 80 KB articles, on a link that takes ~200 ms
/// per body and 150 ms of latency. The queue starts with every file's first article,
/// before any of those files has its output decided. Requested one after another, each
/// of those first articles leaves the link idle for part of the latency (~1 s more in
/// all); requested ahead, only the first article pays it (~0.3 s).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipelining_covers_the_first_article_of_every_file() {
    async fn download(latency_ms: u64) -> Duration {
        let srcs: Vec<TestFile> = (0..6)
            .map(|i| TestFile::new(&format!("pf{i}"), &format!("first{i}.bin"), random_bytes(160_000, 60 + i), 2))
            .collect();
        let server = MockServer::start(MockConfig {
            articles: articles_of(srcs.iter()),
            latency_ms,
            bandwidth_bps: 400_000,
            ..Default::default()
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let nzb = nzb_of(srcs.iter().map(|s| s.nzb_file(None)).collect());
        let out = run(nzb, dir.path(), vec![server_for(&server, false, 1)], options(1)).await;
        let elapsed = started.elapsed();
        out.result.expect("download succeeds");
        for s in &srcs {
            assert_eq!(sha256(&std::fs::read(dir.path().join(&s.name)).unwrap()), sha256(&s.data));
        }
        assert_eq!(server.body_requests(), 12);
        elapsed
    }
    // The best of up to three timed pairs counts (see `pipelining_hides_reply_latency`).
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let without = download(0).await;
        let with = download(150).await;
        best = best.min(with.saturating_sub(without));
        if best < Duration::from_millis(600) {
            return;
        }
    }
    panic!("latency added {best:?} at best: first articles were not pipelined");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_connection_limit_is_respected() {
    let src = TestFile::new("cl", "limit.bin", random_bytes(200_000, 23), 10);
    let server = MockServer::start(MockConfig { articles: articles_of([&src]), delay_ms: 20, ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    // Ask for 8 connections but the server allows 2.
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 2)], options(8)).await;
    out.result.unwrap();
    assert!(server.connections_accepted() <= 2, "{}", server.connections_accepted());
}
