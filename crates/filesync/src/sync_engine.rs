use crate::bundler;
use crate::exclusions::Exclusions;
use crate::manifest;
use crate::protocol::FILE_STABILITY_MS;
use crate::protocol::*;
use crate::transport::Connection;
use crossbeam_channel::bounded;
use log::warn;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

const PIPELINE_DEPTH: usize = 128;

pub fn safe_relative(p: &Path) -> bool {
    p.components().all(|c| matches!(c, Component::Normal(_)))
}

pub enum ChunkResult {
    Pending,

    ReadyToCommit([u8; 32]),
}

#[derive(Debug)]
pub enum FinishResult {
    Committed,
    CommittedWithConflict(ConflictInfo),
    MissingChunks(Vec<u32>),
}

/// Information about a conflict that was resolved by creating a conflict copy.
#[derive(Debug, Clone)]
pub struct ConflictInfo {
    /// Original file path (incoming file applied here).
    pub original_path: PathBuf,
    /// Path where the diverged local copy was saved.
    pub conflict_copy_path: PathBuf,
}

/// Result of applying a bundle.
#[derive(Debug, Default)]
pub struct ApplyResult {
    /// Number of entries actually written (files + dirs).
    pub written: usize,
    /// Conflict copies created during this apply.
    pub conflicts: Vec<ConflictInfo>,
}

struct LargeFileAssembly {
    tmp_path: PathBuf,
    total_chunks: u32,

    received: HashSet<u32>,
    expected_hash: [u8; 32],
    dst: PathBuf,
    file_size: u64,

    final_hash_pending: Option<[u8; 32]>,
}

pub struct SyncEngine {
    root: PathBuf,
    node_id: String,
    manifest: RwLock<Manifest>,

    suppressed: Arc<RwLock<HashSet<PathBuf>>>,

    suppressed_deletes: Arc<RwLock<HashSet<PathBuf>>>,

    in_progress: RwLock<HashMap<PathBuf, LargeFileAssembly>>,

    exclusions: Arc<Exclusions>,
}

/// Returns the path that a conflict copy of `rel_path` should use.
///
/// Format: `{dir}/{stem} (conflict {unix_secs} {node_id}).{ext}`
/// If the file has no extension the extension suffix is omitted.
pub fn conflict_copy_name(rel_path: &Path, node_id: &str, unix_secs: u64) -> PathBuf {
    let stem = rel_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = rel_path.extension().and_then(|s| s.to_str());
    let conflict_filename = match ext {
        Some(e) => format!("{stem} (conflict {unix_secs} {node_id}).{e}"),
        None => format!("{stem} (conflict {unix_secs} {node_id})"),
    };
    match rel_path.parent().filter(|p| *p != Path::new("")) {
        Some(parent) => parent.join(conflict_filename),
        None => PathBuf::from(conflict_filename),
    }
}

/// Compute the BLAKE3 hash of a file on disk.  Returns `None` if the file
/// cannot be read (e.g. does not exist yet).
fn hash_file(path: &Path) -> Option<[u8; 32]> {
    use std::io::Read;
    let file = fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hasher.finalize().into())
}

/// Move a file or directory into the `.bh_filesync/trash` folder.
/// The item is renamed to `<unix_ms>_<original_name>` inside the trash dir.
/// If rename fails (e.g. cross-device), falls back to a hard delete.
fn move_to_trash(root: &Path, full_path: &Path, rel: &Path) {
    if !full_path.exists() {
        return;
    }
    let trash_dir = root.join(crate::protocol::TRASH_DIR);
    if fs::create_dir_all(&trash_dir).is_err() {
        // Can't create trash — fall back to hard delete
        if full_path.is_dir() {
            let _ = fs::remove_dir_all(full_path);
        } else {
            let _ = fs::remove_file(full_path);
        }
        return;
    }

    let file_name = rel
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("unknown"))
        .to_string_lossy();
    let unix_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let trash_dest = trash_dir.join(format!("{unix_ms}_{file_name}"));

    if fs::rename(full_path, &trash_dest).is_err() {
        // Cross-device or permission issue — fall back to hard delete
        warn!("trash rename failed for {rel:?}; falling back to delete");
        if full_path.is_dir() {
            let _ = fs::remove_dir_all(full_path);
        } else {
            let _ = fs::remove_file(full_path);
        }
    }
}

/// Remove empty intermediate directories left behind inside the transfers
/// folder after a large-file tmp file has been consumed or deleted.
/// Walks upward from `tmp_path`'s parent, removing directories that are
/// empty, stopping when it reaches the transfers root or a non-empty dir.
fn cleanup_transfer_dirs(root: &Path, tmp_path: &Path) {
    let transfers_dir = root.join(crate::protocol::TMP_DIR);
    let mut current = tmp_path.parent();
    while let Some(dir) = current {
        if dir == transfers_dir || !dir.starts_with(&transfers_dir) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            // Non-empty or permission error — stop climbing
            break;
        }
        current = dir.parent();
    }
}

impl SyncEngine {
    pub fn new(root: PathBuf, node_id: String, exclusions: Arc<Exclusions>) -> Self {
        log::debug!(
            "SyncEngine: {} active exclusion rule(s)",
            exclusions.rule_count()
        );
        Self {
            manifest: RwLock::new(Manifest {
                files: HashMap::new(),
                node_id: node_id.clone(),
            }),
            root,
            node_id,
            suppressed: Arc::new(RwLock::new(HashSet::new())),
            suppressed_deletes: Arc::new(RwLock::new(HashSet::new())),
            in_progress: RwLock::new(HashMap::new()),
            exclusions,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn is_excluded(&self, rel: &Path) -> bool {
        self.exclusions.is_excluded(rel)
    }

    pub fn is_suppressed(&self, p: &Path) -> bool {
        self.suppressed.read().contains(p)
    }

    pub fn is_delete_suppressed(&self, p: &Path) -> bool {
        self.suppressed_deletes.read().contains(p)
    }

    pub fn is_file_stable(&self, rel: &Path) -> bool {
        let full = self.root.join(rel);

        let meta = match std::fs::metadata(&full) {
            Ok(m) => m,
            Err(_) => return false,
        };

        if meta.is_dir() {
            return true;
        }

        let modified_ms = match meta.modified() {
            Ok(t) => t
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            Err(_) => return false,
        };

        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let age_ms = now_ms.saturating_sub(modified_ms);
        age_ms >= FILE_STABILITY_MS
    }

    pub fn scan(&self) -> std::io::Result<Manifest> {
        let m = manifest::build_manifest(&self.root, &self.node_id, &self.exclusions)?;
        *self.manifest.write() = m.clone();
        Ok(m)
    }

    pub fn get_manifest(&self) -> Manifest {
        self.manifest.read().clone()
    }

    pub fn send_paths(&self, paths: &[PathBuf], conn: &Arc<Connection>) -> std::io::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }

        let chunk_size = (paths.len() + READ_THREADS - 1) / READ_THREADS;
        let (msg_tx, msg_rx) = bounded::<Message>(PIPELINE_DEPTH);

        let handles: Vec<_> = paths
            .chunks(chunk_size)
            .map(|slice| {
                let root = self.root.clone();
                let slice = slice.to_vec();
                let tx = msg_tx.clone();
                thread::Builder::new()
                    .name("file-reader".into())
                    .spawn(move || {
                        bundler::stream_messages(&root, &slice, &tx);
                    })
                    .expect("spawn file-reader")
            })
            .collect();

        drop(msg_tx);

        for msg in msg_rx {
            conn.send(&msg)?;
        }

        for h in handles {
            h.join().ok();
        }
        Ok(())
    }

    pub fn create_bundles(&self, paths: &[PathBuf]) -> Vec<FileBundle> {
        let (tx, rx) = bounded::<Message>(PIPELINE_DEPTH);
        let root = self.root.clone();
        let paths = paths.to_vec();
        thread::spawn(move || bundler::stream_messages(&root, &paths, &tx));
        rx.into_iter()
            .filter_map(|m| {
                if let Message::Bundle(b) = m {
                    Some(b)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns `Some(manifest_hash)` when both the on-disk file and the incoming
    /// file have diverged from the recorded manifest hash (i.e. a genuine
    /// two-sided conflict).  Returns `None` when there is no conflict.
    fn detect_conflict(
        &self,
        rel_path: &Path,
        full_path: &Path,
        incoming_hash: &[u8; 32],
    ) -> Option<[u8; 32]> {
        // Must be a known file (present in our manifest)
        let manifest_hash = self.manifest.read().files.get(rel_path).map(|m| m.hash)?;
        // The incoming file must differ from the last-synced state
        if incoming_hash == &manifest_hash {
            return None;
        }
        // The on-disk file must also differ from the last-synced state
        let on_disk_hash = hash_file(full_path)?;
        if on_disk_hash == manifest_hash {
            return None; // local never changed — no conflict
        }
        Some(manifest_hash)
    }

    pub fn apply_bundle(&self, bundle: &FileBundle) -> std::io::Result<ApplyResult> {
        let mut written = Vec::new();
        let mut result = ApplyResult::default();

        for fd in &bundle.files {
            if !safe_relative(&fd.metadata.rel_path) {
                warn!("rejected unsafe path: {:?}", fd.metadata.rel_path);
                continue;
            }

            let full = self.root.join(&fd.metadata.rel_path);

            // --- Conflict detection (files only) ---
            if !fd.metadata.is_dir {
                if let Some(_ancestor_hash) =
                    self.detect_conflict(&fd.metadata.rel_path, &full, &fd.metadata.hash)
                {
                    let unix_secs = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let conflict_rel =
                        conflict_copy_name(&fd.metadata.rel_path, &self.node_id, unix_secs);
                    let conflict_full = self.root.join(&conflict_rel);
                    if let Some(p) = conflict_full.parent() {
                        fs::create_dir_all(p)?;
                    }
                    match fs::copy(&full, &conflict_full) {
                        Ok(_) => {
                            log::info!(
                                "conflict: {:?} diverged; local copy saved as {:?}",
                                fd.metadata.rel_path,
                                conflict_rel
                            );
                            result.conflicts.push(ConflictInfo {
                                original_path: fd.metadata.rel_path.clone(),
                                conflict_copy_path: conflict_rel,
                            });
                        }
                        Err(e) => {
                            warn!("conflict copy failed for {:?}: {e}", fd.metadata.rel_path);
                        }
                    }
                }
            }

            // --- Apply the incoming file ---
            self.suppressed.write().insert(fd.metadata.rel_path.clone());
            written.push(fd.metadata.rel_path.clone());

            if fd.metadata.is_dir {
                fs::create_dir_all(&full)?;
            } else {
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&full, &fd.content)?;
            }

            self.manifest
                .write()
                .files
                .insert(fd.metadata.rel_path.clone(), fd.metadata.clone());
            result.written += 1;
        }

        self.schedule_unsuppress(written);
        Ok(result)
    }

    pub fn begin_large_file(
        &self,
        metadata: FileMetadata,
        total_chunks: u32,
    ) -> std::io::Result<()> {
        if !safe_relative(&metadata.rel_path) {
            warn!("rejected unsafe large-file path: {:?}", metadata.rel_path);
            return Ok(());
        }

        let dst = self.root.join(&metadata.rel_path);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp_path = self
            .root
            .join(crate::protocol::TMP_DIR)
            .join(&metadata.rel_path)
            .with_extension("tmp");
        if let Some(p) = tmp_path.parent() {
            fs::create_dir_all(p)?;
        }

        {
            let f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)?;
            f.set_len(metadata.size)?;
        }

        {
            let tmp_rel = tmp_path
                .strip_prefix(&self.root)
                .unwrap_or(&tmp_path)
                .to_path_buf();
            let mut sup = self.suppressed.write();
            sup.insert(metadata.rel_path.clone());
            sup.insert(tmp_rel);
        }

        self.in_progress.write().insert(
            metadata.rel_path.clone(),
            LargeFileAssembly {
                tmp_path,
                total_chunks,
                received: HashSet::new(),
                expected_hash: metadata.hash,
                dst,
                file_size: metadata.size,
                final_hash_pending: None,
            },
        );
        Ok(())
    }

    pub fn receive_large_file_chunk(
        &self,
        path: &PathBuf,
        chunk_index: u32,
        data: &[u8],
    ) -> std::io::Result<ChunkResult> {
        use std::io::{Seek, SeekFrom, Write};

        let mut map = self.in_progress.write();
        let asm = match map.get_mut(path) {
            Some(a) => a,
            None => {
                warn!("chunk for unknown large file: {path:?}");
                return Ok(ChunkResult::Pending);
            }
        };

        let offset = chunk_index as u64 * FILE_CHUNK_SIZE as u64;
        let mut f = fs::OpenOptions::new().write(true).open(&asm.tmp_path)?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)?;

        asm.received.insert(chunk_index);

        if let Some(hash) = asm.final_hash_pending {
            if asm.received.len() as u32 == asm.total_chunks {
                return Ok(ChunkResult::ReadyToCommit(hash));
            }
        }

        Ok(ChunkResult::Pending)
    }

    pub fn finish_large_file(
        &self,
        path: &PathBuf,
        final_hash: [u8; 32],
    ) -> std::io::Result<FinishResult> {
        {
            let mut map = self.in_progress.write();
            let asm = match map.get_mut(path) {
                Some(a) => a,
                None => {
                    warn!("LargeFileEnd for unknown path: {path:?}");
                    return Ok(FinishResult::Committed);
                }
            };

            if (asm.received.len() as u32) < asm.total_chunks {
                let missing: Vec<u32> = (0..asm.total_chunks)
                    .filter(|i| !asm.received.contains(i))
                    .collect();
                asm.final_hash_pending = Some(final_hash);
                return Ok(FinishResult::MissingChunks(missing));
            }
        }

        self.commit_large_file(path, final_hash)
    }

    pub fn commit_large_file(
        &self,
        path: &PathBuf,
        final_hash: [u8; 32],
    ) -> std::io::Result<FinishResult> {
        let asm = match self.in_progress.write().remove(path) {
            Some(a) => a,
            None => {
                warn!("commit_large_file for unknown path: {path:?}");
                return Ok(FinishResult::Committed);
            }
        };

        {
            use std::io::Read;
            let mut hasher = blake3::Hasher::new();
            let file = fs::File::open(&asm.tmp_path)?;
            let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            let got: [u8; 32] = hasher.finalize().into();
            if got != final_hash {
                let _ = fs::remove_file(&asm.tmp_path);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("large file hash mismatch for {path:?}"),
                ));
            }
        }

        // --- Conflict detection ---
        let conflict = {
            let manifest_hash = self.manifest.read().files.get(path).map(|m| m.hash);
            if let Some(manifest_hash) = manifest_hash {
                if final_hash != manifest_hash {
                    if let Some(on_disk_hash) = hash_file(&asm.dst) {
                        if on_disk_hash != manifest_hash {
                            let unix_secs = SystemTime::now()
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            let conflict_rel = conflict_copy_name(path, &self.node_id, unix_secs);
                            let conflict_full = self.root.join(&conflict_rel);
                            if let Some(p) = conflict_full.parent() {
                                let _ = fs::create_dir_all(p);
                            }
                            match fs::copy(&asm.dst, &conflict_full) {
                                Ok(_) => {
                                    log::info!(
                                        "conflict: large file {:?} diverged; \
                                         local copy saved as {:?}",
                                        path,
                                        conflict_rel
                                    );
                                    Some(ConflictInfo {
                                        original_path: path.clone(),
                                        conflict_copy_path: conflict_rel,
                                    })
                                }
                                Err(e) => {
                                    warn!("conflict copy failed for large file {:?}: {e}", path);
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        };

        fs::rename(&asm.tmp_path, &asm.dst)?;
        cleanup_transfer_dirs(&self.root, &asm.tmp_path);

        let meta = fs::metadata(&asm.dst)?;
        let modified_ms = meta
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let file_meta = FileMetadata {
            rel_path: path.clone(),
            size: meta.len(),
            hash: final_hash,
            modified_ms,
            is_dir: false,
        };
        self.manifest.write().files.insert(path.clone(), file_meta);

        let tmp_rel = asm
            .tmp_path
            .strip_prefix(&self.root)
            .unwrap_or(&asm.tmp_path)
            .to_path_buf();
        self.schedule_unsuppress(vec![path.clone(), tmp_rel]);
        if let Some(ci) = conflict {
            Ok(FinishResult::CommittedWithConflict(ci))
        } else {
            Ok(FinishResult::Committed)
        }
    }

    pub fn clear_in_progress(&self) {
        let mut map = self.in_progress.write();
        for (path, asm) in map.drain() {
            if let Err(e) = std::fs::remove_file(&asm.tmp_path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!("clear_in_progress: removing tmp for {path:?}: {e}");
                }
            }
            cleanup_transfer_dirs(&self.root, &asm.tmp_path);
        }
    }

    pub fn apply_rename(&self, from: &PathBuf, to: &PathBuf) -> std::io::Result<()> {
        if !safe_relative(from) || !safe_relative(to) {
            warn!("apply_rename: rejected unsafe path(s): {from:?} → {to:?}");
            return Ok(());
        }

        let src = self.root.join(from);
        let dst = self.root.join(to);

        if !src.exists() {
            log::debug!("apply_rename: source absent, skipping: {from:?}");
            return Ok(());
        }

        {
            let mut sup = self.suppressed.write();
            sup.insert(from.clone());
            sup.insert(to.clone());
        }

        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::rename(&src, &dst)?;

        {
            let mut m = self.manifest.write();
            if let Some(mut meta) = m.files.remove(from) {
                meta.rel_path = to.clone();

                if let Ok(fs_meta) = fs::metadata(&dst) {
                    if let Ok(modified) = fs_meta.modified() {
                        if let Ok(dur) = modified.duration_since(std::time::SystemTime::UNIX_EPOCH)
                        {
                            meta.modified_ms = dur.as_millis() as u64;
                        }
                    }
                }
                m.files.insert(to.clone(), meta);
            }
        }

        log::info!("apply_rename: {from:?} → {to:?}");
        self.schedule_unsuppress(vec![from.clone(), to.clone()]);
        Ok(())
    }

    pub fn apply_deletes(&self, paths: &[PathBuf]) -> std::io::Result<usize> {
        let mut removed = Vec::new();
        let mut count = 0usize;

        for rel in paths {
            if !safe_relative(rel) {
                continue;
            }

            self.suppressed_deletes.write().insert(rel.clone());
            removed.push(rel.clone());

            let full = self.root.join(rel);
            if full.is_dir() {
                move_to_trash(&self.root, &full, rel);
                let children: Vec<PathBuf> = self
                    .manifest
                    .read()
                    .files
                    .keys()
                    .filter(|p| p.starts_with(rel))
                    .cloned()
                    .collect();
                let mut m = self.manifest.write();
                for child in children {
                    m.files.remove(&child);
                }
                m.files.remove(rel);
            } else {
                move_to_trash(&self.root, &full, rel);
                self.manifest.write().files.remove(rel);
            }
            count += 1;
        }

        self.schedule_unsuppress_deletes(removed);
        Ok(count)
    }

    fn schedule_unsuppress(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let sup = self.suppressed.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(SUPPRESSION_SECS));
            let mut set = sup.write();
            for p in &paths {
                set.remove(p);
            }
        });
    }

    fn schedule_unsuppress_deletes(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let sup = self.suppressed_deletes.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(SUPPRESSION_SECS));
            let mut set = sup.write();
            for p in &paths {
                set.remove(p);
            }
        });
    }
}
