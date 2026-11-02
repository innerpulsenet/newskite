//! Newskite download engine (outline F3).
//!
//! [`download`] fetches every segment of an NZB job over NNTP, decodes yEnc and writes each
//! decoded part at its offset (`begin - 1`) in the output file, so parts may complete in any
//! order and a file is never buffered whole in memory.
//!
//! # Behavior
//!
//! * **Concurrency.** `options.connections` worker tasks (capped by the sum of the servers'
//!   `connections` and by the number of segments) each own at most one NNTP connection at a
//!   time. A per-server semaphore keeps every server within its own connection limit.
//!   Workers prefer the first server and spill over to later ones when it is full, on
//!   fail-over and on retry. Connections are opened lazily, reused across segments and,
//!   through a [`ConnectionPool`] ([`download_with_pool`]), across jobs.
//! * **Pipelining.** On the primary server a worker sends the `BODY` request for its next
//!   segment as soon as the current article's status line arrives, so the request's round
//!   trip overlaps the current body transfer and the connection never idles between
//!   articles. At most one request is in flight ahead per connection, so in-flight memory
//!   stays bounded by one raw article plus one decoded part per connection. Decoding and
//!   CRC checking run on the blocking thread pool, never on the socket tasks.
//! * **Naming.** The output name is the yEnc `name=` of the first part decoded for a file,
//!   sanitized, falling back to [`newskite_nzb::NzbFile::filename`]. It is always a single
//!   path component inside `dest_dir`. Two files of one job that resolve to the same name
//!   get `"<stem> (2).<ext>"`, `(3)`, ... (case-insensitively). A name that already exists
//!   on disk is treated the same way unless the file counts as complete from an earlier run
//!   (see **Resume**).
//! * **Part checks.** A decoded part is accepted only when its yEnc `size=` is at most the
//!   bytes the NZB declares for the file plus a quarter and 1 MiB (16 MiB per segment
//!   without declared `bytes`), and when its `size=` and `name=` equal those of the file's
//!   first accepted part. A refused part (another post's article under a reused message id,
//!   or a hostile header) is never written; it is handled like an undecodable article.
//! * **In-progress files** are written as `<name>.nkpart` and renamed to `<name>` only when
//!   every segment arrived. A file with missing articles keeps its `.nkpart` and is reported
//!   with [`EngineEvent::FileFailed`]; the job then returns
//!   [`EngineError::MissingArticles`] with every missing message id.
//! * **Final size** is the yEnc header `size=` of the first accepted part (never less than the
//!   highest byte written). If the decoded data does not cover it, a warning is logged. The
//!   data and the rename are flushed to disk before `FileDone` is emitted.
//! * **Missing articles** (`430`/`423`), undecodable and refused articles are tried on every
//!   other usable server, never twice on the same one. Missing everywhere means missing. A
//!   fallback server that keeps failing transiently while it is asked for such an article
//!   (for example an expired backup account) is given up on for that article after
//!   `options.retries` retries, so the article is reported missing instead of the job
//!   aborting with [`EngineError::Connect`].
//! * **Transient errors** (I/O, timeout, connect, TLS, protocol) drop the connection and are
//!   retried up to `options.retries` more times per segment with exponential backoff
//!   (200 ms, 400 ms, ... capped at 5 s, each ±25 % jitter so workers do not reconnect in
//!   lockstep), walking the servers in order. A failed connection attempt (refused, TLS,
//!   `400`/`502` at the greeting or login) also puts the server in a cooldown of the same
//!   length that every worker honors before its next attempt, so one refusal is not
//!   followed by twenty more. When a segment exhausts its retries (outside the fail-over
//!   case above) the whole job stops with [`EngineError::Connect`]; the `.nkpart` files stay
//!   so a later run can resume.
//! * **Authentication failure** on a server excludes it for the rest of the job; when every
//!   server has failed authentication the job stops with [`EngineError::AuthFailed`].
//! * **CRC mismatches** are counted per file ([`FileReport::crc_errors`]) but the data is
//!   kept, since PAR2 may repair it.
//! * **Speed limit**: a token bucket shared by all workers (bursts of one segment), following
//!   a [`SpeedLimit`] that can be changed while the job runs.
//! * **Cancellation** stops all workers promptly and returns [`EngineError::Cancelled`];
//!   partial `.nkpart` files are left in place.
//! * **Resume**: `skip_files` lists output names completed by earlier runs. A file is skipped
//!   without fetching anything when its NZB-derived name is in `skip_files`, exists in
//!   `dest_dir` and no other file of the job derives the same name. Otherwise its first
//!   decoded part decides: the file is skipped when a `skip_files` entry equal to its
//!   sanitized yEnc name, or a numbered `"<stem> (n).<ext>"` variant of it, exists with the
//!   part's `size=` as its length and the part's bytes at the part's offset. Matching by
//!   content keeps files whose names collide from being taken for each other and finds
//!   outputs an earlier run saved under a numbered name. Skipped files emit only a `Log`
//!   line and are included in the [`JobReport`] with their on-disk size.
//!
//! # Events
//!
//! First, once: `Layout`, every file of the job in NZB order with its segment count and
//! whether it was found complete from an earlier run; a file's position in it is the
//! `file` of the article events. For each downloaded file: `FileStarted` (once the output
//! name is decided, before any `SegmentDone` of that file), one `SegmentDone` per stored
//! segment (`done` counts every processed segment of the file, including missing ones, so
//! it reaches `total`; `file` and `index` place the segment), a `SegmentMissing` for each
//! article no server could supply, then either `FileDone` (after the rename) or
//! `FileFailed`. A file found complete on disk by its content once the run started emits
//! `FileComplete` instead. All events of a file carry the same
//! resolved name. `Log` lines report job start, server fail-over, retries, completed files,
//! CRC problems and missing-article summaries. Passwords never appear in any event.
//!
//! [`Progress::bytes_down`] counts decoded bytes stored in this run (skipped files are not
//! counted); [`Progress::segments_done`] counts processed segments, missing ones included.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod limiter;
mod naming;
mod pool;
mod pwrite;
mod run;

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use newskite_nzb::Nzb;
use newskite_types::{EngineEvent, ServerConfig};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

pub use limiter::SpeedLimit;
pub use pool::{ConnectionPool, POOL_IDLE_MAX};

/// Extension (without the dot) of in-progress output files: `<name>.nkpart`.
pub const PART_EXTENSION: &str = "nkpart";

/// First retry delay; doubles per attempt.
pub const BACKOFF_BASE: Duration = Duration::from_millis(200);

/// Upper bound for the retry delay.
pub const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Tunables for one [`download`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineOptions {
    /// Maximum simultaneous segment fetches across all servers (at least 1 is used).
    pub connections: usize,
    /// Extra attempts per segment after a transient error.
    pub retries: u32,
    /// Bytes per second across the whole job (0 = unlimited), read as the job runs:
    /// [`SpeedLimit::set`] on a clone changes it mid-download.
    pub speed_limit: SpeedLimit,
    /// Bound for TCP connect + TLS + greeting + authentication.
    pub connect_timeout: Duration,
    /// Output file names already completed by an earlier run (resume), as reported in
    /// `FileDone`/[`FileReport::name`]. See the crate docs for how files are matched.
    pub skip_files: HashSet<String>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            connections: 8,
            retries: 2,
            speed_limit: SpeedLimit::default(),
            connect_timeout: Duration::from_secs(30),
            skip_files: HashSet::new(),
        }
    }
}

/// Everything [`download`] needs for one job.
#[derive(Clone)]
pub struct DownloadRequest {
    /// The parsed NZB.
    pub nzb: Nzb,
    /// Directory the output files are written into (created if missing).
    pub dest_dir: PathBuf,
    /// Servers in fail-over order. Disabled or host-less entries are ignored.
    pub servers: Vec<ServerConfig>,
    /// Tunables.
    pub options: EngineOptions,
}

impl fmt::Debug for DownloadRequest {
    // Hand-written so server passwords never reach a log line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let servers: Vec<String> = self.servers.iter().map(ServerConfig::address).collect();
        f.debug_struct("DownloadRequest")
            .field("files", &self.nzb.files.len())
            .field("dest_dir", &self.dest_dir)
            .field("servers", &servers)
            .field("options", &self.options)
            .finish()
    }
}

/// Outcome for one file of the job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileReport {
    /// Resolved output file name.
    pub name: String,
    /// Final path when complete (or skipped); the `.nkpart` path when articles are missing.
    pub path: PathBuf,
    /// Final size for complete/skipped files, otherwise decoded bytes written.
    pub bytes: u64,
    /// Message ids (without `<>`) that no server could deliver, in segment order.
    pub missing: Vec<String>,
    /// Segments whose yEnc CRC did not match (data kept).
    pub crc_errors: usize,
}

/// Outcome of a successful job.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobReport {
    /// One entry per NZB file, in NZB order.
    pub files: Vec<FileReport>,
    /// Decoded bytes written during this run.
    pub bytes_written: u64,
}

/// Why a job did not complete.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// No usable server was supplied.
    #[error("no enabled server configured")]
    NoServer,
    /// Every server rejected the credentials; carries the last server reply.
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    /// A segment kept failing with network errors after all retries.
    #[error("connection failed: {0}")]
    Connect(String),
    /// Articles no server could deliver (message ids without `<>`).
    #[error("{} missing article(s)", ids.len())]
    MissingArticles {
        /// Every missing message id, in file then segment order.
        ids: Vec<String>,
    },
    /// Local file-system failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The cancellation token fired.
    #[error("download cancelled")]
    Cancelled,
}

/// Live counters, shared with the UI/service.
#[derive(Debug, Default)]
pub struct Progress {
    /// Decoded bytes stored during this run (monotonic).
    pub bytes_down: AtomicU64,
    /// Segments processed, including missing ones (monotonic).
    pub segments_done: AtomicU64,
    /// Per-server health, in the order servers were first used.
    servers: std::sync::Mutex<Vec<newskite_types::ServerHealth>>,
}

impl Progress {
    /// Fresh zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current [`Progress::bytes_down`].
    pub fn bytes_down(&self) -> u64 {
        self.bytes_down.load(Ordering::Relaxed)
    }

    /// Current [`Progress::segments_done`].
    pub fn segments_done(&self) -> u64 {
        self.segments_done.load(Ordering::Relaxed)
    }

    /// Snapshot of every server's health since this `Progress` was created.
    pub fn server_health(&self) -> Vec<newskite_types::ServerHealth> {
        self.servers.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Updates (creating if needed) the health record of `address`.
    pub(crate) fn health(&self, address: &str, f: impl FnOnce(&mut newskite_types::ServerHealth)) {
        let mut v = self.servers.lock().unwrap_or_else(|e| e.into_inner());
        let i = match v.iter().position(|h| h.address == address) {
            Some(i) => i,
            None => {
                v.push(newskite_types::ServerHealth { address: address.to_string(), ..Default::default() });
                v.len() - 1
            }
        };
        f(&mut v[i]);
    }
}

/// Downloads one job. See the crate docs for the full behavior.
///
/// Event send failures (receiver dropped) are ignored. Returns the per-file report when
/// every file completed (or was skipped). Connections are closed when the job ends; use
/// [`download_with_pool`] to keep them for the next job.
pub async fn download(
    req: DownloadRequest,
    events: UnboundedSender<EngineEvent>,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> Result<JobReport, EngineError> {
    let pool = Arc::new(ConnectionPool::new());
    let result = run::run(req, pool.clone(), events, progress, cancel).await;
    pool.close_idle();
    result
}

/// Like [`download`], but takes idle connections from `pool` instead of opening new ones
/// where it can, and hands the job's connections back to it at the end. The caller decides
/// when to close them ([`ConnectionPool::close_idle`]), typically when the queue goes idle.
pub async fn download_with_pool(
    req: DownloadRequest,
    pool: Arc<ConnectionPool>,
    events: UnboundedSender<EngineEvent>,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> Result<JobReport, EngineError> {
    run::run(req, pool, events, progress, cancel).await
}

/// Retry delay before attempt `attempt` (1-based): 200 ms, 400 ms, 800 ms, ... capped at 5 s.
pub fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    BACKOFF_BASE.saturating_mul(1u32 << shift).min(BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_delay(0), Duration::from_millis(200));
        assert_eq!(backoff_delay(1), Duration::from_millis(200));
        assert_eq!(backoff_delay(2), Duration::from_millis(400));
        assert_eq!(backoff_delay(3), Duration::from_millis(800));
        assert_eq!(backoff_delay(6), Duration::from_millis(5000));
        assert_eq!(backoff_delay(u32::MAX), BACKOFF_MAX);
    }

    #[test]
    fn request_debug_hides_passwords() {
        let req = DownloadRequest {
            nzb: Nzb::default(),
            dest_dir: PathBuf::from("out"),
            servers: vec![ServerConfig {
                host: "news.example".into(),
                username: "user".into(),
                password: "hunter2".into(),
                ..Default::default()
            }],
            options: EngineOptions::default(),
        };
        let s = format!("{req:?}");
        assert!(!s.contains("hunter2"));
        assert!(s.contains("news.example:563"));
    }
}
