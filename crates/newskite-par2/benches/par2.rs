//! Verify and repair throughput on a synthetic set: `cargo bench -p newskite-par2 --bench par2`.
//!
//! Builds a set of four files (`TOTAL_MIB`, default 256) with `SLICE_KIB` slices (default
//! 768, a common posting size) and enough recovery slices, then times, per round:
//! verifying the intact set (MD5 + CRC32 of every slice), verifying a damaged copy (one
//! file deleted, `CORRUPT` slices of another overwritten) and repairing it. The files
//! live in a temporary folder (`BENCH_DIR` overrides it), so after the first round they
//! are in the page cache and the numbers are the CPU path, not the disk. `THREADS` sets
//! [`RepairOptions::threads`]; `ROUNDS` the repetitions (the best round is reported).
//! `OBFUSCATE=1` renames the files first, as obfuscated posts arrive.

use std::path::{Path, PathBuf};
use std::time::Instant;

use newskite_par2::{create, load, repair, verify, RepairOptions};

fn env(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn pseudo_random(len: usize, mut seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        out.extend_from_slice(&seed.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
    }
    out.truncate(len);
    out
}

fn split(dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut files: Vec<PathBuf> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_file()).collect();
    files.sort();
    files.into_iter().partition(|p| newskite_par2::has_magic(p))
}

fn main() {
    let total = env("TOTAL_MIB", 256) << 20;
    let slice = (env("SLICE_KIB", 768) << 10) as u64;
    let corrupt = env("CORRUPT", 8);
    let rounds = env("ROUNDS", 3);
    let threads = env("THREADS", RepairOptions::default().threads);

    let tmp = match std::env::var_os("BENCH_DIR") {
        Some(d) => tempfile::tempdir_in(d),
        None => tempfile::tempdir(),
    }
    .unwrap();
    let dir = tmp.path().to_path_buf();
    // Four files of 40/30/24/6 % of the total; the smallest one is deleted each round.
    let names = ["a.bin", "b.bin", "c.bin", "d.bin"];
    let sizes = [total * 40 / 100 + 123, total * 30 / 100, total * 24 / 100 + 4, total * 6 / 100 + 77];
    let originals: Vec<Vec<u8>> = sizes.iter().enumerate().map(|(i, &n)| pseudo_random(n, i as u64 + 1)).collect();
    for (n, d) in names.iter().zip(&originals) {
        std::fs::write(dir.join(n), d).unwrap();
    }
    let missing = (sizes[3] as u64).div_ceil(slice) as usize + corrupt;
    let recovery = missing as u32 + 4;
    let t = Instant::now();
    create(&dir, &names, slice, recovery, &dir.join("set")).unwrap();
    println!(
        "set: {} MiB in {} slices of {} KiB, {recovery} recovery slices (created in {:.2} s)",
        total >> 20,
        sizes.iter().map(|&n| (n as u64).div_ceil(slice)).sum::<u64>(),
        slice >> 10,
        t.elapsed().as_secs_f64()
    );

    let (par2, _) = split(&dir);
    let (sets, _) = load(&par2);
    let set = &sets[0];
    let mb = set.data_bytes() as f64 / 1e6;
    let opts = RepairOptions { threads, ..Default::default() };
    let (mut best_verify, mut best_damaged, mut best_repair) = (f64::MAX, f64::MAX, f64::MAX);
    // With OBFUSCATE=1 the files are renamed (as obfuscated posts arrive) before each round,
    // so verification has to find them by content and damaged ones slice by slice.
    let obfuscate = env("OBFUSCATE", 0) != 0;
    let on_disk = |i: usize| if obfuscate { format!("x{:08x}", 0x9e37_79b9u32.wrapping_mul(i as u32 + 1)) } else { names[i].to_string() };
    for round in 0..rounds {
        if obfuscate {
            for (i, n) in names.iter().enumerate() {
                std::fs::rename(dir.join(n), dir.join(on_disk(i))).unwrap();
            }
        }
        let (_, others) = split(&dir);
        let t = Instant::now();
        let ver = verify(set, &dir, &others, &mut |_| {});
        let verify_s = t.elapsed().as_secs_f64();
        assert_eq!(ver.missing_slices(), 0);
        assert_eq!(ver.is_intact(), !obfuscate);

        // Damage: delete d.bin, overwrite `corrupt` slices spread over a.bin.
        std::fs::remove_file(dir.join(on_disk(3))).unwrap();
        let mut a = originals[0].clone();
        let a_slices = (a.len() as u64).div_ceil(slice) as usize;
        for c in 0..corrupt {
            let s = (c * a_slices / corrupt.max(1)) as u64 * slice;
            a[s as usize + 7] ^= 0x5a;
        }
        std::fs::write(dir.join(on_disk(0)), &a).unwrap();
        let (_, others) = split(&dir);
        let t = Instant::now();
        let ver = verify(set, &dir, &others, &mut |_| {});
        let damaged_s = t.elapsed().as_secs_f64();
        assert_eq!(ver.missing_slices(), missing);

        let t = Instant::now();
        let out = repair(set, &ver, &opts, &mut |_| {}).unwrap();
        let repair_s = t.elapsed().as_secs_f64();
        assert_eq!(out.slices_rebuilt, missing);
        for b in &out.backups {
            std::fs::remove_file(b).unwrap();
        }
        println!(
            "round {round}: verify intact {verify_s:.3} s ({:.0} MB/s), verify damaged {damaged_s:.3} s, repair {missing} slices {repair_s:.3} s",
            mb / verify_s
        );
        best_verify = best_verify.min(verify_s);
        best_damaged = best_damaged.min(damaged_s);
        best_repair = best_repair.min(repair_s);
    }
    for (n, d) in names.iter().zip(&originals) {
        assert!(std::fs::read(dir.join(n)).unwrap() == *d, "{n} restored");
    }
    // Arithmetic done by a repair: every missing slice from every other input slice, then
    // the k x k inverse applied.
    let inputs = set.slice_count() - missing;
    let gf_gb = (missing * (inputs + missing)) as f64 * slice as f64 / 1e9;
    println!(
        "best: verify intact {:.0} MB/s, verify damaged {best_damaged:.3} s, repair {best_repair:.3} s ({:.1} GB/s of slice products, {threads} threads)",
        mb / best_verify,
        gf_gb / best_repair
    );
}
