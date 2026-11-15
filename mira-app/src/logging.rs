//! Logging to a file, since a bundled app has no terminal to write to.
//!
//! Logs go beside the terminal UI's, in their own files:
//! - **macOS**: `~/Library/Logs/mira/`
//! - **Linux**: `~/.local/state/mira/` (`XDG_STATE_HOME`)
//! - **Windows**: `%LOCALAPPDATA%\mira\logs\`

use std::path::PathBuf;
use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::RollingFileAppender;
use tracing_appender::rolling::Rotation;
use tracing_subscriber::EnvFilter;

/// What every log file's name starts with. Distinct from the terminal UI's,
/// so the two never write to one file.
const LOG_FILE_PREFIX: &str = "mira.log";

/// How many days of logs are kept.
const KEPT_FILES: usize = 14;

/// Where the logs are written.
pub fn log_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|home| home.join("Library/Logs/mira"))
    }

    #[cfg(target_os = "linux")]
    {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
            .map(|state| state.join("mira"))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        dirs::data_local_dir().map(|local| local.join("mira").join("logs"))
    }
}

/// Send logs to a daily file, and log panics there too.
///
/// The returned guard MUST be held until the app exits, or the last lines
/// are lost. Without a usable log directory, logs go to stderr.
///
/// `RUST_LOG` sets the level; without it, Mira's own crates log at `info`.
pub fn init() -> Option<WorkerGuard> {
    let filter = if std::env::var_os(EnvFilter::DEFAULT_ENV).is_some() {
        EnvFilter::builder()
            .with_default_directive(LevelFilter::WARN.into())
            .from_env_lossy()
    } else {
        EnvFilter::new(concat!(
            "warn,",
            env!("CARGO_CRATE_NAME"),
            "=info,mira_proxy=info"
        ))
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(true)
        .with_file(true)
        .with_line_number(true);

    let guard = match appender() {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            builder.with_writer(writer).init();
            Some(guard)
        }
        Err(reason) => {
            builder.with_writer(std::io::stderr).init();
            tracing::warn!(reason, "logging to stderr");
            None
        }
    };
    log_panics();
    guard
}

fn appender() -> Result<RollingFileAppender, String> {
    let dir = log_dir().ok_or("there is no home directory")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .max_log_files(KEPT_FILES)
        .build(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// Log a panic before the default hook reports it.
fn log_panics() {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(panic = %info, %backtrace, "panicked");
        report(info);
    }));
}
