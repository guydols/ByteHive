#![cfg(test)]
use bytehive_core::{FrameworkConfig, HttpRequest, HttpResponse};
use bytehive_filebrowser::{hash_password, now_ms, FileBrowserApp, Inner, Share};
use serde_json::Value;
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
    make_app_with_limit(tmp, allow_delete, 10 * 1024 * 1024)
}

fn make_app_with_limit(
    tmp: &TempDir,
    allow_delete: bool,
    max_upload_bytes: u64,
) -> std::sync::Arc<FileBrowserApp> {
    let app = FileBrowserApp::new(&make_fw_config());
    *app.inner.write() = Some(Inner {
        root: tmp.path().to_path_buf(),
        max_upload_bytes,
        allow_delete,
    });
    app
}

fn stopped_app() -> std::sync::Arc<FileBrowserApp> {
    FileBrowserApp::new(&make_fw_config())
}

fn req(method: &str, query: &str) -> HttpRequest {
    HttpRequest {
        method: method.to_string(),
        path: String::new(),
        query: query.to_string(),
        headers: HashMap::new(),
        body: vec![],
        auth: None,
    }
}

fn req_body(method: &str, query: &str, body: Vec<u8>) -> HttpRequest {
    HttpRequest {
        method: method.to_string(),
        path: String::new(),
        query: query.to_string(),
        headers: HashMap::new(),
        body,
        auth: None,
    }
}

fn json_body(v: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&v).unwrap()
}

fn body_json(resp: &HttpResponse) -> Value {
    serde_json::from_slice(&resp.body).expect("response body is not valid JSON")
}

fn is_ok(r: &HttpResponse) -> bool {
    r.status == 200
}
fn is_400(r: &HttpResponse) -> bool {
    r.status == 400
}
fn is_401(r: &HttpResponse) -> bool {
    r.status == 401
}
fn is_404(r: &HttpResponse) -> bool {
    r.status == 404
}
fn is_500(r: &HttpResponse) -> bool {
    r.status == 500
}

fn write_file(tmp: &TempDir, name: &str, contents: &[u8]) {
    std::fs::write(tmp.path().join(name), contents).unwrap();
}

fn make_subdir(tmp: &TempDir, name: &str) {
    std::fs::create_dir_all(tmp.path().join(name)).unwrap();
}

#[test]
fn status_not_running() {
    let app = stopped_app();
    let resp = app.handle_status();
    assert!(is_500(&resp));
}

#[test]
fn status_running_fields() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_status();
    assert!(is_ok(&resp));
    let j = body_json(&resp);
    assert!(j["root"].is_string());
    assert_eq!(j["share_count"].as_u64().unwrap(), 0);
    assert_eq!(j["allow_delete"].as_bool().unwrap(), true);
}

#[test]
fn status_counts_only_active_shares() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "f.txt", b"hi");
    let app = make_app(&tmp, true);

    app.shares.write().insert(
        "expired".into(),
        Share {
            token: "expired".into(),
            path: "f.txt".into(),
            is_dir: false,
            name: "f.txt".into(),
            password_protected: false,
            password_hash: None,
            expires_ms: Some(1),
            created_by: "alice".into(),
            created_ms: now_ms(),
            download_count: 0,
        },
    );

    app.shares.write().insert(
        "active".into(),
        Share {
            token: "active".into(),
            path: "f.txt".into(),
            is_dir: false,
            name: "f.txt".into(),
            password_protected: false,
            password_hash: None,
            expires_ms: None,
            created_by: "alice".into(),
            created_ms: now_ms(),
            download_count: 0,
        },
    );

    let j = body_json(&app.handle_status());
    assert_eq!(j["share_count"].as_u64().unwrap(), 1);
}

#[test]
fn ls_not_running() {
    let app = stopped_app();
    let resp = app.handle_ls(&req("GET", ""));
    assert!(is_500(&resp));
}

#[test]
fn ls_root_empty_dir() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_ls(&req("GET", ""));
    assert!(is_ok(&resp));
    let j = body_json(&resp);
    assert_eq!(j["entries"].as_array().unwrap().len(), 0);
}

#[test]
fn ls_root_with_files() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "a.txt", b"");
    write_file(&tmp, "b.txt", b"");
    make_subdir(&tmp, "subdir");
    let app = make_app(&tmp, true);
    let resp = app.handle_ls(&req("GET", ""));
    assert!(is_ok(&resp));
    let j = body_json(&resp);
    let entries = j["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);

    assert_eq!(entries[0]["is_dir"].as_bool().unwrap(), true);
}

#[test]
fn ls_subdirectory() {
    let tmp = TempDir::new().unwrap();
    make_subdir(&tmp, "sub");
    write_file(&tmp, "sub/child.txt", b"");
    let app = make_app(&tmp, true);
    let resp = app.handle_ls(&req("GET", "path=sub"));
    assert!(is_ok(&resp));
    let j = body_json(&resp);
    let entries = j["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"].as_str().unwrap(), "child.txt");
    assert_eq!(entries[0]["path"].as_str().unwrap(), "sub/child.txt");
}

#[test]
fn ls_path_traversal_rejected() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_ls(&req("GET", "path=.."));
    assert!(is_400(&resp));
}

#[test]
fn ls_on_file_returns_400() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "f.txt", b"data");
    let app = make_app(&tmp, true);
    let resp = app.handle_ls(&req("GET", "path=f.txt"));
    assert!(is_400(&resp));
}

#[test]
fn ls_entry_path_includes_parent() {
    let tmp = TempDir::new().unwrap();
    make_subdir(&tmp, "p");
    write_file(&tmp, "p/x.rs", b"");
    let app = make_app(&tmp, true);
    let resp = app.handle_ls(&req("GET", "path=p"));
    let j = body_json(&resp);
    let entries = j["entries"].as_array().unwrap();
    assert_eq!(entries[0]["path"].as_str().unwrap(), "p/x.rs");
}

#[test]
fn download_not_running() {
    let app = stopped_app();
    let resp = app.handle_download(&req("GET", "path=f.txt"));
    assert!(is_500(&resp));
}

#[test]
fn download_missing_path_param() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_download(&req("GET", ""));
    assert!(is_400(&resp));
}

#[test]
fn download_file_ok() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "hello.txt", b"hello world");
    let app = make_app(&tmp, true);
    let resp = app.handle_download(&req("GET", "path=hello.txt"));
    assert!(is_ok(&resp));
    assert_eq!(resp.body, b"hello world");
    assert!(resp
        .headers
        .get("content-disposition")
        .unwrap()
        .contains("hello.txt"));
}

#[test]
fn download_sets_correct_mime() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "img.png", &[0x89, 0x50, 0x4e, 0x47]);
    let app = make_app(&tmp, true);
    let resp = app.handle_download(&req("GET", "path=img.png"));
    assert!(is_ok(&resp));
    assert_eq!(resp.content_type, "image/png");
}

#[test]
fn download_directory_returns_zip() {
    let tmp = TempDir::new().unwrap();
    make_subdir(&tmp, "mydir");
    write_file(&tmp, "mydir/inner.txt", b"content");
    let app = make_app(&tmp, true);
    let resp = app.handle_download(&req("GET", "path=mydir"));
    assert!(is_ok(&resp));
    assert_eq!(resp.content_type, "application/zip");
    assert!(resp
        .headers
        .get("content-disposition")
        .unwrap()
        .contains("mydir.zip"));
}

#[test]
fn download_traversal_rejected() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_download(&req("GET", "path=../secret"));
    assert!(is_400(&resp));
}

#[test]
fn upload_not_running() {
    let app = stopped_app();
    let resp = app.handle_upload(&req_body("POST", "name=f.txt", b"data".to_vec()));
    assert!(is_500(&resp));
}

#[test]
fn upload_missing_name() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_upload(&req_body("POST", "", b"data".to_vec()));
    assert!(is_400(&resp));
}

#[test]
fn upload_exceeds_limit() {
    let tmp = TempDir::new().unwrap();
    let app = make_app_with_limit(&tmp, true, 4);
    let resp = app.handle_upload(&req_body("POST", "name=f.txt", vec![0u8; 5]));
    assert!(is_400(&resp));
    assert!(String::from_utf8_lossy(&resp.body).contains("limit"));
}

#[test]
fn upload_invalid_name_slash() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_upload(&req_body("POST", "name=a/b.txt", b"d".to_vec()));
    assert!(is_400(&resp));
}

#[test]
fn upload_invalid_name_dotdot() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_upload(&req_body("POST", "name=..", b"d".to_vec()));
    assert!(is_400(&resp));
}

#[test]
fn upload_success_root() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_upload(&req_body("POST", "name=upload.txt", b"abc".to_vec()));
    assert!(is_ok(&resp));
    let j = body_json(&resp);
    assert_eq!(j["ok"].as_bool().unwrap(), true);
    assert!(tmp.path().join("upload.txt").exists());
}

#[test]
fn upload_success_subdir() {
    let tmp = TempDir::new().unwrap();
    make_subdir(&tmp, "sub");
    let app = make_app(&tmp, true);
    let resp = app.handle_upload(&req_body(
        "POST",
        "dir=sub&name=file.txt",
        b"content".to_vec(),
    ));
    assert!(is_ok(&resp));
    let j = body_json(&resp);
    assert_eq!(j["path"].as_str().unwrap(), "sub/file.txt");
    assert!(tmp.path().join("sub/file.txt").exists());
}

#[test]
fn upload_creates_missing_subdir() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_upload(&req_body("POST", "dir=newdir&name=file.txt", b"x".to_vec()));
    assert!(is_ok(&resp));
    assert!(tmp.path().join("newdir/file.txt").exists());
}

#[test]
fn mkdir_not_running() {
    let app = stopped_app();
    let r = req_body("POST", "", json_body(serde_json::json!({"path": "x"})));
    assert!(is_500(&app.handle_mkdir(&r)));
}

#[test]
fn mkdir_invalid_json() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body("POST", "", b"not-json".to_vec());
    assert!(is_400(&app.handle_mkdir(&r)));
}

#[test]
fn mkdir_missing_path_field() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body("POST", "", json_body(serde_json::json!({})));
    assert!(is_400(&app.handle_mkdir(&r)));
}

#[test]
fn mkdir_traversal_rejected() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"path": "../escape"})),
    );
    assert!(is_400(&app.handle_mkdir(&r)));
}

#[test]
fn mkdir_success() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"path": "newdir/sub"})),
    );
    let resp = app.handle_mkdir(&r);
    assert!(is_ok(&resp));
    assert!(tmp.path().join("newdir/sub").is_dir());
}

#[test]
fn delete_not_running() {
    let app = stopped_app();
    let resp = app.handle_delete(&req("DELETE", "path=f.txt"));
    assert!(is_500(&resp));
}

#[test]
fn delete_when_not_allowed() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "f.txt", b"x");
    let app = make_app(&tmp, false);
    let resp = app.handle_delete(&req("DELETE", "path=f.txt"));
    assert!(is_401(&resp));
    assert!(tmp.path().join("f.txt").exists());
}

#[test]
fn delete_missing_path() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_delete(&req("DELETE", ""));
    assert!(is_400(&resp));
}

#[test]
fn delete_file() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "rm.txt", b"bye");
    let app = make_app(&tmp, true);
    let resp = app.handle_delete(&req("DELETE", "path=rm.txt"));
    assert!(is_ok(&resp));
    assert!(!tmp.path().join("rm.txt").exists());
}

#[test]
fn delete_directory() {
    let tmp = TempDir::new().unwrap();
    make_subdir(&tmp, "rmdir");
    write_file(&tmp, "rmdir/child.txt", b"");
    let app = make_app(&tmp, true);
    let resp = app.handle_delete(&req("DELETE", "path=rmdir"));
    assert!(is_ok(&resp));
    assert!(!tmp.path().join("rmdir").exists());
}

#[test]
fn delete_traversal_rejected() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_delete(&req("DELETE", "path=.."));
    assert!(is_400(&resp));
}

#[test]
fn rename_not_running() {
    let app = stopped_app();
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"from": "a.txt", "to": "b.txt"})),
    );
    assert!(is_500(&app.handle_rename(&r)));
}

#[test]
fn rename_missing_from() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body("POST", "", json_body(serde_json::json!({"to": "b.txt"})));
    assert!(is_400(&app.handle_rename(&r)));
}

#[test]
fn rename_missing_to() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body("POST", "", json_body(serde_json::json!({"from": "a.txt"})));
    assert!(is_400(&app.handle_rename(&r)));
}

#[test]
fn rename_success() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "old.txt", b"data");
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"from": "old.txt", "to": "new.txt"})),
    );
    let resp = app.handle_rename(&r);
    assert!(is_ok(&resp));
    assert!(!tmp.path().join("old.txt").exists());
    assert!(tmp.path().join("new.txt").exists());
}

#[test]
fn rename_traversal_rejected() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"from": "../x", "to": "y"})),
    );
    assert!(is_400(&app.handle_rename(&r)));
}

#[test]
fn create_share_not_running() {
    let app = stopped_app();
    let r = req_body("POST", "", json_body(serde_json::json!({"path": "f.txt"})));
    assert!(is_500(&app.handle_create_share(&r, "alice")));
}

#[test]
fn create_share_missing_path() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body("POST", "", json_body(serde_json::json!({})));
    assert!(is_400(&app.handle_create_share(&r, "alice")));
}

#[test]
fn create_share_file_not_found() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"path": "ghost.txt"})),
    );
    assert!(is_404(&app.handle_create_share(&r, "alice")));
}

#[test]
fn create_share_file_success() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "share_me.txt", b"content");
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"path": "share_me.txt"})),
    );
    let resp = app.handle_create_share(&r, "alice");
    assert!(is_ok(&resp));
    let j = body_json(&resp);
    assert_eq!(j["ok"].as_bool().unwrap(), true);
    let token = j["token"].as_str().unwrap().to_string();
    assert!(!token.is_empty());
    assert_eq!(app.shares.read().get(&token).unwrap().created_by, "alice");
}

#[test]
fn create_share_with_password() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "secret.txt", b"top secret");
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"path": "secret.txt", "password": "hunter2"})),
    );
    let resp = app.handle_create_share(&r, "alice");
    assert!(is_ok(&resp));
    let j = body_json(&resp);
    let token = j["token"].as_str().unwrap().to_string();
    let share = app.shares.read().get(&token).cloned().unwrap();
    assert!(share.password_protected);
    assert!(share.password_hash.is_some());
}

#[test]
fn create_share_with_expiry() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "exp.txt", b"");
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"path": "exp.txt", "expires_hours": 1})),
    );
    let before = now_ms();
    let resp = app.handle_create_share(&r, "alice");
    assert!(is_ok(&resp));
    let j = body_json(&resp);
    let token = j["token"].as_str().unwrap().to_string();
    let share = app.shares.read().get(&token).cloned().unwrap();
    let exp = share.expires_ms.unwrap();
    assert!(exp > before);
    assert!(exp <= before + 2 * 3600 * 1000);
}

#[test]
fn create_share_url_format() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "f.txt", b"");
    let app = make_app(&tmp, true);
    let r = req_body("POST", "", json_body(serde_json::json!({"path": "f.txt"})));
    let j = body_json(&app.handle_create_share(&r, "alice"));
    let url = j["url"].as_str().unwrap();
    assert!(url.starts_with("/s/"));
}

#[test]
fn list_shares_empty() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let j = body_json(&app.handle_list_shares("user"));
    assert_eq!(j["shares"].as_array().unwrap().len(), 0);
}

#[test]
fn list_shares_excludes_expired() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "f.txt", b"");
    let app = make_app(&tmp, true);

    app.shares.write().insert(
        "tok1".into(),
        Share {
            token: "tok1".into(),
            path: "f.txt".into(),
            is_dir: false,
            name: "f.txt".into(),
            password_protected: false,
            password_hash: None,
            expires_ms: Some(1),
            created_by: "alice".into(),
            created_ms: now_ms(),
            download_count: 0,
        },
    );
    app.shares.write().insert(
        "tok2".into(),
        Share {
            token: "tok2".into(),
            path: "f.txt".into(),
            is_dir: false,
            name: "f.txt".into(),
            password_protected: false,
            password_hash: None,
            expires_ms: None,
            created_by: "alice".into(),
            created_ms: now_ms(),
            download_count: 0,
        },
    );

    let j = body_json(&app.handle_list_shares("user"));
    assert_eq!(j["shares"].as_array().unwrap().len(), 1);
}

#[test]
fn delete_share_missing_token() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_delete_share(&req("DELETE", ""), "alice", "user");
    assert!(is_400(&resp));
}

#[test]
fn delete_share_not_found() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_delete_share(&req("DELETE", "token=ghost"), "alice", "user");
    assert!(is_404(&resp));
}

#[test]
fn delete_share_unauthorized_other_user() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "f.txt", b"");
    let app = make_app(&tmp, true);
    app.shares.write().insert(
        "tok".into(),
        Share {
            token: "tok".into(),
            path: "f.txt".into(),
            is_dir: false,
            name: "f.txt".into(),
            password_protected: false,
            password_hash: None,
            expires_ms: None,
            created_by: "alice".into(),
            created_ms: now_ms(),
            download_count: 0,
        },
    );
    let resp = app.handle_delete_share(&req("DELETE", "token=tok"), "bob", "user");
    assert!(is_401(&resp));
}

#[test]
fn delete_share_own_share() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "f.txt", b"");
    let app = make_app(&tmp, true);
    app.shares.write().insert(
        "tok".into(),
        Share {
            token: "tok".into(),
            path: "f.txt".into(),
            is_dir: false,
            name: "f.txt".into(),
            password_protected: false,
            password_hash: None,
            expires_ms: None,
            created_by: "alice".into(),
            created_ms: now_ms(),
            download_count: 0,
        },
    );
    let resp = app.handle_delete_share(&req("DELETE", "token=tok"), "alice", "user");
    assert!(is_ok(&resp));
    assert!(!app.shares.read().contains_key("tok"));
}

#[test]
fn delete_share_admin_can_delete_anybodys() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "f.txt", b"");
    let app = make_app(&tmp, true);
    app.shares.write().insert(
        "tok".into(),
        Share {
            token: "tok".into(),
            path: "f.txt".into(),
            is_dir: false,
            name: "f.txt".into(),
            password_protected: false,
            password_hash: None,
            expires_ms: None,
            created_by: "alice".into(),
            created_ms: now_ms(),
            download_count: 0,
        },
    );
    let resp = app.handle_delete_share(&req("DELETE", "token=tok"), "bob", "admin");
    assert!(is_ok(&resp));
}

fn insert_share(app: &FileBrowserApp, token: &str, path: &str, password: Option<&str>) {
    app.shares.write().insert(
        token.into(),
        Share {
            token: token.into(),
            path: path.into(),
            is_dir: false,
            name: path.into(),
            password_protected: password.is_some(),
            password_hash: password.map(hash_password),
            expires_ms: None,
            created_by: "alice".into(),
            created_ms: now_ms(),
            download_count: 0,
        },
    );
}

#[test]
fn share_access_not_running() {
    let app = stopped_app();
    let resp = app.handle_share_access(&req("GET", ""), "tok");
    assert!(is_500(&resp));
}

#[test]
fn share_access_invalid_token_returns_html_error() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let resp = app.handle_share_access(&req("GET", ""), "badtoken");
    assert_eq!(resp.status, 200);
    assert!(resp.content_type.contains("text/html"));
}

#[test]
fn share_access_expired_returns_html_error() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "f.txt", b"data");
    let app = make_app(&tmp, true);
    app.shares.write().insert(
        "tok".into(),
        Share {
            token: "tok".into(),
            path: "f.txt".into(),
            is_dir: false,
            name: "f.txt".into(),
            password_protected: false,
            password_hash: None,
            expires_ms: Some(1),
            created_by: "alice".into(),
            created_ms: now_ms(),
            download_count: 0,
        },
    );
    let resp = app.handle_share_access(&req("GET", ""), "tok");
    assert!(resp.content_type.contains("text/html"));
}

#[test]
fn share_access_file_download() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "dl.txt", b"file content");
    let app = make_app(&tmp, true);
    insert_share(&app, "tok", "dl.txt", None);
    let resp = app.handle_share_access(&req("GET", ""), "tok");
    assert!(is_ok(&resp));
    assert_eq!(resp.body, b"file content");
}

#[test]
fn share_access_increments_download_count() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "dl.txt", b"x");
    let app = make_app(&tmp, true);
    insert_share(&app, "tok", "dl.txt", None);
    app.handle_share_access(&req("GET", ""), "tok");
    assert_eq!(app.shares.read().get("tok").unwrap().download_count, 1);
}

#[test]
fn share_access_password_protected_get_shows_form() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "sec.txt", b"secret");
    let app = make_app(&tmp, true);
    insert_share(&app, "tok", "sec.txt", Some("pw123"));
    let resp = app.handle_share_access(&req("GET", ""), "tok");
    assert!(resp.content_type.contains("text/html"));
    assert_eq!(resp.body.windows(4).find(|w| w == b"form").is_some(), true);
}

#[test]
fn share_access_wrong_password_shows_error_form() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "sec.txt", b"secret");
    let app = make_app(&tmp, true);
    insert_share(&app, "tok", "sec.txt", Some("pw123"));
    let post = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"password": "wrong"})),
    );
    let resp = app.handle_share_access(&post, "tok");
    assert!(resp.content_type.contains("text/html"));
}

#[test]
fn share_access_correct_password_downloads_file() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "sec.txt", b"secret content");
    let app = make_app(&tmp, true);
    insert_share(&app, "tok", "sec.txt", Some("pw123"));
    let post = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"password": "pw123"})),
    );
    let resp = app.handle_share_access(&post, "tok");
    assert!(is_ok(&resp));
    assert_eq!(resp.body, b"secret content");
}

#[test]
fn share_access_file_deleted_returns_html_error() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    insert_share(&app, "tok", "vanished.txt", None);
    let resp = app.handle_share_access(&req("GET", ""), "tok");
    assert!(resp.content_type.contains("text/html"));
}

#[test]
fn preview_not_running() {
    let app = stopped_app();
    assert!(is_500(&app.handle_preview(&req("GET", "path=f.txt"))));
}

#[test]
fn preview_missing_path() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    assert!(is_400(&app.handle_preview(&req("GET", ""))));
}

#[test]
fn preview_directory_returns_400() {
    let tmp = TempDir::new().unwrap();
    make_subdir(&tmp, "dir");
    let app = make_app(&tmp, true);
    assert!(is_400(&app.handle_preview(&req("GET", "path=dir"))));
}

#[test]
fn preview_file_returns_inline_disposition() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "img.png", &[0x89, 0x50, 0x4e, 0x47]);
    let app = make_app(&tmp, true);
    let resp = app.handle_preview(&req("GET", "path=img.png"));
    assert!(is_ok(&resp));
    assert!(resp
        .headers
        .get("content-disposition")
        .unwrap()
        .starts_with("inline"));
    assert!(resp.headers.contains_key("cache-control"));
}

#[test]
fn read_not_running() {
    let app = stopped_app();
    assert!(is_500(&app.handle_read(&req("GET", "path=f.txt"))));
}

#[test]
fn read_missing_path() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    assert!(is_400(&app.handle_read(&req("GET", ""))));
}

#[test]
fn read_directory_returns_400() {
    let tmp = TempDir::new().unwrap();
    make_subdir(&tmp, "d");
    let app = make_app(&tmp, true);
    assert!(is_400(&app.handle_read(&req("GET", "path=d"))));
}

#[test]
fn read_text_file_ok() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "hello.rs", b"fn main() {}");
    let app = make_app(&tmp, true);
    let resp = app.handle_read(&req("GET", "path=hello.rs"));
    assert!(is_ok(&resp));
    let j = body_json(&resp);
    assert_eq!(j["content"].as_str().unwrap(), "fn main() {}");
    assert_eq!(j["language"].as_str().unwrap(), "rust");
}

#[test]
fn read_binary_without_force_returns_400() {
    let tmp = TempDir::new().unwrap();

    write_file(&tmp, "bin.bin", &[0x00, 0x01, 0x02, 0x03]);
    let app = make_app(&tmp, true);
    let resp = app.handle_read(&req("GET", "path=bin.bin"));
    assert!(is_400(&resp));
}

#[test]
fn read_binary_with_force_ok() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "bin.bin", &[0x41, 0x42]);
    let app = make_app(&tmp, true);
    let resp = app.handle_read(&req("GET", "path=bin.bin&force=1"));
    assert!(is_ok(&resp));
    let j = body_json(&resp);
    assert_eq!(j["forced"].as_bool().unwrap(), true);
}

#[test]
fn read_traversal_rejected() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    assert!(is_400(&app.handle_read(&req("GET", "path=../secret"))));
}

#[test]
fn write_not_running() {
    let app = stopped_app();
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"path": "f.txt", "content": "hi"})),
    );
    assert!(is_500(&app.handle_write(&r)));
}

#[test]
fn write_invalid_json() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body("POST", "", b"!!!".to_vec());
    assert!(is_400(&app.handle_write(&r)));
}

#[test]
fn write_missing_path() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body("POST", "", json_body(serde_json::json!({"content": "hi"})));
    assert!(is_400(&app.handle_write(&r)));
}

#[test]
fn write_missing_content() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body("POST", "", json_body(serde_json::json!({"path": "f.txt"})));
    assert!(is_400(&app.handle_write(&r)));
}

#[test]
fn write_known_extension_ok() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"path": "notes.md", "content": "# hi"})),
    );
    let resp = app.handle_write(&r);
    assert!(is_ok(&resp));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("notes.md")).unwrap(),
        "# hi"
    );
}

#[test]
fn write_unknown_extension_without_force_rejected() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"path": "blob.xyz", "content": "data"})),
    );
    assert!(is_400(&app.handle_write(&r)));
}

#[test]
fn write_unknown_extension_with_force_ok() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"path": "blob.xyz", "content": "data", "force": true})),
    );
    let resp = app.handle_write(&r);
    assert!(is_ok(&resp));
}

#[test]
fn write_exceeds_limit() {
    let tmp = TempDir::new().unwrap();
    let app = make_app_with_limit(&tmp, true, 4);
    let big = "x".repeat(1000);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"path": "f.txt", "content": big})),
    );
    assert!(is_400(&app.handle_write(&r)));
}

#[test]
fn copy_not_running() {
    let app = stopped_app();
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"from": "a.txt", "to": "b.txt"})),
    );
    assert!(is_500(&app.handle_copy(&r)));
}

#[test]
fn copy_missing_from() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body("POST", "", json_body(serde_json::json!({"to": "b.txt"})));
    assert!(is_400(&app.handle_copy(&r)));
}

#[test]
fn copy_missing_to() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body("POST", "", json_body(serde_json::json!({"from": "a.txt"})));
    assert!(is_400(&app.handle_copy(&r)));
}

#[test]
fn copy_source_not_found() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"from": "missing.txt", "to": "b.txt"})),
    );
    assert!(is_404(&app.handle_copy(&r)));
}

#[test]
fn copy_destination_exists() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "a.txt", b"a");
    write_file(&tmp, "b.txt", b"b");
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"from": "a.txt", "to": "b.txt"})),
    );
    assert!(is_400(&app.handle_copy(&r)));
}

#[test]
fn copy_file_ok() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "orig.txt", b"original");
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"from": "orig.txt", "to": "copy.txt"})),
    );
    let resp = app.handle_copy(&r);
    assert!(is_ok(&resp));
    assert_eq!(
        std::fs::read(tmp.path().join("copy.txt")).unwrap(),
        b"original"
    );
    assert!(tmp.path().join("orig.txt").exists());
}

#[test]
fn copy_directory_ok() {
    let tmp = TempDir::new().unwrap();
    make_subdir(&tmp, "src_dir");
    write_file(&tmp, "src_dir/inner.txt", b"inner");
    let app = make_app(&tmp, true);
    let r = req_body(
        "POST",
        "",
        json_body(serde_json::json!({"from": "src_dir", "to": "dst_dir"})),
    );
    let resp = app.handle_copy(&r);
    assert!(is_ok(&resp));
    assert_eq!(
        std::fs::read(tmp.path().join("dst_dir/inner.txt")).unwrap(),
        b"inner"
    );
}

#[test]
fn search_not_running() {
    let app = stopped_app();
    assert!(is_500(&app.handle_search(&req("GET", "q=foo"))));
}

#[test]
fn search_missing_query() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    assert!(is_400(&app.handle_search(&req("GET", ""))));
}

#[test]
fn search_finds_matching_file() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "readme.md", b"");
    write_file(&tmp, "other.txt", b"");
    let app = make_app(&tmp, true);
    let resp = app.handle_search(&req("GET", "q=readme"));
    assert!(is_ok(&resp));
    let j = body_json(&resp);
    let results = j["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["name"].as_str().unwrap(), "readme.md");
}

#[test]
fn search_case_insensitive() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "README.md", b"");
    let app = make_app(&tmp, true);
    let resp = app.handle_search(&req("GET", "q=readme"));
    assert!(is_ok(&resp));
    let j = body_json(&resp);
    assert_eq!(j["results"].as_array().unwrap().len(), 1);
}

#[test]
fn search_finds_in_subdirectory() {
    let tmp = TempDir::new().unwrap();
    make_subdir(&tmp, "docs");
    write_file(&tmp, "docs/guide.md", b"");
    let app = make_app(&tmp, true);
    let resp = app.handle_search(&req("GET", "q=guide"));
    let j = body_json(&resp);
    assert_eq!(j["results"].as_array().unwrap().len(), 1);
}

#[test]
fn search_respects_max_results() {
    let tmp = TempDir::new().unwrap();
    for i in 0..10 {
        write_file(&tmp, &format!("file{i}.txt"), b"");
    }
    let app = make_app(&tmp, true);
    let resp = app.handle_search(&req("GET", "q=file&max_results=3"));
    let j = body_json(&resp);
    let results = j["results"].as_array().unwrap();
    assert!(results.len() <= 3);
    assert_eq!(j["truncated"].as_bool().unwrap(), true);
}

#[test]
fn detect_not_running() {
    let app = stopped_app();
    assert!(is_500(&app.handle_detect(&req("GET", "path=f.txt"))));
}

#[test]
fn detect_missing_path() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    assert!(is_400(&app.handle_detect(&req("GET", ""))));
}

#[test]
fn detect_not_found() {
    let tmp = TempDir::new().unwrap();
    let app = make_app(&tmp, true);
    assert!(is_404(&app.handle_detect(&req("GET", "path=ghost.txt"))));
}

#[test]
fn detect_known_text_extension() {
    let tmp = TempDir::new().unwrap();
    write_file(&tmp, "main.rs", b"fn main() {}");
    let app = make_app(&tmp, true);
    let j = body_json(&app.handle_detect(&req("GET", "path=main.rs")));
    assert_eq!(j["is_text"].as_bool().unwrap(), true);
    assert_eq!(j["by_extension"].as_bool().unwrap(), true);
    assert_eq!(j["language"].as_str().unwrap(), "rust");
    assert_eq!(j["is_dir"].as_bool().unwrap(), false);
}

#[test]
fn detect_directory() {
    let tmp = TempDir::new().unwrap();
    make_subdir(&tmp, "mydir");
    let app = make_app(&tmp, true);
    let j = body_json(&app.handle_detect(&req("GET", "path=mydir")));
    assert_eq!(j["is_dir"].as_bool().unwrap(), true);
    assert_eq!(j["by_content"].as_bool().unwrap(), false);
}

#[test]
fn detect_binary_file() {
    let tmp = TempDir::new().unwrap();

    write_file(&tmp, "data.bin", &[0x00; 100]);
    let app = make_app(&tmp, true);
    let j = body_json(&app.handle_detect(&req("GET", "path=data.bin")));
    assert_eq!(j["by_extension"].as_bool().unwrap(), false);
    assert_eq!(j["by_content"].as_bool().unwrap(), false);
    assert_eq!(j["is_text"].as_bool().unwrap(), false);
}
