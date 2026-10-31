//! Newskite headless mode (outline F11): download NZBs from the command line with the same
//! queue service, engine and database the desktop app uses.
//!
//! ```text
//! newskite --headless --nzb FILE [--nzb FILE ...] [--dest DIR] [--server HOST:PORT [--tls]
//!          [--user U] [--pass P]] [--connections N] [--db DIR] [--retries N]
//! ```
//!
//! * `--server` replaces the stored servers **for this run only**: the override lives in
//!   memory ([`Service::set_runtime_settings_override`]) and the stored settings, including
//!   any password, are never touched. Without `--server` the stored servers are used.
//! * `--pass` may instead come from the `NEWSKITE_PASS` environment variable, so the
//!   password does not have to appear in the process list.
//! * `--dest` replaces the download folder for jobs added by this invocation.
//! * The watch folder and auto-shutdown are disabled for the run.
//!
//! Output: exactly one line on stdout per event of interest — `queued <name>`,
//! `complete <name>` and `failed <name>: <reason>` — and, only when stderr is a terminal, a
//! progress line on stderr refreshed at most once per second.
//!
//! Exit codes: [`EXIT_OK`] when every job added by this invocation ended `done`,
//! [`EXIT_USAGE`] for usage errors, [`EXIT_INTERRUPTED`] after Ctrl-C (the active job is
//! left resumable) and [`EXIT_FAILURE`] otherwise.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::{Duration, Instant};

use clap::Parser;
use newskite_service::{Job, JobState, Service, ServiceError, ServiceEvent, ServiceOptions};
use newskite_types::{format, ServerConfig, Settings};
use tokio::sync::broadcast::error::RecvError;

/// Every job added by this invocation ended `done`.
pub const EXIT_OK: i32 = 0;
/// At least one job failed, could not be added or was not downloaded.
pub const EXIT_FAILURE: i32 = 1;
/// Invalid command line (bad flag, malformed `--server`, nothing to download).
pub const EXIT_USAGE: i32 = 2;
/// Interrupted with Ctrl-C; unfinished jobs stay queued and resume on the next run.
pub const EXIT_INTERRUPTED: i32 = 130;

/// Default NNTP port without TLS.
pub const DEFAULT_PORT: u16 = 119;
/// Default NNTPS port (`--tls`).
pub const DEFAULT_TLS_PORT: u16 = 563;
/// Environment variable read for the server password when `--pass` is absent.
pub const PASSWORD_ENV: &str = "NEWSKITE_PASS";

/// How long to wait for the queue run to wind down after the CLI is done with it.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);
/// Width the progress line is padded to, so a shorter line fully overwrites a longer one.
const PROGRESS_WIDTH: usize = 64;

/// Command line of headless mode (`newskite --headless ...`).
#[derive(Clone, Default, PartialEq, Eq, Parser)]
#[command(
    name = "newskite",
    about = "Download NZB files without the window (headless mode)",
    long_about = None
)]
pub struct HeadlessArgs {
    /// Selects headless mode; consumed by the app binary and accepted here harmlessly.
    #[arg(long, hide = true)]
    pub headless: bool,

    /// NZB file to download (repeatable).
    #[arg(long = "nzb", value_name = "FILE", required = true)]
    pub nzb: Vec<PathBuf>,

    /// Download folder for this run (default: the folder from the app settings).
    #[arg(long, value_name = "DIR")]
    pub dest: Option<PathBuf>,

    /// News server for this run (`HOST:PORT`, IPv6 in brackets); not saved.
    #[arg(
        long,
        value_name = "HOST:PORT",
        help = "News server for this run, e.g. news.example.com:563 or [::1]:119 (default port: \
                563 with --tls, 119 without). Not saved. Without it the saved servers are used."
    )]
    pub server: Option<ServerAddr>,

    /// Use TLS for --server.
    #[arg(long)]
    pub tls: bool,

    /// Username for --server.
    #[arg(long, value_name = "U")]
    pub user: Option<String>,

    /// Password for --server (or set NEWSKITE_PASS to keep it out of the process list).
    #[arg(long, value_name = "P", env = PASSWORD_ENV, hide_env_values = true)]
    pub pass: Option<String>,

    /// Simultaneous connections (1-500).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..=500))]
    pub connections: Option<u16>,

    /// Database folder (default: the app's data folder).
    #[arg(long, value_name = "DIR")]
    pub db: Option<PathBuf>,

    /// Retries per article for transient errors (0-100).
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(0..=100))]
    pub retries: Option<u32>,
}

impl fmt::Debug for HeadlessArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeadlessArgs")
            .field("headless", &self.headless)
            .field("nzb", &self.nzb)
            .field("dest", &self.dest)
            .field("server", &self.server)
            .field("tls", &self.tls)
            .field("user", &self.user)
            .field("pass", &self.pass.as_ref().map(|_| "<redacted>"))
            .field("connections", &self.connections)
            .field("db", &self.db)
            .field("retries", &self.retries)
            .finish()
    }
}

/// A `--server` value: host plus optional port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerAddr {
    /// Host name or IP address (IPv6 without brackets).
    pub host: String,
    /// Port, if one was given.
    pub port: Option<u16>,
}

impl ServerAddr {
    /// The given port, or [`DEFAULT_TLS_PORT`]/[`DEFAULT_PORT`] depending on `tls`.
    pub fn port_or_default(&self, tls: bool) -> u16 {
        self.port
            .unwrap_or(if tls { DEFAULT_TLS_PORT } else { DEFAULT_PORT })
    }
}

impl fmt::Display for ServerAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match self.port {
            Some(p) => write!(f, "{host}:{p}"),
            None => f.write_str(&host),
        }
    }
}

/// Why a `--server` value could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServerAddrError {
    /// Nothing was given.
    #[error("the server address is empty")]
    Empty,
    /// The host part is empty or contains invalid characters.
    #[error("invalid host {0:?}")]
    InvalidHost(String),
    /// The port is not a number in 1-65535.
    #[error("invalid port {0:?} (expected 1-65535)")]
    InvalidPort(String),
    /// A bracketed IPv6 address is malformed.
    #[error("invalid IPv6 address {0:?} (use [addr]:port)")]
    InvalidIpv6(String),
    /// Several colons without brackets.
    #[error("ambiguous address {0:?}: write IPv6 addresses as [addr]:port")]
    Ambiguous(String),
}

fn parse_port(p: &str) -> Result<u16, ServerAddrError> {
    match p.parse::<u16>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(ServerAddrError::InvalidPort(p.to_string())),
    }
}

fn check_host(h: &str) -> Result<String, ServerAddrError> {
    let valid = !h.is_empty()
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !h.starts_with('-');
    if valid {
        Ok(h.to_string())
    } else {
        Err(ServerAddrError::InvalidHost(h.to_string()))
    }
}

impl FromStr for ServerAddr {
    type Err = ServerAddrError;

    /// Accepts `host`, `host:port`, `[v6]`, `[v6]:port` and a bare IPv6 address (no port).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(ServerAddrError::Empty);
        }
        if let Some(rest) = s.strip_prefix('[') {
            let (inside, after) = rest
                .split_once(']')
                .ok_or_else(|| ServerAddrError::InvalidIpv6(s.to_string()))?;
            let ip: Ipv6Addr = inside
                .parse()
                .map_err(|_| ServerAddrError::InvalidIpv6(s.to_string()))?;
            let port = match after {
                "" => None,
                a => match a.strip_prefix(':') {
                    Some(p) => Some(parse_port(p)?),
                    None => return Err(ServerAddrError::InvalidIpv6(s.to_string())),
                },
            };
            return Ok(ServerAddr {
                host: ip.to_string(),
                port,
            });
        }
        match s.matches(':').count() {
            0 => Ok(ServerAddr {
                host: check_host(s)?,
                port: None,
            }),
            1 => {
                let (h, p) = s.split_once(':').ok_or(ServerAddrError::Empty)?;
                Ok(ServerAddr {
                    host: check_host(h)?,
                    port: Some(parse_port(p)?),
                })
            }
            _ => match s.parse::<Ipv6Addr>() {
                Ok(ip) => Ok(ServerAddr {
                    host: ip.to_string(),
                    port: None,
                }),
                Err(_) => Err(ServerAddrError::Ambiguous(s.to_string())),
            },
        }
    }
}

/// Setup failures of a headless run (reported on stderr).
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// Invalid combination of arguments.
    #[error("{0}")]
    Usage(String),
    /// The database could not be opened.
    #[error("could not open the database: {0}")]
    Service(#[from] ServiceError),
    /// The async runtime or the filesystem failed.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl CliError {
    /// The process exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Usage(_) => EXIT_USAGE,
            CliError::Service(_) | CliError::Io(_) => EXIT_FAILURE,
        }
    }
}

/// Parses headless arguments (the first item is the program name; `--headless` is
/// accepted). Help and version requests are returned as errors too, like clap does.
pub fn parse_args<I, T>(args: I) -> Result<HeadlessArgs, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    HeadlessArgs::try_parse_from(args)
}

/// Parses `args` and runs headless mode. A parse failure prints clap's message and exits
/// with [`EXIT_USAGE`] (help exits 0).
pub fn run_headless_from_args<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    match parse_args(args) {
        Ok(a) => run_headless(a),
        Err(e) => {
            let code = e.exit_code();
            // Nothing sensible to do if the terminal is gone.
            let _ = e.print();
            exit_code(code)
        }
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Runs headless mode on a dedicated multi-thread tokio runtime, printing to stdout.
/// Ctrl-C stops cleanly (exit 130).
pub fn run_headless(args: HeadlessArgs) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("newskite-cli")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("newskite: could not start the async runtime: {e}");
            return exit_code(EXIT_FAILURE);
        }
    };
    let code = rt.block_on(async {
        let mut out = std::io::stdout();
        run_headless_with(args, &mut out).await
    });
    // Do not wait for blocking tasks (e.g. post-processing) after the run was abandoned.
    rt.shutdown_timeout(Duration::from_secs(5));
    exit_code(code)
}

/// Runs headless mode on the current tokio runtime (which must have the time driver
/// enabled). Event lines go to `out`; a progress line goes to stderr when stderr is a
/// terminal; Ctrl-C stops the run. Returns the exit code.
pub async fn run_headless_with(args: HeadlessArgs, out: &mut impl Write) -> i32 {
    let mut stderr = std::io::stderr();
    let progress: Option<&mut dyn Write> = if stderr.is_terminal() {
        Some(&mut stderr)
    } else {
        None
    };
    run_headless_until(args, out, ctrl_c(), progress).await
}

/// Resolves on Ctrl-C (never, if the handler cannot be installed).
async fn ctrl_c() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Like [`run_headless_with`] with an explicit stop signal (resolving it acts like Ctrl-C)
/// and an optional progress writer (`None` disables the progress line).
pub async fn run_headless_until<W, F>(
    args: HeadlessArgs,
    out: &mut W,
    stop: F,
    progress: Option<&mut dyn Write>,
) -> i32
where
    W: Write + ?Sized,
    F: Future<Output = ()>,
{
    match prepare(&args) {
        Ok(prep) => {
            let service = match Service::open(
                ServiceOptions::new(&prep.data_dir),
                tokio::runtime::Handle::current(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    let e = CliError::from(e);
                    eprintln!("newskite: {e}");
                    return e.exit_code();
                }
            };
            let mut ui = Ui {
                out,
                progress: progress.map(ProgressLine::new),
            };
            let code = drive(&service, &args, &mut ui, stop).await;
            service.shutdown();
            let _ = tokio::time::timeout(STOP_TIMEOUT, service.wait_idle()).await;
            code
        }
        Err(e) => {
            eprintln!("newskite: {e}");
            e.exit_code()
        }
    }
}

/// Validated, resolved inputs.
struct Prepared {
    data_dir: PathBuf,
}

fn prepare(args: &HeadlessArgs) -> Result<Prepared, CliError> {
    if args.nzb.is_empty() {
        return Err(CliError::Usage(
            "nothing to download: pass at least one --nzb FILE".into(),
        ));
    }
    if args.server.is_none() {
        if args.user.is_some() {
            return Err(CliError::Usage("--user requires --server".into()));
        }
        if args.tls {
            return Err(CliError::Usage("--tls requires --server".into()));
        }
    }
    if args.connections == Some(0) {
        return Err(CliError::Usage("--connections must be at least 1".into()));
    }
    let data_dir = match &args.db {
        Some(d) => std::path::absolute(d)?,
        None => newskite_types::config::default_data_dir(),
    };
    Ok(Prepared { data_dir })
}

/// The effective settings for this run: `stored` with the command-line overrides applied,
/// watch folder and auto-shutdown off.
pub fn override_settings(stored: Settings, args: &HeadlessArgs) -> Result<Settings, CliError> {
    let mut s = stored;
    s.watch_enabled = false;
    s.watch_auto_start = false;
    s.auto_shutdown = false;
    if let Some(dest) = &args.dest {
        s.download_dir = std::path::absolute(dest)?;
    }
    if let Some(addr) = &args.server {
        let default = ServerConfig::default();
        s.servers = vec![ServerConfig {
            host: addr.host.clone(),
            port: addr.port_or_default(args.tls),
            tls: args.tls,
            username: args.user.clone().unwrap_or_default(),
            password: args.pass.clone().unwrap_or_default(),
            connections: args.connections.unwrap_or(default.connections),
            priority: 0,
            enabled: true,
        }];
    }
    if let Some(n) = args.connections {
        s.max_connections = n;
    }
    if let Some(r) = args.retries {
        s.retries = r;
    }
    Ok(s.normalized())
}

/// Output sinks: event lines on `out`, optional progress line.
struct Ui<'o, 'p, W: Write + ?Sized> {
    out: &'o mut W,
    progress: Option<ProgressLine<'p>>,
}

impl<W: Write + ?Sized> Ui<'_, '_, W> {
    /// Prints one event line (clearing the progress line first). Write errors are ignored:
    /// a closed stdout must not abort the downloads.
    fn line(&mut self, text: &str) {
        if let Some(p) = &mut self.progress {
            p.clear();
        }
        let _ = writeln!(self.out, "{text}");
        let _ = self.out.flush();
    }
}

/// The in-place progress line on a terminal.
struct ProgressLine<'a> {
    w: &'a mut dyn Write,
    shown: bool,
    last: Option<Instant>,
}

impl<'a> ProgressLine<'a> {
    fn new(w: &'a mut dyn Write) -> Self {
        Self {
            w,
            shown: false,
            last: None,
        }
    }

    /// Draws `text`, at most once per second.
    fn update(&mut self, text: &str) {
        if self
            .last
            .is_some_and(|t| t.elapsed() < Duration::from_millis(950))
        {
            return;
        }
        self.last = Some(Instant::now());
        let _ = write!(self.w, "\r{text:<PROGRESS_WIDTH$}");
        let _ = self.w.flush();
        self.shown = true;
    }

    fn clear(&mut self) {
        if self.shown {
            let _ = write!(self.w, "\r{:PROGRESS_WIDTH$}\r", "");
            let _ = self.w.flush();
            self.shown = false;
        }
    }
}

/// Text of the progress line.
pub fn progress_text(done: u64, total: u64, bytes_per_sec: u64) -> String {
    format!(
        "{:>3}%  {}  {} / {}",
        format::percent(done, total),
        format::speed(bytes_per_sec),
        format::bytes(done),
        format::bytes(total)
    )
}

/// A job added (or found already queued) by this invocation.
struct Tracked {
    id: i64,
    name: String,
    /// `Some(true)` done, `Some(false)` failed; `None` still pending.
    outcome: Option<bool>,
}

enum Finish {
    Resolved,
    Interrupted,
}

async fn drive<W, F>(service: &Service, args: &HeadlessArgs, ui: &mut Ui<'_, '_, W>, stop: F) -> i32
where
    W: Write + ?Sized,
    F: Future<Output = ()>,
{
    let settings = match override_settings(service.settings(), args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("newskite: {e}");
            return e.exit_code();
        }
    };
    if settings.usable_servers().is_empty() {
        eprintln!(
            "newskite: no enabled server configured: pass --server HOST:PORT or add one in the app"
        );
        return EXIT_USAGE;
    }
    service.set_runtime_settings_override(Some(settings));

    let mut rx = service.subscribe();
    let mut tracked: Vec<Tracked> = Vec::new();
    let mut add_failed = false;
    for path in &args.nzb {
        match add_one(service, path) {
            Ok(job) => {
                if tracked.iter().any(|t| t.id == job.id) {
                    continue;
                }
                ui.line(&format!("queued {}", job.name));
                tracked.push(Tracked {
                    id: job.id,
                    name: job.name,
                    outcome: None,
                });
            }
            Err(reason) => {
                ui.line(&format!("failed {}: {reason}", path.display()));
                add_failed = true;
            }
        }
    }
    if tracked.is_empty() {
        return EXIT_FAILURE;
    }

    service.start();
    let finish = watch_run(service, &mut tracked, ui, &mut rx, stop).await;
    if let Some(p) = &mut ui.progress {
        p.clear();
    }
    match finish {
        Finish::Interrupted => {
            let pending = tracked.iter().filter(|t| t.outcome.is_none()).count();
            eprintln!(
                "newskite: interrupted; {pending} job{} left queued (run again to resume)",
                if pending == 1 { "" } else { "s" }
            );
            EXIT_INTERRUPTED
        }
        Finish::Resolved => {
            if !add_failed && tracked.iter().all(|t| t.outcome == Some(true)) {
                EXIT_OK
            } else {
                EXIT_FAILURE
            }
        }
    }
}

/// Adds one NZB; a file that is already queued (an earlier interrupted run) is picked up
/// again instead of being refused.
fn add_one(service: &Service, path: &Path) -> Result<Job, String> {
    match service.add_nzb(path) {
        Ok(job) => Ok(job),
        Err(ServiceError::Duplicate(name)) => {
            let canonical = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
            service
                .jobs()
                .into_iter()
                .find(|j| j.state.is_pending() && j.source_path == canonical)
                .ok_or_else(|| format!("already queued as {name}"))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Follows the run until every tracked job has an outcome or `stop` fires.
async fn watch_run<W, F>(
    service: &Service,
    tracked: &mut [Tracked],
    ui: &mut Ui<'_, '_, W>,
    rx: &mut tokio::sync::broadcast::Receiver<ServiceEvent>,
    stop: F,
) -> Finish
where
    W: Write + ?Sized,
    F: Future<Output = ()>,
{
    let stop = stop;
    tokio::pin!(stop);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut speed = 0u64;
    loop {
        if tracked.iter().all(|t| t.outcome.is_some()) {
            return Finish::Resolved;
        }
        tokio::select! {
            biased;
            _ = &mut stop => {
                service.shutdown();
                return Finish::Interrupted;
            }
            ev = rx.recv() => match ev {
                Ok(ServiceEvent::QueueIdle) | Err(RecvError::Closed) => {
                    check_jobs(service, tracked, ui);
                    finalize_idle(service, tracked, ui);
                }
                Ok(ServiceEvent::JobsChanged) | Err(RecvError::Lagged(_)) => check_jobs(service, tracked, ui),
                Ok(ServiceEvent::Throughput { bytes_per_sec }) => speed = bytes_per_sec,
                Ok(_) => {}
            },
            _ = tick.tick() => {
                check_jobs(service, tracked, ui);
                if !service.status().running {
                    finalize_idle(service, tracked, ui);
                }
                if ui.progress.is_some() && tracked.iter().any(|t| t.outcome.is_none()) {
                    let (done, total) = byte_progress(service, tracked);
                    if let Some(p) = &mut ui.progress {
                        p.update(&progress_text(done, total, speed));
                    }
                }
            }
        }
    }
}

/// Prints outcomes of tracked jobs that reached `done` or `failed` (or vanished).
fn check_jobs<W: Write + ?Sized>(
    service: &Service,
    tracked: &mut [Tracked],
    ui: &mut Ui<'_, '_, W>,
) {
    if tracked.iter().all(|t| t.outcome.is_some()) {
        return;
    }
    let jobs: HashMap<i64, Job> = service.jobs().into_iter().map(|j| (j.id, j)).collect();
    for t in tracked.iter_mut().filter(|t| t.outcome.is_none()) {
        match jobs.get(&t.id) {
            None => {
                ui.line(&format!("failed {}: removed from the queue", t.name));
                t.outcome = Some(false);
            }
            Some(j) if j.state == JobState::Done => {
                ui.line(&format!("complete {}", t.name));
                t.outcome = Some(true);
            }
            Some(j) if j.state == JobState::Failed => {
                let reason = j
                    .error
                    .as_deref()
                    .filter(|e| !e.is_empty())
                    .unwrap_or("unknown error");
                ui.line(&format!("failed {}: {}", t.name, one_line(reason)));
                t.outcome = Some(false);
            }
            Some(_) => {}
        }
    }
}

/// The run ended while tracked jobs are still pending (no server, authentication or
/// connection failure stopped the queue): report them as failed. They stay queued in the
/// database and resume on the next run.
fn finalize_idle<W: Write + ?Sized>(
    service: &Service,
    tracked: &mut [Tracked],
    ui: &mut Ui<'_, '_, W>,
) {
    if service.status().running {
        return;
    }
    let reason = service
        .status()
        .last_error
        .map(|e| format!("not downloaded, the queue stopped: {}", one_line(&e)))
        .unwrap_or_else(|| "not downloaded, the queue stopped".to_string());
    for t in tracked.iter_mut().filter(|t| t.outcome.is_none()) {
        ui.line(&format!("failed {}: {reason}", t.name));
        t.outcome = Some(false);
    }
}

/// Decoded bytes done and total over the tracked jobs.
fn byte_progress(service: &Service, tracked: &[Tracked]) -> (u64, u64) {
    let jobs = service.jobs();
    tracked
        .iter()
        .filter_map(|t| jobs.iter().find(|j| j.id == t.id))
        .fold((0u64, 0u64), |(d, tot), j| {
            let done = if j.state == JobState::Done {
                j.bytes_total
            } else {
                j.bytes_done.min(j.bytes_total)
            };
            (d.saturating_add(done), tot.saturating_add(j.bytes_total))
        })
}

/// Keeps a reason on one output line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> Result<ServerAddr, ServerAddrError> {
        s.parse()
    }

    #[test]
    fn server_addresses_parse() {
        assert_eq!(
            addr("news.example.com:563").unwrap(),
            ServerAddr {
                host: "news.example.com".into(),
                port: Some(563)
            }
        );
        assert_eq!(
            addr("news.example.com").unwrap(),
            ServerAddr {
                host: "news.example.com".into(),
                port: None
            }
        );
        assert_eq!(
            addr(" 127.0.0.1:119 ").unwrap(),
            ServerAddr {
                host: "127.0.0.1".into(),
                port: Some(119)
            }
        );
        assert_eq!(
            addr("[::1]:119").unwrap(),
            ServerAddr {
                host: "::1".into(),
                port: Some(119)
            }
        );
        assert_eq!(
            addr("[::1]").unwrap(),
            ServerAddr {
                host: "::1".into(),
                port: None
            }
        );
        assert_eq!(
            addr("2001:db8::1").unwrap(),
            ServerAddr {
                host: "2001:db8::1".into(),
                port: None
            }
        );
        assert_eq!(addr("[2001:DB8::1]:8119").unwrap().host, "2001:db8::1");
    }

    #[test]
    fn bad_server_addresses_are_rejected() {
        assert_eq!(addr(""), Err(ServerAddrError::Empty));
        assert!(matches!(
            addr("host:"),
            Err(ServerAddrError::InvalidPort(_))
        ));
        assert!(matches!(
            addr("host:0"),
            Err(ServerAddrError::InvalidPort(_))
        ));
        assert!(matches!(
            addr("host:70000"),
            Err(ServerAddrError::InvalidPort(_))
        ));
        assert!(matches!(
            addr("host:abc"),
            Err(ServerAddrError::InvalidPort(_))
        ));
        assert!(matches!(addr(":119"), Err(ServerAddrError::InvalidHost(_))));
        assert!(matches!(
            addr("bad host:119"),
            Err(ServerAddrError::InvalidHost(_))
        ));
        assert!(addr("http://x").is_err());
        assert!(matches!(addr("[::1"), Err(ServerAddrError::InvalidIpv6(_))));
        assert!(matches!(
            addr("[nothost]:119"),
            Err(ServerAddrError::InvalidIpv6(_))
        ));
        assert!(matches!(
            addr("[::1]119"),
            Err(ServerAddrError::InvalidIpv6(_))
        ));
        assert!(matches!(addr("a:b:c"), Err(ServerAddrError::Ambiguous(_))));
    }

    #[test]
    fn default_ports_follow_tls() {
        let a = addr("h").unwrap();
        assert_eq!(a.port_or_default(true), 563);
        assert_eq!(a.port_or_default(false), 119);
        assert_eq!(addr("h:8000").unwrap().port_or_default(true), 8000);
        assert_eq!(addr("[::1]:119").unwrap().to_string(), "[::1]:119");
        assert_eq!(addr("h").unwrap().to_string(), "h");
    }

    #[test]
    fn args_parse_like_the_outline() {
        let a = parse_args([
            "newskite",
            "--headless",
            "--nzb",
            "a.nzb",
            "--nzb",
            "b.nzb",
            "--dest",
            "out",
            "--server",
            "[::1]:119",
            "--tls",
            "--user",
            "u",
            "--pass",
            "p",
            "--connections",
            "6",
            "--db",
            "dbdir",
            "--retries",
            "3",
        ])
        .unwrap();
        assert!(a.headless);
        assert_eq!(a.nzb, vec![PathBuf::from("a.nzb"), PathBuf::from("b.nzb")]);
        assert_eq!(a.dest, Some(PathBuf::from("out")));
        assert_eq!(
            a.server,
            Some(ServerAddr {
                host: "::1".into(),
                port: Some(119)
            })
        );
        assert!(a.tls);
        assert_eq!(a.user.as_deref(), Some("u"));
        assert_eq!(a.pass.as_deref(), Some("p"));
        assert_eq!(a.connections, Some(6));
        assert_eq!(a.db, Some(PathBuf::from("dbdir")));
        assert_eq!(a.retries, Some(3));
    }

    #[test]
    fn usage_errors_have_exit_code_2() {
        for argv in [
            vec!["newskite"],
            vec!["newskite", "--nzb"],
            vec!["newskite", "--nzb", "a.nzb", "--server", "a:b:c"],
            vec!["newskite", "--nzb", "a.nzb", "--connections", "0"],
            vec!["newskite", "--nzb", "a.nzb", "--retries", "x"],
            vec!["newskite", "--nzb", "a.nzb", "--bogus"],
        ] {
            let e = parse_args(argv.iter().copied()).unwrap_err();
            assert_eq!(e.exit_code(), EXIT_USAGE, "{argv:?}");
        }
        assert_eq!(
            parse_args(["newskite", "--help"]).unwrap_err().exit_code(),
            0
        );
    }

    #[test]
    fn debug_output_redacts_the_password() {
        let a = HeadlessArgs {
            pass: Some("hunter2-secret".into()),
            ..Default::default()
        };
        let s = format!("{a:?}");
        assert!(!s.contains("hunter2-secret"));
        assert!(s.contains("<redacted>"));
    }

    #[test]
    fn invalid_combinations_are_usage_errors() {
        let base = HeadlessArgs {
            nzb: vec!["a.nzb".into()],
            ..Default::default()
        };
        assert!(prepare(&base).is_ok());
        let none = HeadlessArgs::default();
        assert_eq!(
            prepare(&none).err().map(|e| e.exit_code()),
            Some(EXIT_USAGE)
        );
        let user = HeadlessArgs {
            user: Some("u".into()),
            ..base.clone()
        };
        assert_eq!(
            prepare(&user).err().map(|e| e.exit_code()),
            Some(EXIT_USAGE)
        );
        let tls = HeadlessArgs {
            tls: true,
            ..base.clone()
        };
        assert_eq!(prepare(&tls).err().map(|e| e.exit_code()), Some(EXIT_USAGE));
    }

    #[test]
    fn override_replaces_servers_and_disables_automation() {
        let stored = Settings {
            watch_enabled: true,
            watch_auto_start: true,
            auto_shutdown: true,
            max_connections: 20,
            servers: vec![ServerConfig {
                host: "stored".into(),
                password: "stored-pw".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let args = HeadlessArgs {
            nzb: vec!["a.nzb".into()],
            server: Some("[::1]".parse().unwrap()),
            user: Some("u".into()),
            pass: Some("p".into()),
            connections: Some(3),
            retries: Some(7),
            dest: Some("rel-dest".into()),
            ..Default::default()
        };
        let s = override_settings(stored.clone(), &args).unwrap();
        assert!(!s.watch_enabled && !s.watch_auto_start && !s.auto_shutdown);
        assert_eq!(s.servers.len(), 1);
        let srv = &s.servers[0];
        assert_eq!((srv.host.as_str(), srv.port, srv.tls), ("::1", 119, false));
        assert_eq!(
            (
                srv.username.as_str(),
                srv.password.as_str(),
                srv.connections
            ),
            ("u", "p", 3)
        );
        assert_eq!(s.max_connections, 3);
        assert_eq!(s.retries, 7);
        assert!(s.download_dir.is_absolute());
        assert!(s.download_dir.ends_with("rel-dest"));

        // Without --server the stored servers stay.
        let plain = HeadlessArgs {
            nzb: vec!["a.nzb".into()],
            ..Default::default()
        };
        let s = override_settings(stored.clone(), &plain).unwrap();
        assert_eq!(s.servers, stored.servers);
        assert_eq!(s.download_dir, stored.download_dir);
        assert_eq!(s.max_connections, 20);
    }

    #[test]
    fn progress_line_is_throttled_and_cleared() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut p = ProgressLine::new(&mut buf);
            p.update("first");
            p.update("second"); // within a second: skipped
            p.clear();
            p.clear(); // nothing shown: no output
        }
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("first"));
        assert!(!s.contains("second"));
        assert_eq!(s.matches('\r').count(), 3);
    }

    #[test]
    fn progress_text_formats() {
        let t = progress_text(512, 1024, 2048);
        assert!(t.starts_with(" 50%"), "{t}");
        assert!(!progress_text(0, 0, 0).contains("NaN"));
    }

    #[test]
    fn reasons_stay_on_one_line() {
        assert_eq!(one_line("a\nb\t c"), "a b c");
    }
}
