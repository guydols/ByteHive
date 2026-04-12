use bytehive_filebrowser::file_type::{is_text_file, monaco_language, sniff_is_text};
use std::io::Write;
use tempfile::NamedTempFile;

#[test]
fn is_text_file_known_extensions() {
    assert!(is_text_file("file.rs"));
    assert!(is_text_file("script.py"));
    assert!(is_text_file("readme.md"));
    assert!(is_text_file("config.toml"));
    assert!(!is_text_file("image.png"));
    assert!(!is_text_file("binary.bin"));
}

#[test]
fn sniff_is_text_detects_plain_text() {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(b"Hello world\nThis is text").unwrap();
    let path = file.path();
    assert!(sniff_is_text(path));
}

#[test]
fn sniff_is_text_detects_binary_by_null_byte() {
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(b"Hello\x00world").unwrap();
    let path = file.path();
    assert!(!sniff_is_text(path));
}

#[test]
fn monaco_language_maps_extensions() {
    assert_eq!(monaco_language("main.rs"), "rust");
    assert_eq!(monaco_language("app.py"), "python");
    assert_eq!(monaco_language("script.js"), "javascript");
    assert_eq!(monaco_language("style.css"), "css");
    assert_eq!(monaco_language("unknown.xyz"), "plaintext");
}

#[test]
fn sniff_is_text_handles_empty_file() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path();

    assert!(sniff_is_text(path));

    std::fs::write(path, b"\x00").unwrap();
    assert!(!sniff_is_text(path));
}
