use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::RepairOutlook;

/// Progress events emitted by the download engine (`engine://event` in the outline).
/// `SegmentDone` is high-frequency; consumers must never turn it into one log line per
/// segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineEvent {
    /// Sent once, before any other file event: every file of the job in NZB order. A
    /// file's position in this list is the `file` of the article events below.
    Layout { files: Vec<FileLayout> },
    FileStarted { name: String, segments: usize },
    /// `done`/`total` count segments of this file; `bytes` is decoded bytes of this
    /// segment; `file` and `index` place it in the [`EngineEvent::Layout`] (the file's
    /// position, and the segment's position within the file, in segment-number order).
    SegmentDone { name: String, done: usize, total: usize, bytes: u64, file: usize, index: usize },
    /// No server could supply this article (positions as in `SegmentDone`); its file
    /// will end with `FileFailed`.
    SegmentMissing { file: usize, index: usize },
    /// File `file` turned out to be complete on disk from an earlier run (found by its
    /// content once the download started): every article of it counts as done.
    FileComplete { file: usize },
    FileDone { name: String, path: PathBuf, bytes: u64 },
    FileFailed { name: String, missing: Vec<String> },
    /// The job's repair outlook changed (sent whenever a figure of it changes).
    Repair(RepairOutlook),
    /// Human-readable engine line (job started, server fail-over, ...).
    Log(String),
    /// A problem that did not stop the download (retries, missing articles, CRC errors).
    Warn(String),
}

/// One file of a job as the engine downloads it (see [`EngineEvent::Layout`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileLayout {
    /// The file's name from the NZB.
    pub name: String,
    /// Articles (segments) of the file.
    pub segments: usize,
    /// Found complete from an earlier run; it is not downloaded again.
    pub complete: bool,
}
