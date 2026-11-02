//! Archive extraction through the native backends (see [`crate::native`]) and the shared
//! safety steps around them: a private staging folder per set, the link sweep and the
//! merge into the target folder.

use std::path::{Component, Path, PathBuf};

use crate::detect::ArchiveSet;
use crate::native;

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

/// Outcome of one archive set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SetOutcome {
    Extracted,
    /// Not produced since extraction became native (kept for the caller's match).
    #[allow(dead_code)]
    Skipped(String),
    Failed(String),
}

/// List, check and extract one set into `target` (which must exist). The backend writes
/// into a fresh private staging folder inside `target`, so nothing it writes can pass
/// through a link left in the target by an earlier set, a failed run or a previous
/// attempt. Whatever the outcome (a failed run can leave partial output), the staging
/// folder is swept for links, merged into `target` without following any link already
/// there and removed, and the whole target is checked once more. A set the backend
/// reports as unsupported (an unknown codec or RAR feature) fails with that reason.
pub(crate) fn extract_set(
    set: &ArchiveSet,
    target: &Path,
    password: Option<&str>,
    log: &mut dyn FnMut(String),
) -> SetOutcome {
    let staging = match make_staging_dir(target) {
        Ok(p) => p,
        Err(e) => return SetOutcome::Failed(format!("could not create a staging folder in {}: {e}", target.display())),
    };
    let native = native::extract(set, &staging, password, log);

    // The native sink never creates links, so the link sweeps are a second line of
    // defense only (and catch links an earlier run left in the target).
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
/// `dst` are replaced (an archive overwrites older output); a link in `dst` is never followed,
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
