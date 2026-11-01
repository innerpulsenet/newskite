//! Segment throughput against the in-process mock server over loopback TCP:
//! `cargo bench -p newskite-engine --bench throughput`.
//!
//! Downloads one 64 MiB file in 750 KB segments with 8 connections and prints segments/s
//! and decoded MB/s. Loopback has no latency, so this measures the client's CPU path
//! (socket reads, dot-unstuffing, decode, offset writes) plus the mock server's own cost,
//! which is identical before and after any client change. An optional per-article delay
//! (`DELAY_MS=20`) stands in for network round-trip time to show the effect of
//! pipelining.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use newskite_engine::{download, DownloadRequest, EngineOptions, Progress};
use newskite_nntp::testkit::{MockConfig, MockServer};
use newskite_nzb::nzb::{Nzb, NzbFile, Segment};
use newskite_nzb::yenc;
use newskite_types::ServerConfig;
use tokio_util::sync::CancellationToken;

fn pseudo_random(len: usize, mut seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        out.extend_from_slice(&seed.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
    }
    out.truncate(len);
    out
}

fn main() {
    let total = 64 * 1024 * 1024usize;
    let chunk = 750_000usize;
    let connections: usize = std::env::var("CONNECTIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
    let delay_ms: u64 = std::env::var("DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let rounds: usize = std::env::var("ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);

    let data = pseudo_random(total, 0xBEEF);
    let parts = data.len().div_ceil(chunk);
    let mut articles = HashMap::new();
    let mut segments = Vec::with_capacity(parts);
    for (i, c) in data.chunks(chunk).enumerate() {
        let begin = (i * chunk) as u64 + 1;
        let body = yenc::encode_article("bench.bin", total as u64, i as u32 + 1, parts as u32, begin, c, 128);
        let id = format!("bench.part{}@newskite", i + 1);
        segments.push(Segment { number: i as u32 + 1, bytes: body.len() as u64, message_id: id.clone() });
        articles.insert(id, body);
    }
    let nzb = Nzb {
        meta: Default::default(),
        files: vec![NzbFile {
            subject: "bench [1/1] - \"bench.bin\" yEnc".into(),
            poster: "bench@newskite".into(),
            date: 0,
            groups: vec!["alt.binaries.test".into()],
            segments,
        }],
    };

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    rt.block_on(async move {
        let server = MockServer::start(MockConfig { articles, delay_ms, ..Default::default() }).await;
        let cfg = ServerConfig {
            host: server.addr.ip().to_string(),
            port: server.addr.port(),
            tls: false,
            connections: connections as u16,
            ..Default::default()
        };
        let mut best = f64::MAX;
        for round in 0..rounds {
            let dir = tempfile::tempdir().unwrap();
            let options = EngineOptions {
                connections,
                retries: 1,
                speed_limit_bps: 0,
                connect_timeout: Duration::from_secs(5),
                skip_files: HashSet::new(),
            };
            let req = DownloadRequest { nzb: nzb.clone(), dest_dir: dir.path().to_path_buf(), servers: vec![cfg.clone()], options };
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let progress = Arc::new(Progress::new());
            let start = Instant::now();
            let report = download(req, tx, progress, CancellationToken::new()).await.expect("download");
            let secs = start.elapsed().as_secs_f64();
            while rx.try_recv().is_ok() {}
            assert_eq!(report.bytes_written as usize, total);
            best = best.min(secs);
            println!(
                "round {round}: {parts} segments in {secs:.2} s = {:.1} segments/s, {:.0} MB/s decoded",
                parts as f64 / secs,
                total as f64 / 1e6 / secs
            );
        }
        println!(
            "best: {:.1} segments/s, {:.0} MB/s decoded ({connections} connections, delay {delay_ms} ms, {} connections opened)",
            parts as f64 / best,
            total as f64 / 1e6 / best,
            server.connections_accepted()
        );
    });
}
