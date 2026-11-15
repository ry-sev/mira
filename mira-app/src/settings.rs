//! The settings: where the proxy listens, what it intercepts, which hosts it
//! serves, and its timeouts and limits.
//!
//! They live in the config file the terminal UI reads too. Applying saves
//! them and restarts the proxy; the capture stays.

use crate::capture::Listening;
use crate::input::new_input;
use crate::input::setting_field;
use crate::logging;
use crate::proxy;
use crate::state::Shared;
use crate::state::Tone;
use crate::theme::MONO;
use crate::theme::colors;
use crate::theme::text;
use crate::trust;
use crate::ui;
use gpui_kit::ClipboardItem;
use gpui_kit::Context;
use gpui_kit::Div;
use gpui_kit::Entity;
use gpui_kit::FontWeight;
use gpui_kit::IntoElement;
use gpui_kit::ParentElement as _;
use gpui_kit::Rems;
use gpui_kit::Render;
use gpui_kit::ScrollHandle;
use gpui_kit::SharedString;
use gpui_kit::Styled as _;
use gpui_kit::Subscription;
use gpui_kit::Window;
use gpui_kit::base::h_flex;
use gpui_kit::base::input::InputEvent;
use gpui_kit::base::input::InputState;
use gpui_kit::div;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::rems;
use mira_proxy::CertificateAuthority;
use mira_proxy::DEFAULT_BIND;
use mira_proxy::DEFAULT_CONNECT_TIMEOUT;
use mira_proxy::DEFAULT_MAX_BODY_SIZE;
use mira_proxy::DEFAULT_MAX_CONNECTIONS;
use mira_proxy::DEFAULT_MAX_EXCHANGES;
use mira_proxy::DEFAULT_MAX_HEADER_SIZE;
use mira_proxy::DEFAULT_MAX_TOTAL_BODY_BYTES;
use mira_proxy::DEFAULT_PORT;
use mira_proxy::DEFAULT_READ_TIMEOUT;
use mira_proxy::DEFAULT_WRITE_TIMEOUT;
use mira_proxy::FileConfig;
use mira_proxy::config_file;
use mira_proxy::config_file::FileHostFilter;
use std::net::IpAddr;
use std::path::Path;
use std::path::PathBuf;

const KB: usize = 1024;
const MB: usize = 1024 * 1024;

/// A setting typed as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Bind,
    Port,
    CaDir,
    Passthrough,
    Allowed,
    Blocked,
    ConnectTimeout,
    ReadTimeout,
    WriteTimeout,
    MaxBody,
    MaxHeader,
    MaxConnections,
    MaxExchanges,
    MaxBodyKept,
    MaxTotalKept,
}

impl Field {
    const ALL: [Self; 15] = [
        Self::Bind,
        Self::Port,
        Self::CaDir,
        Self::Passthrough,
        Self::Allowed,
        Self::Blocked,
        Self::ConnectTimeout,
        Self::ReadTimeout,
        Self::WriteTimeout,
        Self::MaxBody,
        Self::MaxHeader,
        Self::MaxConnections,
        Self::MaxExchanges,
        Self::MaxBodyKept,
        Self::MaxTotalKept,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Bind => "Bind address",
            Self::Port => "Port",
            Self::CaDir => "CA directory",
            Self::Passthrough => "Never intercept",
            Self::Allowed => "Allowed hosts",
            Self::Blocked => "Blocked hosts",
            Self::ConnectTimeout => "Connect",
            Self::ReadTimeout => "Read",
            Self::WriteTimeout => "Write",
            Self::MaxBody => "Request body",
            Self::MaxHeader => "Header block",
            Self::MaxConnections => "Connections",
            Self::MaxExchanges => "Exchanges",
            Self::MaxBodyKept => "Body per exchange",
            Self::MaxTotalKept => "All bodies",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Self::Bind => {
                "127.0.0.1 serves this computer only. 0.0.0.0 also serves other devices on \
                 your network."
            }
            Self::Port => "Where clients connect.",
            Self::CaDir => "Where the certificate authority's certificate and key are kept.",
            Self::Passthrough => {
                "Tunneled untouched even while intercepting, for apps that pin their \
                 certificates."
            }
            Self::Allowed => "When set, requests to any other host are refused.",
            Self::Blocked => "Refused with 403 Forbidden. Takes precedence over allowed hosts.",
            Self::ConnectTimeout => "How long to wait for a server to accept a connection.",
            Self::ReadTimeout => "How long to wait for a server to send more.",
            Self::WriteTimeout => "How long a client or server may take to accept more data.",
            Self::MaxBody => {
                "Larger requests are refused. Larger responses still reach the client but \
                 are captured only in part."
            }
            Self::MaxHeader => "Larger request or response heads are refused.",
            Self::MaxConnections => "Clients past this wait for a free connection.",
            Self::MaxExchanges => "Past this many, the oldest exchanges are dropped.",
            Self::MaxBodyKept => "Longer bodies are cut to this in the capture.",
            Self::MaxTotalKept => "Past this much body data, the oldest exchanges are dropped.",
        }
    }

    /// What an empty field means.
    fn placeholder(self) -> String {
        match self {
            Self::Bind => DEFAULT_BIND.to_string(),
            Self::Port => DEFAULT_PORT.to_string(),
            Self::CaDir => proxy::ca_dir(None).map_or_else(|| "~/.mira".to_owned(), |d| tilde(&d)),
            Self::Passthrough => "*.apple.com, *.icloud.com".to_owned(),
            Self::Allowed => "Every host".to_owned(),
            Self::Blocked => "None".to_owned(),
            Self::ConnectTimeout => DEFAULT_CONNECT_TIMEOUT.as_secs().to_string(),
            Self::ReadTimeout => DEFAULT_READ_TIMEOUT.as_secs().to_string(),
            Self::WriteTimeout => DEFAULT_WRITE_TIMEOUT.as_secs().to_string(),
            Self::MaxBody => (DEFAULT_MAX_BODY_SIZE / MB).to_string(),
            Self::MaxHeader => (DEFAULT_MAX_HEADER_SIZE / KB).to_string(),
            Self::MaxConnections => DEFAULT_MAX_CONNECTIONS.to_string(),
            Self::MaxExchanges => DEFAULT_MAX_EXCHANGES.to_string(),
            Self::MaxBodyKept => "No limit".to_owned(),
            Self::MaxTotalKept => (DEFAULT_MAX_TOTAL_BODY_BYTES / MB).to_string(),
        }
    }

    fn unit(self) -> Option<&'static str> {
        match self {
            Self::ConnectTimeout | Self::ReadTimeout | Self::WriteTimeout => Some("SEC"),
            Self::MaxBody | Self::MaxTotalKept => Some("MB"),
            Self::MaxHeader | Self::MaxBodyKept => Some("KB"),
            _ => None,
        }
    }

    fn width(self) -> Rems {
        match self {
            Self::CaDir | Self::Passthrough | Self::Allowed | Self::Blocked => rems(16.0),
            Self::Bind => rems(10.0),
            _ => rems(7.0),
        }
    }

    /// The field's text for `settings`. Blank where they keep the default.
    fn text(self, settings: &FileConfig) -> String {
        let number = |value: Option<usize>| value.map(|v| v.to_string()).unwrap_or_default();
        let filter = settings.host_filter.as_ref();
        match self {
            Self::Bind => settings.bind.map(|b| b.to_string()).unwrap_or_default(),
            Self::Port => settings.port.map(|p| p.to_string()).unwrap_or_default(),
            Self::CaDir => settings
                .https
                .ca_dir
                .as_deref()
                .map(tilde)
                .unwrap_or_default(),
            Self::Passthrough => settings.https.passthrough_hosts.join(", "),
            Self::Allowed => filter.map(|f| f.include.join(", ")).unwrap_or_default(),
            Self::Blocked => filter.map(|f| f.exclude.join(", ")).unwrap_or_default(),
            Self::ConnectTimeout => seconds(settings.connect_timeout),
            Self::ReadTimeout => seconds(settings.read_timeout),
            Self::WriteTimeout => seconds(settings.write_timeout),
            Self::MaxBody => number(settings.max_body_size_mb),
            Self::MaxHeader => number(settings.max_header_size_kb),
            Self::MaxConnections => number(settings.max_connections),
            Self::MaxExchanges => number(settings.capture.max_exchanges),
            Self::MaxBodyKept => number(settings.capture.max_body_kb),
            Self::MaxTotalKept => number(settings.capture.max_total_body_mb),
        }
    }

    /// Store `text` in `settings`; blank keeps the default.
    fn store(self, text: &str, settings: &mut FileConfig) -> Result<(), &'static str> {
        if text.is_empty() {
            return Ok(());
        }
        match self {
            Self::Bind => {
                settings.bind = Some(
                    text.parse::<IpAddr>()
                        .map_err(|_bad| "Enter an IP address, such as 127.0.0.1.")?,
                );
            }
            Self::Port => {
                settings.port = Some(
                    text.parse::<u16>()
                        .ok()
                        .filter(|&p| p > 0)
                        .ok_or("Enter a port from 1 to 65535.")?,
                );
            }
            Self::CaDir => settings.https.ca_dir = Some(untilde(text)),
            Self::Passthrough => settings.https.passthrough_hosts = hosts(text),
            Self::Allowed => {
                settings
                    .host_filter
                    .get_or_insert_with(FileHostFilter::default)
                    .include = hosts(text);
            }
            Self::Blocked => {
                settings
                    .host_filter
                    .get_or_insert_with(FileHostFilter::default)
                    .exclude = hosts(text);
            }
            Self::ConnectTimeout => settings.connect_timeout = Some(whole(text)?),
            Self::ReadTimeout => settings.read_timeout = Some(whole(text)?),
            Self::WriteTimeout => settings.write_timeout = Some(whole(text)?),
            Self::MaxBody => settings.max_body_size_mb = Some(whole(text)?),
            Self::MaxHeader => settings.max_header_size_kb = Some(whole(text)?),
            Self::MaxConnections => settings.max_connections = Some(whole(text)?),
            Self::MaxExchanges => settings.capture.max_exchanges = Some(whole(text)?),
            Self::MaxBodyKept => settings.capture.max_body_kb = Some(whole(text)?),
            Self::MaxTotalKept => settings.capture.max_total_body_mb = Some(whole(text)?),
        }
        Ok(())
    }
}

fn seconds(value: Option<u64>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

/// A whole number above zero.
fn whole<T: std::str::FromStr + Default + PartialEq>(text: &str) -> Result<T, &'static str> {
    text.parse::<T>()
        .ok()
        .filter(|n| *n != T::default())
        .ok_or("Enter a whole number above 0.")
}

/// Host patterns, separated by commas or spaces.
fn hosts(text: &str) -> Vec<String> {
    text.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
        .collect()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `path`, with the home directory as `~`.
fn tilde(path: &Path) -> String {
    match home().and_then(|home| path.strip_prefix(home).ok().map(Path::to_owned)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// `text` as a path, with a leading `~` as the home directory.
fn untilde(text: &str) -> PathBuf {
    match (text.strip_prefix("~/"), home()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if text == "~" => home().unwrap_or_else(|| PathBuf::from(text)),
        _ => PathBuf::from(text),
    }
}

/// `settings` as the form would produce them, so settings that mean the
/// same compare equal.
fn normalized(settings: &FileConfig) -> FileConfig {
    let mut settings = settings.clone();
    if settings
        .host_filter
        .as_ref()
        .is_some_and(|f| f.include.is_empty() && f.exclude.is_empty())
    {
        settings.host_filter = None;
    }
    settings
}

/// What the form holds, read back as settings.
struct Form {
    settings: FileConfig,

    /// Each field that does not read, and why.
    errors: Vec<(Field, &'static str)>,
}

impl Form {
    fn error(&self, field: Field) -> Option<&'static str> {
        self.errors
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, e)| *e)
    }
}

/// The Settings page.
pub struct SettingsView {
    shared: Shared,

    /// One input per [`Field`], in [`Field::ALL`] order.
    inputs: Vec<Entity<InputState>>,

    intercept: bool,
    allow_external: bool,

    /// Whether the system trusts the CA certificate, once checked.
    trusted: Option<bool>,

    /// The certificate `trusted` describes.
    trust_checked: Option<PathBuf>,

    /// Whether the system is being asked to trust the certificate.
    trusting: bool,

    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(shared: Shared, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let inputs: Vec<_> = Field::ALL
            .into_iter()
            .map(|field| new_input(field.placeholder(), window, cx))
            .collect();
        let mut subscriptions: Vec<Subscription> =
            inputs
                .iter()
                .map(|input| {
                    cx.subscribe_in(input, window, |this, _, event: &InputEvent, window, cx| {
                        match event {
                            InputEvent::Change => cx.notify(),
                            InputEvent::PressEnter { .. } => this.apply(window, cx),
                            _ => {}
                        }
                    })
                })
                .collect();
        subscriptions.push(cx.observe(&shared.capture, |_, _, cx| cx.notify()));
        let mut this = Self {
            shared,
            inputs,
            intercept: false,
            allow_external: false,
            trusted: None,
            trust_checked: None,
            trusting: false,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.revert(window, cx);
        this
    }

    fn input(&self, field: Field) -> &Entity<InputState> {
        let at = Field::ALL.iter().position(|f| *f == field).unwrap_or(0);
        &self.inputs[at]
    }

    /// Show `settings` in the form.
    fn load(&mut self, settings: &FileConfig, window: &mut Window, cx: &mut Context<'_, Self>) {
        for field in Field::ALL {
            let text = field.text(settings);
            self.input(field)
                .update(cx, |input, cx| input.set_value(text, window, cx));
        }
        self.intercept = settings.https.intercept;
        self.allow_external = settings.allow_external;
        cx.notify();
    }

    /// Show the settings in force.
    fn revert(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let settings = self.shared.proxy.read(cx).settings().clone();
        self.load(&settings, window, cx);
    }

    fn read(&self, cx: &Context<'_, Self>) -> Form {
        let mut settings = FileConfig {
            allow_external: self.allow_external,
            ..FileConfig::default()
        };
        settings.https.intercept = self.intercept;
        let mut errors = Vec::new();
        for field in Field::ALL {
            let text = self.input(field).read(cx).value();
            if let Err(reason) = field.store(text.trim(), &mut settings) {
                errors.push((field, reason));
            }
        }
        Form {
            settings: normalized(&settings),
            errors,
        }
    }

    /// Save the form and restart the proxy with it.
    fn apply(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let form = self.read(cx);
        if let Some((field, reason)) = form.errors.first() {
            let message = format!("{}: {reason}", field.name());
            self.shared
                .status
                .update(cx, |status, cx| status.say(Tone::Error, message, cx));
            return;
        }
        let applied = self
            .shared
            .proxy
            .update(cx, |proxy, _| proxy.apply(form.settings.clone()));
        let (tone, message) = match applied {
            Ok(started) => {
                self.shared.capture.update(cx, |capture, cx| {
                    capture.restart(started.intercept, started.limits);
                    cx.notify();
                });
                self.load(&form.settings, window, cx);
                self.trust_checked = None;
                match started.warning {
                    Some(warning) => (Tone::Error, warning),
                    None => (Tone::Ok, "Settings saved. The proxy restarted.".to_owned()),
                }
            }
            Err(reason) => (Tone::Error, format!("Couldn't save the settings: {reason}")),
        };
        self.shared
            .status
            .update(cx, |status, cx| status.say(tone, message, cx));
    }

    fn field(&self, field: Field, form: &Form, window: &Window, cx: &Context<'_, Self>) -> Div {
        let id = SharedString::from(format!("setting-{field:?}"));
        row(
            field.name(),
            field.help(),
            form.error(field),
            setting_field(
                id,
                self.input(field),
                field.width(),
                field.unit(),
                form.error(field).is_some(),
                window,
                cx,
            ),
        )
    }

    fn connection(&self, cx: &Context<'_, Self>) -> Div {
        let c = colors();
        let (color, state, address) = match self.shared.capture.read(cx).listening() {
            Listening::On(addr) => (c.ok, format!("Listening on {addr}"), Some(*addr)),
            Listening::Starting => (c.warn, "Starting…".to_owned(), None),
            Listening::Failed(reason) => (c.err, format!("Not listening: {reason}"), None),
        };
        let status = self.shared.status.clone();
        let help = match address {
            Some(addr) if addr.ip().is_unspecified() => {
                "Set this computer's address and this port as the HTTP and HTTPS proxy on \
                 each client."
            }
            _ => "Set this as the HTTP and HTTPS proxy in your browser or system settings.",
        };
        h_flex()
            .gap(rems(1.5))
            .py(rems(0.75))
            .border_b_1()
            .border_color(c.line_soft)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(rems(0.25))
                    .child(
                        h_flex()
                            .gap(rems(0.5))
                            .child(div().size(rems(0.5)).flex_none().bg(color))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(MONO)
                                    .text_size(text::UI)
                                    .text_color(c.ink)
                                    .child(state),
                            ),
                    )
                    .child(div().text_size(text::LABEL).text_color(c.ink_3).child(help)),
            )
            .when_some(address, |this, addr| {
                this.child(
                    ui::text_button("settings-copy-address", "Copy address", false).on_click(
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(format!(
                                "http://{addr}"
                            )));
                            status.update(cx, |status, cx| {
                                status.say(Tone::Ok, format!("Copied http://{addr}"), cx);
                            });
                        },
                    ),
                )
            })
    }

    /// Where the CA certificate the form points at lives.
    fn ca_certificate(&self, cx: &Context<'_, Self>) -> Option<PathBuf> {
        let settings = self.read(cx).settings;
        proxy::ca_dir(settings.https.ca_dir.as_deref())
            .map(|dir| CertificateAuthority::cert_file_path_in(&dir))
    }

    fn certificate(&self, cx: &Context<'_, Self>) -> Div {
        let path = self.ca_certificate(cx);
        let help = if path.as_ref().is_some_and(|p| p.exists()) {
            "Clients must trust it before HTTPS can be intercepted."
        } else {
            "Created when these settings are applied."
        };
        path_row(
            "ca",
            "CA certificate",
            help,
            path,
            Show::Reveal,
            &self.shared,
        )
    }

    /// Check, in the background, whether the system trusts `path`.
    fn check_trust(&mut self, path: Option<PathBuf>, cx: &Context<'_, Self>) {
        self.trust_checked.clone_from(&path);
        self.trusted = None;
        let Some(path) = path else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let checked = path.clone();
            let trusted = cx
                .background_executor()
                .spawn(async move { trust::is_trusted(&checked) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.trust_checked.as_ref() == Some(&path) {
                    this.trusted = trusted;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Ask the system to trust the CA certificate. It asks for the user's
    /// password itself.
    fn trust(&mut self, cx: &mut Context<'_, Self>) {
        let Some(path) = self.ca_certificate(cx) else {
            return;
        };
        self.trusting = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let target = path.clone();
            let trusted = cx
                .background_executor()
                .spawn(async move { trust::trust(&target) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.trusting = false;
                let (tone, message) = match trusted {
                    Ok(()) => (
                        Tone::Ok,
                        "This Mac now trusts Mira's certificate. Restart open browsers.".to_owned(),
                    ),
                    Err(reason) => (
                        Tone::Error,
                        format!("Couldn't trust the certificate: {reason}"),
                    ),
                };
                this.shared
                    .status
                    .update(cx, |status, cx| status.say(tone, message, cx));
                this.check_trust(Some(path), cx);
            });
        })
        .detach();
    }

    /// Whether this Mac trusts the certificate, with a button to make it.
    fn trust_row(&self, cx: &Context<'_, Self>) -> Option<Div> {
        let exists = self.ca_certificate(cx).is_some_and(|path| path.exists());
        if !trust::SUPPORTED || !exists {
            return None;
        }
        let c = colors();
        let this = cx.entity().downgrade();
        let (help, control) = match self.trusted {
            Some(true) => (
                "Browsers accept intercepted HTTPS. Firefox keeps its own list; \
                 http://mira.proxy shows how to add the certificate there.",
                ui::tag("TRUSTED", c.ok, c.ok.opacity(0.45)).into_any_element(),
            ),
            Some(false) => (
                "Trust it before turning on interception, or browsers will refuse \
                 intercepted HTTPS. macOS asks for your password.",
                ui::text_button(
                    "settings-trust",
                    if self.trusting {
                        "Trusting…"
                    } else {
                        "Trust"
                    },
                    true,
                )
                .disabled(self.trusting)
                .on_click(move |_, _, cx| {
                    let _ = this.update(cx, |this, cx| this.trust(cx));
                })
                .into_any_element(),
            ),
            None => ("Checking…", div().into_any_element()),
        };
        Some(row("Trusted by this Mac", help, None, control))
    }

    fn config_file(&self, cx: &Context<'_, Self>) -> Div {
        let path = self
            .shared
            .proxy
            .read(cx)
            .path()
            .map(Path::to_owned)
            .or_else(config_file::default_path);
        let help = if path.as_ref().is_some_and(|p| p.exists()) {
            "Where these settings are saved. The terminal UI reads it too."
        } else {
            "Created when settings are first applied. The terminal UI reads it too."
        };
        path_row(
            "config",
            "Config file",
            help,
            path,
            Show::Reveal,
            &self.shared,
        )
    }

    fn logs(&self) -> Div {
        path_row(
            "logs",
            "Logs",
            "A file a day, kept for two weeks. Include it when reporting a problem.",
            logging::log_dir(),
            Show::Open,
            &self.shared,
        )
    }

    fn footer(form: &Form, dirty: bool, cx: &Context<'_, Self>) -> Div {
        let c = colors();
        let valid = form.errors.is_empty();
        let (message, color) = if let Some((field, reason)) = form.errors.first() {
            (format!("{}: {reason}", field.name()), c.err)
        } else if dirty {
            (
                "Unsaved changes. Applying restarts the proxy; the capture stays.".to_owned(),
                c.warn,
            )
        } else {
            (String::new(), c.ink_3)
        };
        let this = cx.entity().downgrade();
        let reset = this.clone();
        let revert = this.clone();
        ui::footer()
            .gap(rems(0.5))
            .px(rems(1.0))
            .bg(c.panel_2)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(text::LABEL)
                    .text_color(color)
                    .child(message),
            )
            .child(
                ui::text_button("settings-defaults", "Reset to defaults", false).on_click(
                    move |_, window, cx| {
                        let _ = reset.update(cx, |this, cx| {
                            this.load(&FileConfig::default(), window, cx);
                        });
                    },
                ),
            )
            .child(
                ui::text_button("settings-revert", "Revert", false)
                    .disabled(!dirty)
                    .on_click(move |_, window, cx| {
                        let _ = revert.update(cx, |this, cx| this.revert(window, cx));
                    }),
            )
            .child(
                ui::text_button("settings-apply", "Apply", true)
                    .disabled(!dirty || !valid)
                    .on_click(move |_, window, cx| {
                        let _ = this.update(cx, |this, cx| this.apply(window, cx));
                    }),
            )
    }
}

/// One setting: its name and help on the left, its control on the right,
/// and what is wrong with it, if anything, under the help.
fn row(
    name: &'static str,
    help: &'static str,
    error: Option<&'static str>,
    control: impl IntoElement,
) -> Div {
    let c = colors();
    h_flex()
        .gap(rems(1.5))
        .py(rems(0.75))
        .border_b_1()
        .border_color(c.line_soft)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(rems(0.25))
                .child(
                    div()
                        .text_size(text::UI)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(c.ink)
                        .child(name),
                )
                .child(div().text_size(text::LABEL).text_color(c.ink_3).child(help))
                .when_some(error, |this, error| {
                    this.child(div().text_size(text::LABEL).text_color(c.err).child(error))
                }),
        )
        .child(h_flex().flex_none().gap(rems(0.375)).child(control))
}

/// How a path row shows its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Show {
    /// Select the file in the file manager.
    Reveal,

    /// Open the folder itself.
    Open,
}

/// A file or folder: where it is, with buttons to copy its path and show it
/// once it exists.
fn path_row(
    id: &'static str,
    name: &'static str,
    help: &'static str,
    path: Option<PathBuf>,
    show: Show,
    shared: &Shared,
) -> Div {
    let c = colors();
    let exists = path.as_ref().is_some_and(|p| p.exists());
    let shown = path
        .as_deref()
        .map_or_else(|| "No home directory".to_owned(), tilde);
    let status = shared.status.clone();
    let copied = path.clone();
    let (caption, hint) = match show {
        Show::Reveal => ("Reveal", "Show it in the file manager"),
        Show::Open => ("Open", "Open the folder"),
    };
    row(
        name,
        help,
        None,
        h_flex()
            .gap(rems(0.375))
            .child(
                div()
                    .max_w(rems(14.0))
                    .truncate()
                    .font_family(MONO)
                    .text_size(text::LABEL)
                    .text_color(if exists { c.ink_2 } else { c.ink_4 })
                    .child(shown),
            )
            .when_some(path.filter(|_| exists), |this, path| {
                this.child(ui::text_button((id, 0_usize), "Copy path", false).on_click(
                    move |_, _, cx| {
                        if let Some(path) = &copied {
                            let text = path.display().to_string();
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                            status.update(cx, |status, cx| {
                                status.say(Tone::Ok, format!("Copied {}", tilde(path)), cx);
                            });
                        }
                    },
                ))
                .child(ui::hint(
                    ui::text_button((id, 1_usize), caption, false).on_click(move |_, _, cx| {
                        match show {
                            Show::Reveal => cx.reveal_path(&path),
                            Show::Open => cx.open_with_system(&path),
                        }
                    }),
                    &shared.status,
                    hint,
                ))
            }),
    )
}

/// A setting that is on or off.
fn switch_row(name: &'static str, help: &'static str, control: impl IntoElement) -> Div {
    row(name, help, None, control)
}

/// A titled group of settings.
fn group(title: &'static str) -> Div {
    div()
        .pt(rems(1.75))
        .child(ui::section(title, None).mb(rems(0.0)))
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let c = colors();
        let certificate = self.ca_certificate(cx);
        if trust::SUPPORTED && certificate != self.trust_checked {
            self.check_trust(certificate, cx);
        }
        let form = self.read(cx);
        let applied = normalized(self.shared.proxy.read(cx).settings());
        let dirty = form.settings != applied;
        let this = cx.entity().downgrade();
        let intercept = this.clone();
        let external = this;
        let field = |f: Field| self.field(f, &form, window, cx);

        let content = div()
            .w_full()
            .max_w(rems(46.0))
            .px(rems(1.5))
            .pb(rems(2.0))
            .child(group("Connection"))
            .child(self.connection(cx))
            .child(field(Field::Bind))
            .child(field(Field::Port))
            .child(switch_row(
                "Allow public addresses",
                "Serve clients outside this computer's local and private networks. Leave \
                 off unless you know you need it.",
                ui::switch(
                    "settings-external",
                    self.allow_external,
                    "Allow public addresses",
                    move |on, _, cx| {
                        let _ = external.update(cx, |this, cx| {
                            this.allow_external = on;
                            cx.notify();
                        });
                    },
                ),
            ))
            .child(group("HTTPS"))
            .child(switch_row(
                "Intercept HTTPS",
                "Decrypt HTTPS so its requests and bodies can be inspected. When off, \
                 HTTPS passes through as tunnels.",
                ui::switch(
                    "settings-intercept",
                    self.intercept,
                    "Intercept HTTPS",
                    move |on, _, cx| {
                        let _ = intercept.update(cx, |this, cx| {
                            this.intercept = on;
                            cx.notify();
                        });
                    },
                ),
            ))
            .child(self.certificate(cx))
            .children(self.trust_row(cx))
            .child(field(Field::CaDir))
            .child(field(Field::Passthrough))
            .child(group("Hosts"))
            .child(field(Field::Allowed))
            .child(field(Field::Blocked))
            .child(group("Timeouts"))
            .child(field(Field::ConnectTimeout))
            .child(field(Field::ReadTimeout))
            .child(field(Field::WriteTimeout))
            .child(group("Limits"))
            .child(field(Field::MaxBody))
            .child(field(Field::MaxHeader))
            .child(field(Field::MaxConnections))
            .child(group("Capture memory"))
            .child(field(Field::MaxExchanges))
            .child(field(Field::MaxBodyKept))
            .child(field(Field::MaxTotalKept))
            .child(group("Files"))
            .child(self.config_file(cx))
            .child(self.logs());

        let footer = Self::footer(&form, dirty, cx);
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(ui::scroll_area(
                "settings-scroll",
                &self.scroll,
                h_flex().w_full().justify_center().child(content),
            ))
            .child(footer)
            .bg(c.panel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(texts: &[(Field, &str)]) -> (FileConfig, Vec<(Field, &'static str)>) {
        let mut settings = FileConfig::default();
        let mut errors = Vec::new();
        for (field, text) in texts {
            if let Err(e) = field.store(text, &mut settings) {
                errors.push((*field, e));
            }
        }
        (normalized(&settings), errors)
    }

    #[test]
    fn test_blank_fields_keep_the_defaults() {
        let texts: Vec<_> = Field::ALL.iter().map(|f| (*f, "")).collect();
        let (settings, errors) = form(&texts);
        assert!(errors.is_empty());
        assert_eq!(settings, FileConfig::default());
    }

    #[test]
    fn test_every_field_reads_back_what_it_shows() {
        let (settings, errors) = form(&[
            (Field::Bind, "0.0.0.0"),
            (Field::Port, "9000"),
            (Field::CaDir, "/tmp/ca"),
            (Field::Passthrough, "*.apple.com, *.icloud.com"),
            (Field::Allowed, "a.test b.test"),
            (Field::ConnectTimeout, "5"),
            (Field::MaxBody, "10"),
            (Field::MaxExchanges, "250"),
        ]);
        assert!(errors.is_empty());
        assert_eq!(settings.port, Some(9000));
        assert_eq!(
            settings.https.passthrough_hosts,
            ["*.apple.com", "*.icloud.com"]
        );
        let filter = settings.host_filter.clone().expect("a host filter");
        assert_eq!(filter.include, ["a.test", "b.test"]);
        assert!(filter.exclude.is_empty());
        for field in Field::ALL {
            let mut again = FileConfig::default();
            field
                .store(&field.text(&settings), &mut again)
                .expect("reads back");
            assert_eq!(field.text(&again), field.text(&settings), "{field:?}");
        }
    }

    #[test]
    fn test_bad_values_are_reported_per_field() {
        let (_, errors) = form(&[
            (Field::Bind, "localhost"),
            (Field::Port, "0"),
            (Field::ReadTimeout, "-1"),
            (Field::MaxConnections, "0"),
            (Field::WriteTimeout, "30"),
        ]);
        let fields: Vec<Field> = errors.iter().map(|(f, _)| *f).collect();
        assert_eq!(
            fields,
            [
                Field::Bind,
                Field::Port,
                Field::ReadTimeout,
                Field::MaxConnections
            ]
        );
    }

    #[test]
    fn test_paths_under_home_read_with_a_tilde() {
        let Some(home) = home() else {
            return;
        };
        let path = home.join(".mira");
        assert_eq!(tilde(&path), "~/.mira");
        assert_eq!(untilde("~/.mira"), path);
        assert_eq!(untilde("/etc/mira"), PathBuf::from("/etc/mira"));
    }
}
