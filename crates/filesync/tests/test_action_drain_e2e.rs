#![cfg(target_os = "linux")]

//! End-to-end drain over real TLS sockets: actions journalled while no
//! session was running are delivered when the client connects.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use bytehive_core::MessageBus;
use bytehive_filesync::app::build_server_tls_config;
use bytehive_filesync::client::Client;
use bytehive_filesync::exclusions::{ExclusionConfig, Exclusions};
use bytehive_filesync::known_hosts::KnownClients;
use bytehive_filesync::ledger::now_ms;
use bytehive_filesync::server::Server;
use bytehive_filesync::sync_engine::SyncEngine;
use bytehive_filesync::timestamp_id;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "bh_e2e_{name}_{:x}",
        timestamp_id()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn engine(root: &Path, node: &str) -> Arc<SyncEngine> {
    let ex = Arc::new(Exclusions::compile(&ExclusionConfig::default()));
    Arc::new(SyncEngine::new(root.to_path_buf(), node.to_string(), ex))
}

fn start_server(engine: Arc<SyncEngine>) -> (Arc<Server>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{port}");
    let tls_dir = tmp("srv_tls");
    let tls = build_server_tls_config(&tls_dir).unwrap();
    let known = Arc::new(parking_lot::Mutex::new(
        KnownClients::load_from_config_permissive(tls_dir.join("config.toml")),
    ));
    let server = Arc::new(Server::new(engine, addr.clone(), MessageBus::new(), known, tls));
    let srv = server.clone();
    thread::spawn(move || {
        let _ = srv.run_with_listener(listener);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(&addr).is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(200));
    (server, addr)
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn offline_delete_reaches_server_on_connect() {
    let srv_dir = tmp("srv");
    let cli_dir = tmp("cli");
    let srv_engine = engine(&srv_dir, "srv");
    let cli_engine = engine(&cli_dir, "cli");

    // Both sides hold the same file from an earlier sync.
    let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    for root in [&srv_dir, &cli_dir] {
        std::fs::write(root.join("f.txt"), b"shared").unwrap();
        filetime::set_file_mtime(root.join("f.txt"), filetime::FileTime::from(old)).unwrap();
    }
    srv_engine.scan().unwrap();
    cli_engine.scan().unwrap();

    // The client deletes the file while no session runs: the capture records it.
    let deleted_at = now_ms();
    std::fs::remove_file(cli_dir.join("f.txt")).unwrap();
    cli_engine.journal().delete(PathBuf::from("f.txt"), deleted_at).unwrap();

    let (server, addr) = start_server(srv_engine.clone());
    let client = Arc::new(Client::new_standalone(
        cli_engine.clone(),
        addr,
        cli_dir.join(".identity"),
        None,
    ));
    let cli = client.clone();
    let session = thread::spawn(move || {
        let _ = cli.session();
    });

    wait_until("server applies the offline delete", || {
        !srv_dir.join("f.txt").exists()
    });
    wait_until("client journal drained", || cli_engine.journal().is_empty());
    let tomb = srv_engine
        .ledger_snapshot()
        .get(Path::new("f.txt"))
        .map(|t| t.deleted_at_ms);
    assert_eq!(tomb, Some(deleted_at), "server records the original delete stamp");

    client.shutdown();
    server.shutdown();
    let _ = session.join();
}

fn set_old_mtime(path: &Path) {
    let old = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    filetime::set_file_mtime(path, filetime::FileTime::from(old)).unwrap();
}

fn spawn_client(cli_engine: Arc<SyncEngine>, addr: String, cli_dir: &Path) -> (Arc<Client>, thread::JoinHandle<()>) {
    let client = Arc::new(Client::new_standalone(
        cli_engine,
        addr,
        cli_dir.join(".identity"),
        None,
    ));
    let cli = client.clone();
    let handle = thread::spawn(move || {
        let _ = cli.session();
    });
    (client, handle)
}

#[test]
fn connected_client_old_mtime_copy_into_deleted_path_reaches_server() {
    // The reported bug: `cp -p` (old mtime) into a path deleted earlier is
    // vetoed by the tombstone. A live action must win over it.
    let srv_dir = tmp("live_cli_srv");
    let cli_dir = tmp("live_cli_cli");
    let srv_engine = engine(&srv_dir, "srv");
    let cli_engine = engine(&cli_dir, "cli");
    let (server, addr) = start_server(srv_engine.clone());
    let (client, session) = spawn_client(cli_engine.clone(), addr, &cli_dir);

    std::fs::write(cli_dir.join("f.txt"), b"v1").unwrap();
    wait_until("server receives v1", || srv_dir.join("f.txt").exists());

    std::fs::remove_file(cli_dir.join("f.txt")).unwrap();
    wait_until("server applies the delete", || !srv_dir.join("f.txt").exists());

    std::fs::write(cli_dir.join("f.txt"), b"v2 copied with cp -p").unwrap();
    set_old_mtime(&cli_dir.join("f.txt"));
    wait_until("server receives the old-mtime copy", || {
        std::fs::read(srv_dir.join("f.txt")).map(|b| b == b"v2 copied with cp -p").unwrap_or(false)
    });

    client.shutdown();
    server.shutdown();
    let _ = session.join();
}

#[test]
fn connected_server_old_mtime_copy_reaches_client() {
    // Server-local live action: relayed with a live stamp, not its old mtime.
    let srv_dir = tmp("live_srv_srv");
    let cli_dir = tmp("live_srv_cli");
    let srv_engine = engine(&srv_dir, "srv");
    let cli_engine = engine(&cli_dir, "cli");
    let (server, addr) = start_server(srv_engine.clone());
    let (client, session) = spawn_client(cli_engine.clone(), addr, &cli_dir);

    std::fs::write(srv_dir.join("f.txt"), b"v1").unwrap();
    wait_until("client receives v1", || cli_dir.join("f.txt").exists());

    std::fs::remove_file(srv_dir.join("f.txt")).unwrap();
    wait_until("client applies the delete", || !cli_dir.join("f.txt").exists());

    std::fs::write(srv_dir.join("f.txt"), b"v2 copied on server").unwrap();
    set_old_mtime(&srv_dir.join("f.txt"));
    wait_until("client receives the old-mtime server copy", || {
        std::fs::read(cli_dir.join("f.txt")).map(|b| b == b"v2 copied on server").unwrap_or(false)
    });

    client.shutdown();
    server.shutdown();
    let _ = session.join();
}
