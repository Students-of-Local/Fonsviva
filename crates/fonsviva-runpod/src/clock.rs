//! Time for the supervisor. Wall time stamps events and buckets spend by
//! day; monotonic time measures idle and billing intervals. When the two
//! disagree by more than a minute between ticks, the wall clock jumped (or
//! the machine slept), and the supervisor stops pods it could not watch.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    /// Time since an arbitrary start; never goes backwards.
    fn monotonic(&self) -> Duration;
}

#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    start: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn monotonic(&self) -> Duration {
        self.start.elapsed()
    }
}

/// A clock tests move by hand.
#[derive(Debug)]
pub struct FakeClock {
    inner: Mutex<(DateTime<Utc>, Duration)>,
}

impl FakeClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            inner: Mutex::new((start, Duration::ZERO)),
        }
    }

    /// Moves wall and monotonic time forward together.
    pub fn advance(&self, by: Duration) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.0 += chrono::Duration::from_std(by).unwrap_or_default();
        inner.1 += by;
    }

    /// Moves only the wall clock, as a user changing the system time would.
    pub fn jump_wall(&self, by: chrono::Duration) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.0 += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0
    }

    fn monotonic(&self) -> Duration {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .1
    }
}
