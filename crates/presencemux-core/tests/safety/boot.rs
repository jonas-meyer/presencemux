use presencemux_core::{Cam, Controller, Effects, Epoch, Mic, OnAir, Plane, Preset, PrivacyPlane};

#[test]
fn applies_privacy() {
    let (controller, _) = Controller::new();

    assert_eq!(
        Preset {
            cam: Some(controller.cam().desired()),
            mic: Some(controller.mic().desired()),
        },
        Preset::PRIVACY
    );
    assert_eq!(controller.epoch(), Epoch::BOOT);
}

#[test]
fn effects_are_the_safe_targets() {
    let (_, effects) = Controller::new();

    assert_eq!(
        effects,
        Effects {
            cam: Some(Cam::SAFE),
            mic: Some(Mic::SAFE),
            status_changed: false,
        }
    );
}

#[test]
fn safe_targets_are_off() {
    assert_eq!(Cam::on_air(Cam::SAFE.permission), OnAir::Off);
    assert_eq!(Mic::on_air(Mic::SAFE.permission), OnAir::Off);
}
