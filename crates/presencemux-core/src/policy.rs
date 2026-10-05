//! The decisions of the controller.

use crate::{
    Cam, CamPermission, DeviceStatus, Health, HostStatus, Mic, MicMute, MicPermission, PlaneState,
};

/// Returns the effective cam permission. Live needs a ready camera and cam
/// worker.
pub(crate) fn cam(state: &PlaneState<Cam>) -> CamPermission {
    let observed = state.observed();
    if !observed.worker.is_ready() {
        return CamPermission::Blank;
    }
    match (state.desired(), observed.camera.is_ready()) {
        (CamPermission::Live, true) => CamPermission::Live,
        (CamPermission::Live, false) | (CamPermission::Blank, _) => CamPermission::Blank,
        (CamPermission::Slate(id), _) => CamPermission::Slate(id),
    }
}

/// Returns the effective mic permission. Permitted needs an unmuted mic, a
/// ready audio interface and a ready gate.
pub(crate) fn mic(state: &PlaneState<Mic>) -> MicPermission {
    let observed = state.observed();
    let devices_ready = observed.interface.is_ready() && observed.gate.is_ready();
    match (state.desired(), devices_ready) {
        (MicMute::Unmuted, true) => MicPermission::Permitted,
        (MicMute::Unmuted, false) | (MicMute::Muted, _) => MicPermission::Denied,
    }
}

/// Returns `true` if the device was ready and is not ready now.
pub(crate) fn device_lowers(before: DeviceStatus, after: DeviceStatus) -> bool {
    before.is_ready() && !after.is_ready()
}

/// Returns `true` if the host is no longer connected. A suspend counts too,
/// because the daemon can miss the short states of a new enumeration.
pub(crate) fn host_lowers(before: HostStatus, after: HostStatus) -> bool {
    before == HostStatus::Connected && after != HostStatus::Connected
}

/// Returns `Fault` if a device has a fault, `Degraded` if a device is not ready
/// yet, and `Ready` otherwise.
pub(crate) fn health(statuses: impl IntoIterator<Item = DeviceStatus>) -> Health {
    statuses
        .into_iter()
        .map(|status| match status {
            DeviceStatus::Ready => Health::Ready,
            DeviceStatus::Missing | DeviceStatus::Initializing => Health::Degraded,
            DeviceStatus::Fault => Health::Fault,
        })
        .max()
        .unwrap_or(Health::Ready)
}
