//! Time as an injectable thing, so retry/backoff/quiet-hours/dedupe are testable without waiting.

use std::sync::Mutex;
use std::time::Duration;

pub trait Clock: Send + Sync {
    /// Seconds since the Unix epoch.
    fn unix(&self) -> u64;
    /// Local wall-clock minutes since midnight (0..1440).
    fn local_minutes(&self) -> u32;
    fn sleep(&self, d: Duration);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn unix(&self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
    }
    fn local_minutes(&self) -> u32 {
        let t = self.unix() as libc::time_t;
        // SAFETY: localtime_r writes into the zeroed struct we own and reads `t`.
        let tm = unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&t, &mut tm).is_null() {
                return ((self.unix() / 60) % 1440) as u32;
            }
            tm
        };
        (tm.tm_hour as u32) * 60 + tm.tm_min as u32
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d)
    }
}

/// A clock that only moves when told to; `sleep` advances it and is recorded.
pub struct FakeClock {
    inner: Mutex<Fake>,
}

struct Fake {
    now: u64,
    minutes: Option<u32>,
    slept: Vec<Duration>,
}

impl FakeClock {
    pub fn new(now: u64) -> FakeClock {
        FakeClock { inner: Mutex::new(Fake { now, minutes: None, slept: Vec::new() }) }
    }
    /// Pin the local time of day (otherwise it is derived from `now` as UTC).
    pub fn set_local_minutes(&self, m: u32) {
        if let Ok(mut g) = self.inner.lock() {
            g.minutes = Some(m);
        }
    }
    pub fn advance(&self, secs: u64) {
        if let Ok(mut g) = self.inner.lock() {
            g.now += secs;
        }
    }
    pub fn slept(&self) -> Vec<Duration> {
        self.inner.lock().map(|g| g.slept.clone()).unwrap_or_default()
    }
}

impl Clock for FakeClock {
    fn unix(&self) -> u64 {
        self.inner.lock().map_or(0, |g| g.now)
    }
    fn local_minutes(&self) -> u32 {
        self.inner.lock().map_or(0, |g| g.minutes.unwrap_or(((g.now / 60) % 1440) as u32))
    }
    fn sleep(&self, d: Duration) {
        if let Ok(mut g) = self.inner.lock() {
            g.slept.push(d);
            g.now += d.as_secs();
        }
    }
}

/// `YYYY-MM-DD HH:MM:SS` in local time.
pub fn format_local(unix: u64) -> String {
    let t = unix as libc::time_t;
    // SAFETY: localtime_r fills the zeroed struct we own.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return format!("@{unix}");
        }
        tm
    };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}
