//! The status of the controller in API types.

use presencemux_api::{
    self as api, CamStatus, Cause, DeviceCondition, MicStatus, OwnerStatus, ReportState, Status,
};
use presencemux_core::{CamOutput, CamPermission, Controller, Plane, PlaneState, Report, Reported};

use crate::config::{PresetRef, Slates};
use crate::event_loop::{Monotonic, Owner};

/// The conditions of the owners and their devices, with the time of each
/// change, and the last message of each owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Conditions {
    cam: OwnerStatus,
    audio: OwnerStatus,
}

impl Conditions {
    /// Creates the conditions at boot. Each owner is pending since `now`, and
    /// each device is unknown.
    pub(crate) fn new(now: Monotonic) -> Self {
        Self {
            cam: pending(
                api::Owner::CamWorker,
                [api::Device::CamWorker, api::Device::Camera],
                now,
            ),
            audio: pending(
                api::Owner::AudioWorker,
                [api::Device::Gate, api::Device::AudioInterface],
                now,
            ),
        }
    }

    /// Updates the report state and the device statuses from `controller` at
    /// `now`. Keeps the time of each value that did not change.
    pub(crate) fn update(&mut self, controller: &Controller, now: Monotonic) {
        update(&mut self.cam, controller.cam(), now);
        update(&mut self.audio, controller.mic(), now);
    }

    /// Sets the message of `owner`. Returns whether the message changed.
    pub(crate) fn set_message(&mut self, owner: Owner, message: Option<String>) -> bool {
        let status = match owner {
            Owner::CamWorker => &mut self.cam,
            Owner::AudioWorker => &mut self.audio,
        };
        let changed = status.message != message;
        status.message = message;
        changed
    }
}

/// Returns the status of a pending owner, with `now` as the time of each value.
fn pending(owner: api::Owner, devices: [api::Device; 2], now: Monotonic) -> OwnerStatus {
    let since_usec = now.as_micros();
    OwnerStatus {
        owner,
        report: ReportState::Pending,
        since_usec,
        devices: devices
            .map(|device| DeviceCondition {
                device,
                status: api::DeviceStatus::Unknown,
                since_usec,
            })
            .into(),
        message: None,
    }
}

/// Updates `status` from the reports of the owner of the devices of `plane`
/// at `now`.
fn update<P: Plane>(status: &mut OwnerStatus, plane: &PlaneState<P>, now: Monotonic)
where
    P::Report: Report,
{
    let since_usec = now.as_micros();
    let (report, devices) = match plane.observed() {
        Reported::Pending => (ReportState::Pending, None),
        Reported::Fresh(report) => (ReportState::Fresh, Some(report.devices())),
        Reported::Lost => (ReportState::Lost, None),
    };
    if status.report != report {
        status.report = report;
        status.since_usec = since_usec;
    }
    for condition in &mut status.devices {
        let next = devices
            .and_then(|devices| {
                devices
                    .into_iter()
                    .find(|&(device, _)| api::Device::from(device) == condition.device)
            })
            .map_or(api::DeviceStatus::Unknown, |(_, status)| status.into());
        if condition.status != next {
            condition.status = next;
            condition.since_usec = since_usec;
        }
    }
}

/// Returns the status of the controller in API types.
pub(crate) fn status(
    controller: &Controller,
    slates: &Slates,
    preset: Option<&PresetRef>,
    conditions: &Conditions,
) -> Status {
    let cam = controller.cam();
    let mic = controller.mic();
    Status {
        cam: CamStatus {
            requested: slates.cam_value(cam.requested()).to_owned(),
            granted: slates.cam_value(cam.target().permission).to_owned(),
            actual: cam_output(cam, slates),
            in_effect: cam.summary().in_effect,
            latched: latched(cam),
            hold: cam.hold().map(Cause::from),
            stream: cam.stream().into(),
        },
        mic: MicStatus {
            requested: mic.requested().into(),
            granted: mic.target().permission.into(),
            actual: match mic.observed() {
                Reported::Fresh(report) => Some(report.output.into()),
                Reported::Pending | Reported::Lost => None,
            },
            in_effect: mic.summary().in_effect,
            latched: latched(mic),
            hold: mic.hold().map(Cause::from),
        },
        owners: vec![conditions.cam.clone(), conditions.audio.clone()],
        host: controller.host().into(),
        health: controller.health().into(),
        preset: preset.map(|preset| preset.as_str().to_owned()),
    }
}

/// Returns whether the last fresh report of the owner of the devices of
/// `plane` has a latch.
fn latched<P: Plane>(plane: &PlaneState<P>) -> bool
where
    P::Report: Report,
{
    matches!(plane.observed(), Reported::Fresh(report) if report.latched().is_some())
}

/// Returns the output of the cam worker as a cam value, or `None` without a
/// fresh report.
fn cam_output(state: &PlaneState<presencemux_core::Cam>, slates: &Slates) -> Option<String> {
    let Reported::Fresh(report) = state.observed() else {
        return None;
    };
    let value = match report.output {
        CamOutput::Nothing => api::CAM_NOTHING,
        CamOutput::Blank => slates.cam_value(CamPermission::Blank),
        CamOutput::Slate(Some(id)) => slates.cam_value(CamPermission::Slate(id)),
        CamOutput::Slate(None) => api::CAM_SLATE,
        CamOutput::Live => slates.cam_value(CamPermission::Live),
    };
    Some(value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use presencemux_api::{CamStream, Health, Host, MicOutput, Mute, Permission};
    use presencemux_core::{
        CamReport, CamStream as CoreStream, Command, DeviceStatus, Observation, PlaneId, Preset,
    };

    use super::*;
    use crate::config::Config;

    const BOOT: Monotonic = Monotonic::from_duration(Duration::from_secs(10));
    const LATER: Monotonic = Monotonic::from_duration(Duration::from_secs(20));

    fn cam_report(worker: DeviceStatus, camera: DeviceStatus) -> CamReport {
        CamReport {
            worker,
            camera,
            stream: CoreStream::Idle,
            latched: None,
            output: CamOutput::Blank,
            exceeded: false,
            applied: None,
        }
    }

    /// Returns the conditions of a controller with the cam report `report` at
    /// `LATER`, after the boot at `BOOT`.
    fn conditions_after(report: Observation) -> Conditions {
        let (mut controller, _) = Controller::new();
        let mut conditions = Conditions::new(BOOT);
        let _ = controller.handle_input(report.into());
        conditions.update(&controller, LATER);
        conditions
    }

    #[test]
    fn boot_status_is_privacy() {
        let config = Config::parse("").unwrap();
        let (controller, _) = Controller::new();

        let status = status(&controller, &config.slates, None, &Conditions::new(BOOT));

        assert_eq!(
            (status.cam, status.mic, status.host, status.health),
            (
                CamStatus {
                    requested: "blank".to_owned(),
                    granted: "blank".to_owned(),
                    actual: None,
                    in_effect: false,
                    latched: false,
                    hold: None,
                    stream: CamStream::Idle,
                },
                MicStatus {
                    requested: Mute::Muted,
                    granted: Permission::Denied,
                    actual: None,
                    in_effect: false,
                    latched: false,
                    hold: None,
                },
                Host::Disconnected,
                Health::Degraded,
            )
        );
    }

    #[test]
    fn boot_status_has_each_owner_pending_since_the_boot() {
        let config = Config::parse("").unwrap();
        let (controller, _) = Controller::new();

        let status = status(&controller, &config.slates, None, &Conditions::new(BOOT));

        let unknown = |device| DeviceCondition {
            device,
            status: api::DeviceStatus::Unknown,
            since_usec: 10_000_000,
        };
        assert_eq!(
            status.owners,
            [
                OwnerStatus {
                    owner: api::Owner::CamWorker,
                    report: ReportState::Pending,
                    since_usec: 10_000_000,
                    devices: vec![
                        unknown(api::Device::CamWorker),
                        unknown(api::Device::Camera)
                    ],
                    message: None,
                },
                OwnerStatus {
                    owner: api::Owner::AudioWorker,
                    report: ReportState::Pending,
                    since_usec: 10_000_000,
                    devices: vec![
                        unknown(api::Device::Gate),
                        unknown(api::Device::AudioInterface)
                    ],
                    message: None,
                },
            ]
        );
    }

    #[test]
    fn cam_shows_the_requested_slate_name() {
        let config = Config::parse("[slates.brb]\n").unwrap();
        let brb = config.slates.parse_cam("brb").unwrap();
        let (mut controller, _) = Controller::new();
        let _ = controller.handle_input(
            Command::Apply(Preset {
                cam: Some(brb),
                mic: None,
            })
            .into(),
        );

        let status = status(&controller, &config.slates, None, &Conditions::new(BOOT));

        assert_eq!(status.cam.requested, "brb");
    }

    #[test]
    fn cam_output_nothing_shows_as_nothing() {
        let config = Config::parse("").unwrap();
        let (mut controller, _) = Controller::new();
        let report = CamReport {
            output: CamOutput::Nothing,
            exceeded: false,
            ..cam_report(DeviceStatus::Ready, DeviceStatus::Ready)
        };
        let _ = controller.handle_input(Observation::Cam(report).into());

        let status = status(&controller, &config.slates, None, &Conditions::new(BOOT));

        assert_eq!(status.cam.actual.as_deref(), Some("nothing"));
    }

    #[test]
    fn mic_output_of_a_fresh_report_shows_as_actual() {
        let config = Config::parse("").unwrap();
        let (mut controller, _) = Controller::new();
        let report = presencemux_core::MicReport {
            gate: DeviceStatus::Ready,
            interface: DeviceStatus::Ready,
            latched: None,
            output: presencemux_core::MicOutput::Silent,
            exceeded: false,
            applied: None,
        };
        let _ = controller.handle_input(Observation::Mic(report).into());

        let status = status(&controller, &config.slates, None, &Conditions::new(BOOT));

        assert_eq!(status.mic.actual, Some(MicOutput::Silent));
    }

    #[test]
    fn latched_report_shows_the_plane_latched() {
        let config = Config::parse("").unwrap();
        let (mut controller, _) = Controller::new();
        let report = CamReport {
            latched: Some(presencemux_core::Epoch::BOOT),
            ..cam_report(DeviceStatus::Ready, DeviceStatus::Ready)
        };
        let _ = controller.handle_input(Observation::Cam(report).into());

        let status = status(&controller, &config.slates, None, &Conditions::new(BOOT));

        assert!(status.cam.latched);
    }

    #[test]
    fn slate_output_without_an_id_shows_as_slate() {
        let config = Config::parse("").unwrap();
        let (mut controller, _) = Controller::new();
        let report = CamReport {
            output: CamOutput::Slate(None),
            ..cam_report(DeviceStatus::Ready, DeviceStatus::Ready)
        };
        let _ = controller.handle_input(Observation::Cam(report).into());

        let status = status(&controller, &config.slates, None, &Conditions::new(BOOT));

        assert_eq!(status.cam.actual.as_deref(), Some("slate"));
    }

    #[test]
    fn first_report_makes_the_owner_fresh_at_its_time() {
        let conditions = conditions_after(Observation::Cam(cam_report(
            DeviceStatus::Ready,
            DeviceStatus::Missing,
        )));

        assert_eq!(
            (conditions.cam.report, conditions.cam.since_usec),
            (ReportState::Fresh, 20_000_000)
        );
    }

    #[test]
    fn first_report_gives_each_device_its_status() {
        let conditions = conditions_after(Observation::Cam(cam_report(
            DeviceStatus::Ready,
            DeviceStatus::Missing,
        )));

        assert_eq!(
            conditions.cam.devices,
            [
                DeviceCondition {
                    device: api::Device::CamWorker,
                    status: api::DeviceStatus::Ready,
                    since_usec: 20_000_000,
                },
                DeviceCondition {
                    device: api::Device::Camera,
                    status: api::DeviceStatus::Missing,
                    since_usec: 20_000_000,
                },
            ]
        );
    }

    #[test]
    fn same_device_status_keeps_its_time() {
        let (mut controller, _) = Controller::new();
        let mut conditions = Conditions::new(BOOT);
        let report = cam_report(DeviceStatus::Ready, DeviceStatus::Ready);
        let _ = controller.handle_input(Observation::Cam(report).into());
        conditions.update(&controller, BOOT);
        let _ = controller.handle_input(Observation::Cam(report).into());

        conditions.update(&controller, LATER);

        assert_eq!(
            conditions.cam.devices[0],
            DeviceCondition {
                device: api::Device::CamWorker,
                status: api::DeviceStatus::Ready,
                since_usec: 10_000_000,
            }
        );
    }

    #[test]
    fn lost_owner_makes_each_device_unknown() {
        let (mut controller, _) = Controller::new();
        let mut conditions = Conditions::new(BOOT);
        let report = cam_report(DeviceStatus::Ready, DeviceStatus::Ready);
        let _ = controller.handle_input(Observation::Cam(report).into());
        conditions.update(&controller, BOOT);
        let _ = controller.handle_input(Observation::Lost(PlaneId::Cam).into());

        conditions.update(&controller, LATER);

        assert_eq!(
            conditions.cam.devices,
            [
                DeviceCondition {
                    device: api::Device::CamWorker,
                    status: api::DeviceStatus::Unknown,
                    since_usec: 20_000_000,
                },
                DeviceCondition {
                    device: api::Device::Camera,
                    status: api::DeviceStatus::Unknown,
                    since_usec: 20_000_000,
                },
            ]
        );
    }

    #[test]
    fn new_message_of_an_owner_is_a_change() {
        let mut conditions = Conditions::new(BOOT);

        let changed = conditions.set_message(Owner::AudioWorker, Some("late".to_owned()));

        assert!(changed);
    }

    #[test]
    fn same_message_of_an_owner_is_no_change() {
        let mut conditions = Conditions::new(BOOT);
        conditions.set_message(Owner::AudioWorker, Some("late".to_owned()));

        let changed = conditions.set_message(Owner::AudioWorker, Some("late".to_owned()));

        assert!(!changed);
    }
}
