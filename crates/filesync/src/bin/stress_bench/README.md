# ByteHive FileSync — Stress Benchmark

A headless, long-running stress benchmark for the filesync server/client that
pushes file synchronisation to the limit and verifies transfer integrity while
capturing **per-process** CPU, memory, disk I/O, and network metrics.

## Architecture

The benchmark uses a **separate-process** architecture so that server and client
resource usage is measured independently and the workload-generation overhead of
the orchestrator process is excluded from both:

```
┌─────────────────────────────────────────────┐
│  Orchestrator (main process)                │
│  • Creates / modifies / deletes files       │
│  • Monitors child PIDs via /proc/<pid>/     │
│  • Runs integrity checks (BLAKE3)           │
│  • Generates the HTML report                │
├──────────────────┬──────────────────────────┤
│  Server process  │  Client process          │
│  (subprocess)    │  (subprocess)            │
│  • Runs Server   │  • Runs Client           │
│  • inotify watch │  • inotify watch         │
│  • TLS listener  │  • TLS connection        │
└──────────────────┴──────────────────────────┘
```

The binary supports hidden subprocess modes:
- `stress-bench __server <dir> <port-file>` — server mode
- `stress-bench __client <dir> <server-addr>` — client mode

These are invoked automatically by the orchestrator; you never need to call them
directly.

## Building

```sh
# Debug (faster compile, slower run)
cargo build --bin stress-bench

# Release (recommended for real benchmarks)
cargo build --bin stress-bench --release
```

## Running

```sh
# Quick smoke test (~1 min)
./target/release/stress-bench \
  --duration 1 \
  --small-files 100 \
  --large-files 2 \
  --large-file-size 10 \
  --output ./bench_output

# Medium benchmark (~10 min)
./target/release/stress-bench \
  --duration 10 \
  --small-files 3000 \
  --large-files 5 \
  --large-file-size 50 \
  --output ./bench_output

# Full stress test (~30 min, default)
./target/release/stress-bench \
  --output ./bench_output

# Heavy stress test (~60 min)
./target/release/stress-bench \
  --duration 60 \
  --scale 2.0 \
  --large-file-size 200 \
  --output ./bench_output
```

## CLI Options

| Flag | Default | Description |
|------|---------|-------------|
| `--duration` | `30` | Total benchmark duration in minutes |
| `--scale` | `1.0` | Scale factor for all workload counts |
| `--output` | `./bench_output` | Output directory for the HTML report |
| `--small-files` | `5000` | Number of small files (1KB–100KB) in the flood phase |
| `--large-files` | `5` | Number of large files to transfer |
| `--large-file-size` | `100` | Target size of each large file in MB |
| `--mixed-small` | `2000` | Small files in the mixed-burst phase |
| `--mixed-large` | `3` | Large files in the mixed-burst phase |
| `--modify-count` | `1000` | Files to modify in the modification storm |
| `--delete-count` | `500` | Files to delete |
| `--recreate-count` | `500` | Files to recreate after deletion |
| `--sustained-tick-interval` | `3` | Seconds between sustained-load ticks |
| `--sync-timeout` | `300` | Sync wait timeout per phase in seconds |
| `--keep-dirs` | `false` | Keep temp directories after benchmark |
| `--dhat` | `false` | Profile server & client heap with DHAT via Valgrind (see below) |

## Benchmark Phases

1. **Small File Flood** — Creates thousands of small files (1KB–100KB) in rapid
   succession to stress the bundling pipeline and inotify watcher.

2. **Large File Transfer** — Creates several large files (multi-MB) to exercise
   the chunked large-file transfer protocol (LargeFileStart/Chunk/End).

3. **Mixed Burst** — Interleaves small and large file creation with a nested
   directory structure.

4. **Modification Storm** — Overwrites existing files with new content to test
   change detection and re-sync.

5. **Delete & Recreate** — Deletes files and creates new ones to stress delete
   propagation.

6. **Sustained Mixed Load** — Continuously creates files at a steady rate for
   the remaining benchmark duration, with periodic integrity checks.

After each phase, the orchestrator waits for the sync to propagate and runs a
full BLAKE3 integrity check comparing the server and client directories.

## Metrics Collected

All metrics are collected **per-process** by reading `/proc/<pid>/` for each
child process. The orchestrator's own resource usage (file creation, hashing) is
**excluded** from both server and client measurements.

### Per-Process Metrics (sampled every 1s)

| Metric | Source | Detail |
|--------|--------|--------|
| **CPU %** | `/proc/<pid>/stat` | Total, user-mode, and system/kernel-mode breakdown |
| **Memory** | `/proc/<pid>/status` | RSS, private (anonymous), shared (file-backed + shmem), virtual size |
| **Thread count** | `/proc/<pid>/status` | Number of OS threads |
| **Per-thread CPU** | `/proc/<pid>/task/<tid>/stat` + `comm` | CPU % attributed to each named thread |
| **Disk I/O** | `/proc/<pid>/io` | Cumulative read/write bytes (actual disk I/O) |
| **Network** | `/proc/net/dev` | Loopback interface RX/TX bytes |

### Thread Name → Code Path Mapping

The per-thread CPU breakdown lets you see which parts of the code are consuming
resources. Thread names map to code paths as follows:

**Server threads:**
| Thread Name | Code Path |
|-------------|-----------|
| `server-main` | `Server::run_with_listener()` — accept loop |
| `srv-watcher-bro…` | `local_change_broadcaster()` — processes inotify events |
| `srv-client-127.…` | `handle_client()` — per-client connection handler |
| `srv-recv-bench-…` | `client_recv_loop()` — receives messages from client |
| `inotify-watcher` | inotify event reading thread |
| `tls-io` | TLS I/O background thread |

**Client threads:**
| Thread Name | Code Path |
|-------------|-----------|
| `client-main` | `Client::run()` → `Client::session()` — main sync loop |
| `recv-srv` | `recv_loop()` — receives messages from server |
| `inotify-watcher` | inotify event reading thread |
| `tls-io` | TLS I/O background thread |

## Report

The benchmark generates a self-contained HTML report (`report.html`) with
interactive Chart.js charts. The report includes:

1. **Summary Cards** — Overall stats (duration, files, data) and side-by-side
   server vs. client peak metrics (CPU, memory, threads, I/O rate)

2. **Server CPU Usage** — Total, user, and system CPU % over time with phase
   overlay annotations

3. **Client CPU Usage** — Same structure for the client process

4. **Server Memory** — RSS, private, and shared memory over time

5. **Client Memory** — Same structure for the client process

6. **Server Thread CPU Breakdown** — Per-thread CPU % showing which threads
   (code paths) are consuming CPU at any point in time

7. **Client Thread CPU Breakdown** — Same for client threads

8. **Disk I/O** — Read/write rates (MB/s) for both server and client

9. **Network Throughput** — Loopback TX/RX rates (MB/s)

10. **Events Timeline** — Timestamped table of all benchmark events

11. **Integrity Results** — Pass/fail status for each phase's BLAKE3 check

All charts include:
- Phase overlay boxes showing which benchmark phase was active
- Rich hover tooltips with cross-metric context and nearby events
- **Scroll-to-zoom** and **drag-to-pan** on the X axis (powered by
  `chartjs-plugin-zoom`); a **⟲ Reset zoom** button restores the full view

When `--dhat` is passed, the report gains two additional sections (one per
process) — see [Heap Profiling with DHAT](#heap-profiling-with-dhat) below.

---

## Heap Profiling with DHAT

Pass `--dhat` to wrap the server and client subprocesses with
[Valgrind's DHAT tool](https://valgrind.org/docs/manual/dh-manual.html),
which instruments every heap allocation and tracks which call sites are
responsible for the most **live bytes at peak**.

DHAT answers the question: **"At the moment of maximum heap usage, which
variables and code paths are holding the most memory?"**

### ⚠ Overhead Warning

DHAT adds **~20–50× execution overhead** because Valgrind instruments every
memory operation. Use a short `--duration` when profiling:

```sh
./target/release/stress-bench \
  --duration 1 \
  --small-files 200 \
  --large-files 1 \
  --large-file-size 10 \
  --dhat \
  --output ./bench_output
```

**Requirements:** `valgrind` must be in `$PATH`.

### What it produces

| Artefact | Location | Description |
|----------|----------|-------------|
| Server DHAT data | `<output>/<run-id>/dhat/server.json` | Raw DHAT JSON (all allocation sites) |
| Client DHAT data | `<output>/<run-id>/dhat/client.json` | Raw DHAT JSON (all allocation sites) |
| Embedded report section | `report.html` | Top allocation-site breakdown |

### Report sections added

When `--dhat` is enabled the HTML report gains two extra sections — one for the
server process and one for the client — each containing:

- **Summary stat cards** — total bytes allocated, total allocation calls, and
  the peak live bytes for the single largest allocation site
- **Top Allocation Sites table** — the top 25 call sites sorted by
  **peak live bytes** (the maximum number of bytes simultaneously live from that
  site), with per-site columns for:
  - Peak live bytes (and its share of total allocations)
  - Total bytes allocated over the entire run
  - Bytes read / written
  - Bytes *never accessed* (allocated but never read or written — useful for
    spotting wasteful allocations)
  - Call stack (innermost frame first, with collapsible deeper frames)

### Viewing the full DHAT data

The `.json` files are standard DHAT output and can be loaded into the
[DHAT Viewer](https://nnethercote.github.io/dh_view/dh_view.html) — an
interactive browser-based UI that provides:

- **Total bytes** — aggregate allocation volume per site
- **Peak bytes** — live bytes at peak heap usage per site
- **Access patterns** — read/write byte histograms per block
- Sortable, filterable call-site tree

```
# 1. Open the DHAT Viewer in your browser:
https://nnethercote.github.io/dh_view/dh_view.html

# 2. Click "Load…" and select the JSON file:
./bench_output/<run-id>/dhat/server.json
./bench_output/<run-id>/dhat/client.json
```

### Workflow: finding what uses the most memory at peak

1. **Run a short benchmark with `--dhat`:**
   ```sh
   ./target/release/stress-bench --duration 1 --dhat --output ./bench_output
   ```

2. **Open `report.html`** and look at the memory chart — identify the timestamp
   of peak RSS. The DHAT sections below the charts show which allocation sites
   are sorted by peak live bytes, directly corresponding to that peak moment.

3. **Cross-reference with the per-thread CPU charts** — if a specific thread
   (e.g. `srv-watcher-bro…`) is active during the memory spike, and DHAT shows
   the largest allocation site traces back to the same code path, you have your
   culprit.

4. **Load the full `.json` in the DHAT Viewer** for interactive exploration of
   the complete call-tree, access histograms, and total vs. peak breakdowns.

---

## Platform Requirements

- **Linux only** — requires inotify and `/proc` filesystem
- Sufficient disk space for large-file tests (default: ~500 MB for large files)
- Sufficient inotify watch descriptors (`/proc/sys/fs/inotify/max_user_watches`)
- For DHAT profiling: `valgrind` installed and in `$PATH`

## Example Output

```
  Duration       : 62.2s
  Files created  : 264
  Data written   : 60.3 MB
  Server peak    : 3% CPU, 28.2 MB RSS
  Client peak    : 3% CPU, 22.1 MB RSS
  Integrity      : ✅ ALL PASSED
  Report         : ./bench_output/2024-01-15_14-30-00/report.html
```
