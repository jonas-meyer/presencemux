use crate::{DeviceStatus, Epoch, Granted, OnAir, Plane, PrivacyPlane};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cam;

impl Plane for Cam {
    type Desired = CamPermission;
    type Observed = CamObserved;
    type Target = Granted<CamPermission>;

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CamObserved {
    pub camera: DeviceStatus,
    pub worker: DeviceStatus,
}

/// What the cam worker may send to the laptop.
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
