use std::{fs, path::PathBuf};

use bytehive_filebrowser::fs_util::{extension, mime_for_file, resolve, search_dir};

fn tmp_dir(label: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "fb_fsutil_{}_{label}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn resolve_valid_relative_path() {
    let root = tmp_dir("resolve_ok");
    fs::write(root.join("hello.txt"), b"hi").unwrap();
    let result = resolve(&root, "hello.txt").unwrap();
    assert_eq!(result, root.join("hello.txt"));
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn resolve_rejects_parent_dir_traversal() {
    let root = tmp_dir("resolve_trav");
    assert!(resolve(&root, "../etc/passwd").is_err());
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn resolve_strips_leading_slash() {
    let root = tmp_dir("resolve_strip");
    fs::write(root.join("f.txt"), b"").unwrap();
    let result = resolve(&root, "/f.txt").unwrap();
    assert_eq!(result, root.join("f.txt"));
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn resolve_nested_path() {
    let root = tmp_dir("resolve_nested");
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/c.txt"), b"x").unwrap();
    let result = resolve(&root, "a/b/c.txt").unwrap();
    assert_eq!(result, root.join("a/b/c.txt"));
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn extension_returns_ext() {
    assert_eq!(extension("file.rs"), "rs");
    assert_eq!(extension("archive.tar.gz"), "gz");
    assert_eq!(extension("no_ext"), "");
    assert_eq!(extension(".hidden"), "");
}

#[test]
fn mime_images() {
    assert_eq!(mime_for_file("photo.jpg"), "image/jpeg");
    assert_eq!(mime_for_file("photo.jpeg"), "image/jpeg");
    assert_eq!(mime_for_file("icon.png"), "image/png");
    assert_eq!(mime_for_file("anim.gif"), "image/gif");
    assert_eq!(mime_for_file("graphic.webp"), "image/webp");
    assert_eq!(mime_for_file("doc.pdf"), "application/pdf");
}

#[test]
fn mime_video_audio() {
    assert_eq!(mime_for_file("clip.mp4"), "video/mp4");
    assert_eq!(mime_for_file("song.mp3"), "audio/mpeg");
    assert_eq!(mime_for_file("track.flac"), "audio/flac");
}

#[test]
fn mime_code_and_text() {
    assert_eq!(mime_for_file("main.rs"), "text/plain; charset=utf-8");
    assert_eq!(mime_for_file("style.css"), "text/css; charset=utf-8");
    assert_eq!(
        mime_for_file("data.json"),
        "application/json; charset=utf-8"
    );
    assert_eq!(mime_for_file("page.html"), "text/html; charset=utf-8");
    assert_eq!(
        mime_for_file("config.toml"),
        "application/toml; charset=utf-8"
    );
}

#[test]
fn mime_archives_and_docs() {
    assert_eq!(mime_for_file("bundle.zip"), "application/zip");
    assert_eq!(
        mime_for_file("data.xlsx"),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
}

#[test]
fn mime_unknown_extension() {
    assert_eq!(mime_for_file("file.xyzzy"), "application/octet-stream");
}

#[test]
fn search_dir_finds_matching_files() {
    let root = tmp_dir("search");
    fs::write(root.join("notes.txt"), b"hello").unwrap();
    fs::write(root.join("image.png"), b"bytes").unwrap();
    let mut results = Vec::new();
    search_dir(&root, &root, "notes", 10, &mut results);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["name"], "notes.txt");
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn search_dir_is_case_insensitive() {
    let root = tmp_dir("search_case");
    fs::write(root.join("README.md"), b"").unwrap();
    let mut results = Vec::new();
    search_dir(&root, &root, "readme", 10, &mut results);
    assert_eq!(results.len(), 1);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn search_dir_respects_max_limit() {
    let root = tmp_dir("search_max");
    for i in 0..5 {
        fs::write(root.join(format!("file{i}.txt")), b"").unwrap();
    }
    let mut results = Vec::new();
    search_dir(&root, &root, "file", 3, &mut results);
    assert_eq!(results.len(), 3);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn search_dir_recurses_into_subdirectories() {
    let root = tmp_dir("search_recurse");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub/deep.txt"), b"").unwrap();
    let mut results = Vec::new();
    search_dir(&root, &root, "deep", 10, &mut results);
    assert_eq!(results.len(), 1);
    fs::remove_dir_all(&root).unwrap();
}
