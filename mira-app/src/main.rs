//! Mira's desktop window: the proxy, running, with everything it captures on
//! screen as it happens.
//!
//! The proxy runs on its own thread and runtime (see [`proxy`]). What it
//! captures reaches the window over a channel and is drawn with GPUI and the
//! unstyled controls of GPUI Base, styled by the app itself.

// Prevents an additional console window on Windows in release. Do not remove.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod about;
mod body;
mod capture;
mod curl;
mod format;
mod hosts;
mod icons;
mod input;
mod inspector;
mod logging;
mod network;
mod overview;
mod proxy;
mod search;
mod settings;
mod sidebar;
mod site;
mod state;
mod status_bar;
mod text;
mod theme;
mod top_bar;
mod trust;
mod ui;
mod workspace;

use capture::Capture;
use capture::Captured;
use gpui_kit::AnyWindowHandle;
use gpui_kit::App;
use gpui_kit::AppContext as _;
use gpui_kit::Bounds;
use gpui_kit::Entity;
use gpui_kit::Global;
use gpui_kit::KeyBinding;
use gpui_kit::Menu;
use gpui_kit::MenuItem;
use gpui_kit::SystemMenuType;
use gpui_kit::TitlebarOptions;
use gpui_kit::WindowBounds;
use gpui_kit::WindowOptions;
use gpui_kit::base::ScrollbarMode;
use gpui_kit::base::ScrollbarStyles;
use gpui_kit::base::ScrollbarTheme;
use gpui_kit::base::Theme;
use gpui_kit::base::ThemeAppearance;
use gpui_kit::px;
use gpui_kit::size;
use mira_proxy::FileConfig;
use mira_proxy::Session;
use mira_proxy::config_file;
use proxy::ProxyControl;
use state::Nav;
use state::Page;
use state::Shared;
use state::StatusLine;
use state::Tone;
use std::path::PathBuf;
use workspace::Back;
use workspace::ClearCapture;
use workspace::CloseWindow;
use workspace::ExportHar;
use workspace::FocusFilter;
use workspace::FocusSearch;
use workspace::OpenHar;
use workspace::Quit;
use workspace::ShowAbout;
use workspace::ShowHosts;
use workspace::ShowLogs;
use workspace::ShowNetwork;
use workspace::ShowOverview;
use workspace::ShowSettings;
use workspace::ToggleInspector;
use workspace::ToggleRecording;
use workspace::Workspace;

/// How many messages from the proxy are folded in before the window redraws.
const BATCH: usize = 512;

fn main() {
    let mut log_guard = logging::init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting");

    let (settings, path, mut warning) = settings();
    let (events, received) = smol::channel::unbounded::<Captured>();
    let mut control = ProxyControl::new(settings, path, events);
    let started = control.start();
    warning = warning.or(started.warning);

    let session = Session::with_limits(started.limits);
    let app = gpui_kit::application().with_assets(icons::Assets);
    // The Dock icon, clicked while no window is showing.
    app.on_reopen(|cx| {
        if let Some((window, _)) = show_window(cx) {
            let _ = window.update(cx, |_, window, _| window.activate_window());
        }
    });
    app.run(move |cx| {
        gpui_kit::init(cx);
        apply_theme(cx);
        // Before the menus: they read the keymap when they are built, and
        // a binding added afterwards would never show beside its item.
        bind_keys(cx);
        cx.set_menus(menus());

        let capture = cx.new(|_| Capture::new(session.clone(), started.intercept));
        let shared = Shared {
            capture: capture.clone(),
            nav: cx.new(|_| Nav::new()),
            status: cx.new(|_| StatusLine::new()),
            proxy: cx.new(|_| control),
        };
        if let Some(warning) = warning.clone() {
            shared
                .status
                .update(cx, |status, cx| status.say(Tone::Error, warning, cx));
        }
        relay(capture, received.clone(), cx);

        let proxy = shared.proxy.clone();
        cx.set_global(Main {
            shared: shared.clone(),
            window: None,
            about: None,
        });
        if show_window(cx).is_none() {
            cx.quit();
            return;
        }
        register_actions(cx);

        // GPUI quits when the last window closes, except on macOS, where
        // the app and the proxy keep running until the app is quit.
        cx.on_window_closed(|cx, closed| {
            let main = cx.global_mut::<Main>();
            if main
                .window
                .as_ref()
                .is_some_and(|(window, _)| window.window_id() == closed)
            {
                main.window = None;
            }
            if main.about.is_some_and(|about| about.window_id() == closed) {
                main.about = None;
            }
        })
        .detach();
        cx.on_app_quit(move |cx| {
            proxy.update(cx, |proxy, _| proxy.stop());
            tracing::info!("quitting");
            // The app may exit without returning from `run`, so the
            // last lines are flushed here.
            drop(log_guard.take());
            async {}
        })
        .detach();
        cx.activate(true);
    });
}

/// The settings in the config file, where it is, and why it was ignored, if
/// it was.
fn settings() -> (FileConfig, Option<PathBuf>, Option<String>) {
    match config_file::load_from_default_locations() {
        Ok(Some((settings, path))) => (settings, Some(path), None),
        Ok(None) => (FileConfig::default(), None, None),
        Err(err) => {
            tracing::error!(error = %err, "using the default settings");
            let warning = format!("Using the default settings: {err}");
            (FileConfig::default(), None, Some(warning))
        }
    }
}

/// Fold what the proxy reports into the capture, a batch at a time.
fn relay(capture: Entity<Capture>, received: smol::channel::Receiver<Captured>, cx: &App) {
    cx.spawn(async move |cx| {
        while let Ok(first) = received.recv().await {
            let mut batch = vec![first];
            while batch.len() < BATCH {
                match received.try_recv() {
                    Ok(message) => batch.push(message),
                    Err(_) => break,
                }
            }
            capture.update(cx, |capture, cx| {
                for message in batch {
                    capture.ingest(message);
                }
                cx.notify();
            });
        }
    })
    .detach();
}

/// The scrollbars, in the palette.
fn apply_theme(cx: &mut App) {
    let c = theme::colors();
    let theme = Theme::global_mut(cx);
    theme.appearance = ThemeAppearance::Dark;
    theme.scrollbar = ScrollbarTheme::new()
        .with_mode(ScrollbarMode::Hover)
        .with_styles(
            ScrollbarStyles::default()
                .thumb(|thumb| thumb.bg(c.raised))
                .thumb_hover(|thumb| thumb.bg(c.ink_4))
                .thumb_active(|thumb| thumb.bg(c.ink_4)),
        );
}

/// The window's keyboard shortcuts. "secondary" is Command on macOS and
/// Control elsewhere.
fn bind_keys(cx: &mut App) {
    let requests = Some(network::CONTEXT);
    // `secondary` resolves against the host, which only `load` knows.
    let platform: Vec<(&str, Box<dyn gpui_kit::Action>)> = vec![
        ("secondary-1", Box::new(ShowOverview)),
        ("secondary-2", Box::new(ShowNetwork)),
        ("secondary-3", Box::new(ShowHosts)),
        ("secondary-k", Box::new(FocusSearch)),
        ("secondary-,", Box::new(ShowSettings)),
        ("secondary-i", Box::new(ToggleInspector)),
        ("secondary-r", Box::new(ToggleRecording)),
        ("secondary-f", Box::new(FocusFilter)),
        ("secondary-o", Box::new(OpenHar)),
        ("secondary-s", Box::new(ExportHar)),
        ("secondary-backspace", Box::new(ClearCapture)),
    ];
    let mut bindings: Vec<KeyBinding> = platform
        .into_iter()
        .filter_map(|(keys, action)| {
            KeyBinding::load(keys, action, None, false, None, cx)
                .map_err(|err| tracing::error!(keys, ?err, "could not bind"))
                .ok()
        })
        .collect();
    bindings.extend([
        KeyBinding::new("escape", Back, None),
        KeyBinding::new("down", network::SelectNext, requests),
        KeyBinding::new("up", network::SelectPrevious, requests),
        KeyBinding::new("enter", network::OpenSelected, requests),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-q", Quit, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-w", CloseWindow, None),
    ]);
    cx.bind_keys(bindings);
}

/// The window, while one is open, and what a new one is built from.
///
/// The window can close while the app runs on, so the capture and the proxy
/// live here rather than in it.
struct Main {
    shared: Shared,
    window: Option<(AnyWindowHandle, Entity<Workspace>)>,

    /// The About window, while it is open.
    about: Option<AnyWindowHandle>,
}

impl Global for Main {}

/// The window, opened first if it is closed.
fn show_window(cx: &mut App) -> Option<(AnyWindowHandle, Entity<Workspace>)> {
    let main = cx.try_global::<Main>()?;
    if let Some(open) = &main.window {
        return Some(open.clone());
    }
    let shared = main.shared.clone();
    let opened = gpui_kit::open_window(window_options(cx), cx, |window, cx| {
        cx.new(|cx| Workspace::new(shared, window, cx))
    });
    match opened {
        Ok(open) => {
            cx.global_mut::<Main>().window = Some(open.clone());
            Some(open)
        }
        Err(err) => {
            tracing::error!(error = %err, "could not open a window");
            None
        }
    }
}

/// The About window, brought to the front, or opened if it is closed.
fn show_about(cx: &mut App) {
    let Some(main) = cx.try_global::<Main>() else {
        return;
    };
    if let Some(about) = main.about {
        let _ = about.update(cx, |_, window, _| window.activate_window());
        return;
    }
    match gpui_kit::open_window(about::window_options(cx), cx, |_, cx| {
        cx.new(|_| about::About)
    }) {
        Ok((about, _)) => cx.global_mut::<Main>().about = Some(about),
        Err(err) => tracing::error!(error = %err, "could not open the About window"),
    }
}

/// Run every window command on the workspace, wherever focus is, opening the
/// window first if it is closed.
///
/// Global handlers rather than handlers on the workspace's element: with
/// nothing focused, actions reach only the window's root, above it.
fn register_actions(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &ShowLogs, cx| match logging::log_dir() {
        Some(dir) if dir.exists() => cx.open_with_system(&dir),
        _ => tracing::warn!("there is no log folder to show"),
    });
    cx.on_action(|_: &ShowAbout, cx| cx.defer(show_about));
    cx.on_action(|_: &CloseWindow, cx| {
        // Deferred, as below: the window cannot close itself mid-dispatch.
        cx.defer(|cx| {
            // The About window, when it is in front; otherwise the main one.
            let about = cx.try_global::<Main>().and_then(|main| main.about);
            let active = cx.active_window();
            let open = match about {
                Some(about) if active == Some(about) => Some(about),
                _ => cx
                    .try_global::<Main>()
                    .and_then(|main| main.window.as_ref().map(|(window, _)| *window)),
            };
            if let Some(window) = open {
                let _ = window.update(cx, |_, window, _| window.remove_window());
            }
        });
    });

    let on =
        |run: fn(&mut Workspace, &mut gpui_kit::Window, &mut gpui_kit::Context<'_, Workspace>)| {
            move |cx: &mut App| {
                // Deferred: a key or menu action is dispatched from inside the
                // window, which cannot be updated again until dispatch returns.
                cx.defer(move |cx| {
                    let Some((window, workspace)) = show_window(cx) else {
                        return;
                    };
                    let _ = window.update(cx, |_, window, cx| {
                        workspace.update(cx, |workspace, cx| run(workspace, window, cx));
                    });
                });
            }
        };
    let show_overview = on(|w, window, cx| w.show(Page::Overview, window, cx));
    let show_network = on(|w, window, cx| w.show(Page::Network, window, cx));
    let show_hosts = on(|w, window, cx| w.show(Page::Hosts, window, cx));
    let show_settings = on(|w, window, cx| w.show(Page::Settings, window, cx));
    let focus_search = on(Workspace::focus_search);
    let toggle_inspector = on(|w, _, cx| w.toggle_inspector(cx));
    let open_har = on(Workspace::open_har);
    let export_har = on(Workspace::export_har);
    let clear = on(|w, _, cx| w.clear(cx));
    let toggle_recording = on(|w, _, cx| w.toggle_recording(cx));
    let focus_filter = on(Workspace::focus_filter);
    let back = on(Workspace::back);
    cx.on_action(move |_: &ShowOverview, cx| show_overview(cx));
    cx.on_action(move |_: &ShowNetwork, cx| show_network(cx));
    cx.on_action(move |_: &ShowHosts, cx| show_hosts(cx));
    cx.on_action(move |_: &ShowSettings, cx| show_settings(cx));
    cx.on_action(move |_: &FocusSearch, cx| focus_search(cx));
    cx.on_action(move |_: &ToggleInspector, cx| toggle_inspector(cx));
    cx.on_action(move |_: &OpenHar, cx| open_har(cx));
    cx.on_action(move |_: &ExportHar, cx| export_har(cx));
    cx.on_action(move |_: &ClearCapture, cx| clear(cx));
    cx.on_action(move |_: &ToggleRecording, cx| toggle_recording(cx));
    cx.on_action(move |_: &FocusFilter, cx| focus_filter(cx));
    cx.on_action(move |_: &Back, cx| back(cx));
}

/// The menu bar. Title case, as macOS menus are.
fn menus() -> Vec<Menu> {
    vec![
        Menu::new("Mira").items([
            MenuItem::action("About Mira", ShowAbout),
            MenuItem::separator(),
            MenuItem::action("Settings…", ShowSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Quit Mira", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("Open HAR…", OpenHar),
            MenuItem::action("Export HAR…", ExportHar),
            MenuItem::separator(),
            MenuItem::action("Pause or Resume Recording", ToggleRecording),
            MenuItem::action("Clear Capture", ClearCapture),
            MenuItem::separator(),
            MenuItem::action("Close Window", CloseWindow),
        ]),
        Menu::new("View").items([
            MenuItem::action("Overview", ShowOverview),
            MenuItem::action("Network", ShowNetwork),
            MenuItem::action("Hosts", ShowHosts),
            MenuItem::action("Search", FocusSearch),
            MenuItem::action("Filter Requests", FocusFilter),
            MenuItem::separator(),
            MenuItem::action("Inspector", ToggleInspector),
        ]),
        Menu::new("Help").items([MenuItem::action("Show Log Folder", ShowLogs)]),
    ]
}

/// The window: centred, 1440 by 900, and no smaller than the table and the
/// inspector side by side.
///
/// Pixels rather than the rem scale: these are the platform's window
/// geometry, which the rem scale is drawn inside.
fn window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1440.), px(900.)),
            cx,
        ))),
        window_min_size: Some(size(px(1000.), px(600.))),
        // The top bar is the title bar on macOS and Windows. Linux keeps the
        // system's own, as client-side decorations would also need resize
        // edges and shadows.
        titlebar: Some(TitlebarOptions {
            title: Some("Mira".into()),
            appears_transparent: cfg!(any(target_os = "macos", target_os = "windows")),
            traffic_light_position: Some(top_bar::TRAFFIC_LIGHTS),
        }),
        app_owns_titlebar_drag: true,
        app_id: Some("mira".to_owned()),
        ..WindowOptions::default()
    }
}
