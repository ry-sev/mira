//! Proxy server implementation.

use crate::Result;
use crate::config::Config;
use crate::http::Callbacks;
use crate::http::Exchange;
use crate::http::ExchangeHandler;
use crate::http::Request;
use crate::http::RequestHandler;
use crate::http::Response;
use crate::http::ResponseHandler;
use crate::proxy::handler::handle_connection;
use crate::proxy::pool::ConnectionPool;
use crate::session::Session;
use crate::tls::CertificateAuthority;
use crate::tls::TlsConfig;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tracing::error;
use tracing::info;
use tracing::warn;

/// How long `run_until` waits for in-flight connections to finish after a
/// graceful shutdown signal before returning. Tasks still alive past this
/// will be aborted when the tokio runtime is torn down.
const GRACEFUL_SHUTDOWN_DRAIN: Duration = Duration::from_secs(10);

/// Shared state for the proxy, accessible by connection handlers.
#[derive(Debug)]
pub struct ProxyState {
    /// Proxy configuration
    pub config: Config,

    /// Request ID counter
    pub request_counter: AtomicU64,

    /// Registered callbacks
    pub callbacks: Callbacks,

    /// Connection pool for upstream servers
    pub connection_pool: ConnectionPool,

    /// Optional session for capturing exchanges
    pub session: Option<Session>,

    /// TLS configuration for HTTPS interception (None = passthrough only)
    pub tls_config: Option<TlsConfig>,
}

impl ProxyState {
    /// Generate a new unique request ID.
    pub fn next_request_id(&self) -> u64 {
        self.request_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Hand a finished exchange to the session and the `on_exchange`
    /// callback.
    pub(crate) async fn record(&self, exchange: Exchange) {
        if let Some(session) = &self.session {
            session.add(exchange.clone());
        }
        if let Some(callback) = &self.callbacks.on_exchange {
            callback.invoke(exchange).await;
        }
    }
}

/// The main proxy server.
pub struct Proxy {
    config: Config,
    callbacks: Callbacks,
    session: Option<Session>,
    tls_config: Option<TlsConfig>,
}

impl Proxy {
    /// Create a new proxy with the given configuration.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            callbacks: Callbacks::default(),
            session: None,
            tls_config: None,
        }
    }

    /// Create a new `ProxyBuilder` for configuring the proxy.
    pub fn builder(config: Config) -> ProxyBuilder {
        ProxyBuilder::new(config)
    }

    /// Run the proxy server.
    ///
    /// This will bind to the configured address and start accepting
    /// connections. Returns when the server encounters a fatal error,
    /// or never under normal operation. For a graceful shutdown story
    /// (stop accepting, drain in-flight handlers so their `on_exchange`
    /// callbacks fire) use [`Proxy::run_until`] with an explicit
    /// shutdown future.
    pub async fn run(&self) -> Result<()> {
        self.run_until(std::future::pending::<()>()).await
    }

    /// Run the proxy until `shutdown` resolves, then drain in-flight
    /// handlers and return.
    ///
    /// When the shutdown future fires:
    /// 1. `accept()` is not polled again — new TCP connections pile up in the
    ///    kernel SYN queue rather than being served.
    /// 2. Already-running handlers continue to completion so their `on_request`
    ///    / `on_response` / `on_exchange` callbacks still fire and their
    ///    exchanges land in the session.
    /// 3. If handlers don't finish within a bounded grace period the wait is
    ///    aborted and we return anyway; remaining tasks will be cancelled when
    ///    the tokio runtime tears down.
    pub async fn run_until<S>(&self, shutdown: S) -> Result<()>
    where
        S: Future<Output = ()>,
    {
        let listener = self.bind().await?;
        self.serve_until(listener, shutdown).await
    }

    /// Bind the configured listen address without serving on it yet.
    ///
    /// Lets a caller report a bind failure (port in use, permission
    /// denied) before it commits to anything else, then hand the
    /// listener to [`Proxy::serve_until`].
    pub async fn bind(&self) -> Result<TcpListener> {
        TcpListener::bind(self.config.bind_addr)
            .await
            .map_err(|e| crate::Error::Bind {
                addr: self.config.bind_addr,
                source: e,
            })
    }

    /// Serve on an already-bound `listener` until `shutdown` resolves.
    ///
    /// Shutdown behaves as described for [`Proxy::run_until`].
    pub async fn serve_until<S>(&self, listener: TcpListener, shutdown: S) -> Result<()>
    where
        S: Future<Output = ()>,
    {
        let local_addr = listener.local_addr()?;
        info!(addr = %local_addr, "proxy listening");

        let state = Arc::new(ProxyState {
            config: self.config.clone(),
            request_counter: AtomicU64::new(1),
            callbacks: self.callbacks.clone(),
            connection_pool: ConnectionPool::new(),
            session: self.session.clone(),
            tls_config: self.tls_config.clone(),
        });

        let connection_limit = Arc::new(Semaphore::new(state.config.max_connections));
        tokio::pin!(shutdown);

        // Accept loop — terminates when `shutdown` resolves.
        loop {
            // `biased` so that, when both the shutdown signal and a
            // permit+accept pair are ready, shutdown wins and we don't
            // spawn a new task after being told to stop.
            tokio::select! {
                biased;
                () = &mut shutdown => {
                    info!("graceful shutdown requested");
                    break;
                }
                permit = Arc::clone(&connection_limit).acquire_owned() => {
                    let permit = permit.expect("connection-limit semaphore is never closed");
                    tokio::select! {
                        biased;
                        () = &mut shutdown => {
                            drop(permit);
                            info!("graceful shutdown requested");
                            break;
                        }
                        accept_result = listener.accept() => {
                            match accept_result {
                                Ok((stream, addr)) => {
                                    let state = Arc::clone(&state);
                                    tokio::spawn(async move {
                                        handle_connection(stream, addr, state).await;
                                        // Permit drops here, freeing a slot.
                                        drop(permit);
                                    });
                                }
                                Err(e) => {
                                    error!(error = %e, "failed to accept connection");
                                    drop(permit);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Free the port before draining, so a new proxy can bind it at once.
        drop(listener);

        // Drain: wait (bounded) for in-flight handlers to finish so their
        // captured exchanges land in the session before we return.
        let total_permits = u32::try_from(state.config.max_connections).unwrap_or(u32::MAX);
        match tokio::time::timeout(
            GRACEFUL_SHUTDOWN_DRAIN,
            connection_limit.acquire_many(total_permits),
        )
        .await
        {
            Ok(Ok(_permits)) => {
                info!("all in-flight connections drained, shutdown complete");
            }
            Ok(Err(_closed)) => {
                // Semaphore is never explicitly closed, but handle it
                // gracefully if the invariant is ever broken.
                warn!("connection-limit semaphore closed unexpectedly during drain");
            }
            Err(_elapsed) => {
                warn!(
                    drain_timeout_secs = GRACEFUL_SHUTDOWN_DRAIN.as_secs(),
                    "timed out draining in-flight connections; some exchanges may not be captured"
                );
            }
        }

        Ok(())
    }

    /// Get the configured bind address.
    pub fn bind_addr(&self) -> SocketAddr {
        self.config.bind_addr
    }

    /// Get the session if one was configured.
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }
}

/// Builder for configuring a Proxy with callbacks.
pub struct ProxyBuilder {
    config: Config,
    callbacks: Callbacks,
    session: Option<Session>,
    tls_config: Option<TlsConfig>,
}

impl ProxyBuilder {
    /// Create a new `ProxyBuilder` with the given configuration.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            callbacks: Callbacks::default(),
            session: None,
            tls_config: None,
        }
    }

    // ========================================================================
    // on_request callbacks
    // ========================================================================

    /// Set a synchronous callback for incoming requests.
    ///
    /// This callback is invoked when a request is received, before it is
    /// forwarded to the upstream server.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let proxy = Proxy::builder(config)
    ///     .on_request(|req| {
    ///         println!("{} {}", req.method, req.url);
    ///     })
    ///     .build();
    /// ```
    #[must_use]
    pub fn on_request<F>(mut self, callback: F) -> Self
    where
        F: Fn(Request) + Send + Sync + 'static,
    {
        self.callbacks.on_request = Some(RequestHandler::Sync(Arc::new(callback)));
        self
    }

    /// Set an asynchronous callback for incoming requests.
    #[must_use]
    pub fn on_request_async<F, Fut>(mut self, callback: F) -> Self
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.callbacks.on_request = Some(RequestHandler::Async(Arc::new(move |request| {
            Box::pin(callback(request))
        })));
        self
    }

    // ========================================================================
    // on_response callbacks
    // ========================================================================

    /// Set a synchronous callback for incoming responses.
    ///
    /// This callback is invoked when a response is received from the upstream
    /// server, before it is sent to the client.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let proxy = Proxy::builder(config)
    ///     .on_response(|resp| {
    ///         println!("{} {} ({}ms)", resp.status, resp.reason, resp.duration_ms);
    ///     })
    ///     .build();
    /// ```
    #[must_use]
    pub fn on_response<F>(mut self, callback: F) -> Self
    where
        F: Fn(Response) + Send + Sync + 'static,
    {
        self.callbacks.on_response = Some(ResponseHandler::Sync(Arc::new(callback)));
        self
    }

    /// Set an asynchronous callback for incoming responses.
    #[must_use]
    pub fn on_response_async<F, Fut>(mut self, callback: F) -> Self
    where
        F: Fn(Response) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.callbacks.on_response = Some(ResponseHandler::Async(Arc::new(move |response| {
            Box::pin(callback(response))
        })));
        self
    }

    // ========================================================================
    // on_exchange callbacks
    // ========================================================================

    /// Set a synchronous callback for completed exchanges.
    ///
    /// This callback is invoked after each HTTP request-response cycle
    /// completes. The callback receives the full request and response data.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let proxy = Proxy::builder(config)
    ///     .on_exchange(|exchange| {
    ///         println!("{} {} -> {}",
    ///             exchange.request.method,
    ///             exchange.request.url,
    ///             exchange.response.map(|r| r.status).unwrap_or(0)
    ///         );
    ///     })
    ///     .build();
    /// ```
    #[must_use]
    pub fn on_exchange<F>(mut self, callback: F) -> Self
    where
        F: Fn(Exchange) + Send + Sync + 'static,
    {
        self.callbacks.on_exchange = Some(ExchangeHandler::Sync(Arc::new(callback)));
        self
    }

    /// Set an asynchronous callback for completed exchanges.
    ///
    /// This is useful when the callback needs to perform async operations
    /// like writing to a database or sending to a channel.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let proxy = Proxy::builder(config)
    ///     .on_exchange_async(|exchange| async move {
    ///         // async operations here
    ///     })
    ///     .build();
    /// ```
    #[must_use]
    pub fn on_exchange_async<F, Fut>(mut self, callback: F) -> Self
    where
        F: Fn(Exchange) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.callbacks.on_exchange = Some(ExchangeHandler::Async(Arc::new(move |exchange| {
            Box::pin(callback(exchange))
        })));
        self
    }

    // ========================================================================
    // Session capture
    // ========================================================================

    /// Enable session capture to record all exchanges.
    ///
    /// When enabled, all HTTP exchanges are automatically captured and can be
    /// exported to HAR format later. The session is shared and can be accessed
    /// via `session()` after calling `build()`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let session = Session::new();
    /// let proxy = Proxy::builder(config)
    ///     .with_session(session.clone())
    ///     .build();
    ///
    /// // After proxy runs, export to HAR:
    /// let har_json = session.to_har_json_pretty()?;
    /// ```
    #[must_use]
    pub fn with_session(mut self, session: Session) -> Self {
        self.session = Some(session);
        self
    }

    /// Enable session capture with a new session.
    ///
    /// Creates a new session and returns it along with the builder.
    /// This is a convenience method when you don't need to pre-create the
    /// session.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (session, builder) = Proxy::builder(config).capture_session();
    /// let proxy = builder.build();
    ///
    /// // After proxy runs, export to HAR:
    /// let har_json = session.to_har_json_pretty()?;
    /// ```
    #[must_use]
    pub fn capture_session(mut self) -> (Session, Self) {
        let session = Session::with_limits(self.config.session_limits);
        self.session = Some(session.clone());
        (session, self)
    }

    // ========================================================================
    // TLS configuration
    // ========================================================================

    /// Enable HTTPS interception with the given TLS configuration.
    ///
    /// When TLS config is set, the proxy will perform TLS termination on
    /// CONNECT requests, allowing inspection of HTTPS traffic.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let ca = CertificateAuthority::load_or_generate()?;
    /// let tls_config = TlsConfig::new(ca);
    /// let proxy = Proxy::builder(config)
    ///     .with_tls(tls_config)
    ///     .build();
    /// ```
    #[must_use]
    pub fn with_tls(mut self, tls_config: TlsConfig) -> Self {
        self.tls_config = Some(tls_config);
        self
    }

    /// Enable HTTPS interception with auto-generated/loaded CA.
    ///
    /// This is a convenience method that loads the CA from the default
    /// location (~/.mira/) or generates a new one if it doesn't exist.
    ///
    /// Returns an error if CA generation/loading fails.
    pub fn with_https_interception(mut self) -> Result<Self> {
        let ca_dir = self
            .config
            .https
            .ca_dir
            .clone()
            .or_else(|| CertificateAuthority::default_ca_dir().ok());

        let ca = if let Some(dir) = ca_dir {
            CertificateAuthority::load_or_generate_in(&dir)?
        } else {
            CertificateAuthority::generate()?
        };

        self.config.https.intercept = true;
        self.tls_config = Some(TlsConfig::new(ca));
        Ok(self)
    }

    /// Build the Proxy with the configured callbacks.
    pub fn build(self) -> Proxy {
        Proxy {
            config: self.config,
            callbacks: self.callbacks,
            session: self.session,
            tls_config: self.tls_config,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use tokio::sync::oneshot;

    fn ephemeral_config() -> Config {
        Config::new(0).with_bind_addr(SocketAddr::new(
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
        ))
    }

    #[tokio::test]
    async fn test_run_until_returns_on_shutdown_signal() {
        // Without the shutdown future, `run` would loop forever. With
        // it, the accept loop exits promptly and the drain completes
        // (no in-flight handlers to wait on), so `run_until` returns.
        let proxy = Proxy::new(ephemeral_config());
        let (tx, rx) = oneshot::channel::<()>();

        let handle = tokio::spawn(async move {
            proxy
                .run_until(async move {
                    let _ = rx.await;
                })
                .await
        });

        // Let the listener bind before signaling shutdown.
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(()).expect("send shutdown");

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("run_until should return within the bound")
            .expect("task did not panic")
            .expect("proxy returns Ok on graceful shutdown");
    }
}
