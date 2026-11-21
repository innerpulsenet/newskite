//! Plain/TLS transport and the shared rustls client configuration.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

use crate::NntpError;

/// The transport under an [`crate::NntpConnection`].
pub(crate) enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl Stream {
    /// Wraps `tcp` in TLS, verifying the certificate against `host` (also sent as SNI).
    pub(crate) async fn tls(tcp: TcpStream, host: &str) -> Result<Self, NntpError> {
        let config = shared_client_config().await?;
        let name = ServerName::try_from(host.to_owned())
            .map_err(|e| NntpError::Tls(format!("invalid server name {host:?}: {e}")))?;
        let tls = TlsConnector::from(config)
            .connect(name, tcp)
            .await
            .map_err(|e| NntpError::Tls(tls_error_text(&e)))?;
        Ok(Stream::Tls(Box::new(tls)))
    }
}

fn tls_error_text(e: &io::Error) -> String {
    match e.get_ref() {
        Some(inner) => inner.to_string(),
        None => e.to_string(),
    }
}

/// [`client_config`] without blocking an async thread: the first call reads the platform's
/// certificate store, which may take a while, so it runs on the blocking pool (and every
/// connection opened meanwhile waits there rather than on a runtime thread).
async fn shared_client_config() -> Result<Arc<rustls::ClientConfig>, NntpError> {
    match CONFIG.get() {
        Some(built) => built.clone().map_err(NntpError::Tls),
        None => tokio::task::spawn_blocking(client_config)
            .await
            .map_err(|e| NntpError::Tls(format!("loading the TLS configuration failed: {e}")))?,
    }
}

static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();

/// Builds (once per process) the rustls client config: `ring` provider, safe default
/// protocol versions, roots = platform store plus bundled Mozilla roots.
fn client_config() -> Result<Arc<rustls::ClientConfig>, NntpError> {
    CONFIG
        .get_or_init(build_client_config)
        .clone()
        .map_err(NntpError::Tls)
}

fn build_client_config() -> Result<Arc<rustls::ClientConfig>, String> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    if !native.errors.is_empty() {
        tracing::debug!(errors = native.errors.len(), "some platform root certificates could not be loaded");
    }
    let (added, ignored) = roots.add_parsable_certificates(native.certs);
    tracing::debug!(added, ignored, "loaded platform root certificates");
    // Always add the bundled roots: covers systems with an empty or unreadable store.
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Stream::Plain(s) => s.is_write_vectored(),
            Stream::Tls(s) => s.is_write_vectored(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_config_builds_with_roots() {
        let cfg = client_config().expect("config");
        // Building twice returns the cached instance.
        let again = client_config().expect("config");
        assert!(Arc::ptr_eq(&cfg, &again));
    }

    #[tokio::test]
    async fn shared_config_is_the_cached_one() {
        let from_async = shared_client_config().await.expect("config");
        assert!(Arc::ptr_eq(&from_async, &client_config().expect("config")));
    }
}
