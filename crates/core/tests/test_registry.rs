use bytehive_core::app::{App, AppContext, AppManifest};
use bytehive_core::bus::MessageBus;
use bytehive_core::config::FrameworkSection;
use bytehive_core::error::CoreError;
use bytehive_core::http::{HttpRequest, HttpResponse};
use bytehive_core::registry::{AppRegistry, AppStatus};
use bytehive_core::users::UserStore;
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

struct MockApp {
    name: &'static str,
    start_called: std::sync::atomic::AtomicBool,
    stop_called: std::sync::atomic::AtomicBool,
}

impl MockApp {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            start_called: std::sync::atomic::AtomicBool::new(false),
            stop_called: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl App for MockApp {
    fn manifest(&self) -> AppManifest {
        AppManifest {
            name: self.name,
            version: "1.0",
            description: "mock",
            http_prefix: Some("/api/mock"),
            ui_prefix: Some("/apps/mock"),
            nav_label: "Mock",
            nav_icon: "M",
            show_in_nav: true,
            subscriptions: &[],
            publishes: &[],
        }
    }

    fn start(&self, _ctx: AppContext) -> Result<(), CoreError> {
        self.start_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn stop(&self) {
        self.stop_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn test_setup() -> (Arc<AppRegistry>, Arc<MockApp>) {
    let bus = MessageBus::new();
    let config = Arc::new(bytehive_core::config::FrameworkConfig {
        framework: FrameworkSection::default(),
        users: vec![],
        groups: vec![],
        api_keys: vec![],
        apps: HashMap::new(),
    });
    let user_store = UserStore::empty();
    let reg = AppRegistry::new(bus, config, user_store);
    let app = Arc::new(MockApp::new("mock"));
    (reg, app)
}

#[test]
fn register_app_starts_it() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    thread::sleep(Duration::from_millis(50));
    assert!(app.start_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(!app.stop_called.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn register_duplicate_fails() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    let err = reg.register(app).unwrap_err();
    assert!(matches!(err, CoreError::AppAlreadyRegistered(_)));
}

#[test]
fn stop_app_stops_running_app() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    thread::sleep(Duration::from_millis(50));
    reg.stop_app("mock").unwrap();
    assert!(app.stop_called.load(std::sync::atomic::Ordering::SeqCst));
    let info = reg.app_info("mock").unwrap();
    assert_eq!(info.status, AppStatus::Stopped);
}

#[test]
fn start_stopped_app_restarts() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    reg.stop_app("mock").unwrap();
    app.start_called
        .store(false, std::sync::atomic::Ordering::SeqCst);
    reg.start_app("mock").unwrap();
    thread::sleep(Duration::from_millis(50));
    assert!(app.start_called.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn update_config_modifies_stored_config() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    let toml_str = "key = \"new_value\"";
    reg.update_config("mock", toml_str).unwrap();
    let info = reg.app_info("mock").unwrap();
    assert!(info.config_toml.contains("new_value"));
}

#[test]
fn route_http_returns_404_for_unhandled_path() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    let req = HttpRequest {
        method: "GET".to_string(),
        path: "/api/mock/status".to_string(),
        query: "".to_string(),
        headers: HashMap::new(),
        body: vec![],
        auth: None,
    };
    let resp = reg.route_http(&req);
    assert_eq!(resp.status, 404);
}

struct RespondingApp;

impl App for RespondingApp {
    fn manifest(&self) -> AppManifest {
        AppManifest {
            name: "responder",
            version: "1.0",
            description: "responds to HTTP",
            http_prefix: Some("/api/responder"),
            ui_prefix: None,
            nav_label: "", // empty → should fall back to name
            nav_icon: "R",
            show_in_nav: false,
            subscriptions: &[],
            publishes: &[],
        }
    }

    fn start(&self, _ctx: AppContext) -> Result<(), CoreError> {
        Ok(())
    }

    fn stop(&self) {}

    fn handle_http(&self, _req: &HttpRequest) -> Option<HttpResponse> {
        Some(HttpResponse::ok_text("pong"))
    }
}

fn responder_setup() -> Arc<AppRegistry> {
    let bus = MessageBus::new();
    let config = Arc::new(bytehive_core::config::FrameworkConfig {
        framework: FrameworkSection::default(),
        users: vec![],
        groups: vec![],
        api_keys: vec![],
        apps: HashMap::new(),
    });
    let user_store = UserStore::empty();
    AppRegistry::new(bus, config, user_store)
}

#[test]
fn route_http_returns_app_response() {
    let reg = responder_setup();
    reg.register(Arc::new(RespondingApp)).unwrap();
    let req = HttpRequest {
        method: "GET".to_string(),
        path: "/api/responder/ping".to_string(),
        query: "".to_string(),
        headers: HashMap::new(),
        body: vec![],
        auth: None,
    };
    let resp = reg.route_http(&req);
    assert_eq!(resp.status, 200);
}

#[test]
fn stop_all_clears_all_apps() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    reg.stop_all();
    assert!(reg.all_app_infos().is_empty());
}

#[test]
fn restart_app_stops_then_restarts() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    thread::sleep(Duration::from_millis(50));
    reg.restart_app("mock").unwrap();
    thread::sleep(Duration::from_millis(50));
    let info = reg.app_info("mock").unwrap();
    assert_eq!(info.status, AppStatus::Running);
}

#[test]
fn app_info_unknown_returns_none() {
    let (reg, _) = test_setup();
    assert!(reg.app_info("nonexistent").is_none());
}

#[test]
fn get_known_app_returns_some() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    assert!(reg.get("mock").is_some());
}

#[test]
fn get_unknown_app_returns_none() {
    let (reg, _) = test_setup();
    assert!(reg.get("unknown").is_none());
}

#[test]
fn manifests_returns_all_registered() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    let manifests = reg.manifests();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0].name, "mock");
}

#[test]
fn all_app_infos_returns_all() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    let infos = reg.all_app_infos();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].name, "mock");
}

#[test]
fn nav_label_empty_falls_back_to_name() {
    let reg = responder_setup();
    reg.register(Arc::new(RespondingApp)).unwrap();
    let info = reg.app_info("responder").unwrap();
    // nav_label is "" in manifest → to_info() must substitute the app name
    assert_eq!(info.nav_label, "responder");
}

#[test]
fn stop_app_unknown_returns_error() {
    let (reg, _) = test_setup();
    let err = reg.stop_app("does_not_exist").unwrap_err();
    assert!(matches!(err, CoreError::AppNotFound(_)));
}

#[test]
fn start_app_unknown_returns_error() {
    let (reg, _) = test_setup();
    let err = reg.start_app("does_not_exist").unwrap_err();
    assert!(matches!(err, CoreError::AppNotFound(_)));
}

#[test]
fn stop_already_stopped_is_ok() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    reg.stop_app("mock").unwrap();
    // Stopping an already-stopped app must be a silent no-op
    assert!(reg.stop_app("mock").is_ok());
}

#[test]
fn start_already_running_is_ok() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    // start_app while already Running must succeed without restarting
    assert!(reg.start_app("mock").is_ok());
}

#[test]
fn update_config_unknown_returns_error() {
    let (reg, _) = test_setup();
    let err = reg.update_config("ghost", "key = \"v\"").unwrap_err();
    assert!(matches!(err, CoreError::AppNotFound(_)));
}

#[test]
fn update_config_invalid_toml_returns_error() {
    let (reg, app) = test_setup();
    reg.register(app.clone()).unwrap();
    let err = reg.update_config("mock", "not [[[ valid toml").unwrap_err();
    assert!(matches!(err, CoreError::Config(_)));
}
