//! The V4L2 and UVC gadget structs of the kernel, and the ioctls on the gadget
//! node and the capture node.
//!
//! The structs follow `linux/videodev2.h` and `linux/usb/g_uvc.h` of kernel
//! 6.18 on aarch64. Each padding byte is a named field.

#![expect(unsafe_code, reason = "ioctl and mmap on the V4L2 nodes")]

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::ptr::NonNull;
use std::time::Duration;

use rustix::io::Errno;
use rustix::ioctl::{Getter, Opcode, Setter, Updater, ioctl, opcode};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use rustix::time::{ClockId, clock_gettime};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

const MEMORY_MMAP: u32 = 1;
const PIX_FMT_MJPEG: u32 = u32::from_le_bytes(*b"MJPG");
const FIELD_NONE: u32 = 1;
const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const CAP_VIDEO_OUTPUT: u32 = 0x0000_0002;
const CAP_STREAMING: u32 = 0x0400_0000;

/// The buffer flag for a buffer in the queue of the kernel.
pub(crate) const BUF_FLAG_QUEUED: u32 = 0x0002;
/// The buffer flag for a buffer that the kernel has sent.
pub(crate) const BUF_FLAG_DONE: u32 = 0x0004;
/// The buffer flag for a frame with a transfer error.
const BUF_FLAG_ERROR: u32 = 0x0040;
const BUF_FLAG_TIMESTAMP_MASK: u32 = 0xe000;
const BUF_FLAG_TIMESTAMP_MONOTONIC: u32 = 0x2000;
/// The largest number of buffers of a V4L2 queue, `VIDEO_MAX_FRAME`.
pub(crate) const MAX_BUFFERS: u32 = 64;

/// The buffer queue of a node, `enum v4l2_buf_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Queue {
    /// `V4L2_BUF_TYPE_VIDEO_CAPTURE`.
    Capture,
    /// `V4L2_BUF_TYPE_VIDEO_OUTPUT`.
    Output,
}

impl Queue {
    fn kind(self) -> u32 {
        match self {
            Self::Capture => 1,
            Self::Output => 2,
        }
    }
}

/// `struct v4l2_capability`.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_format` with the single-planar `struct v4l2_pix_format`.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct Format {
    kind: u32,
    padding: u32,
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytes_per_line: u32,
    size_image: u32,
    color_space: u32,
    private: u32,
    flags: u32,
    ycbcr_encoding: u32,
    quantization: u32,
    transfer_function: u32,
    rest: [u8; 152],
}

/// `struct v4l2_streamparm` with `struct v4l2_outputparm` or
/// `struct v4l2_captureparm`. Both start with the same fields.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct StreamParm {
    kind: u32,
    capability: u32,
    mode: u32,
    numerator: u32,
    denominator: u32,
    rest: [u8; 184],
}

/// `struct v4l2_requestbuffers`.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct RequestBuffers {
    count: u32,
    kind: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

/// `struct v4l2_buffer` for single-planar MMAP buffers. The `m` union holds
/// the mapping offset.
#[derive(Clone, Copy, Debug, FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub(crate) struct Buffer {
    pub(crate) index: u32,
    kind: u32,
    bytes_used: u32,
    pub(crate) flags: u32,
    field: u32,
    padding: u32,
    timestamp_sec: i64,
    timestamp_usec: i64,
    timecode: [u8; 16],
    sequence: u32,
    memory: u32,
    offset: u32,
    offset_padding: u32,
    length: u32,
    reserved: u32,
    request_fd: i32,
    tail: u32,
}

/// `struct v4l2_event_subscription`.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct EventSubscription {
    kind: u32,
    id: u32,
    flags: u32,
    reserved: [u32; 5],
}

/// The `u` union of `struct v4l2_event`, with the alignment of the union.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, align(8))]
struct EventPayload([u8; 64]);

/// `struct v4l2_event`.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub(crate) struct Event {
    pub(crate) kind: u32,
    padding: u32,
    payload: EventPayload,
    pending: u32,
    sequence: u32,
    timestamp_sec: i64,
    timestamp_nsec: i64,
    id: u32,
    reserved: [u32; 8],
    tail: u32,
}

impl Event {
    pub(crate) fn payload(&self) -> &[u8; 64] {
        &self.payload.0
    }
}

/// `struct uvc_request_data`.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
struct RequestData {
    length: i32,
    data: [u8; 60],
}

const _: () = {
    assert!(size_of::<Capability>() == 104);
    assert!(size_of::<Format>() == 208);
    assert!(size_of::<StreamParm>() == 204);
    assert!(size_of::<RequestBuffers>() == 20);
    assert!(size_of::<Buffer>() == 88);
    assert!(core::mem::offset_of!(Buffer, timestamp_sec) == 24);
    assert!(core::mem::offset_of!(Buffer, offset) == 64);
    assert!(size_of::<EventSubscription>() == 32);
    assert!(size_of::<Event>() == 136);
    assert!(core::mem::offset_of!(Event, payload) == 8);
    assert!(core::mem::offset_of!(Event, timestamp_sec) == 80);
    assert!(size_of::<RequestData>() == 64);
};

const VIDIOC_QUERYCAP: Opcode = opcode::read::<Capability>(b'V', 0);
const VIDIOC_G_FMT: Opcode = opcode::read_write::<Format>(b'V', 4);
const VIDIOC_S_FMT: Opcode = opcode::read_write::<Format>(b'V', 5);
const VIDIOC_REQBUFS: Opcode = opcode::read_write::<RequestBuffers>(b'V', 8);
const VIDIOC_QUERYBUF: Opcode = opcode::read_write::<Buffer>(b'V', 9);
const VIDIOC_QBUF: Opcode = opcode::read_write::<Buffer>(b'V', 15);
const VIDIOC_DQBUF: Opcode = opcode::read_write::<Buffer>(b'V', 17);
const VIDIOC_STREAMON: Opcode = opcode::write::<i32>(b'V', 18);
const VIDIOC_STREAMOFF: Opcode = opcode::write::<i32>(b'V', 19);
const VIDIOC_S_PARM: Opcode = opcode::read_write::<StreamParm>(b'V', 22);
const VIDIOC_DQEVENT: Opcode = opcode::read::<Event>(b'V', 89);
const VIDIOC_SUBSCRIBE_EVENT: Opcode = opcode::write::<EventSubscription>(b'V', 90);
const UVCIOC_SEND_RESPONSE: Opcode = opcode::write::<RequestData>(b'U', 1);

// The opcodes equal the values of the C macros on aarch64.
const _: () = {
    assert!(VIDIOC_QUERYCAP == 0x8068_5600);
    assert!(VIDIOC_G_FMT == 0xc0d0_5604);
    assert!(VIDIOC_S_FMT == 0xc0d0_5605);
    assert!(VIDIOC_REQBUFS == 0xc014_5608);
    assert!(VIDIOC_QUERYBUF == 0xc058_5609);
    assert!(VIDIOC_QBUF == 0xc058_560f);
    assert!(VIDIOC_DQBUF == 0xc058_5611);
    assert!(VIDIOC_STREAMON == 0x4004_5612);
    assert!(VIDIOC_STREAMOFF == 0x4004_5613);
    assert!(VIDIOC_S_PARM == 0xc0cc_5616);
    assert!(VIDIOC_DQEVENT == 0x8088_5659);
    assert!(VIDIOC_SUBSCRIBE_EVENT == 0x4020_565a);
    assert!(UVCIOC_SEND_RESPONSE == 0x4040_5501);
};

/// Checks that the node is a UVC gadget node with streaming output.
pub(crate) fn check_gadget(fd: BorrowedFd<'_>) -> io::Result<()> {
    check_node(fd, c"g_uvc", CAP_VIDEO_OUTPUT | CAP_STREAMING)
}

/// Checks that the node is a UVC camera node with streaming capture.
pub(crate) fn check_camera(fd: BorrowedFd<'_>) -> io::Result<()> {
    check_node(fd, c"uvcvideo", CAP_VIDEO_CAPTURE | CAP_STREAMING)
}

/// Checks the driver and the device capabilities of the node.
fn check_node(fd: BorrowedFd<'_>, expected: &CStr, caps: u32) -> io::Result<()> {
    // SAFETY: `VIDIOC_QUERYCAP` is `_IOR` with `struct v4l2_capability`, and
    // `Capability` has its layout. Each bit pattern is a valid `Capability`.
    let capability = unsafe { ioctl(fd, Getter::<VIDIOC_QUERYCAP, Capability>::new()) }?;
    let driver = CStr::from_bytes_until_nul(&capability.driver).unwrap_or_default();
    if driver != expected {
        return Err(io::Error::other(format!(
            "the node has the driver {}, expected {}",
            driver.to_string_lossy(),
            expected.to_string_lossy()
        )));
    }
    if capability.device_caps & caps != caps {
        return Err(io::Error::other(format!(
            "the node has the capabilities {:#x}, expected {caps:#x}",
            capability.device_caps
        )));
    }
    Ok(())
}

/// Sets the MJPEG format of `width` x `height`. Returns an error if the node has
/// no such format.
pub(crate) fn set_format(
    fd: BorrowedFd<'_>,
    queue: Queue,
    width: u32,
    height: u32,
) -> io::Result<()> {
    let mut format = Format::new_zeroed();
    format.kind = queue.kind();
    format.width = width;
    format.height = height;
    format.pixelformat = PIX_FMT_MJPEG;
    format.field = FIELD_NONE;
    // SAFETY: `VIDIOC_S_FMT` is `_IOWR` with `struct v4l2_format`, and `Format`
    // has its layout.
    unsafe { ioctl(fd, Updater::<VIDIOC_S_FMT, Format>::new(&mut format)) }?;
    if (format.pixelformat, format.width, format.height) != (PIX_FMT_MJPEG, width, height) {
        return Err(io::Error::other(format!(
            "the node has no MJPEG format of {width}x{height}"
        )));
    }
    Ok(())
}

/// Returns the largest frame size of the current output format.
pub(crate) fn frame_size(fd: BorrowedFd<'_>) -> io::Result<u32> {
    let mut format = Format::new_zeroed();
    format.kind = Queue::Output.kind();
    // SAFETY: `VIDIOC_G_FMT` is `_IOWR` with `struct v4l2_format`, and `Format`
    // has its layout.
    unsafe { ioctl(fd, Updater::<VIDIOC_G_FMT, Format>::new(&mut format)) }?;
    Ok(format.size_image)
}

/// Sets the frame time in seconds, as a fraction. Returns the frame time that
/// the driver chose. On the gadget node, the kernel spreads a frame over its
/// USB requests in this time. On the capture node, the camera sends frames at
/// this interval.
pub(crate) fn set_frame_time(
    fd: BorrowedFd<'_>,
    queue: Queue,
    numerator: u32,
    denominator: u32,
) -> io::Result<(u32, u32)> {
    let mut parm = StreamParm::new_zeroed();
    parm.kind = queue.kind();
    parm.numerator = numerator;
    parm.denominator = denominator;
    // SAFETY: `VIDIOC_S_PARM` is `_IOWR` with `struct v4l2_streamparm`, and
    // `StreamParm` has its layout.
    unsafe { ioctl(fd, Updater::<VIDIOC_S_PARM, StreamParm>::new(&mut parm)) }?;
    Ok((parm.numerator, parm.denominator))
}

pub(crate) fn subscribe(fd: BorrowedFd<'_>, kind: u32) -> io::Result<()> {
    let mut subscription = EventSubscription::new_zeroed();
    subscription.kind = kind;
    // SAFETY: `VIDIOC_SUBSCRIBE_EVENT` is `_IOW` with
    // `struct v4l2_event_subscription`, and `EventSubscription` has its layout.
    unsafe {
        ioctl(
            fd,
            Setter::<VIDIOC_SUBSCRIBE_EVENT, EventSubscription>::new(subscription),
        )
    }?;
    Ok(())
}

/// Returns the next event, or `None` when no event waits.
pub(crate) fn dequeue_event(fd: BorrowedFd<'_>) -> io::Result<Option<Event>> {
    // SAFETY: `VIDIOC_DQEVENT` is `_IOR` with `struct v4l2_event`, and `Event`
    // has its layout. Each bit pattern is a valid `Event`.
    match unsafe { ioctl(fd, Getter::<VIDIOC_DQEVENT, Event>::new()) } {
        Ok(event) => Ok(Some(event)),
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Sends the answer to a setup request. A negative length stalls the control
/// endpoint.
pub(crate) fn send_response(fd: BorrowedFd<'_>, length: i32, data: [u8; 60]) -> io::Result<()> {
    let request = RequestData { length, data };
    // SAFETY: `UVCIOC_SEND_RESPONSE` is `_IOW` with `struct uvc_request_data`,
    // and `RequestData` has its layout.
    unsafe {
        ioctl(
            fd,
            Setter::<UVCIOC_SEND_RESPONSE, RequestData>::new(request),
        )
    }?;
    Ok(())
}

/// Allocates `count` MMAP buffers, or frees them with a count of 0. Returns the
/// number of buffers that the kernel allocated.
pub(crate) fn request_buffers(fd: BorrowedFd<'_>, queue: Queue, count: u32) -> io::Result<u32> {
    let mut request = RequestBuffers::new_zeroed();
    request.count = count;
    request.kind = queue.kind();
    request.memory = MEMORY_MMAP;
    // SAFETY: `VIDIOC_REQBUFS` is `_IOWR` with `struct v4l2_requestbuffers`,
    // and `RequestBuffers` has its layout.
    unsafe {
        ioctl(
            fd,
            Updater::<VIDIOC_REQBUFS, RequestBuffers>::new(&mut request),
        )
    }?;
    Ok(request.count)
}

/// Returns the buffer with the index, or `None` if it does not exist.
pub(crate) fn query_buffer(
    fd: BorrowedFd<'_>,
    queue: Queue,
    index: u32,
) -> io::Result<Option<Buffer>> {
    let mut buffer = Buffer::new(queue, index);
    // SAFETY: `VIDIOC_QUERYBUF` is `_IOWR` with `struct v4l2_buffer`, and
    // `Buffer` has its layout. MMAP buffers carry no user pointers.
    match unsafe { ioctl(fd, Updater::<VIDIOC_QUERYBUF, Buffer>::new(&mut buffer)) } {
        Ok(()) => Ok(Some(buffer)),
        Err(Errno::INVAL) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Queues the buffer with `bytes_used` bytes and the current `CLOCK_MONOTONIC`
/// time. The capture queue ignores both.
pub(crate) fn queue_buffer(
    fd: BorrowedFd<'_>,
    queue: Queue,
    index: u32,
    bytes_used: u32,
) -> io::Result<()> {
    let now = clock_gettime(ClockId::Monotonic);
    let mut buffer = Buffer::new(queue, index);
    buffer.bytes_used = bytes_used;
    buffer.field = FIELD_NONE;
    buffer.timestamp_sec = now.tv_sec;
    buffer.timestamp_usec = now.tv_nsec / 1000;
    // SAFETY: `VIDIOC_QBUF` is `_IOWR` with `struct v4l2_buffer`, and `Buffer`
    // has its layout. MMAP buffers carry no user pointers.
    unsafe { ioctl(fd, Updater::<VIDIOC_QBUF, Buffer>::new(&mut buffer)) }?;
    Ok(())
}

/// Returns a buffer that the kernel has sent or filled, or `None` if no buffer
/// is ready.
pub(crate) fn dequeue_buffer(fd: BorrowedFd<'_>, queue: Queue) -> io::Result<Option<Buffer>> {
    let mut buffer = Buffer::new(queue, 0);
    // SAFETY: `VIDIOC_DQBUF` is `_IOWR` with `struct v4l2_buffer`, and `Buffer`
    // has its layout. MMAP buffers carry no user pointers.
    match unsafe { ioctl(fd, Updater::<VIDIOC_DQBUF, Buffer>::new(&mut buffer)) } {
        Ok(()) => Ok(Some(buffer)),
        Err(Errno::AGAIN) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn stream_on(fd: BorrowedFd<'_>, queue: Queue) -> io::Result<()> {
    stream::<VIDIOC_STREAMON>(fd, queue)
}

pub(crate) fn stream_off(fd: BorrowedFd<'_>, queue: Queue) -> io::Result<()> {
    stream::<VIDIOC_STREAMOFF>(fd, queue)
}

fn stream<const OPCODE: Opcode>(fd: BorrowedFd<'_>, queue: Queue) -> io::Result<()> {
    // SAFETY: The callers pass only `VIDIOC_STREAMON` or `VIDIOC_STREAMOFF`,
    // which are `_IOW` with an int. `Setter` passes a pointer to it, as the
    // kernel expects.
    unsafe { ioctl(fd, Setter::<OPCODE, i32>::new(queue.kind().cast_signed())) }?;
    Ok(())
}

/// Returns the current `CLOCK_MONOTONIC` time.
pub(crate) fn monotonic_now() -> Duration {
    let now = clock_gettime(ClockId::Monotonic);
    let seconds = u64::try_from(now.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(now.tv_nsec).unwrap_or(0);
    Duration::from_secs(seconds) + Duration::from_nanos(nanos)
}

impl Buffer {
    fn new(queue: Queue, index: u32) -> Self {
        let mut buffer = Self::new_zeroed();
        buffer.index = index;
        buffer.kind = queue.kind();
        buffer.memory = MEMORY_MMAP;
        buffer
    }

    /// Returns the `CLOCK_MONOTONIC` time of the buffer, or `None` if the
    /// driver uses another clock.
    pub(crate) fn monotonic_time(&self) -> Option<Duration> {
        if self.flags & BUF_FLAG_TIMESTAMP_MASK != BUF_FLAG_TIMESTAMP_MONOTONIC {
            return None;
        }
        let seconds = u64::try_from(self.timestamp_sec).ok()?;
        let micros = u32::try_from(self.timestamp_usec).ok()?;
        Some(Duration::from_secs(seconds) + Duration::from_micros(micros.into()))
    }
}

/// A kernel buffer that is mapped into this process. Drop unmaps it.
#[derive(Debug)]
pub(crate) struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

impl Mapping {
    pub(crate) fn new(fd: impl AsFd, buffer: &Buffer) -> io::Result<Self> {
        let len = usize::try_from(buffer.length).map_err(io::Error::other)?;
        // SAFETY: A null address lets the kernel choose the place of the new
        // mapping, which no Rust reference uses.
        let ptr = unsafe {
            mmap(
                core::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                fd,
                u64::from(buffer.offset),
            )
        }?;
        let ptr = NonNull::new(ptr.cast()).ok_or_else(|| io::Error::other("mmap returned null"))?;
        Ok(Self { ptr, len })
    }

    /// Copies the data to the start of the buffer.
    pub(crate) fn write(&mut self, data: &[u8]) -> io::Result<()> {
        if data.len() > self.len {
            return Err(io::Error::other("the frame is larger than the buffer"));
        }
        // SAFETY: The check above keeps `data.len()` within `len`, and the
        // mapping is writable for `len` bytes until drop. `&mut self` makes
        // this the only access from this process. A slice of the mapping from
        // `Dequeued::data` borrows the `CaptureStream` that owns the mapping.
        // `&mut self` excludes such a slice, and `data` therefore cannot
        // overlap the mapping.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.as_ptr(), data.len()) };
        Ok(())
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr` and `len` come from `mmap`, and no slice outlives
        // `self`.
        if let Err(error) = unsafe { munmap(self.ptr.as_ptr().cast(), self.len) } {
            tracing::warn!("cannot unmap a buffer: {error}");
        }
    }
}

/// A running capture stream with its mapped buffers. Only this type and
/// [`Dequeued`] queue the buffers of the stream. Drop unmaps the buffers and closes the node. The
/// kernel then stops the stream and frees the buffers.
#[derive(Debug)]
pub(crate) struct CaptureStream {
    /// The mapped buffers, in the order of their index.
    mappings: Vec<Mapping>,
    fd: OwnedFd,
}

impl CaptureStream {
    /// Requests `count` buffers on the capture node `fd`, maps and queues them,
    /// and starts the stream. Returns an error if the kernel allocates fewer
    /// than 2 buffers.
    pub(crate) fn start(fd: OwnedFd, count: u32) -> io::Result<Self> {
        let borrowed = fd.as_fd();
        let count = request_buffers(borrowed, Queue::Capture, count)?;
        if count < 2 {
            return Err(io::Error::other(format!(
                "the kernel allocated only {count} buffers"
            )));
        }
        let mut mappings = Vec::new();
        for index in 0..count {
            let buffer = query_buffer(borrowed, Queue::Capture, index)?
                .ok_or_else(|| io::Error::other(format!("buffer {index} does not exist")))?;
            mappings.push(Mapping::new(borrowed, &buffer)?);
            queue_buffer(borrowed, Queue::Capture, index, 0)?;
        }
        stream_on(borrowed, Queue::Capture)?;
        Ok(Self { mappings, fd })
    }

    /// Returns a filled buffer, or `None` if no buffer is ready.
    pub(crate) fn dequeue(&self) -> io::Result<Option<Dequeued<'_>>> {
        let Some(buffer) = dequeue_buffer(self.fd.as_fd(), Queue::Capture)? else {
            return Ok(None);
        };
        if self.mapping(buffer.index).is_none() {
            return Err(io::Error::other(format!(
                "the kernel filled the unknown buffer {}",
                buffer.index
            )));
        }
        Ok(Some(Dequeued {
            stream: self,
            buffer,
            queued: false,
        }))
    }

    fn mapping(&self, index: u32) -> Option<&Mapping> {
        self.mappings.get(usize::try_from(index).ok()?)
    }
}

impl AsFd for CaptureStream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// A filled capture buffer that the process holds. The kernel does not write
/// the buffer until [`Dequeued::requeue`] or drop queues it again.
#[derive(Debug)]
pub(crate) struct Dequeued<'a> {
    stream: &'a CaptureStream,
    buffer: Buffer,
    /// Whether `requeue` queued the buffer.
    queued: bool,
}

impl Dequeued<'_> {
    /// Returns whether the frame has a transfer error.
    pub(crate) fn has_error(&self) -> bool {
        self.buffer.flags & BUF_FLAG_ERROR != 0
    }

    /// Returns the `CLOCK_MONOTONIC` time of the frame, or `None` if the driver
    /// uses another clock.
    pub(crate) fn monotonic_time(&self) -> Option<Duration> {
        self.buffer.monotonic_time()
    }

    /// Returns the data of the frame.
    pub(crate) fn data(&self) -> io::Result<&[u8]> {
        let mapping = self
            .stream
            .mapping(self.buffer.index)
            .ok_or_else(|| io::Error::other("the buffer has no mapping"))?;
        let len = usize::try_from(self.buffer.bytes_used).map_err(io::Error::other)?;
        if len > mapping.len {
            return Err(io::Error::other("the frame is larger than the buffer"));
        }
        // SAFETY: The mapping is readable for `mapping.len` bytes until drop,
        // and the check above keeps `len` at most `mapping.len`. The slice
        // borrows `self`, and `self` borrows the stream that owns the mapping.
        // In this crate, only `CaptureStream` and `Dequeued` queue a buffer of
        // the stream. Other code uses the fd of the stream only for `poll`.
        // The buffer stays out of the queue until `requeue` or drop. The
        // kernel writes a capture buffer only while it is in the queue.
        Ok(unsafe { core::slice::from_raw_parts(mapping.ptr.as_ptr(), len) })
    }

    /// Queues the buffer again.
    pub(crate) fn requeue(mut self) -> io::Result<()> {
        self.queued = true;
        queue_buffer(self.stream.as_fd(), Queue::Capture, self.buffer.index, 0)
    }
}

impl Drop for Dequeued<'_> {
    fn drop(&mut self) {
        if self.queued {
            return;
        }
        let result = queue_buffer(self.stream.as_fd(), Queue::Capture, self.buffer.index, 0);
        if let Err(error) = result {
            tracing::warn!("cannot queue a capture buffer again: {error}");
        }
    }
}
