//! One body, read as text, as pretty-printed JSON, or as hex.
//!
//! Bodies are shown as plain text with invisible characters made visible,
//! and never rendered as markup.

use crate::capture::Entry;
use crate::format;
use crate::icons::Icon;
use crate::state::Page;
use crate::state::Shared;
use crate::state::Side;
use crate::state::Tone;
use crate::text;
use crate::theme::MONO;
use crate::theme::colors;
use crate::ui;
use gpui_kit::AnyElement;
use gpui_kit::ClipboardItem;
use gpui_kit::Context;
use gpui_kit::Div;
use gpui_kit::InteractiveElement as _;
use gpui_kit::IntoElement;
use gpui_kit::ListAlignment;
use gpui_kit::ListState;
use gpui_kit::ParentElement as _;
use gpui_kit::Render;
use gpui_kit::SharedString;
use gpui_kit::Styled as _;
use gpui_kit::Subscription;
use gpui_kit::Window;
use gpui_kit::base::Scrollbar;
use gpui_kit::base::ScrollbarMode;
use gpui_kit::base::h_flex;
use gpui_kit::div;
use gpui_kit::list;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::rems;
use std::rc::Rc;
use std::sync::Arc;

/// How a body is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    Pretty,
    Hex,
}

/// What the lines were made from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContentKey {
    id: u64,
    side: Side,
    mode: Mode,
    wrap: bool,
    len: usize,
}

/// A body made ready to show.
struct Content {
    key: ContentKey,
    lines: Rc<Vec<SharedString>>,
    /// What Copy puts on the clipboard: the text as shown.
    copy: Arc<String>,
    binary: bool,
    pretty: bool,
    hidden: usize,
    len: usize,
}

/// The Body page.
pub struct BodyView {
    shared: Shared,
    mode: Mode,
    wrap: bool,
    list: ListState,
    content: Option<Content>,
    _subscriptions: Vec<Subscription>,
}

impl BodyView {
    pub fn new(shared: Shared, window: &Window, cx: &mut Context<'_, Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&shared.capture, |_, _, cx| cx.notify()),
            cx.observe(&shared.nav, |_, _, cx| cx.notify()),
        ];
        Self {
            shared,
            mode: Mode::Text,
            wrap: true,
            // Rows a screenful beyond the view are measured ahead, so long
            // lines do not jump into place while scrolling.
            list: ListState::new(0, ListAlignment::Top, window.rem_size() * 40.0),
            content: None,
            _subscriptions: subscriptions,
        }
    }

    /// The bytes being shown, and the exchange they belong to.
    fn source(&self, cx: &Context<'_, Self>) -> Option<(Arc<Entry>, Side)> {
        let nav = self.shared.nav.read(cx);
        let entry = self.shared.capture.read(cx).get(nav.selected()?)?.clone();
        Some((entry, nav.side()))
    }

    fn refresh(&mut self, cx: &Context<'_, Self>) {
        let Some((entry, side)) = self.source(cx) else {
            self.content = None;
            return;
        };
        let bytes = body_bytes(&entry, side);
        let binary = text::is_binary(bytes);
        let mode = if binary { Mode::Hex } else { self.mode };
        let key = ContentKey {
            id: entry.id(),
            side,
            mode,
            wrap: self.wrap,
            len: bytes.len(),
        };
        if self.content.as_ref().is_some_and(|c| c.key == key) {
            return;
        }

        let raw = String::from_utf8_lossy(bytes);
        let hidden = if binary {
            0
        } else {
            text::invisible_count(&raw)
        };
        let pretty_text = if binary {
            None
        } else {
            text::pretty_json(&raw)
        };
        let (lines, copy): (Vec<SharedString>, String) = match mode {
            Mode::Hex => {
                let rows = bytes.len().div_ceil(text::HEX_WIDTH);
                let lines = (0..rows)
                    .map(|row| {
                        let (offset, hex, ascii) = text::hex_row(bytes, row);
                        SharedString::from(format!("{offset}  {hex} {ascii}"))
                    })
                    .collect();
                (lines, String::new())
            }
            Mode::Pretty if pretty_text.is_some() => {
                let shown = text::visible(pretty_text.as_deref().unwrap_or_default());
                (split_lines(&shown), shown)
            }
            Mode::Text | Mode::Pretty => {
                let shown = text::visible(&raw);
                (split_lines(&shown), shown)
            }
        };
        self.list.reset(lines.len());
        self.content = Some(Content {
            key,
            lines: Rc::new(lines),
            copy: Arc::new(copy),
            binary,
            pretty: pretty_text.is_some(),
            hidden,
            len: bytes.len(),
        });
    }

    fn render_header(&self, entry: &Entry, side: Side) -> Div {
        let c = colors();
        let nav = self.shared.nav.downgrade();
        let side_nav = self.shared.nav.downgrade();
        h_flex()
            .flex_none()
            .h(rems(2.5))
            .gap(rems(0.625))
            .pl(rems(0.5))
            .pr(rems(1.0))
            .border_b_1()
            .border_color(c.line)
            .bg(c.panel_2)
            .child(ui::icon_button(
                "body-back",
                Icon::ChevronLeft,
                "Back to Network · Esc",
                &self.shared.status,
                move |_, _, cx| {
                    let _ = nav.update(cx, |nav, cx| nav.show(Page::Network, cx));
                },
            ))
            .child(ui::status_tag(entry))
            .child(
                ui::url_line(entry)
                    .flex_1()
                    .min_w_0()
                    .text_size(crate::theme::text::LABEL),
            )
            .child(ui::segmented(
                "body-side",
                &[(Side::Request, "REQUEST"), (Side::Response, "RESPONSE")],
                side,
                move |side, _, cx| {
                    let _ = side_nav.update(cx, |nav, cx| nav.set_side(side, cx));
                },
            ))
    }

    fn render_toolbar(&self, entry: &Entry, side: Side, cx: &Context<'_, Self>) -> Div {
        let c = colors();
        let status = &self.shared.status;
        // The header's border already rules off the toolbar.
        let Some(content) = &self.content else {
            return ui::toolbar().border_t_0();
        };
        let mime = mime_of(entry, side);
        let mut modes = vec![(Mode::Text, "TEXT")];
        if content.pretty {
            modes.push((Mode::Pretty, "PRETTY"));
        }
        modes.push((Mode::Hex, "HEX"));
        if content.binary {
            modes = vec![(Mode::Hex, "HEX")];
        }
        let shown = content.key.mode;
        ui::toolbar()
            .border_t_0()
            .child(Self::mode_switch(&modes, shown, cx))
            .when(shown != Mode::Hex, |this| {
                let wrap = self.wrap;
                let view = cx.entity().downgrade();
                this.child(ui::toggle(
                    "body-wrap",
                    "WRAP",
                    wrap,
                    status,
                    "Wrap long lines",
                    move |on, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.wrap = on;
                            cx.notify();
                        });
                    },
                ))
            })
            .child(div().flex_1())
            .when(content.hidden > 0, |this| {
                this.child(ui::hint(
                    h_flex()
                        .id("body-hidden")
                        .gap(rems(0.375))
                        .text_size(crate::theme::text::LABEL)
                        .text_color(c.warn)
                        .child(crate::icons::icon(Icon::Warning, rems(0.75)).text_color(c.warn))
                        .child(format!(
                            "{} shown",
                            format::plural(content.hidden, "invisible character")
                        )),
                    status,
                    "Bidirectional overrides, zero-width and control characters are shown as visible stand-ins",
                ))
            })
            .child(
                div()
                    .font_family(MONO)
                    .text_size(crate::theme::text::MICRO)
                    .text_color(c.ink_3)
                    .child(if mime.is_empty() {
                        format::bytes(content.len)
                    } else {
                        format!("{mime} · {}", format::bytes(content.len))
                    }),
            )
            .when(shown != Mode::Hex, |this| {
                let copy = Arc::clone(&content.copy);
                let say = status.downgrade();
                this.child(ui::icon_button(
                    "body-copy",
                    Icon::Copy,
                    "Copy the text",
                    status,
                    move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.as_str().to_owned()));
                        let _ = say.update(cx, |s, cx| s.say(Tone::Ok, "Copied body text", cx));
                    },
                ))
            })
    }

    fn mode_switch(modes: &[(Mode, &'static str)], shown: Mode, cx: &Context<'_, Self>) -> Div {
        let view = cx.entity().downgrade();
        ui::segmented("body-mode", modes, shown, move |mode, _, cx| {
            let _ = view.update(cx, |this, cx| {
                this.mode = mode;
                cx.notify();
            });
        })
    }

    fn render_lines(&self) -> AnyElement {
        let c = colors();
        let Some(content) = &self.content else {
            return div().into_any_element();
        };
        if content.len == 0 {
            return ui::empty_state(Icon::Body, "Empty", "This body has no bytes.")
                .into_any_element();
        }
        let lines = Rc::clone(&content.lines);
        let digits = lines.len().to_string().len();
        #[expect(
            clippy::cast_precision_loss,
            reason = "a handful of digits, for the gutter width"
        )]
        let gutter = rems(0.6 * digits as f32 + 1.0);
        let wrap = content.key.wrap && content.key.mode != Mode::Hex;
        let hex = content.key.mode == Mode::Hex;
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .bg(c.panel)
            .font_family(MONO)
            .text_size(rems(0.75))
            .child(
                list(self.list.clone(), move |ix, _, _| {
                    let line = lines.get(ix).cloned().unwrap_or_default();
                    h_flex()
                        .items_start()
                        .w_full()
                        .px(rems(1.0))
                        .line_height(rems(1.125))
                        .child(
                            div()
                                .flex_none()
                                .w(gutter)
                                .pr(rems(0.75))
                                .text_right()
                                .text_color(c.ink_4)
                                .when(!hex, |this| this.child((ix + 1).to_string())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(if hex { c.ink_2 } else { c.ink })
                                .when(!wrap, |this| this.whitespace_nowrap().overflow_hidden())
                                .child(line),
                        )
                        .into_any_element()
                })
                .size_full(),
            )
            .child(Scrollbar::vertical(&self.list).mode(ScrollbarMode::Hover))
            .into_any_element()
    }
}

/// The bytes of one side of an exchange: what was sent, or what came back,
/// decompressed.
fn body_bytes(entry: &Entry, side: Side) -> &[u8] {
    match side {
        Side::Request => &entry.request.body,
        Side::Response => entry
            .response
            .as_ref()
            .map_or(&[][..], |r| &r.body_decompressed[..]),
    }
}

/// `text` split into lines, without their line endings.
fn split_lines(text: &str) -> Vec<SharedString> {
    text.split('\n')
        .map(|line| SharedString::from(line.strip_suffix('\r').unwrap_or(line).to_owned()))
        .collect()
}

/// The content type of one side of an exchange, without its parameters.
fn mime_of(entry: &Entry, side: Side) -> String {
    match side {
        Side::Response => entry.mime().to_owned(),
        Side::Request => entry
            .request
            .header("content-type")
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned(),
    }
}

impl Render for BodyView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        self.refresh(cx);
        let Some((entry, side)) = self.source(cx) else {
            return div().flex().flex_col().size_full().child(ui::empty_state(
                Icon::Body,
                "No request selected",
                "Open a body from the inspector, or press Enter on a request in Network.",
            ));
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .child(self.render_header(&entry, side))
            .child(self.render_toolbar(&entry, side, cx))
            .child(self.render_lines())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_lines_drops_line_endings() {
        assert_eq!(
            split_lines("a\r\nb\nc"),
            vec![
                SharedString::from("a"),
                SharedString::from("b"),
                SharedString::from("c")
            ]
        );
    }
}
