# filebrowser

Web-based file browser for the filesync root directory. Runs alongside filesync.

## Configuration

```toml
[apps.filebrowser]
max_upload_mb = 200   # default 100
allow_delete  = false # default false
```

## HTTP API

All routes require authentication. Served under `/api/filebrowser/`.

| Route | Method | Description |
|-------|--------|-------------|
| `/list` | GET | Directory listing (`?path=`) |
| `/download` | GET | Download file or directory as zip |
| `/preview` | GET | Inline file preview |
| `/thumb` | GET | Image thumbnail (delegates to preview) |
| `/read` | GET | File contents for editor; `?force=1` bypasses type check |
| `/write` | POST | Save file contents |
| `/mkdir` | POST | Create directory |
| `/delete` | DELETE | Delete (requires `allow_delete = true`) |
| `/rename` | POST | Rename |
| `/copy` | POST | Copy |
| `/detect` | GET | File type detection (text/binary, Monaco language ID) |
| `/search` | GET | Recursive filename search |
| `/share` | POST | Create share link |
| `/shares` | GET | List active shares |
| `/share` | DELETE | Delete share |

Public share access (no auth): `GET/POST /s/:token`

## Features

- Grid and list views with sorting
- Monaco editor for text files with syntax highlighting
- Image / video / audio / PDF preview lightbox
- Directory download as zip
- Password-protected and expiring share links
- File type detection: known-extension list + 8 KB binary-sniff heuristic
- File operations: upload, download, rename, copy, delete, mkdir

## Limitations

- Thumbnails are not resized — full image served, browser CSS-scales it
- Share download count is in-memory only and resets on restart
- Delete is disabled by default (`allow_delete = false`)
