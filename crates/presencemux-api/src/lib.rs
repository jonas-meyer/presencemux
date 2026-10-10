//! The Varlink API of presencemuxd, shared by the daemon and its clients.

use std::fmt;

use futures_core::Stream;
use presencemux_core as controller;
use serde::{Deserialize, Serialize};
use zlink::introspect::{self, CustomType, Type};

/// The name of the Varlink interface.
pub const INTERFACE: &str = "io.presencemux.Controller";
/// The path of the Varlink socket. The systemd unit `presencemuxd.socket`
/// creates it.
pub const SOCKET_PATH: &str = "/run/presencemux/io.presencemux.Controller";
/// The name of the built-in preset that sets every plane to its safe value.
pub const PRESET_PRIVACY: &str = "privacy";
/// The cam value for a blank image.
pub const CAM_BLANK: &str = "blank";
/// The cam value for the live camera.
pub const CAM_LIVE: &str = "live";

/// The state of the controller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Status {
    pub cam: CamStatus,
    pub mic: MicStatus,
    pub host: Host,
    pub health: Health,
    /// The last preset that the user applied. Null after the user changes a
    /// single plane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
}

/// The state of the cam plane. Its values are `blank`, `live` or a slate name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
pub struct CamStatus {
    /// The value that the user asked for.
    pub desired: String,
    /// The value that the controller grants the cam worker.
    pub effective: String,
    /// Why automation lowered the plane. Null after the user sets the plane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
}

/// The state of the mic plane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
pub struct MicStatus {
    /// The value that the user asked for.
    pub desired: Mute,
    /// Whether the controller lets the gate pass audio.
    pub effective: Permission,
    /// Why automation lowered the plane. Null after the user sets the plane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
}

/// Whether the user wants the mic muted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum Mute {
    Muted,
    Unmuted,
}

/// Whether the gate may pass audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum Permission {
    Denied,
    Permitted,
}

/// The USB connection to the computer on the other side of the gadget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum Host {
    Disconnected,
    Suspended,
    Connected,
}

/// The health of the devices. `degraded` means that a device is missing or
/// not ready yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum Health {
    Ready,
    Degraded,
    Fault,
}

/// The reason why automation lowered a plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
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

/// The names of the configured presets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Presets {
    pub presets: Vec<String>,
}

/// An error of `io.presencemux.Controller`.
#[derive(Clone, Debug, PartialEq, Eq, zlink::ReplyError, introspect::ReplyError)]
#[zlink(interface = "io.presencemux.Controller")]
pub enum Error {
    /// The preset is not `privacy` or a configured preset.
    NoSuchPreset,
    /// The cam value is not `blank`, `live` or a configured slate.
    NoSuchSlate,
    /// The daemon has too many requests to handle. Try again.
    TooManyRequests,
    /// The daemon cannot handle requests at this time.
    NotAvailable,
}

/// A client of `io.presencemux.Controller`. The file
/// `io.presencemux.Controller.varlink` documents the methods.
#[zlink::proxy("io.presencemux.Controller")]
pub trait ControllerProxy {
    async fn describe(&mut self) -> zlink::Result<Result<Status, Error>>;
    #[zlink(more)]
    async fn subscribe_status(
        &mut self,
    ) -> zlink::Result<impl Stream<Item = zlink::Result<Result<Status, Error>>>>;
    async fn apply_preset(&mut self, name: &str) -> zlink::Result<Result<(), Error>>;
    async fn set_cam(&mut self, cam: &str) -> zlink::Result<Result<(), Error>>;
    async fn set_mic_mute(&mut self, muted: bool) -> zlink::Result<Result<(), Error>>;
    async fn list_presets(&mut self) -> zlink::Result<Result<Presets, Error>>;
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::NoSuchPreset => "no such preset",
            Self::NoSuchSlate => "no such slate",
            Self::TooManyRequests => "too many requests",
            Self::NotAvailable => "the daemon is not available",
        })
    }
}

impl std::error::Error for Error {}

impl fmt::Display for Mute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Muted => "muted",
            Self::Unmuted => "unmuted",
        })
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Denied => "denied",
            Self::Permitted => "permitted",
        })
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Disconnected => "disconnected",
            Self::Suspended => "suspended",
            Self::Connected => "connected",
        })
    }
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Fault => "fault",
        })
    }
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::CameraFault => "camera_fault",
            Self::CamWorkerFault => "cam_worker_fault",
            Self::AudioInterfaceFault => "audio_interface_fault",
            Self::GateFault => "gate_fault",
            Self::ControllerStalled => "controller_stalled",
            Self::HostDisconnected => "host_disconnected",
            Self::HostSuspended => "host_suspended",
            Self::HostConnected => "host_connected",
        })
    }
}

impl From<controller::MicMute> for Mute {
    fn from(mute: controller::MicMute) -> Self {
        match mute {
            controller::MicMute::Muted => Self::Muted,
            controller::MicMute::Unmuted => Self::Unmuted,
        }
    }
}

impl From<controller::MicPermission> for Permission {
    fn from(permission: controller::MicPermission) -> Self {
        match permission {
            controller::MicPermission::Denied => Self::Denied,
            controller::MicPermission::Permitted => Self::Permitted,
        }
    }
}

impl From<controller::HostStatus> for Host {
    fn from(host: controller::HostStatus) -> Self {
        match host {
            controller::HostStatus::Disconnected => Self::Disconnected,
            controller::HostStatus::Suspended => Self::Suspended,
            controller::HostStatus::Connected => Self::Connected,
        }
    }
}

impl From<controller::Health> for Health {
    fn from(health: controller::Health) -> Self {
        match health {
            controller::Health::Ready => Self::Ready,
            controller::Health::Degraded => Self::Degraded,
            controller::Health::Fault => Self::Fault,
        }
    }
}

impl From<controller::Cause> for Cause {
    fn from(cause: controller::Cause) -> Self {
        match cause {
            controller::Cause::DeviceFault(controller::Device::Camera) => Self::CameraFault,
            controller::Cause::DeviceFault(controller::Device::CamWorker) => Self::CamWorkerFault,
            controller::Cause::DeviceFault(controller::Device::AudioInterface) => {
                Self::AudioInterfaceFault
            }
            controller::Cause::DeviceFault(controller::Device::Gate) => Self::GateFault,
            controller::Cause::ControllerStalled => Self::ControllerStalled,
            controller::Cause::Host(controller::HostStatus::Disconnected) => Self::HostDisconnected,
            controller::Cause::Host(controller::HostStatus::Suspended) => Self::HostSuspended,
            controller::Cause::Host(controller::HostStatus::Connected) => Self::HostConnected,
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

    #[test]
    fn display_matches_the_wire_value() {
        fn check<T: fmt::Display + Serialize>(values: &[T]) {
            for value in values {
                assert_eq!(
                    serde_json::to_value(value).unwrap(),
                    serde_json::Value::String(value.to_string())
                );
            }
        }

        check(&[Mute::Muted, Mute::Unmuted]);
        check(&[Permission::Denied, Permission::Permitted]);
        check(&[Host::Disconnected, Host::Suspended, Host::Connected]);
        check(&[Health::Ready, Health::Degraded, Health::Fault]);
        check(&[
            Cause::CameraFault,
            Cause::CamWorkerFault,
            Cause::AudioInterfaceFault,
            Cause::GateFault,
            Cause::ControllerStalled,
            Cause::HostDisconnected,
            Cause::HostSuspended,
            Cause::HostConnected,
        ]);
    }
}
