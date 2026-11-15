//! TLS support for HTTPS interception.
//!
//! This module provides certificate authority (CA) management, on-the-fly
//! certificate generation for intercepted hosts, and TLS configuration.

mod ca;
mod cert_cache;

use crate::error::Result;
pub use ca::CertificateAuthority;
pub use cert_cache::CertificateCache;
use rustls::ServerConfig;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use std::sync::Arc;
use std::sync::OnceLock;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::TlsConnector;

/// ALPN identifier for HTTP/1.1.
const ALPN_HTTP11: &[u8] = b"http/1.1";

/// ALPN identifier for HTTP/2 over TLS.
const ALPN_HTTP2: &[u8] = b"h2";

/// The application protocol negotiated over a TLS connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlpnProtocol {
    /// HTTP/1.1, or no ALPN at all — the pre-ALPN default.
    Http11,

    /// HTTP/2 over TLS.
    Http2,
}

impl AlpnProtocol {
    /// Read the protocol out of a completed handshake.
    ///
    /// A peer that negotiated nothing gets HTTP/1.1: that is what a
    /// client assumes when a server answers without the ALPN extension.
    /// An unrecognized protocol gets the same treatment — we only ever
    /// offer protocols we speak, so it cannot be reached without
    /// someone changing what is advertised.
    pub fn from_negotiated(protocol: Option<&[u8]>) -> Self {
        match protocol {
            Some(ALPN_HTTP2) => Self::Http2,
            _ => Self::Http11,
        }
    }

    /// The wire identifier, for logging.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http11 => "http/1.1",
            Self::Http2 => "h2",
        }
    }
}

/// TLS configuration for the proxy.
///
/// Manages the CA, certificate cache, and provides TLS acceptors for
/// intercepting HTTPS connections.
#[derive(Clone)]
pub struct TlsConfig {
    /// The certificate authority used to sign generated certificates.
    ca: Arc<CertificateAuthority>,

    /// Cache of generated certificates per host.
    cache: Arc<CertificateCache>,

    /// Lazily-built, shared connectors for originating TLS to upstream
    /// servers. Building them loads the system trust store from disk,
    /// which is far too expensive to repeat per intercepted connection,
    /// so they are constructed once on first use and reused thereafter.
    /// Shared across clones via `Arc` so every handler hits the same
    /// cached connectors.
    upstream_connectors: Arc<OnceLock<UpstreamConnectors>>,
}

/// One connector per protocol the proxy might offer an origin.
///
/// Which one a connection uses mirrors what the *client* negotiated: a
/// client speaking HTTP/1.1 gets an HTTP/1.1 upstream leg, so what the
/// capture shows on one side is what happened on the other. It also
/// spares the proxy an HTTP/1.1-to-HTTP/2 translation that would only
/// ever run for a client that did not ask for HTTP/2.
struct UpstreamConnectors {
    http1: TlsConnector,
    http2: TlsConnector,
}

impl TlsConfig {
    /// Create a new TLS configuration with the given CA.
    pub fn new(ca: CertificateAuthority) -> Self {
        Self {
            ca: Arc::new(ca),
            cache: Arc::new(CertificateCache::new()),
            upstream_connectors: Arc::new(OnceLock::new()),
        }
    }

    /// Get the certificate authority.
    pub fn ca(&self) -> &CertificateAuthority {
        &self.ca
    }

    /// Get a shared TLS connector for originating connections to upstream
    /// servers, offering `protocol` (and HTTP/1.1 as a fallback when that
    /// protocol is HTTP/2).
    ///
    /// The underlying `ClientConfig` (which trusts the system root store)
    /// is built once on first use and cached; subsequent calls clone the
    /// cheap `Arc`-backed connector rather than re-reading the trust store.
    pub fn upstream_connector_for(&self, protocol: AlpnProtocol) -> Result<TlsConnector> {
        if self.upstream_connectors.get().is_none() {
            // Built outside the lock-free `get`; if two handlers race
            // here the first `set` wins and the loser's equivalent pair
            // is dropped.
            let base = create_client_config()?;

            let mut http1 = base.clone();
            http1.alpn_protocols = vec![ALPN_HTTP11.to_vec()];

            let mut http2 = base;
            // Order is preference order: an origin that speaks both
            // SHOULD be spoken to in the protocol the client chose.
            http2.alpn_protocols = vec![ALPN_HTTP2.to_vec(), ALPN_HTTP11.to_vec()];

            let _ = self.upstream_connectors.set(UpstreamConnectors {
                http1: TlsConnector::from(Arc::new(http1)),
                http2: TlsConnector::from(Arc::new(http2)),
            });
        }

        let connectors = self.upstream_connectors.get().ok_or_else(|| {
            crate::error::Error::TlsConfig("upstream connectors unset".to_owned())
        })?;

        Ok(match protocol {
            AlpnProtocol::Http11 => connectors.http1.clone(),
            AlpnProtocol::Http2 => connectors.http2.clone(),
        })
    }

    /// Get a TLS acceptor for the given hostname.
    ///
    /// This will generate a certificate for the host if one doesn't exist
    /// in the cache, signed by the CA. The session may negotiate HTTP/2.
    pub fn acceptor_for_host(&self, host: &str) -> Result<TlsAcceptor> {
        self.acceptor_with_alpn(host, &[ALPN_HTTP2, ALPN_HTTP11], "")
    }

    /// Get a TLS acceptor that offers only HTTP/1.1.
    ///
    /// The magic domain's pages are written as HTTP/1.1 responses
    /// directly onto the stream, so that one session MUST NOT end up
    /// speaking HTTP/2.
    pub fn acceptor_for_http1_only(&self, host: &str) -> Result<TlsAcceptor> {
        self.acceptor_with_alpn(host, &[ALPN_HTTP11], "#http1")
    }

    /// Build (or reuse) an acceptor for `host` offering `alpn`.
    ///
    /// `cache_suffix` keeps acceptors with different protocol offers
    /// from colliding for the same host — the certificate is the same,
    /// the negotiated protocol is not. It goes after the host, and holds no
    /// `:`, because the cache strips anything after a `:` as a port.
    fn acceptor_with_alpn(
        &self,
        host: &str,
        alpn: &[&[u8]],
        cache_suffix: &str,
    ) -> Result<TlsAcceptor> {
        let cache_key = format!("{host}{cache_suffix}");

        // Check cache first
        if let Some(acceptor) = self.cache.get(&cache_key) {
            return Ok(acceptor);
        }

        // Generate new certificate for this host
        let (cert, key) = self.ca.generate_cert_for_host(host)?;

        // Build server config
        let mut config = build_server_config(cert, key)?;
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        let acceptor = TlsAcceptor::from(Arc::new(config));

        // Cache it
        self.cache.insert(&cache_key, acceptor.clone());

        Ok(acceptor)
    }

    /// Clear the certificate cache.
    pub fn clear_cache(&self) {
        self.cache.clear();
    }

    /// Get the number of cached certificates.
    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }
}

impl std::fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsConfig")
            .field("ca", &self.ca)
            .field("cache_size", &self.cache.len())
            .finish()
    }
}

/// Build a rustls `ServerConfig` from a certificate and private key.
fn build_server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> Result<ServerConfig> {
    // Use the ring crypto provider explicitly
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| crate::error::Error::TlsConfig(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| crate::error::Error::TlsConfig(e.to_string()))?;

    // Callers set `alpn_protocols` for the connection they are building:
    // see `TlsConfig::upstream_connector_for` on this side and
    // `acceptor_for_host` / `acceptor_for_http1_only` on the other.
    config.alpn_protocols = vec![ALPN_HTTP11.to_vec()];

    Ok(config)
}

/// Create a rustls `ClientConfig` for connecting to upstream servers.
///
/// This config trusts the system's root certificates.
pub fn create_client_config() -> Result<rustls::ClientConfig> {
    // Use the ring crypto provider
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    // Load native root certificates
    let mut root_store = rustls::RootCertStore::empty();

    // Load native certs - the new API returns a struct with certs and errors
    let native_certs = rustls_native_certs::load_native_certs();

    // Log any errors but continue
    for err in native_certs.errors {
        tracing::warn!("error loading native certificate: {err}");
    }

    // Add the successfully loaded certs
    for cert in native_certs.certs {
        if let Err(e) = root_store.add(cert) {
            tracing::debug!("failed to add certificate to root store: {e}");
        }
    }

    if root_store.is_empty() {
        tracing::warn!("no native root certificates loaded, TLS verification may fail");
    }

    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| crate::error::Error::TlsConfig(e.to_string()))?
        .with_root_certificates(root_store)
        .with_no_client_auth();

    // Offer only what the proxy speaks. Negotiating nothing at all works
    // — peers fall back to HTTP/1.1 — but it leaves the downgrade
    // implicit, and an origin that varies its behaviour by ALPN sees a
    // connection unlike the one the client would have made.
    config.alpn_protocols = vec![ALPN_HTTP11.to_vec()];

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alpn_protocol_reads_the_negotiated_identifier() {
        assert_eq!(
            AlpnProtocol::from_negotiated(Some(b"h2")),
            AlpnProtocol::Http2
        );
        assert_eq!(
            AlpnProtocol::from_negotiated(Some(b"http/1.1")),
            AlpnProtocol::Http11
        );
    }

    #[test]
    fn test_alpn_protocol_defaults_to_http11() {
        // No ALPN extension is what pre-ALPN peers do, and HTTP/1.1 is
        // what a client assumes when a server answers without one.
        assert_eq!(AlpnProtocol::from_negotiated(None), AlpnProtocol::Http11);
        // An unknown protocol can only appear if someone advertises one
        // the proxy does not speak; treating it as HTTP/1.1 keeps the
        // decision in one place rather than panicking on the handshake.
        assert_eq!(
            AlpnProtocol::from_negotiated(Some(b"spdy/3")),
            AlpnProtocol::Http11
        );
    }

    #[test]
    fn test_upstream_connector_offers_the_client_s_protocol() {
        // The upstream leg mirrors the client leg, so a capture shows
        // the same protocol on both sides of the proxy.
        let tls = TlsConfig::new(CertificateAuthority::generate().expect("generate CA"));

        let http1 = tls
            .upstream_connector_for(AlpnProtocol::Http11)
            .expect("http1 connector");
        assert_eq!(http1.config().alpn_protocols, vec![b"http/1.1".to_vec()]);

        let http2 = tls
            .upstream_connector_for(AlpnProtocol::Http2)
            .expect("http2 connector");
        assert_eq!(
            http2.config().alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            "HTTP/2 first, with HTTP/1.1 for origins that cannot"
        );
    }

    #[test]
    fn test_intercept_acceptor_offers_http2_first() {
        // A browser offers `h2, http/1.1`; rustls picks by the server's
        // preference order, so HTTP/2 has to come first to be chosen.
        let tls = TlsConfig::new(CertificateAuthority::generate().expect("generate CA"));
        let acceptor = tls.acceptor_for_host("example.com").expect("acceptor");
        assert_eq!(
            acceptor.config().alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn test_http1_only_acceptor_withholds_http2() {
        let tls = TlsConfig::new(CertificateAuthority::generate().expect("generate CA"));
        let acceptor = tls.acceptor_for_http1_only("mira.proxy").expect("acceptor");
        assert_eq!(acceptor.config().alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn test_http1_only_acceptors_for_different_hosts_do_not_share_an_entry() {
        let tls = TlsConfig::new(CertificateAuthority::generate().expect("generate CA"));
        tls.acceptor_for_http1_only("a.example").expect("acceptor");
        tls.acceptor_for_http1_only("b.example").expect("acceptor");
        assert_eq!(tls.cache_size(), 2);
    }

    #[test]
    fn test_acceptors_for_the_same_host_do_not_collide_across_protocols() {
        let tls = TlsConfig::new(CertificateAuthority::generate().expect("generate CA"));
        let with_h2 = tls.acceptor_for_host("mira.proxy").expect("acceptor");
        let without = tls.acceptor_for_http1_only("mira.proxy").expect("acceptor");

        assert!(with_h2.config().alpn_protocols.contains(&b"h2".to_vec()));
        assert!(!without.config().alpn_protocols.contains(&b"h2".to_vec()));
    }
}
