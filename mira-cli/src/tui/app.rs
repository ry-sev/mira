//! Application state management for the TUI.

use bytes::Bytes;
use mira_proxy::Exchange;
use mira_proxy::Request;
use mira_proxy::Session;
use ratatui::widgets::ScrollbarState;
use ratatui::widgets::TableState;

/// Which pane is currently focused.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FocusPane {
    /// The traffic table is focused.
    #[default]
    Table,

    /// The request pane is focused.
    Request,

    /// The response pane is focused.
    Response,
}

impl FocusPane {
    /// Cycle to the next focus pane (Table -> Request -> Response -> Table).
    pub fn next(self) -> Self {
        match self {
            Self::Table => Self::Request,
            Self::Request => Self::Response,
            Self::Response => Self::Table,
        }
    }

    /// Cycle to the previous focus pane.
    pub fn prev(self) -> Self {
        match self {
            Self::Table => Self::Response,
            Self::Request => Self::Table,
            Self::Response => Self::Request,
        }
    }
}

/// Tab selection for the request pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RequestTab {
    /// Show general request info (URL, method).
    #[default]
    General,

    /// Show request headers.
    Headers,

    /// Show URL query parameters.
    Query,

    /// Show request body content.
    Body,
}

impl RequestTab {
    /// Get all tab names.
    pub fn all() -> &'static [&'static str] {
        &["General", "Headers", "Query", "Body"]
    }

    /// Get the tab index.
    pub fn index(self) -> usize {
        match self {
            Self::General => 0,
            Self::Headers => 1,
            Self::Query => 2,
            Self::Body => 3,
        }
    }

    /// Navigate to the next tab.
    pub fn next(self) -> Self {
        match self {
            Self::General => Self::Headers,
            Self::Headers => Self::Query,
            Self::Query => Self::Body,
            Self::Body => Self::General,
        }
    }

    /// Navigate to the previous tab.
    pub fn prev(self) -> Self {
        match self {
            Self::General => Self::Body,
            Self::Headers => Self::General,
            Self::Query => Self::Headers,
            Self::Body => Self::Query,
        }
    }
}

/// Tab selection for the response pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResponseTab {
    /// Show general response info (status).
    #[default]
    General,

    /// Show response headers.
    Headers,

    /// Show response body content.
    Body,

    /// Show cookies from Set-Cookie headers.
    Cookies,

    /// Show timing breakdown.
    Timings,
}

impl ResponseTab {
    /// Get all tab names.
    pub fn all() -> &'static [&'static str] {
        &["General", "Headers", "Body", "Cookies", "Timings"]
    }

    /// Get the tab index.
    pub fn index(self) -> usize {
        match self {
            Self::General => 0,
            Self::Headers => 1,
            Self::Body => 2,
            Self::Cookies => 3,
            Self::Timings => 4,
        }
    }

    /// Navigate to the next tab.
    pub fn next(self) -> Self {
        match self {
            Self::General => Self::Headers,
            Self::Headers => Self::Body,
            Self::Body => Self::Cookies,
            Self::Cookies => Self::Timings,
            Self::Timings => Self::General,
        }
    }

    /// Navigate to the previous tab.
    pub fn prev(self) -> Self {
        match self {
            Self::General => Self::Timings,
            Self::Headers => Self::General,
            Self::Body => Self::Headers,
            Self::Cookies => Self::Body,
            Self::Timings => Self::Cookies,
        }
    }
}

/// How much of a body the detail pane keeps.
///
/// Everything a row holds, it holds for the row's whole life — and
/// [`crate::tui::ui`] re-parses the body on every frame the Body tab is
/// open, so an oversized one costs memory and frame time both. Far more
/// than this is unreadable in a terminal pane anyway.
///
/// This bounds the *view* only. The session keeps the body whole, so a
/// HAR export is unaffected.
pub const MAX_DISPLAY_BODY_BYTES: usize = 256 * 1024;

/// Cut a body down to what the detail pane keeps.
///
/// Copies rather than slicing: `Bytes::slice` shares the original
/// allocation, so a slice of a 500 MB body would pin all 500 MB —
/// exactly the memory this is meant to give back.
fn truncate_for_display(body: &Bytes) -> Bytes {
    if body.len() <= MAX_DISPLAY_BODY_BYTES {
        return body.clone();
    }
    Bytes::copy_from_slice(&body[..MAX_DISPLAY_BODY_BYTES])
}

/// Request details for the detail pane.
#[derive(Debug, Clone)]
pub struct RequestDetail {
    /// HTTP method.
    pub method: String,

    /// Request URL.
    pub url: String,

    /// HTTP version (e.g., "HTTP/1.1").
    pub version: String,

    /// Request headers.
    pub headers: Vec<(String, String)>,

    /// Request body, cut to [`MAX_DISPLAY_BODY_BYTES`].
    pub body: Bytes,

    /// Full length of the body as captured, so the pane can say how much
    /// of it is not being shown.
    pub body_len: usize,
}

/// Response details for the detail pane.
#[derive(Debug, Clone)]
pub struct ResponseDetail {
    /// HTTP status code.
    pub status: u16,

    /// HTTP reason phrase.
    pub reason: String,

    /// HTTP version (e.g., "HTTP/1.1").
    pub version: String,

    /// Response headers.
    pub headers: Vec<(String, String)>,

    /// Decompressed response body, cut to [`MAX_DISPLAY_BODY_BYTES`].
    pub body: Bytes,

    /// Full length of the decompressed body as captured, so the pane can
    /// say how much of it is not being shown.
    pub body_len: usize,

    /// Remote server address (IP:port).
    pub remote_addr: Option<String>,
}

/// Timing breakdown for the detail pane.
#[derive(Debug, Clone, Default)]
pub struct TimingDetail {
    /// Time spent waiting for a connection from the pool (usually 0).
    pub blocked_ms: Option<f64>,

    /// Time to establish TCP connection (None if reused).
    pub connect_ms: Option<f64>,

    /// Time for SSL/TLS handshake (None for plain HTTP).
    pub ssl_ms: Option<f64>,

    /// Time to send the request (headers + body).
    pub send_ms: Option<f64>,

    /// Time waiting for first byte of response (TTFB).
    pub wait_ms: Option<f64>,

    /// Time to receive the complete response body.
    pub receive_ms: Option<f64>,

    /// Whether the connection was reused from the pool.
    pub connection_reused: bool,

    /// Total duration in milliseconds.
    pub total_ms: Option<u64>,
}

/// Resource type inferred from content-type or URL.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResourceType {
    /// Document (HTML).
    Document,

    /// Stylesheet (CSS).
    Stylesheet,

    /// JavaScript.
    Script,

    /// Image (PNG, JPEG, GIF, SVG, WebP, etc.).
    Image,

    /// Font (WOFF, WOFF2, TTF, OTF).
    Font,

    /// JSON data.
    Json,

    /// XML data.
    Xml,

    /// XHR/Fetch request (API calls).
    #[allow(dead_code)]
    Xhr,

    /// WebSocket.
    #[allow(dead_code)]
    WebSocket,

    /// Media (audio/video).
    Media,

    /// Other/unknown type.
    #[default]
    Other,
}

impl ResourceType {
    /// Get short display name for the resource type (for table column).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Document => "doc",
            Self::Stylesheet => "css",
            Self::Script => "js",
            Self::Image => "img",
            Self::Font => "font",
            Self::Json => "json",
            Self::Xml => "xml",
            Self::Xhr => "xhr",
            Self::WebSocket => "ws",
            Self::Media => "media",
            Self::Other => "other",
        }
    }

    /// Get full display name for the resource type (for detail panes).
    pub fn full_name(self) -> &'static str {
        match self {
            Self::Document => "Document",
            Self::Stylesheet => "Stylesheet",
            Self::Script => "Script",
            Self::Image => "Image",
            Self::Font => "Font",
            Self::Json => "JSON",
            Self::Xml => "XML",
            Self::Xhr => "XHR",
            Self::WebSocket => "WebSocket",
            Self::Media => "Media",
            Self::Other => "Other",
        }
    }

    /// Infer resource type from content-type header and URL.
    pub fn from_content_type_and_url(content_type: Option<&str>, url: &str) -> Self {
        // First try content-type header
        if let Some(ct) = content_type {
            let ct = ct.to_lowercase();
            if ct.contains("text/html") {
                return Self::Document;
            }
            if ct.contains("text/css") {
                return Self::Stylesheet;
            }
            if ct.contains("javascript") || ct.contains("ecmascript") {
                return Self::Script;
            }
            if ct.starts_with("image/") {
                return Self::Image;
            }
            if ct.contains("font")
                || ct.contains("woff")
                || ct.contains("ttf")
                || ct.contains("otf")
            {
                return Self::Font;
            }
            if ct.contains("json") {
                return Self::Json;
            }
            if ct.contains("xml") {
                return Self::Xml;
            }
            if ct.starts_with("audio/") || ct.starts_with("video/") {
                return Self::Media;
            }
        }

        // Fall back to URL extension
        let path = url.split('?').next().unwrap_or(url);
        if let Some(ext) = path.rsplit('.').next() {
            let ext = ext.to_lowercase();
            match ext.as_str() {
                "html" | "htm" => return Self::Document,
                "css" => return Self::Stylesheet,
                "js" | "mjs" => return Self::Script,
                "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp" => {
                    return Self::Image;
                }
                "woff" | "woff2" | "ttf" | "otf" | "eot" => return Self::Font,
                "json" => return Self::Json,
                "xml" => return Self::Xml,
                "mp3" | "mp4" | "webm" | "ogg" | "wav" | "avi" | "mkv" => return Self::Media,
                _ => {}
            }
        }

        Self::Other
    }
}

/// Data for updating an exchange row when a response arrives.
#[derive(Debug, Clone, Default)]
pub struct ResponseUpdate {
    /// HTTP status code.
    pub status: Option<u16>,

    /// Response body size in bytes.
    pub size: Option<usize>,

    /// Request duration in milliseconds.
    pub time_ms: Option<u64>,

    /// Resource type (inferred from content-type).
    pub resource_type: ResourceType,

    /// Full response details.
    pub response: Option<ResponseDetail>,

    /// Timing breakdown.
    pub timings: Option<TimingDetail>,
}

impl ResponseUpdate {
    /// Extract response data from a completed exchange.
    pub fn from_exchange(exchange: &Exchange) -> Self {
        if let Some(resp) = &exchange.response {
            let timing_detail = resp.timings.as_ref().map(|t| TimingDetail {
                blocked_ms: t.blocked_ms,
                connect_ms: t.connect_ms,
                ssl_ms: t.ssl_ms,
                send_ms: Some(t.send_ms),
                wait_ms: Some(t.wait_ms),
                receive_ms: Some(t.receive_ms),
                connection_reused: t.connection_reused,
                total_ms: Some(resp.duration_ms),
            });

            let content_type = resp
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .map(|(_, value)| value.as_str());

            let url = exchange.request.url.to_string();
            let resource_type = ResourceType::from_content_type_and_url(content_type, &url);

            let response_detail = ResponseDetail {
                status: resp.status,
                reason: resp.reason.clone(),
                version: resp.version.clone(),
                headers: resp.headers.clone(),
                body: truncate_for_display(&resp.body_decompressed),
                body_len: resp.body_decompressed.len(),
                remote_addr: resp.remote_addr.map(|addr| addr.to_string()),
            };

            Self {
                status: Some(resp.status),
                size: Some(resp.body.len()),
                time_ms: Some(resp.duration_ms),
                resource_type,
                response: Some(response_detail),
                timings: timing_detail,
            }
        } else {
            let url = exchange.request.url.to_string();
            Self {
                resource_type: ResourceType::from_content_type_and_url(None, &url),
                ..Default::default()
            }
        }
    }
}

/// A row in the exchange table.
#[derive(Debug, Clone)]
pub struct ExchangeRow {
    /// Unique request ID (used internally for tracking, not displayed).
    #[allow(dead_code)]
    pub id: u64,

    /// HTTP method (GET, POST, etc.).
    pub method: String,

    /// Request URL.
    pub url: String,

    /// Resource type (inferred from content-type or URL).
    pub resource_type: ResourceType,

    /// HTTP status code (None if pending/failed).
    pub status: Option<u16>,

    /// Response body size in bytes.
    pub size: Option<usize>,

    /// Request duration in milliseconds.
    pub time_ms: Option<u64>,

    /// Full request details for detail view.
    pub request: RequestDetail,

    /// Full response details for detail view (if available).
    pub response: Option<ResponseDetail>,

    /// Timing breakdown for detail view (if available).
    pub timings: Option<TimingDetail>,
}

impl ExchangeRow {
    /// Format the status for display.
    pub fn status_display(&self) -> String {
        self.status.map_or_else(|| "-".into(), |s| s.to_string())
    }

    /// Format the size for display (human-readable).
    pub fn size_display(&self) -> String {
        match self.size {
            Some(size) if size >= 1_048_576 => format!("{:.1} MB", size as f64 / 1_048_576.0),
            Some(size) if size >= 1024 => format!("{:.1} KB", size as f64 / 1024.0),
            Some(size) => format!("{size} B"),
            None => "-".into(),
        }
    }

    /// Format the time for display.
    pub fn time_display(&self) -> String {
        match self.time_ms {
            Some(ms) if ms >= 1000 => format!("{:.2} s", ms as f64 / 1000.0),
            Some(ms) => format!("{ms} ms"),
            None => "-".into(),
        }
    }

    /// Format the resource type for display.
    pub fn type_display(&self) -> &'static str {
        self.resource_type.as_str()
    }

    /// Create a pending row from a request (before response arrives).
    pub fn from_request(request: &Request) -> Self {
        let url = request.url.to_string();

        let request_detail = RequestDetail {
            method: request.method.clone(),
            url: url.clone(),
            version: request.version.clone(),
            headers: request.headers.clone(),
            body: truncate_for_display(&request.body),
            body_len: request.body.len(),
        };

        Self {
            id: request.id,
            method: request.method.clone(),
            url,
            resource_type: ResourceType::Other, // Will be updated when response arrives
            status: None,
            size: None,
            time_ms: None,
            request: request_detail,
            response: None,
            timings: None,
        }
    }

    /// Create a complete row from a full exchange (request + response).
    ///
    /// Used when loading exchanges from a HAR file.
    pub fn from_exchange(exchange: &Exchange) -> Self {
        let mut row = Self::from_request(&exchange.request);

        let update = ResponseUpdate::from_exchange(exchange);
        row.status = update.status;
        row.size = update.size;
        row.time_ms = update.time_ms;
        row.resource_type = update.resource_type;
        row.response = update.response;
        row.timings = update.timings;

        row
    }
}

/// Application state.
pub struct App {
    /// Whether the application should quit.
    pub should_quit: bool,

    /// Table state for selection.
    pub table_state: TableState,

    /// Scrollbar state.
    pub scrollbar_state: ScrollbarState,

    /// List of exchanges.
    pub exchanges: Vec<ExchangeRow>,

    /// Session for HAR export (if enabled).
    pub session: Option<Session>,

    /// Whether the detail pane is visible.
    pub show_detail: bool,

    /// Which pane is currently focused.
    pub focus: FocusPane,

    /// Index of the exchange being viewed in detail (if any).
    pub detail_index: Option<usize>,

    /// Current tab in the request pane.
    pub request_tab: RequestTab,

    /// Current tab in the response pane.
    pub response_tab: ResponseTab,

    /// Scroll position for request pane content.
    pub request_scroll: u16,

    /// Maximum scroll position for request pane content (set during render).
    pub request_max_scroll: u16,

    /// Scroll position for response pane content.
    pub response_scroll: u16,

    /// Maximum scroll position for response pane content (set during render).
    pub response_max_scroll: u16,

    /// Whether the filter input is active.
    pub filter_active: bool,

    /// Current filter text (filters URLs).
    pub filter_text: String,

    /// Indices of exchanges that match the current filter.
    pub filtered_indices: Vec<usize>,

    /// Whether to automatically scroll to new rows as they appear.
    /// Disabled when user manually scrolls away from the bottom.
    pub auto_scroll: bool,

    /// Most rows kept. Older ones are dropped, as the session drops its
    /// oldest exchanges.
    pub max_rows: usize,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// Create a new application state.
    pub fn new() -> Self {
        Self {
            should_quit: false,
            table_state: TableState::default(),
            scrollbar_state: ScrollbarState::default(),
            exchanges: Vec::new(),
            session: None,
            show_detail: false,
            focus: FocusPane::default(),
            detail_index: None,
            request_tab: RequestTab::default(),
            response_tab: ResponseTab::default(),
            request_scroll: 0,
            request_max_scroll: 0,
            response_scroll: 0,
            response_max_scroll: 0,
            filter_active: false,
            filter_text: String::new(),
            filtered_indices: Vec::new(),
            auto_scroll: true,
            max_rows: mira_proxy::DEFAULT_MAX_EXCHANGES,
        }
    }

    /// Create an application state that keeps at most `max_rows` rows.
    pub fn with_max_rows(max_rows: usize) -> Self {
        Self {
            max_rows: max_rows.max(1),
            ..Self::new()
        }
    }

    /// Set the session for HAR export.
    pub fn set_session(&mut self, session: Session) {
        self.session = Some(session);
    }

    /// Add an exchange to the table.
    pub fn add_exchange(&mut self, exchange: ExchangeRow) {
        let index = self.exchanges.len();
        if !self.filter_text.is_empty() {
            let filter_lower = self.filter_text.to_lowercase();
            if exchange.url.to_lowercase().contains(&filter_lower) {
                self.filtered_indices.push(index);
            }
        }
        self.exchanges.push(exchange);
        self.trim_rows();

        let len = if self.filter_text.is_empty() {
            self.exchanges.len()
        } else {
            self.filtered_indices.len()
        };
        self.scrollbar_state = self.scrollbar_state.content_length(len);

        if self.auto_scroll && !self.show_detail && len > 0 {
            let last_idx = len - 1;
            self.table_state.select(Some(last_idx));
            self.scrollbar_state = self.scrollbar_state.position(last_idx);
        }
    }

    /// Drop the oldest rows once there are more than `max_rows`.
    ///
    /// Rows go in batches of a tenth of the cap, so the shift of every
    /// remaining row is paid once per batch rather than once per row.
    fn trim_rows(&mut self) {
        let slack = (self.max_rows / 10).max(1);
        if self.exchanges.len() <= self.max_rows.saturating_add(slack) {
            return;
        }
        let dropped = self.exchanges.len() - self.max_rows;
        self.exchanges.drain(..dropped);

        // Indices into `exchanges` shift down; those that pointed at a
        // dropped row go.
        let selection_shift = if self.filter_text.is_empty() {
            dropped
        } else {
            self.filtered_indices
                .iter()
                .filter(|&&i| i < dropped)
                .count()
        };
        self.filtered_indices.retain(|&i| i >= dropped);
        for index in &mut self.filtered_indices {
            *index -= dropped;
        }

        match self.detail_index {
            Some(index) if index >= dropped => self.detail_index = Some(index - dropped),
            Some(_) => self.close_detail(),
            None => {}
        }

        if let Some(selected) = self.table_state.selected() {
            let selected = selected.saturating_sub(selection_shift);
            self.table_state.select(Some(selected));
            self.scrollbar_state = self.scrollbar_state.position(selected);
        }
    }

    /// Update an existing exchange with response data.
    ///
    /// Finds the exchange by request ID and updates it with response
    /// information. This is used to update pending requests when their
    /// responses arrive.
    pub fn update_exchange(&mut self, id: u64, update: ResponseUpdate) {
        // Responses arrive for recent requests, so search from the end.
        if let Some(exchange) = self.exchanges.iter_mut().rev().find(|e| e.id == id) {
            exchange.status = update.status;
            exchange.size = update.size;
            exchange.time_ms = update.time_ms;
            exchange.resource_type = update.resource_type;
            exchange.response = update.response;
            exchange.timings = update.timings;
        }
    }

    /// Clear all exchanges from the table and session.
    pub fn clear_exchanges(&mut self) {
        self.exchanges.clear();
        self.filtered_indices.clear();
        self.filter_text.clear();
        self.filter_active = false;
        self.table_state.select(None);
        self.scrollbar_state = self.scrollbar_state.content_length(0);
        self.close_detail();
        // Also clear the session so HAR export reflects the cleared state
        if let Some(session) = &self.session {
            session.clear();
        }
    }

    /// Select the next row.
    pub fn next_row(&mut self) {
        let count = self.visible_count();
        if count == 0 {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i >= count - 1 {
                    count - 1 // Stop at last row
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
        self.scrollbar_state = self.scrollbar_state.position(i);

        self.auto_scroll = i == count - 1;
    }

    /// Select the previous row.
    pub fn previous_row(&mut self) {
        let count = self.visible_count();
        if count == 0 {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i == 0 {
                    0 // Stop at first row
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
        self.scrollbar_state = self.scrollbar_state.position(i);

        self.auto_scroll = i == count - 1;
    }

    /// Jump to the last row and enable auto-scroll.
    pub fn scroll_to_bottom(&mut self) {
        let count = self.visible_count();
        if count == 0 {
            return;
        }
        let last = count - 1;
        self.table_state.select(Some(last));
        self.scrollbar_state = self.scrollbar_state.position(last);
        self.auto_scroll = true;
    }

    /// Request to quit the application.
    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Toggle the detail pane for the currently selected row.
    #[allow(dead_code)]
    pub fn toggle_detail(&mut self) {
        if self.show_detail {
            self.close_detail();
        } else if let Some(selected) = self.table_state.selected() {
            self.show_detail = true;
            self.focus = FocusPane::Request;
            self.detail_index = Some(selected);
        }
    }

    /// Close the detail pane.
    pub fn close_detail(&mut self) {
        self.show_detail = false;
        self.focus = FocusPane::Table;
        self.detail_index = None;
        self.request_tab = RequestTab::default();
        self.response_tab = ResponseTab::default();
        self.request_scroll = 0;
        self.request_max_scroll = 0;
        self.response_scroll = 0;
        self.response_max_scroll = 0;
    }

    /// Cycle focus to the next pane.
    pub fn next_focus(&mut self) {
        if !self.show_detail {
            return;
        }
        self.focus = self.focus.next();
    }

    /// Cycle focus to the previous pane.
    pub fn prev_focus(&mut self) {
        if !self.show_detail {
            return;
        }
        self.focus = self.focus.prev();
    }

    /// Focus on the traffic table.
    #[allow(dead_code)]
    pub fn focus_table(&mut self) {
        self.focus = FocusPane::Table;
    }

    /// Focus on the request pane.
    pub fn focus_request(&mut self) {
        if self.show_detail {
            self.focus = FocusPane::Request;
        }
    }

    /// Focus on the response pane.
    #[allow(dead_code)]
    pub fn focus_response(&mut self) {
        if self.show_detail {
            self.focus = FocusPane::Response;
        }
    }

    /// Navigate to next tab in the currently focused pane.
    pub fn next_tab(&mut self) {
        match self.focus {
            FocusPane::Request => {
                self.request_tab = self.request_tab.next();
                self.request_scroll = 0;
            }
            FocusPane::Response => {
                self.response_tab = self.response_tab.next();
                self.response_scroll = 0;
            }
            FocusPane::Table => {}
        }
    }

    /// Navigate to previous tab in the currently focused pane.
    pub fn prev_tab(&mut self) {
        match self.focus {
            FocusPane::Request => {
                self.request_tab = self.request_tab.prev();
                self.request_scroll = 0;
            }
            FocusPane::Response => {
                self.response_tab = self.response_tab.prev();
                self.response_scroll = 0;
            }
            FocusPane::Table => {}
        }
    }

    /// Set request tab by index (1-4).
    pub fn set_request_tab(&mut self, index: usize) {
        self.request_tab = match index {
            1 => RequestTab::General,
            2 => RequestTab::Headers,
            3 => RequestTab::Query,
            4 => RequestTab::Body,
            _ => return,
        };
        self.request_scroll = 0;
    }

    /// Set response tab by index (1-5).
    pub fn set_response_tab(&mut self, index: usize) {
        self.response_tab = match index {
            1 => ResponseTab::General,
            2 => ResponseTab::Headers,
            3 => ResponseTab::Body,
            4 => ResponseTab::Cookies,
            5 => ResponseTab::Timings,
            _ => return,
        };
        self.response_scroll = 0;
    }

    /// Scroll down in the focused pane.
    pub fn scroll_down(&mut self, amount: u16) {
        match self.focus {
            FocusPane::Request => {
                self.request_scroll = self
                    .request_scroll
                    .saturating_add(amount)
                    .min(self.request_max_scroll);
            }
            FocusPane::Response => {
                self.response_scroll = self
                    .response_scroll
                    .saturating_add(amount)
                    .min(self.response_max_scroll);
            }
            FocusPane::Table => {}
        }
    }

    /// Scroll up in the focused pane.
    pub fn scroll_up(&mut self, amount: u16) {
        match self.focus {
            FocusPane::Request => {
                self.request_scroll = self.request_scroll.saturating_sub(amount);
            }
            FocusPane::Response => {
                self.response_scroll = self.response_scroll.saturating_sub(amount);
            }
            FocusPane::Table => {}
        }
    }

    /// Reset scroll position in the focused pane.
    pub fn reset_scroll(&mut self) {
        match self.focus {
            FocusPane::Request => {
                self.request_scroll = 0;
            }
            FocusPane::Response => {
                self.response_scroll = 0;
            }
            FocusPane::Table => {}
        }
    }

    /// Start filter input mode.
    pub fn start_filter(&mut self) {
        self.filter_active = true;
    }

    /// Cancel filter input and clear the filter.
    pub fn cancel_filter(&mut self) {
        self.filter_active = false;
        self.filter_text.clear();
        self.update_filtered_indices();
        self.table_state.select(if self.exchanges.is_empty() {
            None
        } else {
            Some(0)
        });
    }

    /// Confirm filter input (exit input mode but keep filter active).
    pub fn confirm_filter(&mut self) {
        self.filter_active = false;
        self.update_filtered_indices();
        if self.visible_count() > 0 {
            self.table_state.select(Some(0));
        } else {
            self.table_state.select(None);
        }
    }

    /// Add a character to the filter text.
    pub fn filter_push(&mut self, c: char) {
        self.filter_text.push(c);
        self.update_filtered_indices();
    }

    /// Remove the last character from the filter text.
    pub fn filter_pop(&mut self) {
        self.filter_text.pop();
        self.update_filtered_indices();
    }

    /// Update the filtered indices based on current filter text.
    pub fn update_filtered_indices(&mut self) {
        if self.filter_text.is_empty() {
            self.filtered_indices.clear();
            self.scrollbar_state = self.scrollbar_state.content_length(self.exchanges.len());
        } else {
            let filter_lower = self.filter_text.to_lowercase();
            self.filtered_indices = self
                .exchanges
                .iter()
                .enumerate()
                .filter(|(_, ex)| ex.url.to_lowercase().contains(&filter_lower))
                .map(|(i, _)| i)
                .collect();
            self.scrollbar_state = self
                .scrollbar_state
                .content_length(self.filtered_indices.len());
        }
    }

    /// Get the exchanges to display (filtered or all).
    pub fn visible_exchanges(&self) -> Vec<(usize, &ExchangeRow)> {
        if self.filter_text.is_empty() {
            self.exchanges.iter().enumerate().collect()
        } else {
            self.filtered_indices
                .iter()
                .filter_map(|&i| self.exchanges.get(i).map(|ex| (i, ex)))
                .collect()
        }
    }

    /// Get the real exchange index from the visible table index.
    pub fn real_index(&self, visible_index: usize) -> Option<usize> {
        if self.filter_text.is_empty() {
            if visible_index < self.exchanges.len() {
                Some(visible_index)
            } else {
                None
            }
        } else {
            self.filtered_indices.get(visible_index).copied()
        }
    }

    /// Check if filter is currently showing results (non-empty filter text).
    pub fn has_filter(&self) -> bool {
        !self.filter_text.is_empty()
    }

    /// Get the count of visible exchanges (filtered or all).
    pub fn visible_count(&self) -> usize {
        if self.filter_text.is_empty() {
            self.exchanges.len()
        } else {
            self.filtered_indices.len()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mira_proxy::Encoding;
    use mira_proxy::Response;

    /// A row for request `id` at `http://example.com/{path}`.
    fn row(id: u64, path: &str) -> ExchangeRow {
        let mut exchange = exchange_with_body(0);
        exchange.request.id = id;
        exchange.request.url = format!("http://example.com/{path}")
            .parse()
            .expect("valid url");
        ExchangeRow::from_exchange(&exchange)
    }

    #[test]
    fn test_rows_are_capped_and_indices_follow() {
        let mut app = App::with_max_rows(10);
        for id in 0..10 {
            app.add_exchange(row(id, if id % 2 == 0 { "even" } else { "odd" }));
        }
        app.filter_text = "even".to_owned();
        app.update_filtered_indices();
        // Viewing request 8, the last even row.
        app.detail_index = Some(8);
        app.show_detail = true;

        // One row over the cap plus slack forces a trim.
        app.add_exchange(row(10, "even"));
        app.add_exchange(row(11, "odd"));

        assert_eq!(app.exchanges.len(), 10, "the oldest rows go");
        assert_eq!(app.exchanges[0].id, 2);
        assert_eq!(
            app.detail_index.map(|i| app.exchanges[i].id),
            Some(8),
            "the detail view still shows the same request"
        );
        let visible: Vec<u64> = app.visible_exchanges().iter().map(|(_, r)| r.id).collect();
        assert_eq!(visible, vec![2, 4, 6, 8, 10]);
    }

    #[test]
    fn test_detail_closes_when_its_row_is_dropped() {
        let mut app = App::with_max_rows(10);
        for id in 0..10 {
            app.add_exchange(row(id, "x"));
        }
        app.detail_index = Some(0);
        app.show_detail = true;
        app.add_exchange(row(10, "x"));
        app.add_exchange(row(11, "x"));
        assert!(!app.show_detail);
        assert_eq!(app.detail_index, None);
    }

    /// An exchange whose response body is `body_len` bytes of text.
    fn exchange_with_body(body_len: usize) -> Exchange {
        let body = Bytes::from(vec![b'x'; body_len]);

        Exchange {
            request: Request {
                id: 1,
                client_addr: "127.0.0.1:12345".parse().expect("valid addr"),
                method: "POST".into(),
                url: "http://example.com/upload".parse().expect("valid url"),
                version: "HTTP/1.1".into(),
                headers: vec![("Host".into(), "example.com".into())],
                body: body.clone(),
            },
            response: Some(Response {
                request_id: 1,
                status: 200,
                reason: "OK".into(),
                version: "HTTP/1.1".into(),
                headers: vec![("Content-Type".into(), "text/plain".into())],
                body: body.clone(),
                body_decompressed: body,
                encoding: Encoding::Identity,
                resource_type: mira_proxy::ResourceType::Other,
                duration_ms: 5,
                timings: None,
                remote_addr: None,
            }),
            error: None,
        }
    }

    #[test]
    fn test_row_keeps_small_bodies_whole() {
        let row = ExchangeRow::from_exchange(&exchange_with_body(1024));

        assert_eq!(row.request.body.len(), 1024);
        assert_eq!(row.request.body_len, 1024);
        let response = row.response.as_ref().expect("response");
        assert_eq!(response.body.len(), 1024);
        assert_eq!(response.body_len, 1024);
    }

    #[test]
    fn test_row_cuts_oversized_bodies_but_records_the_real_length() {
        let full = MAX_DISPLAY_BODY_BYTES * 4;
        let row = ExchangeRow::from_exchange(&exchange_with_body(full));

        assert_eq!(row.request.body.len(), MAX_DISPLAY_BODY_BYTES);
        assert_eq!(
            row.request.body_len, full,
            "the pane needs the real length to say what it is not showing"
        );
        let response = row.response.as_ref().expect("response");
        assert_eq!(response.body.len(), MAX_DISPLAY_BODY_BYTES);
        assert_eq!(response.body_len, full);
    }

    #[test]
    fn test_row_size_column_reports_the_full_body_size() {
        let full = MAX_DISPLAY_BODY_BYTES * 4;
        let row = ExchangeRow::from_exchange(&exchange_with_body(full));

        assert_eq!(
            row.size,
            Some(full),
            "the table shows what was transferred, not what the view kept"
        );
    }

    #[test]
    fn test_display_truncation_does_not_reach_the_har_export() {
        // The contract: the view holds a prefix, the export holds the
        // body the server actually sent.
        let full = MAX_DISPLAY_BODY_BYTES * 4;
        let exchange = exchange_with_body(full);

        let row = ExchangeRow::from_exchange(&exchange);
        let session = Session::new();
        session.add(exchange);

        assert_eq!(
            row.response.as_ref().expect("response").body.len(),
            MAX_DISPLAY_BODY_BYTES
        );

        let har = session.to_har();
        let entry = &har.log.entries[0];
        assert_eq!(
            entry.response.content.size,
            i64::try_from(full).expect("fits"),
            "the HAR must report the whole body"
        );
        assert_eq!(
            entry.response.content.text.as_ref().map(String::len),
            Some(full),
            "and carry all of it"
        );
        assert_eq!(
            entry.request.body_size,
            i64::try_from(full).expect("fits"),
            "request bodies are exported whole too"
        );
    }
}
