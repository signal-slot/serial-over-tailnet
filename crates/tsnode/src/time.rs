//! Time as the node's state machines see it: a monotonic [`Instant`] the
//! caller passes in, and a [`WallClock`] that maps it to Unix time for the
//! one place that needs calendar time (WireGuard's TAI64N timestamps).
//!
//! Nothing in the state machines reads a clock: whoever drives them (a thread
//! on `std`, a task on embassy) reads its own and hands the value over.

use core::ops::{Add, AddAssign, Sub};
use core::time::Duration;

/// A point on a monotonic clock, in microseconds from an origin the caller
/// chooses (usually boot). Only differences between instants mean anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Instant(u64);

impl Instant {
    pub const fn from_micros(us: u64) -> Self {
        Instant(us)
    }

    pub const fn from_millis(ms: u64) -> Self {
        Instant(ms * 1000)
    }

    pub const fn as_micros(self) -> u64 {
        self.0
    }

    /// Time from `earlier` to `self`, zero if `earlier` is later.
    pub fn duration_since(self, earlier: Instant) -> Duration {
        Duration::from_micros(self.0.saturating_sub(earlier.0))
    }

    /// The instant on the process's monotonic clock (origin: first call).
    #[cfg(feature = "std")]
    pub fn now() -> Instant {
        static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let origin = *ORIGIN.get_or_init(std::time::Instant::now);
        Instant(origin.elapsed().as_micros() as u64)
    }
}

fn micros(d: Duration) -> u64 {
    d.as_micros().min(u64::MAX as u128) as u64
}

impl Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, d: Duration) -> Instant {
        Instant(self.0.saturating_add(micros(d)))
    }
}

impl AddAssign<Duration> for Instant {
    fn add_assign(&mut self, d: Duration) {
        *self = *self + d;
    }
}

impl Sub<Duration> for Instant {
    type Output = Instant;
    fn sub(self, d: Duration) -> Instant {
        Instant(self.0.saturating_sub(micros(d)))
    }
}

impl Sub<Instant> for Instant {
    type Output = Duration;
    fn sub(self, earlier: Instant) -> Duration {
        self.duration_since(earlier)
    }
}

/// Unix time as an offset from the monotonic clock. Until it is set, Unix
/// time reads as the time since the clock's origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WallClock {
    /// Unix time in microseconds at `Instant(0)`.
    offset_us: i64,
}

impl WallClock {
    /// The clock that reads `unix` (time since 1970) at `now`.
    pub fn new(now: Instant, unix: Duration) -> Self {
        WallClock {
            offset_us: micros(unix) as i64 - now.0 as i64,
        }
    }

    /// The process's system clock (`std` only).
    #[cfg(feature = "std")]
    pub fn system() -> Self {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        WallClock::new(Instant::now(), unix)
    }

    /// Unix time at `now`.
    pub fn unix(&self, now: Instant) -> Duration {
        Duration::from_micros((now.0 as i64).saturating_add(self.offset_us).max(0) as u64)
    }

    /// The TAI64N label of `now`, as a WireGuard initiation carries it.
    pub fn tai64n(&self, now: Instant) -> [u8; 12] {
        let t = self.unix(now);
        let tai = t.as_secs().wrapping_add(0x4000_0000_0000_000a);
        let mut out = [0u8; 12];
        out[..8].copy_from_slice(&tai.to_be_bytes());
        out[8..].copy_from_slice(&t.subsec_nanos().to_be_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instants_and_wall_clock() {
        let a = Instant::from_millis(1500);
        let b = a + Duration::from_millis(250);
        assert_eq!(b - a, Duration::from_millis(250));
        assert_eq!(a - b, Duration::ZERO);
        let w = WallClock::new(b, Duration::from_secs(1_800_000_000));
        assert_eq!(w.unix(b), Duration::from_secs(1_800_000_000));
        assert_eq!(
            w.unix(b + Duration::from_secs(3)),
            Duration::from_secs(1_800_000_003)
        );
        // Later instants give later labels, byte-wise.
        assert!(w.tai64n(b + Duration::from_nanos(1000)) > w.tai64n(b));
        assert!(w.tai64n(b + Duration::from_secs(1)) > w.tai64n(b + Duration::from_millis(999)));
    }
}
