use bytehive_filebrowser::crypto::{constant_eq, hash_password};

#[test]
fn hash_password_returns_hex_string() {
    let h = hash_password("test");
    assert_eq!(h.len(), 64);
    assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn constant_eq_works() {
    assert!(constant_eq("abc", "abc"));
    assert!(!constant_eq("abc", "abcd"));
    assert!(!constant_eq("abc", "abd"));
    assert!(!constant_eq("", "a"));
    assert!(constant_eq("", ""));
}
