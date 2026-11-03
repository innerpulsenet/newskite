//! The repair outlook: how many PAR2 input slices the job's missing and corrupt articles
//! damaged, against how many recovery blocks can still arrive (see [`RepairOutlook`]).
//!
//! * **Identifying files.** A file is PAR2 when its NZB or yEnc name ends in `.par2`, its
//!   first bytes are a PAR2 packet header, or it was read as PAR2. It is a data file of a
//!   recovery set when its name is one the set protects, or when its length and the MD5 of
//!   its first 16 KiB match one (obfuscated posts). It is "other" once its first article
//!   shows neither, and unidentified before that.
//! * **Damage.** Every article of a data file that is missing or failed its CRC. Articles
//!   of one post hold the same number of bytes (checked against every part's offset), so
//!   each covers a known byte range and the damaged slices are counted exactly. Until that
//!   length is known a partly missing file counts nothing; a fully missing one counts all
//!   its slices. Every shortcut undercounts, never overcounts.
//! * **Recovery.** A finished PAR2 file is read and its blocks for each set counted. One
//!   still downloading counts at most its `.volNN+MM` block count and at most as many
//!   blocks as its intact articles can hold (a block is the slice plus a 68-byte packet
//!   header); an unidentified file counts the same size bound for every set, since it may
//!   be PAR2. So `available` only ever overstates, and `needed > available` is final.
//!
//! The outlook reported is that of the set closest to (or furthest past) running out.

use std::collections::{HashMap, HashSet};

use md5::{Digest, Md5};
use newskite_par2::RecoverySet;
use newskite_types::RepairOutlook;

/// First bytes of every PAR2 packet.
pub(crate) const PAR2_MAGIC: &[u8; 8] = b"PAR2\0PKT";
/// PAR2 identifies a file by the MD5 of this many leading bytes (or all of a shorter file).
const HEAD_BYTES: u64 = 16 * 1024;
/// Header and exponent of a recovery packet, on top of its slice.
const PACKET_OVERHEAD: u64 = 68;

type Id = [u8; 16];

/// What the first bytes of a file say about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub magic: bool,
    /// MD5 of the first 16 KiB, when the bytes given cover them.
    pub md5_16k: Option<Id>,
}

impl Head {
    /// `data` starts at offset 0 of a file of `size` bytes.
    pub(crate) fn of(data: &[u8], size: u64) -> Self {
        let want = HEAD_BYTES.min(size) as usize;
        Self {
            magic: data.starts_with(PAR2_MAGIC),
            md5_16k: (data.len() >= want).then(|| Md5::digest(&data[..want]).into()),
        }
    }
}

struct Set {
    id: Id,
    slice: u64,
    blocks: u64,
    /// Lowercased base name -> length.
    by_name: HashMap<String, u64>,
    by_head: HashSet<(Id, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Unknown,
    Par2,
    Data { set: usize, len: u64 },
    Other,
}

struct FileInfo {
    /// NZB-derived name.
    label: String,
    /// Segment numbers and encoded sizes, in segment order.
    numbers: Vec<u32>,
    encoded: Vec<u64>,
    /// yEnc `name=` and `size=` of the file's parts.
    yenc: Option<(String, u64)>,
    head: Option<Head>,
    /// Bytes per article (every article but the last), from a decoded part; 0 = unknown.
    part_len: u64,
    /// A part's offset did not follow from `part_len` and its number.
    irregular: bool,
    damaged: Vec<bool>,
    damaged_count: usize,
    /// Recovery blocks per set, once read as PAR2.
    read: Option<HashMap<Id, u64>>,
    /// Damaged slice indices for (length, slice size), kept up to date as articles fail.
    slices: Option<(u64, u64, HashSet<u64>)>,
}

impl FileInfo {
    fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.label.as_str()).chain(self.yenc.as_ref().map(|(n, _)| n.as_str()))
    }

    /// Encoded bytes of the articles that arrived intact or are still to come.
    fn intact_encoded(&self) -> u64 {
        self.encoded.iter().zip(&self.damaged).filter(|(_, &d)| !d).map(|(&b, _)| b).fold(0, u64::saturating_add)
    }

    fn total_encoded(&self) -> u64 {
        self.encoded.iter().fold(0, |a, &b| a.saturating_add(b))
    }

    /// Byte range of article `si` when the article length is known.
    fn range(&self, si: usize, len: u64) -> Option<(u64, u64)> {
        let n = u64::from(*self.numbers.get(si)?).checked_sub(1)?;
        let start = n.checked_mul(self.part_len)?;
        (start < len).then(|| (start, (start + self.part_len).min(len)))
    }

    fn add_slices(&mut self, si: usize) {
        let Some((len, slice, _)) = self.slices else { return };
        if let Some((start, end)) = self.range(si, len) {
            if let Some((_, _, set)) = self.slices.as_mut() {
                set.extend(start / slice..=(end - 1) / slice);
            }
        }
    }

    /// Slices of this file the damage touches, for a file of `len` bytes cut in `slice`s.
    fn damaged_slices(&mut self, len: u64, slice: u64) -> u64 {
        let total = len.div_ceil(slice);
        if self.damaged_count == 0 {
            return 0;
        }
        if self.damaged_count == self.damaged.len() {
            return total;
        }
        if self.part_len == 0 || self.irregular {
            return 0;
        }
        if !matches!(self.slices, Some((l, s, _)) if l == len && s == slice) {
            self.slices = Some((len, slice, HashSet::new()));
            for si in 0..self.damaged.len() {
                if self.damaged[si] {
                    self.add_slices(si);
                }
            }
        }
        self.slices.as_ref().map_or(0, |(_, _, s)| s.len() as u64).min(total)
    }
}

pub(crate) fn is_par2_name(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".par2")
}

/// The block count in a `name.vol012+08.par2` style name.
pub(crate) fn vol_blocks(name: &str) -> Option<u64> {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".par2")?;
    let tail = &stem[stem.rfind(".vol")? + 4..];
    let (first, count) = tail.split_once(['+', '-'])?;
    first.parse::<u64>().ok()?;
    count.parse().ok()
}

fn base_name(name: &str) -> String {
    name.rsplit(['/', '\\']).next().unwrap_or(name).to_lowercase()
}

/// Tracks one job's repair outlook as its articles arrive.
pub(crate) struct Tracker {
    files: Vec<FileInfo>,
    sets: Vec<Set>,
    /// The outlook last reported.
    pub last: Option<RepairOutlook>,
}

impl Tracker {
    /// `files`: NZB name, segment numbers and encoded segment sizes (0 for unknown sizes is
    /// replaced by `unknown_bytes`, an overestimate), in NZB order.
    pub(crate) fn new(files: Vec<(String, Vec<u32>, Vec<u64>)>, unknown_bytes: u64) -> Self {
        let files = files
            .into_iter()
            .map(|(label, numbers, encoded)| {
                let n = numbers.len();
                FileInfo {
                    label,
                    numbers,
                    encoded: encoded.into_iter().map(|b| if b > 0 { b } else { unknown_bytes }).collect(),
                    yenc: None,
                    head: None,
                    part_len: 0,
                    irregular: false,
                    damaged: vec![false; n],
                    damaged_count: 0,
                    read: None,
                    slices: None,
                }
            })
            .collect();
        Self { files, sets: Vec::new(), last: None }
    }

    /// A decoded part of file `fi`, article `si`: its yEnc `name=` and `size=` and the
    /// 1-based `(begin, end)` it covers. `head` is given for the part at offset 0. Returns
    /// whether anything the outlook depends on changed.
    pub(crate) fn note_part(&mut self, fi: usize, si: usize, name: &str, size: u64, (begin, end): (u64, u64), head: Option<Head>) -> bool {
        let Some(f) = self.files.get_mut(fi) else { return false };
        let mut changed = false;
        if f.yenc.is_none() {
            f.yenc = Some((name.to_string(), size));
            changed = true;
        }
        if f.head.is_none() && head.is_some() {
            f.head = head;
            changed = true;
        }
        if f.part_len == 0 && end < size && end >= begin {
            f.part_len = end - begin + 1;
            f.slices = None;
            changed = true;
        }
        if f.part_len > 0 && !f.irregular {
            let expected = f.numbers.get(si).and_then(|&n| u64::from(n).checked_sub(1)).map(|n| n * f.part_len + 1);
            if expected != Some(begin) {
                f.irregular = true;
                f.slices = None;
                changed = true;
            }
        }
        changed
    }

    /// File `fi` was found complete on disk from an earlier run.
    pub(crate) fn note_complete(&mut self, fi: usize, len: u64, head: Option<Head>) {
        if let Some(f) = self.files.get_mut(fi) {
            f.yenc.get_or_insert_with(|| (f.label.clone(), len));
            f.head = f.head.or(head);
        }
    }

    /// Article `si` of file `fi` is missing or failed its CRC.
    pub(crate) fn note_damaged(&mut self, fi: usize, si: usize) -> bool {
        let Some(f) = self.files.get_mut(fi) else { return false };
        match f.damaged.get_mut(si) {
            Some(d) if !*d => *d = true,
            _ => return false,
        }
        f.damaged_count += 1;
        f.add_slices(si);
        true
    }

    /// Whether file `fi` is (or may be) PAR2 and is worth reading once complete.
    pub(crate) fn is_par2(&self, fi: usize) -> bool {
        self.files.get(fi).is_some_and(|f| f.names().any(is_par2_name) || f.head.is_some_and(|h| h.magic))
    }

    /// File `fi` was read as PAR2: `sets` are the recovery sets found in it.
    pub(crate) fn note_par2(&mut self, fi: usize, sets: &[RecoverySet]) {
        for s in sets {
            if self.sets.iter().any(|k| k.id == s.id) || s.files.is_empty() {
                continue;
            }
            self.sets.push(Set {
                id: s.id,
                slice: s.slice_size.max(1),
                blocks: s.slice_count() as u64,
                by_name: s.files.iter().map(|e| (base_name(&e.name), e.length)).collect(),
                by_head: s.files.iter().map(|e| (e.md5_16k, e.length)).collect(),
            });
        }
        if let Some(f) = self.files.get_mut(fi) {
            f.read = Some(sets.iter().map(|s| (s.id, s.recovery.len() as u64)).collect());
        }
    }

    fn role(&self, f: &FileInfo) -> Role {
        if f.read.is_some() || f.names().any(is_par2_name) || f.head.is_some_and(|h| h.magic) {
            return Role::Par2;
        }
        for (i, set) in self.sets.iter().enumerate() {
            if let Some(&len) = f.names().find_map(|n| set.by_name.get(&base_name(n))) {
                return Role::Data { set: i, len };
            }
        }
        if let (Some(Head { md5_16k: Some(md5), .. }), Some((_, size))) = (f.head, &f.yenc) {
            if let Some(i) = self.sets.iter().position(|s| s.by_head.contains(&(md5, *size))) {
                return Role::Data { set: i, len: *size };
            }
        }
        if f.head.is_some() {
            Role::Other
        } else {
            Role::Unknown
        }
    }

    /// The outlook now.
    pub(crate) fn outlook(&mut self) -> RepairOutlook {
        let roles: Vec<Role> = self.files.iter().map(|f| self.role(f)).collect();
        if self.sets.is_empty() {
            return RepairOutlook { no_par2: roles.iter().all(|r| *r == Role::Other), ..Default::default() };
        }
        let mut needed = vec![0u64; self.sets.len()];
        for (f, role) in self.files.iter_mut().zip(&roles) {
            if let Role::Data { set, len } = *role {
                needed[set] += f.damaged_slices(len, self.sets[set].slice);
            }
        }
        let open = roles.iter().zip(&self.files).any(|(r, f)| *r == Role::Unknown || (*r == Role::Par2 && f.read.is_none()));
        let mut worst: Option<RepairOutlook> = None;
        for (i, set) in self.sets.iter().enumerate() {
            let block = set.slice.saturating_add(PACKET_OVERHEAD);
            let (mut posted, mut available) = (0u64, 0u64);
            for (f, role) in self.files.iter().zip(&roles) {
                match (role, &f.read) {
                    (Role::Par2, Some(read)) => {
                        let n = read.get(&set.id).copied().unwrap_or(0);
                        posted += n;
                        available += n;
                    }
                    (Role::Par2, None) => {
                        let fits = f.intact_encoded() / block;
                        let named = f.names().find_map(vol_blocks);
                        posted += named.unwrap_or(f.total_encoded() / block);
                        available += named.map_or(fits, |n| n.min(fits));
                    }
                    (Role::Unknown, _) => available += f.intact_encoded() / block,
                    _ => {}
                }
            }
            let o = RepairOutlook {
                known: true,
                no_par2: false,
                blocks: set.blocks,
                needed: needed[i],
                posted,
                available,
                exact: !open,
            };
            let worse = worst.is_none_or(|w| (o.needed as u128) * (w.available.max(1) as u128) > (w.needed as u128) * (o.available.max(1) as u128));
            if worse {
                worst = Some(o);
            }
        }
        worst.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use newskite_par2::FileEntry;

    const S: u64 = 1000;

    fn set(id: u8, files: &[(&str, u64)], recovery: usize) -> RecoverySet {
        let mut first = 0;
        let entries = files
            .iter()
            .map(|&(name, length)| {
                let e = FileEntry {
                    id: [0; 16],
                    name: name.into(),
                    length,
                    md5: [0; 16],
                    md5_16k: [id; 16],
                    slices: vec![newskite_par2::SliceHash { md5: [0; 16], crc: 0 }; length.div_ceil(S) as usize],
                    first_slice: first,
                };
                first += e.slices.len();
                e
            })
            .collect();
        RecoverySet {
            id: [id; 16],
            slice_size: S,
            files: entries,
            recovery: (0..recovery)
                .map(|e| newskite_par2::RecoveryRef {
                    exponent: e as u32,
                    path: "x".into(),
                    offset: 0,
                    packet_offset: 0,
                    packet_len: 0,
                    packet_md5: [0; 16],
                })
                .collect(),
            creator: None,
            incomplete: Vec::new(),
        }
    }

    /// A file of `n` articles of `enc` encoded bytes each.
    fn file(name: &str, n: u32, enc: u64) -> (String, Vec<u32>, Vec<u64>) {
        (name.into(), (1..=n).collect(), vec![enc; n as usize])
    }

    #[test]
    fn vol_names_give_block_counts() {
        assert_eq!(vol_blocks("a.vol000+01.par2"), Some(1));
        assert_eq!(vol_blocks("A.Vol015+16.PAR2"), Some(16));
        assert_eq!(vol_blocks("a.vol3-7.par2"), Some(7));
        assert_eq!(vol_blocks("a.par2"), None);
        assert_eq!(vol_blocks("a.volume.par2"), None);
    }

    #[test]
    fn head_reads_magic_and_hashes_the_first_16k() {
        let mut data = PAR2_MAGIC.to_vec();
        data.resize(20_000, 7);
        let h = Head::of(&data, 50_000);
        assert!(h.magic);
        assert_eq!(h.md5_16k, Some(Md5::digest(&data[..16_384]).into()));
        assert_eq!(Head::of(&data[..100], 50_000).md5_16k, None, "too short to hash");
        assert_eq!(Head::of(&data[..100], 100).md5_16k, Some(Md5::digest(&data[..100]).into()), "a small file hashes whole");
    }

    #[test]
    fn nothing_known_until_par2_is_read_and_no_par2_once_every_file_is_other() {
        let mut t = Tracker::new(vec![file("a.bin", 2, 600)], 0);
        assert_eq!(t.outlook(), RepairOutlook::default());
        t.note_part(0, 0, "a.bin", 1000, (1, 500), Some(Head { magic: false, md5_16k: None }));
        assert!(t.outlook().no_par2);
    }

    #[test]
    fn damage_counts_exact_slices_from_article_offsets() {
        // 10 articles of 500 bytes over 5000 bytes: two articles per 1000-byte slice.
        let mut t = Tracker::new(vec![file("data.rar", 10, 520), file("data.par2", 1, 900), file("data.vol00+04.par2", 5, 1100)], 0);
        t.note_par2(1, &[set(1, &[("data.rar", 5000)], 0)]);
        t.note_part(0, 0, "data.rar", 5000, (1, 500), Some(Head { magic: false, md5_16k: None }));
        assert!(t.note_damaged(0, 2)); // bytes 1000..1500: slice 1
        assert!(!t.note_damaged(0, 2), "counted once");
        t.note_damaged(0, 3); // 1500..2000: slice 1 again
        t.note_damaged(0, 9); // 4500..5000: slice 4
        let o = t.outlook();
        assert!(o.known);
        assert_eq!((o.blocks, o.needed), (5, 2));
        // The volume names 4 blocks and its 5 x 1100 encoded bytes can hold 5 of 1068.
        assert_eq!((o.posted, o.available, o.exact), (4, 4, false));
        assert!(!o.hopeless());
    }

    #[test]
    fn hopeless_once_damage_passes_what_recovery_can_hold() {
        let mut t = Tracker::new(vec![file("d.rar", 10, 520), file("d.par2", 1, 900), file("d.vol00+02.par2", 2, 1100)], 0);
        t.note_par2(1, &[set(1, &[("d.rar", 5000)], 0)]);
        t.note_part(0, 1, "d.rar", 5000, (501, 1000), None);
        for si in [0, 2, 4] {
            t.note_damaged(0, si);
        }
        let o = t.outlook();
        assert_eq!((o.needed, o.available), (3, 2));
        assert!(o.hopeless());
    }

    #[test]
    fn lost_volume_articles_shrink_available_and_a_read_volume_is_exact() {
        let mut t = Tracker::new(vec![file("d.rar", 10, 520), file("d.par2", 1, 900), file("d.vol00+04.par2", 4, 1100)], 0);
        t.note_par2(1, &[set(1, &[("d.rar", 5000)], 0)]);
        t.note_damaged(2, 0);
        t.note_damaged(2, 1);
        // 2 intact articles x 1100 bytes hold at most 2 blocks of 1068.
        assert_eq!(t.outlook().available, 2);
        t.note_par2(2, &[set(1, &[("d.rar", 5000)], 2)]);
        let o = t.outlook();
        assert_eq!((o.posted, o.available, o.exact), (2, 2, true));
    }

    #[test]
    fn unidentified_files_may_be_par2_until_their_first_article_says_otherwise() {
        let mut t = Tracker::new(vec![file("x1", 10, 520), file("x2", 4, 1100), file("idx", 1, 900)], 0);
        t.note_par2(2, &[set(9, &[("movie.part1.rar", 5000)], 0)]);
        assert_eq!(t.outlook().available, 4 + 4, "both may still be PAR2");
        // x1 is the protected file (length and first-16k hash), x2 is not PAR2.
        t.note_part(0, 0, "x1", 5000, (1, 500), Some(Head { magic: false, md5_16k: Some([9; 16]) }));
        t.note_part(1, 0, "x2", 4000, (1, 1000), Some(Head { magic: false, md5_16k: Some([3; 16]) }));
        t.note_damaged(0, 5);
        let o = t.outlook();
        assert_eq!((o.needed, o.available, o.exact), (1, 0, true));
        assert!(o.hopeless());
    }

    #[test]
    fn irregular_or_unknown_article_lengths_count_nothing_but_a_fully_missing_file_counts_all() {
        let mut t = Tracker::new(vec![file("a.rar", 4, 520), file("b.rar", 2, 520), file("s.par2", 1, 900)], 0);
        t.note_par2(2, &[set(1, &[("a.rar", 2000), ("b.rar", 1000)], 0)]);
        t.note_damaged(0, 1);
        assert_eq!(t.outlook().needed, 0, "article length unknown");
        t.note_part(0, 2, "a.rar", 2000, (1001, 1500), None);
        assert_eq!(t.outlook().needed, 1);
        t.note_part(0, 3, "a.rar", 2000, (1400, 2000), None);
        assert_eq!(t.outlook().needed, 0, "offsets do not follow: no longer trusted");
        t.note_damaged(1, 0);
        t.note_damaged(1, 1);
        assert_eq!(t.outlook().needed, 1, "all of b.rar's slices");
    }

    #[test]
    fn the_worst_of_several_sets_is_reported() {
        let mut t = Tracker::new(
            vec![file("e1.rar", 2, 520), file("e1.par2", 1, 900), file("e2.rar", 2, 520), file("e2.par2", 1, 900)],
            0,
        );
        t.note_par2(1, &[set(1, &[("e1.rar", 1000)], 3)]);
        t.note_par2(3, &[set(2, &[("e2.rar", 1000)], 1)]);
        t.note_damaged(2, 0);
        t.note_damaged(2, 1);
        let o = t.outlook();
        assert_eq!((o.needed, o.available, o.posted), (1, 1, 1));
    }
}
