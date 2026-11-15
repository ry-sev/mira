//! Mira - HTTP/HTTPS intercepting proxy
//!
//! A debugging proxy for inspecting and modifying HTTP traffic.

mod logging;
mod tui;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use crossterm::event::{self};
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;
use mira_proxy::CertificateAuthority;
use mira_proxy::Config;
use mira_proxy::Proxy;
use mira_proxy::Session;
use mira_proxy::SessionLimits;
use mira_proxy::TlsConfig;
use mira_proxy::config_file as config;
use parking_lot::Mutex;
use parking_lot::RawMutex;
use parking_lot::lock_api::MutexGuard;
use ratatui::Terminal;
use ratatui::prelude::CrosstermBackend;
use std::fs;
use std::io::stdout;
use std::io::{self};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;
use tracing::warn;
use tui::App;
use tui::app::ExchangeRow;
use tui::app::FocusPane;
use tui::app::ResponseUpdate;
use tui::draw;

#[derive(Parser, Debug)]
#[command(name = "mira-cli")]
#[command(version, about, long_about = None)]
struct Args {
    /// Address to bind to [default: 127.0.0.1]
    #[arg(short, long)]
    bind: Option<IpAddr>,

    /// Port to listen on [default: 8888]
    #[arg(short, long)]
    port: Option<u16>,

    /// Export captured traffic to HAR file on exit (default: <timestamp>.har)
    #[arg(long, value_name = "FILE", num_args = 0..=1, require_equals = false)]
    #[allow(clippy::option_option)]
    har: Option<Option<PathBuf>>,

    /// Enable HTTPS interception (requires trusting the CA certificate)
    #[arg(short, long)]
    intercept: bool,

    /// Directory to store CA certificate and key (default: ~/.mira)
    #[arg(long, value_name = "DIR")]
    ca_dir: Option<PathBuf>,

    /// Hosts to pass through without interception (can be specified multiple
    /// times) Supports wildcards: *.example.com
    #[arg(long, value_name = "HOST")]
    passthrough: Vec<String>,

    /// Allow connections from external (public) IP addresses
    ///
    /// By default, the proxy only accepts connections from private/local IP
    /// addresses (localhost, LAN). Enable this flag to allow connections
    /// from the public internet. WARNING: Only enable this if you
    /// understand the security implications.
    #[arg(long)]
    allow_external: bool,

    /// Path to config file (TOML format)
    ///
    /// If not specified, searches for config in standard XDG locations.
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Disable automatic config file loading
    ///
    /// By default, mira searches for a config file in standard locations.
    /// Use this flag to skip config file loading entirely.
    #[arg(long, conflicts_with = "config")]
    no_config: bool,

    /// Connection timeout in seconds (default: 30)
    #[arg(long, value_name = "SECONDS")]
    connect_timeout: Option<u64>,

    /// Read timeout in seconds (default: 60)
    #[arg(long, value_name = "SECONDS")]
    read_timeout: Option<u64>,

    /// Write timeout in seconds (default: 60)
    #[arg(long, value_name = "SECONDS")]
    write_timeout: Option<u64>,

    /// Maximum body size in megabytes (default: 500). Larger requests are
    /// rejected; larger responses are relayed but captured only in part
    #[arg(long, value_name = "MB")]
    max_body_size: Option<usize>,

    /// Maximum request/response header block size in kilobytes (default: 64)
    #[arg(long, value_name = "KB")]
    max_header_size: Option<usize>,

    /// Print the effective configuration and exit
    ///
    /// Useful for debugging configuration file loading and CLI overrides.
    /// Prints the merged configuration (CLI args + config file + defaults) as
    /// TOML.
    #[arg(long)]
    dump_config: bool,

    /// Load a HAR file at startup
    ///
    /// Imports exchanges from a HAR file into the session. Supports both
    /// standard HAR files (from browsers, other proxies) and Mira-exported
    /// HAR files with full-fidelity extension fields.
    #[arg(long, value_name = "FILE")]
    load: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // The guard MUST live until exit, or buffered log lines are lost.
    let _log_guard = logging::init().context("failed to initialize logging")?;

    if let Some(log_path) = logging::log_file_path() {
        info!(path = %log_path.display(), "logging initialized");
    }

    let args = Args::parse();

    // Load configuration: CLI args override config file, which overrides defaults
    let config = build_config(&args)?;

    if args.dump_config {
        let file_config = config::FileConfig::from(&config);
        let toml = config::to_toml(&file_config)?;
        println!("{toml}");
        return Ok(());
    }

    let tls_config = if config.https.intercept {
        let ca = if let Some(ca_dir) = &config.https.ca_dir {
            CertificateAuthority::load_or_generate_in(ca_dir)
                .context("failed to load or generate CA")?
        } else {
            CertificateAuthority::load_or_generate().context("failed to load or generate CA")?
        };

        // Printed before the TUI takes over the terminal. The library only
        // builds the text, so the CLI chooses where it goes.
        print!("{}", CertificateAuthority::trust_instructions()?);

        Some(TlsConfig::new(ca))
    } else {
        None
    };

    let session = if let Some(har_path) = &args.load {
        let loaded = load_har(har_path, config.session_limits)?;
        Some(loaded)
    } else if args.har.is_some() {
        Some(Session::with_limits(config.session_limits))
    } else {
        None
    };

    let app = Arc::new(Mutex::new(App::with_max_rows(
        config.session_limits.max_exchanges,
    )));

    // Store session in app for clearing
    if let Some(s) = &session {
        app.lock().set_session(s.clone());
    }

    // If we loaded a session, populate the TUI with the exchanges
    if args.load.is_some()
        && let Some(s) = &session
    {
        let mut app_guard = app.lock();
        for captured in s.exchanges() {
            let row = ExchangeRow::from_exchange(&captured.exchange);
            app_guard.add_exchange(row);
        }
    }

    let proxy = build_proxy(config, tls_config, session.clone(), Arc::clone(&app));

    let result = run_tui(proxy, app).await;

    if let Some(session) = session
        && args.har.is_some()
    {
        let har_path = args.har.flatten().unwrap_or_else(|| {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            PathBuf::from(format!("{timestamp}.har"))
        });
        export_har(&session, &har_path)?;
    }

    result
}

/// Build the proxy with callbacks that update the TUI.
fn build_proxy(
    config: Config,
    tls_config: Option<TlsConfig>,
    session: Option<Session>,
    app: Arc<Mutex<App>>,
) -> Proxy {
    let mut builder = Proxy::builder(config);

    if let Some(tls) = tls_config {
        builder = builder.with_tls(tls);
    }

    if let Some(s) = session {
        builder = builder.with_session(s);
    }

    let app_for_request = Arc::clone(&app);

    builder
        // Show requests immediately as they come in (before response)
        .on_request(move |request| {
            let row = ExchangeRow::from_request(&request);
            app_for_request.lock().add_exchange(row);
        })
        // Update the row when the response arrives
        .on_exchange(move |exchange| {
            let update = ResponseUpdate::from_exchange(&exchange);
            app.lock().update_exchange(exchange.request.id, update);
        })
        .build()
}

/// Run the TUI event loop.
async fn run_tui(proxy: Proxy, app: Arc<Mutex<App>>) -> Result<()> {
    // Bind before the TUI takes over the terminal. Binding inside the
    // spawned task would leave a port-in-use error unseen behind a TUI
    // that looks like it is working.
    let listener = proxy.bind().await.context("failed to start the proxy")?;

    enable_raw_mode().context("failed to enable raw mode")?;
    stdout()
        .execute(EnterAlternateScreen)
        .context("failed to enter alternate screen")?;

    let mut terminal =
        Terminal::new(CrosstermBackend::new(stdout())).context("failed to create terminal")?;

    // Spawn the proxy in the background with a graceful-shutdown channel.
    // Aborting mid-connection would lose any in-flight exchanges (their
    // `on_exchange` callback never fires, so they don't land in the
    // session for HAR export). `serve_until` stops accepting new work on
    // shutdown and drains in-flight handlers within a bounded window.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let proxy_handle = tokio::spawn(async move {
        // If the sender drops without signaling, treat that as shutdown
        // too — we still want to return rather than hang.
        let shutdown = async move {
            let _ = shutdown_rx.await;
        };
        proxy.serve_until(listener, shutdown).await
    });

    let result = event_loop(&mut terminal, app).await;

    disable_raw_mode().context("failed to disable raw mode")?;
    stdout()
        .execute(LeaveAlternateScreen)
        .context("failed to leave alternate screen")?;

    // Signal graceful shutdown and wait (bounded) for the proxy to drain
    // before we proceed to serialize the session.
    let _ = shutdown_tx.send(());
    match tokio::time::timeout(Duration::from_secs(12), proxy_handle).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => tracing::warn!(error = %e, "proxy returned error during shutdown"),
        Ok(Err(join_err)) => tracing::warn!(error = %join_err, "proxy task panicked"),
        Err(_elapsed) => {
            tracing::warn!("proxy did not drain in time; in-flight exchanges may be lost");
        }
    }

    result
}

/// Main event loop for the TUI.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: Arc<Mutex<App>>,
) -> Result<()> {
    loop {
        {
            let mut app = app.lock();
            terminal
                .draw(|frame| draw(frame, &mut app))
                .context("failed to draw")?;
        }

        // Poll for events with a timeout to allow the UI to refresh
        if event::poll(Duration::from_millis(100)).context("failed to poll events")?
            && let Event::Key(key) = event::read().context("failed to read event")?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            let mut app = app.lock();

            // Handle filter input mode first
            if app.filter_active {
                match key.code {
                    KeyCode::Esc => {
                        app.cancel_filter();
                    }
                    KeyCode::Enter => {
                        app.confirm_filter();
                    }
                    KeyCode::Backspace => {
                        app.filter_pop();
                    }
                    KeyCode::Char(c) => {
                        app.filter_push(c);
                    }
                    _ => {}
                }
                if app.should_quit {
                    return Ok(());
                }
                continue;
            }

            match (app.show_detail, app.focus) {
                (true, FocusPane::Request | FocusPane::Response) => {
                    // Detail pane is focused
                    match key.code {
                        KeyCode::Char('q') => {
                            app.quit();
                        }
                        KeyCode::Esc => {
                            app.close_detail();
                        }
                        // Tab cycles through panes: Request -> Response -> Table -> Request
                        KeyCode::Tab => {
                            app.next_focus();
                        }
                        KeyCode::BackTab => {
                            app.prev_focus();
                        }
                        KeyCode::Char('l') | KeyCode::Right => {
                            app.next_tab();
                        }
                        KeyCode::Char('h') | KeyCode::Left => {
                            app.prev_tab();
                        }
                        KeyCode::Char('1') => {
                            if app.focus == FocusPane::Request {
                                app.set_request_tab(1);
                            } else {
                                app.set_response_tab(1);
                            }
                        }
                        KeyCode::Char('2') => {
                            if app.focus == FocusPane::Request {
                                app.set_request_tab(2);
                            } else {
                                app.set_response_tab(2);
                            }
                        }
                        KeyCode::Char('3') => {
                            if app.focus == FocusPane::Request {
                                app.set_request_tab(3);
                            } else {
                                app.set_response_tab(3);
                            }
                        }
                        KeyCode::Char('4') => {
                            if app.focus == FocusPane::Request {
                                app.set_request_tab(4);
                            } else {
                                app.set_response_tab(4);
                            }
                        }
                        KeyCode::Char('5') if app.focus == FocusPane::Response => {
                            app.set_response_tab(5);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.scroll_down(1);
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.scroll_up(1);
                        }
                        KeyCode::PageDown | KeyCode::Char('d') => {
                            app.scroll_down(10);
                        }
                        KeyCode::PageUp | KeyCode::Char('u') => {
                            app.scroll_up(10);
                        }
                        KeyCode::Home | KeyCode::Char('g') => {
                            app.reset_scroll();
                        }
                        KeyCode::Delete => {
                            app.clear_exchanges();
                        }
                        KeyCode::Char('/') => {
                            app.start_filter();
                        }
                        _ => {}
                    }
                }
                (true, FocusPane::Table) => {
                    // Table focused with detail pane visible
                    match key.code {
                        KeyCode::Char('q') => {
                            app.quit();
                        }
                        KeyCode::Esc => {
                            app.close_detail();
                        }
                        KeyCode::Tab => {
                            app.next_focus();
                        }
                        KeyCode::BackTab => {
                            app.prev_focus();
                        }
                        KeyCode::Enter => {
                            // Update detail view to show currently selected row and focus request
                            // pane
                            update_detail_view_and(&mut app, |app| {
                                app.focus_request();
                            });
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.next_row();
                            update_detail_view_and(&mut app, |_| {});
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.previous_row();
                            update_detail_view_and(&mut app, |_| {});
                        }
                        KeyCode::Delete => {
                            app.clear_exchanges();
                        }
                        KeyCode::Char('/') => {
                            app.start_filter();
                        }
                        // Jump to bottom and re-enable auto-scroll
                        KeyCode::Char('G') => {
                            app.scroll_to_bottom();
                            update_detail_view_and(&mut app, |_| {});
                        }
                        _ => {}
                    }
                }
                (false, _) => {
                    // Table only view
                    match key.code {
                        KeyCode::Char('q') => {
                            app.quit();
                        }
                        KeyCode::Esc => {
                            if app.has_filter() {
                                app.cancel_filter();
                            } else {
                                app.quit();
                            }
                        }
                        KeyCode::Enter => {
                            // Map visible index to real exchange index before opening detail
                            if let Some(selected) = app.table_state.selected()
                                && let Some(real_idx) = app.real_index(selected)
                            {
                                app.detail_index = Some(real_idx);
                                app.show_detail = true;
                                app.focus = FocusPane::Request;
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.next_row();
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.previous_row();
                        }
                        // Jump to bottom and re-enable auto-scroll
                        KeyCode::Char('G') => {
                            app.scroll_to_bottom();
                        }
                        KeyCode::Delete => {
                            app.clear_exchanges();
                        }
                        KeyCode::Char('/') => {
                            app.start_filter();
                        }
                        _ => {}
                    }
                }
            }

            if app.should_quit {
                return Ok(());
            }
        }
    }
}

fn update_detail_view_and<F>(app: &mut MutexGuard<'_, RawMutex, App>, and_then: F)
where
    F: FnOnce(&mut App),
{
    if let Some(selected) = app.table_state.selected()
        && let Some(real_idx) = app.real_index(selected)
    {
        app.detail_index = Some(real_idx);
        app.request_scroll = 0;
        app.response_scroll = 0;
    }
    and_then(app);
}

/// Export the session to a HAR file.
fn export_har(session: &Session, path: &PathBuf) -> Result<()> {
    let count = session.len();
    if count == 0 {
        info!("no exchanges to export");
        return Ok(());
    }

    let har_json = session
        .to_har_json_pretty()
        .context("failed to serialize HAR")?;

    fs::write(path, har_json)
        .with_context(|| format!("failed to write HAR to {}", path.display()))?;

    let dropped = session.dropped_count();
    if dropped > 0 {
        warn!(
            dropped,
            "capture limits dropped older exchanges; the HAR holds the most recent traffic only \
             (raise the [capture] limits in the config file to keep more)"
        );
    }

    info!(path = %path.display(), count, "exported HAR file");

    Ok(())
}

/// Load a session from a HAR file.
fn load_har(path: &PathBuf, limits: SessionLimits) -> Result<Session> {
    let json = fs::read_to_string(path)
        .with_context(|| format!("failed to read HAR from {}", path.display()))?;

    // Loaded under the same limits the session will capture with, so a
    // huge HAR cannot put the proxy over its memory budget before it has
    // served a single request. Anything that did not fit is reported
    // rather than quietly missing.
    let session = Session::from_har_json_with_limits(&json, limits)
        .with_context(|| format!("failed to parse HAR file {}", path.display()))?;

    let dropped = session.dropped_count();
    if dropped > 0 {
        warn!(
            path = %path.display(),
            dropped,
            max_exchanges = limits.max_exchanges,
            "HAR file exceeded the capture limits; oldest entries were not loaded \
             (raise capture.max_exchanges in the config file to keep them)"
        );
    }

    info!(path = %path.display(), count = session.len(), "loaded HAR file");

    Ok(session)
}

/// Build the proxy [`Config`] from CLI args and optional TOML config file.
///
/// Precedence: CLI args > config file > defaults
fn build_config(args: &Args) -> Result<Config> {
    let mut file_config = if args.no_config {
        // User explicitly disabled config file loading
        config::FileConfig::default()
    } else if let Some(config_path) = &args.config {
        // User specified explicit config file path
        info!(path = %config_path.display(), "loading config file");
        config::load_from_path(config_path)?
    } else {
        // Try to load from default locations
        match config::load_from_default_locations()? {
            Some((cfg, path)) => {
                info!(path = %path.display(), "loaded config file");
                cfg
            }
            None => config::FileConfig::default(),
        }
    };

    // CLI args override config file values
    // Only apply CLI values when explicitly provided (Option::Some)

    if let Some(port) = args.port {
        file_config.port = Some(port);
    }
    if let Some(bind) = args.bind {
        file_config.bind = Some(bind);
    }

    // Boolean flags: CLI flag enables, but doesn't disable config file value
    // (use --no-config to start fresh if you want to disable something set in
    // config)
    if args.allow_external {
        file_config.allow_external = true;
    }
    if args.intercept {
        file_config.https.intercept = true;
    }

    // Optional CLI args override config file
    if let Some(ca_dir) = &args.ca_dir {
        file_config.https.ca_dir = Some(ca_dir.clone());
    }

    // Append passthrough hosts from CLI to config file hosts
    for host in &args.passthrough {
        file_config.https.passthrough_hosts.push(host.clone());
    }

    if let Some(secs) = args.connect_timeout {
        file_config.connect_timeout = Some(secs);
    }
    if let Some(secs) = args.read_timeout {
        file_config.read_timeout = Some(secs);
    }
    if let Some(secs) = args.write_timeout {
        file_config.write_timeout = Some(secs);
    }

    if let Some(mb) = args.max_body_size {
        file_config.max_body_size_mb = Some(mb);
    }
    if let Some(kb) = args.max_header_size {
        file_config.max_header_size_kb = Some(kb);
    }

    Ok(file_config.into())
}
