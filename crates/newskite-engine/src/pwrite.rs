//! Cross-platform positional I/O: write (or read) a buffer at an absolute file offset
//! without moving a shared cursor, so several segments can be written to one file
//! concurrently.

use std::fs::File;
use std::io;

/// Writes all of `buf` at byte `offset` of `file`.
#[cfg(unix)]
pub(crate) fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

/// Writes all of `buf` at byte `offset` of `file`.
///
/// `seek_write` moves the handle's cursor as a side effect, which is harmless because the
/// engine only ever writes positionally.
#[cfg(windows)]
pub(crate) fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write whole buffer"));
            }
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

/// Fallback for targets without positional I/O: seek + write under a global lock.
#[cfg(not(any(unix, windows)))]
pub(crate) fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut f = file;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(buf)
}

/// Reads exactly `buf.len()` bytes at byte `offset` of `file`.
#[cfg(unix)]
pub(crate) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

/// Reads exactly `buf.len()` bytes at byte `offset` of `file`.
#[cfg(windows)]
pub(crate) fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "failed to fill whole buffer")),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Fallback for targets without positional I/O: seek + read.
#[cfg(not(any(unix, windows)))]
pub(crate) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = file;
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn out_of_order_writes_assemble() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        let file = File::create(&path).unwrap();
        write_all_at(&file, b"world", 6).unwrap();
        write_all_at(&file, b"hello ", 0).unwrap();
        drop(file);
        let mut s = String::new();
        File::open(&path).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello world");
        let file = File::open(&path).unwrap();
        let mut buf = [0u8; 5];
        read_exact_at(&file, &mut buf, 6).unwrap();
        assert_eq!(&buf, b"world");
        assert!(read_exact_at(&file, &mut buf, 8).is_err(), "short read is an error");
    }
}
