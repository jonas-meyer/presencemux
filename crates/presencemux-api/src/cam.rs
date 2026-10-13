//! The Varlink API of the cam worker, `io.presencemux.Cam`. presencemuxd is its
//! only client.

use std::fmt;

use serde::{Deserialize, Serialize};
use zlink::introspect::{self, CustomType, Type};

/// The name of the Varlink interface.
pub const INTERFACE: &str = "io.presencemux.Cam";
/// The path of the Varlink socket. The systemd unit `presencemux-cam.socket`
/// creates it.
pub const SOCKET_PATH: &str = "/run/presencemux/io.presencemux.Cam";

/// A permission for the cam worker that expires at a fixed time. Times are
/// `CLOCK_MONOTONIC` in microseconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "camelCase")]
#[zlink(rename_all = "camelCase")]
pub struct Lease {
    /// The start time of the daemon run that sends the lease.
    #[serde(rename = "sessionUSec")]
    #[zlink(rename = "sessionUSec")]
    pub session_usec: i64,
    /// The number of the user command that gave the permission.
    pub epoch: i64,
    pub permission: Permission,
    /// The name of the slate in `config.toml`. Only a `slate` permission has a
    /// name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slate: Option<String>,
    /// The time at which the lease expires.
    #[serde(rename = "untilUSec")]
    #[zlink(rename = "untilUSec")]
    pub until_usec: i64,
}

/// A frame that a lease permits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum Permission {
    Blank,
    Slate,
    Live,
}

/// The state of the cam worker, in the reply to each call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
pub struct Report {
    /// The state of the cam worker and its gadget node.
    pub worker: DeviceStatus,
    /// The state of the camera.
    pub camera: DeviceStatus,
    /// The latch, if the cam worker is latched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latched: Option<Latch>,
    /// Why the cam worker rejected the lease of this call. Null after an
    /// accepted lease and in `Describe`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected: Option<Rejection>,
    /// The frame that the cam worker sends.
    pub shown: Shown,
    /// Whether the host receives a stream.
    pub host: HostStream,
    /// A message for the logs, for example about an unknown slate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The state of the cam worker or the camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum DeviceStatus {
    Missing,
    Initializing,
    Ready,
    Fault,
}

impl From<HostStream> for presencemux_core::CamStream {
    fn from(stream: HostStream) -> Self {
        match stream {
            HostStream::Idle => Self::Idle,
            HostStream::Streaming => Self::Streaming,
        }
    }
}

impl From<DeviceStatus> for presencemux_core::DeviceStatus {
    fn from(status: DeviceStatus) -> Self {
        match status {
            DeviceStatus::Missing => Self::Missing,
            DeviceStatus::Initializing => Self::Initializing,
            DeviceStatus::Ready => Self::Ready,
            DeviceStatus::Fault => Self::Fault,
        }
    }
}

/// A block of live video until a lease with a newer grant arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "camelCase")]
#[zlink(rename_all = "camelCase")]
pub struct Latch {
    /// The session of the lease at the time of the latch.
    #[serde(rename = "sessionUSec")]
    #[zlink(rename = "sessionUSec")]
    pub session_usec: i64,
    /// The epoch of the lease at the time of the latch.
    pub epoch: i64,
    pub reason: LatchReason,
}

/// The reason for a latch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum LatchReason {
    /// The lease expired before the next lease arrived.
    Expired,
    /// A live lease had no frame younger than 500 ms.
    Stalled,
    /// The cam worker started. Its first lease latches it.
    Started,
}

/// The reason for the rejection of a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum Rejection {
    /// The lease expires at or before now.
    Late,
    /// The lease expires more than 2 s after now.
    TooLong,
    /// The lease comes from an older daemon run than the last lease.
    OldSession,
    /// The lease has an older epoch than the last lease of its session.
    OldEpoch,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Late => "late",
            Self::TooLong => "too_long",
            Self::OldSession => "old_session",
            Self::OldEpoch => "old_epoch",
        })
    }
}

/// The frame that the cam worker sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum Shown {
    Blank,
    Slate,
    Live,
}

/// Whether the host receives a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum HostStream {
    Idle,
    Streaming,
}

/// The reply of `Renew`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Renewed {
    pub report: Report,
}

/// The reply of `Describe`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Description {
    /// The last accepted lease. Null if it expired or if no lease arrived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease: Option<Lease>,
    pub report: Report,
}

/// An error of `io.presencemux.Cam`.
#[derive(Clone, Debug, PartialEq, Eq, zlink::ReplyError, introspect::ReplyError)]
#[zlink(interface = "io.presencemux.Cam")]
pub enum Error {
    /// The lease has a time or an epoch out of range, or a slate name that does
    /// not match its permission.
    InvalidLease,
}

/// A client of `io.presencemux.Cam`. The file `io.presencemux.Cam.varlink`
/// documents the methods.
#[zlink::proxy("io.presencemux.Cam")]
pub trait CamProxy {
    async fn renew(&mut self, lease: &Lease) -> zlink::Result<Result<Renewed, Error>>;
    async fn describe(&mut self) -> zlink::Result<Result<Description, Error>>;
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::InvalidLease => "invalid lease",
        })
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_fields_are_camel_case_in_json() {
        let json = serde_json::to_value(Lease {
            session_usec: 1,
            epoch: 2,
            permission: Permission::Slate,
            slate: Some("away".to_owned()),
            until_usec: 3,
        })
        .unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "sessionUSec": 1,
                "epoch": 2,
                "permission": "slate",
                "slate": "away",
                "untilUSec": 3,
            })
        );
    }

    #[test]
    fn too_long_rejection_displays_as_its_wire_value() {
        let shown = Rejection::TooLong.to_string();

        assert_eq!(shown, "too_long");
    }

    #[test]
    fn rejection_values_are_snake_case_in_json() {
        let json = serde_json::to_value(Rejection::OldSession).unwrap();

        assert_eq!(json, serde_json::json!("old_session"));
    }
}
