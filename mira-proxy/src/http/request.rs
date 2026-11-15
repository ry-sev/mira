//! HTTP request parsing utilities.

use crate::error::Error;
use crate::error::Result;
use crate::http::MAX_HEADERS;
use crate::http::header_list_contains;
use crate::http::header_list_tokens;
use crate::http::headers::declared_content_length;
use crate::http::headers::final_coding_is_chunked;
use crate::http::headers::has_transfer_encoding;

/// A parsed HTTP request from raw bytes.
#[derive(Debug)]
pub struct ParsedRequest {
    /// The HTTP method (GET, POST, CONNECT, etc.)
    pub method: String,

    /// The request target (path or authority for CONNECT)
    pub target: String,

    /// HTTP version string
    pub version: String,

    /// Raw headers as key-value pairs
    pub headers: Vec<(String, String)>,

    /// Number of bytes consumed from the input
    pub header_len: usize,
}

impl ParsedRequest {
    /// Parse an HTTP request from raw bytes.
    ///
    /// Returns `None` if the request is incomplete (need more data).
    /// Returns `Err` if the request is malformed.
    pub fn parse(buf: &[u8]) -> Result<Option<Self>> {
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut headers);

        match req.parse(buf) {
            Ok(httparse::Status::Complete(len)) => {
                let method = req
                    .method
                    .ok_or_else(|| Error::InvalidRequest("missing method".into()))?
                    .to_owned();

                let target = req
                    .path
                    .ok_or_else(|| Error::InvalidRequest("missing path".into()))?
                    .to_owned();

                let version = match req.version {
                    Some(0) => "HTTP/1.0".to_owned(),
                    _ => "HTTP/1.1".to_owned(),
                };

                let headers = req
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
                    method,
                    target,
                    version,
                    headers,
                    header_len: len,
                }))
            }
            Ok(httparse::Status::Partial) => Ok(None),
            Err(httparse::Error::TooManyHeaders) => Err(Error::TooManyHeaders { max: MAX_HEADERS }),
            Err(e) => Err(Error::HttpParse(e.to_string())),
        }
    }

    /// Check if this is a CONNECT request (for HTTPS tunneling).
    pub fn is_connect(&self) -> bool {
        self.method.eq_ignore_ascii_case("CONNECT")
    }

    /// Get the Host header value.
    pub fn host(&self) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("host"))
            .map(|(_, v)| v.as_str())
    }

    /// Extract host and port from the request.
    ///
    /// For CONNECT requests, parses the authority from the target. For an
    /// absolute-form target the authority in that target wins: RFC 7230
    /// §5.4 ranks it above whatever `Host` the client sent, and routing
    /// by the `Host` header instead would send the request to a different
    /// origin than the one it names. Otherwise the `Host` header is used.
    pub fn host_port(&self) -> Result<(String, u16)> {
        let authority = if self.is_connect() {
            self.target.clone()
        } else if let Some((authority, _)) = split_absolute_form(&self.target) {
            authority.to_owned()
        } else {
            self.host()
                .ok_or_else(|| Error::InvalidRequest("missing Host header".into()))?
                .to_owned()
        };

        parse_authority(&authority, if self.is_connect() { 443 } else { 80 })
    }

    /// Rewrite an absolute-form request target into origin-form.
    ///
    /// Clients address a proxy in absolute-form (`GET http://host/path
    /// HTTP/1.1`); origin servers expect origin-form (`GET /path`). RFC
    /// 7230 §5.3.1 obliges an origin to accept absolute-form, but enough
    /// real servers mishandle it that conventional proxies convert before
    /// forwarding.
    ///
    /// The authority is not simply dropped — it becomes the `Host`
    /// header, replacing any the client sent, because §5.4 ranks it
    /// higher. That keeps the request pointed at the origin it named, and
    /// keeps the `Host` the upstream sees consistent with the host the
    /// proxy connected to.
    ///
    /// Returns whether anything was rewritten. No-op for CONNECT
    /// (authority-form), `OPTIONS *` (asterisk-form), and targets already
    /// in origin-form.
    pub fn rewrite_to_origin_form(&mut self) -> bool {
        if self.is_connect() {
            return false;
        }
        let Some((authority, origin_form)) = split_absolute_form(&self.target) else {
            return false;
        };
        let authority = authority.to_owned();

        self.target = origin_form;
        self.set_host(&authority);
        true
    }

    /// Set the `Host` header, replacing whatever the client sent.
    ///
    /// Any duplicate `Host` headers are collapsed into the one: passing
    /// more than one upstream is how request-smuggling desyncs start.
    fn set_host(&mut self, authority: &str) {
        let mut seen = false;
        self.headers.retain_mut(|(name, value)| {
            if !name.eq_ignore_ascii_case("host") {
                return true;
            }
            if seen {
                return false;
            }
            seen = true;
            authority.clone_into(value);
            true
        });

        if !seen {
            self.headers.push(("Host".to_owned(), authority.to_owned()));
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

    /// Check if the request uses chunked transfer encoding, meaning its
    /// final transfer coding is `chunked`.
    pub fn is_chunked(&self) -> bool {
        final_coding_is_chunked(&self.headers)
    }

    /// Why this request's body framing cannot be trusted, if it cannot.
    ///
    /// RFC 9112 §6.3 requires a `400` and a close for both cases. The
    /// proxy and the origin could each pick a different body length from
    /// such a request, and that disagreement is what request smuggling
    /// exploits.
    pub fn framing_error(&self) -> Option<&'static str> {
        if has_transfer_encoding(&self.headers) {
            // Content-Length is ignored, and removed, when chunked wins.
            return (!self.is_chunked()).then_some("Transfer-Encoding must end with chunked");
        }
        declared_content_length(&self.headers)
            .is_err()
            .then_some("invalid or conflicting Content-Length")
    }

    /// Check if this is a WebSocket upgrade request.
    ///
    /// Requires `upgrade` to be listed in `Connection` and `websocket` to
    /// be offered in `Upgrade`. Both headers are comma-separated lists,
    /// so both are matched by whole token: a substring search would treat
    /// `Connection: x-upgrade-hint` as an upgrade request and hand an
    /// ordinary exchange to the tunnel code.
    pub fn is_websocket_upgrade(&self) -> bool {
        let connection_offers_upgrade = self
            .header("connection")
            .is_some_and(|value| header_list_contains(value, "upgrade"));

        // Each `Upgrade` element is a protocol name with an optional
        // version (`websocket`, `HTTP/2.0`), so compare the name alone.
        let offers_websocket = self.header("upgrade").is_some_and(|value| {
            header_list_tokens(value).any(|protocol| {
                protocol
                    .split('/')
                    .next()
                    .is_some_and(|name| name.eq_ignore_ascii_case("websocket"))
            })
        });

        connection_offers_upgrade && offers_websocket
    }
}

/// Split an absolute-form request target into its authority and the
/// origin-form target that follows it.
///
/// Returns `None` for any target that is not absolute-form — origin-form
/// (`/path`), asterisk-form (`*`), and CONNECT's authority-form all fall
/// through unchanged.
///
/// Deliberately textual rather than routed through a URL parser: a
/// debugging proxy SHOULD hand the origin the bytes the client sent, and
/// URL normalization would quietly rewrite percent-encoding, host case,
/// and default ports along the way.
fn split_absolute_form(target: &str) -> Option<(&str, String)> {
    let rest = strip_scheme(target)?;

    // The authority runs up to the first path, query, or fragment
    // delimiter — or to the end, for a target like `http://host`.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, remainder) = rest.split_at(end);

    // `userinfo@` MUST NOT appear in a Host header (RFC 7230 §5.3.2).
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_userinfo, host)| host);
    if authority.is_empty() {
        return None;
    }

    // An empty path goes out as "/" (RFC 7230 §5.3.1). A bare query or
    // fragment gets the same treatment rather than being dropped.
    let origin_form = if remainder.starts_with('/') {
        remainder.to_owned()
    } else {
        format!("/{remainder}")
    };

    Some((authority, origin_form))
}

/// Strip a leading `http://` or `https://`, case-insensitively.
///
/// Compares bytes rather than slicing the `str`, so a target that is not
/// ASCII at the front cannot panic on a char boundary.
fn strip_scheme(target: &str) -> Option<&str> {
    ["http://", "https://"].into_iter().find_map(|scheme| {
        let bytes = target.as_bytes();
        (bytes.len() >= scheme.len()
            && bytes[..scheme.len()].eq_ignore_ascii_case(scheme.as_bytes()))
        .then(|| &target[scheme.len()..])
    })
}

/// Parse a host:port authority string.
fn parse_authority(authority: &str, default_port: u16) -> Result<(String, u16)> {
    // Handle IPv6 addresses like [::1]:8080
    if authority.starts_with('[')
        && let Some(bracket_end) = authority.find(']')
    {
        let host = authority[1..bracket_end].to_owned();
        let port = if authority.len() > bracket_end + 2
            && &authority[bracket_end + 1..bracket_end + 2] == ":"
        {
            authority[bracket_end + 2..]
                .parse()
                .map_err(|_e| Error::InvalidRequest(format!("invalid port in {authority}")))?
        } else {
            default_port
        };
        return Ok((host, port));
    }

    // Regular host:port or just host
    if let Some(colon) = authority.rfind(':') {
        // Make sure this isn't part of an IPv6 address without brackets
        let before_colon = &authority[..colon];
        if !before_colon.contains(':') {
            let host = before_colon.to_owned();
            let port = authority[colon + 1..]
                .parse()
                .map_err(|_e| Error::InvalidRequest(format!("invalid port in {authority}")))?;
            return Ok((host, port));
        }
    }

    Ok((authority.to_owned(), default_port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> ParsedRequest {
        ParsedRequest::parse(raw.as_bytes())
            .expect("parses")
            .expect("complete")
    }

    #[test]
    fn test_rewrite_absolute_form_to_origin_form() {
        let mut req =
            parse("GET http://example.com/path?q=1 HTTP/1.1\r\nHost: example.com\r\n\r\n");

        assert!(req.rewrite_to_origin_form());
        assert_eq!(req.target, "/path?q=1");
        assert_eq!(req.host(), Some("example.com"));
    }

    #[test]
    fn test_rewrite_prefers_the_target_authority_over_the_host_header() {
        // RFC 7230 §5.4: the authority in the target outranks `Host`.
        // Dropping the authority without moving it into `Host` would
        // hand the origin a request for a different site.
        let mut req = parse("GET http://real.example/p HTTP/1.1\r\nHost: spoofed.example\r\n\r\n");

        // Routing agrees with the target even before the rewrite.
        assert_eq!(req.host_port().expect("host_port").0, "real.example");

        assert!(req.rewrite_to_origin_form());
        assert_eq!(req.target, "/p");
        assert_eq!(req.host(), Some("real.example"));
        assert_eq!(req.host_port().expect("host_port").0, "real.example");
    }

    #[test]
    fn test_rewrite_collapses_duplicate_host_headers() {
        let mut req = parse(
            "GET http://example.com/p HTTP/1.1\r\nHost: a.example\r\nHost: b.example\r\n\r\n",
        );

        req.rewrite_to_origin_form();

        let hosts: Vec<&str> = req
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(hosts, vec!["example.com"], "exactly one Host survives");
    }

    #[test]
    fn test_rewrite_adds_host_when_the_client_sent_none() {
        // Legal in absolute-form under HTTP/1.0; the origin still needs one.
        let mut req = parse("GET http://example.com/p HTTP/1.0\r\n\r\n");

        assert!(req.rewrite_to_origin_form());
        assert_eq!(req.host(), Some("example.com"));
    }

    #[test]
    fn test_rewrite_empty_path_becomes_slash() {
        let mut req = parse("GET http://example.com HTTP/1.1\r\nHost: example.com\r\n\r\n");

        assert!(req.rewrite_to_origin_form());
        assert_eq!(req.target, "/");
    }

    #[test]
    fn test_rewrite_bare_query_keeps_the_query() {
        let mut req = parse("GET http://example.com?q=1 HTTP/1.1\r\nHost: example.com\r\n\r\n");

        assert!(req.rewrite_to_origin_form());
        assert_eq!(req.target, "/?q=1");
    }

    #[test]
    fn test_rewrite_strips_userinfo_from_the_host_header() {
        let mut req =
            parse("GET http://user:pw@example.com/p HTTP/1.1\r\nHost: example.com\r\n\r\n");

        assert!(req.rewrite_to_origin_form());
        assert_eq!(req.target, "/p");
        assert_eq!(
            req.host(),
            Some("example.com"),
            "userinfo must not reach the Host header"
        );
    }

    #[test]
    fn test_rewrite_keeps_ipv6_authority_and_port() {
        let mut req = parse("GET http://[::1]:8080/p HTTP/1.1\r\nHost: [::1]:8080\r\n\r\n");

        assert!(req.rewrite_to_origin_form());
        assert_eq!(req.target, "/p");
        assert_eq!(req.host(), Some("[::1]:8080"));
        let (host, port) = req.host_port().expect("host_port");
        assert_eq!((host.as_str(), port), ("::1", 8080));
    }

    #[test]
    fn test_rewrite_handles_uppercase_scheme() {
        let mut req = parse("GET HTTP://example.com/p HTTP/1.1\r\nHost: example.com\r\n\r\n");

        assert!(req.rewrite_to_origin_form());
        assert_eq!(req.target, "/p");
    }

    #[test]
    fn test_rewrite_handles_https_absolute_form() {
        let mut req = parse("GET https://example.com/p HTTP/1.1\r\nHost: example.com\r\n\r\n");

        assert!(req.rewrite_to_origin_form());
        assert_eq!(req.target, "/p");
    }

    #[test]
    fn test_rewrite_preserves_the_target_verbatim() {
        // No URL normalization: percent-encoding, host case, and an
        // explicit default port are all forwarded as the client wrote
        // them, because a debugging proxy that quietly rewrites requests
        // is lying to whoever is reading the capture.
        let mut req = parse(
            "GET http://Example.COM:80/a%2Fb//c?x=%20 HTTP/1.1\r\nHost: Example.COM:80\r\n\r\n",
        );

        assert!(req.rewrite_to_origin_form());
        assert_eq!(req.target, "/a%2Fb//c?x=%20");
        assert_eq!(req.host(), Some("Example.COM:80"));
    }

    #[test]
    fn test_rewrite_leaves_origin_form_alone() {
        let mut req = parse("GET /path HTTP/1.1\r\nHost: example.com\r\n\r\n");

        assert!(!req.rewrite_to_origin_form());
        assert_eq!(req.target, "/path");
        assert_eq!(req.host(), Some("example.com"));
    }

    #[test]
    fn test_rewrite_leaves_connect_and_asterisk_form_alone() {
        let mut connect =
            parse("CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n");
        assert!(!connect.rewrite_to_origin_form());
        assert_eq!(connect.target, "example.com:443");

        let mut options = parse("OPTIONS * HTTP/1.1\r\nHost: example.com\r\n\r\n");
        assert!(!options.rewrite_to_origin_form());
        assert_eq!(options.target, "*");
    }

    #[test]
    fn test_rewrite_ignores_a_scheme_only_target() {
        // No authority to move anywhere; leave the request untouched
        // rather than inventing a Host.
        let mut req = parse("GET http:// HTTP/1.1\r\nHost: example.com\r\n\r\n");

        assert!(!req.rewrite_to_origin_form());
        assert_eq!(req.host(), Some("example.com"));
    }

    #[test]
    fn test_rewrite_handles_non_ascii_target_without_panicking() {
        // The scheme check compares bytes, so a short multi-byte target
        // cannot split a char boundary.
        let mut req = parse("GET /é HTTP/1.1\r\nHost: example.com\r\n\r\n");

        assert!(!req.rewrite_to_origin_form());
        assert_eq!(req.target, "/é");
    }

    #[test]
    fn test_parse_connect_request() {
        let req = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n";
        let parsed = ParsedRequest::parse(req).unwrap().unwrap();

        assert!(parsed.is_connect());
        assert_eq!(parsed.target, "example.com:443");

        let (host, port) = parsed.host_port().unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

    #[test]
    fn test_parse_get_request() {
        let req = b"GET /path HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let parsed = ParsedRequest::parse(req).unwrap().unwrap();

        assert!(!parsed.is_connect());
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.target, "/path");

        let (host, port) = parsed.host_port().unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
    }

    #[test]
    fn test_parse_authority() {
        assert_eq!(
            parse_authority("example.com", 80).unwrap(),
            ("example.com".to_owned(), 80)
        );
        assert_eq!(
            parse_authority("example.com:8080", 80).unwrap(),
            ("example.com".to_owned(), 8080)
        );
        assert_eq!(
            parse_authority("[::1]:8080", 80).unwrap(),
            ("::1".to_owned(), 8080)
        );
    }

    #[test]
    fn test_incomplete_request() {
        let req = b"GET /path HTTP/1.1\r\nHost: exam";
        let parsed = ParsedRequest::parse(req).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn test_websocket_upgrade_detection() {
        let req = b"GET /ws HTTP/1.1\r\nHost: example.com\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
        let parsed = ParsedRequest::parse(req).unwrap().unwrap();

        assert!(parsed.is_websocket_upgrade());
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.target, "/ws");
    }

    #[test]
    fn test_non_websocket_request() {
        let req = b"GET /path HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let parsed = ParsedRequest::parse(req).unwrap().unwrap();

        assert!(!parsed.is_websocket_upgrade());
    }

    #[test]
    fn test_websocket_upgrade_rejects_a_connection_token_that_merely_contains_upgrade() {
        // `x-upgrade-hint` is not `upgrade`. Matching it would hand an
        // ordinary request to the tunnel code, which stops parsing HTTP
        // the moment it takes over.
        let parsed = parse(
            "GET /path HTTP/1.1\r\nHost: example.com\r\nConnection: keep-alive, \
             x-upgrade-hint\r\nUpgrade: websocket\r\n\r\n",
        );

        assert!(!parsed.is_websocket_upgrade());
    }

    #[test]
    fn test_websocket_upgrade_accepts_upgrade_among_other_connection_tokens() {
        let parsed = parse(
            "GET /ws HTTP/1.1\r\nHost: example.com\r\nConnection: keep-alive, Upgrade\r\nUpgrade: \
             websocket\r\n\r\n",
        );

        assert!(parsed.is_websocket_upgrade());
    }

    #[test]
    fn test_websocket_upgrade_accepts_websocket_among_other_offered_protocols() {
        // `Upgrade` is `1#protocol`, and each protocol may carry a
        // version — neither ruled out an exact-equality check before.
        let parsed = parse(
            "GET /ws HTTP/1.1\r\nHost: example.com\r\nConnection: Upgrade\r\nUpgrade: websocket, \
             HTTP/2.0\r\n\r\n",
        );

        assert!(parsed.is_websocket_upgrade());
    }

    #[test]
    fn test_websocket_upgrade_rejects_a_different_protocol() {
        let parsed = parse(
            "GET /x HTTP/1.1\r\nHost: example.com\r\nConnection: Upgrade\r\nUpgrade: h2c\r\n\r\n",
        );

        assert!(!parsed.is_websocket_upgrade());
    }

    #[test]
    fn test_websocket_upgrade_requires_both_headers() {
        let connection_only =
            parse("GET /x HTTP/1.1\r\nHost: example.com\r\nConnection: Upgrade\r\n\r\n");
        assert!(!connection_only.is_websocket_upgrade());

        let upgrade_only =
            parse("GET /x HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\n\r\n");
        assert!(!upgrade_only.is_websocket_upgrade());
    }

    #[test]
    fn test_is_chunked_plain() {
        let req = b"POST /x HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\n\r\n";
        let parsed = ParsedRequest::parse(req).unwrap().unwrap();
        assert!(parsed.is_chunked());
    }

    #[test]
    fn test_is_chunked_stacked_encoding() {
        let req =
            b"POST /x HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: gzip, chunked\r\n\r\n";
        let parsed = ParsedRequest::parse(req).unwrap().unwrap();
        assert!(parsed.is_chunked());
    }

    #[test]
    fn test_is_chunked_rejects_substring() {
        let req =
            b"POST /x HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked-foo\r\n\r\n";
        let parsed = ParsedRequest::parse(req).unwrap().unwrap();
        assert!(!parsed.is_chunked());
    }
}
