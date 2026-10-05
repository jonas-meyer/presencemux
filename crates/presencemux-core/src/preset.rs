use crate::{CamPermission, MicMute};

/// Desired values for some planes, applied in one step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Preset {
    /// `None` keeps the current value.
    pub cam: Option<CamPermission>,
    pub mic: Option<MicMute>,
}

impl Preset {
    /// Every plane at its safe value. Boot starts here, and configuration
    /// cannot change it.
    pub const PRIVACY: Self = Self {
        cam: Some(CamPermission::Blank),
        mic: Some(MicMute::Muted),
    };
}
