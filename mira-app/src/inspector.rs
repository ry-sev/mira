//! The inspector: everything captured about the selected exchange, in
//! collapsible blocks beside the page.

use crate::capture::Entry;
use crate::format;
use crate::icons::Icon;
use crate::icons::icon;
use crate::site;
use crate::state::Shared;
use crate::state::Side;
use crate::state::Tone;
use crate::theme::MONO;
use crate::theme::colors;
use crate::theme::phase_colors;
use crate::theme::text;
use crate::ui;
use gpui_kit::AnyElement;
use gpui_kit::ClipboardItem;
use gpui_kit::Context;
use gpui_kit::Div;
use gpui_kit::Hsla;
use gpui_kit::InteractiveElement as _;
use gpui_kit::IntoElement;
use gpui_kit::ParentElement as _;
use gpui_kit::Render;
use gpui_kit::ScrollHandle;
use gpui_kit::SharedString;
use gpui_kit::Styled as _;
use gpui_kit::Subscription;
use gpui_kit::Window;
use gpui_kit::base::Button;
use gpui_kit::base::h_flex;
use gpui_kit::div;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::relative;
use gpui_kit::rems;
use mira_proxy::Timings;
use std::collections::HashSet;

/// The blocks of the inspector, each of which can be folded away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Block {
    Timing,
    Query,
    Body,
    ResponseHeaders,
    RequestHeaders,
}

impl Block {
    fn title(self) -> &'static str {
        match self {
            Self::Timing => "Timing",
            Self::Query => "Query",
            Self::Body => "Body",
            Self::ResponseHeaders => "Response headers",
            Self::RequestHeaders => "Request headers",
        }
    }
}

/// The inspector panel.
pub struct InspectorView {
    shared: Shared,
    closed: HashSet<Block>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl InspectorView {
    pub fn new(shared: Shared, cx: &mut Context<'_, Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&shared.capture, |_, _, cx| cx.notify()),
            cx.observe(&shared.nav, |_, _, cx| cx.notify()),
        ];
        Self {
            shared,
            closed: HashSet::new(),
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    fn block(
        &self,
        which: Block,
        count: Option<usize>,
        content: impl FnOnce() -> AnyElement,
        cx: &Context<'_, Self>,
    ) -> Div {
        let c = colors();
        let open = !self.closed.contains(&which);
        let view = cx.entity().downgrade();
        div()
            .border_b_1()
            .border_color(c.line_soft)
            .child(
                Button::new(("inspector-block", which as usize))
                    .accessibility_label(which.title())
                    .w_full()
                    .h(rems(2.0))
                    .px(rems(1.0))
                    .flex()
                    .items_center()
                    .justify_start()
                    .gap(rems(0.375))
                    .hover(|style| style.bg(c.hover))
                    .child(
                        icon(
                            if open {
                                Icon::ChevronDown
                            } else {
                                Icon::ChevronRight
                            },
                            rems(0.6875),
                        )
                        .text_color(c.ink_4),
                    )
                    .child(ui::label(which.title()).text_color(c.ink_2))
                    .when_some(count, |this, count| {
                        this.child(
                            div()
                                .font_family(MONO)
                                .text_size(text::MICRO)
                                .text_color(c.ink_4)
                                .child(format::count(count)),
                        )
                    })
                    .on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if !this.closed.remove(&which) {
                                this.closed.insert(which);
                            }
                            cx.notify();
                        });
                    }),
            )
            .when(open, |this| {
                this.child(div().px(rems(1.0)).pb(rems(0.75)).child(content()))
            })
    }

    fn render_detail(&self, entry: &Entry, cx: &Context<'_, Self>) -> Div {
        let query = site::params(&entry.request.url);
        let request_headers = entry.request.headers.clone();
        let response_headers = entry
            .response
            .as_ref()
            .map(|r| r.headers.clone())
            .unwrap_or_default();
        let timings = entry.response.as_ref().and_then(|r| r.timings.clone());
        let duration = entry.duration_ms();
        div()
            .child(self.render_summary(entry))
            .child(self.block(
                Block::Timing,
                None,
                || timing(timings.as_ref(), duration).into_any_element(),
                cx,
            ))
            .when(!query.is_empty(), |this| {
                let count = query.len();
                this.child(self.block(
                    Block::Query,
                    Some(count),
                    move || ui::pairs(&query).into_any_element(),
                    cx,
                ))
            })
            .child(self.block(
                Block::Body,
                None,
                || self.render_bodies(entry).into_any_element(),
                cx,
            ))
            .child(self.block(
                Block::ResponseHeaders,
                Some(response_headers.len()),
                move || headers(&response_headers).into_any_element(),
                cx,
            ))
            .child(self.block(
                Block::RequestHeaders,
                Some(request_headers.len()),
                move || headers(&request_headers).into_any_element(),
                cx,
            ))
    }

    fn render_summary(&self, entry: &Entry) -> Div {
        let c = colors();
        let status = self.shared.status.clone();
        let copy_status = status.downgrade();
        let copied = entry.request.url.to_string();
        let curl_status = status.downgrade();
        let curl_request = (entry.request.method != "CONNECT").then(|| entry.request.clone());
        let parts = site::parts(&entry.request.url);
        div()
            .relative()
            .px(rems(1.0))
            .pt(rems(0.75))
            .pb(rems(0.875))
            .border_b_1()
            .border_color(c.line_soft)
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(rems(0.125))
                    .bg(entry.color),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(rems(0.375))
                    .child(ui::tag(
                        SharedString::from(entry.request.method.clone()),
                        c.ink,
                        c.line,
                    ))
                    .child(ui::status_tag(entry))
                    .child(ui::plain_tag(entry.kind.short()))
                    .child(ui::plain_tag(SharedString::from(
                        entry
                            .response
                            .as_ref()
                            .map_or_else(|| entry.request.version.clone(), |r| r.version.clone()),
                    ))),
            )
            .child(
                h_flex()
                    .mt(rems(0.625))
                    .items_start()
                    .gap(rems(0.25))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(MONO)
                            .text_size(text::LABEL)
                            .child(
                                gpui_kit::StyledText::new(format!(
                                    "{}{}{}{}",
                                    parts.scheme, parts.host, parts.path, parts.query
                                ))
                                .with_highlights(url_highlights(&parts, entry.color)),
                            ),
                    )
                    .child(ui::icon_button(
                        "inspector-copy-url",
                        Icon::Copy,
                        "Copy URL",
                        &status,
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                            let _ =
                                copy_status.update(cx, |s, cx| s.say(Tone::Ok, "Copied URL", cx));
                        },
                    ))
                    .when_some(curl_request, |this, request| {
                        this.child(ui::icon_button(
                            "inspector-copy-curl",
                            Icon::Terminal,
                            "Copy as cURL",
                            &status,
                            move |_, _, cx| {
                                let curl = crate::curl::command(&request);
                                cx.write_to_clipboard(ClipboardItem::new_string(curl.command));
                                let message = if curl.omitted_body {
                                    "Copied as cURL, without the binary body"
                                } else {
                                    "Copied as cURL"
                                };
                                let _ =
                                    curl_status.update(cx, |s, cx| s.say(Tone::Ok, message, cx));
                            },
                        ))
                    }),
            )
            .when_some(entry.error.clone(), |this, error| {
                this.child(
                    h_flex()
                        .items_start()
                        .mt(rems(0.5))
                        .gap(rems(0.375))
                        .text_size(text::LABEL)
                        .line_height(relative(1.5))
                        .text_color(c.err)
                        // Centred on the first line of the message.
                        .child(
                            icon(Icon::Error, rems(0.75))
                                .mt(rems(0.1875))
                                .text_color(c.err),
                        )
                        .child(div().flex_1().min_w_0().font_family(MONO).child(error)),
                )
            })
            .child(
                h_flex()
                    .mt(rems(0.625))
                    .gap(rems(0.75))
                    .font_family(MONO)
                    .text_size(text::MICRO)
                    .text_color(c.ink_3)
                    .child(format::offset(entry.start_ms))
                    .when(entry.response.is_some(), |this| {
                        this.child(format::ms(entry.duration_ms()))
                            .child(format::bytes(entry.size()))
                    })
                    .when_some(
                        entry.response.as_ref().and_then(|r| r.remote_addr),
                        |this, addr| this.child(div().min_w_0().truncate().child(addr.to_string())),
                    ),
            )
    }

    fn render_bodies(&self, entry: &Entry) -> Div {
        let id = entry.id();
        let nav = self.shared.nav.downgrade();
        let request_nav = nav.clone();
        let request_len = entry.request.body.len();
        let request_type = entry
            .request
            .header("content-type")
            .unwrap_or_default()
            .to_owned();
        div()
            .flex()
            .flex_col()
            .gap(rems(0.5))
            .child(match &entry.response {
                Some(response) if !response.body_decompressed.is_empty() => ui::body_link(
                    "inspector-open-response",
                    "Response",
                    body_detail(response.body.len(), entry.mime()),
                    move |_, _, cx| {
                        let _ = nav.update(cx, |nav, cx| nav.open_body(id, Side::Response, cx));
                    },
                )
                .into_any_element(),
                Some(_) => ui::quiet("The response had no body.").into_any_element(),
                None if entry.pending() => {
                    ui::quiet("Waiting for the response.").into_any_element()
                }
                None => ui::quiet("No response arrived.").into_any_element(),
            })
            .when(request_len > 0, |this| {
                this.child(ui::body_link(
                    "inspector-open-request",
                    "Request",
                    body_detail(
                        request_len,
                        request_type.split(';').next().unwrap_or_default(),
                    ),
                    move |_, _, cx| {
                        let _ =
                            request_nav.update(cx, |nav, cx| nav.open_body(id, Side::Request, cx));
                    },
                ))
            })
    }
}

/// Where the host, path, and query fall in a URL drawn from `parts`.
fn url_highlights(
    parts: &site::Parts,
    host_color: Hsla,
) -> Vec<(std::ops::Range<usize>, gpui_kit::HighlightStyle)> {
    let c = colors();
    let style = |color| gpui_kit::HighlightStyle {
        color: Some(color),
        ..gpui_kit::HighlightStyle::default()
    };
    let host = parts.scheme.len()..parts.scheme.len() + parts.host.len();
    let path = host.end..host.end + parts.path.len();
    let query = path.end..path.end + parts.query.len();
    vec![
        (0..parts.scheme.len(), style(c.ink_4)),
        (host, style(host_color)),
        (path, style(c.ink)),
        (query, style(c.ink_3)),
    ]
}

fn headers(headers: &[(String, String)]) -> Div {
    if headers.is_empty() {
        ui::quiet("None.")
    } else {
        ui::pairs(headers)
    }
}

/// A request's phases as one bar, with each phase's time below it.
fn timing(timings: Option<&Timings>, total: f64) -> Div {
    let c = colors();
    let p = phase_colors();
    let Some(t) = timings else {
        return ui::quiet(if total > 0.0 {
            format!("{} in all; no breakdown recorded.", format::ms(total))
        } else {
            "No timings recorded.".to_owned()
        });
    };
    // TLS happens inside the connect for plain proxies and alongside it for
    // intercepted ones; Mira records it apart, so it is drawn as its own phase.
    let phases: Vec<(&'static str, f64, Hsla)> = [
        ("Queued", t.blocked_ms.unwrap_or(0.0), p.blocked),
        ("Connect", t.connect_ms.unwrap_or(0.0), p.connect),
        ("TLS", t.ssl_ms.unwrap_or(0.0), p.ssl),
        ("Send", t.send_ms, p.send),
        ("Wait", t.wait_ms, p.wait),
        ("Receive", t.receive_ms, p.receive),
    ]
    .into_iter()
    .filter(|(_, value, _)| *value > 0.0)
    .collect();
    if phases.is_empty() {
        return ui::quiet("No timings recorded.");
    }
    let sum = phases
        .iter()
        .map(|(_, value, _)| value)
        .sum::<f64>()
        .max(1.0);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a share of the bar, for layout"
    )]
    let share = |value: f64| (value / sum) as f32;
    div()
        .child(
            h_flex()
                .h(rems(0.5))
                .w_full()
                .overflow_hidden()
                .bg(c.well)
                .children(
                    phases.iter().map(|&(_, value, color)| {
                        div().h_full().w(relative(share(value))).bg(color)
                    }),
                ),
        )
        .child(
            div()
                .mt(rems(0.5))
                .flex()
                .flex_wrap()
                .gap_y(rems(0.25))
                .children(phases.iter().map(|&(name, value, color)| {
                    h_flex()
                        .w(relative(1.0 / 3.0))
                        .pr(rems(0.75))
                        .gap(rems(0.375))
                        .text_size(text::MICRO)
                        .child(div().size(rems(0.375)).flex_none().bg(color))
                        .child(div().text_color(c.ink_3).child(name))
                        .child(
                            div()
                                .ml_auto()
                                .font_family(MONO)
                                .text_color(c.ink_2)
                                .child(format::ms(value)),
                        )
                })),
        )
        .child(
            div()
                .mt(rems(0.375))
                .font_family(MONO)
                .text_size(text::MICRO)
                .text_color(c.ink_4)
                .child(if t.connection_reused {
                    "REUSED CONNECTION"
                } else {
                    "NEW CONNECTION"
                }),
        )
}

/// A body's size and content type: `1.0 KB · application/json`.
fn body_detail(len: usize, mime: &str) -> String {
    let mime = mime.trim();
    if mime.is_empty() {
        format!("{} · no content type", format::bytes(len))
    } else {
        format!("{} · {mime}", format::bytes(len))
    }
}

impl Render for InspectorView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let c = colors();
        let nav = self.shared.nav.read(cx);
        let selected = nav.selected();
        let entry = selected.and_then(|id| self.shared.capture.read(cx).get(id).cloned());
        let status = self.shared.status.clone();
        let clear_nav = self.shared.nav.downgrade();

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(c.panel_2)
            .border_l_1()
            .border_color(c.line)
            .child(
                h_flex()
                    .flex_none()
                    .h(rems(2.5))
                    .gap(rems(0.5))
                    .pl(rems(1.0))
                    .pr(rems(0.375))
                    .border_b_1()
                    .border_color(c.line)
                    .child(ui::label("Request").text_color(c.ink_2))
                    .when_some(selected, |this, id| {
                        this.child(
                            div()
                                .font_family(MONO)
                                .text_size(text::MICRO)
                                .text_color(c.ink_4)
                                .child(format!("#{id}")),
                        )
                    })
                    .child(div().flex_1())
                    .when(selected.is_some(), |this| {
                        this.child(ui::icon_button(
                            "inspector-clear",
                            Icon::Close,
                            "Clear selection · Esc",
                            &status,
                            move |_, _, cx| {
                                let _ = clear_nav.update(cx, |nav, cx| nav.select(None, cx));
                            },
                        ))
                    }),
            )
            .child(match entry {
                Some(entry) => ui::scroll_area(
                    "inspector-scroll",
                    &self.scroll,
                    self.render_detail(&entry, cx),
                )
                .into_any_element(),
                None => ui::empty_state(
                    Icon::Target,
                    "No request selected",
                    "Pick a request in Network, Hosts, or Search to see its headers, timing, and bodies.",
                )
                .into_any_element(),
            })
    }
}
