//! Unit tests for [`bytehive_filesync::ledger`] relocated from `src/ledger.rs`
//! (repo policy: no inline `#[cfg(test)]` in implementation files).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bytehive_filesync::ledger::{
    now_ms, DeletionLedger, Tombstone, LEDGER_DIR_NAME, LEDGER_FILE_NAME,
};

fn tomb(path: &str, at: u64) -> Tombstone {
    Tombstone::new(PathBuf::from(path), at, "node-a".to_string(), None, None)
}

#[test]
fn record_keeps_newest() {
    let mut l = DeletionLedger::new();
    l.record(PathBuf::from("a.txt"), 100, "n1".to_string(), None, None);
    l.record(PathBuf::from("a.txt"), 50, "n2".to_string(), None, None);
    assert_eq!(l.get(Path::new("a.txt")).unwrap().deleted_at_ms, 100);
    l.record(PathBuf::from("a.txt"), 200, "n2".to_string(), None, None);
    assert_eq!(l.get(Path::new("a.txt")).unwrap().deleted_at_ms, 200);
}

#[test]
fn has_newer_than_strict() {
    let mut l = DeletionLedger::new();
    assert!(!l.has_newer_than(Path::new("a.txt"), 10));
    l.record(PathBuf::from("a.txt"), 100, "n1".to_string(), None, None);
    assert!(l.has_newer_than(Path::new("a.txt"), 99));
    assert!(!l.has_newer_than(Path::new("a.txt"), 100));
    assert!(!l.has_newer_than(Path::new("a.txt"), 101));
}

#[test]
fn merge_remote_lww() {
    let mut local = DeletionLedger::new();
    local.record(PathBuf::from("a.txt"), 100, "n1".to_string(), None, None);
    let mut remote = HashMap::new();
    remote.insert(PathBuf::from("a.txt"), tomb("a.txt", 50));
    remote.insert(PathBuf::from("b.txt"), tomb("b.txt", 300));
    local.merge_remote(remote);
    assert_eq!(local.get(Path::new("a.txt")).unwrap().deleted_at_ms, 100);
    assert_eq!(local.get(Path::new("b.txt")).unwrap().deleted_at_ms, 300);
}

#[test]
fn prune_ttl() {
    let mut l = DeletionLedger::new();
    l.record(PathBuf::from("old.txt"), 0, "n".to_string(), None, None);
    l.record(
        PathBuf::from("new.txt"),
        1_000,
        "n".to_string(),
        None,
        None,
    );
    let removed = l.prune(1_000 + 10, 100);
    assert_eq!(removed, 1);
    assert!(l.get(Path::new("old.txt")).is_none());
    assert!(l.get(Path::new("new.txt")).is_some());
}

#[test]
fn remove_clears_tombstone() {
    let mut l = DeletionLedger::new();
    l.record(PathBuf::from("a.txt"), 5, "n".to_string(), None, None);
    assert!(l.remove(Path::new("a.txt")).is_some());
    assert!(!l.has_newer_than(Path::new("a.txt"), 0));
}

#[test]
fn save_load_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let mut l = DeletionLedger::new();
    l.record(
        PathBuf::from("a.txt"),
        123,
        "node-x".to_string(),
        Some("abc".to_string()),
        Some(100),
    );
    l.save_atomic(dir.path()).unwrap();
    let loaded = DeletionLedger::load(dir.path());
    let t = loaded.get(Path::new("a.txt")).unwrap();
    assert_eq!(t.deleted_at_ms, 123);
    assert_eq!(t.deleter_node, "node-x");
    assert_eq!(t.prev_hash.as_deref(), Some("abc"));
    assert_eq!(t.prev_mtime_ms, Some(100));
}

#[test]
fn load_missing_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let l = DeletionLedger::load(dir.path());
    assert!(l.is_empty());
}

#[test]
fn load_corrupt_backs_up_and_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let bh = dir.path().join(LEDGER_DIR_NAME);
    std::fs::create_dir_all(&bh).unwrap();
    std::fs::write(bh.join(LEDGER_FILE_NAME), "{ not json").unwrap();
    let l = DeletionLedger::load(dir.path());
    assert!(l.is_empty());
    let backups: Vec<_> = std::fs::read_dir(&bh)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("deletion-ledger.corrupt-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
}

#[test]
fn now_ms_is_sane() {
    // Smoke-cover the clock helper (also guards against future removal).
    let t = now_ms();
    assert!(t > 1_700_000_000_000, "wall clock must be post-2023");
}
