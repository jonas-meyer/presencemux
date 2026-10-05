use core::fmt::Write;

use presencemux_core::{
    CamPermission, Command, Controller, Device, DeviceStatus, MicMute, Observation, Preset, SlateId,
};

use crate::support::{STATUSES, after};

/// Returns a controller with the given device statuses, then the preset. A
/// device change after the preset would lower it.
fn controller(devices: &[(Device, DeviceStatus)], preset: Preset) -> Controller {
    let observations = devices
        .iter()
        .map(|&(device, status)| Observation::Device(device, status).into());
    after(observations.chain([Command::Apply(preset).into()]))
}

#[test]
fn cam() {
    let mut table = String::from("desired | camera | worker | effective\n");
    for desired in [
        CamPermission::Blank,
        CamPermission::Slate(SlateId(0)),
        CamPermission::Live,
    ] {
        for camera in STATUSES {
            for worker in STATUSES {
                let controller = controller(
                    &[(Device::Camera, camera), (Device::CamWorker, worker)],
                    Preset {
                        cam: Some(desired),
                        mic: None,
                    },
                );
                let effective = controller.cam().target().permission;
                writeln!(
                    table,
                    "{desired:?} | {camera:?} | {worker:?} | {effective:?}"
                )
                .unwrap();
            }
        }
    }
    insta::assert_snapshot!(table);
}

#[test]
fn mic() {
    let mut table = String::from("desired | interface | gate | effective\n");
    for desired in [MicMute::Muted, MicMute::Unmuted] {
        for interface in STATUSES {
            for gate in STATUSES {
                let controller = controller(
                    &[(Device::AudioInterface, interface), (Device::Gate, gate)],
                    Preset {
                        cam: None,
                        mic: Some(desired),
                    },
                );
                let effective = controller.mic().target().permission;
                writeln!(
                    table,
                    "{desired:?} | {interface:?} | {gate:?} | {effective:?}"
                )
                .unwrap();
            }
        }
    }
    insta::assert_snapshot!(table);
}
