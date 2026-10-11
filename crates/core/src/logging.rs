//! Server logging: stderr (unchanged), a rotated server log file that mirrors
//! every stderr record, and a rotated JSON-lines file recording bus traffic.
//!
//! Layout under the logs root (default `<binary dir>/logs`, see
//! [`resolve_log_dir`]):
//!
//! ```text
//! logs/server/server.log        active server log (same records as stderr)
//! logs/server/server.log.1..N   rotated copies, newest = .1
//! logs/bus/bus.jsonl            active bus record, one JSON object per line
//! logs/bus/bus.jsonl.1..N       rotated copies
//! ```
//!
//! Failures here never panic: I/O errors are reported once on the transition
//! into the failing state (to stderr), and the record is dropped from the
//! file only.

use crate::bus::{BusMessage, BusReceiver, MessageBus};
use crate::config::LoggingConfig;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Subdirectory of the binary directory used when `[logging] dir` is unset.
pub const LOG_SUBDIR: &str = "logs";
/// Server log path relative to the logs root.
pub const SERVER_LOG_REL: &str = "server/server.log";
/// Bus log path relative to the logs root.
pub const BUS_LOG_REL: &str = "bus/bus.jsonl";
/// Queue depth of the bus logger subscription. Large so bursts are absorbed
/// while the writer thread catches up; drops are still counted.
pub const BUS_LOG_QUEUE_DEPTH: usize = 65_536;
/// Placeholder written in place of redacted values.
pub const REDACTED: &str = "[REDACTED]";

const MIB: u64 = 1024 * 1024;
/// Directory recursion limit for cleanup (logs/<stream>/file is depth 2).
const CLEANUP_MAX_DEPTH: usize = 3;

/// Object keys whose value is redacted if the lowercased, alphanumeric-only
/// key *contains* one of these.
const SENSITIVE_SUBSTRINGS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "authorization",
    "cookie",
    "credential",
];
/// Object keys redacted only on an exact (normalized) match.
const SENSITIVE_EXACT: &[&str] = &[
    "login",
    "auth",
    "pass",
    "pwd",
    "key",
    "session",
    "sessionid",
];

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Default logs root: `<dir of exe>/logs`, or `./logs` if the exe path is
/// unavailable or has no parent.
pub fn default_log_dir_from(exe: io::Result<PathBuf>) -> PathBuf {
    match exe {
        Ok(path) => match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.join(LOG_SUBDIR),
            _ => PathBuf::from(LOG_SUBDIR),
        },
        Err(_) => PathBuf::from(LOG_SUBDIR),
    }
}

/// Resolve the logs root: the configured `[logging] dir` if set (non-blank),
/// otherwise the binary-relative default.
pub fn resolve_log_dir(cfg: &LoggingConfig) -> PathBuf {
    match cfg.dir.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => default_log_dir_from(std::env::current_exe()),
    }
}

pub fn server_log_path(root: &Path) -> PathBuf {
    root.join(SERVER_LOG_REL)
}

pub fn bus_log_path(root: &Path) -> PathBuf {
    root.join(BUS_LOG_REL)
}

fn numbered(path: &Path, n: u32) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

// ---------------------------------------------------------------------------
// Rotating file
// ---------------------------------------------------------------------------

struct OpenFile {
    file: File,
    size: u64,
}

fn open_append(path: &Path) -> io::Result<OpenFile> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let size = file.metadata()?.len();
    Ok(OpenFile { file, size })
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn rename_if_exists(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Shift `name` -> `name.1` -> … -> `name.<max_files>`, dropping the oldest.
/// With `max_files == 0` the active file is simply removed.
pub fn rotate_files(path: &Path, max_files: u32) -> io::Result<()> {
    if max_files == 0 {
        return remove_if_exists(path);
    }
    remove_if_exists(&numbered(path, max_files))?;
    for i in (1..max_files).rev() {
        rename_if_exists(&numbered(path, i), &numbered(path, i + 1))?;
    }
    rename_if_exists(path, &numbered(path, 1))
}

/// Append-only file that rotates by size. All writes go through one mutex, so
/// concurrent writers within the process never interleave a line or rotate
/// twice. (Multiple processes writing the same file are not supported.)
pub struct RotatingFile {
    path: PathBuf,
    max_bytes: Option<u64>,
    max_files: u32,
    state: Mutex<Option<OpenFile>>,
}

impl RotatingFile {
    /// `max_file_mb == 0` disables size rotation.
    pub fn new(path: impl Into<PathBuf>, max_file_mb: u64, max_files: u32) -> Self {
        Self {
            path: path.into(),
            max_bytes: (max_file_mb > 0).then(|| max_file_mb.saturating_mul(MIB)),
            max_files,
            state: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record. Rotates first if the record would push the active
    /// file past the size limit (a file is never rotated while empty, so an
    /// oversized single record is still written). The file is opened lazily
    /// and reopened after any error.
    pub fn write_line(&self, line: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock();
        let result = self.write_locked(&mut *state, line);
        if result.is_err() {
            *state = None;
        }
        result
    }

    fn write_locked(&self, state: &mut Option<OpenFile>, line: &[u8]) -> io::Result<()> {
        if state.is_none() {
            *state = Some(open_append(&self.path)?);
        }
        let need = line.len() as u64;
        let must_rotate = match (state.as_ref(), self.max_bytes) {
            (Some(open), Some(max)) => open.size > 0 && open.size + need > max,
            _ => false,
        };
        if must_rotate {
            *state = None; // close the handle before renaming
            rotate_files(&self.path, self.max_files)?;
            *state = Some(open_append(&self.path)?);
        }
        let open = state
            .as_mut()
            .ok_or_else(|| io::Error::other("log file not open"))?;
        open.file.write_all(line)?;
        open.size += need;
        Ok(())
    }
}

/// A `RotatingFile` that reports I/O failures to stderr once per transition
/// (failing / recovered) instead of once per record.
pub struct LogSink {
    name: &'static str,
    file: RotatingFile,
    failing: AtomicBool,
    lost: AtomicU64,
}

impl LogSink {
    pub fn new(name: &'static str, file: RotatingFile) -> Self {
        Self {
            name,
            file,
            failing: AtomicBool::new(false),
            lost: AtomicU64::new(0),
        }
    }

    /// Records lost to I/O errors so far.
    pub fn lost_records(&self) -> u64 {
        self.lost.load(Ordering::Relaxed)
    }

    pub fn write_line(&self, line: &[u8]) {
        match self.file.write_line(line) {
            Ok(()) => {
                if self.failing.swap(false, Ordering::Relaxed) {
                    eprintln!(
                        "{}: writes to {} recovered",
                        self.name,
                        self.file.path().display()
                    );
                }
            }
            Err(e) => {
                self.lost.fetch_add(1, Ordering::Relaxed);
                if !self.failing.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "warning: {}: write to {} failed: {e} — continuing with stderr only",
                        self.name,
                        self.file.path().display()
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Time formatting (UTC, no external date crate)
// ---------------------------------------------------------------------------

/// Format Unix milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn format_utc_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Server log (stderr tee)
// ---------------------------------------------------------------------------

/// One server-log line: `<ts> <LEVEL> [<target>] <message>\n`.
pub fn format_server_line(ts_ms: u64, record: &log::Record) -> String {
    format!(
        "{} {:<5} [{}] {}\n",
        format_utc_ms(ts_ms),
        record.level().as_str(),
        record.target(),
        record.args()
    )
}

/// Forwards records that pass the stderr filter to both stderr and the server
/// log file, so the file holds exactly the records shown on stderr.
struct TeeLogger {
    stderr: env_logger::Logger,
    server: LogSink,
}

impl log::Log for TeeLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        log::Log::enabled(&self.stderr, metadata)
    }

    fn log(&self, record: &log::Record) {
        if !self.stderr.matches(record) {
            return;
        }
        log::Log::log(&self.stderr, record);
        let line = format_server_line(now_ms(), record);
        self.server.write_line(line.as_bytes());
    }

    fn flush(&self) {
        log::Log::flush(&self.stderr);
    }
}

/// Install the global logger: the given env_logger on stderr, teed to
/// `<root>/server/server.log`. Call once, early in `main`.
pub fn init(
    stderr: env_logger::Logger,
    root: &Path,
    cfg: &LoggingConfig,
) -> Result<(), log::SetLoggerError> {
    let max_level = stderr.filter();
    let server = LogSink::new(
        "server log",
        RotatingFile::new(server_log_path(root), cfg.max_file_mb, cfg.max_files),
    );
    log::set_boxed_logger(Box::new(TeeLogger { stderr, server }))?;
    log::set_max_level(max_level);
    Ok(())
}

// ---------------------------------------------------------------------------
// Bus JSONL
// ---------------------------------------------------------------------------

/// Redact credential-like values in place. Object values whose key looks
/// sensitive (see `SENSITIVE_*`) are replaced by [`REDACTED`] wholesale; string
/// values starting with `Bearer ` or `Basic ` are replaced as well.
pub fn redact_payload(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *val = Value::String(REDACTED.to_string());
                } else {
                    redact_payload(val);
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                redact_payload(item);
            }
        }
        Value::String(s) => {
            if looks_like_auth_header(s) {
                *s = REDACTED.to_string();
            }
        }
        _ => {}
    }
}

pub fn is_sensitive_key(key: &str) -> bool {
    let norm: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    SENSITIVE_SUBSTRINGS.iter().any(|s| norm.contains(s))
        || SENSITIVE_EXACT.iter().any(|s| norm == *s)
}

fn looks_like_auth_header(s: &str) -> bool {
    let t = s.trim_start();
    ["bearer ", "basic "]
        .iter()
        .any(|p| t.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p)))
}

#[derive(Serialize)]
struct BusLine<'a> {
    timestamp: String,
    id: u64,
    source: &'a str,
    topic: &'a str,
    payload: &'a Value,
}

/// One bus.jsonl line (newline-terminated) for `msg`, payload redacted.
/// `timestamp` is the message's `timestamp_ms` rendered as UTC ISO-8601.
pub fn bus_record_line(msg: &BusMessage) -> String {
    let mut payload = msg.payload.clone();
    redact_payload(&mut payload);
    let line = BusLine {
        timestamp: format_utc_ms(msg.timestamp_ms),
        id: msg.id,
        source: &msg.source,
        topic: &msg.topic,
        payload: &payload,
    };
    let mut out = serde_json::to_string(&line)
        .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }).to_string());
    out.push('\n');
    out
}

fn bus_loop(rx: BusReceiver, sink: LogSink) {
    let mut reported = 0u64;
    loop {
        let status = rx.recv_timeout(Duration::from_secs(1));
        if let Ok(msg) = &status {
            sink.write_line(bus_record_line(msg).as_bytes());
        }
        let dropped = rx.dropped();
        if dropped > reported {
            log::warn!(
                "bus logger: {} message(s) dropped (subscriber queue full); \
                 bus.jsonl is incomplete — {} dropped in total",
                dropped - reported,
                dropped
            );
            reported = dropped;
        }
        if let Err(crossbeam_channel::RecvTimeoutError::Disconnected) = status {
            break;
        }
    }
}

/// Subscribe to every topic with a large queue and write each message to
/// `<root>/bus/bus.jsonl` on a background thread. The subscription is made
/// synchronously, so messages published after this returns are captured.
pub fn spawn_bus_logger(
    bus: &MessageBus,
    root: &Path,
    cfg: &LoggingConfig,
) -> io::Result<JoinHandle<()>> {
    let rx = bus.subscribe("*", BUS_LOG_QUEUE_DEPTH);
    let sink = LogSink::new(
        "bus log",
        RotatingFile::new(bus_log_path(root), cfg.max_file_mb, cfg.max_files),
    );
    std::thread::Builder::new()
        .name("bus-logger".into())
        .spawn(move || bus_loop(rx, sink))
}

// ---------------------------------------------------------------------------
// Cleanup
// ---------------------------------------------------------------------------

/// Retention limits, derived from `[logging]`. `None` disables that limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub retention: Option<Duration>,
    pub max_total_bytes: Option<u64>,
}

impl RetentionPolicy {
    pub fn from_config(cfg: &LoggingConfig) -> Self {
        Self {
            retention: (cfg.retention_days > 0)
                .then(|| Duration::from_secs(cfg.retention_days.saturating_mul(86_400))),
            max_total_bytes: (cfg.max_total_mb > 0).then(|| cfg.max_total_mb.saturating_mul(MIB)),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CleanupReport {
    pub deleted_expired: usize,
    pub deleted_for_size: usize,
    pub bytes_freed: u64,
    pub total_bytes_after: u64,
    /// Active files alone exceed the cap; nothing more could be deleted.
    pub over_cap_with_active: bool,
}

struct Entry {
    path: PathBuf,
    size: u64,
    mtime: SystemTime,
    /// `Some(n)` for a rotated file `<stream>.log.n` / `<stream>.jsonl.n`.
    /// Only rotated files are ever candidates for deletion.
    rotated: Option<u32>,
}

/// `server.log.3` -> Some(3); `bus.jsonl.1` -> Some(1); `server.log` -> None.
pub fn parse_rotated_name(name: &str) -> Option<u32> {
    let (base, num) = name.rsplit_once('.')?;
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !(base.ends_with(".log") || base.ends_with(".jsonl")) {
        return None;
    }
    num.parse().ok()
}

fn collect(dir: &Path, depth: usize, now: SystemTime, out: &mut Vec<Entry>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            if depth < CLEANUP_MAX_DEPTH {
                collect(&path, depth + 1, now, out);
            }
        } else if ft.is_file() {
            let Ok(meta) = entry.metadata() else { continue };
            let name = entry.file_name();
            let rotated = name.to_str().and_then(parse_rotated_name);
            out.push(Entry {
                path,
                size: meta.len(),
                mtime: meta.modified().unwrap_or(now),
                rotated,
            });
        }
    }
}

fn remove_entry(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => {
            log::warn!("log cleanup: cannot delete {}: {e}", path.display());
            false
        }
    }
}

/// One cleanup pass over `root` (recursively, shallow). Order:
/// 1. delete rotated files whose mtime is older than `retention`;
/// 2. if the total size still exceeds `max_total_bytes`, delete rotated files
///    oldest-first (mtime, then highest rotation number) until under the cap.
///
/// Active files (`server.log`, `bus.jsonl`, anything not rotated) are never
/// deleted. `now` is injected for testing.
pub fn cleanup(root: &Path, policy: &RetentionPolicy, now: SystemTime) -> CleanupReport {
    let mut entries = Vec::new();
    collect(root, 0, now, &mut entries);
    let mut report = CleanupReport::default();

    if let Some(ret) = policy.retention {
        if let Some(cutoff) = now.checked_sub(ret) {
            let mut kept = Vec::with_capacity(entries.len());
            for e in entries {
                if e.rotated.is_some() && e.mtime < cutoff && remove_entry(&e.path) {
                    report.deleted_expired += 1;
                    report.bytes_freed += e.size;
                } else {
                    kept.push(e);
                }
            }
            entries = kept;
        }
    }

    let mut total: u64 = entries.iter().map(|e| e.size).sum();
    if let Some(cap) = policy.max_total_bytes {
        if total > cap {
            let mut candidates: Vec<&Entry> =
                entries.iter().filter(|e| e.rotated.is_some()).collect();
            candidates.sort_by(|a, b| {
                a.mtime
                    .cmp(&b.mtime)
                    .then_with(|| b.rotated.cmp(&a.rotated))
            });
            let mut doomed: Vec<(PathBuf, u64)> = Vec::new();
            for c in candidates {
                if total <= cap {
                    break;
                }
                doomed.push((c.path.clone(), c.size));
                total = total.saturating_sub(c.size);
            }
            for (path, size) in doomed {
                if remove_entry(&path) {
                    report.deleted_for_size += 1;
                    report.bytes_freed += size;
                }
            }
            if total > cap {
                report.over_cap_with_active = true;
                log::warn!(
                    "log cleanup: logs dir {} is {} bytes, over the {} byte cap, \
                     and only active files remain",
                    root.display(),
                    total,
                    cap
                );
            }
        }
    }

    report.total_bytes_after = total;
    report
}

/// Run [`cleanup`] now, then every `interval_secs` (0 = once, at startup).
/// Runs on a background thread so startup is not blocked.
pub fn spawn_cleanup(root: PathBuf, cfg: &LoggingConfig) -> io::Result<JoinHandle<()>> {
    let policy = RetentionPolicy::from_config(cfg);
    let interval = Duration::from_secs(cfg.cleanup_interval_secs);
    std::thread::Builder::new()
        .name("log-cleanup".into())
        .spawn(move || loop {
            let report = cleanup(&root, &policy, SystemTime::now());
            if report.deleted_expired + report.deleted_for_size > 0 {
                log::info!(
                    "log cleanup: removed {} expired and {} over-cap rotated file(s), freed {} bytes",
                    report.deleted_expired,
                    report.deleted_for_size,
                    report.bytes_freed
                );
            }
            if interval.is_zero() {
                break;
            }
            std::thread::sleep(interval);
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read;
    use std::sync::Arc;

    const MB: u64 = 1024 * 1024;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn read(path: &Path) -> String {
        let mut s = String::new();
        File::open(path).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    fn write_file(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    fn set_age(path: &Path, when: SystemTime) {
        let f = OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }

    // ---- default path resolution -------------------------------------------

    #[test]
    fn default_dir_is_next_to_binary() {
        let d = default_log_dir_from(Ok(PathBuf::from("/opt/bh/bin/bytehive")));
        assert_eq!(d, PathBuf::from("/opt/bh/bin/logs"));
    }

    #[test]
    fn default_dir_falls_back_to_cwd_when_exe_unknown() {
        let d = default_log_dir_from(Err(io::Error::other("no exe")));
        assert_eq!(d, PathBuf::from("logs"));
    }

    #[test]
    fn default_dir_falls_back_when_exe_has_no_parent() {
        let d = default_log_dir_from(Ok(PathBuf::from("bytehive")));
        assert_eq!(d, PathBuf::from("logs"));
    }

    #[test]
    fn resolve_uses_configured_dir_verbatim() {
        let cfg = LoggingConfig {
            dir: Some("/var/log/bytehive".into()),
            ..Default::default()
        };
        assert_eq!(resolve_log_dir(&cfg), PathBuf::from("/var/log/bytehive"));
    }

    #[test]
    fn resolve_treats_blank_dir_as_unset() {
        let cfg = LoggingConfig {
            dir: Some("   ".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_log_dir(&cfg),
            default_log_dir_from(std::env::current_exe())
        );
    }

    #[test]
    fn layout_paths() {
        let root = Path::new("/r");
        assert_eq!(server_log_path(root), PathBuf::from("/r/server/server.log"));
        assert_eq!(bus_log_path(root), PathBuf::from("/r/bus/bus.jsonl"));
    }

    // ---- rotation ------------------------------------------------------------

    #[test]
    fn rotates_exactly_at_size_boundary() {
        let dir = tmp();
        let path = dir.path().join("a.log");
        let rf = RotatingFile {
            path: path.clone(),
            max_bytes: Some(100),
            max_files: 3,
            state: Mutex::new(None),
        };
        rf.write_line(&[b'a'; 60]).unwrap();
        rf.write_line(&[b'b'; 40]).unwrap(); // exactly 100: no rotation
        assert!(!numbered(&path, 1).exists());
        assert_eq!(fs::metadata(&path).unwrap().len(), 100);

        rf.write_line(b"c").unwrap(); // 101 > 100: rotate
        assert_eq!(fs::metadata(numbered(&path, 1)).unwrap().len(), 100);
        assert_eq!(read(&path), "c");
    }

    #[test]
    fn oversized_single_record_is_written_not_looped() {
        let dir = tmp();
        let path = dir.path().join("big.log");
        let rf = RotatingFile {
            path: path.clone(),
            max_bytes: Some(10),
            max_files: 2,
            state: Mutex::new(None),
        };
        rf.write_line(&[b'z'; 50]).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 50);
        assert!(!numbered(&path, 1).exists());
    }

    #[test]
    fn max_file_mb_zero_disables_rotation() {
        let dir = tmp();
        let path = dir.path().join("nolimit.log");
        let rf = RotatingFile::new(&path, 0, 3);
        for _ in 0..5 {
            rf.write_line(&[b'q'; 1000]).unwrap();
        }
        assert_eq!(fs::metadata(&path).unwrap().len(), 5000);
        assert!(!numbered(&path, 1).exists());
    }

    #[test]
    fn max_files_prunes_oldest() {
        let dir = tmp();
        let path = dir.path().join("p.log");
        let rf = RotatingFile {
            path: path.clone(),
            max_bytes: Some(10),
            max_files: 2,
            state: Mutex::new(None),
        };
        // Each write of 10 bytes after the first forces a rotation.
        for tag in [b'1', b'2', b'3', b'4'] {
            rf.write_line(&[tag; 10]).unwrap();
        }
        // active = 4, .1 = 3, .2 = 2, and "1" has been pruned
        assert_eq!(read(&path), "4".repeat(10));
        assert_eq!(read(&numbered(&path, 1)), "3".repeat(10));
        assert_eq!(read(&numbered(&path, 2)), "2".repeat(10));
        assert!(!numbered(&path, 3).exists());
    }

    #[test]
    fn max_files_zero_keeps_no_rotated_copies() {
        let dir = tmp();
        let path = dir.path().join("z.log");
        let rf = RotatingFile {
            path: path.clone(),
            max_bytes: Some(10),
            max_files: 0,
            state: Mutex::new(None),
        };
        rf.write_line(&[b'a'; 10]).unwrap();
        rf.write_line(&[b'b'; 10]).unwrap();
        assert_eq!(read(&path), "b".repeat(10));
        assert!(!numbered(&path, 1).exists());
    }

    #[test]
    fn rotation_survives_concurrent_writers() {
        let dir = tmp();
        let path = dir.path().join("c.log");
        let rf = Arc::new(RotatingFile {
            path: path.clone(),
            max_bytes: Some(4096),
            max_files: 50,
            state: Mutex::new(None),
        });
        let mut handles = Vec::new();
        for t in 0..4 {
            let rf = Arc::clone(&rf);
            handles.push(std::thread::spawn(move || {
                for i in 0..200 {
                    let line = format!("{t:02}-{i:04}-{}\n", "p".repeat(50));
                    rf.write_line(line.as_bytes()).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Every surviving line is intact (no interleaving within a line).
        let mut total = 0;
        for n in std::iter::once(path.clone()).chain((1..=50).map(|i| numbered(&path, i))) {
            if let Ok(s) = fs::read_to_string(&n) {
                for line in s.lines() {
                    assert!(line.ends_with(&"p".repeat(50)), "torn line: {line}");
                    total += 1;
                }
            }
        }
        assert_eq!(total, 800);
    }

    #[test]
    fn io_error_is_returned_not_panicked() {
        let dir = tmp();
        // A path whose parent is a regular file cannot be created.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        let rf = RotatingFile::new(blocker.join("sub").join("f.log"), 1, 1);
        assert!(rf.write_line(b"hello\n").is_err());
        let sink = LogSink::new("test", rf);
        sink.write_line(b"again\n");
        sink.write_line(b"again\n");
        assert_eq!(sink.lost_records(), 2);
    }

    // ---- cleanup -----------------------------------------------------------

    #[test]
    fn parse_rotated_names() {
        assert_eq!(parse_rotated_name("server.log.1"), Some(1));
        assert_eq!(parse_rotated_name("bus.jsonl.12"), Some(12));
        assert_eq!(parse_rotated_name("server.log"), None);
        assert_eq!(parse_rotated_name("bus.jsonl"), None);
        assert_eq!(parse_rotated_name("notes.txt.1"), None);
        assert_eq!(parse_rotated_name("server.log.bak"), None);
        assert_eq!(parse_rotated_name("server.log."), None);
    }

    #[test]
    fn retention_deletes_only_expired_rotated_files() {
        let dir = tmp();
        let root = dir.path();
        let now = SystemTime::now();
        let old = now - Duration::from_secs(30 * 86_400);
        let recent = now - Duration::from_secs(86_400);

        let old_rot = root.join("server/server.log.1");
        let recent_rot = root.join("server/server.log.2");
        let old_active = root.join("server/server.log");
        write_file(&old_rot, 10);
        write_file(&recent_rot, 10);
        write_file(&old_active, 10);
        set_age(&old_rot, old);
        set_age(&recent_rot, recent);
        set_age(&old_active, old);

        let policy = RetentionPolicy {
            retention: Some(Duration::from_secs(14 * 86_400)),
            max_total_bytes: None,
        };
        let report = cleanup(root, &policy, now);
        assert_eq!(report.deleted_expired, 1);
        assert_eq!(report.bytes_freed, 10);
        assert!(!old_rot.exists());
        assert!(recent_rot.exists());
        assert!(old_active.exists(), "active file must never be deleted");
    }

    #[test]
    fn retention_zero_keeps_everything() {
        let dir = tmp();
        let now = SystemTime::now();
        let f = dir.path().join("bus/bus.jsonl.1");
        write_file(&f, 5);
        set_age(&f, now - Duration::from_secs(365 * 86_400));
        let policy = RetentionPolicy::from_config(&LoggingConfig {
            retention_days: 0,
            max_total_mb: 0,
            ..Default::default()
        });
        assert_eq!(policy.retention, None);
        cleanup(dir.path(), &policy, now);
        assert!(f.exists());
    }

    #[test]
    fn cleanup_without_limits_deletes_nothing() {
        let dir = tmp();
        let path = dir.path().join("server/server.log");
        let rf = RotatingFile {
            path: path.clone(),
            max_bytes: Some(4),
            max_files: 2,
            state: Mutex::new(None),
        };
        for _ in 0..4 {
            rf.write_line(b"abcd").unwrap();
        }
        let policy = RetentionPolicy {
            retention: None,
            max_total_bytes: None,
        };
        let report = cleanup(dir.path(), &policy, SystemTime::now());
        assert_eq!(report.deleted_expired + report.deleted_for_size, 0);
        assert!(path.exists());
        assert!(numbered(&path, 2).exists());
        assert!(!numbered(&path, 3).exists());
    }

    #[test]
    fn total_cap_deletes_oldest_rotated_first_and_never_active() {
        let dir = tmp();
        let root = dir.path();
        let now = SystemTime::now();
        // Four rotated files of 1000 bytes: ages 4h (oldest) .. 1h (newest).
        let a = root.join("server/server.log.3"); // oldest
        let b = root.join("server/server.log.2");
        let c = root.join("bus/bus.jsonl.2");
        let d = root.join("bus/bus.jsonl.1"); // newest rotated
        let active_srv = root.join("server/server.log");
        let active_bus = root.join("bus/bus.jsonl");
        write_file(&a, 1000);
        write_file(&b, 1000);
        write_file(&c, 1000);
        write_file(&d, 1000);
        write_file(&active_srv, 1000);
        write_file(&active_bus, 1000);
        set_age(&a, now - Duration::from_secs(4 * 3600));
        set_age(&b, now - Duration::from_secs(3 * 3600));
        set_age(&c, now - Duration::from_secs(2 * 3600));
        set_age(&d, now - Duration::from_secs(3600));
        set_age(&active_srv, now - Duration::from_secs(10 * 86_400));
        set_age(&active_bus, now - Duration::from_secs(10 * 86_400));

        // Total = 6000 bytes, cap = 4500: deleting a (1000) leaves 5000,
        // deleting b (1000) leaves 4000 <= cap, so exactly a and b go.
        let policy = RetentionPolicy {
            retention: None,
            max_total_bytes: Some(4500),
        };
        let report = cleanup(root, &policy, now);
        assert_eq!(report.deleted_for_size, 2);
        assert!(!a.exists(), "oldest rotated file goes first");
        assert!(!b.exists(), "second oldest goes next");
        assert!(c.exists());
        assert!(d.exists());
        assert!(active_srv.exists(), "active file never deleted");
        assert!(active_bus.exists(), "active file never deleted");
        assert_eq!(report.total_bytes_after, 4000);
        assert!(!report.over_cap_with_active);
    }

    #[test]
    fn total_cap_reports_when_only_active_files_exceed_cap() {
        let dir = tmp();
        let root = dir.path();
        let active = root.join("server/server.log");
        write_file(&active, 5000);
        let policy = RetentionPolicy {
            retention: None,
            max_total_bytes: Some(1000),
        };
        let report = cleanup(root, &policy, SystemTime::now());
        assert!(active.exists());
        assert!(report.over_cap_with_active);
        assert_eq!(report.deleted_for_size, 0);
        assert_eq!(report.total_bytes_after, 5000);
    }

    #[test]
    fn cleanup_on_missing_root_is_noop() {
        let dir = tmp();
        let report = cleanup(
            &dir.path().join("does-not-exist"),
            &RetentionPolicy {
                retention: Some(Duration::from_secs(1)),
                max_total_bytes: Some(1),
            },
            SystemTime::now(),
        );
        assert_eq!(report, CleanupReport::default());
    }

    #[test]
    fn policy_from_config_maps_units() {
        let p = RetentionPolicy::from_config(&LoggingConfig::default());
        assert_eq!(p.retention, Some(Duration::from_secs(14 * 86_400)));
        assert_eq!(p.max_total_bytes, Some(500 * MB));
    }

    // ---- formatting & redaction ---------------------------------------------

    #[test]
    fn utc_formatting_known_values() {
        assert_eq!(format_utc_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_utc_ms(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        // leap day
        assert_eq!(format_utc_ms(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn redacts_credentials_and_keeps_other_fields() {
        let mut v = json!({
            "user": "alice",
            "password": "hunter2",
            "meta": {
                "api_key": "k-123",
                "session_token": "s-456",
                "admin_token": "a-789",
                "login": {"u": "alice", "p": "x"},
                "node_id": "gui-1"
            },
            "items": [{"token": "z"}, {"name": "ok"}],
            "authz": "Bearer abc.def",
            "note": "plain text"
        });
        redact_payload(&mut v);
        assert_eq!(v["user"], "alice");
        assert_eq!(v["password"], REDACTED);
        assert_eq!(v["meta"]["api_key"], REDACTED);
        assert_eq!(v["meta"]["session_token"], REDACTED);
        assert_eq!(v["meta"]["admin_token"], REDACTED);
        assert_eq!(v["meta"]["login"], REDACTED);
        assert_eq!(v["meta"]["node_id"], "gui-1");
        assert_eq!(v["items"][0]["token"], REDACTED);
        assert_eq!(v["items"][1]["name"], "ok");
        assert_eq!(v["authz"], REDACTED);
        assert_eq!(v["note"], "plain text");
    }

    #[test]
    fn redaction_key_matching() {
        for k in [
            "password",
            "Password",
            "API-KEY",
            "apikey",
            "session_token",
            "secret",
            "Authorization",
            "key",
            "session",
        ] {
            assert!(is_sensitive_key(k), "{k} should be sensitive");
        }
        for k in [
            "username",
            "user",
            "display_name",
            "node_id",
            "keys_count_label",
            "topic",
        ] {
            assert!(!is_sensitive_key(k), "{k} should not be sensitive");
        }
    }

    #[test]
    fn redaction_leaves_scalars_untouched() {
        let mut v = json!(42);
        redact_payload(&mut v);
        assert_eq!(v, json!(42));
        let mut n = Value::Null;
        redact_payload(&mut n);
        assert_eq!(n, Value::Null);
    }

    #[test]
    fn bus_line_has_expected_shape_and_is_redacted() {
        let msg = BusMessage {
            id: 7,
            source: "core".into(),
            topic: "auth.login".into(),
            payload: json!({"user": "alice", "password": "hunter2"}),
            timestamp_ms: 1_700_000_000_123,
        };
        let line = bus_record_line(&msg);
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "exactly one line");
        assert!(!line.contains("hunter2"));
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        let obj = parsed.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["id", "payload", "source", "timestamp", "topic"]);
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["source"], "core");
        assert_eq!(parsed["topic"], "auth.login");
        assert_eq!(parsed["timestamp"], "2023-11-14T22:13:20.123Z");
        assert_eq!(parsed["payload"]["user"], "alice");
        assert_eq!(parsed["payload"]["password"], REDACTED);
    }

    #[test]
    fn bus_line_payload_with_newlines_stays_one_line() {
        let msg = BusMessage {
            id: 1,
            source: "s".into(),
            topic: "t".into(),
            payload: json!({"text": "a\nb"}),
            timestamp_ms: 0,
        };
        let line = bus_record_line(&msg);
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn server_line_format() {
        let record = log::Record::builder()
            .args(format_args!("hello {}", 1))
            .level(log::Level::Warn)
            .target("bytehive::x")
            .build();
        let line = format_server_line(0, &record);
        assert_eq!(
            line,
            "1970-01-01T00:00:00.000Z WARN  [bytehive::x] hello 1\n"
        );
    }

    // ---- bus logger end-to-end -------------------------------------------------

    #[test]
    fn bus_logger_captures_burst_losslessly_and_writes_jsonl() {
        let dir = tmp();
        let bus = MessageBus::new();
        let cfg = LoggingConfig {
            max_file_mb: 0,
            ..Default::default()
        };
        let handle = spawn_bus_logger(&bus, dir.path(), &cfg).unwrap();
        let n = 5_000u64;
        for i in 0..n {
            bus.publish(
                "src",
                "filesync.file_changed",
                json!({"i": i, "password": "p"}),
            );
        }
        drop(bus); // disconnects the logger's subscription -> thread exits
        handle.join().unwrap();

        let text = read(&bus_log_path(dir.path()));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len() as u64, n);
        for (idx, l) in lines.iter().enumerate() {
            let v: Value = serde_json::from_str(l).expect("valid json line");
            assert_eq!(v["id"].as_u64().unwrap(), idx as u64 + 1);
            assert_eq!(v["payload"]["password"], REDACTED);
        }
    }
}
