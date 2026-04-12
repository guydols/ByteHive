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
