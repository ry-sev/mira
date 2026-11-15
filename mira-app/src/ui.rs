//! The window's own controls and recurring pieces: square edges, hairline
//! borders, mono labels, and color only for meaning.
//!
//! Behavior comes from GPUI Base: buttons, toggles, and inputs keep their
//! focus, keyboard, and accessibility contracts. This module only decides
//! how they look.

use crate::capture::Entry;
use crate::icons::Icon;
use crate::icons::icon;
use crate::state::StatusLine;
use crate::theme::MONO;
use crate::theme::colors;
use crate::theme::text;
use gpui_kit::App;
use gpui_kit::ClickEvent;
use gpui_kit::Div;
use gpui_kit::ElementId;
use gpui_kit::Entity;
use gpui_kit::FontWeight;
use gpui_kit::Hsla;
use gpui_kit::InteractiveElement as _;
use gpui_kit::IntoElement;
use gpui_kit::ParentElement as _;
use gpui_kit::SharedString;
use gpui_kit::StatefulInteractiveElement;
use gpui_kit::Styled as _;
use gpui_kit::Window;
use gpui_kit::base::Button;
use gpui_kit::base::Switch;
use gpui_kit::base::Toggle;
use gpui_kit::base::h_flex;
use gpui_kit::div;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::relative;
use gpui_kit::rems;

/// A small uppercase mono caption, for section and field labels.
pub fn label(caption: impl Into<SharedString>) -> Div {
    let caption: SharedString = caption.into();
    div()
        .font_family(MONO)
        .text_size(text::MICRO)
        .text_color(colors().ink_3)
        .child(SharedString::from(caption.to_uppercase()))
}

/// A small boxed tag: a method, a status, a type, a badge.
pub fn tag(caption: impl Into<SharedString>, color: Hsla, border: Hsla) -> Div {
    h_flex()
        .flex_none()
        .h(rems(1.0625))
        .px(rems(0.375))
        .border_1()
        .border_color(border)
        .font_family(MONO)
        .text_size(rems(0.59375))
        .text_color(color)
        .child(caption.into())
}

/// A neutral tag.
pub fn plain_tag(caption: impl Into<SharedString>) -> Div {
    tag(caption, colors().ink_2, colors().line)
}

/// The color of a status, or of a failure to get one.
pub fn status_color(status: Option<u16>, failed: bool) -> Hsla {
    let c = colors();
    match status {
        _ if failed && status.is_none() => c.err,
        Some(400..) => c.err,
        Some(300..=399) => c.redirect,
        Some(200..=299) => c.ok,
        _ => c.ink_2,
    }
}

/// An exchange's status, or why it has none.
pub fn status_tag(entry: &Entry) -> Div {
    let caption = match entry.status() {
        Some(status) => status.to_string(),
        None if entry.error.is_some() => "ERR".to_owned(),
        None => "…".to_owned(),
    };
    let color = status_color(entry.status(), entry.failed());
    let border = if color == colors().ink_2 {
        colors().line
    } else {
        color.opacity(0.45)
    };
    tag(caption, color, border)
}

/// A URL with its host in the site's color, its path plain, and its query dim.
pub fn url_line(entry: &Entry) -> Div {
    let c = colors();
    h_flex()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .font_family(MONO)
        .child(
            div()
                .flex_none()
                .text_color(entry.color)
                .child(SharedString::from(entry.host.clone())),
        )
        .child(
            div()
                .text_color(c.ink)
                .child(SharedString::from(entry.path.clone())),
        )
        .child(
            div()
                .text_color(c.ink_3)
                .child(SharedString::from(entry.query.clone())),
        )
}

/// Say `hint` in the status bar while the pointer is over `element`.
pub fn hint<E: StatefulInteractiveElement>(
    element: E,
    status: &Entity<StatusLine>,
    hint: impl Into<SharedString>,
) -> E {
    let status = status.downgrade();
    let hint = hint.into();
    element.on_hover(move |hovered, _, cx| {
        let hint = hovered.then(|| hint.clone());
        let _ = status.update(cx, |status, cx| status.set_hint(hint, cx));
    })
}

/// A square button holding one icon.
pub fn icon_button(
    id: impl Into<ElementId>,
    glyph: Icon,
    label: &'static str,
    status: &Entity<StatusLine>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    icon_toggle(id, glyph, label, false, status, on_click)
}

/// The group an icon button's icon brightens with on hover.
const ICON_BUTTON: &str = "icon-button";

/// An icon button that lights up in the accent color while `lit`.
pub fn icon_toggle(
    id: impl Into<ElementId>,
    glyph: Icon,
    label: &'static str,
    lit: bool,
    status: &Entity<StatusLine>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let c = colors();
    let button = Button::new(id)
        .group(ICON_BUTTON)
        .accessibility_label(label)
        .size(rems(1.75))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .map(|this| {
            if lit {
                this.bg(c.live.opacity(0.15))
            } else {
                this.hover(|style| style.bg(c.hover))
            }
        })
        .styles(|styles| styles.disabled(|style| style.opacity(0.4)))
        .child(icon(glyph, rems(0.875)).map(|svg| {
            if lit {
                svg.text_color(c.live)
            } else {
                svg.group_hover(ICON_BUTTON, |style| style.text_color(c.ink))
            }
        }))
        .on_click(on_click);
    hint(button, status, label)
}

/// A latching button that lights up in the accent color when on.
pub fn toggle(
    id: impl Into<ElementId>,
    caption: &'static str,
    on: bool,
    status: &Entity<StatusLine>,
    hint_text: &'static str,
    on_change: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let c = colors();
    let id = id.into();
    let control = Toggle::new(id.clone())
        .pressed(on)
        .accessibility_label(hint_text)
        .h(rems(1.75))
        .px(rems(0.5))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .border_1()
        .font_family(MONO)
        .text_size(text::MICRO)
        .map(|this| {
            if on {
                this.border_color(c.live.opacity(0.4))
                    .bg(c.live.opacity(0.15))
                    .text_color(c.live)
            } else {
                this.border_color(gpui_kit::transparent_black())
                    .text_color(c.ink_3)
                    .hover(|style| style.bg(c.hover).text_color(c.ink))
            }
        })
        .child(caption)
        .on_change(move |next, _, window, cx| on_change(next, window, cx));
    hint(div().id(id).child(control), status, hint_text)
}

/// An on-off switch: a square track whose thumb moves right and lights up
/// when on.
pub fn switch(
    id: impl Into<ElementId>,
    on: bool,
    label: &'static str,
    on_change: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> Switch {
    let c = colors();
    Switch::new(id)
        .checked(on)
        .accessibility_label(label)
        .flex()
        .flex_none()
        .items_center()
        .w(rems(2.0))
        .h(rems(1.125))
        .p(rems(0.1875))
        .border_1()
        .map(|this| {
            if on {
                this.justify_end()
                    .border_color(c.live.opacity(0.5))
                    .bg(c.live.opacity(0.15))
            } else {
                this.justify_start()
                    .border_color(c.line)
                    .bg(c.well)
                    .hover(|style| style.border_color(c.ink_4))
            }
        })
        .child(
            div()
                .size(rems(0.625))
                .bg(if on { c.live } else { c.ink_3 }),
        )
        .on_change(move |next, _, window, cx| on_change(next, window, cx))
}

/// A button with a caption. A primary one is drawn in the accent color.
pub fn text_button(id: impl Into<ElementId>, caption: &'static str, primary: bool) -> Button {
    let c = colors();
    row_button(id)
        .accessibility_label(caption)
        .h(rems(1.75))
        .px(rems(0.75))
        .flex()
        .flex_none()
        .items_center()
        .border_1()
        .text_size(text::LABEL)
        .map(|this| {
            if primary {
                this.border_color(c.live.opacity(0.6))
                    .bg(c.live.opacity(0.15))
                    .text_color(c.live)
                    .hover(|style| style.bg(c.live.opacity(0.25)))
            } else {
                this.border_color(c.line)
                    .text_color(c.ink_2)
                    .hover(|style| style.bg(c.hover).text_color(c.ink))
            }
        })
        .styles(|styles| styles.disabled(|style| style.opacity(0.4)))
        .child(caption)
}

/// A row of mutually exclusive options.
pub fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(T, &'static str)],
    selected: T,
    on_select: impl Fn(T, &mut Window, &mut App) + 'static,
) -> Div {
    let c = colors();
    let on_select = std::rc::Rc::new(on_select);
    h_flex()
        .flex_none()
        .border_1()
        .border_color(c.line)
        .bg(c.well)
        .children(options.iter().enumerate().map(|(at, &(value, caption))| {
            let chosen = value == selected;
            let on_select = on_select.clone();
            Button::new((id, at))
                .selected(chosen)
                .accessibility_label(caption)
                .h(rems(1.5))
                .px(rems(0.5))
                .flex()
                .items_center()
                .font_family(MONO)
                .text_size(text::MICRO)
                .when(at > 0, |this| this.border_l_1().border_color(c.line))
                .map(|this| {
                    if chosen {
                        this.bg(c.live.opacity(0.15)).text_color(c.live)
                    } else {
                        this.text_color(c.ink_3)
                            .hover(|style| style.bg(c.hover).text_color(c.ink))
                    }
                })
                .child(caption)
                .on_click(move |_, window, cx| on_select(value, window, cx))
        }))
}

/// A full-width link to a body: what it is, its size and type, and where it
/// leads.
pub fn body_link(
    id: impl Into<ElementId>,
    caption: &'static str,
    detail: String,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let c = colors();
    row_button(id)
        .w_full()
        .h(rems(2.0))
        .px(rems(0.625))
        .flex()
        .items_center()
        .justify_start()
        .gap(rems(0.5))
        .bg(c.well)
        .border_1()
        .border_color(c.line)
        .hover(|style| style.bg(c.hover).border_color(c.ink_4))
        .child(icon(Icon::Body, rems(0.875)).text_color(c.ink_3))
        .child(
            div()
                .flex_none()
                .text_size(text::UI)
                .text_color(c.ink)
                .child(caption),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_right()
                .font_family(MONO)
                .text_size(text::MICRO)
                .text_color(c.ink_3)
                .child(detail),
        )
        .child(icon(Icon::ChevronRight, rems(0.75)).text_color(c.ink_4))
        .on_click(on_click)
}

/// The header of a page: its name and a line of detail, on one row.
pub fn page_header(title: impl Into<SharedString>, meta: Option<String>) -> Div {
    let c = colors();
    h_flex()
        .flex_none()
        .h(rems(2.5))
        .gap(rems(0.75))
        .px(rems(1.0))
        .border_b_1()
        .border_color(c.line)
        .bg(c.panel_2)
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(text::BODY)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(c.ink)
                .child(title.into()),
        )
        .when_some(meta, |this, meta| {
            this.child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(MONO)
                    .text_size(text::LABEL)
                    .text_color(c.ink_3)
                    .child(meta),
            )
        })
}

/// A Button for a row of text.
///
/// Button's own line height is the bare font size, which clips descenders
/// once the text truncates.
pub fn row_button(id: impl Into<ElementId>) -> Button {
    Button::new(id).line_height(relative(1.5))
}

/// The strip along the bottom of a column.
///
/// Every footer MUST be built from this, so that footers side by side stay
/// level. The height includes the rule.
pub fn footer() -> Div {
    h_flex()
        .flex_none()
        .h(rems(2.5))
        .border_t_1()
        .border_color(colors().line)
}

/// The control strip above a list.
pub fn toolbar() -> Div {
    let c = colors();
    h_flex()
        .flex_none()
        .flex_wrap()
        .min_h(rems(2.5))
        .gap(rems(0.5))
        .px(rems(1.0))
        .py(rems(0.3125))
        .border_y_1()
        .border_color(c.line)
        .bg(c.panel_2)
}

/// How many rows a filter leaves, on the right of a toolbar; blank when it hides none.
pub fn tally(shown: usize, total: usize) -> Div {
    div()
        .ml_auto()
        .font_family(MONO)
        .text_size(text::MICRO)
        .text_color(colors().ink_3)
        .when(shown != total, |this| {
            this.child(format!("{} SHOWN", crate::format::count(shown)))
        })
}

/// A titled group within a page, with a rule after its title.
pub fn section(title: &'static str, more: Option<String>) -> Div {
    let c = colors();
    h_flex()
        .mb(rems(0.75))
        .h(rems(1.5))
        .gap(rems(0.75))
        .child(label(title).text_color(c.ink_2))
        .child(div().h_px().flex_1().bg(c.line_soft))
        .when_some(more, |this, more| {
            this.child(
                div()
                    .font_family(MONO)
                    .text_size(text::MICRO)
                    .text_color(c.ink_4)
                    .child(more),
            )
        })
}

/// Names beside their values, such as headers. Names share one column, 40%
/// of the width, and wrap within it, so values line up and one long name
/// cannot squeeze every value.
pub fn pairs(rows: &[(String, String)]) -> Div {
    let c = colors();
    div()
        .flex()
        .flex_col()
        .gap(rems(0.25))
        .font_family(MONO)
        .text_size(text::LABEL)
        .children(rows.iter().map(|(name, value)| {
            h_flex()
                .items_start()
                .gap(rems(0.75))
                .child(
                    div()
                        .flex_none()
                        .w(relative(0.4))
                        .text_color(c.ink_3)
                        .child(SharedString::from(crate::text::visible(name))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(if value.is_empty() { c.ink_4 } else { c.ink })
                        .child(if value.is_empty() {
                            SharedString::from("empty")
                        } else {
                            SharedString::from(crate::text::visible(value))
                        }),
                )
        }))
}

/// A region that scrolls vertically when `content` outgrows it.
///
/// The scrollbar sits beside the scrolling element rather than inside it.
/// It is positioned absolutely with no offset, so inside the scrolling
/// element it would be placed after the content and add a whole viewport of
/// empty space to the scroll range.
pub fn scroll_area(
    id: &'static str,
    handle: &gpui_kit::ScrollHandle,
    content: impl IntoElement,
) -> Div {
    div()
        .relative()
        .flex_1()
        .min_h_0()
        .child(
            div()
                .id(id)
                .size_full()
                .overflow_y_scroll()
                .track_scroll(handle)
                .child(content),
        )
        .child(
            gpui_kit::base::Scrollbar::vertical(handle).mode(gpui_kit::base::ScrollbarMode::Hover),
        )
}

/// A quiet line of text, for empty states.
pub fn quiet(content: impl Into<SharedString>) -> Div {
    div()
        .text_size(text::LABEL)
        .text_color(colors().ink_3)
        .child(content.into())
}

/// A whole-page empty state: an icon in a box, a title, and a line of help.
pub fn empty_state(glyph: Icon, title: &'static str, help: &'static str) -> Div {
    let c = colors();
    div()
        .flex()
        .flex_col()
        .flex_1()
        .items_center()
        .justify_center()
        .gap(rems(0.75))
        .px(rems(2.0))
        .child(
            h_flex()
                .size(rems(2.25))
                .justify_center()
                .border_1()
                .border_color(c.line)
                .child(icon(glyph, rems(1.0)).text_color(c.ink_4)),
        )
        .child(div().text_size(text::UI).text_color(c.ink_2).child(title))
        .child(
            div()
                .max_w(rems(24.0))
                .text_size(text::LABEL)
                .text_color(c.ink_3)
                .text_center()
                .child(help),
        )
}
