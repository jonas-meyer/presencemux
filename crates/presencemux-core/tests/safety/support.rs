use presencemux_core::{
    CamPermission, CamReport, CamStream, Cause, Command, Controller, Device, DeviceStatus, Epoch,
    Event, HostStatus, MicMute, MicReport, Observation, OnAir, PlaneId, PlaneSummary, Preset,
    SlateId,
};
use proptest::prelude::*;
use proptest::sample::select;

pub(crate) const STATUSES: [DeviceStatus; 4] = [
    DeviceStatus::Missing,
    DeviceStatus::Initializing,
    DeviceStatus::Ready,
    DeviceStatus::Fault,
];

/// Returns a controller after the given events.
pub(crate) fn after(events: impl IntoIterator<Item = Event>) -> Controller {
    let (mut controller, _) = Controller::new();
    for event in events {
        let _ = controller.handle_input(event);
    }
    controller
}

/// Returns the cause of a plane after automation lowers it. A plane that was
/// already off keeps its cause.
pub(crate) fn cause_after_lowering(before: PlaneSummary, cause: Cause) -> Option<Cause> {
    if before.requested > OnAir::Off {
        Some(cause)
    } else {
        before.cause
    }
}

pub(crate) fn device() -> impl Strategy<Value = Device> {
    select(&Device::ALL[..])
}

/// Returns a report of the owner of `device`, with `status` for `device`,
/// every other device ready, no latch and an idle stream.
pub(crate) fn report_with(device: Device, status: DeviceStatus) -> Observation {
    let status_of = |other| {
        if other == device {
            status
        } else {
            DeviceStatus::Ready
        }
    };
    match device.plane() {
        PlaneId::Cam => Observation::Cam(CamReport {
            worker: status_of(Device::CamWorker),
            camera: status_of(Device::Camera),
            stream: CamStream::Idle,
            latched: None,
        }),
        PlaneId::Mic => Observation::Mic(MicReport {
            gate: status_of(Device::Gate),
            interface: status_of(Device::AudioInterface),
            latched: None,
        }),
    }
}

/// Returns a report of the owner of the devices of `plane`, with every device
/// ready, no latch and an idle stream.
pub(crate) fn ready_report(plane: PlaneId) -> Observation {
    match plane {
        PlaneId::Cam => Observation::Cam(CamReport {
            worker: DeviceStatus::Ready,
            camera: DeviceStatus::Ready,
            stream: CamStream::Idle,
            latched: None,
        }),
        PlaneId::Mic => Observation::Mic(MicReport {
            gate: DeviceStatus::Ready,
            interface: DeviceStatus::Ready,
            latched: None,
        }),
    }
}

/// Returns a report of the owner of the devices of `plane`, with every device
/// ready, an idle stream and a latch at `epoch`.
pub(crate) fn latched_report(plane: PlaneId, epoch: Epoch) -> Observation {
    let latched = Some(epoch);
    match plane {
        PlaneId::Cam => Observation::Cam(CamReport {
            worker: DeviceStatus::Ready,
            camera: DeviceStatus::Ready,
            stream: CamStream::Idle,
            latched,
        }),
        PlaneId::Mic => Observation::Mic(MicReport {
            gate: DeviceStatus::Ready,
            interface: DeviceStatus::Ready,
            latched,
        }),
    }
}

/// Returns a report of the cam worker with ready devices, no latch and
/// `stream`.
pub(crate) fn stream_report(stream: CamStream) -> Observation {
    Observation::Cam(CamReport {
        worker: DeviceStatus::Ready,
        camera: DeviceStatus::Ready,
        stream,
        latched: None,
    })
}

/// Returns a latch for one report in ten, and no latch for the others.
fn latched() -> impl Strategy<Value = Option<Epoch>> {
    proptest::option::weighted(0.1, epoch())
}

pub(crate) fn cam_report() -> impl Strategy<Value = CamReport> {
    (device_status(), device_status(), cam_stream(), latched()).prop_map(
        |(worker, camera, stream, latched)| CamReport {
            worker,
            camera,
            stream,
            latched,
        },
    )
}

pub(crate) fn mic_report() -> impl Strategy<Value = MicReport> {
    (device_status(), device_status(), latched()).prop_map(|(gate, interface, latched)| MicReport {
        gate,
        interface,
        latched,
    })
}

pub(crate) fn device_status() -> impl Strategy<Value = DeviceStatus> {
    select(&STATUSES[..])
}

pub(crate) fn not_ready() -> impl Strategy<Value = DeviceStatus> {
    let statuses: Vec<_> = STATUSES.into_iter().filter(|s| !s.is_ready()).collect();
    select(statuses)
}

pub(crate) fn not_fault() -> impl Strategy<Value = DeviceStatus> {
    let statuses: Vec<_> = STATUSES
        .into_iter()
        .filter(|&s| s != DeviceStatus::Fault)
        .collect();
    select(statuses)
}

pub(crate) fn host_status() -> impl Strategy<Value = HostStatus> {
    select(
        &[
            HostStatus::Disconnected,
            HostStatus::Suspended,
            HostStatus::Connected,
        ][..],
    )
}

pub(crate) fn not_connected() -> impl Strategy<Value = HostStatus> {
    select(&[HostStatus::Disconnected, HostStatus::Suspended][..])
}

pub(crate) fn plane() -> impl Strategy<Value = PlaneId> {
    select(&PlaneId::ALL[..])
}

fn epoch() -> impl Strategy<Value = Epoch> {
    (0..24_u32).prop_map(Epoch::new)
}

fn cam() -> impl Strategy<Value = CamPermission> {
    prop_oneof![
        Just(CamPermission::Blank),
        (0u16..3).prop_map(|id| CamPermission::Slate(SlateId(id))),
        Just(CamPermission::Live),
    ]
}

fn mic() -> impl Strategy<Value = MicMute> {
    prop_oneof![Just(MicMute::Muted), Just(MicMute::Unmuted)]
}

/// Any preset, also one that no configuration has.
pub(crate) fn preset() -> impl Strategy<Value = Preset> {
    (proptest::option::of(cam()), proptest::option::of(mic()))
        .prop_map(|(cam, mic)| Preset { cam, mic })
}

/// A preset that sets the plane.
pub(crate) fn preset_setting(plane: PlaneId) -> BoxedStrategy<Preset> {
    match plane {
        PlaneId::Cam => (cam(), proptest::option::of(mic()))
            .prop_map(|(cam, mic)| Preset {
                cam: Some(cam),
                mic,
            })
            .boxed(),
        PlaneId::Mic => (proptest::option::of(cam()), mic())
            .prop_map(|(cam, mic)| Preset {
                cam,
                mic: Some(mic),
            })
            .boxed(),
    }
}

/// A preset that does not set the plane.
pub(crate) fn preset_without(plane: PlaneId) -> BoxedStrategy<Preset> {
    match plane {
        PlaneId::Cam => proptest::option::of(mic())
            .prop_map(|mic| Preset { cam: None, mic })
            .boxed(),
        PlaneId::Mic => proptest::option::of(cam())
            .prop_map(|cam| Preset { cam, mic: None })
            .boxed(),
    }
}

pub(crate) fn observation() -> impl Strategy<Value = Observation> {
    prop_oneof![
        2 => cam_report().prop_map(Observation::Cam),
        2 => mic_report().prop_map(Observation::Mic),
        1 => plane().prop_map(Observation::Lost),
        1 => (plane(), epoch()).prop_map(|(p, e)| Observation::LeaseExpired(p, e)),
        1 => host_status().prop_map(Observation::Host),
        1 => epoch().prop_map(Observation::StreamTimeout),
    ]
}

pub(crate) fn cam_stream() -> impl Strategy<Value = CamStream> {
    select(&[CamStream::Idle, CamStream::Streaming][..])
}

pub(crate) fn event() -> impl Strategy<Value = Event> {
    prop_oneof![
        1 => preset().prop_map(|p| Command::Apply(p).into()),
        3 => observation().prop_map(Event::from),
    ]
}

pub(crate) fn events() -> impl Strategy<Value = Vec<Event>> {
    prop::collection::vec(event(), 0..64)
}

pub(crate) fn observations() -> impl Strategy<Value = Vec<Observation>> {
    prop::collection::vec(observation(), 0..32)
}
