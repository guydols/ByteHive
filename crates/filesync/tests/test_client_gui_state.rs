#![cfg(target_os = "linux")]

//! Integration coverage for `Client::new_standalone(..., Some(gui_state))`.
//!
//! Every other integration test in this crate exercises the client with
//! `gui_state: None` (see `integration_tests.rs`), leaving the GUI-attached
//! code path — used exclusively by the real `filesync-gui` binary — entirely
//! untested. This test fills that gap by driving a real client/server pair
//! over TCP+TLS with a live `SharedState` attached, and asserts that initial
//! sync actually completes (i.e. does not hang) and leaves the GUI snapshot
//! in a sane, non-stuck state with correct file/byte counters.
//!
//! Conflict-propagation into `SyncSnapshot.conflicts` (the "GUI can't show
//! conflicts" bug) is covered separately by whitebox unit tests in
//! `src/client.rs` (`push_gui_conflicts_tests`), since reliably reproducing a
//! two-sided conflict over a real network race is inherently flaky; the unit
//! tests instead directly exercise the exact function that routes
//! `ConflictInfo` into the GUI state.

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use bytehive_core::MessageBus;

use bytehive_filesync::app::build_server_tls_config;
use bytehive_filesync::client::Client;
use bytehive_filesync::exclusions::{ExclusionConfig, Exclusions};
use bytehive_filesync::gui::state::{new_shared_state, ConnectionStatus};
use bytehive_filesync::known_hosts::KnownClients;
use bytehive_filesync::server::Server;
use bytehive_filesync::sync_engine::SyncEngine;
use bytehive_filesync::timestamp_id;

fn tmp_dir(label: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("filesync_gui_{label}_{:x}", timestamp_id()));
    fs::create_dir_all(&d).unwrap();
    d
}

fn no_exclusions() -> Arc<Exclusions> {
    Arc::new(Exclusions::compile(&ExclusionConfig::default()))
}

fn make_engine(root: PathBuf) -> Arc<SyncEngine> {
    let id = format!("test-{:x}", timestamp_id());
    Arc::new(SyncEngine::new(root, id, no_exclusions()))
}

struct TestServer {
    server: Arc<Server>,
    port: u16,
}

impl TestServer {
    fn new(dir: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();

        let bind = format!("127.0.0.1:{port}");
        let engine = make_engine(dir);
        let bus = MessageBus::new();
        let tls_dir = tmp_dir("server_tls");
        let tls = build_server_tls_config(&tls_dir).expect("server TLS config");
        let known_clients = Arc::new(parking_lot::Mutex::new(
            KnownClients::load_from_config_permissive(tls_dir.join("config.toml")),
        ));

        let server = Arc::new(Server::new(engine, bind, bus, known_clients, tls));

        let srv = server.clone();
        thread::Builder::new()
            .name(format!("gui-test-server:{port}"))
            .spawn(move || {
                let _ = srv.run_with_listener(listener);
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok() {
                break;
            }
            if Instant::now() >= deadline {
                panic!("server on port {port} did not become ready within 5 s");
            }
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(200));

        TestServer { server, port }
    }

    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.server.shutdown();
        thread::sleep(Duration::from_millis(100));
    }
}

/// Runs `client.session()` on a background thread, waits (with timeout) for
/// `condition` to become true, then shuts the server down to unblock the
/// client's live-sync loop and joins the thread.
fn run_session(
    client: Client,
    server: &Arc<Server>,
    timeout: Duration,
    condition: impl Fn() -> bool,
) -> bool {
    let handle = thread::Builder::new()
        .name("gui-test-session".into())
        .spawn(move || client.session())
        .unwrap();

    let deadline = Instant::now() + timeout;
    let mut satisfied = false;
    while Instant::now() < deadline {
        if condition() {
            satisfied = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !satisfied {
        satisfied = condition();
    }

    server.shutdown();
    let _ = handle.join();
    satisfied
}

#[test]
fn gui_state_reaches_idle_after_initial_sync_with_no_hang() {
    let srv_dir = tmp_dir("idle_srv");
    let cli_dir = tmp_dir("idle_cli");

    fs::write(srv_dir.join("hello.txt"), b"hello from server").unwrap();

    let srv = TestServer::new(srv_dir.clone());
    let gui_state = new_shared_state();

    let identity_dir = cli_dir.join(".identity");
    let cli_engine = make_engine(cli_dir.clone());
    let client = Client::new_standalone(
        cli_engine,
        srv.addr(),
        identity_dir,
        Some(gui_state.clone()),
    );

    let cli_dir_check = cli_dir.clone();
    let gui_state_check = gui_state.clone();
    let done = run_session(client, &srv.server, Duration::from_secs(10), move || {
        cli_dir_check.join("hello.txt").exists()
            && !matches!(
                gui_state_check.read().status,
                ConnectionStatus::InitialSync | ConnectionStatus::Connecting
            )
    });

    assert!(
        done,
        "initial sync with gui_state attached must complete (this is the reported freeze scenario)"
    );

    let snap = gui_state.read().clone();
    assert_eq!(
        fs::read(cli_dir.join("hello.txt")).unwrap(),
        b"hello from server"
    );
    assert!(
        matches!(
            snap.status,
            ConnectionStatus::Idle | ConnectionStatus::Syncing
        ),
        "expected Idle/Syncing after a successful initial sync, got {:?}",
        snap.status
    );
    assert_eq!(snap.files_received, 1, "expected exactly one file received");
    assert!(snap.bytes_received > 0, "expected non-zero bytes_received");
    assert!(
        snap.conflicts.is_empty(),
        "no conflicts should have been recorded for a plain sync"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}
