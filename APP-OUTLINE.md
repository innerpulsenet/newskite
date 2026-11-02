# Newskite — application outline (implementation-agnostic)

A from-scratch specification of what this application **is**, feature by feature, plus an overview
of its interface. Written so another agent can re-implement it in any language/stack without seeing
this codebase, the original commercial client, or the vendor's services.

**Deliberately excluded:** any vendor search/browse API, vendor account/billing integration,
ratings or content-report dialogs. Search is indexer-agnostic (Newznab) or absent — see §10.

Scope of this document: features (§4), interface (§5–§7), data (§8), flows (§9), non-goals (§10),
build order (§11), acceptance criteria (§12).

---

## 1. Positioning

Newskite is a **desktop Usenet (NZB) download client**: the user gives it an NZB file, it downloads
the binary articles from an NNTP server over TLS, decodes them, writes the exact original file(s)
to disk, repairs/extracts as needed, and keeps a reorderable queue, history and live throughput
stats. It is a background-work application: long-running transfers, precise progress reporting,
and a UI that stays responsive and theme-correct on a Linux desktop (KDE Plasma primarily).

Ships as a Linux desktop app (`.deb` + `.AppImage`) with a single window, plus an optional headless
CLI that drives the same queue engine.

---

## 2. Domain primer (needed to build any of this)

- **Usenet / NNTP** — a server protocol (TCP, usually TLS on 563). Relevant commands:
  `AUTHINFO USER`/`PASS`, `GROUP`, `STAT <msgid>`, `BODY <msgid>`, `QUIT`.
  Responses are `NNN text`; multi-line bodies end with a line containing a single `.`.
  Lines in the body beginning with `.` are **dot-stuffed** (sent as `..`) and must be unstuffed.
  `430`/`423` = article missing.
- **NZB** — an XML manifest listing `<file>` entries (subject, poster, date, groups) each with
  `<segment bytes=… number=…>message-id</segment>`. Segments must be processed in `number` order;
  the file is the concatenation of its segments' decoded bytes.
- **yEnc** — the binary→text encoding posters use. Byte `b` is encoded as `(b + 42) mod 256`.
  The character `=` introduces an escape: `=` followed by `(b + 42 + 64) mod 256`.
  A byte must be escaped **when its encoded value is one of** NUL `0x00`, LF `0x0A`, CR `0x0D`,
  `=` `0x3D` (i.e. payload bytes 214, 224, 227, 19). CR/LF are line framing, never payload.
  Headers `=ybegin` (name, size, part, total), optional `=ypart begin=… end=…` and `=yend`.
  `begin`/`end` are **1-based inclusive byte offsets of the part within the whole file** — this is
  what makes parallel, out-of-order assembly possible.
- **PAR2** — parity files that can verify/repair missing articles' output; optional post-step.
- **Newznab** — the common indexer search API (XML/JSON over HTTP with an API key). Optional
  input path; see §10.

---

## 3. Architecture-independent shape

Three concerns, cleanly separated:

1. **Engine** — NZB parsing, article fetch, yEnc decode, assembly, progress events. No UI code.
2. **Persistence** — queue/history/settings/throughput. Durable across restarts.
3. **Presentation** — window/CLI, theming, event rendering.

The same engine+persistence must be drivable from both the GUI and a CLI. Progress is emitted as
events, not polled state.

---

## 4. Features

Each feature lists: what it does, rules, and edge cases the implementation must handle.

### F1. Add NZBs
- Add by file path (typed/pasted in the UI; also via CLI argument).
- Parse header/metadata: first file's name, file count, segment count, total encoded bytes.
- The job is persisted **before** any downloading starts; the NZB path is stored so the job can be
  resumed after an app restart.
- A malformed or unreadable NZB produces an error message and **no** job row.
- Multi-file NZBs are supported (archive sets: one NZB → several output files).

### F2. Watch folder
- A configurable folder; when enabled, a scan enqueues every `*.nzb` found.
- Scanning is user-triggered (a "Scan watch folder" action) plus optionally on a timer.
- Already-queued files must not be enqueued twice (dedupe by source path or by job row presence).

### F3. Download engine
- For each file in the job: fetch every segment `BODY <msgid>`, decode yEnc, write decoded bytes at
  `begin-1` in the output file (`pwrite`/offset-write — never buffer the whole file).
- Concurrency: a configurable number of simultaneous segment fetches (per server connection count);
  default 20 max, typically 4–30.
- Order-independent: parts may complete out of order; the final file must be byte-exact.
- A file's final size comes from the highest `end` offset written; trailing extension is allowed.
- **Missing articles are reported, never silently skipped**: the job fails with the list of missing
  message-ids, and the partially written file is not passed off as complete.
- Transient network/auth errors: retry N times (default 2) with backoff, walking configured servers.
- Per-segment progress must be surfaced frequently enough for a smooth graph (≥1 update/second).

### F4. Queue management
- Job states: `queued`, `downloading`, `done`, `failed`, `paused` (persisted strings). `paused`
  means **held by the user**: the job keeps its place but queue runs skip it.
- The queue has an explicit order (a position per job); jobs can be moved to the top, up, down
  or to the bottom. Runs download `queued` jobs in that order.
- Whole queue: Start (every queued job), Pause (immediate: the current job is interrupted,
  goes back to `queued` and resumes first), Resume.
- Per job: start one job on its own (a run limited to it, or added to an active limited run;
  the rest of the queue stays put), pause/hold one job (a downloading job is interrupted and
  held; the run moves on), retry a failed job, remove a job.
- Interrupting keeps completed files; a partial file is downloaded again.
- Progress is persisted per job (`bytes_done` / `bytes_total`) so the UI shows position after
  restart, and a crashed/interrupted run resumes rather than restarting.
- Only one queue run at a time (idempotent start).
- Finished jobs (done/failed) form the history; it can be cleared (files stay on disk).
- When the queue empties, emit an "idle" event (used by notifications / auto-shutdown).

### F5. Post-processing (planned/optional)
- PAR2 verify/repair of the output set when `.par2` files are present.
- Archive extraction when the output set is `7z`/`rar`/`zip` (including passworded archives;
  password can be supplied per job).
- Cleanup policy for successfully extracted archives and parity files.
- Naming: extract into a folder named after the release/job when a set is detected.

### F6. Throughput monitoring
- Sample download throughput at ~1 Hz into persistent history (bytes per second).
- Show live speed, historical sparkline, and "remaining jobs" in the UI.
- History survives restart and is the backing store for the graph.

### F7. (Removed) Bookmarks
- Newsgroup bookmarks, search and browse are out of scope: NZB downloading is the product.
  The bookmark table may stay in the database for compatibility; no UI uses it.

### F8. Settings
- **Connection**: a list of servers {host, port, TLS, username, password, connections, enabled};
  add/remove/edit rows; a "Test" action that performs a real NNTP handshake and shows the raw reply
  (accepts a `DATE`-style command).
- **Downloads**: download folder, watch folder, watch enabled.
- **Performance**: max connections, speed limit (KiB/s, 0 = unlimited).
- **Automation**: auto-shutdown when the queue empties (with a visible countdown, cancellable).
- Settings persist as JSON; the **first enabled server with a non-empty host** is used for runs.
- Passwords are stored locally; document the storage (plain JSON is acceptable for a rewrite, but
  say so explicitly or use an OS keyring).

### F9. Diagnostics & logs
- An in-app message/log pane showing engine lines (job started, file wrote, job complete, errors)
  plus command failures, capped ring buffer (~50 lines), newest first.
- A connection/activity pane showing the active server, plain/TLS, connection count, username and
  configured max.
- An account/summary pane showing cumulative downloaded bytes and job counts (done/pending).

### F10. Theming
- Follow the **desktop's** light/dark scheme; do not ship a fixed palette.
- Derive a small token set: surface, surface-raised, surface-sunken, text, text-muted, accent,
  accent-text, border, positive, warning, negative.
- On KDE: read `~/.config/kdeglobals` (`[General] ColorScheme`, `[Colors:Window]
  BackgroundNormal/ForegroundNormal`, `[Colors:Selection] BackgroundNormal`, `[General]
  AccentColor`), convert `r,g,b` to hex, derive the rest by mixing, and pick light/dark from
  background luminance. (A portal-based reader for `org.freedesktop.appearance` is a fine
  alternative; support both if cheap.)
- Every color in the UI must come from tokens; no literal colors in component code.
- Re-theme without restart when the scheme changes (nice-to-have).

### F11. Packaging & CLI
- Desktop bundles (`.deb`/`.AppImage` or equivalent) with a `.desktop` file, icon, and MIME
  registrations for `application/x-nzb` and `x-scheme-handler/nzb`/`nzblnk`.
- Headless CLI using the same engine:
  `app --headless --nzb FILE --dest DIR --server HOST:PORT [--tls] [--user U --pass P]
   [--connections N] [--db DIR]`
  Exit code 0 on success, non-zero on failure; prints queued/complete lines.

---

## 5. Interface overview — window

Single window: a toolbar, the job tables, a strip of three docks, and a status bar. The
intent is a focused NZB downloader: everything on screen is about the queue.

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ MASTHEAD │ Add NZB  Start/Pause/Resume  Scan Folder  Downloads       Settings │
├──────────────────────────────────────────────────────────────────────────────┤
│ TABS: Queue (n) | History (n)                              [Filter by name]  │
│ summary: 3 jobs · 4.2 GB to go · about 21 min at 3.4 MB/s · 1 held           │
│ #  Name / folder or live speed   State   Size   Progress   ▶/⏸ ↑ ⋯ ×  │ INSP- │
├─────────────────────────────────────────────────────────────────────┤ ECTOR │
│ Articles (grid)        │ Log [All|Warn|Err]                         │(sel.) │
├──────────────────────────────────────────────────────────────────────────────┤
│ SERVER STRIP: host:port TLS · articles · missing · CONN ▮▮▮▯ 12/20  [Retry]  │
│ [Online] 2 servers · 20 conn · TLS   ∿ Idle · peak 6.4 MB/s ~~~   ⛁ 128 GB free│
└──────────────────────────────────────────────────────────────────────────────┘
```

Rules:
- The window resizes gracefully down to ~960×600; docks stack in one column below 900 px.
- From 1100 px wide, a selected job opens the **Inspector** as a full-height right column;
  narrower windows show the selection in the Details dock instead.
- Empty states everywhere (no server, no jobs, no history) must render as readable text, never
  `NaN`, `Infinity`, `undefined`, or a blank panel.
- All colors from theme tokens; all numbers formatted (e.g. `1.4 MB`, `0 B/s`).
- Time left is shown only from a measured speed, never guessed.

---

## 6. Interface overview — controls

### 6.1 Toolbar (left→right)
| Control | Behavior |
|---|---|
| Masthead | Mark, wordmark and server readout; opens About |
| **Add NZB** | Reveals an inline path field (Browse…, Add); on success: queue reload, Queue tab, the new job selected |
| Start / Pause / Resume | The whole queue; label and icon follow the state; Start is disabled with nothing queued |
| Scan Folder | Import new `.nzb` files from the watch folder now (shown only when one is set) |
| Downloads | Opens the download folder in the file manager |
| Settings | Opens the Settings sheet |

### 6.2 Queue tab
- Summary line: job count, bytes to go, time left at the measured speed, held count; a
  `Start all` button when a run limited to single jobs leaves others queued.
- Columns: `#` (queue position), `Name` (subline: folder, live speed + time left while
  downloading, or the error), `State` badge, `Size`, `Progress` (segment track + `NN%` +
  `done / total`), controls.
- Controls per row: start/pause toggle (downloading → pause; queued in the active run → hold;
  queued outside any run or held → start on its own; failed → retry), move up, a `⋯` menu,
  remove (inline `Remove?` Yes/No). Right click opens the same menu: pause/hold/resume/download
  now, move to top/up/down/bottom, open download folder, archive password, remove.
- Click selects a row (Details dock). Keys on the selection: Space toggles, Delete asks to
  remove, Alt+↑/↓ move, Esc clears. Ctrl+F focuses the filter.
- Empty state: "The queue is empty. Add an NZB to begin." with an Add NZB button.

### 6.2b History tab
Done and failed jobs, most recently finished first: `Name` (subline: total time and average
speed, or the error), `Result` (chip plus what decided it: repaired / extracted / verified,
or the failure kind), `Size`, `Finished` (relative; exact UTC time on hover), actions (open
folder, retry and archive password for failed jobs, delete files, remove). Double-click
opens the folder. `Clear history` with inline confirmation removes the rows (files stay on
disk) or, via `Delete files too…` and a second confirmation, deletes their folders.

### 6.2c Inspector (selected job)
Header (state chip, name, NZB, open folder, close). Tabs:
- **Overview** — for a failed job a problem card first (failure kind, the full selectable
  reason, what to do, Retry / Set password / Servers / Copy error). Then the **lifecycle**:
  Added → Download → Verify → Repair → Extract → Clean up → Done/Failed, each step marked
  done, live (with its fraction and detail), needs-a-look, failed, skipped or waiting, with
  its duration. Then **numbers** (size, average speed, time downloading and
  post-processing, articles, missing, slices rebuilt, files cleaned up; only measured
  values) and **where** (folder, extraction folder, NZB).
- **Files** — per-file tracks, incomplete files first.
- **Log** — the job's own log (kept across restarts, newest 500 lines), Copy log, Open log
  file.

Toasts in the lower right announce each finished (6 s) or failed (15 s) job, with Show.

### 6.3 Bottom docks
1. **Articles** — the article grid of the job downloading now, else of the selected job:
   one cell per article in file and segment order (arrived, missing on every server,
   waiting), several articles per cell for big jobs (said in the key; a cell with any
   missing article shows missing). The map comes from the engine's per-article events and
   is saved when a run ends, so finished jobs keep theirs; the inspector shows it too.
   Hovering a cell names its file and articles; freshly arrived cells glow briefly.
2. **Server** — `host:port`, TLS/Plain chip, username, fail-over count, per-server health
   this session (ok / errors / login refused; articles, not-on-server, errors; last error on
   hover), connection slots (`effective / max`), and tiles for articles fetched and missing
   this session and the lifetime total. On narrow windows a selected job shows here as
   **Details** instead.
3. **Log** — last 500 lines, newest first, level filter (All / Warnings / Errors); three
   columns (level mark, clock, text) so long lines wrap under their own text. A line about
   a job opens it. Everything is also written to `newskite.log` in the data folder
   (rotated at 4 MB).

All dock motion stops under reduced motion.

### 6.4 Status bar
Left: a state pill — `Online` when a server is set and the queue is idle, `Downloading`
while active, `Paused`, `No server`, or the error cause — and the servers a run uses
(`2 servers · 20 conn · TLS`). Center: the speed, what is left and the time left while a
run is active (`Idle · peak …` otherwise, the fastest second on record) beside a sparkline
of the last 10 min (or the stored 24 h); hovering it shows the throughput graph: the live
plot over the 24 h history strip (two time axes never share one plot; one stored sample
still draws a visible, flat trace). Right: free space on the download folder's filesystem
(warning color when low or smaller than what the queue still needs; the lifetime total on
hover) and the innerpulse credit.

### 6.5 Settings sheet (modal)
Sections, each a labeled group; footer `Cancel` / `Save`:

- **Connection**
  - List of server rows. Per row: `Host`, `Port`, `TLS` (checkbox), `Username`, `Password`,
    `Connections`, `Enabled` (checkbox), `Remove`, `Test` (shows the raw NNTP reply).
  - `Add server` button. At least one row is always present (may be empty/disabled).
- **Downloads**: `Download folder`, `Watch folder`, `Watch folder enabled`.
- **Performance**: `Max connections`, `Speed limit (KiB/s, 0 = unlimited)`.
- **Automation**: `Shut down when queue finishes`.

### 6.6 Dialogs
Modals: Settings and About. Destructive actions (remove job, clear history) use inline
confirmation, not a modal.

---

## 7. Interface overview — events and state flow

The UI is event-driven; it does not poll the engine:

- `engine://event` — one of `FileStarted{name,segments}`, `SegmentDone{name,done,total,bytes}`,
  `FileDone{name,path,bytes}`, `FileFailed{name,missing[]}`.
- `engine://job` — human log line (string).
- `engine://queue-idle` — queue drained.
- `engine://paused` — pause acknowledged.
- `engine://watch-imported` — names of NZBs auto-enqueued.

On mount: load theme, settings, jobs, queue order, throughput history. While a job is downloading,
refresh the job list on a short interval (~1.5 s) *in addition to* event-driven updates.

---

## 8. Data model

Implement these entities (a relational DB is natural; any durable store is acceptable).

**Job** — `id`, `name`, `state` (queued|downloading|done|failed|paused), `bytes_total`,
`bytes_done`, `dest_dir`, `source_path` (NZB), `created_at`.
**JobFile** (optional) — `job_id`, `name`, `bytes_total`, `bytes_done`, `state`.
**Server** — `host`, `port`, `tls`, `username`, `password`, `connections`, `priority`, `enabled`.
**Job** also carries a queue `position` (0 until the user reorders).
**ThroughputSample** — `timestamp` (or date+hour), `bytes_up`, `bytes_down`.
**Setting** — `key`, `value` (JSON for the settings document).

Settings document (single JSON blob, exact field names are free):
`download_dir`, `watch_folder`, `watch_enabled`, `max_connections`, `speed_limit_kbps`,
`auto_shutdown`, `servers[]`.

Filesystem: outputs go to `download_dir` as `<dest>/<name-from-NZB-subject>`. Databases live under
the user's data dir (e.g. `~/.local/share/<App>/db/`).

---

## 9. Flows

**Import → run**
1. User provides an NZB path. 2. Parse; on error show a message. 3. Persist job `queued` with
byte totals and source path. 4. UI switches to Queue. 5. On Start: for each job, mark `downloading`,
download every file, update `bytes_done`, mark `done` (or `failed` with reason). 6. Emit idle.

**Failure**
- Auth failure / no server → the job does not start; a single clear message ("no enabled server
  configured" / "authentication required").
- Missing articles → job `failed`; message names the missing message-ids (count at minimum).
- Malformed NZB → job `failed` (or never created if the parse fails at import).

**Restart**
- Queue rows with `queued`/`downloading`/`paused` are resumed; `done`/`failed` are history.
- Throughput history and the queue order persist; theme is re-resolved from the desktop.

**Watch folder**
- Scan → enqueue new `*.nzb` → notify UI → user presses Start (or auto-start if configured).

---

## 10. Explicit non-goals

- Any vendor **search/browse/account/billing API** (the original client's private endpoints).
  Do not reverse these. Search surfaces in the UI must be labeled as indexer-driven or
  "not configured".
- Content ratings, DMCA/content-report dialogs, vendor "messages" from a remote account service.
- Windows/macOS parity. Linux desktop first.
- Multi-user or server/daemon mode; this is a single-user desktop app.

If a search feature is desired later: implement **Newznab** (indexer URL + API key → search →
download NZB → existing import path). Keep it optional and pluggable; never hardcode an indexer.

---

## 11. Suggested build order

1. **Domain core**: NZB parser, yEnc decoder (byte-exact round-trip tests), NNTP client (auth,
   BODY, dot-unstuffing), assembly to disk with offset writes. Prove with a local mock NNTP server:
   download a multi-part file and compare hashes.
2. **Persistence + queue**: jobs, states, progress, resume-after-restart; a headless CLI driver.
3. **Window shell**: layout, toolbar, tabs, docks, status bar, theme tokens from the desktop.
4. **Queue UI**: table, Start/Pause/Resume/Remove, inline import, connection pill.
5. **Settings sheet**: server editor with Test, paths, performance, automation.
6. **Per-job control**: hold/start one job, reorder, history tab.
7. **Throughput history + graph**, messages log, account summary.
8. **Post-process**: PAR2 verify/repair, archive extraction, cleanup.
9. **Automation**: speed limit, scheduler window, auto-shutdown with countdown, notifications.
10. **Packaging**: bundles, desktop entry, MIME handlers, icons; optional CLI install.

---

## 12. Acceptance criteria

- A 2 MiB file split into 8 yEnc parts downloads over a real TCP NNTP connection (with auth) and
  the output's SHA-256 equals the source: **byte-exact assembly**.
- yEnc round-trip passes for payloads covering **all 256 byte values** (this is what catches the
  escape-direction bug).
- Missing article → job `failed`, message id reported, output not presented as complete.
- Job state and `bytes_done` survive an app restart; unfinished jobs resume.
- Queue Start is idempotent; Pause stops before the next job; Resume continues; Remove deletes.
- Settings round-trip through the store; the first enabled server with a host is used for runs;
  Test shows a real protocol reply.
- Throughput history persists and renders after restart (a single sample still draws a trace).
- UI contains **no literal colors**; switching the desktop scheme changes every surface/text
  color used.
- Empty state (no server, no jobs, no history) renders cleanly — no `NaN`, no blank panels.
- Bundles install and launch; the window opens themed to the desktop.
