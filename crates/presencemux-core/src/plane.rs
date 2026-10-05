use core::fmt::Debug;

use crate::{Device, DeviceStatus, HostStatus, OnAir, policy};

/// An output of the controller, with its own adapter in the daemon.
pub trait Plane {
    /// The value at boot. It must be safe.
    type Desired: Copy + PartialEq + Debug + Default;
    /// The value before any report.
    type Observed: Clone + PartialEq + Debug + Default;
    type Target: Copy + PartialEq + Debug;

    /// The target at boot.
    const SAFE: Self::Target;
}

/// A plane that can show the person or the room.
pub trait PrivacyPlane: Plane<Target = Granted<Self::Permission>> {
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
    /// The level that the plane shows now.
    pub on_air: OnAir,
    pub cause: Option<Cause>,
}

/// The number of the user command that granted a target. Epochs start at zero
/// in each daemon run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Epoch(u32);

impl Epoch {
    pub const BOOT: Self = Self(0);

    pub const fn get(self) -> u32 {
        self.0
    }

    // Saturates, because a wrapped epoch would look older than the last grant.
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
    ControllerStalled,
    Host(HostStatus),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaneState<P: Plane> {
    pub(crate) desired: P::Desired,
    pub(crate) observed: P::Observed,
    pub(crate) target: P::Target,
    pub(crate) cause: Option<Cause>,
}

impl<P: Plane> PlaneState<P> {
    pub(crate) fn new() -> Self {
        Self {
            desired: P::Desired::default(),
            observed: P::Observed::default(),
            target: P::SAFE,
            cause: None,
        }
    }

    pub fn desired(&self) -> P::Desired {
        self.desired
    }

    pub fn observed(&self) -> &P::Observed {
        &self.observed
    }

    pub fn target(&self) -> P::Target {
        self.target
    }

    /// Returns why automation lowered this plane, until the user sets it again.
    pub fn cause(&self) -> Option<Cause> {
        self.cause
    }

    pub fn summary(&self) -> PlaneSummary
    where
        P: PrivacyPlane,
    {
        PlaneSummary {
            requested: P::requested(self.desired),
            on_air: P::on_air(self.target.permission),
            cause: self.cause,
        }
    }

    pub(crate) fn set(&mut self, desired: P::Desired) {
        self.desired = desired;
        self.cause = None;
    }

    /// Sets the safe desired value. Records the cause only if the desired value
    /// changes.
    pub(crate) fn lower(&mut self, cause: Cause) {
        let safe = P::Desired::default();
        if self.desired != safe {
            self.desired = safe;
            self.cause = Some(cause);
        }
    }

    /// Records the status of a device and lowers the plane if the policy says
    /// so.
    pub(crate) fn observe(
        &mut self,
        device: Device,
        status: DeviceStatus,
        field: impl FnOnce(&mut P::Observed) -> &mut DeviceStatus,
    ) {
        let before = core::mem::replace(field(&mut self.observed), status);
        if policy::device_lowers(before, status) {
            self.lower(Cause::DeviceFault(device));
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
