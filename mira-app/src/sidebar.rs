//! The sidebar: search, the pages of the capture, the hosts seen so far,
//! and the settings.

use crate::format;
use crate::icons::Icon;
use crate::icons::icon;
use crate::input;
use crate::state::Page;
use crate::state::Shared;
use crate::theme::MONO;
use crate::theme::colors;
use crate::theme::text;
use crate::ui;
use gpui_kit::Context;
use gpui_kit::Div;
use gpui_kit::ElementId;
use gpui_kit::Entity;
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
use gpui_kit::base::input::InputEvent;
use gpui_kit::base::input::InputState;
use gpui_kit::div;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::relative;
use gpui_kit::rems;

/// How many hosts the sidebar lists.
const LISTED_HOSTS: usize = 24;

/// The sidebar.
pub struct Sidebar {
    shared: Shared,
    search: Entity<InputState>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl Sidebar {
    pub fn new(shared: Shared, search: Entity<InputState>, cx: &mut Context<'_, Self>) -> Self {
        let nav = shared.nav.downgrade();
        let subscriptions = vec![
            cx.observe(&shared.capture, |_, _, cx| cx.notify()),
            cx.observe(&shared.nav, |_, _, cx| cx.notify()),
            cx.subscribe(&search, move |_, state, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let query = state.read(cx).value().to_string();
                    let _ = nav.update(cx, |nav, cx| nav.search(query, cx));
                }
            }),
        ];
        Self {
            shared,
            search,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    /// A titled group: room above it, then its title on a row of its own.
    fn group(title: &'static str) -> Div {
        div().pt(rems(1.0)).child(
            h_flex()
                .h(rems(1.5))
                .mb(rems(0.125))
                .pl(rems(1.0))
                .pr(rems(0.375))
                .child(ui::label(title)),
        )
    }
}

/// One page link.
fn item(id: impl Into<ElementId>, glyph: Icon, caption: &'static str, active: bool) -> Button {
    let c = colors();
    ui::row_button(id)
        .selected(active)
        .accessibility_label(caption)
        .relative()
        .w_full()
        .h(rems(1.75))
        .pl(rems(1.0))
        .pr(rems(0.75))
        .flex()
        .items_center()
        .gap(rems(0.625))
        .text_size(text::UI)
        .map(|this| {
            if active {
                this.bg(c.select).text_color(c.ink).child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(rems(0.125))
                        .bg(c.live),
                )
            } else {
                this.text_color(c.ink_2)
                    .hover(|style| style.bg(c.hover).text_color(c.ink))
            }
        })
        .child(icon(glyph, rems(0.875)).text_color(c.ink_3))
        .child(div().flex_1().truncate().child(caption))
}

/// One host in the list.
fn host_item(
    id: impl Into<ElementId>,
    name: &str,
    color: Hsla,
    count: usize,
    active: bool,
) -> Button {
    let c = colors();
    ui::row_button(id)
        .selected(active)
        .accessibility_label(SharedString::from(name.to_owned()))
        .relative()
        .w_full()
        .h(rems(1.75))
        .pl(rems(1.0))
        .pr(rems(0.75))
        .flex()
        .items_center()
        .gap(rems(0.625))
        .map(|this| {
            if active {
                this.bg(c.select).child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(rems(0.125))
                        .bg(color),
                )
            } else {
                this.hover(|style| style.bg(c.hover))
            }
        })
        .child(div().size(rems(0.5)).flex_none().bg(color))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(MONO)
                .text_size(text::LABEL)
                .text_color(color)
                .child(SharedString::from(name.to_owned())),
        )
        .child(
            div()
                .font_family(MONO)
                .text_size(text::MICRO)
                .text_color(c.ink_4)
                .child(format::count(count)),
        )
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let c = colors();
        let status = self.shared.status.clone();
        let capture = self.shared.capture.read(cx);
        let mut hosts = capture.hosts();
        hosts.sort_by_key(|h| std::cmp::Reverse(h.requests));
        hosts.truncate(LISTED_HOSTS);
        let nav_state = self.shared.nav.read(cx);
        let page = nav_state.page();
        let host_filter = nav_state.host_filter().map(str::to_owned);

        let page_item = |id: &'static str,
                         glyph: Icon,
                         caption: &'static str,
                         target: Page,
                         shortcut: &'static str| {
            let nav = self.shared.nav.downgrade();
            let active = page == target || (target == Page::Network && page == Page::Body);
            ui::hint(
                item(id, glyph, caption, active).on_click(move |_, _, cx| {
                    let _ = nav.update(cx, |nav, cx| nav.show(target, cx));
                }),
                &status,
                SharedString::from(format!("{caption} · {shortcut}")),
            )
        };

        v_column()
            .size_full()
            .bg(c.panel_2)
            .border_r_1()
            .border_color(c.line)
            .child(
                h_flex()
                    .flex_none()
                    .h(rems(2.5))
                    .px(rems(0.625))
                    .border_b_1()
                    .border_color(c.line)
                    .child(input::field(
                        "sidebar-search",
                        &self.search,
                        Icon::Search,
                        relative(1.0),
                        window,
                        cx,
                    )),
            )
            .child(
                div()
                    .flex_none()
                    .child(Self::group("Capture"))
                    .child(page_item(
                        "sidebar-overview",
                        Icon::Overview,
                        "Overview",
                        Page::Overview,
                        "⌘1",
                    ))
                    .child(page_item(
                        "sidebar-network",
                        Icon::Network,
                        "Network",
                        Page::Network,
                        "⌘2",
                    ))
                    .child(page_item(
                        "sidebar-hosts",
                        Icon::Host,
                        "Hosts",
                        Page::Hosts,
                        "⌘3",
                    ))
                    .child(page_item(
                        "sidebar-search-page",
                        Icon::Search,
                        "Search",
                        Page::Search,
                        "⌘K",
                    ))
                    .child(Self::group("Hosts")),
            )
            // Only the hosts scroll, so the pages stay in reach.
            .child(ui::scroll_area(
                "sidebar-hosts-scroll",
                &self.scroll,
                div()
                    .pb(rems(1.0))
                    .when(hosts.is_empty(), |this| {
                        this.child(
                            div()
                                .pl(rems(1.0))
                                .pt(rems(0.25))
                                .child(ui::quiet("None yet.")),
                        )
                    })
                    .children(hosts.into_iter().map(|host| {
                        let nav = self.shared.nav.downgrade();
                        let active = host_filter.as_deref() == Some(host.name.as_str());
                        let name = host.name.clone();
                        host_item(
                            SharedString::from(format!("sidebar-host-{}", host.name)),
                            &host.name,
                            host.color,
                            host.requests,
                            active,
                        )
                        .on_click(move |_, _, cx| {
                            let target = if active { None } else { Some(name.clone()) };
                            let _ = nav.update(cx, |nav, cx| nav.filter_host(target, cx));
                        })
                    })),
            ))
            .child(ui::footer().child(page_item(
                "sidebar-settings",
                Icon::Settings,
                "Settings",
                Page::Settings,
                "⌘,",
            )))
    }
}

fn v_column() -> Div {
    div().flex().flex_col()
}
