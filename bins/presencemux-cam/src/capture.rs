//! The capture of the camera on its own thread.
//!
//! The capture thread makes all ioctls on the capture node. The event loop of
//! the gadget node never waits for the camera.

use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

use crate::discovery::CameraNode;
use crate::v4l2::{self, BUF_FLAG_ERROR, Queue, monotonic_now};
use crate::{HEIGHT, WIDTH};

/// The number of kernel buffers of the capture queue.
const BUFFERS: u32 = 4;
/// The frame time that the thread requests, in seconds, as a fraction. The
/// driver chooses the nearest frame time of the camera.
const FRAME_TIME: (u32, u32) = (1, 30);
/// The largest age of the newest frame of a ready camera.
const FRESH: Duration = Duration::from_millis(500);
/// The longest time without a frame. A longer time gives `Fault`, and the
/// thread starts the capture again.
const FRAME_TIMEOUT: Duration = Duration::from_secs(3);
/// The time between two attempts to start the capture.
const RETRY: Duration = Duration::from_secs(1);

/// The status of the camera, as the worker reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CameraStatus {
    /// udev reports no camera node.
    Missing,
    /// The capture starts, and no frame has arrived yet.
    Initializing,
    /// The newest frame is younger than [`FRESH`].
    Ready,
    /// The capture failed, the first frame is late, or the frames stopped.
    Fault,
}

impl fmt::Display for CameraStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "missing",
            Self::Initializing => "initializing",
            Self::Ready => "ready",
            Self::Fault => "fault",
        })
    }
}

/// The handle of the capture thread.
#[derive(Debug)]
pub(crate) struct Capture {
    shared: Arc<Shared>,
    thread: JoinHandle<()>,
}

impl Capture {
    /// Starts the capture thread. The thread waits for a node.
    pub(crate) fn spawn() -> io::Result<Self> {
        let wake = rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            wake,
        });
        let thread = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("capture".to_owned())
            .spawn(move || run(&thread))?;
        Ok(Self { shared, thread })
    }

    /// Sets the camera node. The thread stops the capture on the old node and
    /// starts it on the new one.
    pub(crate) fn set_node(&self, node: Option<&CameraNode>) {
        let changed = self.shared.lock().set_node(node, monotonic_now());
        if changed {
            self.shared.wake();
        }
    }

    /// Returns the status of the camera at the `CLOCK_MONOTONIC` time `now`.
    pub(crate) fn status(&self, now: Duration) -> CameraStatus {
        self.shared.lock().status(now)
    }

    /// Returns whether the capture thread has ended. Only a panic ends it.
    pub(crate) fn has_ended(&self) -> bool {
        self.thread.is_finished()
    }
}

/// The state that the capture thread and the handle share.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    /// An eventfd that wakes the thread when the node changes.
    wake: OwnedFd,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wakes the capture thread.
    fn wake(&self) {
        if let Err(error) = rustix::io::write(&self.wake, &1u64.to_ne_bytes()) {
            tracing::warn!("cannot wake the capture thread: {error}");
        }
    }

    /// Waits until the camera is readable, the node changes, or the timeout
    /// ends. Returns the poll events of the camera.
    fn wait(
        &self,
        camera: Option<BorrowedFd<'_>>,
        timeout: Option<Duration>,
    ) -> io::Result<PollFlags> {
        let timeout =
            timeout.map(|timeout| Timespec::try_from(timeout).unwrap_or(Timespec::default()));
        let mut fds = vec![PollFd::new(&self.wake, PollFlags::IN)];
        if let Some(camera) = &camera {
            fds.push(PollFd::new(camera, PollFlags::IN));
        }
        match rustix::event::poll(&mut fds, timeout.as_ref()) {
            Ok(_) | Err(Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
        if fds[0].revents().contains(PollFlags::IN) {
            // The read resets the counter of the eventfd.
            let mut counter = [0; 8];
            match rustix::io::read(&self.wake, &mut counter) {
                Ok(_) | Err(Errno::AGAIN) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(fds.get(1).map_or(PollFlags::empty(), PollFd::revents))
    }

    /// Waits for [`RETRY`], or until the node is no longer `node`.
    fn wait_retry(&self, node: &CameraNode) -> io::Result<()> {
        let end = monotonic_now() + RETRY;
        while self.lock().node.as_ref() == Some(node) {
            let Some(remaining) = end
                .checked_sub(monotonic_now())
                .filter(|remaining| !remaining.is_zero())
            else {
                break;
            };
            self.wait(None, Some(remaining))?;
        }
        Ok(())
    }

    /// Waits until a node is set, and returns it.
    fn wait_for_node(&self) -> io::Result<CameraNode> {
        loop {
            if let Some(node) = self.lock().node.clone() {
                return Ok(node);
            }
            self.wait(None, None)?;
        }
    }

    /// Updates the stream state, if the node is still `node`. Returns whether
    /// the node is still `node`.
    fn update(&self, node: &CameraNode, f: impl FnOnce(&mut Stream)) -> bool {
        self.lock().update(node, f)
    }
}

/// The camera node and the state of its stream.
#[derive(Debug, Default)]
struct State {
    node: Option<CameraNode>,
    stream: Stream,
}

/// The state of the stream on the current node. Times are `CLOCK_MONOTONIC`.
#[derive(Clone, Copy, Debug, Default)]
struct Stream {
    /// The time of the node change or of the last stream start.
    since: Option<Duration>,
    /// The capture time of the newest frame.
    newest: Option<Duration>,
    /// Whether the last start or the stream failed.
    failed: bool,
}

impl Stream {
    fn new(since: Duration) -> Self {
        Self {
            since: Some(since),
            ..Self::default()
        }
    }
}

impl State {
    /// Sets the node and resets the stream state at `now`. Returns whether the
    /// node changed.
    fn set_node(&mut self, node: Option<&CameraNode>, now: Duration) -> bool {
        if self.node.as_ref() == node {
            return false;
        }
        self.node = node.cloned();
        self.stream = Stream::new(now);
        true
    }

    /// Updates the stream state, if the node is still `node`. Returns whether
    /// the node is still `node`.
    fn update(&mut self, node: &CameraNode, f: impl FnOnce(&mut Stream)) -> bool {
        let current = self.node.as_ref() == Some(node);
        if current {
            f(&mut self.stream);
        }
        current
    }

    fn status(&self, now: Duration) -> CameraStatus {
        let stream = &self.stream;
        if self.node.is_none() {
            CameraStatus::Missing
        } else if stream.failed {
            CameraStatus::Fault
        } else if let Some(newest) = stream.newest {
            match now.checked_sub(newest) {
                Some(age) if age < FRESH => CameraStatus::Ready,
                _ => CameraStatus::Fault,
            }
        } else {
            match stream.since.and_then(|since| now.checked_sub(since)) {
                Some(waited) if waited >= FRAME_TIMEOUT => CameraStatus::Fault,
                _ => CameraStatus::Initializing,
            }
        }
    }
}

/// Runs the capture thread. Starts the capture on each new node, and tries
/// again after [`RETRY`] after an error. Returns only when the thread cannot
/// wait.
fn run(shared: &Shared) {
    if let Err(error) = capture(shared) {
        tracing::error!("the capture thread cannot wait: {error}");
    }
}

/// Starts the capture on each new node. Returns only when the thread cannot
/// wait.
fn capture(shared: &Shared) -> io::Result<()> {
    let mut last_error = None;
    loop {
        let node = shared.wait_for_node()?;
        let Err(error) = attempt(shared, &node, &mut last_error) else {
            continue;
        };
        if !shared.update(&node, |stream| stream.failed = true) {
            continue;
        }
        let message = format!("cannot capture from {}: {error}", node.path.display());
        if last_error.as_ref() != Some(&message) {
            tracing::warn!("{message}");
            last_error = Some(message);
        }
        shared.wait_retry(&node)?;
    }
}

/// Opens the camera node and captures until the node changes or the stream
/// fails. Clears `last_error` when the stream starts.
fn attempt(shared: &Shared, node: &CameraNode, last_error: &mut Option<String>) -> io::Result<()> {
    let camera = Camera::open(&node.path)?;
    if !shared.update(node, |stream| *stream = Stream::new(monotonic_now())) {
        return Ok(());
    }
    let (numerator, denominator) = camera.frame_time;
    tracing::info!(
        "the capture started on {}, frame time {numerator}/{denominator} s",
        node.path.display()
    );
    *last_error = None;
    camera.run(shared, node)
}

/// An open capture node with a running stream. A drop closes the node. The
/// kernel then stops the stream and frees the buffers.
struct Camera {
    fd: OwnedFd,
    /// The frame time in seconds that the driver chose, as a fraction.
    frame_time: (u32, u32),
}

impl Camera {
    /// Opens the node, sets the format and starts the stream.
    fn open(path: &Path) -> io::Result<Self> {
        let flags = OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let fd = rustix::fs::open(path, flags, Mode::empty())?;
        let borrowed = fd.as_fd();
        v4l2::check_camera(borrowed)?;
        v4l2::set_format(borrowed, Queue::Capture, WIDTH, HEIGHT)?;
        let (numerator, denominator) = FRAME_TIME;
        let frame_time = v4l2::set_frame_time(borrowed, Queue::Capture, numerator, denominator)?;
        let count = v4l2::request_buffers(borrowed, Queue::Capture, BUFFERS)?;
        if count < 2 {
            return Err(io::Error::other(format!(
                "the kernel allocated only {count} buffers"
            )));
        }
        for index in 0..count {
            v4l2::queue_buffer(borrowed, Queue::Capture, index, 0)?;
        }
        v4l2::stream_on(borrowed, Queue::Capture)?;
        Ok(Self { fd, frame_time })
    }

    /// Records the frames until the node changes or the stream fails. A stream
    /// without a frame for [`FRAME_TIMEOUT`] fails.
    fn run(&self, shared: &Shared, node: &CameraNode) -> io::Result<()> {
        let mut last_frame = monotonic_now();
        loop {
            let remaining = (last_frame + FRAME_TIMEOUT)
                .checked_sub(monotonic_now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "the camera sent no frame for {} s",
                        FRAME_TIMEOUT.as_secs()
                    ))
                })?;
            let flags = shared.wait(Some(self.fd.as_fd()), Some(remaining))?;
            let newest = self.dequeue()?;
            if newest.is_some() {
                last_frame = monotonic_now();
            } else if flags.intersects(PollFlags::ERR | PollFlags::HUP) {
                return Err(io::Error::other(format!(
                    "poll gave {flags:?} without a frame"
                )));
            }
            let current = shared.update(node, |stream| {
                if newest.is_some() {
                    stream.newest = newest;
                }
            });
            if !current {
                return Ok(());
            }
        }
    }

    /// Dequeues each filled buffer and queues it again. Returns the capture
    /// time of the newest frame without the error flag.
    fn dequeue(&self) -> io::Result<Option<Duration>> {
        let fd = self.fd.as_fd();
        let mut newest = None;
        while let Some(buffer) = v4l2::dequeue_buffer(fd, Queue::Capture)? {
            v4l2::queue_buffer(fd, Queue::Capture, buffer.index, 0)?;
            if buffer.flags & BUF_FLAG_ERROR == 0 {
                newest = Some(buffer.monotonic_time().unwrap_or_else(monotonic_now));
            }
        }
        Ok(newest)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const START: Duration = Duration::from_secs(100);

    fn node(generation: u64) -> CameraNode {
        CameraNode {
            path: PathBuf::from("/dev/video1"),
            generation,
        }
    }

    fn state(stream: Stream) -> State {
        State {
            node: Some(node(1)),
            stream,
        }
    }

    fn with_frame_at(newest: Duration) -> State {
        state(Stream {
            since: Some(START),
            newest: Some(newest),
            failed: false,
        })
    }

    #[test]
    fn camera_without_a_node_is_missing() {
        let state = State::default();

        assert_eq!(state.status(START), CameraStatus::Missing);
    }

    #[test]
    fn failed_capture_without_a_node_is_missing() {
        let state = State {
            node: None,
            stream: Stream {
                failed: true,
                ..Stream::default()
            },
        };

        assert_eq!(state.status(START), CameraStatus::Missing);
    }

    #[test]
    fn camera_without_a_frame_2999_ms_after_the_start_is_initializing() {
        let state = state(Stream::new(START));

        assert_eq!(
            state.status(START + Duration::from_millis(2_999)),
            CameraStatus::Initializing
        );
    }

    #[test]
    fn camera_without_a_frame_3_s_after_the_start_is_at_fault() {
        let state = state(Stream::new(START));

        assert_eq!(
            state.status(START + Duration::from_secs(3)),
            CameraStatus::Fault
        );
    }

    #[test]
    fn camera_with_a_frame_younger_than_500_ms_is_ready() {
        let state = with_frame_at(START);

        assert_eq!(
            state.status(START + Duration::from_micros(499_999)),
            CameraStatus::Ready
        );
    }

    #[test]
    fn camera_with_a_frame_age_of_exactly_500_ms_is_at_fault() {
        let state = with_frame_at(START);

        assert_eq!(
            state.status(START + Duration::from_millis(500)),
            CameraStatus::Fault
        );
    }

    #[test]
    fn camera_with_a_frame_from_the_future_is_at_fault() {
        let state = with_frame_at(START + Duration::from_millis(1));

        assert_eq!(state.status(START), CameraStatus::Fault);
    }

    #[test]
    fn failed_capture_is_at_fault_also_with_a_fresh_frame() {
        let state = state(Stream {
            since: Some(START),
            newest: Some(START),
            failed: true,
        });

        assert_eq!(state.status(START), CameraStatus::Fault);
    }

    #[test]
    fn new_node_resets_the_stream_to_initializing() {
        let mut state = with_frame_at(START);

        state.set_node(Some(&node(2)), START);

        assert_eq!(state.status(START), CameraStatus::Initializing);
    }

    #[test]
    fn same_node_keeps_the_stream() {
        let mut state = with_frame_at(START);

        let changed = state.set_node(Some(&node(1)), START);

        assert!(!changed);
        assert_eq!(state.status(START), CameraStatus::Ready);
    }

    #[test]
    fn update_for_an_old_node_changes_nothing() {
        let mut state = state(Stream::new(START));
        state.set_node(Some(&node(2)), START);

        let current = state.update(&node(1), |stream| stream.newest = Some(START));

        assert!(!current);
        assert_eq!(state.status(START), CameraStatus::Initializing);
    }
}
