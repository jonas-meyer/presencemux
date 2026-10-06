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

pub use cam::{Cam, CamObserved, CamPermission, SlateId};
pub use device::{Device, DeviceStatus, Health};
pub use host::HostStatus;
pub use mic::{Mic, MicMute, MicObserved, MicPermission};
pub use on_air::OnAir;
pub use plane::{Cause, Epoch, Granted, Plane, PlaneId, PlaneState, PlaneSummary, PrivacyPlane};
pub use preset::Preset;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Command(Command),
    Observation(Observation),
}

/// An input from the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Apply(Preset),
}

/// An input from automation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    Device(Device, DeviceStatus),
    /// A lease of this plane lapsed. The media layer or the event loop reports
    /// it.
    LeaseExpired(PlaneId),
    Host(HostStatus),
}

impl From<Command> for Event {
    fn from(command: Command) -> Self {
        Self::Command(command)
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
            Event::Observation(Observation::Device(device, status)) => {
                self.observe(device, status);
            }
            Event::Observation(Observation::LeaseExpired(plane)) => {
                self.lower(plane, Cause::ControllerStalled);
            }
            Event::Observation(Observation::Host(status)) => self.observe_host(status),
        }
        let cam = self.cam.update(Granted {
            permission: policy::cam(&self.cam),
            epoch: self.epoch,
        });
        let mic = self.mic.update(Granted {
            permission: policy::mic(&self.mic),
            epoch: self.epoch,
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
        let cam = self.cam.observed();
        let mic = self.mic.observed();
        policy::health([cam.camera, cam.worker, mic.interface, mic.gate])
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
        // Without `..`, a new field in `Preset` does not compile until it is handled here.
        let Preset { cam, mic } = preset;
        self.epoch = self.epoch.next();
        if let Some(cam) = cam {
            self.cam.set(cam);
        }
        if let Some(mic) = mic {
            self.mic.set(mic);
        }
    }

    fn observe(&mut self, device: Device, status: DeviceStatus) {
        match device {
            Device::Camera => self.cam.observe(device, status, |o| &mut o.camera),
            Device::CamWorker => self.cam.observe(device, status, |o| &mut o.worker),
            Device::AudioInterface => self.mic.observe(device, status, |o| &mut o.interface),
            Device::Gate => self.mic.observe(device, status, |o| &mut o.gate),
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

#[cfg(test)]
mod tests {
    use super::*;
    use DeviceStatus::{Fault, Ready};

    fn device(device: Device, status: DeviceStatus) -> Event {
        Observation::Device(device, status).into()
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

    #[test]
    fn command_with_ready_devices_goes_live_under_a_newer_epoch() {
        let mut controller = after(&[
            device(Device::Camera, Ready),
            device(Device::CamWorker, Ready),
            device(Device::AudioInterface, Ready),
            device(Device::Gate, Ready),
        ]);
        let before = controller.epoch();

        let effects = controller.handle_input(live());

        let cam = effects.cam.unwrap();
        let mic = effects.mic.unwrap();
        assert_eq!(cam.permission, CamPermission::Live);
        assert_eq!(mic.permission, MicPermission::Permitted);
        assert!(cam.epoch > before);
        assert!(mic.epoch > before);
    }

    #[test]
    fn device_starting_after_the_command_lets_the_plane_rise() {
        let mut controller = after(&[device(Device::CamWorker, Ready), live()]);

        let effects = controller.handle_input(device(Device::Camera, Ready));

        assert_eq!(effects.cam.map(|g| g.permission), Some(CamPermission::Live));
    }

    #[test]
    fn health_is_degraded_while_a_device_is_missing() {
        let controller = after(&[
            device(Device::CamWorker, Ready),
            device(Device::AudioInterface, Ready),
            device(Device::Gate, Ready),
        ]);

        assert_eq!(controller.health(), Health::Degraded);
    }

    #[test]
    fn health_is_fault_when_a_device_has_a_fault() {
        let controller = after(&[device(Device::Gate, Fault)]);

        assert_eq!(controller.health(), Health::Fault);
    }

    #[test]
    fn health_is_ready_when_every_device_is_ready() {
        let controller = after(&[
            device(Device::Camera, Ready),
            device(Device::CamWorker, Ready),
            device(Device::AudioInterface, Ready),
            device(Device::Gate, Ready),
        ]);

        assert_eq!(controller.health(), Health::Ready);
    }

    #[test]
    fn unchanged_report_gives_no_effects() {
        let mut controller = after(&[device(Device::Camera, Ready)]);

        let effects = controller.handle_input(device(Device::Camera, Ready));

        assert_eq!(effects, Effects::default());
    }
}
