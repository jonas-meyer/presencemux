//! The decisions of the controller.

use crate::{
    Cam, CamPermission, CamStream, Cause, DeviceStatus, Epoch, Health, HostStatus, Mic, MicMute,
    MicPermission, PlaneState, PrivacyPlane, Report, Reported,
};

/// Returns the effective cam permission. `Live` needs a ready camera and a
/// ready cam worker. A worker that exceeds its grant gets `Blank`.
pub(crate) fn cam(state: &PlaneState<Cam>) -> CamPermission {
    let Some(report) = state.fresh() else {
        return CamPermission::Blank;
    };
    if !report.worker.is_ready() || report.exceeded {
        return CamPermission::Blank;
    }
    match (state.desired(), report.camera.is_ready()) {
        (CamPermission::Live, true) => CamPermission::Live,
        (CamPermission::Live, false) | (CamPermission::Blank, _) => CamPermission::Blank,
        (CamPermission::Slate(id), _) => CamPermission::Slate(id),
    }
}

/// Returns the effective mic permission. `Permitted` needs an unmuted mic, a
/// ready audio interface and a ready gate. A gate that exceeds its grant gets
/// `Denied`.
pub(crate) fn mic(state: &PlaneState<Mic>) -> MicPermission {
    let devices_ready = state.fresh().is_some_and(|report| {
        report.interface.is_ready() && report.gate.is_ready() && !report.exceeded
    });
    match (state.desired(), devices_ready) {
        (MicMute::Unmuted, true) => MicPermission::Permitted,
        (MicMute::Unmuted, false) | (MicMute::Muted, _) => MicPermission::Denied,
    }
}

/// Returns `true` if a stream timeout with `epoch` lowers the cam plane. The
/// timeout lowers the plane if `epoch` is not older than the last command for
/// the plane and the host receives no stream.
pub(crate) fn stream_lowers(state: &PlaneState<Cam>, epoch: Epoch) -> bool {
    epoch >= state.epoch && state.stream() == CamStream::Idle
}

/// Returns the cause if the change from `before` to `after` lowers the plane
/// `P`. `granted` is the epoch of the last command for the plane. A lost owner
/// comes first, then each device in the order of [`Report::devices`], then an
/// output above its grant, then a latch.
pub(crate) fn cause<P: PrivacyPlane>(
    before: &Reported<P::Report>,
    after: &Reported<P::Report>,
    granted: Epoch,
) -> Option<Cause> {
    let after = match after {
        Reported::Fresh(after) => after,
        Reported::Lost => {
            return (!matches!(before, Reported::Lost)).then_some(Cause::OwnerLost(P::ID));
        }
        Reported::Pending => return None,
    };
    let exceeded = matches!(before, Reported::Fresh(before) if before.exceeded());
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
        .or_else(|| (after.exceeded() && !exceeded).then_some(Cause::OutputExceeded(P::ID)))
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

/// Returns `true` if a resume applies the request of `plane` again. The plane
/// must have a hold, the health of its owner must be `Ready`, and the host
/// must be connected.
pub(crate) fn resumes<P: PrivacyPlane>(plane: &PlaneState<P>, host: HostStatus) -> bool {
    plane.hold.is_some()
        && health::<P>(&plane.observed) == Health::Ready
        && host == HostStatus::Connected
}

/// Returns `true` if the host is no longer connected. A suspended host is not
/// connected.
pub(crate) fn host_lowers(before: HostStatus, after: HostStatus) -> bool {
    before == HostStatus::Connected && after != HostStatus::Connected
}

/// Returns `Fault` if a device has a fault, the owner is lost or exceeds its
/// grant, `Degraded` if a device is not ready yet or the owner has not
/// reported, and `Ready` otherwise.
pub(crate) fn health<P: PrivacyPlane>(observed: &Reported<P::Report>) -> Health {
    match observed {
        Reported::Pending => Health::Degraded,
        Reported::Lost => Health::Fault,
        Reported::Fresh(report) if report.exceeded() => Health::Fault,
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
