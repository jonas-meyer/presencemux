//! The choice of the frame for the host: live, a slate or blank.
//!
//! Times are `CLOCK_MONOTONIC`. The choice has no I/O and no clock of its own.

use std::fmt;
use std::time::Duration;

use presencemux_lease::LeaseHolder;
pub(crate) use presencemux_lease::{Grant, Rejection};

/// The largest age of a live frame that the worker sends. A live lease without
/// such a frame for this time latches the worker.
const FRESH: Duration = Duration::from_millis(500);

/// A frame that a lease permits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Permission {
    Blank,
    /// The slate with this name in `config.toml`.
    Slate(String),
    Live,
}

/// A permission from presencemuxd that expires at a fixed time.
pub(crate) type Lease = presencemux_lease::Lease<Permission>;

/// A block of live video until a lease with a newer grant arrives.
pub(crate) type Latch = presencemux_lease::Latch<Reason>;

/// The reason for a latch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Reason {
    /// The lease expired before the next lease arrived.
    Expired,
    /// A live lease had no fresh frame.
    Stalled,
    /// The worker started. The first lease latches the worker.
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
            Self::Stalled => "stalled",
            Self::Started => "started",
        })
    }
}

/// The frame that the worker sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shown<'a> {
    Blank,
    /// The slate with this name.
    Slate(&'a str),
    Live,
}

/// The lease and the latch of the worker.
#[derive(Clone, Debug, Default)]
pub(crate) struct Choice {
    holder: LeaseHolder<Permission, Reason>,
    /// The start time of the live permission of the current grant.
    live_since: Option<Duration>,
}

impl Choice {
    /// Returns the latch, if the worker is latched.
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
    /// worker first.
    pub(crate) fn renew(&mut self, lease: Lease, now: Duration) -> Result<(), Rejection> {
        let new_live = self
            .holder
            .last()
            .is_none_or(|last| last.grant != lease.grant || last.permission != Permission::Live);
        let live = lease.permission == Permission::Live;
        self.holder.renew(lease, now)?;
        if !live {
            self.live_since = None;
        } else if new_live {
            self.live_since = Some(now);
        }
        Ok(())
    }

    /// Checks the lease and the frames at the time `now`. `newest` is the
    /// capture time of the newest camera frame. Latches the worker if the last
    /// lease expired, or if a live lease had no fresh frame for [`FRESH`].
    /// Returns the new latch, if the worker latched now.
    pub(crate) fn check(&mut self, now: Duration, newest: Option<Duration>) -> Option<Latch> {
        if let Some(latch) = self.holder.expire(now) {
            return Some(latch);
        }
        let lease = self.holder.lease(now)?;
        let stalled = lease.permission == Permission::Live
            && self.holder.latch().is_none()
            && !is_fresh(now, newest.max(self.live_since));
        if !stalled {
            return None;
        }
        self.holder.latch_now(Reason::Stalled)
    }

    /// Returns the frame at the time `now`. `newest` is the capture time of the
    /// newest camera frame.
    pub(crate) fn shown(&self, now: Duration, newest: Option<Duration>) -> Shown<'_> {
        let permission = self.lease(now).map(|lease| &lease.permission);
        decide(permission, self.latch().is_some(), is_fresh(now, newest))
    }
}

/// Returns whether a frame with the capture time `newest` is younger than
/// [`FRESH`] at the time `now`. A capture time after `now` is not fresh.
pub(crate) fn is_fresh(now: Duration, newest: Option<Duration>) -> bool {
    newest
        .and_then(|newest| now.checked_sub(newest))
        .is_some_and(|age| age < FRESH)
}

/// Returns the frame for `permission`, `latched` and `fresh`. `permission` is
/// `None` without a lease, or after its expiry.
fn decide(permission: Option<&Permission>, latched: bool, fresh: bool) -> Shown<'_> {
    match permission {
        Some(Permission::Live) if !latched && fresh => Shown::Live,
        Some(Permission::Slate(name)) => Shown::Slate(name),
        Some(Permission::Live | Permission::Blank) | None => Shown::Blank,
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use proptest::prelude::*;

    use presencemux_lease::MAX_LEASE;

    use super::*;

    const SESSION: Duration = Duration::from_secs(1_000);
    const NOW: Duration = Duration::from_secs(2_000);
    /// The length of the leases in the tests, the longest that the worker
    /// accepts.
    const LEASE: Duration = Duration::from_secs(2);

    fn grant(epoch: u32) -> Grant {
        Grant {
            session: SESSION,
            epoch,
        }
    }

    fn lease(epoch: u32, permission: Permission) -> Lease {
        Lease {
            grant: grant(epoch),
            permission,
            until: NOW + LEASE,
        }
    }

    fn live(epoch: u32) -> Lease {
        lease(epoch, Permission::Live)
    }

    fn slate(epoch: u32) -> Lease {
        lease(epoch, Permission::Slate("away".to_owned()))
    }

    /// Returns a choice with the blank lease of epoch 1 at `NOW`, after the
    /// blank boot lease of epoch 0. The worker is not latched.
    fn blank_choice() -> Choice {
        let mut choice = Choice::default();
        choice.renew(lease(0, Permission::Blank), NOW).unwrap();
        choice.renew(lease(1, Permission::Blank), NOW).unwrap();
        choice
    }

    /// Returns a choice with the live lease of epoch 1 at `NOW`, after the
    /// blank boot lease of epoch 0. The worker is not latched.
    fn live_choice() -> Choice {
        let mut choice = Choice::default();
        choice.renew(lease(0, Permission::Blank), NOW).unwrap();
        choice.renew(live(1), NOW).unwrap();
        choice
    }

    /// Returns `live_choice` after a stall at `NOW + 500 ms`.
    fn stalled_choice() -> Choice {
        let mut choice = live_choice();
        choice.check(NOW + Duration::from_millis(500), Some(NOW));
        choice
    }

    /// Returns `live_choice` after the expiry of its lease at `NOW + LEASE`.
    fn expired_choice() -> Choice {
        let mut choice = live_choice();
        choice.check(NOW + LEASE, Some(NOW + LEASE));
        choice
    }

    #[test]
    fn live_lease_with_a_fresh_frame_shows_live() {
        let choice = live_choice();

        let shown = choice.shown(NOW, Some(NOW));

        assert_eq!(shown, Shown::Live);
    }

    #[test]
    fn live_lease_with_a_frame_just_younger_than_500_ms_shows_live() {
        let choice = live_choice();

        let shown = choice.shown(NOW + Duration::from_micros(499_999), Some(NOW));

        assert_eq!(shown, Shown::Live);
    }

    #[test]
    fn live_lease_with_a_frame_of_500_ms_shows_blank() {
        let choice = live_choice();

        let shown = choice.shown(NOW + Duration::from_millis(500), Some(NOW));

        assert_eq!(shown, Shown::Blank);
    }

    #[test]
    fn live_lease_without_a_frame_shows_blank() {
        let choice = live_choice();

        let shown = choice.shown(NOW, None);

        assert_eq!(shown, Shown::Blank);
    }

    #[test]
    fn live_lease_with_a_frame_from_the_future_shows_blank() {
        let choice = live_choice();

        let shown = choice.shown(NOW, Some(NOW + Duration::from_millis(1)));

        assert_eq!(shown, Shown::Blank);
    }

    #[test]
    fn live_lease_without_a_fresh_frame_for_500_ms_latches_the_worker_as_stalled() {
        let mut choice = live_choice();

        let latch = choice.check(NOW + Duration::from_millis(500), Some(NOW));

        assert_eq!(
            latch,
            Some(Latch {
                grant: grant(1),
                reason: Reason::Stalled,
            })
        );
    }

    #[test]
    fn new_live_grant_with_an_old_frame_does_not_latch_the_worker_at_once() {
        let later = NOW + Duration::from_secs(5);
        let mut choice = Choice::default();
        let boot = Lease {
            until: later + LEASE,
            ..lease(0, Permission::Blank)
        };
        choice.renew(boot, later).unwrap();
        let new_live = Lease {
            until: later + LEASE,
            ..live(1)
        };
        choice.renew(new_live, later).unwrap();

        let latch = choice.check(later, Some(NOW));

        assert_eq!(latch, None);
    }

    #[test]
    fn new_live_grant_without_a_frame_latches_the_worker_after_500_ms() {
        let mut choice = live_choice();

        let latch = choice.check(NOW + Duration::from_millis(500), None);

        assert_eq!(latch.map(|latch| latch.reason), Some(Reason::Stalled));
    }

    #[test]
    fn stalled_worker_with_a_fresh_frame_again_shows_blank() {
        let choice = stalled_choice();
        let now = NOW + Duration::from_millis(600);

        let shown = choice.shown(now, Some(now));

        assert_eq!(shown, Shown::Blank);
    }

    #[test]
    fn slate_lease_without_a_frame_does_not_latch_the_worker() {
        let mut choice = live_choice();
        choice.renew(slate(2), NOW).unwrap();

        let latch = choice.check(NOW + Duration::from_millis(500), None);

        assert_eq!(latch, None);
    }

    #[test]
    fn first_lease_latches_the_worker_as_started() {
        let mut choice = Choice::default();

        choice.renew(live(3), NOW).unwrap();

        assert_eq!(
            choice.latch(),
            Some(Latch {
                grant: grant(3),
                reason: Reason::Started,
            })
        );
    }

    #[test]
    fn first_live_lease_shows_blank() {
        let mut choice = Choice::default();
        choice.renew(live(3), NOW).unwrap();

        let shown = choice.shown(NOW, Some(NOW));

        assert_eq!(shown, Shown::Blank);
    }

    #[test]
    fn newer_epoch_clears_the_latch() {
        let mut choice = Choice::default();
        choice.renew(live(3), NOW).unwrap();

        choice.renew(live(4), NOW).unwrap();

        assert_eq!(choice.latch(), None);
    }

    #[test]
    fn newer_session_with_a_lower_epoch_clears_the_latch() {
        let mut choice = Choice::default();
        choice.renew(live(3), NOW).unwrap();
        let newer_session = Lease {
            grant: Grant {
                session: SESSION + Duration::from_secs(1),
                epoch: 0,
            },
            ..live(0)
        };

        choice.renew(newer_session, NOW).unwrap();

        assert_eq!(choice.latch(), None);
    }

    #[test]
    fn lease_with_the_same_epoch_keeps_the_latch() {
        let mut choice = Choice::default();
        choice.renew(live(3), NOW).unwrap();

        choice.renew(live(3), NOW).unwrap();

        assert!(choice.latch().is_some());
    }

    #[test]
    fn latched_worker_shows_the_slate_of_a_lease_with_the_same_epoch() {
        let mut choice = Choice::default();
        choice.renew(live(3), NOW).unwrap();
        choice.renew(slate(3), NOW).unwrap();

        let shown = choice.shown(NOW, Some(NOW));

        assert_eq!(shown, Shown::Slate("away"));
    }

    #[test]
    fn newer_grant_after_a_stall_shows_live() {
        let mut choice = stalled_choice();
        let now = NOW + Duration::from_millis(500);
        choice.renew(live(2), now).unwrap();

        let shown = choice.shown(now, Some(now));

        assert_eq!(shown, Shown::Live);
    }

    #[test]
    fn newer_grant_after_an_expiry_shows_live() {
        let mut choice = expired_choice();
        let now = NOW + LEASE;
        let newer = Lease {
            until: now + LEASE,
            ..live(2)
        };
        choice.renew(newer, now).unwrap();

        let shown = choice.shown(now, Some(now));

        assert_eq!(shown, Shown::Live);
    }

    #[test]
    fn worker_without_a_lease_shows_blank() {
        let choice = Choice::default();

        let shown = choice.shown(NOW, Some(NOW));

        assert_eq!(shown, Shown::Blank);
    }

    #[test]
    fn blank_lease_with_a_fresh_frame_shows_blank() {
        let mut choice = live_choice();
        choice.renew(lease(2, Permission::Blank), NOW).unwrap();

        let shown = choice.shown(NOW, Some(NOW));

        assert_eq!(shown, Shown::Blank);
    }

    #[test]
    fn slate_lease_without_a_frame_shows_the_slate() {
        let mut choice = live_choice();
        choice.renew(slate(2), NOW).unwrap();

        let shown = choice.shown(NOW, None);

        assert_eq!(shown, Shown::Slate("away"));
    }

    #[test]
    fn expired_lease_shows_blank() {
        let choice = live_choice();

        let shown = choice.shown(NOW + LEASE, Some(NOW + LEASE));

        assert_eq!(shown, Shown::Blank);
    }

    #[test]
    fn expiry_of_the_lease_latches_the_worker_as_expired() {
        let mut choice = live_choice();

        let latch = choice.check(NOW + LEASE, Some(NOW + LEASE));

        assert_eq!(
            latch,
            Some(Latch {
                grant: grant(1),
                reason: Reason::Expired,
            })
        );
    }

    #[test]
    fn expiry_gives_the_latch_once() {
        let mut choice = expired_choice();

        let latch = choice.check(NOW + LEASE, Some(NOW + LEASE));

        assert_eq!(latch, None);
    }

    #[test]
    fn expired_lease_is_not_the_accepted_lease() {
        let choice = expired_choice();

        assert_eq!(choice.lease(NOW + LEASE), None);
    }

    #[test]
    fn lease_at_its_expiry_time_is_not_the_accepted_lease() {
        let choice = live_choice();

        assert_eq!(choice.lease(NOW + LEASE), None);
    }

    #[test]
    fn renewal_after_the_expiry_keeps_the_worker_latched() {
        let mut choice = live_choice();
        let later = NOW + LEASE;
        let renewal = Lease {
            until: later + LEASE,
            ..live(1)
        };

        choice.renew(renewal, later).unwrap();

        assert_eq!(
            choice.latch().map(|latch| latch.reason),
            Some(Reason::Expired)
        );
    }

    #[test]
    fn rejected_lease_after_the_expiry_latches_the_worker_as_expired() {
        let mut choice = live_choice();

        choice.renew(live(0), NOW + LEASE).unwrap_err();

        assert_eq!(
            choice.latch().map(|latch| latch.reason),
            Some(Reason::Expired)
        );
    }

    #[test]
    fn expiry_replaces_the_reason_of_a_stalled_latch() {
        let mut choice = stalled_choice();

        choice.check(NOW + LEASE, None);

        assert_eq!(
            choice.latch().map(|latch| latch.reason),
            Some(Reason::Expired)
        );
    }

    #[test]
    fn lease_that_expires_now_is_late() {
        let mut choice = Choice::default();
        let expired = Lease {
            until: NOW,
            ..live(0)
        };

        let result = choice.renew(expired, NOW);

        assert_eq!(result, Err(Rejection::Late));
    }

    #[test]
    fn lease_that_expires_2_s_after_now_is_accepted() {
        let mut choice = Choice::default();
        let long = Lease {
            until: NOW + Duration::from_secs(2),
            ..live(0)
        };

        let result = choice.renew(long, NOW);

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn lease_that_expires_later_than_2_s_after_now_is_too_long() {
        let mut choice = Choice::default();
        let long = Lease {
            until: NOW + Duration::from_secs(2) + Duration::from_micros(1),
            ..live(0)
        };

        let result = choice.renew(long, NOW);

        assert_eq!(result, Err(Rejection::TooLong));
    }

    #[test]
    fn lease_from_an_older_session_is_rejected() {
        let mut choice = live_choice();
        let older_session = Lease {
            grant: Grant {
                session: Duration::from_secs(999),
                epoch: 9,
            },
            ..live(0)
        };

        let result = choice.renew(older_session, NOW);

        assert_eq!(result, Err(Rejection::OldSession));
    }

    #[test]
    fn lease_with_an_older_epoch_is_rejected() {
        let mut choice = live_choice();

        let result = choice.renew(live(0), NOW);

        assert_eq!(result, Err(Rejection::OldEpoch));
    }

    #[test]
    fn lease_with_the_same_grant_and_a_higher_permission_is_accepted() {
        let mut choice = live_choice();
        choice.renew(slate(2), NOW).unwrap();

        let result = choice.renew(live(2), NOW);

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn raise_to_live_in_the_same_grant_without_a_frame_does_not_latch_the_worker_before_500_ms() {
        let mut choice = blank_choice();
        let raised = NOW + Duration::from_millis(400);
        choice.renew(live(1), raised).unwrap();

        let latch = choice.check(raised + Duration::from_millis(499), None);

        assert_eq!(latch, None);
    }

    #[test]
    fn raise_to_live_in_the_same_grant_without_a_frame_latches_the_worker_after_500_ms() {
        let mut choice = blank_choice();
        let raised = NOW + Duration::from_millis(400);
        choice.renew(live(1), raised).unwrap();

        let latch = choice.check(raised + Duration::from_millis(500), None);

        assert_eq!(latch.map(|latch| latch.reason), Some(Reason::Stalled));
    }

    #[test]
    fn lease_with_the_same_grant_and_a_lower_permission_is_accepted() {
        let mut choice = live_choice();

        let result = choice.renew(slate(1), NOW);

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn rejected_lease_does_not_replace_the_live_lease() {
        let mut choice = live_choice();
        choice.renew(slate(0), NOW).unwrap_err();

        let shown = choice.shown(NOW, Some(NOW));

        assert_eq!(shown, Shown::Live);
    }

    /// Returns the latch as text for the snapshot.
    fn latch_text(latch: Option<Latch>) -> String {
        latch.map_or_else(
            || "none".to_owned(),
            |latch| format!("{:?} at epoch {}", latch.reason, latch.grant.epoch),
        )
    }

    /// An event in the snapshot of the latch.
    #[derive(Clone, Copy, Debug)]
    enum Transition {
        SameGrant,
        NewerEpoch,
        NewerSession,
        Expiry,
        NoFrame,
    }

    impl Transition {
        const ALL: [Self; 5] = [
            Self::SameGrant,
            Self::NewerEpoch,
            Self::NewerSession,
            Self::Expiry,
            Self::NoFrame,
        ];

        fn name(self) -> &'static str {
            match self {
                Self::SameGrant => "live lease, same grant",
                Self::NewerEpoch => "live lease, newer epoch",
                Self::NewerSession => "live lease, newer session",
                Self::Expiry => "expiry",
                Self::NoFrame => "no frame for 500 ms",
            }
        }

        /// Applies the event to `choice` at `now`, and returns the result as
        /// text.
        fn apply(self, choice: &mut Choice, now: Duration) -> String {
            let epoch = choice.holder.last().map_or(0, |last| last.grant.epoch);
            let renewal = |grant| Lease {
                grant,
                permission: Permission::Live,
                until: now + LEASE,
            };
            match self {
                Self::SameGrant => format!("{:?}", choice.renew(renewal(grant(epoch)), now)),
                Self::NewerEpoch => {
                    format!("{:?}", choice.renew(renewal(grant(epoch + 1)), now))
                }
                Self::NewerSession => {
                    let newer = Grant {
                        session: SESSION + Duration::from_secs(1),
                        epoch: 0,
                    };
                    format!("{:?}", choice.renew(renewal(newer), now))
                }
                Self::Expiry => {
                    let later = now + Duration::from_secs(2);
                    format!(
                        "{:?}",
                        choice.check(later, Some(later)).map(|latch| latch.reason)
                    )
                }
                Self::NoFrame => {
                    let later = now + Duration::from_millis(500);
                    format!(
                        "{:?}",
                        choice.check(later, Some(now)).map(|latch| latch.reason)
                    )
                }
            }
        }
    }

    #[test]
    fn latch_after_each_event() {
        let mut started = Choice::default();
        started.renew(live(3), NOW).unwrap();
        // Each state is a choice with a live lease, and the time of the state.
        let states = [
            (live_choice(), NOW),
            (started, NOW),
            (stalled_choice(), NOW + Duration::from_millis(500)),
            (expired_choice(), NOW + LEASE),
        ];
        let mut table = String::from(
            "latch before | event | result | latch after | shown with a fresh frame\n",
        );

        for (choice, now) in &states {
            for transition in Transition::ALL {
                let mut choice = choice.clone();
                let before = latch_text(choice.latch());
                let result = transition.apply(&mut choice, *now);
                let shown = choice.shown(*now, Some(*now));
                writeln!(
                    table,
                    "{before} | {} | {result} | {} | {shown:?}",
                    transition.name(),
                    latch_text(choice.latch())
                )
                .unwrap();
            }
        }

        insta::assert_snapshot!(table);
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
            permission: Permission,
            /// The length of the lease in milliseconds.
            length: u64,
        },
        /// The frame timer runs, with a frame of `age` milliseconds.
        Check { advance: u64, age: Option<u64> },
    }

    fn permission() -> impl Strategy<Value = Permission> {
        prop_oneof![
            Just(Permission::Blank),
            Just(Permission::Slate("away".to_owned())),
            Just(Permission::Live),
        ]
    }

    fn event() -> impl Strategy<Value = Event> {
        prop_oneof![
            (
                0..1_500u64,
                prop::bool::weighted(0.05),
                0..4u32,
                permission(),
                0..2_500u64
            )
                .prop_map(|(advance, newer_session, epoch, permission, length)| {
                    Event::Renew {
                        advance,
                        newer_session,
                        epoch,
                        permission,
                        length,
                    }
                }),
            (0..700u64, prop::option::of(0..1_000u64))
                .prop_map(|(advance, age)| Event::Check { advance, age }),
        ]
    }

    /// Returns the rejection of `lease` at `now` after the accepted lease
    /// `last`, from the rules in `cam.md`.
    fn expected_rejection(lease: &Lease, last: Option<&Lease>, now: Duration) -> Option<Rejection> {
        if lease.until <= now {
            Some(Rejection::Late)
        } else if lease.until > now + MAX_LEASE {
            Some(Rejection::TooLong)
        } else if last.is_some_and(|last| lease.grant.session < last.grant.session) {
            Some(Rejection::OldSession)
        } else if last.is_some_and(|last| lease.grant < last.grant) {
            Some(Rejection::OldEpoch)
        } else {
            None
        }
    }

    proptest! {
        #[test]
        fn leases_and_frames_keep_the_rules(events in prop::collection::vec(event(), 0..64)) {
            let mut choice = Choice::default();
            let mut now = NOW;
            let mut session = SESSION;
            let mut newest = None;
            let mut accepted: Option<Lease> = None;
            // The newest grant that blocks live. It is the newest grant of the
            // first lease, of an expired lease or of a latch.
            let mut blocked: Option<Grant> = None;

            for event in events {
                let advance = match &event {
                    Event::Renew { advance, .. } | Event::Check { advance, .. } => *advance,
                };
                now += Duration::from_millis(advance);
                if let Some(last) = &accepted && last.until <= now {
                    blocked = blocked.max(Some(last.grant));
                }
                match event {
                    Event::Renew { newer_session, epoch, permission, length, .. } => {
                        if newer_session {
                            session += Duration::from_secs(1);
                        }
                        let lease = Lease {
                            grant: Grant { session, epoch },
                            permission,
                            until: now + Duration::from_millis(length),
                        };
                        let expected = expected_rejection(&lease, accepted.as_ref(), now);

                        let result = choice.renew(lease.clone(), now);

                        prop_assert_eq!(result.err(), expected);
                        if result.is_ok() {
                            if accepted.is_none() {
                                blocked = Some(lease.grant);
                            }
                            accepted = Some(lease);
                        }
                    }
                    Event::Check { age, .. } => {
                        newest = age.map(|age| now.saturating_sub(Duration::from_millis(age)));
                        if let Some(latch) = choice.check(now, newest) {
                            blocked = blocked.max(Some(latch.grant));
                        }
                    }
                }
                if choice.shown(now, newest) == Shown::Live {
                    let lease = accepted.as_ref().unwrap();
                    prop_assert!(Some(lease.grant) > blocked);
                    prop_assert!(lease.until > now);
                    prop_assert_eq!(&lease.permission, &Permission::Live);
                    prop_assert!(is_fresh(now, newest));
                }
            }
        }
    }
}
