//! Configuration for the Mira proxy.

use crate::session::SessionLimits;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Default port.
pub const DEFAULT_PORT: u16 = 8888;

/// Default bind address (localhost).
pub const DEFAULT_BIND: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Default connect timeout (30 seconds).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default read timeout (60 seconds).
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Default write timeout (60 seconds).
pub const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// Default max body size (500 MB).
pub const DEFAULT_MAX_BODY_SIZE: usize = 500 * 1024 * 1024;

/// Default maximum request/response header size (64 KB).
///
/// Generous enough for real traffic (browsers with large cookie jars,
/// long `Authorization` headers) while keeping a client that never sends
/// the header terminator from growing a buffer for the whole read
/// timeout, once per connection.
pub const DEFAULT_MAX_HEADER_SIZE: usize = 64 * 1024;

/// Default cap on idle upstream connections kept per HTTP/2 session (4).
///
/// HTTP/2 multiplexes, so several streams can want an upstream
/// connection at the same moment. Keeping a few idle avoids a TLS
/// handshake per stream without letting one session hoard sockets.
pub const DEFAULT_MAX_IDLE_UPSTREAMS_PER_SESSION: usize = 4;

/// Default maximum number of concurrent client connections.
///
/// The proxy won't `accept()` a new connection once this many are already
/// being handled, so upstream load is bounded and the kernel SYN queue
/// provides backpressure to clients.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;

/// Check if an IP address is a private/local address.
///
/// Private addresses include:
/// - Loopback (127.0.0.0/8, `::1`)
/// - Private networks (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16)
/// - Shared address space / CGNAT (100.64.0.0/10, RFC 6598) — used by ISP /
///   cloud / mobile carrier-grade NATs, not publicly routable
/// - Link-local (169.254.0.0/16, `fe80::/10`)
/// - Unique local (`fc00::/7`)
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_private_ipv4(ipv4),
        IpAddr::V6(ipv6) => is_private_ipv6(ipv6),
    }
}

/// Check if an IPv4 address is private/local.
fn is_private_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();

    // Loopback: 127.0.0.0/8
    if octets[0] == 127 {
        return true;
    }

    // Private Class A: 10.0.0.0/8
    if octets[0] == 10 {
        return true;
    }

    // Private Class B: 172.16.0.0/12 (172.16.0.0 - 172.31.255.255)
    if octets[0] == 172 && (16..=31).contains(&octets[1]) {
        return true;
    }

    // Private Class C: 192.168.0.0/16
    if octets[0] == 192 && octets[1] == 168 {
        return true;
    }

    // Shared address space (RFC 6598): 100.64.0.0/10. Used by carrier-
    // grade NATs (ISPs, mobile, some cloud providers); not publicly
    // routable and MUST be treated the same as RFC 1918 private space.
    if octets[0] == 100 && (64..=127).contains(&octets[1]) {
        return true;
    }

    // Link-local: 169.254.0.0/16
    if octets[0] == 169 && octets[1] == 254 {
        return true;
    }

    false
}

/// Check if an IPv6 address is private/local.
fn is_private_ipv6(ip: Ipv6Addr) -> bool {
    // Loopback: ::1
    if ip.is_loopback() {
        return true;
    }

    let segments = ip.segments();

    // Link-local: fe80::/10
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }

    // Unique local: fc00::/7 (fc00:: - fdff::)
    if (segments[0] & 0xfe00) == 0xfc00 {
        return true;
    }

    // IPv4-mapped IPv6 addresses (::ffff:x.x.x.x)
    if let Some(ipv4) = ip.to_ipv4_mapped() {
        return is_private_ipv4(ipv4);
    }

    false
}

/// Proxy configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address to bind the proxy listener to.
    pub bind_addr: SocketAddr,

    /// Allow connections from external (public) IP addresses.
    ///
    /// By default, only connections from private/local IP addresses are
    /// accepted. Enable this to allow connections from the public internet.
    pub allow_external: bool,

    /// Timeout for connecting to upstream servers.
    pub connect_timeout: Duration,

    /// Timeout for reading from upstream servers.
    pub read_timeout: Duration,

    /// Timeout for each write to an upstream server or to a client.
    ///
    /// A peer that stops reading would otherwise hold its connection, and
    /// the proxy's connection slot, indefinitely.
    pub write_timeout: Duration,

    /// Maximum body size (bytes).
    ///
    /// A request body over this is rejected. A response body over it is
    /// still relayed to the client in full, but only this many bytes of it
    /// are captured.
    pub max_body_size: usize,

    /// Maximum size of a request or response header block (bytes).
    ///
    /// Headers that grow past this without terminating are rejected
    /// rather than buffered indefinitely.
    pub max_header_size: usize,

    /// Maximum idle upstream connections kept per intercepted HTTP/2
    /// session.
    pub max_idle_upstreams_per_session: usize,

    /// Maximum number of concurrent client connections.
    ///
    /// Once this limit is reached, the accept loop pauses until an
    /// in-flight connection finishes, providing backpressure at the
    /// kernel accept queue rather than spawning unboundedly.
    pub max_connections: usize,

    /// Bounds on what a capturing session retains.
    ///
    /// Applies to sessions the proxy creates itself (see
    /// `ProxyBuilder::capture_session`); a session supplied by the caller
    /// keeps whatever limits it was built with.
    pub session_limits: SessionLimits,

    /// Host filter for include/exclude patterns.
    pub host_filter: Option<HostFilter>,

    /// HTTPS interception configuration.
    pub https: HttpsConfig,
}

/// Configuration for HTTPS interception.
#[derive(Debug, Clone, Default)]
pub struct HttpsConfig {
    /// Whether to intercept HTTPS traffic.
    ///
    /// When enabled, the proxy will perform TLS termination and re-encryption
    /// to inspect HTTPS traffic. Requires trusting the proxy's CA certificate.
    ///
    /// When disabled (default), HTTPS traffic is tunneled through without
    /// inspection.
    pub intercept: bool,

    /// Custom CA certificate directory.
    ///
    /// If not specified, defaults to `~/.mira/`.
    pub ca_dir: Option<PathBuf>,

    /// Hosts to always pass through without interception.
    ///
    /// This is useful for hosts with certificate pinning that would reject
    /// the proxy's generated certificates. These hosts will be tunneled
    /// even when interception is enabled.
    ///
    /// Supports the same patterns as `HostFilter`:
    /// - Exact match: `example.com`
    /// - Wildcard prefix: `*.example.com` (subdomains only, not the apex — add
    ///   both patterns for full coverage)
    pub passthrough_hosts: Vec<String>,
}

impl HttpsConfig {
    /// Create a new HTTPS config with interception enabled.
    pub fn intercept() -> Self {
        Self {
            intercept: true,
            ca_dir: None,
            passthrough_hosts: Vec::new(),
        }
    }

    /// Check if a host should be passed through without interception.
    pub fn should_passthrough(&self, host: &str) -> bool {
        let host = strip_port(host);

        for pattern in &self.passthrough_hosts {
            if HostFilter::pattern_matches(pattern, host) {
                return true;
            }
        }

        false
    }
}

impl Config {
    /// Create a new configuration with the specified port.
    pub fn new(port: u16) -> Self {
        Self {
            bind_addr: SocketAddr::new(DEFAULT_BIND, port),
            allow_external: false,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            max_body_size: DEFAULT_MAX_BODY_SIZE,
            max_header_size: DEFAULT_MAX_HEADER_SIZE,
            max_idle_upstreams_per_session: DEFAULT_MAX_IDLE_UPSTREAMS_PER_SESSION,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            session_limits: SessionLimits::default(),
            host_filter: None,
            https: HttpsConfig::default(),
        }
    }

    /// Set the bind address.
    #[must_use]
    pub fn with_bind_addr(mut self, addr: SocketAddr) -> Self {
        self.bind_addr = addr;
        self
    }

    /// Allow or block connections from external (public) IP addresses.
    ///
    /// By default, only connections from private/local IP addresses are
    /// accepted. Enable this to allow connections from the public internet.
    #[must_use]
    pub fn with_allow_external(mut self, allow: bool) -> Self {
        self.allow_external = allow;
        self
    }

    /// Set the connect timeout.
    #[must_use]
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Set the read timeout.
    #[must_use]
    pub fn with_read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
    }

    /// Set the write timeout.
    #[must_use]
    pub fn with_write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = timeout;
        self
    }

    /// Set all timeouts to the same value.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self.read_timeout = timeout;
        self.write_timeout = timeout;
        self
    }

    /// Set the maximum body size. See [`Config::max_body_size`].
    #[must_use]
    pub fn with_max_body_size(mut self, size: usize) -> Self {
        self.max_body_size = size;
        self
    }

    /// Set the bounds on what a capturing session retains.
    #[must_use]
    pub fn with_session_limits(mut self, limits: SessionLimits) -> Self {
        self.session_limits = limits;
        self
    }

    /// Set the maximum request/response header block size.
    ///
    /// Must be at least 1 byte; zero would reject every request, so it is
    /// silently clamped up.
    #[must_use]
    pub fn with_max_header_size(mut self, size: usize) -> Self {
        self.max_header_size = size.max(1);
        self
    }

    /// Set the maximum number of concurrent client connections.
    ///
    /// Must be at least 1. A value of zero would block every accept
    /// forever, so it is silently clamped up to 1.
    #[must_use]
    pub fn with_max_connections(mut self, max: usize) -> Self {
        self.max_connections = max.max(1);
        self
    }

    /// Set the host filter.
    #[must_use]
    pub fn with_host_filter(mut self, filter: HostFilter) -> Self {
        self.host_filter = Some(filter);
        self
    }

    /// Enable or disable HTTPS interception.
    #[must_use]
    pub fn with_https_intercept(mut self, intercept: bool) -> Self {
        self.https.intercept = intercept;
        self
    }

    /// Set a custom CA directory for HTTPS interception.
    #[must_use]
    pub fn with_ca_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.https.ca_dir = Some(dir.into());
        self
    }

    /// Add a host pattern to always pass through without interception.
    #[must_use]
    pub fn with_passthrough_host(mut self, pattern: impl Into<String>) -> Self {
        self.https.passthrough_hosts.push(pattern.into());
        self
    }

    /// Add multiple host patterns to always pass through without interception.
    #[must_use]
    pub fn with_passthrough_hosts(
        mut self,
        patterns: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.https
            .passthrough_hosts
            .extend(patterns.into_iter().map(Into::into));
        self
    }

    /// Set the full HTTPS configuration.
    #[must_use]
    pub fn with_https(mut self, https: HttpsConfig) -> Self {
        self.https = https;
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new(DEFAULT_PORT)
    }
}

/// Host filtering configuration.
#[derive(Debug, Clone)]
pub struct HostFilter {
    /// Patterns to include (if empty, all hosts are included).
    pub include: Vec<String>,

    /// Patterns to exclude (takes precedence over include).
    pub exclude: Vec<String>,
}

impl HostFilter {
    /// Create a new empty host filter (allows all hosts).
    pub fn new() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
        }
    }

    /// Add an include pattern.
    ///
    /// Patterns support:
    /// - Exact match: `example.com`
    /// - Wildcard prefix: `*.example.com` (matches any subdomain but NOT the
    ///   apex `example.com` — list both patterns to cover apex plus subdomains)
    /// - Wildcard suffix: `example.*` (matches any TLD)
    #[must_use]
    pub fn include(mut self, pattern: impl Into<String>) -> Self {
        self.include.push(pattern.into());
        self
    }

    /// Add multiple include patterns.
    #[must_use]
    pub fn include_many(mut self, patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.include.extend(patterns.into_iter().map(Into::into));
        self
    }

    /// Add an exclude pattern.
    #[must_use]
    pub fn exclude(mut self, pattern: impl Into<String>) -> Self {
        self.exclude.push(pattern.into());
        self
    }

    /// Add multiple exclude patterns.
    #[must_use]
    pub fn exclude_many(mut self, patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.exclude.extend(patterns.into_iter().map(Into::into));
        self
    }

    /// Check if a host matches this filter.
    pub fn matches(&self, host: &str) -> bool {
        let host = strip_port(host);

        // Check excludes first (takes precedence)
        for pattern in &self.exclude {
            if Self::pattern_matches(pattern, host) {
                return false;
            }
        }

        // If no includes specified, allow all non-excluded hosts
        if self.include.is_empty() {
            return true;
        }

        for pattern in &self.include {
            if Self::pattern_matches(pattern, host) {
                return true;
            }
        }

        false
    }

    /// Check if a pattern matches a host.
    ///
    /// Wildcard semantics follow TLS/X.509 (RFC 6125 §6.4.3): a
    /// `*.example.com` pattern matches only subdomains, **not** the apex
    /// `example.com`. To include both, list both patterns:
    /// `example.com` and `*.example.com`.
    ///
    /// Host names are case-insensitive, so matching is too.
    pub(crate) fn pattern_matches(pattern: &str, host: &str) -> bool {
        let pattern = pattern.to_ascii_lowercase();
        let host = host.to_ascii_lowercase();
        let (pattern, host) = (pattern.as_str(), host.as_str());

        if pattern.starts_with("*.") {
            // Wildcard prefix: *.example.com matches foo.example.com
            // but NOT bare example.com.
            let suffix = &pattern[1..]; // .example.com
            host.ends_with(suffix)
        } else if pattern.ends_with(".*") {
            // Wildcard suffix: example.* matches example.com, example.org
            let prefix = &pattern[..pattern.len() - 1]; // example.
            host.starts_with(prefix)
        } else if pattern.contains('*') {
            let parts: Vec<&str> = pattern.split('*').collect();
            if parts.len() == 2 {
                // The prefix and suffix MUST NOT overlap: `a*a` is not
                // satisfied by the single `a` of the host `a`.
                host.len() >= parts[0].len() + parts[1].len()
                    && host.starts_with(parts[0])
                    && host.ends_with(parts[1])
            } else {
                pattern == host
            }
        } else {
            pattern == host
        }
    }
}

/// The host part of `host` or `host:port`.
///
/// A bracketed IPv6 literal (`[::1]:8080`) loses its brackets and port; a
/// bare one (`::1`) is left whole, since its colons are not a port.
fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split_once(']').map_or(rest, |(address, _)| address);
    }
    match host.rsplit_once(':') {
        Some((name, _port)) if !name.contains(':') => name,
        _ => host,
    }
}

impl Default for HostFilter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_filter_exact_match() {
        let filter = HostFilter::new().include("example.com");
        assert!(filter.matches("example.com"));
        assert!(!filter.matches("other.com"));
        assert!(!filter.matches("sub.example.com"));
    }

    #[test]
    fn test_host_filter_wildcard_prefix() {
        // Per TLS/X.509 wildcard semantics, `*.example.com` matches
        // subdomains only — NOT the apex `example.com`.
        let filter = HostFilter::new().include("*.example.com");
        assert!(!filter.matches("example.com"));
        assert!(filter.matches("sub.example.com"));
        assert!(filter.matches("a.b.example.com"));
        assert!(!filter.matches("other.com"));
        // `notexample.com` must not match — wildcard is tied to a dot.
        assert!(!filter.matches("notexample.com"));
    }

    #[test]
    fn test_host_filter_apex_plus_wildcard_requires_both_patterns() {
        let filter = HostFilter::new()
            .include("example.com")
            .include("*.example.com");
        assert!(filter.matches("example.com"));
        assert!(filter.matches("sub.example.com"));
        assert!(filter.matches("a.b.example.com"));
        assert!(!filter.matches("other.com"));
    }

    #[test]
    fn test_host_filter_wildcard_suffix() {
        let filter = HostFilter::new().include("example.*");
        assert!(filter.matches("example.com"));
        assert!(filter.matches("example.org"));
        assert!(!filter.matches("other.com"));
    }

    #[test]
    fn test_host_filter_exclude_takes_precedence() {
        let filter = HostFilter::new()
            .include("*.example.com")
            .exclude("blocked.example.com");
        assert!(filter.matches("allowed.example.com"));
        assert!(!filter.matches("blocked.example.com"));
    }

    #[test]
    fn test_host_filter_empty_allows_all() {
        let filter = HostFilter::new();
        assert!(filter.matches("any.host.com"));
    }

    #[test]
    fn test_host_filter_strips_port() {
        let filter = HostFilter::new().include("example.com");
        assert!(filter.matches("example.com:8080"));
    }

    #[test]
    fn test_host_filter_ignores_case() {
        let filter = HostFilter::new().include("*.Example.com");
        assert!(filter.matches("API.example.COM"));
        assert!(
            HostFilter::new()
                .include("example.com")
                .matches("EXAMPLE.com:443")
        );
    }

    #[test]
    fn test_host_filter_handles_ipv6_literals() {
        let filter = HostFilter::new().include("::1");
        assert!(filter.matches("[::1]:8080"));
        assert!(filter.matches("[::1]"));
        assert!(
            filter.matches("::1"),
            "a bare literal's colons are not a port"
        );
        assert!(!filter.matches("[::2]:8080"));
    }

    #[test]
    fn test_host_filter_general_wildcard_needs_room_for_both_ends() {
        let filter = HostFilter::new().include("api*.com");
        assert!(filter.matches("api.example.com"));
        assert!(!HostFilter::new().include("a*a").matches("a"));
        assert!(HostFilter::new().include("a*a").matches("aa"));
    }

    #[test]
    fn test_passthrough_strips_ports_like_the_filter() {
        let https = HttpsConfig {
            passthrough_hosts: vec!["::1".to_owned(), "Pinned.example".to_owned()],
            ..HttpsConfig::default()
        };
        assert!(https.should_passthrough("[::1]:443"));
        assert!(https.should_passthrough("pinned.EXAMPLE:443"));
    }

    #[test]
    fn test_is_private_ip_loopback() {
        // IPv4 loopback
        assert!(is_private_ip("127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("127.255.255.255".parse().unwrap()));
        // IPv6 loopback
        assert!(is_private_ip("::1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_private_networks() {
        // Class A: 10.0.0.0/8
        assert!(is_private_ip("10.0.0.1".parse().unwrap()));
        assert!(is_private_ip("10.255.255.255".parse().unwrap()));
        // Class B: 172.16.0.0/12
        assert!(is_private_ip("172.16.0.1".parse().unwrap()));
        assert!(is_private_ip("172.31.255.255".parse().unwrap()));
        assert!(!is_private_ip("172.15.0.1".parse().unwrap())); // Outside range
        assert!(!is_private_ip("172.32.0.1".parse().unwrap())); // Outside range
        // Class C: 192.168.0.0/16
        assert!(is_private_ip("192.168.0.1".parse().unwrap()));
        assert!(is_private_ip("192.168.255.255".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_link_local() {
        // IPv4 link-local
        assert!(is_private_ip("169.254.0.1".parse().unwrap()));
        assert!(is_private_ip("169.254.255.255".parse().unwrap()));
        // IPv6 link-local
        assert!(is_private_ip("fe80::1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_public() {
        // Public IPv4 addresses
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip("203.0.113.1".parse().unwrap()));
        // Public IPv6 addresses
        assert!(!is_private_ip("2001:4860:4860::8888".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_unique_local_ipv6() {
        // Unique local addresses (fc00::/7)
        assert!(is_private_ip("fc00::1".parse().unwrap()));
        assert!(is_private_ip("fd00::1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_cgnat_shared_space() {
        // RFC 6598 shared address space: 100.64.0.0/10 (100.64.0.0 -
        // 100.127.255.255). These are used by carrier-grade NATs and
        // must not be treated as publicly reachable.
        assert!(is_private_ip("100.64.0.0".parse().unwrap()));
        assert!(is_private_ip("100.64.0.1".parse().unwrap()));
        assert!(is_private_ip("100.100.50.1".parse().unwrap()));
        assert!(is_private_ip("100.127.255.255".parse().unwrap()));
        // Just outside the range — these are public.
        assert!(!is_private_ip("100.63.255.255".parse().unwrap()));
        assert!(!is_private_ip("100.128.0.0".parse().unwrap()));
        // Unrelated 100.x addresses (public).
        assert!(!is_private_ip("100.0.0.1".parse().unwrap()));
    }
}
