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
/// Skips the `.filesync_tmp` directory.
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
        // Skip the .filesync_tmp directory
        if rel.components().any(|c| c.as_os_str() == ".filesync_tmp") {
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

/// Count the number of regular files in a directory (excluding .filesync_tmp).
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
                    .any(|c| c.as_os_str() == ".filesync_tmp")
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
