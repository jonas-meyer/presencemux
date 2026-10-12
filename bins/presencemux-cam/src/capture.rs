//! The capture of the camera on its own thread.
//!
//! The capture thread makes all ioctls on the capture node. The event loop of
//! the gadget node never waits for the camera. While the event loop wants live
//! frames, the thread also requantizes the newest frame and keeps the result.

use std::fmt;
use std::io;
use std::mem;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use tokio::sync::Notify;

use crate::discovery::CameraNode;
use crate::live::{MAX_SIZE, Requantizer};
use crate::output::{Frame, FrameId};
use crate::v4l2::{self, CaptureStream, Dequeued, Queue, monotonic_now};
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
            live_ready: Notify::new(),
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

    /// Returns the capture time of the newest camera frame of the current node.
    pub(crate) fn newest(&self) -> Option<Duration> {
        self.shared.lock().stream.newest
    }

    /// Sets whether the thread requantizes the camera frames into live frames.
    /// If `live` is false, drops the live frame.
    pub(crate) fn set_live(&self, live: bool) {
        self.shared.lock().set_live(live);
    }

    /// Waits until the thread has a new live frame.
    pub(crate) async fn live_ready(&self) {
        self.shared.live_ready.notified().await;
    }

    /// Calls `f` with the newest live frame of the current node. Holds the mutex
    /// of the shared state while `f` runs.
    pub(crate) fn with_live_frame<R>(&self, f: impl FnOnce(Option<LiveFrame<'_>>) -> R) -> R {
        let state = self.shared.lock();
        f(state.live_frame.as_ref().map(|stored| LiveFrame {
            frame: Frame {
                id: stored.id,
                jpeg: &stored.jpeg,
            },
            times: stored.times,
        }))
    }

    /// Returns whether the capture thread has ended. Only a panic ends it.
    pub(crate) fn has_ended(&self) -> bool {
        self.thread.is_finished()
    }
}

/// The newest live frame, borrowed from the shared state.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LiveFrame<'a> {
    pub(crate) frame: Frame<'a>,
    pub(crate) times: LiveTimes,
}

/// The `CLOCK_MONOTONIC` times of a live frame in the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LiveTimes {
    /// The capture time of the camera frame.
    pub(crate) captured: Duration,
    /// The time at which the thread dequeued the camera frame.
    pub(crate) dequeued: Duration,
    /// The time at which the requantization ended.
    pub(crate) requantized: Duration,
}

/// The state that the capture thread and the handle share.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    /// An eventfd that wakes the thread when the node changes.
    wake: OwnedFd,
    /// The `Notify` that wakes the event loop for each new live frame.
    live_ready: Notify,
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

/// The camera node, the state of its stream and the newest live frame.
#[derive(Debug, Default)]
struct State {
    node: Option<CameraNode>,
    stream: Stream,
    /// Whether the event loop wants live frames.
    live: bool,
    /// The newest live frame of the current node.
    live_frame: Option<StoredLiveFrame>,
    /// The number of live frames so far. The count gives each frame its
    /// identity.
    live_count: u64,
}

/// A live frame that the shared state owns.
#[derive(Debug)]
struct StoredLiveFrame {
    id: FrameId,
    times: LiveTimes,
    jpeg: Vec<u8>,
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
        self.live_frame = None;
        true
    }

    /// Sets whether the event loop wants live frames. If `live` is false, drops
    /// the live frame.
    fn set_live(&mut self, live: bool) {
        self.live = live;
        if !live {
            self.live_frame = None;
        }
    }

    /// Replaces the live frame with `jpeg`, with a new identity, and leaves
    /// the buffer of the replaced frame in `jpeg` for reuse. Ignores `jpeg` if
    /// the event loop does not want live frames, or if the node is not
    /// `node`. Returns whether the node is still `node`.
    fn set_live_frame(&mut self, node: &CameraNode, times: LiveTimes, jpeg: &mut Vec<u8>) -> bool {
        if self.node.as_ref() != Some(node) {
            return false;
        }
        if !self.live {
            return true;
        }
        self.live_count += 1;
        let stored = StoredLiveFrame {
            id: FrameId(self.live_count),
            times,
            jpeg: mem::take(jpeg),
        };
        if let Some(replaced) = self.live_frame.replace(stored) {
            *jpeg = replaced.jpeg;
        }
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

/// An open capture node with a running stream.
struct Camera {
    stream: CaptureStream,
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
        let stream = CaptureStream::start(fd, BUFFERS)?;
        Ok(Self { stream, frame_time })
    }

    /// Records the frames until the node changes or the stream fails. A stream
    /// without a frame for [`FRAME_TIMEOUT`] fails. While the event loop wants
    /// live frames, requantizes the newest frame after each wait.
    fn run(&self, shared: &Shared, node: &CameraNode) -> io::Result<()> {
        let mut live_frames = LiveFrames::new();
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
            let flags = shared.wait(Some(self.stream.as_fd()), Some(remaining))?;
            let newest = self.newest_frame()?;
            let dequeued = monotonic_now();
            let captured = newest
                .as_ref()
                .map(|frame| frame.monotonic_time().unwrap_or(dequeued));
            if captured.is_some() {
                last_frame = dequeued;
            } else if flags.intersects(PollFlags::ERR | PollFlags::HUP) {
                return Err(io::Error::other(format!(
                    "poll gave {flags:?} without a frame"
                )));
            }
            let current = shared.update(node, |stream| {
                if captured.is_some() {
                    stream.newest = captured;
                }
            });
            if !current {
                return Ok(());
            }
            let (Some(frame), Some(captured)) = (newest, captured) else {
                continue;
            };
            let wanted = shared.lock().live;
            let current = if wanted {
                live_frames.requantize(shared, node, &frame, captured, dequeued)
            } else {
                true
            };
            frame.requeue()?;
            if !current {
                return Ok(());
            }
        }
    }

    /// Dequeues each filled buffer. Queues each buffer again, except the newest
    /// buffer without the error flag. Returns that buffer.
    fn newest_frame(&self) -> io::Result<Option<Dequeued<'_>>> {
        let mut newest = None;
        while let Some(buffer) = self.stream.dequeue()? {
            if buffer.has_error() {
                buffer.requeue()?;
            } else if let Some(older) = newest.replace(buffer) {
                older.requeue()?;
            }
        }
        Ok(newest)
    }
}

/// The requantizer and its state on the capture thread, for one open camera.
struct LiveFrames {
    requantizer: Requantizer,
    /// The buffer for the next live frame.
    jpeg: Vec<u8>,
    /// The quality of the last live frame.
    quality: Option<u8>,
    /// The last error of the requantization. The thread logs an error only when
    /// it differs from this one.
    last_error: Option<String>,
}

impl LiveFrames {
    fn new() -> Self {
        Self {
            requantizer: Requantizer::new(MAX_SIZE),
            jpeg: Vec::new(),
            quality: None,
            last_error: None,
        }
    }

    /// Requantizes the camera frame `frame` into the live frame of `node`, and
    /// wakes the event loop. `captured` and `dequeued` are the times of the
    /// camera frame. Logs a new error and a change of the quality. Returns
    /// whether the node is still `node`.
    fn requantize(
        &mut self,
        shared: &Shared,
        node: &CameraNode,
        frame: &Dequeued<'_>,
        captured: Duration,
        dequeued: Duration,
    ) -> bool {
        let result = frame
            .data()
            .map_err(|error| error.to_string())
            .and_then(|data| {
                (self.requantizer.requantize(data, &mut self.jpeg))
                    .map(|live| (live.quality, live.one_part, live.jpeg.len()))
                    .map_err(|error| error.to_string())
            });
        let (quality, one_part, size) = match result {
            Ok(live) => live,
            Err(error) => {
                let message = format!("cannot requantize a frame: {error}");
                if self.last_error.as_ref() != Some(&message) {
                    tracing::warn!("{message}");
                    self.last_error = Some(message);
                }
                return true;
            }
        };
        self.last_error = None;
        if self.quality != Some(quality) {
            tracing::info!("live quality: {quality}, {size} bytes");
            self.quality = Some(quality);
        }
        if one_part {
            tracing::debug!("the parts of a frame did not synchronize");
        }
        let times = LiveTimes {
            captured,
            dequeued,
            requantized: monotonic_now(),
        };
        let current = shared.lock().set_live_frame(node, times, &mut self.jpeg);
        if current {
            shared.live_ready.notify_one();
        }
        current
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
            ..State::default()
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
            ..State::default()
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

    fn times(captured: Duration) -> LiveTimes {
        LiveTimes {
            captured,
            dequeued: captured,
            requantized: captured,
        }
    }

    /// Returns a state with a stream on node 1 that wants live frames.
    fn live_state() -> State {
        State {
            live: true,
            ..state(Stream::new(START))
        }
    }

    fn live_id(state: &State) -> Option<FrameId> {
        state.live_frame.as_ref().map(|frame| frame.id)
    }

    #[test]
    fn first_live_frame_does_not_have_the_identity_of_blank() {
        let mut state = live_state();

        state.set_live_frame(&node(1), times(START), &mut b"jpeg".to_vec());

        assert!(live_id(&state).is_some());
        assert_ne!(live_id(&state), Some(crate::BLANK.id));
    }

    #[test]
    fn each_live_frame_has_a_new_identity() {
        let mut state = live_state();
        state.set_live_frame(&node(1), times(START), &mut b"first".to_vec());
        let first = live_id(&state);

        state.set_live_frame(&node(1), times(START), &mut b"second".to_vec());

        assert_ne!(live_id(&state), first);
    }

    #[test]
    fn new_live_frame_replaces_the_bytes_of_the_last_one() {
        let mut state = live_state();
        state.set_live_frame(&node(1), times(START), &mut b"first".to_vec());

        state.set_live_frame(&node(1), times(START), &mut b"second".to_vec());

        let stored = state.live_frame.as_ref().map(|frame| frame.jpeg.as_slice());
        assert_eq!(stored, Some(&b"second"[..]));
    }

    #[test]
    fn new_live_frame_returns_the_buffer_of_the_last_one() {
        let mut state = live_state();
        state.set_live_frame(&node(1), times(START), &mut b"first".to_vec());
        let mut jpeg = b"second".to_vec();

        state.set_live_frame(&node(1), times(START), &mut jpeg);

        assert_eq!(jpeg, b"first");
    }

    #[test]
    fn live_frame_for_an_old_node_changes_nothing() {
        let mut state = live_state();
        state.set_node(Some(&node(2)), START);

        state.set_live_frame(&node(1), times(START), &mut b"jpeg".to_vec());

        assert_eq!(live_id(&state), None);
    }

    #[test]
    fn live_frame_for_an_old_node_returns_false() {
        let mut state = live_state();
        state.set_node(Some(&node(2)), START);

        let current = state.set_live_frame(&node(1), times(START), &mut b"jpeg".to_vec());

        assert!(!current);
    }

    #[test]
    fn live_frame_while_live_is_off_changes_nothing() {
        let mut state = state(Stream::new(START));

        state.set_live_frame(&node(1), times(START), &mut b"jpeg".to_vec());

        assert_eq!(live_id(&state), None);
    }

    #[test]
    fn live_frame_while_live_is_off_returns_true() {
        let mut state = state(Stream::new(START));

        let current = state.set_live_frame(&node(1), times(START), &mut b"jpeg".to_vec());

        assert!(current);
    }

    #[test]
    fn end_of_live_drops_the_live_frame() {
        let mut state = live_state();
        state.set_live_frame(&node(1), times(START), &mut b"jpeg".to_vec());

        state.set_live(false);

        assert_eq!(live_id(&state), None);
    }

    #[test]
    fn new_node_drops_the_live_frame() {
        let mut state = live_state();
        state.set_live_frame(&node(1), times(START), &mut b"jpeg".to_vec());

        state.set_node(Some(&node(2)), START);

        assert_eq!(live_id(&state), None);
    }
}
