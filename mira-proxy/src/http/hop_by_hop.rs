//! Hop-by-hop header filtering for proxy forwarding (RFC 7230 §6.1).
//!
//! A proxy MUST NOT forward hop-by-hop headers to the next hop. These
//! headers apply only to a single transport-level connection and include
//! (per the RFC): `Connection`, `Keep-Alive`, `Proxy-Authenticate`,
//! `Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`,
//! plus any header name listed in the `Connection` header's value.
//!
//! Mira preserves `Transfer-Encoding` so that the existing byte-stream body
//! forwarding continues to work. A fully conforming implementation would
//! re-frame chunked bodies into `Content-Length` instead; that is a larger
//! change than this filter addresses.
//!
//! `Upgrade` / `Connection: Upgrade` traffic is handled in a dedicated
//! code path (`tunnel::establish_websocket`) that forwards the client's
//! header bytes verbatim; it never passes through this filter. See
//! [`serialize_upgrade_request_head`] for the one correction that path
//! does make.

use crate::http::ParsedRequest;
use crate::http::ParsedResponse;
use crate::http::header_list_contains;

/// Standard hop-by-hop header names, lower-cased.
///
/// `Transfer-Encoding` is intentionally excluded — see module docs.
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    // Non-standard but widely used by older HTTP clients / proxies.
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
];

/// Check whether the given header name is hop-by-hop in the context of
/// the current message, considering both the standard set and any header
/// names listed in the message's `Connection` header.
pub fn is_hop_by_hop(name: &str, connection_value: Option<&str>) -> bool {
    if HOP_BY_HOP_HEADERS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(name))
    {
        return true;
    }
    connection_value.is_some_and(|value| header_list_contains(value, name))
}

/// Whether `name` is a `Content-Length` on a chunked message.
///
/// Chunked framing wins when both are present (RFC 9112 §6.3), and a
/// proxy MUST remove the `Content-Length` before forwarding. A next hop
/// that framed by it instead would read a different message boundary.
fn conflicting_length(name: &str, chunked: bool) -> bool {
    chunked && name.eq_ignore_ascii_case("content-length")
}

/// Serialize a request head (request line + headers + terminating CRLF),
/// stripping hop-by-hop headers and any `Content-Length` that conflicts
/// with chunked framing.
///
/// The returned bytes are suitable to write to an upstream connection in
/// place of the original client header bytes.
pub fn serialize_request_head(request: &ParsedRequest) -> Vec<u8> {
    let connection = request.header("connection");
    let chunked = request.is_chunked();

    // Rough preallocation: method + target + version + 4 CRLFs, plus each
    // header's name + ": " + value + CRLF.
    let estimate: usize = request.method.len()
        + request.target.len()
        + request.version.len()
        + 6
        + request
            .headers
            .iter()
            .map(|(k, v)| k.len() + v.len() + 4)
            .sum::<usize>()
        + 2;
    let mut out = Vec::with_capacity(estimate);

    out.extend_from_slice(request.method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(request.target.as_bytes());
    out.push(b' ');
    out.extend_from_slice(request.version.as_bytes());
    out.extend_from_slice(b"\r\n");

    for (name, value) in &request.headers {
        if is_hop_by_hop(name, connection) || conflicting_length(name, chunked) {
            continue;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    out.extend_from_slice(b"\r\n");
    out
}

/// Serialize the head of an upgrade request for forwarding.
///
/// Unlike [`serialize_request_head`], the client's header bytes are
/// passed through exactly as they arrived. `Connection: Upgrade` and
/// `Upgrade: websocket` are hop-by-hop by the letter of RFC 7230 §6.1
/// but are precisely what makes the handshake work, and handshake
/// implementations can be particular about header order and spelling.
///
/// Only the request line is rebuilt, so an absolute-form target from a
/// client addressing the proxy (`GET http://host/ws HTTP/1.1`) reaches
/// the origin in origin-form like every other forwarded request —
/// servers that route on the request path 404 the handshake otherwise,
/// and the failure reads as the origin's fault rather than the proxy's.
///
/// `request` supplies the corrected line, so callers MUST have run
/// [`ParsedRequest::rewrite_to_origin_form`] first. The `Host` header is
/// left as the client wrote it; when a client contradicts its own target
/// authority the proxy still routes by the authority, so only a broken
/// or hostile client can tell the difference.
pub fn serialize_upgrade_request_head(raw_head: &[u8], request: &ParsedRequest) -> Vec<u8> {
    // Everything from the first CRLF onward is headers, forwarded as
    // sent. A head that parsed has one; if it somehow does not, passing
    // the bytes through unchanged is the safe reading.
    let Some(line_end) = raw_head.windows(2).position(|pair| pair == b"\r\n") else {
        return raw_head.to_vec();
    };

    let mut out = Vec::with_capacity(raw_head.len());
    out.extend_from_slice(request.method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(request.target.as_bytes());
    out.push(b' ');
    out.extend_from_slice(request.version.as_bytes());
    out.extend_from_slice(&raw_head[line_end..]);
    out
}

/// Serialize a response head (status line + headers + terminating CRLF),
/// stripping hop-by-hop headers and any `Content-Length` that conflicts
/// with chunked framing.
///
/// The returned bytes are suitable to write to the downstream client in
/// place of the original upstream header bytes.
pub fn serialize_response_head(response: &ParsedResponse) -> Vec<u8> {
    let connection = response.header("connection");
    let chunked = response.is_chunked();

    let estimate: usize = response.version.len()
        + response.reason.len()
        + 8 // " XXX " + CRLF
        + response
            .headers
            .iter()
            .map(|(k, v)| k.len() + v.len() + 4)
            .sum::<usize>()
        + 2;
    let mut out = Vec::with_capacity(estimate);

    out.extend_from_slice(response.version.as_bytes());
    out.push(b' ');
    out.extend_from_slice(response.status.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(response.reason.as_bytes());
    out.extend_from_slice(b"\r\n");

    for (name, value) in &response.headers {
        if is_hop_by_hop(name, connection) || conflicting_length(name, chunked) {
            continue;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    out.extend_from_slice(b"\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_head(raw: &str) -> ParsedRequest {
        ParsedRequest::parse(raw.as_bytes())
            .expect("parses")
            .expect("complete")
    }

    const RAW_UPGRADE: &str = "GET http://example.com/ws HTTP/1.1\r\nHost: \
                               example.com\r\nConnection: Upgrade\r\nUpgrade: \
                               websocket\r\nSec-WebSocket-Key: \
                               dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";

    #[test]
    fn test_serialize_upgrade_head_rewrites_only_the_request_line() {
        let mut request = parse_head(RAW_UPGRADE);
        request.rewrite_to_origin_form();

        let out = serialize_upgrade_request_head(RAW_UPGRADE.as_bytes(), &request);
        let text = std::str::from_utf8(&out).expect("utf8");

        assert!(
            text.starts_with("GET /ws HTTP/1.1\r\n"),
            "the origin must see origin-form: {text}"
        );
        // Everything after the request line is byte-identical to what
        // the client sent — order, spelling, and the hop-by-hop headers
        // the handshake depends on.
        let sent_headers = RAW_UPGRADE
            .split_once("\r\n")
            .expect("has a request line")
            .1;
        let forwarded_headers = text.split_once("\r\n").expect("has a request line").1;
        assert_eq!(forwarded_headers, sent_headers);
    }

    #[test]
    fn test_serialize_upgrade_head_leaves_an_origin_form_request_alone() {
        let raw = "GET /ws HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\n\r\n";
        let mut request = parse_head(raw);
        request.rewrite_to_origin_form();

        let out = serialize_upgrade_request_head(raw.as_bytes(), &request);
        assert_eq!(std::str::from_utf8(&out).expect("utf8"), raw);
    }

    #[test]
    fn test_serialize_upgrade_head_passes_through_a_head_without_a_crlf() {
        // Cannot happen for a head that parsed; forwarding the bytes
        // unchanged is the safe reading if it ever does.
        let request = parse_head("GET /ws HTTP/1.1\r\nHost: x\r\n\r\n");
        let out = serialize_upgrade_request_head(b"garbage", &request);
        assert_eq!(out, b"garbage");
    }

    #[test]
    fn test_is_hop_by_hop_standard_set() {
        assert!(is_hop_by_hop("Connection", None));
        assert!(is_hop_by_hop("connection", None));
        assert!(is_hop_by_hop("Keep-Alive", None));
        assert!(is_hop_by_hop("Proxy-Authenticate", None));
        assert!(is_hop_by_hop("Proxy-Authorization", None));
        assert!(is_hop_by_hop("Proxy-Connection", None));
        assert!(is_hop_by_hop("TE", None));
        assert!(is_hop_by_hop("Trailer", None));
        assert!(is_hop_by_hop("Upgrade", None));
    }

    #[test]
    fn test_is_hop_by_hop_transfer_encoding_preserved() {
        // Mira intentionally treats Transfer-Encoding as end-to-end so
        // existing chunked-body forwarding keeps working.
        assert!(!is_hop_by_hop("Transfer-Encoding", None));
    }

    #[test]
    fn test_is_hop_by_hop_connection_listed_headers() {
        // Headers named in the Connection header become hop-by-hop for
        // this message.
        let conn = Some("keep-alive, X-Foo, X-Bar");
        assert!(is_hop_by_hop("X-Foo", conn));
        assert!(is_hop_by_hop("x-bar", conn));
        // And a header not listed stays end-to-end.
        assert!(!is_hop_by_hop("X-Baz", conn));
    }

    #[test]
    fn test_is_hop_by_hop_end_to_end_headers() {
        assert!(!is_hop_by_hop("Host", None));
        assert!(!is_hop_by_hop("User-Agent", None));
        assert!(!is_hop_by_hop("Content-Type", None));
        assert!(!is_hop_by_hop("Content-Length", None));
        assert!(!is_hop_by_hop("Authorization", None));
        assert!(!is_hop_by_hop("Cookie", None));
    }

    #[test]
    fn test_serialize_request_head_strips_hop_by_hop() {
        let req = b"POST /api HTTP/1.1\r\n\
                    Host: example.com\r\n\
                    Connection: keep-alive, X-Custom\r\n\
                    Keep-Alive: timeout=5\r\n\
                    Proxy-Authorization: Basic abc\r\n\
                    Proxy-Connection: close\r\n\
                    X-Custom: should-also-drop\r\n\
                    Content-Type: application/json\r\n\
                    Content-Length: 3\r\n\
                    \r\n";
        let parsed = ParsedRequest::parse(req).unwrap().unwrap();
        let out = serialize_request_head(&parsed);
        let text = std::str::from_utf8(&out).unwrap();

        // Request line is preserved.
        assert!(text.starts_with("POST /api HTTP/1.1\r\n"));
        // End-to-end headers pass through.
        assert!(text.contains("Host: example.com\r\n"));
        assert!(text.contains("Content-Type: application/json\r\n"));
        assert!(text.contains("Content-Length: 3\r\n"));
        // Hop-by-hop headers are stripped.
        assert!(!text.contains("Connection:"));
        assert!(!text.contains("Keep-Alive:"));
        assert!(!text.contains("Proxy-Authorization:"));
        assert!(!text.contains("Proxy-Connection:"));
        // Connection-listed custom header is stripped too.
        assert!(!text.contains("X-Custom:"));
        // Terminating empty line is present.
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_serialize_response_head_strips_hop_by_hop() {
        let resp = b"HTTP/1.1 200 OK\r\n\
                     Content-Type: text/html\r\n\
                     Content-Length: 13\r\n\
                     Connection: close\r\n\
                     Keep-Alive: timeout=5\r\n\
                     Proxy-Authenticate: Basic realm=\"proxy\"\r\n\
                     \r\n";
        let parsed = ParsedResponse::parse_headers(resp).unwrap().unwrap();
        let out = serialize_response_head(&parsed);
        let text = std::str::from_utf8(&out).unwrap();

        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: text/html\r\n"));
        assert!(text.contains("Content-Length: 13\r\n"));
        assert!(!text.contains("Connection:"));
        assert!(!text.contains("Keep-Alive:"));
        assert!(!text.contains("Proxy-Authenticate:"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_serialize_drops_content_length_from_a_chunked_message() {
        let req = b"POST /x HTTP/1.1\r\n\
                    Host: example.com\r\n\
                    Content-Length: 30\r\n\
                    Transfer-Encoding: chunked\r\n\
                    \r\n";
        let parsed = ParsedRequest::parse(req).unwrap().unwrap();
        let text = String::from_utf8(serialize_request_head(&parsed)).unwrap();
        assert!(text.contains("Transfer-Encoding: chunked\r\n"));
        assert!(!text.contains("Content-Length"), "got: {text}");

        let resp = b"HTTP/1.1 200 OK\r\n\
                     Content-Length: 30\r\n\
                     Transfer-Encoding: chunked\r\n\
                     \r\n";
        let parsed = ParsedResponse::parse_headers(resp).unwrap().unwrap();
        let text = String::from_utf8(serialize_response_head(&parsed)).unwrap();
        assert!(!text.contains("Content-Length"), "got: {text}");
    }

    #[test]
    fn test_serialize_preserves_transfer_encoding() {
        let req = b"POST /x HTTP/1.1\r\n\
                    Host: example.com\r\n\
                    Transfer-Encoding: chunked\r\n\
                    \r\n";
        let parsed = ParsedRequest::parse(req).unwrap().unwrap();
        let out = serialize_request_head(&parsed);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(text.contains("Transfer-Encoding: chunked\r\n"));
    }
}
