//! What happened to a job: persisted with the job so history and the inspector can show
//! it after a restart.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Broad cause of a failed job, so the UI can offer the matching fix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureKind {
    /// Articles no server had (and PAR2 could not make up for them).
    MissingArticles,
    /// Stopped early: more data was missing than the job's PAR2 recovery data can rebuild.
    Unrepairable,
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
            FailureKind::Unrepairable => "can't be repaired",
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
            FailureKind::Unrepairable => {
                "More is missing than the recovery data can rebuild, so the download was stopped. Add a backup server on another provider, or find another NZB of this release."
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
    /// The last repair outlook of the download (see [`RepairOutlook`]).
    pub repair_outlook: Option<RepairOutlook>,
}

impl JobOutcome {
    /// PAR2 rebuilt data for this job.
    pub fn repaired(&self) -> bool {
        self.repair.as_ref().is_some_and(|r| r.state == StepState::Repaired) || self.slices_rebuilt > 0
    }
}

/// What the job's PAR2 recovery data can still make up for, measured while downloading.
///
/// Every damaged input slice (a missing or corrupt article lands in one or two) needs one
/// recovery block. `needed` counts the slices damaged so far, from the articles' offsets;
/// `available` is the most recovery blocks that can still arrive intact. While PAR2 files
/// are downloading or unidentified it is an upper bound, so `needed > available` means the
/// repair will fail whatever arrives later.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepairOutlook {
    /// A PAR2 recovery set was read; the block figures are 0 until then.
    pub known: bool,
    /// Every file is identified and none is PAR2: nothing can be repaired.
    pub no_par2: bool,
    /// Input slices the set protects.
    pub blocks: u64,
    /// Damaged slices so far.
    pub needed: u64,
    /// Recovery blocks posted with the job (estimated from sizes until every PAR2 file is read).
    pub posted: u64,
    /// Most recovery blocks that can still arrive intact.
    pub available: u64,
    /// Every PAR2 file was read and every file identified: `posted` and `available` are counts.
    pub exact: bool,
}

impl RepairOutlook {
    /// The damage already exceeds what the recovery data could ever rebuild.
    pub fn hopeless(&self) -> bool {
        self.known && self.needed > self.available
    }

    /// Fraction of the recovery data the damage uses up (above 1 when hopeless).
    pub fn used(&self) -> f32 {
        match self.available {
            0 if self.needed == 0 => 0.0,
            0 => f32::INFINITY,
            a => self.needed as f32 / a as f32,
        }
    }
}

/// Health of one news server, kept for the life of the process.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHealth {
    /// `host:port`.
    pub address: String,
    /// Articles delivered.
    pub articles: u64,
    /// Article bytes received (yEnc-encoded, as they came over the wire).
    pub bytes: u64,
    /// "No such article" answers.
    pub missing: u64,
    /// Transient errors (each one was retried or failed over).
    pub errors: u64,
    /// Refused for the current job (login failed); reason attached.
    pub unusable: Option<String>,
    /// The most recent error, with its unix time.
    pub last_error: Option<(i64, String)>,
}
