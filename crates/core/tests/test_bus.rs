use bytehive_core::bus::MessageBus;
use serde_json::json;

#[test]
fn publish_reaches_exact_subscriber() {
    let bus = MessageBus::new();
    let rx = bus.sub("auth.login");
    bus.publish("core", "auth.login", json!({"user": "alice"}));
    let msg = rx
        .recv_timeout(std::time::Duration::from_millis(200))
        .unwrap();
    assert_eq!(msg.source, "core");
    assert_eq!(msg.topic, "auth.login");
    assert_eq!(msg.payload["user"], "alice");
}

#[test]
fn publish_reaches_wildcard_subscriber() {
    let bus = MessageBus::new();
    let rx = bus.sub("filesync.*");
    bus.publish("fs", "filesync.sync_complete", json!(null));
    let msg = rx
        .recv_timeout(std::time::Duration::from_millis(200))
        .unwrap();
    assert_eq!(msg.topic, "filesync.sync_complete");
}

#[test]
fn publish_reaches_global_wildcard() {
    let bus = MessageBus::new();
    let rx = bus.sub("*");
    bus.publish("x", "any.topic", json!(1));
    let msg = rx
        .recv_timeout(std::time::Duration::from_millis(200))
        .unwrap();
    assert_eq!(msg.topic, "any.topic");
}

#[test]
fn message_ids_are_monotonically_increasing() {
    let bus = MessageBus::new();
    let rx = bus.sub("*");
    bus.publish("s", "t1", json!(null));
    bus.publish("s", "t2", json!(null));
    let m1 = rx
        .recv_timeout(std::time::Duration::from_millis(200))
        .unwrap();
    let m2 = rx
        .recv_timeout(std::time::Duration::from_millis(200))
        .unwrap();
    assert!(m2.id > m1.id);
}

#[test]
fn patterns_returns_registered_keys() {
    let bus = MessageBus::new();
    let _r1 = bus.sub("topic.a");
    let _r2 = bus.sub("topic.b");
    let pats = bus.patterns();
    assert!(pats.contains(&"topic.a".to_string()));
    assert!(pats.contains(&"topic.b".to_string()));
}

#[test]
fn full_queue_does_not_panic() {
    let bus = MessageBus::new();
    let _rx = bus.subscribe("t", 1);
    bus.publish("s", "t", json!(1));
    bus.publish("s", "t", json!(2));
}

#[test]
fn gc_removes_entries_when_all_receivers_dropped() {
    let bus = MessageBus::new();
    {
        let _rx = bus.sub("ephemeral.topic");
        // _rx is dropped here, all senders for this pattern are gone
    }
    // After gc the pattern should be removed
    bus.gc();
    let pats = bus.patterns();
    assert!(!pats.contains(&"ephemeral.topic".to_string()));
}

#[test]
fn publish_to_disconnected_subscriber_does_not_panic() {
    let bus = MessageBus::new();
    let rx = bus.sub("dropped.topic");
    drop(rx);
    bus.publish("src", "dropped.topic", json!({"x": 1}));
}

#[test]
fn wildcard_suffix_pattern_matches_exact_prefix() {
    let bus = MessageBus::new();
    let rx = bus.sub("auth.*");
    bus.publish("core", "auth", json!(null));
    let msg = rx
        .recv_timeout(std::time::Duration::from_millis(200))
        .unwrap();
    assert_eq!(msg.topic, "auth");
}
