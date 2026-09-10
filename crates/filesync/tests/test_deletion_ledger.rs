//! Regression tests for the filesync deletion ledger (TODO 4):
//! persistent tombstones, 90-day TTL, protocol v8, no compat shim.
//!
//! Fast, no network: two `SyncEngine`s over `tempfile` dirs simulate the
//! offline-delete / recreate races; wire roundtrips cover the v8 messages.

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

use bytehive_filesync::{
    exclusions::{ExclusionConfig, Exclusions},
    ledger::{now_ms, DeletionLedger, DELETION_LEDGER_TTL_MS},
    manifest,
    protocol::{
        read_message, serialise_message, Message, TombstoneDto, PROTOCOL_VERSION,
    },
    sync_engine::SyncEngine,
};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

fn make_engine(root: PathBuf, node: &str) -> SyncEngine {
    let ex = Arc::new(Exclusions::compile(&ExclusionConfig::default()));
    SyncEngine::new(root, node.to_string(), ex)
}

/// `(path, deleted_at_ms, deleter, prev_hash, prev_mtime)` — order-stable
/// projection for ledger-equality asserts (`TombstoneDto` has no `PartialEq`).
fn dto_key(d: &TombstoneDto) -> (PathBuf, u64, String, Option<String>, Option<u64>) {
    (
        d.path.clone(),
        d.deleted_at_ms,
        d.deleter_node.clone(),
        d.prev_hash.clone(),
        d.prev_mtime_ms,
    )
}

fn sorted_entries(engine: &SyncEngine) -> Vec<(PathBuf, u64, String, Option<String>, Option<u64>)> {
    let mut v: Vec<_> = engine.ledger_entries().iter().map(dto_key).collect();
    v.sort();
    v
}

#[test]
fn ttl_constant_is_90_days() {
    assert_eq!(DELETION_LEDGER_TTL_MS, 90 * DAY_MS);
    assert_eq!(PROTOCOL_VERSION, 8, "ledger work rode the clean break to v8");
}

/// Core resurrection repro: A deletes `f` while B is offline holding a stale
/// copy. After ledger merge, B must veto the upload, converge locally, and
/// both ledgers must agree.
#[test]
fn offline_delete_stays_deleted() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let engine_a = make_engine(dir_a.path().to_path_buf(), "node-a");
    let engine_b = make_engine(dir_b.path().to_path_buf(), "node-b");

    // Same content on both sides; B's copy goes stale (B is "offline").
    std::fs::write(dir_a.path().join("f.txt"), b"shared content").unwrap();
    std::fs::write(dir_b.path().join("f.txt"), b"shared content").unwrap();
    engine_a.scan().unwrap();
    engine_b.scan().unwrap();

    // A deletes f at T0 (after B's mtime, so the tombstone is strictly newer).
    let t0 = now_ms() + 5_000;
    let n = engine_a
        .apply_deletes(&[PathBuf::from("f.txt")], t0, "node-a")
        .unwrap();
    assert_eq!(n, 1);
    assert!(!dir_a.path().join("f.txt").exists());

    // B comes back online: merge A's ledger, then plan its upload.
    assert_eq!(engine_b.merge_ledger(engine_a.ledger_entries()), 1);
    let local_b = engine_b.get_manifest();
    let remote_a = engine_a.get_manifest();
    let send = manifest::compute_send_list(&local_b, &remote_a, false);
    assert!(
        send.contains(&PathBuf::from("f.txt")),
        "without the ledger, B would resurrect f.txt by uploading it"
    );

    let ledger_b = DeletionLedger::load(dir_b.path());
    let (to_send, recreated) =
        manifest::filter_resurrected(send, &local_b, Some(&ledger_b));
    assert!(
        !to_send.contains(&PathBuf::from("f.txt")),
        "stale copy must be vetoed by the newer tombstone"
    );
    assert!(recreated.is_empty());

    // Veto convergence (mirrors the client/server lane): delete locally with
    // the ORIGINAL stamp so the tombstone survives.
    let tomb = ledger_b
        .get(&PathBuf::from("f.txt"))
        .expect("tombstone must exist after merge");
    assert_eq!(tomb.deleted_at_ms, t0);
    engine_b
        .apply_deletes(
            &[PathBuf::from("f.txt")],
            tomb.deleted_at_ms,
            &tomb.deleter_node.clone(),
        )
        .unwrap();
    assert!(!dir_b.path().join("f.txt").exists());
    assert!(!engine_b
        .get_manifest()
        .files
        .contains_key(&PathBuf::from("f.txt")));

    // Both sides agree on the tombstone.
    assert_eq!(sorted_entries(&engine_a), sorted_entries(&engine_b));
    assert_eq!(sorted_entries(&engine_b)[0].1, t0);
    assert_eq!(sorted_entries(&engine_b)[0].2, "node-a");
}

/// Recreate wins: B's copy is newer than the tombstone (and different
/// content) → upload proceeds and the stale tombstone lifts.
#[test]
fn recreate_after_delete_wins() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let engine_a = make_engine(dir_a.path().to_path_buf(), "node-a");
    let engine_b = make_engine(dir_b.path().to_path_buf(), "node-b");

    std::fs::write(dir_a.path().join("f.txt"), b"old content").unwrap();
    engine_a.scan().unwrap();
    std::fs::write(dir_b.path().join("f.txt"), b"new content").unwrap();
    engine_b.scan().unwrap();
    let mtime_b = engine_b
        .get_manifest()
        .files
        .get(&PathBuf::from("f.txt"))
        .unwrap()
        .modified_ms;

    // Delete predates B's current copy.
    let t0 = mtime_b.saturating_sub(10_000);
    engine_a
        .apply_deletes(&[PathBuf::from("f.txt")], t0, "node-a")
        .unwrap();
    engine_b.merge_ledger(engine_a.ledger_entries());

    let local_b = engine_b.get_manifest();
    let send = vec![PathBuf::from("f.txt")];
    let ledger_b = DeletionLedger::load(dir_b.path());
    let (to_send, recreated) =
        manifest::filter_resurrected(send, &local_b, Some(&ledger_b));
    assert_eq!(to_send, vec![PathBuf::from("f.txt")]);
    assert_eq!(recreated, vec![PathBuf::from("f.txt")]);

    engine_b.clear_tombstones(&recreated);
    assert!(engine_b.ledger_entries().is_empty());
    assert!(DeletionLedger::load(dir_b.path()).is_empty());
    // The recreated file itself is untouched.
    assert_eq!(
        std::fs::read(dir_b.path().join("f.txt")).unwrap(),
        b"new content"
    );
}

/// Persistence roundtrip, 90-day TTL boundaries, corrupt-file backup.
#[test]
fn ledger_persists_and_prunes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_engine(dir.path().to_path_buf(), "node-a");

    let t = now_ms();
    engine.record_delete(
        PathBuf::from("gone.txt"),
        t,
        "node-a",
        Some("ab".repeat(32)),
        Some(t - 1_000),
    );

    // On-disk artifact exists.
    let ledger_file = dir.path().join(".bh_filesync").join("deletion-ledger.json");
    assert!(ledger_file.is_file(), "ledger must persist under .bh_filesync");

    // Save→load roundtrip preserves the tombstone.
    let loaded = DeletionLedger::load(dir.path());
    let tomb = loaded.get(&PathBuf::from("gone.txt")).unwrap();
    assert_eq!(tomb.deleted_at_ms, t);
    assert_eq!(tomb.deleter_node, "node-a");
    assert_eq!(tomb.prev_mtime_ms, Some(t - 1_000));

    // TTL boundaries: exactly 90d keeps, 90d+1ms drops, 89d keeps.
    let mut at_90d = loaded.clone();
    assert_eq!(at_90d.prune(t + 90 * DAY_MS, DELETION_LEDGER_TTL_MS), 0);
    let mut past = loaded.clone();
    assert_eq!(
        past.prune(t + 90 * DAY_MS + 1, DELETION_LEDGER_TTL_MS),
        1
    );
    assert!(past.is_empty());
    let mut before = loaded.clone();
    assert_eq!(before.prune(t + 89 * DAY_MS, DELETION_LEDGER_TTL_MS), 0);
    assert!(!before.is_empty());

    // Corrupt JSON backs up and loads empty instead of panicking.
    std::fs::write(&ledger_file, "{ this is not json").unwrap();
    let recovered = DeletionLedger::load(dir.path());
    assert!(recovered.is_empty());
    let backups: Vec<_> = std::fs::read_dir(dir.path().join(".bh_filesync"))
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

/// Protocol v8: LedgerExchange encode/decode (Delete shape already covered
/// in test_protocol.rs; re-asserted here for the no-compat story).
#[test]
fn ledger_exchange_roundtrip() {
    let msg = Message::LedgerExchange {
        entries: vec![
            TombstoneDto {
                path: PathBuf::from("gone.txt"),
                deleted_at_ms: 1_700_000_000_000,
                deleter_node: "node-a".to_string(),
                prev_hash: Some("ab".repeat(32)),
                prev_mtime_ms: Some(1_699_999_999_000),
            },
            TombstoneDto {
                path: PathBuf::from("old/dir"),
                deleted_at_ms: 42,
                deleter_node: "node-b".to_string(),
                prev_hash: None,
                prev_mtime_ms: None,
            },
        ],
    };
    let frame = serialise_message(&msg).unwrap();
    let mut cur = Cursor::new(frame);
    let expected_prev = "ab".repeat(32);
    match read_message(&mut cur).unwrap() {
        Message::LedgerExchange { entries } => {
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].path, PathBuf::from("gone.txt"));
            assert_eq!(entries[0].deleted_at_ms, 1_700_000_000_000);
            assert_eq!(entries[0].deleter_node, "node-a");
            assert_eq!(entries[0].prev_hash.as_deref(), Some(expected_prev.as_str()));
            assert_eq!(entries[0].prev_mtime_ms, Some(1_699_999_999_000));
            assert_eq!(entries[1].path, PathBuf::from("old/dir"));
            assert_eq!(entries[1].prev_hash, None);
        }
        _ => panic!("expected LedgerExchange"),
    }
}

#[test]
fn v8_delete_shape_roundtrip() {
    let msg = Message::Delete {
        paths: vec![PathBuf::from("f.txt")],
        deleted_at_ms: 1_700_000_000_001,
        deleter: "node-a".to_string(),
    };
    let frame = serialise_message(&msg).unwrap();
    let mut cur = Cursor::new(frame);
    match read_message(&mut cur).unwrap() {
        Message::Delete {
            paths,
            deleted_at_ms,
            deleter,
        } => {
            assert_eq!(paths, vec![PathBuf::from("f.txt")]);
            assert_eq!(deleted_at_ms, 1_700_000_000_001);
            assert_eq!(deleter, "node-a");
        }
        _ => panic!("expected Delete"),
    }
}

/// Tombstone payload survives the engine DTO export used by the exchange.
#[test]
fn ledger_dto_export_preserves_prev_info() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_engine(dir.path().to_path_buf(), "node-a");
    std::fs::write(dir.path().join("f.txt"), b"content").unwrap();
    engine.scan().unwrap();
    let meta = engine
        .get_manifest()
        .files
        .get(&PathBuf::from("f.txt"))
        .unwrap()
        .clone();

    let t0 = now_ms() + 5_000;
    engine
        .apply_deletes(&[PathBuf::from("f.txt")], t0, "node-a")
        .unwrap();
    let entries = engine.ledger_entries();
    assert_eq!(entries.len(), 1);
    let expected_hash = bytehive_filesync::hex(&meta.hash);
    assert_eq!(entries[0].prev_hash.as_deref(), Some(expected_hash.as_str()));
    assert_eq!(entries[0].prev_mtime_ms, Some(meta.modified_ms));

    // And the DTOs feed back through merge on a second engine unchanged.
    let dir_b = tempfile::tempdir().unwrap();
    let engine_b = make_engine(dir_b.path().to_path_buf(), "node-b");
    assert_eq!(engine_b.merge_ledger(entries), 1);
    let got = sorted_entries(&engine_b);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, PathBuf::from("f.txt"));
    assert_eq!(got[0].1, t0);
    assert_eq!(got[0].2, "node-a");
    assert_eq!(got[0].3.as_deref(), Some(expected_hash.as_str()));
    assert_eq!(got[0].4, Some(meta.modified_ms));
}
