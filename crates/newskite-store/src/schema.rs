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
