//! What happened to a job: persisted with the job so history and the inspector can show
//! it after a restart.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Broad cause of a failed job, so the UI can offer the matching fix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureKind {
    /// Articles no server had (and PAR2 could not make up for them).
    MissingArticles,
    /// The server refused the login.
    Auth,
    /// Connections kept failing.
    Connection,
    /// Writing to disk failed.
    Disk,
    /// PAR2 found damage it could not repair.
    Repair,
    /// An archive needs a password that was not given or is wrong.
    Password,
    /// An archive could not be extracted.
    Extract,
    /// The NZB could not be read.
    Nzb,
    Other,
}

impl FailureKind {
    /// Short label ("missing articles").
    pub fn label(self) -> &'static str {
        match self {
            FailureKind::MissingArticles => "missing articles",
            FailureKind::Auth => "login refused",
            FailureKind::Connection => "connection",
            FailureKind::Disk => "disk error",
            FailureKind::Repair => "repair failed",
            FailureKind::Password => "password",
            FailureKind::Extract => "extract failed",
            FailureKind::Nzb => "bad NZB",
            FailureKind::Other => "error",
        }
    }

    /// What the user can do about it.
    pub fn hint(self) -> &'static str {
        match self {
            FailureKind::MissingArticles => {
                "The servers no longer have some articles. Add a backup server, or find another NZB with more recovery data."
            }
            FailureKind::Auth => "Check the user name and password in Settings › Servers, then retry.",
            FailureKind::Connection => "Check the network and the server address, then retry.",
            FailureKind::Disk => "Check free space and permissions of the download folder, then retry.",
            FailureKind::Repair => "There is not enough recovery data. Another NZB of the same release may have more.",
            FailureKind::Password => "Set the archive password, then retry.",
            FailureKind::Extract => "The archive could not be extracted. Its files are still in the download folder.",
            FailureKind::Nzb => "The NZB file could not be read. Remove the job and add the NZB again.",
            FailureKind::Other => "See the job log for details.",
        }
    }
}

/// Result of one post-processing step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepState {
    Ok,
    Repaired,
    Skipped,
    Failed,
}

/// One post-processing step as it finished.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRecord {
    pub state: StepState,
    /// The verdict text ("skipped (no PAR2 files)", "repair complete", ...).
    pub detail: String,
    /// Wall-clock milliseconds.
    pub ms: u64,
}

/// Everything recorded about a finished (or failed) job.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobOutcome {
    pub failure: Option<FailureKind>,
    pub verify: Option<StepRecord>,
    pub repair: Option<StepRecord>,
    pub extract: Option<StepRecord>,
    pub cleanup: Option<StepRecord>,
    /// One-line PAR2 summary ("3 files: 1 damaged; rebuilt 8 slices ...").
    pub par2_summary: Option<String>,
    /// PAR2 slices rebuilt.
    pub slices_rebuilt: u64,
    /// Missing articles that PAR2 made up for.
    pub articles_repaired: u64,
    /// Folder archives were extracted into.
    pub extracted_to: Option<PathBuf>,
    /// Files removed by cleanup.
    pub files_removed: u64,
}

impl JobOutcome {
    /// PAR2 rebuilt data for this job.
    pub fn repaired(&self) -> bool {
        self.repair.as_ref().is_some_and(|r| r.state == StepState::Repaired) || self.slices_rebuilt > 0
    }
}

/// Health of one news server, kept for the life of the process.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHealth {
    /// `host:port`.
    pub address: String,
    /// Articles delivered.
    pub articles: u64,
    /// Decoded bytes delivered.
    pub bytes: u64,
    /// "No such article" answers.
    pub missing: u64,
    /// Transient errors (each one was retried or failed over).
    pub errors: u64,
    /// Connections open right now.
    pub connections: u32,
    /// Refused for the current job (login failed); reason attached.
    pub unusable: Option<String>,
    /// The most recent error, with its unix time.
    pub last_error: Option<(i64, String)>,
}
