//! The repair outlook end to end: real PAR2 files posted next to their data on the mock
//! server, articles removed, and the outlook and early stop checked.

mod common;

use common::*;
use newskite_engine::EngineError;
use newskite_types::{EngineEvent, RepairOutlook};

/// 400 000 bytes in 8 articles of 50 000; PAR2 slices of 25 000 (16 of them), 3 blocks.
const DATA: usize = 400_000;
const SLICE: u64 = 25_000;

struct Post {
    files: Vec<TestFile>,
}

/// The data file, its PAR2 index and one volume, under real or random names.
fn post(obfuscated: bool, data_parts: usize) -> Post {
    let src = tempfile::tempdir().unwrap();
    let data = random_bytes(DATA, 0x5EED);
    std::fs::write(src.path().join("data.rar"), &data).unwrap();
    let par2 = newskite_par2::create(src.path(), &["data.rar"], SLICE, 3, &src.path().join("data")).unwrap();
    let index = std::fs::read(&par2[0]).unwrap();
    let vol = std::fs::read(&par2[1]).unwrap();
    let name = |real: &str, fake: &str| if obfuscated { fake.to_string() } else { real.to_string() };
    Post {
        files: vec![
            TestFile::new("d", &name("data.rar", "a8f3c1"), data, data_parts),
            TestFile::new("i", &name("data.par2", "77e0b2"), index, 1),
            TestFile::new("v", &name("data.vol00+03.par2", "c91d4e"), vol, 2),
        ],
    }
}

fn outlooks(events: &[EngineEvent]) -> Vec<RepairOutlook> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Repair(o) => Some(*o),
            _ => None,
        })
        .collect()
}

async fn download(p: &Post, drop: &[usize], stop: bool) -> (Outcome, usize) {
    let mut articles = articles_of(&p.files);
    for &i in drop {
        articles.remove(&p.files[0].articles[i].0);
    }
    let server = mock(articles, false).await;
    let dir = tempfile::tempdir().unwrap();
    let nzb = nzb_of(p.files.iter().map(|f| f.nzb_file(None)).collect());
    let mut opts = options(1);
    opts.stop_unrepairable = stop;
    let out = run(nzb, dir.path(), vec![server_for(&server, false, 1)], opts).await;
    (out, server.body_requests())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repairable_damage_is_measured_exactly_and_the_job_runs_to_the_end() {
    let p = post(false, 8);
    // Article 3 holds bytes 100 000..150 000: slices 4 and 5.
    let (out, requests) = download(&p, &[2], true).await;
    assert!(matches!(out.result, Err(EngineError::MissingArticles { ref ids }) if ids.len() == 1));
    assert_eq!(requests, 11, "every article asked for");
    let last = *outlooks(&out.events).last().unwrap();
    assert_eq!(last, RepairOutlook { known: true, no_par2: false, blocks: 16, needed: 2, posted: 3, available: 3, exact: true });
    assert!(logs(&out.events).iter().any(|l| l.starts_with("PAR2: 16 slice(s) protected")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unrepairable_damage_stops_the_download_early() {
    // 40 articles of 10 000 bytes: articles 2, 4, 7 and 9 each sit in one slice of their
    // own (0 to 3), one more than the 3 blocks posted.
    let p = post(false, 40);
    let (out, requests) = download(&p, &[1, 3, 6, 8], true).await;
    match out.result {
        Err(EngineError::Unrepairable { needed, available, missing }) => {
            assert_eq!((needed, available, missing), (4, 3, 4));
        }
        other => panic!("expected Unrepairable, got {other:?}"),
    }
    assert!(requests < 43, "stopped before the end ({requests} of 43 asked for)");
    assert!(outlooks(&out.events).last().unwrap().hopeless());
    assert!(logs(&out.events).iter().any(|l| l.starts_with("Stopping: the damage needs at least 4 recovery block(s)")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_option_an_unrepairable_job_downloads_everything() {
    let p = post(false, 40);
    let (out, requests) = download(&p, &[1, 3, 6, 8], false).await;
    assert!(matches!(out.result, Err(EngineError::MissingArticles { ref ids }) if ids.len() == 4));
    assert_eq!(requests, 43);
    assert!(outlooks(&out.events).last().unwrap().hopeless());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn obfuscated_names_are_identified_by_their_first_bytes() {
    // 20 articles of 20 000 bytes (the first must hold 16 KiB to be hashed): articles 2, 5
    // and 7 touch slices 0-1, 3 and 4-5.
    let p = post(true, 20);
    let (out, requests) = download(&p, &[1, 4, 6], true).await;
    assert!(matches!(out.result, Err(EngineError::Unrepairable { needed: 5, available: 3, missing: 3 })), "{:?}", out.result);
    assert!(requests < 23);
    let p = post(true, 8);
    let (out, _) = download(&p, &[2], true).await;
    assert!(matches!(out.result, Err(EngineError::MissingArticles { .. })));
    let last = *outlooks(&out.events).last().unwrap();
    assert_eq!((last.needed, last.posted, last.available, last.exact), (2, 3, 3, true));
}
