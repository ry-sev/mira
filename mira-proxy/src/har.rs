//! HAR 1.2 (HTTP Archive) format support.
//!
//! This module provides types and utilities for exporting and importing HTTP
//! traffic using the HAR 1.2 format. HAR is a JSON-based format for archiving
//! HTTP request/response data.
//!
//! Mira extends the standard HAR format with custom `_mira*` fields to preserve
//! additional metadata for full-fidelity round-trips. These extensions are
//! ignored by other tools and are optional when importing.
//!
//! See: <http://www.softwareishard.com/blog/har-12-spec/>

use crate::Result;
use crate::http::Encoding;
use crate::http::Exchange;
use crate::http::Request;
use crate::http::ResourceType;
use crate::http::Response;
use crate::http::Timings;
use base64::Engine;
use bytes::Bytes;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use std::net::SocketAddr;
use std::time::SystemTime;
use url::Url;

// ============================================================================
// HAR 1.2 Data Structures
// ============================================================================

/// Root HAR object containing the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Har {
    /// The log object containing all data.
    pub log: HarLog,
}

impl Har {
    /// Create a new HAR with the given log.
    pub fn new(log: HarLog) -> Self {
        Self { log }
    }

    /// Serialize to JSON string.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// Serialize to pretty-printed JSON string.
    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

/// The root of exported data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarLog {
    /// Version number of the format (always "1.2").
    pub version: String,

    /// Name and version info of the log creator application.
    pub creator: HarCreator,

    /// Name and version info of used browser (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser: Option<HarCreator>,

    /// List of all exported pages (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<Vec<HarPage>>,

    /// List of all exported requests.
    pub entries: Vec<HarEntry>,

    /// A comment provided by the user or application (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

impl HarLog {
    /// Create a new HAR log with default creator info.
    pub fn new() -> Self {
        Self {
            version: "1.2".into(),
            creator: HarCreator::mira(),
            browser: None,
            pages: None,
            entries: Vec::new(),
            comment: None,
        }
    }

    /// Add an entry to the log.
    pub fn add_entry(&mut self, entry: HarEntry) {
        self.entries.push(entry);
    }

    /// Create a HAR entry from an Exchange.
    pub fn entry_from_exchange(exchange: &Exchange, started: DateTime<Utc>) -> HarEntry {
        HarEntry::from_exchange(exchange, started)
    }
}

impl Default for HarLog {
    fn default() -> Self {
        Self::new()
    }
}

/// Creator or browser information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarCreator {
    /// Name of the application/browser.
    pub name: String,

    /// Version of the application/browser.
    pub version: String,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

impl HarCreator {
    /// Create creator info for Mira.
    pub fn mira() -> Self {
        Self {
            name: "Mira".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            comment: None,
        }
    }
}

/// Represents an exported page.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarPage {
    /// Start time of the page load (ISO 8601).
    pub started_date_time: String,

    /// Unique identifier of the page.
    pub id: String,

    /// Page title.
    pub title: String,

    /// Detailed timing info about page load.
    pub page_timings: HarPageTimings,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Page load timing information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarPageTimings {
    /// Content of the page loaded (ms since page load, -1 if N/A).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_content_load: Option<f64>,

    /// Page is loaded - onLoad event fired (ms since page load, -1 if N/A).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_load: Option<f64>,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Represents a single HTTP request/response exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarEntry {
    /// Reference to parent page (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pageref: Option<String>,

    /// Start time of the request (ISO 8601).
    pub started_date_time: String,

    /// Total elapsed time of the request in milliseconds.
    pub time: f64,

    /// Detailed info about the request.
    pub request: HarRequest,

    /// Detailed info about the response.
    pub response: HarResponse,

    /// Info about cache usage.
    pub cache: HarCache,

    /// Detailed timing info about request/response round trip.
    pub timings: HarTimings,

    /// IP address of the server (optional).
    #[serde(rename = "serverIPAddress")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_ip_address: Option<String>,

    /// Unique ID of the TCP/IP connection (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

impl HarEntry {
    /// Create a HAR entry from an Exchange.
    pub fn from_exchange(exchange: &Exchange, started: DateTime<Utc>) -> Self {
        let request = HarRequest::from_request(&exchange.request);

        let (response, time, timings) = if let Some(resp) = &exchange.response {
            let har_timings = Self::build_timings(resp);
            (
                HarResponse::from_response(resp),
                resp.duration_ms as f64,
                har_timings,
            )
        } else {
            (HarResponse::error(), 0.0, HarTimings::unavailable())
        };

        // `serverIPAddress` is the server's IP per HAR spec (not host:port).
        // `connection` is the TCP/IP connection identifier — siblings on a
        // persistent connection MUST share it. The upstream peer address is
        // a stable-enough key for grouping without threading a per-socket
        // connection counter through the handler plumbing.
        let remote_addr = exchange.response.as_ref().and_then(|r| r.remote_addr);
        let server_ip_address = remote_addr.map(|addr| addr.ip().to_string());
        let connection = remote_addr.map(|addr| addr.to_string());

        Self {
            pageref: None,
            started_date_time: started.to_rfc3339(),
            time,
            request,
            response,
            cache: HarCache::default(),
            timings,
            server_ip_address,
            connection,
            comment: exchange.error.clone(),
        }
    }

    /// Build HAR timings from Response timings.
    fn build_timings(response: &Response) -> HarTimings {
        if let Some(timings) = &response.timings {
            HarTimings {
                // blocked is 0 since we don't queue requests
                blocked: timings.blocked_ms.map(|v| if v == 0.0 { 0.0 } else { v }),

                // DNS is not tracked (would need async DNS resolver)
                dns: Some(-1.0),

                // connect is None if connection was reused (-1 in HAR)
                connect: timings.connect_ms.or(Some(-1.0)),

                send: timings.send_ms,

                wait: timings.wait_ms,

                receive: timings.receive_ms,

                // Plain HTTP has no TLS
                ssl: timings.ssl_ms.or(Some(-1.0)),

                comment: None,
            }
        } else {
            HarTimings::unavailable()
        }
    }

    /// Convert a HAR entry back to a Mira Exchange.
    ///
    /// Uses Mira extension fields if present, otherwise falls back to
    /// inferring values from standard HAR fields or using defaults.
    ///
    /// # Arguments
    /// * `id_counter` - Counter to generate unique IDs if `_miraId` is not
    ///   present
    pub fn to_exchange(&self, id_counter: u64) -> Result<Exchange> {
        let request = self.request.to_request(id_counter)?;
        let request_id = request.id;

        let response = if self.response.is_error() {
            None
        } else {
            Some(self.response.to_response(request_id, &self.timings))
        };

        // Error is stored in the comment field
        let error = self.comment.clone();

        Ok(Exchange {
            request,
            response,
            error,
        })
    }

    /// Parse the `started_date_time` field into a `DateTime`.
    pub fn started_time(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&self.started_date_time)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }
}

/// Detailed info about a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarRequest {
    /// Request method (GET, POST, etc.).
    pub method: String,

    /// Absolute URL of the request.
    pub url: String,

    /// Request HTTP version.
    pub http_version: String,

    /// List of cookie objects.
    pub cookies: Vec<HarCookie>,

    /// List of header objects.
    pub headers: Vec<HarHeader>,

    /// List of query parameter objects.
    pub query_string: Vec<HarQueryParam>,

    /// Posted data info (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_data: Option<HarPostData>,

    /// Total number of bytes from start of request until double CRLF.
    pub headers_size: i64,

    /// Size of the request body in bytes (-1 if not available).
    pub body_size: i64,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,

    // -------------------------------------------------------------------------
    // Mira extension fields (prefixed with _mira per HAR spec for custom fields)
    // -------------------------------------------------------------------------
    /// Mira internal request ID for correlation.
    #[serde(rename = "_miraId", skip_serializing_if = "Option::is_none")]
    pub mira_id: Option<u64>,

    /// Client socket address that made the request.
    #[serde(rename = "_miraClientAddr", skip_serializing_if = "Option::is_none")]
    pub mira_client_addr: Option<String>,
}

impl HarRequest {
    /// Create a HAR request from a Request.
    pub fn from_request(request: &Request) -> Self {
        let cookies = parse_cookies_from_header(request.header("cookie"));
        let headers: Vec<HarHeader> = request
            .headers
            .iter()
            .map(|(k, v)| HarHeader {
                name: k.clone(),

                value: v.clone(),

                comment: None,
            })
            .collect();

        let query_string = parse_query_string(request.url.query());

        let post_data = if !request.body.is_empty() {
            let mime_type = request
                .header("content-type")
                .unwrap_or("application/octet-stream");

            // Serialize the body as UTF-8 text when it round-trips cleanly,
            // otherwise base64-encode it and flag the encoding explicitly —
            // consumers key off the `encoding` field rather than guessing
            // whether a UTF-8 string is "accidentally" base64.
            let (text, encoding) = match std::str::from_utf8(&request.body) {
                Ok(s) => (s.to_owned(), None),
                Err(_) => (
                    base64::engine::general_purpose::STANDARD.encode(&request.body),
                    Some("base64".to_owned()),
                ),
            };

            Some(HarPostData {
                mime_type: mime_type.into(),
                params: Vec::new(),
                text,
                encoding,
                comment: None,
            })
        } else {
            None
        };

        // Calculate header size (approximate)
        let headers_size = calculate_request_headers_size(request);
        let body_size = request.body.len() as i64;

        Self {
            method: request.method.clone(),
            url: request.url.to_string(),
            http_version: request.version.clone(),
            cookies,
            headers,
            query_string,
            post_data,
            headers_size,
            body_size,
            comment: None,
            // Mira extensions
            mira_id: Some(request.id),
            mira_client_addr: Some(request.client_addr.to_string()),
        }
    }

    /// Convert a HAR request back to a Mira Request.
    ///
    /// Uses Mira extension fields if present, otherwise falls back to
    /// inferring values from standard HAR fields or using defaults.
    ///
    /// # Arguments
    /// * `id_counter` - Counter to generate unique IDs if `_miraId` is not
    ///   present
    pub fn to_request(&self, id_counter: u64) -> Result<Request> {
        // Use Mira extension or generate new ID
        let id = self.mira_id.unwrap_or(id_counter);

        // Use Mira extension or default to 0.0.0.0:0
        let client_addr: SocketAddr = self
            .mira_client_addr
            .as_ref()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));

        let url = Url::parse(&self.url).map_err(|e| {
            crate::Error::InvalidRequest(format!("invalid URL '{}': {}", self.url, e))
        })?;

        let headers: Vec<(String, String)> = self
            .headers
            .iter()
            .map(|h| (h.name.clone(), h.value.clone()))
            .collect();

        // Extract body from post_data. Trust `encoding` when it's set —
        // don't pattern-match the text for base64-like characters, which
        // falsely fires on plaintext tokens that happen to use only the
        // base64 alphabet.
        let body = if let Some(post_data) = &self.post_data {
            if post_data.encoding.as_deref() == Some("base64") {
                base64::engine::general_purpose::STANDARD
                    .decode(&post_data.text)
                    .map_or_else(|_| Bytes::from(post_data.text.clone()), Bytes::from)
            } else {
                Bytes::from(post_data.text.clone())
            }
        } else {
            Bytes::new()
        };

        Ok(Request {
            id,
            client_addr,
            method: self.method.clone(),
            url,
            version: self.http_version.clone(),
            headers,
            body,
        })
    }
}

/// Detailed info about a response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarResponse {
    /// Response status code.
    pub status: u16,

    /// Response status description.
    pub status_text: String,

    /// Response HTTP version.
    pub http_version: String,

    /// List of cookie objects.
    pub cookies: Vec<HarCookie>,

    /// List of header objects.
    pub headers: Vec<HarHeader>,

    /// Details about the response body.
    pub content: HarContent,

    /// Redirection target URL from Location header.
    #[serde(rename = "redirectURL")]
    pub redirect_url: String,

    /// Total number of bytes from start of response until double CRLF.
    pub headers_size: i64,

    /// Size of the received response body in bytes.
    pub body_size: i64,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,

    // -------------------------------------------------------------------------
    // Mira extension fields (prefixed with _mira per HAR spec for custom fields)
    // -------------------------------------------------------------------------
    /// Content encoding of the original body (gzip, deflate, br, identity).
    #[serde(rename = "_miraEncoding", skip_serializing_if = "Option::is_none")]
    pub mira_encoding: Option<Encoding>,

    /// Resource type inferred from Content-Type.
    #[serde(rename = "_miraResourceType", skip_serializing_if = "Option::is_none")]
    pub mira_resource_type: Option<ResourceType>,

    /// Remote server address (IP:port).
    #[serde(rename = "_miraRemoteAddr", skip_serializing_if = "Option::is_none")]
    pub mira_remote_addr: Option<String>,

    /// Whether the connection was reused from the pool.
    #[serde(
        rename = "_miraConnectionReused",
        skip_serializing_if = "Option::is_none"
    )]
    pub mira_connection_reused: Option<bool>,
}

impl HarResponse {
    /// Create a HAR response from a Response.
    pub fn from_response(response: &Response) -> Self {
        let cookies = parse_cookies_from_set_cookie(&response.headers);
        let headers: Vec<HarHeader> = response
            .headers
            .iter()
            .map(|(k, v)| HarHeader {
                name: k.clone(),
                value: v.clone(),
                comment: None,
            })
            .collect();

        let redirect_url = response.header("location").unwrap_or("").into();

        let content = HarContent::from_response(response);
        let headers_size = calculate_response_headers_size(response);
        let body_size = response.body.len() as i64;

        Self {
            status: response.status,
            status_text: response.reason.clone(),
            http_version: response.version.clone(),
            cookies,
            headers,
            content,
            redirect_url,
            headers_size,
            body_size,
            comment: None,
            // Mira extensions
            mira_encoding: Some(response.encoding),
            mira_resource_type: Some(response.resource_type),
            mira_remote_addr: response.remote_addr.map(|addr| addr.to_string()),
            mira_connection_reused: response.timings.as_ref().map(|t| t.connection_reused),
        }
    }

    /// Create an error response (when request failed).
    pub fn error() -> Self {
        Self {
            status: 0,
            status_text: String::new(),
            http_version: String::new(),
            cookies: Vec::new(),
            headers: Vec::new(),
            content: HarContent::empty(),
            redirect_url: String::new(),
            headers_size: -1,
            body_size: -1,
            comment: None,
            // Mira extensions (None for error responses)
            mira_encoding: None,
            mira_resource_type: None,
            mira_remote_addr: None,
            mira_connection_reused: None,
        }
    }

    /// Check if this response represents an error (status 0).
    pub fn is_error(&self) -> bool {
        self.status == 0
    }

    /// Convert a HAR response back to a Mira Response.
    ///
    /// Uses Mira extension fields if present, otherwise falls back to
    /// inferring values from standard HAR fields or using defaults.
    ///
    /// # Arguments
    /// * `request_id` - The ID of the request this response corresponds to
    /// * `har_timings` - The HAR timings for this entry
    pub fn to_response(&self, request_id: u64, har_timings: &HarTimings) -> Response {
        let headers: Vec<(String, String)> = self
            .headers
            .iter()
            .map(|h| (h.name.clone(), h.value.clone()))
            .collect();

        // Get encoding from Mira extension or infer from Content-Encoding header
        let encoding = self.mira_encoding.unwrap_or_else(|| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-encoding"))
                .map_or(Encoding::Identity, |(_, v)| Encoding::from_header(v))
        });

        // Get resource type from Mira extension or infer from Content-Type header
        let resource_type = self.mira_resource_type.unwrap_or_else(|| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                .map_or(ResourceType::Other, |(_, v)| {
                    ResourceType::from_mime_type(v)
                })
        });

        let remote_addr: Option<SocketAddr> =
            self.mira_remote_addr.as_ref().and_then(|s| s.parse().ok());

        let body_decompressed = if let Some(text) = &self.content.text {
            if self.content.encoding.as_deref() == Some("base64") {
                base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .map_or_else(|_| Bytes::from(text.clone()), Bytes::from)
            } else {
                Bytes::from(text.clone())
            }
        } else {
            Bytes::new()
        };

        // For HAR import, we store the decompressed body as both body and
        // body_decompressed since HAR always stores decompressed content. If we
        // need to re-compress, we'd need the original compressed bytes which
        // HAR doesn't preserve.
        let body = body_decompressed.clone();

        let connection_reused = self
            .mira_connection_reused
            // Infer: if connect is -1, connection was likely reused
            .unwrap_or_else(|| har_timings.connect.is_some_and(|c| c < 0.0));

        let timings = Timings {
            blocked_ms: har_timings.blocked.filter(|&v| v >= 0.0),
            connect_ms: har_timings.connect.filter(|&v| v >= 0.0),
            send_ms: har_timings.send,
            wait_ms: har_timings.wait,
            receive_ms: har_timings.receive,
            ssl_ms: har_timings.ssl.filter(|&v| v >= 0.0),
            connection_reused,
        };

        // Calculate duration from timings. Per HAR 1.2 `time` is the
        // sum of *all* timings (excluding -1 values), not just the
        // send/wait/receive triple — dropping connect_ms and ssl_ms
        // understates the duration on first-request HAR entries.
        let duration_ms = har_timings.total_time() as u64;

        Response {
            request_id,
            status: self.status,
            reason: self.status_text.clone(),
            version: self.http_version.clone(),
            headers,
            body,
            body_decompressed,
            encoding,
            resource_type,
            duration_ms,
            timings: Some(timings),
            remote_addr,
        }
    }
}

/// Cookie information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarCookie {
    /// Cookie name.
    pub name: String,

    /// Cookie value.
    pub value: String,

    /// Cookie path (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Cookie domain (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,

    /// Cookie expiration time (optional, ISO 8601).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,

    /// HTTP only flag (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_only: Option<bool>,

    /// Secure flag (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Header name-value pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarHeader {
    /// Header name.
    pub name: String,

    /// Header value.
    pub value: String,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Query string parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarQueryParam {
    /// Parameter name.
    pub name: String,

    /// Parameter value.
    pub value: String,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Posted data information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarPostData {
    /// MIME type of posted data.
    pub mime_type: String,

    /// List of posted parameters (for URL-encoded).
    #[serde(default)]
    pub params: Vec<HarParam>,

    /// Plain text posted data.
    pub text: String,

    /// Encoding used for the `text` field when the body is binary
    /// (currently only `"base64"`).
    ///
    /// This mirrors the `encoding` field on [`HarContent`]; HAR 1.2 doesn't
    /// define it on `postData` but several tools interpret it the same
    /// way. Setting this explicitly avoids having to guess whether a
    /// UTF-8 `text` value is secretly base64 on the import path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Posted parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarParam {
    /// Parameter name.
    pub name: String,

    /// Parameter value (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,

    /// File name (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,

    /// Content type of posted file (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Response body content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarContent {
    /// Length of the returned content in bytes.
    pub size: i64,

    /// Number of bytes saved by compression (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression: Option<i64>,

    /// MIME type of the response.
    pub mime_type: String,

    /// Response body text (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,

    /// Encoding used for text field (e.g., "base64") (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

impl HarContent {
    /// Create content from a Response.
    pub fn from_response(response: &Response) -> Self {
        let mime_type = response
            .content_type()
            .unwrap_or("application/octet-stream")
            .into();

        // Use decompressed body for HAR content
        let decompressed = &response.body_decompressed;
        let size = decompressed.len() as i64;

        let compression = if response.is_compressed() {
            Some(size - response.body.len() as i64)
        } else {
            None
        };

        let (text, encoding) = if let Some(text_content) = response.text() {
            (Some(text_content.to_owned()), None)
        } else if !decompressed.is_empty() {
            let encoded = base64::engine::general_purpose::STANDARD.encode(decompressed);
            (Some(encoded), Some("base64".into()))
        } else {
            (None, None)
        };

        Self {
            size,
            compression,
            mime_type,
            text,
            encoding,
            comment: None,
        }
    }

    /// Create empty content (for error responses).
    pub fn empty() -> Self {
        Self {
            size: 0,
            compression: None,
            mime_type: String::new(),
            text: None,
            encoding: None,
            comment: None,
        }
    }
}

/// Cache information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarCache {
    /// State of cache entry before request (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_request: Option<HarCacheEntry>,

    /// State of cache entry after request (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_request: Option<HarCacheEntry>,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Cache entry state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarCacheEntry {
    /// Expiration time (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,

    /// Last access time.
    pub last_access: String,

    /// `ETag`.
    #[serde(rename = "eTag")]
    pub e_tag: String,

    /// Number of times cache entry was opened.
    pub hit_count: i64,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Request/response timing information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarTimings {
    /// Time spent waiting for network connection (-1 if N/A).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<f64>,

    /// DNS resolution time (-1 if N/A).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dns: Option<f64>,

    /// Time to create TCP connection (-1 if N/A).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect: Option<f64>,

    /// Time to send HTTP request.
    pub send: f64,

    /// Waiting for response from server.
    pub wait: f64,

    /// Time to read entire response.
    pub receive: f64,

    /// SSL/TLS negotiation time (-1 if N/A).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssl: Option<f64>,

    /// Optional comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

impl HarTimings {
    /// Total elapsed time in milliseconds, defined by the HAR 1.2 spec
    /// as the sum of every timing component *except* those flagged as
    /// not-applicable (`None` or a negative sentinel like `-1`).
    ///
    /// `send`, `wait`, and `receive` are always counted; `blocked`,
    /// `dns`, `connect`, and `ssl` contribute only when non-negative.
    pub fn total_time(&self) -> f64 {
        let mut total = self.send + self.wait + self.receive;
        for field in [self.blocked, self.dns, self.connect, self.ssl] {
            if let Some(v) = field
                && v >= 0.0
            {
                total += v;
            }
        }
        total
    }

    /// Create timings with all values unavailable (-1 or 0).
    pub fn unavailable() -> Self {
        Self {
            blocked: Some(-1.0),
            dns: Some(-1.0),
            connect: Some(-1.0),
            send: 0.0,
            wait: 0.0,
            receive: 0.0,
            ssl: Some(-1.0),
            comment: None,
        }
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Parse cookies from Cookie header value.
fn parse_cookies_from_header(header: Option<&str>) -> Vec<HarCookie> {
    let Some(header) = header else {
        return Vec::new();
    };

    header
        .split(';')
        .filter_map(|cookie| {
            let cookie = cookie.trim();
            let (name, value) = cookie.split_once('=')?;
            Some(HarCookie {
                name: name.trim().into(),
                value: value.trim().into(),
                path: None,
                domain: None,
                expires: None,
                http_only: None,
                secure: None,
                comment: None,
            })
        })
        .collect()
}

/// Parse cookies from Set-Cookie headers.
fn parse_cookies_from_set_cookie(headers: &[(String, String)]) -> Vec<HarCookie> {
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
        .filter_map(|(_, v)| {
            // Parse Set-Cookie header: name=value; Path=/; Domain=.example.com; ...
            let mut parts = v.split(';');
            let name_value = parts.next()?.trim();
            let (name, value) = name_value.split_once('=')?;

            let mut cookie = HarCookie {
                name: name.trim().into(),
                value: value.trim().into(),
                path: None,
                domain: None,
                expires: None,
                http_only: None,
                secure: None,
                comment: None,
            };

            for part in parts {
                let part = part.trim();
                if let Some((attr, val)) = part.split_once('=') {
                    let attr_lower = attr.to_lowercase();
                    match attr_lower.as_str() {
                        "path" => cookie.path = Some(val.into()),
                        "domain" => cookie.domain = Some(val.into()),
                        "expires" => cookie.expires = Some(val.into()),

                        _ => {}
                    }
                } else {
                    let part_lower = part.to_lowercase();
                    if part_lower == "httponly" {
                        cookie.http_only = Some(true);
                    } else if part_lower == "secure" {
                        cookie.secure = Some(true);
                    }
                }
            }

            Some(cookie)
        })
        .collect()
}

/// Parse query string into parameters.
fn parse_query_string(query: Option<&str>) -> Vec<HarQueryParam> {
    let Some(query) = query else {
        return Vec::new();
    };

    query
        .split('&')
        .filter_map(|param| {
            if param.is_empty() {
                return None;
            }
            let (name, value) = param.split_once('=').unwrap_or((param, ""));
            Some(HarQueryParam {
                name: url_decode(name),
                value: url_decode(value),
                comment: None,
            })
        })
        .collect()
}

/// URL decode a string.
///
/// Percent-escapes are decoded to bytes and the result read as UTF-8, so
/// a multi-byte character such as `%C3%A9` comes out as `é`. Invalid
/// UTF-8 is replaced rather than dropped, and a `%` not followed by two
/// hex digits is kept as written.
fn url_decode(s: &str) -> String {
    let input = s.as_bytes();
    let mut bytes = Vec::with_capacity(input.len());
    let mut i = 0;
    while let Some(&byte) = input.get(i) {
        match byte {
            b'%' => {
                let escaped = input
                    .get(i + 1..i + 3)
                    .and_then(|hex| std::str::from_utf8(hex).ok())
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                if let Some(decoded) = escaped {
                    bytes.push(decoded);
                    i += 3;
                    continue;
                }
                bytes.push(b'%');
            }
            b'+' => bytes.push(b' '),
            other => bytes.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Whether a recorded version string names HTTP/2.
fn is_http2(version: &str) -> bool {
    version.eq_ignore_ascii_case("HTTP/2") || version.eq_ignore_ascii_case("HTTP/2.0")
}

/// Calculate approximate request headers size.
fn calculate_request_headers_size(request: &Request) -> i64 {
    // HTTP/2 sends headers as HPACK, so there is no line-oriented size
    // to report. HAR's answer for "not applicable" is -1, which is
    // honest where a plausible-looking guess would not be.
    if is_http2(&request.version) {
        return -1;
    }

    // Request line: METHOD PATH HTTP/VERSION\r\n
    let request_line = format!(
        "{} {} {}\r\n",
        request.method,
        request.url.path(),
        request.version
    );

    let headers_size: usize = request
        .headers
        .iter()
        .map(|(k, v)| k.len() + 2 + v.len() + 2) // "name: value\r\n"
        .sum();

    (request_line.len() + headers_size + 2) as i64 // +2 for final \r\n
}

/// Calculate approximate response headers size.
fn calculate_response_headers_size(response: &Response) -> i64 {
    // See `calculate_request_headers_size`: HPACK has no line-oriented
    // header size.
    if is_http2(&response.version) {
        return -1;
    }

    // Status line: HTTP/VERSION STATUS REASON\r\n
    let status_line = format!(
        "{} {} {}\r\n",
        response.version, response.status, response.reason
    );

    let headers_size: usize = response
        .headers
        .iter()
        .map(|(k, v)| k.len() + 2 + v.len() + 2) // "name: value\r\n"
        .sum();

    (status_line.len() + headers_size + 2) as i64 // +2 for final \r\n
}

/// Get current time as ISO 8601 string.
pub fn now_iso8601() -> String {
    Utc::now().to_rfc3339()
}

/// Convert `SystemTime` to ISO 8601 string.
pub fn system_time_to_iso8601(time: SystemTime) -> String {
    let datetime: DateTime<Utc> = time.into();
    datetime.to_rfc3339()
}

// ============================================================================
// HAR Builder
// ============================================================================

/// Builder for collecting exchanges and generating HAR output.
#[derive(Debug, Default)]
pub struct HarBuilder {
    entries: Vec<(Exchange, DateTime<Utc>)>,
    comment: Option<String>,
}

impl HarBuilder {
    /// Create a new HAR builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an exchange to the HAR.
    pub fn add_exchange(&mut self, exchange: Exchange) {
        self.entries.push((exchange, Utc::now()));
    }

    /// Add an exchange with a specific timestamp.
    pub fn add_exchange_at(&mut self, exchange: Exchange, started: DateTime<Utc>) {
        self.entries.push((exchange, started));
    }

    /// Set the HAR comment.
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    /// Build the HAR.
    pub fn build(self) -> Har {
        let mut log = HarLog::new();
        log.comment = self.comment;

        for (exchange, started) in self.entries {
            let entry = HarEntry::from_exchange(&exchange, started);
            log.add_entry(entry);
        }

        Har::new(log)
    }

    /// Build and serialize to JSON.
    pub fn to_json(self) -> Result<String> {
        self.build().to_json()
    }

    /// Build and serialize to pretty JSON.
    pub fn to_json_pretty(self) -> Result<String> {
        self.build().to_json_pretty()
    }

    /// Get the number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the builder is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Timings;
    use bytes::Bytes;

    fn sample_request() -> Request {
        Request {
            id: 1,
            client_addr: "127.0.0.1:12345".parse().expect("valid addr"),
            method: "GET".into(),
            url: "http://example.com/path?foo=bar&baz=qux"
                .parse()
                .expect("valid url"),
            version: "HTTP/1.1".into(),
            headers: vec![
                ("Host".into(), "example.com".into()),
                ("User-Agent".into(), "test-agent".into()),
                ("Cookie".into(), "session=abc123; user=john".into()),
            ],
            body: Bytes::new(),
        }
    }

    fn sample_response() -> Response {
        use crate::http::Encoding;

        Response {
            request_id: 1,
            status: 200,
            reason: "OK".into(),
            version: "HTTP/1.1".into(),
            headers: vec![
                ("Content-Type".into(), "text/html; charset=utf-8".into()),
                ("Content-Length".into(), "13".into()),
                (
                    "Set-Cookie".into(),
                    "token=xyz; Path=/; HttpOnly; Secure".into(),
                ),
            ],
            body: Bytes::from_static(b"Hello, World!"),
            body_decompressed: Bytes::from_static(b"Hello, World!"),
            encoding: Encoding::Identity,
            resource_type: ResourceType::Document,
            duration_ms: 150,
            timings: Some(Timings {
                blocked_ms: Some(0.0),
                connect_ms: Some(10.0),
                send_ms: 5.0,
                wait_ms: 100.0,
                receive_ms: 35.0,
                ssl_ms: None,
                connection_reused: false,
            }),
            remote_addr: None,
        }
    }

    #[test]
    fn test_http2_header_sizes_are_reported_as_not_applicable() {
        // HPACK has no line-oriented header size, and HAR spells "not
        // applicable" as -1. Reporting a plausible HTTP/1.1-shaped guess
        // instead would be a fabricated number.
        let mut request = sample_request();
        request.version = "HTTP/2".into();
        let mut response = sample_response();
        response.version = "HTTP/2".into();

        let har_request = HarRequest::from_request(&request);
        let har_response = HarResponse::from_response(&response);

        assert_eq!(har_request.headers_size, -1);
        assert_eq!(har_response.headers_size, -1);
        assert_eq!(har_request.http_version, "HTTP/2");
    }

    #[test]
    fn test_har_request_from_request() {
        let request = sample_request();
        let har_request = HarRequest::from_request(&request);

        assert_eq!(har_request.method, "GET");
        assert_eq!(har_request.url, "http://example.com/path?foo=bar&baz=qux");
        assert_eq!(har_request.http_version, "HTTP/1.1");
        assert_eq!(har_request.headers.len(), 3);
        assert_eq!(har_request.cookies.len(), 2);
        assert_eq!(har_request.query_string.len(), 2);
        assert_eq!(har_request.query_string[0].name, "foo");
        assert_eq!(har_request.query_string[0].value, "bar");
    }

    #[test]
    fn test_har_response_from_response() {
        let response = sample_response();
        let har_response = HarResponse::from_response(&response);

        assert_eq!(har_response.status, 200);
        assert_eq!(har_response.status_text, "OK");
        assert_eq!(har_response.headers.len(), 3);
        assert_eq!(har_response.cookies.len(), 1);
        assert_eq!(har_response.cookies[0].name, "token");
        assert_eq!(har_response.cookies[0].path, Some("/".into()));
        assert_eq!(har_response.cookies[0].http_only, Some(true));
        assert_eq!(har_response.cookies[0].secure, Some(true));
        assert_eq!(har_response.content.size, 13);
        assert_eq!(har_response.content.text, Some("Hello, World!".into()));
    }

    #[test]
    fn test_har_builder() {
        let request = sample_request();
        let response = sample_response();
        let exchange = Exchange {
            request,
            response: Some(response),
            error: None,
        };

        let mut builder = HarBuilder::new();
        builder.add_exchange(exchange);

        let har = builder.build();

        assert_eq!(har.log.version, "1.2");
        assert_eq!(har.log.creator.name, "Mira");
        assert_eq!(har.log.entries.len(), 1);

        let entry = &har.log.entries[0];
        assert_eq!(entry.request.method, "GET");
        assert_eq!(entry.response.status, 200);
        assert_eq!(entry.time, 150.0);
    }

    #[test]
    fn test_har_to_json() {
        let request = sample_request();
        let response = sample_response();
        let exchange = Exchange {
            request,
            response: Some(response),
            error: None,
        };

        let mut builder = HarBuilder::new();
        builder.add_exchange(exchange);

        let json = builder.to_json().expect("should serialize");
        assert!(json.contains("\"version\":\"1.2\""));
        assert!(json.contains("\"method\":\"GET\""));
        assert!(json.contains("\"status\":200"));
    }

    #[test]
    fn test_parse_query_string() {
        let params = parse_query_string(Some("foo=bar&baz=qux&empty="));
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].name, "foo");
        assert_eq!(params[0].value, "bar");
        assert_eq!(params[2].name, "empty");
        assert_eq!(params[2].value, "");
    }

    #[test]
    fn test_url_decode_reads_multibyte_utf8() {
        assert_eq!(url_decode("caf%C3%A9"), "café");
        assert_eq!(url_decode("%E2%9C%93+ok"), "✓ ok");
        // Malformed escapes survive as written.
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_decode("%zz"), "%zz");
        assert_eq!(url_decode("%FF"), "\u{FFFD}");
    }

    #[test]
    fn test_url_decode() {
        assert_eq!(url_decode("hello%20world"), "hello world");
        assert_eq!(url_decode("foo+bar"), "foo bar");
        assert_eq!(url_decode("a%3Db%26c%3Dd"), "a=b&c=d");
    }

    #[test]
    fn test_har_timings_from_response() {
        let request = sample_request();
        let response = sample_response();
        let exchange = Exchange {
            request,
            response: Some(response),
            error: None,
        };

        let mut builder = HarBuilder::new();
        builder.add_exchange(exchange);

        let har = builder.build();
        let entry = &har.log.entries[0];

        // Check that timings are populated from Response.timings
        assert_eq!(entry.timings.blocked, Some(0.0));
        assert_eq!(entry.timings.dns, Some(-1.0)); // DNS not tracked
        assert_eq!(entry.timings.connect, Some(10.0));
        assert_eq!(entry.timings.send, 5.0);
        assert_eq!(entry.timings.wait, 100.0);
        assert_eq!(entry.timings.receive, 35.0);
        assert_eq!(entry.timings.ssl, Some(-1.0)); // SSL not applicable yet
    }

    #[test]
    fn test_har_timings_connection_reused() {
        use crate::http::Encoding;
        use crate::http::ResourceType;
        use crate::http::Timings;

        let request = sample_request();
        let response = Response {
            request_id: 1,
            status: 200,
            reason: "OK".into(),
            version: "HTTP/1.1".into(),
            headers: vec![],
            body: Bytes::new(),
            body_decompressed: Bytes::new(),
            encoding: Encoding::Identity,
            resource_type: ResourceType::Other,
            duration_ms: 50,
            timings: Some(Timings {
                blocked_ms: Some(0.0),
                connect_ms: None, // Connection was reused
                send_ms: 2.0,
                wait_ms: 40.0,
                receive_ms: 8.0,
                ssl_ms: None,
                connection_reused: true,
            }),
            remote_addr: None,
        };

        let exchange = Exchange {
            request,
            response: Some(response),
            error: None,
        };

        let mut builder = HarBuilder::new();
        builder.add_exchange(exchange);

        let har = builder.build();
        let entry = &har.log.entries[0];

        // connect should be -1 when connection was reused
        assert_eq!(entry.timings.connect, Some(-1.0));
        assert_eq!(entry.timings.send, 2.0);
        assert_eq!(entry.timings.wait, 40.0);
        assert_eq!(entry.timings.receive, 8.0);
    }

    // =========================================================================
    // HAR Import Tests
    // =========================================================================

    #[test]
    fn test_har_request_round_trip() {
        let request = sample_request();
        let har_request = HarRequest::from_request(&request);

        // Verify Mira extension fields are populated
        assert_eq!(har_request.mira_id, Some(1));
        assert_eq!(har_request.mira_client_addr, Some("127.0.0.1:12345".into()));

        // Convert back to Request
        let restored = har_request.to_request(999).expect("should convert");

        assert_eq!(restored.id, 1); // Should use _miraId, not counter
        assert_eq!(restored.client_addr.to_string(), "127.0.0.1:12345");
        assert_eq!(restored.method, "GET");
        assert_eq!(
            restored.url.to_string(),
            "http://example.com/path?foo=bar&baz=qux"
        );
        assert_eq!(restored.version, "HTTP/1.1");
        assert_eq!(restored.headers.len(), 3);
    }

    #[test]
    fn test_har_response_round_trip() {
        let response = sample_response();
        let har_response = HarResponse::from_response(&response);

        // Verify Mira extension fields are populated
        assert_eq!(har_response.mira_encoding, Some(Encoding::Identity));
        assert_eq!(
            har_response.mira_resource_type,
            Some(ResourceType::Document)
        );
        assert_eq!(har_response.mira_connection_reused, Some(false));

        // Create timings for conversion
        let har_timings = HarTimings {
            blocked: Some(0.0),
            dns: Some(-1.0),
            connect: Some(10.0),
            send: 5.0,
            wait: 100.0,
            receive: 35.0,
            ssl: Some(-1.0),
            comment: None,
        };

        // Convert back to Response
        let restored = har_response.to_response(1, &har_timings);

        assert_eq!(restored.request_id, 1);
        assert_eq!(restored.status, 200);
        assert_eq!(restored.reason, "OK");
        assert_eq!(restored.encoding, Encoding::Identity);
        assert_eq!(restored.resource_type, ResourceType::Document);
        assert_eq!(
            restored.body_decompressed,
            Bytes::from_static(b"Hello, World!")
        );
    }

    #[test]
    fn test_har_entry_round_trip() {
        let request = sample_request();
        let response = sample_response();
        let exchange = Exchange {
            request,
            response: Some(response),
            error: None,
        };

        let started = Utc::now();
        let entry = HarEntry::from_exchange(&exchange, started);

        // Convert back to Exchange
        let restored = entry.to_exchange(999).expect("should convert");

        assert_eq!(restored.request.id, 1); // Should use _miraId
        assert_eq!(restored.request.method, "GET");
        assert_eq!(restored.response.as_ref().map(|r| r.status), Some(200));
        assert!(restored.error.is_none());
    }

    #[test]
    fn test_har_import_without_mira_extensions() {
        // Simulate a HAR from Chrome/Firefox without Mira extensions
        let har_json = r#"{
            "log": {
                "version": "1.2",
                "creator": { "name": "Chrome", "version": "120.0" },
                "entries": [{
                    "startedDateTime": "2024-01-15T10:30:00.000Z",
                    "time": 150,
                    "request": {
                        "method": "POST",
                        "url": "https://api.example.com/users",
                        "httpVersion": "HTTP/2",
                        "cookies": [],
                        "headers": [
                            { "name": "Content-Type", "value": "application/json" }
                        ],
                        "queryString": [],
                        "postData": {
                            "mimeType": "application/json",
                            "text": "{\"name\":\"test\"}"
                        },
                        "headersSize": 100,
                        "bodySize": 15
                    },
                    "response": {
                        "status": 201,
                        "statusText": "Created",
                        "httpVersion": "HTTP/2",
                        "cookies": [],
                        "headers": [
                            { "name": "Content-Type", "value": "application/json" }
                        ],
                        "content": {
                            "size": 25,
                            "mimeType": "application/json",
                            "text": "{\"id\":123,\"name\":\"test\"}"
                        },
                        "redirectURL": "",
                        "headersSize": 50,
                        "bodySize": 25
                    },
                    "cache": {},
                    "timings": {
                        "send": 5,
                        "wait": 100,
                        "receive": 45
                    }
                }]
            }
        }"#;

        let har: Har = serde_json::from_str(har_json).expect("should parse");
        let entry = &har.log.entries[0];

        // Convert to Exchange using fallback values
        let exchange = entry.to_exchange(42).expect("should convert");

        // Request should use fallback values
        assert_eq!(exchange.request.id, 42); // Used counter since no _miraId
        assert_eq!(exchange.request.client_addr.to_string(), "0.0.0.0:0"); // Fallback
        assert_eq!(exchange.request.method, "POST");
        assert_eq!(
            exchange.request.url.to_string(),
            "https://api.example.com/users"
        );

        // Response should infer values from headers
        let response = exchange.response.expect("should have response");
        assert_eq!(response.status, 201);
        assert_eq!(response.resource_type, ResourceType::Json); // Inferred from Content-Type
    }

    #[test]
    fn test_har_full_round_trip_json() {
        use crate::Session;

        let request = sample_request();
        let response = sample_response();
        let exchange = Exchange {
            request,
            response: Some(response),
            error: None,
        };

        // Create a session and add the exchange
        let session = Session::new();
        session.add(exchange);

        // Export to JSON
        let json = session.to_har_json_pretty().expect("should serialize");

        // Import from JSON
        let restored_session = Session::from_har_json(&json).expect("should parse");

        // Verify round-trip fidelity
        assert_eq!(restored_session.len(), 1);

        let exchanges = restored_session.exchanges();
        let restored = &exchanges[0].exchange;

        assert_eq!(restored.request.id, 1);
        assert_eq!(restored.request.method, "GET");
        assert_eq!(restored.request.client_addr.to_string(), "127.0.0.1:12345");
        assert_eq!(restored.response.as_ref().map(|r| r.status), Some(200));
        assert_eq!(
            restored.response.as_ref().map(|r| r.encoding),
            Some(Encoding::Identity)
        );
    }

    #[test]
    fn test_har_entry_populates_server_ip_and_connection_from_remote_addr() {
        // Two requests against the same upstream peer should share the
        // HAR `connection` identifier and the server IP address.
        let mut resp_a = sample_response();
        resp_a.remote_addr = Some("93.184.216.34:443".parse().expect("valid addr"));
        let mut resp_b = sample_response();
        resp_b.remote_addr = Some("93.184.216.34:443".parse().expect("valid addr"));

        let exchange_a = Exchange {
            request: sample_request(),
            response: Some(resp_a),
            error: None,
        };
        let exchange_b = Exchange {
            request: sample_request(),
            response: Some(resp_b),
            error: None,
        };

        let started = Utc::now();
        let entry_a = HarEntry::from_exchange(&exchange_a, started);
        let entry_b = HarEntry::from_exchange(&exchange_b, started);

        assert_eq!(entry_a.server_ip_address.as_deref(), Some("93.184.216.34"));
        assert_eq!(entry_a.connection.as_deref(), Some("93.184.216.34:443"));
        // Siblings on the same upstream share the connection identifier.
        assert_eq!(entry_a.connection, entry_b.connection);
    }

    #[test]
    fn test_har_entry_omits_server_ip_when_remote_addr_missing() {
        // Error exchange (no response) and a response without a remote
        // addr both leave the HAR fields absent rather than bogus.
        let error_entry = HarEntry::from_exchange(
            &Exchange {
                request: sample_request(),
                response: None,
                error: Some("connection refused".into()),
            },
            Utc::now(),
        );
        assert!(error_entry.server_ip_address.is_none());
        assert!(error_entry.connection.is_none());

        let mut resp = sample_response();
        resp.remote_addr = None;
        let ok_entry = HarEntry::from_exchange(
            &Exchange {
                request: sample_request(),
                response: Some(resp),
                error: None,
            },
            Utc::now(),
        );
        assert!(ok_entry.server_ip_address.is_none());
        assert!(ok_entry.connection.is_none());
    }

    #[test]
    fn test_har_post_data_binary_body_round_trips_via_base64() {
        // A request body containing invalid UTF-8 is base64-encoded with
        // an explicit encoding marker, and the import side decodes it
        // back to the original bytes.
        let body = Bytes::from_static(&[0x00, 0xFF, 0xC3, 0x28, 0x7F]);
        let request = Request {
            id: 7,
            client_addr: "127.0.0.1:1".parse().expect("addr"),
            method: "POST".into(),
            url: "http://example.com/upload".parse().expect("url"),
            version: "HTTP/1.1".into(),
            headers: vec![("Content-Type".into(), "application/octet-stream".into())],
            body: body.clone(),
        };

        let har_request = HarRequest::from_request(&request);
        let post_data = har_request.post_data.as_ref().expect("has post_data");
        assert_eq!(post_data.encoding.as_deref(), Some("base64"));
        assert_eq!(
            post_data.text,
            base64::engine::general_purpose::STANDARD.encode(&body)
        );

        let restored = har_request.to_request(0).expect("converts back");
        assert_eq!(restored.body, body);
    }

    #[test]
    fn test_har_post_data_text_that_looks_like_base64_is_preserved() {
        // Prior fragile detection mistook tokens drawn from the base64
        // alphabet (e.g. JWTs, API keys) for encoded binary and decoded
        // them, corrupting the body. With an explicit encoding field this
        // text is preserved verbatim.
        let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
        let request = Request {
            id: 8,
            client_addr: "127.0.0.1:1".parse().expect("addr"),
            method: "POST".into(),
            url: "http://example.com/auth".parse().expect("url"),
            version: "HTTP/1.1".into(),
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: Bytes::from(token),
        };

        let har_request = HarRequest::from_request(&request);
        let post_data = har_request.post_data.as_ref().expect("has post_data");
        // UTF-8 text gets no encoding marker.
        assert!(post_data.encoding.is_none());
        assert_eq!(post_data.text, token);

        let restored = har_request.to_request(0).expect("converts back");
        assert_eq!(restored.body, Bytes::from(token));
    }

    #[test]
    fn test_har_timings_total_time_sums_all_non_negative() {
        // All seven components present: sum should be everything.
        let timings = HarTimings {
            blocked: Some(1.0),
            dns: Some(2.0),
            connect: Some(4.0),
            send: 8.0,
            wait: 16.0,
            receive: 32.0,
            ssl: Some(64.0),
            comment: None,
        };
        assert!((timings.total_time() - 127.0).abs() < f64::EPSILON);

        // -1 sentinels are excluded; so are `None`.
        let timings = HarTimings {
            blocked: Some(-1.0),
            dns: None,
            connect: Some(10.0),
            send: 1.0,
            wait: 2.0,
            receive: 4.0,
            ssl: Some(5.0),
            comment: None,
        };
        assert!((timings.total_time() - (10.0 + 1.0 + 2.0 + 4.0 + 5.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_har_import_duration_includes_connect_and_ssl() {
        // Before the fix this returned 5 (send+wait+receive only),
        // silently dropping the 10 ms of connect and 20 ms of SSL that
        // were part of the original request's wall time.
        let har_response = HarResponse {
            status: 200,
            status_text: "OK".into(),
            http_version: "HTTP/1.1".into(),
            cookies: Vec::new(),
            headers: Vec::new(),
            content: HarContent::empty(),
            redirect_url: String::new(),
            headers_size: 0,
            body_size: 0,
            comment: None,
            mira_encoding: None,
            mira_resource_type: None,
            mira_remote_addr: None,
            mira_connection_reused: None,
        };
        let har_timings = HarTimings {
            blocked: Some(0.0),
            dns: Some(-1.0),
            connect: Some(10.0),
            send: 1.0,
            wait: 2.0,
            receive: 2.0,
            ssl: Some(20.0),
            comment: None,
        };

        let response = har_response.to_response(1, &har_timings);
        assert_eq!(response.duration_ms, 35);
    }

    #[test]
    fn test_har_error_response() {
        let request = sample_request();
        let exchange = Exchange {
            request,
            response: None,
            error: Some("Connection refused".into()),
        };

        let started = Utc::now();
        let entry = HarEntry::from_exchange(&exchange, started);

        // Verify error is stored in comment
        assert_eq!(entry.comment, Some("Connection refused".into()));
        assert_eq!(entry.response.status, 0);
        assert!(entry.response.is_error());

        // Convert back
        let restored = entry.to_exchange(1).expect("should convert");
        assert!(restored.response.is_none());
        assert_eq!(restored.error, Some("Connection refused".into()));
    }
}
