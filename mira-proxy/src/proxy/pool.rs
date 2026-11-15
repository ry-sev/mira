//! Connection pool for upstream HTTP connections.
//!
//! Provides HTTP/1.1 keep-alive connection reuse to reduce latency
//! and resource usage when making multiple requests to the same host.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::net::TcpStream;
use tracing::debug;
use tracing::trace;

/// Default maximum idle time for pooled connections (60 seconds).
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Default maximum connections per host.
const DEFAULT_MAX_CONNECTIONS_PER_HOST: usize = 4;

/// A pooled TCP connection.
#[derive(Debug)]
struct PooledConnection {
    stream: TcpStream,
    created_at: Instant,
    last_used: Instant,
}

impl PooledConnection {
    fn new(stream: TcpStream) -> Self {
        let now = Instant::now();
        Self {
            stream,
            created_at: now,
            last_used: now,
        }
    }

    fn is_expired(&self, idle_timeout: Duration) -> bool {
        self.last_used.elapsed() > idle_timeout
    }
}

/// Configuration for the connection pool.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Maximum idle time before a connection is closed.
    pub idle_timeout: Duration,

    /// Maximum number of connections per host.
    pub max_connections_per_host: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            max_connections_per_host: DEFAULT_MAX_CONNECTIONS_PER_HOST,
        }
    }
}

/// A connection pool for reusing TCP connections to upstream servers.
#[derive(Debug)]
pub struct ConnectionPool {
    /// Pooled connections keyed by host:port
    connections: Arc<Mutex<HashMap<String, Vec<PooledConnection>>>>,

    /// Pool configuration
    config: PoolConfig,
}

impl ConnectionPool {
    /// Create a new connection pool with default configuration.
    pub fn new() -> Self {
        Self::with_config(PoolConfig::default())
    }

    /// Create a new connection pool with the given configuration.
    pub fn with_config(config: PoolConfig) -> Self {
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            config,
        }
    }

    /// Try to get an existing connection from the pool.
    ///
    /// Returns `None` if no suitable connection is available.
    pub fn get(&self, host: &str, port: u16) -> Option<TcpStream> {
        let key = format!("{host}:{port}");
        let mut connections = self.connections.lock();

        let pool = connections.get_mut(&key)?;

        // Remove expired connections and find a valid one
        while let Some(mut conn) = pool.pop() {
            if conn.is_expired(self.config.idle_timeout) {
                trace!(key, "dropping expired connection");
                continue;
            }

            conn.last_used = Instant::now();
            debug!(
                key,
                age_ms = conn.created_at.elapsed().as_millis(),
                "reusing pooled connection"
            );
            return Some(conn.stream);
        }

        None
    }

    /// Return a connection to the pool for reuse.
    ///
    /// The connection MUST still be valid (not closed by the server).
    pub fn put(&self, host: &str, port: u16, stream: TcpStream) {
        let key = format!("{host}:{port}");
        let mut connections = self.connections.lock();

        let pool = connections.entry(key.clone()).or_default();

        // Don't exceed max connections per host
        if pool.len() >= self.config.max_connections_per_host {
            trace!(key, "pool full, dropping connection");
            return;
        }

        pool.retain(|c| !c.is_expired(self.config.idle_timeout));

        trace!(
            key,
            pool_size = pool.len() + 1,
            "returning connection to pool"
        );
        pool.push(PooledConnection::new(stream));
    }

    /// Remove all expired connections from the pool.
    pub fn cleanup(&self) {
        let mut connections = self.connections.lock();
        let idle_timeout = self.config.idle_timeout;

        connections.retain(|key, pool| {
            pool.retain(|c| !c.is_expired(idle_timeout));
            if pool.is_empty() {
                trace!(key, "removing empty pool");
                false
            } else {
                true
            }
        });
    }

    /// Get the total number of pooled connections.
    pub fn len(&self) -> usize {
        self.connections.lock().values().map(Vec::len).sum()
    }

    /// Check if the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for ConnectionPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pool_get_returns_none_when_empty() {
        let pool = ConnectionPool::new();
        assert!(pool.get("example.com", 80).is_none());
    }
}
