//! The slates: still images and loops of JPEG frames from files.
//!
//! The worker reads each slate file at start and checks each frame. A file
//! with one frame is a still image. A file with more frames is a loop of raw
//! MJPEG, one JPEG after the other.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::output::{Frame, FrameId};
use crate::{HEIGHT, WIDTH};

/// The largest size of a slate frame, the same as for a live frame.
const MAX_FRAME_SIZE: usize = crate::live::MAX_SIZE;
/// The largest number of frames of a loop, 30 s at 30 fps.
const MAX_FRAMES: usize = 900;
/// The largest size of all slate files together.
const MAX_TOTAL_SIZE: u64 = 64 * 1024 * 1024;
/// The top bit of the identities of slate frames. Live frame identities start
/// at 1, and blank is 0.
const SLATE_ID: u64 = 1 << 63;

/// A slate with its frames.
#[derive(Debug)]
pub(crate) struct Slate {
    /// The content of the slate file.
    data: Vec<u8>,
    /// The position of each frame in `data`, in the order of the file.
    frames: Vec<Range<usize>>,
    /// The identity of the first frame.
    first_id: u64,
}

impl Slate {
    /// Returns the identity of the slate.
    pub(crate) fn id(&self) -> u64 {
        self.first_id
    }

    /// Returns the number of frames.
    pub(crate) fn len(&self) -> usize {
        self.frames.len()
    }

    /// Returns the frame at `index`, modulo the number of frames.
    pub(crate) fn frame(&self, index: usize) -> Frame<'_> {
        let index = index % self.frames.len();
        Frame {
            id: FrameId(self.first_id + index as u64),
            jpeg: &self.data[self.frames[index].clone()],
        }
    }
}

/// The slates of the configuration.
#[derive(Debug, Default)]
pub(crate) struct Slates {
    /// The slate or the error of each name.
    slates: BTreeMap<String, Result<Slate, String>>,
}

impl Slates {
    /// Reads and checks the slate file of each name in `files`. An entry of
    /// `files` can also hold the error of its slate table. Logs each error.
    pub(crate) fn load(files: &BTreeMap<String, Result<PathBuf, String>>) -> Self {
        let mut slates = BTreeMap::new();
        let mut total = 0;
        for (index, (name, file)) in files.iter().enumerate() {
            let first_id = SLATE_ID | (index as u64) << 32;
            let slate = file.clone().and_then(|path| {
                let (data, frames) = load_file(&path, &mut total)?;
                let slate = Slate {
                    data,
                    frames,
                    first_id,
                };
                if slate.len() == 1 {
                    tracing::info!("slate {name}: a still image from {}", path.display());
                } else {
                    tracing::info!(
                        "slate {name}: a loop of {} frames from {}",
                        slate.len(),
                        path.display()
                    );
                }
                Ok(slate)
            });
            if let Err(error) = &slate {
                tracing::warn!("slate {name}: {error}");
            }
            slates.insert(name.clone(), slate);
        }
        Self { slates }
    }

    /// Returns the slate `name`, or its error.
    pub(crate) fn get<'a>(&'a self, name: &'a str) -> Result<&'a Slate, SlateError<'a>> {
        match self.slates.get(name) {
            Some(Ok(slate)) => Ok(slate),
            Some(Err(error)) => Err(SlateError::Invalid { name, error }),
            None => Err(SlateError::Unknown { name }),
        }
    }

    /// Returns slates with one slate `name` of the frames in `data`.
    #[cfg(test)]
    pub(crate) fn with_slate(name: &str, data: &[u8]) -> Self {
        let frames = split(data).unwrap();
        let slate = Slate {
            data: data.to_vec(),
            frames,
            first_id: SLATE_ID,
        };
        Self {
            slates: BTreeMap::from([(name.to_owned(), Ok(slate))]),
        }
    }

    /// Returns slates with one slate `name` and its error.
    #[cfg(test)]
    pub(crate) fn with_error(name: &str, error: &str) -> Self {
        Self {
            slates: BTreeMap::from([(name.to_owned(), Err(error.to_owned()))]),
        }
    }
}

/// An error of a slate.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SlateError<'a> {
    /// The configuration has no slate with this name.
    #[error("no slate named {name}")]
    Unknown { name: &'a str },
    /// The slate table or the slate file is not valid.
    #[error("slate {name}: {error}")]
    Invalid { name: &'a str, error: &'a str },
}

/// Reads the slate file at `path`, and returns its data and the position of
/// each frame. Returns an error if the file makes `total` larger than
/// [`MAX_TOTAL_SIZE`]. Otherwise adds the size of the file to `total`.
fn load_file(path: &Path, total: &mut u64) -> Result<(Vec<u8>, Vec<Range<usize>>), String> {
    let cannot_read = |error: std::io::Error| format!("cannot read {}: {error}", path.display());
    let file = File::open(path).map_err(cannot_read)?;
    if !file.metadata().map_err(cannot_read)?.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let budget = MAX_TOTAL_SIZE.saturating_sub(*total);
    let mut data = Vec::new();
    let len = file
        .take(budget + 1)
        .read_to_end(&mut data)
        .map_err(cannot_read)?;
    if len as u64 > budget {
        return Err(format!(
            "the slate files are larger than {} MiB together",
            MAX_TOTAL_SIZE / 1024 / 1024
        ));
    }
    *total += len as u64;
    let frames = split(&data)?;
    Ok((data, frames))
}

/// Splits `data` into JPEG frames, checks each frame, and returns the position
/// of each frame.
fn split(data: &[u8]) -> Result<Vec<Range<usize>>, String> {
    let mut frames = Vec::new();
    let mut start = 0;
    while start < data.len() {
        if frames.len() == MAX_FRAMES {
            return Err(format!("more than {MAX_FRAMES} frames"));
        }
        let len = frame_len(&data[start..])
            .map_err(|error| format!("frame {}: {error}", frames.len()))?;
        frames.push(start..start + len);
        start += len;
    }
    if frames.is_empty() {
        return Err("the file is empty".to_owned());
    }
    Ok(frames)
}

/// An error of a slate frame.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
enum FrameError {
    #[error("not a JPEG frame")]
    NotJpeg,
    #[error("the frame is truncated")]
    Truncated,
    #[error("a segment length below 2")]
    SegmentLength,
    /// A coding other than baseline.
    #[error("not a baseline JPEG frame")]
    Coding,
    #[error("samples other than 8 bits")]
    Precision,
    /// A size other than the size of the gadget.
    #[error("{width}x{height}, not {WIDTH}x{HEIGHT}")]
    Size { width: u16, height: u16 },
    /// A number of components other than 3.
    #[error("{0} components, not 3")]
    Components(u8),
    /// A sampling other than 4:2:0 or 4:2:2.
    #[error("a sampling other than 4:2:0 or 4:2:2")]
    Sampling,
    #[error("a scan before the frame header")]
    NoFrameHeader,
    /// A frame larger than [`MAX_FRAME_SIZE`].
    #[error("{0} bytes, more than {MAX_FRAME_SIZE}")]
    TooLarge(usize),
}

/// Checks the JPEG frame at the start of `data`, and returns its length with
/// the end-of-image marker.
fn frame_len(data: &[u8]) -> Result<usize, FrameError> {
    if !data.starts_with(&[0xFF, 0xD8]) {
        return Err(FrameError::NotJpeg);
    }
    let mut pos = 2;
    let mut sized = false;
    loop {
        // A marker can have fill bytes of 0xFF before it.
        while data.get(pos) == Some(&0xFF) && data.get(pos + 1) == Some(&0xFF) {
            pos += 1;
        }
        if data.get(pos) != Some(&0xFF) {
            return Err(FrameError::Truncated);
        }
        let marker = *data.get(pos + 1).ok_or(FrameError::Truncated)?;
        pos += 2;
        match marker {
            0xD9 => break,
            0xD0..=0xD7 | 0x01 => continue,
            _ => {}
        }
        let len = data
            .get(pos..pos + 2)
            .map(|len| usize::from(u16::from_be_bytes([len[0], len[1]])))
            .ok_or(FrameError::Truncated)?;
        if len < 2 {
            return Err(FrameError::SegmentLength);
        }
        let segment = data.get(pos + 2..pos + len).ok_or(FrameError::Truncated)?;
        match marker {
            0xC0 => {
                check_frame_header(segment)?;
                sized = true;
            }
            0xC1..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                return Err(FrameError::Coding);
            }
            0xDA if !sized => return Err(FrameError::NoFrameHeader),
            _ => {}
        }
        pos += len;
        if marker == 0xDA {
            pos = scan_end(data, pos)?;
        }
    }
    if pos > MAX_FRAME_SIZE {
        return Err(FrameError::TooLarge(pos));
    }
    Ok(pos)
}

/// Checks a baseline frame header: 8-bit samples, the size of the gadget, and
/// three components at 4:2:0 or 4:2:2.
fn check_frame_header(segment: &[u8]) -> Result<(), FrameError> {
    let header = segment.get(..6).ok_or(FrameError::Truncated)?;
    if header[0] != 8 {
        return Err(FrameError::Precision);
    }
    let height = u16::from_be_bytes([header[1], header[2]]);
    let width = u16::from_be_bytes([header[3], header[4]]);
    if (u32::from(width), u32::from(height)) != (WIDTH, HEIGHT) {
        return Err(FrameError::Size { width, height });
    }
    if header[5] != 3 {
        return Err(FrameError::Components(header[5]));
    }
    let components = segment.get(6..15).ok_or(FrameError::Truncated)?;
    match [components[1], components[4], components[7]] {
        [0x22 | 0x21, 0x11, 0x11] => Ok(()),
        _ => Err(FrameError::Sampling),
    }
}

/// Returns the position of the marker after the entropy-coded data that starts
/// at `pos`. Skips stuffed zero bytes and restart markers.
fn scan_end(data: &[u8], mut pos: usize) -> Result<usize, FrameError> {
    loop {
        let offset = data
            .get(pos..)
            .and_then(|rest| rest.iter().position(|&byte| byte == 0xFF))
            .ok_or(FrameError::Truncated)?;
        pos += offset;
        match data.get(pos + 1) {
            None => return Err(FrameError::Truncated),
            Some(0x00 | 0xD0..=0xD7 | 0xFF) => pos += 1,
            Some(_) => return Ok(pos),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use proptest::collection::vec;
    use proptest::prelude::*;

    use super::*;

    const BLANK_JPEG: &[u8] = include_bytes!("../assets/blank.jpg");
    const AWAY_JPEG: &[u8] = include_bytes!("../assets/slates/away.jpg");
    const BRB_JPEG: &[u8] = include_bytes!("../assets/slates/brb.jpg");
    const STILL_HERE_JPEG: &[u8] = include_bytes!("../assets/slates/still-here.jpg");
    const BARS_JPEG: &[u8] = include_bytes!("../fixtures/smpte-422.jpg");

    /// Returns the position of the first SOF0 marker in `jpeg`.
    fn sof0(jpeg: &[u8]) -> usize {
        jpeg.windows(2)
            .position(|bytes| bytes == [0xFF, 0xC0])
            .unwrap()
    }

    /// Returns `jpeg` with its SOF0 marker changed to `marker`.
    fn with_sof(jpeg: &[u8], marker: u8) -> Vec<u8> {
        let mut changed = jpeg.to_vec();
        changed[sof0(jpeg) + 1] = marker;
        changed
    }

    /// Returns `jpeg` with the sampling of its first component changed to
    /// `sampling`.
    fn with_sampling(jpeg: &[u8], sampling: u8) -> Vec<u8> {
        let mut changed = jpeg.to_vec();
        changed[sof0(jpeg) + 11] = sampling;
        changed
    }

    /// Returns a temporary directory with a file `name` with `content`.
    fn file(name: &str, content: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        (dir, path)
    }

    /// Returns the slate files of one slate `name`.
    fn files(name: &str, file: Result<PathBuf, &str>) -> BTreeMap<String, Result<PathBuf, String>> {
        BTreeMap::from([(name.to_owned(), file.map_err(str::to_owned))])
    }

    #[test]
    fn away_slate_is_a_valid_frame() {
        assert_eq!(frame_len(AWAY_JPEG), Ok(AWAY_JPEG.len()));
    }

    #[test]
    fn brb_slate_is_a_valid_frame() {
        assert_eq!(frame_len(BRB_JPEG), Ok(BRB_JPEG.len()));
    }

    #[test]
    fn still_here_slate_is_a_valid_frame() {
        assert_eq!(frame_len(STILL_HERE_JPEG), Ok(STILL_HERE_JPEG.len()));
    }

    #[test]
    fn blank_is_a_valid_frame() {
        assert_eq!(frame_len(BLANK_JPEG), Ok(BLANK_JPEG.len()));
    }

    #[test]
    fn frame_of_1280x720_has_the_wrong_size() {
        assert_eq!(
            frame_len(BARS_JPEG),
            Err(FrameError::Size {
                width: 1280,
                height: 720
            })
        );
    }

    #[test]
    fn png_is_not_a_jpeg_frame() {
        assert_eq!(frame_len(b"\x89PNG\r\n"), Err(FrameError::NotJpeg));
    }

    #[test]
    fn half_a_frame_is_truncated() {
        let half = &AWAY_JPEG[..AWAY_JPEG.len() / 2];

        assert_eq!(frame_len(half), Err(FrameError::Truncated));
    }

    #[test]
    fn segment_length_of_1_is_an_error() {
        let data = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x01];

        assert_eq!(frame_len(&data), Err(FrameError::SegmentLength));
    }

    #[test]
    fn progressive_frame_is_not_baseline() {
        let progressive = with_sof(AWAY_JPEG, 0xC2);

        assert_eq!(frame_len(&progressive), Err(FrameError::Coding));
    }

    #[test]
    fn extended_sequential_frame_is_not_baseline() {
        let extended = with_sof(AWAY_JPEG, 0xC1);

        assert_eq!(frame_len(&extended), Err(FrameError::Coding));
    }

    #[test]
    fn frame_at_4_4_4_has_the_wrong_sampling() {
        let full = with_sampling(AWAY_JPEG, 0x11);

        assert_eq!(frame_len(&full), Err(FrameError::Sampling));
    }

    #[test]
    fn loop_of_three_frames_splits_into_three_frames() {
        let data = [AWAY_JPEG, BLANK_JPEG, AWAY_JPEG].concat();

        let frames = split(&data).unwrap();

        assert_eq!(frames.len(), 3);
    }

    #[test]
    fn second_frame_of_a_loop_starts_after_the_first() {
        let data = [AWAY_JPEG, BLANK_JPEG].concat();

        let frames = split(&data).unwrap();

        assert_eq!(&data[frames[1].clone()], BLANK_JPEG);
    }

    #[test]
    fn loop_with_a_bad_frame_names_the_frame() {
        let data = [AWAY_JPEG, BARS_JPEG].concat();

        let error = split(&data).unwrap_err();

        assert_eq!(error, "frame 1: 1280x720, not 1920x1080");
    }

    #[test]
    fn loop_of_901_frames_is_too_long() {
        let data = BLANK_JPEG.repeat(901);

        let error = split(&data).unwrap_err();

        assert_eq!(error, "more than 900 frames");
    }

    #[test]
    fn empty_file_is_an_error() {
        let error = split(&[]).unwrap_err();

        assert_eq!(error, "the file is empty");
    }

    #[test]
    fn slate_with_a_missing_file_gives_cannot_read() {
        let slates = Slates::load(&files("away", Ok(PathBuf::from("/nonexistent/away.jpg"))));

        let error = slates.get("away").unwrap_err().to_string();

        assert_eq!(
            error,
            "slate away: cannot read /nonexistent/away.jpg: No such file or directory (os error 2)"
        );
    }

    #[test]
    fn slate_with_a_directory_is_not_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let slates = Slates::load(&files("away", Ok(dir.path().to_owned())));

        let error = slates.get("away").unwrap_err().to_string();

        assert_eq!(
            error,
            format!("slate away: {} is not a regular file", dir.path().display())
        );
    }

    #[test]
    fn slate_file_over_the_total_size_is_an_error() {
        let (_dir, path) = file("away.jpg", AWAY_JPEG);
        let mut total = MAX_TOTAL_SIZE - 1;

        let error = load_file(&path, &mut total).unwrap_err();

        assert_eq!(error, "the slate files are larger than 64 MiB together");
    }

    #[test]
    fn slate_with_an_error_in_its_table_gives_the_error() {
        let slates = Slates::load(&files("away", Err("missing field `file`")));

        let error = slates.get("away").unwrap_err().to_string();

        assert_eq!(error, "slate away: missing field `file`");
    }

    #[test]
    fn unknown_slate_gives_no_slate_named() {
        let slates = Slates::default();

        let error = slates.get("brb").unwrap_err().to_string();

        assert_eq!(error, "no slate named brb");
    }

    #[test]
    fn two_slates_have_different_identities() {
        let (_away_dir, away) = file("away.jpg", AWAY_JPEG);
        let (_brb_dir, brb) = file("brb.jpg", BRB_JPEG);
        let mut files = files("away", Ok(away));
        files.insert("brb".to_owned(), Ok(brb));
        let slates = Slates::load(&files);

        let ids = [
            slates.get("away").unwrap().id(),
            slates.get("brb").unwrap().id(),
        ];

        assert_ne!(ids[0], ids[1]);
    }

    #[test]
    fn frame_index_past_the_end_gives_the_first_frame() {
        let slates = Slates::with_slate("away", &[AWAY_JPEG, BLANK_JPEG].concat());
        let slate = slates.get("away").unwrap();

        let frame = slate.frame(2);

        assert_eq!(frame.jpeg, AWAY_JPEG);
    }

    #[test]
    fn slate_frames_do_not_have_the_identity_of_blank() {
        let slates = Slates::with_slate("away", AWAY_JPEG);
        let slate = slates.get("away").unwrap();

        let frame = slate.frame(0);

        assert_ne!(frame.id, crate::BLANK.id);
    }

    proptest! {
        #[test]
        fn frame_len_does_not_panic_on_random_bytes(data in vec(any::<u8>(), 0..512)) {
            let _ = frame_len(&data);
        }

        #[test]
        fn frame_len_does_not_panic_on_a_changed_slate(
            changes in vec((0..AWAY_JPEG.len(), any::<u8>()), 1..16),
        ) {
            let mut data = AWAY_JPEG.to_vec();
            for (position, byte) in changes {
                data[position] = byte;
            }
            let _ = frame_len(&data);
        }
    }
}
