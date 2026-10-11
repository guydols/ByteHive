use crate::action_journal::{JournalEntry, JournalOp};
use crate::bundler;
use crate::cert_fingerprint;
use crate::common::{self, LargeFileEndOutcome, PendingChanges};
use crate::gui::state::ConnectionStatus;
use crate::gui::state::{ConflictKind, SharedState};
use crate::known_hosts::KnownServers;
use crate::ledger::DeletionLedger;
use crate::manifest;
use crate::protocol::*;
use crate::sync_engine::{ConflictInfo, SyncEngine};
use crate::timestamp_id;
use crate::transport::{is_heartbeat_timeout, Connection, HEARTBEAT_POLL_SECS};

use bytehive_core::MessageBus;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use log::{debug, error, info, warn};
use parking_lot::Mutex;

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

pub use crate::common::count_manifest;

pub struct Client {
    engine: Arc<SyncEngine>,
    server_addr: String,
    bus: Option<Arc<MessageBus>>,
    stopped: Arc<AtomicBool>,
    identity_dir: PathBuf,
    tls_config: Arc<rustls::ClientConfig>,
    gui_state: Option<SharedState>,
    awaiting_approval: Arc<AtomicBool>,
    active_conn: Arc<Mutex<Option<Arc<Connection>>>>,
}

struct ActiveConnGuard<'a> {
    slot: &'a Mutex<Option<Arc<Connection>>>,
}

impl<'a> Drop for ActiveConnGuard<'a> {
    fn drop(&mut self) {
        self.slot.lock().take();
    }
}

impl Client {
    pub fn new(root: std::path::PathBuf, server_addr: String) -> Self {
        use crate::exclusions::{ExclusionConfig, Exclusions};
        let identity_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("bytehive")
            .join("filesync");
        let tls_config = crate::app::build_client_tls_config(&identity_dir).unwrap_or_else(|e| {
            log::warn!("filesync: TLS config failed ({e}), falling back to ephemeral cert");
            crate::app::build_ephemeral_client_tls_config()
        });
        let node_id = format!("cli-{:x}", timestamp_id());
        let exclusions = Arc::new(Exclusions::compile(&ExclusionConfig::default()));
        Self {
            engine: Arc::new(SyncEngine::new(root, node_id, exclusions)),
            server_addr,
            bus: None,
            stopped: Arc::new(AtomicBool::new(false)),
            identity_dir,
            tls_config,
            gui_state: None,
            awaiting_approval: Arc::new(AtomicBool::new(false)),
            active_conn: Arc::new(Mutex::new(None)),
        }
    }

    pub fn new_with_engine(
        engine: Arc<SyncEngine>,
        server_addr: String,
        bus: Arc<MessageBus>,
        identity_dir: PathBuf,
        tls_config: Arc<rustls::ClientConfig>,
    ) -> Self {
        debug!(
            "filesync client: new_with_engine (node={} server={} identity_dir={:?})",
            engine.node_id(),
            server_addr,
            identity_dir,
        );
        Self {
            engine,
            server_addr,
            bus: Some(bus),
            stopped: Arc::new(AtomicBool::new(false)),
            identity_dir,
            tls_config,
            gui_state: None,
            awaiting_approval: Arc::new(AtomicBool::new(false)),
            active_conn: Arc::new(Mutex::new(None)),
        }
    }

    pub fn new_standalone(
        engine: Arc<SyncEngine>,
        server_addr: String,
        identity_dir: PathBuf,
        gui_state: Option<SharedState>,
    ) -> Self {
        let tls_config = crate::app::build_client_tls_config(&identity_dir).unwrap_or_else(|e| {
            log::warn!("filesync: TLS config failed ({e}), falling back to ephemeral cert");
            crate::app::build_ephemeral_client_tls_config()
        });
        debug!(
            "filesync client: new_standalone (node={} server={} identity_dir={:?} gui={})",
            engine.node_id(),
            server_addr,
            identity_dir,
            gui_state.is_some()
        );
        Self {
            engine,
            server_addr,
            bus: None,
            stopped: Arc::new(AtomicBool::new(false)),
            identity_dir,
            tls_config,
            gui_state,
            awaiting_approval: Arc::new(AtomicBool::new(false)),
            active_conn: Arc::new(Mutex::new(None)),
        }
    }

    pub fn engine(&self) -> &Arc<SyncEngine> {
        &self.engine
    }

    pub fn shutdown(&self) {
        debug!("filesync client: shutdown requested");
        self.stopped.store(true, Ordering::SeqCst);
        self.force_disconnect();
    }

    /// Force-close the active connection without marking the client stopped,
    /// so the `run()` loop can reconnect immediately (used on resume).
    pub fn force_disconnect(&self) {
        if let Some(conn) = self.active_conn.lock().clone() {
            debug!("filesync client: forcing active connection closed");
            conn.shutdown();
        }
    }

    pub fn is_awaiting_approval(&self) -> bool {
        self.awaiting_approval.load(Ordering::SeqCst)
    }

    pub fn run(&self) {
        const APPROVAL_POLL_SECS: u64 = 30;
        let mut backoff = Duration::from_secs(1);

        loop {
            if self.stopped.load(Ordering::SeqCst) {
                debug!("filesync: run loop: stop flag set, exiting");
                break;
            }
            info!("filesync: connecting to {} …", self.server_addr);
            // Heartbeat fast-path: a heartbeat timeout means the peer is
            // (probably) gone or we just resumed from suspend — the wall-clock
            // gap already proved liveness is lost, so reconnect immediately
            // instead of waiting out the backoff.
            let mut skip_backoff = false;
            match self.session() {
                Ok(()) => {
                    info!("filesync: session ended cleanly");
                    self.awaiting_approval.store(false, Ordering::SeqCst);
                    backoff = Duration::from_secs(1);
                    debug!("filesync: backoff reset to 1 s after clean session");
                }
                Err(e) => {
                    if self.stopped.load(Ordering::SeqCst) {
                        debug!("filesync: session error during shutdown (expected): {e}");
                        break;
                    }
                    let msg = e.to_string();
                    if msg.contains("awaiting_approval") {
                        // Server told us we're pending; use long fixed interval.
                        info!(
                            "filesync: awaiting admin approval — will retry in {APPROVAL_POLL_SECS} s"
                        );
                        backoff = Duration::from_secs(APPROVAL_POLL_SECS);
                    } else if msg.contains("client_rejected") {
                        error!(
                            "filesync: this client has been rejected by the server administrator"
                        );
                        // Back off a long time; the admin needs to explicitly re-allow.
                        backoff = Duration::from_secs(300);
                    } else if is_heartbeat_timeout(&e) {
                        info!("filesync: heartbeat timeout — reconnecting immediately");
                        backoff = Duration::from_secs(1);
                        skip_backoff = true;
                    } else {
                        error!("filesync: session error: {e}");
                        debug!("filesync: session error kind={:?}", e.kind());
                    }
                }
            }
            if self.stopped.load(Ordering::SeqCst) {
                debug!("filesync: run loop: stop flag set after session, exiting");
                break;
            }
            if skip_backoff {
                continue;
            }
            info!("filesync: reconnecting in {} s …", backoff.as_secs());
            debug!(
                "filesync: backoff={} ms before next connection attempt",
                backoff.as_millis()
            );
            let deadline = Instant::now() + backoff;
            while Instant::now() < deadline {
                thread::sleep(Duration::from_millis(100));
                if self.stopped.load(Ordering::SeqCst) {
                    debug!("filesync: stop flag set during backoff sleep, exiting run loop");
                    return;
                }
            }
            if !self.awaiting_approval.load(Ordering::SeqCst) {
                backoff = (backoff * 2).min(Duration::from_secs(60));
                debug!("filesync: next backoff will be {} s", backoff.as_secs());
            }
        }
    }

    pub fn session(&self) -> io::Result<()> {
        if self.stopped.load(Ordering::SeqCst) {
            debug!("filesync session: shutdown already requested, not connecting");
            return Ok(());
        }

        self.engine.clear_in_progress();
        debug!("filesync session: cleared any in-progress large-file state");

        // Capture starts once per engine and keeps journalling while disconnected.
        self.engine.start_live_capture()?;

        debug!(
            "filesync session: opening TCP connection to {}",
            self.server_addr
        );
        let stream = TcpStream::connect(&self.server_addr).map_err(|e| {
            debug!(
                "filesync session: TCP connect to {} failed (kind={:?}): {e}",
                self.server_addr,
                e.kind()
            );
            e
        })?;
        debug!(
            "filesync session: TCP connected — local={:?} peer={:?}",
            stream.local_addr(),
            stream.peer_addr()
        );

        let server_name = rustls::pki_types::ServerName::try_from("filesync.local")
            .expect("static server name is valid")
            .to_owned();

        debug!("filesync session: TLS handshake starting");
        let conn = Arc::new(
            Connection::new_client(stream, self.tls_config.clone(), server_name).map_err(|e| {
                error!(
                    "filesync session: TLS handshake with {} failed (kind={:?}): {e}",
                    self.server_addr,
                    e.kind()
                );
                e
            })?,
        );
        debug!("filesync session: TLS 1.3 handshake complete");

        *self.active_conn.lock() = Some(conn.clone());
        let _active_conn_guard = ActiveConnGuard {
            slot: &self.active_conn,
        };

        {
            let known_servers_path = self.identity_dir.join("known_servers.toml");
            let mut ks = KnownServers::load_or_create(&known_servers_path);

            match &conn.peer_cert {
                None => {
                    warn!("filesync session: server did not present a certificate — cannot verify identity");
                }
                Some(der) => {
                    let server_fp = cert_fingerprint(der);
                    match ks.get_fingerprint(&self.server_addr) {
                        None => {
                            info!(
                                "filesync: trusting new server {} — pinning fingerprint {}… \
                                 (delete {:?} to re-trust after cert rotation)",
                                self.server_addr,
                                &server_fp[..16],
                                known_servers_path
                            );
                            ks.pin(&self.server_addr, &server_fp);
                        }
                        Some(stored_fp) if stored_fp == server_fp => {
                            debug!(
                                "filesync session: server fingerprint verified for {}",
                                self.server_addr
                            );
                        }
                        Some(stored_fp) => {
                            let msg = format!(
                                "filesync: SERVER FINGERPRINT MISMATCH for {}! \
                                 Stored: {}…  Got: {}…  \
                                 If the server legitimately regenerated its certificate, \
                                 delete {:?} to re-trust.",
                                self.server_addr,
                                &stored_fp[..16],
                                &server_fp[..16],
                                known_servers_path
                            );
                            error!("{msg}");
                            conn.shutdown();
                            return Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
                        }
                    }
                }
            }
        }

        debug!(
            "filesync session: sending Hello (node_id={} proto={})",
            self.engine.node_id(),
            PROTOCOL_VERSION
        );
        conn.send(&Message::Hello {
            node_id: self.engine.node_id().to_string(),
            protocol_version: PROTOCOL_VERSION,
            credential: None,
        })?;

        match recv_with_heartbeat(&conn)? {
            Message::Hello {
                node_id,
                protocol_version,
                ..
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    error!(
                        "filesync session: protocol version mismatch — client={PROTOCOL_VERSION} server={protocol_version}"
                    );
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "filesync: protocol version mismatch — \
                             client={PROTOCOL_VERSION}, server={protocol_version}"
                        ),
                    ));
                }
                // Approved — clear the awaiting flag.
                self.awaiting_approval.store(false, Ordering::SeqCst);
                if let Some(ref gs) = self.gui_state {
                    let mut s = gs.write();
                    s.status = ConnectionStatus::InitialSync;
                    s.bytes_received = 0;
                    s.bytes_sent = 0;
                    s.files_received = 0;
                    s.files_sent = 0;
                    s.transfer_total = 0;
                }
                info!("filesync: server node_id={node_id}");
                debug!(
                    "filesync session: protocol version agreed: {protocol_version} with server {node_id}"
                );
            }
            Message::ApprovalPending { fingerprint } => {
                self.awaiting_approval.store(true, Ordering::SeqCst);
                if let Some(ref gs) = self.gui_state {
                    gs.write().status = ConnectionStatus::AwaitingApproval;
                }
                info!(
                    "filesync: connection pending admin approval on server \
                     (our fingerprint: {}…)",
                    &fingerprint[..16.min(fingerprint.len())]
                );
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "awaiting_approval",
                ));
            }
            Message::Rejected { reason } => {
                self.awaiting_approval.store(false, Ordering::SeqCst);
                if let Some(ref gs) = self.gui_state {
                    gs.write().status =
                        ConnectionStatus::Error(format!("Rejected by server: {reason}"));
                }
                error!("filesync: connection rejected by server: {reason}");
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("client_rejected: {reason}"),
                ));
            }
            other => {
                error!("filesync session: expected Hello from server, got unexpected message");
                debug!("filesync session: unexpected first message variant: {other:?}");
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "filesync: expected Hello from server",
                ));
            }
        }

        // ---- Deletion-ledger exchange (server speaks first, we reply) ----
        debug!("filesync session: waiting for server LedgerExchange");
        let peer_ledger_entries = match recv_with_heartbeat(&conn)? {
            Message::LedgerExchange { entries } => entries,
            other => {
                error!("filesync session: expected LedgerExchange from server");
                debug!(
                    "filesync session: unexpected message instead of LedgerExchange: {other:?}"
                );
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "filesync: expected LedgerExchange",
                ));
            }
        };
        let merged = self.engine.merge_ledger(peer_ledger_entries);
        debug!("filesync session: merged {merged} tombstone(s) from server");
        conn.send(&Message::LedgerExchange {
            entries: self.engine.ledger_entries(),
        })?;
        debug!("filesync session: sent LedgerExchange to server");

        // ---- Action-journal drain: user actions captured while disconnected
        // reach the server before manifests are exchanged, so the server does
        // not re-send paths this client deleted or renamed offline.
        let drained = {
            let mut send = |msg: Message| conn.send(&msg);
            let mut await_ack = |upto: u64| await_action_ack(&conn, upto);
            drain_action_journal(&self.engine, &mut send, &mut await_ack)?
        };
        if drained.settled_entries > 0 {
            info!(
                "filesync: delivered {} journalled action(s) to server ({} file(s), {} B)",
                drained.settled_entries, drained.files_sent, drained.bytes_sent
            );
        }

        debug!(
            "filesync session: scanning local root {:?}",
            self.engine.root()
        );
        let local = self.engine.scan()?;
        let (l_files, l_dirs, l_bytes) = count_manifest(&local);
        debug!(
            "filesync session: local manifest — {} file(s) {} dir(s) {} B total",
            l_files, l_dirs, l_bytes
        );

        if let Some(ref gs) = self.gui_state {
            let mut s = gs.write();
            s.file_count = l_files;
            s.dir_count = l_dirs;
            s.total_bytes = l_bytes;
        }

        debug!("filesync session: sending local ManifestExchange");
        conn.send(&Message::ManifestExchange(local.clone()))?;

        debug!("filesync session: waiting for server ManifestExchange");
        let remote = match recv_with_heartbeat(&conn)? {
            Message::ManifestExchange(m) => m,
            other => {
                error!("filesync session: expected ManifestExchange from server");
                debug!(
                    "filesync session: unexpected message instead of ManifestExchange: {other:?}"
                );
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "filesync: expected ManifestExchange",
                ));
            }
        };
        {
            let r_files = remote.files.values().filter(|m| !m.is_dir).count();
            let r_dirs = remote.files.values().filter(|m| m.is_dir).count();
            let r_bytes: u64 = remote.files.values().map(|m| m.size).sum();
            debug!(
                "filesync session: server manifest received — {} file(s) {} dir(s) {} B total",
                r_files, r_dirs, r_bytes
            );
        }

        let bytes_incoming: u64 = manifest::compute_send_list(&remote, &local, true)
            .iter()
            .filter_map(|p| remote.files.get(p))
            .filter(|m| !m.is_dir)
            .map(|m| m.size)
            .sum();
        if bytes_incoming > 0 {
            match common::check_disk_space(self.engine.root(), bytes_incoming) {
                Ok(avail) => {
                    debug!(
                        "filesync session: disk-space check OK — {} B available, {} B required",
                        avail, bytes_incoming
                    );
                }
                Err(_) => {
                    let avail = common::available_disk_space(self.engine.root()).unwrap_or(0);
                    error!(
                        "filesync: not enough disk space on client — \
                         {} B available, {} B required; aborting sync",
                        avail, bytes_incoming
                    );

                    let _ = conn.send(&Message::InsufficientDiskSpace {
                        available_bytes: avail,
                        required_bytes: bytes_incoming,
                    });
                    return Err(io::Error::new(
                        io::ErrorKind::StorageFull,
                        format!(
                            "filesync: client out of disk space: \
                             {} B available, {} B required",
                            avail, bytes_incoming
                        ),
                    ));
                }
            }
        }

        let expected_rx: u64 = remote
            .files
            .iter()
            .filter(|(p, m)| !m.is_dir && !local.files.contains_key(*p))
            .map(|(_, m)| m.size)
            .sum();
        let expected_rx_files = remote
            .files
            .iter()
            .filter(|(p, m)| !m.is_dir && !local.files.contains_key(*p))
            .count();
        let expected_tx: u64 = {
            let to_send_preview = manifest::compute_send_list(&local, &remote, false);
            to_send_preview
                .iter()
                .filter_map(|p| local.files.get(p))
                .filter(|m| !m.is_dir)
                .map(|m| m.size)
                .sum()
        };
        let transfer_total = expected_rx + expected_tx;

        debug!(
            "filesync session: initial sync plan — expect_rx={} B ({} file(s)) expect_tx={} B total={} B",
            expected_rx, expected_rx_files, expected_tx, transfer_total
        );

        if let Some(ref gs) = self.gui_state {
            gs.write().transfer_total = transfer_total;
        }

        let sync_start = Instant::now();
        let mut files_received = 0usize;
        let mut dirs_received = 0usize;
        let mut bytes_received: u64 = 0;

        debug!("filesync session: entering initial-sync receive loop");
        loop {
            match recv_with_heartbeat(&conn)? {
                Message::Bundle(b) => {
                    let n_files = b.files.iter().filter(|f| !f.metadata.is_dir).count();
                    let n_dirs = b.files.iter().filter(|f| f.metadata.is_dir).count();
                    let bundle_bytes: u64 = b.files.iter().map(|f| f.metadata.size).sum();
                    debug!(
                        "filesync session: recv Bundle id={} files={} dirs={} size={} B (rx_total={} B)",
                        b.bundle_id, n_files, n_dirs, bundle_bytes,
                        bytes_received + bundle_bytes
                    );
                    let apply_result = self.engine.apply_bundle(&b)?;
                    let n = apply_result.written;
                    push_gui_conflicts(&self.gui_state, &self.engine, &apply_result.conflicts);
                    for fd in &b.files {
                        if fd.metadata.is_dir {
                            dirs_received += 1;
                        } else {
                            files_received += 1;
                            bytes_received += fd.metadata.size;
                        }
                    }

                    if let Some(ref gs) = self.gui_state {
                        let mut s = gs.write();
                        s.bytes_received = bytes_received;
                        s.files_received = files_received as u64;
                    }
                    info!("filesync: initial sync +{n} file(s) from server");
                    common::publish_changed(&self.bus, &b, "server");
                }
                Message::LargeFileStart {
                    ref metadata,
                    total_chunks,
                    action_ms,
                } => {
                    debug!(
                        "filesync session: recv LargeFileStart path={:?} size={} B chunks={} (~{} B/chunk)",
                        metadata.rel_path, metadata.size, total_chunks,
                        metadata.size / total_chunks.max(1) as u64
                    );
                    self.engine
                        .begin_large_file_with_action(metadata.clone(), total_chunks, action_ms)?;
                }
                Message::LargeFileChunk {
                    ref path,
                    chunk_index,
                    ref data,
                } => {
                    debug!(
                        "filesync session: recv LargeFileChunk path={path:?} chunk={chunk_index} size={} B rx_total={} B",
                        data.len(), bytes_received
                    );
                    bytes_received += data.len() as u64;

                    if let Some(ref gs) = self.gui_state {
                        gs.write().bytes_received = bytes_received;
                    }
                    match self
                        .engine
                        .receive_large_file_chunk(path, chunk_index, data)?
                    {
                        crate::sync_engine::ChunkResult::ReadyToCommit(hash) => {
                            debug!(
                                "filesync session: all chunks present early for {path:?}, committing"
                            );
                            self.engine.commit_large_file(path, hash)?;
                            files_received += 1;
                            if let Some(ref gs) = self.gui_state {
                                gs.write().files_received = files_received as u64;
                            }
                            info!("filesync: initial sync large file committed {path:?} (after retransmit)");
                        }
                        crate::sync_engine::ChunkResult::Pending => {}
                    }
                }
                Message::LargeFileEnd {
                    ref path,
                    final_hash,
                } => {
                    debug!("filesync session: recv LargeFileEnd path={path:?}");
                    match self.engine.finish_large_file(path, final_hash)? {
                        crate::sync_engine::FinishResult::Committed => {
                            files_received += 1;
                            if let Some(ref gs) = self.gui_state {
                                gs.write().files_received = files_received as u64;
                            }
                            debug!(
                                "filesync session: large file committed {path:?} (files_received={})",
                                files_received
                            );
                            info!("filesync: initial sync large file committed {path:?}");
                        }
                        crate::sync_engine::FinishResult::CommittedWithConflict(ci) => {
                            files_received += 1;
                            if let Some(ref gs) = self.gui_state {
                                gs.write().files_received = files_received as u64;
                            }
                            debug!(
                                "filesync session: large file committed {path:?} (files_received={})",
                                files_received
                            );
                            warn!(
                                "filesync: initial sync large file committed {path:?} \
                                 (conflict copy: {:?})",
                                ci.conflict_copy_path
                            );
                            push_gui_conflicts(&self.gui_state, &self.engine, &[ci]);
                        }
                        crate::sync_engine::FinishResult::MissingChunks(indices) => {
                            warn!(
                                "filesync: initial sync {path:?} missing {} chunk(s), requesting retransmit",
                                indices.len()
                            );
                            debug!(
                                "filesync session: missing chunk indices for {path:?}: {indices:?}"
                            );
                            conn.send(&Message::RequestChunks {
                                path: path.clone(),
                                chunk_indices: indices,
                            })?;
                        }
                    }
                }
                Message::Delete {
                    paths,
                    deleted_at_ms,
                    deleter,
                } => {
                    debug!(
                        "filesync session: recv Delete {} path(s) during initial sync",
                        paths.len()
                    );
                    if let Err(e) = common::handle_recv_delete_with_meta(
                        &self.engine,
                        &paths,
                        deleted_at_ms,
                        &deleter,
                        "server",
                        &self.bus,
                        "filesync session",
                    ) {
                        error!("filesync session: apply_deletes during initial sync: {e}");
                    }
                    if let Some(ref gs) = self.gui_state {
                        gs.write().begin_sync_activity();
                    }
                }
                Message::SyncComplete => {
                    debug!(
                        "filesync session: received SyncComplete — files_received={} dirs_received={} bytes_received={} B elapsed={}ms",
                        files_received, dirs_received, bytes_received,
                        sync_start.elapsed().as_millis()
                    );
                    break;
                }
                Message::InsufficientDiskSpace {
                    available_bytes,
                    required_bytes,
                } => {
                    error!(
                        "filesync session: server reports insufficient disk space — \
                         {} B available, {} B required; aborting sync",
                        available_bytes, required_bytes
                    );
                    return Err(io::Error::new(
                        io::ErrorKind::StorageFull,
                        format!(
                            "filesync: server out of disk space: \
                             {} B available, {} B required",
                            available_bytes, required_bytes
                        ),
                    ));
                }
                other => {
                    warn!("filesync session: unexpected message during initial-sync recv phase");
                    debug!("filesync session: unexpected message variant: {other:?}");
                }
            }
        }

        let raw_to_send = manifest::compute_send_list(&local, &remote, false);
        let snap = self.engine.ledger_snapshot();
        let (to_send, recreated) =
            manifest::filter_resurrected(raw_to_send.clone(), &local, Some(&snap));
        // Tombstone wins over our stale copy: converge by deleting locally
        // while preserving the original tombstone stamp (never re-stamp).
        {
            let send_set: HashSet<&PathBuf> = to_send.iter().collect();
            for path in raw_to_send.iter().filter(|p| !send_set.contains(p)) {
                if let Some(tomb) = snap.get(path) {
                    debug!(
                        "filesync session: tombstone wins for {path:?} — deleting local stale copy"
                    );
                    self.engine.apply_deletes(
                        &[path.clone()],
                        tomb.deleted_at_ms,
                        &tomb.deleter_node,
                    )?;
                }
            }
        }
        // Refresh: veto step changed disk + manifest.
        let local = self.engine.get_manifest();
        // Tell the server about deletes it missed while we were offline.
        for (path, deleted_at_ms, deleter) in compute_deletes_to_push(&local, &remote, &snap) {
            debug!("filesync session: pushing missed delete {path:?} to server");
            conn.send(&Message::Delete {
                paths: vec![path],
                deleted_at_ms,
                deleter,
            })?;
        }
        let files_sent = to_send
            .iter()
            .filter(|p| local.files.get(*p).map(|m| !m.is_dir).unwrap_or(false))
            .count();
        let bytes_sent: u64 = to_send
            .iter()
            .filter_map(|p| local.files.get(p))
            .map(|m| m.size)
            .sum();

        debug!(
            "filesync session: send phase — {} path(s): {} file(s) {} B",
            to_send.len(),
            files_sent,
            bytes_sent
        );

        if !to_send.is_empty() {
            info!("filesync: sending {} file(s) to server", to_send.len());
            self.engine.send_paths(&to_send, &conn)?;
            debug!("filesync session: send_paths complete");
        }
        // Legitimate recreates were just uploaded — lift their tombstones.
        self.engine.clear_tombstones(&recreated);

        debug!("filesync session: sending SyncComplete to server");
        conn.send(&Message::SyncComplete)?;

        let sync_duration_ms = sync_start.elapsed().as_millis() as u64;

        let manifest_snap = self.engine.get_manifest();
        let (local_file_count, local_dir_count, local_total_bytes) = count_manifest(&manifest_snap);

        if let Some(ref gs) = self.gui_state {
            let mut s = gs.write();
            s.bytes_sent = bytes_sent;
            s.files_sent = files_sent as u64;
            s.transfer_total = 0;
            s.status = ConnectionStatus::Idle;
            s.last_connected = Some(Instant::now());
            s.file_count = local_file_count;
            s.dir_count = local_dir_count;
            s.total_bytes = local_total_bytes;
        }

        debug!(
            "filesync session: sync complete in {}ms — sent {} file(s) {} B | received {} file(s) {} dir(s) {} B | local {} file(s) {} dir(s) {} B",
            sync_duration_ms,
            files_sent, bytes_sent,
            files_received, dirs_received, bytes_received,
            local_file_count, local_dir_count, local_total_bytes
        );

        if let Some(ref bus) = self.bus {
            bus.publish(
                "filesync",
                "filesync.sync_stats",
                serde_json::json!({
                    "node":            self.engine.node_id(),
                    "peer":            "server",
                    "role":            "client",
                    "files_sent":      files_sent,
                    "bytes_sent":      bytes_sent,
                    "files_received":  files_received,
                    "dirs_received":   dirs_received,
                    "bytes_received":  bytes_received,
                    "duration_ms":     sync_duration_ms,
                }),
            );

            bus.publish(
                "filesync",
                "filesync.sync_complete",
                serde_json::json!({
                    "node":             self.engine.node_id(),
                    "files_sent":       files_sent,
                    "files_received":   files_received,
                    "bytes_sent":       bytes_sent,
                    "bytes_received":   bytes_received,
                    "duration_ms":      sync_duration_ms,
                    "local_file_count": local_file_count,
                    "local_dir_count":  local_dir_count,
                    "local_total_bytes": local_total_bytes,
                }),
            );
        }

        info!("filesync: initial sync complete");

        let (ack_tx, ack_rx) = bounded::<u64>(16);
        let (shut_tx, shut_rx) = bounded::<()>(1);

        let eng_r = self.engine.clone();
        let conn_r = conn.clone();
        let bus_r = self.bus.clone();
        let gui_state_r = self.gui_state.clone();
        debug!("filesync session: spawning recv-loop thread");
        let recv_handle = thread::Builder::new()
            .name("recv-srv".into())
            .spawn(move || {
                recv_loop(eng_r, conn_r, bus_r, gui_state_r, ack_tx);
                let _ = shut_tx.send(());
            })?;

        debug!("filesync session: entering send-loop");
        send_loop(
            self.engine.clone(),
            conn.clone(),
            shut_rx,
            ack_rx,
            self.bus.clone(),
            self.gui_state.clone(),
        );

        debug!("filesync session: send-loop returned, shutting down connection");
        conn.shutdown();
        recv_handle.join().ok();
        debug!("filesync session: recv-loop thread joined, session complete");
        Ok(())
    }
}

/// Blocking recv with heartbeat maintenance: polls with a 2 s timeout so
/// Ping is sent while idle and a dead peer / suspend-resume gap trips the
/// heartbeat timeout (returned as Err) instead of hanging forever.
fn recv_with_heartbeat(conn: &Connection) -> io::Result<Message> {
    loop {
        match conn.recv_timeout(Duration::from_secs(HEARTBEAT_POLL_SECS)) {
            Ok(msg) => return Ok(msg),
            Err(e)
                if e.kind() == io::ErrorKind::TimedOut
                    || e.kind() == io::ErrorKind::WouldBlock =>
            {
                // Idle window: send Ping if due, trip timeout if peer is
                // stale (suspend recovery lands here on next check).
                conn.heartbeat_maintenance()?;
            }
            Err(e) => return Err(e),
        }
    }
}

fn recv_loop(
    engine: Arc<SyncEngine>,
    conn: Arc<Connection>,
    bus: Option<Arc<MessageBus>>,
    gui_state: Option<SharedState>,
    acks: Sender<u64>,
) {
    let prefix = "filesync recv";
    debug!("{prefix}: loop started, waiting for incremental messages from server");
    loop {
        match recv_with_heartbeat(&conn) {
            Ok(Message::Bundle(b)) => {
                let n_files = b.files.iter().filter(|f| !f.metadata.is_dir).count();
                let n_dirs = b.files.iter().filter(|f| f.metadata.is_dir).count();
                let bundle_bytes: u64 = b.files.iter().map(|f| f.metadata.size).sum();
                debug!(
                    "{prefix}: Bundle id={} files={} dirs={} size={} B",
                    b.bundle_id, n_files, n_dirs, bundle_bytes
                );
                match common::handle_recv_bundle(&engine, &b, "server", &bus, prefix) {
                    Ok(applied) => {
                        if let Some(ref gs) = gui_state {
                            let mut s = gs.write();
                            s.begin_sync_activity();
                            s.files_received += applied.files_count as u64;
                            s.bytes_received += applied.bytes;
                        }
                        push_gui_conflicts(&gui_state, &engine, &applied.conflicts);
                    }
                    Err(e) => error!("{prefix}: apply_bundle: {e}"),
                }

                // Send acknowledgment for this bundle
                let sequence_numbers: Vec<u64> = b
                    .files
                    .iter()
                    .map(|fd| fd.metadata.change_sequence)
                    .filter(|&seq| seq > 0)
                    .collect();

                if !sequence_numbers.is_empty() {
                    if let Err(e) = conn.send(&Message::ChangeAcknowledgment {
                        bundle_id: b.bundle_id,
                        sequence_numbers: sequence_numbers.clone(),
                    }) {
                        warn!(
                            "{prefix}: failed to send acknowledgment for bundle {}: {e}",
                            b.bundle_id
                        );
                    } else {
                        debug!(
                            "{prefix}: sent acknowledgment for bundle {} (sequences: {:?})",
                            b.bundle_id, sequence_numbers
                        );
                    }
                }
            }
            Ok(Message::LargeFileStart {
                ref metadata,
                total_chunks,
                action_ms,
            }) => {
                debug!(
                    "{prefix}: LargeFileStart path={:?} size={} B chunks={}",
                    metadata.rel_path, metadata.size, total_chunks
                );
                if let Err(e) = common::handle_recv_large_file_start(
                    &engine,
                    metadata.clone(),
                    total_chunks,
                    action_ms,
                    "server",
                    prefix,
                ) {
                    error!("{prefix}: large_file_start: {e}");
                }
                if let Some(ref gs) = gui_state {
                    gs.write().begin_sync_activity();
                }
            }
            Ok(Message::LargeFileChunk {
                ref path,
                chunk_index,
                ref data,
            }) => {
                debug!(
                    "{prefix}: LargeFileChunk path={path:?} chunk={chunk_index} size={} B",
                    data.len()
                );
                if let Err(e) = common::handle_recv_large_file_chunk(
                    &engine,
                    path,
                    chunk_index,
                    data,
                    "server",
                    prefix,
                ) {
                    error!("{prefix}: large_file_chunk: {e}");
                }
                if let Some(ref gs) = gui_state {
                    let mut s = gs.write();
                    s.begin_sync_activity();
                    s.bytes_received += data.len() as u64;
                }
            }
            Ok(Message::LargeFileEnd {
                ref path,
                final_hash,
            }) => {
                debug!("{prefix}: LargeFileEnd path={path:?}");
                match common::handle_recv_large_file_end(
                    &engine, path, final_hash, "server", &bus, prefix,
                ) {
                    Ok(LargeFileEndOutcome::MissingChunks(indices)) => {
                        warn!(
                            "{prefix}: {path:?} missing {} chunk(s), requesting retransmit",
                            indices.len()
                        );
                        debug!("{prefix}: missing chunk indices for {path:?}: {indices:?}");
                        common::request_retransmit(&conn, path, indices, prefix);
                    }
                    Ok(LargeFileEndOutcome::Committed) => {
                        debug!("{prefix}: LargeFileEnd committed {path:?}");
                        if let Some(ref gs) = gui_state {
                            let mut s = gs.write();
                            s.begin_sync_activity();
                            s.files_received += 1;
                        }
                    }
                    Ok(LargeFileEndOutcome::CommittedWithConflict(ci)) => {
                        debug!("{prefix}: LargeFileEnd committed with conflict {path:?}");
                        if let Some(ref gs) = gui_state {
                            let mut s = gs.write();
                            s.begin_sync_activity();
                            s.files_received += 1;
                        }
                        push_gui_conflicts(&gui_state, &engine, &[ci]);
                    }
                    Err(e) => error!("{prefix}: large_file_end: {e}"),
                }
            }
            Ok(Message::Delete {
                paths,
                deleted_at_ms,
                deleter,
            }) => {
                debug!("{prefix}: Delete {} path(s)", paths.len());
                if let Err(e) = common::handle_recv_delete_with_meta(
                    &engine,
                    &paths,
                    deleted_at_ms,
                    &deleter,
                    "server",
                    &bus,
                    prefix,
                ) {
                    error!("{prefix}: apply_deletes: {e}");
                }
                if let Some(ref gs) = gui_state {
                    gs.write().begin_sync_activity();
                }
            }
            Ok(Message::Rename {
                from,
                to,
                action_ms,
            }) => {
                debug!("{prefix}: Rename {from:?} → {to:?}");
                if let Err(e) = common::handle_recv_rename(
                    &engine, &from, &to, action_ms, "server", &bus, prefix,
                ) {
                    error!("{prefix}: apply_rename {from:?} → {to:?}: {e}");
                }
                if let Some(ref gs) = gui_state {
                    gs.write().begin_sync_activity();
                }
            }
            Err(e) => {
                let kind = e.kind();
                if kind == io::ErrorKind::ConnectionAborted
                    || kind == io::ErrorKind::UnexpectedEof
                    || kind == io::ErrorKind::BrokenPipe
                {
                    debug!("{prefix}: connection closed by server (kind={kind:?}): {e}");
                } else {
                    error!("{prefix}: server gone: {e}");
                    debug!("{prefix}: connection error kind={kind:?}, recv-loop exiting");
                }
                return;
            }
            Ok(Message::InsufficientDiskSpace {
                available_bytes,
                required_bytes,
            }) => {
                error!(
                    "{prefix}: server reports insufficient disk space — \
                     {available_bytes} B available, {required_bytes} B required; disconnecting"
                );
                return;
            }
            Ok(Message::ChangeAcknowledgment {
                bundle_id,
                sequence_numbers,
            }) => {
                debug!(
                    "{prefix}: ChangeAcknowledgment bundle_id={} sequences={:?}",
                    bundle_id, sequence_numbers
                );
            }
            Ok(Message::ActionBatchAck { upto_seq }) => {
                if acks.send(upto_seq).is_err() {
                    debug!("{prefix}: ActionBatchAck {upto_seq} dropped (session closing)");
                }
            }
            Ok(other) => {
                warn!("{prefix}: unexpected message in live sync phase — possible protocol issue");
                debug!("{prefix}: unexpected message variant: {other:?}");
            }
        }
    }
}

/// Live send loop (connected phase): captures periodic-rescan differences
/// into the journal and drains it, one acknowledged chunk at a time.
/// Watcher capture runs independently in the engine's capture thread.
fn send_loop(
    engine: Arc<SyncEngine>,
    conn: Arc<Connection>,
    shutdown: Receiver<()>,
    acks: Receiver<u64>,
    bus: Option<Arc<MessageBus>>,
    gui_state: Option<SharedState>,
) {
    debug!("filesync send: loop started");
    let mut rescan = PendingChanges::new();
    let mut drain_count = 0u64;

    loop {
        match shutdown.recv_timeout(Duration::from_millis(DEBOUNCE_MS)) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                debug!(
                    "filesync send: shutdown signal received after {} drain(s), exiting",
                    drain_count
                );
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        // Heartbeat tick: sends Ping when idle, breaks the loop on timeout so
        // the session tears down and run() reconnects + rescans.
        if let Err(e) = conn.heartbeat_maintenance() {
            if is_heartbeat_timeout(&e) {
                error!("filesync send: heartbeat timeout ({e}), tearing down session");
            } else {
                error!("filesync send: heartbeat maintenance failed ({e}), tearing down session");
            }
            break;
        }

        rescan_into_journal(&engine, &mut rescan);

        if engine.journal().is_empty() {
            continue;
        }
        drain_count += 1;
        let drained = {
            let mut send = |msg: Message| conn.send(&msg);
            let mut await_ack = |upto: u64| await_action_ack_live(&acks, &conn, upto);
            drain_action_journal(&engine, &mut send, &mut await_ack)
        };
        match drained {
            Ok(stats) => {
                if stats.settled_entries > 0 {
                    debug!(
                        "filesync send: drain #{drain_count} settled {} action(s), {} file(s) {} B, acked≤{}",
                        stats.settled_entries, stats.files_sent, stats.bytes_sent, stats.acked_upto
                    );
                    if let Some(ref gs) = gui_state {
                        let mut s = gs.write();
                        s.begin_sync_activity();
                        s.files_sent += stats.files_sent as u64;
                        s.bytes_sent += stats.bytes_sent;
                    }
                    if let Some(ref bus) = bus {
                        bus.publish(
                            "filesync",
                            "filesync.incremental_stats",
                            serde_json::json!({
                                "node":          engine.node_id(),
                                "peer":          "server",
                                "files_changed": stats.files_sent,
                                "bytes_sent":    stats.bytes_sent,
                            }),
                        );
                    }
                }
            }
            Err(e) => {
                error!("filesync send: journal drain error: {e}");
                debug!("filesync send: drain error kind={:?}, exiting send-loop", e.kind());
                break;
            }
        }
    }
    debug!("filesync send: loop exited after {} drain(s)", drain_count);
}

/// Feed periodic full-rescan differences into the journal as user actions
/// (the watcher can miss changes; rescans observe them at scan time).
fn rescan_into_journal(engine: &SyncEngine, rescan: &mut PendingChanges) {
    rescan.periodic_rescan(engine, "filesync");
    if rescan.changes.is_empty() && rescan.ready.is_empty() && rescan.deletes.is_empty() {
        return;
    }
    let now = crate::ledger::now_ms();
    for p in rescan.changes.drain().chain(rescan.ready.drain()) {
        if engine.is_suppressed(&p) || engine.is_excluded(&p) {
            continue;
        }
        if let Err(e) = engine.journal().upsert(p.clone(), now) {
            error!("filesync send: journal append for {p:?} failed: {e}");
        }
    }
    for p in rescan.deletes.drain() {
        if engine.is_delete_suppressed(&p) || engine.is_excluded(&p) {
            continue;
        }
        if let Err(e) = engine.journal().delete(p.clone(), now) {
            error!("filesync send: journal append for {p:?} failed: {e}");
        }
    }
}

/// Entries per drain chunk; each chunk ends with one `ActionBatchEnd`.
const DRAIN_CHUNK_MAX_ENTRIES: usize = 256;
/// Bounded pipeline between the file reader threads and the sender.
const DRAIN_PIPELINE_DEPTH: usize = 128;

/// Result of one action-journal drain pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DrainStats {
    /// Journal entries settled: acknowledged by the peer, or resolved locally.
    pub settled_entries: usize,
    /// Highest sequence number acknowledged in this pass (0 when none).
    pub acked_upto: u64,
    /// Files streamed to the peer.
    pub files_sent: usize,
    /// Bytes streamed to the peer, from file sizes at drain time.
    pub bytes_sent: u64,
    /// The pass stopped at a file still being written; retry later.
    pub stopped_unstable: bool,
}

enum Wire {
    Upsert {
        path: PathBuf,
        action_ms: u64,
        size: u64,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        action_ms: u64,
    },
    Delete {
        path: PathBuf,
        action_ms: u64,
    },
}

enum Decision {
    Send(Wire),
    Settled,
    Stop,
}

/// Deliver the action journal to the peer in acknowledged chunks.
///
/// Each entry is first admitted against the local (merged) ledger: a create
/// or modify, or a rename target, covered by a strictly newer tombstone is
/// rejected here (the local copy moves to trash, as the peer would do), and
/// an accepted action is recorded as a live action at its own stamp. Accepted
/// entries are sent in journal order, then `ActionBatchEnd{upto}` closes the
/// chunk. `await_ack(upto)` returns the peer's contiguous-applied prefix for
/// the chunk (which may be less than `upto` when the peer failed to apply
/// something mid-chunk); only entries at or below that prefix are removed
/// from the journal, and a short ack stops the pass so the remainder is
/// redelivered on the next pass starting at `last_ok + 1`. A file still being
/// written stops the pass; it is retried on the next pass.
///
/// `send` and `await_ack` abstract the transport (handshake or live loop).
pub fn drain_action_journal(
    engine: &Arc<SyncEngine>,
    send: &mut dyn FnMut(Message) -> io::Result<()>,
    await_ack: &mut dyn FnMut(u64) -> io::Result<u64>,
) -> io::Result<DrainStats> {
    let journal = engine.journal().clone();
    let pending = journal.snapshot();
    let node = engine.node_id().to_string();
    let mut stats = DrainStats::default();
    let mut i = 0usize;

    while i < pending.len() {
        let chunk_start = i;
        let mut wires: Vec<Wire> = Vec::new();
        let mut count = 0usize;
        let mut bytes = 0u64;
        let mut upto: Option<u64> = None;
        let mut stop = false;

        while i < pending.len() && count < DRAIN_CHUNK_MAX_ENTRIES {
            let entry = &pending[i];
            let size = match entry.op {
                JournalOp::Upsert => std::fs::metadata(engine.root().join(&entry.path))
                    .map(|m| m.len())
                    .unwrap_or(0),
                _ => 0,
            };
            if count > 0 && bytes + size > BUNDLE_MAX_BYTES as u64 {
                break;
            }
            match decide(engine, &node, entry, size) {
                Decision::Send(wire) => wires.push(wire),
                Decision::Settled => {}
                Decision::Stop => {
                    stop = true;
                    break;
                }
            }
            count += 1;
            bytes += size;
            upto = Some(entry.seq);
            i += 1;
        }

        if let Some(upto) = upto {
            let mut run: Vec<PathBuf> = Vec::new();
            let mut stamps: HashMap<PathBuf, u64> = HashMap::new();
            for wire in wires {
                match wire {
                    Wire::Upsert {
                        path,
                        action_ms,
                        size,
                    } => {
                        stats.files_sent += 1;
                        stats.bytes_sent += size;
                        stamps.insert(path.clone(), action_ms);
                        run.push(path);
                    }
                    Wire::Rename {
                        from,
                        to,
                        action_ms,
                    } => {
                        stream_upserts(engine, &run, &stamps, send)?;
                        run.clear();
                        stamps.clear();
                        send(Message::Rename {
                            from,
                            to,
                            action_ms,
                        })?;
                    }
                    Wire::Delete { path, action_ms } => {
                        stream_upserts(engine, &run, &stamps, send)?;
                        run.clear();
                        stamps.clear();
                        send(Message::Delete {
                            paths: vec![path],
                            deleted_at_ms: action_ms,
                            deleter: node.clone(),
                        })?;
                    }
                }
            }
            stream_upserts(engine, &run, &stamps, send)?;
            engine.flush_ledger();
            send(Message::ActionBatchEnd { upto_seq: upto })?;
            // The peer acks only its contiguous-applied prefix, which may be
            // short of `upto` (a mid-chunk apply failure or missing chunks).
            // Never delete past it: unacked entries stay journalled and are
            // redelivered on the next pass. Clamp defensively: the ack must
            // not cover entries beyond this chunk.
            let acked = await_ack(upto)?.min(upto);
            journal.ack(acked)?;
            let settled = pending[chunk_start..i]
                .iter()
                .filter(|e| e.seq <= acked)
                .count();
            stats.settled_entries += settled;
            stats.acked_upto = stats.acked_upto.max(acked);
            if acked < upto {
                debug!(
                    "filesync: partial ActionBatchAck {acked} for chunk ≤{upto} — \
                     {unacked} entr(ies) stay journalled for redelivery",
                    unacked = count - settled,
                );
                break;
            }
        }

        if stop {
            stats.stopped_unstable = true;
            break;
        }
    }
    Ok(stats)
}

/// Admission and local bookkeeping for one journal entry (see
/// [`drain_action_journal`]).
fn decide(engine: &Arc<SyncEngine>, node: &str, entry: &JournalEntry, size: u64) -> Decision {
    match &entry.op {
        JournalOp::Upsert => {
            if !engine.root().join(&entry.path).exists() {
                // Gone before delivery; a delete entry carries the removal.
                return Decision::Settled;
            }
            if !engine.is_file_stable(&entry.path) {
                return Decision::Stop;
            }
            if let Some(tomb) = engine.tombstone_beating(&entry.path, entry.action_ms) {
                warn!(
                    "filesync: local {:?} is older than tombstone @{} by {} — moving local copy to trash",
                    entry.path, tomb.deleted_at_ms, tomb.deleter_node
                );
                if let Err(e) =
                    engine.apply_deletes(&[entry.path.clone()], tomb.deleted_at_ms, &tomb.deleter_node)
                {
                    warn!("filesync: rejecting {:?}: {e}", entry.path);
                }
                return Decision::Settled;
            }
            engine.stage_live_action(&entry.path, entry.action_ms);
            Decision::Send(Wire::Upsert {
                path: entry.path.clone(),
                action_ms: entry.action_ms,
                size,
            })
        }
        JournalOp::Rename { to } => {
            if let Some(tomb) = engine.tombstone_beating(to, entry.action_ms) {
                warn!(
                    "filesync: rename target {:?} is covered by tombstone @{} by {} — deleting {:?}",
                    to, tomb.deleted_at_ms, tomb.deleter_node, entry.path
                );
                if let Err(e) =
                    engine.apply_deletes(&[to.clone()], tomb.deleted_at_ms, &tomb.deleter_node)
                {
                    warn!("filesync: rejecting rename target {to:?}: {e}");
                }
                if engine.root().join(&entry.path).exists() {
                    // Source was recreated locally since the rename: keep it.
                    return Decision::Settled;
                }
                if let Err(e) = engine.apply_deletes(&[entry.path.clone()], entry.action_ms, node) {
                    warn!("filesync: deleting rename source {:?}: {e}", entry.path);
                }
                return Decision::Send(Wire::Delete {
                    path: entry.path.clone(),
                    action_ms: entry.action_ms,
                });
            }
            engine.stage_live_action(to, entry.action_ms);
            Decision::Send(Wire::Rename {
                from: entry.path.clone(),
                to: to.clone(),
                action_ms: entry.action_ms,
            })
        }
        JournalOp::Delete => {
            if engine.root().join(&entry.path).exists() {
                // Recreated locally since the delete (a later upsert was not
                // captured, e.g. a remote apply): keep the content.
                return Decision::Settled;
            }
            engine.stage_delete(entry.path.clone(), entry.action_ms);
            Decision::Send(Wire::Delete {
                path: entry.path.clone(),
                action_ms: entry.action_ms,
            })
        }
    }
}

/// Stream a run of consecutive upserts through the bundler (preserving
/// order) and hand each message to `send` as it is produced.
fn stream_upserts(
    engine: &Arc<SyncEngine>,
    paths: &[PathBuf],
    stamps: &HashMap<PathBuf, u64>,
    send: &mut dyn FnMut(Message) -> io::Result<()>,
) -> io::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let (tx, rx) = bounded::<Message>(DRAIN_PIPELINE_DEPTH);
    thread::scope(|s| -> io::Result<()> {
        s.spawn(move || {
            let stamp = |rel: &Path, _mtime: u64| stamps.get(rel).copied();
            bundler::stream_messages(engine.root(), paths, &tx, None, &stamp);
        });
        for msg in rx {
            send(msg)?;
        }
        Ok(())
    })
}

/// Wait (handshake phase) for the `ActionBatchAck` closing `upto` and
/// return the peer's contiguous-applied prefix, which may be short of `upto`
/// when the peer failed to apply something mid-chunk. The caller keeps every
/// entry above the prefix journalled for redelivery.
fn await_action_ack(conn: &Connection, upto: u64) -> io::Result<u64> {
    loop {
        match recv_with_heartbeat(conn)? {
            Message::ActionBatchAck { upto_seq } => {
                if upto_seq < upto {
                    debug!("filesync: partial ActionBatchAck {upto_seq} while awaiting {upto}");
                }
                return Ok(upto_seq);
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("filesync: expected ActionBatchAck, got {other:?}"),
                ));
            }
        }
    }
}

/// Wait (live phase) for the ack closing `upto`, keeping heartbeats alive.
/// Returns the peer's contiguous-applied prefix (possibly short of `upto`).
fn await_action_ack_live(
    acks: &Receiver<u64>,
    conn: &Connection,
    upto: u64,
) -> io::Result<u64> {
    loop {
        match acks.recv_timeout(Duration::from_secs(HEARTBEAT_POLL_SECS)) {
            Ok(got) => {
                if got < upto {
                    debug!("filesync send: partial ActionBatchAck {got} while awaiting {upto}");
                }
                return Ok(got);
            }
            Err(RecvTimeoutError::Timeout) => conn.heartbeat_maintenance()?,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "filesync: connection closed while awaiting journal ack",
                ));
            }
        }
    }
}

/// Paths the peer still lists but our merged ledger marks deleted (tombstone
/// newer than the peer's copy): the peer missed the delete while offline.
/// Returns `(path, deleted_at_ms, deleter)` so the original stamp survives.
fn compute_deletes_to_push(
    local: &Manifest,
    remote: &Manifest,
    ledger: &DeletionLedger,
) -> Vec<(PathBuf, u64, String)> {
    let mut out: Vec<(PathBuf, u64, String)> = Vec::new();
    for (path, tomb) in ledger.iter() {
        if local.files.contains_key(path) {
            continue;
        }
        if let Some(peer_meta) = remote.files.get(path) {
            if tomb.deleted_at_ms > peer_meta.modified_ms {
                out.push((path.clone(), tomb.deleted_at_ms, tomb.deleter_node.clone()));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn push_gui_conflicts(
    gui_state: &Option<SharedState>,
    engine: &SyncEngine,
    conflicts: &[ConflictInfo],
) {
    if conflicts.is_empty() {
        return;
    }
    let Some(gs) = gui_state else { return };

    let root = engine.root();
    let mut s = gs.write();
    for ci in conflicts {
        let filename = ci
            .original_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| ci.original_path.to_string_lossy().into_owned());
        let folder_path = root
            .join(&ci.original_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned());
        let remote_modified = format_modified(&root.join(&ci.original_path));
        let local_modified = format_modified(&root.join(&ci.conflict_copy_path));

        s.push_conflict(
            filename,
            folder_path,
            local_modified,
            remote_modified,
            ConflictKind::BothModified,
        );
        s.log_event(format!(
            "Conflict detected: {:?} (local copy saved as {:?})",
            ci.original_path, ci.conflict_copy_path
        ));
    }
}

fn format_modified(path: &Path) -> String {
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => format_unix_secs(
            t.duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        ),
        Err(_) => "unknown".to_string(),
    }
}

pub fn format_unix_secs(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02} {hour:02}:{minute:02}")
}
