use super::types::IntegrityResult;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How many consecutive stable polls (same file count on both sides) before
/// we consider the sync settled.
const STABLE_POLLS_REQUIRED: usize = 4;
/// Interval between polls during the wait loop.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Walk a directory and return a map of relative paths to their BLAKE3 hashes.
/// Skips the `.bh_filesync` directory.
fn hash_directory(root: &Path) -> HashMap<PathBuf, [u8; 32]> {
    let mut map = HashMap::new();
    let walker = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok());

    for entry in walker {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        // Skip the .bh_filesync directory
        if rel.components().any(|c| c.as_os_str() == ".bh_filesync") {
            continue;
        }

        let hash = hash_file(path);
        map.insert(rel, hash);
    }

    map
}

/// Compute BLAKE3 hash of a file using streaming reads.
fn hash_file(path: &Path) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return [0u8; 32],
    };
    let mut buf = [0u8; 65536];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => break,
        };
    }
    *hasher.finalize().as_bytes()
}

/// Compare the contents of a source directory against a destination directory.
/// Returns an IntegrityResult describing matches, mismatches, missing, and extra files.
pub fn check_integrity(source_dir: &Path, dest_dir: &Path) -> IntegrityResult {
    let source_hashes = hash_directory(source_dir);
    let dest_hashes = hash_directory(dest_dir);

    let mut matched = 0usize;
    let mut mismatched = Vec::new();
    let mut missing_from_dest = Vec::new();

    for (rel_path, source_hash) in &source_hashes {
        match dest_hashes.get(rel_path) {
            Some(dest_hash) => {
                if source_hash == dest_hash {
                    matched += 1;
                } else {
                    mismatched.push(rel_path.clone());
                }
            }
            None => {
                missing_from_dest.push(rel_path.clone());
            }
        }
    }

    let extra_in_dest: Vec<PathBuf> = dest_hashes
        .keys()
        .filter(|p| !source_hashes.contains_key(*p))
        .cloned()
        .collect();

    IntegrityResult {
        matched,
        mismatched,
        missing_from_dest,
        extra_in_dest,
    }
}

/// Count the number of regular files in a directory (excluding .bh_filesync).
pub fn count_files(dir: &Path) -> usize {
    if !dir.exists() {
        return 0;
    }
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().is_file()
                && !e
                    .path()
                    .components()
                    .any(|c| c.as_os_str() == ".bh_filesync")
        })
        .count()
}

/// Wait until the destination directory has at least `expected_count` files,
/// or until timeout. Returns true if the count was reached.
pub fn wait_for_file_count(dest_dir: &Path, expected_count: usize, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        let current = count_files(dest_dir);
        if current >= expected_count {
            return true;
        }
        if start.elapsed() >= timeout {
            eprintln!(
                "[integrity] Timeout waiting for file count: got {current}, expected {expected_count}"
            );
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Wait for the destination to have the same number of files as the source.
///
/// Instead of a single settle sleep, this uses **progressive settling**: the
/// destination file-count must match the source for `STABLE_POLLS_REQUIRED`
/// consecutive polls before we declare sync complete.  This handles large
/// file transfers that arrive in chunks as well as delete propagation where
/// the dest count temporarily overshoots.
pub fn wait_for_sync(source_dir: &Path, dest_dir: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    let mut stable_streak: usize = 0;
    let mut prev_dest_count: Option<usize> = None;

    loop {
        let source_count = count_files(source_dir);
        let dest_count = count_files(dest_dir);

        if dest_count >= source_count && source_count > 0 {
            // Counts match — but only trust it once we see the same dest
            // count for several consecutive polls (no new files arriving).
            match prev_dest_count {
                Some(prev) if prev == dest_count => {
                    stable_streak += 1;
                }
                _ => {
                    stable_streak = 1;
                }
            }
            if stable_streak >= STABLE_POLLS_REQUIRED {
                return true;
            }
        } else {
            stable_streak = 0;
        }

        prev_dest_count = Some(dest_count);

        if start.elapsed() >= timeout {
            eprintln!(
                "[integrity] Sync timeout: source has {source_count} files, \
                 dest has {dest_count} (stable streak {stable_streak}/{STABLE_POLLS_REQUIRED})"
            );
            return false;
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    fn write(dir: &std::path::Path, name: &str, data: &[u8]) {
        std::fs::write(dir.join(name), data).unwrap();
    }

    // ── count_files ──────────────────────────────────────────────────────────

    #[test]
    fn count_files_empty_directory() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(count_files(tmp.path()), 0);
    }

    #[test]
    fn count_files_nonexistent_directory() {
        assert_eq!(
            count_files(std::path::Path::new(
                "/tmp/xyz_no_exist_bench_integrity_test"
            )),
            0
        );
    }

    #[test]
    fn count_files_counts_regular_files() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.txt", b"a");
        write(tmp.path(), "b.txt", b"b");
        write(tmp.path(), "c.txt", b"c");
        assert_eq!(count_files(tmp.path()), 3);
    }

    #[test]
    fn count_files_skips_bh_filesync_directory() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "real.txt", b"data");
        let hidden = tmp.path().join(".bh_filesync");
        std::fs::create_dir_all(&hidden).unwrap();
        write(&hidden, "internal.dat", b"internal");
        assert_eq!(count_files(tmp.path()), 1, "must skip .bh_filesync files");
    }

    #[test]
    fn count_files_counts_files_in_subdirectories() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        write(tmp.path(), "root.txt", b"root");
        write(&sub, "child.txt", b"child");
        assert_eq!(count_files(tmp.path()), 2);
    }

    // ── hash_file ─────────────────────────────────────────────────────────────

    #[test]
    fn hash_file_is_consistent() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("f.bin");
        std::fs::write(&p, b"hello world").unwrap();
        assert_eq!(hash_file(&p), hash_file(&p));
    }

    #[test]
    fn hash_file_differs_for_different_content() {
        let tmp = TempDir::new().unwrap();
        let p1 = tmp.path().join("a.bin");
        let p2 = tmp.path().join("b.bin");
        std::fs::write(&p1, b"foo").unwrap();
        std::fs::write(&p2, b"bar").unwrap();
        assert_ne!(hash_file(&p1), hash_file(&p2));
    }

    #[test]
    fn hash_file_returns_zeros_for_missing_file() {
        let h = hash_file(std::path::Path::new(
            "/tmp/no_such_file_xyz_bench_integrity.bin",
        ));
        assert_eq!(h, [0u8; 32]);
    }

    #[test]
    fn hash_file_agrees_with_blake3_for_known_input() {
        let tmp = TempDir::new().unwrap();
        let content = b"deterministic content";
        let p = tmp.path().join("known.bin");
        std::fs::write(&p, content).unwrap();
        let expected: [u8; 32] = *blake3::hash(content).as_bytes();
        assert_eq!(hash_file(&p), expected);
    }

    #[test]
    fn hash_file_empty_file_matches_blake3_empty() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("empty.bin");
        std::fs::write(&p, b"").unwrap();
        let expected: [u8; 32] = *blake3::hash(b"").as_bytes();
        assert_eq!(hash_file(&p), expected);
    }

    // ── check_integrity ───────────────────────────────────────────────────────

    #[test]
    fn check_integrity_identical_dirs_pass() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        write(src.path(), "a.txt", b"hello");
        write(dst.path(), "a.txt", b"hello");
        let r = check_integrity(src.path(), dst.path());
        assert_eq!(r.matched, 1);
        assert!(r.mismatched.is_empty());
        assert!(r.missing_from_dest.is_empty());
        assert!(r.passed());
    }

    #[test]
    fn check_integrity_detects_content_mismatch() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        write(src.path(), "a.txt", b"hello");
        write(dst.path(), "a.txt", b"world");
        let r = check_integrity(src.path(), dst.path());
        assert_eq!(r.mismatched.len(), 1);
        assert!(!r.passed());
    }

    #[test]
    fn check_integrity_detects_missing_file() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        write(src.path(), "a.txt", b"hello");
        let r = check_integrity(src.path(), dst.path());
        assert_eq!(r.missing_from_dest.len(), 1);
        assert!(!r.passed());
    }

    #[test]
    fn check_integrity_detects_extra_in_dest() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        write(src.path(), "a.txt", b"hello");
        write(dst.path(), "a.txt", b"hello");
        write(dst.path(), "extra.txt", b"extra");
        let r = check_integrity(src.path(), dst.path());
        assert_eq!(r.extra_in_dest.len(), 1);
        assert!(r.passed(), "extra files alone do not fail passed()");
    }

    #[test]
    fn check_integrity_multiple_files_all_matched() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        for i in 0..5u8 {
            let content = format!("content {i}");
            write(src.path(), &format!("f{i}.txt"), content.as_bytes());
            write(dst.path(), &format!("f{i}.txt"), content.as_bytes());
        }
        let r = check_integrity(src.path(), dst.path());
        assert_eq!(r.matched, 5);
        assert!(r.passed());
    }

    #[test]
    fn check_integrity_both_empty_dirs_pass() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let r = check_integrity(src.path(), dst.path());
        assert_eq!(r.matched, 0);
        assert!(r.passed());
    }

    #[test]
    fn check_integrity_skips_bh_filesync_in_both_dirs() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        write(src.path(), "real.txt", b"same");
        write(dst.path(), "real.txt", b"same");
        // Put mismatched hidden files — they must be ignored
        let src_hidden = src.path().join(".bh_filesync");
        let dst_hidden = dst.path().join(".bh_filesync");
        std::fs::create_dir_all(&src_hidden).unwrap();
        std::fs::create_dir_all(&dst_hidden).unwrap();
        write(&src_hidden, "meta", b"server data");
        write(&dst_hidden, "meta", b"client data different");
        let r = check_integrity(src.path(), dst.path());
        assert_eq!(r.matched, 1);
        assert!(r.passed());
    }

    // ── wait_for_file_count ───────────────────────────────────────────────────

    #[test]
    fn wait_for_file_count_immediate_success() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "a.txt", b"a");
        write(tmp.path(), "b.txt", b"b");
        assert!(wait_for_file_count(tmp.path(), 2, Duration::from_secs(1)));
    }

    #[test]
    fn wait_for_file_count_times_out() {
        let tmp = TempDir::new().unwrap();
        // No files → expecting 5 → must time out
        assert!(!wait_for_file_count(
            tmp.path(),
            5,
            Duration::from_millis(300)
        ));
    }

    #[test]
    fn wait_for_file_count_zero_expected_always_succeeds() {
        let tmp = TempDir::new().unwrap();
        // expected_count = 0: current (0) >= 0 is always true
        assert!(wait_for_file_count(
            tmp.path(),
            0,
            Duration::from_millis(100)
        ));
    }

    #[test]
    fn wait_for_file_count_exact_count_succeeds() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "only.txt", b"x");
        assert!(wait_for_file_count(tmp.path(), 1, Duration::from_secs(1)));
    }

    // ── wait_for_sync ─────────────────────────────────────────────────────────

    #[test]
    fn wait_for_sync_both_empty_times_out() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        // Both empty: source_count == 0 so `source_count > 0` is never satisfied
        assert!(!wait_for_sync(
            src.path(),
            dst.path(),
            Duration::from_millis(300)
        ));
    }

    #[test]
    fn wait_for_sync_already_in_sync() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        for i in 0..5u8 {
            let content = format!("data {i}");
            write(src.path(), &format!("f{i}.txt"), content.as_bytes());
            write(dst.path(), &format!("f{i}.txt"), content.as_bytes());
        }
        // Needs STABLE_POLLS_REQUIRED (4) consecutive polls at 500 ms each → ~2–3 s
        assert!(wait_for_sync(
            src.path(),
            dst.path(),
            Duration::from_secs(5)
        ));
    }
}
