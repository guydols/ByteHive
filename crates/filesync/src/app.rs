use crate::client::Client;
use crate::exclusions::{ExclusionConfig, Exclusions};
use crate::server::{AuthChecker, Server};
use crate::sync_engine::SyncEngine;
use crate::{hex, timestamp_id};

use bytehive_core::{
    App, AppContext, AppManifest, BusMessage, CoreError, HttpRequest, HttpResponse, MessageBus,
};
use parking_lot::RwLock;
use rustls::crypto::ring::cipher_suite::{
    TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const METRICS_INTERVAL_SECS: u64 = 60;

#[derive(Debug, Clone, Deserialize)]
pub struct FileSyncConfig {
    pub root: PathBuf,
    pub mode: String,
    pub bind_addr: Option<String>,
    pub server_addr: Option<String>,

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

enum ShutdownHandle {
    Server(Arc<Server>),
    Client(Arc<Client>),
}

struct State {
    engine: Arc<SyncEngine>,
    cfg: FileSyncConfig,
    shutdown: ShutdownHandle,
}

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
            ],
        }
    }

    fn start(&self, ctx: AppContext) -> Result<(), CoreError> {
        let cfg: FileSyncConfig = ctx
            .config
            .get()
            .map_err(|e| CoreError::Config(format!("filesync config: {e}")))?;

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

                let auth_service = ctx.auth_service.clone();
                let auth_checker: AuthChecker = Arc::new(move |cred: &str| {
                    auth_service.authenticate_credential(cred).is_some()
                });

                let tls_server_config = build_server_tls_config()
                    .map_err(|e| CoreError::Config(format!("filesync: TLS setup failed: {e}")))?;

                let server = Arc::new(Server::new_with_engine_and_auth(
                    eng,
                    bind,
                    bus,
                    Some(auth_checker),
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
                }));
            }

            "client" => {
                let server_addr = cfg.server_addr.clone().ok_or_else(|| {
                    CoreError::Config("filesync: server_addr required for client mode".into())
                })?;

                let bus = Arc::clone(&ctx.bus);
                let eng = Arc::clone(&engine);
                let tls_client_config = build_client_tls_config();

                let client = Arc::new(Client::new_with_engine(
                    eng,
                    server_addr,
                    bus,
                    cfg.auth_token.clone(),
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
                }));
            }

            other => {
                return Err(CoreError::Config(format!(
                    "filesync: unknown mode {other:?} (expected \"server\" or \"client\")"
                )));
            }
        }

        log::info!(
            "filesync started in {} mode (TLS 1.3), root={:?}",
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
            ("GET", "" | "/" | "/status") => {
                let manifest = state.engine.get_manifest();
                let (file_count, dir_count, total_bytes) = crate::client::count_manifest(&manifest);
                Some(HttpResponse::ok_json(json!({
                    "mode":              state.cfg.mode,
                    "root":              state.cfg.root,
                    "node_id":           state.engine.node_id(),
                    "file_count":        file_count,
                    "dir_count":         dir_count,
                    "total_bytes":       total_bytes,
                    "tls":               "TLS 1.3 / ECDSA P-384",
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
                            "hash":        hex(&m.hash),
                        })
                    })
                    .collect();
                Some(HttpResponse::ok_json(json!({ "files": files })))
            }

            ("POST", "/rescan") => match state.engine.scan() {
                Ok(m) => {
                    let (file_count, dir_count, total_bytes) = crate::client::count_manifest(&m);
                    Some(HttpResponse::ok_json(json!({
                        "ok":         true,
                        "file_count": file_count,
                        "dir_count":  dir_count,
                        "total_bytes": total_bytes,
                    })))
                }
                Err(e) => Some(HttpResponse::internal_error(e.to_string())),
            },

            _ => None,
        }
    }

    fn on_message(&self, _msg: &Arc<BusMessage>) {}
}

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
                "node":        manifest.node_id,
                "mode":        mode,
                "file_count":  file_count,
                "dir_count":   dir_count,
                "total_bytes": total_bytes,
                "total_entries": file_count + dir_count,
                "root":        engine.root(),
            }),
        );
    }
}

fn strong_crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::CryptoProvider {
        cipher_suites: vec![TLS13_AES_256_GCM_SHA384, TLS13_CHACHA20_POLY1305_SHA256],
        ..rustls::crypto::ring::default_provider()
    })
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

pub fn build_server_tls_config() -> Result<Arc<rustls::ServerConfig>, String> {
    let (cert, key) = generate_self_signed_cert()?;

    let config = rustls::ServerConfig::builder_with_provider(strong_crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("protocol version config failed: {e}"))?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key.into())
        .map_err(|e| format!("certificate config failed: {e}"))?;

    Ok(Arc::new(config))
}

pub fn build_client_tls_config() -> Arc<rustls::ClientConfig> {
    let config = rustls::ClientConfig::builder_with_provider(strong_crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3 is always available in rustls with the ring provider")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();

    Arc::new(config)
}

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
