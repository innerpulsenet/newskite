//! Positional file I/O: read or write at an absolute offset without moving a shared
//! cursor, so several threads can use one handle at once; and files opened on demand.

use std::fs::File;
use std::io;
use std::path::Path;

/// Where to read from: a handle the caller keeps open, or a path opened when needed, so
/// working through thousands of files never holds thousands of descriptors.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Source<'a> {
    Open(&'a File),
    Path(&'a Path),
}

/// The file one thread opened last, kept for the next read from the same file.
#[derive(Default)]
pub(crate) struct LastFile {
    key: usize,
    file: Option<File>,
}

impl LastFile {
    /// The handle for `src`; a path is opened unless it is the one opened last (`key`
    /// identifies it).
    pub(crate) fn get<'s>(&'s mut self, key: usize, src: Source<'s>) -> io::Result<&'s File> {
        match src {
            Source::Open(f) => Ok(f),
            Source::Path(p) => {
                if self.file.is_none() || self.key != key {
                    self.file = None;
                    self.file = Some(File::open(p)?);
                    self.key = key;
                }
                Ok(self.file.as_ref().expect("opened above"))
            }
        }
    }
}

/// Reads up to `buf.len()` bytes at `offset`, zero-filling the rest (past the end of the
/// file). Returns the bytes read.
pub(crate) fn read_at_padded(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match read_at(file, &mut buf[n..], offset + n as u64) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf[n..].fill(0);
    Ok(n)
}

/// Writes all of `buf` at `offset`.
pub(crate) fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match write_at(file, buf, offset) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write whole buffer")),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}

/// `seek_read` moves the handle's cursor as a side effect, which is harmless: every access
/// through this module names its offset.
#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(windows)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, offset)
}

/// Fallback for targets without positional I/O: seek + read/write under a global lock.
#[cfg(not(any(unix, windows)))]
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(not(any(unix, windows)))]
fn read_at(mut file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    file.seek(SeekFrom::Start(offset))?;
    file.read(buf)
}

#[cfg(not(any(unix, windows)))]
fn write_at(mut file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::io::{Seek, SeekFrom, Write};
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    file.seek(SeekFrom::Start(offset))?;
    file.write(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        write_all_at(&f, b"world", 6).unwrap();
        write_all_at(&f, b"hello ", 0).unwrap();
        let mut buf = [0xffu8; 8];
        assert_eq!(read_at_padded(&f, 6, &mut buf).unwrap(), 5);
        assert_eq!(&buf, b"world\0\0\0");
        assert_eq!(read_at_padded(&f, 100, &mut buf).unwrap(), 0);
        assert_eq!(buf, [0; 8]);
    }
}
