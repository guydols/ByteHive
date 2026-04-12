use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const GROUP_ADMIN: &str = "admin";
pub const GROUP_USER: &str = "user";
const PROTECTED_GROUPS: &[&str] = &[GROUP_ADMIN, GROUP_USER];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserEntry {
    pub username: String,

    pub password_hash: String,
    #[serde(default)]
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub name: String,
    pub key: String,
    #[serde(default)]
    pub as_user: String,
    #[serde(default)]
    pub expires_ms: Option<u64>,
    #[serde(default)]
    pub created_at: u64,
}

impl ApiKey {
    pub fn is_expired(&self) -> bool {
        self.expires_ms.map(|e| now_ms() > e).unwrap_or(false)
    }
    pub fn effective_username(&self) -> String {
        if self.as_user.is_empty() {
            format!("api-{}", self.name)
        } else {
            self.as_user.clone()
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiKeyInfo {
    pub name: String,
    pub as_user: String,
    pub expires_ms: Option<u64>,
    pub created_at: u64,
    pub expired: bool,
}

impl From<&ApiKey> for ApiKeyInfo {
    fn from(k: &ApiKey) -> Self {
        ApiKeyInfo {
            name: k.name.clone(),
            as_user: k.effective_username(),
            expires_ms: k.expires_ms,
            created_at: k.created_at,
            expired: k.is_expired(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AuthMethod {
    Session,
    ApiKey { key_name: String },
    AdminToken,
    DevMode,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthContext {
    pub username: String,
    pub display_name: String,
    pub groups: Vec<String>,
    pub method: AuthMethod,
}

impl AuthContext {
    pub fn is_admin(&self) -> bool {
        self.groups.iter().any(|g| g == GROUP_ADMIN)
    }
    pub fn can_write(&self) -> bool {
        self.groups
            .iter()
            .any(|g| g == GROUP_ADMIN || g == GROUP_USER)
    }
    pub fn in_group(&self, name: &str) -> bool {
        self.groups.iter().any(|g| g == name)
    }

    pub fn dev_admin() -> Self {
        AuthContext {
            username: "admin".into(),
            display_name: "Admin (dev)".into(),
            groups: vec![GROUP_ADMIN.into(), GROUP_USER.into()],
            method: AuthMethod::DevMode,
        }
    }
    pub fn admin_token() -> Self {
        AuthContext {
            username: "admin".into(),
            display_name: "Admin (token)".into(),
            groups: vec![GROUP_ADMIN.into(), GROUP_USER.into()],
            method: AuthMethod::AdminToken,
        }
    }
    fn for_api_key(key: &ApiKey) -> Self {
        AuthContext {
            username: key.effective_username(),
            display_name: format!("API Key: {}", key.name),
            groups: vec![GROUP_ADMIN.into(), GROUP_USER.into()],
            method: AuthMethod::ApiKey {
                key_name: key.name.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub token: String,
    pub username: String,
    pub display_name: String,
    pub expires_ms: u64,
}

impl Session {
    pub fn ttl_secs(&self) -> u64 {
        let now = now_ms();
        if self.expires_ms > now {
            (self.expires_ms - now) / 1000
        } else {
            0
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UserInfo {
    pub username: String,
    pub display_name: String,
    pub groups: Vec<String>,
}

pub struct UserStore {
    users: RwLock<Vec<UserEntry>>,
    groups: RwLock<Vec<Group>>,
    api_keys: RwLock<Vec<ApiKey>>,
    sessions: RwLock<HashMap<String, Session>>,
    pub admin_token: String,
    session_ttl_ms: u64,
    config_path: Option<PathBuf>,
    raw_config: RwLock<String>,
}

impl UserStore {
    pub fn new(
        users: Vec<UserEntry>,
        groups: Vec<Group>,
        api_keys: Vec<ApiKey>,
        admin_token: impl Into<String>,
        config_path: Option<PathBuf>,
        raw_config: String,
    ) -> Arc<Self> {
        let mut groups = groups;
        for (name, desc) in [
            (
                GROUP_ADMIN,
                "Administrators — full access and ops dashboard",
            ),
            (GROUP_USER, "Standard users — read/write access to app APIs"),
        ] {
            if !groups.iter().any(|g| g.name == name) {
                groups.push(Group {
                    name: name.into(),
                    description: desc.into(),
                    members: vec![],
                });
            }
        }
        Arc::new(Self {
            users: RwLock::new(users),
            groups: RwLock::new(groups),
            api_keys: RwLock::new(api_keys),
            sessions: RwLock::new(HashMap::new()),
            admin_token: admin_token.into(),
            session_ttl_ms: 8 * 3600 * 1000,
            config_path,
            raw_config: RwLock::new(raw_config),
        })
    }

    pub fn empty() -> Arc<Self> {
        Self::new(vec![], vec![], vec![], "", None, String::new())
    }

    pub fn has_users(&self) -> bool {
        !self.users.read().is_empty()
    }

    pub fn needs_setup(&self) -> bool {
        self.users.read().is_empty()
    }

    pub fn complete_setup(&self, password: &str) -> Result<(), String> {
        if !self.needs_setup() {
            return Err("setup has already been completed".into());
        }
        if password.len() < 8 {
            return Err("password must be at least 8 characters".into());
        }

        {
            let mut users = self.users.write();
            let mut groups = self.groups.write();

            users.push(UserEntry {
                username: "admin".into(),
                display_name: "Administrator".into(),
                password_hash: Self::hash_password(password),
            });

            for g in groups.iter_mut() {
                if g.name == GROUP_ADMIN || g.name == GROUP_USER {
                    if !g.members.contains(&"admin".to_string()) {
                        g.members.push("admin".into());
                    }
                }
            }
        }

        self.persist();
        log::info!("first-run setup complete — admin user created");
        Ok(())
    }

    pub fn hash_password(password: &str) -> String {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .expect("Argon2 hashing failed — OS random source unavailable")
            .to_string()
    }

    pub fn verify_password(stored_hash: &str, password: &str) -> Result<bool, VerifyError> {
        if is_legacy_sha256(stored_hash) {
            return Err(VerifyError::LegacyHash);
        }

        let parsed = PasswordHash::new(stored_hash).map_err(|_| VerifyError::InvalidFormat)?;

        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok())
    }

    pub fn login(&self, username: &str, password: &str) -> Option<Session> {
        let users = self.users.read();
        let user = users.iter().find(|u| u.username == username)?;

        match Self::verify_password(&user.password_hash, password) {
            Ok(true) => {}
            Ok(false) => {
                log::warn!("login: wrong password for '{username}'");
                return None;
            }
            Err(VerifyError::LegacyHash) => {
                log::error!(
                    "login: user '{username}' has a legacy SHA-256 password hash. \
                     Reset their password via the admin panel or \
                     `bytehive hash-password <new-password>`."
                );
                return None;
            }
            Err(VerifyError::InvalidFormat) => {
                log::error!("login: user '{username}' has an unrecognised password_hash format");
                return None;
            }
        }

        let display_name = if user.display_name.is_empty() {
            user.username.clone()
        } else {
            user.display_name.clone()
        };

        let session = Session {
            token: Uuid::new_v4().to_string(),
            username: user.username.clone(),
            display_name,
            expires_ms: now_ms() + self.session_ttl_ms,
        };
        self.sessions
            .write()
            .insert(session.token.clone(), session.clone());
        Some(session)
    }

    pub fn validate(&self, token: &str) -> Option<Session> {
        let sessions = self.sessions.read();
        let s = sessions.get(token)?;
        if s.expires_ms < now_ms() {
            return None;
        }
        Some(s.clone())
    }

    pub fn refresh(&self, token: &str) {
        let new_expiry = now_ms() + self.session_ttl_ms;
        if let Some(s) = self.sessions.write().get_mut(token) {
            s.expires_ms = new_expiry;
        }
    }

    pub fn logout(&self, token: &str) {
        self.sessions.write().remove(token);
    }

    pub fn authenticate_credential(&self, credential: &str) -> Option<AuthContext> {
        if let Some(sess) = self.validate(credential) {
            self.refresh(credential);
            let groups = self.groups_for_user(&sess.username);
            return Some(AuthContext {
                username: sess.username,
                display_name: sess.display_name,
                groups,
                method: AuthMethod::Session,
            });
        }
        {
            let keys = self.api_keys.read();
            if let Some(key) = keys
                .iter()
                .find(|k| constant_eq(&k.key, credential) && !k.is_expired())
            {
                return Some(AuthContext::for_api_key(key));
            }
        }
        if !self.admin_token.is_empty() && constant_eq(&self.admin_token, credential) {
            return Some(AuthContext::admin_token());
        }
        None
    }

    pub fn list_users(&self) -> Vec<UserEntry> {
        self.users.read().clone()
    }

    pub fn add_user(&self, entry: UserEntry) -> Result<(), String> {
        {
            let mut users = self.users.write();
            if users.iter().any(|u| u.username == entry.username) {
                return Err(format!("user '{}' already exists", entry.username));
            }
            users.push(entry);
        }
        self.persist();
        Ok(())
    }

    pub fn remove_user(&self, username: &str) -> Result<(), String> {
        {
            let mut users = self.users.write();
            let before = users.len();
            users.retain(|u| u.username != username);
            if users.len() == before {
                return Err(format!("user '{username}' not found"));
            }
        }
        self.sessions.write().retain(|_, s| s.username != username);
        for g in self.groups.write().iter_mut() {
            g.members.retain(|m| m != username);
        }
        self.persist();
        Ok(())
    }

    pub fn update_user(
        &self,
        username: &str,
        new_display_name: Option<String>,
        new_password: Option<&str>,
    ) -> Result<(), String> {
        {
            let mut users = self.users.write();
            let user = users
                .iter_mut()
                .find(|u| u.username == username)
                .ok_or_else(|| format!("user '{username}' not found"))?;
            if let Some(name) = new_display_name {
                user.display_name = name;
            }
            if let Some(pw) = new_password {
                user.password_hash = Self::hash_password(pw);
            }
        }
        self.persist();
        Ok(())
    }

    pub fn list_groups(&self) -> Vec<Group> {
        self.groups.read().clone()
    }

    pub fn add_group(&self, group: Group) -> Result<(), String> {
        {
            let mut groups = self.groups.write();
            if groups.iter().any(|g| g.name == group.name) {
                return Err(format!("group '{}' already exists", group.name));
            }
            groups.push(group);
        }
        self.persist();
        Ok(())
    }

    pub fn remove_group(&self, name: &str) -> Result<(), String> {
        if PROTECTED_GROUPS.contains(&name) {
            return Err(format!("group '{name}' is protected and cannot be removed"));
        }
        {
            let mut groups = self.groups.write();
            let before = groups.len();
            groups.retain(|g| g.name != name);
            if groups.len() == before {
                return Err(format!("group '{name}' not found"));
            }
        }
        self.persist();
        Ok(())
    }

    pub fn add_member_to_group(&self, group_name: &str, username: &str) -> Result<(), String> {
        {
            let mut groups = self.groups.write();
            let group = groups
                .iter_mut()
                .find(|g| g.name == group_name)
                .ok_or_else(|| format!("group '{group_name}' not found"))?;
            if !group.members.contains(&username.to_string()) {
                group.members.push(username.to_string());
            }
        }
        self.persist();
        Ok(())
    }

    pub fn remove_member_from_group(&self, group_name: &str, username: &str) -> Result<(), String> {
        {
            let mut groups = self.groups.write();
            let group = groups
                .iter_mut()
                .find(|g| g.name == group_name)
                .ok_or_else(|| format!("group '{group_name}' not found"))?;
            group.members.retain(|m| m != username);
        }
        self.persist();
        Ok(())
    }

    pub fn groups_for_user(&self, username: &str) -> Vec<String> {
        self.groups
            .read()
            .iter()
            .filter(|g| g.members.contains(&username.to_string()))
            .map(|g| g.name.clone())
            .collect()
    }

    pub fn list_api_keys(&self) -> Vec<ApiKeyInfo> {
        self.api_keys.read().iter().map(ApiKeyInfo::from).collect()
    }

    pub fn create_api_key(
        &self,
        name: impl Into<String>,
        as_user: impl Into<String>,
        expires_ms: Option<u64>,
    ) -> Result<String, String> {
        let name = name.into();
        {
            let mut keys = self.api_keys.write();
            if keys.iter().any(|k| k.name == name) {
                return Err(format!("API key '{name}' already exists"));
            }
            let raw = Uuid::new_v4().to_string();
            keys.push(ApiKey {
                name,
                key: raw.clone(),
                as_user: as_user.into(),
                expires_ms,
                created_at: now_ms(),
            });
            drop(keys);
            self.persist();
            return Ok(raw);
        }
    }

    pub fn revoke_api_key(&self, name: &str) -> Result<(), String> {
        {
            let mut keys = self.api_keys.write();
            let before = keys.len();
            keys.retain(|k| k.name != name);
            if keys.len() == before {
                return Err(format!("API key '{name}' not found"));
            }
        }
        self.persist();
        Ok(())
    }

    pub fn gc(&self) {
        let now = now_ms();
        self.sessions.write().retain(|_, s| s.expires_ms >= now);
        let mut changed = false;
        self.api_keys.write().retain(|k| {
            let keep = !k.expires_ms.map(|e| e < now).unwrap_or(false);
            if !keep {
                changed = true;
            }
            keep
        });
        if changed {
            self.persist();
        }
    }

    fn persist(&self) {
        let Some(path) = &self.config_path else {
            return;
        };

        let users = self.users.read().clone();
        let groups = self.groups.read().clone();
        let api_keys = self.api_keys.read().clone();
        let base = self.raw_config.read().clone();

        let fresh_auth = serialize_auth_sections(&users, &groups, &api_keys);

        let output = splice_auth_sections(&base, &fresh_auth);

        match std::fs::write(path, &output) {
            Ok(()) => {
                *self.raw_config.write() = output;
                log::debug!("config persisted to {:?}", path);
            }
            Err(e) => log::error!("persist: write {:?}: {e}", path),
        }
    }
}

#[derive(serde::Serialize)]
struct UsersSection {
    users: Vec<UserEntry>,
}
#[derive(serde::Serialize)]
struct GroupsSection {
    groups: Vec<Group>,
}
#[derive(serde::Serialize)]
struct ApiKeysSection {
    api_keys: Vec<ApiKey>,
}

fn serialize_auth_sections(users: &[UserEntry], groups: &[Group], keys: &[ApiKey]) -> String {
    let mut out = String::new();

    if !users.is_empty() {
        match toml::to_string_pretty(&UsersSection {
            users: users.to_vec(),
        }) {
            Ok(s) => {
                out.push_str(&s);
                out.push('\n');
            }
            Err(e) => log::error!("persist: serialize users: {e}"),
        }
    }
    if !groups.is_empty() {
        match toml::to_string_pretty(&GroupsSection {
            groups: groups.to_vec(),
        }) {
            Ok(s) => {
                out.push_str(&s);
                out.push('\n');
            }
            Err(e) => log::error!("persist: serialize groups: {e}"),
        }
    }
    if !keys.is_empty() {
        match toml::to_string_pretty(&ApiKeysSection {
            api_keys: keys.to_vec(),
        }) {
            Ok(s) => {
                out.push_str(&s);
                out.push('\n');
            }
            Err(e) => log::error!("persist: serialize api_keys: {e}"),
        }
    }

    out
}

fn splice_auth_sections(original: &str, fresh_auth: &str) -> String {
    const AUTH_NAMES: &[&str] = &["users", "groups", "api_keys"];

    let mut before: Vec<&str> = Vec::new();
    let mut after: Vec<&str> = Vec::new();
    let mut found_first_auth = false;
    let mut in_auth = false;

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
            if AUTH_NAMES.contains(&name.as_str()) {
                pending.clear();
                in_auth = true;
                found_first_auth = true;
                continue;
            } else {
                in_auth = false;
                let target = if found_first_auth {
                    &mut after
                } else {
                    &mut before
                };
                target.extend(pending.drain(..));
                target.push(line);
                continue;
            }
        }

        if is_table_hdr {
            in_auth = false;
            let target = if found_first_auth {
                &mut after
            } else {
                &mut before
            };
            target.extend(pending.drain(..));
            target.push(line);
            continue;
        }

        if in_auth {
            if trimmed.is_empty() || trimmed.starts_with('#') {
                pending.push(line);
            } else {
                pending.clear();
            }
        } else {
            let target = if found_first_auth {
                &mut after
            } else {
                &mut before
            };
            target.extend(pending.drain(..));
            target.push(line);
        }
    }

    let before_str = before.join("\n");
    let after_str = after.join("\n");

    let mut out = before_str.trim_end().to_string();

    if !fresh_auth.trim().is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(fresh_auth.trim_end());
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

pub enum VerifyError {
    LegacyHash,

    InvalidFormat,
}

fn is_legacy_sha256(s: &str) -> bool {
    s.len() == 64 && !s.starts_with('$') && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
