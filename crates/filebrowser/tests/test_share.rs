use bytehive_filebrowser::{
    crypto::{hash_password, now_ms},
    share::Share,
};

fn make_share(password: Option<&str>, expires_ms: Option<u64>) -> Share {
    Share {
        token: "tok123".into(),
        path: "docs/file.txt".into(),
        is_dir: false,
        name: "file.txt".into(),
        password_protected: password.is_some(),
        password_hash: password.map(hash_password),
        expires_ms,
        created_by: "alice".into(),
        created_ms: now_ms(),
        download_count: 0,
    }
}

#[test]
fn not_expired_when_no_expiry() {
    let share = make_share(None, None);
    assert!(!share.is_expired());
    assert!(!share.is_expired_at(now_ms() + 999_999_999));
}

#[test]
fn not_expired_when_future_expiry() {
    let share = make_share(None, Some(now_ms() + 60_000));
    assert!(!share.is_expired());
}

#[test]
fn expired_when_past_expiry() {
    let share = make_share(None, Some(now_ms() - 1));
    assert!(share.is_expired());
}

#[test]
fn is_expired_at_uses_provided_time() {
    let expire_at = 1_000_000u64;
    let share = make_share(None, Some(expire_at));
    assert!(share.is_expired_at(expire_at + 1));
    assert!(!share.is_expired_at(expire_at - 1));
}

#[test]
fn check_password_no_password_always_true() {
    let share = make_share(None, None);
    assert!(share.check_password("anything"));
    assert!(share.check_password(""));
}

#[test]
fn check_password_correct_password() {
    let share = make_share(Some("s3cr3t"), None);
    assert!(share.check_password("s3cr3t"));
}

#[test]
fn check_password_wrong_password() {
    let share = make_share(Some("s3cr3t"), None);
    assert!(!share.check_password("wrong"));
    assert!(!share.check_password(""));
}

#[test]
fn share_fields_are_preserved() {
    let share = make_share(Some("pw"), Some(9999));
    assert_eq!(share.token, "tok123");
    assert_eq!(share.name, "file.txt");
    assert!(share.password_protected);
    assert_eq!(share.expires_ms, Some(9999));
}
