//! What the window is showing, shared by its views.
//!
//! [`Nav`] is where the window is: the page, the selected exchange, the
//! inspector, and the filters other views hand to Network. [`StatusLine`]
//! is what the status bar says: the hint for whatever is under the pointer,
//! or a short-lived message after an action.

use crate::capture::Capture;
use crate::proxy::ProxyControl;
use gpui_kit::Context;
use gpui_kit::Entity;
use gpui_kit::SharedString;
use gpui_kit::Task;
use std::time::Duration;

/// How long a message stays in the status bar.
const MESSAGE_TIME: Duration = Duration::from_secs(4);

/// The handles every view holds: the capture, where the window is, the
/// status line, and the proxy.
#[derive(Clone)]
pub struct Shared {
    pub capture: Entity<Capture>,
    pub nav: Entity<Nav>,
    pub status: Entity<StatusLine>,
    pub proxy: Entity<ProxyControl>,
}

/// The pages of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Overview,
    Network,
    Hosts,
    Search,
    Body,
    Settings,
}

/// Which body the Body page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Request,
    Response,
}

/// Where the window is.
pub struct Nav {
    page: Page,
    selected: Option<u64>,
    inspector: bool,
    side: Side,
    host_filter: Option<String>,
    query: String,
    search_generation: u64,
}

impl Nav {
    pub fn new() -> Self {
        Self {
            page: Page::Network,
            selected: None,
            inspector: false,
            side: Side::Response,
            host_filter: None,
            query: String::new(),
            search_generation: 0,
        }
    }

    pub fn page(&self) -> Page {
        self.page
    }

    pub fn selected(&self) -> Option<u64> {
        self.selected
    }

    pub fn inspector(&self) -> bool {
        self.inspector
    }

    pub fn side(&self) -> Side {
        self.side
    }

    pub fn host_filter(&self) -> Option<&str> {
        self.host_filter.as_deref()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Changes with every search, so searching the same text again re-runs it.
    pub fn search_generation(&self) -> u64 {
        self.search_generation
    }

    pub fn show(&mut self, page: Page, cx: &mut Context<'_, Self>) {
        if self.page != page {
            self.page = page;
            cx.notify();
        }
    }

    pub fn select(&mut self, id: Option<u64>, cx: &mut Context<'_, Self>) {
        if self.selected != id {
            self.selected = id;
            cx.notify();
        }
    }

    /// Select `id` and show it in the inspector, opening the inspector if
    /// it was hidden. What a click on a request does.
    pub fn inspect(&mut self, id: u64, cx: &mut Context<'_, Self>) {
        self.selected = Some(id);
        self.inspector = true;
        cx.notify();
    }

    pub fn toggle_inspector(&mut self, cx: &mut Context<'_, Self>) {
        self.inspector = !self.inspector;
        cx.notify();
    }

    /// Open one of an exchange's bodies on the Body page.
    pub fn open_body(&mut self, id: u64, side: Side, cx: &mut Context<'_, Self>) {
        self.selected = Some(id);
        self.side = side;
        self.page = Page::Body;
        cx.notify();
    }

    pub fn set_side(&mut self, side: Side, cx: &mut Context<'_, Self>) {
        if self.side != side {
            self.side = side;
            cx.notify();
        }
    }

    /// Show Network with only `host`'s requests, or every host's.
    pub fn filter_host(&mut self, host: Option<String>, cx: &mut Context<'_, Self>) {
        self.host_filter = host;
        self.page = Page::Network;
        cx.notify();
    }

    /// Search for `query` and show the results.
    pub fn search(&mut self, query: String, cx: &mut Context<'_, Self>) {
        self.query = query;
        self.search_generation += 1;
        self.page = Page::Search;
        cx.notify();
    }
}

/// How a message reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Error,
}

/// What the status bar says.
pub struct StatusLine {
    hint: Option<SharedString>,
    message: Option<(Tone, SharedString)>,
    expiry: Option<Task<()>>,
}

impl StatusLine {
    pub fn new() -> Self {
        Self {
            hint: None,
            message: None,
            expiry: None,
        }
    }

    pub fn hint(&self) -> Option<&SharedString> {
        self.hint.as_ref()
    }

    pub fn message(&self) -> Option<&(Tone, SharedString)> {
        self.message.as_ref()
    }

    /// The pointer moved onto, or off, something with a hint.
    pub fn set_hint(&mut self, hint: Option<SharedString>, cx: &mut Context<'_, Self>) {
        if self.hint != hint {
            self.hint = hint;
            cx.notify();
        }
    }

    /// Say `text` for a few seconds.
    pub fn say(&mut self, tone: Tone, text: impl Into<SharedString>, cx: &mut Context<'_, Self>) {
        self.message = Some((tone, text.into()));
        self.expiry = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MESSAGE_TIME).await;
            let _ = this.update(cx, |this, cx| {
                this.message = None;
                cx.notify();
            });
        }));
        cx.notify();
    }
}
