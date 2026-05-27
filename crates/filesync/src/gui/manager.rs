use crate::client::Client;
use crate::exclusions::{ExclusionConfig, Exclusions};
use crate::gui::config::GuiConfig;
use crate::gui::state::{ConnectionStatus, SharedState};
use crate::suspend_detector::SuspendDetector;
use crate::sync_engine::SyncEngine;
use crate::timestamp_id;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub struct SyncManager {
    pub state: SharedState,
    paused: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl SyncManager {
    pub fn new(state: SharedState) -> Self {
        Self {
            state,
            paused: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn start(&self, cfg: GuiConfig) {
        let state = self.state.clone();
        let paused = self.paused.clone();
        let stopped = self.stopped.clone();

        thread::Builder::new()
            .name("sync-manager".into())
            .spawn(move || {
                session_loop(cfg, state, paused, stopped);
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
}

fn session_loop(
    cfg: GuiConfig,
    state: SharedState,
    paused: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
) {
    let exclusions = Arc::new(Exclusions::compile(&ExclusionConfig {
        exclude_patterns: cfg.exclude_patterns.clone(),
        exclude_regex: cfg.exclude_regex.clone(),
    }));

    let node_id = format!("gui-{:x}", timestamp_id());
    let engine = Arc::new(SyncEngine::new(cfg.sync_root.clone(), node_id, exclusions));

    match engine.scan() {
        Ok(_) => {
            refresh_manifest_stats(&engine, &state);
            state.write().log_event("Local folder scanned.");
        }
        Err(e) => {
            state.write().log_event(format!("Initial scan failed: {e}"));
        }
    }

    let mut suspend_detector = SuspendDetector::new();

    loop {
        if stopped.load(Ordering::SeqCst) {
            break;
        }

        if paused.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(250));
            let _ = suspend_detector.check_for_resume();
            continue;
        }

        {
            let mut s = state.write();
            s.status = ConnectionStatus::Connecting;
            s.log_event(format!("Connecting to {} …", cfg.server_addr));
        }

        let identity_dir: PathBuf = GuiConfig::config_dir().join("filesync");

        let client = Client::new_standalone(
            engine.clone(),
            cfg.server_addr.clone(),
            identity_dir,
            Some(state.clone()),
        );

        match client.session() {
            Ok(()) => {
                let mut s = state.write();
                s.status = ConnectionStatus::Disconnected;
                s.log_event("Session ended cleanly.");
            }
            Err(e) => {
                let msg = e.to_string();
                let mut s = state.write();
                s.status = ConnectionStatus::Error(msg.clone());
                s.log_event(format!("Connection error: {msg}"));
            }
        }

        refresh_manifest_stats(&engine, &state);

        if stopped.load(Ordering::SeqCst) {
            break;
        }
        if paused.load(Ordering::SeqCst) {
            continue;
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if stopped.load(Ordering::SeqCst) || paused.load(Ordering::SeqCst) {
                break;
            }

            if suspend_detector.check_for_resume() {
                state
                    .write()
                    .log_event("System resumed from suspend — reconnecting immediately.");
                break;
            }

            thread::sleep(Duration::from_millis(1000));
        }
    }

    state.write().status = ConnectionStatus::Disconnected;
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
