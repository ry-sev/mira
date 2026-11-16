//! The window: the top bar over the sidebar, the page, and the inspector,
//! with the status bar along the bottom.
//!
//! The workspace owns the views and the commands that act on the whole
//! capture: opening and exporting HAR files, and clearing.

use crate::body::BodyView;
use crate::hosts::HostsView;
use crate::input::new_input;
use crate::inspector::InspectorView;
use crate::network::NetworkView;
use crate::overview::OverviewView;
use crate::search::SearchView;
use crate::settings::SettingsView;
use crate::sidebar::Sidebar;
use crate::state::Page;
use crate::state::Shared;
use crate::state::Tone;
use crate::status_bar::StatusBar;
use crate::theme::SANS;
use crate::theme::colors;
use crate::theme::text;
use crate::top_bar::Command;
use crate::top_bar::TopBar;
use gpui_kit::AnyView;
use gpui_kit::AppContext as _;
use gpui_kit::Context;
use gpui_kit::Entity;
use gpui_kit::FocusHandle;
use gpui_kit::Focusable as _;
use gpui_kit::InteractiveElement as _;
use gpui_kit::IntoElement;
use gpui_kit::ParentElement as _;
use gpui_kit::PathPromptOptions;
use gpui_kit::Render;
use gpui_kit::Styled as _;
use gpui_kit::Subscription;
use gpui_kit::Window;
use gpui_kit::base::h_flex;
use gpui_kit::base::input::InputState;
use gpui_kit::div;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::rems;
use mira_proxy::Session;
use std::path::PathBuf;

/// The key context the window's own bindings live in.
pub const CONTEXT: &str = "Workspace";

/// Quits.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct Quit;

/// Opens the About window.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct ShowAbout;

/// Closes the window, leaving the app and the proxy running on macOS.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct CloseWindow;

/// Opens the folder the logs are written to.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct ShowLogs;

/// Opens a HAR file in place of the capture.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct OpenHar;

/// Writes the capture to a HAR file.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct ExportHar;

/// Forgets every captured exchange.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct ClearCapture;

/// Shows the overview.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct ShowOverview;

/// Shows every request.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct ShowNetwork;

/// Shows every host.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct ShowHosts;

/// Shows the settings.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct ShowSettings;

/// Moves to the search field.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct FocusSearch;

/// Pauses or resumes recording.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct ToggleRecording;

/// Moves to the Network filter.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct FocusFilter;

/// Shows or hides the inspector.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct ToggleInspector;

/// Leaves a body for Network, or clears the selection.
#[derive(Clone, Debug, Default, Eq, PartialEq, gpui_kit::Action)]
#[action(namespace = mira)]
pub struct Back;

/// The window's root view.
pub struct Workspace {
    shared: Shared,
    search: Entity<InputState>,
    top_bar: Entity<TopBar>,
    sidebar: Entity<Sidebar>,
    status_bar: Entity<StatusBar>,
    overview: Entity<OverviewView>,
    network: Entity<NetworkView>,
    hosts: Entity<HostsView>,
    search_view: Entity<SearchView>,
    body: Entity<BodyView>,
    settings: Entity<SettingsView>,
    inspector: Entity<InspectorView>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(shared: Shared, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let search = new_input("Search URLs, headers, bodies", window, cx);
        let workspace = cx.entity().downgrade();
        let sidebar = cx.new(|cx| Sidebar::new(shared.clone(), search.clone(), cx));
        let top_bar = cx.new(|cx| {
            TopBar::new(
                shared.clone(),
                move |command, window, cx| {
                    let _ = workspace.update(cx, |this, cx| this.run(command, window, cx));
                },
                cx,
            )
        });
        let network = cx.new(|cx| NetworkView::new(shared.clone(), window, cx));
        let body = cx.new(|cx| BodyView::new(shared.clone(), window, cx));
        let settings = cx.new(|cx| SettingsView::new(shared.clone(), window, cx));
        let subscriptions = vec![cx.observe(&shared.nav, |_, _, cx| cx.notify())];
        // The request list starts focused, so the arrow keys work at once.
        window.focus(&network.read(cx).focus_handle(cx), cx);
        let focus = cx.focus_handle();
        Self {
            top_bar,
            status_bar: cx.new(|cx| StatusBar::new(shared.clone(), cx)),
            overview: cx.new(|cx| OverviewView::new(shared.clone(), cx)),
            hosts: cx.new(|cx| HostsView::new(shared.clone(), cx)),
            search_view: cx.new(|cx| SearchView::new(shared.clone(), cx)),
            inspector: cx.new(|cx| InspectorView::new(shared.clone(), cx)),
            network,
            body,
            settings,
            sidebar,
            search,
            focus,
            shared,
            _subscriptions: subscriptions,
        }
    }

    fn run(&mut self, command: Command, window: &mut Window, cx: &mut Context<'_, Self>) {
        match command {
            Command::OpenHar => self.open_har(window, cx),
            Command::ExportHar => self.export_har(window, cx),
            Command::Clear => self.clear(cx),
            Command::ToggleRecording => self.toggle_recording(cx),
        }
    }

    pub fn toggle_recording(&self, cx: &mut Context<'_, Self>) {
        let recording = self.shared.capture.update(cx, |capture, cx| {
            capture.set_recording(!capture.recording());
            cx.notify();
            capture.recording()
        });
        self.shared.status.update(cx, |status, cx| {
            let message = if recording {
                "Recording resumed"
            } else {
                "Recording paused. Traffic still passes through the proxy."
            };
            status.say(Tone::Ok, message, cx);
        });
    }

    pub fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.shared
            .nav
            .update(cx, |nav, cx| nav.show(Page::Network, cx));
        self.network
            .update(cx, |network, cx| network.focus_filter(window, cx));
    }

    pub fn show(&self, page: Page, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.shared.nav.update(cx, |nav, cx| nav.show(page, cx));
        if page == Page::Network {
            window.focus(&self.network.read(cx).focus_handle(cx), cx);
        }
    }

    pub fn toggle_inspector(&self, cx: &mut Context<'_, Self>) {
        self.shared
            .nav
            .update(cx, |nav, cx| nav.toggle_inspector(cx));
    }

    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.search
            .update(cx, |search, cx| search.focus(window, cx));
    }

    /// Leave a body for Network; anywhere else, clear the selection.
    pub fn back(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.shared.nav.read(cx).page() == Page::Body {
            self.show(Page::Network, window, cx);
        } else {
            self.shared.nav.update(cx, |nav, cx| nav.select(None, cx));
        }
    }

    pub fn clear(&self, cx: &mut Context<'_, Self>) {
        self.shared.capture.update(cx, |capture, cx| {
            capture.clear();
            cx.notify();
        });
        self.shared.nav.update(cx, |nav, cx| {
            nav.select(None, cx);
            nav.filter_host(None, cx);
        });
        self.shared.status.update(cx, |status, cx| {
            status.say(Tone::Ok, "Cleared the capture", cx);
        });
    }

    pub fn open_har(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open".into()),
        });
        let limits = self.shared.capture.read(cx).session().limits();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let name = file_name(&path);
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let json = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                    Session::from_har_json_with_limits(&json, limits).map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| match loaded {
                Ok(session) => {
                    let count = session.len();
                    this.shared.capture.update(cx, |capture, cx| {
                        capture.replace_with(&session);
                        cx.notify();
                    });
                    this.shared.nav.update(cx, |nav, cx| {
                        nav.select(None, cx);
                        nav.filter_host(None, cx);
                    });
                    this.shared.status.update(cx, |status, cx| {
                        status.say(
                            Tone::Ok,
                            format!(
                                "Opened {} from {name}",
                                crate::format::plural(count, "exchange")
                            ),
                            cx,
                        );
                    });
                }
                Err(reason) => this.shared.status.update(cx, |status, cx| {
                    status.say(Tone::Error, format!("Couldn't open {name}: {reason}"), cx);
                }),
            });
        })
        .detach();
    }

    pub fn export_har(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let session = self.shared.capture.read(cx).session().clone();
        if session.is_empty() {
            self.shared.status.update(cx, |status, cx| {
                status.say(Tone::Error, "Nothing to export yet", cx);
            });
            return;
        }
        let directory = std::env::var_os("HOME").map_or_else(PathBuf::new, PathBuf::from);
        let suggested = format!("mira-{}.har", chrono::Local::now().format("%Y%m%d-%H%M%S"));
        let picked = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = picked.await else {
                return;
            };
            let name = file_name(&path);
            let count = session.len();
            let written = cx
                .background_executor()
                .spawn(async move {
                    let json = session.to_har_json_pretty().map_err(|e| e.to_string())?;
                    std::fs::write(&path, json).map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.shared.status.update(cx, |status, cx| match written {
                    Ok(()) => status.say(
                        Tone::Ok,
                        format!(
                            "Saved {} to {name}",
                            crate::format::plural(count, "exchange")
                        ),
                        cx,
                    ),
                    Err(reason) => {
                        status.say(Tone::Error, format!("Couldn't save {name}: {reason}"), cx);
                    }
                });
            });
        })
        .detach();
    }

    fn page(&self, cx: &Context<'_, Self>) -> AnyView {
        match self.shared.nav.read(cx).page() {
            Page::Overview => self.overview.clone().into(),
            Page::Network => self.network.clone().into(),
            Page::Hosts => self.hosts.clone().into(),
            Page::Search => self.search_view.clone().into(),
            Page::Body => self.body.clone().into(),
            Page::Settings => self.settings.clone().into(),
        }
    }
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let c = colors();
        let inspector = self.shared.nav.read(cx).inspector();
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .flex()
            .flex_col()
            .size_full()
            .bg(c.void)
            .text_color(c.ink)
            .font_family(SANS)
            .text_size(text::UI)
            .child(self.top_bar.clone())
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(div().w(rems(15.0)).flex_none().child(self.sidebar.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .bg(c.panel)
                            .child(self.page(cx)),
                    )
                    .when(inspector, |this| {
                        this.child(
                            div()
                                .w(rems(22.5))
                                .flex_none()
                                .child(self.inspector.clone()),
                        )
                    }),
            )
            .child(self.status_bar.clone())
    }
}
