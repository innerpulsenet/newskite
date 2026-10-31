//! The NNTP connection: handshake, authentication, commands and multi-line bodies.

use std::fmt;
use std::time::Duration;

use newskite_types::ServerConfig;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::msgid::{bare, normalize_message_id};
use crate::stream::Stream;
use crate::NntpError;

/// Largest article body accepted from a server. Usenet binary segments are typically
/// well under 1 MiB; anything past this is treated as a hostile or broken server.
pub const MAX_ARTICLE_BYTES: usize = 64 * 1024 * 1024;

/// Default per-read inactivity timeout once a connection is established.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Longest single-line (status) response we accept.
const MAX_STATUS_LINE: usize = 16 * 1024;
/// Read buffer size for the socket.
const READ_BUFFER: usize = 64 * 1024;
/// The reusable line buffer is shrunk back to this after an unusually long line.
const LINE_BUF_KEEP: usize = 64 * 1024;
/// Upper bound for the polite `QUIT` exchange.
const QUIT_TIMEOUT: Duration = Duration::from_secs(2);
/// Mask shown in place of credentials in logs and transcripts.
const MASK: &str = "***";

/// A single-line NNTP response: a three-digit code and the rest of the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// The three-digit status code.
    pub code: u16,
    /// Everything after the code, without leading whitespace or the line ending.
    pub text: String,
}

impl Response {
    /// Parses a raw status line (with or without its line ending).
    fn parse(line: &[u8]) -> Result<Self, NntpError> {
        let line = strip_eol(line);
        let text = String::from_utf8_lossy(line);
        let bytes = text.as_bytes();
        let well_formed = bytes.len() >= 3
            && bytes[..3].iter().all(u8::is_ascii_digit)
            && (bytes.len() == 3 || bytes[3] == b' ' || bytes[3] == b'\t');
        if !well_formed {
            return Err(NntpError::Protocol(format!(
                "malformed response line: {:?}",
                truncate(&text, 120)
            )));
        }
        let code = text[..3]
            .parse::<u16>()
            .map_err(|e| NntpError::Protocol(format!("bad response code: {e}")))?;
        Ok(Response { code, text: text[3..].trim().to_owned() })
    }
}

impl fmt::Display for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.text.is_empty() {
            write!(f, "{}", self.code)
        } else {
            write!(f, "{} {}", self.code, self.text)
        }
    }
}

/// Credentials kept for (re-)authentication. Never printed.
struct Credentials {
    username: String,
    password: String,
}

/// An established NNTP session over plain TCP or TLS.
///
/// Created with [`NntpConnection::connect`], which reads the greeting and authenticates
/// when the server config has a username. All reads are bounded by a per-read inactivity
/// timeout ([`DEFAULT_READ_TIMEOUT`], see [`NntpConnection::set_read_timeout`]).
///
/// After an I/O error, timeout or malformed reply the session state is unknown; the
/// connection marks itself broken ([`NntpConnection::is_broken`]) and refuses further
/// commands, so callers should drop it and open a new one.
pub struct NntpConnection {
    reader: BufReader<Stream>,
    greeting: String,
    address: String,
    tls: bool,
    credentials: Option<Credentials>,
    read_timeout: Duration,
    line_buf: Vec<u8>,
    broken: bool,
    /// When `Some`, every command (masked) and reply is appended (used by [`test_server`]).
    transcript: Option<Vec<String>>,
}

impl fmt::Debug for NntpConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NntpConnection")
            .field("address", &self.address)
            .field("tls", &self.tls)
            .field("greeting", &self.greeting)
            .field("authenticated_with_credentials", &self.credentials.is_some())
            .field("broken", &self.broken)
            .finish_non_exhaustive()
    }
}

impl NntpConnection {
    /// Connects to `server`, reads the greeting (200/201) and, when a username is
    /// configured, authenticates with `AUTHINFO USER`/`PASS`.
    ///
    /// `timeout` bounds the whole sequence: TCP connect, TLS handshake, greeting and
    /// authentication. Exceeding it yields [`NntpError::Timeout`].
    pub async fn connect(server: &ServerConfig, timeout: Duration) -> Result<Self, NntpError> {
        Self::connect_inner(server, timeout, false).await
    }

    async fn connect_inner(server: &ServerConfig, timeout: Duration, record: bool) -> Result<Self, NntpError> {
        let host = server.host.trim();
        if host.is_empty() {
            return Err(NntpError::Connect("no server host configured".into()));
        }
        let fut = async {
            let address = format!("{host}:{}", server.port);
            let tcp = TcpStream::connect((host, server.port))
                .await
                .map_err(|e| NntpError::Connect(format!("{address}: {e}")))?;
            if let Err(e) = tcp.set_nodelay(true) {
                tracing::debug!(error = %e, "could not set TCP_NODELAY");
            }
            let stream = if server.tls { Stream::tls(tcp, host).await? } else { Stream::Plain(tcp) };
            let credentials = server.has_credentials().then(|| Credentials {
                username: server.username.clone(),
                password: server.password.clone(),
            });
            let mut conn = NntpConnection {
                reader: BufReader::with_capacity(READ_BUFFER, stream),
                greeting: String::new(),
                address,
                tls: server.tls,
                credentials,
                read_timeout: DEFAULT_READ_TIMEOUT,
                line_buf: Vec::new(),
                broken: false,
                transcript: record.then(Vec::new),
            };
            let greeting = conn.read_response().await?;
            match greeting.code {
                200 | 201 => conn.greeting = greeting.to_string(),
                400 | 502 => {
                    return Err(NntpError::Connect(format!("server refused the session: {greeting}")));
                }
                _ => return Err(NntpError::Protocol(format!("unexpected greeting: {greeting}"))),
            }
            tracing::debug!(address = %conn.address, tls = conn.tls, greeting = %conn.greeting, "NNTP connected");
            if conn.credentials.is_some() {
                conn.authenticate().await?;
            }
            Ok(conn)
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(result) => result,
            Err(_) => Err(NntpError::Timeout),
        }
    }

    /// The server's greeting line, e.g. `"200 news.example.com ready"`.
    pub fn greeting(&self) -> &str {
        &self.greeting
    }

    /// `host:port` this connection was opened to.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Whether the transport is TLS.
    pub fn is_tls(&self) -> bool {
        self.tls
    }

    /// True once an I/O error, timeout or protocol violation left the session in an
    /// unknown state; every further command fails and the connection should be dropped.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// The current per-read inactivity timeout.
    pub fn read_timeout(&self) -> Duration {
        self.read_timeout
    }

    /// Changes the per-read inactivity timeout (applies to every subsequent read/write).
    pub fn set_read_timeout(&mut self, timeout: Duration) {
        self.read_timeout = timeout;
    }

    /// Sends one command line and reads its single-line response.
    ///
    /// Lines containing CR, LF or NUL are refused ([`NntpError::Protocol`]) so callers
    /// cannot smuggle extra commands. The response is returned whatever its code; use the
    /// typed helpers ([`body`](Self::body), [`stat`](Self::stat), ...) for code handling.
    pub async fn command(&mut self, line: &str) -> Result<Response, NntpError> {
        if line.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
            return Err(NntpError::Protocol("refusing to send a command containing CR, LF or NUL".into()));
        }
        if self.broken {
            return Err(NntpError::Protocol("connection is unusable after an earlier error".into()));
        }
        self.send_line(line).await?;
        self.read_response().await
    }

    /// Fetches an article body (`BODY <id>`); `message_id` may omit the angle brackets.
    ///
    /// Returns the dot-unstuffed payload without the terminating `.` line; every line is
    /// terminated by `\r\n` (bare `\n` is normalized). 430/423 → [`NntpError::ArticleMissing`]
    /// (with the id without brackets); 480 → re-authenticates once if credentials are
    /// configured, otherwise [`NntpError::AuthRequired`].
    pub async fn body(&mut self, message_id: &str) -> Result<Vec<u8>, NntpError> {
        let id = normalize_message_id(message_id)?;
        let r = self.command_with_reauth(&format!("BODY {id}")).await?;
        match r.code {
            222 => self.read_multiline().await,
            430 | 423 => Err(NntpError::ArticleMissing(bare(&id).to_owned())),
            480 => Err(NntpError::AuthRequired),
            _ => Err(self.unexpected("BODY", &r)),
        }
    }

    /// Checks whether an article exists (`STAT <id>`): 223 → `true`, 430/423 → `false`.
    pub async fn stat(&mut self, message_id: &str) -> Result<bool, NntpError> {
        let id = normalize_message_id(message_id)?;
        let r = self.command_with_reauth(&format!("STAT {id}")).await?;
        match r.code {
            223 => Ok(true),
            430 | 423 => Ok(false),
            480 => Err(NntpError::AuthRequired),
            _ => Err(self.unexpected("STAT", &r)),
        }
    }

    /// Selects a newsgroup (`GROUP name`). The reply is returned as-is (211 on success,
    /// 411 for an unknown group); 480 → re-auth once or [`NntpError::AuthRequired`].
    pub async fn group(&mut self, name: &str) -> Result<Response, NntpError> {
        let name = name.trim();
        if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(NntpError::Protocol(format!("invalid newsgroup name {name:?}")));
        }
        let r = self.command_with_reauth(&format!("GROUP {name}")).await?;
        match r.code {
            480 => Err(NntpError::AuthRequired),
            _ => Ok(r),
        }
    }

    /// Asks for the server time (`DATE`); expects `111 yyyymmddhhmmss`.
    pub async fn date(&mut self) -> Result<Response, NntpError> {
        let r = self.command_with_reauth("DATE").await?;
        match r.code {
            111 => Ok(r),
            480 => Err(NntpError::AuthRequired),
            _ => Err(NntpError::Protocol(format!("unexpected reply to DATE: {r}"))),
        }
    }

    /// Politely ends the session (`QUIT`, bounded to a couple of seconds) and closes the
    /// socket. Errors are ignored: the connection is going away either way.
    pub async fn quit(mut self) {
        if !self.broken {
            self.read_timeout = self.read_timeout.min(QUIT_TIMEOUT);
            if let Err(e) = self.command("QUIT").await {
                tracing::debug!(error = %e, "QUIT failed");
            }
        }
        let _ = tokio::time::timeout(QUIT_TIMEOUT, self.reader.get_mut().shutdown()).await;
    }

    // ---- internals -------------------------------------------------------------------

    /// Sends a command; on 480 with configured credentials, authenticates once and resends.
    async fn command_with_reauth(&mut self, line: &str) -> Result<Response, NntpError> {
        let r = self.command(line).await?;
        if r.code == 480 && self.credentials.is_some() {
            tracing::debug!(address = %self.address, "server asked for authentication; re-authenticating");
            self.authenticate().await?;
            return self.command(line).await;
        }
        Ok(r)
    }

    /// `AUTHINFO USER` → 381 → `AUTHINFO PASS` → 281 (or 281 right after USER).
    async fn authenticate(&mut self) -> Result<(), NntpError> {
        let (user_line, pass_line) = match &self.credentials {
            Some(c) => {
                let bad = |s: &str| s.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0));
                if bad(&c.username) || bad(&c.password) {
                    return Err(NntpError::AuthFailed("credentials contain line breaks or NUL".into()));
                }
                (format!("AUTHINFO USER {}", c.username), format!("AUTHINFO PASS {}", c.password))
            }
            None => return Err(NntpError::AuthRequired),
        };
        let r = self.command(&user_line).await?;
        match r.code {
            281 => return Ok(()),
            381 => {}
            _ => return Err(auth_error(&r)),
        }
        let r = self.command(&pass_line).await?;
        drop(pass_line);
        match r.code {
            281 => Ok(()),
            _ => Err(auth_error(&r)),
        }
    }

    /// Builds the error for an unexpected reply code. A 1xx/2xx reply we did not ask for
    /// may be followed by a multi-line block, so the session is considered broken.
    fn unexpected(&mut self, what: &str, r: &Response) -> NntpError {
        if r.code < 400 {
            self.broken = true;
        }
        NntpError::Protocol(format!("unexpected reply to {what}: {r}"))
    }

    async fn send_line(&mut self, line: &str) -> Result<(), NntpError> {
        let masked = mask_command(line);
        tracing::trace!(address = %self.address, command = %masked, "->");
        if let Some(t) = self.transcript.as_mut() {
            t.push(format!("-> {masked}"));
        }
        let mut buf = Vec::with_capacity(line.len() + 2);
        buf.extend_from_slice(line.as_bytes());
        buf.extend_from_slice(b"\r\n");
        let stream = self.reader.get_mut();
        let write = async {
            stream.write_all(&buf).await?;
            stream.flush().await
        };
        match tokio::time::timeout(self.read_timeout, write).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                self.broken = true;
                Err(NntpError::Io(e))
            }
            Err(_) => {
                self.broken = true;
                Err(NntpError::Timeout)
            }
        }
    }

    async fn read_response(&mut self) -> Result<Response, NntpError> {
        let mut line = std::mem::take(&mut self.line_buf);
        line.clear();
        let result = match self.read_line(&mut line, MAX_STATUS_LINE).await {
            Ok(()) => Response::parse(&line),
            Err(e) => Err(e),
        };
        self.restore_line_buf(line);
        match result {
            Ok(r) => {
                tracing::trace!(address = %self.address, code = r.code, "<-");
                if self.transcript.is_some() {
                    let shown = self.redact(&r.to_string());
                    if let Some(t) = self.transcript.as_mut() {
                        t.push(format!("<- {shown}"));
                    }
                }
                Ok(r)
            }
            Err(e) => {
                self.broken = true;
                Err(e)
            }
        }
    }

    /// Reads a dot-terminated multi-line block, unstuffing leading dots.
    async fn read_multiline(&mut self) -> Result<Vec<u8>, NntpError> {
        let mut out = Vec::with_capacity(READ_BUFFER);
        let mut line = std::mem::take(&mut self.line_buf);
        let result = loop {
            line.clear();
            // Room for the remaining budget plus a stuffed dot and CRLF.
            let limit = MAX_ARTICLE_BYTES.saturating_sub(out.len()) + 3;
            if let Err(e) = self.read_line(&mut line, limit).await {
                break Err(e);
            }
            let content = strip_eol(&line);
            if content == b"." {
                break Ok(());
            }
            let content = match content.first() {
                Some(b'.') => &content[1..],
                _ => content,
            };
            if out.len() + content.len() + 2 > MAX_ARTICLE_BYTES {
                self.broken = true;
                break Err(NntpError::Protocol(format!(
                    "article exceeds the {} MiB limit",
                    MAX_ARTICLE_BYTES / (1024 * 1024)
                )));
            }
            out.extend_from_slice(content);
            out.extend_from_slice(b"\r\n");
        };
        self.restore_line_buf(line);
        result.map(|()| out)
    }

    /// Reads one line (through `\n`) into `buf`, failing if it would exceed `limit` bytes.
    async fn read_line(&mut self, buf: &mut Vec<u8>, limit: usize) -> Result<(), NntpError> {
        let reader = &mut self.reader;
        let read = async {
            let mut limited = reader.take(limit as u64);
            limited.read_until(b'\n', &mut *buf).await
        };
        let outcome = tokio::time::timeout(self.read_timeout, read).await;
        let err = match outcome {
            Err(_) => NntpError::Timeout,
            Ok(Err(e)) => NntpError::Io(e),
            Ok(Ok(0)) => NntpError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "server closed the connection",
            )),
            Ok(Ok(n)) => {
                if buf.last() == Some(&b'\n') {
                    return Ok(());
                }
                if n >= limit {
                    NntpError::Protocol(format!("server sent a line longer than {limit} bytes"))
                } else {
                    NntpError::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "server closed the connection mid-line",
                    ))
                }
            }
        };
        self.broken = true;
        Err(err)
    }

    fn restore_line_buf(&mut self, mut line: Vec<u8>) {
        if line.capacity() > LINE_BUF_KEEP {
            line = Vec::new();
        }
        self.line_buf = line;
    }

    /// Hides credentials a server might echo back in a reply (transcript only).
    fn redact(&self, text: &str) -> String {
        let mut shown = text.to_owned();
        if let Some(c) = &self.credentials {
            if !c.password.is_empty() {
                shown = shown.replace(&c.password, MASK);
            }
            // Very short usernames would mask unrelated text; they are not secret enough
            // to justify mangling the transcript.
            if c.username.len() >= 3 {
                shown = shown.replace(&c.username, MASK);
            }
        }
        shown
    }
}

/// Settings "Test" action: connects, authenticates (if a username is set) and issues
/// `DATE`, returning a transcript of the exchange, one line per step:
///
/// ```text
/// <- 200 news.example.com ready
/// -> AUTHINFO USER ***
/// <- 381 password required
/// -> AUTHINFO PASS ***
/// <- 281 authentication accepted
/// -> DATE
/// <- 111 20261031120000
/// ```
///
/// Credentials are always masked. Any `DATE` reply (even 500) is reported rather than
/// treated as failure, since the handshake itself succeeded. `timeout` bounds the
/// connect/auth phase and, separately, the `DATE` exchange.
pub async fn test_server(server: &ServerConfig, timeout: Duration) -> Result<String, NntpError> {
    let mut conn = NntpConnection::connect_inner(server, timeout, true).await?;
    conn.set_read_timeout(timeout);
    conn.command("DATE").await?;
    let transcript = conn.transcript.take().unwrap_or_default().join("\n");
    conn.quit().await;
    Ok(transcript)
}

fn auth_error(r: &Response) -> NntpError {
    match r.code {
        400..=599 => NntpError::AuthFailed(r.to_string()),
        _ => NntpError::Protocol(format!("unexpected reply during authentication: {r}")),
    }
}

/// Replaces AUTHINFO arguments with a mask so credentials never reach logs/transcripts.
fn mask_command(line: &str) -> String {
    let mut parts = line.splitn(3, ' ');
    let first = parts.next().unwrap_or_default();
    if first.eq_ignore_ascii_case("AUTHINFO") {
        let sub = parts.next().unwrap_or_default();
        if parts.next().is_some() {
            return format!("{first} {sub} {MASK}");
        }
    }
    line.to_owned()
}

fn strip_eol(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_responses() {
        let r = Response::parse(b"200 news.example.com ready\r\n").unwrap();
        assert_eq!(r, Response { code: 200, text: "news.example.com ready".into() });
        assert_eq!(r.to_string(), "200 news.example.com ready");
        let r = Response::parse(b"205\n").unwrap();
        assert_eq!(r.code, 205);
        assert_eq!(r.text, "");
        assert_eq!(r.to_string(), "205");
        assert!(Response::parse(b"ok then\r\n").is_err());
        assert!(Response::parse(b"20\r\n").is_err());
        assert!(Response::parse(b"2000 x\r\n").is_err());
        assert!(Response::parse(b"").is_err());
    }

    #[test]
    fn masks_authinfo_arguments() {
        assert_eq!(mask_command("AUTHINFO USER bob"), "AUTHINFO USER ***");
        assert_eq!(mask_command("authinfo pass s3cret with space"), "authinfo pass ***");
        assert_eq!(mask_command("BODY <a@b>"), "BODY <a@b>");
        assert_eq!(mask_command("AUTHINFO"), "AUTHINFO");
    }

    #[test]
    fn strips_line_endings() {
        assert_eq!(strip_eol(b"abc\r\n"), b"abc");
        assert_eq!(strip_eol(b"abc\n"), b"abc");
        assert_eq!(strip_eol(b"abc"), b"abc");
        assert_eq!(strip_eol(b"a\rb\r\n"), b"a\rb");
    }

    #[test]
    fn auth_error_mapping() {
        let r = |code| Response { code, text: "nope".into() };
        assert!(matches!(auth_error(&r(481)), NntpError::AuthFailed(t) if t == "481 nope"));
        assert!(matches!(auth_error(&r(482)), NntpError::AuthFailed(_)));
        assert!(matches!(auth_error(&r(502)), NntpError::AuthFailed(_)));
        assert!(matches!(auth_error(&r(211)), NntpError::Protocol(_)));
    }
}
