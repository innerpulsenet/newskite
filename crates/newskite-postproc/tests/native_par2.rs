//! Post-processing with native PAR2.

use std::fs;
use std::path::Path;

use newskite_postproc::{process_with_events, PostEvent, PostOptions, PostStage, Verdict};

fn bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn setup(names: &[(&str, usize)], recovery: u32) -> (tempfile::TempDir, std::path::PathBuf, Vec<Vec<u8>>) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("dl");
    fs::create_dir(&dir).unwrap();
    let mut data = Vec::new();
    for (i, (n, len)) in names.iter().enumerate() {
        let d = bytes(*len, i as u64 + 3);
        fs::write(dir.join(n), &d).unwrap();
        data.push(d);
    }
    let list: Vec<&str> = names.iter().map(|(n, _)| *n).collect();
    newskite_par2::create(&dir, &list, 4096, recovery, &dir.join("rel")).unwrap();
    fs::write(dir.join("rel.nfo"), b"info").unwrap();
    (tmp, dir, data)
}

fn go(dir: &Path, opts: &PostOptions) -> (newskite_postproc::PostReport, Vec<String>, Vec<PostStage>) {
    let mut logs = Vec::new();
    let mut stages = Vec::new();
    let r = process_with_events(dir, "job", opts, &mut |e| match e {
        PostEvent::Log(l) => logs.push(l),
        PostEvent::Stage { stage, .. } => {
            if stages.last() != Some(&stage) {
                stages.push(stage)
            }
        }
    })
    .unwrap();
    (r, logs, stages)
}

#[test]
fn verified_download_without_archives_loses_its_leftovers() {
    let (_t, dir, _) = setup(&[("movie.mkv", 50_000)], 3);
    let (r, logs, stages) = go(&dir, &PostOptions::default());
    assert_eq!(r.par2, Verdict::Ok, "{logs:#?}");
    assert_eq!(stages.first(), Some(&PostStage::Verify));
    let s = r.par2_stats.as_ref().unwrap();
    assert_eq!((s.files, s.intact, s.slices_missing), (1, 1, 0));
    assert!(!dir.join("rel.par2").exists() && !dir.join("rel.vol00+03.par2").exists());
    assert!(!dir.join("rel.nfo").exists());
    assert!(dir.join("movie.mkv").exists());
    assert_eq!(r.removed.len(), 3);
}

#[test]
fn leftovers_kept_when_disabled() {
    let (_t, dir, _) = setup(&[("movie.mkv", 50_000)], 3);
    let (r, _, _) = go(&dir, &PostOptions { remove_leftovers: false, ..Default::default() });
    assert_eq!(r.par2, Verdict::Ok);
    assert!(r.removed.is_empty());
    assert!(dir.join("rel.par2").exists() && dir.join("rel.nfo").exists());
}

#[test]
fn partial_download_is_completed_from_nkpart() {
    let (_t, dir, data) = setup(&[("movie.mkv", 60_000), ("movie.srt", 3000)], 4);
    // The engine left the file as .nkpart with a missing article zero-filled.
    let mut d = data[0].clone();
    d[8192..12288].fill(0);
    fs::remove_file(dir.join("movie.mkv")).unwrap();
    fs::write(dir.join("movie.mkv.nkpart"), &d).unwrap();
    let (r, logs, stages) = go(&dir, &PostOptions::default());
    assert_eq!(r.par2, Verdict::Repaired, "{logs:#?}");
    assert!(stages.contains(&PostStage::Repair), "{stages:?}");
    assert_eq!(fs::read(dir.join("movie.mkv")).unwrap(), data[0]);
    assert_eq!(r.partials_removed, vec![dir.join("movie.mkv.nkpart")]);
    assert!(r.incomplete.is_empty());
    let s = r.par2_stats.unwrap();
    assert_eq!((s.missing, s.slices_missing, s.slices_rebuilt), (1, 1, 1));
    assert!(logs.iter().any(|l| l.contains("1 of 15 slices found") || l.contains("14 of 15")), "{logs:#?}");
    // Repaired from the partial: no backups or leftovers stay behind.
    assert!(!dir.join("rel.par2").exists());
}

#[test]
fn unrepairable_partial_stays_incomplete() {
    let (_t, dir, data) = setup(&[("movie.mkv", 60_000)], 1);
    let mut d = data[0].clone();
    d[0..20_000].fill(0);
    fs::remove_file(dir.join("movie.mkv")).unwrap();
    fs::write(dir.join("movie.mkv.nkpart"), &d).unwrap();
    let (r, logs, _) = go(&dir, &PostOptions::default());
    match &r.par2 {
        Verdict::Failed(why) => assert!(why.contains("need 4 more recovery slice"), "{why}"),
        v => panic!("{v:?} {logs:#?}"),
    }
    assert_eq!(r.incomplete, vec![dir.join("movie.mkv.nkpart")]);
    assert!(dir.join("rel.par2").exists(), "parity kept for a later retry");
}

/// Packet layout of a volume `newskite_par2::create` writes: recovery packets first, each
/// 64 header bytes, 4 exponent bytes and one 4096-byte slice.
const RECOVERY_PACKET: usize = 64 + 4 + 4096;

#[test]
fn damaged_par2_volumes_and_sidecars_are_not_payload() {
    // The data arrived whole; a PAR2 volume (under a random name, found by its packets)
    // and the .nfo lost articles. That is not an incomplete download.
    let (_t, dir, data) = setup(&[("movie.mkv", 50_000)], 3);
    let mut vol = fs::read(dir.join("rel.vol00+03.par2")).unwrap();
    vol[RECOVERY_PACKET..2 * RECOVERY_PACKET].fill(0);
    fs::remove_file(dir.join("rel.vol00+03.par2")).unwrap();
    fs::write(dir.join("Zq81xKe.nkpart"), &vol).unwrap();
    fs::remove_file(dir.join("rel.nfo")).unwrap();
    fs::write(dir.join("rel.nfo.nkpart"), b"in\0\0").unwrap();
    assert!(newskite_postproc::is_unneeded(&dir.join("Zq81xKe.nkpart")));
    assert!(newskite_postproc::is_unneeded(&dir.join("rel.nfo.nkpart")));
    assert!(!newskite_postproc::is_unneeded(&dir.join("movie.mkv.nkpart")), "absent, and its name is payload");

    let (r, logs, _) = go(&dir, &PostOptions::default());
    assert_eq!(r.par2, Verdict::Ok, "{logs:#?}");
    assert!(r.incomplete.is_empty(), "{:?}", r.incomplete);
    assert_eq!(r.unneeded, vec![dir.join("Zq81xKe.nkpart"), dir.join("rel.nfo.nkpart")]);
    assert!(!dir.join("Zq81xKe.nkpart").exists() && !dir.join("rel.nfo.nkpart").exists(), "removed once verified");
    assert_eq!(fs::read(dir.join("movie.mkv")).unwrap(), data[0]);
}

#[test]
fn unneeded_partials_stay_when_the_data_is_not_verified() {
    let (_t, dir, _) = setup(&[("movie.mkv", 50_000)], 3);
    fs::remove_file(dir.join("rel.nfo")).unwrap();
    fs::write(dir.join("rel.nfo.nkpart"), b"in").unwrap();
    let (r, _, _) = go(&dir, &PostOptions { repair: true, ..Default::default() });
    assert!(r.par2.is_success());
    // Same files, but with PAR2 unreadable: nothing proves the data, so the partial stays
    // (and still does not count as incomplete).
    let (_t, dir, _) = setup(&[("movie.mkv", 50_000)], 3);
    for p in ["rel.par2", "rel.vol00+03.par2"] {
        fs::write(dir.join(p), b"not par2").unwrap();
    }
    fs::write(dir.join("rel.nfo.nkpart"), b"in").unwrap();
    let (r, logs, _) = go(&dir, &PostOptions::default());
    assert!(matches!(r.par2, Verdict::Skipped(_)), "{logs:#?}");
    assert!(r.incomplete.is_empty());
    assert_eq!(r.unneeded, vec![dir.join("rel.nfo.nkpart")]);
    assert!(dir.join("rel.nfo.nkpart").exists());
}

#[test]
fn a_damaged_par2_volume_still_supplies_its_intact_recovery_blocks() {
    // Two slices of the data are missing; the only recovery data is a volume that lost one
    // of its four blocks to a missing article, and the index is gone entirely. The three
    // intact blocks (and the volume's own copy of the set description) repair it.
    let (_t, dir, data) = setup(&[("movie.mkv", 60_000)], 4);
    let mut d = data[0].clone();
    d[8192..16384].fill(0);
    fs::remove_file(dir.join("movie.mkv")).unwrap();
    fs::write(dir.join("movie.mkv.nkpart"), &d).unwrap();
    let mut vol = fs::read(dir.join("rel.vol00+04.par2")).unwrap();
    vol[100..2000].fill(0);
    fs::remove_file(dir.join("rel.vol00+04.par2")).unwrap();
    fs::write(dir.join("rel.vol00+04.par2.nkpart"), &vol).unwrap();
    fs::remove_file(dir.join("rel.par2")).unwrap();
    assert!(newskite_postproc::has_par2(&dir), "a partial volume is PAR2 data too");

    let (r, logs, _) = go(&dir, &PostOptions::default());
    assert_eq!(r.par2, Verdict::Repaired, "{logs:#?}");
    assert_eq!(fs::read(dir.join("movie.mkv")).unwrap(), data[0]);
    assert!(r.incomplete.is_empty(), "{:?}", r.incomplete);
    assert_eq!(r.par2_stats.unwrap().slices_rebuilt, 2);
    assert!(logs.iter().any(|l| l.contains("recovery slice 0 is damaged")), "{logs:#?}");
}
