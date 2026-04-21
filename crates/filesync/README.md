# filesync

Bidirectional real-time file synchronisation over mutual TLS 1.3.
Part of the [ByteHive](../../README.md) platform — runs as a framework app
or standalone via the `filesync-gui` binary.

| | |
|---|---|
| **Crate** | `bytehive-filesync` 0.1.0 (Rust 2021) |
| **Binaries** | `filesync-gui` (standalone GUI), `stress-bench` (benchmark suite) |
| **Nav icon** | 🗂 (U+1F5C2) |
| **HTTP prefix** | `/api/filesync` |
| **UI prefix** | `/apps/filesync` |

---

## Table of Contents

1. [Configuration](#configuration)
2. [Exclusion Engine](#exclusion-engine)
3. [TLS & Identity](#tls--identity)
4. [Trust Model](#trust-model)
5. [Wire Protocol](#wire-protocol)
6. [Sync Engine](#sync-engine)
7. [Filesystem Watcher](#filesystem-watcher)
8. [Transport Layer](#transport-layer)
9. [Server Architecture](#server-architecture)
10. [Client Architecture](#client-architecture)
11. [HTTP API](#http-api)
12. [Bus Events](#bus-events)
13. [Protocol Constants](#protocol-constants)
14. [Limitations](#limitations)

---

## Configuration

All settings live under `[apps.filesync]` in the main ByteHive `config.toml`:

```toml
[apps.filesync]
root      = "/path/to/folder"
mode      = "server"          # or "client"
bind_addr = "0.0.0.0:7878"   # server only
server_addr = "host:7878"    # client only

# Exclusion rules (optional)
exclude_patterns = ["*.log", "build/**", "**/.cache/**"]
exclude_regex    = ['.*\.(tmp|bak)$', '^private/']
```

| Key | Required | Side | Description |
|-----|----------|------|-------------|
| `root` | ✅ | both | Absolute path to the folder tree to sync |
| `mode` | ✅ | both | `"server"` or `"client"` |
| `bind_addr` | server | server | Address the server listens on |
| `server_addr` | client | client | Address the client connects to |
| `exclude_patterns` | — | both | Shell-style glob patterns to exclude |
| `exclude_regex` | — | both | Regex rules to exclude (matched against relative path) |

> **Note:** `auth_token` is a legacy field and is unused — identity is
> established via mutual TLS certificates.

---

## Exclusion Engine

The `ExclusionConfig` accepts two kinds of rules:

- **`exclude_patterns`** — shell-style globs. `**` matches across directories,
  `*` matches within a single segment, `?` matches one character. Regex
  metacharacters are escaped automatically.
- **`exclude_regex`** — full regular expressions matched against the relative
  path (always using forward-slash separators, even on Windows).

Built-in rules that are always active:

- `.bh_filesync`
- `.bh_filesync/**`

Every compiled rule carries a diagnostic label (`builtin:`, `glob:`, or
`regex:`) so that log output clearly indicates which rule excluded a path.

---

## TLS & Identity

- **TLS 1.3 only.** TLS 1.2 signatures are explicitly rejected by the server
  verifier.
- **Cipher suites:** `TLS_AES_256_GCM_SHA384`, `TLS_CHACHA20_POLY1305_SHA256`.
- **Mutual TLS is mandatory** — both server and client present ECDSA P-384
  self-signed certificates.
- Certificates are **persisted to disk** for stable identity across restarts:
  - Server: `{config_dir}/filesync/server.der`, `server.key.der`
  - Client: `{config_dir}/filesync/client.der`, `client.key.der`
- If TLS initialisation fails, an **ephemeral certificate** is generated as a
  fallback.
- Custom verifiers:
  - **`AcceptAnyCert`** — used client-side (server cert verified via TOFU
    pinning instead).
  - **`AcceptAnyClientCert`** — used server-side with
    `client_auth_mandatory = true` (client trust managed by the Known Clients
    system).

---

## Trust Model

### Server → Client: Known Clients

Every client that connects is tracked by the server in a **Known Clients**
registry persisted as `[[filesync_known_clients]]` sections inside the main
`config.toml` (using a splice mechanism that preserves unrelated config
content).

Each entry stores:

| Field | Description |
|-------|-------------|
| `node_id` | The client's self-reported node identifier |
| `fingerprint` | SHA-256 fingerprint of the client's TLS certificate |
| `label` | Optional human-friendly label |
| `status` | `Pending`, `Allowed`, or `Rejected` |
| `addr` | Last-seen socket address |
| `first_seen_ms` | Timestamp of first connection |
| `last_seen_ms` | Timestamp of most recent connection |

**Workflow:**

1. Unknown client connects → registered as **Pending**.
2. Bus event `filesync.client_approval_needed` is published.
3. Server sends the client an `ApprovalPending` message; the client will
   reconnect every **30 seconds** until approved.
4. An operator approves or rejects the client via the HTTP API.
5. Approved clients proceed to sync; rejected clients receive a `Rejected`
   message and back off for **300 seconds**.

### Client → Server: TOFU Pinning

Clients implement **Trust On First Use** for the server certificate:

- On first connection the server's certificate fingerprint is stored in
  `{identity_dir}/known_servers.toml`.
- Subsequent connections **must** match the pinned fingerprint; a mismatch
  aborts the connection with an error.

Each entry stores: `addr`, `fingerprint`, `first_seen_ms`.

---

## Wire Protocol

**Protocol version: 6.** The version must match exactly between peers.

### Framing

```text
┌──────────────────┬────────────────────────────────┐
│ 4 bytes (BE u32) │ LZ4-compressed bincode payload │
│   payload length │                                │
└──────────────────┴────────────────────────────────┘
```

Maximum frame size: **32 MiB** (`MAX_FRAME_BYTES`).

### Message Variants

The `Message` enum has **13 variants**:

| Variant | Direction | Purpose |
|---------|-----------|---------|
| `Hello` | both | Handshake — carries `node_id` and `protocol_version` |
| `ApprovalPending` | S → C | Client is pending operator approval |
| `Rejected` | S → C | Client has been rejected |
| `ManifestExchange` | both | Full file manifest for diff computation |
| `Bundle` | both | Batch of small files |
| `Delete` | both | File deletion |
| `Rename` | both | File rename |
| `SyncComplete` | both | Signals end of initial sync phase |
| `LargeFileStart` | both | Begin streaming a large file |
| `LargeFileChunk` | both | One chunk of a large file |
| `LargeFileEnd` | both | Finish streaming a large file |
| `RequestChunks` | both | Request retransmission of missing chunks |
| `InsufficientDiskSpace` | both | Peer lacks disk space to receive a file |

### Handshake Flow

```text
Client                              Server
  │                                    │
  │──── Hello ────────────────────▶│
  │                                    │  (register / check known clients)
  │◀──── Hello ────────────────────│
  │                                    │
  │  ┌─ if Pending ───────────────┐│
  │◀─┤  ApprovalPending              ││
  │  └── reconnect in 30 s ──────────┘│
  │                                    │
  │  ┌─ if Rejected ──────────────┐│
  │◀─┤  Rejected                     ││
  │  └── back off 300 s ─────────────┘│
  │                                    │
  │◀──── ManifestExchange ───────────│
  │──── ManifestExchange ──────────▶│
  │                                    │
  │◀──── Bundle / LargeFile* ────────│  (server sends missing files)
  │◀──── SyncComplete ─────────────│
  │                                    │
  │──── Bundle / LargeFile* ───────▶│  (client sends missing files)
  │──── SyncComplete ────────────▶│
  │                                    │
  │◀═══ live incremental sync ══════▶│
```

### Small File Transfer

Files smaller than **8 MiB** (`LARGE_FILE_THRESHOLD`) are batched into
`Bundle` messages. Each bundle holds up to **8 MiB** total
(`BUNDLE_MAX_BYTES`) or **500 files** (`BUNDLE_MAX_FILES`), whichever limit
is reached first.

### Large File Transfer

Files ≥ 8 MiB are streamed in three phases:

1. **`LargeFileStart`** — metadata (path, total size, hash).
2. **`LargeFileChunk`** (×N) — 8 MiB chunks (`FILE_CHUNK_SIZE`).
3. **`LargeFileEnd`** — signals completion; receiver verifies BLAKE3 hash.

The receiving side assembles chunks into a **pre-allocated temporary file**
under `.bh_filesync/transfers/`. Missing chunks can be requested via
**`RequestChunks`** for retransmission.

---

## Sync Engine

### Manifest

The manifest is a `HashMap<PathBuf, FileMetadata>` keyed by relative path.

`FileMetadata` contains:

| Field | Type | Description |
|-------|------|-------------|
| `rel_path` | `PathBuf` | Relative path within the sync root |
| `size` | `u64` | File size in bytes |
| `hash` | `[u8; 32]` | BLAKE3 content hash |
| `modified_ms` | `u64` | Last-modified time (ms since epoch) |
| `is_dir` | `bool` | Whether the entry is a directory |

Manifest building uses `WalkDir` for traversal and `rayon` for parallel
hashing across **4 threads** (`HASH_THREADS`).

### Send List Computation

A file is sent to the remote peer when any of these conditions hold:

1. The file is **missing** on the remote side.
2. The hashes differ **and** the local copy is **newer** (higher `modified_ms`).
3. The hashes differ, `modified_ms` is equal, **and** the sender is the server
   (server wins ties).

### Conflict Detection

When both the on-disk file and an incoming file differ from the last-known
manifest hash, a **conflict copy** is created:

```text
filename (conflict <unix_secs> <node_id>).ext
```

There is no automatic merge — the conflict copy is preserved for manual
resolution.

### Path Safety

`safe_relative()` rejects paths containing `..`, leading `/`, or any absolute
component to prevent directory-traversal attacks.

---

## Filesystem Watcher

- **Linux-only** — uses `inotify` directly.
- Watched events: `CREATE`, `CLOSE_WRITE`, `DELETE`, `DELETE_SELF`,
  `MOVED_FROM`, `MOVED_TO`.
- All directories under the sync root are watched recursively; newly created
  directories receive watches automatically.
- **Rename tracking:** `MOVED_FROM` / `MOVED_TO` pairs are correlated via
  inotify cookies. Orphaned rename halves time out after **500 ms**.
- A `CREATE` event without a matching `CLOSE_WRITE` is logged but **not**
  emitted — the watcher waits for the file to be fully written.
- Poll interval: **50 ms**; event buffer: **64 KiB**.

### Debouncing & Suppression

| Mechanism | Value | Purpose |
|-----------|-------|---------|
| Debounce window | 200 ms (`DEBOUNCE_MS`) | Coalesce rapid changes |
| Max batch interval | 2 s | Upper bound before flushing |
| File stability | 500 ms (`FILE_STABILITY_MS`) | File must be untouched for this long before sending |
| Write suppression | 2 s (`SUPPRESSION_SECS`) | Ignore watcher events for files we just wrote (prevents echo loops) |

---

## Transport Layer

Each peer connection is wrapped in a `Connection` struct providing
channel-based send/receive over TLS:

- A single **`tls-io` thread** per connection interleaves reads and writes.
- Read poll timeout: **5 ms** (set *after* TLS handshake completion to avoid
  `EAGAIN` issues during negotiation).
- Send queue capacity: **512** (`SEND_QUEUE_DEPTH`).
- Receive channel capacity: **1024** (`RECV_CHANNEL_DEPTH`).
- Peer certificate is extracted after handshake for fingerprinting.
- Queue pressure warnings are logged at **>50% full** and **near-full**.

---

## Server Architecture

- **Multi-client:** each accepted client spawns a dedicated thread with its own
  broadcast channel (capacity **512**, `CLIENT_BROADCAST_DEPTH`).
- Changes received from one client are **broadcast to all other clients** as
  pre-serialised frames.
- Non-blocking accept loop with **20 ms** poll interval.
- A separate **local change broadcaster** thread watches the filesystem and
  pushes changes to all connected peers.
- **Periodic full rescan** every **15 minutes** (`FULL_SCAN_INTERVAL_SECS`).
- **Preemptive disk-space checks** before receiving files; sends
  `InsufficientDiskSpace` if the volume is too full.

---

## Client Architecture

- **Reconnect loop** with exponential backoff:
  - Normal failures: **1 s → 60 s**.
  - Rejected by server: **300 s**.
  - Pending approval: **30 s**.
- Shutdown check granularity: **100 ms**.
- **Initial sync order:** receive server files first, then send local files.
- A dedicated **`recv-srv` thread** handles incoming messages during live sync.
- `PendingChanges` accumulator for debouncing and batching outgoing changes.
- Backlog warnings logged at **>100 pending items**.
- Two constructors: `standalone` (for `filesync-gui`) and `framework` (for
  ByteHive integration).

---

## HTTP API

All routes are under the `/api/filesync` prefix.

| Route | Method | Description |
|-------|--------|-------------|
| `/api/filesync/status` | `GET` | Mode, root, node_id, file/dir/byte counts, TLS info, pending approvals, exclusion patterns |
| `/api/filesync/manifest` | `GET` | Full manifest as JSON array |
| `/api/filesync/rescan` | `POST` | Trigger a manual rescan of the sync root |
| `/api/filesync/known-clients` | `GET` | List all known clients (server only) |
| `/api/filesync/known-clients/{fp}/approve` | `POST` | Approve a pending client by fingerprint |
| `/api/filesync/known-clients/{fp}/reject` | `POST` | Reject a client by fingerprint |
| `/api/filesync/known-clients/{fp}/label` | `POST` | Set a human-readable label for a client |
| `/api/filesync/known-clients/{fp}` | `DELETE` | Remove a known client entry |

---

## Bus Events

Events are published on the ByteHive message bus.

| Topic | Key Fields | Trigger |
|-------|-----------|---------|
| `filesync.file_changed` | `path`, `node` | File created or modified |
| `filesync.file_deleted` | `paths[]`, `node` | One or more files removed |
| `filesync.file_renamed` | `from`, `to`, `node` | File renamed |
| `filesync.sync_complete` | `node`, `files`, `bytes` | Initial sync finished |
| `filesync.sync_stats` | `sent`, `received`, `bytes_sent`, `bytes_received` | After initial sync |
| `filesync.incremental_stats` | `paths`, `bytes` | After an incremental flush |
| `filesync.root_stats` | `node`, `mode`, `file_count`, `dir_count`, `total_bytes` | Periodic heartbeat (every 60 s) |
| `filesync.client_joined` | `client_id`, `peer` | New client TCP connection (server) |
| `filesync.client_approval_needed` | `node_id`, `fingerprint`, `addr` | Unknown client connected (server) |
| `filesync.client_approved` | — | Client approved via API |
| `filesync.client_rejected` | — | Client rejected via API |

---

## Protocol Constants

| Constant | Value | Description |
|----------|-------|-------------|
| `PROTOCOL_VERSION` | 6 | Must match exactly between peers |
| `BUNDLE_MAX_BYTES` | 8 MiB | Max total size of a small-file bundle |
| `BUNDLE_MAX_FILES` | 500 | Max file count per bundle |
| `LARGE_FILE_THRESHOLD` | 8 MiB | Files ≥ this size use chunked transfer |
| `FILE_CHUNK_SIZE` | 8 MiB | Size of each large-file chunk |
| `MAX_FRAME_BYTES` | 32 MiB | Maximum wire frame size |
| `DEBOUNCE_MS` | 200 | Watcher debounce window (ms) |
| `FILE_STABILITY_MS` | 500 | File must be idle this long before sync (ms) |
| `SUPPRESSION_SECS` | 2 | Suppress watcher events after our own writes (s) |
| `SEND_QUEUE_DEPTH` | 512 | Per-connection send channel capacity |
| `CLIENT_BROADCAST_DEPTH` | 512 | Server broadcast channel per client |
| `HASH_THREADS` | 4 | Parallel BLAKE3 hashing threads |
| `READ_THREADS` | 4 | Parallel file-read threads |
| `FULL_SCAN_INTERVAL_SECS` | 900 | Periodic full rescan interval (15 min) |
| `METRICS_INTERVAL_SECS` | 60 | `root_stats` bus event interval |
| `BH_DIR` | `.bh_filesync` | Internal ByteHive state folder |
| `TMP_DIR` | `.bh_filesync/transfers` | Temp directory for large-file assembly |
| `TRASH_DIR` | `.bh_filesync/trash` | Trash folder for deleted files |

---

## Key Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `bytehive-core` | workspace | Framework integration |
| `bincode` | 1 | Binary serialisation |
| `blake3` | 1 | Content hashing |
| `lz4_flex` | 0.11 | Frame compression |
| `rayon` | 1 | Parallel manifest building |
| `walkdir` | 2 | Recursive directory traversal |
| `inotify` | 0.11 | Linux filesystem notifications |
| `regex` | 1 | Exclusion pattern matching |
| `rustls` | 0.23 | TLS 1.3 implementation |
| `rcgen` | 0.13 | Self-signed certificate generation |
| `iced` | 0.14 | GUI toolkit (standalone binary) |
| `gtk` | 0.18 | GTK integration |
| `tray-icon` | 0.21 | System-tray icon |
| `dirs` | 6 | Platform config/data directories |
| `fs2` | 0.4 | File locking and disk-space queries |

---

## Limitations

- **Linux only** — filesystem watching relies on `inotify`.
- **No conflict resolution** — last write wins; conflicts are saved as
  side-copies for manual inspection.
- **No partial-file transfer** — files are always transferred in their
  entirety (no rsync-style delta sync).
- **No relay mode** — clients must be able to reach the server directly over
  TCP.
- **Self-signed certificates** — no CA chain validation; no protection against
  active MITM (acceptable for trusted LANs).
- **Exact protocol version match** — peers on different versions cannot
  communicate (currently v6).
- **No file encryption at rest** — data is only encrypted in transit.
- **No bandwidth throttling** — transfers use as much bandwidth as available.
- **No selective sync** — the entire root directory is synced (exclusion
  patterns can only *filter out* files, not opt-in to a subset).
