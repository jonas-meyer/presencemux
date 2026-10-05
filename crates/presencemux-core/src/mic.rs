use crate::{DeviceStatus, Epoch, Granted, OnAir, Plane, PrivacyPlane};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mic;

impl Plane for Mic {
    type Desired = MicMute;
    type Observed = MicObserved;
    type Target = Granted<MicPermission>;

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MicObserved {
    pub interface: DeviceStatus,
    /// The stage of the audio chain that enforces the mic permission.
    pub gate: DeviceStatus,
}

/// What the audio chain may pass to the laptop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MicPermission {
    Denied,
    Permitted,
}
