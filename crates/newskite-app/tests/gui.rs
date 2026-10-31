//! Headless GUI tests with egui_kittest: the main window renders the empty state and a
//! populated in-memory service in dark and light desktop themes, basic interactions
//! work, and PNG snapshots are written to `tests/snapshots/` for visual inspection.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui;
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use newskite_app::gui::queue::EMPTY_QUEUE;
use newskite_app::gui::{Action, Tab};
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

    /// `forward = false` keeps service events (e.g. the 1 Hz sampler) out of the window
    /// so a test can feed events deterministically.
    fn harness_with(
        &self,
        dark: bool,
        size: egui::Vec2,
        forward: bool,
    ) -> Harness<'static, Option<NewskiteApp>> {
        let service = self.service.clone();
        let handle = self.rt.handle().clone();
        let t = theme(dark);
        Harness::builder()
            .with_size(size)
            .with_pixels_per_point(1.0)
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
                    dest_dir: dir.join("downloads").join(name),
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

fn snapshot(harness: &mut Harness<'_, Option<NewskiteApp>>, name: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("cannot create {}: {e}", dir.display());
        return;
    }
    match harness.render() {
        Ok(img) => {
            let path = dir.join(format!("{name}.png"));
            img.save(&path).expect("write png");
            eprintln!("wrote {}", path.display());
        }
        // No GPU adapter (CI): the UI assertions above still ran.
        Err(e) => eprintln!("render unavailable, snapshot {name} skipped: {e}"),
    }
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

#[test]
fn empty_state_renders_cleanly() {
    let fx = Fixture::new(|_, _| {});
    for dark in [true, false] {
        let mut h = fx.harness(dark, SIZE);
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
        assert_no_bad_numbers(&h);
        snapshot(&mut h, if dark { "empty-dark" } else { "empty-light" });
    }
}

#[test]
fn populated_window_renders() {
    let fx = Fixture::new(seed_populated);
    for dark in [true, false] {
        let mut h = fx.harness(dark, SIZE);
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
        assert!(
            h.query_by_label_contains("not-shown").is_none(),
            "password must never be displayed"
        );
        assert_no_bad_numbers(&h);
        snapshot(
            &mut h,
            if dark {
                "populated-dark"
            } else {
                "populated-light"
            },
        );
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
    h.run_steps(2);
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
    h.run_steps(3);
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
    snapshot(&mut h, "settings-light");

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
    h.run_steps(3);
    assert_eq!(fx.service.jobs().len(), 1);
    h.get_by_label("Sample Release");
}
