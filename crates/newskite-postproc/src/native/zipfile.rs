//! zip backend (`zip` crate): single archives, byte-split `.zip.NNN` volumes and spanned
//! `.z01` ... `.zip` sets (see [`super::spanned`]).

use std::io::{self, BufReader, Read, Seek};

use zip::read::{ArchiveOffset, Config};
use zip::result::ZipError;
use zip::ZipArchive;

use super::{apply_mode, apply_mtime, chained_volumes, check_listing, safe_relative, spanned, volume_label, CopyError, NativeError, Sink, VolumeReader};
use crate::detect::ArchiveSet;

/// Longest link destination read from a link entry (only used to judge it).
const LINK_TARGET_MAX: u64 = 4096;

/// Read buffer in front of the volumes.
const BUFFER: usize = 64 * 1024;

pub(super) fn extract(set: &ArchiveSet, sink: &mut Sink<'_>, password: Option<&str>) -> Result<(), NativeError> {
    if let Some(volumes) = spanned::volumes(set) {
        let volumes = volumes?;
        let reader = spanned::open(&volumes)?;
        // The rewritten archive starts at offset 0 by construction; nothing to detect.
        let config = Config { archive_offset: ArchiveOffset::Known(0) };
        let za = ZipArchive::with_config(config, BufReader::with_capacity(BUFFER, reader)).map_err(|e| open_error(e, true))?;
        return extract_entries(za, sink, password, &format!("zip ({} spanned volumes)", volumes.len()), true);
    }
    let volumes = chained_volumes(set)?;
    let multi = volumes.len() > 1;
    let reader = VolumeReader::open(&volumes).map_err(|e| NativeError::Io(format!("could not open the archive: {e}")))?;
    let za = ZipArchive::new(BufReader::with_capacity(BUFFER, reader)).map_err(|e| open_error(e, multi))?;
    extract_entries(za, sink, password, &volume_label("zip", volumes.len()), multi)
}

/// List and extract every entry of an opened archive. `backend` names it in the log;
/// `multi`: it spans several volumes (a short read means a missing volume).
fn extract_entries<R: Read + Seek>(
    mut za: ZipArchive<R>,
    sink: &mut Sink<'_>,
    password: Option<&str>,
    backend: &str,
    multi: bool,
) -> Result<(), NativeError> {
    // 1. List.
    let mut entries = Vec::with_capacity(za.len());
    let mut total = 0u64;
    for i in 0..za.len() {
        let e = za.by_index_data(i).map_err(|e| open_error(e, multi))?;
        let name = e.name().map(|n| n.into_owned()).unwrap_or_else(|_| String::from_utf8_lossy(e.name_raw()).into_owned());
        if !e.is_dir() {
            total = total.saturating_add(e.size());
        }
        entries.push(name);
    }
    check_listing(sink, entries.iter().map(String::as_str), total, backend)?;

    // 2. Extract.
    for (i, name) in entries.iter().enumerate() {
        let Some(rel) = safe_relative(name)? else { continue };
        let (is_dir, is_link, encrypted, mode) = {
            let e = za.by_index_data(i).map_err(|e| open_error(e, multi))?;
            (e.is_dir(), e.is_symlink(), e.encrypted(), e.unix_mode())
        };
        if is_dir {
            sink.create_dir(&rel)?;
            continue;
        }
        let mut file = if encrypted {
            let Some(pw) = password else { return Err(NativeError::PasswordRequired) };
            za.by_index_decrypt(i, pw.as_bytes())
        } else {
            za.by_index(i)
        }
        .map_err(|e| entry_error(e, encrypted && password.is_some()))?;
        // DOS times in zip have no time zone; only the UTC extended timestamp is used.
        let mtime: Option<u32> = file.extra_data_fields().find_map(|f| match f {
            zip::ExtraField::ExtendedTimestamp(ts) => ts.mod_time(),
            _ => None,
        });
        if is_link {
            let mut target = Vec::new();
            (&mut file).take(LINK_TARGET_MAX).read_to_end(&mut target).map_err(|e| read_error(e, encrypted && password.is_some(), multi))?;
            sink.link(name, &String::from_utf8_lossy(&target));
            continue;
        }
        match sink.write_entry(&rel, &mut file) {
            Ok((path, _)) => {
                apply_mode(&path, mode);
                apply_mtime(&path, mtime.map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::from(s))));
            }
            Err(CopyError::Write(e)) => return Err(e),
            Err(CopyError::Read(e)) => return Err(read_error(e, encrypted && password.is_some(), multi)),
        }
    }
    Ok(())
}

/// Error while reading the central directory or opening an entry.
fn open_error(e: ZipError, multi: bool) -> NativeError {
    match e {
        ZipError::Io(io) => read_error(io, false, multi),
        other => entry_error(other, false),
    }
}

fn entry_error(e: ZipError, password_given: bool) -> NativeError {
    match e {
        ZipError::InvalidPassword => {
            if password_given { NativeError::WrongPassword } else { NativeError::PasswordRequired }
        }
        ZipError::UnsupportedArchive(m) if m == ZipError::PASSWORD_REQUIRED => NativeError::PasswordRequired,
        ZipError::UnsupportedArchive(m) => NativeError::Unsupported(m.to_string()),
        ZipError::CompressionMethodNotSupported(id) => NativeError::Unsupported(format!("zip compression method {id}")),
        ZipError::InvalidArchive(m) => NativeError::Corrupt(m.into_owned()),
        ZipError::Io(io) => read_error(io, password_given, false),
        other => NativeError::Corrupt(other.to_string()),
    }
}

/// Error from the decompressing reader. With ZipCrypto a wrong password passes the
/// one-byte check 1 time in 256 and then fails the CRC; that is reported as a wrong
/// password too.
fn read_error(e: io::Error, encrypted_with_password: bool, multi: bool) -> NativeError {
    if encrypted_with_password && matches!(e.kind(), io::ErrorKind::InvalidData | io::ErrorKind::Other) {
        return NativeError::WrongPassword;
    }
    match e.kind() {
        io::ErrorKind::UnexpectedEof if multi => NativeError::MissingVolume,
        io::ErrorKind::UnexpectedEof => NativeError::Corrupt("truncated".into()),
        io::ErrorKind::InvalidData | io::ErrorKind::Other => NativeError::Corrupt(short(&e)),
        _ => NativeError::Io(format!("could not read the archive: {e}")),
    }
}

fn short(e: &io::Error) -> String {
    e.to_string().chars().take(120).collect()
}
