use crate::client::Client;
use crate::exclusions::{ExclusionConfig, Exclusions};
use crate::gui::config::GuiConfig;
use crate::gui::state::{ConnectionStatus, SharedState};
use crate::suspend_detector::SuspendDetector;
use crate::sync_engine::SyncEngine;
use crate::timestamp_id;
use parking_lot::Mutex;
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

    // Show local stats immediately so the Stats panel isn't stuck at zero
    // while waiting for a connection.
    match engine.scan() {
        Ok(_) => {
            refresh_manifest_stats(&engine, &state);
            state.write().log_event("Local folder scanned.");
        }
        Err(e) => {
            state.write().log_event(format!("Initial scan failed: {e}"));
        }
    }

    // Tracks whichever `Client` is currently connecting/connected, so the
    // suspend-monitor thread below can force it to disconnect. `None` while
    // we're idling between sessions (paused, or waiting to reconnect).
    let active_client: Arc<Mutex<Option<Arc<Client>>>> = Arc::new(Mutex::new(None));
    // Set by the suspend-monitor thread whenever it detects a resume, so the
    // reconnect-backoff loop below can skip its wait and retry immediately.
    let resume_pending = Arc::new(AtomicBool::new(false));

    // Runs for the lifetime of this sync session (independent of connect/
    // reconnect cycles) so that suspend/resume is detected promptly even
    // while a session is actively connected — not just between attempts.
    // On resume it force-closes whatever connection is active, which makes
    // the main loop below reconnect and rescan exactly like the startup
    // sequence.
    {
        let state = state.clone();
        let stopped = stopped.clone();
        let active_client = active_client.clone();
        let resume_pending = resume_pending.clone();
        thread::Builder::new()
            .name("suspend-monitor".into())
            .spawn(move || {
                let mut suspend_detector = SuspendDetector::new();
                loop {
                    if stopped.load(Ordering::SeqCst) {
                        break;
                    }

                    if suspend_detector.check_for_resume() {
                        state
                            .write()
                            .log_event("System resumed from suspend — reconnecting.");
                        resume_pending.store(true, Ordering::SeqCst);
                        if let Some(client) = active_client.lock().clone() {
                            client.shutdown();
                        }
                    }

                    thread::sleep(Duration::from_millis(1000));
                }
            })
            .expect("spawn suspend-monitor");
    }

    loop {
        if stopped.load(Ordering::SeqCst) {
            break;
        }

        if paused.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(250));
            continue;
        }

        resume_pending.store(false, Ordering::SeqCst);

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

        *active_client.lock() = Some(client.clone());

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

        *active_client.lock() = None;

        refresh_manifest_stats(&engine, &state);

        if stopped.load(Ordering::SeqCst) {
            break;
        }
        if paused.load(Ordering::SeqCst) {
            continue;
        }

        if resume_pending.swap(false, Ordering::SeqCst) {
            // The suspend-monitor already forced this disconnect; reconnect
            // right away instead of waiting out the usual backoff.
            continue;
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if stopped.load(Ordering::SeqCst) || paused.load(Ordering::SeqCst) {
                break;
            }

            if resume_pending.swap(false, Ordering::SeqCst) {
                state
                    .write()
                    .log_event("System resumed from suspend — reconnecting immediately.");
                break;
            }

            thread::sleep(Duration::from_millis(200));
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
