use crate::exclusions::Exclusions;
use crate::ledger::DeletionLedger;
use crate::protocol::{FileMetadata, Manifest, HASH_THREADS};
use rayon::prelude::*;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

fn hash_file_streaming(path: &Path) -> io::Result<(u64, [u8; 32])> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((size, hasher.finalize().into()))
}

use std::io;

pub fn build_manifest(root: &Path, node_id: &str, exclusions: &Exclusions) -> io::Result<Manifest> {
    if HASH_THREADS > 0 {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(HASH_THREADS)
            .build_global();
    }

    let paths: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let p = e.into_path();
            let rel = p.strip_prefix(root).ok()?;
            if rel.as_os_str().is_empty() {
                return None;
            }

            if exclusions.is_excluded(rel) {
                log::debug!(
                    "manifest: skipping {:?} (rule: {:?})",
                    rel,
                    exclusions.matching_rule(rel)
                );
                return None;
            }
            Some(p)
        })
        .collect();

    let entries: Vec<(PathBuf, FileMetadata)> = paths
        .par_iter()
        .filter_map(|full| {
            let rel = full.strip_prefix(root).ok()?.to_path_buf();
            let meta = std::fs::metadata(full).ok()?;
            let modified_ms = meta
                .modified()
                .ok()?
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()?
                .as_millis() as u64;

            if meta.is_dir() {
                return Some((
                    rel.clone(),
                    FileMetadata {
                        rel_path: rel,
                        size: 0,
                        hash: [0u8; 32],
                        modified_ms,
                        change_sequence: 0,
                        is_dir: true,
                    },
                ));
            }

            let (size, hash) = hash_file_streaming(full).ok()?;

            Some((
                rel.clone(),
                FileMetadata {
                    rel_path: rel,
                    size,
                    hash,
                    modified_ms,
                    change_sequence: 0,
                    is_dir: false,
                },
            ))
        })
        .collect();

    Ok(Manifest {
        files: entries.into_iter().collect(),
        node_id: node_id.to_string(),
    })
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
/// - A path is vetoed (dropped from `to_send`) when the ledger holds a
///   tombstone strictly newer than the local file's `modified_ms` (via
///   [`DeletionLedger::has_newer_than`]) AND the local copy looks like the
///   same-or-older version the tombstone describes (local content hash
///   equals `prev_hash`, or local mtime is not newer than `prev_mtime_ms`;
///   missing prev info fails closed toward the delete).
/// - A path whose local `modified_ms` is strictly newer than the
///   tombstone is a legitimate recreate: it stays in `to_send` and is
///   reported in `resurrected` so the caller can drop the stale tombstone
///   (e.g. via `SyncEngine::clear_tombstones`, which removes + saves).
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
        let Some(tomb) = ledger.get(&path) else {
            to_send.push(path);
            continue;
        };
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
