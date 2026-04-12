use bytehive_core::auth::Auth;
use std::collections::HashMap;

#[test]
fn auth_check_empty_token_always_true() {
    let auth = Auth::new("");
    let headers = HashMap::new();
    assert!(auth.check(&headers));
    let mut headers = HashMap::new();
    headers.insert("authorization".to_string(), "Bearer anything".to_string());
    assert!(auth.check(&headers));
}

#[test]
fn auth_check_matches_bearer_token() {
    let auth = Auth::new("secret123");
    let mut headers = HashMap::new();
    headers.insert("authorization".to_string(), "Bearer secret123".to_string());
    assert!(auth.check(&headers));
    headers.insert("authorization".to_string(), "Bearer wrong".to_string());
    assert!(!auth.check(&headers));
}

#[test]
fn auth_verify_token_constant_time() {
    let auth = Auth::new("secret");
    assert!(auth.verify_token("secret"));
    assert!(!auth.verify_token("secret2"));
    assert!(!auth.verify_token(""));
}

#[test]
fn verify_token_empty_stored_token_always_true() {
    let auth = Auth::new("");
    // when stored token is empty, any provided value must return true
    assert!(auth.verify_token("anything"));
    assert!(auth.verify_token(""));
    assert!(auth.verify_token("does-not-matter"));
}
