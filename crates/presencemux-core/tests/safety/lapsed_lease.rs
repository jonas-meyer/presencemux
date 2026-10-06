use presencemux_core::{Cause, Observation, OnAir, PlaneId};
use proptest::prelude::*;

use crate::support::{after, cause_after_lowering, events, observations, plane};

proptest! {
    #[test]
    fn lowers_only_its_plane(prefix in events(), lapsed in plane()) {
        let mut controller = after(prefix);
        let before = controller.clone();

        let _ = controller.handle_input(Observation::LeaseExpired(lapsed).into());

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
    fn keeps_its_plane_off_until_a_command(
        prefix in events(),
        lapsed in plane(),
        later in observations(),
    ) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(Observation::LeaseExpired(lapsed).into());

        for observation in later {
            let _ = controller.handle_input(observation.into());
            prop_assert_eq!(controller.plane(lapsed).on_air, OnAir::Off);
        }
    }
}
