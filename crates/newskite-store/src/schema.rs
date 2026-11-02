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
