//! The USB Video Class function of the cam worker, without I/O.
//!
//! The cam worker implements the camera function of the gadget in userspace,
//! through FunctionFS, with a bulk endpoint (see `docs/ffs-camera.md`). This
//! crate holds the parts that need no I/O:
//!
//! - [`descriptors`] and [`strings`]: the bytes that the worker writes to
//!   `ep0` before the bind.
//! - [`Function`]: the answers to the setup requests of the host, and the
//!   start and the stop of the stream.
//! - [`Framer`] and [`chunks`]: the payload of each frame on the bulk
//!   endpoint.

mod descriptors;
mod function;
mod payload;

pub use descriptors::{
    CONTROL_INTERFACE, ENDPOINT_ADDRESS, MAX_PACKET, NAME, STREAMING_INTERFACE, descriptors,
    strings,
};
pub use function::{Action, Answer, Event, Function, Setup};
pub use payload::{CHUNK_LEN, FrameTooLarge, Framer, HEADER_LEN, chunks};

/// The only video format of the camera: MJPEG with one frame size and one
/// frame interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub width: u16,
    pub height: u16,
    /// The frame interval in units of 100 ns.
    pub frame_interval: u32,
}

impl Format {
    /// Returns the largest frame size in bytes, two bytes for each pixel. The
    /// frame descriptor gives it as `dwMaxVideoFrameBufferSize`.
    pub fn max_frame_size(self) -> u32 {
        u32::from(self.width) * u32::from(self.height) * 2
    }

    /// Returns the largest payload size in bytes: one header and the largest
    /// frame.
    pub fn max_payload_size(self) -> u32 {
        self.max_frame_size() + u32::from(HEADER_LEN)
    }
}
