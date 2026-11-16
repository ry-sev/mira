//! The About window: the app icon, the version and commit the app was built
//! from, and links to both on GitHub.

use crate::icons::APP_ICON;
use crate::theme::MONO;
use crate::theme::SANS;
use crate::theme::colors;
use crate::theme::text;
use crate::ui;
use gpui_kit::App;
use gpui_kit::Bounds;
use gpui_kit::Context;
use gpui_kit::FontWeight;
use gpui_kit::InteractiveElement;
use gpui_kit::IntoElement;
use gpui_kit::MouseButton;
use gpui_kit::ParentElement as _;
use gpui_kit::Render;
use gpui_kit::SharedString;
use gpui_kit::Styled as _;
use gpui_kit::TitlebarOptions;
use gpui_kit::Window;
use gpui_kit::WindowBounds;
use gpui_kit::WindowOptions;
use gpui_kit::base::h_flex;
use gpui_kit::base::v_flex;
use gpui_kit::div;
use gpui_kit::img;
use gpui_kit::px;
use gpui_kit::rems;
use gpui_kit::size;

/// Where the source lives.
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// What Mira is. The workspace's description: this crate's own describes
/// the window rather than the app.
const DESCRIPTION: &str = "HTTP/HTTPS proxy for inspecting and monitoring web traffic.";

/// The version this build was made from.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit this build was made from, when it was built from a checkout.
const COMMIT: Option<&str> = option_env!("MIRA_COMMIT");

/// The release page of `version`. Releases are tagged `vX.Y.Z`.
fn release_url(version: &str) -> String {
    format!("{REPOSITORY}/releases/tag/v{version}")
}

/// The history up to and including `commit`.
fn commit_url(commit: &str) -> String {
    format!("{REPOSITORY}/commits/{commit}")
}

/// The window's contents. It holds no state: everything it shows is fixed
/// when the app is built.
pub struct About;

/// The window: small, centred, of a fixed size, and moved by dragging
/// anywhere in it.
pub fn window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(300.), px(380.)),
            cx,
        ))),
        // On macOS the content runs up under the window buttons, with no
        // title bar of its own. Elsewhere the system's title bar stays.
        titlebar: Some(TitlebarOptions {
            title: Some("About Mira".into()),
            appears_transparent: cfg!(target_os = "macos"),
            traffic_light_position: None,
        }),
        // The window moves itself, from anywhere in it; see `drag_to_move`.
        app_owns_titlebar_drag: true,
        is_resizable: false,
        is_minimizable: false,
        app_id: Some("mira".to_owned()),
        ..WindowOptions::default()
    }
}

/// Makes `element` move the window when dragged.
///
/// A no-op on Windows, where the system's title bar moves the window.
fn drag_to_move<E: InteractiveElement>(element: E) -> E {
    element.on_mouse_down(MouseButton::Left, |_, window, _| {
        window.start_window_move();
    })
}

/// Keeps a press on `element` from moving the window, so it reaches the
/// element as a click.
///
/// The element's own click listener has already seen the press by then.
fn no_drag<E: InteractiveElement>(element: E) -> E {
    element.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// One row of facts: a caption on the left, a link on the right.
fn fact(caption: &'static str, value: SharedString, url: String) -> impl IntoElement {
    let c = colors();
    h_flex()
        .gap(rems(0.5))
        .child(
            div()
                .w(rems(4.5))
                .text_right()
                .text_color(c.ink_3)
                .child(caption),
        )
        .child(no_drag(
            ui::row_button(caption)
                .accessibility_label(format!("{caption} {value}, opens in the browser"))
                .font_family(MONO)
                .text_color(c.focus)
                .hover(|style| style.underline())
                .child(value)
                .on_click(move |_, _, cx| cx.open_url(&url)),
        ))
}

impl Render for About {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        let c = colors();
        drag_to_move(v_flex())
            .size_full()
            .items_center()
            .justify_center()
            .gap(rems(0.75))
            .bg(c.panel_2)
            .text_color(c.ink)
            .font_family(SANS)
            .text_size(text::UI)
            .child(img(APP_ICON).size(rems(6.0)))
            .child(
                div()
                    .text_size(rems(1.375))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Mira"),
            )
            .child(
                div()
                    .max_w(rems(14.0))
                    .text_center()
                    .text_color(c.ink_2)
                    .child(DESCRIPTION),
            )
            .child(
                v_flex()
                    .gap(rems(0.125))
                    .child(fact("Version", VERSION.into(), release_url(VERSION)))
                    .children(
                        COMMIT.map(|commit| fact("Commit", commit.into(), commit_url(commit))),
                    ),
            )
            .child(
                div().pt(rems(0.75)).child(no_drag(
                    ui::text_button("github", "GitHub", false)
                        .accessibility_label("GitHub, opens in the browser")
                        .on_click(|_, _, cx| cx.open_url(REPOSITORY)),
                )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_release_url_points_at_the_tag() {
        assert_eq!(
            release_url("1.2.3"),
            "https://github.com/ry-sev/mira/releases/tag/v1.2.3"
        );
    }

    #[test]
    fn test_commit_url_points_at_the_history() {
        assert_eq!(
            commit_url("abc123def"),
            "https://github.com/ry-sev/mira/commits/abc123def"
        );
    }
}
