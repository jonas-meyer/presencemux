//! The policy daemon of PresenceMux.

mod config;
mod event_loop;
mod status;
mod systemd;
mod varlink;

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

use crate::config::Config;
use crate::event_loop::EventLoop;
use crate::varlink::Service;

const CONFIG: &str = "/etc/presencemux/config.toml";

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Config(#[from] config::Error),
    #[error("systemd did not pass the Varlink socket")]
    NoSocket,
    #[error("cannot use the Varlink socket: {0}")]
    Socket(std::io::Error),
    #[error("cannot register the signal handlers: {0}")]
    Signal(std::io::Error),
    #[error("cannot accept Varlink connections: {0}")]
    Accept(std::io::Error),
    #[error("the event loop stopped")]
    EventLoopStopped,
}

#[tokio::main(flavor = "local")]
async fn main() -> ExitCode {
    init_logging();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("presencemuxd failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Error> {
    let config = Arc::new(Config::load(Path::new(CONFIG))?);
    let listener = systemd::listener()
        .map_err(Error::Socket)?
        .ok_or(Error::NoSocket)?;
    let (event_loop, handle, outputs) = EventLoop::new(Arc::clone(&config));
    let event_loop = event_loop.with_watchdog(systemd::watchdog_period());
    let service = Service::new(handle, outputs.status, config);
    let mut terminate = signal(SignalKind::terminate()).map_err(Error::Signal)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(Error::Signal)?;

    systemd::ready();
    tracing::info!("presencemuxd started in Privacy");

    // The event loop runs in this `select!`. If it ends or panics, the daemon
    // exits, and systemd starts it again in Privacy.
    tokio::select! {
        biased;
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
        () = event_loop.run() => return Err(Error::EventLoopStopped),
        result = varlink::serve(listener, service, varlink::MAX_CONNECTIONS) => {
            let Err(error) = result;
            return Err(Error::Accept(error));
        }
    }

    // The leases at the media layer expire after this.
    systemd::stopping();
    tracing::info!("presencemuxd stopped");
    Ok(())
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
        .then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr));
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::registry()
        .with(journald)
        .with(stderr)
        .with(filter)
        .init();
    if let Some(error) = journald_error {
        tracing::warn!("cannot log to the journal, using stderr: {error}");
    }
}
