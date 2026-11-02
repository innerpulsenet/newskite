//! The [`Store`] itself: a mutex-guarded SQLite connection with typed accessors.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use newskite_types::Settings;
use rusqlite::types::Type;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::model::{Bookmark, Job, JobFile, JobLogLine, JobState, JobStats, NewBookmark, NewJob, ThroughputSample, Totals};
use crate::schema;
use crate::StoreError;

/// File name of the database inside the directory passed to [`Store::open`].
pub const DB_FILE_NAME: &str = "newskite.sqlite3";

/// File name of the single-instance lock inside the directory passed to [`Store::open`].
/// While a [`Store`] opened from that directory is alive it holds an exclusive advisory
/// lock on this file; a second [`Store::open`] of the same directory is refused.
pub const LOCK_FILE_NAME: &str = "newskite.lock";

/// How long a statement waits on a locked database before failing with `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Key of the settings document in the `settings` table.
const SETTINGS_KEY: &str = "settings";

/// Key prefix of unreadable settings documents kept by [`Store::save_settings`].
const SETTINGS_BACKUP_PREFIX: &str = "settings.unreadable-";

/// Key of the lifetime downloaded-bytes counter in the `counters` table.
const DOWNLOADED_BYTES_KEY: &str = "downloaded_bytes";

const JOB_COLUMNS: &str = "id, name, state, bytes_total, bytes_done, dest_dir, source_path, created_at, error, password, \
     started_at, finished_at, download_ms, post_ms, articles_total, articles_missing, outcome";

/// Lines kept per job in `job_log`.
pub const JOB_LOG_KEEP: i64 = 500;

/// Lifetime article counters in the `counters` table.
const ARTICLES_FETCHED_KEY: &str = "articles_fetched";
const ARTICLES_MISSING_KEY: &str = "articles_missing";

/// Queue order: explicit position, then age, then id.
const QUEUE_ORDER: &str = "position ASC, created_at ASC, id ASC";

const BOOKMARK_COLUMNS: &str = "id, parent_id, name, newsgroup, color, expanded, ord";

/// Thread-safe handle to the Newskite database.
///
/// All methods take `&self`; the connection is guarded by a [`Mutex`], so a `Store` can be
/// shared between threads (`Arc<Store>`). Methods that address a row by id return
/// [`StoreError::NotFound`] when that row does not exist, unless documented otherwise.
pub struct Store {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
    /// The locked [`LOCK_FILE_NAME`] (file stores only). Dropping the store closes it,
    /// which releases the lock.
    _lock: Option<File>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("path", &self.path).finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------------------

/// `u64` → SQLite integer, clamped to `i64::MAX`.
fn to_db(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// SQLite integer → `u64`; negative values (never written by this crate) read as 0.
fn from_db(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// Paths are stored as UTF-8 text (lossy for non-UTF-8 paths; see crate docs).
fn path_to_db(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Current time as unix seconds (0 if the clock is before 1970).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn job_from_row(row: &Row<'_>) -> rusqlite::Result<Job> {
    let state_str: String = row.get(2)?;
    let state = state_str
        .parse::<JobState>()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(e)))?;
    Ok(Job {
        id: row.get(0)?,
        name: row.get(1)?,
        state,
        bytes_total: from_db(row.get(3)?),
        bytes_done: from_db(row.get(4)?),
        dest_dir: PathBuf::from(row.get::<_, String>(5)?),
        source_path: PathBuf::from(row.get::<_, String>(6)?),
        created_at: row.get(7)?,
        error: row.get(8)?,
        password: row.get(9)?,
        stats: JobStats {
            started_at: row.get(10)?,
            finished_at: row.get(11)?,
            download_ms: from_db(row.get(12)?),
            post_ms: from_db(row.get(13)?),
            articles_total: from_db(row.get(14)?),
            articles_missing: from_db(row.get(15)?),
            // An outcome written by another build that no longer parses is dropped.
            outcome: row.get::<_, Option<String>>(16)?.and_then(|s| serde_json::from_str(&s).ok()).map(Box::new),
        },
    })
}

fn bookmark_from_row(row: &Row<'_>) -> rusqlite::Result<Bookmark> {
    Ok(Bookmark {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        name: row.get(2)?,
        newsgroup: row.get(3)?,
        color: row.get(4)?,
        expanded: row.get(5)?,
        order: row.get(6)?,
    })
}

fn job_file_from_row(row: &Row<'_>) -> rusqlite::Result<JobFile> {
    Ok(JobFile {
        job_id: row.get(0)?,
        name: row.get(1)?,
        bytes_total: from_db(row.get(2)?),
        bytes_done: from_db(row.get(3)?),
        state: row.get(4)?,
    })
}

/// Map "zero rows changed" to [`StoreError::NotFound`].
fn require_changed(n: usize) -> Result<(), StoreError> {
    if n == 0 {
        Err(StoreError::NotFound)
    } else {
        Ok(())
    }
}

fn bookmark_exists(conn: &Connection, id: i64) -> Result<bool, StoreError> {
    Ok(conn
        .query_row("SELECT 1 FROM bookmarks WHERE id = ?1", [id], |_| Ok(()))
        .optional()?
        .is_some())
}

/// Create `dir` and its missing parents; on Unix the directories created here get mode
/// `0700` (existing ones are left alone).
fn create_private_dir(dir: &Path) -> Result<(), StoreError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    Ok(())
}

/// Open options that create a file with mode `0600` on Unix.
fn private_file_options() -> std::fs::OpenOptions {
    let mut opts = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts
}

/// Take the exclusive single-instance lock on `dir/`[`LOCK_FILE_NAME`]. Returns `None`
/// only when the filesystem does not support file locks at all, in which case the store
/// still opens (without the guard).
fn lock_data_dir(dir: &Path) -> Result<Option<File>, StoreError> {
    let lock_path = dir.join(LOCK_FILE_NAME);
    let file = private_file_options().read(true).write(true).create(true).truncate(false).open(&lock_path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Err(StoreError::Invalid(format!(
            "another Newskite instance is already using {}; close it first or use a different data directory",
            dir.display()
        ))),
        Err(std::fs::TryLockError::Error(e)) if e.kind() == std::io::ErrorKind::Unsupported => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Make sure the database file exists with mode `0600` before SQLite opens it (SQLite gives
/// `-wal`/`-shm` the main file's mode), and tighten files an older version left readable
/// by others. No-op outside Unix.
fn make_db_files_private(db: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match private_file_options().write(true).create_new(true).open(db) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let mut wal = db.as_os_str().to_owned();
        wal.push("-wal");
        let mut shm = db.as_os_str().to_owned();
        shm.push("-shm");
        for p in [db.to_path_buf(), PathBuf::from(wal), PathBuf::from(shm)] {
            let Ok(meta) = std::fs::metadata(&p) else { continue };
            if meta.permissions().mode() & 0o077 != 0 {
                // Best effort: a file owned by someone else cannot be changed, and that
                // must not stop the store from opening.
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = db;
    Ok(())
}

impl Store {
    // -----------------------------------------------------------------------------------
    // Opening
    // -----------------------------------------------------------------------------------

    /// Open (creating if needed) `dir/newskite.sqlite3`. Creates `dir` and its parents,
    /// enables WAL, foreign keys and a busy timeout, and runs pending migrations.
    ///
    /// Single instance: the store takes an exclusive advisory lock on
    /// `dir/`[`LOCK_FILE_NAME`] for as long as it lives. If another store (in this or
    /// another process) already holds it, this returns [`StoreError::Invalid`] with a
    /// message saying another Newskite instance is using the directory.
    ///
    /// Privacy (Unix): directories this call creates get mode `0700`, and the database
    /// file, its `-wal`/`-shm` companions and the lock file are kept at `0600`, because the
    /// database holds server and archive passwords in plain text. An existing data
    /// directory keeps its mode.
    pub fn open(dir: &Path) -> Result<Self, StoreError> {
        create_private_dir(dir)?;
        let lock = lock_data_dir(dir)?;
        let path = dir.join(DB_FILE_NAME);
        make_db_files_private(&path)?;
        let conn = Connection::open(&path)?;
        Self::init(conn, Some(path), lock)
    }

    /// Open a private in-memory database (tests, `--headless` dry runs).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn, None, None)
    }

    fn init(mut conn: Connection, path: Option<PathBuf>, lock: Option<File>) -> Result<Self, StoreError> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        // journal_mode returns the resulting mode as a row; in-memory databases report
        // "memory", which is fine.
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        schema::migrate(&mut conn)?;
        Ok(Self { conn: Mutex::new(conn), path, _lock: lock })
    }

    /// Path of the database file, or `None` for an in-memory store.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Lock the connection. A poisoned mutex only means another thread panicked while
    /// holding it; SQLite keeps the connection consistent (open transactions are rolled
    /// back on drop), so the guard is recovered instead of propagating the panic.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // -----------------------------------------------------------------------------------
    // Jobs
    // -----------------------------------------------------------------------------------

    /// Persist a new job in state `queued` with `bytes_done = 0` and `created_at = now`.
    pub fn insert_job(&self, j: &NewJob) -> Result<Job, StoreError> {
        self.insert_job_at(j, now_unix())
    }

    /// Like [`Store::insert_job`] with an explicit creation time (unix seconds); used when
    /// importing history and by tests of queue ordering.
    pub fn insert_job_at(&self, j: &NewJob, created_at: i64) -> Result<Job, StoreError> {
        let conn = self.conn();
        // Position 0 while the queue was never reordered (created_at order applies);
        // after a move every pending job has a position and a new one goes last.
        conn.execute(
            "INSERT INTO jobs (name, state, bytes_total, bytes_done, dest_dir, source_path, created_at, error, password, position)
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6, NULL, ?7,
                     (SELECT CASE WHEN MAX(position) > 0 THEN MAX(position) + 1 ELSE 0 END FROM jobs))",
            params![
                j.name,
                JobState::Queued.as_str(),
                to_db(j.bytes_total),
                path_to_db(&j.dest_dir),
                path_to_db(&j.source_path),
                created_at,
                j.password,
            ],
        )?;
        let id = conn.last_insert_rowid();
        Self::get_job_locked(&conn, id)?.ok_or(StoreError::NotFound)
    }

    fn get_job_locked(conn: &Connection, id: i64) -> Result<Option<Job>, StoreError> {
        Ok(conn
            .query_row(&format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"), [id], job_from_row)
            .optional()?)
    }

    /// Fetch one job by id.
    pub fn get_job(&self, id: i64) -> Result<Option<Job>, StoreError> {
        Self::get_job_locked(&self.conn(), id)
    }

    /// All jobs, id ascending (queue and history).
    pub fn list_jobs(&self) -> Result<Vec<Job>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT {JOB_COLUMNS} FROM jobs ORDER BY id ASC"))?;
        let rows = stmt.query_map([], job_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The most recently inserted job (highest id) whose source NZB path equals `p`, if any.
    /// Comparison is on the stored UTF-8 form of the path, without canonicalization.
    pub fn job_by_source(&self, p: &Path) -> Result<Option<Job>, StoreError> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE source_path = ?1 ORDER BY id DESC LIMIT 1"),
                [path_to_db(p)],
                job_from_row,
            )
            .optional()?)
    }

    /// The oldest pending job (`queued`, `downloading` or `paused`) whose source NZB path
    /// equals `p`, if any. Unlike [`Store::job_by_source`] this looks at every job for the
    /// path, not only the newest one, so it is the right check for "is this NZB already in
    /// the queue". Comparison is on the stored UTF-8 form of the path.
    pub fn pending_job_by_source(&self, p: &Path) -> Result<Option<Job>, StoreError> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {JOB_COLUMNS} FROM jobs
                     WHERE source_path = ?1 AND state IN ('queued', 'downloading', 'paused')
                     ORDER BY id ASC LIMIT 1"
                ),
                [path_to_db(p)],
                job_from_row,
            )
            .optional()?)
    }

    /// Set a job's state and its error text (`None` clears the error).
    pub fn set_job_state(&self, id: i64, s: JobState, error: Option<&str>) -> Result<(), StoreError> {
        let n = self.conn().execute(
            "UPDATE jobs SET state = ?2, error = ?3 WHERE id = ?1",
            params![id, s.as_str(), error],
        )?;
        require_changed(n)
    }

    /// Record decoded bytes written so far for a job.
    pub fn set_job_progress(&self, id: i64, bytes_done: u64) -> Result<(), StoreError> {
        let n = self
            .conn()
            .execute("UPDATE jobs SET bytes_done = ?2 WHERE id = ?1", params![id, to_db(bytes_done)])?;
        require_changed(n)
    }

    /// Records when a job first started downloading (later calls keep the first time) and
    /// how many articles its NZB has.
    pub fn mark_job_started(&self, id: i64, ts: i64, articles_total: u64) -> Result<(), StoreError> {
        let n = self.conn().execute(
            "UPDATE jobs SET started_at = COALESCE(started_at, ?2), articles_total = ?3 WHERE id = ?1",
            params![id, ts, to_db(articles_total)],
        )?;
        require_changed(n)
    }

    /// Adds download time to a job.
    pub fn add_job_download_ms(&self, id: i64, ms: u64) -> Result<(), StoreError> {
        let n = self
            .conn()
            .execute("UPDATE jobs SET download_ms = download_ms + ?2 WHERE id = ?1", params![id, to_db(ms)])?;
        require_changed(n)
    }

    /// Records how a job finished: finish time, post-processing time, missing articles and
    /// the outcome (stored as JSON).
    pub fn set_job_outcome(
        &self,
        id: i64,
        finished_at: i64,
        post_ms: u64,
        articles_missing: u64,
        outcome: &newskite_types::JobOutcome,
    ) -> Result<(), StoreError> {
        let json = serde_json::to_string(outcome)?;
        let n = self.conn().execute(
            "UPDATE jobs SET finished_at = ?2, post_ms = ?3, articles_missing = ?4, outcome = ?5 WHERE id = ?1",
            params![id, finished_at, to_db(post_ms), to_db(articles_missing), json],
        )?;
        require_changed(n)
    }

    /// Forgets the last finish (a retried job is pending again). Timings of earlier runs
    /// and the log are kept.
    pub fn clear_job_outcome(&self, id: i64) -> Result<(), StoreError> {
        let n = self.conn().execute(
            "UPDATE jobs SET finished_at = NULL, post_ms = 0, articles_missing = 0, outcome = NULL WHERE id = ?1",
            [id],
        )?;
        require_changed(n)
    }

    /// Appends a line to a job's log, keeping the newest [`JOB_LOG_KEEP`] lines.
    pub fn append_job_log(&self, job_id: i64, ts: i64, level: &str, text: &str) -> Result<(), StoreError> {
        let conn = self.conn();
        let n = conn.execute(
            "INSERT INTO job_log (job_id, ts, level, text) SELECT ?1, ?2, ?3, ?4 WHERE EXISTS (SELECT 1 FROM jobs WHERE id = ?1)",
            params![job_id, ts, level, text],
        )?;
        require_changed(n)?;
        let id = conn.last_insert_rowid();
        // Trim now and then rather than on every line.
        if id % 50 == 0 {
            conn.execute("DELETE FROM job_log WHERE job_id = ?1 AND id <= ?2", params![job_id, id - JOB_LOG_KEEP])?;
        }
        Ok(())
    }

    /// A job's log, oldest first.
    pub fn job_log(&self, job_id: i64) -> Result<Vec<JobLogLine>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT ts, level, text FROM (SELECT id, ts, level, text FROM job_log WHERE job_id = ?1 ORDER BY id DESC LIMIT ?2) ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![job_id, JOB_LOG_KEEP], |r| {
            Ok(JobLogLine { ts: r.get(0)?, level: r.get(1)?, text: r.get(2)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Adds to the lifetime article counters.
    pub fn add_article_counts(&self, fetched: u64, missing: u64) -> Result<(), StoreError> {
        let conn = self.conn();
        for (key, n) in [(ARTICLES_FETCHED_KEY, fetched), (ARTICLES_MISSING_KEY, missing)] {
            if n > 0 {
                conn.execute(
                    "INSERT INTO counters (key, value) VALUES (?1, ?2)
                     ON CONFLICT (key) DO UPDATE SET value = value + excluded.value",
                    params![key, to_db(n)],
                )?;
            }
        }
        Ok(())
    }

    /// Set or clear (`None`) the archive password of a job.
    pub fn set_job_password(&self, id: i64, pw: Option<&str>) -> Result<(), StoreError> {
        let n = self.conn().execute("UPDATE jobs SET password = ?2 WHERE id = ?1", params![id, pw])?;
        require_changed(n)
    }

    /// Delete a job and all of its `job_files` rows.
    pub fn remove_job(&self, id: i64) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM job_files WHERE job_id = ?1", [id])?;
        tx.execute("DELETE FROM job_log WHERE job_id = ?1", [id])?;
        tx.execute("DELETE FROM article_maps WHERE job_id = ?1", [id])?;
        let n = tx.execute("DELETE FROM jobs WHERE id = ?1", [id])?;
        if n == 0 {
            // Dropping the transaction rolls back (nothing was deleted anyway).
            return Err(StoreError::NotFound);
        }
        tx.commit()?;
        Ok(())
    }

    /// The next job a queue run should process: state `queued` (a `paused` job is held by
    /// the user and skipped), in queue order: `position`, then oldest `created_at`, ties
    /// broken by id.
    pub fn next_runnable_job(&self) -> Result<Option<Job>, StoreError> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {JOB_COLUMNS} FROM jobs WHERE state = 'queued'
                     ORDER BY {QUEUE_ORDER} LIMIT 1"
                ),
                [],
                job_from_row,
            )
            .optional()?)
    }

    /// Like [`Store::next_runnable_job`], restricted to the jobs whose id is in `ids`
    /// (same queue order). Ids that do not exist or are not `queued` are ignored; an
    /// empty `ids` yields `None`.
    pub fn next_runnable_job_among(&self, ids: &[i64]) -> Result<Option<Job>, StoreError> {
        if ids.is_empty() {
            return Ok(None);
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        let conn = self.conn();
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {JOB_COLUMNS} FROM jobs WHERE state = 'queued'
                     AND id IN ({placeholders})
                     ORDER BY {QUEUE_ORDER} LIMIT 1"
                ),
                rusqlite::params_from_iter(ids.iter()),
                job_from_row,
            )
            .optional()?)
    }

    /// Ids of the pending jobs (`queued`, `downloading`, `paused`) in queue order.
    pub fn queue_order(&self) -> Result<Vec<i64>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT id FROM jobs WHERE state IN ('queued', 'downloading', 'paused') ORDER BY {QUEUE_ORDER}"
        ))?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Gives the jobs in `ids` the queue positions 1, 2, 3… in that order (one
    /// transaction). Ids that do not exist are ignored.
    pub fn set_queue_order(&self, ids: &[i64]) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for (i, id) in ids.iter().enumerate() {
            tx.execute("UPDATE jobs SET position = ?2 WHERE id = ?1", params![id, i as i64 + 1])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Startup recovery: every job left `downloading` by a previous process becomes
    /// `queued` again. Returns how many jobs were reset.
    ///
    /// Safe to call right after [`Store::open`]: the single-instance lock guarantees no
    /// other live store owns those jobs, so they really were interrupted.
    pub fn reset_interrupted(&self) -> Result<usize, StoreError> {
        Ok(self
            .conn()
            .execute("UPDATE jobs SET state = 'queued' WHERE state = 'downloading'", [])?)
    }

    /// Insert or replace the per-file row `(f.job_id, f.name)`.
    /// Returns [`StoreError::NotFound`] if the job does not exist.
    pub fn upsert_job_file(&self, f: &JobFile) -> Result<(), StoreError> {
        let conn = self.conn();
        if Self::get_job_locked(&conn, f.job_id)?.is_none() {
            return Err(StoreError::NotFound);
        }
        conn.execute(
            "INSERT INTO job_files (job_id, name, bytes_total, bytes_done, state) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (job_id, name) DO UPDATE SET
                 bytes_total = excluded.bytes_total,
                 bytes_done  = excluded.bytes_done,
                 state       = excluded.state",
            params![f.job_id, f.name, to_db(f.bytes_total), to_db(f.bytes_done), f.state],
        )?;
        Ok(())
    }

    /// All file rows of a job, by name. Empty for unknown jobs.
    pub fn job_files(&self, job_id: i64) -> Result<Vec<JobFile>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT job_id, name, bytes_total, bytes_done, state FROM job_files WHERE job_id = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map([job_id], job_file_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Saves a job's article map (encoded text, see the service), replacing an earlier
    /// one. [`StoreError::NotFound`] for unknown jobs.
    pub fn save_article_map(&self, job_id: i64, map: &str) -> Result<(), StoreError> {
        let conn = self.conn();
        if Self::get_job_locked(&conn, job_id)?.is_none() {
            return Err(StoreError::NotFound);
        }
        conn.execute(
            "INSERT INTO article_maps (job_id, map) VALUES (?1, ?2)
             ON CONFLICT (job_id) DO UPDATE SET map = excluded.map",
            params![job_id, map],
        )?;
        Ok(())
    }

    /// The saved article map of a job, if one was saved.
    pub fn article_map(&self, job_id: i64) -> Result<Option<String>, StoreError> {
        let conn = self.conn();
        Ok(conn
            .query_row("SELECT map FROM article_maps WHERE job_id = ?1", [job_id], |r| r.get(0))
            .optional()?)
    }

    // -----------------------------------------------------------------------------------
    // Bookmarks
    // -----------------------------------------------------------------------------------

    /// All bookmarks in tree (depth-first pre-) order: each parent precedes its children,
    /// siblings sorted by `order` then id.
    pub fn list_bookmarks(&self) -> Result<Vec<Bookmark>, StoreError> {
        let all: Vec<Bookmark> = {
            let conn = self.conn();
            let mut stmt =
                conn.prepare(&format!("SELECT {BOOKMARK_COLUMNS} FROM bookmarks ORDER BY ord ASC, id ASC"))?;
            let rows = stmt.query_map([], bookmark_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let ids: HashSet<i64> = all.iter().map(|b| b.id).collect();
        let mut children: HashMap<Option<i64>, Vec<usize>> = HashMap::new();
        for (idx, b) in all.iter().enumerate() {
            // Foreign keys prevent dangling parents; treat one as top level defensively.
            let key = b.parent_id.filter(|p| ids.contains(p));
            children.entry(key).or_default().push(idx);
        }

        let mut out = Vec::with_capacity(all.len());
        let mut visited = HashSet::with_capacity(all.len());
        // Iterative DFS; push children in reverse so the first sibling pops first.
        let mut stack: Vec<usize> = children.get(&None).map(|v| v.iter().rev().copied().collect()).unwrap_or_default();
        while let Some(idx) = stack.pop() {
            let b = &all[idx];
            if !visited.insert(b.id) {
                continue;
            }
            out.push(b.clone());
            if let Some(kids) = children.get(&Some(b.id)) {
                stack.extend(kids.iter().rev().copied());
            }
        }
        // Rows unreachable from the roots could only exist through a parent cycle, which
        // update_bookmark refuses; append them rather than silently hiding data.
        if out.len() < all.len() {
            for b in &all {
                if !visited.contains(&b.id) {
                    out.push(b.clone());
                }
            }
        }
        Ok(out)
    }

    /// Add a bookmark after its existing siblings, expanded.
    /// Returns [`StoreError::Invalid`] if `parent_id` names a missing bookmark.
    pub fn add_bookmark(&self, b: &NewBookmark) -> Result<Bookmark, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if let Some(parent) = b.parent_id {
            if !bookmark_exists(&tx, parent)? {
                return Err(StoreError::Invalid(format!("parent bookmark {parent} does not exist")));
            }
        }
        let order: i64 = tx.query_row(
            "SELECT COALESCE(MAX(ord) + 1, 0) FROM bookmarks WHERE parent_id IS ?1",
            [b.parent_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO bookmarks (parent_id, name, newsgroup, color, expanded, ord) VALUES (?1, ?2, ?3, ?4, 1, ?5)",
            params![b.parent_id, b.name, b.newsgroup, b.color, order],
        )?;
        let id = tx.last_insert_rowid();
        let created = tx.query_row(
            &format!("SELECT {BOOKMARK_COLUMNS} FROM bookmarks WHERE id = ?1"),
            [id],
            bookmark_from_row,
        )?;
        tx.commit()?;
        Ok(created)
    }

    /// Overwrite every field of an existing bookmark (matched by `b.id`).
    /// Returns [`StoreError::NotFound`] for an unknown id and [`StoreError::Invalid`] if the
    /// new parent is missing, the bookmark itself, or one of its descendants.
    pub fn update_bookmark(&self, b: &Bookmark) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if !bookmark_exists(&tx, b.id)? {
            return Err(StoreError::NotFound);
        }
        if let Some(parent) = b.parent_id {
            // Walk up from the new parent; reaching b.id would create a cycle.
            let mut cursor = Some(parent);
            let mut seen = HashSet::new();
            while let Some(cur) = cursor {
                if cur == b.id {
                    return Err(StoreError::Invalid(format!(
                        "bookmark {} cannot be moved under itself or its descendant {parent}",
                        b.id
                    )));
                }
                if !seen.insert(cur) {
                    break; // pre-existing cycle; stop walking
                }
                let next: Option<Option<i64>> = tx
                    .query_row("SELECT parent_id FROM bookmarks WHERE id = ?1", [cur], |r| r.get(0))
                    .optional()?;
                match next {
                    Some(p) => cursor = p,
                    None if cur == parent => {
                        return Err(StoreError::Invalid(format!("parent bookmark {parent} does not exist")));
                    }
                    None => break,
                }
            }
        }
        tx.execute(
            "UPDATE bookmarks SET parent_id = ?2, name = ?3, newsgroup = ?4, color = ?5, expanded = ?6, ord = ?7
             WHERE id = ?1",
            params![b.id, b.parent_id, b.name, b.newsgroup, b.color, b.expanded, b.order],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Delete a bookmark and, recursively, all of its descendants.
    pub fn remove_bookmark(&self, id: i64) -> Result<(), StoreError> {
        // ON DELETE CASCADE on parent_id (foreign keys are enabled) removes the subtree.
        let n = self.conn().execute("DELETE FROM bookmarks WHERE id = ?1", [id])?;
        require_changed(n)
    }

    // -----------------------------------------------------------------------------------
    // Throughput
    // -----------------------------------------------------------------------------------

    /// Record bytes for one second. If a sample for `s.ts` already exists, the byte counts
    /// are added to it (saturating at `i64::MAX`).
    pub fn add_sample(&self, s: &ThroughputSample) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO throughput (ts, bytes_up, bytes_down) VALUES (?1, ?2, ?3)
             ON CONFLICT (ts) DO UPDATE SET
                 bytes_up = CASE WHEN bytes_up > 9223372036854775807 - excluded.bytes_up
                                 THEN 9223372036854775807 ELSE bytes_up + excluded.bytes_up END,
                 bytes_down = CASE WHEN bytes_down > 9223372036854775807 - excluded.bytes_down
                                   THEN 9223372036854775807 ELSE bytes_down + excluded.bytes_down END",
            params![s.ts, to_db(s.bytes_up), to_db(s.bytes_down)],
        )?;
        Ok(())
    }

    /// Samples with `ts >= ts`, oldest first.
    pub fn samples_since(&self, ts: i64) -> Result<Vec<ThroughputSample>, StoreError> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT ts, bytes_up, bytes_down FROM throughput WHERE ts >= ?1 ORDER BY ts ASC")?;
        let rows = stmt.query_map([ts], |r| {
            Ok(ThroughputSample { ts: r.get(0)?, bytes_up: from_db(r.get(1)?), bytes_down: from_db(r.get(2)?) })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Delete samples with `ts < ts`; returns how many were removed.
    pub fn prune_samples_before(&self, ts: i64) -> Result<usize, StoreError> {
        Ok(self.conn().execute("DELETE FROM throughput WHERE ts < ?1", [ts])?)
    }

    /// Add `n` to the lifetime downloaded-bytes counter (saturating at `i64::MAX`).
    pub fn add_downloaded_bytes(&self, n: u64) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO counters (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET
                 value = CASE WHEN value > 9223372036854775807 - excluded.value
                              THEN 9223372036854775807 ELSE value + excluded.value END",
            params![DOWNLOADED_BYTES_KEY, to_db(n)],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------------------

    /// Load the settings document. Returns `Settings::default().normalized()` when none has
    /// been saved. Missing fields take their defaults and unknown fields are ignored, so
    /// documents written by older or newer builds still load; the result is always
    /// `normalized()`. A document that is not valid JSON (or not an object) yields
    /// [`StoreError::Json`] rather than silently discarding the user's server list.
    pub fn load_settings(&self) -> Result<Settings, StoreError> {
        let raw: Option<String> = self
            .conn()
            .query_row("SELECT value FROM settings WHERE key = ?1", [SETTINGS_KEY], |r| r.get(0))
            .optional()?;
        match raw {
            None => Ok(Settings::default().normalized()),
            Some(json) => Ok(serde_json::from_str::<Settings>(&json)?.normalized()),
        }
    }

    /// Persist the settings document (as JSON, including server passwords in plain text).
    ///
    /// If the document it replaces cannot be loaded (see [`Store::load_settings`]), that
    /// document is first kept, verbatim, under a `settings.unreadable-<unix secs>` key (once
    /// per distinct text), so saving the defaults shown after a load failure never destroys
    /// the user's server list. See [`Store::settings_backups`].
    pub fn save_settings(&self, s: &Settings) -> Result<(), StoreError> {
        let json = serde_json::to_string(s)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let current: Option<String> = tx
            .query_row("SELECT value FROM settings WHERE key = ?1", [SETTINGS_KEY], |r| r.get(0))
            .optional()?;
        if let Some(raw) = current.filter(|raw| serde_json::from_str::<Settings>(raw).is_err()) {
            let kept: bool = tx
                .query_row(
                    "SELECT 1 FROM settings WHERE substr(key, 1, ?1) = ?2 AND value = ?3",
                    params![SETTINGS_BACKUP_PREFIX.len() as i64, SETTINGS_BACKUP_PREFIX, raw],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !kept {
                let base = format!("{SETTINGS_BACKUP_PREFIX}{}", now_unix());
                let mut key = base.clone();
                let mut n = 2u32;
                while tx
                    .query_row("SELECT 1 FROM settings WHERE key = ?1", [&key], |_| Ok(()))
                    .optional()?
                    .is_some()
                {
                    key = format!("{base}-{n}");
                    n += 1;
                }
                tx.execute("INSERT INTO settings (key, value) VALUES (?1, ?2)", params![key, raw])?;
            }
        }
        tx.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![SETTINGS_KEY, json],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Unreadable settings documents kept by [`Store::save_settings`], as `(key, raw text)`
    /// pairs, oldest first. The text may contain passwords; never log it.
    pub fn settings_backups(&self) -> Result<Vec<(String, String)>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT key, value FROM settings WHERE substr(key, 1, ?1) = ?2 ORDER BY length(key), key",
        )?;
        let rows = stmt.query_map(params![SETTINGS_BACKUP_PREFIX.len() as i64, SETTINGS_BACKUP_PREFIX], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // -----------------------------------------------------------------------------------
    // Seen sources (watch folder)
    // -----------------------------------------------------------------------------------

    /// Record that the NZB file at `p` with this fingerprint (`size` in bytes, `mtime` in
    /// unix nanoseconds) was imported or its job removed, replacing an older fingerprint for
    /// the same path. These rows are kept when jobs are removed, so the watch folder does
    /// not import the same file again.
    pub fn mark_source_seen(&self, p: &Path, size: u64, mtime: i64) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO seen_sources (source_path, size, mtime, seen_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (source_path) DO UPDATE SET
                 size = excluded.size, mtime = excluded.mtime, seen_at = excluded.seen_at",
            params![path_to_db(p), to_db(size), mtime, now_unix()],
        )?;
        Ok(())
    }

    /// `true` when [`Store::mark_source_seen`] recorded `p` with exactly this fingerprint.
    pub fn source_seen(&self, p: &Path, size: u64, mtime: i64) -> Result<bool, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM seen_sources WHERE source_path = ?1 AND size = ?2 AND mtime = ?3",
                params![path_to_db(p), to_db(size), mtime],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    // -----------------------------------------------------------------------------------
    // Totals
    // -----------------------------------------------------------------------------------

    /// Lifetime downloaded bytes plus job counts by state.
    pub fn totals(&self) -> Result<Totals, StoreError> {
        let conn = self.conn();
        let (done, pending, failed): (i64, i64, i64) = conn.query_row(
            "SELECT COALESCE(SUM(state = 'done'), 0),
                    COALESCE(SUM(state IN ('queued', 'downloading', 'paused')), 0),
                    COALESCE(SUM(state = 'failed'), 0)
             FROM jobs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let counter = |key: &str| -> Result<u64, StoreError> {
            Ok(conn
                .query_row("SELECT value FROM counters WHERE key = ?1", [key], |r| r.get::<_, i64>(0))
                .optional()?
                .map(from_db)
                .unwrap_or(0))
        };
        Ok(Totals {
            downloaded_bytes: counter(DOWNLOADED_BYTES_KEY)?,
            articles_fetched: counter(ARTICLES_FETCHED_KEY)?,
            articles_missing: counter(ARTICLES_MISSING_KEY)?,
            jobs_done: from_db(done),
            jobs_pending: from_db(pending),
            jobs_failed: from_db(failed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_helpers() {
        assert_eq!(to_db(u64::MAX), i64::MAX);
        assert_eq!(to_db(42), 42);
        assert_eq!(from_db(-5), 0);
        assert_eq!(from_db(7), 7);
    }

    #[test]
    fn now_is_after_2020() {
        assert!(now_unix() > 1_577_836_800);
    }
}
