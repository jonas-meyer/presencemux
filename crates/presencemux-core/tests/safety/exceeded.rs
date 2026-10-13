use presencemux_core::{Cause, Command, OnAir, PlaneId};
use proptest::prelude::*;

use crate::support::{
    after, events, exceeding_report, hold_after_lowering, plane, preset_setting, ready_report,
};

proptest! {
    #[test]
    fn lowers_only_its_plane_with_output_exceeded(prefix in events(), exceeded in plane()) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(ready_report(exceeded).into());
        let before = controller.clone();

        let _ = controller.handle_input(exceeding_report(exceeded).into());

        prop_assert_eq!(controller.plane(exceeded).granted, OnAir::Off);
        prop_assert_eq!(
            controller.plane(exceeded).hold(),
            hold_after_lowering(before.plane(exceeded), Cause::OutputExceeded(exceeded))
        );
        for other in PlaneId::ALL.into_iter().filter(|&p| p != exceeded) {
            prop_assert_eq!(controller.plane(other), before.plane(other));
        }
    }

    #[test]
    fn keeps_its_plane_off_after_a_command_while_it_lasts(
        prefix in events(),
        (exceeded, command) in plane().prop_flat_map(|p| (Just(p), preset_setting(p))),
    ) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(exceeding_report(exceeded).into());

        let _ = controller.handle_input(Command::Apply(command).into());

        prop_assert_eq!(controller.plane(exceeded).granted, OnAir::Off);
    }
}
