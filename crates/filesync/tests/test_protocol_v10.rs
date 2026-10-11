//! Protocol v10 wire tests: live-action stamps on file data, the drain
//! batch markers, and live-action records in LedgerExchange.

use std::io::Cursor;
use std::path::PathBuf;

use bytehive_filesync::protocol::{
    read_message, serialise_message, FileBundle, FileData, FileMetadata, Message, TombstoneDto,
    PROTOCOL_VERSION,
};

fn roundtrip(msg: &Message) -> Message {
    let frame = serialise_message(msg).unwrap();
    read_message(&mut Cursor::new(frame)).unwrap()
}

fn meta(rel: &str) -> FileMetadata {
    FileMetadata {
        rel_path: PathBuf::from(rel),
        size: 3,
        hash: [7u8; 32],
        modified_ms: 111,
        change_sequence: 0,
        is_dir: false,
    }
}

#[test]
fn protocol_version_is_ten() {
    assert_eq!(PROTOCOL_VERSION, 10);
}

#[test]
fn file_data_action_stamp_roundtrips_some_and_none() {
    let msg = Message::Bundle(FileBundle {
        files: vec![
            FileData {
                metadata: meta("live.txt"),
                content: b"abc".to_vec(),
                action_ms: Some(9_000),
            },
            FileData {
                metadata: meta("catchup.txt"),
                content: b"abc".to_vec(),
                action_ms: None,
            },
        ],
        bundle_id: 5,
    });
    match roundtrip(&msg) {
        Message::Bundle(b) => {
            assert_eq!(b.bundle_id, 5);
            assert_eq!(b.files[0].action_ms, Some(9_000));
            assert_eq!(b.files[1].action_ms, None);
            assert_eq!(b.files[1].content, b"abc".to_vec());
        }
        other => panic!("expected Bundle, got {other:?}"),
    }
}

#[test]
fn large_file_start_stamp_roundtrips_none() {
    let msg = Message::LargeFileStart {
        metadata: meta("big.bin"),
        total_chunks: 2,
        action_ms: None,
    };
    match roundtrip(&msg) {
        Message::LargeFileStart { action_ms, total_chunks, .. } => {
            assert_eq!(action_ms, None);
            assert_eq!(total_chunks, 2);
        }
        other => panic!("expected LargeFileStart, got {other:?}"),
    }
}

#[test]
fn action_batch_markers_roundtrip() {
    match roundtrip(&Message::ActionBatchEnd { upto_seq: 42 }) {
        Message::ActionBatchEnd { upto_seq } => assert_eq!(upto_seq, 42),
        other => panic!("expected ActionBatchEnd, got {other:?}"),
    }
    match roundtrip(&Message::ActionBatchAck { upto_seq: 43 }) {
        Message::ActionBatchAck { upto_seq } => assert_eq!(upto_seq, 43),
        other => panic!("expected ActionBatchAck, got {other:?}"),
    }
}

#[test]
fn ledger_exchange_carries_live_action_records() {
    let msg = Message::LedgerExchange {
        entries: vec![
            TombstoneDto {
                path: PathBuf::from("gone.txt"),
                deleted_at_ms: 10,
                deleter_node: "n".into(),
                prev_hash: None,
                prev_mtime_ms: None,
                alive: false,
            },
            TombstoneDto {
                path: PathBuf::from("live.txt"),
                deleted_at_ms: 20,
                deleter_node: String::new(),
                prev_hash: None,
                prev_mtime_ms: None,
                alive: true,
            },
        ],
    };
    match roundtrip(&msg) {
        Message::LedgerExchange { entries } => {
            assert!(!entries[0].alive);
            assert!(entries[1].alive);
            assert_eq!(entries[1].deleted_at_ms, 20);
        }
        other => panic!("expected LedgerExchange, got {other:?}"),
    }
}

#[test]
fn rename_action_stamp_roundtrips() {
    let msg = Message::Rename {
        from: PathBuf::from("a"),
        to: PathBuf::from("b"),
        action_ms: 77,
    };
    match roundtrip(&msg) {
        Message::Rename { action_ms, .. } => assert_eq!(action_ms, 77),
        other => panic!("expected Rename, got {other:?}"),
    }
}
