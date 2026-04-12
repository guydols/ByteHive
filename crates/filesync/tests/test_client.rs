use std::collections::HashMap;
use std::path::PathBuf;

use bytehive_filesync::{
    client::count_manifest,
    protocol::{FileMetadata, Manifest},
};

fn make_manifest(entries: &[(&str, u64, bool)]) -> Manifest {
    let mut files = HashMap::new();
    for (path, size, is_dir) in entries {
        let p = PathBuf::from(path);
        files.insert(
            p.clone(),
            FileMetadata {
                rel_path: p,
                size: *size,
                hash: [0u8; 32],
                modified_ms: 0,
                is_dir: *is_dir,
            },
        );
    }
    Manifest {
        files,
        node_id: "test-node".to_string(),
    }
}

#[test]
fn count_empty_manifest() {
    let (f, d, b) = count_manifest(&make_manifest(&[]));
    assert_eq!(f, 0);
    assert_eq!(d, 0);
    assert_eq!(b, 0);
}

#[test]
fn count_files_only() {
    let m = make_manifest(&[("a.txt", 100, false), ("b.txt", 200, false)]);
    let (f, d, b) = count_manifest(&m);
    assert_eq!(f, 2);
    assert_eq!(d, 0);
    assert_eq!(b, 300);
}

#[test]
fn count_dirs_only() {
    let m = make_manifest(&[("dir1", 0, true), ("dir2", 0, true), ("dir3", 0, true)]);
    let (f, d, b) = count_manifest(&m);
    assert_eq!(f, 0);
    assert_eq!(d, 3);
    assert_eq!(b, 0);
}

#[test]
fn count_mixed_files_and_dirs() {
    let m = make_manifest(&[
        ("src", 0, true),
        ("src/main.rs", 1024, false),
        ("src/lib.rs", 512, false),
        ("target", 0, true),
    ]);
    let (f, d, b) = count_manifest(&m);
    assert_eq!(f, 2);
    assert_eq!(d, 2);
    assert_eq!(b, 1536);
}

#[test]
fn count_dirs_do_not_add_to_byte_total() {
    let m = make_manifest(&[("bigdir", 999_999, true), ("small.txt", 42, false)]);
    let (f, d, b) = count_manifest(&m);
    assert_eq!(f, 1);
    assert_eq!(d, 1);
    assert_eq!(b, 42, "directory sizes must not inflate the byte counter");
}

#[test]
fn count_large_file() {
    let large = 10 * 1024 * 1024 * 1024u64;
    let m = make_manifest(&[("huge.iso", large, false)]);
    let (f, d, b) = count_manifest(&m);
    assert_eq!(f, 1);
    assert_eq!(d, 0);
    assert_eq!(b, large);
}

#[test]
fn count_byte_sum_is_exact() {
    let m = make_manifest(&[
        ("f1.bin", 1, false),
        ("f2.bin", 2, false),
        ("f3.bin", 4, false),
        ("f4.bin", 8, false),
    ]);
    let (f, _, b) = count_manifest(&m);
    assert_eq!(f, 4);
    assert_eq!(b, 15);
}

#[test]
fn count_zero_size_file_counts_as_file() {
    let m = make_manifest(&[("empty.txt", 0, false)]);
    let (f, d, b) = count_manifest(&m);
    assert_eq!(f, 1);
    assert_eq!(d, 0);
    assert_eq!(b, 0);
}

#[test]
fn count_many_files_and_dirs() {
    let m = make_manifest(&[
        ("d1", 0, true),
        ("d1/f.txt", 100, false),
        ("d2", 0, true),
        ("d2/f.txt", 100, false),
        ("d3", 0, true),
        ("d3/f.txt", 100, false),
        ("d4", 0, true),
        ("d4/f.txt", 100, false),
        ("d5", 0, true),
        ("d5/f.txt", 100, false),
    ]);
    let (f, d, b) = count_manifest(&m);
    assert_eq!(f, 5);
    assert_eq!(d, 5);
    assert_eq!(b, 500);
}
