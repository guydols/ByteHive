use super::types::ProcessSample;
use super::types::ThreadSample;

use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Clock ticks per second. This is 100 (USER_HZ) on virtually all Linux systems
/// and is adequate for a benchmark tool.
const CLOCK_TICKS_PER_SEC: f64 = 100.0;

// ─── /proc readers ──────────────────────────────────────────────────────────

/// Reads cumulative (utime, stime) ticks for a process from `/proc/<pid>/stat`.
fn read_process_cpu_ticks(pid: u32) -> (u64, u64) {
    let path = format!("/proc/{}/stat", pid);
    let stat = fs::read_to_string(path).unwrap_or_default();
    // The comm field (field 2) is enclosed in parens and may contain spaces or
    // parens itself.  Find the *last* closing ')' to reliably skip it.
    let after_comm = match stat.rfind(')') {
        Some(idx) => &stat[idx + 1..],
        None => return (0, 0),
    };
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // Index 0 here = stat field 3 (state).
    // utime = stat field 14 → index 11
    // stime = stat field 15 → index 12
    if fields.len() > 12 {
        let utime: u64 = fields[11].parse().unwrap_or(0);
        let stime: u64 = fields[12].parse().unwrap_or(0);
        (utime, stime)
    } else {
        (0, 0)
    }
}

/// Reads memory and thread info from `/proc/<pid>/status`.
///
/// Returns (rss_bytes, vm_size_bytes, shared_bytes, private_bytes, thread_count).
fn read_process_memory(pid: u32) -> (u64, u64, u64, u64, u32) {
    let path = format!("/proc/{}/status", pid);
    let status = fs::read_to_string(path).unwrap_or_default();

    let mut vm_rss_kb: u64 = 0;
    let mut vm_size_kb: u64 = 0;
    let mut rss_file_kb: u64 = 0;
    let mut rss_shmem_kb: u64 = 0;
    let mut thread_count: u32 = 0;

    for line in status.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("VmRSS:") {
            vm_rss_kb = parse_status_kb(trimmed);
        } else if trimmed.starts_with("VmSize:") {
            vm_size_kb = parse_status_kb(trimmed);
        } else if trimmed.starts_with("RssFile:") {
            rss_file_kb = parse_status_kb(trimmed);
        } else if trimmed.starts_with("RssShmem:") {
            rss_shmem_kb = parse_status_kb(trimmed);
        } else if trimmed.starts_with("Threads:") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                thread_count = parts[1].parse().unwrap_or(0);
            }
        }
    }

    let rss_bytes = vm_rss_kb * 1024;
    let vm_size_bytes = vm_size_kb * 1024;
    let shared_bytes = (rss_file_kb + rss_shmem_kb) * 1024;
    let private_bytes = rss_bytes.saturating_sub(shared_bytes);

    (
        rss_bytes,
        vm_size_bytes,
        shared_bytes,
        private_bytes,
        thread_count,
    )
}

/// Parses a `/proc/<pid>/status` line of the form `Key:  1234 kB` and returns
/// the numeric value in kB.
fn parse_status_kb(line: &str) -> u64 {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[1].parse().unwrap_or(0)
    } else {
        0
    }
}

/// Reads cumulative disk I/O from `/proc/<pid>/io`.
///
/// Returns (read_bytes, write_bytes) – the actual disk I/O counters.
fn read_process_io(pid: u32) -> (u64, u64) {
    let path = format!("/proc/{}/io", pid);
    let io = fs::read_to_string(path).unwrap_or_default();

    let mut read_bytes: u64 = 0;
    let mut write_bytes: u64 = 0;

    for line in io.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("read_bytes:") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                read_bytes = parts[1].parse().unwrap_or(0);
            }
        } else if trimmed.starts_with("write_bytes:") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                write_bytes = parts[1].parse().unwrap_or(0);
            }
        }
    }

    (read_bytes, write_bytes)
}

/// Reads cumulative RX and TX bytes for the loopback interface from
/// `/proc/net/dev` (global, not per-process).
fn read_net_loopback_bytes() -> (u64, u64) {
    let dev = fs::read_to_string("/proc/net/dev").unwrap_or_default();
    for line in dev.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("lo:") {
            let after_colon = match trimmed.find(':') {
                Some(idx) => &trimmed[idx + 1..],
                None => continue,
            };
            let fields: Vec<&str> = after_colon.split_whitespace().collect();
            // field 0 = rx_bytes, field 8 = tx_bytes
            if fields.len() > 8 {
                let rx: u64 = fields[0].parse().unwrap_or(0);
                let tx: u64 = fields[8].parse().unwrap_or(0);
                return (rx, tx);
            }
        }
    }
    (0, 0)
}

// ─── Per-thread sampling ────────────────────────────────────────────────────

/// Per-thread tick state keyed by TID: (prev_utime, prev_stime).
type ThreadTickMap = HashMap<u32, (u64, u64)>;

/// Reads (utime, stime) from `/proc/<pid>/task/<tid>/stat`.
///
/// In the per-thread stat file the utime and stime fields are at the same
/// positions as in the process stat file (fields 14 and 15, 1-indexed; after
/// skipping comm they are at offsets 11 and 12 in the remainder).
fn read_thread_cpu_ticks(pid: u32, tid: u32) -> (u64, u64) {
    let path = format!("/proc/{}/task/{}/stat", pid, tid);
    let stat = fs::read_to_string(path).unwrap_or_default();
    let after_comm = match stat.rfind(')') {
        Some(idx) => &stat[idx + 1..],
        None => return (0, 0),
    };
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    if fields.len() > 12 {
        let utime: u64 = fields[11].parse().unwrap_or(0);
        let stime: u64 = fields[12].parse().unwrap_or(0);
        (utime, stime)
    } else {
        (0, 0)
    }
}

/// Reads the thread name from `/proc/<pid>/task/<tid>/comm`.
fn read_thread_name(pid: u32, tid: u32) -> String {
    let path = format!("/proc/{}/task/{}/comm", pid, tid);
    fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Enumerates all TIDs under `/proc/<pid>/task/`.
fn list_tids(pid: u32) -> Vec<u32> {
    let path = format!("/proc/{}/task", pid);
    let entries = match fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut tids = Vec::new();
    for entry in entries {
        if let Ok(entry) = entry {
            if let Some(name) = entry.file_name().to_str() {
                if let Ok(tid) = name.parse::<u32>() {
                    tids.push(tid);
                }
            }
        }
    }
    tids
}

/// Samples all threads for a given PID and computes per-thread CPU deltas.
///
/// `prev_ticks` is mutated in-place to store the new tick values.  Threads
/// that have disappeared are pruned from the map.
fn sample_threads(pid: u32, prev_ticks: &mut ThreadTickMap, wall_delta: f64) -> Vec<ThreadSample> {
    let tids = list_tids(pid);
    let mut current_tids: HashMap<u32, ()> = HashMap::with_capacity(tids.len());
    let mut samples = Vec::with_capacity(tids.len());

    for tid in tids {
        current_tids.insert(tid, ());

        let name = read_thread_name(pid, tid);
        let (utime, stime) = read_thread_cpu_ticks(pid, tid);

        let (user_cpu_percent, sys_cpu_percent) =
            if let Some(&(prev_u, prev_s)) = prev_ticks.get(&tid) {
                let du = utime.saturating_sub(prev_u) as f64;
                let ds = stime.saturating_sub(prev_s) as f64;
                if wall_delta > 0.0 {
                    (
                        (du / CLOCK_TICKS_PER_SEC) / wall_delta * 100.0,
                        (ds / CLOCK_TICKS_PER_SEC) / wall_delta * 100.0,
                    )
                } else {
                    (0.0, 0.0)
                }
            } else {
                (0.0, 0.0)
            };

        prev_ticks.insert(tid, (utime, stime));

        samples.push(ThreadSample {
            name,
            cpu_percent: user_cpu_percent + sys_cpu_percent,
            user_cpu_percent,
            sys_cpu_percent,
        });
    }

    // Prune threads that no longer exist.
    prev_ticks.retain(|tid, _| current_tids.contains_key(tid));

    // Sort by cpu_percent descending.
    samples.sort_by(|a, b| {
        b.cpu_percent
            .partial_cmp(&a.cpu_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    samples
}

// ─── Per-process sampler state ──────────────────────────────────────────────

/// Internal state for sampling a single process.
struct ProcessState {
    pid: u32,
    prev_utime: u64,
    prev_stime: u64,
    prev_wall: Instant,
    thread_ticks: ThreadTickMap,
}

impl ProcessState {
    fn new(pid: u32, now: Instant) -> Self {
        let (utime, stime) = read_process_cpu_ticks(pid);
        Self {
            pid,
            prev_utime: utime,
            prev_stime: stime,
            prev_wall: now,
            thread_ticks: HashMap::new(),
        }
    }

    /// Takes a single sample and advances internal state.
    fn sample(&mut self, elapsed_secs: f64) -> ProcessSample {
        let now = Instant::now();
        let wall_delta = now.duration_since(self.prev_wall).as_secs_f64();

        // ── CPU ──
        let (utime, stime) = read_process_cpu_ticks(self.pid);
        let du = utime.saturating_sub(self.prev_utime) as f64;
        let ds = stime.saturating_sub(self.prev_stime) as f64;
        let (user_cpu_percent, sys_cpu_percent) = if wall_delta > 0.0 {
            (
                (du / CLOCK_TICKS_PER_SEC) / wall_delta * 100.0,
                (ds / CLOCK_TICKS_PER_SEC) / wall_delta * 100.0,
            )
        } else {
            (0.0, 0.0)
        };
        let cpu_percent = user_cpu_percent + sys_cpu_percent;

        self.prev_utime = utime;
        self.prev_stime = stime;
        self.prev_wall = now;

        // ── Memory ──
        let (rss_bytes, vm_size_bytes, shared_bytes, private_bytes, thread_count) =
            read_process_memory(self.pid);

        // ── Threads ──
        let threads = sample_threads(self.pid, &mut self.thread_ticks, wall_delta);

        // ── Disk I/O ──
        let (io_read_bytes, io_write_bytes) = read_process_io(self.pid);

        // ── Network (loopback, global) ──
        let (net_rx_bytes, net_tx_bytes) = read_net_loopback_bytes();

        ProcessSample {
            elapsed_secs,
            cpu_percent,
            user_cpu_percent,
            sys_cpu_percent,
            rss_bytes,
            vm_size_bytes,
            shared_bytes,
            private_bytes,
            thread_count,
            threads,
            io_read_bytes,
            io_write_bytes,
            net_rx_bytes,
            net_tx_bytes,
        }
    }
}

// ─── Public API ─────────────────────────────────────────────────────────────

pub struct ProcessMetricsCollector {
    interval: Duration,
}

impl ProcessMetricsCollector {
    pub fn new(interval: Duration) -> Self {
        Self { interval }
    }

    /// Starts a background thread that periodically samples both the server
    /// and client processes identified by their PIDs.  Returns a
    /// [`MetricsHandle`] that can be used to retrieve snapshots or stop
    /// collection.
    pub fn start(
        self,
        server_pid: u32,
        client_pid: u32,
        start_instant: Instant,
        data_store: std::sync::Arc<super::data_store::DataStore>,
    ) -> MetricsHandle {
        let server_samples: Arc<Mutex<Vec<ProcessSample>>> = Arc::new(Mutex::new(Vec::new()));
        let client_samples: Arc<Mutex<Vec<ProcessSample>>> = Arc::new(Mutex::new(Vec::new()));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let srv_clone = server_samples.clone();
        let cli_clone = client_samples.clone();
        let stop_clone = stop_flag.clone();
        let interval = self.interval;

        let data_store_clone = data_store;

        let handle = thread::Builder::new()
            .name("metrics-sampler".into())
            .spawn(move || {
                let now = Instant::now();
                let mut server_state = ProcessState::new(server_pid, now);
                let mut client_state = ProcessState::new(client_pid, now);

                while !stop_clone.load(Ordering::Relaxed) {
                    thread::sleep(interval);

                    let elapsed = start_instant.elapsed().as_secs_f64();

                    let srv_sample = server_state.sample(elapsed);
                    let cli_sample = client_state.sample(elapsed);

                    data_store_clone.append(&super::data_store::DataRecord::ServerSample {
                        sample: srv_sample.clone(),
                    });
                    data_store_clone.append(&super::data_store::DataRecord::ClientSample {
                        sample: cli_sample.clone(),
                    });

                    if let Ok(mut guard) = srv_clone.lock() {
                        guard.push(srv_sample);
                    }
                    if let Ok(mut guard) = cli_clone.lock() {
                        guard.push(cli_sample);
                    }
                }
            })
            .expect("failed to spawn metrics-sampler thread");

        MetricsHandle {
            server_samples,
            client_samples,
            stop_flag,
            thread: Some(handle),
        }
    }
}

pub struct MetricsHandle {
    server_samples: Arc<Mutex<Vec<ProcessSample>>>,
    client_samples: Arc<Mutex<Vec<ProcessSample>>>,
    stop_flag: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MetricsHandle {
    /// Returns snapshots of all collected samples so far for
    /// (server, client).
    pub fn snapshot(&self) -> (Vec<ProcessSample>, Vec<ProcessSample>) {
        let srv = self
            .server_samples
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let cli = self
            .client_samples
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        (srv, cli)
    }

    /// Stops sampling and returns all collected samples for
    /// (server, client).
    pub fn stop(mut self) -> (Vec<ProcessSample>, Vec<ProcessSample>) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
        let srv = self
            .server_samples
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let cli = self
            .client_samples
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        (srv, cli)
    }
}
