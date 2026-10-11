//! Unit tests for the live-action record (`alive`) in the deletion ledger.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bytehive_filesync::ledger::{now_ms, DeletionLedger, Tombstone, DELETION_LEDGER_TTL_MS};

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

#[test]
fn live_action_supersedes_older_tombstone() {
    let mut l = DeletionLedger::new();
    l.record(p("a.txt"), 100, "n".into(), None, None);
    assert!(l.record_alive(Path::new("a.txt"), 200));
    assert!(l.get(Path::new("a.txt")).is_none(), "tombstone at 100 is superseded by live 200");
    assert_eq!(l.alive_since(Path::new("a.txt")), 200);
}

#[test]
fn stale_tombstone_is_rejected_below_live_action() {
    let mut l = DeletionLedger::new();
    l.record_alive(Path::new("a.txt"), 200);
    l.record(p("a.txt"), 150, "n".into(), None, None);
    assert!(l.get(Path::new("a.txt")).is_none());
    l.record(p("a.txt"), 250, "n".into(), None, None);
    assert_eq!(l.get(Path::new("a.txt")).unwrap().deleted_at_ms, 250);
}

#[test]
fn stale_tombstone_is_rejected_when_merged_from_a_peer() {
    let mut l = DeletionLedger::new();
    l.record_alive(Path::new("a.txt"), 200);
    let mut remote = HashMap::new();
    remote.insert(
        p("a.txt"),
        Tombstone::new(p("a.txt"), 150, "peer".into(), None, None),
    );
    l.merge_remote(remote);
    assert!(l.get(Path::new("a.txt")).is_none());
}

#[test]
fn newer_ancestor_tombstone_still_covers_alive_child() {
    let mut l = DeletionLedger::new();
    l.record(p("docs"), 300, "n".into(), None, None);
    l.record_alive(Path::new("docs/a.txt"), 200);
    let cover = l
        .covering_tombstone(Path::new("docs/a.txt"))
        .expect("dir deleted after the child was recreated still covers it");
    assert_eq!(cover.path, p("docs"));
}

#[test]
fn older_ancestor_tombstone_does_not_cover_alive_child() {
    let mut l = DeletionLedger::new();
    l.record(p("docs"), 100, "n".into(), None, None);
    l.record_alive(Path::new("docs/a.txt"), 200);
    assert!(l.covering_tombstone(Path::new("docs/a.txt")).is_none());
    assert!(!l.has_newer_than_prefix(Path::new("docs/a.txt"), 150));
    // A sibling that was never recreated remains covered.
    assert!(l.covering_tombstone(Path::new("docs/b.txt")).is_some());
}

#[test]
fn record_alive_reports_change_only_when_state_changes() {
    let mut l = DeletionLedger::new();
    assert!(l.record_alive(Path::new("a.txt"), 10));
    assert!(!l.record_alive(Path::new("a.txt"), 5), "older action changes nothing");
    assert!(!l.record_alive(Path::new("a.txt"), 10), "same action changes nothing");
    assert!(l.record_alive(Path::new("a.txt"), 11));
    assert_eq!(l.alive_since(Path::new("a.txt")), 11);
}

#[test]
fn equal_stamp_tombstone_is_lifted_by_live_action() {
    let mut l = DeletionLedger::new();
    l.record(p("a.txt"), 300, "n".into(), None, None);
    l.record_alive(Path::new("a.txt"), 300);
    assert!(l.get(Path::new("a.txt")).is_none());
}

#[test]
fn alive_roundtrips_through_save_and_load() {
    let dir = tempfile::tempdir().unwrap();
    let mut l = DeletionLedger::new();
    l.record_alive(Path::new("live.txt"), 777);
    l.record(p("gone.txt"), 500, "n".into(), None, None);
    l.save_atomic(dir.path()).unwrap();
    let loaded = DeletionLedger::load(dir.path());
    assert_eq!(loaded.alive_since(Path::new("live.txt")), 777);
    assert_eq!(loaded.get(Path::new("gone.txt")).unwrap().deleted_at_ms, 500);
}

#[test]
fn ledger_file_without_alive_key_loads() {
    let dir = tempfile::tempdir().unwrap();
    let bh = dir.path().join(".bh_filesync");
    std::fs::create_dir_all(&bh).unwrap();
    std::fs::write(
        bh.join("deletion-ledger.json"),
        r#"{"entries":{"old.txt":{"path":"old.txt","deleted_at_ms":42,"deleter_node":"n","prev_hash":null,"prev_mtime_ms":null}}}"#,
    )
    .unwrap();
    let loaded = DeletionLedger::load(dir.path());
    assert_eq!(loaded.get(Path::new("old.txt")).unwrap().deleted_at_ms, 42);
    assert_eq!(loaded.alive_since(Path::new("old.txt")), 0);
}

#[test]
fn prune_drops_expired_live_records_only() {
    let now = now_ms();
    let mut l = DeletionLedger::new();
    l.record_alive(Path::new("expired.txt"), now - DELETION_LEDGER_TTL_MS - 1_000);
    l.record_alive(Path::new("fresh.txt"), now - 1_000);
    let removed = l.prune(now, DELETION_LEDGER_TTL_MS);
    assert_eq!(removed, 1);
    assert_eq!(l.alive_since(Path::new("expired.txt")), 0);
    assert_eq!(l.alive_since(Path::new("fresh.txt")), now - 1_000);
}
