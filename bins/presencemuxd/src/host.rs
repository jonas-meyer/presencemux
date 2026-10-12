//! The watch of the host status, from the state of the USB device controller
//! (UDC).

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use presencemux_api::Host;
use presencemux_core::{HostStatus, Observation};
use rustix::event::{PollFd, PollFlags, Timespec};
use tokio::io::unix::AsyncFd;
use tokio::io::{Interest, Ready};
use tokio::time;

use crate::event_loop::{Handle, HandleError};

/// The directory of the UDCs in sysfs.
const UDC_CLASS: &str = "/sys/class/udc";
/// The time between two attempts to open the state file of the UDC.
const RETRY: Duration = Duration::from_secs(1);
/// The longest time between two reads of the state.
const RECHECK: Duration = Duration::from_secs(5);

/// Sends the host status to the event loop at start and after each change.
/// When it cannot read the state of the UDC, sends `Disconnected` and
/// tries again after [`RETRY`]. Returns when the event loop stops.
pub(crate) async fn watch(handle: Handle) {
    let mut last = None;
    let mut last_error = None;
    loop {
        let error = match open(Path::new(UDC_CLASS)) {
            Ok(file) => {
                last_error = None;
                match follow(&handle, &file, &mut last).await {
                    Ok(Stop) => return,
                    Err(error) => error,
                }
            }
            Err(error) => error,
        };
        let message = error.to_string();
        if last_error.as_ref() != Some(&message) {
            tracing::warn!("host: cannot read the UDC state: {message}");
            last_error = Some(message);
        }
        let sent = send(&handle, &mut last, HostStatus::Disconnected, "no state").await;
        if sent.is_err() {
            return;
        }
        time::sleep(RETRY).await;
    }
}

/// The end of the watch because the event loop stopped.
struct Stop;

/// Opens the state file of the first UDC in `class`.
fn open(class: &Path) -> io::Result<AsyncFd<File>> {
    let path = state_path(class)?;
    let file = AsyncFd::with_interest(File::open(&path)?, Interest::PRIORITY)?;
    tracing::info!("host: reads the UDC state from {}", path.display());
    Ok(file)
}

/// Reads the state after each change and at least every [`RECHECK`], and
/// sends each new host status. Returns an error when a read or a wait fails.
async fn follow(
    handle: &Handle,
    file: &AsyncFd<File>,
    last: &mut Option<HostStatus>,
) -> io::Result<Stop> {
    let mut announced = false;
    loop {
        let state = read_state(file.get_ref())?;
        let status = host_status(&state);
        if hides_a_disconnect(*last, status, announced) {
            let sent = send(handle, last, HostStatus::Disconnected, "between two reads").await;
            if sent.is_err() {
                return Ok(Stop);
            }
        }
        if send(handle, last, status, &state).await.is_err() {
            return Ok(Stop);
        }
        announced = wait_for_change(file).await?;
    }
}

/// Waits for a change of the state, at most [`RECHECK`]. Returns whether the
/// kernel announced a change since the last read, after a wakeup and after a
/// timeout.
async fn wait_for_change(file: &AsyncFd<File>) -> io::Result<bool> {
    if let Ok(guard) = time::timeout(RECHECK, file.ready(Interest::PRIORITY)).await {
        guard?.clear_ready_matching(Ready::PRIORITY);
    }
    change_announced(file.get_ref())
}

/// Returns whether the kernel announced a change of the state since the last
/// read of `file`, without a wait.
fn change_announced(file: &File) -> io::Result<bool> {
    let mut fds = [PollFd::new(file, PollFlags::PRI)];
    rustix::event::poll(&mut fds, Some(&Timespec::default()))?;
    Ok(fds[0].revents().contains(PollFlags::PRI))
}

/// Returns whether a disconnect happened between two reads.
fn hides_a_disconnect(last: Option<HostStatus>, status: HostStatus, announced: bool) -> bool {
    announced && last == Some(status) && status != HostStatus::Disconnected
}

/// Sends `status` if it differs from `last`, and logs the new status with
/// `detail`.
async fn send(
    handle: &Handle,
    last: &mut Option<HostStatus>,
    status: HostStatus,
    detail: &str,
) -> Result<(), HandleError> {
    if *last == Some(status) {
        return Ok(());
    }
    tracing::info!("host: {} ({detail})", Host::from(status));
    *last = Some(status);
    handle.observe(Observation::Host(status)).await
}

/// Returns the path of the state file of the first UDC by name in `class`.
fn state_path(class: &Path) -> io::Result<PathBuf> {
    let mut names: Vec<_> = class
        .read_dir()?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<_>>()?;
    names.sort();
    let name = names.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} has no UDC", class.display()),
        )
    })?;
    Ok(class.join(name).join("state"))
}

/// Returns the content of `file` from offset 0, without whitespace at the
/// start and the end.
fn read_state(file: &File) -> io::Result<String> {
    let mut buffer = [0; 64];
    let len = file.read_at(&mut buffer, 0)?;
    Ok(String::from_utf8_lossy(&buffer[..len]).trim().to_owned())
}

/// Returns the host status for a state of the UDC.
fn host_status(state: &str) -> HostStatus {
    match state {
        "configured" => HostStatus::Connected,
        "suspended" => HostStatus::Suspended,
        _ => HostStatus::Disconnected,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Creates a state file with `content` and opens it.
    fn state_file(content: &str) -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, content).unwrap();
        let file = File::open(&path).unwrap();
        (dir, file)
    }

    #[test]
    fn configured_state_gives_connected() {
        assert_eq!(host_status("configured"), HostStatus::Connected);
    }

    #[test]
    fn suspended_state_gives_suspended() {
        assert_eq!(host_status("suspended"), HostStatus::Suspended);
    }

    #[test]
    fn addressed_state_gives_disconnected() {
        assert_eq!(host_status("addressed"), HostStatus::Disconnected);
    }

    #[test]
    fn not_attached_state_gives_disconnected() {
        assert_eq!(host_status("not attached"), HostStatus::Disconnected);
    }

    #[test]
    fn unknown_state_gives_disconnected() {
        assert_eq!(host_status("reconnecting"), HostStatus::Disconnected);
    }

    #[test]
    fn empty_state_gives_disconnected() {
        assert_eq!(host_status(""), HostStatus::Disconnected);
    }

    #[test]
    fn announced_change_to_the_same_connected_status_hides_a_disconnect() {
        let hidden = hides_a_disconnect(Some(HostStatus::Connected), HostStatus::Connected, true);

        assert!(hidden);
    }

    #[test]
    fn same_connected_status_without_an_announced_change_hides_no_disconnect() {
        let hidden = hides_a_disconnect(Some(HostStatus::Connected), HostStatus::Connected, false);

        assert!(!hidden);
    }

    #[test]
    fn announced_change_to_another_status_hides_no_disconnect() {
        let hidden =
            hides_a_disconnect(Some(HostStatus::Disconnected), HostStatus::Connected, true);

        assert!(!hidden);
    }

    #[test]
    fn announced_change_while_disconnected_hides_no_disconnect() {
        let hidden = hides_a_disconnect(
            Some(HostStatus::Disconnected),
            HostStatus::Disconnected,
            true,
        );

        assert!(!hidden);
    }

    #[test]
    fn read_state_drops_the_newline() {
        let (_dir, file) = state_file("configured\n");

        let state = read_state(&file).unwrap();

        assert_eq!(state, "configured");
    }

    #[test]
    fn read_state_reads_from_the_start_each_time() {
        let (_dir, file) = state_file("configured\n");

        let states = [read_state(&file).unwrap(), read_state(&file).unwrap()];

        assert_eq!(states, ["configured", "configured"]);
    }

    #[test]
    fn state_path_is_in_the_first_udc_by_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("b.usb")).unwrap();
        fs::create_dir(dir.path().join("a.usb")).unwrap();

        let path = state_path(dir.path()).unwrap();

        assert_eq!(path, dir.path().join("a.usb").join("state"));
    }

    #[test]
    fn class_without_a_udc_has_no_state_path() {
        let dir = tempfile::tempdir().unwrap();

        let result = state_path(dir.path());

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}
