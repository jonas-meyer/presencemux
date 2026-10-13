//! The pure, sans-IO controller of PresenceMux.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(clippy::wildcard_enum_match_arm)]

mod cam;
mod device;
mod host;
mod mic;
mod on_air;
mod plane;
mod policy;
mod preset;
mod report;

pub use cam::{Cam, CamOutput, CamPermission, CamReport, CamStream, SlateId};
pub use device::{Device, DeviceStatus, Health};
pub use host::HostStatus;
pub use mic::{Mic, MicMute, MicOutput, MicPermission, MicReport};
pub use on_air::OnAir;
pub use plane::{Cause, Epoch, Granted, Plane, PlaneId, PlaneState, PlaneSummary, PrivacyPlane};
pub use preset::Preset;
pub use report::{Report, Reported};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Command(Command),
    Observation(Observation),
}

/// An input from the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Apply(Preset),
    /// Applies the request of a held plane again. Acts only while the plane
    /// has a hold, the health of its owner is `Ready` and the host is
    /// connected.
    Resume(PlaneId),
}

/// An input from automation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    /// A report of the cam worker.
    Cam(CamReport),
    /// A report of the audio worker.
    Mic(MicReport),
    /// The daemon has no contact with the owner of the devices of this plane.
    Lost(PlaneId),
    /// A lease of this plane lapsed. The media layer or the event loop reports
    /// it, with the epoch of the lease. A lapse with an epoch older than the
    /// last command for this plane has no effect.
    LeaseExpired(PlaneId, Epoch),
    Host(HostStatus),
    /// The host received no stream of the cam worker for the stream timeout.
    /// The event loop reports it, with the epoch of the cam plane at the start
    /// of the time without a stream. A timeout with an epoch older than the
    /// last command for the cam plane, or while the host receives the stream,
    /// has no effect.
    StreamTimeout(Epoch),
}

impl From<Command> for Event {
    fn from(command: Command) -> Self {
        Self::Command(command)
    }
}

impl From<CamReport> for Observation {
    fn from(report: CamReport) -> Self {
        Self::Cam(report)
    }
}

impl From<MicReport> for Observation {
    fn from(report: MicReport) -> Self {
        Self::Mic(report)
    }
}

impl From<Observation> for Event {
    fn from(observation: Observation) -> Self {
        Self::Observation(observation)
    }
}

/// The targets that changed after one input. A field is `None` if its plane did
/// not change.
#[must_use]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub cam: Option<Granted<CamPermission>>,
    pub mic: Option<Granted<MicPermission>>,
    /// Whether the status changed, also without a new target.
    pub status_changed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Controller {
    epoch: Epoch,
    host: HostStatus,
    cam: PlaneState<Cam>,
    mic: PlaneState<Mic>,
}

impl Controller {
    /// Creates a controller at [`Preset::PRIVACY`], with the boot targets to
    /// apply.
    pub fn new() -> (Self, Effects) {
        let controller = Self {
            epoch: Epoch::BOOT,
            host: HostStatus::default(),
            cam: PlaneState::new(),
            mic: PlaneState::new(),
        };
        let effects = controller.resync();
        (controller, effects)
    }

    pub fn handle_input(&mut self, event: Event) -> Effects {
        let before = self.clone();
        match event {
            Event::Command(Command::Apply(preset)) => self.apply(preset),
            Event::Command(Command::Resume(plane)) => self.resume(plane),
            Event::Observation(Observation::Cam(report)) => {
                self.cam.observe(Reported::Fresh(report));
            }
            Event::Observation(Observation::Mic(report)) => {
                self.mic.observe(Reported::Fresh(report));
            }
            Event::Observation(Observation::Lost(plane)) => match plane {
                PlaneId::Cam => self.cam.observe(Reported::Lost),
                PlaneId::Mic => self.mic.observe(Reported::Lost),
            },
            Event::Observation(Observation::LeaseExpired(plane, epoch)) => {
                if epoch >= self.granted(plane) {
                    self.lower(plane, Cause::ControllerStalled);
                }
            }
            Event::Observation(Observation::Host(status)) => self.observe_host(status),
            Event::Observation(Observation::StreamTimeout(epoch)) => {
                if policy::stream_lowers(&self.cam, epoch) {
                    self.lower(PlaneId::Cam, Cause::StreamStopped);
                }
            }
        }
        let cam = self.cam.update(Granted {
            permission: policy::cam(&self.cam),
            epoch: self.cam.epoch,
        });
        let mic = self.mic.update(Granted {
            permission: policy::mic(&self.mic),
            epoch: self.mic.epoch,
        });
        Effects {
            cam,
            mic,
            status_changed: *self != before,
        }
    }

    /// Returns the current target of every plane.
    pub fn resync(&self) -> Effects {
        Effects {
            cam: Some(self.cam.target()),
            mic: Some(self.mic.target()),
            status_changed: false,
        }
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    pub fn host(&self) -> HostStatus {
        self.host
    }

    pub fn health(&self) -> Health {
        policy::health::<Cam>(self.cam.observed()).max(policy::health::<Mic>(self.mic.observed()))
    }

    pub fn cam(&self) -> &PlaneState<Cam> {
        &self.cam
    }

    pub fn mic(&self) -> &PlaneState<Mic> {
        &self.mic
    }

    pub fn plane(&self, id: PlaneId) -> PlaneSummary {
        match id {
            PlaneId::Cam => self.cam.summary(),
            PlaneId::Mic => self.mic.summary(),
        }
    }

    fn apply(&mut self, preset: Preset) {
        // Without `..`, a new field in `Preset` does not compile until this code handles it.
        let Preset { cam, mic } = preset;
        self.epoch = self.epoch.next();
        if let Some(cam) = cam {
            self.cam.set(cam, self.epoch);
        }
        if let Some(mic) = mic {
            self.mic.set(mic, self.epoch);
        }
    }

    fn resume(&mut self, plane: PlaneId) {
        match plane {
            PlaneId::Cam => resume(&mut self.cam, self.host, &mut self.epoch),
            PlaneId::Mic => resume(&mut self.mic, self.host, &mut self.epoch),
        }
    }

    /// Returns the epoch of the last command that set the plane.
    pub fn granted(&self, plane: PlaneId) -> Epoch {
        match plane {
            PlaneId::Cam => self.cam.epoch,
            PlaneId::Mic => self.mic.epoch,
        }
    }

    fn observe_host(&mut self, status: HostStatus) {
        let before = core::mem::replace(&mut self.host, status);
        if policy::host_lowers(before, status) {
            for plane in PlaneId::ALL {
                self.lower(plane, Cause::Host(status));
            }
        }
    }

    fn lower(&mut self, plane: PlaneId, cause: Cause) {
        match plane {
            PlaneId::Cam => self.cam.lower(cause),
            PlaneId::Mic => self.mic.lower(cause),
        }
    }
}

/// Resumes `plane` if [`policy::resumes`] permits it. The resume takes the
/// epoch after `epoch`.
fn resume<P: PrivacyPlane>(plane: &mut PlaneState<P>, host: HostStatus, epoch: &mut Epoch) {
    if policy::resumes(plane, host) {
        *epoch = epoch.next();
        plane.resume(*epoch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DeviceStatus::{Fault, Missing, Ready};

    fn cam(worker: DeviceStatus, camera: DeviceStatus) -> CamReport {
        CamReport {
            worker,
            camera,
            stream: CamStream::Idle,
            latched: None,
            output: CamOutput::Blank,
            exceeded: false,
            applied: None,
        }
    }

    fn mic(gate: DeviceStatus, interface: DeviceStatus) -> MicReport {
        MicReport {
            gate,
            interface,
            latched: None,
            output: MicOutput::Silent,
            exceeded: false,
            applied: None,
        }
    }

    fn live() -> Event {
        Command::Apply(Preset {
            cam: Some(CamPermission::Live),
            mic: Some(MicMute::Unmuted),
        })
        .into()
    }

    fn after(events: &[Event]) -> Controller {
        let (mut controller, _) = Controller::new();
        for &event in events {
            let _ = controller.handle_input(event);
        }
        controller
    }

    /// Returns a controller with ready devices and both planes live.
    fn live_controller() -> Controller {
        after(&[
            Observation::Cam(cam(Ready, Ready)).into(),
            Observation::Mic(mic(Ready, Ready)).into(),
            live(),
        ])
    }

    /// Returns a controller with a connected host and both planes live, whose
    /// camera had a fault and is ready again. The cam plane has the hold
    /// `DeviceFault(Camera)`, and the mic plane has no hold.
    fn held_controller() -> Controller {
        after(&[
            Observation::Host(HostStatus::Connected).into(),
            Observation::Cam(cam(Ready, Ready)).into(),
            Observation::Mic(mic(Ready, Ready)).into(),
            live(),
            Observation::Cam(cam(Ready, Fault)).into(),
            Observation::Cam(cam(Ready, Ready)).into(),
        ])
    }

    #[test]
    fn resume_after_the_fault_clears_grants_the_request_again() {
        let mut controller = held_controller();

        let _ = controller.handle_input(Command::Resume(PlaneId::Cam).into());

        assert_eq!(controller.cam().target().permission, CamPermission::Live);
    }

    #[test]
    fn resume_clears_the_hold() {
        let mut controller = held_controller();

        let _ = controller.handle_input(Command::Resume(PlaneId::Cam).into());

        assert_eq!(controller.cam().hold(), None);
    }

    #[test]
    fn resume_while_the_camera_is_missing_keeps_the_hold() {
        let mut controller = held_controller();
        let _ = controller.handle_input(Observation::Cam(cam(Ready, Missing)).into());

        let _ = controller.handle_input(Command::Resume(PlaneId::Cam).into());

        assert_eq!(
            controller.cam().hold(),
            Some(Cause::DeviceFault(Device::Camera))
        );
    }

    #[test]
    fn resume_while_the_host_is_suspended_keeps_the_hold() {
        let mut controller = held_controller();
        let _ = controller.handle_input(Observation::Host(HostStatus::Suspended).into());

        let _ = controller.handle_input(Command::Resume(PlaneId::Mic).into());

        assert_eq!(
            controller.mic().hold(),
            Some(Cause::Host(HostStatus::Suspended))
        );
    }

    #[test]
    fn resume_grants_the_request_in_a_new_epoch() {
        let mut controller = held_controller();
        let before = controller.epoch();

        let _ = controller.handle_input(Command::Resume(PlaneId::Cam).into());

        assert_eq!(controller.cam().target().epoch, before.next());
    }

    #[test]
    fn resume_of_the_mic_without_a_hold_keeps_the_epoch() {
        let mut controller = held_controller();
        let before = controller.epoch();

        let _ = controller.handle_input(Command::Resume(PlaneId::Mic).into());

        assert_eq!(controller.epoch(), before);
    }

    #[test]
    fn resume_of_one_plane_keeps_the_hold_of_the_other() {
        let mut controller = held_controller();
        let _ = controller.handle_input(Observation::Mic(mic(Ready, Fault)).into());
        let _ = controller.handle_input(Observation::Mic(mic(Ready, Ready)).into());

        let _ = controller.handle_input(Command::Resume(PlaneId::Cam).into());

        assert_eq!(
            controller.mic().hold(),
            Some(Cause::DeviceFault(Device::AudioInterface))
        );
    }

    #[test]
    fn command_with_ready_devices_goes_live() {
        let mut controller = after(&[
            Observation::Cam(cam(Ready, Ready)).into(),
            Observation::Mic(mic(Ready, Ready)).into(),
        ]);

        let effects = controller.handle_input(live());

        assert_eq!(
            (
                effects.cam.unwrap().permission,
                effects.mic.unwrap().permission
            ),
            (CamPermission::Live, MicPermission::Permitted)
        );
    }

    #[test]
    fn command_grants_its_targets_under_the_next_epoch() {
        let mut controller = after(&[
            Observation::Cam(cam(Ready, Ready)).into(),
            Observation::Mic(mic(Ready, Ready)).into(),
        ]);

        let effects = controller.handle_input(live());

        assert_eq!(
            (effects.cam.unwrap().epoch, effects.mic.unwrap().epoch),
            (Epoch::new(1), Epoch::new(1))
        );
    }

    #[test]
    fn device_starting_after_the_command_lets_the_plane_rise() {
        let mut controller = after(&[Observation::Cam(cam(Ready, Missing)).into(), live()]);

        let effects = controller.handle_input(Observation::Cam(cam(Ready, Ready)).into());

        assert_eq!(effects.cam.map(|g| g.permission), Some(CamPermission::Live));
    }

    #[test]
    fn fault_before_the_device_is_ready_cancels_the_pending_live() {
        let mut controller = after(&[
            Observation::Cam(cam(Ready, Missing)).into(),
            live(),
            Observation::Cam(cam(Ready, Fault)).into(),
        ]);

        let _ = controller.handle_input(Observation::Cam(cam(Ready, Ready)).into());

        assert_eq!(controller.cam().target().permission, CamPermission::Blank);
    }

    #[test]
    fn report_with_a_worker_fault_and_a_camera_fault_gives_the_worker_fault() {
        let mut controller = live_controller();

        let _ = controller.handle_input(Observation::Cam(cam(Fault, Fault)).into());

        assert_eq!(
            controller.cam().hold(),
            Some(Cause::DeviceFault(Device::CamWorker))
        );
    }

    #[test]
    fn report_with_a_gate_fault_and_a_missing_interface_gives_the_gate_fault() {
        let mut controller = live_controller();

        let _ = controller.handle_input(Observation::Mic(mic(Fault, Missing)).into());

        assert_eq!(
            controller.mic().hold(),
            Some(Cause::DeviceFault(Device::Gate))
        );
    }

    #[test]
    fn lost_owner_lowers_its_plane_with_owner_lost() {
        let mut controller = live_controller();

        let _ = controller.handle_input(Observation::Lost(PlaneId::Cam).into());

        assert_eq!(
            controller.cam().hold(),
            Some(Cause::OwnerLost(PlaneId::Cam))
        );
    }

    #[test]
    fn lost_owner_keeps_the_other_plane() {
        let mut controller = live_controller();

        let _ = controller.handle_input(Observation::Lost(PlaneId::Cam).into());

        assert_eq!(
            controller.mic().target().permission,
            MicPermission::Permitted
        );
    }

    #[test]
    fn latch_of_the_current_grant_lowers_its_plane_as_stalled() {
        let mut controller = live_controller();
        let report = CamReport {
            latched: Some(controller.granted(PlaneId::Cam)),
            ..cam(Ready, Ready)
        };

        let _ = controller.handle_input(Observation::Cam(report).into());

        assert_eq!(controller.cam().hold(), Some(Cause::ControllerStalled));
    }

    #[test]
    fn latch_from_before_the_last_command_keeps_the_live_cam() {
        let mut controller = live_controller();
        let report = CamReport {
            latched: Some(Epoch::BOOT),
            ..cam(Ready, Ready)
        };

        let _ = controller.handle_input(Observation::Cam(report).into());

        assert_eq!(controller.cam().target().permission, CamPermission::Live);
    }

    #[test]
    fn report_with_a_fault_and_a_latch_gives_the_fault() {
        let mut controller = live_controller();
        let report = CamReport {
            latched: Some(controller.granted(PlaneId::Cam)),
            ..cam(Ready, Fault)
        };

        let _ = controller.handle_input(Observation::Cam(report).into());

        assert_eq!(
            controller.cam().hold(),
            Some(Cause::DeviceFault(Device::Camera))
        );
    }

    #[test]
    fn held_plane_keeps_the_request_of_the_user() {
        let mut controller = live_controller();

        let _ = controller.handle_input(Observation::Lost(PlaneId::Cam).into());

        assert_eq!(controller.cam().requested(), CamPermission::Live);
    }

    #[test]
    fn command_for_the_held_plane_clears_the_hold() {
        let mut controller = live_controller();
        let _ = controller.handle_input(Observation::Lost(PlaneId::Cam).into());

        let _ = controller.handle_input(live());

        assert_eq!(controller.cam().hold(), None);
    }

    #[test]
    fn command_for_another_plane_keeps_the_hold() {
        let mut controller = live_controller();
        let _ = controller.handle_input(Observation::Lost(PlaneId::Cam).into());

        let _ = controller.handle_input(
            Command::Apply(Preset {
                cam: None,
                mic: Some(MicMute::Muted),
            })
            .into(),
        );

        assert_eq!(
            controller.cam().hold(),
            Some(Cause::OwnerLost(PlaneId::Cam))
        );
    }

    #[test]
    fn exceeded_output_lowers_the_plane_as_exceeded() {
        let mut controller = live_controller();
        let report = CamReport {
            exceeded: true,
            ..cam(Ready, Ready)
        };

        let _ = controller.handle_input(Observation::Cam(report).into());

        assert_eq!(
            controller.cam().hold(),
            Some(Cause::OutputExceeded(PlaneId::Cam))
        );
    }

    #[test]
    fn exceeded_output_keeps_the_mic_denied_after_a_command() {
        let mut controller = live_controller();
        let report = MicReport {
            exceeded: true,
            ..mic(Ready, Ready)
        };
        let _ = controller.handle_input(Observation::Mic(report).into());

        let _ = controller.handle_input(live());

        assert_eq!(controller.mic().target().permission, MicPermission::Denied);
    }

    #[test]
    fn camera_fault_with_an_exceeded_output_gives_the_camera_fault() {
        let mut controller = live_controller();
        let report = CamReport {
            exceeded: true,
            ..cam(Ready, Fault)
        };

        let _ = controller.handle_input(Observation::Cam(report).into());

        assert_eq!(
            controller.cam().hold(),
            Some(Cause::DeviceFault(Device::Camera))
        );
    }

    #[test]
    fn health_is_fault_while_an_output_exceeds_its_grant() {
        let report = MicReport {
            exceeded: true,
            ..mic(Ready, Ready)
        };
        let controller = after(&[
            Observation::Cam(cam(Ready, Ready)).into(),
            Observation::Mic(report).into(),
        ]);

        assert_eq!(controller.health(), Health::Fault);
    }

    #[test]
    fn actual_level_is_unknown_while_the_owner_is_lost() {
        let controller = after(&[Observation::Lost(PlaneId::Cam).into()]);

        assert_eq!(controller.plane(PlaneId::Cam).actual, None);
    }

    #[test]
    fn actual_level_comes_from_the_reported_output() {
        let report = CamReport {
            output: CamOutput::Slate(Some(SlateId(0))),
            ..cam(Ready, Ready)
        };
        let controller = after(&[Observation::Cam(report).into()]);

        assert_eq!(controller.plane(PlaneId::Cam).actual, Some(OnAir::Canned));
    }

    #[test]
    fn target_is_in_effect_when_the_owner_applies_it() {
        let mut controller = live_controller();
        let report = CamReport {
            output: CamOutput::Live,
            exceeded: false,
            applied: Some(controller.cam().target()),
            ..cam(Ready, Ready)
        };

        let _ = controller.handle_input(Observation::Cam(report).into());

        assert!(controller.plane(PlaneId::Cam).in_effect);
    }

    #[test]
    fn target_is_not_in_effect_while_the_owner_applies_an_older_grant() {
        let mut controller = live_controller();
        let report = CamReport {
            applied: Some(Granted {
                permission: CamPermission::Blank,
                epoch: Epoch::BOOT,
            }),
            ..cam(Ready, Ready)
        };

        let _ = controller.handle_input(Observation::Cam(report).into());

        assert!(!controller.plane(PlaneId::Cam).in_effect);
    }

    #[test]
    fn stream_timeout_while_idle_lowers_the_live_cam_with_stream_stopped() {
        let mut controller = live_controller();
        let epoch = controller.granted(PlaneId::Cam);

        let _ = controller.handle_input(Observation::StreamTimeout(epoch).into());

        assert_eq!(controller.cam().hold(), Some(Cause::StreamStopped));
    }

    #[test]
    fn stream_timeout_while_streaming_keeps_the_live_cam() {
        let streaming = CamReport {
            stream: CamStream::Streaming,
            ..cam(Ready, Ready)
        };
        let mut controller = after(&[Observation::Cam(streaming).into(), live()]);
        let epoch = controller.granted(PlaneId::Cam);

        let _ = controller.handle_input(Observation::StreamTimeout(epoch).into());

        assert_eq!(controller.cam().target().permission, CamPermission::Live);
    }

    #[test]
    fn stream_timeout_while_idle_lowers_a_slate() {
        let mut controller = after(&[
            Observation::Cam(cam(Ready, Missing)).into(),
            Command::Apply(Preset {
                cam: Some(CamPermission::Slate(SlateId(0))),
                mic: None,
            })
            .into(),
        ]);
        let epoch = controller.granted(PlaneId::Cam);

        let _ = controller.handle_input(Observation::StreamTimeout(epoch).into());

        assert_eq!(controller.cam().target().permission, CamPermission::Blank);
    }

    #[test]
    fn health_is_degraded_while_an_owner_has_not_reported() {
        let controller = after(&[Observation::Mic(mic(Ready, Ready)).into()]);

        assert_eq!(controller.health(), Health::Degraded);
    }

    #[test]
    fn health_is_degraded_while_a_device_is_missing() {
        let controller = after(&[
            Observation::Cam(cam(Ready, Missing)).into(),
            Observation::Mic(mic(Ready, Ready)).into(),
        ]);

        assert_eq!(controller.health(), Health::Degraded);
    }

    #[test]
    fn health_is_fault_when_a_device_has_a_fault() {
        let controller = after(&[Observation::Mic(mic(Fault, Ready)).into()]);

        assert_eq!(controller.health(), Health::Fault);
    }

    #[test]
    fn health_is_fault_when_an_owner_is_lost() {
        let controller = after(&[
            Observation::Cam(cam(Ready, Ready)).into(),
            Observation::Lost(PlaneId::Mic).into(),
        ]);

        assert_eq!(controller.health(), Health::Fault);
    }

    #[test]
    fn health_is_ready_when_every_device_is_ready() {
        let controller = after(&[
            Observation::Cam(cam(Ready, Ready)).into(),
            Observation::Mic(mic(Ready, Ready)).into(),
        ]);

        assert_eq!(controller.health(), Health::Ready);
    }

    #[test]
    fn unchanged_report_gives_no_effects() {
        let mut controller = after(&[Observation::Cam(cam(Ready, Ready)).into()]);

        let effects = controller.handle_input(Observation::Cam(cam(Ready, Ready)).into());

        assert_eq!(effects, Effects::default());
    }
}
