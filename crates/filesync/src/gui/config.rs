use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;

pub const APP_NAME: &str = "bytehive-filesync";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GuiConfig {
    pub server_addr: String,

    pub sync_root: PathBuf,

    pub auth_token: String,

    #[serde(default)]
    pub exclude_patterns: Vec<String>,

    #[serde(default)]
    pub exclude_regex: Vec<String>,

    /// Minimum log level for the GUI client process.
    /// Accepted values: "error", "warn", "info", "debug", "trace".
    /// Falls back to "info" when absent.  Can be overridden at runtime
    /// by setting the RUST_LOG environment variable.
    #[serde(default)]
    pub log_level: Option<String>,
}

impl GuiConfig {
    pub fn config_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(APP_NAME)
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    pub fn load() -> Option<Self> {
        let content = fs::read_to_string(Self::config_path()).ok()?;
        toml::from_str(&content).ok()
    }

    pub fn save(&self) -> io::Result<()> {
        let dir = Self::config_dir();
        fs::create_dir_all(&dir)?;
        let content = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        fs::write(Self::config_path(), content)
    }

    pub fn is_complete(&self) -> bool {
        !self.server_addr.is_empty()
            && !self.sync_root.as_os_str().is_empty()
            && !self.auth_token.is_empty()
    }
}
