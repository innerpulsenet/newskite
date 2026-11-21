//! PAR2 verify/repair, natively through [`newskite_par2`].

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::detect::{is_par2, par2_key, par2_set_id};
use crate::Verdict;
use newskite_types::format;
use std::time::Instant;

/// Give obfuscated PAR2 files (random names, no `.par2` extension) proper names, so they
/// are recognizable in the folder and removed with the other parity files on cleanup
/// (the native verifier itself finds PAR2 data by packet magic, whatever the name).
/// Files are found by packet magic and grouped by recovery-set id. A group joins an
/// already-named `.par2` file of the same set when there is one; otherwise its smallest
/// file becomes `newskite-<id>.par2` and the rest `.volNNN+01.par2` volumes. Never
/// overwrites an existing file. Returns the files renamed: (from, to).
pub(crate) fn name_obfuscated_par2(dir: &Path, files: &[PathBuf], log: &mut dyn FnMut(String)) -> Vec<(PathBuf, PathBuf)> {
    let mut named: BTreeMap<[u8; 16], String> = BTreeMap::new();
    let mut groups: BTreeMap<[u8; 16], Vec<(u64, PathBuf)>> = BTreeMap::new();
    for f in files {
        let Some(id) = par2_set_id(f) else { continue };
        if is_par2(f) {
            let key = f.file_name().and_then(|n| par2_key(&n.to_string_lossy())).map(|(k, _)| k);
            if let Some(key) = key {
                named.entry(id).or_insert(key);
            }
        } else {
            let size = std::fs::metadata(f).map(|m| m.len()).unwrap_or(u64::MAX);
            groups.entry(id).or_default().push((size, f.clone()));
        }
    }
    let mut renamed = Vec::new();
    for (id, mut members) in groups {
        members.sort();
        let (base, mut need_main) = match named.get(&id) {
            Some(key) => (key.clone(), false),
            None => (format!("newskite-{}", id[..4].iter().map(|b| format!("{b:02x}")).collect::<String>()), true),
        };
        let mut vol = 0u32;
        for (_, src) in members {
            let target = loop {
                let name = if need_main {
                    format!("{base}.par2")
                } else {
                    vol += 1;
                    format!("{base}.vol{vol:03}+01.par2")
                };
                let candidate = dir.join(&name);
                if !candidate.exists() {
                    break candidate;
                }
                if need_main {
                    // The main name is taken by an unrelated file: fall back to volumes only
                    // under a name that cannot clash.
                    need_main = false;
                }
            };
            match rename_no_clobber(&src, &target) {
                Ok(()) => {
                    log(format!(
                        "par2: {} is a PAR2 file; named it {}",
                        src.file_name().unwrap_or_default().to_string_lossy(),
                        target.file_name().unwrap_or_default().to_string_lossy()
                    ));
                    need_main = false;
                    renamed.push((src.clone(), target.clone()));
                }
                Err(e) => log(format!(
                    "par2: could not name PAR2 file {}: {e}",
                    src.file_name().unwrap_or_default().to_string_lossy()
                )),
            }
        }
    }
    renamed
}

/// Rename without replacing an existing file: hard-link then unlink (atomic no-clobber),
/// falling back to a checked rename on file systems without hard links.
fn rename_no_clobber(src: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(src, dst) {
        Ok(()) => std::fs::remove_file(src),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => {
            if dst.exists() {
                return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "target exists"));
            }
            std::fs::rename(src, dst)
        }
    }
}

/// Counts from a native PAR2 run, summed over its recovery sets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Par2Stats {
    /// Recovery sets checked.
    pub sets: usize,
    /// Files protected.
    pub files: usize,
    /// Files present and correct before any repair.
    pub intact: usize,
    /// Files present but damaged or truncated.
    pub damaged: usize,
    /// Files absent (including those found under another name).
    pub missing: usize,
    /// Files whose complete content was found under another name.
    pub misnamed: usize,
    /// Input slices that were missing.
    pub slices_missing: usize,
    /// Input slices in total.
    pub slices_total: usize,
    /// Recovery slices available.
    pub recovery_available: usize,
    /// Slices rebuilt by the repair.
    pub slices_rebuilt: usize,
    /// Complete copies found under other names and moved to their real names.
    pub files_renamed: usize,
    /// Bytes protected.
    pub data_bytes: u64,
    /// Wall-clock time spent verifying and repairing, in milliseconds.
    pub verify_ms: u64,
    pub repair_ms: u64,
}

impl Par2Stats {
    /// One line for logs and the UI, e.g. `"12 files: 1 damaged, 1 missing; rebuilt 8 of
    /// 8 missing slices from 12 recovery slices (0.4 s)"`.
    pub fn summary(&self) -> String {
        let mut problems = Vec::new();
        if self.damaged > 0 {
            problems.push(format!("{} damaged", self.damaged));
        }
        if self.missing > self.misnamed {
            problems.push(format!("{} missing", self.missing - self.misnamed));
        }
        if self.misnamed > 0 {
            problems.push(format!("{} misnamed", self.misnamed));
        }
        let files = format!("{} file{}", self.files, if self.files == 1 { "" } else { "s" });
        let head = if problems.is_empty() { format!("{files}: all correct") } else { format!("{files}: {}", problems.join(", ")) };
        let mut out = head;
        if self.slices_missing > 0 {
            out.push_str(&format!(
                "; {} of {} slices missing, {} recovery slices available",
                self.slices_missing, self.slices_total, self.recovery_available
            ));
        }
        if self.slices_rebuilt > 0 {
            out.push_str(&format!("; rebuilt {} slices", self.slices_rebuilt));
        }
        out.push_str(&format!(" (verify {:.1} s", self.verify_ms as f64 / 1000.0));
        if self.repair_ms > 0 {
            out.push_str(&format!(", repair {:.1} s", self.repair_ms as f64 / 1000.0));
        }
        out.push(')');
        out
    }
}

/// What [`run`] found and did.
#[derive(Default)]
pub(crate) struct Par2Outcome {
    pub verdict: Option<Verdict>,
    /// Damaged originals the repair kept as `name.1` (cleanup candidates).
    pub backups: Vec<PathBuf>,
    pub stats: Option<Par2Stats>,
    /// Partial (`.nkpart`) files that supplied slices.
    pub partials_used: Vec<PathBuf>,
    /// Protected files that are correct now.
    pub good_targets: Vec<PathBuf>,
    /// Complete copies moved to their real names: (from, to).
    pub renamed: Vec<(PathBuf, PathBuf)>,
}

/// Progress callback: stage, fraction done (when known), detail line.
pub(crate) type Progress<'a> = &'a mut dyn FnMut(crate::PostStage, Option<f32>, String);

/// Verify (and if allowed, repair) every PAR2 set in `dir`, natively. `files` are the
/// regular files of the folder; `partials` the engine's incomplete `.nkpart` files. A
/// partial PAR2 volume still holds intact packets (each recovery block is checked against
/// its MD5 before use), so it is read like a complete one; the other partials are extra
/// slice sources. When no recovery set parses, the step is skipped.
pub(crate) fn run(
    dir: &Path,
    files: &[PathBuf],
    partials: &[PathBuf],
    repair: bool,
    log: &mut dyn FnMut(String),
    progress: Progress,
) -> Par2Outcome {
    use newskite_par2::{Event, FileStatus, Stage};
    let (partial_par2, partial_data): (Vec<PathBuf>, Vec<PathBuf>) = partials.iter().cloned().partition(|p| crate::is_par2_partial(p));
    let (mut par2_paths, mut others): (Vec<PathBuf>, Vec<PathBuf>) = files.iter().cloned().partition(|f| newskite_par2::has_magic(f));
    par2_paths.extend(partial_par2);
    if par2_paths.is_empty() {
        return Par2Outcome { verdict: Some(Verdict::Skipped(crate::NO_PAR2.into())), ..Default::default() };
    }
    progress(crate::PostStage::Verify, None, "Reading PAR2 files".into());
    let (sets, bad) = newskite_par2::load(&par2_paths);
    if bad > 0 {
        log(format!("par2: skipped {bad} corrupt or unreadable PAR2 packet(s)/file(s)"));
    }
    if sets.is_empty() {
        // Unverifiable is not damaged: extraction still runs (and checks its own CRCs).
        log("par2: no usable recovery set (main packet missing or corrupt); skipping verification".into());
        return Par2Outcome { verdict: Some(Verdict::Skipped("PAR2 files unreadable".into())), ..Default::default() };
    }
    others.extend(partial_data);
    let partial_set: HashSet<&Path> = partials.iter().map(PathBuf::as_path).collect();
    let mut partials_used: HashSet<&Path> = HashSet::new();

    let mut out = Par2Outcome::default();
    let mut stats = Par2Stats { sets: sets.len(), ..Default::default() };
    let mut failures = Vec::new();
    let mut repaired_any = false;
    for set in &sets {
        let label = set
            .files
            .first()
            .map(|f| if set.files.len() > 1 { format!("{} (+{} more)", f.name, set.files.len() - 1) } else { f.name.clone() })
            .unwrap_or_else(|| "empty set".into());
        if !set.incomplete.is_empty() {
            log(format!(
                "par2: {label}: {} file(s) listed without a description; they cannot be checked",
                set.incomplete.len()
            ));
        }
        log(format!(
            "par2: {label}: {} file(s), {} slices of {}, {} recovery slice(s){}",
            set.files.len(),
            set.slice_count(),
            format::bytes(set.slice_size),
            set.recovery.len(),
            set.creator.as_deref().map(|c| format!(", made by {c}")).unwrap_or_default()
        ));
        let started = Instant::now();
        let mut last = -1.0f32;
        let mut current = String::new();
        let ver = newskite_par2::verify(set, dir, &others, &mut |e| match e {
            Event::File { index, count, name, .. } => {
                current = format!("Verifying {name} ({}/{count})", index + 1);
                progress(crate::PostStage::Verify, (last >= 0.0).then_some(last), current.clone());
            }
            Event::Bytes { done, total, .. } => {
                let f = if total == 0 { 1.0 } else { done as f32 / total as f32 };
                if f - last >= 0.005 || f >= 1.0 {
                    last = f;
                    progress(crate::PostStage::Verify, Some(f), current.clone());
                }
            }
            Event::FileResult { name, status } => match status {
                FileStatus::Intact => {}
                FileStatus::Misnamed(p) => log(format!(
                    "par2: {name}: found complete as {}",
                    p.file_name().unwrap_or_default().to_string_lossy()
                )),
                FileStatus::Damaged { found, total } => log(format!("par2: {name}: damaged ({found} of {total} slices good)")),
                FileStatus::Missing { found, total } if found > 0 => {
                    log(format!("par2: {name}: missing ({found} of {total} slices found in other files)"))
                }
                FileStatus::Missing { .. } => log(format!("par2: {name}: missing")),
                FileStatus::UnsafeName => log(format!("par2: {name}: unsafe file name; refusing to write it")),
            },
            Event::Note(n) => log(format!("par2: {n}")),
        });
        stats.verify_ms += started.elapsed().as_millis() as u64;
        stats.files += ver.files.len();
        stats.slices_total += ver.sources.len();
        stats.slices_missing += ver.missing_slices();
        stats.recovery_available += ver.recovery_available;
        stats.data_bytes += set.data_bytes();
        for f in &ver.files {
            match f.status {
                FileStatus::Intact => stats.intact += 1,
                FileStatus::Damaged { .. } => stats.damaged += 1,
                FileStatus::Missing { .. } | FileStatus::UnsafeName => stats.missing += 1,
                FileStatus::Misnamed(_) => {
                    stats.missing += 1;
                    stats.misnamed += 1;
                }
            }
        }
        for s in ver.sources.iter().flatten() {
            if let Some(&p) = partial_set.get(s.path.as_path()) {
                if partials_used.insert(p) {
                    out.partials_used.push(p.to_path_buf());
                }
            }
        }
        let multi = sets.len() > 1;
        let fail = |log: &mut dyn FnMut(String), failures: &mut Vec<String>, reason: String| {
            log(format!("par2: {label}: {reason}"));
            failures.push(if multi { format!("{label}: {reason}") } else { reason });
        };
        if ver.is_intact() {
            log(format!("par2: {label}: all files are correct"));
            out.good_targets.extend(ver.files.iter().filter_map(|f| f.target.clone()));
            continue;
        }
        let bad_files = ver.files.iter().filter(|f| f.status != FileStatus::Intact).count();
        if !repair {
            fail(log, &mut failures, format!("{bad_files} file(s) damaged or missing; repair is disabled"));
            continue;
        }
        if !ver.repairable() {
            let reason = if ver.files.iter().any(|f| f.status == FileStatus::UnsafeName) {
                "repair not possible (a file has an unsafe name)".to_string()
            } else {
                format!(
                    "repair not possible (need {} more recovery slice(s): {} missing, {} available)",
                    ver.blocks_short(),
                    ver.missing_slices(),
                    ver.recovery_available
                )
            };
            fail(log, &mut failures, reason);
            continue;
        }
        log(format!(
            "par2: {label}: repairing {} slice(s) with {} recovery slice(s) available",
            ver.missing_slices(),
            ver.recovery_available
        ));
        let started = Instant::now();
        let mut last = -1.0f32;
        let result = newskite_par2::repair(set, &ver, &newskite_par2::RepairOptions::default(), &mut |e| match e {
            Event::File { stage: Stage::Prepare, index, count, .. } => progress(
                crate::PostStage::Repair,
                Some(0.0),
                format!("Checking recovery data ({}/{count})", index + 1),
            ),
            Event::Bytes { done, total, .. } => {
                let f = if total == 0 { 1.0 } else { done as f32 / total as f32 };
                if f - last >= 0.005 || f >= 1.0 {
                    last = f;
                    progress(crate::PostStage::Repair, Some(f), format!("Rebuilding {} slice(s)", ver.missing_slices()));
                }
            }
            Event::Note(n) => log(format!("par2: {n}")),
            _ => {}
        });
        stats.repair_ms += started.elapsed().as_millis() as u64;
        match result {
            Ok(r) => {
                // Moving complete copies to their real names is not a repair.
                repaired_any |= !r.rebuilt.is_empty();
                stats.slices_rebuilt += r.slices_rebuilt;
                stats.files_renamed += r.renamed.len();
                out.renamed.extend(r.renamed.iter().cloned());
                out.backups.extend(r.backups);
                out.good_targets.extend(ver.files.iter().filter_map(|f| f.target.clone()));
                log(format!(
                    "par2: {label}: repair complete ({} rebuilt, {} renamed)",
                    r.rebuilt.len(),
                    r.renamed.len()
                ));
            }
            Err(e) => fail(log, &mut failures, e.to_string()),
        }
    }
    log(format!("par2: {}", stats.summary()));
    out.verdict = Some(if !failures.is_empty() {
        Verdict::Failed(failures.join("; "))
    } else if repaired_any {
        Verdict::Repaired
    } else {
        Verdict::Ok
    });
    out.stats = Some(stats);
    out
}
