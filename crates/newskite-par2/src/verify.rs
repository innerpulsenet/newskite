//! Verification: which slices of a recovery set are present, and where.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use md5::{Digest, Md5};

use crate::packet::{FileEntry, RecoverySet};
use crate::{Event, Stage};

/// Where one good input slice can be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliceSource {
    pub path: PathBuf,
    pub offset: u64,
}

/// State of one protected file after verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileStatus {
    /// Present under its name with the right content.
    Intact,
    /// Its content was found complete in another file (obfuscated or renamed copy).
    Misnamed(PathBuf),
    /// Present but damaged or truncated: `found` of `total` slices are good.
    Damaged { found: usize, total: usize },
    /// Not present; `found` slices were located in other files.
    Missing { found: usize, total: usize },
    /// Its name is not a safe relative path; it is never written.
    UnsafeName,
}

/// Verification result of one file.
#[derive(Clone, Debug)]
pub struct FileReport {
    pub name: String,
    pub length: u64,
    /// Where the file belongs (`None` for an unsafe name).
    pub target: Option<PathBuf>,
    pub status: FileStatus,
}

/// Verification result of a recovery set.
#[derive(Clone, Debug)]
pub struct Verification {
    pub files: Vec<FileReport>,
    /// Per global slice: where a good copy is, if anywhere.
    pub sources: Vec<Option<SliceSource>>,
    /// Recovery slices whose packets were found (checksums are checked at repair time).
    pub recovery_available: usize,
}

impl Verification {
    pub fn missing_slices(&self) -> usize {
        self.sources.iter().filter(|s| s.is_none()).count()
    }

    /// Every file is intact.
    pub fn is_intact(&self) -> bool {
        self.files.iter().all(|f| f.status == FileStatus::Intact)
    }

    /// Enough recovery slices exist for the missing ones (and no file has an unsafe name).
    pub fn repairable(&self) -> bool {
        self.missing_slices() <= self.recovery_available
            && !self.files.iter().any(|f| f.status == FileStatus::UnsafeName)
    }

    /// Recovery slices still needed beyond those available (0 when repairable).
    pub fn blocks_short(&self) -> usize {
        self.missing_slices().saturating_sub(self.recovery_available)
    }
}

/// `dir/name` when `name` is a plain relative path (`/` or `\` separated), else `None`.
pub fn safe_target(dir: &Path, name: &str) -> Option<PathBuf> {
    if name.starts_with(['/', '\\']) {
        return None;
    }
    let mut out = dir.to_path_buf();
    let mut any = false;
    for part in name.split(['/', '\\']) {
        if part.is_empty() {
            continue;
        }
        let p = Path::new(part);
        let mut comps = p.components();
        match (comps.next(), comps.next()) {
            (Some(Component::Normal(c)), None) if !part.contains(':') => out.push(c),
            _ => return None,
        }
        any = true;
    }
    any.then_some(out)
}

/// Reads up to `buf.len()` bytes at `offset`, zero-filling the rest. Returns bytes read.
pub(crate) fn read_padded(f: &mut File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    f.seek(SeekFrom::Start(offset))?;
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf[n..].fill(0);
    Ok(n)
}

fn md5_16k(path: &Path) -> Option<[u8; 16]> {
    let mut f = File::open(path).ok()?;
    let mut buf = vec![0u8; 16384];
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => return None,
        }
    }
    Some(Md5::digest(&buf[..n]).into())
}

/// Checks `path` against `entry` slice by slice. Returns (whole file correct, good slice
/// indices within the file).
fn check_file(
    path: &Path,
    entry: &FileEntry,
    slice_size: u64,
    buf: &mut [u8],
    progress: &mut dyn FnMut(u64),
) -> io::Result<(bool, Vec<usize>)> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut whole = Md5::new();
    let mut crc_ok = Vec::new();
    for k in 0..entry.slices.len() {
        let start = k as u64 * slice_size;
        if start >= len {
            break;
        }
        let n = read_padded(&mut f, start, buf)?;
        whole.update(&buf[..n.min(entry.slice_len(k, slice_size) as usize)]);
        if crc32fast::hash(buf) == entry.slices[k].crc {
            crc_ok.push(k);
        }
        progress(n as u64);
    }
    let intact = len == entry.length && whole.finalize().as_slice() == entry.md5;
    if intact {
        return Ok((true, (0..entry.slices.len()).collect()));
    }
    // Damaged: confirm the CRC matches with MD5 (a second read of those slices only).
    let mut good = Vec::new();
    for k in crc_ok {
        read_padded(&mut f, k as u64 * slice_size, buf)?;
        if Md5::digest(&*buf).as_slice() == entry.slices[k].md5 {
            good.push(k);
        }
    }
    Ok((false, good))
}

fn full_md5(path: &Path) -> Option<[u8; 16]> {
    let mut f = File::open(path).ok()?;
    let mut h = Md5::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => h.update(&buf[..n]),
            Err(_) => return None,
        }
    }
    Some(h.finalize().into())
}

/// Verifies `set` against the files in `dir`. `others` are further candidate files (all
/// regular files of the folder that are not PAR2 files); renamed copies and stray good
/// slices are looked for in them.
pub fn verify(set: &RecoverySet, dir: &Path, others: &[PathBuf], on: &mut dyn FnMut(Event)) -> Verification {
    let slice_size = set.slice_size;
    let mut buf = vec![0u8; slice_size as usize];
    let mut sources: Vec<Option<SliceSource>> = vec![None; set.slice_count()];
    let mut reports = Vec::new();
    let total_bytes = set.data_bytes();
    let mut done_bytes = 0u64;
    let targets: BTreeSet<PathBuf> = set.files.iter().filter_map(|f| safe_target(dir, &f.name)).collect();
    let mut used: BTreeSet<PathBuf> = BTreeSet::new();

    for (i, entry) in set.files.iter().enumerate() {
        on(Event::File { stage: Stage::Verify, index: i, count: set.files.len(), name: entry.name.clone() });
        let Some(target) = safe_target(dir, &entry.name) else {
            reports.push(FileReport { name: entry.name.clone(), length: entry.length, target: None, status: FileStatus::UnsafeName });
            continue;
        };
        let total = entry.slices.len();
        let is_file = std::fs::symlink_metadata(&target).map(|m| m.is_file()).unwrap_or(false);
        let mut status = FileStatus::Missing { found: 0, total };
        if is_file {
            let mut tick = |n: u64| {
                done_bytes += n;
                on(Event::Bytes { stage: Stage::Verify, done: done_bytes.min(total_bytes), total: total_bytes });
            };
            match check_file(&target, entry, slice_size, &mut buf, &mut tick) {
                Ok((true, _)) => status = FileStatus::Intact,
                Ok((false, good)) => {
                    for &k in &good {
                        sources[entry.first_slice + k] =
                            Some(SliceSource { path: target.clone(), offset: k as u64 * slice_size });
                    }
                    status = FileStatus::Damaged { found: good.len(), total };
                }
                Err(e) => on(Event::Note(format!("{}: could not read: {e}", entry.name))),
            }
            if entry.length == 0 && status != FileStatus::Intact {
                status = FileStatus::Damaged { found: 0, total: 0 };
            }
        }
        if status == FileStatus::Intact {
            for k in 0..total {
                sources[entry.first_slice + k] = Some(SliceSource { path: target.clone(), offset: k as u64 * slice_size });
            }
            used.insert(target.clone());
        } else {
            // A complete copy under another name (obfuscated posts).
            let found = others.iter().find(|p| {
                !targets.contains(*p)
                    && !used.contains(*p)
                    && std::fs::metadata(p).map(|m| m.is_file() && m.len() == entry.length).unwrap_or(false)
                    && md5_16k(p) == Some(entry.md5_16k)
                    && full_md5(p) == Some(entry.md5)
            });
            if let Some(p) = found {
                for k in 0..total {
                    sources[entry.first_slice + k] = Some(SliceSource { path: p.clone(), offset: k as u64 * slice_size });
                }
                used.insert(p.clone());
                status = FileStatus::Misnamed(p.clone());
            }
        }
        reports.push(FileReport { name: entry.name.clone(), length: entry.length, target: Some(target), status });
    }

    // Stray slices: aligned slices of other files (damaged renamed copies).
    let mut wanted: HashMap<u32, Vec<usize>> = HashMap::new();
    let mut owner: Vec<(usize, usize)> = Vec::with_capacity(sources.len());
    for (fi, e) in set.files.iter().enumerate() {
        for k in 0..e.slices.len() {
            owner.push((fi, k));
            if sources[e.first_slice + k].is_none() {
                wanted.entry(e.slices[k].crc).or_default().push(e.first_slice + k);
            }
        }
    }
    if !wanted.is_empty() {
        for p in others.iter().filter(|p| !targets.contains(*p) && !used.contains(*p)) {
            let Ok(mut f) = File::open(p) else { continue };
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            let mut off = 0u64;
            let mut hits = 0;
            while off < len && !wanted.is_empty() {
                if read_padded(&mut f, off, &mut buf).is_err() {
                    break;
                }
                if let Some(cands) = wanted.get(&crc32fast::hash(&buf)) {
                    let md5: [u8; 16] = Md5::digest(&buf).into();
                    if let Some(&g) = cands.iter().find(|&&g| {
                        let (fi, k) = owner[g];
                        sources[g].is_none() && set.files[fi].slices[k].md5 == md5
                    }) {
                        sources[g] = Some(SliceSource { path: p.clone(), offset: off });
                        hits += 1;
                    }
                }
                off += slice_size;
            }
            if hits > 0 {
                on(Event::Note(format!(
                    "found {hits} good slice(s) in {}",
                    p.file_name().unwrap_or_default().to_string_lossy()
                )));
            }
        }
        // Update the counts with the stray slices.
        for (fi, r) in reports.iter_mut().enumerate() {
            let e = &set.files[fi];
            let found = (0..e.slices.len()).filter(|k| sources[e.first_slice + k].is_some()).count();
            match &mut r.status {
                FileStatus::Damaged { found: f, .. } | FileStatus::Missing { found: f, .. } => *f = found,
                _ => {}
            }
        }
    }

    for r in &reports {
        on(Event::FileResult { name: r.name.clone(), status: r.status.clone() });
    }
    Verification { files: reports, sources, recovery_available: set.recovery.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_plain_relative() {
        let d = Path::new("/dl/job");
        assert_eq!(safe_target(d, "a.rar"), Some(d.join("a.rar")));
        assert_eq!(safe_target(d, "Sample/x.mkv"), Some(d.join("Sample").join("x.mkv")));
        assert_eq!(safe_target(d, "Sample\\x.mkv"), Some(d.join("Sample").join("x.mkv")));
        for bad in ["../x", "/etc/passwd", "a/../../b", "", ".", "C:x", "c:\\x"] {
            assert_eq!(safe_target(d, bad), None, "{bad}");
        }
    }
}
