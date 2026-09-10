use crate::protocol::*;
use crate::sync_engine::{ChunkResult, ConflictInfo, FinishResult, SyncEngine};
use crate::transport::Connection;
use crate::watcher::FsEvent;

use bytehive_core::MessageBus;
use log::{error, info, warn};

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub fn available_disk_space(path: &Path) -> io::Result<u64> {
    fs2::available_space(path)
}

pub fn check_disk_space(path: &Path, required_bytes: u64) -> io::Result<u64> {
    let available = available_disk_space(path)?;
    if available < required_bytes {
        Err(io::Error::new(
            io::ErrorKind::StorageFull,
            format!(
                "insufficient disk space: {} B available, {} B required",
                available, required_bytes
            ),
        ))
    } else {
        Ok(available)
    }
}

const MAX_BATCH_MS: u64 = 2_000;

pub struct PendingChanges {
    pub changes: HashSet<PathBuf>,
    pub ready: HashSet<PathBuf>,
    pub deletes: HashSet<PathBuf>,
    pub renames: Vec<(PathBuf, PathBuf)>,
    pub last_event: Instant,
    pub last_full_scan: Instant,
}

impl PendingChanges {
    pub fn new() -> Self {
        Self {
            changes: HashSet::new(),
            ready: HashSet::new(),
            deletes: HashSet::new(),
            renames: Vec::new(),
            last_event: Instant::now(),
            last_full_scan: Instant::now(),
        }
    }

    pub fn collect_event(&mut self, engine: &SyncEngine, event: FsEvent) {
        match event {
            FsEvent::Changed(p) => {
                if !engine.is_suppressed(&p) && !engine.is_excluded(&p) {
                    self.deletes.remove(&p);
                    if !self.ready.contains(&p) {
                        self.changes.insert(p);
                    }
                    self.last_event = Instant::now();
                }
            }
            FsEvent::WriteComplete(p) => {
                if !engine.is_suppressed(&p) && !engine.is_excluded(&p) {
                    self.deletes.remove(&p);
                    self.changes.remove(&p);
                    self.ready.insert(p);
                    self.last_event = Instant::now();
                }
            }
            FsEvent::Deleted(p) => {
                if !engine.is_delete_suppressed(&p) && !engine.is_excluded(&p) {
                    self.changes.remove(&p);
                    self.ready.remove(&p);
                    self.deletes.insert(p);
                    self.last_event = Instant::now();
                }
            }
            FsEvent::Renamed(from, to) => {
                if !engine.is_suppressed(&from)
                    && !engine.is_suppressed(&to)
                    && !engine.is_excluded(&from)
                    && !engine.is_excluded(&to)
                {
                    self.changes.remove(&from);
                    self.ready.remove(&from);
                    self.deletes.remove(&from);
                    self.changes.remove(&to);
                    self.ready.remove(&to);
                    self.renames.push((from, to));
                    self.last_event = Instant::now();
                }
            }
        }
    }

    pub fn periodic_rescan(&mut self, engine: &SyncEngine, label: &str) {
        if self.last_full_scan.elapsed().as_secs() < engine.full_scan_interval_secs() {
            return;
        }
        info!("{label}: periodic full re-scan starting …");
        let old = engine.get_manifest();
        match engine.scan() {
            Ok(new) => {
                let (changed, deleted) = crate::manifest::diff_manifests(&old, &new);
                if !changed.is_empty() {
                    info!(
                        "{label}: periodic scan found {} new/changed path(s)",
                        changed.len()
                    );
                    for p in changed {
                        self.deletes.remove(&p);
                        self.changes.remove(&p);
                        self.ready.insert(p);
                    }
                    self.last_event = Instant::now();
                }
                if !deleted.is_empty() {
                    info!(
                        "{label}: periodic scan found {} deleted path(s)",
                        deleted.len()
                    );
                    for p in deleted {
                        self.changes.remove(&p);
                        self.ready.remove(&p);
                        self.deletes.insert(p);
                    }
                    self.last_event = Instant::now();
                }
            }
            Err(e) => error!("{label}: periodic scan error: {e}"),
        }
        self.last_full_scan = Instant::now();
    }

    pub fn should_flush(&self) -> bool {
        let elapsed = self.last_event.elapsed().as_millis() as u64;
        elapsed >= DEBOUNCE_MS || elapsed >= MAX_BATCH_MS
    }

    pub fn reset_timer(&mut self) {
        self.last_event = Instant::now();
    }

    pub fn take_renames(&mut self) -> Vec<(PathBuf, PathBuf)> {
        std::mem::take(&mut self.renames)
    }

    pub fn take_ready(&mut self) -> Vec<PathBuf> {
        self.ready.drain().collect()
    }

    pub fn take_stable_changes(&mut self, engine: &SyncEngine) -> Vec<PathBuf> {
        let paths: Vec<PathBuf> = self.changes.drain().collect();
        let mut stable = Vec::new();
        for path in paths {
            if engine.is_file_stable(&path) {
                stable.push(path.clone());
                // Clear change history for this file since we're processing it
                engine.clear_change_history(&path);
            } else {
                self.changes.insert(path);
            }
        }
        stable
    }

    pub fn take_deletes(&mut self, root: &Path) -> (Vec<PathBuf>, usize) {
        let original_count = self.deletes.len();
        let mut paths: Vec<PathBuf> = self.deletes.drain().collect();
        expand_deleted_ancestors(root, &mut paths);
        (paths, original_count)
    }

    /// Drain pending deletes and record a local tombstone for each path
    /// BEFORE it is sent, so a later remote copy cannot resurrect it.
    /// Tombstones use wall-clock [`crate::ledger::now_ms`] and this node's
    /// id; prev hash/mtime come from the manifest when available.
    /// (Next lane: switch client/server flush paths to this method.)
    pub fn take_deletes_with_engine(
        &mut self,
        engine: &SyncEngine,
    ) -> (Vec<PathBuf>, usize) {
        let (paths, original_count) = self.take_deletes(engine.root());
        if !paths.is_empty() {
            let now = crate::ledger::now_ms();
            let node = engine.node_id().to_string();
            let manifest = engine.get_manifest();
            for p in &paths {
                let (prev_hash, prev_mtime_ms) = manifest
                    .files
                    .get(p)
                    .map(|m| (Some(crate::hex(&m.hash)), Some(m.modified_ms)))
                    .unwrap_or((None, None));
                engine.record_delete(p.clone(), now, &node, prev_hash, prev_mtime_ms);
            }
        }
        (paths, original_count)
    }
}

pub fn expand_deleted_ancestors(root: &Path, paths: &mut Vec<PathBuf>) {
    let mut extra = HashSet::new();
    for p in paths.iter() {
        let mut cur: &Path = p.as_ref();
        while let Some(parent) = cur.parent() {
            if parent.as_os_str().is_empty() {
                break;
            }
            if !extra.contains(parent) && !root.join(parent).exists() {
                extra.insert(parent.to_path_buf());
            }
            cur = parent;
        }
    }
    paths.extend(extra);
}

pub fn count_manifest(manifest: &Manifest) -> (usize, usize, u64) {
    let mut files = 0usize;
    let mut dirs = 0usize;
    let mut bytes: u64 = 0;
    for meta in manifest.files.values() {
        if meta.is_dir {
            dirs += 1;
        } else {
            files += 1;
            bytes += meta.size;
        }
    }
    (files, dirs, bytes)
}

pub fn publish_changed(bus: &Option<Arc<MessageBus>>, bundle: &FileBundle, node: &str) {
    if let Some(ref bus) = bus {
        for fd in &bundle.files {
            bus.publish(
                "filesync",
                "filesync.file_changed",
                serde_json::json!({ "path": fd.metadata.rel_path, "node": node }),
            );
        }
    }
}

pub struct BundleApplied {
    pub applied: usize,
    pub files_count: usize,
    pub dirs_count: usize,
    pub bytes: u64,
    pub conflicts: Vec<ConflictInfo>,
}

pub enum ChunkOutcome {
    Pending,
    Committed,
}

pub enum LargeFileEndOutcome {
    Committed,
    CommittedWithConflict(ConflictInfo),
    MissingChunks(Vec<u32>),
}

pub fn handle_recv_bundle(
    engine: &SyncEngine,
    bundle: &FileBundle,
    peer: &str,
    bus: &Option<Arc<MessageBus>>,
    log_prefix: &str,
) -> io::Result<BundleApplied> {
    let apply_result = engine.apply_bundle(bundle)?;

    let files_count = bundle.files.iter().filter(|f| !f.metadata.is_dir).count();
    let dirs_count = bundle.files.iter().filter(|f| f.metadata.is_dir).count();
    let bytes: u64 = bundle.files.iter().map(|f| f.metadata.size).sum();

    let applied = apply_result.written;

    info!("{log_prefix}: +{applied} file(s) from {peer}");
    publish_changed(bus, bundle, peer);

    for ci in &apply_result.conflicts {
        warn!(
            "{log_prefix}: conflict copy from {peer}: {:?} → {:?}",
            ci.original_path, ci.conflict_copy_path
        );
        if let Some(ref bus) = bus {
            bus.publish(
                "filesync",
                "filesync.conflict_copy",
                serde_json::json!({
                    "original":      ci.original_path,
                    "conflict_copy": ci.conflict_copy_path,
                    "peer":          peer,
                    "node":          engine.node_id(),
                }),
            );
        }
    }

    if files_count > 0 {
        if let Some(ref bus) = bus {
            bus.publish(
                "filesync",
                "filesync.incremental_stats",
                serde_json::json!({
                    "node":           engine.node_id(),
                    "peer":           peer,
                    "files_changed":  files_count,
                    "bytes_received": bytes,
                    "dirs_changed":   dirs_count,
                }),
            );
        }
    }

    Ok(BundleApplied {
        applied,
        files_count,
        dirs_count,
        bytes,
        conflicts: apply_result.conflicts,
    })
}

pub fn handle_recv_large_file_start(
    engine: &SyncEngine,
    metadata: FileMetadata,
    total_chunks: u32,
    peer: &str,
    log_prefix: &str,
) -> io::Result<()> {
    engine
        .begin_large_file(metadata, total_chunks)
        .map_err(|e| {
            error!("{log_prefix}: large_file_start from {peer}: {e}");
            e
        })
}

pub fn handle_recv_large_file_chunk(
    engine: &SyncEngine,
    path: &PathBuf,
    chunk_index: u32,
    data: &[u8],
    peer: &str,
    log_prefix: &str,
) -> io::Result<ChunkOutcome> {
    match engine.receive_large_file_chunk(path, chunk_index, data)? {
        ChunkResult::ReadyToCommit(hash) => {
            match engine.commit_large_file(path, hash).map_err(|e| {
                error!("{log_prefix}: large file commit after retransmit from {peer}: {e}");
                e
            })? {
                FinishResult::Committed => {}
                FinishResult::CommittedWithConflict(ci) => {
                    warn!(
                        "{log_prefix}: conflict copy during retransmit commit: {:?} → {:?}",
                        ci.original_path, ci.conflict_copy_path
                    );
                }
                FinishResult::MissingChunks(_) => {}
            }
            info!("{log_prefix}: large file committed {path:?} from {peer} (after retransmit)");
            Ok(ChunkOutcome::Committed)
        }
        ChunkResult::Pending => Ok(ChunkOutcome::Pending),
    }
}

pub fn handle_recv_large_file_end(
    engine: &SyncEngine,
    path: &PathBuf,
    final_hash: [u8; 32],
    peer: &str,
    bus: &Option<Arc<MessageBus>>,
    log_prefix: &str,
) -> io::Result<LargeFileEndOutcome> {
    match engine.finish_large_file(path, final_hash)? {
        FinishResult::Committed => {
            info!("{log_prefix}: large file committed {path:?} from {peer}");
            if let Some(ref bus) = bus {
                bus.publish(
                    "filesync",
                    "filesync.file_changed",
                    serde_json::json!({ "path": path, "node": peer }),
                );
                bus.publish(
                    "filesync",
                    "filesync.incremental_stats",
                    serde_json::json!({
                        "node":          engine.node_id(),
                        "peer":          peer,
                        "files_changed": 1,
                    }),
                );
            }
            Ok(LargeFileEndOutcome::Committed)
        }
        FinishResult::CommittedWithConflict(ci) => {
            info!(
                "{log_prefix}: large file committed {path:?} from {peer} \
                 (conflict copy: {:?})",
                ci.conflict_copy_path
            );
            if let Some(ref bus) = bus {
                bus.publish(
                    "filesync",
                    "filesync.conflict_copy",
                    serde_json::json!({
                        "original":      ci.original_path,
                        "conflict_copy": ci.conflict_copy_path,
                        "peer":          peer,
                        "node":          engine.node_id(),
                    }),
                );
                bus.publish(
                    "filesync",
                    "filesync.file_changed",
                    serde_json::json!({ "path": path, "node": peer }),
                );
                bus.publish(
                    "filesync",
                    "filesync.incremental_stats",
                    serde_json::json!({
                        "node":          engine.node_id(),
                        "peer":          peer,
                        "files_changed": 1,
                    }),
                );
            }
            Ok(LargeFileEndOutcome::CommittedWithConflict(ci))
        }
        FinishResult::MissingChunks(indices) => {
            warn!(
                "{log_prefix}: {path:?} from {peer} missing {} chunk(s), requesting retransmit",
                indices.len()
            );
            Ok(LargeFileEndOutcome::MissingChunks(indices))
        }
    }
}

/// Apply a remote delete, preserving the SENDER's timestamp/deleter for
/// last-writer-wins (never re-stamp locally).
pub fn handle_recv_delete_with_meta(
    engine: &SyncEngine,
    paths: &[PathBuf],
    deleted_at_ms: u64,
    deleter: &str,
    peer: &str,
    bus: &Option<Arc<MessageBus>>,
    log_prefix: &str,
) -> io::Result<usize> {
    let n = engine.apply_deletes(paths, deleted_at_ms, deleter)?;
    info!("{log_prefix}: -{n} path(s) from {peer}");
    if let Some(ref bus) = bus {
        bus.publish(
            "filesync",
            "filesync.file_deleted",
            serde_json::json!({
                "paths": paths,
                "node": peer,
                "deleted_at_ms": deleted_at_ms,
                "deleter": deleter,
            }),
        );
        bus.publish(
            "filesync",
            "filesync.incremental_stats",
            serde_json::json!({
                "node":          engine.node_id(),
                "peer":          peer,
                "files_deleted": n,
            }),
        );
    }
    Ok(n)
}

/// Legacy 5-arg entry point kept so the current client/server recv loops
/// compile untouched in this lane. Stamps with local wall-clock time;
/// prefer [`handle_recv_delete_with_meta`] (next lane switches callers to
/// it so sender timestamps survive for LWW).
pub fn handle_recv_delete(
    engine: &SyncEngine,
    paths: &[PathBuf],
    peer: &str,
    bus: &Option<Arc<MessageBus>>,
    log_prefix: &str,
) -> io::Result<usize> {
    handle_recv_delete_with_meta(
        engine,
        paths,
        crate::ledger::now_ms(),
        peer,
        peer,
        bus,
        log_prefix,
    )
}

pub fn handle_recv_rename(
    engine: &SyncEngine,
    from: &PathBuf,
    to: &PathBuf,
    peer: &str,
    bus: &Option<Arc<MessageBus>>,
    log_prefix: &str,
) -> io::Result<()> {
    engine.apply_rename(from, to)?;
    info!("{log_prefix}: renamed {from:?} → {to:?} from {peer}");
    if let Some(ref bus) = bus {
        bus.publish(
            "filesync",
            "filesync.file_renamed",
            serde_json::json!({
                "from": from,
                "to":   to,
                "node": peer,
            }),
        );
    }
    Ok(())
}

pub fn request_retransmit(conn: &Connection, path: &PathBuf, indices: Vec<u32>, log_prefix: &str) {
    if let Err(e) = conn.send(&Message::RequestChunks {
        path: path.clone(),
        chunk_indices: indices,
    }) {
        error!("{log_prefix}: send RequestChunks for {path:?}: {e}");
    }
}
