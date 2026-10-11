//! Admin gate for every mutating / triggering filesync HTTP route.
//!
//! For each mutating route:
//! * a plain user gets 403 and the on-disk / in-memory state is unchanged;
//! * an unauthenticated request gets 401 and state is unchanged;
//! * an admin succeeds (or gets the normal non-error result).
//!
//! Read-only GETs (status, manifest, trash listing) stay readable by a plain user.
//! Unmatched non-GET paths are gated too, so they cannot be used to mutate as non-admin.

use bytehive_core::app::{App, AppContext};
use bytehive_core::bus::MessageBus;
use bytehive_core::config::FrameworkConfig;
use bytehive_core::http::{HttpRequest, HttpResponse};
use bytehive_core::users::{AuthContext, AuthMethod, UserStore, GROUP_ADMIN};
use bytehive_core::TrashManager;
use bytehive_filesync::{ClientStatus, FileSyncApp, KnownClients};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FP: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const NODE: &str = "gui-admin-gate";
const BASE: &str = "/api/filesync";

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

fn body_json(resp: &HttpResponse) -> serde_json::Value {
    serde_json::from_slice(&resp.body).unwrap_or(serde_json::Value::Null)
}

fn tmp_dir(name: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("bh_admin_gate_{}_{name}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create tmp dir");
    dir
}

/// Server-mode filesync config with one pending client in the known-clients list.
fn server_fixture(name: &str) -> (PathBuf, PathBuf, PathBuf) {
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
    (dir, root, cfg_path)
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

/// Poll `/status` until `file_count` equals `want` (initial scan completes asynchronously).
fn wait_for_file_count(app: &FileSyncApp, want: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = app
            .handle_http(&req("GET", &format!("{BASE}/status"), "", plain_user()))
            .unwrap();
        if body_json(&resp)["file_count"].as_u64() == Some(want) {
            return;
        }
        assert!(Instant::now() < deadline, "file_count never reached {want}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Seed one trash entry for `rel` under `root` and return its id.
fn seed_trash(root: &Path, rel: &str) -> String {
    let full = root.join(rel);
    std::fs::write(&full, b"trashed").unwrap();
    let tm = TrashManager::new(root.to_path_buf(), None);
    tm.move_to_trash(&full, Path::new(rel), "tester");
    let entries = tm.list_trash();
    entries
        .iter()
        .find(|e| e.original_path == Path::new(rel))
        .expect("seeded trash entry")
        .id
        .clone()
}

fn trash_ids(app: &FileSyncApp) -> Vec<String> {
    let resp = app
        .handle_http(&req("GET", &format!("{BASE}/trash"), "", plain_user()))
        .unwrap();
    body_json(&resp)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect()
}

fn disk_status(cfg_path: &Path) -> String {
    KnownClients::load_from_config(cfg_path)
        .status(FP)
        .expect("client on disk")
        .as_str()
        .to_string()
}

fn disk_client_present(cfg_path: &Path) -> bool {
    KnownClients::load_from_config(cfg_path).get(FP).is_some()
}

fn assert_status(resp: &HttpResponse, want: u16) {
    assert_eq!(
        resp.status,
        want,
        "body: {}",
        String::from_utf8_lossy(&resp.body)
    );
}

// ---------------------------------------------------------------------------
// POST /rescan
// ---------------------------------------------------------------------------

#[test]
fn rescan_is_admin_only() {
    let (_d, root, cfg) = server_fixture("rescan");
    std::fs::write(root.join("a.txt"), b"a").unwrap();
    let app = start_app(&cfg);
    wait_for_file_count(&app, 1);

    // New file on disk, not yet in the manifest.
    std::fs::write(root.join("b.txt"), b"b").unwrap();

    let path = format!("{BASE}/rescan");
    assert_status(
        &app.handle_http(&req("POST", &path, "", plain_user()))
            .unwrap(),
        403,
    );
    assert_status(
        &app.handle_http(&req("POST", &path, "", None)).unwrap(),
        401,
    );
    wait_for_file_count(&app, 1); // non-admin rescan did not run

    let resp = app.handle_http(&req("POST", &path, "", admin())).unwrap();
    assert_status(&resp, 200);
    assert_eq!(body_json(&resp)["file_count"], 2);
    app.stop();
}

// ---------------------------------------------------------------------------
// Trash: restore / purge one / empty / purge-expired
// ---------------------------------------------------------------------------

#[test]
fn trash_restore_is_admin_only() {
    let (_d, root, cfg) = server_fixture("restore");
    let app = start_app(&cfg);
    wait_for_file_count(&app, 0);
    let id = seed_trash(&root, "doc.txt");
    let path = format!("{BASE}/trash/{id}/restore");

    assert_status(
        &app.handle_http(&req("POST", &path, "", plain_user()))
            .unwrap(),
        403,
    );
    assert_status(
        &app.handle_http(&req("POST", &path, "", None)).unwrap(),
        401,
    );
    assert_eq!(trash_ids(&app), vec![id.clone()], "trash entry must remain");
    assert!(!root.join("doc.txt").exists(), "file must not be restored");

    assert_status(
        &app.handle_http(&req("POST", &path, "", admin())).unwrap(),
        200,
    );
    assert!(
        root.join("doc.txt").exists(),
        "admin restore brings the file back"
    );
    assert!(trash_ids(&app).is_empty());
    app.stop();
}

#[test]
fn trash_purge_one_is_admin_only() {
    let (_d, root, cfg) = server_fixture("purge_one");
    let app = start_app(&cfg);
    wait_for_file_count(&app, 0);
    let id = seed_trash(&root, "gone.txt");
    let path = format!("{BASE}/trash/{id}");

    assert_status(
        &app.handle_http(&req("DELETE", &path, "", plain_user()))
            .unwrap(),
        403,
    );
    assert_status(
        &app.handle_http(&req("DELETE", &path, "", None)).unwrap(),
        401,
    );
    assert_eq!(trash_ids(&app), vec![id.clone()], "trash entry must remain");

    assert_status(
        &app.handle_http(&req("DELETE", &path, "", admin())).unwrap(),
        200,
    );
    assert!(trash_ids(&app).is_empty());
    app.stop();
}

#[test]
fn trash_empty_is_admin_only() {
    let (_d, root, cfg) = server_fixture("empty");
    let app = start_app(&cfg);
    wait_for_file_count(&app, 0);
    let id = seed_trash(&root, "keep.txt");
    let path = format!("{BASE}/trash");

    assert_status(
        &app.handle_http(&req("DELETE", &path, "", plain_user()))
            .unwrap(),
        403,
    );
    assert_status(
        &app.handle_http(&req("DELETE", &path, "", None)).unwrap(),
        401,
    );
    assert_eq!(trash_ids(&app), vec![id], "trash must remain");

    let resp = app.handle_http(&req("DELETE", &path, "", admin())).unwrap();
    assert_status(&resp, 200);
    assert_eq!(body_json(&resp)["purged"], 1);
    assert!(trash_ids(&app).is_empty());
    app.stop();
}

#[test]
fn trash_purge_expired_is_admin_only() {
    let (_d, root, cfg) = server_fixture("purge_expired");
    let app = start_app(&cfg);
    wait_for_file_count(&app, 0);
    let id = seed_trash(&root, "old.txt");
    let path = format!("{BASE}/trash/purge-expired");

    assert_status(
        &app.handle_http(&req("POST", &path, "", plain_user()))
            .unwrap(),
        403,
    );
    assert_status(
        &app.handle_http(&req("POST", &path, "", None)).unwrap(),
        401,
    );
    assert_eq!(trash_ids(&app), vec![id.clone()], "trash must remain");

    // No expiry configured, so admin gets a normal 200 with nothing purged.
    let resp = app.handle_http(&req("POST", &path, "", admin())).unwrap();
    assert_status(&resp, 200);
    assert_eq!(body_json(&resp)["purged"], 0);
    assert_eq!(trash_ids(&app), vec![id]);
    app.stop();
}

// ---------------------------------------------------------------------------
// Known-clients regression: mutations stay admin-only
// ---------------------------------------------------------------------------

#[test]
fn known_client_approve_and_delete_remain_admin_only() {
    let (_d, _root, cfg) = server_fixture("kc");
    let app = start_app(&cfg);
    let approve = format!("{BASE}/known-clients/{FP}/approve");
    let remove = format!("{BASE}/known-clients/{FP}");

    assert_status(
        &app.handle_http(&req("POST", &approve, "", plain_user()))
            .unwrap(),
        403,
    );
    assert_status(
        &app.handle_http(&req("POST", &approve, "", None)).unwrap(),
        401,
    );
    assert_status(
        &app.handle_http(&req("DELETE", &remove, "", plain_user()))
            .unwrap(),
        403,
    );
    assert_status(
        &app.handle_http(&req("DELETE", &remove, "", None)).unwrap(),
        401,
    );
    assert_eq!(disk_status(&cfg), "pending");
    assert!(disk_client_present(&cfg));

    assert_status(
        &app.handle_http(&req("POST", &approve, "", admin()))
            .unwrap(),
        200,
    );
    assert_eq!(disk_status(&cfg), ClientStatus::Allowed.as_str());
    app.stop();
}

// ---------------------------------------------------------------------------
// Fallthrough: unmatched non-GET paths are gated, not passed through
// ---------------------------------------------------------------------------

#[test]
fn unmatched_mutating_paths_are_gated() {
    let (_d, _root, cfg) = server_fixture("fallthrough");
    let app = start_app(&cfg);
    let paths: [(&str, String); 4] = [
        ("POST", format!("{BASE}/no-such-route")),
        ("PUT", format!("{BASE}/trash")),
        ("PATCH", format!("{BASE}/rescan")),
        ("DELETE", format!("{BASE}/trash/x/y/z")),
    ];
    for (method, path) in paths {
        assert_status(
            &app.handle_http(&req(method, &path, "", plain_user()))
                .unwrap(),
            403,
        );
        assert_status(
            &app.handle_http(&req(method, &path, "", None)).unwrap(),
            401,
        );
    }
    // Admin reaching an unknown route gets the normal "not handled" (None),
    // i.e. the host returns its usual 404.
    assert!(app
        .handle_http(&req("POST", &format!("{BASE}/no-such-route"), "", admin()))
        .is_none());
    app.stop();
}

// ---------------------------------------------------------------------------
// Read-only routes remain readable by a plain user
// ---------------------------------------------------------------------------

#[test]
fn read_only_routes_stay_open_to_plain_user() {
    let (_d, root, cfg) = server_fixture("readonly");
    std::fs::write(root.join("r.txt"), b"r").unwrap();
    let app = start_app(&cfg);
    wait_for_file_count(&app, 1);
    let id = seed_trash(&root, "listed.txt");

    for suffix in ["/status", "/manifest", "/trash"] {
        let resp = app
            .handle_http(&req("GET", &format!("{BASE}{suffix}"), "", plain_user()))
            .unwrap();
        assert_status(&resp, 200);
    }
    let trash = app
        .handle_http(&req("GET", &format!("{BASE}/trash"), "", plain_user()))
        .unwrap();
    assert_eq!(body_json(&trash)["entries"][0]["id"], id.as_str());
    app.stop();
}
