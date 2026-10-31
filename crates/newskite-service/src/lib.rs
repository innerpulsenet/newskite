//! Newskite queue service, shared by the GUI and the headless CLI.
//!
//! A [`Service`] owns the [`Store`] and drives the download queue on a tokio runtime:
//!
//! * **Import (F1).** [`Service::add_nzb`] parses the NZB first; a malformed or unreadable
//!   file produces [`ServiceError::Nzb`] and no job row. The job is persisted `queued` with
//!   its byte total, its canonical source path and its destination folder
//!   (`<download_dir>/<job name>`, made unique among pending jobs) before anything downloads.
//! * **Watch folder (F2).** [`Service::scan_watch_folder`] enqueues every `*.nzb` (case
//!   insensitive, not recursive) whose canonical path has never been imported, in any state.
//!   A background timer scans every `watch_interval_secs` while `watch_enabled` (settings are
//!   re-read every second, so changes apply without a restart). Manual scans work whenever a
//!   watch folder is configured. Timer scans log only imports and new problems.
//! * **Queue (F4).** One run at a time ([`Service::start`] is idempotent). A run processes
//!   `queued`/`paused` jobs oldest first. [`Service::pause`] is cooperative: the current job
//!   finishes and the run stops before the next one; the `Paused` event is emitted at once as
//!   the acknowledgment. [`Service::resume`] clears the pause and starts a run if none is
//!   active. [`Service::remove_job`] deletes the row and cancels the job if it is downloading
//!   (partial files are left on disk).
//! * **Per job.** Usable servers come from [`Settings::usable_servers`]; with none, the job is
//!   left `queued`, a single error "no enabled server configured" is logged and the run stops.
//!   Files recorded as `done` in earlier runs are passed to the engine as `skip_files`, so an
//!   interrupted job resumes instead of restarting. Progress (`bytes_done`) is persisted at
//!   most once per second, at every file end and when the job ends. Engine events are
//!   rebroadcast as [`ServiceEvent::Engine`]; file-level events and engine log lines become
//!   human log lines (never one per segment).
//! * **Outcomes.** Success runs post-processing (F5) when enabled; a PAR2 or extraction
//!   failure marks the job `failed` with the reason, so the user can, for example, set the
//!   archive password and [`Service::retry_job`] (completed files are skipped on retry).
//!   Missing articles mark the job `failed` naming up to five message ids. Authentication
//!   and connection failures mark the job `failed` and stop the run instead of burning
//!   through the queue. A job interrupted by [`Service::shutdown`] goes back to `queued`.
//! * **Throughput (F6).** A 1 Hz sampler emits [`ServiceEvent::Throughput`], stores one sample
//!   per second while bytes moved or a run is active, adds to the lifetime download counter
//!   and prunes samples older than seven days once an hour.
//! * **Auto-shutdown (F8).** When a run that processed at least one job drains the queue and
//!   `auto_shutdown` is set, a cancellable countdown ([`ServiceEvent::ShutdownCountdown`]) ends
//!   in the platform power-off command. It only ever happens when
//!   [`ServiceOptions::allow_shutdown`] is true (false for the CLI and tests); the command
//!   can be replaced with [`Service::set_shutdown_hook`].
//! * **Logs (F9).** A ring of the newest [`LOG_CAPACITY`] lines ([`Service::logs`], newest
//!   first); every line is also broadcast as [`ServiceEvent::JobLog`] (and, for errors, as
//!   [`ServiceEvent::Error`]) and mirrored to `tracing`. Passwords never appear in any line.
//!
//! Store calls are synchronous and short; the methods of [`Service`] that are not `async`
//! can be called from any thread (the GUI thread included) as long as the runtime behind the
//! [`Handle`] is alive.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

// Lets the shared test helpers (tests/common) name this crate from inside unit tests.
#[cfg(test)]
extern crate self as newskite_service;

mod runner;
mod shutdown;
mod timers;

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use newskite_engine::Progress;
use newskite_nzb::NzbError;
use newskite_postproc::Tools;
use newskite_types::{format, EngineEvent, ServerConfig, Settings};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

pub use newskite_store::{
    Bookmark, Job, JobFile, JobState, NewBookmark, Store, StoreError, ThroughputSample, Totals,
};
pub use shutdown::shutdown_command;

/// Number of lines kept in the in-memory log ring.
pub const LOG_CAPACITY: usize = 50;

/// Capacity of the event broadcast channel; slow receivers lag (and skip) rather than
/// block the service.
pub const EVENT_CAPACITY: usize = 1024;

/// Throughput samples older than this are pruned (seven days).
pub const SAMPLE_RETENTION_SECS: i64 = 7 * 24 * 3600;

/// Timeout for [`Service::test_server`].
pub const TEST_SERVER_TIMEOUT: Duration = Duration::from_secs(10);

/// Connect timeout (TCP + TLS + greeting + auth) used for downloads.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Message logged (once per run) when no usable server is configured.
pub const NO_SERVER_MESSAGE: &str = "no enabled server configured";

/// Events broadcast to every subscriber (the GUI, the CLI).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceEvent {
    /// A download engine event of a job (`engine://event`).
    Engine {
        /// Job the event belongs to.
        job_id: i64,
        /// The engine event.
        event: EngineEvent,
    },
    /// A human log line (`engine://job`); also stored in the log ring.
    JobLog(String),
    /// Job rows changed (added, removed, state or progress); reload the list.
    JobsChanged,
    /// No queue run is active any more (`engine://queue-idle`).
    QueueIdle,
    /// Pause acknowledged (`engine://paused`); the current job still finishes.
    Paused,
    /// The queue was resumed.
    Resumed,
    /// Names of jobs imported from the watch folder (`engine://watch-imported`).
    WatchImported(Vec<String>),
    /// Bytes downloaded during the last second (~1 Hz).
    Throughput {
        /// Decoded bytes per second.
        bytes_per_sec: u64,
    },
    /// Auto-shutdown countdown tick; `0` means the shutdown command is being run.
    ShutdownCountdown {
        /// Seconds left before shutting down.
        secs_left: u32,
    },
    /// The auto-shutdown countdown was canceled.
    ShutdownCanceled,
    /// An error line (also delivered as `JobLog`); drives the status-bar error state.
    Error(String),
}

/// Severity of a [`LogLine`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LogLevel {
    /// Normal progress information.
    Info,
    /// Something unexpected that did not stop the work.
    Warn,
    /// A failure.
    Error,
}

impl LogLevel {
    /// `"info"`, `"warn"` or `"error"`.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One line of the in-app message log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    /// Unix seconds.
    pub ts: i64,
    /// Severity.
    pub level: LogLevel,
    /// Human text (never contains passwords).
    pub text: String,
}

/// Snapshot of the queue state for status displays.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// A queue run is active.
    pub running: bool,
    /// Pause requested: the run stops (or stopped) at the next job boundary.
    pub paused: bool,
    /// Job currently downloading or post-processing.
    pub active_job: Option<i64>,
    /// Bytes per second over the last sampled second.
    pub speed_bps: u64,
    /// Last run-level error (cleared when a run starts).
    pub last_error: Option<String>,
}

/// Construction options for [`Service`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceOptions {
    /// Directory holding the database (see [`newskite_types::config::default_data_dir`]).
    pub data_dir: PathBuf,
    /// Allow the auto-shutdown feature to power off the computer. False in tests and the CLI.
    pub allow_shutdown: bool,
}

impl ServiceOptions {
    /// Options for `data_dir` with `allow_shutdown = false`.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self { data_dir: data_dir.into(), allow_shutdown: false }
    }
}

/// Replacement for the platform power-off command (see [`Service::set_shutdown_hook`]).
/// Runs on a blocking thread; an `Err` is logged.
pub type ShutdownHook = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Errors returned by [`Service`] commands. Every error is also logged.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// Database failure.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The NZB could not be read or parsed.
    #[error(transparent)]
    Nzb(#[from] NzbError),
    /// No enabled server with a host is configured.
    #[error("no enabled server configured")]
    NoServer,
    /// The NZB is already queued or downloading (carries the existing job's name).
    #[error("already queued: {0}")]
    Duplicate(String),
    /// No job (or bookmark) with this id.
    #[error("job {0} not found")]
    NotFound(i64),
    /// Filesystem failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Anything else (invalid request, background task failure).
    #[error("{0}")]
    Other(String),
}

/// Map a store error, turning `NotFound` into [`ServiceError::NotFound`] for `id`.
fn store_err(id: i64, e: StoreError) -> ServiceError {
    match e {
        StoreError::NotFound => ServiceError::NotFound(id),
        other => ServiceError::Store(other),
    }
}

/// Mutable queue state, guarded by `Inner::state`. Never held across an `.await`.
#[derive(Default)]
pub(crate) struct RunState {
    pub(crate) running: bool,
    pub(crate) paused: bool,
    pub(crate) active_job: Option<i64>,
    pub(crate) job_cancel: Option<CancellationToken>,
    /// The active job was removed by the user (its cancellation is not a failure).
    pub(crate) removed_active: bool,
    pub(crate) last_error: Option<String>,
    /// Token of the running auto-shutdown countdown.
    pub(crate) countdown: Option<CancellationToken>,
}

pub(crate) struct Inner {
    pub(crate) store: Store,
    pub(crate) rt: Handle,
    pub(crate) opts: ServiceOptions,
    pub(crate) events: broadcast::Sender<ServiceEvent>,
    pub(crate) state: Mutex<RunState>,
    logs: Mutex<VecDeque<LogLine>>,
    /// Shared counters handed to every engine run; the sampler diffs `bytes_down`.
    pub(crate) progress: Arc<Progress>,
    /// Fired by [`Service::shutdown`]; parent of every job and countdown token.
    pub(crate) shutdown: CancellationToken,
    /// `true` while a run is active (backs [`Service::wait_idle`]).
    pub(crate) running_tx: watch::Sender<bool>,
    pub(crate) speed_bps: AtomicU64,
    pub(crate) tools: OnceLock<Tools>,
    pub(crate) shutdown_hook: Mutex<Option<ShutdownHook>>,
    /// Watch-folder files that failed to import, with their mtime, so timer scans do not
    /// log the same problem every interval.
    pub(crate) watch_rejected: Mutex<HashSet<(PathBuf, Option<SystemTime>)>>,
    settings_error_logged: AtomicBool,
    pub(crate) sampler_error_logged: AtomicBool,
    /// In-memory settings used instead of the stored document (never persisted); set by
    /// [`Service::set_runtime_settings_override`] (the headless CLI).
    settings_override: Mutex<Option<Settings>>,
}

/// Lock a mutex, recovering the data if a panicking thread poisoned it.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Current unix time in seconds.
pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

impl Inner {
    pub(crate) fn emit(&self, ev: ServiceEvent) {
        // No subscribers is fine.
        let _ = self.events.send(ev);
    }

    pub(crate) fn lock_state(&self) -> MutexGuard<'_, RunState> {
        lock(&self.state)
    }

    pub(crate) fn log(&self, level: LogLevel, text: impl Into<String>) {
        let text = text.into();
        match level {
            LogLevel::Info => tracing::info!(target: "newskite_service", "{text}"),
            LogLevel::Warn => tracing::warn!(target: "newskite_service", "{text}"),
            LogLevel::Error => tracing::error!(target: "newskite_service", "{text}"),
        }
        {
            let mut logs = lock(&self.logs);
            logs.push_front(LogLine { ts: now_secs(), level, text: text.clone() });
            logs.truncate(LOG_CAPACITY);
        }
        if level == LogLevel::Error {
            self.emit(ServiceEvent::Error(text.clone()));
        }
        self.emit(ServiceEvent::JobLog(text));
    }

    /// Effective settings for runs, imports and timers: the runtime override when one is
    /// set, otherwise the stored document.
    pub(crate) fn settings(&self) -> Settings {
        if let Some(s) = lock(&self.settings_override).as_ref() {
            return s.clone();
        }
        self.stored_settings()
    }

    /// The stored settings document; a load failure is logged once and defaults are used.
    pub(crate) fn stored_settings(&self) -> Settings {
        match self.store.load_settings() {
            Ok(s) => s,
            Err(e) => {
                if !self.settings_error_logged.swap(true, Ordering::Relaxed) {
                    self.log(LogLevel::Error, format!("settings could not be loaded ({e}); using defaults"));
                }
                Settings::default().normalized()
            }
        }
    }

    /// Record a run-level error for [`Status::last_error`].
    pub(crate) fn set_last_error(&self, msg: impl Into<String>) {
        self.lock_state().last_error = Some(msg.into());
    }

    /// Parse and persist a new job (no logging of failures; callers do that).
    pub(crate) fn add_nzb_inner(&self, path: &Path) -> Result<Job, ServiceError> {
        let path = std::fs::canonicalize(path)?;
        let nzb = newskite_nzb::nzb::parse_file(&path)?;
        if let Some(existing) = self.store.job_by_source(&path)? {
            if existing.state.is_pending() {
                return Err(ServiceError::Duplicate(existing.name));
            }
        }
        let name = nzb.job_name(&path);
        let settings = self.settings();
        let jobs = self.store.list_jobs()?;
        let dest_dir = unique_dest(&settings.download_dir, &name, &jobs);
        let job = self.store.insert_job(&newskite_store::NewJob {
            name: name.clone(),
            bytes_total: nzb.total_bytes(),
            dest_dir,
            source_path: path,
            password: None,
        })?;
        self.log(
            LogLevel::Info,
            format!(
                "queued {name} ({} file{}, {})",
                nzb.files.len(),
                if nzb.files.len() == 1 { "" } else { "s" },
                format::bytes(job.bytes_total)
            ),
        );
        self.emit(ServiceEvent::JobsChanged);
        Ok(job)
    }
}

/// `<base>/<name>`, or `<base>/<name> (N)` when a pending job already uses that folder.
fn unique_dest(base: &Path, name: &str, jobs: &[Job]) -> PathBuf {
    let taken = |p: &Path| jobs.iter().any(|j| j.state.is_pending() && j.dest_dir == p);
    let mut candidate = base.join(name);
    let mut n = 2u32;
    while taken(&candidate) {
        candidate = base.join(format!("{name} ({n})"));
        n += 1;
    }
    candidate
}

/// The queue service. Cheap to clone; all clones share one state.
#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

impl fmt::Debug for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Service").field("opts", &self.inner.opts).field("status", &self.status()).finish()
    }
}

impl Service {
    /// Opens (or creates) the database in `opts.data_dir`, resets jobs left `downloading`
    /// by an earlier process to `queued`, and starts the throughput sampler and the
    /// watch-folder timer on `rt`.
    pub fn open(opts: ServiceOptions, rt: Handle) -> Result<Self, ServiceError> {
        let store = Store::open(&opts.data_dir)?;
        Ok(Self::with_store(store, opts, rt))
    }

    /// Like [`Service::open`] with an already opened store (`opts.data_dir` is not used to
    /// open anything). Interrupted jobs are reset and the timers are started.
    pub fn with_store(store: Store, opts: ServiceOptions, rt: Handle) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let (running_tx, _) = watch::channel(false);
        let inner = Arc::new(Inner {
            store,
            rt,
            opts,
            events,
            state: Mutex::new(RunState::default()),
            logs: Mutex::new(VecDeque::with_capacity(LOG_CAPACITY)),
            progress: Arc::new(Progress::new()),
            shutdown: CancellationToken::new(),
            running_tx,
            speed_bps: AtomicU64::new(0),
            tools: OnceLock::new(),
            shutdown_hook: Mutex::new(None),
            watch_rejected: Mutex::new(HashSet::new()),
            settings_error_logged: AtomicBool::new(false),
            sampler_error_logged: AtomicBool::new(false),
            settings_override: Mutex::new(None),
        });
        match inner.store.reset_interrupted() {
            Ok(0) => {}
            Ok(n) => inner.log(
                LogLevel::Info,
                format!("{n} interrupted job{} will resume", if n == 1 { "" } else { "s" }),
            ),
            Err(e) => inner.log(LogLevel::Error, format!("could not reset interrupted jobs: {e}")),
        }
        timers::spawn_sampler(&inner);
        timers::spawn_watch_timer(&inner);
        Self { inner }
    }

    /// A receiver for every event from now on. Receivers that fall more than
    /// [`EVENT_CAPACITY`] events behind get `RecvError::Lagged` and skip ahead.
    pub fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        self.inner.events.subscribe()
    }

    /// Imports an NZB: parse first (no row on failure), refuse a duplicate of a pending job
    /// with the same canonical source path, then persist the job `queued`.
    pub fn add_nzb(&self, path: &Path) -> Result<Job, ServiceError> {
        self.inner.add_nzb_inner(path).inspect_err(|e| {
            self.inner.log(LogLevel::Error, format!("could not add {}: {e}", path.display()));
        })
    }

    /// Scans the watch folder now and enqueues new `*.nzb` files. Returns the names of the
    /// imported jobs (empty, with a log line, when no watch folder is configured or it does
    /// not exist). Starts the queue afterwards when `watch_auto_start` is set and the queue
    /// is not paused.
    pub fn scan_watch_folder(&self) -> Result<Vec<String>, ServiceError> {
        timers::scan_watch_folder(&self.inner, true)
    }

    /// Starts a queue run. Returns `false` (and does nothing) when a run is already active
    /// or the service is shut down. Clears a pending pause and cancels a running
    /// auto-shutdown countdown.
    pub fn start(&self) -> bool {
        runner::start(&self.inner)
    }

    /// Requests a cooperative pause: the current job finishes, the next one does not start.
    /// Emits [`ServiceEvent::Paused`] immediately.
    pub fn pause(&self) {
        let running = {
            let mut st = self.inner.lock_state();
            if st.paused {
                return;
            }
            st.paused = true;
            st.running
        };
        self.inner.emit(ServiceEvent::Paused);
        self.inner.log(
            LogLevel::Info,
            if running { "pause requested; the current job will finish first" } else { "queue paused" },
        );
    }

    /// Clears a pause, emits [`ServiceEvent::Resumed`] and starts a run if none is active.
    pub fn resume(&self) {
        let was_paused = std::mem::take(&mut self.inner.lock_state().paused);
        if was_paused {
            self.inner.emit(ServiceEvent::Resumed);
            self.inner.log(LogLevel::Info, "queue resumed");
        }
        runner::start(&self.inner);
    }

    /// Snapshot of the queue state.
    pub fn status(&self) -> Status {
        let st = self.inner.lock_state();
        Status {
            running: st.running,
            paused: st.paused,
            active_job: st.active_job,
            speed_bps: self.inner.speed_bps.load(Ordering::Relaxed),
            last_error: st.last_error.clone(),
        }
    }

    /// Deletes a job (and its file rows). If it is downloading, the download is canceled;
    /// files already written stay on disk.
    pub fn remove_job(&self, id: i64) -> Result<(), ServiceError> {
        let result = (|| {
            let job = self.inner.store.get_job(id)?.ok_or(ServiceError::NotFound(id))?;
            self.inner.store.remove_job(id).map_err(|e| store_err(id, e))?;
            let cancel = {
                let mut st = self.inner.lock_state();
                if st.active_job == Some(id) {
                    st.removed_active = true;
                    st.job_cancel.clone()
                } else {
                    None
                }
            };
            if let Some(token) = cancel {
                token.cancel();
            }
            Ok(job)
        })();
        match result {
            Ok(job) => {
                self.inner.log(LogLevel::Info, format!("removed {}", job.name));
                self.inner.emit(ServiceEvent::JobsChanged);
                Ok(())
            }
            Err(e) => {
                self.inner.log(LogLevel::Error, format!("could not remove job {id}: {e}"));
                Err(e)
            }
        }
    }

    /// Puts a `failed` job back to `queued` (error cleared). Other states are refused with
    /// [`ServiceError::Other`]. Does not start the queue.
    pub fn retry_job(&self, id: i64) -> Result<(), ServiceError> {
        let result = (|| {
            let job = self.inner.store.get_job(id)?.ok_or(ServiceError::NotFound(id))?;
            if job.state != JobState::Failed {
                return Err(ServiceError::Other(format!(
                    "only failed jobs can be retried ({} is {})",
                    job.name, job.state
                )));
            }
            self.inner.store.set_job_state(id, JobState::Queued, None).map_err(|e| store_err(id, e))?;
            Ok(job)
        })();
        match result {
            Ok(job) => {
                self.inner.log(LogLevel::Info, format!("requeued {}", job.name));
                self.inner.emit(ServiceEvent::JobsChanged);
                Ok(())
            }
            Err(e) => {
                self.inner.log(LogLevel::Error, format!("could not retry job {id}: {e}"));
                Err(e)
            }
        }
    }

    /// Sets (or clears with `None`/empty) the archive password used by post-processing.
    /// The password is stored in plain text in the local database and never logged.
    pub fn set_job_password(&self, id: i64, pw: Option<String>) -> Result<(), ServiceError> {
        let pw = pw.filter(|p| !p.is_empty());
        match self.inner.store.set_job_password(id, pw.as_deref()) {
            Ok(()) => {
                self.inner.log(
                    LogLevel::Info,
                    format!("archive password {} for job {id}", if pw.is_some() { "set" } else { "cleared" }),
                );
                self.inner.emit(ServiceEvent::JobsChanged);
                Ok(())
            }
            Err(e) => {
                let e = store_err(id, e);
                self.inner.log(LogLevel::Error, format!("could not set the password of job {id}: {e}"));
                Err(e)
            }
        }
    }

    /// All jobs, id ascending (empty, with a logged error, if the database fails).
    pub fn jobs(&self) -> Vec<Job> {
        self.inner.store.list_jobs().unwrap_or_else(|e| {
            self.inner.log(LogLevel::Error, format!("could not list jobs: {e}"));
            Vec::new()
        })
    }

    /// Per-file rows of a job (empty on error).
    pub fn job_files(&self, job_id: i64) -> Vec<JobFile> {
        self.inner.store.job_files(job_id).unwrap_or_else(|e| {
            self.inner.log(LogLevel::Error, format!("could not list the files of job {job_id}: {e}"));
            Vec::new()
        })
    }

    /// Account summary counters (zeros on error).
    pub fn totals(&self) -> Totals {
        self.inner.store.totals().unwrap_or_else(|e| {
            self.inner.log(LogLevel::Error, format!("could not read totals: {e}"));
            Totals::default()
        })
    }

    /// The stored settings document (defaults, with a logged error, if it cannot be
    /// loaded). A runtime override ([`Service::set_runtime_settings_override`]) is not
    /// reflected here, so saving what this returns never persists override values.
    pub fn settings(&self) -> Settings {
        self.inner.stored_settings()
    }

    /// Replaces (with `Some`) or clears (with `None`) an in-memory settings override.
    /// While set, imports (destination folder), queue runs (servers, connections,
    /// retries, speed limit, post-processing), the watch timer and auto-shutdown use the
    /// override instead of the stored document. The override is normalized, is never
    /// written to the database and is never logged (it may carry server passwords). Used
    /// by the headless CLI for `--server`/`--dest`. Like saved settings, it applies from
    /// the next job.
    pub fn set_runtime_settings_override(&self, s: Option<Settings>) {
        *lock(&self.inner.settings_override) = s.map(Settings::normalized);
    }

    /// `true` while a runtime settings override is set.
    pub fn has_runtime_settings_override(&self) -> bool {
        lock(&self.inner.settings_override).is_some()
    }

    /// Normalizes and persists the settings. They apply to the next job (a running job
    /// keeps its servers, connections and speed limit).
    pub fn save_settings(&self, s: Settings) -> Result<(), ServiceError> {
        match self.inner.store.save_settings(&s.normalized()) {
            Ok(()) => {
                self.inner.settings_error_logged.store(false, Ordering::Relaxed);
                self.inner.log(LogLevel::Info, "settings saved");
                Ok(())
            }
            Err(e) => {
                self.inner.log(LogLevel::Error, format!("could not save settings: {e}"));
                Err(e.into())
            }
        }
    }

    /// Bookmarks in tree order (empty on error).
    pub fn bookmarks(&self) -> Vec<Bookmark> {
        self.inner.store.list_bookmarks().unwrap_or_else(|e| {
            self.inner.log(LogLevel::Error, format!("could not list bookmarks: {e}"));
            Vec::new()
        })
    }

    /// Adds a bookmark after its siblings.
    pub fn add_bookmark(&self, b: NewBookmark) -> Result<Bookmark, ServiceError> {
        self.inner.store.add_bookmark(&b).map_err(|e| {
            self.inner.log(LogLevel::Error, format!("could not add bookmark {}: {e}", b.name));
            e.into()
        })
    }

    /// Updates a bookmark (name, group, color, expanded, order, parent).
    pub fn update_bookmark(&self, b: &Bookmark) -> Result<(), ServiceError> {
        self.inner.store.update_bookmark(b).map_err(|e| {
            let e = store_err(b.id, e);
            self.inner.log(LogLevel::Error, format!("could not update bookmark {}: {e}", b.name));
            e
        })
    }

    /// Removes a bookmark and its children.
    pub fn remove_bookmark(&self, id: i64) -> Result<(), ServiceError> {
        self.inner.store.remove_bookmark(id).map_err(|e| {
            let e = store_err(id, e);
            self.inner.log(LogLevel::Error, format!("could not remove bookmark {id}: {e}"));
            e
        })
    }

    /// Stored throughput samples of the last `secs` seconds, oldest first (empty on error).
    pub fn throughput_history(&self, secs: u64) -> Vec<ThroughputSample> {
        let since = now_secs().saturating_sub(i64::try_from(secs).unwrap_or(i64::MAX));
        self.inner.store.samples_since(since).unwrap_or_else(|e| {
            self.inner.log(LogLevel::Error, format!("could not read throughput history: {e}"));
            Vec::new()
        })
    }

    /// The newest [`LOG_CAPACITY`] log lines, newest first.
    pub fn logs(&self) -> Vec<LogLine> {
        lock(&self.inner.logs).iter().cloned().collect()
    }

    /// Adds a line to the log ring (UI and command failures go here too).
    pub fn log(&self, level: LogLevel, text: impl Into<String>) {
        self.inner.log(level, text);
    }

    /// Settings "Test": connect, authenticate and send `DATE` (10 s timeout). Returns the
    /// transcript of raw replies (credentials redacted) or the error text.
    pub async fn test_server(&self, s: ServerConfig) -> Result<String, String> {
        if s.host.trim().is_empty() {
            let msg = "server test failed: no host set".to_string();
            self.inner.log(LogLevel::Error, msg.clone());
            return Err("no host set".into());
        }
        let addr = s.address();
        match newskite_nntp::test_server(&s, TEST_SERVER_TIMEOUT).await {
            Ok(t) => {
                self.inner.log(LogLevel::Info, format!("server test {addr}: ok"));
                Ok(t)
            }
            Err(e) => {
                let msg = e.to_string();
                self.inner.log(LogLevel::Error, format!("server test {addr} failed: {msg}"));
                Err(msg)
            }
        }
    }

    /// Cancels a running auto-shutdown countdown (emits [`ServiceEvent::ShutdownCanceled`]).
    /// Does nothing when no countdown is running.
    pub fn cancel_shutdown(&self) {
        shutdown::cancel(&self.inner);
    }

    /// Replaces the platform power-off command run when an auto-shutdown countdown
    /// completes (tests, or a GUI that wants to save state first). Only used when
    /// [`ServiceOptions::allow_shutdown`] is true.
    pub fn set_shutdown_hook(&self, hook: ShutdownHook) {
        *lock(&self.inner.shutdown_hook) = Some(hook);
    }

    /// Resolves when no queue run is active.
    pub async fn wait_idle(&self) {
        let mut rx = self.inner.running_tx.subscribe();
        // The sender lives in `Inner`, which `self` keeps alive, so this cannot fail.
        let _ = rx.wait_for(|running| !*running).await;
    }

    /// Stops the timers and the countdown and cancels the active job, which goes back to
    /// `queued` so the next start resumes it. Post-processing already in progress finishes.
    /// Use [`Service::wait_idle`] to wait for the run to end. Irreversible.
    pub fn shutdown(&self) {
        if self.inner.shutdown.is_cancelled() {
            return;
        }
        {
            // Under the lock, so a countdown that is about to fire sees the cancellation.
            let mut st = self.inner.lock_state();
            st.countdown = None;
            self.inner.shutdown.cancel();
        }
        self.inner.log(LogLevel::Info, "service stopping");
    }

    /// The options the service was created with.
    pub fn options(&self) -> &ServiceOptions {
        &self.inner.opts
    }
}

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod common;

#[cfg(test)]
mod tests;
