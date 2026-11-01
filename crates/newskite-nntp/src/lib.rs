//! Async NNTP client for Newskite.
//!
//! [`NntpConnection`] speaks the reader subset of NNTP (RFC 3977 / RFC 4643) that a binary
//! downloader needs: greeting, `AUTHINFO USER`/`PASS`, `BODY`, `STAT`, `GROUP`, `DATE` and
//! `QUIT`, over plain TCP or TLS (rustls with the `ring` provider; trust roots are the
//! platform store plus the bundled Mozilla roots).
//!
//! Multi-line bodies are read chunk by chunk straight from the socket buffer, dot-unstuffed,
//! and returned with every line terminated by `\r\n` (bare `\n` from sloppy servers is
//! normalized). A single article is capped at [`MAX_ARTICLE_BYTES`] so a hostile server
//! cannot exhaust memory. `BODY` requests can be pipelined
//! ([`NntpConnection::send_body`] / [`NntpConnection::begin_body`] /
//! [`NntpConnection::finish_body`]) so a connection never sits idle for a round trip
//! between articles.
//!
//! [`test_server`] performs the Settings "Test" handshake and returns a human-readable
//! transcript that never contains the password (or the username).
//!
//! With the `testkit` feature, [`testkit::MockServer`] provides an in-process NNTP server
//! for tests in this and downstream crates.

mod conn;
mod error;
mod msgid;
mod stream;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use conn::{test_server, NntpConnection, Response, DEFAULT_READ_TIMEOUT, MAX_ARTICLE_BYTES};
pub use error::NntpError;
pub use msgid::normalize_message_id;
