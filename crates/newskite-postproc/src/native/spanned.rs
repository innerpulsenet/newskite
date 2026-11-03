//! Spanned zip archives (`.z01`, `.z02` ... `.zip`, written by Info-ZIP `zip -s` and
//! WinZip). Every volume is a "disk": the central directory addresses each entry by its
//! disk number and an offset relative to that disk, and the end record is in the `.zip`,
//! the last disk.
//!
//! [`open`] presents such a set to the `zip` crate as one single-disk archive: the
//! volumes in disk order, cut where the central directory starts, followed by an
//! in-memory copy of the central directory in which every local header offset is made
//! absolute (the lengths of the disks before the entry's disk plus its relative offset)
//! and every disk number is zeroed, and fresh end records (ZIP64 ones when an offset, the
//! directory size or the entry count no longer fits the classic record). Entry data is
//! read from the volumes unchanged, so every codec and encryption method of the `zip`
//! crate works as for a single archive, and an entry may cross any number of volumes.
//! The split marker `PK\x07\x08` at the start of `.z01` is part of disk 0; the stored
//! offsets already account for it.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::{NativeError, VolumeReader};
use crate::detect::{ArchiveKind, ArchiveSet};

const CENTRAL_HEADER_SIG: u32 = 0x0201_4b50;
const DIGITAL_SIGNATURE_SIG: u32 = 0x0505_4b50;
const END_SIG: u32 = 0x0605_4b50;
const ZIP64_END_SIG: u32 = 0x0606_4b50;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
/// Extra field holding the ZIP64 values of a central directory entry.
const ZIP64_EXTRA_ID: u16 = 0x0001;
/// Fixed sizes of the records (without variable-length tails).
const CENTRAL_HEADER_LEN: usize = 46;
const END_LEN: usize = 22;
const ZIP64_END_LEN: usize = 56;
const ZIP64_LOCATOR_LEN: usize = 20;
/// Longest archive comment after the end record.
const MAX_COMMENT: usize = 0xFFFF;
/// A central directory larger than this is refused instead of loaded (it would hold
/// millions of entries).
const MAX_CENTRAL_DIRECTORY: u64 = 256 * 1024 * 1024;

/// The volumes of a spanned zip in disk order (`.z01`, `.z02` ..., then the `.zip`), or
/// `None` when `set` is not a spanned zip. A gap in the numbering is a missing volume.
pub(crate) fn volumes(set: &ArchiveSet) -> Option<Result<Vec<PathBuf>, NativeError>> {
    if set.kind != ArchiveKind::Zip {
        return None;
    }
    let lower = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if !lower(&set.first).ends_with(".zip") {
        return None;
    }
    let mut parts: Vec<(u32, PathBuf)> = set
        .volumes
        .iter()
        .filter_map(|v| {
            let name = lower(v);
            let digits = name.rsplit_once('.')?.1.strip_prefix('z')?;
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((digits.parse().ok()?, v.clone()))
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    parts.sort();
    if parts.iter().enumerate().any(|(i, (n, _))| *n != i as u32 + 1) {
        return Some(Err(NativeError::MissingVolume));
    }
    let mut out: Vec<PathBuf> = parts.into_iter().map(|(_, p)| p).collect();
    out.push(set.first.clone());
    Some(Ok(out))
}

/// The values of the end records that matter here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct End {
    /// Number of the disk holding the end record, i.e. the last one.
    last_disk: u64,
    /// Disk the central directory starts on, and its offset there.
    cd_disk: u64,
    cd_offset: u64,
    cd_size: u64,
    entries: u64,
}

/// Open a spanned zip (its volumes in disk order, as from [`volumes`]) as one
/// single-disk archive.
pub(crate) fn open(paths: &[PathBuf]) -> Result<SpannedReader, NativeError> {
    let read_err = |e: io::Error| NativeError::Io(format!("could not read the archive: {e}"));
    let mut volumes =
        VolumeReader::open(paths).map_err(|e| NativeError::Io(format!("could not open the archive: {e}")))?;
    let starts = volumes.starts();
    let total = volumes.total_len();

    // 1. The end record, at the end of the last volume (after it only a comment).
    let last_start = *starts.last().ok_or(NativeError::MissingVolume)?;
    let tail_len = (total - last_start).min((END_LEN + MAX_COMMENT) as u64);
    let tail_start = total - tail_len;
    let tail = read_at(&mut volumes, tail_start, tail_len as usize).map_err(read_err)?;
    let at = find_end(&tail).ok_or_else(|| NativeError::Corrupt("end of central directory not found".into()))?;
    let end_pos = tail_start + at as u64;
    let e = &tail[at..];
    let mut end = End {
        last_disk: u64::from(u16le(e, 4)),
        cd_disk: u64::from(u16le(e, 6)),
        entries: u64::from(u16le(e, 10)),
        cd_size: u64::from(u32le(e, 12)),
        cd_offset: u64::from(u32le(e, 16)),
    };

    // 2. A ZIP64 locator right before it points to the ZIP64 end record, whose values
    //    replace the classic ones.
    let mut records_start = end_pos;
    if let Some(loc_pos) = end_pos.checked_sub(ZIP64_LOCATOR_LEN as u64) {
        let loc = read_at(&mut volumes, loc_pos, ZIP64_LOCATOR_LEN).map_err(read_err)?;
        if u32le(&loc, 0) == ZIP64_LOCATOR_SIG {
            let pos = disk_offset(&starts, u64::from(u32le(&loc, 4)), u64le(&loc, 8))?;
            if pos.checked_add(ZIP64_END_LEN as u64).is_none_or(|e| e > loc_pos) {
                return Err(NativeError::Corrupt("ZIP64 end record outside the archive".into()));
            }
            let rec = read_at(&mut volumes, pos, ZIP64_END_LEN).map_err(read_err)?;
            if u32le(&rec, 0) != ZIP64_END_SIG {
                return Err(NativeError::Corrupt("ZIP64 end record not found".into()));
            }
            end = End {
                last_disk: u64::from(u32le(&rec, 16)),
                cd_disk: u64::from(u32le(&rec, 20)),
                entries: u64le(&rec, 32),
                cd_size: u64le(&rec, 40),
                cd_offset: u64le(&rec, 48),
            };
            records_start = pos;
        }
    }

    // 3. Every volume must be there: a missing last `.zNN` leaves no gap in the names.
    let found = paths.len() as u64;
    let expected = end.last_disk + 1;
    if expected > found {
        return Err(NativeError::MissingVolume);
    }
    if expected < found {
        return Err(NativeError::Corrupt(format!("{found} volumes found, but the archive has {expected}")));
    }

    // 4. The central directory, which may span volumes, rewritten for a single disk.
    let cd_start = disk_offset(&starts, end.cd_disk, end.cd_offset)?;
    if end.cd_size > MAX_CENTRAL_DIRECTORY {
        return Err(NativeError::Corrupt("central directory is implausibly large".into()));
    }
    if cd_start.checked_add(end.cd_size).is_none_or(|e| e > records_start) {
        return Err(NativeError::Corrupt("central directory outside the archive".into()));
    }
    let cd = read_at(&mut volumes, cd_start, end.cd_size as usize).map_err(read_err)?;
    let (mut tail, count) = rewrite_central_directory(&cd, &starts, cd_start)?;
    if count != end.entries {
        return Err(NativeError::Corrupt(format!(
            "central directory lists {count} entries, the end record {}",
            end.entries
        )));
    }
    let records = end_records(cd_start, tail.len() as u64, count);
    tail.extend_from_slice(&records);
    Ok(SpannedReader { volumes, cut: cd_start, tail, pos: 0 })
}

/// Position of the end record in `tail` (the end of the last volume): the last signature
/// whose comment ends exactly at the end of the volume, else the last one whose comment
/// fits.
fn find_end(tail: &[u8]) -> Option<usize> {
    let last = tail.len().checked_sub(END_LEN)?;
    let mut fits = None;
    for i in (0..=last).rev() {
        if u32le(tail, i) != END_SIG {
            continue;
        }
        let comment_end = i + END_LEN + usize::from(u16le(tail, i + 20));
        if comment_end == tail.len() {
            return Some(i);
        }
        if comment_end < tail.len() && fits.is_none() {
            fits = Some(i);
        }
    }
    fits
}

/// Absolute position of `offset` on disk `disk` (`starts` holds each disk's start).
fn disk_offset(starts: &[u64], disk: u64, offset: u64) -> Result<u64, NativeError> {
    let start = usize::try_from(disk)
        .ok()
        .and_then(|d| starts.get(d))
        .ok_or_else(|| NativeError::Corrupt(format!("reference to disk {} of {}", disk.saturating_add(1), starts.len())))?;
    start.checked_add(offset).ok_or_else(|| NativeError::Corrupt("offset out of range".into()))
}

/// Copy of the central directory `cd` with every entry's local header offset made
/// absolute and its disk number zeroed; an offset that no longer fits 32 bits moves into
/// the ZIP64 extra field. Every local header must start before `cut` (where the central
/// directory starts). Returns the copy and the number of entries.
fn rewrite_central_directory(cd: &[u8], starts: &[u64], cut: u64) -> Result<(Vec<u8>, u64), NativeError> {
    let bad = |what: &str| NativeError::Corrupt(format!("central directory: {what}"));
    let mut out = Vec::with_capacity(cd.len() + cd.len() / 8);
    let mut count = 0u64;
    let mut p = 0usize;
    while p < cd.len() {
        let rest = &cd[p..];
        if rest.len() < 4 {
            return Err(bad("truncated"));
        }
        match u32le(rest, 0) {
            CENTRAL_HEADER_SIG => {}
            DIGITAL_SIGNATURE_SIG if rest.len() >= 6 => {
                // Not needed to read the archive; dropped.
                p += 6 + usize::from(u16le(rest, 4));
                continue;
            }
            _ => return Err(bad("bad entry signature")),
        }
        if rest.len() < CENTRAL_HEADER_LEN {
            return Err(bad("truncated"));
        }
        let name_len = usize::from(u16le(rest, 28));
        let extra_len = usize::from(u16le(rest, 30));
        let comment_len = usize::from(u16le(rest, 32));
        let len = CENTRAL_HEADER_LEN + name_len + extra_len + comment_len;
        if rest.len() < len {
            return Err(bad("truncated"));
        }
        let name = &rest[CENTRAL_HEADER_LEN..CENTRAL_HEADER_LEN + name_len];
        let extra = &rest[CENTRAL_HEADER_LEN + name_len..CENTRAL_HEADER_LEN + name_len + extra_len];
        let comment = &rest[CENTRAL_HEADER_LEN + name_len + extra_len..len];
        let compressed = u32le(rest, 20);
        let uncompressed = u32le(rest, 24);
        let disk16 = u16le(rest, 34);
        let offset32 = u32le(rest, 42);

        // The ZIP64 field holds a value for exactly the fields that hold their
        // sentinel, in this order.
        let (zip64, others) = split_extra(extra);
        let mut z = zip64.unwrap_or_default();
        let uncompressed64 = zip64_value(&mut z, uncompressed == u32::MAX, 8)?;
        let compressed64 = zip64_value(&mut z, compressed == u32::MAX, 8)?;
        let offset = zip64_value(&mut z, offset32 == u32::MAX, 8)?.unwrap_or(u64::from(offset32));
        let disk = zip64_value(&mut z, disk16 == u16::MAX, 4)?.unwrap_or(u64::from(disk16));
        let absolute = disk_offset(starts, disk, offset)?;
        if absolute >= cut {
            return Err(bad("entry outside the archive data"));
        }

        let large = absolute >= u64::from(u32::MAX);
        let mut values: Vec<u8> = Vec::with_capacity(24);
        for v in [uncompressed64, compressed64, large.then_some(absolute)].into_iter().flatten() {
            values.extend_from_slice(&v.to_le_bytes());
        }
        let mut new_extra = Vec::with_capacity(extra.len() + 12);
        if !values.is_empty() {
            new_extra.extend_from_slice(&ZIP64_EXTRA_ID.to_le_bytes());
            new_extra.extend_from_slice(&(values.len() as u16).to_le_bytes());
            new_extra.extend_from_slice(&values);
        }
        for field in others {
            new_extra.extend_from_slice(field);
        }
        let new_extra_len = u16::try_from(new_extra.len()).map_err(|_| bad("extra field too long"))?;

        let header = out.len();
        out.extend_from_slice(&rest[..CENTRAL_HEADER_LEN]);
        let h = &mut out[header..];
        h[30..32].copy_from_slice(&new_extra_len.to_le_bytes());
        h[34..36].copy_from_slice(&0u16.to_le_bytes());
        let offset32 = if large { u32::MAX } else { absolute as u32 };
        h[42..46].copy_from_slice(&offset32.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&new_extra);
        out.extend_from_slice(comment);
        count += 1;
        p += len;
    }
    Ok((out, count))
}

/// Split an extra field block into the data of its ZIP64 field (if any) and the other
/// fields (each with its header), which are kept verbatim. A malformed tail (padding some
/// writers leave) is kept verbatim too.
fn split_extra(extra: &[u8]) -> (Option<&[u8]>, Vec<&[u8]>) {
    let mut zip64 = None;
    let mut others = Vec::new();
    let mut p = 0;
    while p < extra.len() {
        let end = (extra.len() - p >= 4).then(|| p + 4 + usize::from(u16le(extra, p + 2)));
        let Some(end) = end.filter(|&e| e <= extra.len()) else {
            others.push(&extra[p..]);
            break;
        };
        if u16le(extra, p) == ZIP64_EXTRA_ID && zip64.is_none() {
            zip64 = Some(&extra[p + 4..end]);
        } else {
            others.push(&extra[p..end]);
        }
        p = end;
    }
    (zip64, others)
}

/// The next `width`-byte value of a ZIP64 field when `present`.
fn zip64_value(z: &mut &[u8], present: bool, width: usize) -> Result<Option<u64>, NativeError> {
    if !present {
        return Ok(None);
    }
    if z.len() < width {
        return Err(NativeError::Corrupt("central directory: ZIP64 extra field truncated".into()));
    }
    let mut b = [0u8; 8];
    b[..width].copy_from_slice(&z[..width]);
    *z = &z[width..];
    Ok(Some(u64::from_le_bytes(b)))
}

/// End records for a single-disk central directory of `len` bytes and `entries` entries
/// starting at `start`: a ZIP64 end record and locator when a value does not fit the
/// classic record, then the classic record (with the sentinel in every field that does
/// not fit) and no comment.
fn end_records(start: u64, len: u64, entries: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(ZIP64_END_LEN + ZIP64_LOCATOR_LEN + END_LEN);
    if entries >= u64::from(u16::MAX) || len >= u64::from(u32::MAX) || start >= u64::from(u32::MAX) {
        out.extend_from_slice(&ZIP64_END_SIG.to_le_bytes());
        out.extend_from_slice(&(ZIP64_END_LEN as u64 - 12).to_le_bytes()); // size of the rest
        out.extend_from_slice(&45u16.to_le_bytes()); // version made by: 4.5 (ZIP64)
        out.extend_from_slice(&45u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u32.to_le_bytes()); // this disk
        out.extend_from_slice(&0u32.to_le_bytes()); // disk of the central directory
        out.extend_from_slice(&entries.to_le_bytes()); // entries on this disk
        out.extend_from_slice(&entries.to_le_bytes()); // entries in total
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&start.to_le_bytes());
        out.extend_from_slice(&ZIP64_LOCATOR_SIG.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // disk of the ZIP64 end record
        out.extend_from_slice(&(start + len).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes()); // disks in total
    }
    let entries16 = u16::try_from(entries).unwrap_or(u16::MAX);
    out.extend_from_slice(&END_SIG.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // disk of the central directory
    out.extend_from_slice(&entries16.to_le_bytes());
    out.extend_from_slice(&entries16.to_le_bytes());
    out.extend_from_slice(&u32::try_from(len).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(&u32::try_from(start).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment length
    out
}

fn read_at(r: &mut VolumeReader, pos: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    r.seek(SeekFrom::Start(pos))?;
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64le(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

/// A spanned zip read as one single-disk archive: the volumes up to the start of the
/// central directory, then the rewritten central directory and end records.
pub(crate) struct SpannedReader {
    volumes: VolumeReader,
    /// Where the volumes are cut and the in-memory tail begins.
    cut: u64,
    tail: Vec<u8>,
    pos: u64,
}

impl SpannedReader {
    fn len(&self) -> u64 {
        self.cut + self.tail.len() as u64
    }
}

impl Read for SpannedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos < self.cut {
            let want = buf.len().min((self.cut - self.pos).min(usize::MAX as u64) as usize);
            self.volumes.seek(SeekFrom::Start(self.pos))?;
            let n = self.volumes.read(&mut buf[..want])?;
            self.pos += n as u64;
            return Ok(n);
        }
        let at = (self.pos - self.cut).min(self.tail.len() as u64) as usize;
        let n = buf.len().min(self.tail.len() - at);
        buf[..n].copy_from_slice(&self.tail[at..at + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SpannedReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let new = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.len().checked_add_signed(d),
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

    fn set(names: &[&str]) -> ArchiveSet {
        let volumes: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
        ArchiveSet { kind: ArchiveKind::Zip, first: volumes[0].clone(), volumes }
    }

    #[test]
    fn disk_order_and_gaps() {
        let s = set(&["a.zip", "a.z02", "a.z01"]);
        assert_eq!(volumes(&s).unwrap().unwrap(), vec![PathBuf::from("a.z01"), PathBuf::from("a.z02"), PathBuf::from("a.zip")]);
        let s = set(&["A.ZIP", "A.Z01"]);
        assert_eq!(volumes(&s).unwrap().unwrap(), vec![PathBuf::from("A.Z01"), PathBuf::from("A.ZIP")]);
        assert_eq!(volumes(&set(&["a.zip", "a.z01", "a.z03"])), Some(Err(NativeError::MissingVolume)));
        assert_eq!(volumes(&set(&["a.zip", "a.z02"])), Some(Err(NativeError::MissingVolume)));
        assert_eq!(volumes(&set(&["a.zip"])), None);
        assert_eq!(volumes(&set(&["a.zip.001", "a.zip.002"])), None);
        let mut s = set(&["a.zip", "a.z01"]);
        s.kind = ArchiveKind::SevenZip;
        assert_eq!(volumes(&s), None);
    }

    #[test]
    fn end_record_found_before_comment_and_junk() {
        let mut v = vec![0u8; 10];
        let mut rec = END_SIG.to_le_bytes().to_vec();
        rec.extend_from_slice(&[0u8; 16]);
        rec.extend_from_slice(&3u16.to_le_bytes());
        v.extend_from_slice(&rec);
        v.extend_from_slice(b"abc");
        assert_eq!(find_end(&v), Some(10));
        // A signature inside the comment whose own comment would not fit is passed over.
        let mut w = v.clone();
        w.truncate(10 + END_LEN);
        w[30..32].copy_from_slice(&(END_LEN as u16).to_le_bytes());
        w.extend_from_slice(&rec[..END_LEN - 2]);
        w.extend_from_slice(&5u16.to_le_bytes());
        assert_eq!(find_end(&w), Some(10));
        assert_eq!(find_end(&[0u8; 21]), None);
    }

    /// One central directory entry (no name) with the given 32-bit fields and extra.
    fn central(disk: u16, offset: u32, sizes: u32, extra: &[u8]) -> Vec<u8> {
        let mut h = CENTRAL_HEADER_SIG.to_le_bytes().to_vec();
        h.extend_from_slice(&[0u8; 16]); // versions, flags, method, time, date, crc
        h.extend_from_slice(&sizes.to_le_bytes());
        h.extend_from_slice(&sizes.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes()); // name length
        h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        h.extend_from_slice(&0u16.to_le_bytes()); // comment length
        h.extend_from_slice(&disk.to_le_bytes());
        h.extend_from_slice(&[0u8; 6]); // attributes
        h.extend_from_slice(&offset.to_le_bytes());
        h.push(b'x');
        h.extend_from_slice(extra);
        h
    }

    fn zip64_extra(values: &[u64], disk: Option<u32>) -> Vec<u8> {
        let mut data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        if let Some(d) = disk {
            data.extend_from_slice(&d.to_le_bytes());
        }
        let mut f = ZIP64_EXTRA_ID.to_le_bytes().to_vec();
        f.extend_from_slice(&(data.len() as u16).to_le_bytes());
        f.extend_from_slice(&data);
        f
    }

    #[test]
    fn offsets_made_absolute_and_moved_into_zip64_when_large() {
        const GIB: u64 = 1 << 30;
        let starts = [0, 3 * GIB, 6 * GIB];
        let timestamp = [0x55, 0x54, 0x05, 0x00, 0x01, 1, 2, 3, 4];
        let mut cd = central(0, 4, 10, &timestamp);
        cd.extend(central(1, 100, 10, &[]));
        // ZIP64 sizes plus a ZIP64 disk number and offset (disk 2, offset 7).
        let mut extra = zip64_extra(&[5 * GIB, 5 * GIB, 7], Some(2));
        extra.extend_from_slice(&timestamp);
        cd.extend(central(u16::MAX, u32::MAX, u32::MAX, &extra));
        cd.extend(DIGITAL_SIGNATURE_SIG.to_le_bytes());
        cd.extend(0u16.to_le_bytes());
        let (out, count) = rewrite_central_directory(&cd, &starts, 7 * GIB).unwrap();
        assert_eq!(count, 3);

        // (32-bit offset, ZIP64 field data, other extra fields) per entry.
        let mut p = 0;
        let mut seen = Vec::new();
        while p < out.len() {
            let e = &out[p..];
            assert_eq!(u32le(e, 0), CENTRAL_HEADER_SIG);
            assert_eq!(u16le(e, 34), 0, "disk number zeroed");
            let extra_len = usize::from(u16le(e, 30));
            let (zip64, others) = split_extra(&e[47..47 + extra_len]);
            seen.push((u32le(e, 42), zip64.map(<[u8]>::to_vec), others.concat()));
            p += 47 + extra_len;
        }
        assert_eq!(seen[0], (4, None, timestamp.to_vec()));
        // 3 GiB + 100 still fits 32 bits.
        assert_eq!(seen[1], ((3 * GIB + 100) as u32, None, Vec::new()));
        // 6 GiB + 7 does not: the offset follows the two sizes in the ZIP64 field, and
        // the disk number is gone.
        let want: Vec<u8> = [5 * GIB, 5 * GIB, 6 * GIB + 7].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(seen[2], (u32::MAX, Some(want), timestamp.to_vec()));
    }

    #[test]
    fn bad_central_directories_rejected() {
        let starts = [0, 1000];
        // Disk out of range.
        assert!(rewrite_central_directory(&central(2, 0, 1, &[]), &starts, 1500).is_err());
        // Local header at or after the central directory.
        assert!(rewrite_central_directory(&central(1, 600, 1, &[]), &starts, 1500).is_err());
        // Sentinel offset without a ZIP64 value.
        assert!(rewrite_central_directory(&central(0, u32::MAX, 1, &[]), &starts, 1500).is_err());
        // Truncated entry and junk.
        let c = central(0, 0, 1, &[]);
        assert!(rewrite_central_directory(&c[..c.len() - 1], &starts, 1500).is_err());
        assert!(rewrite_central_directory(b"PK\x05\x06xxxx", &starts, 1500).is_err());
    }

    #[test]
    fn end_records_switch_to_zip64() {
        let classic = end_records(1000, 200, 3);
        assert_eq!(classic.len(), END_LEN);
        assert_eq!(find_end(&classic), Some(0));
        assert_eq!((u32le(&classic, 12), u32le(&classic, 16), u16le(&classic, 10)), (200, 1000, 3));

        let big = end_records(5 << 30, 200, 70_000);
        assert_eq!(big.len(), ZIP64_END_LEN + ZIP64_LOCATOR_LEN + END_LEN);
        assert_eq!(u32le(&big, 0), ZIP64_END_SIG);
        assert_eq!((u64le(&big, 32), u64le(&big, 40), u64le(&big, 48)), (70_000, 200, 5 << 30));
        let loc = &big[ZIP64_END_LEN..];
        assert_eq!((u32le(loc, 0), u64le(loc, 8), u32le(loc, 16)), (ZIP64_LOCATOR_SIG, (5 << 30) + 200, 1));
        let end = &big[ZIP64_END_LEN + ZIP64_LOCATOR_LEN..];
        assert_eq!((u16le(end, 10), u32le(end, 12), u32le(end, 16)), (u16::MAX, 200, u32::MAX));
    }

    #[cfg(unix)]
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

    /// Disk 0 is a sparse file of 4.25 GiB (the split marker, then nothing), so the entry
    /// and the central directory on disk 1 end up above 4 GiB: the rewritten archive
    /// needs a ZIP64 offset and ZIP64 end records, and the `zip` crate must read it.
    #[cfg(unix)]
    #[test]
    fn offsets_above_4_gib_read_through_zip64() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let z01 = dir.path().join("big.z01");
        let mut f = std::fs::File::create(&z01).unwrap();
        std::io::Write::write_all(&mut f, b"PK\x07\x08").unwrap();
        f.set_len((4 << 30) + (1 << 28)).unwrap();
        drop(f);
        if std::fs::metadata(&z01).unwrap().blocks() > 1024 {
            eprintln!("note: skipping test, the temporary folder does not support sparse files");
            return;
        }

        let (name, data) = (b"big.txt", b"hello from above 4 GiB");
        let mut fixed = Vec::new();
        fixed.extend_from_slice(&10u16.to_le_bytes()); // version needed
        fixed.extend_from_slice(&[0u8; 8]); // flags, method (stored), time, date
        fixed.extend_from_slice(&crc32(data).to_le_bytes());
        fixed.extend_from_slice(&(data.len() as u32).to_le_bytes());
        fixed.extend_from_slice(&(data.len() as u32).to_le_bytes());
        fixed.extend_from_slice(&(name.len() as u16).to_le_bytes());
        fixed.extend_from_slice(&0u16.to_le_bytes()); // extra length
        let mut disk1 = 0x0403_4b50u32.to_le_bytes().to_vec();
        disk1.extend_from_slice(&fixed);
        disk1.extend_from_slice(name);
        disk1.extend_from_slice(data);
        let cd_offset = disk1.len() as u32;
        let mut cd = CENTRAL_HEADER_SIG.to_le_bytes().to_vec();
        cd.extend_from_slice(&10u16.to_le_bytes()); // version made by
        cd.extend_from_slice(&fixed);
        cd.extend_from_slice(&0u16.to_le_bytes()); // comment length
        cd.extend_from_slice(&1u16.to_le_bytes()); // disk 1
        cd.extend_from_slice(&[0u8; 6]); // attributes
        cd.extend_from_slice(&0u32.to_le_bytes()); // offset 0 on disk 1
        cd.extend_from_slice(name);
        disk1.extend_from_slice(&cd);
        disk1.extend_from_slice(&END_SIG.to_le_bytes());
        for v in [1u16, 1, 1, 1] {
            disk1.extend_from_slice(&v.to_le_bytes()); // this disk, CD disk, entries x2
        }
        disk1.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        disk1.extend_from_slice(&cd_offset.to_le_bytes());
        disk1.extend_from_slice(&0u16.to_le_bytes());
        let zip = dir.path().join("big.zip");
        std::fs::write(&zip, &disk1).unwrap();

        let reader = open(&[z01, zip]).unwrap();
        assert!(reader.tail.len() > ZIP64_END_LEN + ZIP64_LOCATOR_LEN, "ZIP64 end records expected");
        let config = zip::read::Config { archive_offset: zip::read::ArchiveOffset::Known(0) };
        let mut za = zip::ZipArchive::with_config(config, reader).unwrap();
        let mut out = Vec::new();
        za.by_name("big.txt").unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }
}
