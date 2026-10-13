//! A command-line client for presencemuxd.

use std::io::{self, Write};
use std::path::PathBuf;
use std::pin::pin;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use presencemux_api::{
    Cause, ControllerProxy, Device, Owner, OwnerStatus, PRESET_PRIVACY, Plane, ReportState,
    SOCKET_PATH, Status,
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
            writeln!(out, "{PRESET_PRIVACY}")?;
            for name in connection.list_presets().await??.presets {
                writeln!(out, "{name}")?;
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
    write_actual(out, cam.actual.as_deref(), cam.in_effect)?;
    write_latched(out, cam.latched)?;
    write!(out, ", stream {}", cam.stream)?;
    write_hold(out, cam.hold)?;
    write!(
        out,
        "mic           requested {}, granted {}",
        mic.requested, mic.granted
    )?;
    write_actual(
        out,
        mic.actual.map(|actual| actual.to_string()).as_deref(),
        mic.in_effect,
    )?;
    write_latched(out, mic.latched)?;
    write_hold(out, mic.hold)?;
    writeln!(out, "host          {host}")?;
    writeln!(out, "health        {health}")?;
    for owner in owners {
        write_owner(out, owner, now_usec)?;
    }
    Ok(())
}

/// Writes the actual value of a plane, and `updating` while the worker
/// applies an older grant.
fn write_actual(out: &mut impl Write, actual: Option<&str>, in_effect: bool) -> io::Result<()> {
    match actual {
        Some(actual) => {
            write!(out, ", actual {actual}")?;
            if !in_effect {
                write!(out, ", updating")?;
            }
            Ok(())
        }
        None => write!(out, ", actual unknown"),
    }
}

/// Writes `latched` if the worker blocks its output after a latch.
fn write_latched(out: &mut impl Write, latched: bool) -> io::Result<()> {
    if latched {
        write!(out, ", latched")?;
    }
    Ok(())
}

/// Writes the hold of a plane, if automation holds it, and ends the line.
fn write_hold(out: &mut impl Write, hold: Option<Cause>) -> io::Result<()> {
    if let Some(hold) = hold {
        write!(out, ", held by {hold}")?;
    }
    writeln!(out)
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

/// Returns `micros` as a short age in the largest whole unit: seconds,
/// minutes, hours or days.
fn age(micros: i64) -> String {
    let seconds = micros.max(0) / 1_000_000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
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
                in_effect: true,
                latched: true,
                hold: Some(Cause::CameraFault),
                stream: CamStream::Idle,
            },
            mic: MicStatus {
                requested: Mute::Muted,
                granted: Permission::Denied,
                actual: None,
                in_effect: false,
                latched: false,
                hold: None,
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
             cam           requested brb, granted blank, actual blank, latched, stream idle, held by camera_fault\n\
             mic           requested muted, granted denied, actual unknown\n\
             host          connected\n\
             health        fault\n\
             cam worker    fresh, worker ready for 2h, camera fault for 3m\n\
             audio worker  lost for 5s\n"
        );
    }

    #[test]
    fn actual_value_of_an_older_grant_shows_updating() {
        let mut out = Vec::new();

        write_actual(&mut out, Some("live"), false).unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), ", actual live, updating");
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

    #[test]
    fn age_of_59_s_shows_in_seconds() {
        assert_eq!(age(59_999_999), "59s");
    }

    #[test]
    fn age_of_90_min_shows_in_hours() {
        assert_eq!(age(5_400_000_000), "1h");
    }

    #[test]
    fn age_of_a_condition_from_the_future_shows_as_0_s() {
        assert_eq!(age(-1), "0s");
    }
}
