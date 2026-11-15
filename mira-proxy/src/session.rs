//! Session management for capturing HTTP traffic.
//!
//! A session collects HTTP exchanges for later export (e.g., to HAR format).
//! Sessions are thread-safe and can be shared across connection handlers.

use crate::Result;
use crate::har::Har;
use crate::har::HarLog;
use crate::http::Exchange;
use bytes::Bytes;
use chrono::DateTime;
use chrono::Utc;
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::Arc;
use tracing::debug;

/// Default cap on exchanges retained by a session (10,000).
pub const DEFAULT_MAX_EXCHANGES: usize = 10_000;

/// Default cap on the bytes kept for any single captured body: none.
///
/// A capture is an archival artifact — inside a HAR export, a body this
/// cut short is indistinguishable from one the server really did truncate,
/// and the whole point of reaching for a debugging proxy is to see what
/// actually crossed the wire. Memory stays bounded by
/// [`DEFAULT_MAX_TOTAL_BODY_BYTES`] and [`DEFAULT_MAX_EXCHANGES`], which
/// drop whole exchanges rather than mutilating recent ones; the worst
/// case is that budget plus one outsized body, since the newest exchange
/// is never evicted on arrival.
///
/// Set [`SessionLimits::max_body_bytes`] explicitly to trade fidelity for
/// holding more exchanges in the same space.
pub const DEFAULT_MAX_CAPTURED_BODY_BYTES: usize = usize::MAX;

/// Default cap on the total captured body bytes held by a session (128 MB).
pub const DEFAULT_MAX_TOTAL_BODY_BYTES: usize = 128 * 1024 * 1024;

/// Bounds on what a capturing session retains.
///
/// A proxy left running for hours would otherwise turn into a memory
/// leak: every exchange holds its request and response bodies, and the
/// proxy's own `max_body_size` permits 500 MB per body. These bounds keep
/// a long capture's footprint predictable, at the price of dropping the
/// oldest exchanges and cutting outsized bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimits {
    /// Maximum number of exchanges retained. Once reached, adding an
    /// exchange evicts the oldest.
    pub max_exchanges: usize,

    /// Maximum bytes retained for a single body. Longer bodies are
    /// truncated to this and the exchange is marked truncated.
    pub max_body_bytes: usize,

    /// Maximum total captured body bytes across the whole session. Once
    /// exceeded, the oldest exchanges are evicted until it fits.
    pub max_total_body_bytes: usize,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_exchanges: DEFAULT_MAX_EXCHANGES,
            max_body_bytes: DEFAULT_MAX_CAPTURED_BODY_BYTES,
            max_total_body_bytes: DEFAULT_MAX_TOTAL_BODY_BYTES,
        }
    }
}

impl SessionLimits {
    /// Limits that retain everything.
    ///
    /// For sessions built from data the user handed us on purpose — a HAR
    /// file being loaded — where silently dropping entries would be worse
    /// than the memory it costs.
    pub fn unlimited() -> Self {
        Self {
            max_exchanges: usize::MAX,
            max_body_bytes: usize::MAX,
            max_total_body_bytes: usize::MAX,
        }
    }
}

/// A captured exchange with its timestamp.
#[derive(Debug, Clone)]
pub struct CapturedExchange {
    /// The HTTP exchange.
    pub exchange: Exchange,

    /// When the exchange was captured.
    pub timestamp: DateTime<Utc>,

    /// Whether any of this exchange's bodies was cut down to fit the
    /// session's [`SessionLimits::max_body_bytes`]. What is stored is
    /// then a prefix of what actually went over the wire.
    pub truncated: bool,
}

impl CapturedExchange {
    /// Create a new captured exchange with the current timestamp.
    pub fn new(exchange: Exchange) -> Self {
        Self::with_timestamp(exchange, Utc::now())
    }

    /// Create a new captured exchange with a specific timestamp.
    pub fn with_timestamp(exchange: Exchange, timestamp: DateTime<Utc>) -> Self {
        Self {
            exchange,
            timestamp,
            truncated: false,
        }
    }

    /// Cut every body down to `max_body_bytes`, recording whether
    /// anything was lost.
    fn truncate_bodies(&mut self, max_body_bytes: usize) {
        let mut truncated = truncate_body(&mut self.exchange.request.body, max_body_bytes);
        if let Some(response) = &mut self.exchange.response {
            truncated |= truncate_body(&mut response.body, max_body_bytes);
            truncated |= truncate_body(&mut response.body_decompressed, max_body_bytes);
        }
        self.truncated |= truncated;
    }

    /// Bytes of body this exchange retains.
    fn body_bytes(&self) -> usize {
        let mut total = self.exchange.request.body.len();
        if let Some(response) = &self.exchange.response {
            total += response.body.len();
            // For an identity-encoded response the decompressed body is a
            // clone of the raw one — the same allocation, so counting it
            // again would halve the effective budget.
            if response.encoding.is_compressed() {
                total += response.body_decompressed.len();
            }
        }
        total
    }
}

/// Cut `body` down to `max` bytes, returning whether anything was lost.
///
/// Copies rather than slicing: `Bytes::slice` shares the original
/// allocation, so slicing a 500 MB body would keep all 500 MB alive —
/// exactly the memory we are trying not to hold.
fn truncate_body(body: &mut Bytes, max: usize) -> bool {
    if body.len() <= max {
        return false;
    }
    *body = Bytes::copy_from_slice(&body[..max]);
    true
}

/// Inner storage for session data.
#[derive(Debug)]
struct SessionInner {
    /// Newest at the back; eviction takes from the front.
    exchanges: VecDeque<CapturedExchange>,

    limits: SessionLimits,

    /// Running total of [`CapturedExchange::body_bytes`] for everything
    /// currently retained, so eviction does not have to re-walk the deque.
    body_bytes: usize,

    /// How many exchanges have been evicted to stay within the limits.
    dropped: u64,
}

impl SessionInner {
    fn with_limits(limits: SessionLimits) -> Self {
        Self {
            exchanges: VecDeque::new(),
            limits,
            body_bytes: 0,
            dropped: 0,
        }
    }

    /// Push a capture and evict from the front until the limits hold.
    fn push(&mut self, mut captured: CapturedExchange) {
        captured.truncate_bodies(self.limits.max_body_bytes);
        self.body_bytes += captured.body_bytes();
        self.exchanges.push_back(captured);
        self.evict();
    }

    /// Drop the oldest exchanges until the session is within its limits.
    fn evict(&mut self) {
        // Never evict down to nothing: a single exchange over the total
        // budget is kept rather than dropped on arrival.
        while self.exchanges.len() > self.limits.max_exchanges.max(1)
            || (self.exchanges.len() > 1 && self.body_bytes > self.limits.max_total_body_bytes)
        {
            let Some(evicted) = self.exchanges.pop_front() else {
                break;
            };
            self.body_bytes = self.body_bytes.saturating_sub(evicted.body_bytes());
            self.dropped += 1;
            if self.dropped == 1 || self.dropped.is_multiple_of(1000) {
                debug!(
                    dropped = self.dropped,
                    retained = self.exchanges.len(),
                    body_bytes = self.body_bytes,
                    "session limit reached; evicting oldest exchanges"
                );
            }
        }
    }
}

impl Default for SessionInner {
    fn default() -> Self {
        Self::with_limits(SessionLimits::default())
    }
}

/// A session that captures HTTP exchanges for later export.
///
/// Sessions are thread-safe and can be cloned (clones share the same storage).
///
/// # Example
///
/// ```
/// use mira_proxy::Session;
///
/// let session = Session::new();
///
/// // Session can be cloned and shared across threads
/// let session_clone = session.clone();
///
/// // Check the number of captured exchanges
/// assert_eq!(session.len(), 0);
/// ```
#[derive(Debug, Clone, Default)]
pub struct Session {
    inner: Arc<RwLock<SessionInner>>,
}

impl Session {
    /// Create a new empty session with the default [`SessionLimits`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new empty session with explicit limits.
    pub fn with_limits(limits: SessionLimits) -> Self {
        Self {
            inner: Arc::new(RwLock::new(SessionInner::with_limits(limits))),
        }
    }

    /// The limits this session enforces.
    pub fn limits(&self) -> SessionLimits {
        self.inner.read().limits
    }

    /// Enforce `limits` from now on.
    ///
    /// Exchanges past the new bounds are evicted at once, oldest first.
    /// Bodies already retained keep their length; the body limit applies
    /// to exchanges added afterwards.
    pub fn set_limits(&self, limits: SessionLimits) {
        let mut inner = self.inner.write();
        inner.limits = limits;
        inner.evict();
    }

    /// How many exchanges have been evicted to stay within the limits.
    ///
    /// Non-zero means the session no longer holds the whole capture, so
    /// an export is a window onto the most recent traffic rather than all
    /// of it.
    pub fn dropped_count(&self) -> u64 {
        self.inner.read().dropped
    }

    /// Bytes of captured body currently retained.
    pub fn captured_body_bytes(&self) -> usize {
        self.inner.read().body_bytes
    }

    /// Add an exchange to the session.
    ///
    /// The exchange is timestamped with the current time. Bodies over the
    /// limit are truncated, and older exchanges are evicted if this one
    /// pushes the session past its bounds.
    pub fn add(&self, exchange: Exchange) {
        self.inner.write().push(CapturedExchange::new(exchange));
    }

    /// Add an exchange to the session with a specific timestamp.
    ///
    /// Subject to the same limits as [`Session::add`].
    pub fn add_with_timestamp(&self, exchange: Exchange, timestamp: DateTime<Utc>) {
        self.inner
            .write()
            .push(CapturedExchange::with_timestamp(exchange, timestamp));
    }

    /// Get the number of captured exchanges.
    pub fn len(&self) -> usize {
        self.inner.read().exchanges.len()
    }

    /// Check if the session is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.read().exchanges.is_empty()
    }

    /// Clear all captured exchanges.
    pub fn clear(&self) {
        let mut inner = self.inner.write();
        inner.exchanges.clear();
        inner.body_bytes = 0;
        inner.dropped = 0;
    }

    /// Get a clone of all captured exchanges, oldest first.
    pub fn exchanges(&self) -> Vec<CapturedExchange> {
        self.inner.read().exchanges.iter().cloned().collect()
    }

    /// Export the session to HAR format.
    pub fn to_har(&self) -> Har {
        let exchanges = self.exchanges();
        let mut log = HarLog::new();

        for captured in exchanges {
            let entry = HarLog::entry_from_exchange(&captured.exchange, captured.timestamp);
            log.add_entry(entry);
        }

        Har::new(log)
    }

    /// Export the session to HAR JSON string.
    pub fn to_har_json(&self) -> Result<String> {
        self.to_har().to_json()
    }

    /// Export the session to pretty-printed HAR JSON string.
    pub fn to_har_json_pretty(&self) -> Result<String> {
        self.to_har().to_json_pretty()
    }

    /// Import a session from a HAR structure.
    ///
    /// This creates a new session populated with exchanges from the HAR file.
    /// Mira extension fields (`_mira*`) are used when present for full
    /// fidelity; otherwise values are inferred from standard HAR fields.
    pub fn from_har(har: &Har) -> Result<Self> {
        Self::from_har_with_limits(har, SessionLimits::unlimited())
    }

    /// Import a session from a HAR structure under explicit limits.
    ///
    /// [`Session::from_har`] loads everything: a HAR file is data the
    /// user pointed us at deliberately, and quietly dropping half of it
    /// would be worse than the memory. Use this when the imported session
    /// will go on capturing live traffic and needs a bound.
    pub fn from_har_with_limits(har: &Har, limits: SessionLimits) -> Result<Self> {
        let session = Self::with_limits(limits);
        let mut id_counter = 1u64;

        for entry in &har.log.entries {
            let exchange = entry.to_exchange(id_counter)?;

            // Use the ID from the exchange (which may have come from _miraId)
            // to ensure our counter stays ahead
            id_counter = id_counter.max(exchange.request.id + 1);

            let timestamp = entry.started_time().unwrap_or_else(Utc::now);

            session.add_with_timestamp(exchange, timestamp);
        }

        Ok(session)
    }

    /// Import a session from a HAR JSON string.
    ///
    /// This creates a new session populated with exchanges from the HAR file.
    /// Mira extension fields (`_mira*`) are used when present for full
    /// fidelity; otherwise values are inferred from standard HAR fields.
    pub fn from_har_json(json: &str) -> Result<Self> {
        let har: Har = serde_json::from_str(json)?;
        Self::from_har(&har)
    }

    /// Import a session from a HAR JSON string under explicit limits.
    pub fn from_har_json_with_limits(json: &str, limits: SessionLimits) -> Result<Self> {
        let har: Har = serde_json::from_str(json)?;
        Self::from_har_with_limits(&har, limits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Encoding;
    use crate::http::Request;
    use crate::http::Response;
    use bytes::Bytes;

    /// An exchange whose response body is `body_len` bytes.
    fn exchange_with_body(id: u64, body_len: usize) -> Exchange {
        let mut exchange = sample_exchange();
        exchange.request.id = id;
        let body = Bytes::from(vec![b'x'; body_len]);
        if let Some(response) = &mut exchange.response {
            response.request_id = id;
            response.body = body.clone();
            response.body_decompressed = body;
        }
        exchange
    }

    fn sample_exchange() -> Exchange {
        let request = Request {
            id: 1,
            client_addr: "127.0.0.1:12345".parse().expect("valid addr"),
            method: "GET".into(),
            url: "http://example.com/test".parse().expect("valid url"),
            version: "HTTP/1.1".into(),
            headers: vec![("Host".into(), "example.com".into())],
            body: Bytes::new(),
        };

        let response = Response {
            request_id: 1,
            status: 200,
            reason: "OK".into(),
            version: "HTTP/1.1".into(),
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: Bytes::from_static(b"Hello"),
            body_decompressed: Bytes::from_static(b"Hello"),
            encoding: Encoding::Identity,
            resource_type: crate::http::ResourceType::Other,
            duration_ms: 50,
            timings: None,
            remote_addr: None,
        };

        Exchange {
            request,
            response: Some(response),
            error: None,
        }
    }

    #[test]
    fn test_session_new() {
        let session = Session::new();
        assert!(session.is_empty());
        assert_eq!(session.len(), 0);
    }

    #[test]
    fn test_session_add_exchange() {
        let session = Session::new();
        let exchange = sample_exchange();

        session.add(exchange);

        assert!(!session.is_empty());
        assert_eq!(session.len(), 1);
    }

    #[test]
    fn test_session_clear() {
        let session = Session::new();
        session.add(sample_exchange());
        session.add(sample_exchange());

        assert_eq!(session.len(), 2);

        session.clear();

        assert!(session.is_empty());
        assert_eq!(session.len(), 0);
    }

    #[test]
    fn test_session_clone_shares_storage() {
        let session = Session::new();
        let session_clone = session.clone();

        session.add(sample_exchange());

        // Clone should see the same data
        assert_eq!(session.len(), 1);
        assert_eq!(session_clone.len(), 1);

        // Adding to clone should be visible in original
        session_clone.add(sample_exchange());

        assert_eq!(session.len(), 2);
        assert_eq!(session_clone.len(), 2);
    }

    #[test]
    fn test_session_exchanges() {
        let session = Session::new();
        session.add(sample_exchange());
        session.add(sample_exchange());

        let exchanges = session.exchanges();

        assert_eq!(exchanges.len(), 2);
        assert_eq!(exchanges[0].exchange.request.method, "GET");
    }

    #[test]
    fn test_session_to_har() {
        let session = Session::new();
        session.add(sample_exchange());

        let har = session.to_har();

        assert_eq!(har.log.version, "1.2");
        assert_eq!(har.log.entries.len(), 1);
        assert_eq!(har.log.entries[0].request.method, "GET");
        assert_eq!(har.log.entries[0].response.status, 200);
    }

    #[test]
    fn test_session_to_har_json() {
        let session = Session::new();
        session.add(sample_exchange());

        let json = session.to_har_json().expect("should serialize");

        assert!(json.contains("\"version\":\"1.2\""));
        assert!(json.contains("\"method\":\"GET\""));
        assert!(json.contains("\"status\":200"));
    }

    #[test]
    fn test_session_evicts_oldest_beyond_the_entry_cap() {
        let session = Session::with_limits(SessionLimits {
            max_exchanges: 3,
            ..SessionLimits::default()
        });

        for id in 1..=5 {
            session.add(exchange_with_body(id, 0));
        }

        assert_eq!(session.len(), 3, "the cap bounds what is retained");
        assert_eq!(session.dropped_count(), 2);
        let ids: Vec<u64> = session
            .exchanges()
            .iter()
            .map(|c| c.exchange.request.id)
            .collect();
        assert_eq!(ids, vec![3, 4, 5], "the newest exchanges survive");
    }

    #[test]
    fn test_session_truncates_outsized_bodies() {
        let session = Session::with_limits(SessionLimits {
            max_body_bytes: 1024,
            ..SessionLimits::default()
        });

        session.add(exchange_with_body(1, 4096));

        let captured = session.exchanges();
        let response = captured[0].exchange.response.as_ref().expect("response");
        assert_eq!(response.body.len(), 1024, "the body is cut to the cap");
        assert_eq!(response.body_decompressed.len(), 1024);
        assert!(
            captured[0].truncated,
            "the capture is marked as no longer complete"
        );
    }

    #[test]
    fn test_session_keeps_bodies_under_the_cap_intact() {
        let session = Session::with_limits(SessionLimits {
            max_body_bytes: 1024,
            ..SessionLimits::default()
        });

        session.add(exchange_with_body(1, 1024));

        let captured = session.exchanges();
        let response = captured[0].exchange.response.as_ref().expect("response");
        assert_eq!(response.body.len(), 1024);
        assert!(!captured[0].truncated, "nothing was lost");
    }

    #[test]
    fn test_session_evicts_to_stay_within_the_total_body_budget() {
        let session = Session::with_limits(SessionLimits {
            max_exchanges: 100,
            max_body_bytes: 1024,
            max_total_body_bytes: 4096,
        });

        // Eight 1 KB bodies against a 4 KB budget.
        for id in 1..=8 {
            session.add(exchange_with_body(id, 1024));
        }

        assert!(
            session.captured_body_bytes() <= 4096,
            "retained {} bytes, over budget",
            session.captured_body_bytes()
        );
        assert_eq!(session.len(), 4);
        assert_eq!(session.dropped_count(), 4);
    }

    #[test]
    fn test_session_keeps_one_exchange_even_when_it_alone_exceeds_the_budget() {
        let session = Session::with_limits(SessionLimits {
            max_exchanges: 100,
            max_body_bytes: 8192,
            max_total_body_bytes: 1024,
        });

        session.add(exchange_with_body(1, 8192));

        assert_eq!(
            session.len(),
            1,
            "the newest exchange is never evicted on arrival"
        );
    }

    #[test]
    fn test_session_clear_resets_the_accounting() {
        let session = Session::with_limits(SessionLimits {
            max_exchanges: 2,
            ..SessionLimits::default()
        });
        for id in 1..=5 {
            session.add(exchange_with_body(id, 512));
        }
        assert!(session.dropped_count() > 0);

        session.clear();

        assert!(session.is_empty());
        assert_eq!(session.captured_body_bytes(), 0);
        assert_eq!(session.dropped_count(), 0);
    }

    #[test]
    fn test_set_limits_evicts_the_oldest_at_once() {
        let session = Session::new();
        for id in 1..=5 {
            session.add(exchange_with_body(id, 0));
        }

        session.set_limits(SessionLimits {
            max_exchanges: 2,
            ..SessionLimits::default()
        });

        let ids: Vec<u64> = session
            .exchanges()
            .iter()
            .map(|c| c.exchange.request.id)
            .collect();
        assert_eq!(ids, [4, 5]);
        assert_eq!(session.dropped_count(), 3);
        assert_eq!(session.limits().max_exchanges, 2);
    }

    #[test]
    fn test_session_default_limits_are_bounded() {
        // The whole point of the defaults: a session left capturing does
        // not grow without limit. The bound comes from the entry count
        // and the total byte budget — not from cutting bodies, which
        // would damage the capture to save space.
        let limits = Session::new().limits();
        assert!(limits.max_exchanges < usize::MAX);
        assert!(limits.max_total_body_bytes < usize::MAX);
    }

    #[test]
    fn test_session_keeps_whole_bodies_by_default() {
        // A HAR export has to hold what the server actually sent.
        let session = Session::new();
        session.add(exchange_with_body(1, 8 * 1024 * 1024));

        let captured = session.exchanges();
        let response = captured[0].exchange.response.as_ref().expect("response");
        assert_eq!(response.body.len(), 8 * 1024 * 1024);
        assert!(!captured[0].truncated);
    }

    #[test]
    fn test_from_har_keeps_everything() {
        // A HAR file is data the user pointed us at on purpose, so the
        // import path does not silently drop any of it.
        let source = Session::new();
        for id in 1..=5 {
            source.add(exchange_with_body(id, 0));
        }
        let json = source.to_har_json().expect("serialize");

        let imported = Session::from_har_json(&json).expect("import");
        assert_eq!(imported.len(), 5);
        assert_eq!(imported.limits(), SessionLimits::unlimited());
        assert_eq!(imported.dropped_count(), 0);
    }

    #[test]
    fn test_from_har_with_limits_reports_what_it_dropped() {
        let source = Session::new();
        for id in 1..=5 {
            source.add(exchange_with_body(id, 0));
        }
        let json = source.to_har_json().expect("serialize");

        let imported = Session::from_har_json_with_limits(
            &json,
            SessionLimits {
                max_exchanges: 2,
                ..SessionLimits::default()
            },
        )
        .expect("import");

        assert_eq!(imported.len(), 2);
        assert_eq!(
            imported.dropped_count(),
            3,
            "the caller can tell the import was incomplete"
        );
    }

    #[test]
    fn test_session_thread_safety() {
        use std::thread;

        let session = Session::new();

        let handles: Vec<_> = (0..10)
            .map(|_| {
                let session = session.clone();
                thread::spawn(move || {
                    for _ in 0..100 {
                        session.add(sample_exchange());
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("thread should not panic");
        }

        assert_eq!(session.len(), 1000);
    }
}
