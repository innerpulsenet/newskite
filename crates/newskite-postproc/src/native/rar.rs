//! RAR backend: the bundled UnRAR library driven through its DLL interface
//! (`unrar_sys`).
//!
//! UnRAR is only ever asked to *test* entries (`RAR_TEST`): it decompresses and checks
//! each entry and hands the data to our callback, which writes it through [`Sink`].
//! UnRAR itself never creates a file, folder or link, so its own path handling (which
//! resolves hard-link and file-copy sources relative to the process's working folder
//! when a destination name is given) never comes into play. Link entries (symbolic
//! links, junctions, hard links) are skipped and logged; file copies (`-oi`) are copied
//! from the already extracted source inside the staging folder.
//!
//! Volumes are opened by UnRAR from the first one (`.partNN.rar`, `.rNN`); a missing
//! volume aborts instead of prompting, and so does a missing password. The password is
//! handed over through the `UCM_NEEDPASSWORDW` callback as a wide string, so non-ASCII
//! passwords survive regardless of the process locale.

use std::ffi::c_int;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::ptr;

use unrar_sys as ffi;

use super::{check_listing, safe_relative, NativeError, Sink};
use crate::detect::ArchiveSet;

// UnRAR reads its settings path from the registry (`GetRarDataPath`), but `unrar_sys`
// does not link the library those functions live in, so MSVC builds fail to link.
#[cfg(windows)]
#[link(name = "advapi32")]
extern "system" {}

/// Longest link destination kept from a RAR 4 link entry's data.
const LINK_TARGET_MAX: usize = 4096;
/// Size of the redirection (link destination) buffer, in wide characters.
const REDIR_BUF: usize = 4096;

/// `FILE_SYSTEM_REDIRECT` values in `RARHeaderDataEx::RedirType`.
const FSREDIR_NONE: u32 = 0;
const FSREDIR_HARDLINK: u32 = 4;
const FSREDIR_FILECOPY: u32 = 5;

pub(super) fn extract(set: &ArchiveSet, sink: &mut Sink<'_>, password: Option<&str>) -> Result<(), NativeError> {
    let pw_given = password.is_some();
    // 1. List (this walks every volume, so a missing one is found before writing).
    let mut entries: Vec<Entry> = Vec::new();
    {
        let st = StatePtr::new(State::new(password, None));
        let arc = Archive::open(&set.first, ffi::RAR_OM_LIST, &st)?;
        let mut hd = HeaderBuf::new();
        while let Some(e) = arc.read_header(&mut hd, &st)? {
            let code = arc.process(ffi::RAR_SKIP);
            check(code, st.get(), e.encrypted || arc.encrypted_headers, pw_given)?;
            entries.push(e);
        }
    }
    let total = entries.iter().filter(|e| !e.dir).map(|e| e.size).fold(0u64, u64::saturating_add);
    check_listing(sink, entries.iter().map(|e| e.name.as_str()), total, "rar")?;

    // 2. Test every entry, writing the data it yields.
    let st = StatePtr::new(State::new(password, Some(sink)));
    let arc = Archive::open(&set.first, ffi::RAR_OM_EXTRACT, &st)?;
    let mut hd = HeaderBuf::new();
    let skip = |enc: bool| check(arc.process(ffi::RAR_SKIP), st.get(), enc, pw_given);
    while let Some(e) = arc.read_header(&mut hd, &st)? {
        let enc = e.encrypted || arc.encrypted_headers;
        let Some(rel) = safe_relative(&e.name)? else {
            skip(enc)?;
            continue;
        };
        if e.dir {
            st.get().sink().create_dir(&rel)?;
            skip(enc)?;
            continue;
        }
        match e.redir {
            FSREDIR_NONE => {}
            FSREDIR_FILECOPY => {
                skip(enc)?;
                copy_within(st.get().sink(), &e, &rel)?;
                continue;
            }
            FSREDIR_HARDLINK => {
                skip(enc)?;
                st.get().sink().link(&e.name, &e.redir_name);
                continue;
            }
            _ if !e.redir_name.is_empty() => {
                skip(enc)?;
                st.get().sink().link(&e.name, &e.redir_name);
                continue;
            }
            _ => {
                // RAR 4 Unix link: the destination is the entry's data.
                st.get().out = Out::Mem(Vec::new());
                let code = arc.process(ffi::RAR_TEST);
                let target = match std::mem::replace(&mut st.get().out, Out::Discard) {
                    Out::Mem(v) => String::from_utf8_lossy(&v).into_owned(),
                    _ => String::new(),
                };
                check(code, st.get(), enc, pw_given)?;
                st.get().sink().link(&e.name, &target);
                continue;
            }
        }
        let (path, file) = st.get().sink().create_file(&rel)?;
        st.get().out = Out::File { file, rel: rel.clone(), written: 0 };
        let code = arc.process(ffi::RAR_TEST);
        let written = match std::mem::replace(&mut st.get().out, Out::Discard) {
            Out::File { written, .. } => written,
            _ => 0,
        };
        if let Err(err) = check(code, st.get(), enc, pw_given) {
            let _ = std::fs::remove_file(&path);
            return Err(err);
        }
        st.get().sink().file_done(written);
        #[cfg(unix)]
        if e.unix_host {
            super::apply_mode(&path, Some(e.attr));
        }
        super::apply_mtime(&path, e.mtime);
    }
    Ok(())
}

/// A file copy entry: duplicate the already extracted source (inside the staging
/// folder, path sanitized like any entry name). A missing source is reported.
fn copy_within(sink: &mut Sink<'_>, e: &Entry, rel: &Path) -> Result<(), NativeError> {
    let src = match safe_relative(&e.redir_name) {
        Ok(Some(s)) => sink.path_of(&s),
        _ => return Err(NativeError::Corrupt(format!("file copy {} has an invalid source", e.name))),
    };
    if !std::fs::symlink_metadata(&src).is_ok_and(|m| m.is_file()) {
        return Err(NativeError::Corrupt(format!("source of file copy {} was not extracted", e.name)));
    }
    let (path, mut out) = sink.create_file(rel)?;
    let n = File::open(&src)
        .and_then(|mut f| std::io::copy(&mut f, &mut out))
        .map_err(|err| NativeError::output(rel, &err))?;
    drop(out);
    sink.advance(n);
    sink.file_done(n);
    super::apply_mtime(&path, e.mtime);
    Ok(())
}

/// One archive entry from `RARReadHeaderEx`.
#[cfg_attr(not(unix), allow(dead_code))]
struct Entry {
    name: String,
    size: u64,
    dir: bool,
    encrypted: bool,
    redir: u32,
    redir_name: String,
    attr: u32,
    unix_host: bool,
    mtime: Option<std::time::SystemTime>,
}

/// Where entry data from the callback goes.
enum Out {
    Discard,
    Mem(Vec<u8>),
    File { file: File, rel: PathBuf, written: u64 },
}

/// Callback state shared with UnRAR through `user_data` (owned by a [`StatePtr`]).
struct State<'s, 'a> {
    /// NUL-terminated wide password, wiped on drop.
    password: Option<Vec<ffi::WCHAR>>,
    password_requested: bool,
    missing_volume: bool,
    out: Out,
    write_error: Option<NativeError>,
    sink: Option<&'s mut Sink<'a>>,
}

impl<'s, 'a> State<'s, 'a> {
    fn new(password: Option<&str>, sink: Option<&'s mut Sink<'a>>) -> Self {
        Self {
            password: password.map(to_wide),
            password_requested: false,
            missing_volume: false,
            out: Out::Discard,
            write_error: None,
            sink,
        }
    }

    fn sink(&mut self) -> &mut Sink<'a> {
        self.sink.as_deref_mut().expect("sink present while extracting")
    }

    /// Handle one callback message; the return value goes back to UnRAR (-1 aborts).
    fn on_message(&mut self, msg: ffi::UINT, p1: ffi::LPARAM, p2: ffi::LPARAM) -> c_int {
        match msg {
            ffi::UCM_CHANGEVOLUME | ffi::UCM_CHANGEVOLUMEW => {
                if p2 == ffi::RAR_VOL_ASK {
                    // The next volume does not exist; asking would mean a prompt.
                    self.missing_volume = true;
                    -1
                } else {
                    0
                }
            }
            ffi::UCM_NEEDPASSWORDW => match &self.password {
                Some(pw) if p1 != 0 && p2 > 1 => {
                    let cap = p2 as usize;
                    let n = (pw.len() - 1).min(cap - 1);
                    // SAFETY: UnRAR passes a buffer of `p2` wide characters at `p1`.
                    unsafe {
                        let dst = p1 as *mut ffi::WCHAR;
                        ptr::copy_nonoverlapping(pw.as_ptr(), dst, n);
                        *dst.add(n) = 0;
                    }
                    1
                }
                _ => {
                    self.password_requested = true;
                    -1
                }
            },
            // Only reached when the wide request returned nothing.
            ffi::UCM_NEEDPASSWORD => {
                self.password_requested = true;
                -1
            }
            ffi::UCM_PROCESSDATA => {
                if p1 == 0 || p2 <= 0 {
                    return 0;
                }
                // SAFETY: UnRAR passes `p2` bytes of unpacked data at `p1`.
                let data = unsafe { std::slice::from_raw_parts(p1 as *const u8, p2 as usize) };
                match &mut self.out {
                    Out::Discard => 0,
                    Out::Mem(v) => {
                        let room = LINK_TARGET_MAX.saturating_sub(v.len());
                        v.extend_from_slice(&data[..data.len().min(room)]);
                        0
                    }
                    Out::File { file, rel, written } => match file.write_all(data) {
                        Ok(()) => {
                            *written += data.len() as u64;
                            if let Some(s) = self.sink.as_deref_mut() {
                                s.advance(data.len() as u64);
                            }
                            0
                        }
                        Err(e) => {
                            self.write_error = Some(NativeError::output(rel, &e));
                            -1
                        }
                    },
                }
            }
            _ => 0,
        }
    }
}

/// Owner of a heap-allocated [`State`]. UnRAR keeps a raw pointer to it, so all Rust
/// access goes through the same raw pointer ([`StatePtr::get`]), never through a
/// reference held across an UnRAR call. Declared before the [`Archive`] using it, so
/// the handle is closed first.
struct StatePtr<'s, 'a>(*mut State<'s, 'a>);

impl<'s, 'a> StatePtr<'s, 'a> {
    fn new(st: State<'s, 'a>) -> Self {
        Self(Box::into_raw(Box::new(st)))
    }

    #[allow(clippy::mut_from_ref)]
    fn get(&self) -> &mut State<'s, 'a> {
        // SAFETY: owned allocation; UnRAR only touches it during our calls into it, and
        // callers never hold the returned reference across such a call.
        unsafe { &mut *self.0 }
    }
}

impl Drop for StatePtr<'_, '_> {
    fn drop(&mut self) {
        // SAFETY: allocated by `Box::into_raw` in `new`, freed once.
        drop(unsafe { Box::from_raw(self.0) });
    }
}

impl Drop for State<'_, '_> {
    fn drop(&mut self) {
        if let Some(pw) = &mut self.password {
            pw.iter_mut().for_each(|c| *c = 0);
        }
    }
}

extern "C" fn callback(msg: ffi::UINT, user_data: ffi::LPARAM, p1: ffi::LPARAM, p2: ffi::LPARAM) -> c_int {
    if user_data == 0 {
        return -1;
    }
    // A panic must never unwind into C++.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `user_data` is the `State` registered by `Archive::open`, which
        // outlives the handle; UnRAR calls back only from inside our own calls.
        let st = unsafe { &mut *(user_data as *mut State<'_, '_>) };
        st.on_message(msg, p1, p2)
    }))
    .unwrap_or(-1)
}

/// An open archive handle (closed on drop).
struct Archive {
    handle: *const ffi::Handle,
    encrypted_headers: bool,
}

impl Archive {
    /// Open `path` and register `st` for callbacks. `st` must outlive the returned
    /// value (callers declare it first, so it is dropped last).
    fn open(path: &Path, mode: u32, st: &StatePtr<'_, '_>) -> Result<Self, NativeError> {
        let user_data = st.0 as ffi::LPARAM;
        let name = ArcName::new(path)?;
        let mut data = name.open_data(mode);
        data.callback = Some(callback);
        data.user_data = user_data;
        // SAFETY: `data` and the name buffer it points to live across the call. UnRAR
        // writes its results into `data` (the binding declares the pointer `*const`).
        let handle = unsafe { ffi::RAROpenArchiveEx(ptr::addr_of_mut!(data).cast_const()) };
        let code = data.open_result as c_int;
        if handle.is_null() || code != ffi::ERAR_SUCCESS {
            if !handle.is_null() {
                // SAFETY: a handle returned by RAROpenArchiveEx.
                unsafe { ffi::RARCloseArchive(handle) };
            }
            return Err(match code {
                ffi::ERAR_EOPEN => NativeError::Io(format!("could not open {}", path.display())),
                ffi::ERAR_BAD_ARCHIVE => NativeError::Corrupt("not a RAR archive".into()),
                _ => {
                    let st = st.get();
                    error_for(code, st, data.flags & ffi::ROADF_ENCHEADERS != 0, st.password.is_some())
                }
            });
        }
        // SAFETY: valid handle; `user_data` outlives it (see above).
        unsafe { ffi::RARSetCallback(handle, Some(callback), user_data) };
        Ok(Self { handle, encrypted_headers: data.flags & ffi::ROADF_ENCHEADERS != 0 })
    }

    /// Next entry header, `None` at the end of the archive.
    fn read_header(&self, hd: &mut HeaderBuf, st: &StatePtr<'_, '_>) -> Result<Option<Entry>, NativeError> {
        hd.reset();
        // SAFETY: valid handle; `hd` points to a live header with its redirection buffer.
        let code = unsafe { ffi::RARReadHeaderEx(self.handle, ptr::addr_of_mut!(*hd.header).cast_const()) };
        match code {
            ffi::ERAR_SUCCESS => Ok(Some(hd.entry())),
            ffi::ERAR_END_ARCHIVE => Ok(None),
            _ => {
                let st = st.get();
                Err(error_for(code, st, self.encrypted_headers, st.password.is_some()))
            }
        }
    }

    /// Process the entry whose header was just read (`RAR_SKIP` or `RAR_TEST`).
    fn process(&self, op: c_int) -> c_int {
        // SAFETY: valid handle; no destination paths (nothing is written by UnRAR).
        unsafe { ffi::RARProcessFile(self.handle, op, ptr::null(), ptr::null()) }
    }
}

impl Drop for Archive {
    fn drop(&mut self) {
        // SAFETY: a handle returned by RAROpenArchiveEx, closed once.
        unsafe { ffi::RARCloseArchive(self.handle) };
    }
}

/// Map a result code of a process call.
fn check(code: c_int, st: &State<'_, '_>, encrypted: bool, password_given: bool) -> Result<(), NativeError> {
    if code == ffi::ERAR_SUCCESS {
        Ok(())
    } else {
        Err(error_for(code, st, encrypted, password_given))
    }
}

fn error_for(code: c_int, st: &State<'_, '_>, encrypted: bool, password_given: bool) -> NativeError {
    if let Some(e) = &st.write_error {
        return e.clone();
    }
    if st.missing_volume || code == ffi::ERAR_EOPEN {
        return NativeError::MissingVolume;
    }
    if st.password_requested || code == ffi::ERAR_MISSING_PASSWORD {
        return NativeError::PasswordRequired;
    }
    match code {
        ffi::ERAR_BAD_PASSWORD => NativeError::WrongPassword,
        // RAR 4 has no password check value: a wrong password shows up as a CRC error.
        ffi::ERAR_BAD_DATA if encrypted && password_given => NativeError::WrongPassword,
        ffi::ERAR_BAD_DATA => NativeError::Corrupt("CRC error or damaged header".into()),
        ffi::ERAR_BAD_ARCHIVE => NativeError::Corrupt("not a RAR archive".into()),
        ffi::ERAR_UNKNOWN_FORMAT => NativeError::Unsupported("unknown RAR format or encryption".into()),
        ffi::ERAR_NO_MEMORY => NativeError::Io("not enough memory to extract the RAR archive".into()),
        ffi::ERAR_EREAD => NativeError::Io("could not read the archive".into()),
        ffi::ERAR_EREFERENCE => NativeError::Corrupt("file reference without its source".into()),
        other => NativeError::Corrupt(format!("UnRAR error {other}")),
    }
}

/// `RARHeaderDataEx` (large, so boxed) plus the buffer for link destinations.
struct HeaderBuf {
    header: Box<ffi::HeaderDataEx>,
    redir: Vec<ffi::WCHAR>,
}

impl HeaderBuf {
    fn new() -> Self {
        Self { header: Box::default(), redir: vec![0; REDIR_BUF] }
    }

    fn reset(&mut self) {
        *self.header = ffi::HeaderDataEx::default();
        self.redir.iter_mut().for_each(|c| *c = 0);
        self.header.redir_name = self.redir.as_mut_ptr();
        self.header.redir_name_size = self.redir.len() as u32;
    }

    fn entry(&self) -> Entry {
        let h = &*self.header;
        // HOST_UNIX in the DLL's header data.
        const HOST_UNIX: u32 = 3;
        let mtime_ft = (u64::from(h.mtime_high) << 32) | u64::from(h.mtime_low);
        Entry {
            name: from_wide(&h.filename_w),
            size: (u64::from(h.unp_size_high) << 32) | u64::from(h.unp_size),
            dir: h.flags & ffi::RHDF_DIRECTORY != 0,
            encrypted: h.flags & ffi::RHDF_ENCRYPTED != 0,
            redir: h.redir_type,
            redir_name: from_wide(&self.redir),
            attr: h.file_attr,
            unix_host: h.host_os == HOST_UNIX,
            mtime: filetime_to_system(mtime_ft),
        }
    }
}

/// Windows FILETIME (100 ns since 1601) to `SystemTime`; `None` before 1970.
fn filetime_to_system(ft: u64) -> Option<std::time::SystemTime> {
    const UNIX_EPOCH_FT: u64 = 116_444_736_000_000_000;
    let since = ft.checked_sub(UNIX_EPOCH_FT)?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(since.saturating_mul(100)))
}

/// Wide (`wchar_t`) buffer up to its NUL to a `String`.
fn from_wide(buf: &[ffi::WCHAR]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    #[cfg(windows)]
    {
        String::from_utf16_lossy(&buf[..end])
    }
    #[cfg(not(windows))]
    {
        buf[..end].iter().map(|&c| char::from_u32(c as u32).unwrap_or('\u{fffd}')).collect()
    }
}

/// NUL-terminated wide string.
fn to_wide(s: &str) -> Vec<ffi::WCHAR> {
    #[cfg(windows)]
    let mut v: Vec<ffi::WCHAR> = s.encode_utf16().collect();
    #[cfg(not(windows))]
    let mut v: Vec<ffi::WCHAR> = s.chars().map(|c| c as ffi::WCHAR).collect();
    v.push(0);
    v
}

/// The archive name in the form the platform's `RAROpenArchiveEx` expects: narrow
/// bytes on Linux/NetBSD (as the `unrar` crate does), wide elsewhere.
struct ArcName {
    #[cfg(any(target_os = "linux", target_os = "netbsd"))]
    narrow: std::ffi::CString,
    #[cfg(not(any(target_os = "linux", target_os = "netbsd")))]
    wide: Vec<ffi::WCHAR>,
}

impl ArcName {
    fn new(path: &Path) -> Result<Self, NativeError> {
        #[cfg(any(target_os = "linux", target_os = "netbsd"))]
        {
            let narrow = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
                .map_err(|_| NativeError::Io(format!("invalid archive path {}", path.display())))?;
            Ok(Self { narrow })
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            let mut wide: Vec<ffi::WCHAR> = path.as_os_str().encode_wide().collect();
            wide.push(0);
            Ok(Self { wide })
        }
        #[cfg(not(any(target_os = "linux", target_os = "netbsd", windows)))]
        {
            Ok(Self { wide: to_wide(&path.to_string_lossy()) })
        }
    }

    fn open_data(&self, mode: u32) -> ffi::OpenArchiveDataEx {
        #[cfg(any(target_os = "linux", target_os = "netbsd"))]
        {
            ffi::OpenArchiveDataEx::new(self.narrow.as_ptr(), mode)
        }
        #[cfg(not(any(target_os = "linux", target_os = "netbsd")))]
        {
            ffi::OpenArchiveDataEx::new(self.wide.as_ptr(), mode)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_round_trip() {
        let w = to_wide("pässwörd ❤");
        assert_eq!(*w.last().unwrap(), 0);
        assert_eq!(from_wide(&w), "pässwörd ❤");
    }

    #[test]
    fn filetime_conversion() {
        assert_eq!(filetime_to_system(0), None);
        let t = filetime_to_system(116_444_736_000_000_000 + 10_000_000).unwrap();
        assert_eq!(t, std::time::UNIX_EPOCH + std::time::Duration::from_secs(1));
    }
}
