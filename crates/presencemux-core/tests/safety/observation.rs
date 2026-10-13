use presencemux_core::PlaneId;
use proptest::prelude::*;

use crate::support::{after, events, held_level, observation};

proptest! {
    /// A device that starts can let a plane rise up to the earlier request.
    #[test]
    fn never_raises_a_plane(prefix in events(), next in observation()) {
        let mut controller = after(prefix);
        let before = controller.clone();

        let _ = controller.handle_input(next.into());

        for plane in PlaneId::ALL {
            prop_assert!(held_level(controller.plane(plane)) <= held_level(before.plane(plane)));
            prop_assert!(controller.plane(plane).granted <= held_level(before.plane(plane)));
        }
        prop_assert_eq!(controller.epoch(), before.epoch());
    }
}
