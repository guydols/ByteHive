use crate::client::Client;
use crate::exclusions::{ExclusionConfig, Exclusions};
use crate::gui::config::GuiConfig;
use crate::gui::state::{ConnectionStatus, SharedState, TrashItem};
use crate::sync_engine::SyncEngine;
use crate::timestamp_id;
use crate::transport::is_heartbeat_timeout;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub struct SyncManager {
    pub state: SharedState,
    paused: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    engine: Arc<parking_lot::RwLock<Option<Arc<SyncEngine>>>>,
}

impl SyncManager {
    pub fn new(state: SharedState) -> Self {
        Self {
            state,
            paused: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
            engine: Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    pub fn start(&self, cfg: GuiConfig) {
        let state = self.state.clone();
        let paused = self.paused.clone();
        let stopped = self.stopped.clone();
        let engine_slot = self.engine.clone();

        thread::Builder::new()
            .name("sync-manager".into())
            .spawn(move || {
                session_loop(cfg, state, paused, stopped, engine_slot);
            })
            .expect("spawn sync-manager");
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.state.write().status = ConnectionStatus::Paused;
        self.state.write().log_event("Sync paused by user.");
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.state.write().log_event("Sync resumed.");
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Current engine handle, if the background session thread has created
    /// one yet. Follows the same pause/resume pattern — never blocks the
    /// session loop, just reads the shared slot.
    pub fn engine_handle(&self) -> Option<Arc<SyncEngine>> {
        self.engine.read().clone()
    }

    /// Poll the engine's trash list and publish it to the shared snapshot.
    /// Safe to call on every UI tick; no-ops until the engine exists.
    pub fn refresh_trash(&self) {
        let engine = match self.engine_handle() {
            Some(e) => e,
            None => return,
        };
        let items: Vec<TrashItem> = engine
            .list_trash()
            .iter()
            .map(trash_item_from)
            .collect();
        self.state.write().trash = items;
    }

    pub fn restore_trash_entry(
        &self,
        id: &str,
        dest: Option<&Path>,
    ) -> Result<PathBuf, String> {
        let engine = self
            .engine_handle()
            .ok_or_else(|| "Sync engine is not ready yet.".to_string())?;
        let dest_rel = engine.restore_trash_entry_with_dest(id, dest)?;
        {
            let mut s = self.state.write();
            s.log_event(format!("Trash entry restored to {dest_rel:?}"));
        }
        self.refresh_trash();
        Ok(dest_rel)
    }

    pub fn purge_trash_entry(&self, id: &str) -> Result<(), String> {
        let engine = self
            .engine_handle()
            .ok_or_else(|| "Sync engine is not ready yet.".to_string())?;
        engine.purge_trash_entry(id)?;
        {
            let mut s = self.state.write();
            s.log_event(format!("Trash entry {id} deleted forever."));
        }
        self.refresh_trash();
        Ok(())
    }

    pub fn empty_trash(&self) -> usize {
        let count = self.engine_handle().map(|e| e.empty_trash()).unwrap_or(0);
        if count > 0 {
            self.state
                .write()
                .log_event(format!("Trash emptied ({count} item(s))."));
        }
        self.refresh_trash();
        count
    }

    pub fn purge_expired_trash(&self) -> usize {
        let count = self
            .engine_handle()
            .map(|e| e.purge_expired_trash())
            .unwrap_or(0);
        if count > 0 {
            self.state
                .write()
                .log_event(format!("Purged {count} expired trash item(s)."));
        }
        self.refresh_trash();
        count
    }
}

fn trash_item_from(e: &bytehive_core::TrashEntry) -> TrashItem {
    TrashItem {
        id: e.id.clone(),
        original_path: e.original_path.to_string_lossy().into_owned(),
        size: e.size,
        deleted_at_ms: e.deleted_at_ms,
        is_dir: e.is_dir,
    }
}

fn session_loop(
    cfg: GuiConfig,
    state: SharedState,
    paused: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    engine_slot: Arc<parking_lot::RwLock<Option<Arc<SyncEngine>>>>,
) {
    let exclusions = Arc::new(Exclusions::compile(&ExclusionConfig {
        exclude_patterns: cfg.exclude_patterns.clone(),
        exclude_regex: cfg.exclude_regex.clone(),
    }));

    let node_id = format!("gui-{:x}", timestamp_id());
    let engine = Arc::new(SyncEngine::new(cfg.sync_root.clone(), node_id, exclusions));
    *engine_slot.write() = Some(engine.clone());

    // Show local stats immediately so the Stats panel isn't stuck at zero
    // while waiting for a connection.
    match engine.scan() {
        Ok(_) => {
            refresh_manifest_stats(&engine, &state);
            publish_trash(&engine, &state);
            state.write().log_event("Local folder scanned.");
        }
        Err(e) => {
            state.write().log_event(format!("Initial scan failed: {e}"));
        }
    }

    loop {
        if stopped.load(Ordering::SeqCst) {
            break;
        }

        if paused.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(250));
            continue;
        }

        {
            let mut s = state.write();
            s.status = ConnectionStatus::Connecting;
            s.log_event(format!("Connecting to {} …", cfg.server_addr));
        }

        let identity_dir: PathBuf = GuiConfig::config_dir().join("filesync");

        let client = Arc::new(Client::new_standalone(
            engine.clone(),
            cfg.server_addr.clone(),
            identity_dir,
            Some(state.clone()),
        ));

        // Heartbeat fast-path: a heartbeat timeout means liveness is already
        // proven lost (dead peer or suspend/resume gap) — skip the 5 s wait
        // once and reconnect immediately.
        let mut skip_wait = false;
        match client.session() {
            Ok(()) => {
                let mut s = state.write();
                s.status = ConnectionStatus::Disconnected;
                s.log_event("Session ended cleanly.");
            }
            Err(e) => {
                let msg = e.to_string();
                if is_heartbeat_timeout(&e) {
                    state.write().log_event(
                        "Heartbeat timeout — reconnecting immediately.",
                    );
                    skip_wait = true;
                }
                let mut s = state.write();
                s.status = ConnectionStatus::Error(msg.clone());
                s.log_event(format!("Connection error: {msg}"));
            }
        }

        refresh_manifest_stats(&engine, &state);
        publish_trash(&engine, &state);

        if stopped.load(Ordering::SeqCst) {
            break;
        }
        if paused.load(Ordering::SeqCst) {
            continue;
        }

        if skip_wait {
            continue;
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if stopped.load(Ordering::SeqCst) || paused.load(Ordering::SeqCst) {
                break;
            }

            thread::sleep(Duration::from_millis(200));
        }
    }

    state.write().status = ConnectionStatus::Disconnected;
}

fn publish_trash(engine: &SyncEngine, state: &SharedState) {
    let items: Vec<TrashItem> = engine.list_trash().iter().map(trash_item_from).collect();
    state.write().trash = items;
}

pub fn refresh_manifest_stats(engine: &SyncEngine, state: &SharedState) {
    let manifest = engine.get_manifest();
    let mut file_count = 0usize;
    let mut dir_count = 0usize;
    let mut total_bytes: u64 = 0;
    for meta in manifest.files.values() {
        if meta.is_dir {
            dir_count += 1;
        } else {
            file_count += 1;
            total_bytes += meta.size;
        }
    }
    let mut s = state.write();
    s.file_count = file_count;
    s.dir_count = dir_count;
    s.total_bytes = total_bytes;
    s.last_connected = Some(Instant::now());
}
