use bytehive_filesync::gui::manager::SyncManager;
use bytehive_filesync::gui::state::{new_shared_state, ConnectionStatus};

// ─── Initial state ────────────────────────────────────────────────────────────

#[test]
fn sync_manager_starts_not_paused() {
    let state = new_shared_state();
    let manager = SyncManager::new(state);
    assert!(!manager.is_paused());
}

#[test]
fn sync_manager_initial_state_is_disconnected() {
    let state = new_shared_state();
    let _manager = SyncManager::new(state.clone());
    assert_eq!(state.read().status, ConnectionStatus::Disconnected);
}

#[test]
fn sync_manager_initial_log_is_empty() {
    let state = new_shared_state();
    let _manager = SyncManager::new(state.clone());
    assert!(state.read().log.entries().is_empty());
}

// ─── pause ────────────────────────────────────────────────────────────────────

#[test]
fn pause_sets_is_paused_to_true() {
    let state = new_shared_state();
    let manager = SyncManager::new(state);
    manager.pause();
    assert!(manager.is_paused());
}

#[test]
fn pause_sets_connection_status_to_paused() {
    let state = new_shared_state();
    let manager = SyncManager::new(state.clone());
    manager.pause();
    assert_eq!(state.read().status, ConnectionStatus::Paused);
}

#[test]
fn pause_writes_a_log_entry() {
    let state = new_shared_state();
    let manager = SyncManager::new(state.clone());
    manager.pause();
    let entries = state.read().log.entries().to_vec();
    assert!(
        !entries.is_empty(),
        "pause should write at least one log entry"
    );
}

#[test]
fn pause_log_entry_mentions_pause() {
    let state = new_shared_state();
    let manager = SyncManager::new(state.clone());
    manager.pause();
    let entries = state.read().log.entries().to_vec();
    let has_pause_message = entries
        .iter()
        .any(|e| e.to_lowercase().contains("pause") || e.to_lowercase().contains("paused"));
    assert!(
        has_pause_message,
        "pause log should mention pausing; got: {:?}",
        entries
    );
}

#[test]
fn pause_called_twice_stays_paused() {
    let state = new_shared_state();
    let manager = SyncManager::new(state);
    manager.pause();
    manager.pause();
    assert!(manager.is_paused());
}

// ─── resume ───────────────────────────────────────────────────────────────────

#[test]
fn resume_after_pause_clears_is_paused() {
    let state = new_shared_state();
    let manager = SyncManager::new(state);
    manager.pause();
    manager.resume();
    assert!(!manager.is_paused());
}

#[test]
fn resume_writes_a_log_entry() {
    let state = new_shared_state();
    let manager = SyncManager::new(state.clone());
    manager.pause();
    let entries_before = state.read().log.entries().len();
    manager.resume();
    let entries_after = state.read().log.entries().len();
    assert!(
        entries_after > entries_before,
        "resume should add at least one log entry"
    );
}

#[test]
fn resume_log_entry_mentions_resume() {
    let state = new_shared_state();
    let manager = SyncManager::new(state.clone());
    manager.pause();
    manager.resume();
    let entries = state.read().log.entries().to_vec();
    let has_resume_message = entries
        .iter()
        .any(|e| e.to_lowercase().contains("resume") || e.to_lowercase().contains("resumed"));
    assert!(
        has_resume_message,
        "resume log should mention resuming; got: {:?}",
        entries
    );
}

#[test]
fn resume_without_prior_pause_does_not_panic() {
    let state = new_shared_state();
    let manager = SyncManager::new(state);
    // Calling resume without a prior pause should be a no-op, not a crash.
    manager.resume();
    assert!(!manager.is_paused());
}

#[test]
fn resume_called_twice_stays_not_paused() {
    let state = new_shared_state();
    let manager = SyncManager::new(state);
    manager.pause();
    manager.resume();
    manager.resume();
    assert!(!manager.is_paused());
}

// ─── Alternating pause / resume ───────────────────────────────────────────────

#[test]
fn alternating_pause_resume_three_cycles() {
    let state = new_shared_state();
    let manager = SyncManager::new(state.clone());
    for _ in 0..3 {
        manager.pause();
        assert!(manager.is_paused());
        assert_eq!(state.read().status, ConnectionStatus::Paused);
        manager.resume();
        assert!(!manager.is_paused());
    }
}

#[test]
fn alternating_pause_resume_accumulates_log_entries() {
    let state = new_shared_state();
    let manager = SyncManager::new(state.clone());
    for _ in 0..3 {
        manager.pause();
        manager.resume();
    }
    // 3 pauses + 3 resumes = at least 6 entries
    let entry_count = state.read().log.entries().len();
    assert!(
        entry_count >= 6,
        "expected at least 6 log entries after 3 pause/resume cycles, got {}",
        entry_count
    );
}

// ─── stop ─────────────────────────────────────────────────────────────────────

#[test]
fn stop_does_not_panic() {
    let state = new_shared_state();
    let manager = SyncManager::new(state);
    manager.stop(); // only sets a flag; no thread is running here
}

#[test]
fn stop_while_paused_does_not_panic() {
    let state = new_shared_state();
    let manager = SyncManager::new(state);
    manager.pause();
    manager.stop();
}

#[test]
fn stop_can_be_called_multiple_times() {
    let state = new_shared_state();
    let manager = SyncManager::new(state);
    manager.stop();
    manager.stop();
}

// ─── Shared-state independence ────────────────────────────────────────────────

#[test]
fn two_managers_share_state_correctly() {
    // Two managers wrapping the same Arc<RwLock<_>> should see each other's writes.
    let state = new_shared_state();
    let manager1 = SyncManager::new(state.clone());
    let manager2 = SyncManager::new(state.clone());

    manager1.pause();
    // manager2 observes the status written by manager1
    assert_eq!(state.read().status, ConnectionStatus::Paused);
    assert!(manager1.is_paused());

    manager2.resume();
    assert!(!manager2.is_paused());
}

#[test]
fn manager_pause_does_not_affect_separate_state() {
    let state1 = new_shared_state();
    let state2 = new_shared_state();
    let manager1 = SyncManager::new(state1.clone());
    let _manager2 = SyncManager::new(state2.clone());

    manager1.pause();

    assert_eq!(state1.read().status, ConnectionStatus::Paused);
    // state2 is unrelated and must remain Disconnected
    assert_eq!(state2.read().status, ConnectionStatus::Disconnected);
}
