//! The capture at a glance: how requests ended, the busiest hosts, the
//! slowest requests, and what went wrong.

use crate::capture::Entry;
use crate::format;
use crate::icons::Icon;
use crate::state::Nav;
use crate::state::Page;
use crate::state::Shared;
use crate::theme::MONO;
use crate::theme::colors;
use crate::theme::text;
use crate::ui;
use gpui_kit::Context;
use gpui_kit::Hsla;
use gpui_kit::InteractiveElement as _;
use gpui_kit::IntoElement;
use gpui_kit::ParentElement as _;
use gpui_kit::Render;
use gpui_kit::ScrollHandle;
use gpui_kit::SharedString;
use gpui_kit::Styled as _;
use gpui_kit::Subscription;
use gpui_kit::WeakEntity;
use gpui_kit::Window;
use gpui_kit::base::h_flex;
use gpui_kit::div;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::relative;
use gpui_kit::rems;
use std::sync::Arc;

/// How many hosts and failures the overview lists.
const LISTED: usize = 8;

/// How many of the slowest requests the overview lists.
const SLOWEST: usize = 5;

/// The Overview page.
pub struct OverviewView {
    shared: Shared,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl OverviewView {
    pub fn new(shared: Shared, cx: &mut Context<'_, Self>) -> Self {
        let subscriptions = vec![cx.observe(&shared.capture, |_, _, cx| cx.notify())];
        Self {
            shared,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }
}

/// A request in one of the lists: when it started, its status, its URL, and
/// a note on the right. Clicking it shows it in Network.
fn entry_row(
    prefix: &'static str,
    entry: &Arc<Entry>,
    note: Option<(String, Hsla)>,
    nav: WeakEntity<Nav>,
) -> impl IntoElement {
    let c = colors();
    let id = entry.id();
    ui::row_button((prefix, id))
        .accessibility_label(SharedString::from(entry.request.url.to_string()))
        // Out by the padding, so the contents line up with the section title.
        .mx(rems(-0.5))
        .h(rems(1.75))
        .px(rems(0.5))
        .flex()
        .items_center()
        .gap(rems(0.625))
        .hover(|style| style.bg(c.hover))
        .child(
            div()
                .w(rems(4.5))
                .flex_none()
                .font_family(MONO)
                .text_size(text::MICRO)
                .text_color(c.ink_4)
                .child(format::offset(entry.start_ms)),
        )
        .child(ui::status_tag(entry))
        .child(ui::url_line(entry).flex_1().text_size(text::LABEL))
        .when_some(note, |this, (note, color)| {
            this.child(
                div()
                    .max_w(rems(16.0))
                    .truncate()
                    .font_family(MONO)
                    .text_size(text::MICRO)
                    .text_color(color)
                    .child(note),
            )
        })
        .on_click(move |_, _, cx| {
            let _ = nav.update(cx, |nav, cx| {
                nav.inspect(id, cx);
                nav.show(Page::Network, cx);
            });
        })
}

impl Render for OverviewView {
    #[expect(
        clippy::cast_precision_loss,
        reason = "shares of the status bar, for layout"
    )]
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let c = colors();
        let capture = self.shared.capture.read(cx);
        let totals = capture.totals();
        let mut hosts = capture.hosts();
        hosts.sort_by_key(|h| std::cmp::Reverse(h.requests));
        hosts.truncate(LISTED);
        let failures: Vec<_> = capture
            .entries()
            .iter()
            .rev()
            .filter(|e| e.failed())
            .take(LISTED)
            .cloned()
            .collect();
        let mut slowest: Vec<_> = capture
            .entries()
            .iter()
            .filter(|e| e.response.is_some())
            .cloned()
            .collect();
        slowest.sort_by(|a, b| b.duration_ms().total_cmp(&a.duration_ms()));
        slowest.truncate(SLOWEST);
        let shares = [
            (totals.success, c.ok, "2XX"),
            (totals.redirect, c.redirect, "3XX"),
            (totals.client_error, c.err, "4XX"),
            (totals.server_error, c.err.opacity(0.7), "5XX"),
            (totals.no_response, c.err.opacity(0.45), "NO RESPONSE"),
        ];
        let shown: usize = shares.iter().map(|(n, _, _)| n).sum::<usize>().max(1);

        let empty = capture.entries().is_empty();
        let content = div()
            .child(
                div()
                    .px(rems(1.0))
                    .pt(rems(1.25))
                    .child(ui::section("Status codes", None))
                    .child(
                        h_flex()
                            .h(rems(0.5))
                            .w_full()
                            .overflow_hidden()
                            .bg(c.well)
                            .children(shares.iter().filter(|(n, _, _)| *n > 0).map(
                                |&(n, color, _)| {
                                    div()
                                        .h_full()
                                        .w(relative(n as f32 / shown as f32))
                                        .bg(color)
                                },
                            )),
                    )
                    .child(
                        h_flex()
                            .mt(rems(0.625))
                            .flex_wrap()
                            .gap(rems(1.25))
                            .children(shares.iter().map(|&(n, color, name)| {
                                h_flex()
                                    .gap(rems(0.375))
                                    .font_family(MONO)
                                    .text_size(text::MICRO)
                                    .child(div().size(rems(0.375)).bg(color))
                                    .child(div().text_color(c.ink_3).child(name))
                                    .child(div().text_color(c.ink_2).child(format::count(n)))
                            })),
                    ),
            )
            .child(
                div()
                    .px(rems(1.0))
                    .mt(rems(2.0))
                    .child(ui::section("Busiest hosts", None))
                    .when(hosts.is_empty(), |this| {
                        this.child(ui::quiet("No hosts yet."))
                    })
                    .children(hosts.into_iter().map(|host| {
                        let nav = self.shared.nav.downgrade();
                        let name = host.name.clone();
                        ui::row_button(SharedString::from(format!("overview-host-{}", host.name)))
                            .accessibility_label(SharedString::from(host.name.clone()))
                            .mx(rems(-0.5))
                            .h(rems(1.75))
                            .px(rems(0.5))
                            .flex()
                            .items_center()
                            .gap(rems(0.625))
                            .hover(|style| style.bg(c.hover))
                            .child(div().size(rems(0.5)).flex_none().bg(host.color))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(MONO)
                                    .text_size(text::LABEL)
                                    .text_color(host.color)
                                    .child(SharedString::from(host.name.clone())),
                            )
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(text::MICRO)
                                    .text_color(c.ink_3)
                                    .child(format!(
                                        "{} · {}",
                                        format::plural(host.requests, "request"),
                                        format::bytes(host.bytes)
                                    )),
                            )
                            .when(host.failed > 0, |this| {
                                this.child(
                                    div()
                                        .font_family(MONO)
                                        .text_size(text::MICRO)
                                        .text_color(c.err)
                                        .child(format!("{} FAILED", format::count(host.failed))),
                                )
                            })
                            .on_click(move |_, _, cx| {
                                let name = name.clone();
                                let _ = nav.update(cx, |nav, cx| nav.filter_host(Some(name), cx));
                            })
                    })),
            )
            .child(
                div()
                    .px(rems(1.0))
                    .mt(rems(2.0))
                    .child(ui::section("Slowest requests", None))
                    .when(slowest.is_empty(), |this| {
                        this.child(ui::quiet("Nothing has finished yet."))
                    })
                    .children(slowest.iter().map(|entry| {
                        entry_row(
                            "overview-slow",
                            entry,
                            Some((format::ms(entry.duration_ms()), c.ink_2)),
                            self.shared.nav.downgrade(),
                        )
                    })),
            )
            .child(
                div()
                    .px(rems(1.0))
                    .mt(rems(2.0))
                    .pb(rems(3.0))
                    .child(ui::section("Recent failures", None))
                    .when(failures.is_empty(), |this| {
                        this.child(ui::quiet("Nothing has failed."))
                    })
                    .children(failures.iter().map(|entry| {
                        entry_row(
                            "overview-failure",
                            entry,
                            entry.error.clone().map(|error| (error, c.err)),
                            self.shared.nav.downgrade(),
                        )
                    })),
            );
        if empty {
            return ui::empty_state(
                Icon::Overview,
                "Nothing captured yet",
                "Set the client's HTTP proxy to the address in the top bar, and a summary of its traffic builds up here.",
            )
            .size_full();
        }
        div().flex().flex_col().size_full().child(ui::scroll_area(
            "overview-scroll",
            &self.scroll,
            content,
        ))
    }
}
