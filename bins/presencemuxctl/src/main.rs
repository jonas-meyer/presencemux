//! A command-line client for presencemuxd.

use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::pin::pin;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use presencemux_api::{Cause, ControllerProxy, PRESET_PRIVACY, SOCKET_PATH, Status};
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
    /// Lists the presets.
    Presets,
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
        Command::Status => write_status(&mut out, &connection.describe().await??)?,
        Command::Watch => {
            let mut statuses = pin!(connection.subscribe_status().await?);
            while let Some(status) = statuses.next().await {
                write_status(&mut out, &status??)?;
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

fn write_status(out: &mut impl Write, status: &Status) -> io::Result<()> {
    let Status {
        cam,
        mic,
        host,
        health,
        preset,
    } = status;
    writeln!(out, "preset  {}", preset.as_deref().unwrap_or("-"))?;
    write_plane(out, "cam", &cam.desired, &cam.effective, cam.cause)?;
    write_plane(out, "mic", &mic.desired, &mic.effective, mic.cause)?;
    writeln!(out, "host    {host}")?;
    writeln!(out, "health  {health}")
}

fn write_plane(
    out: &mut impl Write,
    name: &str,
    desired: &impl fmt::Display,
    effective: &impl fmt::Display,
    cause: Option<Cause>,
) -> io::Result<()> {
    write!(out, "{name:<8}desired {desired}, effective {effective}")?;
    if let Some(cause) = cause {
        write!(out, ", lowered by {cause}")?;
    }
    writeln!(out)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use presencemux_api::{CamStatus, Health, Host, MicStatus, Mute, Permission};

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

    #[test]
    fn status_shows_each_plane_with_its_cause() {
        let status = Status {
            cam: CamStatus {
                desired: "brb".to_owned(),
                effective: "blank".to_owned(),
                cause: Some(Cause::CameraFault),
            },
            mic: MicStatus {
                desired: Mute::Muted,
                effective: Permission::Denied,
                cause: None,
            },
            host: Host::Connected,
            health: Health::Degraded,
            preset: Some("brb".to_owned()),
        };
        let mut out = Vec::new();

        write_status(&mut out, &status).unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "preset  brb\n\
             cam     desired brb, effective blank, lowered by camera_fault\n\
             mic     desired muted, effective denied\n\
             host    connected\n\
             health  degraded\n"
        );
    }
}
