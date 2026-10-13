//! The lease and the latch of a media worker.
//!
//! presencemuxd sends each privacy permission to its worker as a lease that
//! expires at a fixed time. The worker keeps a [`LeaseHolder`]. The holder
//! accepts or rejects each lease, and it latches when a lease expires. A latch
//! blocks the permission until a lease with a newer grant arrives.
//!
//! Times are `CLOCK_MONOTONIC`. The crate has no I/O and no clock of its own.

use std::fmt;
use std::time::Duration;

/// The longest time from now to the expiry time of a lease.
pub const MAX_LEASE: Duration = Duration::from_secs(2);

/// The session and the epoch of a lease. The order of grants is by session,
/// then by epoch. The order of the fields gives this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Grant {
    /// The start time of the daemon run that sent the lease.
    pub session: Duration,
    /// The number of the user command that gave the permission.
    pub epoch: u32,
}

/// A permission `P` from presencemuxd that expires at a fixed time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease<P> {
    pub grant: Grant,
    pub permission: P,
    /// The time at which the lease expires.
    pub until: Duration,
}

/// The reasons for a latch that the holder sets itself. A worker can have
/// more reasons.
pub trait LatchReason: Copy {
    /// The lease expired before the next lease arrived.
    const EXPIRED: Self;
    /// The worker started. The first lease latches the worker.
    const STARTED: Self;
}

/// A block of the permission until a lease with a newer grant arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Latch<R> {
    /// The grant of the lease at the time of the latch.
    pub grant: Grant,
    pub reason: R,
}

/// The reason for the rejection of a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rejection {
    /// The lease expires at or before now.
    Late,
    /// The lease expires more than [`MAX_LEASE`] after now.
    TooLong,
    /// The lease comes from an older daemon run than the last lease.
    OldSession,
    /// The lease has an older epoch than the last lease of its session.
    OldEpoch,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Late => "late",
            Self::TooLong => "too long",
            Self::OldSession => "old session",
            Self::OldEpoch => "old epoch",
        })
    }
}

/// The last lease and the latch of a worker.
#[derive(Clone, Debug)]
pub struct LeaseHolder<P, R> {
    /// The last accepted lease. It stays after it expires. `renew` compares
    /// each new lease with it.
    last: Option<Lease<P>>,
    /// Whether the last lease expired.
    expired: bool,
    latch: Option<Latch<R>>,
}

impl<P, R> Default for LeaseHolder<P, R> {
    fn default() -> Self {
        Self {
            last: None,
            expired: false,
            latch: None,
        }
    }
}

impl<P, R: LatchReason> LeaseHolder<P, R> {
    /// Returns the latch, if the worker is latched.
    pub fn latch(&self) -> Option<Latch<R>> {
        self.latch
    }

    /// Returns the last accepted lease, also after its expiry.
    pub fn last(&self) -> Option<&Lease<P>> {
        self.last.as_ref()
    }

    /// Returns the last accepted lease, if it does not expire at or before
    /// `now`.
    pub fn lease(&self, now: Duration) -> Option<&Lease<P>> {
        self.last
            .as_ref()
            .filter(|lease| !self.expired && lease.until > now)
    }

    /// Checks `lease` at the time `now`, and accepts it or returns the
    /// rejection. If the last lease expired at or before `now`, latches the
    /// worker first. The first accepted lease latches the worker as started,
    /// and a lease with a newer grant than the latch clears the latch.
    pub fn renew(&mut self, lease: Lease<P>, now: Duration) -> Result<(), Rejection> {
        self.expire(now);
        if lease.until <= now {
            return Err(Rejection::Late);
        }
        if now
            .checked_add(MAX_LEASE)
            .is_some_and(|max| lease.until > max)
        {
            return Err(Rejection::TooLong);
        }
        match &self.last {
            Some(last) if lease.grant.session < last.grant.session => {
                return Err(Rejection::OldSession);
            }
            Some(last) if lease.grant < last.grant => return Err(Rejection::OldEpoch),
            Some(_) => {}
            None => {
                self.latch = Some(Latch {
                    grant: lease.grant,
                    reason: R::STARTED,
                });
            }
        }
        if self.latch.is_some_and(|latch| lease.grant > latch.grant) {
            self.latch = None;
        }
        self.last = Some(lease);
        self.expired = false;
        Ok(())
    }

    /// Latches the worker if the last lease expires at or before `now`. The
    /// expiry replaces the reason of an earlier latch. Returns the new latch,
    /// if the lease expired now.
    pub fn expire(&mut self, now: Duration) -> Option<Latch<R>> {
        let last = self.last.as_ref()?;
        if self.expired || last.until > now {
            return None;
        }
        self.expired = true;
        self.latch = Some(Latch {
            grant: last.grant,
            reason: R::EXPIRED,
        });
        self.latch
    }

    /// Latches the worker with `reason` in the grant of the last lease.
    /// Returns the latch, or `None` without a lease.
    pub fn latch_now(&mut self, reason: R) -> Option<Latch<R>> {
        let grant = self.last.as_ref()?.grant;
        self.latch = Some(Latch { grant, reason });
        self.latch
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const SESSION: Duration = Duration::from_secs(1_000);
    const NOW: Duration = Duration::from_secs(2_000);
    /// The length of the leases in the tests.
    const LEASE: Duration = Duration::from_secs(1);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Reason {
        Expired,
        Started,
        Other,
    }

    impl LatchReason for Reason {
        const EXPIRED: Self = Self::Expired;
        const STARTED: Self = Self::Started;
    }

    type Holder = LeaseHolder<bool, Reason>;

    fn grant(epoch: u32) -> Grant {
        Grant {
            session: SESSION,
            epoch,
        }
    }

    fn lease(epoch: u32) -> Lease<bool> {
        Lease {
            grant: grant(epoch),
            permission: true,
            until: NOW + LEASE,
        }
    }

    /// Returns a holder with the lease of epoch 1 at `NOW`, after the boot
    /// lease of epoch 0. The holder is not latched.
    fn unlatched() -> Holder {
        let mut holder = Holder::default();
        holder.renew(lease(0), NOW).unwrap();
        holder.renew(lease(1), NOW).unwrap();
        holder
    }

    /// Returns `unlatched` after the expiry of its lease at `NOW + LEASE`.
    fn expired() -> Holder {
        let mut holder = unlatched();
        holder.expire(NOW + LEASE);
        holder
    }

    #[test]
    fn first_lease_latches_as_started() {
        let mut holder = Holder::default();

        holder.renew(lease(3), NOW).unwrap();

        assert_eq!(
            holder.latch(),
            Some(Latch {
                grant: grant(3),
                reason: Reason::Started,
            })
        );
    }

    #[test]
    fn lease_with_a_newer_epoch_clears_the_latch() {
        let mut holder = Holder::default();
        holder.renew(lease(3), NOW).unwrap();

        holder.renew(lease(4), NOW).unwrap();

        assert_eq!(holder.latch(), None);
    }

    #[test]
    fn lease_with_a_newer_session_and_a_lower_epoch_clears_the_latch() {
        let mut holder = Holder::default();
        holder.renew(lease(3), NOW).unwrap();
        let newer = Lease {
            grant: Grant {
                session: SESSION + Duration::from_secs(1),
                epoch: 0,
            },
            ..lease(0)
        };

        holder.renew(newer, NOW).unwrap();

        assert_eq!(holder.latch(), None);
    }

    #[test]
    fn lease_with_the_same_grant_keeps_the_latch() {
        let mut holder = Holder::default();
        holder.renew(lease(3), NOW).unwrap();

        holder.renew(lease(3), NOW).unwrap();

        assert_eq!(
            holder.latch().map(|latch| latch.reason),
            Some(Reason::Started)
        );
    }

    #[test]
    fn expiry_latches_as_expired() {
        let mut holder = unlatched();

        let latch = holder.expire(NOW + LEASE);

        assert_eq!(
            latch,
            Some(Latch {
                grant: grant(1),
                reason: Reason::Expired,
            })
        );
    }

    #[test]
    fn expire_before_the_expiry_time_does_not_latch() {
        let mut holder = unlatched();

        let latch = holder.expire(NOW + Duration::from_nanos(999_999_999));

        assert_eq!(latch, None);
        assert_eq!(holder.latch(), None);
    }

    #[test]
    fn expiry_gives_the_latch_once() {
        let mut holder = expired();

        let latch = holder.expire(NOW + LEASE * 2);

        assert_eq!(latch, None);
    }

    #[test]
    fn expired_lease_stays_expired_at_an_earlier_time() {
        let holder = expired();

        assert_eq!(holder.lease(NOW), None);
    }

    #[test]
    fn expired_lease_is_the_last_lease() {
        let holder = expired();

        assert_eq!(holder.last(), Some(&lease(1)));
    }

    #[test]
    fn lease_at_its_expiry_time_is_not_the_lease() {
        let holder = unlatched();

        assert_eq!(holder.lease(NOW + LEASE), None);
    }

    #[test]
    fn renewal_with_the_same_grant_after_the_expiry_keeps_the_latch() {
        let mut holder = expired();
        let later = NOW + LEASE;
        let renewal = Lease {
            until: later + LEASE,
            ..lease(1)
        };

        holder.renew(renewal, later).unwrap();

        assert_eq!(
            holder.latch().map(|latch| latch.reason),
            Some(Reason::Expired)
        );
    }

    #[test]
    fn rejected_lease_after_the_expiry_latches_as_expired() {
        let mut holder = unlatched();
        let later = NOW + LEASE;

        let _ = holder.renew(lease(0), later);

        assert_eq!(
            holder.latch().map(|latch| latch.reason),
            Some(Reason::Expired)
        );
    }

    #[test]
    fn expiry_replaces_the_reason_of_an_earlier_latch() {
        let mut holder = unlatched();
        holder.latch_now(Reason::Other);

        holder.expire(NOW + LEASE);

        assert_eq!(
            holder.latch().map(|latch| latch.reason),
            Some(Reason::Expired)
        );
    }

    #[test]
    fn own_latch_takes_the_grant_of_the_last_lease() {
        let mut holder = unlatched();

        let latch = holder.latch_now(Reason::Other);

        assert_eq!(
            latch,
            Some(Latch {
                grant: grant(1),
                reason: Reason::Other,
            })
        );
    }

    #[test]
    fn own_latch_without_a_lease_does_not_latch() {
        let mut holder = Holder::default();

        let latch = holder.latch_now(Reason::Other);

        assert_eq!(latch, None);
    }

    #[test]
    fn own_latch_after_the_expiry_replaces_the_expired_reason() {
        let mut holder = expired();

        holder.latch_now(Reason::Other);

        assert_eq!(
            holder.latch().map(|latch| latch.reason),
            Some(Reason::Other)
        );
    }

    #[test]
    fn lease_that_expires_now_is_late() {
        let mut holder = unlatched();
        let late = Lease {
            until: NOW,
            ..lease(2)
        };

        let result = holder.renew(late, NOW);

        assert_eq!(result, Err(Rejection::Late));
    }

    #[test]
    fn lease_that_expires_2_s_after_now_is_accepted() {
        let mut holder = unlatched();
        let longest = Lease {
            until: NOW + Duration::from_secs(2),
            ..lease(2)
        };

        let result = holder.renew(longest, NOW);

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn lease_that_expires_later_than_2_s_after_now_is_too_long() {
        let mut holder = unlatched();
        let long = Lease {
            until: NOW + Duration::from_secs(2) + Duration::from_nanos(1),
            ..lease(2)
        };

        let result = holder.renew(long, NOW);

        assert_eq!(result, Err(Rejection::TooLong));
    }

    #[test]
    fn lease_from_an_older_session_is_rejected() {
        let mut holder = unlatched();
        let old = Lease {
            grant: Grant {
                session: Duration::from_secs(999),
                epoch: 9,
            },
            ..lease(0)
        };

        let result = holder.renew(old, NOW);

        assert_eq!(result, Err(Rejection::OldSession));
    }

    #[test]
    fn lease_with_an_older_epoch_is_rejected() {
        let mut holder = unlatched();

        let result = holder.renew(lease(0), NOW);

        assert_eq!(result, Err(Rejection::OldEpoch));
    }

    #[test]
    fn rejected_lease_keeps_the_last_lease() {
        let mut holder = unlatched();
        let old = Lease {
            permission: false,
            ..lease(0)
        };

        let _ = holder.renew(old, NOW);

        assert_eq!(holder.lease(NOW), Some(&lease(1)));
    }

    /// One event of the property test, after `advance` milliseconds.
    #[derive(Clone, Debug)]
    enum Event {
        /// A lease arrives.
        Renew {
            advance: u64,
            /// Whether the lease comes from a newer daemon run.
            newer_session: bool,
            epoch: u32,
            /// The length of the lease in milliseconds.
            length: u64,
        },
        /// The timer of the worker runs.
        Expire { advance: u64 },
        /// The worker latches for its own reason.
        Latch { advance: u64 },
    }

    fn event() -> impl Strategy<Value = Event> {
        prop_oneof![
            4 => (0..1_500_u64, prop::bool::weighted(0.1), 0..4_u32, 0..2_500_u64).prop_map(
                |(advance, newer_session, epoch, length)| Event::Renew {
                    advance,
                    newer_session,
                    epoch,
                    length,
                }
            ),
            2 => (0..1_500_u64).prop_map(|advance| Event::Expire { advance }),
            1 => (0..1_500_u64).prop_map(|advance| Event::Latch { advance }),
        ]
    }

    /// Returns the rejection of `lease` at `now` after the accepted lease
    /// `last`.
    fn expected_rejection(
        lease: &Lease<bool>,
        last: Option<&Lease<bool>>,
        now: Duration,
    ) -> Option<Rejection> {
        if lease.until <= now {
            Some(Rejection::Late)
        } else if lease.until > now + Duration::from_secs(2) {
            Some(Rejection::TooLong)
        } else if last.is_some_and(|last| lease.grant.session < last.grant.session) {
            Some(Rejection::OldSession)
        } else if last.is_some_and(|last| lease.grant < last.grant) {
            Some(Rejection::OldEpoch)
        } else {
            None
        }
    }

    /// Returns the leases of `events` with their arrival times, from `NOW` on.
    /// Other events only move the time.
    fn leases(events: &[Event]) -> Vec<(Lease<bool>, Duration)> {
        let mut now = NOW;
        let mut session = SESSION;
        let mut leases = Vec::new();
        for event in events {
            match *event {
                Event::Renew {
                    advance,
                    newer_session,
                    epoch,
                    length,
                } => {
                    now += Duration::from_millis(advance);
                    if newer_session {
                        session += Duration::from_secs(1);
                    }
                    let lease = Lease {
                        grant: Grant { session, epoch },
                        permission: true,
                        until: now + Duration::from_millis(length),
                    };
                    leases.push((lease, now));
                }
                Event::Expire { advance } | Event::Latch { advance } => {
                    now += Duration::from_millis(advance);
                }
            }
        }
        leases
    }

    proptest! {
        /// Each lease gets the rejection of the lease rules, or the holder
        /// accepts it.
        #[test]
        fn rejection_follows_the_lease_rules(
            events in prop::collection::vec(event(), 0..64),
        ) {
            let mut holder = Holder::default();
            let mut accepted: Option<Lease<bool>> = None;

            for (lease, now) in leases(&events) {
                let expected = expected_rejection(&lease, accepted.as_ref(), now);

                let result = holder.renew(lease, now);

                prop_assert_eq!(result.err(), expected);
                if result.is_ok() {
                    accepted = Some(lease);
                }
            }
        }

        /// The holder is unlatched only under a lease with a newer grant than
        /// each block before it. A block is the first lease, an expired lease
        /// or a latch for a reason of the worker. The model of the blocks does
        /// not read the latch of the holder.
        #[test]
        fn holder_is_unlatched_only_under_a_grant_newer_than_each_block(
            events in prop::collection::vec(event(), 0..64),
        ) {
            let mut holder = Holder::default();
            let mut now = NOW;
            let mut session = SESSION;
            let mut accepted: Option<Lease<bool>> = None;
            let mut blocked: Option<Grant> = None;

            for event in events {
                let advance = match &event {
                    Event::Renew { advance, .. }
                    | Event::Expire { advance }
                    | Event::Latch { advance } => *advance,
                };
                now += Duration::from_millis(advance);
                if let Some(last) = &accepted && last.until <= now {
                    blocked = blocked.max(Some(last.grant));
                }
                match event {
                    Event::Renew { newer_session, epoch, length, .. } => {
                        if newer_session {
                            session += Duration::from_secs(1);
                        }
                        let lease = Lease {
                            grant: Grant { session, epoch },
                            permission: true,
                            until: now + Duration::from_millis(length),
                        };
                        if holder.renew(lease, now).is_ok() {
                            if accepted.is_none() {
                                blocked = Some(lease.grant);
                            }
                            accepted = Some(lease);
                        }
                    }
                    Event::Expire { .. } => {
                        holder.expire(now);
                    }
                    Event::Latch { .. } => {
                        holder.latch_now(Reason::Other);
                        if let Some(last) = &accepted {
                            blocked = blocked.max(Some(last.grant));
                        }
                    }
                }
                if let Some(lease) = holder.lease(now) && holder.latch().is_none() {
                    prop_assert_eq!(Some(lease), accepted.as_ref());
                    prop_assert!(lease.until > now);
                    prop_assert!(Some(lease.grant) > blocked);
                }
            }
        }
    }
}
