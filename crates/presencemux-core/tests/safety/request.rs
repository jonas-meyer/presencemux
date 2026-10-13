use presencemux_core::{Controller, PlaneId};
use proptest::prelude::*;

use crate::support::{events, held_level};

proptest! {
    /// The policy snapshot covers the rule that a live plane needs ready
    /// devices.
    #[test]
    fn is_never_exceeded(events in events()) {
        let (mut controller, _) = Controller::new();

        for event in events {
            let _ = controller.handle_input(event);
            for plane in PlaneId::ALL {
                let state = controller.plane(plane);
                prop_assert!(state.granted <= held_level(state), "{plane:?} after {event:?}");
            }
        }
    }
}
