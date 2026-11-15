//! The strip along the top: the brand, where the proxy listens, and the
//! commands that act on the whole capture.
//!
//! It is also the window's title bar on macOS and Windows: dragging it moves
//! the window, and the window buttons sit inside it.

use crate::capture::Listening;
use crate::icons::Icon;
use crate::icons::icon;
use crate::state::Page;
use crate::state::Shared;
use crate::state::StatusLine;
use crate::state::Tone;
use crate::theme::MONO;
use crate::theme::colors;
use crate::theme::text;
use crate::ui;
use gpui_kit::App;
use gpui_kit::ClickEvent;
use gpui_kit::ClipboardItem;
use gpui_kit::Context;
use gpui_kit::Div;
use gpui_kit::Entity;
use gpui_kit::FontWeight;
use gpui_kit::InteractiveElement as _;
use gpui_kit::IntoElement;
use gpui_kit::MouseButton;
use gpui_kit::MouseDownEvent;
use gpui_kit::ParentElement as _;
use gpui_kit::Pixels;
use gpui_kit::Point;
use gpui_kit::Render;
use gpui_kit::SharedString;
use gpui_kit::Styled as _;
use gpui_kit::Subscription;
use gpui_kit::Window;
use gpui_kit::WindowControlArea;
use gpui_kit::base::Button;
use gpui_kit::base::h_flex;
use gpui_kit::div;
use gpui_kit::point;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::px;
use gpui_kit::relative;
use gpui_kit::rems;
use std::rc::Rc;

/// Where macOS draws the window buttons: centred in the bar's height.
pub const TRAFFIC_LIGHTS: Point<Pixels> = point(px(12.), px(12.));

/// How far the brand starts from the left on macOS, past the window buttons.
const TRAFFIC_LIGHTS_INSET: Pixels = px(92.);

/// The bar's height. Pixels, as [`TRAFFIC_LIGHTS`] centres on it.
const HEIGHT: Pixels = px(38.);

/// What a top bar button asks the window to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    OpenHar,
    ExportHar,
    Clear,
    ToggleRecording,
}

/// What the window does with a [`Command`].
type CommandHandler = Rc<dyn Fn(Command, &mut Window, &mut App)>;

/// The top bar.
pub struct TopBar {
    shared: Shared,
    on_command: CommandHandler,
    _subscriptions: Vec<Subscription>,
}

impl TopBar {
    pub fn new(
        shared: Shared,
        on_command: impl Fn(Command, &mut Window, &mut App) + 'static,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&shared.capture, |_, _, cx| cx.notify()),
            cx.observe(&shared.nav, |_, _, cx| cx.notify()),
        ];
        Self {
            shared,
            on_command: Rc::new(on_command),
            _subscriptions: subscriptions,
        }
    }
}

/// The mark: the app icon's star, in the accent.
fn mark() -> impl IntoElement {
    icon(Icon::Star, rems(0.875)).text_color(colors().live)
}

/// Makes `region` move the window when dragged.
///
/// Windows reads the area from its hit test. macOS hands the drag to the system
/// on mouse down, and a double click does what the system setting says.
fn drag_region(region: Div) -> Div {
    region
        .window_control_area(WindowControlArea::Drag)
        .on_mouse_down(MouseButton::Left, |event: &MouseDownEvent, window, _| {
            if !cfg!(target_os = "macos") {
                return;
            }
            if event.click_count == 2 {
                window.titlebar_double_click();
            } else {
                window.start_window_move();
            }
        })
}

/// One of the minimise, maximise, and close buttons Windows needs drawn.
///
/// The glyphs come from Segoe MDL2 Assets, which ships with Windows 10 and
/// later. Windows itself acts on the click, through the hit test.
fn caption_button(
    id: &'static str,
    area: WindowControlArea,
    glyph: &'static str,
    close: bool,
) -> impl IntoElement {
    let c = colors();
    div()
        .id(id)
        .window_control_area(area)
        .flex()
        .items_center()
        .justify_center()
        .w(px(46.))
        .h_full()
        .font_family("Segoe MDL2 Assets")
        .text_size(px(10.))
        .text_color(c.ink_2)
        .hover(move |style| {
            if close {
                style
                    .bg(gpui_kit::rgb(0x00c4_2b1c))
                    .text_color(gpui_kit::white())
            } else {
                style.bg(c.hover).text_color(c.ink)
            }
        })
        .child(glyph)
}

/// Pause or resume recording, drawn as Chrome's developer tools draw it: a
/// red stop square while recording, a grey record dot while paused.
fn record_button(
    recording: bool,
    status: &Entity<StatusLine>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let c = colors();
    let (glyph, label, color) = if recording {
        (Icon::Recording, "Pause recording · ⌘R", c.live)
    } else {
        (Icon::Record, "Resume recording · ⌘R", c.ink_2)
    };
    ui::hint(
        Button::new("top-record")
            .accessibility_label(label)
            .size(rems(1.75))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .hover(|style| style.bg(c.hover))
            .child(icon(glyph, rems(1.0)).text_color(color))
            .on_click(on_click),
        status,
        label,
    )
}

/// Where the proxy listens, centred like a window title. Clicking it copies
/// the proxy's URL, or, when the proxy could not start, opens Settings.
fn address_pill(listening: &Listening, shared: &Shared) -> impl IntoElement {
    const GROUP: &str = "address-pill";
    let c = colors();
    let status = shared.status.clone();
    let nav = shared.nav.downgrade();
    let (glyph, text_color, border, caption, hint) = match listening {
        Listening::On(addr) => (
            Icon::Host,
            c.ink,
            c.line,
            addr.to_string(),
            format!("Copy the proxy's address, http://{addr}"),
        ),
        Listening::Starting => (
            Icon::Host,
            c.ink_3,
            c.line,
            "Starting…".to_owned(),
            "The proxy is starting".to_owned(),
        ),
        Listening::Failed(reason) => (
            Icon::Error,
            c.err,
            c.err.opacity(0.5),
            "Not listening".to_owned(),
            format!("Not listening: {reason}. Open Settings to change the address."),
        ),
    };
    let target = match listening {
        Listening::On(addr) => Some(format!("http://{addr}")),
        _ => None,
    };
    let failed = matches!(listening, Listening::Failed(_));
    let pill = Button::new("top-address")
        .group(GROUP)
        .accessibility_label(SharedString::from(hint.clone()))
        .flex()
        .flex_none()
        .items_center()
        .gap(rems(0.5))
        .max_w(rems(28.0))
        .h(rems(1.5))
        .pl(rems(0.625))
        .pr(rems(0.5))
        .border_1()
        .border_color(border)
        .bg(c.well)
        .hover(move |style| style.border_color(if failed { c.err } else { c.ink_4 }))
        .child(icon(glyph, rems(0.75)).text_color(if failed { c.err } else { c.ink_3 }))
        .child(
            div()
                .min_w_0()
                .truncate()
                .line_height(relative(1.5))
                .font_family(MONO)
                .text_size(text::LABEL)
                .font_weight(FontWeight::MEDIUM)
                .text_color(text_color)
                .child(caption),
        )
        .child(
            icon(
                if failed {
                    Icon::ChevronRight
                } else {
                    Icon::Copy
                },
                rems(0.75),
            )
            .text_color(gpui_kit::transparent_black())
            .group_hover(GROUP, |style| style.text_color(c.ink_3)),
        )
        .on_click(move |_, _, cx| {
            if let Some(url) = &target {
                cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                status.update(cx, |status, cx| {
                    status.say(Tone::Ok, format!("Copied {url}"), cx);
                });
            } else if failed {
                let _ = nav.update(cx, |nav, cx| nav.show(Page::Settings, cx));
            }
        });
    ui::hint(pill, &shared.status, hint)
}

/// A thin vertical rule between groups of controls.
fn rule() -> impl IntoElement {
    div().h(rems(1.0)).w_px().mx(rems(0.25)).bg(colors().line)
}

impl Render for TopBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let c = colors();
        let capture = self.shared.capture.read(cx);
        let address = address_pill(capture.listening(), &self.shared);
        let recording = capture.recording();
        let inspector = self.shared.nav.read(cx).inspector();
        let traffic_lights = cfg!(target_os = "macos") && !window.is_fullscreen();
        let caption_buttons = cfg!(target_os = "windows");
        let maximized = window.is_maximized();
        let status = &self.shared.status;
        let nav = self.shared.nav.downgrade();

        let command = |id: &'static str, glyph: Icon, label: &'static str, which: Command| {
            let on_command = Rc::clone(&self.on_command);
            ui::icon_button(id, glyph, label, status, move |_, window, cx| {
                on_command(which, window, cx);
            })
        };

        h_flex()
            .relative()
            .flex_none()
            .h(HEIGHT)
            .items_stretch()
            .border_b_1()
            .border_color(c.line)
            .bg(c.panel_2)
            .child(
                drag_region(h_flex())
                    .w(rems(15.0))
                    .flex_none()
                    .gap(rems(0.625))
                    .px(rems(1.0))
                    .when(traffic_lights, |this| this.pl(TRAFFIC_LIGHTS_INSET))
                    .border_r_1()
                    .border_color(c.line)
                    .child(mark())
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(text::LABEL)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(c.ink)
                            .child("M I R A"),
                    )
                    .child(
                        div()
                            .ml_auto()
                            .font_family(MONO)
                            .text_size(text::MICRO)
                            .text_color(c.ink_4)
                            .child("PROXY"),
                    ),
            )
            .child(drag_region(h_flex()).flex_1().min_w_0())
            .child(
                h_flex()
                    .flex_none()
                    .gap(rems(0.125))
                    .px(rems(0.5))
                    .border_l_1()
                    .border_color(c.line)
                    .child({
                        let on_command = Rc::clone(&self.on_command);
                        record_button(recording, status, move |_, window, cx| {
                            on_command(Command::ToggleRecording, window, cx);
                        })
                    })
                    .child(rule())
                    .child(command(
                        "top-open",
                        Icon::Folder,
                        "Open a HAR file · ⌘O",
                        Command::OpenHar,
                    ))
                    .child(command(
                        "top-export",
                        Icon::Download,
                        "Export the capture as HAR · ⌘S",
                        Command::ExportHar,
                    ))
                    .child(command(
                        "top-clear",
                        Icon::Trash,
                        "Clear the capture · ⌘⌫",
                        Command::Clear,
                    ))
                    .child(rule())
                    .child(ui::icon_toggle(
                        "top-inspector",
                        Icon::Inspector,
                        "Request inspector · ⌘I",
                        inspector,
                        status,
                        move |_, _, cx| {
                            let _ = nav.update(cx, |nav, cx| nav.toggle_inspector(cx));
                        },
                    )),
            )
            .when(caption_buttons, |this| {
                this.child(
                    h_flex()
                        .flex_none()
                        .items_stretch()
                        .child(caption_button(
                            "caption-minimize",
                            WindowControlArea::Min,
                            "\u{e921}",
                            false,
                        ))
                        .child(caption_button(
                            "caption-maximize",
                            WindowControlArea::Max,
                            if maximized { "\u{e923}" } else { "\u{e922}" },
                            false,
                        ))
                        .child(caption_button(
                            "caption-close",
                            WindowControlArea::Close,
                            "\u{e8bb}",
                            true,
                        )),
                )
            })
            // Centred on the window rather than on the space between the
            // brand and the commands. Only the pill takes the pointer; the
            // bar around it still drags the window.
            .child(
                h_flex()
                    .absolute()
                    .inset_0()
                    .justify_center()
                    .child(address),
            )
    }
}
