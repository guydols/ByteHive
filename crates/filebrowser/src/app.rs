use crate::config::FileBrowserConfig;
use crate::share::Share;
use bytehive_core::html::FILEBROWSER_HTML;
use bytehive_core::{
    App, AppContext, AppManifest, BusMessage, CoreError, FrameworkConfig, HttpRequest, HttpResponse,
};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub struct Inner {
    pub root: PathBuf,
    pub max_upload_bytes: u64,
    pub allow_delete: bool,
    pub trash_manager: std::sync::Arc<bytehive_core::TrashManager>,
}

pub struct FileBrowserApp {
    pub inner: RwLock<Option<Inner>>,

    pub shares: Arc<RwLock<HashMap<String, Share>>>,
    filesync_root: Option<PathBuf>,
    filesync_trash_expiry_days: Option<u64>,
}

impl FileBrowserApp {
    pub fn new(fw_config: &FrameworkConfig) -> Arc<Self> {
        let filesync_cfg = fw_config.apps.get("filesync");

        let filesync_root = filesync_cfg
            .and_then(|v| v.get("root"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from);

        let filesync_trash_expiry_days = filesync_cfg
            .and_then(|v| v.get("trash_expiry_days"))
            .and_then(|v| v.as_integer())
            .map(|v| v as u64);

        Arc::new(Self {
            inner: RwLock::new(None),
            shares: Arc::new(RwLock::new(HashMap::new())),
            filesync_root,
            filesync_trash_expiry_days,
        })
    }

    pub fn root(&self) -> Option<PathBuf> {
        self.inner.read().as_ref().map(|i| i.root.clone())
    }
}

impl App for FileBrowserApp {
    fn manifest(&self) -> AppManifest {
        AppManifest {
            name: "filebrowser",
            version: env!("CARGO_PKG_VERSION"),
            description: "Web file browser — browse, upload, download and share files.",
            http_prefix: Some("/api/filebrowser"),
            ui_prefix: Some("/apps/filebrowser"),
            nav_label: "Files",
            nav_icon: "\u{1F5C2}",
            show_in_nav: true,
            subscriptions: &[],
            publishes: &["filebrowser.share_created", "filebrowser.share_accessed"],
        }
    }

    fn start(&self, ctx: AppContext) -> Result<(), CoreError> {
        let cfg: FileBrowserConfig = ctx.config.get().unwrap_or_default();

        let root = if let Some(r) = cfg.root {
            PathBuf::from(r)
        } else if let Some(r) = &self.filesync_root {
            r.clone()
        } else {
            return Err(CoreError::Config(
                "filebrowser: no root configured. \
                 Set root in [apps.filebrowser] or configure [apps.filesync]."
                    .into(),
            ));
        };

        if !root.exists() {
            std::fs::create_dir_all(&root).map_err(CoreError::Io)?;
        }

        let trash_manager =
            bytehive_core::TrashManager::new(root.clone(), self.filesync_trash_expiry_days);

        *self.inner.write() = Some(Inner {
            root,
            max_upload_bytes: cfg.max_upload_mb * 1024 * 1024,
            allow_delete: cfg.allow_delete,
            trash_manager: trash_manager.clone(),
        });

        let shares_arc = Arc::clone(&self.shares);
        let shutdown = ctx.shutdown.clone();
        std::thread::Builder::new()
            .name("filebrowser-gc".into())
            .spawn(move || loop {
                if shutdown
                    .recv_timeout(std::time::Duration::from_secs(300))
                    .is_ok()
                {
                    break;
                }
                let now = crate::crypto::now_ms();
                shares_arc.write().retain(|_, s| !s.is_expired_at(now));
            })
            .map_err(CoreError::Io)?;

        if self
            .filesync_trash_expiry_days
            .map(|d| d > 0)
            .unwrap_or(false)
        {
            let tm = trash_manager.clone();
            let shutdown2 = ctx.shutdown.clone();
            std::thread::Builder::new()
                .name("filebrowser-trash-gc".into())
                .spawn(move || loop {
                    if shutdown2
                        .recv_timeout(std::time::Duration::from_secs(3600))
                        .is_ok()
                    {
                        break;
                    }
                    tm.purge_expired();
                })
                .map_err(CoreError::Io)?;
        }

        log::info!(
            "filebrowser started, root = {:?}",
            self.inner.read().as_ref().map(|i| &i.root)
        );
        Ok(())
    }

    fn stop(&self) {
        *self.inner.write() = None;
        log::info!("filebrowser stopped");
    }

    fn handle_http(&self, req: &HttpRequest) -> Option<HttpResponse> {
        let sub = req
            .path
            .strip_prefix("/api/filebrowser")
            .unwrap_or(&req.path);

        if req.path.starts_with("/apps/filebrowser") {
            return Some(HttpResponse::ok_html(FILEBROWSER_HTML));
        }

        if let Some(token) = sub.strip_prefix("/s/").map(|s| s.trim_end_matches('/')) {
            return Some(self.handle_share_access(req, token));
        }

        let user = req
            .headers
            .get("x-bytehive-user")
            .cloned()
            .unwrap_or_default();
        let role = req
            .headers
            .get("x-bytehive-role")
            .cloned()
            .unwrap_or_default();

        if user.is_empty() {
            return Some(HttpResponse::unauthorized());
        }

        let is_readonly = role == "readonly";

        match (req.method.as_str(), sub) {
            ("GET", "/status") => Some(self.handle_status()),
            ("GET", "/ls") => Some(self.handle_ls(req)),
            ("GET", "/download") => Some(self.handle_download(req)),
            ("POST", "/upload") => {
                if is_readonly {
                    return Some(HttpResponse::unauthorized());
                }
                Some(self.handle_upload(req))
            }
            ("POST", "/mkdir") => {
                if is_readonly {
                    return Some(HttpResponse::unauthorized());
                }
                Some(self.handle_mkdir(req))
            }
            ("DELETE", "/delete") => {
                if is_readonly {
                    return Some(HttpResponse::unauthorized());
                }
                Some(self.handle_delete(req))
            }
            ("POST", "/rename") => {
                if is_readonly {
                    return Some(HttpResponse::unauthorized());
                }
                Some(self.handle_rename(req))
            }
            ("POST", "/share") => {
                if is_readonly {
                    return Some(HttpResponse::unauthorized());
                }
                Some(self.handle_create_share(req, &user))
            }
            ("GET", "/shares") => Some(self.handle_list_shares(&role)),
            ("DELETE", "/share") => {
                if is_readonly {
                    return Some(HttpResponse::unauthorized());
                }
                Some(self.handle_delete_share(req, &user, &role))
            }
            ("GET", "/preview") => Some(self.handle_preview(req)),
            ("GET", "/read") => Some(self.handle_read(req)),
            ("POST", "/write") => {
                if is_readonly {
                    return Some(HttpResponse::unauthorized());
                }
                Some(self.handle_write(req))
            }
            ("POST", "/copy") => {
                if is_readonly {
                    return Some(HttpResponse::unauthorized());
                }
                Some(self.handle_copy(req))
            }
            ("GET", "/search") => Some(self.handle_search(req)),
            ("GET", "/thumb") => Some(self.handle_thumb(req)),
            ("GET", "/detect") => Some(self.handle_detect(req)),
            ("GET", "/trash") => Some(self.handle_trash_list()),
            ("POST", sub) if sub.starts_with("/trash/") && sub.ends_with("/restore") => {
                if is_readonly {
                    return Some(HttpResponse::unauthorized());
                }
                let id = sub
                    .trim_start_matches("/trash/")
                    .trim_end_matches("/restore");
                Some(self.handle_trash_restore(id))
            }
            ("DELETE", sub) if sub.starts_with("/trash/") => {
                if is_readonly {
                    return Some(HttpResponse::unauthorized());
                }
                let id = sub.trim_start_matches("/trash/");
                Some(self.handle_trash_purge_entry(id))
            }
            ("DELETE", "/trash") => {
                if is_readonly {
                    return Some(HttpResponse::unauthorized());
                }
                Some(self.handle_trash_empty())
            }
            ("POST", "/trash/purge-expired") => {
                if is_readonly {
                    return Some(HttpResponse::unauthorized());
                }
                Some(self.handle_trash_purge_expired())
            }
            _ => None,
        }
    }

    fn on_message(&self, _msg: &Arc<BusMessage>) {}
}
