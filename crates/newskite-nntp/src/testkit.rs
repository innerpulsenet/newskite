//! In-process mock NNTP server for tests (feature `testkit`).
//!
//! Listens on `127.0.0.1:0`, greets with `200`, and implements just enough NNTP for the
//! client and the download engine: `AUTHINFO USER`/`PASS`, `BODY`, `STAT`, `GROUP`,
//! `DATE`, `MODE READER` and `QUIT`; anything else gets `500`. When credentials are
//! configured, `BODY`/`STAT`/`GROUP` before authentication get `480`.
//!
//! Article bodies are stored raw and dot-stuffed on the way out, each line sent with
//! `\r\n`. Note the server is for tests only: credentials are compared naively.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

/// Behavior of a [`MockServer`].
#[derive(Clone, Debug, Default)]
pub struct MockConfig {
    /// Required username; `None` = no authentication required (AUTHINFO is still accepted).
    pub username: Option<String>,
    /// Required password (checked when `username` is set).
    pub password: Option<String>,
    /// Articles by message id **without** angle brackets → raw (not dot-stuffed) body.
    pub articles: HashMap<String, Vec<u8>>,
    /// The first N accepted connections are closed immediately (simulates flaky servers).
    pub fail_first_n_connections: usize,
    /// Delay before answering each `BODY`, in milliseconds.
    pub delay_ms: u64,
}

#[derive(Default)]
struct Counters {
    connections: AtomicUsize,
    body_requests: AtomicUsize,
}

/// A running mock server. Dropping it (or calling [`MockServer::shutdown`]) stops the
/// accept loop and closes every open session.
pub struct MockServer {
    /// The bound address (`127.0.0.1:<ephemeral port>`).
    pub addr: SocketAddr,
    counters: Arc<Counters>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for MockServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockServer").field("addr", &self.addr).finish_non_exhaustive()
    }
}

impl MockServer {
    /// Binds `127.0.0.1:0` and starts serving `cfg` on the current tokio runtime.
    ///
    /// # Panics
    /// If the loopback listener cannot be bound (test-environment failure).
    pub async fn start(cfg: MockConfig) -> MockServer {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("mock NNTP server: bind 127.0.0.1:0");
        let addr = listener.local_addr().expect("mock NNTP server: local_addr");
        let counters = Arc::new(Counters::default());
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(accept_loop(listener, Arc::new(cfg), counters.clone(), stop_rx));
        MockServer { addr, counters, stop, task }
    }

    /// Connections accepted so far, including ones dropped by `fail_first_n_connections`.
    pub fn connections_accepted(&self) -> usize {
        self.counters.connections.load(Ordering::SeqCst)
    }

    /// `BODY` commands received so far (counted before authentication checks).
    pub fn body_requests(&self) -> usize {
        self.counters.body_requests.load(Ordering::SeqCst)
    }

    /// Stops accepting and closes all sessions.
    pub fn shutdown(self) {
        // Drop does the work.
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        self.task.abort();
    }
}

async fn accept_loop(
    listener: TcpListener,
    cfg: Arc<MockConfig>,
    counters: Arc<Counters>,
    mut stop: watch::Receiver<bool>,
) {
    // Sessions live in the set, so they are aborted when this task ends or is aborted.
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            accepted = listener.accept() => {
                let Ok((socket, _peer)) = accepted else { continue };
                let index = counters.connections.fetch_add(1, Ordering::SeqCst);
                if index < cfg.fail_first_n_connections {
                    drop(socket);
                    continue;
                }
                sessions.spawn(session(socket, cfg.clone(), counters.clone()));
            }
            Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
        }
    }
}

async fn session(socket: TcpStream, cfg: Arc<MockConfig>, counters: Arc<Counters>) {
    let _ = serve(socket, &cfg, &counters).await;
}

async fn serve(socket: TcpStream, cfg: &MockConfig, counters: &Counters) -> std::io::Result<()> {
    let (read_half, mut writer) = socket.into_split();
    let mut reader = BufReader::new(read_half);
    let needs_auth = cfg.username.is_some();
    let mut authenticated = !needs_auth;
    let mut pending_user: Option<String> = None;

    writer.write_all(b"200 newskite mock server ready\r\n").await?;
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).await? == 0 {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        let mut parts = text.splitn(2, ' ');
        let verb = parts.next().unwrap_or_default().to_ascii_uppercase();
        let arg = parts.next().unwrap_or_default().trim();

        let reply: Vec<u8> = match verb.as_str() {
            "AUTHINFO" => {
                let mut sub = arg.splitn(2, ' ');
                let kind = sub.next().unwrap_or_default().to_ascii_uppercase();
                let value = sub.next().unwrap_or_default().to_owned();
                match kind.as_str() {
                    "USER" if !needs_auth => {
                        authenticated = true;
                        b"281 authentication accepted\r\n".to_vec()
                    }
                    "USER" => {
                        pending_user = Some(value);
                        b"381 password required\r\n".to_vec()
                    }
                    "PASS" => match pending_user.take() {
                        None => b"482 authentication commands issued out of sequence\r\n".to_vec(),
                        Some(user) => {
                            let ok = cfg.username.as_deref() == Some(user.as_str())
                                && cfg.password.as_deref().unwrap_or_default() == value;
                            if ok {
                                authenticated = true;
                                b"281 authentication accepted\r\n".to_vec()
                            } else {
                                b"481 authentication failed\r\n".to_vec()
                            }
                        }
                    },
                    _ => b"501 unknown AUTHINFO subcommand\r\n".to_vec(),
                }
            }
            "BODY" => {
                counters.body_requests.fetch_add(1, Ordering::SeqCst);
                if !authenticated {
                    b"480 authentication required\r\n".to_vec()
                } else {
                    if cfg.delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(cfg.delay_ms)).await;
                    }
                    match cfg.articles.get(strip_brackets(arg)) {
                        Some(body) => {
                            let mut out = format!("222 0 {arg}\r\n").into_bytes();
                            out.extend_from_slice(&dot_stuff(body));
                            out
                        }
                        None => b"430 no such article\r\n".to_vec(),
                    }
                }
            }
            "STAT" => {
                if !authenticated {
                    b"480 authentication required\r\n".to_vec()
                } else if cfg.articles.contains_key(strip_brackets(arg)) {
                    format!("223 0 {arg}\r\n").into_bytes()
                } else {
                    b"430 no such article\r\n".to_vec()
                }
            }
            "GROUP" => {
                if !authenticated {
                    b"480 authentication required\r\n".to_vec()
                } else if arg.is_empty() {
                    b"501 syntax error\r\n".to_vec()
                } else {
                    format!("211 0 0 0 {arg}\r\n").into_bytes()
                }
            }
            "DATE" => format!("111 {}\r\n", utc_timestamp(SystemTime::now())).into_bytes(),
            "MODE" if arg.eq_ignore_ascii_case("READER") => b"200 reader mode\r\n".to_vec(),
            "QUIT" => {
                writer.write_all(b"205 closing connection\r\n").await?;
                writer.flush().await?;
                return Ok(());
            }
            _ => b"500 unknown command\r\n".to_vec(),
        };
        writer.write_all(&reply).await?;
        writer.flush().await?;
    }
}

fn strip_brackets(id: &str) -> &str {
    let id = id.trim();
    id.strip_prefix('<').and_then(|s| s.strip_suffix('>')).unwrap_or(id)
}

/// Encodes a raw body as an NNTP multi-line block: lines split on `\n` (a preceding `\r`
/// is dropped), leading dots doubled, every line sent with `\r\n`, then `.\r\n`.
pub fn dot_stuff(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + body.len() / 64 + 8);
    if !body.is_empty() {
        let content = body.strip_suffix(b"\n").unwrap_or(body);
        for line in content.split(|b| *b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.first() == Some(&b'.') {
                out.push(b'.');
            }
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
        }
    }
    out.extend_from_slice(b".\r\n");
    out
}

/// `yyyymmddhhmmss` in UTC.
fn utc_timestamp(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}{:02}{:02}{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian (H. Hinnant).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stuffing() {
        assert_eq!(dot_stuff(b""), b".\r\n");
        assert_eq!(dot_stuff(b"a\r\n.b\n..c\n.\n"), b"a\r\n..b\r\n...c\r\n..\r\n.\r\n");
        assert_eq!(dot_stuff(b"no newline"), b"no newline\r\n.\r\n");
    }

    #[test]
    fn timestamps() {
        assert_eq!(utc_timestamp(UNIX_EPOCH), "19700101000000");
        // 2026-10-31T12:00:00Z
        assert_eq!(utc_timestamp(UNIX_EPOCH + Duration::from_secs(1_793_448_000)), "20261031120000");
        // Leap day 2024-02-29T23:59:59Z
        assert_eq!(utc_timestamp(UNIX_EPOCH + Duration::from_secs(1_709_251_199)), "20240229235959");
    }
}
