use core::fmt::Debug;

use crate::{Device, HostStatus, OnAir, Report, Reported, policy};

/// An output of the controller, with its own adapter in the daemon.
pub trait Plane {
    /// The value at boot. It must be safe.
    type Desired: Copy + PartialEq + Debug + Default;
    /// The report of the owner of the devices of the plane.
    type Report: Copy + PartialEq + Debug;
    type Target: Copy + PartialEq + Debug;

    const ID: PlaneId;
    /// The target at boot.
    const SAFE: Self::Target;
}

/// A plane that can show the person or the room.
pub trait PrivacyPlane:
    Plane<Target = Granted<Self::Permission>, Report: Report<Permission = Self::Permission>>
{
    type Permission: Copy + PartialEq + Debug;

    fn on_air(permission: Self::Permission) -> OnAir;

    /// Returns the on-air level that a desired value asks for.
    fn requested(desired: Self::Desired) -> OnAir;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlaneId {
    Cam,
    Mic,
}

impl PlaneId {
    pub const ALL: [Self; 2] = [Self::Cam, Self::Mic];
}

/// The state of a privacy plane, as on-air levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlaneSummary {
    /// The level that the user asked for.
    pub requested: OnAir,
    /// The level of the target of core.
    pub granted: OnAir,
    /// The level of the output that the owner reports. `None` without a fresh
    /// report.
    pub actual: Option<OnAir>,
    /// Whether the owner applies the target of core.
    pub in_effect: bool,
    pub hold: Option<Cause>,
}

/// The number of the user command that granted a target. Epochs start at zero
/// in each daemon run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Epoch(u32);

impl Epoch {
    pub const BOOT: Self = Self(0);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    /// Returns the next epoch. It saturates at the maximum.
    pub(crate) const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Granted<T> {
    pub permission: T,
    pub epoch: Epoch,
}

/// The reason why automation lowered a plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cause {
    DeviceFault(Device),
    /// The daemon has no contact with the owner of the devices of the plane.
    OwnerLost(PlaneId),
    /// The owner of the devices of the plane sends more than the grant under
    /// which it made the output permits.
    OutputExceeded(PlaneId),
    ControllerStalled,
    Host(HostStatus),
    /// The host received no stream of the cam worker for the stream timeout.
    StreamStopped,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaneState<P: Plane> {
    /// The value of the last command that set this plane.
    pub(crate) requested: P::Desired,
    /// Why automation holds this plane at the safe value, until a command sets
    /// it.
    pub(crate) hold: Option<Cause>,
    pub(crate) observed: Reported<P::Report>,
    pub(crate) target: P::Target,
    /// The epoch of the last command that set this plane.
    pub(crate) epoch: Epoch,
}

impl<P: Plane> PlaneState<P> {
    pub(crate) fn new() -> Self {
        Self {
            requested: P::Desired::default(),
            hold: None,
            observed: Reported::Pending,
            target: P::SAFE,
            epoch: Epoch::BOOT,
        }
    }

    pub fn requested(&self) -> P::Desired {
        self.requested
    }

    /// Returns the safe value while a hold is set, and the request otherwise.
    pub fn desired(&self) -> P::Desired {
        if self.hold.is_some() {
            P::Desired::default()
        } else {
            self.requested
        }
    }

    pub fn observed(&self) -> &Reported<P::Report> {
        &self.observed
    }

    pub fn target(&self) -> P::Target {
        self.target
    }

    /// Returns why automation holds this plane at the safe value, until a
    /// command sets it.
    pub fn hold(&self) -> Option<Cause> {
        self.hold
    }

    pub fn summary(&self) -> PlaneSummary
    where
        P: PrivacyPlane,
    {
        let fresh = match &self.observed {
            Reported::Fresh(report) => Some(report),
            Reported::Pending | Reported::Lost => None,
        };
        PlaneSummary {
            requested: P::requested(self.requested),
            granted: P::on_air(self.target.permission),
            actual: fresh.map(Report::output),
            in_effect: fresh.is_some_and(|report| report.applied() == Some(self.target)),
            hold: self.hold,
        }
    }

    pub(crate) fn set(&mut self, requested: P::Desired, epoch: Epoch) {
        self.requested = requested;
        self.epoch = epoch;
        self.hold = None;
    }

    /// Clears the hold, and applies the request again in `epoch`.
    pub(crate) fn resume(&mut self, epoch: Epoch) {
        self.epoch = epoch;
        self.hold = None;
    }

    /// Holds the plane at the safe value. Sets the hold only if the desired
    /// value changes.
    pub(crate) fn lower(&mut self, cause: Cause) {
        if self.desired() != P::Desired::default() {
            self.hold = Some(cause);
        }
    }

    /// Records `next`, and lowers the plane if `next` gives a cause.
    pub(crate) fn observe(&mut self, next: Reported<P::Report>)
    where
        P: PrivacyPlane,
    {
        let before = core::mem::replace(&mut self.observed, next);
        if let Some(cause) = policy::cause::<P>(&before, &next, self.epoch) {
            self.lower(cause);
        }
    }

    /// Returns `next` if it differs from the last target.
    pub(crate) fn update(&mut self, next: P::Target) -> Option<P::Target> {
        (next != self.target).then(|| {
            self.target = next;
            next
        })
    }
}
