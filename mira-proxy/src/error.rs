//! Error types for Mira proxy.

use std::net::SocketAddr;
use std::time::Duration;

/// Result type alias using Mira's Error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors that can occur in the Mira proxy.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The listen address could not be bound.
    #[error("failed to bind to {addr}")]
    Bind {
        /// The address that was tried.
        addr: SocketAddr,

        /// Why binding failed.
        #[source]
        source: std::io::Error,
    },

    /// An I/O error on a client or upstream connection.
    #[error("connection error: {0}")]
    Connection(#[from] std::io::Error),

    /// A request or response head is not valid HTTP.
    #[error("HTTP parse error: {0}")]
    HttpParse(String),

    /// The client's request cannot be forwarded as sent.
    #[error("invalid HTTP request: {0}")]
    InvalidRequest(String),

    /// The origin's response cannot be relayed as sent.
    #[error("invalid HTTP response: {0}")]
    InvalidResponse(String),

    /// The origin could not be connected to.
    #[error("upstream connection failed: {host}")]
    UpstreamConnection {
        /// The `host:port` that was tried.
        host: String,

        /// Why connecting failed.
        #[source]
        source: std::io::Error,
    },

    /// An operation did not finish within its deadline.
    #[error("{operation} timed out after {duration:?}")]
    Timeout {
        /// What timed out, for logs and error responses.
        operation: String,

        /// The deadline that passed.
        duration: Duration,
    },

    /// A request body exceeds `max_body_size`.
    #[error("body too large: {size} bytes exceeds maximum of {max} bytes")]
    BodyTooLarge {
        /// The body's size, or as much of it as was seen.
        size: usize,

        /// The configured maximum.
        max: usize,
    },

    /// A head grew past `max_header_size` without ending.
    #[error("header block too large: {size} bytes exceeds maximum of {max} bytes")]
    HeadersTooLarge {
        /// Bytes buffered when the limit was hit.
        size: usize,

        /// The configured maximum.
        max: usize,
    },

    /// A head carries more header fields than the parser holds.
    #[error("too many header fields: more than {max}")]
    TooManyHeaders {
        /// The most fields a head may carry.
        max: usize,
    },

    /// An error from hyper, which serves HTTP/2.
    #[error("hyper error: {0}")]
    Hyper(#[from] hyper::Error),

    /// A URI that does not parse.
    #[error("invalid URI: {0}")]
    InvalidUri(#[from] http::uri::InvalidUri),

    /// An HTTP message that could not be built.
    #[error("HTTP error: {0}")]
    Http(#[from] http::Error),

    /// A HAR document that could not be read or written.
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    /// The TLS setup is unusable, such as an invalid server name.
    #[error("TLS configuration error: {0}")]
    TlsConfig(String),

    /// A TLS handshake with the client or the origin failed.
    #[error("TLS handshake failed: {0}")]
    TlsHandshake(String),

    /// A certificate could not be generated or signed.
    #[error("certificate generation failed: {0}")]
    CertificateGeneration(String),

    /// The CA could not be read from disk.
    #[error("failed to load certificate: {0}")]
    CertificateLoad(String),

    /// The CA could not be written to disk.
    #[error("failed to save certificate: {0}")]
    CertificateSave(String),

    /// A config file could not be read, parsed, or written.
    #[error("config file {}: {reason}", path.display())]
    ConfigFile {
        /// The file.
        path: std::path::PathBuf,

        /// What went wrong.
        reason: String,
    },
}
