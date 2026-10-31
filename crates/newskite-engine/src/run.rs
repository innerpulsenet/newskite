//! The download run: work queue, worker pool, per-server connection slots, output files.

use std::collections::{HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use newskite_nntp::{NntpConnection, NntpError};
use newskite_nzb::nzb::{sanitize_filename, Segment};
use newskite_nzb::yenc::{self, YencPart};
use newskite_types::{format, EngineEvent, ServerConfig};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Notify, OnceCell, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::limiter::RateLimiter;
use crate::naming::{claim_key, part_path, with_suffix};
use crate::pwrite::write_all_at;
use crate::{backoff_delay, DownloadRequest, EngineError, FileReport, JobReport, Progress};

/// How long a polite `QUIT` may take at the end of a run.
const QUIT_TIMEOUT: Duration = Duration::from_secs(2);
/// Message ids listed in a missing-article summary line.
const MISSING_PREVIEW: usize = 3;
/// Individual fail-over log lines per job before going quiet.
const FAILOVER_LOG_LIMIT: usize = 5;
/// Upper bound when searching for a free `"name (n).ext"`.
const MAX_NAME_SUFFIX: u32 = 10_000;

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
    /// yEnc `size=` of the first decoded part; 0 = unknown.
    size_hint: AtomicU64,
    crc_errors: AtomicUsize,
    missing: Mutex<Vec<(u32, String)>>,
    report: Mutex<Option<FileReport>>,
}

impl FileJob {
    fn new(label: String, segments: Vec<Segment>) -> Self {
        Self {
            label,
            segments,
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
}

struct Shared {
    dest_dir: PathBuf,
    servers: Vec<ServerSlot>,
    /// Notified whenever a connection slot is released or a server becomes unusable.
    released: Notify,
    files: Vec<FileJob>,
    queue: Mutex<VecDeque<(usize, usize)>>,
    /// Claimed output names (see [`claim_key`]).
    claimed: Mutex<HashSet<String>>,
    skip_files: HashSet<String>,
    retries: u32,
    connect_timeout: Duration,
    limiter: Option<RateLimiter>,
    events: UnboundedSender<EngineEvent>,
    progress: Arc<Progress>,
    /// Child of the caller's token; also fired on fatal errors.
    abort: CancellationToken,
    fatal: Mutex<Option<EngineError>>,
    bytes_written: AtomicU64,
    failovers: AtomicUsize,
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

    fn next_item(&self) -> Option<(usize, usize)> {
        lock(&self.queue).pop_front()
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
        if !slot.unusable.swap(true, Ordering::SeqCst) {
            self.log(format!(
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
    // Declared after `conn` so the socket closes before the slot is handed on.
    slot: Slot,
}

enum ConnFail {
    Stopped,
    NoEligible,
    Auth { server: usize, msg: String },
    Transient { server: usize, err: NntpError },
}

enum Flow {
    Continue,
    Stop,
}

pub(crate) async fn run(
    req: DownloadRequest,
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
    for f in &nzb.files {
        let base = f.filename();
        let label = (1..=MAX_NAME_SUFFIX)
            .map(|n| with_suffix(&base, n))
            .find(|cand| seen.insert(claim_key(cand)))
            .unwrap_or(base);
        files.push(FileJob::new(label, f.segments.clone()));
    }

    // Create the destination and find files already complete from an earlier run.
    let labels: Vec<String> = files.iter().map(|f| f.label.clone()).collect();
    let skip_files = options.skip_files.clone();
    let dir = dest_dir.clone();
    let pre_skipped: Vec<bool> = tokio::task::spawn_blocking(move || -> io::Result<Vec<bool>> {
        std::fs::create_dir_all(&dir)?;
        Ok(labels.iter().map(|l| skip_files.contains(l) && dir.join(l).is_file()).collect())
    })
    .await
    .map_err(|e| io::Error::other(format!("preparing the download folder failed: {e}")))??;

    let mut claimed = HashSet::new();
    let mut queue = VecDeque::new();
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
            queue.extend((0..file.segments.len()).map(|si| (fi, si)));
        }
    }

    let total_segments = queue.len();
    let slots: Vec<ServerSlot> = servers
        .into_iter()
        .map(|cfg| ServerSlot {
            address: cfg.address(),
            permits: Arc::new(Semaphore::new(usize::from(cfg.connections.max(1)))),
            unusable: AtomicBool::new(false),
            cfg,
        })
        .collect();
    let capacity: usize = slots.iter().map(|s| usize::from(s.cfg.connections.max(1))).sum();
    let workers = options.connections.max(1).min(capacity).min(total_segments);

    let shared = Arc::new(Shared {
        dest_dir,
        servers: slots,
        released: Notify::new(),
        files,
        queue: Mutex::new(queue),
        claimed: Mutex::new(claimed),
        skip_files: options.skip_files,
        retries: options.retries,
        connect_timeout: options.connect_timeout,
        limiter: RateLimiter::new(options.speed_limit_bps),
        events,
        progress,
        abort: cancel.child_token(),
        fatal: Mutex::new(None),
        bytes_written: AtomicU64::new(0),
        failovers: AtomicUsize::new(0),
    });

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

    let mut set = JoinSet::new();
    for _ in 0..workers {
        set.spawn(worker(shared.clone()));
    }
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            shared.fail(EngineError::Io(io::Error::other(format!("download worker failed: {e}"))));
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
    if !missing.is_empty() {
        shared.log(format!("Download incomplete: {} missing article(s)", missing.len()));
        return Err(EngineError::MissingArticles { ids: missing });
    }
    let bytes_written = shared.bytes_written.load(Ordering::SeqCst);
    shared.log(format!("Download complete: {} file(s), {} written", reports.len(), format::bytes(bytes_written)));
    Ok(JobReport { files: reports, bytes_written })
}

fn skipped_report(shared: &Shared, file: &FileJob) -> FileReport {
    let (name, path) = match file.output.get() {
        Some(o) => (o.name.clone(), o.final_path.clone()),
        None => (file.label.clone(), shared.dest_dir.join(&file.label)),
    };
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    FileReport { name, path, bytes, missing: Vec::new(), crc_errors: 0 }
}

async fn worker(shared: Arc<Shared>) {
    let mut held: Option<Held> = None;
    while !shared.abort.is_cancelled() {
        let Some((fi, si)) = shared.next_item() else { break };
        if let Flow::Stop = process_segment(&shared, &mut held, fi, si).await {
            break;
        }
    }
    if let Some(h) = held.take() {
        if shared.abort.is_cancelled() {
            return; // Just drop the socket.
        }
        let Held { conn, slot } = h;
        let _ = tokio::time::timeout(QUIT_TIMEOUT, conn.quit()).await;
        drop(slot);
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
        *held = None;
        return Err(ConnFail::NoEligible);
    }
    let mut upgrade = None;
    if let Some(h) = held.as_ref() {
        match order.iter().position(|&s| s == h.slot.server) {
            Some(_) if h.conn.is_broken() => *held = None,
            None => *held = None,
            Some(0) => return Ok(()),
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
                *held = None;
            }
        }
    }
    let slot = match upgrade {
        Some(s) => s,
        None => acquire_slot(shared, tried, prefer).await?,
    };
    let server = slot.server;
    let cfg = &shared.servers[server].cfg;
    let res = tokio::select! {
        _ = shared.abort.cancelled() => return Err(ConnFail::Stopped),
        r = NntpConnection::connect(cfg, shared.connect_timeout) => r,
    };
    match res {
        Ok(conn) => {
            *held = Some(Held { conn, slot });
            Ok(())
        }
        Err(NntpError::AuthFailed(msg)) => Err(ConnFail::Auth { server, msg }),
        Err(NntpError::AuthRequired) => Err(ConnFail::Auth {
            server,
            msg: "server requires authentication but no username is configured".into(),
        }),
        Err(err) => Err(ConnFail::Transient { server, err }),
    }
}

/// Fetches, decodes and stores one segment, walking servers as needed.
async fn process_segment(shared: &Arc<Shared>, held: &mut Option<Held>, fi: usize, si: usize) -> Flow {
    let file = &shared.files[fi];
    if file.is_skipped() {
        return finish_segment(shared, fi, None).await;
    }
    let seg = &file.segments[si];
    let mut tried = vec![false; shared.servers.len()];
    let mut failures = 0u32;
    let mut prefer = 0usize;
    let mut reason: Option<String> = None;

    loop {
        if shared.abort.is_cancelled() {
            return Flow::Stop;
        }
        match ensure_conn(shared, held, &tried, prefer).await {
            Ok(()) => {}
            Err(ConnFail::Stopped) => return Flow::Stop,
            Err(ConnFail::NoEligible) => {
                if shared.servers.iter().all(|s| s.unusable.load(Ordering::SeqCst)) {
                    return Flow::Stop; // mark_unusable already recorded AuthFailed.
                }
                return record_missing(shared, fi, si, reason).await;
            }
            Err(ConnFail::Auth { server, msg }) => {
                shared.mark_unusable(server, &msg);
                continue;
            }
            Err(ConnFail::Transient { server, err }) => {
                match retry_wait(shared, &mut failures, &mut prefer, server, &err).await {
                    Flow::Continue => continue,
                    Flow::Stop => return Flow::Stop,
                }
            }
        }
        let Some(h) = held.as_mut() else { continue };
        let server = h.slot.server;
        let res = tokio::select! {
            biased;
            _ = shared.abort.cancelled() => {
                *held = None;
                return Flow::Stop;
            }
            r = h.conn.body(&seg.message_id) => r,
        };
        match res {
            Ok(raw) => {
                if let Some(limiter) = &shared.limiter {
                    tokio::select! {
                        _ = shared.abort.cancelled() => return Flow::Stop,
                        _ = limiter.acquire(raw.len()) => {}
                    }
                }
                match yenc::decode(&raw) {
                    Ok(part) => {
                        drop(raw);
                        return store_part(shared, fi, part).await;
                    }
                    Err(e) => {
                        let msg = format!(
                            "{}: article <{}> from {} could not be decoded: {e}",
                            file.label, seg.message_id, shared.servers[server].address
                        );
                        shared.log(msg.clone());
                        reason = Some(msg);
                        tried[server] = true;
                    }
                }
            }
            Err(NntpError::ArticleMissing(_)) => {
                tried[server] = true;
                if let Some(&next) = shared.order(&tried, server).first() {
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
                *held = None;
                shared.mark_unusable(server, &msg);
            }
            Err(NntpError::AuthRequired) => {
                *held = None;
                shared.mark_unusable(server, "server requires authentication");
            }
            Err(NntpError::InvalidMessageId(id)) => {
                let msg = format!("{}: invalid message id {id:?}", file.label);
                shared.log(msg.clone());
                return record_missing(shared, fi, si, Some(msg)).await;
            }
            Err(err) => {
                *held = None;
                match retry_wait(shared, &mut failures, &mut prefer, server, &err).await {
                    Flow::Continue => {}
                    Flow::Stop => return Flow::Stop,
                }
            }
        }
    }
}

/// Counts a transient failure; either sleeps the backoff (and moves on to the next
/// server) or, once retries are exhausted, aborts the job with `Connect`.
async fn retry_wait(
    shared: &Shared,
    failures: &mut u32,
    prefer: &mut usize,
    server: usize,
    err: &NntpError,
) -> Flow {
    *failures += 1;
    let address = &shared.servers[server].address;
    if *failures > shared.retries {
        shared.fail(EngineError::Connect(format!("{address}: {err}")));
        return Flow::Stop;
    }
    let delay = backoff_delay(*failures);
    shared.log(format!(
        "{address}: {err}; retrying in {} ms (retry {} of {})",
        delay.as_millis(),
        failures,
        shared.retries
    ));
    *prefer = (server + 1) % shared.servers.len();
    tokio::select! {
        _ = shared.abort.cancelled() => Flow::Stop,
        _ = tokio::time::sleep(delay) => Flow::Continue,
    }
}

async fn record_missing(shared: &Arc<Shared>, fi: usize, si: usize, reason: Option<String>) -> Flow {
    let file = &shared.files[fi];
    let seg = &file.segments[si];
    if let Some(r) = reason {
        tracing::debug!(target: "newskite_engine", reason = %r, "segment failed");
    }
    lock(&file.missing).push((seg.number, seg.message_id.clone()));
    finish_segment(shared, fi, None).await
}

/// Writes a decoded part into its file (deciding the output on the first part).
async fn store_part(shared: &Arc<Shared>, fi: usize, part: YencPart) -> Flow {
    let file = &shared.files[fi];
    let name_hint = part.name.clone();
    let init = {
        let shared = shared.clone();
        move || async move { open_output(shared, fi, Some(name_hint)).await }
    };
    let out = match file.output.get_or_try_init(init).await {
        Ok(o) => o.clone(),
        Err(e) => {
            shared.fail(EngineError::Io(e));
            return Flow::Stop;
        }
    };
    if out.skipped {
        return finish_segment(shared, fi, None).await;
    }
    let _ = file.size_hint.compare_exchange(0, part.size, Ordering::SeqCst, Ordering::SeqCst);
    if part.crc_ok == Some(false) {
        file.crc_errors.fetch_add(1, Ordering::Relaxed);
    }
    let Some(handle) = lock(&out.file).clone() else {
        shared.fail(EngineError::Io(io::Error::other(format!("{}: output file is closed", out.name))));
        return Flow::Stop;
    };
    let len = part.data.len() as u64;
    let end = part.end;
    // decode() guarantees begin >= 1.
    let offset = part.begin.saturating_sub(1);
    let data = part.data;
    let written = tokio::task::spawn_blocking(move || write_all_at(&handle, &data, offset)).await;
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
    finish_segment(shared, fi, Some((out.name.as_str(), len))).await
}

/// Counts a processed segment, emits `SegmentDone` for stored ones and finalizes the
/// file after its last segment.
async fn finish_segment(shared: &Arc<Shared>, fi: usize, stored: Option<(&str, u64)>) -> Flow {
    let file = &shared.files[fi];
    let total = file.segments.len();
    let done = {
        let mut p = lock(&file.processed);
        *p += 1;
        if let Some((name, bytes)) = stored {
            shared.emit(EngineEvent::SegmentDone { name: name.to_string(), done: *p, total, bytes });
        }
        *p
    };
    shared.progress.segments_done.fetch_add(1, Ordering::Relaxed);
    if done == total {
        if let Err(e) = finalize(shared, fi).await {
            shared.fail(EngineError::Io(e));
            return Flow::Stop;
        }
    }
    Flow::Continue
}

/// Decides the output name for a file and opens its `.nkpart` (or marks it skipped).
async fn open_output(shared: Arc<Shared>, fi: usize, yenc_name: Option<String>) -> io::Result<Arc<Output>> {
    let s = shared.clone();
    let out = tokio::task::spawn_blocking(move || resolve_output(&s, fi, yenc_name, true))
        .await
        .map_err(|e| io::Error::other(format!("opening output failed: {e}")))??;
    let out = Arc::new(out);
    let file = &shared.files[fi];
    if out.skipped {
        shared.log(format!("{}: already complete, skipping", out.name));
    } else {
        shared.emit(EngineEvent::FileStarted { name: out.name.clone(), segments: file.segments.len() });
    }
    Ok(out)
}

/// Blocking: picks a free name (or detects a completed earlier download) and, when
/// `create`, creates the `.nkpart` file.
fn resolve_output(shared: &Shared, fi: usize, yenc_name: Option<String>, create: bool) -> io::Result<Output> {
    let file = &shared.files[fi];
    let base = yenc_name
        .filter(|n| !n.trim().is_empty())
        .map(|n| sanitize_filename(&n))
        .unwrap_or_else(|| file.label.clone());
    let mut claimed = lock(&shared.claimed);

    let existing = shared.dest_dir.join(&base);
    if shared.skip_files.contains(&base) && !claimed.contains(&claim_key(&base)) && existing.is_file() {
        claimed.insert(claim_key(&base));
        return Ok(Output {
            name: base,
            part_path: part_path(&existing),
            final_path: existing,
            file: Mutex::new(None),
            skipped: true,
        });
    }
    let (name, final_path) = free_name(shared, &mut claimed, &base)?;
    drop(claimed);
    let part = part_path(&final_path);
    let handle = if create {
        let f = OpenOptions::new().create(true).write(true).truncate(true).open(&part)?;
        Some(Arc::new(f))
    } else {
        None
    };
    Ok(Output { name, final_path, part_path: part, file: Mutex::new(handle), skipped: false })
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
                shared.log(format!(
                    "{}: decoded {} of {}; the gaps are zero-filled (the NZB may be incomplete)",
                    out.name,
                    format::bytes(bytes),
                    format::bytes(target)
                ));
            }
            if crc_errors > 0 {
                shared.log(format!("{}: {crc_errors} segment(s) failed the CRC check; PAR2 may repair them", out.name));
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
    *lock(&file.report) = Some(report);
    Ok(())
}

fn fail_file(shared: &Shared, name: &str, missing: &[String]) {
    let preview: Vec<String> = missing.iter().take(MISSING_PREVIEW).map(|id| format!("<{id}>")).collect();
    let more = if missing.len() > MISSING_PREVIEW { ", ..." } else { "" };
    shared.log(format!("{name}: {} missing article(s): {}{more}", missing.len(), preview.join(", ")));
    shared.emit(EngineEvent::FileFailed { name: name.to_string(), missing: missing.to_vec() });
}

/// Blocking: sets the final length, closes the handle and renames `.nkpart` to the final
/// name (picking a fresh name if something appeared there meanwhile).
fn complete_file(shared: &Shared, out: &Output, handle: Option<Arc<File>>, len: u64) -> io::Result<(String, PathBuf)> {
    match handle {
        Some(h) => h.set_len(len)?,
        None => OpenOptions::new().write(true).open(&out.part_path)?.set_len(len)?,
    }
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
    Ok((name, path))
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
            released: Notify::new(),
            files: Vec::new(),
            queue: Mutex::new(VecDeque::new()),
            claimed: Mutex::new(HashSet::new()),
            skip_files: HashSet::new(),
            retries: 0,
            connect_timeout: Duration::from_secs(1),
            limiter: None,
            events: tx,
            progress: Arc::new(Progress::new()),
            abort: CancellationToken::new(),
            fatal: Mutex::new(None),
            bytes_written: AtomicU64::new(0),
            failovers: AtomicUsize::new(0),
        };
        let ids: Vec<String> = (1..=5).map(|i| format!("id{i}@x")).collect();
        fail_file(&shared, "f.bin", &ids);
        match rx.try_recv().unwrap() {
            EngineEvent::Log(l) => assert_eq!(l, "f.bin: 5 missing article(s): <id1@x>, <id2@x>, <id3@x>, ..."),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(rx.try_recv().unwrap(), EngineEvent::FileFailed { name: "f.bin".into(), missing: ids });
    }

    #[test]
    fn order_rotates_and_filters() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mk = |h: &str| ServerSlot {
            cfg: ServerConfig { host: h.into(), ..Default::default() },
            address: h.into(),
            permits: Arc::new(Semaphore::new(1)),
            unusable: AtomicBool::new(false),
        };
        let shared = Shared {
            dest_dir: PathBuf::from("."),
            servers: vec![mk("a"), mk("b"), mk("c")],
            released: Notify::new(),
            files: Vec::new(),
            queue: Mutex::new(VecDeque::new()),
            claimed: Mutex::new(HashSet::new()),
            skip_files: HashSet::new(),
            retries: 0,
            connect_timeout: Duration::from_secs(1),
            limiter: None,
            events: tx,
            progress: Arc::new(Progress::new()),
            abort: CancellationToken::new(),
            fatal: Mutex::new(None),
            bytes_written: AtomicU64::new(0),
            failovers: AtomicUsize::new(0),
        };
        assert_eq!(shared.order(&[false, false, false], 0), vec![0, 1, 2]);
        assert_eq!(shared.order(&[false, false, false], 2), vec![2, 0, 1]);
        assert_eq!(shared.order(&[true, false, false], 0), vec![1, 2]);
        shared.servers[1].unusable.store(true, Ordering::SeqCst);
        assert_eq!(shared.order(&[true, false, false], 0), vec![2]);
    }
}
