//! Archive and PAR2 set detection by file name and magic bytes.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Archive container format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArchiveKind {
    /// RAR 4 or RAR 5.
    Rar,
    /// 7-Zip.
    SevenZip,
    /// PKZIP.
    Zip,
}

impl ArchiveKind {
    /// Short lowercase name for messages.
    pub fn name(self) -> &'static str {
        match self {
            ArchiveKind::Rar => "rar",
            ArchiveKind::SevenZip => "7z",
            ArchiveKind::Zip => "zip",
        }
    }
}

/// What the first bytes of a file say about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Magic {
    /// A RAR archive. `volume`: part of a multi-volume set; `first`: it is the first volume
    /// (always true for non-volume archives).
    Rar { volume: bool, first: bool },
    SevenZip,
    Zip,
}

impl Magic {
    pub fn kind(self) -> ArchiveKind {
        match self {
            Magic::Rar { .. } => ArchiveKind::Rar,
            Magic::SevenZip => ArchiveKind::SevenZip,
            Magic::Zip => ArchiveKind::Zip,
        }
    }
}

const RAR4_SIG: &[u8] = b"Rar!\x1a\x07\x00";
const RAR5_SIG: &[u8] = b"Rar!\x1a\x07\x01\x00";
const SEVENZ_SIG: &[u8] = b"7z\xbc\xaf\x27\x1c";
const ZIP_SIG: &[u8] = b"PK\x03\x04";

/// Identify an archive from its leading bytes (at least 64 bytes recommended so RAR
/// volume flags can be read).
pub fn sniff_bytes(head: &[u8]) -> Option<Magic> {
    if let Some(rest) = head.strip_prefix(RAR5_SIG) {
        let (volume, first) = rar5_volume_flags(rest).unwrap_or((false, true));
        Some(Magic::Rar { volume, first })
    } else if let Some(rest) = head.strip_prefix(RAR4_SIG) {
        let (volume, first) = rar4_volume_flags(rest).unwrap_or((false, true));
        Some(Magic::Rar { volume, first })
    } else if head.starts_with(SEVENZ_SIG) {
        Some(Magic::SevenZip)
    } else if head.starts_with(ZIP_SIG) {
        Some(Magic::Zip)
    } else {
        None
    }
}

/// Read up to 64 bytes of `path` and sniff them. I/O errors yield `None`.
pub fn sniff_file(path: &Path) -> Option<Magic> {
    let mut f = File::open(path).ok()?;
    let mut buf = [0u8; 64];
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    sniff_bytes(&buf[..n])
}

/// RAR 4 main header: CRC(2) TYPE(1)=0x73 FLAGS(2 LE). 0x0001 = volume,
/// 0x0100 = first volume (RAR 3.0+).
fn rar4_volume_flags(rest: &[u8]) -> Option<(bool, bool)> {
    if rest.len() < 5 || rest[2] != 0x73 {
        return None;
    }
    let flags = u16::from_le_bytes([rest[3], rest[4]]);
    let volume = flags & 0x0001 != 0;
    let first = !volume || flags & 0x0100 != 0;
    Some((volume, first))
}

/// RAR 5 main header: CRC32(4), size vint, type vint (1 = main), header flags vint,
/// [extra size vint], [data size vint], archive flags vint. Archive flag 0x0001 = volume,
/// 0x0002 = volume number present (absent only in the first volume).
fn rar5_volume_flags(rest: &[u8]) -> Option<(bool, bool)> {
    let mut pos = 4usize; // skip CRC32
    let _size = read_vint(rest, &mut pos)?;
    let htype = read_vint(rest, &mut pos)?;
    if htype != 1 {
        return None;
    }
    let hflags = read_vint(rest, &mut pos)?;
    if hflags & 0x01 != 0 {
        read_vint(rest, &mut pos)?;
    }
    if hflags & 0x02 != 0 {
        read_vint(rest, &mut pos)?;
    }
    let aflags = read_vint(rest, &mut pos)?;
    let volume = aflags & 0x0001 != 0;
    let first = !volume || aflags & 0x0002 == 0;
    Some((volume, first))
}

fn read_vint(b: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*pos)?;
        *pos += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// Name-based classification of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NameClass {
    pub kind: ArchiveKind,
    /// Lowercased set key (the name with the volume suffix removed).
    pub key: String,
    /// Volume order inside the set (lower = earlier).
    pub order: u32,
    /// True when this file can be the one handed to the tool (given it has the lowest
    /// order in its set).
    pub can_lead: bool,
}

fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let n = s.len().checked_sub(suffix.len())?;
    if s.is_char_boundary(n) && s[n..].eq_ignore_ascii_case(suffix) {
        Some(&s[..n])
    } else {
        None
    }
}

/// Split `stem.<prefix><digits>` into (stem, digits) when the last extension is
/// `prefix` followed by 1..=4 ASCII digits.
fn numbered_ext<'a>(name: &'a str, prefix: &str) -> Option<(&'a str, u32)> {
    let dot = name.rfind('.')?;
    let ext = &name[dot + 1..];
    let digits = if prefix.is_empty() { ext } else { strip_prefix_ci(ext, prefix)? };
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((&name[..dot], digits.parse().ok()?))
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Classify a file by name. Recognized: `.partNN.rar`, `.rar` + `.rNN`/`.sNN`,
/// `.7z`, `.7z.NNN`, `.zip`, `.zNN`, `.zip.NNN`.
pub(crate) fn classify_name(name: &str) -> Option<NameClass> {
    let mk = |kind, stem: &str, order, can_lead| {
        Some(NameClass { kind, key: stem.to_lowercase(), order, can_lead })
    };
    if let Some(stem) = strip_suffix_ci(name, ".rar") {
        if let Some((base, n)) = numbered_ext(stem, "part") {
            return mk(ArchiveKind::Rar, base, n, n <= 1);
        }
        return mk(ArchiveKind::Rar, stem, 0, true);
    }
    if let Some((stem, n)) = numbered_ext(name, "r") {
        if n < 100 {
            return mk(ArchiveKind::Rar, stem, n + 1, false);
        }
    }
    if let Some((stem, n)) = numbered_ext(name, "s") {
        if n < 100 {
            return mk(ArchiveKind::Rar, stem, n + 101, false);
        }
    }
    if let Some(stem) = strip_suffix_ci(name, ".7z") {
        return mk(ArchiveKind::SevenZip, stem, 0, true);
    }
    if let Some((stem, n)) = numbered_ext(name, "") {
        if let Some(base) = strip_suffix_ci(stem, ".7z") {
            return mk(ArchiveKind::SevenZip, base, n, n <= 1);
        }
        if let Some(base) = strip_suffix_ci(stem, ".zip") {
            return mk(ArchiveKind::Zip, base, n, n <= 1);
        }
    }
    if let Some(stem) = strip_suffix_ci(name, ".zip") {
        // Spanned zip (.z01, .z02, ..., .zip): the .zip is what the tool opens.
        return mk(ArchiveKind::Zip, stem, 0, true);
    }
    if let Some((stem, n)) = numbered_ext(name, "z") {
        if (1..100).contains(&n) {
            return mk(ArchiveKind::Zip, stem, n, false);
        }
    }
    None
}

/// Whether a file with an unrecognized name should be sniffed for archive magic:
/// no extension, or an "extension" that does not look like a real one (obfuscated
/// Usenet posts often use random names). Known document/media types that are zip
/// containers (docx, epub, jar, cbz ...) have normal extensions and are left alone.
fn should_sniff(name: &str) -> bool {
    match name.rfind('.') {
        None => true,
        Some(0) => false, // dotfile
        Some(dot) => {
            let ext = &name[dot + 1..];
            !(1..=5).contains(&ext.len())
                || !ext.bytes().all(|b| b.is_ascii_alphanumeric())
                || !ext.bytes().any(|b| b.is_ascii_alphabetic())
        }
    }
}

/// One archive set ready to extract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveSet {
    /// Format used to choose the tool (from magic bytes when they are recognized,
    /// otherwise from the file name).
    pub kind: ArchiveKind,
    /// The file handed to the tool (first volume).
    pub first: PathBuf,
    /// All files belonging to the set, including `first` (removed on cleanup).
    pub volumes: Vec<PathBuf>,
}

/// A set whose first volume is missing (cannot be extracted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokenSet {
    pub name: String,
    pub volumes: Vec<PathBuf>,
}

/// `(order, can_lead, path)` of one file in a name-based group.
type Member = (u32, bool, PathBuf);

/// Group the regular files of a directory into archive sets.
pub(crate) fn find_archive_sets(files: &[PathBuf]) -> (Vec<ArchiveSet>, Vec<BrokenSet>) {
    let mut groups: BTreeMap<(ArchiveKind, String), Vec<Member>> = BTreeMap::new();
    let mut sets = Vec::new();
    for path in files {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
        if let Some(c) = classify_name(&name) {
            groups.entry((c.kind, c.key)).or_default().push((c.order, c.can_lead, path.clone()));
        } else if should_sniff(&name) {
            match sniff_file(path) {
                Some(Magic::Rar { first: false, .. }) => {
                    // A later volume of an obfuscated multi-volume set; it can only be
                    // reached through its first volume, which unrar finds by name.
                }
                Some(m) => sets.push(ArchiveSet { kind: m.kind(), first: path.clone(), volumes: vec![path.clone()] }),
                None => {}
            }
        }
    }
    let mut broken = Vec::new();
    for ((kind, key), mut members) in groups {
        members.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2)));
        let volumes: Vec<PathBuf> = members.iter().map(|m| m.2.clone()).collect();
        // The lowest-ordered member that may lead is the first volume; a set made only
        // of continuation volumes (.r00, .7z.002, .part02.rar...) lost its first volume.
        match members.iter().find(|m| m.1) {
            Some((_, _, first)) => {
                let kind = sniff_file(first).map(Magic::kind).unwrap_or(kind);
                sets.push(ArchiveSet { kind, first: first.clone(), volumes });
            }
            None => broken.push(BrokenSet { name: format!("{key} ({})", kind.name()), volumes }),
        }
    }
    sets.sort_by(|a, b| a.first.cmp(&b.first));
    (sets, broken)
}

/// One PAR2 set: the main file passed to `par2` and every file of the set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Par2Set {
    pub main: PathBuf,
    pub files: Vec<PathBuf>,
}

/// `name.par2` -> `name`; `name.vol07+08.par2` -> `name`. Returns (key, is_volume).
pub(crate) fn par2_key(name: &str) -> Option<(String, bool)> {
    let stem = strip_suffix_ci(name, ".par2")?;
    if let Some(dot) = stem.rfind('.') {
        let ext = &stem[dot + 1..];
        if let Some(rest) = strip_prefix_ci(ext, "vol") {
            let mut parts = rest.splitn(2, ['+', '-']);
            let a = parts.next().unwrap_or("");
            let b = parts.next().unwrap_or("");
            let num = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
            if num(a) && num(b) {
                return Some((stem[..dot].to_lowercase(), true));
            }
        }
    }
    Some((stem.to_lowercase(), false))
}

/// Group `.par2` files into sets and pick each set's main file (the one without a
/// `.volNN+MM` suffix, else the smallest).
pub(crate) fn find_par2_sets(files: &[PathBuf]) -> Vec<Par2Set> {
    let mut groups: BTreeMap<String, Vec<(bool, u64, PathBuf)>> = BTreeMap::new();
    for p in files {
        let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
        if let Some((key, is_vol)) = par2_key(&name) {
            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(u64::MAX);
            groups.entry(key).or_default().push((is_vol, size, p.clone()));
        }
    }
    groups
        .into_values()
        .filter_map(|mut members| {
            members.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2)));
            let main = members.first()?.2.clone();
            let mut files: Vec<PathBuf> = members.into_iter().map(|m| m.2).collect();
            files.sort();
            Some(Par2Set { main, files })
        })
        .collect()
}

/// True for `*.par2` (any case).
pub(crate) fn is_par2(path: &Path) -> bool {
    path.file_name().map(|n| n.to_string_lossy()).is_some_and(|n| strip_suffix_ci(&n, ".par2").is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_bytes() {
        assert_eq!(sniff_bytes(b"7z\xbc\xaf\x27\x1c\x00\x04"), Some(Magic::SevenZip));
        assert_eq!(sniff_bytes(b"PK\x03\x04rest"), Some(Magic::Zip));
        assert_eq!(sniff_bytes(b"PK\x05\x06"), None);
        assert_eq!(sniff_bytes(b"Rar!\x1a\x07\x00"), Some(Magic::Rar { volume: false, first: true }));
        assert_eq!(sniff_bytes(b"Rar!\x1a\x07\x01\x00"), Some(Magic::Rar { volume: false, first: true }));
        assert_eq!(sniff_bytes(b"hello"), None);
        assert_eq!(sniff_bytes(b""), None);
    }

    #[test]
    fn rar4_volume_flags_parsed() {
        // CRC(2) type 0x73 flags 0x0101 (volume + first volume)
        let mut b = RAR4_SIG.to_vec();
        b.extend_from_slice(&[0, 0, 0x73, 0x01, 0x01, 13, 0]);
        assert_eq!(sniff_bytes(&b), Some(Magic::Rar { volume: true, first: true }));
        let mut b = RAR4_SIG.to_vec();
        b.extend_from_slice(&[0, 0, 0x73, 0x01, 0x00, 13, 0]);
        assert_eq!(sniff_bytes(&b), Some(Magic::Rar { volume: true, first: false }));
    }

    #[test]
    fn rar5_volume_flags_parsed() {
        // CRC32, size, type=1, hflags=0, aflags
        let mk = |aflags: u8, volnum: Option<u8>| {
            let mut b = RAR5_SIG.to_vec();
            b.extend_from_slice(&[0, 0, 0, 0, 5, 1, 0, aflags]);
            if let Some(v) = volnum {
                b.push(v);
            }
            b
        };
        assert_eq!(sniff_bytes(&mk(0, None)), Some(Magic::Rar { volume: false, first: true }));
        assert_eq!(sniff_bytes(&mk(1, None)), Some(Magic::Rar { volume: true, first: true }));
        assert_eq!(sniff_bytes(&mk(3, Some(2))), Some(Magic::Rar { volume: true, first: false }));
    }

    #[test]
    fn vint_multibyte() {
        let mut pos = 0;
        assert_eq!(read_vint(&[0x81, 0x01], &mut pos), Some(129));
        assert_eq!(pos, 2);
        let mut pos = 0;
        assert_eq!(read_vint(&[0x81], &mut pos), None);
    }

    #[test]
    fn name_classes() {
        let c = classify_name("Movie.Part01.RAR").unwrap();
        assert_eq!((c.kind, c.key.as_str(), c.order, c.can_lead), (ArchiveKind::Rar, "movie", 1, true));
        let c = classify_name("movie.rar").unwrap();
        assert_eq!((c.key.as_str(), c.order), ("movie", 0));
        let c = classify_name("movie.r00").unwrap();
        assert_eq!((c.key.as_str(), c.order, c.can_lead), ("movie", 1, false));
        let c = classify_name("movie.s02").unwrap();
        assert_eq!(c.order, 103);
        let c = classify_name("x.7z.001").unwrap();
        assert_eq!((c.kind, c.key.as_str(), c.order, c.can_lead), (ArchiveKind::SevenZip, "x", 1, true));
        let c = classify_name("x.7z.002").unwrap();
        assert!(!c.can_lead);
        let c = classify_name("x.zip.001").unwrap();
        assert_eq!((c.kind, c.can_lead), (ArchiveKind::Zip, true));
        let c = classify_name("x.z01").unwrap();
        assert_eq!((c.kind, c.can_lead), (ArchiveKind::Zip, false));
        assert_eq!(classify_name("movie.mkv"), None);
        assert_eq!(classify_name("movie.mkv.001"), None);
        assert_eq!(classify_name("readme.txt"), None);
        assert_eq!(classify_name("a.r"), None);
    }

    #[test]
    fn sniff_policy() {
        assert!(should_sniff("a8f7e6d5c4b3a2918"));
        assert!(should_sniff("release.a1b2c3d4e5f6"));
        assert!(should_sniff("file.123"));
        assert!(!should_sniff("book.epub"));
        assert!(!should_sniff("doc.docx"));
        assert!(!should_sniff(".hidden"));
    }

    #[test]
    fn par2_keys() {
        assert_eq!(par2_key("Set.par2"), Some(("set".into(), false)));
        assert_eq!(par2_key("Set.vol000+200.PAR2"), Some(("set".into(), true)));
        assert_eq!(par2_key("Set.vol07-08.par2"), Some(("set".into(), true)));
        assert_eq!(par2_key("a.volume.par2"), Some(("a.volume".into(), false)));
        assert_eq!(par2_key("a.rar"), None);
    }

    #[test]
    fn grouping_picks_first_volume_and_reports_broken() {
        let files: Vec<PathBuf> = [
            "/nonexistent/r.part02.rar",
            "/nonexistent/r.part01.rar",
            "/nonexistent/r.part03.rar",
            "/nonexistent/old.r00",
            "/nonexistent/old.rar",
            "/nonexistent/old.r01",
            "/nonexistent/s.7z.002",
            "/nonexistent/s.7z.001",
            "/nonexistent/lost.part02.rar",
            "/nonexistent/movie.mkv",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let (sets, broken) = find_archive_sets(&files);
        let firsts: Vec<_> = sets.iter().map(|s| s.first.to_string_lossy().into_owned()).collect();
        assert_eq!(firsts, vec!["/nonexistent/old.rar", "/nonexistent/r.part01.rar", "/nonexistent/s.7z.001"]);
        assert_eq!(sets[0].volumes.len(), 3);
        assert_eq!(sets[1].volumes.len(), 3);
        assert_eq!(sets[2].volumes.len(), 2);
        assert_eq!(broken.len(), 1);
        assert!(broken[0].name.starts_with("lost"));
    }

    #[test]
    fn par2_main_selection() {
        let files: Vec<PathBuf> =
            ["/nx/a.vol01+02.par2", "/nx/a.par2", "/nx/b.vol00+01.par2", "/nx/b.vol01+02.par2"].iter().map(PathBuf::from).collect();
        let sets = find_par2_sets(&files);
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].main, PathBuf::from("/nx/a.par2"));
        assert_eq!(sets[0].files.len(), 2);
        // No plain .par2 for b: sizes unknown (both u64::MAX) -> name order.
        assert_eq!(sets[1].main, PathBuf::from("/nx/b.vol00+01.par2"));
    }
}
