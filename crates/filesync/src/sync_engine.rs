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

    MissingChunks(Vec<u32>),
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

    pub fn apply_bundle(&self, bundle: &FileBundle) -> std::io::Result<usize> {
        let mut written = Vec::new();
        let mut count = 0usize;

        for fd in &bundle.files {
            if !safe_relative(&fd.metadata.rel_path) {
                warn!("rejected unsafe path: {:?}", fd.metadata.rel_path);
                continue;
            }

            let full = self.root.join(&fd.metadata.rel_path);
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
            count += 1;
        }

        self.schedule_unsuppress(written);
        Ok(count)
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

        fs::rename(&asm.tmp_path, &asm.dst)?;

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
        Ok(FinishResult::Committed)
    }

    pub fn clear_in_progress(&self) {
        let mut map = self.in_progress.write();
        for (path, asm) in map.drain() {
            if let Err(e) = std::fs::remove_file(&asm.tmp_path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!("clear_in_progress: removing tmp for {path:?}: {e}");
                }
            }
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
                let _ = fs::remove_dir_all(&full);
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
                let _ = fs::remove_file(&full);
                let _ = fs::remove_dir(&full);
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
