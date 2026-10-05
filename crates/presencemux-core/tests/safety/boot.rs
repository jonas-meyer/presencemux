use presencemux_core::{
    Cam, CamPermission, Controller, Effects, Epoch, Mic, MicMute, OnAir, Plane, Preset,
    PrivacyPlane,
};

#[test]
fn applies_privacy_with_safe_targets() {
    let (controller, effects) = Controller::new();

    assert_eq!(
        Preset::PRIVACY,
        Preset {
            cam: Some(controller.cam().desired()),
            mic: Some(controller.mic().desired()),
        }
    );
    assert_eq!(controller.cam().desired(), CamPermission::Blank);
    assert_eq!(controller.mic().desired(), MicMute::Muted);
    assert_eq!(controller.epoch(), Epoch::BOOT);
    assert_eq!(
        effects,
        Effects {
            cam: Some(Cam::SAFE),
            mic: Some(Mic::SAFE),
            status_changed: false,
        }
    );
    assert_eq!(Cam::on_air(Cam::SAFE.permission), OnAir::Off);
    assert_eq!(Mic::on_air(Mic::SAFE.permission), OnAir::Off);
}
