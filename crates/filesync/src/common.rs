//! Shared helpers used by both the filesync **client** and **server**.
//!
//! Includes preemptive disk-space checking via [`check_disk_space`] /
//! [`available_disk_space`], which are called after a manifest exchange to
//! abort the sync early when the local filesystem has insufficient room.
//!
//! Everything that was duplicated between `client.rs` and `server.rs` lives
//! here so that bug-fixes and behavioural changes only need to happen once.

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

// ── Disk-space helpers ────────────────────────────────────────────────────

/// Returns the number of bytes currently available on the filesystem that
/// contains `path`.  Thin wrapper over [`fs2::available_space`].
pub fn available_disk_space(path: &Path) -> io::Result<u64> {
    fs2::available_space(path)
}

/// Asserts that at least `required_bytes` are available on the filesystem
/// that contains `path`.
///
/// * `Ok(available)` – there is enough space; `available` is the free-byte
///   count at the time of the check.
/// * `Err(StorageFull)` – not enough space; the error message includes both
///   the available and required byte counts.
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

// ── Pending-changes accumulator ──────────────────────────────────────────

const MAX_BATCH_MS: u64 = 2_000;

/// Accumulates filesystem events and decides when they should be flushed to
/// the network.  Used identically by the client send-loop and the server
/// local-change broadcaster.
pub struct PendingChanges {
    /// Paths that have been *changed* but may not yet be stable on disk.
    pub changes: HashSet<PathBuf>,
    /// Paths whose writes are complete (ready to send immediately).
    pub ready: HashSet<PathBuf>,
    /// Paths that have been deleted locally.
    pub deletes: HashSet<PathBuf>,
    /// Rename pairs `(from, to)`.
    pub renames: Vec<(PathBuf, PathBuf)>,
    /// Timestamp of the most recent event (used for debouncing).
    pub last_event: Instant,
    /// Timestamp of the most recent periodic full re-scan.
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

    // ── event collection ─────────────────────────────────────────────

    /// Incorporate a single filesystem event into the pending sets.
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

    // ── periodic re-scan ─────────────────────────────────────────────

    /// Run a full re-scan of the root directory if enough time has elapsed
    /// since the last one.  Any newly-discovered changes or deletions are
    /// folded into the pending sets.
    pub fn periodic_rescan(&mut self, engine: &SyncEngine, label: &str) {
        if self.last_full_scan.elapsed().as_secs() < FULL_SCAN_INTERVAL_SECS {
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

    // ── flush helpers ────────────────────────────────────────────────

    /// Returns `true` when enough time has elapsed since the last event to
    /// justify a network flush.
    pub fn should_flush(&self) -> bool {
        let elapsed = self.last_event.elapsed().as_millis() as u64;
        elapsed >= DEBOUNCE_MS || elapsed >= MAX_BATCH_MS
    }

    /// Reset the event timer (call after a successful flush).
    pub fn reset_timer(&mut self) {
        self.last_event = Instant::now();
    }

    /// Drain all pending renames.
    pub fn take_renames(&mut self) -> Vec<(PathBuf, PathBuf)> {
        std::mem::take(&mut self.renames)
    }

    /// Drain all ready (write-complete) paths.
    pub fn take_ready(&mut self) -> Vec<PathBuf> {
        self.ready.drain().collect()
    }

    /// Drain changed paths whose files are *stable* on disk.
    /// Unstable paths are returned to the `changes` set for a later flush.
    pub fn take_stable_changes(&mut self, engine: &SyncEngine) -> Vec<PathBuf> {
        let paths: Vec<PathBuf> = self.changes.drain().collect();
        let mut stable = Vec::new();
        for path in paths {
            if engine.is_file_stable(&path) {
                stable.push(path);
            } else {
                self.changes.insert(path);
            }
        }
        stable
    }

    /// Drain all pending deletes, expanding empty ancestor directories.
    /// Returns `(expanded_paths, original_count)` so callers that need the
    /// pre-expansion count (e.g. for stats) can use it.
    pub fn take_deletes(&mut self, root: &Path) -> (Vec<PathBuf>, usize) {
        let original_count = self.deletes.len();
        let mut paths: Vec<PathBuf> = self.deletes.drain().collect();
        expand_deleted_ancestors(root, &mut paths);
        (paths, original_count)
    }
}

// ── Shared utilities ─────────────────────────────────────────────────────

/// Walk up from each deleted path and add any ancestor directories that no
/// longer exist on disk.  This ensures that empty parent directories created
/// solely by sync are cleaned up on the remote side as well.
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

/// Count the files, directories, and total bytes in a manifest snapshot.
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

/// Publish a `filesync.file_changed` bus event for every file in a bundle.
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

// ── Incoming-message handler return types ─────────────────────────────────

/// Statistics returned after applying a received bundle.
pub struct BundleApplied {
    /// Number of entries the engine actually wrote (from `apply_bundle`).
    pub applied: usize,
    /// Number of non-directory files in the bundle.
    pub files_count: usize,
    /// Number of directories in the bundle.
    pub dirs_count: usize,
    /// Total bytes across all entries in the bundle.
    pub bytes: u64,
    /// Conflict copies created while applying this bundle.
    pub conflicts: Vec<ConflictInfo>,
}

/// Outcome of processing a `LargeFileChunk` message.
pub enum ChunkOutcome {
    /// More chunks are expected.
    Pending,
    /// All chunks received; the file has been committed.
    Committed,
}

/// Outcome of processing a `LargeFileEnd` message.
pub enum LargeFileEndOutcome {
    /// The large file was successfully committed to disk.
    Committed,
    /// Some chunks were lost in transit and must be retransmitted.
    MissingChunks(Vec<u32>),
}

// ── Incoming-message handlers ────────────────────────────────────────────
//
// Each handler performs the engine operation, logs the result, and publishes
// relevant bus events.  The caller is responsible for any additional actions
// such as broadcasting to other peers (server) or updating GUI state
// (client).

/// Apply a received `Bundle` to the engine.  Publishes `file_changed` and
/// `incremental_stats` bus events.
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

/// Begin tracking a new large-file transfer.  Logs on error.
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

/// Process a single large-file chunk.  If all chunks are now present the
/// file is committed automatically.
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

/// Finalise a large-file transfer.  Publishes bus events on success.
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
            Ok(LargeFileEndOutcome::Committed)
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

/// Apply incoming deletes.  Publishes `file_deleted` and
/// `incremental_stats` bus events.
pub fn handle_recv_delete(
    engine: &SyncEngine,
    paths: &[PathBuf],
    peer: &str,
    bus: &Option<Arc<MessageBus>>,
    log_prefix: &str,
) -> io::Result<usize> {
    let n = engine.apply_deletes(paths)?;
    info!("{log_prefix}: -{n} path(s) from {peer}");
    if let Some(ref bus) = bus {
        bus.publish(
            "filesync",
            "filesync.file_deleted",
            serde_json::json!({ "paths": paths, "node": peer }),
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

/// Apply an incoming rename.  Publishes a `file_renamed` bus event.
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

/// Send a `RequestChunks` message for a large file that is missing data.
pub fn request_retransmit(conn: &Connection, path: &PathBuf, indices: Vec<u32>, log_prefix: &str) {
    if let Err(e) = conn.send(&Message::RequestChunks {
        path: path.clone(),
        chunk_indices: indices,
    }) {
        error!("{log_prefix}: send RequestChunks for {path:?}: {e}");
    }
}
