//! Free space and size of the download folder's filesystem (the sidebar's disk meter and
//! the status bar's reading).

use std::path::Path;

/// Free bytes (available to an unprivileged user) and size of the filesystem holding
/// `dir`, or its nearest existing ancestor (the download folder may not exist yet), read
/// with one call. `None` when no ancestor can be read or the platform has no way to ask.
pub fn space(dir: &Path) -> Option<(u64, u64)> {
    if dir.as_os_str().is_empty() {
        return None;
    }
    let probe = dir.ancestors().find(|p| p.is_dir())?;
    space_of(probe)
}

/// Free bytes of the filesystem holding `dir` (see [`space`]).
pub fn free_bytes(dir: &Path) -> Option<u64> {
    space(dir).map(|(free, _)| free)
}

/// Size in bytes of the filesystem holding `dir` (see [`space`]).
pub fn total_bytes(dir: &Path) -> Option<u64> {
    space(dir).map(|(_, total)| total)
}

#[cfg(unix)]
fn space_of(p: &Path) -> Option<(u64, u64)> {
    let st = rustix::fs::statvfs(p).ok()?;
    Some((st.f_bavail.saturating_mul(st.f_frsize), st.f_blocks.saturating_mul(st.f_frsize)))
}

#[cfg(windows)]
fn space_of(p: &Path) -> Option<(u64, u64)> {
    crate::win::disk_space(p)
}

#[cfg(not(any(unix, windows)))]
fn space_of(_p: &Path) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_space_of_a_missing_folder_reads_its_parent() {
        let tmp = std::env::temp_dir();
        let missing = tmp.join("newskite-no-such-folder").join("deeper");
        assert_eq!(free_bytes(&missing).is_some(), free_bytes(&tmp).is_some());
        assert_eq!(free_bytes(Path::new("")), None);
        if let (Some(free), Some(total)) = (free_bytes(&tmp), total_bytes(&tmp)) {
            assert!(free <= total);
        }
    }
}
