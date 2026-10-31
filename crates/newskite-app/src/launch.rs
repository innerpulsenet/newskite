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

/// Decodes `%XX` escapes; `None` for malformed escapes or non-UTF-8 results.
pub fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Turns the path part of a `file:` URI into a local path.
fn file_uri_path(rest: &str) -> Option<PathBuf> {
    // file:///abs, file://localhost/abs, file:/abs
    let rest = rest
        .strip_prefix("//")
        .map(|r| r.strip_prefix("localhost").unwrap_or(r))
        .unwrap_or(rest);
    if !rest.starts_with('/') {
        return None; // remote host
    }
    let decoded = percent_decode(rest)?;
    // Windows: file:///C:/dir/x.nzb -> C:/dir/x.nzb
    let b = decoded.as_bytes();
    if cfg!(windows) && b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        return Some(PathBuf::from(&decoded[1..]));
    }
    Some(PathBuf::from(decoded))
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
            Some(p) => LaunchItem::Nzb(p),
            None => unsupported("not a local file URI"),
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
            return match percent_decode(inner) {
                Some(p) => LaunchItem::Nzb(PathBuf::from(p)),
                None => unsupported("malformed escape in URI"),
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

#[cfg(test)]
mod tests {
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
    }
}
