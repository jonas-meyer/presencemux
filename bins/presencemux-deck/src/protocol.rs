//! The HID protocol of the Stream Deck Neo. It turns images and settings into
//! report bytes, and input reports into presses and releases. It has no I/O.
//!
//! `docs/deck.md` (Device) holds the layouts, from the documentation of Elgato
//! and from tests on the device.

use std::time::Duration;

/// The number of keys.
pub(crate) const KEYS: u8 = 8;

/// The length of an output report.
pub(crate) const OUTPUT_LEN: usize = 1024;

/// The length of the header of an image report.
const HEADER_LEN: usize = 8;

/// The number of image bytes in one output report.
const CHUNK_LEN: usize = OUTPUT_LEN - HEADER_LEN;

/// The length of a feature report.
pub(crate) const FEATURE_LEN: usize = 32;

/// The length of the buffer for an input report.
pub(crate) const INPUT_LEN: usize = 512;

/// The number of inputs in an input report: the keys, then the touch points.
const INPUTS: usize = 10;

/// An output report.
pub(crate) type OutputReport = [u8; OUTPUT_LEN];

/// A feature report.
pub(crate) type FeatureReport = [u8; FEATURE_LEN];

/// A key, numbered row by row from the top left.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Key(u8);

impl Key {
    /// Returns the key with `index`, or `None` for an index of 8 or more.
    #[cfg(test)]
    pub(crate) const fn new(index: u8) -> Option<Self> {
        if index < KEYS {
            Some(Self(index))
        } else {
            None
        }
    }

    pub(crate) const fn index(self) -> u8 {
        self.0
    }

    /// Returns every key in order.
    pub(crate) const fn all() -> [Self; KEYS as usize] {
        let mut keys = [Self(0); KEYS as usize];
        let mut index = 0;
        while index < KEYS {
            keys[index as usize] = Self(index);
            index += 1;
        }
        keys
    }
}

/// A touch point below the keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Side {
    Left,
    Right,
}

/// An input of the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Input {
    Key(Key),
    TouchPoint(Side),
}

impl Input {
    /// Returns every input in the order of the reports: the keys, then the
    /// touch points.
    fn all() -> impl Iterator<Item = Self> {
        Key::all()
            .into_iter()
            .map(Self::Key)
            .chain([Side::Left, Side::Right].map(Self::TouchPoint))
    }

    /// Returns the index of the input in reports.
    const fn index(self) -> u8 {
        match self {
            Self::Key(key) => key.index(),
            Self::TouchPoint(Side::Left) => 8,
            Self::TouchPoint(Side::Right) => 9,
        }
    }
}

/// A region of the LCD that takes an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Region {
    Input(Input),
    InfoBar,
}

impl Region {
    /// Returns the command and the index of the image report of the region.
    const fn command(self) -> (u8, u8) {
        match self {
            Self::Input(input) => (0x07, input.index()),
            Self::InfoBar => (0x0b, 0),
        }
    }
}

/// An image too large for the chunk numbers of the image reports.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("the image has {0} bytes, more than an image report can number")]
pub(crate) struct ImageTooLarge(usize);

/// Returns the output reports that write the JPEG image `jpeg` to `region`.
/// The image must be rotated by 180°.
pub(crate) fn image(region: Region, jpeg: &[u8]) -> Result<Vec<OutputReport>, ImageTooLarge> {
    let (command, index) = region.command();
    let chunks: Vec<&[u8]> = if jpeg.is_empty() {
        vec![&[]]
    } else {
        jpeg.chunks(CHUNK_LEN).collect()
    };
    let last = chunks.len() - 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(position, chunk)| {
            let is_last = position == last;
            let number = u16::try_from(position).map_err(|_| ImageTooLarge(jpeg.len()))?;
            let len = u16::try_from(chunk.len()).map_err(|_| ImageTooLarge(jpeg.len()))?;
            let mut report = [0; OUTPUT_LEN];
            report[..4].copy_from_slice(&[0x02, command, index, u8::from(is_last)]);
            report[4..6].copy_from_slice(&len.to_le_bytes());
            report[6..8].copy_from_slice(&number.to_le_bytes());
            report[HEADER_LEN..HEADER_LEN + chunk.len()].copy_from_slice(chunk);
            Ok(report)
        })
        .collect()
}

/// Returns a feature report with the byte `0x03`, then `bytes`.
fn set(bytes: &[u8]) -> FeatureReport {
    let mut report = [0; FEATURE_LEN];
    report[0] = 0x03;
    report[1..=bytes.len()].copy_from_slice(bytes);
    report
}

/// Returns the feature report that resets the device. The device then shows
/// its own screen.
pub(crate) fn reset() -> FeatureReport {
    set(&[0x02])
}

/// Returns the feature report that sets the brightness to `percent`, at most
/// 100.
pub(crate) fn brightness(percent: u8) -> FeatureReport {
    set(&[0x08, percent.min(100)])
}

/// Returns the feature report that sets the sleep timer to `timeout`, in whole
/// seconds. A timeout under 1 s turns the sleep off.
pub(crate) fn sleep_timer(timeout: Duration) -> FeatureReport {
    let seconds = i32::try_from(timeout.as_secs()).unwrap_or(i32::MAX);
    let [a, b, c, d] = seconds.to_le_bytes();
    set(&[0x0d, a, b, c, d])
}

/// Returns the feature report that fills `input` with the color `rgb`.
pub(crate) fn fill(input: Input, [r, g, b]: [u8; 3]) -> FeatureReport {
    set(&[0x06, input.index(), r, g, b])
}

/// The report id of the main firmware version.
pub(crate) const FIRMWARE_VERSION: u8 = 0x05;

/// Returns the version in the feature report `report` of
/// [`FIRMWARE_VERSION`], or `None` if the report holds no ASCII version.
pub(crate) fn firmware_version(report: &FeatureReport) -> Option<&str> {
    // Byte 1 is the length of the bytes after it, a 4-byte checksum and then the version.
    let end = 2 + usize::from(report[1]);
    let version = report.get(6..end)?;
    let version = version.split(|&byte| byte == 0).next()?;
    str::from_utf8(version)
        .ok()
        .filter(|version| !version.is_empty() && version.is_ascii())
}

/// The inputs that an input report shows as pressed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pressed(u16);

/// An input report that does not have the layout of the device.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum InputError {
    #[error("the input report has {0} bytes")]
    Short(usize),
    #[error("the input report has the id {0:#04x} and the command {1:#04x}")]
    Unknown(u8, u8),
}

/// A change of one input between two input reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    Press(Input),
    Release(Input),
}

impl Pressed {
    /// Returns the inputs that the input report `report` shows as pressed.
    pub(crate) fn parse(report: &[u8]) -> Result<Self, InputError> {
        let header = report.get(..4).ok_or(InputError::Short(report.len()))?;
        if header[..2] != [0x01, 0x00] {
            return Err(InputError::Unknown(header[0], header[1]));
        }
        let states = report
            .get(4..4 + INPUTS)
            .ok_or(InputError::Short(report.len()))?;
        Ok(Self(
            Input::all()
                .filter(|input| states[usize::from(input.index())] != 0)
                .fold(0, |pressed, input| pressed | 1 << input.index()),
        ))
    }

    /// Returns whether `input` is pressed.
    pub(crate) fn contains(self, input: Input) -> bool {
        self.0 & 1 << input.index() != 0
    }

    /// Returns the changes from `self` to `next`, in the order of the inputs.
    pub(crate) fn changes(self, next: Self) -> impl Iterator<Item = Change> {
        Input::all().filter_map(
            move |input| match (self.contains(input), next.contains(input)) {
                (false, true) => Some(Change::Press(input)),
                (true, false) => Some(Change::Release(input)),
                (false, false) | (true, true) => None,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use proptest::prelude::*;

    use super::*;

    fn key(index: u8) -> Input {
        Input::Key(Key::new(index).unwrap())
    }

    /// Returns the inputs that `pressed` shows as pressed.
    fn pressed_inputs(pressed: Pressed) -> Vec<Input> {
        Input::all()
            .filter(|&input| pressed.contains(input))
            .collect()
    }

    /// Returns an input report from the device with the 12 state bytes
    /// `states`.
    fn input_report(states: [u8; 12]) -> Vec<u8> {
        let mut report = vec![0x01, 0x00, 0x0c, 0x00];
        report.extend(states);
        report.resize(INPUT_LEN, 0);
        report
    }

    /// Returns the inputs that the device shows as pressed with the 12 state
    /// bytes `states`.
    fn pressed(states: [u8; 12]) -> Pressed {
        Pressed::parse(&input_report(states)).unwrap()
    }

    /// Returns the header and the image length of each report, one line each.
    fn headers(reports: &[OutputReport]) -> String {
        let mut table = String::new();
        for report in reports {
            let header: Vec<String> = report[..HEADER_LEN]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            writeln!(table, "{}", header.join(" ")).unwrap();
        }
        table
    }

    #[test]
    fn key_image_of_2500_bytes_takes_3_reports() {
        let reports = image(Region::Input(key(3)), &[0xaa; 2500]).unwrap();

        insta::assert_snapshot!(headers(&reports));
    }

    #[test]
    fn info_bar_image_has_command_0x0b() {
        let reports = image(Region::InfoBar, &[0xaa; 10]).unwrap();

        assert_eq!(reports[0][..4], [0x02, 0x0b, 0x00, 0x01]);
    }

    #[test]
    fn right_touch_point_image_has_index_9() {
        let reports = image(Region::Input(Input::TouchPoint(Side::Right)), &[0xaa; 10]).unwrap();

        assert_eq!(reports[0][..4], [0x02, 0x07, 0x09, 0x01]);
    }

    #[test]
    fn image_of_one_full_chunk_takes_1_report() {
        let reports = image(Region::InfoBar, &[0xaa; CHUNK_LEN]).unwrap();

        assert_eq!(reports.len(), 1);
    }

    #[test]
    fn image_of_one_byte_more_than_a_chunk_takes_2_reports() {
        let reports = image(Region::InfoBar, &[0xaa; CHUNK_LEN + 1]).unwrap();

        assert_eq!(reports.len(), 2);
    }

    #[test]
    fn empty_image_takes_1_last_report_without_data() {
        let reports = image(Region::InfoBar, &[]).unwrap();

        assert_eq!(headers(&reports), "02 0b 00 01 00 00 00 00\n");
    }

    #[test]
    fn short_last_chunk_is_padded_with_zeros() {
        let reports = image(Region::InfoBar, &[0xaa; 10]).unwrap();

        assert!(reports[0][HEADER_LEN + 10..].iter().all(|&byte| byte == 0));
    }

    proptest! {
        #[test]
        fn image_reports_hold_the_image_in_order(jpeg in prop::collection::vec(any::<u8>(), 0..5000)) {
            let reports = image(Region::InfoBar, &jpeg).unwrap();

            let data: Vec<u8> = reports
                .iter()
                .flat_map(|report| {
                    let len = usize::from(u16::from_le_bytes([report[4], report[5]]));
                    report[HEADER_LEN..HEADER_LEN + len].to_vec()
                })
                .collect();
            prop_assert_eq!(data, jpeg);
        }
    }

    #[test]
    fn feature_reports_match_the_device() {
        let reports = [
            ("reset", reset()),
            ("brightness 60", brightness(60)),
            ("brightness 150", brightness(150)),
            ("sleep 5 s", sleep_timer(Duration::from_secs(5))),
            ("sleep off", sleep_timer(Duration::ZERO)),
            (
                "fill left red",
                fill(Input::TouchPoint(Side::Left), [0xff, 0, 0]),
            ),
        ];

        let mut table = String::new();
        for (name, report) in reports {
            let bytes: Vec<String> = report[..8]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            writeln!(table, "{name:15} | {}", bytes.join(" ")).unwrap();
        }
        insta::assert_snapshot!(table);
    }

    #[test]
    fn feature_report_is_padded_to_32_bytes() {
        assert_eq!(reset()[2..], [0; FEATURE_LEN - 2]);
    }

    #[test]
    fn firmware_report_of_the_device_gives_its_version() {
        let mut report = [0; FEATURE_LEN];
        report[..14].copy_from_slice(&[
            0x05, 0x0c, 0xd5, 0x27, 0x93, 0xe1, b'1', b'.', b'0', b'0', b'.', b'0', b'1', b'3',
        ]);

        assert_eq!(firmware_version(&report), Some("1.00.013"));
    }

    #[test]
    fn firmware_report_without_a_version_gives_none() {
        let mut report = [0; FEATURE_LEN];
        report[..2].copy_from_slice(&[0x05, 0x04]);

        assert_eq!(firmware_version(&report), None);
    }

    #[test]
    fn input_report_with_key_0_gives_key_0_pressed() {
        let pressed = Pressed::parse(&input_report([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])).unwrap();

        assert_eq!(pressed_inputs(pressed), [key(0)]);
    }

    #[test]
    fn input_report_with_two_keys_gives_both_pressed() {
        let pressed = Pressed::parse(&input_report([1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])).unwrap();

        assert_eq!(pressed_inputs(pressed), [key(0), key(1)]);
    }

    #[test]
    fn input_report_with_byte_9_gives_the_right_touch_point_pressed() {
        let pressed = Pressed::parse(&input_report([0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0])).unwrap();

        assert!(pressed.contains(Input::TouchPoint(Side::Right)));
    }

    #[test]
    fn input_report_ignores_bytes_10_and_11() {
        let pressed = Pressed::parse(&input_report([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1])).unwrap();

        assert_eq!(pressed, Pressed::default());
    }

    #[test]
    fn short_input_report_is_an_error() {
        let result = Pressed::parse(&[0x01, 0x00, 0x0c, 0x00, 0x01]);

        assert_eq!(result, Err(InputError::Short(5)));
    }

    #[test]
    fn input_report_with_another_id_is_an_error() {
        let result = Pressed::parse(&[0x02; 16]);

        assert_eq!(result, Err(InputError::Unknown(0x02, 0x02)));
    }

    #[test]
    fn brightness_above_100_is_100() {
        assert_eq!(brightness(150)[..3], [0x03, 0x08, 0x64]);
    }

    #[test]
    fn new_pressed_key_gives_a_press() {
        let changes: Vec<Change> = Pressed::default()
            .changes(pressed([0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]))
            .collect();

        assert_eq!(changes, [Change::Press(key(2))]);
    }

    #[test]
    fn released_key_gives_a_release() {
        let changes: Vec<Change> = pressed([0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0])
            .changes(Pressed::default())
            .collect();

        assert_eq!(changes, [Change::Release(key(2))]);
    }

    #[test]
    fn report_without_a_change_gives_no_change() {
        let changes: Vec<Change> = pressed([0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0])
            .changes(pressed([0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]))
            .collect();

        assert_eq!(changes, []);
    }

    #[test]
    fn release_of_one_of_two_keys_gives_only_its_release() {
        let changes: Vec<Change> = pressed([1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
            .changes(pressed([0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]))
            .collect();

        assert_eq!(changes, [Change::Release(key(0))]);
    }

    #[test]
    fn wake_report_after_no_press_gives_no_change() {
        let wake = Pressed::parse(&input_report([0; 12])).unwrap();

        let changes: Vec<Change> = Pressed::default().changes(wake).collect();

        assert_eq!(changes, []);
    }
}
