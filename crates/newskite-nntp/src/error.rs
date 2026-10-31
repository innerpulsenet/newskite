//! Error type for the NNTP client.

/// Everything that can go wrong talking to an NNTP server.
#[derive(Debug, thiserror::Error)]
pub enum NntpError {
    /// Socket-level failure (reset, unexpected EOF, ...).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// TLS configuration or handshake failure.
    #[error("TLS error: {0}")]
    Tls(String),
    /// The server did not answer within the allowed time.
    #[error("timed out waiting for the server")]
    Timeout,
    /// Could not establish a usable session (DNS, refused, bad greeting).
    #[error("connection failed: {0}")]
    Connect(String),
    /// The server requires authentication and no credentials are configured.
    #[error("server requires authentication")]
    AuthRequired,
    /// The server rejected the credentials; carries the server's reply text.
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    /// The article is not available on this server (430/423); carries the message id.
    #[error("article not found: {0}")]
    ArticleMissing(String),
    /// The server sent something we do not understand or did not expect.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The message id is empty or contains characters not allowed on a command line.
    #[error("invalid message id: {0:?}")]
    InvalidMessageId(String),
}

impl NntpError {
    /// True for failures worth retrying (possibly on another connection or server):
    /// `Io`, `Timeout`, `Connect`, `Tls` and `Protocol`.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            NntpError::Io(_)
                | NntpError::Timeout
                | NntpError::Connect(_)
                | NntpError::Tls(_)
                | NntpError::Protocol(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_classification() {
        assert!(NntpError::Timeout.is_transient());
        assert!(NntpError::Connect("x".into()).is_transient());
        assert!(NntpError::Tls("x".into()).is_transient());
        assert!(NntpError::Protocol("x".into()).is_transient());
        assert!(NntpError::Io(std::io::Error::other("x")).is_transient());
        assert!(!NntpError::AuthRequired.is_transient());
        assert!(!NntpError::AuthFailed("x".into()).is_transient());
        assert!(!NntpError::ArticleMissing("x".into()).is_transient());
        assert!(!NntpError::InvalidMessageId("x".into()).is_transient());
    }
}
