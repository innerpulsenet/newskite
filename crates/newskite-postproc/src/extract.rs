//! Archive listing, safety checks and extraction through 7-Zip / unrar / unzip.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::detect::{ArchiveKind, ArchiveSet};
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

/// Prefer unrar for RAR, 7-Zip for everything else, unzip for single-volume zip when
/// 7-Zip is missing. (7-Zip can also read RAR when unrar is absent.)
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
            "wrong password".into()
        } else {
            "archive is password-protected; set a password for this job".into()
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
    Skipped(String),
    Failed(String),
}

/// List, check and extract one set into `target` (which must exist).
pub(crate) fn extract_set(
    set: &ArchiveSet,
    target: &Path,
    tools: &Tools,
    password: Option<&str>,
    log: &mut dyn FnMut(String),
) -> SetOutcome {
    let name = set.first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Some(ex) = choose_extractor(set, tools) else {
        let hint = match set.kind {
            ArchiveKind::Rar => "install unrar or 7-Zip",
            ArchiveKind::SevenZip => "install 7-Zip",
            ArchiveKind::Zip => "install 7-Zip or unzip",
        };
        let reason = format!("no tool to extract {} archives ({hint})", set.kind.name());
        log(format!("extract: {name}: {reason}"));
        return SetOutcome::Skipped(reason);
    };
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

    // 2. Extract.
    let cmd = extract_command(&ex, &set.first, target, password);
    log(format!("extract: {}", cmd.display()));
    let xrun = match cmd.run(EXTRACT_TIMEOUT, EXTRACT_CAPTURE) {
        Ok(r) => r,
        Err(e) => return SetOutcome::Failed(e.to_string()),
    };
    if !run_succeeded(tool, &xrun) {
        return SetOutcome::Failed(failure_reason(tool, &xrun, password.is_some()));
    }

    // 3. Defense in depth: no extracted link may point outside the target.
    if let Err(reason) = remove_escaping_links(target, log) {
        return SetOutcome::Failed(reason);
    }
    SetOutcome::Extracted
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
    let root_n = normalize(root);
    let mut stack = vec![root.to_path_buf()];
    let mut first_bad: Option<String> = None;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.filter_map(Result::ok) {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_symlink() {
                let Ok(dest) = std::fs::read_link(&path) else { continue };
                let resolved = if dest.is_absolute() { dest.clone() } else { dir.join(&dest) };
                if !normalize(&resolved).starts_with(&root_n) {
                    log(format!("extract: removing link {} pointing outside the target", path.display()));
                    // Directory links on Windows need remove_dir; try both.
                    if std::fs::remove_file(&path).is_err() {
                        let _ = std::fs::remove_dir(&path);
                    }
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
}
