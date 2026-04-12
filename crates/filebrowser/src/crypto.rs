use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn hash_password(pw: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"filebrowser-share:");
    h.update(pw.as_bytes());
    format!("{:x}", h.finalize())
}

pub fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
