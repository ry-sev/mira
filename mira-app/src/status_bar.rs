//! The status bar: what the thing under the pointer does, or what just
//! happened, and the capture's running totals.

use crate::capture::Listening;
use crate::format;
use crate::icons::Icon;
use crate::icons::icon;
use crate::state::Shared;
use crate::state::Tone;
use crate::theme::MONO;
use crate::theme::colors;
use crate::theme::text;
use gpui_kit::Context;
use gpui_kit::IntoElement;
use gpui_kit::ParentElement as _;
use gpui_kit::Render;
use gpui_kit::Styled as _;
use gpui_kit::Subscription;
use gpui_kit::Window;
use gpui_kit::base::h_flex;
use gpui_kit::div;
use gpui_kit::rems;

/// The status bar.
pub struct StatusBar {
    shared: Shared,
    _subscriptions: Vec<Subscription>,
}

impl StatusBar {
    pub fn new(shared: Shared, cx: &mut Context<'_, Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&shared.capture, |_, _, cx| cx.notify()),
            cx.observe(&shared.status, |_, _, cx| cx.notify()),
        ];
        Self {
            shared,
            _subscriptions: subscriptions,
        }
    }
}

fn separator() -> impl IntoElement {
    div().h(rems(0.75)).w_px().bg(colors().line)
}

impl Render for StatusBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let c = colors();
        let line = self.shared.status.read(cx);
        let left = match line.message() {
            Some((tone, message)) => h_flex()
                .gap(rems(0.5))
                .min_w_0()
                .child(div().size(rems(0.375)).flex_none().bg(match tone {
                    Tone::Ok => c.ok,
                    Tone::Error => c.err,
                }))
                .child(div().truncate().text_color(c.ink).child(message.clone())),
            None => h_flex().min_w_0().child(
                div()
                    .truncate()
                    .text_color(c.ink_3)
                    .child(line.hint().cloned().unwrap_or_default()),
            ),
        };
        let capture = self.shared.capture.read(cx);
        let totals = capture.totals();
        let intercept = capture.intercept();
        let (dot, state) = match capture.listening() {
            Listening::On(_) if !capture.recording() => (c.warn, "PAUSED"),
            Listening::On(_) => (c.ok, "RECORDING"),
            Listening::Starting => (c.ink_4, "STARTING"),
            Listening::Failed(_) => (c.err, "STOPPED"),
        };

        h_flex()
            .flex_none()
            .h(rems(1.5))
            .gap(rems(1.0))
            .px(rems(0.75))
            .border_t_1()
            .border_color(c.line)
            .bg(c.panel_2)
            .text_size(text::LABEL)
            .child(div().flex_1().min_w_0().child(left))
            .child(
                h_flex()
                    .gap(rems(0.25))
                    .font_family(MONO)
                    .text_size(text::MICRO)
                    .text_color(c.ink_3)
                    .child(format!("{} REQ", format::count(totals.requests)))
                    .child("·")
                    .child(format!("{} PENDING", format::count(totals.pending)))
                    .child("·")
                    .child(
                        div()
                            .text_color(if totals.failed > 0 { c.err } else { c.ink_3 })
                            .child(format!("{} FAILED", format::count(totals.failed))),
                    )
                    .child("·")
                    .child(format::bytes(totals.bytes)),
            )
            .child(separator())
            .child(
                h_flex()
                    .gap(rems(0.375))
                    .font_family(MONO)
                    .text_size(text::MICRO)
                    .text_color(if intercept { c.live } else { c.ink_3 })
                    .child(icon(Icon::Lock, rems(0.6875)).text_color(if intercept {
                        c.live
                    } else {
                        c.ink_3
                    }))
                    .child(if intercept {
                        "HTTPS INTERCEPT"
                    } else {
                        "HTTPS PASSTHROUGH"
                    }),
            )
            .child(separator())
            .child(
                h_flex()
                    .gap(rems(0.375))
                    .font_family(MONO)
                    .text_size(text::MICRO)
                    .text_color(dot)
                    .child(div().size(rems(0.375)).bg(dot))
                    .child(state),
            )
    }
}
