//! Regression tests for hostile or inconsistent articles, dead fallback servers and resume
//! identity (name collisions, numbered names).

mod common;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use common::*;
use newskite_engine::EngineError;
use newskite_nzb::yenc;

/// Total size of every regular file under `dir`.
fn dir_bytes(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum()
}

/// A port that refuses connections.
fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn obfuscate(nf: &mut newskite_nzb::nzb::NzbFile, tag: &str) {
    nf.subject = format!("\"{tag}\" yEnc (1/{})", nf.segments.len());
}

/// A valid yEnc article from a different post (huge `size=`, far `begin=`) must not be
/// written into the file or set its length; the segment counts as missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_article_with_far_offsets_is_rejected() {
    let src = TestFile::new("fa", "real.bin", random_bytes(40_000, 31), 4);
    let mut articles = articles_of([&src]);
    let foreign_id = src.articles[1].0.clone();
    let foreign = yenc::encode_article("other.bin", 1_000_000_000, 2, 100, 500_000_001, &random_bytes(10_000, 32), 128);
    articles.insert(foreign_id.clone(), foreign);
    let server = mock(articles, false).await;
    let dir = tempfile::tempdir().unwrap();

    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 1)], options(1)).await;
    match out.result {
        Err(EngineError::MissingArticles { ids }) => assert_eq!(ids, vec![foreign_id]),
        other => panic!("expected MissingArticles, got {other:?}"),
    }
    assert!(!dir.path().join("real.bin").exists(), "never presented as complete");
    assert!(dir_bytes(dir.path()) <= 40_000, "nothing written past the real file");
}

/// A plausible article from another post (different `size=`/`name=`) is rejected on the first
/// server and fetched from the next one, so the file is byte-exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_article_with_other_size_falls_over_to_next_server() {
    let src = TestFile::new("fb", "real.bin", random_bytes(40_000, 33), 4);
    let other = TestFile::new("ob", "other.bin", random_bytes(30_000, 34), 4);
    let mut bad = articles_of([&src]);
    bad.insert(src.articles[1].0.clone(), other.articles[1].1.clone());
    let a = mock(bad, false).await;
    let b = mock(articles_of([&src]), false).await;
    let dir = tempfile::tempdir().unwrap();

    let out = run(
        nzb_of(vec![src.nzb_file(None)]),
        dir.path(),
        vec![server_for(&a, false, 1), server_for(&b, false, 1)],
        options(1),
    )
    .await;
    let report = out.result.expect("the backup supplies the real article");
    assert_eq!(sha256(&std::fs::read(dir.path().join("real.bin")).unwrap()), sha256(&src.data));
    assert_eq!(report.files[0].bytes, 40_000);
    assert_eq!(b.body_requests(), 1, "only the foreign article is fetched again");
    assert!(logs(&out.events).iter().any(|l| l.contains("does not belong to this file")), "{:?}", logs(&out.events));
}

/// An article whose `size=` is far beyond what the NZB declares is rejected, so a 5-byte
/// download can never become a 1 TB file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_yenc_size_is_rejected() {
    let mut src = TestFile::new("big", "x.bin", random_bytes(5, 35), 1);
    src.articles[0].1 = yenc::encode_article("x.bin", 1_000_000_000_000, 1, 1, 1, &src.data, 128);
    let server = mock(articles_of([&src]), false).await;
    let dir = tempfile::tempdir().unwrap();
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 1)], options(1)).await;
    match out.result {
        Err(EngineError::MissingArticles { ids }) => assert_eq!(ids.len(), 1),
        other => panic!("expected MissingArticles, got {other:?}"),
    }
    assert!(dir_bytes(dir.path()) <= 5);
    assert!(logs(&out.events).iter().any(|l| l.contains("does not belong to this file")));
}

/// A huge `begin=` with a matching `size=` is rejected the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn huge_begin_offset_is_rejected() {
    let mut src = TestFile::new("far", "far.bin", random_bytes(5, 36), 1);
    let size = 1_000_000_000_000u64;
    src.articles[0].1 = yenc::encode_article("far.bin", size, 1, 1, size - 4, &src.data, 128);
    let server = mock(articles_of([&src]), false).await;
    let dir = tempfile::tempdir().unwrap();
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 1)], options(1)).await;
    assert!(matches!(out.result, Err(EngineError::MissingArticles { .. })), "{:?}", out.result);
    assert!(dir_bytes(dir.path()) <= 5);
}

/// NZBs without `bytes=` still download normally (the size bound falls back to a per-segment
/// allowance instead of rejecting everything).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nzb_without_declared_bytes_still_downloads() {
    let src = TestFile::new("nb", "nobytes.bin", random_bytes(60_000, 37), 3);
    let mut nf = src.nzb_file(None);
    for s in &mut nf.segments {
        s.bytes = 0;
    }
    let server = mock(articles_of([&src]), false).await;
    let dir = tempfile::tempdir().unwrap();
    let out = run(nzb_of(vec![nf]), dir.path(), vec![server_for(&server, false, 2)], options(2)).await;
    out.result.expect("download succeeds");
    assert_eq!(sha256(&std::fs::read(dir.path().join("nobytes.bin")).unwrap()), sha256(&src.data));
}

/// The primary lacks one article and the only fallback is unreachable: the job must report
/// the missing message id, not abort with a connection error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dead_backup_reports_missing_article() {
    let src = TestFile::new("db", "dead.bin", random_bytes(60_000, 38), 3);
    let missing_id = src.articles[1].0.clone();
    let mut articles = articles_of([&src]);
    articles.remove(&missing_id);
    let primary = mock(articles, false).await;
    let backup = newskite_types::ServerConfig {
        host: "127.0.0.1".into(),
        port: closed_port(),
        tls: false,
        connections: 2,
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(2);
    opts.retries = 1;
    let started = Instant::now();
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&primary, false, 2), backup], opts).await;
    match out.result {
        Err(EngineError::MissingArticles { ids }) => assert_eq!(ids, vec![missing_id.clone()]),
        other => panic!("expected MissingArticles, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(dir.path().join("dead.bin.nkpart").exists(), "partial kept");
    assert_eq!(primary.body_requests(), 3);
}

/// Two NZB files whose yEnc names sanitize to the same name, obfuscated subjects. Run 1
/// completed X as movie.avi; Y was interrupted. Y decodes first in run 2 and must not be
/// taken for X.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_duplicate_yenc_name_skips_the_right_file() {
    let x = TestFile::new("x", "CD1/movie.avi", random_bytes(20_000, 39), 2);
    let y = TestFile::new("y", "CD2/movie.avi", random_bytes(25_000, 40), 2);
    let mut nx = x.nzb_file(None);
    obfuscate(&mut nx, "a1b2c3");
    let mut ny = y.nzb_file(None);
    obfuscate(&mut ny, "d4e5f6");
    let server = mock(articles_of([&x, &y]), false).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("movie.avi"), &x.data).unwrap();
    std::fs::write(dir.path().join("movie (2).avi.nkpart"), &y.data[..10_000]).unwrap();

    let mut opts = options(1);
    opts.skip_files = HashSet::from(["movie.avi".to_string()]);
    let out = run(nzb_of(vec![ny, nx]), dir.path(), vec![server_for(&server, false, 1)], opts).await;
    let report = out.result.expect("resume succeeds");
    assert_eq!(report.files[0].name, "movie (2).avi", "Y is downloaded");
    assert_eq!(report.files[1].name, "movie.avi", "X is the completed file");
    assert_eq!(sha256(&std::fs::read(dir.path().join("movie (2).avi")).unwrap()), sha256(&y.data));
    assert_eq!(sha256(&std::fs::read(dir.path().join("movie.avi")).unwrap()), sha256(&x.data));
    assert_eq!(listing(dir.path()), vec!["movie (2).avi", "movie.avi"]);
    assert_eq!(server.body_requests(), 3, "all of Y plus one part of X to identify it");
}

/// Same collision with readable subjects: the NZB-derived label of X is "movie.avi", but in
/// run 1 Y won that name. X must not be skipped by its label.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_colliding_labels_are_verified_by_content() {
    let x = TestFile::new("lx", "movie.avi", random_bytes(20_000, 41), 2);
    let y = TestFile::new("ly", "movie.avi", random_bytes(20_000, 42), 2);
    let server = mock(articles_of([&x, &y]), false).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("movie.avi"), &y.data).unwrap();

    let mut opts = options(1);
    opts.skip_files = HashSet::from(["movie.avi".to_string()]);
    let out = run(nzb_of(vec![x.nzb_file(None), y.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 1)], opts)
        .await;
    let report = out.result.expect("resume succeeds");
    assert_eq!(report.files[0].name, "movie (2).avi");
    assert_eq!(report.files[1].name, "movie.avi");
    assert_eq!(sha256(&std::fs::read(dir.path().join("movie (2).avi")).unwrap()), sha256(&x.data));
    assert_eq!(sha256(&std::fs::read(dir.path().join("movie.avi")).unwrap()), sha256(&y.data));
    assert_eq!(server.body_requests(), 3);
}

/// Run 1 saved the file as "x (2).bin" because an unrelated x.bin existed. Resume must
/// recognize it instead of downloading a third copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_recognizes_numbered_output_name() {
    let src = TestFile::new("nn", "x.bin", random_bytes(30_000, 43), 3);
    let server = mock(articles_of([&src]), false).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.bin"), b"unrelated older file").unwrap();
    std::fs::write(dir.path().join("x (2).bin"), &src.data).unwrap();

    let mut opts = options(1);
    opts.skip_files = HashSet::from(["x (2).bin".to_string()]);
    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 1)], opts).await;
    let report = out.result.expect("resume succeeds");
    assert_eq!(report.files[0].name, "x (2).bin");
    assert_eq!(report.files[0].bytes, 30_000);
    assert_eq!(server.body_requests(), 1, "one part to identify the file, nothing more");
    assert_eq!(listing(dir.path()), vec!["x (2).bin", "x.bin"]);
}

/// A recorded name whose content does not match (for example a truncated file) is not
/// skipped; the file is downloaded again under a free name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_does_not_skip_mismatched_content() {
    let src = TestFile::new("mm", "real.bin", random_bytes(30_000, 44), 3);
    let mut nf = src.nzb_file(None);
    obfuscate(&mut nf, "zzz");
    let server = mock(articles_of([&src]), false).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("real.bin"), vec![0u8; 30_000]).unwrap();

    let mut opts = options(1);
    opts.skip_files = HashSet::from(["real.bin".to_string()]);
    let out = run(nzb_of(vec![nf]), dir.path(), vec![server_for(&server, false, 1)], opts).await;
    let report = out.result.expect("download succeeds");
    assert_eq!(report.files[0].name, "real (2).bin");
    assert_eq!(sha256(&std::fs::read(dir.path().join("real (2).bin")).unwrap()), sha256(&src.data));
}

/// An obfuscated post whose output post-processing moved to its real name: no recorded name
/// points at it any more, but its content does, so a retry fetches one article to find it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_finds_a_file_renamed_since_by_its_content() {
    let src = TestFile::new("rn", "Y9zTHX09lDgCv9Ej", random_bytes(30_000, 45), 3);
    let server = mock(articles_of([&src]), false).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("The.Movie.mkv"), &src.data).unwrap();

    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 1)], options(1)).await;
    let report = out.result.expect("resume succeeds");
    assert_eq!(report.files[0].name, "The.Movie.mkv");
    assert_eq!(report.files[0].bytes, 30_000);
    assert_eq!(server.body_requests(), 1, "one part to identify the file, nothing more");
    assert_eq!(listing(dir.path()), vec!["The.Movie.mkv"]);
}

/// The damaged original a PAR2 repair keeps as `name.1` starts like the file and has its
/// length, but is never taken for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_never_takes_a_repair_backup_for_the_file() {
    let src = TestFile::new("bk", "Qp4r", random_bytes(30_000, 46), 3);
    let server = mock(articles_of([&src]), false).await;
    let dir = tempfile::tempdir().unwrap();
    let mut damaged = src.data.clone();
    damaged[25_000..].fill(0);
    std::fs::write(dir.path().join("movie.mkv"), b"repaired copy, moved away since").unwrap();
    std::fs::write(dir.path().join("movie.mkv.1"), &damaged).unwrap();

    let out = run(nzb_of(vec![src.nzb_file(None)]), dir.path(), vec![server_for(&server, false, 1)], options(1)).await;
    let report = out.result.expect("download succeeds");
    assert_eq!(report.files[0].name, "Qp4r");
    assert_eq!(server.body_requests(), 3, "downloaded again");
    assert_eq!(sha256(&std::fs::read(dir.path().join("Qp4r")).unwrap()), sha256(&src.data));
}
