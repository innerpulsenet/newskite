//! Desktop integration without helper programs: file pickers, opening folders, files and
//! web pages.
//!
//! On Linux and the BSDs everything goes over D-Bus with the pure-Rust `zbus` client the
//! theme watcher already uses: the XDG desktop portal's `FileChooser` and `OpenURI`
//! interfaces, and the file manager's `org.freedesktop.FileManager1` for folders. No
//! `xdg-open`, `zenity` or `kdialog` is run and no system `libdbus` is loaded. On Windows
//! and macOS the platform APIs are reached through `rfd` and `open`.
//!
//! Every call blocks until the desktop answers (a picker until the user closes it), so
//! callers run them off the UI thread.

use std::path::{Path, PathBuf};

/// What to pick.
#[derive(Clone, Debug, Default)]
pub struct Pick {
    /// Dialog title.
    pub title: String,
    /// Pick folders instead of files.
    pub directory: bool,
    /// Allow several.
    pub multiple: bool,
    /// One filter: its name and the extensions it accepts (without dots).
    pub filter: Option<(String, Vec<String>)>,
    /// Folder the dialog starts in.
    pub start: Option<PathBuf>,
}

#[cfg(unix)]
/// `path` as a `file://` URI: every byte outside the unreserved set and `/` is
/// percent-encoded, so any file name survives the round trip.
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut s = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// The local path of a `file://` URI (`None` for any other scheme, another host or a
/// bad escape); the launcher's parser, which file managers' URIs already go through.
pub fn uri_path(uri: &str) -> Option<PathBuf> {
    crate::launch::file_uri_path(uri.strip_prefix("file:")?).ok()
}

/// The object path a portal request with `token` gets for the connection named
/// `unique_name` (":1.42" → `/org/freedesktop/portal/desktop/request/1_42/<token>`).
pub fn request_path(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::{Fd, OwnedObjectPath, OwnedValue, Value};

    use super::{file_uri, request_path, uri_path, Pick};

    const PORTAL: &str = "org.freedesktop.portal.Desktop";
    const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

    fn err(e: impl std::fmt::Display) -> String {
        e.to_string()
    }

    fn token() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        format!("newskite_{}_{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
    }

    pub fn pick(p: &Pick) -> Result<Option<Vec<PathBuf>>, String> {
        let conn = Connection::session().map_err(err)?;
        let token = token();
        let unique = conn.unique_name().map(|n| n.to_string()).unwrap_or_default();
        // Listen on the request path before asking, so a fast answer cannot be missed.
        let expected = request_path(&unique, &token);
        let listen = |path: &str| -> Result<_, String> {
            Proxy::new(&conn, PORTAL, path.to_owned(), "org.freedesktop.portal.Request")
                .map_err(err)?
                .receive_signal("Response")
                .map_err(err)
        };
        let mut responses = listen(&expected)?;

        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        options.insert("modal", Value::from(true));
        options.insert("multiple", Value::from(p.multiple));
        options.insert("directory", Value::from(p.directory));
        if let Some((name, exts)) = &p.filter {
            // Glob patterns (type 0), matched case-insensitively by spelling both cases.
            let mut globs: Vec<(u32, String)> = Vec::new();
            for e in exts {
                for g in [format!("*.{}", e.to_lowercase()), format!("*.{}", e.to_uppercase())] {
                    if !globs.iter().any(|(_, x)| *x == g) {
                        globs.push((0, g));
                    }
                }
            }
            options.insert("filters", Value::from(vec![(name.clone(), globs)]));
        }
        if let Some(dir) = &p.start {
            use std::os::unix::ffi::OsStrExt;
            let mut bytes = dir.as_os_str().as_bytes().to_vec();
            bytes.push(0);
            options.insert("current_folder", Value::from(bytes));
        }
        let chooser = Proxy::new(&conn, PORTAL, PORTAL_PATH, "org.freedesktop.portal.FileChooser").map_err(err)?;
        let handle: OwnedObjectPath = chooser
            .call("OpenFile", &("", p.title.as_str(), options))
            .map_err(err)?;
        // Portals older than 0.9 pick the path themselves.
        if handle.as_str() != expected {
            responses = listen(handle.as_str())?;
        }
        let msg = responses.next().ok_or("the portal closed the request without an answer")?;
        let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize().map_err(err)?;
        if code != 0 {
            return Ok(None); // 1: canceled, 2: ended another way.
        }
        let uris: Vec<String> = match results.get("uris") {
            Some(v) => Vec::<String>::try_from(v.try_clone().map_err(err)?).map_err(err)?,
            None => Vec::new(),
        };
        Ok(Some(uris.iter().filter_map(|u| uri_path(u)).collect()))
    }

    pub fn open_url(url: &str) -> Result<(), String> {
        let conn = Connection::session().map_err(err)?;
        let proxy = Proxy::new(&conn, PORTAL, PORTAL_PATH, "org.freedesktop.portal.OpenURI").map_err(err)?;
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        let _: OwnedObjectPath = proxy.call("OpenURI", &("", url, options)).map_err(err)?;
        Ok(())
    }

    /// A local file or folder in its default application, handed over as an open file
    /// descriptor (the portal's way for local paths).
    fn open_local(conn: &Connection, path: &Path) -> Result<(), String> {
        let file = std::fs::File::open(path).map_err(err)?;
        let proxy = Proxy::new(conn, PORTAL, PORTAL_PATH, "org.freedesktop.portal.OpenURI").map_err(err)?;
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        let _: OwnedObjectPath = proxy.call("OpenFile", &("", Fd::from(&file), options)).map_err(err)?;
        Ok(())
    }

    pub fn show_folder(dir: &Path) -> Result<(), String> {
        let conn = Connection::session().map_err(err)?;
        let fm = Proxy::new(&conn, "org.freedesktop.FileManager1", "/org/freedesktop/FileManager1", "org.freedesktop.FileManager1")
            .and_then(|p| p.call::<_, _, ()>("ShowFolders", &(vec![file_uri(dir)], "")));
        match fm {
            Ok(()) => Ok(()),
            // No file manager answers on the bus: the portal opens the folder's handler.
            Err(_) => open_local(&conn, dir),
        }
    }

    pub fn open_file(path: &Path) -> Result<(), String> {
        let conn = Connection::session().map_err(err)?;
        open_local(&conn, path)
    }

    pub fn show_item(path: &Path) -> Result<(), String> {
        let conn = Connection::session().map_err(err)?;
        let fm = Proxy::new(&conn, "org.freedesktop.FileManager1", "/org/freedesktop/FileManager1", "org.freedesktop.FileManager1")
            .and_then(|p| p.call::<_, _, ()>("ShowItems", &(vec![file_uri(path)], "")));
        match (fm, path.parent()) {
            (Ok(()), _) => Ok(()),
            // No file manager answers on the bus: the portal opens the folder's handler.
            (Err(_), Some(dir)) => open_local(&conn, dir),
            (Err(e), None) => Err(err(e)),
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use std::path::{Path, PathBuf};

    use super::Pick;

    pub fn pick(p: &Pick) -> Result<Option<Vec<PathBuf>>, String> {
        let mut d = rfd::FileDialog::new().set_title(&p.title);
        if let Some((name, exts)) = &p.filter {
            d = d.add_filter(name, exts);
        }
        if let Some(dir) = &p.start {
            d = d.set_directory(dir);
        }
        Ok(match (p.directory, p.multiple) {
            (true, _) => d.pick_folder().map(|f| vec![f]),
            (false, true) => d.pick_files(),
            (false, false) => d.pick_file().map(|f| vec![f]),
        })
    }

    pub fn open_url(url: &str) -> Result<(), String> {
        open::that_detached(url).map_err(|e| e.to_string())
    }

    pub fn show_folder(dir: &Path) -> Result<(), String> {
        open::that_detached(dir).map_err(|e| e.to_string())
    }

    pub fn open_file(path: &Path) -> Result<(), String> {
        open::that_detached(path).map_err(|e| e.to_string())
    }

    pub fn show_item(path: &Path) -> Result<(), String> {
        show_folder(path.parent().unwrap_or(path))
    }
}

/// Shows a picker; `Ok(None)` when the user canceled.
pub fn pick(p: &Pick) -> Result<Option<Vec<PathBuf>>, String> {
    imp::pick(p)
}

/// Opens a web page in the browser.
pub fn open_url(url: &str) -> Result<(), String> {
    imp::open_url(url)
}

/// Shows a folder in the file manager.
pub fn show_folder(dir: &Path) -> Result<(), String> {
    imp::show_folder(dir)
}

/// Opens a file in its default application.
pub fn open_file(path: &Path) -> Result<(), String> {
    imp::open_file(path)
}

/// Shows a file selected in its folder in the file manager (where the platform allows;
/// otherwise its folder), without opening it.
pub fn show_item(path: &Path) -> Result<(), String> {
    imp::show_item(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn file_uris_round_trip_any_name() {
        for p in ["/home/a/Downloads/Newskite", "/tmp/with space/ünï #1%?.nzb", "/"] {
            let uri = file_uri(Path::new(p));
            assert!(uri.starts_with("file:///"), "{uri}");
            assert!(!uri.contains(' ') && !uri.contains('#') && !uri.contains('?'), "{uri}");
            assert_eq!(uri_path(&uri).as_deref(), Some(Path::new(p)));
        }
        assert_eq!(uri_path("file://localhost/x%20y").as_deref(), Some(Path::new("/x y")));
        assert_eq!(uri_path("file://otherhost/x"), None);
        assert_eq!(uri_path("https://example.org/x"), None);
        assert_eq!(uri_path("file:///bad%zz"), None);
        assert_eq!(uri_path("file:///cut%2"), None);
    }

    #[test]
    fn request_paths_follow_the_portal_rule() {
        assert_eq!(request_path(":1.42", "t_1"), "/org/freedesktop/portal/desktop/request/1_42/t_1");
    }
}
