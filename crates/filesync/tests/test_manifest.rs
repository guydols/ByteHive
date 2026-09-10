use std::{collections::HashMap, path::PathBuf};

use bytehive_filesync::{
    exclusions::{ExclusionConfig, Exclusions},
    hex,
    ledger::DeletionLedger,
    manifest::{build_manifest, compute_send_list, filter_resurrected},
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
                change_sequence: 0,
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
    std::fs::create_dir_all(dir.join(".bh_filesync/transfers")).unwrap();
    std::fs::write(dir.join(".bh_filesync/transfers/partial.tmp"), b"temp").unwrap();
    std::fs::write(dir.join("real.txt"), b"real").unwrap();
    let m = build_manifest(&dir, "n", &no_exclusions()).unwrap();
    assert!(m.files.contains_key(&PathBuf::from("real.txt")));
    assert!(
        !m.files.contains_key(&PathBuf::from(".bh_filesync")),
        ".bh_filesync dir must be excluded by default rules"
    );
    assert!(
        !m.files
            .contains_key(&PathBuf::from(".bh_filesync/transfers/partial.tmp")),
        "contents of .bh_filesync must be excluded"
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

// ---- Resurrection-filter tests, relocated from src/manifest.rs ----
// (repo policy: no inline #[cfg(test)] in implementation files).

fn filter_meta(rel: &str, hash_byte: u8, mtime: u64) -> FileMetadata {
    FileMetadata {
        rel_path: PathBuf::from(rel),
        size: 4,
        hash: [hash_byte; 32],
        modified_ms: mtime,
        change_sequence: 0,
        is_dir: false,
    }
}

fn filter_manifest_with(entries: Vec<FileMetadata>) -> Manifest {
    Manifest {
        files: entries
            .into_iter()
            .map(|m| (m.rel_path.clone(), m))
            .collect(),
        node_id: "test".to_string(),
    }
}

fn filter_ledger_with(path: &str, deleted_at: u64, prev_hash: Option<String>) -> DeletionLedger {
    let mut l = DeletionLedger::new();
    l.record(
        PathBuf::from(path),
        deleted_at,
        "peer".to_string(),
        prev_hash,
        Some(50),
    );
    l
}

#[test]
fn filter_resurrected_vetoes_stale_copy() {
    let local = filter_manifest_with(vec![filter_meta("gone.txt", 0xAA, 50)]);
    let prev_hash = hex(&[0xAA; 32]);
    let ledger = filter_ledger_with("gone.txt", 100, Some(prev_hash));
    let (to_send, resurrected) =
        filter_resurrected(vec![PathBuf::from("gone.txt")], &local, Some(&ledger));
    assert!(to_send.is_empty(), "stale copy must be vetoed");
    assert!(resurrected.is_empty());
}

#[test]
fn filter_resurrected_allows_legitimate_recreate() {
    let local = filter_manifest_with(vec![filter_meta("back.txt", 0xBB, 150)]);
    let ledger = filter_ledger_with("back.txt", 100, Some(hex(&[0xAA; 32])));
    let (to_send, resurrected) =
        filter_resurrected(vec![PathBuf::from("back.txt")], &local, Some(&ledger));
    assert_eq!(to_send, vec![PathBuf::from("back.txt")]);
    assert_eq!(resurrected, vec![PathBuf::from("back.txt")]);
}

#[test]
fn filter_resurrected_passes_through_without_ledger() {
    let local = filter_manifest_with(vec![filter_meta("a.txt", 1, 10)]);
    let (to_send, resurrected) =
        filter_resurrected(vec![PathBuf::from("a.txt")], &local, None);
    assert_eq!(to_send, vec![PathBuf::from("a.txt")]);
    assert!(resurrected.is_empty());
}

#[test]
fn filter_resurrected_diverged_content_is_not_vetoed() {
    // Different hash AND newer mtime than prev => not the deleted version.
    let local = filter_manifest_with(vec![filter_meta("c.txt", 0xCC, 60)]);
    let ledger = filter_ledger_with("c.txt", 100, Some(hex(&[0xAA; 32])));
    let (to_send, _) =
        filter_resurrected(vec![PathBuf::from("c.txt")], &local, Some(&ledger));
    // mtime 60 <= prev_mtime 50? No (60 > 50) and hash differs, so no veto.
    assert_eq!(to_send, vec![PathBuf::from("c.txt")]);
}

#[test]
fn compute_send_list_unchanged_without_ledger() {
    let local = filter_manifest_with(vec![filter_meta("x.txt", 9, 200)]);
    let remote = Manifest {
        files: std::collections::HashMap::new(),
        node_id: "r".to_string(),
    };
    assert_eq!(
        compute_send_list(&local, &remote, false),
        vec![PathBuf::from("x.txt")]
    );
}
