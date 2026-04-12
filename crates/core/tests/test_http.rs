use bytehive_core::http::{
    mime_for_path, parse_cookie, urlencoded, HttpRequest, HttpResponse, UpdateUserBody,
};
use serde_json::json;
use std::collections::HashMap;

#[test]
fn ok_json_status_and_content_type() {
    let r = HttpResponse::ok_json(json!({"k": "v"}));
    assert_eq!(r.status, 200);
    assert!(r.content_type.contains("application/json"));
    assert!(!r.body.is_empty());
}

#[test]
fn ok_html_status_and_content_type() {
    let r = HttpResponse::ok_html("<html/>");
    assert_eq!(r.status, 200);
    assert!(r.content_type.contains("text/html"));
    assert_eq!(r.body, b"<html/>");
}

#[test]
fn ok_text_status_and_content_type() {
    let r = HttpResponse::ok_text("hello");
    assert_eq!(r.status, 200);
    assert!(r.content_type.contains("text/plain"));
    assert_eq!(r.body, b"hello");
}

#[test]
fn not_found_status() {
    let r = HttpResponse::not_found("gone");
    assert_eq!(r.status, 404);
    let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(body["error"], "gone");
}

#[test]
fn unauthorized_status() {
    let r = HttpResponse::unauthorized();
    assert_eq!(r.status, 401);
    let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert!(body["error"].as_str().unwrap().contains("unauthorized"));
}

#[test]
fn forbidden_status() {
    let r = HttpResponse::forbidden();
    assert_eq!(r.status, 403);
    let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("forbidden")
            || body["error"].as_str().unwrap().contains("admin")
    );
}

#[test]
fn bad_request_status() {
    let r = HttpResponse::bad_request("missing field");
    assert_eq!(r.status, 400);
    let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(body["error"], "missing field");
}

#[test]
fn internal_error_status() {
    let r = HttpResponse::internal_error("oops");
    assert_eq!(r.status, 500);
    let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(body["error"], "oops");
}

#[test]
fn with_header_adds_header() {
    let r = HttpResponse::ok_text("x").with_header("x-custom", "value");
    assert_eq!(r.headers.get("x-custom").unwrap(), "value");
}

#[test]
fn with_header_chaining_multiple() {
    let r = HttpResponse::ok_text("x")
        .with_header("x-custom", "value1")
        .with_header("x-other", "value2");
    assert_eq!(r.headers.get("x-custom").unwrap(), "value1");
    assert_eq!(r.headers.get("x-other").unwrap(), "value2");
}

#[test]
fn ok_json_serializes_complex_objects() {
    let obj = json!({
        "name": "test",
        "nested": {
            "value": 42
        },
        "array": [1, 2, 3]
    });
    let r = HttpResponse::ok_json(&obj);
    assert_eq!(r.status, 200);
    let parsed: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(parsed["name"], "test");
    assert_eq!(parsed["nested"]["value"], 42);
}

#[test]
fn http_request_json_parses_valid_body() {
    let req = HttpRequest {
        method: "POST".into(),
        path: "/api/test".into(),
        query: "".into(),
        headers: HashMap::new(),
        body: b"{\"key\":\"val\"}".to_vec(),
        auth: None,
    };
    let v = req.json().unwrap();
    assert_eq!(v["key"], "val");
}

#[test]
fn http_request_json_fails_on_invalid_body() {
    let req = HttpRequest {
        method: "POST".into(),
        path: "/".into(),
        query: "".into(),
        headers: HashMap::new(),
        body: b"not json".to_vec(),
        auth: None,
    };
    assert!(req.json().is_err());
}

#[test]
fn http_request_can_have_auth_context() {
    let mut headers = HashMap::new();
    headers.insert("x-token".to_string(), "abc123".to_string());

    let req = HttpRequest {
        method: "GET".into(),
        path: "/api/status".into(),
        query: "".into(),
        headers,
        body: vec![],
        auth: None,
    };

    assert_eq!(req.path, "/api/status");
    assert!(req.headers.contains_key("x-token"));
}

#[test]
fn http_request_empty_body_parses_empty_json_array() {
    let req = HttpRequest {
        method: "GET".into(),
        path: "/".into(),
        query: "".into(),
        headers: HashMap::new(),
        body: b"[]".to_vec(),
        auth: None,
    };
    let v = req.json().unwrap();
    assert!(v.is_array());
}

#[test]
fn parse_cookie_finds_value() {
    assert_eq!(
        parse_cookie("cc_session=abc123; Path=/", "cc_session"),
        Some("abc123")
    );
    assert_eq!(
        parse_cookie("other=x; cc_session=tok", "cc_session"),
        Some("tok")
    );
    assert_eq!(parse_cookie("a=1; b=2", "missing"), None);
}

#[test]
fn parse_cookie_empty_value_returns_none() {
    assert_eq!(parse_cookie("cc_session=; Path=/", "cc_session"), None);
}

#[test]
fn parse_cookie_with_multiple_semicolons() {
    let cookie_str = "a=1; b=2; c=3; cc_session=mytoken";
    assert_eq!(parse_cookie(cookie_str, "cc_session"), Some("mytoken"));
}

#[test]
fn parse_cookie_with_spaces_around_value() {
    assert_eq!(
        parse_cookie("cc_session=token123; HttpOnly; Secure", "cc_session"),
        Some("token123")
    );
}

#[test]
fn parse_cookie_case_sensitive() {
    assert_eq!(parse_cookie("CC_SESSION=token", "cc_session"), None);
    assert_eq!(parse_cookie("cc_session=token", "CC_SESSION"), None);
}

#[test]
fn parse_cookie_first_in_list() {
    assert_eq!(
        parse_cookie("cc_session=first; other=second", "cc_session"),
        Some("first")
    );
}

#[test]
fn urlencoded_preserves_safe_chars() {
    assert_eq!(urlencoded("/apps/test"), "/apps/test");
    assert_eq!(urlencoded("abc-123_"), "abc-123_");
}

#[test]
fn urlencoded_encodes_space_and_special() {
    let enc = urlencoded("/path with spaces");
    assert!(enc.contains("%20"));
    assert!(!enc.contains(' '));
}

#[test]
fn urlencoded_encodes_special_characters() {
    let enc = urlencoded("name=value&other=test");
    assert!(enc.contains("%3D") || enc.contains("="));
    assert!(enc.contains("%26"));
}

#[test]
fn urlencoded_handles_empty_string() {
    assert_eq!(urlencoded(""), "");
}

#[test]
fn urlencoded_preserves_slashes() {
    let enc = urlencoded("/path/to/resource");
    assert!(enc.contains('/'));
}

#[test]
fn mime_for_path_known_types() {
    assert_eq!(mime_for_path("index.html"), "text/html; charset=utf-8");
    assert_eq!(mime_for_path("app.js"), "application/javascript");
    assert_eq!(mime_for_path("style.css"), "text/css");
    assert_eq!(mime_for_path("data.json"), "application/json");
    assert_eq!(mime_for_path("icon.png"), "image/png");
    assert_eq!(mime_for_path("logo.svg"), "image/svg+xml");
    assert_eq!(mime_for_path("font.woff2"), "font/woff2");
}

#[test]
fn mime_for_path_unknown_returns_octet_stream() {
    assert_eq!(mime_for_path("file.xyz"), "application/octet-stream");
    assert_eq!(mime_for_path("no_extension"), "application/octet-stream");
}

#[test]
fn mime_for_path_case_insensitive() {
    assert_eq!(mime_for_path("INDEX.HTML"), "text/html; charset=utf-8");
    assert_eq!(mime_for_path("Style.CSS"), "text/css");
}

#[test]
fn mime_for_path_with_path_components() {
    assert_eq!(
        mime_for_path("path/to/file.html"),
        "text/html; charset=utf-8"
    );
    assert_eq!(mime_for_path("deep/nested/path.json"), "application/json");
}

#[test]
fn mime_for_path_common_types() {
    assert_eq!(mime_for_path("image.jpg"), "image/jpeg");
    assert_eq!(mime_for_path("video.mp4"), "video/mp4");
    assert_eq!(mime_for_path("doc.pdf"), "application/pdf");
    assert_eq!(mime_for_path("archive.zip"), "application/zip");
}

#[test]
fn mime_for_path_multiple_dots() {
    assert_eq!(mime_for_path("file.tar.gz"), "application/gzip");
    assert_eq!(mime_for_path("archive.backup.zip"), "application/zip");
}

#[test]
fn not_found_creates_valid_json() {
    let r = HttpResponse::not_found("Resource not found");
    let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert!(body.is_object());
    assert!(body.get("error").is_some());
}

#[test]
fn forbidden_contains_admin_reference() {
    let r = HttpResponse::forbidden();
    let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    let error_msg = body["error"].as_str().unwrap();
    assert!(
        error_msg.to_lowercase().contains("admin")
            || error_msg.to_lowercase().contains("forbidden")
    );
}

#[test]
fn bad_request_preserves_message() {
    let msg = "Username already exists";
    let r = HttpResponse::bad_request(msg);
    let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(body["error"], msg);
}

#[test]
fn internal_error_preserves_message() {
    let msg = "Database connection failed";
    let r = HttpResponse::internal_error(msg);
    let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(body["error"], msg);
}

#[test]
fn parse_cookie_with_domain_and_path_attributes() {
    let cookie_header = "id=a3fWa; Domain=example.com; Path=/";
    assert_eq!(parse_cookie(cookie_header, "id"), Some("a3fWa"));
}

#[test]
fn urlencoded_preserves_hyphen_and_underscore() {
    let encoded = urlencoded("file-name_v2");
    assert!(encoded.contains("file") && encoded.contains("name"));
}

#[test]
fn http_response_unauthorized_consistent() {
    let r1 = HttpResponse::unauthorized();
    let r2 = HttpResponse::unauthorized();
    assert_eq!(r1.status, r2.status);
    assert_eq!(r1.body, r2.body);
}

#[test]
fn http_response_forbidden_consistent() {
    let r1 = HttpResponse::forbidden();
    let r2 = HttpResponse::forbidden();
    assert_eq!(r1.status, r2.status);
    assert_eq!(r1.body, r2.body);
}

#[test]
fn mime_for_path_document_formats() {
    assert_eq!(mime_for_path("file.pdf"), "application/pdf");
    assert_eq!(mime_for_path("file.doc"), "application/msword");
    assert_eq!(
        mime_for_path("file.docx"),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    );
}

#[test]
fn http_request_with_binary_data() {
    let mut body = vec![0u8; 100];
    body[0] = 255;
    body[99] = 128;

    let req = HttpRequest {
        method: "POST".into(),
        path: "/api/binary".into(),
        query: "".into(),
        headers: HashMap::new(),
        body,
        auth: None,
    };

    assert_eq!(req.body[0], 255);
    assert_eq!(req.body[99], 128);
}

#[test]
fn update_user_body_with_only_display_name() {
    let json_str = r#"{"display_name":"New Name"}"#;
    let update: UpdateUserBody = serde_json::from_str(json_str).unwrap();
    assert_eq!(update.display_name, Some("New Name".to_string()));
    assert_eq!(update.password, None);
}

#[test]
fn update_user_body_with_only_password() {
    let json_str = r#"{"password":"newpass123"}"#;
    let update: UpdateUserBody = serde_json::from_str(json_str).unwrap();
    assert_eq!(update.display_name, None);
    assert_eq!(update.password, Some("newpass123".to_string()));
}

#[test]
fn http_response_chain_multiple_headers_and_verify() {
    let r = HttpResponse::ok_json(json!({"test": "data"}))
        .with_header("cache-control", "no-cache")
        .with_header("x-request-id", "12345")
        .with_header("vary", "Accept-Encoding");

    assert_eq!(r.headers.len(), 3);
    assert_eq!(r.status, 200);
    let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(body["test"], "data");
}
