use crate::PlaneId;

/// A device that a plane depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Device {
    Camera,
    CamWorker,
    AudioInterface,
    Gate,
}

impl Device {
    pub const ALL: [Self; 4] = [
        Self::Camera,
        Self::CamWorker,
        Self::AudioInterface,
        Self::Gate,
    ];

    pub fn plane(self) -> PlaneId {
        match self {
            Self::Camera | Self::CamWorker => PlaneId::Cam,
            Self::AudioInterface | Self::Gate => PlaneId::Mic,
        }
    }
}

/// The lifecycle state of a device or worker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DeviceStatus {
    /// Not present, or not reported yet.
    #[default]
    Missing,
    Initializing,
    Ready,
    /// The device does not operate. The daemon also sets this status when the
    /// owner of the device sends no more reports.
    Fault,
}

/// A summary of the status of all devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Health {
    Ready,
    Degraded,
    Fault,
}

impl DeviceStatus {
    pub fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }
}
