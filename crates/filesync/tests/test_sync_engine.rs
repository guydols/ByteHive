use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use bytehive_filesync::{
    exclusions::{ExclusionConfig, Exclusions},
    protocol::{FileBundle, FileData, FileMetadata},
    sync_engine::{safe_relative, SyncEngine},
};

fn tmp_dir(suffix: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "filesync_se_{}_{suffix}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

fn make_engine(root: PathBuf) -> SyncEngine {
    let ex = Arc::new(Exclusions::compile(&ExclusionConfig::default()));
    SyncEngine::new(root, "test-node".to_string(), ex)
}

fn file_bundle(rel: &str, content: &[u8]) -> FileBundle {
    let hash: [u8; 32] = blake3::hash(content).into();
    FileBundle {
        files: vec![FileData {
            metadata: FileMetadata {
                rel_path: PathBuf::from(rel),
                size: content.len() as u64,
                hash,
                modified_ms: 1000,
                is_dir: false,
            },
            content: content.to_vec(),
        }],
        bundle_id: 1,
    }
}

fn dir_bundle(rel: &str) -> FileBundle {
    FileBundle {
        files: vec![FileData {
            metadata: FileMetadata {
                rel_path: PathBuf::from(rel),
                size: 0,
                hash: [0u8; 32],
                modified_ms: 0,
                is_dir: true,
            },
            content: vec![],
        }],
        bundle_id: 2,
    }
}

#[test]
fn safe_relative_accepts_normal_paths() {
    assert!(safe_relative(Path::new("file.txt")));
    assert!(safe_relative(Path::new("a/b/c.rs")));
    assert!(safe_relative(Path::new("deep/nested/path/to/file")));
}

#[test]
fn safe_relative_rejects_dotdot() {
    assert!(!safe_relative(Path::new("../escape")));
    assert!(!safe_relative(Path::new("a/../../etc/passwd")));
    assert!(!safe_relative(Path::new("sub/../..")));
}

#[test]
fn safe_relative_rejects_absolute_paths() {
    assert!(!safe_relative(Path::new("/absolute/path")));
    assert!(!safe_relative(Path::new("/etc/shadow")));
}

#[test]
fn apply_bundle_writes_file_to_disk() {
    let dir = tmp_dir("write_file");
    let engine = make_engine(dir.clone());
    let n = engine
        .apply_bundle(&file_bundle("hello.txt", b"hello world"))
        .unwrap()
        .written;
    assert_eq!(n, 1);
    assert_eq!(fs::read(dir.join("hello.txt")).unwrap(), b"hello world");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_bundle_updates_manifest() {
    let dir = tmp_dir("manifest_update");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("data.bin", b"content"))
        .unwrap();
    let manifest = engine.get_manifest();
    assert!(manifest.files.contains_key(&PathBuf::from("data.bin")));
    let meta = manifest.files.get(&PathBuf::from("data.bin")).unwrap();
    assert_eq!(meta.size, 7);
    assert!(!meta.is_dir);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_bundle_creates_parent_directories() {
    let dir = tmp_dir("parent_dirs");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("a/b/c/nested.txt", b"data"))
        .unwrap();
    assert!(dir.join("a/b/c/nested.txt").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_bundle_creates_directory_entry() {
    let dir = tmp_dir("dir_entry");
    let engine = make_engine(dir.clone());
    let n = engine.apply_bundle(&dir_bundle("mydir")).unwrap().written;
    assert_eq!(n, 1);
    assert!(dir.join("mydir").is_dir());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_bundle_rejects_path_traversal() {
    let dir = tmp_dir("path_traversal");
    let engine = make_engine(dir.clone());
    let evil_bundle = FileBundle {
        files: vec![FileData {
            metadata: FileMetadata {
                rel_path: PathBuf::from("../escaped.txt"),
                size: 4,
                hash: [0u8; 32],
                modified_ms: 0,
                is_dir: false,
            },
            content: b"evil".to_vec(),
        }],
        bundle_id: 99,
    };
    let n = engine.apply_bundle(&evil_bundle).unwrap().written;
    assert_eq!(n, 0, "path traversal must be rejected");
    assert!(!dir.parent().unwrap().join("escaped.txt").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_bundle_rejects_absolute_path() {
    let dir = tmp_dir("abs_path");
    let engine = make_engine(dir.clone());
    let evil_bundle = FileBundle {
        files: vec![FileData {
            metadata: FileMetadata {
                rel_path: PathBuf::from("/tmp/evil.txt"),
                size: 4,
                hash: [0u8; 32],
                modified_ms: 0,
                is_dir: false,
            },
            content: b"evil".to_vec(),
        }],
        bundle_id: 100,
    };
    let n = engine.apply_bundle(&evil_bundle).unwrap().written;
    assert_eq!(n, 0);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_bundle_suppresses_path_immediately() {
    let dir = tmp_dir("suppress");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("watchme.txt", b"data"))
        .unwrap();

    assert!(engine.is_suppressed(Path::new("watchme.txt")));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_bundle_handles_multiple_files_in_one_bundle() {
    let dir = tmp_dir("multi");
    let engine = make_engine(dir.clone());
    let bundle = FileBundle {
        files: vec![
            FileData {
                metadata: FileMetadata {
                    rel_path: PathBuf::from("a.txt"),
                    size: 1,
                    hash: blake3::hash(b"a").into(),
                    modified_ms: 0,
                    is_dir: false,
                },
                content: b"a".to_vec(),
            },
            FileData {
                metadata: FileMetadata {
                    rel_path: PathBuf::from("b.txt"),
                    size: 1,
                    hash: blake3::hash(b"b").into(),
                    modified_ms: 0,
                    is_dir: false,
                },
                content: b"b".to_vec(),
            },
        ],
        bundle_id: 5,
    };
    let n = engine.apply_bundle(&bundle).unwrap().written;
    assert_eq!(n, 2);
    assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"a");
    assert_eq!(fs::read(dir.join("b.txt")).unwrap(), b"b");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_deletes_removes_file() {
    let dir = tmp_dir("delete_file");
    let engine = make_engine(dir.clone());

    engine
        .apply_bundle(&file_bundle("victim.txt", b"delete me"))
        .unwrap();
    assert!(dir.join("victim.txt").exists());

    let n = engine
        .apply_deletes(&[PathBuf::from("victim.txt")])
        .unwrap();
    assert_eq!(n, 1);
    assert!(!dir.join("victim.txt").exists());
    let manifest = engine.get_manifest();
    assert!(!manifest.files.contains_key(&PathBuf::from("victim.txt")));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_deletes_nonexistent_file_still_counts() {
    let dir = tmp_dir("delete_nonexistent");
    let engine = make_engine(dir.clone());

    let n = engine.apply_deletes(&[PathBuf::from("ghost.txt")]).unwrap();
    assert_eq!(n, 1);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_deletes_rejects_path_traversal() {
    let dir = tmp_dir("delete_traversal");
    let engine = make_engine(dir.clone());
    let n = engine
        .apply_deletes(&[PathBuf::from("../not_here")])
        .unwrap();
    assert_eq!(n, 0, "path traversal in delete must be silently skipped");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_deletes_removes_directory_recursively() {
    let dir = tmp_dir("delete_dir");
    let engine = make_engine(dir.clone());
    engine.apply_bundle(&dir_bundle("mydir")).unwrap();
    fs::write(dir.join("mydir/file.txt"), b"data").unwrap();
    engine.apply_deletes(&[PathBuf::from("mydir")]).unwrap();
    assert!(!dir.join("mydir").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn is_excluded_matches_glob_rule() {
    let dir = tmp_dir("excl");
    let ex = Arc::new(Exclusions::compile(&ExclusionConfig {
        exclude_patterns: vec!["*.log".to_string()],
        exclude_regex: vec![],
    }));
    let engine = SyncEngine::new(dir.clone(), "n".to_string(), ex);
    assert!(engine.is_excluded(Path::new("server.log")));
    assert!(!engine.is_excluded(Path::new("server.rs")));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn is_excluded_returns_false_with_no_rules() {
    let dir = tmp_dir("no_excl");
    let engine = make_engine(dir.clone());
    assert!(!engine.is_excluded(Path::new("anything.txt")));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_flow_happy_path() {
    let dir = tmp_dir("lf_happy");
    let engine = make_engine(dir.clone());
    let content = b"large file content for testing hash verification";
    let hash: [u8; 32] = blake3::hash(content).into();
    let rel = PathBuf::from("large.bin");
    let meta = FileMetadata {
        rel_path: rel.clone(),
        size: content.len() as u64,
        hash,
        modified_ms: 1,
        is_dir: false,
    };

    engine.begin_large_file(meta, 1).unwrap();
    engine.receive_large_file_chunk(&rel, 0, content).unwrap();
    engine.finish_large_file(&rel, hash).unwrap();

    assert!(dir.join("large.bin").exists());
    assert_eq!(fs::read(dir.join("large.bin")).unwrap(), content);
    assert!(engine.get_manifest().files.contains_key(&rel));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_finish_rejects_hash_mismatch() {
    let dir = tmp_dir("lf_hash_fail");
    let engine = make_engine(dir.clone());
    let content = b"some data";
    let correct_hash: [u8; 32] = blake3::hash(content).into();
    let wrong_hash = [0xFFu8; 32];
    let rel = PathBuf::from("bad.bin");
    let meta = FileMetadata {
        rel_path: rel.clone(),
        size: content.len() as u64,
        hash: wrong_hash,
        modified_ms: 0,
        is_dir: false,
    };

    engine.begin_large_file(meta, 1).unwrap();
    engine.receive_large_file_chunk(&rel, 0, content).unwrap();

    let result = engine.finish_large_file(&rel, wrong_hash);
    assert!(result.is_err(), "hash mismatch must return an error");
    let err = result.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_rejects_unsafe_path() {
    let dir = tmp_dir("lf_unsafe");
    let engine = make_engine(dir.clone());
    let meta = FileMetadata {
        rel_path: PathBuf::from("../outside.bin"),
        size: 0,
        hash: [0u8; 32],
        modified_ms: 0,
        is_dir: false,
    };

    let result = engine.begin_large_file(meta, 1);
    assert!(
        result.is_ok(),
        "begin_large_file should not error on unsafe path, just ignore"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn scan_populates_manifest_from_disk() {
    let dir = tmp_dir("scan");
    fs::write(dir.join("scan_me.txt"), b"content").unwrap();
    let engine = make_engine(dir.clone());
    let manifest = engine.scan().unwrap();
    assert!(manifest.files.contains_key(&PathBuf::from("scan_me.txt")));
    let meta = manifest.files.get(&PathBuf::from("scan_me.txt")).unwrap();
    let expected: [u8; 32] = blake3::hash(b"content").into();
    assert_eq!(meta.hash, expected);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_rename_moves_file_on_disk() {
    let dir = tmp_dir("rename_move");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("src.txt", b"content"))
        .unwrap();
    engine
        .apply_rename(&PathBuf::from("src.txt"), &PathBuf::from("dst.txt"))
        .unwrap();
    assert!(!dir.join("src.txt").exists());
    assert!(dir.join("dst.txt").exists());
    assert_eq!(fs::read(dir.join("dst.txt")).unwrap(), b"content");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_rename_updates_manifest() {
    let dir = tmp_dir("rename_manifest");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("before.txt", b"data"))
        .unwrap();
    engine
        .apply_rename(&PathBuf::from("before.txt"), &PathBuf::from("after.txt"))
        .unwrap();
    let manifest = engine.get_manifest();
    assert!(!manifest.files.contains_key(&PathBuf::from("before.txt")));
    assert!(manifest.files.contains_key(&PathBuf::from("after.txt")));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_rename_rejects_unsafe_src() {
    let dir = tmp_dir("rename_unsafe_src");
    let engine = make_engine(dir.clone());
    engine
        .apply_rename(&PathBuf::from("../escape.txt"), &PathBuf::from("safe.txt"))
        .unwrap();
    assert!(!dir.join("safe.txt").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_rename_rejects_unsafe_dst() {
    let dir = tmp_dir("rename_unsafe_dst");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("real.txt", b"data"))
        .unwrap();
    engine
        .apply_rename(&PathBuf::from("real.txt"), &PathBuf::from("../outside.txt"))
        .unwrap();
    assert!(
        dir.join("real.txt").exists(),
        "source must remain untouched when dst is unsafe"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_rename_no_op_for_missing_source() {
    let dir = tmp_dir("rename_no_src");
    let engine = make_engine(dir.clone());
    engine
        .apply_rename(&PathBuf::from("ghost.txt"), &PathBuf::from("dst.txt"))
        .unwrap();
    assert!(!dir.join("dst.txt").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn get_manifest_initially_empty() {
    let dir = tmp_dir("manifest_init_empty");
    let engine = make_engine(dir.clone());
    let manifest = engine.get_manifest();
    assert!(
        manifest.files.is_empty(),
        "manifest must be empty before any apply or scan"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn scan_updates_in_engine_manifest() {
    let dir = tmp_dir("scan_engine_update");
    fs::write(dir.join("file.txt"), b"hello").unwrap();
    let engine = make_engine(dir.clone());
    assert!(
        engine.get_manifest().files.is_empty(),
        "manifest starts empty"
    );
    engine.scan().unwrap();
    assert!(engine
        .get_manifest()
        .files
        .contains_key(&PathBuf::from("file.txt")));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn create_bundles_returns_bundles_for_paths() {
    let dir = tmp_dir("create_bundles_paths");
    fs::write(dir.join("a.txt"), b"aaa").unwrap();
    fs::write(dir.join("b.txt"), b"bbb").unwrap();
    let engine = make_engine(dir.clone());
    let bundles = engine.create_bundles(&[PathBuf::from("a.txt"), PathBuf::from("b.txt")]);
    let total_files: usize = bundles.iter().map(|b| b.files.len()).sum();
    assert_eq!(total_files, 2);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn create_bundles_empty_input_returns_empty() {
    let dir = tmp_dir("create_bundles_empty");
    let engine = make_engine(dir.clone());
    let bundles = engine.create_bundles(&[]);
    assert!(bundles.is_empty());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_finish_detects_missing_chunks() {
    use bytehive_filesync::sync_engine::FinishResult;

    let dir = tmp_dir("lf_missing_chunks");
    let engine = make_engine(dir.clone());
    let chunk0 = vec![1u8; 1024];
    let chunk1 = vec![2u8; 1024];
    let mut all_content = chunk0.clone();
    all_content.extend_from_slice(&chunk1);
    let hash: [u8; 32] = blake3::hash(&all_content).into();
    let rel = PathBuf::from("partial.bin");
    let meta = FileMetadata {
        rel_path: rel.clone(),
        size: all_content.len() as u64,
        hash,
        modified_ms: 0,
        is_dir: false,
    };
    engine.begin_large_file(meta, 2).unwrap();
    engine.receive_large_file_chunk(&rel, 0, &chunk0).unwrap();
    let result = engine.finish_large_file(&rel, hash).unwrap();
    match result {
        FinishResult::MissingChunks(missing) => {
            assert_eq!(missing, vec![1], "chunk 1 must be reported missing");
        }
        FinishResult::Committed => panic!("must not commit when a chunk is missing"),
        FinishResult::CommittedWithConflict(_) => panic!("must not commit when a chunk is missing"),
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn is_file_stable_returns_false_for_nonexistent() {
    let dir = tmp_dir("stable_nonexistent");
    let engine = make_engine(dir.clone());
    assert!(!engine.is_file_stable(Path::new("does_not_exist.txt")));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn is_file_stable_returns_true_for_directory() {
    let dir = tmp_dir("stable_dir");
    fs::create_dir_all(dir.join("subdir")).unwrap();
    let engine = make_engine(dir.clone());
    assert!(engine.is_file_stable(Path::new("subdir")));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_bundle_overwrites_existing_file_and_updates_manifest() {
    let dir = tmp_dir("overwrite_existing");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("data.txt", b"old"))
        .unwrap();
    engine
        .apply_bundle(&file_bundle("data.txt", b"new and longer"))
        .unwrap();
    assert_eq!(fs::read(dir.join("data.txt")).unwrap(), b"new and longer");
    let meta = engine
        .get_manifest()
        .files
        .get(&PathBuf::from("data.txt"))
        .unwrap()
        .clone();
    assert_eq!(meta.size, b"new and longer".len() as u64);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_deletes_directory_clears_manifest_entries() {
    let dir = tmp_dir("delete_dir_clears_manifest");
    let engine = make_engine(dir.clone());
    engine.apply_bundle(&dir_bundle("mydir")).unwrap();
    engine
        .apply_bundle(&file_bundle("mydir/child.txt", b"child"))
        .unwrap();
    let before = engine.get_manifest();
    assert!(before.files.contains_key(&PathBuf::from("mydir")));
    assert!(before.files.contains_key(&PathBuf::from("mydir/child.txt")));
    engine.apply_deletes(&[PathBuf::from("mydir")]).unwrap();
    let after = engine.get_manifest();
    assert!(!after.files.contains_key(&PathBuf::from("mydir")));
    assert!(!after.files.contains_key(&PathBuf::from("mydir/child.txt")));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn is_suppressed_initially_false() {
    let dir = tmp_dir("suppress_init_false");
    let engine = make_engine(dir.clone());
    assert!(!engine.is_suppressed(Path::new("any_file.txt")));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn is_delete_suppressed_initially_false() {
    let dir = tmp_dir("del_suppress_init_false");
    let engine = make_engine(dir.clone());
    assert!(!engine.is_delete_suppressed(Path::new("any_file.txt")));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn send_paths_empty_slice_is_ok() {
    let dir = tmp_dir("send_paths_empty");
    let engine = make_engine(dir.clone());
    // A mock connection is hard to construct directly, so we test via create_bundles instead
    // which internally calls the same bundler logic. Empty input must return empty.
    let bundles = engine.create_bundles(&[]);
    assert!(bundles.is_empty());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn begin_large_file_rejects_path_traversal() {
    use bytehive_filesync::protocol::FileMetadata;
    let dir = tmp_dir("lf_unsafe_path");
    let engine = make_engine(dir.clone());
    let meta = FileMetadata {
        rel_path: PathBuf::from("../escape.bin"),
        size: 100,
        hash: [0u8; 32],
        modified_ms: 0,
        is_dir: false,
    };
    // Must succeed (no-op, not an error) but must not create anything outside the root
    engine.begin_large_file(meta, 1).unwrap();
    assert!(!dir.join("../escape.bin").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn receive_large_file_chunk_unknown_path_returns_pending() {
    use bytehive_filesync::sync_engine::ChunkResult;
    let dir = tmp_dir("chunk_unknown");
    let engine = make_engine(dir.clone());
    // There's no in-progress large file for this path
    let result = engine
        .receive_large_file_chunk(&PathBuf::from("unknown.bin"), 0, b"data")
        .unwrap();
    assert!(matches!(result, ChunkResult::Pending));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn commit_large_file_unknown_path_returns_committed() {
    use bytehive_filesync::sync_engine::FinishResult;
    let dir = tmp_dir("commit_unknown");
    let engine = make_engine(dir.clone());
    // Committing a file that was never begun
    let result = engine
        .commit_large_file(&PathBuf::from("ghost.bin"), [0u8; 32])
        .unwrap();
    assert!(matches!(result, FinishResult::Committed));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn clear_in_progress_removes_assembly() {
    use bytehive_filesync::protocol::FileMetadata;
    let dir = tmp_dir("clear_in_progress");
    let engine = make_engine(dir.clone());
    // Begin a large file assembly
    let meta = FileMetadata {
        rel_path: PathBuf::from("bigfile.bin"),
        size: 1024,
        hash: [0u8; 32],
        modified_ms: 0,
        is_dir: false,
    };
    engine.begin_large_file(meta, 2).unwrap();
    // clear_in_progress should not panic and should clean up
    engine.clear_in_progress();
    // After clearing, committing the path should be a no-op (unknown path)
    use bytehive_filesync::sync_engine::FinishResult;
    let result = engine
        .commit_large_file(&PathBuf::from("bigfile.bin"), [0u8; 32])
        .unwrap();
    assert!(matches!(result, FinishResult::Committed));
    fs::remove_dir_all(&dir).unwrap();
}
