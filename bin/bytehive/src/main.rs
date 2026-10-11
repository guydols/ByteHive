use bytehive_core::auth::Auth;
use bytehive_core::config::FrameworkConfig;
use bytehive_core::logging;
use bytehive_core::{ApiServer, AppRegistry, MessageBus, UserStore};
use bytehive_filebrowser::FileBrowserApp;

use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "bytehive", about = "ByteHive personal cloud framework")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
}

/// How the config file was obtained. The server starts in every case; only
/// `Loaded`/`Absent` may be rewritten by the user store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigState {
    Loaded,
    Absent,
    Invalid,
}

/// Load the config without ever aborting. A missing file yields defaults
/// (first-run setup can create it); an unreadable or invalid file yields
/// defaults and is marked `Invalid` so it is never overwritten.
fn load_config(path: &Path) -> (FrameworkConfig, ConfigState, Option<String>) {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
            FrameworkConfig::default(),
            ConfigState::Absent,
            Some(format!(
                "config file {} not found — starting with built-in defaults",
                path.display()
            )),
        ),
        _ => match FrameworkConfig::load(path) {
            Ok(c) => (c, ConfigState::Loaded, None),
            Err(e) => (
                FrameworkConfig::default(),
                ConfigState::Invalid,
                Some(format!(
                    "config file {} is unusable ({e}) — starting with built-in defaults; \
                     the file will not be rewritten",
                    path.display()
                )),
            ),
        },
    }
}

fn main() {
    let cli = Cli::parse();

    let (cfg, config_state, config_notice) = load_config(&cli.config);
    let cfg = Arc::new(cfg);

    // Raw text kept so UserStore can splice auth sections back on every
    // mutation without disturbing comments/formatting elsewhere.
    let raw_toml = FrameworkConfig::load_raw(&cli.config);

    std::env::set_var("RUST_LOG", &cfg.framework.log_level);
    let stderr_logger = env_logger::Builder::from_default_env().build();
    let log_root = logging::resolve_log_dir(&cfg.logging);
    if let Err(e) = logging::init(stderr_logger, &log_root, &cfg.logging) {
        eprintln!("warning: logger already installed: {e}");
    }

    log::info!("ByteHive starting up");
    log::info!(
        "logs: {} (server) and {} (bus)",
        logging::server_log_path(&log_root).display(),
        logging::bus_log_path(&log_root).display()
    );
    if let Some(notice) = config_notice {
        log::warn!("{notice}");
    }

    let bus = MessageBus::new();

    match logging::spawn_bus_logger(&bus, &log_root, &cfg.logging) {
        Ok(_) => log::debug!("bus logger started"),
        Err(e) => log::warn!("bus logger not started: {e}"),
    }
    if let Err(e) = logging::spawn_cleanup(log_root.clone(), &cfg.logging) {
        log::warn!("log cleanup not started: {e}");
    }

    // Never let the user store rewrite a config file we could not parse.
    let persist_path = match config_state {
        ConfigState::Invalid => None,
        ConfigState::Loaded | ConfigState::Absent => Some(cli.config.clone()),
    };

    let user_store = UserStore::new(
        cfg.users.clone(),
        cfg.groups.clone(),
        cfg.api_keys.clone(),
        cfg.framework.http_token.clone(),
        persist_path,
        raw_toml,
    );

    let auth = Arc::new(Auth::new(cfg.framework.http_token.clone()));

    let registry = AppRegistry::new(
        Arc::clone(&bus),
        Arc::clone(&cfg),
        Arc::clone(&user_store),
        cli.config.clone(),
    );

    register_filesync(&registry, &cfg);

    let api = ApiServer::new(
        cfg.framework.http_addr.clone(),
        Arc::clone(&registry),
        Arc::clone(&bus),
        auth,
        Arc::clone(&user_store),
        cfg.framework.web_root.clone(),
    );

    match api.start() {
        Ok(_) => log::info!("Dashboard → http://{}/", cfg.framework.http_addr),
        Err(e) => {
            log::error!("Failed to start HTTP API: {e}");
            std::process::exit(1);
        }
    }

    log::info!("ByteHive running. Press Ctrl+C to stop.");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

fn register_filesync(registry: &Arc<AppRegistry>, cfg: &FrameworkConfig) {
    if !cfg.apps.contains_key("filesync") {
        log::debug!("no [apps.filesync] config — skipping");
        return;
    }
    match registry.register(bytehive_filesync::FileSyncApp::new()) {
        Ok(()) => log::info!("filesync registered"),
        Err(e) => log::error!("failed to register filesync: {e}"),
    }
    match registry.register(FileBrowserApp::new(cfg)) {
        Ok(()) => log::info!("filebrowser registered"),
        Err(e) => log::error!("failed to register filebrowser: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch dir under the OS temp dir (removed on drop).
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp_file(name: &str, body: Option<&str>) -> (Scratch, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bytehive-main-test-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        if let Some(body) = body {
            std::fs::write(&path, body).unwrap();
        }
        (Scratch(dir), path)
    }

    #[test]
    fn missing_config_starts_with_defaults() {
        let (_d, path) = tmp_file("config.toml", None);
        let (cfg, state, notice) = load_config(&path);
        assert_eq!(state, ConfigState::Absent);
        assert!(notice.unwrap().contains("not found"));
        assert_eq!(cfg.framework.log_level, "info");
        assert_eq!(cfg.logging, bytehive_core::config::LoggingConfig::default());
    }

    #[test]
    fn invalid_config_starts_with_defaults_and_is_not_persisted() {
        let (_d, path) = tmp_file("config.toml", Some("[framework\nnot toml"));
        let (cfg, state, notice) = load_config(&path);
        assert_eq!(state, ConfigState::Invalid);
        assert!(notice.unwrap().contains("will not be rewritten"));
        assert_eq!(cfg.framework.http_addr, "0.0.0.0:9000");
    }

    #[test]
    fn valid_config_is_loaded_with_logging_section() {
        let (_d, path) = tmp_file(
            "config.toml",
            Some("[framework]\nhttp_addr = \"127.0.0.1:9100\"\n[logging]\nmax_files = 3\n"),
        );
        let (cfg, state, notice) = load_config(&path);
        assert_eq!(state, ConfigState::Loaded);
        assert!(notice.is_none());
        assert_eq!(cfg.framework.http_addr, "127.0.0.1:9100");
        assert_eq!(cfg.logging.max_files, 3);
    }

    #[test]
    fn repo_config_toml_still_parses() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config.toml");
        let (_cfg, state, _) = load_config(&path);
        assert_eq!(state, ConfigState::Loaded);
    }
}
