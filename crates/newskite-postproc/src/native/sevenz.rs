//! 7z backend (`sevenz-rust2`): single archives and split `.7z.NNN` volumes.

use std::borrow::Cow;
use std::io::{self, BufReader, Read};

use sevenz_rust2::{ArchiveEntry, ArchiveReader, EncoderMethod, Error, Password};

use super::{apply_mode, apply_mtime, chained_volumes, check_listing, safe_relative, volume_label, CopyError, NativeError, Sink, VolumeReader};
use crate::detect::ArchiveSet;

/// Longest link destination read from a link entry (only used to judge it).
const LINK_TARGET_MAX: u64 = 4096;
/// Windows attribute flags as stored by 7-Zip.
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Set when the high 16 bits hold a Unix mode (p7zip / 7-Zip on Unix).
const FILE_ATTRIBUTE_UNIX_EXTENSION: u32 = 0x8000;
const S_IFMT: u32 = 0o170_000;
const S_IFLNK: u32 = 0o120_000;
/// Sentinel for an error already recorded by the entry callback.
const ABORTED: &str = "newskite: aborted by entry callback";

pub(super) fn extract(set: &ArchiveSet, sink: &mut Sink<'_>, password: Option<&str>) -> Result<(), NativeError> {
    let volumes = chained_volumes(set)?;
    let multi = volumes.len() > 1;
    let open = |pw: Password| -> Result<ArchiveReader<BufReader<VolumeReader>>, NativeError> {
        let reader = VolumeReader::open(&volumes).map_err(|e| NativeError::Io(format!("could not open the archive: {e}")))?;
        ArchiveReader::new(BufReader::with_capacity(64 * 1024, reader), pw).map_err(|e| map_error(e, false, multi))
    };
    let mut ar = match open(password.map(Password::from).unwrap_or_else(Password::empty)) {
        Ok(ar) => ar,
        // A wrong password for an encrypted header decodes to garbage; tell that apart
        // from real damage by checking whether the header needs a password at all.
        Err(e) if password.is_some() && !matches!(e, NativeError::Io(_) | NativeError::Unsupported(_)) => {
            return Err(match open(Password::empty()) {
                Err(NativeError::PasswordRequired) => NativeError::WrongPassword,
                _ => e,
            });
        }
        Err(e) => return Err(e),
    };
    // Multi-threaded LZMA2 decoding is not worth a second core on small hosts.
    ar.set_thread_count(std::thread::available_parallelism().map_or(1, |n| n.get().min(2) as u32));

    // 1. List.
    let files = &ar.archive().files;
    let total = files.iter().filter(|f| !f.is_directory).map(|f| f.size).fold(0u64, u64::saturating_add);
    check_listing(sink, files.iter().filter(|f| !f.is_anti_item).map(|f| f.name.as_str()), total, &volume_label("7z", volumes.len()))?;
    let encrypted = ar
        .archive()
        .blocks
        .iter()
        .any(|b| b.coders.iter().any(|c| c.encoder_method_id() == EncoderMethod::ID_AES256_SHA256));
    if encrypted && password.is_none() {
        return Err(NativeError::PasswordRequired);
    }

    // 2. Extract. Errors from our side are parked in `failed`; the decoder is stopped
    //    with a sentinel error.
    let mut failed: Option<NativeError> = None;
    let res = ar.for_each_entries(|entry, reader| match one_entry(entry, reader, sink) {
        Ok(()) => Ok(true),
        Err(CopyError::Read(e)) => Err(Error::Io(e, Cow::Borrowed(""))),
        Err(CopyError::Write(e)) => {
            failed = Some(e);
            Err(Error::Other(Cow::Borrowed(ABORTED)))
        }
    });
    if let Some(e) = failed {
        return Err(e);
    }
    res.map_err(|e| map_error(e, encrypted, multi))
}

fn one_entry(entry: &ArchiveEntry, reader: &mut dyn Read, sink: &mut Sink<'_>) -> Result<(), CopyError> {
    let drain = |reader: &mut dyn Read| io::copy(reader, &mut io::sink()).map(|_| ()).map_err(CopyError::Read);
    if entry.is_anti_item {
        return drain(reader);
    }
    let Some(rel) = safe_relative(&entry.name).map_err(CopyError::Write)? else { return drain(reader) };
    if entry.is_directory {
        sink.create_dir(&rel).map_err(CopyError::Write)?;
        return drain(reader);
    }
    let attrs = if entry.has_windows_attributes { entry.windows_attributes } else { 0 };
    let unix_mode = (attrs & FILE_ATTRIBUTE_UNIX_EXTENSION != 0).then_some(attrs >> 16);
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 || unix_mode.is_some_and(|m| m & S_IFMT == S_IFLNK) {
        let mut target = Vec::new();
        reader.take(LINK_TARGET_MAX).read_to_end(&mut target).map_err(CopyError::Read)?;
        drain(reader)?;
        sink.link(&entry.name, &String::from_utf8_lossy(&target));
        return Ok(());
    }
    let (path, _) = sink.write_entry(&rel, reader)?;
    apply_mode(&path, unix_mode);
    // NtTime counts from 1601; skip dates before 1970 (unset or bogus).
    const NT_UNIX_EPOCH: u64 = 116_444_736_000_000_000;
    if entry.has_last_modified_date && u64::from(entry.last_modified_date) >= NT_UNIX_EPOCH {
        apply_mtime(&path, Some(entry.last_modified_date.into()));
    }
    Ok(())
}

fn map_error(e: Error, encrypted: bool, multi: bool) -> NativeError {
    match e {
        Error::PasswordRequired => NativeError::PasswordRequired,
        // The decoder says "maybe" whenever a password was given, encrypted or not.
        Error::MaybeBadPassword(_) if encrypted => NativeError::WrongPassword,
        Error::MaybeBadPassword(io) => NativeError::Corrupt(io.to_string().chars().take(120).collect()),
        Error::UnsupportedCompressionMethod(m) => NativeError::Unsupported(format!("7z compression method {m}")),
        Error::ExternalUnsupported => NativeError::Unsupported("7z external streams".into()),
        Error::Unsupported(m) => NativeError::Unsupported(m.into_owned()),
        Error::UnsupportedVersion { major, minor } => NativeError::Unsupported(format!("7z format version {major}.{minor}")),
        Error::MaxMemLimited { .. } => NativeError::Unsupported("7z dictionary larger than the decoder allows".into()),
        Error::Io(io, _) | Error::FileOpen(io, _) if !matches!(io.kind(), io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof | io::ErrorKind::Other) => {
            NativeError::Io(format!("could not read the archive: {io}"))
        }
        Error::Io(io, _) if io.kind() == io::ErrorKind::UnexpectedEof && !encrypted => {
            if multi { NativeError::MissingVolume } else { NativeError::Corrupt("truncated".into()) }
        }
        Error::BadSignature(_) => NativeError::Corrupt("not a 7z archive".into()),
        // Integrity failures of encrypted data almost always mean a wrong password.
        _ if encrypted => NativeError::WrongPassword,
        other => NativeError::Corrupt(format!("{other:?}").chars().take(120).collect()),
    }
}
