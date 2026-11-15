//! Running the proxy beside the window.
//!
//! The proxy is a tokio program and GPUI runs its own executor, so the proxy
//! gets a thread and a runtime of its own. What it captures crosses back to
//! the window as [`Captured`] messages over a channel; nothing of the window
//! is touched from the proxy's threads.
//!
//! [`ProxyControl`] owns the running proxy and the settings it runs with,
//! and restarts it when the settings change.

use crate::capture::Captured;
use chrono::Utc;
use mira_proxy::CertificateAuthority;
use mira_proxy::Config;
use mira_proxy::Error;
use mira_proxy::FileConfig;
use mira_proxy::Proxy;
use mira_proxy::SessionLimits;
use mira_proxy::TlsConfig;
use mira_proxy::config_file;
use parking_lot::Mutex;
use smol::channel::Sender;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::oneshot;

/// How long a restarted proxy keeps trying for a port its predecessor still
/// holds.
const REBIND_WAIT: Duration = Duration::from_secs(3);

/// The running proxy. Dropping it stops the proxy.
pub struct ProxyHandle {
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl ProxyHandle {
    /// Stop accepting connections, let in-flight exchanges finish so they
    /// reach the window, and wait for the proxy thread to end.
    pub fn stop(&mut self) {
        self.signal();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Stop accepting connections and return at once. In-flight exchanges
    /// still finish and reach the window.
    fn release(mut self) {
        self.signal();
        self.thread.take();
    }

    fn signal(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What a start leaves the window to show.
pub struct Started {
    /// Whether HTTPS is intercepted.
    pub intercept: bool,

    /// What the capture may retain.
    pub limits: SessionLimits,

    /// Why the proxy runs with less than its settings ask, if it does.
    pub warning: Option<String>,
}

/// The proxy, and the settings it runs with.
pub struct ProxyControl {
    settings: FileConfig,

    /// Where `settings` are saved.
    path: Option<PathBuf>,

    handle: Option<ProxyHandle>,
    events: Sender<Captured>,

    /// The last ID handed to an exchange, across every run of the proxy.
    /// Each run counts from 1, and the window needs IDs that stay unique.
    last_id: Arc<AtomicU64>,
}

impl ProxyControl {
    /// A control for a proxy not yet started.
    pub fn new(settings: FileConfig, path: Option<PathBuf>, events: Sender<Captured>) -> Self {
        Self {
            settings,
            path,
            handle: None,
            events,
            last_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The settings the proxy runs with.
    pub fn settings(&self) -> &FileConfig {
        &self.settings
    }

    /// Where the settings are saved.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Start the proxy with the current settings.
    pub fn start(&mut self) -> Started {
        self.launch(false)
    }

    /// Save `settings` and restart the proxy with them.
    ///
    /// # Errors
    ///
    /// Returns why the settings could not be saved. The proxy keeps running
    /// as it was.
    pub fn apply(&mut self, settings: FileConfig) -> Result<Started, String> {
        let path = self
            .path
            .clone()
            .or_else(config_file::default_path)
            .ok_or_else(|| "there is no place to save settings".to_owned())?;
        config_file::save_to_path(&settings, &path).map_err(|e| e.to_string())?;
        self.path = Some(path);
        self.settings = settings;
        if let Some(old) = self.handle.take() {
            old.release();
        }
        Ok(self.launch(true))
    }

    /// Stop the proxy and wait for it.
    pub fn stop(&mut self) {
        if let Some(mut handle) = self.handle.take() {
            handle.stop();
        }
    }

    fn launch(&mut self, restart: bool) -> Started {
        let mut config = Config::from(self.settings.clone());
        // The CA is loaded, and created on first launch, whether or not
        // interception is on, so its certificate can be trusted, copied,
        // and downloaded from mira.proxy before interception starts. A CA
        // that cannot be loaded leaves HTTPS passing through, and the window
        // says why, rather than the proxy not starting at all.
        let (tls, warning) = match load_ca(config.https.ca_dir.as_deref()) {
            Ok(ca) => (Some(TlsConfig::new(ca)), None),
            Err(err) => {
                tracing::error!(error = %err, "could not load the CA");
                let warning = config
                    .https
                    .intercept
                    .then(|| format!("HTTPS interception is off: {err}"));
                config = config.with_https_intercept(false);
                (None, warning)
            }
        };
        let started = Started {
            intercept: config.https.intercept,
            limits: config.session_limits,
            warning,
        };
        let run = Run {
            config,
            tls,
            events: self.events.clone(),
            ids: Ids::new(Arc::clone(&self.last_id)),
            rebind: restart.then_some(REBIND_WAIT),
        };
        self.handle = Some(start(run));
        started
    }
}

/// Where the CA lives: `dir`, or `~/.mira`.
pub fn ca_dir(dir: Option<&Path>) -> Option<PathBuf> {
    match dir {
        Some(dir) => Some(dir.to_owned()),
        None => CertificateAuthority::default_ca_dir().ok(),
    }
}

fn load_ca(dir: Option<&Path>) -> mira_proxy::Result<CertificateAuthority> {
    match dir {
        Some(dir) => CertificateAuthority::load_or_generate_in(dir),
        None => CertificateAuthority::load_or_generate(),
    }
}

/// The window's IDs for one run's exchanges.
struct Ids {
    last: Arc<AtomicU64>,

    /// The run's own ID of each request seen, to the window's.
    seen: Mutex<HashMap<u64, u64>>,
}

impl Ids {
    fn new(last: Arc<AtomicU64>) -> Self {
        Self {
            last,
            seen: Mutex::new(HashMap::new()),
        }
    }

    fn next(&self) -> u64 {
        self.last.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The ID for a request seen for the first time.
    fn request(&self, own: u64) -> u64 {
        let id = self.next();
        self.seen.lock().insert(own, id);
        id
    }

    /// The ID for a finished exchange: its request's, or a new one.
    fn exchange(&self, own: u64) -> u64 {
        let seen = self.seen.lock().remove(&own);
        seen.unwrap_or_else(|| self.next())
    }
}

/// Everything one run of the proxy needs.
struct Run {
    config: Config,
    tls: Option<TlsConfig>,
    events: Sender<Captured>,
    ids: Ids,

    /// How long to retry binding a port still held, after a restart.
    rebind: Option<Duration>,
}

/// Start the proxy on its own thread.
///
/// The window records what it reports, so pausing can leave exchanges out of
/// both the list and the HAR export.
fn start(run: Run) -> ProxyHandle {
    let (shutdown, stopped) = oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("mira-proxy".to_owned())
        .spawn(move || serve(run, stopped));
    match thread {
        Ok(thread) => ProxyHandle {
            shutdown: Some(shutdown),
            thread: Some(thread),
        },
        Err(err) => {
            tracing::error!(error = %err, "could not start the proxy thread");
            ProxyHandle {
                shutdown: None,
                thread: None,
            }
        }
    }
}

fn serve(run: Run, stopped: oneshot::Receiver<()>) {
    let Run {
        config,
        tls,
        events,
        ids,
        rebind,
    } = run;
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("mira-proxy-worker")
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = events.try_send(Captured::Failed(format!(
                "could not start the proxy runtime: {err}"
            )));
            return;
        }
    };

    let ids = Arc::new(ids);
    let requests = (events.clone(), Arc::clone(&ids));
    let exchanges = (events.clone(), ids);
    let mut builder = Proxy::builder(config)
        .on_request(move |mut request| {
            let (events, ids) = &requests;
            request.id = ids.request(request.id);
            let _ = events.try_send(Captured::Request(Box::new(request), Utc::now()));
        })
        .on_exchange(move |mut exchange| {
            let (events, ids) = &exchanges;
            let id = ids.exchange(exchange.request.id);
            exchange.request.id = id;
            if let Some(response) = &mut exchange.response {
                response.request_id = id;
            }
            let _ = events.try_send(Captured::Exchange(Box::new(exchange), Utc::now()));
        });
    if let Some(tls) = tls {
        builder = builder.with_tls(tls);
    }
    let proxy = builder.build();

    runtime.block_on(async {
        let listener = match bind(&proxy, rebind).await {
            Ok(listener) => listener,
            Err(err) => {
                let _ = events.try_send(Captured::Failed(causes(&err)));
                return;
            }
        };
        match listener.local_addr() {
            Ok(addr) => {
                let _ = events.try_send(Captured::Listening(addr));
            }
            Err(err) => {
                let _ = events.try_send(Captured::Failed(err.to_string()));
                return;
            }
        }
        let shutdown = async move {
            let _ = stopped.await;
        };
        if let Err(err) = proxy.serve_until(listener, shutdown).await {
            let _ = events.try_send(Captured::Failed(err.to_string()));
        }
    });
}

/// `error` and what caused it, such as the address already being in use.
fn causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Bind the proxy's port. After a restart the previous run may still hold
/// it for a moment, so a port in use is retried for up to `rebind`.
async fn bind(
    proxy: &Proxy,
    rebind: Option<Duration>,
) -> mira_proxy::Result<tokio::net::TcpListener> {
    let deadline = rebind.map(|wait| tokio::time::Instant::now() + wait);
    loop {
        match proxy.bind().await {
            Err(Error::Bind { source, .. })
                if source.kind() == std::io::ErrorKind::AddrInUse
                    && deadline.is_some_and(|d| tokio::time::Instant::now() < d) =>
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            bound => return bound,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ids_stay_unique_across_runs() {
        let last = Arc::new(AtomicU64::new(0));
        let first = Ids::new(Arc::clone(&last));
        assert_eq!(first.request(1), 1);
        assert_eq!(first.request(2), 2);

        // A restarted proxy counts from 1 again.
        let second = Ids::new(Arc::clone(&last));
        assert_eq!(second.request(1), 3);

        // Each run's exchanges find their own requests, even interleaved.
        assert_eq!(first.exchange(2), 2);
        assert_eq!(second.exchange(1), 3);
        assert_eq!(first.exchange(1), 1);

        // An exchange whose request was never reported gets an ID of its own.
        assert_eq!(second.exchange(9), 4);
    }
}
