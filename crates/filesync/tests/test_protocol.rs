use std::io::Cursor;
use std::path::PathBuf;

use bytehive_filesync::protocol::{
    read_message, serialise_message, write_message, FileBundle, FileData, FileMetadata, Manifest,
    Message, BUNDLE_MAX_BYTES, BUNDLE_MAX_FILES, FILE_CHUNK_SIZE, LARGE_FILE_THRESHOLD,
    MAX_FRAME_BYTES, PROTOCOL_VERSION,
};

fn hello_msg() -> Message {
    Message::Hello {
        node_id: "test-node".to_string(),
        protocol_version: PROTOCOL_VERSION,
        credential: Some("s3cr3t".to_string()),
    }
}

fn bundle_msg(filename: &str, content: &[u8]) -> Message {
    let hash: [u8; 32] = blake3::hash(content).into();
    Message::Bundle(FileBundle {
        files: vec![FileData {
            metadata: FileMetadata {
                rel_path: PathBuf::from(filename),
                size: content.len() as u64,
                hash,
                modified_ms: 1_000_000,
                is_dir: false,
            },
            content: content.to_vec(),
        }],
        bundle_id: 42,
    })
}

#[test]
fn roundtrip_hello() {
    let msg = hello_msg();
    let frame = serialise_message(&msg).unwrap();
    assert!(
        frame.len() >= 4,
        "frame must have at least the 4-byte length prefix"
    );
    let mut cur = Cursor::new(frame);
    match read_message(&mut cur).unwrap() {
        Message::Hello {
            node_id,
            protocol_version,
            credential,
        } => {
            assert_eq!(node_id, "test-node");
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert_eq!(credential, Some("s3cr3t".to_string()));
        }
        _ => panic!("expected Hello"),
    }
}

#[test]
fn roundtrip_bundle() {
    let content = b"the quick brown fox";
    let msg = bundle_msg("fox.txt", content);
    let frame = serialise_message(&msg).unwrap();
    let mut cur = Cursor::new(frame);
    match read_message(&mut cur).unwrap() {
        Message::Bundle(b) => {
            assert_eq!(b.bundle_id, 42);
            assert_eq!(b.files.len(), 1);
            assert_eq!(b.files[0].content, content);
            assert_eq!(b.files[0].metadata.size, content.len() as u64);
            let expected: [u8; 32] = blake3::hash(content).into();
            assert_eq!(b.files[0].metadata.hash, expected);
        }
        _ => panic!("expected Bundle"),
    }
}

#[test]
fn roundtrip_delete() {
    let msg = Message::Delete {
        paths: vec![PathBuf::from("a.txt"), PathBuf::from("sub/b.txt")],
    };
    let frame = serialise_message(&msg).unwrap();
    let mut cur = Cursor::new(frame);
    match read_message(&mut cur).unwrap() {
        Message::Delete { paths } => {
            assert_eq!(paths.len(), 2);
            assert_eq!(paths[0], PathBuf::from("a.txt"));
            assert_eq!(paths[1], PathBuf::from("sub/b.txt"));
        }
        _ => panic!("expected Delete"),
    }
}

#[test]
fn roundtrip_sync_complete() {
    let msg = Message::SyncComplete;
    let frame = serialise_message(&msg).unwrap();
    let mut cur = Cursor::new(frame);
    match read_message(&mut cur).unwrap() {
        Message::SyncComplete => {}
        _ => panic!("expected SyncComplete"),
    }
}

#[test]
fn roundtrip_manifest_exchange() {
    let mut files = std::collections::HashMap::new();
    let path = PathBuf::from("readme.md");
    let hash = [0xABu8; 32];
    files.insert(
        path.clone(),
        FileMetadata {
            rel_path: path,
            size: 42,
            hash,
            modified_ms: 9999,
            is_dir: false,
        },
    );
    let manifest = Manifest {
        files,
        node_id: "node-abc".to_string(),
    };
    let msg = Message::ManifestExchange(manifest);
    let frame = serialise_message(&msg).unwrap();
    let mut cur = Cursor::new(frame);
    match read_message(&mut cur).unwrap() {
        Message::ManifestExchange(m) => {
            assert_eq!(m.node_id, "node-abc");
            assert_eq!(m.files.len(), 1);
            let meta = m.files.get(&PathBuf::from("readme.md")).unwrap();
            assert_eq!(meta.size, 42);
            assert_eq!(meta.hash, [0xABu8; 32]);
        }
        _ => panic!("expected ManifestExchange"),
    }
}

#[test]
fn roundtrip_large_file_start() {
    let hash = [0xCDu8; 32];
    let metadata = FileMetadata {
        rel_path: PathBuf::from("big.bin"),
        size: 32 * 1024 * 1024,
        hash,
        modified_ms: 12345,
        is_dir: false,
    };
    let msg = Message::LargeFileStart {
        metadata,
        total_chunks: 4,
    };
    let frame = serialise_message(&msg).unwrap();
    let mut cur = Cursor::new(frame);
    match read_message(&mut cur).unwrap() {
        Message::LargeFileStart {
            metadata: m,
            total_chunks,
        } => {
            assert_eq!(total_chunks, 4);
            assert_eq!(m.size, 32 * 1024 * 1024);
            assert_eq!(m.hash, [0xCDu8; 32]);
            assert_eq!(m.rel_path, PathBuf::from("big.bin"));
        }
        _ => panic!("expected LargeFileStart"),
    }
}

#[test]
fn roundtrip_large_file_chunk() {
    let data = vec![42u8; 1024];
    let msg = Message::LargeFileChunk {
        path: PathBuf::from("big.bin"),
        chunk_index: 3,
        data: data.clone(),
    };
    let frame = serialise_message(&msg).unwrap();
    let mut cur = Cursor::new(frame);
    match read_message(&mut cur).unwrap() {
        Message::LargeFileChunk {
            path,
            chunk_index,
            data: d,
        } => {
            assert_eq!(path, PathBuf::from("big.bin"));
            assert_eq!(chunk_index, 3);
            assert_eq!(d, data);
        }
        _ => panic!("expected LargeFileChunk"),
    }
}

#[test]
fn roundtrip_large_file_end() {
    let hash = [0xEFu8; 32];
    let msg = Message::LargeFileEnd {
        path: PathBuf::from("big.bin"),
        final_hash: hash,
    };
    let frame = serialise_message(&msg).unwrap();
    let mut cur = Cursor::new(frame);
    match read_message(&mut cur).unwrap() {
        Message::LargeFileEnd { path, final_hash } => {
            assert_eq!(path, PathBuf::from("big.bin"));
            assert_eq!(final_hash, hash);
        }
        _ => panic!("expected LargeFileEnd"),
    }
}

#[test]
fn write_read_message_roundtrip() {
    let msg = hello_msg();
    let mut buf = Vec::new();
    write_message(&mut buf, &msg).unwrap();
    let mut cur = Cursor::new(buf);
    match read_message(&mut cur).unwrap() {
        Message::Hello { node_id, .. } => assert_eq!(node_id, "test-node"),
        _ => panic!("expected Hello"),
    }
}

#[test]
fn read_message_rejects_oversized_frame() {
    let bad_len = (MAX_FRAME_BYTES as u32).saturating_add(1);
    let mut buf = bad_len.to_be_bytes().to_vec();
    let mut cur = Cursor::new(buf);
    let err = read_message(&mut cur).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("MAX_FRAME_BYTES") || err.to_string().contains("exceeds"));
}

#[test]
fn serialise_message_produces_valid_length_prefix() {
    let msg = bundle_msg("tiny.txt", b"x");
    let frame = serialise_message(&msg).unwrap();
    let declared = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
    assert_eq!(declared, frame.len() - 4);
}

#[test]
fn constants_are_consistent() {
    assert!(LARGE_FILE_THRESHOLD > 0);
    assert!(BUNDLE_MAX_BYTES > 0);
    assert!(BUNDLE_MAX_FILES > 0);
    assert!(FILE_CHUNK_SIZE > 0);
    assert!(MAX_FRAME_BYTES >= BUNDLE_MAX_BYTES);
    assert!(MAX_FRAME_BYTES >= FILE_CHUNK_SIZE);
    assert!(PROTOCOL_VERSION >= 1);
}

#[test]
fn roundtrip_rename() {
    let msg = Message::Rename {
        from: PathBuf::from("old/path.txt"),
        to: PathBuf::from("new/path.txt"),
    };
    let frame = serialise_message(&msg).unwrap();
    let mut cur = Cursor::new(frame);
    match read_message(&mut cur).unwrap() {
        Message::Rename { from, to } => {
            assert_eq!(from, PathBuf::from("old/path.txt"));
            assert_eq!(to, PathBuf::from("new/path.txt"));
        }
        _ => panic!("expected Rename"),
    }
}

#[test]
fn roundtrip_request_chunks() {
    let msg = Message::RequestChunks {
        path: PathBuf::from("big.bin"),
        chunk_indices: vec![0, 2, 5],
    };
    let frame = serialise_message(&msg).unwrap();
    let mut cur = Cursor::new(frame);
    match read_message(&mut cur).unwrap() {
        Message::RequestChunks {
            path,
            chunk_indices,
        } => {
            assert_eq!(path, PathBuf::from("big.bin"));
            assert_eq!(chunk_indices, vec![0, 2, 5]);
        }
        _ => panic!("expected RequestChunks"),
    }
}

#[test]
fn truncated_header_returns_error() {
    let short = vec![0u8, 0]; // only 2 bytes instead of 4
    let mut cur = Cursor::new(short);
    assert!(read_message(&mut cur).is_err());
}

#[test]
fn truncated_body_returns_error() {
    let declared_len: u32 = 100;
    let mut frame = declared_len.to_be_bytes().to_vec();
    frame.extend_from_slice(&[0u8; 10]); // only 10 bytes instead of 100
    let mut cur = Cursor::new(frame);
    assert!(read_message(&mut cur).is_err());
}

#[test]
fn roundtrip_hello_no_credential() {
    let msg = Message::Hello {
        node_id: "anon-node".to_string(),
        protocol_version: PROTOCOL_VERSION,
        credential: None,
    };
    let frame = serialise_message(&msg).unwrap();
    let mut cur = Cursor::new(frame);
    match read_message(&mut cur).unwrap() {
        Message::Hello {
            node_id,
            credential,
            ..
        } => {
            assert_eq!(node_id, "anon-node");
            assert_eq!(credential, None);
        }
        _ => panic!("expected Hello"),
    }
}

#[test]
fn delete_message_length_prefix_is_consistent() {
    let msg1 = Message::Delete {
        paths: vec![PathBuf::from("a.txt")],
    };
    let msg2 = Message::Delete {
        paths: vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")],
    };
    let frame1 = serialise_message(&msg1).unwrap();
    let frame2 = serialise_message(&msg2).unwrap();
    let len1 = u32::from_be_bytes(frame1[..4].try_into().unwrap()) as usize;
    let len2 = u32::from_be_bytes(frame2[..4].try_into().unwrap()) as usize;
    assert_eq!(len1, frame1.len() - 4);
    assert_eq!(len2, frame2.len() - 4);
    assert!(
        frame2.len() > frame1.len(),
        "more paths must produce a larger frame"
    );
}
