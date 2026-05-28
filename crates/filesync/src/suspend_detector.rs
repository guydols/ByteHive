use std::fs;
use std::time::{Duration, Instant};

/// Detects system suspend/resume by comparing wall-clock time against system uptime.
/// When wall-clock advances significantly more than uptime, the system was suspended.
pub struct SuspendDetector {
    last_check: Instant,
    last_uptime: Duration,
}

impl SuspendDetector {
    pub fn new() -> Self {
        let uptime = read_system_uptime().unwrap_or(Duration::ZERO);
        Self {
            last_check: Instant::now(),
            last_uptime: uptime,
        }
    }

    pub fn check_for_resume(&mut self) -> bool {
        let now = Instant::now();
        let wall_elapsed = now.duration_since(self.last_check);

        let current_uptime = match read_system_uptime() {
            Ok(uptime) => uptime,
            Err(_) => {
                self.last_check = now;
                return false;
            }
        };

        let uptime_elapsed = current_uptime.saturating_sub(self.last_uptime);

        const SUSPEND_THRESHOLD_SECS: u64 = 5;
        let suspended = wall_elapsed.as_secs() > uptime_elapsed.as_secs() + SUSPEND_THRESHOLD_SECS;

        self.last_check = now;
        self.last_uptime = current_uptime;

        suspended
    }
}

impl Default for SuspendDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
fn read_system_uptime() -> std::io::Result<Duration> {
    let contents = fs::read_to_string("/proc/uptime")?;
    let uptime_str = contents
        .split_whitespace()
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "empty uptime"))?;

    let uptime_secs: f64 = uptime_str
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid uptime"))?;

    Ok(Duration::from_secs_f64(uptime_secs))
}

#[cfg(not(target_os = "linux"))]
fn read_system_uptime() -> std::io::Result<Duration> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "system uptime reading not supported on this platform",
    ))
}
