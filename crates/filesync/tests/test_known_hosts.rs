#[cfg(test)]
mod tests {
    use bytehive_filesync::{ClientStatus, KnownClients, KnownServers};
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bh_kh_test_{n}_{name}"))
    }

    // ── KnownClients ─────────────────────────────────────────────────────────

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

    // ── KnownServers ─────────────────────────────────────────────────────────

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
