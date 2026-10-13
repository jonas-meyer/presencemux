//! The clock of the lease deadlines, `CLOCK_MONOTONIC`.

use std::time::Duration;

use rustix::time::{ClockId, clock_gettime};

/// Returns the time on `CLOCK_MONOTONIC` in nanoseconds. It makes no system
/// call. The real-time thread calls it.
pub(crate) fn now_nanos() -> u64 {
    let now = clock_gettime(ClockId::Monotonic);
    let seconds = u64::try_from(now.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(now.tv_nsec).unwrap_or(0);
    seconds * 1_000_000_000 + nanos
}

/// Returns the time on `CLOCK_MONOTONIC`.
pub(crate) fn now() -> Duration {
    Duration::from_nanos(now_nanos())
}
