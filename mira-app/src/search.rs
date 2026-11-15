//! Search through every URL, header, and body at once.
//!
//! Searching reads every body, so it runs off the main thread, when a query
//! is submitted rather than on every keystroke.

use crate::capture::Entry;
use crate::format;
use crate::icons::Icon;
use crate::state::Shared;
use crate::state::Side;
use crate::text;
use crate::theme::MONO;
use crate::theme::colors;
use crate::ui;
use gpui_kit::Context;
use gpui_kit::HighlightStyle;
use gpui_kit::InteractiveElement as _;
use gpui_kit::IntoElement;
use gpui_kit::ParentElement as _;
use gpui_kit::Render;
use gpui_kit::SharedString;
use gpui_kit::Styled as _;
use gpui_kit::StyledText;
use gpui_kit::Subscription;
use gpui_kit::Task;
use gpui_kit::UniformListScrollHandle;
use gpui_kit::Window;
use gpui_kit::base::Scrollbar;
use gpui_kit::base::ScrollbarMode;
use gpui_kit::base::h_flex;
use gpui_kit::div;
use gpui_kit::rems;
use gpui_kit::uniform_list;
use std::rc::Rc;
use std::sync::Arc;

/// The most hits a search keeps.
const MAX_HITS: usize = 2_000;

/// How much text a snippet shows either side of the match.
const CONTEXT_BYTES: usize = 48;

/// Where in an exchange a match was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Url,
    RequestHeader,
    ResponseHeader,
    RequestBody,
    ResponseBody,
}

impl Field {
    fn caption(self) -> &'static str {
        match self {
            Self::Url => "URL",
            Self::RequestHeader => "REQ HEADER",
            Self::ResponseHeader => "RES HEADER",
            Self::RequestBody => "REQ BODY",
            Self::ResponseBody => "RES BODY",
        }
    }

    fn side(self) -> Option<Side> {
        match self {
            Self::RequestBody => Some(Side::Request),
            Self::ResponseBody => Some(Side::Response),
            _ => None,
        }
    }
}

/// One match.
struct Hit {
    entry: Arc<Entry>,
    field: Field,
    /// The text around the match, and where the match is within it.
    snippet: String,
    matched: std::ops::Range<usize>,
}

/// Where a search stands.
enum Results {
    Idle,
    Searching,
    Done(Rc<Vec<Hit>>),
}

/// The Search page.
pub struct SearchView {
    shared: Shared,
    results: Results,
    searched: Option<u64>,
    task: Option<Task<()>>,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl SearchView {
    pub fn new(shared: Shared, cx: &mut Context<'_, Self>) -> Self {
        let subscriptions = vec![cx.observe(&shared.nav, |this, _, cx| this.start(cx))];
        Self {
            shared,
            results: Results::Idle,
            searched: None,
            task: None,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Start a search if a new one was asked for.
    fn start(&mut self, cx: &mut Context<'_, Self>) {
        let nav = self.shared.nav.read(cx);
        let generation = nav.search_generation();
        if self.searched == Some(generation) {
            return;
        }
        self.searched = Some(generation);
        let needle = nav.query().trim().to_owned();
        if needle.is_empty() {
            self.results = Results::Idle;
            self.task = None;
            cx.notify();
            return;
        }
        let entries: Vec<Arc<Entry>> = self.shared.capture.read(cx).entries().to_vec();
        self.results = Results::Searching;
        let search = cx
            .background_executor()
            .spawn(async move { find(&entries, &needle) });
        self.task = Some(cx.spawn(async move |this, cx| {
            let hits = search.await;
            let _ = this.update(cx, |this, cx| {
                this.results = Results::Done(Rc::new(hits));
                cx.notify();
            });
        }));
        cx.notify();
    }
}

/// Every place `needle` appears in `entries`, ignoring ASCII case.
fn find(entries: &[Arc<Entry>], needle: &str) -> Vec<Hit> {
    let needle = needle.to_ascii_lowercase();
    let mut hits = Vec::new();
    for entry in entries {
        let mut look = |field: Field, haystack: &str| {
            if hits.len() < MAX_HITS
                && let Some((snippet, matched)) = snippet(haystack, &needle)
            {
                hits.push(Hit {
                    entry: Arc::clone(entry),
                    field,
                    snippet,
                    matched,
                });
            }
        };
        look(Field::Url, entry.request.url.as_str());
        if let Some(header) = entry
            .request
            .headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}"))
            .find(|line| line.to_ascii_lowercase().contains(&needle))
        {
            look(Field::RequestHeader, &header);
        }
        if let Some(response) = &entry.response
            && let Some(header) = response
                .headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}"))
                .find(|line| line.to_ascii_lowercase().contains(&needle))
        {
            look(Field::ResponseHeader, &header);
        }
        if !entry.request.body.is_empty() && !text::is_binary(&entry.request.body) {
            look(
                Field::RequestBody,
                &String::from_utf8_lossy(&entry.request.body),
            );
        }
        if let Some(response) = &entry.response
            && !response.body_decompressed.is_empty()
            && !text::is_binary(&response.body_decompressed)
        {
            look(
                Field::ResponseBody,
                &String::from_utf8_lossy(&response.body_decompressed),
            );
        }
        if hits.len() >= MAX_HITS {
            break;
        }
    }
    hits
}

/// The text around the first match of `needle` in `haystack`, on one line,
/// with invisible characters made visible, and where the match falls in it.
fn snippet(haystack: &str, needle: &str) -> Option<(String, std::ops::Range<usize>)> {
    // ASCII lowering keeps byte offsets, so a match in the lowered text is at
    // the same place in the original.
    let at = haystack.to_ascii_lowercase().find(needle)?;
    let mut start = at.saturating_sub(CONTEXT_BYTES);
    while !haystack.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (at + needle.len() + CONTEXT_BYTES).min(haystack.len());
    while !haystack.is_char_boundary(end) {
        end += 1;
    }
    let one_line = |text: &str| text::visible(&text.replace(['\n', '\r', '\t'], " "));
    let before = one_line(&haystack[start..at]);
    let matched = one_line(&haystack[at..at + needle.len()]);
    let after = one_line(&haystack[at + needle.len()..end]);
    let lead = if start > 0 { "…" } else { "" };
    let tail = if end < haystack.len() { "…" } else { "" };
    let from = lead.len() + before.len();
    let range = from..from + matched.len();
    Some((format!("{lead}{before}{matched}{after}{tail}"), range))
}

impl Render for SearchView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let c = colors();
        let query = self.shared.nav.read(cx).query().trim().to_owned();
        let (meta, body): (Option<String>, gpui_kit::AnyElement) = match &self.results {
            Results::Idle => (
                None,
                ui::empty_state(
                    Icon::Search,
                    "Search the capture",
                    "Type in the sidebar's search field and press Enter. URLs, headers, and the text of every body are searched.",
                )
                .into_any_element(),
            ),
            Results::Searching => (
                Some("Searching…".to_owned()),
                div().flex_1().into_any_element(),
            ),
            Results::Done(hits) if hits.is_empty() => (
                Some("No matches".to_owned()),
                ui::empty_state(
                    Icon::Search,
                    "Nothing matches",
                    "Searches ignore case. Bodies that look binary are skipped.",
                )
                .into_any_element(),
            ),
            Results::Done(hits) => {
                let mut requests: Vec<u64> = hits.iter().map(|h| h.entry.id()).collect();
                requests.dedup();
                let meta = format!(
                    "{}{} in {}",
                    format::plural(hits.len(), "match"),
                    if hits.len() >= MAX_HITS { "+" } else { "" },
                    format::plural(requests.len(), "request")
                );
                let rows = Rc::clone(hits);
                let nav = self.shared.nav.downgrade();
                let list = div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .bg(c.panel)
                    .child(
                        uniform_list("search-rows", rows.len(), move |range, _, _| {
                            range
                                .filter_map(|at| rows.get(at).map(|hit| (at, hit)))
                                .map(|(at, hit)| {
                                    let nav = nav.clone();
                                    let id = hit.entry.id();
                                    let side = hit.field.side();
                                    ui::row_button(("search-hit", at))
                                        .accessibility_label(hit.field.caption())
                                        .w_full()
                                        .h(rems(2.75))
                                        .px(rems(1.0))
                                        .flex()
                                        .flex_col()
                                        .items_start()
                                        .justify_center()
                                        .gap(rems(0.25))
                                        .border_b_1()
                                        .border_color(c.line_soft.opacity(0.7))
                                        .hover(|style| style.bg(c.hover))
                                        .child(
                                            h_flex()
                                                .w_full()
                                                .gap(rems(0.5))
                                                .child(ui::plain_tag(hit.field.caption()))
                                                .child(ui::status_tag(&hit.entry))
                                                .child(
                                                    ui::url_line(&hit.entry)
                                                        .flex_1()
                                                        .text_size(crate::theme::text::LABEL),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .w_full()
                                                .truncate()
                                                .font_family(MONO)
                                                .text_size(crate::theme::text::LABEL)
                                                .text_color(c.ink_2)
                                                .child(
                                                    StyledText::new(hit.snippet.clone())
                                                        .with_highlights([(
                                                            hit.matched.clone(),
                                                            HighlightStyle {
                                                                color: Some(c.void),
                                                                background_color: Some(c.warn),
                                                                ..HighlightStyle::default()
                                                            },
                                                        )]),
                                                ),
                                        )
                                        .on_click(move |_, _, cx| {
                                            let _ = nav.update(cx, |nav, cx| match side {
                                                Some(side) => nav.open_body(id, side, cx),
                                                None => nav.inspect(id, cx),
                                            });
                                        })
                                })
                                .collect()
                        })
                        .track_scroll(&self.scroll)
                        .size_full(),
                    )
                    .child(Scrollbar::vertical(&self.scroll).mode(ScrollbarMode::Hover));
                (Some(meta), list.into_any_element())
            }
        };
        let title = if query.is_empty() {
            SharedString::from("Search")
        } else {
            SharedString::from(format!("“{query}”"))
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .child(ui::page_header(title, meta))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snippet_marks_the_match_in_context() {
        let (text, range) = snippet("hello World", "world").expect("found");
        assert_eq!(text, "hello World");
        assert_eq!(&text[range], "World");
    }

    #[test]
    fn test_snippet_trims_long_text_on_char_boundaries() {
        let haystack = format!("{}needle{}", "é".repeat(60), "x".repeat(60));
        let (text, range) = snippet(&haystack, "needle").expect("found");
        assert!(text.starts_with('…') && text.ends_with('…'));
        assert_eq!(&text[range], "needle");
    }

    #[test]
    fn test_snippet_keeps_to_one_line() {
        let (text, _) = snippet("a\nb needle c\nd", "needle").expect("found");
        assert!(!text.contains('\n'));
    }
}
