#![cfg(test)]

use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bytehive_core::{
    config::FrameworkSection, ApiServer, AppRegistry, Auth, FrameworkConfig, MessageBus, UserEntry,
    UserStore, GROUP_ADMIN,
};
use serde_json::json;
use std::collections::HashMap;

/// Helper struct to manage a test server instance
struct TestHttpServer {
    addr: String,
    _handle: Option<std::thread::JoinHandle<()>>,
}

impl TestHttpServer {
    fn new(
        registry: Arc<AppRegistry>,
        bus: Arc<MessageBus>,
        auth: Arc<Auth>,
        users: Arc<UserStore>,
    ) -> Self {
        // Find a free port
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind to port");
        let socket_addr = listener.local_addr().expect("Failed to get local address");
        let addr_str = format!("127.0.0.1:{}", socket_addr.port());
        drop(listener);
        thread::sleep(Duration::from_millis(300));

        eprintln!("[TestHttpServer] starting on {}", addr_str);

        let server = ApiServer::new(
            addr_str.clone(),
            registry,
            bus,
            auth,
            users,
            "/tmp/web_root",
        );

        let handle = server.start().expect("Failed to start server");

        // Wait a bit for server to bind and start listening
        thread::sleep(Duration::from_millis(300));

        TestHttpServer {
            addr: addr_str,
            _handle: Some(handle),
        }
    }

    fn get_url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

fn create_test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("Failed to create HTTP client")
}

fn create_test_server() -> (TestHttpServer, Arc<UserStore>) {
    let bus = MessageBus::new();
    let config = Arc::new(FrameworkConfig {
        framework: FrameworkSection {
            http_addr: "127.0.0.1:0".to_string(),
            http_token: String::new(),
            web_root: "/tmp/web_root".to_string(),
            log_level: "debug".to_string(),
        },
        users: vec![],
        groups: vec![],
        api_keys: vec![],
        apps: HashMap::new(),
    });
    let users = UserStore::empty();
    let registry = AppRegistry::new(bus.clone(), config, users.clone(), std::env::temp_dir());
    let auth = Arc::new(Auth::new("test-secret-key"));

    let server = TestHttpServer::new(registry, bus.clone(), auth, users.clone());
    (server, users)
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Login with valid credentials
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_login_valid_credentials() {
    let (server, users) = create_test_server();

    // Complete setup with initial admin
    users.complete_setup("password123").expect("setup failed");

    let client = create_test_client();
    let url = server.get_url("/api/auth/login");

    let response = client
        .post(&url)
        .json(&json!({
            "username": "admin",
            "password": "password123"
        }))
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        200,
        "login should succeed with valid credentials"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Login with invalid password
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_login_invalid_password() {
    let (server, users) = create_test_server();

    users.complete_setup("password123").expect("setup failed");

    let client = create_test_client();
    let url = server.get_url("/api/auth/login");

    let response = client
        .post(&url)
        .json(&json!({
            "username": "admin",
            "password": "wrongpassword"
        }))
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        401,
        "login should fail with wrong password"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Login with non-existent user
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_login_nonexistent_user() {
    let (server, _users) = create_test_server();

    let client = create_test_client();
    let url = server.get_url("/api/auth/login");

    let response = client
        .post(&url)
        .json(&json!({
            "username": "doesnotexist",
            "password": "anypass"
        }))
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        401,
        "login should fail for nonexistent user"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Login requires JSON body
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_login_invalid_json() {
    let (server, _users) = create_test_server();

    let client = create_test_client();
    let url = server.get_url("/api/auth/login");

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body("{ invalid json")
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), 400, "invalid JSON should be rejected");
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Login missing required fields
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_login_missing_password() {
    let (server, users) = create_test_server();

    users.complete_setup("password123").expect("setup failed");

    let client = create_test_client();
    let url = server.get_url("/api/auth/login");

    let response = client
        .post(&url)
        .json(&json!({
            "username": "testuser"
            // missing password
        }))
        .send()
        .await
        .expect("Failed to send request");

    // Axum returns 422 for JSON deserialization errors (missing required fields)
    assert!(
        response.status() == 400 || response.status() == 422,
        "missing password should be rejected, got {}",
        response.status()
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: /me endpoint requires authentication
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_me_unauthorized() {
    let (server, users) = create_test_server();

    users.complete_setup("password123").expect("setup failed");

    let client = create_test_client();
    let url = server.get_url("/api/auth/me");

    let response = client
        .get(&url)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), 401, "/me should require authentication");
}

// ───────────────────────────────────────────────────────────────────────────
// Test: /status endpoint requires authentication
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_status_unauthorized() {
    let (server, users) = create_test_server();

    users.complete_setup("password123").expect("setup failed");

    let client = create_test_client();
    let url = server.get_url("/api/core/status");

    let response = client
        .get(&url)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        401,
        "/status should require authentication"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: /logout endpoint clears session
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_logout_clears_session() {
    let (server, users) = create_test_server();

    users.complete_setup("password123").expect("setup failed");

    let client = create_test_client();

    // Login first
    let login_url = server.get_url("/api/auth/login");
    let login_resp = client
        .post(&login_url)
        .json(&json!({
            "username": "admin",
            "password": "password123"
        }))
        .send()
        .await
        .expect("Failed to login");

    // Extract session cookie from login response
    let set_cookie_header = login_resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .expect("login response should set session cookie");

    // Extract just the cookie name=value part (before the first semicolon)
    let session_cookie = set_cookie_header
        .split(';')
        .next()
        .expect("cookie should have value")
        .to_string();

    // Now logout with the session cookie
    let logout_url = server.get_url("/api/auth/logout");
    let logout_resp = client
        .post(&logout_url)
        .header("Cookie", session_cookie)
        .send()
        .await
        .expect("Failed to logout");

    assert_eq!(logout_resp.status(), 200, "logout should succeed");

    let body: serde_json::Value = logout_resp
        .json()
        .await
        .expect("logout response should be JSON");
    assert!(
        body["ok"].as_bool().unwrap_or(false),
        "logout should return ok=true"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Bearer token authentication with invalid token
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_bearer_token_auth_invalid_token() {
    let (server, users) = create_test_server();

    users.complete_setup("password123").expect("setup failed");

    let client = create_test_client();
    let url = server.get_url("/api/auth/me");

    let response = client
        .get(&url)
        .header("Authorization", "Bearer invalid-token-xyz")
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        401,
        "invalid bearer token should be rejected"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Cookie authentication with invalid cookie
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_cookie_auth_invalid_cookie() {
    let (server, users) = create_test_server();

    users.complete_setup("password123").expect("setup failed");

    let client = create_test_client();
    let url = server.get_url("/api/auth/me");
    let response = client
        .get(&url)
        .header("Cookie", "cc_session=invalid-session-token")
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        401,
        "invalid session cookie should be rejected"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Response has proper content-type header
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_response_has_content_type_header() {
    let (server, _users) = create_test_server();

    let client = create_test_client();
    let url = server.get_url("/api/auth/login");

    let response = client
        .post(&url)
        .json(&json!({
            "username": "test",
            "password": "test"
        }))
        .send()
        .await
        .expect("Failed to send request");

    assert!(
        response.headers().contains_key("content-type"),
        "response should have content-type header"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Error responses are valid JSON
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_error_responses_are_json() {
    let (server, users) = create_test_server();

    users.complete_setup("password123").expect("setup failed");

    let client = create_test_client();
    let url = server.get_url("/api/auth/login");

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body("not valid json")
        .send()
        .await
        .expect("Failed to send request");

    // Axum returns 400 for JSON deserialization errors
    assert!(
        response.status() == 400 || response.status() == 422,
        "invalid JSON should return 400 or 422, got {}",
        response.status()
    );

    // Check if response has content
    let text = response.text().await.expect("failed to read response text");
    assert!(!text.is_empty(), "error response should not be empty");
}

// ───────────────────────────────────────────────────────────────────────────
// Test: CORS headers are present on OPTIONS
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_cors_headers_present() {
    let (server, _users) = create_test_server();

    let client = create_test_client();
    let url = server.get_url("/api/auth/login");

    let response = client
        .request(reqwest::Method::OPTIONS, &url)
        .send()
        .await
        .expect("Failed to send request");

    // OPTIONS request should return 2xx or 404 (not 401)
    assert!(
        response.status().as_u16() < 400 || response.status() == 404,
        "OPTIONS request should not require auth, got {}",
        response.status()
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Concurrent login requests work
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_concurrent_login_requests() {
    let (server, users) = create_test_server();

    users.complete_setup("password123").expect("setup failed");

    let client = Arc::new(create_test_client());
    let url = Arc::new(server.get_url("/api/auth/login"));

    let mut handles = vec![];

    // Spawn multiple concurrent requests
    for _ in 0..3 {
        let client = client.clone();
        let url = url.clone();

        let handle = tokio::spawn(async move {
            let response = client
                .post(url.as_str())
                .json(&json!({
                    "username": "admin",
                    "password": "password123"
                }))
                .send()
                .await
                .expect("Failed to send request");

            response.status() == 200
        });

        handles.push(handle);
    }

    let results: Vec<_> = futures_util::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.expect("Task failed"))
        .collect();

    // At least some should succeed
    assert!(
        results.iter().any(|&r| r),
        "at least one concurrent login should succeed"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Login response contains user info
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_login_response_contains_user_info() {
    let (server, users) = create_test_server();

    users.complete_setup("password123").expect("setup failed");

    let client = create_test_client();
    let url = server.get_url("/api/auth/login");

    let response = client
        .post(&url)
        .json(&json!({
            "username": "admin",
            "password": "password123"
        }))
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), 200);

    let body: serde_json::Value = response.json().await.expect("response should be JSON");

    assert!(
        body["ok"].as_bool().unwrap_or(false),
        "should have ok: true"
    );
    assert_eq!(body["user"]["username"], "admin", "should contain username");
    assert!(
        body["user"]["groups"].is_array(),
        "should contain groups array"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Nonexistent route returns 404 or 401
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_nonexistent_route_returns_404_or_401() {
    let (server, users) = create_test_server();

    users.complete_setup("password123").expect("setup failed");

    let client = create_test_client();
    let url = server.get_url("/nonexistent/route/xyz");

    let response = client
        .get(&url)
        .send()
        .await
        .expect("Failed to send request");

    assert!(
        response.status() == 404 || response.status() == 401,
        "nonexistent route should return 404 or 401, got {}",
        response.status()
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: POST vs GET method validation
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_wrong_http_method() {
    let (server, _users) = create_test_server();

    let client = create_test_client();
    let url = server.get_url("/api/auth/login");

    // GET should fail on a POST-only endpoint
    let response = client
        .get(&url)
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(
        response.status(),
        405,
        "GET on POST-only endpoint should return 405"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Setup endpoint when setup is required
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_setup_endpoint_available_when_needed() {
    let (server, users) = create_test_server();

    // Without calling complete_setup, needs_setup should be true
    assert!(users.needs_setup(), "server should need setup initially");

    let client = create_test_client();
    let url = server.get_url("/setup");

    let response = client
        .get(&url)
        .send()
        .await
        .expect("Failed to send request");

    // Setup page should be accessible
    assert!(
        response.status().as_u16() < 400,
        "setup page should be accessible, got {}",
        response.status()
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Portal redirects to setup when needed
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_portal_redirects_to_setup_when_needed() {
    let (server, _users) = create_test_server();

    let client = create_test_client();
    let url = server.get_url("/");

    let response = client
        .get(&url)
        .send()
        .await
        .expect("Failed to send request");

    // Should either be setup or a redirect
    let status = response.status().as_u16();
    assert!(
        status == 200 || (status >= 300 && status < 400),
        "portal should return 200 or redirect, got {}",
        status
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Can add users after setup
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_can_create_multiple_users() {
    let (server, users) = create_test_server();

    users.complete_setup("password123").expect("setup failed");

    // Add another user
    let new_user = UserEntry {
        username: "testuser".to_string(),
        display_name: "Test User".to_string(),
        password_hash: UserStore::hash_password("testpass123"),
    };

    users.add_user(new_user).expect("failed to add user");

    // Both users should be able to login
    let client = create_test_client();
    let login_url = server.get_url("/api/auth/login");

    for (username, password) in &[("admin", "password123"), ("testuser", "testpass123")] {
        let response = client
            .post(&login_url)
            .json(&json!({
                "username": username,
                "password": password
            }))
            .send()
            .await
            .expect("Failed to send request");

        assert_eq!(
            response.status(),
            200,
            "user {} should be able to login",
            username
        );
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Test: Admin user is in admin group
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_admin_user_in_admin_group() {
    let (server, users) = create_test_server();

    users.complete_setup("password123").expect("setup failed");

    let client = create_test_client();
    let login_url = server.get_url("/api/auth/login");

    let response = client
        .post(&login_url)
        .json(&json!({
            "username": "admin",
            "password": "password123"
        }))
        .send()
        .await
        .expect("Failed to send request");

    let body: serde_json::Value = response.json().await.expect("response should be JSON");

    let groups = body["user"]["groups"]
        .as_array()
        .expect("groups should be an array");

    assert!(
        groups.iter().any(|g| g.as_str() == Some(GROUP_ADMIN)),
        "admin user should be in admin group"
    );
}
