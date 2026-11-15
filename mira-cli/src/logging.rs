//! Cross-platform logging configuration for Mira.
//!
//! Logs are written to platform-specific directories:
//! - **macOS**: `~/Library/Logs/mira/`
//! - **Linux**: `~/.local/state/mira/` (`XDG_STATE_HOME`)
//! - **Windows**: `%LOCALAPPDATA%\mira\logs\`

use anyhow::Context;
use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Returns the platform-specific log directory for Mira.
///
/// - **macOS**: `~/Library/Logs/mira/`
/// - **Linux**: `~/.local/state/mira/` (respects [`XDG_STATE_HOME`])
/// - **Windows**: `%LOCALAPPDATA%\mira\logs\`
pub fn log_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|home| home.join("Library/Logs/mira"))
    }

    #[cfg(target_os = "linux")]
    {
        // XDG_STATE_HOME or fallback to ~/.local/state
        std::env::var("XDG_STATE_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
            .map(|state| state.join("mira"))
    }

    #[cfg(target_os = "windows")]
    {
        dirs::data_local_dir().map(|local| local.join("mira").join("logs"))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        // Fallback for other platforms: use data_local_dir
        dirs::data_local_dir().map(|local| local.join("mira").join("logs"))
    }
}

/// Returns the path to the current log file.
///
/// The appender rotates daily and names each file `mira-cli.log.YYYY-MM-DD`,
/// dated in UTC.
pub fn log_file_path() -> Option<PathBuf> {
    let today = chrono::Utc::now().format("%Y-%m-%d");
    log_dir().map(|dir| dir.join(format!("{LOG_FILE_PREFIX}.{today}")))
}

/// What every log file's name starts with.
const LOG_FILE_PREFIX: &str = "mira-cli.log";

/// Initialize the tracing subscriber with file logging.
///
/// Returns a guard that MUST be held for the lifetime of the program
/// to ensure all logs are flushed before exit.
///
/// The log level is controlled by the `RUST_LOG` environment variable.
/// If not set, defaults to `info` level for the Mira crates.
pub fn init() -> Result<WorkerGuard> {
    let log_dir = log_dir().context("failed to determine log directory")?;

    fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create log directory: {}", log_dir.display()))?;

    let file_appender = tracing_appender::rolling::daily(&log_dir, LOG_FILE_PREFIX);

    // Wrap in non-blocking writer for performance
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    // `RUST_LOG`, when set, decides on its own. Adding the defaults on top
    // of it would override it: a directive for the same target replaces
    // the one from the environment, so `RUST_LOG=mira_proxy=debug` would
    // be pinned back to `info`.
    let env_filter = if std::env::var_os(EnvFilter::DEFAULT_ENV).is_some() {
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

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(false)
                .with_file(true)
                .with_line_number(true),
        )
        .init();

    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_dir_returns_some() {
        // Should return a valid path on all supported platforms
        let dir = log_dir();
        assert!(
            dir.is_some(),
            "log_dir() should return Some on this platform"
        );

        let path = dir.expect("already checked");
        assert!(
            path.to_string_lossy().contains("mira"),
            "log directory should contain 'mira'"
        );
    }

    #[test]
    fn test_log_file_path() {
        let path = log_file_path();
        assert!(path.is_some());

        let path = path.expect("already checked");
        let name = path
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        // `mira-cli.log.` followed by a date such as 2026-11-14.
        let date = name.strip_prefix("mira-cli.log.").expect("dated log name");
        assert_eq!(date.len(), 10, "got: {name}");
        assert!(
            chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok(),
            "got: {name}"
        );
    }
}
