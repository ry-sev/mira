//! Text fields: a recessed well with an icon.

use crate::icons::Icon;
use crate::icons::icon;
use crate::theme::MONO;
use crate::theme::colors;
use crate::theme::text;
use gpui_kit::App;
use gpui_kit::AppContext as _;
use gpui_kit::Context;
use gpui_kit::ElementId;
use gpui_kit::Entity;
use gpui_kit::Focusable as _;
use gpui_kit::InteractiveElement as _;
use gpui_kit::IntoElement;
use gpui_kit::Length;
use gpui_kit::MouseButton;
use gpui_kit::ParentElement as _;
use gpui_kit::SharedString;
use gpui_kit::Styled as _;
use gpui_kit::Window;
use gpui_kit::base::Input;
use gpui_kit::base::InputBase;
use gpui_kit::base::input::InputEditorStyle;
use gpui_kit::base::input::InputState;
use gpui_kit::div;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::rems;

/// A single-line input with `placeholder`, colored for the dark panels.
pub fn new_input<V: 'static>(
    placeholder: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut Context<'_, V>,
) -> Entity<InputState> {
    cx.new(|cx| {
        let mut state = InputState::new(window, cx).placeholder(placeholder);
        state.set_editor_style(editor_style());
        state
    })
}

fn editor_style() -> InputEditorStyle {
    let c = colors();
    InputEditorStyle {
        foreground: c.ink,
        muted_foreground: c.ink_4,
        selection: c.focus.opacity(0.35),
        caret: c.ink,
        ..InputEditorStyle::default()
    }
}

/// A field for `state`, `width` wide, led by `glyph`.
pub fn field(
    id: impl Into<ElementId>,
    state: &Entity<InputState>,
    glyph: Icon,
    width: impl Into<Length>,
    window: &Window,
    cx: &App,
) -> impl IntoElement {
    let c = colors();
    let width: Length = width.into();
    let focused = state.read(cx).focus_handle(cx).is_focused(window);
    let target = state.clone();
    InputBase::new(id)
        .focused(focused)
        .flex()
        .flex_none()
        .items_center()
        .gap(rems(0.5))
        .h(rems(1.75))
        .w(width)
        .px(rems(0.5))
        .border_1()
        .border_color(c.line)
        .bg(c.well)
        .styles(|styles| styles.focused(|style| style.border_color(c.focus)))
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            target.update(cx, |state, cx| state.focus(window, cx));
        })
        .child(icon(glyph, rems(0.75)).text_color(c.ink_3))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .font_family(MONO)
                .text_size(text::LABEL)
                .child(Input::new(state)),
        )
}

/// A settings field for `state`, `width` wide, with `unit` after the text and
/// a red border while `invalid`.
pub fn setting_field(
    id: impl Into<ElementId>,
    state: &Entity<InputState>,
    width: impl Into<Length>,
    unit: Option<&'static str>,
    invalid: bool,
    window: &Window,
    cx: &App,
) -> impl IntoElement {
    let c = colors();
    let focused = state.read(cx).focus_handle(cx).is_focused(window);
    let target = state.clone();
    InputBase::new(id)
        .focused(focused)
        .flex()
        .flex_none()
        .items_center()
        .gap(rems(0.5))
        .h(rems(1.75))
        .w(width.into())
        .px(rems(0.5))
        .border_1()
        .border_color(if invalid { c.err } else { c.line })
        .bg(c.well)
        .styles(move |styles| {
            styles.focused(move |style| style.border_color(if invalid { c.err } else { c.focus }))
        })
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            target.update(cx, |state, cx| state.focus(window, cx));
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .font_family(MONO)
                .text_size(text::LABEL)
                .child(Input::new(state)),
        )
        .when_some(unit, |this, unit| {
            this.child(
                div()
                    .flex_none()
                    .font_family(MONO)
                    .text_size(text::MICRO)
                    .text_color(c.ink_4)
                    .child(unit),
            )
        })
}

/// The compact filter above a list.
pub fn filter_field(
    id: impl Into<ElementId>,
    state: &Entity<InputState>,
    window: &Window,
    cx: &App,
) -> impl IntoElement {
    field(id, state, Icon::Filter, rems(14.0), window, cx)
}
