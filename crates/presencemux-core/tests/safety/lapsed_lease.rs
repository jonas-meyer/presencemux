use presencemux_core::{Cause, Command, Effects, Epoch, Observation, OnAir, PlaneId};
use proptest::prelude::*;

use crate::support::{
    after, cause_after_lowering, events, latched_report, observations, plane, preset_setting,
    preset_without, ready_report,
};

proptest! {
    #[test]
    fn of_the_current_grant_lowers_only_its_plane(prefix in events(), lapsed in plane()) {
        let mut controller = after(prefix);
        let before = controller.clone();
        let epoch = controller.granted(lapsed);

        let _ = controller.handle_input(Observation::LeaseExpired(lapsed, epoch).into());

        prop_assert_eq!(controller.plane(lapsed).requested, OnAir::Off);
        prop_assert_eq!(
            controller.plane(lapsed).cause,
            cause_after_lowering(before.plane(lapsed), Cause::ControllerStalled)
        );
        for other in PlaneId::ALL.into_iter().filter(|&p| p != lapsed) {
            prop_assert_eq!(controller.plane(other), before.plane(other));
        }
    }

    #[test]
    fn of_a_newer_epoch_lowers_its_plane(prefix in events(), lapsed in plane()) {
        let mut controller = after(prefix);

        let _ = controller.handle_input(Observation::LeaseExpired(lapsed, Epoch::new(u32::MAX)).into());

        prop_assert_eq!(controller.plane(lapsed).requested, OnAir::Off);
    }

    #[test]
    fn keeps_its_plane_off_until_a_command(
        prefix in events(),
        lapsed in plane(),
        later in observations(),
    ) {
        let mut controller = after(prefix);
        let epoch = controller.granted(lapsed);
        let _ = controller.handle_input(Observation::LeaseExpired(lapsed, epoch).into());

        for observation in later {
            let _ = controller.handle_input(observation.into());
            prop_assert_eq!(controller.plane(lapsed).on_air, OnAir::Off);
        }
    }

    #[test]
    fn from_before_a_command_for_its_plane_has_no_effect(
        prefix in events(),
        (lapsed, command) in plane().prop_flat_map(|p| (Just(p), preset_setting(p))),
    ) {
        let mut controller = after(prefix);
        let old = controller.granted(lapsed);
        let _ = controller.handle_input(Command::Apply(command).into());
        let before = controller.clone();

        let effects = controller.handle_input(Observation::LeaseExpired(lapsed, old).into());

        prop_assert_eq!(effects, Effects::default());
        prop_assert_eq!(controller, before);
    }

    #[test]
    fn from_before_a_command_for_another_plane_lowers_its_plane(
        prefix in events(),
        (lapsed, command) in plane().prop_flat_map(|p| (Just(p), preset_without(p))),
    ) {
        let mut controller = after(prefix);
        let old = controller.granted(lapsed);
        let _ = controller.handle_input(Command::Apply(command).into());

        let _ = controller.handle_input(Observation::LeaseExpired(lapsed, old).into());

        prop_assert_eq!(controller.plane(lapsed).requested, OnAir::Off);
    }

    #[test]
    fn latch_in_a_report_of_the_current_grant_lowers_only_its_plane(
        prefix in events(),
        lapsed in plane(),
    ) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(ready_report(lapsed).into());
        let before = controller.clone();
        let epoch = controller.granted(lapsed);

        let _ = controller.handle_input(latched_report(lapsed, epoch).into());

        prop_assert_eq!(controller.plane(lapsed).requested, OnAir::Off);
        prop_assert_eq!(
            controller.plane(lapsed).cause,
            cause_after_lowering(before.plane(lapsed), Cause::ControllerStalled)
        );
        for other in PlaneId::ALL.into_iter().filter(|&p| p != lapsed) {
            prop_assert_eq!(controller.plane(other), before.plane(other));
        }
    }

    #[test]
    fn latch_in_a_report_from_before_a_command_for_its_plane_keeps_its_plane(
        prefix in events(),
        (lapsed, command) in plane().prop_flat_map(|p| (Just(p), preset_setting(p))),
    ) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(ready_report(lapsed).into());
        let old = controller.granted(lapsed);
        let _ = controller.handle_input(Command::Apply(command).into());
        let before = controller.clone();

        let effects = controller.handle_input(latched_report(lapsed, old).into());

        prop_assert_eq!((effects.cam, effects.mic), (None, None));
        prop_assert_eq!(controller.plane(lapsed), before.plane(lapsed));
    }
}
