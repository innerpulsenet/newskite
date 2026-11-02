//! PAR2 packet parsing and the recovery-set model built from it.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};

pub(crate) const MAGIC: &[u8; 8] = b"PAR2\0PKT";
pub(crate) const HEADER: usize = 64;
pub(crate) const T_MAIN: &[u8; 16] = b"PAR 2.0\0Main\0\0\0\0";
pub(crate) const T_DESC: &[u8; 16] = b"PAR 2.0\0FileDesc";
pub(crate) const T_IFSC: &[u8; 16] = b"PAR 2.0\0IFSC\0\0\0\0";
pub(crate) const T_RECV: &[u8; 16] = b"PAR 2.0\0RecvSlic";
pub(crate) const T_CREATOR: &[u8; 16] = b"PAR 2.0\0Creator\0";

/// Packets larger than this are ignored as corrupt (a recovery slice is at most a few
/// hundred MiB in practice).
const MAX_PACKET: u64 = 1 << 31;
/// Non-recovery packets are read into memory; anything bigger is not a real one.
const MAX_SMALL_PACKET: u64 = 64 << 20;

pub type Id = [u8; 16];

/// MD5 and CRC32 of one input slice (zero-padded to the slice size).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SliceHash {
    pub md5: Id,
    pub crc: u32,
}

/// One file protected by a recovery set.
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub id: Id,
    /// Name as stored (may contain `/` for subfolders; checked before use).
    pub name: String,
    pub length: u64,
    pub md5: Id,
    pub md5_16k: Id,
    /// Per-slice checksums, `ceil(length / slice_size)` of them.
    pub slices: Vec<SliceHash>,
    /// Global index of this file's first slice in the set.
    pub first_slice: usize,
}

impl FileEntry {
    /// Bytes of slice `k` of this file that hold data (the last one may be short).
    pub fn slice_len(&self, k: usize, slice_size: u64) -> u64 {
        let start = k as u64 * slice_size;
        self.length.saturating_sub(start).min(slice_size)
    }
}

/// Where a recovery slice is stored.
#[derive(Clone, Debug)]
pub struct RecoveryRef {
    pub exponent: u32,
    pub path: PathBuf,
    /// Offset of the slice data in `path`.
    pub offset: u64,
    /// Offset of the packet (for the checksum).
    pub packet_offset: u64,
    pub packet_len: u64,
    pub packet_md5: Id,
}

/// A recovery set: the files it protects (in PAR2 order) and the recovery slices found.
#[derive(Clone, Debug)]
pub struct RecoverySet {
    pub id: Id,
    pub slice_size: u64,
    pub files: Vec<FileEntry>,
    /// One per exponent (duplicates dropped), sorted by exponent.
    pub recovery: Vec<RecoveryRef>,
    pub creator: Option<String>,
    /// File IDs the main packet lists but whose description or checksums were not found.
    pub incomplete: Vec<Id>,
}

impl RecoverySet {
    /// Total input slices.
    pub fn slice_count(&self) -> usize {
        self.files.iter().map(|f| f.slices.len()).sum()
    }

    /// Total bytes protected.
    pub fn data_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.length).sum()
    }
}

#[derive(Default)]
struct Partial {
    main: Option<(u64, Vec<Id>)>,
    descs: BTreeMap<Id, (String, u64, Id, Id)>,
    ifsc: BTreeMap<Id, Vec<SliceHash>>,
    recovery: BTreeMap<u32, RecoveryRef>,
    creator: Option<String>,
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
fn u64le(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}
fn id(b: &[u8]) -> Id {
    b[..16].try_into().unwrap()
}

/// True when `path` starts with a PAR2 packet header.
pub fn has_magic(path: &Path) -> bool {
    let mut b = [0u8; 8];
    File::open(path).and_then(|mut f| f.read_exact(&mut b)).is_ok() && &b == MAGIC
}

/// Parses every packet in `paths` and builds the recovery sets found (a set needs its
/// main packet). Unreadable files and corrupt packets are skipped; their count is
/// returned with the sets.
pub fn load(paths: &[PathBuf]) -> (Vec<RecoverySet>, usize) {
    let mut partial: BTreeMap<Id, Partial> = BTreeMap::new();
    let mut bad = 0;
    for p in paths {
        if scan_file(p, &mut partial, &mut bad).is_err() {
            bad += 1;
        }
    }
    let mut sets = Vec::new();
    for (set_id, p) in partial {
        let Some((slice_size, ids)) = p.main else { continue };
        let mut files = Vec::new();
        let mut incomplete = Vec::new();
        let mut first = 0usize;
        for fid in ids {
            let (Some((name, length, md5, md5_16k)), Some(slices)) = (p.descs.get(&fid), p.ifsc.get(&fid)) else {
                incomplete.push(fid);
                continue;
            };
            let want = length.div_ceil(slice_size) as usize;
            if slices.len() != want {
                incomplete.push(fid);
                continue;
            }
            files.push(FileEntry {
                id: fid,
                name: name.clone(),
                length: *length,
                md5: *md5,
                md5_16k: *md5_16k,
                slices: slices.clone(),
                first_slice: first,
            });
            first += want;
        }
        sets.push(RecoverySet {
            id: set_id,
            slice_size,
            files,
            recovery: p.recovery.into_values().collect(),
            creator: p.creator,
            incomplete,
        });
    }
    (sets, bad)
}

/// Reads packets from one file, resynchronizing on the magic after garbage.
fn scan_file(path: &Path, sets: &mut BTreeMap<Id, Partial>, bad: &mut usize) -> io::Result<()> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut pos = 0u64;
    let mut header = [0u8; HEADER];
    while pos + HEADER as u64 <= len {
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut header)?;
        if &header[..8] != MAGIC {
            match find_magic(&mut f, pos + 1, len)? {
                Some(next) => {
                    pos = next;
                    continue;
                }
                None => break,
            }
        }
        let plen = u64le(&header[8..]);
        if plen < HEADER as u64 || !plen.is_multiple_of(4) || plen > MAX_PACKET || pos + plen > len {
            *bad += 1;
            pos += 1;
            continue;
        }
        let packet_md5 = id(&header[16..]);
        let set_id = id(&header[32..]);
        let ptype: [u8; 16] = header[48..64].try_into().unwrap();
        let body_len = plen - HEADER as u64;
        if &ptype == T_RECV {
            if body_len < 4 {
                *bad += 1;
                pos += 1;
                continue;
            }
            let mut e = [0u8; 4];
            f.read_exact(&mut e)?;
            // The checksum covers the whole slice; it is checked before the slice is used.
            sets.entry(set_id).or_default().recovery.entry(u32le(&e)).or_insert(RecoveryRef {
                exponent: u32le(&e),
                path: path.to_path_buf(),
                offset: pos + HEADER as u64 + 4,
                packet_offset: pos,
                packet_len: plen,
                packet_md5,
            });
            pos += plen;
            continue;
        }
        if body_len > MAX_SMALL_PACKET {
            pos += plen;
            continue;
        }
        let mut body = vec![0u8; body_len as usize];
        f.read_exact(&mut body)?;
        let mut h = Md5::new();
        h.update(&header[32..]);
        h.update(&body);
        if h.finalize().as_slice() != packet_md5 {
            *bad += 1;
            pos += 1;
            continue;
        }
        let entry = sets.entry(set_id).or_default();
        parse_body(&ptype, &body, set_id, entry);
        pos += plen;
    }
    Ok(())
}

fn parse_body(ptype: &[u8; 16], body: &[u8], set_id: Id, p: &mut Partial) {
    if ptype == T_MAIN && body.len() >= 12 {
        // The set id is the MD5 of the main packet body.
        if Md5::digest(body).as_slice() != set_id {
            return;
        }
        let slice_size = u64le(body);
        let n = u32le(&body[8..]) as usize;
        if slice_size == 0 || !slice_size.is_multiple_of(4) || body.len() < 12 + n * 16 {
            return;
        }
        let ids = (0..n).map(|i| id(&body[12 + i * 16..])).collect();
        p.main.get_or_insert((slice_size, ids));
    } else if ptype == T_DESC && body.len() >= 56 {
        let fid = id(body);
        let md5 = id(&body[16..]);
        let md5_16k = id(&body[32..]);
        let length = u64le(&body[48..]);
        let raw = &body[56..];
        let end = raw.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        let name = String::from_utf8_lossy(&raw[..end]).into_owned();
        p.descs.entry(fid).or_insert((name, length, md5, md5_16k));
    } else if ptype == T_IFSC && body.len() >= 16 && (body.len() - 16).is_multiple_of(20) {
        let fid = id(body);
        let slices = body[16..].chunks_exact(20).map(|c| SliceHash { md5: id(c), crc: u32le(&c[16..]) }).collect();
        p.ifsc.entry(fid).or_insert(slices);
    } else if ptype == T_CREATOR {
        let end = body.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        p.creator.get_or_insert_with(|| String::from_utf8_lossy(&body[..end]).into_owned());
    }
}

/// Offset of the next packet magic at or after `from`.
fn find_magic(f: &mut File, from: u64, len: u64) -> io::Result<Option<u64>> {
    const CHUNK: usize = 1 << 20;
    let mut buf = vec![0u8; CHUNK + 7];
    let mut pos = from;
    while pos < len {
        f.seek(SeekFrom::Start(pos))?;
        let want = ((len - pos) as usize).min(buf.len());
        f.read_exact(&mut buf[..want])?;
        if let Some(i) = buf[..want].windows(8).position(|w| w == MAGIC) {
            return Ok(Some(pos + i as u64));
        }
        if want < 8 {
            break;
        }
        pos += (want - 7) as u64;
    }
    Ok(None)
}

/// Checks a recovery packet's MD5 (reads the whole slice).
pub(crate) fn recovery_intact(r: &RecoveryRef) -> io::Result<bool> {
    let mut f = File::open(&r.path)?;
    f.seek(SeekFrom::Start(r.packet_offset + 32))?;
    let mut h = Md5::new();
    let mut left = r.packet_len - 32;
    let mut buf = vec![0u8; 1 << 20];
    while left > 0 {
        let n = (left as usize).min(buf.len());
        f.read_exact(&mut buf[..n])?;
        h.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(h.finalize().as_slice() == r.packet_md5)
}
