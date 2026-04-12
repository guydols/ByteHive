# filesync

Bidirectional file sync over TLS 1.3. Runs as a ByteHive app or standalone via the
`filesync-gui` binary.

## Configuration

```toml
[apps.filesync]
root      = "/path/to/folder"
mode      = "server"          # or "client"
bind_addr = "0.0.0.0:7878"   # server only

# client only:
# server_addr = "host:7878"
# auth_token  = "<api-key-or-session-token>"

# Exclusion rules (optional):
# exclude_patterns = ["*.log", "build/**", "**/.cache/**"]
# exclude_regex    = ['.*\.(tmp|bak)$', '^private/']
```

Exclusion patterns use shell-style globs (`*` within a segment, `**` across directories,
`?` one character). Regex rules match the relative path with forward-slash separators.

## HTTP API

Requires `Authorization: Bearer <token>`.

| Route | Method | Description |
|-------|--------|-------------|
| `/api/filesync/status` | GET | Mode, root, node ID, file/dir counts |
| `/api/filesync/manifest` | GET | Full manifest with BLAKE3 hashes |
| `/api/filesync/rescan` | POST | Re-scan disk and rebuild manifest |

## Bus events published

| Topic | Key payload fields | When |
|-------|-------------------|------|
| `filesync.file_changed` | `path, node` | File created or modified |
| `filesync.file_deleted` | `paths[], node` | One or more paths removed |
| `filesync.sync_complete` | `node, files, bytes` | Initial sync handshake done |
| `filesync.client_joined` | `client_id, peer` | New client connected (server) |
| `filesync.sync_stats` | `sent, received, bytes_sent, bytes_received` | After each initial sync |
| `filesync.incremental_stats` | `paths, bytes` | After each incremental flush |
| `filesync.root_stats` | `node, mode, file_count, dir_count, total_bytes` | Periodic heartbeat (60 s) |

## Protocol

Framed binary: `bincode` + LZ4 compression, 4-byte big-endian length prefix.

Files ≤ 8 MiB are batched into `Bundle` messages (up to 8 MiB or 500 files). Larger
files stream as `LargeFileStart / LargeFileChunk / LargeFileEnd`. Integrity verified
with BLAKE3.

Handshake:
1. Client → `Hello{node_id, protocol_version, credential}`
2. Server → `Hello{node_id, protocol_version, credential: None}`
3. Server → `ManifestExchange`
4. Client → `ManifestExchange`
5. Each side sends files the other is missing, then `SyncComplete`
6. Both sides watch for incremental changes and push them as they occur

Bump `PROTOCOL_VERSION` whenever the `Message` enum layout changes.

## TLS

- TLS 1.3 only; cipher suites: `TLS_AES_256_GCM_SHA384` and `TLS_CHACHA20_POLY1305_SHA256`
- Server generates a self-signed ECDSA P-384 cert in memory on each startup
- Clients skip cert verification (`AcceptAnyCert`); access control is via `Hello.credential`
- Wire is fully encrypted; no protection against active MITM (acceptable for LAN use)

## Limitations

- Linux only (inotify)
- No conflict resolution — last write wins
- No partial-file transfer — files always transferred whole
- No relay mode — clients must reach the server directly over TCP
- Self-signed cert is ephemeral — regenerated on every restart
