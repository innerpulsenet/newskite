//! Plain data types persisted by [`crate::Store`].

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

/// Lifecycle state of a download job.
///
/// Stored in the database as the exact lowercase strings returned by [`JobState::as_str`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobState {
    /// Waiting to be picked up by a queue run.
    Queued,
    /// Currently being downloaded (reset to `Queued` at startup by
    /// [`crate::Store::reset_interrupted`]).
    Downloading,
    /// Completed successfully (history).
    Done,
    /// Failed; `Job::error` carries the reason (history, can be retried).
    Failed,
    /// Held by the user: kept in the queue but skipped by queue runs until started again.
    Paused,
}

impl JobState {
    /// All states, in declaration order.
    pub const ALL: [JobState; 5] = [
        JobState::Queued,
        JobState::Downloading,
        JobState::Done,
        JobState::Failed,
        JobState::Paused,
    ];

    /// The canonical database/display string: `"queued"`, `"downloading"`, `"done"`,
    /// `"failed"` or `"paused"`.
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Downloading => "downloading",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Paused => "paused",
        }
    }

    /// `true` for states that still belong to the queue (`queued`, `downloading`, `paused`).
    pub fn is_pending(self) -> bool {
        matches!(self, JobState::Queued | JobState::Downloading | JobState::Paused)
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing an unknown job state string.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown job state: {0:?}")]
pub struct ParseJobStateError(pub String);

impl FromStr for JobState {
    type Err = ParseJobStateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        JobState::ALL
            .into_iter()
            .find(|st| st.as_str() == s)
            .ok_or_else(|| ParseJobStateError(s.to_owned()))
    }
}

/// A persisted download job.
///
/// `password` is the optional archive password for post-processing. It is stored in plain
/// text in the local database (see the README "Credential storage" section) and is redacted
/// from this type's `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct Job {
    /// Database row id (monotonic, never reused while the row exists).
    pub id: i64,
    /// Display name (usually derived from the NZB).
    pub name: String,
    /// Current state.
    pub state: JobState,
    /// Sum of all segment sizes in the NZB.
    pub bytes_total: u64,
    /// Decoded bytes written so far.
    pub bytes_done: u64,
    /// Output directory.
    pub dest_dir: PathBuf,
    /// The NZB file the job was created from.
    pub source_path: PathBuf,
    /// Creation time, unix seconds.
    pub created_at: i64,
    /// Failure reason for `Failed` jobs (or whatever was last passed to `set_job_state`).
    pub error: Option<String>,
    /// Archive password used by post-processing.
    pub password: Option<String>,
    /// Timings, article counts and outcome.
    pub stats: JobStats,
}

/// What is known about a job's run(s).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobStats {
    /// When its first download began, unix seconds.
    pub started_at: Option<i64>,
    /// When it last reached `done` or `failed`, unix seconds.
    pub finished_at: Option<i64>,
    /// Time spent downloading, summed over runs.
    pub download_ms: u64,
    /// Time spent post-processing in the last run.
    pub post_ms: u64,
    /// Articles in the NZB.
    pub articles_total: u64,
    /// Articles no server had in the last run.
    pub articles_missing: u64,
    /// What happened when it last finished (boxed: most jobs in a list are pending).
    pub outcome: Option<Box<newskite_types::JobOutcome>>,
}

impl JobStats {
    /// Average download speed in bytes/s over the time spent downloading, when known.
    pub fn avg_speed(&self, bytes: u64) -> Option<u64> {
        (self.download_ms >= 1000 && bytes > 0).then(|| bytes.saturating_mul(1000) / self.download_ms)
    }
}

/// One line of a job's own log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobLogLine {
    /// Unix seconds.
    pub ts: i64,
    /// `"info"`, `"warn"` or `"error"`.
    pub level: String,
    pub text: String,
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("state", &self.state)
            .field("bytes_total", &self.bytes_total)
            .field("bytes_done", &self.bytes_done)
            .field("dest_dir", &self.dest_dir)
            .field("source_path", &self.source_path)
            .field("created_at", &self.created_at)
            .field("error", &self.error)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("stats", &self.stats)
            .finish()
    }
}

/// Input for [`crate::Store::insert_job`]. New jobs start `Queued` with `bytes_done = 0`.
#[derive(Clone, PartialEq, Eq)]
pub struct NewJob {
    /// Display name.
    pub name: String,
    /// Total bytes expected.
    pub bytes_total: u64,
    /// Output directory.
    pub dest_dir: PathBuf,
    /// The NZB file the job comes from.
    pub source_path: PathBuf,
    /// Optional archive password (stored in plain text).
    pub password: Option<String>,
}

impl fmt::Debug for NewJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewJob")
            .field("name", &self.name)
            .field("bytes_total", &self.bytes_total)
            .field("dest_dir", &self.dest_dir)
            .field("source_path", &self.source_path)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Per-file progress row of a job. `name` is unique within a job.
///
/// `state` is free-form; the service uses `"done"` to mark files that can be skipped
/// on resume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobFile {
    /// Owning job id.
    pub job_id: i64,
    /// Output file name (sanitized by the engine).
    pub name: String,
    /// Expected size.
    pub bytes_total: u64,
    /// Bytes written so far.
    pub bytes_done: u64,
    /// Free-form state string (e.g. `"downloading"`, `"done"`, `"failed"`).
    pub state: String,
}

/// A newsgroup shortcut in the sidebar; may be nested via `parent_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    /// Row id.
    pub id: i64,
    /// Parent bookmark (folder) or `None` for top level.
    pub parent_id: Option<i64>,
    /// Display name.
    pub name: String,
    /// Optional newsgroup, e.g. `alt.binaries.example`.
    pub newsgroup: Option<String>,
    /// Optional color tag (free-form string, typically `#rrggbb`).
    pub color: Option<String>,
    /// Whether the node is expanded in the tree.
    pub expanded: bool,
    /// Sort key among siblings (ascending; ties broken by id).
    pub order: i64,
}

/// Input for [`crate::Store::add_bookmark`]. The new bookmark is expanded and placed
/// after its existing siblings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewBookmark {
    /// Parent bookmark or `None`.
    pub parent_id: Option<i64>,
    /// Display name.
    pub name: String,
    /// Optional newsgroup.
    pub newsgroup: Option<String>,
    /// Optional color tag.
    pub color: Option<String>,
}

/// Bytes moved during one wall-clock second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThroughputSample {
    /// Unix seconds identifying the sample's second.
    pub ts: i64,
    /// Bytes uploaded during that second.
    pub bytes_up: u64,
    /// Bytes downloaded during that second.
    pub bytes_down: u64,
}

/// Aggregate counters for the account summary pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// Lifetime downloaded bytes (see [`crate::Store::add_downloaded_bytes`]).
    pub downloaded_bytes: u64,
    /// Jobs in state `done`.
    pub jobs_done: u64,
    /// Jobs in `queued`, `downloading` or `paused`.
    pub jobs_pending: u64,
    /// Jobs in state `failed`.
    pub jobs_failed: u64,
    /// Lifetime articles fetched and missing.
    pub articles_fetched: u64,
    pub articles_missing: u64,
}
