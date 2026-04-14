pub mod app;
pub mod bundler;
pub mod client;
pub mod common;
pub mod exclusions;
pub mod gui;
pub mod known_hosts;
pub mod manifest;
pub mod protocol;
pub mod server;
pub mod sync_engine;
pub mod transport;
pub mod watcher;

pub use app::FileSyncApp;
pub use known_hosts::{ClientStatus, KnownClient, KnownClients, KnownServer, KnownServers};

pub fn timestamp_id() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let now = d
        .as_secs()
        .wrapping_mul(1_000_000_000)
        .wrapping_add(d.subsec_nanos() as u64);

    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let next = if now > last {
            now
        } else {
            last.wrapping_add(1)
        };
        match LAST.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(updated) => last = updated,
        }
    }
}

pub fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Compute the BLAKE3 fingerprint of a DER-encoded certificate.
///
/// The result is a 64-character lowercase hex string that uniquely identifies
/// the certificate.  This is used for both client and server identity checks
/// in the known-hosts system.
pub fn cert_fingerprint(cert_der: &[u8]) -> String {
    blake3::hash(cert_der).to_hex().to_string()
}
