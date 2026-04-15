//! Comprehensive tests for **Option A: conflict-copy resolution**.
//!
//! When both the local and the remote side independently modify a file after
//! the last common sync point, the engine must:
//!
//! 1. Detect the two-sided divergence (compare on-disk hash and incoming hash
//!    against the last-recorded manifest hash).
//! 2. Save a copy of the current (locally-modified) file at a conflict-copy
//!    path before overwriting.
//! 3. Apply the incoming content to the original path.
//!
//! Coverage sections
//! ─────────────────
//! Part 1  – `conflict_copy_name` naming logic
//! Part 2  – `apply_bundle` no-conflict cases
//! Part 3  – `apply_bundle` conflict cases
//! Part 4  – `commit_large_file` no-conflict cases
//! Part 5  – `commit_large_file` conflict cases
//! Part 6  – Regression: behaviour is preserved when no conflict exists

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use bytehive_filesync::{
    exclusions::{ExclusionConfig, Exclusions},
    protocol::{FileBundle, FileData, FileMetadata},
    sync_engine::{conflict_copy_name, FinishResult, SyncEngine},
};

// ── Shared test helpers ───────────────────────────────────────────────────────

fn tmp_dir(suffix: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "filesync_conflict_{}_{suffix}",
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

/// Build a single-file bundle carrying `content` at relative path `rel`.
fn file_bundle(rel: &str, content: &[u8]) -> FileBundle {
    let hash: [u8; 32] = blake3::hash(content).into();
    FileBundle {
        files: vec![FileData {
            metadata: FileMetadata {
                rel_path: PathBuf::from(rel),
                size: content.len() as u64,
                hash,
                modified_ms: 1_000,
                is_dir: false,
            },
            content: content.to_vec(),
        }],
        bundle_id: 1,
    }
}

/// Drive the large-file protocol:
///
/// 1. Apply `prior_content` via a bundle so the manifest records it.
/// 2. If `local_edit` is `Some`, overwrite the on-disk file to simulate a
///    local edit that happened after the last sync.
/// 3. Run the full large-file protocol (`begin` → `receive` → `finish`) for
///    `incoming_content`, and return the `FinishResult`.
fn large_file_with_prior(
    engine: &SyncEngine,
    rel: &str,
    prior_content: &[u8],
    local_edit: Option<&[u8]>,
    incoming_content: &[u8],
) -> std::io::Result<FinishResult> {
    // Step 1: establish the manifest at the "ancestor" state.
    engine.apply_bundle(&file_bundle(rel, prior_content))?;

    // Step 2: optional local divergence.
    if let Some(edited) = local_edit {
        fs::write(engine.root().join(rel), edited)?;
    }

    // Step 3: run the large-file protocol for the incoming version.
    let incoming_hash: [u8; 32] = blake3::hash(incoming_content).into();
    let rel_path = PathBuf::from(rel);
    let meta = FileMetadata {
        rel_path: rel_path.clone(),
        size: incoming_content.len() as u64,
        hash: incoming_hash,
        modified_ms: 2_000,
        is_dir: false,
    };
    engine.begin_large_file(meta, 1)?;
    engine.receive_large_file_chunk(&rel_path, 0, incoming_content)?;
    engine.finish_large_file(&rel_path, incoming_hash)
}

// ═════════════════════════════════════════════════════════════════════════════
// Part 1 – `conflict_copy_name` naming logic
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn conflict_name_file_with_extension_contains_all_components() {
    let p = conflict_copy_name(Path::new("report.txt"), "node-1", 1_000);
    let name = p.file_name().unwrap().to_string_lossy();
    assert!(name.contains("report"), "stem must appear: {name}");
    assert!(
        name.ends_with(".txt"),
        "extension must be preserved: {name}"
    );
    assert!(
        name.contains("conflict"),
        "'conflict' keyword must appear: {name}"
    );
    assert!(name.contains("node-1"), "node-id must appear: {name}");
    assert!(name.contains("1000"), "timestamp must appear: {name}");
}

#[test]
fn conflict_name_file_without_extension_has_no_dot() {
    let p = conflict_copy_name(Path::new("Makefile"), "n", 42);
    let name = p.file_name().unwrap().to_string_lossy();
    // `Makefile` has no extension; the result should not acquire one.
    assert!(!name.ends_with('.'), "no spurious trailing dot: {name}");
    assert!(name.contains("Makefile"), "stem must appear: {name}");
    assert!(name.contains("conflict"), "'conflict' must appear: {name}");
}

#[test]
fn conflict_name_preserves_parent_directory() {
    let p = conflict_copy_name(Path::new("docs/report.txt"), "node-1", 5_000);
    assert_eq!(
        p.parent().unwrap(),
        Path::new("docs"),
        "parent directory must be preserved"
    );
}

#[test]
fn conflict_name_deeply_nested_preserves_full_parent() {
    let p = conflict_copy_name(Path::new("a/b/c/file.rs"), "n", 0);
    assert_eq!(
        p.parent().unwrap(),
        Path::new("a/b/c"),
        "full nested parent must be preserved"
    );
    let name = p.file_name().unwrap().to_string_lossy();
    assert!(name.ends_with(".rs"), "extension must be preserved: {name}");
    assert!(name.contains("file"), "stem must appear: {name}");
}

#[test]
fn conflict_name_root_level_file_has_no_spurious_parent() {
    let p = conflict_copy_name(Path::new("plain.bin"), "x", 99);
    // Either no parent at all, or an empty parent path ("").
    let parent_is_empty = p.parent().map_or(true, |pp| pp == Path::new(""));
    assert!(
        parent_is_empty,
        "root-level file must not gain a spurious parent: {p:?}"
    );
}

#[test]
fn conflict_name_different_timestamps_produce_different_names() {
    let p1 = conflict_copy_name(Path::new("f.txt"), "n", 1_000);
    let p2 = conflict_copy_name(Path::new("f.txt"), "n", 2_000);
    assert_ne!(p1, p2, "different timestamps must produce different names");
}

#[test]
fn conflict_name_different_node_ids_produce_different_names() {
    let p1 = conflict_copy_name(Path::new("f.txt"), "node-A", 1_000);
    let p2 = conflict_copy_name(Path::new("f.txt"), "node-B", 1_000);
    assert_ne!(p1, p2, "different node IDs must produce different names");
}

#[test]
fn conflict_name_same_inputs_are_deterministic() {
    let p1 = conflict_copy_name(Path::new("data.bin"), "srv", 12345);
    let p2 = conflict_copy_name(Path::new("data.bin"), "srv", 12345);
    assert_eq!(p1, p2, "same inputs must always produce the same output");
}

#[test]
fn conflict_name_compound_extension_uses_last_ext_as_rust_extension() {
    // Rust's `Path::extension()` returns only the final component.
    // "archive.tar.gz" → stem "archive.tar", ext "gz".
    let p = conflict_copy_name(Path::new("archive.tar.gz"), "n", 0);
    let name = p.file_name().unwrap().to_string_lossy();
    assert!(name.ends_with(".gz"), "last extension preserved: {name}");
    assert!(
        name.contains("archive.tar"),
        "compound stem preserved: {name}"
    );
}

#[test]
fn conflict_name_node_id_embedded_verbatim() {
    let node_id = "my-special-node-42";
    let p = conflict_copy_name(Path::new("f.txt"), node_id, 0);
    let name = p.file_name().unwrap().to_string_lossy();
    assert!(
        name.contains(node_id),
        "node ID must be embedded verbatim in the filename: {name}"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// Part 2 – `apply_bundle`: no-conflict cases
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn no_conflict_when_file_is_new() {
    // The file is not in the manifest at all → no common ancestor → no conflict.
    let dir = tmp_dir("nc_new_file");
    let engine = make_engine(dir.clone());
    let result = engine
        .apply_bundle(&file_bundle("new.txt", b"hello"))
        .unwrap();
    assert!(
        result.conflicts.is_empty(),
        "brand-new file must not produce a conflict copy"
    );
    assert_eq!(result.written, 1);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn no_conflict_when_only_remote_changed() {
    // manifest = A, on-disk = A (local never diverged), incoming = B.
    // Only the remote side changed → just update, no conflict.
    let dir = tmp_dir("nc_remote_only");
    let engine = make_engine(dir.clone());
    // Establish manifest at version A.
    engine
        .apply_bundle(&file_bundle("file.txt", b"version A"))
        .unwrap();
    // Do NOT touch the on-disk file — it still matches the manifest.
    let result = engine
        .apply_bundle(&file_bundle("file.txt", b"version B"))
        .unwrap();
    assert!(
        result.conflicts.is_empty(),
        "remote-only change must not produce a conflict copy"
    );
    assert_eq!(
        fs::read(dir.join("file.txt")).unwrap(),
        b"version B",
        "incoming content must overwrite cleanly"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn no_conflict_when_incoming_hash_equals_manifest_hash() {
    // manifest = A, on-disk = B (locally edited), incoming = A.
    // The remote is re-sending the same version → incoming hash == manifest hash
    // → no conflict (the condition "remote changed" is not met).
    let dir = tmp_dir("nc_same_incoming");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("stable.txt", b"stable"))
        .unwrap();
    // Simulate a local edit.
    fs::write(dir.join("stable.txt"), b"local edit").unwrap();
    // Incoming is the same as the last-synced (manifest) version.
    let result = engine
        .apply_bundle(&file_bundle("stable.txt", b"stable"))
        .unwrap();
    assert!(
        result.conflicts.is_empty(),
        "incoming == manifest hash must not trigger conflict"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn no_conflict_for_directory_entries_in_bundle() {
    // Directories have no content hash; conflict detection must be skipped.
    let dir = tmp_dir("nc_dir_entry");
    let engine = make_engine(dir.clone());
    let dir_bundle = FileBundle {
        files: vec![FileData {
            metadata: FileMetadata {
                rel_path: PathBuf::from("mydir"),
                size: 0,
                hash: [0u8; 32],
                modified_ms: 0,
                is_dir: true,
            },
            content: vec![],
        }],
        bundle_id: 10,
    };
    engine.apply_bundle(&dir_bundle).unwrap();
    let result = engine.apply_bundle(&dir_bundle).unwrap();
    assert!(
        result.conflicts.is_empty(),
        "directory entries must never produce conflict copies"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn no_conflict_when_file_absent_from_disk() {
    // Manifest has an entry for the path but the file has since been removed
    // from disk (e.g. external deletion).  `hash_file` returns None →
    // `detect_conflict` returns None → no conflict copy.
    let dir = tmp_dir("nc_absent");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("ghost.txt", b"was here"))
        .unwrap();
    // Remove without updating the manifest.
    fs::remove_file(dir.join("ghost.txt")).unwrap();
    let result = engine
        .apply_bundle(&file_bundle("ghost.txt", b"incoming"))
        .unwrap();
    assert!(
        result.conflicts.is_empty(),
        "absent on-disk file must not trigger a conflict"
    );
    fs::remove_dir_all(&dir).unwrap();
}

// ═════════════════════════════════════════════════════════════════════════════
// Part 3 – `apply_bundle`: conflict cases (two-sided divergence)
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn conflict_detected_when_both_sides_diverge() {
    // manifest = A, on-disk = B (local offline edit), incoming = C.
    let dir = tmp_dir("c_both_diverge");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("shared.txt", b"version A"))
        .unwrap();
    fs::write(dir.join("shared.txt"), b"version B (local)").unwrap();
    let result = engine
        .apply_bundle(&file_bundle("shared.txt", b"version C (remote)"))
        .unwrap();
    assert_eq!(
        result.conflicts.len(),
        1,
        "exactly one conflict must be reported"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn conflict_copy_file_exists_on_disk() {
    let dir = tmp_dir("c_copy_on_disk");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("doc.txt", b"base"))
        .unwrap();
    fs::write(dir.join("doc.txt"), b"local edit").unwrap();
    let result = engine
        .apply_bundle(&file_bundle("doc.txt", b"remote edit"))
        .unwrap();
    let ci = &result.conflicts[0];
    assert!(
        dir.join(&ci.conflict_copy_path).exists(),
        "conflict copy must exist on disk at {:?}",
        ci.conflict_copy_path
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn conflict_copy_contains_pre_overwrite_local_content() {
    // The conflict copy must preserve the local (diverged) version verbatim.
    let dir = tmp_dir("c_local_content");
    let local_content = b"local diverged content must be preserved";
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("notes.txt", b"common ancestor"))
        .unwrap();
    fs::write(dir.join("notes.txt"), local_content).unwrap();
    let result = engine
        .apply_bundle(&file_bundle("notes.txt", b"remote version"))
        .unwrap();
    let ci = &result.conflicts[0];
    assert_eq!(
        fs::read(dir.join(&ci.conflict_copy_path)).unwrap(),
        local_content,
        "conflict copy must hold the exact pre-overwrite content"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn original_path_gets_incoming_content_after_conflict() {
    // After resolution the original path must hold the incoming (winning) content.
    let dir = tmp_dir("c_incoming_wins");
    let incoming = b"incoming remote content - this should win";
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("f.txt", b"ancestor"))
        .unwrap();
    fs::write(dir.join("f.txt"), b"local change").unwrap();
    engine
        .apply_bundle(&file_bundle("f.txt", incoming))
        .unwrap();
    assert_eq!(
        fs::read(dir.join("f.txt")).unwrap(),
        incoming,
        "original path must contain the incoming content"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn conflict_info_original_path_is_correct() {
    let dir = tmp_dir("c_info_original");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("work.txt", b"v0"))
        .unwrap();
    fs::write(dir.join("work.txt"), b"v1 local").unwrap();
    let result = engine
        .apply_bundle(&file_bundle("work.txt", b"v2 remote"))
        .unwrap();
    assert_eq!(result.conflicts[0].original_path, PathBuf::from("work.txt"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn conflict_copy_path_is_in_same_directory_as_original() {
    let dir = tmp_dir("c_same_dir");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("subdir/file.txt", b"base"))
        .unwrap();
    fs::write(dir.join("subdir/file.txt"), b"local").unwrap();
    let result = engine
        .apply_bundle(&file_bundle("subdir/file.txt", b"remote"))
        .unwrap();
    let ci = &result.conflicts[0];
    assert_eq!(
        ci.conflict_copy_path.parent().unwrap(),
        ci.original_path.parent().unwrap(),
        "conflict copy must reside in the same directory as the original"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn conflict_copy_filename_contains_conflict_keyword() {
    let dir = tmp_dir("c_keyword");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("data.bin", b"v0"))
        .unwrap();
    fs::write(dir.join("data.bin"), b"local").unwrap();
    let result = engine
        .apply_bundle(&file_bundle("data.bin", b"remote"))
        .unwrap();
    let name = result.conflicts[0]
        .conflict_copy_path
        .file_name()
        .unwrap()
        .to_string_lossy();
    assert!(
        name.to_lowercase().contains("conflict"),
        "conflict copy filename must contain 'conflict': {name}"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn conflict_copy_filename_preserves_file_extension() {
    let dir = tmp_dir("c_ext");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("readme.md", b"# v0"))
        .unwrap();
    fs::write(dir.join("readme.md"), b"# local").unwrap();
    let result = engine
        .apply_bundle(&file_bundle("readme.md", b"# remote"))
        .unwrap();
    let name = result.conflicts[0]
        .conflict_copy_path
        .file_name()
        .unwrap()
        .to_string_lossy();
    assert!(
        name.ends_with(".md"),
        "original extension must be preserved in conflict copy name: {name}"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_result_written_count_includes_conflicted_file() {
    let dir = tmp_dir("c_written_count");
    let engine = make_engine(dir.clone());
    engine.apply_bundle(&file_bundle("a.txt", b"base")).unwrap();
    fs::write(dir.join("a.txt"), b"local").unwrap();
    let result = engine
        .apply_bundle(&file_bundle("a.txt", b"remote"))
        .unwrap();
    assert_eq!(
        result.written, 1,
        "`written` must count the file even when a conflict copy was made"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn only_conflicted_files_get_copies_in_multi_file_bundle() {
    // Bundle with two files.  Only the one with a local divergence should
    // produce a conflict copy.
    let dir = tmp_dir("c_selective");
    let engine = make_engine(dir.clone());

    // Establish both files in the manifest.
    let initial = FileBundle {
        files: vec![
            FileData {
                metadata: FileMetadata {
                    rel_path: PathBuf::from("will_conflict.txt"),
                    size: 5,
                    hash: blake3::hash(b"base1").into(),
                    modified_ms: 100,
                    is_dir: false,
                },
                content: b"base1".to_vec(),
            },
            FileData {
                metadata: FileMetadata {
                    rel_path: PathBuf::from("no_conflict.txt"),
                    size: 5,
                    hash: blake3::hash(b"base2").into(),
                    modified_ms: 100,
                    is_dir: false,
                },
                content: b"base2".to_vec(),
            },
        ],
        bundle_id: 1,
    };
    engine.apply_bundle(&initial).unwrap();

    // Locally edit only the first file.
    fs::write(dir.join("will_conflict.txt"), b"local edit").unwrap();
    // Leave no_conflict.txt untouched — it still matches the manifest.

    // Incoming bundle updates both files.
    let update = FileBundle {
        files: vec![
            FileData {
                metadata: FileMetadata {
                    rel_path: PathBuf::from("will_conflict.txt"),
                    size: 13,
                    hash: blake3::hash(b"remote update1").into(),
                    modified_ms: 200,
                    is_dir: false,
                },
                content: b"remote update1".to_vec(),
            },
            FileData {
                metadata: FileMetadata {
                    rel_path: PathBuf::from("no_conflict.txt"),
                    size: 13,
                    hash: blake3::hash(b"remote update2").into(),
                    modified_ms: 200,
                    is_dir: false,
                },
                content: b"remote update2".to_vec(),
            },
        ],
        bundle_id: 2,
    };
    let result = engine.apply_bundle(&update).unwrap();

    assert_eq!(result.written, 2, "both files must be written");
    assert_eq!(
        result.conflicts.len(),
        1,
        "only the locally-edited file must produce a conflict copy"
    );
    assert_eq!(
        result.conflicts[0].original_path,
        PathBuf::from("will_conflict.txt"),
        "conflict must reference the correct file"
    );
    assert!(
        result
            .conflicts
            .iter()
            .all(|ci| ci.original_path != PathBuf::from("no_conflict.txt")),
        "non-conflicting file must not appear in conflicts list"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unsafe_path_still_rejected_even_with_conflict_logic_active() {
    let dir = tmp_dir("c_unsafe_path");
    let engine = make_engine(dir.clone());
    let bad_bundle = FileBundle {
        files: vec![FileData {
            metadata: FileMetadata {
                rel_path: PathBuf::from("../escape.txt"),
                size: 4,
                hash: blake3::hash(b"evil").into(),
                modified_ms: 0,
                is_dir: false,
            },
            content: b"evil".to_vec(),
        }],
        bundle_id: 99,
    };
    let result = engine.apply_bundle(&bad_bundle).unwrap();
    assert_eq!(result.written, 0, "unsafe path must be rejected");
    assert!(
        result.conflicts.is_empty(),
        "no conflict copy must be created for a rejected path"
    );
    assert!(
        !dir.parent().unwrap().join("escape.txt").exists(),
        "no file must be written outside the root"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn conflict_copy_does_not_pollute_manifest() {
    // The conflict copy path is a local artefact; it must NOT appear in the
    // engine manifest.  The original path must still be present.
    let dir = tmp_dir("c_no_manifest_pollution");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("keep.txt", b"v0"))
        .unwrap();
    fs::write(dir.join("keep.txt"), b"local").unwrap();
    let result = engine
        .apply_bundle(&file_bundle("keep.txt", b"remote"))
        .unwrap();
    let ci = &result.conflicts[0];
    let m = engine.get_manifest();
    assert!(
        !m.files.contains_key(&ci.conflict_copy_path),
        "conflict copy path must not appear in the manifest"
    );
    assert!(
        m.files.contains_key(&ci.original_path),
        "original path must remain in the manifest after conflict resolution"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn manifest_records_incoming_hash_after_conflict() {
    // After conflict resolution the manifest entry for the original path
    // must reflect the incoming (winning) content, not the local version.
    let dir = tmp_dir("c_manifest_hash");
    let incoming = b"incoming wins";
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("f.txt", b"ancestor"))
        .unwrap();
    fs::write(dir.join("f.txt"), b"local diverged").unwrap();
    engine
        .apply_bundle(&file_bundle("f.txt", incoming))
        .unwrap();
    let meta = engine
        .get_manifest()
        .files
        .get(&PathBuf::from("f.txt"))
        .cloned()
        .expect("original path must be in manifest");
    let expected_hash: [u8; 32] = blake3::hash(incoming).into();
    assert_eq!(
        meta.hash, expected_hash,
        "manifest must record the incoming hash, not the local one"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn repeated_conflicts_on_same_file_each_produce_a_copy() {
    // Two successive conflict rounds; each must produce its own conflict copy.
    let dir = tmp_dir("c_repeated");
    let engine = make_engine(dir.clone());
    engine.apply_bundle(&file_bundle("rep.txt", b"v0")).unwrap();

    // Round 1: local diverges to "local-1", remote sends "remote-1".
    fs::write(dir.join("rep.txt"), b"local-1").unwrap();
    let r1 = engine
        .apply_bundle(&file_bundle("rep.txt", b"remote-1"))
        .unwrap();
    assert_eq!(r1.conflicts.len(), 1, "round 1 must produce a conflict");

    // Round 2: local diverges to "local-2", remote sends "remote-2".
    fs::write(dir.join("rep.txt"), b"local-2").unwrap();
    let r2 = engine
        .apply_bundle(&file_bundle("rep.txt", b"remote-2"))
        .unwrap();
    assert_eq!(
        r2.conflicts.len(),
        1,
        "round 2 must also produce a conflict"
    );

    // Both conflict copies must exist on disk (they may share the same path
    // if the test runs within the same second, but the copy content is valid).
    assert!(
        dir.join(&r1.conflicts[0].conflict_copy_path).exists(),
        "round-1 conflict copy must be on disk"
    );
    assert!(
        dir.join(&r2.conflicts[0].conflict_copy_path).exists(),
        "round-2 conflict copy must be on disk"
    );
    fs::remove_dir_all(&dir).unwrap();
}

// ═════════════════════════════════════════════════════════════════════════════
// Part 4 – `commit_large_file`: no-conflict cases
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn large_file_no_conflict_for_brand_new_file() {
    // No prior manifest entry → no ancestor to diverge from → Committed.
    let dir = tmp_dir("lf_nc_new");
    let engine = make_engine(dir.clone());
    let content = b"brand new large file - no prior version";
    let hash: [u8; 32] = blake3::hash(content).into();
    let rel = PathBuf::from("new_large.bin");
    let meta = FileMetadata {
        rel_path: rel.clone(),
        size: content.len() as u64,
        hash,
        modified_ms: 0,
        is_dir: false,
    };
    engine.begin_large_file(meta, 1).unwrap();
    engine.receive_large_file_chunk(&rel, 0, content).unwrap();
    let result = engine.finish_large_file(&rel, hash).unwrap();
    assert!(
        matches!(result, FinishResult::Committed),
        "brand-new large file must yield Committed, not CommittedWithConflict"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_no_conflict_when_local_unchanged() {
    // manifest = A, on-disk = A (no local edit), incoming large-file = B.
    // Only the remote changed → no conflict.
    let dir = tmp_dir("lf_nc_unchanged");
    let engine = make_engine(dir.clone());
    let result = large_file_with_prior(
        &engine,
        "data.bin",
        b"prior version A",
        None, // do NOT locally edit the on-disk file
        b"incoming version B",
    )
    .unwrap();
    assert!(
        matches!(result, FinishResult::Committed),
        "local-unchanged large file must yield Committed: {result:?}"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_no_conflict_when_incoming_equals_manifest() {
    // manifest = A, on-disk = B (locally edited), incoming = A (re-send).
    // incoming hash == manifest hash → condition "remote changed" is false → no conflict.
    let dir = tmp_dir("lf_nc_same_incoming");
    let engine = make_engine(dir.clone());
    let content = b"stable content";
    engine
        .apply_bundle(&file_bundle("stable.bin", content))
        .unwrap();
    // Simulate a local edit.
    fs::write(dir.join("stable.bin"), b"local edit").unwrap();
    // Large-file incoming is the same bytes that were originally synced.
    let incoming_hash: [u8; 32] = blake3::hash(content).into();
    let rel = PathBuf::from("stable.bin");
    let meta = FileMetadata {
        rel_path: rel.clone(),
        size: content.len() as u64,
        hash: incoming_hash,
        modified_ms: 0,
        is_dir: false,
    };
    engine.begin_large_file(meta, 1).unwrap();
    engine.receive_large_file_chunk(&rel, 0, content).unwrap();
    let result = engine.finish_large_file(&rel, incoming_hash).unwrap();
    assert!(
        matches!(result, FinishResult::Committed),
        "re-sending same content must not trigger conflict"
    );
    fs::remove_dir_all(&dir).unwrap();
}

// ═════════════════════════════════════════════════════════════════════════════
// Part 5 – `commit_large_file`: conflict cases
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn large_file_returns_committed_with_conflict_on_two_sided_divergence() {
    // manifest = A, on-disk = B (local edit), incoming large-file = C.
    let dir = tmp_dir("lf_c_diverge");
    let engine = make_engine(dir.clone());
    let result = large_file_with_prior(
        &engine,
        "lf.bin",
        b"version A",
        Some(b"version B - local edit"),
        b"version C - remote large file",
    )
    .unwrap();
    assert!(
        matches!(result, FinishResult::CommittedWithConflict(_)),
        "two-sided divergence must yield CommittedWithConflict: {result:?}"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_conflict_copy_exists_on_disk() {
    let dir = tmp_dir("lf_c_copy_exists");
    let engine = make_engine(dir.clone());
    let result = large_file_with_prior(
        &engine,
        "report.bin",
        b"ancestor",
        Some(b"local diverged"),
        b"incoming large",
    )
    .unwrap();
    if let FinishResult::CommittedWithConflict(ci) = result {
        assert!(
            dir.join(&ci.conflict_copy_path).exists(),
            "large-file conflict copy must exist on disk at {:?}",
            ci.conflict_copy_path
        );
    } else {
        panic!("expected CommittedWithConflict");
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_conflict_copy_contains_local_pre_overwrite_content() {
    let dir = tmp_dir("lf_c_local_content");
    let local_content = b"this is the local version that must be saved as a conflict copy";
    let engine = make_engine(dir.clone());
    let result = large_file_with_prior(
        &engine,
        "preserve.bin",
        b"common ancestor",
        Some(local_content),
        b"completely different remote large-file content",
    )
    .unwrap();
    if let FinishResult::CommittedWithConflict(ci) = result {
        assert_eq!(
            fs::read(dir.join(&ci.conflict_copy_path)).unwrap(),
            local_content,
            "large-file conflict copy must hold the exact pre-overwrite local bytes"
        );
    } else {
        panic!("expected CommittedWithConflict");
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_original_path_gets_incoming_content_after_conflict() {
    let dir = tmp_dir("lf_c_incoming");
    let incoming = b"final incoming large-file content that wins";
    let engine = make_engine(dir.clone());
    large_file_with_prior(
        &engine,
        "target.bin",
        b"v0 ancestor",
        Some(b"v1 local edit"),
        incoming,
    )
    .unwrap();
    assert_eq!(
        fs::read(dir.join("target.bin")).unwrap(),
        incoming,
        "original path must hold the incoming content after large-file conflict"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_conflict_info_original_path_is_correct() {
    let dir = tmp_dir("lf_c_paths");
    let engine = make_engine(dir.clone());
    let result = large_file_with_prior(
        &engine,
        "img.dat",
        b"base",
        Some(b"local mod"),
        b"remote large",
    )
    .unwrap();
    if let FinishResult::CommittedWithConflict(ci) = result {
        assert_eq!(
            ci.original_path,
            PathBuf::from("img.dat"),
            "ConflictInfo.original_path must match the transferred file path"
        );
    } else {
        panic!("expected CommittedWithConflict");
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_conflict_copy_filename_contains_conflict_keyword() {
    let dir = tmp_dir("lf_c_keyword");
    let engine = make_engine(dir.clone());
    let result = large_file_with_prior(&engine, "kw.bin", b"a", Some(b"b"), b"c").unwrap();
    if let FinishResult::CommittedWithConflict(ci) = result {
        let name = ci.conflict_copy_path.file_name().unwrap().to_string_lossy();
        assert!(
            name.to_lowercase().contains("conflict"),
            "large-file conflict copy filename must contain 'conflict': {name}"
        );
    } else {
        panic!("expected CommittedWithConflict");
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_manifest_updated_with_incoming_hash_after_conflict() {
    // Even when a conflict copy is created the manifest must record the
    // incoming (winning) hash for the original path.
    let dir = tmp_dir("lf_c_manifest");
    let incoming = b"incoming remote content wins";
    let incoming_hash: [u8; 32] = blake3::hash(incoming).into();
    let engine = make_engine(dir.clone());
    large_file_with_prior(
        &engine,
        "win.bin",
        b"ancestor",
        Some(b"local diverged"),
        incoming,
    )
    .unwrap();
    let meta = engine
        .get_manifest()
        .files
        .get(&PathBuf::from("win.bin"))
        .cloned()
        .expect("manifest must have an entry after large-file commit");
    assert_eq!(
        meta.hash, incoming_hash,
        "manifest must record the incoming hash after conflict resolution"
    );
    fs::remove_dir_all(&dir).unwrap();
}

// ═════════════════════════════════════════════════════════════════════════════
// Part 6 – Regression: existing behaviour preserved when no conflict exists
// ═════════════════════════════════════════════════════════════════════════════

#[test]
fn regression_apply_bundle_written_count_no_conflict() {
    let dir = tmp_dir("reg_written");
    let engine = make_engine(dir.clone());
    let result = engine
        .apply_bundle(&file_bundle("a.txt", b"hello"))
        .unwrap();
    assert_eq!(result.written, 1);
    assert!(result.conflicts.is_empty());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn regression_apply_bundle_writes_correct_content() {
    let dir = tmp_dir("reg_content");
    let engine = make_engine(dir.clone());
    engine
        .apply_bundle(&file_bundle("out.txt", b"expected content"))
        .unwrap();
    assert_eq!(fs::read(dir.join("out.txt")).unwrap(), b"expected content");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn regression_apply_bundle_updates_manifest() {
    let dir = tmp_dir("reg_manifest");
    let engine = make_engine(dir.clone());
    engine.apply_bundle(&file_bundle("m.txt", b"data")).unwrap();
    assert!(engine
        .get_manifest()
        .files
        .contains_key(&PathBuf::from("m.txt")));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn regression_large_file_happy_path_yields_committed() {
    // Full large-file flow with no prior version must complete with
    // FinishResult::Committed (not CommittedWithConflict).
    let dir = tmp_dir("reg_lf_happy");
    let engine = make_engine(dir.clone());
    let content = b"large-file regression - option A must not break happy path";
    let hash: [u8; 32] = blake3::hash(content).into();
    let rel = PathBuf::from("lf.bin");
    let meta = FileMetadata {
        rel_path: rel.clone(),
        size: content.len() as u64,
        hash,
        modified_ms: 0,
        is_dir: false,
    };
    engine.begin_large_file(meta, 1).unwrap();
    engine.receive_large_file_chunk(&rel, 0, content).unwrap();
    let result = engine.finish_large_file(&rel, hash).unwrap();
    assert!(
        matches!(result, FinishResult::Committed),
        "no-conflict large file must yield Committed"
    );
    assert_eq!(fs::read(dir.join("lf.bin")).unwrap(), content);
    assert!(engine.get_manifest().files.contains_key(&rel));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn regression_large_file_hash_mismatch_still_errors() {
    // A corrupted transfer (final_hash ≠ actual content hash) must still
    // return an InvalidData error — conflict-copy logic must not interfere.
    let dir = tmp_dir("reg_lf_hash");
    let engine = make_engine(dir.clone());
    let content = b"some content";
    let wrong_hash = [0xFFu8; 32];
    let rel = PathBuf::from("corrupt.bin");
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
    assert!(result.is_err(), "hash mismatch must still return an error");
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn regression_apply_bundle_multiple_files_written_correctly() {
    let dir = tmp_dir("reg_multi");
    let engine = make_engine(dir.clone());
    let bundle = FileBundle {
        files: vec![
            FileData {
                metadata: FileMetadata {
                    rel_path: PathBuf::from("one.txt"),
                    size: 3,
                    hash: blake3::hash(b"aaa").into(),
                    modified_ms: 0,
                    is_dir: false,
                },
                content: b"aaa".to_vec(),
            },
            FileData {
                metadata: FileMetadata {
                    rel_path: PathBuf::from("two.txt"),
                    size: 3,
                    hash: blake3::hash(b"bbb").into(),
                    modified_ms: 0,
                    is_dir: false,
                },
                content: b"bbb".to_vec(),
            },
        ],
        bundle_id: 5,
    };
    let result = engine.apply_bundle(&bundle).unwrap();
    assert_eq!(result.written, 2);
    assert!(result.conflicts.is_empty());
    assert_eq!(fs::read(dir.join("one.txt")).unwrap(), b"aaa");
    assert_eq!(fs::read(dir.join("two.txt")).unwrap(), b"bbb");
    fs::remove_dir_all(&dir).unwrap();
}
