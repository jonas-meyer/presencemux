use core::fmt::Debug;

use crate::{Device, HostStatus, OnAir, Report, Reported, policy};

/// An output of the controller, with its own adapter in the daemon.
pub trait Plane {
    /// The value that the user asks for. Its default is the value at boot,
    /// and it must be safe.
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
    /// The level of the target of the controller.
    pub granted: OnAir,
    /// What the host gets and can get. `None` without a fresh report.
    pub exposed: Option<Exposed>,
    /// Why the exposure differs from the request, if it does.
    pub reason: Option<Reason>,
    /// Whether a resume applies the request again now.
    pub resumable: bool,
}

impl PlaneSummary {
    /// Returns why automation holds the plane at the safe value.
    pub const fn hold(&self) -> Option<Cause> {
        match self.reason {
            Some(Reason::Held(cause)) => Some(cause),
            Some(Reason::Updating | Reason::Blocked | Reason::Waiting) | None => None,
        }
    }
}

/// What the host of a plane gets, and what it can get without a press.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Exposed {
    /// The level of the output that the owner reports.
    pub actual: OnAir,
    /// The level that the host can get without a press. It is the higher of
    /// `actual` and the level of the armed grant.
    pub exposure: OnAir,
}

impl Exposed {
    /// Returns whether the host gets the exposure now.
    pub fn flow(self) -> Flow {
        if self.actual < self.exposure {
            Flow::Idle
        } else {
            Flow::Flowing
        }
    }
}

/// Whether the host gets the exposure of a plane now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Flow {
    Flowing,
    /// The host gets less than the exposure, for example because no app reads
    /// the stream.
    Idle,
}

/// Why the exposure of a plane differs from its request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    /// Automation holds the plane at the safe value.
    Held(Cause),
    /// The owner applies an older grant.
    Updating,
    /// The latch of the owner blocks the grant.
    Blocked,
    /// The policy grants less than the request, for example while a device
    /// is not ready.
    Waiting,
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
    /// The owner of the devices of the plane reports an output above its
    /// grant.
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
    /// Why automation holds this plane at the safe value. A command that sets
    /// or resumes the plane clears it.
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

    /// Returns why automation holds this plane at the safe value. A command
    /// that sets or resumes the plane clears the hold.
    pub fn hold(&self) -> Option<Cause> {
        self.hold
    }

    /// Returns the fresh report of the owner, if there is one.
    pub fn fresh(&self) -> Option<&P::Report> {
        match &self.observed {
            Reported::Fresh(report) => Some(report),
            Reported::Pending | Reported::Lost => None,
        }
    }

    /// Returns the permission of the grant that the owner applies, unless the
    /// owner is latched. The host can get it without a press.
    pub(crate) fn armed(&self) -> Option<P::Permission>
    where
        P: PrivacyPlane,
    {
        let report = self.fresh()?;
        if report.latched().is_some() {
            return None;
        }
        report.applied().map(|grant| grant.permission)
    }

    /// Returns what the host gets and can get. `None` without a fresh report.
    pub(crate) fn exposed(&self) -> Option<Exposed>
    where
        P: PrivacyPlane,
    {
        let actual = self.fresh()?.output();
        let armed = self.armed().map_or(OnAir::Off, P::on_air);
        Some(Exposed {
            actual,
            exposure: actual.max(armed),
        })
    }

    /// Returns the summary of the plane with the host status `host`.
    pub(crate) fn summary(&self, host: HostStatus) -> PlaneSummary
    where
        P: PrivacyPlane,
    {
        let requested = P::requested(self.requested);
        let granted = P::on_air(self.target.permission);
        let reason = match (self.hold, self.fresh()) {
            (Some(cause), _) => Some(Reason::Held(cause)),
            (None, Some(report)) => self.report_reason(report),
            (None, None) => None,
        };
        PlaneSummary {
            requested,
            granted,
            exposed: self.exposed(),
            reason,
            resumable: policy::resumes(self, host),
        }
    }

    /// Returns why the exposure differs from the request, from the fresh report
    /// `report`. Use it only while the plane has no hold. `Updating` comes
    /// first, then `Blocked`, then `Waiting`.
    fn report_reason(&self, report: &P::Report) -> Option<Reason>
    where
        P: PrivacyPlane,
    {
        let granted = P::on_air(self.target.permission);
        if report.applied() != Some(self.target) {
            Some(Reason::Updating)
        } else if report.latched().is_some() && granted > report.output() {
            Some(Reason::Blocked)
        } else if P::requested(self.requested) > granted {
            Some(Reason::Waiting)
        } else {
            None
        }
    }

    pub(crate) fn set(&mut self, requested: P::Desired, epoch: Epoch) {
        self.requested = requested;
        self.epoch = epoch;
        self.hold = None;
    }

    /// Clears the hold and applies the request again in `epoch`.
    pub(crate) fn resume(&mut self, epoch: Epoch) {
        self.epoch = epoch;
        self.hold = None;
    }

    /// Holds the plane at the safe value. Does nothing if the desired value is
    /// already the safe value.
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
