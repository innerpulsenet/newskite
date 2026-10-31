use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Progress events emitted by the download engine (`engine://event` in the outline).
/// `SegmentDone` is high-frequency; consumers must never turn it into one log line per
/// segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineEvent {
    FileStarted { name: String, segments: usize },
    /// `done`/`total` count segments of this file; `bytes` is decoded bytes of this segment.
    SegmentDone { name: String, done: usize, total: usize, bytes: u64 },
    FileDone { name: String, path: PathBuf, bytes: u64 },
    FileFailed { name: String, missing: Vec<String> },
    /// Human-readable engine line (job started, server fail-over, ...).
    Log(String),
}
