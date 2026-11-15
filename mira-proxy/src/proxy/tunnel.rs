//! TCP tunnel for HTTPS CONNECT requests and WebSocket upgrades.

use crate::config::Config;
use crate::http::BodyFraming;
use crate::http::Exchange;
use crate::http::ForwardedBody;
use crate::http::HEADERS_TOO_LARGE_RESPONSE;
use crate::http::HeadRead;
use crate::http::ParsedRequest;
use crate::http::ParsedResponse;
use crate::http::Request;
use crate::http::RequestSent;
use crate::http::Response;
use crate::http::Timings;
use crate::http::WriteDeadline;
use crate::http::apply_request_proxy_headers;
use crate::http::apply_response_proxy_headers;
use crate::http::can_reuse_connection;
use crate::http::client_keeps_alive;
use crate::http::forward_body;
use crate::http::read_body;
use crate::http::read_request_head;
use crate::http::response_has_no_body;
use crate::http::send_continue_if_expected;
use crate::http::send_gateway_error;
use crate::http::send_proxy_error;
use crate::http::serialize_request_head;
use crate::http::serialize_response_head;
use crate::http::serialize_upgrade_request_head;
use crate::http::write_all_within;
use crate::http::write_request_and_parse_head;
use crate::proxy::http2;
use crate::proxy::magic;
use crate::proxy::prefixed::PrefixedStream;
use crate::proxy::server::ProxyState;
use crate::tls::AlpnProtocol;
use bytes::Bytes;
use bytes::BytesMut;
use parking_lot::Mutex;
use rustls::pki_types::InvalidDnsNameError;
use rustls::pki_types::ServerName;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::debug;
use tracing::info;
use tracing::trace;
use tracing::warn;
use url::Url;

/// Open a TCP connection to the origin within `connect_timeout`.
async fn connect_tcp(
    host: &str,
    port: u16,
    config: &Config,
    operation: &'static str,
) -> crate::Result<TcpStream> {
    timeout(config.connect_timeout, TcpStream::connect((host, port)))
        .await
        .map_err(|_elapsed| crate::Error::Timeout {
            operation: operation.into(),
            duration: config.connect_timeout,
        })?
        .map_err(|e| crate::Error::UpstreamConnection {
            host: format!("{host}:{port}"),
            source: e,
        })
}

/// Open a passthrough tunnel: connect to the origin and tell the client
/// `200 Connection Established`.
///
/// A failure to connect is answered with a `502` or `504` before it is
/// returned. Bytes then flow through [`relay`].
pub async fn open_passthrough(
    client: &mut TcpStream,
    host: &str,
    port: u16,
    config: &Config,
) -> crate::Result<TcpStream> {
    debug!(host, port, "establishing tunnel");

    let upstream = match connect_tcp(host, port, config, "tunnel connect").await {
        Ok(upstream) => upstream,
        Err(e) => {
            // The client has not been told 200 yet, so it can still
            // learn why the tunnel failed.
            send_gateway_error(client, &e, config.write_timeout).await;
            return Err(e);
        }
    };

    write_all_within(
        client,
        b"HTTP/1.1 200 Connection Established\r\n\r\n",
        config.write_timeout,
        "write CONNECT response",
    )
    .await?;

    Ok(upstream)
}

/// The response a passthrough `CONNECT` is recorded with: the proxy's own
/// `200`, timed by how long the origin took to accept the connection.
pub fn tunnel_response(request_id: u64, start: Instant, upstream: &TcpStream) -> Response {
    let parsed = ParsedResponse {
        status: 200,
        reason: "Connection Established".to_owned(),
        version: "HTTP/1.1".to_owned(),
        headers: Vec::new(),
        header_len: 0,
        body: Bytes::new(),
    };
    let connect_ms = start.elapsed().as_secs_f64() * 1000.0;
    let timings = Timings {
        blocked_ms: Some(0.0),
        connect_ms: Some(connect_ms),
        send_ms: 0.0,
        wait_ms: 0.0,
        receive_ms: 0.0,
        ssl_ms: None,
        connection_reused: false,
    };
    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    Response::from_parsed_with_timings(
        &parsed,
        request_id,
        duration_ms,
        Some(timings),
        upstream.peer_addr().ok(),
    )
}

/// Open a plain WebSocket connection: connect to the origin, forward the
/// upgrade request, and relay its answer to the client.
///
/// A failure to connect is answered with a `502` or `504` before it is
/// returned. When the origin switches protocols, bytes then flow through
/// [`relay`].
pub async fn open_websocket(
    client: &mut TcpStream,
    host: &str,
    port: u16,
    request_headers: &[u8],
    config: &Config,
) -> crate::Result<(TcpStream, Upgrade)> {
    debug!(host, port, "establishing WebSocket tunnel");

    let mut upstream = match connect_tcp(host, port, config, "websocket connect").await {
        Ok(upstream) => upstream,
        Err(e) => {
            send_gateway_error(client, &e, config.write_timeout).await;
            return Err(e);
        }
    };

    let upgrade = upgrade_handshake(client, &mut upstream, request_headers, config).await?;
    Ok((upstream, upgrade))
}

/// What the origin said to an upgrade request.
pub struct Upgrade {
    /// The origin's response, with the body of a rejection captured.
    pub response: ParsedResponse,

    /// Whether the origin switched protocols. If so, the connection now
    /// carries the new protocol and only [`relay`] can serve it.
    pub switched: bool,
}

/// Forward an upgrade request and relay the origin's answer to the client.
///
/// The request goes out as given, preserving `Connection` and `Upgrade`.
/// A `101` is relayed with whatever followed it, which is already the
/// new protocol. Anything else is an ordinary response and is relayed in
/// full.
async fn upgrade_handshake<C, U>(
    client: &mut C,
    upstream: &mut U,
    request_headers: &[u8],
    config: &Config,
) -> crate::Result<Upgrade>
where
    C: AsyncWrite + Unpin + Send,
    U: AsyncRead + AsyncWrite + Unpin,
{
    write_all_within(
        upstream,
        request_headers,
        config.write_timeout,
        "write websocket upgrade request",
    )
    .await?;

    let mut response_buf = BytesMut::with_capacity(4096);
    let parsed_response = loop {
        // Parse before checking the size: a short head can share a read
        // with plenty of what follows it.
        if let Some(head) = ParsedResponse::parse_headers(&response_buf)? {
            break head;
        }
        if response_buf.len() > config.max_header_size {
            return Err(crate::Error::HeadersTooLarge {
                size: response_buf.len(),
                max: config.max_header_size,
            });
        }
        let read_future = upstream.read_buf(&mut response_buf);
        let n = timeout(config.read_timeout, read_future)
            .await
            .map_err(|_elapsed| crate::Error::Timeout {
                operation: "read websocket upgrade response".into(),
                duration: config.read_timeout,
            })??;
        if n == 0 {
            return Err(crate::Error::InvalidResponse(
                "connection closed before WebSocket upgrade response".into(),
            ));
        }
    };

    if parsed_response.status == 101 {
        write_all_within(
            client,
            &response_buf,
            config.write_timeout,
            "write websocket upgrade response",
        )
        .await?;
        return Ok(Upgrade {
            response: parsed_response,
            switched: true,
        });
    }

    let body =
        forward_rejected_upgrade(client, upstream, &response_buf, &parsed_response, config).await?;
    Ok(Upgrade {
        response: parsed_response.with_body(body),
        switched: false,
    })
}

/// Copy bytes both ways until either side closes. `what` names the
/// tunnel in logs.
pub async fn relay<A, B>(a: &mut A, b: &mut B, what: &'static str)
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    match tokio::io::copy_bidirectional(a, b).await {
        Ok((a_to_b, b_to_a)) => trace!(what, a_to_b, b_to_a, "tunnel closed"),
        // A reset is how many tunnels end.
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(e) => debug!(what, error = %e, "tunnel error"),
    }
}

/// Establish an intercepted HTTPS tunnel.
///
/// This performs TLS termination on the client side and TLS origination
/// to the upstream, allowing the proxy to inspect HTTPS traffic.
///
/// The flow is:
/// 1. Send "200 Connection Established" to client
/// 2. Perform TLS handshake with client using a generated certificate
/// 3. Connect to upstream server with TLS
/// 4. Process HTTP requests over the decrypted streams
pub async fn establish_intercepted(
    mut client: TcpStream,
    early_bytes: Bytes,
    host: &str,
    port: u16,
    client_addr: SocketAddr,
    state: &Arc<ProxyState>,
) -> crate::Result<()> {
    // Callers check for TLS before routing here; this is a backstop.
    let tls_config = state
        .tls_config
        .as_ref()
        .ok_or_else(|| crate::Error::TlsConfig("TLS config not initialized".to_owned()))?;

    debug!(host, port, "establishing intercepted HTTPS tunnel");

    // Build the TLS acceptor *before* acknowledging the CONNECT. If leaf
    // certificate generation fails the client is still speaking plain
    // HTTP, so we can return a real 502 response with a diagnosable
    // reason instead of sending 200 and then leaving them in a broken
    // half-TLS state.
    // The magic domain's pages are written as HTTP/1.1 responses
    // directly onto the stream, so that session is offered HTTP/1.1
    // only; everything else may negotiate HTTP/2.
    let built = if magic::is_magic_domain(host) {
        tls_config.acceptor_for_http1_only(host)
    } else {
        tls_config.acceptor_for_host(host)
    };
    let acceptor = match built {
        Ok(acceptor) => acceptor,
        Err(e) => {
            warn!(host, port, error = %e, "failed to build TLS acceptor for intercepted CONNECT");
            let _ = client
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\n\
                      Content-Length: 0\r\n\
                      Connection: close\r\n\
                      \r\n",
                )
                .await;
            return Err(e);
        }
    };

    // Now that we know we can terminate TLS, commit the client to the
    // tunnel and perform the handshake.
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;

    // Perform TLS handshake with client (we act as server). The socket
    // is moved in rather than borrowed: an HTTP/2 session hands the
    // stream to hyper, which owns it for the connection's lifetime.
    //
    // Bounded like a request head: a client that opens the tunnel and
    // never sends a ClientHello would otherwise hold a connection slot
    // for as long as it likes.
    let tls_start = Instant::now();
    let handshake_timeout = state.config.read_timeout;
    // Bytes the client sent along with its CONNECT are the start of its
    // ClientHello, so the handshake MUST read them first.
    let client = PrefixedStream::new(early_bytes, client);
    let mut client_tls = timeout(handshake_timeout, acceptor.accept(client))
        .await
        .map_err(|_elapsed| crate::Error::Timeout {
            operation: "client TLS handshake".into(),
            duration: handshake_timeout,
        })?
        .map_err(|e| crate::Error::TlsHandshake(format!("client handshake failed: {e}")))?;
    let client_handshake_ms = tls_start.elapsed().as_secs_f64() * 1000.0;

    // What the client agreed to speak inside the tunnel.
    let client_protocol = AlpnProtocol::from_negotiated(client_tls.get_ref().1.alpn_protocol());

    debug!(
        host,
        port,
        protocol = client_protocol.as_str(),
        handshake_ms = %format!("{client_handshake_ms:.2}"),
        "client TLS handshake complete"
    );

    // Magic domain over HTTPS: there is no real upstream at mira.proxy,
    // so serve the CA download / info page locally once the client TLS
    // handshake succeeds (which requires the client to already trust the
    // Mira CA — browsers will reject otherwise, which is the expected
    // behavior for a user who is here TO get the CA).
    if magic::is_magic_domain(host) {
        debug!(host, "serving magic domain over intercepted HTTPS");
        return serve_intercepted_magic_domain(&mut client_tls, state).await;
    }

    // HTTP/2 carries concurrent streams over this one connection, which
    // is a different shape entirely from the sequential request/response
    // loop below. hyper owns the frame layer there; the forwarding
    // underneath is the same code.
    if client_protocol == AlpnProtocol::Http2 {
        return http2::serve_intercepted(
            client_tls,
            host,
            port,
            client_addr,
            client_handshake_ms,
            state,
        )
        .await;
    }

    // The tunnel holds on to the *connector* rather than to a single
    // connection: an origin may close its side at any point (after
    // `Connection: close`, or on its own keep-alive idle timeout) while
    // the client's tunnel lives on, so upstream connections MUST be
    // replaceable. The shared TLS connector loads the system trust store
    // once and caches it, so reconnecting stays cheap.
    //
    // The first connection is opened when the first request arrives, not
    // here: a failure to reach the origin then has a request to answer
    // with a `502`. The client-side handshake is a one-time cost of the
    // tunnel itself, so that first connection carries it.
    let upstream_connector = TlsUpstreamConnector::new(host, port, AlpnProtocol::Http11, state)?
        .carrying_client_handshake(client_handshake_ms);

    process_intercepted_requests(
        &mut client_tls,
        &upstream_connector,
        None,
        host,
        port,
        client_addr,
        state,
    )
    .await
}

/// One-time cost of establishing an upstream connection (TCP connect +
/// TLS handshake), attributed to the first request that runs on it.
/// Later requests on the same connection see `None` and report a reused
/// connection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SetupTimings {
    pub(crate) connect_ms: f64,
    pub(crate) ssl_ms: f64,
}

/// A live upstream connection held by an intercepted tunnel.
pub(crate) struct Upstream<S> {
    pub(crate) stream: S,
    pub(crate) remote_addr: Option<SocketAddr>,

    /// What this connection negotiated. An origin offered HTTP/2 may
    /// still pick HTTP/1.1, so the caller learns which leg it got rather
    /// than assuming.
    pub(crate) protocol: AlpnProtocol,

    /// Setup cost, consumed by the first request sent on this
    /// connection. Its presence therefore also means "nothing has been
    /// sent here yet".
    pub(crate) pending_setup: Option<SetupTimings>,
}

impl<S> Upstream<S> {
    /// Whether this connection is still untouched — no request has been
    /// written to it, so the origin has had no opportunity to close it
    /// out from under us mid-session.
    pub(crate) fn is_unused(&self) -> bool {
        self.pending_setup.is_some()
    }
}

/// Opens upstream connections for an intercepted tunnel.
///
/// The request loop is written against this rather than a single stream
/// so it can replace a dead connection mid-tunnel, and so it can be
/// exercised over in-memory streams in tests.
pub(crate) trait UpstreamConnector {
    type Stream: AsyncRead + AsyncWrite + Unpin;

    fn connect(&self) -> impl Future<Output = crate::Result<Upstream<Self::Stream>>> + Send;
}

/// Connects to the origin over TLS, timing the TCP connect separately
/// from the handshake so HAR can report a real `connect_ms`.
///
/// Owns everything it needs: the HTTP/2 path shares one of these across
/// concurrently served streams, which outlive any borrow of the tunnel's
/// stack frame.
pub(crate) struct TlsUpstreamConnector {
    host: String,
    port: u16,
    server_name: ServerName<'static>,
    connector: tokio_rustls::TlsConnector,
    config: Config,

    /// Client-side handshake time, added to the first connection's setup.
    client_handshake_ms: Mutex<Option<f64>>,
}

impl TlsUpstreamConnector {
    /// Build a connector for one origin, offering `offer` (and HTTP/1.1
    /// beneath it) to the origin's ALPN.
    pub(crate) fn new(
        host: &str,
        port: u16,
        offer: AlpnProtocol,
        state: &ProxyState,
    ) -> crate::Result<Self> {
        let tls_config = state
            .tls_config
            .as_ref()
            .ok_or_else(|| crate::Error::TlsConfig("TLS config not initialized".to_owned()))?;

        let server_name: ServerName<'static> =
            host.to_owned()
                .try_into()
                .map_err(|e: InvalidDnsNameError| {
                    crate::Error::TlsConfig(format!("invalid server name: {host}: {e}"))
                })?;

        Ok(Self {
            host: host.to_owned(),
            port,
            server_name,
            connector: tls_config.upstream_connector_for(offer)?,
            config: state.config.clone(),
            client_handshake_ms: Mutex::new(None),
        })
    }

    /// Attribute the client-side handshake to the first connection made.
    pub(crate) fn carrying_client_handshake(self, handshake_ms: f64) -> Self {
        *self.client_handshake_ms.lock() = Some(handshake_ms);
        self
    }
}

impl UpstreamConnector for TlsUpstreamConnector {
    type Stream = tokio_rustls::client::TlsStream<TcpStream>;

    async fn connect(&self) -> crate::Result<Upstream<Self::Stream>> {
        let tcp_start = Instant::now();
        let tcp = timeout(
            self.config.connect_timeout,
            TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        .map_err(|_elapsed| crate::Error::Timeout {
            operation: "upstream connect".into(),
            duration: self.config.connect_timeout,
        })?
        .map_err(|e| crate::Error::UpstreamConnection {
            host: format!("{}:{}", self.host, self.port),
            source: e,
        })?;
        let connect_ms = tcp_start.elapsed().as_secs_f64() * 1000.0;

        // Capture the remote server address before wrapping in TLS.
        let remote_addr = tcp.peer_addr().ok();

        let handshake_start = Instant::now();
        let stream = timeout(
            self.config.connect_timeout,
            self.connector.connect(self.server_name.clone(), tcp),
        )
        .await
        .map_err(|_elapsed| crate::Error::Timeout {
            operation: "upstream TLS handshake".into(),
            duration: self.config.connect_timeout,
        })?
        .map_err(|e| crate::Error::TlsHandshake(format!("upstream handshake failed: {e}")))?;
        let ssl_ms = handshake_start.elapsed().as_secs_f64() * 1000.0
            + self.client_handshake_ms.lock().take().unwrap_or_default();

        let protocol = AlpnProtocol::from_negotiated(stream.get_ref().1.alpn_protocol());

        debug!(
            host = self.host.as_str(),
            port = self.port,
            protocol = protocol.as_str(),
            handshake_ms = %format!("{ssl_ms:.2}"),
            "upstream TLS handshake complete"
        );

        Ok(Upstream {
            stream,
            remote_addr,
            protocol,
            pending_setup: Some(SetupTimings { connect_ms, ssl_ms }),
        })
    }
}

/// Process HTTP requests over an intercepted TLS session.
///
/// `upstream` is the connection opened while establishing the tunnel;
/// further ones are opened through `connector` whenever the current
/// connection is spent.
async fn process_intercepted_requests<C, K>(
    client: &mut C,
    connector: &K,
    initial_upstream: Option<Upstream<K::Stream>>,
    host: &str,
    port: u16,
    client_addr: SocketAddr,
    state: &Arc<ProxyState>,
) -> crate::Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin + Send,
    K: UpstreamConnector,
{
    let config = &state.config;
    let mut buf = BytesMut::with_capacity(8192);

    // The connection the tunnel currently holds, if any. `None` means
    // the last exchange ended it (`Connection: close`, an unframed body,
    // or a failed reuse) and the next request MUST open a fresh one.
    let mut upstream = initial_upstream;

    loop {
        // Read request headers under a total deadline — not a per-read
        // timeout — so a slowloris-style byte-drip attacker can't reset
        // the clock by dribbling bytes in forever.
        let Some(mut parsed_request) =
            read_intercepted_request_headers(client, &mut buf, config, host, port).await?
        else {
            return Ok(());
        };

        // Inside an intercepted tunnel the target is normally already
        // origin-form, but a client is free to send absolute-form here
        // too — normalize so the origin never sees it.
        parsed_request.rewrite_to_origin_form();

        if let Some(reason) = parsed_request.framing_error() {
            warn!(
                host,
                port, reason, "rejecting request with ambiguous framing"
            );
            send_proxy_error(client, 400, "Bad Request", reason, config.write_timeout).await;
            return Ok(());
        }

        let request_id = state.next_request_id();

        info!(
            id = request_id,
            method = %parsed_request.method,
            target = %parsed_request.target,
            host,
            "HTTPS request"
        );

        // WebSocket upgrade over intercepted HTTPS (wss://). Once the
        // server returns 101, the stream stops being HTTP/1.1 and the
        // request/response loop can no longer parse it — hand off to a
        // bidirectional copy. Use the raw upgrade bytes (rather than the
        // hop-by-hop-stripped serializer) because `Connection: Upgrade`
        // and `Upgrade: websocket` are exactly what makes the handshake
        // work.
        if parsed_request.is_websocket_upgrade() {
            let upgrade_bytes =
                serialize_upgrade_request_head(&buf[..parsed_request.header_len], &parsed_request);
            let _ = buf.split_to(parsed_request.header_len);

            let request =
                Request::from_parsed(&parsed_request, request_id, client_addr, Bytes::new(), true);
            if let Some(callback) = &state.callbacks.on_request {
                callback.invoke(request.clone()).await;
            }

            info!(
                id = request_id,
                host, port, "WebSocket upgrade (intercepted HTTPS)"
            );

            // The upgrade takes the connection over for good, so it has
            // to run on one we know is live: a connection that already
            // carried a request may have been closed by the origin
            // since, and there is no retry once we stop speaking HTTP.
            let mut conn = match upstream.take() {
                Some(conn) if conn.is_unused() => conn,
                _ => match connector.connect().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        send_gateway_error(client, &e, config.write_timeout).await;
                        return Err(e);
                    }
                },
            };

            let start = Instant::now();
            let upgraded =
                upgrade_handshake(client, &mut conn.stream, &upgrade_bytes, config).await;
            let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            state
                .record(match &upgraded {
                    Ok(upgrade) => Exchange {
                        request,
                        response: Some(Response::from_parsed_with_timings(
                            &upgrade.response,
                            request_id,
                            duration_ms,
                            None,
                            conn.remote_addr,
                        )),
                        error: None,
                    },
                    Err(e) => Exchange {
                        request,
                        response: None,
                        error: Some(e.to_string()),
                    },
                })
                .await;

            // A rejected upgrade ends the session, as the client is not
            // expecting to go on speaking HTTP.
            if upgraded?.switched {
                info!(
                    id = request_id,
                    "WebSocket tunnel established over intercepted HTTPS"
                );
                relay(client, &mut conn.stream, "intercepted WebSocket").await;
            }
            return Ok(());
        }

        send_continue_if_expected(client, &parsed_request, config.max_body_size).await?;
        let request_body =
            match read_body(client, &mut buf, &parsed_request, config.max_body_size).await {
                Ok(body) => body,
                Err(crate::Error::BodyTooLarge { size, max }) => {
                    warn!(host, port, size, max, "request body too large; closing");
                    let detail = format!("request body exceeds the proxy's {max}-byte limit");
                    send_proxy_error(
                        client,
                        413,
                        "Content Too Large",
                        &detail,
                        config.write_timeout,
                    )
                    .await;
                    return Ok(());
                }
                Err(e) => return Err(e),
            };

        // Add Via and X-Forwarded-For before callbacks see the request,
        // so both the upstream and the TUI observe the headers as sent.
        apply_request_proxy_headers(
            &mut parsed_request.headers,
            &parsed_request.version,
            client_addr.ip(),
        );

        let request = Request::from_parsed(
            &parsed_request,
            request_id,
            client_addr,
            request_body.clone(),
            true, // This is an intercepted HTTPS request
        );

        if let Some(callback) = &state.callbacks.on_request {
            callback.invoke(request.clone()).await;
        }

        // Forward request to upstream and capture response. Strip
        // hop-by-hop headers (RFC 7230 §6.1) before forwarding.
        let header_bytes = serialize_request_head(&parsed_request);
        let outgoing = Outgoing {
            request: &parsed_request,
            head: &header_bytes,
            body: &request_body,
            id: request_id,
        };
        let (conn, result) =
            send_with_retry(client, connector, upstream.take(), &outgoing, config).await;

        let _ = buf.split_to(parsed_request.header_len);

        let exchange = match &result {
            Ok(outcome) => Exchange {
                request: request.clone(),
                response: Some(outcome.response.clone()),
                error: None,
            },
            Err(failure) => Exchange {
                request: request.clone(),
                response: None,
                error: Some(failure.error.to_string()),
            },
        };

        state.record(exchange).await;

        match result {
            Ok(outcome) => {
                // Keep the connection only when the exchange left it in a
                // reusable state; otherwise drop it here and let the next
                // request reconnect.
                if outcome.keep_alive {
                    upstream = conn;
                } else {
                    debug!(
                        id = request_id,
                        host, port, "upstream connection spent; next request will reconnect"
                    );
                }

                if let Some(callback) = &state.callbacks.on_response {
                    callback.invoke(outcome.response).await;
                }

                // A close-delimited or truncated body ends only when the
                // client sees the session close.
                if !outcome.client_keeps_alive {
                    debug!(id = request_id, host, port, "closing tunnel after response");
                    let _ = client.shutdown().await;
                    return Ok(());
                }
            }
            Err(failure) if !failure.responded => {
                // Nothing of a response reached the client, so it can
                // still be told what went wrong.
                warn!(id = request_id, error = %failure.error, "upstream failed; answering for it");
                send_gateway_error(client, &failure.error, config.write_timeout).await;
                return Ok(());
            }
            Err(failure) => {
                // A mid-exchange failure can leave the upstream TLS stream
                // desynced — the request may have been partially written or
                // the response only partially read. Reusing it for the next
                // request would map responses to the wrong request, and the
                // client has already seen part of this response, so tear the
                // tunnel down instead of looping.
                warn!(id = request_id, error = %failure.error, "intercepted request failed; closing tunnel");
                return Err(failure.error);
            }
        }
    }
}

/// A request ready to go upstream.
struct Outgoing<'a> {
    request: &'a ParsedRequest,
    head: &'a [u8],
    body: &'a Bytes,
    id: u64,
}

/// Send `outgoing` on the held connection, or a fresh one if there is
/// none.
///
/// A reused keep-alive connection can be closed by the origin at any
/// moment, and we only find out by trying to use it. When the attempt
/// failed before a single response byte came back, nothing has reached
/// the client and the origin provably never acted on the request, so it
/// is replayed once on a fresh connection.
///
/// Returns the connection used, if one was opened, alongside the result.
async fn send_with_retry<C, K>(
    client: &mut C,
    connector: &K,
    held: Option<Upstream<K::Stream>>,
    outgoing: &Outgoing<'_>,
    config: &Config,
) -> (
    Option<Upstream<K::Stream>>,
    Result<ForwardOutcome, ForwardFailure>,
)
where
    C: AsyncWrite + Unpin + Send,
    K: UpstreamConnector,
{
    let mut conn = match held {
        Some(conn) => conn,
        None => match connector.connect().await {
            Ok(conn) => conn,
            Err(error) => return (None, Err(ForwardFailure::before_response(error))),
        },
    };
    let reused = !conn.is_unused();

    let result = forward_intercepted_request(
        client,
        &mut conn,
        outgoing.request,
        outgoing.head,
        outgoing.body,
        outgoing.id,
        config,
    )
    .await;
    let retry = matches!(&result, Err(failure) if reused && failure.retryable);
    if !retry {
        return (Some(conn), result);
    }

    if let Err(failure) = &result {
        debug!(
            id = outgoing.id,
            error = %failure.error,
            "upstream closed the tunnel's connection; retrying on a fresh one"
        );
    }
    let mut fresh = match connector.connect().await {
        Ok(fresh) => fresh,
        Err(error) => return (None, Err(ForwardFailure::before_response(error))),
    };
    let result = forward_intercepted_request(
        client,
        &mut fresh,
        outgoing.request,
        outgoing.head,
        outgoing.body,
        outgoing.id,
        config,
    )
    .await;
    (Some(fresh), result)
}

/// Handle a WebSocket upgrade inside an intercepted TLS session.
///
/// Forwards the upgrade request (verbatim, preserving `Connection` and
/// `Upgrade`) to upstream, reads the response headers, and on a 101 hands
/// off to a bidirectional byte copy between the two TLS streams. Non-101
/// responses are forwarded to the client and the session ends.
/// Serve a magic-domain request over an intercepted TLS session.
///
/// Reads one HTTP request from the TLS stream, extracts the path, and
/// hands off to [`magic::handle_magic_request`]. The response always sets
/// `Connection: close`, so a single request/response cycle ends the
/// session — matching the plain-HTTP magic-domain path.
async fn serve_intercepted_magic_domain<C>(
    client_tls: &mut C,
    state: &Arc<ProxyState>,
) -> crate::Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let config = &state.config;
    let mut buf = BytesMut::with_capacity(4096);

    let parsed_request = match read_request_head(
        client_tls,
        &mut buf,
        config.read_timeout,
        config.max_header_size,
    )
    .await?
    {
        HeadRead::Parsed(request) => request,
        HeadRead::TooLarge { buffered } => {
            warn!(
                buffered,
                max = config.max_header_size,
                "magic domain request headers exceeded the maximum size; closing"
            );
            client_tls.write_all(HEADERS_TOO_LARGE_RESPONSE).await?;
            return Ok(());
        }
        HeadRead::Closed | HeadRead::TimedOut { .. } | HeadRead::Malformed { .. } => return Ok(()),
    };

    let path = if let Ok(url) = Url::parse(&parsed_request.target) {
        url.path().to_owned()
    } else {
        parsed_request
            .target
            .split('?')
            .next()
            .unwrap_or(&parsed_request.target)
            .to_owned()
    };
    let path = if path.is_empty() { "/" } else { &path };

    magic::handle_magic_request(client_tls, path, state).await
}

/// Relay an upgrade response that was not a 101, in full.
///
/// A rejected upgrade is an ordinary HTTP response and may carry a body.
/// Forwarding only the headers that happened to arrive alongside the
/// status line leaves the client waiting on a `Content-Length` that will
/// never be satisfied — and then the connection closes under it.
///
/// The head goes out verbatim rather than through the hop-by-hop
/// serializer: the point of this path is to show the client exactly what
/// the origin said about its upgrade attempt.
async fn forward_rejected_upgrade<C, U>(
    client: &mut C,
    upstream: &mut U,
    response_buf: &BytesMut,
    parsed_response: &ParsedResponse,
    config: &Config,
) -> crate::Result<Bytes>
where
    C: AsyncWrite + Unpin + Send,
    U: AsyncRead + Unpin,
{
    let head_len = parsed_response.header_len;
    write_all_within(
        client,
        &response_buf[..head_len],
        config.write_timeout,
        "write rejected upgrade response head",
    )
    .await?;

    // An upgrade request is always a GET, so only the bodyless statuses
    // (1xx, 204, 304) carry nothing.
    if response_has_no_body("GET", parsed_response.status) {
        return Ok(Bytes::new());
    }

    let body = forward_body(
        upstream,
        &mut WriteDeadline::new(
            client,
            config.write_timeout,
            "write rejected upgrade response body",
        ),
        BodyFraming::for_response(parsed_response),
        &response_buf[head_len..],
        config.read_timeout,
        "read rejected upgrade response body",
        config.max_body_size,
    )
    .await?;

    Ok(body.into_bytes())
}

/// Read a request head from the intercepted TLS stream, handling the
/// ways it can fail.
///
/// Returns `Ok(None)` when the tunnel should be torn down — a clean
/// client close, an idle timeout, a slowloris drip, or an oversized
/// header block, each logged for what it is. Oversized heads get an RFC
/// 6585 431 before the close.
async fn read_intercepted_request_headers<C>(
    client: &mut C,
    buf: &mut BytesMut,
    config: &Config,
    host: &str,
    port: u16,
) -> crate::Result<Option<ParsedRequest>>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    match read_request_head(client, buf, config.read_timeout, config.max_header_size).await? {
        HeadRead::Parsed(request) => Ok(Some(request)),
        HeadRead::Closed => {
            debug!(host, port, "client closed connection");
            Ok(None)
        }
        HeadRead::TimedOut { buffered: 0 } => {
            debug!(host, port, "client read timeout, closing");
            Ok(None)
        }
        HeadRead::TimedOut { buffered } => {
            warn!(
                host,
                port, buffered, "request header read timed out (possible slowloris); closing"
            );
            Ok(None)
        }
        HeadRead::TooLarge { buffered } => {
            warn!(
                host,
                port,
                buffered,
                max = config.max_header_size,
                "request headers exceeded the maximum size; closing"
            );
            client.write_all(HEADERS_TOO_LARGE_RESPONSE).await?;
            Ok(None)
        }
        HeadRead::Malformed { reason } => {
            warn!(host, port, reason, "malformed request head; closing");
            send_proxy_error(client, 400, "Bad Request", &reason, config.write_timeout).await;
            Ok(None)
        }
    }
}

/// Outcome of a successful forward over an intercepted tunnel.
struct ForwardOutcome {
    response: Response,

    /// Whether the upstream connection can carry another request.
    keep_alive: bool,

    /// Whether the client session can carry another request.
    client_keeps_alive: bool,
}

/// A failed forward over an intercepted tunnel.
struct ForwardFailure {
    error: crate::Error,

    /// Whether any of the response reached the client. If not, the
    /// proxy can still answer with a `502` or `504`.
    responded: bool,

    /// Whether the attempt can be replayed on a fresh connection: true
    /// only while the origin has produced no response byte, so nothing
    /// has been written to the client and the request provably had no
    /// effect.
    retryable: bool,
}

impl ForwardFailure {
    /// A failure that MUST NOT be replayed — the origin has already acted
    /// on the request, or the client has already seen part of the
    /// response.
    fn fatal(error: impl Into<crate::Error>) -> Self {
        Self {
            error: error.into(),
            responded: true,
            retryable: false,
        }
    }

    /// A failure before the origin produced a usable response, with
    /// nothing sent to the client yet.
    fn before_response(error: impl Into<crate::Error>) -> Self {
        Self {
            error: error.into(),
            responded: false,
            retryable: false,
        }
    }
}

/// Forward an HTTP request over an intercepted TLS connection.
///
/// The connection's pending setup cost (upstream TCP connect + TLS
/// handshake) is consumed here, so the first request on each connection
/// reports real `connect_ms`/`ssl_ms` values and later ones report
/// `connection_reused: true`.
async fn forward_intercepted_request<C, S>(
    client: &mut C,
    upstream: &mut Upstream<S>,
    parsed_request: &ParsedRequest,
    header_bytes: &[u8],
    request_body: &Bytes,
    request_id: u64,
    config: &Config,
) -> Result<ForwardOutcome, ForwardFailure>
where
    C: AsyncWrite + Unpin + Send,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let start = Instant::now();
    let setup_timings = upstream.pending_setup.take();

    // Send the request and read the response head. A failure here is
    // retryable as long as the origin sent nothing back.
    let RequestSent {
        mut response_buf,
        mut parsed_response,
        sent,
        first_byte,
    } = write_request_and_parse_head(
        &mut upstream.stream,
        header_bytes,
        request_body,
        config.read_timeout,
        config.write_timeout,
        config.max_header_size,
        "read HTTPS response headers",
    )
    .await
    .map_err(|failure| ForwardFailure {
        error: failure.error,
        responded: false,
        retryable: !failure.saw_response_bytes,
    })?;

    if let Some(reason) = parsed_response.framing_error() {
        return Err(ForwardFailure::before_response(
            crate::Error::InvalidResponse(reason.to_owned()),
        ));
    }

    let body_start = parsed_response.header_len;
    let no_body_status = response_has_no_body(&parsed_request.method, parsed_response.status);

    // Read the headers' verdict on keep-alive before we rewrite them.
    // The body relay gets the final say below: an unframed or truncated
    // body means the connection is gone whatever the headers claimed.
    let headers_allow_reuse = can_reuse_connection(&parsed_response, parsed_request);

    // Append Mira to the Via header on the way back out.
    apply_response_proxy_headers(&mut parsed_response.headers, &parsed_response.version);

    // Send headers to client, stripping hop-by-hop headers
    // (RFC 7230 §6.1) so per-connection metadata does not leak.
    let response_head = serialize_response_head(&parsed_response);
    write_all_within(
        client,
        &response_head,
        config.write_timeout,
        "write HTTPS response head",
    )
    .await
    .map_err(ForwardFailure::fatal)?;

    // Read and forward body via the shared helper (same state machine as
    // the plain-HTTP path — any framing fix lands in both callers).
    let body = if no_body_status {
        ForwardedBody::none()
    } else {
        let framing = BodyFraming::for_response(&parsed_response);
        let initial = if response_buf.len() > body_start {
            response_buf.split_off(body_start)
        } else {
            BytesMut::new()
        };
        forward_body(
            &mut upstream.stream,
            &mut WriteDeadline::new(client, config.write_timeout, "write HTTPS response body"),
            framing,
            &initial,
            config.read_timeout,
            "read HTTPS response body",
            config.max_body_size,
        )
        .await
        .map_err(ForwardFailure::fatal)?
    };

    if !body.is_complete() {
        warn!(
            id = request_id,
            "upstream closed mid-body; response relayed truncated"
        );
    }
    let keep_alive = headers_allow_reuse && body.leaves_connection_usable();
    let client_continues = body.leaves_connection_usable() && client_keeps_alive(parsed_request);

    let finished = Instant::now();

    // The first request on a connection picks up its one-time TCP connect
    // + TLS handshake costs; follow-up requests genuinely reuse it and
    // report those as N/A.
    let (connect_ms, ssl_ms, connection_reused) = match setup_timings {
        Some(t) => (Some(t.connect_ms), Some(t.ssl_ms), false),
        None => (None, None, true),
    };
    let timings = Timings {
        blocked_ms: Some(0.0),
        connect_ms,
        send_ms: (sent - start).as_secs_f64() * 1000.0,
        wait_ms: (first_byte - sent).as_secs_f64() * 1000.0,
        receive_ms: (finished - first_byte).as_secs_f64() * 1000.0,
        ssl_ms,
        connection_reused,
    };

    // `Duration::as_millis` returns u128; saturate on the extraordinarily
    // unlikely overflow rather than wrapping silently.
    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    parsed_response = parsed_response.with_body(body.into_bytes());

    info!(
        id = request_id,
        status = parsed_response.status,
        duration_ms,
        body_size = parsed_response.body.len(),
        reused = connection_reused,
        "HTTPS response"
    );

    let response = Response::from_parsed_with_timings(
        &parsed_response,
        request_id,
        duration_ms,
        Some(timings),
        upstream.remote_addr,
    );

    Ok(ForwardOutcome {
        response,
        keep_alive,
        client_keeps_alive: client_continues,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::pool::ConnectionPool;
    use crate::tls::CertificateAuthority;
    use crate::tls::TlsConfig;
    use parking_lot::Mutex;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::io::DuplexStream;
    use tokio::io::duplex;
    use tokio::net::TcpListener;

    /// An [`UpstreamConnector`] that hands out pre-built in-memory
    /// streams, so the request loop's reconnect behavior can be driven
    /// without a real origin.
    struct ScriptedConnector {
        connections: Mutex<VecDeque<DuplexStream>>,
        connects: AtomicUsize,
    }

    impl ScriptedConnector {
        fn new(connections: Vec<DuplexStream>) -> Self {
            Self {
                connections: Mutex::new(connections.into()),
                connects: AtomicUsize::new(0),
            }
        }

        /// How many times the tunnel asked for a new connection.
        fn connects(&self) -> usize {
            self.connects.load(Ordering::SeqCst)
        }
    }

    impl UpstreamConnector for ScriptedConnector {
        type Stream = DuplexStream;

        fn connect(&self) -> impl Future<Output = crate::Result<Upstream<Self::Stream>>> + Send {
            let next = self.connections.lock().pop_front();
            self.connects.fetch_add(1, Ordering::SeqCst);
            async move {
                let stream = next.ok_or_else(|| {
                    crate::Error::InvalidResponse("no scripted connection left".into())
                })?;
                Ok(Upstream {
                    stream,
                    remote_addr: None,
                    protocol: AlpnProtocol::Http11,
                    pending_setup: Some(SetupTimings {
                        connect_ms: 0.0,
                        ssl_ms: 0.0,
                    }),
                })
            }
        }
    }

    /// An initial tunnel connection over an in-memory stream.
    fn scripted_upstream(stream: DuplexStream) -> Upstream<DuplexStream> {
        Upstream {
            stream,
            remote_addr: None,
            protocol: AlpnProtocol::Http11,
            pending_setup: Some(SetupTimings {
                connect_ms: 0.0,
                ssl_ms: 0.0,
            }),
        }
    }

    /// Read from `stream` until everything read so far contains `needle`.
    async fn read_until_contains<R>(stream: &mut R, needle: &str) -> String
    where
        R: AsyncRead + Unpin,
    {
        let mut seen = Vec::new();
        let mut scratch = [0u8; 1024];
        loop {
            let n = timeout(Duration::from_secs(2), stream.read(&mut scratch))
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

    fn proxy_state_with_tls() -> Arc<ProxyState> {
        let ca = CertificateAuthority::generate().expect("generate CA");
        let mut config = Config::default();
        config.https.intercept = true;
        Arc::new(ProxyState {
            config,
            request_counter: AtomicU64::new(1),
            callbacks: Default::default(),
            connection_pool: ConnectionPool::new(),
            session: None,
            tls_config: Some(TlsConfig::new(ca)),
        })
    }

    #[tokio::test]
    async fn test_serve_intercepted_magic_domain_serves_ca_pem() {
        let (mut client_test, mut client_handler) = duplex(8192);
        let state = proxy_state_with_tls();
        let expected_ca_pem = state.tls_config.as_ref().unwrap().ca().cert_pem();

        let handler = tokio::spawn(async move {
            serve_intercepted_magic_domain(&mut client_handler, &state).await
        });

        client_test
            .write_all(b"GET /ca.pem HTTP/1.1\r\nHost: mira.proxy\r\n\r\n")
            .await
            .expect("write req");

        // Read everything the handler produces until EOF.
        let mut response = Vec::new();
        client_test
            .read_to_end(&mut response)
            .await
            .expect("read resp");

        let text = std::str::from_utf8(&response).expect("valid utf8");
        assert!(
            text.starts_with("HTTP/1.1 200 OK\r\n"),
            "expected 200, got: {}",
            text.lines().next().unwrap_or("")
        );
        assert!(text.contains("Content-Type: application/x-pem-file"));
        // The CA PEM body should follow the header/body separator verbatim.
        let body_start = text.find("\r\n\r\n").expect("has header terminator") + 4;
        assert_eq!(
            &text[body_start..],
            expected_ca_pem,
            "body should be the CA PEM"
        );

        let result = handler.await.expect("joined");
        assert!(result.is_ok(), "handler should exit cleanly: {result:?}");
    }

    #[tokio::test]
    async fn test_serve_intercepted_magic_domain_index_without_path_defaults_to_root() {
        let (mut client_test, mut client_handler) = duplex(8192);
        let state = proxy_state_with_tls();

        let handler = tokio::spawn(async move {
            serve_intercepted_magic_domain(&mut client_handler, &state).await
        });

        client_test
            .write_all(b"GET / HTTP/1.1\r\nHost: mira.proxy\r\n\r\n")
            .await
            .expect("write req");

        let mut response = Vec::new();
        client_test
            .read_to_end(&mut response)
            .await
            .expect("read resp");

        let text = std::str::from_utf8(&response).expect("valid utf8");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: text/html"));
        // Index page advertises the CA download.
        assert!(text.contains("Download CA Certificate"));

        handler.await.expect("joined").expect("ok");
    }

    /// Drive [`process_intercepted_requests`] over in-memory streams.
    fn spawn_tunnel(
        client: DuplexStream,
        connector: Arc<ScriptedConnector>,
        initial: Upstream<DuplexStream>,
        state: Arc<ProxyState>,
    ) -> tokio::task::JoinHandle<crate::Result<()>> {
        tokio::spawn(async move {
            let mut client = client;
            let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");
            process_intercepted_requests(
                &mut client,
                connector.as_ref(),
                Some(initial),
                "example.com",
                443,
                client_addr,
                &state,
            )
            .await
        })
    }

    const GET_REQUEST: &[u8] = b"GET /x HTTP/1.1\r\nHost: example.com\r\n\r\n";

    #[tokio::test]
    async fn test_oversized_intercepted_request_headers_get_a_431() {
        let (client_peer, mut client) = duplex(4096);
        let config = Config::default().with_max_header_size(1024);

        let reader = tokio::spawn(async move {
            let mut buf = BytesMut::new();
            read_intercepted_request_headers(&mut client, &mut buf, &config, "example.com", 443)
                .await
        });

        let (mut peer_read, mut peer_write) = tokio::io::split(client_peer);
        peer_write
            .write_all(b"GET /x HTTP/1.1\r\n")
            .await
            .expect("write request line");
        // Headers that never terminate, sent from a task so we can read
        // the rejection while the flood is still blocked on the socket.
        let flood = tokio::spawn(async move {
            let line = format!("X-Pad: {}\r\n", "a".repeat(512));
            while peer_write.write_all(line.as_bytes()).await.is_ok() {}
        });

        let seen = read_until_contains(&mut peer_read, "\r\n\r\n").await;
        assert!(
            seen.starts_with("HTTP/1.1 431 Request Header Fields Too Large\r\n"),
            "got: {seen}"
        );

        let parsed = reader
            .await
            .expect("joined")
            .expect("an oversized head is handled, not an error");
        assert!(parsed.is_none(), "the tunnel must be torn down");

        flood.abort();
    }

    #[tokio::test]
    async fn test_intercepted_tunnel_reuses_connection_for_keep_alive_responses() {
        let (mut client, client_handler) = duplex(4096);
        let (mut origin, upstream) = duplex(4096);
        // No spare connections: needing one would fail the exchange.
        let connector = Arc::new(ScriptedConnector::new(Vec::new()));

        let tunnel = spawn_tunnel(
            client_handler,
            Arc::clone(&connector),
            scripted_upstream(upstream),
            proxy_state_with_tls(),
        );

        for body in ["first", "secnd"] {
            client.write_all(GET_REQUEST).await.expect("write request");
            let seen = read_until_contains(&mut origin, "\r\n\r\n").await;
            assert!(seen.starts_with("GET /x HTTP/1.1\r\n"), "got: {seen}");
            origin
                .write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n{body}").as_bytes())
                .await
                .expect("respond");
            let response = read_until_contains(&mut client, body).await;
            assert!(
                response.starts_with("HTTP/1.1 200 OK\r\n"),
                "got: {response}"
            );
        }

        assert_eq!(
            connector.connects(),
            0,
            "keep-alive responses must not cost a reconnect"
        );

        drop(client);
        tunnel.await.expect("joined").expect("tunnel ends cleanly");
    }

    #[tokio::test]
    async fn test_intercepted_tunnel_answers_502_when_the_origin_is_unreachable() {
        let (mut client, mut client_handler) = duplex(4096);
        // No connections to hand out: every connect fails.
        let connector = ScriptedConnector::new(Vec::new());
        let session = crate::Session::new();
        let mut state = Arc::try_unwrap(proxy_state_with_tls()).expect("sole owner");
        state.session = Some(session.clone());
        let state = Arc::new(state);

        let tunnel = tokio::spawn(async move {
            let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");
            process_intercepted_requests(
                &mut client_handler,
                &connector,
                None,
                "example.com",
                443,
                client_addr,
                &state,
            )
            .await
        });

        client.write_all(GET_REQUEST).await.expect("write request");
        let mut response = Vec::new();
        timeout(Duration::from_secs(2), client.read_to_end(&mut response))
            .await
            .expect("the tunnel answers and closes")
            .expect("read");
        let text = String::from_utf8_lossy(&response);
        assert!(
            text.starts_with("HTTP/1.1 502 Bad Gateway\r\n"),
            "got: {text}"
        );
        assert!(
            text.contains("no scripted connection left"),
            "the reason is given: {text}"
        );

        tunnel.await.expect("joined").expect("answered, not failed");
        let captured = session.exchanges();
        assert_eq!(captured.len(), 1, "the failed exchange is still recorded");
        assert!(captured[0].exchange.error.is_some());
    }

    #[tokio::test]
    async fn test_intercepted_tunnel_rejects_ambiguous_framing() {
        let (mut client, client_handler) = duplex(4096);
        let (_origin, upstream) = duplex(4096);
        let tunnel = spawn_tunnel(
            client_handler,
            Arc::new(ScriptedConnector::new(Vec::new())),
            scripted_upstream(upstream),
            proxy_state_with_tls(),
        );

        client
            .write_all(
                b"POST /x HTTP/1.1\r\nHost: example.com\r\n\
                  Transfer-Encoding: chunked, identity\r\n\r\n",
            )
            .await
            .expect("write request");
        let response = read_until_contains(&mut client, "\r\n\r\n").await;
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "got: {response}"
        );

        tunnel.await.expect("joined").expect("rejected, not failed");
    }

    #[tokio::test]
    async fn test_intercepted_tunnel_reconnects_after_upstream_says_close() {
        let (mut client, client_handler) = duplex(4096);
        let (mut first_origin, first_upstream) = duplex(4096);
        let (mut second_origin, second_upstream) = duplex(4096);
        let connector = Arc::new(ScriptedConnector::new(vec![second_upstream]));

        let tunnel = spawn_tunnel(
            client_handler,
            Arc::clone(&connector),
            scripted_upstream(first_upstream),
            proxy_state_with_tls(),
        );

        // First exchange: the origin answers and announces it is done.
        client.write_all(GET_REQUEST).await.expect("write request");
        read_until_contains(&mut first_origin, "\r\n\r\n").await;
        first_origin
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nfirst")
            .await
            .expect("respond");
        let response = read_until_contains(&mut client, "first").await;
        assert!(
            !response.to_lowercase().contains("connection: close"),
            "the origin's hop-by-hop close must not leak to the client: {response}"
        );
        // The origin hangs up, as it said it would.
        drop(first_origin);

        // The client's tunnel is still open, so the next request has to
        // reach the origin over a brand new upstream connection.
        client
            .write_all(GET_REQUEST)
            .await
            .expect("write request 2");
        let seen = read_until_contains(&mut second_origin, "\r\n\r\n").await;
        assert!(seen.starts_with("GET /x HTTP/1.1\r\n"), "got: {seen}");
        second_origin
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nsecnd")
            .await
            .expect("respond");
        let response = read_until_contains(&mut client, "secnd").await;
        assert!(
            response.starts_with("HTTP/1.1 200 OK\r\n"),
            "got: {response}"
        );

        assert_eq!(connector.connects(), 1, "exactly one reconnect expected");

        drop(client);
        tunnel.await.expect("joined").expect("tunnel ends cleanly");
    }

    /// Run one exchange over an in-memory upstream that answers with
    /// `response` and then hangs up.
    async fn forward_once(response: &'static [u8]) -> ForwardOutcome {
        let (mut origin, upstream_stream) = duplex(4096);
        let (_client_peer, mut client) = duplex(4096);

        let origin_task = tokio::spawn(async move {
            read_until_contains(&mut origin, "\r\n\r\n").await;
            origin.write_all(response).await.expect("respond");
            // Hanging up is what makes a short Content-Length short.
        });

        let mut upstream = scripted_upstream(upstream_stream);
        let parsed = ParsedRequest::parse(&BytesMut::from(
            &b"GET /x HTTP/1.1\r\nHost: example.com\r\n\r\n"[..],
        ))
        .expect("parses")
        .expect("complete");
        let head = serialize_request_head(&parsed);

        let outcome = forward_intercepted_request(
            &mut client,
            &mut upstream,
            &parsed,
            &head,
            &Bytes::new(),
            1,
            &Config::default(),
        )
        .await
        .unwrap_or_else(|failure| panic!("forward failed: {}", failure.error));

        origin_task.await.expect("origin joined");
        outcome
    }

    #[tokio::test]
    async fn test_forward_keeps_connection_after_complete_body() {
        let outcome = forward_once(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").await;
        assert_eq!(outcome.response.body, Bytes::from_static(b"hello"));
        assert!(
            outcome.keep_alive,
            "a keep-alive response read in full leaves the connection usable"
        );
        assert!(outcome.client_keeps_alive);
    }

    #[tokio::test]
    async fn test_forward_drops_connection_after_truncated_body() {
        // Ten bytes promised, three delivered, then the origin hangs up.
        // The headers alone claim keep-alive; only the body relay knows
        // the socket is already gone.
        let outcome = forward_once(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc").await;
        assert_eq!(
            outcome.response.body,
            Bytes::from_static(b"abc"),
            "the partial body is still relayed and captured"
        );
        assert!(
            !outcome.keep_alive,
            "a connection closed mid-body must not be reused"
        );
        assert!(
            !outcome.client_keeps_alive,
            "the client must see the close, or it waits for the missing bytes"
        );
    }

    #[tokio::test]
    async fn test_forward_drops_connection_after_unframed_body() {
        // No Content-Length, no chunking: the close delimits the body.
        let outcome = forward_once(b"HTTP/1.1 200 OK\r\n\r\nhello").await;
        assert_eq!(outcome.response.body, Bytes::from_static(b"hello"));
        assert!(
            !outcome.keep_alive,
            "a close-delimited body leaves nothing to reuse"
        );
        assert!(
            !outcome.client_keeps_alive,
            "the close is how the client learns the body ended"
        );
    }

    #[tokio::test]
    async fn test_intercepted_tunnel_retries_request_on_dead_reused_connection() {
        let (mut client, client_handler) = duplex(4096);
        let (mut first_origin, first_upstream) = duplex(4096);
        let (mut second_origin, second_upstream) = duplex(4096);
        let connector = Arc::new(ScriptedConnector::new(vec![second_upstream]));

        let tunnel = spawn_tunnel(
            client_handler,
            Arc::clone(&connector),
            scripted_upstream(first_upstream),
            proxy_state_with_tls(),
        );

        // First exchange leaves the connection reusable.
        client.write_all(GET_REQUEST).await.expect("write request");
        read_until_contains(&mut first_origin, "\r\n\r\n").await;
        first_origin
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nfirst")
            .await
            .expect("respond");
        read_until_contains(&mut client, "first").await;

        // ...and then the origin drops it anyway, as keep-alive servers
        // do on their own idle timeout. The tunnel has no way to know.
        drop(first_origin);

        // The next request fails on the dead connection before the origin
        // can have acted on it, so it is replayed — body and all — on a
        // fresh connection, even though POST is not idempotent.
        client
            .write_all(
                b"POST /submit HTTP/1.1\r\nHost: example.com\r\nContent-Length: 4\r\n\r\nsent",
            )
            .await
            .expect("write request 2");
        let seen = read_until_contains(&mut second_origin, "sent").await;
        assert!(seen.starts_with("POST /submit HTTP/1.1\r\n"), "got: {seen}");
        assert!(
            seen.ends_with("sent"),
            "request body must be replayed: {seen}"
        );
        second_origin
            .write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 5\r\n\r\nsecnd")
            .await
            .expect("respond");
        let response = read_until_contains(&mut client, "secnd").await;
        assert!(
            response.starts_with("HTTP/1.1 201 Created\r\n"),
            "the client sees only the successful attempt: {response}"
        );

        assert_eq!(connector.connects(), 1, "exactly one reconnect expected");

        drop(client);
        tunnel.await.expect("joined").expect("tunnel ends cleanly");
    }

    #[tokio::test]
    async fn test_intercepted_websocket_upgrade_succeeds_and_tunnels() {
        let (mut client_test, mut client_handler) = duplex(4096);
        let (mut upstream_test, mut upstream_handler) = duplex(4096);

        let request = b"GET /ws HTTP/1.1\r\n\
                        Host: example.com\r\n\
                        Connection: Upgrade\r\n\
                        Upgrade: websocket\r\n\
                        Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                        Sec-WebSocket-Version: 13\r\n\
                        \r\n";
        let response = b"HTTP/1.1 101 Switching Protocols\r\n\
                         Upgrade: websocket\r\n\
                         Connection: Upgrade\r\n\
                         Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\
                         \r\n";

        let config = Config::default();

        let handler = tokio::spawn(async move {
            handle_intercepted_websocket_upgrade(
                &mut client_handler,
                &mut upstream_handler,
                request,
                42,
                &config,
            )
            .await
        });

        // Handler forwards the upgrade request to upstream verbatim.
        let mut seen = vec![0u8; request.len()];
        upstream_test.read_exact(&mut seen).await.expect("read req");
        assert_eq!(seen, request);

        // Upstream responds 101.
        upstream_test.write_all(response).await.expect("write 101");

        // Handler forwards the 101 to the client.
        let mut client_seen = vec![0u8; response.len()];
        client_test
            .read_exact(&mut client_seen)
            .await
            .expect("read 101");
        assert_eq!(client_seen, response);

        // Now prove bidirectional flow: client bytes reach upstream and
        // upstream bytes reach the client — no HTTP parsing in the way.
        client_test
            .write_all(b"client->server frame")
            .await
            .expect("write c->s");
        let mut c2s = vec![0u8; 20];
        upstream_test.read_exact(&mut c2s).await.expect("read c->s");
        assert_eq!(&c2s, b"client->server frame");

        upstream_test
            .write_all(b"server->client frame")
            .await
            .expect("write s->c");
        let mut s2c = vec![0u8; 20];
        client_test.read_exact(&mut s2c).await.expect("read s->c");
        assert_eq!(&s2c, b"server->client frame");

        // Close both sides to end the bidirectional copy.
        drop(client_test);
        drop(upstream_test);

        let result = handler.await.expect("handler joined");
        assert!(result.is_ok(), "handler should exit cleanly: {result:?}");
    }

    /// The intercepted upgrade path as the request loop runs it: the
    /// handshake, then a relay once the origin switches protocols.
    async fn handle_intercepted_websocket_upgrade<C, U>(
        client: &mut C,
        upstream: &mut U,
        request: &[u8],
        _request_id: u64,
        config: &Config,
    ) -> crate::Result<()>
    where
        C: AsyncRead + AsyncWrite + Unpin + Send,
        U: AsyncRead + AsyncWrite + Unpin,
    {
        if upgrade_handshake(client, upstream, request, config)
            .await?
            .switched
        {
            relay(client, upstream, "test").await;
        }
        Ok(())
    }

    const UPGRADE_REQUEST: &[u8] = b"GET /ws HTTP/1.1\r\n\
                                     Host: example.com\r\n\
                                     Connection: Upgrade\r\n\
                                     Upgrade: websocket\r\n\
                                     Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                                     Sec-WebSocket-Version: 13\r\n\
                                     \r\n";

    /// A connected TCP pair: the second half is what the proxy writes to.
    async fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind client");
        let addr = listener.local_addr().expect("client addr");
        let dialer = tokio::spawn(async move { TcpStream::connect(addr).await.expect("connect") });
        let (proxy_side, _) = listener.accept().await.expect("accept");
        let client_side = dialer.await.expect("dialer joined");
        (client_side, proxy_side)
    }

    #[tokio::test]
    async fn test_intercepted_websocket_rejection_forwards_the_whole_body() {
        let (mut client_test, mut client_handler) = duplex(4096);
        let (mut upstream_test, mut upstream_handler) = duplex(4096);
        let config = Config::default();

        let handler = tokio::spawn(async move {
            handle_intercepted_websocket_upgrade(
                &mut client_handler,
                &mut upstream_handler,
                UPGRADE_REQUEST,
                7,
                &config,
            )
            .await
        });

        let mut seen = vec![0u8; UPGRADE_REQUEST.len()];
        upstream_test.read_exact(&mut seen).await.expect("read req");

        // Send the head alone and wait for it to reach the client before
        // sending the body. That ordering is the point: the body is not
        // sitting in the buffer the head was parsed from, so it has to be
        // read separately or not at all.
        upstream_test
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 11\r\n\r\n")
            .await
            .expect("write head");
        let head = read_until_contains(&mut client_test, "\r\n\r\n").await;
        assert!(
            head.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "got: {head}"
        );

        upstream_test
            .write_all(b"hello world")
            .await
            .expect("write body");

        let body = read_until_contains(&mut client_test, "hello world").await;
        assert_eq!(
            body, "hello world",
            "the declared body must be relayed after the head"
        );

        handler.await.expect("joined").expect("ok");
    }

    #[tokio::test]
    async fn test_intercepted_websocket_rejection_forwards_a_chunked_body() {
        let (mut client_test, mut client_handler) = duplex(4096);
        let (mut upstream_test, mut upstream_handler) = duplex(4096);
        let config = Config::default();

        let handler = tokio::spawn(async move {
            handle_intercepted_websocket_upgrade(
                &mut client_handler,
                &mut upstream_handler,
                UPGRADE_REQUEST,
                8,
                &config,
            )
            .await
        });

        let mut seen = vec![0u8; UPGRADE_REQUEST.len()];
        upstream_test.read_exact(&mut seen).await.expect("read req");
        upstream_test
            .write_all(b"HTTP/1.1 403 Forbidden\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .expect("write head");
        read_until_contains(&mut client_test, "\r\n\r\n").await;

        upstream_test
            .write_all(b"5\r\nnope!\r\n0\r\n\r\n")
            .await
            .expect("write chunks");

        let body = read_until_contains(&mut client_test, "0\r\n\r\n").await;
        assert_eq!(
            body, "5\r\nnope!\r\n0\r\n\r\n",
            "the chunked body is relayed whole"
        );

        handler.await.expect("joined").expect("ok");
    }

    #[tokio::test]
    async fn test_intercepted_websocket_rejection_does_not_wait_on_a_bodyless_status() {
        // A 304 carries no body whatever its headers say. Waiting for one
        // would hang the client until the read timeout.
        let (mut client_test, mut client_handler) = duplex(4096);
        let (mut upstream_test, mut upstream_handler) = duplex(4096);
        let config = Config::default();

        let handler = tokio::spawn(async move {
            handle_intercepted_websocket_upgrade(
                &mut client_handler,
                &mut upstream_handler,
                UPGRADE_REQUEST,
                9,
                &config,
            )
            .await
        });

        let mut seen = vec![0u8; UPGRADE_REQUEST.len()];
        upstream_test.read_exact(&mut seen).await.expect("read req");
        upstream_test
            .write_all(b"HTTP/1.1 304 Not Modified\r\n\r\n")
            .await
            .expect("write head");
        // Upstream deliberately stays open.

        let relayed = read_until_contains(&mut client_test, "\r\n\r\n").await;
        assert!(relayed.starts_with("HTTP/1.1 304 Not Modified\r\n"));

        let result = timeout(Duration::from_secs(2), handler)
            .await
            .expect("handler should return without waiting for a body");
        result.expect("joined").expect("ok");
    }

    #[tokio::test]
    async fn test_plain_websocket_rejection_forwards_the_whole_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
        let port = listener.local_addr().expect("origin addr").port();
        // The body is held back until the test has seen the head reach
        // the client, so it cannot ride along in the same read.
        let (body_gate, wait_for_gate) = tokio::sync::oneshot::channel::<()>();
        let origin = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut scratch = [0u8; 1024];
            let _ = sock.read(&mut scratch).await.expect("read request");
            sock.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 11\r\n\r\n")
                .await
                .expect("write head");
            wait_for_gate.await.expect("gate");
            sock.write_all(b"hello world").await.expect("write body");
            // Stay open so the body is not delimited by a close.
            let _ = sock.read(&mut scratch).await;
        });

        let (mut client_peer, mut client) = tcp_pair().await;

        let handler = tokio::spawn(async move {
            let config = Config::default();
            open_websocket(&mut client, "127.0.0.1", port, UPGRADE_REQUEST, &config)
                .await
                .map(|(_upstream, upgrade)| upgrade)
        });

        let head = read_until_contains(&mut client_peer, "\r\n\r\n").await;
        assert!(
            head.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "got: {head}"
        );
        body_gate.send(()).expect("origin still waiting");

        let body = read_until_contains(&mut client_peer, "hello world").await;
        assert_eq!(body, "hello world");

        let upgrade = handler.await.expect("joined").expect("handshake ran");
        assert!(!upgrade.switched, "a non-101 reply is not an upgrade");
        assert_eq!(upgrade.response.status, 400);
        assert_eq!(
            upgrade.response.body,
            Bytes::from_static(b"hello world"),
            "the rejection's body is captured too"
        );

        origin.abort();
    }

    #[tokio::test]
    async fn test_intercepted_websocket_upgrade_rejected_forwards_and_returns() {
        let (mut client_test, mut client_handler) = duplex(4096);
        let (mut upstream_test, mut upstream_handler) = duplex(4096);

        let request = b"GET /ws HTTP/1.1\r\n\
                        Host: example.com\r\n\
                        Connection: Upgrade\r\n\
                        Upgrade: websocket\r\n\
                        Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                        Sec-WebSocket-Version: 13\r\n\
                        \r\n";
        let response = b"HTTP/1.1 400 Bad Request\r\n\
                         Content-Length: 0\r\n\
                         \r\n";

        let config = Config::default();

        let handler = tokio::spawn(async move {
            handle_intercepted_websocket_upgrade(
                &mut client_handler,
                &mut upstream_handler,
                request,
                7,
                &config,
            )
            .await
        });

        let mut seen = vec![0u8; request.len()];
        upstream_test.read_exact(&mut seen).await.expect("read req");
        upstream_test
            .write_all(response)
            .await
            .expect("write rejection");

        let mut client_seen = vec![0u8; response.len()];
        client_test
            .read_exact(&mut client_seen)
            .await
            .expect("read rejection");
        assert_eq!(client_seen, response);

        // Handler should return Ok(()) after forwarding the non-101 response
        // — no bidirectional copy, no error escalation.
        let result = handler.await.expect("handler joined");
        assert!(result.is_ok(), "handler should exit cleanly: {result:?}");
    }
}
