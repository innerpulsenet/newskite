use std::path::{Path, PathBuf};
use std::process::Command;

use newskite_par2::{create, load, repair, verify, Event, FileStatus, Par2Error, RepairOptions};

fn data(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761).wrapping_add(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    originals: Vec<(String, Vec<u8>)>,
}

fn fixture(files: &[(&str, usize)]) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("job");
    std::fs::create_dir(&dir).unwrap();
    let mut originals = Vec::new();
    for (i, (name, len)) in files.iter().enumerate() {
        let d = data(*len, i as u32 + 7);
        std::fs::write(dir.join(name), &d).unwrap();
        originals.push((name.to_string(), d));
    }
    Fixture { _tmp: tmp, dir, originals }
}

fn par2_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file() && newskite_par2::has_magic(p))
        .collect();
    v.sort();
    v
}

fn others(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file() && !newskite_par2::has_magic(p))
        .collect();
    v.sort();
    v
}

fn assert_restored(f: &Fixture) {
    for (name, d) in &f.originals {
        assert_eq!(&std::fs::read(f.dir.join(name)).unwrap(), d, "{name}");
    }
}

fn run(f: &Fixture, opts: &RepairOptions) -> Result<newskite_par2::RepairOutcome, Par2Error> {
    let (sets, _) = load(&par2_files(&f.dir));
    assert_eq!(sets.len(), 1);
    let set = &sets[0];
    let ver = verify(set, &f.dir, &others(&f.dir), &mut |_| {});
    repair(set, &ver, opts, &mut |_| {})
}

#[test]
fn intact_set_verifies() {
    let f = fixture(&[("a.bin", 10_000), ("b.bin", 4096), ("empty", 0)]);
    create(&f.dir, &["a.bin", "b.bin", "empty"], 1024, 3, &f.dir.join("set")).unwrap();
    let (sets, bad) = load(&par2_files(&f.dir));
    assert_eq!((sets.len(), bad), (1, 0));
    assert_eq!(sets[0].files.len(), 3);
    assert_eq!(sets[0].recovery.len(), 3);
    assert_eq!(sets[0].creator.as_deref(), Some("Newskite"));
    let mut events = Vec::new();
    let ver = verify(&sets[0], &f.dir, &others(&f.dir), &mut |e| events.push(e));
    assert!(ver.is_intact(), "{:?}", ver.files);
    assert_eq!(ver.missing_slices(), 0);
    assert!(events.iter().any(|e| matches!(e, Event::Bytes { done, total, .. } if done == total && *total == 14096)));
}

#[test]
fn repairs_damaged_and_missing_files() {
    let f = fixture(&[("a.bin", 20_000), ("b.bin", 7_000), ("c.bin", 3_333)]);
    create(&f.dir, &["a.bin", "b.bin", "c.bin"], 2048, 12, &f.dir.join("set")).unwrap();
    // Zero a range in a (like a missing article), truncate c, delete b.
    let mut a = std::fs::read(f.dir.join("a.bin")).unwrap();
    a[5000..9000].fill(0);
    std::fs::write(f.dir.join("a.bin"), &a).unwrap();
    std::fs::remove_file(f.dir.join("b.bin")).unwrap();
    let c = std::fs::read(f.dir.join("c.bin")).unwrap();
    std::fs::write(f.dir.join("c.bin"), &c[..2100]).unwrap();

    let (sets, _) = load(&par2_files(&f.dir));
    let ver = verify(&sets[0], &f.dir, &others(&f.dir), &mut |_| {});
    let status = |n: &str| ver.files.iter().find(|r| r.name == n).unwrap().status.clone();
    assert_eq!(status("a.bin"), FileStatus::Damaged { found: 7, total: 10 });
    assert_eq!(status("b.bin"), FileStatus::Missing { found: 0, total: 4 });
    assert_eq!(status("c.bin"), FileStatus::Damaged { found: 1, total: 2 });
    assert_eq!(ver.missing_slices(), 8);
    assert!(ver.repairable());

    let out = repair(&sets[0], &ver, &RepairOptions::default(), &mut |_| {}).unwrap();
    assert_eq!(out.slices_rebuilt, 8);
    assert_eq!(out.rebuilt.len(), 3);
    assert_eq!(out.backups.len(), 2, "{:?}", out.backups);
    assert!(f.dir.join("a.bin.1").exists());
    assert_restored(&f);
    let ver = verify(&sets[0], &f.dir, &others(&f.dir), &mut |_| {});
    assert!(ver.is_intact());
}

#[test]
fn small_memory_limit_uses_several_passes() {
    let f = fixture(&[("a.bin", 64 * 1024 + 6), ("b.bin", 40_000)]);
    create(&f.dir, &["a.bin", "b.bin"], 16 * 1024, 4, &f.dir.join("set")).unwrap();
    std::fs::remove_file(f.dir.join("b.bin")).unwrap();
    let opts = RepairOptions { memory_limit: 8 * 1024, threads: 3 };
    run(&f, &opts).unwrap();
    assert_restored(&f);
}

#[test]
fn obfuscated_names_are_restored() {
    let f = fixture(&[("movie.mkv", 30_000), ("movie.nfo", 500)]);
    create(&f.dir, &["movie.mkv", "movie.nfo"], 4096, 2, &f.dir.join("set")).unwrap();
    std::fs::rename(f.dir.join("movie.mkv"), f.dir.join("a8f3c1d9e2")).unwrap();
    // PAR2 files with random names work too.
    std::fs::rename(f.dir.join("set.par2"), f.dir.join("x1y2z3")).unwrap();
    let out = run(&f, &RepairOptions::default()).unwrap();
    assert_eq!(out.renamed.len(), 1);
    assert_eq!(out.slices_rebuilt, 0);
    assert!(!f.dir.join("a8f3c1d9e2").exists());
    assert_restored(&f);
}

#[test]
fn damaged_renamed_copy_contributes_slices() {
    let f = fixture(&[("big.bin", 40_000)]);
    create(&f.dir, &["big.bin"], 4096, 2, &f.dir.join("set")).unwrap();
    let mut d = std::fs::read(f.dir.join("big.bin")).unwrap();
    d[100..200].fill(0xAA);
    std::fs::write(f.dir.join("obf.001"), &d).unwrap();
    std::fs::remove_file(f.dir.join("big.bin")).unwrap();
    let (sets, _) = load(&par2_files(&f.dir));
    let ver = verify(&sets[0], &f.dir, &others(&f.dir), &mut |_| {});
    assert_eq!(ver.files[0].status, FileStatus::Missing { found: 9, total: 10 });
    repair(&sets[0], &ver, &RepairOptions::default(), &mut |_| {}).unwrap();
    assert_restored(&f);
}

#[test]
fn not_enough_recovery_changes_nothing() {
    let f = fixture(&[("a.bin", 20_000)]);
    create(&f.dir, &["a.bin"], 1024, 2, &f.dir.join("set")).unwrap();
    let mut a = std::fs::read(f.dir.join("a.bin")).unwrap();
    a[0..5000].fill(0);
    std::fs::write(f.dir.join("a.bin"), &a).unwrap();
    let (sets, _) = load(&par2_files(&f.dir));
    let ver = verify(&sets[0], &f.dir, &others(&f.dir), &mut |_| {});
    assert!(!ver.repairable());
    assert_eq!(ver.blocks_short(), 3);
    let err = repair(&sets[0], &ver, &RepairOptions::default(), &mut |_| {}).unwrap_err();
    assert!(matches!(err, Par2Error::NotEnoughRecovery { needed: 5, available: 2 }), "{err}");
    assert_eq!(std::fs::read(f.dir.join("a.bin")).unwrap(), a);
    assert!(!f.dir.join("a.bin.1").exists());
}

#[test]
fn corrupt_recovery_slice_is_skipped() {
    let f = fixture(&[("a.bin", 8192)]);
    let written = create(&f.dir, &["a.bin"], 1024, 3, &f.dir.join("set")).unwrap();
    // Flip a byte inside the first recovery slice's data.
    let vol = &written[1];
    let mut v = std::fs::read(vol).unwrap();
    v[64 + 4 + 10] ^= 0xff;
    std::fs::write(vol, &v).unwrap();
    let mut a = std::fs::read(f.dir.join("a.bin")).unwrap();
    a[0] ^= 1;
    a[2000] ^= 1;
    std::fs::write(f.dir.join("a.bin"), &a).unwrap();
    let (sets, _) = load(&par2_files(&f.dir));
    let ver = verify(&sets[0], &f.dir, &others(&f.dir), &mut |_| {});
    let mut notes = Vec::new();
    repair(&sets[0], &ver, &RepairOptions::default(), &mut |e| {
        if let Event::Note(n) = e {
            notes.push(n)
        }
    })
    .unwrap();
    assert!(notes.iter().any(|n| n.contains("damaged; skipping")), "{notes:?}");
    assert_restored(&f);
}

#[test]
fn unsafe_names_are_refused() {
    let f = fixture(&[("a.bin", 4000)]);
    // Build a set whose stored name escapes the folder.
    let outside = f.dir.parent().unwrap().join("evil.bin");
    std::fs::write(&outside, data(4000, 1)).unwrap();
    create(&f.dir, &["../evil.bin"], 1024, 2, &f.dir.join("set")).unwrap();
    std::fs::remove_file(&outside).unwrap();
    let (sets, _) = load(&par2_files(&f.dir));
    let ver = verify(&sets[0], &f.dir, &others(&f.dir), &mut |_| {});
    assert_eq!(ver.files[0].status, FileStatus::UnsafeName);
    assert!(matches!(repair(&sets[0], &ver, &RepairOptions::default(), &mut |_| {}), Err(Par2Error::UnsafeName)));
    assert!(!outside.exists());
}

/// A damaged copy of another size (not a candidate for the whole file) still gives its
/// good slices at aligned offsets (not the last one: junk follows it where PAR2 pads with
/// zeros), and slices matching nothing are ignored.
#[test]
fn stray_slices_in_other_files_are_found() {
    let f = fixture(&[("big.bin", 40_000)]);
    create(&f.dir, &["big.bin"], 4096, 2, &f.dir.join("set")).unwrap();
    let mut d = std::fs::read(f.dir.join("big.bin")).unwrap();
    d[5000] ^= 0xff;
    d.extend_from_slice(&[7u8; 5000]);
    std::fs::write(f.dir.join("other.bin"), &d).unwrap();
    std::fs::write(f.dir.join("noise.bin"), data(30_000, 99)).unwrap();
    std::fs::remove_file(f.dir.join("big.bin")).unwrap();
    let (sets, _) = load(&par2_files(&f.dir));
    let mut notes = Vec::new();
    let ver = verify(&sets[0], &f.dir, &others(&f.dir), &mut |e| {
        if let Event::Note(n) = e {
            notes.push(n)
        }
    });
    assert_eq!(ver.files[0].status, FileStatus::Missing { found: 8, total: 10 });
    assert_eq!(notes, ["found 8 good slice(s) in other.bin"]);
    repair(&sets[0], &ver, &RepairOptions::default(), &mut |_| {}).unwrap();
    assert_restored(&f);
}

/// A damaged renamed copy of one file also holds a slice another (missing) file needs:
/// the copy is read once, for both searches.
#[test]
fn damaged_renamed_copy_supplies_slices_of_other_files() {
    let f = fixture(&[("a.bin", 40_000), ("b.bin", 20_000)]);
    let mut a = std::fs::read(f.dir.join("a.bin")).unwrap();
    let mut b = std::fs::read(f.dir.join("b.bin")).unwrap();
    b[8192..12288].copy_from_slice(&a[12288..16384]);
    std::fs::write(f.dir.join("b.bin"), &b).unwrap();
    let f = Fixture { originals: vec![("a.bin".into(), a.clone()), ("b.bin".into(), b)], ..f };
    create(&f.dir, &["a.bin", "b.bin"], 4096, 8, &f.dir.join("set")).unwrap();
    // Past the first 16 KiB, so the copy is still recognized as a's.
    a[30_000] ^= 1;
    std::fs::write(f.dir.join("x9f2"), &a).unwrap();
    std::fs::remove_file(f.dir.join("a.bin")).unwrap();
    std::fs::remove_file(f.dir.join("b.bin")).unwrap();
    let (sets, _) = load(&par2_files(&f.dir));
    let mut notes = Vec::new();
    let ver = verify(&sets[0], &f.dir, &others(&f.dir), &mut |e| {
        if let Event::Note(n) = e {
            notes.push(n)
        }
    });
    let status = |n: &str| ver.files.iter().find(|r| r.name == n).unwrap().status.clone();
    assert_eq!(status("a.bin"), FileStatus::Missing { found: 9, total: 10 });
    assert_eq!(status("b.bin"), FileStatus::Missing { found: 1, total: 5 });
    assert_eq!(notes, ["found 1 good slice(s) in x9f2"]);
    repair(&sets[0], &ver, &RepairOptions::default(), &mut |_| {}).unwrap();
    assert_restored(&f);
}

/// Slices big enough that each pass is split over several threads, odd file lengths
/// (short last slices), and a memory limit that makes passes shorter than a slice.
#[test]
fn threads_split_slices_and_passes() {
    for opts in [RepairOptions { threads: 3, memory_limit: 21 * 40_000 }, RepairOptions { threads: 7, ..Default::default() }] {
        let f = fixture(&[("a.bin", 350_001), ("b.bin", 123_457), ("c.bin", 99_999)]);
        create(&f.dir, &["a.bin", "b.bin", "c.bin"], 100_000, 6, &f.dir.join("set")).unwrap();
        let mut a = std::fs::read(f.dir.join("a.bin")).unwrap();
        a[150_000] ^= 1;
        a[350_000] ^= 1;
        std::fs::write(f.dir.join("a.bin"), &a).unwrap();
        std::fs::remove_file(f.dir.join("b.bin")).unwrap();
        let out = run(&f, &opts).unwrap();
        assert_eq!(out.slices_rebuilt, 4, "{opts:?}");
        assert_restored(&f);
    }
}

/// A file with more slices than one read covers is checked in several runs (several
/// threads); damage in each run is found and repaired.
#[test]
fn large_file_is_checked_in_runs() {
    let f = fixture(&[("big.bin", 9 * 1024 * 1024 + 3)]);
    create(&f.dir, &["big.bin"], 64 * 1024, 3, &f.dir.join("set")).unwrap();
    let (sets, _) = load(&par2_files(&f.dir));
    let ver = verify(&sets[0], &f.dir, &others(&f.dir), &mut |_| {});
    assert!(ver.is_intact());
    let mut d = std::fs::read(f.dir.join("big.bin")).unwrap();
    d[5] ^= 1;
    d[130 * 65536 + 7] ^= 1;
    let last = d.len() - 1;
    d[last] ^= 1;
    std::fs::write(f.dir.join("big.bin"), &d).unwrap();
    let ver = verify(&sets[0], &f.dir, &others(&f.dir), &mut |_| {});
    assert_eq!(ver.files[0].status, FileStatus::Damaged { found: 145 - 3, total: 145 });
    repair(&sets[0], &ver, &RepairOptions::default(), &mut |_| {}).unwrap();
    assert_restored(&f);
}

/// Two files with the same content under obfuscated names each get their own copy.
#[test]
fn identical_obfuscated_files_get_distinct_copies() {
    let f = fixture(&[("x.bin", 9_000)]);
    std::fs::copy(f.dir.join("x.bin"), f.dir.join("y.bin")).unwrap();
    create(&f.dir, &["x.bin", "y.bin"], 2048, 2, &f.dir.join("set")).unwrap();
    std::fs::rename(f.dir.join("x.bin"), f.dir.join("q1")).unwrap();
    std::fs::rename(f.dir.join("y.bin"), f.dir.join("q2")).unwrap();
    let (sets, _) = load(&par2_files(&f.dir));
    let ver = verify(&sets[0], &f.dir, &others(&f.dir), &mut |_| {});
    let picked: Vec<_> = ver
        .files
        .iter()
        .map(|r| match &r.status {
            FileStatus::Misnamed(p) => p.file_name().unwrap().to_owned(),
            s => panic!("{}: {s:?}", r.name),
        })
        .collect();
    assert_ne!(picked[0], picked[1]);
    repair(&sets[0], &ver, &RepairOptions::default(), &mut |_| {}).unwrap();
    assert_eq!(std::fs::read(f.dir.join("x.bin")).unwrap(), std::fs::read(f.dir.join("y.bin")).unwrap());
    assert_eq!(std::fs::read(f.dir.join("x.bin")).unwrap(), f.originals[0].1);
}

fn par2cmd() -> Option<PathBuf> {
    let p = PathBuf::from("/usr/bin/par2");
    p.exists().then_some(p).or_else(|| {
        Command::new("par2").arg("-V").output().ok().filter(|o| o.status.success()).map(|_| PathBuf::from("par2"))
    })
}

/// Our recovery data must be what par2cmdline computes, and vice versa.
#[test]
fn interoperates_with_par2cmdline() {
    let Some(par2) = par2cmd() else {
        eprintln!("par2 not installed; skipping");
        return;
    };
    // par2cmdline repairs what we created.
    let f = fixture(&[("a.bin", 50_000), ("b.bin", 12_345)]);
    create(&f.dir, &["a.bin", "b.bin"], 4096, 6, &f.dir.join("ours")).unwrap();
    std::fs::remove_file(f.dir.join("b.bin")).unwrap();
    let st = Command::new(&par2).args(["r", "-q", "ours.par2"]).current_dir(&f.dir).output().unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stdout));
    assert_restored(&f);

    // We repair what par2cmdline created.
    let g = fixture(&[("x.bin", 80_000), ("y.bin", 5_000)]);
    let st = Command::new(&par2)
        .args(["c", "-q", "-s8192", "-c8", "theirs.par2", "x.bin", "y.bin"])
        .current_dir(&g.dir)
        .output()
        .unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
    let mut x = std::fs::read(g.dir.join("x.bin")).unwrap();
    x[10_000..30_000].fill(0);
    std::fs::write(g.dir.join("x.bin"), &x).unwrap();
    std::fs::remove_file(g.dir.join("y.bin")).unwrap();
    let out = run(&g, &RepairOptions::default()).unwrap();
    assert_eq!(out.rebuilt.len(), 2);
    assert_restored(&g);
}
