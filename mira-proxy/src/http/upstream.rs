//! Sending a request to an upstream connection and reading back the
//! response head.
//!
//! Both proxy paths need the same step — write the request head, write
//! the body, read until the response headers parse — and both need to
//! know whether a failure happened *before* any response byte arrived.
//! That distinction is what makes a retry on a fresh connection safe:
//! the origin either never saw the request or produced nothing for it,
//! so replaying it cannot double-process a non-idempotent method, and
//! nothing has been written to the client yet.

use crate::http::ParsedRequest;
use crate::http::ParsedResponse;
use crate::http::header_list_contains;
use crate::http::write_all_within;
use bytes::Buf;
use bytes::Bytes;
use bytes::BytesMut;
use std::time::Duration;
use std::time::Instant;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::time::timeout;
use tracing::trace;

/// Largest request body sent in the same write as its head (64 KB).
const COALESCE_BODY_LIMIT: usize = 64 * 1024;

/// A request written upstream whose response headers have been parsed.
pub struct RequestSent {
    /// Everything read off the upstream so far — the response head plus
    /// whatever body bytes arrived alongside it.
    pub response_buf: BytesMut,
    /// The parsed response head.
    pub parsed_response: ParsedResponse,
    /// When the request finished going out.
    pub sent: Instant,
    /// When the first response byte came back.
    pub first_byte: Instant,
}

/// A failed attempt to send a request upstream.
pub struct SendFailed {
    /// What went wrong.
    pub error: crate::Error,
    /// Whether any response byte was read before the failure. When
    /// `false`, the request may be safely retried on a fresh connection.
    pub saw_response_bytes: bool,
}

/// Write a request to `upstream` and read until the response head parses.
///
/// Each read is bounded by `read_timeout` and the request write by
/// `write_timeout`; on a read expiry the error is tagged with
/// `timeout_op` so logs distinguish plain-HTTP from intercepted-HTTPS
/// stalls. A response head that grows past
/// `max_header_size` without terminating is rejected rather than
/// buffered — an origin can drip headers just as a client can.
pub async fn write_request_and_parse_head<U>(
    upstream: &mut U,
    header_bytes: &[u8],
    request_body: &Bytes,
    read_timeout: Duration,
    write_timeout: Duration,
    max_header_size: usize,
    timeout_op: &'static str,
) -> Result<RequestSent, SendFailed>
where
    U: AsyncRead + AsyncWrite + Unpin,
{
    // A write failure means the request never landed, so it is always
    // safe to retry regardless of how far the write got.
    //
    // A small body goes out in the same write as the head. Two small
    // writes back to back can stall on Nagle's algorithm meeting the
    // origin's delayed ACK.
    const OP: &str = "write request upstream";
    let written = if request_body.is_empty() {
        write_all_within(upstream, header_bytes, write_timeout, OP).await
    } else if request_body.len() <= COALESCE_BODY_LIMIT {
        let mut request = Vec::with_capacity(header_bytes.len() + request_body.len());
        request.extend_from_slice(header_bytes);
        request.extend_from_slice(request_body);
        write_all_within(upstream, &request, write_timeout, OP).await
    } else {
        match write_all_within(upstream, header_bytes, write_timeout, OP).await {
            Ok(()) => write_all_within(upstream, request_body, write_timeout, OP).await,
            Err(e) => Err(e),
        }
    };
    if let Err(error) = written {
        return Err(SendFailed {
            error,
            saw_response_bytes: false,
        });
    }
    let sent = Instant::now();

    let mut response_buf = BytesMut::with_capacity(65536);
    let mut first_byte_time: Option<Instant> = None;
    let mut saw_response_bytes = false;

    let parsed_response = loop {
        // Parse before checking the size: a complete head can arrive in
        // the same read as enough body to push the buffer past the cap.
        match ParsedResponse::parse_headers(&response_buf) {
            Ok(Some(head)) if is_interim(head.status) => {
                // RFC 9110 §15.2: a 1xx precedes the real response. Taking
                // it as final leaves the real one unread on the connection,
                // where the next request on that connection receives it.
                trace!(status = head.status, "skipping interim response");
                response_buf.advance(head.header_len);
                continue;
            }
            Ok(Some(head)) => break head,
            Ok(None) => {}
            Err(error) => {
                return Err(SendFailed {
                    error,
                    saw_response_bytes: true,
                });
            }
        }

        if response_buf.len() > max_header_size {
            return Err(SendFailed {
                error: crate::Error::HeadersTooLarge {
                    size: response_buf.len(),
                    max: max_header_size,
                },
                saw_response_bytes: true,
            });
        }

        let read_future = upstream.read_buf(&mut response_buf);
        let n = match timeout(read_timeout, read_future).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                return Err(SendFailed {
                    error: e.into(),
                    saw_response_bytes,
                });
            }
            Err(_elapsed) => {
                return Err(SendFailed {
                    error: crate::Error::Timeout {
                        operation: timeout_op.into(),
                        duration: read_timeout,
                    },
                    saw_response_bytes,
                });
            }
        };
        if n == 0 {
            return Err(SendFailed {
                error: crate::Error::InvalidResponse("connection closed before response".into()),
                saw_response_bytes,
            });
        }
        saw_response_bytes = true;
        first_byte_time.get_or_insert_with(Instant::now);
    };

    let first_byte = first_byte_time.unwrap_or_else(Instant::now);

    Ok(RequestSent {
        response_buf,
        parsed_response,
        sent,
        first_byte,
    })
}

/// Whether `status` is an interim response that a final one follows.
///
/// `101 Switching Protocols` is final: the connection stops speaking
/// HTTP after it.
fn is_interim(status: u16) -> bool {
    (100..200).contains(&status) && status != 101
}

/// Whether the client connection can carry another request once this
/// one is answered.
///
/// An HTTP/1.0 client is always closed on: Mira strips the `Connection`
/// header it would need to see to keep going.
pub fn client_keeps_alive(request: &ParsedRequest) -> bool {
    request.version != "HTTP/1.0"
        && !request
            .header("connection")
            .is_some_and(|value| header_list_contains(value, "close"))
}

/// Determine if an upstream connection can carry another request, based
/// on the HTTP keep-alive semantics of the exchange that just completed.
///
/// `Connection` is a token list, so `close` is looked for as an element
/// rather than as the whole value — `Connection: keep-alive, close` ends
/// the connection just as plainly as `Connection: close` does.
pub fn can_reuse_connection(response: &ParsedResponse, request: &ParsedRequest) -> bool {
    // Either side can call an end to the connection.
    if response
        .header("connection")
        .is_some_and(|value| header_list_contains(value, "close"))
    {
        return false;
    }

    if request
        .header("connection")
        .is_some_and(|value| header_list_contains(value, "close"))
    {
        return false;
    }

    // HTTP/1.1 defaults to keep-alive, HTTP/1.0 defaults to close
    if response.version.contains("1.0") {
        response
            .header("connection")
            .is_some_and(|value| header_list_contains(value, "keep-alive"))
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::io::duplex;

    fn parsed_request(extra_headers: &str) -> ParsedRequest {
        let req = format!("GET / HTTP/1.1\r\nHost: x\r\n{extra_headers}\r\n");
        ParsedRequest::parse(&BytesMut::from(req.as_bytes()))
            .expect("parses")
            .expect("complete")
    }

    fn parsed_response(head: &str) -> ParsedResponse {
        ParsedResponse::parse_headers(&BytesMut::from(head.as_bytes()))
            .expect("parses")
            .expect("complete")
    }

    #[tokio::test]
    async fn test_write_request_and_parse_head_returns_head_and_leftover_body() {
        let (mut origin, mut upstream) = duplex(4096);

        const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";

        let sender = tokio::spawn(async move {
            write_request_and_parse_head(
                &mut upstream,
                REQUEST,
                &Bytes::new(),
                Duration::from_secs(2),
                Duration::from_secs(2),
                8192,
                "test",
            )
            .await
            .map_err(|f| f.error)
        });

        let mut seen = vec![0u8; REQUEST.len()];
        origin.read_exact(&mut seen).await.expect("read request");
        assert_eq!(seen, REQUEST);
        origin
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
            .await
            .expect("write response");

        let sent = sender.await.expect("joined").expect("send ok");
        assert_eq!(sent.parsed_response.status, 200);
        // Body bytes that arrived with the head stay in the buffer for
        // the caller to relay.
        assert_eq!(
            &sent.response_buf[sent.parsed_response.header_len..],
            b"hello"
        );
    }

    #[tokio::test]
    async fn test_write_request_and_parse_head_skips_interim_responses() {
        // `100 Continue` and `103 Early Hints` come before the real
        // answer. Stopping at the first leaves the `200` on the
        // connection for the next request to receive.
        let (mut origin, mut upstream) = duplex(4096);
        origin
            .write_all(
                b"HTTP/1.1 100 Continue\r\n\r\n\
                  HTTP/1.1 103 Early Hints\r\nLink: </a.css>\r\n\r\n\
                  HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            )
            .await
            .expect("write responses");

        let sent = write_request_and_parse_head(
            &mut upstream,
            b"POST / HTTP/1.1\r\nHost: x\r\n\r\n",
            &Bytes::new(),
            Duration::from_secs(2),
            Duration::from_secs(2),
            8192,
            "test",
        )
        .await
        .map_err(|f| f.error)
        .expect("send ok");

        assert_eq!(sent.parsed_response.status, 200);
        assert_eq!(&sent.response_buf[sent.parsed_response.header_len..], b"ok");
    }

    #[tokio::test]
    async fn test_write_request_and_parse_head_accepts_a_head_that_arrives_with_a_large_body() {
        // The size cap is for heads that never end, not for a short head
        // that shares a read with plenty of body.
        let (mut origin, mut upstream) = duplex(16 * 1024);
        let mut response = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\n\r\n".to_vec();
        response.extend_from_slice(&[b'x'; 4096]);
        origin.write_all(&response).await.expect("write response");

        let sent = write_request_and_parse_head(
            &mut upstream,
            b"GET / HTTP/1.1\r\nHost: x\r\n\r\n",
            &Bytes::new(),
            Duration::from_secs(2),
            Duration::from_secs(2),
            1024,
            "test",
        )
        .await
        .map_err(|f| f.error)
        .expect("send ok");

        assert_eq!(sent.parsed_response.status, 200);
    }

    #[test]
    fn test_client_keeps_alive() {
        assert!(client_keeps_alive(&parsed_request("")));
        assert!(!client_keeps_alive(&parsed_request(
            "Connection: close\r\n"
        )));

        let http10 = ParsedRequest::parse(b"GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n")
            .expect("parses")
            .expect("complete");
        assert!(!client_keeps_alive(&http10));
    }

    #[tokio::test]
    async fn test_write_request_and_parse_head_reports_retryable_on_closed_connection() {
        let (origin, mut upstream) = duplex(4096);
        // Origin closes without answering — a stale keep-alive socket.
        drop(origin);

        let failure = write_request_and_parse_head(
            &mut upstream,
            b"POST / HTTP/1.1\r\nHost: x\r\n\r\n",
            &Bytes::new(),
            Duration::from_secs(2),
            Duration::from_secs(2),
            8192,
            "test",
        )
        .await
        .err()
        .expect("should fail");

        assert!(
            !failure.saw_response_bytes,
            "no response byte arrived, so the request is safe to retry: {}",
            failure.error
        );
    }

    #[tokio::test]
    async fn test_write_request_and_parse_head_reports_partial_response_as_unretryable() {
        let (mut origin, mut upstream) = duplex(4096);

        const REQUEST: &[u8] = b"POST / HTTP/1.1\r\nHost: x\r\n\r\n";

        let sender = tokio::spawn(async move {
            write_request_and_parse_head(
                &mut upstream,
                REQUEST,
                &Bytes::new(),
                Duration::from_secs(2),
                Duration::from_secs(2),
                8192,
                "test",
            )
            .await
            .err()
            .map(|f| f.saw_response_bytes)
        });

        let mut seen = vec![0u8; REQUEST.len()];
        origin.read_exact(&mut seen).await.expect("read request");
        // Half a response head, then a close: the origin acted on the
        // request, so replaying it would double-process.
        origin
            .write_all(b"HTTP/1.1 200 OK\r\n")
            .await
            .expect("write partial");
        drop(origin);

        let saw_bytes = sender.await.expect("joined").expect("should fail");
        assert!(saw_bytes, "partial response must not be retried");
    }

    #[tokio::test]
    async fn test_write_request_and_parse_head_caps_an_endless_response_head() {
        let (mut origin, mut upstream) = duplex(4096);

        // An origin that answers with headers it never terminates. The
        // proxy must not buffer them for the whole read timeout.
        let flood = tokio::spawn(async move {
            origin
                .write_all(b"HTTP/1.1 200 OK\r\n")
                .await
                .expect("write status line");
            let line = format!("X-Pad: {}\r\n", "a".repeat(512));
            while origin.write_all(line.as_bytes()).await.is_ok() {}
        });

        let failure = write_request_and_parse_head(
            &mut upstream,
            b"GET / HTTP/1.1\r\nHost: x\r\n\r\n",
            &Bytes::new(),
            Duration::from_secs(30),
            Duration::from_secs(30),
            8192,
            "test",
        )
        .await
        .err()
        .expect("should fail");

        assert!(
            matches!(failure.error, crate::Error::HeadersTooLarge { .. }),
            "got: {}",
            failure.error
        );
        flood.abort();
    }

    #[test]
    fn test_can_reuse_connection_defaults_to_keep_alive_on_http_1_1() {
        assert!(can_reuse_connection(
            &parsed_response("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"),
            &parsed_request(""),
        ));
    }

    #[test]
    fn test_can_reuse_connection_honours_close_from_either_side() {
        assert!(!can_reuse_connection(
            &parsed_response("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n"),
            &parsed_request(""),
        ));
        assert!(!can_reuse_connection(
            &parsed_response("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"),
            &parsed_request("Connection: close\r\n"),
        ));
    }

    #[test]
    fn test_can_reuse_connection_sees_close_among_other_tokens() {
        // `Connection` is a token list; comparing the whole value would
        // miss the close and pool a socket the peer is about to shut.
        assert!(!can_reuse_connection(
            &parsed_response("HTTP/1.1 200 OK\r\nConnection: keep-alive, close\r\n\r\n"),
            &parsed_request(""),
        ));
        assert!(!can_reuse_connection(
            &parsed_response("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"),
            &parsed_request("Connection: TE, close\r\n"),
        ));
    }

    #[test]
    fn test_can_reuse_connection_ignores_tokens_that_merely_contain_close() {
        assert!(can_reuse_connection(
            &parsed_response("HTTP/1.1 200 OK\r\nConnection: x-not-close\r\n\r\n"),
            &parsed_request(""),
        ));
    }

    #[test]
    fn test_can_reuse_connection_requires_explicit_keep_alive_on_http_1_0() {
        assert!(!can_reuse_connection(
            &parsed_response("HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n"),
            &parsed_request(""),
        ));
        assert!(can_reuse_connection(
            &parsed_response("HTTP/1.0 200 OK\r\nConnection: keep-alive\r\n\r\n"),
            &parsed_request(""),
        ));
    }
}
