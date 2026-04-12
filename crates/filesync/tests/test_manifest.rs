use std::{collections::HashMap, path::PathBuf};

use bytehive_filesync::{
    exclusions::{ExclusionConfig, Exclusions},
    manifest::{build_manifest, compute_send_list},
    protocol::{FileMetadata, Manifest},
};

fn no_exclusions() -> Exclusions {
    Exclusions::compile(&ExclusionConfig::default())
}

fn tmp_dir(suffix: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "filesync_manifest_{}_{suffix}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn make_manifest(entries: &[(&str, u64, [u8; 32], bool, u64)], node: &str) -> Manifest {
    let mut files = HashMap::new();
    for (path, size, hash, is_dir, modified_ms) in entries {
        let p = PathBuf::from(path);
        files.insert(
            p.clone(),
            FileMetadata {
                rel_path: p,
                size: *size,
                hash: *hash,
                modified_ms: *modified_ms,
                is_dir: *is_dir,
            },
        );
    }
    Manifest {
        files,
        node_id: node.to_string(),
    }
}

#[test]
fn build_manifest_empty_directory() {
    let dir = tmp_dir("empty");
    let m = build_manifest(&dir, "node-1", &no_exclusions()).unwrap();
    assert_eq!(m.node_id, "node-1");
    assert!(m.files.is_empty(), "empty root must produce empty manifest");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn build_manifest_single_file_hash_and_size() {
    let dir = tmp_dir("single");
    let content = b"hello filesync";
    std::fs::write(dir.join("hello.txt"), content).unwrap();

    let m = build_manifest(&dir, "n", &no_exclusions()).unwrap();
    assert_eq!(m.files.len(), 1);
    let meta = m.files.get(&PathBuf::from("hello.txt")).unwrap();
    let expected: [u8; 32] = blake3::hash(content).into();
    assert_eq!(meta.hash, expected, "BLAKE3 hash must match");
    assert_eq!(meta.size, content.len() as u64);
    assert!(!meta.is_dir);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn build_manifest_directory_entry_has_zero_hash() {
    let dir = tmp_dir("direntry");
    std::fs::create_dir_all(dir.join("subdir")).unwrap();

    let m = build_manifest(&dir, "n", &no_exclusions()).unwrap();
    let meta = m.files.get(&PathBuf::from("subdir")).unwrap();
    assert!(meta.is_dir);
    assert_eq!(meta.size, 0);
    assert_eq!(meta.hash, [0u8; 32]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn build_manifest_nested_files_and_dirs() {
    let dir = tmp_dir("nested");
    std::fs::create_dir_all(dir.join("a/b")).unwrap();
    std::fs::write(dir.join("a/b/deep.txt"), b"deep").unwrap();
    std::fs::write(dir.join("root.txt"), b"root").unwrap();

    let m = build_manifest(&dir, "n", &no_exclusions()).unwrap();
    assert!(m.files.contains_key(&PathBuf::from("a")));
    assert!(m.files.contains_key(&PathBuf::from("a/b")));
    assert!(m.files.contains_key(&PathBuf::from("a/b/deep.txt")));
    assert!(m.files.contains_key(&PathBuf::from("root.txt")));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn build_manifest_respects_glob_exclusion() {
    let dir = tmp_dir("excl_glob");
    std::fs::write(dir.join("keep.txt"), b"keep").unwrap();
    std::fs::write(dir.join("skip.log"), b"skip").unwrap();

    let excl = Exclusions::compile(&ExclusionConfig {
        exclude_patterns: vec!["*.log".to_string()],
        exclude_regex: vec![],
    });
    let m = build_manifest(&dir, "n", &excl).unwrap();
    assert!(m.files.contains_key(&PathBuf::from("keep.txt")));
    assert!(!m.files.contains_key(&PathBuf::from("skip.log")));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn build_manifest_respects_regex_exclusion() {
    let dir = tmp_dir("excl_regex");
    std::fs::write(dir.join("file.tmp"), b"tmp").unwrap();
    std::fs::write(dir.join("file.rs"), b"src").unwrap();

    let excl = Exclusions::compile(&ExclusionConfig {
        exclude_patterns: vec![],
        exclude_regex: vec![r".*\.tmp$".to_string()],
    });
    let m = build_manifest(&dir, "n", &excl).unwrap();
    assert!(!m.files.contains_key(&PathBuf::from("file.tmp")));
    assert!(m.files.contains_key(&PathBuf::from("file.rs")));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn build_manifest_node_id_is_preserved() {
    let dir = tmp_dir("nodeid");
    let m = build_manifest(&dir, "my-special-node", &no_exclusions()).unwrap();
    assert_eq!(m.node_id, "my-special-node");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn send_list_local_only_file_always_sent() {
    let h = [1u8; 32];
    let local = make_manifest(&[("new.txt", 10, h, false, 1000)], "l");
    let remote = make_manifest(&[], "r");
    let list = compute_send_list(&local, &remote, false);
    assert_eq!(list, vec![PathBuf::from("new.txt")]);
}

#[test]
fn send_list_identical_hash_not_sent() {
    let h = [1u8; 32];
    let local = make_manifest(&[("f.txt", 5, h, false, 1000)], "l");
    let remote = make_manifest(&[("f.txt", 5, h, false, 1000)], "r");
    assert!(compute_send_list(&local, &remote, false).is_empty());
}

#[test]
fn send_list_newer_local_file_sent() {
    let lh = [1u8; 32];
    let rh = [2u8; 32];
    let local = make_manifest(&[("f.txt", 5, lh, false, 2000)], "l");
    let remote = make_manifest(&[("f.txt", 5, rh, false, 1000)], "r");
    let list = compute_send_list(&local, &remote, false);
    assert_eq!(list, vec![PathBuf::from("f.txt")]);
}

#[test]
fn send_list_older_local_file_not_sent() {
    let lh = [1u8; 32];
    let rh = [2u8; 32];
    let local = make_manifest(&[("f.txt", 5, lh, false, 1000)], "l");
    let remote = make_manifest(&[("f.txt", 5, rh, false, 2000)], "r");
    assert!(compute_send_list(&local, &remote, false).is_empty());
}

#[test]
fn send_list_server_wins_on_timestamp_tie() {
    let lh = [1u8; 32];
    let rh = [2u8; 32];

    let local = make_manifest(&[("f.txt", 5, lh, false, 1000)], "l");
    let remote = make_manifest(&[("f.txt", 5, rh, false, 1000)], "r");
    let as_server = compute_send_list(&local, &remote, true);
    assert_eq!(as_server, vec![PathBuf::from("f.txt")]);
}

#[test]
fn send_list_client_does_not_win_on_timestamp_tie() {
    let lh = [1u8; 32];
    let rh = [2u8; 32];
    let local = make_manifest(&[("f.txt", 5, lh, false, 1000)], "l");
    let remote = make_manifest(&[("f.txt", 5, rh, false, 1000)], "r");

    assert!(compute_send_list(&local, &remote, false).is_empty());
}

#[test]
fn send_list_dirs_are_included_when_remote_lacks_them() {
    let h = [0u8; 32];
    let local = make_manifest(&[("subdir", 0, h, true, 500)], "l");
    let remote = make_manifest(&[], "r");
    let list = compute_send_list(&local, &remote, false);
    assert_eq!(list, vec![PathBuf::from("subdir")]);
}

#[test]
fn compute_send_list_empty_both_sides() {
    let local = make_manifest(&[], "l");
    let remote = make_manifest(&[], "r");
    assert!(compute_send_list(&local, &remote, false).is_empty());
    assert!(compute_send_list(&local, &remote, true).is_empty());
}

#[test]
fn build_manifest_excludes_filesync_tmp_dir() {
    let dir = tmp_dir("excl_tmp");
    std::fs::create_dir_all(dir.join(".filesync_tmp")).unwrap();
    std::fs::write(dir.join(".filesync_tmp/partial.tmp"), b"temp").unwrap();
    std::fs::write(dir.join("real.txt"), b"real").unwrap();
    let m = build_manifest(&dir, "n", &no_exclusions()).unwrap();
    assert!(m.files.contains_key(&PathBuf::from("real.txt")));
    assert!(
        !m.files.contains_key(&PathBuf::from(".filesync_tmp")),
        ".filesync_tmp dir must be excluded by default rules"
    );
    assert!(
        !m.files
            .contains_key(&PathBuf::from(".filesync_tmp/partial.tmp")),
        "contents of .filesync_tmp must be excluded"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn different_content_produces_different_hashes() {
    let dir = tmp_dir("diff_hash");
    std::fs::write(dir.join("a.bin"), b"content A").unwrap();
    std::fs::write(dir.join("b.bin"), b"content B").unwrap();
    let m = build_manifest(&dir, "n", &no_exclusions()).unwrap();
    let ha = m.files.get(&PathBuf::from("a.bin")).unwrap().hash;
    let hb = m.files.get(&PathBuf::from("b.bin")).unwrap().hash;
    assert_ne!(
        ha, hb,
        "different content must produce different blake3 hashes"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn send_list_multiple_files_some_sent_some_not() {
    let h1 = [1u8; 32];
    let h2 = [2u8; 32];
    let hr = [9u8; 32];
    let local = make_manifest(
        &[
            ("newer.txt", 5, h1, false, 2000),
            ("older.txt", 5, h2, false, 1000),
            ("same.txt", 5, h1, false, 1000),
        ],
        "l",
    );
    let remote = make_manifest(
        &[
            ("newer.txt", 5, hr, false, 1000),
            ("older.txt", 5, hr, false, 2000),
            ("same.txt", 5, h1, false, 1000),
        ],
        "r",
    );
    let list = compute_send_list(&local, &remote, false);
    assert!(
        list.contains(&PathBuf::from("newer.txt")),
        "newer local file with different hash must be sent"
    );
    assert!(
        !list.contains(&PathBuf::from("older.txt")),
        "older local file must not be sent"
    );
    assert!(
        !list.contains(&PathBuf::from("same.txt")),
        "identical hash file must not be sent"
    );
}
