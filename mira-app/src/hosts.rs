//! Every server the client talked to, with what it sent and how often it
//! failed. Picking one shows its requests in Network.

use crate::capture::HostSummary;
use crate::format;
use crate::icons::Icon;
use crate::network::ROW;
use crate::state::Shared;
use crate::theme::MONO;
use crate::theme::colors;
use crate::theme::text;
use crate::ui;
use gpui_kit::Context;
use gpui_kit::Div;
use gpui_kit::InteractiveElement as _;
use gpui_kit::IntoElement;
use gpui_kit::ParentElement as _;
use gpui_kit::Render;
use gpui_kit::SharedString;
use gpui_kit::Styled as _;
use gpui_kit::Subscription;
use gpui_kit::UniformListScrollHandle;
use gpui_kit::Window;
use gpui_kit::base::Scrollbar;
use gpui_kit::base::ScrollbarMode;
use gpui_kit::base::h_flex;
use gpui_kit::div;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::rems;
use gpui_kit::uniform_list;
use std::rc::Rc;

/// How the hosts are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sort {
    First,
    Requests,
    Bytes,
}

/// The Hosts page.
pub struct HostsView {
    shared: Shared,
    sort: Sort,
    failed_only: bool,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl HostsView {
    pub fn new(shared: Shared, cx: &mut Context<'_, Self>) -> Self {
        let subscriptions = vec![cx.observe(&shared.capture, |_, _, cx| cx.notify())];
        Self {
            shared,
            sort: Sort::First,
            failed_only: false,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }
}

/// The table's columns, shared by its heading and its rows.
fn columns() -> [Div; 5] {
    [
        h_flex().flex_1().min_w_0().gap(rems(0.625)).pr(rems(0.75)),
        div().flex_none().w(rems(6.0)).pr(rems(0.75)).text_right(),
        div().flex_none().w(rems(5.0)).pr(rems(0.75)).text_right(),
        div().flex_none().w(rems(5.0)).pr(rems(0.75)).text_right(),
        div().flex_none().w(rems(5.5)).text_right(),
    ]
}

fn header() -> impl IntoElement {
    let c = colors();
    let [host, requests, size, failed, first] = columns();
    h_flex()
        .flex_none()
        .h(rems(1.75))
        .w_full()
        .px(rems(1.0))
        .border_b_1()
        .border_color(c.line)
        .bg(c.panel)
        .font_family(MONO)
        .text_size(text::MICRO)
        .text_color(c.ink_3)
        // An empty swatch, so the heading lines up with the names.
        .child(host.child(div().size(rems(0.5)).flex_none()).child("HOST"))
        .child(requests.child("REQUESTS"))
        .child(size.child("SIZE"))
        .child(failed.child("FAILED"))
        .child(first.child("FIRST SEEN"))
}

fn host_row(host: &HostSummary) -> Div {
    let c = colors();
    let [name, requests, size, failed, first] = columns();
    h_flex()
        .w_full()
        .h(rems(ROW))
        .px(rems(1.0))
        .border_b_1()
        .border_color(c.line_soft.opacity(0.7))
        .font_family(MONO)
        .text_size(text::MICRO)
        .text_color(c.ink_3)
        .child(
            name.child(div().size(rems(0.5)).flex_none().bg(host.color))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(text::LABEL)
                        .text_color(host.color)
                        .child(SharedString::from(host.name.clone())),
                ),
        )
        .child(
            requests
                .text_color(c.ink_2)
                .child(format::count(host.requests)),
        )
        .child(size.child(format::bytes(host.bytes)))
        .child(if host.failed > 0 {
            failed.text_color(c.err).child(format::count(host.failed))
        } else {
            failed.text_color(c.ink_4).child("—")
        })
        .child(
            first
                .text_color(c.ink_4)
                .child(format::offset(host.first_ms)),
        )
}

impl Render for HostsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let c = colors();
        let status = &self.shared.status;
        let mut hosts = self.shared.capture.read(cx).hosts();
        let total = hosts.len();
        if self.failed_only {
            hosts.retain(|h| h.failed > 0);
        }
        match self.sort {
            Sort::First => hosts.sort_by(|a, b| a.first_ms.total_cmp(&b.first_ms)),
            Sort::Requests => hosts.sort_by_key(|h| std::cmp::Reverse(h.requests)),
            Sort::Bytes => hosts.sort_by_key(|h| std::cmp::Reverse(h.bytes)),
        }
        let hosts = Rc::new(hosts);
        let shown = hosts.len();
        let view = cx.entity().downgrade();
        let failed_view = view.clone();
        let nav = self.shared.nav.downgrade();

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            // The top bar's border already rules off the toolbar.
            .child(
                ui::toolbar()
                    .border_t_0()
                    .child(ui::segmented(
                        "hosts-sort",
                        &[
                            (Sort::First, "FIRST"),
                            (Sort::Requests, "REQUESTS"),
                            (Sort::Bytes, "SIZE"),
                        ],
                        self.sort,
                        move |sort, _, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.sort = sort;
                                cx.notify();
                            });
                        },
                    ))
                    .child(ui::toggle(
                        "hosts-failed",
                        "FAILED",
                        self.failed_only,
                        status,
                        "Only hosts with failed requests",
                        move |on, _, cx| {
                            let _ = failed_view.update(cx, |this, cx| {
                                this.failed_only = on;
                                cx.notify();
                            });
                        },
                    ))
                    .child(ui::tally(shown, total)),
            )
            .child(header())
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .bg(c.panel)
                    .when(hosts.is_empty(), |this| {
                        this.child(ui::empty_state(
                            Icon::Host,
                            if total == 0 {
                                "No hosts yet"
                            } else {
                                "No hosts match"
                            },
                            "Every server the client talks to through the proxy is listed here.",
                        ))
                    })
                    .when(!hosts.is_empty(), |this| {
                        let rows = Rc::clone(&hosts);
                        this.child(
                            uniform_list("hosts-rows", rows.len(), move |range, _, _| {
                                range
                                    .filter_map(|at| rows.get(at))
                                    .map(|host| {
                                        let nav = nav.clone();
                                        let name = host.name.clone();
                                        ui::row_button(SharedString::from(format!(
                                            "host-{}",
                                            host.name
                                        )))
                                        .accessibility_label(SharedString::from(host.name.clone()))
                                        .w_full()
                                        .hover(|style| style.bg(c.hover))
                                        .child(host_row(host))
                                        .on_click(
                                            move |_, _, cx| {
                                                let name = name.clone();
                                                let _ = nav.update(cx, |nav, cx| {
                                                    nav.filter_host(Some(name), cx);
                                                });
                                            },
                                        )
                                    })
                                    .collect()
                            })
                            .track_scroll(&self.scroll)
                            .size_full(),
                        )
                        .child(Scrollbar::vertical(&self.scroll).mode(ScrollbarMode::Hover))
                    }),
            )
    }
}
