//! The audio worker of PresenceMux. Its mic gate passes the audio interface
//! to the gadget mic only under a lease from presencemuxd.

mod clock;
mod fade;
mod gate;
mod pipewire;
mod systemd;
mod varlink;

use std::cell::RefCell;
use std::convert::Infallible;
use std::future::pending;
use std::io;
use std::os::fd::OwnedFd;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

use crate::pipewire::RtState;
use crate::systemd::Watchdog;
use crate::varlink::{Control, Service, Shared};

/// The time between two checks of the lease.
const CHECK_PERIOD: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("cannot read the fds from systemd: {0}")]
    Fds(io::Error),
    #[error("cannot register the signal handlers: {0}")]
    Signal(io::Error),
    #[error(transparent)]
    PipeWire(#[from] pipewire::Error),
    #[error("cannot serve the Varlink socket: {0}")]
    Varlink(io::Error),
}

#[tokio::main(flavor = "local")]
async fn main() -> ExitCode {
    init_logging();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("presencemux-audio failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Error> {
    let mut terminate = signal(SignalKind::terminate()).map_err(Error::Signal)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(Error::Signal)?;
    let rt = Arc::new(RtState::default());
    let mut ended = pipewire::spawn(Arc::clone(&rt))?;
    let control: Shared = Rc::new(RefCell::new(Control::new(rt)));

    let mut fds = systemd::passed_fds().map_err(Error::Fds)?;
    let mut varlink = None;
    if let Some(fd) = fds.remove(systemd::VARLINK_FD_NAME) {
        let service = Service::new(Rc::clone(&control));
        let listener = listener(fd).map_err(Error::Varlink)?;
        varlink = Some(tokio::task::spawn_local(varlink::serve(
            listener,
            service,
            socket_gid(),
        )));
    } else {
        tracing::info!("systemd passed no Varlink socket, the gate stays closed");
    }
    systemd::ready();
    tracing::info!("presencemux-audio started");

    let mut watchdog = Watchdog::new();
    let mut checks = tokio::time::interval(CHECK_PERIOD);
    checks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            error = &mut ended => {
                return Err(Error::PipeWire(error.unwrap_or(pipewire::Error::Ended)));
            }
            error = failure(&mut varlink) => return Err(Error::Varlink(error)),
            _ = checks.tick() => {
                control.borrow_mut().check(clock::now());
                watchdog.ping();
            }
        }
    }

    systemd::stopping();
    tracing::info!("presencemux-audio stopped");
    Ok(())
}

/// Returns the Varlink socket in `fd` as a listener of the runtime.
fn listener(fd: OwnedFd) -> io::Result<UnixListener> {
    let listener = std::os::unix::net::UnixListener::from(fd);
    listener.set_nonblocking(true)?;
    UnixListener::from_std(listener)
}

/// Returns the group of the Varlink socket file, or `None` if the worker cannot
/// read it.
fn socket_gid() -> Option<u32> {
    match rustix::fs::stat(presencemux_api::audio::SOCKET_PATH) {
        Ok(stat) => Some(stat.st_gid),
        Err(error) => {
            tracing::warn!(
                "cannot read the group of {}, the worker serves root: {error}",
                presencemux_api::audio::SOCKET_PATH
            );
            None
        }
    }
}

/// Returns the error of the Varlink task when it ends. Waits forever without
/// a task.
async fn failure(task: &mut Option<JoinHandle<io::Result<Infallible>>>) -> io::Error {
    let Some(task) = task else {
        return pending().await;
    };
    match task.await {
        Ok(Err(error)) => error,
        Err(error) => io::Error::other(error),
    }
}

/// Logs to the journal under systemd, and to stderr otherwise. `RUST_LOG`
/// sets the level.
fn init_logging() {
    let under_systemd = std::env::var_os("JOURNAL_STREAM").is_some_and(|value| !value.is_empty());
    let (journald, journald_error) = match under_systemd.then(tracing_journald::layer).transpose() {
        Ok(layer) => (layer, None),
        Err(error) => (None, Some(error)),
    };
    let stderr = journald
        .is_none()
        .then(|| tracing_subscriber::fmt::layer().with_writer(io::stderr));
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::registry()
        .with(journald)
        .with(stderr)
        .with(filter)
        .init();
    if let Some(error) = journald_error {
        tracing::warn!("cannot log to the journal, logs go to stderr: {error}");
    }
}
