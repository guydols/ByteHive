# ByteHive Usage Guide

A comprehensive guide to setting up, configuring, and using ByteHive — a modular framework for file synchronization and management.

---

## Table of Contents

- [Quick Start](#quick-start)
- [CLI Reference](#cli-reference)
- [Configuration](#configuration)
  - [Framework Settings](#framework-settings)
  - [Users, Groups & API Keys](#users-groups--api-keys)
  - [App Configuration](#app-configuration)
- [First-Run Setup](#first-run-setup)
- [Web Portal](#web-portal)
- [Authentication](#authentication)
  - [Cookie-Based Sessions](#cookie-based-sessions)
  - [API Keys](#api-keys)
  - [Static Admin Token](#static-admin-token)
- [Access Model](#access-model)
- [HTTP API Reference](#http-api-reference)
  - [Public Endpoints](#public-endpoints)
  - [Authenticated Endpoints](#authenticated-endpoints)
  - [Admin-Only Endpoints](#admin-only-endpoints)
- [FilesSync](#filessync)
  - [Server Mode](#server-mode)
  - [Client Mode](#client-mode)
  - [Client Approval System](#client-approval-system)
  - [Exclusion Patterns](#exclusion-patterns)
  - [FilesSync API](#filessync-api)
  - [FilesSync Bus Events](#filessync-bus-events)
- [FileBrowser](#filebrowser)
  - [FileBrowser Configuration](#filebrowser-configuration)
  - [Web Interface](#web-interface)
  - [Share Links](#share-links)
  - [FileBrowser API](#filebrowser-api)
- [Deployment](#deployment)
  - [Deploying as a Sync Server](#deploying-as-a-sync-server)
  - [Deploying Clients](#deploying-clients)
- [Common Workflows](#common-workflows)

---

## Quick Start

```bash
# Build from source
cargo build --release

# Start with defaults (reads config.toml from current directory)
./bytehive

# Start with a specific config file
./bytehive --config /etc/bytehive/config.toml

# Open http://localhost:9000/ in your browser to complete first-run setup
```

---

## CLI Reference

ByteHive ships as a single binary: `bytehive`.

### Arguments

| Flag | Long | Default | Description |
|------|------|---------|-------------|
| `-c` | `--config` | `config.toml` | Path to the configuration file |

### Commands

**Hash a password** (for manual config file editing):

```bash
bytehive hash-password <plaintext>
```

This outputs an Argon2id hash suitable for pasting into the `password_hash`
field of a `[[users]]` entry.

### Examples

```bash
# Start with defaults
./bytehive

# Use a specific config
./bytehive -c /etc/bytehive/config.toml
./bytehive --config /etc/bytehive/config.toml

# Generate a password hash
./bytehive hash-password "my-secure-password"
```

---

## Configuration

ByteHive is configured through a single TOML file (default: `config.toml`).

> **Important:** When the server is running, the configuration file is managed
> automatically. Users, groups, and API keys added via the admin panel are
> written back to the file immediately. Edits to `[framework]` and `[apps.*]`
> sections are preserved, but **comments in the auth sections (`[[users]]`,
> `[[groups]]`, `[[api_keys]]`) may be lost** when the server writes back.

### Framework Settings

```toml
[framework]
http_addr  = "0.0.0.0:9000"   # HTTP server bind address
http_token = ""                # Optional static admin token (prefer API keys)
web_root   = ""                # Optional custom web root for /web/* static files
log_level  = "info"            # One of: debug, info, warn, error
```

| Field | Default | Description |
|-------|---------|-------------|
| `http_addr` | `0.0.0.0:9000` | Address and port the HTTP server binds to |
| `http_token` | `""` | Static admin token for API access; prefer API keys instead |
| `web_root` | `""` | Custom directory for static files served under `/web/*` |
| `log_level` | `info` | Logging verbosity: `debug`, `info`, `warn`, `error` |

### Users, Groups & API Keys

```toml
[[users]]
username      = "alice"
display_name  = "Alice"
password_hash = "$argon2id$v=19$m=65536,t=3,p=4$<salt>$<hash>"

[[groups]]
name        = "admin"
description = "Administrators"
members     = ["alice"]

[[groups]]
name        = "user"
description = "Standard users"
members     = ["alice"]

[[api_keys]]
name       = "ci-pipeline"
key        = "uuid-here"
as_user    = "ci"              # Label shown in audit logs
expires_ms = 1999999999000     # Optional Unix-ms expiry timestamp
created_at = 0
```

Generate a `password_hash` with:

```bash
bytehive hash-password "the-password"
```

### App Configuration

Apps are configured under `[apps.<name>]` sections:

```toml
[apps.filesync]
root      = "/path/to/folder"
mode      = "server"
bind_addr = "0.0.0.0:7878"

[apps.filebrowser]
max_upload_mb = 200
allow_delete  = true
```

See the [FilesSync](#filessync) and [FileBrowser](#filebrowser) sections below
for full details on each app's configuration options.

### Full Example

```toml
[framework]
http_addr  = "0.0.0.0:9000"
log_level  = "info"

[[users]]
username      = "admin"
display_name  = "Admin"
password_hash = "$argon2id$v=19$m=65536,t=3,p=4$..."

[[groups]]
name        = "admin"
description = "Administrators"
members     = ["admin"]

[[groups]]
name        = "user"
description = "Standard users"
members     = ["admin"]

[[api_keys]]
name       = "ci-pipeline"
key        = "550e8400-e29b-41d4-a716-446655440000"
as_user    = "ci"
expires_ms = 1999999999000
created_at = 1700000000000

[apps.filesync]
root      = "/srv/sync"
mode      = "server"
bind_addr = "0.0.0.0:7878"
exclude_patterns = ["*.log", "build/**"]

[apps.filebrowser]
max_upload_mb = 200
allow_delete  = true
```

---

## First-Run Setup

When ByteHive starts with no `[[users]]` entries in the configuration file, it
automatically enables an interactive setup flow:

1. **Start the server** with an empty or new config file (no `[[users]]` entries).
2. **Open your browser** and navigate to `http://<host>:<port>/`.
3. You will be **redirected to `/setup`** automatically.
4. **Set the admin password** — minimum 8 characters.
5. An `admin` user is created and added to both the `admin` and `user` groups.
6. You are **automatically logged in** and can begin using the system.

The password is hashed with Argon2id and written to the configuration file.

---

## Web Portal

ByteHive provides a built-in web interface at three main URLs:

| URL | Purpose |
|-----|---------|
| `http://<host>:<port>/` | **Portal login page** — enter username and password |
| `http://<host>:<port>/admin` | **Admin dashboard** — ops dashboard (admin group only) |
| `http://<host>:<port>/apps/filebrowser` | **File browser** — web-based file manager |

---

## Authentication

ByteHive supports three authentication methods.

### Cookie-Based Sessions

The primary method for browser-based access.

- Created via `POST /api/auth/login` with body `{"username": "...", "password": "..."}`.
- Cookie name: `cc_session` (HttpOnly, SameSite=Lax).
- Sessions last **8 hours** and are **refreshed on each authenticated request**.

**Example:**

```bash
curl -c cookies.txt -X POST http://localhost:9000/api/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username": "admin", "password": "my-password"}'

# Use the session cookie for subsequent requests
curl -b cookies.txt http://localhost:9000/api/core/status
```

### API Keys

Best for programmatic access, CI/CD pipelines, and scripts.

- Passed via `Authorization: Bearer <key>` header **or** `?token=<key>` query parameter.
- Created via the admin panel or the `POST /api/core/apikeys` endpoint.
- Keys are UUID v4 format.
- All API keys grant **admin-equivalent access**.
- Optional expiry via `expires_ms` (Unix timestamp in milliseconds).

**Example:**

```bash
# Using the Authorization header
curl -H "Authorization: Bearer 550e8400-e29b-41d4-a716-446655440000" \
  http://localhost:9000/api/core/status

# Using the query parameter
curl "http://localhost:9000/api/core/status?token=550e8400-e29b-41d4-a716-446655440000"
```

### Static Admin Token

Configured via `http_token` in the `[framework]` section. This is a simple
shared secret — **prefer API keys** for production use.

```toml
[framework]
http_token = "my-static-token"
```

---

## Access Model

Access is controlled by group membership. Two groups are built-in and
**cannot be deleted**:

| Group | Access Level |
|-------|-------------|
| `admin` | **Full access:** all APIs, ops dashboard, user/group/key management |
| `user` | **Read/write access** to app APIs (e.g., FileBrowser, FilesSync) |
| Any other | **App-defined;** `can_write()` returns false (read-only by default) |

Custom groups can be created for app-specific access control, but only
`admin` and `user` have framework-level meaning.

---

## HTTP API Reference

### Public Endpoints

These routes require no authentication:

| Route | Method | Description |
|-------|--------|-------------|
| `/` | GET | Portal login page |
| `/setup` | GET | First-run setup page |
| `/api/auth/login` | POST | Create session — body: `{"username", "password"}` |
| `/api/auth/setup` | POST | Complete first-run setup — body: `{"password"}` |
| `/web/*path` | GET | Static file serving from `web_root` |
| `/s/:token` | GET/POST | Public share link access |
| `/bytehive-icon.svg` | GET | ByteHive icon |
| `/bytehive-logo-full.svg` | GET | ByteHive full logo |

### Authenticated Endpoints

These routes require a valid session cookie or API key:

| Route | Method | Description |
|-------|--------|-------------|
| `/api/auth/me` | GET | Current user info with groups |
| `/api/auth/logout` | POST | Destroy session |
| `/api/*` | ANY | Proxied to app matching `http_prefix` |
| `/apps/*` | ANY | Proxied to app matching `ui_prefix` |

### Admin-Only Endpoints

These routes require the user to be in the `admin` group:

#### System

| Route | Method | Description |
|-------|--------|-------------|
| `/admin` | GET | Ops dashboard page |
| `/api/core/status` | GET | Framework version + registered apps |
| `/api/core/events` | GET | SSE event stream (all bus messages) |
| `/api/core/config/export` | GET | Export config as TOML (keys redacted) |

#### App Management

| Route | Method | Description |
|-------|--------|-------------|
| `/api/core/apps` | GET | List all registered apps |
| `/api/core/apps/:name` | GET | Single app info |
| `/api/core/apps/:name/config` | PUT | Update app config — body: `{"toml": "..."}` |
| `/api/core/apps/:name/start` | POST | Start a stopped app |
| `/api/core/apps/:name/stop` | POST | Stop a running app |
| `/api/core/apps/:name/restart` | POST | Restart an app (300 ms pause) |

#### User Management

| Route | Method | Description |
|-------|--------|-------------|
| `/api/core/users` | GET | List all users |
| `/api/core/users` | POST | Create user — body: `{"username", "password", "display_name?", "groups?"}` |
| `/api/core/users/:username` | PUT | Update user — body: `{"display_name?", "password?"}` |
| `/api/core/users/:username` | DELETE | Delete user |

#### Group Management

| Route | Method | Description |
|-------|--------|-------------|
| `/api/core/groups` | GET | List all groups |
| `/api/core/groups` | POST | Create group — body: `{"name", "description?", "members?"}` |
| `/api/core/groups/:name` | DELETE | Delete group (built-in groups cannot be deleted) |
| `/api/core/groups/:name/members/:username` | POST | Add member to group |
| `/api/core/groups/:name/members/:username` | DELETE | Remove member from group |

#### API Key Management

| Route | Method | Description |
|-------|--------|-------------|
| `/api/core/apikeys` | GET | List API keys (raw key values are not returned) |
| `/api/core/apikeys` | POST | Create API key — body: `{"name", "as_user?", "expires_ms?"}` |
| `/api/core/apikeys/:name` | DELETE | Revoke API key |

---

## FilesSync

FilesSync provides real-time, bidirectional file synchronization between a
server and one or more clients using mutual TLS authentication.

### Server Mode

Configure the server to share a folder:

```toml
[apps.filesync]
root      = "/path/to/sync/folder"
mode      = "server"
bind_addr = "0.0.0.0:7878"
```

The server authenticates clients using **mutual TLS certificates**. New clients
must be approved via the [client approval system](#client-approval-system)
before synchronization begins.

### Client Mode

Configure a client to connect to a server:

```toml
[apps.filesync]
root        = "/path/to/local/folder"
mode        = "client"
server_addr = "192.168.1.10:7878"
```

On first connection, the client **pins the server's certificate fingerprint**
using a Trust-On-First-Use (TOFU) model. All subsequent connections must present
the same certificate fingerprint.

### Client Approval System

When a new client connects for the first time, its certificate fingerprint is
registered as **"pending"**. The pending client receives an `ApprovalPending`
message and will **reconnect every 30 seconds** until approved.

Manage clients through the admin panel or the API:

| Action | API Call |
|--------|---------|
| View all clients | `GET /api/filesync/known-clients` |
| Approve a client | `POST /api/filesync/known-clients/{fingerprint}/approve` |
| Reject a client | `POST /api/filesync/known-clients/{fingerprint}/reject` |
| Label a client | `POST /api/filesync/known-clients/{fingerprint}/label` — body: `{"label": "My Laptop"}` |
| Remove a client | `DELETE /api/filesync/known-clients/{fingerprint}` |

**Example — approving a client:**

```bash
# List pending clients
curl -H "Authorization: Bearer <key>" \
  http://localhost:9000/api/filesync/known-clients

# Approve by fingerprint
curl -X POST -H "Authorization: Bearer <key>" \
  http://localhost:9000/api/filesync/known-clients/abc123def456/approve

# Give it a friendly label
curl -X POST -H "Authorization: Bearer <key>" \
  -H "Content-Type: application/json" \
  -d '{"label": "Office Desktop"}' \
  http://localhost:9000/api/filesync/known-clients/abc123def456/label
```

### Exclusion Patterns

FilesSync supports two mechanisms for excluding files from synchronization.
Both match against **relative paths using forward-slash separators** and are
OR-combined (a file matching any pattern is excluded).

#### Glob Patterns (`exclude_patterns`)

| Pattern | Matches |
|---------|---------|
| `*` | Any characters except `/` |
| `**` | Any characters including `/` (crosses directory boundaries) |
| `?` | Exactly one character |

```toml
exclude_patterns = ["*.log", "build/**", "**/.cache/**", "**/node_modules/**"]
```

#### Regex Patterns (`exclude_regex`)

Standard regular expressions matched against the relative file path:

```toml
exclude_regex = ['.*\.(tmp|bak)$', '^private/', 'node_modules']
```

#### Built-In Exclusions

The following are **always ignored** regardless of configuration:

- `.git`
- `.DS_Store`
- `.bh_filesync`
- `.bh_filesync/**`

### FilesSync API

All routes require authentication. Server-only endpoints are noted.

| Route | Method | Description |
|-------|--------|-------------|
| `/api/filesync/status` | GET | Mode, root, node ID, file/dir counts, TLS info |
| `/api/filesync/manifest` | GET | Full manifest with BLAKE3 hashes |
| `/api/filesync/rescan` | POST | Trigger a manual rescan |
| `/api/filesync/known-clients` | GET | List known clients *(server only)* |
| `/api/filesync/known-clients/:fp/approve` | POST | Approve client *(server only)* |
| `/api/filesync/known-clients/:fp/reject` | POST | Reject client *(server only)* |
| `/api/filesync/known-clients/:fp/label` | POST | Set client label *(server only)* |
| `/api/filesync/known-clients/:fp` | DELETE | Remove client *(server only)* |

### FilesSync Bus Events

Subscribe to real-time events via the SSE stream at `/api/core/events`:

| Topic | When |
|-------|------|
| `filesync.file_changed` | File created or modified |
| `filesync.file_deleted` | File(s) removed |
| `filesync.sync_complete` | Initial sync handshake done |
| `filesync.sync_stats` | After each initial sync |
| `filesync.incremental_stats` | After each incremental flush |
| `filesync.root_stats` | Periodic heartbeat (every 60 seconds) |
| `filesync.client_joined` | New client connected *(server)* |
| `filesync.client_approval_needed` | Unknown client connected *(server)* |
| `filesync.client_approved` | Client approved *(server)* |
| `filesync.client_rejected` | Client rejected *(server)* |

---

## FileBrowser

FileBrowser is a full-featured web file manager with editing, previewing, and
sharing capabilities.

### FileBrowser Configuration

```toml
[apps.filebrowser]
max_upload_mb = 200    # Maximum upload size in MB (default: 200)
allow_delete  = true   # Allow file/directory deletion (default: true)
root          = "/optional/custom/path"  # Defaults to filesync root if omitted
```

### Web Interface

Navigate to `http://<host>:<port>/apps/filebrowser` after logging in.

**Features:**

- **Browse** directories with switchable grid/list view
- **Upload** files via drag & drop or file picker
- **Download** files or entire directories as ZIP archives
- **Create, rename, copy, delete** files and directories
- **Edit** text files with Monaco editor (syntax highlighting for ~75 languages)
- **Preview** images, videos, audio, and PDFs inline
- **Search** files by name recursively
- **Share** files via public links with optional password and expiry

### Share Links

Create a shareable link for any file:

```bash
curl -X POST -H "Authorization: Bearer <key>" \
  -H "Content-Type: application/json" \
  -d '{"path": "documents/report.pdf", "password": "optional", "expires_hours": 24}' \
  http://localhost:9000/api/filebrowser/share
```

The response includes a share token. The share URL format is:

```
http://<host>:<port>/s/<32-hex-token>
```

**Share behavior:**

- Password-protected shares display a password entry page before download.
- Expired shares are **automatically cleaned up every 5 minutes**.
- Share download counts are tracked in-memory only (reset on restart).

### FileBrowser API

All routes require authentication unless otherwise noted.

| Route | Method | Description |
|-------|--------|-------------|
| `/api/filebrowser/status` | GET | Root path, share count, config |
| `/api/filebrowser/ls` | GET | Directory listing — `?path=<dir>` |
| `/api/filebrowser/download` | GET | Download file or directory as ZIP — `?path=<path>` |
| `/api/filebrowser/upload` | POST | Upload file — `?dir=<dir>&name=<filename>` |
| `/api/filebrowser/mkdir` | POST | Create directory — body: `{"path": "new/dir"}` |
| `/api/filebrowser/delete` | DELETE | Delete file/dir — `?path=<path>` (requires `allow_delete`) |
| `/api/filebrowser/rename` | POST | Rename — body: `{"from": "old", "to": "new"}` |
| `/api/filebrowser/copy` | POST | Copy — body: `{"from": "src", "to": "dst"}` |
| `/api/filebrowser/read` | GET | Read text file — `?path=<file>&force=<bool>` |
| `/api/filebrowser/write` | POST | Write text file — body: `{"path", "content", "force?"}` |
| `/api/filebrowser/detect` | GET | Detect file type — `?path=<file>` |
| `/api/filebrowser/search` | GET | Search files — `?path=<root>&q=<query>&max_results=<n>` |
| `/api/filebrowser/preview` | GET | Inline preview — `?path=<file>` |
| `/api/filebrowser/thumb` | GET | Thumbnail — `?path=<file>` |
| `/api/filebrowser/share` | POST | Create share link |
| `/api/filebrowser/shares` | GET | List active shares |
| `/api/filebrowser/share` | DELETE | Delete share — `?token=<token>` |

---

## Deployment

### Deploying as a Sync Server

1. **Build** the binary:

   ```bash
   cargo build --release
   ```

2. **Create** a `config.toml` with the sync server configuration:

   ```toml
   [framework]
   http_addr = "0.0.0.0:9000"
   log_level = "info"

   [apps.filesync]
   root      = "/srv/sync"
   mode      = "server"
   bind_addr = "0.0.0.0:7878"

   [apps.filebrowser]
   max_upload_mb = 200
   allow_delete  = true
   ```

3. **Start** the server:

   ```bash
   ./bytehive --config config.toml
   ```

4. **Complete first-run setup** by navigating to `http://<host>:9000/` in your
   browser. Set the admin password (minimum 8 characters).

5. **Create an API key** in the admin panel at `http://<host>:9000/admin` for
   client authentication and automation.

6. On each **client machine**, configure with `mode = "client"` and the
   server's `server_addr` (see below).

### Deploying Clients

#### Framework Client

Run another ByteHive instance with a client configuration. This gives you the
full web portal, file browser, and admin panel on the client machine as well:

```toml
[framework]
http_addr = "0.0.0.0:9001"
log_level = "info"

[apps.filesync]
root        = "/home/user/cloud"
mode        = "client"
server_addr = "192.168.1.10:7878"
```

```bash
./bytehive --config client-config.toml
```

After starting, the client will connect to the server and its certificate
fingerprint will appear as "pending" on the server. Approve it via the server's
admin panel or API before synchronization begins.

#### Standalone GUI Client

For a lightweight desktop experience without the full framework:

```bash
./filesync-gui
```

The GUI client has its own configuration, a system tray icon, and does **not**
require the full ByteHive framework to be running.

---

## Common Workflows

### Creating a User via API

```bash
curl -X POST -H "Authorization: Bearer <api-key>" \
  -H "Content-Type: application/json" \
  -d '{"username": "bob", "password": "secure-pass-123", "display_name": "Bob", "groups": ["user"]}' \
  http://localhost:9000/api/core/users
```

### Uploading a File via API

```bash
curl -X POST -H "Authorization: Bearer <api-key>" \
  --data-binary @report.pdf \
  "http://localhost:9000/api/filebrowser/upload?dir=documents&name=report.pdf"
```

### Downloading a Directory as ZIP

```bash
curl -H "Authorization: Bearer <api-key>" \
  -o backup.zip \
  "http://localhost:9000/api/filebrowser/download?path=documents"
```

### Monitoring Events via SSE

```bash
curl -N -H "Authorization: Bearer <api-key>" \
  http://localhost:9000/api/core/events
```

This opens a streaming connection that emits server-sent events for all bus
messages including file changes, sync completions, and client connections.

### Exporting the Configuration

```bash
curl -H "Authorization: Bearer <api-key>" \
  http://localhost:9000/api/core/config/export
```

Returns the current configuration as TOML with sensitive keys redacted.
