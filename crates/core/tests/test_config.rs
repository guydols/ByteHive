use bytehive_core::config::{AppConfig, FrameworkConfig, FrameworkSection};
use bytehive_core::error::CoreError;
use std::collections::HashMap;
use std::io::Write;
use tempfile::NamedTempFile;

#[test]
fn framework_config_load_valid_toml() {
    let content = r#"
        [framework]
        http_addr = "127.0.0.1:8080"
        http_token = "test"
        web_root = "/web"
        log_level = "debug"

        [apps.myapp]
        key = "value"
    "#;
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(content.as_bytes()).unwrap();
    let config = FrameworkConfig::load(file.path()).unwrap();
    assert_eq!(config.framework.http_addr, "127.0.0.1:8080");
    assert_eq!(config.framework.http_token, "test");
    assert_eq!(config.framework.web_root, "/web");
    assert_eq!(config.framework.log_level, "debug");
    assert!(config.apps.contains_key("myapp"));
    let app_config = config.app_config("myapp");
    let val: toml::Value = app_config.get().unwrap();
    assert_eq!(val["key"].as_str(), Some("value"));
}

#[test]
fn framework_config_load_missing_file_returns_error() {
    let path = std::path::PathBuf::from("/nonexistent/file.toml");
    let err = FrameworkConfig::load(&path).unwrap_err();
    assert!(matches!(err, CoreError::Config(_)));
}

#[test]
fn app_config_empty_returns_default() {
    let config = FrameworkConfig {
        framework: FrameworkSection::default(),
        users: vec![],
        groups: vec![],
        api_keys: vec![],
        apps: HashMap::new(),
    };
    let app_cfg = config.app_config("nonexistent");
    assert_eq!(app_cfg.raw().as_table().unwrap().len(), 0);
}

#[test]
fn app_config_empty_produces_empty_table() {
    let cfg = AppConfig::empty();
    assert!(cfg.raw().as_table().unwrap().is_empty());
}

#[test]
fn framework_config_load_invalid_toml_returns_error() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut file, b"not valid [[[toml").unwrap();
    let err = FrameworkConfig::load(file.path()).unwrap_err();
    assert!(matches!(err, CoreError::Config(_)));
}

#[test]
fn load_raw_returns_file_contents() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut file, b"[framework]\n").unwrap();
    let raw = FrameworkConfig::load_raw(file.path());
    assert_eq!(raw, "[framework]\n");
}

#[test]
fn load_raw_returns_empty_string_for_missing_file() {
    let raw = FrameworkConfig::load_raw(std::path::Path::new("/nonexistent/missing.toml"));
    assert!(raw.is_empty());
}

#[test]
fn framework_defaults_applied_when_fields_omitted() {
    let content = "[framework]\n";
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(content.as_bytes()).unwrap();
    let config = FrameworkConfig::load(file.path()).unwrap();
    assert_eq!(config.framework.http_addr, "0.0.0.0:9000");
    assert_eq!(config.framework.log_level, "info");
}

#[test]
fn app_config_get_returns_error_on_type_mismatch() {
    use serde::Deserialize;
    #[derive(Deserialize)]
    struct Typed {
        #[allow(dead_code)]
        port: u16,
    }
    let cfg = AppConfig::empty();
    let result: Result<Typed, _> = cfg.get();
    assert!(result.is_err());
}
