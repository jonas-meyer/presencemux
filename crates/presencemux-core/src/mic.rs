use crate::{Device, DeviceStatus, Epoch, Granted, OnAir, Plane, PlaneId, PrivacyPlane, Report};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mic;

impl Plane for Mic {
    type Desired = MicMute;
    type Report = MicReport;
    type Target = Granted<MicPermission>;

    const ID: PlaneId = PlaneId::Mic;

    const SAFE: Self::Target = Granted {
        permission: MicPermission::Denied,
        epoch: Epoch::BOOT,
    };
}

impl PrivacyPlane for Mic {
    type Permission = MicPermission;

    fn on_air(permission: MicPermission) -> OnAir {
        match permission {
            MicPermission::Denied => OnAir::Off,
            MicPermission::Permitted => OnAir::Live,
        }
    }

    fn requested(desired: MicMute) -> OnAir {
        match desired {
            MicMute::Muted => OnAir::Off,
            MicMute::Unmuted => OnAir::Live,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MicMute {
    #[default]
    Muted,
    Unmuted,
}

/// A report of the audio worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MicReport {
    /// The stage of the audio chain that enforces the mic permission.
    pub gate: DeviceStatus,
    pub interface: DeviceStatus,
    /// The epoch of the lease at the latch of the gate, if the gate is
    /// latched.
    pub latched: Option<Epoch>,
}

impl Report for MicReport {
    fn devices(&self) -> [(Device, DeviceStatus); 2] {
        [
            (Device::Gate, self.gate),
            (Device::AudioInterface, self.interface),
        ]
    }

    fn latched(&self) -> Option<Epoch> {
        self.latched
    }
}

/// What the audio chain may pass to the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MicPermission {
    Denied,
    Permitted,
}
