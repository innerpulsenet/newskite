//! The queue runner: picks jobs, drives the engine, persists progress and outcomes.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use newskite_engine::{download_with_pool, DownloadRequest, EngineError, EngineOptions, JobReport};
use newskite_postproc::{detect_tools, PostEvent, PostOptions, PostReport};
use newskite_store::{Job, JobFile, JobState, StoreError};
use newskite_types::{format, EngineEvent, ServerConfig, Settings};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{shutdown, Inner, LogLevel, PostProgress, ServiceEvent, NO_SERVER_MESSAGE};

/// How long to wait for the event forwarder to drain after the engine returned.
const FORWARDER_DRAIN: Duration = Duration::from_secs(10);

/// Missing message ids listed in a job's error text.
const MAX_LISTED_IDS: usize = 5;

/// Why a run ended.
#[derive(Debug)]
enum End {
    /// Nothing runnable is left.
    Drained,
    /// Pause requested.
    Paused,
    /// [`crate::Service::shutdown`] was called.
    Shutdown,
    /// No usable server: the job stays queued.
    NoServer,
    /// A fatal error was already recorded and logged.
    Stopped,
    /// The queue could not be read; message to log.
    Store(String),
}

/// Starts a run unless one is active. See [`crate::Service::start`].
pub(crate) fn start(inner: &Arc<Inner>) -> bool {
    start_with(inner, None)
}

/// Starts a run unless one is active; with `only`, the run processes just those job ids.
/// An active run limited to some jobs is widened instead: to the whole queue (`None`) or
/// by the listed ids. See [`crate::Service::start_jobs`].
pub(crate) fn start_with(inner: &Arc<Inner>, only: Option<Vec<i64>>) -> bool {
    if inner.shutdown.is_cancelled() {
        inner.log(LogLevel::Warn, "the queue cannot start: the service is stopping");
        return false;
    }
    let (was_paused, countdown) = {
        let mut st = inner.lock_state();
        if st.running {
            if let Some(scope) = st.only.as_mut() {
                match only {
                    None => st.only = None,
                    Some(ids) => {
                        for id in ids {
                            if !scope.contains(&id) {
                                scope.push(id);
                            }
                        }
                    }
                }
            }
            return false;
        }
        st.running = true;
        st.only = only;
        st.last_error = None;
        st.removed_active = false;
        inner.running_tx.send_replace(true);
        (std::mem::take(&mut st.paused), st.countdown.take())
    };
    if let Some(token) = countdown {
        token.cancel();
        inner.emit(ServiceEvent::ShutdownCanceled);
        inner.log(LogLevel::Info, "auto-shutdown canceled: the queue was started again");
    }
    if was_paused {
        inner.emit(ServiceEvent::Resumed);
    }
    let task_inner = inner.clone();
    inner.rt.spawn(async move {
        let mut guard = RunGuard { inner: task_inner.clone(), armed: true };
        run_queue(&task_inner, &mut guard).await;
    });
    true
}

/// Resets the running flag if the run task ends abnormally (panic), so the queue can be
/// started again and `wait_idle` resolves.
struct RunGuard {
    inner: Arc<Inner>,
    armed: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut st = self.inner.lock_state();
            st.running = false;
            st.active_job = None;
            st.job_cancel = None;
            st.stop_active = None;
            st.only = None;
            st.last_error = Some("the queue run stopped unexpectedly".into());
        }
        self.inner.log(LogLevel::Error, "the queue run stopped unexpectedly");
        crate::timers::flush_throughput(&self.inner);
        self.inner.emit(ServiceEvent::QueueIdle);
        self.inner.running_tx.send_replace(false);
    }
}

enum Pick {
    Job(Job, CancellationToken),
    End(End),
}

/// Runs the queue until nothing runnable is left. While the run is limited
/// (`RunState::only`), just the jobs whose id is listed are considered; every other queued
/// job is left alone. The scope is re-read for every pick, so it can widen mid-run.
async fn run_queue(inner: &Arc<Inner>, guard: &mut RunGuard) {
    let mut processed = 0usize;
    let mut attempted: HashSet<i64> = HashSet::new();
    let mut requeued: HashSet<i64> = HashSet::new();
    let mut forced_end: Option<End> = None;
    let mut last_settings;

    let end = loop {
        let settings = inner.settings();
        let servers = settings.usable_servers();
        last_settings = settings.clone();

        // Pick the next job, or end the run, atomically with respect to start()/remove_job().
        let pick = {
            let mut st = inner.lock_state();
            let pick = if let Some(end) = forced_end.take() {
                Pick::End(end)
            } else if inner.shutdown.is_cancelled() {
                Pick::End(End::Shutdown)
            } else if st.paused {
                Pick::End(End::Paused)
            } else {
                let next = match st.only.as_deref() {
                    Some(ids) => inner.store.next_runnable_job_among(ids),
                    None => inner.store.next_runnable_job(),
                };
                match next {
                    Err(e) => Pick::End(End::Store(format!("could not read the queue: {e}"))),
                    Ok(None) => Pick::End(End::Drained),
                    // A job interrupted and started again in this run (pause, then start)
                    // may be picked again; anything else coming back means it is stuck.
                    Ok(Some(job)) if attempted.contains(&job.id) && !requeued.contains(&job.id) => {
                        Pick::End(End::Store(format!("job {} could not leave the queue; stopping", job.name)))
                    }
                    Ok(Some(_)) if servers.is_empty() => Pick::End(End::NoServer),
                    Ok(Some(job)) => {
                        let token = inner.shutdown.child_token();
                        st.active_job = Some(job.id);
                        st.job_cancel = Some(token.clone());
                        st.removed_active = false;
                        st.stop_active = None;
                        Pick::Job(job, token)
                    }
                }
            };
            if let Pick::End(end) = &pick {
                finish_run_locked(inner, &mut st, end, processed);
                guard.armed = false;
            }
            pick
        };

        match pick {
            Pick::End(end) => break end,
            Pick::Job(job, token) => {
                let id = job.id;
                requeued.remove(&id);
                attempted.insert(id);
                let outcome = process_job(inner, job, servers, &settings, token).await;
                if outcome.interrupted {
                    requeued.insert(id);
                } else {
                    processed += 1;
                }
                forced_end = outcome.end;
                let mut st = inner.lock_state();
                st.active_job = None;
                st.job_cancel = None;
                st.removed_active = false;
                st.stop_active = None;
            }
        }
    };

    if matches!(end, End::Drained) && processed > 0 {
        shutdown::maybe_start_countdown(inner, &last_settings);
    }
}

/// Ends the run while holding the state lock, so a concurrent `start()` either sees the
/// run as active (and its job gets picked) or starts a new one after us.
fn finish_run_locked(inner: &Inner, st: &mut crate::RunState, end: &End, processed: usize) {
    match end {
        End::Drained => {
            if processed > 0 {
                inner.log(LogLevel::Info, "queue finished");
            }
        }
        End::Paused => inner.log(LogLevel::Info, "queue paused"),
        End::Shutdown | End::Stopped => {}
        End::NoServer => {
            st.last_error = Some(NO_SERVER_MESSAGE.into());
            inner.log(LogLevel::Error, NO_SERVER_MESSAGE);
        }
        End::Store(msg) => {
            st.last_error = Some(msg.clone());
            inner.log(LogLevel::Error, msg.clone());
        }
    }
    st.running = false;
    st.active_job = None;
    st.job_cancel = None;
    st.removed_active = false;
    st.stop_active = None;
    st.only = None;
    // The queue is idle: give the server its connection slots back.
    inner.pool.close_idle();
    // Before the idle signal, so whoever waits for it sees this run's bytes in the totals.
    crate::timers::flush_throughput(inner);
    inner.emit(ServiceEvent::QueueIdle);
    inner.running_tx.send_replace(false);
}

/// How one job's turn in a run ended.
struct Outcome {
    /// `Some(end)` when the run must stop.
    end: Option<End>,
    /// The user interrupted the job (held it or paused the queue); it was not finished.
    interrupted: bool,
}

/// Downloads (and post-processes) one job.
async fn process_job(
    inner: &Arc<Inner>,
    job: Job,
    servers: Vec<ServerConfig>,
    settings: &Settings,
    token: CancellationToken,
) -> Outcome {
    let mut interrupted = false;
    let end = download_job(inner, job, servers, settings, token, &mut interrupted).await;
    Outcome { end, interrupted }
}

/// [`process_job`]'s body. Returns `Some(end)` when the run must stop; sets `interrupted`
/// when the user stopped the job.
async fn download_job(
    inner: &Arc<Inner>,
    job: Job,
    servers: Vec<ServerConfig>,
    settings: &Settings,
    token: CancellationToken,
    interrupted: &mut bool,
) -> Option<End> {
    match inner.store.set_job_state(job.id, JobState::Downloading, None) {
        Ok(()) => {}
        Err(StoreError::NotFound) => return None, // removed meanwhile
        Err(e) => {
            let msg = format!("could not start {}: {e}", job.name);
            inner.set_last_error(msg.clone());
            inner.log(LogLevel::Error, msg);
            return Some(End::Stopped);
        }
    }
    inner.emit(ServiceEvent::JobsChanged);
    inner.log(LogLevel::Info, format!("started {}", job.name));

    // Parse the NZB again (it is the durable source of the job).
    let path = job.source_path.clone();
    let nzb = match tokio::task::spawn_blocking(move || newskite_nzb::nzb::parse_file(&path)).await {
        Ok(Ok(nzb)) => nzb,
        Ok(Err(e)) => {
            fail_job(inner, &job, &format!("could not read NZB {}: {e}", job.source_path.display()));
            return None;
        }
        Err(e) => {
            fail_job(inner, &job, &format!("could not read NZB: {e}"));
            return None;
        }
    };

    // Resume: files completed in an earlier run are skipped.
    let files = inner.store.job_files(job.id).unwrap_or_else(|e| {
        inner.log(LogLevel::Warn, format!("could not read file progress of {}: {e}", job.name));
        Vec::new()
    });
    let done_files: Vec<JobFile> = files.into_iter().filter(|f| f.state == "done").collect();
    let dest = job.dest_dir.clone();
    let (done_files, notes) = match tokio::task::spawn_blocking(move || verify_done_files(&dest, done_files)).await {
        Ok(checked) => checked,
        Err(e) => {
            inner.log(LogLevel::Warn, format!("could not check the completed files of {}: {e}", job.name));
            (Vec::new(), Vec::new())
        }
    };
    for note in notes {
        inner.log(LogLevel::Warn, note);
    }
    let base_bytes: u64 = done_files.iter().map(|f| f.bytes_done).sum();
    let skip_files: HashSet<String> = done_files.iter().map(|f| f.name.clone()).collect();
    if !skip_files.is_empty() {
        inner.log(
            LogLevel::Info,
            format!(
                "resuming {}: {} file{} already complete",
                job.name,
                skip_files.len(),
                if skip_files.len() == 1 { "" } else { "s" }
            ),
        );
    }

    let options = EngineOptions {
        connections: settings.effective_connections(),
        retries: settings.retries,
        speed_limit_bps: settings.speed_limit_bps(),
        connect_timeout: crate::CONNECT_TIMEOUT,
        skip_files,
    };
    let req = DownloadRequest { nzb, dest_dir: job.dest_dir.clone(), servers, options };

    let (tx, rx) = mpsc::unbounded_channel();
    let mut forwarder = inner.rt.spawn(forward_events(inner.clone(), job.id, job.bytes_total, base_bytes, rx));
    let result = download_with_pool(req, inner.pool.clone(), tx, inner.progress.clone(), token).await;
    match tokio::time::timeout(FORWARDER_DRAIN, &mut forwarder).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(target: "newskite_service", "event forwarder failed: {e}"),
        Err(_) => {
            forwarder.abort();
            tracing::warn!(target: "newskite_service", "event forwarder did not drain; aborted");
        }
    }

    let stop = {
        let mut st = inner.lock_state();
        if st.removed_active {
            return None;
        }
        st.stop_active.take()
    };
    // Interrupted by the user (hold or pause): the download stopped, whatever error it
    // reported for that. A download that finished anyway goes on to post-processing.
    if let (Some(state), Err(_)) = (stop, &result) {
        *interrupted = true;
        let _ = inner.store.set_job_state(job.id, state, None);
        inner.emit(ServiceEvent::JobsChanged);
        let what = if state == JobState::Paused { "paused" } else { "interrupted" };
        inner.log(LogLevel::Info, format!("{what} {}; completed files are kept", job.name));
        return None;
    }

    match result {
        Ok(report) => {
            finish_ok(inner, &job, settings, &report).await;
            None
        }
        Err(EngineError::Cancelled) => {
            // Only shutdown (or removal, handled above) cancels a job: leave it resumable.
            let _ = inner.store.set_job_state(job.id, JobState::Queued, None);
            inner.emit(ServiceEvent::JobsChanged);
            inner.log(LogLevel::Info, format!("interrupted {}; it will resume on the next start", job.name));
            Some(End::Shutdown)
        }
        Err(EngineError::MissingArticles { ids }) => {
            let p = &settings.postprocess;
            let dir = job.dest_dir.clone();
            let par2 = p.enabled && p.repair && tokio::task::spawn_blocking(move || newskite_postproc::has_par2(&dir)).await.unwrap_or(false);
            if par2 {
                // PAR2 can often rebuild what the servers no longer have.
                inner.log(LogLevel::Warn, format!("{}: {}; trying a PAR2 repair", job.name, missing_message(&ids)));
                finish_partial(inner, &job, settings, &ids).await;
            } else {
                fail_job(inner, &job, &missing_message(&ids));
            }
            None
        }
        Err(EngineError::AuthFailed(m)) => {
            fail_job(inner, &job, &format!("authentication failed: {m}"));
            Some(End::Stopped)
        }
        Err(EngineError::NoServer) => {
            let _ = inner.store.set_job_state(job.id, JobState::Queued, None);
            inner.emit(ServiceEvent::JobsChanged);
            Some(End::NoServer)
        }
        Err(EngineError::Connect(m)) => {
            fail_job(inner, &job, &format!("connection failed: {m}"));
            Some(End::Stopped)
        }
        Err(EngineError::Io(e)) => {
            fail_job(inner, &job, &format!("I/O error: {e}"));
            None
        }
    }
}

/// Runs post-processing for `job`, streaming its progress into [`crate::Status::post`]
/// and [`ServiceEvent::PostProgress`]. `Err` carries the failure reason.
async fn post_process(inner: &Arc<Inner>, job: &Job, settings: &Settings) -> Result<PostReport, String> {
    inner.log(LogLevel::Info, format!("post-processing {}", job.name));
    let opts = PostOptions::from_settings(&settings.postprocess, job.password.clone());
    let dir = job.dest_dir.clone();
    let name = job.name.clone();
    let job_id = job.id;
    let task_inner = inner.clone();
    let started = Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        let tools = task_inner.tools.get_or_init(detect_tools);
        let ev = task_inner.clone();
        newskite_postproc::process_with_events(&dir, &name, &opts, tools, &mut |e| match e {
            PostEvent::Log(line) => ev.log(LogLevel::Info, line),
            PostEvent::Stage { stage, fraction, detail } => {
                let p = PostProgress {
                    job_id,
                    stage,
                    permille: fraction.map(|f| (f.clamp(0.0, 1.0) * 1000.0) as u16),
                    detail,
                };
                ev.lock_state().post = Some(p.clone());
                ev.emit(ServiceEvent::PostProgress(p));
            }
        })
    })
    .await;
    inner.lock_state().post = None;
    let secs = started.elapsed().as_secs();
    match result {
        Ok(Ok(post)) => {
            inner.log(
                LogLevel::Info,
                format!(
                    "post-processing {}: par2 {}, extract {} ({})",
                    job.name,
                    post.par2,
                    post.extract,
                    format::duration_secs(secs)
                ),
            );
            if post.par2.is_failed() || post.extract.is_failed() {
                return Err(format!("post-processing failed: par2 {}, extract {}", post.par2, post.extract));
            }
            Ok(post)
        }
        Ok(Err(e)) => Err(format!("post-processing error: {e}")),
        Err(e) => Err(format!("post-processing stopped unexpectedly: {e}")),
    }
}

/// Post-processing (when enabled) and the final `done`/`failed` state of a downloaded job.
async fn finish_ok(inner: &Arc<Inner>, job: &Job, settings: &Settings, report: &JobReport) {
    if settings.postprocess.enabled {
        if let Err(reason) = post_process(inner, job, settings).await {
            fail_job(inner, job, &reason);
            return;
        }
        if inner.lock_state().removed_active {
            return;
        }
    }
    let written: u64 = report.files.iter().map(|f| f.bytes).sum();
    mark_done(
        inner,
        job,
        format!(
            "completed {} ({} file{}, {})",
            job.name,
            report.files.len(),
            if report.files.len() == 1 { "" } else { "s" },
            format::bytes(written)
        ),
    );
}

/// A download with missing articles and PAR2 data: post-process it anyway. Done when PAR2
/// rebuilt every incomplete file, else failed with both reasons.
async fn finish_partial(inner: &Arc<Inner>, job: &Job, settings: &Settings, ids: &[String]) {
    let missing = missing_message(ids);
    match post_process(inner, job, settings).await {
        Ok(post) if post.incomplete.is_empty() => {
            if inner.lock_state().removed_active {
                return;
            }
            mark_done(
                inner,
                job,
                format!(
                    "completed {}: PAR2 repaired {} missing article{}",
                    job.name,
                    ids.len(),
                    if ids.len() == 1 { "" } else { "s" }
                ),
            );
        }
        Ok(post) => fail_job(
            inner,
            job,
            &format!("{missing}; {} file(s) still incomplete after PAR2 (not covered by the recovery data)", post.incomplete.len()),
        ),
        Err(reason) => fail_job(inner, job, &format!("{missing}; {reason}")),
    }
}

fn mark_done(inner: &Inner, job: &Job, line: String) {
    let _ = inner.store.set_job_progress(job.id, job.bytes_total);
    match inner.store.set_job_state(job.id, JobState::Done, None) {
        Ok(()) | Err(StoreError::NotFound) => {}
        Err(e) => inner.log(LogLevel::Error, format!("could not mark {} done: {e}", job.name)),
    }
    inner.log(LogLevel::Info, line);
    inner.emit(ServiceEvent::JobsChanged);
}

/// Marks a job failed with `reason`, logs it and records it as the last error.
fn fail_job(inner: &Inner, job: &Job, reason: &str) {
    match inner.store.set_job_state(job.id, JobState::Failed, Some(reason)) {
        Ok(()) | Err(StoreError::NotFound) => {}
        Err(e) => inner.log(LogLevel::Error, format!("could not mark {} failed: {e}", job.name)),
    }
    let msg = format!("failed {}: {reason}", job.name);
    inner.set_last_error(msg.clone());
    inner.log(LogLevel::Error, msg);
    inner.emit(ServiceEvent::JobsChanged);
}

/// `"3 missing articles: a, b, c"`; at most [`MAX_LISTED_IDS`] ids, then `"... (+N more)"`.
pub(crate) fn missing_message(ids: &[String]) -> String {
    let n = ids.len();
    let listed: Vec<&str> = ids.iter().take(MAX_LISTED_IDS).map(String::as_str).collect();
    let mut s = format!("{n} missing article{}", if n == 1 { "" } else { "s" });
    if !listed.is_empty() {
        s.push_str(": ");
        s.push_str(&listed.join(", "));
    }
    if n > MAX_LISTED_IDS {
        s.push_str(&format!(", ... (+{} more)", n - MAX_LISTED_IDS));
    }
    s
}

/// Rebroadcasts engine events, keeps `job_files` and `bytes_done` current and turns
/// file-level events into log lines.
async fn forward_events(
    inner: Arc<Inner>,
    job_id: i64,
    bytes_total: u64,
    base_bytes: u64,
    mut rx: mpsc::UnboundedReceiver<EngineEvent>,
) {
    let mut run_bytes: u64 = 0;
    let mut per_file: HashMap<String, u64> = HashMap::new();
    let mut last_persist = Instant::now();
    let mut persist_error_logged = false;

    let persist = |run_bytes: u64, logged: &mut bool| {
        let done = base_bytes.saturating_add(run_bytes).min(bytes_total);
        match inner.store.set_job_progress(job_id, done) {
            Ok(()) | Err(StoreError::NotFound) => {}
            Err(e) => {
                if !std::mem::replace(logged, true) {
                    inner.log(LogLevel::Warn, format!("could not save progress of job {job_id}: {e}"));
                }
            }
        }
    };

    while let Some(ev) = rx.recv().await {
        match &ev {
            EngineEvent::FileStarted { name, segments } => {
                upsert_file(&inner, job_id, name, 0, 0, "downloading");
                inner.log(
                    LogLevel::Info,
                    format!("downloading {name} ({segments} segment{})", if *segments == 1 { "" } else { "s" }),
                );
            }
            EngineEvent::SegmentDone { name, bytes, .. } => {
                run_bytes = run_bytes.saturating_add(*bytes);
                *per_file.entry(name.clone()).or_default() += *bytes;
                if last_persist.elapsed() >= Duration::from_secs(1) {
                    persist(run_bytes, &mut persist_error_logged);
                    last_persist = Instant::now();
                }
            }
            EngineEvent::FileDone { name, bytes, .. } => {
                upsert_file(&inner, job_id, name, *bytes, *bytes, "done");
                persist(run_bytes, &mut persist_error_logged);
                last_persist = Instant::now();
                inner.log(LogLevel::Info, format!("wrote {name} ({})", format::bytes(*bytes)));
            }
            EngineEvent::FileFailed { name, missing } => {
                let got = per_file.get(name).copied().unwrap_or(0);
                upsert_file(&inner, job_id, name, 0, got, "failed");
                persist(run_bytes, &mut persist_error_logged);
                last_persist = Instant::now();
                inner.log(
                    LogLevel::Warn,
                    format!(
                        "{name}: {} missing article{}",
                        missing.len(),
                        if missing.len() == 1 { "" } else { "s" }
                    ),
                );
            }
            EngineEvent::Log(line) => inner.log(LogLevel::Info, line.clone()),
        }
        inner.emit(ServiceEvent::Engine { job_id, event: ev });
    }
    persist(run_bytes, &mut persist_error_logged);
}

/// Blocking: keeps the `done` rows whose file is still in `dest` with the recorded length;
/// returns them plus a log line for each dropped one. A file shorter than recorded is what
/// lost writes (power loss) leave behind; it is this job's own output, so it is removed and
/// downloaded again under its own name. A missing file or one of another length is only
/// dropped from the skip list (the engine then picks a free name rather than overwrite it).
fn verify_done_files(dest: &Path, done: Vec<JobFile>) -> (Vec<JobFile>, Vec<String>) {
    let mut keep = Vec::with_capacity(done.len());
    let mut notes = Vec::new();
    for f in done {
        let path = dest.join(&f.name);
        // Names come from the engine and are single components; anything else is not ours.
        if path.parent() != Some(dest) {
            notes.push(format!("{}: unexpected file name in the job record; downloading it again", f.name));
            continue;
        }
        let len = match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() => m.len(),
            Ok(_) | Err(_) => {
                notes.push(format!("{} is no longer in the download folder; downloading it again", f.name));
                continue;
            }
        };
        if len == f.bytes_total {
            keep.push(f);
            continue;
        }
        if len < f.bytes_total && std::fs::remove_file(&path).is_ok() {
            notes.push(format!(
                "{} is incomplete on disk ({} of {}); downloading it again",
                f.name,
                format::bytes(len),
                format::bytes(f.bytes_total)
            ));
        } else {
            notes.push(format!(
                "{} on disk is {}, not the recorded {}; downloading it again",
                f.name,
                format::bytes(len),
                format::bytes(f.bytes_total)
            ));
        }
    }
    (keep, notes)
}

fn upsert_file(inner: &Inner, job_id: i64, name: &str, bytes_total: u64, bytes_done: u64, state: &str) {
    let row = JobFile { job_id, name: name.to_string(), bytes_total, bytes_done, state: state.to_string() };
    match inner.store.upsert_job_file(&row) {
        Ok(()) | Err(StoreError::NotFound) => {}
        Err(e) => tracing::warn!(target: "newskite_service", "could not save file row {name}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::missing_message;

    #[test]
    fn missing_message_lists_at_most_five() {
        let ids: Vec<String> = (1..=7).map(|i| format!("id{i}@x")).collect();
        assert_eq!(
            missing_message(&ids),
            "7 missing articles: id1@x, id2@x, id3@x, id4@x, id5@x, ... (+2 more)"
        );
        assert_eq!(missing_message(&ids[..1]), "1 missing article: id1@x");
        assert_eq!(missing_message(&[]), "0 missing articles");
    }
}
