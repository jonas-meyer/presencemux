use presencemux_core::{Controller, Effects};
use proptest::prelude::*;

use crate::support::events;

/// The targets that the adapters have, built only from returned effects.
#[derive(Debug, PartialEq)]
struct Applied(Effects);

impl Applied {
    fn apply(&mut self, effects: Effects) {
        let Effects {
            cam,
            mic,
            status_changed: _,
        } = effects;
        self.0.cam = cam.or(self.0.cam);
        self.0.mic = mic.or(self.0.mic);
    }
}

proptest! {
    #[test]
    fn applied_always_equal_a_resync(events in events()) {
        let (mut controller, boot) = Controller::new();
        let mut applied = Applied(boot);
        prop_assert_eq!(&applied, &Applied(controller.resync()));

        for event in events {
            applied.apply(controller.handle_input(event));
            prop_assert_eq!(&applied, &Applied(controller.resync()));
        }
    }
}
