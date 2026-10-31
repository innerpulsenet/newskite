//! Post-processing for finished downloads (outline F5): PAR2 verify/repair, archive
//! extraction and cleanup, implemented by driving external tools (`par2`, `7z`/`7zz`/`7za`,
//! `unrar`, `unzip`). Missing tools degrade gracefully: the step is reported as
//! [`Verdict::Skipped`] with a reason.
//!
//! The API is blocking; callers on an async runtime should use `spawn_blocking`.
//!
//! # Behavior
//!
//! 1. **PAR2.** `.par2` files are grouped into sets (`name.par2` + `name.volNN+MM.par2`).
//!    For each set the main file (no `.volNN+MM`, else the smallest) is verified with
//!    `par2 verify`. Damaged/missing files are repaired with `par2 repair` when
//!    [`PostOptions::repair`] is set; unmatched files in the folder are passed as extra
//!    files so obfuscated (renamed) copies are recognized and renamed back. Exit codes
//!    (0 ok, 1 repair possible, 2 repair not possible) are cross-checked with the output.
//!    With repair disabled, a damaged set is a failure.
//! 2. **Extraction** (when [`PostOptions::extract`] is set and PAR2 did not fail).
//!    Archive sets are detected by name (`.rar`, `.partNN.rar`, `.rNN`, `.7z`,
//!    `.7z.NNN`, `.zip`, `.zNN`, `.zip.NNN`) and magic bytes (files with no or a
//!    random-looking extension are sniffed; the magic of each set's first volume also
//!    decides the tool when it disagrees with the extension). Only the first volume is
//!    handed to the tool. unrar is preferred for RAR, 7-Zip for everything else, unzip
//!    for single-volume zip when 7-Zip is missing.
//!
//!    Every set is extracted into **`dir/<sanitized job_name>/`** (see
//!    [`extraction_dir_name`]); if a *file* of that name already exists,
//!    `dir/<name>.extracted/` is used instead. Before extracting, the archive is listed
//!    (`7z l -slt`, `unrar lb`, `unzip -Z1`) and refused if any entry is absolute or has
//!    a `..` component; after extracting, symbolic links pointing outside the folder are
//!    deleted and the set is reported as failed.
//!
//!    **Passwords** are passed on the tool's command line (`-p<pw>` for 7-Zip/unrar,
//!    `-P <pw>` for unzip), which other local users can see in the process list while the
//!    tool runs. Passwords are never logged (commands are logged with `-p***`). Without a
//!    password the tools are made non-interactive (`-p-` for unrar, a placeholder
//!    password for 7-Zip/unzip), and stdin is always the null device, so a tool can never
//!    hang on a prompt.
//! 3. **Cleanup** (when [`PostOptions::cleanup`] is set) runs only when the extraction
//!    verdict is [`Verdict::Ok`]: volumes of every extracted set are deleted and, when no
//!    set was skipped, also all `.par2` files and the `name.1` backups par2 left after a
//!    repair. Deleted paths are returned in [`PostReport::removed`].

mod detect;
mod extract;
mod par2;
mod tools;

use std::fmt;
use std::path::{Path, PathBuf};

pub use detect::{sniff_bytes, sniff_file, ArchiveKind, Magic};
pub use extract::{is_unsafe_entry, parse_7z_listing, parse_bare_listing};
pub use tools::{detect_tools, detect_tools_in, find_tool, Tools};

use newskite_types::PostProcessSettings;

/// What to do after a download completes.
#[derive(Clone, PartialEq, Eq)]
pub struct PostOptions {
    /// Repair damaged/missing files with PAR2 (verification always runs when PAR2 files
    /// and the `par2` tool are present).
    pub repair: bool,
    /// Extract archives.
    pub extract: bool,
    /// Delete archives and parity files after a successful extraction.
    pub cleanup: bool,
    /// Archive password for this job (never logged).
    pub password: Option<String>,
}

impl Default for PostOptions {
    fn default() -> Self {
        Self { repair: true, extract: true, cleanup: false, password: None }
    }
}

impl PostOptions {
    /// Options from the persisted settings plus a per-job password. An empty password
    /// is treated as none.
    pub fn from_settings(s: &PostProcessSettings, password: Option<String>) -> Self {
        Self {
            repair: s.repair,
            extract: s.extract,
            cleanup: s.cleanup,
            password: password.filter(|p| !p.is_empty()),
        }
    }
}

impl fmt::Debug for PostOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostOptions")
            .field("repair", &self.repair)
            .field("extract", &self.extract)
            .field("cleanup", &self.cleanup)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .finish()
    }
}

/// Result of one post-processing step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The step did not run (nothing to do, disabled or tool missing); reason attached.
    Skipped(String),
    /// The step ran and everything is fine.
    Ok,
    /// PAR2 repaired damaged or missing files.
    Repaired,
    /// The step failed; short human reason attached.
    Failed(String),
}

impl Verdict {
    /// True for [`Verdict::Failed`].
    pub fn is_failed(&self) -> bool {
        matches!(self, Verdict::Failed(_))
    }

    /// True for [`Verdict::Ok`] and [`Verdict::Repaired`].
    pub fn is_success(&self) -> bool {
        matches!(self, Verdict::Ok | Verdict::Repaired)
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Skipped(r) => write!(f, "skipped ({r})"),
            Verdict::Ok => f.write_str("ok"),
            Verdict::Repaired => f.write_str("repaired"),
            Verdict::Failed(r) => write!(f, "failed ({r})"),
        }
    }
}

/// Outcome of [`process`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostReport {
    pub par2: Verdict,
    pub extract: Verdict,
    /// Folder the archives were extracted into (when at least one set was extracted).
    pub extracted_to: Option<PathBuf>,
    /// Files deleted by cleanup.
    pub removed: Vec<PathBuf>,
}

/// Errors that prevent post-processing from running at all. Tool failures are reported
/// as [`Verdict::Failed`] instead.
#[derive(Debug, thiserror::Error)]
pub enum PostError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("tool error: {0}")]
    Tool(String),
}

/// Folder name used for extraction: `job_name` with path separators, control characters
/// and characters invalid on Windows (`<>:"/\|?*`) replaced by `_`, leading/trailing
/// spaces and dots trimmed, Windows reserved device names prefixed with `_`, and capped
/// at 200 bytes. Never empty, `.` or `..` (falls back to `extracted`).
pub fn extraction_dir_name(job_name: &str) -> String {
    let mapped: String = job_name
        .chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let mut s = mapped.trim_matches(|c: char| c == ' ' || c == '.').to_string();
    if s.len() > 200 {
        let mut cut = 200;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s = s.trim_end_matches([' ', '.']).to_string();
    }
    if s.is_empty() {
        return "extracted".into();
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        s.insert(0, '_');
    }
    s
}

/// Regular files directly in `dir` (no recursion, links not followed), excluding the
/// engine's in-progress `.nkpart` files; sorted.
fn list_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut v = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("nkpart")) {
            continue;
        }
        v.push(path);
    }
    v.sort();
    Ok(v)
}

/// Run PAR2 verify/repair, extraction and cleanup for the download in `dir`.
/// See the crate documentation for the exact behavior. `log` receives human-readable
/// progress lines (never passwords).
pub fn process(
    dir: &Path,
    job_name: &str,
    opts: &PostOptions,
    tools: &Tools,
    log: &mut dyn FnMut(String),
) -> Result<PostReport, PostError> {
    let dir = std::path::absolute(dir)?;
    if !std::fs::metadata(&dir)?.is_dir() {
        return Err(PostError::Io(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("{} is not a folder", dir.display()),
        )));
    }

    // 1. PAR2
    let files = list_files(&dir)?;
    let (par2_verdict, backups) = par2::run(&dir, &files, tools.par2.as_deref(), opts.repair, log);
    log(format!("par2: {par2_verdict}"));

    let mut report = PostReport { par2: par2_verdict, extract: Verdict::Skipped(String::new()), extracted_to: None, removed: Vec::new() };

    // 2. Extraction
    if !opts.extract {
        report.extract = Verdict::Skipped("extraction disabled".into());
        return Ok(report);
    }
    if report.par2.is_failed() {
        report.extract = Verdict::Skipped("not attempted because PAR2 verification failed".into());
        log(format!("extract: {}", report.extract));
        return Ok(report);
    }
    // par2 may have renamed files back to their real names; look again.
    let files = list_files(&dir)?;
    let (sets, broken) = detect::find_archive_sets(&files);
    if sets.is_empty() && broken.is_empty() {
        report.extract = Verdict::Skipped("no archives found".into());
        log(format!("extract: {}", report.extract));
        return Ok(report);
    }

    let mut failures: Vec<String> = broken.iter().map(|b| format!("{}: first volume missing", b.name)).collect();
    for f in &failures {
        log(format!("extract: {f}"));
    }
    let mut skipped: Vec<String> = Vec::new();
    let mut extracted: Vec<&detect::ArchiveSet> = Vec::new();

    if !sets.is_empty() {
        let target = extraction_target(&dir, job_name);
        if let Err(e) = std::fs::create_dir_all(&target) {
            report.extract = Verdict::Failed(format!("could not create {}: {e}", target.display()));
            log(format!("extract: {}", report.extract));
            return Ok(report);
        }
        let password = opts.password.as_deref().filter(|p| !p.is_empty());
        for set in &sets {
            let name = set.first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match extract::extract_set(set, &target, tools, password, log) {
                extract::SetOutcome::Extracted => {
                    log(format!("extract: {name}: extracted to {}", target.display()));
                    extracted.push(set);
                }
                extract::SetOutcome::Skipped(r) => skipped.push(format!("{name}: {r}")),
                extract::SetOutcome::Failed(r) => {
                    log(format!("extract: {name}: {r}"));
                    failures.push(if sets.len() + broken.len() > 1 { format!("{name}: {r}") } else { r });
                }
            }
        }
        if !extracted.is_empty() {
            report.extracted_to = Some(target);
        } else {
            // Do not leave an empty folder behind.
            let _ = std::fs::remove_dir(&target);
        }
    }

    report.extract = if !failures.is_empty() {
        Verdict::Failed(failures.join("; "))
    } else if extracted.is_empty() {
        Verdict::Skipped(skipped.join("; "))
    } else {
        Verdict::Ok
    };
    log(format!("extract: {}", report.extract));

    // 3. Cleanup
    if opts.cleanup && report.extract == Verdict::Ok {
        let mut doomed: Vec<PathBuf> = extracted.iter().flat_map(|s| s.volumes.iter().cloned()).collect();
        if skipped.is_empty() {
            doomed.extend(files.iter().filter(|f| detect::is_par2(f)).cloned());
            doomed.extend(backups);
        }
        doomed.sort();
        doomed.dedup();
        for p in doomed {
            match std::fs::remove_file(&p) {
                Ok(()) => report.removed.push(p),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log(format!("cleanup: could not remove {}: {e}", p.display())),
            }
        }
        log(format!("cleanup: removed {} file(s)", report.removed.len()));
    }
    Ok(report)
}

/// `dir/<sanitized job_name>`, or `dir/<name>.extracted` when a non-folder of that name
/// is in the way.
fn extraction_target(dir: &Path, job_name: &str) -> PathBuf {
    let name = extraction_dir_name(job_name);
    let t = dir.join(&name);
    match std::fs::symlink_metadata(&t) {
        Ok(m) if !m.is_dir() => dir.join(format!("{name}.extracted")),
        _ => t,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_name_sanitized() {
        assert_eq!(extraction_dir_name("My.Release.2024"), "My.Release.2024");
        assert_eq!(extraction_dir_name("../../etc"), "_.._etc");
        assert_eq!(extraction_dir_name("a/b\\c:d*e?f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(extraction_dir_name(""), "extracted");
        assert_eq!(extraction_dir_name(".."), "extracted");
        assert_eq!(extraction_dir_name(" . "), "extracted");
        assert_eq!(extraction_dir_name("name.\n"), "name._");
        assert_eq!(extraction_dir_name("con"), "_con");
        assert_eq!(extraction_dir_name("LPT1.txt"), "_LPT1.txt");
        assert_eq!(extraction_dir_name("COMPUTER"), "COMPUTER");
        let long = "é".repeat(300);
        let s = extraction_dir_name(&long);
        assert!(s.len() <= 200 && !s.is_empty());
    }

    #[test]
    fn options_debug_hides_password() {
        let o = PostOptions { password: Some("hunter2".into()), ..Default::default() };
        assert!(!format!("{o:?}").contains("hunter2"));
        let o = PostOptions::from_settings(&PostProcessSettings::default(), Some(String::new()));
        assert_eq!(o.password, None);
        assert!(o.repair && o.extract && !o.cleanup);
    }

    #[test]
    fn missing_dir_is_io_error() {
        let r = process(Path::new("/definitely/not/here/nk"), "x", &PostOptions::default(), &Tools::none(), &mut |_| {});
        assert!(matches!(r, Err(PostError::Io(_))));
    }

    #[test]
    fn verdict_display() {
        assert_eq!(Verdict::Failed("x".into()).to_string(), "failed (x)");
        assert!(Verdict::Repaired.is_success());
        assert!(!Verdict::Skipped(String::new()).is_success());
    }
}
