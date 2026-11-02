//! `cargo run --release -p newskite-par2 --example par2 -- <folder>`: verify and repair
//! every PAR2 set in a folder with the native implementation, printing timings.

use std::time::Instant;

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: par2 <folder>"));
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
    let (par2, others): (Vec<_>, Vec<_>) = files.into_iter().partition(|p| newskite_par2::has_magic(p));
    let (sets, bad) = newskite_par2::load(&par2);
    println!("{} set(s), {bad} bad packet(s)", sets.len());
    for set in &sets {
        let t = Instant::now();
        let ver = newskite_par2::verify(set, &dir, &others, &mut |_| {});
        let mb = set.data_bytes() as f64 / 1e6;
        let s = t.elapsed().as_secs_f64();
        println!("verify: {mb:.0} MB in {s:.2} s ({:.0} MB/s); {} missing of {} slices, {} recovery", mb / s, ver.missing_slices(), ver.sources.len(), ver.recovery_available);
        if ver.is_intact() {
            continue;
        }
        let t = Instant::now();
        match newskite_par2::repair(set, &ver, &Default::default(), &mut |_| {}) {
            Ok(r) => println!("repair: {} slices in {:.2} s ({} file(s) rebuilt)", r.slices_rebuilt, t.elapsed().as_secs_f64(), r.rebuilt.len()),
            Err(e) => println!("repair failed: {e}"),
        }
    }
}
