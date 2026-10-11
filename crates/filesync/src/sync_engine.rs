use crate::action_journal::ActionJournal;
use crate::bundler;
use crate::exclusions::Exclusions;
use crate::ledger::{now_ms, DeletionLedger, Tombstone, DELETION_LEDGER_TTL_MS};
use crate::manifest;
use crate::manifest_cache::ManifestCache;
use crate::protocol::FILE_STABILITY_MS;
use crate::protocol::*;
use crate::transport::Connection;
use crate::watcher::FsEvent;
use bytehive_core::TrashManager;
use crossbeam_channel::bounded;
use log::warn;
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    action_ms: Option<u64>,

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
    ledger: RwLock<DeletionLedger>,
    suppressed: Arc<RwLock<HashSet<PathBuf>>>,
    suppressed_deletes: Arc<RwLock<HashSet<PathBuf>>>,
    in_progress: RwLock<HashMap<PathBuf, LargeFileAssembly>>,
    exclusions: Arc<Exclusions>,
    trash_manager: Arc<TrashManager>,
    full_scan_interval_secs: u64,
    change_sequences: RwLock<HashMap<PathBuf, FileChangeHistory>>,
    last_sequence_number: RwLock<u64>,
    journal: Arc<ActionJournal>,
    ledger_dirty: AtomicBool,
    live_capture: Mutex<bool>,
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
    crate::manifest::hash_file_tiered(path)
        .ok()
        .map(|(_, hash)| hash)
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
        let ledger = DeletionLedger::load(&root);
        let journal = Arc::new(ActionJournal::open(&root));
        Self {
            manifest: RwLock::new(Manifest {
                files: HashMap::new(),
                node_id: node_id.clone(),
            }),
            ledger: RwLock::new(ledger),
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
            journal,
            ledger_dirty: AtomicBool::new(false),
            live_capture: Mutex::new(false),
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
            ledger: RwLock::new(self.ledger.read().clone()),
            suppressed: self.suppressed.clone(),
            suppressed_deletes: self.suppressed_deletes.clone(),
            in_progress: RwLock::new(HashMap::new()),
            exclusions: self.exclusions.clone(),
            trash_manager: self.trash_manager.clone(),
            full_scan_interval_secs: self.full_scan_interval_secs,
            change_sequences: RwLock::new(HashMap::new()),
            last_sequence_number: RwLock::new(0),
            journal: self.journal.clone(),
            ledger_dirty: AtomicBool::new(false),
            live_capture: Mutex::new(false),
        }
    }

    pub fn trash_manager(&self) -> &Arc<TrashManager> {
        &self.trash_manager
    }

    pub fn list_trash(&self) -> Vec<bytehive_core::TrashEntry> {
        self.trash_manager.list_trash()
    }

    pub fn restore_trash_entry(&self, id: &str) -> Result<(), String> {
        // Capture the original path first so a successful restore can lift
        // the tombstones covering that prefix (otherwise the next send
        // would veto the restored file as a resurrection).
        let original: Option<PathBuf> = self
            .trash_manager
            .list_trash()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.original_path.clone());
        let result = self.trash_manager.restore_entry(id);
        if result.is_ok() {
            if let Some(prefix) = original {
                let to_clear: Vec<PathBuf> = self
                    .ledger
                    .read()
                    .iter()
                    .filter(|(k, _)| *k == &prefix || k.strip_prefix(&prefix).is_ok())
                    .map(|(k, _)| k.clone())
                    .collect();
                self.clear_tombstones(&to_clear);
            }
        }
        result
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
        self.scan_with_dirty(&HashSet::new())
    }

    /// Full scan with a dirty-path set that bypasses the manifest hash cache
    /// (e.g. watcher-pending paths). Drains nothing; defaults to empty via
    /// [`SyncEngine::scan`].
    pub fn scan_with_dirty(&self, dirty: &HashSet<PathBuf>) -> std::io::Result<Manifest> {
        let total = std::time::Instant::now();
        let cache = ManifestCache::load(&self.root);
        let (m, mut updated, stats) = manifest::build_manifest(
            &self.root,
            &self.node_id,
            &self.exclusions,
            Some(&cache),
            dirty,
        )?;
        if let Err(e) = updated.save_atomic(&self.root) {
            log::warn!("filesync: manifest cache save failed: {e}");
        }
        log::info!(
            "filesync: scan complete — {} file(s), {} dir(s), {} byte(s) in {}ms \
             (walk {}ms, hash {}ms cpu, {:.1} MB/s, cache {} hit/{} miss)",
            stats.files,
            stats.dirs,
            stats.bytes,
            total.elapsed().as_millis(),
            stats.walk_ms,
            stats.hash_ms,
            stats.hash_mb_s,
            stats.cache_hits,
            stats.cache_misses,
        );
        *self.manifest.write() = m.clone();
        // Cheap upkeep (persisted only when entries actually expire).
        self.prune_ledger();
        Ok(m)
    }

    pub fn get_manifest(&self) -> Manifest {
        self.manifest.read().clone()
    }

    // ---- Deletion ledger (tombstones + live-action records) ----

    /// Mutate the deletion ledger. Marks it dirty when `f` reports a change;
    /// persist with [`SyncEngine::flush_ledger`] at the end of a batch.
    fn ledger_mut(&self, f: impl FnOnce(&mut DeletionLedger) -> bool) {
        if f(&mut self.ledger.write()) {
            self.ledger_dirty.store(true, Ordering::SeqCst);
        }
    }

    /// Persist the deletion ledger when it changed since the last save.
    pub fn flush_ledger(&self) {
        if !self.ledger_dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Err(e) = self.ledger.read().save_atomic(&self.root) {
            self.ledger_dirty.store(true, Ordering::SeqCst);
            log::warn!("ledger: save failed: {e}");
        }
    }

    /// Copy of the current deletion ledger (tombstones and live records).
    pub fn ledger_snapshot(&self) -> DeletionLedger {
        self.ledger.read().clone()
    }

    /// Action journal of this sync root (client-side captured user actions).
    pub fn journal(&self) -> &Arc<ActionJournal> {
        &self.journal
    }

    /// Stamp for an outgoing file: the newest of the local live-action record
    /// and the mtime, or `None` when no live action is recorded (catch-up
    /// semantics, judged by mtime on the receiver).
    pub fn wire_action_stamp(&self, rel: &Path, modified_ms: u64) -> Option<u64> {
        let alive = self.ledger.read().alive_since(rel);
        if alive > 0 {
            Some(alive.max(modified_ms))
        } else {
            None
        }
    }

    /// Start capturing this root's user actions into the journal. Idempotent
    /// per engine: the watcher lives for the engine's lifetime, so actions are
    /// journalled while no peer is connected.
    pub fn start_live_capture(self: &Arc<Self>) -> io::Result<()> {
        let mut started = self.live_capture.lock();
        if *started {
            return Ok(());
        }
        let (tx, rx) = bounded::<FsEvent>(4096);
        crate::watcher::start_watcher(self.root.clone(), tx)?;
        let engine = Arc::clone(self);
        thread::Builder::new()
            .name("action-capture".into())
            .spawn(move || {
                for event in rx {
                    engine.capture_fs_event(event);
                }
            })?;
        *started = true;
        Ok(())
    }

    /// Journal one watcher event as a user action, stamped with receipt time.
    pub fn capture_fs_event(&self, event: FsEvent) {
        let now = now_ms();
        let result = match event {
            FsEvent::Changed(p) | FsEvent::WriteComplete(p) => {
                if self.is_suppressed(&p) || self.is_excluded(&p) {
                    return;
                }
                self.journal.upsert(p, now)
            }
            FsEvent::Deleted(p) => {
                if self.is_delete_suppressed(&p) || self.is_excluded(&p) {
                    return;
                }
                self.journal.delete(p, now)
            }
            FsEvent::Renamed(from, to) => {
                if self.is_suppressed(&from)
                    || self.is_suppressed(&to)
                    || self.is_excluded(&from)
                    || self.is_excluded(&to)
                {
                    return;
                }
                self.journal.rename(from, to, now)
            }
        };
        if let Err(e) = result {
            log::error!("action journal: append failed: {e}");
        }
    }

    /// Record server-side live user actions (`(path, action_ms)`) so relays of
    /// those paths carry a live stamp instead of the possibly old mtime.
    pub fn record_live_actions(&self, actions: &[(PathBuf, u64)]) {
        if actions.is_empty() {
            return;
        }
        self.ledger_mut(|l| {
            let mut changed = false;
            for (path, at) in actions {
                changed |= l.record_alive(path, *at);
            }
            changed
        });
        self.flush_ledger();
    }

    /// Record a deletion tombstone and persist the ledger (best effort:
    /// save errors are logged, not propagated).
    pub fn record_delete(
        &self,
        path: PathBuf,
        deleted_at_ms: u64,
        deleter: &str,
        prev_hash: Option<String>,
        prev_mtime_ms: Option<u64>,
    ) {
        self.ledger_mut(|l| {
            l.record(
                path,
                deleted_at_ms,
                deleter.to_string(),
                prev_hash,
                prev_mtime_ms,
            );
            true
        });
        self.flush_ledger();
    }

    /// Merge remote ledger entries and persist. Live records (`alive`) are
    /// applied before tombstones, so a stale tombstone is never kept over a
    /// newer live action. Returns the number of DTOs received.
    pub fn merge_ledger(&self, dtos: Vec<TombstoneDto>) -> usize {
        let n = dtos.len();
        if n == 0 {
            return 0;
        }
        let mut alive: Vec<(PathBuf, u64)> = Vec::new();
        let mut remote: HashMap<PathBuf, Tombstone> = HashMap::new();
        for d in dtos {
            if d.alive {
                alive.push((d.path, d.deleted_at_ms));
            } else {
                remote.insert(
                    d.path.clone(),
                    Tombstone::new(
                        d.path,
                        d.deleted_at_ms,
                        d.deleter_node,
                        d.prev_hash,
                        d.prev_mtime_ms,
                    ),
                );
            }
        }
        self.ledger_mut(|l| {
            for (path, at) in &alive {
                l.record_alive(path, *at);
            }
            l.merge_remote(remote);
            true
        });
        self.flush_ledger();
        n
    }

    /// Snapshot the ledger as DTOs for a LedgerExchange: tombstones, then
    /// live-action records (`alive: true`, `deleted_at_ms` = action time).
    pub fn ledger_entries(&self) -> Vec<TombstoneDto> {
        let l = self.ledger.read();
        let mut out: Vec<TombstoneDto> = l
            .iter()
            .map(|(_, t)| TombstoneDto {
                path: t.path.clone(),
                deleted_at_ms: t.deleted_at_ms,
                deleter_node: t.deleter_node.clone(),
                prev_hash: t.prev_hash.clone(),
                prev_mtime_ms: t.prev_mtime_ms,
                alive: false,
            })
            .collect();
        out.extend(l.alive_iter().map(|(path, at)| TombstoneDto {
            path: path.clone(),
            deleted_at_ms: *at,
            deleter_node: String::new(),
            prev_hash: None,
            prev_mtime_ms: None,
            alive: true,
        }));
        out
    }

    /// Returns true if a tombstone for `path` exists and is strictly newer
    /// than `mtime_ms`.
    pub fn has_tombstone_newer_than(&self, path: &Path, mtime_ms: u64) -> bool {
        self.ledger.read().has_newer_than(path, mtime_ms)
    }

    /// Covering-tombstone veto for a catch-up write judged by `incoming_mtime_ms`.
    /// Returns the tombstoned path when it vetoes `path`.
    pub fn covering_tombstone_veto(
        &self,
        path: &Path,
        incoming_mtime_ms: u64,
    ) -> Option<PathBuf> {
        self.veto_for(path, None, incoming_mtime_ms)
    }

    /// Admission check for an incoming write of `rel`. The write is vetoed
    /// when a covering tombstone is strictly newer than its comparand: the
    /// live action stamp, or the mtime for catch-up (`action_ms == None`).
    /// Equal stamps are accepted. Returns the tombstoned path on veto.
    fn veto_for(&self, rel: &Path, action_ms: Option<u64>, modified_ms: u64) -> Option<PathBuf> {
        let cmp = action_ms.unwrap_or(modified_ms);
        let ledger = self.ledger.read();
        ledger
            .covering_tombstone(rel)
            .filter(|t| t.deleted_at_ms > cmp)
            .map(|t| t.path.clone())
    }

    /// Record an admitted write. A live action raises the live-action record
    /// for `rel`, lifting any tombstone it supersedes. A catch-up write lifts
    /// an exact tombstone strictly older than its mtime. Persist via
    /// [`SyncEngine::flush_ledger`].
    fn admit_record(&self, rel: &Path, action_ms: Option<u64>, modified_ms: u64) {
        match action_ms {
            Some(at) => {
                if self.ledger.read().alive_record_needed(rel, at) {
                    self.ledger_mut(|l| l.record_alive(rel, at));
                }
            }
            None => {
                let lift = matches!(
                    self.ledger.read().get(rel),
                    Some(t) if modified_ms > t.deleted_at_ms
                );
                if lift {
                    self.ledger_mut(|l| l.record_alive(rel, modified_ms));
                }
            }
        }
    }

    /// Drop tombstones and live records older than the 90-day TTL. Persists
    /// only when at least one entry was pruned. Returns the number removed.
    pub fn prune_ledger(&self) -> usize {
        let pruned = self.ledger.write().prune(now_ms(), DELETION_LEDGER_TTL_MS);
        if pruned > 0 {
            self.ledger_dirty.store(true, Ordering::SeqCst);
            self.flush_ledger();
            log::info!("ledger: pruned {pruned} expired entr(ies)");
        }
        pruned
    }

    /// Clear tombstones for legitimately recreated paths (recreate win)
    /// and persist once. Used with [`crate::manifest::filter_resurrected`].
    pub fn clear_tombstones(&self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        self.ledger_mut(|l| {
            let mut removed_any = false;
            for p in paths {
                if l.remove(p).is_some() {
                    removed_any = true;
                }
            }
            removed_any
        });
        self.flush_ledger();
    }

    /// Covering tombstone that beats a live action stamped `action_ms`
    /// (strictly newer), or `None` when the action may be admitted.
    pub fn tombstone_beating(&self, path: &Path, action_ms: u64) -> Option<Tombstone> {
        self.ledger
            .read()
            .covering_tombstone(path)
            .filter(|t| t.deleted_at_ms > action_ms)
            .cloned()
    }

    /// Stage a tombstone for a user delete captured in the journal. The
    /// manifest supplies the previous content info. Persist with
    /// [`SyncEngine::flush_ledger`].
    pub fn stage_delete(&self, path: PathBuf, deleted_at_ms: u64) {
        let (prev_hash, prev_mtime_ms) = self
            .manifest
            .read()
            .files
            .get(&path)
            .map(|m| (Some(crate::hex(&m.hash)), Some(m.modified_ms)))
            .unwrap_or((None, None));
        let node = self.node_id.clone();
        self.ledger_mut(|l| {
            l.record(path, deleted_at_ms, node, prev_hash, prev_mtime_ms);
            true
        });
    }

    /// Stage a live-action record for a journalled create/modify/rename
    /// target. Persist with [`SyncEngine::flush_ledger`].
    pub fn stage_live_action(&self, path: &Path, action_ms: u64) {
        if self.ledger.read().alive_record_needed(path, action_ms) {
            self.ledger_mut(|l| l.record_alive(path, action_ms));
        }
    }

    pub fn send_paths(&self, paths: &[PathBuf], conn: &Arc<Connection>) -> std::io::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }

        let chunk_size = (paths.len() + READ_THREADS - 1) / READ_THREADS;
        let (msg_tx, msg_rx) = bounded::<Message>(PIPELINE_DEPTH);

        thread::scope(|s| -> std::io::Result<()> {
            for slice in paths.chunks(chunk_size) {
                let tx = msg_tx.clone();
                s.spawn(move || {
                    let stamp = |rel: &Path, mtime: u64| self.wire_action_stamp(rel, mtime);
                    bundler::stream_messages(&self.root, slice, &tx, None, &stamp);
                });
            }
            drop(msg_tx);

            for msg in msg_rx {
                conn.send(&msg)?;
            }
            Ok(())
        })
    }

    pub fn create_bundles(&self, paths: &[PathBuf]) -> Vec<FileBundle> {
        let (tx, rx) = bounded::<Message>(PIPELINE_DEPTH);
        let root = self.root.clone();
        let paths = paths.to_vec();
        let engine = Arc::new(self.clone_with_fresh_state());
        thread::spawn(move || {
            let stamp_engine = engine.clone();
            let stamp = move |rel: &Path, mtime: u64| stamp_engine.wire_action_stamp(rel, mtime);
            bundler::stream_messages(&root, &paths, &tx, Some(engine), &stamp)
        });
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

            // Resurrection guard: a covering tombstone strictly newer than
            // the comparand (live action stamp, or mtime for catch-up) vetoes
            // the write. Admitted writes update the live-action record.
            if let Some(tomb_path) =
                self.veto_for(&fd.metadata.rel_path, fd.action_ms, fd.metadata.modified_ms)
            {
                log::warn!(
                    "apply_bundle: vetoing {:?} (covering tombstone {:?} beats {} {})",
                    fd.metadata.rel_path,
                    tomb_path,
                    if fd.action_ms.is_some() { "live action" } else { "incoming mtime" },
                    fd.action_ms.unwrap_or(fd.metadata.modified_ms),
                );
                continue;
            }
            self.admit_record(
                &fd.metadata.rel_path,
                fd.action_ms,
                fd.metadata.modified_ms,
            );

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

        self.flush_ledger();

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

    /// Catch-up form of [`SyncEngine::begin_large_file_with_action`].
    pub fn begin_large_file(
        &self,
        metadata: FileMetadata,
        total_chunks: u32,
    ) -> std::io::Result<()> {
        self.begin_large_file_with_action(metadata, total_chunks, None)
    }

    pub fn begin_large_file_with_action(
        &self,
        metadata: FileMetadata,
        total_chunks: u32,
        action_ms: Option<u64>,
    ) -> std::io::Result<()> {
        if !safe_relative(&metadata.rel_path) {
            warn!("rejected unsafe large-file path: {:?}", metadata.rel_path);
            return Ok(());
        }

        // Resurrection guard (same policy as apply_bundle): veto the stream
        // setup when a covering tombstone beats the comparand.
        if let Some(tomb_path) =
            self.veto_for(&metadata.rel_path, action_ms, metadata.modified_ms)
        {
            log::warn!(
                "begin_large_file: vetoing {:?} (covering tombstone {:?} beats {} {})",
                metadata.rel_path,
                tomb_path,
                if action_ms.is_some() { "live action" } else { "incoming mtime" },
                action_ms.unwrap_or(metadata.modified_ms),
            );
            return Ok(());
        }
        self.admit_record(&metadata.rel_path, action_ms, metadata.modified_ms);
        self.flush_ledger();

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
                action_ms,
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

        // Re-check the resurrection guard at commit time: a tombstone may
        // have arrived between LargeFileStart and commit. Veto the write
        // (clean up tmp) instead of recreating a deleted path.
        if let Some(tomb_path) = self.veto_for(path, asm.action_ms, asm.modified_ms) {
            log::warn!(
                "commit_large_file: vetoing {path:?} (covering tombstone {tomb_path:?} beats {} {})",
                if asm.action_ms.is_some() { "live action" } else { "incoming mtime" },
                asm.action_ms.unwrap_or(asm.modified_ms),
            );
            // Owns `asm.tmp_path` exclusively (per-path staging name).
            let _ = fs::remove_file(&asm.tmp_path);
            return Ok(FinishResult::Committed);
        }
        self.admit_record(path, asm.action_ms, asm.modified_ms);
        self.flush_ledger();

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
                // Only this transfer's own staging file is removed; siblings
                // (e.g. pack-X.idx beside pack-X.pack) are untouched.
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

        // Moves only this transfer's staging file into place. The staging
        // directory is shared by all transfers and is intentionally never
        // removed here: removing it could race another transfer's
        // create_dir_all in begin_large_file and make its open() fail.
        fs::rename(&asm.tmp_path, &asm.dst)?;

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
        }
    }

    /// Catch-up/legacy form of [`SyncEngine::apply_rename_at`] stamped now.
    pub fn apply_rename(&self, from: &PathBuf, to: &PathBuf) -> std::io::Result<()> {
        let node = self.node_id.clone();
        self.apply_rename_at(from, to, now_ms(), &node)
    }

    /// Apply a live rename stamped `action_ms`. The destination is admitted
    /// against tombstones; when vetoed, the moved content is stale and the
    /// source is deleted at `action_ms` instead (as the rename's effect).
    pub fn apply_rename_at(
        &self,
        from: &PathBuf,
        to: &PathBuf,
        action_ms: u64,
        deleter: &str,
    ) -> std::io::Result<()> {
        if !safe_relative(from) || !safe_relative(to) {
            warn!("apply_rename: rejected unsafe path(s): {from:?} → {to:?}");
            return Ok(());
        }

        if let Some(tomb_path) = self.veto_for(to, Some(action_ms), 0) {
            log::warn!(
                "apply_rename: {to:?} vetoed by covering tombstone {tomb_path:?}; deleting source {from:?}"
            );
            self.apply_deletes(&[from.clone()], action_ms, deleter)?;
            return Ok(());
        }
        self.admit_record(to, Some(action_ms), 0);
        self.flush_ledger();

        let src = self.root.join(from);
        let dst = self.root.join(to);

        if !src.exists() {
            // Idempotent retransmit: source gone. If manifest still holds
            // stale `from`-prefixed keys but `to` already exists on disk,
            // converge the manifest without touching the filesystem.
            if dst.exists() {
                let pairs: Vec<(PathBuf, PathBuf)> = {
                    let m = self.manifest.read();
                    let mut v = Vec::new();
                    for k in m.files.keys() {
                        if k == from {
                            v.push((k.clone(), to.clone()));
                        } else if let Ok(stripped) = k.strip_prefix(from) {
                            v.push((k.clone(), to.join(stripped)));
                        }
                    }
                    v
                };
                if !pairs.is_empty() {
                    let mut suppressed = Vec::with_capacity(pairs.len() * 2);
                    {
                        let mut m = self.manifest.write();
                        for (old, new) in pairs {
                            if let Some(mut meta) = m.files.remove(&old) {
                                meta.rel_path = new.clone();
                                if meta.is_dir {
                                    if let Ok(fs_meta) =
                                        fs::metadata(self.root.join(&new))
                                    {
                                        if let Ok(modified) = fs_meta.modified() {
                                            if let Ok(dur) = modified.duration_since(
                                                std::time::SystemTime::UNIX_EPOCH,
                                            ) {
                                                meta.modified_ms =
                                                    dur.as_millis() as u64;
                                            }
                                        }
                                    }
                                }
                                m.files.insert(new.clone(), meta);
                            }
                            suppressed.push(old);
                            suppressed.push(new);
                        }
                    }
                    {
                        let mut sup = self.suppressed.write();
                        for p in &suppressed {
                            sup.insert(p.clone());
                        }
                    }
                    self.schedule_unsuppress(suppressed);
                }
            } else {
                log::debug!("apply_rename: source absent, skipping: {from:?}");
            }
            return Ok(());
        }

        // Collect every manifest key under `from` (component-wise via
        // strip_prefix, so `a/b` does not match `a/bc`) and remap it under
        // `to`. Includes the top entry itself.
        let pairs: Vec<(PathBuf, PathBuf)> = {
            let m = self.manifest.read();
            let mut v = Vec::new();
            for k in m.files.keys() {
                if k == from {
                    v.push((k.clone(), to.clone()));
                } else if let Ok(stripped) = k.strip_prefix(from) {
                    v.push((k.clone(), to.join(stripped)));
                }
            }
            v
        };

        {
            let mut sup = self.suppressed.write();
            sup.insert(from.clone());
            sup.insert(to.clone());
            for (old, new) in &pairs {
                sup.insert(old.clone());
                sup.insert(new.clone());
            }
        }

        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }

        // Idempotent when `dst` already exists (retransmit/race): files
        // overwrite via rename; a failing dir-onto-dir rename still
        // converges via the manifest remap below instead of erroring.
        let rename_result = fs::rename(&src, &dst);
        if let Err(e) = rename_result {
            if dst.exists() {
                log::warn!("apply_rename: {from:?} → {to:?} rename failed but dst exists, converging manifest: {e}");
            } else {
                return Err(e);
            }
        }

        {
            let mut suppressed = Vec::with_capacity(pairs.len() * 2 + 2);
            suppressed.push(from.clone());
            suppressed.push(to.clone());
            {
                let mut m = self.manifest.write();
                for (old, new) in pairs {
                    if let Some(mut meta) = m.files.remove(&old) {
                        meta.rel_path = new.clone();
                        // Refresh dir mtime from disk after the move; files
                        // keep the incoming manifest mtime via watcher/scan.
                        if meta.is_dir {
                            if let Ok(fs_meta) = fs::metadata(self.root.join(&new)) {
                                if let Ok(modified) = fs_meta.modified() {
                                    if let Ok(dur) = modified.duration_since(
                                        std::time::SystemTime::UNIX_EPOCH,
                                    ) {
                                        meta.modified_ms = dur.as_millis() as u64;
                                    }
                                }
                            }
                        } else if let Ok(fs_meta) = fs::metadata(self.root.join(&new)) {
                            if let Ok(modified) = fs_meta.modified() {
                                if let Ok(dur) = modified.duration_since(
                                    std::time::SystemTime::UNIX_EPOCH,
                                ) {
                                    meta.modified_ms = dur.as_millis() as u64;
                                }
                            }
                        }
                        m.files.insert(new.clone(), meta);
                    }
                    suppressed.push(old);
                    suppressed.push(new);
                }
            }
            // Deduplicate before scheduling (pairs already include from/to).
            suppressed.sort();
            suppressed.dedup();

            log::info!("apply_rename: {from:?} → {to:?}");
            self.schedule_unsuppress(suppressed);
        }
        Ok(())
    }

    pub fn apply_deletes(
        &self,
        paths: &[PathBuf],
        deleted_at_ms: u64,
        deleter: &str,
    ) -> std::io::Result<usize> {
        let mut removed = Vec::new();
        let mut count = 0usize;
        // Stage tombstone data in memory; persist once at the end.
        let mut tombstones: Vec<(PathBuf, Option<String>, Option<u64>)> = Vec::new();

        for rel in paths {
            if !safe_relative(rel) {
                continue;
            }

            // Collect the top path plus every manifest child under it
            // (component-wise: `a/b` must not match `a/bc`) BEFORE removal,
            // so delete-suppression covers the whole subtree recursively.
            let subtree: Vec<PathBuf> = {
                let m = self.manifest.read();
                let mut v = vec![rel.clone()];
                for k in m.files.keys() {
                    if k != rel {
                        if let Ok(stripped) = k.strip_prefix(rel) {
                            let _ = stripped;
                            v.push(k.clone());
                        }
                    }
                }
                v
            };

            // Live-action guard: a path whose live action is at least as new
            // as this delete was recreated after the delete was issued. It is
            // kept, and the delete only removes what it is newer than.
            let protected: HashSet<PathBuf> = {
                let ledger = self.ledger.read();
                subtree
                    .iter()
                    .filter(|p| {
                        let at = ledger.alive_since(p);
                        at > 0 && at >= deleted_at_ms
                    })
                    .cloned()
                    .collect()
            };

            // Snapshot manifest info BEFORE removal for the tombstone.
            let (prev_hash, prev_mtime_ms) = self
                .manifest
                .read()
                .files
                .get(rel)
                .map(|m| (Some(crate::hex(&m.hash)), Some(m.modified_ms)))
                .unwrap_or((None, None));

            let full = self.root.join(rel);
            if protected.is_empty() {
                {
                    let mut sup = self.suppressed_deletes.write();
                    for p in &subtree {
                        sup.insert(p.clone());
                        removed.push(p.clone());
                    }
                }
                if full.is_dir() {
                    self.trash_manager.move_to_trash(&full, rel, &self.node_id);
                    let mut m = self.manifest.write();
                    for child in &subtree {
                        m.files.remove(child);
                    }
                } else {
                    self.trash_manager.move_to_trash(&full, rel, &self.node_id);
                    self.manifest.write().files.remove(rel);
                }
            } else {
                log::info!(
                    "apply_deletes: keeping {} live-recreated path(s) under {:?} (action newer than delete @{})",
                    protected.len(),
                    rel,
                    deleted_at_ms
                );
                let mut targets: Vec<PathBuf> = subtree
                    .iter()
                    .filter(|p| !protected.contains(*p))
                    .cloned()
                    .collect();
                targets.sort_by_key(|p| p.components().count());
                let mut gone: Vec<PathBuf> = Vec::new();
                for target in targets {
                    if gone.iter().any(|g| target.starts_with(g)) {
                        continue;
                    }
                    let target_full = self.root.join(&target);
                    if target_full.is_dir()
                        && protected.iter().any(|q| q != &target && q.starts_with(&target))
                    {
                        continue;
                    }
                    self.suppressed_deletes.write().insert(target.clone());
                    removed.push(target.clone());
                    self.trash_manager
                        .move_to_trash(&target_full, &target, &self.node_id);
                    self.manifest
                        .write()
                        .files
                        .retain(|k, _| !k.starts_with(&target));
                    gone.push(target);
                }
            }

            // Record ALWAYS, even when the file was absent on disk and no
            // manifest entry existed (fixes the silent-absent gap: the peer
            // may still hold the file and would otherwise resurrect it).
            // `record` ignores the tombstone when a newer live action exists.
            tombstones.push((rel.clone(), prev_hash, prev_mtime_ms));
            count += 1;
        }

        if !tombstones.is_empty() {
            self.ledger_mut(|l| {
                for (path, prev_hash, prev_mtime_ms) in tombstones {
                    l.record(
                        path,
                        deleted_at_ms,
                        deleter.to_string(),
                        prev_hash,
                        prev_mtime_ms,
                    );
                }
                true
            });
            self.flush_ledger();
        }

        removed.sort();
        removed.dedup();
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
