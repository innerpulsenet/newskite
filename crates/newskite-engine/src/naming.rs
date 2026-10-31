//! Output-file naming helpers.

use std::path::{Path, PathBuf};

use crate::PART_EXTENSION;

/// `<final>.nkpart`, next to the final path.
pub(crate) fn part_path(final_path: &Path) -> PathBuf {
    let mut s = final_path.as_os_str().to_owned();
    s.push(".");
    s.push(PART_EXTENSION);
    PathBuf::from(s)
}

/// `name` for `n <= 1`, otherwise `"<stem> (n).<ext>"` (or `"<name> (n)"` without an
/// extension; a leading dot is not treated as an extension separator).
pub(crate) fn with_suffix(name: &str, n: u32) -> String {
    if n <= 1 {
        return name.to_string();
    }
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => format!("{} ({n}){}", &name[..i], &name[i..]),
        _ => format!("{name} ({n})"),
    }
}

/// Key used to detect name collisions. Lowercased because Windows and macOS file
/// systems are usually case-insensitive.
pub(crate) fn claim_key(name: &str) -> String {
    name.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes() {
        assert_eq!(with_suffix("a.bin", 1), "a.bin");
        assert_eq!(with_suffix("a.bin", 2), "a (2).bin");
        assert_eq!(with_suffix("archive.part01.rar", 3), "archive.part01 (3).rar");
        assert_eq!(with_suffix("README", 2), "README (2)");
        assert_eq!(with_suffix(".hidden", 2), ".hidden (2)");
        assert_eq!(with_suffix("trailing.", 2), "trailing. (2)");
    }

    #[test]
    fn part_path_appends_extension() {
        let p = part_path(Path::new("dir").join("movie.mkv").as_path());
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), "movie.mkv.nkpart");
        assert_eq!(p.parent().unwrap(), Path::new("dir"));
    }

    #[test]
    fn claim_keys_fold_case() {
        assert_eq!(claim_key("File.BIN"), claim_key("file.bin"));
    }
}
