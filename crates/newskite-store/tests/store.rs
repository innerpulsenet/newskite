use std::path::{Path, PathBuf};
use std::sync::Arc;

use newskite_store::{
    Bookmark, JobFile, JobState, NewBookmark, NewJob, Store, StoreError, ThroughputSample, Totals, DB_FILE_NAME,
    SCHEMA_VERSION,
};
use newskite_types::{ServerConfig, Settings};

fn new_job(name: &str) -> NewJob {
    NewJob {
        name: name.to_owned(),
        bytes_total: 1000,
        dest_dir: PathBuf::from("/downloads").join(name),
        source_path: PathBuf::from(format!("/nzbs/{name}.nzb")),
        password: None,
    }
}

fn mem() -> Store {
    Store::open_in_memory().expect("open in-memory store")
}

fn nb(parent: Option<i64>, name: &str) -> NewBookmark {
    NewBookmark { parent_id: parent, name: name.to_owned(), newsgroup: None, color: None }
}

#[test]
fn store_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Store>();
}

#[test]
fn job_state_strings_are_exact() {
    let expected = [
        (JobState::Queued, "queued"),
        (JobState::Downloading, "downloading"),
        (JobState::Done, "done"),
        (JobState::Failed, "failed"),
        (JobState::Paused, "paused"),
    ];
    for (state, s) in expected {
        assert_eq!(state.as_str(), s);
        assert_eq!(state.to_string(), s);
        assert_eq!(s.parse::<JobState>().unwrap(), state);
    }
    assert!("Queued".parse::<JobState>().is_err());
    assert!("".parse::<JobState>().is_err());
    assert!(JobState::Paused.is_pending());
    assert!(!JobState::Done.is_pending());
}

#[test]
fn job_state_is_stored_as_exact_string() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut ids = Vec::new();
    for (i, state) in JobState::ALL.into_iter().enumerate() {
        let j = store.insert_job(&new_job(&format!("j{i}"))).unwrap();
        store.set_job_state(j.id, state, None).unwrap();
        ids.push((j.id, state));
    }
    drop(store);
    // Inspect the raw column with an independent connection.
    let conn = rusqlite::Connection::open(dir.path().join(DB_FILE_NAME)).unwrap();
    for (id, state) in ids {
        let raw: String = conn.query_row("SELECT state FROM jobs WHERE id = ?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(raw, state.as_str());
    }
}

#[test]
fn insert_and_get_job() {
    let store = mem();
    let mut nj = new_job("alpha");
    nj.password = Some("hunter2".into());
    let job = store.insert_job(&nj).unwrap();
    assert!(job.id > 0);
    assert_eq!(job.name, "alpha");
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.bytes_total, 1000);
    assert_eq!(job.bytes_done, 0);
    assert_eq!(job.dest_dir, nj.dest_dir);
    assert_eq!(job.source_path, nj.source_path);
    assert_eq!(job.password.as_deref(), Some("hunter2"));
    assert!(job.error.is_none());
    assert!(job.created_at > 0);
    assert_eq!(store.get_job(job.id).unwrap(), Some(job.clone()));
    assert_eq!(store.get_job(job.id + 100).unwrap(), None);

    // Debug output never contains the password.
    let dbg = format!("{job:?} {nj:?}");
    assert!(!dbg.contains("hunter2"), "{dbg}");
    assert!(dbg.contains("<redacted>"));
}

#[test]
fn huge_byte_counts_are_clamped() {
    let store = mem();
    let mut nj = new_job("big");
    nj.bytes_total = u64::MAX;
    let j = store.insert_job(&nj).unwrap();
    assert_eq!(j.bytes_total, i64::MAX as u64);
    store.set_job_progress(j.id, u64::MAX).unwrap();
    assert_eq!(store.get_job(j.id).unwrap().unwrap().bytes_done, i64::MAX as u64);
}

#[test]
fn list_jobs_is_id_ascending() {
    let store = mem();
    let a = store.insert_job_at(&new_job("a"), 300).unwrap();
    let b = store.insert_job_at(&new_job("b"), 100).unwrap();
    let c = store.insert_job_at(&new_job("c"), 200).unwrap();
    let ids: Vec<i64> = store.list_jobs().unwrap().iter().map(|j| j.id).collect();
    assert_eq!(ids, vec![a.id, b.id, c.id]);
    assert!(mem().list_jobs().unwrap().is_empty());
}

#[test]
fn job_by_source_returns_newest_match() {
    let store = mem();
    let first = store.insert_job(&new_job("x")).unwrap();
    store.set_job_state(first.id, JobState::Done, None).unwrap();
    let second = store.insert_job(&new_job("x")).unwrap();
    store.insert_job(&new_job("y")).unwrap();
    let found = store.job_by_source(Path::new("/nzbs/x.nzb")).unwrap().unwrap();
    assert_eq!(found.id, second.id);
    assert!(store.job_by_source(Path::new("/nzbs/none.nzb")).unwrap().is_none());
}

#[test]
fn set_state_progress_password() {
    let store = mem();
    let j = store.insert_job(&new_job("s")).unwrap();

    store.set_job_state(j.id, JobState::Failed, Some("3 articles missing")).unwrap();
    let got = store.get_job(j.id).unwrap().unwrap();
    assert_eq!(got.state, JobState::Failed);
    assert_eq!(got.error.as_deref(), Some("3 articles missing"));

    store.set_job_state(j.id, JobState::Queued, None).unwrap();
    let got = store.get_job(j.id).unwrap().unwrap();
    assert_eq!(got.state, JobState::Queued);
    assert!(got.error.is_none(), "None clears the error");

    store.set_job_progress(j.id, 512).unwrap();
    assert_eq!(store.get_job(j.id).unwrap().unwrap().bytes_done, 512);

    store.set_job_password(j.id, Some("secret")).unwrap();
    assert_eq!(store.get_job(j.id).unwrap().unwrap().password.as_deref(), Some("secret"));
    store.set_job_password(j.id, None).unwrap();
    assert!(store.get_job(j.id).unwrap().unwrap().password.is_none());

    // Missing ids are reported.
    assert!(matches!(store.set_job_state(999, JobState::Done, None), Err(StoreError::NotFound)));
    assert!(matches!(store.set_job_progress(999, 1), Err(StoreError::NotFound)));
    assert!(matches!(store.set_job_password(999, None), Err(StoreError::NotFound)));
}

#[test]
fn remove_job_removes_its_files() {
    let store = mem();
    let a = store.insert_job(&new_job("a")).unwrap();
    let b = store.insert_job(&new_job("b")).unwrap();
    for job in [&a, &b] {
        store
            .upsert_job_file(&JobFile {
                job_id: job.id,
                name: "f.rar".into(),
                bytes_total: 10,
                bytes_done: 10,
                state: "done".into(),
            })
            .unwrap();
    }
    store.remove_job(a.id).unwrap();
    assert!(store.get_job(a.id).unwrap().is_none());
    assert!(store.job_files(a.id).unwrap().is_empty());
    assert_eq!(store.job_files(b.id).unwrap().len(), 1, "other jobs untouched");
    assert!(matches!(store.remove_job(a.id), Err(StoreError::NotFound)));
}

#[test]
fn next_runnable_job_ordering() {
    let store = mem();
    assert!(store.next_runnable_job().unwrap().is_none());

    let newer = store.insert_job_at(&new_job("newer"), 2_000).unwrap();
    let tie_a = store.insert_job_at(&new_job("tie_a"), 1_000).unwrap();
    let tie_b = store.insert_job_at(&new_job("tie_b"), 1_000).unwrap();
    let oldest_done = store.insert_job_at(&new_job("oldest_done"), 10).unwrap();
    store.set_job_state(oldest_done.id, JobState::Done, None).unwrap();
    let old_failed = store.insert_job_at(&new_job("old_failed"), 20).unwrap();
    store.set_job_state(old_failed.id, JobState::Failed, Some("x")).unwrap();
    let old_downloading = store.insert_job_at(&new_job("old_dl"), 30).unwrap();
    store.set_job_state(old_downloading.id, JobState::Downloading, None).unwrap();

    // Oldest created_at wins; same timestamp -> lower id.
    assert_eq!(store.next_runnable_job().unwrap().unwrap().id, tie_a.id);
    store.set_job_state(tie_a.id, JobState::Done, None).unwrap();
    assert_eq!(store.next_runnable_job().unwrap().unwrap().id, tie_b.id);

    // Paused (held) jobs are skipped.
    store.set_job_state(tie_b.id, JobState::Paused, None).unwrap();
    assert_eq!(store.next_runnable_job().unwrap().unwrap().id, newer.id);
    store.set_job_state(tie_b.id, JobState::Failed, None).unwrap();
    assert_eq!(store.next_runnable_job().unwrap().unwrap().id, newer.id);
    store.set_job_state(newer.id, JobState::Done, None).unwrap();

    // Done / failed / downloading are never picked.
    assert!(store.next_runnable_job().unwrap().is_none());
}

#[test]
fn next_runnable_job_among_is_restricted_to_the_ids() {
    let store = mem();
    let oldest = store.insert_job_at(&new_job("oldest"), 10).unwrap();
    let b = store.insert_job_at(&new_job("b"), 30).unwrap();
    let a = store.insert_job_at(&new_job("a"), 20).unwrap();
    let done = store.insert_job_at(&new_job("done"), 5).unwrap();
    store.set_job_state(done.id, JobState::Done, None).unwrap();

    assert!(store.next_runnable_job_among(&[]).unwrap().is_none());
    assert!(store.next_runnable_job_among(&[done.id, 9_999]).unwrap().is_none());
    // The unlisted older job is skipped; among the listed ones the oldest wins.
    assert_eq!(store.next_runnable_job_among(&[b.id, a.id]).unwrap().unwrap().id, a.id);
    store.set_job_state(a.id, JobState::Paused, None).unwrap();
    assert_eq!(store.next_runnable_job_among(&[b.id, a.id]).unwrap().unwrap().id, b.id, "held job skipped");
    store.set_job_state(a.id, JobState::Done, None).unwrap();
    assert_eq!(store.next_runnable_job_among(&[b.id, a.id]).unwrap().unwrap().id, b.id);
    assert_eq!(store.next_runnable_job().unwrap().unwrap().id, oldest.id);
}

#[test]
fn queue_order_follows_explicit_positions() {
    let store = mem();
    let a = store.insert_job_at(&new_job("a"), 10).unwrap();
    let b = store.insert_job_at(&new_job("b"), 20).unwrap();
    let c = store.insert_job_at(&new_job("c"), 30).unwrap();
    let done = store.insert_job_at(&new_job("done"), 5).unwrap();
    store.set_job_state(done.id, JobState::Done, None).unwrap();
    assert_eq!(store.queue_order().unwrap(), vec![a.id, b.id, c.id], "created_at order, history excluded");

    store.set_queue_order(&[c.id, a.id, b.id]).unwrap();
    assert_eq!(store.queue_order().unwrap(), vec![c.id, a.id, b.id]);
    assert_eq!(store.next_runnable_job().unwrap().unwrap().id, c.id);
    // A job added after a reorder goes last, whatever its created_at.
    let d = store.insert_job_at(&new_job("d"), 1).unwrap();
    assert_eq!(store.queue_order().unwrap(), vec![c.id, a.id, b.id, d.id]);
    // Held jobs keep their place in the order but are not runnable.
    store.set_job_state(c.id, JobState::Paused, None).unwrap();
    assert_eq!(store.queue_order().unwrap()[0], c.id);
    assert_eq!(store.next_runnable_job().unwrap().unwrap().id, a.id);
}

#[test]
fn reset_interrupted_requeues_downloading_only() {
    let store = mem();
    let a = store.insert_job(&new_job("a")).unwrap();
    let b = store.insert_job(&new_job("b")).unwrap();
    let c = store.insert_job(&new_job("c")).unwrap();
    let d = store.insert_job(&new_job("d")).unwrap();
    store.set_job_state(a.id, JobState::Downloading, None).unwrap();
    store.set_job_state(b.id, JobState::Downloading, None).unwrap();
    store.set_job_state(c.id, JobState::Done, None).unwrap();
    store.set_job_state(d.id, JobState::Paused, None).unwrap();
    store.set_job_progress(a.id, 77).unwrap();

    assert_eq!(store.reset_interrupted().unwrap(), 2);
    let a2 = store.get_job(a.id).unwrap().unwrap();
    assert_eq!(a2.state, JobState::Queued);
    assert_eq!(a2.bytes_done, 77, "progress kept");
    assert_eq!(store.get_job(b.id).unwrap().unwrap().state, JobState::Queued);
    assert_eq!(store.get_job(c.id).unwrap().unwrap().state, JobState::Done);
    assert_eq!(store.get_job(d.id).unwrap().unwrap().state, JobState::Paused);
    assert_eq!(store.reset_interrupted().unwrap(), 0);
}

#[test]
fn job_files_upsert_and_unique_name() {
    let store = mem();
    let j = store.insert_job(&new_job("files")).unwrap();
    let mut f = JobFile { job_id: j.id, name: "b.r00".into(), bytes_total: 100, bytes_done: 0, state: "queued".into() };
    store.upsert_job_file(&f).unwrap();
    f.bytes_done = 100;
    f.state = "done".into();
    store.upsert_job_file(&f).unwrap();
    store
        .upsert_job_file(&JobFile {
            job_id: j.id,
            name: "a.rar".into(),
            bytes_total: 50,
            bytes_done: 10,
            state: "downloading".into(),
        })
        .unwrap();

    let files = store.job_files(j.id).unwrap();
    assert_eq!(files.len(), 2, "same name updates in place");
    assert_eq!(files[0].name, "a.rar");
    assert_eq!(files[1], f);

    assert!(store.job_files(12345).unwrap().is_empty());
    let orphan = JobFile { job_id: 12345, name: "x".into(), bytes_total: 1, bytes_done: 0, state: "queued".into() };
    assert!(matches!(store.upsert_job_file(&orphan), Err(StoreError::NotFound)));
}

#[test]
fn persistence_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("a").join("db");
    let id = {
        let store = Store::open(&nested).unwrap();
        assert_eq!(store.path(), Some(nested.join(DB_FILE_NAME).as_path()));
        let j = store.insert_job(&new_job("persist")).unwrap();
        store.set_job_state(j.id, JobState::Downloading, None).unwrap();
        store.set_job_progress(j.id, 4242).unwrap();
        store
            .upsert_job_file(&JobFile {
                job_id: j.id,
                name: "part1.rar".into(),
                bytes_total: 4000,
                bytes_done: 4000,
                state: "done".into(),
            })
            .unwrap();
        let s = Settings { max_connections: 7, ..Default::default() };
        store.save_settings(&s).unwrap();
        store.add_bookmark(&nb(None, "Linux")).unwrap();
        store.add_sample(&ThroughputSample { ts: 1_700_000_000, bytes_up: 0, bytes_down: 9 }).unwrap();
        store.add_downloaded_bytes(4242).unwrap();
        j.id
    };

    let store = Store::open(&nested).unwrap();
    let j = store.get_job(id).unwrap().unwrap();
    assert_eq!(j.state, JobState::Downloading);
    assert_eq!(j.bytes_done, 4242);
    assert_eq!(store.reset_interrupted().unwrap(), 1);
    assert_eq!(store.get_job(id).unwrap().unwrap().state, JobState::Queued);
    assert_eq!(store.job_files(id).unwrap()[0].state, "done");
    assert_eq!(store.load_settings().unwrap().max_connections, 7);
    assert_eq!(store.list_bookmarks().unwrap()[0].name, "Linux");
    assert_eq!(store.samples_since(0).unwrap().len(), 1);
    assert_eq!(store.totals().unwrap().downloaded_bytes, 4242);

    // WAL mode is active on file databases and the schema version is set.
    let conn = rusqlite::Connection::open(nested.join(DB_FILE_NAME)).unwrap();
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn refuses_newer_schema() {
    let dir = tempfile::tempdir().unwrap();
    drop(Store::open(dir.path()).unwrap());
    {
        let conn = rusqlite::Connection::open(dir.path().join(DB_FILE_NAME)).unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1)).unwrap();
    }
    assert!(matches!(Store::open(dir.path()), Err(StoreError::Invalid(_))));
}

#[test]
fn open_fails_when_dir_is_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    assert!(Store::open(&file).is_err());
}

#[test]
fn bookmarks_tree_order_and_update() {
    let store = mem();
    assert!(store.list_bookmarks().unwrap().is_empty());
    let root_a = store.add_bookmark(&nb(None, "A")).unwrap();
    let root_b = store.add_bookmark(&nb(None, "B")).unwrap();
    let a1 = store.add_bookmark(&nb(Some(root_a.id), "A1")).unwrap();
    let a2 = store
        .add_bookmark(&NewBookmark {
            parent_id: Some(root_a.id),
            name: "A2".into(),
            newsgroup: Some("alt.binaries.test".into()),
            color: Some("#3366cc".into()),
        })
        .unwrap();
    let a1x = store.add_bookmark(&nb(Some(a1.id), "A1x")).unwrap();
    let b1 = store.add_bookmark(&nb(Some(root_b.id), "B1")).unwrap();

    assert!(root_a.expanded);
    assert_eq!((root_a.order, root_b.order), (0, 1));
    assert_eq!((a1.order, a2.order), (0, 1));
    assert_eq!(a2.newsgroup.as_deref(), Some("alt.binaries.test"));
    assert_eq!(a2.color.as_deref(), Some("#3366cc"));

    let names = |s: &Store| s.list_bookmarks().unwrap().into_iter().map(|b| b.name).collect::<Vec<_>>();
    assert_eq!(names(&store), ["A", "A1", "A1x", "A2", "B", "B1"]);

    // Reorder roots and collapse one.
    let mut b = root_b.clone();
    b.order = -1;
    b.expanded = false;
    b.color = Some("#ff0000".into());
    store.update_bookmark(&b).unwrap();
    assert_eq!(names(&store), ["B", "B1", "A", "A1", "A1x", "A2"]);
    let stored: Bookmark = store.list_bookmarks().unwrap().into_iter().find(|x| x.id == b.id).unwrap();
    assert_eq!(stored, b);

    // Move A2 under B.
    let mut moved = a2.clone();
    moved.parent_id = Some(root_b.id);
    store.update_bookmark(&moved).unwrap();
    assert_eq!(names(&store), ["B", "B1", "A2", "A", "A1", "A1x"]);
    assert_eq!(store.list_bookmarks().unwrap()[2].parent_id, Some(root_b.id));
    assert!(b1.order < moved.order, "A2 keeps its explicit order among B's children");

    // Cycles and bad parents are refused.
    let mut cyc = root_a.clone();
    cyc.parent_id = Some(a1x.id);
    assert!(matches!(store.update_bookmark(&cyc), Err(StoreError::Invalid(_))));
    let mut selfp = root_a.clone();
    selfp.parent_id = Some(root_a.id);
    assert!(matches!(store.update_bookmark(&selfp), Err(StoreError::Invalid(_))));
    let mut missing_parent = root_a.clone();
    missing_parent.parent_id = Some(9999);
    assert!(matches!(store.update_bookmark(&missing_parent), Err(StoreError::Invalid(_))));
    assert!(matches!(store.add_bookmark(&nb(Some(9999), "orphan")), Err(StoreError::Invalid(_))));
    let mut ghost = root_a.clone();
    ghost.id = 9999;
    assert!(matches!(store.update_bookmark(&ghost), Err(StoreError::NotFound)));
}

#[test]
fn remove_bookmark_is_recursive() {
    let store = mem();
    let root = store.add_bookmark(&nb(None, "root")).unwrap();
    let keep = store.add_bookmark(&nb(None, "keep")).unwrap();
    let child = store.add_bookmark(&nb(Some(root.id), "child")).unwrap();
    store.add_bookmark(&nb(Some(child.id), "grandchild")).unwrap();
    store.add_bookmark(&nb(Some(keep.id), "keep-child")).unwrap();

    store.remove_bookmark(root.id).unwrap();
    let left: Vec<String> = store.list_bookmarks().unwrap().into_iter().map(|b| b.name).collect();
    assert_eq!(left, ["keep", "keep-child"]);
    assert!(matches!(store.remove_bookmark(root.id), Err(StoreError::NotFound)));
    assert!(matches!(store.remove_bookmark(child.id), Err(StoreError::NotFound)));
}

#[test]
fn settings_round_trip_and_defaults() {
    let store = mem();
    // Absent -> default().normalized().
    assert_eq!(store.load_settings().unwrap(), Settings::default().normalized());

    let mut s = Settings {
        max_connections: 12,
        speed_limit_kbps: 2048,
        watch_enabled: true,
        watch_folder: PathBuf::from("/home/u/nzb-watch"),
        servers: vec![
            ServerConfig { host: "news.example.com".into(), username: "u".into(), password: "p".into(), ..Default::default() },
            ServerConfig { host: "backup.example.com".into(), port: 119, tls: false, priority: 1, ..Default::default() },
        ],
        ..Default::default()
    };
    store.save_settings(&s).unwrap();
    assert_eq!(store.load_settings().unwrap(), s);

    // Saving again overwrites.
    s.retries = 9;
    store.save_settings(&s).unwrap();
    assert_eq!(store.load_settings().unwrap().retries, 9);

    // Saving a document with no servers loads back normalized.
    let mut empty = Settings::default();
    empty.servers.clear();
    empty.max_connections = 0;
    store.save_settings(&empty).unwrap();
    let loaded = store.load_settings().unwrap();
    assert_eq!(loaded.servers.len(), 1);
    assert!(!loaded.servers[0].enabled);
    assert_eq!(loaded.max_connections, 1);
}

#[test]
fn settings_partial_and_unknown_json_loads() {
    let dir = tempfile::tempdir().unwrap();
    drop(Store::open(dir.path()).unwrap());
    let conn = rusqlite::Connection::open(dir.path().join(DB_FILE_NAME)).unwrap();
    conn.execute(
        "INSERT INTO settings (key, value) VALUES ('settings', ?1)",
        [r#"{"max_connections": 4, "some_future_field": {"x": 1}, "servers": []}"#],
    )
    .unwrap();
    drop(conn);
    let store = Store::open(dir.path()).unwrap();
    let s = store.load_settings().unwrap();
    assert_eq!(s.max_connections, 4);
    assert_eq!(s.retries, Settings::default().retries);
    assert_eq!(s.servers.len(), 1, "normalized adds a placeholder server");

    // Corrupt JSON is reported, not silently replaced.
    let conn = rusqlite::Connection::open(dir.path().join(DB_FILE_NAME)).unwrap();
    conn.execute("UPDATE settings SET value = '{not json' WHERE key = 'settings'", []).unwrap();
    drop(conn);
    assert!(matches!(store.load_settings(), Err(StoreError::Json(_))));
}

#[test]
fn throughput_upsert_since_and_prune() {
    let store = mem();
    store.add_sample(&ThroughputSample { ts: 100, bytes_up: 1, bytes_down: 1000 }).unwrap();
    store.add_sample(&ThroughputSample { ts: 100, bytes_up: 2, bytes_down: 500 }).unwrap();
    store.add_sample(&ThroughputSample { ts: 101, bytes_up: 0, bytes_down: 10 }).unwrap();
    store.add_sample(&ThroughputSample { ts: 99, bytes_up: 0, bytes_down: 5 }).unwrap();

    let all = store.samples_since(0).unwrap();
    assert_eq!(
        all,
        vec![
            ThroughputSample { ts: 99, bytes_up: 0, bytes_down: 5 },
            ThroughputSample { ts: 100, bytes_up: 3, bytes_down: 1500 },
            ThroughputSample { ts: 101, bytes_up: 0, bytes_down: 10 },
        ]
    );
    // Inclusive lower bound.
    assert_eq!(store.samples_since(100).unwrap().len(), 2);
    assert!(store.samples_since(102).unwrap().is_empty());

    // Saturating upsert.
    store.add_sample(&ThroughputSample { ts: 200, bytes_up: 0, bytes_down: u64::MAX }).unwrap();
    store.add_sample(&ThroughputSample { ts: 200, bytes_up: 0, bytes_down: 10 }).unwrap();
    assert_eq!(store.samples_since(200).unwrap()[0].bytes_down, i64::MAX as u64);

    // Strict upper bound for pruning.
    assert_eq!(store.prune_samples_before(100).unwrap(), 1);
    assert_eq!(store.samples_since(0).unwrap()[0].ts, 100);
    assert_eq!(store.prune_samples_before(100).unwrap(), 0);
    assert_eq!(store.prune_samples_before(i64::MAX).unwrap(), 3);
    assert!(store.samples_since(i64::MIN).unwrap().is_empty());
}

#[test]
fn totals_counts_states_and_bytes() {
    let store = mem();
    assert_eq!(store.totals().unwrap(), Totals::default());

    let states = [
        JobState::Queued,
        JobState::Downloading,
        JobState::Paused,
        JobState::Done,
        JobState::Done,
        JobState::Failed,
    ];
    for (i, st) in states.into_iter().enumerate() {
        let j = store.insert_job(&new_job(&format!("t{i}"))).unwrap();
        store.set_job_state(j.id, st, None).unwrap();
    }
    store.add_downloaded_bytes(1_000).unwrap();
    store.add_downloaded_bytes(234).unwrap();
    store.add_downloaded_bytes(0).unwrap();
    assert_eq!(
        store.totals().unwrap(),
        Totals { downloaded_bytes: 1_234, jobs_done: 2, jobs_pending: 3, jobs_failed: 1 }
    );

    store.add_downloaded_bytes(u64::MAX).unwrap();
    assert_eq!(store.totals().unwrap().downloaded_bytes, i64::MAX as u64);
}

#[test]
fn concurrent_access_from_threads() {
    let store = Arc::new(mem());
    let job = store.insert_job(&new_job("shared")).unwrap();
    let handles: Vec<_> = (0..8)
        .map(|t| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                for i in 0..50 {
                    store.add_sample(&ThroughputSample { ts: i, bytes_up: 0, bytes_down: 1 }).unwrap();
                    store.add_downloaded_bytes(1).unwrap();
                    store.set_job_progress(job.id, (t * 100 + i) as u64).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let samples = store.samples_since(0).unwrap();
    assert_eq!(samples.len(), 50);
    assert!(samples.iter().all(|s| s.bytes_down == 8));
    assert_eq!(store.totals().unwrap().downloaded_bytes, 400);
}

#[test]
fn foreign_keys_are_enforced() {
    // Deleting a job via SQL cascade path is covered by remove_job; here check the pragma
    // through behavior: a bookmark removed takes its children with it even when the store
    // is reopened (pragma is per-connection).
    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(dir.path()).unwrap();
        let p = store.add_bookmark(&nb(None, "p")).unwrap();
        store.add_bookmark(&nb(Some(p.id), "c")).unwrap();
    }
    let store = Store::open(dir.path()).unwrap();
    let p = store.list_bookmarks().unwrap()[0].id;
    store.remove_bookmark(p).unwrap();
    assert!(store.list_bookmarks().unwrap().is_empty());
}

#[test]
fn second_open_of_the_same_dir_is_refused_until_the_first_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let first = Store::open(dir.path()).unwrap();
    let j = first.insert_job(&new_job("live")).unwrap();
    first.set_job_state(j.id, JobState::Downloading, None).unwrap();

    // A second instance (same process or another one) must not get the database, so it
    // can never reset the first instance's live job.
    match Store::open(dir.path()) {
        Err(StoreError::Invalid(msg)) => assert!(msg.contains("another Newskite instance"), "{msg}"),
        other => panic!("expected the second open to be refused, got {other:?}"),
    }
    assert_eq!(first.get_job(j.id).unwrap().unwrap().state, JobState::Downloading);

    drop(first);
    let again = Store::open(dir.path()).unwrap();
    assert_eq!(again.get_job(j.id).unwrap().unwrap().state, JobState::Downloading);
}

#[test]
fn pending_job_by_source_finds_older_pending_jobs() {
    let store = mem();
    // Job 1 failed and was later retried (pending again); job 2 for the same NZB is done.
    let first = store.insert_job(&new_job("x")).unwrap();
    let second = store.insert_job(&new_job("x")).unwrap();
    store.set_job_state(second.id, JobState::Done, None).unwrap();
    assert_eq!(store.job_by_source(Path::new("/nzbs/x.nzb")).unwrap().unwrap().id, second.id);
    let pending = store.pending_job_by_source(Path::new("/nzbs/x.nzb")).unwrap().unwrap();
    assert_eq!(pending.id, first.id);

    for state in [JobState::Downloading, JobState::Paused] {
        store.set_job_state(first.id, state, None).unwrap();
        assert_eq!(store.pending_job_by_source(Path::new("/nzbs/x.nzb")).unwrap().unwrap().id, first.id);
    }
    for state in [JobState::Done, JobState::Failed] {
        store.set_job_state(first.id, state, None).unwrap();
        assert!(store.pending_job_by_source(Path::new("/nzbs/x.nzb")).unwrap().is_none());
    }
    assert!(store.pending_job_by_source(Path::new("/nzbs/none.nzb")).unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn database_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("new").join("db");
    {
        let store = Store::open(&data).unwrap();
        let mut s = Settings::default();
        s.servers.push(ServerConfig { password: "hunter2".into(), ..Default::default() });
        store.save_settings(&s).unwrap();
        assert_eq!(mode(&data), 0o700, "created data dir");
        for name in [DB_FILE_NAME.to_owned(), format!("{DB_FILE_NAME}-wal"), format!("{DB_FILE_NAME}-shm")] {
            assert_eq!(mode(&data.join(&name)), 0o600, "{name}");
        }
        assert_eq!(mode(&data.join(newskite_store::LOCK_FILE_NAME)), 0o600, "lock file");
    }

    // Files left world-readable by an older version are tightened on the next open.
    let old = dir.path().join("old");
    drop(Store::open(&old).unwrap());
    std::fs::set_permissions(old.join(DB_FILE_NAME), std::fs::Permissions::from_mode(0o644)).unwrap();
    let store = Store::open(&old).unwrap();
    store.save_settings(&Settings::default()).unwrap();
    assert_eq!(mode(&old.join(DB_FILE_NAME)), 0o600);
    assert_eq!(mode(&old.join(format!("{DB_FILE_NAME}-wal"))), 0o600);
}

#[test]
fn saving_over_an_unreadable_settings_document_keeps_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let corrupt = r#"{"servers": [{"host": "news.example.com", "port": "563", "password": "hunter2"}]}"#;
    {
        let store = Store::open(dir.path()).unwrap();
        store.save_settings(&Settings::default()).unwrap();
    }
    let conn = rusqlite::Connection::open(dir.path().join(DB_FILE_NAME)).unwrap();
    conn.execute("UPDATE settings SET value = ?1 WHERE key = 'settings'", [corrupt]).unwrap();
    drop(conn);

    let store = Store::open(dir.path()).unwrap();
    assert!(matches!(store.load_settings(), Err(StoreError::Json(_))));
    assert!(store.settings_backups().unwrap().is_empty());

    // Saving replaces the live document, but the unreadable one is kept, once.
    let s = Settings { max_connections: 3, ..Default::default() };
    store.save_settings(&s).unwrap();
    store.save_settings(&s).unwrap();
    assert_eq!(store.load_settings().unwrap().max_connections, 3);
    let backups = store.settings_backups().unwrap();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert!(backups[0].0.starts_with("settings.unreadable-"), "{}", backups[0].0);
    assert_eq!(backups[0].1, corrupt);

    // The same unreadable text saved over again is not backed up twice.
    let conn = rusqlite::Connection::open(dir.path().join(DB_FILE_NAME)).unwrap();
    conn.execute("UPDATE settings SET value = ?1 WHERE key = 'settings'", [corrupt]).unwrap();
    drop(conn);
    store.save_settings(&s).unwrap();
    assert_eq!(store.settings_backups().unwrap().len(), 1);
}

#[test]
fn seen_sources_survive_job_removal_and_track_the_file_fingerprint() {
    let store = mem();
    let p = Path::new("/watch/a.nzb");
    assert!(!store.source_seen(p, 100, 5).unwrap());
    store.mark_source_seen(p, 100, 5).unwrap();
    assert!(store.source_seen(p, 100, 5).unwrap());
    // A different size or mtime means a new file under the same name.
    assert!(!store.source_seen(p, 101, 5).unwrap());
    assert!(!store.source_seen(p, 100, 6).unwrap());
    store.mark_source_seen(p, 101, 6).unwrap();
    assert!(store.source_seen(p, 101, 6).unwrap());
    assert!(!store.source_seen(p, 100, 5).unwrap());

    let j = store.insert_job(&new_job("a")).unwrap();
    store.remove_job(j.id).unwrap();
    assert!(store.source_seen(p, 101, 6).unwrap());
}
