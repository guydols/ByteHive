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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Event, EventKind, IntegrityResult, LogLine, ProcessSample, ThreadSample};
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::TempDir;

    fn dummy_sample(elapsed: f64) -> ProcessSample {
        ProcessSample {
            elapsed_secs: elapsed,
            cpu_percent: 0.0,
            user_cpu_percent: 0.0,
            sys_cpu_percent: 0.0,
            rss_bytes: 0,
            vm_size_bytes: 0,
            shared_bytes: 0,
            private_bytes: 0,
            thread_count: 1,
            threads: vec![],
            io_read_bytes: 0,
            io_write_bytes: 0,
            net_rx_bytes: 0,
            net_tx_bytes: 0,
        }
    }

    #[test]
    fn create_produces_a_valid_store() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        assert!(DataStore::create(&path).is_ok());
        assert!(path.exists());
    }

    #[test]
    fn load_empty_store() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        let store = DataStore::create(&path).unwrap();
        drop(store);
        let loaded = load(&path).unwrap();
        assert!(loaded.events.is_empty());
        assert!(loaded.server_samples.is_empty());
        assert!(loaded.client_samples.is_empty());
        assert!(loaded.server_logs.is_empty());
        assert!(loaded.client_logs.is_empty());
        assert!(!loaded.completed);
    }

    #[test]
    fn append_event_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        let store = DataStore::create(&path).unwrap();
        store.append(&DataRecord::Event {
            event: Event {
                elapsed_secs: 1.5,
                kind: EventKind::PhaseStart("alpha".to_string()),
            },
        });
        drop(store);
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.events.len(), 1);
        assert_eq!(loaded.events[0].elapsed_secs, 1.5);
    }

    #[test]
    fn append_complete_marks_run_as_done() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        let store = DataStore::create(&path).unwrap();
        store.append(&DataRecord::Complete {
            total_duration_secs: 42.0,
        });
        drop(store);
        let loaded = load(&path).unwrap();
        assert!(loaded.completed);
        assert_eq!(loaded.total_duration, Duration::from_secs_f64(42.0));
    }

    #[test]
    fn append_server_and_client_samples_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        let store = DataStore::create(&path).unwrap();
        store.append(&DataRecord::ServerSample {
            sample: dummy_sample(1.0),
        });
        store.append(&DataRecord::ClientSample {
            sample: dummy_sample(2.0),
        });
        drop(store);
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.server_samples.len(), 1);
        assert_eq!(loaded.client_samples.len(), 1);
        assert_eq!(loaded.server_samples[0].elapsed_secs, 1.0);
        assert_eq!(loaded.client_samples[0].elapsed_secs, 2.0);
    }

    #[test]
    fn append_logs_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        let store = DataStore::create(&path).unwrap();
        store.append(&DataRecord::ServerLog {
            line: LogLine {
                elapsed_secs: 0.5,
                source: "server".to_string(),
                level: "INFO".to_string(),
                message: "started".to_string(),
            },
        });
        store.append(&DataRecord::ClientLog {
            line: LogLine {
                elapsed_secs: 1.0,
                source: "client".to_string(),
                level: "DEBUG".to_string(),
                message: "connected".to_string(),
            },
        });
        drop(store);
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.server_logs.len(), 1);
        assert_eq!(loaded.client_logs.len(), 1);
        assert_eq!(loaded.server_logs[0].message, "started");
        assert_eq!(loaded.client_logs[0].message, "connected");
    }

    #[test]
    fn append_integrity_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        let store = DataStore::create(&path).unwrap();
        store.append(&DataRecord::Integrity {
            phase: "phase1".to_string(),
            result: IntegrityResult {
                matched: 10,
                mismatched: vec![],
                missing_from_dest: vec![],
                extra_in_dest: vec![PathBuf::from("extra.dat")],
            },
        });
        drop(store);
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.integrity_results.len(), 1);
        assert_eq!(loaded.integrity_results[0].0, "phase1");
        assert_eq!(loaded.integrity_results[0].1.matched, 10);
    }

    #[test]
    fn load_skips_malformed_lines() {
        use std::io::Write;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        let store = DataStore::create(&path).unwrap();
        store.append(&DataRecord::Event {
            event: Event {
                elapsed_secs: 0.1,
                kind: EventKind::Info("ok".to_string()),
            },
        });
        drop(store);
        // Append malformed lines after the valid record
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{{not valid json!!!}}").unwrap();
        writeln!(f, "garbage line with no json at all").unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.events.len(), 1);
    }

    #[test]
    fn load_duration_estimated_from_max_elapsed_when_incomplete() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        let store = DataStore::create(&path).unwrap();
        store.append(&DataRecord::ServerSample {
            sample: dummy_sample(99.5),
        });
        store.append(&DataRecord::ClientSample {
            sample: dummy_sample(50.0),
        });
        drop(store);
        let loaded = load(&path).unwrap();
        assert!(!loaded.completed);
        // Duration should be estimated from the max elapsed seen (99.5 s)
        assert!(loaded.total_duration.as_secs_f64() > 99.0);
    }

    #[test]
    fn meta_record_is_accepted_without_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        let store = DataStore::create(&path).unwrap();
        store.append(&DataRecord::Meta {
            run_id: "run-001".to_string(),
            command: "stress-bench --duration 1".to_string(),
        });
        drop(store);
        let loaded = load(&path).unwrap();
        // Meta records don't populate any output fields but must not cause errors
        assert!(loaded.events.is_empty());
    }

    #[test]
    fn multiple_events_preserve_order_and_count() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        let store = DataStore::create(&path).unwrap();
        for i in 0u32..5 {
            store.append(&DataRecord::Event {
                event: Event {
                    elapsed_secs: i as f64,
                    kind: EventKind::Info(format!("event {i}")),
                },
            });
        }
        drop(store);
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.events.len(), 5);
        for (i, ev) in loaded.events.iter().enumerate() {
            assert_eq!(ev.elapsed_secs, i as f64);
        }
    }

    #[test]
    fn create_truncates_existing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        // Write some data first
        {
            let store = DataStore::create(&path).unwrap();
            store.append(&DataRecord::Event {
                event: Event {
                    elapsed_secs: 1.0,
                    kind: EventKind::Info("first run".to_string()),
                },
            });
        }
        // Create again — should truncate the previous contents
        {
            let store = DataStore::create(&path).unwrap();
            store.append(&DataRecord::Complete {
                total_duration_secs: 5.0,
            });
        }
        let loaded = load(&path).unwrap();
        // Only the Complete record from the second run should be present
        assert!(loaded.events.is_empty());
        assert!(loaded.completed);
    }

    #[test]
    fn phase_end_event_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        let store = DataStore::create(&path).unwrap();
        store.append(&DataRecord::Event {
            event: Event {
                elapsed_secs: 3.0,
                kind: EventKind::PhaseEnd("large_files".to_string()),
            },
        });
        drop(store);
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.events.len(), 1);
        assert!(matches!(
            &loaded.events[0].kind,
            EventKind::PhaseEnd(name) if name == "large_files"
        ));
    }

    #[test]
    fn files_created_event_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.ndjson");
        let store = DataStore::create(&path).unwrap();
        store.append(&DataRecord::Event {
            event: Event {
                elapsed_secs: 5.0,
                kind: EventKind::FilesCreated {
                    count: 42,
                    total_bytes: 1_048_576,
                },
            },
        });
        drop(store);
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.events.len(), 1);
        assert!(matches!(
            &loaded.events[0].kind,
            EventKind::FilesCreated {
                count: 42,
                total_bytes: 1_048_576
            }
        ));
    }
}
