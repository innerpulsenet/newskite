# Newskite — crate contracts

Authoritative public APIs for each crate. Product spec: `APP-OUTLINE.md` (feature ids F1–F11,
acceptance criteria §12). Implementers may add items, but must not change or remove the
signatures below without updating this file. All crates: Rust 2021, MSRV 1.89, no `unsafe`
unless unavoidable and commented, `thiserror` for error enums, American English everywhere.

Stack: **egui/eframe** GUI (pure Rust, cross-platform), **tokio** async engine, **rustls**
TLS (`ring` provider, native roots + webpki-roots fallback), **rusqlite** (bundled SQLite)
store, external `par2`/`7z`/`unrar`/`unzip` binaries for post-processing (detected at
runtime; missing tools degrade gracefully). Linux (KDE) first; must still compile on
Windows/macOS (use `cfg` for platform code, never assume `/`).

Dependency graph (arrows = depends on):

```
types ← nzb ← engine ← service ← cli ← app(newskite bin)
types ← nntp ← engine           service ← app
types ← store ← service          theme ← app
types ← postproc ← service
```

## newskite-types (done)
`config::{Settings, ServerConfig, PostProcessSettings, default_download_dir, default_data_dir}`,
`events::EngineEvent`, `format::{bytes, speed, percent, fraction, duration_secs}`.

## newskite-nzb — NZB parsing + yEnc
```rust
pub mod nzb {
  pub struct Nzb { pub meta: BTreeMap<String,String>, pub files: Vec<NzbFile> }
  pub struct NzbFile { pub subject: String, pub poster: String, pub date: i64,
                       pub groups: Vec<String>, pub segments: Vec<Segment> /* sorted by number, deduped */ }
  pub struct Segment { pub number: u32, pub bytes: u64, pub message_id: String /* without <> */ }
  #[derive(thiserror::Error)] pub enum NzbError { Xml(String), Empty, Io(std::io::Error) }
  pub fn parse(bytes: &[u8]) -> Result<Nzb, NzbError>;            // no files/segments => Empty
  pub fn parse_file(path: &Path) -> Result<Nzb, NzbError>;
  impl Nzb { pub fn total_bytes(&self)->u64; pub fn segment_count(&self)->usize;
             pub fn job_name(&self, fallback_path:&Path)->String /* meta "name"/"title", else NZB file stem; sanitized */ }
  impl NzbFile { pub fn filename(&self)->String /* quoted name in subject, else best-effort; ALWAYS sanitized */;
                 pub fn total_bytes(&self)->u64 }
  pub fn sanitize_filename(raw:&str)->String; // strips dirs/.., NUL, control chars, '/' '\\' ':' etc.; never empty, never "." or ".."
}
pub mod yenc {
  pub struct YencPart { pub name: String, pub size: u64, pub part: Option<u32>, pub total: Option<u32>,
                        pub begin: u64 /* 1-based inclusive */, pub end: u64, pub data: Vec<u8>,
                        pub crc_ok: Option<bool> /* pcrc32 (or crc32 for single-part) checked when present */ }
  #[derive(thiserror::Error)] pub enum YencError { NoHeader, BadHeader(String), SizeMismatch{expected:u64, got:u64}, BadRange }
  pub fn decode(article_body: &[u8]) -> Result<YencPart, YencError>; // body = unstuffed BODY payload, may have junk before =ybegin
  pub fn decode_data(encoded: &[u8]) -> Vec<u8>;                     // raw lines, CR/LF dropped, escapes handled
  pub fn encode_data(data: &[u8], line_len: usize) -> Vec<u8>;       // escapes NUL LF CR '=' (+ leading '.', TAB/space at line edges)
  pub fn encode_article(name:&str, file_size:u64, part:u32, total:u32, begin_1based:u64, data:&[u8], line_len:usize) -> Vec<u8>;
}
```
Single-part articles without `=ypart`: begin=1, end=size. `end-begin+1` must equal `data.len()`
(else SizeMismatch). Tests: all-256-byte round trip, multipart, CRC, junk before header.

## newskite-nntp — async NNTP client + test server
```rust
pub struct NntpConnection { /* plain or TLS stream, buffered */ }
#[derive(thiserror::Error)] pub enum NntpError {
  Io(std::io::Error), Tls(String), Timeout, Connect(String),
  AuthRequired, AuthFailed(String), ArticleMissing(String /* message-id */),
  Protocol(String), InvalidMessageId(String) }
impl NntpError { pub fn is_transient(&self)->bool /* Io/Timeout/Connect/Tls/Protocol */ }
pub struct Response { pub code: u16, pub text: String }
impl NntpConnection {
  pub async fn connect(server:&ServerConfig, timeout:Duration) -> Result<Self,NntpError>; // greeting 200/201, AUTHINFO if username set
  pub fn greeting(&self) -> &str;
  pub async fn command(&mut self, line:&str) -> Result<Response,NntpError>;   // single-line response; rejects CR/LF in line
  pub async fn body(&mut self, message_id:&str) -> Result<Vec<u8>,NntpError>; // 222 multi-line, dot-unstuffed, terminator removed, lines joined with \r\n; 430/423 -> ArticleMissing; 480 -> AuthRequired
  pub async fn stat(&mut self, message_id:&str) -> Result<bool,NntpError>;
  pub async fn group(&mut self, name:&str) -> Result<Response,NntpError>;
  pub async fn date(&mut self) -> Result<Response,NntpError>;
  pub async fn quit(self);
}
/// Settings "Test": connect, auth, DATE; returns a human transcript of raw replies
/// (greeting, auth result, DATE reply). Never includes the password.
pub async fn test_server(server:&ServerConfig, timeout:Duration) -> Result<String,NntpError>;

#[cfg(feature="testkit")] pub mod testkit {
  pub struct MockServer { pub addr: SocketAddr, /* ... */ }
  pub struct MockConfig { pub username: Option<String>, pub password: Option<String>,
                          pub articles: HashMap<String /*msgid no <>*/, Vec<u8> /*raw body, NOT stuffed*/>,
                          pub fail_first_n_connections: usize, pub delay_ms: u64 }
  impl MockServer { pub async fn start(cfg: MockConfig) -> MockServer; pub fn connections_accepted(&self)->usize;
                    pub fn body_requests(&self)->usize; pub fn shutdown(self) }
}
```
Message ids are sent as `<id>`; accept ids given with or without brackets. TLS via
tokio-rustls/rustls 0.23 with `ring`, roots = rustls-native-certs plus webpki-roots.

## newskite-engine — job download
```rust
pub struct EngineOptions { pub connections: usize, pub retries: u32, pub speed_limit_bps: u64 /*0=unlimited*/,
                           pub connect_timeout: Duration, pub skip_files: HashSet<String> /* filenames already complete */ }
pub struct DownloadRequest { pub nzb: Nzb, pub dest_dir: PathBuf, pub servers: Vec<ServerConfig> /* usable, ordered */, pub options: EngineOptions }
pub struct FileReport { pub name: String, pub path: PathBuf, pub bytes: u64, pub missing: Vec<String>, pub crc_errors: usize }
pub struct JobReport { pub files: Vec<FileReport>, pub bytes_written: u64 }
#[derive(thiserror::Error)] pub enum EngineError { NoServer, AuthFailed(String), Connect(String),
   MissingArticles{ ids: Vec<String> }, Io(std::io::Error), Cancelled }
pub struct Progress { pub bytes_down: AtomicU64 /* decoded bytes, monotonic */, pub segments_done: AtomicU64 }
pub async fn download(req: DownloadRequest, events: tokio::sync::mpsc::UnboundedSender<EngineEvent>,
                      progress: Arc<Progress>, cancel: tokio_util::sync::CancellationToken) -> Result<JobReport,EngineError>;
```
Rules (F3): bounded concurrency (`connections` fetches total, pooled connections reused,
spread across servers up to each server's `connections`); offset writes at `begin-1` via a
cross-platform positional-write helper (unix `FileExt::write_at`, windows `seek_write`);
never buffer a whole file; output name from yEnc `name=` of the first decoded part, falling
back to `NzbFile::filename()`, always sanitized, always inside `dest_dir`; in-progress files
written as `<name>.nkpart` and renamed on success; missing articles collected, file reported
`FileFailed{missing}`, job returns `MissingArticles` (all ids) and leaves `.nkpart` (never
presented complete); transient errors retried `retries` times with backoff, walking servers
in order; auth failure on every server → `AuthFailed` (job doesn't proceed); speed limit is a
shared token bucket; SegmentDone emitted per segment (UI coalesces); files in `skip_files`
whose final path exists are skipped (resume). Acceptance test: 2 MiB random file, 8 yEnc
parts, testkit server with auth, real TCP, SHA-256 equal.

## newskite-store — SQLite persistence
```rust
pub enum JobState { Queued, Downloading, Done, Failed, Paused }  // as_str(): "queued"|"downloading"|"done"|"failed"|"paused"; FromStr
pub struct Job { pub id:i64, pub name:String, pub state:JobState, pub bytes_total:u64, pub bytes_done:u64,
                 pub dest_dir:PathBuf, pub source_path:PathBuf, pub created_at:i64 /*unix secs*/,
                 pub error:Option<String>, pub password:Option<String> }
pub struct NewJob { pub name:String, pub bytes_total:u64, pub dest_dir:PathBuf, pub source_path:PathBuf, pub password:Option<String> }
pub struct JobFile { pub job_id:i64, pub name:String, pub bytes_total:u64, pub bytes_done:u64, pub state:String }
pub struct Bookmark { pub id:i64, pub parent_id:Option<i64>, pub name:String, pub newsgroup:Option<String>,
                      pub color:Option<String>, pub expanded:bool, pub order:i64 }
pub struct NewBookmark { pub parent_id:Option<i64>, pub name:String, pub newsgroup:Option<String>, pub color:Option<String> }
pub struct ThroughputSample { pub ts:i64 /*unix secs*/, pub bytes_up:u64, pub bytes_down:u64 /* bytes during that second */ }
pub struct Totals { pub downloaded_bytes:u64, pub jobs_done:u64, pub jobs_pending:u64 /*queued+downloading+paused*/, pub jobs_failed:u64 }
#[derive(thiserror::Error)] pub enum StoreError { Sqlite(rusqlite::Error), Json(serde_json::Error), Io(std::io::Error), NotFound, Invalid(String) }
pub struct Store { /* Mutex<Connection>; Send + Sync */ }
impl Store {
  pub fn open(dir:&Path)->Result<Self,StoreError>;     // creates dir, newskite.sqlite3, WAL, migrations (user_version)
  pub fn open_in_memory()->Result<Self,StoreError>;
  // jobs
  pub fn insert_job(&self,j:&NewJob)->Result<Job,_>; pub fn get_job(&self,id:i64)->Result<Option<Job>,_>;
  pub fn list_jobs(&self)->Result<Vec<Job>,_> /* id ascending */; pub fn job_by_source(&self,p:&Path)->Result<Option<Job>,_>;
  pub fn set_job_state(&self,id:i64,s:JobState,error:Option<&str>)->Result<(),_>;
  pub fn set_job_progress(&self,id:i64,bytes_done:u64)->Result<(),_>;
  pub fn set_job_password(&self,id:i64,pw:Option<&str>)->Result<(),_>;
  pub fn remove_job(&self,id:i64)->Result<(),_> /* also its job_files */;
  pub fn next_runnable_job(&self)->Result<Option<Job>,_> /* queued|paused, oldest created_at then id */;
  pub fn reset_interrupted(&self)->Result<usize,_> /* downloading -> queued, at startup */;
  pub fn upsert_job_file(&self,f:&JobFile)->Result<(),_>; pub fn job_files(&self,job_id:i64)->Result<Vec<JobFile>,_>;
  // bookmarks
  pub fn list_bookmarks(&self)->Result<Vec<Bookmark>,_> /* tree order: parents before children, by order */;
  pub fn add_bookmark(&self,b:&NewBookmark)->Result<Bookmark,_>; pub fn update_bookmark(&self,b:&Bookmark)->Result<(),_>;
  pub fn remove_bookmark(&self,id:i64)->Result<(),_> /* recursive */;
  // throughput
  pub fn add_sample(&self,s:&ThroughputSample)->Result<(),_>; pub fn samples_since(&self,ts:i64)->Result<Vec<ThroughputSample>,_>;
  pub fn prune_samples_before(&self,ts:i64)->Result<usize,_>; pub fn add_downloaded_bytes(&self,n:u64)->Result<(),_> /* lifetime counter */;
  // settings
  pub fn load_settings(&self)->Result<Settings,_> /* default().normalized() when absent */; pub fn save_settings(&self,s:&Settings)->Result<(),_>;
  pub fn totals(&self)->Result<Totals,_>;
}
```

## newskite-theme — desktop color tokens
```rust
pub struct Rgb { pub r:u8, pub g:u8, pub b:u8 }  // hex(), parse_kde("r,g,b"), parse_hex, mix(other,t), relative_luminance(), contrast(other)
pub struct Tokens { pub surface, surface_raised, surface_sunken, text, text_muted, accent, accent_text, border, positive, warning, negative : Rgb }
pub enum ThemeSource { KdeGlobals, Portal, Fallback }
pub struct Theme { pub tokens:Tokens, pub dark:bool, pub source:ThemeSource, pub scheme_name:Option<String> }
pub fn resolve() -> Theme;                         // kdeglobals → portal (org.freedesktop.appearance color-scheme + accent-color) → fallback(prefers_dark?)
pub fn from_kdeglobals_str(ini:&str) -> Option<Theme>;
pub fn derive(bg:Rgb, fg:Rgb, accent:Rgb, positive:Option<Rgb>, warning:Option<Rgb>, negative:Option<Rgb>) -> Tokens;
pub fn fallback(dark:bool) -> Theme;               // the ONLY place literal palette values live
pub fn kdeglobals_path() -> Option<PathBuf>;
pub struct ThemeWatcher; impl ThemeWatcher { pub fn new()->Self; pub fn poll_changed(&mut self)->Option<Theme> /* cheap mtime check, ≤1 call/s */ }
```
KDE keys: `[General] ColorScheme, AccentColor`, `[Colors:Window] BackgroundNormal/ForegroundNormal/ForegroundInactive`,
`[Colors:View] BackgroundNormal, ForegroundPositive/ForegroundNeutral/ForegroundNegative`, `[Colors:Selection] BackgroundNormal/ForegroundNormal`.
Dark iff background luminance < 0.5. accent_text chosen for contrast ≥ 4.5 against accent.

## newskite-postproc — PAR2 / extraction / cleanup (blocking API)
```rust
pub struct Tools { pub par2:Option<PathBuf>, pub sevenz:Option<PathBuf>, pub unrar:Option<PathBuf>, pub unzip:Option<PathBuf> }
pub fn detect_tools() -> Tools;                           // PATH lookup (par2, 7z/7za/7zz, unrar, unzip)
pub struct PostOptions { pub repair:bool, pub extract:bool, pub cleanup:bool, pub password:Option<String> }
pub enum Verdict { Skipped(String), Ok, Repaired, Failed(String) }
pub struct PostReport { pub par2:Verdict, pub extract:Verdict, pub extracted_to:Option<PathBuf>, pub removed:Vec<PathBuf> }
#[derive(thiserror::Error)] pub enum PostError { Io(std::io::Error), Tool(String) }
pub fn process(dir:&Path, job_name:&str, opts:&PostOptions, tools:&Tools, log:&mut dyn FnMut(String)) -> Result<PostReport,PostError>;
```
Detection by extension and magic bytes (`.rar`/`.partNN.rar`/`.r00`, `.7z`/`.7z.001`, `.zip`);
only the first volume is passed to the tool; extract into `dir/<sanitized job_name>/` (or
straight into `dir` if it is already the job folder — implementer's call, document it);
password passed to the tool as an argument (documented); never extract outside the target
(tools invoked with explicit output dir, reject archive entries with `..`/absolute paths
where the tool allows listing). Cleanup only after a successful extraction. Commands are
spawned with argument vectors, never through a shell.

## newskite-service — queue engine shared by GUI and CLI
```rust
pub enum ServiceEvent {
  Engine { job_id:i64, event:EngineEvent }, JobLog(String),   // engine://event, engine://job
  JobsChanged, QueueIdle, Paused, Resumed, WatchImported(Vec<String>),
  Throughput { bytes_per_sec:u64 },                            // ~1 Hz
  ShutdownCountdown { secs_left:u32 }, ShutdownCanceled, Error(String) }
pub struct LogLine { pub ts:i64, pub level:LogLevel /* Info|Warn|Error */, pub text:String }
pub struct Status { pub running:bool, pub paused:bool, pub active_job:Option<i64>, pub speed_bps:u64, pub last_error:Option<String> }
pub struct ServiceOptions { pub data_dir:PathBuf, pub allow_shutdown:bool /* false in tests & CLI */ }
#[derive(thiserror::Error)] pub enum ServiceError { Store(StoreError), Nzb(NzbError), NoServer, Duplicate(String), NotFound(i64), Io(std::io::Error), Other(String) }
#[derive(Clone)] pub struct Service { /* Arc<Inner> */ }
impl Service {
  pub fn open(opts:ServiceOptions, rt:tokio::runtime::Handle) -> Result<Self,ServiceError>; // opens store, reset_interrupted, starts 1 Hz sampler + watch timer
  pub fn with_store(store:Store, opts:ServiceOptions, rt:Handle) -> Self;
  pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ServiceEvent>;
  pub fn add_nzb(&self, path:&Path) -> Result<Job,ServiceError>;   // parse first; no row on parse error; Duplicate if same source already queued/active
  pub fn scan_watch_folder(&self) -> Result<Vec<String>,ServiceError>;
  pub fn start(&self) -> bool;          // idempotent: false if a run is already active
  pub fn pause(&self); pub fn resume(&self); pub fn status(&self) -> Status;
  pub fn remove_job(&self, id:i64) -> Result<(),ServiceError>;     // cancels if active
  pub fn retry_job(&self, id:i64) -> Result<(),ServiceError>;      // failed -> queued
  pub fn set_job_password(&self, id:i64, pw:Option<String>) -> Result<(),ServiceError>;
  pub fn jobs(&self) -> Vec<Job>; pub fn totals(&self) -> Totals;
  pub fn settings(&self) -> Settings; pub fn save_settings(&self, s:Settings) -> Result<(),ServiceError>;
  pub fn bookmarks(&self) -> Vec<Bookmark>; pub fn add_bookmark(&self,b:NewBookmark)->Result<Bookmark,ServiceError>;
  pub fn update_bookmark(&self,b:&Bookmark)->Result<(),ServiceError>; pub fn remove_bookmark(&self,id:i64)->Result<(),ServiceError>;
  pub fn throughput_history(&self, secs:u64) -> Vec<ThroughputSample>;
  pub fn logs(&self) -> Vec<LogLine>;   // newest first, ring of 50
  pub fn log(&self, level:LogLevel, text:impl Into<String>); // UI/command failures go here too
  pub async fn test_server(&self, s:ServerConfig) -> Result<String,String>;
  pub fn cancel_shutdown(&self);
  pub async fn wait_idle(&self);        // resolves when no run is active
  pub fn shutdown(&self);               // stop timers, cancel active job (job left resumable)
}
```
Queue rules (F4): run processes `queued`/`paused` oldest first; `pause()` = stop after the
current job finishes (cooperative) — set state of the next jobs untouched; `resume()` clears
pause and starts a run if none active. Pausing mid-job is NOT done (outline allows the
simplification); `remove_job` on the active job cancels it. Progress persisted at most every
second and at file end. Restart resume: `reset_interrupted` + JobFile rows with state "done"
become `skip_files`. Post-process after a successful job if enabled. Idle event + optional
auto-shutdown countdown (cancellable; only when `allow_shutdown`; Linux `systemctl poweroff`,
Windows `shutdown /s /t 0`, macOS `osascript -e 'tell app "System Events" to shut down'`).
Throughput sampler stores one sample per second only while bytes moved or a run is active,
prunes >7 days. No server → job does not start, single message "no enabled server configured".

## newskite-cli
```rust
#[derive(clap::Parser)] pub struct HeadlessArgs { --nzb FILE (repeatable), --dest DIR, --server HOST:PORT, --tls,
   --user U, --pass P, --connections N, --db DIR, --retries N }
pub fn run_headless(args: HeadlessArgs) -> std::process::ExitCode; // own tokio runtime; prints "queued <name>", "complete <name>", "failed <name>: <reason>"
```
`--server` given → overrides settings for this run without persisting the password (store
settings untouched); absent → uses stored settings. Exit 0 only if every job is done.

## newskite (app binary)
`main.rs`: `newskite --headless ...` → `newskite_cli::run_headless`; otherwise GUI, with any
positional `*.nzb` paths/`file://`/`nzb://` URIs added to the queue on launch. GUI per
outline §5–§7 in `src/gui/`. Theme tokens → egui `Visuals`; **no literal colors** outside
`newskite-theme`.
