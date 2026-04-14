//! Persistent known-host tables for the filesync TLS identity model.
//!
//! # Server side — `KnownClients`
//! The server stores each client's certificate fingerprint directly inside
//! the main `config.toml` under `[[filesync_known_clients]]` array-of-tables
//! sections.  Writes splice the section in-place so that the rest of the
//! config file (e.g. `[framework]`, `[[users]]`, `[apps.filesync]`, …) is
//! preserved verbatim.
//!
//! Each entry can be in one of three states:
//!
//! * `Pending`  — the client has connected at least once but has not yet
//!               been approved by an administrator.
//! * `Allowed`  — an administrator explicitly approved this client; it may
//!               sync freely.
//! * `Rejected` — an administrator explicitly rejected this client; it is
//!               turned away at the door.
//!
//! When a client in `Pending` or `Rejected` state tries to connect the
//! server sends back a `Message::ApprovalPending` or `Message::Rejected`
//! application-layer message and closes the connection.  The client retries
//! with back-off until it is `Allowed`.
//!
//! # Client side — `KnownServers`
//! The client uses a simple Trust-On-First-Use (TOFU) model.  On the first
//! successful TLS handshake with a given server address the server's
//! certificate fingerprint is recorded in a separate `known_servers.toml`
//! file.  On every subsequent connection the stored fingerprint must match;
//! a mismatch aborts the session with an error (possible MITM or cert
//! rotation).
//!
//! Deleting `known_servers.toml` re-enables TOFU for all servers (useful
//! when a server has legitimately regenerated its certificate).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

// ── helpers ──────────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ─────────────────────────────────────────────────────────────────────────────
// ClientStatus
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientStatus {
    /// Seen but not yet approved.
    Pending,
    /// Explicitly approved by an administrator.
    Allowed,
    /// Explicitly rejected by an administrator.
    Rejected,
}

impl ClientStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Allowed => "allowed",
            Self::Rejected => "rejected",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// KnownClient
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownClient {
    /// The node_id the client reported in its Hello message.
    pub node_id: String,

    /// BLAKE3 hex fingerprint of the client's DER certificate.
    pub fingerprint: String,

    /// Optional human-friendly label (editable by the admin).
    #[serde(default)]
    pub label: String,

    /// Current authorization state.
    pub status: ClientStatus,

    /// Last observed remote address (IP:port).
    #[serde(default)]
    pub addr: String,

    /// Unix-millisecond timestamp of the first connection attempt.
    pub first_seen_ms: u64,

    /// Unix-millisecond timestamp of the most recent connection attempt.
    pub last_seen_ms: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// KnownClients (server-side store) — reads/writes [[filesync_known_clients]]
// inside the main config.toml via the splice mechanism.
// ─────────────────────────────────────────────────────────────────────────────

/// Wrapper used to deserialise `[[filesync_known_clients]]` out of the full
/// config file.  All other keys in the file are ignored.
#[derive(serde::Deserialize, Default)]
struct KnownClientsRaw {
    #[serde(default)]
    filesync_known_clients: Vec<KnownClient>,
}

/// Wrapper used to serialise only the `[[filesync_known_clients]]` section
/// so that `toml::to_string_pretty` emits the correct array-of-tables header.
#[derive(serde::Serialize)]
struct FilesyncKnownClientsSection {
    filesync_known_clients: Vec<KnownClient>,
}

pub struct KnownClients {
    /// Full path to the main `config.toml` that holds the
    /// `[[filesync_known_clients]]` entries alongside all other config.
    config_path: PathBuf,
    clients: Vec<KnownClient>,
    /// When `true`, unknown clients are immediately approved instead of placed
    /// in the `Pending` state.  Only intended for benchmarks and tests.
    auto_approve: bool,
}

impl KnownClients {
    /// Load the known-clients list from the `[[filesync_known_clients]]`
    /// sections inside `config_path` (the main `config.toml`).  If the file
    /// does not exist or contains no such sections, the store starts empty.
    /// Parse errors are logged and fall back to empty.
    pub fn load_from_config(config_path: impl Into<PathBuf>) -> Self {
        Self::load_inner(config_path.into(), false)
    }

    /// Like `load_from_config`, but any unknown client is automatically
    /// approved without requiring admin action.  Intended for benchmarks and
    /// integration tests only — do not use in production.
    pub fn load_from_config_permissive(config_path: impl Into<PathBuf>) -> Self {
        Self::load_inner(config_path.into(), true)
    }

    fn load_inner(config_path: PathBuf, auto_approve: bool) -> Self {
        let clients = if config_path.exists() {
            match std::fs::read_to_string(&config_path) {
                Ok(s) => toml::from_str::<KnownClientsRaw>(&s)
                    .map_err(|e| {
                        log::warn!(
                            "filesync: known_clients parse error ({config_path:?}): {e} — starting empty"
                        );
                    })
                    .map(|raw| raw.filesync_known_clients)
                    .unwrap_or_default(),
                Err(e) => {
                    log::warn!("filesync: cannot read {config_path:?}: {e} — starting empty");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        Self {
            config_path,
            clients,
            auto_approve,
        }
    }

    // ── queries ──────────────────────────────────────────────────────────────

    /// Return the current status of a fingerprint, or `None` if unknown.
    pub fn status(&self, fingerprint: &str) -> Option<ClientStatus> {
        self.clients
            .iter()
            .find(|c| c.fingerprint == fingerprint)
            .map(|c| c.status.clone())
    }

    /// Immutable view of all known clients.
    pub fn list(&self) -> &[KnownClient] {
        &self.clients
    }

    /// Number of clients currently in `Pending` state.
    pub fn pending_count(&self) -> usize {
        self.clients
            .iter()
            .filter(|c| c.status == ClientStatus::Pending)
            .count()
    }

    // ── mutations ────────────────────────────────────────────────────────────

    /// Record a new connection attempt.
    ///
    /// * If the fingerprint is already in the table, update `last_seen_ms`
    ///   and return `false` (not new).
    /// * If the fingerprint is new, insert a `Pending` entry and return
    ///   `true` (new — caller should publish an approval-needed bus event).
    pub fn upsert_pending(&mut self, node_id: &str, fingerprint: &str, addr: &str) -> bool {
        let now = now_ms();
        if let Some(c) = self
            .clients
            .iter_mut()
            .find(|c| c.fingerprint == fingerprint)
        {
            c.last_seen_ms = now;
            if c.node_id.is_empty() {
                c.node_id = node_id.to_string();
            }
            if !addr.is_empty() {
                c.addr = addr.to_string();
            }
            self.save();
            false
        } else {
            // In permissive/bench mode, skip the pending state and approve
            // the client immediately.
            let status = if self.auto_approve {
                ClientStatus::Allowed
            } else {
                ClientStatus::Pending
            };
            self.clients.push(KnownClient {
                node_id: node_id.to_string(),
                fingerprint: fingerprint.to_string(),
                label: String::new(),
                status,
                addr: addr.to_string(),
                first_seen_ms: now,
                last_seen_ms: now,
            });
            self.save();
            true
        }
    }

    /// Change the status of a client identified by its fingerprint.
    ///
    /// Returns `true` on success, `false` if the fingerprint was not found.
    pub fn set_status(&mut self, fingerprint: &str, status: ClientStatus) -> bool {
        match self
            .clients
            .iter_mut()
            .find(|c| c.fingerprint == fingerprint)
        {
            Some(c) => {
                c.status = status;
                self.save();
                true
            }
            None => false,
        }
    }

    /// Update the human-readable label for a client.
    pub fn set_label(&mut self, fingerprint: &str, label: &str) -> bool {
        match self
            .clients
            .iter_mut()
            .find(|c| c.fingerprint == fingerprint)
        {
            Some(c) => {
                c.label = label.to_string();
                self.save();
                true
            }
            None => false,
        }
    }

    /// Remove a client entry entirely.
    ///
    /// Returns `true` if an entry was removed.
    pub fn remove(&mut self, fingerprint: &str) -> bool {
        let before = self.clients.len();
        self.clients.retain(|c| c.fingerprint != fingerprint);
        let changed = self.clients.len() != before;
        if changed {
            self.save();
        }
        changed
    }

    // ── persistence ──────────────────────────────────────────────────────────

    fn save(&self) {
        // Read the current config file so we can splice the new section in
        // while preserving all other content.
        let original = if self.config_path.exists() {
            match std::fs::read_to_string(&self.config_path) {
                Ok(s) => s,
                Err(e) => {
                    log::error!(
                        "filesync: cannot read {:?} for known_clients save: {e}",
                        self.config_path
                    );
                    return;
                }
            }
        } else {
            String::new()
        };

        let new_content = splice_known_clients(&original, &self.clients);

        if let Some(parent) = self.config_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::error!(
                    "filesync: cannot create directory {:?} for known_clients: {e}",
                    parent
                );
                return;
            }
        }

        if let Err(e) = std::fs::write(&self.config_path, &new_content) {
            log::error!(
                "filesync: failed to write known_clients to {:?}: {e}",
                self.config_path
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// splice_known_clients — in-place rewrite of [[filesync_known_clients]] blocks
// ─────────────────────────────────────────────────────────────────────────────

/// Rewrite the `[[filesync_known_clients]]` array-of-tables sections inside
/// `original` (a full `config.toml` string) with freshly serialised `clients`,
/// leaving every other line untouched.
///
/// * If `clients` is empty the section is removed entirely.
/// * The function mirrors the logic of `splice_auth_sections` in
///   `crates/core/src/users.rs` but targets a single section name.
fn splice_known_clients(original: &str, clients: &[KnownClient]) -> String {
    // Build the replacement section text up front.
    let new_section = if clients.is_empty() {
        String::new()
    } else {
        match toml::to_string_pretty(&FilesyncKnownClientsSection {
            filesync_known_clients: clients.to_vec(),
        }) {
            Ok(s) => s,
            Err(e) => {
                log::error!("filesync: failed to serialize known_clients: {e}");
                // Return original unchanged rather than corrupting the file.
                return original.to_string();
            }
        }
    };

    let mut before: Vec<&str> = Vec::new();
    let mut after: Vec<&str> = Vec::new();
    let mut found_first = false;
    let mut in_section = false;

    // Blank / comment lines that trail an auth block are "pending": we keep
    // them only if the next non-blank line belongs to a non-auth section.
    let mut pending: Vec<&str> = Vec::new();

    for line in original.lines() {
        let trimmed = line.trim();

        let is_array_hdr =
            trimmed.starts_with("[[") && trimmed.ends_with("]]") && !trimmed.starts_with('#');
        let is_table_hdr =
            trimmed.starts_with('[') && !trimmed.starts_with("[[") && !trimmed.starts_with('#');

        if is_array_hdr {
            let inner = &trimmed[2..trimmed.len() - 2];
            let name = inner.trim().to_ascii_lowercase();
            if name == "filesync_known_clients" {
                // Entering our managed section — discard any trailing
                // whitespace that followed the previous entry.
                pending.clear();
                in_section = true;
                found_first = true;
                continue;
            } else {
                in_section = false;
                let target = if found_first { &mut after } else { &mut before };
                target.extend(pending.drain(..));
                target.push(line);
                continue;
            }
        }

        if is_table_hdr {
            in_section = false;
            let target = if found_first { &mut after } else { &mut before };
            target.extend(pending.drain(..));
            target.push(line);
            continue;
        }

        if in_section {
            // Inside a managed block: buffer blank/comment lines; drop value
            // lines (they belong to the old entry we're replacing).
            if trimmed.is_empty() || trimmed.starts_with('#') {
                pending.push(line);
            } else {
                pending.clear();
            }
        } else {
            let target = if found_first { &mut after } else { &mut before };
            target.extend(pending.drain(..));
            target.push(line);
        }
    }

    // ── reassemble ───────────────────────────────────────────────────────────

    let before_str = before.join("\n");
    let after_str = after.join("\n");

    let mut out = before_str.trim_end().to_string();

    if !new_section.trim().is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(new_section.trim_end());
        out.push('\n');
    }

    let after_trimmed = after_str.trim_start();
    if !after_trimmed.is_empty() {
        out.push('\n');
        out.push_str(after_trimmed);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    } else if !out.ends_with('\n') {
        out.push('\n');
    }

    out
}

// ─────────────────────────────────────────────────────────────────────────────
// KnownServer / KnownServers  (client-side TOFU)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownServer {
    /// The `host:port` address the client connected to.
    pub addr: String,

    /// BLAKE3 hex fingerprint of the server's DER certificate.
    pub fingerprint: String,

    /// Unix-millisecond timestamp of the first time this server was trusted.
    pub first_seen_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct KnownServersFile {
    #[serde(default)]
    servers: Vec<KnownServer>,
}

pub struct KnownServers {
    path: PathBuf,
    inner: KnownServersFile,
}

impl KnownServers {
    /// Load the store from `path` (a dedicated `known_servers.toml`),
    /// creating an empty store if the file does not yet exist.
    pub fn load_or_create(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let inner = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(s) => toml::from_str(&s)
                    .map_err(|e| {
                        log::warn!(
                            "filesync: known_servers parse error ({path:?}): {e} — starting empty"
                        );
                    })
                    .unwrap_or_default(),
                Err(e) => {
                    log::warn!("filesync: cannot read {path:?}: {e} — starting empty");
                    KnownServersFile::default()
                }
            }
        } else {
            KnownServersFile::default()
        };

        Self { path, inner }
    }

    // ── queries ──────────────────────────────────────────────────────────────

    /// Look up the pinned fingerprint for a server address.
    ///
    /// Returns `None` if the server has never been trusted (TOFU first-use).
    pub fn get_fingerprint(&self, addr: &str) -> Option<&str> {
        self.inner
            .servers
            .iter()
            .find(|s| s.addr == addr)
            .map(|s| s.fingerprint.as_str())
    }

    /// Immutable view of all known servers.
    pub fn list(&self) -> &[KnownServer] {
        &self.inner.servers
    }

    // ── mutations ────────────────────────────────────────────────────────────

    /// Pin (or re-pin) the fingerprint for a server address.
    ///
    /// If the address is already in the table the fingerprint is updated and
    /// the `first_seen_ms` timestamp is preserved.
    pub fn pin(&mut self, addr: &str, fingerprint: &str) {
        if let Some(s) = self.inner.servers.iter_mut().find(|s| s.addr == addr) {
            s.fingerprint = fingerprint.to_string();
        } else {
            self.inner.servers.push(KnownServer {
                addr: addr.to_string(),
                fingerprint: fingerprint.to_string(),
                first_seen_ms: now_ms(),
            });
        }
        self.save();
    }

    /// Remove a server entry (use after cert rotation to re-enable TOFU).
    pub fn remove(&mut self, addr: &str) -> bool {
        let before = self.inner.servers.len();
        self.inner.servers.retain(|s| s.addr != addr);
        let changed = self.inner.servers.len() != before;
        if changed {
            self.save();
        }
        changed
    }

    // ── persistence ──────────────────────────────────────────────────────────

    fn save(&self) {
        if let Some(parent) = self.path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::error!(
                    "filesync: cannot create directory {:?} for known_servers: {e}",
                    parent
                );
                return;
            }
        }
        match toml::to_string_pretty(&self.inner) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&self.path, &s) {
                    log::error!(
                        "filesync: failed to write known_servers {:?}: {e}",
                        self.path
                    );
                }
            }
            Err(e) => {
                log::error!("filesync: failed to serialize known_servers: {e}");
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Return a unique path in the system temp dir for each test invocation.
    fn tmp_path(name: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bh_kh_unit_{n}_{name}"))
    }

    // ── splice_known_clients unit tests ──────────────────────────────────────

    #[test]
    fn splice_into_empty_string_produces_section() {
        let out = splice_known_clients(
            "",
            &[KnownClient {
                node_id: "n1".into(),
                fingerprint: "fp1".into(),
                label: String::new(),
                status: ClientStatus::Pending,
                addr: "1.2.3.4:1".into(),
                first_seen_ms: 1,
                last_seen_ms: 1,
            }],
        );
        assert!(out.contains("[[filesync_known_clients]]"));
        assert!(out.contains("fp1"));
    }

    #[test]
    fn splice_empty_clients_removes_section() {
        let original = "[framework]\nname = \"test\"\n\n[[filesync_known_clients]]\nfingerprint = \"fp1\"\nnode_id = \"n1\"\nstatus = \"pending\"\nfirst_seen_ms = 1\nlast_seen_ms = 1\n";
        let out = splice_known_clients(original, &[]);
        assert!(!out.contains("filesync_known_clients"));
        assert!(out.contains("[framework]"));
    }

    #[test]
    fn splice_preserves_surrounding_config() {
        let original = "[framework]\nname = \"test\"\n\n[[filesync_known_clients]]\nfingerprint = \"old\"\nnode_id = \"n\"\nstatus = \"pending\"\nfirst_seen_ms = 1\nlast_seen_ms = 1\n\n[apps.filesync]\nroot = \"/data\"\n";
        let replacement = vec![KnownClient {
            node_id: "n".into(),
            fingerprint: "new".into(),
            label: String::new(),
            status: ClientStatus::Allowed,
            addr: String::new(),
            first_seen_ms: 1,
            last_seen_ms: 2,
        }];
        let out = splice_known_clients(original, &replacement);
        assert!(out.contains("[framework]"));
        assert!(out.contains("[apps.filesync]"));
        assert!(out.contains("new"));
        assert!(!out.contains("\"old\""));
    }

    #[test]
    fn splice_roundtrip_multiple_entries() {
        let clients = vec![
            KnownClient {
                node_id: "a".into(),
                fingerprint: "fp-a".into(),
                label: String::new(),
                status: ClientStatus::Allowed,
                addr: "1.1.1.1:1".into(),
                first_seen_ms: 10,
                last_seen_ms: 20,
            },
            KnownClient {
                node_id: "b".into(),
                fingerprint: "fp-b".into(),
                label: "my label".into(),
                status: ClientStatus::Rejected,
                addr: "2.2.2.2:2".into(),
                first_seen_ms: 30,
                last_seen_ms: 40,
            },
        ];
        let spliced = splice_known_clients("", &clients);
        let parsed: KnownClientsRaw = toml::from_str(&spliced).expect("valid toml");
        assert_eq!(parsed.filesync_known_clients.len(), 2);
        assert_eq!(parsed.filesync_known_clients[0].fingerprint, "fp-a");
        assert_eq!(parsed.filesync_known_clients[1].fingerprint, "fp-b");
        assert_eq!(parsed.filesync_known_clients[1].label, "my label");
    }

    // ── KnownClients integration tests ───────────────────────────────────────

    #[test]
    fn new_client_upsert_returns_true() {
        let p = tmp_path("kc1.toml");
        let mut kc = KnownClients::load_from_config(&p);
        assert!(kc.upsert_pending("node-1", "fp-aaa", "127.0.0.1:1234"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn repeat_upsert_returns_false() {
        let p = tmp_path("kc2.toml");
        let mut kc = KnownClients::load_from_config(&p);
        kc.upsert_pending("node-1", "fp-bbb", "127.0.0.1:1");
        assert!(!kc.upsert_pending("node-1", "fp-bbb", "127.0.0.1:2"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn new_client_has_pending_status() {
        let p = tmp_path("kc3.toml");
        let mut kc = KnownClients::load_from_config(&p);
        kc.upsert_pending("node-1", "fp-ccc", "127.0.0.1:1");
        assert_eq!(kc.status("fp-ccc"), Some(ClientStatus::Pending));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn approve_changes_status_to_allowed() {
        let p = tmp_path("kc4.toml");
        let mut kc = KnownClients::load_from_config(&p);
        kc.upsert_pending("node-1", "fp-ddd", "127.0.0.1:1");
        assert!(kc.set_status("fp-ddd", ClientStatus::Allowed));
        assert_eq!(kc.status("fp-ddd"), Some(ClientStatus::Allowed));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn set_status_unknown_fingerprint_returns_false() {
        let p = tmp_path("kc5.toml");
        let mut kc = KnownClients::load_from_config(&p);
        assert!(!kc.set_status("no-such-fp", ClientStatus::Allowed));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn remove_deletes_entry() {
        let p = tmp_path("kc6.toml");
        let mut kc = KnownClients::load_from_config(&p);
        kc.upsert_pending("node-1", "fp-eee", "127.0.0.1:1");
        assert!(kc.remove("fp-eee"));
        assert_eq!(kc.status("fp-eee"), None);
        assert!(!kc.remove("fp-eee")); // second remove returns false
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn pending_count_is_accurate() {
        let p = tmp_path("kc7.toml");
        let mut kc = KnownClients::load_from_config(&p);
        kc.upsert_pending("n1", "fp-f1", "");
        kc.upsert_pending("n2", "fp-f2", "");
        kc.upsert_pending("n3", "fp-f3", "");
        assert_eq!(kc.pending_count(), 3);
        kc.set_status("fp-f1", ClientStatus::Allowed);
        assert_eq!(kc.pending_count(), 2);
        kc.set_status("fp-f2", ClientStatus::Rejected);
        assert_eq!(kc.pending_count(), 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn persists_and_reloads() {
        let p = tmp_path("kc8.toml");
        {
            let mut kc = KnownClients::load_from_config(&p);
            kc.upsert_pending("node-1", "fp-ppp", "10.0.0.1:7878");
            kc.set_status("fp-ppp", ClientStatus::Allowed);
        }
        // reload from disk
        let kc2 = KnownClients::load_from_config(&p);
        assert_eq!(kc2.status("fp-ppp"), Some(ClientStatus::Allowed));
        assert_eq!(kc2.list()[0].addr, "10.0.0.1:7878");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_nonexistent_file_gives_empty_store() {
        let p = tmp_path("kc9_nonexistent.toml");
        let kc = KnownClients::load_from_config(&p);
        assert_eq!(kc.list().len(), 0);
    }

    #[test]
    fn persists_alongside_existing_config_content() {
        let p = tmp_path("kc10.toml");
        // Seed the "config file" with some pre-existing content.
        std::fs::write(
            &p,
            "[framework]\nname = \"test\"\n\n[apps.filesync]\nroot = \"/tmp\"\n",
        )
        .unwrap();

        let mut kc = KnownClients::load_from_config(&p);
        kc.upsert_pending("node-x", "fp-xyz", "9.9.9.9:1");

        // Reload and verify the client was saved.
        let kc2 = KnownClients::load_from_config(&p);
        assert_eq!(kc2.status("fp-xyz"), Some(ClientStatus::Pending));

        // The rest of the config must still be intact.
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("[framework]"));
        assert!(raw.contains("[apps.filesync]"));
        assert!(raw.contains("[[filesync_known_clients]]"));

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn remove_last_entry_strips_section_from_config() {
        let p = tmp_path("kc11.toml");
        std::fs::write(&p, "[framework]\nname = \"test\"\n").unwrap();

        let mut kc = KnownClients::load_from_config(&p);
        kc.upsert_pending("n", "fp-del", "");
        kc.remove("fp-del");

        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("filesync_known_clients"));
        assert!(raw.contains("[framework]"));

        let _ = std::fs::remove_file(&p);
    }

    // ── KnownServers tests (unchanged) ────────────────────────────────────────

    #[test]
    fn first_pin_is_tofu() {
        let p = tmp_path("ks1.toml");
        let mut ks = KnownServers::load_or_create(&p);
        assert!(ks.get_fingerprint("server:7878").is_none());
        ks.pin("server:7878", "abcdef");
        assert_eq!(ks.get_fingerprint("server:7878"), Some("abcdef"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn re_pin_updates_fingerprint() {
        let p = tmp_path("ks2.toml");
        let mut ks = KnownServers::load_or_create(&p);
        ks.pin("server:7878", "old-fp");
        ks.pin("server:7878", "new-fp");
        assert_eq!(ks.get_fingerprint("server:7878"), Some("new-fp"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn remove_server_clears_pin() {
        let p = tmp_path("ks3.toml");
        let mut ks = KnownServers::load_or_create(&p);
        ks.pin("srv:1", "fp-x");
        assert!(ks.remove("srv:1"));
        assert!(ks.get_fingerprint("srv:1").is_none());
        assert!(!ks.remove("srv:1"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn server_persists_and_reloads() {
        let p = tmp_path("ks4.toml");
        {
            let mut ks = KnownServers::load_or_create(&p);
            ks.pin("192.168.1.10:7878", "server-fingerprint-hex");
        }
        let ks2 = KnownServers::load_or_create(&p);
        assert_eq!(
            ks2.get_fingerprint("192.168.1.10:7878"),
            Some("server-fingerprint-hex")
        );
        let _ = std::fs::remove_file(&p);
    }
}
