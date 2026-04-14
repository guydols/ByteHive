mod data_store;
mod dhat;
mod harness;
mod integrity;
mod metrics;
mod report;
mod types;
mod workload;

use metrics::ProcessMetricsCollector;
use report::BenchmarkReport;
use types::{Event, EventKind, WorkloadStats};

use clap::Parser;
use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ─── CLI (orchestrator mode) ────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "stress-bench",
    about = "ByteHive FileSync stress benchmark — pushes file synchronisation to the limit and verifies integrity"
)]
struct Cli {
    /// Total benchmark duration in minutes (sustained load phase fills remaining time)
    #[arg(short, long, default_value = "30")]
    duration: u64,

    /// Log level for the server and client subprocesses.
    /// Accepted values: error, warn, info, debug, trace.
    /// Logs at this level and above are captured live to the terminal and
    /// embedded in the HTML report's Log Stream section.
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Scale factor for workload sizes (1.0 = default, 2.0 = double everything)
    #[arg(short, long, default_value = "1.0")]
    scale: f64,

    /// Output directory for the HTML report
    #[arg(short, long, default_value = "./bench_output")]
    output: PathBuf,

    /// Number of small files (1KB-100KB) in the flood phase
    #[arg(long, default_value = "5000")]
    small_files: usize,

    /// Number of large files to transfer
    #[arg(long, default_value = "5")]
    large_files: usize,

    /// Target size of each large file in MB
    #[arg(long, default_value = "100")]
    large_file_size: usize,

    /// Number of files for the mixed-burst small file portion
    #[arg(long, default_value = "2000")]
    mixed_small: usize,

    /// Number of files for the mixed-burst large file portion
    #[arg(long, default_value = "3")]
    mixed_large: usize,

    /// Number of files to modify in the modification storm
    #[arg(long, default_value = "1000")]
    modify_count: usize,

    /// Number of files to delete in the delete-and-recreate phase
    #[arg(long, default_value = "500")]
    delete_count: usize,

    /// Number of files to recreate after deletion
    #[arg(long, default_value = "500")]
    recreate_count: usize,

    /// Seconds between sustained-load ticks
    #[arg(long, default_value = "3")]
    sustained_tick_interval: u64,

    /// Sync wait timeout per phase in seconds
    #[arg(long, default_value = "300")]
    sync_timeout: u64,

    /// Keep temp directories after benchmark (useful for debugging)
    #[arg(long, default_value = "false")]
    keep_dirs: bool,

    /// Enable DHAT heap profiling via `valgrind --tool=dhat`.
    /// Requires `valgrind` to be installed.
    /// Produces per-allocation-site peak-live-bytes data embedded in the HTML report.
    /// NOTE: DHAT adds ~20–50× overhead — use `--duration 1` for profiling runs.
    #[arg(long)]
    dhat: bool,
}

// ─── helpers ────────────────────────────────────────────────────────────────

/// Converts the current wall-clock time into a `YYYY-MM-DD_HH-MM-SS` string
/// for use as a unique run-directory name, without any extra dependencies.
fn bench_run_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let sec = secs % 60;
    let min = (secs / 60) % 60;
    let hour = (secs / 3600) % 24;
    let mut days = secs / 86400;

    // Days since 1970-01-01 → Gregorian year / month / day
    let mut year = 1970u64;
    loop {
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let diy = if leap { 366 } else { 365 };
        if days < diy {
            break;
        }
        days -= diy;
        year += 1;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days: [u64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0usize;
    for &md in &month_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }
    let day = days + 1;

    format!(
        "{year:04}-{:02}-{day:02}_{hour:02}-{min:02}-{sec:02}",
        month + 1
    )
}

fn scaled(n: usize, scale: f64) -> usize {
    ((n as f64) * scale).max(1.0) as usize
}

fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn banner(msg: &str) {
    let bar = "═".repeat(60);
    eprintln!("\n╔{bar}╗");
    eprintln!("║ {msg:^58} ║");
    eprintln!("╚{bar}╝\n");
}

fn phase_banner(name: &str) {
    eprintln!("┌──────────────────────────────────────────────────────────────");
    eprintln!("│  Phase: {name}");
    eprintln!("└──────────────────────────────────────────────────────────────");
}

fn record_workload_event(acc: &mut DataAccumulator, start: &Instant, stats: &WorkloadStats) {
    let elapsed = start.elapsed().as_secs_f64();
    if stats.files_created > 0 {
        acc.push_event(Event {
            elapsed_secs: elapsed,
            kind: EventKind::FilesCreated {
                count: stats.files_created,
                total_bytes: stats.bytes_written,
            },
        });
    }
    if stats.files_modified > 0 {
        acc.push_event(Event {
            elapsed_secs: elapsed,
            kind: EventKind::FilesModified {
                count: stats.files_modified,
                total_bytes: stats.bytes_written,
            },
        });
    }
    if stats.files_deleted > 0 {
        acc.push_event(Event {
            elapsed_secs: elapsed,
            kind: EventKind::FilesDeleted {
                count: stats.files_deleted,
            },
        });
    }
}

// ─── DataAccumulator ────────────────────────────────────────────────────────

/// Collects benchmark events and integrity results while simultaneously
/// appending each record to the crash-safe `data.ndjson` store.
struct DataAccumulator {
    events: Vec<Event>,
    integrity_results: Vec<(String, types::IntegrityResult)>,
    data_store: std::sync::Arc<data_store::DataStore>,
}

impl DataAccumulator {
    fn new(data_store: std::sync::Arc<data_store::DataStore>) -> Self {
        Self {
            events: Vec::new(),
            integrity_results: Vec::new(),
            data_store,
        }
    }

    fn push_event(&mut self, event: Event) {
        self.data_store.append(&data_store::DataRecord::Event {
            event: event.clone(),
        });
        self.events.push(event);
    }

    fn push_integrity(&mut self, phase: String, result: types::IntegrityResult) {
        self.data_store.append(&data_store::DataRecord::Integrity {
            phase: phase.clone(),
            result: result.clone(),
        });
        self.integrity_results.push((phase, result));
    }
}

fn wait_and_verify(
    phase_name: &str,
    start: &Instant,
    acc: &mut DataAccumulator,
    server_dir: &PathBuf,
    client_dir: &PathBuf,
    sync_timeout: Duration,
) {
    eprintln!("  ⏳ Waiting for sync to propagate…");
    let wait_start = Instant::now();
    let synced = integrity::wait_for_sync(server_dir, client_dir, sync_timeout);
    let wait_elapsed = wait_start.elapsed().as_secs_f64();

    acc.push_event(Event {
        elapsed_secs: start.elapsed().as_secs_f64(),
        kind: EventKind::SyncWaitComplete {
            duration_secs: wait_elapsed,
        },
    });

    if synced {
        eprintln!("  ✅ Sync completed in {wait_elapsed:.1}s — verifying integrity…");
    } else {
        eprintln!("  ⚠️  Sync timeout after {wait_elapsed:.1}s — verifying what we have…");
    }

    let result = integrity::check_integrity(server_dir, client_dir);
    let passed = result.passed();

    acc.push_event(Event {
        elapsed_secs: start.elapsed().as_secs_f64(),
        kind: EventKind::IntegrityCheck {
            phase: phase_name.to_string(),
            passed,
            matched: result.matched,
            mismatched: result.mismatched.len(),
            missing: result.missing_from_dest.len(),
            extra: result.extra_in_dest.len(),
        },
    });

    if passed {
        eprintln!("  ✅ Integrity OK — {} files matched", result.matched);
    } else {
        eprintln!(
            "  ❌ Integrity FAILED — {} matched, {} mismatched, {} missing, {} extra",
            result.matched,
            result.mismatched.len(),
            result.missing_from_dest.len(),
            result.extra_in_dest.len()
        );
        for p in result.mismatched.iter().take(5) {
            eprintln!("     MISMATCH: {}", p.display());
        }
        for p in result.missing_from_dest.iter().take(5) {
            eprintln!("     MISSING:  {}", p.display());
        }
    }

    acc.push_integrity(phase_name.to_string(), result);
}

// ─── Subprocess: __server ───────────────────────────────────────────────────
//
// Usage: stress-bench __server <dir> <port-file>
//
// Starts the filesync server, writes the bound port to <port-file>, and blocks
// until stdin is closed (EOF).  This allows the orchestrator to signal shutdown
// by dropping the stdin pipe handle.

fn run_server_subprocess(args: &[String]) {
    if args.len() < 2 {
        eprintln!("usage: stress-bench __server <dir> <port-file>");
        std::process::exit(1);
    }

    let dir = PathBuf::from(&args[0]);
    let port_file = PathBuf::from(&args[1]);

    fs::create_dir_all(&dir).expect("create server dir");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
    listener.set_nonblocking(true).expect("set nonblocking");
    let port = listener.local_addr().expect("local addr").port();

    // Write the port so the orchestrator can read it.
    fs::write(&port_file, port.to_string()).expect("write port file");

    eprintln!(
        "[server-subprocess] pid={} listening on 127.0.0.1:{port}",
        std::process::id()
    );

    // Build the server using the library types.
    use bytehive_core::MessageBus;
    use bytehive_filesync::app::build_server_tls_config;
    use bytehive_filesync::exclusions::{ExclusionConfig, Exclusions};
    use bytehive_filesync::known_hosts::KnownClients;
    use bytehive_filesync::server::Server;
    use bytehive_filesync::sync_engine::SyncEngine;
    use bytehive_filesync::timestamp_id;
    use parking_lot::Mutex;

    let id = format!("bench-srv-{:x}", timestamp_id());
    let exclusions = Arc::new(Exclusions::compile(&ExclusionConfig::default()));
    let engine = Arc::new(SyncEngine::new(dir, id, exclusions));
    let bus = MessageBus::new();
    // Stress bench uses a temp dir for the server identity cert and an
    // in-memory known_clients that auto-approves everyone.
    let bench_state_dir = std::env::temp_dir().join(format!("bh_bench_srv_{port}"));
    std::fs::create_dir_all(&bench_state_dir).expect("create bench state dir");
    let tls = build_server_tls_config(&bench_state_dir).expect("server TLS config");
    let known_clients = Arc::new(Mutex::new(KnownClients::load_from_config_permissive(
        bench_state_dir.join("config.toml"),
    )));
    let server = Arc::new(Server::new(
        engine,
        format!("127.0.0.1:{port}"),
        bus,
        known_clients,
        tls,
    ));

    let srv = server.clone();
    let handle = std::thread::Builder::new()
        .name("server-main".into())
        .spawn(move || {
            if let Err(e) = srv.run_with_listener(listener) {
                let msg = e.to_string();
                if !msg.contains("stopped") && !msg.contains("reset") {
                    eprintln!("[server-subprocess] unexpected exit: {e}");
                }
            }
        })
        .expect("spawn server thread");

    // Block on stdin — when the orchestrator drops our stdin pipe, we get EOF.
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 64];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break, // EOF — orchestrator wants us to shut down
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    eprintln!("[server-subprocess] stdin closed, shutting down");
    server.shutdown();
    let _ = handle.join();
    eprintln!("[server-subprocess] exited cleanly");
}

// ─── Subprocess: __client ───────────────────────────────────────────────────
//
// Usage: stress-bench __client <dir> <server-addr>

fn run_client_subprocess(args: &[String]) {
    if args.len() < 2 {
        eprintln!("usage: stress-bench __client <dir> <server-addr>");
        std::process::exit(1);
    }

    let dir = PathBuf::from(&args[0]);
    let server_addr = args[1].clone();

    fs::create_dir_all(&dir).expect("create client dir");

    eprintln!(
        "[client-subprocess] pid={} connecting to {server_addr}",
        std::process::id()
    );

    use bytehive_filesync::client::Client;
    use bytehive_filesync::exclusions::{ExclusionConfig, Exclusions};
    use bytehive_filesync::sync_engine::SyncEngine;
    use bytehive_filesync::timestamp_id;

    let id = format!("bench-cli-{:x}", timestamp_id());
    let exclusions = Arc::new(Exclusions::compile(&ExclusionConfig::default()));
    let engine = Arc::new(SyncEngine::new(dir, id, exclusions));
    // Stress bench clients use the GUI config dir as their identity directory
    // so they get a stable cert across runs.
    let identity_dir = bytehive_filesync::gui::config::GuiConfig::config_dir().join("filesync");
    let client = Arc::new(Client::new_standalone(
        engine,
        server_addr,
        identity_dir,
        None,
    ));

    let cli = client.clone();
    let _handle = std::thread::Builder::new()
        .name("client-main".into())
        .spawn(move || {
            cli.run();
        })
        .expect("spawn client thread");

    // Block on stdin — EOF signals shutdown.
    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 64];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    eprintln!("[client-subprocess] stdin closed, shutting down");
    client.shutdown();
    // The client thread may be blocked in recv_loop waiting on a TLS read.
    // Give it a brief moment to exit, then force-exit the process so we
    // don't hang waiting for the server to close the connection.
    std::thread::sleep(Duration::from_millis(500));
    eprintln!("[client-subprocess] exiting");
    std::process::exit(0);
}

// ─── main ───────────────────────────────────────────────────────────────────

fn init_subprocess_logger() {
    // Use a compact format whose first token is always the level word so the
    // orchestrator's log-reader thread can parse it reliably:
    //   INFO  [bytehive_filesync::client] filesync session: TLS handshake complete
    // The level comes from RUST_LOG, which the orchestrator sets before spawning.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            use std::io::Write;
            writeln!(
                buf,
                "{:<5} [{}] {}",
                record.level(),
                record.target(),
                record.args()
            )
        })
        .init();
}

fn main() {
    // ── Subprocess dispatch MUST come before env_logger::init() ─────────
    // Each subprocess configures its own logger (custom compact format +
    // level from the RUST_LOG env var the orchestrator sets).
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.len() > 1 && raw_args[1] == "__server" {
        init_subprocess_logger();
        run_server_subprocess(&raw_args[2..]);
        return;
    }
    if raw_args.len() > 1 && raw_args[1] == "__client" {
        init_subprocess_logger();
        run_client_subprocess(&raw_args[2..]);
        return;
    }
    if raw_args.len() > 1 && raw_args[1] == "__report" {
        if raw_args.len() < 3 {
            eprintln!("usage: stress-bench __report <data.ndjson> [output.html]");
            std::process::exit(1);
        }
        let data_path = PathBuf::from(&raw_args[2]);
        let output_path = if raw_args.len() >= 4 {
            PathBuf::from(&raw_args[3])
        } else {
            data_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("report.html")
        };
        eprintln!("📂 Loading data from: {}", data_path.display());
        let loaded = match data_store::load(&data_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("❌ Failed to load data file: {e}");
                std::process::exit(1);
            }
        };
        if !loaded.completed {
            eprintln!(
                "⚠️  This run did not finish cleanly — report shows data up to the crash point."
            );
        }
        let report = BenchmarkReport::from_loaded_data(loaded);
        eprintln!("📊 Generating HTML report…");
        match report.generate_html(&output_path) {
            Ok(()) => eprintln!("✅ Report written to: {}", output_path.display()),
            Err(e) => {
                eprintln!("❌ Failed to write report: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // ── Normal orchestrator mode ────────────────────────────────────────
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();
    let bench_start = Instant::now();
    let total_budget = Duration::from_secs(cli.duration * 60);
    let sync_timeout = Duration::from_secs(cli.sync_timeout);
    let scale = cli.scale;

    // ── output / run directory ───────────────────────────────────────────
    // Every invocation gets its own timestamped sub-directory so results from
    // multiple runs never overwrite each other.
    let run_id = bench_run_id();
    let run_dir = cli.output.join(&run_id);
    fs::create_dir_all(&run_dir).expect("create run output dir");

    // Persist the exact command line so the run is reproducible.
    let command_line = raw_args.join(" ");
    fs::write(run_dir.join("command.txt"), &command_line).expect("write command.txt");

    // ── crash-safe data store ────────────────────────────────────────────────
    let data_store =
        data_store::DataStore::create(&run_dir.join("data.ndjson")).expect("create data store");
    data_store.append(&data_store::DataRecord::Meta {
        run_id: run_id.clone(),
        command: command_line.clone(),
    });
    let mut acc = DataAccumulator::new(data_store.clone());

    // ── temp directories ────────────────────────────────────────────────
    let base = std::env::temp_dir().join(format!("filesync_bench_{}", std::process::id()));
    let server_dir = base.join("server");
    let client_dir = base.join("client");
    fs::create_dir_all(&server_dir).expect("create server dir");
    fs::create_dir_all(&client_dir).expect("create client dir");

    // ── DHAT output paths ────────────────────────────────────────────────
    let (server_dhat_path, client_dhat_path): (Option<PathBuf>, Option<PathBuf>) = if cli.dhat {
        let dhat_dir = run_dir.join("dhat");
        fs::create_dir_all(&dhat_dir).expect("create dhat output dir");
        (
            Some(dhat_dir.join("server.json")),
            Some(dhat_dir.join("client.json")),
        )
    } else {
        (None, None)
    };

    banner("ByteHive FileSync — Stress Benchmark");
    eprintln!("  Architecture : separate server + client processes");
    eprintln!("  Server dir   : {}", server_dir.display());
    eprintln!("  Client dir   : {}", client_dir.display());
    eprintln!("  Output       : {}", run_dir.display());
    eprintln!("  Run ID       : {run_id}");
    eprintln!("  Duration     : {} min", cli.duration);
    eprintln!("  Scale        : {:.1}x", scale);
    eprintln!("  Log level    : {}", cli.log_level);
    eprintln!("  Small files  : {}", scaled(cli.small_files, scale));
    eprintln!(
        "  Large files  : {} × {} MB",
        scaled(cli.large_files, scale),
        cli.large_file_size
    );
    if cli.dhat {
        eprintln!(
            "  DHAT         : enabled (output in {}/dhat/) — ~20-50× slower",
            run_dir.display()
        );
    }
    eprintln!();

    // ── start server & client as subprocesses ───────────────────────────
    eprintln!("🚀 Starting server subprocess…");
    let mut server = harness::BenchServer::start(
        server_dir.clone(),
        server_dhat_path,
        bench_start,
        &cli.log_level,
        data_store.clone(),
    );
    let addr = server.addr();
    eprintln!("   Server PID: {}, listening on {addr}", server.pid());

    eprintln!("🚀 Starting client subprocess…");
    let mut client = harness::BenchClient::start(
        client_dir.clone(),
        addr,
        client_dhat_path,
        bench_start,
        &cli.log_level,
        data_store.clone(),
    );
    eprintln!("   Client PID: {}", client.pid());

    // ── start per-process metrics collector ──────────────────────────────
    // Start AFTER subprocesses are up so the first sample captures real data.
    let metrics_handle = ProcessMetricsCollector::new(Duration::from_secs(1)).start(
        server.pid(),
        client.pid(),
        bench_start,
        data_store.clone(),
    );

    acc.push_event(Event {
        elapsed_secs: bench_start.elapsed().as_secs_f64(),
        kind: EventKind::Info(format!(
            "Server PID={}, Client PID={} — metrics collector started",
            server.pid(),
            client.pid()
        )),
    });

    eprintln!("🔗 Waiting for initial connection…");
    if !harness::wait_for_connection(&server_dir, &client_dir, Duration::from_secs(30)) {
        eprintln!("💀 Could not establish initial connection — aborting.");
        let _ = client.shutdown_and_collect_logs();
        let _ = server.shutdown_and_collect_logs();
        let _ = fs::remove_dir_all(&base);
        std::process::exit(1);
    }
    acc.push_event(Event {
        elapsed_secs: bench_start.elapsed().as_secs_f64(),
        kind: EventKind::Info("Initial connection established".into()),
    });
    eprintln!(
        "✅ Connected in {:.1}s\n",
        bench_start.elapsed().as_secs_f64()
    );

    // Give the inotify watcher a moment to fully initialise
    std::thread::sleep(Duration::from_secs(1));

    // Pre-create all phase directories so the watcher is already tracking them
    // before any files land.
    eprintln!("📁 Pre-creating phase directories…");
    workload::prepare_directories(&server_dir);
    eprintln!("  Done — watcher should be tracking all sub-directories.\n");

    // ── Phase 1: Small File Flood ───────────────────────────────────────
    if bench_start.elapsed() < total_budget {
        let phase = "small_file_flood";
        phase_banner(phase);
        acc.push_event(Event {
            elapsed_secs: bench_start.elapsed().as_secs_f64(),
            kind: EventKind::PhaseStart(phase.into()),
        });

        let count = scaled(cli.small_files, scale);
        eprintln!("  Creating {count} small files (1KB–100KB)…");
        let t = Instant::now();
        let stats = workload::small_file_flood(&server_dir, count);
        eprintln!(
            "  Done in {:.1}s — {} files, {}",
            t.elapsed().as_secs_f64(),
            stats.files_created,
            human_bytes(stats.bytes_written),
        );
        record_workload_event(&mut acc, &bench_start, &stats);

        wait_and_verify(
            phase,
            &bench_start,
            &mut acc,
            &server_dir,
            &client_dir,
            sync_timeout,
        );

        acc.push_event(Event {
            elapsed_secs: bench_start.elapsed().as_secs_f64(),
            kind: EventKind::PhaseEnd(phase.into()),
        });
    }

    // ── Phase 2: Large File Transfer ────────────────────────────────────
    if bench_start.elapsed() < total_budget {
        let phase = "large_file_transfer";
        phase_banner(phase);
        acc.push_event(Event {
            elapsed_secs: bench_start.elapsed().as_secs_f64(),
            kind: EventKind::PhaseStart(phase.into()),
        });

        let count = scaled(cli.large_files, scale);
        eprintln!(
            "  Creating {count} large files (~{} MB each)…",
            cli.large_file_size
        );
        let t = Instant::now();
        let stats = workload::large_file_transfer(&server_dir, count, cli.large_file_size);
        eprintln!(
            "  Done in {:.1}s — {} files, {}",
            t.elapsed().as_secs_f64(),
            stats.files_created,
            human_bytes(stats.bytes_written),
        );
        record_workload_event(&mut acc, &bench_start, &stats);

        wait_and_verify(
            phase,
            &bench_start,
            &mut acc,
            &server_dir,
            &client_dir,
            sync_timeout,
        );

        acc.push_event(Event {
            elapsed_secs: bench_start.elapsed().as_secs_f64(),
            kind: EventKind::PhaseEnd(phase.into()),
        });
    }

    // ── Phase 3: Mixed Burst ────────────────────────────────────────────
    if bench_start.elapsed() < total_budget {
        let phase = "mixed_burst";
        phase_banner(phase);
        acc.push_event(Event {
            elapsed_secs: bench_start.elapsed().as_secs_f64(),
            kind: EventKind::PhaseStart(phase.into()),
        });

        let small_n = scaled(cli.mixed_small, scale);
        let large_n = scaled(cli.mixed_large, scale);
        eprintln!(
            "  Mixed burst: {small_n} small + {large_n} large (~{} MB)…",
            cli.large_file_size
        );
        let t = Instant::now();
        let stats = workload::mixed_burst(&server_dir, small_n, large_n, cli.large_file_size);
        eprintln!(
            "  Done in {:.1}s — {} files, {}",
            t.elapsed().as_secs_f64(),
            stats.files_created,
            human_bytes(stats.bytes_written),
        );
        record_workload_event(&mut acc, &bench_start, &stats);

        wait_and_verify(
            phase,
            &bench_start,
            &mut acc,
            &server_dir,
            &client_dir,
            sync_timeout,
        );

        acc.push_event(Event {
            elapsed_secs: bench_start.elapsed().as_secs_f64(),
            kind: EventKind::PhaseEnd(phase.into()),
        });
    }

    // ── Phase 4: Modification Storm ─────────────────────────────────────
    if bench_start.elapsed() < total_budget {
        let phase = "modification_storm";
        phase_banner(phase);
        acc.push_event(Event {
            elapsed_secs: bench_start.elapsed().as_secs_f64(),
            kind: EventKind::PhaseStart(phase.into()),
        });

        let count = scaled(cli.modify_count, scale);
        eprintln!("  Modifying {count} existing files…");
        let t = Instant::now();
        let stats = workload::modification_storm(&server_dir, count);
        eprintln!(
            "  Done in {:.1}s — {} files modified, {}",
            t.elapsed().as_secs_f64(),
            stats.files_modified,
            human_bytes(stats.bytes_written),
        );
        record_workload_event(&mut acc, &bench_start, &stats);

        wait_and_verify(
            phase,
            &bench_start,
            &mut acc,
            &server_dir,
            &client_dir,
            sync_timeout,
        );

        acc.push_event(Event {
            elapsed_secs: bench_start.elapsed().as_secs_f64(),
            kind: EventKind::PhaseEnd(phase.into()),
        });
    }

    // ── Phase 5: Delete & Recreate ──────────────────────────────────────
    if bench_start.elapsed() < total_budget {
        let phase = "delete_and_recreate";
        phase_banner(phase);
        acc.push_event(Event {
            elapsed_secs: bench_start.elapsed().as_secs_f64(),
            kind: EventKind::PhaseStart(phase.into()),
        });

        let del = scaled(cli.delete_count, scale);
        let create = scaled(cli.recreate_count, scale);
        eprintln!("  Deleting {del} files, recreating {create}…");
        let t = Instant::now();
        let stats = workload::delete_and_recreate(&server_dir, del, create);
        eprintln!(
            "  Done in {:.1}s — {} deleted, {} created, {}",
            t.elapsed().as_secs_f64(),
            stats.files_deleted,
            stats.files_created,
            human_bytes(stats.bytes_written),
        );
        record_workload_event(&mut acc, &bench_start, &stats);

        // Deletes take a bit longer to propagate; give extra settle time
        eprintln!("  Allowing extra settle time for delete propagation…");
        std::thread::sleep(Duration::from_secs(5));

        wait_and_verify(
            phase,
            &bench_start,
            &mut acc,
            &server_dir,
            &client_dir,
            sync_timeout,
        );

        acc.push_event(Event {
            elapsed_secs: bench_start.elapsed().as_secs_f64(),
            kind: EventKind::PhaseEnd(phase.into()),
        });
    }

    // ── Phase 6: Sustained Mixed Load ───────────────────────────────────
    if bench_start.elapsed() < total_budget {
        let phase = "sustained_load";
        phase_banner(phase);
        acc.push_event(Event {
            elapsed_secs: bench_start.elapsed().as_secs_f64(),
            kind: EventKind::PhaseStart(phase.into()),
        });

        let remaining = total_budget.saturating_sub(bench_start.elapsed());
        let sustained_end = Instant::now() + remaining;
        let tick_interval = Duration::from_secs(cli.sustained_tick_interval);
        let integrity_interval = Duration::from_secs(120); // verify every 2 minutes
        let mut tick: usize = 0;
        let mut last_integrity_check = Instant::now();
        let mut cumulative_stats = WorkloadStats::default();

        eprintln!(
            "  Running sustained load for {:.0}s ({} tick interval)…",
            remaining.as_secs_f64(),
            cli.sustained_tick_interval,
        );

        while Instant::now() < sustained_end {
            let stats = workload::sustained_tick(&server_dir, tick);
            cumulative_stats.files_created += stats.files_created;
            cumulative_stats.bytes_written += stats.bytes_written;

            if tick % 50 == 0 {
                eprintln!(
                    "  [tick {tick:>5}] +{} files, {} total, {:.0}s remaining",
                    stats.files_created,
                    cumulative_stats.files_created,
                    sustained_end
                        .saturating_duration_since(Instant::now())
                        .as_secs_f64(),
                );
            }

            // Record workload event every 10 ticks to keep the event log manageable
            if tick % 10 == 0 {
                record_workload_event(&mut acc, &bench_start, &stats);
            }

            // Periodic integrity check during sustained load
            if last_integrity_check.elapsed() >= integrity_interval {
                eprintln!("  📋 Periodic integrity check at tick {tick}…");
                // Brief wait for sync to catch up before checking
                std::thread::sleep(Duration::from_secs(5));
                let sync_ok =
                    integrity::wait_for_sync(&server_dir, &client_dir, Duration::from_secs(60));
                if !sync_ok {
                    eprintln!("  ⚠️  Sync didn't fully catch up for periodic check");
                }

                let result = integrity::check_integrity(&server_dir, &client_dir);
                let passed = result.passed();
                let check_name = format!("sustained_tick_{tick}");

                acc.push_event(Event {
                    elapsed_secs: bench_start.elapsed().as_secs_f64(),
                    kind: EventKind::IntegrityCheck {
                        phase: check_name.clone(),
                        passed,
                        matched: result.matched,
                        mismatched: result.mismatched.len(),
                        missing: result.missing_from_dest.len(),
                        extra: result.extra_in_dest.len(),
                    },
                });

                if passed {
                    eprintln!("  ✅ Periodic check OK — {} files matched", result.matched);
                } else {
                    eprintln!(
                        "  ❌ Periodic check FAILED — {} matched, {} mismatched, {} missing",
                        result.matched,
                        result.mismatched.len(),
                        result.missing_from_dest.len()
                    );
                }
                acc.push_integrity(check_name, result);
                last_integrity_check = Instant::now();
            }

            tick += 1;
            std::thread::sleep(tick_interval);
        }

        // Record the cumulative stats for sustained phase
        acc.push_event(Event {
            elapsed_secs: bench_start.elapsed().as_secs_f64(),
            kind: EventKind::FilesCreated {
                count: cumulative_stats.files_created,
                total_bytes: cumulative_stats.bytes_written,
            },
        });

        // Final integrity check for sustained phase
        eprintln!("  Final sustained-phase integrity check…");
        wait_and_verify(
            phase,
            &bench_start,
            &mut acc,
            &server_dir,
            &client_dir,
            sync_timeout,
        );

        acc.push_event(Event {
            elapsed_secs: bench_start.elapsed().as_secs_f64(),
            kind: EventKind::PhaseEnd(phase.into()),
        });
    }

    // ── Shutdown ────────────────────────────────────────────────────────
    banner("Benchmark Complete — Generating Report");

    // Stop metrics collection first so we capture the final samples before
    // subprocess shutdown.
    let (server_samples, client_samples) = metrics_handle.stop();
    let total_duration = bench_start.elapsed();
    eprintln!(
        "  Collected {} server + {} client metric samples",
        server_samples.len(),
        client_samples.len()
    );

    // Collect DHAT output paths before shutdown (borrows server/client).
    let server_dhat_out: Option<PathBuf> = server.dhat_path().map(|p| p.to_path_buf());
    let client_dhat_out: Option<PathBuf> = client.dhat_path().map(|p| p.to_path_buf());

    // Shut down server first — this closes the TLS connection, which unblocks
    // the client's recv_loop so the client can exit cleanly.
    eprintln!("  Shutting down server subprocess…");
    let server_logs = server.shutdown_and_collect_logs();
    std::thread::sleep(Duration::from_millis(300));
    eprintln!("  Shutting down client subprocess…");
    let client_logs = client.shutdown_and_collect_logs();

    // ── DHAT analysis ────────────────────────────────────────────────────
    // valgrind writes the DHAT JSON file after the monitored process exits.
    // Poll for it, then parse the allocation-site breakdown.
    let dhat_timeout = Duration::from_secs(120);

    let dhat_server = server_dhat_out.as_deref().and_then(|path| {
        eprintln!("  ⏳ Waiting for server DHAT output…");
        dhat::wait_for_output(path, dhat_timeout).map(|p| {
            eprintln!("  🔬 Parsing server DHAT profile…");
            dhat::parse(&p)
        })
    });

    let dhat_client = client_dhat_out.as_deref().and_then(|path| {
        eprintln!("  ⏳ Waiting for client DHAT output…");
        dhat::wait_for_output(path, dhat_timeout).map(|p| {
            eprintln!("  🔬 Parsing client DHAT profile…");
            dhat::parse(&p)
        })
    });

    if let Some(ref s) = dhat_server {
        eprintln!(
            "  Server heap: {} total bytes, {} peak site bytes",
            s.total_bytes, s.max_site_peak_bytes
        );
    }
    if let Some(ref c) = dhat_client {
        eprintln!(
            "  Client heap: {} total bytes, {} peak site bytes",
            c.total_bytes, c.max_site_peak_bytes
        );
    }

    acc.push_event(Event {
        elapsed_secs: bench_start.elapsed().as_secs_f64(),
        kind: EventKind::Info("Benchmark finished".into()),
    });

    // Mark the run as cleanly completed in the data store.
    data_store.append(&data_store::DataRecord::Complete {
        total_duration_secs: total_duration.as_secs_f64(),
    });

    // ── Build report ────────────────────────────────────────────────────
    let report = BenchmarkReport::new(
        total_duration,
        acc.events,
        acc.integrity_results,
        server_samples,
        client_samples,
        dhat_server,
        dhat_client,
        server_logs,
        client_logs,
    );

    let report_path = run_dir.join("report.html");
    report
        .generate_html(&report_path)
        .expect("generate HTML report");

    // ── Summary ─────────────────────────────────────────────────────────
    let all_passed = report.integrity_results.iter().all(|(_, r)| r.passed());
    let total_files: usize = report
        .events
        .iter()
        .map(|e| match &e.kind {
            EventKind::FilesCreated { count, .. } => *count,
            _ => 0,
        })
        .sum();
    let total_bytes: u64 = report
        .events
        .iter()
        .map(|e| match &e.kind {
            EventKind::FilesCreated { total_bytes, .. } => *total_bytes,
            EventKind::FilesModified { total_bytes, .. } => *total_bytes,
            _ => 0,
        })
        .sum();

    let srv_peak_cpu = report
        .server_samples
        .iter()
        .map(|s| s.cpu_percent as u64)
        .max()
        .unwrap_or(0);
    let cli_peak_cpu = report
        .client_samples
        .iter()
        .map(|s| s.cpu_percent as u64)
        .max()
        .unwrap_or(0);
    let srv_peak_mem = report
        .server_samples
        .iter()
        .map(|s| s.rss_bytes)
        .max()
        .unwrap_or(0);
    let cli_peak_mem = report
        .client_samples
        .iter()
        .map(|s| s.rss_bytes)
        .max()
        .unwrap_or(0);

    eprintln!();
    eprintln!("  Duration       : {:.1}s", total_duration.as_secs_f64());
    eprintln!("  Files created  : {total_files}");
    eprintln!("  Data written   : {}", human_bytes(total_bytes));
    eprintln!(
        "  Server peak    : {srv_peak_cpu}% CPU, {} RSS",
        human_bytes(srv_peak_mem)
    );
    eprintln!(
        "  Client peak    : {cli_peak_cpu}% CPU, {} RSS",
        human_bytes(cli_peak_mem)
    );
    eprintln!(
        "  Integrity      : {}",
        if all_passed {
            "✅ ALL PASSED"
        } else {
            "❌ FAILURES DETECTED"
        }
    );
    eprintln!("  Run dir        : {}", run_dir.display());
    eprintln!("  Report         : {}", report_path.display());
    eprintln!(
        "  Command        : {}",
        run_dir.join("command.txt").display()
    );
    eprintln!();

    // ── Cleanup ─────────────────────────────────────────────────────────
    if !cli.keep_dirs {
        eprintln!("  Cleaning up temp directories…");
        let _ = fs::remove_dir_all(&base);
    } else {
        eprintln!("  Keeping temp directories at {}", base.display());
    }

    if !all_passed {
        std::process::exit(1);
    }
}
