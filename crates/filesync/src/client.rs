use crate::cert_fingerprint;
use crate::common::{self, LargeFileEndOutcome, PendingChanges};
use crate::gui::state::ConnectionStatus;
use crate::gui::state::SharedState;
use crate::known_hosts::KnownServers;
use crate::manifest;
use crate::protocol::*;
use crate::sync_engine::SyncEngine;
use crate::timestamp_id;
use crate::transport::Connection;
use crate::watcher::{self, FsEvent};

use bytehive_core::MessageBus;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, TryRecvError};
use log::{debug, error, info, warn};

use std::io;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub use crate::common::count_manifest;

pub struct Client {
    engine: Arc<SyncEngine>,
    server_addr: String,
    bus: Option<Arc<MessageBus>>,
    stopped: Arc<AtomicBool>,
    /// Directory where the client's stable identity cert and `known_servers.toml`
    /// are stored.  Populated from the framework config dir at construction.
    identity_dir: PathBuf,
    tls_config: Arc<rustls::ClientConfig>,
    gui_state: Option<SharedState>,
    /// Set to `true` when the server responds with `ApprovalPending`.  The run
    /// loop uses a longer, fixed back-off while this flag is set.
    awaiting_approval: Arc<AtomicBool>,
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
        }
    }

    pub fn engine(&self) -> &Arc<SyncEngine> {
        &self.engine
    }

    pub fn shutdown(&self) {
        debug!("filesync client: shutdown requested");
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// Whether the last connection attempt ended with an `ApprovalPending`
    /// response from the server.  The GUI can poll this to show status.
    pub fn is_awaiting_approval(&self) -> bool {
        self.awaiting_approval.load(Ordering::SeqCst)
    }

    pub fn run(&self) {
        // Default back-off for normal errors.  When the server replies with
        // ApprovalPending we use a much longer fixed interval so we don't
        // hammer it while waiting for an admin to act.
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

        // ── TOFU server fingerprint check ────────────────────────────────────
        // Verify the server's certificate fingerprint against our local store.
        // On first connection we record it (Trust On First Use).  On subsequent
        // connections we require it to match — a mismatch means the server cert
        // has changed unexpectedly (possible MITM or unannounced rotation).
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
                            // First time we connect to this server — pin its cert (TOFU).
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
                            // Fingerprint mismatch — abort immediately.
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
        {
            let l_files = local.files.values().filter(|m| !m.is_dir).count();
            let l_dirs = local.files.values().filter(|m| m.is_dir).count();
            let l_bytes: u64 = local.files.values().map(|m| m.size).sum();
            debug!(
                "filesync session: local manifest — {} file(s) {} dir(s) {} B total",
                l_files, l_dirs, l_bytes
            );
        }

        debug!("filesync session: sending local ManifestExchange");
        conn.send(&Message::ManifestExchange(local.clone()))?;

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
            let mut s = gs.write();
            s.status = ConnectionStatus::InitialSync;
            s.bytes_received = 0;
            s.bytes_sent = 0;
            s.files_received = 0;
            s.files_sent = 0;
            s.transfer_total = transfer_total;
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
                    let n = self.engine.apply_bundle(&b)?.written;
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
                Message::SyncComplete => {
                    debug!(
                        "filesync session: received SyncComplete — files_received={} dirs_received={} bytes_received={} B elapsed={}ms",
                        files_received, dirs_received, bytes_received,
                        sync_start.elapsed().as_millis()
                    );
                    break;
                }
                other => {
                    warn!("filesync session: unexpected message during initial-sync recv phase");
                    debug!("filesync session: unexpected message variant: {other:?}");
                }
            }
        }

        let to_send = manifest::compute_send_list(&local, &remote, false);
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

        debug!("filesync session: sending SyncComplete to server");
        conn.send(&Message::SyncComplete)?;

        if let Some(ref gs) = self.gui_state {
            let mut s = gs.write();
            s.bytes_sent = bytes_sent;
            s.files_sent = files_sent as u64;
            s.transfer_total = 0;
            s.status = ConnectionStatus::Idle;
            s.last_connected = Some(Instant::now());
        }

        let sync_duration_ms = sync_start.elapsed().as_millis() as u64;

        let manifest_snap = self.engine.get_manifest();
        let (local_file_count, local_dir_count, local_total_bytes) = count_manifest(&manifest_snap);

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
        debug!("filesync session: spawning recv-loop thread");
        let recv_handle = thread::Builder::new()
            .name("recv-srv".into())
            .spawn(move || {
                recv_loop(eng_r, conn_r, bus_r);
                let _ = shut_tx.send(());
            })?;

        debug!("filesync session: entering send-loop");
        send_loop(
            self.engine.clone(),
            conn.clone(),
            fs_rx,
            shut_rx,
            self.bus.clone(),
        );

        debug!("filesync session: send-loop returned, shutting down connection");
        conn.shutdown();
        recv_handle.join().ok();
        debug!("filesync session: recv-loop thread joined, session complete");
        Ok(())
    }
}

fn recv_loop(engine: Arc<SyncEngine>, conn: Arc<Connection>, bus: Option<Arc<MessageBus>>) {
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
                if let Err(e) = common::handle_recv_bundle(&engine, &b, "server", &bus, prefix) {
                    error!("{prefix}: apply_bundle: {e}");
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
                    }
                    Err(e) => error!("{prefix}: large_file_end: {e}"),
                }
            }
            Ok(Message::Delete { paths }) => {
                debug!("{prefix}: Delete {} path(s)", paths.len());
                if let Err(e) = common::handle_recv_delete(&engine, &paths, "server", &bus, prefix)
                {
                    error!("{prefix}: apply_deletes: {e}");
                }
            }
            Ok(Message::Rename { from, to }) => {
                debug!("{prefix}: Rename {from:?} → {to:?}");
                if let Err(e) =
                    common::handle_recv_rename(&engine, &from, &to, "server", &bus, prefix)
                {
                    error!("{prefix}: apply_rename {from:?} → {to:?}: {e}");
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
            if let Err(e) = flush_to_server(&engine, &conn, &mut pending, &bus) {
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

fn flush_to_server(
    engine: &Arc<SyncEngine>,
    conn: &Arc<Connection>,
    pending: &mut PendingChanges,
    bus: &Option<Arc<MessageBus>>,
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
        send_paths_to_server(engine, conn, bus, ready)?;
    }

    let stable = pending.take_stable_changes(engine);
    if !stable.is_empty() {
        debug!(
            "filesync send: flushing {} stable-change path(s)",
            stable.len()
        );
        send_paths_to_server(engine, conn, bus, stable)?;
    }

    let (paths, delete_count) = pending.take_deletes(engine.root());
    if !paths.is_empty() {
        debug!(
            "filesync send: flushing {} delete path(s) ({} pre-expansion)",
            paths.len(),
            delete_count
        );
        conn.send(&Message::Delete { paths })?;

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

fn send_paths_to_server(
    engine: &Arc<SyncEngine>,
    conn: &Arc<Connection>,
    bus: &Option<Arc<MessageBus>>,
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
    engine.send_paths(&paths, conn)?;
    debug!(
        "filesync send: send_paths complete — {} file(s) {} B",
        files_count, bytes_sent
    );

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
