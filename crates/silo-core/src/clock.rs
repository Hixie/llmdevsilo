//! Logical time used to order journal records and events.
//!
//! The harness orders everything by sequence numbers rather than wall-clock
//! timers so that replayed sessions are deterministic. The real clock also
//! records wall-clock milliseconds for human consumption; the fake clock
//! never does, so journals produced under test are byte-for-byte stable.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timestamp {
    /// Monotonic logical counter, unique per clock instance.
    pub logical: u64,
    /// Wall-clock milliseconds since the Unix epoch. Absent under the fake
    /// clock so that test journals are deterministic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
}

pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> Timestamp;
}

pub type SharedClock = Arc<dyn Clock>;

#[derive(Debug, Default)]
pub struct RealClock {
    counter: AtomicU64,
}

impl Clock for RealClock {
    fn now(&self) -> Timestamp {
        let logical = self.counter.fetch_add(1, Ordering::SeqCst);
        let wall_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_millis() as u64);
        Timestamp { logical, wall_ms }
    }
}

/// Deterministic clock for tests. Only advances when told to; `now` reports
/// the current value without side effects, so mock components coordinate via
/// explicit sequence numbers rather than racing timers.
#[derive(Debug, Default)]
pub struct FakeClock {
    counter: AtomicU64,
}

impl FakeClock {
    pub fn advance(&self, by: u64) {
        self.counter.fetch_add(by, Ordering::SeqCst);
    }

    pub fn set(&self, value: u64) {
        self.counter.store(value, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        Timestamp {
            logical: self.counter.load(Ordering::SeqCst),
            wall_ms: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_clock_is_monotonic() {
        let clock = RealClock::default();
        let a = clock.now();
        let b = clock.now();
        assert!(b.logical > a.logical);
        assert!(a.wall_ms.is_some());
    }

    #[test]
    fn fake_clock_is_deterministic() {
        let clock = FakeClock::default();
        assert_eq!(
            clock.now(),
            Timestamp {
                logical: 0,
                wall_ms: None
            }
        );
        clock.advance(5);
        assert_eq!(clock.now().logical, 5);
        clock.set(2);
        assert_eq!(clock.now().logical, 2);
    }
}
