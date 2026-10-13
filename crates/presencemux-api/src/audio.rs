//! The Varlink API of the audio worker, `io.presencemux.Audio`. presencemuxd
//! is its only client.

use std::fmt;

use serde::{Deserialize, Serialize};
use zlink::introspect::{self, CustomType, Type};

/// The name of the Varlink interface.
pub const INTERFACE: &str = "io.presencemux.Audio";
/// The path of the Varlink socket. The systemd unit `presencemux-audio.socket`
/// creates it.
pub const SOCKET_PATH: &str = "/run/presencemux/io.presencemux.Audio";

/// A permission for the mic gate that expires at a fixed time. Times are
/// `CLOCK_MONOTONIC` in microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
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
    /// The time at which the lease expires.
    #[serde(rename = "untilUSec")]
    #[zlink(rename = "untilUSec")]
    pub until_usec: i64,
}

/// Whether the gate can pass audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum Permission {
    Denied,
    Permitted,
}

/// The state of the audio worker, in the reply to each call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
pub struct Report {
    /// The state of the gate and its PipeWire streams.
    pub gate: DeviceStatus,
    /// The state of the USB audio interface.
    pub interface: DeviceStatus,
    /// The latch, if the gate is latched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latched: Option<Latch>,
    /// Why the gate rejected the lease of this call. Null after an accepted
    /// lease and in `Describe`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected: Option<Rejection>,
    /// The audio that the gate sends to the gadget, from the last cycle of
    /// its real-time thread.
    pub output: Output,
    /// The grant of the lease under which the gate wrote the cycle of
    /// `output`. Null for a cycle without a valid lease.
    #[serde(rename = "outputGrant", skip_serializing_if = "Option::is_none")]
    #[zlink(rename = "outputGrant")]
    pub output_grant: Option<Grant>,
    /// The grant of the lease that the gate applies. Null without a valid
    /// lease.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<Grant>,
}

/// The audio that the gate sends to the gadget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum Output {
    /// Digital silence, or no cycle in the last 100 ms.
    Silent,
    /// Audio with a falling gain, after a lease that closes the gate.
    Closing,
    /// Audio with a gain above 0 while the gate opens or is open.
    Passing,
}

/// The grant of a lease: the session, the epoch and the permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "camelCase")]
#[zlink(rename_all = "camelCase")]
pub struct Grant {
    /// The start time of the daemon run that sent the lease.
    #[serde(rename = "sessionUSec")]
    #[zlink(rename = "sessionUSec")]
    pub session_usec: i64,
    pub epoch: i64,
    pub permission: Permission,
}

/// The state of the gate or the audio interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CustomType)]
#[serde(rename_all = "snake_case")]
#[zlink(rename_all = "snake_case")]
pub enum DeviceStatus {
    Missing,
    Initializing,
    Ready,
    Fault,
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

/// A block of the mic until a lease with a newer grant arrives.
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
    /// The audio worker started. The first lease latches the gate.
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

/// An error of `io.presencemux.Audio`.
#[derive(Clone, Debug, PartialEq, Eq, zlink::ReplyError, introspect::ReplyError)]
#[zlink(interface = "io.presencemux.Audio")]
pub enum Error {
    /// The lease has a time or an epoch out of range.
    InvalidLease,
}

/// A client of `io.presencemux.Audio`. The file `io.presencemux.Audio.varlink`
/// documents the methods.
#[zlink::proxy("io.presencemux.Audio")]
pub trait AudioProxy {
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
            permission: Permission::Permitted,
            until_usec: 3,
        })
        .unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "sessionUSec": 1,
                "epoch": 2,
                "permission": "permitted",
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
