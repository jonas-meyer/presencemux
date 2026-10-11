//! The buffers of a running stream on the gadget node.

use std::io;
use std::os::fd::BorrowedFd;

use crate::v4l2::{self, BUF_FLAG_DONE, BUF_FLAG_QUEUED, MAX_BUFFERS, Mapping};

/// The number of kernel buffers.
const BUFFERS: u32 = 2;

/// The identity of a frame. Frames with the same identity have the same bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameId(pub(crate) u64);

/// A JPEG frame to send.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame<'a> {
    pub(crate) id: FrameId,
    pub(crate) jpeg: &'a [u8],
}

/// One kernel buffer and its state.
#[derive(Debug)]
struct Slot {
    index: u32,
    mapping: Mapping,
    /// Whether the kernel holds the buffer.
    in_kernel: bool,
    /// The frame in the buffer.
    frame: Option<FrameId>,
}

/// The buffers of a running stream.
#[derive(Debug)]
pub(crate) struct Output {
    slots: Vec<Slot>,
}

impl Output {
    /// Allocates and maps the buffers, and starts the stream. Frees the buffers
    /// on an error.
    pub(crate) fn start(fd: BorrowedFd<'_>) -> io::Result<Self> {
        let count = v4l2::request_buffers(fd, BUFFERS)?;
        let result = map(fd, count).and_then(|slots| {
            v4l2::stream_on(fd)?;
            Ok(Self { slots })
        });
        if result.is_err() {
            release(fd);
        }
        result
    }

    /// Maps the buffers of a stream that ran before a restart. Returns `None`
    /// and frees the buffers if no stream runs.
    pub(crate) fn adopt(fd: BorrowedFd<'_>) -> io::Result<Option<Self>> {
        // `REQBUFS` 0 fails with `EBUSY` only while a stream runs.
        match v4l2::request_buffers(fd, 0) {
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::ResourceBusy => {}
            Err(error) => return Err(error),
        }
        let mut slots = Vec::new();
        for index in 0..MAX_BUFFERS {
            let Some(buffer) = v4l2::query_buffer(fd, index)? else {
                break;
            };
            slots.push(Slot {
                index,
                mapping: Mapping::new(fd, &buffer)?,
                in_kernel: buffer.flags & (BUF_FLAG_QUEUED | BUF_FLAG_DONE) != 0,
                frame: None,
            });
        }
        Ok(Some(Self { slots }))
    }

    /// Dequeues the buffers that the kernel has sent.
    pub(crate) fn dequeue_sent(&mut self, fd: BorrowedFd<'_>) -> io::Result<()> {
        while let Some(buffer) = v4l2::dequeue_buffer(fd)? {
            if let Some(slot) = self
                .slots
                .iter_mut()
                .find(|slot| slot.index == buffer.index)
            {
                slot.in_kernel = false;
            }
        }
        Ok(())
    }

    /// Queues the frame in a buffer that the kernel does not hold. Does nothing
    /// if the kernel holds every buffer.
    pub(crate) fn send(&mut self, fd: BorrowedFd<'_>, frame: Frame<'_>) -> io::Result<()> {
        let free = |slot: &Slot| !slot.in_kernel;
        let position = (self.slots.iter())
            .position(|slot| free(slot) && slot.frame == Some(frame.id))
            .or_else(|| self.slots.iter().position(free));
        let Some(slot) = position.map(|position| &mut self.slots[position]) else {
            return Ok(());
        };
        if slot.frame != Some(frame.id) {
            slot.frame = None;
            slot.mapping.write(frame.jpeg)?;
            slot.frame = Some(frame.id);
        }
        let bytes_used = u32::try_from(frame.jpeg.len()).map_err(io::Error::other)?;
        v4l2::queue_buffer(fd, slot.index, bytes_used)?;
        slot.in_kernel = true;
        Ok(())
    }

    /// Stops the stream and frees the buffers.
    pub(crate) fn stop(self, fd: BorrowedFd<'_>) -> io::Result<()> {
        let result = v4l2::stream_off(fd);
        // The kernel frees the buffers only after the unmap.
        drop(self.slots);
        release(fd);
        result
    }
}

/// Maps the `count` buffers that the kernel allocated.
fn map(fd: BorrowedFd<'_>, count: u32) -> io::Result<Vec<Slot>> {
    (0..count)
        .map(|index| {
            let buffer = v4l2::query_buffer(fd, index)?
                .ok_or_else(|| io::Error::other(format!("buffer {index} does not exist")))?;
            Ok(Slot {
                index,
                mapping: Mapping::new(fd, &buffer)?,
                in_kernel: false,
                frame: None,
            })
        })
        .collect()
}

/// Frees the buffers. Logs a failure.
fn release(fd: BorrowedFd<'_>) {
    if let Err(error) = v4l2::request_buffers(fd, 0) {
        tracing::warn!("cannot free the buffers: {error}");
    }
}
