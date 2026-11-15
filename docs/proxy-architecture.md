# Proxy Architecture

This document explains the internal architecture of Mira's HTTP/HTTPS intercepting proxy.

## Overview

Mira is built on Tokio's async runtime and uses a connection-per-task model. Each incoming client connection spawns a dedicated async task that handles the full request lifecycle. The accept loop holds a semaphore of `max_connections` permits, so once that many connections are in flight it stops accepting and further clients wait in the kernel accept queue rather than becoming unbounded tasks.

```
┌─────────────────────────────────────────────────────────────────────────┐
│                              Proxy Server                               │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│  ┌───────────────┐    ┌──────────────────────────────────────────────┐  │
│  │  TcpListener  │───▶│  Accept Loop (spawns task per connection)    │  │
│  └───────────────┘    └──────────────────────────────────────────────┘  │
│                                        │                                │
│                                        ▼                                │
│  ┌──────────────────────────────────────────────────────────────────┐   │
│  │                        ProxyState (shared)                       │   │
│  ├───────────────┬────────────────┬─────────────┬───────────────────┤   │
│  │    Config     │   Callbacks    │   Session   │    TlsConfig      │   │
│  │               │                │  (capture)  │   (optional)      │   │
│  ├───────────────┴────────────────┴─────────────┴───────────────────┤   │
│  │                      ConnectionPool                              │   │
│  └──────────────────────────────────────────────────────────────────┘   │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

## Core Components

### ProxyState

Shared state wrapped in `Arc`, accessible by all connection handlers:

| Field             | Type             | Purpose                                    |
| ----------------- | ---------------- | ------------------------------------------ |
| `config`          | `Config`         | Proxy settings (ports, timeouts, filters)  |
| `request_counter` | `AtomicU64`      | Generates unique request IDs               |
| `callbacks`       | `Callbacks`      | User-registered event handlers             |
| `connection_pool` | `ConnectionPool` | HTTP/1.1 keep-alive connection reuse       |
| `session`         | `Option<Session>`| Captured exchanges for export              |
| `tls_config`      | `Option<TlsConfig>` | CA and cert cache for HTTPS interception |

### Connection Handler

Each client connection follows this flow:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           Connection Handler                            │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│   ┌─────────────────────────────────────────────────────────────────┐   │
│   │  Reject clients with public IPs unless --allow-external is set  │   │
│   └─────────────────────────────────────────────────────────────────┘   │
│                                   │                                     │
│                                   ▼                                     │
│   ┌─────────────────────────────────────────────────────────────────┐   │
│   │  Read the request head under a total deadline (slowloris cap)   │   │
│   └─────────────────────────────────────────────────────────────────┘   │
│                                   │                                     │
│                                   ▼                                     │
│   ┌─────────────────────────────────────────────────────────────────┐   │
│   │  Rewrite an absolute-form target to origin-form                 │   │
│   └─────────────────────────────────────────────────────────────────┘   │
│                                   │                                     │
│                                   ▼                                     │
│   ┌─────────────────────────────────────────────────────────────────┐   │
│   │  Apply the host filter (403 and close when blocked)             │   │
│   └─────────────────────────────────────────────────────────────────┘   │
│                                   │                                     │
│        ┌─────────────────┬────────┴────────┬─────────────────┐          │
│        ▼                 ▼                 ▼                 ▼          │
│ ┌─────────────┐   ┌─────────────┐   ┌─────────────┐   ┌─────────────┐   │
│ │   CONNECT   │   │  mira.proxy │   │  WebSocket  │   │  Plain HTTP │   │
│ │   (HTTPS)   │   │   (magic)   │   │   Upgrade   │   │   Request   │   │
│ └─────────────┘   └─────────────┘   └─────────────┘   └─────────────┘   │
│        │                 │                 │                 │          │
│        ▼                 ▼                 ▼                 ▼          │
│ ┌─────────────┐   ┌─────────────┐   ┌─────────────┐   ┌─────────────┐   │
│ │  Tunnel or  │   │   Serve CA  │   │    Tunnel   │   │   Forward   │   │
│ │  intercept  │   │     page    │   │   handler   │   │   request   │   │
│ └─────────────┘   └─────────────┘   └─────────────┘   └─────────────┘   │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

The proxy speaks HTTP/1.1 to its clients — that is what carries `CONNECT` — so
HTTP/2 only ever appears inside a TLS session the proxy terminates.

## Request Flows

### Plain HTTP

Direct forwarding without encryption:

```
    Client                      Proxy                       Upstream
       │                          │                            │
       │  GET /api/data HTTP/1.1  │                            │
       │  Host: example.com       │                            │
       │─────────────────────────▶│                            │
       │                          │                            │
       │                          │  GET /api/data HTTP/1.1    │
       │                          │  Host: example.com         │
       │                          │───────────────────────────▶│
       │                          │                            │
       │                          │  HTTP/1.1 200 OK           │
       │                          │  Content-Type: application/│
       │                          │◀───────────────────────────│
       │                          │                            │
       │  HTTP/1.1 200 OK         │                            │
       │  Content-Type: application/                           │
       │◀─────────────────────────│                            │
       │                          │                            │
```

### HTTPS Passthrough

When interception is disabled, the proxy blindly tunnels encrypted bytes:

```
    Client                      Proxy                       Upstream
       │                          │                            │
       │  CONNECT example.com:443 │                            │
       │─────────────────────────▶│                            │
       │                          │                            │
       │  HTTP/1.1 200            │                            │
       │  Connection Established  │      TCP connect           │
       │◀─────────────────────────│───────────────────────────▶│
       │                          │                            │
       │                          │                            │
       ╠══════════════════════════╬════════════════════════════╣
       ║     Encrypted TLS tunnel (bidirectional)              ║
       ║     Proxy cannot read content                         ║
       ╠══════════════════════════╬════════════════════════════╣
       │                          │                            │
```

The proxy uses `tokio::io::copy_bidirectional` to efficiently shuffle bytes between client and upstream without inspecting them.

### HTTPS Interception (MITM)

When interception is enabled, the proxy terminates TLS on both sides:

```
    Client                      Proxy                       Upstream
       │                          │                            │
       │  CONNECT example.com:443 │                            │
       │─────────────────────────▶│                            │
       │                          │                            │
       │  HTTP/1.1 200            │                            │
       │  Connection Established  │                            │
       │◀─────────────────────────│                            │
       │                          │                            │
       │                          │                            │
       │  ┌────────────────────┐  │                            │
       │  │ TLS Handshake      │  │                            │
       │  │ (proxy's cert for  │  │                            │
       │  │  example.com)      │  │                            │
       │◀═╪════════════════════╪═▶│                            │
       │  └────────────────────┘  │                            │
       │                          │  ┌────────────────────┐    │
       │                          │  │ TLS Handshake      │    │
       │                          │  │ (real server cert) │    │
       │                          │◀═╪════════════════════╪═══▶│
       │                          │  └────────────────────┘    │
       │                          │                            │
       │  GET /api HTTP/1.1       │  GET /api HTTP/1.1         │
       │─────────────────────────▶│───────────────────────────▶│
       │         (decrypted)      │        (re-encrypted)      │
       │                          │                            │
       │  HTTP/1.1 200 OK         │  HTTP/1.1 200 OK           │
       │◀─────────────────────────│◀───────────────────────────│
       │                          │                            │
```

### HTTP/2 Inside an Intercepted Session

The client-side TLS handshake negotiates ALPN. When the client picks `h2`,
the session is handed to hyper's HTTP/2 server instead of the sequential
request/response loop, and each stream is forwarded through the same
primitives the HTTP/1.1 path uses.

```
    Client                      Proxy                       Origin
       │                          │                            │
       │ CONNECT example.com:443  │                            │
       │─────────────────────────▶│                            │
       │ 200 Connection Est.      │                            │
       │◀─────────────────────────│                            │
       │                          │                            │
       │ TLS: h2, http/1.1        │                            │
       │◀════════════════════════▶│                            │
       │ negotiated: h2           │                            │
       │                          │                            │
       │                          │ TLS: h2, http/1.1          │
       │                          │◀══════════════════════════▶│
       │                          │ the origin picks           │
       │                          │                            │
       │ stream 1  ──────────────▶│                            │
       │ stream 3  ──────────────▶│                            │
       │ stream 5  ──────────────▶│                            │
       │                          │ forwarded per stream       │
       │                          │───────────────────────────▶│
       │                          │                            │
       │ responses, interleaved   │                            │
       │◀─────────────────────────│────────────────────────────│
       │                          │                            │
```

The upstream leg is discovered on the first connection and shared by the
whole session:

| Origin speaks | Upstream leg                                                            |
| ------------- | ----------------------------------------------------------------------- |
| `h2`          | One multiplexed connection every stream sends on                        |
| `http/1.1`    | Exclusive connections streams check out and return; idle ones are kept, up to `max_idle_upstreams_per_session` (default 4) |

Details that differ from the HTTP/1.1 path:

- **Connection-specific headers are illegal**, not merely ignored (RFC 9113 §8.2.2). A request carrying one is rejected, and none are generated on the way out.
- **`:authority` outranks a `host` header** when a client sends both (RFC 9113 §8.3.1); the `Host` sent upstream is derived from it.
- **Request bodies are buffered** up to `max_body_size`; response bodies stream to the client through a short channel, so a fast origin and a slow client cannot buffer a whole response in memory.
- **Bodies are reframed for the other leg.** A chunked HTTP/1.1 origin body reaches the client as its decoded payload, since HTTP/2 frames bodies itself. A request body sent to an HTTP/1.1 origin always carries a `Content-Length`, which HTTP/2 clients need not send.
- **A truncated origin body resets the stream** instead of ending it cleanly, so the client cannot mistake a partial body for a whole one.
- **Captured exchanges record `HTTP/2`** as their version, and the client-side handshake cost is folded into the first exchange the session carries.

The magic domain is an exception: its pages are written as HTTP/1.1 responses
directly onto the stream, so that session is offered `http/1.1` only.

## TLS Interception Details

### Certificate Generation Flow

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         TLS Configuration                               │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│   ┌─────────────────────┐      ┌─────────────────────────────────────┐  │
│   │ CertificateAuthority│      │         CertificateCache            │  │
│   ├─────────────────────┤      ├─────────────────────────────────────┤  │
│   │ • Root CA cert      │      │ • DashMap<hostname, TlsAcceptor>    │  │
│   │ • CA private key    │      │ • Thread-safe concurrent access     │  │
│   │ • 10-year validity  │      │ • Avoids regenerating certificates  │  │
│   └─────────────────────┘      └─────────────────────────────────────┘  │
│            │                                    ▲                       │
│            │ signs                              │ caches                │
│            ▼                                    │                       │
│   ┌─────────────────────┐                       │                       │
│   │   Leaf Certificate  │───────────────────────┘                       │
│   ├─────────────────────┤                                               │
│   │ • Per-host cert     │                                               │
│   │ • 1-year validity   │                                               │
│   │ • SAN: hostname     │                                               │
│   └─────────────────────┘                                               │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### acceptor_for_host() Flow

```
                           ┌──────────────────────┐
                           │  Request for host    │
                           │  "api.example.com"   │
                           └──────────────────────┘
                                      │
                                      ▼
                           ┌──────────────────────┐
                           │  Check cert cache    │
                           └──────────────────────┘
                                      │
                    ┌─────────────────┴─────────────────┐
                    │                                   │
                    ▼                                   ▼
           ┌───────────────┐                   ┌───────────────┐
           │  Cache HIT    │                   │  Cache MISS   │
           └───────────────┘                   └───────────────┘
                    │                                   │
                    │                                   ▼
                    │                          ┌───────────────────────┐
                    │                          │  Generate leaf cert   │
                    │                          │  signed by CA         │
                    │                          └───────────────────────┘
                    │                                   │
                    │                                   ▼
                    │                          ┌───────────────────────┐
                    │                          │  Create TlsAcceptor   │
                    │                          └───────────────────────┘
                    │                                   │
                    │                                   ▼
                    │                          ┌───────────────────────┐
                    │                          │  Store in cache       │
                    │                          └───────────────────────┘
                    │                                   │
                    └─────────────────┬─────────────────┘
                                      │
                                      ▼
                           ┌──────────────────────┐
                           │  Return TlsAcceptor  │
                           └──────────────────────┘
```

### ALPN

The acceptor is built before the `200 Connection Established` is sent, so a
certificate failure can still be answered with a real `502` rather than
leaving the client in a half-TLS state. What it offers depends on the host:

| Session                | Offered to the client | Offered to the origin |
| ---------------------- | --------------------- | --------------------- |
| Intercepted host       | `h2`, `http/1.1`      | Mirrors what the client negotiated |
| `mira.proxy`           | `http/1.1` only       | n/a (served locally)  |

Acceptors are cached per host, keyed so that the two protocol offers cannot
collide — the certificate is the same, the negotiated protocol is not. The
cache holds at most 1024 acceptors (`MAX_CACHED_ACCEPTORS`); past that, adding
a host evicts another, whose certificate is generated again if it returns.
Certificates are dated a day back, so a client whose clock runs slightly slow
still accepts them. The
upstream `ClientConfig` loads the system trust store, which is far too
expensive to repeat per connection, so one connector per offered protocol is
built on first use and reused for the life of the process.

## Connection Pooling

Plain-HTTP upstream connections are pooled for keep-alive reuse. Intercepted
TLS sessions do not use this pool — their upstream connections are TLS
sessions tied to one host, held by the tunnel (HTTP/1.1) or the HTTP/2
session (see above) for as long as the client's connection lives.

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          ConnectionPool                                 │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│   HashMap<String, Vec<PooledConnection>>                                │
│                                                                         │
│   ┌───────────────────────────────────────────────────────────────┐     │
│   │ "api.example.com:80"                                          │     │
│   ├───────────────────────────────────────────────────────────────┤     │
│   │ ┌─────────────┐  ┌─────────────┐  ┌─────────────┐             │     │
│   │ │ Connection 1│  │ Connection 2│  │ Connection 3│             │     │
│   │ │ idle: 5s    │  │ idle: 12s   │  │ idle: 45s   │             │     │
│   │ └─────────────┘  └─────────────┘  └─────────────┘             │     │
│   └───────────────────────────────────────────────────────────────┘     │
│                                                                         │
│   ┌───────────────────────────────────────────────────────────────┐     │
│   │ "cdn.example.net:80"                                          │     │
│   ├───────────────────────────────────────────────────────────────┤     │
│   │ ┌─────────────┐  ┌─────────────┐                              │     │
│   │ │ Connection 1│  │ Connection 2│                              │     │
│   │ │ idle: 2s    │  │ idle: 8s    │                              │     │
│   │ └─────────────┘  └─────────────┘                              │     │
│   └───────────────────────────────────────────────────────────────┘     │
│                                                                         │
│   Configuration:                                                        │
│   • Max 4 connections per host                                          │
│   • 60-second idle timeout                                              │
│   • Automatic cleanup of expired connections                            │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

### Pool Operations

| Operation | Description                                            |
| --------- | ------------------------------------------------------ |
| `get()`   | Retrieve an idle connection, or return None            |
| `put()`   | Return a connection to the pool after use              |
| cleanup   | Expired connections are removed on next `get()` call   |

## Callback System

Callbacks allow observing (and eventually modifying) traffic:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          Request Lifecycle                              │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│   ┌─────────────────┐                                                   │
│   │ Parse Request   │                                                   │
│   │ + Read Body     │                                                   │
│   └────────┬────────┘                                                   │
│            │                                                            │
│            ▼                                                            │
│   ┌─────────────────┐     ┌──────────────────────────────────────────┐  │
│   │  on_request()   │────▶│  User callback receives Request struct   │  │
│   └────────┬────────┘     │  • id, method, url, headers, body        │  │
│            │              └──────────────────────────────────────────┘  │
│            ▼                                                            │
│   ┌─────────────────┐                                                   │
│   │ Forward to      │                                                   │
│   │ Upstream        │                                                   │
│   └────────┬────────┘                                                   │
│            │                                                            │
│            ▼                                                            │
│   ┌─────────────────┐                                                   │
│   │ Receive Response│                                                   │
│   │ + Stream Body   │                                                   │
│   └────────┬────────┘                                                   │
│            │                                                            │
│            ▼                                                            │
│   ┌─────────────────┐     ┌──────────────────────────────────────────┐  │
│   │  on_response()  │────▶│  User callback receives Response struct  │  │
│   └────────┬────────┘     │  • status, headers, body, timings        │  │
│            │              └──────────────────────────────────────────┘  │
│            ▼                                                            │
│   ┌─────────────────┐     ┌──────────────────────────────────────────┐  │
│   │  on_exchange()  │────▶│  User callback receives Exchange struct  │  │
│   └────────┬────────┘     │  • Complete request + response pair      │  │
│            │              └──────────────────────────────────────────┘  │
│            ▼                                                            │
│   ┌─────────────────┐                                                   │
│   │ Session Capture │                                                   │
│   │ (if enabled)    │                                                   │
│   └─────────────────┘                                                   │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

Session capture is bounded by `SessionLimits` (entry count, per-body
bytes, and total body bytes), so a proxy left running does not grow
without limit. Passing the limits is what the `[capture]` section of the
config file does; exceeding them evicts the oldest exchanges, which
`Session::dropped_count()` reports. Callbacks always see the full,
untruncated exchange — only what the session retains is bounded.

### Handler Types

Both sync and async callbacks are supported:

```rust
// Sync callback
proxy.on_request(|req| {
    println!("{} {}", req.method, req.url);
});

// Async callback
proxy.on_response_async(|res| async move {
    save_to_database(&res).await;
});
```

## Timing Capture

Each response includes detailed timing information:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                            Request Timeline                             │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                         │
│   blocked      connect        ssl         send      wait      receive   │
│   ├──────┼───────────────┼───────────┼──────────┼─────────┼──────────┤  │
│   │      │               │           │          │         │          │  │
│   ▼      ▼               ▼           ▼          ▼         ▼          ▼  │
│ Start  DNS+TCP      TLS handshake  Request   First    Response       │  │
│ time   connect       (if HTTPS)    sent      byte     complete       │  │
│                                              (TTFB)                     │
│                                                                         │
│   Timings struct:                                                       │
│   ┌────────────────────────────────────────────────────────────────┐    │
│   │ blocked_ms:  Time waiting in queue (currently always 0)        │    │
│   │ connect_ms:  TCP connection time (None if reused)              │    │
│   │ ssl_ms:      TLS handshake time (None for plain HTTP)          │    │
│   │ send_ms:     Time to send request                              │    │
│   │ wait_ms:     Time to first byte (server processing)            │    │
│   │ receive_ms:  Time to download response body                    │    │
│   │ connection_reused: Whether connection came from pool           │    │
│   └────────────────────────────────────────────────────────────────┘    │
│                                                                         │
└─────────────────────────────────────────────────────────────────────────┘
```

On an intercepted session, the client-side TLS handshake is a one-time cost
of the tunnel rather than of any upstream connection, so it is folded into
the `ssl_ms` of the first exchange the session carries.

## WebSocket Handling

WebSocket upgrades are detected and tunneled:

```
    Client                      Proxy                       Upstream
       │                          │                            │
       │  GET /ws HTTP/1.1        │                            │
       │  Connection: Upgrade     │                            │
       │  Upgrade: websocket      │                            │
       │─────────────────────────▶│                            │
       │                          │                            │
       │                          │  GET /ws HTTP/1.1          │
       │                          │  Connection: Upgrade       │
       │                          │  Upgrade: websocket        │
       │                          │───────────────────────────▶│
       │                          │                            │
       │                          │  HTTP/1.1 101 Switching    │
       │                          │  Protocols                 │
       │                          │◀───────────────────────────│
       │                          │                            │
       │  HTTP/1.1 101 Switching  │                            │
       │  Protocols               │                            │
       │◀─────────────────────────│                            │
       │                          │                            │
       │                          │                            │
       ╠══════════════════════════╬════════════════════════════╣
       ║   WebSocket frames (bidirectional byte stream)        ║
       ╠══════════════════════════╬════════════════════════════╣
       │                          │                            │
```

Key difference from HTTPS CONNECT:
- WebSocket uses a regular HTTP request with upgrade headers (not CONNECT)
- Proxy forwards the upgrade request and waits for 101 response
- After 101 confirmation, switches to bidirectional tunneling

The same handling applies to `wss://` inside an intercepted TLS session: the
upgrade is recognized there too, and once the origin answers `101` the
decrypted streams are copied in both directions. An upgrade the origin
*rejects* is not a tunnel — the whole response, body included, is forwarded
back to the client as an ordinary exchange.

## Magic Domain

Requests for `mira.proxy` are answered by the proxy itself rather than
forwarded, so a client configured to use Mira can fetch the CA without any
other network access:

| Path               | Response                                    |
| ------------------ | ------------------------------------------- |
| `/`                | HTML page with per-platform trust instructions |
| `/ca.crt`, `/ca.der` | CA certificate, DER                       |
| `/ca.pem`          | CA certificate, PEM                         |

It works over plain HTTP and over intercepted HTTPS. The HTTPS route only
succeeds for a client that already trusts the CA, which is the expected
outcome for someone who is there to install it.

## Lifecycle and Limits

| Bound                  | Effect                                                             |
| ---------------------- | ------------------------------------------------------------------ |
| `max_connections`      | Accept-loop semaphore; clients queue in the kernel instead of spawning tasks |
| `max_header_size`      | A head that grows past this without terminating is rejected with `431` |
| `max_body_size`        | Larger request bodies are rejected. Larger response bodies are relayed in full but captured only up to this size |
| `connect_timeout`      | Upstream TCP connect, and the upstream TLS handshake               |
| `read_timeout`         | Each upstream read, and the client TLS handshake on an intercepted `CONNECT` |
| `write_timeout`        | Each write to an upstream or a client, so a peer that stops reading cannot hold its connection forever |
| Decompressed bodies    | Capped at 128 MB (`MAX_DECOMPRESSED_SIZE`), so a compression bomb cannot exhaust memory |
| Request-head deadline  | Total, not per-read, so a byte-drip client cannot reset the clock  |
| `SessionLimits`        | Bounds retained exchanges and body bytes; oldest are evicted       |

`Proxy::run_until(shutdown)` stops polling `accept()` when the shutdown
future resolves, then waits a bounded grace period for in-flight handlers so
their `on_exchange` callbacks still fire and their exchanges land in the
session. `Proxy::run()` is the same thing with a shutdown that never
resolves. `run_until` is `Proxy::bind()` followed by
`Proxy::serve_until(listener, shutdown)`. The CLI calls the two separately,
so a port that is already in use is reported before the TUI takes over the
terminal. Draining on quit is what makes the HAR export include requests
that were still in flight.

## HTTP/1.1 Framing Rules

Both HTTP/1.1 paths, plain and intercepted, share these rules:

- **`Transfer-Encoding: chunked` wins over `Content-Length`** when a message carries both (RFC 9112 §6.3), and the `Content-Length` is removed before forwarding. A next hop that framed by the other header would see a different message boundary, which is how request smuggling works.
- **Interim `1xx` responses are skipped**, apart from `101`. A `100 Continue` or `103 Early Hints` from the origin is not the answer. Taking it as the answer would leave the real response on the connection for the next request to receive.
- **`Expect: 100-continue` is answered by the proxy.** Mira buffers the whole request body before forwarding it, so the origin's `100` could never reach the client in time.
- **The client connection closes after a response that only a close can end:** a body with no framing, or one the origin cut short. It also closes when the client sent `Connection: close` or spoke HTTP/1.0.
- **Ambiguous framing is rejected with `400`:** `Content-Length` values that disagree or are not plain digits, or a `Transfer-Encoding` whose last coding is not `chunked`. The proxy and the origin could each read a different body length from such a request.

## Responses the Proxy Sends Itself

When a request cannot be forwarded and nothing of a response has reached the
client yet, Mira answers for the origin and closes the connection. The body is
a plain-text line giving the reason. The exchange is still recorded, with its
error.

| Status | When                                                                    |
| ------ | ----------------------------------------------------------------------- |
| `400`  | Malformed head, ambiguous framing, missing `Host`, or a bad port        |
| `403`  | The host filter blocks the host                                         |
| `413`  | The request body exceeds `max_body_size`                                |
| `431`  | The head exceeds `max_header_size`, or has more than 128 header fields  |
| `502`  | The origin could not be reached, its TLS handshake failed, or its response was malformed |
| `504`  | Connecting to or hearing back from the origin timed out                 |

HTTP/2 streams get the same statuses, on the stream rather than the connection.

A passthrough `CONNECT` and a WebSocket upgrade are recorded as one exchange
each, once the tunnel opens or the upgrade is answered. The bytes that follow
are relayed without being captured. Bytes a client sends in the same packet as
its `CONNECT` are passed on to the tunnel, not dropped. For an intercepted
tunnel those bytes are the start of the TLS handshake.

## Data Types

### Request

| Field         | Type                    | Description                          |
| ------------- | ----------------------- | ------------------------------------ |
| `id`          | `u64`                   | Unique identifier                    |
| `client_addr` | `SocketAddr`            | Client's IP and port                 |
| `method`      | `String`                | HTTP method (GET, POST, etc.)        |
| `url`         | `Url`                   | Full URL including scheme            |
| `version`     | `String`                | `HTTP/1.1`, `HTTP/2`, ...            |
| `headers`     | `Vec<(String, String)>` | Request headers                      |
| `body`        | `Bytes`                 | Request body bytes                   |

### Response

| Field              | Type                    | Description                          |
| ------------------ | ----------------------- | ------------------------------------ |
| `request_id`       | `u64`                   | Matches the request                  |
| `status`           | `u16`                   | HTTP status code                     |
| `reason`           | `String`                | Reason phrase (empty on HTTP/2)      |
| `version`          | `String`                | `HTTP/1.1`, `HTTP/2`, ...            |
| `headers`          | `Vec<(String, String)>` | Response headers                     |
| `body`             | `Bytes`                 | Raw response body, as sent           |
| `body_decompressed`| `Bytes`                 | Decoded body (same bytes if not encoded) |
| `encoding`         | `Encoding`              | gzip, deflate, brotli, or none       |
| `resource_type`    | `ResourceType`          | Inferred type (json, html, etc.)     |
| `duration_ms`      | `u64`                   | Total time from request to last byte |
| `timings`          | `Option<Timings>`       | Detailed timing breakdown            |
| `remote_addr`      | `Option<SocketAddr>`    | Address the response came from       |

### Exchange

| Field      | Type               | Description                             |
| ---------- | ------------------ | --------------------------------------- |
| `request`  | `Request`          | The original request                    |
| `response` | `Option<Response>` | The response (None if failed)           |
| `error`    | `Option<String>`   | Error message if request failed         |

## Module Structure

```
mira-proxy/
├── src/
│   ├── lib.rs              # Public API exports
│   ├── config.rs           # Configuration types, host filter, private-IP check
│   ├── config_file.rs      # The TOML config file both front ends share
│   ├── error.rs            # Error types (thiserror)
│   ├── har.rs              # HAR 1.2 import/export format
│   ├── session.rs          # Bounded exchange capture for export
│   │
│   ├── http/
│   │   ├── mod.rs          # HTTP module exports
│   │   ├── body.rs         # Body reading, framing, and streaming relay
│   │   ├── callback.rs     # Request, Response, Exchange, Timings
│   │   ├── compression.rs  # gzip, deflate, brotli decompression
│   │   ├── forwarding.rs   # Via and X-Forwarded-For headers
│   │   ├── head.rs         # Reading a request head off a client stream
│   │   ├── headers.rs      # Comma-separated header list helpers
│   │   ├── hop_by_hop.rs   # Hop-by-hop header filtering (RFC 7230 §6.1)
│   │   ├── request.rs      # HTTP request parsing
│   │   ├── response.rs     # HTTP response parsing
│   │   └── upstream.rs     # Writing a request upstream, reading its head
│   │
│   ├── proxy/
│   │   ├── mod.rs          # Proxy module exports
│   │   ├── server.rs       # Proxy, ProxyBuilder, ProxyState, accept loop
│   │   ├── handler.rs      # Connection handling logic
│   │   ├── http2.rs        # HTTP/2 sessions inside intercepted TLS
│   │   ├── magic.rs        # mira.proxy CA download handler
│   │   ├── pool.rs         # Plain-HTTP connection pooling
│   │   ├── prefixed.rs     # Replays bytes read past a CONNECT head
│   │   └── tunnel.rs       # CONNECT, interception, and WebSocket tunneling
│   │
│   └── tls/
│       ├── mod.rs          # TLS config, ALPN, upstream connectors
│       ├── ca.rs           # Certificate authority
│       └── cert_cache.rs   # Certificate caching

mira-cli/
└── src/
    ├── main.rs             # CLI parsing, proxy wiring, TUI event loop
    ├── logging.rs          # File-based tracing subscriber
    └── tui/
        ├── app.rs          # TUI state (rows, filter, focus, tabs)
        └── ui.rs           # Rendering

mira-app/
└── src/
    ├── main.rs             # Settings, proxy thread, actions, key bindings, menus, window
    ├── proxy.rs            # Runs and restarts the proxy on its own tokio runtime
    ├── capture.rs          # Captured exchanges, folded in from the proxy's channel; pausing
    ├── state.rs            # Navigation, selection, and the status line
    ├── workspace.rs        # The window's root view and its commands
    ├── top_bar.rs          # Title bar: brand, listening address, capture commands
    ├── sidebar.rs          # Search field, pages, hosts
    ├── status_bar.rs       # Hover hints, messages, totals, HTTPS mode, recording state
    ├── overview.rs         # Status codes, busiest hosts, slowest requests, failures
    ├── network.rs          # The request table and waterfall
    ├── hosts.rs            # Every host, sortable
    ├── search.rs           # Background search through URLs, headers, bodies
    ├── body.rs             # One body as text, pretty JSON, or hex
    ├── inspector.rs        # The selected exchange's details
    ├── settings.rs         # The config file as a form; applying restarts the proxy
    ├── logging.rs          # Daily log files, panics included
    ├── curl.rs             # A request as the curl command that repeats it
    ├── ui.rs / input.rs    # Shared controls over GPUI Base's unstyled ones
    ├── theme.rs            # Palette, type scale, fonts, site colors
    ├── icons.rs            # Icons and the asset source serving them
    └── site.rs, format.rs, text.rs  # URLs and sites, number formats, safe text
```

The desktop app is drawn with GPUI and GPUI Base's unstyled controls
(`gpui-kit` without its default features), styled by the app itself.
The proxy runs on its own thread and tokio runtime; what it captures crosses
to the window as messages over a channel and is folded into the capture on
the main thread, a batch at a time. The window, not the proxy, adds each
exchange it keeps to the session, so a paused stretch of traffic stays out of
the HAR export as well as the list.

Applying new settings starts a new proxy and lets the old one finish its
exchanges in the background. Each run numbers its requests from 1, so the
window renumbers them from one counter that spans every run.
