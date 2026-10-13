use presencemux_core::{CamStream, Cause, Command, Effects, Epoch, Observation, OnAir, PlaneId};
use proptest::prelude::*;

use crate::support::{
    after, cause_after_lowering, events, observations, preset_setting, stream_report,
};

proptest! {
    #[test]
    fn of_the_current_epoch_while_idle_lowers_only_the_cam_plane(prefix in events()) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(stream_report(CamStream::Idle).into());
        let before = controller.clone();
        let epoch = controller.granted(PlaneId::Cam);

        let _ = controller.handle_input(Observation::StreamTimeout(epoch).into());

        prop_assert_eq!(controller.plane(PlaneId::Cam).requested, OnAir::Off);
        prop_assert_eq!(
            controller.plane(PlaneId::Cam).cause,
            cause_after_lowering(before.plane(PlaneId::Cam), Cause::StreamStopped)
        );
        prop_assert_eq!(controller.plane(PlaneId::Mic), before.plane(PlaneId::Mic));
    }

    #[test]
    fn while_the_host_receives_the_stream_has_no_effect(prefix in events()) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(stream_report(CamStream::Streaming).into());
        let before = controller.clone();
        let epoch = controller.granted(PlaneId::Cam);

        let effects = controller.handle_input(Observation::StreamTimeout(epoch).into());

        prop_assert_eq!(effects, Effects::default());
        prop_assert_eq!(controller, before);
    }

    #[test]
    fn from_before_a_command_for_the_cam_plane_has_no_effect(
        prefix in events(),
        command in preset_setting(PlaneId::Cam),
    ) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(stream_report(CamStream::Idle).into());
        let old = controller.granted(PlaneId::Cam);
        let _ = controller.handle_input(Command::Apply(command).into());
        let before = controller.clone();

        let effects = controller.handle_input(Observation::StreamTimeout(old).into());

        prop_assert_eq!(effects, Effects::default());
        prop_assert_eq!(controller, before);
    }

    #[test]
    fn of_a_newer_epoch_while_idle_lowers_the_cam_plane(prefix in events()) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(stream_report(CamStream::Idle).into());

        let _ = controller.handle_input(Observation::StreamTimeout(Epoch::new(u32::MAX)).into());

        prop_assert_eq!(controller.plane(PlaneId::Cam).requested, OnAir::Off);
    }

    #[test]
    fn keeps_the_cam_plane_off_until_a_command(prefix in events(), later in observations()) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(stream_report(CamStream::Idle).into());
        let epoch = controller.granted(PlaneId::Cam);
        let _ = controller.handle_input(Observation::StreamTimeout(epoch).into());

        for observation in later {
            let _ = controller.handle_input(observation.into());
            prop_assert_eq!(controller.plane(PlaneId::Cam).on_air, OnAir::Off);
        }
    }
}
