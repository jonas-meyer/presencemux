use presencemux_core::PlaneId;
use proptest::prelude::*;

use crate::support::{after, events, observation};

proptest! {
    /// A device that starts can let a plane rise, but not above the earlier
    /// request.
    #[test]
    fn never_raises_a_plane(prefix in events(), next in observation()) {
        let mut controller = after(prefix);
        let before = controller.clone();

        let _ = controller.handle_input(next.into());

        for plane in PlaneId::ALL {
            prop_assert!(controller.plane(plane).requested <= before.plane(plane).requested);
            prop_assert!(controller.plane(plane).on_air <= before.plane(plane).requested);
        }
        prop_assert_eq!(controller.epoch(), before.epoch());
    }
}
