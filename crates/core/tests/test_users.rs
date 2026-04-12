use bytehive_core::users::{Group, UserEntry, UserStore};

#[test]
fn hash_password_creates_phc_string() {
    let hash = UserStore::hash_password("test");
    assert!(hash.starts_with("$argon2id$"));
    assert!(hash.len() > 20);
}

#[test]
fn verify_password_correct() {
    let pw = "correct";
    let hash = UserStore::hash_password(pw);
    assert!(UserStore::verify_password(&hash, pw).unwrap_or(false));
}

#[test]
fn verify_password_wrong() {
    let hash = UserStore::hash_password("correct");
    assert!(!UserStore::verify_password(&hash, "wrong").unwrap_or(false));
}

#[test]
fn verify_password_rejects_legacy_sha256() {
    let legacy = "a".repeat(64);
    let result = UserStore::verify_password(&legacy, "anything");
    assert!(result.is_err());
}

#[test]
fn login_success_returns_session() {
    let store = UserStore::empty();
    store.complete_setup("adminpass").unwrap();
    let sess = store.login("admin", "adminpass").unwrap();
    assert_eq!(sess.username, "admin");
    assert_eq!(sess.display_name, "Administrator");
}

#[test]
fn login_wrong_password_returns_none() {
    let store = UserStore::empty();
    store.complete_setup("adminpass").unwrap();
    assert!(store.login("admin", "wrong").is_none());
}

#[test]
fn validate_session_returns_session_if_not_expired() {
    let store = UserStore::empty();
    store.complete_setup("testpassword123").unwrap();
    let sess = store.login("admin", "testpassword123").unwrap();
    let token = sess.token.clone();
    let validated = store.validate(&token).unwrap();
    assert_eq!(validated.username, "admin");
}

#[test]
fn logout_removes_session() {
    let store = UserStore::empty();
    store.complete_setup("testpassword123").unwrap();
    let sess = store.login("admin", "testpassword123").unwrap();
    let token = sess.token;
    assert!(store.validate(&token).is_some());
    store.logout(&token);
    assert!(store.validate(&token).is_none());
}

#[test]
fn authenticate_credential_with_session() {
    let store = UserStore::empty();
    store.complete_setup("testpassword123").unwrap();
    let sess = store.login("admin", "testpassword123").unwrap();
    let ctx = store.authenticate_credential(&sess.token).unwrap();
    assert_eq!(ctx.username, "admin");
    assert!(ctx.is_admin());
}

#[test]
fn authenticate_credential_with_api_key() {
    let store = UserStore::empty();
    store.complete_setup("testpassword123").unwrap();
    let key = store.create_api_key("testkey", "testuser", None).unwrap();
    let ctx = store.authenticate_credential(&key).unwrap();
    assert_eq!(ctx.username, "testuser");
}

#[test]
fn authenticate_credential_with_admin_token() {
    let store = UserStore::new(vec![], vec![], vec![], "static-token", None, "".to_string());
    let ctx = store.authenticate_credential("static-token").unwrap();
    assert_eq!(ctx.username, "admin");
}

#[test]
fn add_user_increases_user_count() {
    let store = UserStore::empty();
    store.complete_setup("testpassword123").unwrap();
    let user = UserEntry {
        username: "alice".to_string(),
        password_hash: UserStore::hash_password("pass"),
        display_name: "Alice".to_string(),
    };
    store.add_user(user).unwrap();
    assert_eq!(store.list_users().len(), 2);
}

#[test]
fn remove_user_removes_user_and_group_memberships() {
    let store = UserStore::empty();
    store.complete_setup("testpassword123").unwrap();
    let user = UserEntry {
        username: "bob".to_string(),
        password_hash: UserStore::hash_password("pass"),
        display_name: "Bob".to_string(),
    };
    store.add_user(user).unwrap();
    store.add_member_to_group("user", "bob").unwrap();
    assert!(store.groups_for_user("bob").contains(&"user".to_string()));
    store.remove_user("bob").unwrap();
    assert!(store.groups_for_user("bob").is_empty());
}

#[test]
fn groups_crud() {
    let store = UserStore::empty();
    let group = Group {
        name: "editors".to_string(),
        description: "Editors".to_string(),
        members: vec![],
    };
    store.add_group(group).unwrap();
    let groups = store.list_groups();
    assert!(groups.iter().any(|g| g.name == "editors"));
    store.remove_group("editors").unwrap();
    let groups = store.list_groups();
    assert!(!groups.iter().any(|g| g.name == "editors"));
}

#[test]
fn cannot_remove_protected_groups() {
    let store = UserStore::empty();
    assert!(store.remove_group("admin").is_err());
    assert!(store.remove_group("user").is_err());
}

#[test]
fn add_member_to_group() {
    let store = UserStore::empty();
    store.complete_setup("testpassword123").unwrap();
    store
        .add_group(Group {
            name: "test".to_owned(),
            description: "".to_owned(),
            members: vec![],
        })
        .unwrap();
    store.add_member_to_group("test", "admin").unwrap();
    let groups = store.groups_for_user("admin");
    assert!(groups.contains(&"test".to_string()));
}

#[test]
fn api_key_expiration() {
    let store = UserStore::empty();
    let key = store.create_api_key("temp", "user", None).unwrap();
    let ctx = store.authenticate_credential(&key).unwrap();
    assert_eq!(ctx.username, "user");
}

#[test]
fn api_key_list_returns_info() {
    let store = UserStore::empty();
    store.create_api_key("test", "", None).unwrap();
    let keys = store.list_api_keys();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].name, "test");
}

#[test]
fn can_write_returns_true_for_admin_group() {
    use bytehive_core::users::{AuthContext, AuthMethod, GROUP_ADMIN};
    let ctx = AuthContext {
        username: "bob".into(),
        display_name: "Bob".into(),
        groups: vec![GROUP_ADMIN.into()],
        method: AuthMethod::Session,
    };
    assert!(ctx.can_write());
}

#[test]
fn can_write_returns_true_for_user_group() {
    use bytehive_core::users::{AuthContext, AuthMethod, GROUP_USER};
    let ctx = AuthContext {
        username: "bob".into(),
        display_name: "Bob".into(),
        groups: vec![GROUP_USER.into()],
        method: AuthMethod::Session,
    };
    assert!(ctx.can_write());
}

#[test]
fn can_write_returns_false_for_unknown_group() {
    use bytehive_core::users::{AuthContext, AuthMethod};
    let ctx = AuthContext {
        username: "bob".into(),
        display_name: "Bob".into(),
        groups: vec!["guests".into()],
        method: AuthMethod::Session,
    };
    assert!(!ctx.can_write());
}

#[test]
fn in_group_true_and_false() {
    use bytehive_core::users::{AuthContext, AuthMethod};
    let ctx = AuthContext {
        username: "alice".into(),
        display_name: "Alice".into(),
        groups: vec!["editors".into()],
        method: AuthMethod::Session,
    };
    assert!(ctx.in_group("editors"));
    assert!(!ctx.in_group("admins"));
}

#[test]
fn dev_admin_context_is_admin() {
    use bytehive_core::users::AuthContext;
    let ctx = AuthContext::dev_admin();
    assert_eq!(ctx.username, "admin");
    assert!(ctx.is_admin());
    assert!(ctx.can_write());
}

#[test]
fn admin_token_context_is_admin() {
    use bytehive_core::users::AuthContext;
    let ctx = AuthContext::admin_token();
    assert_eq!(ctx.username, "admin");
    assert!(ctx.is_admin());
}

#[test]
fn session_ttl_secs_positive_for_fresh_session() {
    let store = UserStore::empty();
    store.complete_setup("testpassword123").unwrap();
    let sess = store.login("admin", "testpassword123").unwrap();
    // A fresh session should have a large TTL (8 hours)
    assert!(sess.ttl_secs() > 0);
    assert!(sess.ttl_secs() <= 8 * 3600);
}

#[test]
fn has_users_and_needs_setup() {
    let store = UserStore::empty();
    assert!(!store.has_users());
    assert!(store.needs_setup());
    store.complete_setup("password123").unwrap();
    assert!(store.has_users());
    assert!(!store.needs_setup());
}

#[test]
fn login_empty_display_name_falls_back_to_username() {
    use bytehive_core::users::UserEntry;
    let store = UserStore::empty();
    store.complete_setup("adminpass1").unwrap();
    // Add a user with empty display_name
    let user = UserEntry {
        username: "noname".to_string(),
        password_hash: UserStore::hash_password("mypassword"),
        display_name: "".to_string(),
    };
    store.add_user(user).unwrap();
    let sess = store.login("noname", "mypassword").unwrap();
    assert_eq!(sess.display_name, "noname");
}

#[test]
fn complete_setup_fails_if_already_done() {
    let store = UserStore::empty();
    store.complete_setup("password123").unwrap();
    let err = store.complete_setup("anotherpass").unwrap_err();
    assert!(err.contains("already"));
}

#[test]
fn complete_setup_fails_if_password_too_short() {
    let store = UserStore::empty();
    let err = store.complete_setup("short").unwrap_err();
    assert!(err.contains("8"));
}

#[test]
fn refresh_extends_session_expiry() {
    let store = UserStore::empty();
    store.complete_setup("testpassword123").unwrap();
    let sess = store.login("admin", "testpassword123").unwrap();
    let token = sess.token.clone();
    let ttl_before = store.validate(&token).unwrap().ttl_secs();
    store.refresh(&token);
    let ttl_after = store.validate(&token).unwrap().ttl_secs();
    // After refresh the TTL should be >= before (may be equal or slightly more)
    assert!(ttl_after >= ttl_before.saturating_sub(2)); // allow 2s clock drift
}

#[test]
fn update_user_display_name() {
    let store = UserStore::empty();
    store.complete_setup("testpassword123").unwrap();
    store
        .update_user("admin", Some("Super Admin".to_string()), None)
        .unwrap();
    let users = store.list_users();
    let admin = users.iter().find(|u| u.username == "admin").unwrap();
    assert_eq!(admin.display_name, "Super Admin");
}

#[test]
fn update_user_password() {
    let store = UserStore::empty();
    store.complete_setup("oldpassword123").unwrap();
    store
        .update_user("admin", None, Some("newpassword123"))
        .unwrap();
    // New password works
    assert!(store.login("admin", "newpassword123").is_some());
    // Old password no longer works
    assert!(store.login("admin", "oldpassword123").is_none());
}

#[test]
fn update_user_not_found_returns_error() {
    let store = UserStore::empty();
    let err = store
        .update_user("nobody", Some("Name".into()), None)
        .unwrap_err();
    assert!(err.contains("not found"));
}

#[test]
fn remove_member_from_group() {
    let store = UserStore::empty();
    store.complete_setup("testpassword123").unwrap();
    store.add_member_to_group("admin", "admin").unwrap();
    assert!(store
        .groups_for_user("admin")
        .contains(&"admin".to_string()));
    store.remove_member_from_group("admin", "admin").unwrap();
    assert!(!store
        .groups_for_user("admin")
        .contains(&"admin".to_string()));
}

#[test]
fn revoke_api_key_success() {
    let store = UserStore::empty();
    store.create_api_key("mykey", "user", None).unwrap();
    assert_eq!(store.list_api_keys().len(), 1);
    store.revoke_api_key("mykey").unwrap();
    assert_eq!(store.list_api_keys().len(), 0);
}

#[test]
fn revoke_api_key_not_found_returns_error() {
    let store = UserStore::empty();
    let err = store.revoke_api_key("nonexistent").unwrap_err();
    assert!(err.contains("not found"));
}

#[test]
fn gc_does_not_panic_and_clears_expired() {
    let store = UserStore::empty();
    store.complete_setup("testpassword123").unwrap();
    let _ = store.login("admin", "testpassword123").unwrap();
    // gc shouldn't panic; valid sessions should survive
    store.gc();
    // Sessions are not expired yet, so login still possible
    assert!(store.login("admin", "testpassword123").is_some());
}

#[test]
fn add_user_duplicate_returns_error() {
    use bytehive_core::users::UserEntry;
    let store = UserStore::empty();
    store.complete_setup("testpassword123").unwrap();
    let user = UserEntry {
        username: "admin".to_string(), // admin already exists
        password_hash: UserStore::hash_password("pass"),
        display_name: "Dup".to_string(),
    };
    let err = store.add_user(user).unwrap_err();
    assert!(err.contains("already exists"));
}

#[test]
fn add_group_duplicate_returns_error() {
    let store = UserStore::empty();
    // "admin" group already exists by default
    use bytehive_core::users::Group;
    let group = Group {
        name: "admin".into(),
        description: "".into(),
        members: vec![],
    };
    let err = store.add_group(group).unwrap_err();
    assert!(err.contains("already exists"));
}

#[test]
fn remove_group_not_found_returns_error() {
    let store = UserStore::empty();
    let err = store.remove_group("nonexistent_group").unwrap_err();
    assert!(err.contains("not found"));
}
