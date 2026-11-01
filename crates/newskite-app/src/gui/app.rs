//! Application state, event plumbing and the per-frame layout.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};
use newskite_service::{
    Bookmark, Job, JobState, LogLevel, LogLine, NewBookmark, Service, ServiceEvent, Status,
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
use super::{about, docks, fonts, icons, motion, queue, settings, sidebar, statusbar, toolbar};

/// How often the job list is re-read while a job is downloading (outline §7).
pub const JOBS_REFRESH: Duration = Duration::from_millis(1500);
/// Live throughput series length (seconds).
pub const LIVE_WINDOW_SECS: usize = 600;
/// Stored history shown in the graph (seconds).
pub const HISTORY_SECS: u64 = 24 * 3600;
/// Share of the window height the 2×2 dock grid starts at (clamped to 260–320 px).
const GRID_DOCK_SHARE: f32 = 0.46;
/// How often the stored throughput history is re-read (also after each queue run).
pub const HISTORY_REFRESH: Duration = Duration::from_secs(60);
/// Theme watcher poll interval.
const THEME_POLL: Duration = Duration::from_secs(1);
/// How often the reduced-motion preference is re-read (when not fixed by the config).
const MOTION_POLL: Duration = Duration::from_secs(5);

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
    /// Indexer search (placeholder).
    Search,
    /// Latest file groups (placeholder).
    Latest,
    /// The download queue (default tab).
    Queue,
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
    /// Start the queue.
    Start,
    /// Pause after the current job.
    Pause,
    /// Resume.
    Resume,
    /// Scan the watch folder now.
    ScanWatch,
    /// Remove a job.
    RemoveJob(i64),
    /// Failed → queued.
    RetryJob(i64),
    /// Set or clear a job's archive password.
    SetJobPassword(i64, Option<String>),
    /// Add a bookmark.
    AddBookmark(NewBookmark),
    /// Remove a bookmark (and its children).
    RemoveBookmark(i64),
    /// Expand/collapse a bookmark.
    ToggleBookmark(i64),
    /// Show the settings modal.
    OpenSettings,
    /// Persist the settings draft.
    SaveSettings(Box<Settings>),
    /// Close the settings modal without saving.
    CloseSettings,
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
    /// Focus the search box.
    FocusSearch,
    /// Write an info line to the message log (harmless placeholder actions).
    Note(String),
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
    /// Bookmarks in tree order.
    pub bookmarks: Vec<Bookmark>,
    /// Log lines, newest first.
    pub logs: Vec<LogLine>,
    /// Current saved settings.
    pub settings: Settings,
    /// Queue status (re-read each frame).
    pub status: Status,
    /// Stored throughput history loaded at start.
    pub history: Vec<ThroughputSample>,
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
}

impl Data {
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
    /// Toolbar search text.
    pub search_text: String,
    /// Focus the search box next frame.
    pub focus_search: bool,
    /// Open NZB inline field.
    pub open_nzb: OpenNzbState,
    /// Bookmarks sidebar state.
    pub sidebar: sidebar::SidebarState,
    /// Queue tab state.
    pub queue: queue::QueueState,
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
}

enum ThemeState {
    Watch(Box<ThemeWatcher>),
    Fixed,
}

/// The Newskite main window.
pub struct NewskiteApp {
    pub(crate) service: Service,
    rt: Handle,
    ctx: egui::Context,
    rx: mpsc::Receiver<UiMsg>,
    tx: mpsc::Sender<UiMsg>,
    theme_state: ThemeState,
    last_theme_poll: Instant,
    /// The desktop's theme (or the fixed one), before the Appearance choice.
    desktop_theme: Theme,
    /// Settings › Appearance.
    pub appearance: Appearance,
    appearance_file: Option<PathBuf>,
    /// Reduced motion is in effect (the mark rests).
    pub reduced_motion: bool,
    fixed_motion: Option<bool>,
    last_motion_poll: Instant,
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
    notifications: bool,
    /// Jobs already finished when last notified (so only new completions notify).
    finished_seen: HashSet<i64>,
    /// The body of the last completion notification (also kept when notifications are
    /// off, so the behavior can be checked).
    pub last_notice: Option<String>,
    /// Frequent events folded by the forwarder thread (see [`super::events`]).
    pending: SharedPending,
    /// A manual watch-folder scan is running.
    scanning: bool,
    last_history_reload: Instant,
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
        let (theme_state, desktop_theme) = match config.theme {
            ThemeMode::Desktop => {
                let hint = ctx.system_theme().map(|t| t == egui::Theme::Dark);
                let watcher = ThemeWatcher::with_hint(hint);
                let theme = watcher.current().clone();
                (ThemeState::Watch(Box::new(watcher)), theme)
            }
            ThemeMode::Fixed(t) => (ThemeState::Fixed, t),
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
        let reduced_motion = config.reduced_motion.unwrap_or_else(motion::reduced_motion);

        if config.forward_events {
            spawn_forwarder(&service, tx.clone(), pending.clone(), ctx.clone());
        }

        let mut app = Self {
            service,
            rt,
            ctx,
            rx,
            tx,
            theme_state,
            last_theme_poll: Instant::now(),
            desktop_theme,
            appearance,
            appearance_file: config.appearance_file,
            reduced_motion,
            fixed_motion: config.reduced_motion,
            last_motion_poll: Instant::now(),
            masthead_clock: MarkClock::new(),
            about_clock: MarkClock::new(),
            theme,
            palette,
            data: Data::default(),
            view: ViewState {
                tab: Tab::Queue,
                search_text: String::new(),
                focus_search: false,
                open_nzb: OpenNzbState::default(),
                sidebar: sidebar::SidebarState::default(),
                queue: queue::QueueState::default(),
                settings: None,
                dialog_open: false,
                about_open: false,
                masthead_mark: brand::rest_heights(),
                about_mark: brand::rest_heights(),
            },
            actions: Vec::new(),
            last_jobs_reload: Instant::now(),
            notifications: config.notifications,
            finished_seen: HashSet::new(),
            last_notice: None,
            pending,
            scanning: false,
            last_history_reload: Instant::now(),
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
        self.reload_jobs();
        self.data.bookmarks = self.service.bookmarks();
        // Settings first: reading them logs an unreadable document once, and the log
        // read after it then includes that line.
        self.data.settings = self.service.settings();
        self.data.settings_load_error = self.service.settings_load_error();
        self.data.logs = self.service.logs();
        self.data.status = self.service.status();
        self.reload_history();
    }

    /// Re-reads the stored throughput history (the last [`HISTORY_SECS`]).
    pub fn reload_history(&mut self) {
        self.data.history = self.service.throughput_history(HISTORY_SECS);
        self.last_history_reload = Instant::now();
    }

    /// Re-reads jobs and totals.
    pub fn reload_jobs(&mut self) {
        self.data.jobs = self.service.jobs();
        self.data.totals = self.service.totals();
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
                    self.scanning = false;
                    reload_jobs = true;
                    reload_logs = true;
                }
            }
        }
        if reload_jobs {
            self.reload_jobs();
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
                EngineEvent::FileStarted { .. } | EngineEvent::Log(_) => {}
            },
            ServiceEvent::JobLog(_) | ServiceEvent::Error(_) => *reload_logs = true,
            ServiceEvent::JobsChanged | ServiceEvent::WatchImported(_) => *reload_jobs = true,
            ServiceEvent::QueueIdle => {
                *reload_jobs = true;
                self.reload_jobs();
                self.notify_finished();
                self.reload_history();
            }
            ServiceEvent::Paused | ServiceEvent::Resumed => {}
            // Normally folded (and stamped on arrival) by the forwarder; a sample sent
            // straight to the channel is stamped now.
            ServiceEvent::Throughput { bytes_per_sec } => self.push_live(now_secs(), bytes_per_sec),
            ServiceEvent::ShutdownCountdown { secs_left } => {
                self.data.shutdown_secs = Some(secs_left)
            }
            ServiceEvent::ShutdownCanceled => self.data.shutdown_secs = None,
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
            Action::Pause => self.service.pause(),
            Action::Resume => self.service.resume(),
            Action::ScanWatch => {
                // Directory reads and NZB parsing (possibly on a slow network mount) run
                // off the UI thread. Errors are logged by the service.
                if !self.scanning {
                    self.scanning = true;
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
                if self.service.retry_job(id).is_ok() {
                    // Its next completion is news again.
                    self.finished_seen.remove(&id);
                }
                self.reload_jobs();
            }
            Action::SetJobPassword(id, pw) => {
                let _ = self.service.set_job_password(id, pw);
                self.reload_jobs();
            }
            Action::AddBookmark(b) => {
                match self.service.add_bookmark(b) {
                    Ok(_) => self.view.sidebar.form = None,
                    Err(e) => {
                        if let Some(form) = self.view.sidebar.form.as_mut() {
                            form.error = Some(format!("Could not add the bookmark: {e}"));
                        }
                        self.data.logs = self.service.logs();
                    }
                }
                self.reload_bookmarks();
            }
            Action::RemoveBookmark(id) => {
                let _ = self.service.remove_bookmark(id);
                self.reload_bookmarks();
            }
            Action::ToggleBookmark(id) => {
                if let Some(b) = self.data.bookmarks.iter().find(|b| b.id == id) {
                    let mut b = b.clone();
                    b.expanded = !b.expanded;
                    let _ = self.service.update_bookmark(&b);
                }
                self.reload_bookmarks();
            }
            Action::OpenSettings => {
                if self.view.settings.is_none() {
                    self.view.about_open = false;
                    self.data.settings = self.service.settings();
                    self.data.settings_load_error = self.service.settings_load_error();
                    let mut draft = SettingsDraft::new(self.data.settings.clone());
                    draft.appearance = self.appearance;
                    draft.load_error = self.data.settings_load_error.clone();
                    self.view.settings = Some(draft);
                }
            }
            Action::SaveSettings(s) => match self.service.save_settings(*s) {
                Ok(()) => {
                    self.data.settings = self.service.settings();
                    self.data.settings_load_error = self.service.settings_load_error();
                    // The sheet's appearance is applied with the save, never before it.
                    if let Some(a) = self.view.settings.take().map(|d| d.appearance) {
                        self.set_appearance(a);
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
            Action::CloseSettings => self.view.settings = None,
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
            Action::FocusSearch => {
                self.view.tab = Tab::Search;
                self.view.focus_search = true;
            }
            Action::Note(text) => {
                self.service.log(LogLevel::Info, text);
                self.data.logs = self.service.logs();
            }
            Action::OpenAbout => {
                if self.view.settings.is_none() && !self.view.about_open {
                    self.view.about_open = true;
                    self.about_clock.reset();
                }
            }
            Action::CloseAbout => self.view.about_open = false,
            Action::OpenUrl(url) => {
                if let Err(e) = open::that_detached(&url) {
                    self.service.log(
                        LogLevel::Error,
                        format!("cannot open {url} in the browser: {e}"),
                    );
                    self.data.logs = self.service.logs();
                }
            }
            Action::SetAppearance(a) => self.set_appearance(a),
        }
    }

    /// Re-reads the bookmarks; an add form whose parent no longer exists falls back to
    /// the top level (what its picker already shows).
    fn reload_bookmarks(&mut self) {
        self.data.bookmarks = self.service.bookmarks();
        if let Some(form) = self.view.sidebar.form.as_mut() {
            if form
                .parent
                .is_some_and(|p| !self.data.bookmarks.iter().any(|b| b.id == p))
            {
                form.parent = None;
            }
        }
    }

    /// Wears the theme for `a` and stores the choice (a write failure is logged; the
    /// choice still applies for this session).
    pub fn set_appearance(&mut self, a: Appearance) {
        if a != self.appearance {
            self.appearance = a;
            self.retheme();
            if let Some(path) = &self.appearance_file {
                if let Err(e) = appearance::save(path, a) {
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
        self.rt.spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Open NZB")
                .add_filter("NZB files", &["nzb", "NZB"])
                .pick_files()
                .await;
            let msg = match picked {
                Some(files) if !files.is_empty() => {
                    UiMsg::NzbPicked(files.iter().map(|f| f.path().to_path_buf()).collect())
                }
                _ => UiMsg::DialogClosed,
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
        self.rt.spawn(async move {
            let mut dlg = rfd::AsyncFileDialog::new().set_title(target.title());
            if let Some(dir) = start {
                dlg = dlg.set_directory(dir);
            }
            let msg = match dlg.pick_folder().await {
                Some(f) => UiMsg::FolderPicked(target, f.path().to_path_buf()),
                None => UiMsg::DialogClosed,
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn open_download_folder(&mut self) {
        let dir = self.data.settings.download_dir.clone();
        if dir.as_os_str().is_empty() {
            self.service
                .log(LogLevel::Warn, "no download folder configured");
        } else if let Err(e) = std::fs::create_dir_all(&dir) {
            self.service.log(
                LogLevel::Error,
                format!("cannot create download folder {}: {e}", dir.display()),
            );
        } else if let Err(e) = open::that_detached(&dir) {
            self.service.log(
                LogLevel::Error,
                format!("cannot open {} in the file manager: {e}", dir.display()),
            );
        }
        self.data.logs = self.service.logs();
    }

    /// Polls the theme watcher (at most once per second) and re-themes on change.
    fn poll_theme(&mut self, ctx: &egui::Context) {
        if self.last_theme_poll.elapsed() < THEME_POLL {
            return;
        }
        self.last_theme_poll = Instant::now();
        if let ThemeState::Watch(w) = &mut self.theme_state {
            w.set_prefers_dark_hint(ctx.system_theme().map(|t| t == egui::Theme::Dark));
            if let Some(t) = w.poll_changed() {
                tracing::info!(source = ?t.source, dark = t.dark, "desktop theme changed");
                self.desktop_theme = t;
                self.retheme();
            }
        }
        if self.fixed_motion.is_none() && self.last_motion_poll.elapsed() >= MOTION_POLL {
            self.last_motion_poll = Instant::now();
            self.reduced_motion = motion::reduced_motion();
        }
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

    /// NZB files dropped onto the window are imported like Open NZB.
    fn dropped_files(&mut self, ctx: &egui::Context) {
        let paths: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        for p in paths {
            self.actions.push(Action::AddNzb(p));
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let open = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
        let prefs = KeyboardShortcut::new(Modifiers::COMMAND, Key::Comma);
        let (o, p, esc) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&open),
                i.consume_shortcut(&prefs),
                i.key_pressed(Key::Escape),
            )
        });
        if o {
            self.actions.push(Action::ShowOpenNzb);
        }
        if p {
            self.actions.push(Action::OpenSettings);
        }
        if esc {
            if self.view.settings.is_some() {
                self.actions.push(Action::CloseSettings);
            } else if self.view.about_open {
                self.actions.push(Action::CloseAbout);
            } else if self.view.open_nzb.visible {
                self.view.open_nzb.visible = false;
                self.view.open_nzb.error = None;
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
        self.poll_theme(ctx);
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
            self.reload_history();
        }
        self.shortcuts(ctx);
        self.dropped_files(ctx);
        self.animate_marks(ctx);

        let mut actions = Vec::new();
        let pal = self.palette;
        let width = ctx.content_rect().width();

        egui::TopBottomPanel::top("toolbar")
            .frame(
                egui::Frame::new()
                    .fill(pal.raised)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .stroke(egui::Stroke::new(1.0, pal.border)),
            )
            .show(ctx, |ui| {
                toolbar::show(ui, &pal, &self.data, &mut self.view, &mut actions)
            });

        egui::TopBottomPanel::bottom("statusbar")
            .exact_height(28.0)
            .frame(
                egui::Frame::new()
                    .fill(pal.raised)
                    .inner_margin(egui::Margin::symmetric(10, 3))
                    .stroke(egui::Stroke::new(1.0, pal.border)),
            )
            .show(ctx, |ui| {
                statusbar::show(ui, &pal, &self.data, &mut actions)
            });

        // One panel id per arrangement so each keeps its own (user-resizable) height. The
        // 2×2 grid starts at a share of the window height, so a 600 px window still leaves
        // the queue a few rows.
        let window_h = ctx.content_rect().height();
        let (dock_id, dock_h) = match docks::layout_for(width) {
            docks::DockLayout::Row => ("docks-row", 240.0),
            docks::DockLayout::Grid => (
                "docks-grid",
                (window_h * GRID_DOCK_SHARE).clamp(260.0, 320.0),
            ),
            docks::DockLayout::Stack => ("docks-stack", 260.0),
        };
        egui::TopBottomPanel::bottom(dock_id)
            .resizable(true)
            .default_height(dock_h)
            .min_height(150.0)
            .max_height((ctx.content_rect().height() * 0.6).max(160.0))
            .frame(
                egui::Frame::new()
                    .fill(pal.surface)
                    .inner_margin(egui::Margin::same(8)),
            )
            .show(ctx, |ui| {
                docks::show(ui, &pal, &self.data, self.reduced_motion, &mut actions)
            });

        egui::SidePanel::left("bookmarks")
            .resizable(true)
            .default_width(220.0)
            .width_range(170.0..=360.0)
            .frame(
                egui::Frame::new()
                    .fill(pal.surface)
                    .inner_margin(egui::Margin::same(10))
                    .stroke(egui::Stroke::new(1.0, pal.border)),
            )
            .show(ctx, |ui| {
                sidebar::show(ui, &pal, &self.data, &mut self.view.sidebar, &mut actions)
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(pal.surface)
                    .inner_margin(egui::Margin::same(10)),
            )
            .show(ctx, |ui| {
                if let Some(secs) = self.data.shutdown_secs {
                    shutdown_banner(ui, &pal, secs, &mut actions);
                }
                queue::show_center(ui, &pal, &self.data, &mut self.view, &mut actions);
            });

        if let Some(draft) = self.view.settings.as_mut() {
            settings::show(ctx, &pal, draft, self.view.dialog_open, &mut actions);
        } else if self.view.about_open {
            about::show(ctx, &pal, &self.view.about_mark, &mut actions);
        }

        self.actions.extend(actions);
        self.apply_actions();

        // Keep ticking: theme polling and periodic refresh while downloading. (The marks
        // ask for frame-rate repaints themselves, and only while they move.)
        ctx.request_repaint_after(if self.data.status.running {
            Duration::from_millis(500)
        } else {
            THEME_POLL
        });
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
                    if ui.button("Cancel shutdown").clicked() {
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

/// Drops from `seen` the jobs that are no longer done or failed (retried, removed), so
/// their next completion notifies again.
fn forget_unfinished(seen: &mut HashSet<i64>, jobs: &[Job]) {
    seen.retain(|id| {
        jobs.iter()
            .any(|j| j.id == *id && matches!(j.state, JobState::Done | JobState::Failed))
    });
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
    Some(match (done, failed) {
        (d, 0) => format!("{d} download{} finished", if d == 1 { "" } else { "s" }),
        (0, f) => format!("{f} download{} failed", if f == 1 { "" } else { "s" }),
        (d, f) => format!("{d} finished, {f} failed"),
    })
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
                    .summary("Newskite queue finished")
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
        }
    }

    #[test]
    fn a_retried_job_notifies_when_it_finishes_again() {
        // Run 1 ends with the job failed: notified, then remembered.
        let mut seen = HashSet::new();
        let jobs = vec![job(1, JobState::Failed)];
        assert_eq!(
            completion_notice(&jobs, &seen).as_deref(),
            Some("1 download failed")
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
            Some("1 download finished")
        );
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
            Some("1 finished, 1 failed")
        );
    }
}
