//! A captured request, as the curl command that sends it again.

use crate::text::is_binary;
use mira_proxy::Request;

/// Headers left out of the command.
///
/// curl sets `Host` and `Content-Length` itself, the connection headers only
/// concern the hop to the proxy, and Mira appends to `Via` and
/// `X-Forwarded-For` while forwarding.
const SKIPPED: [&str; 8] = [
    "host",
    "content-length",
    "connection",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "via",
    "x-forwarded-for",
];

/// A curl command, and whether a binary body was left out of it.
pub struct Curl {
    pub command: String,
    pub omitted_body: bool,
}

/// The curl command that repeats `request`.
pub fn command(request: &Request) -> Curl {
    let body = !request.body.is_empty();
    let omitted_body = body && is_binary(&request.body);
    let sends_body = body && !omitted_body;
    let mut parts = vec![format!("curl {}", quote(request.url.as_str()))];
    // curl sends GET, or POST once it has data; anything else is spelled out.
    let implied = if sends_body { "POST" } else { "GET" };
    if request.method != implied {
        parts.push(format!("-X {}", request.method));
    }
    for (name, value) in &request.headers {
        if !SKIPPED.contains(&name.to_ascii_lowercase().as_str()) {
            parts.push(format!("-H {}", quote(&format!("{name}: {value}"))));
        }
    }
    if sends_body {
        parts.push(format!(
            "--data-raw {}",
            quote(&String::from_utf8_lossy(&request.body))
        ));
    }
    Curl {
        command: parts.join(" \\\n  "),
        omitted_body,
    }
}

/// `text` in single quotes for a POSIX shell.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, headers: &[(&str, &str)], body: &'static [u8]) -> Request {
        Request {
            id: 1,
            client_addr: "127.0.0.1:5000".parse().expect("addr"),
            method: method.to_owned(),
            url: "http://example.com/a?b=1".parse().expect("url"),
            version: "HTTP/1.1".to_owned(),
            headers: headers
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect(),
            body: bytes::Bytes::from_static(body),
        }
    }

    #[test]
    fn test_get_keeps_its_own_headers_only() {
        let curl = command(&request(
            "GET",
            &[
                ("Host", "example.com"),
                ("Accept", "*/*"),
                ("Proxy-Connection", "Keep-Alive"),
                ("Via", "1.1 mira"),
            ],
            b"",
        ));
        assert_eq!(
            curl.command,
            "curl 'http://example.com/a?b=1' \\\n  -H 'Accept: */*'"
        );
        assert!(!curl.omitted_body);
    }

    #[test]
    fn test_post_body_is_quoted_and_implies_the_method() {
        let curl = command(&request("POST", &[], b"{\"it's\":1}"));
        assert_eq!(
            curl.command,
            "curl 'http://example.com/a?b=1' \\\n  --data-raw '{\"it'\\''s\":1}'"
        );
    }

    #[test]
    fn test_other_methods_and_binary_bodies() {
        let curl = command(&request("PUT", &[], b"\x00\x01\x02\x03"));
        assert_eq!(curl.command, "curl 'http://example.com/a?b=1' \\\n  -X PUT");
        assert!(curl.omitted_body);
    }
}
