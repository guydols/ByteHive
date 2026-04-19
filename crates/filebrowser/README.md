# 🗂 bytehive-filebrowser

**Web file browser — browse, upload, download and share files.**

`bytehive-filebrowser` is a self-contained ByteHive application that provides a
full-featured web file browser with file management, inline editing, previews,
and password-protected share links.

| | |
|---|---|
| **Crate** | `bytehive-filebrowser` |
| **Version** | 0.1.0 |
| **Edition** | Rust 2021 |
| **Nav label** | Files |
| **Nav icon** | 🗂 (U+1F5C2) |

## Dependencies

| Crate | Purpose |
|---|---|
| `bytehive-core` | Framework integration (app manifest, HTTP, config) |
| `serde` / `serde_json` | Serialization |
| `parking_lot` | `RwLock` for the in-memory share store |
| `log` | Logging |
| `uuid` 1 (v4) | Share token generation |
| `sha2` 0.10 | Share password hashing |
| `zip` 2 | Directory download as ZIP |
| `tempfile` 3 | *(dev only)* Test helpers |

---

## App Manifest

The app registers itself with the ByteHive framework using the following
manifest values:

| Field | Value |
|---|---|
| `name` | `filebrowser` |
| `version` | Read from `CARGO_PKG_VERSION` (0.1.0) |
| `description` | *Web file browser — browse, upload, download and share files.* |
| `http_prefix` | `/api/filebrowser` |
| `ui_prefix` | `/apps/filebrowser` |
| `nav_label` | Files |
| `nav_icon` | 🗂 |
| `show_in_nav` | `true` |
| `subscriptions` | *(none)* |
| `publishes` | `filebrowser.share_created`, `filebrowser.share_accessed` |

---

## Configuration

All configuration lives under the `[apps.filebrowser]` table:

```toml
[apps.filebrowser]
root          = "/optional/custom/path"   # defaults to filesync root
max_upload_mb = 200                       # default 200
allow_delete  = true                      # default true
```

### Root directory resolution

1. If `apps.filebrowser.root` is set, that path is used.
2. Otherwise the app falls back to `apps.filesync.root` from the framework
   config.
3. If **neither** value is set the app **fails to start**.

---

## Inner State

At runtime the app holds:

| Field | Type | Notes |
|---|---|---|
| `root` | `PathBuf` | Resolved root directory |
| `max_upload_bytes` | `u64` | `max_upload_mb * 1024 * 1024` |
| `allow_delete` | `bool` | Whether the DELETE endpoint is enabled |
| `shares` | `Arc<RwLock<HashMap<String, Share>>>` | In-memory share store |

A **garbage-collection thread** runs every **300 seconds** (5 min) and removes
expired shares from the map.

---

## Authentication & Authorization

### UI routes (no auth required)

| Path | Description |
|---|---|
| `/apps/filebrowser` | Serves `FILEBROWSER_HTML` from core assets |
| `/s/{token}` | Public share access page |

### API routes (auth required)

All other endpoints require the following headers set by the framework:

- `x-bytehive-user` — authenticated username
- `x-bytehive-role` — user role (`admin`, `user`, or `readonly`)

### Role enforcement

The **`readonly`** role is blocked from all mutating operations:

> upload, mkdir, delete, rename, share (create / delete), write, copy

The **`admin`** and **`user`** roles have full access to every endpoint.

---

## HTTP API Reference

All API routes are served under the `http_prefix` (`/api/filebrowser`).

### `GET /status`

Returns high-level status information.

**Response:**

```json
{
  "root": "/data/files",
  "share_count": 3,
  "max_upload_mb": 200,
  "allow_delete": true
}
```

`share_count` only includes non-expired shares.

---

### `GET /ls`

List directory contents.

| Query param | Default | Description |
|---|---|---|
| `path` | `""` | Relative path inside root |

**Response:**

```json
{
  "path": "documents",
  "entries": [
    {
      "name": "notes",
      "path": "documents/notes",
      "is_dir": true,
      "size": 0,
      "mtime": 1700000000,
      "ext": null
    },
    {
      "name": "readme.md",
      "path": "documents/readme.md",
      "is_dir": false,
      "size": 1234,
      "mtime": 1700000000,
      "ext": "md"
    }
  ]
}
```

Entries are sorted **directories first**, then **alphabetically by name**.

---

### `GET /download`

Download a file or directory.

| Query param | Required | Description |
|---|---|---|
| `path` | Yes | Relative path inside root |

- **File** — served as an attachment with the correct MIME type.
- **Directory** — compressed as a ZIP archive (Deflate, in-memory) and served
  as an attachment.

Headers set: `Content-Disposition: attachment; filename="<name>"`

---

### `POST /upload`

Upload a single file.

| Query param | Default | Description |
|---|---|---|
| `dir` | `""` | Target directory |
| `name` | *(required)* | File name (must not contain `/`, `\`, or be `..`) |

The request body is the raw file content.

**Validations:**

- Body size must be at most `max_upload_bytes`.
- The target directory is created automatically if it does not exist.

**Response:**

```json
{ "ok": true, "path": "uploads/photo.jpg", "size": 204800 }
```

---

### `POST /mkdir`

Create a directory (including intermediate parents).

**Request body:**

```json
{ "path": "documents/new-folder" }
```

**Response:**

```json
{ "ok": true, "path": "documents/new-folder" }
```

---

### `DELETE /delete`

Delete a file or directory.

| Query param | Required | Description |
|---|---|---|
| `path` | Yes | Relative path inside root |

- Returns **401** if `allow_delete` is `false` in the config.
- Directories are removed **recursively**.

**Response:**

```json
{ "ok": true }
```

---

### `POST /rename`

Rename or move a file/directory.

**Request body:**

```json
{ "from": "old-name.txt", "to": "new-name.txt" }
```

**Response:**

```json
{ "ok": true }
```

---

### `POST /copy`

Copy a file or directory.

**Request body:**

```json
{ "from": "source.txt", "to": "destination.txt" }
```

**Validations:**

- Source must exist.
- Destination must **not** already exist.
- Directories are copied recursively; files use `std::fs::copy`.

**Response:**

```json
{ "ok": true, "from": "source.txt", "to": "destination.txt" }
```

---

### `GET /read`

Read file contents for the in-browser editor.

| Query param | Required | Description |
|---|---|---|
| `path` | Yes | Relative path inside root |
| `force` | No | Set to `"1"` or `"true"` to bypass text-file checks |

**Limits:**

- `MAX_READ_BYTES` = **2 MB** (2 * 1024 * 1024). Files larger than this are
  rejected.
- Unless `force` is set, non-text files (determined by extension check +
  content sniffing) are rejected.

**Response:**

```json
{
  "content": "fn main() { ... }",
  "size": 42,
  "language": "rust",
  "path": "src/main.rs",
  "forced": false
}
```

Content is a UTF-8 string (lossy conversion for non-UTF-8 files).

---

### `POST /write`

Save file contents from the editor.

**Request body:**

```json
{
  "path": "src/main.rs",
  "content": "fn main() { ... }",
  "force": false
}
```

**Validations:**

- Body size must be at most `max_upload_bytes`.
- Unless `force` is set, non-text files are rejected.

**Response:**

```json
{ "ok": true, "path": "src/main.rs", "size": 34 }
```

---

### `GET /detect`

Detect whether a file is text or binary.

| Query param | Required | Description |
|---|---|---|
| `path` | Yes | Relative path inside root |

**Response:**

```json
{
  "is_text": true,
  "by_extension": true,
  "by_content": true,
  "language": "rust",
  "size": 1234,
  "is_dir": false
}
```

`is_text` = `by_extension || by_content`.

---

### `GET /search`

Recursive filename search.

| Query param | Default | Description |
|---|---|---|
| `path` | `""` | Directory to search within |
| `q` | *(required)* | Search query (case-insensitive substring match) |
| `max_results` | `200` | Maximum results (capped at **500**) |

`path` must point to a directory.

**Response:**

```json
{
  "path": "",
  "query": "readme",
  "results": [
    {
      "name": "README.md",
      "path": "README.md",
      "is_dir": false,
      "size": 2048,
      "mtime": 1700000000,
      "ext": "md"
    }
  ],
  "truncated": false
}
```

> **Note:** This is a **filename-only** search, not a content search.

---

### `GET /preview`

Inline file preview.

| Query param | Required | Description |
|---|---|---|
| `path` | Yes | Relative path inside root |

- Cannot preview directories.
- Headers: `Content-Disposition: inline`, `Cache-Control: private, max-age=60`.
- Served with the correct MIME type.

---

### `GET /thumb`

Thumbnail endpoint. **Delegates entirely to `handle_preview`** — no
server-side resizing is performed. The full image is served and the browser
scales it via CSS.

---

## Share System

Shares allow unauthenticated users to download files or directories via a
unique link.

### Share data model

| Field | Type | Notes |
|---|---|---|
| `token` | `String` | UUID v4 with dashes removed (32 hex chars) |
| `path` | `String` | Relative path to the shared file/directory |
| `is_dir` | `bool` | Whether the path is a directory |
| `name` | `String` | Display name |
| `password_protected` | `bool` | Whether a password is required |
| `password_hash` | `String` | SHA-256 hash (skipped in serialization) |
| `expires_ms` | `Option<u64>` | Expiry timestamp in milliseconds |
| `created_by` | `String` | Username of the creator |
| `created_ms` | `u64` | Creation timestamp in milliseconds |
| `download_count` | `u64` | Number of downloads (in-memory only) |

### Password hashing

Passwords are hashed with **SHA-256** using the prefix `filebrowser-share:`.
Verification uses **constant-time comparison**.

### Share expiry

Expiry is optional (set via `expires_hours` at creation time). A background GC
thread runs every **300 seconds** (5 minutes) and removes expired shares.

### API endpoints

#### `POST /share` — Create a share

**Request body:**

```json
{
  "path": "documents/report.pdf",
  "password": "s3cret",
  "expires_hours": 24
}
```

Both `password` and `expires_hours` are optional.

**Response:**

```json
{
  "ok": true,
  "token": "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "url": "/s/a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "share": { ... }
}
```

The app publishes a **`filebrowser.share_created`** event.

#### `GET /shares` — List active shares

Returns all non-expired shares.

```json
{ "shares": [ ... ] }
```

#### `DELETE /share` — Delete a share

| Query param | Required | Description |
|---|---|---|
| `token` | Yes | Share token |

Only the **creator** or an **admin** can delete a share.

```json
{ "ok": true }
```

### Public share access — `/s/{token}`

No authentication required.

| Method | Behavior |
|---|---|
| `GET` | If password-protected, shows a password page. Otherwise, serves the file. |
| `POST` | Accepts `{ "password": "..." }` to verify the share password. |

On successful access:

- Increments `download_count`.
- Directory results in a ZIP download.
- File results in an attachment download.
- Publishes a **`filebrowser.share_accessed`** event.

Returns styled **error HTML** if the token is not found or the share has
expired.

---

## File Type Detection

Two complementary strategies determine whether a file is text or binary:

### 1. Extension-based detection

A list of **~75 known text/code extensions** including:

> `txt`, `md`, `rs`, `py`, `js`, `ts`, `json`, `yaml`, `toml`, `go`, `java`,
> `c`, `cpp`, `html`, `css`, `xml`, `sh`, `sql`, `rb`, `php`, `swift`, `kt`,
> `scala`, `r`, `lua`, `vim`, `dockerfile`, `makefile`, and many more.

### 2. Content sniffing

1. Read the first **8 KB** of the file.
2. If any **null byte** (`0x00`) is found, the file is **binary**.
3. Otherwise count "suspicious" bytes (bytes < 0x09 or in 0x0e..0x1f,
   excluding ESC).
4. If the suspicious-byte ratio is **< 10%**, the file is **text**.

A file is considered text if **either** strategy says it is text
(`is_text = by_extension || by_content`).

---

## Monaco Language Mapping

File extensions are mapped to Monaco editor language IDs for syntax
highlighting:

| Extension(s) | Language ID |
|---|---|
| `rs` | `rust` |
| `py` | `python` |
| `js`, `jsx` | `javascript` |
| `ts`, `tsx` | `typescript` |
| `json` | `json` |
| `html`, `vue`, `svelte` | `html` |
| `css` | `css` |
| `scss`, `sass` | `scss` |
| `md` | `markdown` |
| `yml`, `yaml` | `yaml` |
| `toml` | `toml` |
| `xml`, `svg` | `xml` |
| `sh`, `bash`, `zsh` | `shell` |
| `sql` | `sql` |
| `go` | `go` |
| `java` | `java` |
| `c`, `h` | `c` |
| `cpp`, `cc`, `hpp` | `cpp` |
| `cs` | `csharp` |
| `rb` | `ruby` |
| `php` | `php` |
| `swift` | `swift` |
| `kt` | `kotlin` |
| `scala` | `scala` |
| `r` | `r` |
| `lua` | `lua` |
| *(other)* | `plaintext` |

Approximately **50 extensions** are mapped in total.

---

## MIME Type Detection

A comprehensive extension-to-MIME-type mapping covers:

| Category | Examples |
|---|---|
| **Documents** | `pdf`, `doc`, `docx`, `xls`, `xlsx`, `ppt`, `pptx` |
| **Images** | `png`, `jpg`, `gif`, `webp`, `svg`, `ico`, `bmp`, `tiff`, `avif` |
| **Video** | `mp4`, `webm`, `mkv`, `avi`, `mov`, `ogv` |
| **Audio** | `mp3`, `ogg`, `wav`, `flac`, `aac`, `m4a`, `opus` |
| **Text / Code** | All ~50 code extensions (`text/plain` or specific types) |
| **Archives** | `zip`, `tar`, `gz`, `bz2`, `xz`, `7z`, `rar` |

**Fallback:** `application/octet-stream`

---

## Path Security

All user-supplied paths pass through a `resolve()` function that enforces
strict sandboxing:

1. **Strip** leading `/`.
2. **Replace** all `\` with `/`.
3. **Reject** `..` (`ParentDir`) components.
4. **Reject** absolute paths and Windows prefix components.
5. **Canonicalize** the result and verify it does not escape the configured
   root directory.
6. For paths to files that do not yet exist, the **parent directory** is
   checked instead.

---

## Known Limitations

| Area | Limitation |
|---|---|
| **Thumbnails** | No server-side resizing — the full image is served and scaled by the browser via CSS. |
| **Share state** | Download counts are in-memory only and reset on restart. |
| **Text editor** | Limited to files of 2 MB or smaller. |
| **Share passwords** | Use SHA-256 (not Argon2 like user passwords). |
| **Versioning** | No file versioning or edit history. |
| **Upload** | Single-file upload only via API — no drag-and-drop folder upload. |
| **ZIP downloads** | Built entirely in memory; may be problematic for very large directories. |
| **Concurrency** | No file locking or concurrent edit detection. |
| **Search** | Filename-only substring search; no full-text content search. |
