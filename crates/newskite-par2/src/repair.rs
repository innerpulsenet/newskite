//! Repair: rebuilds missing input slices from recovery slices and writes the damaged or
//! missing files.
//!
//! With `k` missing slices `D_m` and `k` recovery slices `R_j` (exponent `e_j`), PAR2
//! defines `R_j = sum_i c_i^e_j * D_i` over every input slice `i`. Moving the known slices
//! to the left gives `R'_j = sum_m A[j][m] * D_m` with `A[j][m] = c_m^e_j`, so
//! `D = A^-1 * R'`. The slices are processed in byte ranges ("passes") so memory stays
//! bounded by [`RepairOptions::memory_limit`] whatever the slice size and block count.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};

use crate::gf::{self, MulTable};
use crate::packet::{recovery_intact, RecoveryRef, RecoverySet};
use crate::verify::{read_padded, FileStatus, Verification};
use crate::{Event, Par2Error, Stage};

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
/// replaced unless every rebuilt file checks out against its MD5.
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
    let result = rebuild_files(set, ver, opts, &missing, &chosen, &inverse, &consts, &mut rebuilds, on);
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
            let Some(r) = pool.next() else {
                return Err(Par2Error::NotEnoughRecovery { needed: k, available: checked });
            };
            on(Event::File { stage: Stage::Prepare, index: chosen.len(), count: k, name: format!("recovery slice {}", r.exponent) });
            match recovery_intact(r) {
                Ok(true) => {
                    chosen.push(r.clone());
                    checked += 1;
                }
                Ok(false) => on(Event::Note(format!("recovery slice {} is damaged; skipping it", r.exponent))),
                Err(e) => on(Event::Note(format!("recovery slice {} unreadable ({e}); skipping it", r.exponent))),
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

#[allow(clippy::too_many_arguments)]
fn rebuild_files(
    set: &RecoverySet,
    ver: &Verification,
    opts: &RepairOptions,
    missing: &[usize],
    chosen: &[RecoveryRef],
    inverse: &[u16],
    consts: &[u16],
    rebuilds: &mut [Rebuild],
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
    let slots: Vec<usize> = rebuilds.iter().map(|r| r.file).collect();
    let slot_of = |fi: usize| slots.iter().position(|&f| f == fi);

    // Known-good slices into the copies.
    let mut buf = vec![0u8; ss as usize];
    for r in rebuilds.iter_mut() {
        let e = &set.files[r.file];
        for s in 0..e.slices.len() {
            if let Some(src) = &ver.sources[e.first_slice + s] {
                let n = e.slice_len(s, ss) as usize;
                let mut f = File::open(&src.path)?;
                read_padded(&mut f, src.offset, &mut buf[..n])?;
                r.handle.seek(SeekFrom::Start(s as u64 * ss))?;
                r.handle.write_all(&buf[..n])?;
            }
        }
        r.handle.set_len(e.length)?;
    }
    drop(buf);

    let k = missing.len();
    if k > 0 {
        let chunk = ((opts.memory_limit / (k + 2)).max(4096) as u64).min(ss) / 4 * 4;
        let chunk = chunk.max(4);
        let passes = ss.div_ceil(chunk);
        let available: Vec<usize> = (0..ver.sources.len()).filter(|&i| ver.sources[i].is_some()).collect();
        let work_total = passes * (available.len() as u64 + k as u64);
        let mut work_done = 0u64;
        let threads = opts.threads.max(1);
        for pass in 0..passes {
            let c0 = pass * chunk;
            let len = chunk.min(ss - c0) as usize;
            // R'_j, starting from the recovery data.
            let mut acc: Vec<Vec<u8>> = Vec::with_capacity(k);
            for r in chosen {
                let mut b = vec![0u8; len];
                let mut f = File::open(&r.path)?;
                read_padded(&mut f, r.offset + c0, &mut b)?;
                acc.push(b);
            }
            // Subtract every known slice: R'_j ^= c_i^e_j * D_i. Inputs are read in
            // batches and the recovery rows split across threads.
            let batch_cap = (opts.memory_limit / 4 / len.max(1)).clamp(1, 64);
            let mut idx = 0;
            while idx < available.len() {
                let batch: Vec<usize> = available[idx..(idx + batch_cap).min(available.len())].to_vec();
                idx += batch.len();
                let mut data = Vec::with_capacity(batch.len());
                for &g in &batch {
                    let (fi, s) = owner[g];
                    let src = ver.sources[g].as_ref().expect("available");
                    let have = set.files[fi].slice_len(s, ss).saturating_sub(c0).min(len as u64) as usize;
                    let mut b = vec![0u8; len];
                    let mut f = File::open(&src.path)?;
                    read_padded(&mut f, src.offset + c0, &mut b[..have])?;
                    data.push(b);
                }
                let per = k.div_ceil(threads);
                std::thread::scope(|scope| {
                    for (part, rows) in acc.chunks_mut(per).enumerate() {
                        let data = &data;
                        let batch = &batch;
                        scope.spawn(move || {
                            for (o, row) in rows.iter_mut().enumerate() {
                                let e = chosen[part * per + o].exponent;
                                for (b, &g) in data.iter().zip(batch) {
                                    let t = MulTable::new(gf::pow(consts[g], e));
                                    gf::mul_acc(row, b, &t);
                                }
                            }
                        });
                    }
                });
                work_done += batch.len() as u64;
                on(Event::Bytes { stage: Stage::Repair, done: work_done, total: work_total });
            }
            // D_m = sum_j inverse[m][j] * R'_j, written into the owning copy.
            let mut tmp = vec![0u8; len];
            for (m, &g) in missing.iter().enumerate() {
                tmp.fill(0);
                for (j, row) in acc.iter().enumerate() {
                    let c = inverse[m * k + j];
                    if c != 0 {
                        gf::mul_acc(&mut tmp, row, &MulTable::new(c));
                    }
                }
                let (fi, s) = owner[g];
                let have = set.files[fi].slice_len(s, ss).saturating_sub(c0).min(len as u64) as usize;
                if have > 0 {
                    let slot = slot_of(fi).ok_or_else(|| Par2Error::Internal("rebuilt slice has no file".into()))?;
                    let h = &mut rebuilds[slot].handle;
                    h.seek(SeekFrom::Start(s as u64 * ss + c0))?;
                    h.write_all(&tmp[..have])?;
                }
                work_done += 1;
                on(Event::Bytes { stage: Stage::Repair, done: work_done, total: work_total });
            }
        }
    }

    // Check every copy before anything is replaced.
    for r in rebuilds.iter_mut() {
        let e = &set.files[r.file];
        r.handle.flush()?;
        r.handle.sync_all()?;
        r.handle.seek(SeekFrom::Start(0))?;
        let mut h = Md5::new();
        let mut b = vec![0u8; 1 << 20];
        loop {
            let n = r.handle.read(&mut b)?;
            if n == 0 {
                break;
            }
            h.update(&b[..n]);
        }
        if h.finalize().as_slice() != e.md5 {
            return Err(Par2Error::VerifyAfterRepair(e.name.clone()));
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
