//! The notifications of the systemd service.

use std::time::{Duration, Instant};

use sd_notify::NotifyState;

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
