//! Shared body reading and forwarding helpers.
//!
//! The plain-HTTP path and the intercepted-HTTPS path need the same
//! Content-Length / Transfer-Encoding / read-until-close loop when
//! pulling request bodies from clients and relaying response bodies to
//! them. Keeping one copy here means any future framing bug fix (chunked
//! parsing, size enforcement, timeout handling) lands in one place.

use crate::Result;
use crate::http::ParsedRequest;
use crate::http::ParsedResponse;
use crate::http::chunked_body_end;
use crate::http::header_list_contains;
use bytes::Bytes;
use bytes::BytesMut;
use std::future::Future;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;
use tracing::warn;

const READ_CHUNK_SIZE: usize = 8192;

/// How a response body is framed on the wire.
#[derive(Debug, Clone, Copy)]
pub enum BodyFraming {
    /// Exact byte length from Content-Length.
    ContentLength(usize),

    /// Transfer-Encoding: chunked, terminated by a zero-length chunk.
    Chunked,

    /// No framing header — read until the peer closes (HTTP/1.0-style).
    UntilClose,
}

impl BodyFraming {
    /// Pick the framing a response body uses (RFC 9112 §6.3): chunked,
    /// else an explicit `Content-Length`, else delimited by the
    /// connection closing.
    ///
    /// Chunked wins when both headers are present. The RFC requires it,
    /// and reading by the wrong one desyncs the connection.
    ///
    /// Callers MUST rule out the bodyless statuses first — see
    /// [`crate::http::response_has_no_body`] — since a `204` or `304`
    /// carries no body no matter what its headers say.
    pub fn for_response(response: &ParsedResponse) -> Self {
        if response.is_chunked() {
            Self::Chunked
        } else if let Some(n) = response.content_length() {
            Self::ContentLength(n)
        } else {
            Self::UntilClose
        }
    }
}

/// A response body that has been relayed to the client.
///
/// Carries how the relay ended, not just the bytes: an upstream that
/// closes mid-body leaves a dead socket behind, and the caller MUST know
/// that before it considers keeping the connection around.
#[derive(Debug)]
pub struct ForwardedBody {
    bytes: Bytes,
    framing: BodyFraming,

    /// Whether the framing was satisfied before the read loop ended.
    complete: bool,
}

impl ForwardedBody {
    /// The empty body of a response that carries none by definition
    /// (HEAD, 1xx, 204, 304) — nothing to read, so nothing can truncate.
    pub fn none() -> Self {
        Self {
            bytes: Bytes::new(),
            framing: BodyFraming::ContentLength(0),
            complete: true,
        }
    }

    /// Whether the whole body arrived. `false` means the upstream closed
    /// mid-body, so what the client received is truncated.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Whether the upstream connection can carry another request.
    ///
    /// A truncated body means the socket is already gone. So does an
    /// unframed body: with no Content-Length and no chunking, the close
    /// *is* the delimiter, complete or not.
    pub fn leaves_connection_usable(&self) -> bool {
        self.complete && !matches!(self.framing, BodyFraming::UntilClose)
    }

    /// The captured bytes, however far they got.
    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }
}

/// Read a request body off a client stream into `Bytes`.
///
/// Consumes exactly the bytes the request's framing claims — the
/// Content-Length count, or the chunked body up to its terminator — and
/// leaves anything beyond it in `buf`, after the head. On a keep-alive
/// connection those trailing bytes are the start of a pipelined request,
/// and folding them into this body would both lose that request and
/// smuggle its bytes upstream as body content.
///
/// A request with neither `Content-Length` nor `Transfer-Encoding:
/// chunked` has no body at all (RFC 7230 §3.3.3 rule 6), whatever its
/// method: only responses may be delimited by the connection closing.
/// Chunked framing wins over `Content-Length` when both are present.
///
/// Returns [`crate::Error::BodyTooLarge`] once `max_body_size` is
/// exceeded. A client that hangs up mid-body yields the partial body
/// rather than an error.
pub async fn read_body<R>(
    reader: &mut R,
    buf: &mut BytesMut,
    parsed_request: &ParsedRequest,
    max_body_size: usize,
) -> Result<Bytes>
where
    R: AsyncRead + Unpin,
{
    // Everything the client sent after the head. It may hold this
    // request's body, the start of the next one, or both; the framing
    // decides where the split falls.
    let mut pending = if buf.len() > parsed_request.header_len {
        buf.split_off(parsed_request.header_len)
    } else {
        BytesMut::new()
    };

    let body_len = if parsed_request.is_chunked() {
        read_chunked_body(reader, &mut pending, max_body_size).await?
    } else if let Some(content_length) = parsed_request.content_length() {
        read_content_length_body(reader, &mut pending, content_length, max_body_size).await?
    } else {
        0
    };

    let body = pending.split_to(body_len).freeze();

    // Hand back what was not this request's body, so the next read finds
    // the pipelined request instead of losing it.
    buf.unsplit(pending);

    Ok(body)
}

/// The interim response that tells a client to go ahead with its body.
const CONTINUE_RESPONSE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// Answer an `Expect: 100-continue` request before its body is read.
///
/// The proxy buffers the whole body before it forwards anything, so the
/// origin's own `100 Continue` can never reach the client in time. A
/// client waiting on one stalls until its own timeout gives up.
/// RFC 9110 §10.1.1 lets the proxy answer instead. A body that is
/// already known to be too large gets no `100`: the read that follows
/// rejects it.
pub async fn send_continue_if_expected<W>(
    client: &mut W,
    parsed_request: &ParsedRequest,
    max_body_size: usize,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let expects_continue = parsed_request
        .header("expect")
        .is_some_and(|value| header_list_contains(value, "100-continue"));
    if !expects_continue || parsed_request.version == "HTTP/1.0" {
        return Ok(());
    }

    let has_body = parsed_request.is_chunked()
        || parsed_request
            .content_length()
            .is_some_and(|n| n > 0 && n <= max_body_size);
    if has_body {
        client.write_all(CONTINUE_RESPONSE).await?;
    }
    Ok(())
}

/// Buffer `content_length` body bytes, returning how many are actually
/// available — fewer only when the client hung up early.
async fn read_content_length_body<R>(
    reader: &mut R,
    pending: &mut BytesMut,
    content_length: usize,
    max_body_size: usize,
) -> Result<usize>
where
    R: AsyncRead + Unpin,
{
    // The length is declared up front, so an oversized body can be
    // rejected before reading any of it.
    if content_length > max_body_size {
        return Err(crate::Error::BodyTooLarge {
            size: content_length,
            max: max_body_size,
        });
    }

    while pending.len() < content_length {
        // Reads land straight in `pending`; reserving first keeps them
        // chunk-sized rather than filling whatever scrap of spare
        // capacity happens to be left.
        pending.reserve(READ_CHUNK_SIZE);
        if reader.read_buf(pending).await? == 0 {
            break;
        }
    }

    Ok(content_length.min(pending.len()))
}

/// Buffer a chunked body, returning the byte length of its framing.
async fn read_chunked_body<R>(
    reader: &mut R,
    pending: &mut BytesMut,
    max_body_size: usize,
) -> Result<usize>
where
    R: AsyncRead + Unpin,
{
    loop {
        // Once the framing terminates, its true length is known and can
        // be judged against the limit exactly.
        if let Some(end) = chunked_body_end(pending) {
            if end > max_body_size {
                return Err(crate::Error::BodyTooLarge {
                    size: end,
                    max: max_body_size,
                });
            }
            return Ok(end);
        }

        // Still unterminated: the buffered bytes are all body so far.
        if pending.len() > max_body_size {
            return Err(crate::Error::BodyTooLarge {
                size: pending.len(),
                max: max_body_size,
            });
        }

        pending.reserve(READ_CHUNK_SIZE);
        if reader.read_buf(pending).await? == 0 {
            // Client hung up mid-body. Keep what arrived; the framing was
            // never satisfied, so all of it is body.
            return Ok(pending.len());
        }
    }
}

/// Where a relayed body is written.
///
/// The HTTP/1.1 paths write bytes straight onto the client socket; the
/// HTTP/2 path hands each chunk to hyper as a body frame. The framing
/// state machine above them is identical and MUST stay that way — the
/// truncation and pipelining bugs it has already grown fixes for do not
/// want a second copy — so the sink is the part that varies.
pub trait BodySink {
    /// Write one chunk of body, in order. An error ends the relay.
    fn write_chunk(&mut self, chunk: &[u8]) -> impl Future<Output = Result<()>> + Send;

    /// Whether this sink takes the payload of a chunked body rather than
    /// its wire bytes.
    ///
    /// An HTTP/1.1 client reads the chunk framing itself. An HTTP/2
    /// client has its own framing, so chunk-size lines in its DATA
    /// frames would corrupt the body.
    fn decodes_chunked(&self) -> bool {
        false
    }
}

/// Anything writable is a sink: the HTTP/1.1 client socket, a TLS
/// stream, a test duplex.
impl<W> BodySink for W
where
    W: AsyncWrite + Unpin + Send,
{
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        self.write_all(chunk).await.map_err(Into::into)
    }
}

/// Write all of `bytes`, failing if that takes longer than `limit`.
///
/// A peer that stops reading fills its receive window, after which a
/// plain `write_all` waits forever and holds the connection with it.
pub async fn write_all_within<W>(
    writer: &mut W,
    bytes: &[u8],
    limit: Duration,
    operation: &'static str,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    timeout(limit, writer.write_all(bytes))
        .await
        .map_err(|_elapsed| crate::Error::Timeout {
            operation: operation.into(),
            duration: limit,
        })?
        .map_err(Into::into)
}

/// A sink that bounds each write by a deadline.
///
/// See [`write_all_within`].
pub struct WriteDeadline<'a, W> {
    writer: &'a mut W,
    limit: Duration,
    operation: &'static str,
}

impl<'a, W> WriteDeadline<'a, W> {
    /// Bound each write to `writer` by `limit`, naming timeouts
    /// `operation`.
    pub fn new(writer: &'a mut W, limit: Duration, operation: &'static str) -> Self {
        Self {
            writer,
            limit,
            operation,
        }
    }
}

impl<W> BodySink for WriteDeadline<'_, W>
where
    W: AsyncWrite + Unpin + Send,
{
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        write_all_within(self.writer, chunk, self.limit, self.operation).await
    }
}

/// Stream a response body from `upstream` to `client` while capturing
/// a copy for callbacks/sessions.
///
/// `initial` is any body bytes pulled off the upstream when parsing
/// response headers — they are written to the client and counted toward
/// the body before the read loop starts. Each read from `upstream` is
/// bounded by `read_timeout`; on expiry the error is tagged with
/// `timeout_op` so logs distinguish plain-HTTP from intercepted-HTTPS
/// stalls.
///
/// At most `capture_limit` bytes are kept for the capture. The client
/// still receives the whole body; only the copy is cut short, so one
/// large download cannot hold its full size in memory.
///
/// A body cut short by the upstream closing is *not* an error — the
/// bytes that did arrive have already been relayed — but the returned
/// [`ForwardedBody`] records it so the caller does not go on reusing a
/// connection the origin has already dropped.
pub async fn forward_body<R, S>(
    upstream: &mut R,
    client: &mut S,
    framing: BodyFraming,
    initial: &[u8],
    read_timeout: Duration,
    timeout_op: &'static str,
    capture_limit: usize,
) -> Result<ForwardedBody>
where
    R: AsyncRead + Unpin,
    S: BodySink,
{
    let mut relay = Relay {
        captured: BytesMut::with_capacity(initial.len().min(capture_limit)),
        capture_limit,
        capture_cut: false,
        received: 0,
        chunked: matches!(framing, BodyFraming::Chunked).then(ChunkedDecoder::default),
        decode: client.decodes_chunked(),
        decoded: Vec::new(),
    };

    if !initial.is_empty() {
        relay.pass(initial, client).await?;
    }

    // Reuse a single scratch buffer across iterations — response bodies
    // routinely span many 8 KB reads and per-read allocation adds up.
    let mut scratch = vec![0u8; READ_CHUNK_SIZE];

    let complete = loop {
        // The initial buffer may already satisfy framing (e.g. a small
        // chunked body that fully fit alongside the response headers),
        // so check before issuing another read.
        let framing_satisfied = match framing {
            BodyFraming::ContentLength(n) => relay.received >= n,
            BodyFraming::Chunked => relay.chunked.as_ref().is_some_and(ChunkedDecoder::is_done),
            BodyFraming::UntilClose => false,
        };
        if framing_satisfied {
            break true;
        }

        let want = match framing {
            BodyFraming::ContentLength(n) => READ_CHUNK_SIZE.min(n - relay.received),
            BodyFraming::Chunked | BodyFraming::UntilClose => READ_CHUNK_SIZE,
        };
        let read_future = upstream.read(&mut scratch[..want]);
        let n = timeout(read_timeout, read_future)
            .await
            .map_err(|_elapsed| crate::Error::Timeout {
                operation: timeout_op.into(),
                duration: read_timeout,
            })??;
        if n == 0 {
            // Upstream closed. For UntilClose this is normal termination;
            // for framed responses it means the server gave up mid-body,
            // but the caller keeps the partial data we captured rather
            // than discarding it in an error.
            break matches!(framing, BodyFraming::UntilClose);
        }
        relay.pass(&scratch[..n], client).await?;
    };

    if relay.capture_cut {
        warn!(
            received = relay.received,
            captured = relay.captured.len(),
            "response body exceeded max_body_size; relayed in full, captured in part"
        );
    }

    Ok(ForwardedBody {
        bytes: relay.captured.freeze(),
        framing,
        complete,
    })
}

/// Running state of one body relay.
struct Relay {
    captured: BytesMut,
    capture_limit: usize,
    capture_cut: bool,

    /// Wire bytes seen so far, captured or not.
    received: usize,

    /// Tracks where a chunked body ends, so completion is known without
    /// rescanning everything received.
    chunked: Option<ChunkedDecoder>,

    /// Whether the sink wants chunk payload instead of wire bytes.
    decode: bool,

    /// Scratch space for the payload handed to a decoding sink.
    decoded: Vec<u8>,
}

impl Relay {
    /// Capture `bytes` and hand them to the sink.
    async fn pass<S: BodySink>(&mut self, bytes: &[u8], sink: &mut S) -> Result<()> {
        self.received += bytes.len();

        let room = self.capture_limit.saturating_sub(self.captured.len());
        if bytes.len() > room {
            self.capture_cut = true;
        }
        self.captured
            .extend_from_slice(&bytes[..bytes.len().min(room)]);

        let Some(decoder) = &mut self.chunked else {
            return sink.write_chunk(bytes).await;
        };

        if !self.decode {
            decoder.feed(bytes, |_| {});
            return sink.write_chunk(bytes).await;
        }

        self.decoded.clear();
        let decoded = &mut self.decoded;
        decoder.feed(bytes, |payload| decoded.extend_from_slice(payload));
        if decoder.is_malformed() {
            return Err(crate::Error::InvalidResponse(
                "malformed chunked response body".to_owned(),
            ));
        }
        if self.decoded.is_empty() {
            return Ok(());
        }
        sink.write_chunk(&self.decoded).await
    }
}

/// Incremental parser for the chunked transfer coding (RFC 9112 §7.1).
///
/// Bytes arrive in arbitrary pieces, so the parser keeps its position
/// between calls instead of rescanning the body from the start each time.
/// Chunk extensions and trailer fields are skipped.
#[derive(Debug, Default)]
struct ChunkedDecoder {
    state: ChunkState,
}

#[derive(Debug, Clone, Copy)]
enum ChunkState {
    /// Reading a chunk-size line.
    Size {
        value: u64,
        digits: usize,
        in_extension: bool,
        saw_cr: bool,
    },

    /// Inside chunk data, with this many bytes still to come.
    Data(u64),

    /// Expecting the CR that closes a chunk's data.
    DataCr,

    /// Expecting the LF that follows it.
    DataLf,

    /// Reading trailer lines after the last chunk. An empty line ends
    /// the body.
    Trailer {
        line_len: usize,
        saw_cr: bool,
    },

    Done,
    Malformed,
}

impl Default for ChunkState {
    fn default() -> Self {
        Self::Size {
            value: 0,
            digits: 0,
            in_extension: false,
            saw_cr: false,
        }
    }
}

impl ChunkedDecoder {
    /// Consume `input`, passing each run of chunk payload to `payload`.
    ///
    /// Bytes past the end of the body, or past a framing error, are
    /// ignored.
    fn feed(&mut self, mut input: &[u8], mut payload: impl FnMut(&[u8])) {
        while let Some((&byte, rest)) = input.split_first() {
            match &mut self.state {
                ChunkState::Done | ChunkState::Malformed => return,
                ChunkState::Data(remaining) => {
                    let take =
                        usize::try_from(*remaining).map_or(input.len(), |r| r.min(input.len()));
                    payload(&input[..take]);
                    *remaining -= u64::try_from(take).unwrap_or(u64::MAX);
                    if *remaining == 0 {
                        self.state = ChunkState::DataCr;
                    }
                    input = &input[take..];
                }
                _ => {
                    self.state = self.state.step(byte);
                    input = rest;
                }
            }
        }
    }

    fn is_done(&self) -> bool {
        matches!(self.state, ChunkState::Done)
    }

    fn is_malformed(&self) -> bool {
        matches!(self.state, ChunkState::Malformed)
    }
}

impl ChunkState {
    /// Advance by one framing byte. Chunk data is consumed in bulk by
    /// [`ChunkedDecoder::feed`] and never reaches here.
    fn step(self, byte: u8) -> Self {
        match self {
            Self::Size {
                value,
                digits,
                in_extension,
                saw_cr,
            } => {
                if saw_cr {
                    if byte != b'\n' || digits == 0 {
                        return Self::Malformed;
                    }
                    if value == 0 {
                        return Self::Trailer {
                            line_len: 0,
                            saw_cr: false,
                        };
                    }
                    return Self::Data(value);
                }
                if byte == b'\r' {
                    return Self::Size {
                        value,
                        digits,
                        in_extension,
                        saw_cr: true,
                    };
                }
                if in_extension {
                    return self;
                }
                if let Some(digit) = char::from(byte).to_digit(16) {
                    return value
                        .checked_mul(16)
                        .and_then(|v| v.checked_add(u64::from(digit)))
                        .map_or(Self::Malformed, |value| Self::Size {
                            value,
                            digits: digits + 1,
                            in_extension,
                            saw_cr,
                        });
                }
                match byte {
                    // Leading whitespace is tolerated; whitespace or `;`
                    // after the size starts an extension.
                    b' ' | b'\t' if digits == 0 => self,
                    b' ' | b'\t' | b';' => Self::Size {
                        value,
                        digits,
                        in_extension: true,
                        saw_cr,
                    },
                    _ => Self::Malformed,
                }
            }
            Self::DataCr if byte == b'\r' => Self::DataLf,
            Self::DataLf if byte == b'\n' => Self::default(),
            Self::Trailer { line_len, saw_cr } => match (saw_cr, byte) {
                (true, b'\n') if line_len == 0 => Self::Done,
                (true, b'\n') => Self::Trailer {
                    line_len: 0,
                    saw_cr: false,
                },
                (true, _) => Self::Malformed,
                (false, b'\r') => Self::Trailer {
                    line_len,
                    saw_cr: true,
                },
                (false, _) => Self::Trailer {
                    line_len: line_len + 1,
                    saw_cr: false,
                },
            },
            Self::Done => Self::Done,
            Self::Data(_) | Self::DataCr | Self::DataLf | Self::Malformed => Self::Malformed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::is_chunked_body_complete;
    use tokio::io::duplex;

    /// Run `wire` through a fresh decoder in pieces of `step` bytes,
    /// returning the payload and the final state.
    fn decode_in_pieces(wire: &[u8], step: usize) -> (Vec<u8>, ChunkedDecoder) {
        let mut decoder = ChunkedDecoder::default();
        let mut payload = Vec::new();
        for piece in wire.chunks(step) {
            decoder.feed(piece, |p| payload.extend_from_slice(p));
        }
        (payload, decoder)
    }

    #[test]
    fn test_chunked_decoder_handles_any_split() {
        let wire = b"5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: t\r\n\r\n";
        for step in 1..=wire.len() {
            let (payload, decoder) = decode_in_pieces(wire, step);
            assert_eq!(payload, b"hello world", "split every {step} bytes");
            assert!(decoder.is_done(), "split every {step} bytes");
        }
    }

    #[test]
    fn test_chunked_decoder_is_not_fooled_by_a_terminator_inside_data() {
        // The payload itself contains `0\r\n\r\n`.
        let (payload, decoder) = decode_in_pieces(b"7\r\n0\r\n\r\nab\r\n", 3);
        assert_eq!(payload, b"0\r\n\r\nab");
        assert!(!decoder.is_done(), "the last chunk has not arrived");
    }

    #[test]
    fn test_chunked_decoder_rejects_bad_framing() {
        let (_, decoder) = decode_in_pieces(b"zz\r\n", 1);
        assert!(decoder.is_malformed());

        let (_, decoder) = decode_in_pieces(b"2\r\nabXY", 1);
        assert!(decoder.is_malformed(), "data must end in CRLF");
    }

    /// A sink that wants chunk payload, like the HTTP/2 relay.
    struct DecodingSink(Vec<u8>);

    impl BodySink for DecodingSink {
        async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
            self.0.extend_from_slice(chunk);
            Ok(())
        }

        fn decodes_chunked(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn test_forward_body_gives_a_decoding_sink_the_payload() {
        let (mut upstream_peer, mut upstream) = duplex(256);
        upstream_peer
            .write_all(b"5\r\nhello\r\n0\r\n\r\n")
            .await
            .expect("write");

        let mut sink = DecodingSink(Vec::new());
        let body = forward_body(
            &mut upstream,
            &mut sink,
            BodyFraming::Chunked,
            &[],
            Duration::from_secs(1),
            "test",
            usize::MAX,
        )
        .await
        .expect("ok");

        assert_eq!(sink.0, b"hello", "no chunk framing reaches the sink");
        assert!(body.is_complete());
        // The capture keeps the wire form, which `with_body` decodes.
        assert!(is_chunked_body_complete(&body.into_bytes()));
    }

    #[tokio::test]
    async fn test_forward_body_relays_everything_but_captures_up_to_the_limit() {
        let (mut upstream_peer, mut upstream) = duplex(256);
        let (mut client_peer, mut client) = duplex(256);
        upstream_peer.write_all(b"0123456789").await.expect("write");

        let body = forward_body(
            &mut upstream,
            &mut client,
            BodyFraming::ContentLength(10),
            &[],
            Duration::from_secs(1),
            "test",
            4,
        )
        .await
        .expect("ok");

        assert!(
            body.is_complete(),
            "framing counts wire bytes, not captured ones"
        );
        assert_eq!(body.into_bytes(), Bytes::from_static(b"0123"));

        drop(client);
        let mut seen = Vec::new();
        AsyncReadExt::read_to_end(&mut client_peer, &mut seen)
            .await
            .expect("read client");
        assert_eq!(seen, b"0123456789", "the client gets the whole body");
    }

    #[tokio::test]
    async fn test_read_body_prefers_chunked_over_content_length() {
        // RFC 9112 §6.3: Transfer-Encoding overrides Content-Length. Reading
        // by the length here would forward `0\r\n\r\nGET /smuggled`
        // as body bytes that a chunked-reading origin sees as a request.
        let (parsed, mut buf) = parsed_with_buf(
            b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 30\r\n\
              Transfer-Encoding: chunked\r\n\r\n\
              0\r\n\r\nGET /smuggled HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        let (_peer, mut reader) = duplex(64);

        let body = read_body(&mut reader, &mut buf, &parsed, 1024)
            .await
            .expect("ok");

        assert_eq!(body, Bytes::from_static(b"0\r\n\r\n"));
        assert!(
            buf[parsed.header_len..].starts_with(b"GET /smuggled"),
            "what follows the chunked body is the next request"
        );
    }

    #[tokio::test]
    async fn test_forward_body_gives_up_on_a_client_that_stops_reading() {
        let (mut upstream_peer, mut upstream) = duplex(64 * 1024);
        // Nobody reads from `_client_peer`, so its 16 bytes fill at once.
        let (_client_peer, mut client) = duplex(16);
        upstream_peer.write_all(&[b'x'; 1024]).await.expect("write");

        let err = forward_body(
            &mut upstream,
            &mut WriteDeadline::new(&mut client, Duration::from_millis(50), "test write"),
            BodyFraming::ContentLength(1024),
            &[],
            Duration::from_secs(5),
            "test",
            usize::MAX,
        )
        .await
        .expect_err("a stalled client must not hold the relay forever");

        assert!(
            matches!(&err, crate::Error::Timeout { operation, .. } if operation == "test write"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_send_continue_if_expected() {
        let expecting = parsed("POST", "Content-Length: 5\r\nExpect: 100-continue\r\n");
        let (mut peer, mut client) = duplex(64);
        send_continue_if_expected(&mut client, &expecting, 1024)
            .await
            .expect("ok");
        drop(client);
        let mut seen = Vec::new();
        AsyncReadExt::read_to_end(&mut peer, &mut seen)
            .await
            .expect("read");
        assert_eq!(seen, CONTINUE_RESPONSE);

        // Too large to accept, or not asked for: nothing is sent.
        for request in [
            parsed("POST", "Content-Length: 4096\r\nExpect: 100-continue\r\n"),
            parsed("POST", "Content-Length: 5\r\n"),
        ] {
            let (mut peer, mut client) = duplex(64);
            send_continue_if_expected(&mut client, &request, 1024)
                .await
                .expect("ok");
            drop(client);
            let mut seen = Vec::new();
            AsyncReadExt::read_to_end(&mut peer, &mut seen)
                .await
                .expect("read");
            assert!(seen.is_empty());
        }
    }

    fn parsed(method: &str, extra_headers: &str) -> ParsedRequest {
        // Build a minimal parseable HTTP request so we can exercise the
        // body reader without hand-constructing a ParsedRequest.
        let req = format!("{method} / HTTP/1.1\r\nHost: x\r\n{extra_headers}\r\n");
        let buf = BytesMut::from(req.as_bytes());
        ParsedRequest::parse(&buf)
            .expect("parses")
            .expect("complete")
    }

    /// Parse `raw` as a request and hand back the buffer it arrived in,
    /// mirroring how the connection loop holds bytes across a request.
    fn parsed_with_buf(raw: &[u8]) -> (ParsedRequest, BytesMut) {
        let buf = BytesMut::from(raw);
        let parsed = ParsedRequest::parse(&buf)
            .expect("parses")
            .expect("complete");
        (parsed, buf)
    }

    #[tokio::test]
    async fn test_read_body_leaves_a_pipelined_request_in_the_buffer() {
        let (parsed, mut buf) = parsed_with_buf(
            b"POST /one HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\n\
              helloGET /two HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        let (_peer, mut reader) = duplex(64);

        let body = read_body(&mut reader, &mut buf, &parsed, 1024)
            .await
            .expect("ok");

        assert_eq!(
            body,
            Bytes::from_static(b"hello"),
            "the body is exactly what Content-Length claimed"
        );
        // The head stays at the front for the caller to strip; the
        // pipelined request follows it untouched.
        assert_eq!(
            &buf[parsed.header_len..],
            b"GET /two HTTP/1.1\r\nHost: x\r\n\r\n",
            "the next request must survive"
        );
    }

    #[tokio::test]
    async fn test_read_body_chunked_leaves_a_pipelined_request_in_the_buffer() {
        let (parsed, mut buf) = parsed_with_buf(
            b"POST /one HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
              5\r\nhello\r\n0\r\n\r\nGET /two HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        let (_peer, mut reader) = duplex(64);

        let body = read_body(&mut reader, &mut buf, &parsed, 1024)
            .await
            .expect("ok");

        assert_eq!(
            body,
            Bytes::from_static(b"5\r\nhello\r\n0\r\n\r\n"),
            "the body stops at the chunk terminator"
        );
        assert_eq!(
            &buf[parsed.header_len..],
            b"GET /two HTTP/1.1\r\nHost: x\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn test_read_body_treats_an_unframed_request_as_bodyless() {
        // No Content-Length, no chunked: RFC 7230 §3.3.3 rule 6 says the
        // body length is zero, so everything after the head belongs to
        // the next request — even for a POST.
        let (parsed, mut buf) = parsed_with_buf(
            b"POST /one HTTP/1.1\r\nHost: x\r\n\r\nGET /two HTTP/1.1\r\nHost: x\r\n\r\n",
        );
        let (_peer, mut reader) = duplex(64);

        let body = read_body(&mut reader, &mut buf, &parsed, 1024)
            .await
            .expect("ok");

        assert!(body.is_empty());
        assert_eq!(
            &buf[parsed.header_len..],
            b"GET /two HTTP/1.1\r\nHost: x\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn test_read_body_rejects_an_oversized_content_length_before_reading() {
        // The length is declared, so there is no reason to buffer any of
        // it first. Nothing is ever written to the peer here; if the
        // reader is touched at all, this hangs.
        let (parsed, mut buf) =
            parsed_with_buf(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 4096\r\n\r\n");
        let (_peer, mut reader) = duplex(64);

        let err = read_body(&mut reader, &mut buf, &parsed, 16)
            .await
            .expect_err("should be too large");

        assert!(matches!(
            err,
            crate::Error::BodyTooLarge {
                size: 4096,
                max: 16
            }
        ));
    }

    #[tokio::test]
    async fn test_read_body_keeps_a_partial_body_when_the_client_hangs_up() {
        let (parsed, mut buf) =
            parsed_with_buf(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nabc");
        let (peer, mut reader) = duplex(64);
        drop(peer);

        let body = read_body(&mut reader, &mut buf, &parsed, 1024)
            .await
            .expect("a short body is not an error");

        assert_eq!(body, Bytes::from_static(b"abc"));
    }

    #[tokio::test]
    async fn test_read_body_content_length_reads_exact_count() {
        let parsed_req = parsed("POST", "Content-Length: 5\r\n");
        let (mut peer, mut reader) = duplex(64);
        peer.write_all(b"hello").await.expect("write");
        drop(peer);

        let mut buf = BytesMut::new();
        let body = read_body(&mut reader, &mut buf, &parsed_req, 1024)
            .await
            .expect("ok");
        assert_eq!(body, Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn test_read_body_chunked_stops_at_terminator() {
        let parsed_req = parsed("POST", "Transfer-Encoding: chunked\r\n");
        let (mut peer, mut reader) = duplex(128);
        // Two data chunks + terminator.
        peer.write_all(b"5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n")
            .await
            .expect("write");
        drop(peer);

        let mut buf = BytesMut::new();
        let body = read_body(&mut reader, &mut buf, &parsed_req, 1024)
            .await
            .expect("ok");
        assert!(is_chunked_body_complete(&body));
    }

    #[tokio::test]
    async fn test_read_body_rejects_oversize() {
        // Chunked framing exercises the per-iteration size check. We
        // write enough wire bytes that the body buffer crosses the max
        // before the terminator is observed.
        let parsed_req = parsed("POST", "Transfer-Encoding: chunked\r\n");
        let (mut peer, mut reader) = duplex(4096);

        let mut all = Vec::new();
        for _ in 0..10 {
            all.extend_from_slice(b"10\r\naaaaaaaaaaaaaaaa\r\n");
        }
        all.extend_from_slice(b"0\r\n\r\n");
        peer.write_all(&all).await.expect("write");
        drop(peer);

        let mut buf = BytesMut::new();
        let err = read_body(&mut reader, &mut buf, &parsed_req, 16)
            .await
            .expect_err("should be too large");
        assert!(matches!(err, crate::Error::BodyTooLarge { .. }));
    }

    #[tokio::test]
    async fn test_forward_body_initial_satisfies_content_length_without_reading() {
        // If the complete body already arrived alongside the response
        // headers, forward_body must not issue another read on `upstream`
        // — otherwise it hangs on a keep-alive connection.
        let (upstream_peer, mut upstream) = duplex(64);
        let (_client_peer, mut client) = duplex(64);

        // Nothing is ever written to upstream_peer; if forward_body reads
        // from upstream, this test will hang.
        let initial = b"hello world";
        let body = forward_body(
            &mut upstream,
            &mut client,
            BodyFraming::ContentLength(initial.len()),
            initial,
            Duration::from_secs(1),
            "test",
            usize::MAX,
        )
        .await
        .expect("ok");

        assert!(body.is_complete());
        assert!(body.leaves_connection_usable());
        assert_eq!(body.into_bytes(), Bytes::from_static(initial));
        drop(upstream_peer);
    }

    #[tokio::test]
    async fn test_forward_body_reports_truncated_content_length_body() {
        let (mut upstream_peer, mut upstream) = duplex(256);
        let (_client_peer, mut client) = duplex(256);

        // Ten bytes promised, three delivered, then the peer hangs up.
        upstream_peer.write_all(b"abc").await.expect("write");
        drop(upstream_peer);

        let body = forward_body(
            &mut upstream,
            &mut client,
            BodyFraming::ContentLength(10),
            &[],
            Duration::from_secs(1),
            "test",
            usize::MAX,
        )
        .await
        .expect("partial body is not an error");

        assert!(!body.is_complete(), "the body was cut short");
        assert!(
            !body.leaves_connection_usable(),
            "a connection closed mid-body is dead"
        );
        // What did arrive is still captured and relayed.
        assert_eq!(body.into_bytes(), Bytes::from_static(b"abc"));
    }

    #[tokio::test]
    async fn test_forward_body_until_close_is_complete_but_ends_the_connection() {
        let (mut upstream_peer, mut upstream) = duplex(256);
        let (_client_peer, mut client) = duplex(256);

        upstream_peer.write_all(b"hello").await.expect("write");
        drop(upstream_peer);

        let body = forward_body(
            &mut upstream,
            &mut client,
            BodyFraming::UntilClose,
            &[],
            Duration::from_secs(1),
            "test",
            usize::MAX,
        )
        .await
        .expect("ok");

        assert!(body.is_complete(), "the close *is* the framing here");
        assert!(
            !body.leaves_connection_usable(),
            "but it also means there is no connection left to reuse"
        );
        assert_eq!(body.into_bytes(), Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn test_forward_body_reports_truncated_chunked_body() {
        let (mut upstream_peer, mut upstream) = duplex(256);
        let (_client_peer, mut client) = duplex(256);

        // A data chunk with no terminating zero-length chunk.
        upstream_peer
            .write_all(b"5\r\nhello\r\n")
            .await
            .expect("write");
        drop(upstream_peer);

        let body = forward_body(
            &mut upstream,
            &mut client,
            BodyFraming::Chunked,
            &[],
            Duration::from_secs(1),
            "test",
            usize::MAX,
        )
        .await
        .expect("partial body is not an error");

        assert!(!body.is_complete(), "the chunk terminator never arrived");
        assert!(!body.leaves_connection_usable());
    }

    #[tokio::test]
    async fn test_forward_body_streams_chunked_body_to_client() {
        let (mut upstream_peer, mut upstream) = duplex(256);
        let (mut client_peer, mut client) = duplex(256);

        let writer = tokio::spawn(async move {
            upstream_peer
                .write_all(b"5\r\nhello\r\n0\r\n\r\n")
                .await
                .expect("write");
        });

        let body = forward_body(
            &mut upstream,
            &mut client,
            BodyFraming::Chunked,
            &[],
            Duration::from_secs(1),
            "test",
            usize::MAX,
        )
        .await
        .expect("ok");

        writer.await.expect("writer joined");
        assert!(body.is_complete());
        assert!(body.leaves_connection_usable());
        let body = body.into_bytes();
        assert!(is_chunked_body_complete(&body));

        // The client sees everything that went through the forwarder.
        drop(client);
        let mut seen = Vec::new();
        AsyncReadExt::read_to_end(&mut client_peer, &mut seen)
            .await
            .expect("read client");
        assert_eq!(seen, body.as_ref());
    }
}
