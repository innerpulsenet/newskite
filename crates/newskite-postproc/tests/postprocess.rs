//! Integration tests for post-processing, which runs entirely in-process. Archives are
//! built in-test with the `zip` and `sevenz-rust2` writers (spanned zips are split by hand)
//! or taken from small RAR fixtures, and PAR2 sets are made with [`newskite_par2::create`].
//! A few tests check archives and PAR2 sets made by other programs (7-Zip, Info-ZIP `zip`,
//! par2cmdline): they use the program only to *create* the fixture and print a note and
//! return early when it is not installed.
//!
//! `tests/fixtures/rar/*.rar` come from the `unrar` crate's test data (MIT OR
//! Apache-2.0); no RAR writer is available, so RAR is tested with those.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use newskite_postproc::{process, PostOptions, PostReport, Verdict};

/// A program used only to create fixtures, looked up on `PATH` under the first of `names`
/// found. `None` (after printing a note) when none is installed.
fn fixture_tool(names: &[&str]) -> Option<PathBuf> {
    let dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    let found = names
        .iter()
        .find_map(|n| dirs.iter().flat_map(|d| [d.join(n), d.join(format!("{n}.exe"))]).find(|p| p.is_file()));
    if found.is_none() {
        eprintln!("note: skipping test, `{}` is not installed", names[0]);
    }
    found
}

/// The fixture program's path, or return from the test when it is not installed.
macro_rules! tool_or_skip {
    ($($name:expr),+) => {
        match fixture_tool(&[$($name),+]) {
            Some(path) => path,
            None => return,
        }
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

fn run(dir: &Path, job: &str, opts: &PostOptions) -> (PostReport, Vec<String>) {
    let mut logs = Vec::new();
    let report = process(dir, job, opts, &mut |l| logs.push(l)).expect("process");
    (report, logs)
}

fn with_password(pw: &str) -> PostOptions {
    PostOptions { password: Some(pw.into()), ..Default::default() }
}

fn assert_payload_extracted(target: &Path, src: &Path) {
    assert_eq!(fs::read(target.join("a.txt")).unwrap(), fs::read(src.join("a.txt")).unwrap());
    assert_eq!(
        fs::read(target.join("sub").join("b.bin")).unwrap(),
        fs::read(src.join("sub").join("b.bin")).unwrap()
    );
}

fn assert_failed_with(r: &Verdict, needle: &str, logs: &[String]) {
    match r {
        Verdict::Failed(reason) => assert!(reason.contains(needle), "{reason:?} lacks {needle:?}: {logs:#?}"),
        other => panic!("expected a failure containing {needle:?}, got {other:?}: {logs:#?}"),
    }
}

fn sub_path(rel: &str) -> String {
    Path::new("sub").join(rel).to_string_lossy().into_owned()
}

/// The fixture payload (`a.txt`, `sub/b.bin`) plus a `sub/` folder entry as a zip.
fn payload_zip(f: &Fixture, path: &Path, password: Option<&str>) {
    let mut w = zip::ZipWriter::new(fs::File::create(path).unwrap());
    let mut opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    if let Some(pw) = password {
        opts = opts.with_aes_encryption(zip::AesMode::Aes256, pw);
    }
    w.start_file("a.txt", opts).unwrap();
    w.write_all(&fs::read(f.src.join("a.txt")).unwrap()).unwrap();
    w.add_directory("sub/", opts).unwrap();
    w.start_file("sub/b.bin", opts.compression_method(zip::CompressionMethod::Stored)).unwrap();
    w.write_all(&fs::read(f.src.join("sub").join("b.bin")).unwrap()).unwrap();
    w.finish().unwrap();
}

/// The fixture payload as a 7z (LZMA2), optionally AES-encrypted.
fn payload_7z(f: &Fixture, out: impl Write + std::io::Seek, password: Option<&str>, encrypt_header: bool) {
    let mut w = sevenz_rust2::ArchiveWriter::new(out).unwrap();
    if let Some(pw) = password {
        w.set_content_methods(vec![
            sevenz_rust2::encoder_options::AesEncoderOptions::new(pw.into()).into(),
            sevenz_rust2::EncoderMethod::LZMA2.into(),
        ]);
        w.set_encrypt_header(encrypt_header);
    }
    w.push_archive_entry::<&[u8]>(sevenz_rust2::ArchiveEntry::new_directory("sub"), None).unwrap();
    for rel in ["a.txt", "sub/b.bin"] {
        let data = fs::read(f.src.join(rel)).unwrap();
        w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file(rel), Some(&data[..])).unwrap();
    }
    w.finish().unwrap();
}

/// Split `data` into `name.001`, `name.002` ... of `chunk` bytes in `dir`.
fn split_volumes(dir: &Path, name: &str, data: &[u8], chunk: usize) -> Vec<PathBuf> {
    data.chunks(chunk)
        .enumerate()
        .map(|(i, c)| {
            let p = dir.join(format!("{name}.{:03}", i + 1));
            fs::write(&p, c).unwrap();
            p
        })
        .collect()
}

// ---------------------------------------------------------------- native zip

#[test]
fn zip_extracts_natively_into_job_folder() {
    let f = fixture();
    let zip = f.dl.join("release.zip");
    payload_zip(&f, &zip, None);

    let (r, logs) = run(&f.dl, "My Release: 2024", &PostOptions::default());
    assert_eq!(r.par2, Verdict::Skipped("no PAR2 files".into()));
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    let target = f.dl.join("My Release_ 2024");
    assert_eq!(r.extracted_to.as_deref(), Some(target.as_path()));
    assert_payload_extracted(&target, &f.src);
    assert!(logs.iter().any(|l| l.contains("via native zip")), "{logs:#?}");
    // cleanup is on by default
    assert!(!zip.exists());
    assert_eq!(r.removed, vec![zip.clone()]);
}

#[test]
fn aes_zip_right_wrong_and_missing_password() {
    let f = fixture();
    let zip = f.dl.join("secret.zip");
    payload_zip(&f, &zip, Some("Correct Horse"));

    let (r, logs) = run(&f.dl, "job", &with_password("wrong one"));
    assert_eq!(r.extract, Verdict::Failed("wrong password".into()), "{logs:#?}");
    assert!(logs.iter().all(|l| !l.contains("wrong one")), "password leaked into logs");

    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_failed_with(&r.extract, "password required", &logs);

    let (r, logs) = run(&f.dl, "job", &with_password("Correct Horse"));
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert!(logs.iter().all(|l| !l.contains("Correct Horse")), "password leaked into logs");
    assert_payload_extracted(&f.dl.join("job"), &f.src);
    assert!(!zip.exists());
}

#[test]
fn password_set_for_unencrypted_zip_is_ignored() {
    let f = fixture();
    payload_zip(&f, &f.dl.join("plain.zip"), None);
    let (r, logs) = run(&f.dl, "job", &with_password("unused"));
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
}

#[test]
fn split_zip_volumes_are_chained() {
    let f = fixture();
    let whole = f.src.join("whole.zip");
    payload_zip(&f, &whole, None);
    let volumes = split_volumes(&f.dl, "big.zip", &fs::read(&whole).unwrap(), 40_000);
    assert!(volumes.len() >= 3);

    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_payload_extracted(&f.dl.join("job"), &f.src);
    assert!(logs.iter().any(|l| l.contains(&format!("zip ({} volumes)", volumes.len()))), "{logs:#?}");
    assert_eq!(r.removed, volumes);
}

#[test]
fn corrupt_zip_fails() {
    let f = fixture();
    let mut data = b"PK\x03\x04".to_vec();
    data.extend(pseudo_random(5000, 9));
    fs::write(f.dl.join("broken.zip"), data).unwrap();
    let (r, logs) = run(&f.dl, "job", &PostOptions { cleanup: true, ..Default::default() });
    assert_failed_with(&r.extract, "corrupt", &logs);
    assert!(f.dl.join("broken.zip").exists());
    assert!(!f.dl.join("job").exists(), "empty target must be removed");
}

#[test]
fn damaged_zip_data_fails_and_leaves_no_partial_file() {
    let f = fixture();
    let zip = f.dl.join("release.zip");
    payload_zip(&f, &zip, None);
    let mut d = fs::read(&zip).unwrap();
    // Inside the stored sub/b.bin data: the CRC check must catch it.
    let at = d.len() / 2;
    for b in &mut d[at..at + 64] {
        *b = !*b;
    }
    fs::write(&zip, d).unwrap();
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_failed_with(&r.extract, "corrupt", &logs);
    assert!(!f.dl.join("job").join("sub").join("b.bin").exists(), "{logs:#?}");
    assert!(zip.exists());
}

#[test]
fn large_set_reports_progress() {
    let f = fixture();
    let zip = f.dl.join("zeros.zip");
    let mut w = zip::ZipWriter::new(fs::File::create(&zip).unwrap());
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    w.start_file("zeros.bin", opts).unwrap();
    let block = vec![0u8; 1024 * 1024];
    for _ in 0..40 {
        w.write_all(&block).unwrap();
    }
    w.finish().unwrap();
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    let steps: Vec<_> = logs.iter().filter(|l| l.contains("% (")).collect();
    assert!(steps.len() >= 10, "{logs:#?}");
    assert!(steps.iter().any(|l| l.contains(": 50% (") && l.ends_with("of 40.0 MiB)")), "{steps:#?}");
    assert_eq!(fs::metadata(f.dl.join("job").join("zeros.bin")).unwrap().len(), 40 * 1024 * 1024);
}

// ---------------------------------------------------------------- spanned zip

/// Turn a single-disk zip (as written by `zip::ZipWriter`: no comment, no ZIP64) into a
/// spanned set in `dir` laid out like `zip -s` output: `name.z01`, `name.z02` ... of
/// `chunk` bytes, then `name.zip` with the rest and the end record. Disk 0 starts with the
/// `PK\x07\x08` split marker, and every offset is relative to the disk it falls on.
/// Returns the volumes in disk order.
fn split_spanned(dir: &Path, name: &str, zip: &[u8], chunk: usize) -> Vec<PathBuf> {
    let le16 = |b: &[u8], at: usize| usize::from(u16::from_le_bytes([b[at], b[at + 1]]));
    let le32 = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize;
    let end_at = zip.len() - 22;
    assert_eq!(&zip[end_at..end_at + 4], b"PK\x05\x06", "unexpected end record");
    let (cd_size, cd_start) = (le32(zip, end_at + 12), le32(zip, end_at + 16));
    // (disk, offset on that disk) of a position in `zip`, which moves by the marker.
    let place = |pos: usize| ((pos + 4) / chunk, (pos + 4) % chunk);
    let mut cd = zip[cd_start..cd_start + cd_size].to_vec();
    let mut p = 0;
    while p < cd.len() {
        assert_eq!(&cd[p..p + 4], b"PK\x01\x02");
        let (disk, offset) = place(le32(&cd, p + 42));
        cd[p + 34..p + 36].copy_from_slice(&(disk as u16).to_le_bytes());
        cd[p + 42..p + 46].copy_from_slice(&(offset as u32).to_le_bytes());
        p += 46 + le16(&cd, p + 28) + le16(&cd, p + 30) + le16(&cd, p + 32);
    }
    let mut stream = b"PK\x07\x08".to_vec();
    stream.extend_from_slice(&zip[..cd_start]);
    stream.extend_from_slice(&cd);
    let mut disks: Vec<Vec<u8>> = stream.chunks(chunk).map(<[u8]>::to_vec).collect();
    let mut end = zip[end_at..].to_vec();
    let (cd_disk, cd_offset) = place(cd_start);
    end[4..6].copy_from_slice(&((disks.len() - 1) as u16).to_le_bytes());
    end[6..8].copy_from_slice(&(cd_disk as u16).to_le_bytes());
    end[16..20].copy_from_slice(&(cd_offset as u32).to_le_bytes());
    disks.last_mut().unwrap().extend_from_slice(&end);
    let count = disks.len();
    disks
        .into_iter()
        .enumerate()
        .map(|(i, data)| {
            let p = if i + 1 == count { dir.join(format!("{name}.zip")) } else { dir.join(format!("{name}.z{:02}", i + 1)) };
            fs::write(&p, data).unwrap();
            p
        })
        .collect()
}

/// The smallest volume size from `min` on for which the central directory of `zip`
/// (split as by [`split_spanned`]) crosses from one volume into the next.
fn chunk_splitting_central_directory(zip: &[u8], min: usize) -> usize {
    let end_at = zip.len() - 22;
    let le32 = |at: usize| u32::from_le_bytes(zip[at..at + 4].try_into().unwrap()) as usize;
    let (first, last) = (le32(end_at + 16) + 4, le32(end_at + 16) + le32(end_at + 12) + 3);
    (min..).find(|n| first / n != last / n).unwrap()
}

/// The fixture payload plus a few more entries, as a spanned set of `span.*` volumes in
/// `f.dl` whose central directory crosses a volume boundary.
fn spanned_payload(f: &Fixture, password: Option<&str>) -> Vec<PathBuf> {
    let whole = f.src.join("whole.zip");
    payload_zip(f, &whole, password);
    let zip = fs::read(&whole).unwrap();
    let chunk = chunk_splitting_central_directory(&zip, 30_000);
    let volumes = split_spanned(&f.dl, "span", &zip, chunk);
    assert!(volumes.len() >= 4, "{volumes:?}");
    volumes
}

#[test]
fn spanned_zip_extracts_natively() {
    let f = fixture();
    let volumes = spanned_payload(&f, None);
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_payload_extracted(&f.dl.join("job"), &f.src);
    assert!(logs.iter().any(|l| l.contains(&format!("via native zip ({} spanned volumes)", volumes.len()))), "{logs:#?}");
    assert_eq!(r.removed, volumes);
}

#[test]
fn aes_spanned_zip_right_wrong_and_missing_password() {
    let f = fixture();
    let volumes = spanned_payload(&f, Some("Correct Horse"));
    let (r, logs) = run(&f.dl, "job", &with_password("wrong one"));
    assert_eq!(r.extract, Verdict::Failed("wrong password".into()), "{logs:#?}");
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_failed_with(&r.extract, "password required", &logs);
    let (r, logs) = run(&f.dl, "job", &with_password("Correct Horse"));
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert!(logs.iter().all(|l| !l.contains("Correct Horse")), "password leaked into logs");
    assert_payload_extracted(&f.dl.join("job"), &f.src);
    assert_eq!(r.removed, volumes);
}

#[test]
fn spanned_zip_with_missing_volume_fails_without_cleanup() {
    // A gap in the numbering, and a missing last `.zNN` (no gap, but the end record
    // counts one disk more).
    for missing in ["span.z02", "LAST"] {
        let f = fixture();
        let volumes = spanned_payload(&f, None);
        let gone = if missing == "LAST" { volumes[volumes.len() - 2].clone() } else { f.dl.join(missing) };
        fs::remove_file(&gone).unwrap();
        let (r, logs) = run(&f.dl, "job", &PostOptions { cleanup: true, ..Default::default() });
        assert_failed_with(&r.extract, "missing or incomplete archive volume", &logs);
        assert!(r.removed.is_empty(), "{missing}");
        assert!(volumes.iter().filter(|v| **v != gone).all(|v| v.exists()), "{missing}");
        assert!(!f.dl.join("job").exists(), "{missing}: {logs:#?}");
    }
}

#[test]
fn damaged_spanned_zip_end_record_fails() {
    let f = fixture();
    let volumes = spanned_payload(&f, None);
    let last = volumes.last().unwrap();
    let mut d = fs::read(last).unwrap();
    let n = d.len();
    // The central directory offset now points past the archive data.
    d[n - 6..n - 2].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(last, d).unwrap();
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_failed_with(&r.extract, "corrupt", &logs);
    assert!(r.removed.is_empty());
}

/// Spanned sets made by Info-ZIP (`zip -s`), stored and deflated, with several entries
/// one of which crosses volume boundaries.
#[test]
fn spanned_zips_made_by_info_zip_extract_natively() {
    let zip_tool = tool_or_skip!("zip");
    for method in ["-0", "-6"] {
        let f = fixture();
        fs::write(f.src.join("c.txt"), "compressible text\n".repeat(5_000)).unwrap();
        fs::write(f.src.join("sub").join("d.bin"), pseudo_random(90_000, 13)).unwrap();
        let archive = f.dl.join("span.zip");
        sh(&zip_tool, &["-q", "-r", method, "-s", "64k", archive.to_str().unwrap(), "a.txt", "c.txt", "sub"], &f.src);
        let mut volumes: Vec<PathBuf> = fs::read_dir(&f.dl).unwrap().map(|e| e.unwrap().path()).collect();
        volumes.sort();
        assert!(volumes.len() >= 4 && f.dl.join("span.z03").exists(), "{method}: {volumes:?}");

        let (r, logs) = run(&f.dl, "job", &PostOptions::default());
        assert_eq!(r.extract, Verdict::Ok, "{method}: {logs:#?}");
        let job = f.dl.join("job");
        assert_payload_extracted(&job, &f.src);
        for rel in ["c.txt", "sub/d.bin"] {
            assert_eq!(fs::read(job.join(rel)).unwrap(), fs::read(f.src.join(rel)).unwrap(), "{method}: {rel}");
        }
        assert_eq!(r.removed, volumes, "{method}");
    }
}

#[test]
fn passworded_spanned_zip_made_by_info_zip() {
    let zip_tool = tool_or_skip!("zip");
    let f = fixture();
    let archive = f.dl.join("span.zip");
    // ZipCrypto, the only encryption Info-ZIP writes.
    sh(&zip_tool, &["-q", "-r", "-P", "Correct Horse", "-s", "64k", archive.to_str().unwrap(), "a.txt", "sub"], &f.src);
    assert!(f.dl.join("span.z02").exists());
    let (r, logs) = run(&f.dl, "job", &with_password("wrong one"));
    assert_eq!(r.extract, Verdict::Failed("wrong password".into()), "{logs:#?}");
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_failed_with(&r.extract, "password required", &logs);
    let (r, logs) = run(&f.dl, "job", &with_password("Correct Horse"));
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_payload_extracted(&f.dl.join("job"), &f.src);
}

// ---------------------------------------------------------------- native 7z

#[test]
fn sevenz_extracts_natively() {
    let f = fixture();
    let archive = f.dl.join("payload.7z");
    payload_7z(&f, fs::File::create(&archive).unwrap(), None, false);
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_payload_extracted(&f.dl.join("job"), &f.src);
    assert!(logs.iter().any(|l| l.contains("via native 7z")), "{logs:#?}");
    assert_eq!(r.removed, vec![archive]);
}

#[test]
fn encrypted_7z_right_wrong_and_missing_password_natively() {
    for header_encryption in [true, false] {
        let f = fixture();
        let archive = f.dl.join("secret.7z");
        payload_7z(&f, fs::File::create(&archive).unwrap(), Some("Correct Horse"), header_encryption);

        let (r, logs) = run(&f.dl, "job", &with_password("wrong one"));
        assert_eq!(r.extract, Verdict::Failed("wrong password".into()), "hdr={header_encryption} {logs:#?}");
        assert!(logs.iter().all(|l| !l.contains("wrong one")), "password leaked into logs");

        let (r, logs) = run(&f.dl, "job", &PostOptions::default());
        assert_failed_with(&r.extract, "password required", &logs);

        let (r, logs) = run(&f.dl, "job", &with_password("Correct Horse"));
        assert_eq!(r.extract, Verdict::Ok, "hdr={header_encryption} {logs:#?}");
        assert!(logs.iter().all(|l| !l.contains("Correct Horse")), "password leaked into logs");
        assert_payload_extracted(&f.dl.join("job"), &f.src);
        assert_eq!(r.removed, vec![archive.clone()]);
    }
}

#[test]
fn multi_volume_7z_is_chained_and_cleaned_up() {
    let f = fixture();
    let mut whole = std::io::Cursor::new(Vec::new());
    payload_7z(&f, &mut whole, None, false);
    // LZMA2 barely shrinks random data, so this gives several volumes.
    let volumes = split_volumes(&f.dl, "big.7z", whole.get_ref(), 40_000);
    assert!(volumes.len() >= 3, "{volumes:?}");

    let (r, logs) = run(&f.dl, "Big Job", &PostOptions::default());
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_payload_extracted(&f.dl.join("Big Job"), &f.src);
    assert!(logs.iter().any(|l| l.contains(&format!("7z ({} volumes)", volumes.len()))), "{logs:#?}");
    assert_eq!(r.removed, volumes);
}

#[test]
fn missing_middle_volume_fails_without_cleanup() {
    let f = fixture();
    let mut whole = std::io::Cursor::new(Vec::new());
    payload_7z(&f, &mut whole, None, false);
    split_volumes(&f.dl, "big.7z", whole.get_ref(), 40_000);
    fs::remove_file(f.dl.join("big.7z.002")).unwrap();
    let (r, logs) = run(&f.dl, "job", &PostOptions { cleanup: true, ..Default::default() });
    assert_failed_with(&r.extract, "missing or incomplete archive volume", &logs);
    assert!(r.removed.is_empty());
    assert!(f.dl.join("big.7z.001").exists());
}

#[test]
fn missing_last_volume_fails() {
    let f = fixture();
    let mut whole = std::io::Cursor::new(Vec::new());
    payload_7z(&f, &mut whole, None, false);
    let volumes = split_volumes(&f.dl, "big.7z", whole.get_ref(), 40_000);
    fs::remove_file(volumes.last().unwrap()).unwrap();
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_failed_with(&r.extract, "missing or incomplete archive volume", &logs);
    assert!(r.removed.is_empty());
}

#[test]
fn missing_first_volume_is_reported() {
    let f = fixture();
    let mut whole = std::io::Cursor::new(Vec::new());
    payload_7z(&f, &mut whole, None, false);
    split_volumes(&f.dl, "big.7z", whole.get_ref(), 40_000);
    fs::remove_file(f.dl.join("big.7z.001")).unwrap();
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_failed_with(&r.extract, "first volume missing", &logs);
}

#[test]
fn obfuscated_archive_detected_by_magic() {
    let f = fixture();
    payload_7z(&f, fs::File::create(f.dl.join("a8f7e6d5c4b3a2918f7e")).unwrap(), None, false);
    // A real document that happens to be a zip container must not be touched.
    let docx = f.dl.join("notes.docx");
    payload_zip(&f, &docx, None);
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_payload_extracted(&f.dl.join("job"), &f.src);
    assert!(logs.iter().all(|l| !l.contains("notes.docx")), "{logs:#?}");
    assert!(docx.exists());
}

// ---------------------------------------------------------------- native RAR

fn rar_fixture(f: &Fixture, name: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rar").join(name);
    let dst = f.dl.join(name);
    fs::copy(src, &dst).unwrap();
    dst
}

#[test]
fn rar4_and_rar5_extract_natively() {
    let f = fixture();
    let v = rar_fixture(&f, "version.rar"); // RAR 1.5 format
    rar_fixture(&f, "solid.rar"); // RAR 5, solid
    rar_fixture(&f, "unicode.rar"); // RAR 5, non-ASCII entry name
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    let job = f.dl.join("job");
    assert_eq!(fs::read_to_string(job.join("VERSION")).unwrap(), "unrar-0.4.0");
    assert_eq!(fs::read_to_string(job.join(".gitignore")).unwrap(), "target\nCargo.lock\n");
    assert!(fs::read_dir(&job).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains('✌')), "{logs:#?}");
    assert!(logs.iter().any(|l| l.contains("via native rar")), "{logs:#?}");
    assert!(r.removed.contains(&v));
}

#[test]
fn encrypted_rar_right_wrong_and_missing_password() {
    // crypted.rar: RAR 1.5 format, data encrypted ("unrar"); comment-hpw-password.rar:
    // RAR 5 with encrypted headers ("password").
    for (file, pw) in [("crypted.rar", "unrar"), ("comment-hpw-password.rar", "password")] {
        let f = fixture();
        let archive = rar_fixture(&f, file);

        let (r, logs) = run(&f.dl, "job", &with_password("not it"));
        assert_eq!(r.extract, Verdict::Failed("wrong password".into()), "{file}: {logs:#?}");
        assert!(logs.iter().all(|l| !l.contains("not it")), "password leaked into logs");
        assert!(!f.dl.join("job").join(".gitignore").exists(), "{file}: no output for a wrong password");

        let (r, logs) = run(&f.dl, "job", &PostOptions::default());
        assert_failed_with(&r.extract, "password required", &logs);

        let (r, logs) = run(&f.dl, "job", &with_password(pw));
        assert_eq!(r.extract, Verdict::Ok, "{file}: {logs:#?}");
        // (The second fixture's file name contains its password.)
        assert!(logs.iter().all(|l| !l.replace(file, "").contains(pw)), "password leaked into logs");
        assert_eq!(fs::read_to_string(f.dl.join("job").join(".gitignore")).unwrap(), "target\nCargo.lock\n");
        assert_eq!(r.removed, vec![archive]);
    }
}

#[test]
fn rar_with_missing_volume_fails_without_output() {
    let f = fixture();
    let first = rar_fixture(&f, "archive.part1.rar");
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_failed_with(&r.extract, "missing or incomplete archive volume", &logs);
    assert!(r.removed.is_empty());
    assert!(first.exists());
    // Listing walks all volumes first, so nothing was written.
    assert!(!f.dl.join("job").exists(), "{logs:#?}");
}

#[test]
fn junk_rar_fails() {
    let f = fixture();
    fs::write(f.dl.join("x.par2"), b"PAR2\0PKT").unwrap();
    fs::write(f.dl.join("x.rar"), b"Rar!\x1a\x07\x01\x00junk").unwrap();
    let (r, logs) = run(&f.dl, "job", &PostOptions { cleanup: true, ..Default::default() });
    assert_eq!(r.par2, Verdict::Skipped("PAR2 files unreadable".into()));
    assert!(r.extract.is_failed(), "{logs:#?}");
    assert!(r.removed.is_empty());
    assert!(f.dl.join("x.rar").exists() && f.dl.join("x.par2").exists());
    assert!(!f.dl.join("job").exists());
}

// ---------------------------------------------------------------- general

#[test]
fn no_archives_and_no_par2_is_skipped() {
    let f = fixture();
    fs::write(f.dl.join("movie.mkv"), pseudo_random(10_000, 3)).unwrap();
    fs::write(f.dl.join("movie.mkv.nkpart"), b"partial").unwrap();
    let opts = PostOptions { cleanup: true, ..Default::default() };
    let (r, _) = run(&f.dl, "job", &opts);
    assert_eq!(r.par2, Verdict::Skipped("no PAR2 files".into()));
    assert_eq!(r.extract, Verdict::Skipped("no archives found".into()));
    assert_eq!(r.extracted_to, None);
    assert!(r.removed.is_empty());
    assert!(!f.dl.join("job").exists());
}

#[test]
fn extraction_disabled_is_skipped() {
    let f = fixture();
    fs::write(f.dl.join("x.zip"), b"PK\x03\x04").unwrap();
    let opts = PostOptions { extract: false, ..Default::default() };
    let (r, _) = run(&f.dl, "job", &opts);
    assert_eq!(r.extract, Verdict::Skipped("extraction disabled".into()));
}

// ------------------------------------------------------- archives made by 7-Zip

#[test]
fn archives_made_by_7zip_extract_natively() {
    let seven = tool_or_skip!("7z", "7zz", "7za");
    let sub = sub_path("b.bin");
    let cases: [(&str, &[&str]); 5] = [
        ("deflate.zip", &["-tzip"]),
        ("bzip2.zip", &["-tzip", "-mm=BZip2"]),
        ("lzma.7z", &["-t7z", "-m0=LZMA"]),
        ("ppmd.7z", &["-t7z", "-m0=PPMd"]),
        ("solid.7z", &["-t7z", "-ms=on", "-mx9"]),
    ];
    for (name, switches) in cases {
        let f = fixture();
        let archive = f.dl.join(name);
        let mut args = vec!["a"];
        args.extend_from_slice(switches);
        args.extend([archive.to_str().unwrap(), "a.txt", sub.as_str()]);
        sh(&seven, &args, &f.src);
        let (r, logs) = run(&f.dl, "job", &PostOptions::default());
        assert_eq!(r.extract, Verdict::Ok, "{name}: {logs:#?}");
        assert_payload_extracted(&f.dl.join("job"), &f.src);
    }
}

#[test]
fn passworded_archives_made_by_7zip() {
    let seven = tool_or_skip!("7z", "7zz", "7za");
    let sub = sub_path("b.bin");
    // 7-Zip writes ZipCrypto for zip by default and AES for 7z; -mhe hides the names.
    for (name, extra) in [("zc.zip", "-tzip"), ("aes.zip", "-mem=AES256"), ("hdr.7z", "-mhe=on"), ("data.7z", "-t7z")] {
        let f = fixture();
        let archive = f.dl.join(name);
        let mut args = vec!["a", "-pCorrect Horse", extra];
        if name == "aes.zip" {
            args.insert(1, "-tzip");
        }
        args.extend([archive.to_str().unwrap(), "a.txt", sub.as_str()]);
        sh(&seven, &args, &f.src);

        let (r, logs) = run(&f.dl, "job", &with_password("wrong one"));
        assert_eq!(r.extract, Verdict::Failed("wrong password".into()), "{name}: {logs:#?}");
        let (r, logs) = run(&f.dl, "job", &PostOptions::default());
        assert_failed_with(&r.extract, "password required", &logs);
        let (r, logs) = run(&f.dl, "job", &with_password("Correct Horse"));
        assert_eq!(r.extract, Verdict::Ok, "{name}: {logs:#?}");
        assert_payload_extracted(&f.dl.join("job"), &f.src);
    }
}

#[test]
fn volumes_made_by_7zip_are_chained() {
    let seven = tool_or_skip!("7z", "7zz", "7za");
    let f = fixture();
    let base = f.dl.join("big.7z");
    sh(&seven, &["a", "-mx0", "-v40k", base.to_str().unwrap(), "a.txt", &sub_path("b.bin")], &f.src);
    let mut volumes: Vec<PathBuf> = fs::read_dir(&f.dl).unwrap().map(|e| e.unwrap().path()).collect();
    volumes.sort();
    assert!(volumes.len() >= 3, "expected several volumes, got {volumes:?}");
    let (r, logs) = run(&f.dl, "Big Job", &PostOptions::default());
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_payload_extracted(&f.dl.join("Big Job"), &f.src);
    assert_eq!(r.removed, volumes);
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
    let with_modes: Vec<(&str, &[u8], u32)> = entries.iter().map(|(n, d)| (*n, *d, 0)).collect();
    write_raw_zip_modes(path, &with_modes);
}

/// Like [`write_raw_zip`], with a Unix mode per entry (0 = none). A symbolic link is an
/// entry with mode `S_IFLNK | perms` whose data is the link destination.
fn write_raw_zip_modes(path: &Path, entries: &[(&str, &[u8], u32)]) {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, mode) in entries {
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
        // version made by: 2.0, host 3 (Unix) when a mode is given so readers honor it
        central.extend(if *mode != 0 { 0x0314u16 } else { 20u16 }.to_le_bytes());
        central.extend(&hdr[4..]); // version needed .. extra len (26 bytes)
        central.extend(0u16.to_le_bytes()); // comment len
        central.extend(0u16.to_le_bytes()); // disk
        central.extend(0u16.to_le_bytes()); // internal attrs
        central.extend((mode << 16).to_le_bytes()); // external attrs (Unix mode in the high half)
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
fn raw_zip_writer_produces_valid_archives() {
    let f = fixture();
    write_raw_zip(&f.dl.join("ok.zip"), &[("dir/good.txt", b"fine")]);
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_eq!(fs::read(f.dl.join("job").join("dir").join("good.txt")).unwrap(), b"fine");
}

#[test]
fn zip_slip_archive_is_refused() {
    for evil in ["../escaped.txt", "/tmp/nk-escaped.txt", "a/../../escaped.txt", "..\\escaped.txt"] {
        let f = fixture();
        write_raw_zip(&f.dl.join("evil.zip"), &[("good.txt", b"fine"), (evil, b"pwned")]);
        let (r, logs) = run(&f.dl, "job", &PostOptions { cleanup: true, ..Default::default() });
        assert_failed_with(&r.extract, "unsafe path", &logs);
        assert!(!f.dl.join("escaped.txt").exists(), "{evil}: file escaped the target");
        assert!(!f.dl.join("job").join("good.txt").exists(), "{evil}: nothing may be extracted");
        assert!(f.dl.join("evil.zip").exists(), "{evil}: archive must not be cleaned up");
    }
}

const S_IFLNK: u32 = 0o120_000;

/// Link entries are never created. A harmless one is skipped and the set succeeds.
#[test]
fn internal_link_entry_is_skipped() {
    let f = fixture();
    write_raw_zip_modes(
        &f.dl.join("a.zip"),
        &[("real.txt", b"data", 0o100_644), ("alias.txt", b"real.txt", S_IFLNK | 0o777)],
    );
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    let target = f.dl.join("job");
    assert_eq!(fs::read(target.join("real.txt")).unwrap(), b"data");
    assert!(fs::symlink_metadata(target.join("alias.txt")).is_err(), "link must not be created");
    assert!(logs.iter().any(|l| l.contains("skipping link") && l.contains("alias.txt")), "{logs:#?}");
}

/// One set carries a link pointing outside the target and an entry below it; a second
/// set, or a retry, then writes `link/pwn.txt`. No file may land outside the target.
#[cfg(unix)]
#[test]
fn escaping_link_from_failed_set_is_never_written_through() {
    for link_dest in ["ABS", "../../outside"] {
        let f = fixture();
        let outside = f.dl.parent().unwrap().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let dest = if link_dest == "ABS" { outside.to_str().unwrap().to_string() } else { link_dest.to_string() };
        write_raw_zip_modes(
            &f.dl.join("a.zip"),
            &[("link", dest.as_bytes(), S_IFLNK | 0o777), ("link/x", b"x", 0o100_644)],
        );
        write_raw_zip(&f.dl.join("b.zip"), &[("link/pwn.txt", b"owned")]);
        let (r, logs) = run(&f.dl, "job", &PostOptions::default());
        assert!(!outside.join("pwn.txt").exists(), "{link_dest}: file written outside the target {logs:#?}");
        assert!(!outside.join("x").exists(), "{link_dest}: file written outside the target {logs:#?}");
        assert_failed_with(&r.extract, "link pointing outside", &logs);
        let target = f.dl.join("job");
        assert!(!fs::symlink_metadata(target.join("link")).unwrap().file_type().is_symlink(), "{link_dest}: link created");

        // Retry (or a later run) with an escaping link already in the target folder:
        // it must be replaced, never followed.
        let _ = fs::remove_file(target.join("link"));
        let _ = fs::remove_dir_all(target.join("link"));
        std::os::unix::fs::symlink(&outside, target.join("link")).unwrap();
        fs::remove_file(f.dl.join("a.zip")).unwrap();
        let (_r, logs) = run(&f.dl, "job", &PostOptions::default());
        assert!(!outside.join("pwn.txt").exists(), "{link_dest} retry: file written outside the target {logs:#?}");
        assert_eq!(fs::read(target.join("link").join("pwn.txt")).unwrap(), b"owned", "{link_dest} retry {logs:#?}");
        // No staging folders are left behind.
        let leftovers: Vec<_> = fs::read_dir(&target)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".newskite"))
            .collect();
        assert!(leftovers.is_empty(), "staging folder left behind: {leftovers:?}");
    }
}

// ---------------------------------------------------------------- PAR2

/// A PAR2 set `<base>.par2` + `<base>.vol00+NN.par2` for `files` in `dir`, made natively:
/// 4 KiB slices and `recovery` recovery slices. Returns the PAR2 files.
fn par2_create(dir: &Path, base: &str, files: &[&str], recovery: u32) -> Vec<PathBuf> {
    let mut made = newskite_par2::create(dir, files, 4096, recovery, &dir.join(base)).unwrap();
    made.sort();
    made
}

/// A PAR2 set made by par2cmdline (`base` names the main file), with 25% redundancy.
fn par2cmdline_create(par2: &Path, dir: &Path, base: &str, files: &[&str]) {
    let mut args = vec!["c", "-q", "-q", "-r25", "-n2"];
    args.push(base);
    args.extend(files);
    sh(par2, &args, dir);
}

/// par2cmdline uses every core with spinning worker threads; several instances at once
/// slow each other down dramatically, so the tests that run it go one at a time.
static PAR2CMDLINE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn par2cmdline_guard() -> std::sync::MutexGuard<'static, ()> {
    PAR2CMDLINE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
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
    let f = fixture();
    fs::write(f.dl.join("data.bin"), pseudo_random(200_000, 11)).unwrap();
    par2_create(&f.dl, "data", &["data.bin"], 10);
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_eq!(r.par2, Verdict::Ok, "{logs:#?}");
}

/// Sets made by par2cmdline are verified and repaired natively too.
#[test]
fn par2cmdline_sets_are_verified_and_repaired() {
    let par2 = tool_or_skip!("par2");
    let _guard = par2cmdline_guard();
    let f = fixture();
    let one = pseudo_random(300_000, 21);
    let two = pseudo_random(120_000, 22);
    fs::write(f.dl.join("one.bin"), &one).unwrap();
    fs::write(f.dl.join("two.bin"), &two).unwrap();
    par2cmdline_create(&par2, &f.dl, "set.par2", &["one.bin", "two.bin"]);
    let (r, logs) = run(&f.dl, "job", &PostOptions { remove_leftovers: false, ..Default::default() });
    assert_eq!(r.par2, Verdict::Ok, "{logs:#?}");

    damage(&f.dl.join("one.bin"), 5_000, 4_096);
    // Obfuscated name: found by content and renamed back.
    fs::rename(f.dl.join("two.bin"), f.dl.join("x7f3a9c1")).unwrap();
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_eq!(r.par2, Verdict::Repaired, "{logs:#?}");
    assert_eq!(fs::read(f.dl.join("one.bin")).unwrap(), one);
    assert_eq!(fs::read(f.dl.join("two.bin")).unwrap(), two);
}

#[test]
fn par2_repairs_damaged_and_renamed_files() {
    let f = fixture();
    let one = pseudo_random(300_000, 21);
    let two = pseudo_random(120_000, 22);
    fs::write(f.dl.join("one.bin"), &one).unwrap();
    fs::write(f.dl.join("two.bin"), &two).unwrap();
    par2_create(&f.dl, "set", &["one.bin", "two.bin"], 10);
    damage(&f.dl.join("one.bin"), 5_000, 4_096);
    // Obfuscated name: found by content and renamed back.
    fs::rename(f.dl.join("two.bin"), f.dl.join("x7f3a9c1")).unwrap();

    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_eq!(r.par2, Verdict::Repaired, "{logs:#?}");
    assert_eq!(fs::read(f.dl.join("one.bin")).unwrap(), one);
    assert_eq!(fs::read(f.dl.join("two.bin")).unwrap(), two);
}

#[test]
fn par2_repair_disabled_reports_failure_and_skips_extraction() {
    let f = fixture();
    fs::write(f.dl.join("data.bin"), pseudo_random(200_000, 31)).unwrap();
    par2_create(&f.dl, "data", &["data.bin"], 10);
    damage(&f.dl.join("data.bin"), 1_000, 100);
    let opts = PostOptions { repair: false, ..Default::default() };
    let (r, _) = run(&f.dl, "job", &opts);
    assert!(r.par2.is_failed(), "{:?}", r.par2);
    assert!(matches!(r.extract, Verdict::Skipped(ref s) if s.contains("PAR2")));
}

#[test]
fn par2_not_repairable_fails() {
    let f = fixture();
    fs::write(f.dl.join("data.bin"), pseudo_random(200_000, 41)).unwrap();
    par2_create(&f.dl, "data", &["data.bin"], 10);
    // Destroy far more than the 10 recovery slices can rebuild.
    damage(&f.dl.join("data.bin"), 0, 150_000);
    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    match &r.par2 {
        Verdict::Failed(reason) => assert!(reason.contains("repair not possible"), "{reason} {logs:#?}"),
        other => panic!("expected failure, got {other:?}"),
    }
}

#[test]
fn par2_repair_then_extract_then_cleanup() {
    let f = fixture();
    let zip = f.dl.join("release.zip");
    payload_zip(&f, &zip, None);
    let par2_files = par2_create(&f.dl, "release", &["release.zip"], 4);
    assert_eq!(par2_files.len(), 2);
    // Inside the stored sub/b.bin data.
    damage(&zip, 20_000, 2_000);
    // Release sidecars go with the archives; an unarchived payload file stays.
    fs::write(f.dl.join("info.nfo"), b"release notes").unwrap();
    fs::write(f.dl.join("extra.mkv"), b"payload").unwrap();

    let opts = PostOptions { cleanup: true, ..Default::default() };
    let (r, logs) = run(&f.dl, "Release", &opts);
    assert_eq!(r.par2, Verdict::Repaired, "{logs:#?}");
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    assert_payload_extracted(&f.dl.join("Release"), &f.src);

    let mut expected = par2_files.clone();
    expected.push(zip.clone());
    expected.push(f.dl.join("release.zip.1")); // the repair's backup of the damaged copy
    expected.push(f.dl.join("info.nfo"));
    expected.sort();
    assert_eq!(r.removed, expected, "{logs:#?}");
    for p in &expected {
        assert!(!p.exists(), "{} should be gone", p.display());
    }
    assert!(f.dl.join("extra.mkv").exists());
}

#[test]
fn fully_obfuscated_release_is_renamed_by_par2_then_extracted() {
    // Mirrors common posts: every file (archive volumes AND the PAR2 files) has a random
    // name with no extension. The PAR2 files are found by their packets and named, the
    // volumes are matched by content and renamed back.
    let f = fixture();
    let mut whole = std::io::Cursor::new(Vec::new());
    payload_7z(&f, &mut whole, None, false);
    split_volumes(&f.dl, "release.7z", whole.get_ref(), 60_000);
    let volumes: Vec<String> = {
        let mut v: Vec<String> = fs::read_dir(&f.dl)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    assert!(volumes.len() >= 3, "{volumes:?}");
    let refs: Vec<&str> = volumes.iter().map(String::as_str).collect();
    par2_create(&f.dl, "release", &refs, 4);

    let mut all: Vec<PathBuf> = fs::read_dir(&f.dl).unwrap().map(|e| e.unwrap().path()).collect();
    all.sort();
    for (i, p) in all.iter().enumerate() {
        fs::rename(p, f.dl.join(format!("Qx{i:02}obf9ZkL{}", i * 7))).unwrap();
    }

    let (r, logs) = run(&f.dl, "Release", &PostOptions::default());
    assert_eq!(r.par2, Verdict::Repaired, "{logs:#?}");
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    // Every volume was renamed back to its real name (then cleanup removed it).
    for v in &volumes {
        assert!(r.removed.contains(&f.dl.join(v)), "{v} should be renamed back: {logs:#?}");
    }
    assert_payload_extracted(&f.dl.join("Release"), &f.src);
    // Cleanup is on by default: only the extracted folder remains.
    let left: Vec<String> = fs::read_dir(&f.dl).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, vec!["Release".to_string()], "{logs:#?}");
}

#[test]
fn cleanup_removes_sidecars_but_keeps_unarchived_payload() {
    let f = fixture();
    let zip = f.dl.join("release.zip");
    payload_zip(&f, &zip, None);
    par2_create(&f.dl, "release", &["release.zip"], 2);
    fs::write(f.dl.join("release.sfv"), b"release.zip 00000000\n").unwrap();
    fs::write(f.dl.join("GROUP.NFO"), b"info").unwrap();
    fs::write(f.dl.join("sample.mkv"), b"unarchived payload").unwrap();

    let (r, logs) = run(&f.dl, "Release", &PostOptions::default());
    assert_eq!(r.extract, Verdict::Ok, "{logs:#?}");
    let mut left: Vec<String> = fs::read_dir(&f.dl).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(left, vec!["Release".to_string(), "sample.mkv".to_string()], "{logs:#?}");
    assert!(logs.iter().any(|l| l.contains("kept 1 other file")), "{logs:#?}");
}

#[test]
fn obfuscated_target_in_subfolder_is_renamed_into_it() {
    // Releases often carry Sample/<name>-sample.mkv in the PAR2 set; the folder is created
    // when the file is renamed back into it.
    let f = fixture();
    let main = pseudo_random(200_000, 51);
    let sample = pseudo_random(60_000, 52);
    fs::create_dir(f.dl.join("Sample")).unwrap();
    fs::write(f.dl.join("main.bin"), &main).unwrap();
    fs::write(f.dl.join("Sample").join("s.mkv"), &sample).unwrap();
    par2_create(&f.dl, "set", &["main.bin", "Sample/s.mkv"], 4);
    fs::rename(f.dl.join("main.bin"), f.dl.join("obfA")).unwrap();
    fs::rename(f.dl.join("Sample").join("s.mkv"), f.dl.join("obfB")).unwrap();
    fs::remove_dir(f.dl.join("Sample")).unwrap();

    let (r, logs) = run(&f.dl, "job", &PostOptions::default());
    assert_eq!(r.par2, Verdict::Repaired, "{logs:#?}");
    assert_eq!(fs::read(f.dl.join("main.bin")).unwrap(), main);
    assert_eq!(fs::read(f.dl.join("Sample").join("s.mkv")).unwrap(), sample);
}
