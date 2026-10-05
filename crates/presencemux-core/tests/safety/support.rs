use presencemux_core::{
    CamPermission, Cause, Command, Controller, Device, DeviceStatus, Event, HostStatus, MicMute,
    Observation, OnAir, PlaneId, PlaneSummary, Preset, SlateId,
};
use proptest::prelude::*;
use proptest::sample::select;

pub const STATUSES: [DeviceStatus; 4] = [
    DeviceStatus::Missing,
    DeviceStatus::Initializing,
    DeviceStatus::Ready,
    DeviceStatus::Fault,
];

/// Returns a controller after the given events.
pub fn after(events: impl IntoIterator<Item = Event>) -> Controller {
    let (mut controller, _) = Controller::new();
    for event in events {
        let _ = controller.handle_input(event);
    }
    controller
}

/// Returns the cause of a plane after automation lowers it. A plane that was
/// already off keeps its cause.
pub fn cause_after_lowering(before: PlaneSummary, cause: Cause) -> Option<Cause> {
    if before.requested > OnAir::Off {
        Some(cause)
    } else {
        before.cause
    }
}

pub fn device() -> impl Strategy<Value = Device> {
    select(&Device::ALL[..])
}

pub fn device_status() -> impl Strategy<Value = DeviceStatus> {
    select(&STATUSES[..])
}

pub fn not_ready() -> impl Strategy<Value = DeviceStatus> {
    let statuses: Vec<_> = STATUSES.into_iter().filter(|s| !s.is_ready()).collect();
    select(statuses)
}

pub fn host_status() -> impl Strategy<Value = HostStatus> {
    select(
        &[
            HostStatus::Disconnected,
            HostStatus::Suspended,
            HostStatus::Connected,
        ][..],
    )
}

pub fn not_connected() -> impl Strategy<Value = HostStatus> {
    select(&[HostStatus::Disconnected, HostStatus::Suspended][..])
}

pub fn plane() -> impl Strategy<Value = PlaneId> {
    select(&PlaneId::ALL[..])
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

/// Any preset, not only the configured ones.
pub fn preset() -> impl Strategy<Value = Preset> {
    (proptest::option::of(cam()), proptest::option::of(mic()))
        .prop_map(|(cam, mic)| Preset { cam, mic })
}

pub fn observation() -> impl Strategy<Value = Observation> {
    prop_oneof![
        4 => (device(), device_status()).prop_map(|(d, s)| Observation::Device(d, s)),
        1 => plane().prop_map(Observation::LeaseExpired),
        1 => host_status().prop_map(Observation::Host),
    ]
}

pub fn event() -> impl Strategy<Value = Event> {
    prop_oneof![
        1 => preset().prop_map(|p| Command::Apply(p).into()),
        3 => observation().prop_map(Event::from),
    ]
}

pub fn events() -> impl Strategy<Value = Vec<Event>> {
    prop::collection::vec(event(), 0..64)
}

pub fn observations() -> impl Strategy<Value = Vec<Observation>> {
    prop::collection::vec(observation(), 0..32)
}
