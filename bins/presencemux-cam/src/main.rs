//! The cam worker of PresenceMux. It sends live video, a slate or blank to the
//! host through the UVC gadget function.

mod capture;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "only the tests use frame choice until the lease exists"
    )
)]
mod choice;
mod discovery;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "only the tests use the requantizer until frame choice and the lease exist"
    )
)]
mod live;
mod output;
mod systemd;
mod uvc;
mod v4l2;

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use rustix::fs::{Mode, OFlags};
use tokio::io::unix::AsyncFd;
use tokio::io::{Interest, Ready};
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::MissedTickBehavior;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

use crate::capture::{CameraStatus, Capture};
use crate::discovery::Discovery;
use crate::output::{Frame, FrameId, Output};
use crate::systemd::Watchdog;
use crate::uvc::{Action, Input, Limits, Uvc};
use crate::v4l2::Queue;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
/// `streaming_maxpacket` of the gadget.
const MAX_PAYLOAD: u32 = 2048;
/// The time between two frames at 30 fps.
const FRAME_PERIOD: Duration = Duration::from_nanos(33_333_333);
/// The interval between two watchdog pings while the worker waits for the
/// gadget node.
const PING_PERIOD: Duration = Duration::from_millis(200);

const BLANK: Frame<'static> = Frame {
    id: FrameId(0),
    jpeg: include_bytes!("../assets/blank.jpg"),
};

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("cannot read the fds from systemd: {0}")]
    FdStore(io::Error),
    #[error("cannot watch the V4L2 nodes with udev: {0}")]
    Discovery(io::Error),
    #[error("cannot start the capture thread: {0}")]
    Capture(io::Error),
    #[error("the capture thread has ended")]
    CaptureEnded,
    #[error("cannot open the gadget node {}: {source}", path.display())]
    Open { path: PathBuf, source: io::Error },
    #[error("cannot prepare the gadget node: {0}")]
    Prepare(io::Error),
    #[error("cannot use the gadget node: {0}")]
    Node(io::Error),
    #[error("cannot read the events of the gadget node: {0}")]
    Events(io::Error),
    #[error("cannot register the signal handlers: {0}")]
    Signal(io::Error),
}

#[tokio::main(flavor = "local")]
async fn main() -> ExitCode {
    init_logging();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("presencemux-cam failed: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Error> {
    let mut terminate = signal(SignalKind::terminate()).map_err(Error::Signal)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(Error::Signal)?;
    systemd::ready();
    let mut watchdog = Watchdog::new();
    let mut discovery = Discovery::new().map_err(Error::Discovery)?;
    let capture = Capture::spawn().map_err(Error::Capture)?;
    capture.set_node(discovery.nodes().camera());

    let mut fds = systemd::passed_fds().map_err(Error::FdStore)?;
    let stored = fds.remove(systemd::UVC_FD_NAME);
    let adopted = stored.is_some();
    let node = if let Some(node) = stored {
        node
    } else {
        let opened = tokio::select! {
            biased;
            _ = terminate.recv() => None,
            _ = interrupt.recv() => None,
            node = open_node(&mut discovery, &capture, &mut watchdog) => Some(node?),
        };
        let Some(node) = opened else {
            systemd::stopping();
            return Ok(());
        };
        prepare_node(node.as_fd()).map_err(Error::Prepare)?;
        systemd::store_node(&node);
        node
    };
    let limits = Limits {
        max_frame_size: v4l2::frame_size(node.as_fd()).map_err(Error::Node)?,
        max_payload: MAX_PAYLOAD,
    };
    let output = if adopted {
        Output::adopt(node.as_fd()).map_err(Error::Node)?
    } else {
        None
    };
    let mut worker = Worker {
        uvc: if output.is_some() {
            Uvc::with_stream(limits)
        } else {
            Uvc::new(limits)
        },
        output,
        start_pending: false,
        frame_error: None,
        camera: capture.status(v4l2::monotonic_now()),
    };
    tracing::info!(
        "presencemux-cam started, adopted: {adopted}, streaming: {}, camera: {}",
        worker.output.is_some(),
        worker.camera
    );

    // Events can wait in the stored gadget node from before a restart.
    worker.handle_events(node.as_fd()).map_err(Error::Events)?;
    let node = AsyncFd::with_interest(node, Interest::PRIORITY).map_err(Error::Node)?;
    let mut frames = tokio::time::interval(FRAME_PERIOD);
    frames.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            guard = node.ready(Interest::PRIORITY) => {
                let mut guard = guard.map_err(Error::Events)?;
                worker.handle_events(node.get_ref().as_fd()).map_err(Error::Events)?;
                guard.clear_ready_matching(Ready::PRIORITY);
            }
            changed = discovery.changed() => {
                changed.map_err(Error::Discovery)?;
                capture.set_node(discovery.nodes().camera());
            }
            _ = frames.tick() => {
                if capture.has_ended() {
                    return Err(Error::CaptureEnded);
                }
                worker.send_frame(node.get_ref().as_fd());
                worker.check_camera(&capture);
                watchdog.ping();
            }
        }
    }

    systemd::stopping();
    tracing::info!("presencemux-cam stopped");
    Ok(())
}

/// The protocol state and the stream of the gadget node.
struct Worker {
    uvc: Uvc,
    output: Option<Output>,
    /// Whether the host waits for a stream that did not start yet. The frame
    /// timer tries the start again.
    start_pending: bool,
    /// The last error of the stream. The worker logs an error only when it
    /// differs from this one.
    frame_error: Option<String>,
    /// The last status of the camera.
    camera: CameraStatus,
}

impl Worker {
    /// Reads and handles each event that waits on the gadget node.
    fn handle_events(&mut self, fd: BorrowedFd<'_>) -> io::Result<()> {
        while let Some(event) = v4l2::dequeue_event(fd)? {
            let Some(input) = Input::parse(event.kind, event.payload()) else {
                continue;
            };
            match input {
                Input::Connect => tracing::info!("host connected"),
                Input::Disconnect => tracing::info!("host disconnected"),
                _ => {}
            }
            if let Some(action) = self.uvc.handle(input) {
                self.perform(fd, action);
            }
        }
        Ok(())
    }

    fn perform(&mut self, fd: BorrowedFd<'_>, action: Action) {
        match action {
            Action::Respond(response) => {
                let (length, data) = response.wire();
                if let Err(error) = v4l2::send_response(fd, length, data) {
                    tracing::warn!("cannot answer a setup request: {error}");
                }
            }
            Action::StartStream => self.start_stream(fd),
            Action::StopStream => {
                self.start_pending = false;
                if let Some(output) = self.output.take()
                    && let Err(error) = output.stop(fd)
                {
                    tracing::warn!("cannot stop the stream: {error}");
                }
                tracing::info!("stream stopped");
            }
        }
    }

    /// Starts the stream and queues the first frame. On an error, the stream
    /// start stays pending.
    fn start_stream(&mut self, fd: BorrowedFd<'_>) {
        match Output::start(fd) {
            Ok(output) => {
                self.output = Some(output);
                self.start_pending = false;
                self.frame_error = None;
                tracing::info!("stream started");
                self.send_frame(fd);
            }
            Err(error) => {
                self.start_pending = true;
                self.log_error(&format!("cannot start the stream: {error}"));
            }
        }
    }

    /// Queues the next frame, if a stream runs and a buffer is free. Tries a
    /// pending stream start again first.
    fn send_frame(&mut self, fd: BorrowedFd<'_>) {
        if self.start_pending {
            self.start_stream(fd);
            return;
        }
        let Some(output) = &mut self.output else {
            return;
        };
        match output
            .dequeue_sent(fd)
            .and_then(|()| output.send(fd, BLANK))
        {
            Ok(()) => self.frame_error = None,
            Err(error) => self.log_error(&format!("cannot send a frame: {error}")),
        }
    }

    /// Updates the status of the camera, and logs a change.
    fn check_camera(&mut self, capture: &Capture) {
        let status = capture.status(v4l2::monotonic_now());
        if status != self.camera {
            tracing::info!("camera: {status}");
            self.camera = status;
        }
    }

    /// Logs an error of the stream, unless it is the same as the last one.
    fn log_error(&mut self, message: &str) {
        if self.frame_error.as_deref() != Some(message) {
            tracing::warn!("{message}");
            self.frame_error = Some(message.to_owned());
        }
    }
}

/// Waits until the gadget node exists, and opens it. While it waits, it passes
/// changes of the camera node to the capture thread.
async fn open_node(
    discovery: &mut Discovery,
    capture: &Capture,
    watchdog: &mut Watchdog,
) -> Result<OwnedFd, Error> {
    let mut logged = false;
    let mut ping = tokio::time::interval(PING_PERIOD);
    let path = loop {
        if let Some(path) = discovery.nodes().gadget() {
            break path.to_owned();
        }
        if !logged {
            tracing::info!("the worker waits for the gadget node");
            logged = true;
        }
        tokio::select! {
            changed = discovery.changed() => {
                changed.map_err(Error::Discovery)?;
                capture.set_node(discovery.nodes().camera());
            }
            _ = ping.tick() => watchdog.ping(),
        }
    };
    open(&path)
}

/// Opens the gadget node at `path`.
fn open(path: &Path) -> Result<OwnedFd, Error> {
    let flags = OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC;
    rustix::fs::open(path, flags, Mode::empty()).map_err(|error| Error::Open {
        path: path.to_owned(),
        source: error.into(),
    })
}

/// Checks the gadget node, sets the format and the frame time, and subscribes
/// to the UVC events. The host enumerates the gadget after this call.
fn prepare_node(fd: BorrowedFd<'_>) -> io::Result<()> {
    v4l2::check_gadget(fd)?;
    v4l2::set_format(fd, Queue::Output, WIDTH, HEIGHT)?;
    v4l2::set_frame_time(fd, Queue::Output, 1, 60)?;
    for kind in [
        uvc::EVENT_DISCONNECT,
        uvc::EVENT_CONNECT,
        uvc::EVENT_STREAMON,
        uvc::EVENT_STREAMOFF,
        uvc::EVENT_DATA,
        uvc::EVENT_SETUP,
    ] {
        v4l2::subscribe(fd, kind)?;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_frame_is_a_baseline_jpeg_of_the_gadget_size() {
        let jpeg = BLANK.jpeg;
        let sof = jpeg
            .windows(2)
            .position(|marker| marker == [0xff, 0xc0])
            .expect("a baseline SOF0 marker");
        let size = jpeg.get(sof + 5..sof + 9).expect("a complete SOF0 segment");
        let height = u16::from_be_bytes([size[0], size[1]]);
        let width = u16::from_be_bytes([size[2], size[3]]);

        assert_eq!((u32::from(width), u32::from(height)), (WIDTH, HEIGHT));
    }
}
