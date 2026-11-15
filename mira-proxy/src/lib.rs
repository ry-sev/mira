//! Mira - HTTP/HTTPS intercepting proxy
//!
//! A debugging proxy similar to Charles Proxy and mitmproxy, written in Rust.
//! Allows developers to inspect, capture, and debug HTTP traffic.
//!
//! # Example
//!
//! ```ignore
//! use mira_proxy::Config;
//! use mira_proxy::Proxy;
//!
//! #[tokio::main]
//! async fn main() {
//!     let proxy = Proxy::builder(Config::new(8888))
//!         .on_exchange(|exchange| {
//!             println!(
//!                 "{} {} -> {}",
//!                 exchange.request.method,
//!                 exchange.request.url,
//!                 exchange.response.map_or(0, |r| r.status)
//!             );
//!         })
//!         .build();
//!
//!     proxy.run().await.unwrap();
//! }
//! ```

pub mod config;
pub mod config_file;
pub mod error;
pub mod har;
pub mod http;
pub mod proxy;
pub mod session;
pub mod tls;

pub use config::Config;
pub use config::DEFAULT_BIND;
pub use config::DEFAULT_CONNECT_TIMEOUT;
pub use config::DEFAULT_MAX_BODY_SIZE;
pub use config::DEFAULT_MAX_CONNECTIONS;
pub use config::DEFAULT_MAX_HEADER_SIZE;
pub use config::DEFAULT_PORT;
pub use config::DEFAULT_READ_TIMEOUT;
pub use config::DEFAULT_WRITE_TIMEOUT;
pub use config::HostFilter;
pub use config::HttpsConfig;
pub use config::is_private_ip;
pub use config_file::FileConfig;
pub use error::Error;
pub use error::Result;
pub use har::Har;
pub use har::HarBuilder;
pub use har::HarEntry;
pub use har::HarLog;
pub use http::Encoding;
pub use http::Exchange;
pub use http::Request;
pub use http::ResourceType;
pub use http::Response;
pub use http::Timings;
pub use proxy::Proxy;
pub use proxy::ProxyBuilder;
pub use session::CapturedExchange;
pub use session::DEFAULT_MAX_CAPTURED_BODY_BYTES;
pub use session::DEFAULT_MAX_EXCHANGES;
pub use session::DEFAULT_MAX_TOTAL_BODY_BYTES;
pub use session::Session;
pub use session::SessionLimits;
pub use tls::AlpnProtocol;
pub use tls::CertificateAuthority;
pub use tls::CertificateCache;
pub use tls::TlsConfig;
