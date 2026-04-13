use crate::app::{App, AppContext, AppManifest};
use crate::bus::MessageBus;
use crate::config::{AppConfig, FrameworkConfig};
use crate::error::CoreError;
use crate::http::{HttpRequest, HttpResponse};
use crate::users::UserStore;
use crossbeam_channel::bounded;
use parking_lot::RwLock;
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AppStatus {
    Running,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,

    pub http_prefix: Option<&'static str>,

    pub ui_prefix: Option<&'static str>,

    pub nav_label: &'static str,

    pub nav_icon: &'static str,

    pub show_in_nav: bool,
    pub subscriptions: Vec<&'static str>,
    pub publishes: Vec<&'static str>,
    pub status: AppStatus,

    pub status_detail: Option<String>,

    pub config_toml: String,

    pub uptime_secs: Option<u64>,
}

struct Slot {
    app: Arc<dyn App>,
    manifest: AppManifest,

    stop_tx: crossbeam_channel::Sender<()>,

    config: AppConfig,
    status: AppStatus,
    status_detail: Option<String>,
    started_at: Option<Instant>,
}

impl Slot {
    fn to_info(&self) -> AppInfo {
        let nav_label = if self.manifest.nav_label.is_empty() {
            self.manifest.name
        } else {
            self.manifest.nav_label
        };
        AppInfo {
            name: self.manifest.name,
            version: self.manifest.version,
            description: self.manifest.description,
            http_prefix: self.manifest.http_prefix,
            ui_prefix: self.manifest.ui_prefix,
            nav_label,
            nav_icon: self.manifest.nav_icon,
            show_in_nav: self.manifest.show_in_nav,
            subscriptions: self.manifest.subscriptions.to_vec(),
            publishes: self.manifest.publishes.to_vec(),
            status: self.status.clone(),
            status_detail: self.status_detail.clone(),
            config_toml: toml::to_string_pretty(&self.config.inner).unwrap_or_default(),
            uptime_secs: self.started_at.map(|t| t.elapsed().as_secs()),
        }
    }
}

pub struct AppRegistry {
    slots: RwLock<Vec<Slot>>,
    bus: Arc<MessageBus>,
    config: Arc<FrameworkConfig>,
    user_store: Arc<UserStore>,
    config_path: std::path::PathBuf,
}

impl AppRegistry {
    pub fn new(
        bus: Arc<MessageBus>,
        config: Arc<FrameworkConfig>,
        user_store: Arc<UserStore>,
        config_path: std::path::PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            slots: RwLock::new(Vec::new()),
            bus,
            config,
            user_store,
            config_path,
        })
    }

    pub fn register(&self, app: Arc<dyn App>) -> Result<(), CoreError> {
        let manifest = app.manifest();
        let name = manifest.name;

        if self.slots.read().iter().any(|s| s.manifest.name == name) {
            return Err(CoreError::AppAlreadyRegistered(name.to_string()));
        }

        let initial_config = self.config.app_config(name);
        let (stop_tx, stop_rx) = bounded::<()>(1);

        Self::wire_subscriptions(&app, &manifest, &stop_rx, &self.bus)?;

        let ctx = AppContext {
            bus: Arc::clone(&self.bus),
            config: initial_config.clone(),
            shutdown: stop_rx,
            auth_service: Arc::clone(&self.user_store),
            config_path: self.config_path.clone(),
        };

        log::info!("starting app '{}' v{}", name, manifest.version);
        let result = app.start(ctx);
        let (status, detail) = match &result {
            Ok(()) => (AppStatus::Running, None),
            Err(e) => (AppStatus::Failed, Some(e.to_string())),
        };

        if let Err(e) = result {
            return Err(e);
        }
        log::info!("app '{}' started", name);

        self.slots.write().push(Slot {
            app,
            manifest,
            stop_tx,
            config: initial_config,
            status,
            status_detail: detail,
            started_at: Some(Instant::now()),
        });
        Ok(())
    }

    fn wire_subscriptions(
        app: &Arc<dyn App>,
        manifest: &AppManifest,
        stop_rx: &crossbeam_channel::Receiver<()>,
        bus: &Arc<MessageBus>,
    ) -> Result<(), CoreError> {
        for &pattern in manifest.subscriptions {
            let msg_rx = bus.sub(pattern);
            let app_ref = Arc::clone(app);
            let stop = stop_rx.clone();
            let tag = format!("{}-sub-{}", manifest.name, pattern);

            std::thread::Builder::new()
                .name(tag)
                .spawn(move || loop {
                    crossbeam_channel::select! {
                        recv(msg_rx) -> msg => match msg {
                            Ok(msg) => app_ref.on_message(&msg),
                            Err(_)  => break,
                        },
                        recv(stop) -> _ => break,
                    }
                })
                .map_err(CoreError::Io)?;
        }
        Ok(())
    }

    pub fn stop_app(&self, name: &str) -> Result<(), CoreError> {
        let mut slots = self.slots.write();
        let slot = slots
            .iter_mut()
            .find(|s| s.manifest.name == name)
            .ok_or_else(|| CoreError::AppNotFound(name.to_string()))?;

        if slot.status == AppStatus::Stopped {
            return Ok(());
        }
        let _ = slot.stop_tx.send(());
        slot.app.stop();
        slot.status = AppStatus::Stopped;
        slot.status_detail = None;
        slot.started_at = None;
        log::info!("app '{name}' stopped");
        Ok(())
    }

    pub fn start_app(&self, name: &str) -> Result<(), CoreError> {
        let (app, manifest, config) = {
            let slots = self.slots.read();
            let slot = slots
                .iter()
                .find(|s| s.manifest.name == name)
                .ok_or_else(|| CoreError::AppNotFound(name.to_string()))?;
            if slot.status == AppStatus::Running {
                return Ok(());
            }
            (
                Arc::clone(&slot.app),
                slot.manifest.clone(),
                slot.config.clone(),
            )
        };

        let (stop_tx, stop_rx) = bounded::<()>(1);
        Self::wire_subscriptions(&app, &manifest, &stop_rx, &self.bus)?;

        let ctx = AppContext {
            bus: Arc::clone(&self.bus),
            config: config.clone(),
            shutdown: stop_rx,
            auth_service: Arc::clone(&self.user_store),
            config_path: self.config_path.clone(),
        };

        log::info!("starting app '{name}'");
        let result = app.start(ctx);
        let (status, detail) = match &result {
            Ok(()) => (AppStatus::Running, None),
            Err(e) => (AppStatus::Failed, Some(e.to_string())),
        };
        let ok = result.is_ok();

        let mut slots = self.slots.write();
        if let Some(slot) = slots.iter_mut().find(|s| s.manifest.name == name) {
            slot.stop_tx = stop_tx;
            slot.status = status;
            slot.status_detail = detail;
            slot.started_at = if ok { Some(Instant::now()) } else { None };
        }
        if ok {
            log::info!("app '{name}' started");
            Ok(())
        } else {
            Err(CoreError::App(format!("app '{name}' failed to start")))
        }
    }

    pub fn restart_app(&self, name: &str) -> Result<(), CoreError> {
        log::info!("restarting app '{name}'");
        self.stop_app(name)?;
        std::thread::sleep(std::time::Duration::from_millis(300));
        self.start_app(name)
    }

    pub fn update_config(&self, name: &str, toml_str: &str) -> Result<(), CoreError> {
        let value: toml::Value = toml::from_str(toml_str)
            .map_err(|e| CoreError::Config(format!("TOML parse error: {e}")))?;

        let mut slots = self.slots.write();
        let slot = slots
            .iter_mut()
            .find(|s| s.manifest.name == name)
            .ok_or_else(|| CoreError::AppNotFound(name.to_string()))?;

        slot.config = AppConfig { inner: value };
        log::info!("config updated for app '{name}' — restart to apply");
        Ok(())
    }

    pub fn all_app_infos(&self) -> Vec<AppInfo> {
        self.slots.read().iter().map(|s| s.to_info()).collect()
    }

    pub fn app_info(&self, name: &str) -> Option<AppInfo> {
        self.slots
            .read()
            .iter()
            .find(|s| s.manifest.name == name)
            .map(|s| s.to_info())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn App>> {
        self.slots
            .read()
            .iter()
            .find(|s| s.manifest.name == name)
            .map(|s| Arc::clone(&s.app))
    }

    pub fn manifests(&self) -> Vec<AppManifest> {
        self.slots
            .read()
            .iter()
            .map(|s| s.manifest.clone())
            .collect()
    }

    pub fn route_http(&self, req: &HttpRequest) -> HttpResponse {
        let slots = self.slots.read();
        for slot in slots.iter() {
            if slot.status != AppStatus::Running {
                continue;
            }

            let matches = slot
                .manifest
                .http_prefix
                .map(|p| path_matches(p, &req.path))
                .unwrap_or(false)
                || slot
                    .manifest
                    .ui_prefix
                    .map(|p| path_matches(p, &req.path))
                    .unwrap_or(false);

            if matches {
                if let Some(resp) = slot.app.handle_http(req) {
                    return resp;
                }
            }
        }
        HttpResponse::not_found(format!("no handler for {}", req.path))
    }

    pub fn stop_all(&self) {
        let mut slots = self.slots.write();
        for slot in slots.iter_mut().rev() {
            if slot.status == AppStatus::Running {
                log::info!("stopping app '{}'", slot.manifest.name);
                let _ = slot.stop_tx.send(());
                slot.app.stop();
                slot.status = AppStatus::Stopped;
            }
        }
        slots.clear();
    }
}

fn path_matches(prefix: &str, path: &str) -> bool {
    if !path.starts_with(prefix) {
        return false;
    }
    let rest = &path[prefix.len()..];
    rest.is_empty() || rest.starts_with('/') || rest.starts_with('?')
}
