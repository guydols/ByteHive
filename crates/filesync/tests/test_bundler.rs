use bytehive_filesync::{
    bundler::stream_messages,
    protocol::{Message, FILE_CHUNK_SIZE, LARGE_FILE_THRESHOLD},
};
use crossbeam_channel::bounded;
use std::path::PathBuf;

fn tmp_dir(suffix: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "filesync_bundler_{}_{suffix}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn collect_messages(root: &std::path::Path, paths: &[PathBuf]) -> Vec<Message> {
    let (tx, rx) = bounded(256);
    stream_messages(root, paths, &tx);
    drop(tx);
    rx.iter().collect()
}

#[test]
fn single_small_file_produces_one_bundle() {
    let dir = tmp_dir("single");
    let content = b"hello filesync";
    std::fs::write(dir.join("hello.txt"), content).unwrap();

    let msgs = collect_messages(&dir, &[PathBuf::from("hello.txt")]);
    assert_eq!(msgs.len(), 1);
    match &msgs[0] {
        Message::Bundle(b) => {
            assert_eq!(b.files.len(), 1);
            assert_eq!(b.files[0].content, content);
            assert_eq!(b.files[0].metadata.size, content.len() as u64);
            assert!(!b.files[0].metadata.is_dir);
            let expected: [u8; 32] = blake3::hash(content).into();
            assert_eq!(
                b.files[0].metadata.hash, expected,
                "BLAKE3 hash in bundle must be correct"
            );
        }
        _ => panic!("expected Bundle, got {msgs:?}"),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn empty_file_is_bundled_correctly() {
    let dir = tmp_dir("empty_file");
    std::fs::write(dir.join("empty.txt"), b"").unwrap();

    let msgs = collect_messages(&dir, &[PathBuf::from("empty.txt")]);
    assert_eq!(msgs.len(), 1);
    match &msgs[0] {
        Message::Bundle(b) => {
            assert_eq!(b.files[0].content, b"");
            assert_eq!(b.files[0].metadata.size, 0);

            let expected: [u8; 32] = blake3::hash(b"").into();
            assert_eq!(b.files[0].metadata.hash, expected);
        }
        _ => panic!("expected Bundle"),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn multiple_small_files_all_delivered() {
    let dir = tmp_dir("multi");
    for i in 0..10u8 {
        std::fs::write(dir.join(format!("f{i}.txt")), vec![i; 64]).unwrap();
    }
    let paths: Vec<PathBuf> = (0..10)
        .map(|i| PathBuf::from(format!("f{i}.txt")))
        .collect();
    let msgs = collect_messages(&dir, &paths);

    let total_files: usize = msgs
        .iter()
        .map(|m| match m {
            Message::Bundle(b) => b.files.len(),
            _ => 0,
        })
        .sum();
    assert_eq!(
        total_files, 10,
        "all 10 files must appear across the bundles"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn bundle_ids_are_unique_across_batches() {
    let dir = tmp_dir("bundle_ids");

    for i in 0..5 {
        std::fs::write(dir.join(format!("f{i}.bin")), vec![0u8; 1024]).unwrap();
    }
    let paths: Vec<PathBuf> = (0..5).map(|i| PathBuf::from(format!("f{i}.bin"))).collect();
    let msgs = collect_messages(&dir, &paths);

    let ids: Vec<u64> = msgs
        .iter()
        .filter_map(|m| match m {
            Message::Bundle(b) => Some(b.bundle_id),
            _ => None,
        })
        .collect();

    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), unique.len(), "all bundle IDs must be unique");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn directory_entry_is_bundled_as_is_dir() {
    let dir = tmp_dir("dir_entry");
    std::fs::create_dir_all(dir.join("subdir")).unwrap();

    let msgs = collect_messages(&dir, &[PathBuf::from("subdir")]);
    assert_eq!(msgs.len(), 1);
    match &msgs[0] {
        Message::Bundle(b) => {
            assert_eq!(b.files.len(), 1);
            let meta = &b.files[0].metadata;
            assert!(meta.is_dir, "directory must be marked is_dir=true");
            assert_eq!(meta.size, 0);
            assert_eq!(meta.hash, [0u8; 32]);
            assert!(b.files[0].content.is_empty());
        }
        _ => panic!("expected Bundle"),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn missing_path_is_silently_skipped() {
    let dir = tmp_dir("missing");
    let msgs = collect_messages(&dir, &[PathBuf::from("does_not_exist.txt")]);
    assert!(
        msgs.is_empty(),
        "missing file must not produce any messages"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn mixed_present_and_missing_paths_only_present_bundled() {
    let dir = tmp_dir("mixed_paths");
    std::fs::write(dir.join("real.txt"), b"data").unwrap();
    let paths = vec![PathBuf::from("real.txt"), PathBuf::from("ghost.txt")];

    let msgs = collect_messages(&dir, &paths);
    let total: usize = msgs
        .iter()
        .map(|m| match m {
            Message::Bundle(b) => b.files.len(),
            _ => 0,
        })
        .sum();
    assert_eq!(total, 1, "only the existing file should appear");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_produces_start_chunks_end_sequence() {
    let dir = tmp_dir("large_file");

    let large_size = LARGE_FILE_THRESHOLD as usize + 1;
    let content: Vec<u8> = (0..large_size).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.join("large.bin"), &content).unwrap();

    let msgs = collect_messages(&dir, &[PathBuf::from("large.bin")]);

    assert!(
        matches!(msgs.first(), Some(Message::LargeFileStart { .. })),
        "first message must be LargeFileStart"
    );
    assert!(
        matches!(msgs.last(), Some(Message::LargeFileEnd { .. })),
        "last message must be LargeFileEnd"
    );

    for m in msgs.iter().skip(1).take(msgs.len().saturating_sub(2)) {
        assert!(
            matches!(m, Message::LargeFileChunk { .. }),
            "middle messages must be LargeFileChunk"
        );
    }

    if let Some(Message::LargeFileEnd { final_hash, .. }) = msgs.last() {
        let expected: [u8; 32] = blake3::hash(&content).into();
        assert_eq!(
            *final_hash, expected,
            "LargeFileEnd hash must match actual content"
        );
    }

    if let Some(Message::LargeFileStart { metadata, .. }) = msgs.first() {
        let expected: [u8; 32] = blake3::hash(&content).into();
        assert_eq!(
            metadata.hash, expected,
            "LargeFileStart metadata hash must match actual content"
        );
        assert_eq!(metadata.size, large_size as u64);
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_chunks_are_contiguous_and_ordered() {
    let dir = tmp_dir("large_order");
    let large_size = (FILE_CHUNK_SIZE * 3) + 512;
    let content = vec![0xABu8; large_size];
    std::fs::write(dir.join("ordered.bin"), &content).unwrap();

    let msgs = collect_messages(&dir, &[PathBuf::from("ordered.bin")]);
    let mut expected_index = 0u32;
    for m in &msgs {
        if let Message::LargeFileChunk { chunk_index, .. } = m {
            assert_eq!(*chunk_index, expected_index, "chunks must arrive in order");
            expected_index += 1;
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_chunk_data_reconstructs_original() {
    let dir = tmp_dir("large_reconstruct");
    let large_size = FILE_CHUNK_SIZE + 4096;
    let content: Vec<u8> = (0..large_size).map(|i| (i % 256) as u8).collect();
    std::fs::write(dir.join("reconstruct.bin"), &content).unwrap();

    let msgs = collect_messages(&dir, &[PathBuf::from("reconstruct.bin")]);
    let mut reconstructed = Vec::new();
    for m in &msgs {
        if let Message::LargeFileChunk { data, .. } = m {
            reconstructed.extend_from_slice(data);
        }
    }
    assert_eq!(
        reconstructed, content,
        "reassembled chunks must match original file"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn empty_path_list_produces_no_messages() {
    let dir = tmp_dir("no_paths");
    let msgs = collect_messages(&dir, &[]);
    assert!(msgs.is_empty(), "empty path list must produce no messages");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn large_file_one_chunk_end_hash_matches_content() {
    let dir = tmp_dir("one_chunk_hash");
    let large_size = LARGE_FILE_THRESHOLD as usize + 1;
    let content: Vec<u8> = (0..large_size).map(|i| (i % 199) as u8).collect();
    std::fs::write(dir.join("one_chunk.bin"), &content).unwrap();

    let msgs = collect_messages(&dir, &[PathBuf::from("one_chunk.bin")]);

    if let Some(Message::LargeFileEnd { final_hash, .. }) = msgs.last() {
        let expected: [u8; 32] = blake3::hash(&content).into();
        assert_eq!(
            *final_hash, expected,
            "LargeFileEnd hash must match content"
        );
    } else {
        panic!("expected LargeFileEnd as last message, got: {msgs:?}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn bundle_file_metadata_rel_path_matches_input() {
    let dir = tmp_dir("rel_path_check");
    std::fs::write(dir.join("check_path.txt"), b"check").unwrap();

    let msgs = collect_messages(&dir, &[PathBuf::from("check_path.txt")]);
    assert_eq!(msgs.len(), 1);
    match &msgs[0] {
        Message::Bundle(b) => {
            assert_eq!(
                b.files[0].metadata.rel_path,
                PathBuf::from("check_path.txt"),
                "rel_path in bundle metadata must match the input path"
            );
        }
        _ => panic!("expected Bundle"),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn multiple_files_produce_correct_total_size() {
    let dir = tmp_dir("total_size");
    let sizes: [u64; 4] = [100, 200, 400, 800];
    for (i, &size) in sizes.iter().enumerate() {
        std::fs::write(dir.join(format!("f{i}.bin")), vec![i as u8; size as usize]).unwrap();
    }
    let paths: Vec<PathBuf> = (0..sizes.len())
        .map(|i| PathBuf::from(format!("f{i}.bin")))
        .collect();

    let msgs = collect_messages(&dir, &paths);
    let total_in_bundles: u64 = msgs
        .iter()
        .flat_map(|m| match m {
            Message::Bundle(b) => b.files.iter().map(|f| f.metadata.size).collect::<Vec<_>>(),
            _ => vec![],
        })
        .sum();
    let expected_total: u64 = sizes.iter().sum();
    assert_eq!(
        total_in_bundles, expected_total,
        "sum of metadata.size in bundles must equal sum of actual file sizes"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
