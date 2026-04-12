use crate::bundler;
use crate::common::{self, LargeFileEndOutcome, PendingChanges};
use crate::manifest;
use crate::protocol::*;
use crate::sync_engine::SyncEngine;
use crate::transport::{Connection, Frame};
use crate::watcher::{self, FsEvent};

use bytehive_core::MessageBus;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use log::{debug, error, info, warn};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub type AuthChecker = Arc<dyn Fn(&str) -> bool + Send + Sync>;

struct Peer {
    broadcast_tx: Sender<Option<Frame>>,
    conn: Arc<Connection>,
}

pub struct Server {
    engine: Arc<SyncEngine>,
    bind_addr: String,
    bus: Option<Arc<MessageBus>>,
    auth_checker: Option<AuthChecker>,
    stopped: Arc<AtomicBool>,
    peers: Arc<RwLock<HashMap<String, Peer>>>,

    active_conns: Arc<RwLock<Vec<Arc<Connection>>>>,
    tls_config: Arc<rustls::ServerConfig>,
}

struct ConnGuard {
    conn: Arc<Connection>,
    active_conns: Arc<RwLock<Vec<Arc<Connection>>>>,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.active_conns
            .write()
            .retain(|c| !Arc::ptr_eq(c, &self.conn));
    }
}

impl Server {
    pub fn new_with_engine(
        engine: Arc<SyncEngine>,
        bind_addr: String,
        bus: Arc<MessageBus>,
        tls_config: Arc<rustls::ServerConfig>,
    ) -> Self {
        Self::new_with_engine_and_auth(engine, bind_addr, bus, None, tls_config)
    }

    pub fn new_with_engine_and_auth(
        engine: Arc<SyncEngine>,
        bind_addr: String,
        bus: Arc<MessageBus>,
        auth_checker: Option<AuthChecker>,
        tls_config: Arc<rustls::ServerConfig>,
    ) -> Self {
        if auth_checker.is_none() {
            warn!("filesync server: authentication is DISABLED (no auth_checker provided)");
        }
        Self {
            engine,
            bind_addr,
            bus: Some(bus),
            auth_checker,
            stopped: Arc::new(AtomicBool::new(false)),
            peers: Arc::new(RwLock::new(HashMap::new())),
            active_conns: Arc::new(RwLock::new(Vec::new())),
            tls_config,
        }
    }

    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);

        {
            let peers = self.peers.read();
            for peer in peers.values() {
                peer.conn.shutdown();
                let _ = peer.broadcast_tx.try_send(None);
            }
        }

        {
            let conns = self.active_conns.read();
            for conn in conns.iter() {
                conn.shutdown();
            }
        }
    }

    pub fn run(&self) -> io::Result<()> {
        let listener = TcpListener::bind(&self.bind_addr)?;
        listener.set_nonblocking(true)?;
        self.run_with_listener(listener)
    }

    pub fn run_with_listener(&self, listener: TcpListener) -> io::Result<()> {
        let (fs_tx, fs_rx) = bounded::<FsEvent>(4096);
        let _watcher = watcher::start_watcher(self.engine.root().into(), fs_tx).map_err(|e| {
            error!("filesync server: watcher failed to start: {e}");
            e
        })?;

        let eng = self.engine.clone();
        let peers = self.peers.clone();
        let bus = self.bus.clone();
        let stopped = self.stopped.clone();
        thread::Builder::new()
            .name("srv-watcher-broadcast".into())
            .spawn(move || local_change_broadcaster(eng, peers, fs_rx, bus, stopped))?;

        info!("filesync server listening on {} (TLS 1.3)", self.bind_addr);
        debug!("filesync server: accept loop started");

        loop {
            if self.stopped.load(Ordering::SeqCst) {
                break;
            }
            match listener.accept() {
                Ok((stream, addr)) => {
                    info!("filesync: new connection from {addr}");
                    debug!(
                        "filesync: accepted TCP connection from {addr}, spawning handler thread"
                    );
                    let eng = self.engine.clone();
                    let bus = self.bus.clone();
                    let peers = self.peers.clone();
                    let active_conns = self.active_conns.clone();
                    let auth = self.auth_checker.clone();
                    let tls = self.tls_config.clone();
                    thread::Builder::new()
                        .name(format!("srv-client-{addr}"))
                        .spawn(move || {
                            if let Err(e) =
                                handle_client(stream, eng, peers, active_conns, bus, auth, tls)
                            {
                                error!("filesync: client {addr} session error: {e}");
                            }
                            info!("filesync: client {addr} disconnected");
                        })?;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    if !self.stopped.load(Ordering::SeqCst) {
                        error!("filesync: accept error: {e}");
                    }
                    break;
                }
            }
        }
        Ok(())
    }
}

fn handle_client(
    stream: TcpStream,
    engine: Arc<SyncEngine>,
    peers: Arc<RwLock<HashMap<String, Peer>>>,
    active_conns: Arc<RwLock<Vec<Arc<Connection>>>>,
    bus: Option<Arc<MessageBus>>,
    auth_checker: Option<AuthChecker>,
    tls_config: Arc<rustls::ServerConfig>,
) -> io::Result<()> {
    let conn = Arc::new(Connection::new_server(stream, tls_config)?);
    debug!("filesync: TLS handshake complete with new client");

    active_conns.write().push(conn.clone());
    let _conn_guard = ConnGuard {
        conn: conn.clone(),
        active_conns,
    };

    let client_id = match conn.recv()? {
        Message::Hello {
            node_id,
            protocol_version,
            credential,
        } => {
            if protocol_version != PROTOCOL_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "filesync: protocol version mismatch — \
                         server={PROTOCOL_VERSION}, client={protocol_version}"
                    ),
                ));
            }
            if let Some(ref checker) = auth_checker {
                let cred = credential.as_deref().unwrap_or("");
                if !checker(cred) {
                    warn!("filesync: rejected client {node_id} — invalid credential");
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "filesync: authentication failed",
                    ));
                }
                info!("filesync: client {node_id} authenticated");
                debug!("filesync: credential accepted for {node_id}");
            }
            debug!("filesync: received Hello from {node_id} proto={protocol_version}");
            node_id
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "filesync: expected Hello as first message",
            ))
        }
    };

    conn.send(&Message::Hello {
        node_id: engine.node_id().to_string(),
        protocol_version: PROTOCOL_VERSION,
        credential: None,
    })?;
    debug!("filesync: sent Hello response to {client_id}");

    let local = engine.scan()?;
    let l_files = local.files.values().filter(|m| !m.is_dir).count();
    let l_dirs = local.files.values().filter(|m| m.is_dir).count();
    let l_bytes: u64 = local.files.values().map(|m| m.size).sum();
    debug!(
        "filesync: server manifest — {} file(s) {} dir(s) {} B",
        l_files, l_dirs, l_bytes
    );
    conn.send(&Message::ManifestExchange(local.clone()))?;
    debug!("filesync: sent ManifestExchange to {client_id}");

    let remote = match conn.recv()? {
        Message::ManifestExchange(m) => m,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "filesync: expected ManifestExchange",
            ))
        }
    };
    let r_files = remote.files.values().filter(|m| !m.is_dir).count();
    let r_dirs = remote.files.values().filter(|m| m.is_dir).count();
    debug!(
        "filesync: {client_id} manifest — {} file(s) {} dir(s)",
        r_files, r_dirs
    );

    let to_client = manifest::compute_send_list(&local, &remote, true);
    let files_sent_to_client = to_client
        .iter()
        .filter(|p| local.files.get(*p).map(|m| !m.is_dir).unwrap_or(false))
        .count();
    let bytes_sent_to_client: u64 = to_client
        .iter()
        .filter_map(|p| local.files.get(p))
        .map(|m| m.size)
        .sum();
    debug!(
        "filesync: initial sync plan for {client_id} — {} path(s): {} file(s) {} B",
        to_client.len(),
        files_sent_to_client,
        bytes_sent_to_client
    );

    if !to_client.is_empty() {
        info!(
            "filesync: sending {} file(s) to {client_id}",
            to_client.len()
        );
        engine.send_paths(&to_client, &conn)?;
        debug!("filesync: send_paths to {client_id} complete");
    }
    conn.send(&Message::SyncComplete)?;
    debug!("filesync: sent SyncComplete to {client_id}, waiting for client's initial sync");

    let mut files_received = 0usize;
    let mut bytes_received: u64 = 0;

    let sync_start = Instant::now();
    loop {
        match conn.recv()? {
            Message::Bundle(b) => {
                let n_files = b.files.iter().filter(|f| !f.metadata.is_dir).count();
                let b_bytes: u64 = b.files.iter().map(|f| f.metadata.size).sum();
                debug!(
                    "filesync: initial recv Bundle from {client_id}: bundle_id={} files={} size={} B",
                    b.bundle_id, n_files, b_bytes
                );
                let n = engine.apply_bundle(&b)?;
                for fd in &b.files {
                    if !fd.metadata.is_dir {
                        files_received += 1;
                        bytes_received += fd.metadata.size;
                    }
                }
                info!("filesync: initial sync +{n} file(s) from {client_id}");
                common::publish_changed(&bus, &b, &client_id);
                broadcast_to_others(&peers, &client_id, &Message::Bundle(b));
            }
            Message::LargeFileStart {
                ref metadata,
                total_chunks,
            } => {
                debug!(
                    "filesync: initial recv LargeFileStart from {client_id}: path={:?} size={} B chunks={}",
                    metadata.rel_path, metadata.size, total_chunks
                );
                engine.begin_large_file(metadata.clone(), total_chunks)?;
                broadcast_to_others(
                    &peers,
                    &client_id,
                    &Message::LargeFileStart {
                        metadata: metadata.clone(),
                        total_chunks,
                    },
                );
            }
            Message::LargeFileChunk {
                ref path,
                chunk_index,
                ref data,
            } => {
                debug!(
                    "filesync: initial recv LargeFileChunk from {client_id}: path={path:?} chunk={chunk_index} size={} B",
                    data.len()
                );
                bytes_received += data.len() as u64;
                let msg = Message::LargeFileChunk {
                    path: path.clone(),
                    chunk_index,
                    data: data.clone(),
                };
                match engine.receive_large_file_chunk(path, chunk_index, data) {
                    Ok(crate::sync_engine::ChunkResult::ReadyToCommit(hash)) => {
                        broadcast_to_others(&peers, &client_id, &msg);
                        match engine.commit_large_file(path, hash) {
                            Ok(_) => info!("filesync: initial sync large file committed {path:?} from {client_id} (after retransmit)"),
                            Err(e) => error!("filesync: initial sync commit after retransmit: {e}"),
                        }
                    }
                    Ok(crate::sync_engine::ChunkResult::Pending) => {
                        broadcast_to_others(&peers, &client_id, &msg);
                    }
                    Err(e) => {
                        error!("filesync: initial sync large_file_chunk from {client_id}: {e}")
                    }
                }
            }
            Message::LargeFileEnd {
                ref path,
                final_hash,
            } => {
                debug!("filesync: initial recv LargeFileEnd from {client_id}: path={path:?}");
                match engine.finish_large_file(path, final_hash) {
                    Ok(crate::sync_engine::FinishResult::Committed) => {
                        files_received += 1;
                        info!(
                            "filesync: initial sync large file committed {path:?} from {client_id}"
                        );
                        if let Some(ref bus) = bus {
                            bus.publish(
                                "filesync",
                                "filesync.file_changed",
                                serde_json::json!({ "path": path, "node": client_id }),
                            );
                        }
                        broadcast_to_others(
                            &peers,
                            &client_id,
                            &Message::LargeFileEnd {
                                path: path.clone(),
                                final_hash,
                            },
                        );
                    }
                    Ok(crate::sync_engine::FinishResult::MissingChunks(indices)) => {
                        warn!(
                            "filesync: initial sync {path:?} missing {} chunk(s), requesting retransmit",
                            indices.len()
                        );
                        if let Err(e) = conn.send(&Message::RequestChunks {
                            path: path.clone(),
                            chunk_indices: indices,
                        }) {
                            error!("filesync: initial sync send RequestChunks: {e}");
                        }
                    }
                    Err(e) => error!("filesync: initial sync large_file_end from {client_id}: {e}"),
                }
            }
            Message::SyncComplete => {
                debug!(
                    "filesync: received SyncComplete from {client_id} — initial sync complete in {}ms",
                    sync_start.elapsed().as_millis()
                );
                break;
            }
            _ => {
                warn!("filesync: unexpected message during initial sync from {client_id}");
            }
        }
    }

    let peer_count = peers.read().len() + 1;
    if let Some(ref bus) = bus {
        bus.publish(
            "filesync",
            "filesync.sync_stats",
            serde_json::json!({
                "node":                    engine.node_id(),
                "peer":                    client_id,
                "role":                    "server",
                "files_sent":              files_sent_to_client,
                "bytes_sent":              bytes_sent_to_client,
                "files_received":          files_received,
                "bytes_received":          bytes_received,
                "total_entries_in_manifest": local.files.len(),
            }),
        );

        bus.publish(
            "filesync",
            "filesync.client_joined",
            serde_json::json!({
                "client_id":       client_id,
                "connected_peers": peer_count,
                "files_exchanged": files_sent_to_client + files_received,
                "bytes_exchanged": bytes_sent_to_client + bytes_received,
            }),
        );
    }
    debug!("filesync: bus events published for {client_id} (peer_count={peer_count})");

    let (bcast_tx, bcast_rx) = bounded::<Option<Frame>>(CLIENT_BROADCAST_DEPTH);
    peers.write().insert(
        client_id.clone(),
        Peer {
            broadcast_tx: bcast_tx,
            conn: conn.clone(),
        },
    );

    debug!("filesync: spawning broadcast-fwd and recv threads for {client_id}");
    let conn_fwd = conn.clone();
    let fwd_handle = thread::Builder::new()
        .name(format!("srv-bcast-fwd-{client_id}"))
        .spawn(move || {
            while let Ok(Some(frame)) = bcast_rx.recv() {
                if conn_fwd.send_frame(frame).is_err() {
                    break;
                }
            }
        })?;

    let (done_tx, done_rx) = bounded::<()>(1);
    let eng_r = engine.clone();
    let conn_r = conn.clone();
    let peers_r = peers.clone();
    let bus_r = bus.clone();
    let cid = client_id.clone();
    let recv_handle = thread::Builder::new()
        .name(format!("srv-recv-{client_id}"))
        .spawn(move || {
            client_recv_loop(cid, eng_r, conn_r, peers_r, bus_r);
            let _ = done_tx.send(());
        })?;

    let _ = done_rx.recv();
    debug!("filesync: {client_id} recv thread done, removing from peer map");

    peers.write().remove(&client_id);
    debug!(
        "filesync: {client_id} removed from peer map ({} peers remaining)",
        peers.read().len()
    );

    conn.shutdown();
    recv_handle.join().ok();
    fwd_handle.join().ok();

    Ok(())
}

fn client_recv_loop(
    client_id: String,
    engine: Arc<SyncEngine>,
    conn: Arc<Connection>,
    peers: Arc<RwLock<HashMap<String, Peer>>>,
    bus: Option<Arc<MessageBus>>,
) {
    debug!("filesync: recv loop started for {client_id}");
    loop {
        match conn.recv() {
            Ok(Message::Bundle(b)) => {
                let n_files = b.files.iter().filter(|f| !f.metadata.is_dir).count();
                let b_bytes: u64 = b.files.iter().map(|f| f.metadata.size).sum();
                debug!(
                    "filesync: recv Bundle from {client_id}: bundle_id={} files={} size={} B",
                    b.bundle_id, n_files, b_bytes
                );
                match common::handle_recv_bundle(&engine, &b, &client_id, &bus, "filesync") {
                    Ok(_) => broadcast_to_others(&peers, &client_id, &Message::Bundle(b)),
                    Err(e) => error!("filesync: apply_bundle from {client_id}: {e}"),
                }
            }
            Ok(Message::LargeFileStart {
                metadata,
                total_chunks,
            }) => {
                debug!(
                    "filesync: recv LargeFileStart from {client_id}: path={:?} size={} B chunks={}",
                    metadata.rel_path, metadata.size, total_chunks
                );
                if common::handle_recv_large_file_start(
                    &engine,
                    metadata.clone(),
                    total_chunks,
                    &client_id,
                    "filesync",
                )
                .is_ok()
                {
                    broadcast_to_others(
                        &peers,
                        &client_id,
                        &Message::LargeFileStart {
                            metadata,
                            total_chunks,
                        },
                    );
                }
            }
            Ok(Message::LargeFileChunk {
                ref path,
                chunk_index,
                ref data,
            }) => {
                debug!(
                    "filesync: recv LargeFileChunk from {client_id}: path={path:?} chunk={chunk_index} size={} B",
                    data.len()
                );
                match common::handle_recv_large_file_chunk(
                    &engine,
                    path,
                    chunk_index,
                    data,
                    &client_id,
                    "filesync",
                ) {
                    Ok(_) => broadcast_to_others(
                        &peers,
                        &client_id,
                        &Message::LargeFileChunk {
                            path: path.clone(),
                            chunk_index,
                            data: data.clone(),
                        },
                    ),
                    Err(e) => error!("filesync: large_file_chunk from {client_id}: {e}"),
                }
            }
            Ok(Message::LargeFileEnd {
                ref path,
                final_hash,
            }) => {
                debug!("filesync: recv LargeFileEnd from {client_id}: path={path:?}");
                match common::handle_recv_large_file_end(
                    &engine, path, final_hash, &client_id, &bus, "filesync",
                ) {
                    Ok(LargeFileEndOutcome::Committed) => {
                        broadcast_to_others(
                            &peers,
                            &client_id,
                            &Message::LargeFileEnd {
                                path: path.clone(),
                                final_hash,
                            },
                        );
                    }
                    Ok(LargeFileEndOutcome::MissingChunks(indices)) => {
                        common::request_retransmit(&conn, path, indices, "filesync");
                    }
                    Err(e) => error!("filesync: large_file_end from {client_id}: {e}"),
                }
            }
            Ok(Message::Delete { paths }) => {
                debug!(
                    "filesync: recv Delete from {client_id}: {} path(s)",
                    paths.len()
                );
                match common::handle_recv_delete(&engine, &paths, &client_id, &bus, "filesync") {
                    Ok(_) => broadcast_to_others(&peers, &client_id, &Message::Delete { paths }),
                    Err(e) => error!("filesync: apply_deletes from {client_id}: {e}"),
                }
            }
            Ok(Message::Rename { from, to }) => {
                debug!("filesync: recv Rename from {client_id}: {from:?} → {to:?}");
                match common::handle_recv_rename(&engine, &from, &to, &client_id, &bus, "filesync")
                {
                    Ok(()) => {
                        broadcast_to_others(&peers, &client_id, &Message::Rename { from, to })
                    }
                    Err(e) => error!("filesync: apply_rename from {client_id}: {e}"),
                }
            }
            Ok(Message::RequestChunks {
                ref path,
                ref chunk_indices,
            }) => {
                debug!(
                    "filesync: recv RequestChunks from {client_id}: path={path:?} {} chunk(s)",
                    chunk_indices.len()
                );
                handle_request_chunks(&engine, &conn, path, chunk_indices, &client_id);
            }
            Err(e) => {
                let kind = e.kind();
                if kind == io::ErrorKind::ConnectionAborted
                    || kind == io::ErrorKind::UnexpectedEof
                    || kind == io::ErrorKind::BrokenPipe
                {
                    debug!("filesync: {client_id} disconnected (kind={kind:?}): {e}");
                } else {
                    error!("filesync: recv from {client_id}: {e}");
                    debug!("filesync: connection error from {client_id} (kind={kind:?})");
                }
                return;
            }
            _ => {
                warn!("filesync: unexpected message from {client_id} in live sync phase");
            }
        }
    }
}

fn handle_request_chunks(
    engine: &SyncEngine,
    conn: &Connection,
    path: &PathBuf,
    chunk_indices: &[u32],
    _client_id: &str,
) {
    use std::io::{Read, Seek, SeekFrom};
    debug!(
        "filesync: retransmit {path:?} — {} chunk(s) requested",
        chunk_indices.len()
    );
    let full = engine.root().join(path);
    match std::fs::File::open(&full) {
        Ok(mut file) => {
            for &idx in chunk_indices {
                let offset = idx as u64 * FILE_CHUNK_SIZE as u64;
                if file.seek(SeekFrom::Start(offset)).is_err() {
                    continue;
                }
                let mut data = vec![0u8; FILE_CHUNK_SIZE];
                let mut filled = 0;
                let mut io_err = false;
                while filled < FILE_CHUNK_SIZE {
                    match file.read(&mut data[filled..]) {
                        Ok(0) => break,
                        Ok(n) => filled += n,
                        Err(e) => {
                            error!("filesync: retransmit read chunk {idx} of {path:?}: {e}");
                            io_err = true;
                            break;
                        }
                    }
                }
                if filled == 0 || io_err {
                    continue;
                }
                data.truncate(filled);
                match conn.send(&Message::LargeFileChunk {
                    path: path.clone(),
                    chunk_index: idx,
                    data,
                }) {
                    Ok(_) => {
                        debug!("filesync: retransmitted chunk {idx} for {path:?} ({filled} B)");
                    }
                    Err(_) => break,
                }
            }
        }
        Err(e) => error!("filesync: RequestChunks open {path:?}: {e}"),
    }
}

fn local_change_broadcaster(
    engine: Arc<SyncEngine>,
    peers: Arc<RwLock<HashMap<String, Peer>>>,
    fs_rx: Receiver<FsEvent>,
    bus: Option<Arc<MessageBus>>,
    stopped: Arc<AtomicBool>,
) {
    let mut pending = PendingChanges::new();
    debug!("filesync server: local change broadcaster started");

    loop {
        if stopped.load(Ordering::SeqCst) {
            break;
        }

        pending.periodic_rescan(&engine, "filesync server");

        match fs_rx.recv_timeout(Duration::from_millis(DEBOUNCE_MS)) {
            Ok(event) => pending.collect_event(&engine, event),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }

        if pending.should_flush() {
            debug!("filesync server: flushing local changes");
            flush_local_changes(&engine, &peers, &bus, &mut pending);
        }
    }
}

fn flush_local_changes(
    engine: &Arc<SyncEngine>,
    peers: &Arc<RwLock<HashMap<String, Peer>>>,
    bus: &Option<Arc<MessageBus>>,
    pending: &mut PendingChanges,
) {
    let rn = pending.renames.len();
    if rn > 0 {
        debug!("filesync server: broadcasting {} rename(s)", rn);
    }
    for (from, to) in pending.take_renames() {
        log::info!("filesync: server broadcast rename {from:?} → {to:?}");
        if let Some(ref bus) = bus {
            bus.publish(
                "filesync",
                "filesync.file_renamed",
                serde_json::json!({
                    "from": from,
                    "to":   to,
                    "node": engine.node_id(),
                }),
            );
        }
        broadcast_to_others(peers, "", &Message::Rename { from, to });
    }

    let ready_paths = pending.take_ready();
    if !ready_paths.is_empty() {
        debug!(
            "filesync server: broadcasting {} ready path(s)",
            ready_paths.len()
        );
        broadcast_paths(engine, peers, bus, ready_paths);
    }

    let stable_paths = pending.take_stable_changes(engine);
    if !stable_paths.is_empty() {
        debug!(
            "filesync server: broadcasting {} stable-change path(s)",
            stable_paths.len()
        );
        broadcast_paths(engine, peers, bus, stable_paths);
    }

    if !pending.deletes.is_empty() {
        debug!(
            "filesync server: broadcasting {} delete(s)",
            pending.deletes.len()
        );
        let (paths, _) = pending.take_deletes(engine.root());
        if let Some(ref bus) = bus {
            bus.publish(
                "filesync",
                "filesync.file_deleted",
                serde_json::json!({ "paths": paths, "node": engine.node_id() }),
            );
            bus.publish(
                "filesync",
                "filesync.incremental_stats",
                serde_json::json!({
                    "node":          engine.node_id(),
                    "peer":          "broadcast",
                    "files_deleted": paths.len(),
                }),
            );
        }
        broadcast_to_others(peers, "", &Message::Delete { paths });
    }

    pending.reset_timer();
}

fn broadcast_paths(
    engine: &Arc<SyncEngine>,
    peers: &Arc<RwLock<HashMap<String, Peer>>>,
    bus: &Option<Arc<MessageBus>>,
    paths: Vec<PathBuf>,
) {
    const BROADCAST_PIPELINE_DEPTH: usize = 128;
    debug!(
        "filesync server: broadcast_paths — {} path(s) to {} peer(s)",
        paths.len(),
        peers.read().len()
    );

    let (msg_tx, msg_rx) = bounded::<Message>(BROADCAST_PIPELINE_DEPTH);
    let root = engine.root().to_path_buf();
    let paths_for_thread = paths.clone();
    std::thread::Builder::new()
        .name("broadcast-reader".into())
        .spawn(move || bundler::stream_messages(&root, &paths_for_thread, &msg_tx))
        .expect("spawn broadcast-reader");

    let mut total_files = 0usize;
    let mut total_bytes = 0u64;

    for msg in msg_rx {
        match &msg {
            Message::Bundle(bundle) => {
                if let Some(ref bus) = bus {
                    for fd in &bundle.files {
                        if !fd.metadata.is_dir {
                            total_files += 1;
                            total_bytes += fd.metadata.size;
                        }
                        bus.publish(
                            "filesync",
                            "filesync.file_changed",
                            serde_json::json!({
                                "path": fd.metadata.rel_path,
                                "node": engine.node_id()
                            }),
                        );
                    }
                }
            }

            Message::LargeFileStart { metadata, .. } => {
                if let Some(ref bus) = bus {
                    total_files += 1;
                    total_bytes += metadata.size;
                    bus.publish(
                        "filesync",
                        "filesync.file_changed",
                        serde_json::json!({
                            "path": metadata.rel_path,
                            "node": engine.node_id()
                        }),
                    );
                }
            }
            _ => {}
        }
        broadcast_to_others(peers, "", &msg);
    }

    if total_files > 0 {
        debug!(
            "filesync server: broadcast_paths complete — {} file(s) {} B",
            total_files, total_bytes
        );
        if let Some(ref bus) = bus {
            bus.publish(
                "filesync",
                "filesync.incremental_stats",
                serde_json::json!({
                    "node":          engine.node_id(),
                    "peer":          "broadcast",
                    "files_changed": total_files,
                    "bytes_sent":    total_bytes,
                }),
            );
        }
    }
}

fn broadcast_to_others(peers: &Arc<RwLock<HashMap<String, Peer>>>, sender_id: &str, msg: &Message) {
    let peers = peers.read();
    if peers.is_empty() {
        return;
    }

    let frame = match crate::protocol::serialise_message(msg) {
        Ok(f) => Arc::new(f),
        Err(e) => {
            error!("filesync: broadcast serialise error: {e}");
            return;
        }
    };

    for (id, peer) in peers.iter() {
        if id == sender_id {
            continue;
        }
        match peer.broadcast_tx.try_send(Some(frame.clone())) {
            Ok(_) => {}
            Err(TrySendError::Full(_)) => {
                warn!("filesync: broadcast to {id} dropped (queue full)");
            }
            Err(TrySendError::Disconnected(_)) => {
                log::debug!("filesync: broadcast to {id} skipped (peer disconnecting)");
            }
        }
    }
}
