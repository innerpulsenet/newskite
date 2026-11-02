//! The queue runner: picks jobs, drives the engine, persists progress and outcomes.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use newskite_engine::{download_with_pool, DownloadRequest, EngineError, EngineOptions, JobReport};
use newskite_postproc::{detect_tools, PostEvent, PostOptions, PostReport, PostStage, Verdict};
use newskite_store::{Job, JobFile, JobState, StoreError};
use newskite_types::{format, EngineEvent, FailureKind, JobOutcome, ServerConfig, Settings, StepRecord, StepState};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{lock, shutdown, Inner, LogLevel, PostProgress, ServiceEvent, NO_SERVER_MESSAGE};

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
    Job(Box<Job>, CancellationToken),
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
                        Pick::Job(Box::new(job), token)
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
                let outcome = process_job(inner, *job, servers, &settings, token).await;
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
    let (id, name) = (job.id, job.name.clone());
    let backup = lock(&inner.backup_first).get(&id).copied();
    // A backup retry runs on the fail-over servers' own connections, so no worker spills
    // onto the primary while it has room there: the primary only gets what they lack.
    let (servers, connections) = match settings.backup_first_servers() {
        Some(order) if backup.is_some() => {
            let backups: usize = order[..order.len() - 1].iter().map(|s| s.connections.max(1) as usize).sum();
            (order, settings.effective_connections().min(backups))
        }
        _ => (servers, settings.effective_connections()),
    };
    if backup.is_some() {
        let first = servers.first().map(ServerConfig::address).unwrap_or_default();
        inner.log_job(id, LogLevel::Info, format!("{name}: backup retry, asking {first} first"));
    }
    let end = download_job(inner, job, servers, connections, settings, token, &mut interrupted).await;
    if let (Some(before), false) = (backup, interrupted) {
        lock(&inner.backup_first).remove(&id);
        report_backup_retry(inner, id, &name, before);
    }
    Outcome { end, interrupted }
}

/// One log line on how a backup retry of job `id` ended, read back from the store so it
/// states what was recorded; `before` is the count of articles it was missing.
fn report_backup_retry(inner: &Inner, id: i64, name: &str, before: u64) {
    match inner.store.get_job(id) {
        Ok(Some(job)) if job.state == JobState::Done => {
            inner.log_job(id, LogLevel::Info, format!("{name}: backup retry succeeded"));
        }
        Ok(Some(job)) if job.state == JobState::Failed => {
            let after = job.stats.articles_missing;
            let s = if after == 1 { "" } else { "s" };
            let change = if before > 0 && after > 0 && after < before {
                format!("missing articles down from {before} to {after}")
            } else if before > 0 && after >= before {
                format!("still {after} missing article{s}; no server delivered them")
            } else {
                job.error.unwrap_or_else(|| "no reason recorded".into())
            };
            inner.log_job(id, LogLevel::Warn, format!("{name}: backup retry failed: {change}"));
        }
        // Removed, requeued by a shutdown, or unreadable: nothing final to report.
        _ => {}
    }
}

/// [`process_job`]'s body, fetching with at most `connections` at once. Returns `Some(end)` when the run must stop; sets `interrupted`
/// when the user stopped the job.
async fn download_job(
    inner: &Arc<Inner>,
    job: Job,
    servers: Vec<ServerConfig>,
    connections: usize,
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
            inner.log_job(job.id, LogLevel::Error, msg);
            return Some(End::Stopped);
        }
    }
    inner.emit(ServiceEvent::JobsChanged);
    inner.log_job(job.id, LogLevel::Info, format!("started {}", job.name));

    // Parse the NZB again (it is the durable source of the job).
    let path = job.source_path.clone();
    let nzb = match tokio::task::spawn_blocking(move || newskite_nzb::nzb::parse_file(&path)).await {
        Ok(Ok(nzb)) => nzb,
        Ok(Err(e)) => {
            fail_job(inner, &job, &format!("could not read NZB {}: {e}", job.source_path.display()), Finish::failed(FailureKind::Nzb));
            return None;
        }
        Err(e) => {
            fail_job(inner, &job, &format!("could not read NZB: {e}"), Finish::failed(FailureKind::Nzb));
            return None;
        }
    };

    // Resume: files completed in an earlier run are skipped.
    let files = inner.store.job_files(job.id).unwrap_or_else(|e| {
        inner.log_job(job.id, LogLevel::Warn, format!("could not read file progress of {}: {e}", job.name));
        Vec::new()
    });
    let done_files: Vec<JobFile> = files.into_iter().filter(|f| f.state == "done").collect();
    let dest = job.dest_dir.clone();
    let (done_files, notes) = match tokio::task::spawn_blocking(move || verify_done_files(&dest, done_files)).await {
        Ok(checked) => checked,
        Err(e) => {
            inner.log_job(job.id, LogLevel::Warn, format!("could not check the completed files of {}: {e}", job.name));
            (Vec::new(), Vec::new())
        }
    };
    for note in notes {
        inner.log_job(job.id, LogLevel::Warn, note);
    }
    let base_bytes: u64 = done_files.iter().map(|f| f.bytes_done).sum();
    let skip_files: HashSet<String> = done_files.iter().map(|f| f.name.clone()).collect();
    if !skip_files.is_empty() {
        inner.log_job(job.id, 
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
        connections,
        retries: settings.retries,
        speed_limit_bps: settings.speed_limit_bps(),
        connect_timeout: crate::CONNECT_TIMEOUT,
        skip_files,
    };
    let articles_total: u64 = nzb.files.iter().map(|f| f.segments.len() as u64).sum();
    let _ = inner.store.mark_job_started(job.id, crate::now_secs(), articles_total);
    let req = DownloadRequest { nzb, dest_dir: job.dest_dir.clone(), servers, options };

    let (tx, rx) = mpsc::unbounded_channel();
    let mut forwarder = inner.rt.spawn(forward_events(inner.clone(), job.id, job.bytes_total, base_bytes, rx));
    let started = Instant::now();
    let result = download_with_pool(req, inner.pool.clone(), tx, inner.progress.clone(), token).await;
    let _ = inner.store.add_job_download_ms(job.id, started.elapsed().as_millis() as u64);
    match tokio::time::timeout(FORWARDER_DRAIN, &mut forwarder).await {
        Ok(Ok((fetched, missing))) => {
            let _ = inner.store.add_article_counts(fetched, missing);
        }
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
        inner.log_job(job.id, LogLevel::Info, format!("{what} {}; completed files are kept", job.name));
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
            inner.log_job(job.id, LogLevel::Info, format!("interrupted {}; it will resume on the next start", job.name));
            Some(End::Shutdown)
        }
        Err(EngineError::MissingArticles { ids }) => {
            let p = &settings.postprocess;
            let dir = job.dest_dir.clone();
            let par2 = p.enabled && p.repair && tokio::task::spawn_blocking(move || newskite_postproc::has_par2(&dir)).await.unwrap_or(false);
            if par2 {
                // PAR2 can often rebuild what the servers no longer have.
                inner.log_job(job.id, LogLevel::Warn, format!("{}: {}; trying a PAR2 repair", job.name, missing_message(&ids)));
                finish_partial(inner, &job, settings, &ids).await;
            } else {
                let fin = Finish::failed(FailureKind::MissingArticles).missing(ids.len());
                fail_job(inner, &job, &missing_message(&ids), fin);
            }
            None
        }
        Err(EngineError::AuthFailed(m)) => {
            fail_job(inner, &job, &format!("authentication failed: {m}"), Finish::failed(FailureKind::Auth));
            Some(End::Stopped)
        }
        Err(EngineError::NoServer) => {
            let _ = inner.store.set_job_state(job.id, JobState::Queued, None);
            inner.emit(ServiceEvent::JobsChanged);
            Some(End::NoServer)
        }
        Err(EngineError::Connect(m)) => {
            fail_job(inner, &job, &format!("connection failed: {m}"), Finish::failed(FailureKind::Connection));
            Some(End::Stopped)
        }
        Err(EngineError::Io(e)) => {
            fail_job(inner, &job, &format!("I/O error: {e}"), Finish::failed(FailureKind::Disk));
            None
        }
    }
}

/// How a job ends: its outcome, post-processing time and missing-article count.
#[derive(Default)]
struct Finish {
    outcome: JobOutcome,
    post_ms: u64,
    missing: u64,
}

impl Finish {
    fn failed(kind: FailureKind) -> Self {
        Self { outcome: JobOutcome { failure: Some(kind), ..Default::default() }, ..Default::default() }
    }

    fn missing(mut self, n: usize) -> Self {
        self.missing = n as u64;
        self
    }
}

fn step_state(v: &Verdict) -> StepState {
    match v {
        Verdict::Ok => StepState::Ok,
        Verdict::Repaired => StepState::Repaired,
        Verdict::Skipped(_) => StepState::Skipped,
        Verdict::Failed(_) => StepState::Failed,
    }
}

/// The outcome of a post-processing run, from its report and the time spent per stage.
fn outcome_of(post: &PostReport, stage_ms: &HashMap<PostStage, u64>) -> JobOutcome {
    let ms = |s: PostStage| stage_ms.get(&s).copied().unwrap_or(0);
    let mut o = JobOutcome::default();
    let stats = post.par2_stats.as_ref();
    let par2_state = step_state(&post.par2);
    let repaired = post.par2 == Verdict::Repaired;
    let repair_ran = stats.is_some_and(|s| s.slices_missing > 0 || s.repair_ms > 0) || repaired;
    o.verify = Some(StepRecord {
        state: match par2_state {
            StepState::Repaired => StepState::Ok,
            StepState::Failed if repair_ran => StepState::Ok,
            s => s,
        },
        detail: match (&post.par2, stats) {
            (Verdict::Skipped(r), _) => format!("skipped ({r})"),
            (_, Some(s)) => s.summary(),
            (v, None) => v.to_string(),
        },
        ms: stats.map_or(ms(PostStage::Verify), |s| s.verify_ms),
    });
    if repair_ran {
        o.repair = Some(StepRecord {
            state: if repaired { StepState::Repaired } else { par2_state },
            detail: match (&post.par2, stats) {
                (Verdict::Failed(r), _) => r.clone(),
                (_, Some(s)) => format!("rebuilt {} slice{}", s.slices_rebuilt, if s.slices_rebuilt == 1 { "" } else { "s" }),
                (v, None) => v.to_string(),
            },
            ms: stats.map_or(ms(PostStage::Repair), |s| s.repair_ms),
        });
    }
    o.extract = Some(StepRecord { state: step_state(&post.extract), detail: post.extract.to_string(), ms: ms(PostStage::Extract) });
    let removed = post.removed.len() + post.partials_removed.len();
    o.cleanup = Some(StepRecord {
        state: if removed > 0 { StepState::Ok } else { StepState::Skipped },
        detail: if removed > 0 {
            format!("removed {removed} file{}", if removed == 1 { "" } else { "s" })
        } else {
            "nothing to remove".into()
        },
        ms: ms(PostStage::Cleanup),
    });
    o.par2_summary = stats.map(|s| s.summary());
    o.slices_rebuilt = stats.map_or(0, |s| s.slices_rebuilt as u64);
    o.extracted_to = post.extracted_to.clone();
    o.files_removed = removed as u64;
    if post.par2.is_failed() {
        o.failure = Some(FailureKind::Repair);
    } else if let Verdict::Failed(r) = &post.extract {
        o.failure = Some(if r.contains("password") { FailureKind::Password } else { FailureKind::Extract });
    }
    o
}

/// Runs post-processing for `job`, streaming its progress into [`crate::Status::post`]
/// and [`ServiceEvent::PostProgress`]. `Err` carries the failure reason; both carry what
/// is known about the outcome.
async fn post_process(inner: &Arc<Inner>, job: &Job, settings: &Settings) -> (Result<PostReport, String>, Finish) {
    inner.log_job(job.id, LogLevel::Info, format!("post-processing {}", job.name));
    let opts = PostOptions::from_settings(&settings.postprocess, job.password.clone());
    let dir = job.dest_dir.clone();
    let name = job.name.clone();
    let job_id = job.id;
    let task_inner = inner.clone();
    let started = Instant::now();
    let result = tokio::task::spawn_blocking(move || {
        let tools = task_inner.tools.get_or_init(detect_tools);
        let ev = task_inner.clone();
        // Time per stage, from the stage changes.
        let mut stage_ms: HashMap<PostStage, u64> = HashMap::new();
        let mut current: Option<(PostStage, Instant)> = None;
        let report = newskite_postproc::process_with_events(&dir, &name, &opts, tools, &mut |e| match e {
            PostEvent::Log(line) => {
                let level = if line.contains("could not") || line.contains("still incomplete") {
                    LogLevel::Warn
                } else {
                    LogLevel::Info
                };
                ev.log_job(job_id, level, line)
            }
            PostEvent::Stage { stage, fraction, detail } => {
                if current.map(|(s, _)| s) != Some(stage) {
                    if let Some((s, t)) = current.take() {
                        *stage_ms.entry(s).or_default() += t.elapsed().as_millis() as u64;
                    }
                    current = Some((stage, Instant::now()));
                }
                let p = PostProgress {
                    job_id,
                    stage,
                    permille: fraction.map(|f| (f.clamp(0.0, 1.0) * 1000.0) as u16),
                    detail,
                };
                ev.lock_state().post = Some(p.clone());
                ev.emit(ServiceEvent::PostProgress(p));
            }
        });
        if let Some((s, t)) = current {
            *stage_ms.entry(s).or_default() += t.elapsed().as_millis() as u64;
        }
        (report, stage_ms)
    })
    .await;
    inner.lock_state().post = None;
    let post_ms = started.elapsed().as_millis() as u64;
    let secs = post_ms / 1000;
    match result {
        Ok((Ok(post), stage_ms)) => {
            let fin = Finish { outcome: outcome_of(&post, &stage_ms), post_ms, missing: 0 };
            inner.log_job(
                job.id,
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
                return (Err(format!("post-processing failed: par2 {}, extract {}", post.par2, post.extract)), fin);
            }
            (Ok(post), fin)
        }
        Ok((Err(e), _)) => (Err(format!("post-processing error: {e}")), Finish { post_ms, ..Finish::failed(FailureKind::Disk) }),
        Err(e) => (Err(format!("post-processing stopped unexpectedly: {e}")), Finish { post_ms, ..Finish::failed(FailureKind::Other) }),
    }
}

/// Post-processing (when enabled) and the final `done`/`failed` state of a downloaded job.
async fn finish_ok(inner: &Arc<Inner>, job: &Job, settings: &Settings, report: &JobReport) {
    let mut fin = Finish::default();
    if settings.postprocess.enabled {
        let (result, f) = post_process(inner, job, settings).await;
        if let Err(reason) = result {
            fail_job(inner, job, &reason, f);
            return;
        }
        fin = f;
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
        fin,
    );
}

/// A download with missing articles and PAR2 data: post-process it anyway. Done when PAR2
/// rebuilt every incomplete file, else failed with both reasons.
async fn finish_partial(inner: &Arc<Inner>, job: &Job, settings: &Settings, ids: &[String]) {
    let missing = missing_message(ids);
    let (result, mut fin) = post_process(inner, job, settings).await;
    fin.missing = ids.len() as u64;
    match result {
        Ok(post) if post.incomplete.is_empty() => {
            if inner.lock_state().removed_active {
                return;
            }
            fin.outcome.articles_repaired = ids.len() as u64;
            mark_done(
                inner,
                job,
                format!(
                    "completed {}: PAR2 repaired {} missing article{}",
                    job.name,
                    ids.len(),
                    if ids.len() == 1 { "" } else { "s" }
                ),
                fin,
            );
        }
        Ok(post) => {
            fin.outcome.failure = Some(FailureKind::MissingArticles);
            let reason = format!(
                "{missing}; {} file(s) still incomplete after PAR2 (not covered by the recovery data)",
                post.incomplete.len()
            );
            fail_job(inner, job, &reason, fin)
        }
        Err(reason) => {
            fin.outcome.failure.get_or_insert(FailureKind::MissingArticles);
            fail_job(inner, job, &format!("{missing}; {reason}"), fin)
        }
    }
}

/// Records the finish in the store and tells subscribers.
fn record_finish(inner: &Inner, job: &Job, ok: bool, detail: String, fin: Finish) {
    if let Err(e) = inner.store.set_job_outcome(job.id, crate::now_secs(), fin.post_ms, fin.missing, &fin.outcome) {
        if !matches!(e, StoreError::NotFound) {
            inner.log_job(job.id, LogLevel::Warn, format!("could not record the outcome of {}: {e}", job.name));
        }
    }
    inner.emit(ServiceEvent::JobFinished { job_id: job.id, name: job.name.clone(), ok, detail });
}

fn mark_done(inner: &Inner, job: &Job, line: String, fin: Finish) {
    let _ = inner.store.set_job_progress(job.id, job.bytes_total);
    match inner.store.set_job_state(job.id, JobState::Done, None) {
        Ok(()) | Err(StoreError::NotFound) => {}
        Err(e) => inner.log_job(job.id, LogLevel::Error, format!("could not mark {} done: {e}", job.name)),
    }
    inner.log_job(job.id, LogLevel::Info, line.clone());
    let detail = match (&fin.outcome.extracted_to, fin.outcome.repaired()) {
        (_, true) => "repaired and complete".to_string(),
        (Some(_), false) => "extracted".to_string(),
        _ => "complete".to_string(),
    };
    record_finish(inner, job, true, detail, fin);
    inner.emit(ServiceEvent::JobsChanged);
}

/// Marks a job failed with `reason`, logs it and records it as the last error.
fn fail_job(inner: &Inner, job: &Job, reason: &str, fin: Finish) {
    match inner.store.set_job_state(job.id, JobState::Failed, Some(reason)) {
        Ok(()) | Err(StoreError::NotFound) => {}
        Err(e) => inner.log_job(job.id, LogLevel::Error, format!("could not mark {} failed: {e}", job.name)),
    }
    let msg = format!("failed {}: {reason}", job.name);
    inner.set_last_error(msg.clone());
    inner.log_job(job.id, LogLevel::Error, msg);
    record_finish(inner, job, false, reason.to_string(), fin);
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
) -> (u64, u64) {
    let (mut fetched, mut missing_n) = (0u64, 0u64);
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
                    inner.log_job(job_id, LogLevel::Warn, format!("could not save progress of job {job_id}: {e}"));
                }
            }
        }
    };

    while let Some(ev) = rx.recv().await {
        match &ev {
            EngineEvent::FileStarted { name, segments } => {
                upsert_file(&inner, job_id, name, 0, 0, "downloading");
                inner.log_job(job_id, 
                    LogLevel::Info,
                    format!("downloading {name} ({segments} segment{})", if *segments == 1 { "" } else { "s" }),
                );
            }
            EngineEvent::SegmentDone { name, bytes, .. } => {
                fetched += 1;
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
                inner.log_job(job_id, LogLevel::Info, format!("wrote {name} ({})", format::bytes(*bytes)));
            }
            EngineEvent::FileFailed { name, missing } => {
                missing_n += missing.len() as u64;
                let got = per_file.get(name).copied().unwrap_or(0);
                upsert_file(&inner, job_id, name, 0, got, "failed");
                persist(run_bytes, &mut persist_error_logged);
                last_persist = Instant::now();
                inner.log_job(job_id, 
                    LogLevel::Warn,
                    format!(
                        "{name}: {} missing article{}",
                        missing.len(),
                        if missing.len() == 1 { "" } else { "s" }
                    ),
                );
            }
            EngineEvent::Log(line) => inner.log_job(job_id, LogLevel::Info, line.clone()),
            EngineEvent::Warn(line) => inner.log_job(job_id, LogLevel::Warn, line.clone()),
        }
        inner.emit(ServiceEvent::Engine { job_id, event: ev });
    }
    persist(run_bytes, &mut persist_error_logged);
    (fetched, missing_n)
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
