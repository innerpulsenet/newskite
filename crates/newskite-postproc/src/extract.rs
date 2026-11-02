//! Archive extraction: the native backends (see [`crate::native`]) first, with 7-Zip /
//! unrar / unzip as a fallback for archives a backend reports as unsupported; shared
//! safety checks (entry paths, staging, link removal, merging into the target).

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::detect::{ArchiveKind, ArchiveSet};
use crate::native::{self, NativeError};
use crate::tools::{ToolCommand, ToolRun, Tools};

/// Listing is fast; a stuck listing almost certainly means a tool waiting on something.
const LIST_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Extraction of very large sets on slow disks can take hours.
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
/// Listings must be complete to be checked; refuse archives with absurdly long listings.
const LIST_CAPTURE: usize = 64 * 1024 * 1024;
const EXTRACT_CAPTURE: usize = 1024 * 1024;

/// Placeholder password used when the job has none. Passing *some* password guarantees
/// that 7-Zip and unzip never fall back to an interactive prompt (unrar has `-p-` for
/// this). For an unencrypted archive the value is ignored; for an encrypted one it simply
/// fails like a wrong password, which is reported as "password required".
const NO_PASSWORD_PLACEHOLDER: &str = "newskite-no-password";

/// Which external program handles a set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Extractor {
    SevenZip(PathBuf),
    Unrar(PathBuf),
    Unzip(PathBuf),
}

impl Extractor {
    fn name(&self) -> &'static str {
        match self {
            Extractor::SevenZip(_) => "7z",
            Extractor::Unrar(_) => "unrar",
            Extractor::Unzip(_) => "unzip",
        }
    }
}

/// Fallback tool for a set the native backend cannot read: unrar for RAR, 7-Zip for
/// everything else, unzip for single-volume zip when 7-Zip is missing. (7-Zip can also
/// read RAR when unrar is absent.)
pub(crate) fn choose_extractor(set: &ArchiveSet, tools: &Tools) -> Option<Extractor> {
    let sevenz = tools.sevenz.clone().map(Extractor::SevenZip);
    match set.kind {
        ArchiveKind::Rar => tools.unrar.clone().map(Extractor::Unrar).or(sevenz),
        ArchiveKind::SevenZip => sevenz,
        ArchiveKind::Zip => sevenz.or_else(|| {
            let single = set.volumes.len() == 1;
            if single { tools.unzip.clone().map(Extractor::Unzip) } else { None }
        }),
    }
}

fn path_arg(p: &Path) -> OsString {
    p.as_os_str().to_os_string()
}

/// Command that lists entry paths of `archive`.
pub(crate) fn list_command(ex: &Extractor, archive: &Path, password: Option<&str>) -> ToolCommand {
    match ex {
        Extractor::SevenZip(p) => ToolCommand::new(p)
            .arg("l")
            .arg("-slt")
            .secret_arg("-p", password.unwrap_or(NO_PASSWORD_PLACEHOLDER))
            .arg("--")
            .arg(path_arg(archive)),
        Extractor::Unrar(p) => {
            let c = ToolCommand::new(p).arg("lb").arg("-c-");
            let c = match password {
                Some(pw) => c.secret_arg("-p", pw),
                None => c.arg("-p-"),
            };
            c.arg("--").arg(path_arg(archive))
        }
        // zipinfo mode: one entry name per line; names are never encrypted in zip.
        Extractor::Unzip(p) => ToolCommand::new(p).arg("-Z1").arg(path_arg(archive)),
    }
}

/// Command that extracts `archive` into `target` (overwriting, never prompting).
pub(crate) fn extract_command(ex: &Extractor, archive: &Path, target: &Path, password: Option<&str>) -> ToolCommand {
    match ex {
        Extractor::SevenZip(p) => {
            let mut out = OsString::from("-o");
            out.push(target.as_os_str());
            ToolCommand::new(p)
                .arg("x")
                .arg("-y")
                .arg("-aoa")
                .arg("-bd")
                .secret_arg("-p", password.unwrap_or(NO_PASSWORD_PLACEHOLDER))
                .arg(out)
                .arg("--")
                .arg(path_arg(archive))
        }
        Extractor::Unrar(p) => {
            let c = ToolCommand::new(p).arg("x").arg("-y").arg("-o+").arg("-c-");
            let c = match password {
                Some(pw) => c.secret_arg("-p", pw),
                None => c.arg("-p-"),
            };
            // unrar treats the destination as a folder only with a trailing separator.
            let mut dest = target.as_os_str().to_os_string();
            dest.push(std::path::MAIN_SEPARATOR_STR);
            c.arg("--").arg(path_arg(archive)).arg(dest)
        }
        Extractor::Unzip(p) => ToolCommand::new(p)
            .arg("-o")
            .arg("-P")
            .secret_arg("", password.unwrap_or(NO_PASSWORD_PLACEHOLDER))
            .arg(path_arg(archive))
            .arg("-d")
            .arg(path_arg(target)),
    }
}

/// Entry paths from `7z l -slt` output (the `Path = ` lines after the `----------`
/// separator; the block before it describes the archive itself).
pub fn parse_7z_listing(out: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut in_entries = false;
    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        if !in_entries {
            if line.trim() == "----------" {
                in_entries = true;
            }
            continue;
        }
        if let Some(p) = line.strip_prefix("Path = ") {
            entries.push(p.to_string());
        }
    }
    entries
}

/// Entry paths from `unrar lb` / `unzip -Z1` output: one per non-empty line.
pub fn parse_bare_listing(out: &str) -> Vec<String> {
    out.lines().map(|l| l.trim_end_matches('\r')).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

/// True when an archive entry path could escape the extraction folder: absolute paths
/// (`/x`, `\x`, `C:...`, UNC) or any `..` component, with both `/` and `\` treated as
/// separators (archives created on Windows use `\`).
pub fn is_unsafe_entry(name: &str) -> bool {
    if name.starts_with('/') || name.starts_with('\\') {
        return true;
    }
    let b = name.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return true;
    }
    name.split(['/', '\\']).any(|c| c == "..")
}

/// Short failure reason from a tool run.
pub(crate) fn failure_reason(tool: &str, run: &ToolRun, password_given: bool) -> String {
    if run.timed_out {
        return format!("{tool} timed out");
    }
    let lower = run.combined().to_lowercase();
    let password_problem = ["wrong password", "incorrect password", "password is incorrect", "password incorrect"]
        .iter()
        .any(|p| lower.contains(p))
        || (tool == "unrar" && run.code == Some(11))
        || (tool == "unzip" && run.code == Some(82));
    if password_problem {
        return if password_given {
            native::WRONG_PASSWORD.into()
        } else {
            native::PASSWORD_REQUIRED.into()
        };
    }
    if [
        "missing volume",
        "unexpected end of archive",
        "start extraction from a previous volume",
        "cannot find volume",
        "insert disk",
    ]
    .iter()
    .any(|p| lower.contains(p))
    {
        return "missing or incomplete archive volume".into();
    }
    if lower.contains("no space left") || lower.contains("disk full") || lower.contains("not enough space") {
        return "not enough disk space".into();
    }
    if [
        "crc failed",
        "checksum error",
        "data error",
        "headers error",
        "can't open as archive",
        "cannot open the file as archive",
        "is not rar archive",
        "end-of-central-directory signature not found",
        "corrupt",
        "unsupported method",
    ]
    .iter()
    .any(|p| lower.contains(p))
    {
        return "archive is corrupt or damaged".into();
    }
    let detail = run
        .combined()
        .lines()
        .map(str::trim)
        .find(|l| l.to_lowercase().contains("error"))
        .map(|l| {
            let short: String = l.chars().take(160).collect();
            format!(": {short}")
        })
        .unwrap_or_default();
    match run.code {
        Some(c) => format!("{tool} exited with code {c}{detail}"),
        None => format!("{tool} was terminated{detail}"),
    }
}

/// Exit code 0 is success; 1 (warning) is accepted unless the output names a real
/// problem; anything else fails.
fn run_succeeded(tool: &str, run: &ToolRun) -> bool {
    if run.timed_out {
        return false;
    }
    match run.code {
        Some(0) => true,
        Some(1) => {
            let reason = failure_reason(tool, run, true);
            reason.starts_with(tool) // only the generic fallback reason => plain warning
        }
        _ => false,
    }
}

/// Outcome of one archive set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SetOutcome {
    Extracted,
    /// Not produced since extraction became native (kept for the caller's match).
    #[allow(dead_code)]
    Skipped(String),
    Failed(String),
}

/// List, check and extract one set into `target` (which must exist): natively, or with
/// an external tool when the native backend reports the archive as unsupported and a
/// suitable tool is installed.
pub(crate) fn extract_set(
    set: &ArchiveSet,
    target: &Path,
    tools: &Tools,
    password: Option<&str>,
    log: &mut dyn FnMut(String),
) -> SetOutcome {
    let name = set.first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let staging = match make_staging_dir(target) {
        Ok(p) => p,
        Err(e) => return SetOutcome::Failed(format!("could not create a staging folder in {}: {e}", target.display())),
    };
    let native = native::extract(set, &staging, password, log);

    if let Err(NativeError::Unsupported(why)) = &native {
        // Nothing of a run that stopped on an unsupported feature is kept; the tool
        // starts over in a staging folder of its own.
        if let Err(e) = std::fs::remove_dir_all(&staging) {
            log(format!("extract: could not remove staging folder {}: {e}", staging.display()));
        }
        let Some(ex) = choose_extractor(set, tools) else {
            let hint = match set.kind {
                ArchiveKind::Rar => "unrar or 7-Zip",
                ArchiveKind::SevenZip => "7-Zip",
                ArchiveKind::Zip => "7-Zip",
            };
            let reason = format!("{why}; install {hint} to extract it");
            log(format!("extract: {name}: {reason}"));
            return SetOutcome::Failed(reason);
        };
        log(format!("extract: {name}: native extraction not possible ({why}); falling back to {}", ex.name()));
        return extract_with_tool(set, target, &ex, password, log);
    }

    // Same finishing steps as for a tool (see `extract_with_tool`). The native sink
    // never creates links, so the link sweep is a second line of defense only.
    let staged_links = remove_links_outside(&staging, true, log);
    let merged = merge_into(&staging, target, log);
    if let Err(e) = std::fs::remove_dir_all(&staging) {
        log(format!("extract: could not remove staging folder {}: {e}", staging.display()));
    }
    let target_links = remove_escaping_links(target, log);

    let summary = match native {
        Ok(s) => s,
        Err(e) => return SetOutcome::Failed(e.to_string()),
    };
    if let Some(link) = summary.escaping_link {
        return SetOutcome::Failed(format!("archive contained a link pointing outside the target ({link})"));
    }
    if let Err(reason) = staged_links.and(merged).and(target_links) {
        return SetOutcome::Failed(reason);
    }
    SetOutcome::Extracted
}

/// The pre-native extraction path through an external tool: list, check, extract into
/// a private staging folder, then sweep links and merge into `target`.
fn extract_with_tool(
    set: &ArchiveSet,
    target: &Path,
    ex: &Extractor,
    password: Option<&str>,
    log: &mut dyn FnMut(String),
) -> SetOutcome {
    let name = set.first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let ex = ex.clone();
    let tool = ex.name();

    // 1. List and refuse entries that could escape the target folder.
    let list = list_command(&ex, &set.first, password);
    log(format!("extract: {}", list.display()));
    let lrun = match list.run(LIST_TIMEOUT, LIST_CAPTURE) {
        Ok(r) => r,
        Err(e) => return SetOutcome::Failed(e.to_string()),
    };
    if !run_succeeded(tool, &lrun) {
        return SetOutcome::Failed(failure_reason(tool, &lrun, password.is_some()));
    }
    if lrun.truncated {
        return SetOutcome::Failed("archive listing is too large to check".into());
    }
    let entries = match ex {
        Extractor::SevenZip(_) => parse_7z_listing(&lrun.stdout),
        _ => parse_bare_listing(&lrun.stdout),
    };
    if let Some(bad) = entries.iter().find(|e| is_unsafe_entry(e)) {
        let shown: String = bad.chars().take(120).collect();
        log(format!("extract: {name}: refusing unsafe entry path {shown:?}"));
        return SetOutcome::Failed(format!("archive contains an unsafe path ({shown})"));
    }
    log(format!("extract: {name}: {} entries via {tool}", entries.len()));

    // 2. Extract into a fresh private staging folder inside the target, so nothing the
    //    tool writes can pass through a link left in the target by an earlier set, a
    //    failed run or a previous attempt.
    let staging = match make_staging_dir(target) {
        Ok(p) => p,
        Err(e) => return SetOutcome::Failed(format!("could not create a staging folder in {}: {e}", target.display())),
    };
    let cmd = extract_command(&ex, &set.first, &staging, password);
    log(format!("extract: {}", cmd.display()));
    let xrun = cmd.run(EXTRACT_TIMEOUT, EXTRACT_CAPTURE);

    // 3. Whatever the outcome (a failed run can leave partial output, including links):
    //    drop links leaving the staging folder, move the rest into the target without
    //    following any link already there, remove the staging folder and check the
    //    whole target once more.
    let staged_links = remove_links_outside(&staging, true, log);
    let merged = merge_into(&staging, target, log);
    if let Err(e) = std::fs::remove_dir_all(&staging) {
        log(format!("extract: could not remove staging folder {}: {e}", staging.display()));
    }
    let target_links = remove_escaping_links(target, log);

    let xrun = match xrun {
        Ok(r) => r,
        Err(e) => return SetOutcome::Failed(e.to_string()),
    };
    if !run_succeeded(tool, &xrun) {
        return SetOutcome::Failed(failure_reason(tool, &xrun, password.is_some()));
    }
    if let Err(reason) = staged_links.and(merged).and(target_links) {
        return SetOutcome::Failed(reason);
    }
    SetOutcome::Extracted
}

/// Prefix of the per-set staging folders created inside the extraction folder.
const STAGING_PREFIX: &str = ".newskite-staging-";

/// Create a new, empty, private folder inside `target` for one extraction run.
fn make_staging_dir(target: &Path) -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut last_err = None;
    for _ in 0..16 {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = target.join(format!("{STAGING_PREFIX}{}-{seq}-{nanos:09}", std::process::id()));
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        match b.create(&p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("no free staging folder name")))
}

/// Remove a file or link (directory links on Windows need `remove_dir`).
fn remove_non_dir(p: &Path) -> std::io::Result<()> {
    std::fs::remove_file(p).or_else(|e| std::fs::remove_dir(p).map_err(|_| e))
}

/// Move everything in `src` into `dst`, merging folders. Existing files and links in
/// `dst` are replaced (like the tools' overwrite mode); a link in `dst` is never followed,
/// so a folder from `src` replaces a link of the same name instead of being merged
/// through it. A non-folder in `src` never replaces a real folder in `dst` (reported).
fn merge_into(src: &Path, dst: &Path, log: &mut dyn FnMut(String)) -> Result<(), String> {
    let mut first_err: Option<String> = None;
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((from_dir, to_dir)) = stack.pop() {
        let rd = match std::fs::read_dir(&from_dir) {
            Ok(rd) => rd,
            Err(e) => {
                first_err.get_or_insert(format!("could not read {}: {e}", from_dir.display()));
                continue;
            }
        };
        for entry in rd.filter_map(Result::ok) {
            let from = entry.path();
            let to = to_dir.join(entry.file_name());
            let Ok(ft) = entry.file_type() else { continue };
            let res = match std::fs::symlink_metadata(&to) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::rename(&from, &to),
                Err(e) => Err(e),
                Ok(m) if m.is_dir() && ft.is_dir() => {
                    stack.push((from, to));
                    continue;
                }
                Ok(m) if m.is_dir() => {
                    let msg = format!("{} conflicts with an existing folder", to.display());
                    log(format!("extract: {msg}"));
                    first_err.get_or_insert(msg);
                    continue;
                }
                Ok(_) => remove_non_dir(&to).and_then(|()| std::fs::rename(&from, &to)),
            };
            if let Err(e) = res {
                let msg = format!("could not move {} into place: {e}", to.display());
                log(format!("extract: {msg}"));
                first_err.get_or_insert(msg);
            }
        }
    }
    first_err.map_or(Ok(()), Err)
}

/// Lexically normalize `p` (resolve `.`/`..` without touching the filesystem).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Walk `root` (not following links) and delete symbolic links whose destination is
/// outside `root`. Returns an error describing the first such link.
pub(crate) fn remove_escaping_links(root: &Path, log: &mut dyn FnMut(String)) -> Result<(), String> {
    remove_links_outside(root, false, log)
}

/// Like [`remove_escaping_links`]; with `reject_absolute`, every link with an absolute
/// destination is deleted too (an archive cannot know where it will be extracted).
/// A link whose destination exists is judged by its fully resolved path (so chains of
/// links are followed); a dangling one by its lexically normalized destination.
fn remove_links_outside(root: &Path, reject_absolute: bool, log: &mut dyn FnMut(String)) -> Result<(), String> {
    let root_n = normalize(root);
    let root_c = std::fs::canonicalize(root).ok();
    let mut stack = vec![root.to_path_buf()];
    let mut first_bad: Option<String> = None;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.filter_map(Result::ok) {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_symlink() {
                let Ok(dest) = std::fs::read_link(&path) else { continue };
                let lexical_inside = {
                    let resolved = if dest.is_absolute() { dest.clone() } else { dir.join(&dest) };
                    normalize(&resolved).starts_with(&root_n)
                };
                let inside = match (std::fs::canonicalize(&path), &root_c) {
                    (Ok(real), Some(rc)) => real.starts_with(rc) && lexical_inside,
                    _ => lexical_inside,
                };
                if !inside || (reject_absolute && dest.is_absolute()) {
                    log(format!("extract: removing link {} pointing outside the target", path.display()));
                    let _ = remove_non_dir(&path);
                    first_bad.get_or_insert_with(|| {
                        format!(
                            "archive contained a link pointing outside the target ({})",
                            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                        )
                    });
                }
            } else if ft.is_dir() {
                stack.push(path);
            }
        }
    }
    first_bad.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_entries() {
        for bad in [
            "../evil.txt",
            "a/../../evil",
            "a\\..\\evil",
            "/etc/passwd",
            "\\windows\\system32",
            "C:\\x",
            "c:x",
            "..",
            "dir/..",
        ] {
            assert!(is_unsafe_entry(bad), "{bad}");
        }
        for ok in ["a.txt", "dir/a.txt", "dir\\a.txt", "..hidden", "a..b/c", "...", "dir/.../x"] {
            assert!(!is_unsafe_entry(ok), "{ok}");
        }
    }

    #[test]
    fn parses_7z_slt_listing_and_detects_zip_slip() {
        let out = "\n7-Zip 26.00 (x64)\n\nListing archive: evil.zip\n\n--\nPath = evil.zip\nType = zip\nPhysical Size = 300\n\n----------\nPath = good.txt\nFolder = -\nSize = 5\n\nPath = ../evil.txt\nFolder = -\nSize = 4\n\n";
        let entries = parse_7z_listing(out);
        assert_eq!(entries, vec!["good.txt".to_string(), "../evil.txt".to_string()]);
        assert!(entries.iter().any(|e| is_unsafe_entry(e)));
        // The archive's own Path line before the separator is not an entry.
        assert!(!entries.contains(&"evil.zip".to_string()));
    }

    #[test]
    fn parses_bare_listing_and_detects_zip_slip() {
        let out = "docs/readme.txt\r\n/abs/path\r\n\r\nok.bin\n";
        let entries = parse_bare_listing(out);
        assert_eq!(entries, vec!["docs/readme.txt", "/abs/path", "ok.bin"]);
        let bad: Vec<_> = entries.iter().filter(|e| is_unsafe_entry(e)).collect();
        assert_eq!(bad, vec!["/abs/path"]);
    }

    fn run(code: i32, out: &str) -> ToolRun {
        ToolRun { code: Some(code), stdout: out.into(), ..Default::default() }
    }

    #[test]
    fn failure_reasons() {
        let r = run(2, "ERROR: Wrong password : a.bin");
        assert_eq!(failure_reason("7z", &r, true), "wrong password");
        assert!(failure_reason("7z", &r, false).contains("set a password"));
        let r = run(2, "ERROR: enc.7z : Cannot open encrypted archive. Wrong password?");
        assert_eq!(failure_reason("7z", &r, true), "wrong password");
        assert_eq!(failure_reason("unrar", &run(11, "The specified password is incorrect."), true), "wrong password");
        assert_eq!(failure_reason("unzip", &run(82, "skipping: a.bin  incorrect password"), true), "wrong password");
        assert_eq!(failure_reason("7z", &run(2, "ERROR: Missing volume : x.7z.002"), false), "missing or incomplete archive volume");
        assert_eq!(failure_reason("7z", &run(2, "ERROR: x.7z\nCan't open as archive: 1"), false), "archive is corrupt or damaged");
        assert_eq!(failure_reason("7z", &run(7, "Command Line Error:\nUnknown switch"), false), "7z exited with code 7: Command Line Error:");
        let t = ToolRun { timed_out: true, ..Default::default() };
        assert_eq!(failure_reason("unrar", &t, false), "unrar timed out");
    }

    #[test]
    fn warning_exit_accepted_only_without_problems() {
        assert!(run_succeeded("7z", &run(0, "Everything is Ok")));
        assert!(run_succeeded("7z", &run(1, "WARNING: cannot set times")));
        assert!(!run_succeeded("unzip", &run(1, "skipping: a  incorrect password")));
        assert!(!run_succeeded("7z", &run(2, "")));
    }

    fn set(kind: ArchiveKind, vols: usize) -> ArchiveSet {
        let volumes: Vec<PathBuf> = (0..vols).map(|i| PathBuf::from(format!("v{i}"))).collect();
        ArchiveSet { kind, first: volumes[0].clone(), volumes }
    }

    #[test]
    fn extractor_choice() {
        let all = Tools {
            par2: None,
            sevenz: Some("7z".into()),
            unrar: Some("unrar".into()),
            unzip: Some("unzip".into()),
        };
        assert_eq!(choose_extractor(&set(ArchiveKind::Rar, 1), &all), Some(Extractor::Unrar("unrar".into())));
        assert_eq!(choose_extractor(&set(ArchiveKind::Zip, 1), &all), Some(Extractor::SevenZip("7z".into())));
        let no7z = Tools { sevenz: None, ..all.clone() };
        assert_eq!(choose_extractor(&set(ArchiveKind::Zip, 1), &no7z), Some(Extractor::Unzip("unzip".into())));
        assert_eq!(choose_extractor(&set(ArchiveKind::Zip, 2), &no7z), None);
        assert_eq!(choose_extractor(&set(ArchiveKind::SevenZip, 1), &no7z), None);
        let only7z = Tools { sevenz: Some("7z".into()), ..Tools::none() };
        assert_eq!(choose_extractor(&set(ArchiveKind::Rar, 3), &only7z), Some(Extractor::SevenZip("7z".into())));
    }

    #[test]
    fn commands_never_prompt_and_redact_password() {
        let a = Path::new("a.rar");
        let t = Path::new("out");
        let unrar = Extractor::Unrar("unrar".into());
        let c = extract_command(&unrar, a, t, None);
        assert!(c.args.iter().any(|x| x == "-p-"));
        let c = extract_command(&unrar, a, t, Some("s3cret"));
        assert!(!c.display().contains("s3cret"));
        assert!(c.args.last().is_some_and(|d| d.to_string_lossy().ends_with(std::path::MAIN_SEPARATOR)));
        let z = Extractor::SevenZip("7z".into());
        let c = extract_command(&z, a, t, None);
        assert!(c.args.iter().any(|x| x.to_string_lossy().starts_with("-p") && x.len() > 2));
        assert!(c.args.iter().any(|x| x == "-y"));
        let c = list_command(&z, a, Some("s3cret"));
        assert!(!c.display().contains("s3cret"));
        let u = Extractor::Unzip("unzip".into());
        let c = extract_command(&u, a, t, Some("s3cret"));
        assert!(!c.display().contains("s3cret"));
        assert!(c.display().contains("-P ***"));
    }

    #[cfg(unix)]
    #[test]
    fn escaping_links_removed() {
        let root = std::env::temp_dir().join(format!("nk-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::os::unix::fs::symlink("../../../etc", root.join("sub/out")).unwrap();
        std::os::unix::fs::symlink("../sub", root.join("sub/in")).unwrap();
        let mut logs = Vec::new();
        let r = remove_escaping_links(&root, &mut |l| logs.push(l));
        assert!(r.is_err());
        assert!(std::fs::symlink_metadata(root.join("sub/out")).is_err());
        assert!(std::fs::symlink_metadata(root.join("sub/in")).is_ok());
        assert!(remove_escaping_links(&root, &mut |_| {}).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("nk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[cfg(unix)]
    #[test]
    fn link_chains_resolved_and_absolute_links_rejected_in_staging() {
        let base = scratch("chain");
        let root = base.join("root");
        std::fs::create_dir_all(root.join("s")).unwrap();
        // s/up -> .. (the root, fine); s/a -> up/x/../.. looks inside lexically but
        // really resolves to the root's parent.
        std::fs::create_dir_all(root.join("x")).unwrap();
        std::os::unix::fs::symlink("..", root.join("s/up")).unwrap();
        std::os::unix::fs::symlink("up/x/../..", root.join("s/a")).unwrap();
        std::os::unix::fs::symlink(root.join("x"), root.join("abs_in")).unwrap();
        assert!(remove_escaping_links(&root, &mut |_| {}).is_err());
        assert!(std::fs::symlink_metadata(root.join("s/a")).is_err());
        assert!(std::fs::symlink_metadata(root.join("s/up")).is_ok());
        assert!(std::fs::symlink_metadata(root.join("abs_in")).is_ok());
        assert!(remove_links_outside(&root, true, &mut |_| {}).is_err());
        assert!(std::fs::symlink_metadata(root.join("abs_in")).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn merge_replaces_links_and_never_follows_them() {
        let base = scratch("merge");
        let (src, dst, outside) = (base.join("src"), base.join("dst"), base.join("outside"));
        for d in [src.join("link"), src.join("dir"), dst.join("dir"), outside.clone()] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(src.join("link/pwn.txt"), b"owned").unwrap();
        std::fs::write(src.join("dir/new.txt"), b"new").unwrap();
        std::fs::write(src.join("f.txt"), b"two").unwrap();
        std::fs::write(dst.join("dir/old.txt"), b"old").unwrap();
        std::fs::write(dst.join("f.txt"), b"one").unwrap();
        std::os::unix::fs::symlink(&outside, dst.join("link")).unwrap();
        assert_eq!(merge_into(&src, &dst, &mut |_| {}), Ok(()));
        assert!(!outside.join("pwn.txt").exists());
        assert!(!std::fs::symlink_metadata(dst.join("link")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(dst.join("link/pwn.txt")).unwrap(), b"owned");
        assert_eq!(std::fs::read(dst.join("dir/old.txt")).unwrap(), b"old");
        assert_eq!(std::fs::read(dst.join("dir/new.txt")).unwrap(), b"new");
        assert_eq!(std::fs::read(dst.join("f.txt")).unwrap(), b"two");
        // A file never replaces a real folder.
        std::fs::remove_dir_all(src.join("dir")).unwrap();
        std::fs::write(src.join("dir"), b"file").unwrap();
        assert!(merge_into(&src, &dst, &mut |_| {}).is_err());
        assert!(dst.join("dir/old.txt").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
