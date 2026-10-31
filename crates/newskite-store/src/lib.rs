//! SQLite persistence for Newskite: the job queue and history, per-file progress,
//! bookmarks, throughput history, the settings document and lifetime counters.
//!
//! The database lives in a single file, [`DB_FILE_NAME`], inside the directory given to
//! [`Store::open`] (by default [`newskite_types::config::default_data_dir`]). It runs in
//! WAL mode with foreign keys enabled and a busy timeout, and its schema is versioned
//! with `PRAGMA user_version` (see [`SCHEMA_VERSION`]).
//!
//! Storage notes:
//! - Unsigned 64-bit counters are stored as SQLite `INTEGER` (signed 64-bit); values above
//!   `i64::MAX` are clamped on write and negative values read back as 0.
//! - Paths are stored as UTF-8 text using `to_string_lossy`, so a non-UTF-8 path (possible
//!   on Unix) is persisted with replacement characters. Such paths are rare for NZB
//!   sources and download folders and the lossy form is still shown correctly in the UI.
//! - Server passwords (inside the settings JSON) and per-job archive passwords are stored
//!   in plain text in this local database. They are never logged by this crate. On Unix
//!   the database files are kept at mode `0600` and a data directory created by
//!   [`Store::open`] gets mode `0700`.
//! - Only one [`Store`] may have a given data directory open at a time (an exclusive
//!   advisory lock on [`LOCK_FILE_NAME`]); a second open is refused.

mod model;
mod schema;
mod store;

pub use model::{
    Bookmark, Job, JobFile, JobState, NewBookmark, NewJob, ParseJobStateError, ThroughputSample, Totals,
};
pub use schema::SCHEMA_VERSION;
pub use store::{Store, DB_FILE_NAME, LOCK_FILE_NAME};

/// Errors returned by [`Store`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Underlying SQLite failure.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The settings document could not be (de)serialized.
    #[error("settings JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// Filesystem error (creating the data directory).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The referenced row does not exist.
    #[error("not found")]
    NotFound,
    /// Invalid input or unexpected stored data.
    #[error("invalid: {0}")]
    Invalid(String),
}
