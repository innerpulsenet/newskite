//! Repair: rebuilds missing input slices from recovery slices and writes the damaged or
//! missing files.
//!
//! With `k` missing slices `D_m` and `k` recovery slices `R_j` (exponent `e_j`), PAR2
//! defines `R_j = sum_i c_i^e_j * D_i` over every input slice `i`. Moving the known slices
//! to the left gives `R'_j = sum_m A[j][m] * D_m` with `A[j][m] = c_m^e_j`, so
//! `D = A^-1 * R'`. The slices are processed in byte ranges ("passes") so memory stays
//! bounded by [`RepairOptions::memory_limit`] whatever the slice size and block count.
//!
//! Within a pass every thread owns one byte range of all `k` rows, reads its range of the
//! recovery and input slices itself (positional reads on shared handles) and writes its
//! range of the rebuilt slices, so all threads work whatever `k` is and no thread waits
//! for another. Inputs are taken in groups and the rows updated in small sub-blocks, so
//! the data being combined stays in cache while a group is applied.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use crate::gf::{self, Kernel, MulTable};
use crate::packet::{recovery_intact, RecoveryRef, RecoverySet};
use crate::pio::{read_at_padded, write_all_at, LastFile, Source};
use crate::verify::{check_slices, check_threads, is_complete, par_map, FileStatus, SliceCheck, Verification};
use crate::{Event, Par2Error, Stage};

/// Input slices read per group: each sub-block of an accumulator row is updated from this
/// many inputs while it is in cache (their sub-blocks fit the first-level cache too).
const GROUP: usize = 16;
/// A thread gets at least this many bytes of a pass (tiny slices use fewer threads).
const MIN_THREAD_BYTES: usize = 16 << 10;

/// Tuning for [`repair`].
#[derive(Clone, Debug)]
pub struct RepairOptions {
    /// Upper bound for the working buffers, in bytes.
    pub memory_limit: usize,
    /// Worker threads for the arithmetic (at least 1).
    pub threads: usize,
}

impl Default for RepairOptions {
    fn default() -> Self {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8);
        Self { memory_limit: 256 << 20, threads }
    }
}

/// What [`repair`] changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepairOutcome {
    /// Files rebuilt from recovery data (names as in the set).
    pub rebuilt: Vec<String>,
    /// Complete copies moved to their real names: (from, to).
    pub renamed: Vec<(PathBuf, PathBuf)>,
    /// Damaged originals kept as `name.1` (`.2`, ...) before the good copy took the name.
    pub backups: Vec<PathBuf>,
    /// Slices reconstructed.
    pub slices_rebuilt: usize,
}

struct Rebuild {
    file: usize,
    target: PathBuf,
    temp: PathBuf,
    handle: File,
}

/// Repairs `set` using `ver` (from [`crate::verify`] on the same folder). Nothing is
/// replaced unless every slice of every rebuilt file checks out against its MD5.
pub fn repair(
    set: &RecoverySet,
    ver: &Verification,
    opts: &RepairOptions,
    on: &mut dyn FnMut(Event),
) -> Result<RepairOutcome, Par2Error> {
    if ver.files.iter().any(|f| f.status == FileStatus::UnsafeName) {
        return Err(Par2Error::UnsafeName);
    }
    let missing: Vec<usize> = (0..ver.sources.len()).filter(|&i| ver.sources[i].is_none()).collect();
    let k = missing.len();
    if k > ver.recovery_available {
        return Err(Par2Error::NotEnoughRecovery { needed: k, available: ver.recovery_available });
    }
    let consts = gf::input_constants(set.slice_count());
    let mut out = RepairOutcome { slices_rebuilt: k, ..Default::default() };

    // Recovery slices: checksum-checked, and a combination whose matrix inverts.
    let (chosen, inverse) = if k > 0 { choose_recovery(set, &missing, &consts, on)? } else { (Vec::new(), Vec::new()) };

    // Files to rebuild into temporary copies.
    let mut rebuilds = Vec::new();
    for (fi, r) in ver.files.iter().enumerate() {
        if !matches!(r.status, FileStatus::Damaged { .. } | FileStatus::Missing { .. }) {
            continue;
        }
        let target = r.target.clone().ok_or(Par2Error::UnsafeName)?;
        let base = target.parent().ok_or(Par2Error::UnsafeName)?;
        make_dirs(base)?;
        let temp = target.with_file_name(format!(
            ".{}.nkrepair",
            target.file_name().unwrap_or_default().to_string_lossy()
        ));
        if std::fs::symlink_metadata(&temp).is_ok_and(|m| m.is_file()) {
            std::fs::remove_file(&temp)?;
        }
        let handle = OpenOptions::new().read(true).write(true).create_new(true).open(&temp)?;
        rebuilds.push(Rebuild { file: fi, target, temp, handle });
    }
    let result = rebuild_files(set, ver, opts, &missing, &chosen, &inverse, &consts, &rebuilds, on);
    if let Err(e) = result {
        for r in &rebuilds {
            let _ = std::fs::remove_file(&r.temp);
        }
        return Err(e);
    }

    // Every copy checked out: move them into place.
    for r in rebuilds {
        drop(r.handle);
        if std::fs::symlink_metadata(&r.target).is_ok() {
            out.backups.push(backup(&r.target)?);
        }
        std::fs::rename(&r.temp, &r.target)?;
        on(Event::Note(format!("rebuilt {}", set.files[r.file].name)));
        out.rebuilt.push(set.files[r.file].name.clone());
    }
    for r in &ver.files {
        if let (FileStatus::Misnamed(from), Some(to)) = (&r.status, &r.target) {
            make_dirs(to.parent().ok_or(Par2Error::UnsafeName)?)?;
            if std::fs::symlink_metadata(to).is_ok() {
                out.backups.push(backup(to)?);
            }
            std::fs::rename(from, to)?;
            on(Event::Note(format!(
                "renamed {} to {}",
                from.file_name().unwrap_or_default().to_string_lossy(),
                r.name
            )));
            out.renamed.push((from.clone(), to.clone()));
        }
    }
    Ok(out)
}

/// Picks `k` intact recovery slices whose matrix is invertible. Returns them with
/// `A^-1` (row-major, `k`x`k`, rows = missing slices).
fn choose_recovery(
    set: &RecoverySet,
    missing: &[usize],
    consts: &[u16],
    on: &mut dyn FnMut(Event),
) -> Result<(Vec<RecoveryRef>, Vec<u16>), Par2Error> {
    let k = missing.len();
    let mut chosen: Vec<RecoveryRef> = Vec::with_capacity(k);
    let mut pool = set.recovery.iter();
    let mut checked = 0usize;
    loop {
        while chosen.len() < k {
            // The next candidates, as many as are still needed, are checked at once and
            // taken in order.
            let batch: Vec<&RecoveryRef> = pool.by_ref().take(k - chosen.len()).collect();
            if batch.is_empty() {
                return Err(Par2Error::NotEnoughRecovery { needed: k, available: checked });
            }
            for (n, r) in batch.iter().enumerate() {
                let name = format!("recovery slice {}", r.exponent);
                on(Event::File { stage: Stage::Prepare, index: chosen.len() + n, count: k, name });
            }
            let intact = par_map(&batch, check_threads(), |r| recovery_intact(r));
            for (r, res) in batch.into_iter().zip(intact) {
                match res {
                    Ok(true) => {
                        chosen.push(r.clone());
                        checked += 1;
                    }
                    Ok(false) => on(Event::Note(format!("recovery slice {} is damaged; skipping it", r.exponent))),
                    Err(e) => on(Event::Note(format!("recovery slice {} unreadable ({e}); skipping it", r.exponent))),
                }
            }
        }
        let a: Vec<u16> = chosen
            .iter()
            .flat_map(|r| missing.iter().map(move |&m| gf::pow(consts[m], r.exponent)))
            .collect();
        match gf::invert(&a, k) {
            // inverse(A) maps R' (rows j) to D (rows m).
            Some(inv) => return Ok((chosen, inv)),
            None => {
                // Rare: drop the newest pick and try the next one.
                on(Event::Note("recovery matrix is singular; trying another recovery slice".into()));
                chosen.pop();
            }
        }
    }
}

/// Every file slices are read from, numbered. Threads open them as they go (keeping the
/// last one open), so a set of thousands of files never holds thousands of descriptors.
#[derive(Default)]
struct Sources {
    paths: Vec<PathBuf>,
    index: HashMap<PathBuf, usize>,
}

impl Sources {
    fn id(&mut self, path: &Path) -> usize {
        if let Some(&i) = self.index.get(path) {
            return i;
        }
        self.paths.push(path.to_path_buf());
        self.index.insert(path.to_path_buf(), self.paths.len() - 1);
        self.paths.len() - 1
    }
}

/// One known input slice of a pass: global index, source file, offset of the slice in
/// it and the slice's data bytes (the rest of the slice is zero padding).
struct Input {
    global: usize,
    file: usize,
    offset: u64,
    len: u64,
}

/// What every thread of a pass shares.
struct Pass<'a> {
    kernel: Kernel,
    paths: &'a [PathBuf],
    /// Chosen recovery slices: (source file, offset of the slice data) and exponent.
    recovery: &'a [(usize, u64, u32)],
    inputs: &'a [Input],
    consts: &'a [u16],
    inverse: &'a [u16],
    /// Per missing slice: the copy it belongs to, its offset there and its data bytes.
    targets: &'a [(&'a File, u64, u64)],
    /// Offset of this pass in the slices.
    c0: u64,
}

impl Pass<'_> {
    /// One thread's share: `rows` are the `k` accumulator rows restricted to the range
    /// starting `r0` bytes into the pass. Sends the bytes of work done to `progress`.
    fn run(&self, rows: &mut [&mut [u8]], r0: usize, progress: &mpsc::Sender<u64>) -> io::Result<()> {
        let k = rows.len();
        let Some(rlen) = rows.first().map(|r| r.len()) else { return Ok(()) };
        let pos = self.c0 + r0 as u64;
        let (mut recovery_file, mut input_file) = (LastFile::default(), LastFile::default());
        // R'_j, starting from the recovery data.
        for (row, &(h, off, _)) in rows.iter_mut().zip(self.recovery) {
            read_at_padded(recovery_file.get(h, Source::Path(&self.paths[h]))?, off + pos, row)?;
        }
        drop(recovery_file);
        // Subtract every known slice: R'_j ^= c_i^e_j * D_i.
        // Rows are updated a sub-block at a time: a sub-block of one row stays in the
        // first-level cache while the whole group is added to it.
        let sub = self.kernel.block().min(rlen);
        let mut buf = vec![0u8; GROUP.min(self.inputs.len()).max(1) * rlen];
        let mut tables = Vec::with_capacity(k * GROUP);
        for group in self.inputs.chunks(GROUP) {
            let n = group.len();
            for (input, b) in group.iter().zip(buf.chunks_exact_mut(rlen)) {
                let have = input.len.saturating_sub(pos).min(rlen as u64) as usize;
                let f = input_file.get(input.file, Source::Path(&self.paths[input.file]))?;
                read_at_padded(f, input.offset + pos, &mut b[..have])?;
                b[have..].fill(0);
            }
            tables.clear();
            for &(_, _, e) in self.recovery {
                tables.extend(group.iter().map(|input| MulTable::new(gf::pow(self.consts[input.global], e))));
            }
            let mut s0 = 0;
            while s0 < rlen {
                let s1 = (s0 + sub).min(rlen);
                for (j, row) in rows.iter_mut().enumerate() {
                    for (i, b) in buf.chunks_exact(rlen).take(n).enumerate() {
                        self.kernel.mul_acc(&mut row[s0..s1], &b[s0..s1], &tables[j * n + i]);
                    }
                }
                s0 = s1;
            }
            let _ = progress.send((n * rlen) as u64);
        }
        // D_m = sum_j inverse[m][j] * R'_j, written into the owning copy. The sum is built
        // a sub-block at a time, so that sub-block of D_m stays in cache over all k rows.
        let out = &mut buf[..rlen];
        for (m, &(file, start, data_len)) in self.targets.iter().enumerate() {
            tables.clear();
            tables.extend(self.inverse[m * k..(m + 1) * k].iter().map(|&c| MulTable::new(c)));
            out.fill(0);
            let mut s0 = 0;
            while s0 < rlen {
                let s1 = (s0 + sub).min(rlen);
                for (row, t) in rows.iter().zip(&tables) {
                    self.kernel.mul_acc(&mut out[s0..s1], &row[s0..s1], t);
                }
                s0 = s1;
            }
            let have = data_len.saturating_sub(pos).min(rlen as u64) as usize;
            if have > 0 {
                write_all_at(file, &out[..have], start + pos)?;
            }
            let _ = progress.send(rlen as u64);
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn rebuild_files(
    set: &RecoverySet,
    ver: &Verification,
    opts: &RepairOptions,
    missing: &[usize],
    chosen: &[RecoveryRef],
    inverse: &[u16],
    consts: &[u16],
    rebuilds: &[Rebuild],
    on: &mut dyn FnMut(Event),
) -> Result<(), Par2Error> {
    let ss = set.slice_size;
    // Global slice -> (file index, slice within file).
    let mut owner = Vec::with_capacity(ver.sources.len());
    for (fi, f) in set.files.iter().enumerate() {
        for s in 0..f.slices.len() {
            owner.push((fi, s));
        }
    }
    let mut sources = Sources::default();
    let source: Vec<Option<(usize, u64)>> =
        ver.sources.iter().map(|s| s.as_ref().map(|s| (sources.id(&s.path), s.offset))).collect();

    // Known-good slices into the copies.
    copy_known(set, &source, &sources.paths, rebuilds)?;

    let k = missing.len();
    if k > 0 {
        let recovery: Vec<(usize, u64, u32)> = chosen.iter().map(|r| (sources.id(&r.path), r.offset, r.exponent)).collect();
        let targets: Vec<(&File, u64, u64)> = missing
            .iter()
            .map(|&g| {
                let (fi, s) = owner[g];
                let r = rebuilds
                    .iter()
                    .find(|r| r.file == fi)
                    .ok_or_else(|| Par2Error::Internal("rebuilt slice has no file".into()))?;
                Ok((&r.handle, s as u64 * ss, set.files[fi].slice_len(s, ss)))
            })
            .collect::<Result<_, Par2Error>>()?;
        let inputs: Vec<Input> = source
            .iter()
            .enumerate()
            .filter_map(|(g, s)| {
                let (fi, sl) = owner[g];
                s.map(|(file, offset)| Input { global: g, file, offset, len: set.files[fi].slice_len(sl, ss) })
            })
            .collect();

        // Memory: k accumulator rows plus a group of inputs and one output row.
        let chunk = ((opts.memory_limit / (k + GROUP + 1)).max(4096) as u64).min(ss);
        let chunk = if chunk < ss { chunk / 64 * 64 } else { ss };
        let passes = ss.div_ceil(chunk);
        let work_total = passes * (inputs.len() as u64 + k as u64);
        let mut work_done = 0u64;
        let mut acc = vec![0u8; k * chunk as usize];
        for pass in 0..passes {
            let c0 = pass * chunk;
            let len = chunk.min(ss - c0) as usize;
            let threads = opts.threads.max(1).min(len.div_ceil(MIN_THREAD_BYTES));
            let part = len.div_ceil(threads).next_multiple_of(64);
            // Each thread's range of every row.
            let mut shares: Vec<Vec<&mut [u8]>> = (0..len.div_ceil(part)).map(|_| Vec::with_capacity(k)).collect();
            for row in acc[..k * len].chunks_exact_mut(len) {
                for (share, piece) in shares.iter_mut().zip(row.chunks_mut(part)) {
                    share.push(piece);
                }
            }
            let ctx = Pass {
                kernel: Kernel::best(),
                paths: &sources.paths,
                recovery: &recovery,
                inputs: &inputs,
                consts,
                inverse,
                targets: &targets,
                c0,
            };
            let (tx, rx) = mpsc::channel::<u64>();
            let results: Vec<io::Result<()>> = std::thread::scope(|scope| {
                let workers: Vec<_> = shares
                    .into_iter()
                    .enumerate()
                    .map(|(t, mut rows)| {
                        let (ctx, tx) = (&ctx, tx.clone());
                        scope.spawn(move || ctx.run(&mut rows, t * part, &tx))
                    })
                    .collect();
                drop(tx);
                let mut bytes = 0u64;
                let mut last = work_done;
                for n in rx {
                    bytes += n;
                    let done = work_done + bytes / len as u64;
                    if done != last {
                        last = done;
                        on(Event::Bytes { stage: Stage::Repair, done, total: work_total });
                    }
                }
                workers
                    .into_iter()
                    .map(|w| w.join().unwrap_or_else(|_| Err(io::Error::other("repair thread panicked"))))
                    .collect()
            });
            results.into_iter().collect::<io::Result<()>>()?;
            work_done += inputs.len() as u64 + k as u64;
        }
    }

    check_copies(set, rebuilds)
}

/// Copies the known-good slices into the copies; consecutive slices stored one after the
/// other in the same file go in one run.
fn copy_known(set: &RecoverySet, source: &[Option<(usize, u64)>], paths: &[PathBuf], rebuilds: &[Rebuild]) -> io::Result<()> {
    let ss = set.slice_size;
    let mut open: Option<(usize, File)> = None;
    for r in rebuilds {
        let e = &set.files[r.file];
        let mut s = 0;
        while s < e.slices.len() {
            let Some((h, off)) = source[e.first_slice + s] else {
                s += 1;
                continue;
            };
            let mut end = s + 1;
            while end < e.slices.len() && source[e.first_slice + end] == Some((h, off + (end - s) as u64 * ss)) {
                end += 1;
            }
            let start = s as u64 * ss;
            let total = (end as u64 * ss).min(e.length) - start;
            if open.as_ref().is_none_or(|(k, _)| *k != h) {
                open = Some((h, File::open(&paths[h])?));
            }
            let mut from = &open.as_ref().expect("opened above").1;
            let mut to = &r.handle;
            from.seek(SeekFrom::Start(off))?;
            to.seek(SeekFrom::Start(start))?;
            io::copy(&mut from.take(total), &mut to)?;
            s = end;
        }
        r.handle.set_len(e.length)?;
    }
    Ok(())
}

/// Syncs every copy, then checks each one slice by slice (all slices of all copies spread
/// over several threads) before anything is replaced.
fn check_copies(set: &RecoverySet, rebuilds: &[Rebuild]) -> Result<(), Par2Error> {
    par_map(rebuilds, check_threads(), |r| r.handle.sync_all()).into_iter().collect::<io::Result<()>>()?;
    let mut jobs = Vec::with_capacity(rebuilds.len());
    for r in rebuilds {
        jobs.push(SliceCheck { file: Source::Open(&r.handle), len: r.handle.metadata()?.len(), entry: &set.files[r.file] });
    }
    let checked = check_slices(&jobs, set.slice_size, check_threads(), &mut |_| {});
    for (job, res) in jobs.iter().zip(checked) {
        if !is_complete(job.entry, job.len, &res?) {
            return Err(Par2Error::VerifyAfterRepair(job.entry.name.clone()));
        }
    }
    Ok(())
}

/// Renames `path` to the first free `path.1`, `path.2`, ... and returns the new name.
fn backup(path: &Path) -> io::Result<PathBuf> {
    for n in 1..1000 {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".{n}"));
        let b = PathBuf::from(name);
        if std::fs::symlink_metadata(&b).is_err() {
            std::fs::rename(path, &b)?;
            return Ok(b);
        }
    }
    Err(io::Error::other(format!("no free backup name for {}", path.display())))
}

/// Creates `dir` and missing parents, refusing to pass through anything that is not a
/// real folder (a link could lead outside the download).
fn make_dirs(dir: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(io::Error::other(format!("{} is in the way", dir.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(p) = dir.parent() {
                make_dirs(p)?;
            }
            std::fs::create_dir(dir)
        }
        Err(e) => Err(e),
    }
}
