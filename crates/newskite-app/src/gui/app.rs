//! Application state, event plumbing and the per-frame layout.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};
use newskite_service::{
    Job, JobFile, JobMove, JobState, LogLevel, LogLine, Service, ServiceEvent, Status,
    ThroughputSample, Totals,
};
use newskite_theme::{Theme, ThemeWatcher};
use newskite_types::{EngineEvent, Settings};
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;

use super::appearance::{self, Appearance};
use super::brand::{self, MarkClock};
use super::events::{Pending, SharedPending};
use super::settings::{FolderTarget, SettingsDraft};
use super::theme::{self, Palette};
use super::{about, disk, docks, fonts, icons, motion, queue, settings, statusbar};

/// How often the job list is re-read while a job is downloading.
pub const JOBS_REFRESH: Duration = Duration::from_millis(1500);
/// How often the article maps of downloading jobs are read again.
const MAPS_REFRESH: Duration = Duration::from_millis(200);
/// Shortest gap between event-driven job reloads: a burst of finished files (each one
/// an event) reads the store once per gap instead of once per frame.
const JOBS_RELOAD_MIN: Duration = Duration::from_millis(150);
/// Live throughput series length (seconds).
pub const LIVE_WINDOW_SECS: usize = 600;
/// Stored history shown in the graph (seconds).
pub const HISTORY_SECS: u64 = 24 * 3600;
/// How often the stored throughput history is re-read (also after each queue run).
pub const HISTORY_REFRESH: Duration = Duration::from_secs(60);
/// Theme watcher poll interval (on its own thread; the window is only woken by a change).
const THEME_POLL: Duration = Duration::from_secs(1);
/// Theme polls between two reads of the reduced-motion preference.
const MOTION_EVERY: u32 = 5;
/// How often an idle window redraws anyway, so relative times ("5 min ago") and the
/// periodic history refresh move on.
const IDLE_TICK: Duration = Duration::from_secs(30);
/// Longest the article maps of jobs that are not on show may hold up one frame; the
/// rest are read on the following frames.
const MAPS_BUDGET: Duration = Duration::from_millis(4);

/// Where the theme comes from.
pub enum ThemeMode {
    /// Follow the desktop (kdeglobals / portal / fallback) and re-theme live.
    Desktop,
    /// A fixed theme (tests, screenshots).
    Fixed(Theme),
}

/// Construction options for [`NewskiteApp`].
pub struct AppConfig {
    /// Theme source.
    pub theme: ThemeMode,
    /// Send a desktop notification when a queue run finishes jobs.
    pub notifications: bool,
    /// Spawn the background thread that forwards service events (off in tests that
    /// feed events by hand).
    pub forward_events: bool,
    /// Where the Appearance choice is stored (`<data dir>/appearance.json`); `None` keeps
    /// it in memory only (tests).
    pub appearance_file: Option<PathBuf>,
    /// Force reduced motion on (`Some(true)`) or off; `None` reads the desktop setting
    /// and `NEWSKITE_REDUCED_MOTION` (see [`motion`]).
    pub reduced_motion: Option<bool>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: ThemeMode::Desktop,
            notifications: true,
            forward_events: true,
            appearance_file: None,
            reduced_motion: None,
        }
    }
}

/// Center-area tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    /// The download queue: queued, downloading and paused jobs (default tab).
    Queue,
    /// Finished jobs: done and failed.
    History,
    /// The message log.
    Log,
}

/// Messages delivered to the UI thread.
#[derive(Debug)]
pub enum UiMsg {
    /// An event from the service.
    Service(ServiceEvent),
    /// The event stream lagged; reload everything.
    Resync,
    /// NZB files chosen in the native file dialog.
    NzbPicked(Vec<PathBuf>),
    /// A folder chosen for a settings field.
    FolderPicked(FolderTarget, PathBuf),
    /// A native dialog closed without a choice.
    DialogClosed,
    /// The picker could not be shown (why).
    DialogFailed(String),
    /// The NZB picker could not be shown (why); the typed-path field opens instead.
    NzbPickerFailed(String),
    /// What a later `newskite` launch handed over (`handoff`): its NZBs are imported and
    /// the window comes forward (an empty list only raises it).
    Launched(Vec<crate::launch::LaunchItem>),
    /// Result of a server "Test".
    TestResult {
        /// Row the test belongs to.
        row_id: u64,
        /// Transcript or error text.
        result: Result<String, String>,
    },
    /// An NZB import (run off the UI thread) finished.
    NzbImported {
        /// The imported file.
        path: PathBuf,
        /// The new job, or why the import failed.
        result: Result<Job, String>,
    },
    /// A manual watch-folder scan (run off the UI thread) finished.
    WatchScanned,
    /// Background work (deleting folders, asking the file manager) finished: jobs and
    /// the log may have changed.
    Refresh,
    /// The stored throughput history and the disk's free and total bytes, read off the
    /// UI thread.
    History {
        /// The last [`HISTORY_SECS`] of samples.
        samples: Vec<ThroughputSample>,
        /// Free and total bytes of the download folder's filesystem.
        disk: Option<(u64, u64)>,
    },
    /// The desktop's theme changed.
    ThemeChanged(Box<Theme>),
    /// The desktop's reduced-motion preference changed.
    MotionChanged(bool),
}

/// Something the user asked for this frame; applied after the UI pass so panels only
/// need shared access to the data.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Import an NZB file.
    AddNzb(PathBuf),
    /// Open the native NZB picker.
    BrowseNzb,
    /// Open a folder picker for a settings field.
    BrowseFolder(FolderTarget),
    /// Start the whole queue.
    Start,
    /// Pause the whole queue now (the current job is interrupted and resumes first).
    Pause,
    /// Resume.
    Resume,
    /// Download one job (resuming it if held), leaving the rest of the queue alone.
    StartJob(i64),
    /// Hold one job (interrupting it if it is downloading).
    PauseJob(i64),
    /// Move a job within the queue.
    MoveJob(i64, JobMove),
    /// Put the pending jobs in this order (a row dragged to a new place).
    ReorderQueue(Vec<i64>),
    /// Remove every finished job from the list.
    ClearHistory,
    /// Remove every finished job and delete its download folder.
    ClearHistoryAndFiles,
    /// Remove a finished job and delete its download folder.
    DeleteJobFiles(i64),
    /// Set the speed limit in KiB/s (0 = unlimited): saved, and in force at once.
    SetSpeedLimit(u64),
    /// Open a folder (or a file's folder) in the file manager.
    OpenPath(PathBuf),
    /// Show a file selected in its folder in the file manager (the folder alone when the
    /// file is gone), without opening the file itself.
    RevealPath(PathBuf),
    /// Open the application log file.
    OpenLogFile,
    /// Start editing a job's archive password (the row's inline editor).
    EditPassword(i64),
    /// Dismiss a toast.
    DismissToast(u64),
    /// Select a job and switch to the tab that lists it.
    ShowJob(i64),
    /// Open a job's download folder in the file manager.
    OpenJobFolder(i64),
    /// Select a job (its details show in the Details dock) or clear the selection.
    Select(Option<i64>),
    /// Scan the watch folder now.
    ScanWatch,
    /// Remove a job.
    RemoveJob(i64),
    /// Failed → queued, and download it.
    RetryJob(i64),
    /// Failed → queued with the fail-over servers asked first, and download it.
    RetryOnBackup(i64),
    /// Every failed job → queued with the fail-over servers asked first, and download them.
    RetryAllOnBackup,
    /// Set or clear a job's archive password.
    SetJobPassword(i64, Option<String>),
    /// Show the settings modal.
    OpenSettings,
    /// Persist the settings draft.
    SaveSettings(Box<Settings>),
    /// Close the settings modal without saving (a sheet with unsaved changes asks first).
    CloseSettings,
    /// Close the settings modal and drop its unsaved changes.
    DiscardSettings,
    /// Test a server from the settings modal.
    TestServer(u64, Box<newskite_types::ServerConfig>),
    /// Open the download folder in the file manager.
    OpenDownloadFolder,
    /// Cancel the auto-shutdown countdown.
    CancelShutdown,
    /// Switch tab.
    SetTab(Tab),
    /// Reveal (and focus) the Open NZB field.
    ShowOpenNzb,
    /// Show the About dialog.
    OpenAbout,
    /// Close the About dialog.
    CloseAbout,
    /// Open a web page in the browser (the innerpulse credit).
    OpenUrl(String),
    /// Wear another theme (Settings › Appearance) and remember the choice.
    SetAppearance(Appearance),
}

/// Everything the panels display, refreshed from the service.
#[derive(Default)]
pub struct Data {
    /// Jobs, id ascending.
    pub jobs: Vec<Job>,
    /// Totals for the account dock.
    pub totals: Totals,
    /// Pending job ids in queue order (the order a run downloads them).
    pub queue_order: Vec<i64>,
    /// File rows of the selected job.
    pub selected_files: Vec<JobFile>,
    /// The selected job's own log, oldest first.
    pub selected_log: Vec<newskite_service::JobLogLine>,
    /// Health of every server used this session.
    pub servers: Vec<newskite_service::ServerHealth>,
    /// Finish announcements, oldest first.
    pub toasts: Vec<super::toast::Toast>,
    /// Free bytes on the download folder's filesystem, when it can be read.
    pub disk_free: Option<u64>,
    /// Log lines, newest first.
    pub logs: Vec<LogLine>,
    /// Current saved settings.
    pub settings: Settings,
    /// Queue status (re-read each frame).
    pub status: Status,
    /// Stored throughput history loaded at start (set with [`Data::set_history`]).
    pub history: Vec<ThroughputSample>,
    /// The fastest stored second in [`Self::history`].
    pub history_peak: u64,
    /// [`Self::history`] in the sparkline's cells (see [`statusbar`]), as of its last read.
    pub history_cells: Vec<f32>,
    /// Live throughput series `(unix secs, bytes/s)`, oldest first.
    pub live: VecDeque<(i64, u64)>,
    /// Smoothed `bytes_done` of downloading jobs (DB value + live segment bytes).
    pub live_done: HashMap<i64, u64>,
    /// Seconds left on the auto-shutdown countdown, if one is running.
    pub shutdown_secs: Option<u32>,
    /// Articles (segments) fetched and decoded this session (`SegmentDone` events).
    pub articles_fetched: u64,
    /// Articles reported missing this session (from `FileFailed` events).
    pub articles_missing: u64,
    /// Why the stored settings document could not be read, if it could not.
    pub settings_load_error: Option<String>,
    /// Article maps of the jobs on show (the downloading ones and the selection).
    pub article_maps: HashMap<i64, newskite_service::ArticleMap>,
    /// Repair outlooks read from the service with the article maps (live for downloading
    /// jobs); finished jobs also carry theirs in their outcome (see [`Data::outlook`]).
    pub outlooks: HashMap<i64, newskite_service::RepairOutlook>,
    /// The job the Articles dock shows: the one downloading, else the selection.
    pub activity_job: Option<i64>,
    /// Every job's article map folded into strip buckets (see `articles::STRIP_BUCKETS`),
    /// for the rows' strips; jobs without a map have none.
    pub strips: HashMap<i64, Vec<newskite_service::ArticleCounts>>,
    /// Size of the download folder's filesystem, when it can be read.
    pub disk_total: Option<u64>,
}

impl Data {
    /// Replaces the stored throughput history and what is derived from it.
    pub fn set_history(&mut self, samples: Vec<ThroughputSample>) {
        self.history_peak = samples.iter().map(|s| s.bytes_down).max().unwrap_or(0);
        let raw: Vec<(i64, u64)> = samples.iter().map(|s| (s.ts, s.bytes_down)).collect();
        self.history_cells =
            if raw.is_empty() { Vec::new() } else { docks::history_buckets(&raw, now_secs(), HISTORY_SECS as i64, statusbar::SPARK_CELLS) };
        self.history = samples;
    }

    /// How many jobs are finished (done or failed).
    pub fn finished_count(&self) -> usize {
        self.jobs.iter().filter(|j| !j.state.is_pending()).count()
    }

    /// Whether any job is queued, downloading or held.
    pub fn has_pending(&self) -> bool {
        self.jobs.iter().any(|j| j.state.is_pending())
    }

    /// The repair outlook to show for `job`: as last read from the service, else as saved
    /// with its outcome.
    pub fn outlook(&self, job: &Job) -> Option<newskite_service::RepairOutlook> {
        self.outlooks.get(&job.id).copied().or_else(|| job.stats.outcome.as_ref().and_then(|o| o.repair_outlook))
    }

    /// `bytes_done` to display for `job`: never above the total, never NaN-producing.
    pub fn shown_done(&self, job: &Job) -> u64 {
        let base = if job.state == JobState::Downloading {
            self.live_done
                .get(&job.id)
                .copied()
                .unwrap_or(job.bytes_done)
                .max(job.bytes_done)
        } else {
            job.bytes_done
        };
        if job.bytes_total > 0 {
            base.min(job.bytes_total)
        } else {
            base
        }
    }

    /// Pending jobs in queue order (jobs missing from the stored order go last, by id).
    pub fn queue(&self) -> Vec<&Job> {
        let mut v: Vec<&Job> = self.jobs.iter().filter(|j| j.state.is_pending()).collect();
        // One rank lookup per job, not a scan of the order per comparison.
        let rank: HashMap<i64, usize> = self.queue_order.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        v.sort_by_cached_key(|j| (rank.get(&j.id).copied().unwrap_or(usize::MAX), j.id));
        v
    }

    /// Finished jobs (done and failed), most recently finished first (jobs finished
    /// before finish times were recorded go by id).
    pub fn history(&self) -> Vec<&Job> {
        let mut v: Vec<&Job> = self.jobs.iter().filter(|j| !j.state.is_pending()).collect();
        v.sort_by(|a, b| (b.stats.finished_at, b.id).cmp(&(a.stats.finished_at, a.id)));
        v
    }

    /// Bytes still to download over every queued, downloading and paused job.
    pub fn bytes_left(&self) -> u64 {
        self.jobs
            .iter()
            .filter(|j| j.state.is_pending())
            .map(|j| j.bytes_total.saturating_sub(self.shown_done(j)))
            .fold(0u64, u64::saturating_add)
    }

    /// Whether the active run will download `job` (a run of the whole queue takes every
    /// queued job; a run started for single jobs only those).
    pub fn in_run(&self, job: &Job) -> bool {
        self.status.running
            && match &self.status.run_scope {
                None => true,
                Some(ids) => ids.contains(&job.id),
            }
    }

    /// Current speed: the latest live sample, else the service's figure.
    pub fn speed(&self) -> u64 {
        self.live
            .back()
            .map(|&(_, b)| b)
            .unwrap_or(self.status.speed_bps)
    }
}

/// Transient UI state (not persisted).
pub struct ViewState {
    /// Selected center tab.
    pub tab: Tab,
    /// The name filter above the job tables.
    pub filter: String,
    /// Open NZB inline field.
    pub open_nzb: OpenNzbState,
    /// Bottom dock state.
    pub docks: docks::DockState,
    /// Server strip state.
    pub strip: super::serverstrip::StripState,
    /// Queue tab state.
    pub queue: queue::QueueState,
    /// Inspector (selected job) state.
    pub inspector: super::inspector::InspectorState,
    /// Which queued jobs the list shows.
    pub list_filter: super::center::ListFilter,
    /// The job the inspector last showed, so it can slide away with its content.
    pub inspector_job: Option<Job>,
    /// Settings modal (open when `Some`).
    pub settings: Option<SettingsDraft>,
    /// A native dialog is open (prevents stacking dialogs).
    pub dialog_open: bool,
    /// The About dialog is open.
    pub about_open: bool,
    /// The masthead mark's bar heights this frame.
    pub masthead_mark: [f64; 6],
    /// The About mark's bar heights this frame.
    pub about_mark: [f64; 6],
    /// Reduced motion is in effect this frame (copied from the app so views can rest).
    pub reduced_motion: bool,
    /// A manual watch-folder scan is running.
    pub scanning: bool,
}

/// The toolbar's inline Open NZB field.
#[derive(Default)]
pub struct OpenNzbState {
    /// Field visible.
    pub visible: bool,
    /// Path text.
    pub path: String,
    /// Request focus next frame.
    pub focus: bool,
    /// Last import error (shown inline).
    pub error: Option<String>,
    /// Imports still running in the background.
    pub importing: usize,
    /// The path field had the keyboard when last drawn.
    pub has_focus: bool,
}

/// The Newskite main window.
pub struct NewskiteApp {
    pub(crate) service: Service,
    rt: Handle,
    ctx: egui::Context,
    rx: mpsc::Receiver<UiMsg>,
    tx: mpsc::Sender<UiMsg>,
    /// Dropped with the window: the theme thread stops when it is gone.
    _alive: Arc<()>,
    /// The desktop's theme (or the fixed one), before the Appearance choice.
    desktop_theme: Theme,
    /// Settings › Appearance.
    pub appearance: Appearance,
    appearance_file: Option<PathBuf>,
    /// Reduced motion is in effect (the mark rests): the desktop asks for it, or the
    /// user does in Settings › Appearance.
    pub reduced_motion: bool,
    /// What the desktop (or the launch configuration) asks for.
    desktop_reduced_motion: bool,
    /// Settings › Appearance › Reduce motion.
    pub reduce_motion_pref: bool,
    masthead_clock: MarkClock,
    about_clock: MarkClock,
    /// The theme being worn (the desktop's, or an innerpulse theme).
    pub theme: Theme,
    /// Colors derived from [`Self::theme`].
    pub palette: Palette,
    /// Displayed data.
    pub data: Data,
    /// Transient UI state.
    pub view: ViewState,
    actions: Vec<Action>,
    last_jobs_reload: Instant,
    /// Events asked for a job reload that was deferred (see [`JOBS_RELOAD_MIN`]).
    jobs_dirty: bool,
    /// When the article maps of downloading jobs were last read.
    last_maps_reload: Instant,
    /// Jobs whose saved article map may be new (they just finished).
    maps_stale: HashSet<i64>,
    /// Jobs the service has no article map for (not asked again until a run ends).
    no_map: HashSet<i64>,
    /// The job whose log `Data::selected_log` holds.
    selected_log_for: Option<i64>,
    /// A log line arrived since that log was read: read it again on the next reload
    /// (progress reloads alone leave the 500-line log as it is).
    selected_log_dirty: bool,
    notifications: bool,
    /// Jobs already finished when last notified (so only new completions notify).
    finished_seen: HashSet<i64>,
    /// The body of the last completion notification (also kept when notifications are
    /// off, so the behavior can be checked).
    pub last_notice: Option<String>,
    /// Id of the newest toast.
    toast_seq: u64,
    /// Frequent events folded by the forwarder thread (see [`super::events`]).
    pending: SharedPending,
    last_history_reload: Instant,
    /// A background history read is on its way.
    history_loading: bool,
    /// The window title as last set.
    title: String,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl NewskiteApp {
    /// Creates the app, applies the theme and (optionally) starts the event forwarder.
    pub fn new(ctx: egui::Context, service: Service, rt: Handle, config: AppConfig) -> Self {
        // Unbounded, but only rare events travel here: the frequent ones are folded into
        // `pending` by the forwarder, which keeps memory bounded while the UI is away.
        let (tx, rx) = mpsc::channel();
        let pending: SharedPending = Arc::new(Mutex::new(Pending::default()));
        let alive = Arc::new(());
        let (watcher, desktop_theme) = match config.theme {
            ThemeMode::Desktop => {
                let hint = ctx.system_theme().map(|t| t == egui::Theme::Dark);
                let watcher = ThemeWatcher::with_hint(hint);
                let theme = watcher.current().clone();
                (Some(watcher), theme)
            }
            ThemeMode::Fixed(t) => (None, t),
        };
        let appearance = config
            .appearance_file
            .as_deref()
            .map(appearance::load)
            .unwrap_or_default();
        let theme = appearance.resolve(&desktop_theme);
        fonts::install(&ctx);
        icons::install(&ctx);
        let palette = theme::apply(&ctx, &theme);
        let desktop_reduced_motion = config.reduced_motion.unwrap_or_else(motion::reduced_motion);
        let reduce_motion_pref = config.appearance_file.as_deref().is_some_and(appearance::load_reduce_motion);
        let reduced_motion = desktop_reduced_motion || reduce_motion_pref;

        if config.forward_events {
            spawn_forwarder(&service, tx.clone(), pending.clone(), ctx.clone());
        }
        // The desktop's theme and motion preference are watched off the UI thread, so an
        // idle window is not redrawn just to poll them.
        let watch_motion = config.reduced_motion.is_none();
        if watcher.is_some() || (watch_motion && config.forward_events) {
            spawn_desktop_watch(watcher, watch_motion.then_some(desktop_reduced_motion), tx.clone(), ctx.clone(), Arc::downgrade(&alive));
        }

        let mut app = Self {
            service,
            rt,
            ctx,
            rx,
            tx,
            _alive: alive,
            desktop_theme,
            appearance,
            appearance_file: config.appearance_file,
            reduced_motion,
            desktop_reduced_motion,
            reduce_motion_pref,
            masthead_clock: MarkClock::new(),
            about_clock: MarkClock::new(),
            theme,
            palette,
            data: Data::default(),
            view: ViewState {
                tab: Tab::Queue,
                filter: String::new(),
                open_nzb: OpenNzbState::default(),
                docks: docks::DockState::default(),
                strip: Default::default(),
                queue: queue::QueueState::default(),
                inspector: Default::default(),
                list_filter: Default::default(),
                inspector_job: None,
                settings: None,
                dialog_open: false,
                about_open: false,
                masthead_mark: brand::rest_heights(),
                about_mark: brand::rest_heights(),
                reduced_motion,
                scanning: false,
            },
            actions: Vec::new(),
            last_jobs_reload: Instant::now(),
            jobs_dirty: false,
            last_maps_reload: Instant::now(),
            maps_stale: HashSet::new(),
            no_map: HashSet::new(),
            selected_log_for: None,
            selected_log_dirty: true,
            notifications: config.notifications,
            finished_seen: HashSet::new(),
            last_notice: None,
            toast_seq: 0,
            pending,
            last_history_reload: Instant::now(),
            history_loading: false,
            title: newskite_types::APP_NAME.to_owned(),
        };
        app.reload_all();
        app.finished_seen = app.finished_ids();
        app
    }

    /// The service this window drives.
    pub fn service(&self) -> &Service {
        &self.service
    }

    /// Queues an action as if the user had triggered it (used by tests and `main`).
    pub fn push_action(&mut self, a: Action) {
        self.actions.push(a);
    }

    /// Sends a message to the UI thread's queue (used by tests).
    pub fn sender(&self) -> mpsc::Sender<UiMsg> {
        self.tx.clone()
    }

    fn finished_ids(&self) -> HashSet<i64> {
        self.data
            .jobs
            .iter()
            .filter(|j| matches!(j.state, JobState::Done | JobState::Failed))
            .map(|j| j.id)
            .collect()
    }

    /// Re-reads everything from the service (start-up and after a lagged stream).
    pub fn reload_all(&mut self) {
        // Settings first: reading them logs an unreadable document once, and the log
        // read after it then includes that line.
        self.data.settings = self.service.settings();
        self.data.settings_load_error = self.service.settings_load_error();
        self.data.logs = self.service.logs();
        self.data.status = self.service.status();
        self.selected_log_dirty = true;
        self.reload_jobs();
        // Finishes may have been missed: every saved article map is read again.
        self.maps_stale.extend(self.data.jobs.iter().map(|j| j.id));
        self.reload_history();
    }

    /// Re-reads the stored throughput history (the last [`HISTORY_SECS`]) and the disk's
    /// space, on the calling thread (start-up and tests; see [`Self::refresh_history`]).
    pub fn reload_history(&mut self) {
        let samples = self.service.throughput_history(HISTORY_SECS);
        self.data.set_history(samples);
        self.set_disk(disk::space(&self.data.settings.download_dir));
        self.last_history_reload = Instant::now();
    }

    fn set_disk(&mut self, space: Option<(u64, u64)>) {
        self.data.disk_free = space.map(|(free, _)| free);
        self.data.disk_total = space.map(|(_, total)| total);
    }

    /// [`Self::reload_history`] off the UI thread (a day of samples is a large read, and
    /// the download folder may be on a slow mount); the result arrives as
    /// [`UiMsg::History`].
    fn refresh_history(&mut self) {
        self.last_history_reload = Instant::now();
        if self.history_loading {
            return;
        }
        self.history_loading = true;
        let svc = self.service.clone();
        let dir = self.data.settings.download_dir.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn_blocking(move || {
            let samples = svc.throughput_history(HISTORY_SECS);
            let _ = tx.send(UiMsg::History { samples, disk: disk::space(&dir) });
            ctx.request_repaint();
        });
    }

    /// Runs `work` on a blocking worker (deleting folders, asking the file manager: both
    /// can take seconds), then has the UI re-read jobs and the log.
    fn in_background(&self, work: impl FnOnce(&Service) + Send + 'static) {
        let svc = self.service.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn_blocking(move || {
            work(&svc);
            let _ = tx.send(UiMsg::Refresh);
            ctx.request_repaint();
        });
    }

    /// Requeues `ids` with the fail-over servers asked first and downloads them. The
    /// service logs each step and, when a download ends, how the backup retry went.
    fn retry_on_backup(&mut self, ids: &[i64]) {
        let mut started = false;
        for &id in ids {
            if self.service.retry_job_on_backup(id).is_ok() && self.service.start_job(id).is_ok() {
                // Its next completion is news again.
                self.finished_seen.remove(&id);
                started = true;
            }
        }
        if started {
            self.view.tab = Tab::Queue;
        }
        self.reload_jobs();
        self.data.logs = self.service.logs();
    }

    /// Keeps the article maps the window draws: every job's strip, and the full map of
    /// the downloading jobs and the selection. Downloading jobs are read every
    /// [`MAPS_REFRESH`] (their maps change constantly); the others once, and again when
    /// they finish. Jobs without a map are remembered, so they are not asked for every
    /// frame. Maps that are not on show are read for at most [`MAPS_BUDGET`] per frame, so
    /// a long history never holds up the window.
    fn refresh_article_maps(&mut self) {
        let downloading: Vec<i64> =
            self.data.jobs.iter().filter(|j| j.state == JobState::Downloading).map(|j| j.id).collect();
        let selected = self.view.queue.selected;
        self.data.activity_job = downloading.first().copied().or(selected);
        let full: HashSet<i64> = downloading.iter().copied().chain(selected).collect();
        let ids: HashSet<i64> = self.data.jobs.iter().map(|j| j.id).collect();
        self.data.article_maps.retain(|id, _| full.contains(id));
        self.data.outlooks.retain(|id, _| full.contains(id));
        self.data.strips.retain(|id, _| ids.contains(id));
        self.maps_stale.retain(|id| ids.contains(id));
        self.no_map.retain(|id| ids.contains(id) && !self.maps_stale.contains(id));
        let live_due = !downloading.is_empty() && self.last_maps_reload.elapsed() >= MAPS_REFRESH;
        let started = Instant::now();
        let mut more = false;
        // The jobs on show first (never deferred), then the rest in list order.
        let order = self.data.jobs.iter().map(|j| j.id).filter(|id| full.contains(id));
        let rest = self.data.jobs.iter().map(|j| j.id).filter(|id| !full.contains(id));
        let order: Vec<i64> = order.chain(rest).collect();
        for id in order {
            let live = downloading.contains(&id);
            if self.no_map.contains(&id) && !live {
                continue;
            }
            let on_show = full.contains(&id);
            let missing = !self.data.strips.contains_key(&id) || (on_show && !self.data.article_maps.contains_key(&id));
            let due = self.maps_stale.contains(&id) || missing || (live_due && live);
            if !due {
                continue;
            }
            if !on_show && started.elapsed() >= MAPS_BUDGET {
                more = true;
                break;
            }
            self.maps_stale.remove(&id);
            match self.service.article_map(id) {
                Some(m) => {
                    self.data.strips.insert(id, m.cells(super::articles::STRIP_BUCKETS).1);
                    if on_show {
                        self.data.article_maps.insert(id, m);
                        // None keeps what `Repair` events delivered.
                        if let Some(o) = self.service.repair_outlook(id) {
                            self.data.outlooks.insert(id, o);
                        }
                    }
                    self.no_map.remove(&id);
                }
                None => {
                    self.data.strips.remove(&id);
                    self.data.article_maps.remove(&id);
                    self.no_map.insert(id);
                }
            }
        }
        if live_due {
            self.last_maps_reload = Instant::now();
        }
        if more {
            self.ctx.request_repaint();
        } else if !downloading.is_empty() {
            self.ctx.request_repaint_after(MAPS_REFRESH);
        }
    }

    /// Re-reads jobs and totals.
    pub fn reload_jobs(&mut self) {
        self.data.jobs = self.service.jobs();
        self.data.totals = self.service.totals();
        self.data.queue_order = self.service.queue_order();
        // A selection whose job is gone is dropped.
        if let Some(id) = self.view.queue.selected {
            if !self.data.jobs.iter().any(|j| j.id == id) {
                self.view.queue.selected = None;
            }
        }
        self.data.selected_files = match self.view.queue.selected {
            Some(id) => self.service.job_files(id),
            None => Vec::new(),
        };
        self.reload_selected_log();
        self.data.servers = self.service.server_health();
        let downloading: HashSet<i64> = self
            .data
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Downloading)
            .map(|j| j.id)
            .collect();
        self.data.live_done.retain(|id, _| downloading.contains(id));
        for j in self
            .data
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Downloading)
        {
            let e = self.data.live_done.entry(j.id).or_insert(j.bytes_done);
            *e = (*e).max(j.bytes_done);
        }
        // A job that is no longer finished (retried) notifies again when it finishes.
        forget_unfinished(&mut self.finished_seen, &self.data.jobs);
        self.last_jobs_reload = Instant::now();
    }

    /// Re-reads the selected job's log when the selection moved or a line was logged
    /// since; otherwise the copy on hand is current.
    fn reload_selected_log(&mut self) {
        let selected = self.view.queue.selected;
        if !self.selected_log_dirty && self.selected_log_for == selected {
            return;
        }
        self.data.selected_log = match selected {
            Some(id) => self.service.job_log(id),
            None => Vec::new(),
        };
        self.selected_log_for = selected;
        self.selected_log_dirty = false;
    }

    /// Drains pending UI messages.
    pub fn drain_messages(&mut self) {
        let mut reload_jobs = false;
        let mut reload_logs = false;
        let (segments, throughput) = self
            .pending
            .lock()
            .map(|mut p| p.take())
            .unwrap_or_default();
        for (job_id, t) in segments {
            let e = self.data.live_done.entry(job_id).or_insert(0);
            *e = e.saturating_add(t.bytes);
            self.data.articles_fetched = self.data.articles_fetched.saturating_add(t.articles);
        }
        for (ts, bytes_per_sec) in throughput {
            self.push_live(ts, bytes_per_sec);
        }
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                UiMsg::Service(ev) => self.on_event(ev, &mut reload_jobs, &mut reload_logs),
                UiMsg::Resync => {
                    self.reload_all();
                }
                UiMsg::NzbPicked(paths) => {
                    self.view.dialog_open = false;
                    for p in paths {
                        self.view.open_nzb.path = p.display().to_string();
                        self.actions.push(Action::AddNzb(p));
                    }
                }
                UiMsg::FolderPicked(target, path) => {
                    self.view.dialog_open = false;
                    if let Some(d) = self.view.settings.as_mut() {
                        d.set_folder(target, path);
                    }
                }
                UiMsg::DialogClosed => self.view.dialog_open = false,
                UiMsg::DialogFailed(why) => {
                    self.view.dialog_open = false;
                    self.service.log(LogLevel::Error, why);
                    self.data.logs = self.service.logs();
                }
                UiMsg::NzbPickerFailed(why) => {
                    self.view.dialog_open = false;
                    self.service.log(LogLevel::Error, why.clone());
                    self.data.logs = self.service.logs();
                    self.view.open_nzb.visible = true;
                    self.view.open_nzb.focus = true;
                    self.view.open_nzb.error = Some(format!("{why}. Type or paste the NZB's path instead."));
                }
                UiMsg::Launched(items) => {
                    // As with drops: what could not be opened is logged, and said under the
                    // header too when nothing else was handed over.
                    let any_nzb = items.iter().any(|i| matches!(i, crate::launch::LaunchItem::Nzb(_)));
                    for item in items {
                        match item {
                            crate::launch::LaunchItem::Nzb(p) => self.actions.push(Action::AddNzb(p)),
                            crate::launch::LaunchItem::Unsupported { arg, reason } => {
                                let why = format!("cannot open {arg}: {reason}");
                                self.service.log(LogLevel::Warn, why.clone());
                                if !any_nzb {
                                    self.view.open_nzb.visible = true;
                                    self.view.open_nzb.error = Some(why);
                                }
                                reload_logs = true;
                            }
                        }
                    }
                    // The user just asked for this window.
                    self.ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                UiMsg::TestResult { row_id, result } => {
                    if let Some(d) = self.view.settings.as_mut() {
                        d.set_test_result(row_id, result);
                    }
                }
                UiMsg::NzbImported { path, result } => {
                    self.view.open_nzb.importing = self.view.open_nzb.importing.saturating_sub(1);
                    match result {
                        Ok(job) => {
                            self.view.open_nzb.error = None;
                            if self.view.open_nzb.path.trim() == path.display().to_string() {
                                self.view.open_nzb.path.clear();
                            }
                            self.view.tab = Tab::Queue;
                            self.view.queue.highlight = Some(job.id);
                            self.view.queue.selected = Some(job.id);
                            reload_jobs = true;
                        }
                        Err(e) => {
                            // The service already logged the failure; show it inline too.
                            self.view.open_nzb.visible = true;
                            self.view.open_nzb.error = Some(e);
                            reload_logs = true;
                        }
                    }
                }
                UiMsg::WatchScanned => {
                    self.view.scanning = false;
                    reload_jobs = true;
                    reload_logs = true;
                }
                UiMsg::Refresh => {
                    reload_jobs = true;
                    reload_logs = true;
                }
                UiMsg::History { samples, disk } => {
                    self.history_loading = false;
                    self.data.set_history(samples);
                    self.set_disk(disk);
                }
                UiMsg::ThemeChanged(t) => {
                    tracing::info!(source = ?t.source, dark = t.dark, "desktop theme changed");
                    self.desktop_theme = *t;
                    self.retheme();
                }
                UiMsg::MotionChanged(reduced) => {
                    self.desktop_reduced_motion = reduced;
                    self.reduced_motion = reduced || self.reduce_motion_pref;
                }
            }
        }
        if reload_jobs || self.jobs_dirty {
            let since = self.last_jobs_reload.elapsed();
            if since >= JOBS_RELOAD_MIN {
                self.reload_jobs();
                self.jobs_dirty = false;
            } else {
                self.jobs_dirty = true;
                self.ctx.request_repaint_after(JOBS_RELOAD_MIN - since);
            }
        }
        if reload_logs {
            self.data.logs = self.service.logs();
        }
    }

    fn on_event(&mut self, ev: ServiceEvent, reload_jobs: &mut bool, reload_logs: &mut bool) {
        match ev {
            ServiceEvent::Engine { job_id, event } => match event {
                EngineEvent::SegmentDone { bytes, .. } => {
                    let e = self.data.live_done.entry(job_id).or_insert(0);
                    *e = e.saturating_add(bytes);
                    self.data.articles_fetched = self.data.articles_fetched.saturating_add(1);
                }
                EngineEvent::FileFailed { missing, .. } => {
                    self.data.articles_missing = self
                        .data
                        .articles_missing
                        .saturating_add(missing.len() as u64);
                    *reload_jobs = true
                }
                EngineEvent::FileDone { .. } => *reload_jobs = true,
                EngineEvent::Repair(o) => {
                    self.data.outlooks.insert(job_id, o);
                }
                // Article maps are read from the service on their own cadence.
                EngineEvent::Layout { .. } | EngineEvent::SegmentMissing { .. } | EngineEvent::FileComplete { .. } => {}
                EngineEvent::FileStarted { .. } | EngineEvent::Log(_) | EngineEvent::Warn(_) => {}
            },
            ServiceEvent::JobLog(_) | ServiceEvent::Error(_) => {
                self.selected_log_dirty = true;
                *reload_logs = true;
            }
            ServiceEvent::JobsChanged | ServiceEvent::WatchImported(_) => *reload_jobs = true,
            ServiceEvent::QueueIdle => {
                *reload_jobs = true;
                self.reload_jobs();
                self.notify_finished();
                self.refresh_history();
            }
            ServiceEvent::Paused | ServiceEvent::Resumed => {}
            // Normally folded (and stamped on arrival) by the forwarder; a sample sent
            // straight to the channel is stamped now.
            ServiceEvent::Throughput { bytes_per_sec } => self.push_live(now_secs(), bytes_per_sec),
            ServiceEvent::ShutdownCountdown { secs_left } => {
                self.data.shutdown_secs = Some(secs_left)
            }
            ServiceEvent::ShutdownCanceled => self.data.shutdown_secs = None,
            ServiceEvent::PostProgress(p) => self.data.status.post = Some(p),
            ServiceEvent::JobFinished { job_id, name, ok, detail } => {
                self.maps_stale.insert(job_id);
                self.toast_seq += 1;
                self.data.toasts.push(super::toast::Toast {
                    id: self.toast_seq,
                    job_id,
                    name,
                    ok,
                    detail,
                    shown_at: None,
                });
                *reload_jobs = true;
            }
        }
    }

    /// Appends a live throughput sample (oldest dropped past the window).
    fn push_live(&mut self, ts: i64, bytes_per_sec: u64) {
        self.data.live.push_back((ts, bytes_per_sec));
        while self.data.live.len() > LIVE_WINDOW_SECS {
            self.data.live.pop_front();
        }
    }

    /// Desktop notification for jobs that finished since the last one.
    fn notify_finished(&mut self) {
        let now = self.finished_ids();
        let Some(body) = completion_notice(&self.data.jobs, &self.finished_seen) else {
            self.finished_seen = now;
            return;
        };
        self.finished_seen = now;
        self.last_notice = Some(body.clone());
        if self.notifications {
            desktop_notify(body);
        }
    }

    /// Applies queued actions.
    pub fn apply_actions(&mut self) {
        let actions = std::mem::take(&mut self.actions);
        // Imports parse whole NZB files and touch the store: they run off the UI thread,
        // one batch per frame so dropped files are queued in the order given.
        let mut imports = Vec::new();
        for a in actions {
            match a {
                Action::AddNzb(path) => imports.push(path),
                other => self.apply(other),
            }
        }
        if !imports.is_empty() {
            self.import_nzbs(imports);
        }
    }

    /// Imports NZB files on a blocking worker; each result comes back as
    /// [`UiMsg::NzbImported`].
    fn import_nzbs(&mut self, paths: Vec<PathBuf>) {
        self.view.open_nzb.importing += paths.len();
        let svc = self.service.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn_blocking(move || {
            for path in paths {
                let result = svc.add_nzb(&path).map_err(|e| e.to_string());
                if tx.send(UiMsg::NzbImported { path, result }).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
    }

    fn apply(&mut self, a: Action) {
        match a {
            Action::AddNzb(path) => self.import_nzbs(vec![path]),
            Action::BrowseNzb => self.browse_nzb(),
            Action::BrowseFolder(target) => self.browse_folder(target),
            Action::Start => {
                self.service.start();
            }
            Action::Pause => self.service.pause_now(),
            Action::Resume => self.service.resume(),
            Action::StartJob(id) => {
                if self.service.start_job(id).is_ok() {
                    self.finished_seen.remove(&id);
                }
                self.reload_jobs();
                self.data.logs = self.service.logs();
            }
            Action::PauseJob(id) => {
                let _ = self.service.pause_job(id);
                self.reload_jobs();
                self.data.logs = self.service.logs();
            }
            Action::MoveJob(id, to) => {
                let _ = self.service.move_job(id, to);
                self.reload_jobs();
            }
            Action::ReorderQueue(ids) => {
                let _ = self.service.reorder_queue(&ids);
                self.reload_jobs();
            }
            Action::ClearHistory => {
                self.service.clear_history();
                self.reload_jobs();
                self.data.logs = self.service.logs();
            }
            // Deleting folders can take a while (gigabytes, slow disks): off the UI thread.
            Action::ClearHistoryAndFiles => self.in_background(|svc| {
                svc.clear_history_and_files();
            }),
            Action::DeleteJobFiles(id) => {
                if self.view.queue.selected == Some(id) {
                    self.view.queue.selected = None;
                    self.data.selected_files.clear();
                }
                self.in_background(move |svc| {
                    let _ = svc.delete_job_files(id);
                });
            }
            Action::OpenJobFolder(id) => self.open_job_folder(id),
            Action::SetSpeedLimit(kbps) => {
                let _ = self.service.set_speed_limit_kbps(kbps);
                self.data.settings = self.service.settings();
                self.data.logs = self.service.logs();
            }
            Action::OpenPath(p) => self.in_background(move |svc| open_path(svc, &p)),
            Action::RevealPath(p) => self.in_background(move |svc| reveal_path(svc, &p)),
            Action::OpenLogFile => self.in_background(|svc| open_path(svc, &svc.log_file_path())),
            Action::EditPassword(id) => {
                self.view.queue.password_edit = Some((id, String::new()));
                // The inline editor lives in the job's row.
                if let Some(j) = self.data.jobs.iter().find(|j| j.id == id) {
                    self.view.tab = if j.state.is_pending() { Tab::Queue } else { Tab::History };
                }
            }
            Action::DismissToast(id) => self.data.toasts.retain(|t| t.id != id),
            Action::ShowJob(id) => {
                if let Some(j) = self.data.jobs.iter().find(|j| j.id == id) {
                    self.view.tab = if j.state.is_pending() { Tab::Queue } else { Tab::History };
                }
                self.apply(Action::Select(Some(id)));
            }
            Action::Select(id) => {
                if self.view.queue.selected != id {
                    self.view.queue.selected = id;
                    self.data.selected_files = match id {
                        Some(id) => self.service.job_files(id),
                        None => Vec::new(),
                    };
                    self.reload_selected_log();
                    // A failed job opens on its problem, others on the overview.
                    self.view.inspector.tab = super::inspector::InspectorTab::Overview;
                }
            }
            Action::ScanWatch => {
                // Directory reads and NZB parsing (possibly on a slow network mount) run
                // off the UI thread. Errors are logged by the service.
                if !self.view.scanning {
                    self.view.scanning = true;
                    let svc = self.service.clone();
                    let tx = self.tx.clone();
                    let ctx = self.ctx.clone();
                    self.rt.spawn_blocking(move || {
                        let _ = svc.scan_watch_folder();
                        let _ = tx.send(UiMsg::WatchScanned);
                        ctx.request_repaint();
                    });
                }
            }
            Action::RemoveJob(id) => {
                let _ = self.service.remove_job(id);
                self.reload_jobs();
            }
            Action::RetryJob(id) => {
                // Requeue and download it (start_job retries a failed job first).
                if self.service.start_job(id).is_ok() {
                    // Its next completion is news again.
                    self.finished_seen.remove(&id);
                    self.view.tab = Tab::Queue;
                }
                self.reload_jobs();
                self.data.logs = self.service.logs();
            }
            Action::RetryOnBackup(id) => {
                self.retry_on_backup(&[id]);
            }
            Action::RetryAllOnBackup => {
                let ids: Vec<i64> = super::serverstrip::failed_jobs(&self.data).iter().map(|j| j.id).collect();
                self.retry_on_backup(&ids);
            }
            Action::SetJobPassword(id, pw) => {
                let _ = self.service.set_job_password(id, pw);
                self.reload_jobs();
            }
            Action::OpenSettings => {
                if self.view.settings.is_none() {
                    self.view.about_open = false;
                    self.data.settings = self.service.settings();
                    self.data.settings_load_error = self.service.settings_load_error();
                    let mut draft = SettingsDraft::new(self.data.settings.clone());
                    draft.open_with_appearance(self.appearance, self.reduce_motion_pref);
                    draft.load_error = self.data.settings_load_error.clone();
                    self.view.settings = Some(draft);
                }
            }
            Action::SaveSettings(s) => match self.service.save_settings(*s) {
                Ok(()) => {
                    self.data.settings = self.service.settings();
                    self.data.settings_load_error = self.service.settings_load_error();
                    // The sheet's appearance is applied with the save, never before it.
                    if let Some((a, reduce)) = self.view.settings.take().map(|d| (d.appearance, d.reduce_motion)) {
                        self.set_look(a, reduce);
                    }
                    self.data.logs = self.service.logs();
                }
                Err(e) => {
                    if let Some(d) = self.view.settings.as_mut() {
                        d.save_error = Some(e.to_string());
                    }
                    self.service
                        .log(LogLevel::Error, format!("saving settings failed: {e}"));
                }
            },
            // A sheet with unsaved changes asks before it closes.
            Action::CloseSettings => match self.view.settings.as_mut() {
                Some(d) if d.dirty() => d.confirm_discard = true,
                _ => self.view.settings = None,
            },
            Action::DiscardSettings => self.view.settings = None,
            Action::TestServer(row_id, server) => {
                let svc = self.service.clone();
                let tx = self.tx.clone();
                let ctx = self.ctx.clone();
                self.rt.spawn(async move {
                    let result = svc.test_server(*server).await;
                    let _ = tx.send(UiMsg::TestResult { row_id, result });
                    ctx.request_repaint();
                });
            }
            Action::OpenDownloadFolder => self.open_download_folder(),
            Action::CancelShutdown => {
                self.service.cancel_shutdown();
                self.data.shutdown_secs = None;
            }
            Action::SetTab(t) => self.view.tab = t,
            Action::ShowOpenNzb => {
                self.view.open_nzb.visible = true;
                self.view.open_nzb.focus = true;
            }
            Action::OpenAbout => {
                if self.view.settings.is_none() && !self.view.about_open {
                    self.view.about_open = true;
                    self.about_clock.reset();
                }
            }
            Action::CloseAbout => self.view.about_open = false,
            Action::OpenUrl(url) => self.in_background(move |svc| {
                if let Err(e) = crate::desktop::open_url(&url) {
                    svc.log(LogLevel::Error, format!("cannot open {url} in the browser: {e}"));
                }
            }),
            Action::SetAppearance(a) => self.set_appearance(a),
        }
    }

    /// Wears the theme for `a` and stores the choice (a write failure is logged; the
    /// choice still applies for this session).
    pub fn set_appearance(&mut self, a: Appearance) {
        self.set_look(a, self.reduce_motion_pref);
    }

    /// [`Self::set_appearance`] together with the Reduce motion switch.
    pub fn set_look(&mut self, a: Appearance, reduce_motion: bool) {
        if a != self.appearance || reduce_motion != self.reduce_motion_pref {
            self.appearance = a;
            self.reduce_motion_pref = reduce_motion;
            self.reduced_motion = self.desktop_reduced_motion || reduce_motion;
            self.retheme();
            if let Some(path) = &self.appearance_file {
                if let Err(e) = appearance::save(path, a, reduce_motion) {
                    self.service.log(
                        LogLevel::Error,
                        format!("cannot save the appearance to {}: {e}", path.display()),
                    );
                    self.data.logs = self.service.logs();
                }
            }
        }
    }

    fn retheme(&mut self) {
        let t = self.appearance.resolve(&self.desktop_theme);
        if t != self.theme {
            self.palette = theme::apply(&self.ctx, &t);
            self.theme = t;
        }
    }

    fn browse_nzb(&mut self) {
        if self.view.dialog_open {
            return;
        }
        self.view.dialog_open = true;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn_blocking(move || {
            let picked = crate::desktop::pick(&crate::desktop::Pick {
                title: "Open NZB".into(),
                multiple: true,
                filter: Some(("NZB files".into(), vec!["nzb".into()])),
                ..Default::default()
            });
            let msg = match picked {
                Ok(Some(files)) if !files.is_empty() => UiMsg::NzbPicked(files),
                Ok(_) => UiMsg::DialogClosed,
                Err(e) => UiMsg::NzbPickerFailed(format!("cannot show the file picker: {e}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn browse_folder(&mut self, target: FolderTarget) {
        if self.view.dialog_open {
            return;
        }
        self.view.dialog_open = true;
        let start = self
            .view
            .settings
            .as_ref()
            .map(|d| d.folder(target))
            .filter(|p| p.is_dir());
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn_blocking(move || {
            let picked = crate::desktop::pick(&crate::desktop::Pick {
                title: target.title().into(),
                directory: true,
                start,
                ..Default::default()
            });
            let msg = match picked {
                Ok(Some(mut dirs)) if !dirs.is_empty() => UiMsg::FolderPicked(target, dirs.remove(0)),
                Ok(_) => UiMsg::DialogClosed,
                Err(e) => UiMsg::DialogFailed(format!("cannot show the folder picker: {e}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn open_job_folder(&mut self, id: i64) {
        let Some(dir) = self.data.jobs.iter().find(|j| j.id == id).map(|j| j.dest_dir.clone()) else {
            return;
        };
        self.in_background(move |svc| {
            // Before the first file is written the folder may not exist: show its parent.
            let target = if dir.is_dir() { dir } else { dir.parent().map(PathBuf::from).unwrap_or(dir) };
            if let Err(e) = crate::desktop::show_folder(&target) {
                svc.log(LogLevel::Error, format!("cannot open {} in the file manager: {e}", target.display()));
            }
        });
    }

    fn open_download_folder(&mut self) {
        let dir = self.data.settings.download_dir.clone();
        self.in_background(move |svc| {
            if dir.as_os_str().is_empty() {
                svc.log(LogLevel::Warn, "no download folder configured");
            } else if let Err(e) = std::fs::create_dir_all(&dir) {
                svc.log(LogLevel::Error, format!("cannot create download folder {}: {e}", dir.display()));
            } else if let Err(e) = crate::desktop::show_folder(&dir) {
                svc.log(LogLevel::Error, format!("cannot open {} in the file manager: {e}", dir.display()));
            }
        });
    }

    /// The masthead's working tempo: a beat every 1.2 s while bytes move, every 2.4 s
    /// while a job is active without traffic (connecting, waiting), none otherwise.
    pub fn working_tempo(&self) -> Option<f64> {
        let st = &self.data.status;
        let active = (st.running && !st.paused)
            || self
                .data
                .jobs
                .iter()
                .any(|j| j.state == JobState::Downloading);
        if !active {
            None
        } else if self.data.speed() > 0 {
            Some(brand::PERIOD_BUSY)
        } else {
            Some(brand::PERIOD_CALM)
        }
    }

    /// Advances the marks and schedules the next repaint only while one moves.
    fn animate_marks(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let tempo = self.working_tempo();
        let (h, next) = self.masthead_clock.frame(now, tempo, self.reduced_motion);
        self.view.masthead_mark = h;
        let mut wake = next;
        if self.view.about_open {
            let (h, next) =
                self.about_clock
                    .frame(now, Some(brand::PERIOD_AMBIENT), self.reduced_motion);
            self.view.about_mark = h;
            wake = match (wake, next) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
        if let Some(d) = wake {
            ctx.request_repaint_after(d);
        }
    }

    /// NZB files dropped onto the window, or copied in a file manager and pasted with
    /// Ctrl+V while no text field has the keyboard, are imported like Add NZB. A folder
    /// adds the `.nzb` files directly inside it; anything else is skipped and said so.
    fn dropped_files(&mut self, ctx: &egui::Context) {
        let mut paths: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        let typing = ctx.memory(|m| m.focused().is_some());
        let modal = self.view.settings.is_some() || self.view.about_open;
        let mut refused: Vec<String> = Vec::new();
        if !typing && !modal {
            let pasted: Vec<String> = ctx.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::Paste(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect()
            });
            for line in pasted.iter().flat_map(|t| t.lines()) {
                let line = line.trim();
                // text/uri-list comments start with '#'.
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                match crate::launch::path_from_text(line) {
                    Ok(p) => paths.push(p),
                    // A pasted link Newskite cannot fetch is said, not swallowed; other
                    // text on the clipboard is none of its business.
                    Err(why) if line.contains("://") => refused.push(format!("cannot open {line}: {why}")),
                    Err(_) => {}
                }
            }
        }
        if paths.is_empty() {
            if let Some(why) = refused.into_iter().next() {
                self.service.log(LogLevel::Warn, why.clone());
                self.data.logs = self.service.logs();
                self.view.open_nzb.visible = true;
                self.view.open_nzb.error = Some(why);
            }
            return;
        }
        let (nzbs, skipped) = expand_drop(&paths);
        if !skipped.is_empty() {
            let names: Vec<String> = skipped
                .iter()
                .map(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.display().to_string())
                })
                .collect();
            let text = format!("Not an NZB file, skipped: {}", names.join(", "));
            self.service.log(LogLevel::Warn, text.clone());
            self.data.logs = self.service.logs();
            if nzbs.is_empty() {
                // Nothing was added: say so where the user looks, under the toolbar.
                self.view.open_nzb.visible = true;
                self.view.open_nzb.error = Some(text);
            }
        }
        for p in nzbs {
            self.actions.push(Action::AddNzb(p));
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let open = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
        let prefs = KeyboardShortcut::new(Modifiers::COMMAND, Key::Comma);
        let find = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);
        let save = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
        let tabs = [(Key::Num1, Tab::Queue), (Key::Num2, Tab::History), (Key::Num3, Tab::Log)];
        let modal = self.view.settings.is_some() || self.view.about_open;
        let (o, p, f, s, tab, esc) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&open),
                i.consume_shortcut(&prefs),
                i.consume_shortcut(&find),
                i.consume_shortcut(&save),
                tabs.into_iter().find(|(k, _)| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, *k))).map(|(_, t)| t),
                i.key_pressed(Key::Escape),
            )
        });
        // The window's own shortcuts rest while a dialog is open over it.
        if !modal {
            if o {
                self.actions.push(Action::BrowseNzb);
            }
            if p {
                self.actions.push(Action::OpenSettings);
            }
            if f {
                self.view.queue.focus_filter = true;
            }
            if let Some(t) = tab {
                self.actions.push(Action::SetTab(t));
            }
        } else if s {
            if let Some(d) = self.view.settings.as_mut() {
                d.save_requested = true;
            }
        }
        // Esc closes one thing: the topmost transient state, in this order.
        let typing = ctx.memory(|m| m.focused().is_some()) && ctx.wants_keyboard_input();
        if esc {
            let q = &mut self.view.queue;
            if let Some(d) = self.view.settings.as_mut() {
                // Esc backs out of the discard question before it closes the sheet.
                if d.confirm_discard {
                    d.confirm_discard = false;
                } else {
                    self.actions.push(Action::CloseSettings);
                }
            } else if self.view.about_open {
                self.actions.push(Action::CloseAbout);
            } else if q.drag.is_some() {
                // The list drops the row where it was.
            } else if q.password_edit.is_some() {
                q.password_edit = None;
            } else if q.confirm_remove.is_some() || q.confirm_delete.is_some() {
                q.confirm_remove = None;
                q.confirm_delete = None;
            } else if q.confirm_clear || q.confirm_clear_files {
                q.confirm_clear = false;
                q.confirm_clear_files = false;
            } else if self.view.strip.confirm_retry_all {
                self.view.strip.confirm_retry_all = false;
            } else if self.view.open_nzb.visible && (!typing || self.view.open_nzb.has_focus) {
                self.view.open_nzb.visible = false;
                self.view.open_nzb.error = None;
            } else if typing {
                // The field with the keyboard takes it (a search clears itself).
            } else if q.selected.is_some() {
                self.actions.push(Action::Select(None));
            }
        }
        // Enter answers an inline "Remove?" with yes (deleting files always takes a click).
        if !typing && !modal {
            if let Some(id) = self.view.queue.confirm_remove {
                if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                    self.view.queue.confirm_remove = None;
                    self.actions.push(Action::RemoveJob(id));
                }
            }
        }
        // Keys for the selected job, unless a text field or dialog has the keyboard.
        if let (Some(id), false, false) = (self.view.queue.selected, typing, modal) {
            let Some(job) = self.data.jobs.iter().find(|j| j.id == id) else {
                return;
            };
            let (space, delete, up, down) = ctx.input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::Space),
                    i.consume_key(Modifiers::NONE, Key::Delete) || i.consume_key(Modifiers::COMMAND, Key::Backspace),
                    i.consume_key(Modifiers::ALT, Key::ArrowUp),
                    i.consume_key(Modifiers::ALT, Key::ArrowDown),
                )
            });
            if space {
                if let Some(a) = queue::toggle_action(&self.data, job) {
                    self.actions.push(a);
                }
            }
            if delete {
                self.view.queue.confirm_remove = Some(id);
            }
            if job.state.is_pending() {
                if up {
                    self.actions.push(Action::MoveJob(id, JobMove::Up));
                }
                if down {
                    self.actions.push(Action::MoveJob(id, JobMove::Down));
                }
            }
        }
        // ↑/↓ and Home/End walk the rows on screen. Alt+↑/↓ (above) were taken first: the
        // plain keys would match them too.
        if !typing && !modal && self.view.tab != Tab::Log {
            let keys = [(Key::ArrowUp, queue::Nav::Up), (Key::ArrowDown, queue::Nav::Down), (Key::Home, queue::Nav::Home), (Key::End, queue::Nav::End)];
            let nav = ctx.input_mut(|i| keys.into_iter().find(|(k, _)| i.consume_key(Modifiers::NONE, *k)).map(|(_, n)| n));
            if let Some(next) = nav.and_then(|n| queue::step_selection(&self.view.queue.visible, self.view.queue.selected, n)) {
                // A row or button that had the keyboard would also walk egui's focus.
                ctx.memory_mut(|m| {
                    if let Some(f) = m.focused() {
                        m.surrender_focus(f);
                    }
                });
                self.actions.push(Action::Select(Some(next)));
                self.view.queue.reveal = true;
            }
        }
    }

    /// One frame: drain events, lay out every panel, apply actions.
    pub fn ui(&mut self, ctx: &egui::Context) {
        if !fonts::ready(ctx) {
            // The brand faces were installed during this frame (headless harnesses create
            // the app mid-frame); draw from the next one.
            fonts::install(ctx);
            ctx.request_repaint();
            return;
        }
        self.drain_messages();
        self.data.status = self.service.status();
        let downloading = self
            .data
            .jobs
            .iter()
            .any(|j| j.state == JobState::Downloading);
        if (self.data.status.running || downloading)
            && self.last_jobs_reload.elapsed() >= JOBS_REFRESH
        {
            self.reload_jobs();
        }
        // The stored "last 24 h" moves on while the window is open.
        if self.last_history_reload.elapsed() >= HISTORY_REFRESH {
            self.refresh_history();
        }
        self.refresh_article_maps();
        let title = window_title(&self.data);
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
        self.shortcuts(ctx);
        self.dropped_files(ctx);
        self.animate_marks(ctx);

        let mut actions = Vec::new();
        let pal = self.palette;
        let width = ctx.content_rect().width();
        self.view.reduced_motion = self.reduced_motion;
        // egui's own transitions (hover washes, sliding panels, toggles) follow the
        // reduced-motion preference too.
        let anim = if self.reduced_motion { 0.0 } else { motion::UI_SECS };
        if ctx.style().animation_time != anim {
            ctx.all_styles_mut(|s| s.animation_time = anim);
        }

        // With materials the whole window is one plate, painted once behind every panel;
        // the panels are cut from it by grooves instead of being filled and outlined.
        let plate = ctx.layer_painter(egui::LayerId::background());
        let edge = |fill: egui::Color32| match pal.metal {
            Some(_) => egui::Frame::new(),
            None => egui::Frame::new().fill(fill).stroke(egui::Stroke::new(1.0, pal.border)),
        };
        if let Some(m) = &pal.metal {
            let screen = ctx.content_rect();
            super::metal::plate(&plate, m, screen);
            // A light along the top edge in the family accent, brighter while bytes move.
            let level = if self.data.status.running { 1.0 } else { 0.45 };
            super::metal::edge_light(&plate, screen, pal.accent, 0.1, level);
        }

        // The sidebar: views and actions on the left (a rail of icons on narrow windows).
        let rail = width < super::sidebar::RAIL_BELOW;
        let side = egui::SidePanel::left("sidebar")
            .resizable(false)
            .exact_width(if rail { super::sidebar::RAIL_W } else { super::sidebar::WIDE_W })
            .frame(edge(pal.raised).inner_margin(egui::Margin { left: 10, right: 10, top: 18, bottom: 12 }))
            .show(ctx, |ui| super::sidebar::show(ui, &pal, &self.data, &self.view, rail, &mut actions))
            .response
            .rect;
        if let Some(m) = &pal.metal {
            super::metal::groove_v(&plate, m, side.right() - 2.0, side.top(), side.bottom());
        }

        let bar = egui::TopBottomPanel::bottom("statusbar")
            .exact_height(30.0)
            .frame(edge(pal.raised).inner_margin(egui::Margin::symmetric(12, 3)))
            .show(ctx, |ui| {
                statusbar::show(ui, &pal, &self.data, rail, self.reduced_motion, &mut actions)
            })
            .response
            .rect;
        if let Some(m) = &pal.metal {
            super::metal::groove_h(&plate, m, bar.left(), bar.right(), bar.top());
        }

        // The server readings: one quiet line above the status bar. Toasts stack from its
        // top edge, so they never cover its actions.
        let strip = egui::TopBottomPanel::bottom("serverstrip")
            .exact_height(super::serverstrip::STRIP_H)
            .frame(edge(pal.surface).inner_margin(egui::Margin::symmetric(14, 0)))
            .show(ctx, |ui| {
                super::serverstrip::show(ui, &pal, &self.data, &mut self.view.strip, &mut actions)
            })
            .response
            .rect;
        if let Some(m) = &pal.metal {
            super::metal::groove_h(&plate, m, strip.left(), strip.right(), strip.top());
        }
        let strip_top = strip.top();

        // The detail pane slides in beside the list when a job is selected, and out (still
        // showing that job) when the selection is cleared.
        let selected_job = self.view.queue.selected.and_then(|id| self.data.jobs.iter().find(|j| j.id == id)).cloned();
        let inspecting = selected_job.is_some();
        if inspecting {
            self.view.inspector_job = selected_job.clone();
        }
        if let Some(job) = self.view.inspector_job.clone() {
            let shown = egui::SidePanel::right("inspector")
                .resizable(true)
                .default_width(super::inspector::PANEL_DEFAULT_W)
                .min_width(super::inspector::PANEL_MIN_W)
                .max_width((width * 0.45).max(super::inspector::PANEL_MIN_W))
                .frame(edge(pal.raised).inner_margin(egui::Margin::symmetric(20, 18)))
                .show_animated(ctx, inspecting, |ui| {
                    if let Some(m) = &pal.metal {
                        let r = ui.max_rect().expand2(egui::vec2(20.0, 18.0));
                        super::metal::groove_v(ui.painter(), m, r.left(), r.top(), r.bottom());
                    }
                    super::inspector::show(
                        ui,
                        &pal,
                        &self.data,
                        &job,
                        &mut self.view.inspector,
                        self.reduced_motion,
                        &mut actions,
                    )
                });
            if shown.is_none() && !inspecting {
                self.view.inspector_job = None;
            }
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(pal.panel(pal.surface))
                    .inner_margin(egui::Margin { left: 28, right: 24, top: 22, bottom: 12 }),
            )
            .show(ctx, |ui| {
                if let Some(secs) = self.data.shutdown_secs {
                    shutdown_banner(ui, &pal, secs, &mut actions);
                }
                super::center::show(ui, &pal, &self.data, &mut self.view, &mut actions);
            });

        let reduced = self.reduced_motion;
        if let Some(draft) = self.view.settings.as_mut() {
            settings::show(
                ctx,
                &pal,
                draft,
                self.view.dialog_open,
                reduced,
                &mut actions,
            );
        } else {
            motion::fade_reset(ctx, settings::fade_id());
        }
        if self.view.about_open && self.view.settings.is_none() {
            about::show(ctx, &pal, &self.view.about_mark, reduced, &mut actions);
        } else {
            motion::fade_reset(ctx, about::fade_id());
        }
        super::toast::show(ctx, &pal, &mut self.data.toasts, strip_top, reduced, &mut actions);
        drop_overlay(ctx, &pal, reduced);

        self.actions.extend(actions);
        self.apply_actions();

        // Keep ticking while downloading (the periodic refresh); an idle window only
        // redraws on input, on events and every [`IDLE_TICK`]. (Whatever moves asks for its
        // own frames, and only while it moves.)
        ctx.request_repaint_after(if self.data.status.running { Duration::from_millis(500) } else { IDLE_TICK });
    }
}

fn reveal_path(svc: &Service, p: &std::path::Path) {
    let shown = if p.is_file() {
        crate::desktop::show_item(p)
    } else {
        match p.parent().filter(|d| d.is_dir()) {
            Some(dir) => crate::desktop::show_folder(dir),
            None => Err("it is no longer there".to_owned()),
        }
    };
    if let Err(e) = shown {
        svc.log(LogLevel::Error, format!("cannot show {} in the file manager: {e}", p.display()));
    }
}

fn open_path(svc: &Service, p: &std::path::Path) {
    let opened = if p.is_dir() { crate::desktop::show_folder(p) } else { crate::desktop::open_file(p) };
    if let Err(e) = opened {
        svc.log(LogLevel::Error, format!("cannot open {}: {e}", p.display()));
    }
}

/// Watches the desktop's theme (when `watcher` is given) and its reduced-motion
/// preference (when `motion` holds the value in effect) on a thread of its own, and wakes
/// the window only when one of them changed. Stops once `alive` is gone.
fn spawn_desktop_watch(
    mut watcher: Option<ThemeWatcher>,
    mut motion: Option<bool>,
    tx: mpsc::Sender<UiMsg>,
    ctx: egui::Context,
    alive: std::sync::Weak<()>,
) {
    let spawned = std::thread::Builder::new().name("newskite-desktop-watch".into()).spawn(move || {
        let mut polls = 0u32;
        loop {
            std::thread::sleep(THEME_POLL);
            if alive.upgrade().is_none() {
                break;
            }
            let mut changed = false;
            if let Some(w) = watcher.as_mut() {
                w.set_prefers_dark_hint(ctx.system_theme().map(|t| t == egui::Theme::Dark));
                if let Some(t) = w.poll_changed() {
                    changed |= tx.send(UiMsg::ThemeChanged(Box::new(t))).is_ok();
                }
            }
            polls = polls.wrapping_add(1);
            if let (Some(was), true) = (motion, polls.is_multiple_of(MOTION_EVERY)) {
                let now = motion::reduced_motion();
                if now != was {
                    motion = Some(now);
                    changed |= tx.send(UiMsg::MotionChanged(now)).is_ok();
                }
            }
            if changed {
                ctx.request_repaint();
            }
        }
    });
    if let Err(e) = spawned {
        tracing::debug!("cannot start the desktop watch thread: {e}");
    }
}

/// While files are dragged over the window: a veil over everything with a card that
/// says what a drop will do. Its article grid lights one cell per NZB being dragged
/// (a real count, not decoration).
fn drop_overlay(ctx: &egui::Context, pal: &Palette, reduced: bool) {
    let hovered: Vec<Option<PathBuf>> =
        ctx.input(|i| i.raw.hovered_files.iter().map(|f| f.path.clone()).collect());
    let fade_id = egui::Id::new("drop-overlay-fade");
    if hovered.is_empty() {
        motion::fade_reset(ctx, fade_id);
        ctx.data_mut(|d| d.remove_temp::<Arc<(Vec<PathBuf>, usize)>>(egui::Id::new("drop-overlay-count")));
        return;
    }
    let t = motion::fade_in(ctx, fade_id, 0.16, reduced);
    // Some sources only say a drag is in progress, not which files.
    let known: Vec<PathBuf> = hovered.iter().flatten().cloned().collect();
    // Folders are read once per drag, not once per frame.
    let cache_id = egui::Id::new("drop-overlay-count");
    let cached = ctx.data(|d| d.get_temp::<Arc<(Vec<PathBuf>, usize)>>(cache_id)).filter(|c| c.0 == known);
    let nzb_count = match cached {
        Some(c) => c.1,
        None => {
            let n = expand_drop(&known).0.len();
            ctx.data_mut(|d| d.insert_temp(cache_id, Arc::new((known.clone(), n))));
            n
        }
    };
    let unknown = known.is_empty();
    let (title, color) = match nzb_count {
        _ if unknown => ("Drop NZB files to add them".to_owned(), pal.text),
        0 => ("These are not NZB files".to_owned(), pal.warning),
        1 => ("Drop to add 1 NZB".to_owned(), pal.text),
        n => (format!("Drop to add {n} NZBs"), pal.text),
    };
    let rect = ctx.content_rect();
    egui::Area::new(egui::Id::new("drop-overlay"))
        .order(egui::Order::Foreground)
        .fixed_pos(rect.min)
        .interactable(false)
        .show(ctx, |ui| {
            ui.set_opacity(t);
            ui.painter()
                .rect_filled(rect, 0.0, Palette::fade(pal.surface, 0.84));
            let lift = (1.0 - motion::ease_out(t)) * 12.0;
            let card = egui::Rect::from_center_size(rect.center() + egui::vec2(0.0, lift), egui::vec2(440.0, 184.0));
            let beat = motion::Beat::new(ctx, true, reduced, brand::PERIOD_AMBIENT * 2.0);
            let radius = egui::CornerRadius::same(18);
            ui.painter().add(egui::Shadow { offset: [0, 16], blur: 44, spread: 0, color: pal.shadow }.as_shape(card, radius));
            match &pal.metal {
                Some(m) => {
                    ui.painter().rect_filled(card, radius, m.frost);
                    ui.painter().add(super::metal::frost(m, card, 18));
                }
                None => {
                    ui.painter().rect_filled(card, radius, pal.raised);
                }
            }
            ui.painter().rect_filled(card.shrink(6.0), egui::CornerRadius::same(13), Palette::fade(pal.accent_soft, 0.5));
            // A dashed accent edge whose dashes drift around the card while it waits.
            let drift = if beat.moving { beat.phase * 40.0 } else { 0.0 };
            ui.painter().extend(egui::Shape::dashed_line_with_offset(
                &super::queue::rounded_outline(card.shrink(6.0), 13.0),
                egui::Stroke::new(2.0, pal.accent),
                &[10.0],
                &[7.0],
                drift,
            ));
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(card.shrink(28.0))
                    .layout(egui::Layout::top_down(egui::Align::Center)),
            );
            super::widgets::article_grid_lit(
                &mut child,
                pal,
                beat.moving.then_some(beat.phase),
                nzb_count,
            );
            child.add_space(16.0);
            child.label(egui::RichText::new(title).size(16.0).strong().color(color));
            child.add_space(4.0);
            child.label(
                egui::RichText::new("They join the end of the queue")
                    .small()
                    .color(pal.muted),
            );
        });
    // The drifting edge and the fade ask for their own frames; a still card needs none.
    if !reduced && t < 1.0 {
        ctx.request_repaint_after(motion::FRAME);
    }
}

fn shutdown_banner(ui: &mut egui::Ui, pal: &Palette, secs: u32, actions: &mut Vec<Action>) {
    // A notice in the flow: surface 1, a hairline and a 2 px warning rule at the left edge
    // (status colors never fill).
    let shown = egui::Frame::new()
        .fill(pal.raised)
        .stroke(egui::Stroke::new(1.0, pal.border))
        .corner_radius(egui::CornerRadius {
            nw: 0,
            sw: 0,
            ne: 4,
            se: 4,
        })
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let text = if secs == 0 {
                    "Queue finished. Shutting down now…".to_owned()
                } else {
                    format!("Queue finished. Shutting down in {secs} s.")
                };
                ui.label(egui::RichText::new(text).color(pal.text).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let cancel = super::widgets::compact_button(
                        ui,
                        pal,
                        super::widgets::ButtonKind::Outline,
                        "Cancel shutdown",
                        true,
                    );
                    if cancel.clicked() {
                        actions.push(Action::CancelShutdown);
                    }
                });
            });
        });
    let r = shown.response.rect;
    ui.painter().line_segment(
        [
            egui::pos2(r.left() + 1.0, r.top()),
            egui::pos2(r.left() + 1.0, r.bottom()),
        ],
        egui::Stroke::new(2.0, pal.warning),
    );
    ui.add_space(6.0);
}

impl eframe::App for NewskiteApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui(ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.palette.surface.to_normalized_gamma_f32()
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.service.shutdown();
    }
}

/// Forwards service events to the UI thread and wakes it. The frequent ones are folded
/// into `pending` (segments summed per job, throughput stamped on arrival; see
/// [`super::events`]) and only schedule a repaint within 100 ms; the rest travel through
/// the channel and wake the UI at once.
fn spawn_forwarder(
    service: &Service,
    tx: mpsc::Sender<UiMsg>,
    pending: SharedPending,
    ctx: egui::Context,
) {
    let mut rx = service.subscribe();
    let spawned = std::thread::Builder::new()
        .name("newskite-ui-events".into())
        .spawn(move || loop {
            let msg = match rx.blocking_recv() {
                Ok(ServiceEvent::Engine {
                    job_id,
                    event: EngineEvent::SegmentDone { bytes, .. },
                }) => {
                    if let Ok(mut p) = pending.lock() {
                        p.add_segment(job_id, bytes);
                    }
                    ctx.request_repaint_after(Duration::from_millis(100));
                    continue;
                }
                Ok(ServiceEvent::Throughput { bytes_per_sec }) => {
                    if let Ok(mut p) = pending.lock() {
                        p.add_throughput(now_secs(), bytes_per_sec);
                    }
                    ctx.request_repaint();
                    continue;
                }
                Ok(ev) => UiMsg::Service(ev),
                Err(RecvError::Lagged(_)) => UiMsg::Resync,
                Err(RecvError::Closed) => break,
            };
            if tx.send(msg).is_err() {
                break;
            }
            ctx.request_repaint();
        });
    if let Err(e) = spawned {
        service.log(
            LogLevel::Error,
            format!("cannot start the UI event thread: {e}"),
        );
    }
}

/// `true` for a path whose extension is `.nzb` (any case).
pub fn is_nzb(p: &std::path::Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("nzb"))
}

/// Splits dropped paths into NZB files to import and paths to skip. A folder contributes
/// the `.nzb` files directly inside it (sorted by name, not recursive); a folder without
/// any is skipped.
pub fn expand_drop(paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut nzbs = Vec::new();
    let mut skipped = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut inside: Vec<PathBuf> = std::fs::read_dir(p)
                .map(|rd| {
                    rd.filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|f| f.is_file() && is_nzb(f))
                        .collect()
                })
                .unwrap_or_default();
            inside.sort();
            if inside.is_empty() {
                skipped.push(p.clone());
            }
            nzbs.extend(inside);
        } else if is_nzb(p) {
            nzbs.push(p.clone());
        } else {
            skipped.push(p.clone());
        }
    }
    (nzbs, skipped)
}

/// Drops from `seen` the jobs that are no longer done or failed (retried, removed), so
/// their next completion notifies again.
fn forget_unfinished(seen: &mut HashSet<i64>, jobs: &[Job]) {
    if seen.is_empty() {
        return;
    }
    let finished: HashSet<i64> =
        jobs.iter().filter(|j| matches!(j.state, JobState::Done | JobState::Failed)).map(|j| j.id).collect();
    seen.retain(|id| finished.contains(id));
}

/// The window's title: while a run is active, how far the pending bytes are and the
/// measured speed, so a minimized window still says how it is going; the name alone
/// otherwise.
pub fn window_title(data: &Data) -> String {
    let st = &data.status;
    let name = newskite_types::APP_NAME;
    if st.running && !st.paused {
        let pct = (statusbar::pending_fraction(data) * 100.0).floor() as u32;
        match data.speed() {
            0 => format!("{pct}% · {name}"),
            speed => format!("{pct}% · {} · {name}", newskite_types::format::speed(speed)),
        }
    } else if st.paused {
        format!("Paused · {name}")
    } else {
        name.to_owned()
    }
}

/// The notification text for jobs finished (done or failed) and not yet in `seen`, if
/// there are any.
fn completion_notice(jobs: &[Job], seen: &HashSet<i64>) -> Option<String> {
    let new: Vec<&Job> = jobs
        .iter()
        .filter(|j| matches!(j.state, JobState::Done | JobState::Failed) && !seen.contains(&j.id))
        .collect();
    if new.is_empty() {
        return None;
    }
    let done = new.iter().filter(|j| j.state == JobState::Done).count();
    let failed = new.len() - done;
    let counts = match (done, failed) {
        (d, 0) => format!("{d} download{} finished", if d == 1 { "" } else { "s" }),
        (0, f) => format!("{f} download{} failed", if f == 1 { "" } else { "s" }),
        (d, f) => format!("{d} finished, {f} failed"),
    };
    // Failed ones first: they are the ones to act on.
    let mut named: Vec<&Job> = new.clone();
    named.sort_by_key(|j| (j.state != JobState::Failed, j.id));
    let mut names: Vec<String> = named.iter().take(3).map(|j| j.name.clone()).collect();
    if new.len() > 3 {
        names.push(format!("and {} more", new.len() - 3));
    }
    Some(format!("{counts}: {}", names.join(", ")))
}

/// Title of a completion notification: it says whether anything needs attention (read
/// from the counts before the names, so a job called "failed" cannot fool it).
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn notice_title(body: &str) -> &'static str {
    if body.split(':').next().unwrap_or("").contains("failed") {
        "Newskite: downloads need attention"
    } else {
        "Newskite: downloads complete"
    }
}

/// Shows a desktop notification (Linux/BSD); a no-op elsewhere. Runs on its own thread
/// so a slow notification daemon never blocks the UI.
fn desktop_notify(body: String) {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::thread::Builder::new()
            .name("newskite-notify".into())
            .spawn(move || {
                if let Err(e) = notify_rust::Notification::new()
                    .appname(newskite_types::APP_NAME)
                    .summary(notice_title(&body))
                    .body(&body)
                    .icon(newskite_types::APP_ID)
                    .show()
                {
                    tracing::debug!("desktop notification failed: {e}");
                }
            });
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        tracing::debug!("queue finished: {body}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: i64, state: JobState) -> Job {
        Job {
            id,
            name: format!("j{id}"),
            state,
            bytes_total: 1,
            bytes_done: 0,
            dest_dir: PathBuf::new(),
            source_path: PathBuf::new(),
            created_at: id,
            error: None,
            password: None,
            stats: Default::default(),
        }
    }

    #[test]
    fn a_retried_job_notifies_when_it_finishes_again() {
        // Run 1 ends with the job failed: notified, then remembered.
        let mut seen = HashSet::new();
        let jobs = vec![job(1, JobState::Failed)];
        assert_eq!(
            completion_notice(&jobs, &seen).as_deref(),
            Some("1 download failed: j1")
        );
        seen.insert(1);
        assert_eq!(completion_notice(&jobs, &seen), None);
        // Retry: queued again, so it is forgotten...
        let jobs = vec![job(1, JobState::Queued)];
        forget_unfinished(&mut seen, &jobs);
        assert!(seen.is_empty());
        // ...and its completion is news.
        let jobs = vec![job(1, JobState::Done)];
        assert_eq!(
            completion_notice(&jobs, &seen).as_deref(),
            Some("1 download finished: j1")
        );
    }

    #[test]
    fn drops_keep_nzbs_expand_folders_and_skip_the_rest() {
        let dir = tempfile_dir();
        let a = dir.join("a.NZB");
        let b = dir.join("notes.txt");
        let folder = dir.join("batch");
        let empty = dir.join("empty");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(&empty).unwrap();
        for p in [
            &a,
            &b,
            &folder.join("2.nzb"),
            &folder.join("1.nzb"),
            &folder.join("x.jpg"),
        ] {
            std::fs::write(p, "x").unwrap();
        }
        let (nzbs, skipped) = expand_drop(&[a.clone(), b.clone(), folder.clone(), empty.clone()]);
        assert_eq!(nzbs, vec![a, folder.join("1.nzb"), folder.join("2.nzb")]);
        assert_eq!(skipped, vec![b, empty]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "newskite-drop-test-{}-{}",
            std::process::id(),
            now_secs()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_title_reports_a_run_and_rests_otherwise() {
        let mut d = Data::default();
        assert_eq!(window_title(&d), "Newskite");
        let mut a = job(1, JobState::Downloading);
        a.bytes_total = 1000;
        a.bytes_done = 425;
        d.jobs = vec![a];
        d.status.running = true;
        assert_eq!(window_title(&d), "42% · Newskite", "no measured speed, none shown");
        d.status.speed_bps = 2 * 1024 * 1024;
        assert_eq!(window_title(&d), "42% · 2.0 MB/s · Newskite");
        d.status.running = false;
        d.status.paused = true;
        assert_eq!(window_title(&d), "Paused · Newskite");
    }

    #[test]
    fn notice_counts_done_and_failed() {
        let jobs = vec![
            job(1, JobState::Done),
            job(2, JobState::Done),
            job(3, JobState::Failed),
            job(4, JobState::Queued),
        ];
        let seen = HashSet::from([1]);
        assert_eq!(
            completion_notice(&jobs, &seen).as_deref(),
            Some("1 finished, 1 failed: j3, j2")
        );
        assert_eq!(notice_title("1 finished, 1 failed: j3, j2"), "Newskite: downloads need attention");
        assert_eq!(notice_title("2 downloads finished: a, b"), "Newskite: downloads complete");
        let many: Vec<Job> = (1..=5).map(|i| job(i, JobState::Done)).collect();
        assert_eq!(
            completion_notice(&many, &HashSet::new()).as_deref(),
            Some("5 downloads finished: j1, j2, j3, and 2 more")
        );
    }
}
