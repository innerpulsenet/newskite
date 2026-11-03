//! Single-instance hand-off. The window that holds the data directory's lock listens on
//! [`SOCKET_FILE_NAME`] in that directory (owner-only); a later `newskite` launch (a
//! double-clicked `.nzb`, an `nzb://` link, the menu entry) that finds the lock taken
//! passes its launch items there and exits, and the window queues them and comes forward.
//!
//! The request is one line of JSON, `{"open": [...]}`: local NZBs as `file://` URIs
//! (absolute, every byte escaped, so any file name survives), anything else as the
//! original argument, which the window parses again. An empty list only raises the
//! window. The reply is one line, `ok` or `error <why>`.
//!
//! Off Unix there are no socket files: the window listens on a loopback TCP port and
//! writes the port and a random token to [`PORT_FILE_NAME`] in the data directory (which
//! only the user can read). A request starts with the token on a line of its own, and local
//! NZBs travel as absolute paths, not URIs.

use std::io;
use std::path::{Path, PathBuf};

use crate::launch::{self, LaunchItem};

/// The hand-off socket, next to the lock file in the data directory.
pub const SOCKET_FILE_NAME: &str = "newskite.sock";

/// Longest request accepted (bytes); a launch is a handful of paths.
const MAX_REQUEST: u64 = 1024 * 1024;

/// Off Unix: the file holding the listening window's port and token.
#[cfg(not(unix))]
pub const PORT_FILE_NAME: &str = "newskite.handoff";

/// The socket's path for `data_dir`.
pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SOCKET_FILE_NAME)
}

/// The request line for `items` (with its newline). Relative paths are made absolute
/// here, against the launching process's working directory.
pub fn encode(items: &[LaunchItem]) -> String {
    let args: Vec<String> = items
        .iter()
        .map(|item| match item {
            LaunchItem::Nzb(p) => nzb_arg(&std::path::absolute(p).unwrap_or_else(|_| p.clone())),
            LaunchItem::Unsupported { arg, .. } => arg.clone(),
        })
        .collect();
    let mut line = serde_json::json!({ "open": args }).to_string();
    line.push('\n');
    line
}

#[cfg(unix)]
fn nzb_arg(path: &Path) -> String {
    crate::desktop::file_uri(path)
}

/// A Windows path is UTF-16, so it is text already (barring unpaired surrogates); sent as
/// is, it is parsed back as a plain path, drive letters and UNC shares included.
#[cfg(not(unix))]
fn nzb_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The launch items of a request line; `None` when it is not one.
pub fn decode(line: &str) -> Option<Vec<LaunchItem>> {
    let value: serde_json::Value = serde_json::from_str(line.trim_end()).ok()?;
    let args = value.get("open")?.as_array()?;
    args.iter().map(|a| a.as_str().map(launch::parse_item)).collect()
}

/// Hands `items` to the window listening for `data_dir`. `Ok` once the window has
/// accepted them; an error when nothing listens (a headless run holds the lock, or the
/// window predates the hand-off) or it refused them.
#[cfg(unix)]
pub fn send(data_dir: &Path, items: &[LaunchItem]) -> io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let mut stream = UnixStream::connect(socket_path(data_dir))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(encode(items).as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream.take(4096)).read_line(&mut reply)?;
    match reply.trim_end() {
        "ok" => Ok(()),
        "" => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the window closed the connection")),
        other => Err(io::Error::other(other.strip_prefix("error ").unwrap_or(other).to_owned())),
    }
}

/// The bound hand-off socket of the window that holds the lock.
#[cfg(unix)]
pub struct Listener {
    listener: std::os::unix::net::UnixListener,
    path: PathBuf,
}

#[cfg(unix)]
impl Listener {
    /// Binds the socket for `data_dir`. Call only while holding the data directory's
    /// lock: a socket file already there is then a leftover of an earlier run and is
    /// replaced.
    pub fn bind(data_dir: &Path) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let path = socket_path(data_dir);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self { listener, path })
    }

    /// The socket file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Answers launches on a thread of its own for the rest of the process, passing each
    /// request's items to `deliver`.
    pub fn serve(self, deliver: impl Fn(Vec<LaunchItem>) + Send + 'static) -> io::Result<()> {
        use std::time::Duration;

        std::thread::Builder::new().name("newskite-handoff".into()).spawn(move || {
            for stream in self.listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let result = stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(2))))
                            .and_then(|()| answer(stream, None, &deliver));
                        if let Err(e) = result {
                            tracing::warn!("a later launch could not hand over its NZBs: {e}");
                        }
                    }
                    Err(e) => tracing::warn!("hand-off socket: {e}"),
                }
            }
        })?;
        Ok(())
    }
}

/// Reads one request (after the token line, when there is a `token`), delivers it and
/// replies.
fn answer(
    mut stream: impl io::Read + io::Write,
    token: Option<&str>,
    deliver: &impl Fn(Vec<LaunchItem>),
) -> io::Result<()> {
    use std::io::{BufRead, BufReader, Read};

    let mut reader = BufReader::new((&mut stream).take(MAX_REQUEST));
    let mut line = String::new();
    if let Some(token) = token {
        reader.read_line(&mut line)?;
        if line.trim_end() != token {
            drop(reader);
            return stream.write_all(b"error not a Newskite launch request\n");
        }
        line.clear();
    }
    reader.read_line(&mut line)?;
    drop(reader);
    let reply = match decode(&line) {
        Some(items) => {
            deliver(items);
            "ok\n".to_owned()
        }
        None => "error not a Newskite launch request\n".to_owned(),
    };
    stream.write_all(reply.as_bytes())
}

/// Off Unix: reads the listening window's port and token from [`PORT_FILE_NAME`].
#[cfg(not(unix))]
fn read_port_file(data_dir: &Path) -> io::Result<(u16, String)> {
    let text = std::fs::read_to_string(data_dir.join(PORT_FILE_NAME))?;
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "the hand-off file is damaged");
    let (port, token) = text.trim().split_once(' ').ok_or_else(bad)?;
    Ok((port.parse().map_err(|_| bad())?, token.to_owned()))
}

#[cfg(not(unix))]
pub fn send(data_dir: &Path, items: &[LaunchItem]) -> io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    use std::time::Duration;

    let (port, token) = read_port_file(data_dir)?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    // This launch came from the user, so it may bring a window forward; pass that on to
    // the open window, which Windows would otherwise keep from raising itself.
    #[cfg(windows)]
    crate::win::allow_any_foreground();
    stream.write_all(format!("{token}\n{}", encode(items)).as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream.take(4096)).read_line(&mut reply)?;
    match reply.trim_end() {
        "ok" => Ok(()),
        "" => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the window closed the connection")),
        other => Err(io::Error::other(other.strip_prefix("error ").unwrap_or(other).to_owned())),
    }
}

/// The listening hand-off port of the window that holds the lock.
#[cfg(not(unix))]
pub struct Listener {
    listener: std::net::TcpListener,
    token: String,
    path: PathBuf,
}

#[cfg(not(unix))]
impl Listener {
    /// Listens on a loopback port and records it (with a fresh token) for `data_dir`.
    /// Call only while holding the data directory's lock: a port file already there is
    /// then a leftover of an earlier run and is replaced.
    pub fn bind(data_dir: &Path) -> io::Result<Self> {
        use std::net::{Ipv4Addr, TcpListener};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let token = random_token();
        let path = data_dir.join(PORT_FILE_NAME);
        // Written whole and then renamed into place, so a launch never reads half a file.
        let tmp = data_dir.join(format!("{PORT_FILE_NAME}.tmp"));
        std::fs::write(&tmp, format!("{port} {token}\n"))?;
        std::fs::rename(&tmp, &path)?;
        Ok(Self { listener, token, path })
    }

    /// The port file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Answers launches on a thread of its own for the rest of the process, passing each
    /// request's items to `deliver`.
    pub fn serve(self, deliver: impl Fn(Vec<LaunchItem>) + Send + 'static) -> io::Result<()> {
        use std::time::Duration;

        std::thread::Builder::new().name("newskite-handoff".into()).spawn(move || {
            for stream in self.listener.incoming() {
                let result = stream.and_then(|stream| {
                    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                    answer(stream, Some(&self.token), &deliver)
                });
                if let Err(e) = result {
                    tracing::warn!("a later launch could not hand over its NZBs: {e}");
                }
            }
        })?;
        Ok(())
    }
}

/// 128 unguessable bits as hex. Each `RandomState` is keyed from the operating system's
/// random source, so its hashes cannot be predicted by another process.
#[cfg(not(unix))]
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    (0..2u8)
        .map(|i| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u8(i);
            h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
            format!("{:016x}", h.finish())
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn requests_round_trip_any_file_name() {
        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/caf\xe9 \"x\"\n.nzb"));
        let items = vec![
            LaunchItem::Nzb(odd.clone()),
            LaunchItem::Unsupported { arg: "nzblnk:?t=a&h=b".into(), reason: "x".into() },
        ];
        let line = encode(&items);
        assert_eq!(line.matches('\n').count(), 1, "one line: {line:?}");
        let back = decode(&line).expect("decodes");
        assert_eq!(back.len(), 2);
        assert!(matches!(&back[0], LaunchItem::Nzb(p) if *p == odd));
        assert!(matches!(&back[1], LaunchItem::Unsupported { arg, .. } if arg == "nzblnk:?t=a&h=b"));
    }

    #[test]
    fn relative_paths_are_sent_absolute() {
        let line = encode(&[LaunchItem::Nzb(PathBuf::from("some.nzb"))]);
        let back = decode(&line).expect("decodes");
        let LaunchItem::Nzb(p) = &back[0] else { panic!("{back:?}") };
        assert!(p.is_absolute() && p.ends_with("some.nzb"), "{p:?}");
    }

    #[test]
    fn other_lines_are_not_requests() {
        assert!(decode("hello\n").is_none());
        assert!(decode("{\"open\": [1]}\n").is_none());
        assert!(decode("{}\n").is_none());
        assert_eq!(decode("{\"open\": []}\n").map(|v| v.len()), Some(0));
    }

    #[test]
    fn a_later_launch_reaches_the_listener() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A leftover socket file from an earlier run is replaced.
        std::fs::write(socket_path(dir.path()), b"stale").expect("stale file");
        let listener = Listener::bind(dir.path()).expect("bind");
        let mode = std::fs::metadata(listener.path()).expect("meta").permissions();
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777, 0o600);
        let (tx, rx) = mpsc::channel();
        listener.serve(move |items| tx.send(items).expect("send")).expect("serve");

        let nzb = dir.path().join("a.nzb");
        send(dir.path(), &[LaunchItem::Nzb(nzb.clone())]).expect("handed over");
        let got = rx.recv_timeout(Duration::from_secs(5)).expect("delivered");
        assert!(matches!(&got[..], [LaunchItem::Nzb(p)] if *p == nzb));
        // Raising only: no items.
        send(dir.path(), &[]).expect("raise");
        assert!(rx.recv_timeout(Duration::from_secs(5)).expect("delivered").is_empty());
    }

    #[test]
    fn nothing_listening_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(send(dir.path(), &[]).is_err());
    }
}

#[cfg(all(test, not(unix)))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn requests_round_trip_windows_paths() {
        let (local, unc) = (r"C:\Users\me\Downloads\café #1 (x).nzb", r"\\server\share\b.nzb");
        let items = vec![LaunchItem::Nzb(PathBuf::from(local)), LaunchItem::Nzb(PathBuf::from(unc))];
        let back = decode(&encode(&items)).expect("decodes");
        assert!(matches!(&back[0], LaunchItem::Nzb(p) if p == Path::new(local)));
        assert!(matches!(&back[1], LaunchItem::Nzb(p) if p == Path::new(unc)));
    }

    #[test]
    fn a_later_launch_reaches_the_listener() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A leftover port file from an earlier run is replaced.
        std::fs::write(dir.path().join(PORT_FILE_NAME), b"1 stale").expect("stale file");
        let listener = Listener::bind(dir.path()).expect("bind");
        let (tx, rx) = mpsc::channel();
        listener.serve(move |items| tx.send(items).expect("send")).expect("serve");

        let nzb = dir.path().join("a.nzb");
        send(dir.path(), &[LaunchItem::Nzb(nzb.clone())]).expect("handed over");
        let got = rx.recv_timeout(Duration::from_secs(5)).expect("delivered");
        assert!(matches!(&got[..], [LaunchItem::Nzb(p)] if *p == nzb));
        send(dir.path(), &[]).expect("raise");
        assert!(rx.recv_timeout(Duration::from_secs(5)).expect("delivered").is_empty());
    }

    #[test]
    fn a_wrong_token_is_refused() {
        use std::io::{BufRead, BufReader, Write};

        let dir = tempfile::tempdir().expect("tempdir");
        let listener = Listener::bind(dir.path()).expect("bind");
        let (tx, rx) = mpsc::channel();
        listener.serve(move |items| tx.send(items).expect("send")).expect("serve");

        let (port, _) = read_port_file(dir.path()).expect("port file");
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.write_all(b"guess\n{\"open\": []}\n").expect("write");
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).expect("reply");
        assert!(reply.starts_with("error"), "{reply:?}");
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }
}
