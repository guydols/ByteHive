use bytehive_filesync::suspend_detector::SuspendDetector;
use std::time::Duration;

#[test]
fn detector_initializes() {
    let _detector = SuspendDetector::new();
}

#[test]
fn detector_default_trait() {
    let _detector = SuspendDetector::default();
}

#[test]
fn check_for_resume_does_not_panic() {
    let mut detector = SuspendDetector::new();
    std::thread::sleep(Duration::from_millis(10));
    let _result = detector.check_for_resume();
}

#[test]
fn check_for_resume_normal_operation() {
    let mut detector = SuspendDetector::new();
    std::thread::sleep(Duration::from_millis(50));
    let resumed = detector.check_for_resume();
    assert!(!resumed);
}
