//! Certificate cache for generated per-host certificates.

use dashmap::DashMap;
use tokio_rustls::TlsAcceptor;

/// Most acceptors the cache holds (1024).
///
/// Each holds a certificate and its key. A long session that visits
/// thousands of hosts would otherwise keep every one of them.
pub const MAX_CACHED_ACCEPTORS: usize = 1024;

/// Cache of TLS acceptors for generated certificates.
///
/// This avoids regenerating certificates for hosts we've already seen.
/// The cache is thread-safe and can be shared across async tasks. Once
/// it holds [`MAX_CACHED_ACCEPTORS`], adding a host evicts another; an
/// evicted host just has its certificate generated again.
pub struct CertificateCache {
    /// Map from hostname to TLS acceptor.
    cache: DashMap<String, TlsAcceptor>,
}

impl CertificateCache {
    /// Create a new empty certificate cache.
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
        }
    }

    /// Get a cached TLS acceptor for the given host.
    pub fn get(&self, host: &str) -> Option<TlsAcceptor> {
        // Normalize the host (strip port if present, lowercase)
        let normalized = normalize_host(host);
        self.cache.get(&normalized).map(|r| r.clone())
    }

    /// Insert a TLS acceptor for the given host.
    pub fn insert(&self, host: &str, acceptor: TlsAcceptor) {
        let normalized = normalize_host(host);
        if self.cache.len() >= MAX_CACHED_ACCEPTORS && !self.cache.contains_key(&normalized) {
            // Any entry will do: regenerating one costs a key pair.
            let victim = self.cache.iter().next().map(|entry| entry.key().clone());
            if let Some(victim) = victim {
                self.cache.remove(&victim);
            }
        }
        self.cache.insert(normalized, acceptor);
    }

    /// Remove a cached entry.
    pub fn remove(&self, host: &str) -> Option<TlsAcceptor> {
        let normalized = normalize_host(host);
        self.cache.remove(&normalized).map(|(_, v)| v)
    }

    /// Clear all cached certificates.
    pub fn clear(&self) {
        self.cache.clear();
    }

    /// Get the number of cached certificates.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

impl Default for CertificateCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize a host for cache lookup.
///
/// Strips the port if present and converts to lowercase.
fn normalize_host(host: &str) -> String {
    // Strip port if present
    let host = if let Some((h, _)) = host.rsplit_once(':') {
        // Check if this is actually a port (numeric) or part of IPv6
        if host.starts_with('[') {
            // IPv6 address like [::1]:8080
            if let Some((h, _)) = host.rsplit_once("]:") {
                &h[1..] // Remove leading [
            } else {
                host.trim_start_matches('[').trim_end_matches(']')
            }
        } else if h.contains(':') {
            // IPv6 without brackets, no port
            host
        } else {
            h
        }
    } else {
        host
    };

    host.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_host() {
        assert_eq!(normalize_host("example.com"), "example.com");
        assert_eq!(normalize_host("Example.COM"), "example.com");
        assert_eq!(normalize_host("example.com:443"), "example.com");
        assert_eq!(normalize_host("Example.COM:8080"), "example.com");
    }

    #[test]
    fn test_cache_is_bounded() {
        let tls = crate::tls::TlsConfig::new(
            crate::tls::CertificateAuthority::generate().expect("generate CA"),
        );
        let acceptor = tls.acceptor_for_host("example.com").expect("acceptor");

        let cache = CertificateCache::new();
        for n in 0..MAX_CACHED_ACCEPTORS + 10 {
            cache.insert(&format!("host{n}.example"), acceptor.clone());
        }
        assert_eq!(cache.len(), MAX_CACHED_ACCEPTORS);
        assert!(
            cache
                .get(&format!("host{}.example", MAX_CACHED_ACCEPTORS + 9))
                .is_some(),
            "the newest entry is kept"
        );
    }

    #[test]
    fn test_normalize_ipv6() {
        assert_eq!(normalize_host("[::1]:8080"), "::1");
        assert_eq!(normalize_host("[::1]"), "::1");
    }
}
