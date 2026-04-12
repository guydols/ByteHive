use bytehive_core::auth::Auth;
use bytehive_core::config::FrameworkConfig;
use bytehive_core::{ApiServer, AppRegistry, MessageBus, UserStore};
use bytehive_filebrowser::FileBrowserApp;

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "bytehive", about = "ByteHive personal cloud framework")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
}

fn main() {
    let cli = Cli::parse();

    let cfg = match FrameworkConfig::load(&cli.config) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("Failed to load config {:?}: {e}", cli.config);
            std::process::exit(1);
        }
    };

    // Raw text kept so UserStore can splice auth sections back on every
    // mutation without disturbing comments/formatting elsewhere.
    let raw_toml = FrameworkConfig::load_raw(&cli.config);

    std::env::set_var("RUST_LOG", &cfg.framework.log_level);
    env_logger::init();

    log::info!("ByteHive starting up");

    let bus = MessageBus::new();

    let user_store = UserStore::new(
        cfg.users.clone(),
        cfg.groups.clone(),
        cfg.api_keys.clone(),
        cfg.framework.http_token.clone(),
        Some(cli.config.clone()),
        raw_toml,
    );

    let auth = Arc::new(Auth::new(cfg.framework.http_token.clone()));

    let registry = AppRegistry::new(Arc::clone(&bus), Arc::clone(&cfg), Arc::clone(&user_store));

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
