use bytehive_filesync::gui::app;
use bytehive_filesync::gui::config::GuiConfig;
use bytehive_filesync::gui::tray;

fn main() {
    let log_level = GuiConfig::load()
        .and_then(|c| c.log_level)
        .unwrap_or_else(|| "info".to_string());
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level)).init();

    let tray_handle = tray::setup_tray();

    if let Err(e) = app::run(tray_handle) {
        eprintln!("GUI error: {e}");
        std::process::exit(1);
    }
}
