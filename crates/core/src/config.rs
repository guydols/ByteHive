use crate::error::CoreError;
use crate::users::{ApiKey, Group, UserEntry};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameworkConfig {
    pub framework: FrameworkSection,

    #[serde(default)]
    pub users: Vec<UserEntry>,

    #[serde(default)]
    pub groups: Vec<Group>,

    #[serde(default)]
    pub api_keys: Vec<ApiKey>,

    #[serde(default)]
    pub apps: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
