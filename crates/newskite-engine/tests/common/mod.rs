//! Shared helpers for the engine integration tests.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use newskite_engine::{download, DownloadRequest, EngineError, EngineOptions, JobReport, Progress};
use newskite_nntp::testkit::{MockConfig, MockServer};
use newskite_nzb::nzb::{Nzb, NzbFile, Segment};
use newskite_nzb::yenc;
use newskite_types::{EngineEvent, ServerConfig};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

pub const USER: &str = "tester";
pub const PASS: &str = "s3cret-pass";

/// Deterministic xorshift64* bytes.
pub fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.max(1);
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        out.extend_from_slice(&x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
    }
    out.truncate(len);
    out
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

/// One source file split into yEnc articles.
pub struct TestFile {
    pub name: String,
    pub data: Vec<u8>,
    /// (message id, article body) in part order.
    pub articles: Vec<(String, Vec<u8>)>,
}

impl TestFile {
    /// Splits `data` into `parts` multipart yEnc articles named `yenc_name`.
    pub fn new(tag: &str, yenc_name: &str, data: Vec<u8>, parts: usize) -> Self {
        let size = data.len() as u64;
        let chunk = data.len().div_ceil(parts.max(1)).max(1);
        let total = data.chunks(chunk).count() as u32;
        let articles = data
            .chunks(chunk)
            .enumerate()
            .map(|(i, c)| {
                let begin = (i * chunk) as u64 + 1;
                let body = yenc::encode_article(yenc_name, size, i as u32 + 1, total, begin, c, 128);
                (format!("{tag}.part{}@test.newskite", i + 1), body)
            })
            .collect();
        Self { name: yenc_name.to_string(), data, articles }
    }

    /// NZB entry; segment `n` maps to `articles[order[n-1]]` (identity when `order` is None).
    pub fn nzb_file(&self, order: Option<&[usize]>) -> NzbFile {
        let segments = (0..self.articles.len())
            .map(|i| {
                let idx = order.map_or(i, |o| o[i]);
                let (id, body) = &self.articles[idx];
                Segment { number: i as u32 + 1, bytes: body.len() as u64, message_id: id.clone() }
            })
            .collect();
        NzbFile {
            subject: format!("test [1/1] - \"{}\" yEnc (1/{})", self.name, self.articles.len()),
            poster: "poster@test".into(),
            date: 0,
            groups: vec!["alt.binaries.test".into()],
            segments,
        }
    }
}

pub fn nzb_of(files: Vec<NzbFile>) -> Nzb {
    Nzb { meta: Default::default(), files }
}

pub fn articles_of<'a>(files: impl IntoIterator<Item = &'a TestFile>) -> HashMap<String, Vec<u8>> {
    files.into_iter().flat_map(|f| f.articles.iter().cloned()).collect()
}

pub async fn mock(articles: HashMap<String, Vec<u8>>, auth: bool) -> MockServer {
    MockServer::start(MockConfig {
        username: auth.then(|| USER.to_string()),
        password: auth.then(|| PASS.to_string()),
        articles,
        ..Default::default()
    })
    .await
}

pub fn server_for(m: &MockServer, auth: bool, connections: u16) -> ServerConfig {
    ServerConfig {
        host: m.addr.ip().to_string(),
        port: m.addr.port(),
        tls: false,
        username: if auth { USER.into() } else { String::new() },
        password: if auth { PASS.into() } else { String::new() },
        connections,
        priority: 0,
        enabled: true,
    }
}

pub fn options(connections: usize) -> EngineOptions {
    EngineOptions {
        connections,
        retries: 2,
        speed_limit_bps: 0,
        connect_timeout: Duration::from_secs(5),
        skip_files: HashSet::new(),
    }
}

pub struct Outcome {
    pub result: Result<JobReport, EngineError>,
    pub events: Vec<EngineEvent>,
    pub progress: Arc<Progress>,
}

pub async fn run(nzb: Nzb, dest: &Path, servers: Vec<ServerConfig>, opts: EngineOptions) -> Outcome {
    run_with_cancel(nzb, dest, servers, opts, CancellationToken::new()).await
}

pub async fn run_with_cancel(
    nzb: Nzb,
    dest: &Path,
    servers: Vec<ServerConfig>,
    opts: EngineOptions,
    cancel: CancellationToken,
) -> Outcome {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let progress = Arc::new(Progress::new());
    let req = DownloadRequest { nzb, dest_dir: dest.to_path_buf(), servers, options: opts };
    let result = tokio::time::timeout(Duration::from_secs(60), download(req, tx, progress.clone(), cancel))
        .await
        .expect("download did not finish within 60 s");
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    Outcome { result, events, progress }
}

/// Every entry of `dir` (file names), sorted.
pub fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

pub fn logs(events: &[EngineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Log(l) => Some(l.clone()),
            _ => None,
        })
        .collect()
}

pub fn path_in(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

/// Changes one yEnc data byte (not an escape, not a line edge) so the CRC no longer
/// matches but the decoded length stays the same.
pub fn corrupt_one_data_byte(body: &mut [u8]) {
    let first = body.windows(2).position(|w| w == b"\r\n").unwrap() + 2;
    let start = first + body[first..].windows(2).position(|w| w == b"\r\n").unwrap() + 2;
    let p = (start + 10..body.len() - 1)
        .find(|&p| {
            let b = body[p];
            !matches!(b, b'=' | b'\r' | b'\n' | b'.' | b' ' | b'\t')
                && body[p - 1] != b'='
                && !matches!(body[p + 1], b'\r' | b'\n')
        })
        .unwrap();
    body[p] = if body[p] == b'A' { b'B' } else { b'A' };
}
