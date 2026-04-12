use crate::crypto::{constant_eq, hash_password, now_ms};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Share {
    pub token: String,
    pub path: String,
    pub is_dir: bool,
    pub name: String,
    pub password_protected: bool,
    #[serde(skip)]
    pub password_hash: Option<String>,
    pub expires_ms: Option<u64>,
    pub created_by: String,
    pub created_ms: u64,
    pub download_count: u64,
}

impl Share {
    pub fn is_expired_at(&self, now: u64) -> bool {
        self.expires_ms.map(|e| e < now).unwrap_or(false)
    }

    pub fn is_expired(&self) -> bool {
        self.is_expired_at(now_ms())
    }

    pub fn check_password(&self, provided: &str) -> bool {
        match &self.password_hash {
            None => true,
            Some(hash) => {
                let provided_hash = hash_password(provided);
                constant_eq(hash, &provided_hash)
            }
        }
    }
}
