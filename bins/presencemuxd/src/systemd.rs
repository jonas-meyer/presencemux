//! The socket and the notifications of the systemd service.

use std::io;
use std::time::Duration;

use sd_notify::NotifyState;
use tokio::net::UnixListener;

/// Takes the Varlink socket that systemd passes to the service.
pub(crate) fn listener() -> io::Result<Option<UnixListener>> {
    let Some(listener) = listenfd::ListenFd::from_env().take_unix_listener(0)? else {
        return Ok(None);
    };
    listener.set_nonblocking(true)?;
    UnixListener::from_std(listener).map(Some)
}

/// Returns how often to ping the watchdog, which is half of its timeout.
pub(crate) fn watchdog_period() -> Option<Duration> {
    sd_notify::watchdog_enabled().map(|timeout| timeout / 2)
}

pub(crate) fn ready() {
    notify(NotifyState::Ready);
}

pub(crate) fn stopping() {
    notify(NotifyState::Stopping);
}

pub(crate) fn watchdog() {
    notify(NotifyState::Watchdog);
}

fn notify(state: NotifyState<'_>) {
    if let Err(error) = sd_notify::notify(&[state]) {
        tracing::warn!("cannot notify systemd: {error}");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn socket_unit_listens_on_the_api_socket_path() {
        let unit = include_str!("../debian/presencemuxd.socket");
        let line = format!("ListenStream={}", presencemux_api::SOCKET_PATH);

        assert!(unit.lines().any(|unit_line| unit_line == line), "{unit}");
    }
}
