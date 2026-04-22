use bytehive_filesync::gui::config::GuiConfig;
use std::path::PathBuf;

// ─── is_complete ──────────────────────────────────────────────────────────────

#[test]
fn is_complete_all_fields_set_returns_true() {
    let cfg = GuiConfig {
        server_addr: "localhost:9000".into(),
        sync_root: PathBuf::from("/home/user/sync"),
        auth_token: "secret-token".into(),
        ..Default::default()
    };
    assert!(cfg.is_complete());
}

#[test]
fn is_complete_empty_server_addr_returns_false() {
    let cfg = GuiConfig {
        server_addr: String::new(),
        sync_root: PathBuf::from("/home/user/sync"),
        auth_token: "token".into(),
        ..Default::default()
    };
    assert!(!cfg.is_complete());
}

#[test]
fn is_complete_empty_sync_root_returns_false() {
    let cfg = GuiConfig {
        server_addr: "localhost:9000".into(),
        sync_root: PathBuf::new(),
        auth_token: "token".into(),
        ..Default::default()
    };
    assert!(!cfg.is_complete());
}

#[test]
fn is_complete_empty_auth_token_returns_false() {
    let cfg = GuiConfig {
        server_addr: "localhost:9000".into(),
        sync_root: PathBuf::from("/home/user/sync"),
        auth_token: String::new(),
        ..Default::default()
    };
    assert!(!cfg.is_complete());
}

#[test]
fn is_complete_all_empty_returns_false() {
    let cfg = GuiConfig::default();
    assert!(!cfg.is_complete());
}

#[test]
fn is_complete_only_server_addr_set_returns_false() {
    let cfg = GuiConfig {
        server_addr: "localhost:9000".into(),
        ..Default::default()
    };
    assert!(!cfg.is_complete());
}

#[test]
fn is_complete_only_sync_root_set_returns_false() {
    let cfg = GuiConfig {
        sync_root: PathBuf::from("/sync"),
        ..Default::default()
    };
    assert!(!cfg.is_complete());
}

#[test]
fn is_complete_only_auth_token_set_returns_false() {
    let cfg = GuiConfig {
        auth_token: "tok".into(),
        ..Default::default()
    };
    assert!(!cfg.is_complete());
}

#[test]
fn is_complete_server_and_root_but_no_token_returns_false() {
    let cfg = GuiConfig {
        server_addr: "host:1234".into(),
        sync_root: PathBuf::from("/sync"),
        auth_token: String::new(),
        ..Default::default()
    };
    assert!(!cfg.is_complete());
}

#[test]
fn is_complete_server_and_token_but_no_root_returns_false() {
    let cfg = GuiConfig {
        server_addr: "host:1234".into(),
        sync_root: PathBuf::new(),
        auth_token: "token".into(),
        ..Default::default()
    };
    assert!(!cfg.is_complete());
}

// ─── Default values ───────────────────────────────────────────────────────────

#[test]
fn default_has_empty_server_addr() {
    let cfg = GuiConfig::default();
    assert!(cfg.server_addr.is_empty());
}

#[test]
fn default_has_empty_sync_root() {
    let cfg = GuiConfig::default();
    assert_eq!(cfg.sync_root, PathBuf::new());
}

#[test]
fn default_has_empty_auth_token() {
    let cfg = GuiConfig::default();
    assert!(cfg.auth_token.is_empty());
}

#[test]
fn default_has_no_log_level() {
    let cfg = GuiConfig::default();
    assert!(cfg.log_level.is_none());
}

#[test]
fn default_has_empty_exclude_patterns() {
    let cfg = GuiConfig::default();
    assert!(cfg.exclude_patterns.is_empty());
}

#[test]
fn default_has_empty_exclude_regex() {
    let cfg = GuiConfig::default();
    assert!(cfg.exclude_regex.is_empty());
}

// ─── TOML round-trip ──────────────────────────────────────────────────────────

#[test]
fn toml_round_trip_preserves_server_addr() {
    let original = GuiConfig {
        server_addr: "192.168.1.100:9000".into(),
        sync_root: PathBuf::from("/data/sync"),
        auth_token: "abc123".into(),
        ..Default::default()
    };
    let toml_str = toml::to_string_pretty(&original).expect("serialization must succeed");
    let loaded: GuiConfig = toml::from_str(&toml_str).expect("deserialization must succeed");
    assert_eq!(loaded.server_addr, original.server_addr);
}

#[test]
fn toml_round_trip_preserves_sync_root() {
    let original = GuiConfig {
        server_addr: "host:1234".into(),
        sync_root: PathBuf::from("/home/alice/projects/sync"),
        auth_token: "tok".into(),
        ..Default::default()
    };
    let toml_str = toml::to_string_pretty(&original).expect("serialization must succeed");
    let loaded: GuiConfig = toml::from_str(&toml_str).expect("deserialization must succeed");
    assert_eq!(loaded.sync_root, original.sync_root);
}

#[test]
fn toml_round_trip_preserves_auth_token() {
    let original = GuiConfig {
        server_addr: "host:1234".into(),
        sync_root: PathBuf::from("/sync"),
        auth_token: "super-secret-token-xyz".into(),
        ..Default::default()
    };
    let toml_str = toml::to_string_pretty(&original).expect("serialization must succeed");
    let loaded: GuiConfig = toml::from_str(&toml_str).expect("deserialization must succeed");
    assert_eq!(loaded.auth_token, original.auth_token);
}

#[test]
fn toml_round_trip_preserves_exclude_patterns() {
    let original = GuiConfig {
        server_addr: "host:1234".into(),
        sync_root: PathBuf::from("/sync"),
        auth_token: "tok".into(),
        exclude_patterns: vec!["*.log".into(), "build/**".into(), ".git/**".into()],
        ..Default::default()
    };
    let toml_str = toml::to_string_pretty(&original).expect("serialization must succeed");
    let loaded: GuiConfig = toml::from_str(&toml_str).expect("deserialization must succeed");
    assert_eq!(loaded.exclude_patterns, original.exclude_patterns);
}

#[test]
fn toml_round_trip_preserves_exclude_regex() {
    let original = GuiConfig {
        server_addr: "host:1234".into(),
        sync_root: PathBuf::from("/sync"),
        auth_token: "tok".into(),
        exclude_regex: vec![r".*\.tmp$".into(), r"^\.".into()],
        ..Default::default()
    };
    let toml_str = toml::to_string_pretty(&original).expect("serialization must succeed");
    let loaded: GuiConfig = toml::from_str(&toml_str).expect("deserialization must succeed");
    assert_eq!(loaded.exclude_regex, original.exclude_regex);
}

#[test]
fn toml_round_trip_preserves_log_level() {
    let original = GuiConfig {
        server_addr: "host:1234".into(),
        sync_root: PathBuf::from("/sync"),
        auth_token: "tok".into(),
        log_level: Some("debug".into()),
        ..Default::default()
    };
    let toml_str = toml::to_string_pretty(&original).expect("serialization must succeed");
    let loaded: GuiConfig = toml::from_str(&toml_str).expect("deserialization must succeed");
    assert_eq!(loaded.log_level, original.log_level);
}

#[test]
fn toml_round_trip_full_config_preserves_all_fields() {
    let original = GuiConfig {
        server_addr: "10.0.0.1:9100".into(),
        sync_root: PathBuf::from("/mnt/shared"),
        auth_token: "long-auth-token-value-here".into(),
        exclude_patterns: vec!["*.log".into(), "tmp/**".into()],
        exclude_regex: vec![r"^\..+".into()],
        log_level: Some("warn".into()),
    };
    let toml_str = toml::to_string_pretty(&original).expect("serialization must succeed");
    let loaded: GuiConfig = toml::from_str(&toml_str).expect("deserialization must succeed");
    assert_eq!(loaded.server_addr, original.server_addr);
    assert_eq!(loaded.sync_root, original.sync_root);
    assert_eq!(loaded.auth_token, original.auth_token);
    assert_eq!(loaded.exclude_patterns, original.exclude_patterns);
    assert_eq!(loaded.exclude_regex, original.exclude_regex);
    assert_eq!(loaded.log_level, original.log_level);
}

#[test]
fn toml_minimal_required_fields_deserializes_successfully() {
    let toml_str = r#"
        server_addr = "localhost:9000"
        sync_root   = "/sync"
        auth_token  = "mytoken"
    "#;
    let cfg: GuiConfig = toml::from_str(toml_str).expect("must parse with only required fields");
    assert_eq!(cfg.server_addr, "localhost:9000");
    assert_eq!(cfg.sync_root, PathBuf::from("/sync"));
    assert_eq!(cfg.auth_token, "mytoken");
}

#[test]
fn toml_missing_optional_fields_default_to_empty() {
    let toml_str = r#"
        server_addr = "localhost:9000"
        sync_root   = "/sync"
        auth_token  = "tok"
    "#;
    let cfg: GuiConfig = toml::from_str(toml_str).expect("must parse");
    assert!(cfg.exclude_patterns.is_empty());
    assert!(cfg.exclude_regex.is_empty());
    assert!(cfg.log_level.is_none());
}

#[test]
fn toml_round_trip_empty_optional_vecs_stay_empty() {
    let original = GuiConfig {
        server_addr: "host:1234".into(),
        sync_root: PathBuf::from("/sync"),
        auth_token: "tok".into(),
        exclude_patterns: vec![],
        exclude_regex: vec![],
        log_level: None,
    };
    let toml_str = toml::to_string_pretty(&original).expect("serialization must succeed");
    let loaded: GuiConfig = toml::from_str(&toml_str).expect("deserialization must succeed");
    assert!(loaded.exclude_patterns.is_empty());
    assert!(loaded.exclude_regex.is_empty());
    assert!(loaded.log_level.is_none());
}

#[test]
fn toml_serialized_contains_server_addr_key() {
    let cfg = GuiConfig {
        server_addr: "myserver:1234".into(),
        sync_root: PathBuf::from("/s"),
        auth_token: "t".into(),
        ..Default::default()
    };
    let toml_str = toml::to_string_pretty(&cfg).expect("serialization must succeed");
    assert!(
        toml_str.contains("server_addr"),
        "serialized TOML must contain 'server_addr' key"
    );
}

#[test]
fn toml_serialized_contains_auth_token_key() {
    let cfg = GuiConfig {
        server_addr: "host:1234".into(),
        sync_root: PathBuf::from("/s"),
        auth_token: "mytoken".into(),
        ..Default::default()
    };
    let toml_str = toml::to_string_pretty(&cfg).expect("serialization must succeed");
    assert!(
        toml_str.contains("auth_token"),
        "serialized TOML must contain 'auth_token' key"
    );
}

// ─── Path helpers ─────────────────────────────────────────────────────────────

#[test]
fn config_dir_contains_app_name() {
    let dir = GuiConfig::config_dir();
    let dir_str = dir.to_string_lossy();
    assert!(
        dir_str.contains("bytehive-filesync"),
        "config dir '{}' should contain the app name 'bytehive-filesync'",
        dir_str
    );
}

#[test]
fn config_path_is_inside_config_dir() {
    let dir = GuiConfig::config_dir();
    let path = GuiConfig::config_path();
    assert!(
        path.starts_with(&dir),
        "config path '{}' must be inside config dir '{}'",
        path.display(),
        dir.display()
    );
}

#[test]
fn config_path_filename_is_config_toml() {
    let path = GuiConfig::config_path();
    assert_eq!(
        path.file_name().expect("config path must have a filename"),
        "config.toml"
    );
}

#[test]
fn config_path_has_toml_extension() {
    let path = GuiConfig::config_path();
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("toml"));
}

#[test]
fn config_dir_and_path_are_deterministic() {
    // Calling these multiple times returns the same result.
    assert_eq!(GuiConfig::config_dir(), GuiConfig::config_dir());
    assert_eq!(GuiConfig::config_path(), GuiConfig::config_path());
}
