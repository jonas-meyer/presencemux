use core::fmt::Write;

use presencemux_core::{
    CamPermission, CamReport, CamStream, Cause, Command, Controller, DeviceStatus, MicMute,
    MicReport, Observation, PlaneId, Preset, Report, Reported, SlateId,
};

use crate::support::{STATUSES, after};

/// Returns a controller after `report`, then `preset`. A report after the
/// preset can lower the plane.
fn controller(report: Observation, preset: Preset) -> Controller {
    after([report.into(), Command::Apply(preset).into()])
}

fn cam_report(worker: DeviceStatus, camera: DeviceStatus) -> CamReport {
    CamReport {
        worker,
        camera,
        stream: CamStream::Idle,
        latched: None,
    }
}

fn mic_report(gate: DeviceStatus, interface: DeviceStatus) -> MicReport {
    MicReport {
        gate,
        interface,
        latched: None,
    }
}

/// Returns `Lost`, then a report for each combination of device statuses.
fn reports<R>(report: impl Fn(DeviceStatus, DeviceStatus) -> R) -> Vec<Reported<R>> {
    let mut reports = vec![Reported::Lost];
    for first in STATUSES {
        for second in STATUSES {
            reports.push(Reported::Fresh(report(first, second)));
        }
    }
    reports
}

/// Returns `reported` as the observation for `plane`.
fn observation<R: Into<Observation>>(plane: PlaneId, reported: Reported<R>) -> Observation {
    match reported {
        Reported::Fresh(report) => report.into(),
        Reported::Lost => Observation::Lost(plane),
        Reported::Pending => panic!("the cause tables have no pending report"),
    }
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
                    Observation::Cam(cam_report(worker, camera)),
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
                    Observation::Mic(mic_report(gate, interface)),
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

/// Returns the text of `reported` for the cause tables. A fresh report gives
/// its device statuses in the order of [`Report::devices`].
fn describe<R: Report>(reported: &Reported<R>) -> String {
    match reported {
        Reported::Fresh(report) => {
            let [(_, first), (_, second)] = report.devices();
            format!("{first:?}, {second:?}")
        }
        Reported::Pending => "Pending".to_owned(),
        Reported::Lost => "Lost".to_owned(),
    }
}

/// Returns the text of a cause for the cause tables.
fn cause(cause: Option<Cause>) -> String {
    cause.map_or_else(|| "-".to_owned(), |cause| format!("{cause:?}"))
}

#[test]
fn cam_cause() {
    let live = Preset {
        cam: Some(CamPermission::Live),
        mic: None,
    };
    let mut table = String::from("before (worker, camera) | after | cause\n");
    let befores = [
        Reported::Lost,
        Reported::Fresh(cam_report(DeviceStatus::Missing, DeviceStatus::Missing)),
        Reported::Fresh(cam_report(DeviceStatus::Ready, DeviceStatus::Ready)),
    ];
    for before in befores {
        for after in reports(cam_report) {
            let mut controller = controller(observation(PlaneId::Cam, before), live);
            let _ = controller.handle_input(observation(PlaneId::Cam, after).into());
            writeln!(
                table,
                "{} | {} | {}",
                describe(&before),
                describe(&after),
                cause(controller.cam().cause())
            )
            .unwrap();
        }
    }
    insta::assert_snapshot!(table);
}

#[test]
fn mic_cause() {
    let unmuted = Preset {
        cam: None,
        mic: Some(MicMute::Unmuted),
    };
    let mut table = String::from("before (gate, interface) | after | cause\n");
    let befores = [
        Reported::Lost,
        Reported::Fresh(mic_report(DeviceStatus::Missing, DeviceStatus::Missing)),
        Reported::Fresh(mic_report(DeviceStatus::Ready, DeviceStatus::Ready)),
    ];
    for before in befores {
        for after in reports(mic_report) {
            let mut controller = controller(observation(PlaneId::Mic, before), unmuted);
            let _ = controller.handle_input(observation(PlaneId::Mic, after).into());
            writeln!(
                table,
                "{} | {} | {}",
                describe(&before),
                describe(&after),
                cause(controller.mic().cause())
            )
            .unwrap();
        }
    }
    insta::assert_snapshot!(table);
}
