//! The notifications of the systemd service and the socket that systemd
//! passes.

#![expect(unsafe_code, reason = "an OwnedFd from the fds that systemd passes")]

use std::collections::HashMap;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sd_notify::NotifyState;

/// The name of the Varlink socket in `presencemux-audio.socket`.
pub(crate) const VARLINK_FD_NAME: &str = "varlink";

/// Takes the fds that systemd passes to the service, by name. A second call
/// returns no fds.
pub(crate) fn passed_fds() -> io::Result<HashMap<String, OwnedFd>> {
    static TAKEN: AtomicBool = AtomicBool::new(false);
    let mut fds = HashMap::new();
    if TAKEN.swap(true, Ordering::SeqCst) {
        return Ok(fds);
    }
    for (fd, name) in sd_notify::listen_fds_with_names()? {
        // SAFETY: systemd passes these fds to this process. `TAKEN` makes this
        // the only call that reads them, and no other code reads `LISTEN_FDS`.
        fds.insert(name, unsafe { OwnedFd::from_raw_fd(fd) });
    }
    Ok(fds)
}

pub(crate) fn ready() {
    notify(NotifyState::Ready);
}

pub(crate) fn stopping() {
    notify(NotifyState::Stopping);
}

/// The systemd watchdog. It sends at most one ping in each half of the
/// watchdog timeout.
#[derive(Debug)]
pub(crate) struct Watchdog {
    period: Option<Duration>,
    last: Option<Instant>,
}

impl Watchdog {
    pub(crate) fn new() -> Self {
        Self {
            period: sd_notify::watchdog_enabled().map(|timeout| timeout / 2),
            last: None,
        }
    }

    pub(crate) fn ping(&mut self) {
        let Some(period) = self.period else {
            return;
        };
        let now = Instant::now();
        if self.last.is_none_or(|last| now - last >= period) {
            notify(NotifyState::Watchdog);
            self.last = Some(now);
        }
    }
}

fn notify(state: NotifyState<'_>) {
    if let Err(error) = sd_notify::notify(&[state]) {
        tracing::warn!("cannot notify systemd: {error}");
    }
}
