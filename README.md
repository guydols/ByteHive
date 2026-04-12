# ByteHive

A framework and personal cloud server. A central message bus and HTTP control plane
for focused, composable applications.

See [ARCHITECTURE.md](ARCHITECTURE.md) for system design, module map, and the `App` trait contract.

> [!WARNING]  
> This software is in an experimental stage, I cannot guarantee (100%) file integrity.

## Build

```bash
# Prerequisites: Rust stable, Linux (inotify)
cargo build --release
./target/release/bytehive --config config.toml
```

On first run with no `[[users]]` entries the server redirects to `/setup` for interactive
admin password creation.

## Configuration

```toml
[framework]
http_addr  = "0.0.0.0:9000"   # portal and API
http_token = ""                # optional static admin token; prefer named [[api_keys]]
log_level  = "info"

[apps.filesync]
root      = "/path/to/folder"
mode      = "server"            # or "client"
bind_addr = "0.0.0.0:7878"
# server_addr = "host:7878"     # client mode
# auth_token  = "<api-key>"     # client mode
```

See `config.toml` for the full reference including user accounts, groups, API keys, and
exclusion rules. See [crates/filesync/README.md](crates/filesync/README.md) and
[crates/filebrowser/README.md](crates/filebrowser/README.md) for app-specific options.
