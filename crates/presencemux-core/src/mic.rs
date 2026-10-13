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
    pub output: MicOutput,
    /// Whether the output is above the grant under which the gate wrote it.
    pub exceeded: bool,
    /// The grant of the lease that the gate applies, if the gate has a valid
    /// lease.
    pub applied: Option<Granted<MicPermission>>,
}

/// The audio that the host gets now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MicOutput {
    /// The gate writes digital silence.
    Silent,
    /// The gate passes audio with a falling gain, after a lease that closes
    /// it.
    Closing,
    /// The gate passes audio while it opens or is open.
    Passing,
}

impl Report for MicReport {
    type Permission = MicPermission;

    fn devices(&self) -> [(Device, DeviceStatus); 2] {
        [
            (Device::Gate, self.gate),
            (Device::AudioInterface, self.interface),
        ]
    }

    fn latched(&self) -> Option<Epoch> {
        self.latched
    }

    fn output(&self) -> OnAir {
        match self.output {
            MicOutput::Silent => OnAir::Off,
            MicOutput::Closing | MicOutput::Passing => OnAir::Live,
        }
    }

    fn exceeded(&self) -> bool {
        self.exceeded
    }

    fn applied(&self) -> Option<Granted<MicPermission>> {
        self.applied
    }
}

/// What the audio chain may pass to the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MicPermission {
    Denied,
    Permitted,
}
