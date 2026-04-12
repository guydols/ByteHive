//! Crash-safe NDJSON data store for the stress benchmark.
//!
//! Every record written by the orchestrator, log reader threads, and metrics
//! sampler is appended to a single `data.ndjson` file in the run directory.
//! Because each line is a self-contained JSON object, the file stays readable
//! after a crash — the report generator skips any incomplete trailing line.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::types::{Event, IntegrityResult, LogLine, ProcessSample};

// ─── On-disk record ──────────────────────────────────────────────────────────

/// One NDJSON line written to `data.ndjson`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataRecord {
    /// Written once at the very start.
    Meta { run_id: String, command: String },
    /// A benchmark event (phase start/end, file ops, integrity check, …).
    Event { event: Event },
    /// Integrity comparison result for a named phase.
    Integrity {
        phase: String,
        result: IntegrityResult,
    },
    /// One server metrics sample.
    ServerSample { sample: ProcessSample },
    /// One client metrics sample.
    ClientSample { sample: ProcessSample },
    /// One server log line (from stderr **or** stdout of the server subprocess).
    ServerLog { line: LogLine },
    /// One client log line (from stderr **or** stdout of the client subprocess).
    ClientLog { line: LogLine },
    /// Written once on a clean shutdown; signals that the run finished normally.
    Complete { total_duration_secs: f64 },
}

// ─── Writer ──────────────────────────────────────────────────────────────────

/// Append-only NDJSON data store, safe to share across threads via `Arc`.
pub struct DataStore {
    file: Mutex<File>,
}

impl DataStore {
    /// Create (or truncate) the data store file at `path`.
    pub fn create(path: &Path) -> io::Result<Arc<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        Ok(Arc::new(Self {
            file: Mutex::new(file),
        }))
    }

    /// Append `record` as a single NDJSON line and flush immediately.
    ///
    /// Errors are silently swallowed — the benchmark must not abort because it
    /// cannot write to the data store.
    pub fn append(&self, record: &DataRecord) {
        let Ok(json) = serde_json::to_string(record) else {
            return;
        };
        if let Ok(mut guard) = self.file.lock() {
            let _ = writeln!(guard, "{json}");
            let _ = guard.flush();
        }
    }
}

// ─── Reader / loader ─────────────────────────────────────────────────────────

/// All data recovered from a `data.ndjson` file.
pub struct LoadedData {
    /// Total benchmark duration.  Estimated from the last timestamp when the
    /// `Complete` record is absent (i.e. the run crashed).
    pub total_duration: std::time::Duration,
    pub events: Vec<Event>,
    pub integrity_results: Vec<(String, IntegrityResult)>,
    pub server_samples: Vec<ProcessSample>,
    pub client_samples: Vec<ProcessSample>,
    pub server_logs: Vec<LogLine>,
    pub client_logs: Vec<LogLine>,
    /// `true` only when a `Complete` record was present (clean shutdown).
    pub completed: bool,
}

/// Load a data store file and reconstruct all benchmark data.
///
/// Incomplete / malformed lines — e.g. from a crash mid-write — are silently
/// skipped, so the load always succeeds for any prefix of a valid file.
pub fn load(path: &Path) -> io::Result<LoadedData> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut total_duration = std::time::Duration::ZERO;
    let mut completed = false;
    let mut events: Vec<Event> = Vec::new();
    let mut integrity_results: Vec<(String, IntegrityResult)> = Vec::new();
    let mut server_samples: Vec<ProcessSample> = Vec::new();
    let mut client_samples: Vec<ProcessSample> = Vec::new();
    let mut server_logs: Vec<LogLine> = Vec::new();
    let mut client_logs: Vec<LogLine> = Vec::new();
    let mut last_elapsed: f64 = 0.0;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record: DataRecord = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(_) => continue, // skip malformed / truncated lines
        };
        match record {
            DataRecord::Meta { .. } => {}
            DataRecord::Event { event } => {
                last_elapsed = last_elapsed.max(event.elapsed_secs);
                events.push(event);
            }
            DataRecord::Integrity { phase, result } => {
                integrity_results.push((phase, result));
            }
            DataRecord::ServerSample { sample } => {
                last_elapsed = last_elapsed.max(sample.elapsed_secs);
                server_samples.push(sample);
            }
            DataRecord::ClientSample { sample } => {
                last_elapsed = last_elapsed.max(sample.elapsed_secs);
                client_samples.push(sample);
            }
            DataRecord::ServerLog { line } => {
                last_elapsed = last_elapsed.max(line.elapsed_secs);
                server_logs.push(line);
            }
            DataRecord::ClientLog { line } => {
                last_elapsed = last_elapsed.max(line.elapsed_secs);
                client_logs.push(line);
            }
            DataRecord::Complete {
                total_duration_secs,
            } => {
                total_duration = std::time::Duration::from_secs_f64(total_duration_secs);
                completed = true;
            }
        }
    }

    // If the run crashed, estimate duration from the latest timestamp seen.
    if !completed && last_elapsed > 0.0 {
        total_duration = std::time::Duration::from_secs_f64(last_elapsed);
    }

    Ok(LoadedData {
        total_duration,
        events,
        integrity_results,
        server_samples,
        client_samples,
        server_logs,
        client_logs,
        completed,
    })
}
