use presencemux_core::{Cause, DeviceStatus, Observation, OnAir, PlaneId};
use proptest::prelude::*;

use crate::support::{after, cause_after_lowering, device, events, not_ready, observations};

proptest! {
    #[test]
    fn lowers_only_its_plane(
        prefix in events(),
        failed in device(),
        status in not_ready(),
    ) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(Observation::Device(failed, DeviceStatus::Ready).into());
        let before = controller.clone();

        let _ = controller.handle_input(Observation::Device(failed, status).into());

        let lowered = controller.plane(failed.plane());
        prop_assert_eq!(lowered.requested, OnAir::Off);
        prop_assert_eq!(
            lowered.cause,
            cause_after_lowering(before.plane(failed.plane()), Cause::DeviceFault(failed))
        );
        for other in PlaneId::ALL.into_iter().filter(|&p| p != failed.plane()) {
            prop_assert_eq!(controller.plane(other), before.plane(other));
        }
    }

    #[test]
    fn keeps_its_plane_off_until_a_command(
        prefix in events(),
        failed in device(),
        later in observations(),
    ) {
        let mut controller = after(prefix);
        let _ = controller.handle_input(Observation::Device(failed, DeviceStatus::Ready).into());
        let _ = controller.handle_input(Observation::Device(failed, DeviceStatus::Fault).into());

        for observation in later {
            let _ = controller.handle_input(observation.into());
            prop_assert_eq!(controller.plane(failed.plane()).on_air, OnAir::Off);
        }
    }
}
