//! Proxy-added forwarding headers: `Via` and `X-Forwarded-For`.
//!
//! Per RFC 7230 §5.7.1 a proxy MUST append its own identifier to the
//! `Via` header on both requests and responses. `X-Forwarded-For` is a
//! de-facto standard (predating RFC 7239's `Forwarded`) for recording
//! the originating client's IP on the request path.
//!
//! Existing values are preserved: when another proxy has already
//! populated `Via` or `X-Forwarded-For`, we append our entry to the
//! existing list rather than replacing it.

use std::net::IpAddr;

/// Pseudonym used in the `Via` header. Short and stable.
const VIA_PSEUDONYM: &str = "mira";

/// Append Mira's entries to the request's forwarding headers.
///
/// - `Via`: appended as `<version> mira` (e.g. `1.1 mira`).
/// - `X-Forwarded-For`: appended with the client's IP.
pub fn apply_request_proxy_headers(
    headers: &mut Vec<(String, String)>,
    http_version: &str,
    client_ip: IpAddr,
) {
    append_via(headers, http_version);
    append_x_forwarded_for(headers, client_ip);
}

/// Append Mira's entry to the response's `Via` header.
pub fn apply_response_proxy_headers(headers: &mut Vec<(String, String)>, http_version: &str) {
    append_via(headers, http_version);
}

fn append_via(headers: &mut Vec<(String, String)>, http_version: &str) {
    // `http_version` is typically "HTTP/1.1" or "HTTP/1.0"; the Via entry
    // uses just the version number.
    let version_number = http_version.strip_prefix("HTTP/").unwrap_or("1.1");
    let entry = format!("{version_number} {VIA_PSEUDONYM}");
    append_or_insert(headers, "Via", &entry);
}

fn append_x_forwarded_for(headers: &mut Vec<(String, String)>, client_ip: IpAddr) {
    let ip_str = client_ip.to_string();
    append_or_insert(headers, "X-Forwarded-For", &ip_str);
}

/// Append `entry` to the existing header of `name` (case-insensitive), or
/// insert a new header if one doesn't exist.
fn append_or_insert(headers: &mut Vec<(String, String)>, name: &str, entry: &str) {
    for (existing_name, value) in headers.iter_mut() {
        if existing_name.eq_ignore_ascii_case(name) {
            value.push_str(", ");
            value.push_str(entry);
            return;
        }
    }
    headers.push((name.to_owned(), entry.to_owned()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_contain(headers: &[(String, String)], name: &str, value: &str) -> bool {
        headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case(name) && v == value)
    }

    #[test]
    fn test_apply_request_proxy_headers_inserts_when_absent() {
        let mut headers = vec![("Host".into(), "example.com".into())];
        apply_request_proxy_headers(&mut headers, "HTTP/1.1", "203.0.113.5".parse().unwrap());

        assert!(headers_contain(&headers, "Via", "1.1 mira"));
        assert!(headers_contain(&headers, "X-Forwarded-For", "203.0.113.5"));
    }

    #[test]
    fn test_apply_request_proxy_headers_appends_to_existing() {
        let mut headers = vec![
            ("Host".into(), "example.com".into()),
            ("Via".into(), "1.0 upstream-proxy".into()),
            ("X-Forwarded-For".into(), "198.51.100.9".into()),
        ];
        apply_request_proxy_headers(&mut headers, "HTTP/1.1", "203.0.113.5".parse().unwrap());

        assert!(headers_contain(
            &headers,
            "Via",
            "1.0 upstream-proxy, 1.1 mira"
        ));
        assert!(headers_contain(
            &headers,
            "X-Forwarded-For",
            "198.51.100.9, 203.0.113.5"
        ));
        // We must not create duplicates.
        assert_eq!(
            headers.iter().filter(|(n, _)| n == "Via").count(),
            1,
            "should append to existing Via, not duplicate"
        );
        assert_eq!(
            headers
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case("x-forwarded-for"))
                .count(),
            1,
            "should append to existing X-Forwarded-For, not duplicate"
        );
    }

    #[test]
    fn test_apply_request_proxy_headers_handles_case_insensitivity() {
        let mut headers = vec![("via".into(), "1.1 some-proxy".into())];
        apply_request_proxy_headers(&mut headers, "HTTP/1.1", "203.0.113.5".parse().unwrap());
        // Existing lowercase header is extended — not shadowed.
        assert!(headers_contain(&headers, "via", "1.1 some-proxy, 1.1 mira"));
    }

    #[test]
    fn test_apply_response_proxy_headers_adds_via_only() {
        let mut headers = vec![("Content-Type".into(), "text/plain".into())];
        apply_response_proxy_headers(&mut headers, "HTTP/1.1");

        assert!(headers_contain(&headers, "Via", "1.1 mira"));
        // XFF is a request-direction header and must not appear in the response.
        assert!(
            !headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("x-forwarded-for"))
        );
    }

    #[test]
    fn test_apply_proxy_headers_handles_ipv6_client() {
        let mut headers = Vec::new();
        let ip: IpAddr = "2001:db8::1".parse().unwrap();
        apply_request_proxy_headers(&mut headers, "HTTP/1.1", ip);
        assert!(headers_contain(&headers, "X-Forwarded-For", "2001:db8::1"));
    }

    #[test]
    fn test_via_uses_http_1_0_when_request_is_1_0() {
        let mut headers = Vec::new();
        apply_request_proxy_headers(&mut headers, "HTTP/1.0", "127.0.0.1".parse().unwrap());
        assert!(headers_contain(&headers, "Via", "1.0 mira"));
    }
}
