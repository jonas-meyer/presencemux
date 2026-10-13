use presencemux_core::{Cause, HostStatus, Observation, OnAir, PlaneId};
use proptest::prelude::*;

use crate::support::{after, events, hold_after_lowering, not_connected, observations};

proptest! {
    #[test]
    fn leaving_turns_every_plane_off_until_a_command(
        prefix in events(),
        left in not_connected(),
        later in observations(),
    ) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(Observation::Host(HostStatus::Connected).into());
        let before = controller.clone();

        let _ = controller.handle_input(Observation::Host(left).into());

        for plane in PlaneId::ALL {
            prop_assert_eq!(controller.plane(plane).granted, OnAir::Off);
            prop_assert_eq!(
                controller.plane(plane).hold,
                hold_after_lowering(before.plane(plane), Cause::Host(left))
            );
        }
        for observation in later {
            let _ = controller.handle_input(observation.into());
            for plane in PlaneId::ALL {
                prop_assert_eq!(controller.plane(plane).granted, OnAir::Off);
            }
        }
    }
}
