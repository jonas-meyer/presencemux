//! The lease and the latch of the mic gate.
//!
//! Times are `CLOCK_MONOTONIC`. The gate has no I/O and no clock of its own.

use std::fmt;
use std::time::Duration;

use presencemux_lease::LeaseHolder;
pub(crate) use presencemux_lease::{Grant, Rejection};

/// Whether a lease lets the gate pass audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Permission {
    Denied,
    Permitted,
}

/// A permission from presencemuxd that expires at a fixed time.
pub(crate) type Lease = presencemux_lease::Lease<Permission>;

/// A block of the mic until a lease with a newer grant arrives.
pub(crate) type Latch = presencemux_lease::Latch<Reason>;

/// The reason for a latch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Reason {
    /// The lease expired before the next lease arrived.
    Expired,
    /// The worker started. The first lease latches the gate.
    Started,
}

impl presencemux_lease::LatchReason for Reason {
    const EXPIRED: Self = Self::Expired;
    const STARTED: Self = Self::Started;
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Expired => "expired",
            Self::Started => "started",
        })
    }
}

/// What the gate does with the audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// The gate passes audio until this time.
    Open { until: Duration },
    /// A valid lease denies the mic. The gate closes with a ramp.
    Denied,
    /// The gate has no valid lease, or it is latched. The gate closes at once.
    Blocked,
}

/// The lease and the latch of the gate.
#[derive(Clone, Debug, Default)]
pub(crate) struct Gate {
    holder: LeaseHolder<Permission, Reason>,
}

impl Gate {
    /// Returns the latch, if the gate is latched.
    pub(crate) fn latch(&self) -> Option<Latch> {
        self.holder.latch()
    }

    /// Returns the last accepted lease, if it does not expire at or before
    /// `now`.
    pub(crate) fn lease(&self, now: Duration) -> Option<&Lease> {
        self.holder.lease(now)
    }

    /// Checks `lease` at the time `now`, and accepts it or returns the
    /// rejection. If the last lease expired at or before `now`, latches the
    /// gate first.
    pub(crate) fn renew(&mut self, lease: Lease, now: Duration) -> Result<(), Rejection> {
        self.holder.renew(lease, now)
    }

    /// Latches the gate if the last lease expires at or before `now`. Returns
    /// the new latch, if the gate latched now.
    pub(crate) fn check(&mut self, now: Duration) -> Option<Latch> {
        self.holder.expire(now)
    }

    /// Returns the state of the gate at `now`.
    pub(crate) fn state(&self, now: Duration) -> State {
        match (self.holder.lease(now), self.holder.latch()) {
            (Some(lease), None) if lease.permission == Permission::Permitted => {
                State::Open { until: lease.until }
            }
            (Some(_), None) => State::Denied,
            (None, _) | (Some(_), Some(_)) => State::Blocked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: Duration = Duration::from_secs(1_000);
    const NOW: Duration = Duration::from_secs(2_000);
    /// The length of the leases in the tests.
    const LEASE: Duration = Duration::from_secs(1);

    fn lease(epoch: u32, permission: Permission) -> Lease {
        Lease {
            grant: Grant {
                session: SESSION,
                epoch,
            },
            permission,
            until: NOW + LEASE,
        }
    }

    /// Returns a gate with the lease of epoch 1 and `permission` at `NOW`,
    /// after the denied boot lease of epoch 0. The gate is not latched.
    fn gate(permission: Permission) -> Gate {
        let mut gate = Gate::default();
        gate.renew(lease(0, Permission::Denied), NOW).unwrap();
        gate.renew(lease(1, permission), NOW).unwrap();
        gate
    }

    #[test]
    fn permitted_lease_opens_the_gate_until_its_expiry() {
        let gate = gate(Permission::Permitted);

        assert_eq!(gate.state(NOW), State::Open { until: NOW + LEASE });
    }

    #[test]
    fn denied_lease_gives_the_denied_state() {
        let gate = gate(Permission::Denied);

        assert_eq!(gate.state(NOW), State::Denied);
    }

    #[test]
    fn latched_gate_with_a_denied_lease_is_blocked() {
        let mut gate = Gate::default();

        gate.renew(lease(3, Permission::Denied), NOW).unwrap();

        assert_eq!(gate.state(NOW), State::Blocked);
    }

    #[test]
    fn check_before_the_expiry_time_does_not_latch() {
        let mut gate = gate(Permission::Permitted);

        let latch = gate.check(NOW + Duration::from_nanos(999_999_999));

        assert_eq!(latch, None);
    }

    #[test]
    fn gate_without_a_lease_is_blocked() {
        let gate = Gate::default();

        assert_eq!(gate.state(NOW), State::Blocked);
    }

    #[test]
    fn first_permitted_lease_is_blocked_by_the_started_latch() {
        let mut gate = Gate::default();

        gate.renew(lease(3, Permission::Permitted), NOW).unwrap();

        assert_eq!(gate.state(NOW), State::Blocked);
    }

    #[test]
    fn gate_at_the_expiry_time_is_blocked_without_a_check() {
        let gate = gate(Permission::Permitted);

        assert_eq!(gate.state(NOW + LEASE), State::Blocked);
    }

    #[test]
    fn check_at_the_expiry_time_latches_the_gate_as_expired() {
        let mut gate = gate(Permission::Permitted);

        let latch = gate.check(NOW + LEASE);

        assert_eq!(latch.map(|latch| latch.reason), Some(Reason::Expired));
    }

    #[test]
    fn permitted_lease_with_the_same_epoch_after_an_expiry_is_blocked() {
        let mut gate = gate(Permission::Permitted);
        let later = NOW + LEASE;
        let renewal = Lease {
            until: later + LEASE,
            ..lease(1, Permission::Permitted)
        };

        gate.renew(renewal, later).unwrap();

        assert_eq!(gate.state(later), State::Blocked);
    }

    #[test]
    fn permitted_lease_with_a_newer_epoch_after_an_expiry_opens_the_gate() {
        let mut gate = gate(Permission::Permitted);
        let later = NOW + LEASE;
        let renewal = Lease {
            until: later + LEASE,
            ..lease(2, Permission::Permitted)
        };

        gate.renew(renewal, later).unwrap();

        assert_eq!(
            gate.state(later),
            State::Open {
                until: later + LEASE
            }
        );
    }
}
