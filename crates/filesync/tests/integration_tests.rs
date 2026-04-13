#![cfg(target_os = "linux")]

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytehive_core::MessageBus;

use bytehive_filesync::app::build_server_tls_config;
use bytehive_filesync::client::Client;
use bytehive_filesync::exclusions::{ExclusionConfig, Exclusions};
use bytehive_filesync::known_hosts::KnownClients;
use bytehive_filesync::server::Server;
use bytehive_filesync::sync_engine::SyncEngine;
use bytehive_filesync::timestamp_id;

const MAX_CONCURRENT_SERVERS: usize = 8;
static SEM_COUNT: Mutex<usize> = Mutex::new(MAX_CONCURRENT_SERVERS);
static SEM_CVAR: Condvar = Condvar::new();

struct ServerSlot;

impl ServerSlot {
    fn acquire(label: &str) -> Self {
        tlog(label, "waiting for server slot");
        let mut count = SEM_COUNT.lock().unwrap();
        while *count == 0 {
            count = SEM_CVAR.wait(count).unwrap();
        }
        *count -= 1;
        tlog(
            label,
            format!("acquired server slot ({} remaining)", *count),
        );
        ServerSlot
    }
}

impl Drop for ServerSlot {
    fn drop(&mut self) {
        let mut count = SEM_COUNT.lock().unwrap();
        *count += 1;
        SEM_CVAR.notify_one();
    }
}

fn tlog(label: &str, msg: impl std::fmt::Display) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        % 10_000.0;
    eprintln!("[{secs:08.3}][{label}] {msg}");
}

fn list_dir(dir: &Path) -> String {
    let mut entries: Vec<String> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let rel = e.path().strip_prefix(dir).ok()?;
            if rel.as_os_str().is_empty() {
                None
            } else {
                Some(rel.display().to_string())
            }
        })
        .collect();
    entries.sort();
    if entries.is_empty() {
        "(empty)".to_string()
    } else {
        entries.join(", ")
    }
}

fn files_missing(dir: &Path, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter(|&&n| !dir.join(n).exists())
        .map(|&n| n.to_owned())
        .collect()
}

fn tmp_dir(label: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("filesync_int_{label}_{:x}", timestamp_id()));
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
    pub server: Arc<Server>,
    port: u16,
    pub dir: PathBuf,
    label: String,
    started_at: Instant,
    _slot: ServerSlot,
}

impl TestServer {
    fn new(dir: PathBuf, permissive: bool) -> Self {
        Self::new_with_label(dir, permissive, "server")
    }

    fn new_with_label(dir: PathBuf, permissive: bool, label: &str) -> Self {
        let label = label.to_string();
        let _slot = ServerSlot::acquire(&label);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        tlog(&label, format!("bound to 127.0.0.1:{port}"));

        let bind = format!("127.0.0.1:{port}");
        let engine = make_engine(dir.clone());
        let bus = MessageBus::new();
        let tls_dir = tmp_dir(&format!("{label}_tls"));
        let tls = build_server_tls_config(&tls_dir).expect("server TLS config");
        let known_clients = Arc::new(parking_lot::Mutex::new(if permissive {
            KnownClients::load_from_config_permissive(tls_dir.join("config.toml"))
        } else {
            KnownClients::load_from_config(tls_dir.join("config.toml"))
        }));

        let server = Arc::new(Server::new(engine, bind, bus, known_clients, tls));

        let srv = server.clone();
        let thr_label = label.clone();
        thread::Builder::new()
            .name(format!("test-server:{port}"))
            .spawn(move || {
                tlog(&thr_label, "accept loop starting");
                if let Err(e) = srv.run_with_listener(listener) {
                    let s = e.to_string();
                    if !s.contains("stopped") && !s.contains("reset") {
                        eprintln!("[{thr_label}] server exited unexpectedly: {e}");
                    } else {
                        tlog(&thr_label, format!("accept loop exited: {e}"));
                    }
                }
            })
            .unwrap();

        let started_at = Instant::now();
        let readiness_deadline = started_at + Duration::from_secs(5);
        loop {
            let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok() {
                tlog(
                    &label,
                    format!("server ready after {:.0?}", started_at.elapsed()),
                );
                break;
            }
            if Instant::now() >= readiness_deadline {
                panic!("[{label}] server on port {port} did not become ready within 5 s");
            }
            thread::sleep(Duration::from_millis(10));
        }

        // The readiness-check probe connection is still being processed by
        // handle_client (TLS handshake attempt on the dropped socket).  Give
        // the server a moment to finish that work so the very first real
        // TcpStream::connect() from the test client doesn't race it and get
        // EAGAIN from the kernel – which otherwise adds 2 × SESSION_RETRY_DELAY
        // of dead time to every test.
        thread::sleep(Duration::from_millis(200));

        TestServer {
            server,
            port,
            dir,
            label,
            started_at,
            _slot,
        }
    }

    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        tlog(
            &self.label,
            format!("shutting down (lived {:.0?})", self.started_at.elapsed()),
        );
        self.server.shutdown();
        thread::sleep(Duration::from_millis(100));
    }
}

fn make_client(root: PathBuf, server_addr: &str, _token: Option<&str>) -> Client {
    let identity_dir = root.join(".identity");
    let engine = make_engine(root);
    Client::new_standalone(engine, server_addr.to_string(), identity_dir, None)
}

fn make_client_with_engine(
    engine: Arc<SyncEngine>,
    server_addr: &str,
    _token: Option<&str>,
) -> Client {
    let identity_dir = engine.root().join(".identity");
    Client::new_standalone(engine, server_addr.to_string(), identity_dir, None)
}

const SESSION_MAX_RETRIES: u32 = 20;
const SESSION_RETRY_DELAY: Duration = Duration::from_millis(50);

fn is_transient_connect_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(e.kind(), WouldBlock | TimedOut | ConnectionRefused) || e.raw_os_error() == Some(11)
}

fn session_with_retry(client: &Client, label: &str) -> std::io::Result<()> {
    let mut last_err = None;
    for attempt in 0..=SESSION_MAX_RETRIES {
        if attempt > 0 {
            tlog(
                label,
                format!(
                    "transient connect error on attempt {attempt}: {} — retrying in {:.0?}",
                    last_err.as_ref().unwrap(),
                    SESSION_RETRY_DELAY,
                ),
            );
            thread::sleep(SESSION_RETRY_DELAY);
        }
        match client.session() {
            Ok(()) => return Ok(()),
            Err(e) if is_transient_connect_error(&e) => last_err = Some(e),
            Err(e) => {
                tlog(label, format!("non-retryable session error: {e}"));
                return Err(e);
            }
        }
    }

    tlog(
        label,
        format!(
            "all {SESSION_MAX_RETRIES} retries failed — last error: {}",
            last_err.as_ref().unwrap()
        ),
    );
    Err(last_err.unwrap())
}

fn run_sync(
    client: Client,
    server: &Arc<Server>,
    timeout: Duration,
    condition: impl Fn() -> bool,
) -> bool {
    run_sync_labelled(client, server, timeout, condition, "run_sync", &[])
}

fn run_sync_labelled(
    client: Client,
    server: &Arc<Server>,
    timeout: Duration,
    condition: impl Fn() -> bool,
    label: &str,
    diagnostic_dirs: &[(&str, &Path)],
) -> bool {
    tlog(
        label,
        format!("starting client session (timeout {timeout:.0?})"),
    );
    let session_start = Instant::now();

    let thr_label = label.to_string();
    let handle = thread::Builder::new()
        .name(format!("session:{label}"))
        .spawn(move || session_with_retry(&client, &thr_label))
        .unwrap();

    let deadline = Instant::now() + timeout;
    let mut satisfied = false;
    let mut last_log = Instant::now();

    while Instant::now() < deadline {
        if condition() {
            satisfied = true;
            break;
        }

        if last_log.elapsed() >= Duration::from_secs(2) {
            tlog(
                label,
                format!(
                    "still waiting… {:.0?} elapsed, {:.0?} remaining",
                    session_start.elapsed(),
                    deadline.saturating_duration_since(Instant::now()),
                ),
            );
            for (name, dir) in diagnostic_dirs.iter() {
                tlog(label, format!("  {name}: {}", list_dir(dir)));
            }
            last_log = Instant::now();
        }
        thread::sleep(Duration::from_millis(50));
    }

    if !satisfied {
        satisfied = condition();
    }

    if satisfied {
        tlog(
            label,
            format!("condition satisfied in {:.0?}", session_start.elapsed()),
        );
    } else {
        tlog(
            label,
            format!(
                "TIMEOUT after {:.0?} — condition not satisfied",
                session_start.elapsed()
            ),
        );
        for (name, dir) in diagnostic_dirs.iter() {
            tlog(label, format!("  {name} contents: {}", list_dir(dir)));
        }
    }

    tlog(label, "shutting server down to unblock client");
    server.shutdown();

    match handle.join() {
        Ok(Ok(())) => tlog(label, "session thread exited cleanly"),
        Ok(Err(e)) => tlog(label, format!("session thread exited with error: {e}")),
        Err(_) => tlog(label, "session thread panicked"),
    }

    satisfied
}

fn run_sync_timed(client: Client, server: &Arc<Server>, ms: u64) {
    run_sync(client, server, Duration::from_millis(ms), || false);
}

fn all_files_exist(dir: &Path, names: &[&str]) -> bool {
    names.iter().all(|n| dir.join(n).exists())
}

#[test]
fn sync_empty_both_sides() {
    let srv_dir = tmp_dir("empty_srv");
    let cli_dir = tmp_dir("empty_cli");
    tlog("empty_both_sides", "starting");

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "empty_both_sides");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    run_sync_timed(client, &srv.server, 1_000);

    tlog("empty_both_sides", "done");

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn sync_server_files_to_empty_client() {
    let srv_dir = tmp_dir("s2c_srv");
    let cli_dir = tmp_dir("s2c_cli");

    fs::write(srv_dir.join("hello.txt"), b"hello from server").unwrap();
    fs::write(srv_dir.join("data.bin"), vec![0xABu8; 2048]).unwrap();
    fs::create_dir_all(srv_dir.join("sub")).unwrap();
    fs::write(srv_dir.join("sub/nested.txt"), b"nested file content").unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "s2c");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    let cli = cli_dir.clone();
    let expected = &["hello.txt", "data.bin", "sub/nested.txt"];
    let done = run_sync_labelled(
        client,
        &srv.server,
        Duration::from_secs(10),
        move || all_files_exist(&cli, expected),
        "s2c",
        &[("srv", &srv_dir), ("cli", &cli_dir)],
    );

    assert!(
        done,
        "all server files must arrive on the client; missing: {:?}",
        files_missing(&cli_dir, expected)
    );
    assert_eq!(
        fs::read(cli_dir.join("hello.txt")).unwrap(),
        b"hello from server"
    );
    assert_eq!(
        fs::read(cli_dir.join("data.bin")).unwrap(),
        vec![0xABu8; 2048]
    );
    assert_eq!(
        fs::read(cli_dir.join("sub/nested.txt")).unwrap(),
        b"nested file content"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn sync_client_files_to_empty_server() {
    let srv_dir = tmp_dir("c2s_srv");
    let cli_dir = tmp_dir("c2s_cli");

    fs::write(cli_dir.join("report.txt"), b"uploaded by client").unwrap();
    fs::write(cli_dir.join("image.png"), vec![0xFFu8; 4096]).unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "c2s");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    let srvd = srv_dir.clone();
    let expected = &["report.txt", "image.png"];
    let done = run_sync_labelled(
        client,
        &srv.server,
        Duration::from_secs(10),
        move || all_files_exist(&srvd, expected),
        "c2s",
        &[("srv", &srv_dir), ("cli", &cli_dir)],
    );

    assert!(
        done,
        "all client files must arrive on the server; missing: {:?}",
        files_missing(&srv_dir, expected)
    );
    assert_eq!(
        fs::read(srv_dir.join("report.txt")).unwrap(),
        b"uploaded by client"
    );
    assert_eq!(
        fs::read(srv_dir.join("image.png")).unwrap(),
        vec![0xFFu8; 4096]
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn sync_bidirectional_disjoint_files() {
    let srv_dir = tmp_dir("bidir_srv");
    let cli_dir = tmp_dir("bidir_cli");

    fs::write(srv_dir.join("from_server.txt"), b"server content").unwrap();
    fs::write(cli_dir.join("from_client.txt"), b"client content").unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "bidir");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    let (clid, srvd) = (cli_dir.clone(), srv_dir.clone());
    let done = run_sync_labelled(
        client,
        &srv.server,
        Duration::from_secs(10),
        move || clid.join("from_server.txt").exists() && srvd.join("from_client.txt").exists(),
        "bidir",
        &[("srv", &srv_dir), ("cli", &cli_dir)],
    );

    assert!(
        done,
        "bidirectional transfer must complete; srv has: {}, cli has: {}",
        list_dir(&srv_dir),
        list_dir(&cli_dir)
    );
    assert_eq!(
        fs::read(cli_dir.join("from_server.txt")).unwrap(),
        b"server content"
    );
    assert_eq!(
        fs::read(srv_dir.join("from_client.txt")).unwrap(),
        b"client content"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn transferred_file_passes_blake3_integrity_check() {
    let srv_dir = tmp_dir("hash_srv");
    let cli_dir = tmp_dir("hash_cli");

    let content: Vec<u8> = (0u8..=255u8).cycle().take(37_777).collect();
    fs::write(srv_dir.join("binary.bin"), &content).unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "hash");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        client,
        &srv.server,
        Duration::from_secs(10),
        move || clid.join("binary.bin").exists(),
        "hash",
        &[("srv", &srv_dir), ("cli", &cli_dir)],
    );

    assert!(done, "file must arrive; cli has: {}", list_dir(&cli_dir));
    let received = fs::read(cli_dir.join("binary.bin")).unwrap();
    assert_eq!(received.len(), content.len(), "received size must match");

    let expected: [u8; 32] = blake3::hash(&content).into();
    let actual: [u8; 32] = blake3::hash(&received).into();
    assert_eq!(actual, expected, "BLAKE3 hash must match after transfer");

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn identical_files_on_both_sides_are_not_retransferred() {
    let srv_dir = tmp_dir("dedup_srv");
    let cli_dir = tmp_dir("dedup_cli");

    let content = b"identical on both sides, do not retransfer";
    fs::write(srv_dir.join("shared.txt"), content).unwrap();
    fs::write(cli_dir.join("shared.txt"), content).unwrap();

    let mtime_before = fs::metadata(cli_dir.join("shared.txt"))
        .unwrap()
        .modified()
        .unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "dedup");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    run_sync_timed(client, &srv.server, 1_500);

    let mtime_after = fs::metadata(cli_dir.join("shared.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        "mtime must be unchanged — identical file must not be re-written"
    );
    assert_eq!(
        fs::read(cli_dir.join("shared.txt")).unwrap(),
        content,
        "content must be unchanged"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn nested_directory_tree_transfers_completely() {
    let srv_dir = tmp_dir("tree_srv");
    let cli_dir = tmp_dir("tree_cli");

    fs::create_dir_all(srv_dir.join("a/b/c")).unwrap();
    fs::write(srv_dir.join("a/root.txt"), b"root level").unwrap();
    fs::write(srv_dir.join("a/b/mid.txt"), b"mid level").unwrap();
    fs::write(srv_dir.join("a/b/c/deep.txt"), b"deep level").unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "tree");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    let clid = cli_dir.clone();
    let expected = &["a/root.txt", "a/b/mid.txt", "a/b/c/deep.txt"];
    let done = run_sync_labelled(
        client,
        &srv.server,
        Duration::from_secs(10),
        move || all_files_exist(&clid, expected),
        "tree",
        &[("srv", &srv_dir), ("cli", &cli_dir)],
    );

    assert!(
        done,
        "all nested files must arrive; missing: {:?}",
        files_missing(&cli_dir, expected)
    );
    assert_eq!(fs::read(cli_dir.join("a/root.txt")).unwrap(), b"root level");
    assert_eq!(fs::read(cli_dir.join("a/b/mid.txt")).unwrap(), b"mid level");
    assert_eq!(
        fs::read(cli_dir.join("a/b/c/deep.txt")).unwrap(),
        b"deep level"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn large_file_streamed_over_tls() {
    use bytehive_filesync::protocol::LARGE_FILE_THRESHOLD;

    let srv_dir = tmp_dir("large_srv");
    let cli_dir = tmp_dir("large_cli");

    let size = LARGE_FILE_THRESHOLD as usize + 1;
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    tlog(
        "large_file",
        format!("file size: {size} B ({:.1} MiB)", size as f64 / 1_048_576.0),
    );
    fs::write(srv_dir.join("large.bin"), &content).unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "large_file");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        client,
        &srv.server,
        Duration::from_secs(30),
        move || clid.join("large.bin").exists(),
        "large_file",
        &[("cli", &cli_dir)],
    );

    assert!(
        done,
        "large file must arrive; cli has: {}",
        list_dir(&cli_dir)
    );
    let received = fs::read(cli_dir.join("large.bin")).unwrap();
    assert_eq!(received.len(), size, "received size must match");

    let expected: [u8; 32] = blake3::hash(&content).into();
    let actual: [u8; 32] = blake3::hash(&received).into();
    assert_eq!(
        actual, expected,
        "BLAKE3 hash must match after large-file transfer"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn many_small_files_all_transfer_correctly() {
    let srv_dir = tmp_dir("many_srv");
    let cli_dir = tmp_dir("many_cli");
    const N: usize = 60;

    for i in 0..N {
        fs::write(
            srv_dir.join(format!("f{i:03}.txt")),
            format!("content of file {i}").as_bytes(),
        )
        .unwrap();
    }

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "many_files");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        client,
        &srv.server,
        Duration::from_secs(15),
        move || (0..N).all(|i| clid.join(format!("f{i:03}.txt")).exists()),
        "many_files",
        &[("cli", &cli_dir)],
    );

    if !done {
        let arrived: usize = (0..N)
            .filter(|i| cli_dir.join(format!("f{i:03}.txt")).exists())
            .count();
        panic!("only {arrived}/{N} files arrived before timeout");
    }

    for i in 0..N {
        let got = fs::read(cli_dir.join(format!("f{i:03}.txt"))).unwrap();
        assert_eq!(
            got,
            format!("content of file {i}").as_bytes(),
            "f{i:03}.txt content must match"
        );
    }

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn empty_file_transfers_correctly() {
    let srv_dir = tmp_dir("zero_srv");
    let cli_dir = tmp_dir("zero_cli");

    fs::write(srv_dir.join("empty.txt"), b"").unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "empty_file");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        client,
        &srv.server,
        Duration::from_secs(5),
        move || clid.join("empty.txt").exists(),
        "empty_file",
        &[("cli", &cli_dir)],
    );

    assert!(
        done,
        "empty file must arrive; cli has: {}",
        list_dir(&cli_dir)
    );
    let received = fs::read(cli_dir.join("empty.txt")).unwrap();
    assert!(received.is_empty(), "received file must be empty");

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn mixed_file_types_all_transfer() {
    let srv_dir = tmp_dir("mixed_srv");
    let cli_dir = tmp_dir("mixed_cli");

    fs::write(srv_dir.join("utf8.txt"), "日本語テスト 🌍\n".as_bytes()).unwrap();
    fs::write(
        srv_dir.join("binary.dat"),
        vec![0x00u8, 0x01, 0x7F, 0x80, 0xFE, 0xFF],
    )
    .unwrap();
    fs::write(srv_dir.join("zero.bin"), b"").unwrap();
    fs::create_dir_all(srv_dir.join("deep/path/to")).unwrap();
    fs::write(srv_dir.join("deep/path/to/leaf.txt"), b"deeply nested").unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "mixed");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    let clid = cli_dir.clone();
    let expected = &[
        "utf8.txt",
        "binary.dat",
        "zero.bin",
        "deep/path/to/leaf.txt",
    ];
    let done = run_sync_labelled(
        client,
        &srv.server,
        Duration::from_secs(10),
        move || all_files_exist(&clid, expected),
        "mixed",
        &[("cli", &cli_dir)],
    );

    assert!(
        done,
        "all mixed-type files must arrive; missing: {:?}",
        files_missing(&cli_dir, expected)
    );
    assert_eq!(
        fs::read(cli_dir.join("utf8.txt")).unwrap(),
        "日本語テスト 🌍\n".as_bytes()
    );
    assert_eq!(
        fs::read(cli_dir.join("binary.dat")).unwrap(),
        vec![0x00u8, 0x01, 0x7F, 0x80, 0xFE, 0xFF]
    );
    assert!(fs::read(cli_dir.join("zero.bin")).unwrap().is_empty());
    assert_eq!(
        fs::read(cli_dir.join("deep/path/to/leaf.txt")).unwrap(),
        b"deeply nested"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn engine_manifest_contains_all_received_files() {
    let srv_dir = tmp_dir("manifest_srv");
    let cli_dir = tmp_dir("manifest_cli");

    fs::write(srv_dir.join("alpha.txt"), b"alpha").unwrap();
    fs::write(srv_dir.join("beta.txt"), b"beta").unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "manifest");
    let engine = make_engine(cli_dir.clone());
    let client = make_client_with_engine(engine.clone(), &srv.addr(), None);

    let clid = cli_dir.clone();
    let expected = &["alpha.txt", "beta.txt"];
    let done = run_sync_labelled(
        client,
        &srv.server,
        Duration::from_secs(10),
        move || all_files_exist(&clid, expected),
        "manifest",
        &[("cli", &cli_dir)],
    );

    assert!(
        done,
        "files must arrive before checking manifest; missing: {:?}",
        files_missing(&cli_dir, expected)
    );

    let manifest = engine.get_manifest();
    assert!(
        manifest.files.contains_key(Path::new("alpha.txt")),
        "manifest must contain alpha.txt; manifest has: {:?}",
        manifest.files.keys().collect::<Vec<_>>()
    );
    assert!(
        manifest.files.contains_key(Path::new("beta.txt")),
        "manifest must contain beta.txt; manifest has: {:?}",
        manifest.files.keys().collect::<Vec<_>>()
    );

    let am = manifest.files.get(Path::new("alpha.txt")).unwrap();
    assert_eq!(am.size, 5);
    assert_eq!(am.hash, *blake3::hash(b"alpha").as_bytes());

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn auth_valid_token_is_accepted() {
    let srv_dir = tmp_dir("auth_ok_srv");
    let cli_dir = tmp_dir("auth_ok_cli");
    fs::write(srv_dir.join("secret.txt"), b"secret data").unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "auth_ok");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        client,
        &srv.server,
        Duration::from_secs(10),
        move || clid.join("secret.txt").exists(),
        "auth_ok",
        &[("cli", &cli_dir)],
    );

    assert!(done, "authenticated client must receive file");
    assert_eq!(
        fs::read(cli_dir.join("secret.txt")).unwrap(),
        b"secret data"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn auth_wrong_token_is_rejected() {
    let srv_dir = tmp_dir("auth_bad_srv");
    let cli_dir = tmp_dir("auth_bad_cli");

    let srv = TestServer::new_with_label(srv_dir.clone(), false, "auth_bad");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    tlog(
        "auth_bad",
        "calling session() — expecting rejection (no retry on auth errors)",
    );
    let start = Instant::now();

    let result = session_with_retry(&client, "auth_bad");
    tlog(
        "auth_bad",
        format!(
            "session_with_retry returned in {:.0?}: {result:?}",
            start.elapsed()
        ),
    );
    assert!(
        result.is_err(),
        "session with wrong token must fail, got: {result:?}"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn auth_no_token_rejected_when_required() {
    let srv_dir = tmp_dir("auth_none_srv");
    let cli_dir = tmp_dir("auth_none_cli");

    let srv = TestServer::new_with_label(srv_dir.clone(), false, "auth_none");
    let client = make_client(cli_dir.clone(), &srv.addr(), None);

    tlog(
        "auth_none",
        "calling session() — expecting rejection (no retry on auth errors)",
    );
    let start = Instant::now();
    let result = session_with_retry(&client, "auth_none");
    tlog(
        "auth_none",
        format!(
            "session_with_retry returned in {:.0?}: {result:?}",
            start.elapsed()
        ),
    );
    assert!(
        result.is_err(),
        "session without token must fail when auth is required"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn second_client_receives_files_uploaded_by_first() {
    let srv_dir = tmp_dir("two_cli_srv");
    let cli_a_dir = tmp_dir("two_cli_a");
    let cli_b_dir = tmp_dir("two_cli_b");

    fs::write(cli_a_dir.join("from_a.txt"), b"uploaded by A").unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "two_cli_srv1");

    tlog("two_cli", "starting client A session");
    let client_a = make_client(cli_a_dir.clone(), &srv.addr(), None);
    let srvd = srv_dir.clone();
    let done_a = run_sync_labelled(
        client_a,
        &srv.server,
        Duration::from_secs(10),
        move || srvd.join("from_a.txt").exists(),
        "two_cli_A",
        &[("srv", &srv_dir), ("cli_a", &cli_a_dir)],
    );
    assert!(
        done_a,
        "client A must upload its file to the server; srv has: {}",
        list_dir(&srv_dir)
    );
    tlog("two_cli", "client A upload confirmed");

    tlog("two_cli", "starting srv2 for client B");
    let srv2 = TestServer::new_with_label(srv_dir.clone(), true, "two_cli_srv2");
    let client_b = make_client(cli_b_dir.clone(), &srv2.addr(), None);

    let clid_b = cli_b_dir.clone();
    let done_b = run_sync_labelled(
        client_b,
        &srv2.server,
        Duration::from_secs(10),
        move || clid_b.join("from_a.txt").exists(),
        "two_cli_B",
        &[("cli_b", &cli_b_dir)],
    );
    assert!(
        done_b,
        "client B must receive the file uploaded by client A; cli_b has: {}",
        list_dir(&cli_b_dir)
    );
    assert_eq!(
        fs::read(cli_b_dir.join("from_a.txt")).unwrap(),
        b"uploaded by A"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_a_dir).ok();
    fs::remove_dir_all(&cli_b_dir).ok();
}

#[test]
fn reconnecting_client_does_not_retransfer_synced_files() {
    let srv_dir = tmp_dir("recon_srv");
    let cli_dir = tmp_dir("recon_cli");

    fs::write(srv_dir.join("once.txt"), b"transfer me once").unwrap();

    tlog("recon", "first session starting");
    let srv1 = TestServer::new_with_label(srv_dir.clone(), true, "recon_srv1");
    let engine = make_engine(cli_dir.clone());

    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        make_client_with_engine(engine.clone(), &srv1.addr(), None),
        &srv1.server,
        Duration::from_secs(10),
        move || clid.join("once.txt").exists(),
        "recon_session1",
        &[("cli", &cli_dir)],
    );
    assert!(
        done,
        "first session must deliver the file; cli has: {}",
        list_dir(&cli_dir)
    );

    let mtime_after_first = fs::metadata(cli_dir.join("once.txt"))
        .unwrap()
        .modified()
        .unwrap();
    tlog(
        "recon",
        format!("first session: mtime = {mtime_after_first:?}"),
    );

    thread::sleep(Duration::from_millis(500));

    tlog("recon", "second session starting");
    let srv2 = TestServer::new_with_label(srv_dir.clone(), true, "recon_srv2");
    run_sync_timed(
        make_client_with_engine(engine.clone(), &srv2.addr(), None),
        &srv2.server,
        1_000,
    );

    let mtime_after_second = fs::metadata(cli_dir.join("once.txt"))
        .unwrap()
        .modified()
        .unwrap();
    tlog(
        "recon",
        format!("second session: mtime = {mtime_after_second:?}"),
    );

    assert_eq!(
        mtime_after_first, mtime_after_second,
        "mtime must be unchanged — file must not be re-written on second sync"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn file_created_on_server_after_initial_sync() {
    let srv_dir = tmp_dir("created_srv");
    let cli_dir = tmp_dir("created_cli");

    fs::write(srv_dir.join("sentinel.txt"), b"sentinel").unwrap();

    let srv1 = TestServer::new_with_label(srv_dir.clone(), true, "created_srv1");
    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        make_client(cli_dir.clone(), &srv1.addr(), None),
        &srv1.server,
        Duration::from_secs(10),
        move || clid.join("sentinel.txt").exists(),
        "created_phase1",
        &[("srv", &srv_dir), ("cli", &cli_dir)],
    );
    assert!(
        done,
        "phase 1: sentinel must reach client; cli: {}",
        list_dir(&cli_dir)
    );

    fs::write(srv_dir.join("new_file.txt"), b"created after sync").unwrap();

    let srv2 = TestServer::new_with_label(srv_dir.clone(), true, "created_srv2");
    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        make_client(cli_dir.clone(), &srv2.addr(), None),
        &srv2.server,
        Duration::from_secs(10),
        move || clid.join("new_file.txt").exists(),
        "created_phase2",
        &[("cli", &cli_dir)],
    );
    assert!(
        done,
        "phase 2: new_file.txt must reach client; cli: {}",
        list_dir(&cli_dir)
    );
    assert_eq!(
        fs::read(cli_dir.join("new_file.txt")).unwrap(),
        b"created after sync"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn file_created_on_client_after_initial_sync() {
    let srv_dir = tmp_dir("clicreate_srv");
    let cli_dir = tmp_dir("clicreate_cli");

    fs::write(cli_dir.join("sentinel.txt"), b"sentinel").unwrap();

    let srv1 = TestServer::new_with_label(srv_dir.clone(), true, "clicreate_srv1");
    let srvd = srv_dir.clone();
    let done = run_sync_labelled(
        make_client(cli_dir.clone(), &srv1.addr(), None),
        &srv1.server,
        Duration::from_secs(10),
        move || srvd.join("sentinel.txt").exists(),
        "clicreate_phase1",
        &[("srv", &srv_dir)],
    );
    assert!(
        done,
        "phase 1: sentinel must reach server; srv: {}",
        list_dir(&srv_dir)
    );

    fs::write(cli_dir.join("client_file.txt"), b"from client").unwrap();

    let srv2 = TestServer::new_with_label(srv_dir.clone(), true, "clicreate_srv2");
    let srvd = srv_dir.clone();
    let done = run_sync_labelled(
        make_client(cli_dir.clone(), &srv2.addr(), None),
        &srv2.server,
        Duration::from_secs(10),
        move || srvd.join("client_file.txt").exists(),
        "clicreate_phase2",
        &[("srv", &srv_dir)],
    );
    assert!(
        done,
        "phase 2: client_file.txt must reach server; srv: {}",
        list_dir(&srv_dir)
    );
    assert_eq!(
        fs::read(srv_dir.join("client_file.txt")).unwrap(),
        b"from client"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn directory_with_files_created_on_server_after_sync() {
    let srv_dir = tmp_dir("newdir_srv");
    let cli_dir = tmp_dir("newdir_cli");

    fs::write(srv_dir.join("sentinel.txt"), b"sentinel").unwrap();

    let srv1 = TestServer::new_with_label(srv_dir.clone(), true, "newdir_srv1");
    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        make_client(cli_dir.clone(), &srv1.addr(), None),
        &srv1.server,
        Duration::from_secs(10),
        move || clid.join("sentinel.txt").exists(),
        "newdir_phase1",
        &[("cli", &cli_dir)],
    );
    assert!(done, "phase 1 failed; cli: {}", list_dir(&cli_dir));

    fs::create_dir_all(srv_dir.join("new_dir")).unwrap();
    fs::write(srv_dir.join("new_dir/alpha.txt"), b"alpha").unwrap();
    fs::write(srv_dir.join("new_dir/beta.txt"), b"beta").unwrap();

    let srv2 = TestServer::new_with_label(srv_dir.clone(), true, "newdir_srv2");
    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        make_client(cli_dir.clone(), &srv2.addr(), None),
        &srv2.server,
        Duration::from_secs(10),
        move || clid.join("new_dir/alpha.txt").exists() && clid.join("new_dir/beta.txt").exists(),
        "newdir_phase2",
        &[("cli", &cli_dir)],
    );
    assert!(done, "phase 2 failed; cli: {}", list_dir(&cli_dir));
    assert!(cli_dir.join("new_dir").is_dir());
    assert_eq!(
        fs::read(cli_dir.join("new_dir/alpha.txt")).unwrap(),
        b"alpha"
    );
    assert_eq!(fs::read(cli_dir.join("new_dir/beta.txt")).unwrap(), b"beta");

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn file_updated_on_server_after_initial_sync() {
    let srv_dir = tmp_dir("update_srv");
    let cli_dir = tmp_dir("update_cli");

    fs::write(srv_dir.join("data.txt"), b"original").unwrap();

    let srv1 = TestServer::new_with_label(srv_dir.clone(), true, "update_srv1");
    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        make_client(cli_dir.clone(), &srv1.addr(), None),
        &srv1.server,
        Duration::from_secs(10),
        move || clid.join("data.txt").exists(),
        "update_phase1",
        &[("cli", &cli_dir)],
    );
    assert!(done, "phase 1 failed");
    assert_eq!(fs::read(cli_dir.join("data.txt")).unwrap(), b"original");

    thread::sleep(Duration::from_millis(50));
    fs::write(srv_dir.join("data.txt"), b"updated content").unwrap();

    let srv2 = TestServer::new_with_label(srv_dir.clone(), true, "update_srv2");
    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        make_client(cli_dir.clone(), &srv2.addr(), None),
        &srv2.server,
        Duration::from_secs(10),
        move || {
            fs::read(clid.join("data.txt"))
                .map(|c| c == b"updated content")
                .unwrap_or(false)
        },
        "update_phase2",
        &[("cli", &cli_dir)],
    );
    assert!(done, "phase 2 failed; cli: {}", list_dir(&cli_dir));
    assert_eq!(
        fs::read(cli_dir.join("data.txt")).unwrap(),
        b"updated content"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn file_deleted_on_server_propagates_to_client() {
    let srv_dir = tmp_dir("delete_srv");
    let cli_dir = tmp_dir("delete_cli");

    fs::write(srv_dir.join("keeper.txt"), b"keep").unwrap();
    fs::write(srv_dir.join("victim.txt"), b"delete me").unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "delete_srv");

    let triggered = Arc::new(AtomicBool::new(false));
    let triggered_cond = triggered.clone();
    let srv_dir_cond = srv_dir.clone();
    let cli_dir_cond = cli_dir.clone();

    let done = run_sync_labelled(
        make_client(cli_dir.clone(), &srv.addr(), None),
        &srv.server,
        Duration::from_secs(25),
        move || {
            if triggered_cond.load(Ordering::SeqCst) {
                !cli_dir_cond.join("victim.txt").exists()
                    && cli_dir_cond.join("keeper.txt").exists()
            } else {
                if cli_dir_cond.join("victim.txt").exists()
                    && cli_dir_cond.join("keeper.txt").exists()
                {
                    fs::remove_file(srv_dir_cond.join("victim.txt")).ok();
                    triggered_cond.store(true, Ordering::SeqCst);
                }
                false
            }
        },
        "delete_srv",
        &[("srv", &srv_dir), ("cli", &cli_dir)],
    );
    assert!(
        done,
        "victim.txt must be deleted on client; cli: {}",
        list_dir(&cli_dir)
    );
    assert!(
        cli_dir.join("keeper.txt").exists(),
        "keeper.txt must remain"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn multiple_files_created_on_server_after_sync() {
    let srv_dir = tmp_dir("multi_create_srv");
    let cli_dir = tmp_dir("multi_create_cli");

    fs::write(srv_dir.join("sentinel.txt"), b"sentinel").unwrap();

    let srv1 = TestServer::new_with_label(srv_dir.clone(), true, "multi_create_srv1");
    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        make_client(cli_dir.clone(), &srv1.addr(), None),
        &srv1.server,
        Duration::from_secs(10),
        move || clid.join("sentinel.txt").exists(),
        "multi_create_phase1",
        &[("cli", &cli_dir)],
    );
    assert!(done, "phase 1 failed");

    for i in 0..5u32 {
        fs::write(
            srv_dir.join(format!("batch_{i}.txt")),
            format!("batch file {i}").as_bytes(),
        )
        .unwrap();
    }

    let srv2 = TestServer::new_with_label(srv_dir.clone(), true, "multi_create_srv2");
    let clid = cli_dir.clone();
    let done = run_sync_labelled(
        make_client(cli_dir.clone(), &srv2.addr(), None),
        &srv2.server,
        Duration::from_secs(15),
        move || (0..5u32).all(|i| clid.join(format!("batch_{i}.txt")).exists()),
        "multi_create_phase2",
        &[("cli", &cli_dir)],
    );
    assert!(done, "phase 2 failed; cli: {}", list_dir(&cli_dir));
    for i in 0..5u32 {
        assert_eq!(
            fs::read(cli_dir.join(format!("batch_{i}.txt"))).unwrap(),
            format!("batch file {i}").as_bytes()
        );
    }

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}

#[test]
fn file_renamed_on_server_propagates_to_client() {
    let srv_dir = tmp_dir("rename_srv");
    let cli_dir = tmp_dir("rename_cli");

    fs::write(srv_dir.join("old_name.txt"), b"rename me").unwrap();

    let srv = TestServer::new_with_label(srv_dir.clone(), true, "rename_srv");

    let triggered = Arc::new(AtomicBool::new(false));
    let triggered_cond = triggered.clone();
    let srv_dir_cond = srv_dir.clone();
    let cli_dir_cond = cli_dir.clone();

    let done = run_sync_labelled(
        make_client(cli_dir.clone(), &srv.addr(), None),
        &srv.server,
        Duration::from_secs(25),
        move || {
            if triggered_cond.load(Ordering::SeqCst) {
                cli_dir_cond.join("new_name.txt").exists()
            } else {
                if cli_dir_cond.join("old_name.txt").exists() {
                    fs::rename(
                        srv_dir_cond.join("old_name.txt"),
                        srv_dir_cond.join("new_name.txt"),
                    )
                    .ok();
                    triggered_cond.store(true, Ordering::SeqCst);
                }
                false
            }
        },
        "rename_srv",
        &[("srv", &srv_dir), ("cli", &cli_dir)],
    );
    assert!(
        done,
        "new_name.txt must reach client; cli: {}",
        list_dir(&cli_dir)
    );
    assert_eq!(
        fs::read(cli_dir.join("new_name.txt")).unwrap(),
        b"rename me"
    );

    fs::remove_dir_all(&srv_dir).ok();
    fs::remove_dir_all(&cli_dir).ok();
}
