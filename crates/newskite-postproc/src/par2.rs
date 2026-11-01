//! PAR2 verify/repair through `par2` (par2cmdline).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::detect::{find_par2_sets, is_par2, par2_key, par2_set_id, Par2Set};
use crate::tools::{ToolCommand, ToolRun};
use crate::Verdict;

/// Verify/repair can read the whole download several times; be generous.
const PAR2_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// Only the tail of par2's output is needed (summary lines).
const PAR2_CAPTURE: usize = 1024 * 1024;
/// Keep the command line well below the Windows 32 KiB limit.
const MAX_EXTRA_ARG_BYTES: usize = 24 * 1024;

/// Outcome of `par2 verify`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerifyState {
    /// All files are correct.
    Intact,
    /// Damaged or missing files, enough recovery data.
    Repairable,
    /// Damaged or missing files, not enough recovery data (reason attached).
    NotRepairable(String),
    /// par2 itself failed (bad arguments, I/O error, timeout ...).
    Error(String),
}

/// Map a par2 exit code to a short explanation.
fn par2_code_reason(code: Option<i32>, timed_out: bool) -> String {
    if timed_out {
        return "par2 timed out".into();
    }
    match code {
        Some(3) => "par2 rejected the command line".into(),
        Some(4) => "par2 found insufficient critical data to verify".into(),
        Some(5) => "par2 repair failed".into(),
        Some(6) => "par2 file I/O error".into(),
        Some(7) => "par2 internal error".into(),
        Some(8) => "par2 ran out of memory".into(),
        Some(c) => format!("par2 exited with code {c}"),
        None => "par2 was terminated".into(),
    }
}

/// "You need 101 more recovery blocks to be able to repair." -> that sentence.
fn need_more_blocks(out: &str) -> Option<String> {
    out.lines()
        .map(str::trim)
        .find(|l| l.starts_with("You need") && l.contains("recovery block"))
        .map(|l| l.trim_end_matches('.').to_string())
}

fn not_possible_reason(out: &str) -> String {
    match need_more_blocks(out) {
        Some(need) => format!("repair not possible ({})", need.to_lowercase()),
        None => "repair not possible (not enough recovery data)".into(),
    }
}

/// Classify a `par2 verify` run from its exit code, cross-checked with its output.
pub(crate) fn classify_verify(run: &ToolRun) -> VerifyState {
    let out = run.combined();
    if run.timed_out {
        return VerifyState::Error(par2_code_reason(None, true));
    }
    match run.code {
        Some(0) if out.contains("Repair is required") => VerifyState::Repairable,
        Some(0) => VerifyState::Intact,
        Some(1) => VerifyState::Repairable,
        Some(2) => VerifyState::NotRepairable(not_possible_reason(&out)),
        c => VerifyState::Error(par2_code_reason(c, false)),
    }
}

/// Classify a `par2 repair` run: `Ok(true)` = repaired, `Ok(false)` = nothing needed.
pub(crate) fn classify_repair(run: &ToolRun) -> Result<bool, String> {
    let out = run.combined();
    if run.timed_out {
        return Err(par2_code_reason(None, true));
    }
    match run.code {
        Some(0) => Ok(!out.contains("repair is not required")),
        Some(2) => Err(not_possible_reason(&out)),
        c => Err(par2_code_reason(c, false)),
    }
}

/// Target names par2 reported (`Target: "name" - found.`).
pub(crate) fn reported_targets(out: &str) -> BTreeSet<String> {
    out.lines()
        .filter_map(|l| {
            let rest = l.trim_start().strip_prefix("Target: \"")?;
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        })
        .collect()
}

/// Create the folders that targets in subfolders (e.g. `Sample/x-sample.mkv`) need:
/// par2cmdline does not create them, so renaming an obfuscated file into one fails with
/// an I/O error and aborts every rename after it. Only plain relative paths are honored,
/// and an existing component must be a real folder (never a symlink), so a hostile PAR2
/// file cannot make par2 write outside `dir`.
pub(crate) fn prepare_target_dirs(dir: &Path, targets: &BTreeSet<String>, log: &mut dyn FnMut(String)) {
    use std::path::Component;
    for target in targets {
        let Some(parent) = Path::new(target).parent().filter(|p| !p.as_os_str().is_empty()) else { continue };
        if !parent.components().all(|c| matches!(c, Component::Normal(_))) {
            log(format!("par2: ignoring unsafe target path {target:?}"));
            continue;
        }
        let mut current = dir.to_path_buf();
        for comp in parent.components() {
            current.push(comp);
            match std::fs::symlink_metadata(&current) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => {
                    log(format!("par2: {} is in the way of target {target:?}", current.display()));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if let Err(e) = std::fs::create_dir(&current) {
                        log(format!("par2: could not create {}: {e}", current.display()));
                        break;
                    }
                }
                Err(e) => {
                    log(format!("par2: could not inspect {}: {e}", current.display()));
                    break;
                }
            }
        }
    }
}

fn rel(name: &Path) -> PathBuf {
    Path::new(".").join(name)
}

fn file_names(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// par2 renames a damaged file `x` to `x.1` (`x.2`, ...) before writing the repaired copy.
fn new_backups(dir: &Path, before: &BTreeSet<String>, after: &BTreeSet<String>) -> Vec<PathBuf> {
    after
        .difference(before)
        .filter(|n| {
            n.rsplit_once('.').is_some_and(|(stem, num)| {
                !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) && after.contains(stem)
            })
        })
        .map(|n| dir.join(n))
        .collect()
}

/// Give obfuscated PAR2 files (random names, no `.par2` extension) proper names so
/// par2cmdline accepts them: it only loads a main file and volumes ending in `.par2`.
/// Files are found by packet magic and grouped by recovery-set id. A group joins an
/// already-named `.par2` file of the same set when there is one; otherwise its smallest
/// file becomes `newskite-<id>.par2` and the rest `.volNNN+01.par2` volumes. par2's
/// repair then matches the data files by content and renames them back. Never
/// overwrites an existing file. Returns the number of files renamed.
pub(crate) fn name_obfuscated_par2(dir: &Path, files: &[PathBuf], log: &mut dyn FnMut(String)) -> usize {
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
    let mut renamed = 0;
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
                    renamed += 1;
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

/// Verify (and if allowed, repair) every PAR2 set in `dir`.
/// Returns the verdict and the backup files par2 left behind (cleanup candidates).
pub(crate) fn run(
    dir: &Path,
    files: &[PathBuf],
    par2: Option<&Path>,
    repair: bool,
    log: &mut dyn FnMut(String),
) -> (Verdict, Vec<PathBuf>) {
    let sets = find_par2_sets(files);
    if sets.is_empty() {
        return (Verdict::Skipped("no PAR2 files".into()), Vec::new());
    }
    let Some(par2) = par2 else {
        log("par2: tool not found; skipping verification".into());
        return (Verdict::Skipped("par2 tool not found".into()), Vec::new());
    };
    let mut failures = Vec::new();
    let mut repaired_any = false;
    let mut backups = Vec::new();
    for set in &sets {
        match run_set(dir, files, par2, set, repair, log, &mut backups) {
            Ok(repaired) => repaired_any |= repaired,
            Err(reason) => {
                let name = set.main.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                log(format!("par2: {name}: {reason}"));
                failures.push(if sets.len() > 1 { format!("{name}: {reason}") } else { reason });
            }
        }
    }
    let verdict = if !failures.is_empty() {
        Verdict::Failed(failures.join("; "))
    } else if repaired_any {
        Verdict::Repaired
    } else {
        Verdict::Ok
    };
    (verdict, backups)
}

fn run_set(
    dir: &Path,
    files: &[PathBuf],
    par2: &Path,
    set: &Par2Set,
    repair: bool,
    log: &mut dyn FnMut(String),
    backups: &mut Vec<PathBuf>,
) -> Result<bool, String> {
    let Some(main_name) = set.main.file_name() else { return Err("invalid PAR2 file name".into()) };
    let main_rel = rel(Path::new(main_name));
    let verify = ToolCommand::new(par2).arg("v").arg("-q").arg(&main_rel).cwd(dir);
    log(format!("par2: {}", verify.display()));
    let vrun = verify.run(PAR2_TIMEOUT, PAR2_CAPTURE).map_err(|e| e.to_string())?;
    let state = classify_verify(&vrun);
    match &state {
        VerifyState::Intact => {
            log(format!("par2: {}: all files are correct", main_name.to_string_lossy()));
            return Ok(false);
        }
        VerifyState::Error(reason) => return Err(reason.clone()),
        VerifyState::Repairable | VerifyState::NotRepairable(_) => {}
    }
    if !repair {
        return Err(match state {
            VerifyState::NotRepairable(r) => r,
            _ => "files are damaged or missing; repair is disabled".into(),
        });
    }

    // Files that par2 did not recognize as targets may be renamed (obfuscated) copies of
    // missing targets; hand them to the repair so it can match them by content.
    let targets = reported_targets(&vrun.combined());
    prepare_target_dirs(dir, &targets, log);
    let set_files: BTreeSet<&PathBuf> = set.files.iter().collect();
    let mut extras = Vec::new();
    let mut budget = MAX_EXTRA_ARG_BYTES;
    for f in files {
        if set_files.contains(f) || is_par2(f) {
            continue;
        }
        let Some(name) = f.file_name() else { continue };
        if targets.contains(name.to_string_lossy().as_ref()) {
            continue;
        }
        let cost = name.len() + 3;
        if cost > budget {
            log("par2: too many extra files for one command line; passing a subset".into());
            break;
        }
        budget -= cost;
        extras.push(rel(Path::new(name)));
    }
    if let VerifyState::NotRepairable(reason) = &state {
        if extras.is_empty() {
            return Err(reason.clone());
        }
        log(format!("par2: {reason}; retrying with {} unmatched file(s)", extras.len()));
    }

    let before = file_names(dir);
    let mut cmd = ToolCommand::new(par2).arg("r").arg("-q").arg(&main_rel).cwd(dir);
    for e in &extras {
        cmd = cmd.arg(e);
    }
    log(format!("par2: {}", cmd.display()));
    let rrun = cmd.run(PAR2_TIMEOUT, PAR2_CAPTURE).map_err(|e| e.to_string());
    let after = file_names(dir);
    backups.extend(new_backups(dir, &before, &after));
    let repaired = classify_repair(&rrun?)?;
    if repaired {
        log(format!("par2: {}: repair complete", main_name.to_string_lossy()));
    }
    Ok(repaired)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(code: Option<i32>, out: &str) -> ToolRun {
        ToolRun { code, stdout: out.into(), ..Default::default() }
    }

    #[test]
    fn verify_classification() {
        assert_eq!(classify_verify(&run(Some(0), "All files are correct, repair is not required.")), VerifyState::Intact);
        assert_eq!(classify_verify(&run(Some(1), "Repair is required.\nRepair is possible.")), VerifyState::Repairable);
        assert_eq!(classify_verify(&run(Some(0), "Repair is required.")), VerifyState::Repairable);
        let s = classify_verify(&run(
            Some(2),
            "Repair is required.\nRepair is not possible.\nYou need 101 more recovery blocks to be able to repair.\n",
        ));
        assert_eq!(s, VerifyState::NotRepairable("repair not possible (you need 101 more recovery blocks to be able to repair)".into()));
        assert!(matches!(classify_verify(&run(Some(6), "")), VerifyState::Error(r) if r.contains("I/O")));
        let t = ToolRun { timed_out: true, ..Default::default() };
        assert!(matches!(classify_verify(&t), VerifyState::Error(r) if r.contains("timed out")));
    }

    #[test]
    fn repair_classification() {
        assert_eq!(classify_repair(&run(Some(0), "Repair complete.")), Ok(true));
        assert_eq!(classify_repair(&run(Some(0), "All files are correct, repair is not required.")), Ok(false));
        assert!(classify_repair(&run(Some(2), "Repair is not possible.")).is_err());
        assert!(classify_repair(&run(Some(5), "")).unwrap_err().contains("repair failed"));
    }

    #[test]
    fn target_dirs_created_only_for_safe_relative_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("job");
        std::fs::create_dir(&dir).unwrap();
        let targets: BTreeSet<String> =
            ["Sample/x.mkv", "a/b/c.bin", "plain.rar", "../escape/x", "/abs/x", "./dot/x"].iter().map(|s| s.to_string()).collect();
        let mut logs = Vec::new();
        prepare_target_dirs(&dir, &targets, &mut |l| logs.push(l));
        assert!(dir.join("Sample").is_dir());
        assert!(dir.join("a/b").is_dir());
        assert!(!tmp.path().join("escape").exists());
        assert!(!dir.join("dot").exists());
        assert_eq!(logs.iter().filter(|l| l.contains("unsafe")).count(), 3, "{logs:?}");
    }

    #[cfg(unix)]
    #[test]
    fn target_dirs_never_follow_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("job");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("Sample")).unwrap();
        let targets: BTreeSet<String> = ["Sample/deeper/x.mkv".to_string()].into_iter().collect();
        let mut logs = Vec::new();
        prepare_target_dirs(&dir, &targets, &mut |l| logs.push(l));
        assert!(!outside.join("deeper").exists());
        assert!(logs.iter().any(|l| l.contains("in the way")), "{logs:?}");
    }

    #[test]
    fn targets_parsed() {
        let out = "Loading \"x.par2\".\nTarget: \"f2.bin\" - missing.\nTarget: \"f1.bin\" - damaged. Found 1 of 2 data blocks.\nFile: \"obf\" - no data found.";
        let t = reported_targets(out);
        assert_eq!(t.into_iter().collect::<Vec<_>>(), vec!["f1.bin".to_string(), "f2.bin".to_string()]);
    }

    #[test]
    fn backups_detected() {
        let before: BTreeSet<String> = ["a.bin", "b.bin", "s.par2"].iter().map(|s| s.to_string()).collect();
        let after: BTreeSet<String> =
            ["a.bin", "a.bin.1", "b.bin", "s.par2", "new.7"].iter().map(|s| s.to_string()).collect();
        let b = new_backups(Path::new("/d"), &before, &after);
        assert_eq!(b, vec![Path::new("/d").join("a.bin.1")]);
    }
}
