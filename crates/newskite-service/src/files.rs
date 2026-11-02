//! Deleting a finished job's download folder ([`crate::Service::delete_job_files`]).
//!
//! A job's folder is `<download_dir>/<job name>` and no two jobs share one (see
//! `unique_dest`), so the whole folder belongs to the job. Before anything is deleted the
//! folder is checked so a stale or hand-edited row can never take other data with it.

use std::path::{Component, Path, PathBuf};

use newskite_store::Job;

/// Why a job's folder may not be deleted.
pub(crate) fn refuse_reason(job: &Job, download_dir: &Path, others: &[Job]) -> Option<String> {
    let dir = &job.dest_dir;
    if !dir.is_absolute() {
        return Some(format!("{} is not an absolute path", dir.display()));
    }
    if dir.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Some(format!("{} is not a plain path", dir.display()));
    }
    // A folder directly under the root (or the root itself) is never a job folder.
    if dir.components().filter(|c| matches!(c, Component::Normal(_))).count() < 2 {
        return Some(format!("{} is too close to the file system root", dir.display()));
    }
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let real = canon(dir);
    let base = canon(download_dir);
    if real == base || base.starts_with(&real) {
        return Some(format!("{} is (or holds) the download folder", dir.display()));
    }
    if let Some(home) = home_dir() {
        if canon(&home).starts_with(&real) {
            return Some(format!("{} is (or holds) the home folder", dir.display()));
        }
    }
    for other in others.iter().filter(|o| o.id != job.id) {
        let o = canon(&other.dest_dir);
        if o.starts_with(&real) || real.starts_with(&o) {
            return Some(format!("{} is shared with {}", dir.display(), other.name));
        }
    }
    None
}

fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

/// What a deletion removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Deleted {
    /// Regular files removed.
    pub files: u64,
    /// Their total size.
    pub bytes: u64,
}

/// Deletes `dir` and everything in it without following links (a link is removed, never
/// its target). A missing folder is not an error. A link at `dir` itself is refused.
pub(crate) fn delete_tree(dir: &Path) -> std::io::Result<Deleted> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Deleted::default()),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return Err(std::io::Error::other(format!("{} is not a folder", dir.display())));
    }
    let mut out = Deleted::default();
    tally(dir, &mut out);
    std::fs::remove_dir_all(dir)?;
    Ok(out)
}

fn tally(dir: &Path, out: &mut Deleted) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            tally(&entry.path(), out);
        } else if ft.is_file() {
            out.files += 1;
            out.bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use newskite_store::JobState;

    fn job(id: i64, dir: &Path) -> Job {
        Job {
            id,
            name: format!("job{id}"),
            state: JobState::Done,
            bytes_total: 0,
            bytes_done: 0,
            dest_dir: dir.to_path_buf(),
            source_path: PathBuf::new(),
            created_at: 0,
            error: None,
            password: None,
            stats: Default::default(),
        }
    }

    #[test]
    fn refuses_dangerous_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("downloads");
        std::fs::create_dir_all(base.join("a")).unwrap();
        let a = job(1, &base.join("a"));
        assert_eq!(refuse_reason(&a, &base, std::slice::from_ref(&a)), None);
        assert!(refuse_reason(&job(2, &base), &base, &[]).is_some());
        assert!(refuse_reason(&job(2, tmp.path()), &base, &[]).is_some());
        assert!(refuse_reason(&job(2, Path::new("rel/dir")), &base, &[]).is_some());
        assert!(refuse_reason(&job(2, Path::new("/x")), &base, &[]).is_some());
        assert!(refuse_reason(&job(2, &base.join("a/../b")), &base, &[]).is_some());
        let inner = job(3, &base.join("a/inner"));
        assert!(refuse_reason(&a, &base, &[a.clone(), inner]).unwrap().contains("shared"));
    }

    #[test]
    fn deletes_tree_and_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("job");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a.bin"), [0u8; 10]).unwrap();
        std::fs::write(d.join("sub/b.bin"), [0u8; 5]).unwrap();
        let out = delete_tree(&d).unwrap();
        assert_eq!(out, Deleted { files: 2, bytes: 15 });
        assert!(!d.exists());
        assert_eq!(delete_tree(&d).unwrap(), Deleted::default());
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_links() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), b"x").unwrap();
        let d = tmp.path().join("job");
        std::fs::create_dir_all(&d).unwrap();
        std::os::unix::fs::symlink(&outside, d.join("link")).unwrap();
        delete_tree(&d).unwrap();
        assert!(outside.join("keep").exists());
        let link = tmp.path().join("joblink");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(delete_tree(&link).is_err());
        assert!(outside.join("keep").exists());
    }
}
