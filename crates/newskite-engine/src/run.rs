//! The download run: work queue, worker pool, per-server connection slots, output files.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use newskite_nntp::{NntpConnection, NntpError};
use newskite_nzb::nzb::{sanitize_filename, Segment};
use newskite_nzb::yenc::{self, YencPart};
use newskite_types::{format, EngineEvent, FileLayout, ServerConfig};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Notify, OnceCell, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::limiter::RateLimiter;
use crate::naming::{claim_key, part_path, suffix_number, with_suffix};
use crate::pool::ConnectionPool;
use crate::pwrite::{read_exact_at, write_all_at};
use crate::repair::{is_par2_name, vol_blocks, Head, Tracker};
use crate::{backoff_delay, DownloadRequest, EngineError, FileReport, JobReport, Progress};

/// Message ids listed in a missing-article summary line.
const MISSING_PREVIEW: usize = 3;
/// Individual fail-over log lines per job before going quiet.
const FAILOVER_LOG_LIMIT: usize = 5;
/// Individual "bad article" log lines (undecodable, foreign or invalid ids) per job before
/// going quiet; a summary line follows at the end of the run (outline §6.3: one line per
/// meaningful event, never one per segment).
const BAD_ARTICLE_LOG_LIMIT: usize = 5;
/// Upper bound when searching for a free `"name (n).ext"`.
const MAX_NAME_SUFFIX: u32 = 10_000;
/// Allowance for a segment whose NZB entry declares no `bytes`.
const UNKNOWN_SEGMENT_BYTES: u64 = 16 * 1024 * 1024;
/// Fixed slack on top of a file's declared bytes when bounding its yEnc `size=`.
const SIZE_SLACK: u64 = 1024 * 1024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One configured server plus its connection budget for this job.
struct ServerSlot {
    cfg: ServerConfig,
    address: String,
    permits: Arc<Semaphore>,
    /// Set after an authentication failure; the server is not used again in this job.
    unusable: AtomicBool,
    /// Consecutive failed connection attempts; reset by a successful connect.
    strikes: AtomicU32,
    /// No worker opens a connection to this server before this instant (set after a failed
    /// attempt, growing with `strikes`), so one refusal does not trigger a reconnect storm.
    cooldown: Mutex<Option<Instant>>,
}

impl ServerSlot {
    fn new(cfg: ServerConfig) -> Self {
        Self {
            address: cfg.address(),
            permits: Arc::new(Semaphore::new(usize::from(cfg.connections.max(1)))),
            unusable: AtomicBool::new(false),
            strikes: AtomicU32::new(0),
            cooldown: Mutex::new(None),
            cfg,
        }
    }

    fn note_connected(&self) {
        self.strikes.store(0, Ordering::SeqCst);
        *lock(&self.cooldown) = None;
    }

    fn note_connect_failure(&self) {
        let strikes = self.strikes.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        *lock(&self.cooldown) = Some(Instant::now() + jittered(backoff_delay(strikes)));
    }
}

/// The decided output for a file (set exactly once per file).
struct Output {
    name: String,
    final_path: PathBuf,
    part_path: PathBuf,
    /// Open `.nkpart` handle; taken (closed) at finalization. `None` when skipped.
    file: Mutex<Option<Arc<File>>>,
    /// Already complete from an earlier run.
    skipped: bool,
}

/// Per-file state shared by the workers.
struct FileJob {
    /// NZB-derived name, de-duplicated within the job; the fallback output name.
    label: String,
    segments: Vec<Segment>,
    output: OnceCell<Arc<Output>>,
    /// Processed segment count; the lock also orders this file's events.
    processed: Mutex<usize>,
    bytes: AtomicU64,
    max_end: AtomicU64,
    /// yEnc `size=` of the first accepted part; 0 = unknown.
    size_hint: AtomicU64,
    /// Largest yEnc `size=` (and so `end=`) accepted for this file, derived from the bytes
    /// the NZB declares for it.
    size_cap: u64,
    /// yEnc `size=` and `name=` of the first accepted part; later parts must match.
    identity: Mutex<Option<(u64, String)>>,
    crc_errors: AtomicUsize,
    missing: Mutex<Vec<(u32, String)>>,
    report: Mutex<Option<FileReport>>,
}

impl FileJob {
    fn new(label: String, segments: Vec<Segment>) -> Self {
        // Segment `bytes` are encoded article sizes, so the decoded file is smaller; the
        // margin only absorbs NZBs that under-report a little.
        let declared = segments
            .iter()
            .map(|s| if s.bytes > 0 { s.bytes } else { UNKNOWN_SEGMENT_BYTES })
            .fold(0u64, u64::saturating_add);
        let size_cap = declared.saturating_add(declared / 4).saturating_add(SIZE_SLACK);
        Self {
            label,
            segments,
            size_cap,
            identity: Mutex::new(None),
            output: OnceCell::new(),
            processed: Mutex::new(0),
            bytes: AtomicU64::new(0),
            max_end: AtomicU64::new(0),
            size_hint: AtomicU64::new(0),
            crc_errors: AtomicUsize::new(0),
            missing: Mutex::new(Vec::new()),
            report: Mutex::new(None),
        }
    }

    fn is_skipped(&self) -> bool {
        self.output.get().is_some_and(|o| o.skipped)
    }

    /// Checks a decoded part against the NZB and against the file's other parts. The first
    /// part that passes fixes the file's yEnc `size=` and `name=`; a part from another post
    /// (reused message id, wrong article on a server, hostile poster) is refused so it is
    /// never written into this file. `decode` already guarantees `begin <= end <= size`.
    fn check_part(&self, part: &YencPart) -> Result<(), String> {
        if part.size > self.size_cap {
            return Err(format!(
                "it declares size={}, more than the {} this NZB file allows",
                part.size,
                format::bytes(self.size_cap)
            ));
        }
        let mut identity = lock(&self.identity);
        match &*identity {
            None => {
                *identity = Some((part.size, part.name.clone()));
                self.size_hint.store(part.size, Ordering::SeqCst);
                Ok(())
            }
            Some((size, name)) if *size == part.size && *name == part.name => Ok(()),
            Some((size, name)) => Err(format!(
                "it declares size={} name={:?}, but the file's other parts declare size={size} name={name:?}",
                part.size, part.name
            )),
        }
    }
}

/// A file in the download folder: its name and length.
type OnDisk = (String, u64);

struct Shared {
    dest_dir: PathBuf,
    servers: Vec<ServerSlot>,
    pool: Arc<ConnectionPool>,
    /// Notified whenever a connection slot is released or a server becomes unusable.
    released: Notify,
    files: Vec<FileJob>,
    queue: Mutex<VecDeque<(usize, usize)>>,
    /// Claimed output names (see [`claim_key`]).
    claimed: Mutex<HashSet<String>>,
    skip_files: HashSet<String>,
    retries: u32,
    connect_timeout: Duration,
    limiter: RateLimiter,
    events: UnboundedSender<EngineEvent>,
    progress: Arc<Progress>,
    /// Child of the caller's token; also fired on fatal errors.
    abort: CancellationToken,
    fatal: Mutex<Option<EngineError>>,
    bytes_written: AtomicU64,
    failovers: AtomicUsize,
    /// Articles rejected as damaged, foreign or with an invalid id (see
    /// [`BAD_ARTICLE_LOG_LIMIT`]).
    bad_articles: AtomicUsize,
    /// The repair outlook (see [`crate::repair`]).
    repair: Mutex<Tracker>,
    stop_unrepairable: bool,
    /// Complete files in `dest_dir` when the run started (see [`resume_candidates`]).
    on_disk: Vec<OnDisk>,
    /// Files being finalized (see [`finish_segment`]); drained by [`run`] before it reports.
    finalizing: Mutex<JoinSet<()>>,
}

impl Shared {
    fn emit(&self, ev: EngineEvent) {
        // A dropped receiver only means nobody is watching.
        let _ = self.events.send(ev);
    }

    fn log(&self, line: impl Into<String>) {
        let line = line.into();
        tracing::info!(target: "newskite_engine", "{line}");
        self.emit(EngineEvent::Log(line));
    }

    /// A problem that did not stop the download.
    fn warn(&self, line: impl Into<String>) {
        let line = line.into();
        tracing::warn!(target: "newskite_engine", "{line}");
        self.emit(EngineEvent::Warn(line));
    }

    /// Records a transient error against `server`'s health.
    fn note_error(&self, server: usize, err: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        self.progress.health(&self.servers[server].address, |h| {
            h.errors += 1;
            h.last_error = Some((now, err.to_string()));
        });
    }

    /// Logs a per-article problem, but only the first [`BAD_ARTICLE_LOG_LIMIT`] of a run;
    /// the rest go to tracing only and are summarized by [`finish`].
    fn log_bad_article(&self, line: &str) {
        let n = self.bad_articles.fetch_add(1, Ordering::Relaxed);
        if n < BAD_ARTICLE_LOG_LIMIT {
            self.warn(line);
        } else {
            tracing::debug!(target: "newskite_engine", "{line}");
        }
    }

    /// Records the first fatal error and stops every worker.
    fn fail(&self, err: EngineError) {
        {
            let mut f = lock(&self.fatal);
            if f.is_none() {
                tracing::warn!(target: "newskite_engine", error = %err, "download aborted");
                *f = Some(err);
            }
        }
        self.abort.cancel();
    }

    /// A decoded part of file `fi`, article `si`, about to be stored.
    fn note_part(&self, fi: usize, si: usize, part: &YencPart) {
        let head = (part.begin == 1).then(|| Head::of(&part.data, part.size));
        let changed = lock(&self.repair).note_part(fi, si, &part.name, part.size, (part.begin, part.end), head);
        if changed {
            self.update_outlook();
        }
    }

    /// Article `si` of file `fi` is missing or failed its CRC.
    fn note_damaged(&self, fi: usize, si: usize) {
        if lock(&self.repair).note_damaged(fi, si) {
            self.update_outlook();
        }
    }

    /// Recomputes the repair outlook; reports a change, and stops the job when it can no
    /// longer be repaired and the caller asked for that.
    fn update_outlook(&self) {
        let (o, was) = {
            let mut t = lock(&self.repair);
            let o = t.outlook();
            if t.last == Some(o) {
                return;
            }
            (o, t.last.replace(o))
        };
        if o.known && !was.is_some_and(|w| w.known) {
            let approx = if o.exact { "" } else { "up to " };
            self.log(format!("PAR2: {} slice(s) protected, {approx}{} recovery block(s) posted", o.blocks, o.posted));
        }
        self.emit(EngineEvent::Repair(o));
        if o.hopeless() && self.stop_unrepairable && !self.abort.is_cancelled() {
            let missing = self.files.iter().map(|f| lock(&f.missing).len()).sum();
            self.warn(format!(
                "Stopping: the damage needs at least {} recovery block(s), but at most {} can still arrive ({} posted); the download cannot be repaired",
                o.needed, o.available, o.posted
            ));
            self.fail(EngineError::Unrepairable { needed: o.needed, available: o.available, missing });
        }
    }

    fn next_item(&self) -> Option<(usize, usize)> {
        lock(&self.queue).pop_front()
    }

    /// Puts a segment back at the head of the queue (its pipelined request was lost with
    /// the connection it was sent on).
    fn requeue(&self, item: (usize, usize)) {
        lock(&self.queue).push_front(item);
    }

    /// The top-ranked usable server: the only one requests are pipelined on, so workers
    /// never drift onto a fallback server while the primary has free capacity.
    fn primary_server(&self) -> Option<usize> {
        self.servers.iter().position(|s| !s.unusable.load(Ordering::SeqCst))
    }

    /// Whether `server` may still be asked for a segment with the given `tried` set.
    fn can_serve(&self, server: usize, tried: &[bool]) -> bool {
        !tried[server] && !self.servers[server].unusable.load(Ordering::SeqCst)
    }

    /// Usable, not-yet-tried servers, starting at `prefer` and wrapping around.
    fn order(&self, tried: &[bool], prefer: usize) -> Vec<usize> {
        let n = self.servers.len();
        (0..n)
            .map(|k| (prefer + k) % n)
            .filter(|&i| !tried[i] && !self.servers[i].unusable.load(Ordering::SeqCst))
            .collect()
    }

    fn mark_unusable(&self, server: usize, msg: &str) {
        let slot = &self.servers[server];
        self.progress.health(&slot.address, |h| h.unusable = Some(msg.to_string()));
        if !slot.unusable.swap(true, Ordering::SeqCst) {
            self.warn(format!(
                "{}: authentication failed ({msg}); not using this server for the rest of the job",
                slot.address
            ));
        }
        self.released.notify_waiters();
        if self.servers.iter().all(|s| s.unusable.load(Ordering::SeqCst)) {
            self.fail(EngineError::AuthFailed(format!("{}: {msg}", slot.address)));
        }
    }
}

/// A held connection slot on one server. Dropping it frees the slot and wakes waiters.
struct Slot {
    server: usize,
    permit: Option<OwnedSemaphorePermit>,
    shared: Arc<Shared>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        // Release the permit before notifying so a woken waiter can take it.
        drop(self.permit.take());
        self.shared.released.notify_waiters();
    }
}

struct Held {
    conn: NntpConnection,
    /// Segment whose `BODY` request is already on the wire (the next reply on `conn`).
    prefetched: Option<(usize, usize)>,
    /// Taken from the pool and not yet proven alive: a first failure is silently replaced
    /// by a fresh connection instead of counting as a retry.
    untested: bool,
    // Declared after `conn` so the socket closes before the slot is handed on.
    slot: Slot,
}

/// Per-segment fetch state; lives across [`Flow::Yield`]s.
struct SegmentState {
    fi: usize,
    si: usize,
    tried: Vec<bool>,
    failures: u32,
    prefer: usize,
    reason: Option<String>,
    /// Set once a server answered "missing", "undecodable" or "foreign" for this segment:
    /// from then on a fallback that keeps failing transiently is given up on for this
    /// article instead of aborting the job.
    failover: bool,
}

impl SegmentState {
    fn new(shared: &Shared, (fi, si): (usize, usize)) -> Self {
        Self { fi, si, tried: vec![false; shared.servers.len()], failures: 0, prefer: 0, reason: None, failover: false }
    }
}

enum ConnFail {
    Stopped,
    NoEligible,
    Auth { server: usize, msg: String },
    Transient { server: usize, err: NntpError },
}

enum Flow {
    /// The segment is done (stored, missing or skipped).
    Continue,
    /// The job is over (aborted or cancelled).
    Stop,
    /// The segment must move to another server but the held connection has a pipelined
    /// reply pending: process that segment first, then come back to this one.
    Yield,
}

pub(crate) async fn run(
    req: DownloadRequest,
    pool: Arc<ConnectionPool>,
    events: UnboundedSender<EngineEvent>,
    progress: Arc<Progress>,
    cancel: CancellationToken,
) -> Result<crate::JobReport, EngineError> {
    let DownloadRequest { nzb, dest_dir, servers, options } = req;
    let servers: Vec<ServerConfig> = servers.into_iter().filter(ServerConfig::is_usable).collect();
    if servers.is_empty() {
        return Err(EngineError::NoServer);
    }
    if cancel.is_cancelled() {
        return Err(EngineError::Cancelled);
    }

    // Labels: NZB-derived names, de-duplicated case-insensitively in NZB order.
    let mut seen = HashSet::new();
    let mut files = Vec::with_capacity(nzb.files.len());
    let mut base_counts: HashMap<String, usize> = HashMap::new();
    for f in &nzb.files {
        *base_counts.entry(claim_key(&f.filename())).or_default() += 1;
    }
    // A label can identify a completed file only when no other file of the job derives the
    // same name: within a collision group, which file won the plain name in an earlier run
    // depended on decode order, so those files are identified by content instead.
    let mut unique_label = Vec::with_capacity(nzb.files.len());
    for f in &nzb.files {
        let base = f.filename();
        let label = (1..=MAX_NAME_SUFFIX)
            .map(|n| with_suffix(&base, n))
            .find(|cand| seen.insert(claim_key(cand)))
            .unwrap_or_else(|| base.clone());
        unique_label.push(label == base && base_counts.get(&claim_key(&base)) == Some(&1));
        files.push(FileJob::new(label, f.segments.clone()));
    }

    // Create the destination and find files already complete from an earlier run.
    let labels: Vec<String> = files.iter().map(|f| f.label.clone()).collect();
    let skip_files = options.skip_files.clone();
    let dir = dest_dir.clone();
    let (pre_skipped, on_disk) = tokio::task::spawn_blocking(move || -> io::Result<(Vec<bool>, Vec<OnDisk>)> {
        std::fs::create_dir_all(&dir)?;
        let pre = labels
            .iter()
            .zip(&unique_label)
            .map(|(l, &unique)| unique && skip_files.contains(l) && dir.join(l).is_file())
            .collect();
        Ok((pre, resume_candidates(&dir)))
    })
    .await
    .map_err(|e| io::Error::other(format!("preparing the download folder failed: {e}")))??;

    let mut claimed = HashSet::new();
    let mut download_bytes = 0u64;
    let mut download_files = 0usize;
    let mut skipped_names = Vec::new();
    for (fi, (file, &skip)) in files.iter().zip(&pre_skipped).enumerate() {
        if skip {
            claimed.insert(claim_key(&file.label));
            let final_path = dest_dir.join(&file.label);
            let out = Output {
                name: file.label.clone(),
                part_path: part_path(&final_path),
                final_path,
                file: Mutex::new(None),
                skipped: true,
            };
            // Freshly created cell: cannot already be set.
            let _ = file.output.set(Arc::new(out));
            skipped_names.push(file.label.clone());
        } else {
            download_files += 1;
            download_bytes += nzb.files[fi].total_bytes();
        }
    }
    let queue = download_order(&files, &pre_skipped);

    let total_segments = queue.len();
    let slots: Vec<ServerSlot> = servers.into_iter().map(ServerSlot::new).collect();
    let capacity: usize = slots.iter().map(|s| usize::from(s.cfg.connections.max(1))).sum();
    let workers = options.connections.max(1).min(capacity).min(total_segments);
    let tracker = Tracker::new(
        files
            .iter()
            .map(|f| {
                let numbers = f.segments.iter().map(|s| s.number).collect();
                (f.label.clone(), numbers, f.segments.iter().map(|s| s.bytes).collect())
            })
            .collect(),
        UNKNOWN_SEGMENT_BYTES,
    );

    let shared = Arc::new(Shared {
        dest_dir,
        servers: slots,
        pool,
        released: Notify::new(),
        files,
        queue: Mutex::new(queue),
        claimed: Mutex::new(claimed),
        skip_files: options.skip_files,
        retries: options.retries,
        connect_timeout: options.connect_timeout,
        limiter: RateLimiter::new(options.speed_limit),
        events,
        progress,
        abort: cancel.child_token(),
        fatal: Mutex::new(None),
        bytes_written: AtomicU64::new(0),
        failovers: AtomicUsize::new(0),
        bad_articles: AtomicUsize::new(0),
        repair: Mutex::new(tracker),
        stop_unrepairable: options.stop_unrepairable,
        on_disk,
        finalizing: Mutex::new(JoinSet::new()),
    });

    // A new job tries every server again; earlier login failures stay in last_error.
    for slot in &shared.servers {
        shared.progress.health(&slot.address, |h| h.unusable = None);
    }
    let fallback = match shared.servers.len() {
        1 => String::new(),
        n => format!(" (+{} fallback)", n - 1),
    };
    shared.log(format!(
        "Starting download: {download_files} file(s), ~{} in {total_segments} segment(s), server {}{fallback}, {workers} connection(s)",
        format::bytes(download_bytes),
        shared.servers[0].address,
    ));
    for name in &skipped_names {
        shared.log(format!("{name}: already complete, skipping"));
    }
    // Files complete from an earlier run still count for the repair outlook.
    let found: Vec<(usize, PathBuf)> = pre_skipped
        .iter()
        .enumerate()
        .filter(|(_, &s)| s)
        .map(|(fi, _)| (fi, shared.dest_dir.join(&shared.files[fi].label)))
        .collect();
    if !found.is_empty() {
        let s = shared.clone();
        let _ = tokio::task::spawn_blocking(move || {
            for (fi, path) in found {
                note_on_disk(&s, fi, &path);
            }
        })
        .await;
    }
    // Before any file event: the article events place themselves in this layout.
    shared.emit(EngineEvent::Layout {
        files: shared
            .files
            .iter()
            .zip(&pre_skipped)
            .map(|(f, &complete)| FileLayout { name: f.label.clone(), segments: f.segments.len(), complete })
            .collect(),
    });
    shared.update_outlook();

    let mut set = JoinSet::new();
    for _ in 0..workers {
        set.spawn(worker(shared.clone()));
    }
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            shared.fail(EngineError::Io(io::Error::other(format!("download worker failed: {e}"))));
        }
    }
    // Every file whose last segment was processed finishes (flushed, renamed, reported)
    // before the run does, cancelled or not; no worker is left to start another.
    let mut finalizing = std::mem::take(&mut *lock(&shared.finalizing));
    while let Some(res) = finalizing.join_next().await {
        if let Err(e) = res {
            shared.fail(EngineError::Io(io::Error::other(format!("finishing a file failed: {e}"))));
        }
    }

    finish(&shared, &cancel)
}

/// Builds the job result once every worker has stopped.
fn finish(shared: &Shared, cancel: &CancellationToken) -> Result<JobReport, EngineError> {
    if let Some(err) = lock(&shared.fatal).take() {
        return Err(err);
    }
    let mut reports = Vec::with_capacity(shared.files.len());
    let mut missing = Vec::new();
    for file in &shared.files {
        let report = if file.is_skipped() {
            skipped_report(shared, file)
        } else {
            match lock(&file.report).clone() {
                Some(r) => r,
                // Unfinished file: only possible when the run was cut short.
                None => return Err(EngineError::Cancelled),
            }
        };
        missing.extend(report.missing.iter().cloned());
        reports.push(report);
    }
    if cancel.is_cancelled() && !lock(&shared.queue).is_empty() {
        return Err(EngineError::Cancelled);
    }
    let failovers = shared.failovers.load(Ordering::Relaxed);
    if failovers > FAILOVER_LOG_LIMIT {
        shared.log(format!("{failovers} article(s) were fetched from fallback servers"));
    }
    log_bad_article_summary(shared);
    if !missing.is_empty() {
        shared.warn(format!("Download incomplete: {} missing article(s)", missing.len()));
        return Err(EngineError::MissingArticles { ids: missing });
    }
    let bytes_written = shared.bytes_written.load(Ordering::SeqCst);
    shared.log(format!("Download complete: {} file(s), {} written", reports.len(), format::bytes(bytes_written)));
    Ok(JobReport { files: reports, bytes_written })
}

/// The end-of-run line for bad articles beyond the individually logged ones.
fn log_bad_article_summary(shared: &Shared) {
    let bad = shared.bad_articles.load(Ordering::Relaxed);
    if bad > BAD_ARTICLE_LOG_LIMIT {
        shared.warn(format!(
            "{bad} article fetch(es) were rejected as damaged, foreign or invalid (first {BAD_ARTICLE_LOG_LIMIT} listed above)"
        ));
    }
}

fn skipped_report(shared: &Shared, file: &FileJob) -> FileReport {
    let (name, path) = match file.output.get() {
        Some(o) => (o.name.clone(), o.final_path.clone()),
        None => (file.label.clone(), shared.dest_dir.join(&file.label)),
    };
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    FileReport { name, path, bytes, missing: Vec::new(), crc_errors: 0 }
}

/// The queue: the PAR2 index (the smallest `.par2` that is not a volume, else the smallest
/// volume) whole, then the first article of every other file, then the rest in NZB order.
fn download_order(files: &[FileJob], skipped: &[bool]) -> VecDeque<(usize, usize)> {
    let todo: Vec<usize> = (0..files.len()).filter(|&fi| !skipped[fi]).collect();
    let declared = |fi: usize| files[fi].segments.iter().map(|s| s.bytes).fold(0u64, u64::saturating_add);
    let index = todo
        .iter()
        .copied()
        .filter(|&fi| is_par2_name(&files[fi].label))
        .min_by_key(|&fi| (vol_blocks(&files[fi].label).is_some(), declared(fi)));
    let mut queue = VecDeque::new();
    if let Some(fi) = index {
        queue.extend((0..files[fi].segments.len()).map(|si| (fi, si)));
    }
    let rest: Vec<usize> = todo.into_iter().filter(|&fi| Some(fi) != index).collect();
    queue.extend(rest.iter().filter(|&&fi| !files[fi].segments.is_empty()).map(|&fi| (fi, 0)));
    for fi in rest {
        queue.extend((1..files[fi].segments.len()).map(|si| (fi, si)));
    }
    queue
}

/// Blocking: tells the repair outlook about file `fi`, complete at `path` from an earlier
/// run (its first bytes, and its recovery blocks when it is PAR2).
fn note_on_disk(shared: &Shared, fi: usize, path: &Path) {
    let Ok(f) = File::open(path) else { return };
    let len = f.metadata().map_or(0, |m| m.len());
    let mut head = vec![0u8; len.min(16 * 1024) as usize];
    let head = read_exact_at(&f, &mut head, 0).is_ok().then(|| Head::of(&head, len));
    let par2 = {
        let mut t = lock(&shared.repair);
        t.note_complete(fi, len, head);
        t.is_par2(fi)
    };
    if par2 {
        read_par2(shared, fi, path);
    }
}

/// Blocking: reads file `fi` at `path` as PAR2 for the repair outlook.
fn read_par2(shared: &Shared, fi: usize, path: &Path) {
    let (sets, _) = newskite_par2::load(&[path.to_path_buf()]);
    lock(&shared.repair).note_par2(fi, &sets);
}

async fn worker(shared: Arc<Shared>) {
    let mut held: Option<Held> = None;
    // Segments that stepped aside for a pipelined reply (see `Flow::Yield`).
    let mut deferred: Vec<SegmentState> = Vec::new();
    while !shared.abort.is_cancelled() {
        // A pipelined reply must be read before anything else happens on its connection,
        // so its segment goes first; then the ones that stepped aside; then new work.
        let mut state = if let Some(item) = held.as_mut().and_then(|h| h.prefetched.take()) {
            SegmentState::new(&shared, item)
        } else if let Some(state) = deferred.pop() {
            state
        } else if let Some(item) = shared.next_item() {
            SegmentState::new(&shared, item)
        } else {
            break;
        };
        match process_segment(&shared, &mut held, &mut state).await {
            Flow::Continue => {}
            Flow::Yield => deferred.push(state),
            Flow::Stop => break,
        }
    }
    if shared.abort.is_cancelled() {
        return; // Just drop the socket.
    }
    drop_conn(&shared, &mut held);
}

/// Gives the worker's connection up: a pending pipelined segment goes back to the queue,
/// a healthy idle connection goes to the pool for the next worker or job, anything else is
/// closed. The slot is released either way.
fn drop_conn(shared: &Shared, held: &mut Option<Held>) {
    let Some(h) = held.take() else { return };
    if let Some(item) = h.prefetched {
        shared.requeue(item);
    } else {
        shared.pool.put(&shared.servers[h.slot.server].cfg, h.conn);
    }
}

/// Waits for a free connection slot on the best eligible server.
async fn acquire_slot(shared: &Arc<Shared>, tried: &[bool], prefer: usize) -> Result<Slot, ConnFail> {
    loop {
        let notified = shared.released.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let order = shared.order(tried, prefer);
        if order.is_empty() {
            return Err(ConnFail::NoEligible);
        }
        for server in order {
            if let Ok(p) = shared.servers[server].permits.clone().try_acquire_owned() {
                return Ok(Slot { server, permit: Some(p), shared: shared.clone() });
            }
        }
        tokio::select! {
            _ = &mut notified => {}
            _ = shared.abort.cancelled() => return Err(ConnFail::Stopped),
        }
    }
}

/// Makes sure `held` is a healthy connection to the best eligible server.
async fn ensure_conn(
    shared: &Arc<Shared>,
    held: &mut Option<Held>,
    tried: &[bool],
    prefer: usize,
) -> Result<(), ConnFail> {
    let order = shared.order(tried, prefer);
    if order.is_empty() {
        drop_conn(shared, held);
        return Err(ConnFail::NoEligible);
    }
    let mut upgrade = None;
    if let Some(h) = held.as_ref() {
        match order.iter().position(|&s| s == h.slot.server) {
            Some(_) if h.conn.is_broken() => drop_conn(shared, held),
            None => drop_conn(shared, held),
            Some(0) => return Ok(()),
            // A pipelined reply pins the segment to this connection.
            Some(_) if h.prefetched.is_some() => return Ok(()),
            Some(pos) => {
                // Prefer a higher-ranked server if it has a free slot right now.
                upgrade = order[..pos].iter().find_map(|&server| {
                    shared.servers[server]
                        .permits
                        .clone()
                        .try_acquire_owned()
                        .ok()
                        .map(|p| Slot { server, permit: Some(p), shared: shared.clone() })
                });
                if upgrade.is_none() {
                    return Ok(());
                }
                drop_conn(shared, held);
            }
        }
    }
    let slot = match upgrade {
        Some(s) => s,
        None => acquire_slot(shared, tried, prefer).await?,
    };
    let server = slot.server;
    let slot_info = &shared.servers[server];
    if let Some(conn) = shared.pool.take(&slot_info.cfg) {
        *held = Some(Held { conn, prefetched: None, untested: true, slot });
        return Ok(());
    }
    wait_cooldown(shared, server).await?;
    let res = tokio::select! {
        _ = shared.abort.cancelled() => return Err(ConnFail::Stopped),
        r = NntpConnection::connect(&slot_info.cfg, shared.connect_timeout) => r,
    };
    match res {
        Ok(conn) => {
            slot_info.note_connected();
            *held = Some(Held { conn, prefetched: None, untested: false, slot });
            Ok(())
        }
        Err(NntpError::AuthFailed(msg)) => Err(ConnFail::Auth { server, msg }),
        Err(NntpError::AuthRequired) => Err(ConnFail::Auth {
            server,
            msg: "server requires authentication but no username is configured".into(),
        }),
        Err(err) => {
            slot_info.note_connect_failure();
            Err(ConnFail::Transient { server, err })
        }
    }
}

/// Sleeps out the server's connect cooldown, if one is active.
async fn wait_cooldown(shared: &Shared, server: usize) -> Result<(), ConnFail> {
    let until = *lock(&shared.servers[server].cooldown);
    if let Some(until) = until {
        let now = Instant::now();
        if until > now {
            tokio::select! {
                _ = shared.abort.cancelled() => return Err(ConnFail::Stopped),
                _ = tokio::time::sleep(until - now) => {}
            }
        }
    }
    Ok(())
}

/// Fetches, decodes and stores one segment, walking servers as needed.
async fn process_segment(shared: &Arc<Shared>, held: &mut Option<Held>, st: &mut SegmentState) -> Flow {
    let (fi, si) = (st.fi, st.si);
    let file = &shared.files[fi];
    let seg = &file.segments[si];

    loop {
        if shared.abort.is_cancelled() {
            return Flow::Stop;
        }
        // A file's first article may be requested ahead before the file turns out to be
        // complete from an earlier run; its reply is still read (and dropped by
        // `store_part`) so the next reply on the connection is the next segment's.
        if file.is_skipped() && held.as_ref().is_none_or(|h| h.conn.outstanding_bodies() == 0) {
            return finish_segment(shared, fi, si, None).await;
        }
        // A connection with a pipelined reply pending can only be used for that reply:
        // step aside when it cannot serve this segment any more.
        if let Some(h) = held.as_ref() {
            if h.prefetched.is_some() && !h.conn.is_broken() && !shared.can_serve(h.slot.server, &st.tried) {
                return Flow::Yield;
            }
        }
        match ensure_conn(shared, held, &st.tried, st.prefer).await {
            Ok(()) => {}
            Err(ConnFail::Stopped) => return Flow::Stop,
            Err(ConnFail::NoEligible) => {
                if shared.servers.iter().all(|s| s.unusable.load(Ordering::SeqCst)) {
                    return Flow::Stop; // mark_unusable already recorded AuthFailed.
                }
                return record_missing(shared, fi, si, st.reason.take()).await;
            }
            Err(ConnFail::Auth { server, msg }) => {
                shared.mark_unusable(server, &msg);
                continue;
            }
            Err(ConnFail::Transient { server, err }) => {
                match retry_wait(shared, &mut st.failures, &mut st.prefer, server, &err, st.failover).await {
                    Retry::Again => continue,
                    Retry::SkipServer => {
                        st.reason = Some(skip_server(shared, &mut st.tried, server, &seg.message_id, &err));
                        continue;
                    }
                    Retry::Stop => return Flow::Stop,
                }
            }
        }
        let Some(h) = held.as_mut() else { continue };
        let server = h.slot.server;
        let untested = h.untested;
        let res = tokio::select! {
            biased;
            _ = shared.abort.cancelled() => {
                drop_conn(shared, held);
                return Flow::Stop;
            }
            r = fetch(shared, h, &seg.message_id) => r,
        };
        match res {
            Ok(raw) => {
                let received = raw.len() as u64;
                if shared.limiter.limited() {
                    tokio::select! {
                        _ = shared.abort.cancelled() => return Flow::Stop,
                        _ = shared.limiter.acquire(raw.len()) => {}
                    }
                }
                // Once the file's output is decided, its handle goes along so decode,
                // check and write run as one blocking task (see `Decoded`).
                let handle = open_handle(file);
                let checked = {
                    // Decode and verify on the blocking pool so a slow decode never
                    // stalls another connection's socket task.
                    let shared = shared.clone();
                    tokio::task::spawn_blocking(move || {
                        let part = yenc::decode(&raw).map_err(|e| format!("could not be decoded: {e}"))?;
                        shared.files[fi]
                            .check_part(&part)
                            .map_err(|why| format!("does not belong to this file: {why}"))?;
                        let written = handle.map(|h| write_all_at(&h, &part.data, part.begin.saturating_sub(1)));
                        Ok::<_, String>(Decoded { part, written })
                    })
                    .await
                };
                match checked {
                    Ok(Ok(decoded)) => {
                        shared.progress.health(&shared.servers[server].address, |h| {
                            h.articles += 1;
                            h.bytes += received;
                        });
                        return store_part(shared, fi, si, decoded).await;
                    }
                    Ok(Err(why)) => {
                        shared.note_error(server, &why);
                        let msg = format!(
                            "{}: article <{}> from {} {why}",
                            file.label, seg.message_id, shared.servers[server].address
                        );
                        shared.log_bad_article(&msg);
                        st.reason = Some(msg);
                        st.tried[server] = true;
                        st.failover = true;
                    }
                    Err(e) => {
                        shared.fail(EngineError::Io(io::Error::other(format!("decode task failed: {e}"))));
                        return Flow::Stop;
                    }
                }
            }
            Err(NntpError::ArticleMissing(_)) => {
                shared.progress.health(&shared.servers[server].address, |h| h.missing += 1);
                st.tried[server] = true;
                st.failover = true;
                if let Some(&next) = shared.order(&st.tried, server).first() {
                    let n = shared.failovers.fetch_add(1, Ordering::Relaxed);
                    if n < FAILOVER_LOG_LIMIT {
                        shared.log(format!(
                            "Article <{}> not on {}; trying {}",
                            seg.message_id, shared.servers[server].address, shared.servers[next].address
                        ));
                    }
                }
            }
            Err(NntpError::AuthFailed(msg)) => {
                drop_conn(shared, held);
                shared.mark_unusable(server, &msg);
            }
            Err(NntpError::AuthRequired) => {
                drop_conn(shared, held);
                shared.mark_unusable(server, "server requires authentication");
            }
            Err(NntpError::InvalidMessageId(id)) => {
                let msg = format!("{}: invalid message id {id:?}", file.label);
                shared.log_bad_article(&msg);
                return record_missing(shared, fi, si, Some(msg)).await;
            }
            Err(err) => {
                drop_conn(shared, held);
                if untested {
                    // A pooled connection the server had already closed: not a failure of
                    // the server or the segment, just open a fresh one.
                    tracing::debug!(target: "newskite_engine", address = %shared.servers[server].address, error = %err, "pooled connection was dead; reconnecting");
                    continue;
                }
                match retry_wait(shared, &mut st.failures, &mut st.prefer, server, &err, st.failover).await {
                    Retry::Again => {}
                    Retry::SkipServer => {
                        st.reason = Some(skip_server(shared, &mut st.tried, server, &seg.message_id, &err));
                    }
                    Retry::Stop => return Flow::Stop,
                }
            }
        }
    }
}

/// Fetches one body on `h`, pipelining the request for the worker's next segment behind
/// it. The request for `message_id` is already on the wire when this segment was itself
/// prefetched.
async fn fetch(shared: &Arc<Shared>, h: &mut Held, message_id: &str) -> Result<Vec<u8>, NntpError> {
    if h.conn.outstanding_bodies() == 0 {
        h.conn.send_body(message_id).await?;
    }
    h.conn.begin_body(message_id).await?;
    h.untested = false;
    prefetch_next(shared, h).await;
    h.conn.finish_body().await
}

/// Sends the `BODY` request for the next queued segment while the current body is still
/// streaming. Only on the primary server, so a worker that spilled over to a fallback
/// returns to the primary as soon as it has room, and only for a file's first article or
/// files whose output is already decided: the first decoded part of a file may reveal it
/// as complete from an earlier run, and then no second part should have been requested,
/// but the first article is needed either way (the queue starts with the first article
/// of every file, which would otherwise all go unpipelined). Segments of skipped files
/// are completed on the spot instead.
async fn prefetch_next(shared: &Arc<Shared>, h: &mut Held) {
    if shared.primary_server() != Some(h.slot.server) {
        return;
    }
    while let Some(item) = shared.next_item() {
        let file = &shared.files[item.0];
        if file.is_skipped() {
            if let Flow::Stop = finish_segment(shared, item.0, item.1, None).await {
                return;
            }
            continue;
        }
        if item.1 != 0 && file.output.get().is_none() {
            shared.requeue(item);
            return;
        }
        match h.conn.send_body(&file.segments[item.1].message_id).await {
            Ok(()) => h.prefetched = Some(item),
            // A bad id or a dead socket: the segment is handled, and the error reported,
            // when it is processed the ordinary way.
            Err(_) => shared.requeue(item),
        }
        return;
    }
}

/// `delay` with ±25 % random jitter, so workers that failed together do not retry together.
fn jittered(delay: Duration) -> Duration {
    use std::hash::{BuildHasher, RandomState};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // RandomState is randomly seeded per thread; hashing a counter gives fresh values.
    let r = RandomState::new().hash_one(n) % 51;
    delay.mul_f64(0.75 + r as f64 / 100.0)
}

/// Outcome of [`retry_wait`].
enum Retry {
    /// Backoff slept; try again.
    Again,
    /// Retries exhausted while failing over: treat the article as not available on this
    /// server (the failure counter is reset for the next one).
    SkipServer,
    /// The job was aborted (or cancelled).
    Stop,
}

/// Counts a transient failure; either sleeps the backoff (and moves on to the next
/// server) or, once retries are exhausted, gives up on the server for this article (when
/// `failover`, i.e. another server already answered that it lacks the article) or aborts
/// the job with `Connect`.
async fn retry_wait(
    shared: &Shared,
    failures: &mut u32,
    prefer: &mut usize,
    server: usize,
    err: &NntpError,
    failover: bool,
) -> Retry {
    *failures += 1;
    let address = &shared.servers[server].address;
    if *failures > shared.retries {
        if failover {
            *failures = 0;
            return Retry::SkipServer;
        }
        shared.fail(EngineError::Connect(format!("{address}: {err}")));
        return Retry::Stop;
    }
    shared.note_error(server, &err.to_string());
    let delay = backoff_delay(*failures);
    shared.warn(format!(
        "{address}: {err}; retrying in {} ms (retry {} of {})",
        delay.as_millis(),
        failures,
        shared.retries
    ));
    *prefer = (server + 1) % shared.servers.len();
    tokio::select! {
        _ = shared.abort.cancelled() => Retry::Stop,
        _ = tokio::time::sleep(delay) => Retry::Again,
    }
}

/// Marks `server` as tried for this article after it kept failing during fail-over;
/// returns the logged reason.
fn skip_server(shared: &Shared, tried: &mut [bool], server: usize, message_id: &str, err: &NntpError) -> String {
    tried[server] = true;
    let msg = format!(
        "Article <{message_id}> could not be fetched from {} ({err}); treating it as not available there",
        shared.servers[server].address
    );
    shared.warn(msg.clone());
    msg
}

async fn record_missing(shared: &Arc<Shared>, fi: usize, si: usize, reason: Option<String>) -> Flow {
    let file = &shared.files[fi];
    let seg = &file.segments[si];
    if let Some(r) = reason {
        tracing::debug!(target: "newskite_engine", reason = %r, "segment failed");
    }
    lock(&file.missing).push((seg.number, seg.message_id.clone()));
    shared.emit(EngineEvent::SegmentMissing { file: fi, index: si });
    shared.note_damaged(fi, si);
    finish_segment(shared, fi, si, None).await
}

/// A part decoded and checked on the blocking pool. `written` is the outcome of writing
/// it there, when the file's output was already decided (the steady state: one blocking
/// task per segment); `None` means the first part of a file, which decides the output
/// and is written by [`store_part`] in a task of its own.
struct Decoded {
    part: YencPart,
    written: Option<io::Result<()>>,
}

/// The open `.nkpart` handle of a file whose output is decided and not skipped.
fn open_handle(file: &FileJob) -> Option<Arc<File>> {
    let out = file.output.get()?;
    if out.skipped {
        return None;
    }
    lock(&out.file).clone()
}

/// Stores a decoded part in its file (deciding the output on the first part).
async fn store_part(shared: &Arc<Shared>, fi: usize, si: usize, decoded: Decoded) -> Flow {
    let file = &shared.files[fi];
    let Decoded { part, written } = decoded;
    let part = Arc::new(part);
    let init = {
        let shared = shared.clone();
        let first = part.clone();
        move || async move { open_output(shared, fi, first).await }
    };
    let out = match file.output.get_or_try_init(init).await {
        Ok(o) => o.clone(),
        Err(e) => {
            shared.fail(EngineError::Io(e));
            return Flow::Stop;
        }
    };
    if out.skipped {
        return finish_segment(shared, fi, si, None).await;
    }
    shared.note_part(fi, si, &part);
    if part.crc_ok == Some(false) {
        file.crc_errors.fetch_add(1, Ordering::Relaxed);
        shared.note_damaged(fi, si);
    }
    let len = part.data.len() as u64;
    let end = part.end;
    let written = match written {
        Some(w) => Ok(w),
        None => {
            let Some(handle) = lock(&out.file).clone() else {
                shared.fail(EngineError::Io(io::Error::other(format!("{}: output file is closed", out.name))));
                return Flow::Stop;
            };
            // decode() guarantees begin >= 1.
            let offset = part.begin.saturating_sub(1);
            tokio::task::spawn_blocking(move || write_all_at(&handle, &part.data, offset)).await
        }
    };
    match written {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            shared.fail(EngineError::Io(io::Error::new(e.kind(), format!("writing {}: {e}", out.name))));
            return Flow::Stop;
        }
        Err(e) => {
            shared.fail(EngineError::Io(io::Error::other(format!("write task failed: {e}"))));
            return Flow::Stop;
        }
    }
    file.bytes.fetch_add(len, Ordering::SeqCst);
    file.max_end.fetch_max(end, Ordering::SeqCst);
    shared.bytes_written.fetch_add(len, Ordering::SeqCst);
    shared.progress.bytes_down.fetch_add(len, Ordering::Relaxed);
    finish_segment(shared, fi, si, Some((out.name.as_str(), len))).await
}

/// Counts processed segment `si` of file `fi`, emits `SegmentDone` for stored ones and
/// finalizes the file after its last segment. Finalizing (flushing the data to disk, the
/// rename, reading a PAR2 file) runs as a task of its own so the worker's connection goes
/// on fetching meanwhile; [`run`] waits for it, and a failure stops the job as before.
async fn finish_segment(shared: &Arc<Shared>, fi: usize, si: usize, stored: Option<(&str, u64)>) -> Flow {
    let file = &shared.files[fi];
    let total = file.segments.len();
    let done = {
        let mut p = lock(&file.processed);
        *p += 1;
        if let Some((name, bytes)) = stored {
            shared.emit(EngineEvent::SegmentDone {
                name: name.to_string(),
                done: *p,
                total,
                bytes,
                file: fi,
                index: si,
            });
        }
        *p
    };
    shared.progress.segments_done.fetch_add(1, Ordering::Relaxed);
    if done == total {
        let s = shared.clone();
        lock(&shared.finalizing).spawn(async move {
            if let Err(e) = finalize(&s, fi).await {
                s.fail(EngineError::Io(e));
            }
        });
    }
    Flow::Continue
}

/// Decides the output name for a file and opens its `.nkpart` (or marks it skipped).
async fn open_output(shared: Arc<Shared>, fi: usize, first: Arc<YencPart>) -> io::Result<Arc<Output>> {
    let s = shared.clone();
    let out = tokio::task::spawn_blocking(move || resolve_output(&s, fi, Some(&first), true))
        .await
        .map_err(|e| io::Error::other(format!("opening output failed: {e}")))??;
    let out = Arc::new(out);
    let file = &shared.files[fi];
    if out.skipped {
        shared.log(format!("{}: already complete, skipping", out.name));
        shared.emit(EngineEvent::FileComplete { file: fi });
        let (s, path) = (shared.clone(), out.final_path.clone());
        let _ = tokio::task::spawn_blocking(move || note_on_disk(&s, fi, &path)).await;
    } else {
        shared.emit(EngineEvent::FileStarted { name: out.name.clone(), segments: file.segments.len() });
    }
    Ok(out)
}

/// Blocking: picks a free name (or detects a completed earlier download from the file's
/// first decoded part) and, when `create`, creates the `.nkpart` file.
fn resolve_output(shared: &Shared, fi: usize, first: Option<&YencPart>, create: bool) -> io::Result<Output> {
    let file = &shared.files[fi];
    let base = first
        .map(|p| p.name.as_str())
        .filter(|n| !n.trim().is_empty())
        .map(sanitize_filename)
        .unwrap_or_else(|| file.label.clone());

    if let Some(part) = first {
        if let Some((name, existing)) = completed_earlier(shared, &base, part) {
            return Ok(Output {
                name,
                part_path: part_path(&existing),
                final_path: existing,
                file: Mutex::new(None),
                skipped: true,
            });
        }
    }
    let mut claimed = lock(&shared.claimed);
    let (name, final_path) = free_name(shared, &mut claimed, &base)?;
    drop(claimed);
    let part = part_path(&final_path);
    let handle = if create {
        let f = OpenOptions::new().create(true).write(true).truncate(true).open(&part)?;
        if let Some(size) = first.map(|p| p.size).filter(|&s| s > 0) {
            preallocate(&f, size).map_err(|e| {
                io::Error::new(e.kind(), format!("{name}: cannot reserve {} on disk: {e}", format::bytes(size)))
            })?;
        }
        Some(Arc::new(f))
    } else {
        None
    };
    Ok(Output { name, final_path, part_path: part, file: Mutex::new(handle), skipped: false })
}

/// Blocking: reserves `size` bytes for a fresh `.nkpart`, so the parts that arrive out of
/// order land in one extent instead of growing the file piecemeal, and a disk without
/// room for the file is noticed on its first part rather than its last. Allocated for
/// real where the file system can (Linux `fallocate`); elsewhere, or when it refuses, the
/// length is set (a sparse file). Only a full disk is an error.
fn preallocate(file: &File, size: u64) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    match rustix::fs::fallocate(file, rustix::fs::FallocateFlags::empty(), 0, size) {
        Ok(()) => return Ok(()),
        Err(rustix::io::Errno::NOSPC) => return Err(io::Error::from(rustix::io::Errno::NOSPC)),
        Err(e) => tracing::debug!(target: "newskite_engine", error = %e, "fallocate unsupported here; setting the length"),
    }
    match file.set_len(size) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::StorageFull => Err(e),
        Err(e) => {
            tracing::debug!(target: "newskite_engine", error = %e, "could not set the part file's length");
            Ok(())
        }
    }
}

/// Blocking: the regular files directly in `dir` with their lengths, by name: everything
/// but in-progress `.nkpart` files and the damaged originals a PAR2 repair keeps as
/// `name.1` (`.2`, ...) next to `name`, which have the right length and start but may be
/// damaged further in.
fn resume_candidates(dir: &Path) -> Vec<OnDisk> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<(String, u64)> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.metadata().ok()?.len())))
        .filter(|(n, _)| !n.to_ascii_lowercase().ends_with(&format!(".{}", crate::PART_EXTENSION)))
        .collect();
    let names: HashSet<String> = files.iter().map(|(n, _)| n.clone()).collect();
    files.retain(|(n, _)| {
        !n.rsplit_once('.').is_some_and(|(stem, num)| !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) && names.contains(stem))
    });
    files.sort();
    files
}

/// Blocking: the `skip_files` entry (`base` or a numbered `"<stem> (n).<ext>"` variant of
/// it, lowest `n` first) that no file of this job claimed yet and whose on-disk content is
/// this file's: same length as the part's yEnc `size=` and the part's bytes at its offset.
/// Matching by content (not by name alone) keeps files whose names collide from being taken
/// for each other, and finds outputs an earlier run saved under a numbered name. Failing
/// that, any other file of the folder with that content (see [`resume_candidates`]): a
/// file post-processing moved to its real name (obfuscated posts), or one renamed by hand.
/// The match is claimed. The files are read without holding the claims, so other workers
/// deciding their outputs are not held up; a match another file claimed meanwhile is
/// passed over and the search repeated without it.
fn completed_earlier(shared: &Shared, base: &str, part: &YencPart) -> Option<(String, PathBuf)> {
    loop {
        let candidates: Vec<&String> = {
            let claimed = lock(&shared.claimed);
            let mut by_name: Vec<(u32, &String)> =
                shared.skip_files.iter().filter_map(|s| suffix_number(s, base).map(|n| (n, s))).collect();
            by_name.sort();
            let by_content = shared.on_disk.iter().filter(|(_, len)| *len == part.size).map(|(name, _)| name);
            by_name
                .into_iter()
                .map(|(_, name)| name)
                .chain(by_content)
                .filter(|name| !claimed.contains(&claim_key(name)))
                .collect()
        };
        let (name, path) = candidates.into_iter().find_map(|name| {
            let path = shared.dest_dir.join(name);
            (path.parent() == Some(shared.dest_dir.as_path()) && holds_part(&path, part)).then(|| (name.clone(), path))
        })?;
        if lock(&shared.claimed).insert(claim_key(&name)) {
            return Some((name, path));
        }
    }
}

/// Blocking: whether the regular file at `path` is `part.size` bytes long and holds the
/// part's data at its offset.
fn holds_part(path: &Path, part: &YencPart) -> bool {
    let Ok(f) = File::open(path) else { return false };
    match f.metadata() {
        Ok(m) if m.is_file() && m.len() == part.size => {}
        _ => return false,
    }
    let mut buf = vec![0u8; part.data.len()];
    read_exact_at(&f, &mut buf, part.begin.saturating_sub(1)).is_ok() && buf == part.data
}

/// First of `base`, `base (2)`, ... that no file of this job claimed and that does not
/// exist in `dest_dir`; claims it.
fn free_name(shared: &Shared, claimed: &mut HashSet<String>, base: &str) -> io::Result<(String, PathBuf)> {
    for n in 1..=MAX_NAME_SUFFIX {
        let cand = with_suffix(base, n);
        let key = claim_key(&cand);
        if claimed.contains(&key) {
            continue;
        }
        let path = shared.dest_dir.join(&cand);
        if path.symlink_metadata().is_ok() {
            continue;
        }
        // Defense in depth: the sanitized name must be a single component.
        if path.parent() != Some(shared.dest_dir.as_path()) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unsafe output name {cand:?}")));
        }
        claimed.insert(key);
        return Ok((cand, path));
    }
    Err(io::Error::other(format!("no free file name for {base:?}")))
}

/// Completes a file after its last segment: rename on success, report on failure.
async fn finalize(shared: &Arc<Shared>, fi: usize) -> io::Result<()> {
    let file = &shared.files[fi];
    let mut missing = std::mem::take(&mut *lock(&file.missing));
    missing.sort_by_key(|(number, _)| *number);
    let missing: Vec<String> = missing.into_iter().map(|(_, id)| id).collect();
    let crc_errors = file.crc_errors.load(Ordering::SeqCst);
    let bytes = file.bytes.load(Ordering::SeqCst);

    let report = match file.output.get().cloned() {
        Some(out) if out.skipped => skipped_report(shared, file),
        None => {
            // Nothing could be decoded: no file was created.
            let s = shared.clone();
            let out = tokio::task::spawn_blocking(move || resolve_output(&s, fi, None, false))
                .await
                .map_err(|e| io::Error::other(format!("naming output failed: {e}")))??;
            shared.emit(EngineEvent::FileStarted { name: out.name.clone(), segments: file.segments.len() });
            fail_file(shared, &out.name, &missing);
            FileReport { name: out.name, path: out.final_path, bytes: 0, missing, crc_errors }
        }
        Some(out) if !missing.is_empty() => {
            drop(lock(&out.file).take());
            fail_file(shared, &out.name, &missing);
            FileReport { name: out.name.clone(), path: out.part_path.clone(), bytes, missing, crc_errors }
        }
        Some(out) => {
            let handle = lock(&out.file).take();
            let max_end = file.max_end.load(Ordering::SeqCst);
            let hint = file.size_hint.load(Ordering::SeqCst);
            let target = hint.max(max_end);
            if bytes < target {
                shared.warn(format!(
                    "{}: decoded {} of {}; the gaps are zero-filled (the NZB may be incomplete)",
                    out.name,
                    format::bytes(bytes),
                    format::bytes(target)
                ));
            }
            if crc_errors > 0 {
                shared.warn(format!("{}: {crc_errors} segment(s) failed the CRC check; PAR2 may repair them", out.name));
            }
            let s = shared.clone();
            let o = out.clone();
            let (name, path) = tokio::task::spawn_blocking(move || complete_file(&s, &o, handle, target))
                .await
                .map_err(|e| io::Error::other(format!("finishing file failed: {e}")))??;
            shared.emit(EngineEvent::FileDone { name: name.clone(), path: path.clone(), bytes: target });
            shared.log(format!("Completed {name} ({})", format::bytes(target)));
            FileReport { name, path, bytes: target, missing: Vec::new(), crc_errors }
        }
    };
    let path = report.path.clone();
    *lock(&file.report) = Some(report);
    if lock(&shared.repair).is_par2(fi) {
        let s = shared.clone();
        let _ = tokio::task::spawn_blocking(move || read_par2(&s, fi, &path)).await;
    }
    shared.update_outlook();
    Ok(())
}

fn fail_file(shared: &Shared, name: &str, missing: &[String]) {
    let preview: Vec<String> = missing.iter().take(MISSING_PREVIEW).map(|id| format!("<{id}>")).collect();
    let more = if missing.len() > MISSING_PREVIEW { ", ..." } else { "" };
    shared.warn(format!("{name}: {} missing article(s): {}{more}", missing.len(), preview.join(", ")));
    shared.emit(EngineEvent::FileFailed { name: name.to_string(), missing: missing.to_vec() });
}

/// Blocking: sets the final length, flushes the data to disk, closes the handle and
/// renames `.nkpart` to the final name (picking a fresh name if something appeared there
/// meanwhile), then flushes the directory entry.
///
/// The flushes make the file durable before `FileDone` lets a caller record it as complete:
/// otherwise a power loss could keep the rename (and the caller's record) but lose the
/// delayed-allocation data, and a resume would skip a file full of zeros.
fn complete_file(shared: &Shared, out: &Output, handle: Option<Arc<File>>, len: u64) -> io::Result<(String, PathBuf)> {
    let handle = match handle {
        Some(h) => h,
        None => Arc::new(OpenOptions::new().write(true).open(&out.part_path)?),
    };
    handle.set_len(len)?;
    handle.sync_data()?;
    drop(handle);
    let (name, path) = if out.final_path.symlink_metadata().is_ok() {
        let mut claimed = lock(&shared.claimed);
        let picked = free_name(shared, &mut claimed, &out.name)?;
        drop(claimed);
        shared.log(format!("{} appeared during the download; saving as {}", out.name, picked.0));
        picked
    } else {
        (out.name.clone(), out.final_path.clone())
    };
    std::fs::rename(&out.part_path, &path)?;
    sync_dir(&shared.dest_dir);
    Ok((name, path))
}

/// Best effort: flushes a directory so a rename inside it is durable. Not possible (nor
/// needed: NTFS journals metadata) on Windows; failures are only logged at debug level
/// because some file systems do not support it.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Err(e) = File::open(dir).and_then(|d| d.sync_all()) {
        tracing::debug!(target: "newskite_engine", dir = %dir.display(), error = %e, "directory flush failed");
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_preview_format() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Shared {
            dest_dir: PathBuf::from("."),
            servers: Vec::new(),
            pool: Arc::new(ConnectionPool::new()),
            released: Notify::new(),
            files: Vec::new(),
            queue: Mutex::new(VecDeque::new()),
            claimed: Mutex::new(HashSet::new()),
            skip_files: HashSet::new(),
            retries: 0,
            connect_timeout: Duration::from_secs(1),
            limiter: RateLimiter::new(crate::SpeedLimit::default()),
            events: tx,
            progress: Arc::new(Progress::new()),
            abort: CancellationToken::new(),
            fatal: Mutex::new(None),
            bytes_written: AtomicU64::new(0),
            failovers: AtomicUsize::new(0),
            bad_articles: AtomicUsize::new(0),
            repair: Mutex::new(Tracker::new(Vec::new(), 0)),
            stop_unrepairable: false,
            on_disk: Vec::new(),
            finalizing: Mutex::new(JoinSet::new()),
        };
        let ids: Vec<String> = (1..=5).map(|i| format!("id{i}@x")).collect();
        fail_file(&shared, "f.bin", &ids);
        match rx.try_recv().unwrap() {
            EngineEvent::Warn(l) => assert_eq!(l, "f.bin: 5 missing article(s): <id1@x>, <id2@x>, <id3@x>, ..."),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(rx.try_recv().unwrap(), EngineEvent::FileFailed { name: "f.bin".into(), missing: ids });
    }

    #[test]
    fn bad_article_lines_are_capped_and_summarized() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Shared {
            dest_dir: PathBuf::from("."),
            servers: Vec::new(),
            pool: Arc::new(ConnectionPool::new()),
            released: Notify::new(),
            files: Vec::new(),
            queue: Mutex::new(VecDeque::new()),
            claimed: Mutex::new(HashSet::new()),
            skip_files: HashSet::new(),
            retries: 0,
            connect_timeout: Duration::from_secs(1),
            limiter: RateLimiter::new(crate::SpeedLimit::default()),
            events: tx,
            progress: Arc::new(Progress::new()),
            abort: CancellationToken::new(),
            fatal: Mutex::new(None),
            bytes_written: AtomicU64::new(0),
            failovers: AtomicUsize::new(0),
            bad_articles: AtomicUsize::new(0),
            repair: Mutex::new(Tracker::new(Vec::new(), 0)),
            stop_unrepairable: false,
            on_disk: Vec::new(),
            finalizing: Mutex::new(JoinSet::new()),
        };
        for i in 0..60 {
            shared.log_bad_article(&format!("f.bin: article <{i}@x> could not be decoded"));
        }
        log_bad_article_summary(&shared);
        let mut lines = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let EngineEvent::Warn(l) = ev {
                lines.push(l);
            }
        }
        assert_eq!(lines.len(), BAD_ARTICLE_LOG_LIMIT + 1, "{lines:?}");
        assert!(lines[0].contains("<0@x>"));
        assert!(lines.last().is_some_and(|l| l.starts_with("60 article fetch(es) were rejected")));
    }

    #[test]
    fn order_rotates_and_filters() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mk = |h: &str| ServerSlot::new(ServerConfig { host: h.into(), connections: 1, ..Default::default() });
        let shared = Shared {
            dest_dir: PathBuf::from("."),
            servers: vec![mk("a"), mk("b"), mk("c")],
            pool: Arc::new(ConnectionPool::new()),
            released: Notify::new(),
            files: Vec::new(),
            queue: Mutex::new(VecDeque::new()),
            claimed: Mutex::new(HashSet::new()),
            skip_files: HashSet::new(),
            retries: 0,
            connect_timeout: Duration::from_secs(1),
            limiter: RateLimiter::new(crate::SpeedLimit::default()),
            events: tx,
            progress: Arc::new(Progress::new()),
            abort: CancellationToken::new(),
            fatal: Mutex::new(None),
            bytes_written: AtomicU64::new(0),
            failovers: AtomicUsize::new(0),
            bad_articles: AtomicUsize::new(0),
            repair: Mutex::new(Tracker::new(Vec::new(), 0)),
            stop_unrepairable: false,
            on_disk: Vec::new(),
            finalizing: Mutex::new(JoinSet::new()),
        };
        assert_eq!(shared.order(&[false, false, false], 0), vec![0, 1, 2]);
        assert_eq!(shared.order(&[false, false, false], 2), vec![2, 0, 1]);
        assert_eq!(shared.order(&[true, false, false], 0), vec![1, 2]);
        assert_eq!(shared.primary_server(), Some(0));
        assert!(shared.can_serve(1, &[true, false, false]));
        shared.servers[1].unusable.store(true, Ordering::SeqCst);
        assert_eq!(shared.order(&[true, false, false], 0), vec![2]);
        assert!(!shared.can_serve(1, &[true, false, false]));
        shared.servers[0].unusable.store(true, Ordering::SeqCst);
        assert_eq!(shared.primary_server(), Some(2));
    }

    #[test]
    fn jitter_stays_within_a_quarter() {
        for _ in 0..200 {
            let d = jittered(Duration::from_millis(1000));
            assert!(d >= Duration::from_millis(750) && d <= Duration::from_millis(1250), "{d:?}");
        }
    }

    #[test]
    fn connect_failures_set_a_growing_cooldown() {
        let slot = ServerSlot::new(ServerConfig { host: "a".into(), ..Default::default() });
        assert!(lock(&slot.cooldown).is_none());
        slot.note_connect_failure();
        let first = lock(&slot.cooldown).expect("cooldown set");
        slot.note_connect_failure();
        let second = lock(&slot.cooldown).expect("cooldown set");
        assert!(second > first);
        assert_eq!(slot.strikes.load(Ordering::SeqCst), 2);
        slot.note_connected();
        assert!(lock(&slot.cooldown).is_none());
        assert_eq!(slot.strikes.load(Ordering::SeqCst), 0);
    }
}
