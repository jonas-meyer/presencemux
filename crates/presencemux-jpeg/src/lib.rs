//! A pure JPEG requantizer for baseline frames. It decodes the entropy-coded
//! data, requantizes the DCT coefficients, and encodes them again.
//!
//! The decoder splits one frame into parts, and separate threads can decode
//! the parts. The frame needs no restart markers. The crate has no I/O and
//! starts no threads.
//!
//! The steps for one frame:
//!
//! 1. `Frame::parse` reads the headers and updates the decode `Tables`.
//! 2. `Part::decode` decodes each part, and `join` joins the parts into the
//!    `Coefficients`.
//! 3. `Encoder::prepare` gives a `Prepared` encoder. `Prepared::encode`
//!    requantizes and encodes each part.
//! 4. `Prepared::write` writes the output frame.

#![forbid(unsafe_code)]

mod decode;
mod encode;
mod header;
mod huffman;
mod requant;
#[cfg(test)]
mod tests;

pub use decode::{Coefficients, Part, Tables, decode, join};
pub use encode::{Encoded, Encoder, Prepared};
pub use header::Frame;

/// The largest frame, in blocks of 8x8 samples. A 3840x2160 frame at 4:2:2
/// has 259,200 blocks.
pub const MAX_BLOCKS: usize = 1 << 19;

/// An error in a JPEG frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum Error {
    /// The data does not start with a JPEG start-of-image marker.
    #[error("not a JPEG frame")]
    NotJpeg,
    /// The headers are not valid.
    #[error("invalid header: {0}")]
    Header(&'static str),
    /// The frame uses a feature that the crate does not support.
    #[error("unsupported: {0}")]
    Unsupported(Feature),
    /// The data ends before the end of the headers or of the last block.
    #[error("the frame is truncated")]
    Truncated,
    /// The entropy-coded data is not valid.
    #[error("corrupt entropy-coded data")]
    Corrupt,
    /// The parts of a frame did not synchronize. Decode the frame again with
    /// one part.
    #[error("the parts did not synchronize")]
    NoSync,
}

/// A JPEG feature that the crate does not support.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum Feature {
    /// Progressive coding.
    #[error("progressive frames")]
    Progressive,
    /// Lossless, hierarchical or arithmetic coding.
    #[error("lossless, hierarchical or arithmetic frames")]
    OtherCoding,
    /// A restart interval other than 0.
    #[error("restart markers")]
    RestartMarkers,
    /// Samples other than 8 bits.
    #[error("samples other than 8 bits")]
    Precision,
    /// A height of 0, with the height in a DNL marker.
    #[error("the height from a DNL marker")]
    HeightFromDnl,
    /// More than one scan, or a scan without all components.
    #[error("more than one scan")]
    MultipleScans,
    /// More than `MAX_BLOCKS` blocks.
    #[error("frames larger than MAX_BLOCKS")]
    TooLarge,
}
