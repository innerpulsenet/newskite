//! End-to-end tests of headless mode against the in-process mock NNTP server.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use newskite_cli::{
    parse_args, run_headless_until, run_headless_with, EXIT_FAILURE, EXIT_INTERRUPTED, EXIT_OK,
    EXIT_USAGE,
};
use newskite_nntp::testkit::MockServer;
use newskite_store::{JobState, Store};
use tempfile::TempDir;

/// Arguments for a run against `mock` with auth, everything under `root`.
fn argv(root: &Path, mock: &MockServer, nzbs: &[&Path], extra: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = vec!["newskite".into(), "--headless".into()];
    for n in nzbs {
        v.push("--nzb".into());
        v.push(n.display().to_string());
    }
    v.extend(
        [
            "--dest",
            &root.join("dl").display().to_string(),
            "--db",
            &root.join("db").display().to_string(),
            "--server",
            &mock.addr.to_string(),
            "--user",
            USER,
            "--pass",
            PASS,
            "--connections",
            "2",
            "--retries",
            "0",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    v.extend(extra.iter().map(|s| s.to_string()));
    v
}

/// Runs headless mode with no stop signal and no progress line; returns (code, stdout).
async fn run(args: Vec<String>) -> (i32, String) {
    let args = parse_args(args).expect("valid arguments");
    let mut out = Vec::new();
    let code = tokio::time::timeout(
        Duration::from_secs(60),
        run_headless_until(args, &mut out, std::future::pending::<()>(), None),
    )
    .await
    .expect("headless run did not finish in time");
    (code, String::from_utf8(out).unwrap())
}

fn read_file(p: PathBuf) -> Vec<u8> {
    std::fs::read(&p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
}

/// Every file under `dir` (the database and its WAL/journal files).
fn all_bytes_under(dir: &Path) -> Vec<u8> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_file() {
            v.extend(std::fs::read(&p).unwrap());
        }
    }
    v
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn success_with_auth_downloads_byte_exact() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("ok", "movie.bin", 1024 * 1024 + 123, 8, 1);
    let mock = mock_for(&[&file], &[], 0).await;
    let nzb = write_nzb(
        &tmp.path().join("nzbs"),
        "ok.nzb",
        Some("cli-job"),
        &[&file],
    );

    let args = parse_args(argv(tmp.path(), &mock, &[&nzb], &[])).unwrap();
    let mut out = Vec::new();
    let code = tokio::time::timeout(Duration::from_secs(60), run_headless_with(args, &mut out))
        .await
        .unwrap();
    let stdout = String::from_utf8(out).unwrap();

    assert_eq!(code, EXIT_OK, "stdout: {stdout}");
    assert_eq!(stdout, "queued cli-job\ncomplete cli-job\n");
    let got = read_file(tmp.path().join("dl").join("cli-job").join("movie.bin"));
    assert_eq!(sha256(&got), sha256(&file.data));
    assert_eq!(mock.body_requests(), 8);

    let store = Store::open(&tmp.path().join("db")).unwrap();
    let jobs = store.list_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, JobState::Done);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn password_and_server_are_not_persisted() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("pw", "pw.bin", 64 * 1024, 2, 2);
    let mock = mock_for(&[&file], &[], 0).await;
    let nzb = write_nzb(&tmp.path().join("nzbs"), "pw.nzb", Some("pw-job"), &[&file]);

    let (code, stdout) = run(argv(tmp.path(), &mock, &[&nzb], &[])).await;
    assert_eq!(code, EXIT_OK, "stdout: {stdout}");
    assert!(!stdout.contains(PASS));

    let db = tmp.path().join("db");
    let settings = Store::open(&db).unwrap().load_settings().unwrap();
    assert!(settings
        .servers
        .iter()
        .all(|s| s.password.is_empty() && s.username.is_empty()));
    assert!(settings
        .servers
        .iter()
        .all(|s| s.host != mock.addr.ip().to_string()));
    assert!(
        !contains(&all_bytes_under(&db), PASS.as_bytes()),
        "password found in the database files"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_article_fails_with_its_id() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("miss", "miss.bin", 256 * 1024, 4, 3);
    let missing = file.ids()[2].clone();
    let mock = mock_for(&[&file], &[&missing], 0).await;
    let nzb = write_nzb(
        &tmp.path().join("nzbs"),
        "miss.nzb",
        Some("miss-job"),
        &[&file],
    );

    let (code, stdout) = run(argv(tmp.path(), &mock, &[&nzb], &[])).await;
    assert_eq!(code, EXIT_FAILURE, "stdout: {stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "stdout: {stdout}");
    assert_eq!(lines[0], "queued miss-job");
    assert!(lines[1].starts_with("failed miss-job: "), "{}", lines[1]);
    assert!(lines[1].contains(&missing), "{}", lines[1]);
    assert!(!tmp
        .path()
        .join("dl")
        .join("miss-job")
        .join("miss.bin")
        .exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_server_fails() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("down", "down.bin", 32 * 1024, 2, 4);
    // Grab a free port, then close the listener so connecting is refused.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let nzb = write_nzb(
        &tmp.path().join("nzbs"),
        "down.nzb",
        Some("down-job"),
        &[&file],
    );
    let args = vec![
        "newskite".to_string(),
        "--nzb".into(),
        nzb.display().to_string(),
        "--dest".into(),
        tmp.path().join("dl").display().to_string(),
        "--db".into(),
        tmp.path().join("db").display().to_string(),
        "--server".into(),
        format!("127.0.0.1:{port}"),
        "--retries".into(),
        "0".into(),
    ];
    let (code, stdout) = run(args).await;
    assert_eq!(code, EXIT_FAILURE, "stdout: {stdout}");
    assert!(
        stdout.starts_with("queued down-job\nfailed down-job: "),
        "stdout: {stdout}"
    );
    assert_eq!(stdout.lines().count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_password_fails_without_downloading() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("auth", "auth.bin", 32 * 1024, 2, 5);
    let mock = mock_for(&[&file], &[], 0).await;
    let nzb = write_nzb(
        &tmp.path().join("nzbs"),
        "auth.nzb",
        Some("auth-job"),
        &[&file],
    );
    let mut a = argv(tmp.path(), &mock, &[&nzb], &[]);
    let i = a.iter().position(|s| s == PASS).unwrap();
    a[i] = "wrong-password".into();
    let (code, stdout) = run(a).await;
    assert_eq!(code, EXIT_FAILURE, "stdout: {stdout}");
    assert!(stdout.contains("failed auth-job: "), "stdout: {stdout}");
    assert!(!stdout.contains("wrong-password"));
    assert_eq!(mock.body_requests(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_nzb_is_reported_and_other_jobs_still_run() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("mix", "mix.bin", 64 * 1024, 2, 6);
    let mock = mock_for(&[&file], &[], 0).await;
    let good = write_nzb(
        &tmp.path().join("nzbs"),
        "good.nzb",
        Some("good-job"),
        &[&file],
    );
    let broken = tmp.path().join("nzbs").join("broken.nzb");
    std::fs::write(&broken, "<nzb><file>not closed").unwrap();
    let absent = tmp.path().join("nzbs").join("absent.nzb");

    let (code, stdout) = run(argv(tmp.path(), &mock, &[&broken, &good, &absent], &[])).await;
    assert_eq!(code, EXIT_FAILURE, "stdout: {stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 4, "stdout: {stdout}");
    assert!(
        lines[0].starts_with(&format!("failed {}: ", broken.display())),
        "{}",
        lines[0]
    );
    assert_eq!(lines[1], "queued good-job");
    assert!(
        lines[2].starts_with(&format!("failed {}: ", absent.display())),
        "{}",
        lines[2]
    );
    assert_eq!(lines[3], "complete good-job");
    let got = read_file(tmp.path().join("dl").join("good-job").join("mix.bin"));
    assert_eq!(sha256(&got), sha256(&file.data));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_jobs_all_complete() {
    let tmp = TempDir::new().unwrap();
    let a = TestFile::random("ja", "a.bin", 96 * 1024, 3, 7);
    let b = TestFile::random("jb", "b.bin", 80 * 1024, 2, 8);
    let mock = mock_for(&[&a, &b], &[], 0).await;
    let na = write_nzb(&tmp.path().join("nzbs"), "a.nzb", Some("job-a"), &[&a]);
    let nb = write_nzb(&tmp.path().join("nzbs"), "b.nzb", Some("job-b"), &[&b]);
    // The same NZB twice on the command line is one job.
    let (code, stdout) = run(argv(tmp.path(), &mock, &[&na, &nb, &na], &[])).await;
    assert_eq!(code, EXIT_OK, "stdout: {stdout}");
    assert_eq!(
        stdout,
        "queued job-a\nqueued job-b\ncomplete job-a\ncomplete job-b\n"
    );
    assert_eq!(
        sha256(&read_file(tmp.path().join("dl/job-a/a.bin"))),
        sha256(&a.data)
    );
    assert_eq!(
        sha256(&read_file(tmp.path().join("dl/job-b/b.bin"))),
        sha256(&b.data)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_leaves_the_job_resumable() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("int", "int.bin", 256 * 1024, 16, 9);
    let mock = mock_for(&[&file], &[], 150).await;
    let nzb = write_nzb(
        &tmp.path().join("nzbs"),
        "int.nzb",
        Some("int-job"),
        &[&file],
    );

    let args = parse_args(argv(tmp.path(), &mock, &[&nzb], &[])).unwrap();
    let mut out = Vec::new();
    let stop = tokio::time::sleep(Duration::from_millis(700));
    let code = tokio::time::timeout(
        Duration::from_secs(30),
        run_headless_until(args, &mut out, stop, None),
    )
    .await
    .unwrap();
    let stdout = String::from_utf8(out).unwrap();
    assert_eq!(code, EXIT_INTERRUPTED, "stdout: {stdout}");
    assert_eq!(stdout, "queued int-job\n");
    {
        let store = Store::open(&tmp.path().join("db")).unwrap();
        let jobs = store.list_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, JobState::Queued);
    }
    assert!(mock.body_requests() < 16);

    // Running the same command again picks the queued job up and finishes it.
    let (code, stdout) = run(argv(tmp.path(), &mock, &[&nzb], &[])).await;
    assert_eq!(code, EXIT_OK, "stdout: {stdout}");
    assert_eq!(stdout, "queued int-job\ncomplete int-job\n");
    assert_eq!(
        sha256(&read_file(tmp.path().join("dl/int-job/int.bin"))),
        sha256(&file.data)
    );
    assert_eq!(
        Store::open(&tmp.path().join("db"))
            .unwrap()
            .list_jobs()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_server_anywhere_is_a_usage_error() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("ns", "ns.bin", 16 * 1024, 1, 10);
    let nzb = write_nzb(&tmp.path().join("nzbs"), "ns.nzb", Some("ns-job"), &[&file]);
    let args = vec![
        "newskite".to_string(),
        "--nzb".into(),
        nzb.display().to_string(),
        "--db".into(),
        tmp.path().join("db").display().to_string(),
    ];
    let (code, stdout) = run(args).await;
    assert_eq!(code, EXIT_USAGE);
    assert_eq!(stdout, "");
    // Nothing was queued.
    assert!(Store::open(&tmp.path().join("db"))
        .unwrap()
        .list_jobs()
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_server_is_used_without_server_flag() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("st", "st.bin", 48 * 1024, 2, 11);
    let mock = mock_for(&[&file], &[], 0).await;
    {
        let store = Store::open(&tmp.path().join("db")).unwrap();
        let s = newskite_types::Settings {
            download_dir: tmp.path().join("stored-dl"),
            postprocess: newskite_types::PostProcessSettings {
                enabled: false,
                ..Default::default()
            },
            servers: vec![newskite_types::ServerConfig {
                host: mock.addr.ip().to_string(),
                port: mock.addr.port(),
                tls: false,
                username: USER.into(),
                password: PASS.into(),
                connections: 2,
                ..Default::default()
            }],
            ..Default::default()
        };
        store.save_settings(&s).unwrap();
    }
    let nzb = write_nzb(&tmp.path().join("nzbs"), "st.nzb", Some("st-job"), &[&file]);
    let args = vec![
        "newskite".to_string(),
        "--nzb".into(),
        nzb.display().to_string(),
        "--db".into(),
        tmp.path().join("db").display().to_string(),
    ];
    let (code, stdout) = run(args).await;
    assert_eq!(code, EXIT_OK, "stdout: {stdout}");
    assert_eq!(stdout, "queued st-job\ncomplete st-job\n");
    assert_eq!(
        sha256(&read_file(tmp.path().join("stored-dl/st-job/st.bin"))),
        sha256(&file.data)
    );
}

/// A headless run on a data directory another instance (here: the GUI's store) has open is
/// refused before it touches the database, so the other instance's live job stays put.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_run_is_refused_while_another_instance_holds_the_db() {
    let tmp = TempDir::new().unwrap();
    let file = TestFile::random("si", "si.bin", 16 * 1024, 2, 9);
    let mock = mock_for(&[&file], &[], 0).await;
    let gui_nzb = write_nzb(
        &tmp.path().join("nzbs"),
        "gui.nzb",
        Some("gui-job"),
        &[&file],
    );
    let cli_nzb = write_nzb(
        &tmp.path().join("nzbs"),
        "cli.nzb",
        Some("cli-job"),
        &[&file],
    );

    let gui = Store::open(&tmp.path().join("db")).unwrap();
    let live = gui
        .insert_job(&newskite_store::NewJob {
            name: "gui-job".into(),
            bytes_total: 16 * 1024,
            dest_dir: tmp.path().join("dl").join("gui-job"),
            source_path: std::fs::canonicalize(&gui_nzb).unwrap(),
            password: None,
        })
        .unwrap();
    gui.set_job_state(live.id, JobState::Downloading, None)
        .unwrap();

    let (code, stdout) = run(argv(tmp.path(), &mock, &[&cli_nzb], &[])).await;
    assert_eq!(code, EXIT_FAILURE, "stdout: {stdout}");
    assert_eq!(mock.body_requests(), 0);
    let jobs = gui.list_jobs().unwrap();
    assert_eq!(jobs.len(), 1, "the refused run must not add its job");
    assert_eq!(jobs[0].state, JobState::Downloading);
}

/// A headless run on a database with an older queued job (the GUI's backlog) downloads only
/// its own `--nzb` job: the backlog job is not sent to the `--server` override and stays
/// queued for the app.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_backlog_is_not_run_with_the_server_override() {
    let tmp = TempDir::new().unwrap();
    let gui_file = TestFile::random("bl", "backlog.bin", 32 * 1024, 2, 21);
    let cli_file = TestFile::random("me", "mine.bin", 48 * 1024, 3, 22);
    let mock = mock_for(&[&gui_file, &cli_file], &[], 0).await;
    let gui_nzb = write_nzb(
        &tmp.path().join("gui-nzbs"),
        "backlog.nzb",
        Some("backlog-job"),
        &[&gui_file],
    );
    let backlog_id = {
        let store = Store::open(&tmp.path().join("db")).unwrap();
        store
            .insert_job_at(
                &newskite_store::NewJob {
                    name: "backlog-job".into(),
                    bytes_total: gui_file.data.len() as u64,
                    dest_dir: tmp.path().join("gui-dl").join("backlog-job"),
                    source_path: std::fs::canonicalize(&gui_nzb).unwrap(),
                    password: None,
                },
                1_000,
            )
            .unwrap()
            .id
    };
    let nzb = write_nzb(&tmp.path().join("nzbs"), "mine.nzb", Some("mine-job"), &[&cli_file]);

    let (code, stdout) = run(argv(tmp.path(), &mock, &[&nzb], &[])).await;
    assert_eq!(code, EXIT_OK, "stdout: {stdout}");
    assert_eq!(stdout, "queued mine-job\ncomplete mine-job\n");
    assert_eq!(mock.body_requests(), 3, "only the CLI job's articles are fetched");

    let store = Store::open(&tmp.path().join("db")).unwrap();
    let backlog = store
        .list_jobs()
        .unwrap()
        .into_iter()
        .find(|j| j.id == backlog_id)
        .expect("backlog job still exists");
    assert_eq!(backlog.state, JobState::Queued);
    assert!(!tmp.path().join("gui-dl").exists());
}
