use crate::client::Client;
use crate::exclusions::{ExclusionConfig, Exclusions};
use crate::known_hosts::{ClientStatus, KnownClients};
use crate::server::Server;
use crate::sync_engine::SyncEngine;
use crate::{cert_fingerprint, timestamp_id};

use bytehive_core::{
    App, AppContext, AppManifest, BusMessage, CoreError, HttpRequest, HttpResponse, MessageBus,
};
use parking_lot::{Mutex, RwLock};
use rustls::crypto::ring::cipher_suite::{
    TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const METRICS_INTERVAL_SECS: u64 = 60;

// ─────────────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct FileSyncConfig {
    pub root: PathBuf,
    pub mode: String,
    pub bind_addr: Option<String>,
    pub server_addr: Option<String>,

    /// Kept for backward compatibility with existing config files.
    /// No longer used for TCP authentication; identity is now established via
    /// the mutual-TLS certificate fingerprint stored in `known_clients.toml`.
    #[serde(default)]
    pub auth_token: Option<String>,

    #[serde(default)]
    pub exclude_patterns: Vec<String>,

    #[serde(default)]
    pub exclude_regex: Vec<String>,
}

impl FileSyncConfig {
    pub fn exclusions(&self) -> Arc<Exclusions> {
        Arc::new(Exclusions::compile(&ExclusionConfig {
            exclude_patterns: self.exclude_patterns.clone(),
            exclude_regex: self.exclude_regex.clone(),
        }))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// App state
// ─────────────────────────────────────────────────────────────────────────────

enum ShutdownHandle {
    Server(Arc<Server>),
    Client(Arc<Client>),
}

struct State {
    engine: Arc<SyncEngine>,
    cfg: FileSyncConfig,
    shutdown: ShutdownHandle,
    /// Only `Some` in server mode.
    known_clients: Option<Arc<Mutex<KnownClients>>>,
    bus: Arc<MessageBus>,
    #[allow(dead_code)]
    filesync_dir: PathBuf,
}

// ─────────────────────────────────────────────────────────────────────────────
// FileSyncApp
// ─────────────────────────────────────────────────────────────────────────────

pub struct FileSyncApp {
    state: RwLock<Option<Arc<State>>>,
}

impl FileSyncApp {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: RwLock::new(None),
        })
    }
}

impl App for FileSyncApp {
    fn manifest(&self) -> AppManifest {
        AppManifest {
            name: "filesync",
            version: env!("CARGO_PKG_VERSION"),
            description: "Bidirectional file-system sync over TLS 1.3 with LZ4 compression.",
            http_prefix: Some("/api/filesync"),
            ui_prefix: Some("/apps/filesync"),
            nav_label: "Filesync",
            nav_icon: "\u{1F5C2}",
            show_in_nav: false,
            subscriptions: &[],
            publishes: &[
                "filesync.file_changed",
                "filesync.file_deleted",
                "filesync.sync_complete",
                "filesync.sync_stats",
                "filesync.incremental_stats",
                "filesync.root_stats",
                "filesync.client_joined",
                "filesync.client_approval_needed",
                "filesync.client_approved",
                "filesync.client_rejected",
            ],
        }
    }

    fn start(&self, ctx: AppContext) -> Result<(), CoreError> {
        let cfg: FileSyncConfig = ctx
            .config
            .get()
            .map_err(|e| CoreError::Config(format!("filesync config: {e}")))?;

        // Filesync-specific state directory lives under the main config dir.
        // Future server path:  /etc/bytehive/filesync/
        // Future client path:  /home/$USER/.config/bytehive/filesync/
        let filesync_dir = ctx.config_dir().join("filesync");
        std::fs::create_dir_all(&filesync_dir).map_err(|e| {
            CoreError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("filesync: cannot create state dir {filesync_dir:?}: {e}"),
            ))
        })?;

        let node_id = if cfg.mode == "server" {
            format!("srv-{:x}", timestamp_id())
        } else {
            format!("cli-{:x}", timestamp_id())
        };

        let exclusions = cfg.exclusions();
        log::info!(
            "filesync: {} exclusion rule(s) active",
            exclusions.rule_count()
        );

        let engine = Arc::new(SyncEngine::new(cfg.root.clone(), node_id, exclusions));

        {
            let eng = Arc::clone(&engine);
            thread::Builder::new()
                .name("filesync-initial-scan".into())
                .spawn(move || match eng.scan() {
                    Ok(m) => {
                        let (fc, dc, tb) = crate::client::count_manifest(&m);
                        log::info!(
                            "filesync: initial scan complete — {fc} file(s), {dc} dir(s), {tb} byte(s)"
                        );
                    }
                    Err(e) => {
                        log::warn!("filesync: initial scan failed (metrics will show 0 until next scan): {e}");
                    }
                })
                .map_err(CoreError::Io)?;
        }

        match cfg.mode.as_str() {
            "server" => {
                let bind = cfg.bind_addr.clone().ok_or_else(|| {
                    CoreError::Config("filesync: bind_addr required for server mode".into())
                })?;

                let bus = Arc::clone(&ctx.bus);
                let eng = Arc::clone(&engine);

                let known_clients = Arc::new(Mutex::new(KnownClients::load_from_config(
                    ctx.config_path.clone(),
                )));

                let tls_server_config = build_server_tls_config(&filesync_dir)
                    .map_err(|e| CoreError::Config(format!("filesync: TLS setup failed: {e}")))?;

                let server = Arc::new(Server::new(
                    eng,
                    bind,
                    Arc::clone(&bus),
                    Arc::clone(&known_clients),
                    tls_server_config,
                ));

                let srv = Arc::clone(&server);
                thread::Builder::new()
                    .name("filesync-server".into())
                    .spawn(move || {
                        if let Err(e) = srv.run() {
                            log::error!("filesync server error: {e}");
                        }
                    })
                    .map_err(CoreError::Io)?;

                spawn_metrics_thread(Arc::clone(&engine), Arc::clone(&ctx.bus), cfg.mode.clone());

                *self.state.write() = Some(Arc::new(State {
                    engine,
                    cfg: cfg.clone(),
                    shutdown: ShutdownHandle::Server(server),
                    known_clients: Some(known_clients),
                    bus,
                    filesync_dir,
                }));
            }

            "client" => {
                let server_addr = cfg.server_addr.clone().ok_or_else(|| {
                    CoreError::Config("filesync: server_addr required for client mode".into())
                })?;

                let bus = Arc::clone(&ctx.bus);
                let eng = Arc::clone(&engine);

                let tls_client_config = build_client_tls_config(&filesync_dir)
                    .map_err(|e| CoreError::Config(format!("filesync: TLS setup failed: {e}")))?;

                let client = Arc::new(Client::new_with_engine(
                    eng,
                    server_addr,
                    Arc::clone(&bus),
                    filesync_dir.clone(),
                    tls_client_config,
                ));

                let cli = Arc::clone(&client);
                thread::Builder::new()
                    .name("filesync-client".into())
                    .spawn(move || cli.run())
                    .map_err(CoreError::Io)?;

                spawn_metrics_thread(Arc::clone(&engine), Arc::clone(&ctx.bus), cfg.mode.clone());

                *self.state.write() = Some(Arc::new(State {
                    engine,
                    cfg: cfg.clone(),
                    shutdown: ShutdownHandle::Client(client),
                    known_clients: None,
                    bus,
                    filesync_dir,
                }));
            }

            other => {
                return Err(CoreError::Config(format!(
                    "filesync: unknown mode {other:?} (expected \"server\" or \"client\")"
                )));
            }
        }

        log::info!(
            "filesync started in {} mode (TLS 1.3 mutual auth), root={:?}",
            cfg.mode,
            cfg.root
        );
        Ok(())
    }

    fn stop(&self) {
        log::info!("filesync stopping");
        let state = self.state.write().take();
        if let Some(s) = state {
            match &s.shutdown {
                ShutdownHandle::Server(srv) => srv.shutdown(),
                ShutdownHandle::Client(cli) => cli.shutdown(),
            }
        }
    }

    fn handle_http(&self, req: &HttpRequest) -> Option<HttpResponse> {
        let guard = self.state.read();
        let state = guard.as_ref()?;

        let sub = req.path.strip_prefix("/api/filesync").unwrap_or(&req.path);

        match (req.method.as_str(), sub) {
            // ── status & manifest ──────────────────────────────────────────
            ("GET", "" | "/" | "/status") => {
                let manifest = state.engine.get_manifest();
                let (file_count, dir_count, total_bytes) = crate::client::count_manifest(&manifest);
                let pending = state
                    .known_clients
                    .as_ref()
                    .map(|kc| kc.lock().pending_count())
                    .unwrap_or(0);
                Some(HttpResponse::ok_json(json!({
                    "mode":              state.cfg.mode,
                    "root":              state.cfg.root,
                    "node_id":           state.engine.node_id(),
                    "file_count":        file_count,
                    "dir_count":         dir_count,
                    "total_bytes":       total_bytes,
                    "tls":               "TLS 1.3 / ECDSA P-384 (mutual auth)",
                    "pending_approvals": pending,
                    "exclude_patterns":  state.cfg.exclude_patterns,
                    "exclude_regex":     state.cfg.exclude_regex,
                })))
            }

            ("GET", "/manifest") => {
                let manifest = state.engine.get_manifest();
                let files: Vec<_> = manifest
                    .files
                    .values()
                    .map(|m| {
                        json!({
                            "path":        m.rel_path,
                            "size":        m.size,
                            "modified_ms": m.modified_ms,
                            "is_dir":      m.is_dir,
                            "hash":        crate::hex(&m.hash),
                        })
                    })
                    .collect();
                Some(HttpResponse::ok_json(json!({ "files": files })))
            }

            ("POST", "/rescan") => match state.engine.scan() {
                Ok(m) => {
                    let (file_count, dir_count, total_bytes) = crate::client::count_manifest(&m);
                    Some(HttpResponse::ok_json(json!({
                        "ok":          true,
                        "file_count":  file_count,
                        "dir_count":   dir_count,
                        "total_bytes": total_bytes,
                    })))
                }
                Err(e) => Some(HttpResponse::internal_error(e.to_string())),
            },

            // ── known-clients (server mode only) ──────────────────────────
            ("GET", "/known-clients") => {
                let Some(ref kc_arc) = state.known_clients else {
                    return Some(HttpResponse::ok_json(json!({
                        "clients": [],
                        "mode": state.cfg.mode,
                        "note": "known-clients management is only available in server mode"
                    })));
                };
                let kc = kc_arc.lock();
                let clients: Vec<_> = kc
                    .list()
                    .iter()
                    .map(|c| {
                        json!({
                            "node_id":       c.node_id,
                            "fingerprint":   c.fingerprint,
                            "label":         c.label,
                            "status":        c.status.as_str(),
                            "addr":          c.addr,
                            "first_seen_ms": c.first_seen_ms,
                            "last_seen_ms":  c.last_seen_ms,
                        })
                    })
                    .collect();
                Some(HttpResponse::ok_json(json!({ "clients": clients })))
            }

            ("POST", sub) if sub.starts_with("/known-clients/") && sub.ends_with("/approve") => {
                let fp = sub
                    .trim_start_matches("/known-clients/")
                    .trim_end_matches("/approve");
                let Some(ref kc_arc) = state.known_clients else {
                    return Some(HttpResponse::bad_request(
                        "known-clients management is only available in server mode".to_string(),
                    ));
                };
                let node_id = {
                    let mut kc = kc_arc.lock();
                    if !kc.set_status(fp, ClientStatus::Allowed) {
                        return Some(HttpResponse::not_found(format!(
                            "no client with fingerprint {fp}"
                        )));
                    }
                    kc.list()
                        .iter()
                        .find(|c| c.fingerprint == fp)
                        .map(|c| c.node_id.clone())
                        .unwrap_or_default()
                };
                state.bus.publish(
                    "filesync",
                    "filesync.client_approved",
                    json!({ "fingerprint": fp, "node_id": node_id }),
                );
                log::info!(
                    "filesync: client approved — node={node_id} fp={}…",
                    &fp[..16.min(fp.len())]
                );
                Some(HttpResponse::ok_json(json!({ "ok": true })))
            }

            ("POST", sub) if sub.starts_with("/known-clients/") && sub.ends_with("/reject") => {
                let fp = sub
                    .trim_start_matches("/known-clients/")
                    .trim_end_matches("/reject");
                let Some(ref kc_arc) = state.known_clients else {
                    return Some(HttpResponse::bad_request(
                        "known-clients management is only available in server mode".to_string(),
                    ));
                };
                let node_id = {
                    let mut kc = kc_arc.lock();
                    if !kc.set_status(fp, ClientStatus::Rejected) {
                        return Some(HttpResponse::not_found(format!(
                            "no client with fingerprint {fp}"
                        )));
                    }
                    kc.list()
                        .iter()
                        .find(|c| c.fingerprint == fp)
                        .map(|c| c.node_id.clone())
                        .unwrap_or_default()
                };
                state.bus.publish(
                    "filesync",
                    "filesync.client_rejected",
                    json!({ "fingerprint": fp, "node_id": node_id }),
                );
                log::info!(
                    "filesync: client rejected — node={node_id} fp={}…",
                    &fp[..16.min(fp.len())]
                );
                Some(HttpResponse::ok_json(json!({ "ok": true })))
            }

            ("POST", sub) if sub.starts_with("/known-clients/") && sub.ends_with("/label") => {
                let fp = sub
                    .trim_start_matches("/known-clients/")
                    .trim_end_matches("/label");
                let Some(ref kc_arc) = state.known_clients else {
                    return Some(HttpResponse::bad_request(
                        "known-clients management is only available in server mode".to_string(),
                    ));
                };
                let label = req
                    .json()
                    .ok()
                    .and_then(|v: serde_json::Value| v["label"].as_str().map(|s| s.to_string()))
                    .unwrap_or_default();
                let mut kc = kc_arc.lock();
                if kc.set_label(fp, &label) {
                    Some(HttpResponse::ok_json(json!({ "ok": true })))
                } else {
                    Some(HttpResponse::not_found(format!(
                        "no client with fingerprint {fp}"
                    )))
                }
            }

            ("DELETE", sub) if sub.starts_with("/known-clients/") => {
                let fp = sub.trim_start_matches("/known-clients/");
                let Some(ref kc_arc) = state.known_clients else {
                    return Some(HttpResponse::bad_request(
                        "known-clients management is only available in server mode".to_string(),
                    ));
                };
                let mut kc = kc_arc.lock();
                if kc.remove(fp) {
                    log::info!(
                        "filesync: client entry removed — fp={}…",
                        &fp[..16.min(fp.len())]
                    );
                    Some(HttpResponse::ok_json(json!({ "ok": true })))
                } else {
                    Some(HttpResponse::not_found(format!(
                        "no client with fingerprint {fp}"
                    )))
                }
            }

            _ => None,
        }
    }

    fn on_message(&self, _msg: &Arc<BusMessage>) {}
}

// ─────────────────────────────────────────────────────────────────────────────
// Metrics
// ─────────────────────────────────────────────────────────────────────────────

fn spawn_metrics_thread(engine: Arc<SyncEngine>, bus: Arc<MessageBus>, mode: String) {
    thread::Builder::new()
        .name("filesync-metrics".into())
        .spawn(move || metrics_loop(engine, bus, mode))
        .expect("spawn filesync-metrics thread");
}

fn metrics_loop(engine: Arc<SyncEngine>, bus: Arc<MessageBus>, mode: String) {
    loop {
        thread::sleep(Duration::from_secs(METRICS_INTERVAL_SECS));

        let manifest = engine.get_manifest();
        let (file_count, dir_count, total_bytes) = crate::client::count_manifest(&manifest);

        bus.publish(
            "filesync",
            "filesync.root_stats",
            json!({
                "node":          manifest.node_id,
                "mode":          mode,
                "file_count":    file_count,
                "dir_count":     dir_count,
                "total_bytes":   total_bytes,
                "total_entries": file_count + dir_count,
                "root":          engine.root(),
            }),
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TLS helpers
// ─────────────────────────────────────────────────────────────────────────────

fn strong_crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::CryptoProvider {
        cipher_suites: vec![TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256],
        ..rustls::crypto::ring::default_provider()
    })
}

/// Load a certificate + private key pair from `{dir}/{name}.der` and
/// `{dir}/{name}.key.der`.  If either file is missing a fresh ECDSA P-384
/// self-signed keypair is generated, saved, and returned.
///
/// This gives every node a *stable* long-lived identity that survives
/// process restarts.  The certificate fingerprint is what the known-hosts
/// system uses to identify peers.
fn load_or_generate_identity(
    dir: &Path,
    name: &str,
) -> Result<(CertificateDer<'static>, PrivatePkcs8KeyDer<'static>), String> {
    let cert_path = dir.join(format!("{name}.der"));
    let key_path = dir.join(format!("{name}.key.der"));

    if cert_path.exists() && key_path.exists() {
        let cert_bytes =
            std::fs::read(&cert_path).map_err(|e| format!("cannot read {cert_path:?}: {e}"))?;
        let key_bytes =
            std::fs::read(&key_path).map_err(|e| format!("cannot read {key_path:?}: {e}"))?;

        let fp = cert_fingerprint(&cert_bytes);
        log::debug!(
            "filesync: loaded {name} identity from disk (fp: {}…)",
            &fp[..16]
        );

        Ok((
            CertificateDer::from(cert_bytes),
            PrivatePkcs8KeyDer::from(key_bytes),
        ))
    } else {
        let (cert, key) = generate_self_signed_cert()?;

        if let Err(e) = std::fs::create_dir_all(dir) {
            return Err(format!("cannot create dir {dir:?}: {e}"));
        }
        std::fs::write(&cert_path, cert.as_ref())
            .map_err(|e| format!("cannot write {cert_path:?}: {e}"))?;
        std::fs::write(&key_path, key.secret_pkcs8_der())
            .map_err(|e| format!("cannot write {key_path:?}: {e}"))?;

        let fp = cert_fingerprint(cert.as_ref());
        log::info!(
            "filesync: generated new {name} identity certificate \
             (fp: {}…) at {cert_path:?}",
            &fp[..16]
        );

        Ok((cert, key))
    }
}

fn generate_self_signed_cert(
) -> Result<(CertificateDer<'static>, PrivatePkcs8KeyDer<'static>), String> {
    use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P384_SHA384};

    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384)
        .map_err(|e| format!("key generation failed: {e}"))?;

    let params = CertificateParams::new(vec!["filesync.local".to_string()])
        .map_err(|e| format!("cert params failed: {e}"))?;

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("self-sign failed: {e}"))?;

    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivatePkcs8KeyDer::from(key_pair.serialize_der());

    Ok((cert_der, key_der))
}

/// Build the server TLS config.
///
/// * Stable ECDSA P-384 self-signed certificate (persisted in
///   `{filesync_dir}/server.der` + `server.key.der`).
/// * Mutual TLS mandatory: every connecting client must present a certificate.
///   The TLS layer accepts any cert; the application layer checks the
///   fingerprint against `known_clients.toml`.
pub fn build_server_tls_config(filesync_dir: &Path) -> Result<Arc<rustls::ServerConfig>, String> {
    let (cert, key) = load_or_generate_identity(filesync_dir, "server")?;

    let config = rustls::ServerConfig::builder_with_provider(strong_crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("protocol version config failed: {e}"))?
        .with_client_cert_verifier(Arc::new(AcceptAnyClientCert))
        .with_single_cert(vec![cert], key.into())
        .map_err(|e| format!("certificate config failed: {e}"))?;

    Ok(Arc::new(config))
}

/// Build a client TLS config with an ephemeral (in-memory only) certificate.
///
/// Used as a fallback when the identity directory cannot be created or written
/// (e.g. during tests or in restricted environments).  The client will still
/// be able to connect but will not have a stable fingerprint — the server will
/// treat it as a new unknown client on every restart.
pub fn build_ephemeral_client_tls_config() -> Arc<rustls::ClientConfig> {
    let (cert, key) = generate_self_signed_cert().expect("ephemeral cert generation failed");
    rustls::ClientConfig::builder_with_provider(strong_crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3 must be available")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_client_auth_cert(vec![cert], key.into())
        .expect("ephemeral client cert config failed")
        .into()
}

/// Build the client TLS config.
///
/// * Stable ECDSA P-384 self-signed certificate (persisted in
///   `{filesync_dir}/client.der` + `client.key.der`).  Presented to the
///   server during the mutual-TLS handshake (proves key ownership).
/// * `AcceptAnyCert` for the server-cert verifier: the TLS layer accepts
///   any server certificate; the application layer verifies the fingerprint
///   against `known_servers.toml` (TOFU model).
pub fn build_client_tls_config(filesync_dir: &Path) -> Result<Arc<rustls::ClientConfig>, String> {
    let (cert, key) = load_or_generate_identity(filesync_dir, "client")?;

    let config = rustls::ClientConfig::builder_with_provider(strong_crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("protocol version config failed: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_client_auth_cert(vec![cert], key.into())
        .map_err(|e| format!("client certificate config failed: {e}"))?;

    Ok(Arc::new(config))
}

// ─────────────────────────────────────────────────────────────────────────────
// TLS certificate verifiers
// ─────────────────────────────────────────────────────────────────────────────

/// Client-side verifier: accepts any server certificate at the TLS layer.
///
/// The actual server fingerprint check (TOFU) is performed at the application
/// layer in `Client::session()` *after* the handshake completes.
#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Server-side verifier: accepts any client certificate at the TLS layer.
///
/// Accepting any cert allows the TLS handshake to complete, giving us
/// encryption + proof-of-key-ownership via the TLS 1.3 signature.  The
/// application layer then checks the fingerprint against `known_clients.toml`
/// and rejects, pends, or allows the client accordingly.
#[derive(Debug)]
struct AcceptAnyClientCert;

impl rustls::server::danger::ClientCertVerifier for AcceptAnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // TLS 1.3 only — reject any TLS 1.2 attempt.
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
