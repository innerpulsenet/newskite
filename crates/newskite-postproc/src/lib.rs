//! Post-processing for finished downloads (outline F5): PAR2 verify/repair, archive
//! extraction and cleanup, all in-process: PAR2 through [`newskite_par2`], zip, 7z and RAR
//! through native backends. No external program is ever started.
//!
//! The API is blocking; callers on an async runtime should use `spawn_blocking`.
//!
//! # Behavior
//!
//! 1. **PAR2** runs natively ([`newskite_par2`]): every file holding PAR2 packets (any
//!    name) is parsed, each recovery set is verified slice by slice, and damaged or
//!    missing files are rebuilt when [`PostOptions::repair`] is set. Complete copies under
//!    other names (obfuscated posts) are renamed back, good slices are collected from
//!    other files, and the engine's incomplete `.nkpart` files (articles missing) serve
//!    as slice sources too, so a download with missing articles can still be completed.
//!    After a successful PAR2 step, a `.nkpart` file is deleted when it supplied slices
//!    or its final name is now a correct protected file; the rest are reported in
//!    [`PostReport::incomplete`]. Per-file results are logged and summed in
//!    [`PostReport::par2_stats`]; progress goes to [`PostEvent::Stage`]. When no
//!    recovery set parses (main packet missing or corrupt), the step is
//!    [`Verdict::Skipped`] (`PAR2 files unreadable`) and extraction still runs, checking
//!    its own CRCs. With repair disabled, a damaged set is a failure.
//! 2. **Extraction** (when [`PostOptions::extract`] is set and PAR2 did not fail).
//!    Archive sets are detected by name (`.rar`, `.partNN.rar`, `.rNN`, `.7z`,
//!    `.7z.NNN`, `.zip`, `.zNN`, `.zip.NNN`) and magic bytes (files with no or a
//!    random-looking extension are sniffed; the magic of each set's first volume also
//!    decides the format when it disagrees with the extension).
//!
//!    **Backends.** zip uses the `zip` crate (deflate, deflate64, bzip2, LZMA, XZ, zstd,
//!    PPMd; AES and ZipCrypto passwords), 7z uses `sevenz-rust2` (LZMA/LZMA2, PPMd,
//!    bzip2, deflate, zstd, BCJ/BCJ2/delta filters; AES passwords, encrypted headers) and
//!    RAR (RAR 1.5 to RAR 7) uses the UnRAR library compiled into the program, in test
//!    mode, so it decompresses and verifies while every byte is written by our own code.
//!    Split `.7z.NNN` and `.zip.NNN` volumes are read as one chained stream (a gap in the
//!    numbering is a missing volume). Spanned zips (`.z01`, `.z02` ... `.zip`, as written
//!    by `zip -s` or WinZip) are read as one archive: the volumes in disk order followed
//!    by a copy of the central directory, rewritten in memory with absolute offsets. UnRAR
//!    follows `.partNN.rar` / `.rNN` volumes from the first one. An archive a backend
//!    cannot read (an unknown codec or RAR feature) fails with `unsupported archive:
//!    <what>`, like corrupt data, a missing volume or a password problem. Progress is
//!    logged every 5% for sets of 32 MiB or more.
//!
//!    Every set is extracted into **`dir/<sanitized job_name>/`** (see
//!    [`extraction_dir_name`]); if a *file* of that name already exists,
//!    `dir/<name>.extracted/` is used instead. Before anything is written, the archive is
//!    listed and refused if any entry is absolute or has a `..` component; an archive
//!    without entries counts as damaged. Each set is then extracted into a fresh private
//!    staging folder (`.newskite-staging-*`) inside the extraction folder. The backends
//!    sanitize every entry path again before writing, create folders one component at a
//!    time without following links, open files with `create_new` (a duplicate entry
//!    unlinks the earlier file first, so data never goes through a link or a hard link)
//!    and **never create links**: symbolic links, junctions and hard links in an archive
//!    are skipped and logged, and one pointing outside the extraction folder (absolute or
//!    climbing out with `..`) reports the set as failed. A file that fails to decode is
//!    removed. Whether extraction succeeds or fails, symbolic links in the staging folder
//!    that point outside it or use an absolute destination are deleted (defense in
//!    depth), the remaining output is moved into the extraction folder without following
//!    any link already there (an existing file or link of the same name is replaced; a
//!    file never replaces a real folder), the staging folder is removed and the whole
//!    extraction folder is checked again for links pointing outside it. Any deleted link
//!    reports the set as failed. Links are judged by their fully resolved destination
//!    when it exists, else lexically.
//!
//!    **Passwords.** A wrong password fails the set with "wrong password", a missing
//!    one with "password required ..."; nothing ever prompts or waits. The password
//!    never leaves the process: the backends get it in memory (UnRAR through its
//!    callback, as a wide string), and it is never logged. Formats without a password
//!    check value (RAR 4, ZipCrypto, 7z data) can only tell a wrong password from damage
//!    by the failed integrity check of encrypted data, which is reported as a wrong
//!    password.
//! 3. **Cleanup.** With [`PostOptions::cleanup`], the volumes of every extracted set are
//!    deleted when the extraction verdict is [`Verdict::Ok`]. With
//!    [`PostOptions::remove_leftovers`], all `.par2` files, the `name.1` backups a PAR2
//!    repair leaves and release sidecars (`.sfv`, `.nfo`, ...) are deleted when PAR2
//!    verified or repaired the files (or there were none) and no archive set failed or was
//!    skipped; this also applies to downloads without archives. Deleted paths are
//!    returned in [`PostReport::removed`].

mod detect;
mod extract;
mod native;
mod par2;

use std::fmt;
use std::path::{Path, PathBuf};

pub use detect::{sniff_bytes, sniff_file, ArchiveKind, Magic};
pub use extract::is_unsafe_entry;
pub use par2::Par2Stats;

use newskite_types::PostProcessSettings;

/// What to do after a download completes.
#[derive(Clone, PartialEq, Eq)]
pub struct PostOptions {
    /// Repair damaged/missing files with PAR2 (verification always runs when PAR2 files
    /// are present).
    pub repair: bool,
    /// Extract archives.
    pub extract: bool,
    /// Delete archive volumes after a successful extraction.
    pub cleanup: bool,
    /// Delete leftovers once the data is known good: `.par2` files, the `name.1` backups
    /// a PAR2 repair leaves, and release sidecars (`.sfv`, `.nfo`, ...).
    pub remove_leftovers: bool,
    /// Archive password for this job (never logged).
    pub password: Option<String>,
}

impl Default for PostOptions {
    fn default() -> Self {
        Self { repair: true, extract: true, cleanup: true, remove_leftovers: true, password: None }
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
            remove_leftovers: s.remove_leftovers,
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
            .field("remove_leftovers", &self.remove_leftovers)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .finish()
    }
}

/// Result of one post-processing step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The step did not run (nothing to do, disabled or PAR2 files unreadable); reason
    /// attached.
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
    /// Counts from the native PAR2 step (`None` when it did not run natively).
    pub par2_stats: Option<Par2Stats>,
    /// Incomplete `.nkpart` files that are superseded by repaired files and were deleted.
    pub partials_removed: Vec<PathBuf>,
    /// Incomplete `.nkpart` files that are still incomplete (not covered by PAR2).
    pub incomplete: Vec<PathBuf>,
}

/// Post-processing stage, for progress display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PostStage {
    Verify,
    Repair,
    Extract,
    Cleanup,
}

impl PostStage {
    pub fn label(self) -> &'static str {
        match self {
            PostStage::Verify => "Verifying",
            PostStage::Repair => "Repairing",
            PostStage::Extract => "Extracting",
            PostStage::Cleanup => "Cleaning up",
        }
    }
}

/// What [`process_with_events`] reports while it runs.
#[derive(Clone, Debug, PartialEq)]
pub enum PostEvent {
    /// A human-readable progress line (never a password).
    Log(String),
    /// The current stage, its fraction done when known, and a short detail.
    Stage { stage: PostStage, fraction: Option<f32>, detail: String },
}

/// Errors that prevent post-processing from running at all. Failures of a step are
/// reported as [`Verdict::Failed`] instead.
#[derive(Debug, thiserror::Error)]
pub enum PostError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
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

/// True when `dir` directly holds a file with PAR2 packets (any name).
pub fn has_par2(dir: &Path) -> bool {
    list_files(dir).map(|v| v.iter().any(|f| newskite_par2::has_magic(f))).unwrap_or(false)
}

/// Engine partial files (`*.nkpart`) directly in `dir`; sorted.
fn list_partials(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut v = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("nkpart")) {
            v.push(path);
        }
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
    log: &mut dyn FnMut(String),
) -> Result<PostReport, PostError> {
    process_with_events(dir, job_name, opts, &mut |e| {
        if let PostEvent::Log(l) = e {
            log(l)
        }
    })
}

/// [`process`] with stage progress as well as log lines.
pub fn process_with_events(
    dir: &Path,
    job_name: &str,
    opts: &PostOptions,
    on: &mut dyn FnMut(PostEvent),
) -> Result<PostReport, PostError> {
    let on = std::cell::RefCell::new(on);
    let mut log = |l: String| (on.borrow_mut())(PostEvent::Log(l));
    let mut progress =
        |stage: PostStage, fraction: Option<f32>, detail: String| (on.borrow_mut())(PostEvent::Stage { stage, fraction, detail });
    run_all(dir, job_name, opts, &mut log, &mut progress)
}

fn run_all(
    dir: &Path,
    job_name: &str,
    opts: &PostOptions,
    log: &mut dyn FnMut(String),
    progress: &mut dyn FnMut(PostStage, Option<f32>, String),
) -> Result<PostReport, PostError> {
    let dir = std::path::absolute(dir)?;
    if !std::fs::metadata(&dir)?.is_dir() {
        return Err(PostError::Io(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("{} is not a folder", dir.display()),
        )));
    }

    // 1. PAR2
    let mut files = list_files(&dir)?;
    if par2::name_obfuscated_par2(&dir, &files, log) > 0 {
        files = list_files(&dir)?;
    }
    let partials = list_partials(&dir)?;
    let outcome = par2::run(&dir, &files, &partials, opts.repair, log, progress);
    let par2_verdict = outcome.verdict.unwrap_or_else(|| Verdict::Skipped(NO_PAR2.into()));
    let backups = outcome.backups;
    log(format!("par2: {par2_verdict}"));

    let mut report = PostReport {
        par2: par2_verdict,
        extract: Verdict::Skipped(String::new()),
        extracted_to: None,
        removed: Vec::new(),
        par2_stats: outcome.stats,
        partials_removed: Vec::new(),
        incomplete: Vec::new(),
    };

    // Partial files that PAR2 made redundant go; the others are still incomplete.
    for p in partials {
        let final_path = p.with_extension("");
        let superseded = report.par2.is_success()
            && (outcome.partials_used.contains(&p) || outcome.good_targets.contains(&final_path));
        if superseded {
            match std::fs::remove_file(&p) {
                Ok(()) => report.partials_removed.push(p),
                Err(e) => {
                    log(format!("par2: could not remove {}: {e}", p.display()));
                    report.incomplete.push(p);
                }
            }
        } else {
            report.incomplete.push(p);
        }
    }
    if !report.partials_removed.is_empty() {
        log(format!("par2: {} incomplete download file(s) are complete now", report.partials_removed.len()));
    }
    if !report.incomplete.is_empty() {
        log(format!(
            "par2: {} file(s) are still incomplete: {}",
            report.incomplete.len(),
            report.incomplete.iter().map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect::<Vec<_>>().join(", ")
        ));
    }

    // 2. Extraction
    if opts.extract {
        progress(PostStage::Extract, None, "Extracting archives".into());
    }
    let extracted = extract_phase(&dir, job_name, opts, &mut report, log)?;

    // 3. Cleanup. `files` is listed after PAR2 renamed obfuscated files back.
    progress(PostStage::Cleanup, None, "Cleaning up".into());
    let files = list_files(&dir)?;
    let mut doomed: Vec<PathBuf> = Vec::new();
    if opts.cleanup && report.extract == Verdict::Ok {
        doomed.extend(extracted.volumes.iter().cloned());
    }
    let leftovers = if opts.remove_leftovers { leftovers_allowed(&report, &extracted) } else { Err("disabled".into()) };
    match leftovers {
        Ok(()) => {
            doomed.extend(files.iter().filter(|f| detect::is_par2(f) || is_sidecar(f)).cloned());
            doomed.extend(backups);
        }
        Err(why) if opts.remove_leftovers => log(format!("cleanup: keeping parity and sidecar files ({why})")),
        Err(_) => {}
    }
    if !doomed.is_empty() {
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
        let kept = list_files(&dir).map(|v| v.len()).unwrap_or(0);
        if kept > 0 {
            log(format!("cleanup: kept {kept} other file(s) that are not archives, parity or release sidecars"));
        }
    }
    Ok(report)
}

/// [`Verdict::Skipped`] reason when the download has no PAR2 files.
pub(crate) const NO_PAR2: &str = "no PAR2 files";

/// What extraction did, for cleanup.
#[derive(Default)]
struct Extracted {
    /// Volumes of every set that extracted.
    volumes: Vec<PathBuf>,
    /// Some archive set was skipped (its volumes and parity are still needed).
    skipped_any: bool,
}

/// Parity files, repair backups and sidecars may go once the data is known good: PAR2
/// verified or repaired it (or there was no PAR2 data to begin with) and no archive set
/// failed or was skipped. With PAR2 skipped for another reason (PAR2 files unreadable)
/// the files were never checked, so the parity data stays.
fn leftovers_allowed(report: &PostReport, extracted: &Extracted) -> Result<(), String> {
    match &report.par2 {
        Verdict::Ok | Verdict::Repaired => {}
        Verdict::Skipped(r) if r == NO_PAR2 => {}
        v => return Err(format!("par2 {v}")),
    }
    if report.extract.is_failed() {
        return Err(format!("extract {}", report.extract));
    }
    if extracted.skipped_any {
        return Err("an archive set was skipped".into());
    }
    Ok(())
}

/// Step 2 of [`process`]: sets `report.extract`/`extracted_to`.
fn extract_phase(
    dir: &Path,
    job_name: &str,
    opts: &PostOptions,
    report: &mut PostReport,
    log: &mut dyn FnMut(String),
) -> Result<Extracted, PostError> {
    let mut out = Extracted::default();
    if !opts.extract {
        report.extract = Verdict::Skipped("extraction disabled".into());
        return Ok(out);
    }
    if report.par2.is_failed() {
        report.extract = Verdict::Skipped("not attempted because PAR2 verification failed".into());
        log(format!("extract: {}", report.extract));
        return Ok(out);
    }
    // PAR2 may have renamed files back to their real names; look again.
    let files = list_files(dir)?;
    let (sets, broken) = detect::find_archive_sets(&files);
    if sets.is_empty() && broken.is_empty() {
        report.extract = Verdict::Skipped("no archives found".into());
        log(format!("extract: {}", report.extract));
        return Ok(out);
    }

    let mut failures: Vec<String> = broken.iter().map(|b| format!("{}: first volume missing", b.name)).collect();
    for f in &failures {
        log(format!("extract: {f}"));
    }
    let mut skipped: Vec<String> = Vec::new();
    let mut extracted: Vec<&detect::ArchiveSet> = Vec::new();

    if !sets.is_empty() {
        let target = extraction_target(dir, job_name);
        if let Err(e) = std::fs::create_dir_all(&target) {
            report.extract = Verdict::Failed(format!("could not create {}: {e}", target.display()));
            log(format!("extract: {}", report.extract));
            return Ok(out);
        }
        let password = opts.password.as_deref().filter(|p| !p.is_empty());
        for set in &sets {
            let name = set.first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match extract::extract_set(set, &target, password, log) {
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
    out.volumes = extracted.iter().flat_map(|s| s.volumes.iter().cloned()).collect();
    out.skipped_any = !skipped.is_empty();
    Ok(out)
}

/// Release sidecar files (checksums, info, scene metadata) that have no use once the
/// archives are extracted. Anything else is kept: some releases carry payload files
/// (a sample, subtitles) unarchived next to the archives.
const SIDECAR_EXTENSIONS: &[&str] = &["sfv", "nfo", "srr", "srs", "md5", "sha1", "sha256", "url"];

fn is_sidecar(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|e| SIDECAR_EXTENSIONS.contains(&e.as_str()))
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
        assert!(o.repair && o.extract && o.cleanup);
    }

    #[test]
    fn sidecars_recognized() {
        for n in ["geth.nfo", "x.SFV", "a.srr", "b.sha256", "c.url"] {
            assert!(is_sidecar(Path::new(n)), "{n}");
        }
        for n in ["movie.mkv", "sample.mkv", "subs.srt", "notes", "nfo"] {
            assert!(!is_sidecar(Path::new(n)), "{n}");
        }
    }

    #[test]
    fn missing_dir_is_io_error() {
        let r = process(Path::new("/definitely/not/here/nk"), "x", &PostOptions::default(), &mut |_| {});
        assert!(matches!(r, Err(PostError::Io(_))));
    }

    #[test]
    fn verdict_display() {
        assert_eq!(Verdict::Failed("x".into()).to_string(), "failed (x)");
        assert!(Verdict::Repaired.is_success());
        assert!(!Verdict::Skipped(String::new()).is_success());
    }
}
