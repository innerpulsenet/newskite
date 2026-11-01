//! Headless GUI tests with egui_kittest: the main window renders the empty state and a
//! populated in-memory service in dark and light desktop themes and in the innerpulse
//! ink and paper themes, basic interactions work, and each look is compared with its
//! reference PNG in `tests/snapshots/` (egui_kittest; `UPDATE_SNAPSHOTS=1 cargo test`
//! rewrites the references, a mismatch leaves `<name>.new.png` and `<name>.diff.png`).
//! Motion is reduced in snapshots so the mark shows its rest drawing.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui;
use egui_kittest::kittest::Queryable;
use egui_kittest::{Harness, SnapshotError, SnapshotOptions};
use newskite_app::gui::queue::EMPTY_QUEUE;
use newskite_app::gui::{brand, brand_tokens, Action, Appearance, Tab};
use newskite_app::{AppConfig, NewskiteApp, ThemeMode};
use newskite_service::{
    JobState, LogLevel, NewBookmark, Service, ServiceOptions, Store, ThroughputSample,
};
use newskite_store::NewJob;
use newskite_theme::Theme;
use newskite_types::{ServerConfig, Settings};

const SIZE: egui::Vec2 = egui::vec2(1280.0, 800.0);

/// Desktop-like themes: the theme crate's Breeze fixtures when present, else the
/// built-in fallback palette.
fn theme(dark: bool) -> Theme {
    let name = if dark {
        "breeze-dark.kdeglobals"
    } else {
        "breeze-light.kdeglobals"
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../newskite-theme/tests/fixtures")
        .join(name);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| newskite_theme::from_kdeglobals_str(&s))
        .unwrap_or_else(|| newskite_theme::fallback(dark))
}

struct Fixture {
    rt: tokio::runtime::Runtime,
    service: Service,
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(seed: impl FnOnce(&Store, &Path)) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open_in_memory().expect("store");
        // No usable server: the default row has an empty host.
        let settings = Settings {
            download_dir: dir.path().join("downloads"),
            servers: vec![ServerConfig::default()],
            ..Default::default()
        };
        store.save_settings(&settings).expect("settings");
        seed(&store, dir.path());
        let service = Service::with_store(
            store,
            ServiceOptions::new(dir.path().join("db")),
            rt.handle().clone(),
        );
        Self { rt, service, dir }
    }

    fn harness(&self, dark: bool, size: egui::Vec2) -> Harness<'static, Option<NewskiteApp>> {
        self.harness_with(dark, size, true)
    }

    fn harness_with(
        &self,
        dark: bool,
        size: egui::Vec2,
        forward: bool,
    ) -> Harness<'static, Option<NewskiteApp>> {
        self.harness_themed(theme(dark), size, forward, None, true, 0.25)
    }

    /// `forward = false` keeps service events (e.g. the 1 Hz sampler) out of the window
    /// so a test can feed events deterministically.
    fn harness_themed(
        &self,
        t: Theme,
        size: egui::Vec2,
        forward: bool,
        appearance_file: Option<PathBuf>,
        reduced_motion: bool,
        step_dt: f32,
    ) -> Harness<'static, Option<NewskiteApp>> {
        let service = self.service.clone();
        let handle = self.rt.handle().clone();
        Harness::builder()
            .with_size(size)
            .with_pixels_per_point(1.0)
            .with_step_dt(step_dt)
            .build_state(
                move |ctx, app: &mut Option<NewskiteApp>| {
                    let app = app.get_or_insert_with(|| {
                        NewskiteApp::new(
                            ctx.clone(),
                            service.clone(),
                            handle.clone(),
                            AppConfig {
                                theme: ThemeMode::Fixed(t.clone()),
                                notifications: false,
                                forward_events: forward,
                                appearance_file: appearance_file.clone(),
                                reduced_motion: Some(reduced_motion),
                            },
                        )
                    });
                    app.ui(ctx);
                },
                None,
            )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.service.shutdown();
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn seed_populated(store: &Store, dir: &Path) {
    let mut settings = store.load_settings().expect("load");
    settings.servers[0] = ServerConfig {
        host: "news.example.net".into(),
        port: 563,
        tls: true,
        username: "reader".into(),
        password: "not-shown".into(),
        connections: 12,
        ..Default::default()
    };
    settings.max_connections = 20;
    settings.watch_folder = dir.join("watch");
    store.save_settings(&settings).expect("settings");

    let t0 = now() - 3600;
    let mk = |name: &str, total: u64, at: i64| {
        store
            .insert_job_at(
                &NewJob {
                    name: name.into(),
                    bytes_total: total,
                    // A fixed display path (nothing is written there), so the snapshots
                    // do not change with the temporary directory's random name.
                    dest_dir: Path::new("/home/reader/Downloads/Newskite").join(name),
                    source_path: dir.join(format!("{name}.nzb")),
                    password: None,
                },
                at,
            )
            .expect("job")
    };
    let a = mk("Debian.12.Netinst.amd64", 658 * 1024 * 1024, t0);
    store
        .set_job_progress(a.id, 658 * 1024 * 1024)
        .expect("progress");
    store
        .set_job_state(a.id, JobState::Done, None)
        .expect("state");
    let b = mk("Open.Movie.Project.2160p", 4 * 1024 * 1024 * 1024, t0 + 60);
    store
        .set_job_progress(b.id, 1_700_000_000)
        .expect("progress");
    let c = mk("Linux.Audio.Samples.Pack", 312 * 1024 * 1024, t0 + 120);
    store
        .set_job_state(
            c.id,
            JobState::Failed,
            Some("3 missing articles: part1@x, part7@x, part9@x"),
        )
        .expect("state");
    let d = mk("Public.Domain.Archive", 96 * 1024 * 1024, t0 + 180);
    store
        .set_job_progress(d.id, 20 * 1024 * 1024)
        .expect("progress");
    store
        .set_job_state(d.id, JobState::Paused, None)
        .expect("state");

    let isos = store
        .add_bookmark(&NewBookmark {
            parent_id: None,
            name: "Linux ISOs".into(),
            newsgroup: Some("alt.binaries.linux".into()),
            color: Some("#3daee9".into()),
        })
        .expect("bookmark");
    store
        .add_bookmark(&NewBookmark {
            parent_id: Some(isos.id),
            name: "Debian".into(),
            newsgroup: None,
            color: Some("46,160,67".into()),
        })
        .expect("bookmark");
    store
        .add_bookmark(&NewBookmark {
            parent_id: None,
            name: "Podcasts".into(),
            newsgroup: None,
            color: None,
        })
        .expect("bookmark");

    // A shaped hour of history.
    let start = now() - 3000;
    for i in 0..600_i64 {
        let phase = (i as f64 / 60.0).sin().abs();
        let v = (2_000_000.0 + 9_000_000.0 * phase) as u64;
        store
            .add_sample(&ThroughputSample {
                ts: start + i * 5,
                bytes_up: 0,
                bytes_down: v,
            })
            .expect("sample");
    }
    store
        .add_downloaded_bytes(658 * 1024 * 1024 + 1_700_000_000)
        .expect("counter");
}

/// Pixels allowed to differ from the reference beyond kittest's per-pixel threshold:
/// enough for the wall-clock text a window shows (message times), far below a missing
/// panel, an unthemed surface or a shifted layout.
const SNAPSHOT_PIXEL_ALLOWANCE: usize = 2_500;

fn snapshot(harness: &mut Harness<'_, Option<NewskiteApp>>, name: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
    // One GPU render at a time: concurrent wgpu renders from parallel tests have crashed
    // the software rasterizer here.
    static RENDER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = RENDER.lock().unwrap_or_else(|e| e.into_inner());
    let options = SnapshotOptions::new()
        .output_path(dir)
        .failed_pixel_count_threshold(SNAPSHOT_PIXEL_ALLOWANCE);
    match harness.try_snapshot_options(name, &options) {
        Ok(()) => {}
        // No GPU adapter (CI): the UI assertions above still ran.
        Err(SnapshotError::RenderError { err }) => {
            eprintln!("render unavailable, snapshot {name} skipped: {err}")
        }
        Err(e) => panic!("{e}"),
    }
}

/// Steps the harness until `done` holds (background work such as NZB imports finishes
/// off the UI thread), for at most about five seconds.
fn run_until(
    h: &mut Harness<'_, Option<NewskiteApp>>,
    mut done: impl FnMut(&Harness<'_, Option<NewskiteApp>>) -> bool,
) {
    for _ in 0..500 {
        h.step();
        if done(h) {
            h.run_steps(2);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("condition not reached");
}

fn app<'h>(h: &'h Harness<'_, Option<NewskiteApp>>) -> &'h NewskiteApp {
    h.state().as_ref().expect("app created on first frame")
}

fn assert_no_bad_numbers(h: &Harness<'_, Option<NewskiteApp>>) {
    for bad in ["NaN", "inf B", "Infinity", "undefined"] {
        assert!(
            h.query_by_label_contains(bad).is_none(),
            "found {bad:?} in the UI"
        );
    }
}

/// The four looks every window is checked in: desktop dark and light (Breeze fixtures)
/// and the two innerpulse themes.
fn looks() -> [(Theme, &'static str); 4] {
    [
        (theme(true), "dark"),
        (theme(false), "light"),
        (brand_tokens::ink(), "ink"),
        (brand_tokens::paper(), "paper"),
    ]
}

#[test]
fn empty_state_renders_cleanly() {
    let fx = Fixture::new(|_, _| {});
    for (t, look) in looks() {
        let mut h = fx.harness_themed(t, SIZE, true, None, true, 0.25);
        h.run_steps(4);
        h.get_by_label(EMPTY_QUEUE);
        h.get_by_label("No bookmarks yet");
        h.get_by_label("No throughput history yet");
        h.get_by_label("No server configured.");
        h.get_by_label("Remaining: 0 / 0 jobs");
        assert_eq!(
            h.get_all_by_label("No server").count(),
            2,
            "queue pill and status pill"
        );
        h.get_by_label("0 B/s");
        h.get_by_label("Support Issues?");
        // Masthead (mark, wordmark, readout) and the quiet credit.
        h.get_by_label("Newskite · NO SERVER");
        h.get_by_label("innerpulse");
        assert_no_bad_numbers(&h);
        snapshot(&mut h, &format!("empty-{look}"));
    }
}

#[test]
fn populated_window_renders() {
    let fx = Fixture::new(seed_populated);
    for (t, look) in looks() {
        let mut h = fx.harness_themed(t, SIZE, true, None, true, 0.25);
        h.run_steps(4);
        h.get_by_label("Debian.12.Netinst.amd64");
        h.get_by_label("Open.Movie.Project.2160p");
        h.get_by_label("done");
        h.get_by_label("failed");
        h.get_by_label("paused");
        h.get_by_label("3 missing articles: part1@x, part7@x, part9@x");
        h.get_by_label("Linux ISOs");
        h.get_by_label("Debian");
        h.get_by_label("Ready · news.example.net:563 · 12 conn");
        h.get_by_label("Online");
        h.get_by_label("news.example.net:563");
        h.get_by_label("reader");
        h.get_by_label("12 · max 20 · using 12");
        h.get_by_label_contains("Throughput graph, 600 stored samples");
        h.get_by_label("Remaining: 2 / 4 jobs");
        h.get_by_label("100%  ·  658 MB / 658 MB");
        h.get_by_label("Newskite · NNTP · NEWS.EXAMPLE.NET:563 TLS · 12 CONN");
        h.get_by_label("0 this session");
        assert!(
            h.query_by_label_contains("not-shown").is_none(),
            "password must never be displayed"
        );
        assert_no_bad_numbers(&h);
        snapshot(&mut h, &format!("populated-{look}"));
    }
    // Narrow window: docks stack 2×2, toolbar drops labels.
    let mut h = fx.harness(true, egui::vec2(1000.0, 640.0));
    h.run_steps(4);
    h.get_by_label("Open NZB");
    assert_no_bad_numbers(&h);
    snapshot(&mut h, "populated-dark-narrow");
}

fn write_nzb(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).expect("write nzb");
    p
}

const VALID_NZB: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head><meta type="name">Sample Release</meta></head>
  <file poster="poster@example" date="1700000000" subject="&quot;sample.bin&quot; yEnc (1/2)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="500000" number="1">part1@example</segment>
      <segment bytes="500000" number="2">part2@example</segment>
    </segments>
  </file>
</nzb>"#;

#[test]
fn import_nzb_switches_to_queue_and_errors_inline() {
    let fx = Fixture::new(|_, _| {});
    let mut h = fx.harness(true, SIZE);
    h.run_steps(2);

    // Ctrl+O reveals the field; the Search tab is shown first to check the switch.
    h.state_mut()
        .as_mut()
        .expect("app")
        .push_action(Action::SetTab(Tab::Search));
    h.run_steps(2);
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::O);
    h.run_steps(2);
    assert!(app(&h).view.open_nzb.visible);

    let bad = write_nzb(fx.dir.path(), "broken.nzb", "<nzb><file>");
    h.state_mut()
        .as_mut()
        .expect("app")
        .push_action(Action::AddNzb(bad));
    run_until(&mut h, |h| app(h).view.open_nzb.error.is_some());
    assert!(app(&h).data.jobs.is_empty(), "malformed NZB creates no job");
    assert!(app(&h).view.open_nzb.error.is_some(), "inline error shown");
    assert!(
        !app(&h).data.logs.is_empty(),
        "error reaches the message log"
    );

    let good = write_nzb(fx.dir.path(), "sample.nzb", VALID_NZB);
    h.state_mut()
        .as_mut()
        .expect("app")
        .push_action(Action::AddNzb(good));
    run_until(&mut h, |h| app(h).data.jobs.len() == 1);
    let a = app(&h);
    assert_eq!(a.data.jobs.len(), 1);
    assert_eq!(a.view.tab, Tab::Queue);
    assert!(a.view.open_nzb.error.is_none());
    h.get_by_label("Sample Release");
    h.get_by_label("0%  ·  0 B / 977 KB");
}

#[test]
fn settings_modal_opens_validates_and_closes() {
    let fx = Fixture::new(|_, _| {});
    let mut h = fx.harness(false, SIZE);
    h.run_steps(2);
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Comma);
    h.run_steps(3);
    assert!(app(&h).view.settings.is_some());
    h.get_by_label("Shut down when queue finishes");
    h.get_by_label("Add server");
    h.get_by_label("Passwords are stored unencrypted in the local settings database.");

    // Add a row, then Cancel discards it.
    h.get_by_label("Add server").click();
    h.run_steps(2);
    assert_eq!(
        app(&h)
            .view
            .settings
            .as_ref()
            .map(|d| d.settings.servers.len()),
        Some(2)
    );
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert!(app(&h).view.settings.is_none());
    assert_eq!(fx.service.settings().servers.len(), 1, "cancel never saves");

    // Esc closes too.
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Comma);
    h.run_steps(2);
    assert!(app(&h).view.settings.is_some());
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert!(app(&h).view.settings.is_none());

    // Save persists the draft.
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Comma);
    h.run_steps(2);
    if let Some(d) = h
        .state_mut()
        .as_mut()
        .and_then(|a| a.view.settings.as_mut())
    {
        d.settings.servers[0].host = "news.example.org".into();
        d.settings.speed_limit_kbps = 512;
    }
    h.run_steps(1);
    h.get_by_label("Save").click();
    h.run_steps(3);
    assert!(app(&h).view.settings.is_none());
    let saved = fx.service.settings();
    assert_eq!(saved.servers[0].host, "news.example.org");
    assert_eq!(saved.speed_limit_kbps, 512);
    h.get_by_label("Online");
}

#[test]
fn bookmarks_add_and_remove_inline() {
    let fx = Fixture::new(|_, _| {});
    let mut h = fx.harness(true, SIZE);
    h.run_steps(2);
    h.get_by_label("Add a bookmark").click();
    h.run_steps(2);
    // Empty name is rejected in place.
    h.get_by_label("Add").click();
    h.run_steps(2);
    h.get_by_label("Name is required.");
    assert!(fx.service.bookmarks().is_empty());

    if let Some(form) = h
        .state_mut()
        .as_mut()
        .and_then(|a| a.view.sidebar.form.as_mut())
    {
        form.name = "Linux ISOs".into();
        form.newsgroup = "alt.binaries.linux".into();
        form.color = "#3daee9".into();
    }
    h.run_steps(1);
    h.get_by_label("Add").click();
    h.run_steps(3);
    assert_eq!(fx.service.bookmarks().len(), 1);
    h.get_by_label("Linux ISOs");

    // × asks in place, Yes removes.
    h.get_by_label("Remove bookmark").click();
    h.run_steps(2);
    h.get_by_label("Remove?");
    h.get_by_label("Yes").click();
    h.run_steps(3);
    assert!(fx.service.bookmarks().is_empty());
    h.get_by_label("No bookmarks yet");
}

#[test]
fn messages_dock_shows_logs_newest_first() {
    let fx = Fixture::new(|_, _| {});
    fx.service.log(LogLevel::Info, "first line");
    fx.service.log(LogLevel::Error, "second line");
    let mut h = fx.harness(true, SIZE);
    h.run_steps(3);
    let logs = &app(&h).data.logs;
    assert_eq!(logs[0].text, "second line");
    h.get_by_label("second line");
    h.get_by_label("first line");
}

#[test]
fn queue_remove_confirms_in_place() {
    let fx = Fixture::new(seed_populated);
    let mut h = fx.harness(true, SIZE);
    h.run_steps(3);
    let before = fx.service.jobs().len();
    let id = app(&h)
        .data
        .jobs
        .iter()
        .find(|j| j.name == "Public.Domain.Archive")
        .map(|j| j.id)
        .expect("job");
    h.state_mut()
        .as_mut()
        .expect("app")
        .view
        .queue
        .confirm_remove = Some(id);
    h.run_steps(2);
    h.get_by_label("Remove?");
    h.get_by_label("Yes").click();
    h.run_steps(3);
    assert_eq!(fx.service.jobs().len(), before - 1);
    assert!(fx.service.jobs().iter().all(|j| j.id != id));
}

#[test]
fn single_stored_sample_and_live_series_draw() {
    let fx = Fixture::new(|store, _| {
        store
            .add_sample(&ThroughputSample {
                ts: now() - 120,
                bytes_up: 0,
                bytes_down: 3 * 1024 * 1024,
            })
            .expect("sample");
    });
    let mut h = fx.harness_with(true, SIZE, false);
    h.run_steps(3);
    h.get_by_label_contains("Throughput graph, 1 stored samples, 0 live");
    snapshot(&mut h, "single-sample-dark");

    let tx = h.state().as_ref().expect("app").sender();
    for v in [512 * 1024, 1024 * 1024, 768 * 1024, 1024 * 1024] {
        tx.send(newskite_app::gui::UiMsg::Service(
            newskite_service::ServiceEvent::Throughput { bytes_per_sec: v },
        ))
        .expect("send");
    }
    h.run_steps(3);
    h.get_by_label_contains("Throughput graph, 1 stored samples, 4 live");
    h.get_by_label("1.0 MB/s");
    assert_no_bad_numbers(&h);
}

#[test]
fn dropped_nzb_is_imported() {
    let fx = Fixture::new(|_, _| {});
    let mut h = fx.harness(true, SIZE);
    h.run_steps(2);
    let good = write_nzb(fx.dir.path(), "dropped.nzb", VALID_NZB);
    h.input_mut().dropped_files.push(egui::DroppedFile {
        path: Some(good),
        ..Default::default()
    });
    run_until(&mut h, |h| app(h).data.jobs.len() == 1);
    assert_eq!(fx.service.jobs().len(), 1);
    h.get_by_label("Sample Release");
}

#[test]
fn appearance_switches_on_save_and_persists() {
    let fx = Fixture::new(|_, _| {});
    let file = fx.dir.path().join("prefs").join("appearance.json");
    let mut h = fx.harness_themed(theme(false), SIZE, true, Some(file.clone()), true, 0.25);
    h.run_steps(2);
    assert_eq!(app(&h).appearance, Appearance::FollowDesktop);
    assert!(!app(&h).theme.dark, "follows the (light) desktop theme");

    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Comma);
    h.run_steps(2);
    h.get_by_label("innerpulse ink");
    h.get_by_label("innerpulse paper");
    // Choosing in the sheet changes nothing until Save.
    // The Appearance section is at the end of the sheet: scroll it into view.
    for _ in 0..8 {
        h.get_by_label("innerpulse ink").scroll_down();
        h.run_steps(1);
    }
    h.run_steps(1);
    h.get_by_label("innerpulse ink").click();
    h.run_steps(2);
    assert_eq!(app(&h).appearance, Appearance::FollowDesktop);
    assert_eq!(
        app(&h).view.settings.as_ref().map(|d| d.appearance),
        Some(Appearance::Ink),
        "the sheet holds the choice"
    );
    snapshot(&mut h, "settings-light");
    h.get_by_label("Save").click();
    h.run_steps(3);
    let a = app(&h);
    assert!(a.view.settings.is_none());
    assert_eq!(a.appearance, Appearance::Ink);
    assert_eq!(a.theme, brand_tokens::ink());
    assert!(a.palette.dark);
    let stored = std::fs::read_to_string(&file).expect("appearance file written");
    assert!(stored.contains("innerpulse-ink"), "{stored}");
    drop(h);

    // A new window wears the stored choice.
    let mut h = fx.harness_themed(theme(false), SIZE, true, Some(file), true, 0.25);
    h.run_steps(2);
    assert_eq!(app(&h).appearance, Appearance::Ink);
    assert_eq!(app(&h).theme, brand_tokens::ink());
}

#[test]
fn unreadable_settings_show_a_notice() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let dir = tempfile::tempdir().expect("tempdir");
    let data = dir.path().join("db");
    drop(Store::open(&data).expect("store"));
    {
        let conn =
            rusqlite::Connection::open(data.join(newskite_store::DB_FILE_NAME)).expect("sqlite");
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('settings', ?1)",
            [r#"{"servers": [{"host": "news.example.com", "port": "563"}]}"#],
        )
        .expect("corrupt row");
    }
    let service = Service::with_store(
        Store::open(&data).expect("reopen"),
        ServiceOptions::new(data.clone()),
        rt.handle().clone(),
    );
    let fx = Fixture { rt, service, dir };
    let mut h = fx.harness_themed(brand_tokens::paper(), SIZE, true, None, true, 0.25);
    h.run_steps(3);
    assert!(app(&h).data.settings_load_error.is_some());
    // Messages carries the service's report.
    h.get_by_label_contains("settings could not be loaded");
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Comma);
    h.run_steps(3);
    h.get_by_label_contains("The stored settings could not be read.");
    assert_no_bad_numbers(&h);
    snapshot(&mut h, "settings-notice-paper");
}

#[test]
fn about_dialog_opens_from_the_masthead_and_closes() {
    let fx = Fixture::new(|_, _| {});
    let mut h = fx.harness_themed(brand_tokens::ink(), SIZE, true, None, true, 0.25);
    h.run_steps(2);
    h.get_by_label("Newskite · NO SERVER").click();
    h.run_steps(3);
    assert!(app(&h).view.about_open);
    h.get_by_label("Usenet (NZB) download client");
    h.get_by_label(&format!("Version {}", env!("CARGO_PKG_VERSION")));
    assert_eq!(
        h.get_all_by_label("innerpulse").count(),
        2,
        "credit in the dialog and in the status bar"
    );
    snapshot(&mut h, "about-ink");
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert!(!app(&h).view.about_open);
}

#[test]
fn masthead_mark_draws_in_then_beats_only_while_working() {
    let fx = Fixture::new(seed_populated);
    // Full motion, 20 fps steps, no forwarded events (the test drives the state).
    let mut h = fx.harness_themed(theme(true), SIZE, false, None, false, 0.05);
    h.run_steps(3);
    let rest = brand::rest_heights();
    let first = app(&h).view.masthead_mark;
    assert!(
        first.iter().zip(rest).any(|(a, b)| (a - b).abs() > 1.0),
        "drawing in at first: {first:?}"
    );
    h.run_steps(30);
    let idle = app(&h).view.masthead_mark;
    assert!(
        idle.iter().zip(rest).all(|(a, b)| (a - b).abs() < 1e-6),
        "rests after the draw-in while nothing downloads: {idle:?}"
    );
    assert_eq!(app(&h).working_tempo(), None);

    // Bytes moving on a downloading job: the beat runs at the busy tempo.
    let tx = app(&h).sender();
    tx.send(newskite_app::gui::UiMsg::Service(
        newskite_service::ServiceEvent::Throughput {
            bytes_per_sec: 1_000_000,
        },
    ))
    .expect("send");
    let mut moved = false;
    for _ in 0..12 {
        // The job list is re-read from the store now and then; keep the state for the test.
        if let Some(a) = h.state_mut().as_mut() {
            a.data.jobs[1].state = JobState::Downloading;
        }
        h.step();
        assert_eq!(app(&h).working_tempo(), Some(brand::PERIOD_BUSY));
        let m = app(&h).view.masthead_mark;
        moved |= m.iter().zip(rest).any(|(a, b)| (a - b).abs() > 1.0);
    }
    assert!(moved, "a pass runs while working");
}

#[test]
fn nntp_activity_counts_articles_and_missing() {
    let fx = Fixture::new(seed_populated);
    let mut h = fx.harness_with(true, SIZE, false);
    h.run_steps(2);
    h.get_by_label("0 this session");
    let tx = app(&h).sender();
    let job_id = app(&h).data.jobs[1].id;
    for done in 1..=3 {
        tx.send(newskite_app::gui::UiMsg::Service(
            newskite_service::ServiceEvent::Engine {
                job_id,
                event: newskite_types::EngineEvent::SegmentDone {
                    name: "a.bin".into(),
                    done,
                    total: 9,
                    bytes: 1000,
                },
            },
        ))
        .expect("send");
    }
    tx.send(newskite_app::gui::UiMsg::Service(
        newskite_service::ServiceEvent::Engine {
            job_id,
            event: newskite_types::EngineEvent::FileFailed {
                name: "b.bin".into(),
                missing: vec!["x@y".into(), "z@y".into()],
            },
        },
    ))
    .expect("send");
    h.run_steps(2);
    h.get_by_label("3 this session");
    assert_eq!(app(&h).data.articles_missing, 2);
    assert_no_bad_numbers(&h);
}

/// The minimum window (960×600): the Remove confirmation's Yes/No stay on screen, and the
/// docks show their readings above the status bar.
#[test]
fn minimum_window_keeps_remove_confirmation_and_docks_on_screen() {
    let fx = Fixture::new(seed_populated);
    let size = egui::vec2(960.0, 600.0);
    let mut h = fx.harness(true, size);
    h.run_steps(4);
    let failed = app(&h)
        .data
        .jobs
        .iter()
        .find(|j| j.state == JobState::Failed)
        .map(|j| j.id)
        .expect("failed job");
    // Every action icon of the failed row is inside the window.
    for label in ["Retry (completed files are kept)", "Remove from the queue"] {
        for node in h.get_all_by_label(label) {
            assert!(
                node.rect().right() <= size.x - 4.0,
                "{label} at {:?}",
                node.rect()
            );
        }
    }
    h.state_mut()
        .as_mut()
        .expect("app")
        .view
        .queue
        .confirm_remove = Some(failed);
    h.run_steps(2);
    for label in ["Remove?", "Yes", "No"] {
        let r = h.get_by_label(label).rect();
        assert!(
            r.right() <= size.x - 4.0 && r.left() >= 0.0,
            "{label} at {r:?}"
        );
    }
    // Account readings fit in their card, above the status bar.
    for label in ["Jobs pending", "Max connections"] {
        let r = h.get_by_label(label).rect();
        assert!(r.bottom() <= size.y - 28.0, "{label} at {r:?}");
    }
    snapshot(&mut h, "populated-dark-minimum");
    h.get_by_label("No").click();
    h.run_steps(2);
    assert_eq!(
        app(&h).view.queue.confirm_remove,
        None,
        "No cancels in place"
    );
    h.state_mut()
        .as_mut()
        .expect("app")
        .view
        .queue
        .confirm_remove = Some(failed);
    h.run_steps(2);
    h.get_by_label("Yes").click();
    h.run_steps(3);
    assert!(fx.service.jobs().iter().all(|j| j.id != failed));
}

#[test]
fn a_failed_bookmark_add_says_why_and_drops_a_vanished_parent() {
    let fx = Fixture::new(seed_populated);
    let mut h = fx.harness(true, SIZE);
    h.run_steps(2);
    h.get_by_label("Add a bookmark").click();
    h.run_steps(2);
    let parent = fx
        .service
        .bookmarks()
        .into_iter()
        .find(|b| b.name == "Podcasts")
        .map(|b| b.id)
        .expect("bookmark");
    if let Some(form) = h
        .state_mut()
        .as_mut()
        .and_then(|a| a.view.sidebar.form.as_mut())
    {
        form.name = "Shows".into();
        form.parent = Some(parent);
    }
    // The parent disappears while the form is open (removed elsewhere), then Add.
    fx.service.remove_bookmark(parent).expect("remove");
    h.state_mut()
        .as_mut()
        .expect("app")
        .push_action(Action::AddBookmark(NewBookmark {
            parent_id: Some(parent),
            name: "Shows".into(),
            newsgroup: None,
            color: None,
        }));
    h.run_steps(2);
    let form = app(&h).view.sidebar.form.as_ref().expect("form stays open");
    assert!(
        form.error
            .as_deref()
            .is_some_and(|e| e.starts_with("Could not add the bookmark")),
        "{:?}",
        form.error
    );
    assert_eq!(form.parent, None, "the vanished parent is dropped");
    h.get_by_label_contains("Could not add the bookmark");
}

#[test]
fn open_nzb_field_accepts_a_pasted_file_uri() {
    let fx = Fixture::new(|_, _| {});
    let mut h = fx.harness(true, SIZE);
    h.run_steps(2);
    let dir = fx.dir.path().join("with space");
    std::fs::create_dir_all(&dir).expect("dir");
    let good = write_nzb(&dir, "pasted.nzb", VALID_NZB);
    {
        let a = h.state_mut().as_mut().expect("app");
        a.push_action(Action::ShowOpenNzb);
        a.view.open_nzb.path = format!("file://{}", good.display()).replace(' ', "%20");
    }
    h.run_steps(2);
    h.get_by_label("Add").click();
    run_until(&mut h, |h| app(h).data.jobs.len() == 1);
    assert!(app(&h).view.open_nzb.error.is_none());
}
