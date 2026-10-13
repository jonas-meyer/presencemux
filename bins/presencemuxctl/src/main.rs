//! A command-line client for presencemuxd.

use std::io::{self, Write};
use std::path::PathBuf;
use std::pin::pin;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use presencemux_api::{
    Cause, ControllerProxy, Device, Exposure, Flow, Owner, OwnerStatus, Plane, Reason, ReportState,
    SOCKET_PATH, Status, age,
};
use rustix::time::{ClockId, clock_gettime};
use tokio_stream::StreamExt;

/// Controls the privacy of the PresenceMux camera and microphone.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// The Varlink socket of presencemuxd.
    #[arg(long, global = true, default_value = SOCKET_PATH)]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Shows the status.
    Status,
    /// Shows the status, then the status after each change.
    Watch,
    /// Applies `privacy` or a configured preset.
    Apply { preset: String },
    /// Sets the cam to `blank`, `live` or a slate name.
    Cam { value: String },
    /// Mutes or unmutes the mic.
    Mic { state: MicState },
    /// Applies the request of a held plane again.
    Resume { plane: PlaneArg },
    /// Lists the presets.
    Presets,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum PlaneArg {
    Cam,
    Mic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum MicState {
    #[value(alias = "mute")]
    Muted,
    #[value(alias = "unmute")]
    Unmuted,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("cannot connect to presencemuxd at {}: {source}", socket.display())]
    Connect {
        socket: PathBuf,
        source: zlink::Error,
    },
    #[error("the connection to presencemuxd failed: {0}")]
    Varlink(#[from] zlink::Error),
    #[error("presencemuxd: {0}")]
    Daemon(#[from] presencemux_api::Error),
    #[error("presencemuxd: {source} `{value}`")]
    Unknown {
        value: String,
        source: presencemux_api::Error,
    },
    #[error("cannot write the output: {0}")]
    Output(#[from] io::Error),
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        // The reader of the output stopped, for example `head`.
        Err(Error::Output(error)) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr, "presencemuxctl: {error}");
            if let Some(hint) = hint(&error) {
                let _ = writeln!(stderr, "presencemuxctl: {hint}");
            }
            ExitCode::FAILURE
        }
    }
}

/// Returns what the user can do about an error.
fn hint(error: &Error) -> Option<&'static str> {
    let cause = match error {
        Error::Unknown {
            source: presencemux_api::Error::NoSuchPreset,
            ..
        } => return Some("`presencemuxctl presets` lists the presets"),
        Error::Unknown {
            source: presencemux_api::Error::NoSuchSlate,
            ..
        } => return Some("use `blank`, `live` or a slate name from the configuration"),
        Error::Connect {
            source: zlink::Error::Io(cause),
            ..
        } => cause,
        _ => return None,
    };
    match cause.kind() {
        io::ErrorKind::PermissionDenied => {
            Some("add your user to the presencemux group, then log in again")
        }
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            Some("check `systemctl status presencemuxd.socket`")
        }
        _ => None,
    }
}

async fn run(cli: Cli) -> Result<(), Error> {
    let mut connection = zlink::tokio::unix::connect(&cli.socket)
        .await
        .map_err(|source| Error::Connect {
            socket: cli.socket.clone(),
            source,
        })?;
    let mut out = io::stdout().lock();
    match cli.command {
        Command::Status => write_status(&mut out, &connection.describe().await??, now_usec())?,
        Command::Watch => {
            let mut statuses = pin!(connection.subscribe_status().await?);
            while let Some(status) = statuses.next().await {
                write_status(&mut out, &status??, now_usec())?;
                writeln!(out)?;
                out.flush()?;
            }
        }
        Command::Apply { preset } => connection
            .apply_preset(&preset)
            .await?
            .map_err(naming(&preset))?,
        Command::Cam { value } => connection.set_cam(&value).await?.map_err(naming(&value))?,
        Command::Mic { state } => {
            connection.set_mic_mute(state == MicState::Muted).await??;
        }
        Command::Resume { plane } => {
            let plane = match plane {
                PlaneArg::Cam => Plane::Cam,
                PlaneArg::Mic => Plane::Mic,
            };
            connection.resume(plane).await??;
        }
        Command::Presets => {
            for preset in connection.list_presets().await??.presets {
                writeln!(out, "{}", preset.name)?;
            }
        }
    }
    Ok(())
}

/// Returns a conversion that names the value in an error about an unknown
/// preset or slate.
fn naming(value: &str) -> impl FnOnce(presencemux_api::Error) -> Error {
    move |source| match source {
        presencemux_api::Error::NoSuchPreset | presencemux_api::Error::NoSuchSlate => {
            Error::Unknown {
                value: value.to_owned(),
                source,
            }
        }
        other => Error::Daemon(other),
    }
}

/// Returns the time now on `CLOCK_MONOTONIC`, in microseconds.
fn now_usec() -> i64 {
    let now = clock_gettime(ClockId::Monotonic);
    now.tv_sec * 1_000_000 + now.tv_nsec / 1_000
}

/// Writes `status`. `now_usec` is the time on `CLOCK_MONOTONIC`, in
/// microseconds.
fn write_status(out: &mut impl Write, status: &Status, now_usec: i64) -> io::Result<()> {
    let Status {
        cam,
        mic,
        owners,
        host,
        health,
        preset,
    } = status;
    writeln!(out, "preset        {}", preset.as_deref().unwrap_or("-"))?;
    write!(
        out,
        "cam           requested {}, granted {}",
        cam.requested, cam.granted
    )?;
    write_actual(out, cam.actual.as_deref())?;
    write_state(out, cam.exposure, cam.flow, cam.reason, cam.hold)?;
    writeln!(out, ", stream {}", cam.stream)?;
    write!(
        out,
        "mic           requested {}, granted {}",
        mic.requested, mic.granted
    )?;
    write_actual(out, mic.actual.map(|actual| actual.to_string()).as_deref())?;
    write_state(out, mic.exposure, mic.flow, mic.reason, mic.hold)?;
    writeln!(out)?;
    writeln!(out, "host          {host}")?;
    writeln!(out, "health        {health}")?;
    for owner in owners {
        write_owner(out, owner, now_usec)?;
    }
    Ok(())
}

/// Writes the actual value of a plane.
fn write_actual(out: &mut impl Write, actual: Option<&str>) -> io::Result<()> {
    write!(out, ", actual {}", actual.unwrap_or("unknown"))
}

/// Writes the exposure of a plane while its flow is idle, and the reason why
/// the exposure differs from the request. A hold shows with its cause.
fn write_state(
    out: &mut impl Write,
    exposure: Option<Exposure>,
    flow: Option<Flow>,
    reason: Option<Reason>,
    hold: Option<Cause>,
) -> io::Result<()> {
    if let (Some(exposure), Some(Flow::Idle)) = (exposure, flow) {
        write!(out, ", idle at {exposure}")?;
    }
    match (reason, hold) {
        (Some(Reason::Held), Some(cause)) => write!(out, ", held by {cause}"),
        (Some(reason), _) => write!(out, ", {reason}"),
        (None, _) => Ok(()),
    }
}

/// Writes the line of `owner` with the age of each condition, and its message
/// on the next line.
fn write_owner(out: &mut impl Write, owner: &OwnerStatus, now_usec: i64) -> io::Result<()> {
    let name = match owner.owner {
        Owner::CamWorker => "cam worker",
        Owner::AudioWorker => "audio worker",
    };
    write!(out, "{name:<14}{}", owner.report)?;
    if owner.report == ReportState::Fresh {
        for condition in &owner.devices {
            let device = match condition.device {
                Device::Camera => "camera",
                Device::CamWorker => "worker",
                Device::AudioInterface => "audio interface",
                Device::Gate => "gate",
            };
            write!(
                out,
                ", {device} {} for {}",
                condition.status,
                age(now_usec - condition.since_usec)
            )?;
        }
    } else {
        write!(out, " for {}", age(now_usec - owner.since_usec))?;
    }
    writeln!(out)?;
    if let Some(message) = &owner.message {
        writeln!(out, "              {message}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use presencemux_api::{
        CamStatus, CamStream, DeviceCondition, DeviceStatus, Health, Host, MicStatus, Mute,
        Permission,
    };

    use super::*;

    #[test]
    fn command_line_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn permission_error_has_a_hint() {
        let error = Error::Connect {
            socket: PathBuf::from(SOCKET_PATH),
            source: zlink::Error::Io(io::ErrorKind::PermissionDenied.into()),
        };

        assert!(hint(&error).is_some_and(|hint| hint.contains("presencemux group")));
    }

    #[test]
    fn unknown_preset_error_names_the_preset() {
        let error = naming("tlak")(presencemux_api::Error::NoSuchPreset);

        assert_eq!(error.to_string(), "presencemuxd: no such preset `tlak`");
        assert!(hint(&error).is_some_and(|hint| hint.contains("presencemuxctl presets")));
    }

    #[test]
    fn socket_option_works_after_the_command() {
        let cli =
            Cli::try_parse_from(["presencemuxctl", "status", "--socket", "/tmp/control"]).unwrap();

        assert_eq!(cli.socket, PathBuf::from("/tmp/control"));
    }

    /// The time of the status in the tests, in microseconds.
    const NOW: i64 = 10_000_000_000;

    fn owner(
        owner: Owner,
        report: ReportState,
        age_usec: i64,
        devices: Vec<DeviceCondition>,
    ) -> OwnerStatus {
        OwnerStatus {
            owner,
            report,
            since_usec: NOW - age_usec,
            devices,
            message: None,
        }
    }

    fn condition(device: Device, status: DeviceStatus, age_usec: i64) -> DeviceCondition {
        DeviceCondition {
            device,
            status,
            since_usec: NOW - age_usec,
        }
    }

    #[test]
    fn status_shows_each_plane_and_each_owner() {
        let status = Status {
            cam: CamStatus {
                requested: "brb".to_owned(),
                granted: "blank".to_owned(),
                actual: Some("blank".to_owned()),
                exposure: Some(Exposure::Off),
                slate: None,
                flow: Some(Flow::Flowing),
                reason: Some(Reason::Held),
                hold: Some(Cause::CameraFault),
                resumable: false,
                stream: CamStream::Idle,
                blank_at_usec: None,
            },
            mic: MicStatus {
                requested: Mute::Muted,
                granted: Permission::Denied,
                actual: None,
                exposure: None,
                flow: None,
                reason: None,
                hold: None,
                resumable: false,
            },
            owners: vec![
                owner(
                    Owner::CamWorker,
                    ReportState::Fresh,
                    7_200_000_000,
                    vec![
                        condition(Device::CamWorker, DeviceStatus::Ready, 7_200_000_000),
                        condition(Device::Camera, DeviceStatus::Fault, 180_000_000),
                    ],
                ),
                owner(Owner::AudioWorker, ReportState::Lost, 5_000_000, vec![]),
            ],
            host: Host::Connected,
            health: Health::Fault,
            preset: Some("brb".to_owned()),
        };
        let mut out = Vec::new();

        write_status(&mut out, &status, NOW).unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "preset        brb\n\
             cam           requested brb, granted blank, actual blank, held by camera_fault, stream idle\n\
             mic           requested muted, granted denied, actual unknown\n\
             host          connected\n\
             health        fault\n\
             cam worker    fresh, worker ready for 2h, camera fault for 3m\n\
             audio worker  lost for 5s\n"
        );
    }

    #[test]
    fn plane_with_an_older_grant_shows_updating() {
        let mut out = Vec::new();

        write_state(
            &mut out,
            Some(Exposure::Off),
            Some(Flow::Flowing),
            Some(Reason::Updating),
            None,
        )
        .unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), ", updating");
    }

    #[test]
    fn idle_plane_shows_its_exposure() {
        let mut out = Vec::new();

        write_state(&mut out, Some(Exposure::Live), Some(Flow::Idle), None, None).unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), ", idle at live");
    }

    #[test]
    fn owner_message_shows_on_the_next_line() {
        let status = OwnerStatus {
            message: Some("rejected the lease: late".to_owned()),
            ..owner(Owner::CamWorker, ReportState::Lost, 5_000_000, vec![])
        };
        let mut out = Vec::new();

        write_owner(&mut out, &status, NOW).unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "cam worker    lost for 5s\n              rejected the lease: late\n"
        );
    }
}
