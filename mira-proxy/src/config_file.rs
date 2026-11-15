//! The TOML config file the terminal UI and the desktop window share.
//!
//! Searched for in this order, after an explicit path:
//! 1. `$XDG_CONFIG_HOME/mira/config.toml` (usually
//!    `~/.config/mira/config.toml`)
//! 2. `~/.mira/config.toml` (fallback)

use crate::config::Config;
use crate::config::DEFAULT_BIND;
use crate::config::DEFAULT_CONNECT_TIMEOUT;
use crate::config::DEFAULT_MAX_BODY_SIZE;
use crate::config::DEFAULT_MAX_CONNECTIONS;
use crate::config::DEFAULT_MAX_HEADER_SIZE;
use crate::config::DEFAULT_PORT;
use crate::config::DEFAULT_READ_TIMEOUT;
use crate::config::DEFAULT_WRITE_TIMEOUT;
use crate::config::HostFilter;
use crate::error::Error;
use crate::error::Result;
use crate::session::SessionLimits;
use serde::Deserialize;
use serde::Serialize;
use std::fs;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tracing::debug;

/// Default config file name.
const CONFIG_FILE_NAME: &str = "config.toml";

/// Default fallback directory (same as CA dir default).
const FALLBACK_DIR: &str = ".mira";

/// File-based configuration for Mira.
///
/// This struct is used for TOML serialization/deserialization and provides
/// user-friendly field names and types. It is converted to `mira_proxy::Config`
/// for actual use.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct FileConfig {
    /// Address to bind to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<IpAddr>,

    /// Port to listen on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,

    /// Allow connections from external (public) IP addresses.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub allow_external: bool,

    /// Timeout for connecting to upstream servers (in seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_timeout: Option<u64>,

    /// Timeout for reading from upstream servers (in seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_timeout: Option<u64>,

    /// Timeout for writing to upstream servers (in seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_timeout: Option<u64>,

    /// Maximum request/response body size to capture (in megabytes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_body_size_mb: Option<usize>,

    /// Maximum request/response header block size (in kilobytes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_header_size_kb: Option<usize>,

    /// Maximum number of concurrent client connections the proxy will
    /// serve. Once reached, new TCP connections wait in the kernel accept
    /// queue until an in-flight one finishes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<usize>,

    /// Host filter for include/exclude patterns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_filter: Option<FileHostFilter>,

    /// HTTPS interception configuration.
    #[serde(default, skip_serializing_if = "FileHttpsConfig::is_default")]
    pub https: FileHttpsConfig,

    /// Bounds on what the capture session retains.
    #[serde(default, skip_serializing_if = "FileCaptureConfig::is_default")]
    pub capture: FileCaptureConfig,
}

/// File-based capture limits.
///
/// A long-running capture holds every exchange in memory, bodies
/// included. These bounds keep that footprint predictable; raise them if
/// you would rather spend the memory than lose the oldest traffic.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct FileCaptureConfig {
    /// Maximum number of exchanges retained (default: 10000).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_exchanges: Option<usize>,

    /// Maximum size of a single retained body, in kilobytes (default: 2048).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_body_kb: Option<usize>,

    /// Maximum total retained body bytes, in megabytes (default: 128).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_total_body_mb: Option<usize>,
}

impl FileCaptureConfig {
    /// Check if this is the default configuration.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Get the effective session limits (file values or defaults).
    pub fn limits(&self) -> SessionLimits {
        let defaults = SessionLimits::default();
        SessionLimits {
            max_exchanges: self.max_exchanges.unwrap_or(defaults.max_exchanges),
            max_body_bytes: self
                .max_body_kb
                .map_or(defaults.max_body_bytes, |kb| kb.saturating_mul(1024)),
            max_total_body_bytes: self
                .max_total_body_mb
                .map_or(defaults.max_total_body_bytes, |mb| {
                    mb.saturating_mul(1024 * 1024)
                }),
        }
    }
}

/// File-based HTTPS configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct FileHttpsConfig {
    /// Whether to intercept HTTPS traffic.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub intercept: bool,

    /// Custom CA certificate directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_dir: Option<PathBuf>,

    /// Hosts to always pass through without interception.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_hosts: Vec<String>,
}

impl FileHttpsConfig {
    /// Check if this is the default configuration.
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// File-based host filter configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default)]
pub struct FileHostFilter {
    /// Patterns to include (if empty, all hosts are included).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,

    /// Patterns to exclude (takes precedence over include).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

impl FileConfig {
    /// Get the effective port (file value or default).
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }

    /// Get the effective bind address (file value or default).
    pub fn bind(&self) -> IpAddr {
        self.bind.unwrap_or(DEFAULT_BIND)
    }

    /// Get the effective connect timeout (file value or default).
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
            .map_or(DEFAULT_CONNECT_TIMEOUT, Duration::from_secs)
    }

    /// Get the effective read timeout (file value or default).
    pub fn read_timeout(&self) -> Duration {
        self.read_timeout
            .map_or(DEFAULT_READ_TIMEOUT, Duration::from_secs)
    }

    /// Get the effective write timeout (file value or default).
    pub fn write_timeout(&self) -> Duration {
        self.write_timeout
            .map_or(DEFAULT_WRITE_TIMEOUT, Duration::from_secs)
    }

    /// Get the effective max body size in bytes (file value or default).
    pub fn max_body_size(&self) -> usize {
        self.max_body_size_mb
            .map_or(DEFAULT_MAX_BODY_SIZE, |mb| mb.saturating_mul(1024 * 1024))
    }

    /// Get the effective max header size in bytes (file value or default).
    pub fn max_header_size(&self) -> usize {
        self.max_header_size_kb
            .map_or(DEFAULT_MAX_HEADER_SIZE, |kb| kb.saturating_mul(1024))
    }

    /// Get the effective max concurrent connections (file value or default).
    pub fn max_connections(&self) -> usize {
        self.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS)
    }
}

impl From<FileConfig> for Config {
    fn from(file: FileConfig) -> Self {
        let bind_addr = SocketAddr::new(file.bind(), file.port());

        let mut config = Self::new(file.port())
            .with_bind_addr(bind_addr)
            .with_allow_external(file.allow_external)
            .with_connect_timeout(file.connect_timeout())
            .with_read_timeout(file.read_timeout())
            .with_write_timeout(file.write_timeout())
            .with_max_body_size(file.max_body_size())
            .with_max_header_size(file.max_header_size())
            .with_session_limits(file.capture.limits())
            .with_max_connections(file.max_connections())
            .with_https_intercept(file.https.intercept)
            .with_passthrough_hosts(file.https.passthrough_hosts);

        if let Some(ca_dir) = file.https.ca_dir {
            config = config.with_ca_dir(ca_dir);
        }

        if let Some(filter) = file.host_filter {
            let host_filter = HostFilter::new()
                .include_many(filter.include)
                .exclude_many(filter.exclude);
            config = config.with_host_filter(host_filter);
        }

        config
    }
}

impl From<&Config> for FileConfig {
    fn from(config: &Config) -> Self {
        let defaults = Config::default();

        // Only include values that differ from defaults
        Self {
            bind: (config.bind_addr.ip() != defaults.bind_addr.ip())
                .then_some(config.bind_addr.ip()),
            port: (config.bind_addr.port() != defaults.bind_addr.port())
                .then_some(config.bind_addr.port()),
            allow_external: config.allow_external,
            connect_timeout: (config.connect_timeout != defaults.connect_timeout)
                .then_some(config.connect_timeout.as_secs()),
            read_timeout: (config.read_timeout != defaults.read_timeout)
                .then_some(config.read_timeout.as_secs()),
            write_timeout: (config.write_timeout != defaults.write_timeout)
                .then_some(config.write_timeout.as_secs()),
            max_body_size_mb: (config.max_body_size != defaults.max_body_size)
                .then_some(config.max_body_size / (1024 * 1024)),
            max_header_size_kb: (config.max_header_size != defaults.max_header_size)
                .then_some(config.max_header_size / 1024),
            max_connections: (config.max_connections != defaults.max_connections)
                .then_some(config.max_connections),
            host_filter: config.host_filter.as_ref().map(|f| FileHostFilter {
                include: f.include.clone(),
                exclude: f.exclude.clone(),
            }),
            capture: FileCaptureConfig {
                max_exchanges: (config.session_limits.max_exchanges
                    != defaults.session_limits.max_exchanges)
                    .then_some(config.session_limits.max_exchanges),
                max_body_kb: (config.session_limits.max_body_bytes
                    != defaults.session_limits.max_body_bytes)
                    .then_some(config.session_limits.max_body_bytes / 1024),
                max_total_body_mb: (config.session_limits.max_total_body_bytes
                    != defaults.session_limits.max_total_body_bytes)
                    .then_some(config.session_limits.max_total_body_bytes / (1024 * 1024)),
            },
            https: FileHttpsConfig {
                intercept: config.https.intercept,
                ca_dir: config.https.ca_dir.clone(),
                passthrough_hosts: config.https.passthrough_hosts.clone(),
            },
        }
    }
}

/// Try to find and load a config file from standard locations.
///
/// Returns `None` if no config file exists at any standard location.
/// Returns an error if a config file exists but cannot be parsed.
pub fn load_from_default_locations() -> Result<Option<(FileConfig, PathBuf)>> {
    let paths = config_search_paths();

    for path in paths {
        if path.exists() {
            debug!(path = %path.display(), "found config file");
            let config = load_from_path(&path)?;
            return Ok(Some((config, path)));
        }
        debug!(path = %path.display(), "config file not found");
    }

    Ok(None)
}

/// Load a `FileConfig` from a specific path.
pub fn load_from_path(path: &Path) -> Result<FileConfig> {
    let contents = fs::read_to_string(path).map_err(|e| file_error(path, &e))?;

    parse_config(&contents, path)
}

/// Write `config` to `path`, creating its directory if needed.
pub fn save_to_path(config: &FileConfig, path: &Path) -> Result<()> {
    let toml = to_toml(config)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| file_error(path, &e))?;
    }
    fs::write(path, toml).map_err(|e| file_error(path, &e))
}

/// Where a config file is written when none exists yet: the first place
/// searched.
pub fn default_path() -> Option<PathBuf> {
    config_search_paths().into_iter().next()
}

/// Parse TOML content into a `FileConfig`.
fn parse_config(contents: &str, path: &Path) -> Result<FileConfig> {
    toml::from_str(contents).map_err(|e| file_error(path, &e))
}

/// Serialize a `FileConfig` to TOML.
pub fn to_toml(config: &FileConfig) -> Result<String> {
    toml::to_string_pretty(config).map_err(|e| file_error(Path::new(CONFIG_FILE_NAME), &e))
}

fn file_error(path: &Path, error: &dyn std::fmt::Display) -> Error {
    Error::ConfigFile {
        path: path.to_owned(),
        reason: error.to_string(),
    }
}

/// Get the list of paths to search for config files.
fn config_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // XDG config directory: ~/.config/mira/config.toml
    if let Some(config_dir) = dirs::config_dir() {
        paths.push(config_dir.join("mira").join(CONFIG_FILE_NAME));
    }

    // Fallback: ~/.mira/config.toml
    if let Some(home_dir) = dirs::home_dir() {
        paths.push(home_dir.join(FALLBACK_DIR).join(CONFIG_FILE_NAME));
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let toml = r#"
            port = 9000
        "#;

        let file_config = parse_config(toml, &PathBuf::from("test.toml")).unwrap();
        assert_eq!(file_config.port, Some(9000));

        let config: Config = file_config.into();
        assert_eq!(config.bind_addr.port(), 9000);
    }

    #[test]
    fn test_parse_full_config() {
        let toml = r#"
            bind = "127.0.0.1"
            port = 9000
            allow_external = false
            connect_timeout = 15
            read_timeout = 30
            write_timeout = 30
            max_body_size_mb = 100
            max_header_size_kb = 16

            [capture]
            max_exchanges = 250
            max_body_kb = 64
            max_total_body_mb = 8

            [https]
            intercept = true
            ca_dir = "/custom/ca"
            passthrough_hosts = ["*.apple.com", "*.icloud.com"]

            [host_filter]
            include = ["*.example.com"]
            exclude = ["blocked.example.com"]
        "#;

        let file_config = parse_config(toml, &PathBuf::from("test.toml")).unwrap();
        let config: Config = file_config.into();

        assert_eq!(config.bind_addr.port(), 9000);
        assert!(!config.allow_external);
        assert_eq!(config.connect_timeout.as_secs(), 15);
        assert_eq!(config.read_timeout.as_secs(), 30);
        assert_eq!(config.max_body_size, 100 * 1024 * 1024);
        assert_eq!(config.max_header_size, 16 * 1024);
        assert_eq!(config.session_limits.max_exchanges, 250);
        assert_eq!(config.session_limits.max_body_bytes, 64 * 1024);
        assert_eq!(config.session_limits.max_total_body_bytes, 8 * 1024 * 1024);

        assert!(config.https.intercept);
        assert_eq!(config.https.ca_dir, Some(PathBuf::from("/custom/ca")));
        assert_eq!(config.https.passthrough_hosts.len(), 2);

        let filter = config.host_filter.unwrap();
        assert_eq!(filter.include, vec!["*.example.com"]);
        assert_eq!(filter.exclude, vec!["blocked.example.com"]);
    }

    #[test]
    fn test_huge_sizes_saturate_instead_of_overflowing() {
        let file: FileConfig = toml::from_str(&format!(
            "max_body_size_mb = {max}\nmax_header_size_kb = {max}\n\
             [capture]\nmax_body_kb = {max}\nmax_total_body_mb = {max}\n",
            max = i64::MAX
        ))
        .expect("parses");
        let config = Config::from(file);
        assert_eq!(config.max_body_size, usize::MAX);
        assert_eq!(config.max_header_size, usize::MAX);
        assert_eq!(config.session_limits.max_body_bytes, usize::MAX);
        assert_eq!(config.session_limits.max_total_body_bytes, usize::MAX);
    }

    #[test]
    fn test_parse_empty_config() {
        let toml = "";
        let file_config = parse_config(toml, &PathBuf::from("test.toml")).unwrap();
        let config: Config = file_config.into();

        // Should use all defaults
        assert_eq!(config.bind_addr.port(), 8888);
        assert!(!config.https.intercept);
    }

    #[test]
    fn test_config_search_paths_not_empty() {
        let paths = config_search_paths();
        // Should have at least one path (XDG or fallback)
        assert!(!paths.is_empty());
    }

    #[test]
    fn test_roundtrip_config() {
        let config = Config::new(9000).with_https_intercept(true);

        let file_config = FileConfig::from(&config);
        let roundtrip: Config = file_config.into();

        assert_eq!(roundtrip.bind_addr.port(), 9000);
        assert!(roundtrip.https.intercept);
    }
}
