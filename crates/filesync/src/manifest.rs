use crate::exclusions::Exclusions;
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

/// Compares two manifests and returns the paths that differ.
///
/// Returns `(changed_or_added, deleted)` where:
/// - `changed_or_added` contains every path that is new in `new` or whose hash
///   differs from `old`.
/// - `deleted` contains every path present in `old` but absent in `new`.
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
