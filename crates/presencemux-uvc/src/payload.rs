//! The payloads of the frames on the bulk endpoint.
//!
//! Each frame is one payload: a 2-byte header and the JPEG. On a bulk
//! endpoint, a packet shorter than the packet size ends the payload. A payload
//! with a length that is a multiple of the packet size needs a zero-length
//! packet after it, and `f_fs` does not send one. Such a payload gets one more
//! byte after the JPEG. JPEG decoders ignore this byte.

use crate::{Format, MAX_PACKET};

/// The length of the payload header: `bHeaderLength` and `bmHeaderInfo`.
pub const HEADER_LEN: u8 = 2;
/// The largest part of a payload in one write to the endpoint. Each part
/// except the last is a multiple of the packet size, and the parts form one
/// transfer on the bus.
pub const CHUNK_LEN: usize = 64 * 1024;

/// The end-of-header bit.
const EOH: u8 = 0x80;
/// The end-of-frame bit.
const EOF: u8 = 0x02;
/// The frame ID bit. It toggles with each frame.
const FID: u8 = 0x01;

/// The error for a frame larger than [`Format::max_frame_size`]. The host
/// would cut such a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("frame of {len} bytes, the largest is {max}")]
pub struct FrameTooLarge {
    pub len: usize,
    pub max: u32,
}

/// The payload builder of a stream. It keeps the frame ID.
#[derive(Clone, Debug)]
pub struct Framer {
    format: Format,
    fid: bool,
}

impl Framer {
    pub fn new(format: Format) -> Self {
        Self { format, fid: false }
    }

    /// Returns the payload of `frame` and toggles the frame ID. A payload
    /// that the host does not get completely also uses its frame ID, and the
    /// next payload starts a new frame for the host.
    pub fn payload(&mut self, frame: &[u8]) -> Result<Vec<u8>, FrameTooLarge> {
        let max = self.format.max_frame_size();
        if frame.len() > usize::try_from(max).unwrap_or(usize::MAX) {
            return Err(FrameTooLarge {
                len: frame.len(),
                max,
            });
        }
        let mut payload = Vec::with_capacity(usize::from(HEADER_LEN) + frame.len() + 1);
        payload.extend_from_slice(&[HEADER_LEN, EOH | EOF | if self.fid { FID } else { 0 }]);
        payload.extend_from_slice(frame);
        if payload.len() % usize::from(MAX_PACKET) == 0 {
            payload.push(0);
        }
        self.fid = !self.fid;
        Ok(payload)
    }
}

/// Returns the parts of `payload` for the writes to the endpoint.
pub fn chunks(payload: &[u8]) -> impl Iterator<Item = &[u8]> {
    payload.chunks(CHUNK_LEN)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const FORMAT: Format = Format {
        width: 1920,
        height: 1080,
        frame_interval: 333_333,
    };

    #[test]
    fn payload_starts_with_the_header_and_ends_with_the_frame() {
        let mut framer = Framer::new(FORMAT);

        let payload = framer.payload(&[0xff, 0xd8, 0xff, 0xd9]).unwrap();

        assert_eq!(payload, [2, 0x82, 0xff, 0xd8, 0xff, 0xd9]);
    }

    #[test]
    fn frame_id_toggles_with_each_frame() {
        let mut framer = Framer::new(FORMAT);

        let ids: Vec<u8> = (0..3)
            .map(|_| framer.payload(&[1]).unwrap()[1] & FID)
            .collect();

        assert_eq!(ids, [0, 1, 0]);
    }

    #[test]
    fn payload_with_a_length_of_a_packet_multiple_gets_a_pad_byte() {
        let mut framer = Framer::new(FORMAT);

        let payload = framer.payload(&[7; 1022]).unwrap();

        assert_eq!(payload.len(), 1025);
        assert_eq!(payload.last(), Some(&0));
    }

    #[test]
    fn frame_larger_than_the_format_is_an_error() {
        let mut framer = Framer::new(FORMAT);
        let max = FORMAT.max_frame_size();

        let error = framer.payload(&vec![0; max as usize + 1]).unwrap_err();

        assert_eq!(
            error,
            FrameTooLarge {
                len: max as usize + 1,
                max
            }
        );
    }

    #[test]
    fn frame_of_the_largest_size_is_accepted() {
        let mut framer = Framer::new(FORMAT);
        let max = FORMAT.max_frame_size() as usize;

        let payload = framer.payload(&vec![0; max]);

        assert!(payload.is_ok());
    }

    #[test]
    fn rejected_frame_keeps_the_frame_id() {
        let mut framer = Framer::new(FORMAT);
        let too_large = vec![0; FORMAT.max_frame_size() as usize + 1];
        let _ = framer.payload(&too_large);

        let payload = framer.payload(&[1]).unwrap();

        assert_eq!(payload[1] & FID, 0);
    }

    proptest! {
        #[test]
        fn payload_never_ends_on_a_packet_boundary(len in 0..300_000_usize) {
            let mut framer = Framer::new(FORMAT);

            let payload = framer.payload(&vec![0x55; len]).unwrap();

            prop_assert_ne!(payload.len() % usize::from(MAX_PACKET), 0);
        }

        #[test]
        fn chunks_join_to_the_payload(len in 0..300_000_usize) {
            let mut framer = Framer::new(FORMAT);
            let payload = framer.payload(&vec![0x55; len]).unwrap();

            let parts: Vec<&[u8]> = chunks(&payload).collect();

            prop_assert_eq!(parts.concat(), payload);
        }

        #[test]
        fn every_chunk_but_the_last_is_a_packet_multiple(len in 0..300_000_usize) {
            let mut framer = Framer::new(FORMAT);
            let payload = framer.payload(&vec![0x55; len]).unwrap();

            let parts: Vec<&[u8]> = chunks(&payload).collect();

            for part in &parts[..parts.len() - 1] {
                prop_assert_eq!(part.len() % usize::from(MAX_PACKET), 0);
                prop_assert!(part.len() <= CHUNK_LEN);
            }
        }
    }
}
