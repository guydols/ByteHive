use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ─── DHAT allocation site ────────────────────────────────────────────────────

/// One "program point" from a DHAT JSON output file — a unique allocation
/// call-stack and the aggregate metrics for all allocations made from it.
#[derive(Clone, Debug)]
pub struct DhatAllocSite {
    /// Total bytes allocated from this site over the entire run.
    pub total_bytes: u64,
    /// Total allocation calls (blocks) from this site over the entire run.
    pub total_blocks: u64,
    /// Maximum bytes live at any one time from this site (peak live bytes).
    pub peak_bytes: u64,
    /// Maximum blocks live at any one time from this site.
    pub peak_blocks: u64,
    /// Total bytes read from blocks allocated at this site.
    pub bytes_read: u64,
    /// Total bytes written to blocks allocated at this site.
    pub bytes_written: u64,
    /// Bytes that were allocated but never read or written ("evil" bytes).
    pub bytes_never_accessed: u64,
    /// Call-stack frames from innermost (allocation site) to outermost caller.
    pub frames: Vec<String>,
}

// ─── DHAT run summary ────────────────────────────────────────────────────────

/// Summary parsed from a DHAT JSON output file produced by
/// `valgrind --tool=dhat`.
#[derive(Clone, Debug)]
pub struct DhatSummary {
    /// Path to the DHAT JSON file on disk.
    pub output_path: std::path::PathBuf,
    /// The profiled command string (from `cmd` in the JSON).
    pub command: Option<String>,
    /// Profiled PID (from `pid` in the JSON).
    pub pid: Option<u64>,
    /// Grand total bytes allocated across all sites over the entire run.
    pub total_bytes: u64,
    /// Grand total allocation calls (blocks) across all sites.
    pub total_blocks: u64,
    /// Peak bytes for the single largest allocation site (largest `mb` value).
    pub max_site_peak_bytes: u64,
    /// Top 25 allocation sites sorted by peak live bytes descending.
    pub top_sites: Vec<DhatAllocSite>,
    /// Set when parsing failed; describes the error.
    pub parse_error: Option<String>,
}

impl DhatSummary {
    /// Construct an empty/error summary.
    pub fn error(path: &std::path::Path, msg: String) -> Self {
        Self {
            output_path: path.to_path_buf(),
            command: None,
            pid: None,
            total_bytes: 0,
            total_blocks: 0,
            max_site_peak_bytes: 0,
            top_sites: vec![],
            parse_error: Some(msg),
        }
    }
}

// ─── Per-thread CPU sample ──────────────────────────────────────────────────

/// A single per-thread CPU sample captured from /proc/<pid>/task/<tid>/.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadSample {
    /// Thread name (from /proc/<pid>/task/<tid>/comm).
    pub name: String,
    /// Total CPU usage percentage (user + system) since last sample.
    pub cpu_percent: f64,
    /// User-mode CPU usage percentage since last sample.
    pub user_cpu_percent: f64,
    /// Kernel-mode CPU usage percentage since last sample.
    pub sys_cpu_percent: f64,
}

// ─── Per-process metric sample ──────────────────────────────────────────────

/// A single point-in-time sample of a process's resource usage,
/// collected by reading /proc/<pid>/ for an external process.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessSample {
    /// Seconds elapsed since benchmark start.
    pub elapsed_secs: f64,

    // ── CPU ──────────────────────────────────────────────────────────────
    /// Total CPU usage percentage (user + system) since last sample.
    /// This is relative to a single core — values above 100% indicate
    /// multi-core usage.
    pub cpu_percent: f64,
    /// User-mode CPU percentage since last sample.
    pub user_cpu_percent: f64,
    /// Kernel/system-mode CPU percentage since last sample.
    pub sys_cpu_percent: f64,

    // ── Memory ───────────────────────────────────────────────────────────
    /// Resident Set Size in bytes (VmRSS from /proc/<pid>/status).
    pub rss_bytes: u64,
    /// Virtual memory size in bytes (VmSize from /proc/<pid>/status).
    pub vm_size_bytes: u64,
    /// Shared memory in bytes (RssFile + RssShmem from /proc/<pid>/status).
    pub shared_bytes: u64,
    /// Private (anonymous) memory in bytes (rss - shared).
    pub private_bytes: u64,

    // ── Threads ──────────────────────────────────────────────────────────
    /// Number of threads in the process (Threads from /proc/<pid>/status).
    pub thread_count: u32,
    /// Per-thread CPU breakdown.
    pub threads: Vec<ThreadSample>,

    // ── Disk I/O ─────────────────────────────────────────────────────────
    /// Cumulative bytes read from disk (read_bytes from /proc/<pid>/io).
    pub io_read_bytes: u64,
    /// Cumulative bytes written to disk (write_bytes from /proc/<pid>/io).
    pub io_write_bytes: u64,

    // ── Network (loopback) ───────────────────────────────────────────────
    /// Cumulative bytes received on the loopback interface.
    pub net_rx_bytes: u64,
    /// Cumulative bytes transmitted on the loopback interface.
    pub net_tx_bytes: u64,
}

// ─── Benchmark events ───────────────────────────────────────────────────────

/// A timestamped event that occurred during the benchmark.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub elapsed_secs: f64,
    pub kind: EventKind,
}

/// The kind of benchmark event.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    PhaseStart(String),
    PhaseEnd(String),
    FilesCreated {
        count: usize,
        total_bytes: u64,
    },
    FilesModified {
        count: usize,
        total_bytes: u64,
    },
    FilesDeleted {
        count: usize,
    },
    IntegrityCheck {
        phase: String,
        passed: bool,
        matched: usize,
        mismatched: usize,
        missing: usize,
        extra: usize,
    },
    SyncWaitComplete {
        duration_secs: f64,
    },
    Info(String),
}

// ─── Workload statistics ────────────────────────────────────────────────────

/// Statistics returned by a workload generator.
#[derive(Clone, Debug, Default)]
pub struct WorkloadStats {
    pub files_created: usize,
    pub files_modified: usize,
    pub files_deleted: usize,
    pub bytes_written: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── IntegrityResult::passed ───────────────────────────────────────────────

    #[test]
    fn integrity_result_passed_all_clean() {
        let r = IntegrityResult {
            matched: 5,
            mismatched: vec![],
            missing_from_dest: vec![],
            extra_in_dest: vec![],
        };
        assert!(r.passed());
    }

    #[test]
    fn integrity_result_fails_on_mismatch() {
        let r = IntegrityResult {
            matched: 3,
            mismatched: vec![PathBuf::from("bad.txt")],
            missing_from_dest: vec![],
            extra_in_dest: vec![],
        };
        assert!(!r.passed());
    }

    #[test]
    fn integrity_result_fails_on_missing() {
        let r = IntegrityResult {
            matched: 3,
            mismatched: vec![],
            missing_from_dest: vec![PathBuf::from("missing.txt")],
            extra_in_dest: vec![],
        };
        assert!(!r.passed());
    }

    #[test]
    fn integrity_result_passes_with_extra_only() {
        // extra_in_dest does NOT cause a failure — passed() only checks
        // mismatched and missing_from_dest
        let r = IntegrityResult {
            matched: 3,
            mismatched: vec![],
            missing_from_dest: vec![],
            extra_in_dest: vec![PathBuf::from("extra.txt")],
        };
        assert!(r.passed());
    }

    #[test]
    fn integrity_result_fails_on_both_mismatch_and_missing() {
        let r = IntegrityResult {
            matched: 1,
            mismatched: vec![PathBuf::from("bad.txt")],
            missing_from_dest: vec![PathBuf::from("gone.txt")],
            extra_in_dest: vec![],
        };
        assert!(!r.passed());
    }

    #[test]
    fn integrity_result_zero_matched_and_all_empty_passes() {
        let r = IntegrityResult {
            matched: 0,
            mismatched: vec![],
            missing_from_dest: vec![],
            extra_in_dest: vec![],
        };
        assert!(r.passed());
    }

    // ── WorkloadStats ─────────────────────────────────────────────────────────

    #[test]
    fn workload_stats_default_is_all_zero() {
        let s = WorkloadStats::default();
        assert_eq!(s.files_created, 0);
        assert_eq!(s.files_modified, 0);
        assert_eq!(s.files_deleted, 0);
        assert_eq!(s.bytes_written, 0);
    }

    #[test]
    fn workload_stats_fields_are_independent() {
        let s = WorkloadStats {
            files_created: 10,
            files_modified: 5,
            files_deleted: 3,
            bytes_written: 1024,
        };
        assert_eq!(s.files_created, 10);
        assert_eq!(s.files_modified, 5);
        assert_eq!(s.files_deleted, 3);
        assert_eq!(s.bytes_written, 1024);
    }
}

// ─── Integrity result ───────────────────────────────────────────────────────

/// Result of an integrity check between server and client directories.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntegrityResult {
    pub matched: usize,
    pub mismatched: Vec<PathBuf>,
    pub missing_from_dest: Vec<PathBuf>,
    pub extra_in_dest: Vec<PathBuf>,
}

impl IntegrityResult {
    pub fn passed(&self) -> bool {
        self.mismatched.is_empty() && self.missing_from_dest.is_empty()
    }
}

// ─── Log capture ────────────────────────────────────────────────────────────

/// A single log line captured from a server or client subprocess.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogLine {
    /// Seconds since benchmark start when this line was received.
    pub elapsed_secs: f64,
    /// Origin of the log line: "server" or "client".
    pub source: String,
    /// Parsed log level: "ERROR", "WARN", "INFO", "DEBUG", "TRACE", or "INFO" for unrecognised prefixes.
    pub level: String,
    /// The full original log line text.
    pub message: String,
}
