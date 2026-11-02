//! Native PAR2 verification and repair (PAR 2.0: Reed-Solomon over GF(2^16), MD5 and
//! CRC32 slice checksums), so post-processing needs no external `par2` binary.
//!
//! * [`load`] parses every packet of the given files (names do not matter, so obfuscated
//!   PAR2 files work as-is) and builds the [`RecoverySet`]s found. Corrupt packets are
//!   skipped by their checksum; the parser resynchronizes on the packet magic.
//! * [`verify`] checks each protected file slice by slice (MD5 of every slice; a file is
//!   intact when all its slices are and its length is right), finds complete copies under
//!   other names (obfuscated posts) by size and MD5, and collects good slices at aligned
//!   offsets of other files. It reports a [`FileStatus`] per file.
//! * [`repair`] rebuilds the missing slices into temporary copies, checks every slice of
//!   each copy against its MD5, and only then keeps the damaged original as `name.1` and moves the copy into
//!   place; misnamed copies are renamed to their real names. Target names that are not
//!   plain relative paths are refused, and folders are never created through links.
//!
//! Slice checks, recovery checks and the repair arithmetic run on several threads; the
//! GF(2^16) multiply-accumulate uses SSSE3 or AVX2 when the CPU has them (detected at run
//! time, with a portable fallback), so repair speed is bounded by the disk rather than the
//! CPU on most machines.
//!
//! Progress is reported through [`Event`]s (per file, and bytes/work units per stage) so
//! callers can show it. Data shifted by inserted or deleted bytes (not slice aligned) is
//! not searched for; Usenet damage (missing articles) keeps file offsets.

mod create;
mod gf;
mod packet;
mod pio;
mod repair;
mod verify;

pub use create::create;
pub use packet::{has_magic, FileEntry, Id, RecoveryRef, RecoverySet, SliceHash};
pub use repair::{repair, RepairOptions, RepairOutcome};
pub use verify::{safe_target, verify, FileReport, FileStatus, SliceSource, Verification};

use std::path::PathBuf;

/// Parses the PAR2 packets in `paths`. Returns the recovery sets and how many files or
/// packets were unreadable or corrupt.
pub fn load(paths: &[PathBuf]) -> (Vec<RecoverySet>, usize) {
    packet::load(paths)
}

/// Phase of the work an [`Event`] belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Reading and checking the protected files.
    Verify,
    /// Checking the recovery slices picked for a repair.
    Prepare,
    /// Rebuilding missing slices.
    Repair,
}

/// Progress of [`verify`] and [`repair`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Started on item `index` of `count`.
    File { stage: Stage, index: usize, count: usize, name: String },
    /// Work done so far in a stage (bytes when verifying, slice operations when repairing).
    Bytes { stage: Stage, done: u64, total: u64 },
    /// Final state of one file after verification.
    FileResult { name: String, status: FileStatus },
    /// A human-readable remark.
    Note(String),
}

/// Why a repair could not be done.
#[derive(Debug, thiserror::Error)]
pub enum Par2Error {
    #[error("repair not possible: {needed} recovery slices needed, {available} available")]
    NotEnoughRecovery { needed: usize, available: usize },
    #[error("a protected file has an unsafe name")]
    UnsafeName,
    #[error("{0} did not match its checksum after repair")]
    VerifyAfterRepair(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("internal error: {0}")]
    Internal(String),
}
