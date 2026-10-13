//! The cam worker of PresenceMux. It sends live video, a slate or blank to the
//! host through the UVC gadget function.

mod capture;
mod choice;
mod config;
mod delay;
mod discovery;
mod live;
mod output;
mod slate;
mod systemd;
mod uvc;
mod v4l2;
mod varlink;

use std::cell::RefCell;
use std::convert::Infallible;
use std::future::pending;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use rustix::fs::{Mode, OFlags};
use tokio::io::unix::AsyncFd;
use tokio::io::{Interest, Ready};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

use presencemux_api::cam::DeviceStatus;

use crate::capture::{CameraStatus, Capture, LiveFrame};
use crate::choice::Shown;
use crate::delay::Delays;
use crate::discovery::Discovery;
use crate::output::{Frame, FrameId, Output};
use crate::slate::Slates;
use crate::systemd::Watchdog;
use crate::uvc::{Action, Input, Limits, Uvc};
use crate::v4l2::Queue;
use crate::varlink::{Control, Queued, Service, Shared};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
/// `streaming_maxpacket` of the gadget.
const MAX_PAYLOAD: u32 = 2048;
/// The frame time of the gadget in seconds, as a fraction. The kernel spreads
/// each frame over this time, or sends it at the full rate of the bus if the
/// frame needs more time.
const GADGET_FRAME_TIME: (u32, u32) = (1, 240);
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
    #[error("cannot serve the Varlink socket: {0}")]
    Varlink(io::Error),
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
    let slates = Rc::new(load_slates());
    systemd::ready();
    let mut watchdog = Watchdog::new();
    let mut discovery = Discovery::new().map_err(Error::Discovery)?;
    let capture = Capture::spawn().map_err(Error::Capture)?;
    capture.set_node(discovery.nodes().camera());

    let mut fds = systemd::passed_fds().map_err(Error::FdStore)?;
    let control: Shared = Rc::new(RefCell::new(Control::new(Rc::clone(&slates))));
    let mut varlink = None;
    if let Some(fd) = fds.remove(systemd::VARLINK_FD_NAME) {
        let service = Service::new(Rc::clone(&control));
        let listener = listener(fd).map_err(Error::Varlink)?;
        let socket_gid = socket_gid();
        varlink = Some(tokio::task::spawn_local(varlink::serve(
            listener, service, socket_gid,
        )));
    } else {
        tracing::info!("systemd passed no Varlink socket, the worker sends blank");
    }
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
        live_queued: None,
        delays: Delays::default(),
        slates,
        slate_loop: SlateLoop::default(),
    };
    tracing::info!(
        "presencemux-cam started, adopted: {adopted}, streaming: {}, camera: {}",
        worker.output.is_some(),
        worker.camera
    );

    // Events can wait in the stored gadget node from before a restart.
    worker
        .handle_events(node.as_fd(), &control)
        .map_err(Error::Events)?;
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
                worker.handle_events(node.get_ref().as_fd(), &control).map_err(Error::Events)?;
                guard.clear_ready_matching(Ready::PRIORITY);
            }
            changed = discovery.changed() => {
                changed.map_err(Error::Discovery)?;
                capture.set_node(discovery.nodes().camera());
            }
            error = failure(&mut varlink) => return Err(Error::Varlink(error)),
            () = capture.live_ready() => {
                worker.queue_frame(node.get_ref().as_fd(), &control, &capture, Trigger::LiveFrame);
            }
            _ = frames.tick() => {
                if capture.has_ended() {
                    return Err(Error::CaptureEnded);
                }
                worker.update(&control, &capture);
                worker.queue_frame(node.get_ref().as_fd(), &control, &capture, Trigger::Timer);
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
    /// The last live frame that the worker queued.
    live_queued: Option<FrameId>,
    /// The delays of the live frames.
    delays: Delays,
    /// The slates of the configuration.
    slates: Rc<Slates>,
    /// The position in the frames of the shown slate.
    slate_loop: SlateLoop,
}

impl Worker {
    /// Reads and handles each event that waits on the gadget node.
    fn handle_events(&mut self, fd: BorrowedFd<'_>, control: &Shared) -> io::Result<()> {
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
                self.perform(fd, control, action);
            }
        }
        Ok(())
    }

    fn perform(&mut self, fd: BorrowedFd<'_>, control: &Shared, action: Action) {
        match action {
            Action::Respond(response) => {
                let (length, data) = response.wire();
                if let Err(error) = v4l2::send_response(fd, length, data) {
                    tracing::warn!("cannot answer a setup request: {error}");
                }
            }
            Action::StartStream => self.start_stream(fd, control),
            Action::StopStream => {
                self.start_pending = false;
                control.borrow_mut().queued = None;
                if let Some(output) = self.output.take()
                    && let Err(error) = output.stop(fd)
                {
                    tracing::warn!("cannot stop the stream: {error}");
                }
                tracing::info!("stream stopped");
            }
        }
    }

    /// Starts the stream and queues blank as the first frame. If the start
    /// fails, the stream start stays pending.
    fn start_stream(&mut self, fd: BorrowedFd<'_>, control: &Shared) {
        match Output::start(fd) {
            Ok(mut output) => {
                let mut node = Node {
                    output: &mut output,
                    fd,
                };
                let now = v4l2::monotonic_now();
                let mut control = control.borrow_mut();
                let lease = control.choice.lease(now);
                let mut taken = None;
                let queued = queue(&mut node, BLANK, Shown::Blank, lease, &mut taken);
                control.queued = taken;
                self.output = Some(output);
                self.start_pending = false;
                self.frame_error = None;
                self.live_queued = None;
                tracing::info!("stream started");
                if let Err(error) = queued {
                    self.log_error(&format!("cannot queue a frame: {error}"));
                }
            }
            Err(error) => {
                self.start_pending = true;
                self.log_error(&format!("cannot start the stream: {error}"));
            }
        }
    }

    /// Queues the next frame, if a stream runs and a buffer is free. Queues a
    /// new live frame at once. On the frame timer, first tries a pending stream
    /// start again. Then queues blank if frame choice does not show live or no
    /// fresh live frame exists. After an error with a live frame, queues blank.
    fn queue_frame(
        &mut self,
        fd: BorrowedFd<'_>,
        control: &Shared,
        capture: &Capture,
        trigger: Trigger,
    ) {
        if trigger == Trigger::Timer && self.start_pending {
            self.start_stream(fd, control);
            return;
        }
        let Some(output) = &mut self.output else {
            return;
        };
        if let Err(error) = output.dequeue_sent(fd) {
            self.log_error(&format!("cannot dequeue a sent buffer: {error}"));
            return;
        }
        let now = v4l2::monotonic_now();
        let mut control = control.borrow_mut();
        let control = &mut *control;
        let shown = control.choice.shown(now, control.newest);
        let mut sent = Sent {
            lease: control.choice.lease(now),
            slates: &self.slates,
            slate_loop: &mut self.slate_loop,
            live_queued: &mut self.live_queued,
            delays: &mut self.delays,
            queued: &mut control.queued,
        };
        let result = capture.with_live_frame(|live| {
            sent.slate_loop.observe(shown);
            let next = next_frame(shown, live, *sent.live_queued, now, trigger);
            sent.send(&mut Node { output, fd }, next)
        });
        match result {
            Ok(()) => self.frame_error = None,
            Err(error) => self.log_error(&format!("cannot queue a frame: {error}")),
        }
    }

    /// Updates the camera status and the shared state, and checks the lease and
    /// the frames. Logs a change of the camera and a new latch.
    fn update(&mut self, control: &Shared, capture: &Capture) {
        let now = v4l2::monotonic_now();
        let status = capture.status(now);
        if status != self.camera {
            tracing::info!("camera: {status}");
            self.camera = status;
        }
        let mut control = control.borrow_mut();
        control.camera = self.camera;
        control.streaming = self.output.is_some();
        control.worker = if self.start_pending {
            DeviceStatus::Fault
        } else {
            DeviceStatus::Ready
        };
        control.newest = capture.newest();
        let newest = control.newest;
        if let Some(latch) = control.choice.check(now, newest) {
            tracing::info!("latched: {}, epoch {}", latch.reason, latch.grant.epoch);
        }
        let live = control.choice.shown(now, newest) == Shown::Live;
        capture.set_live(live && self.output.is_some());
    }

    /// Logs an error of the stream, unless it is the same as the last one.
    fn log_error(&mut self, message: &str) {
        if self.frame_error.as_deref() != Some(message) {
            tracing::warn!("{message}");
            self.frame_error = Some(message.to_owned());
        }
    }
}

/// A UVC output that takes frames.
trait Sink {
    /// Queues `frame`. Returns `false` if no buffer is free.
    fn queue(&mut self, frame: Frame<'_>) -> io::Result<bool>;
}

/// The stream of the gadget node.
struct Node<'a> {
    output: &'a mut Output,
    fd: BorrowedFd<'a>,
}

impl Sink for Node<'_> {
    fn queue(&mut self, frame: Frame<'_>) -> io::Result<bool> {
        self.output.queue(self.fd, frame)
    }
}

/// Queues `frame` of the kind `kind` on `sink`. If a buffer takes the frame,
/// records `kind` and the grant of `lease` in `queued`.
fn queue(
    sink: &mut impl Sink,
    frame: Frame<'_>,
    kind: Shown<'_>,
    lease: Option<&choice::Lease>,
    queued: &mut Option<Queued>,
) -> io::Result<bool> {
    let result = sink.queue(frame);
    if matches!(result, Ok(true)) {
        Queued::record(queued, kind, lease);
    }
    result
}

/// The state that changes with each frame that the worker queues.
struct Sent<'a> {
    /// The valid lease of frame choice, if any.
    lease: Option<&'a choice::Lease>,
    /// The slates of the configuration.
    slates: &'a Slates,
    /// The position in the frames of the shown slate.
    slate_loop: &'a mut SlateLoop,
    /// The last live frame that the worker queued.
    live_queued: &'a mut Option<FrameId>,
    /// The delays of the live frames.
    delays: &'a mut Delays,
    /// The last frame that a buffer took.
    queued: &'a mut Option<Queued>,
}

impl Sent<'_> {
    /// Queues the frame `next` on `sink`. After an error with a slate or a live
    /// frame, queues blank and returns the error.
    fn send(&mut self, sink: &mut impl Sink, next: Next<'_>) -> io::Result<()> {
        match next {
            Next::Nothing => Ok(()),
            Next::Blank => self.blank(sink).map(drop),
            Next::Slate(name) => match self.slates.get(name) {
                Ok(slate) => {
                    let frame = slate.frame(self.slate_loop.index(slate.id()));
                    match queue(sink, frame, Shown::Slate(name), self.lease, self.queued) {
                        Ok(true) => {
                            self.slate_loop.advance(slate.len());
                            Ok(())
                        }
                        Ok(false) => Ok(()),
                        Err(error) => self.blank(sink).and(Err(error)),
                    }
                }
                Err(_) => self.blank(sink).map(drop),
            },
            Next::Live(live) => match queue(sink, live.frame, Shown::Live, self.lease, self.queued)
            {
                Ok(false) => Ok(()),
                Ok(true) => {
                    *self.live_queued = Some(live.frame.id);
                    self.delays.record(live.times, v4l2::monotonic_now());
                    Ok(())
                }
                Err(error) => {
                    *self.live_queued = Some(live.frame.id);
                    self.blank(sink).and(Err(error))
                }
            },
        }
    }

    /// Queues the blank frame on `sink`.
    fn blank(&mut self, sink: &mut impl Sink) -> io::Result<bool> {
        queue(sink, BLANK, Shown::Blank, self.lease, self.queued)
    }
}

/// The event that makes the worker queue a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    /// The frame timer.
    Timer,
    /// A new live frame.
    LiveFrame,
}

/// The frame that the worker queues next.
#[derive(Clone, Copy, Debug)]
enum Next<'a> {
    /// The blank frame.
    Blank,
    /// The next frame of the slate with this name.
    Slate(&'a str),
    /// The newest live frame.
    Live(LiveFrame<'a>),
    /// No frame. The host shows the last frame again.
    Nothing,
}

/// Returns the frame that the worker queues next at the time `now`, for the
/// frame `shown` of frame choice and the event `trigger`. `live` is the newest
/// live frame, and `live_queued` the last live frame that the worker queued.
fn next_frame<'a>(
    shown: Shown<'a>,
    live: Option<LiveFrame<'a>>,
    live_queued: Option<FrameId>,
    now: Duration,
    trigger: Trigger,
) -> Next<'a> {
    let fresh = live.filter(|live| choice::is_fresh(now, Some(live.times.captured)));
    match (shown, fresh) {
        (Shown::Live, Some(live)) if live_queued == Some(live.frame.id) => Next::Nothing,
        (Shown::Live, Some(live)) => Next::Live(live),
        (Shown::Slate(name), _) if trigger == Trigger::Timer => Next::Slate(name),
        _ if trigger == Trigger::Timer => Next::Blank,
        _ => Next::Nothing,
    }
}

/// The position in the frames of the shown slate. A slate starts at its first
/// frame each time frame choice shows it.
#[derive(Debug, Default)]
struct SlateLoop {
    /// The identity of the shown slate. `None` while frame choice shows no
    /// slate.
    slate: Option<u64>,
    /// The index of the next frame.
    next: usize,
}

impl SlateLoop {
    /// Resets the loop if frame choice does not show a slate.
    fn observe(&mut self, shown: Shown<'_>) {
        if !matches!(shown, Shown::Slate(_)) {
            self.reset();
        }
    }

    /// Returns the index of the next frame of the slate with the identity
    /// `slate`. Starts at the first frame for a new slate.
    fn index(&mut self, slate: u64) -> usize {
        if self.slate != Some(slate) {
            self.slate = Some(slate);
            self.next = 0;
        }
        self.next
    }

    /// Moves to the next frame of a slate with `len` frames. Returns to the
    /// first frame after the last frame.
    fn advance(&mut self, len: usize) {
        self.next = (self.next + 1) % len;
    }

    /// Resets the loop. The next shown slate starts at its first frame.
    fn reset(&mut self) {
        self.slate = None;
        self.next = 0;
    }
}

/// Returns the slates of the configuration file. Without a valid
/// configuration file, returns no slates.
fn load_slates() -> Slates {
    match config::slate_files(Path::new(config::CONFIG)) {
        Ok(files) => Slates::load(&files),
        Err(error) => {
            tracing::warn!("{error}, the worker has no slates");
            Slates::default()
        }
    }
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
    match rustix::fs::stat(presencemux_api::cam::SOCKET_PATH) {
        Ok(stat) => Some(stat.st_gid),
        Err(error) => {
            tracing::warn!(
                "cannot read the group of {}, the worker serves root: {error}",
                presencemux_api::cam::SOCKET_PATH
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
    v4l2::set_frame_time(fd, Queue::Output, GADGET_FRAME_TIME.0, GADGET_FRAME_TIME.1)?;
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
    use crate::varlink::{Granted, Kind};

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

    const NOW: Duration = Duration::from_secs(100);

    fn live_frame(id: u64, captured: Duration) -> LiveFrame<'static> {
        LiveFrame {
            frame: Frame {
                id: FrameId(id),
                jpeg: b"jpeg",
            },
            times: capture::LiveTimes {
                captured,
                dequeued: captured,
                requantized: captured,
            },
        }
    }

    /// Returns the identity of the live frame in `next`, if it is live.
    fn live_id(next: Next<'_>) -> Option<FrameId> {
        match next {
            Next::Live(live) => Some(live.frame.id),
            Next::Blank | Next::Slate(_) | Next::Nothing => None,
        }
    }

    #[test]
    fn unqueued_live_frame_gives_live_on_a_live_frame_event() {
        let live = live_frame(2, NOW);

        let next = next_frame(
            Shown::Live,
            Some(live),
            Some(FrameId(1)),
            NOW,
            Trigger::LiveFrame,
        );

        assert_eq!(live_id(next), Some(FrameId(2)));
    }

    #[test]
    fn queued_live_frame_gives_nothing_on_the_frame_timer() {
        let live = live_frame(2, NOW);

        let next = next_frame(
            Shown::Live,
            Some(live),
            Some(FrameId(2)),
            NOW,
            Trigger::Timer,
        );

        assert!(matches!(next, Next::Nothing));
    }

    #[test]
    fn live_frame_499_ms_old_gives_live_on_the_frame_timer() {
        let live = live_frame(2, NOW);

        let next = next_frame(
            Shown::Live,
            Some(live),
            None,
            NOW + Duration::from_millis(499),
            Trigger::Timer,
        );

        assert_eq!(live_id(next), Some(FrameId(2)));
    }

    #[test]
    fn live_frame_500_ms_old_gives_blank_on_the_frame_timer() {
        let live = live_frame(2, NOW);

        let next = next_frame(
            Shown::Live,
            Some(live),
            None,
            NOW + Duration::from_millis(500),
            Trigger::Timer,
        );

        assert!(matches!(next, Next::Blank));
    }

    #[test]
    fn live_frame_500_ms_old_gives_nothing_on_a_live_frame_event() {
        let live = live_frame(2, NOW);

        let next = next_frame(
            Shown::Live,
            Some(live),
            None,
            NOW + Duration::from_millis(500),
            Trigger::LiveFrame,
        );

        assert!(matches!(next, Next::Nothing));
    }

    #[test]
    fn live_frame_from_the_future_gives_blank_on_the_frame_timer() {
        let live = live_frame(2, NOW + Duration::from_millis(1));

        let next = next_frame(Shown::Live, Some(live), None, NOW, Trigger::Timer);

        assert!(matches!(next, Next::Blank));
    }

    #[test]
    fn live_without_a_live_frame_gives_blank_on_the_frame_timer() {
        let next = next_frame(Shown::Live, None, None, NOW, Trigger::Timer);

        assert!(matches!(next, Next::Blank));
    }

    #[test]
    fn blank_with_a_fresh_live_frame_gives_blank_on_the_frame_timer() {
        let live = live_frame(2, NOW);

        let next = next_frame(Shown::Blank, Some(live), None, NOW, Trigger::Timer);

        assert!(matches!(next, Next::Blank));
    }

    #[test]
    fn slate_gives_the_slate_on_the_frame_timer() {
        let next = next_frame(Shown::Slate("away"), None, None, NOW, Trigger::Timer);

        assert!(matches!(next, Next::Slate("away")));
    }

    #[test]
    fn slate_gives_nothing_on_a_live_frame_event() {
        let next = next_frame(Shown::Slate("away"), None, None, NOW, Trigger::LiveFrame);

        assert!(matches!(next, Next::Nothing));
    }

    /// The identities of two slates in the tests.
    const AWAY: u64 = 1 << 63;
    const BRB: u64 = (1 << 63) | (1 << 32);

    #[test]
    fn new_slate_starts_at_its_first_frame() {
        let mut slate_loop = SlateLoop::default();
        slate_loop.index(BRB);
        slate_loop.advance(3);

        let index = slate_loop.index(AWAY);

        assert_eq!(index, 0);
    }

    #[test]
    fn same_slate_continues_at_its_next_frame() {
        let mut slate_loop = SlateLoop::default();
        slate_loop.index(AWAY);
        slate_loop.advance(3);

        let index = slate_loop.index(AWAY);

        assert_eq!(index, 1);
    }

    #[test]
    fn loop_returns_to_its_first_frame_after_its_last_frame() {
        let mut slate_loop = SlateLoop::default();
        slate_loop.index(AWAY);
        slate_loop.advance(2);
        slate_loop.advance(2);

        let index = slate_loop.index(AWAY);

        assert_eq!(index, 0);
    }

    #[test]
    fn slate_shown_again_after_blank_starts_at_its_first_frame() {
        let mut slate_loop = SlateLoop::default();
        slate_loop.index(AWAY);
        slate_loop.advance(3);
        slate_loop.observe(Shown::Blank);

        let index = slate_loop.index(AWAY);

        assert_eq!(index, 0);
    }

    #[test]
    fn shown_slate_keeps_the_position_of_its_loop() {
        let mut slate_loop = SlateLoop::default();
        slate_loop.index(AWAY);
        slate_loop.advance(3);
        slate_loop.observe(Shown::Slate("away"));

        let index = slate_loop.index(AWAY);

        assert_eq!(index, 1);
    }

    /// A sink that returns each result of `results` in order.
    struct FakeSink {
        results: Vec<io::Result<bool>>,
    }

    impl Sink for FakeSink {
        fn queue(&mut self, _frame: Frame<'_>) -> io::Result<bool> {
            self.results.remove(0)
        }
    }

    /// Returns a sink that takes each frame.
    fn free_sink() -> FakeSink {
        FakeSink {
            results: vec![Ok(true)],
        }
    }

    /// The state of the worker in the `send` tests.
    #[derive(Default)]
    struct State {
        slates: Slates,
        slate_loop: SlateLoop,
        live_queued: Option<FrameId>,
        delays: Delays,
        lease: Option<choice::Lease>,
        queued: Option<Queued>,
    }

    impl State {
        fn with_slates(slates: Slates) -> Self {
            Self {
                slates,
                ..Self::default()
            }
        }

        fn sent(&mut self) -> Sent<'_> {
            Sent {
                lease: self.lease.as_ref(),
                slates: &self.slates,
                slate_loop: &mut self.slate_loop,
                live_queued: &mut self.live_queued,
                delays: &mut self.delays,
                queued: &mut self.queued,
            }
        }
    }

    #[test]
    fn queued_blank_is_recorded_as_blank() {
        let mut state = State::default();

        state.sent().send(&mut free_sink(), Next::Blank).unwrap();

        assert_eq!(state.queued.map(|queued| queued.kind), Some(Kind::Blank));
    }

    #[test]
    fn queued_slate_is_recorded_with_its_name() {
        let mut state = State::with_slates(Slates::with_slate(
            "away",
            include_bytes!("../assets/slates/away.jpg"),
        ));

        state
            .sent()
            .send(&mut free_sink(), Next::Slate("away"))
            .unwrap();

        assert_eq!(
            state.queued.map(|queued| queued.kind),
            Some(Kind::Slate("away".to_owned()))
        );
    }

    #[test]
    fn queued_live_frame_is_recorded_as_live() {
        let mut state = State::default();

        state
            .sent()
            .send(&mut free_sink(), Next::Live(live_frame(2, NOW)))
            .unwrap();

        assert_eq!(state.queued.map(|queued| queued.kind), Some(Kind::Live));
    }

    #[test]
    fn queued_live_frame_records_the_grant_of_its_lease() {
        let mut state = State {
            lease: Some(choice::Lease {
                grant: choice::Grant {
                    session: Duration::from_secs(1),
                    epoch: 3,
                },
                permission: choice::Permission::Live,
                until: Duration::from_secs(101),
            }),
            ..State::default()
        };

        state
            .sent()
            .send(&mut free_sink(), Next::Live(live_frame(2, NOW)))
            .unwrap();

        assert_eq!(
            state.queued,
            Some(Queued {
                kind: Kind::Live,
                grant: Some(Granted {
                    grant: choice::Grant {
                        session: Duration::from_secs(1),
                        epoch: 3,
                    },
                    permission: choice::Permission::Live,
                }),
            })
        );
    }

    #[test]
    fn queued_blank_without_a_lease_records_no_grant() {
        let mut state = State {
            lease: None,
            ..State::default()
        };

        state.sent().send(&mut free_sink(), Next::Blank).unwrap();

        assert_eq!(
            state.queued,
            Some(Queued {
                kind: Kind::Blank,
                grant: None,
            })
        );
    }

    #[test]
    fn queued_blank_under_a_new_grant_records_the_new_grant() {
        let mut state = State {
            lease: Some(choice::Lease {
                grant: choice::Grant {
                    session: Duration::from_secs(1),
                    epoch: 4,
                },
                permission: choice::Permission::Blank,
                until: Duration::from_secs(101),
            }),
            queued: Some(Queued {
                kind: Kind::Blank,
                grant: Some(Granted {
                    grant: choice::Grant {
                        session: Duration::from_secs(1),
                        epoch: 3,
                    },
                    permission: choice::Permission::Blank,
                }),
            }),
            ..State::default()
        };

        state.sent().send(&mut free_sink(), Next::Blank).unwrap();

        assert_eq!(
            state.queued,
            Some(Queued {
                kind: Kind::Blank,
                grant: Some(Granted {
                    grant: choice::Grant {
                        session: Duration::from_secs(1),
                        epoch: 4,
                    },
                    permission: choice::Permission::Blank,
                }),
            })
        );
    }

    #[test]
    fn repeated_slate_frame_records_the_slate_and_its_grant() {
        let mut state = State {
            lease: Some(choice::Lease {
                grant: choice::Grant {
                    session: Duration::from_secs(1),
                    epoch: 3,
                },
                permission: choice::Permission::Slate("away".to_owned()),
                until: Duration::from_secs(101),
            }),
            ..State::with_slates(Slates::with_slate(
                "away",
                include_bytes!("../assets/slates/away.jpg"),
            ))
        };
        let mut sink = FakeSink {
            results: vec![Ok(true), Ok(true)],
        };
        state.sent().send(&mut sink, Next::Slate("away")).unwrap();

        state.sent().send(&mut sink, Next::Slate("away")).unwrap();

        assert_eq!(
            state.queued,
            Some(Queued {
                kind: Kind::Slate("away".to_owned()),
                grant: Some(Granted {
                    grant: choice::Grant {
                        session: Duration::from_secs(1),
                        epoch: 3,
                    },
                    permission: choice::Permission::Slate("away".to_owned()),
                }),
            })
        );
    }

    #[test]
    fn nothing_keeps_the_last_queued_kind() {
        let mut state = State {
            queued: Some(Queued {
                kind: Kind::Live,
                grant: None,
            }),
            ..State::default()
        };
        let mut sink = FakeSink { results: vec![] };

        state.sent().send(&mut sink, Next::Nothing).unwrap();

        assert_eq!(state.queued.map(|queued| queued.kind), Some(Kind::Live));
    }

    #[test]
    fn live_frame_without_a_free_buffer_keeps_the_last_queued_kind() {
        let mut state = State {
            queued: Some(Queued {
                kind: Kind::Blank,
                grant: None,
            }),
            ..State::default()
        };
        let mut sink = FakeSink {
            results: vec![Ok(false)],
        };

        state
            .sent()
            .send(&mut sink, Next::Live(live_frame(2, NOW)))
            .unwrap();

        assert_eq!(state.queued.map(|queued| queued.kind), Some(Kind::Blank));
    }

    #[test]
    fn failed_live_frame_with_a_queued_blank_is_recorded_as_blank() {
        let mut state = State {
            queued: Some(Queued {
                kind: Kind::Live,
                grant: None,
            }),
            ..State::default()
        };
        let mut sink = FakeSink {
            results: vec![Err(io::Error::other("no device")), Ok(true)],
        };

        let _ = state.sent().send(&mut sink, Next::Live(live_frame(2, NOW)));

        assert_eq!(state.queued.map(|queued| queued.kind), Some(Kind::Blank));
    }

    #[test]
    fn failed_live_frame_returns_its_error() {
        let mut state = State::default();
        let mut sink = FakeSink {
            results: vec![Err(io::Error::other("no device")), Ok(true)],
        };

        let result = state.sent().send(&mut sink, Next::Live(live_frame(2, NOW)));

        assert_eq!(result.unwrap_err().to_string(), "no device");
    }

    #[test]
    fn failed_slate_with_a_queued_blank_is_recorded_as_blank() {
        let mut state = State::with_slates(Slates::with_slate(
            "away",
            include_bytes!("../assets/slates/away.jpg"),
        ));
        let mut sink = FakeSink {
            results: vec![Err(io::Error::other("no device")), Ok(true)],
        };

        let _ = state.sent().send(&mut sink, Next::Slate("away"));

        assert_eq!(state.queued.map(|queued| queued.kind), Some(Kind::Blank));
    }

    #[test]
    fn unknown_slate_is_recorded_as_blank() {
        let mut state = State::default();

        state
            .sent()
            .send(&mut free_sink(), Next::Slate("away"))
            .unwrap();

        assert_eq!(state.queued.map(|queued| queued.kind), Some(Kind::Blank));
    }

    #[test]
    fn blank_with_a_fresh_live_frame_gives_nothing_on_a_live_frame_event() {
        let live = live_frame(2, NOW);

        let next = next_frame(Shown::Blank, Some(live), None, NOW, Trigger::LiveFrame);

        assert!(matches!(next, Next::Nothing));
    }
}
