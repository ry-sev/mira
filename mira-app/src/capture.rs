//! The capture: every exchange the window knows about, in the order they
//! started, and what can be said about them as a whole.
//!
//! Exchanges arrive from the proxy's own threads as [`Captured`] messages and
//! are folded in here on the main thread. A request appears as soon as it is
//! seen and fills in when its response completes.
//!
//! The capture is plain data. Its owner holds it in an entity and notifies
//! after each change.

use crate::site;
use crate::theme;
use chrono::DateTime;
use chrono::Utc;
use gpui_kit::Hsla;
use mira_proxy::CapturedExchange;
use mira_proxy::Exchange;
use mira_proxy::Request;
use mira_proxy::ResourceType;
use mira_proxy::Response;
use mira_proxy::Session;
use mira_proxy::SessionLimits;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

/// What the proxy thread reports to the window.
pub enum Captured {
    /// The proxy is accepting connections on this address.
    Listening(SocketAddr),

    /// The proxy could not start, or stopped.
    Failed(String),

    /// A request was seen. Its response follows as an [`Captured::Exchange`].
    Request(Box<Request>, DateTime<Utc>),

    /// A request-response cycle finished, or failed.
    Exchange(Box<Exchange>, DateTime<Utc>),
}

/// Where the proxy is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listening {
    Starting,
    On(SocketAddr),
    Failed(String),
}

/// A coarse resource type, for filtering and the type column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Document,
    Script,
    Data,
    Stylesheet,
    Font,
    Image,
    Media,
    WebSocket,
    Tunnel,
    Other,
}

impl Kind {
    /// What the type column shows.
    pub fn short(self) -> &'static str {
        match self {
            Self::Document => "DOC",
            Self::Script => "JS",
            Self::Data => "DATA",
            Self::Stylesheet => "CSS",
            Self::Font => "FONT",
            Self::Image => "IMG",
            Self::Media => "MEDIA",
            Self::WebSocket => "WS",
            Self::Tunnel => "TUNNEL",
            Self::Other => "OTHER",
        }
    }

    fn of(request: &Request, response: Option<&Response>) -> Self {
        if request.method.eq_ignore_ascii_case("CONNECT") {
            return Self::Tunnel;
        }
        let Some(response) = response else {
            return Self::Other;
        };
        if response.status == 101 {
            return Self::WebSocket;
        }
        match response.resource_type {
            ResourceType::Document => Self::Document,
            ResourceType::Script | ResourceType::Wasm => Self::Script,
            ResourceType::Json | ResourceType::Xhr | ResourceType::Fetch => Self::Data,
            ResourceType::Stylesheet => Self::Stylesheet,
            ResourceType::Font => Self::Font,
            ResourceType::Image => Self::Image,
            ResourceType::Media => Self::Media,
            ResourceType::Other => Self::Other,
        }
    }
}

/// One exchange, as the window shows it.
pub struct Entry {
    pub request: Request,

    /// `None` while the response is still on its way, or when it failed.
    pub response: Option<Response>,

    /// Why the exchange failed, if it did.
    pub error: Option<String>,

    /// When the request was seen, in milliseconds after the capture began.
    pub start_ms: f64,

    /// The host, with its port when it is not the scheme's default.
    pub host: String,

    pub path: String,
    pub query: String,

    /// The color of the host's site, so subdomains share it.
    pub color: Hsla,

    pub kind: Kind,
}

impl Entry {
    fn new(request: Request, start_ms: f64) -> Self {
        let parts = site::parts(&request.url);
        let site = site::site_of(request.url.host_str().unwrap_or_default());
        Self {
            kind: Kind::of(&request, None),
            color: theme::site_color(&site),
            host: parts.host,
            path: parts.path,
            query: parts.query,
            request,
            response: None,
            error: None,
            start_ms,
        }
    }

    fn with_outcome(mut self, response: Option<Response>, error: Option<String>) -> Self {
        self.kind = Kind::of(&self.request, response.as_ref());
        self.response = response;
        self.error = error;
        self
    }

    pub fn id(&self) -> u64 {
        self.request.id
    }

    pub fn status(&self) -> Option<u16> {
        self.response.as_ref().map(|r| r.status)
    }

    /// Whether the response has not arrived and the exchange has not failed.
    pub fn pending(&self) -> bool {
        self.response.is_none() && self.error.is_none()
    }

    /// Whether the exchange failed or got an error status.
    pub fn failed(&self) -> bool {
        self.error.is_some() || self.status().is_some_and(|s| s >= 400)
    }

    /// Bytes of response body, as sent.
    pub fn size(&self) -> usize {
        self.response.as_ref().map_or(0, |r| r.body.len())
    }

    /// How long the exchange took, in milliseconds.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a duration in milliseconds is far below f64's exact range"
    )]
    pub fn duration_ms(&self) -> f64 {
        self.response.as_ref().map_or(0.0, |r| r.duration_ms as f64)
    }

    /// The response's content type, without parameters.
    pub fn mime(&self) -> &str {
        self.response
            .as_ref()
            .and_then(|r| r.content_type())
            .and_then(|ct| ct.split(';').next())
            .unwrap_or_default()
            .trim()
    }
}

/// A server the client talked to, and how that went.
pub struct HostSummary {
    pub name: String,
    pub color: Hsla,
    pub requests: usize,
    pub failed: usize,
    pub bytes: usize,
    pub first_ms: f64,
}

/// Totals over the whole capture.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    pub requests: usize,
    pub pending: usize,
    pub failed: usize,
    pub bytes: usize,
    pub informational: usize,
    pub success: usize,
    pub redirect: usize,
    pub client_error: usize,
    pub server_error: usize,
    pub no_response: usize,
}

/// Every exchange the window knows about.
pub struct Capture {
    entries: Vec<Arc<Entry>>,
    index: HashMap<u64, usize>,
    origin: Option<DateTime<Utc>>,
    max_entries: usize,
    revision: u64,
    listening: Listening,
    intercept: bool,

    /// Whether new exchanges are kept. While paused, only exchanges already
    /// on screen finish.
    recording: bool,

    /// Every exchange kept, for HAR export.
    session: Session,
}

impl Capture {
    pub fn new(session: Session, intercept: bool) -> Self {
        Self {
            entries: Vec::new(),
            index: HashMap::new(),
            origin: None,
            max_entries: session.limits().max_exchanges.max(1),
            revision: 0,
            listening: Listening::Starting,
            intercept,
            recording: true,
            session,
        }
    }

    /// Changes whenever anything in the capture does, for caches to key on.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn entries(&self) -> &[Arc<Entry>] {
        &self.entries
    }

    pub fn get(&self, id: u64) -> Option<&Arc<Entry>> {
        self.index.get(&id).and_then(|&at| self.entries.get(at))
    }

    pub fn listening(&self) -> &Listening {
        &self.listening
    }

    pub fn intercept(&self) -> bool {
        self.intercept
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn recording(&self) -> bool {
        self.recording
    }

    pub fn set_recording(&mut self, recording: bool) {
        self.recording = recording;
        self.revision += 1;
    }

    /// The proxy is restarting with new settings.
    pub fn restart(&mut self, intercept: bool, limits: SessionLimits) {
        self.listening = Listening::Starting;
        self.intercept = intercept;
        self.session.set_limits(limits);
        self.max_entries = limits.max_exchanges.max(1);
        let excess = self.entries.len().saturating_sub(self.max_entries);
        if excess > 0 {
            self.entries.drain(..excess);
            self.reindex();
        }
        self.revision += 1;
    }

    /// When the last exchange ended, in milliseconds after the start.
    pub fn end_ms(&self) -> f64 {
        self.entries
            .iter()
            .map(|e| e.start_ms + e.duration_ms())
            .fold(0.0, f64::max)
    }

    /// Fold one message from the proxy in.
    pub fn ingest(&mut self, message: Captured) {
        match message {
            Captured::Listening(addr) => self.listening = Listening::On(addr),
            Captured::Failed(reason) => self.listening = Listening::Failed(reason),
            Captured::Request(request, at) => {
                if !self.recording {
                    return;
                }
                let start_ms = self.offset(at);
                self.upsert(Entry::new(*request, start_ms));
            }
            Captured::Exchange(exchange, at) => {
                let seen = self.get(exchange.request.id).map(|e| e.start_ms);
                if !self.recording && seen.is_none() {
                    return;
                }
                self.session.add_with_timestamp((*exchange).clone(), at);
                let Exchange {
                    request,
                    response,
                    error,
                } = *exchange;
                let start_ms = match seen {
                    Some(start_ms) => start_ms,
                    None => self.offset(at),
                };
                self.upsert(Entry::new(request, start_ms).with_outcome(response, error));
            }
        }
        self.revision += 1;
    }

    /// Replace everything with the exchanges of a loaded HAR file.
    pub fn replace_with(&mut self, loaded: &Session) {
        self.clear_entries();
        let captured = loaded.exchanges();
        self.origin = captured.iter().map(|c| c.timestamp).min();
        for CapturedExchange {
            exchange,
            timestamp,
            ..
        } in captured
        {
            let start_ms = self.offset(timestamp);
            self.session.add_with_timestamp(exchange.clone(), timestamp);
            let Exchange {
                request,
                response,
                error,
            } = exchange;
            self.upsert(Entry::new(request, start_ms).with_outcome(response, error));
        }
        self.revision += 1;
    }

    /// Forget every exchange, here and in the session.
    pub fn clear(&mut self) {
        self.clear_entries();
        self.revision += 1;
    }

    fn clear_entries(&mut self) {
        self.entries.clear();
        self.index.clear();
        self.origin = None;
        self.session.clear();
    }

    /// Milliseconds after the capture began. The first exchange starts it.
    #[expect(
        clippy::cast_precision_loss,
        reason = "milliseconds of a capture are far below f64's exact range"
    )]
    fn offset(&mut self, at: DateTime<Utc>) -> f64 {
        let origin = *self.origin.get_or_insert(at);
        (at - origin).num_milliseconds().max(0) as f64
    }

    fn upsert(&mut self, entry: Entry) {
        let id = entry.id();
        let entry = Arc::new(entry);
        if let Some(&at) = self.index.get(&id) {
            self.entries[at] = entry;
            return;
        }
        self.index.insert(id, self.entries.len());
        self.entries.push(entry);
        self.trim();
    }

    /// Drop the oldest entries once there are more than the session keeps.
    ///
    /// In batches of a tenth of the cap, so the index is rebuilt once per
    /// batch rather than once per entry.
    fn trim(&mut self) {
        let slack = (self.max_entries / 10).max(1);
        if self.entries.len() <= self.max_entries.saturating_add(slack) {
            return;
        }
        let dropped = self.entries.len() - self.max_entries;
        self.entries.drain(..dropped);
        self.reindex();
    }

    fn reindex(&mut self) {
        self.index = self
            .entries
            .iter()
            .enumerate()
            .map(|(at, e)| (e.id(), at))
            .collect();
    }

    /// Totals over every entry.
    pub fn totals(&self) -> Totals {
        let mut totals = Totals {
            requests: self.entries.len(),
            ..Totals::default()
        };
        for entry in &self.entries {
            totals.bytes += entry.size();
            if entry.pending() {
                totals.pending += 1;
            }
            if entry.failed() {
                totals.failed += 1;
            }
            match entry.status() {
                Some(100..=199) => totals.informational += 1,
                Some(200..=299) => totals.success += 1,
                Some(300..=399) => totals.redirect += 1,
                Some(400..=499) => totals.client_error += 1,
                Some(_) => totals.server_error += 1,
                None if entry.error.is_some() => totals.no_response += 1,
                None => {}
            }
        }
        totals
    }

    /// Every host, in the order it was first contacted.
    pub fn hosts(&self) -> Vec<HostSummary> {
        let mut hosts: Vec<HostSummary> = Vec::new();
        let mut at: HashMap<&str, usize> = HashMap::new();
        for entry in &self.entries {
            let index = *at.entry(entry.host.as_str()).or_insert_with(|| {
                hosts.push(HostSummary {
                    name: entry.host.clone(),
                    color: entry.color,
                    requests: 0,
                    failed: 0,
                    bytes: 0,
                    first_ms: entry.start_ms,
                });
                hosts.len() - 1
            });
            let host = &mut hosts[index];
            host.requests += 1;
            host.bytes += entry.size();
            host.failed += usize::from(entry.failed());
            host.first_ms = host.first_ms.min(entry.start_ms);
        }
        hosts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u64, url: &str) -> Request {
        Request {
            id,
            client_addr: "127.0.0.1:5000".parse().expect("addr"),
            method: "GET".to_owned(),
            url: url.parse().expect("url"),
            version: "HTTP/1.1".to_owned(),
            headers: Vec::new(),
            body: bytes::Bytes::new(),
        }
    }

    fn response(id: u64, status: u16, body: &'static [u8]) -> Response {
        Response {
            request_id: id,
            status,
            reason: String::new(),
            version: "HTTP/1.1".to_owned(),
            headers: vec![("Content-Type".to_owned(), "text/html".to_owned())],
            body: bytes::Bytes::from_static(body),
            body_decompressed: bytes::Bytes::from_static(body),
            encoding: mira_proxy::Encoding::Identity,
            resource_type: ResourceType::Document,
            duration_ms: 40,
            timings: None,
            remote_addr: None,
        }
    }

    #[test]
    fn test_a_request_fills_in_when_its_response_arrives() {
        let mut capture = Capture::new(Session::new(), false);
        let at = Utc::now();
        capture.ingest(Captured::Request(
            Box::new(request(1, "https://a.example.com/x?y=1")),
            at,
        ));
        let entry = capture.get(1).expect("seen");
        assert!(entry.pending());
        assert_eq!(entry.host, "a.example.com");
        assert_eq!(entry.query, "?y=1");
        assert_eq!(entry.color, theme::site_color("example.com"));

        let exchange = Exchange {
            request: request(1, "https://a.example.com/x?y=1"),
            response: Some(response(1, 200, b"hello")),
            error: None,
        };
        capture.ingest(Captured::Exchange(
            Box::new(exchange),
            at + chrono::Duration::seconds(5),
        ));
        assert_eq!(capture.entries().len(), 1, "the same row, filled in");
        let entry = capture.get(1).expect("seen");
        assert_eq!(entry.status(), Some(200));
        assert_eq!(entry.kind, Kind::Document);
        assert_eq!(entry.size(), 5);
        assert!(
            entry.start_ms.abs() < f64::EPSILON,
            "the start is when the request was seen"
        );
    }

    #[test]
    fn test_paused_capture_keeps_only_exchanges_already_on_screen() {
        let mut capture = Capture::new(Session::new(), false);
        let at = Utc::now();
        capture.ingest(Captured::Request(
            Box::new(request(1, "http://a.test/")),
            at,
        ));
        capture.set_recording(false);
        capture.ingest(Captured::Request(
            Box::new(request(2, "http://a.test/")),
            at,
        ));
        for id in [1, 2] {
            capture.ingest(Captured::Exchange(
                Box::new(Exchange {
                    request: request(id, "http://a.test/"),
                    response: Some(response(id, 200, b"ok")),
                    error: None,
                }),
                at,
            ));
        }

        assert_eq!(capture.entries().len(), 1);
        assert!(!capture.get(1).expect("seen before the pause").pending());
        assert_eq!(capture.session().len(), 1);
    }

    #[test]
    fn test_totals_and_hosts() {
        let mut capture = Capture::new(Session::new(), false);
        let at = Utc::now();
        for (id, url, status) in [
            (1, "http://a.test/", Some(200)),
            (2, "http://a.test/missing", Some(404)),
            (3, "http://b.test/", None),
        ] {
            let exchange = Exchange {
                request: request(id, url),
                response: status.map(|s| response(id, s, b"ab")),
                error: status.is_none().then(|| "refused".to_owned()),
            };
            capture.ingest(Captured::Exchange(Box::new(exchange), at));
        }
        let totals = capture.totals();
        assert_eq!(totals.requests, 3);
        assert_eq!(totals.failed, 2);
        assert_eq!(totals.success, 1);
        assert_eq!(totals.client_error, 1);
        assert_eq!(totals.no_response, 1);
        let hosts = capture.hosts();
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].name, "a.test");
        assert_eq!(hosts[0].requests, 2);
        assert_eq!(hosts[0].failed, 1);
    }

    #[test]
    fn test_restart_with_a_lower_limit_drops_the_oldest() {
        let mut capture = Capture::new(Session::new(), false);
        let at = Utc::now();
        for id in 1..=5 {
            capture.ingest(Captured::Request(
                Box::new(request(id, "http://a.test/")),
                at,
            ));
        }
        capture.ingest(Captured::Listening("127.0.0.1:1".parse().expect("addr")));
        capture.restart(
            true,
            SessionLimits {
                max_exchanges: 2,
                ..SessionLimits::default()
            },
        );
        assert_eq!(capture.listening(), &Listening::Starting);
        assert!(capture.intercept());
        let ids: Vec<u64> = capture.entries().iter().map(|e| e.id()).collect();
        assert_eq!(ids, [4, 5]);
        assert_eq!(capture.get(4).map(|e| e.id()), Some(4));
    }

    #[test]
    fn test_the_oldest_entries_go_past_the_limit() {
        let limits = SessionLimits {
            max_exchanges: 10,
            ..SessionLimits::default()
        };
        let mut capture = Capture::new(Session::with_limits(limits), false);
        let at = Utc::now();
        for id in 0..12 {
            capture.ingest(Captured::Request(
                Box::new(request(id, "http://a.test/")),
                at,
            ));
        }
        assert_eq!(capture.entries().len(), 10);
        assert!(capture.get(0).is_none() && capture.get(1).is_none());
        assert_eq!(capture.get(11).map(|e| e.id()), Some(11));
    }
}
