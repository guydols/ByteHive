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
        trash_expiry_days: None,
        full_scan_interval_secs: None,
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
        trash_expiry_days: None,
        full_scan_interval_secs: None,
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
        trash_expiry_days: None,
        full_scan_interval_secs: None,
    };
    let ex = cfg.exclusions();
    assert_eq!(ex.rule_count(), 2); // 2 default rules
    assert!(!ex.is_excluded(Path::new("any_file.txt")));
}

#[test]
fn full_scan_interval_secs_is_deserialized_from_toml() {
    let toml_str = r#"
root = "/tmp"
mode = "server"
bind_addr = "0.0.0.0:7878"
full_scan_interval_secs = 86400
trash_expiry_days = 30
"#;
    let val: toml::Value = toml::from_str(toml_str).expect("TOML parse failed");
    let cfg: FileSyncConfig = val.try_into().expect("FileSyncConfig deserialize failed");
    assert_eq!(
        cfg.full_scan_interval_secs,
        Some(86400),
        "full_scan_interval_secs should be Some(86400), got {:?}",
        cfg.full_scan_interval_secs
    );
}

#[test]
fn full_config_chain_parses_full_scan_interval_secs() {
    use bytehive_core::config::FrameworkConfig;

    let toml_content = r#"
[framework]
http_addr = "0.0.0.0:9000"

[apps.filesync]
root = "/tmp"
mode = "server"
bind_addr = "0.0.0.0:7878"
full_scan_interval_secs = 86400
trash_expiry_days = 30
"#;

    let fw: FrameworkConfig = toml::from_str(toml_content).expect("FrameworkConfig parse failed");
    let app_cfg = fw.app_config("filesync");
    let fs_cfg: FileSyncConfig = app_cfg.get().expect("FileSyncConfig get() failed");

    assert_eq!(
        fs_cfg.full_scan_interval_secs,
        Some(86400),
        "full_scan_interval_secs should be Some(86400) via the full chain, got {:?}",
        fs_cfg.full_scan_interval_secs
    );
}

#[test]
fn sync_engine_respects_full_scan_interval_from_config() {
    use bytehive_core::config::FrameworkConfig;
    use bytehive_filesync::exclusions::{ExclusionConfig, Exclusions};
    use bytehive_filesync::sync_engine::{SyncEngine, SyncEngineConfig};
    use std::sync::Arc;

    let dir = std::env::temp_dir().join(format!("bh_test_{}", line!()));
    std::fs::create_dir_all(&dir).unwrap();

    let toml_content = r#"
[framework]
http_addr = "0.0.0.0:9000"

[apps.filesync]
root = "/tmp"
mode = "server"
bind_addr = "0.0.0.0:7878"
full_scan_interval_secs = 86400
"#;
    let fw: FrameworkConfig = toml::from_str(toml_content).unwrap();
    let app_cfg = fw.app_config("filesync");
    let fs_cfg: FileSyncConfig = app_cfg.get().unwrap();

    let exclusions = Arc::new(Exclusions::compile(&ExclusionConfig::default()));
    let engine = SyncEngine::new_configured(
        dir.clone(),
        "test-node".into(),
        exclusions,
        SyncEngineConfig {
            trash_expiry_days: fs_cfg.trash_expiry_days,
            full_scan_interval_secs: fs_cfg.full_scan_interval_secs,
        },
    );

    assert_eq!(
        engine.full_scan_interval_secs(),
        86400,
        "SyncEngine should use configured value 86400, got {}",
        engine.full_scan_interval_secs()
    );
}

#[test]
fn sync_engine_zero_interval_falls_back_to_default() {
    use bytehive_filesync::exclusions::{ExclusionConfig, Exclusions};
    use bytehive_filesync::protocol::FULL_SCAN_INTERVAL_SECS;
    use bytehive_filesync::sync_engine::{SyncEngine, SyncEngineConfig};
    use std::sync::Arc;

    let dir = std::env::temp_dir().join(format!("bh_test_{}", line!()));
    std::fs::create_dir_all(&dir).unwrap();

    let exclusions = Arc::new(Exclusions::compile(&ExclusionConfig::default()));
    let engine = SyncEngine::new_configured(
        dir,
        "test-node".into(),
        exclusions,
        SyncEngineConfig {
            trash_expiry_days: None,
            full_scan_interval_secs: Some(0), // 0 should fall back to the default
        },
    );

    assert_eq!(
        engine.full_scan_interval_secs(),
        FULL_SCAN_INTERVAL_SECS,
        "full_scan_interval_secs = 0 should fall back to default {}, got {}",
        FULL_SCAN_INTERVAL_SECS,
        engine.full_scan_interval_secs()
    );
}

#[test]
fn sync_engine_absent_interval_falls_back_to_default() {
    use bytehive_filesync::exclusions::{ExclusionConfig, Exclusions};
    use bytehive_filesync::protocol::FULL_SCAN_INTERVAL_SECS;
    use bytehive_filesync::sync_engine::{SyncEngine, SyncEngineConfig};
    use std::sync::Arc;

    let dir = std::env::temp_dir().join(format!("bh_test_{}", line!()));
    std::fs::create_dir_all(&dir).unwrap();

    let exclusions = Arc::new(Exclusions::compile(&ExclusionConfig::default()));
    let engine = SyncEngine::new_configured(
        dir,
        "test-node".into(),
        exclusions,
        SyncEngineConfig {
            trash_expiry_days: None,
            full_scan_interval_secs: None, // absent → use default
        },
    );

    assert_eq!(
        engine.full_scan_interval_secs(),
        FULL_SCAN_INTERVAL_SECS,
        "absent full_scan_interval_secs should fall back to default {}, got {}",
        FULL_SCAN_INTERVAL_SECS,
        engine.full_scan_interval_secs()
    );
}
