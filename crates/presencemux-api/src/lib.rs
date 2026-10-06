//! The Varlink API of presencemuxd, shared by the daemon and its clients.

use presencemux_core as core;
use serde::{Deserialize, Serialize};

/// The name of the built-in preset that sets every plane to its safe value.
pub const PRESET_PRIVACY: &str = "privacy";
/// The cam value for a blank image.
pub const CAM_BLANK: &str = "blank";
/// The cam value for the live camera.
pub const CAM_LIVE: &str = "live";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub cam: CamStatus,
    pub mic: MicStatus,
    pub host: Host,
    pub health: Health,
    /// The last preset that the user applied, until the user changes a plane by
    /// hand.
    pub preset: Option<String>,
}

/// The cam plane in a [`Status`]. Its values are [`CAM_BLANK`], [`CAM_LIVE`] or
/// a slate name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CamStatus {
    pub desired: String,
    pub effective: String,
    pub cause: Option<Cause>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MicStatus {
    pub desired: Mute,
    pub effective: Permission,
    pub cause: Option<Cause>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mute {
    Muted,
    Unmuted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Denied,
    Permitted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Host {
    Disconnected,
    Suspended,
    Connected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Ready,
    Degraded,
    Fault,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    CameraFault,
    CamWorkerFault,
    AudioInterfaceFault,
    GateFault,
    ControllerStalled,
    HostDisconnected,
    HostSuspended,
    HostConnected,
}

impl From<core::MicMute> for Mute {
    fn from(mute: core::MicMute) -> Self {
        match mute {
            core::MicMute::Muted => Self::Muted,
            core::MicMute::Unmuted => Self::Unmuted,
        }
    }
}

impl From<core::MicPermission> for Permission {
    fn from(permission: core::MicPermission) -> Self {
        match permission {
            core::MicPermission::Denied => Self::Denied,
            core::MicPermission::Permitted => Self::Permitted,
        }
    }
}

impl From<core::HostStatus> for Host {
    fn from(host: core::HostStatus) -> Self {
        match host {
            core::HostStatus::Disconnected => Self::Disconnected,
            core::HostStatus::Suspended => Self::Suspended,
            core::HostStatus::Connected => Self::Connected,
        }
    }
}

impl From<core::Health> for Health {
    fn from(health: core::Health) -> Self {
        match health {
            core::Health::Ready => Self::Ready,
            core::Health::Degraded => Self::Degraded,
            core::Health::Fault => Self::Fault,
        }
    }
}

impl From<core::Cause> for Cause {
    fn from(cause: core::Cause) -> Self {
        match cause {
            core::Cause::DeviceFault(core::Device::Camera) => Self::CameraFault,
            core::Cause::DeviceFault(core::Device::CamWorker) => Self::CamWorkerFault,
            core::Cause::DeviceFault(core::Device::AudioInterface) => Self::AudioInterfaceFault,
            core::Cause::DeviceFault(core::Device::Gate) => Self::GateFault,
            core::Cause::ControllerStalled => Self::ControllerStalled,
            core::Cause::Host(core::HostStatus::Disconnected) => Self::HostDisconnected,
            core::Cause::Host(core::HostStatus::Suspended) => Self::HostSuspended,
            core::Cause::Host(core::HostStatus::Connected) => Self::HostConnected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_snake_case_strings_in_json() {
        let json = serde_json::to_value(MicStatus {
            desired: Mute::Unmuted,
            effective: Permission::Denied,
            cause: Some(Cause::HostSuspended),
        })
        .unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "desired": "unmuted",
                "effective": "denied",
                "cause": "host_suspended",
            })
        );
    }
}
