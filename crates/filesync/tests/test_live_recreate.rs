//! Live-action admission and action-journal drain, exercised engine-to-engine.
//!
//! `apply_wire` mirrors the server-side application of a drained or live
//! message (server.rs `apply_client_action`), so these tests cover the drain
//! contract without sockets. The network path is covered separately in
//! `test_action_drain_e2e.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytehive_filesync::common;
use bytehive_filesync::exclusions::{ExclusionConfig, Exclusions};
use bytehive_filesync::ledger::now_ms;
use bytehive_filesync::protocol::{FileBundle, FileData, FileMetadata, Message, TombstoneDto};
use bytehive_filesync::sync_engine::SyncEngine;
use bytehive_filesync::client::{drain_action_journal, DrainStats};

const PEER: &str = "cli-peer";

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "bh_live_{name}_{:x}",
        bytehive_filesync::timestamp_id()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn engine(root: &Path, node: &str) -> Arc<SyncEngine> {
    let ex = Arc::new(Exclusions::compile(&ExclusionConfig::default()));
    Arc::new(SyncEngine::new(root.to_path_buf(), node.to_string(), ex))
}

fn write(root: &Path, rel: &str, body: &[u8], mtime_ms: u64) {
    let full = root.join(rel);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&full, body).unwrap();
    filetime::set_file_mtime(
        &full,
        filetime::FileTime::from_unix_time(
            (mtime_ms / 1000) as i64,
            ((mtime_ms % 1000) * 1_000_000) as u32,
        ),
    )
    .unwrap();
}

fn has(root: &Path, rel: &str) -> bool {
    root.join(rel).exists()
}

fn read(root: &Path, rel: &str) -> Vec<u8> {
    std::fs::read(root.join(rel)).unwrap()
}

fn apply_wire(server: &SyncEngine, msg: Message) {
    match msg {
        Message::Bundle(b) => {
            common::handle_recv_bundle(server, &b, PEER, &None, "test").unwrap();
        }
        Message::LargeFileStart {
            metadata,
            total_chunks,
            action_ms,
        } => {
            server
                .begin_large_file_with_action(metadata, total_chunks, action_ms)
                .unwrap();
        }
        Message::LargeFileChunk {
            path,
            chunk_index,
            data,
        } => {
            server
                .receive_large_file_chunk(&path, chunk_index, &data)
                .unwrap();
        }
        Message::LargeFileEnd { path, final_hash } => {
            server.finish_large_file(&path, final_hash).unwrap();
        }
        Message::Delete {
            paths,
            deleted_at_ms,
            deleter,
        } => {
            common::handle_recv_delete_with_meta(
                server,
                &paths,
                deleted_at_ms,
                &deleter,
                PEER,
                &None,
                "test",
            )
            .unwrap();
        }
        Message::Rename {
            from,
            to,
            action_ms,
        } => {
            common::handle_recv_rename(server, &from, &to, action_ms, PEER, &None, "test")
                .unwrap();
        }
        Message::ActionBatchEnd { .. } => {}
        other => panic!("unexpected wire message: {other:?}"),
    }
}

/// Drain `client` into `server` (acks immediately). Returns stats and the
/// messages the client sent, in order.
fn drain_into(client: &Arc<SyncEngine>, server: &SyncEngine) -> (DrainStats, Vec<Message>) {
    let mut sent: Vec<Message> = Vec::new();
    let stats = {
        let mut send = |m: Message| {
            sent.push(m.clone());
            apply_wire(server, m);
            Ok(())
        };
        let mut ack = |upto: u64| Ok(upto);
        drain_action_journal(client, &mut send, &mut ack).unwrap()
    };
    (stats, sent)
}

fn bundles_files(msgs: &[Message]) -> Vec<&FileData> {
    msgs.iter()
        .filter_map(|m| match m {
            Message::Bundle(b) => Some(b.files.iter()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn tomb_of(e: &SyncEngine, rel: &str) -> Option<(u64, String)> {
    e.ledger_entries()
        .into_iter()
        .find(|d: &TombstoneDto| !d.alive && d.path == PathBuf::from(rel))
        .map(|d| (d.deleted_at_ms, d.deleter_node))
}

fn bundle_of(rel: &str, body: &[u8], mtime_ms: u64, action_ms: Option<u64>) -> FileBundle {
    FileBundle {
        files: vec![FileData {
            metadata: FileMetadata {
                rel_path: PathBuf::from(rel),
                size: body.len() as u64,
                hash: blake3::hash(body).into(),
                modified_ms: mtime_ms,
                change_sequence: 0,
                is_dir: false,
            },
            content: body.to_vec(),
            action_ms,
        }],
        bundle_id: 1,
    }
}

const OLD_MTIME: u64 = 100_000;

#[test]
fn drained_action_older_than_tombstone_stays_deleted() {
    let srv_dir = tmp("older_srv");
    let cli_dir = tmp("older_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    server.record_delete(PathBuf::from("f.txt"), 1_000_000, "srv", None, None);
    write(&cli_dir, "f.txt", b"stale copy", OLD_MTIME);
    client.journal().upsert(PathBuf::from("f.txt"), 500_000).unwrap();
    client.merge_ledger(server.ledger_entries());

    let (stats, sent) = drain_into(&client, &server);
    assert!(bundles_files(&sent).is_empty(), "stale copy must not be sent");
    assert_eq!(stats.files_sent, 0);
    assert!(!has(&srv_dir, "f.txt"), "server stays deleted");
    assert!(!has(&cli_dir, "f.txt"), "client's stale copy moved to trash");
    assert_eq!(tomb_of(&server, "f.txt").map(|t| t.0), Some(1_000_000));
    assert!(client.journal().is_empty());
}

#[test]
fn drained_action_newer_than_tombstone_is_accepted() {
    let srv_dir = tmp("newer_srv");
    let cli_dir = tmp("newer_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    server.record_delete(PathBuf::from("f.txt"), 500_000, "srv", None, None);
    write(&cli_dir, "f.txt", b"cp -p copy", OLD_MTIME);
    client.journal().upsert(PathBuf::from("f.txt"), 600_000).unwrap();
    client.merge_ledger(server.ledger_entries());

    let (_, sent) = drain_into(&client, &server);
    let files = bundles_files(&sent);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].action_ms, Some(600_000));
    assert_eq!(read(&srv_dir, "f.txt"), b"cp -p copy");
    assert!(tomb_of(&server, "f.txt").is_none(), "tombstone superseded");
    assert_eq!(server.ledger_snapshot().alive_since(Path::new("f.txt")), 600_000);
    assert_eq!(client.ledger_snapshot().alive_since(Path::new("f.txt")), 600_000);
}

#[test]
fn drained_action_equal_to_tombstone_is_accepted() {
    let srv_dir = tmp("equal_srv");
    let cli_dir = tmp("equal_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    server.record_delete(PathBuf::from("f.txt"), 500_000, "srv", None, None);
    write(&cli_dir, "f.txt", b"same stamp", OLD_MTIME);
    client.journal().upsert(PathBuf::from("f.txt"), 500_000).unwrap();
    client.merge_ledger(server.ledger_entries());

    drain_into(&client, &server);
    assert_eq!(read(&srv_dir, "f.txt"), b"same stamp");
}

#[test]
fn late_delete_older_than_accepted_action_is_ignored() {
    let srv_dir = tmp("late_srv");
    let server = engine(&srv_dir, "srv");

    server.record_delete(PathBuf::from("f.txt"), 500_000, "srv", None, None);
    let bundle = bundle_of("f.txt", b"recreated", OLD_MTIME, Some(600_000));
    common::handle_recv_bundle(&server, &bundle, PEER, &None, "test").unwrap();
    assert_eq!(read(&srv_dir, "f.txt"), b"recreated");

    // A delete that was issued before the recreate arrives late.
    common::handle_recv_delete_with_meta(
        &server,
        &[PathBuf::from("f.txt")],
        550_000,
        "third",
        "third",
        &None,
        "test",
    )
    .unwrap();
    assert_eq!(read(&srv_dir, "f.txt"), b"recreated", "late delete must not remove it");
    assert!(tomb_of(&server, "f.txt").is_none(), "no tombstone for the late delete");

    // A delete newer than the recreate still applies.
    common::handle_recv_delete_with_meta(
        &server,
        &[PathBuf::from("f.txt")],
        700_000,
        "third",
        "third",
        &None,
        "test",
    )
    .unwrap();
    assert!(!has(&srv_dir, "f.txt"));
}

#[test]
fn action_without_prior_tombstone_records_live_action() {
    let srv_dir = tmp("noTomb_srv");
    let cli_dir = tmp("noTomb_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    write(&cli_dir, "f.txt", b"fresh", OLD_MTIME);
    client.journal().upsert(PathBuf::from("f.txt"), 42_000).unwrap();
    drain_into(&client, &server);

    assert_eq!(read(&srv_dir, "f.txt"), b"fresh");
    assert_eq!(server.ledger_snapshot().alive_since(Path::new("f.txt")), 42_000);
    assert_eq!(client.ledger_snapshot().alive_since(Path::new("f.txt")), 42_000);
}

#[test]
fn offline_delete_is_captured_and_pushed_on_reconnect() {
    // Regression: an offline delete used to be lost (never pushed).
    let srv_dir = tmp("offdel_srv");
    let cli_dir = tmp("offdel_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    write(&srv_dir, "f.txt", b"shared", OLD_MTIME);
    write(&cli_dir, "f.txt", b"shared", OLD_MTIME);
    server.scan().unwrap();
    client.scan().unwrap();

    let t = now_ms();
    std::fs::remove_file(cli_dir.join("f.txt")).unwrap();
    client.journal().delete(PathBuf::from("f.txt"), t).unwrap();

    drain_into(&client, &server);
    assert!(!has(&srv_dir, "f.txt"), "server must apply the offline delete");
    assert_eq!(tomb_of(&server, "f.txt").map(|x| x.0), Some(t));
    assert_eq!(tomb_of(&client, "f.txt").map(|x| x.0), Some(t));
}

#[test]
fn offline_delete_then_edit_keeps_new_content() {
    let srv_dir = tmp("delEdit_srv");
    let cli_dir = tmp("delEdit_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    write(&srv_dir, "f.txt", b"old", OLD_MTIME);
    write(&cli_dir, "f.txt", b"old", OLD_MTIME);
    server.scan().unwrap();
    client.scan().unwrap();

    let t1 = now_ms() - 2_000;
    let t2 = now_ms() - 1_000;
    client.journal().delete(PathBuf::from("f.txt"), t1).unwrap();
    write(&cli_dir, "f.txt", b"new", t2);
    client.journal().upsert(PathBuf::from("f.txt"), t2).unwrap();
    assert_eq!(client.journal().len(), 1, "delete is superseded by the recreate");

    drain_into(&client, &server);
    assert_eq!(read(&srv_dir, "f.txt"), b"new");
}

#[test]
fn offline_edit_then_delete_deletes() {
    let srv_dir = tmp("editDel_srv");
    let cli_dir = tmp("editDel_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    write(&srv_dir, "f.txt", b"old", OLD_MTIME);
    write(&cli_dir, "f.txt", b"old", OLD_MTIME);
    server.scan().unwrap();
    client.scan().unwrap();

    client.journal().upsert(PathBuf::from("f.txt"), now_ms() - 2_000).unwrap();
    std::fs::remove_file(cli_dir.join("f.txt")).unwrap();
    client.journal().delete(PathBuf::from("f.txt"), now_ms() - 1_000).unwrap();

    drain_into(&client, &server);
    assert!(!has(&srv_dir, "f.txt"));
}

#[test]
fn drain_sends_current_bytes_not_capture_time_bytes() {
    let srv_dir = tmp("current_srv");
    let cli_dir = tmp("current_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    write(&cli_dir, "f.txt", b"at capture", OLD_MTIME);
    client.journal().upsert(PathBuf::from("f.txt"), 9_000).unwrap();
    write(&cli_dir, "f.txt", b"edited later", OLD_MTIME + 1);

    drain_into(&client, &server);
    assert_eq!(read(&srv_dir, "f.txt"), b"edited later");
}

#[test]
fn partial_ack_then_crash_resends_only_unacked_chunk() {
    let srv_dir = tmp("partial_srv");
    let cli_dir = tmp("partial_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    const N: usize = 300;
    for i in 0..N {
        let rel = format!("d/f{i:03}.txt");
        write(&cli_dir, &rel, format!("body {i}").as_bytes(), OLD_MTIME);
        client
            .journal()
            .upsert(PathBuf::from(&rel), 10_000 + i as u64)
            .unwrap();
    }

    // First chunk is acknowledged, the second is not (crash before ack).
    let mut acks = 0usize;
    {
        let mut sent = Vec::new();
        let mut send = |m: Message| {
            apply_wire(&server, m.clone());
            sent.push(m);
            Ok(())
        };
        let mut ack = |upto: u64| {
            acks += 1;
            if acks == 1 {
                Ok(upto)
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "crash"))
            }
        };
        assert!(drain_action_journal(&client, &mut send, &mut ack).is_err());
    }
    let remaining = client.journal().len();
    assert_eq!(remaining, N - 256, "only the first chunk was acked");

    // Restart: journal replays from disk; the unacked chunk is resent.
    drop(client);
    let client = engine(&cli_dir, "cli");
    assert_eq!(client.journal().len(), remaining);
    let (_, sent) = drain_into(&client, &server);
    assert_eq!(bundles_files(&sent).len(), remaining);
    for i in 0..N {
        assert_eq!(read(&srv_dir, &format!("d/f{i:03}.txt")), format!("body {i}").as_bytes());
    }
    assert!(client.journal().is_empty());
}

#[test]
fn crash_before_ack_is_idempotent_no_conflict_copies() {
    let srv_dir = tmp("idem_srv");
    let cli_dir = tmp("idem_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    write(&srv_dir, "f.txt", b"server version", OLD_MTIME);
    server.scan().unwrap();
    write(&cli_dir, "f.txt", b"client version", OLD_MTIME + 5_000);
    client.journal().upsert(PathBuf::from("f.txt"), OLD_MTIME + 5_000).unwrap();

    {
        let mut send = |m: Message| {
            apply_wire(&server, m);
            Ok(())
        };
        let mut ack = |_upto: u64| {
            Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "crash"))
        };
        assert!(drain_action_journal(&client, &mut send, &mut ack).is_err());
    }
    assert_eq!(read(&srv_dir, "f.txt"), b"client version");

    // Resend after the crash: identical content must not create a conflict copy.
    drain_into(&client, &server);
    let conflicts: Vec<_> = std::fs::read_dir(&srv_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("conflict"))
        .collect();
    assert!(conflicts.is_empty(), "idempotent resend must not conflict");
    assert_eq!(read(&srv_dir, "f.txt"), b"client version");
}

#[test]
fn crash_mid_rename_converges_on_resend() {
    let srv_dir = tmp("rename_srv");
    let cli_dir = tmp("rename_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    write(&srv_dir, "a.txt", b"moving", OLD_MTIME);
    server.scan().unwrap();
    write(&cli_dir, "b.txt", b"moving", OLD_MTIME);
    client.scan().unwrap();
    client
        .journal()
        .rename(PathBuf::from("a.txt"), PathBuf::from("b.txt"), 50_000)
        .unwrap();

    // Applied on the server, but the ack is lost.
    {
        let mut send = |m: Message| {
            apply_wire(&server, m);
            Ok(())
        };
        let mut ack = |_upto: u64| {
            Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "crash"))
        };
        assert!(drain_action_journal(&client, &mut send, &mut ack).is_err());
    }
    assert!(!has(&srv_dir, "a.txt"));
    assert!(has(&srv_dir, "b.txt"));

    // Resend: the source is already gone; the rename converges without error.
    drain_into(&client, &server);
    assert!(!has(&srv_dir, "a.txt"));
    assert_eq!(read(&srv_dir, "b.txt"), b"moving");
    assert!(client.journal().is_empty());
}

#[test]
fn two_peers_converge_after_drain() {
    let srv_dir = tmp("conv_srv");
    let cli_dir = tmp("conv_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    for (rel, body) in [("keep.txt", &b"k"[..]), ("gone.txt", b"g"), ("mv.txt", b"m")] {
        write(&srv_dir, rel, body, OLD_MTIME);
        write(&cli_dir, rel, body, OLD_MTIME);
    }
    server.scan().unwrap();
    client.scan().unwrap();

    let t = now_ms();
    write(&cli_dir, "new.txt", b"n", t - 3_000);
    client.journal().upsert(PathBuf::from("new.txt"), t - 3_000).unwrap();
    std::fs::remove_file(cli_dir.join("gone.txt")).unwrap();
    client.journal().delete(PathBuf::from("gone.txt"), t - 2_000).unwrap();
    std::fs::rename(cli_dir.join("mv.txt"), cli_dir.join("mv2.txt")).unwrap();
    client
        .journal()
        .rename(PathBuf::from("mv.txt"), PathBuf::from("mv2.txt"), t - 1_000)
        .unwrap();

    drain_into(&client, &server);
    server.scan().unwrap();
    client.scan().unwrap();

    let files = |root: &Path| {
        let mut v: Vec<(String, Vec<u8>)> = std::fs::read_dir(root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != ".bh_filesync")
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read(e.path()).unwrap(),
                )
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(files(&srv_dir), files(&cli_dir));
    assert!(!has(&srv_dir, "gone.txt"));
    assert!(has(&srv_dir, "mv2.txt") && !has(&srv_dir, "mv.txt"));

    let key = |e: &TombstoneDto| (e.path.clone(), e.deleted_at_ms, e.alive);
    let mut a: Vec<_> = server.ledger_entries().iter().map(key).collect();
    let mut b: Vec<_> = client.ledger_entries().iter().map(key).collect();
    a.sort();
    b.sort();
    assert_eq!(a, b, "ledgers must agree after the drain");
}

#[test]
fn relay_stamp_is_max_of_live_action_and_mtime() {
    let root = tmp("stamp");
    let e = engine(&root, "srv");
    write(&root, "f.txt", b"relay", 100);

    assert_eq!(
        e.create_bundles(&[PathBuf::from("f.txt")])[0].files[0].action_ms,
        None,
        "no live action recorded: catch-up semantics"
    );

    e.record_live_actions(&[(PathBuf::from("f.txt"), 500)]);
    assert_eq!(
        e.create_bundles(&[PathBuf::from("f.txt")])[0].files[0].action_ms,
        Some(500)
    );

    write(&root, "f.txt", b"relay", 900);
    assert_eq!(
        e.create_bundles(&[PathBuf::from("f.txt")])[0].files[0].action_ms,
        Some(900),
        "a newer mtime than the live action wins"
    );
}

#[test]
fn unstable_file_stops_drain_at_its_sequence() {
    let srv_dir = tmp("unstable_srv");
    let cli_dir = tmp("unstable_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    write(&cli_dir, "a.txt", b"a", OLD_MTIME);
    write(&cli_dir, "b.txt", b"b", now_ms());
    write(&cli_dir, "c.txt", b"c", OLD_MTIME);
    let j = client.journal();
    j.upsert(PathBuf::from("a.txt"), 1_000).unwrap();
    j.upsert(PathBuf::from("b.txt"), 2_000).unwrap();
    j.upsert(PathBuf::from("c.txt"), 3_000).unwrap();
    let a_seq = j.snapshot()[0].seq;

    let (stats, _) = drain_into(&client, &server);
    assert!(stats.stopped_unstable);
    assert_eq!(stats.acked_upto, a_seq);
    assert!(has(&srv_dir, "a.txt"));
    assert!(!has(&srv_dir, "b.txt"));
    assert!(!has(&srv_dir, "c.txt"), "entries after the unstable file wait");
    assert_eq!(client.journal().len(), 2);
}

#[test]
fn catch_up_with_old_mtime_is_still_vetoed() {
    let srv_dir = tmp("catchup_srv");
    let server = engine(&srv_dir, "srv");
    server.record_delete(PathBuf::from("f.txt"), 1_000_000, "srv", None, None);

    let bundle = bundle_of("f.txt", b"stale", 500_000, None);
    common::handle_recv_bundle(&server, &bundle, PEER, &None, "test").unwrap();
    assert!(!has(&srv_dir, "f.txt"));
}

#[test]
fn live_large_file_with_old_mtime_is_accepted() {
    let srv_dir = tmp("lf_srv");
    let server = engine(&srv_dir, "srv");
    server.record_delete(PathBuf::from("big.bin"), 1_000_000, "srv", None, None);

    let body = b"hello";
    let meta = FileMetadata {
        rel_path: PathBuf::from("big.bin"),
        size: body.len() as u64,
        hash: blake3::hash(body).into(),
        modified_ms: 500_000,
        change_sequence: 0,
        is_dir: false,
    };
    server
        .begin_large_file_with_action(meta.clone(), 1, Some(2_000_000))
        .unwrap();
    server.receive_large_file_chunk(&PathBuf::from("big.bin"), 0, body).unwrap();
    server
        .finish_large_file(&PathBuf::from("big.bin"), meta.hash)
        .unwrap();
    assert_eq!(read(&srv_dir, "big.bin"), body);
    assert!(tomb_of(&server, "big.bin").is_none());
}

#[test]
fn catch_up_large_file_with_old_mtime_is_vetoed() {
    let srv_dir = tmp("lf_catch_srv");
    let server = engine(&srv_dir, "srv");
    server.record_delete(PathBuf::from("big.bin"), 1_000_000, "srv", None, None);

    let body = b"hello";
    let meta = FileMetadata {
        rel_path: PathBuf::from("big.bin"),
        size: body.len() as u64,
        hash: blake3::hash(body).into(),
        modified_ms: 500_000,
        change_sequence: 0,
        is_dir: false,
    };
    server.begin_large_file_with_action(meta.clone(), 1, None).unwrap();
    let _ = server.receive_large_file_chunk(&PathBuf::from("big.bin"), 0, body);
    server
        .finish_large_file(&PathBuf::from("big.bin"), meta.hash)
        .unwrap();
    assert!(!has(&srv_dir, "big.bin"));
}

#[test]
fn live_rename_into_tombstoned_path_is_accepted() {
    let srv_dir = tmp("ren_ok_srv");
    let server = engine(&srv_dir, "srv");
    write(&srv_dir, "src.txt", b"moved in", OLD_MTIME);
    server.scan().unwrap();
    server.record_delete(PathBuf::from("dst.txt"), 1_000_000, "srv", None, None);

    common::handle_recv_rename(
        &server,
        &PathBuf::from("src.txt"),
        &PathBuf::from("dst.txt"),
        2_000_000,
        PEER,
        &None,
        "test",
    )
    .unwrap();
    assert_eq!(read(&srv_dir, "dst.txt"), b"moved in");
    assert!(!has(&srv_dir, "src.txt"));
    assert!(tomb_of(&server, "dst.txt").is_none());
}

#[test]
fn rename_into_newer_tombstone_deletes_source_instead() {
    let srv_dir = tmp("ren_veto_srv");
    let server = engine(&srv_dir, "srv");
    write(&srv_dir, "src.txt", b"stale move", OLD_MTIME);
    server.scan().unwrap();
    server.record_delete(PathBuf::from("dst.txt"), 5_000_000, "srv", None, None);

    common::handle_recv_rename(
        &server,
        &PathBuf::from("src.txt"),
        &PathBuf::from("dst.txt"),
        1_000_000,
        PEER,
        &None,
        "test",
    )
    .unwrap();
    assert!(!has(&srv_dir, "dst.txt"), "vetoed destination is not created");
    assert!(!has(&srv_dir, "src.txt"), "source deleted as the rename's effect");
    assert_eq!(tomb_of(&server, "src.txt").map(|t| t.0), Some(1_000_000));
}

#[test]
fn clock_skew_future_tombstone_loses_to_later_live_action() {
    // Stamps within the peer-skew allowance (see MAX_FUTURE_SKEW_MS) keep the
    // old ordering rule; anything further in the future is clamped at
    // ingress (far_future_peer_stamps_are_clamped_to_now).
    let srv_dir = tmp("skew_srv");
    let server = engine(&srv_dir, "srv");
    let now = now_ms();
    server.record_delete(PathBuf::from("f.txt"), now + 60_000, "peer", None, None);

    // Action stamped before the tombstone is vetoed.
    let early = bundle_of("f.txt", b"early", now, Some(now));
    common::handle_recv_bundle(&server, &early, PEER, &None, "test").unwrap();
    assert!(!has(&srv_dir, "f.txt"));

    // Action stamped after the (near-future) tombstone wins.
    let late = bundle_of("f.txt", b"late", now, Some(now + 120_000));
    common::handle_recv_bundle(&server, &late, PEER, &None, "test").unwrap();
    assert_eq!(read(&srv_dir, "f.txt"), b"late");
}

#[test]
fn captured_delete_does_not_remove_a_locally_recreated_file() {
    let srv_dir = tmp("recreated_srv");
    let cli_dir = tmp("recreated_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    write(&srv_dir, "f.txt", b"server copy", OLD_MTIME);
    server.scan().unwrap();
    client.journal().delete(PathBuf::from("f.txt"), now_ms() - 1_000).unwrap();
    write(&cli_dir, "f.txt", b"recreated locally", OLD_MTIME + 1);

    let (_, sent) = drain_into(&client, &server);
    assert!(sent.iter().all(|m| !matches!(m, Message::Delete { .. })));
    assert_eq!(read(&srv_dir, "f.txt"), b"server copy");
    assert_eq!(read(&cli_dir, "f.txt"), b"recreated locally");
    assert!(client.journal().is_empty());
}

#[test]
fn partial_ack_keeps_unacked_entries_and_redelivers() {
    // Regression for the ack-then-forget ship-blocker: the server used to
    // ack every `ActionBatchEnd` even when an apply failed mid-chunk, so the
    // client deleted journal entries the server never stored. The server now
    // acks only the contiguous-applied prefix; the client must keep
    // everything above that prefix and redeliver it on retry.
    let srv_dir = tmp("pack_srv");
    let cli_dir = tmp("pack_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    for (rel, body) in [("a.txt", &b"aaa"[..]), ("b.txt", b"bbb"), ("c.txt", b"ccc")] {
        write(&cli_dir, rel, body, OLD_MTIME);
        client.journal().upsert(PathBuf::from(rel), OLD_MTIME).unwrap();
    }
    let seqs: Vec<u64> = client.journal().snapshot().iter().map(|e| e.seq).collect();
    assert_eq!(seqs.len(), 3);

    // First pass: all three entries fit one chunk, but the peer only applied
    // the first and acks that prefix (mid-chunk failure simulation).
    {
        let mut send = |m: Message| {
            if !matches!(m, Message::ActionBatchEnd { .. }) {
                apply_wire(&server, m);
            }
            Ok(())
        };
        let mut ack = |upto: u64| {
            assert!(upto >= seqs[2]);
            Ok(seqs[0])
        };
        let stats = drain_action_journal(&client, &mut send, &mut ack).unwrap();
        assert_eq!(stats.acked_upto, seqs[0], "partial ack advances only to last-ok");
        assert_eq!(stats.settled_entries, 1);
    }
    assert_eq!(client.journal().len(), 2, "unacked entries are NOT forgotten");

    // Retry: the remainder is redelivered from last-ok + 1 and fully acked.
    let (stats, _) = drain_into(&client, &server);
    assert_eq!(stats.settled_entries, 2);
    assert!(client.journal().is_empty());
    for (rel, body) in [("a.txt", &b"aaa"[..]), ("b.txt", b"bbb"), ("c.txt", b"ccc")] {
        assert_eq!(read(&srv_dir, rel), body);
    }
}

#[test]
fn missing_chunks_end_does_not_advance_ack_and_redelivers() {
    // A `LargeFileEnd` with chunks missing must NOT advance the ack past the
    // incomplete file (server still sends `RequestChunks`); the entry stays
    // journalled and the retry delivers it.
    use bytehive_filesync::protocol::LARGE_FILE_THRESHOLD;

    let srv_dir = tmp("missack_srv");
    let cli_dir = tmp("missack_cli");
    let server = engine(&srv_dir, "srv");
    let client = engine(&cli_dir, "cli");

    let big = vec![0xABu8; LARGE_FILE_THRESHOLD as usize + 1024];
    write(&cli_dir, "big.bin", &big, OLD_MTIME);
    client.journal().upsert(PathBuf::from("big.bin"), now_ms()).unwrap();

    // Pass 1: chunks are lost on the wire; the end reports MissingChunks.
    let mut saw_end = false;
    {
        let mut send = |m: Message| {
            match m {
                Message::LargeFileChunk { .. } => { /* dropped on the wire */ }
                Message::LargeFileEnd { path, final_hash } => {
                    saw_end = true;
                    match common::handle_recv_large_file_end(
                        &server, &path, final_hash, PEER, &None, "test",
                    ) {
                        Ok(common::LargeFileEndOutcome::MissingChunks(_)) => {}
                        Ok(_) => panic!("expected MissingChunks, got committed"),
                        Err(e) => panic!("expected MissingChunks, got error: {e}"),
                    }
                }
                Message::ActionBatchEnd { .. } => {}
                other => apply_wire(&server, other),
            }
            Ok(())
        };
        // Mirrors server.rs: the incomplete file fails, so the ack holds the
        // previous prefix (nothing applied yet → 0).
        let mut ack = |_upto: u64| Ok(0u64);
        let stats = drain_action_journal(&client, &mut send, &mut ack).unwrap();
        assert!(saw_end, "the drain must stream a LargeFileEnd");
        assert_eq!(stats.acked_upto, 0, "ack must not advance past the incomplete file");
        assert_eq!(stats.settled_entries, 0);
    }
    assert!(!has(&srv_dir, "big.bin"), "incomplete file is not committed");
    assert_eq!(client.journal().len(), 1, "failed entry stays journalled");

    // Pass 2 (retry with intact wire): delivers and fully acks.
    let (stats, _) = drain_into(&client, &server);
    assert_eq!(stats.settled_entries, 1);
    assert_eq!(read(&srv_dir, "big.bin"), big);
    assert!(client.journal().is_empty());
}

#[test]
fn far_future_peer_stamps_are_clamped_to_now() {
    // A malicious peer stamp (e.g. u64::MAX) must not beat every tombstone
    // under last-writer-wins: ingress clamps anything beyond now + a small
    // skew allowance (5 min) to local now, with a warn.
    use bytehive_filesync::common::MAX_FUTURE_SKEW_MS;

    let now = now_ms();
    assert_eq!(common::clamp_future_stamp(now, PEER, "delete", "test"), now);
    let clamped = common::clamp_future_stamp(u64::MAX, PEER, "delete", "test");
    assert!(
        clamped <= now + MAX_FUTURE_SKEW_MS,
        "far-future stamp must be clamped, got {clamped}"
    );

    // A bundle carrying u64::MAX applies, but its live-action record is
    // clamped to ~now instead of persisting MAX.
    let srv_dir = tmp("clamp_srv");
    let server = engine(&srv_dir, "srv");
    let evil = bundle_of("evil.txt", b"evil", OLD_MTIME, Some(u64::MAX));
    common::handle_recv_bundle(&server, &evil, PEER, &None, "test").unwrap();
    assert_eq!(read(&srv_dir, "evil.txt"), b"evil");
    let alive = server.ledger_snapshot().alive_since(Path::new("evil.txt"));
    assert!(
        alive <= now + MAX_FUTURE_SKEW_MS + 60_000,
        "live-action record must be clamped, got {alive}"
    );
    assert!(alive > OLD_MTIME, "clamped stamp is still a live action");

    // A delete stamped u64::MAX likewise lands at ~now, not MAX.
    write(&srv_dir, "gone.txt", b"bye", OLD_MTIME);
    server.scan().unwrap();
    common::handle_recv_delete_with_meta(
        &server,
        &[PathBuf::from("gone.txt")],
        u64::MAX,
        "evil-peer",
        PEER,
        &None,
        "test",
    )
    .unwrap();
    let tomb = tomb_of(&server, "gone.txt").expect("tombstone recorded");
    assert!(
        tomb.0 <= now + MAX_FUTURE_SKEW_MS + 60_000,
        "tombstone stamp must be clamped, got {}",
        tomb.0
    );
}
