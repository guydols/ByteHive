use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::types::{DhatAllocSite, DhatSummary};

// ─── Output-file discovery ───────────────────────────────────────────────────

/// Polls for the DHAT output file at exactly `path`.
/// (`valgrind --tool=dhat --dhat-out-file=<path>` writes to the exact path given.)
pub fn wait_for_output(path: &Path, timeout: Duration) -> Option<PathBuf> {
    let start = Instant::now();
    loop {
        if path.exists() && path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            // Give valgrind a moment to finish flushing.
            thread::sleep(Duration::from_millis(500));
            eprintln!("[dhat] output file ready: {}", path.display());
            return Some(path.to_path_buf());
        }
        if start.elapsed() >= timeout {
            eprintln!("[dhat] timed out waiting for output at {}", path.display());
            return None;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

// ─── DHAT JSON parser ────────────────────────────────────────────────────────

/// Parses a DHAT JSON output file and returns a `DhatSummary`.
///
/// DHAT JSON v2 format key fields:
/// - `cmd`   — the profiled command string
/// - `pid`   — profiled PID
/// - `pps`   — array of "program points" (one per unique allocation call-stack)
///   - `tb`  — total bytes allocated over the entire run
///   - `tbk` — total blocks (= alloc calls) over the entire run
///   - `mb`  — max bytes live at any one time (peak live bytes for this site)
///   - `mbk` — max blocks live at any one time
///   - `rb`  — bytes read from blocks allocated at this site
///   - `wb`  — bytes written to blocks at this site
///   - `eb`  — "evil" / never-accessed bytes
///   - `fs`  — array of indices into `ftbl` (innermost frame first)
/// - `ftbl`  — frame table: array of human-readable frame strings
pub fn parse(path: &Path) -> DhatSummary {
    eprintln!("[dhat] parsing {}", path.display());

    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[dhat] failed to read output file: {e}");
            return DhatSummary::error(path, format!("read error: {e}"));
        }
    };

    parse_json(path, &data)
}

fn parse_json(path: &Path, data: &str) -> DhatSummary {
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[dhat] JSON parse error: {e}");
            return DhatSummary::error(path, format!("JSON parse error: {e}"));
        }
    };

    let command = v["cmd"].as_str().map(|s| s.to_string());
    let pid = v["pid"].as_u64();

    // Frame table — maps index → human-readable frame string.
    let ftbl: Vec<String> = v["ftbl"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|f| f.as_str().unwrap_or("??").to_string())
                .collect()
        })
        .unwrap_or_default();

    let pps = match v["pps"].as_array() {
        Some(a) => a,
        None => {
            return DhatSummary::error(path, "no 'pps' array in DHAT JSON".to_string());
        }
    };

    let mut all_sites: Vec<DhatAllocSite> = Vec::with_capacity(pps.len());
    let mut grand_total_bytes: u64 = 0;
    let mut grand_total_blocks: u64 = 0;

    for pp in pps {
        let tb = pp["tb"].as_u64().unwrap_or(0);
        let tbk = pp["tbk"].as_u64().unwrap_or(0);
        let mb = pp["mb"].as_u64().unwrap_or(0);
        let mbk = pp["mbk"].as_u64().unwrap_or(0);
        let rb = pp["rb"].as_u64().unwrap_or(0);
        let wb = pp["wb"].as_u64().unwrap_or(0);
        let eb = pp["eb"].as_u64().unwrap_or(0);

        grand_total_bytes += tb;
        grand_total_blocks += tbk;

        // Resolve call-stack frames.
        let frames: Vec<String> = pp["fs"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|i| i.as_u64())
                    .filter_map(|i| ftbl.get(i as usize))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        all_sites.push(DhatAllocSite {
            total_bytes: tb,
            total_blocks: tbk,
            peak_bytes: mb,
            peak_blocks: mbk,
            bytes_read: rb,
            bytes_written: wb,
            bytes_never_accessed: eb,
            frames,
        });
    }

    // Sort by peak_bytes descending so the biggest live consumers come first.
    all_sites.sort_by(|a, b| b.peak_bytes.cmp(&a.peak_bytes));

    // The total "peak" is NOT the sum of individual site peaks (they don't
    // all peak at the same moment). Report the maximum single-site peak and
    // the grand totals separately.
    let max_site_peak_bytes = all_sites.first().map(|s| s.peak_bytes).unwrap_or(0);

    // Keep only the top 25 sites to avoid a huge report.
    let top_sites = all_sites.into_iter().take(25).collect();

    DhatSummary {
        output_path: path.to_path_buf(),
        command,
        pid,
        total_bytes: grand_total_bytes,
        total_blocks: grand_total_blocks,
        max_site_peak_bytes,
        top_sites,
        parse_error: None,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    // ── DhatSummary::error ───────────────────────────────────────────────────

    #[test]
    fn dhat_summary_error_constructor_sets_all_fields() {
        let p = Path::new("/tmp/fake.json");
        let s = DhatSummary::error(p, "test error".to_string());
        assert_eq!(s.parse_error.as_deref(), Some("test error"));
        assert_eq!(s.total_bytes, 0);
        assert_eq!(s.total_blocks, 0);
        assert_eq!(s.max_site_peak_bytes, 0);
        assert!(s.top_sites.is_empty());
        assert!(s.command.is_none());
        assert!(s.pid.is_none());
        assert_eq!(s.output_path, p);
    }

    // ── parse_json ───────────────────────────────────────────────────────────

    #[test]
    fn parse_json_valid_minimal() {
        let p = Path::new("/tmp/test.json");
        let json = r#"{
            "cmd": "test-cmd",
            "pid": 1234,
            "ftbl": ["frame_a", "frame_b"],
            "pps": [
                {"tb": 100, "tbk": 5, "mb": 50, "mbk": 2, "rb": 80, "wb": 90, "eb": 10, "fs": [0, 1]}
            ]
        }"#;
        let s = parse_json(p, json);
        assert!(s.parse_error.is_none());
        assert_eq!(s.command.as_deref(), Some("test-cmd"));
        assert_eq!(s.pid, Some(1234));
        assert_eq!(s.total_bytes, 100);
        assert_eq!(s.total_blocks, 5);
        assert_eq!(s.max_site_peak_bytes, 50);
        assert_eq!(s.top_sites.len(), 1);
        assert_eq!(
            s.top_sites[0].frames,
            vec!["frame_a".to_string(), "frame_b".to_string()]
        );
        assert_eq!(s.top_sites[0].bytes_read, 80);
        assert_eq!(s.top_sites[0].bytes_written, 90);
        assert_eq!(s.top_sites[0].bytes_never_accessed, 10);
    }

    #[test]
    fn parse_json_no_pps_returns_error() {
        let p = Path::new("/tmp/test.json");
        let json = r#"{"cmd": "test", "ftbl": []}"#;
        let s = parse_json(p, json);
        assert!(s.parse_error.is_some());
    }

    #[test]
    fn parse_json_invalid_json_returns_error() {
        let p = Path::new("/tmp/test.json");
        let s = parse_json(p, "not valid json{{{");
        assert!(s.parse_error.is_some());
    }

    #[test]
    fn parse_json_empty_pps_returns_empty_summary() {
        let p = Path::new("/tmp/test.json");
        let json = r#"{"ftbl": [], "pps": []}"#;
        let s = parse_json(p, json);
        assert!(s.parse_error.is_none());
        assert_eq!(s.total_bytes, 0);
        assert_eq!(s.total_blocks, 0);
        assert!(s.top_sites.is_empty());
        assert_eq!(s.max_site_peak_bytes, 0);
    }

    #[test]
    fn parse_json_sorts_by_peak_bytes_descending() {
        let p = Path::new("/tmp/test.json");
        let json = r#"{
            "ftbl": [],
            "pps": [
                {"tb": 10, "tbk": 1, "mb": 5,  "mbk": 1, "rb": 0, "wb": 0, "eb": 0, "fs": []},
                {"tb": 20, "tbk": 1, "mb": 50, "mbk": 1, "rb": 0, "wb": 0, "eb": 0, "fs": []},
                {"tb": 5,  "tbk": 1, "mb": 10, "mbk": 1, "rb": 0, "wb": 0, "eb": 0, "fs": []}
            ]
        }"#;
        let s = parse_json(p, json);
        assert!(s.parse_error.is_none());
        assert_eq!(s.top_sites[0].peak_bytes, 50);
        assert_eq!(s.top_sites[1].peak_bytes, 10);
        assert_eq!(s.top_sites[2].peak_bytes, 5);
    }

    #[test]
    fn parse_json_top_sites_capped_at_25() {
        let p = Path::new("/tmp/test.json");
        let pps_entries: Vec<String> = (0u64..30)
            .map(|i| {
                format!(
                    r#"{{"tb": {i}, "tbk": 1, "mb": {i}, "mbk": 1, "rb": 0, "wb": 0, "eb": 0, "fs": []}}"#
                )
            })
            .collect();
        let json = format!(r#"{{"ftbl": [], "pps": [{}]}}"#, pps_entries.join(","));
        let s = parse_json(p, &json);
        assert!(s.parse_error.is_none());
        assert_eq!(s.top_sites.len(), 25);
    }

    #[test]
    fn parse_json_accumulates_grand_totals_correctly() {
        let p = Path::new("/tmp/test.json");
        let json = r#"{
            "ftbl": [],
            "pps": [
                {"tb": 100, "tbk": 3, "mb": 40, "mbk": 1, "rb": 0, "wb": 0, "eb": 0, "fs": []},
                {"tb": 200, "tbk": 7, "mb": 20, "mbk": 2, "rb": 0, "wb": 0, "eb": 0, "fs": []}
            ]
        }"#;
        let s = parse_json(p, json);
        assert!(s.parse_error.is_none());
        assert_eq!(s.total_bytes, 300);
        assert_eq!(s.total_blocks, 10);
        // max_site_peak_bytes is the largest single-site peak (40), not the sum
        assert_eq!(s.max_site_peak_bytes, 40);
    }

    #[test]
    fn parse_json_missing_optional_cmd_and_pid() {
        let p = Path::new("/tmp/test.json");
        let json = r#"{"ftbl": [], "pps": []}"#;
        let s = parse_json(p, json);
        assert!(s.parse_error.is_none());
        assert!(s.command.is_none());
        assert!(s.pid.is_none());
    }

    // ── wait_for_output ──────────────────────────────────────────────────────

    #[test]
    fn wait_for_output_returns_none_when_file_missing() {
        let p = Path::new("/tmp/definitely_no_such_dhat_file_xyz_bench_test.json");
        let result = wait_for_output(p, std::time::Duration::from_millis(300));
        assert!(result.is_none());
    }

    #[test]
    fn wait_for_output_returns_none_for_empty_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("empty_dhat.json");
        std::fs::write(&p, b"").unwrap();
        // Empty file (len == 0) should NOT be considered ready
        let result = wait_for_output(&p, std::time::Duration::from_millis(300));
        assert!(result.is_none());
    }

    #[test]
    fn wait_for_output_finds_existing_non_empty_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("dhat.json");
        std::fs::write(&p, b"{}").unwrap();
        // wait_for_output sleeps 500 ms after finding the file, so give it 2 s
        let result = wait_for_output(&p, std::time::Duration::from_secs(2));
        assert!(result.is_some());
        assert_eq!(result.unwrap(), p);
    }
}
