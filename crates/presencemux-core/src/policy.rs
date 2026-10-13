//! The decisions of the controller.

use crate::{
    Cam, CamPermission, CamStream, Cause, DeviceStatus, Epoch, Health, HostStatus, Mic, MicMute,
    MicPermission, MicReport, PlaneId, PlaneState, Report, Reported,
};

/// Returns the effective cam permission. Live needs a ready camera and cam
/// worker.
pub(crate) fn cam(state: &PlaneState<Cam>) -> CamPermission {
    let Reported::Fresh(report) = state.observed() else {
        return CamPermission::Blank;
    };
    if !report.worker.is_ready() {
        return CamPermission::Blank;
    }
    match (state.desired(), report.camera.is_ready()) {
        (CamPermission::Live, true) => CamPermission::Live,
        (CamPermission::Live, false) | (CamPermission::Blank, _) => CamPermission::Blank,
        (CamPermission::Slate(id), _) => CamPermission::Slate(id),
    }
}

/// Returns the effective mic permission. Permitted needs an unmuted mic, a
/// ready audio interface and a ready gate.
pub(crate) fn mic(state: &PlaneState<Mic>) -> MicPermission {
    let devices_ready = match state.observed() {
        Reported::Fresh(MicReport {
            gate, interface, ..
        }) => interface.is_ready() && gate.is_ready(),
        Reported::Pending | Reported::Lost => false,
    };
    match (state.desired(), devices_ready) {
        (MicMute::Unmuted, true) => MicPermission::Permitted,
        (MicMute::Unmuted, false) | (MicMute::Muted, _) => MicPermission::Denied,
    }
}

/// Returns `true` if a stream timeout with `epoch` lowers the cam plane: the
/// epoch is not older than the last command for the cam plane, and the host
/// receives no stream.
pub(crate) fn stream_lowers(state: &PlaneState<Cam>, epoch: Epoch) -> bool {
    epoch >= state.epoch && state.stream() == CamStream::Idle
}

/// Returns the cause if the change from `before` to `after` lowers `plane`.
/// `granted` is the epoch of the last command for `plane`. A lost owner comes
/// first, then each device in the order of [`Report::devices`], then a latch.
pub(crate) fn cause<R: Report>(
    plane: PlaneId,
    before: &Reported<R>,
    after: &Reported<R>,
    granted: Epoch,
) -> Option<Cause> {
    let after = match after {
        Reported::Fresh(after) => after,
        Reported::Lost => {
            return (!matches!(before, Reported::Lost)).then_some(Cause::OwnerLost(plane));
        }
        Reported::Pending => return None,
    };
    let before = match before {
        Reported::Fresh(before) => before.devices().map(|(_, status)| status),
        Reported::Pending | Reported::Lost => [DeviceStatus::Missing; 2],
    };
    after
        .devices()
        .into_iter()
        .zip(before)
        .find(|&((_, status), before)| device_lowers(before, status))
        .map(|((device, _), _)| Cause::DeviceFault(device))
        .or_else(|| {
            after
                .latched()
                .filter(|&epoch| epoch >= granted)
                .map(|_| Cause::ControllerStalled)
        })
}

/// Returns `true` if the device was ready and is not ready now, or if the
/// device has a new fault.
fn device_lowers(before: DeviceStatus, after: DeviceStatus) -> bool {
    (before.is_ready() && !after.is_ready())
        || (after == DeviceStatus::Fault && before != DeviceStatus::Fault)
}

/// Returns `true` if the host is no longer connected. A suspend counts too.
pub(crate) fn host_lowers(before: HostStatus, after: HostStatus) -> bool {
    before == HostStatus::Connected && after != HostStatus::Connected
}

/// Returns `Fault` if a device has a fault or the owner is lost, `Degraded` if
/// a device is not ready yet or the owner has not reported, and `Ready`
/// otherwise.
pub(crate) fn health<R: Report>(observed: &Reported<R>) -> Health {
    match observed {
        Reported::Pending => Health::Degraded,
        Reported::Lost => Health::Fault,
        Reported::Fresh(report) => report
            .devices()
            .into_iter()
            .map(|(_, status)| match status {
                DeviceStatus::Ready => Health::Ready,
                DeviceStatus::Missing | DeviceStatus::Initializing => Health::Degraded,
                DeviceStatus::Fault => Health::Fault,
            })
            .max()
            .unwrap_or(Health::Ready),
    }
}
