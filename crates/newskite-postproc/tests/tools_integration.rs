//! Integration tests against the real external tools. Each test prints a note and
//! returns early when a tool it needs is not installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use newskite_postproc::{detect_tools, process, PostOptions, PostReport, Tools, Verdict};

macro_rules! require {
    ($tools:expr, $($field:ident),+) => {
        $(
            if $tools.$field.is_none() {
                eprintln!("note: skipping test, `{}` tool not installed", stringify!($field));
                return;
            }
        )+
    };
}

/// Deterministic pseudo-random bytes (xorshift) so tests need no RNG crate.
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
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

/// Run a helper command used to *create* fixtures; panics with its output on failure.
fn sh(program: &Path, args: &[&str], cwd: &Path) {
    let out = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("spawn fixture tool");
    assert!(
        out.status.success(),
        "{} {:?} failed: {}\n{}",
        program.display(),
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Fixture {
    _tmp: tempfile::TempDir,
    /// The download folder handed to `process`.
    dl: PathBuf,
    /// Scratch folder with the original payload files.
    src: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().join("download");
    let src = tmp.path().join("src");
    fs::create_dir_all(&dl).unwrap();
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("a.txt"), b"hello newskite\n").unwrap();
    fs::write(src.join("sub").join("b.bin"), pseudo_random(150_000, 7)).unwrap();
    Fixture { _tmp: tmp, dl, src }
}

fn run(dir: &Path, job: &str, opts: &PostOptions, tools: &Tools) -> (PostReport, Vec<String>) {
    let mut logs = Vec::new();
    let report = process(dir, job, opts, tools, &mut |l| logs.push(l)).expect("process");
    (report, logs)
}

fn assert_payload_extracted(target: &Path, src: &Path) {
    assert_eq!(fs::read(target.join("a.txt")).unwrap(), fs::read(src.join("a.txt")).unwrap());
    assert_eq!(
        fs::read(target.join("sub").join("b.bin")).unwrap(),
        fs::read(src.join("sub").join("b.bin")).unwrap()
    );
}

fn sub_path(rel: &str) -> String {
    Path::new("sub").join(rel).to_string_lossy().into_owned()
}

#[test]
fn zip_extracts_into_job_folder_with_7z() {
    let tools = detect_tools();
    require!(tools, sevenz);
    let f = fixture();
    let zip = f.dl.join("release.zip");
    sh(tools.sevenz.as_ref().unwrap(), &["a", "-tzip", zip.to_str().unwrap(), "a.txt", &sub_path("b.bin")], &f.src);

    let (r, _) = run(&f.dl, "My Release: 2024", &PostOptions::default(), &tools);
    assert_eq!(r.par2, Verdict::Skipped("no PAR2 files".into()));
    assert_eq!(r.extract, Verdict::Ok);
    let target = f.dl.join("My Release_ 2024");
    assert_eq!(r.extracted_to.as_deref(), Some(target.as_path()));
    assert_payload_extracted(&target, &f.src);
    // cleanup is off by default
    assert!(zip.exists());
    assert!(r.removed.is_empty());
}

#[test]
fn zip_extracts_with_unzip_when_7z_missing() {
    let tools = detect_tools();
    require!(tools, sevenz, unzip);
    let f = fixture();
    let zip = f.dl.join("release.zip");
    sh(tools.sevenz.as_ref().unwrap(), &["a", "-tzip", zip.to_str().unwrap(), "a.txt", &sub_path("b.bin")], &f.src);

    let only_unzip = Tools { unzip: tools.unzip.clone(), ..Tools::none() };
    let (r, logs) = run(&f.dl, "job", &PostOptions::default(), &only_unzip);
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_payload_extracted(&f.dl.join("job"), &f.src);
}

#[test]
fn encrypted_7z_right_wrong_and_missing_password() {
    let tools = detect_tools();
    require!(tools, sevenz);
    for header_encryption in [true, false] {
        let f = fixture();
        let archive = f.dl.join("secret.7z");
        let mut args = vec!["a", "-pCorrect Horse", archive.to_str().unwrap()];
        if header_encryption {
            args.insert(1, "-mhe=on");
        }
        let sub = sub_path("b.bin");
        args.extend(["a.txt", sub.as_str()]);
        sh(tools.sevenz.as_ref().unwrap(), &args, &f.src);

        // Wrong password
        let opts = PostOptions { password: Some("wrong one".into()), ..Default::default() };
        let (r, logs) = run(&f.dl, "job", &opts, &tools);
        assert_eq!(r.extract, Verdict::Failed("wrong password".into()), "hdr={header_encryption} {logs:#?}");
        assert!(logs.iter().all(|l| !l.contains("wrong one")), "password leaked into logs");

        // No password at all: must not hang on a prompt, must explain.
        let (r, _) = run(&f.dl, "job", &PostOptions::default(), &tools);
        match &r.extract {
            Verdict::Failed(reason) => assert!(reason.contains("password"), "{reason}"),
            other => panic!("expected failure, got {other:?}"),
        }

        // Right password
        let opts = PostOptions { password: Some("Correct Horse".into()), cleanup: true, ..Default::default() };
        let (r, logs) = run(&f.dl, "job", &opts, &tools);
        assert_eq!(r.extract, Verdict::Ok, "hdr={header_encryption} {logs:#?}");
        assert!(logs.iter().all(|l| !l.contains("Correct Horse")), "password leaked into logs");
        assert_payload_extracted(&f.dl.join("job"), &f.src);
        assert_eq!(r.removed, vec![archive.clone()]);
        assert!(!archive.exists());
    }
}

#[test]
fn multi_volume_7z_passes_first_volume_and_cleans_up() {
    let tools = detect_tools();
    require!(tools, sevenz);
    let f = fixture();
    let base = f.dl.join("big.7z");
    sh(
        tools.sevenz.as_ref().unwrap(),
        &["a", "-mx0", "-v40k", base.to_str().unwrap(), "a.txt", &sub_path("b.bin")],
        &f.src,
    );
    let mut volumes: Vec<PathBuf> = fs::read_dir(&f.dl).unwrap().map(|e| e.unwrap().path()).collect();
    volumes.sort();
    assert!(volumes.len() >= 3, "expected several volumes, got {volumes:?}");
    assert!(volumes[0].to_string_lossy().ends_with("big.7z.001"));

    let opts = PostOptions { cleanup: true, ..Default::default() };
    let (r, logs) = run(&f.dl, "Big Job", &opts, &tools);
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_payload_extracted(&f.dl.join("Big Job"), &f.src);
    // Only the first volume is handed to the tool.
    let tool_lines: Vec<_> = logs.iter().filter(|l| l.contains(" x ")).collect();
    assert_eq!(tool_lines.len(), 1, "{logs:#?}");
    assert!(tool_lines[0].contains("big.7z.001") && !tool_lines[0].contains("big.7z.002"));
    assert_eq!(r.removed, volumes);
    for v in &volumes {
        assert!(!v.exists());
    }
}

#[test]
fn missing_middle_volume_fails_without_cleanup() {
    let tools = detect_tools();
    require!(tools, sevenz);
    let f = fixture();
    let base = f.dl.join("big.7z");
    sh(tools.sevenz.as_ref().unwrap(), &["a", "-mx0", "-v40k", base.to_str().unwrap(), &sub_path("b.bin")], &f.src);
    fs::remove_file(f.dl.join("big.7z.002")).unwrap();
    let opts = PostOptions { cleanup: true, ..Default::default() };
    let (r, logs) = run(&f.dl, "job", &opts, &tools);
    assert!(r.extract.is_failed(), "{logs:#?}");
    assert!(r.removed.is_empty());
    assert!(f.dl.join("big.7z.001").exists());
}

#[test]
fn missing_first_volume_is_reported() {
    let tools = detect_tools();
    require!(tools, sevenz);
    let f = fixture();
    let base = f.dl.join("big.7z");
    sh(tools.sevenz.as_ref().unwrap(), &["a", "-mx0", "-v40k", base.to_str().unwrap(), &sub_path("b.bin")], &f.src);
    fs::remove_file(f.dl.join("big.7z.001")).unwrap();
    let (r, _) = run(&f.dl, "job", &PostOptions::default(), &tools);
    match r.extract {
        Verdict::Failed(reason) => assert!(reason.contains("first volume missing"), "{reason}"),
        other => panic!("expected failure, got {other:?}"),
    }
}

#[test]
fn obfuscated_archive_detected_by_magic() {
    let tools = detect_tools();
    require!(tools, sevenz);
    let f = fixture();
    let archive = f.dl.join("payload.7z");
    sh(tools.sevenz.as_ref().unwrap(), &["a", archive.to_str().unwrap(), "a.txt", &sub_path("b.bin")], &f.src);
    fs::rename(&archive, f.dl.join("a8f7e6d5c4b3a2918f7e")).unwrap();
    // A real document that happens to be a zip container must not be touched.
    let docx = f.dl.join("notes.docx");
    sh(tools.sevenz.as_ref().unwrap(), &["a", "-tzip", docx.to_str().unwrap(), "a.txt"], &f.src);
    let (r, logs) = run(&f.dl, "job", &PostOptions::default(), &tools);
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_payload_extracted(&f.dl.join("job"), &f.src);
    assert!(logs.iter().all(|l| !l.contains("notes.docx")), "{logs:#?}");
}

#[test]
fn no_archives_and_no_par2_is_skipped() {
    let tools = detect_tools();
    let f = fixture();
    fs::write(f.dl.join("movie.mkv"), pseudo_random(10_000, 3)).unwrap();
    fs::write(f.dl.join("movie.mkv.nkpart"), b"partial").unwrap();
    let opts = PostOptions { cleanup: true, ..Default::default() };
    let (r, _) = run(&f.dl, "job", &opts, &tools);
    assert_eq!(r.par2, Verdict::Skipped("no PAR2 files".into()));
    assert_eq!(r.extract, Verdict::Skipped("no archives found".into()));
    assert_eq!(r.extracted_to, None);
    assert!(r.removed.is_empty());
    assert!(!f.dl.join("job").exists());
}

#[test]
fn no_tools_skips_gracefully() {
    let f = fixture();
    fs::write(f.dl.join("x.par2"), b"PAR2\0PKT").unwrap();
    fs::write(f.dl.join("x.rar"), b"Rar!\x1a\x07\x01\x00junk").unwrap();
    let (r, _) = run(&f.dl, "job", &PostOptions { cleanup: true, ..Default::default() }, &Tools::none());
    assert_eq!(r.par2, Verdict::Skipped("par2 tool not found".into()));
    match &r.extract {
        Verdict::Skipped(reason) => assert!(reason.contains("unrar"), "{reason}"),
        other => panic!("expected skip, got {other:?}"),
    }
    assert!(r.removed.is_empty());
    assert!(f.dl.join("x.rar").exists() && f.dl.join("x.par2").exists());
    assert!(!f.dl.join("job").exists());
}

#[test]
fn extraction_disabled_is_skipped() {
    let tools = detect_tools();
    let f = fixture();
    fs::write(f.dl.join("x.zip"), b"PK\x03\x04").unwrap();
    let opts = PostOptions { extract: false, ..Default::default() };
    let (r, _) = run(&f.dl, "job", &opts, &tools);
    assert_eq!(r.extract, Verdict::Skipped("extraction disabled".into()));
}

#[test]
fn corrupt_zip_fails() {
    let tools = detect_tools();
    require!(tools, sevenz);
    let f = fixture();
    let mut data = b"PK\x03\x04".to_vec();
    data.extend(pseudo_random(5000, 9));
    fs::write(f.dl.join("broken.zip"), data).unwrap();
    let (r, logs) = run(&f.dl, "job", &PostOptions { cleanup: true, ..Default::default() }, &tools);
    assert!(r.extract.is_failed(), "{logs:#?}");
    assert!(f.dl.join("broken.zip").exists());
}

// ---------------------------------------------------------------- zip-slip

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Minimal stored (uncompressed) zip writer that allows arbitrary entry names.
fn write_raw_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let len = data.len() as u32;
        let mut hdr = Vec::new();
        hdr.extend(0x0403_4b50u32.to_le_bytes());
        hdr.extend(20u16.to_le_bytes()); // version needed
        hdr.extend(0u16.to_le_bytes()); // flags
        hdr.extend(0u16.to_le_bytes()); // stored
        hdr.extend(0u16.to_le_bytes()); // time
        hdr.extend(0x21u16.to_le_bytes()); // date 1980-01-01
        hdr.extend(crc.to_le_bytes());
        hdr.extend(len.to_le_bytes());
        hdr.extend(len.to_le_bytes());
        hdr.extend((name.len() as u16).to_le_bytes());
        hdr.extend(0u16.to_le_bytes());
        out.extend(&hdr);
        out.extend(name.as_bytes());
        out.extend(*data);

        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes()); // version made by
        central.extend(&hdr[4..]); // version needed .. extra len (26 bytes)
        central.extend(0u16.to_le_bytes()); // comment len
        central.extend(0u16.to_le_bytes()); // disk
        central.extend(0u16.to_le_bytes()); // internal attrs
        central.extend(0u32.to_le_bytes()); // external attrs
        central.extend(offset.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend(&central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    fs::write(path, out).unwrap();
}

#[test]
fn zip_slip_archive_is_refused_by_every_extractor() {
    let tools = detect_tools();
    let candidates = [
        ("7z", Tools { sevenz: tools.sevenz.clone(), ..Tools::none() }),
        ("unzip", Tools { unzip: tools.unzip.clone(), ..Tools::none() }),
    ];
    let mut ran = 0;
    for (label, t) in candidates {
        if !t.can_extract() {
            eprintln!("note: {label} not installed; skipping that zip-slip variant");
            continue;
        }
        ran += 1;
        let f = fixture();
        write_raw_zip(&f.dl.join("evil.zip"), &[("good.txt", b"fine"), ("../escaped.txt", b"pwned")]);
        // Sanity: our writer produces a valid zip.
        let (r, logs) = run(&f.dl, "job", &PostOptions { cleanup: true, ..Default::default() }, &t);
        match &r.extract {
            Verdict::Failed(reason) => assert!(reason.contains("unsafe path"), "{label}: {reason} {logs:#?}"),
            other => panic!("{label}: expected failure, got {other:?}"),
        }
        assert!(!f.dl.join("escaped.txt").exists(), "{label}: file escaped the target");
        assert!(!f.dl.join("job").join("good.txt").exists(), "{label}: nothing may be extracted");
        assert!(f.dl.join("evil.zip").exists(), "{label}: archive must not be cleaned up");
    }
    if ran == 0 {
        eprintln!("note: no zip extractor installed; zip-slip test did nothing");
    }
}

#[test]
fn raw_zip_writer_produces_valid_archives() {
    let tools = detect_tools();
    require!(tools, sevenz);
    let f = fixture();
    write_raw_zip(&f.dl.join("ok.zip"), &[("dir/good.txt", b"fine")]);
    let (r, logs) = run(&f.dl, "job", &PostOptions::default(), &tools);
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_eq!(fs::read(f.dl.join("job").join("dir").join("good.txt")).unwrap(), b"fine");
}

// ---------------------------------------------------------------- PAR2

/// par2cmdline uses every core with spinning worker threads; several instances at once
/// slow each other down dramatically, so PAR2 tests run one at a time.
static PAR2_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn par2_guard() -> std::sync::MutexGuard<'static, ()> {
    PAR2_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn par2_create(tools: &Tools, dir: &Path, base: &str, files: &[&str]) {
    let mut args = vec!["c", "-q", "-q", "-r25", "-n2"];
    args.push(base);
    args.extend(files);
    sh(tools.par2.as_ref().unwrap(), &args, dir);
}

fn damage(path: &Path, offset: usize, len: usize) {
    let mut d = fs::read(path).unwrap();
    for b in &mut d[offset..offset + len] {
        *b = !*b;
    }
    fs::write(path, d).unwrap();
}

#[test]
fn par2_intact_set_is_ok() {
    let tools = detect_tools();
    require!(tools, par2);
    let _guard = par2_guard();
    let f = fixture();
    fs::write(f.dl.join("data.bin"), pseudo_random(200_000, 11)).unwrap();
    par2_create(&tools, &f.dl, "data.par2", &["data.bin"]);
    let (r, logs) = run(&f.dl, "job", &PostOptions::default(), &tools);
    assert_eq!(r.par2, Verdict::Ok, "{logs:#?}");
}

#[test]
fn par2_repairs_damaged_and_renamed_files() {
    let tools = detect_tools();
    require!(tools, par2);
    let _guard = par2_guard();
    let f = fixture();
    let one = pseudo_random(300_000, 21);
    let two = pseudo_random(120_000, 22);
    fs::write(f.dl.join("one.bin"), &one).unwrap();
    fs::write(f.dl.join("two.bin"), &two).unwrap();
    par2_create(&tools, &f.dl, "set.par2", &["one.bin", "two.bin"]);
    damage(&f.dl.join("one.bin"), 5_000, 4_096);
    // Obfuscated name: par2 must find it by content and rename it back.
    fs::rename(f.dl.join("two.bin"), f.dl.join("x7f3a9c1")).unwrap();

    let (r, logs) = run(&f.dl, "job", &PostOptions::default(), &tools);
    assert_eq!(r.par2, Verdict::Repaired, "{logs:#?}");
    assert_eq!(fs::read(f.dl.join("one.bin")).unwrap(), one);
    assert_eq!(fs::read(f.dl.join("two.bin")).unwrap(), two);
}

#[test]
fn par2_repair_disabled_reports_failure_and_skips_extraction() {
    let tools = detect_tools();
    require!(tools, par2);
    let _guard = par2_guard();
    let f = fixture();
    fs::write(f.dl.join("data.bin"), pseudo_random(200_000, 31)).unwrap();
    par2_create(&tools, &f.dl, "data.par2", &["data.bin"]);
    damage(&f.dl.join("data.bin"), 1_000, 100);
    let opts = PostOptions { repair: false, ..Default::default() };
    let (r, _) = run(&f.dl, "job", &opts, &tools);
    assert!(r.par2.is_failed(), "{:?}", r.par2);
    assert!(matches!(r.extract, Verdict::Skipped(ref s) if s.contains("PAR2")));
}

#[test]
fn par2_not_repairable_fails() {
    let tools = detect_tools();
    require!(tools, par2);
    let _guard = par2_guard();
    let f = fixture();
    fs::write(f.dl.join("data.bin"), pseudo_random(200_000, 41)).unwrap();
    par2_create(&tools, &f.dl, "data.par2", &["data.bin"]);
    // Destroy far more than the 25% redundancy.
    damage(&f.dl.join("data.bin"), 0, 150_000);
    let (r, logs) = run(&f.dl, "job", &PostOptions::default(), &tools);
    match &r.par2 {
        Verdict::Failed(reason) => assert!(reason.contains("repair not possible"), "{reason} {logs:#?}"),
        other => panic!("expected failure, got {other:?}"),
    }
}

#[test]
fn par2_repair_then_extract_then_cleanup() {
    let tools = detect_tools();
    require!(tools, par2, sevenz);
    let _guard = par2_guard();
    let f = fixture();
    let zip = f.dl.join("release.zip");
    sh(tools.sevenz.as_ref().unwrap(), &["a", "-tzip", "-mx0", zip.to_str().unwrap(), "a.txt", &sub_path("b.bin")], &f.src);
    par2_create(&tools, &f.dl, "release.par2", &["release.zip"]);
    let par2_files: Vec<PathBuf> = {
        let mut v: Vec<PathBuf> = fs::read_dir(&f.dl)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "par2"))
            .collect();
        v.sort();
        v
    };
    assert!(par2_files.len() >= 2);
    damage(&zip, 20_000, 2_000);
    // A file unrelated to the archive set must survive cleanup.
    fs::write(f.dl.join("info.nfo"), b"release notes").unwrap();

    let opts = PostOptions { cleanup: true, ..Default::default() };
    let (r, logs) = run(&f.dl, "Release", &opts, &tools);
    assert_eq!(r.par2, Verdict::Repaired, "{logs:#?}");
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_payload_extracted(&f.dl.join("Release"), &f.src);

    let mut expected = par2_files.clone();
    expected.push(zip.clone());
    expected.push(f.dl.join("release.zip.1")); // par2's backup of the damaged copy
    expected.sort();
    assert_eq!(r.removed, expected, "{logs:#?}");
    for p in &expected {
        assert!(!p.exists(), "{} should be gone", p.display());
    }
    assert!(f.dl.join("info.nfo").exists());
}
