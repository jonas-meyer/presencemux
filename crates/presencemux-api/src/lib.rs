//! The Varlink APIs of presencemuxd and its workers. The module [`cam`] holds
//! the API of the cam worker. The module [`audio`] holds the API of the audio
//! worker.

pub mod audio;
pub mod cam;

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
/// The actual cam value for a slate without a name in the configuration.
pub const CAM_SLATE: &str = "slate";
/// The actual cam value while the host receives no stream.
pub const CAM_NOTHING: &str = "nothing";

/// The state of the controller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Status {
    pub cam: CamStatus,
    pub mic: MicStatus,
    /// The workers that own the devices, with the conditions of the devices.
    pub owners: Vec<OwnerStatus>,
    pub host: Host,
    pub health: Health,
    /// The last preset that the user applied. Null after the user changes a
    /// single plane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
}

/// The state of the cam plane. Its values are `blank`, `live` or a slate name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "camelCase")]
#[zlink(rename_all = "camelCase")]
pub struct CamStatus {
    /// The value that the user asked for.
    pub requested: String,
    /// The value that the controller grants the cam worker.
    pub granted: String,
    /// The frames that the host gets, as the cam worker reports them. `nothing`
    /// means no stream. Null without a fresh report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    /// Whether the cam worker applies the grant of the controller.
    pub in_effect: bool,
    /// Whether the cam worker blocks live video after a latch.
    pub latched: bool,
    /// Why automation holds the plane at blank. Null after the user sets the
    /// plane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold: Option<Cause>,
    /// Whether the host receives the stream of the cam worker.
    pub stream: CamStream,
}

/// Whether the host receives the stream of the cam worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum CamStream {
    Idle,
    Streaming,
}

/// The state of the mic plane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "camelCase")]
#[zlink(rename_all = "camelCase")]
pub struct MicStatus {
    /// The value that the user asked for.
    pub requested: Mute,
    /// Whether the controller lets the gate pass audio.
    pub granted: Permission,
    /// The audio that the host gets, as the audio worker reports it. Null
    /// without a fresh report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<MicOutput>,
    /// Whether the audio worker applies the grant of the controller.
    pub in_effect: bool,
    /// Whether the gate blocks the audio after a latch.
    pub latched: bool,
    /// Why automation holds the plane at muted. Null after the user sets the
    /// plane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold: Option<Cause>,
}

/// The audio that the host gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum MicOutput {
    /// The gate writes digital silence.
    Silent,
    /// The gate passes audio with a falling gain, after a lease that closes
    /// it.
    Closing,
    /// The gate passes audio while it opens or is open.
    Passing,
}

/// The state of a worker that owns devices.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "camelCase")]
#[zlink(rename_all = "camelCase")]
pub struct OwnerStatus {
    pub owner: Owner,
    pub report: ReportState,
    /// The time of the last change of `report`, in `CLOCK_MONOTONIC`
    /// microseconds.
    #[serde(rename = "sinceUSec")]
    #[zlink(rename = "sinceUSec")]
    pub since_usec: i64,
    pub devices: Vec<DeviceCondition>,
    /// The rejection or the detail in the last report of the worker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A worker that owns devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum Owner {
    CamWorker,
    AudioWorker,
}

/// Whether the controller has a fresh report of a worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum ReportState {
    /// No report since the start of the daemon.
    Pending,
    Fresh,
    /// The worker does not reply, or sends no more reports.
    Lost,
}

/// The condition of a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "camelCase")]
#[zlink(rename_all = "camelCase")]
pub struct DeviceCondition {
    pub device: Device,
    pub status: DeviceStatus,
    /// The time of the last change of `status`, in `CLOCK_MONOTONIC`
    /// microseconds.
    #[serde(rename = "sinceUSec")]
    #[zlink(rename = "sinceUSec")]
    pub since_usec: i64,
}

/// A device that a worker owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum Device {
    Camera,
    CamWorker,
    AudioInterface,
    Gate,
}

/// The status of a device. `unknown` means that the controller has no fresh
/// report of its worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum DeviceStatus {
    Unknown,
    Missing,
    Initializing,
    Ready,
    Fault,
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
/// not ready yet, or that a worker has not reported yet.
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
    /// The daemon has no contact with the cam worker.
    CamWorkerLost,
    /// The cam worker sends a frame above the grant under which it queued the
    /// frame.
    CamOutputExceeded,
    AudioInterfaceFault,
    GateFault,
    /// The daemon has no contact with the audio worker.
    AudioWorkerLost,
    /// The gate passes audio without a grant that permits it.
    MicOutputExceeded,
    ControllerStalled,
    HostDisconnected,
    HostSuspended,
    HostConnected,
    StreamStopped,
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

impl fmt::Display for CamStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Idle => "idle",
            Self::Streaming => "streaming",
        })
    }
}

impl fmt::Display for MicOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Silent => "silent",
            Self::Closing => "closing",
            Self::Passing => "passing",
        })
    }
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::CamWorker => "cam_worker",
            Self::AudioWorker => "audio_worker",
        })
    }
}

impl fmt::Display for ReportState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Pending => "pending",
            Self::Fresh => "fresh",
            Self::Lost => "lost",
        })
    }
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Camera => "camera",
            Self::CamWorker => "cam_worker",
            Self::AudioInterface => "audio_interface",
            Self::Gate => "gate",
        })
    }
}

impl fmt::Display for DeviceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Unknown => "unknown",
            Self::Missing => "missing",
            Self::Initializing => "initializing",
            Self::Ready => "ready",
            Self::Fault => "fault",
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
            Self::CamWorkerLost => "cam_worker_lost",
            Self::CamOutputExceeded => "cam_output_exceeded",
            Self::AudioInterfaceFault => "audio_interface_fault",
            Self::GateFault => "gate_fault",
            Self::AudioWorkerLost => "audio_worker_lost",
            Self::MicOutputExceeded => "mic_output_exceeded",
            Self::ControllerStalled => "controller_stalled",
            Self::HostDisconnected => "host_disconnected",
            Self::HostSuspended => "host_suspended",
            Self::HostConnected => "host_connected",
            Self::StreamStopped => "stream_stopped",
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

impl From<controller::MicOutput> for MicOutput {
    fn from(output: controller::MicOutput) -> Self {
        match output {
            controller::MicOutput::Silent => Self::Silent,
            controller::MicOutput::Closing => Self::Closing,
            controller::MicOutput::Passing => Self::Passing,
        }
    }
}

impl From<controller::Device> for Device {
    fn from(device: controller::Device) -> Self {
        match device {
            controller::Device::Camera => Self::Camera,
            controller::Device::CamWorker => Self::CamWorker,
            controller::Device::AudioInterface => Self::AudioInterface,
            controller::Device::Gate => Self::Gate,
        }
    }
}

impl From<controller::DeviceStatus> for DeviceStatus {
    fn from(status: controller::DeviceStatus) -> Self {
        match status {
            controller::DeviceStatus::Missing => Self::Missing,
            controller::DeviceStatus::Initializing => Self::Initializing,
            controller::DeviceStatus::Ready => Self::Ready,
            controller::DeviceStatus::Fault => Self::Fault,
        }
    }
}

impl From<controller::CamStream> for CamStream {
    fn from(stream: controller::CamStream) -> Self {
        match stream {
            controller::CamStream::Idle => Self::Idle,
            controller::CamStream::Streaming => Self::Streaming,
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
            controller::Cause::OwnerLost(controller::PlaneId::Cam) => Self::CamWorkerLost,
            controller::Cause::OwnerLost(controller::PlaneId::Mic) => Self::AudioWorkerLost,
            controller::Cause::OutputExceeded(controller::PlaneId::Cam) => Self::CamOutputExceeded,
            controller::Cause::OutputExceeded(controller::PlaneId::Mic) => Self::MicOutputExceeded,
            controller::Cause::ControllerStalled => Self::ControllerStalled,
            controller::Cause::Host(controller::HostStatus::Disconnected) => Self::HostDisconnected,
            controller::Cause::Host(controller::HostStatus::Suspended) => Self::HostSuspended,
            controller::Cause::Host(controller::HostStatus::Connected) => Self::HostConnected,
            controller::Cause::StreamStopped => Self::StreamStopped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_snake_case_strings_in_json() {
        let json = serde_json::to_value(MicStatus {
            requested: Mute::Unmuted,
            granted: Permission::Denied,
            actual: Some(MicOutput::Silent),
            in_effect: true,
            latched: false,
            hold: Some(Cause::HostSuspended),
        })
        .unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "requested": "unmuted",
                "granted": "denied",
                "actual": "silent",
                "inEffect": true,
                "latched": false,
                "hold": "host_suspended",
            })
        );
    }

    #[test]
    fn device_condition_time_is_since_usec_in_json() {
        let json = serde_json::to_value(DeviceCondition {
            device: Device::AudioInterface,
            status: DeviceStatus::Unknown,
            since_usec: 7,
        })
        .unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "device": "audio_interface",
                "status": "unknown",
                "sinceUSec": 7,
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
        check(&[CamStream::Idle, CamStream::Streaming]);
        check(&[MicOutput::Silent, MicOutput::Closing, MicOutput::Passing]);
        check(&[Owner::CamWorker, Owner::AudioWorker]);
        check(&[ReportState::Pending, ReportState::Fresh, ReportState::Lost]);
        check(&[
            Device::Camera,
            Device::CamWorker,
            Device::AudioInterface,
            Device::Gate,
        ]);
        check(&[
            DeviceStatus::Unknown,
            DeviceStatus::Missing,
            DeviceStatus::Initializing,
            DeviceStatus::Ready,
            DeviceStatus::Fault,
        ]);
        check(&[
            Cause::CameraFault,
            Cause::CamWorkerFault,
            Cause::CamWorkerLost,
            Cause::CamOutputExceeded,
            Cause::AudioInterfaceFault,
            Cause::GateFault,
            Cause::AudioWorkerLost,
            Cause::MicOutputExceeded,
            Cause::ControllerStalled,
            Cause::HostDisconnected,
            Cause::HostSuspended,
            Cause::HostConnected,
            Cause::StreamStopped,
        ]);
    }
}
