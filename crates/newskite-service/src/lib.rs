//! Newskite queue service, shared by the GUI and the headless CLI.
//!
//! A [`Service`] owns the [`Store`] and drives the download queue on a tokio runtime:
//!
//! * **Import (F1).** [`Service::add_nzb`] parses the NZB first; a malformed or unreadable
//!   file produces [`ServiceError::Nzb`] and no job row. The job is persisted `queued` with
//!   its byte total, its canonical source path and its destination folder
//!   (`<download_dir>/<job name>`, made unique among every job in the store, compared
//!   case-insensitively, and among folders already on disk) before anything downloads.
//!   Imports are serialized, so concurrent adds and scans never insert one NZB twice.
//! * **Watch folder (F2).** [`Service::scan_watch_folder`] enqueues every `*.nzb` (case
//!   insensitive, not recursive) whose canonical path has no job in any state and was not
//!   imported (or its job removed) before with the same size and mtime, so removing a job
//!   does not bring it back.
//!   A background timer scans every `watch_interval_secs` while `watch_enabled` (settings are
//!   re-read every second, so changes apply without a restart). Manual scans work whenever a
//!   watch folder is configured. Timer scans log only imports and new problems.
//! * **Queue (F4).** One run at a time ([`Service::start`] is idempotent). A run processes
//!   `queued` jobs in queue order (an explicit position set by [`Service::move_job`], then
//!   oldest first); `paused` jobs are held by the user and skipped. [`Service::pause`] is
//!   cooperative: the current job finishes and the run stops before the next one;
//!   [`Service::pause_now`] interrupts the current job too (it goes back to `queued` and
//!   resumes first). The `Paused` event is emitted at once as the acknowledgment.
//!   [`Service::resume`] clears the pause and starts a run if none is active.
//!   [`Service::remove_job`] deletes the row and cancels the job if it is downloading
//!   (partial files are left on disk).
//! * **Per-job control.** [`Service::pause_job`] holds one job (interrupting it if it is
//!   downloading; completed files are kept), [`Service::start_job`] downloads one job without
//!   starting the rest of the queue (or adds it to the active run), [`Service::move_job`]
//!   and [`Service::reorder_queue`] change the queue order, and
//!   [`Service::clear_history`] removes finished jobs from the list.
//! * **Deleting files.** [`Service::delete_job_files`] removes a finished job from the list
//!   and deletes its download folder; [`Service::clear_history_and_files`] does that for
//!   every finished job. A folder is refused (and the job kept) when it is not a plain
//!   absolute path, is or holds the download folder or the home folder, or overlaps another
//!   job's folder. Links inside are removed, never followed.
//! * **Per job.** Usable servers come from [`Settings::usable_servers`]; with none, the job is
//!   left `queued`, a single error "no enabled server configured" is logged and the run stops.
//!   Files recorded as `done` in earlier runs are passed to the engine as `skip_files`, so an
//!   interrupted job resumes instead of restarting. A `done` file that is missing or whose
//!   on-disk length differs from the recorded one is not skipped (one that is shorter, as
//!   lost writes leave it, is removed first so it is downloaded again under its own name). Progress (`bytes_done`) is persisted at
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
//!   and prunes samples older than seven days once an hour. Bytes not yet sampled are
//!   flushed when a run ends and at shutdown.
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

mod files;
mod runner;
mod shutdown;
mod timers;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use newskite_engine::{ConnectionPool, Progress, SpeedLimit};
use newskite_nzb::NzbError;
pub use newskite_postproc::PostStage;
use newskite_types::{format, EngineEvent, ServerConfig, Settings};
pub use newskite_types::{ArticleCounts, ArticleMap, ArticleState, FileArticles};
use tokio::runtime::Handle;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

pub use newskite_store::{
    Bookmark, Job, JobFile, JobLogLine, JobState, JobStats, NewBookmark, Store, StoreError, ThroughputSample, Totals,
};
pub use newskite_types::{FailureKind, JobOutcome, ServerHealth, StepRecord, StepState};
pub use files::Deleted;
pub use shutdown::shutdown_command;

/// Number of lines kept in the in-memory log ring.
pub const LOG_CAPACITY: usize = 500;

/// The log file is rotated (to `newskite.log.1`) once it grows past this size.
pub const LOG_FILE_MAX: u64 = 4 << 20;

/// Name of the log file in the data directory.
pub const LOG_FILE_NAME: &str = "newskite.log";

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
    /// Post-processing progress of the active job (also in [`Status::post`]).
    PostProgress(PostProgress),
    /// A job reached `done` (`ok`) or `failed`; `detail` is a short summary or the reason.
    JobFinished {
        /// The job.
        job_id: i64,
        /// Its name.
        name: String,
        /// Done rather than failed.
        ok: bool,
        /// "repaired and complete", or the failure reason.
        detail: String,
    },
}

/// Where post-processing of a job is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostProgress {
    /// The job being post-processed.
    pub job_id: i64,
    /// Verify, repair, extract or cleanup.
    pub stage: PostStage,
    /// Thousandths done, when the stage can tell.
    pub permille: Option<u16>,
    /// Short detail ("Verifying movie.mkv (2/5)").
    pub detail: String,
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
    /// The job the line is about, if any.
    pub job_id: Option<i64>,
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
    /// The jobs the active run is limited to ([`Service::start_job`]); `None` while the
    /// whole queue runs or no run is active.
    pub run_scope: Option<Vec<i64>>,
    /// Post-processing progress of the active job, while it runs.
    pub post: Option<PostProgress>,
}

/// Where [`Service::move_job`] moves a job within the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobMove {
    /// First in the queue.
    Top,
    /// One place earlier.
    Up,
    /// One place later.
    Down,
    /// Last in the queue.
    Bottom,
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
    /// The active job was interrupted by the user; when its download returns it takes
    /// this state (`paused` for a hold, `queued` for a pause of the whole queue).
    pub(crate) stop_active: Option<JobState>,
    /// The jobs the active run is limited to (`None`: the whole queue).
    pub(crate) only: Option<Vec<i64>>,
    pub(crate) last_error: Option<String>,
    /// Post-processing progress of the active job.
    pub(crate) post: Option<PostProgress>,
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
    /// Open handle of the log file (lazily opened, dropped on rotation).
    log_file: Mutex<Option<std::fs::File>>,
    /// Shared counters handed to every engine run; the sampler diffs `bytes_down`.
    pub(crate) progress: Arc<Progress>,
    /// NNTP connections kept between the jobs of one run; closed when the run ends.
    pub(crate) pool: Arc<ConnectionPool>,
    /// Fired by [`Service::shutdown`]; parent of every job and countdown token.
    pub(crate) shutdown: CancellationToken,
    /// `true` while a run is active (backs [`Service::wait_idle`]).
    pub(crate) running_tx: watch::Sender<bool>,
    pub(crate) speed_bps: AtomicU64,
    pub(crate) shutdown_hook: Mutex<Option<ShutdownHook>>,
    /// Watch-folder files that failed to import, with their mtime, so timer scans do not
    /// log the same problem every interval.
    pub(crate) watch_rejected: Mutex<HashSet<(PathBuf, Option<SystemTime>)>>,
    /// Held across the duplicate checks and the insert of an import.
    import_lock: Mutex<()>,
    /// `progress.bytes_down` already written to the throughput history and lifetime total.
    pub(crate) sampled_down: Mutex<u64>,
    settings_error_logged: AtomicBool,
    pub(crate) sampler_error_logged: AtomicBool,
    /// In-memory settings used instead of the stored document (never persisted); set by
    /// [`Service::set_runtime_settings_override`] (the headless CLI).
    settings_override: Mutex<Option<Settings>>,
    /// The speed limit every job's engine reads as it runs; kept equal to the effective
    /// settings' limit (set on save, on override and at each job's start).
    pub(crate) speed_limit: SpeedLimit,
    /// Jobs requeued by [`Service::retry_job_on_backup`], with the articles they were
    /// missing before: their next download asks the fail-over servers first. Cleared when that download ends (not when it is merely
    /// interrupted, so a paused backup retry resumes as one).
    pub(crate) backup_first: Mutex<HashMap<i64, u64>>,
    /// Article maps of the jobs downloading now, kept up to date from engine events and
    /// saved to the store when their run ends (see [`Service::article_map`]).
    pub(crate) articles: Mutex<HashMap<i64, ArticleMap>>,
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
        self.log_line(None, level, text.into());
    }

    /// A line about one job: also kept in that job's own log.
    pub(crate) fn log_job(&self, job_id: i64, level: LogLevel, text: impl Into<String>) {
        self.log_line(Some(job_id), level, text.into());
    }

    fn log_line(&self, job_id: Option<i64>, level: LogLevel, text: String) {
        let ts = now_secs();
        if let Some(id) = job_id {
            // A removed job has no log any more; that is fine.
            let _ = self.store.append_job_log(id, ts, level.as_str(), &text);
        }
        self.write_log_file(ts, level, job_id, &text);
        match level {
            LogLevel::Info => tracing::info!(target: "newskite_service", "{text}"),
            LogLevel::Warn => tracing::warn!(target: "newskite_service", "{text}"),
            LogLevel::Error => tracing::error!(target: "newskite_service", "{text}"),
        }
        {
            let mut logs = lock(&self.logs);
            logs.push_front(LogLine { ts, level, text: text.clone(), job_id });
            logs.truncate(LOG_CAPACITY);
        }
        if level == LogLevel::Error {
            self.emit(ServiceEvent::Error(text.clone()));
        }
        self.emit(ServiceEvent::JobLog(text));
    }

    /// Appends to `<data_dir>/newskite.log`, rotating it past [`LOG_FILE_MAX`]. Failures
    /// are ignored (the file is a convenience, the in-app log is the record).
    fn write_log_file(&self, ts: i64, level: LogLevel, job_id: Option<i64>, text: &str) {
        use std::io::Write;
        let mut guard = lock(&self.log_file);
        let path = self.opts.data_dir.join(LOG_FILE_NAME);
        if guard.is_none() {
            *guard = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
        }
        let Some(f) = guard.as_mut() else { return };
        let job = job_id.map(|j| format!(" job={j}")).unwrap_or_default();
        let _ = writeln!(f, "{} {:5}{job} {text}", format::timestamp_utc(ts), level.as_str());
        if f.metadata().map(|m| m.len() > LOG_FILE_MAX).unwrap_or(false) {
            *guard = None;
            let _ = std::fs::rename(&path, self.opts.data_dir.join(format!("{LOG_FILE_NAME}.1")));
        }
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
                    self.log(
                        LogLevel::Error,
                        format!(
                            "settings could not be loaded ({e}); using defaults (saving settings keeps \
                             the unreadable document as a backup)"
                        ),
                    );
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
        let job = self.import_nzb(path, None)?;
        // Without a watch fingerprint every outcome is a job or an error.
        job.ok_or_else(|| ServiceError::Other("import skipped".into()))
    }

    /// Parse and persist a new job. The duplicate checks and the insert run under
    /// `import_lock`, so concurrent imports (watch timer, GUI, launch arguments) can never
    /// both insert the same NZB. With `watch = Some(fingerprint)` (a watch-folder import),
    /// a file that already has a job in any state or was seen with that fingerprint is
    /// skipped (`Ok(None)`), and a successful import records the fingerprint.
    pub(crate) fn import_nzb(&self, path: &Path, watch: Option<Fingerprint>) -> Result<Option<Job>, ServiceError> {
        let path = std::fs::canonicalize(path)?;
        let nzb = newskite_nzb::nzb::parse_file(&path)?;
        let name = nzb.job_name(&path);
        let settings = self.settings();
        let job = {
            let _import = lock(&self.import_lock);
            if let Some(fp) = watch {
                if self.watch_source_known(&path, fp)? {
                    return Ok(None);
                }
            }
            if let Some(existing) = self.store.pending_job_by_source(&path)? {
                return Err(ServiceError::Duplicate(existing.name));
            }
            let jobs = self.store.list_jobs()?;
            let dest_dir = unique_dest(&settings.download_dir, &name, &jobs);
            let job = self.store.insert_job(&newskite_store::NewJob {
                name: name.clone(),
                bytes_total: nzb.total_bytes(),
                dest_dir,
                source_path: path.clone(),
                password: None,
            })?;
            if let Some(fp) = watch {
                if let Err(e) = self.store.mark_source_seen(&path, fp.size, fp.mtime) {
                    self.log(LogLevel::Warn, format!("could not record {} as imported: {e}", path.display()));
                }
            }
            job
        };
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
        Ok(Some(job))
    }

    /// Watch-folder dedupe: `path` has a job in any state, or was imported (or its job
    /// removed) while it had this fingerprint.
    pub(crate) fn watch_source_known(&self, path: &Path, fp: Fingerprint) -> Result<bool, StoreError> {
        Ok(self.store.job_by_source(path)?.is_some() || self.store.source_seen(path, fp.size, fp.mtime)?)
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Size and modification time (unix nanoseconds) of an NZB file; a file replaced under
/// the same name gets a new fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    pub(crate) size: u64,
    pub(crate) mtime: i64,
}

impl Fingerprint {
    /// The fingerprint of the file at `path`, or `None` if it cannot be read.
    pub(crate) fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
        Some(Self { size: meta.len(), mtime })
    }
}

/// `<base>/<name>`, or `<base>/<name> (N)` when that folder is taken: used by any job in
/// the store (whatever its state, compared case-insensitively, since Windows and macOS
/// treat `Foo` and `foo` as one folder) or already present on disk. So two jobs never
/// share a folder and post-processing never sees another job's files.
fn unique_dest(base: &Path, name: &str, jobs: &[Job]) -> PathBuf {
    let key = |p: &Path| p.to_string_lossy().to_lowercase();
    let used: HashSet<String> = jobs.iter().map(|j| key(&j.dest_dir)).collect();
    let taken = |p: &Path| used.contains(&key(p)) || std::fs::symlink_metadata(p).is_ok();
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
    ///
    /// Single instance: [`Store::open`] locks the data directory, so while another service
    /// (GUI or headless, in any process) has it open this fails with a
    /// [`ServiceError::Store`] saying another Newskite instance is using it, and nothing
    /// in the database is touched.
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
            log_file: Mutex::new(None),
            progress: Arc::new(Progress::new()),
            pool: Arc::new(ConnectionPool::new()),
            shutdown: CancellationToken::new(),
            running_tx,
            speed_bps: AtomicU64::new(0),
            shutdown_hook: Mutex::new(None),
            watch_rejected: Mutex::new(HashSet::new()),
            import_lock: Mutex::new(()),
            sampled_down: Mutex::new(0),
            settings_error_logged: AtomicBool::new(false),
            sampler_error_logged: AtomicBool::new(false),
            settings_override: Mutex::new(None),
            speed_limit: SpeedLimit::default(),
            backup_first: Mutex::new(HashMap::new()),
            articles: Mutex::new(HashMap::new()),
        });
        inner.speed_limit.set(inner.settings().speed_limit_bps());
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

    /// Like [`Service::start`], but the run processes only the jobs whose id is in `ids`
    /// (oldest first, as usual); every other queued or paused job stays untouched and the
    /// run ends once none of the listed jobs is runnable. Returns `false` (and does
    /// nothing) when a run is already active or the service is shut down. Used by headless
    /// mode so it never works through the stored backlog with its own server override.
    pub fn start_jobs(&self, ids: &[i64]) -> bool {
        runner::start_with(&self.inner, Some(ids.to_vec()))
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

    /// Pauses the queue at once: like [`Service::pause`], and the job downloading now is
    /// interrupted and put back to `queued` (completed files are kept), so it resumes first.
    pub fn pause_now(&self) {
        let (was_paused, cancel) = {
            let mut st = self.inner.lock_state();
            let was = std::mem::replace(&mut st.paused, true);
            let cancel = if st.active_job.is_some() && !st.removed_active && st.stop_active.is_none() {
                st.stop_active = Some(JobState::Queued);
                st.job_cancel.clone()
            } else {
                None
            };
            (was, cancel)
        };
        if let Some(token) = cancel {
            token.cancel();
        }
        if !was_paused {
            self.inner.emit(ServiceEvent::Paused);
            self.inner.log(LogLevel::Info, "queue paused");
        }
    }

    /// Holds one job: a `queued` job becomes `paused` (skipped by runs); a downloading job
    /// is interrupted and becomes `paused` once its download stops (completed files are
    /// kept), and the run moves on to the next job. Holding a `paused` job does nothing;
    /// finished jobs are refused.
    pub fn pause_job(&self, id: i64) -> Result<(), ServiceError> {
        let result = (|| {
            let job = self.inner.store.get_job(id)?.ok_or(ServiceError::NotFound(id))?;
            let cancel = {
                let mut st = self.inner.lock_state();
                if st.active_job == Some(id) {
                    if st.removed_active {
                        return Err(ServiceError::NotFound(id));
                    }
                    st.stop_active = Some(JobState::Paused);
                    st.job_cancel.clone()
                } else {
                    match job.state {
                        JobState::Queued => {
                            self.inner.store.set_job_state(id, JobState::Paused, None).map_err(|e| store_err(id, e))?
                        }
                        JobState::Paused => return Ok((job, false)),
                        other => {
                            return Err(ServiceError::Other(format!(
                                "only queued or downloading jobs can be paused ({} is {other})",
                                job.name
                            )))
                        }
                    }
                    None
                }
            };
            if let Some(token) = cancel {
                token.cancel();
            }
            Ok((job, true))
        })();
        match result {
            Ok((job, changed)) => {
                if changed {
                    self.inner.log(LogLevel::Info, format!("paused {}", job.name));
                    self.inner.emit(ServiceEvent::JobsChanged);
                }
                Ok(())
            }
            Err(e) => {
                self.inner.log(LogLevel::Error, format!("could not pause job {id}: {e}"));
                Err(e)
            }
        }
    }

    /// Downloads one job: a `paused` or `queued` job becomes `queued`; if no run is active
    /// a run limited to this job starts (the rest of the queue stays as it is), and an
    /// active run limited to other jobs takes this one too. A run of the whole queue picks
    /// it up in queue order. A failed job is retried first ([`Service::retry_job`]).
    /// Clears a pending pause of the queue.
    pub fn start_job(&self, id: i64) -> Result<(), ServiceError> {
        let job = match self.inner.store.get_job(id) {
            Ok(Some(job)) => job,
            Ok(None) => {
                self.inner.log(LogLevel::Error, format!("could not start job {id}: not found"));
                return Err(ServiceError::NotFound(id));
            }
            Err(e) => {
                let e = ServiceError::Store(e);
                self.inner.log(LogLevel::Error, format!("could not start job {id}: {e}"));
                return Err(e);
            }
        };
        match job.state {
            JobState::Failed => self.retry_job(id)?,
            JobState::Paused => {
                if let Err(e) = self.inner.store.set_job_state(id, JobState::Queued, None) {
                    let e = store_err(id, e);
                    self.inner.log(LogLevel::Error, format!("could not start {}: {e}", job.name));
                    return Err(e);
                }
                self.inner.log(LogLevel::Info, format!("resumed {}", job.name));
                self.inner.emit(ServiceEvent::JobsChanged);
            }
            JobState::Queued | JobState::Downloading => {}
            JobState::Done => {
                let e = ServiceError::Other(format!("{} is already done", job.name));
                self.inner.log(LogLevel::Error, format!("could not start job {id}: {e}"));
                return Err(e);
            }
        }
        let was_paused = std::mem::take(&mut self.inner.lock_state().paused);
        if was_paused {
            self.inner.emit(ServiceEvent::Resumed);
        }
        runner::start_with(&self.inner, Some(vec![id]));
        Ok(())
    }

    /// Ids of the pending jobs (`queued`, `downloading`, `paused`) in queue order (empty,
    /// with a logged error, if the database fails).
    pub fn queue_order(&self) -> Vec<i64> {
        self.inner.store.queue_order().unwrap_or_else(|e| {
            self.inner.log(LogLevel::Error, format!("could not read the queue order: {e}"));
            Vec::new()
        })
    }

    /// Moves a pending job within the queue order. Moving past either end does nothing.
    pub fn move_job(&self, id: i64, to: JobMove) -> Result<(), ServiceError> {
        let result = (|| {
            let mut order = self.inner.store.queue_order()?;
            let from = order.iter().position(|&j| j == id).ok_or(ServiceError::NotFound(id))?;
            let dest = match to {
                JobMove::Top => 0,
                JobMove::Up => from.saturating_sub(1),
                JobMove::Down => (from + 1).min(order.len() - 1),
                JobMove::Bottom => order.len() - 1,
            };
            if dest == from {
                return Ok(false);
            }
            let job = order.remove(from);
            order.insert(dest, job);
            self.inner.store.set_queue_order(&order)?;
            Ok(true)
        })();
        match result {
            Ok(changed) => {
                if changed {
                    self.inner.emit(ServiceEvent::JobsChanged);
                }
                Ok(())
            }
            Err(e) => {
                self.inner.log(LogLevel::Error, format!("could not move job {id}: {e}"));
                Err(e)
            }
        }
    }

    /// Puts the pending jobs in the order of `ids` (the queue as the user dragged it).
    /// Ids that are not pending are ignored; pending jobs missing from `ids` keep their
    /// relative order after the listed ones, so a list made from a filtered view still
    /// produces a whole order. Nothing happens when the order does not change.
    pub fn reorder_queue(&self, ids: &[i64]) -> Result<(), ServiceError> {
        let result = (|| {
            let current = self.inner.store.queue_order()?;
            let mut seen = HashSet::new();
            let mut order: Vec<i64> =
                ids.iter().copied().filter(|id| current.contains(id) && seen.insert(*id)).collect();
            let rest: Vec<i64> = current.iter().copied().filter(|id| !order.contains(id)).collect();
            order.extend(rest);
            if order == current {
                return Ok(false);
            }
            self.inner.store.set_queue_order(&order)?;
            Ok(true)
        })();
        match result {
            Ok(changed) => {
                if changed {
                    self.inner.emit(ServiceEvent::JobsChanged);
                }
                Ok(())
            }
            Err(e) => {
                self.inner.log(LogLevel::Error, format!("could not reorder the queue: {e}"));
                Err(e)
            }
        }
    }

    /// Removes every finished (`done` or `failed`) job from the list, like
    /// [`Service::remove_job`] (files on disk are kept). Returns how many were removed.
    pub fn clear_history(&self) -> usize {
        let finished: Vec<Job> = self
            .jobs()
            .into_iter()
            .filter(|j| matches!(j.state, JobState::Done | JobState::Failed))
            .collect();
        let mut removed = 0;
        for job in &finished {
            match self.inner.store.remove_job(job.id) {
                Ok(()) => {
                    removed += 1;
                    if let Some(fp) = Fingerprint::of(&job.source_path) {
                        let _ = self.inner.store.mark_source_seen(&job.source_path, fp.size, fp.mtime);
                    }
                }
                Err(StoreError::NotFound) => {}
                Err(e) => self.inner.log(LogLevel::Error, format!("could not remove {}: {e}", job.name)),
            }
        }
        if removed > 0 {
            self.inner.log(
                LogLevel::Info,
                format!("cleared {removed} finished job{} from the list", if removed == 1 { "" } else { "s" }),
            );
            self.inner.emit(ServiceEvent::JobsChanged);
        }
        removed
    }

    /// Removes a finished (`done` or `failed`) job from the list and deletes its download
    /// folder with everything in it. Pending jobs are refused with [`ServiceError::Other`]
    /// (remove them first). When the folder is unsafe to delete (see the module docs) or
    /// the deletion fails, the job stays in the list and the error is returned.
    pub fn delete_job_files(&self, id: i64) -> Result<Deleted, ServiceError> {
        let result = (|| {
            let job = self.inner.store.get_job(id)?.ok_or(ServiceError::NotFound(id))?;
            self.delete_files_of(&job, &self.jobs()).map(|d| (job, d))
        })();
        match result {
            Ok((job, d)) => {
                self.inner.log(
                    LogLevel::Info,
                    format!("deleted {} and its files ({} file{}, {})", job.name, d.files, plural(d.files), format::bytes(d.bytes)),
                );
                self.inner.emit(ServiceEvent::JobsChanged);
                Ok(d)
            }
            Err(e) => {
                self.inner.log(LogLevel::Error, format!("could not delete the files of job {id}: {e}"));
                Err(e)
            }
        }
    }

    /// [`Service::delete_job_files`] for every finished job. Jobs whose folder cannot be
    /// deleted stay in the list (each failure is logged). Returns the jobs removed and the
    /// total deleted.
    pub fn clear_history_and_files(&self) -> (usize, Deleted) {
        let all = self.jobs();
        let mut removed = 0;
        let mut total = Deleted::default();
        for job in all.iter().filter(|j| matches!(j.state, JobState::Done | JobState::Failed)) {
            match self.delete_files_of(job, &all) {
                Ok(d) => {
                    removed += 1;
                    total.files += d.files;
                    total.bytes += d.bytes;
                }
                Err(e) => self.inner.log(LogLevel::Error, format!("could not delete the files of {}: {e}", job.name)),
            }
        }
        if removed > 0 {
            self.inner.log(
                LogLevel::Info,
                format!(
                    "deleted {removed} finished job{} and their files ({} file{}, {})",
                    plural(removed as u64),
                    total.files,
                    plural(total.files),
                    format::bytes(total.bytes)
                ),
            );
            self.inner.emit(ServiceEvent::JobsChanged);
        }
        (removed, total)
    }

    /// Checks, deletes the folder, then removes the row (so a failed deletion keeps the
    /// job visible) and records the source NZB as seen.
    fn delete_files_of(&self, job: &Job, all: &[Job]) -> Result<Deleted, ServiceError> {
        if !matches!(job.state, JobState::Done | JobState::Failed) || self.inner.lock_state().active_job == Some(job.id) {
            return Err(ServiceError::Other(format!("{} is not finished ({})", job.name, job.state)));
        }
        let settings = self.settings();
        if let Some(why) = files::refuse_reason(job, &settings.download_dir, all) {
            return Err(ServiceError::Other(format!("refusing to delete: {why}")));
        }
        let deleted = files::delete_tree(&job.dest_dir)?;
        match self.inner.store.remove_job(job.id) {
            Ok(()) | Err(StoreError::NotFound) => {}
            Err(e) => return Err(store_err(job.id, e)),
        }
        if let Some(fp) = Fingerprint::of(&job.source_path) {
            let _ = self.inner.store.mark_source_seen(&job.source_path, fp.size, fp.mtime);
        }
        Ok(deleted)
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
            run_scope: if st.running { st.only.clone() } else { None },
            post: st.post.clone(),
        }
    }

    /// Deletes a job (and its file rows). If it is downloading, the download is canceled;
    /// files already written stay on disk. The source NZB is recorded as seen (with its
    /// size and mtime), so a watch folder that still holds it does not import it again.
    pub fn remove_job(&self, id: i64) -> Result<(), ServiceError> {
        let result = (|| {
            let job = self.inner.store.get_job(id)?.ok_or(ServiceError::NotFound(id))?;
            self.inner.store.remove_job(id).map_err(|e| store_err(id, e))?;
            // Remember the NZB, so a watch folder holding it does not import it again.
            if let Some(fp) = Fingerprint::of(&job.source_path) {
                if let Err(e) = self.inner.store.mark_source_seen(&job.source_path, fp.size, fp.mtime) {
                    self.inner.log(
                        LogLevel::Warn,
                        format!("could not record {} as removed: {e}", job.source_path.display()),
                    );
                }
            }
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
            lock(&self.inner.articles).remove(&id);
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
    /// [`ServiceError::Other`]; a retry while another job for the same source NZB is pending
    /// is refused with [`ServiceError::Duplicate`]. Does not start the queue.
    pub fn retry_job(&self, id: i64) -> Result<(), ServiceError> {
        let result = (|| {
            let job = self.inner.store.get_job(id)?.ok_or(ServiceError::NotFound(id))?;
            if job.state != JobState::Failed {
                return Err(ServiceError::Other(format!(
                    "only failed jobs can be retried ({} is {})",
                    job.name, job.state
                )));
            }
            // Retrying must not put a second pending job for the same NZB in the queue.
            if let Some(other) = self.inner.store.pending_job_by_source(&job.source_path)? {
                return Err(ServiceError::Duplicate(other.name));
            }
            self.inner.store.set_job_state(id, JobState::Queued, None).map_err(|e| store_err(id, e))?;
            let _ = self.inner.store.clear_job_outcome(id);
            Ok(job)
        })();
        match result {
            Ok(job) => {
                self.inner.log_job(job.id, LogLevel::Info, format!("requeued {}", job.name));
                self.inner.emit(ServiceEvent::JobsChanged);
                Ok(())
            }
            Err(e) => {
                self.inner.log(LogLevel::Error, format!("could not retry job {id}: {e}"));
                Err(e)
            }
        }
    }

    /// Like [`Service::retry_job`], but the job's next download asks the fail-over servers
    /// first, in priority order, and the primary server only for articles none of them
    /// has. Completed files are kept; files with missing articles are downloaded again.
    /// Refused with [`ServiceError::Other`] when fewer than two servers are usable. The
    /// outcome is logged when that download ends. Does not start the queue.
    pub fn retry_job_on_backup(&self, id: i64) -> Result<(), ServiceError> {
        let Some(order) = self.inner.settings().backup_first_servers() else {
            let e = ServiceError::Other("no fail-over server is configured".into());
            self.inner.log(LogLevel::Error, format!("could not retry job {id} on a backup server: {e}"));
            return Err(e);
        };
        let before = self.inner.store.get_job(id).ok().flatten();
        self.retry_job(id)?;
        let (name, missing) = before.map_or_else(|| (format!("job {id}"), 0), |j| (j.name, j.stats.articles_missing));
        lock(&self.inner.backup_first).insert(id, missing);
        let hosts: Vec<String> = order.iter().map(ServerConfig::address).collect();
        self.inner.log_job(id, LogLevel::Info, format!("{name}: backup retry queued; servers in order {}", hosts.join(" → ")));
        Ok(())
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

    /// Which articles of a job arrived, are missing or still wait: live while the job
    /// downloads, else the map saved when its last run ended. `None` for a job that has
    /// not run since article maps were introduced (or whose saved map is unreadable).
    pub fn article_map(&self, job_id: i64) -> Option<ArticleMap> {
        if let Some(m) = lock(&self.inner.articles).get(&job_id) {
            return Some(m.clone());
        }
        match self.inner.store.article_map(job_id) {
            Ok(text) => text.and_then(|t| ArticleMap::from_stored(&t)),
            Err(e) => {
                self.inner.log(LogLevel::Error, format!("could not read the article map of job {job_id}: {e}"));
                None
            }
        }
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

    /// Why the stored settings document cannot be loaded, or `None` when it loads. While
    /// this is `Some`, [`Service::settings`] returns defaults; saving keeps the unreadable
    /// document as a backup in the database (see [`Store::settings_backups`]).
    pub fn settings_load_error(&self) -> Option<String> {
        self.inner.store.load_settings().err().map(|e| e.to_string())
    }

    /// Replaces (with `Some`) or clears (with `None`) an in-memory settings override.
    /// While set, imports (destination folder), queue runs (servers, connections,
    /// retries, speed limit, post-processing), the watch timer and auto-shutdown use the
    /// override instead of the stored document. The override is normalized, is never
    /// written to the database and is never logged (it may carry server passwords). Used
    /// by the headless CLI for `--server`/`--dest`. Like saved settings, it applies from
    /// the next job, except the speed limit, which applies at once.
    pub fn set_runtime_settings_override(&self, s: Option<Settings>) {
        *lock(&self.inner.settings_override) = s.map(Settings::normalized);
        self.inner.speed_limit.set(self.inner.settings().speed_limit_bps());
    }

    /// The speed limit in force now, in bytes per second (0 = unlimited).
    pub fn speed_limit_bps(&self) -> u64 {
        self.inner.speed_limit.get()
    }

    /// Sets the speed limit (KiB/s, 0 = unlimited) for downloads running now and later,
    /// and saves it in the settings. When the stored settings cannot be read the limit
    /// still applies, for this session only (saving would replace them with defaults).
    pub fn set_speed_limit_kbps(&self, kbps: u64) -> Result<(), ServiceError> {
        let mut s = match self.inner.store.load_settings() {
            Ok(s) => s,
            Err(e) => {
                self.inner.speed_limit.set(kbps.saturating_mul(1024));
                self.inner.log(
                    LogLevel::Warn,
                    format!("speed limit applied for this session only: the settings cannot be read ({e})"),
                );
                return Err(e.into());
            }
        };
        s.speed_limit_kbps = kbps;
        self.save_settings(s)?;
        self.inner.log(
            LogLevel::Info,
            if kbps == 0 {
                "speed limit off".to_owned()
            } else {
                format!("speed limit {}/s", format::bytes(kbps.saturating_mul(1024)))
            },
        );
        Ok(())
    }

    /// `true` while a runtime settings override is set.
    pub fn has_runtime_settings_override(&self) -> bool {
        lock(&self.inner.settings_override).is_some()
    }

    /// Normalizes and persists the settings. They apply to the next job (a running job
    /// keeps its servers and connections), except the speed limit, which running
    /// downloads follow at once.
    /// When the stored document cannot be loaded, it is kept as a backup row first (see
    /// [`Store::save_settings`]) and a warning names the backup key.
    pub fn save_settings(&self, s: Settings) -> Result<(), ServiceError> {
        let was_unreadable = self.inner.store.load_settings().is_err();
        match self.inner.store.save_settings(&s.normalized()) {
            Ok(()) => {
                // Running downloads follow a new limit at once (the rest from the next job).
                self.inner.speed_limit.set(self.inner.settings().speed_limit_bps());
                self.inner.settings_error_logged.store(false, Ordering::Relaxed);
                self.inner.log(LogLevel::Info, "settings saved");
                if was_unreadable {
                    let key = self.inner.store.settings_backups().ok().and_then(|b| b.last().map(|(k, _)| k.clone()));
                    self.inner.log(
                        LogLevel::Warn,
                        format!(
                            "the unreadable settings document was kept in the database as {}",
                            key.as_deref().unwrap_or("a backup")
                        ),
                    );
                }
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

    /// A job's own log (kept across restarts, newest [`newskite_store::JOB_LOG_KEEP`]
    /// lines), oldest first.
    pub fn job_log(&self, job_id: i64) -> Vec<newskite_store::JobLogLine> {
        self.inner.store.job_log(job_id).unwrap_or_default()
    }

    /// Health of every news server used since the service started.
    pub fn server_health(&self) -> Vec<newskite_types::ServerHealth> {
        self.inner.progress.server_health()
    }

    /// Path of the log file in the data directory.
    pub fn log_file_path(&self) -> PathBuf {
        self.inner.opts.data_dir.join(LOG_FILE_NAME)
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
        // Bytes since the last sampler tick would otherwise never be recorded.
        timers::flush_throughput(&self.inner);
        self.inner.pool.close_idle();
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
