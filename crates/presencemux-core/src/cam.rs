use crate::{
    Device, DeviceStatus, Epoch, Granted, OnAir, Plane, PlaneId, PlaneState, PrivacyPlane, Report,
    Reported,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cam;

impl Plane for Cam {
    type Desired = CamPermission;
    type Report = CamReport;
    type Target = Granted<CamPermission>;

    const ID: PlaneId = PlaneId::Cam;

    const SAFE: Self::Target = Granted {
        permission: CamPermission::Blank,
        epoch: Epoch::BOOT,
    };
}

impl PrivacyPlane for Cam {
    type Permission = CamPermission;

    fn on_air(permission: CamPermission) -> OnAir {
        match permission {
            CamPermission::Blank => OnAir::Off,
            CamPermission::Slate(_) => OnAir::Canned,
            CamPermission::Live => OnAir::Live,
        }
    }

    fn requested(desired: CamPermission) -> OnAir {
        Self::on_air(desired)
    }
}

/// A report of the cam worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CamReport {
    pub worker: DeviceStatus,
    pub camera: DeviceStatus,
    pub stream: CamStream,
    /// The epoch of the lease at the latch of the worker, if the worker is
    /// latched.
    pub latched: Option<Epoch>,
    pub output: CamOutput,
    /// Whether the output is above the grant under which the worker queued it.
    pub exceeded: bool,
    /// The grant of the lease that the worker applies, if the worker has a
    /// valid lease.
    pub applied: Option<Granted<CamPermission>>,
}

impl Report for CamReport {
    type Permission = CamPermission;

    fn devices(&self) -> [(Device, DeviceStatus); 2] {
        [
            (Device::CamWorker, self.worker),
            (Device::Camera, self.camera),
        ]
    }

    fn latched(&self) -> Option<Epoch> {
        self.latched
    }

    fn output(&self) -> OnAir {
        match self.output {
            CamOutput::Nothing | CamOutput::Blank => OnAir::Off,
            CamOutput::Slate(_) => OnAir::Canned,
            CamOutput::Live => OnAir::Live,
        }
    }

    fn exceeded(&self) -> bool {
        self.exceeded
    }

    fn applied(&self) -> Option<Granted<CamPermission>> {
        self.applied
    }
}

/// The frames that the host gets now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CamOutput {
    /// The host receives no stream.
    Nothing,
    Blank,
    /// A slate, with its id if the daemon knows its name.
    Slate(Option<SlateId>),
    Live,
}

impl PlaneState<Cam> {
    /// Returns whether the host receives the stream of the cam worker. Without
    /// a fresh report, the stream counts as idle.
    pub fn stream(&self) -> CamStream {
        match self.observed() {
            Reported::Fresh(report) => report.stream,
            Reported::Pending | Reported::Lost => CamStream::Idle,
        }
    }
}

/// Whether the host receives the stream of the cam worker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CamStream {
    #[default]
    Idle,
    Streaming,
}

/// What the cam worker may send to the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CamPermission {
    /// A frame with nothing from the room.
    #[default]
    Blank,
    /// A still card, chosen by its [`SlateId`].
    Slate(SlateId),
    /// The live camera. The worker shows a fallback image when frames stop.
    Live,
}

/// Identifies a configured slate. The daemon maps it to a slate name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SlateId(pub u16);
