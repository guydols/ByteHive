use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientStatus {
    Pending,
    Allowed,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownClient {
    pub node_id: String,
    pub fingerprint: String,
    #[serde(default)]
    pub label: String,
    pub status: ClientStatus,
    #[serde(default)]
    pub addr: String,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
}

#[derive(serde::Deserialize, Default)]
struct KnownClientsRaw {
    #[serde(default)]
    filesync_known_clients: Vec<KnownClient>,
}

#[derive(serde::Serialize)]
struct FilesyncKnownClientsSection {
    filesync_known_clients: Vec<KnownClient>,
}

pub struct KnownClients {
    config_path: PathBuf,
    clients: Vec<KnownClient>,
    auto_approve: bool,
}

impl KnownClients {
    pub fn load_from_config(config_path: impl Into<PathBuf>) -> Self {
        Self::load_inner(config_path.into(), false)
    }

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

    pub fn status(&self, fingerprint: &str) -> Option<ClientStatus> {
        self.clients
            .iter()
            .find(|c| c.fingerprint == fingerprint)
            .map(|c| c.status.clone())
    }

    pub fn list(&self) -> &[KnownClient] {
        &self.clients
    }

    pub fn pending_count(&self) -> usize {
        self.clients
            .iter()
            .filter(|c| c.status == ClientStatus::Pending)
            .count()
    }

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

    pub fn remove(&mut self, fingerprint: &str) -> bool {
        let before = self.clients.len();
        self.clients.retain(|c| c.fingerprint != fingerprint);
        let changed = self.clients.len() != before;
        if changed {
            self.save();
        }
        changed
    }

    fn save(&self) {
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

fn splice_known_clients(original: &str, clients: &[KnownClient]) -> String {
    let new_section = if clients.is_empty() {
        String::new()
    } else {
        match toml::to_string_pretty(&FilesyncKnownClientsSection {
            filesync_known_clients: clients.to_vec(),
        }) {
            Ok(s) => s,
            Err(e) => {
                log::error!("filesync: failed to serialize known_clients: {e}");
                return original.to_string();
            }
        }
    };

    let mut before: Vec<&str> = Vec::new();
    let mut after: Vec<&str> = Vec::new();
    let mut found_first = false;
    let mut in_section = false;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownServer {
    pub addr: String,
    pub fingerprint: String,
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

    pub fn get_fingerprint(&self, addr: &str) -> Option<&str> {
        self.inner
            .servers
            .iter()
            .find(|s| s.addr == addr)
            .map(|s| s.fingerprint.as_str())
    }

    pub fn list(&self) -> &[KnownServer] {
        &self.inner.servers
    }

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

    pub fn remove(&mut self, addr: &str) -> bool {
        let before = self.inner.servers.len();
        self.inner.servers.retain(|s| s.addr != addr);
        let changed = self.inner.servers.len() != before;
        if changed {
            self.save();
        }
        changed
    }

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
