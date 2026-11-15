//! Reading a request head off a client stream.
//!
//! Shared by the plain-HTTP path and the intercepted-HTTPS path, which
//! face the same client and therefore the same two abuses: a byte-drip
//! client that never finishes its headers, and one that sends an endless
//! header block. The deadline is a *total* one rather than per-read, so
//! dribbling bytes cannot reset the clock, and the size cap bounds what
//! a connection can make the proxy hold before the deadline fires.

use crate::Result;
use crate::http::ParsedRequest;
use bytes::BytesMut;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tracing::trace;

/// How an attempt to read a request head ended.
#[derive(Debug)]
pub enum HeadRead {
    /// A complete request head was parsed out of the buffer.
    Parsed(ParsedRequest),

    /// The client closed the connection.
    Closed,

    /// The deadline expired first.
    TimedOut {
        /// Bytes of a partial head in the buffer: zero for an ordinary
        /// idle keep-alive timeout, non-zero for a byte-drip (slowloris)
        /// client.
        buffered: usize,
    },

    /// The head outgrew `max_header_size` without terminating, or
    /// carried more header fields than the parser holds.
    TooLarge {
        /// Bytes buffered when the limit was hit.
        buffered: usize,
    },

    /// The head is not valid HTTP.
    Malformed {
        /// Why, for the client's `400`.
        reason: String,
    },
}

/// What one parse attempt over the buffer found.
fn parse_head(buf: &[u8]) -> Option<HeadRead> {
    match ParsedRequest::parse(buf) {
        Ok(Some(request)) => Some(HeadRead::Parsed(request)),
        Ok(None) => None,
        Err(crate::Error::TooManyHeaders { .. }) => Some(HeadRead::TooLarge {
            buffered: buf.len(),
        }),
        Err(e) => Some(HeadRead::Malformed {
            reason: e.to_string(),
        }),
    }
}

/// Read from `client` until a complete request head has been parsed.
///
/// Returns as soon as the buffer already holds a complete head, so
/// pipelined or keep-alive leftovers are picked up without a read. A peer
/// reset mid-read counts as [`HeadRead::Closed`] rather than an error, and
/// a head that does not parse is [`HeadRead::Malformed`]; other I/O
/// errors propagate.
pub async fn read_request_head<C>(
    client: &mut C,
    buf: &mut BytesMut,
    header_read_timeout: Duration,
    max_header_size: usize,
) -> Result<HeadRead>
where
    C: AsyncRead + Unpin,
{
    // Leftover bytes from a prior request on a keep-alive connection may
    // already form a complete request before we even read.
    if !buf.is_empty()
        && let Some(outcome) = parse_head(buf)
    {
        return Ok(outcome);
    }

    let deadline = tokio::time::Instant::now() + header_read_timeout;

    loop {
        // Check before reading too: leftovers alone can already be over
        // the cap without a complete head in them.
        if buf.len() > max_header_size {
            return Ok(HeadRead::TooLarge {
                buffered: buf.len(),
            });
        }

        let read_future = client.read_buf(buf);
        match tokio::time::timeout_at(deadline, read_future).await {
            Ok(Ok(0)) => return Ok(HeadRead::Closed),
            Ok(Ok(n)) => trace!(bytes = n, total = buf.len(), "read head bytes"),
            Ok(Err(e)) => {
                // A peer that resets or truncates mid-read is closing,
                // not malfunctioning.
                if e.kind() == std::io::ErrorKind::ConnectionReset
                    || e.kind() == std::io::ErrorKind::UnexpectedEof
                {
                    return Ok(HeadRead::Closed);
                }
                return Err(e.into());
            }
            Err(_elapsed) => {
                return Ok(HeadRead::TimedOut {
                    buffered: buf.len(),
                });
            }
        }

        if let Some(outcome) = parse_head(buf) {
            return Ok(outcome);
        }
        // else: need more bytes — keep reading, subject to the same
        // deadline and the same cap.
    }
}

/// The response to send a client whose header block was too large.
///
/// RFC 6585 §5. The connection is unusable afterwards — the rest of the
/// oversized head is still in flight — so it announces a close.
pub const HEADERS_TOO_LARGE_RESPONSE: &[u8] = b"HTTP/1.1 431 Request Header Fields Too Large\r\n\
      Content-Length: 0\r\n\
      Connection: close\r\n\
      \r\n";

/// Send a response the proxy makes on its own behalf, then close.
///
/// The connection is ending either way, so a failure to deliver it is
/// not reported.
pub async fn send_proxy_error<W>(
    client: &mut W,
    status: u16,
    reason: &str,
    detail: &str,
    write_timeout: Duration,
) where
    W: AsyncWrite + Unpin,
{
    let response = proxy_error_response(status, reason, detail);
    let _ = tokio::time::timeout(write_timeout, async {
        client.write_all(&response).await?;
        client.shutdown().await
    })
    .await;
}

/// The status a client gets when a request could not be forwarded:
/// `400` when the request itself was at fault, `504` when the origin
/// timed out, `502` for everything else.
pub fn gateway_error_status(error: &crate::Error) -> (u16, &'static str) {
    match error {
        crate::Error::InvalidRequest(_) | crate::Error::InvalidUri(_) => (400, "Bad Request"),
        crate::Error::Timeout { .. } => (504, "Gateway Timeout"),
        _ => (502, "Bad Gateway"),
    }
}

/// Tell the client the origin failed, with a `502` or `504` carrying
/// the reason, then close.
pub async fn send_gateway_error<W>(client: &mut W, error: &crate::Error, write_timeout: Duration)
where
    W: AsyncWrite + Unpin,
{
    let (status, reason) = gateway_error_status(error);
    send_proxy_error(client, status, reason, &error.to_string(), write_timeout).await;
}

/// Build a response the proxy sends on its own behalf, ending the
/// connection.
///
/// `detail` becomes a plain-text body, so a client sees why the proxy
/// answered instead of the origin.
pub fn proxy_error_response(status: u16, reason: &str, detail: &str) -> Vec<u8> {
    let body = format!("{detail}\n");
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::io::duplex;

    const TIMEOUT: Duration = Duration::from_secs(2);

    #[tokio::test]
    async fn test_read_request_head_parses_a_complete_head() {
        let (mut peer, mut client) = duplex(4096);
        peer.write_all(b"GET /x HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .await
            .expect("write");

        let mut buf = BytesMut::new();
        let read = read_request_head(&mut client, &mut buf, TIMEOUT, 8192)
            .await
            .expect("ok");
        match read {
            HeadRead::Parsed(req) => assert_eq!(req.target, "/x"),
            other => panic!("expected a parsed head, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_read_request_head_reports_clean_close() {
        let (peer, mut client) = duplex(4096);
        drop(peer);

        let mut buf = BytesMut::new();
        let read = read_request_head(&mut client, &mut buf, TIMEOUT, 8192)
            .await
            .expect("ok");
        assert!(matches!(read, HeadRead::Closed), "got {read:?}");
    }

    #[tokio::test]
    async fn test_read_request_head_caps_an_endless_header_block() {
        let (mut peer, mut client) = duplex(4096);

        // A client that keeps sending well-formed headers and never
        // sends the terminator. Without a cap this grows until the read
        // timeout, once per connection.
        let flood = tokio::spawn(async move {
            let line = format!("X-Pad: {}\r\n", "a".repeat(512));
            loop {
                if peer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        let mut buf = BytesMut::from(&b"GET /x HTTP/1.1\r\n"[..]);
        let read = read_request_head(&mut client, &mut buf, TIMEOUT, 8192)
            .await
            .expect("ok");

        match read {
            HeadRead::TooLarge { buffered } => assert!(
                buffered > 8192,
                "should report what it had buffered, got {buffered}"
            ),
            other => panic!("expected the cap to trip, got {other:?}"),
        }
        // Well under what the same flood would have accumulated over the
        // full timeout.
        assert!(buf.len() < 8192 * 4, "buffer grew to {}", buf.len());

        drop(client);
        flood.await.expect("flood joined");
    }

    #[tokio::test]
    async fn test_read_request_head_times_out_on_a_byte_drip() {
        let (mut peer, mut client) = duplex(4096);
        peer.write_all(b"GET /x HTTP/1.1\r\n").await.expect("write");

        let mut buf = BytesMut::new();
        let read = read_request_head(&mut client, &mut buf, Duration::from_millis(50), 8192)
            .await
            .expect("ok");

        match read {
            HeadRead::TimedOut { buffered } => {
                assert_eq!(buffered, 17, "a partial head was buffered");
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
        drop(peer);
    }

    #[tokio::test]
    async fn test_read_request_head_reuses_buffered_leftovers() {
        // Two pipelined requests arrive together; the first is parsed
        // without touching the stream.
        let (peer, mut client) = duplex(4096);
        drop(peer);

        let mut buf = BytesMut::from(
            &b"GET /one HTTP/1.1\r\nHost: x\r\n\r\nGET /two HTTP/1.1\r\nHost: x\r\n\r\n"[..],
        );
        let read = read_request_head(&mut client, &mut buf, TIMEOUT, 8192)
            .await
            .expect("ok");
        match read {
            HeadRead::Parsed(req) => assert_eq!(req.target, "/one"),
            other => panic!("expected a parsed head, got {other:?}"),
        }
    }
}
