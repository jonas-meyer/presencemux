//! The status of the controller in API types.

use presencemux_api::{CamStatus, Cause, MicStatus, Status};
use presencemux_core::Controller;

use crate::config::{PresetRef, Slates};

/// Returns the status of the controller in API types.
pub(crate) fn status(
    controller: &Controller,
    slates: &Slates,
    preset: Option<&PresetRef>,
) -> Status {
    let cam = controller.cam();
    let mic = controller.mic();
    Status {
        cam: CamStatus {
            desired: slates.cam_value(cam.desired()).to_owned(),
            effective: slates.cam_value(cam.target().permission).to_owned(),
            cause: cam.cause().map(Cause::from),
        },
        mic: MicStatus {
            desired: mic.desired().into(),
            effective: mic.target().permission.into(),
            cause: mic.cause().map(Cause::from),
        },
        host: controller.host().into(),
        health: controller.health().into(),
        preset: preset.map(|preset| preset.as_str().to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use presencemux_api::{Health, Host, Mute, Permission};
    use presencemux_core::{Command, Preset};

    use super::*;
    use crate::config::Config;

    #[test]
    fn boot_status_is_privacy() {
        let config = Config::parse("").unwrap();
        let (controller, _) = Controller::new();

        let status = status(&controller, &config.slates, None);

        assert_eq!(
            status,
            Status {
                cam: CamStatus {
                    desired: "blank".to_owned(),
                    effective: "blank".to_owned(),
                    cause: None,
                },
                mic: MicStatus {
                    desired: Mute::Muted,
                    effective: Permission::Denied,
                    cause: None,
                },
                host: Host::Disconnected,
                health: Health::Degraded,
                preset: None,
            }
        );
    }

    #[test]
    fn cam_shows_the_slate_name() {
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

        let status = status(&controller, &config.slates, None);

        assert_eq!(status.cam.desired, "brb");
    }
}
