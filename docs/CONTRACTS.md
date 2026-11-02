# Newskite — crate contracts

Authoritative public APIs for each crate. Product spec: `APP-OUTLINE.md` (feature ids F1–F11,
acceptance criteria §12). Implementers may add items, but must not change or remove the
signatures below without updating this file. All crates: Rust 2021, MSRV 1.89, no `unsafe`
unless unavoidable and commented, `thiserror` for error enums, American English everywhere.

Stack: **egui/eframe** GUI (pure Rust, cross-platform), **tokio** async engine, **rustls**
TLS (`ring` provider, native roots + webpki-roots fallback), **rusqlite** (bundled SQLite)
store, native archive extraction (`zip`, `sevenz-rust2`, bundled UnRAR via `unrar_sys`)
plus external `par2` and optional `7z`/`unrar`/`unzip` fallback binaries for
post-processing (detected at runtime; missing tools degrade gracefully). Linux (KDE) first; must still compile on
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
Attribute values (`subject`, `poster`, `date`, …) are XML 1.0-normalized (`\t`/`\r\n`/`\r`/`\n` → space) and
resolve character references and the five predefined entities; unknown entities (e.g. HTML `&eacute;`),
bare `&` and invalid references are kept verbatim without disabling the rest — same policy as element text.

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
                          pub fail_first_n_connections: usize, pub delay_ms: u64 /* per-BODY server work */,
                          pub latency_ms: u64 /* one-way reply latency, in order */, pub bandwidth_bps: u64 /* paced writes, 0 = unlimited */ }
  impl MockServer { pub async fn start(cfg: MockConfig) -> MockServer; pub fn connections_accepted(&self)->usize;
                    pub fn body_requests(&self)->usize; pub fn shutdown(self) }
}
```
Message ids are sent as `<id>`; accept ids given with or without brackets. TLS via
tokio-rustls/rustls 0.23 with `ring`, roots = rustls-native-certs plus webpki-roots.

Implemented additions (additive): `NntpConnection::{address, is_tls, is_broken, read_timeout,
set_read_timeout}`; `pub fn normalize_message_id(&str) -> Result<String,NntpError>` (returns
`<id>`); consts `MAX_ARTICLE_BYTES` (64 MiB cap per body → `Protocol`) and
`DEFAULT_READ_TIMEOUT` (60 s per-read inactivity timeout → `Timeout`); `Response: Display`
(`"211 text"`); `testkit::MockConfig: Default + Clone`; `testkit::dot_stuff(&[u8]) -> Vec<u8>`.
Pipelining (additive): `send_body(&mut self, id) -> Result<(),_>` sends `BODY` without reading
the reply (allowed while a body block is still being read); `begin_body(&mut self, id) ->
Result<(),_>` reads the oldest outstanding status line (Ok = 222, errors as `body()`);
`finish_body(&mut self) -> Result<Vec<u8>,_>` reads the announced block; `outstanding_bodies()
-> usize`. `body()` is the three in sequence. `command()`/`stat()`/… are refused (`Protocol`,
connection not broken) while a reply is outstanding; a 480 with other replies outstanding
breaks the session (`Connect`, transient) instead of replaying. Bodies are read chunk by
chunk from the socket buffer (no per-line reads or timers); the body buffer is pre-sized from
the previous body on the connection.
Semantics: `body()` terminates **every** line with `\r\n` (so a CRLF body round-trips exactly;
bare-LF lines are normalized). After an I/O error, timeout or malformed reply the connection
is `is_broken()` and further commands fail with `Protocol` — drop it and reconnect. `connect`'s
timeout covers TCP + TLS + greeting + auth; refused/DNS → `Connect`. `command()` with CR/LF/NUL
→ `Protocol`. `group()` returns 211/411 replies as-is. `test_server` transcript lines are
`<- reply` / `-> command` with AUTHINFO arguments shown as `***` (username and password also
redacted if the server echoes them); a non-111 DATE reply is reported, not an error.
MockServer: `fail_first_n_connections` still counts toward `connections_accepted`;
`body_requests` counts every BODY (including ones answered 480); AUTHINFO USER when no
credentials are configured answers 281; also answers `MODE READER` (200).
Reply classification: AUTHINFO 400, or 481/482/502 whose text mentions a connection or rate
limit ("too many", "connections", "limit", "temporar", "try again", "overload", "busy"), →
transient `Connect` (an account connection cap must not disable the server); other AUTHINFO
4xx/5xx → `AuthFailed`. `body()` beyond 222/430/423/480: 400 or a limit reply → `Connect`
(session broken); 481/482/483/502 → `AuthFailed`; any other 4xx/5xx (451 takedown, 500/501/503)
→ `ArticleMissing` for that server, so the engine fails over instead of retrying until the job
aborts. `AuthFailed`/`Connect`/`Protocol` texts built from server replies are redacted the same
way as transcripts (password, and usernames of 3+ characters, shown as `***`).

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
pub struct ConnectionPool; impl ConnectionPool { pub fn new()->Self; pub fn idle_count(&self)->usize; pub fn close_idle(&self) }
pub const POOL_IDLE_MAX: Duration; // 90 s: older idle connections are closed, not reused
pub async fn download_with_pool(req, pool: Arc<ConnectionPool>, events, progress, cancel) -> Result<JobReport,EngineError>;
```
Netcode: on the primary (first usable) server a worker sends the next segment's `BODY` as soon
as the current article's 222 line arrives (one request ahead per connection; never before a
file's output is decided, so a resumed complete file still costs one article); decode and part
checks run on the blocking pool; `download()` closes its connections at the end while
`download_with_pool` hands them to the pool (the service keeps one pool per run and closes it
when the queue goes idle or at shutdown; a pooled connection the server closed is replaced
silently). Connect failures (refused, TLS, 400/502 at greeting or login) put the server in a
cooldown every worker honors (same schedule as the retry backoff, ±25 % jitter).
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
whose final path exists are skipped (resume; see "Resume matching" below). Acceptance test:
2 MiB random file, 8 yEnc parts, testkit server with auth, real TCP, SHA-256 equal.

Implemented additions (additive): `pub const PART_EXTENSION = "nkpart"`, `BACKOFF_BASE` (200 ms),
`BACKOFF_MAX` (5 s), `pub fn backoff_delay(attempt:u32)->Duration`; `EngineOptions: Default + Clone +
Debug + PartialEq` (8 connections, 2 retries, unlimited, 30 s connect timeout); `DownloadRequest: Clone`
with a hand-written `Debug` that omits passwords; `FileReport`/`JobReport: Clone + Debug + PartialEq`;
`Progress: Default` plus `new()`, `bytes_down()`, `segments_done()` getters. Semantics: event `name`s are
the resolved output name; `FileStarted` is emitted when the name is decided (first decoded part), before
any `SegmentDone` of that file; `SegmentDone.done` counts every processed segment of the file (missing
ones included) so it reaches `total`; files skipped for resume emit only a `Log` line and appear in
`JobReport` with their on-disk size; `Progress::bytes_down` counts decoded bytes stored this run,
`segments_done` counts processed segments. Missing/undecodable articles are tried once on every other
usable server. A segment that exhausts `retries` on transient errors stops the job with `Connect`
(`.nkpart` files kept), except during fail-over: once a server answered missing/undecodable/refused
for the article, a fallback that exhausts `retries` is treated as not having it, so the article is
reported in `MissingArticles` instead of the job aborting. Name collisions within a job or with an
existing file that is not a completed earlier output get `"<stem> (2).<ext>"`, compared
case-insensitively. Final length = yEnc `size=` of the first accepted part (never less than the
highest byte written); gaps are logged. The file data and the rename are flushed to disk
(`sync_data`, then a directory fsync on Unix) before `FileDone`.

Part checks (hostile or inconsistent articles): a decoded part is accepted only when its `size=` is
at most the NZB's declared bytes for the file × 1.25 + 1 MiB (16 MiB per segment without `bytes`),
and its `size=`/`name=` equal those of the file's first accepted part (so `begin`/`end` are bounded
too). A refused part is never written and is handled like an undecodable article (next server,
then missing). Part numbers are not compared with segment numbers.

Resume matching: a file is skipped without any fetch when its NZB-derived label is in `skip_files`,
exists in `dest_dir`, and no other file of the job derives the same label. Otherwise the first
decoded part decides: skipped when a `skip_files` entry equal to the sanitized yEnc name, or a
numbered `"<stem> (n).<ext>"` variant of it, is unclaimed, has the part's `size=` as its length
and holds the part's bytes at its offset. Known limit: the label shortcut trusts the name, so a
file whose subject names a file that another NZB entry's yEnc name produced in an earlier run can
still be taken for it.

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
  pub fn open(dir:&Path)->Result<Self,StoreError>;     // creates dir, newskite.sqlite3, WAL, migrations (user_version);
                                                       // single instance: exclusive advisory lock on dir/newskite.lock
                                                       // (LOCK_FILE_NAME) held until drop, a second open of the same dir
                                                       // -> Invalid("another Newskite instance is already using ...");
                                                       // Unix: dirs it creates 0700, db/-wal/-shm/lock files 0600
  pub fn open_in_memory()->Result<Self,StoreError>;
  // jobs
  pub fn insert_job(&self,j:&NewJob)->Result<Job,_>; pub fn get_job(&self,id:i64)->Result<Option<Job>,_>;
  pub fn list_jobs(&self)->Result<Vec<Job>,_> /* id ascending */; pub fn job_by_source(&self,p:&Path)->Result<Option<Job>,_> /* newest, any state */;
  pub fn pending_job_by_source(&self,p:&Path)->Result<Option<Job>,_> /* oldest queued|downloading|paused job for p */;
  pub fn set_job_state(&self,id:i64,s:JobState,error:Option<&str>)->Result<(),_>;
  pub fn set_job_progress(&self,id:i64,bytes_done:u64)->Result<(),_>;
  pub fn set_job_password(&self,id:i64,pw:Option<&str>)->Result<(),_>;
  pub fn remove_job(&self,id:i64)->Result<(),_> /* also its job_files */;
  pub fn next_runnable_job(&self)->Result<Option<Job>,_> /* queued only (paused = held), queue order: position, created_at, id */;
  pub fn next_runnable_job_among(&self,ids:&[i64])->Result<Option<Job>,_> /* additive: same, restricted to ids; empty ids -> None */;
  pub fn queue_order(&self)->Result<Vec<i64>,_> /* pending ids (queued|downloading|paused) in queue order */;
  pub fn set_queue_order(&self,ids:&[i64])->Result<(),_> /* positions 1.. in the given order */;
  pub fn reset_interrupted(&self)->Result<usize,_> /* downloading -> queued, at startup (the lock makes the opener the only owner) */;
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
  pub fn settings_backups(&self)->Result<Vec<(String,String)>,_> /* unreadable documents kept by save_settings, oldest first */;
  // watch-folder dedupe (schema v2 table seen_sources; rows survive remove_job)
  pub fn mark_source_seen(&self,p:&Path,size:u64,mtime_unix_nanos:i64)->Result<(),_> /* upsert per path */;
  pub fn source_seen(&self,p:&Path,size:u64,mtime_unix_nanos:i64)->Result<bool,_> /* exact path + fingerprint */;
  pub fn totals(&self)->Result<Totals,_>;
}
```
Implementation additions (additive): `pub const DB_FILE_NAME = "newskite.sqlite3"`, `pub const SCHEMA_VERSION:i64`,
`Store::path()->Option<&Path>`, `Store::insert_job_at(&NewJob, created_at:i64)` (explicit timestamp),
`JobState::{ALL, is_pending()}` + `Display`, `FromStr<Err=ParseJobStateError>`. Semantics: methods addressing a
row by id (`set_job_*`, `remove_job`, `update_bookmark`, `remove_bookmark`, `upsert_job_file` for a missing job)
return `NotFound` when it doesn't exist; `add_bookmark`/`update_bookmark` return `Invalid` for a missing parent or a
parent cycle; `job_by_source` returns the newest matching job; `samples_since` is inclusive, `prune_samples_before`
strict; counters/throughput saturate at `i64::MAX`; settings JSON that is syntactically corrupt (or has a
wrong-typed field) yields `Json` (missing/unknown fields load fine). `save_settings` over such an unreadable
document first keeps it verbatim under `settings.unreadable-<unix secs>` (same transaction, once per distinct
text), so saving defaults after a load failure never loses the server list. `Job`/`NewJob` `Debug` redact the password. A DB with a newer
`user_version` is refused with `Invalid`.

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

Implemented additions (additive): `Rgb: Display + FromStr` (hex or `r,g,b`, `ParseColorError`),
`Tokens::iter() -> impl Iterator<Item=(&'static str, Rgb)>` (diagnostics),
`resolve_with_hint(prefers_dark: Option<bool>) -> Theme` (GUI passes the toolkit's detected
system theme; used when the portal has no preference and for the fallback palette),
`from_kdeglobals_path(&Path) -> Result<Option<Theme>, ThemeError>`,
`ThemeWatcher::{with_hint, with_path, current, set_min_interval, set_prefers_dark_hint}` and
`impl Default for ThemeWatcher`. Cargo feature `portal` (default on; Linux only, zbus 5 blocking,
300 ms cap on a helper thread). Dark detection refinement: dark iff background is darker than
foreground (luminance differs by > 0.05); only for near-equal pairs does it use luminance < 0.5.
Status colors (positive/warning/negative) are nudged toward black/white until ≥ 3:1 on surface.

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

Implementation notes (additive API, see crate docs in `src/lib.rs`): extraction is native
(in-process zip / 7z / RAR, works with `Tools::none()`); an external tool is used only when
the native backend reports an archive as unsupported (spanned `.zNN` zip, unknown codec),
and such a set fails with an "install ..." reason when no tool is found. The native writer
sanitizes every entry path, never follows or creates links (link entries are skipped and
logged; an escaping one fails the set) and reports progress every 5% through `log`.
Extraction always goes
to `dir/<extraction_dir_name(job_name)>/` (`<name>.extracted` if a file has that name).
Each set is extracted into a fresh private `.newskite-staging-*` folder inside it; on
success *and* failure, links leaving the staging folder (or with absolute destinations) are
deleted, the output is merged into the extraction folder without following existing links
(same-name files/links are replaced), the staging folder is removed, and the extraction
folder is rescanned for escaping links. A deleted link makes the set `Failed`. So a link
planted by a failed set or an earlier attempt can never be written through by a later set
or a retry.
PAR2 verify always runs when `.par2` files and `par2` exist; `repair:false` turns a damaged
set into `Failed`; a failed PAR2 step skips extraction. Cleanup runs only when the extract
verdict is `Ok` (archive volumes; plus `.par2` files and par2 `name.1` backups when no set
was skipped). Extra public items: `Tools::{none, can_extract, summary}`, `detect_tools_in`,
`find_tool`, `PostOptions::from_settings(&PostProcessSettings, Option<String>)` + `Default`
+ password-redacting `Debug`, `Verdict::{is_failed, is_success}` + `Display`,
`extraction_dir_name`, `is_unsafe_entry`, `parse_7z_listing`, `parse_bare_listing`,
`sniff_bytes`, `sniff_file`, `ArchiveKind`, `Magic`. Run post-processing jobs one at a
time (par2 uses every core).

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
  pub fn open(opts:ServiceOptions, rt:tokio::runtime::Handle) -> Result<Self,ServiceError>; // opens store (fails while another instance holds the data dir), reset_interrupted, starts 1 Hz sampler + watch timer
  pub fn with_store(store:Store, opts:ServiceOptions, rt:Handle) -> Self;
  pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ServiceEvent>;
  pub fn add_nzb(&self, path:&Path) -> Result<Job,ServiceError>;   // parse first; no row on parse error; Duplicate if same source already queued/active
  pub fn scan_watch_folder(&self) -> Result<Vec<String>,ServiceError>;
  pub fn start(&self) -> bool;          // idempotent: false if a run is already active
  pub fn start_jobs(&self, ids:&[i64]) -> bool; // additive: like start(), but the run processes only these job ids (oldest first) and leaves every other queued job untouched
  pub fn pause(&self); pub fn resume(&self); pub fn status(&self) -> Status;
  pub fn remove_job(&self, id:i64) -> Result<(),ServiceError>;     // cancels if active; records the source NZB as seen (watch folder)
  pub fn retry_job(&self, id:i64) -> Result<(),ServiceError>;      // failed -> queued; Duplicate if another job for the same source is pending
  pub fn set_job_password(&self, id:i64, pw:Option<String>) -> Result<(),ServiceError>;
  pub fn jobs(&self) -> Vec<Job>; pub fn totals(&self) -> Totals;
  pub fn settings(&self) -> Settings; pub fn save_settings(&self, s:Settings) -> Result<(),ServiceError>;
  pub fn settings_load_error(&self) -> Option<String>; // additive: why the stored document is unreadable (settings() then returns defaults)
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
Queue rules (F4): run processes `queued` jobs in queue order (`paused` = held by the user,
skipped); `pause()` = stop after the current job finishes (cooperative) — set state of the
next jobs untouched; `pause_now()` also interrupts the current job back to `queued`;
`resume()` clears pause and starts a run if none active; `remove_job` on the active job
cancels it. See "NZB-focus pass" below for the per-job commands. Progress persisted at most every
second and at file end. Restart resume: `reset_interrupted` + JobFile rows with state "done"
become `skip_files`, except a row whose file is missing or whose on-disk length differs from
`bytes_total` (a shorter one, as lost writes leave it, is deleted so it is fetched again under its
own name; any other mismatch is left alone and the engine picks a free name). Post-process after a successful job if enabled. Idle event + optional
auto-shutdown countdown (cancellable; only when `allow_shutdown`; Linux `systemctl poweroff`,
Windows `shutdown /s /t 0`, macOS `osascript -e 'tell app "System Events" to shut down'`).
Throughput sampler stores one sample per second only while bytes moved or a run is active,
prunes >7 days. Bytes not yet sampled are flushed (added to the current second's sample and the
lifetime total, no `Throughput` event) when a run ends (before `QueueIdle`), in `shutdown()` and
when the sampler stops, so a run shorter than one tick is still counted. No server → job does not start, single message "no enabled server configured".

Implemented additions (additive): consts `LOG_CAPACITY` (50), `EVENT_CAPACITY` (1024),
`SAMPLE_RETENTION_SECS` (7 days), `TEST_SERVER_TIMEOUT` (10 s), `CONNECT_TIMEOUT` (30 s),
`NO_SERVER_MESSAGE`; `ServiceOptions::new(data_dir)` (allow_shutdown=false) + `Clone/Debug/PartialEq`;
`pub type ShutdownHook = Arc<dyn Fn()->Result<(),String>+Send+Sync>` and
`Service::set_shutdown_hook(&self, ShutdownHook)` (replaces the power-off command; still only used
when `allow_shutdown`); `pub fn shutdown_command()->Option<(&str,&[&str])>`; `Service::job_files(id)`,
`Service::options()`; `LogLevel::as_str` + `Display`; `ServiceEvent: Clone+Debug+PartialEq+Eq`,
`Status: Default`; re-exports `Store, StoreError, Job, JobFile, JobState, NewBookmark, Bookmark,
ThroughputSample, Totals`. Semantics: `with_store` also resets interrupted jobs and starts the timers;
every log line is broadcast as `JobLog` (errors additionally as `Error`) and mirrored to `tracing`;
`QueueIdle` is emitted whenever a run ends (drained, paused, stopped), after `running` is false;
`Paused` is emitted by `pause()` itself (acknowledgment); `start()` clears a pending pause (emits
`Resumed`), clears `last_error` and cancels a running shutdown countdown; `start()` after
`shutdown()` returns false. Destination folder is fixed at import: `<download_dir>/<job name>`,
`"<name> (N)"` (lowest free N ≥ 2) if any job in the store, whatever its state, already uses that
folder (compared case-insensitively, for Windows/macOS) or the path already exists on disk, so two
jobs never share a folder (post-processing works on the whole folder). Imports are serialized: the
duplicate checks, destination choice and insert run under one lock, so concurrent `add_nzb` /
watch scans never insert the same NZB twice. `add_nzb` stores the canonical path and dedupes on
it (pending states only, checked across every job for that path via `pending_job_by_source`);
`retry_job` refuses with `Duplicate` while another job for the same source is pending, so at most
one pending job exists per source NZB. Only one process (GUI or headless) may open a data
directory at a time; the headless CLI on the default data directory fails while the GUI runs
(use `--db DIR`). Watch folder: non-recursive, `*.nzb` case-insensitive, dedupe against
any job state and against `seen_sources` (path + size + mtime, recorded on every watch import and by
`remove_job`), so a removed job is not imported again while the same file stays in the folder; a
file replaced under the same name (new size or mtime) is a new NZB; manual scans work whenever a folder is set; the timer scans immediately and then
every `watch_interval_secs` while `watch_enabled`, re-reading settings each second. Outcomes:
success → post-process if enabled; a `Failed` PAR2 or extract verdict (or `PostError`) marks the
job `failed` with the reason (set a password and `retry_job`; done files are skipped on retry);
`MissingArticles` → failed `"N missing articles: id1, ..., id5, ... (+K more)"`, run continues;
`AuthFailed`/`Connect` → failed and the run stops; `Io` → failed, run continues; shutdown →
job back to `queued`. Auto-shutdown countdown only after a run that processed ≥1 job drains the
queue. Settings saved mid-job apply to the next job. Load failure of settings → defaults + one
logged error; `save_settings` then keeps the unreadable document as a backup row and logs a warning
naming its key.

Runtime settings override (additive, for the headless CLI):
`Service::set_runtime_settings_override(&self, Option<Settings>)` and
`Service::has_runtime_settings_override(&self) -> bool`. While set (normalized on set), imports
(destination folder), runs, the watch timer and auto-shutdown use the override instead of the
stored document; it is never written to the database nor logged. `Service::settings()` keeps
returning the **stored** document, so saving what it returns never persists override values.

## newskite-cli
```rust
#[derive(clap::Parser)] pub struct HeadlessArgs { --nzb FILE (repeatable), --dest DIR, --server HOST:PORT, --tls,
   --user U, --pass P, --connections N, --db DIR, --retries N }
pub fn run_headless(args: HeadlessArgs) -> std::process::ExitCode; // own tokio runtime; prints "queued <name>", "complete <name>", "failed <name>: <reason>"
```
`--server` given → overrides settings for this run without persisting the password (store
settings untouched); absent → uses stored settings. Exit 0 only if every job is done.

Implemented additions (additive): `HeadlessArgs` also has a hidden `--headless` flag (accepted
harmlessly), `--pass` falls back to env `NEWSKITE_PASS` (value hidden in help), `--server` is
`Option<ServerAddr>` (`host`, `host:port`, `[v6]`, `[v6]:port`, bare v6; default port 563 with
`--tls`, 119 without), `--connections` 1-500, `--retries` 0-100; `Debug` redacts the password.
```rust
pub const EXIT_OK:i32=0; EXIT_FAILURE:i32=1; EXIT_USAGE:i32=2; EXIT_INTERRUPTED:i32=130;
pub const DEFAULT_PORT:u16=119; DEFAULT_TLS_PORT:u16=563; PASSWORD_ENV:&str="NEWSKITE_PASS";
pub struct ServerAddr { pub host:String, pub port:Option<u16> } // FromStr, Display, port_or_default(tls)
pub enum ServerAddrError { Empty, InvalidHost(String), InvalidPort(String), InvalidIpv6(String), Ambiguous(String) }
pub enum CliError { Usage(String), Service(ServiceError), Io(io::Error) } // exit_code()
pub fn parse_args<I,T>(args:I) -> Result<HeadlessArgs, clap::Error>;   // argv incl. program name
pub fn run_headless_from_args<I,T>(args:I) -> ExitCode;                 // parse + run; clap errors exit 2 (help 0)
pub async fn run_headless_with(args:HeadlessArgs, out:&mut impl Write) -> i32; // current runtime, Ctrl-C, stderr progress if TTY
pub async fn run_headless_until<W:Write+?Sized,F:Future<Output=()>>(args, out:&mut W, stop:F, progress:Option<&mut dyn Write>) -> i32;
pub fn override_settings(stored:Settings, args:&HeadlessArgs) -> Result<Settings,CliError>;
pub fn progress_text(done:u64, total:u64, bytes_per_sec:u64) -> String;
```
Semantics: the run always sets a runtime settings override (stored settings + flags; watch
folder, watch auto-start and auto-shutdown off). No usable server after overrides → exit 2
before anything is queued. `--user`/`--tls` without `--server` → exit 2. Each `--nzb` prints
`queued <name>` or `failed <path>: <reason>` (a file already pending in the database, e.g. after
Ctrl-C, is picked up again and printed as `queued`; the same file twice is one job). The run is
started with `Service::start_jobs` for exactly those jobs, so any other queued job in the database
(the app's backlog on the default data directory) is neither downloaded nor sent to the
`--server` override; it stays queued. Then
`complete <name>` / `failed <name>: <reason>` per job; if the queue stops (auth/connect failure,
no server) while jobs are still pending they are printed `failed <name>: not downloaded, the
queue stopped: <last error>` (they stay queued in the database). When every job of this
invocation has an outcome the service is shut down (any other active job goes back to queued).
Ctrl-C → service shutdown (active job left resumable), exit 130.

## newskite (app binary)
`main.rs`: `newskite --headless ...` → `newskite_cli::run_headless`; otherwise GUI, with any
positional `*.nzb` paths/`file://`/`nzb://` URIs added to the queue on launch. GUI per
outline §5–§7 in `src/gui/`. Theme tokens → egui `Visuals`; **no literal colors** outside
`newskite-theme`, except the innerpulse brand token sets in `gui/brand_tokens.rs` (converted
only in `gui/theme.rs`).

Implemented (additive): the crate is a library `newskite_app` plus the thin `newskite` binary,
so the GUI can be driven headlessly in tests. **egui/eframe/egui_extras/egui_kittest 0.33.3**
(not 0.37: 0.34+ need rustc 1.92/1.95, above the workspace MSRV 1.89 and the installed 1.93);
eframe uses the `glow` renderer, kittest snapshots use `wgpu`. Public items:
`launch::{parse_args, parse_item, percent_decode, Mode, LaunchItem, USAGE}` (`--headless`
anywhere → CLI with argv0 `"newskite --headless"`; `file://` (local only), `nzb:///path` and
`nzb://file://…` are imported; `nzb://http…`, `nzblnk:` and web links are logged as
unsupported — no HTTP fetching yet); `gui::{NewskiteApp, AppConfig{theme, notifications,
forward_events}, ThemeMode::{Desktop, Fixed(Theme)}, Action, UiMsg, Tab}` with
`NewskiteApp::{new(ctx, service, rt_handle, config), ui(ctx), push_action, sender, service,
reload_all, reload_jobs, drain_messages, apply_actions}` and pub fields `theme`, `palette`,
`data`, `view`. `gui::theme::{to_color32, parse_user_color, visuals, apply, Palette}` is the
only place colors are converted (token `Rgb` → `Color32`; shadow = token color with alpha);
a unit test greps every other GUI source for color constructors. The GUI opens the service
with `allow_shutdown: true`. Behavior notes: the queue connection pill says `Connected` only
while a run is active (`Ready` when idle — no persistent connection exists); the graph draws
the last 24 h of stored samples (downsampled, dashed) and the live 10 min series (accent);
NZB files dropped on the window are imported; desktop notification on `QueueIdle` (Linux/BSD,
notify-rust) only when jobs finished since the last one.

Branding (innerpulse kit; additive). `AppConfig` gained `appearance_file: Option<PathBuf>`
(None = in memory only) and `reduced_motion: Option<bool>` (None = detect); construct it with
`..AppConfig::default()`. New `Action`s: `OpenAbout`, `CloseAbout`, `OpenUrl(String)`,
`SetAppearance(Appearance)`. New pub fields/items: `NewskiteApp::{appearance, reduced_motion,
set_appearance, working_tempo}`, `Data::{articles_fetched, articles_missing,
settings_load_error}`, `ViewState::{about_open, masthead_mark, about_mark}`,
`SettingsDraft::{appearance, load_error}`, `Palette::{faint, backdrop, well, mark, action,
action_hover, action_text, focus, fade}`. Modules: `gui::brand` (the six-bar mark: rest
heights, `draw_in_heights`, `beat_heights` copied from `pulse.mjs`, `paint_mark` with
whole-pixel bars, `MarkClock`), `gui::brand_tokens` (ink/paper `BrandSet`s, `EMBER`,
`ink()`, `paper()`, `brand_set_of`), `gui::appearance` (`Appearance::{FollowDesktop, Ink,
Paper}`, JSON at `<data dir>/appearance.json`, `{"theme":"follow-desktop"|"innerpulse-ink"|
"innerpulse-paper"}`; Settings belongs to newskite-types, so the choice is a GUI-only file;
missing/unreadable = follow desktop, the default per F10; applied on Save), `gui::motion`
(reduced motion: `NEWSKITE_REDUCED_MOTION=1|0` overrides, else kdeglobals `[KDE]
AnimationDurationFactor` 0 = reduced; re-read every 5 s), `gui::icons` (kit UI icons from
`assets/brand/kit/ui-icons/` plus Back/Home/List/Newsgroups/Open NZB/folder/key/chevron-up
drawn in the kit grammar, all rasterized by the `egui_extras` svg loader at the target's
physical size and tinted; the product wordmark cropped to its glyph box), `gui::fonts`
(Instrument Sans 400/600 and JetBrains Mono 400/600 static instances embedded from
`assets/brand/fonts/static/`, SIL OFL 1.1 in `assets/brand/fonts/OFL.txt`; egui's defaults
stay as fallbacks; `ui()` skips a frame until the families are live), `gui::about`.
Behavior: the masthead mark draws in once on start and beats only while working (1.2 s while
bytes move, 2.4 s while a job is active without traffic; a started pass always finishes);
the About mark (64 px) draws in then beats every 4.5 s; under reduced motion both rest.
Repaints for motion are requested only while a mark moves (25 ms frames; between passes a
single wake-up for the next one). Ember appears only as the mark, focus rings and the one
filled button per view (Queue › Start, Settings › Save; a disabled primary drops to the
outline). Status is an outlined chip with the kit glyph and words (job states: queued idle,
downloading live in the accent, done ok, failed err, paused warn). Progress is a segment track
(≤ 40 near-square cells, each an equal slice of the job). NNTP Activity shows `Articles` (count
of `SegmentDone` this session) and `Missing` (sum of `FileFailed.missing`). The window icon is
`assets/brand/app-icon-256.png`. If `Service::open` fails because another process holds the
data directory lock, the binary shows a native "Newskite is already open" message naming the
NZB arguments that were not queued and exits 1 (no hand-off to the running window yet).

Review fixes (additive). The "already open" and fatal start-up messages are drawn in a small
eframe window of the app's own (rfd's message dialog needs `zenity`, which KDE does not
install); only when no window can open (eframe itself failed) is `kdialog` or `zenity` tried,
and the .deb recommends `zenity | kdialog` for that case. The app icon is the stock innerpulse
app icon at every size (`packaging/icons/make-icons.mjs` installs the kit's renders; no accent
in the icon). The .deb and AppImage install `fonts-OFL.txt` and a DEP-5
`packaging/debian/copyright`. New pub items: `launch::{percent_decode_bytes, home_dir,
expand_tilde, path_from_text}`; `UiMsg::{NzbImported, WatchScanned}` (NZB imports and manual
watch scans run on `spawn_blocking`, never in the frame); `OpenNzbState::importing`;
`NewskiteApp::{last_notice, reload_history}`; `gui::events::{Pending, SegmentTally,
SharedPending}` (the forwarder folds `SegmentDone` per job and stamps `Throughput` on arrival,
so a stalled UI thread never grows a queue); `statusbar::{Pill::Pausing, short_cause}` (pause is
cooperative: the pill keeps the speed until the run stops; the error chip shows the elided
cause); `widgets::error_line` (every error line carries the err glyph); `Palette::{chart,
series}` and `brand_tokens::{CHART_LIGHT, CHART_DARK, chart_set, CHART_SERIES_SLOT}` (the live
series is chart-3 on the brand themes, the desktop accent otherwise);
`sidebar::{LABEL_SLOTS, slot_name, label_color}` (bookmark swatches are chart slots stored as
`chart-N`, drawn as squares; typed hex still works); `docks::{history_buckets,
grid_card_height}`; `queue::table_widths`; `SettingsDraft::edited`. Behavior: the history trace
is 160 nine-minute buckets of the last 24 h (idle time at zero), re-read every minute and after
each run; the empty queue's article grid shows unlit wells only; dock cards have exact heights
and scroll inside; the queue table shrinks Name/Progress to fit and scrolls sideways only below
its minimum; settings row ids are unique per process and test results apply only to a row
waiting on one; Save applies the appearance only when the settings save succeeds; validation
rejects `host:port`, relative folders, and runs before normalization; `~` is expanded in the
folders and the Open NZB field, which also accepts pasted `file://` URIs. Engine: per-article
"bad article" log lines (undecodable, foreign, invalid id) are capped at 5 per job plus one
summary line. Theme: with the portal in use, `ThemeWatcher` re-resolves on a helper thread and
keeps a portal theme through up to two unanswered refreshes. GUI snapshot tests compare against
the PNGs in `tests/snapshots/` (kittest, threshold 0.6, a small pixel allowance for clock
text); `UPDATE_SNAPSHOTS=1` rewrites them, and failures leave `*.new.png`/`*.diff.png` (ignored by git).

NZB-focus pass (UX overhaul). Search, browsing and bookmarks are out of the product: the GUI
has no sidebar, no Search/Latest tabs and no My List/Newsgroups/Auto Search buttons (the
store keeps its bookmark table and API for database compatibility; nothing in the GUI uses
them). Store: schema v3 adds `jobs.position` (0 until the user reorders; new jobs go last
after a reorder); `next_runnable_job*` take `queued` only, in queue order; `queue_order`,
`set_queue_order`. Service (additive): `pause_now()`; `pause_job(id)` (queued -> paused; the
active job is interrupted and becomes paused, the run moves on; done/failed refused);
`start_job(id)` (paused/queued -> queued, failed retried first; starts a run limited to that
job, or adds it to an active limited run; clears a pause); `move_job(id, JobMove::{Top, Up,
Down, Bottom})`; `queue_order()`; `clear_history() -> usize` (removes done/failed rows, NZBs
marked seen); `Status::run_scope: Option<Vec<i64>>` (the ids an active limited run takes).
An interrupted job is not counted as processed (no "queue finished" or auto-shutdown for a
run that only interrupted). GUI: toolbar = masthead, Add NZB, the queue control (Start /
Pause / Resume by state; Pause is `pause_now`), Scan Folder (only with a watch folder),
Downloads, Settings. Center = tabs `Queue (n)` (pending jobs in queue order: #, name, state,
size, progress, controls) and `History (n)` (done/failed, newest first, Clear history with
inline confirmation), a name filter (Ctrl+F). Row controls: a start/pause toggle
(`queue::toggle_action`: downloading -> pause; queued in the active run -> hold; queued
outside any run or held -> start on its own; failed -> retry), move up, a row menu (also on
right click: pause/hold/resume/download now, move to top/up/down/bottom, open folder,
archive password, remove) and remove. A click selects a row; keys act on the selection
(Space toggle, Delete asks to remove, Alt+Up/Down move, Esc clears). Docks = Throughput
(speed, bytes left and time left from the measured speed only, live + 24 h graph), Details
(the selected job's facts and per-file progress from `job_files`, or with no selection the
server, connection slots and `Articles`/`Missing`/`Downloaded` tiles) and Log (All /
Warnings / Errors filter); three across from 900 px, one scrolling column below. Status
bar: free space on the download folder's filesystem (`gui::disk::free_bytes`, statvfs via
rustix; warning color below 2 GiB or below what the queue still needs) and the lifetime
total at the left, the state pill and credit at the right.

Drag and drop and cell motif (follow-up). winit 0.30's Wayland backend delivers no file
drag and drop, so on a Wayland session with `DISPLAY` set the GUI event loop uses winit's
X11 backend (XWayland; WM_CLASS keeps the app id); `NEWSKITE_WAYLAND=1` keeps native
Wayland. Drops and Ctrl+V of copied files (no text field focused; `file://` URIs or paths,
one per line) go through `app::expand_drop`: `.nzb` files (any case) are imported, a folder
adds the `.nzb` files directly inside it, anything else is skipped with a warning (shown
under the toolbar when nothing was added). While files hover, a foreground overlay shows
an article grid with one lit cell per NZB being dragged (`widgets::article_grid_lit`). The
segment-cell motif also carries the queue total (a track in the Queue summary row,
`queue::queue_bytes`) and the 24 h history (48 thirty-minute cells, brightness from
`docks::heat_alpha`, idle cells empty).
