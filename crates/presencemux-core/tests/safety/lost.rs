use presencemux_core::{Cause, Effects, Observation, OnAir, PlaneId};
use proptest::prelude::*;

use crate::support::{after, events, hold_after_lowering, observations, plane, ready_report};

proptest! {
    #[test]
    fn lowers_only_its_plane_with_owner_lost(prefix in events(), lost in plane()) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(ready_report(lost).into());
        let before = controller.clone();

        let _ = controller.handle_input(Observation::Lost(lost).into());

        prop_assert_eq!(controller.plane(lost).granted, OnAir::Off);
        prop_assert_eq!(
            controller.plane(lost).hold(),
            hold_after_lowering(before.plane(lost), Cause::OwnerLost(lost))
        );
        for other in PlaneId::ALL.into_iter().filter(|&p| p != lost) {
            prop_assert_eq!(controller.plane(other), before.plane(other));
        }
    }

    #[test]
    fn keeps_its_plane_off_until_a_command(
        prefix in events(),
        lost in plane(),
        later in observations(),
    ) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(ready_report(lost).into());
        let _ = controller.handle_input(Observation::Lost(lost).into());

        for observation in later {
            let _ = controller.handle_input(observation.into());
            prop_assert_eq!(controller.plane(lost).granted, OnAir::Off);
        }
    }

    #[test]
    fn a_second_time_has_no_effect(prefix in events(), lost in plane()) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(Observation::Lost(lost).into());
        let before = controller.clone();

        let effects = controller.handle_input(Observation::Lost(lost).into());

        prop_assert_eq!(effects, Effects::default());
        prop_assert_eq!(controller, before);
    }
}
