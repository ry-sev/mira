//! HTTP/2 for intercepted HTTPS sessions.
//!
//! A client speaks HTTP/1.1 to the proxy itself — that is how `CONNECT`
//! reaches us — so HTTP/2 only ever appears *inside* a TLS session the
//! proxy terminates. This module serves that session: hyper handles the
//! frame layer, HPACK, and multiplexing, and each stream is forwarded
//! upstream over HTTP/1.1 through the same primitives the HTTP/1.1 path
//! uses, so framing, truncation detection, and keep-alive decisions have
//! exactly one implementation.
//!
//! Two shapes differ from HTTP/1.1 and drive most of the code here:
//!
//! - **Streams are concurrent.** One connection carries many at once, so the
//!   tunnel's single sequential upstream connection is replaced by a small pool
//!   that streams check out and return.
//! - **Connection-specific headers are illegal** (RFC 9113 §8.2.2), not merely
//!   ignored. They are rejected on the way in and never generated on the way
//!   out.

use crate::config::Config;
use crate::http::BodyFraming;
use crate::http::BodySink;
use crate::http::Exchange;
use crate::http::ParsedRequest;
use crate::http::ParsedResponse;
use crate::http::Request;
use crate::http::RequestSent;
use crate::http::Response;
use crate::http::Timings;
use crate::http::apply_request_proxy_headers;
use crate::http::apply_response_proxy_headers;
use crate::http::can_reuse_connection;
use crate::http::forward_body;
use crate::http::gateway_error_status;
use crate::http::is_hop_by_hop;
use crate::http::response_has_no_body;
use crate::http::serialize_request_head;
use crate::http::write_request_and_parse_head;
use crate::proxy::server::ProxyState;
use crate::proxy::tunnel::SetupTimings;
use crate::proxy::tunnel::TlsUpstreamConnector;
use crate::proxy::tunnel::Upstream;
use crate::proxy::tunnel::UpstreamConnector;
use crate::tls::AlpnProtocol;
use bytes::Bytes;
use bytes::BytesMut;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::Body;
use hyper::body::Frame;
use hyper::body::Incoming;
use hyper::client::conn::http2::SendRequest;
use hyper::service::service_fn;
use hyper_util::rt::TokioExecutor;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::info;
use tracing::warn;

/// The version string recorded for exchanges on an HTTP/2 session.
const HTTP2_VERSION: &str = "HTTP/2";

/// How many body chunks may be queued for the client before the relay
/// stops reading from upstream. Backpressure, so a fast origin and a
/// slow client cannot buffer a response in memory.
const BODY_CHANNEL_DEPTH: usize = 4;

/// Serve an intercepted TLS session that negotiated HTTP/2.
///
/// `client_handshake_ms` is the cost of the client-side TLS handshake,
/// attributed to the first exchange the session carries.
pub async fn serve_intercepted<C>(
    client_tls: C,
    host: &str,
    port: u16,
    client_addr: SocketAddr,
    client_handshake_ms: f64,
    state: &Arc<ProxyState>,
) -> crate::Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let connector = TlsUpstreamConnector::new(host, port, AlpnProtocol::Http2, state)?;
    serve_with_connector(
        client_tls,
        connector,
        host,
        port,
        client_addr,
        client_handshake_ms,
        state,
    )
    .await
}

/// Serve an HTTP/2 session against a given upstream connector.
///
/// Split out from [`serve_intercepted`] so the stream handling can be
/// exercised over in-memory connections, without a TLS origin whose
/// certificate the system trust store would have to accept.
#[allow(clippy::too_many_arguments)]
async fn serve_with_connector<C, K>(
    client_tls: C,
    connector: K,
    host: &str,
    port: u16,
    client_addr: SocketAddr,
    client_handshake_ms: f64,
    state: &Arc<ProxyState>,
) -> crate::Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    K: UpstreamConnector + Send + Sync + 'static,
    K::Stream: Send + 'static,
{
    let session = Arc::new(Http2Session {
        connector,
        upstream: Mutex::new(UpstreamLeg::Unopened),
        pending_handshake_ms: Mutex::new(Some(client_handshake_ms)),
        host: host.to_owned(),
        port,
        client_addr,
        state: Arc::clone(state),
    });

    debug!(host, port, "serving intercepted session over HTTP/2");

    let service = service_fn(move |request: hyper::Request<Incoming>| {
        let session = Arc::clone(&session);
        async move { session.handle(request).await }
    });

    hyper::server::conn::http2::Builder::new(TokioExecutor::new())
        .max_header_list_size(header_list_limit(&state.config))
        .serve_connection(TokioIo::new(client_tls), service)
        .await
        .map_err(|e| {
            crate::Error::InvalidRequest(format!("HTTP/2 connection failed: {}", causes(&e)))
        })
}

/// Everything the concurrently-served streams of one session share.
struct Http2Session<K: UpstreamConnector> {
    connector: K,

    /// The upstream leg, discovered when the first connection is made.
    upstream: Mutex<UpstreamLeg<K::Stream>>,

    /// The client-side handshake cost, claimed by whichever stream is
    /// served first.
    pending_handshake_ms: Mutex<Option<f64>>,

    host: String,
    port: u16,
    client_addr: SocketAddr,
    state: Arc<ProxyState>,
}

/// How this session reaches the origin.
///
/// Which arm it lands on is not known until a connection is made: the
/// proxy offers HTTP/2 and HTTP/1.1, and the origin chooses.
enum UpstreamLeg<S> {
    /// Nothing has connected yet.
    Unopened,

    /// The origin speaks HTTP/1.1. Connections are exclusive, so idle
    /// ones are kept for whichever stream needs one next.
    Http1(Vec<Upstream<S>>),

    /// The origin speaks HTTP/2. One connection is multiplexed by every
    /// stream — that is the entire point of the protocol, so pooling
    /// several would be missing it.
    Http2(Option<Http2Upstream>),
}

/// A multiplexed HTTP/2 connection to the origin.
struct Http2Upstream {
    sender: SendRequest<Full<Bytes>>,
    remote_addr: Option<SocketAddr>,

    /// Claimed by the first stream to use this connection.
    pending_setup: Option<SetupTimings>,
}

/// An upstream connection lent to one stream.
enum Leased<S> {
    /// An exclusive HTTP/1.1 connection, to be returned when done.
    Http1(Upstream<S>),

    /// A handle on the shared HTTP/2 connection. Cloned, not lent —
    /// there is nothing to give back.
    Http2 {
        sender: SendRequest<Full<Bytes>>,
        remote_addr: Option<SocketAddr>,
        setup: Option<SetupTimings>,
    },
}

impl<K> Http2Session<K>
where
    K: UpstreamConnector + Send + Sync + 'static,
    K::Stream: Send + 'static,
{
    /// Serve one HTTP/2 stream.
    async fn handle(
        self: Arc<Self>,
        request: hyper::Request<Incoming>,
    ) -> crate::Result<hyper::Response<StreamedBody>> {
        match self.forward(request).await {
            Ok(response) => Ok(response),
            Err(e) => {
                warn!(host = %self.host, port = self.port, error = %e, "HTTP/2 stream failed");
                // Nothing of a response has gone out yet, so the client
                // can be told why rather than seeing a reset stream.
                Ok(error_response(&e))
            }
        }
    }

    async fn forward(
        self: &Arc<Self>,
        request: hyper::Request<Incoming>,
    ) -> crate::Result<hyper::Response<StreamedBody>> {
        let start = Instant::now();

        let (mut parsed_request, body) = self.read_request(request).await?;
        let request_id = self.state.next_request_id();

        info!(
            id = request_id,
            method = %parsed_request.method,
            target = %parsed_request.target,
            host = %self.host,
            "HTTP/2 request"
        );

        apply_request_proxy_headers(
            &mut parsed_request.headers,
            HTTP2_VERSION,
            self.client_addr.ip(),
        );

        let mut captured_request = Request::from_parsed(
            &parsed_request,
            request_id,
            self.client_addr,
            body.clone(),
            true,
        );
        captured_request.version = HTTP2_VERSION.to_owned();

        if let Some(callback) = &self.state.callbacks.on_request {
            callback.invoke(captured_request.clone()).await;
        }

        let (parsed_response, source, setup, remote_addr, sent, first_byte) =
            match self.send_upstream(&parsed_request, &body, request_id).await {
                Ok(sent) => sent,
                Err(e) => {
                    // The request was announced to the callbacks, so its
                    // outcome MUST be too.
                    self.state
                        .record(Exchange {
                            request: captured_request,
                            response: None,
                            error: Some(e.to_string()),
                        })
                        .await;
                    return Err(e);
                }
            };

        Ok(self.clone().respond(
            ExchangeInProgress {
                request: captured_request,
                request_id,
                setup,
                remote_addr,
                start,
                sent,
                first_byte,
            },
            parsed_response,
            source,
        ))
    }

    /// Send one request upstream, over whichever protocol the origin
    /// speaks, and read back its response head.
    #[allow(clippy::type_complexity)]
    async fn send_upstream(
        &self,
        parsed_request: &ParsedRequest,
        body: &Bytes,
        request_id: u64,
    ) -> crate::Result<(
        ParsedResponse,
        ResponseSource<K::Stream>,
        Option<SetupTimings>,
        Option<SocketAddr>,
        Instant,
        Instant,
    )> {
        match self.checkout().await? {
            Leased::Http2 {
                mut sender,
                remote_addr,
                setup,
            } => {
                let sent = Instant::now();
                sender.ready().await.map_err(|e| {
                    crate::Error::InvalidResponse(format!(
                        "upstream HTTP/2 connection: {}",
                        causes(&e)
                    ))
                })?;
                let request = build_upstream_request(parsed_request, body)?;
                let response = sender.send_request(request).await.map_err(|e| {
                    crate::Error::InvalidResponse(format!(
                        "upstream HTTP/2 request failed: {}",
                        causes(&e)
                    ))
                })?;
                let first_byte = Instant::now();

                let (parts, incoming) = response.into_parts();
                Ok((
                    parsed_response_from_parts(&parts),
                    ResponseSource::Http2 { incoming },
                    setup,
                    remote_addr,
                    sent,
                    first_byte,
                ))
            }
            Leased::Http1(upstream) => {
                let (parsed_response, source, setup, remote_addr, sent, first_byte) = self
                    .send_over_http1(upstream, parsed_request, body, request_id)
                    .await?;
                Ok((
                    parsed_response,
                    source,
                    setup,
                    remote_addr,
                    sent,
                    first_byte,
                ))
            }
        }
    }

    /// Send over an exclusive HTTP/1.1 connection, retrying once if the
    /// origin had already closed a pooled one.
    #[allow(clippy::type_complexity)]
    async fn send_over_http1(
        &self,
        mut upstream: Upstream<K::Stream>,
        parsed_request: &ParsedRequest,
        body: &Bytes,
        request_id: u64,
    ) -> crate::Result<(
        ParsedResponse,
        ResponseSource<K::Stream>,
        Option<SetupTimings>,
        Option<SocketAddr>,
        Instant,
        Instant,
    )> {
        let config = &self.state.config;
        // The upstream leg speaks HTTP/1.1, so the request line does too.
        let mut for_upstream = ParsedRequest {
            method: parsed_request.method.clone(),
            target: parsed_request.target.clone(),
            version: "HTTP/1.1".to_owned(),
            headers: parsed_request.headers.clone(),
            header_len: 0,
        };
        // HTTP/2 frames its own bodies, so a client need not send a
        // Content-Length. HTTP/1.1 has no other way to say where this
        // body ends: without one the origin reads no body, and the body
        // bytes are then read as the start of the next request.
        for_upstream.headers.retain(|(name, _)| {
            !is_connection_specific(name) && !name.eq_ignore_ascii_case("content-length")
        });
        if !body.is_empty() || parsed_request.header("content-length").is_some() {
            for_upstream
                .headers
                .push(("Content-Length".to_owned(), body.len().to_string()));
        }
        let header_bytes = serialize_request_head(&for_upstream);

        let mut setup = upstream.pending_setup.take();
        let mut sent = write_request_and_parse_head(
            &mut upstream.stream,
            &header_bytes,
            body,
            config.read_timeout,
            config.write_timeout,
            config.max_header_size,
            "read HTTP/2 upstream response headers",
        )
        .await;

        if let Err(failure) = &sent
            && !failure.saw_response_bytes
            && setup.is_none()
        {
            // A pooled connection the origin had already closed. No
            // response byte arrived, so the request provably did not land
            // and replaying it is safe for any method.
            debug!(
                id = request_id,
                error = %failure.error,
                "upstream closed a pooled connection; retrying on a fresh one"
            );
            upstream = self.connector.connect().await?;
            setup = upstream.pending_setup.take();
            sent = write_request_and_parse_head(
                &mut upstream.stream,
                &header_bytes,
                body,
                config.read_timeout,
                config.write_timeout,
                config.max_header_size,
                "read HTTP/2 upstream response headers",
            )
            .await;
        }

        let RequestSent {
            mut response_buf,
            parsed_response,
            sent,
            first_byte,
        } = sent.map_err(|failure| failure.error)?;

        if let Some(reason) = parsed_response.framing_error() {
            return Err(crate::Error::InvalidResponse(reason.to_owned()));
        }

        let remote_addr = upstream.remote_addr;
        let headers_allow_reuse = can_reuse_connection(&parsed_response, parsed_request);
        let no_body_status = response_has_no_body(&parsed_request.method, parsed_response.status);
        let framing = BodyFraming::for_response(&parsed_response);
        let body_start = parsed_response.header_len;
        let initial = if response_buf.len() > body_start {
            response_buf.split_off(body_start)
        } else {
            BytesMut::new()
        };

        Ok((
            parsed_response,
            ResponseSource::Http1 {
                upstream,
                framing,
                initial,
                no_body_status,
                headers_allow_reuse,
            },
            setup,
            remote_addr,
            sent,
            first_byte,
        ))
    }

    /// Turn an upstream response into the client-facing one, spawning
    /// the body relay that finishes the capture.
    fn respond(
        self: Arc<Self>,
        exchange: ExchangeInProgress,
        mut parsed_response: ParsedResponse,
        source: ResponseSource<K::Stream>,
    ) -> hyper::Response<StreamedBody> {
        apply_response_proxy_headers(&mut parsed_response.headers, HTTP2_VERSION);

        let (chunks, body) = StreamedBody::channel();
        let response = build_response(&parsed_response, body);

        let relay = BodyRelay {
            session: Arc::clone(&self),
            exchange,
            parsed_response,
            source,
            chunks,
        };
        tokio::spawn(relay.run());

        response
    }

    /// Take an upstream connection for one stream, opening one if the
    /// session has none to lend.
    async fn checkout(&self) -> crate::Result<Leased<K::Stream>> {
        {
            let mut leg = self.upstream.lock();
            match &mut *leg {
                UpstreamLeg::Http1(idle) => {
                    if let Some(upstream) = idle.pop() {
                        return Ok(Leased::Http1(upstream));
                    }
                }
                UpstreamLeg::Http2(Some(http2)) if !http2.sender.is_closed() => {
                    return Ok(Leased::Http2 {
                        sender: http2.sender.clone(),
                        remote_addr: http2.remote_addr,
                        setup: http2.pending_setup.take(),
                    });
                }
                _ => {}
            }
        }

        self.open().await
    }

    /// Open a new upstream connection and record which protocol it
    /// turned out to speak.
    async fn open(&self) -> crate::Result<Leased<K::Stream>> {
        let mut upstream = self.connector.connect().await?;

        // The client-side handshake belongs to the session rather than to
        // any one upstream connection, so the first stream to open one
        // carries it.
        if let Some(handshake_ms) = self.pending_handshake_ms.lock().take()
            && let Some(setup) = &mut upstream.pending_setup
        {
            setup.ssl_ms += handshake_ms;
        }

        if upstream.protocol == AlpnProtocol::Http11 {
            let mut leg = self.upstream.lock();
            if matches!(*leg, UpstreamLeg::Unopened) {
                *leg = UpstreamLeg::Http1(Vec::new());
            }
            return Ok(Leased::Http1(upstream));
        }

        let remote_addr = upstream.remote_addr;
        let setup = upstream.pending_setup.take();

        debug!(
            host = %self.host,
            port = self.port,
            "origin negotiated HTTP/2; multiplexing upstream"
        );

        let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .max_header_list_size(header_list_limit(&self.state.config))
            .handshake(TokioIo::new(upstream.stream))
            .await
            .map_err(|e| {
                crate::Error::TlsHandshake(format!(
                    "upstream HTTP/2 handshake failed: {}",
                    causes(&e)
                ))
            })?;

        tokio::spawn(async move {
            if let Err(e) = connection.await {
                debug!(error = %e, "upstream HTTP/2 connection ended");
            }
        });

        let mut leg = self.upstream.lock();
        if let UpstreamLeg::Http2(Some(existing)) = &mut *leg
            && !existing.sender.is_closed()
        {
            // Another stream won the race to open the connection. Use
            // theirs and let ours drop rather than leaving two open.
            return Ok(Leased::Http2 {
                sender: existing.sender.clone(),
                remote_addr: existing.remote_addr,
                setup: existing.pending_setup.take(),
            });
        }

        *leg = UpstreamLeg::Http2(Some(Http2Upstream {
            sender: sender.clone(),
            remote_addr,
            pending_setup: None,
        }));

        Ok(Leased::Http2 {
            sender,
            remote_addr,
            setup,
        })
    }

    /// Return an HTTP/1.1 connection for another stream to use.
    fn checkin(&self, upstream: Upstream<K::Stream>) {
        let mut leg = self.upstream.lock();
        if let UpstreamLeg::Http1(idle) = &mut *leg
            && idle.len() < self.state.config.max_idle_upstreams_per_session
        {
            idle.push(upstream);
        }
    }

    /// Read a stream's request into the shape the forwarding paths
    /// expect.
    async fn read_request(
        &self,
        request: hyper::Request<Incoming>,
    ) -> crate::Result<(ParsedRequest, Bytes)> {
        let (parts, incoming) = request.into_parts();
        let parsed_request = translate_request(&parts, &self.host, self.port)?;
        let body = read_incoming(incoming, self.state.config.max_body_size).await?;

        Ok((parsed_request, body))
    }
}

/// Where a response body is read from, once its head has been relayed.
enum ResponseSource<S> {
    /// An HTTP/1.1 origin: bytes off the socket, framed by the shared
    /// state machine, with the connection returned afterwards.
    Http1 {
        upstream: Upstream<S>,
        framing: BodyFraming,
        initial: BytesMut,
        no_body_status: bool,
        headers_allow_reuse: bool,
    },

    /// An HTTP/2 origin: frames from hyper, already framed for us.
    Http2 { incoming: Incoming },
}

/// An exchange whose head has gone out and whose body is still arriving.
struct ExchangeInProgress {
    request: Request,
    request_id: u64,
    setup: Option<SetupTimings>,
    remote_addr: Option<SocketAddr>,
    start: Instant,
    sent: Instant,
    first_byte: Instant,
}

/// Relays one response body to the client and records the exchange.
struct BodyRelay<K: UpstreamConnector> {
    session: Arc<Http2Session<K>>,
    exchange: ExchangeInProgress,
    parsed_response: ParsedResponse,
    source: ResponseSource<K::Stream>,
    chunks: mpsc::Sender<crate::Result<Bytes>>,
}

impl<K> BodyRelay<K>
where
    K: UpstreamConnector + Send + Sync + 'static,
    K::Stream: Send + 'static,
{
    async fn run(self) {
        let Self {
            session,
            exchange,
            parsed_response,
            source,
            chunks,
        } = self;
        let mut sink = ChannelSink {
            chunks,
            write_timeout: session.state.config.write_timeout,
        };

        let relayed = match source {
            ResponseSource::Http1 {
                mut upstream,
                framing,
                initial,
                no_body_status,
                headers_allow_reuse,
            } => {
                let body = if no_body_status {
                    Ok(crate::http::ForwardedBody::none())
                } else {
                    forward_body(
                        &mut upstream.stream,
                        &mut sink,
                        framing,
                        &initial,
                        session.state.config.read_timeout,
                        "read HTTP/2 upstream response body",
                        session.state.config.max_body_size,
                    )
                    .await
                };

                match body {
                    Ok(body) => {
                        if headers_allow_reuse && body.leaves_connection_usable() {
                            session.checkin(upstream);
                        }
                        let complete = body.is_complete();
                        Ok((body.into_bytes(), complete))
                    }
                    Err(e) => Err(e),
                }
            }
            ResponseSource::Http2 { incoming } => {
                relay_http2_body(incoming, &mut sink, session.state.config.max_body_size).await
            }
        };

        let finished = Instant::now();

        let (body, complete) = match relayed {
            Ok(relayed) => relayed,
            Err(e) => {
                warn!(
                    id = exchange.request_id,
                    error = %e,
                    "HTTP/2 response body relay failed"
                );
                // Tell the client the response ended early rather than
                // leaving it on a stream that will never finish.
                let _ = sink.chunks.send(Err(e)).await;
                return;
            }
        };

        if !complete {
            warn!(
                id = exchange.request_id,
                "upstream closed mid-body; response relayed truncated"
            );
            // Ending the stream cleanly would tell the client the body is
            // whole. An error resets the stream instead.
            let _ = sink
                .chunks
                .send(Err(crate::Error::InvalidResponse(
                    "upstream closed mid-body".to_owned(),
                )))
                .await;
        }

        let (connect_ms, ssl_ms, connection_reused) = match exchange.setup {
            Some(t) => (Some(t.connect_ms), Some(t.ssl_ms), false),
            None => (None, None, true),
        };
        let timings = Timings {
            blocked_ms: Some(0.0),
            connect_ms,
            send_ms: (exchange.sent - exchange.start).as_secs_f64() * 1000.0,
            wait_ms: (exchange.first_byte - exchange.sent).as_secs_f64() * 1000.0,
            receive_ms: (finished - exchange.first_byte).as_secs_f64() * 1000.0,
            ssl_ms,
            connection_reused,
        };

        let duration_ms = u64::try_from(exchange.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let parsed_response = parsed_response.with_body(body);

        info!(
            id = exchange.request_id,
            status = parsed_response.status,
            duration_ms,
            body_size = parsed_response.body.len(),
            reused = connection_reused,
            "HTTP/2 response"
        );

        let mut response = Response::from_parsed_with_timings(
            &parsed_response,
            exchange.request_id,
            duration_ms,
            Some(timings),
            exchange.remote_addr,
        );
        response.version = HTTP2_VERSION.to_owned();

        session
            .state
            .record(Exchange {
                request: exchange.request,
                response: Some(response.clone()),
                error: None,
            })
            .await;
        if let Some(callback) = &session.state.callbacks.on_response {
            callback.invoke(response).await;
        }
    }
}

/// Relay an HTTP/2 response body, which hyper has already framed.
///
/// At most `capture_limit` bytes are kept for the capture; the client
/// still receives every byte.
async fn relay_http2_body(
    incoming: Incoming,
    sink: &mut ChannelSink,
    capture_limit: usize,
) -> crate::Result<(Bytes, bool)> {
    let mut incoming = std::pin::pin!(incoming);
    let mut captured = BytesMut::new();

    while let Some(frame) = incoming.frame().await {
        let frame = frame.map_err(|e| {
            crate::Error::InvalidResponse(format!("upstream HTTP/2 body failed: {}", causes(&e)))
        })?;
        // Trailers carry no body bytes; hyper surfaces them separately
        // and they are not part of the captured body.
        if let Some(chunk) = frame.data_ref() {
            let room = capture_limit.saturating_sub(captured.len());
            captured.extend_from_slice(&chunk[..chunk.len().min(room)]);
            sink.write_chunk(chunk).await?;
        }
    }

    // hyper reports a truncated stream as an error, so reaching the end
    // of the frames means the body arrived whole.
    Ok((captured.freeze(), true))
}

/// Build the upstream request for an HTTP/2 origin.
///
/// The authority rides in the URI, which becomes `:authority` — HTTP/2's
/// replacement for the `Host` header. Sending both invites them to
/// disagree, so `Host` is dropped here.
fn build_upstream_request(
    parsed_request: &ParsedRequest,
    body: &Bytes,
) -> crate::Result<hyper::Request<Full<Bytes>>> {
    let authority = parsed_request.header("host").unwrap_or_default();
    let uri = format!("https://{authority}{}", parsed_request.target);

    let mut builder = hyper::Request::builder()
        .method(parsed_request.method.as_str())
        .uri(uri)
        .version(hyper::Version::HTTP_2);

    for (name, value) in &parsed_request.headers {
        if name.eq_ignore_ascii_case("host") || is_connection_specific(name) {
            continue;
        }
        builder = builder.header(name, value);
    }

    builder.body(Full::new(body.clone())).map_err(|e| {
        crate::Error::InvalidRequest(format!("could not build upstream HTTP/2 request: {e}"))
    })
}

/// Turn an HTTP/2 request head into the proxy's own representation.
///
/// The pseudo-headers hyper has already split out become a request line
/// and a `Host` header; `host` and `port` name the origin this session
/// was opened for, and stand in when a request carries neither
/// `:authority` nor `Host`.
fn translate_request(
    parts: &http::request::Parts,
    host: &str,
    port: u16,
) -> crate::Result<ParsedRequest> {
    let target = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_owned(), ToString::to_string);

    // `:authority` is HTTP/2's Host, and outranks a `host` header when a
    // client sends both (RFC 9113 §8.3.1).
    let authority = parts
        .uri
        .authority()
        .map(ToString::to_string)
        .or_else(|| {
            parts
                .headers
                .get(hyper::header::HOST)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| {
            if port == 443 {
                host.to_owned()
            } else {
                format!("{host}:{port}")
            }
        });

    let mut headers = vec![("Host".to_owned(), authority)];
    for (name, value) in &parts.headers {
        let name = name.as_str();
        if name.eq_ignore_ascii_case("host") {
            continue;
        }
        // RFC 9113 §8.2.2: connection-specific headers make an HTTP/2
        // message malformed. They cannot be forwarded, and a client that
        // sent one is not speaking HTTP/2.
        if is_connection_specific(name) {
            return Err(crate::Error::InvalidRequest(format!(
                "connection-specific header {name} is not allowed in HTTP/2"
            )));
        }
        headers.push((
            name.to_owned(),
            String::from_utf8_lossy(value.as_bytes()).into_owned(),
        ));
    }

    Ok(ParsedRequest {
        method: parts.method.to_string(),
        target,
        version: HTTP2_VERSION.to_owned(),
        headers,
        header_len: 0,
    })
}

/// Capture an HTTP/2 response head in the proxy's own representation.
fn parsed_response_from_parts(parts: &http::response::Parts) -> ParsedResponse {
    ParsedResponse {
        status: parts.status.as_u16(),
        reason: parts
            .status
            .canonical_reason()
            .unwrap_or_default()
            .to_owned(),
        version: HTTP2_VERSION.to_owned(),
        headers: parts
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect(),
        header_len: 0,
        body: Bytes::new(),
    }
}

/// Feeds relayed body chunks to hyper.
///
/// The channel is bounded, so a client that stops reading stops the
/// relay reading from upstream rather than buffering the difference.
/// A send that waits past `write_timeout` ends the relay.
struct ChannelSink {
    chunks: mpsc::Sender<crate::Result<Bytes>>,
    write_timeout: Duration,
}

impl BodySink for ChannelSink {
    async fn write_chunk(&mut self, chunk: &[u8]) -> crate::Result<()> {
        let send = self.chunks.send(Ok(Bytes::copy_from_slice(chunk)));
        tokio::time::timeout(self.write_timeout, send)
            .await
            .map_err(|_elapsed| crate::Error::Timeout {
                operation: "write HTTP/2 response body".into(),
                duration: self.write_timeout,
            })?
            .map_err(|_closed| {
                crate::Error::InvalidRequest("client stopped reading the response".to_owned())
            })
    }

    fn decodes_chunked(&self) -> bool {
        true
    }
}

/// A response body delivered to the client as it arrives from upstream.
pub struct StreamedBody {
    chunks: mpsc::Receiver<crate::Result<Bytes>>,
}

impl StreamedBody {
    fn channel() -> (mpsc::Sender<crate::Result<Bytes>>, Self) {
        let (tx, rx) = mpsc::channel(BODY_CHANNEL_DEPTH);
        (tx, Self { chunks: rx })
    }
}

impl Body for StreamedBody {
    type Data = Bytes;
    type Error = crate::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.chunks.poll_recv(cx) {
            Poll::Ready(Some(Ok(chunk))) => Poll::Ready(Some(Ok(Frame::data(chunk)))),
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A response the proxy makes for a stream it could not forward.
fn error_response(error: &crate::Error) -> hyper::Response<StreamedBody> {
    let (status, _reason) = match error {
        crate::Error::BodyTooLarge { .. } => (413, "Content Too Large"),
        other => gateway_error_status(other),
    };
    let detail = Bytes::from(format!("{error}\n"));

    let (tx, body) = StreamedBody::channel();
    let length = detail.len();
    // The channel is empty, so there is room for this one chunk.
    let _ = tx.try_send(Ok(detail));

    let mut response = hyper::Response::new(body);
    *response.status_mut() =
        hyper::StatusCode::from_u16(status).unwrap_or(hyper::StatusCode::BAD_GATEWAY);
    let headers = response.headers_mut();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(hyper::header::CONTENT_LENGTH, length.into());
    response
}

/// Build the client-facing response head.
///
/// Connection-specific headers are dropped rather than forwarded: they
/// are illegal in HTTP/2 (RFC 9113 §8.2.2), so an origin speaking
/// HTTP/1.1 keep-alive MUST NOT have its `Connection` header relayed.
fn build_response(
    parsed_response: &ParsedResponse,
    body: StreamedBody,
) -> hyper::Response<StreamedBody> {
    let mut builder = hyper::Response::builder()
        .status(parsed_response.status)
        .version(hyper::Version::HTTP_2);

    let connection = parsed_response.header("connection");
    // A chunked origin body reaches the client decoded, so a
    // `Content-Length` sent alongside the chunking cannot describe it.
    let chunked = parsed_response.is_chunked();
    for (name, value) in &parsed_response.headers {
        if is_connection_specific(name)
            || is_hop_by_hop(name, connection)
            || (chunked && name.eq_ignore_ascii_case("content-length"))
        {
            continue;
        }
        builder = builder.header(name, value);
    }

    builder.body(body).unwrap_or_else(|e| {
        warn!(error = %e, "failed to build HTTP/2 response; sending 502");
        let (_tx, empty) = StreamedBody::channel();
        let mut fallback = hyper::Response::new(empty);
        *fallback.status_mut() = hyper::StatusCode::BAD_GATEWAY;
        fallback
    })
}

/// Headers HTTP/2 forbids outright (RFC 9113 §8.2.2).
///
/// `TE` is the one exception the RFC allows, and only with the value
/// `trailers`; it is simpler and no less correct to drop it.
/// The largest header block either leg of a session accepts, from
/// [`Config::max_header_size`] as on the HTTP/1.1 path.
///
/// hyper's own default is 16 KiB, which real origins exceed: the responses
/// some large sites send browsers carry more than that in policy and cookie
/// headers.
fn header_list_limit(config: &Config) -> u32 {
    u32::try_from(config.max_header_size).unwrap_or(u32::MAX)
}

/// `error` and each error that caused it, outermost first.
///
/// hyper's own message for an HTTP/2 failure is only "http2 error"; the
/// stream or connection error that says what went wrong is its source.
fn causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let message = cause.to_string();
        if !text.ends_with(&message) {
            text.push_str(": ");
            text.push_str(&message);
        }
        source = cause.source();
    }
    text
}

fn is_connection_specific(name: &str) -> bool {
    const FORBIDDEN: &[&str] = &[
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "upgrade",
    ];
    FORBIDDEN.iter().any(|f| f.eq_ignore_ascii_case(name))
}

/// Collect a request body, refusing one past `max_body_size`.
async fn read_incoming(incoming: Incoming, max_body_size: usize) -> crate::Result<Bytes> {
    let mut incoming = std::pin::pin!(incoming);
    let mut body = BytesMut::new();

    while let Some(frame) = incoming.frame().await {
        let frame = frame.map_err(|e| {
            crate::Error::InvalidRequest(format!("HTTP/2 request body failed: {}", causes(&e)))
        })?;
        if let Some(chunk) = frame.data_ref() {
            if body.len() + chunk.len() > max_body_size {
                return Err(crate::Error::BodyTooLarge {
                    size: body.len() + chunk.len(),
                    max: max_body_size,
                });
            }
            body.extend_from_slice(chunk);
        }
    }

    Ok(body.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;
    use crate::config::Config;
    use crate::proxy::pool::ConnectionPool;
    use crate::tls::CertificateAuthority;
    use crate::tls::TlsConfig;
    use http_body_util::Empty;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::io::DuplexStream;
    use tokio::io::duplex;

    /// Hands out pre-built in-memory upstreams, so the HTTP/2 path can
    /// be driven without a TLS origin the trust store would reject.
    struct ScriptedConnector {
        connections: Mutex<VecDeque<DuplexStream>>,
        protocol: AlpnProtocol,
    }

    impl UpstreamConnector for ScriptedConnector {
        type Stream = DuplexStream;

        fn connect(&self) -> impl Future<Output = crate::Result<Upstream<Self::Stream>>> + Send {
            let next = self.connections.lock().pop_front();
            async move {
                let stream = next.ok_or_else(|| {
                    crate::Error::InvalidResponse("no scripted connection left".into())
                })?;
                Ok(Upstream {
                    stream,
                    remote_addr: None,
                    protocol: self.protocol,
                    pending_setup: Some(SetupTimings {
                        connect_ms: 0.0,
                        ssl_ms: 0.0,
                    }),
                })
            }
        }
    }

    fn proxy_state(session: Option<Session>) -> Arc<ProxyState> {
        let ca = CertificateAuthority::generate().expect("generate CA");
        let mut config = Config::default();
        config.https.intercept = true;
        Arc::new(ProxyState {
            config,
            request_counter: AtomicU64::new(1),
            callbacks: Default::default(),
            connection_pool: ConnectionPool::new(),
            session,
            tls_config: Some(TlsConfig::new(ca)),
        })
    }

    /// Read from `stream` until everything read so far contains `needle`.
    async fn read_until_contains<R>(stream: &mut R, needle: &str) -> String
    where
        R: AsyncRead + Unpin,
    {
        let mut seen = Vec::new();
        let mut scratch = [0u8; 1024];
        loop {
            let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut scratch))
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}"))
                .expect("read");
            assert!(
                n > 0,
                "stream closed before {needle:?}; saw: {}",
                String::from_utf8_lossy(&seen)
            );
            seen.extend_from_slice(&scratch[..n]);
            let text = String::from_utf8_lossy(&seen);
            if text.contains(needle) {
                return text.into_owned();
            }
        }
    }

    /// Drive one HTTP/2 request through the proxy against a scripted
    /// origin, returning what the origin saw and what the client got.
    async fn round_trip<B>(
        request: hyper::Request<B>,
        origin_response: &'static [u8],
        session: Option<Session>,
    ) -> (String, hyper::Response<Incoming>)
    where
        B: Body<Data = Bytes> + Send + Unpin + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let (client_side, proxy_side) = duplex(64 * 1024);
        let (mut origin, upstream) = duplex(64 * 1024);

        let state = proxy_state(session);
        let connector = ScriptedConnector {
            connections: Mutex::new(VecDeque::from(vec![upstream])),
            protocol: AlpnProtocol::Http11,
        };

        tokio::spawn(async move {
            let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");
            serve_with_connector(
                proxy_side,
                connector,
                "example.com",
                443,
                client_addr,
                1.5,
                &state,
            )
            .await
        });

        // Speak real HTTP/2 at the proxy.
        let (mut send_request, connection) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(client_side))
                .await
                .expect("client handshake");
        tokio::spawn(connection);

        let pending = tokio::spawn(async move { send_request.send_request(request).await });

        let seen = read_until_contains(&mut origin, "\r\n\r\n").await;
        origin.write_all(origin_response).await.expect("respond");

        let response = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("response timed out")
            .expect("joined")
            .expect("response");

        (seen, response)
    }

    fn get_request() -> hyper::Request<Empty<Bytes>> {
        hyper::Request::builder()
            .method("GET")
            .uri("https://example.com/thing?q=1")
            .body(Empty::new())
            .expect("request")
    }

    #[tokio::test]
    async fn test_http2_request_is_forwarded_to_the_origin_as_http1() {
        let (seen, response) = round_trip(
            get_request(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
            None,
        )
        .await;

        assert!(
            seen.starts_with("GET /thing?q=1 HTTP/1.1\r\n"),
            "the origin leg speaks HTTP/1.1 in origin-form: {seen}"
        );
        assert!(
            seen.contains("Host: example.com\r\n"),
            ":authority becomes the Host header: {seen}"
        );
        assert_eq!(response.status(), 200);

        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        assert_eq!(body, Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn test_unreachable_origin_answers_the_stream_with_a_502() {
        let session = Session::new();
        let (client_side, proxy_side) = duplex(64 * 1024);
        let state = proxy_state(Some(session.clone()));
        // No connections to hand out: every connect fails.
        let connector = ScriptedConnector {
            connections: Mutex::new(VecDeque::new()),
            protocol: AlpnProtocol::Http11,
        };
        tokio::spawn(async move {
            let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");
            serve_with_connector(
                proxy_side,
                connector,
                "example.com",
                443,
                client_addr,
                0.0,
                &state,
            )
            .await
        });

        let (mut send_request, connection) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(client_side))
                .await
                .expect("client handshake");
        tokio::spawn(connection);

        let response = tokio::time::timeout(
            Duration::from_secs(2),
            send_request.send_request(get_request()),
        )
        .await
        .expect("response timed out")
        .expect("a response, not a reset stream");

        assert_eq!(response.status(), 502);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        assert!(
            String::from_utf8_lossy(&body).contains("no scripted connection left"),
            "the reason is given"
        );

        let captured = session.exchanges();
        assert_eq!(captured.len(), 1, "the failed exchange is recorded");
        assert!(captured[0].exchange.error.is_some());
    }

    #[tokio::test]
    async fn test_chunked_origin_body_reaches_an_http2_client_decoded() {
        // HTTP/2 frames bodies itself. Chunk-size lines relayed into DATA
        // frames would become part of the body the client sees.
        let (_seen, response) = round_trip(
            get_request(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
              5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
            None,
        )
        .await;

        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        assert_eq!(body, Bytes::from_static(b"hello world"));
    }

    #[tokio::test]
    async fn test_truncated_origin_body_resets_the_http2_stream() {
        // The origin promises ten bytes and hangs up after three. A clean
        // end of stream would tell the client those three are the body.
        let (_seen, response) = round_trip(
            get_request(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc",
            None,
        )
        .await;

        assert!(
            response.into_body().collect().await.is_err(),
            "a truncated body must not look complete"
        );
    }

    #[tokio::test]
    async fn test_http2_request_body_is_framed_for_an_http1_origin() {
        // No content-length from the client: HTTP/2 does not need one,
        // but the HTTP/1.1 origin does.
        let request = hyper::Request::builder()
            .method("POST")
            .uri("https://example.com/submit")
            .body(Full::new(Bytes::from_static(b"payload")))
            .expect("request");

        let (seen, response) = round_trip(
            request,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            None,
        )
        .await;

        assert!(seen.contains("Content-Length: 7\r\n"), "got: {seen}");
        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn test_http2_response_omits_connection_specific_headers() {
        // The origin keeps its HTTP/1.1 keep-alive headers to itself:
        // relaying them would make the HTTP/2 response malformed.
        let (_seen, response) = round_trip(
            get_request(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\
              Transfer-Encoding: chunked\r\n\r\nhi",
            None,
        )
        .await;

        assert!(response.headers().get("connection").is_none());
        assert!(response.headers().get("transfer-encoding").is_none());
        assert_eq!(response.version(), hyper::Version::HTTP_2);
    }

    #[tokio::test]
    async fn test_http2_exchange_is_captured_as_http2() {
        let session = Session::new();
        let (_seen, response) = round_trip(
            get_request(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
            Some(session.clone()),
        )
        .await;
        // Draining the body is what completes the exchange.
        let _ = response.into_body().collect().await.expect("body");

        // The relay records the exchange after the body finishes.
        for _ in 0..50 {
            if !session.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let captured = session.exchanges();
        assert_eq!(captured.len(), 1, "the exchange reached the session");
        let exchange = &captured[0].exchange;
        assert_eq!(exchange.request.version, "HTTP/2");
        assert_eq!(exchange.request.method, "GET");
        assert_eq!(
            exchange.request.url.as_str(),
            "https://example.com/thing?q=1"
        );
        let response = exchange.response.as_ref().expect("response captured");
        assert_eq!(response.version, "HTTP/2");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, Bytes::from_static(b"hello"));
    }

    /// What an HTTP/2 origin saw of a forwarded request.
    #[derive(Debug)]
    struct SeenByOrigin {
        method: String,
        target: String,
        version: hyper::Version,
        authority: Option<String>,
        has_host_header: bool,
    }

    /// Spawn an HTTP/2 origin on `stream` that answers every stream with
    /// `body` and reports what it received.
    fn spawn_http2_origin(
        stream: DuplexStream,
        body: &'static [u8],
    ) -> mpsc::Receiver<SeenByOrigin> {
        let (seen_tx, seen_rx) = mpsc::channel(8);

        tokio::spawn(async move {
            let service = service_fn(move |request: hyper::Request<Incoming>| {
                let seen_tx = seen_tx.clone();
                async move {
                    let seen = SeenByOrigin {
                        method: request.method().to_string(),
                        target: request
                            .uri()
                            .path_and_query()
                            .map(ToString::to_string)
                            .unwrap_or_default(),
                        version: request.version(),
                        authority: request.uri().authority().map(ToString::to_string),
                        has_host_header: request.headers().contains_key(hyper::header::HOST),
                    };
                    let _ = seen_tx.send(seen).await;
                    Ok::<_, crate::Error>(hyper::Response::new(Full::new(Bytes::from_static(body))))
                }
            });

            let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });

        seen_rx
    }

    /// Start a proxy session whose scripted origin speaks HTTP/2, and
    /// return a client handle onto it.
    async fn http2_origin_session(
        session: Option<Session>,
        origin_body: &'static [u8],
    ) -> (SendRequest<Empty<Bytes>>, mpsc::Receiver<SeenByOrigin>) {
        let (client_side, proxy_side) = duplex(64 * 1024);
        let (origin_side, upstream) = duplex(64 * 1024);

        let seen = spawn_http2_origin(origin_side, origin_body);

        let state = proxy_state(session);
        let connector = ScriptedConnector {
            connections: Mutex::new(VecDeque::from(vec![upstream])),
            protocol: AlpnProtocol::Http2,
        };

        tokio::spawn(async move {
            let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");
            serve_with_connector(
                proxy_side,
                connector,
                "example.com",
                443,
                client_addr,
                1.5,
                &state,
            )
            .await
        });

        let (send_request, connection) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(client_side))
                .await
                .expect("client handshake");
        tokio::spawn(connection);

        (send_request, seen)
    }

    #[tokio::test]
    async fn test_http2_origin_is_spoken_to_in_http2() {
        let (mut send_request, mut seen) = http2_origin_session(None, b"from h2").await;

        let response = send_request
            .send_request(get_request())
            .await
            .expect("response");
        assert_eq!(response.status(), 200);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        assert_eq!(body, Bytes::from_static(b"from h2"));

        let seen = tokio::time::timeout(Duration::from_secs(2), seen.recv())
            .await
            .expect("origin saw the request")
            .expect("request");

        assert_eq!(
            seen.version,
            hyper::Version::HTTP_2,
            "the upstream leg is HTTP/2"
        );
        assert_eq!(seen.method, "GET");
        assert_eq!(seen.target, "/thing?q=1");
        assert_eq!(
            seen.authority.as_deref(),
            Some("example.com"),
            "the authority rides in :authority"
        );
        assert!(
            !seen.has_host_header,
            "sending Host alongside :authority invites the two to disagree"
        );
    }

    #[tokio::test]
    async fn test_concurrent_streams_share_one_upstream_connection() {
        // Exactly one upstream connection is scripted, so a second
        // in-flight stream needing its own would fail outright. Passing
        // is what multiplexing looks like.
        let (send_request, mut seen) = http2_origin_session(None, b"ok").await;

        let mut first = send_request.clone();
        let mut second = send_request.clone();
        let a = tokio::spawn(async move { first.send_request(get_request()).await });
        let b = tokio::spawn(async move { second.send_request(get_request()).await });

        let first = tokio::time::timeout(Duration::from_secs(2), a)
            .await
            .expect("first stream")
            .expect("joined")
            .expect("response");
        let second = tokio::time::timeout(Duration::from_secs(2), b)
            .await
            .expect("second stream")
            .expect("joined")
            .expect("response");

        assert_eq!(first.status(), 200);
        assert_eq!(second.status(), 200);
        assert!(seen.recv().await.is_some());
        assert!(seen.recv().await.is_some());
    }

    #[tokio::test]
    async fn test_large_header_blocks_cross_both_http2_legs() {
        // Past hyper's 16 KiB default, under the 64 KiB `max_header_size`.
        let big = "x".repeat(40 * 1024);

        let (client_side, proxy_side) = duplex(256 * 1024);
        let (origin_side, upstream) = duplex(256 * 1024);

        let reply = big.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request: hyper::Request<Incoming>| {
                let reply = reply.clone();
                async move {
                    let cookie = request
                        .headers()
                        .get(hyper::header::COOKIE)
                        .map_or(0, |v| v.len());
                    let response = hyper::Response::builder()
                        .header("content-security-policy", reply)
                        .header("x-cookie-length", cookie.to_string())
                        .body(Full::new(Bytes::from_static(b"ok")))
                        .expect("response");
                    Ok::<_, crate::Error>(response)
                }
            });
            let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                .max_header_list_size(256 * 1024)
                .serve_connection(TokioIo::new(origin_side), service)
                .await;
        });

        let state = proxy_state(None);
        let connector = ScriptedConnector {
            connections: Mutex::new(VecDeque::from(vec![upstream])),
            protocol: AlpnProtocol::Http2,
        };
        tokio::spawn(async move {
            let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");
            serve_with_connector(
                proxy_side,
                connector,
                "example.com",
                443,
                client_addr,
                1.5,
                &state,
            )
            .await
        });

        // A browser accepts large header blocks; Chrome allows 256 KiB.
        let (mut send_request, connection) =
            hyper::client::conn::http2::Builder::new(TokioExecutor::new())
                .max_header_list_size(256 * 1024)
                .handshake(TokioIo::new(client_side))
                .await
                .expect("client handshake");
        tokio::spawn(connection);

        let request = hyper::Request::builder()
            .method("GET")
            .uri("https://example.com/")
            .header(hyper::header::COOKIE, "c".repeat(30 * 1024))
            .body(Empty::<Bytes>::new())
            .expect("request");
        let response =
            tokio::time::timeout(Duration::from_secs(2), send_request.send_request(request))
                .await
                .expect("response timed out")
                .expect("response");

        assert_eq!(response.status(), 200);
        assert_eq!(
            response
                .headers()
                .get("content-security-policy")
                .map(|v| v.len()),
            Some(big.len()),
            "the origin's large header reaches the client"
        );
        assert_eq!(
            response
                .headers()
                .get("x-cookie-length")
                .and_then(|v| v.to_str().ok()),
            Some("30720"),
            "the client's large cookie reaches the origin"
        );
    }

    #[tokio::test]
    async fn test_http2_to_http2_exchange_is_captured_on_both_legs() {
        let session = Session::new();
        let (mut send_request, _seen) =
            http2_origin_session(Some(session.clone()), b"payload").await;

        let response = send_request
            .send_request(get_request())
            .await
            .expect("response");
        let _ = response.into_body().collect().await.expect("body");

        for _ in 0..50 {
            if !session.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let captured = session.exchanges();
        assert_eq!(captured.len(), 1);
        let exchange = &captured[0].exchange;
        assert_eq!(exchange.request.version, "HTTP/2");
        let response = exchange.response.as_ref().expect("response captured");
        assert_eq!(
            response.version, "HTTP/2",
            "both legs are HTTP/2, so the capture says so without qualification"
        );
        assert_eq!(response.body, Bytes::from_static(b"payload"));
    }

    fn parts_of(request: hyper::Request<Empty<Bytes>>) -> http::request::Parts {
        request.into_parts().0
    }

    #[test]
    fn test_translate_request_rejects_connection_specific_headers() {
        // RFC 9113 §8.2.2: such a message is malformed, not merely odd.
        // A conformant client will not send one, which is exactly why
        // this is checked here rather than through one.
        let parts = parts_of(
            hyper::Request::builder()
                .uri("https://example.com/x")
                .header("proxy-connection", "keep-alive")
                .body(Empty::new())
                .expect("request"),
        );

        let err = translate_request(&parts, "example.com", 443).expect_err("must be rejected");
        assert!(matches!(err, crate::Error::InvalidRequest(_)), "got {err}");
    }

    #[test]
    fn test_translate_request_uses_the_authority_as_host() {
        let parts = parts_of(
            hyper::Request::builder()
                .uri("https://example.com/a/b?q=1")
                .body(Empty::new())
                .expect("request"),
        );

        let translated = translate_request(&parts, "fallback.example", 443).expect("translated");
        assert_eq!(translated.target, "/a/b?q=1");
        assert_eq!(translated.header("host"), Some("example.com"));
    }

    #[test]
    fn test_translate_request_falls_back_to_the_session_origin() {
        // A request with neither `:authority` nor `Host` still has to
        // name a host upstream; the session was opened for one.
        let parts = parts_of(
            hyper::Request::builder()
                .uri("/just/a/path")
                .body(Empty::new())
                .expect("request"),
        );

        let translated = translate_request(&parts, "example.com", 8443).expect("translated");
        assert_eq!(translated.target, "/just/a/path");
        assert_eq!(translated.header("host"), Some("example.com:8443"));
    }

    #[test]
    fn test_translate_request_keeps_one_host_header() {
        let parts = parts_of(
            hyper::Request::builder()
                .uri("https://example.com/x")
                .header("host", "stale.example")
                .body(Empty::new())
                .expect("request"),
        );

        let translated = translate_request(&parts, "example.com", 443).expect("translated");
        let hosts: Vec<&str> = translated
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(hosts, vec!["example.com"], ":authority wins");
    }

    #[test]
    fn test_connection_specific_headers_are_rejected() {
        for name in [
            "Connection",
            "keep-alive",
            "Proxy-Connection",
            "Transfer-Encoding",
            "Upgrade",
        ] {
            assert!(is_connection_specific(name), "{name} must be rejected");
        }
    }

    #[test]
    fn test_ordinary_headers_are_not_connection_specific() {
        for name in ["content-type", "accept", "authorization", "content-length"] {
            assert!(!is_connection_specific(name), "{name} must be allowed");
        }
    }
}
