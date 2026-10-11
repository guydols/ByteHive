use crate::error::CoreError;
use crate::users::{ApiKey, Group, UserEntry};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FrameworkConfig {
    #[serde(default)]
    pub framework: FrameworkSection,

    #[serde(default)]
    pub users: Vec<UserEntry>,

    #[serde(default)]
    pub groups: Vec<Group>,

    #[serde(default)]
    pub api_keys: Vec<ApiKey>,

    #[serde(default)]
    pub apps: HashMap<String, toml::Value>,

    /// `[logging]` — server log file and bus JSONL retention. Optional;
    /// every field has a default.
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// `[logging]` section. See config.toml for documentation of each knob.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LoggingConfig {
    /// Logs root directory. `None` = `<binary dir>/logs`.
    pub dir: Option<String>,
    /// Rotate a log file once it would exceed this many MiB. `0` = no rotation.
    pub max_file_mb: u64,
    /// Rotated copies kept per stream (`name.log.1` … `name.log.N`).
    pub max_files: u32,
    /// Delete rotated files older than this many days. `0` = keep forever.
    pub retention_days: u64,
    /// Cap on total bytes in the logs dir (MiB); oldest rotated files go first.
    /// `0` = no cap.
    pub max_total_mb: u64,
    /// Seconds between cleanup passes. `0` = cleanup at startup only.
    pub cleanup_interval_secs: u64,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            dir: None,
            max_file_mb: 10,
            max_files: 5,
            retention_days: 14,
            max_total_mb: 500,
            cleanup_interval_secs: 3600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameworkSection {
    #[serde(default = "default_http_addr")]
    pub http_addr: String,

    #[serde(default)]
    pub http_token: String,

    #[serde(default)]
    pub web_root: String,

    #[serde(default = "default_log_level")]
    pub log_level: String,
}

impl Default for FrameworkSection {
    fn default() -> Self {
        Self {
            http_addr: default_http_addr(),
            http_token: String::new(),
            web_root: String::new(),
            log_level: default_log_level(),
        }
    }
}

fn default_http_addr() -> String {
    "0.0.0.0:9000".into()
}
fn default_log_level() -> String {
    "info".into()
}

impl FrameworkConfig {
    pub fn load(path: &Path) -> Result<Self, CoreError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| CoreError::Config(format!("cannot read {path:?}: {e}")))?;
        toml::from_str(&raw).map_err(|e| CoreError::Config(format!("TOML parse error: {e}")))
    }

    pub fn load_raw(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    pub fn app_config(&self, name: &str) -> AppConfig {
        AppConfig {
            inner: self
                .apps
                .get(name)
                .cloned()
                .unwrap_or(toml::Value::Table(toml::map::Map::new())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub(crate) inner: toml::Value,
}

impl AppConfig {
    pub fn empty() -> Self {
        AppConfig {
            inner: toml::Value::Table(toml::map::Map::new()),
        }
    }

    pub fn get<T: serde::de::DeserializeOwned>(&self) -> Result<T, CoreError> {
        self.inner
            .clone()
            .try_into()
            .map_err(|e: toml::de::Error| CoreError::Config(e.to_string()))
    }

    pub fn raw(&self) -> &toml::Value {
        &self.inner
    }
}

#[cfg(test)]
mod logging_config_tests {
    use super::*;

    #[test]
    fn config_without_logging_section_uses_defaults() {
        let cfg: FrameworkConfig =
            toml::from_str("[framework]\nhttp_addr = \"127.0.0.1:1\"\n").unwrap();
        assert_eq!(cfg.logging, LoggingConfig::default());
        assert_eq!(cfg.logging.max_file_mb, 10);
        assert_eq!(cfg.logging.max_files, 5);
        assert_eq!(cfg.logging.retention_days, 14);
        assert_eq!(cfg.logging.max_total_mb, 500);
        assert_eq!(cfg.logging.cleanup_interval_secs, 3600);
        assert_eq!(cfg.logging.dir, None);
    }

    #[test]
    fn partial_logging_section_overrides_only_given_fields() {
        let cfg: FrameworkConfig =
            toml::from_str("[framework]\n[logging]\nmax_files = 2\ndir = \"/tmp/x\"\n").unwrap();
        assert_eq!(cfg.logging.max_files, 2);
        assert_eq!(cfg.logging.dir.as_deref(), Some("/tmp/x"));
        assert_eq!(cfg.logging.max_file_mb, 10);
        assert_eq!(cfg.logging.retention_days, 14);
    }

    #[test]
    fn config_without_framework_section_is_valid() {
        let cfg: FrameworkConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.framework.http_addr, "0.0.0.0:9000");
        assert_eq!(cfg.framework.log_level, "info");
    }

    #[test]
    fn framework_default_matches_serde_defaults() {
        let from_serde: FrameworkSection = toml::from_str("").unwrap();
        let d = FrameworkSection::default();
        assert_eq!(d.http_addr, from_serde.http_addr);
        assert_eq!(d.log_level, from_serde.log_level);
        assert_eq!(d.http_token, from_serde.http_token);
        assert_eq!(d.web_root, from_serde.web_root);
    }
}
