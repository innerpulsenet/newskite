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
const READ_BUFFER: usize = 128 * 1024;
/// The reusable line buffer is shrunk back to this after an unusually long line.
const LINE_BUF_KEEP: usize = 64 * 1024;
/// Initial capacity of a body buffer when the connection has not read a body yet.
const BODY_INITIAL: usize = 64 * 1024;
/// Slack added to the previous body's length when sizing the next body buffer.
const BODY_SLACK: usize = 8 * 1024;
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
    write_buf: Vec<u8>,
    /// `BODY` commands sent whose status line has not been read yet (pipelining).
    outstanding: usize,
    /// A `222` status was read and its multi-line block is still to be read.
    in_body: bool,
    /// Length of the last body read; sizes the next body buffer (segments of one job are
    /// nearly uniform, so this avoids regrowing the buffer for every article).
    body_hint: usize,
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
                write_buf: Vec::new(),
                outstanding: 0,
                in_body: false,
                body_hint: 0,
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
    /// Refused while a pipelined `BODY` is outstanding (see [`send_body`](Self::send_body)).
    pub async fn command(&mut self, line: &str) -> Result<Response, NntpError> {
        self.check_idle()?;
        self.send_command(line).await?;
        self.read_response().await
    }

    /// Validates and sends a command line without reading the reply.
    async fn send_command(&mut self, line: &str) -> Result<(), NntpError> {
        if line.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
            return Err(NntpError::Protocol("refusing to send a command containing CR, LF or NUL".into()));
        }
        if self.broken {
            return Err(NntpError::Protocol("connection is unusable after an earlier error".into()));
        }
        self.send_line(line).await
    }

    /// Fails unless no pipelined `BODY` is outstanding and no body block is pending.
    fn check_idle(&self) -> Result<(), NntpError> {
        if self.outstanding > 0 || self.in_body {
            return Err(NntpError::Protocol("a pipelined BODY reply is still pending".into()));
        }
        Ok(())
    }

    /// Number of `BODY` commands sent with [`send_body`](Self::send_body) whose status
    /// line has not been read yet.
    pub fn outstanding_bodies(&self) -> usize {
        self.outstanding
    }

    /// Fetches an article body (`BODY <id>`); `message_id` may omit the angle brackets.
    ///
    /// Returns the dot-unstuffed payload without the terminating `.` line; every line is
    /// terminated by `\r\n` (bare `\n` is normalized). 430/423 → [`NntpError::ArticleMissing`]
    /// (with the id without brackets); 480 → re-authenticates once if credentials are
    /// configured, otherwise [`NntpError::AuthRequired`].
    ///
    /// Other refusals are classified so callers never retry a permanent answer forever:
    /// 400 or a connection/rate-limit reply → [`NntpError::Connect`] (transient, session
    /// considered broken); 481/482/483/502 → [`NntpError::AuthFailed`] (this server refuses
    /// the account); any other 4xx/5xx (451 takedown, 500/501/503, ...) →
    /// [`NntpError::ArticleMissing`] for this server. Error texts are credential-redacted.
    pub async fn body(&mut self, message_id: &str) -> Result<Vec<u8>, NntpError> {
        self.check_idle()?;
        self.send_body(message_id).await?;
        self.begin_body(message_id).await?;
        self.finish_body().await
    }

    /// Pipelining, step 1: sends `BODY <id>` without waiting for the reply.
    ///
    /// Several bodies may be requested back to back, also while the current body block is
    /// still being read; their replies must then be collected in the same order with
    /// [`begin_body`](Self::begin_body) and [`finish_body`](Self::finish_body). Other
    /// commands are refused until every outstanding reply has been read.
    pub async fn send_body(&mut self, message_id: &str) -> Result<(), NntpError> {
        let id = normalize_message_id(message_id)?;
        self.send_command(&format!("BODY {id}")).await?;
        self.outstanding += 1;
        Ok(())
    }

    /// Pipelining, step 2: reads the status line of the oldest outstanding `BODY`, which
    /// must have been for `message_id`. `Ok(())` means the server answered `222` and the
    /// body block follows ([`finish_body`](Self::finish_body)); the errors are those of
    /// [`body`](Self::body).
    ///
    /// A `480` is handled by re-authenticating and resending only when no other reply is
    /// outstanding; otherwise the session is given up as broken ([`NntpError::Connect`],
    /// transient) since replaying the pipeline is not worth the complexity.
    pub async fn begin_body(&mut self, message_id: &str) -> Result<(), NntpError> {
        if self.outstanding == 0 || self.in_body {
            return Err(NntpError::Protocol("no outstanding BODY reply to read".into()));
        }
        let id = normalize_message_id(message_id)?;
        let mut r = self.read_response().await?;
        self.outstanding -= 1;
        if r.code == 480 && self.credentials.is_some() {
            if self.outstanding > 0 {
                self.broken = true;
                return Err(NntpError::Connect("session lost its authentication while replies were pipelined".into()));
            }
            tracing::debug!(address = %self.address, "server asked for authentication; re-authenticating");
            self.authenticate().await?;
            r = self.command(&format!("BODY {id}")).await?;
        }
        match r.code {
            222 => {
                self.in_body = true;
                Ok(())
            }
            430 | 423 => Err(NntpError::ArticleMissing(bare(&id).to_owned())),
            480 => Err(NntpError::AuthRequired),
            400..=599 => Err(self.refusal("BODY", &r, &id)),
            _ => Err(self.unexpected("BODY", &r)),
        }
    }

    /// Pipelining, step 3: reads the body block announced by a successful
    /// [`begin_body`](Self::begin_body).
    pub async fn finish_body(&mut self) -> Result<Vec<u8>, NntpError> {
        if !self.in_body {
            return Err(NntpError::Protocol("no body block is pending".into()));
        }
        let body = self.read_multiline().await;
        self.in_body = false;
        if let Ok(b) = &body {
            self.body_hint = b.len();
        }
        body
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
        if !self.broken && self.outstanding == 0 && !self.in_body {
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
            _ => return Err(self.auth_error(&r)),
        }
        let r = self.command(&pass_line).await?;
        drop(pass_line);
        match r.code {
            281 => Ok(()),
            _ => Err(self.auth_error(&r)),
        }
    }

    /// Classifies a non-success reply to AUTHINFO. 400 and replies that only enforce a
    /// connection or rate limit (`502 Too many connections`, `481 connection limit
    /// reached`) are transient [`NntpError::Connect`], so the caller backs off instead of
    /// treating the account as permanently rejected; other 4xx/5xx → `AuthFailed`.
    /// The reply text is credential-redacted.
    fn auth_error(&mut self, r: &Response) -> NntpError {
        let shown = self.redact(&r.to_string());
        if r.code == 400 || (matches!(r.code, 481 | 482 | 502) && mentions_limit(&r.text)) {
            self.broken = true;
            return NntpError::Connect(format!("server refused authentication for now: {shown}"));
        }
        match r.code {
            400..=599 => NntpError::AuthFailed(shown),
            _ => {
                self.broken = true;
                NntpError::Protocol(format!("unexpected reply during authentication: {shown}"))
            }
        }
    }

    /// Classifies a 4xx/5xx reply to an article command (see [`body`](Self::body)).
    fn refusal(&mut self, what: &str, r: &Response, id: &str) -> NntpError {
        let shown = self.redact(&r.to_string());
        match r.code {
            400 => {
                self.broken = true;
                NntpError::Connect(format!("server ended the session during {what}: {shown}"))
            }
            481 | 482 | 502 if mentions_limit(&r.text) => {
                self.broken = true;
                NntpError::Connect(format!("server refused {what} for now: {shown}"))
            }
            481..=483 | 502 => NntpError::AuthFailed(shown),
            _ => {
                tracing::debug!(address = %self.address, reply = %shown, "{what} refused; treating the article as missing here");
                NntpError::ArticleMissing(bare(id).to_owned())
            }
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
        let mut buf = std::mem::take(&mut self.write_buf);
        buf.clear();
        buf.extend_from_slice(line.as_bytes());
        buf.extend_from_slice(b"\r\n");
        let stream = self.reader.get_mut();
        let write = async {
            stream.write_all(&buf).await?;
            stream.flush().await
        };
        let outcome = tokio::time::timeout(self.read_timeout, write).await;
        self.write_buf = buf;
        match outcome {
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
    ///
    /// Works on the reader's buffer chunk by chunk instead of line by line: runs of body
    /// bytes are appended in one copy, only line starts (where a stuffed dot or the
    /// terminator can be) are examined one byte at a time, and the inactivity timeout wraps
    /// each socket read rather than each line.
    async fn read_multiline(&mut self) -> Result<Vec<u8>, NntpError> {
        let capacity = match self.body_hint {
            0 => BODY_INITIAL,
            n => n.saturating_add(BODY_SLACK).min(MAX_ARTICLE_BYTES),
        };
        let mut out: Vec<u8> = Vec::with_capacity(capacity);
        let mut state = Block::LineStart;
        loop {
            let chunk = match self.fill_buf().await {
                Ok(c) => c,
                Err(e) => {
                    self.broken = true;
                    return Err(e);
                }
            };
            let (used, done) = match unstuff_chunk(chunk, &mut out, &mut state) {
                Ok(step) => step,
                Err(e) => {
                    self.broken = true;
                    return Err(e);
                }
            };
            self.reader.consume(used);
            if done {
                return Ok(out);
            }
        }
    }

    /// The reader's buffered bytes, filling it from the socket (bounded by the inactivity
    /// timeout) when empty. The returned slice is never empty.
    async fn fill_buf(&mut self) -> Result<&[u8], NntpError> {
        let reader = &mut self.reader;
        match tokio::time::timeout(self.read_timeout, reader.fill_buf()).await {
            Err(_) => Err(NntpError::Timeout),
            Ok(Err(e)) => Err(NntpError::Io(e)),
            Ok(Ok([])) => Err(NntpError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "server closed the connection mid-body",
            ))),
            Ok(Ok(_)) => Ok(self.reader.buffer()),
        }
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

    /// Hides credentials a server might echo back in a reply (transcripts and error texts).
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

/// Parser position inside a multi-line block, carried across buffer chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Block {
    /// At the start of a line: a `.` here is either stuffing or the terminator.
    LineStart,
    /// Saw `.` at a line start; the next byte decides.
    AfterDot,
    /// Saw `.\r` at a line start; a `\n` completes the terminator.
    AfterDotCr,
    /// Inside a line: bytes are copied through to the next `\n`.
    Mid,
}

/// Feeds one buffer chunk of a multi-line block through the dot-unstuffing parser.
///
/// Appends body bytes to `out` with every line ending normalized to `\r\n`, and returns
/// how many bytes of `chunk` were consumed plus whether the terminating `.` line was seen.
/// Fails ([`NntpError::Protocol`]) once the body would exceed [`MAX_ARTICLE_BYTES`].
fn unstuff_chunk(chunk: &[u8], out: &mut Vec<u8>, state: &mut Block) -> Result<(usize, bool), NntpError> {
    let mut i = 0;
    while i < chunk.len() {
        match *state {
            Block::LineStart => {
                if chunk[i] == b'.' {
                    *state = Block::AfterDot;
                    i += 1;
                } else {
                    *state = Block::Mid;
                }
            }
            Block::AfterDot => match chunk[i] {
                b'\n' => return Ok((i + 1, true)),
                b'\r' => {
                    *state = Block::AfterDotCr;
                    i += 1;
                }
                // `..x` is a stuffed `.x`; a lone `.x` from a sloppy server is read the same.
                _ => *state = Block::Mid,
            },
            Block::AfterDotCr => {
                if chunk[i] == b'\n' {
                    return Ok((i + 1, true));
                }
                // `.\r` followed by more text: a sloppy line; keep the CR as data.
                out.push(b'\r');
                *state = Block::Mid;
            }
            Block::Mid => {
                let rest = &chunk[i..];
                match rest.iter().position(|&b| b == b'\n') {
                    Some(n) => {
                        // Normalize the line ending: `\r\n` as is, bare `\n` becomes `\r\n`.
                        let line = &rest[..n];
                        if line.is_empty() && out.last() == Some(&b'\r') {
                            // The CR was already appended from the previous chunk.
                            check_limit(out.len(), 1)?;
                            out.push(b'\n');
                        } else {
                            let content = line.strip_suffix(b"\r").unwrap_or(line);
                            check_limit(out.len(), content.len() + 2)?;
                            out.extend_from_slice(content);
                            out.extend_from_slice(b"\r\n");
                        }
                        i += n + 1;
                        *state = Block::LineStart;
                    }
                    None => {
                        check_limit(out.len(), rest.len())?;
                        out.extend_from_slice(rest);
                        i = chunk.len();
                    }
                }
            }
        }
    }
    Ok((i, false))
}

fn check_limit(have: usize, add: usize) -> Result<(), NntpError> {
    if have.saturating_add(add) > MAX_ARTICLE_BYTES {
        return Err(NntpError::Protocol(format!(
            "article exceeds the {} MiB limit",
            MAX_ARTICLE_BYTES / (1024 * 1024)
        )));
    }
    Ok(())
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

/// True when a refusal is about a connection or rate limit rather than the account
/// itself, e.g. `502 Too many connections` or `481 connection limit reached`.
fn mentions_limit(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    ["too many", "connections", "limit", "temporar", "try again", "overload", "busy"]
        .iter()
        .any(|k| t.contains(k))
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

    /// Runs the chunk parser over `input` split at every `split` boundary.
    fn unstuff_split(input: &[u8], split: usize) -> (Vec<u8>, bool, usize) {
        let mut out = Vec::new();
        let mut state = Block::LineStart;
        let mut pos = 0;
        while pos < input.len() {
            let end = (pos + split).min(input.len());
            let (used, done) = unstuff_chunk(&input[pos..end], &mut out, &mut state).unwrap();
            if done {
                return (out, true, pos + used);
            }
            assert_eq!(used, end - pos, "a chunk is consumed whole unless the block ends");
            pos += used;
        }
        (out, false, pos)
    }

    #[test]
    fn unstuffs_and_terminates_at_every_chunk_boundary() {
        let input = b"..a\r\n.\rb\r\n\r\nline\nbare\r.x\r\n.\r\nTRAILING";
        let want = b".a\r\n\rb\r\n\r\nline\r\nbare\r.x\r\n";
        for split in 1..=input.len() {
            let (out, done, used) = unstuff_split(input, split);
            assert!(done, "split {split}");
            assert_eq!(out, want, "split {split}");
            assert_eq!(&input[used..], b"TRAILING", "split {split}");
        }
        // Bare-LF terminator and an empty block.
        assert_eq!(unstuff_split(b"x\n.\n", 1), (b"x\r\n".to_vec(), true, 4));
        assert_eq!(unstuff_split(b".\r\n", 2), (Vec::new(), true, 3));
        // No terminator yet: everything consumed, not done.
        assert_eq!(unstuff_split(b"abc\r\nde", 3), (b"abc\r\nde".to_vec(), false, 7));
    }

    #[test]
    fn limit_replies_are_recognized() {
        assert!(mentions_limit("Too many connections"));
        assert!(mentions_limit("connection limit reached"));
        assert!(mentions_limit("Max connections exceeded for user"));
        assert!(mentions_limit("Service temporarily unavailable"));
        assert!(!mentions_limit("Authentication failed"));
        assert!(!mentions_limit("Access denied, account expired"));
        assert!(!mentions_limit("nope"));
    }
}
