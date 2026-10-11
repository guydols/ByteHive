//! Regression tests for large-file staging isolation.
//!
//! Bug: the staging path was `transfers/<rel>.with_extension("tmp")`, so
//! `objects/pack/pack-X.pack` and `objects/pack/pack-X.idx` both staged to
//! `transfers/objects/pack/pack-X.tmp`. Interleaved transfers then truncated,
//! overwrote and deleted each other's bytes, and the transfers never converged.
//!
//! Coverage
//! ────────
//! Part 1 – concurrent, interleaved same-stem transfers both commit intact
//! Part 2 – a hash-mismatching sibling does not delete the other's staging file
//! Part 3 – staging names are unique (extensions, dirs, `x` vs `x.tmp`)
//! Part 4 – a legacy `.tmp` leftover from the old scheme does not interfere

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
};

use bytehive_filesync::{
    exclusions::{ExclusionConfig, Exclusions},
    protocol::{FileMetadata, FILE_CHUNK_SIZE, TMP_DIR},
    sync_engine::{staging_path, FinishResult, SyncEngine},
};
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_engine(root: &Path) -> SyncEngine {
    let ex = Arc::new(Exclusions::compile(&ExclusionConfig::default()));
    SyncEngine::new(root.to_path_buf(), "test-node".to_string(), ex)
}

/// Deterministic content of `len` bytes, distinct per `seed`.
fn content(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(31) ^ (i >> 9) ^ seed as usize) as u8)
        .collect()
}

/// Two full chunks plus a small tail: exercises multi-chunk offsets.
fn two_chunk_len() -> usize {
    FILE_CHUNK_SIZE + 4096
}

fn meta(rel: &str, data: &[u8], hash: [u8; 32]) -> FileMetadata {
    FileMetadata {
        rel_path: PathBuf::from(rel),
        size: data.len() as u64,
        hash,
        modified_ms: 1_000,
        change_sequence: 0,
        is_dir: false,
    }
}

fn chunk_count(data: &[u8]) -> u32 {
    data.chunks(FILE_CHUNK_SIZE).count() as u32
}

fn hash_of(data: &[u8]) -> [u8; 32] {
    blake3::hash(data).into()
}

fn transfers_dir(root: &Path) -> PathBuf {
    root.join(TMP_DIR)
}

fn transfer_file_count(root: &Path) -> usize {
    match fs::read_dir(transfers_dir(root)) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .count(),
        Err(_) => 0,
    }
}

fn send_chunk(engine: &SyncEngine, rel: &str, data: &[u8], index: u32) -> std::io::Result<()> {
    let start = index as usize * FILE_CHUNK_SIZE;
    let end = (start + FILE_CHUNK_SIZE).min(data.len());
    engine
        .receive_large_file_chunk(&PathBuf::from(rel), index, &data[start..end])
        .map(|_| ())
}

// ── Part 1: concurrent interleaved same-stem transfers ───────────────────────

#[test]
fn same_stem_pack_pair_concurrent_interleaved_both_commit() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let engine = make_engine(root);

    let rel_pack = "objects/pack/pack-abc.pack";
    let rel_idx = "objects/pack/pack-abc.idx";
    let pack = content(1, two_chunk_len());
    let idx = content(2, two_chunk_len() - 1000);

    engine
        .begin_large_file(meta(rel_pack, &pack, hash_of(&pack)), chunk_count(&pack))
        .unwrap();
    engine
        .begin_large_file(meta(rel_idx, &idx, hash_of(&idx)), chunk_count(&idx))
        .unwrap();

    // Both transfers must have their own staging file.
    assert_eq!(
        transfer_file_count(root),
        2,
        "pack and idx must stage separately"
    );

    // Real threads, barrier-synchronised per chunk so writes interleave.
    let max_chunks = chunk_count(&pack).max(chunk_count(&idx));
    let barrier = Barrier::new(2);
    // Errors are recorded, not panicked, so both threads always reach every
    // barrier.wait() and a failure reports instead of deadlocking.
    let (pack_res, idx_res) = std::thread::scope(|s| {
        let eng = &engine;
        let bar = &barrier;
        let pack_ref = &pack;
        let idx_ref = &idx;
        let h_pack = s.spawn(move || {
            let mut chunk_err = None;
            for i in 0..max_chunks {
                if i < chunk_count(pack_ref) {
                    if let Err(e) = send_chunk(eng, rel_pack, pack_ref, i) {
                        chunk_err.get_or_insert(e);
                    }
                }
                bar.wait();
            }
            match chunk_err {
                Some(e) => Err(e),
                None => eng.finish_large_file(&PathBuf::from(rel_pack), hash_of(pack_ref)),
            }
        });
        let h_idx = s.spawn(move || {
            let mut chunk_err = None;
            for i in 0..max_chunks {
                if i < chunk_count(idx_ref) {
                    if let Err(e) = send_chunk(eng, rel_idx, idx_ref, i) {
                        chunk_err.get_or_insert(e);
                    }
                }
                bar.wait();
            }
            match chunk_err {
                Some(e) => Err(e),
                None => eng.finish_large_file(&PathBuf::from(rel_idx), hash_of(idx_ref)),
            }
        });
        (h_pack.join().unwrap(), h_idx.join().unwrap())
    });

    // No ENOENT, no hash mismatch: both commit.
    assert!(
        matches!(pack_res, Ok(FinishResult::Committed)),
        "pack: {pack_res:?}"
    );
    assert!(
        matches!(idx_res, Ok(FinishResult::Committed)),
        "idx: {idx_res:?}"
    );

    assert_eq!(
        fs::read(root.join(rel_pack)).unwrap(),
        pack,
        "pack bytes corrupted"
    );
    assert_eq!(
        fs::read(root.join(rel_idx)).unwrap(),
        idx,
        "idx bytes corrupted"
    );
    assert_eq!(transfer_file_count(root), 0, "staging files left behind");
}

#[test]
fn same_stem_pair_finish_order_reversed_sibling_survives_first_commit() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let engine = make_engine(root);

    let rel_pack = "pkg/pack-z.pack";
    let rel_idx = "pkg/pack-z.idx";
    let pack = content(7, two_chunk_len());
    let idx = content(9, two_chunk_len());

    engine
        .begin_large_file(meta(rel_pack, &pack, hash_of(&pack)), 2)
        .unwrap();
    engine
        .begin_large_file(meta(rel_idx, &idx, hash_of(&idx)), 2)
        .unwrap();

    // Interleave chunk-by-chunk, idx first.
    for i in 0..2 {
        send_chunk(&engine, rel_idx, &idx, i).unwrap();
        send_chunk(&engine, rel_pack, &pack, i).unwrap();
    }

    // Finish the pack first. Its commit must not touch idx's staging file.
    let r = engine
        .finish_large_file(&PathBuf::from(rel_pack), hash_of(&pack))
        .unwrap();
    assert!(matches!(r, FinishResult::Committed));
    assert!(
        staging_path(root, Path::new(rel_idx)).is_file(),
        "sibling staging file was deleted by the other commit"
    );

    let r = engine
        .finish_large_file(&PathBuf::from(rel_idx), hash_of(&idx))
        .unwrap();
    assert!(matches!(r, FinishResult::Committed));
    assert_eq!(fs::read(root.join(rel_pack)).unwrap(), pack);
    assert_eq!(fs::read(root.join(rel_idx)).unwrap(), idx);
}

// ── Part 2: hash mismatch in one transfer must not hurt its sibling ──────────

/// Run an interleaved pair where transfer `bad` (0 = pack, 1 = idx) finishes
/// with a wrong hash. Returns the root tempdir and the good sibling's data.
fn run_mismatch(bad: usize) -> (TempDir, Vec<u8>, String, String) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let engine = make_engine(root);

    let rel = ["lib/pack-m.pack", "lib/pack-m.idx"];
    let data = [content(3, two_chunk_len()), content(4, two_chunk_len())];
    for k in 0..2 {
        engine
            .begin_large_file(meta(rel[k], &data[k], hash_of(&data[k])), 2)
            .unwrap();
    }

    for i in 0..2 {
        for k in 0..2 {
            send_chunk(&engine, rel[k], &data[k], i).unwrap();
        }
    }

    let good = 1 - bad;
    let wrong = hash_of(b"not the content");
    let bad_res = engine.finish_large_file(&PathBuf::from(rel[bad]), wrong);
    assert!(
        bad_res.is_err(),
        "hash mismatch must be reported as an error, got {bad_res:?}"
    );
    assert!(
        !staging_path(root, Path::new(rel[bad])).exists(),
        "bad transfer must remove its own staging file"
    );
    assert!(
        staging_path(root, Path::new(rel[good])).is_file(),
        "bad transfer deleted the sibling's staging file"
    );

    let res = engine
        .finish_large_file(&PathBuf::from(rel[good]), hash_of(&data[good]))
        .unwrap();
    assert!(matches!(res, FinishResult::Committed), "sibling: {res:?}");
    let got = fs::read(root.join(rel[good])).unwrap();
    assert_eq!(got, data[good], "sibling bytes corrupted");
    (
        tmp,
        data[good].clone(),
        rel[good].to_string(),
        rel[bad].to_string(),
    )
}

#[test]
fn mismatch_on_pack_leaves_idx_sibling_committable() {
    let (tmp, _, good, bad) = run_mismatch(0);
    assert!(root_has(tmp.path(), &good));
    assert!(
        !root_has(tmp.path(), &bad),
        "mismatched file must not be committed"
    );
    assert_eq!(transfer_file_count(tmp.path()), 0);
}

#[test]
fn mismatch_on_idx_leaves_pack_sibling_committable() {
    let (tmp, _, good, bad) = run_mismatch(1);
    assert!(root_has(tmp.path(), &good));
    assert!(!root_has(tmp.path(), &bad));
    assert_eq!(transfer_file_count(tmp.path()), 0);
}

fn root_has(root: &Path, rel: &str) -> bool {
    root.join(rel).is_file()
}

// ── Part 3: staging name uniqueness ──────────────────────────────────────────

#[test]
fn staging_names_unique_for_same_stem_extensions() {
    let root = Path::new("/nonexistent-root");
    let dir = "objects/pack";
    let rels: Vec<String> = ["pack", "idx", "rev", "bitmap", "keep", ""]
        .iter()
        .map(|ext| {
            if ext.is_empty() {
                format!("{dir}/pack-abc")
            } else {
                format!("{dir}/pack-abc.{ext}")
            }
        })
        .collect();
    let names: HashSet<PathBuf> = rels
        .iter()
        .map(|r| staging_path(root, Path::new(r)))
        .collect();
    assert_eq!(names.len(), rels.len(), "staging names collided: {names:?}");
}

#[test]
fn staging_names_do_not_collide_with_replaced_extension_forms() {
    // Old scheme mapped `x` and `x.tmp` (and `a.b` / `a.c`) to the same name.
    let root = Path::new("/nonexistent-root");
    let pairs = [
        ("x", "x.tmp"),
        ("data.bin", "data.tmp"),
        ("a.b", "a.c"),
        ("pack-1.pack", "pack-1.idx"),
    ];
    for (a, b) in pairs {
        assert_ne!(
            staging_path(root, Path::new(a)),
            staging_path(root, Path::new(b)),
            "{a} and {b} share a staging file"
        );
    }
}

#[test]
fn staging_names_unique_across_directories() {
    let root = Path::new("/nonexistent-root");
    let rels = [
        "a/pack-abc.pack",
        "b/pack-abc.pack",
        "a/b/pack-abc.pack",
        "pack-abc.pack",
        "a-b/pack-abc.pack",
    ];
    let names: HashSet<PathBuf> = rels
        .iter()
        .map(|r| staging_path(root, Path::new(r)))
        .collect();
    assert_eq!(names.len(), rels.len());
}

#[test]
fn staging_name_is_deterministic_and_under_transfers_dir() {
    let root = Path::new("/nonexistent-root");
    let rel = Path::new("objects/pack/pack-abc.pack");
    let p1 = staging_path(root, rel);
    let p2 = staging_path(root, rel);
    assert_eq!(p1, p2);
    assert_eq!(p1.parent().unwrap(), transfers_dir(root));
    assert_eq!(p1.extension().and_then(|e| e.to_str()), Some("part"));
    // Separator-only differences map to the same path (same dst, same transfer).
    assert_eq!(
        staging_path(root, Path::new("objects//pack/pack-abc.pack")),
        p1
    );
}

// ── Part 4: legacy `.tmp` leftovers from the old naming scheme ───────────────

#[test]
fn legacy_tmp_leftover_does_not_break_transfers() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Leftovers the old binary would have written for the same stem, plus an
    // unrelated legacy file. None of these names is reachable any more.
    let legacy_dir = transfers_dir(root).join("objects/pack");
    fs::create_dir_all(&legacy_dir).unwrap();
    fs::write(legacy_dir.join("pack-abc.tmp"), b"stale partial bytes").unwrap();
    fs::write(transfers_dir(root).join("old.tmp"), b"stale").unwrap();

    let engine = make_engine(root);
    let rel_pack = "objects/pack/pack-abc.pack";
    let rel_idx = "objects/pack/pack-abc.idx";
    let pack = content(5, two_chunk_len());
    let idx = content(6, two_chunk_len());

    engine
        .begin_large_file(meta(rel_pack, &pack, hash_of(&pack)), 2)
        .unwrap();
    engine
        .begin_large_file(meta(rel_idx, &idx, hash_of(&idx)), 2)
        .unwrap();
    for i in 0..2 {
        send_chunk(&engine, rel_pack, &pack, i).unwrap();
        send_chunk(&engine, rel_idx, &idx, i).unwrap();
    }
    assert!(matches!(
        engine
            .finish_large_file(&PathBuf::from(rel_pack), hash_of(&pack))
            .unwrap(),
        FinishResult::Committed
    ));
    assert!(matches!(
        engine
            .finish_large_file(&PathBuf::from(rel_idx), hash_of(&idx))
            .unwrap(),
        FinishResult::Committed
    ));

    assert_eq!(fs::read(root.join(rel_pack)).unwrap(), pack);
    assert_eq!(fs::read(root.join(rel_idx)).unwrap(), idx);
    // Legacy files are inert: they are neither read nor removed by new code.
    assert_eq!(
        fs::read(legacy_dir.join("pack-abc.tmp")).unwrap(),
        b"stale partial bytes"
    );
}
