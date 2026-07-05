use crate::bundler;
use crate::exclusions::Exclusions;
use crate::manifest;
use crate::protocol::FILE_STABILITY_MS;
use crate::protocol::*;
use crate::transport::Connection;
use bytehive_core::TrashManager;
use crossbeam_channel::bounded;
use log::warn;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const PIPELINE_DEPTH: usize = 128;

#[derive(Debug, Clone)]
struct FileChangeHistory {
    last_change_time: Instant,
    change_count: u32,
    last_sequence_number: u64,
}

impl FileChangeHistory {
    fn new(sequence_number: u64) -> Self {
        Self {
            last_change_time: Instant::now(),
            change_count: 1,
            last_sequence_number: sequence_number,
        }
    }

    fn record_change(&mut self, sequence_number: u64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_change_time).as_millis() as u64;

        self.last_change_time = now;
        self.change_count += 1;
        self.last_sequence_number = sequence_number;

        elapsed < FILE_CHANGE_COALESCE_MS
    }

    fn should_coalesce(&self) -> bool {
        self.change_count > 1
    }
}

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

#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub original_path: PathBuf,
    pub conflict_copy_path: PathBuf,
}

#[derive(Debug, Default)]
pub struct ApplyResult {
    pub written: usize,
    pub conflicts: Vec<ConflictInfo>,
}

struct LargeFileAssembly {
    tmp_path: PathBuf,
    total_chunks: u32,

    received: HashSet<u32>,
    expected_hash: [u8; 32],
    dst: PathBuf,
    file_size: u64,
    modified_ms: u64,

    final_hash_pending: Option<[u8; 32]>,
}

#[derive(Debug, Default, Clone)]
pub struct SyncEngineConfig {
    pub trash_expiry_days: Option<u64>,
    pub full_scan_interval_secs: Option<u64>,
}

pub struct SyncEngine {
    root: PathBuf,
    node_id: String,
    manifest: RwLock<Manifest>,
    suppressed: Arc<RwLock<HashSet<PathBuf>>>,
    suppressed_deletes: Arc<RwLock<HashSet<PathBuf>>>,
    in_progress: RwLock<HashMap<PathBuf, LargeFileAssembly>>,
    exclusions: Arc<Exclusions>,
    trash_manager: Arc<TrashManager>,
    full_scan_interval_secs: u64,
    change_sequences: RwLock<HashMap<PathBuf, FileChangeHistory>>,
    last_sequence_number: RwLock<u64>,
}

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

fn cleanup_transfer_dirs(root: &Path, tmp_path: &Path) {
    let transfers_dir = root.join(crate::protocol::TMP_DIR);
    let mut current = tmp_path.parent();
    while let Some(dir) = current {
        if dir == transfers_dir || !dir.starts_with(&transfers_dir) {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        current = dir.parent();
    }
}

impl SyncEngine {
    pub fn new(root: PathBuf, node_id: String, exclusions: Arc<Exclusions>) -> Self {
        Self::new_configured(root, node_id, exclusions, SyncEngineConfig::default())
    }

    pub fn new_configured(
        root: PathBuf,
        node_id: String,
        exclusions: Arc<Exclusions>,
        config: SyncEngineConfig,
    ) -> Self {
        log::debug!(
            "SyncEngine: {} active exclusion rule(s)",
            exclusions.rule_count()
        );
        let full_scan_interval_secs = config
            .full_scan_interval_secs
            .filter(|&v| v > 0)
            .unwrap_or(FULL_SCAN_INTERVAL_SECS);
        log::info!(
            "SyncEngine: full_scan_interval_secs = {}s ({})",
            full_scan_interval_secs,
            if config
                .full_scan_interval_secs
                .map(|v| v > 0)
                .unwrap_or(false)
            {
                "from config"
            } else {
                "default"
            }
        );
        let trash_manager = TrashManager::new(root.clone(), config.trash_expiry_days);
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
            trash_manager,
            full_scan_interval_secs,
            change_sequences: RwLock::new(HashMap::new()),
            last_sequence_number: RwLock::new(0),
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

        if age_ms >= FILE_STABILITY_MS {
            return true;
        }
        self.has_recent_rapid_changes(rel, modified_ms)
    }

    pub fn full_scan_interval_secs(&self) -> u64 {
        self.full_scan_interval_secs
    }

    pub fn has_recent_rapid_changes(&self, rel: &Path, _current_modified_ms: u64) -> bool {
        let history = self.change_sequences.read();
        if let Some(file_history) = history.get(rel) {
            file_history.should_coalesce()
        } else {
            false
        }
    }

    pub fn record_file_change(&self, rel: &Path) -> u64 {
        let mut seq_lock = self.last_sequence_number.write();
        *seq_lock += 1;
        let sequence_number = *seq_lock;

        let mut histories = self.change_sequences.write();
        let entry = histories
            .entry(rel.to_path_buf())
            .or_insert_with(|| FileChangeHistory::new(sequence_number));

        entry.record_change(sequence_number);
        sequence_number
    }

    pub fn get_current_sequence_number(&self) -> u64 {
        *self.last_sequence_number.read()
    }

    pub fn clear_change_history(&self, rel: &Path) {
        let mut histories = self.change_sequences.write();
        histories.remove(rel);
    }

    pub fn clone_with_fresh_state(&self) -> Self {
        Self {
            root: self.root.clone(),
            node_id: self.node_id.clone(),
            manifest: RwLock::new(self.manifest.read().clone()),
            suppressed: self.suppressed.clone(),
            suppressed_deletes: self.suppressed_deletes.clone(),
            in_progress: RwLock::new(HashMap::new()),
            exclusions: self.exclusions.clone(),
            trash_manager: self.trash_manager.clone(),
            full_scan_interval_secs: self.full_scan_interval_secs,
            change_sequences: RwLock::new(HashMap::new()),
            last_sequence_number: RwLock::new(0),
        }
    }

    pub fn trash_manager(&self) -> &Arc<TrashManager> {
        &self.trash_manager
    }

    pub fn list_trash(&self) -> Vec<bytehive_core::TrashEntry> {
        self.trash_manager.list_trash()
    }

    pub fn restore_trash_entry(&self, id: &str) -> Result<(), String> {
        self.trash_manager.restore_entry(id)
    }

    pub fn purge_trash_entry(&self, id: &str) -> Result<(), String> {
        self.trash_manager.purge_entry(id)
    }

    pub fn purge_expired_trash(&self) -> usize {
        self.trash_manager.purge_expired()
    }

    pub fn empty_trash(&self) -> usize {
        self.trash_manager.empty()
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
                        bundler::stream_messages(&root, &slice, &tx, None);
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
        let engine = self.clone_with_fresh_state();
        thread::spawn(move || bundler::stream_messages(&root, &paths, &tx, Some(Arc::new(engine))));
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

    fn detect_conflict(
        &self,
        rel_path: &Path,
        full_path: &Path,
        incoming_hash: &[u8; 32],
    ) -> Option<[u8; 32]> {
        let manifest_hash = self.manifest.read().files.get(rel_path).map(|m| m.hash)?;
        if incoming_hash == &manifest_hash {
            return None;
        }
        let on_disk_hash = hash_file(full_path)?;
        if on_disk_hash == manifest_hash {
            return None;
        }
        if &on_disk_hash == incoming_hash {
            return None;
        }
        Some(manifest_hash)
    }

    pub fn apply_bundle(&self, bundle: &FileBundle) -> std::io::Result<ApplyResult> {
        let mut written = Vec::new();
        let mut result = ApplyResult::default();
        let mut dir_mtimes: Vec<(PathBuf, u64)> = Vec::new();

        for fd in &bundle.files {
            if !safe_relative(&fd.metadata.rel_path) {
                warn!("rejected unsafe path: {:?}", fd.metadata.rel_path);
                continue;
            }

            let full = self.root.join(&fd.metadata.rel_path);

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

            self.suppressed.write().insert(fd.metadata.rel_path.clone());
            written.push(fd.metadata.rel_path.clone());

            if fd.metadata.is_dir {
                fs::create_dir_all(&full)?;
                dir_mtimes.push((full.clone(), fd.metadata.modified_ms));
            } else {
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&full, &fd.content)?;
                let mtime = filetime::FileTime::from_unix_time(
                    (fd.metadata.modified_ms / 1000) as i64,
                    ((fd.metadata.modified_ms % 1000) * 1_000_000) as u32,
                );
                if let Err(e) = filetime::set_file_mtime(&full, mtime) {
                    warn!("failed to set mtime on {:?}: {e}", full);
                }
            }

            self.manifest
                .write()
                .files
                .insert(fd.metadata.rel_path.clone(), fd.metadata.clone());
            result.written += 1;
        }

        dir_mtimes.sort_by(|a, b| b.0.components().count().cmp(&a.0.components().count()));
        for (path, modified_ms) in dir_mtimes {
            let mtime = filetime::FileTime::from_unix_time(
                (modified_ms / 1000) as i64,
                ((modified_ms % 1000) * 1_000_000) as u32,
            );
            if let Err(e) = filetime::set_file_mtime(&path, mtime) {
                warn!("failed to set mtime on dir {:?}: {e}", path);
            }
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
                modified_ms: metadata.modified_ms,
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

        let conflict = {
            let manifest_hash = self.manifest.read().files.get(path).map(|m| m.hash);
            if let Some(manifest_hash) = manifest_hash {
                if final_hash != manifest_hash {
                    if let Some(on_disk_hash) = hash_file(&asm.dst) {
                        if on_disk_hash != manifest_hash && on_disk_hash != final_hash {
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

        let mtime = filetime::FileTime::from_unix_time(
            (asm.modified_ms / 1000) as i64,
            ((asm.modified_ms % 1000) * 1_000_000) as u32,
        );
        if let Err(e) = filetime::set_file_mtime(&asm.dst, mtime) {
            warn!("failed to set mtime on large file {:?}: {e}", asm.dst);
        }

        let file_meta = FileMetadata {
            rel_path: path.clone(),
            size: asm.file_size,
            hash: final_hash,
            modified_ms: asm.modified_ms,
            change_sequence: 0,
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
                self.trash_manager.move_to_trash(&full, rel, &self.node_id);
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
                self.trash_manager.move_to_trash(&full, rel, &self.node_id);
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
