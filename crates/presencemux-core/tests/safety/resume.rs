use presencemux_core::{Command, Controller, DeviceStatus, HostStatus, PlaneId, Report, Reported};
use proptest::prelude::*;

use crate::support::{after, events, plane};

/// Returns whether the owner of the devices of `plane` has a fresh report with
/// every device ready and an output within its grant.
fn ready(controller: &Controller, plane: PlaneId) -> bool {
    fn ready_report<R: Report>(observed: &Reported<R>) -> bool {
        match observed {
            Reported::Fresh(report) => {
                !report.exceeded()
                    && report
                        .devices()
                        .into_iter()
                        .all(|(_, status)| status == DeviceStatus::Ready)
            }
            Reported::Pending | Reported::Lost => false,
        }
    }
    match plane {
        PlaneId::Cam => ready_report(controller.cam().observed()),
        PlaneId::Mic => ready_report(controller.mic().observed()),
    }
}

proptest! {
    #[test]
    fn clears_a_hold_only_with_ready_devices_and_a_connected_host(
        prefix in events(),
        resumed in plane(),
    ) {
        let mut controller = after(prefix);
        let before = controller.clone();

        let _ = controller.handle_input(Command::Resume(resumed).into());

        if before.plane(resumed).hold.is_some() && controller.plane(resumed).hold.is_none() {
            prop_assert_eq!(before.host(), HostStatus::Connected);
            prop_assert!(ready(&before, resumed));
        }
    }

    #[test]
    fn never_changes_the_other_plane(prefix in events(), resumed in plane()) {
        let mut controller = after(prefix);
        let before = controller.clone();

        let _ = controller.handle_input(Command::Resume(resumed).into());

        for other in PlaneId::ALL.into_iter().filter(|&p| p != resumed) {
            prop_assert_eq!(controller.plane(other), before.plane(other));
        }
    }
}
