use crate::bus::{BusMessage, MessageBus};
use crate::config::AppConfig;
use crate::error::CoreError;
use crate::http::{HttpRequest, HttpResponse};
use crate::users::{AuthContext, UserStore};
use crossbeam_channel::Receiver;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct AppManifest {
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
    pub http_prefix: Option<&'static str>,
    pub ui_prefix: Option<&'static str>,
    pub nav_label: &'static str,
    pub nav_icon: &'static str,
    pub show_in_nav: bool,
    pub subscriptions: &'static [&'static str],
    pub publishes: &'static [&'static str],
}

#[derive(Clone)]
pub struct AppContext {
    pub bus: Arc<MessageBus>,
    pub config: AppConfig,
    pub shutdown: Receiver<()>,
    pub auth_service: Arc<UserStore>,
    /// Full path to the main config file (e.g. `/etc/bytehive/config.toml`).
    /// Apps that need to persist state alongside the config file should use
    /// this path directly or derive sibling paths from it.
    /// Server example future path: /etc/bytehive/config.toml
    /// Client example future path: /home/$USER/.config/bytehive/config.toml
    pub config_path: PathBuf,
}

impl AppContext {
    /// Directory that contains the main config file.
    /// Equivalent to `config_path.parent()`.
    pub fn config_dir(&self) -> &Path {
        self.config_path.parent().unwrap_or_else(|| Path::new("."))
    }

    pub fn publish(&self, app_name: &str, topic: impl Into<String>, payload: serde_json::Value) {
        self.bus.publish(app_name, topic, payload);
    }

    pub fn authenticate(&self, credential: &str) -> Option<AuthContext> {
        self.auth_service.authenticate_credential(credential)
    }
}

pub trait App: Send + Sync + 'static {
    fn manifest(&self) -> AppManifest;

    fn start(&self, ctx: AppContext) -> Result<(), CoreError>;

    fn stop(&self);

    fn handle_http(&self, req: &HttpRequest) -> Option<HttpResponse> {
        let _ = req;
        None
    }

    fn on_message(&self, msg: &Arc<BusMessage>) {
        let _ = msg;
    }
}
