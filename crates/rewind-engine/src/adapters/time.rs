use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rewind_core::Timestamp;

use crate::ports::{Clock, VirtualTime};

/// Real time.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        Timestamp(ms)
    }
}

/// Virtual time: it only moves when told to.
#[derive(Debug, Default)]
pub struct VirtualClock {
    now: AtomicU64,
}

impl VirtualClock {
    pub fn new(start: Timestamp) -> Self {
        Self {
            now: AtomicU64::new(start.as_millis()),
        }
    }
}

impl Clock for VirtualClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.now.load(Ordering::SeqCst))
    }
}

impl VirtualTime for VirtualClock {
    fn advance_to(&self, at: Timestamp) {
        self.now.fetch_max(at.as_millis(), Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_time_never_goes_backwards() {
        let clock = VirtualClock::new(Timestamp(1_000));
        clock.advance_to(Timestamp(5_000));
        clock.advance_to(Timestamp(2_000));
        assert_eq!(clock.now(), Timestamp(5_000));
    }

    #[test]
    fn system_time_is_after_2026() {
        assert!(SystemClock.now() > Timestamp(1_767_225_600_000));
    }
}
