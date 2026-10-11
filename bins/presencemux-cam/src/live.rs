//! The requantization of camera frames into live frames for the host.

use presencemux_jpeg::{
    Coefficients, Encoded, Encoder, Error, Frame, Part, Prepared, Tables, join,
};

/// The number of parts of a frame. Each part has its own thread.
const PARTS: usize = 3;
/// The quality of the live frames.
const QUALITY: u8 = 80;
/// The lowest quality of a live frame.
const MIN_QUALITY: u8 = 50;
/// The increase of the quality after a small frame.
const QUALITY_STEP: u8 = 5;
/// The largest size of a live frame.
pub(crate) const MAX_SIZE: usize = 450_000;
/// The exponent of the frame size as a function of the table scale of libjpeg.
/// The value comes from saved camera frames at quality 50 to 80.
const SIZE_EXPONENT: f64 = -0.54;
/// The target size of a lowered quality, as a fraction of the largest size.
const TARGET_FRACTION: f64 = 0.95;

/// The quality of the live frames, from [`MIN_QUALITY`] to [`QUALITY`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Quality {
    current: u8,
}

impl Quality {
    /// Creates the quality [`QUALITY`].
    pub(crate) fn new() -> Self {
        Self { current: QUALITY }
    }

    /// Returns the quality.
    pub(crate) fn get(self) -> u8 {
        self.current
    }

    /// Returns a lower quality for a frame of `size` bytes that is larger than
    /// `max_size`. The lower quality is the highest quality with a predicted
    /// size of at most 95 % of `max_size`, and at least [`MIN_QUALITY`].
    /// Returns `None` at [`MIN_QUALITY`].
    pub(crate) fn lowered(self, size: usize, max_size: usize) -> Option<Self> {
        if self.current <= MIN_QUALITY {
            return None;
        }
        let target = TARGET_FRACTION * bytes(max_size);
        let fits = |quality: u8| predicted_size(size, self.current, quality) <= target;
        let current = (MIN_QUALITY..self.current)
            .rev()
            .find(|&quality| fits(quality))
            .unwrap_or(MIN_QUALITY);
        Some(Self { current })
    }

    /// Returns the quality for the frame after a frame of `size` bytes. Raises
    /// the quality by [`QUALITY_STEP`] if the frame is smaller than three
    /// quarters of `max_size`. Keeps it at [`QUALITY`] or lower.
    pub(crate) fn raised(self, size: usize, max_size: usize) -> Self {
        if size < max_size - max_size / 4 {
            Self {
                current: (self.current + QUALITY_STEP).min(QUALITY),
            }
        } else {
            self
        }
    }
}

/// Returns the predicted size of a frame at quality `to`, from its size `size`
/// at quality `from`.
fn predicted_size(size: usize, from: u8, to: u8) -> f64 {
    bytes(size) * (scale(to) / scale(from)).powf(SIZE_EXPONENT)
}

/// Returns the scale of the libjpeg tables at `quality`, in percent.
fn scale(quality: u8) -> f64 {
    let quality = f64::from(quality.clamp(1, 100));
    if quality < 50.0 {
        5000.0 / quality
    } else {
        200.0 - 2.0 * quality
    }
}

/// Returns a byte count as a float.
fn bytes(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

/// A requantized frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Live<'a> {
    /// The JPEG data. At [`MIN_QUALITY`] it can be larger than the largest
    /// size.
    pub(crate) jpeg: &'a [u8],
    /// The quality of the frame.
    pub(crate) quality: u8,
    /// Whether the parts did not synchronize, and one part decoded the frame.
    pub(crate) one_part: bool,
}

/// A requantizer of camera frames. It keeps its tables, buffers and quality
/// from frame to frame.
#[derive(Debug)]
pub(crate) struct Requantizer {
    tables: Tables,
    encoder: Encoder,
    parts: Vec<Part>,
    encoded: Vec<Encoded>,
    out: Vec<u8>,
    quality: Quality,
    max_size: usize,
}

impl Requantizer {
    /// Creates a requantizer for frames of at most `max_size` bytes.
    pub(crate) fn new(max_size: usize) -> Self {
        Self {
            tables: Tables::new(),
            encoder: Encoder::new(QUALITY),
            parts: (0..PARTS).map(|_| Part::new()).collect(),
            encoded: (0..PARTS).map(|_| Encoded::new()).collect(),
            out: Vec::new(),
            quality: Quality::new(),
            max_size,
        }
    }

    /// Returns the quality of the next frame.
    pub(crate) fn quality(&self) -> u8 {
        self.quality.get()
    }

    /// Requantizes the JPEG frame `jpeg`. Lowers the quality until the frame
    /// has at most `max_size` bytes, or until the quality is at
    /// [`MIN_QUALITY`]. Decodes the frame again in one part if the parts do not
    /// synchronize. Returns an error if the frame does not parse or decode.
    pub(crate) fn requantize(&mut self, jpeg: &[u8]) -> Result<Live<'_>, Error> {
        let frame = Frame::parse(jpeg, &mut self.tables)?;
        decode_parts(&frame, &mut self.parts);
        // This join only checks that the parts synchronize.
        let one_part = matches!(join(&frame, &self.parts), Err(Error::NoSync));
        if one_part {
            self.parts[0].decode(&frame, 0, 1);
        }
        let count = if one_part { 1 } else { PARTS };
        let coefficients = join(&frame, &self.parts[..count])?;
        let encoded = &mut self.encoded[..count];
        let mut quality = self.quality;
        let mut lowered = false;
        loop {
            self.encoder.set_quality(quality.get());
            let prepared = self.encoder.prepare(&frame);
            encode_parts(&prepared, &coefficients, encoded);
            prepared.write(encoded, &mut self.out);
            if self.out.len() <= self.max_size {
                break;
            }
            let Some(lower) = quality.lowered(self.out.len(), self.max_size) else {
                break;
            };
            quality = lower;
            lowered = true;
        }
        self.quality = if lowered {
            quality
        } else {
            quality.raised(self.out.len(), self.max_size)
        };
        Ok(Live {
            jpeg: &self.out,
            quality: quality.get(),
            one_part,
        })
    }
}

/// Decodes the parts of `frame`. Part 0 runs on the calling thread, and each
/// other part on a new thread.
fn decode_parts(frame: &Frame<'_>, parts: &mut [Part]) {
    let count = parts.len();
    let Some((first, rest)) = parts.split_first_mut() else {
        return;
    };
    std::thread::scope(|scope| {
        for (index, part) in rest.iter_mut().enumerate() {
            scope.spawn(move || part.decode(frame, index + 1, count));
        }
        first.decode(frame, 0, count);
    });
}

/// Encodes the parts of `coefficients` into `encoded`. Part 0 runs on the
/// calling thread, and each other part on a new thread.
fn encode_parts(prepared: &Prepared<'_>, coefficients: &Coefficients<'_>, encoded: &mut [Encoded]) {
    let Some((first, rest)) = encoded.split_first_mut() else {
        return;
    };
    std::thread::scope(|scope| {
        for (index, out) in rest.iter_mut().enumerate() {
            scope.spawn(move || prepared.encode(coefficients, index + 1, out));
        }
        prepared.encode(coefficients, 0, first);
    });
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use presencemux_jpeg::decode;

    use super::*;

    /// A 1280x720 4:2:2 frame of SMPTE color bars from GStreamer `jpegenc` at
    /// quality 90. Three parts synchronize on it.
    const BARS: &[u8] = include_bytes!("../fixtures/smpte-422.jpg");
    /// A 128x64 4:2:2 frame from libjpeg. Three parts do not synchronize on it.
    const SMALL: &[u8] =
        include_bytes!("../../../crates/presencemux-jpeg/fixtures/libjpeg-422.jpg");

    /// Requantizes `jpeg` to `quality` in one part, with the API of
    /// `presencemux-jpeg` only.
    fn reference(jpeg: &[u8], quality: u8) -> Vec<u8> {
        let mut tables = Tables::new();
        let frame = Frame::parse(jpeg, &mut tables).unwrap();
        let mut parts = [Part::new()];
        let coefficients = decode(&frame, &mut parts).unwrap();
        let mut encoder = Encoder::new(quality);
        let prepared = encoder.prepare(&frame);
        let mut encoded = [Encoded::new()];
        prepared.encode(&coefficients, 0, &mut encoded[0]);
        let mut out = Vec::new();
        prepared.write(&encoded, &mut out);
        out
    }

    /// Returns the limit one byte below the size of `jpeg` at quality 80.
    fn limit_below_quality_80(jpeg: &[u8]) -> usize {
        reference(jpeg, 80).len() - 1
    }

    #[test]
    fn color_bars_decode_in_three_parts() {
        let mut requantizer = Requantizer::new(MAX_SIZE);

        let live = requantizer.requantize(BARS).unwrap();

        assert!(!live.one_part);
    }

    #[test]
    fn color_bars_equal_the_reference_at_quality_80() {
        let mut requantizer = Requantizer::new(MAX_SIZE);

        let live = requantizer.requantize(BARS).unwrap();

        assert_eq!(live.quality, 80);
        assert_eq!(live.jpeg, reference(BARS, 80));
    }

    #[test]
    fn small_frame_decodes_in_one_part() {
        let mut requantizer = Requantizer::new(MAX_SIZE);

        let live = requantizer.requantize(SMALL).unwrap();

        assert!(live.one_part);
    }

    #[test]
    fn small_frame_equals_the_reference_at_quality_80() {
        let mut requantizer = Requantizer::new(MAX_SIZE);

        let live = requantizer.requantize(SMALL).unwrap();

        assert_eq!(live.jpeg, reference(SMALL, 80));
    }

    #[test]
    fn frame_over_the_limit_fits_the_limit_at_a_lower_quality() {
        let limit = limit_below_quality_80(BARS);
        let mut requantizer = Requantizer::new(limit);

        let live = requantizer.requantize(BARS).unwrap();

        assert!(live.quality < 80);
        assert!(live.jpeg.len() <= limit);
    }

    #[test]
    fn frame_over_the_limit_equals_the_reference_at_its_quality() {
        let mut requantizer = Requantizer::new(limit_below_quality_80(BARS));

        let live = requantizer.requantize(BARS).unwrap();

        assert_eq!(live.jpeg, reference(BARS, live.quality));
    }

    #[test]
    fn next_frame_starts_at_the_quality_of_a_frame_over_the_limit() {
        let mut requantizer = Requantizer::new(limit_below_quality_80(BARS));

        let used = requantizer.requantize(BARS).unwrap().quality;

        assert_eq!(requantizer.quality(), used);
    }

    #[test]
    fn frame_too_large_at_every_quality_gets_quality_50() {
        let mut requantizer = Requantizer::new(1);

        let live = requantizer.requantize(BARS).unwrap();

        assert_eq!(live.quality, 50);
        assert_eq!(live.jpeg, reference(BARS, 50));
    }

    #[test]
    fn half_a_frame_gives_an_error() {
        let mut requantizer = Requantizer::new(MAX_SIZE);

        let result = requantizer.requantize(&BARS[..BARS.len() / 2]);

        assert!(result.is_err());
    }

    #[test]
    fn frame_after_half_a_frame_equals_the_reference() {
        let mut requantizer = Requantizer::new(MAX_SIZE);
        let _ = requantizer.requantize(&BARS[..BARS.len() / 2]);

        let live = requantizer.requantize(BARS).unwrap();

        assert_eq!(live.jpeg, reference(BARS, 80));
    }

    #[test]
    fn quality_starts_at_80() {
        assert_eq!(Quality::new().get(), 80);
    }

    #[test]
    fn frame_10_percent_over_the_limit_lowers_the_quality_to_73() {
        let lowered = Quality::new().lowered(440_000, 400_000);

        assert_eq!(lowered.map(Quality::get), Some(73));
    }

    #[test]
    fn frame_twice_the_limit_lowers_the_quality_to_50() {
        let lowered = Quality::new().lowered(800_000, 400_000);

        assert_eq!(lowered.map(Quality::get), Some(50));
    }

    #[test]
    fn frame_over_the_limit_at_quality_50_gives_no_lower_quality() {
        let quality = Quality { current: 50 };

        let lowered = quality.lowered(800_000, 400_000);

        assert_eq!(lowered, None);
    }

    #[test]
    fn small_frame_raises_the_quality_by_5() {
        let quality = Quality { current: 70 };

        let raised = quality.raised(299_999, 400_000);

        assert_eq!(raised.get(), 75);
    }

    #[test]
    fn frame_of_three_quarters_of_the_limit_keeps_the_quality() {
        let quality = Quality { current: 70 };

        let raised = quality.raised(300_000, 400_000);

        assert_eq!(raised.get(), 70);
    }

    #[test]
    fn small_frame_keeps_the_quality_at_80() {
        let raised = Quality::new().raised(0, 400_000);

        assert_eq!(raised.get(), 80);
    }

    /// Returns the saved camera frames in the directory
    /// `PRESENCEMUX_JPEG_FRAMES`, in name order.
    fn saved_frames() -> Vec<(PathBuf, Vec<u8>)> {
        let dir = std::env::var("PRESENCEMUX_JPEG_FRAMES").expect("PRESENCEMUX_JPEG_FRAMES");
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "jpg"))
            .collect();
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let jpeg = std::fs::read(&path).unwrap();
                (path, jpeg)
            })
            .collect()
    }

    #[test]
    #[ignore = "needs the saved camera frames in the directory PRESENCEMUX_JPEG_FRAMES"]
    fn saved_camera_frames_decode_in_three_parts() {
        let mut requantizer = Requantizer::new(MAX_SIZE);

        let failures: Vec<PathBuf> = saved_frames()
            .into_iter()
            .filter(|(_, jpeg)| requantizer.requantize(jpeg).unwrap().one_part)
            .map(|(path, _)| path)
            .collect();

        assert_eq!(failures, Vec::<PathBuf>::new());
    }

    #[test]
    #[ignore = "needs the saved camera frames in the directory PRESENCEMUX_JPEG_FRAMES"]
    fn saved_camera_frames_fit_the_limit_at_quality_80() {
        let mut requantizer = Requantizer::new(MAX_SIZE);

        let failures: Vec<PathBuf> = saved_frames()
            .into_iter()
            .filter(|(_, jpeg)| {
                let live = requantizer.requantize(jpeg).unwrap();
                live.quality != 80 || live.jpeg.len() > MAX_SIZE
            })
            .map(|(path, _)| path)
            .collect();

        assert_eq!(failures, Vec::<PathBuf>::new());
    }
}
