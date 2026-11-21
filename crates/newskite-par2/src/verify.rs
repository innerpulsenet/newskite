//! Verification: which slices of a recovery set are present, and where.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use md5::{Digest, Md5};

use crate::packet::{FileEntry, RecoverySet};
use crate::pio::{read_at_padded, LastFile, Source};
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

fn md5_16k(path: &Path) -> Option<[u8; 16]> {
    let f = File::open(path).ok()?;
    let mut buf = vec![0u8; 16384];
    let n = read_at_padded(&f, 0, &mut buf).ok()?;
    Some(Md5::digest(&buf[..n]).into())
}

/// Threads for checking slices (MD5 is the cost, so more cores help until the disk is the
/// limit).
pub(crate) fn check_threads() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8)
}

/// `f` over `items` on up to `threads` threads; the results in item order. A panic in `f`
/// is passed on to the caller.
pub(crate) fn par_map<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let threads = threads.max(1).min(items.len());
    let mut out: Vec<(usize, R)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else { break done };
                        done.push((i, f(item)));
                    }
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))).collect()
    });
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, r)| r).collect()
}

/// Bytes per read when checking slices (whole slices, at least one), so a spinning disk
/// serving several threads still reads long runs.
const CHECK_READ: u64 = 8 << 20;
/// Upper bound for the read buffers of all checking threads together.
const CHECK_MEMORY: u64 = 256 << 20;

/// A file to check slice by slice against `entry`.
pub(crate) struct SliceCheck<'a> {
    pub(crate) file: Source<'a>,
    /// The file's length (slices at or past it are not read and count as bad).
    pub(crate) len: u64,
    pub(crate) entry: &'a FileEntry,
}

/// Progress of [`scan_slices`], reported on the calling thread.
pub(crate) enum CheckProgress {
    /// Started on file `index`.
    Started(usize),
    /// Bytes read.
    Bytes(u64),
}

/// Reads the first `count` slices of every `(file, count)` (zero-padded to the slice size
/// as PAR2 defines it) on up to `threads` threads, in runs of consecutive slices so one big
/// file goes as fast as many small ones, and maps each slice with `f(file, slice, bytes)`.
/// Returns per file the results in slice order up to the first read error, and that error.
pub(crate) fn scan_slices<R: Send>(
    files: &[(Source<'_>, usize)],
    slice_size: u64,
    threads: usize,
    progress: &mut dyn FnMut(CheckProgress),
    f: impl Fn(usize, usize, &[u8]) -> R + Sync,
) -> Vec<(Vec<R>, Option<io::Error>)> {
    let per_read = (CHECK_READ / slice_size).max(1) as usize;
    // Work items: (file, first slice, slice count), in file order.
    let mut items = Vec::new();
    for (j, &(_, count)) in files.iter().enumerate() {
        let mut first = 0;
        while first < count {
            let n = per_read.min(count - first);
            items.push((j, first, n));
            first += n;
        }
    }
    let buf_len = per_read * slice_size as usize;
    let threads = threads.max(1).min(items.len()).min((CHECK_MEMORY / buf_len as u64).max(1) as usize);
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    let mut found: Vec<(usize, io::Result<Vec<R>>)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let (tx, next, items, f) = (tx.clone(), &next, &items, &f);
                scope.spawn(move || {
                    let mut buf = vec![0u8; buf_len];
                    let mut last = LastFile::default();
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(j, first, n)) = items.get(i) else { break out };
                        if first == 0 {
                            let _ = tx.send(CheckProgress::Started(j));
                        }
                        let buf = &mut buf[..n * slice_size as usize];
                        let read = last.get(j, files[j].0).and_then(|f| read_at_padded(f, first as u64 * slice_size, buf));
                        let res = read.map(|got| {
                            let _ = tx.send(CheckProgress::Bytes(got as u64));
                            buf.chunks_exact(slice_size as usize).enumerate().map(|(k, s)| f(j, first + k, s)).collect()
                        });
                        out.push((i, res));
                    }
                })
            })
            .collect();
        drop(tx);
        for p in rx {
            progress(p);
        }
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
            .collect()
    });
    found.sort_by_key(|(i, _)| *i);
    let mut results: Vec<(Vec<R>, Option<io::Error>)> = files.iter().map(|_| (Vec::new(), None)).collect();
    for (i, res) in found {
        let (got, err) = &mut results[items[i].0];
        match res {
            Ok(r) if err.is_none() => got.extend(r),
            Ok(_) => {}
            Err(e) => {
                err.get_or_insert(e);
            }
        }
    }
    results
}

/// For each job, which of its entry's slices the file holds intact (MD5 of the padded
/// slice), checked on up to `threads` threads; a read error fails the job.
pub(crate) fn check_slices(
    jobs: &[SliceCheck<'_>],
    slice_size: u64,
    threads: usize,
    progress: &mut dyn FnMut(CheckProgress),
) -> Vec<io::Result<Vec<bool>>> {
    let files: Vec<(Source<'_>, usize)> = jobs
        .iter()
        .map(|j| (j.file, j.len.div_ceil(slice_size).min(j.entry.slices.len() as u64) as usize))
        .collect();
    let scanned = scan_slices(&files, slice_size, threads, progress, |j, k, slice| {
        Md5::digest(slice).as_slice() == jobs[j].entry.slices[k].md5
    });
    jobs.iter()
        .zip(scanned)
        .map(|(job, (mut good, err))| match err {
            Some(e) => Err(e),
            None => {
                good.resize(job.entry.slices.len(), false);
                Ok(good)
            }
        })
        .collect()
}

/// Whether a file of `len` bytes whose slices check out as `good` is `entry` exactly.
pub(crate) fn is_complete(entry: &FileEntry, len: u64, good: &[bool]) -> bool {
    // Equal slices (each MD5-checked) and equal length make equal content; an empty file
    // has no slices, so its whole-file MD5 decides.
    len == entry.length && good.iter().all(|&g| g) && (entry.length > 0 || Md5::digest([]).as_slice() == entry.md5)
}

/// Verifies `set` against the files in `dir`. `others` are further candidate files (all
/// regular files of the folder that are not PAR2 files); renamed copies and stray good
/// slices are looked for in them.
///
/// Every slice is checked by its MD5 (a file is intact when all its slices are and its
/// length is right), with the slices of all files spread over several threads.
pub fn verify(set: &RecoverySet, dir: &Path, others: &[PathBuf], on: &mut dyn FnMut(Event)) -> Verification {
    let slice_size = set.slice_size;
    let count = set.files.len();
    let mut sources: Vec<Option<SliceSource>> = vec![None; set.slice_count()];
    let total_bytes = set.data_bytes();
    let mut done_bytes = 0u64;
    let targets: BTreeSet<PathBuf> = set.files.iter().filter_map(|f| safe_target(dir, &f.name)).collect();
    let mut used: BTreeSet<PathBuf> = BTreeSet::new();
    let threads = check_threads();
    let mut reports: Vec<FileReport> = set
        .files
        .iter()
        .map(|e| {
            let target = safe_target(dir, &e.name);
            let status = match target {
                Some(_) => FileStatus::Missing { found: 0, total: e.slices.len() },
                None => FileStatus::UnsafeName,
            };
            FileReport { name: e.name.clone(), length: e.length, target, status }
        })
        .collect();

    // The files under their own names, all checked at once. Every file gets a `File` event:
    // those present when their check starts, the others right away.
    let mut present = vec![false; count];
    let mut opened = Vec::new();
    for (i, (entry, r)) in set.files.iter().zip(&reports).enumerate() {
        let meta = r.target.as_ref().and_then(|t| std::fs::symlink_metadata(t).ok()).filter(|m| m.is_file());
        match (&r.target, meta) {
            (Some(target), Some(m)) => {
                present[i] = true;
                opened.push((i, target.clone(), m.len()));
            }
            _ => on(Event::File { stage: Stage::Verify, index: i, count, name: entry.name.clone() }),
        }
    }
    let mut tick = |p: CheckProgress, files: &[usize], on: &mut dyn FnMut(Event)| match p {
        CheckProgress::Started(j) => {
            let i = files[j];
            on(Event::File { stage: Stage::Verify, index: i, count, name: set.files[i].name.clone() });
        }
        CheckProgress::Bytes(n) => {
            done_bytes += n;
            on(Event::Bytes { stage: Stage::Verify, done: done_bytes.min(total_bytes), total: total_bytes });
        }
    };
    let jobs: Vec<SliceCheck<'_>> = opened
        .iter()
        .map(|(i, target, len)| SliceCheck { file: Source::Path(target), len: *len, entry: &set.files[*i] })
        .collect();
    let of: Vec<usize> = opened.iter().map(|o| o.0).collect();
    let checked = check_slices(&jobs, slice_size, threads, &mut |p| tick(p, &of, on));
    for ((i, target, len), res) in opened.iter().zip(checked) {
        let entry = &set.files[*i];
        match res {
            Ok(good) => {
                for k in (0..good.len()).filter(|&k| good[k]) {
                    sources[entry.first_slice + k] = Some(SliceSource { path: target.clone(), offset: k as u64 * slice_size });
                }
                reports[*i].status = if is_complete(entry, *len, &good) {
                    used.insert(target.clone());
                    FileStatus::Intact
                } else {
                    FileStatus::Damaged { found: good.iter().filter(|&&g| g).count(), total: entry.slices.len() }
                };
            }
            Err(e) => on(Event::Note(format!("{}: could not read: {e}", entry.name))),
        }
    }
    for (i, entry) in set.files.iter().enumerate() {
        if present[i] && entry.length == 0 && reports[i].status != FileStatus::Intact {
            reports[i].status = FileStatus::Damaged { found: 0, total: 0 };
        }
    }

    // Complete copies under other names (obfuscated posts): the candidates of the right
    // size and first-16k hash are checked at once, then picked in file order.
    let sizes: Vec<Option<u64>> = others
        .iter()
        .map(|p| std::fs::metadata(p).ok().filter(|m| m.is_file()).map(|m| m.len()))
        .collect();
    let mut head: HashMap<usize, Option<[u8; 16]>> = HashMap::new();
    let mut candidates = Vec::new();
    for (i, entry) in set.files.iter().enumerate() {
        if matches!(reports[i].status, FileStatus::Intact | FileStatus::UnsafeName) {
            continue;
        }
        for (o, p) in others.iter().enumerate() {
            if sizes[o] != Some(entry.length) || targets.contains(p) || used.contains(p) {
                continue;
            }
            if *head.entry(o).or_insert_with(|| md5_16k(p)) != Some(entry.md5_16k) {
                continue;
            }
            candidates.push((i, p));
        }
    }
    // Each slice's CRC32 is kept with its MD5, so the stray-slice search below needs no
    // second read of the damaged copies.
    let files: Vec<(Source<'_>, usize)> = candidates.iter().map(|(i, p)| (Source::Path(p), set.files[*i].slices.len())).collect();
    let of: Vec<usize> = candidates.iter().map(|c| c.0).collect();
    let scanned = scan_slices(&files, slice_size, threads, &mut |p| tick(p, &of, on), |_, _, slice| {
        (crc32fast::hash(slice), <[u8; 16]>::from(Md5::digest(slice)))
    });
    let mut hashed: HashMap<&PathBuf, Vec<(u32, [u8; 16])>> = HashMap::new();
    let mut partial = Vec::new();
    for ((i, p), (hashes, err)) in candidates.iter().zip(scanned) {
        let entry = &set.files[*i];
        if err.is_some() {
            continue;
        }
        let mut good: Vec<bool> = hashes.iter().zip(&entry.slices).map(|(h, s)| h.1 == s.md5).collect();
        good.resize(entry.slices.len(), false);
        hashed.entry(*p).or_insert(hashes);
        if !is_complete(entry, entry.length, &good) {
            partial.push((*i, *p, good));
            continue;
        }
        if matches!(reports[*i].status, FileStatus::Misnamed(_)) || used.contains(*p) {
            continue;
        }
        for k in 0..entry.slices.len() {
            sources[entry.first_slice + k] = Some(SliceSource { path: (*p).clone(), offset: k as u64 * slice_size });
        }
        used.insert((*p).clone());
        reports[*i].status = FileStatus::Misnamed((*p).clone());
    }
    // Damaged copies under other names: their good slices are known from the check above.
    for (i, p, good) in partial {
        let entry = &set.files[i];
        if used.contains(p) {
            continue;
        }
        for k in (0..good.len()).filter(|&k| good[k]) {
            sources[entry.first_slice + k]
                .get_or_insert_with(|| SliceSource { path: p.clone(), offset: k as u64 * slice_size });
        }
    }

    // Stray slices: aligned slices of other files (damaged renamed copies). Every slice is
    // matched by CRC32 first; the MD5 is computed only when a still-missing slice has that
    // CRC. The scan runs on several threads, the matches are taken in file order.
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
        let strays: Vec<(&PathBuf, u64)> = others
            .iter()
            .filter(|p| !targets.contains(*p) && !used.contains(*p))
            .filter_map(|p| Some((p, std::fs::metadata(p).ok()?.len())))
            .collect();
        // Damaged copies checked above were hashed already; only the others are read.
        let files: Vec<(Source<'_>, usize)> = strays
            .iter()
            .filter(|(p, _)| !hashed.contains_key(p))
            .map(|(p, len)| (Source::Path(p), len.div_ceil(slice_size) as usize))
            .collect();
        let snapshot = &sources;
        let mut scanned = scan_slices(&files, slice_size, threads, &mut |_| {}, |_, _, slice| {
            let crc = crc32fast::hash(slice);
            let missing = wanted.get(&crc)?.iter().any(|&g| snapshot[g].is_none());
            missing.then(|| (crc, <[u8; 16]>::from(Md5::digest(slice))))
        })
        .into_iter();
        for (p, _) in &strays {
            let hashes: Vec<Option<(u32, [u8; 16])>> = match hashed.remove(p) {
                Some(h) => h.into_iter().map(Some).collect(),
                None => scanned.next().map(|(h, _)| h).unwrap_or_default(),
            };
            let mut hits = 0;
            for (k, hash) in hashes.into_iter().enumerate() {
                let Some((crc, md5)) = hash else { continue };
                let Some(gs) = wanted.get(&crc) else { continue };
                if let Some(&g) = gs.iter().find(|&&g| {
                    let (fi, k) = owner[g];
                    sources[g].is_none() && set.files[fi].slices[k].md5 == md5
                }) {
                    sources[g] = Some(SliceSource { path: (*p).clone(), offset: k as u64 * slice_size });
                    hits += 1;
                }
            }
            if hits > 0 {
                on(Event::Note(format!(
                    "found {hits} good slice(s) in {}",
                    p.file_name().unwrap_or_default().to_string_lossy()
                )));
            }
        }
    }
    // Update the counts with the slices found in other files.
    for (fi, r) in reports.iter_mut().enumerate() {
        let e = &set.files[fi];
        let found = (0..e.slices.len()).filter(|k| sources[e.first_slice + k].is_some()).count();
        match &mut r.status {
            FileStatus::Damaged { found: f, .. } | FileStatus::Missing { found: f, .. } => *f = found,
            _ => {}
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
