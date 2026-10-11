//! Client labels + known-clients admin gate + persist regression.
//!
//! Covers:
//! * label validation through the HTTP handler (400 on bad input, trim, clear)
//! * `set_label` persisting to the config file
//! * admin-only enforcement on every `/api/filesync/known-clients` route
//! * the UserStore persist data-loss regression (label survives a user create)

use bytehive_core::app::{App, AppContext};
use bytehive_core::bus::MessageBus;
use bytehive_core::config::FrameworkConfig;
use bytehive_core::http::{HttpRequest, HttpResponse};
use bytehive_core::users::{AuthContext, AuthMethod, UserEntry, UserStore, GROUP_ADMIN};
use bytehive_filesync::{ClientStatus, FileSyncApp, KnownClients};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const FP: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const NODE: &str = "gui-18dd";

fn ctx_for(groups: &[&str]) -> Option<AuthContext> {
    Some(AuthContext {
        username: "tester".into(),
        display_name: "Tester".into(),
        groups: groups.iter().map(|g| g.to_string()).collect(),
        method: AuthMethod::Session,
    })
}

fn admin() -> Option<AuthContext> {
    ctx_for(&[GROUP_ADMIN, "user"])
}

fn plain_user() -> Option<AuthContext> {
    ctx_for(&["user"])
}

fn req(method: &str, path: &str, body: &str, auth: Option<AuthContext>) -> HttpRequest {
    HttpRequest {
        method: method.into(),
        path: path.into(),
        query: String::new(),
        headers: HashMap::new(),
        body: body.as_bytes().to_vec(),
        auth,
    }
}

fn body_text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn tmp_dir(name: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bh_labels_{}_{name}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create tmp dir");
    dir
}

/// Config file for a server-mode filesync on an ephemeral port, with one
/// pending client seeded into `[[filesync_known_clients]]`.
fn server_fixture(name: &str) -> (PathBuf, PathBuf) {
    let dir = tmp_dir(name);
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let cfg_path = dir.join("config.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "[framework]\nhttp_addr = \"127.0.0.1:0\"\n\n\
             [apps.filesync]\nroot = {:?}\nmode = \"server\"\nbind_addr = \"127.0.0.1:0\"\n",
            root.display().to_string()
        ),
    )
    .unwrap();
    let mut kc = KnownClients::load_from_config(&cfg_path);
    kc.upsert_pending(NODE, FP, "10.0.0.5:4000");
    (dir, cfg_path)
}

fn start_app(cfg_path: &Path) -> Arc<FileSyncApp> {
    let cfg = FrameworkConfig::load(cfg_path).expect("config loads");
    let (_tx, rx) = crossbeam_channel::unbounded::<()>();
    let ctx = AppContext {
        bus: MessageBus::new(),
        config: cfg.app_config("filesync"),
        shutdown: rx,
        auth_service: UserStore::empty(),
        config_path: cfg_path.to_path_buf(),
    };
    let app = FileSyncApp::new();
    app.start(ctx).expect("filesync starts in server mode");
    app
}

fn disk_label(cfg_path: &Path) -> String {
    KnownClients::load_from_config(cfg_path)
        .get(FP)
        .expect("client on disk")
        .label
        .clone()
}

fn disk_status(cfg_path: &Path) -> ClientStatus {
    KnownClients::load_from_config(cfg_path)
        .status(FP)
        .expect("client on disk")
}

fn known_path(suffix: &str) -> String {
    format!("/api/filesync/known-clients{suffix}")
}

// ---------------------------------------------------------------------------
// Label validation via HTTP
// ---------------------------------------------------------------------------

#[test]
fn label_post_trims_and_persists_to_disk() {
    let (_dir, cfg) = server_fixture("trim");
    let app = start_app(&cfg);
    let path = known_path(&format!("/{FP}/label"));
    let resp = app
        .handle_http(&req(
            "POST",
            &path,
            r#"{"label":"  Alice laptop  "}"#,
            admin(),
        ))
        .unwrap();
    assert_eq!(resp.status, 200, "body: {}", body_text(&resp));
    assert_eq!(disk_label(&cfg), "Alice laptop");
    app.stop();
}

#[test]
fn label_post_empty_string_clears_label() {
    let (_dir, cfg) = server_fixture("clear");
    let app = start_app(&cfg);
    let path = known_path(&format!("/{FP}/label"));
    app.handle_http(&req("POST", &path, r#"{"label":"Bob"}"#, admin()))
        .unwrap();
    assert_eq!(disk_label(&cfg), "Bob");
    let resp = app
        .handle_http(&req("POST", &path, r#"{"label":""}"#, admin()))
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(disk_label(&cfg), "");
    app.stop();
}

#[test]
fn label_post_whitespace_only_clears_label() {
    let (_dir, cfg) = server_fixture("ws_clear");
    let app = start_app(&cfg);
    let path = known_path(&format!("/{FP}/label"));
    app.handle_http(&req("POST", &path, r#"{"label":"Bob"}"#, admin()))
        .unwrap();
    let resp = app
        .handle_http(&req("POST", &path, r#"{"label":"   "}"#, admin()))
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(disk_label(&cfg), "");
    app.stop();
}

#[test]
fn label_post_rejects_over_64_chars_with_400_and_keeps_old_label() {
    let (_dir, cfg) = server_fixture("too_long");
    let app = start_app(&cfg);
    let path = known_path(&format!("/{FP}/label"));
    app.handle_http(&req("POST", &path, r#"{"label":"keep"}"#, admin()))
        .unwrap();
    let long = "a".repeat(65);
    let body = format!(r#"{{"label":"{long}"}}"#);
    let resp = app
        .handle_http(&req("POST", &path, &body, admin()))
        .unwrap();
    assert_eq!(resp.status, 400);
    assert!(
        body_text(&resp).contains("64"),
        "error should mention limit"
    );
    assert_eq!(
        disk_label(&cfg),
        "keep",
        "rejected label must not be stored"
    );
    app.stop();
}

#[test]
fn label_post_accepts_exactly_64_chars() {
    let (_dir, cfg) = server_fixture("exact64");
    let app = start_app(&cfg);
    let path = known_path(&format!("/{FP}/label"));
    let ok = "b".repeat(64);
    let body = format!(r#"{{"label":"{ok}"}}"#);
    let resp = app
        .handle_http(&req("POST", &path, &body, admin()))
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(disk_label(&cfg), ok);
    app.stop();
}

#[test]
fn label_post_rejects_control_characters_with_400() {
    let (_dir, cfg) = server_fixture("ctrl");
    let app = start_app(&cfg);
    let path = known_path(&format!("/{FP}/label"));
    // JSON-escaped newline: a log-injection attempt.
    let resp = app
        .handle_http(&req(
            "POST",
            &path,
            r#"{"label":"Alice\nFAKE LOG LINE"}"#,
            admin(),
        ))
        .unwrap();
    assert_eq!(resp.status, 400);
    assert_eq!(disk_label(&cfg), "", "label must not be stored");
    app.stop();
}

#[test]
fn label_post_requires_string_label_field_400() {
    let (_dir, cfg) = server_fixture("badbody");
    let app = start_app(&cfg);
    let path = known_path(&format!("/{FP}/label"));
    for body in ["", "not json", "{}", r#"{"label":42}"#, r#"{"name":"x"}"#] {
        let resp = app.handle_http(&req("POST", &path, body, admin())).unwrap();
        assert_eq!(resp.status, 400, "body {body:?} should be rejected");
    }
    app.stop();
}

#[test]
fn label_post_unknown_fingerprint_is_404() {
    let (_dir, cfg) = server_fixture("unknown_fp");
    let app = start_app(&cfg);
    let resp = app
        .handle_http(&req(
            "POST",
            &known_path("/no-such-fp/label"),
            r#"{"label":"x"}"#,
            admin(),
        ))
        .unwrap();
    assert_eq!(resp.status, 404);
    app.stop();
}

#[test]
fn label_is_visible_in_admin_list() {
    let (_dir, cfg) = server_fixture("list");
    let app = start_app(&cfg);
    app.handle_http(&req(
        "POST",
        &known_path(&format!("/{FP}/label")),
        r#"{"label":"Alice laptop"}"#,
        admin(),
    ))
    .unwrap();
    let resp = app
        .handle_http(&req("GET", &known_path(""), "", admin()))
        .unwrap();
    assert_eq!(resp.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
    assert_eq!(v["clients"][0]["label"], "Alice laptop");
    app.stop();
}

// ---------------------------------------------------------------------------
// Admin-only enforcement
// ---------------------------------------------------------------------------

#[test]
fn non_admin_gets_403_on_every_known_clients_route() {
    let (_dir, cfg) = server_fixture("authz");
    let app = start_app(&cfg);
    let before_label = disk_label(&cfg);
    let before_status = disk_status(&cfg);

    let routes: &[(&str, String, &str)] = &[
        ("GET", known_path(""), ""),
        ("POST", known_path(&format!("/{FP}/approve")), ""),
        ("POST", known_path(&format!("/{FP}/reject")), ""),
        (
            "POST",
            known_path(&format!("/{FP}/label")),
            r#"{"label":"pwned"}"#,
        ),
        ("DELETE", known_path(&format!("/{FP}")), ""),
    ];

    for (method, path, body) in routes {
        let resp = app
            .handle_http(&req(method, path, body, plain_user()))
            .expect("route handled");
        assert_eq!(
            resp.status, 403,
            "{method} {path} must be 403 for non-admin"
        );
    }

    // Nothing changed on disk.
    assert_eq!(disk_label(&cfg), before_label);
    assert_eq!(disk_status(&cfg), before_status);
    assert!(
        KnownClients::load_from_config(&cfg).get(FP).is_some(),
        "non-admin DELETE must not remove the client"
    );
    app.stop();
}

#[test]
fn non_admin_403_applies_even_for_unknown_fingerprint() {
    let (_dir, cfg) = server_fixture("authz_unknown");
    let app = start_app(&cfg);
    let resp = app
        .handle_http(&req("POST", &known_path("/nope/approve"), "", plain_user()))
        .unwrap();
    assert_eq!(resp.status, 403);
    app.stop();
}

#[test]
fn unauthenticated_request_gets_401_on_known_clients() {
    let (_dir, cfg) = server_fixture("anon");
    let app = start_app(&cfg);
    let resp = app
        .handle_http(&req("GET", &known_path(""), "", None))
        .unwrap();
    assert_eq!(resp.status, 401);
    let resp = app
        .handle_http(&req(
            "POST",
            &known_path(&format!("/{FP}/approve")),
            "",
            None,
        ))
        .unwrap();
    assert_eq!(resp.status, 401);
    assert_eq!(disk_status(&cfg), ClientStatus::Pending);
    app.stop();
}

#[test]
fn admin_can_still_approve_reject_and_delete() {
    let (_dir, cfg) = server_fixture("admin_ops");
    let app = start_app(&cfg);

    let r = app
        .handle_http(&req(
            "POST",
            &known_path(&format!("/{FP}/approve")),
            "",
            admin(),
        ))
        .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(disk_status(&cfg), ClientStatus::Allowed);

    let r = app
        .handle_http(&req(
            "POST",
            &known_path(&format!("/{FP}/reject")),
            "",
            admin(),
        ))
        .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(disk_status(&cfg), ClientStatus::Rejected);

    let r = app
        .handle_http(&req("DELETE", &known_path(&format!("/{FP}")), "", admin()))
        .unwrap();
    assert_eq!(r.status, 200);
    assert!(KnownClients::load_from_config(&cfg).get(FP).is_none());
    app.stop();
}

#[test]
fn admin_gate_does_not_cover_other_filesync_routes() {
    // Sanity: the gate is scoped to /known-clients; other filesync routes keep
    // their existing behaviour (status is open to any authenticated caller).
    let (_dir, cfg) = server_fixture("scope");
    let app = start_app(&cfg);
    let resp = app
        .handle_http(&req("GET", "/api/filesync/status", "", plain_user()))
        .unwrap();
    assert_eq!(resp.status, 200);
    app.stop();
}

// ---------------------------------------------------------------------------
// set_label persistence (KnownClients directly)
// ---------------------------------------------------------------------------

#[test]
fn set_label_persists_to_disk() {
    let dir = tmp_dir("set_label");
    let path = dir.join("config.toml");
    let mut kc = KnownClients::load_from_config(&path);
    kc.upsert_pending(NODE, FP, "10.0.0.5:4000");
    assert!(kc.set_label(FP, "Alice laptop"));

    let reloaded = KnownClients::load_from_config(&path);
    let c = reloaded.get(FP).expect("client persisted");
    assert_eq!(c.label, "Alice laptop");
    assert_eq!(c.node_id, NODE);
}

#[test]
fn set_label_unknown_fingerprint_returns_false_and_writes_nothing() {
    let dir = tmp_dir("set_label_unknown");
    let path = dir.join("config.toml");
    let mut kc = KnownClients::load_from_config(&path);
    assert!(!kc.set_label("missing", "x"));
    assert!(!path.exists());
}

// ---------------------------------------------------------------------------
// Persist data-loss regression
// ---------------------------------------------------------------------------

/// Regression: UserStore::persist used to splice into the startup snapshot, so
/// any filesync known-clients change made after startup was reverted by the
/// next user/group/API-key mutation.
#[test]
fn label_survives_user_creation() {
    let dir = tmp_dir("persist_regression");
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        "[framework]\nhttp_addr = \"127.0.0.1:9000\"\n\n\
         [[users]]\nusername = \"admin\"\npassword_hash = \"x\"\n",
    )
    .unwrap();

    // Startup: snapshot taken here, as main.rs does.
    let snapshot = std::fs::read_to_string(&path).unwrap();
    let store = UserStore::new(vec![], vec![], vec![], "", Some(path.clone()), snapshot);

    // Client is seen and labelled after startup.
    let mut kc = KnownClients::load_from_config(&path);
    kc.upsert_pending(NODE, FP, "10.0.0.5:4000");
    assert!(kc.set_label(FP, "Alice laptop"));
    assert!(kc.set_status(FP, ClientStatus::Allowed));

    // Then an admin creates a user.
    store
        .add_user(UserEntry {
            username: "bob".into(),
            password_hash: UserStore::hash_password("pw"),
            display_name: "Bob".into(),
        })
        .unwrap();

    let reloaded = KnownClients::load_from_config(&path);
    let c = reloaded.get(FP).expect("client survives user creation");
    assert_eq!(c.label, "Alice laptop", "label must survive user creation");
    assert_eq!(c.status, ClientStatus::Allowed, "approval must survive");

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("username = \"bob\""), "new user written");
}

#[test]
fn user_and_client_changes_both_survive_interleaved_writes() {
    let dir = tmp_dir("persist_interleave");
    let path = dir.join("config.toml");
    std::fs::write(&path, "[framework]\nhttp_addr = \"127.0.0.1:9000\"\n").unwrap();
    let snapshot = std::fs::read_to_string(&path).unwrap();
    let store = UserStore::new(vec![], vec![], vec![], "", Some(path.clone()), snapshot);
    let mut kc = KnownClients::load_from_config(&path);
    kc.upsert_pending(NODE, FP, "10.0.0.5:4000");

    store
        .add_user(UserEntry {
            username: "carol".into(),
            password_hash: UserStore::hash_password("pw"),
            display_name: String::new(),
        })
        .unwrap();
    kc.set_label(FP, "Carol's phone");
    store
        .add_group(bytehive_core::users::Group {
            name: "family".into(),
            description: String::new(),
            members: vec!["carol".into()],
        })
        .unwrap();
    store.create_api_key("ci", "carol", None).unwrap();
    kc.set_status(FP, ClientStatus::Allowed);

    let text = std::fs::read_to_string(&path).unwrap();
    let v: toml::Value = toml::from_str(&text).expect("file stays valid TOML");
    assert_eq!(v["framework"]["http_addr"].as_str(), Some("127.0.0.1:9000"));
    assert_eq!(v["users"][0]["username"].as_str(), Some("carol"));
    let family = v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["name"].as_str() == Some("family"))
        .expect("family group kept");
    assert_eq!(family["members"][0].as_str(), Some("carol"));
    assert_eq!(v["api_keys"][0]["name"].as_str(), Some("ci"));
    let kcs = v["filesync_known_clients"].as_array().expect("block kept");
    assert_eq!(kcs[0]["label"].as_str(), Some("Carol's phone"));
    assert_eq!(kcs[0]["status"].as_str(), Some("allowed"));
}

/// Concurrent writers from two components must not drop each other's section.
#[test]
fn concurrent_user_and_known_client_writes_do_not_clobber() {
    let dir = tmp_dir("persist_concurrent");
    let path = dir.join("config.toml");
    std::fs::write(&path, "[framework]\nhttp_addr = \"127.0.0.1:9000\"\n").unwrap();
    let snapshot = std::fs::read_to_string(&path).unwrap();
    let store = UserStore::new(vec![], vec![], vec![], "", Some(path.clone()), snapshot);
    let mut kc = KnownClients::load_from_config(&path);
    kc.upsert_pending(NODE, FP, "10.0.0.5:4000");

    let store2 = Arc::clone(&store);
    let user_thread = std::thread::spawn(move || {
        for i in 0..40 {
            store2
                .add_user(UserEntry {
                    username: format!("u{i}"),
                    password_hash: "x".into(),
                    display_name: String::new(),
                })
                .unwrap();
        }
    });
    for i in 0..40 {
        kc.set_label(FP, &format!("label-{i}"));
    }
    user_thread.join().unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    let v: toml::Value = toml::from_str(&text).expect("valid TOML after races");
    assert_eq!(v["users"].as_array().map(|a| a.len()), Some(40));
    assert_eq!(
        KnownClients::load_from_config(&path).get(FP).unwrap().label,
        "label-39"
    );
}
