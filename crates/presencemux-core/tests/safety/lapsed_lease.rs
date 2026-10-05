use presencemux_core::{Cause, Observation, OnAir};
use proptest::prelude::*;

use crate::support::{after, cause_after_lowering, events, observations, plane};

proptest! {
    #[test]
    fn keeps_its_plane_off_until_a_command(
        prefix in events(),
        lapsed in plane(),
        later in observations(),
    ) {
        let mut controller = after(prefix);
        let before = controller.plane(lapsed);

        let _ = controller.handle_input(Observation::LeaseExpired(lapsed).into());
        prop_assert_eq!(
            controller.plane(lapsed).cause,
            cause_after_lowering(before, Cause::ControllerStalled)
        );

        for observation in later {
            let _ = controller.handle_input(observation.into());
            prop_assert_eq!(controller.plane(lapsed).on_air, OnAir::Off);
        }
    }
}
