//! Command-line handling for the GUI binary: `--headless` dispatch and the NZB paths /
//! `file://` / `nzb://` URIs a desktop file manager passes on launch.

use std::ffi::OsString;
use std::path::PathBuf;

/// What the process should do.
#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    /// Run the headless CLI with these arguments (argv[0] first, `--headless` removed).
    Headless(Vec<OsString>),
    /// Open the window and queue these items.
    Gui(Vec<LaunchItem>),
    /// Print usage and exit 0.
    Help,
    /// Print the version and exit 0.
    Version,
    /// Bad arguments: print this message and exit 2.
    Usage(String),
}

/// One positional launch argument.
#[derive(Debug, PartialEq, Eq)]
pub enum LaunchItem {
    /// A local NZB file to import.
    Nzb(PathBuf),
    /// An argument that cannot be imported, with the reason (logged in the app).
    Unsupported {
        /// The original argument.
        arg: String,
        /// Why it was not imported.
        reason: String,
    },
}

/// Usage text for the GUI binary.
pub const USAGE: &str = "\
Usage:
  newskite [FILE.nzb | file://URI | nzb://URI]...   open the window, queueing the NZBs
  newskite --headless --nzb FILE --dest DIR [...]   download without a window (see --headless --help)

Options:
  -h, --help       show this help
  -V, --version    show the version";

/// Parses the process arguments (including argv[0]).
pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Mode {
    let mut args: Vec<OsString> = args.into_iter().collect();
    if args.iter().skip(1).any(|a| a == "--headless") {
        let argv0 = OsString::from("newskite --headless");
        let rest = args.drain(1..).filter(|a| a != "--headless");
        return Mode::Headless(std::iter::once(argv0).chain(rest).collect());
    }
    let mut items = Vec::new();
    let mut options_done = false;
    for a in args.into_iter().skip(1) {
        let s = a.to_string_lossy().into_owned();
        if !options_done {
            match s.as_str() {
                "-h" | "--help" => return Mode::Help,
                "-V" | "--version" => return Mode::Version,
                "--" => {
                    options_done = true;
                    continue;
                }
                o if o.starts_with('-') && o.len() > 1 => {
                    return Mode::Usage(format!("unknown option: {o}"));
                }
                _ => {}
            }
        }
        // Plain paths keep their OS encoding; URIs are decoded.
        if looks_like_uri(&s) {
            items.push(parse_item(&s));
        } else {
            items.push(LaunchItem::Nzb(PathBuf::from(a)));
        }
    }
    Mode::Gui(items)
}

fn looks_like_uri(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.starts_with("file:")
        || lower.starts_with("nzb:")
        || lower.starts_with("nzblnk:")
        || lower.starts_with("http://")
        || lower.starts_with("https://")
}

/// Decodes `%XX` escapes into raw bytes; `None` for a malformed escape (anything but
/// two ASCII hex digits after `%`).
pub fn percent_decode_bytes(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = bytes.get(i + 1).copied().filter(u8::is_ascii_hexdigit)?;
            let lo = bytes.get(i + 2).copied().filter(u8::is_ascii_hexdigit)?;
            let digit = |b: u8| (b as char).to_digit(16).unwrap_or(0) as u8;
            out.push((digit(hi) << 4) | digit(lo));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Decodes `%XX` escapes; `None` for malformed escapes or non-UTF-8 results.
pub fn percent_decode(s: &str) -> Option<String> {
    String::from_utf8(percent_decode_bytes(s)?).ok()
}

/// Why a URI path could not become a local path.
enum UriPathError {
    /// A `%` not followed by two hex digits.
    Malformed,
    /// The bytes are not a valid path on this platform (non-UTF-8 off Unix).
    #[cfg_attr(unix, allow(dead_code))]
    Encoding,
}

/// Turns a decoded URI path into a local path. On Unix any bytes are a valid path (a
/// Latin-1 file name arrives as `%E9`); elsewhere the path must be UTF-8.
fn path_from_uri_path(encoded: &str) -> Result<PathBuf, UriPathError> {
    let bytes = percent_decode_bytes(encoded).ok_or(UriPathError::Malformed)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        let decoded = String::from_utf8(bytes).map_err(|_| UriPathError::Encoding)?;
        // Windows: file:///C:/dir/x.nzb -> C:/dir/x.nzb
        let b = decoded.as_bytes();
        if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
            return Ok(PathBuf::from(&decoded[1..]));
        }
        Ok(PathBuf::from(decoded))
    }
}

/// Drops a URI's `?query` and `#fragment` (a literal `?` or `#` in a file name is
/// escaped as `%3F` / `%23`).
fn strip_query_and_fragment(s: &str) -> &str {
    s.find(['?', '#']).map_or(s, |i| &s[..i])
}

/// Turns the part of a `file:` URI after the scheme into a local path.
pub(crate) fn file_uri_path(rest: &str) -> Result<PathBuf, &'static str> {
    // file:///abs, file://localhost/abs (any case), file:/abs
    let rest = strip_query_and_fragment(rest);
    let path = match rest.strip_prefix("//") {
        Some(authority_and_path) => {
            let slash = authority_and_path
                .find('/')
                .unwrap_or(authority_and_path.len());
            let host = &authority_and_path[..slash];
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return Err("not a local file URI (it names another host)");
            }
            &authority_and_path[slash..]
        }
        None => rest,
    };
    if !path.starts_with('/') {
        return Err("not a local file URI");
    }
    path_from_uri_path(path).map_err(|e| match e {
        UriPathError::Malformed => "malformed escape in URI",
        UriPathError::Encoding => "the file name is not valid text on this system",
    })
}

/// The home directory (`$HOME`, or `%USERPROFILE%` on Windows), if set.
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Expands a leading `~` or `~/` (the shell's home shorthand) in typed text; other text
/// is returned as a path unchanged (`~user` is not expanded).
pub fn expand_tilde(text: &str) -> PathBuf {
    let rest = if text == "~" {
        Some("")
    } else {
        text.strip_prefix("~/")
    };
    match (rest, home_dir()) {
        (Some(""), Some(home)) => home,
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(text),
    }
}

/// Interprets text typed or pasted into the Open NZB field: a path (with `~`
/// expanded), a `file://` URI (as a file manager copies it) or a local `nzb://` URI.
/// The error says why the text cannot be imported.
pub fn path_from_text(text: &str) -> Result<PathBuf, String> {
    // A file manager may copy several URIs, one per line; the first is the one meant.
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let first = first
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(first);
    if first.is_empty() {
        return Err("enter the path of an NZB file".into());
    }
    if looks_like_uri(first) {
        return match parse_item(first) {
            LaunchItem::Nzb(p) => Ok(p),
            LaunchItem::Unsupported { reason, .. } => Err(reason),
        };
    }
    Ok(expand_tilde(first))
}

/// Interprets a launch argument given as a string.
pub fn parse_item(arg: &str) -> LaunchItem {
    let unsupported = |reason: &str| LaunchItem::Unsupported {
        arg: arg.to_owned(),
        reason: reason.to_owned(),
    };
    let lower = arg.to_ascii_lowercase();
    if lower.starts_with("file:") {
        return match file_uri_path(&arg[5..]) {
            Ok(p) => LaunchItem::Nzb(p),
            Err(reason) => unsupported(reason),
        };
    }
    if lower.starts_with("nzb:") {
        let rest = &arg[4..];
        let inner = rest.strip_prefix("//").unwrap_or(rest);
        let inner_lower = inner.to_ascii_lowercase();
        if inner_lower.starts_with("file:") {
            return parse_item(inner);
        }
        if inner.starts_with('/') {
            // nzb:///home/me/x.nzb
            return match path_from_uri_path(strip_query_and_fragment(inner)) {
                Ok(p) => LaunchItem::Nzb(p),
                Err(UriPathError::Malformed) => unsupported("malformed escape in URI"),
                Err(UriPathError::Encoding) => {
                    unsupported("the file name is not valid text on this system")
                }
            };
        }
        return unsupported(
            "downloading NZBs from web links is not supported yet; save the NZB and open the file",
        );
    }
    if lower.starts_with("nzblnk:") {
        return unsupported("nzblnk links need an indexer search, which is not configured");
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return unsupported(
            "downloading NZBs from web links is not supported yet; save the NZB and open the file",
        );
    }
    LaunchItem::Nzb(PathBuf::from(arg))
}

/// Whether a database open error means another Newskite process holds the data
/// directory's lock (`Store::open`'s single-instance check).
pub fn is_instance_conflict(error_text: &str) -> bool {
    error_text.contains("another Newskite instance")
}

/// The text of the "already open" window: what happened, what to do, and which launch
/// arguments were not queued (they are not handed to the running window).
pub fn instance_conflict_message(data_dir: &std::path::Path, items: &[LaunchItem]) -> String {
    let mut msg = format!(
        "Another Newskite window is already open and using {}.\n\n\
         Switch to that window. To queue NZB files there, drag them onto it or use Open NZB.",
        data_dir.display()
    );
    let names: Vec<String> = items
        .iter()
        .map(|i| match i {
            LaunchItem::Nzb(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
            LaunchItem::Unsupported { arg, .. } => arg.clone(),
        })
        .collect();
    if !names.is_empty() {
        const SHOWN: usize = 5;
        let mut list = names
            .iter()
            .take(SHOWN)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        if names.len() > SHOWN {
            list.push_str(&format!(" and {} more", names.len() - SHOWN));
        }
        msg.push_str(&format!("\n\nNot queued: {list}."));
    }
    msg
}

#[cfg(test)]
mod tests {
    #[test]
    fn instance_conflict_text() {
        use super::*;
        assert!(is_instance_conflict(
            "invalid: another Newskite instance is already using /x; close it first"
        ));
        assert!(!is_instance_conflict("database error: disk I/O error"));
        let dir = std::path::Path::new("/home/u/.local/share/newskite");
        let none = instance_conflict_message(dir, &[]);
        assert!(none.contains("already open") && none.contains("/home/u/.local/share/newskite"));
        assert!(!none.contains("Not queued"));
        let items: Vec<LaunchItem> = (0..7)
            .map(|i| LaunchItem::Nzb(PathBuf::from(format!("/tmp/r{i}.nzb"))))
            .collect();
        let m = instance_conflict_message(dir, &items);
        assert!(m.contains("Not queued: r0.nzb, r1.nzb, r2.nzb, r3.nzb, r4.nzb and 2 more."));
    }

    use super::*;

    fn args(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn headless_dispatch_strips_flag() {
        match parse_args(args(&[
            "newskite",
            "--headless",
            "--nzb",
            "a.nzb",
            "--dest",
            "/tmp",
        ])) {
            Mode::Headless(a) => {
                assert_eq!(
                    a,
                    args(&["newskite --headless", "--nzb", "a.nzb", "--dest", "/tmp"])
                );
            }
            other => panic!("{other:?}"),
        }
        // Anywhere in the list.
        assert!(matches!(
            parse_args(args(&["newskite", "--nzb", "x", "--headless"])),
            Mode::Headless(_)
        ));
    }

    #[test]
    fn gui_items() {
        let m = parse_args(args(&[
            "newskite",
            "/home/me/a b.nzb",
            "file:///home/me/c%20d.nzb",
            "nzb:///home/me/e.nzb",
            "nzb://file:///home/me/f.nzb",
            "nzb://https://indexer.example/get/1",
            "nzblnk:?t=x&h=y",
            "--",
            "-dash.nzb",
        ]));
        let Mode::Gui(items) = m else { panic!("{m:?}") };
        assert_eq!(items[0], LaunchItem::Nzb(PathBuf::from("/home/me/a b.nzb")));
        assert_eq!(items[1], LaunchItem::Nzb(PathBuf::from("/home/me/c d.nzb")));
        assert_eq!(items[2], LaunchItem::Nzb(PathBuf::from("/home/me/e.nzb")));
        assert_eq!(items[3], LaunchItem::Nzb(PathBuf::from("/home/me/f.nzb")));
        assert!(matches!(items[4], LaunchItem::Unsupported { .. }));
        assert!(matches!(items[5], LaunchItem::Unsupported { .. }));
        assert_eq!(items[6], LaunchItem::Nzb(PathBuf::from("-dash.nzb")));
    }

    #[test]
    fn options() {
        assert_eq!(parse_args(args(&["newskite"])), Mode::Gui(vec![]));
        assert_eq!(parse_args(args(&["newskite", "--help"])), Mode::Help);
        assert_eq!(parse_args(args(&["newskite", "-V"])), Mode::Version);
        assert!(matches!(
            parse_args(args(&["newskite", "--bogus"])),
            Mode::Usage(_)
        ));
    }

    #[test]
    fn file_uris() {
        assert_eq!(
            parse_item("file://localhost/x/y.nzb"),
            LaunchItem::Nzb(PathBuf::from("/x/y.nzb"))
        );
        assert_eq!(
            parse_item("FILE:/x/y.nzb"),
            LaunchItem::Nzb(PathBuf::from("/x/y.nzb"))
        );
        assert!(matches!(
            parse_item("file://otherhost/x.nzb"),
            LaunchItem::Unsupported { .. }
        ));
        assert!(matches!(
            parse_item("file:///bad%zz.nzb"),
            LaunchItem::Unsupported { .. }
        ));
        assert!(matches!(
            parse_item("file:///bad%f"),
            LaunchItem::Unsupported { .. }
        ));
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b%2Fc").as_deref(), Some("a b/c"));
        assert_eq!(percent_decode("%C3%A9").as_deref(), Some("é"));
        assert_eq!(percent_decode("%FF"), None);
        assert_eq!(percent_decode("%2"), None);
        assert_eq!(percent_decode("plain").as_deref(), Some("plain"));
        // A sign is not a hex digit.
        assert_eq!(percent_decode_bytes("%+F"), None);
        assert_eq!(percent_decode_bytes("%-1"), None);
        assert_eq!(percent_decode_bytes("%E9"), Some(vec![0xe9]));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_file_uri_is_a_local_path() {
        use std::os::unix::ffi::OsStrExt;
        match parse_item("file:///home/me/caf%E9.nzb") {
            LaunchItem::Nzb(p) => assert_eq!(p.as_os_str().as_bytes(), b"/home/me/caf\xe9.nzb"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_item("nzb:///home/me/caf%E9.nzb"),
            LaunchItem::Nzb(_)
        ));
    }

    #[test]
    fn uri_query_fragment_and_host_case() {
        assert_eq!(
            parse_item("file:///x/y.nzb?from=browser#top"),
            LaunchItem::Nzb(PathBuf::from("/x/y.nzb"))
        );
        assert_eq!(
            parse_item("file:///x/a%3Fb%23c.nzb"),
            LaunchItem::Nzb(PathBuf::from("/x/a?b#c.nzb"))
        );
        assert_eq!(
            parse_item("file://LOCALHOST/x/y.nzb"),
            LaunchItem::Nzb(PathBuf::from("/x/y.nzb"))
        );
        match parse_item("file:///bad%zz.nzb") {
            LaunchItem::Unsupported { reason, .. } => assert!(reason.contains("malformed")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn typed_and_pasted_text() {
        assert_eq!(
            path_from_text("file:///home/me/x%20y.nzb\n"),
            Ok(PathBuf::from("/home/me/x y.nzb"))
        );
        assert_eq!(
            path_from_text("  /tmp/a.nzb "),
            Ok(PathBuf::from("/tmp/a.nzb"))
        );
        assert_eq!(
            path_from_text("\"/tmp/q.nzb\""),
            Ok(PathBuf::from("/tmp/q.nzb"))
        );
        assert!(path_from_text("https://indexer.example/get/1").is_err());
        assert!(path_from_text("   ").is_err());
        if let Some(home) = home_dir() {
            assert_eq!(path_from_text("~/x.nzb"), Ok(home.join("x.nzb")));
            assert_eq!(expand_tilde("~"), home);
        }
        assert_eq!(expand_tilde("~other/x"), PathBuf::from("~other/x"));
        assert_eq!(expand_tilde("rel/x"), PathBuf::from("rel/x"));
    }
}
