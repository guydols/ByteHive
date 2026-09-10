use crate::cert_fingerprint;
use crate::common::{self, LargeFileEndOutcome, PendingChanges};
use crate::gui::state::ConnectionStatus;
use crate::gui::state::{ConflictKind, SharedState};
use crate::known_hosts::KnownServers;
use crate::ledger::{DeletionLedger, Tombstone};
use crate::manifest;
use crate::protocol::*;
use crate::sync_engine::{ConflictInfo, SyncEngine};
use crate::timestamp_id;
use crate::transport::Connection;
use crate::watcher::{self, FsEvent};

use bytehive_core::MessageBus;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, TryRecvError};
use log::{debug, error, info, warn};
use parking_lot::Mutex;

use std::io;
use std::collections::HashSet;
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

        match conn.recv()? {
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

        debug!("filesync session: waiting for server ManifestExchange");
        let remote = match conn.recv()? {
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

        debug!("filesync session: sending local ManifestExchange");
        conn.send(&Message::ManifestExchange(local.clone()))?;

        // ---- Deletion-ledger exchange (symmetric order: server sent first,
        // we reply — avoids both-wait deadlock) ----
        debug!("filesync session: waiting for server LedgerExchange");
        let peer_ledger_entries = match conn.recv()? {
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
        // NOTE: LedgerAck intentionally not sent — no side reads it.

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
            match conn.recv()? {
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
                } => {
                    debug!(
                        "filesync session: recv LargeFileStart path={:?} size={} B chunks={} (~{} B/chunk)",
                        metadata.rel_path, metadata.size, total_chunks,
                        metadata.size / total_chunks.max(1) as u64
                    );
                    self.engine
                        .begin_large_file(metadata.clone(), total_chunks)?;
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
        let snap = local_ledger_snapshot(&self.engine);
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

        debug!(
            "filesync session: starting filesystem watcher on {:?}",
            self.engine.root()
        );
        let (fs_tx, fs_rx) = bounded::<FsEvent>(4096);
        let _watcher = watcher::start_watcher(self.engine.root().into(), fs_tx).map_err(|e| {
            error!(
                "filesync session: failed to start watcher on {:?}: {e}",
                self.engine.root()
            );
            e
        })?;
        debug!("filesync session: watcher started (fs_rx channel capacity=4096)");

        let (shut_tx, shut_rx) = bounded::<()>(1);

        let eng_r = self.engine.clone();
        let conn_r = conn.clone();
        let bus_r = self.bus.clone();
        let gui_state_r = self.gui_state.clone();
        debug!("filesync session: spawning recv-loop thread");
        let recv_handle = thread::Builder::new()
            .name("recv-srv".into())
            .spawn(move || {
                recv_loop(eng_r, conn_r, bus_r, gui_state_r);
                let _ = shut_tx.send(());
            })?;

        debug!("filesync session: entering send-loop");
        send_loop(
            self.engine.clone(),
            conn.clone(),
            fs_rx,
            shut_rx,
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

fn recv_loop(
    engine: Arc<SyncEngine>,
    conn: Arc<Connection>,
    bus: Option<Arc<MessageBus>>,
    gui_state: Option<SharedState>,
) {
    let prefix = "filesync recv";
    debug!("{prefix}: loop started, waiting for incremental messages from server");
    loop {
        match conn.recv() {
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
            }) => {
                debug!(
                    "{prefix}: LargeFileStart path={:?} size={} B chunks={}",
                    metadata.rel_path, metadata.size, total_chunks
                );
                if let Err(e) = common::handle_recv_large_file_start(
                    &engine,
                    metadata.clone(),
                    total_chunks,
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
            Ok(Message::Rename { from, to }) => {
                debug!("{prefix}: Rename {from:?} → {to:?}");
                if let Err(e) =
                    common::handle_recv_rename(&engine, &from, &to, "server", &bus, prefix)
                {
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
            Ok(other) => {
                warn!("{prefix}: unexpected message in live sync phase — possible protocol issue");
                debug!("{prefix}: unexpected message variant: {other:?}");
            }
        }
    }
}

fn send_loop(
    engine: Arc<SyncEngine>,
    conn: Arc<Connection>,
    fs_rx: Receiver<FsEvent>,
    shutdown: Receiver<()>,
    bus: Option<Arc<MessageBus>>,
    gui_state: Option<SharedState>,
) {
    debug!("filesync send: loop started");
    let mut pending = PendingChanges::new();
    let mut flush_count = 0u64;

    loop {
        match shutdown.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => {
                debug!(
                    "filesync send: shutdown signal received after {} flush(es), exiting",
                    flush_count
                );
                break;
            }
            Err(TryRecvError::Empty) => {}
        }

        pending.periodic_rescan(&engine, "filesync");

        match fs_rx.recv_timeout(Duration::from_millis(DEBOUNCE_MS)) {
            Ok(event) => {
                debug!("filesync send: fs event: {:?}", event);
                pending.collect_event(&engine, event);
                let total_pending =
                    pending.changes.len() + pending.ready.len() + pending.deletes.len();
                if total_pending > 100 {
                    warn!(
                        "filesync send: large pending backlog — changes={} ready={} deletes={} renames={}",
                        pending.changes.len(),
                        pending.ready.len(),
                        pending.deletes.len(),
                        pending.renames.len()
                    );
                } else {
                    debug!(
                        "filesync send: pending after event — changes={} ready={} deletes={} renames={}",
                        pending.changes.len(),
                        pending.ready.len(),
                        pending.deletes.len(),
                        pending.renames.len()
                    );
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                debug!("filesync send: watcher channel disconnected, exiting send-loop");
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        if pending.should_flush() {
            debug!(
                "filesync send: flush #{} — changes={} ready={} deletes={} renames={}",
                flush_count + 1,
                pending.changes.len(),
                pending.ready.len(),
                pending.deletes.len(),
                pending.renames.len()
            );
            flush_count += 1;
            if let Err(e) = flush_to_server(&engine, &conn, &mut pending, &bus, &gui_state) {
                error!("filesync send: flush #{flush_count} error: {e}");
                debug!(
                    "filesync send: flush error kind={:?}, breaking out of send-loop",
                    e.kind()
                );
                break;
            }
            pending.reset_timer();
            debug!("filesync send: flush #{flush_count} complete");
        }
    }
    debug!("filesync send: loop exited after {} flush(es)", flush_count);
}

/// In-memory snapshot of the engine's deletion ledger for sync decisions
/// (rebuilt from `ledger_entries`; avoids new sync_engine accessors).
fn local_ledger_snapshot(engine: &SyncEngine) -> DeletionLedger {
    let mut snap = DeletionLedger::new();
    snap.merge_remote(
        engine
            .ledger_entries()
            .into_iter()
            .map(|d| {
                let path = d.path.clone();
                (
                    path,
                    Tombstone::new(
                        d.path,
                        d.deleted_at_ms,
                        d.deleter_node,
                        d.prev_hash,
                        d.prev_mtime_ms,
                    ),
                )
            })
            .collect(),
    );
    snap
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

fn flush_to_server(
    engine: &Arc<SyncEngine>,
    conn: &Arc<Connection>,
    pending: &mut PendingChanges,
    bus: &Option<Arc<MessageBus>>,
    gui_state: &Option<SharedState>,
) -> io::Result<()> {
    let renames = pending.take_renames();
    if !renames.is_empty() {
        debug!("filesync send: flushing {} rename(s)", renames.len());
    }
    for (from, to) in renames {
        info!("filesync send: rename {from:?} → {to:?}");
        conn.send(&Message::Rename {
            from: from.clone(),
            to: to.clone(),
        })?;
        if let Some(ref gs) = gui_state {
            gs.write().begin_sync_activity();
        }
        if let Some(ref bus) = bus {
            bus.publish(
                "filesync",
                "filesync.file_renamed",
                serde_json::json!({
                    "from": from,
                    "to":   to,
                    "node": engine.node_id(),
                    "peer": "server",
                }),
            );
        }
    }

    let ready = pending.take_ready();
    if !ready.is_empty() {
        debug!("filesync send: flushing {} ready path(s)", ready.len());
        send_paths_to_server(engine, conn, bus, gui_state, ready)?;
    }

    let stable = pending.take_stable_changes(engine);
    if !stable.is_empty() {
        debug!(
            "filesync send: flushing {} stable-change path(s)",
            stable.len()
        );
        send_paths_to_server(engine, conn, bus, gui_state, stable)?;
    }

    let (paths, delete_count) = pending.take_deletes_with_engine(engine);
    if !paths.is_empty() {
        debug!(
            "filesync send: flushing {} delete path(s) ({} pre-expansion)",
            paths.len(),
            delete_count
        );
        conn.send(&Message::Delete {
            paths,
            deleted_at_ms: crate::ledger::now_ms(),
            deleter: engine.node_id().to_string(),
        })?;
        if let Some(ref gs) = gui_state {
            gs.write().begin_sync_activity();
        }

        if let Some(ref bus) = bus {
            bus.publish(
                "filesync",
                "filesync.incremental_stats",
                serde_json::json!({
                    "node":          engine.node_id(),
                    "peer":          "server",
                    "files_deleted": delete_count,
                }),
            );
        }
    }

    Ok(())
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

fn send_paths_to_server(
    engine: &Arc<SyncEngine>,
    conn: &Arc<Connection>,
    bus: &Option<Arc<MessageBus>>,
    gui_state: &Option<SharedState>,
    paths: Vec<PathBuf>,
) -> io::Result<()> {
    let manifest = engine.get_manifest();
    let files_count = paths
        .iter()
        .filter(|p| manifest.files.get(*p).map(|m| !m.is_dir).unwrap_or(true))
        .count();
    let bytes_sent: u64 = paths
        .iter()
        .filter_map(|p| manifest.files.get(p))
        .map(|m| m.size)
        .sum();

    debug!(
        "filesync send: send_paths — {} path(s): {} file(s) {} B",
        paths.len(),
        files_count,
        bytes_sent
    );
    if let Some(ref gs) = gui_state {
        gs.write().begin_sync_activity();
    }
    engine.send_paths(&paths, conn)?;
    debug!(
        "filesync send: send_paths complete — {} file(s) {} B",
        files_count, bytes_sent
    );
    if let Some(ref gs) = gui_state {
        let mut s = gs.write();
        s.begin_sync_activity();
        s.files_sent += files_count as u64;
        s.bytes_sent += bytes_sent;
    }

    if files_count > 0 {
        if let Some(ref bus) = bus {
            bus.publish(
                "filesync",
                "filesync.incremental_stats",
                serde_json::json!({
                    "node":          engine.node_id(),
                    "peer":          "server",
                    "files_changed": files_count,
                    "bytes_sent":    bytes_sent,
                }),
            );
        }
    }
    Ok(())
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
