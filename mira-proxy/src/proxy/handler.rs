//! Connection handler for the proxy.

use crate::Result;
use crate::config::Config;
use crate::config::is_private_ip;
use crate::http::BodyFraming;
use crate::http::Exchange;
use crate::http::ForwardedBody;
use crate::http::HEADERS_TOO_LARGE_RESPONSE;
use crate::http::HeadRead;
use crate::http::ParsedRequest;
use crate::http::Request;
use crate::http::RequestSent;
use crate::http::Response;
use crate::http::SendFailed;
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
use crate::proxy::magic;
use crate::proxy::server::ProxyState;
use crate::proxy::tunnel;
use bytes::Bytes;
use bytes::BytesMut;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::debug;
use tracing::info;
use tracing::warn;

/// Handle a single client connection.
pub async fn handle_connection(client: TcpStream, client_addr: SocketAddr, state: Arc<ProxyState>) {
    if !state.config.allow_external && !is_private_ip(client_addr.ip()) {
        debug!(
            %client_addr,
            "rejected connection from external IP (use --allow-external to permit)"
        );
        return;
    }

    debug!(%client_addr, "new connection");

    if let Err(e) = process_connection(client, client_addr, &state).await {
        // Don't log connection reset errors as they're common
        let is_reset = matches!(&e, crate::Error::Connection(io)
            if io.kind() == std::io::ErrorKind::ConnectionReset);

        if is_reset {
            debug!(%client_addr, "connection reset by peer");
        } else {
            warn!(%client_addr, error = %e, "connection error");
        }
    }

    debug!(%client_addr, "connection closed");
}

/// Process the client connection, handling requests.
async fn process_connection(
    mut client: TcpStream,
    client_addr: SocketAddr,
    state: &Arc<ProxyState>,
) -> Result<()> {
    let mut buf = BytesMut::with_capacity(8192);

    loop {
        // Read request headers under a total deadline — not a per-read
        // timeout — so a slowloris-style byte-drip attacker can't reset
        // the clock indefinitely. The same deadline doubles as an idle
        // keep-alive cap when no bytes have arrived yet.
        let Some(mut parsed_request) =
            read_request_headers(&mut client, &mut buf, &state.config, client_addr).await?
        else {
            return Ok(());
        };

        // A client addresses a proxy in absolute-form; an origin expects
        // origin-form. Rewrite before anything else reads the target or
        // the Host header, so filtering, routing, the URL the callbacks
        // see, and the bytes sent upstream all agree on the origin.
        parsed_request.rewrite_to_origin_form();

        if let Some(reason) = parsed_request.framing_error() {
            warn!(%client_addr, reason, "rejecting request with ambiguous framing");
            send_proxy_error(
                &mut client,
                400,
                "Bad Request",
                reason,
                state.config.write_timeout,
            )
            .await;
            return Ok(());
        }

        // Check host filter before processing (don't need ID yet)
        if let Some(filter) = &state.config.host_filter {
            let host = parsed_request.host().unwrap_or("");
            if !filter.matches(host) {
                debug!(host, "request blocked by host filter");
                let response =
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                client.write_all(response).await?;
                return Ok(());
            }
        }

        // An intercepted CONNECT gets no ID of its own; the requests inside
        // the tunnel get theirs.
        if parsed_request.is_connect() {
            let (host, port) = match parsed_request.host_port() {
                Ok(authority) => authority,
                Err(e) => {
                    send_gateway_error(&mut client, &e, state.config.write_timeout).await;
                    return Err(e);
                }
            };

            // Anything after the CONNECT head is already tunnel traffic,
            // such as the start of a TLS ClientHello, and MUST go through.
            let early_bytes = buf.split_off(parsed_request.header_len).freeze();
            buf.clear();

            let should_intercept = state.tls_config.is_some()
                && state.config.https.intercept
                && !state.config.https.should_passthrough(&host);

            if should_intercept {
                debug!(host, port, "intercepting HTTPS");
                // Moved rather than borrowed: an HTTP/2 session hands
                // the socket to hyper, which owns it for the
                // connection's lifetime.
                return tunnel::establish_intercepted(
                    client,
                    early_bytes,
                    &host,
                    port,
                    client_addr,
                    state,
                )
                .await;
            }

            // A passthrough tunnel's requests are never seen, so the CONNECT
            // itself is the exchange.
            let request_id = state.next_request_id();

            info!(
                id = request_id,
                method = %parsed_request.method,
                target = %parsed_request.target,
                "request"
            );

            let request = Request::from_parsed(
                &parsed_request,
                request_id,
                client_addr,
                Bytes::new(),
                false,
            );

            if let Some(callback) = &state.callbacks.on_request {
                callback.invoke(request.clone()).await;
            }

            debug!(id = request_id, host, port, "tunneling HTTPS (passthrough)");
            let start = Instant::now();
            let opened = tunnel::open_passthrough(&mut client, &host, port, &state.config).await;
            state
                .record(match &opened {
                    Ok(upstream) => Exchange {
                        request,
                        response: Some(tunnel::tunnel_response(request_id, start, upstream)),
                        error: None,
                    },
                    Err(e) => Exchange {
                        request,
                        response: None,
                        error: Some(e.to_string()),
                    },
                })
                .await;

            let mut upstream = opened?;
            if !early_bytes.is_empty() {
                write_all_within(
                    &mut upstream,
                    &early_bytes,
                    state.config.write_timeout,
                    "write tunnel bytes",
                )
                .await?;
            }
            tunnel::relay(&mut client, &mut upstream, "passthrough").await;
            return Ok(());
        }

        let request_id = state.next_request_id();

        info!(
            id = request_id,
            method = %parsed_request.method,
            host = parsed_request.host().unwrap_or("-"),
            target = %parsed_request.target,
            "request"
        );

        // Check for magic domain (mira.proxy) - serves CA cert and info page
        if let Some(host) = parsed_request.host()
            && magic::is_magic_domain(host)
        {
            debug!(id = request_id, "handling magic domain request");
            // The target is origin-form by now (absolute-form was
            // rewritten above), so the path is everything up to the
            // query string.
            let path = parsed_request
                .target
                .split('?')
                .next()
                .unwrap_or(&parsed_request.target);
            let path = if path.is_empty() { "/" } else { path };
            magic::handle_magic_request(&mut client, path, state).await?;
            return Ok(());
        }

        if parsed_request.is_websocket_upgrade() {
            let (host, port) = match parsed_request.host_port() {
                Ok(authority) => authority,
                Err(e) => {
                    send_gateway_error(&mut client, &e, state.config.write_timeout).await;
                    return Err(e);
                }
            };

            let request = Request::from_parsed(
                &parsed_request,
                request_id,
                client_addr,
                Bytes::new(),
                false,
            );

            if let Some(callback) = &state.callbacks.on_request {
                callback.invoke(request.clone()).await;
            }

            info!(id = request_id, host, port, "WebSocket upgrade");

            // Forward the client's header bytes as sent, with only the
            // request line corrected to origin-form.
            let header_bytes =
                serialize_upgrade_request_head(&buf[..parsed_request.header_len], &parsed_request);

            buf.clear();

            // Establish WebSocket tunnel - this takes over the connection
            let start = Instant::now();
            let opened =
                tunnel::open_websocket(&mut client, &host, port, &header_bytes, &state.config)
                    .await;
            let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            state
                .record(match &opened {
                    Ok((upstream, upgrade)) => Exchange {
                        request,
                        response: Some(Response::from_parsed_with_timings(
                            &upgrade.response,
                            request_id,
                            duration_ms,
                            None,
                            upstream.peer_addr().ok(),
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

            let (mut upstream, upgrade) = match opened {
                Ok(opened) => opened,
                Err(e) => {
                    warn!(id = request_id, error = %e, "WebSocket upgrade failed");
                    return Err(e);
                }
            };
            if !upgrade.switched {
                // The origin answered as plain HTTP; the client was told
                // in full and expects nothing more on this connection.
                warn!(
                    id = request_id,
                    status = upgrade.response.status,
                    "WebSocket upgrade not accepted by upstream"
                );
                return Ok(());
            }
            info!(id = request_id, "WebSocket tunnel established");
            tunnel::relay(&mut client, &mut upstream, "WebSocket").await;
            return Ok(());
        }

        send_continue_if_expected(&mut client, &parsed_request, state.config.max_body_size).await?;
        let request_body = match read_body(
            &mut client,
            &mut buf,
            &parsed_request,
            state.config.max_body_size,
        )
        .await
        {
            Ok(body) => body,
            Err(crate::Error::BodyTooLarge { size, max }) => {
                warn!(
                    id = request_id,
                    size, max, "request body too large; closing"
                );
                let detail = format!("request body exceeds the proxy's {max}-byte limit");
                send_proxy_error(
                    &mut client,
                    413,
                    "Content Too Large",
                    &detail,
                    state.config.write_timeout,
                )
                .await;
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        // Add Via and X-Forwarded-For before callbacks see the request, so
        // both the upstream and the TUI observe the headers as actually
        // sent onward.
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
            false,
        );

        if let Some(callback) = &state.callbacks.on_request {
            callback.invoke(request.clone()).await;
        }

        // Built from the parsed request with hop-by-hop headers stripped
        // (RFC 7230 §6.1), rather than forwarding the client's bytes raw.
        let header_bytes = serialize_request_head(&parsed_request);
        let result = forward_request_with_capture(
            &mut client,
            &parsed_request,
            &header_bytes,
            &request_body,
            request_id,
            state,
        )
        .await;

        // Remove processed request from buffer (headers only, body already consumed)
        let _ = buf.split_to(parsed_request.header_len);

        let exchange = match &result {
            Ok(forwarded) => Exchange {
                request: request.clone(),
                response: Some(forwarded.response.clone()),
                error: None,
            },
            Err(failure) => Exchange {
                request: request.clone(),
                response: None,
                error: Some(failure.error.to_string()),
            },
        };

        state.record(exchange).await;

        // Propagate errors after callbacks. A failure before any of the
        // response went out still gets an answer: a bare close looks like
        // the proxy's own fault rather than the origin's.
        let forwarded = match result {
            Ok(forwarded) => forwarded,
            Err(Failed {
                error,
                responded: false,
            }) => {
                warn!(id = request_id, error = %error, "upstream failed; answering for it");
                send_gateway_error(&mut client, &error, state.config.write_timeout).await;
                return Ok(());
            }
            Err(Failed { error, .. }) => return Err(error),
        };

        // A body delimited by the close, or cut short by the origin, can
        // only be ended for the client by closing too. Waiting for another
        // request instead leaves the client waiting for the end of the body.
        if !forwarded.client_keeps_alive {
            debug!(id = request_id, %client_addr, "closing client connection after response");
            let _ = client.shutdown().await;
            return Ok(());
        }
    }
}

/// A forward that failed.
#[derive(Debug)]
struct Failed {
    error: crate::Error,

    /// Whether any of the response reached the client first. If not,
    /// the proxy can still answer with a `502` or `504`.
    responded: bool,
}

impl Failed {
    fn before_response(error: impl Into<crate::Error>) -> Self {
        Self {
            error: error.into(),
            responded: false,
        }
    }

    fn during_response(error: impl Into<crate::Error>) -> Self {
        Self {
            error: error.into(),
            responded: true,
        }
    }
}

/// A response relayed to the client.
struct Forwarded {
    response: Response,

    /// Whether the client connection can carry another request.
    client_keeps_alive: bool,
}

/// Read a request head from the client, handling the ways it can fail.
///
/// Returns `Ok(Some(parsed))` once headers parse, `Ok(None)` when the
/// connection should be closed — a clean client close, an idle keep-alive
/// timeout, a slowloris drip, or an oversized header block, each logged
/// for what it is. Oversized heads get an RFC 6585 431 before the close.
async fn read_request_headers(
    client: &mut TcpStream,
    buf: &mut BytesMut,
    config: &Config,
    client_addr: SocketAddr,
) -> Result<Option<ParsedRequest>> {
    match read_request_head(client, buf, config.read_timeout, config.max_header_size).await? {
        HeadRead::Parsed(request) => Ok(Some(request)),
        HeadRead::Closed => Ok(None),
        HeadRead::TimedOut { buffered: 0 } => {
            // No bytes arrived at all — ordinary keep-alive idle.
            debug!(%client_addr, "idle client timeout, closing");
            Ok(None)
        }
        HeadRead::TimedOut { buffered } => {
            warn!(
                %client_addr,
                buffered,
                "request header read timed out (possible slowloris); closing"
            );
            Ok(None)
        }
        HeadRead::TooLarge { buffered } => {
            warn!(
                %client_addr,
                buffered,
                max = config.max_header_size,
                "request headers exceeded the maximum size; closing"
            );
            client.write_all(HEADERS_TOO_LARGE_RESPONSE).await?;
            Ok(None)
        }
        HeadRead::Malformed { reason } => {
            warn!(%client_addr, reason, "malformed request head; closing");
            send_proxy_error(client, 400, "Bad Request", &reason, config.write_timeout).await;
            Ok(None)
        }
    }
}

/// Forward an HTTP request to the upstream server, capture and relay the
/// response.
///
/// Returns the Response for use in callbacks, and whether the client
/// connection survives it.
async fn forward_request_with_capture(
    client: &mut TcpStream,
    parsed_request: &ParsedRequest,
    header_bytes: &[u8],
    request_body: &Bytes,
    request_id: u64,
    state: &Arc<ProxyState>,
) -> std::result::Result<Forwarded, Failed> {
    let start = Instant::now();
    let (host, port) = parsed_request
        .host_port()
        .map_err(Failed::before_response)?;
    let config = &state.config;

    // Prefer a pooled connection; on a write or zero-byte read failure
    // (a stale keep-alive), drop it and retry once on a fresh connection.
    // We only retry when zero response bytes have been received, so the
    // retry is safe regardless of request method.
    let (mut upstream, connected, sent_out, connection_reused) = async {
        Ok::<_, crate::Error>(
            if let Some(mut conn) = state.connection_pool.get(&host, port) {
                debug!(id = request_id, host, port, "using pooled connection");
                let connected = Instant::now();
                match send_request(&mut conn, header_bytes, request_body, config).await {
                    Ok(out) => (conn, connected, out, true),
                    Err(SendFailed {
                        error,
                        saw_response_bytes: false,
                    }) => {
                        debug!(
                            id = request_id,
                            host,
                            port,
                            error = %error,
                            "pooled connection appears stale, retrying on a fresh connection"
                        );
                        drop(conn);
                        let (mut fresh, connected) = connect_upstream(
                            &host,
                            port,
                            request_id,
                            &parsed_request.method,
                            config,
                        )
                        .await?;
                        let out = send_request(&mut fresh, header_bytes, request_body, config)
                            .await
                            .map_err(|f| f.error)?;
                        (fresh, connected, out, false)
                    }
                    Err(SendFailed { error, .. }) => return Err(error),
                }
            } else {
                let (mut fresh, connected) =
                    connect_upstream(&host, port, request_id, &parsed_request.method, config)
                        .await?;
                let out = send_request(&mut fresh, header_bytes, request_body, config)
                    .await
                    .map_err(|f| f.error)?;
                (fresh, connected, out, false)
            },
        )
    }
    .await
    .map_err(Failed::before_response)?;

    let remote_addr = upstream.peer_addr().ok();
    let RequestSent {
        mut response_buf,
        mut parsed_response,
        sent,
        first_byte,
    } = sent_out;

    if let Some(reason) = parsed_response.framing_error() {
        return Err(Failed::before_response(crate::Error::InvalidResponse(
            reason.to_owned(),
        )));
    }

    let can_reuse = can_reuse_connection(&parsed_response, parsed_request);

    // Append Mira to the Via header on the way back out, so the client
    // can see which proxies handled the response.
    apply_response_proxy_headers(&mut parsed_response.headers, &parsed_response.version);

    // Send headers to client immediately, with hop-by-hop headers stripped
    // (RFC 7230 §6.1) so that per-connection metadata from the upstream
    // does not leak into the client-facing hop.
    let response_head = serialize_response_head(&parsed_response);
    write_all_within(
        client,
        &response_head,
        config.write_timeout,
        "write response head",
    )
    .await
    .map_err(Failed::during_response)?;

    // `no_body_status` covers HEAD / 1xx / 204 / 304 — no framing, no
    // bytes to read, nothing to relay.
    let body_start = parsed_response.header_len;
    let no_body_status = response_has_no_body(&parsed_request.method, parsed_response.status);

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
            &mut upstream,
            &mut WriteDeadline::new(client, config.write_timeout, "write response body"),
            framing,
            &initial,
            config.read_timeout,
            "read response body",
            config.max_body_size,
        )
        .await
        .map_err(Failed::during_response)?
    };

    if !body.is_complete() {
        warn!(
            id = request_id,
            host, port, "upstream closed mid-body; response relayed truncated"
        );
    }

    let finished = Instant::now();

    let timings = Timings {
        blocked_ms: Some(0.0), // We don't queue requests
        connect_ms: if connection_reused {
            None // -1 in HAR means not applicable
        } else {
            Some((connected - start).as_secs_f64() * 1000.0)
        },
        send_ms: (sent - connected).as_secs_f64() * 1000.0,
        wait_ms: (first_byte - sent).as_secs_f64() * 1000.0,
        receive_ms: (finished - first_byte).as_secs_f64() * 1000.0,
        ssl_ms: None, // Plain HTTP has no TLS
        connection_reused,
    };

    // `Duration::as_millis` returns u128; saturate on the extraordinarily
    // unlikely overflow rather than wrapping silently.
    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    // Whether the connection survives this exchange MUST be decided
    // before the body is consumed for capture.
    let connection_usable = body.leaves_connection_usable();
    let client_continues = connection_usable && client_keeps_alive(parsed_request);
    parsed_response = parsed_response.with_body(body.into_bytes());

    info!(
        id = request_id,
        status = parsed_response.status,
        duration_ms,
        body_size = parsed_response.body.len(),
        reused = connection_reused,
        "response"
    );

    let response = Response::from_parsed_with_timings(
        &parsed_response,
        request_id,
        duration_ms,
        Some(timings),
        remote_addr,
    );

    // Return the connection to the pool only if this exchange left it in
    // a state the next request can use: keep-alive agreed by both sides,
    // and a body that was framed *and* fully read. Pooling a socket the
    // origin closed mid-body just hands the next request a dead
    // connection for its stale-retry to discover.
    if can_reuse && connection_usable {
        state.connection_pool.put(&host, port, upstream);
    }

    if let Some(callback) = &state.callbacks.on_response {
        callback.invoke(response.clone()).await;
    }

    Ok(Forwarded {
        response,
        client_keeps_alive: client_continues,
    })
}

/// Send a request on `upstream` and read back the response head.
///
/// Thin wrapper over [`write_request_and_parse_head`] that pins the
/// timeout label for the plain-HTTP path.
async fn send_request(
    upstream: &mut TcpStream,
    header_bytes: &[u8],
    request_body: &Bytes,
    config: &Config,
) -> std::result::Result<RequestSent, SendFailed> {
    write_request_and_parse_head(
        upstream,
        header_bytes,
        request_body,
        config.read_timeout,
        config.write_timeout,
        config.max_header_size,
        "read response headers",
    )
    .await
}

/// Open a fresh TCP connection to upstream, with the configured timeout.
/// Returns the stream and the instant the connection became live.
async fn connect_upstream(
    host: &str,
    port: u16,
    request_id: u64,
    method: &str,
    config: &Config,
) -> Result<(TcpStream, Instant)> {
    debug!(
        id = request_id,
        host, port, method, "connecting to upstream"
    );
    let connect_future = TcpStream::connect((host, port));
    let conn = timeout(config.connect_timeout, connect_future)
        .await
        .map_err(|_elapsed| crate::Error::Timeout {
            operation: "connect".into(),
            duration: config.connect_timeout,
        })?
        .map_err(|e| crate::Error::UpstreamConnection {
            host: format!("{host}:{port}"),
            source: e,
        })?;
    let connected = Instant::now();
    Ok((conn, connected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::pool::ConnectionPool;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    fn proxy_state() -> Arc<ProxyState> {
        Arc::new(ProxyState {
            config: Config::default(),
            request_counter: AtomicU64::new(1),
            callbacks: Default::default(),
            connection_pool: ConnectionPool::new(),
            session: None,
            tls_config: None,
        })
    }

    /// Start a one-shot origin that answers the first request with
    /// `response`. When `close_after` it then hangs up — which is how a
    /// short Content-Length body reaches the proxy.
    async fn spawn_origin(
        response: &'static [u8],
        close_after: bool,
    ) -> (u16, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
        let port = listener.local_addr().expect("origin addr").port();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut scratch = [0u8; 1024];
            let n = sock.read(&mut scratch).await.expect("read request");
            let seen = scratch[..n].to_vec();
            sock.write_all(response).await.expect("write response");
            if !close_after {
                // Keep the connection open so the pooled socket stays
                // live for the length of the test.
                let _ = sock.read(&mut scratch).await;
            }
            seen
        });
        (port, handle)
    }

    /// A connected TCP pair standing in for the client: the second half
    /// is what the proxy writes the response to.
    async fn client_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind client");
        let addr = listener.local_addr().expect("client addr");
        let dialer = tokio::spawn(async move { TcpStream::connect(addr).await.expect("connect") });
        let (proxy_side, _) = listener.accept().await.expect("accept");
        let client_side = dialer.await.expect("dialer joined");
        (client_side, proxy_side)
    }

    fn parsed_get(port: u16) -> ParsedRequest {
        let raw =
            format!("GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        ParsedRequest::parse(&BytesMut::from(raw.as_bytes()))
            .expect("parses")
            .expect("complete")
    }

    /// An origin that answers two requests on one connection, returning
    /// what it read for each.
    async fn spawn_two_request_origin() -> (u16, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
        let port = listener.local_addr().expect("origin addr").port();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut seen = Vec::new();
            for nth in 1..=2 {
                let mut scratch = [0u8; 1024];
                let n = timeout(Duration::from_secs(2), sock.read(&mut scratch))
                    .await
                    .unwrap_or_else(|_| panic!("request {nth} never reached the origin"))
                    .expect("read");
                seen.push(String::from_utf8_lossy(&scratch[..n]).into_owned());
                sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await
                    .expect("respond");
            }
            seen
        });
        (port, handle)
    }

    #[tokio::test]
    async fn test_absolute_form_websocket_upgrade_reaches_the_origin_in_origin_form() {
        // A client that addresses the proxy directly — rather than
        // CONNECT-ing first — sends the handshake in absolute-form. A
        // server that routes on the request path 404s that, and the
        // failure looks like the origin's fault.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
        let port = listener.local_addr().expect("origin addr").port();
        let origin = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut scratch = [0u8; 1024];
            let n = sock.read(&mut scratch).await.expect("read handshake");
            let seen = String::from_utf8_lossy(&scratch[..n]).into_owned();
            sock.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\n\
                  Upgrade: websocket\r\n\
                  Connection: Upgrade\r\n\
                  Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\
                  \r\n",
            )
            .await
            .expect("write 101");
            seen
        });

        let (mut client_peer, client) = client_pair().await;
        let state = proxy_state();
        let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");

        let handler =
            tokio::spawn(async move { process_connection(client, client_addr, &state).await });

        client_peer
            .write_all(
                format!(
                    "GET http://127.0.0.1:{port}/ws HTTP/1.1\r\nHost: \
                     127.0.0.1:{port}\r\nConnection: Upgrade\r\nUpgrade: \
                     websocket\r\nSec-WebSocket-Key: \
                     dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("write upgrade");

        let seen = origin.await.expect("origin joined");
        assert!(
            seen.starts_with("GET /ws HTTP/1.1\r\n"),
            "origin should see origin-form: {seen}"
        );
        // The handshake headers are forwarded exactly as the client sent
        // them — stripping these is what would break the upgrade.
        assert!(seen.contains("Connection: Upgrade\r\n"), "got: {seen}");
        assert!(seen.contains("Upgrade: websocket\r\n"), "got: {seen}");
        assert!(
            seen.contains("Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"),
            "got: {seen}"
        );

        let mut response = [0u8; 512];
        let n = client_peer.read(&mut response).await.expect("read 101");
        assert!(
            String::from_utf8_lossy(&response[..n])
                .starts_with("HTTP/1.1 101 Switching Protocols\r\n")
        );

        drop(client_peer);
        handler.await.expect("joined").expect("clean exit");
    }

    #[tokio::test]
    async fn test_pipelined_requests_are_forwarded_separately() {
        // Both requests arrive in a single write. The bytes of the second
        // must not be swallowed into the first one's body: that loses the
        // request, and hands the origin body content that is really a
        // request of its own.
        let (port, origin) = spawn_two_request_origin().await;
        let (mut client_peer, client) = client_pair().await;
        let state = proxy_state();
        let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");

        let handler =
            tokio::spawn(async move { process_connection(client, client_addr, &state).await });

        client_peer
            .write_all(
                format!(
                    "POST http://127.0.0.1:{port}/one HTTP/1.1\r\n\
                     Host: 127.0.0.1:{port}\r\n\
                     Content-Length: 5\r\n\
                     \r\n\
                     helloGET http://127.0.0.1:{port}/two HTTP/1.1\r\n\
                     Host: 127.0.0.1:{port}\r\n\
                     \r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("write pipelined requests");

        let seen = origin.await.expect("origin joined");

        assert!(
            seen[0].starts_with("POST /one HTTP/1.1\r\n"),
            "got: {}",
            seen[0]
        );
        assert!(
            seen[0].ends_with("\r\n\r\nhello"),
            "the first request must stop at its body: {}",
            seen[0]
        );
        assert!(
            !seen[0].contains("/two"),
            "the second request must not ride along as body bytes: {}",
            seen[0]
        );
        assert!(
            seen[1].starts_with("GET /two HTTP/1.1\r\n"),
            "got: {}",
            seen[1]
        );

        // Both responses reach the client.
        let mut response = Vec::new();
        while response.windows(2).filter(|w| *w == b"ok").count() < 2 {
            let mut scratch = [0u8; 512];
            let n = timeout(Duration::from_secs(2), client_peer.read(&mut scratch))
                .await
                .expect("client read timed out")
                .expect("read");
            assert!(n > 0, "connection closed before both responses");
            response.extend_from_slice(&scratch[..n]);
        }

        drop(client_peer);
        handler.await.expect("joined").expect("clean exit");
    }

    #[tokio::test]
    async fn test_client_is_closed_after_a_close_delimited_response() {
        // With no Content-Length and no chunking, the client can only tell
        // where the body ends when the connection closes.
        let (port, _origin) = spawn_origin(b"HTTP/1.1 200 OK\r\n\r\nhello", true).await;
        let (mut client_peer, client) = client_pair().await;
        let state = proxy_state();
        let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");

        let handler =
            tokio::spawn(async move { process_connection(client, client_addr, &state).await });

        client_peer
            .write_all(
                format!("GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .expect("write request");

        let mut seen = Vec::new();
        timeout(Duration::from_secs(2), client_peer.read_to_end(&mut seen))
            .await
            .expect("the proxy must close rather than wait for another request")
            .expect("read");
        assert!(String::from_utf8_lossy(&seen).ends_with("\r\n\r\nhello"));

        handler.await.expect("joined").expect("clean exit");
    }

    #[tokio::test]
    async fn test_interim_response_is_not_relayed_as_the_answer() {
        let (port, origin) = spawn_origin(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nok",
            false,
        )
        .await;
        let (mut client_peer, client) = client_pair().await;
        let state = proxy_state();
        let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");

        let handler =
            tokio::spawn(async move { process_connection(client, client_addr, &state).await });

        client_peer
            .write_all(
                format!(
                    "POST http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
                     Expect: 100-continue\r\nContent-Length: 2\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("write head");

        // The proxy answers the expectation itself, then reads the body.
        let mut seen = vec![0u8; 25];
        timeout(Duration::from_secs(2), client_peer.read_exact(&mut seen))
            .await
            .expect("100 Continue arrives without the client sending its body")
            .expect("read");
        assert_eq!(seen, b"HTTP/1.1 100 Continue\r\n\r\n");
        client_peer.write_all(b"hi").await.expect("write body");

        let mut response = Vec::new();
        while !response.ends_with(b"ok") {
            let mut scratch = [0u8; 512];
            let n = timeout(Duration::from_secs(2), client_peer.read(&mut scratch))
                .await
                .expect("final response arrives")
                .expect("read");
            assert!(n > 0, "connection closed before the final response");
            response.extend_from_slice(&scratch[..n]);
        }
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 201 Created\r\n"), "got: {text}");
        assert!(
            !text.contains("100 Continue"),
            "the origin's 100 is not relayed: {text}"
        );

        drop(client_peer);
        drop(origin);
        handler.await.expect("joined").expect("clean exit");
    }

    /// Send `request` through a fresh connection handler and return all
    /// the client receives before the proxy closes.
    async fn exchange_once(request: String) -> String {
        let (mut client_peer, client) = client_pair().await;
        let state = proxy_state();
        let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");
        let handler =
            tokio::spawn(async move { process_connection(client, client_addr, &state).await });

        client_peer
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut seen = Vec::new();
        timeout(Duration::from_secs(5), client_peer.read_to_end(&mut seen))
            .await
            .expect("the proxy answers and closes")
            .expect("read");
        let _ = handler.await.expect("joined");
        String::from_utf8_lossy(&seen).into_owned()
    }

    #[tokio::test]
    async fn test_passthrough_connect_relays_early_bytes_and_is_recorded() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
        let port = listener.local_addr().expect("origin addr").port();
        let origin = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut seen = vec![0u8; 11];
            sock.read_exact(&mut seen).await.expect("read tunnel bytes");
            seen
        });

        let session = crate::Session::new();
        let mut state = Arc::try_unwrap(proxy_state()).expect("sole owner");
        state.session = Some(session.clone());
        let state = Arc::new(state);

        let (mut client_peer, client) = client_pair().await;
        let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");
        let handler =
            tokio::spawn(async move { process_connection(client, client_addr, &state).await });

        // The tunnel's first bytes ride in the same write as the CONNECT.
        client_peer
            .write_all(
                format!(
                    "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\nhello world"
                )
                .as_bytes(),
            )
            .await
            .expect("write CONNECT");

        let seen = timeout(Duration::from_secs(2), origin)
            .await
            .expect("tunnel bytes reach the origin")
            .expect("origin joined");
        assert_eq!(seen, b"hello world");

        let captured = session.exchanges();
        assert_eq!(captured.len(), 1, "the CONNECT is recorded");
        let response = captured[0].exchange.response.as_ref().expect("response");
        assert_eq!(response.status, 200);

        drop(client_peer);
        handler.await.expect("joined").expect("clean exit");
    }

    #[tokio::test]
    async fn test_unreachable_origin_gets_a_502() {
        // Bind and release a port, so nothing is listening on it.
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr").port()
        };

        let seen = exchange_once(format!(
            "GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
        ))
        .await;

        assert!(
            seen.starts_with("HTTP/1.1 502 Bad Gateway\r\n"),
            "got: {seen}"
        );
        assert!(seen.contains("upstream connection failed"), "got: {seen}");
    }

    #[tokio::test]
    async fn test_ambiguous_framing_is_rejected() {
        for headers in [
            "Content-Length: 5\r\nContent-Length: 6\r\n",
            "Content-Length: +5\r\n",
            "Transfer-Encoding: chunked, gzip\r\n",
        ] {
            let seen = exchange_once(format!(
                "POST http://127.0.0.1:9/ HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{headers}\r\nhello"
            ))
            .await;
            assert!(
                seen.starts_with("HTTP/1.1 400 Bad Request\r\n"),
                "{headers}: {seen}"
            );
        }
    }

    #[tokio::test]
    async fn test_malformed_request_gets_a_400() {
        let seen = exchange_once("GET / HTTP/1.1\r\nBad Header\r\n\r\n".to_owned()).await;
        assert!(
            seen.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "got: {seen}"
        );
    }

    #[tokio::test]
    async fn test_oversized_request_body_gets_a_413() {
        let max = Config::default().max_body_size;
        let seen = exchange_once(format!(
            "POST http://127.0.0.1:9/ HTTP/1.1\r\nHost: 127.0.0.1:9\r\nContent-Length: {}\r\n\r\n",
            max + 1
        ))
        .await;
        assert!(
            seen.starts_with("HTTP/1.1 413 Content Too Large\r\n"),
            "got: {seen}"
        );
    }

    #[tokio::test]
    async fn test_absolute_form_request_reaches_the_origin_in_origin_form() {
        // What a client sends a proxy — `GET http://host/path` — is not
        // what an origin expects to read.
        let (port, origin) =
            spawn_origin(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi", true).await;
        let (mut client_peer, client) = client_pair().await;
        let state = proxy_state();
        let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");

        let handler =
            tokio::spawn(async move { process_connection(client, client_addr, &state).await });

        client_peer
            .write_all(
                format!(
                    "GET http://127.0.0.1:{port}/path?q=1 HTTP/1.1\r\nHost: \
                     127.0.0.1:{port}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("write request");

        let seen = origin.await.expect("origin joined");
        let text = String::from_utf8_lossy(&seen);
        assert!(
            text.starts_with("GET /path?q=1 HTTP/1.1\r\n"),
            "origin should see origin-form, got: {text}"
        );
        assert!(
            text.contains(&format!("Host: 127.0.0.1:{port}\r\n")),
            "the authority must survive as the Host header, got: {text}"
        );

        // Drain the response before hanging up, so the handler is not
        // writing into a closed socket when the test ends.
        let mut response = [0u8; 512];
        let n = client_peer
            .read(&mut response)
            .await
            .expect("read response");
        assert!(String::from_utf8_lossy(&response[..n]).starts_with("HTTP/1.1 200 OK\r\n"));

        drop(client_peer);
        handler.await.expect("joined").expect("clean exit");
    }

    #[tokio::test]
    async fn test_target_authority_wins_over_a_mismatched_host_header() {
        // RFC 7230 §5.4. The proxy connects to the authority in the
        // target, so the Host it forwards has to name the same origin.
        let (port, origin) =
            spawn_origin(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi", true).await;
        let (mut client_peer, client) = client_pair().await;
        let state = proxy_state();
        let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");

        let handler =
            tokio::spawn(async move { process_connection(client, client_addr, &state).await });

        client_peer
            .write_all(
                format!("GET http://127.0.0.1:{port}/p HTTP/1.1\r\nHost: spoofed.example\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .expect("write request");

        let seen = origin.await.expect("origin joined");
        let text = String::from_utf8_lossy(&seen);
        assert!(
            text.contains(&format!("Host: 127.0.0.1:{port}\r\n")),
            "got: {text}"
        );
        assert!(
            !text.contains("spoofed.example"),
            "the client's mismatched Host must not be forwarded, got: {text}"
        );

        let mut response = [0u8; 512];
        let n = client_peer
            .read(&mut response)
            .await
            .expect("read response");
        assert!(String::from_utf8_lossy(&response[..n]).starts_with("HTTP/1.1 200 OK\r\n"));

        drop(client_peer);
        handler.await.expect("joined").expect("clean exit");
    }

    #[tokio::test]
    async fn test_oversized_request_headers_get_a_431_and_close() {
        let (client_peer, mut client) = client_pair().await;
        let (mut peer_read, mut peer_write) = client_peer.into_split();
        let config = Config::default().with_max_header_size(1024);
        let client_addr: SocketAddr = "127.0.0.1:34567".parse().expect("client addr");

        // A client that keeps sending headers and never terminates them.
        let flood = tokio::spawn(async move {
            peer_write
                .write_all(b"GET http://example.com/ HTTP/1.1\r\n")
                .await
                .expect("write request line");
            let line = format!("X-Pad: {}\r\n", "a".repeat(512));
            while peer_write.write_all(line.as_bytes()).await.is_ok() {}
        });

        let mut buf = BytesMut::new();
        let parsed = read_request_headers(&mut client, &mut buf, &config, client_addr)
            .await
            .expect("an oversized head is handled, not an error");
        assert!(parsed.is_none(), "the connection must be closed");

        // The client is told why rather than just dropped.
        let mut seen = vec![0u8; 128];
        let n = peer_read.read(&mut seen).await.expect("read response");
        let text = String::from_utf8_lossy(&seen[..n]);
        assert!(
            text.starts_with("HTTP/1.1 431 Request Header Fields Too Large\r\n"),
            "got: {text}"
        );

        drop(client);
        flood.await.expect("flood joined");
    }

    #[tokio::test]
    async fn test_fully_read_body_returns_connection_to_pool() {
        let (port, _origin) =
            spawn_origin(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello", false).await;
        let (_client_peer, mut client) = client_pair().await;
        let state = proxy_state();
        let parsed = parsed_get(port);
        let head = serialize_request_head(&parsed);

        let Forwarded {
            response,
            client_keeps_alive,
        } = forward_request_with_capture(&mut client, &parsed, &head, &Bytes::new(), 1, &state)
            .await
            .expect("forwarded");

        assert_eq!(response.status, 200);
        assert_eq!(response.body, Bytes::from_static(b"hello"));
        assert_eq!(
            state.connection_pool.len(),
            1,
            "a keep-alive response whose body arrived in full is reusable"
        );
        assert!(
            client_keeps_alive,
            "and the client can send another request"
        );
    }

    #[tokio::test]
    async fn test_truncated_body_does_not_return_connection_to_pool() {
        // Ten bytes promised, three delivered, then the origin hangs up.
        let (port, _origin) =
            spawn_origin(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc", true).await;
        let (_client_peer, mut client) = client_pair().await;
        let state = proxy_state();
        let parsed = parsed_get(port);
        let head = serialize_request_head(&parsed);

        let Forwarded {
            response,
            client_keeps_alive,
        } = forward_request_with_capture(&mut client, &parsed, &head, &Bytes::new(), 1, &state)
            .await
            .expect("forwarded");

        // The partial body is still relayed and captured — the client
        // detects the truncation from the unsatisfied Content-Length.
        assert_eq!(response.body, Bytes::from_static(b"abc"));
        assert!(
            state.connection_pool.is_empty(),
            "a connection the origin closed mid-body must not be pooled"
        );
        assert!(
            !client_keeps_alive,
            "the client must be closed on, or it waits for the missing bytes"
        );
    }

    #[tokio::test]
    async fn test_unframed_body_does_not_return_connection_to_pool() {
        // No Content-Length and no chunking: the close itself delimits
        // the body, so the connection cannot survive it.
        let (port, _origin) = spawn_origin(b"HTTP/1.1 200 OK\r\n\r\nhello", true).await;
        let (_client_peer, mut client) = client_pair().await;
        let state = proxy_state();
        let parsed = parsed_get(port);
        let head = serialize_request_head(&parsed);

        let Forwarded {
            response,
            client_keeps_alive,
        } = forward_request_with_capture(&mut client, &parsed, &head, &Bytes::new(), 1, &state)
            .await
            .expect("forwarded");

        assert_eq!(response.body, Bytes::from_static(b"hello"));
        assert!(
            state.connection_pool.is_empty(),
            "a close-delimited body leaves nothing to reuse"
        );
        assert!(
            !client_keeps_alive,
            "the close is how the client learns the body ended"
        );
    }
}
