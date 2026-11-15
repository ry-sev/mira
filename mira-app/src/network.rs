//! Every request, in the order it started, with a waterfall against the
//! capture's timeline.

use crate::capture::Entry;
use crate::capture::Kind;
use crate::format;
use crate::icons::Icon;
use crate::icons::icon;
use crate::input::filter_field;
use crate::input::new_input;
use crate::state::Shared;
use crate::state::Side;
use crate::theme::MONO;
use crate::theme::colors;
use crate::theme::text;
use crate::ui;
use gpui_kit::Context;
use gpui_kit::Div;
use gpui_kit::Entity;
use gpui_kit::FocusHandle;
use gpui_kit::Focusable;
use gpui_kit::HighlightStyle;
use gpui_kit::InteractiveElement as _;
use gpui_kit::IntoElement;
use gpui_kit::MouseButton;
use gpui_kit::ParentElement as _;
use gpui_kit::Render;
use gpui_kit::ScrollStrategy;
use gpui_kit::SharedString;
use gpui_kit::Styled as _;
use gpui_kit::StyledText;
use gpui_kit::Subscription;
use gpui_kit::UniformListScrollHandle;
use gpui_kit::Window;
use gpui_kit::base::Scrollbar;
use gpui_kit::base::ScrollbarMode;
use gpui_kit::base::h_flex;
use gpui_kit::base::input::InputEvent;
use gpui_kit::base::input::InputState;
use gpui_kit::div;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::relative;
use gpui_kit::rems;
use gpui_kit::uniform_list;
use std::rc::Rc;
use std::sync::Arc;

/// The key context the request list's bindings live in.
pub const CONTEXT: &str = "Requests";

/// Selects the next request.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct SelectNext;

/// Selects the previous request.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct SelectPrevious;

/// Opens the selected request's response body.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct OpenSelected;

/// The height of a table row, in rems. Hosts shares it.
pub const ROW: f32 = 1.625;

/// The resource types the list can be narrowed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KindFilter {
    All,
    Document,
    Script,
    Data,
    Image,
    Style,
    Media,
    Other,
}

impl KindFilter {
    const OPTIONS: [(Self, &'static str); 8] = [
        (Self::All, "ALL"),
        (Self::Document, "DOC"),
        (Self::Script, "JS"),
        (Self::Data, "DATA"),
        (Self::Image, "IMG"),
        (Self::Style, "CSS"),
        (Self::Media, "MEDIA"),
        (Self::Other, "OTHER"),
    ];

    fn admits(self, kind: Kind) -> bool {
        match self {
            Self::All => true,
            Self::Document => kind == Kind::Document,
            Self::Script => kind == Kind::Script,
            Self::Data => kind == Kind::Data,
            Self::Image => kind == Kind::Image,
            Self::Style => matches!(kind, Kind::Stylesheet | Kind::Font),
            Self::Media => kind == Kind::Media,
            Self::Other => matches!(kind, Kind::Other | Kind::WebSocket | Kind::Tunnel),
        }
    }
}

/// What the shown rows were computed from.
#[derive(Debug, Clone, PartialEq)]
struct RowsKey {
    revision: u64,
    text: SharedString,
    kind: KindFilter,
    failed_only: bool,
    host: Option<String>,
}

/// The Network page.
pub struct NetworkView {
    shared: Shared,
    filter: Entity<InputState>,
    kind: KindFilter,
    failed_only: bool,
    follow: bool,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    rows: Rc<Vec<Arc<Entry>>>,
    rows_key: Option<RowsKey>,
    _subscriptions: Vec<Subscription>,
}

impl NetworkView {
    pub fn new(shared: Shared, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let filter = new_input("URL, method, or status", window, cx);
        let subscriptions = vec![
            cx.observe(&shared.capture, |_, _, cx| cx.notify()),
            cx.observe(&shared.nav, |_, _, cx| cx.notify()),
            cx.subscribe(&filter, |_, _, _: &InputEvent, cx| cx.notify()),
        ];
        Self {
            shared,
            filter,
            kind: KindFilter::All,
            failed_only: false,
            follow: true,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            rows: Rc::new(Vec::new()),
            rows_key: None,
            _subscriptions: subscriptions,
        }
    }

    /// Recompute the shown rows if anything they depend on changed.
    fn refresh_rows(&mut self, cx: &Context<'_, Self>) {
        let capture = self.shared.capture.read(cx);
        let key = RowsKey {
            revision: capture.revision(),
            text: self.filter.read(cx).value(),
            kind: self.kind,
            failed_only: self.failed_only,
            host: self.shared.nav.read(cx).host_filter().map(str::to_owned),
        };
        if self.rows_key.as_ref() == Some(&key) {
            return;
        }
        let needle = key.text.trim().to_lowercase();
        let rows: Vec<Arc<Entry>> = capture
            .entries()
            .iter()
            .filter(|e| key.host.as_deref().is_none_or(|host| e.host == host))
            .filter(|e| key.kind.admits(e.kind))
            .filter(|e| !key.failed_only || e.failed())
            .filter(|e| needle.is_empty() || admits(e, &needle))
            .cloned()
            .collect();
        let grew = rows.len() > self.rows.len();
        self.rows = Rc::new(rows);
        self.rows_key = Some(key);
        if self.follow && grew && !self.rows.is_empty() {
            self.scroll
                .scroll_to_item(self.rows.len() - 1, ScrollStrategy::Bottom);
        }
    }

    fn step(&mut self, by: isize, cx: &mut Context<'_, Self>) {
        if self.rows.is_empty() {
            return;
        }
        let selected = self.shared.nav.read(cx).selected();
        let at = selected.and_then(|id| self.rows.iter().position(|e| e.id() == id));
        let next = match at {
            Some(at) => at.saturating_add_signed(by).min(self.rows.len() - 1),
            None if by > 0 => 0,
            None => self.rows.len() - 1,
        };
        let id = self.rows[next].id();
        self.follow = false;
        self.scroll.scroll_to_item(next, ScrollStrategy::Nearest);
        self.shared
            .nav
            .update(cx, |nav, cx| nav.select(Some(id), cx));
    }

    fn open_selected(&self, cx: &mut Context<'_, Self>) {
        if let Some(id) = self.shared.nav.read(cx).selected() {
            self.shared
                .nav
                .update(cx, |nav, cx| nav.open_body(id, Side::Response, cx));
        }
    }

    pub fn focus_filter(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.filter
            .update(cx, |filter, cx| filter.focus(window, cx));
    }

    fn render_toolbar(&self, window: &Window, cx: &Context<'_, Self>) -> Div {
        let c = colors();
        let status = &self.shared.status;
        let total = self.shared.capture.read(cx).entries().len();
        let host = self.shared.nav.read(cx).host_filter().map(str::to_owned);
        let view = cx.entity().downgrade();
        let failed_view = view.clone();
        let follow_view = view.clone();
        ui::toolbar()
            .child(filter_field("network-filter", &self.filter, window, cx))
            .child(ui::segmented(
                "network-kind",
                &KindFilter::OPTIONS,
                self.kind,
                move |kind, _, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.kind = kind;
                        cx.notify();
                    });
                },
            ))
            .child(ui::toggle(
                "network-failed",
                "FAILED",
                self.failed_only,
                status,
                "Only requests that failed or got an error status",
                move |on, _, cx| {
                    let _ = failed_view.update(cx, |this, cx| {
                        this.failed_only = on;
                        cx.notify();
                    });
                },
            ))
            .when_some(host, |this, host| {
                let nav = self.shared.nav.downgrade();
                this.child(ui::hint(
                    gpui_kit::base::Button::new("network-host-chip")
                        .h(rems(1.5))
                        .px(rems(0.375))
                        .flex()
                        .items_center()
                        .gap(rems(0.375))
                        .border_1()
                        .border_color(c.ink_4)
                        .font_family(MONO)
                        .text_size(text::MICRO)
                        .text_color(c.ink)
                        .hover(|style| style.border_color(c.ink))
                        .child(host)
                        .child(icon(Icon::Close, rems(0.625)).text_color(c.ink))
                        .on_click(move |_, _, cx| {
                            let _ = nav.update(cx, |nav, cx| nav.filter_host(None, cx));
                        }),
                    status,
                    "Show every host",
                ))
            })
            .child(ui::tally(self.rows.len(), total))
            .child(ui::toggle(
                "network-follow",
                "FOLLOW",
                self.follow,
                status,
                "Scroll to the newest request as it arrives",
                move |on, _, cx| {
                    let _ = follow_view.update(cx, |this, cx| {
                        this.follow = on;
                        cx.notify();
                    });
                },
            ))
    }
}

impl Focusable for NetworkView {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The columns, shared by the header and every row so they line up.
/// Whether `entry` matches a lowercase filter: its URL contains it, or it is
/// the method or the status code.
fn admits(entry: &Entry, needle: &str) -> bool {
    entry.request.url.as_str().to_lowercase().contains(needle)
        || entry.request.method.eq_ignore_ascii_case(needle)
        || entry
            .status()
            .is_some_and(|code| code.to_string() == needle)
}

fn columns() -> [Div; 9] {
    [
        div().flex_none().w(rems(3.75)).pl(rems(1.0)),
        div().flex_none().w(rems(4.0)),
        h_flex().flex_none().w(rems(3.25)),
        div().flex_none().w(rems(3.625)),
        div()
            .flex_none()
            .w(relative(0.2))
            .min_w(rems(6.0))
            .pr(rems(0.75))
            .truncate(),
        div().flex_1().min_w_0().pr(rems(0.75)).truncate(),
        div().flex_none().w(rems(4.5)).pr(rems(0.75)).text_right(),
        div().flex_none().w(rems(4.5)).pr(rems(0.75)).text_right(),
        div()
            .flex_none()
            .w(relative(0.18))
            .min_w(rems(8.0))
            .mr(rems(1.0)),
    ]
}

fn header() -> impl IntoElement {
    let c = colors();
    let [
        number,
        method,
        status,
        kind,
        host,
        path,
        size,
        time,
        waterfall,
    ] = columns();
    h_flex()
        .flex_none()
        .h(rems(1.75))
        .w_full()
        .border_b_1()
        .border_color(c.line)
        .bg(c.panel)
        .font_family(MONO)
        .text_size(text::MICRO)
        .text_color(c.ink_3)
        .child(number.child("#"))
        .child(method.child("METHOD"))
        .child(status.child("STATUS"))
        .child(kind.child("TYPE"))
        .child(host.child("HOST"))
        .child(path.child("PATH"))
        .child(size.child("SIZE"))
        .child(time.child("TIME"))
        .child(waterfall.child("WATERFALL"))
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "a fraction of the timeline, for layout"
)]
fn row(entry: &Entry, selected: bool, end_ms: f64) -> Div {
    let c = colors();
    let [
        number,
        method,
        status,
        kind,
        host,
        path,
        size,
        time,
        waterfall,
    ] = columns();
    let full = format!("{}{}", entry.path, entry.query);
    let path_len = entry.path.len();
    let start = (entry.start_ms / end_ms).clamp(0.0, 1.0) as f32;
    let width = (entry.duration_ms() / end_ms).clamp(0.0, 1.0 - f64::from(start)) as f32;
    let bar = if entry.error.is_some() {
        c.err
    } else {
        entry.color
    };
    h_flex()
        .relative()
        .h(rems(ROW))
        .w_full()
        .border_b_1()
        .border_color(c.line_soft.opacity(0.7))
        .text_size(text::LABEL)
        .map(|this| {
            if selected {
                this.bg(c.select)
            } else {
                this.hover(|style| style.bg(c.hover))
            }
        })
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
            number
                .font_family(MONO)
                .text_size(text::MICRO)
                .text_color(c.ink_4)
                .child(SharedString::from(entry.id().to_string())),
        )
        .child(
            method
                .font_family(MONO)
                .text_size(text::MICRO)
                .text_color(
                    if matches!(entry.request.method.as_str(), "GET" | "CONNECT") {
                        c.ink_3
                    } else {
                        c.ink
                    },
                )
                .child(SharedString::from(entry.request.method.clone())),
        )
        .child(status.child(ui::status_tag(entry)))
        .child(
            kind.font_family(MONO)
                .text_size(text::MICRO)
                .text_color(if entry.kind == Kind::Document {
                    c.ink
                } else {
                    c.ink_3
                })
                .child(entry.kind.short()),
        )
        .child(
            host.font_family(MONO)
                .text_color(entry.color)
                .child(SharedString::from(entry.host.clone())),
        )
        .child(path.font_family(MONO).text_color(c.ink).child(
            StyledText::new(full.clone()).with_highlights([(
                path_len..full.len(),
                HighlightStyle {
                    color: Some(c.ink_3),
                    ..HighlightStyle::default()
                },
            )]),
        ))
        .child(
            size.font_family(MONO)
                .text_size(text::MICRO)
                .text_color(c.ink_3)
                .child(if entry.response.is_some() {
                    format::bytes(entry.size())
                } else {
                    "—".to_owned()
                }),
        )
        .child(
            time.font_family(MONO)
                .text_size(text::MICRO)
                .text_color(c.ink_3)
                .child(if entry.response.is_some() {
                    format::ms(entry.duration_ms())
                } else if entry.pending() {
                    "…".to_owned()
                } else {
                    "—".to_owned()
                }),
        )
        .child(
            waterfall.relative().h(rems(0.625)).child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(relative(start))
                    .w(relative(width))
                    .min_w(rems(0.125))
                    .bg(bar)
                    .opacity(if selected || entry.pending() {
                        1.0
                    } else {
                        0.7
                    }),
            ),
        )
}

impl Render for NetworkView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        self.refresh_rows(cx);
        let c = colors();
        let capture = self.shared.capture.read(cx);
        // The span of the traffic rather than the whole session: a proxy left
        // idle for an hour would otherwise squash every bar into a sliver.
        let end_ms = capture.end_ms().max(1.0);
        let selected = self.shared.nav.read(cx).selected();
        let rows = Rc::clone(&self.rows);
        let total = capture.entries().len();
        let nav = self.shared.nav.downgrade();
        let focus = self.focus.clone();

        let list: gpui_kit::AnyElement = if rows.is_empty() {
            ui::empty_state(
                Icon::Network,
                if total == 0 {
                    "Nothing captured yet"
                } else {
                    "No requests match"
                },
                if total == 0 {
                    "Point a browser or a client at the proxy, and every request it makes appears here as it happens."
                } else {
                    "Loosen the filter, or pick another type."
                },
            )
            .into_any_element()
        } else {
            uniform_list("network-rows", rows.len(), move |range, _, _| {
                range
                    .filter_map(|at| rows.get(at))
                    .map(|entry| {
                        let id = entry.id();
                        let nav = nav.clone();
                        let focus = focus.clone();
                        row(entry, selected == Some(id), end_ms)
                            .id(("network-row", id))
                            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                                window.focus(&focus, cx);
                                let _ = nav.update(cx, |nav, cx| {
                                    if event.click_count >= 2 {
                                        nav.open_body(id, Side::Response, cx);
                                    } else {
                                        nav.inspect(id, cx);
                                    }
                                });
                            })
                    })
                    .collect()
            })
            .track_scroll(&self.scroll)
            .size_full()
            .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            // The top bar's border already rules off the toolbar.
            .child(self.render_toolbar(window, cx).border_t_0())
            .child(header())
            .child(
                div()
                    .id("network-list")
                    .key_context(CONTEXT)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.step(1, cx)))
                    .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.step(-1, cx)))
                    .on_action(cx.listener(|this, _: &OpenSelected, _, cx| this.open_selected(cx)))
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .bg(c.panel)
                    .child(list)
                    .child(Scrollbar::vertical(&self.scroll).mode(ScrollbarMode::Hover)),
            )
    }
}
