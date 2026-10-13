use presencemux_core::{Cause, DeviceStatus, OnAir, PlaneId};
use proptest::prelude::*;

use crate::support::{
    after, device, events, hold_after_lowering, not_fault, not_ready, observations, report_with,
};

proptest! {
    #[test]
    fn lowers_only_its_plane(
        prefix in events(),
        failed in device(),
        status in not_ready(),
    ) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(report_with(failed, DeviceStatus::Ready).into());
        let before = controller.clone();

        let _ = controller.handle_input(report_with(failed, status).into());

        let lowered = controller.plane(failed.plane());
        prop_assert_eq!(lowered.granted, OnAir::Off);
        prop_assert_eq!(
            lowered.hold(),
            hold_after_lowering(before.plane(failed.plane()), Cause::DeviceFault(failed))
        );
        for other in PlaneId::ALL.into_iter().filter(|&p| p != failed.plane()) {
            prop_assert_eq!(controller.plane(other), before.plane(other));
        }
    }

    #[test]
    fn new_fault_lowers_its_plane_from_any_status(
        prefix in events(),
        failed in device(),
        before in not_fault(),
    ) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(report_with(failed, before).into());

        let _ = controller.handle_input(report_with(failed, DeviceStatus::Fault).into());

        prop_assert_eq!(controller.plane(failed.plane()).granted, OnAir::Off);
    }

    #[test]
    fn keeps_its_plane_off_until_a_command(
        prefix in events(),
        failed in device(),
        later in observations(),
    ) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(report_with(failed, DeviceStatus::Ready).into());
        let _ = controller.handle_input(report_with(failed, DeviceStatus::Fault).into());

        for observation in later {
            let _ = controller.handle_input(observation.into());
            prop_assert_eq!(controller.plane(failed.plane()).granted, OnAir::Off);
        }
    }
}
