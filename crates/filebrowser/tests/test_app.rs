#![cfg(test)]

use bytehive_core::{App, FrameworkConfig, HttpRequest, HttpResponse};
use bytehive_filebrowser::{FileBrowserApp, Inner};
use std::collections::HashMap;
use tempfile::TempDir;

fn make_fw_config() -> FrameworkConfig {
    FrameworkConfig {
        framework: Default::default(),
        users: vec![],
        groups: vec![],
        api_keys: vec![],
        apps: HashMap::new(),
    }
}

fn make_app(tmp: &TempDir, allow_delete: bool) -> std::sync::Arc<FileBrowserApp> {
    let app = FileBrowserApp::new(&make_fw_config());
    *app.inner.write() = Some(Inner {
        root: tmp.path().to_path_buf(),
        max_upload_bytes: 10 * 1024 * 1024,
        allow_delete,
    });
    app
}

fn req_authed(method: &str, path: &str, role: &str) -> HttpRequest {
    HttpRequest {
        method: method.to_string(),
        path: path.to_string(),
        query: String::new(),
        headers: {
            let mut h = HashMap::new();
            h.insert("x-bytehive-user".into(), "alice".into());
            h.insert("x-bytehive-role".into(), role.to_string());
            h
        },
        body: vec![],
        auth: None,
    }
}

fn req_anon(method: &str, path: &str) -> HttpRequest {
    HttpRequest {
        method: method.to_string(),
        path: path.to_string(),
        query: String::new(),
        headers: HashMap::new(),
        body: vec![],
        auth: None,
    }
}

fn is_401(r: &HttpResponse) -> bool {
    r.status == 401
}
fn is_ok(r: &HttpResponse) -> bool {
    r.status == 200
}
fn is_500(r: &HttpResponse) -> bool {
    r.status == 500
}

#[test]
fn new_inner_is_none_before_start() {
    let app = FileBrowserApp::new(&make_fw_config());
    assert!(app.inner.read().is_none());
}

#[test]
fn new_shares_empty() {
    let app = FileBrowserApp::new(&make_fw_config());
    assert!(app.shares.read().is_empty());
}

#[test]
fn root_returns_none_before_start() {
    let app = FileBrowserApp::new(&make_fw_config());
    assert!(app.root().is_none());
}

#[test]
fn root_returns_path_after_inner_set() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    assert_eq!(app.root().unwrap(), tmp.path());
}

#[test]
fn manifest_fields() {
    use bytehive_core::App;
    let app = FileBrowserApp::new(&make_fw_config());
    let m = app.manifest();
    assert_eq!(m.name, "filebrowser");
    assert_eq!(m.http_prefix, Some("/api/filebrowser"));
    assert_eq!(m.ui_prefix, Some("/apps/filebrowser"));
    assert!(!m.nav_label.is_empty());
}

#[test]
fn ui_route_returns_html_no_auth() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_anon("GET", "/apps/filebrowser");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_ok(&resp));
    assert!(resp.content_type.contains("text/html"));
}

#[test]
fn ui_route_subpath_also_returns_html() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_anon("GET", "/apps/filebrowser/some/sub");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_ok(&resp));
    assert!(resp.content_type.contains("text/html"));
}

#[test]
fn unauthenticated_request_to_status_returns_401() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_anon("GET", "/api/filebrowser/status");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_401(&resp));
}

#[test]
fn unauthenticated_request_to_ls_returns_401() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_anon("GET", "/api/filebrowser/ls");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_401(&resp));
}

#[test]
fn readonly_upload_returns_401() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let mut req = req_authed("POST", "/api/filebrowser/upload", "readonly");
    req.body = b"data".to_vec();
    let resp = app.handle_http(&req).unwrap();
    assert!(is_401(&resp));
}

#[test]
fn readonly_mkdir_returns_401() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_authed("POST", "/api/filebrowser/mkdir", "readonly");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_401(&resp));
}

#[test]
fn readonly_delete_returns_401() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_authed("DELETE", "/api/filebrowser/delete", "readonly");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_401(&resp));
}

#[test]
fn readonly_rename_returns_401() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_authed("POST", "/api/filebrowser/rename", "readonly");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_401(&resp));
}

#[test]
fn readonly_write_returns_401() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_authed("POST", "/api/filebrowser/write", "readonly");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_401(&resp));
}

#[test]
fn readonly_copy_returns_401() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_authed("POST", "/api/filebrowser/copy", "readonly");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_401(&resp));
}

#[test]
fn readonly_share_create_returns_401() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_authed("POST", "/api/filebrowser/share", "readonly");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_401(&resp));
}

#[test]
fn readonly_share_delete_returns_401() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_authed("DELETE", "/api/filebrowser/share", "readonly");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_401(&resp));
}

#[test]
fn readonly_can_call_status() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_authed("GET", "/api/filebrowser/status", "readonly");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_ok(&resp));
}

#[test]
fn readonly_can_list_shares() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_authed("GET", "/api/filebrowser/shares", "readonly");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_ok(&resp));
}

#[test]
fn readonly_can_search() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let mut req = req_authed("GET", "/api/filebrowser/search", "readonly");
    req.query = "q=test".to_string();
    let resp = app.handle_http(&req).unwrap();
    assert!(is_ok(&resp));
}

#[test]
fn unknown_route_returns_none() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_authed("GET", "/api/filebrowser/nonexistent", "user");
    assert!(app.handle_http(&req).is_none());
}

#[test]
fn completely_unrelated_path_returns_none() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let req = req_authed("GET", "/api/other/thing", "user");
    assert!(app.handle_http(&req).is_none());
}

#[test]
fn status_not_running_returns_500() {
    let app = FileBrowserApp::new(&make_fw_config());
    let req = req_authed("GET", "/api/filebrowser/status", "user");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_500(&resp));
}

#[test]
fn ls_not_running_returns_500() {
    let app = FileBrowserApp::new(&make_fw_config());
    let req = req_authed("GET", "/api/filebrowser/ls", "user");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_500(&resp));
}

#[test]
fn share_access_does_not_require_auth_header() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);

    let req = req_anon("GET", "/api/filebrowser/s/doesnotexist");
    let resp = app.handle_http(&req).unwrap();
    assert_ne!(resp.status, 401);
    assert!(resp.content_type.contains("text/html"));
}

#[test]
fn stop_clears_inner() {
    use bytehive_core::App;
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    assert!(app.root().is_some());
    app.stop();
    assert!(app.root().is_none());
}

#[test]
fn status_after_stop_returns_500() {
    use bytehive_core::App;
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    app.stop();
    let req = req_authed("GET", "/api/filebrowser/status", "user");
    let resp = app.handle_http(&req).unwrap();
    assert!(is_500(&resp));
}
