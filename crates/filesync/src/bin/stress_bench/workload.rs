use super::types::WorkloadStats;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::Duration;

/// How long to sleep after creating a directory so the inotify watcher
/// can register a watch before files land inside it.
const DIR_SETTLE_MS: u64 = 200;

fn settle() {
    thread::sleep(Duration::from_millis(DIR_SETTLE_MS));
}

/// Pre-create every sub-directory the workload phases will use so that the
/// inotify watcher is already watching them before any files are written.
///
/// Directories are created **one level at a time** with a sleep between each
/// level.  This is necessary because inotify only fires a `CREATE+ISDIR`
/// event on the *parent* directory's watch.  If a child directory is created
/// before the watcher has processed the parent's event and added a watch for
/// it, the child's creation is silently lost and no watch is ever added.
///
/// Call this once during setup, **before** any phase starts.
pub fn prepare_directories(dir: &Path) {
    // ── Level 0: top-level phase directories (parent = root, already watched) ──
    let level0 = [
        "small_flood",
        "large_files",
        "mixed_burst",
        "recreated",
        "sustained",
    ];
    for name in &level0 {
        fs::create_dir_all(dir.join(name)).expect("prepare level-0 dir");
    }
    // Wait for the watcher to process CREATE events and add watches for each.
    thread::sleep(Duration::from_millis(400));

    // ── Level 1: depth dirs under mixed_burst ──
    for depth in 0..4 {
        let p = dir.join(format!("mixed_burst/depth{depth}"));
        fs::create_dir(p).ok(); // ok() — already exists is fine
    }
    thread::sleep(Duration::from_millis(400));

    // ── Level 2: branch dirs under each depth ──
    for depth in 0..4 {
        for branch in 0..3 {
            let p = dir.join(format!("mixed_burst/depth{depth}/branch{branch}"));
            fs::create_dir(p).ok();
        }
    }

    // Final settle — give the watcher plenty of time to register everything.
    thread::sleep(Duration::from_millis(500));
}

/// Fast deterministic PRNG for generating file content.
struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 { 1 } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Fill a buffer with pseudo-random bytes.
    fn fill_bytes(&mut self, buf: &mut [u8]) {
        let mut pos = 0;
        while pos + 8 <= buf.len() {
            let val = self.next_u64().to_le_bytes();
            buf[pos..pos + 8].copy_from_slice(&val);
            pos += 8;
        }
        if pos < buf.len() {
            let remaining = buf.len() - pos;
            let val = self.next_u64().to_le_bytes();
            buf[pos..pos + remaining].copy_from_slice(&val[..remaining]);
        }
    }
}

/// Generate deterministic content for a file based on its index and desired size.
fn generate_content(seed: u64, size: usize) -> Vec<u8> {
    let mut rng = Xorshift64::new(seed);
    let mut buf = vec![0u8; size];
    rng.fill_bytes(&mut buf);
    buf
}

/// Phase 1: Create many small files (1KB - 100KB) in rapid succession.
pub fn small_file_flood(dir: &Path, count: usize) -> WorkloadStats {
    let subdir = dir.join("small_flood");
    fs::create_dir_all(&subdir).expect("create small_flood dir");
    settle();

    let mut stats = WorkloadStats::default();
    let mut size_rng = Xorshift64::new(0xDEAD_BEEF_CAFE);

    // Pre-create batch subdirectories so the watcher is ready
    let batch_count = count / 500;
    for b in 1..=batch_count {
        let nested = subdir.join(format!("batch_{b}"));
        fs::create_dir_all(&nested).expect("create nested dir");
    }
    if batch_count > 0 {
        settle();
    }

    for i in 0..count {
        let size = 1024 + (size_rng.next_u64() % (100 * 1024)) as usize; // 1KB - ~100KB
        let content = generate_content(i as u64 + 1, size);
        let path = subdir.join(format!("file_{i:06}.dat"));
        let mut f = fs::File::create(&path).expect("create small file");
        f.write_all(&content).expect("write small file");
        stats.files_created += 1;
        stats.bytes_written += size as u64;
    }

    stats
}

/// Phase 2: Create several large files (configurable size in MB).
pub fn large_file_transfer(dir: &Path, count: usize, size_mb: usize) -> WorkloadStats {
    let subdir = dir.join("large_files");
    fs::create_dir_all(&subdir).expect("create large_files dir");
    settle();

    let mut stats = WorkloadStats::default();
    let chunk_size = 1024 * 1024; // Write 1MB at a time

    for i in 0..count {
        // Vary sizes: 50% to 150% of the given size
        let actual_mb = (size_mb as f64 * (0.5 + (i as f64 / count as f64))).max(1.0) as usize;
        let total_bytes = actual_mb * 1024 * 1024;
        let path = subdir.join(format!("large_{i:04}.bin"));
        let mut f = fs::File::create(&path).expect("create large file");

        let mut rng = Xorshift64::new(0x1000_0000 + i as u64);
        let mut remaining = total_bytes;
        let mut chunk = vec![0u8; chunk_size];

        while remaining > 0 {
            let to_write = remaining.min(chunk_size);
            rng.fill_bytes(&mut chunk[..to_write]);
            f.write_all(&chunk[..to_write])
                .expect("write large file chunk");
            remaining -= to_write;
        }

        stats.files_created += 1;
        stats.bytes_written += total_bytes as u64;
    }

    stats
}

/// Phase 3: Mixed burst - create both small and large files simultaneously.
/// Also creates a nested directory structure.
pub fn mixed_burst(
    dir: &Path,
    small_count: usize,
    large_count: usize,
    large_size_mb: usize,
) -> WorkloadStats {
    let subdir = dir.join("mixed_burst");
    fs::create_dir_all(&subdir).expect("create mixed_burst dir");

    let mut stats = WorkloadStats::default();
    let mut size_rng = Xorshift64::new(0xBEEF_1234);

    // Create nested directory tree first and let inotify register watches
    for depth in 0..4 {
        for branch in 0..3 {
            let nested = subdir.join(format!("depth{depth}/branch{branch}"));
            fs::create_dir_all(&nested).expect("create nested dir");
        }
    }
    settle();

    // Interleave small and large file creation
    let mut small_idx = 0usize;
    let mut large_idx = 0usize;
    let total_ops = small_count + large_count;

    for op in 0..total_ops {
        let do_large = large_idx < large_count
            && (small_idx >= small_count || op % ((small_count / large_count.max(1)) + 1) == 0);

        if do_large {
            let actual_mb = (large_size_mb as f64
                * (0.3 + (large_idx as f64 / large_count as f64) * 0.7))
                .max(1.0) as usize;
            let total_bytes = actual_mb * 1024 * 1024;
            let depth = large_idx % 4;
            let branch = large_idx % 3;
            let path = subdir.join(format!(
                "depth{depth}/branch{branch}/large_{large_idx:04}.bin"
            ));

            let mut f = fs::File::create(&path).expect("create mixed large file");
            let mut rng = Xorshift64::new(0x2000_0000 + large_idx as u64);
            let chunk_size = 1024 * 1024;
            let mut remaining = total_bytes;
            let mut chunk = vec![0u8; chunk_size];

            while remaining > 0 {
                let to_write = remaining.min(chunk_size);
                rng.fill_bytes(&mut chunk[..to_write]);
                f.write_all(&chunk[..to_write]).expect("write chunk");
                remaining -= to_write;
            }

            stats.files_created += 1;
            stats.bytes_written += total_bytes as u64;
            large_idx += 1;
        } else {
            let size = 512 + (size_rng.next_u64() % (200 * 1024)) as usize;
            let content = generate_content(0x3000_0000 + small_idx as u64, size);
            let depth = small_idx % 4;
            let branch = small_idx % 3;
            let path = subdir.join(format!(
                "depth{depth}/branch{branch}/small_{small_idx:06}.dat"
            ));
            let mut f = fs::File::create(&path).expect("create mixed small file");
            f.write_all(&content).expect("write small file");
            stats.files_created += 1;
            stats.bytes_written += size as u64;
            small_idx += 1;
        }
    }

    stats
}

/// Phase 4: Modify existing files with new content. Picks files from the
/// `small_flood` directory and overwrites them.
pub fn modification_storm(dir: &Path, count: usize) -> WorkloadStats {
    let subdir = dir.join("small_flood");
    let mut stats = WorkloadStats::default();

    if !subdir.exists() {
        eprintln!("[workload] modification_storm: small_flood dir does not exist, skipping");
        return stats;
    }

    let mut entries: Vec<_> = fs::read_dir(&subdir)
        .expect("read small_flood dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let modify_count = count.min(entries.len());
    let mut size_rng = Xorshift64::new(0xDEAD_00D1);

    for (i, entry) in entries.iter().take(modify_count).enumerate() {
        let path = entry.path();
        let new_size = 2048 + (size_rng.next_u64() % (150 * 1024)) as usize;
        let content = generate_content(0x4000_0000 + i as u64, new_size);
        let mut f = fs::File::create(&path).expect("overwrite file");
        f.write_all(&content).expect("write modified content");
        stats.files_modified += 1;
        stats.bytes_written += new_size as u64;
    }

    stats
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── Xorshift64 ───────────────────────────────────────────────────────────

    #[test]
    fn xorshift64_zero_seed_treated_as_one() {
        let mut from_zero = Xorshift64::new(0);
        let mut from_one = Xorshift64::new(1);
        // Both must produce the same sequence since 0 is remapped to 1
        assert_eq!(from_zero.next_u64(), from_one.next_u64());
    }

    #[test]
    fn xorshift64_deterministic_for_same_seed() {
        let mut a = Xorshift64::new(12345);
        let mut b = Xorshift64::new(12345);
        for _ in 0..20 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn xorshift64_different_seeds_produce_different_output() {
        let mut a = Xorshift64::new(1);
        let mut b = Xorshift64::new(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn xorshift64_never_produces_zero() {
        let mut rng = Xorshift64::new(999);
        for _ in 0..1000 {
            assert_ne!(rng.next_u64(), 0, "Xorshift64 must never produce zero");
        }
    }

    #[test]
    fn xorshift64_fill_bytes_produces_correct_length() {
        let mut rng = Xorshift64::new(7);
        let mut buf = vec![0u8; 37];
        rng.fill_bytes(&mut buf);
        assert_eq!(buf.len(), 37);
    }

    #[test]
    fn xorshift64_fill_bytes_deterministic() {
        let mut a = Xorshift64::new(7);
        let mut b = Xorshift64::new(7);
        let mut ba = vec![0u8; 64];
        let mut bb = vec![0u8; 64];
        a.fill_bytes(&mut ba);
        b.fill_bytes(&mut bb);
        assert_eq!(ba, bb);
    }

    #[test]
    fn xorshift64_fill_bytes_empty_slice_is_noop() {
        let mut rng = Xorshift64::new(1);
        let mut buf: Vec<u8> = vec![];
        rng.fill_bytes(&mut buf); // must not panic
    }

    #[test]
    fn xorshift64_fill_bytes_not_all_zero_for_nonempty_slice() {
        let mut rng = Xorshift64::new(42);
        let mut buf = vec![0u8; 16];
        rng.fill_bytes(&mut buf);
        // With a good PRNG all-zeros output is astronomically unlikely
        assert_ne!(buf, vec![0u8; 16]);
    }

    // ── generate_content ─────────────────────────────────────────────────────

    #[test]
    fn generate_content_correct_size() {
        assert_eq!(generate_content(1, 1024).len(), 1024);
        assert_eq!(generate_content(2, 0).len(), 0);
        assert_eq!(generate_content(3, 8).len(), 8);
    }

    #[test]
    fn generate_content_deterministic() {
        assert_eq!(generate_content(42, 256), generate_content(42, 256));
    }

    #[test]
    fn generate_content_different_seeds_differ() {
        assert_ne!(generate_content(1, 256), generate_content(2, 256));
    }

    // ── small_file_flood ─────────────────────────────────────────────────────

    #[test]
    fn small_file_flood_creates_correct_count() {
        let tmp = TempDir::new().unwrap();
        let stats = small_file_flood(tmp.path(), 3);
        assert_eq!(stats.files_created, 3);
        assert_eq!(stats.files_modified, 0);
        assert_eq!(stats.files_deleted, 0);
        assert!(stats.bytes_written >= 3 * 1024);
    }

    #[test]
    fn small_file_flood_creates_files_in_subdir() {
        let tmp = TempDir::new().unwrap();
        small_file_flood(tmp.path(), 2);
        let subdir = tmp.path().join("small_flood");
        assert!(subdir.is_dir());
        let file_count = std::fs::read_dir(&subdir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .count();
        assert_eq!(file_count, 2);
    }

    // ── large_file_transfer ──────────────────────────────────────────────────

    #[test]
    fn large_file_transfer_creates_correct_count() {
        let tmp = TempDir::new().unwrap();
        let stats = large_file_transfer(tmp.path(), 2, 1);
        assert_eq!(stats.files_created, 2);
        assert_eq!(stats.files_modified, 0);
        assert_eq!(stats.files_deleted, 0);
        assert!(stats.bytes_written > 0);
    }

    #[test]
    fn large_file_transfer_creates_files_in_subdir() {
        let tmp = TempDir::new().unwrap();
        large_file_transfer(tmp.path(), 1, 1);
        assert!(tmp.path().join("large_files").is_dir());
    }

    // ── mixed_burst ──────────────────────────────────────────────────────────

    #[test]
    fn mixed_burst_creates_small_and_large_files() {
        let tmp = TempDir::new().unwrap();
        // 3 small + 1 large = 4 files total
        let stats = mixed_burst(tmp.path(), 3, 1, 1);
        assert_eq!(stats.files_created, 4);
        assert!(stats.bytes_written > 0);
    }

    #[test]
    fn mixed_burst_creates_subdir() {
        let tmp = TempDir::new().unwrap();
        mixed_burst(tmp.path(), 2, 0, 1);
        assert!(tmp.path().join("mixed_burst").is_dir());
    }

    // ── modification_storm ───────────────────────────────────────────────────

    #[test]
    fn modification_storm_returns_zero_if_no_source_dir() {
        let tmp = TempDir::new().unwrap();
        // small_flood dir does not exist
        let stats = modification_storm(tmp.path(), 5);
        assert_eq!(stats.files_modified, 0);
    }

    #[test]
    fn modification_storm_modifies_existing_files() {
        let tmp = TempDir::new().unwrap();
        small_file_flood(tmp.path(), 5);
        let stats = modification_storm(tmp.path(), 3);
        assert_eq!(stats.files_modified, 3);
        assert!(stats.bytes_written > 0);
    }

    #[test]
    fn modification_storm_capped_at_available_file_count() {
        let tmp = TempDir::new().unwrap();
        // Create 2 files, try to modify 10 — should only modify the 2 that exist
        small_file_flood(tmp.path(), 2);
        let stats = modification_storm(tmp.path(), 10);
        assert_eq!(stats.files_modified, 2);
    }

    // ── delete_and_recreate ──────────────────────────────────────────────────

    #[test]
    fn delete_and_recreate_deletes_and_creates() {
        let tmp = TempDir::new().unwrap();
        small_file_flood(tmp.path(), 5);
        let stats = delete_and_recreate(tmp.path(), 2, 3);
        assert_eq!(stats.files_deleted, 2);
        assert_eq!(stats.files_created, 3);
        assert!(stats.bytes_written > 0);
    }

    #[test]
    fn delete_and_recreate_creates_in_recreated_subdir() {
        let tmp = TempDir::new().unwrap();
        delete_and_recreate(tmp.path(), 0, 2);
        let recreate_dir = tmp.path().join("recreated");
        assert!(recreate_dir.is_dir());
        let count = std::fs::read_dir(&recreate_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .count();
        assert_eq!(count, 2);
    }

    #[test]
    fn delete_and_recreate_no_source_dir_still_creates() {
        let tmp = TempDir::new().unwrap();
        // No small_flood dir — delete count should be 0, create count honoured
        let stats = delete_and_recreate(tmp.path(), 5, 2);
        assert_eq!(stats.files_deleted, 0);
        assert_eq!(stats.files_created, 2);
    }

    // ── sustained_tick ───────────────────────────────────────────────────────

    #[test]
    fn sustained_tick_creates_10_small_files_for_non_divisible_tick() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sustained")).unwrap();
        // tick 1: not divisible by 5 or 20 → 10 small files only
        let stats = sustained_tick(tmp.path(), 1);
        assert_eq!(stats.files_created, 10);
    }

    #[test]
    fn sustained_tick_5_adds_medium_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sustained")).unwrap();
        // tick 5: divisible by 5, not 20 → 10 small + 1 medium = 11
        let stats = sustained_tick(tmp.path(), 5);
        assert_eq!(stats.files_created, 11);
    }

    #[test]
    fn sustained_tick_0_adds_medium_and_large_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sustained")).unwrap();
        // tick 0: divisible by both 5 and 20 → 10 small + 1 medium + 1 large = 12
        let stats = sustained_tick(tmp.path(), 0);
        assert_eq!(stats.files_created, 12);
    }

    #[test]
    fn sustained_tick_creates_sustained_dir_if_missing() {
        let tmp = TempDir::new().unwrap();
        // Do NOT pre-create the sustained dir — the function should create it
        sustained_tick(tmp.path(), 1);
        assert!(tmp.path().join("sustained").is_dir());
    }

    #[test]
    fn sustained_tick_bytes_written_is_positive() {
        let tmp = TempDir::new().unwrap();
        let stats = sustained_tick(tmp.path(), 2);
        assert!(stats.bytes_written > 0);
    }

    // ── prepare_directories ──────────────────────────────────────────────────

    #[test]
    fn prepare_directories_creates_all_top_level_dirs() {
        let tmp = TempDir::new().unwrap();
        prepare_directories(tmp.path());
        for name in &[
            "small_flood",
            "large_files",
            "mixed_burst",
            "recreated",
            "sustained",
        ] {
            assert!(
                tmp.path().join(name).is_dir(),
                "missing top-level dir: {name}"
            );
        }
    }

    #[test]
    fn prepare_directories_creates_mixed_burst_nested_dirs() {
        let tmp = TempDir::new().unwrap();
        prepare_directories(tmp.path());
        for depth in 0..4 {
            for branch in 0..3 {
                let p = tmp
                    .path()
                    .join(format!("mixed_burst/depth{depth}/branch{branch}"));
                assert!(p.is_dir(), "missing nested dir: {}", p.display());
            }
        }
    }

    #[test]
    fn prepare_directories_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        // Calling twice must not panic (create_dir_all / create_dir with .ok() handle exists)
        prepare_directories(tmp.path());
        prepare_directories(tmp.path());
        assert!(tmp.path().join("small_flood").is_dir());
    }
}

/// Phase 5: Delete some files and recreate new ones.
pub fn delete_and_recreate(dir: &Path, delete_count: usize, create_count: usize) -> WorkloadStats {
    let subdir = dir.join("small_flood");
    let mut stats = WorkloadStats::default();

    if subdir.exists() {
        let mut entries: Vec<_> = fs::read_dir(&subdir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries.iter().take(delete_count) {
            let _ = fs::remove_file(entry.path());
            stats.files_deleted += 1;
        }
    }

    // Recreate in a fresh directory
    let recreate_dir = dir.join("recreated");
    fs::create_dir_all(&recreate_dir).expect("create recreated dir");
    settle();

    let mut size_rng = Xorshift64::new(0xDEAD_EC8E);
    for i in 0..create_count {
        let size = 1024 + (size_rng.next_u64() % (80 * 1024)) as usize;
        let content = generate_content(0x5000_0000 + i as u64, size);
        let path = recreate_dir.join(format!("recreated_{i:06}.dat"));
        let mut f = fs::File::create(&path).expect("create recreated file");
        f.write_all(&content).expect("write recreated file");
        stats.files_created += 1;
        stats.bytes_written += size as u64;
    }

    stats
}

/// Phase 6: Sustained mixed workload - called repeatedly from a timer loop.
/// Each tick creates a mix of small and medium files plus occasionally a larger file.
pub fn sustained_tick(dir: &Path, tick: usize) -> WorkloadStats {
    // Write directly into the pre-created `sustained` directory using flat
    // filenames that encode the tick number.  This avoids creating a new
    // sub-directory (and the associated inotify settle delay) on every tick,
    // keeping the sustained phase fast and realistic.
    let subdir = dir.join("sustained");
    // The directory was already created by `prepare_directories`; this is a
    // no-op if it exists, but guards against being called standalone.
    fs::create_dir_all(&subdir).ok();

    let mut stats = WorkloadStats::default();
    let mut size_rng = Xorshift64::new(0x6000_0000 + tick as u64);

    // 10 small files per tick
    for i in 0..10 {
        let size = 1024 + (size_rng.next_u64() % (50 * 1024)) as usize;
        let content = generate_content(0x6000_0000 + tick as u64 * 1000 + i as u64, size);
        let path = subdir.join(format!("t{tick:06}_s{i:03}.dat"));
        let mut f = fs::File::create(&path).expect("create sustained file");
        f.write_all(&content).expect("write sustained file");
        stats.files_created += 1;
        stats.bytes_written += size as u64;
    }

    // Every 5th tick, create a medium file (1-5 MB)
    if tick % 5 == 0 {
        let size = 1024 * 1024 + (size_rng.next_u64() % (4 * 1024 * 1024)) as usize;
        let mut content = vec![0u8; size];
        let mut rng = Xorshift64::new(0x7000_0000 + tick as u64);
        rng.fill_bytes(&mut content);
        let path = subdir.join(format!("t{tick:06}_med.bin"));
        let mut f = fs::File::create(&path).expect("create medium file");
        f.write_all(&content).expect("write medium file");
        stats.files_created += 1;
        stats.bytes_written += size as u64;
    }

    // Every 20th tick, create a larger file (10-20 MB)
    if tick % 20 == 0 {
        let size = 10 * 1024 * 1024 + (size_rng.next_u64() % (10 * 1024 * 1024)) as usize;
        let mut content = vec![0u8; size];
        let mut rng = Xorshift64::new(0x8000_0000 + tick as u64);
        rng.fill_bytes(&mut content);
        let path = subdir.join(format!("t{tick:06}_lrg.bin"));
        let mut f = fs::File::create(&path).expect("create large file");
        f.write_all(&content).expect("write large file");
        stats.files_created += 1;
        stats.bytes_written += size as u64;
    }

    stats
}
