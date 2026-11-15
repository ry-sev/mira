//! Callback types and exchange data for proxy interception.

use crate::http::ParsedRequest;
use crate::http::ParsedResponse;
use crate::http::compression::Encoding;
use crate::http::compression::{self};
use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use url::Url;

/// Resource type inferred from the response Content-Type header.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceType {
    /// HTML or XHTML.
    Document,

    /// CSS.
    Stylesheet,

    /// JavaScript.
    Script,

    /// Any `image/*` type.
    Image,

    /// Any `font/*` type, or WOFF.
    Font,

    /// Any `audio/*` or `video/*` type.
    Media,

    /// WebAssembly.
    Wasm,

    /// An `XMLHttpRequest`. Never inferred from a content type; kept for
    /// HAR files that record it.
    Xhr,

    /// XML.
    Fetch,

    /// JSON.
    Json,

    /// Anything else, or no content type.
    #[default]
    Other,
}

impl ResourceType {
    /// Classify a `Content-Type` value, ignoring its parameters.
    pub fn from_mime_type(mime_type: &str) -> Self {
        // Strip any parameters (e.g., charset) from the mime type
        let content_type = mime_type.split(';').next().unwrap_or_default().trim();

        match content_type {
            "text/html" | "application/xhtml+xml" => Self::Document,
            "text/css" => Self::Stylesheet,
            "text/javascript" | "application/javascript" | "application/x-javascript" => {
                Self::Script
            }
            "application/wasm" => Self::Wasm,
            "application/json" => Self::Json,
            "application/xml" | "text/xml" => Self::Fetch,
            t if t.starts_with("image/") => Self::Image,
            t if t.starts_with("font/")
                || t == "application/font-woff"
                || t == "application/font-woff2" =>
            {
                Self::Font
            }
            t if t.starts_with("audio/") || t.starts_with("video/") => Self::Media,
            _ => Self::Other,
        }
    }
}

impl fmt::Display for ResourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Document => "document",
            Self::Stylesheet => "stylesheet",
            Self::Script => "script",
            Self::Image => "image",
            Self::Font => "font",
            Self::Media => "media",
            Self::Wasm => "wasm",
            Self::Xhr => "xhr",
            Self::Fetch => "fetch",
            Self::Json => "json",
            Self::Other => "other",
        };
        f.pad(s)
    }
}

/// A complete HTTP request with metadata.
#[derive(Debug, Clone)]
pub struct Request {
    /// Unique identifier for this request
    pub id: u64,

    /// Client address
    pub client_addr: SocketAddr,

    /// HTTP method
    pub method: String,

    /// Full URL of the request
    pub url: Url,

    /// HTTP version
    pub version: String,

    /// Request headers
    pub headers: Vec<(String, String)>,

    /// Request body (empty for GET/HEAD, may be large for POST)
    pub body: Bytes,
}

impl Request {
    /// Create a new Request from a parsed request.
    ///
    /// Constructs the full URL from the Host header and request target.
    /// For CONNECT requests, uses the target as the authority.
    ///
    /// # Arguments
    /// * `parsed` - The parsed HTTP request
    /// * `id` - Unique request identifier
    /// * `client_addr` - Client socket address
    /// * `body` - Request body
    /// * `is_https` - Whether this request came through a TLS tunnel
    pub fn from_parsed(
        parsed: &ParsedRequest,
        id: u64,
        client_addr: SocketAddr,
        body: Bytes,
        is_https: bool,
    ) -> Self {
        let url = Self::build_url(parsed, is_https);

        Self {
            id,
            client_addr,
            method: parsed.method.clone(),
            url,
            version: parsed.version.clone(),
            headers: parsed.headers.clone(),
            body,
        }
    }

    /// Build a URL from the parsed request.
    fn build_url(parsed: &ParsedRequest, is_https: bool) -> Url {
        // For CONNECT requests, the target is the authority (host:port)
        if parsed.is_connect() {
            let authority = &parsed.target;
            // Use https scheme since CONNECT is for tunneling TLS
            Url::parse(&format!("https://{authority}")).unwrap_or_else(|_| {
                Url::parse("https://unknown").expect("fallback URL should be valid")
            })
        } else if parsed.target.starts_with("http://") || parsed.target.starts_with("https://") {
            // Absolute URI form (used by proxies)
            Url::parse(&parsed.target).unwrap_or_else(|_| {
                Url::parse("http://unknown").expect("fallback URL should be valid")
            })
        } else {
            // Origin-form: just the path, need Host header
            let host = parsed.host().unwrap_or("unknown");
            let scheme = if is_https { "https" } else { "http" };
            let target = &parsed.target;
            Url::parse(&format!("{scheme}://{host}{target}")).unwrap_or_else(|_| {
                Url::parse("http://unknown").expect("fallback URL should be valid")
            })
        }
    }

    /// Get a header value by name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Get the Host header.
    pub fn host(&self) -> Option<&str> {
        self.header("host")
    }
}

/// A complete HTTP response with metadata.
#[derive(Debug, Clone)]
pub struct Response {
    /// The request ID this response corresponds to
    pub request_id: u64,

    /// HTTP status code
    pub status: u16,

    /// HTTP reason phrase
    pub reason: String,

    /// HTTP version
    pub version: String,

    /// Response headers
    pub headers: Vec<(String, String)>,

    /// Raw response body (may be compressed)
    pub body: Bytes,

    /// Decompressed response body (if applicable)
    pub body_decompressed: Bytes,

    /// Content encoding of the original body
    pub encoding: Encoding,

    /// Resource type inferred from Content-Type header
    pub resource_type: ResourceType,

    /// Time taken for the request-response cycle in milliseconds
    pub duration_ms: u64,

    /// Detailed timing breakdown
    pub timings: Option<Timings>,

    /// Remote server address (IP:port)
    pub remote_addr: Option<SocketAddr>,
}

impl Response {
    /// Create a new Response from a parsed response.
    pub fn from_parsed(parsed: &ParsedResponse, request_id: u64, duration_ms: u64) -> Self {
        Self::from_parsed_with_timings(parsed, request_id, duration_ms, None, None)
    }

    /// Create a new Response from a parsed response with timing information.
    pub fn from_parsed_with_timings(
        parsed: &ParsedResponse,
        request_id: u64,
        duration_ms: u64,
        timings: Option<Timings>,
        remote_addr: Option<SocketAddr>,
    ) -> Self {
        let encoding = parsed
            .header("content-encoding")
            .map_or(Encoding::Identity, Encoding::from_header);

        let body_decompressed = if encoding.is_compressed() {
            compression::decompress(&parsed.body, encoding)
        } else {
            parsed.body.clone()
        };

        let resource_type = parsed
            .header("content-type")
            .map_or(ResourceType::Other, ResourceType::from_mime_type);

        Self {
            request_id,
            status: parsed.status,
            reason: parsed.reason.clone(),
            version: parsed.version.clone(),
            headers: parsed.headers.clone(),
            body: parsed.body.clone(),
            body_decompressed,
            encoding,
            resource_type,
            duration_ms,
            timings,
            remote_addr,
        }
    }

    /// Get a header value by name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Get the Content-Type header.
    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    /// Check if the response body is compressed.
    pub fn is_compressed(&self) -> bool {
        self.encoding.is_compressed()
    }

    /// Get the body as text, decompressing if necessary.
    ///
    /// Returns `None` if the body is not valid UTF-8.
    pub fn text(&self) -> Option<&str> {
        std::str::from_utf8(&self.body_decompressed).ok()
    }
}

/// A complete request-response exchange.
#[derive(Debug, Clone)]
pub struct Exchange {
    /// The request
    pub request: Request,

    /// The response (None if request failed before response)
    pub response: Option<Response>,

    /// Error message if the exchange failed
    pub error: Option<String>,
}

/// Timing breakdown for a request-response cycle.
///
/// All times are in milliseconds. Fields are `None` when the timing
/// doesn't apply (e.g., `connect_ms` is `None` when connection was reused).
#[derive(Debug, Clone, Default)]
pub struct Timings {
    /// Time spent waiting for a connection from the pool (usually 0).
    /// We don't queue requests, so this is typically not applicable.
    pub blocked_ms: Option<f64>,

    /// Time to establish TCP connection.
    /// `None` if the connection was reused from the pool.
    pub connect_ms: Option<f64>,

    /// Time to send the request (headers + body).
    pub send_ms: f64,

    /// Time waiting for first byte of response (TTFB).
    pub wait_ms: f64,

    /// Time to receive the complete response body.
    pub receive_ms: f64,

    /// Time for SSL/TLS handshake.
    /// `None` for plain HTTP or when not applicable.
    pub ssl_ms: Option<f64>,

    /// Whether the connection was reused from the pool.
    pub connection_reused: bool,
}

// ============================================================================
// Callback type aliases
// ============================================================================

/// Async callback for Exchange (complete request-response pair).
pub type ExchangeCallbackAsync =
    Arc<dyn Fn(Exchange) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Sync callback for Exchange.
pub type ExchangeCallbackSync = Arc<dyn Fn(Exchange) + Send + Sync>;

/// Async callback for Request only.
pub type RequestCallbackAsync =
    Arc<dyn Fn(Request) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Sync callback for Request only.
pub type RequestCallbackSync = Arc<dyn Fn(Request) + Send + Sync>;

/// Async callback for Response only.
pub type ResponseCallbackAsync =
    Arc<dyn Fn(Response) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Sync callback for Response only.
pub type ResponseCallbackSync = Arc<dyn Fn(Response) + Send + Sync>;

// ============================================================================
// Callback handlers (enum wrapping sync/async variants)
// ============================================================================

/// Callback handler for Exchange that can be either sync or async.
#[derive(Clone)]
pub enum ExchangeHandler {
    /// Runs to completion before the proxy continues.
    Sync(ExchangeCallbackSync),

    /// Awaited before the proxy continues.
    Async(ExchangeCallbackAsync),
}

impl fmt::Debug for ExchangeHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sync(_) => write!(f, "ExchangeHandler::Sync(...)"),
            Self::Async(_) => write!(f, "ExchangeHandler::Async(...)"),
        }
    }
}

impl ExchangeHandler {
    /// Call the callback with `exchange`.
    pub async fn invoke(&self, exchange: Exchange) {
        match self {
            Self::Sync(f) => f(exchange),
            Self::Async(f) => f(exchange).await,
        }
    }
}

/// Callback handler for Request that can be either sync or async.
#[derive(Clone)]
pub enum RequestHandler {
    /// Runs to completion before the proxy continues.
    Sync(RequestCallbackSync),

    /// Awaited before the proxy continues.
    Async(RequestCallbackAsync),
}

impl fmt::Debug for RequestHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sync(_) => write!(f, "RequestHandler::Sync(...)"),
            Self::Async(_) => write!(f, "RequestHandler::Async(...)"),
        }
    }
}

impl RequestHandler {
    /// Call the callback with `request`.
    pub async fn invoke(&self, request: Request) {
        match self {
            Self::Sync(f) => f(request),
            Self::Async(f) => f(request).await,
        }
    }
}

/// Callback handler for Response that can be either sync or async.
#[derive(Clone)]
pub enum ResponseHandler {
    /// Runs to completion before the proxy continues.
    Sync(ResponseCallbackSync),

    /// Awaited before the proxy continues.
    Async(ResponseCallbackAsync),
}

impl fmt::Debug for ResponseHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sync(_) => write!(f, "ResponseHandler::Sync(...)"),
            Self::Async(_) => write!(f, "ResponseHandler::Async(...)"),
        }
    }
}

impl ResponseHandler {
    /// Call the callback with `response`.
    pub async fn invoke(&self, response: Response) {
        match self {
            Self::Sync(f) => f(response),
            Self::Async(f) => f(response).await,
        }
    }
}

// ============================================================================
// Callbacks container
// ============================================================================

/// Callbacks that can be registered with the proxy.
#[derive(Default, Clone)]
pub struct Callbacks {
    /// Called when a request is received (before forwarding upstream).
    pub on_request: Option<RequestHandler>,

    /// Called when a response is received (before sending to client).
    pub on_response: Option<ResponseHandler>,

    /// Called when a complete exchange (request + response) is finished.
    pub on_exchange: Option<ExchangeHandler>,
}

impl fmt::Debug for Callbacks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Callbacks")
            .field("on_request", &self.on_request.is_some())
            .field("on_response", &self.on_response.is_some())
            .field("on_exchange", &self.on_exchange.is_some())
            .finish()
    }
}
