//! Schema migrations, driven by SQLite's `PRAGMA user_version`.
//!
//! Each entry in [`MIGRATIONS`] upgrades the schema from version `index` to `index + 1`.
//! Migrations are append-only: never edit a shipped entry, add a new one instead.

use rusqlite::Connection;

use crate::StoreError;

/// Ordered migration scripts. `MIGRATIONS[n]` moves `user_version` from `n` to `n + 1`.
const MIGRATIONS: &[&str] = &[
    // v1: initial schema.
    r#"
    CREATE TABLE jobs (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        name         TEXT    NOT NULL,
        state        TEXT    NOT NULL CHECK (state IN ('queued','downloading','done','failed','paused')),
        bytes_total  INTEGER NOT NULL DEFAULT 0,
        bytes_done   INTEGER NOT NULL DEFAULT 0,
        dest_dir     TEXT    NOT NULL,
        source_path  TEXT    NOT NULL,
        created_at   INTEGER NOT NULL,
        error        TEXT,
        password     TEXT
    );
    CREATE INDEX jobs_state_created ON jobs (state, created_at, id);
    CREATE INDEX jobs_source ON jobs (source_path);

    CREATE TABLE job_files (
        job_id       INTEGER NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
        name         TEXT    NOT NULL,
        bytes_total  INTEGER NOT NULL DEFAULT 0,
        bytes_done   INTEGER NOT NULL DEFAULT 0,
        state        TEXT    NOT NULL,
        PRIMARY KEY (job_id, name)
    );

    CREATE TABLE bookmarks (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        parent_id  INTEGER REFERENCES bookmarks (id) ON DELETE CASCADE,
        name       TEXT    NOT NULL,
        newsgroup  TEXT,
        color      TEXT,
        expanded   INTEGER NOT NULL DEFAULT 1,
        ord        INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX bookmarks_parent ON bookmarks (parent_id, ord, id);

    CREATE TABLE throughput (
        ts          INTEGER PRIMARY KEY,
        bytes_up    INTEGER NOT NULL DEFAULT 0,
        bytes_down  INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE settings (
        key    TEXT PRIMARY KEY,
        value  TEXT NOT NULL
    );

    CREATE TABLE counters (
        key    TEXT PRIMARY KEY,
        value  INTEGER NOT NULL DEFAULT 0
    );
    "#,
    // v2: watch-folder sources that were imported (or whose job was removed), kept when
    // jobs are deleted so the watch folder does not import them again.
    r#"
    CREATE TABLE seen_sources (
        source_path  TEXT    PRIMARY KEY,
        size         INTEGER NOT NULL,
        mtime        INTEGER NOT NULL,
        seen_at      INTEGER NOT NULL
    );
    "#,
    // v3: an explicit queue position. 0 for every existing row, so the queue keeps its
    // created_at order until the user moves a job.
    r#"
    ALTER TABLE jobs ADD COLUMN position INTEGER NOT NULL DEFAULT 0;
    "#,
    // v4: what happened to a job (timings, article counts, post-processing outcome as
    // JSON) and a per-job log that survives restarts.
    r#"
    ALTER TABLE jobs ADD COLUMN started_at INTEGER;
    ALTER TABLE jobs ADD COLUMN finished_at INTEGER;
    ALTER TABLE jobs ADD COLUMN download_ms INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE jobs ADD COLUMN post_ms INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE jobs ADD COLUMN articles_total INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE jobs ADD COLUMN articles_missing INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE jobs ADD COLUMN outcome TEXT;
    CREATE TABLE job_log (
        id      INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id  INTEGER NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
        ts      INTEGER NOT NULL,
        level   TEXT    NOT NULL,
        text    TEXT    NOT NULL
    );
    CREATE INDEX job_log_job ON job_log (job_id, id);
    "#,
    // v5: each job's article map (which articles arrived or were missing), saved by the
    // service when a run ends, as encoded text.
    r#"
    CREATE TABLE article_maps (
        job_id  INTEGER PRIMARY KEY REFERENCES jobs (id) ON DELETE CASCADE,
        map     TEXT    NOT NULL
    );
    "#,
    // v6: indexes for the queue order v3 introduced, so picking the next job reads it from
    // an index instead of sorting, and a new job's position (MAX) is one index lookup. The
    // new queue index starts with `state` too, so it replaces jobs_state_created.
    r#"
    CREATE INDEX jobs_queue ON jobs (state, position, created_at, id);
    CREATE INDEX jobs_position ON jobs (position);
    DROP INDEX jobs_state_created;
    "#,
];

/// The schema version this build writes.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Bring the database up to [`SCHEMA_VERSION`]. Each step runs in its own transaction
/// together with the `user_version` bump, so a crash mid-migration leaves the previous
/// version intact. Refuses databases written by a newer build.
pub(crate) fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(StoreError::Invalid(format!(
            "database schema version {current} is newer than supported version {SCHEMA_VERSION}"
        )));
    }
    if current < 0 {
        return Err(StoreError::Invalid(format!("invalid database schema version {current}")));
    }
    for (idx, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        // PRAGMA does not accept bound parameters; the value is a trusted integer.
        tx.execute_batch(&format!("PRAGMA user_version = {}", idx + 1))?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JobState, NewJob, Store, DB_FILE_NAME};

    /// The schema version of the last release (0.9.0).
    const RELEASED_VERSION: usize = 5;

    fn plan(conn: &Connection, sql: &str) -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let nulls = std::iter::repeat_n(rusqlite::types::Null, stmt.parameter_count());
        let rows = stmt.query_map(rusqlite::params_from_iter(nulls), |r| r.get::<_, String>(3)).unwrap();
        rows.map(Result::unwrap).collect::<Vec<_>>().join("; ")
    }

    #[test]
    fn upgrades_a_released_database() {
        let dir = tempfile::tempdir().unwrap();
        {
            // A database as 0.9.0 left it, with a reordered queue and some history.
            let conn = Connection::open(dir.path().join(DB_FILE_NAME)).unwrap();
            for sql in &MIGRATIONS[..RELEASED_VERSION] {
                conn.execute_batch(sql).unwrap();
            }
            conn.execute_batch(&format!("PRAGMA user_version = {RELEASED_VERSION}")).unwrap();
            conn.execute_batch(
                "INSERT INTO jobs (name, state, dest_dir, source_path, created_at, position) VALUES
                     ('old-done', 'done', '/d/a', '/n/a.nzb', 100, 0),
                     ('second', 'queued', '/d/b', '/n/b.nzb', 200, 2),
                     ('first', 'queued', '/d/c', '/n/c.nzb', 300, 1),
                     ('held', 'paused', '/d/d', '/n/d.nzb', 400, 3);
                 INSERT INTO job_log (job_id, ts, level, text) VALUES (2, 1, 'info', 'kept');",
            )
            .unwrap();
        }

        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.list_jobs().unwrap().len(), 4);
        assert_eq!(store.next_runnable_job().unwrap().unwrap().name, "first");
        assert_eq!(store.queue_order().unwrap(), [3, 2, 4]);
        assert_eq!(store.job_log(2).unwrap()[0].text, "kept");
        // A reordered queue: a new job goes after the highest position.
        let new = NewJob {
            name: "new".into(),
            bytes_total: 1,
            dest_dir: "/d/e".into(),
            source_path: "/n/e.nzb".into(),
            password: None,
        };
        let added = store.insert_job(&new).unwrap();
        assert_eq!(store.queue_order().unwrap(), [3, 2, 4, added.id]);
        store.set_job_state(3, JobState::Done, None).unwrap();
        assert_eq!(store.next_runnable_job().unwrap().unwrap().name, "second");
        drop(store);

        let conn = Connection::open(dir.path().join(DB_FILE_NAME)).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'jobs' AND sql IS NOT NULL ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(indexes, ["jobs_position", "jobs_queue", "jobs_source"]);
    }

    #[test]
    fn queue_queries_use_indexes() {
        let store = Store::open_in_memory().unwrap();
        let conn = store.conn();
        let next = plan(&conn, crate::store::SQL_NEXT_RUNNABLE);
        assert!(next.contains("jobs_queue") && !next.contains("TEMP B-TREE"), "{next}");
        let max = plan(&conn, "SELECT MAX(position) FROM jobs");
        assert!(max.contains("jobs_position"), "{max}");
        let insert = plan(&conn, crate::store::SQL_INSERT_JOB);
        assert!(!insert.contains("SCAN jobs"), "{insert}");
    }
}
