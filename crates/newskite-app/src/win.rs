//! The few Windows calls the app makes that the standard library and eframe do not cover.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

/// Joins the console of the process that started us, if it has one. The binary is built
/// for the Windows GUI subsystem (no console window behind the app), so `--headless`,
/// `--help` and `--version` attach to the terminal they were typed in to print there.
/// Started from Explorer there is no such console and this does nothing, and output the
/// parent already redirected (to a file or a pipe) is left where it goes.
pub fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
    };
    // SAFETY: no pointers are passed; both calls fail harmlessly.
    unsafe {
        if GetStdHandle(STD_OUTPUT_HANDLE).is_null() {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

/// Lets any process bring its window to the front. Called by a launch that hands its NZBs
/// to the window already open: the launch has the user's permission to take the
/// foreground, and passing it on is what lets that window raise itself.
pub fn allow_any_foreground() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};
    // SAFETY: no pointers; a refusal only means the window stays where it is.
    unsafe {
        AllowSetForegroundWindow(ASFW_ANY);
    }
}

/// `(available to this user, total)` bytes on the volume holding the existing directory
/// `dir`.
pub fn disk_space(dir: &Path) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let (mut free, mut total) = (0u64, 0u64);
    // SAFETY: `wide` is NUL-terminated and outlives the call; the out pointers are valid
    // u64s, and the last (total free) is optional.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
    (ok != 0).then_some((free, total))
}
