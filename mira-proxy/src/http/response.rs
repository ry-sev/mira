//! HTTP response parsing utilities.

use crate::error::Error;
use crate::error::Result;
use crate::http::headers::declared_content_length;
use crate::http::headers::final_coding_is_chunked;
use crate::http::headers::header_list_tokens;
use bytes::Bytes;
use bytes::BytesMut;

/// Most header fields a request or response head may carry.
pub const MAX_HEADERS: usize = 128;

/// A parsed HTTP response.
#[derive(Debug, Clone)]
pub struct ParsedResponse {
    /// HTTP status code (e.g., 200, 404, 500)
    pub status: u16,

    /// HTTP reason phrase (e.g., "OK", "Not Found")
    pub reason: String,

    /// HTTP version string
    pub version: String,

    /// Response headers as key-value pairs
    pub headers: Vec<(String, String)>,

    /// Number of bytes consumed for the headers
    pub header_len: usize,

    /// Response body (may be empty if not yet read)
    pub body: Bytes,
}

impl ParsedResponse {
    /// Parse HTTP response headers from raw bytes.
    ///
    /// Returns `None` if the response is incomplete (need more data).
    /// Returns `Err` if the response is malformed.
    /// Note: This only parses headers, not the body.
    pub fn parse_headers(buf: &[u8]) -> Result<Option<Self>> {
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut resp = httparse::Response::new(&mut headers);

        match resp.parse(buf) {
            Ok(httparse::Status::Complete(len)) => {
                let status = resp
                    .code
                    .ok_or_else(|| Error::InvalidResponse("missing status code".into()))?;

                let reason = resp.reason.unwrap_or("").to_owned();

                let version = match resp.version {
                    Some(0) => "HTTP/1.0".to_owned(),
                    _ => "HTTP/1.1".to_owned(),
                };

                let headers = resp
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            h.name.to_owned(),
                            String::from_utf8_lossy(h.value).into_owned(),
                        )
                    })
                    .collect();

                Ok(Some(Self {
                    status,
                    reason,
                    version,
                    headers,
                    header_len: len,
                    body: Bytes::new(),
                }))
            }
            Ok(httparse::Status::Partial) => Ok(None),
            Err(httparse::Error::TooManyHeaders) => Err(Error::TooManyHeaders { max: MAX_HEADERS }),
            Err(e) => Err(Error::HttpParse(e.to_string())),
        }
    }

    /// Get a header value by name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Get the Content-Length header value, if present and valid.
    pub fn content_length(&self) -> Option<usize> {
        declared_content_length(&self.headers).ok().flatten()
    }

    /// Check if the response uses chunked transfer encoding, meaning its
    /// final transfer coding is `chunked`.
    pub fn is_chunked(&self) -> bool {
        final_coding_is_chunked(&self.headers)
    }

    /// Why this response's body length cannot be trusted, if it cannot.
    ///
    /// Only an unusable `Content-Length` counts. A response whose last
    /// transfer coding is not `chunked` is valid; it is read until the
    /// connection closes.
    pub fn framing_error(&self) -> Option<&'static str> {
        (!self.is_chunked() && declared_content_length(&self.headers).is_err())
            .then_some("invalid or conflicting Content-Length")
    }

    /// Set the response body.
    ///
    /// If the response uses chunked transfer encoding, the body will be
    /// automatically decoded to remove chunk markers.
    #[must_use]
    pub fn with_body(mut self, body: Bytes) -> Self {
        if self.is_chunked() {
            self.body = decode_chunked(&body);
        } else {
            self.body = body;
        }
        self
    }
}

/// Check whether a `Transfer-Encoding` header value indicates chunked framing.
///
/// `Transfer-Encoding` is a comma-separated list of codings (e.g.
/// `gzip, chunked`). Only the last one frames the message (RFC 9112
/// §6.3), so this matches when the final token is `chunked`
/// (case-insensitive). Whole tokens are compared, so `chunked-foo` does
/// not match.
pub fn transfer_encoding_is_chunked(header_value: &str) -> bool {
    header_list_tokens(header_value)
        .last()
        .is_some_and(|coding| coding.eq_ignore_ascii_case("chunked"))
}

/// Check whether an HTTP response to the given request method and status
/// is defined to carry no body per RFC 7230 §3.3.3.
///
/// Responses with no body:
/// - Any response to a HEAD request (even with `Content-Length` /
///   `Transfer-Encoding`).
/// - 1xx informational responses.
/// - 204 No Content.
/// - 304 Not Modified.
///
/// A proxy that reads the advertised `Content-Length` bytes in these cases
/// will hang waiting for bytes that never arrive, so this check MUST be
/// consulted before attempting to read the body.
pub fn response_has_no_body(request_method: &str, status: u16) -> bool {
    if request_method.eq_ignore_ascii_case("HEAD") {
        return true;
    }
    status < 200 || status == 204 || status == 304
}

/// Check if a chunked body has been fully received.
///
/// See [`chunked_body_end`] for how the framing is walked.
pub fn is_chunked_body_complete(data: &[u8]) -> bool {
    chunked_body_end(data).is_some()
}

/// Find where a chunked body ends within `data`.
///
/// Parses the chunk framing and returns the offset just past the final
/// empty line, once a zero-size terminating chunk has been seen (followed
/// by optional trailer headers). Returns `None` when more data is needed
/// or when the framing is malformed — callers SHOULD keep reading until
/// this returns `Some` or the peer closes the connection.
///
/// Unlike a byte-pattern match on `0\r\n\r\n`, this walks the chunk sizes
/// and will not false-positive on binary payloads that happen to contain
/// the same sequence inside a chunk's data.
///
/// The offset matters as much as the yes/no: anything after it is not
/// part of this message, and on a request stream it is the next pipelined
/// request rather than body bytes.
pub fn chunked_body_end(data: &[u8]) -> Option<usize> {
    let mut pos = 0;
    loop {
        let line_end = find_crlf(data.get(pos..)?)?;
        let size_line = &data[pos..pos + line_end];
        pos += line_end + 2;

        let size_str = std::str::from_utf8(size_line).ok()?;
        let size_str = size_str.split(';').next().unwrap_or("").trim();
        let chunk_size = usize::from_str_radix(size_str, 16).ok()?;

        if chunk_size == 0 {
            // Consume optional trailer headers until an empty line; the
            // body ends after that line's CRLF.
            loop {
                let line_end = find_crlf(data.get(pos..)?)?;
                if line_end == 0 {
                    return Some(pos + 2);
                }
                pos += line_end + 2;
            }
        }

        let data_end = pos.checked_add(chunk_size)?;
        if data_end + 2 > data.len() {
            return None;
        }
        if &data[data_end..data_end + 2] != b"\r\n" {
            return None;
        }
        pos = data_end + 2;
    }
}

/// Decode chunked transfer encoding.
///
/// Chunked encoding format:
/// ```text
/// <chunk-size in hex>\r\n
/// <chunk-data>\r\n
/// ...
/// 0\r\n
/// \r\n
/// ```
fn decode_chunked(data: &[u8]) -> Bytes {
    let mut decoded = BytesMut::new();
    let mut pos = 0;

    while pos < data.len() {
        let Some(line_end) = find_crlf(&data[pos..]) else {
            // Malformed - no CRLF found, return what we have
            break;
        };

        let size_line = &data[pos..pos + line_end];
        pos += line_end + 2; // Skip past the CRLF

        let size_str = std::str::from_utf8(size_line)
            .ok()
            .and_then(|s| s.split(';').next()) // Handle chunk extensions
            .unwrap_or("");

        let Ok(chunk_size) = usize::from_str_radix(size_str.trim(), 16) else {
            break; // Malformed chunk size
        };

        if chunk_size == 0 {
            break;
        }

        if pos + chunk_size > data.len() {
            // Incomplete chunk - take what's available
            decoded.extend_from_slice(&data[pos..]);
            break;
        }

        decoded.extend_from_slice(&data[pos..pos + chunk_size]);
        pos += chunk_size;

        // Skip trailing CRLF after chunk data
        if pos + 2 <= data.len() && &data[pos..pos + 2] == b"\r\n" {
            pos += 2;
        }
    }

    decoded.freeze()
}

/// Find the position of \r\n in the data.
fn find_crlf(data: &[u8]) -> Option<usize> {
    data.windows(2).position(|w| w == b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_response() {
        let resp = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello";
        let parsed = ParsedResponse::parse_headers(resp).unwrap().unwrap();

        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.reason, "OK");
        assert_eq!(parsed.header("content-type"), Some("text/plain"));
        assert_eq!(parsed.content_length(), Some(5));
    }

    #[test]
    fn test_parse_chunked_response() {
        let resp = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let parsed = ParsedResponse::parse_headers(resp).unwrap().unwrap();

        assert!(parsed.is_chunked());
        assert_eq!(parsed.content_length(), None);
    }

    #[test]
    fn test_incomplete_response() {
        let resp = b"HTTP/1.1 200 OK\r\nContent-Type: text";
        let parsed = ParsedResponse::parse_headers(resp).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn test_decode_chunked_body() {
        // Simple chunked body: "Hello" (5 bytes) + "World" (5 bytes)
        let chunked = b"5\r\nHello\r\n5\r\nWorld\r\n0\r\n\r\n";
        let decoded = decode_chunked(chunked);
        assert_eq!(&decoded[..], b"HelloWorld");
    }

    #[test]
    fn test_decode_chunked_with_hex_size() {
        // Chunk size in hex: 0xa = 10 bytes
        let chunked = b"a\r\n0123456789\r\n0\r\n\r\n";
        let decoded = decode_chunked(chunked);
        assert_eq!(&decoded[..], b"0123456789");
    }

    #[test]
    fn test_decode_chunked_single_chunk() {
        // Matches the example.com response pattern: 201 hex = 513 bytes
        let content = "x".repeat(513);
        let chunked_data = format!("201\r\n{content}\r\n0\r\n\r\n");
        let decoded = decode_chunked(chunked_data.as_bytes());
        assert_eq!(decoded.len(), 513);
        assert_eq!(&decoded[..], content.as_bytes());
    }

    #[test]
    fn test_transfer_encoding_is_chunked_plain() {
        assert!(transfer_encoding_is_chunked("chunked"));
        assert!(transfer_encoding_is_chunked("CHUNKED"));
        assert!(transfer_encoding_is_chunked("Chunked"));
        assert!(transfer_encoding_is_chunked("  chunked  "));
    }

    #[test]
    fn test_transfer_encoding_is_chunked_stacked() {
        // Multiple codings — `chunked` is the final coding per RFC 7230.
        assert!(transfer_encoding_is_chunked("gzip, chunked"));
        assert!(transfer_encoding_is_chunked("gzip,chunked"));
        assert!(transfer_encoding_is_chunked("deflate, gzip, chunked"));
        // Case-insensitive, arbitrary whitespace.
        assert!(transfer_encoding_is_chunked("GZIP ,  Chunked"));
    }

    #[test]
    fn test_transfer_encoding_is_chunked_rejects_substrings() {
        // Substring matches must not false-positive.
        assert!(!transfer_encoding_is_chunked("chunked-foo"));
        assert!(!transfer_encoding_is_chunked("x-chunked"));
        assert!(!transfer_encoding_is_chunked("gzip"));
        assert!(!transfer_encoding_is_chunked("identity"));
        assert!(!transfer_encoding_is_chunked(""));
    }

    #[test]
    fn test_parsed_response_is_chunked_with_stacked_encoding() {
        // Regression: previously `eq_ignore_ascii_case("chunked")` missed
        // `gzip, chunked` and the body reader would hang.
        let resp = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n";
        let parsed = ParsedResponse::parse_headers(resp).unwrap().unwrap();
        assert!(parsed.is_chunked());
    }

    #[test]
    fn test_response_has_no_body_head_request() {
        // HEAD always has no body, regardless of status.
        assert!(response_has_no_body("HEAD", 200));
        assert!(response_has_no_body("HEAD", 404));
        assert!(response_has_no_body("HEAD", 500));
        // Case-insensitive match.
        assert!(response_has_no_body("head", 200));
        assert!(response_has_no_body("Head", 200));
    }

    #[test]
    fn test_response_has_no_body_informational_and_empty_statuses() {
        assert!(response_has_no_body("GET", 100));
        assert!(response_has_no_body("GET", 101));
        assert!(response_has_no_body("GET", 199));
        assert!(response_has_no_body("GET", 204));
        assert!(response_has_no_body("GET", 304));
    }

    #[test]
    fn test_response_has_no_body_normal_responses() {
        assert!(!response_has_no_body("GET", 200));
        assert!(!response_has_no_body("GET", 201));
        assert!(!response_has_no_body("GET", 301));
        assert!(!response_has_no_body("GET", 404));
        assert!(!response_has_no_body("GET", 500));
        assert!(!response_has_no_body("POST", 200));
    }

    #[test]
    fn test_is_chunked_body_complete_simple() {
        let chunked = b"5\r\nHello\r\n0\r\n\r\n";
        assert!(is_chunked_body_complete(chunked));
    }

    #[test]
    fn test_is_chunked_body_complete_multiple_chunks() {
        let chunked = b"5\r\nHello\r\n5\r\nWorld\r\n0\r\n\r\n";
        assert!(is_chunked_body_complete(chunked));
    }

    #[test]
    fn test_is_chunked_body_complete_with_trailers() {
        let chunked = b"5\r\nHello\r\n0\r\nExpires: 0\r\nX-Foo: bar\r\n\r\n";
        assert!(is_chunked_body_complete(chunked));
    }

    #[test]
    fn test_is_chunked_body_complete_partial_returns_false() {
        // Missing the final \r\n terminating the response.
        assert!(!is_chunked_body_complete(b"5\r\nHello\r\n0\r\n"));
        // No terminating chunk yet.
        assert!(!is_chunked_body_complete(b"5\r\nHello\r\n"));
        // Only the size line read.
        assert!(!is_chunked_body_complete(b"5\r\n"));
        // Empty input.
        assert!(!is_chunked_body_complete(b""));
    }

    #[test]
    fn test_is_chunked_body_complete_ignores_pattern_inside_chunk_data() {
        // A single chunk whose payload contains the literal bytes `0\r\n\r\n`
        // — previously this would false-positive and terminate the read.
        let payload = b"AAA0\r\n\r\nBBB";
        let chunked = format!("{:x}\r\n", payload.len());
        let mut data = chunked.into_bytes();
        data.extend_from_slice(payload);
        data.extend_from_slice(b"\r\n"); // chunk-data CRLF
        // Not yet terminated — must return false even though `0\r\n\r\n`
        // appears inside the chunk data.
        assert!(!is_chunked_body_complete(&data));

        // Now append the terminating chunk; it should complete.
        data.extend_from_slice(b"0\r\n\r\n");
        assert!(is_chunked_body_complete(&data));
    }

    #[test]
    fn test_is_chunked_body_complete_handles_chunk_extensions() {
        // Chunk size may be followed by `;ext=value` extensions — ignored.
        let chunked = b"5;foo=bar\r\nHello\r\n0\r\n\r\n";
        assert!(is_chunked_body_complete(chunked));
    }

    #[test]
    fn test_is_chunked_body_complete_hex_size() {
        // 0xa = 10 bytes payload.
        let chunked = b"a\r\n0123456789\r\n0\r\n\r\n";
        assert!(is_chunked_body_complete(chunked));
    }

    #[test]
    fn test_with_body_decodes_chunked() {
        let resp = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let parsed = ParsedResponse::parse_headers(resp).unwrap().unwrap();

        let chunked_body = b"5\r\nHello\r\n0\r\n\r\n";
        let parsed = parsed.with_body(Bytes::from_static(chunked_body));

        assert_eq!(&parsed.body[..], b"Hello");
    }

    #[test]
    fn test_with_body_non_chunked_unchanged() {
        let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";
        let parsed = ParsedResponse::parse_headers(resp).unwrap().unwrap();

        let body = Bytes::from_static(b"Hello");
        let parsed = parsed.with_body(body);

        assert_eq!(&parsed.body[..], b"Hello");
    }
}
