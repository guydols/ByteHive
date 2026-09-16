use crate::exclusions::Exclusions;
use crate::ledger::DeletionLedger;
use crate::manifest_cache::{inode_of, mtime_parts, CachedEntry, ManifestCache};
use crate::protocol::{FileMetadata, Manifest, HASH_THREADS};
use crossbeam_channel::bounded;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Instant, SystemTime};

/// Files at or above this size try `update_mmap_rayon` first (with streaming
/// fallback); everything else streams with a 256 KiB buffer.
const TIER_MMAP_MIN_BYTES: u64 = 4 * 1024 * 1024;
/// Streaming read buffer (also used for files under 128 KiB — a single read).
const HASH_BUF_BYTES: usize = 256 * 1024;
/// In-flight paths between the walker and the hash pool (backpressure).
const WALK_CHANNEL_DEPTH: usize = 1024;
/// Traversal threads for jwalk's isolated pool (see below).
const TRAVERSAL_THREADS: usize = 4;

/// Tiered blake3 digest shared by scan / conflict-check / large-file paths.
/// Returns `(size, hash)`. Never switches algorithms (blake3 only).
pub fn hash_file_tiered(path: &Path) -> io::Result<(u64, [u8; 32])> {
    let size = std::fs::metadata(path)?.len();
    if size >= TIER_MMAP_MIN_BYTES {
        let mut hasher = blake3::Hasher::new();
        match hasher.update_mmap_rayon(path) {
            Ok(_) => return Ok((size, hasher.finalize().into())),
            Err(e) => {
                log::debug!(
                    "hash_file_tiered: mmap failed for {path:?}, streaming fallback: {e}"
                );
            }
        }
    }
    let file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(std::io::BufReader::with_capacity(HASH_BUF_BYTES, file))?;
    Ok((size, hasher.finalize().into()))
}

fn parse_hex32(s: &str) -> Result<[u8; 32], ()> {
    if s.len() != 64 {
        return Err(());
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16).ok_or(())?;
        let lo = (chunk[1] as char).to_digit(16).ok_or(())?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Ok(out)
}

/// Per-scan metrics. `hash_ms` is summed cpu time across hash threads;
/// `walk_ms` is walker wall time (the two overlap by design).
#[derive(Debug, Clone, Default)]
pub struct ScanStats {
    pub walk_ms: u64,
    pub hash_ms: u64,
    pub files: usize,
    pub dirs: usize,
    pub bytes: u64,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub hash_mb_s: f64,
}

struct BuildOutcome {
    rel: PathBuf,
    meta: FileMetadata,
    cache_upsert: Option<(PathBuf, CachedEntry)>,
}

#[allow(clippy::too_many_arguments)]
fn hash_one(
    root: &Path,
    rel: &PathBuf,
    cache: Option<&ManifestCache>,
    dirty: &HashSet<PathBuf>,
    hash_nanos: &AtomicU64,
    hash_bytes: &AtomicU64,
    hits: &AtomicUsize,
    misses: &AtomicUsize,
) -> Option<BuildOutcome> {
    let full = root.join(rel);
    let meta = std::fs::metadata(&full).ok()?;
    let modified_ms = meta
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;

    if meta.is_dir() {
        return Some(BuildOutcome {
            rel: rel.clone(),
            meta: FileMetadata {
                rel_path: rel.clone(),
                size: 0,
                hash: [0u8; 32],
                modified_ms,
                change_sequence: 0,
                is_dir: true,
            },
            cache_upsert: None,
        });
    }

    // Fast path: trusted cache hit (stable + old + identity match).
    if !dirty.contains(rel) {
        if let Some(cache) = cache {
            if let Some((mtime_s, mtime_ns)) = mtime_parts(&meta) {
                let now = crate::ledger::now_ms();
                if let Some(hex) = cache.lookup(
                    rel,
                    meta.len(),
                    mtime_s,
                    mtime_ns,
                    inode_of(&meta),
                    now,
                ) {
                    if let Ok(hash) = parse_hex32(&hex) {
                        hits.fetch_add(1, Ordering::Relaxed);
                        log::trace!("manifest: cache hit for {rel:?}");
                        return Some(BuildOutcome {
                            rel: rel.clone(),
                            meta: FileMetadata {
                                rel_path: rel.clone(),
                                size: meta.len(),
                                hash,
                                modified_ms,
                                change_sequence: 0,
                                is_dir: false,
                            },
                            cache_upsert: None,
                        });
                    }
                    // Corrupt cache payload: fall through and re-hash
                    // (the upsert below self-heals the entry).
                }
            }
        }
    }

    misses.fetch_add(1, Ordering::Relaxed);
    let t = Instant::now();
    let (size, hash) = hash_file_tiered(&full).ok()?;
    hash_nanos.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    hash_bytes.fetch_add(size, Ordering::Relaxed);

    let cache_upsert = mtime_parts(&meta).map(|(mtime_s, mtime_ns)| {
        (
            rel.clone(),
            CachedEntry {
                size,
                mtime_s,
                mtime_ns,
                inode: inode_of(&meta),
                hash: crate::hex(&hash),
            },
        )
    });

    Some(BuildOutcome {
        rel: rel.clone(),
        meta: FileMetadata {
            rel_path: rel.clone(),
            size,
            hash,
            modified_ms,
            change_sequence: 0,
            is_dir: false,
        },
        cache_upsert,
    })
}

/// Build a manifest with a pipelined parallel scan: jwalk traverses on the
/// calling thread and streams relative paths through a bounded channel to a
/// dedicated rayon hash pool (`max(num_cpus, HASH_THREADS)` threads) —
/// hashing overlaps walking, no full path vec is collected first.
///
/// `cache` enables the hash cache (see [`ManifestCache`]); `dirty` lists
/// watcher-pending paths that bypass the cache (still re-cached after
/// hashing). Returns `(manifest, updated_cache, stats)`; the caller owns
/// pruning/saving the cache.
pub fn build_manifest(
    root: &Path,
    node_id: &str,
    exclusions: &Exclusions,
    cache: Option<&ManifestCache>,
    dirty: &HashSet<PathBuf>,
) -> io::Result<(Manifest, ManifestCache, ScanStats)> {
    let threads = num_cpus::get().max(HASH_THREADS);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("hash-{i}"))
        .build()
        .map_err(io::Error::other)?;

    let (work_tx, work_rx) = bounded::<PathBuf>(WALK_CHANNEL_DEPTH);
    let (res_tx, res_rx) = crossbeam_channel::unbounded::<BuildOutcome>();
    let walk_ms = AtomicU64::new(0);
    let hash_nanos = AtomicU64::new(0);
    let hash_bytes = AtomicU64::new(0);
    let hits = AtomicUsize::new(0);
    let misses = AtomicUsize::new(0);

    let root_p = root.to_path_buf();
    // Shared metric refs (Copy) for the worker closures below.
    let hash_nanos_r = &hash_nanos;
    let hash_bytes_r = &hash_bytes;
    let hits_r = &hits;
    let misses_r = &misses;
    pool.scope(|s| {
        for _ in 0..threads {
            let rx = work_rx.clone();
            let tx = res_tx.clone();
            let root_c = root_p.clone();
            s.spawn(move |_| {
                for rel in rx {
                    if let Some(outcome) = hash_one(
                        &root_c,
                        &rel,
                        cache,
                        dirty,
                        hash_nanos_r,
                        hash_bytes_r,
                        hits_r,
                        misses_r,
                    ) {
                        if tx.send(outcome).is_err() {
                            break;
                        }
                    }
                }
            });
        }
        // The scope thread walks: traversal overlaps hashing via backpressure.
        // NOTE: skip_hidden(false) is required — walkdir parity (hidden files
        // and .bh_filesync/ are visited, then filtered by exclusions below).
        // NOTE: isolated RayonNewPool, NOT the rayon global pool — jwalk's
        // default global-pool handshake (1s busy-timeout, silent abort) can
        // misfire when many scans share a process, silently degrading to an
        // EMPTY walk (empty manifest, data looks deleted). Sharing our hash
        // pool would deadlock instead (all hash threads parked on recv while
        // traversal waits for a thread). Verified by failing→passing repro;
        // do not change this without a multi-scan-in-one-process test.
        let wstart = Instant::now();
        let walk_opts = jwalk::WalkDir::new(&root_p)
            .skip_hidden(false)
            .parallelism(jwalk::Parallelism::RayonNewPool(TRAVERSAL_THREADS));
        for entry in walk_opts {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let full = entry.path();
            let rel = match full.strip_prefix(&root_p) {
                Ok(r) if !r.as_os_str().is_empty() => r.to_path_buf(),
                _ => continue,
            };
            if exclusions.is_excluded(&rel) {
                log::debug!(
                    "manifest: skipping {:?} (rule: {:?})",
                    rel,
                    exclusions.matching_rule(&rel)
                );
                continue;
            }
            if work_tx.send(rel).is_err() {
                break;
            }
        }
        walk_ms.store(wstart.elapsed().as_millis() as u64, Ordering::Relaxed);
        drop(work_tx);
    });
    drop(work_rx);
    drop(res_tx);

    let mut files_map: HashMap<PathBuf, FileMetadata> = HashMap::new();
    let mut updated = cache.cloned().unwrap_or_default();
    let mut dirs = 0usize;
    for outcome in res_rx {
        if outcome.meta.is_dir {
            dirs += 1;
        }
        if let Some((p, e)) = outcome.cache_upsert {
            updated.upsert(p, e);
        }
        files_map.insert(outcome.rel, outcome.meta);
    }
    let files = files_map.values().filter(|m| !m.is_dir).count();
    let bytes: u64 = files_map.values().map(|m| m.size).sum();
    let existing: HashSet<PathBuf> = files_map.keys().cloned().collect();
    updated.prune_missing(&existing);

    let nanos = hash_nanos.load(Ordering::Relaxed);
    let hbytes = hash_bytes.load(Ordering::Relaxed);
    let stats = ScanStats {
        walk_ms: walk_ms.load(Ordering::Relaxed),
        hash_ms: (nanos / 1_000_000) as u64,
        files,
        dirs,
        bytes,
        cache_hits: hits.load(Ordering::Relaxed),
        cache_misses: misses.load(Ordering::Relaxed),
        hash_mb_s: if nanos > 0 {
            hbytes as f64 / (nanos as f64 / 1e9) / 1e6
        } else {
            0.0
        },
    };

    Ok((
        Manifest {
            files: files_map,
            node_id: node_id.to_string(),
        },
        updated,
        stats,
    ))
}

pub fn compute_send_list(local: &Manifest, remote: &Manifest, is_server: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (path, lm) in &local.files {
        match remote.files.get(path) {
            None => out.push(path.clone()),
            Some(rm) if lm.hash != rm.hash => {
                if lm.modified_ms > rm.modified_ms {
                    out.push(path.clone());
                } else if lm.modified_ms == rm.modified_ms && is_server {
                    out.push(path.clone());
                }
            }
            _ => {}
        }
    }
    out
}

/// Veto uploads that would resurrect a peer-deleted file.
///
/// `send_list` is typically the output of [`compute_send_list`]; `ledger`
/// is the peer's (or merged) deletion ledger. Returns `(to_send,
/// resurrected)`:
/// - A path is vetoed (dropped from `to_send`) when the ledger holds an
///   exact tombstone strictly newer than the local file's `modified_ms`
///   (via [`DeletionLedger::has_newer_than`]) AND the local copy looks like
///   the same-or-older version the tombstone describes (local content hash
///   equals `prev_hash`, or local mtime is not newer than `prev_mtime_ms`;
///   missing prev info fails closed toward the delete).
/// - A path with no exact tombstone but a covering ancestor tombstone
///   (e.g. child of a deleted directory, via
///   [`DeletionLedger::covering_tombstone`]) is vetoed purely on timestamp
///   ordering: ancestor `deleted_at_ms > local modified_ms` vetoes (the
///   ancestor's `prev_hash`/`prev_mtime_ms` describe the directory, not the
///   child, so they are not consulted). This also covers stale directory
///   entries (`hash == [0; 32]`).
/// - A path whose local `modified_ms` is strictly newer than the
///   exact or covering tombstone is a legitimate recreate: it stays in
///   `to_send` and is reported in `resurrected` so the caller can drop the
///   stale tombstone (e.g. via `SyncEngine::clear_tombstones`, which
///   removes + saves).
/// - `None` ledger (or no tombstone / no local entry) passes through.
///
/// The existing `is_server` tie-break in [`compute_send_list`] is
/// untouched; this only filters its output.
pub fn filter_resurrected(
    send_list: Vec<PathBuf>,
    local: &Manifest,
    ledger: Option<&DeletionLedger>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let Some(ledger) = ledger else {
        return (send_list, Vec::new());
    };
    let mut to_send = Vec::with_capacity(send_list.len());
    let mut resurrected = Vec::new();
    for path in send_list {
        let Some(local_meta) = local.files.get(&path) else {
            // No local entry to evaluate — pass through.
            to_send.push(path);
            continue;
        };
        // Exact tombstone first (preserves prev_hash/prev_mtime semantics);
        // otherwise fall back to the nearest covering ancestor tombstone.
        if let Some(tomb) = ledger.get(&path) {
            if local_meta.modified_ms > tomb.deleted_at_ms {
                // Legitimate recreate: local version is newer than the delete.
                resurrected.push(path.clone());
                to_send.push(path);
            } else if ledger.has_newer_than(&path, local_meta.modified_ms) {
                let hash_matches = match &tomb.prev_hash {
                    Some(prev) => crate::hex(&local_meta.hash) == *prev,
                    None => true,
                };
                let mtime_not_newer = match tomb.prev_mtime_ms {
                    Some(prev_mtime) => local_meta.modified_ms <= prev_mtime,
                    None => true,
                };
                if hash_matches || mtime_not_newer {
                    // Stale copy loses to the tombstone — veto the upload.
                    log::debug!(
                        "filter_resurrected: vetoing {:?} (tombstone @{} by {} beats local mtime {})",
                        path,
                        tomb.deleted_at_ms,
                        tomb.deleter_node,
                        local_meta.modified_ms,
                    );
                } else {
                    to_send.push(path);
                }
            } else {
                // Equal timestamps or no ordering signal — pass through and let
                // the normal sync decision stand.
                to_send.push(path);
            }
            continue;
        }
        // No exact tombstone: check for a covering ancestor (dir delete).
        let Some(cover) = ledger.covering_tombstone(&path) else {
            to_send.push(path);
            continue;
        };
        if local_meta.modified_ms > cover.deleted_at_ms {
            // Legitimate recreate under a deleted dir: local version is newer.
            resurrected.push(path.clone());
            to_send.push(path);
        } else if cover.deleted_at_ms > local_meta.modified_ms {
            // Stale child loses to the ancestor delete — veto the upload.
            // Note: no prev_hash/prev_mtime check here; those describe the
            // deleted directory itself, not this child (and stale dir
            // entries with hash == [0; 32] must still be vetoed).
            log::debug!(
                "filter_resurrected: vetoing {:?} (covering tombstone {:?} @{} by {} beats local mtime {})",
                path,
                cover.path,
                cover.deleted_at_ms,
                cover.deleter_node,
                local_meta.modified_ms,
            );
        } else {
            // Equal timestamps — pass through and let the normal sync
            // decision stand.
            to_send.push(path);
        }
    }
    (to_send, resurrected)
}

pub fn diff_manifests(old: &Manifest, new: &Manifest) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut changed = Vec::new();
    let mut deleted = Vec::new();

    for (path, new_meta) in &new.files {
        match old.files.get(path) {
            None => changed.push(path.clone()),
            Some(old_meta) if old_meta.hash != new_meta.hash => changed.push(path.clone()),
            _ => {}
        }
    }

    for path in old.files.keys() {
        if !new.files.contains_key(path) {
            deleted.push(path.clone());
        }
    }

    (changed, deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FileMetadata;

    fn meta(path: &str, byte: u8, mtime: u64) -> FileMetadata {
        FileMetadata {
            rel_path: PathBuf::from(path),
            size: 1,
            hash: [byte; 32],
            modified_ms: mtime,
            change_sequence: 0,
            is_dir: false,
        }
    }

    fn dir_meta(path: &str, mtime: u64) -> FileMetadata {
        FileMetadata {
            rel_path: PathBuf::from(path),
            size: 0,
            hash: [0; 32],
            modified_ms: mtime,
            change_sequence: 0,
            is_dir: true,
        }
    }

    fn manifest_with(entries: Vec<FileMetadata>) -> Manifest {
        Manifest {
            files: entries.into_iter().map(|m| (m.rel_path.clone(), m)).collect(),
            node_id: "test".to_string(),
        }
    }

    fn ledger_with(path: &str, deleted_at: u64) -> DeletionLedger {
        let mut l = DeletionLedger::new();
        l.record(
            PathBuf::from(path),
            deleted_at,
            "peer".to_string(),
            None,
            None,
        );
        l
    }

    #[test]
    fn child_file_vetoed_by_parent_tombstone() {
        // Dir delete tombstones only the top path; the stale child must veto.
        let local = manifest_with(vec![meta("docs/a.txt", 0xAA, 50)]);
        let ledger = ledger_with("docs", 100);
        let (to_send, resurrected) = filter_resurrected(
            vec![PathBuf::from("docs/a.txt")],
            &local,
            Some(&ledger),
        );
        assert!(to_send.is_empty(), "stale child must be vetoed");
        assert!(resurrected.is_empty());
    }

    #[test]
    fn child_recreate_with_newer_mtime_allowed() {
        // Legitimate recreate wins: child mtime newer than parent delete.
        let local = manifest_with(vec![meta("docs/a.txt", 0xBB, 150)]);
        let ledger = ledger_with("docs", 100);
        let (to_send, resurrected) = filter_resurrected(
            vec![PathBuf::from("docs/a.txt")],
            &local,
            Some(&ledger),
        );
        assert_eq!(to_send, vec![PathBuf::from("docs/a.txt")]);
        assert_eq!(resurrected, vec![PathBuf::from("docs/a.txt")]);
    }

    #[test]
    fn stale_dir_entry_vetoed_by_parent_tombstone() {
        // Directory entries carry hash == [0; 32]; a stale subdir must veto.
        let local = manifest_with(vec![dir_meta("docs/sub", 50)]);
        let ledger = ledger_with("docs", 100);
        let (to_send, resurrected) = filter_resurrected(
            vec![PathBuf::from("docs/sub")],
            &local,
            Some(&ledger),
        );
        assert!(to_send.is_empty(), "stale dir child must be vetoed");
        assert!(resurrected.is_empty());
    }
}
