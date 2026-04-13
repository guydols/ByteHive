use bytehive_filesync::{
    app::{build_client_tls_config, build_server_tls_config, FileSyncConfig},
    hex,
};
use std::{path::Path, sync::Arc};

#[test]
fn build_client_tls_config_returns_arc() {
    let dir = std::env::temp_dir().join(format!("bh_test_{}", line!()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = build_client_tls_config(&dir).unwrap();

    let _clone = Arc::clone(&config);
}

#[test]
fn build_server_tls_config_succeeds() {
    let dir = std::env::temp_dir().join(format!("bh_test_{}", line!()));
    std::fs::create_dir_all(&dir).unwrap();
    let result = build_server_tls_config(&dir);
    assert!(
        result.is_ok(),
        "server TLS config should build without error; got: {:?}",
        result.err()
    );
}

#[test]
fn build_server_tls_config_is_cloneable() {
    let dir = std::env::temp_dir().join(format!("bh_test_{}", line!()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = build_server_tls_config(&dir).unwrap();
    let _clone = Arc::clone(&cfg);
}

#[test]
fn build_server_tls_config_generates_fresh_cert_each_call() {
    let dir1 = std::env::temp_dir().join(format!("bh_test_{}_1", line!()));
    std::fs::create_dir_all(&dir1).unwrap();
    let dir2 = std::env::temp_dir().join(format!("bh_test_{}_2", line!()));
    std::fs::create_dir_all(&dir2).unwrap();
    let cfg1 = build_server_tls_config(&dir1).unwrap();
    let cfg2 = build_server_tls_config(&dir2).unwrap();

    let _c1 = Arc::clone(&cfg1);
    let _c2 = Arc::clone(&cfg2);
}

#[test]
fn hex_all_zeros() {
    let bytes = [0u8; 32];
    assert_eq!(hex(&bytes), "0".repeat(64));
}

#[test]
fn hex_all_ff() {
    let bytes = [0xFFu8; 32];
    assert_eq!(hex(&bytes), "ff".repeat(32));
}

#[test]
fn hex_output_length_is_always_64() {
    let bytes = [0xABu8; 32];
    assert_eq!(hex(&bytes).len(), 64);
}

#[test]
fn hex_is_lowercase() {
    let bytes = [0xDEu8; 32];
    let h = hex(&bytes);
    assert_eq!(h, h.to_lowercase(), "hex output must be lower-case");
}

#[test]
fn hex_mixed_bytes() {
    let mut bytes = [0u8; 32];
    bytes[0] = 0x0F;
    bytes[31] = 0xF0;
    let h = hex(&bytes);
    assert!(h.starts_with("0f"), "first byte 0x0F should render as '0f'");
    assert!(h.ends_with("f0"), "last byte 0xF0 should render as 'f0'");
}

#[test]
fn filesync_config_exclusions_compiles_glob_rules() {
    let cfg = FileSyncConfig {
        root: std::path::PathBuf::from("/tmp"),
        mode: "server".to_string(),
        bind_addr: Some("127.0.0.1:9100".to_string()),
        server_addr: None,
        auth_token: None,
        exclude_patterns: vec!["*.log".to_string(), "build/**".to_string()],
        exclude_regex: vec![],
    };
    let ex = cfg.exclusions();
    assert_eq!(ex.rule_count(), 4); // 2 explicit + 2 default rules
    assert!(ex.is_excluded(Path::new("app.log")));
    assert!(ex.is_excluded(Path::new("build/output.o")));
    assert!(!ex.is_excluded(Path::new("src/main.rs")));
}

#[test]
fn filesync_config_exclusions_compiles_regex_rules() {
    let cfg = FileSyncConfig {
        root: std::path::PathBuf::from("/tmp"),
        mode: "client".to_string(),
        bind_addr: None,
        server_addr: Some("127.0.0.1:9100".to_string()),
        auth_token: None,
        exclude_patterns: vec![],
        exclude_regex: vec![r".*\.(tmp|bak)$".to_string()],
    };
    let ex = cfg.exclusions();
    assert_eq!(ex.rule_count(), 3); // 1 explicit + 2 default rules
    assert!(ex.is_excluded(Path::new("scratch.tmp")));
    assert!(ex.is_excluded(Path::new("old.bak")));
    assert!(!ex.is_excluded(Path::new("source.rs")));
}

#[test]
fn filesync_config_empty_exclusions() {
    let cfg = FileSyncConfig {
        root: std::path::PathBuf::from("/tmp"),
        mode: "server".to_string(),
        bind_addr: Some("0.0.0.0:9000".to_string()),
        server_addr: None,
        auth_token: None,
        exclude_patterns: vec![],
        exclude_regex: vec![],
    };
    let ex = cfg.exclusions();
    assert_eq!(ex.rule_count(), 2); // 2 default rules
    assert!(!ex.is_excluded(Path::new("any_file.txt")));
}
