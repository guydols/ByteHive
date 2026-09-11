//! Regression tests for the manifest hash cache (initial-scan speedup).
//! Fast, no network: tempdirs + `SyncEngine::scan` + direct cache API.
//! Policy: zero inline tests in `src/` — all coverage lives here.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytehive_filesync::{
    exclusions::{ExclusionConfig, Exclusions},
    hex,
    manifest_cache::{
        inode_of, mtime_parts, now_ms, CachedEntry, ManifestCache, CACHE_MIN_AGE_MS,
    },
    sync_engine::SyncEngine,
};

fn make_engine(root: PathBuf, node: &str) -> SyncEngine {
    let ex = Arc::new(Exclusions::compile(&ExclusionConfig::default()));
    SyncEngine::new(root, node.to_string(), ex)
}

fn tmp_engine(label: &str, node: &str) -> (tempfile::TempDir, SyncEngine) {
    let dir = tempfile::tempdir().unwrap();
    let _ = label;
    let engine = make_engine(dir.path().to_path_buf(), node);
    (dir, engine)
}

/// Set mtime to an exact instant (controls the 5s stability gate).
fn set_mtime(path: &Path, ms: u64) {
    let ft = filetime::FileTime::from_unix_time(
        (ms / 1000) as i64,
        ((ms % 1000) * 1_000_000) as u32,
    );
    filetime::set_file_mtime(path, ft).unwrap();
}

fn manifest_hash(engine: &SyncEngine, rel: &str) -> [u8; 32] {
    engine
        .get_manifest()
        .files
        .get(&PathBuf::from(rel))
        .unwrap_or_else(|| panic!("{rel} missing from manifest"))
        .hash
}

#[test]
fn cache_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = ManifestCache::load(dir.path());
    assert!(c.is_empty());
    c.upsert(
        PathBuf::from("a.txt"),
        CachedEntry {
            size: 12,
            mtime_s: 1_700_000_000,
            mtime_ns: 0,
            inode: 4242,
            hash: "ab".repeat(32),
        },
    );
    assert!(c.is_dirty());
    c.save_atomic(dir.path()).unwrap();
    assert!(!c.is_dirty());
    assert!(dir
        .path()
        .join(".bh_filesync")
        .join("manifest-cache.json")
        .is_file());
    let loaded = ManifestCache::load(dir.path());
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded.entries, c.entries);
}

#[test]
fn stable_file_skips_rehash() {
    // Same size + same mtime + same inode + older than 5s: the second scan
    // must reuse the cached hash WITHOUT reading the file. Proved by
    // rewriting different content under an identical stat fingerprint.
    let (dir, engine) = tmp_engine("skip", "n");
    let f = dir.path().join("f.txt");
    std::fs::write(&f, b"content-one!").unwrap();
    let old_ms = now_ms() - 10_000;
    set_mtime(&f, old_ms);
    engine.scan().unwrap();
    let h1 = manifest_hash(&engine, "f.txt");
    assert_eq!(blake3::hash(b"content-one!").to_hex().as_str(), hex(&h1));

    std::fs::write(&f, b"content-two!").unwrap(); // same 12 B, new inode? no — same file
    set_mtime(&f, old_ms); // restore identical stat fingerprint
    engine.scan().unwrap();
    let h2 = manifest_hash(&engine, "f.txt");
    assert_eq!(h1, h2, "stable stat fingerprint must reuse the cached hash");
    assert_ne!(
        blake3::hash(b"content-two!").to_hex().as_str(),
        hex(&h2),
        "test is meaningful only if content really changed"
    );
}

#[test]
fn mtime_bump_rehashes() {
    let (dir, engine) = tmp_engine("mtime", "n");
    let f = dir.path().join("f.txt");
    std::fs::write(&f, b"content-one!").unwrap();
    set_mtime(&f, now_ms() - 30_000);
    engine.scan().unwrap();
    let h1 = manifest_hash(&engine, "f.txt");

    // Same size, different content, DIFFERENT (but still old) mtime.
    std::fs::write(&f, b"content-two!").unwrap();
    set_mtime(&f, now_ms() - 20_000);
    engine.scan().unwrap();
    let h2 = manifest_hash(&engine, "f.txt");
    assert_ne!(h1, h2);
    assert_eq!(blake3::hash(b"content-two!").to_hex().as_str(), hex(&h2));
}

#[test]
fn recreate_new_inode_rehashes() {
    let (dir, engine) = tmp_engine("inode", "n");
    let f = dir.path().join("f.txt");
    let old_ms = now_ms() - 30_000;
    std::fs::write(&f, b"content-one!").unwrap();
    set_mtime(&f, old_ms);
    engine.scan().unwrap();
    let h1 = manifest_hash(&engine, "f.txt");

    // Delete + recreate: new inode, same size, same mtime, new content.
    std::fs::remove_file(&f).unwrap();
    std::fs::write(&f, b"content-two!").unwrap();
    set_mtime(&f, old_ms);
    engine.scan().unwrap();
    let h2 = manifest_hash(&engine, "f.txt");
    assert_ne!(h1, h2, "inode change must force a re-hash");
    assert_eq!(blake3::hash(b"content-two!").to_hex().as_str(), hex(&h2));
}

#[test]
fn fresh_file_age_gate_forces_rehash() {
    // A prefabricated cache entry matching the live stat EXACTLY must still
    // be ignored when the file is younger than 5s.
    let (dir, engine) = tmp_engine("gate", "n");
    let f = dir.path().join("f.txt");
    std::fs::write(&f, b"fresh content here").unwrap();
    let meta = std::fs::metadata(&f).unwrap();
    let (ms, mns) = mtime_parts(&meta).unwrap();
    let age = now_ms().saturating_sub(ms as u64 * 1000 + mns as u64 / 1_000_000);
    assert!(age <= CACHE_MIN_AGE_MS, "test file must be fresh");

    let mut c = ManifestCache::load(dir.path());
    c.upsert(
        PathBuf::from("f.txt"),
        CachedEntry {
            size: meta.len(),
            mtime_s: ms,
            mtime_ns: mns,
            inode: inode_of(&meta),
            hash: "ff".repeat(32), // bogus — must NOT surface
        },
    );
    c.save_atomic(dir.path()).unwrap();

    engine.scan().unwrap();
    let h = manifest_hash(&engine, "f.txt");
    assert_eq!(
        blake3::hash(b"fresh content here").to_hex().as_str(),
        hex(&h)
    );
    assert_ne!(hex(&h), "ff".repeat(32));
}

#[test]
fn prune_missing_drops_gone_paths() {
    let (dir, engine) = tmp_engine("prune", "n");
    std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
    // Old mtimes so both entries actually cache.
    set_mtime(&dir.path().join("a.txt"), now_ms() - 30_000);
    set_mtime(&dir.path().join("b.txt"), now_ms() - 30_000);
    engine.scan().unwrap();
    assert_eq!(ManifestCache::load(dir.path()).len(), 2);

    std::fs::remove_file(dir.path().join("b.txt")).unwrap();
    engine.scan().unwrap();
    let loaded = ManifestCache::load(dir.path());
    assert!(loaded.entries.contains_key(&PathBuf::from("a.txt")));
    assert!(!loaded.entries.contains_key(&PathBuf::from("b.txt")));
}

#[test]
fn corrupt_cache_backs_up_no_panic() {
    let dir = tempfile::tempdir().unwrap();
    let bh = dir.path().join(".bh_filesync");
    std::fs::create_dir_all(&bh).unwrap();
    std::fs::write(bh.join("manifest-cache.json"), "{ not json").unwrap();
    let c = ManifestCache::load(dir.path());
    assert!(c.is_empty());
    let backups: Vec<_> = std::fs::read_dir(&bh)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("manifest-cache.corrupt-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
}

#[test]
fn warm_scan_returns_identical_manifest() {
    let (dir, engine) = tmp_engine("warm", "n");
    std::fs::write(dir.path().join("a.txt"), b"aaa").unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/b.txt"), b"bbb").unwrap();
    engine.scan().unwrap();
    let first = engine.get_manifest();
    engine.scan().unwrap();
    let second = engine.get_manifest();
    fn proj(m: &bytehive_filesync::protocol::Manifest) -> Vec<(PathBuf, u64, [u8; 32], u64, bool)> {
        let mut v: Vec<_> = m
            .files
            .iter()
            .map(|(p, f)| (p.clone(), f.size, f.hash, f.modified_ms, f.is_dir))
            .collect();
        v.sort();
        v
    }
    assert_eq!(proj(&first), proj(&second));
    assert_eq!(first.node_id, second.node_id);
}

#[test]
fn rapid_rewrite_picks_up_new_hash() {
    // Rewrite within 5s (fresh mtime): the stability gate must re-hash.
    let (dir, engine) = tmp_engine("rapid", "n");
    let f = dir.path().join("f.txt");
    std::fs::write(&f, b"version one").unwrap();
    engine.scan().unwrap();
    let h1 = manifest_hash(&engine, "f.txt");

    std::fs::write(&f, b"version two!!").unwrap();
    engine.scan().unwrap();
    let h2 = manifest_hash(&engine, "f.txt");
    assert_ne!(h1, h2);
    assert_eq!(blake3::hash(b"version two!!").to_hex().as_str(), hex(&h2));
}

#[test]
fn unchanged_scan_skips_cache_save() {
    let (dir, engine) = tmp_engine("skip-save", "n");
    std::fs::write(dir.path().join("a.txt"), b"aaa").unwrap();
    engine.scan().unwrap();
    let cache_file = dir
        .path()
        .join(".bh_filesync")
        .join("manifest-cache.json");
    assert!(cache_file.is_file());
    let m1 = std::fs::metadata(&cache_file).unwrap().ino();
    let t1 = std::fs::metadata(&cache_file).unwrap().modified().unwrap();
    assert!(m1 > 0);
    engine.scan().unwrap();
    let t2 = std::fs::metadata(&cache_file).unwrap().modified().unwrap();
    assert_eq!(t1, t2, "warm scan with no changes must skip the save");
}

#[test]
fn dirty_set_bypasses_cache() {
    // A watcher-dirty path is re-hashed even with a matching stable entry.
    let (dir, engine) = tmp_engine("dirty", "n");
    let f = dir.path().join("f.txt");
    std::fs::write(&f, b"content-one!").unwrap();
    let old_ms = now_ms() - 30_000;
    set_mtime(&f, old_ms);
    engine.scan().unwrap();

    std::fs::write(&f, b"content-two!").unwrap();
    set_mtime(&f, old_ms); // identical fingerprint — would hit…
    let mut dirty = HashSet::new();
    dirty.insert(PathBuf::from("f.txt"));
    engine.scan_with_dirty(&dirty).unwrap(); // …but dirty bypasses
    let h = manifest_hash(&engine, "f.txt");
    assert_eq!(blake3::hash(b"content-two!").to_hex().as_str(), hex(&h));
}

#[test]
fn cache_lookup_helpers_agree_with_fs() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f.txt");
    std::fs::write(&f, b"abc").unwrap();
    let meta = std::fs::metadata(&f).unwrap();
    let (ms, mns) = mtime_parts(&meta).expect("mtime must parse");
    assert!(ms > 0 && mns < 1_000_000_000);
    assert_eq!(inode_of(&meta), meta.ino());

    let mut c = ManifestCache::new();
    c.upsert(
        PathBuf::from("f.txt"),
        CachedEntry {
            size: 3,
            mtime_s: ms,
            mtime_ns: mns,
            inode: meta.ino(),
            hash: "00".repeat(32),
        },
    );
    // Fresh file → gate denies even a perfect match.
    assert!(c
        .lookup(
            Path::new("f.txt"),
            3,
            ms,
            mns,
            meta.ino(),
            now_ms()
        )
        .is_none());
    // Same entry, far-future clock → hit.
    assert_eq!(
        c.lookup(
            Path::new("f.txt"),
            3,
            ms,
            mns,
            meta.ino(),
            (ms as u64) * 1000 + 60_000
        )
        .as_deref(),
        Some(&"00".repeat(32) as &str)
    );
    // Wrong size/inode → miss.
    assert!(c
        .lookup(Path::new("f.txt"), 4, ms, mns, meta.ino(), u64::MAX)
        .is_none());
    assert!(c
        .lookup(Path::new("f.txt"), 3, ms, mns, meta.ino() + 1, u64::MAX)
        .is_none());
}
