//! Free space on the download folder's filesystem (the status bar's reading).

use std::path::Path;

/// Bytes available to an unprivileged user on the filesystem holding `dir`, or on its
/// nearest existing ancestor (the download folder may not exist yet). `None` when no
/// ancestor can be read or the platform has no `statvfs`.
pub fn free_bytes(dir: &Path) -> Option<u64> {
    if dir.as_os_str().is_empty() {
        return None;
    }
    let probe = dir.ancestors().find(|p| p.is_dir())?;
    statvfs_free(probe)
}

#[cfg(unix)]
fn statvfs_free(p: &Path) -> Option<u64> {
    let st = rustix::fs::statvfs(p).ok()?;
    Some(st.f_bavail.saturating_mul(st.f_frsize))
}

#[cfg(not(unix))]
fn statvfs_free(_p: &Path) -> Option<u64> {
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
    }
}
