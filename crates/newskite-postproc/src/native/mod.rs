//! In-process archive extraction (zip, 7z, RAR) into a private staging folder.
//!
//! Every backend lists the archive first and refuses it when any entry is unsafe
//! ([`is_unsafe_entry`]), then streams each entry through [`Sink`], which maps the entry
//! name to a sanitized path below the staging folder, creates folders one component at a
//! time without following links, writes files with `create_new` (an existing file of the
//! same name is unlinked first, never written through) and reports progress. Symbolic
//! links, junctions and hard links stored in an archive are never created: they are
//! skipped and logged, and one that points outside the extraction folder (absolute or
//! climbing out with `..`) reports the set as failed.

mod rar;
mod sevenz;
mod zipfile;

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use crate::detect::{classify_name, ArchiveKind, ArchiveSet};
use crate::extract::is_unsafe_entry;

/// Reason shown when an encrypted archive is met without a password.
pub(crate) const PASSWORD_REQUIRED: &str = "password required: the archive is encrypted; set a password for this job";
/// Reason shown when the job's password does not decrypt the archive.
pub(crate) const WRONG_PASSWORD: &str = "wrong password";

/// Why a native backend could not extract a set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NativeError {
    /// The archive uses a feature the backend does not implement (an unknown codec, a
    /// spanned zip ...). The only error for which an external tool is tried instead.
    Unsupported(String),
    PasswordRequired,
    WrongPassword,
    MissingVolume,
    Corrupt(String),
    /// An entry path is absolute or climbs out with `..`.
    UnsafePath(String),
    /// Writing the output failed.
    Io(String),
}

impl std::fmt::Display for NativeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NativeError::Unsupported(r) => write!(f, "unsupported archive: {r}"),
            NativeError::PasswordRequired => f.write_str(PASSWORD_REQUIRED),
            NativeError::WrongPassword => f.write_str(WRONG_PASSWORD),
            NativeError::MissingVolume => f.write_str("missing or incomplete archive volume"),
            NativeError::Corrupt(d) if d.is_empty() => f.write_str("archive is corrupt or damaged"),
            NativeError::Corrupt(d) => write!(f, "archive is corrupt or damaged ({d})"),
            NativeError::UnsafePath(p) => write!(f, "archive contains an unsafe path ({p})"),
            NativeError::Io(m) => f.write_str(m),
        }
    }
}

impl NativeError {
    /// Output error with a short reason (disk full is spelled out).
    pub(crate) fn output(what: &Path, e: &io::Error) -> Self {
        if e.kind() == io::ErrorKind::StorageFull || e.raw_os_error() == Some(28) {
            return NativeError::Io("not enough disk space".into());
        }
        NativeError::Io(format!("could not write {}: {e}", what.display()))
    }
}

/// What a successful native run produced.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Summary {
    pub files: u64,
    pub bytes: u64,
    /// Link entries skipped because they stay inside the extraction folder.
    pub links_skipped: u64,
    /// Name of the first link entry pointing outside the extraction folder (skipped too,
    /// but the set is reported as failed).
    pub escaping_link: Option<String>,
}

/// Extract `set` natively into `staging` (an empty folder).
pub(crate) fn extract(
    set: &ArchiveSet,
    staging: &Path,
    password: Option<&str>,
    log: &mut dyn FnMut(String),
) -> Result<Summary, NativeError> {
    let name = set.first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut sink = Sink::new(staging, &name, log);
    // Decoders run on archive bytes from the network; a bug in one must fail the set,
    // not take the service down with it.
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match set.kind {
        ArchiveKind::Zip => zipfile::extract(set, &mut sink, password),
        ArchiveKind::SevenZip => sevenz::extract(set, &mut sink, password),
        ArchiveKind::Rar => rar::extract(set, &mut sink, password),
    }));
    let result = match run {
        Ok(r) => r,
        Err(_) => Err(NativeError::Corrupt(format!("the {} decoder stopped on malformed data", set.kind.name()))),
    };
    result.map(|()| sink.finish())
}

/// Check every listed entry name before anything is written; an archive without
/// entries is treated as damaged. The entry count and `total` size are logged.
pub(crate) fn check_listing<'n>(
    sink: &mut Sink<'_>,
    names: impl IntoIterator<Item = &'n str>,
    total: u64,
    backend: &str,
) -> Result<(), NativeError> {
    let mut count = 0usize;
    for n in names {
        count += 1;
        if is_unsafe_entry(n) || n.contains('\0') {
            let shown: String = n.chars().take(120).collect();
            sink.log(format!("refusing unsafe entry path {shown:?}"));
            return Err(NativeError::UnsafePath(shown));
        }
    }
    if count == 0 {
        // UnRAR in particular reads junk after a valid signature as an empty archive;
        // succeeding would let cleanup delete it.
        return Err(NativeError::Corrupt("no entries found".into()));
    }
    sink.progress.total = total;
    sink.log(format!("{count} entries, {} via native {backend}", human_bytes(total)));
    Ok(())
}

/// Entry name -> path relative to the staging folder. Both `/` and `\` separate
/// components; empty and `.` components are dropped. `None` for names with nothing left
/// (e.g. `./`). Unsafe names are an error (they were refused at listing already; this is
/// the second line of defense).
pub(crate) fn safe_relative(name: &str) -> Result<Option<PathBuf>, NativeError> {
    if is_unsafe_entry(name) || name.contains('\0') {
        return Err(NativeError::UnsafePath(name.chars().take(120).collect()));
    }
    let mut out = PathBuf::new();
    for c in name.split(['/', '\\']) {
        if c.is_empty() || c == "." {
            continue;
        }
        #[cfg(windows)]
        let c = crate::extraction_dir_name(c);
        out.push(c);
    }
    // Paranoia: whatever the platform made of the components, only normal ones remain.
    if out.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(NativeError::UnsafePath(name.chars().take(120).collect()));
    }
    Ok(if out.as_os_str().is_empty() { None } else { Some(out) })
}

/// True when a link stored at entry `name` with destination `target` would point
/// outside the extraction folder: an absolute destination, or one that climbs above the
/// root lexically.
pub(crate) fn link_escapes(name: &str, target: &str) -> bool {
    if target.is_empty() {
        return false;
    }
    if target.starts_with(['/', '\\']) || (target.len() >= 2 && target.as_bytes()[1] == b':') {
        return true;
    }
    // Depth of the folder holding the link, then walk the destination from there.
    let mut depth: i64 = name.split(['/', '\\']).filter(|c| !c.is_empty() && *c != ".").count() as i64 - 1;
    for c in target.split(['/', '\\']) {
        match c {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            _ => depth += 1,
        }
    }
    false
}

/// Progress reported through the log every 5% (only for sets of at least
/// [`PROGRESS_MIN_TOTAL`] bytes, so small archives stay quiet).
pub(crate) struct Progress {
    pub total: u64,
    pub done: u64,
    next_pct: u64,
}

const PROGRESS_STEP_PCT: u64 = 5;
const PROGRESS_MIN_TOTAL: u64 = 32 * 1024 * 1024;

impl Progress {
    /// Percentage to report after adding `n` bytes, when a new step was reached.
    fn add(&mut self, n: u64) -> Option<u64> {
        self.done = self.done.saturating_add(n);
        if self.total < PROGRESS_MIN_TOTAL {
            return None;
        }
        let pct = (u128::from(self.done.min(self.total)) * 100 / u128::from(self.total)) as u64;
        if pct >= self.next_pct && pct < 100 {
            self.next_pct = (pct / PROGRESS_STEP_PCT + 1) * PROGRESS_STEP_PCT;
            Some(pct)
        } else {
            None
        }
    }
}

/// `1536` -> `1.5 KiB`.
pub(crate) fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} bytes") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// Where a backend writes: a staging folder that only ever contains real folders and
/// regular files created by this sink.
pub(crate) struct Sink<'a> {
    root: PathBuf,
    name: String,
    log: &'a mut dyn FnMut(String),
    pub progress: Progress,
    summary: Summary,
    /// Last folder verified or created (relative), to skip re-checking siblings.
    last_dir: Option<PathBuf>,
}

/// Copy buffer size for entry data.
const CHUNK: usize = 256 * 1024;

/// An entry whose data could not be read (decoder error) or written (output error).
pub(crate) enum CopyError {
    Read(io::Error),
    Write(NativeError),
}

impl<'a> Sink<'a> {
    pub fn new(root: &Path, name: &str, log: &'a mut dyn FnMut(String)) -> Self {
        Self {
            root: root.to_path_buf(),
            name: name.to_string(),
            log,
            progress: Progress { total: 0, done: 0, next_pct: PROGRESS_STEP_PCT },
            summary: Summary::default(),
            last_dir: None,
        }
    }

    /// Log one line prefixed with `extract: <archive>: `.
    pub fn log(&mut self, msg: String) {
        (self.log)(format!("extract: {}: {msg}", self.name));
    }

    /// Count `n` more bytes of output and log a progress line at every 5% step.
    pub fn advance(&mut self, n: u64) {
        if let Some(pct) = self.progress.add(n) {
            let line = format!("{pct}% ({} of {})", human_bytes(self.progress.done), human_bytes(self.progress.total));
            self.log(line);
        }
    }

    fn finish(mut self) -> Summary {
        let s = &self.summary;
        let mut line = format!("wrote {} file(s), {}", s.files, human_bytes(s.bytes));
        if s.links_skipped > 0 {
            line.push_str(&format!("; skipped {} link(s)", s.links_skipped));
        }
        self.log(line);
        self.summary
    }

    /// Create the folder for entry `rel` (relative, sanitized) and every parent, one
    /// component at a time: an existing real folder is reused, a link in the way is
    /// removed (never followed), a file in the way is an error.
    pub fn create_dir(&mut self, rel: &Path) -> Result<(), NativeError> {
        if self.last_dir.as_deref() == Some(rel) {
            return Ok(());
        }
        let mut cur = self.root.clone();
        for c in rel.components() {
            cur.push(c);
            match std::fs::symlink_metadata(&cur) {
                Ok(m) if m.is_dir() => continue,
                Ok(m) if m.file_type().is_symlink() => {
                    // Not created by this sink; never traverse it.
                    remove_non_dir(&cur).map_err(|e| NativeError::output(&cur, &e))?;
                }
                Ok(_) => {
                    return Err(NativeError::Io(format!(
                        "could not create folder {}: a file of that name is in the way",
                        rel_display(&self.root, &cur)
                    )));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(NativeError::output(&cur, &e)),
            }
            match std::fs::create_dir(&cur) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && cur.symlink_metadata().is_ok_and(|m| m.is_dir()) => {}
                Err(e) => return Err(NativeError::output(&cur, &e)),
            }
        }
        self.last_dir = Some(rel.to_path_buf());
        Ok(())
    }

    /// Create a new, empty file for entry `rel`. Its folder is created as by
    /// [`Sink::create_dir`]; an existing file or link of that name (a duplicate entry) is
    /// unlinked first, and the file is opened with `create_new`, so data never goes
    /// through a link or into a file that has another name too.
    pub fn create_file(&mut self, rel: &Path) -> Result<(PathBuf, File), NativeError> {
        if let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
            self.create_dir(parent)?;
        }
        let path = self.root.join(rel);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => {
                return Err(NativeError::Io(format!(
                    "could not write {}: a folder of that name is in the way",
                    rel.display()
                )));
            }
            Ok(_) => remove_non_dir(&path).map_err(|e| NativeError::output(rel, &e))?,
            Err(_) => {}
        }
        let file = OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| NativeError::output(rel, &e))?;
        Ok((path, file))
    }

    /// Stream `reader` into a new file for entry `name`. Returns the bytes written. A
    /// partially written file is removed when reading or writing fails.
    pub fn write_entry(&mut self, rel: &Path, reader: &mut dyn Read) -> Result<(PathBuf, u64), CopyError> {
        let (path, mut file) = self.create_file(rel).map_err(CopyError::Write)?;
        let mut buf = vec![0u8; CHUNK];
        let mut written = 0u64;
        let res = loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => break Err(CopyError::Read(e)),
            };
            if let Err(e) = file.write_all(&buf[..n]) {
                break Err(CopyError::Write(NativeError::output(rel, &e)));
            }
            written += n as u64;
            self.advance(n as u64);
        };
        if let Err(e) = res {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        self.file_done(written);
        Ok((path, written))
    }

    /// Account for one finished file (written by [`Sink::write_entry`] or a backend
    /// that writes through [`Sink::create_file`] itself).
    pub fn file_done(&mut self, bytes: u64) {
        self.summary.files += 1;
        self.summary.bytes += bytes;
    }

    /// A link entry (symbolic link, junction or hard link) at `name` pointing to
    /// `target` (empty when unknown). Never created; one that escapes is remembered.
    pub fn link(&mut self, name: &str, target: &str) {
        let shown: String = name.chars().take(120).collect();
        if link_escapes(name, target) {
            self.log(format!("skipping link {shown:?}: it points outside the extraction folder"));
            self.summary.escaping_link.get_or_insert(shown);
        } else {
            self.log(format!("skipping link {shown:?} (links are not extracted)"));
            self.summary.links_skipped += 1;
        }
    }

    /// Absolute path of a sanitized relative entry path.
    pub fn path_of(&self, rel: &Path) -> PathBuf {
        self.root.join(rel)
    }
}

fn rel_display(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

/// Remove a file or link (directory links on Windows need `remove_dir`).
fn remove_non_dir(p: &Path) -> io::Result<()> {
    std::fs::remove_file(p).or_else(|e| std::fs::remove_dir(p).map_err(|_| e))
}

/// Apply the permission bits stored in an archive (Unix only), keeping the file
/// readable and writable by its owner and dropping set-id/sticky bits.
#[cfg(unix)]
pub(crate) fn apply_mode(path: &Path, mode: Option<u32>) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(m) = mode.filter(|m| m & 0o777 != 0) {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode((m & 0o777) | 0o600));
    }
}

#[cfg(not(unix))]
pub(crate) fn apply_mode(_path: &Path, _mode: Option<u32>) {}

/// Set a file's modification time (best effort).
pub(crate) fn apply_mtime(path: &Path, t: Option<std::time::SystemTime>) {
    if let Some(t) = t {
        if let Ok(f) = OpenOptions::new().write(true).open(path) {
            let _ = f.set_modified(t);
        }
    }
}

/// The volumes of a zip or 7z set in reading order, checked for gaps. A plain `.7z` /
/// `.zip` (or a sniffed file) is read alone; numbered volumes (`.7z.001`, `.zip.001`
/// ...) are chained. Spanned zips (`.z01` ... `.zip`) are [`NativeError::Unsupported`]:
/// their offsets are relative to each disk, so they cannot be read as one stream.
pub(crate) fn chained_volumes(set: &ArchiveSet) -> Result<Vec<PathBuf>, NativeError> {
    if set.volumes.len() <= 1 {
        return Ok(vec![set.first.clone()]);
    }
    let name_of = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let spanned = set.kind == ArchiveKind::Zip
        && set.volumes.iter().any(|v| {
            let n = name_of(v).to_ascii_lowercase();
            n.rsplit_once('.').is_some_and(|(_, ext)| ext.len() >= 2 && ext.starts_with('z') && ext[1..].bytes().all(|b| b.is_ascii_digit()))
        });
    if spanned {
        return Err(NativeError::Unsupported("spanned zip archives (.z01, .z02 ...) are not supported natively".into()));
    }
    let first_order = classify_name(&name_of(&set.first)).map(|c| c.order);
    if first_order == Some(0) {
        // A plain .7z / .zip next to numbered parts of the same name: read it alone.
        return Ok(vec![set.first.clone()]);
    }
    let mut numbered: Vec<(u32, PathBuf)> = set
        .volumes
        .iter()
        .filter_map(|v| classify_name(&name_of(v)).filter(|c| c.order >= 1).map(|c| (c.order, v.clone())))
        .collect();
    numbered.sort();
    for (i, (order, _)) in numbered.iter().enumerate() {
        if *order != i as u32 + 1 {
            return Err(NativeError::MissingVolume);
        }
    }
    if numbered.is_empty() {
        return Ok(vec![set.first.clone()]);
    }
    Ok(numbered.into_iter().map(|(_, p)| p).collect())
}

/// `Read + Seek` over several files as if they were one (split volumes). Opens one
/// volume at a time.
pub(crate) struct VolumeReader {
    /// `(path, start offset, length)` per volume.
    parts: Vec<(PathBuf, u64, u64)>,
    total: u64,
    pos: u64,
    /// Open volume: index and the file's current offset.
    cur: Option<(usize, File, u64)>,
}

impl VolumeReader {
    pub fn open(paths: &[PathBuf]) -> io::Result<Self> {
        let mut parts = Vec::with_capacity(paths.len());
        let mut start = 0u64;
        for p in paths {
            let len = std::fs::metadata(p)?.len();
            parts.push((p.clone(), start, len));
            start += len;
        }
        Ok(Self { parts, total: start, pos: 0, cur: None })
    }
}

/// Backend name for the listing log line, e.g. `7z (3 volumes)`.
pub(crate) fn volume_label(backend: &str, volumes: usize) -> String {
    if volumes > 1 { format!("{backend} ({volumes} volumes)") } else { backend.to_string() }
}

impl Read for VolumeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.total {
            return Ok(0);
        }
        let idx = self.parts.partition_point(|(_, start, len)| start + len <= self.pos);
        let (path, start, len) = &self.parts[idx];
        let within = self.pos - start;
        if self.cur.as_ref().map(|c| c.0) != Some(idx) {
            self.cur = Some((idx, File::open(path)?, 0));
        }
        let (_, file, off) = self.cur.as_mut().expect("volume open");
        if *off != within {
            file.seek(SeekFrom::Start(within))?;
            *off = within;
        }
        let want = buf.len().min((len - within).min(usize::MAX as u64) as usize);
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{} is shorter than expected", path.display())));
        }
        *off += n as u64;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for VolumeReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let new = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.total.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        match new {
            Some(n) => {
                self.pos = n;
                Ok(n)
            }
            None => Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before the start of the archive")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_relative_paths() {
        assert_eq!(safe_relative("a/b.txt").unwrap(), Some(PathBuf::from("a").join("b.txt")));
        assert_eq!(safe_relative("a\\b.txt").unwrap(), Some(PathBuf::from("a").join("b.txt")));
        assert_eq!(safe_relative("./x//y/").unwrap(), Some(PathBuf::from("x").join("y")));
        assert_eq!(safe_relative("./").unwrap(), None);
        for bad in ["../x", "/etc/passwd", "a/../../x", "C:\\x", "a\0b"] {
            assert!(safe_relative(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn link_escape_rules() {
        assert!(link_escapes("link", "/etc"));
        assert!(link_escapes("link", "C:\\Windows"));
        assert!(link_escapes("link", ".."));
        assert!(link_escapes("a/link", "../../x"));
        assert!(link_escapes("a/link", "b/../../../x"));
        assert!(!link_escapes("a/link", "../x"));
        assert!(!link_escapes("a/b/link", "../../x"));
        assert!(!link_escapes("link", "target.txt"));
        assert!(!link_escapes("link", ""));
    }

    #[test]
    fn progress_steps() {
        let mut p = Progress { total: 100 * 1024 * 1024, done: 0, next_pct: PROGRESS_STEP_PCT };
        let mut seen = Vec::new();
        for _ in 0..100 {
            if let Some(pct) = p.add(1024 * 1024) {
                seen.push(pct);
            }
        }
        assert_eq!(seen, (1..20).map(|i| i * 5).collect::<Vec<_>>());
        let mut small = Progress { total: 1000, done: 0, next_pct: PROGRESS_STEP_PCT };
        assert_eq!(small.add(999), None);
    }

    #[test]
    fn human_byte_sizes() {
        assert_eq!(human_bytes(12), "12 bytes");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn volume_reader_chains_and_seeks() {
        let dir = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
        let mut paths = Vec::new();
        for (i, chunk) in data.chunks(3_000).enumerate() {
            let p = dir.path().join(format!("v.{:03}", i + 1));
            std::fs::write(&p, chunk).unwrap();
            paths.push(p);
        }
        let mut r = VolumeReader::open(&paths).unwrap();
        assert_eq!(paths.len(), 4);
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        assert_eq!(all, data);
        r.seek(SeekFrom::Start(2_990)).unwrap();
        let mut b = [0u8; 20];
        r.read_exact(&mut b).unwrap();
        assert_eq!(&b[..], &data[2_990..3_010]);
        assert_eq!(r.seek(SeekFrom::End(-5)).unwrap(), 9_995);
        let mut tail = Vec::new();
        r.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, &data[9_995..]);
        assert!(r.seek(SeekFrom::Current(-20_000)).is_err());
    }

    fn set(kind: ArchiveKind, names: &[&str]) -> ArchiveSet {
        let volumes: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
        ArchiveSet { kind, first: volumes[0].clone(), volumes }
    }

    #[test]
    fn volume_order_and_gaps() {
        let s = set(ArchiveKind::SevenZip, &["a.7z.001", "a.7z.002", "a.7z.003"]);
        assert_eq!(chained_volumes(&s).unwrap().len(), 3);
        let s = set(ArchiveKind::SevenZip, &["a.7z.001", "a.7z.003"]);
        assert_eq!(chained_volumes(&s), Err(NativeError::MissingVolume));
        let s = set(ArchiveKind::Zip, &["a.zip", "a.z01", "a.z02"]);
        assert!(matches!(chained_volumes(&s), Err(NativeError::Unsupported(_))));
        let s = set(ArchiveKind::Zip, &["a.zip.001", "a.zip.002"]);
        assert_eq!(chained_volumes(&s).unwrap(), vec![PathBuf::from("a.zip.001"), PathBuf::from("a.zip.002")]);
        let s = set(ArchiveKind::SevenZip, &["a.7z", "a.7z.001"]);
        assert_eq!(chained_volumes(&s).unwrap(), vec![PathBuf::from("a.7z")]);
    }
}
