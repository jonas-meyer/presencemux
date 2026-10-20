//! The descriptors and strings of the camera function for FunctionFS.
//!
//! The camera must be the first function of the configuration, with the
//! interfaces 0 and 1 and the endpoint 0x81. `f_fs` renumbers the standard
//! descriptors, but not the copies of these numbers in the class-specific
//! descriptors.

use zerocopy::little_endian::{U16, U32};
use zerocopy::{Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::Format;

/// The interface number of VideoControl.
pub const CONTROL_INTERFACE: u8 = 0;
/// The interface number of VideoStreaming.
pub const STREAMING_INTERFACE: u8 = 1;
/// The address of the bulk IN endpoint of VideoStreaming.
pub const ENDPOINT_ADDRESS: u8 = 0x81;
/// The packet size of the bulk endpoint at high speed.
pub const MAX_PACKET: u16 = 512;
/// The packet size of the bulk endpoint at full speed.
const MAX_PACKET_FULL_SPEED: u16 = 64;
/// The name of the camera. The interface association descriptor and both
/// interfaces use it.
pub const NAME: &str = "Studio Camera";
/// The index of [`NAME`] in [`strings`].
const NAME_INDEX: u8 = 1;

const FUNCTIONFS_DESCRIPTORS_MAGIC_V2: u32 = 3;
const FUNCTIONFS_STRINGS_MAGIC: u32 = 2;
const FUNCTIONFS_HAS_FS_DESC: u32 = 1;
const FUNCTIONFS_HAS_HS_DESC: u32 = 2;
const LANGUAGE_EN_US: u16 = 0x0409;

const DT_INTERFACE: u8 = 0x04;
const DT_ENDPOINT: u8 = 0x05;
const DT_INTERFACE_ASSOCIATION: u8 = 0x0b;
const DT_CS_INTERFACE: u8 = 0x24;

const CLASS_VIDEO: u8 = 0x0e;
const SUBCLASS_CONTROL: u8 = 0x01;
const SUBCLASS_STREAMING: u8 = 0x02;
const SUBCLASS_INTERFACE_COLLECTION: u8 = 0x03;

const VC_HEADER: u8 = 0x01;
const VC_INPUT_TERMINAL: u8 = 0x02;
const VC_OUTPUT_TERMINAL: u8 = 0x03;
const VS_INPUT_HEADER: u8 = 0x01;
const VS_FORMAT_MJPEG: u8 = 0x06;
const VS_FRAME_MJPEG: u8 = 0x07;
const VS_COLORFORMAT: u8 = 0x0d;

const UVC_VERSION: u16 = 0x0110;
const CLOCK_FREQUENCY: u32 = 48_000_000;
const CAMERA_TERMINAL_ID: u8 = 1;
const OUTPUT_TERMINAL_ID: u8 = 2;
const ITT_CAMERA: u16 = 0x0201;
const TT_STREAMING: u16 = 0x0101;
const ENDPOINT_BULK: u8 = 0x02;
// The BT.709 primaries and transfer characteristics and the SMPTE 170M matrix
// of JPEG.
const COLOR_PRIMARIES: u8 = 1;
const TRANSFER_CHARACTERISTICS: u8 = 1;
const MATRIX_COEFFICIENTS: u8 = 4;

/// Returns the descriptors for `ep0` of FunctionFS, in format v2, for full
/// speed and high speed.
pub fn descriptors(format: Format) -> Vec<u8> {
    let blob = DescriptorsBlob {
        magic: U32::new(FUNCTIONFS_DESCRIPTORS_MAGIC_V2),
        length: U32::new(len_u32(size_of::<DescriptorsBlob>())),
        flags: U32::new(FUNCTIONFS_HAS_FS_DESC | FUNCTIONFS_HAS_HS_DESC),
        full_speed_count: U32::new(DESCRIPTOR_COUNT),
        high_speed_count: U32::new(DESCRIPTOR_COUNT),
        full_speed: CameraDescriptors::new(format, MAX_PACKET_FULL_SPEED),
        high_speed: CameraDescriptors::new(format, MAX_PACKET),
    };
    blob.as_bytes().to_vec()
}

/// Returns the strings for `ep0` of FunctionFS: [`NAME`] in US English.
pub fn strings() -> Vec<u8> {
    let header = StringsHeader {
        magic: U32::new(FUNCTIONFS_STRINGS_MAGIC),
        length: U32::new(len_u32(size_of::<StringsHeader>() + NAME.len() + 1)),
        string_count: U32::new(1),
        language_count: U32::new(1),
        language: U16::new(LANGUAGE_EN_US),
    };
    [header.as_bytes(), NAME.as_bytes(), &[0]].concat()
}

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).expect("the FunctionFS blobs are small")
}

/// Returns the `bLength` of descriptor `T`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the assert checks that the size fits in u8"
)]
const fn length<T>() -> u8 {
    assert!(size_of::<T>() <= u8::MAX as usize);
    size_of::<T>() as u8
}

/// Returns `wTotalLength` for class-specific descriptors of `len` bytes.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the assert checks that the size fits in u16"
)]
const fn total_length(len: usize) -> U16 {
    assert!(len <= u16::MAX as usize);
    U16::new(len as u16)
}

/// The descriptors blob of FunctionFS v2 with full speed and high speed.
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct DescriptorsBlob {
    magic: U32,
    length: U32,
    flags: U32,
    full_speed_count: U32,
    high_speed_count: U32,
    full_speed: CameraDescriptors,
    high_speed: CameraDescriptors,
}

/// The header of the strings blob of FunctionFS, with one language.
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct StringsHeader {
    magic: U32,
    length: U32,
    string_count: U32,
    language_count: U32,
    language: U16,
}

/// The number of descriptors in [`CameraDescriptors`].
const DESCRIPTOR_COUNT: u32 = 11;

/// The descriptors of the camera at one speed, in the order of the
/// configuration descriptor.
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct CameraDescriptors {
    association: InterfaceAssociation,
    control: Interface,
    control_header: ControlHeader,
    camera: CameraTerminal,
    output: OutputTerminal,
    streaming: Interface,
    streaming_header: StreamingHeader,
    mjpeg: MjpegFormat,
    frame: MjpegFrame,
    color: ColorMatching,
    endpoint: Endpoint,
}

impl CameraDescriptors {
    fn new(format: Format, max_packet: u16) -> Self {
        let pixels = u32::from(format.width) * u32::from(format.height);
        Self {
            association: InterfaceAssociation {
                length: length::<InterfaceAssociation>(),
                descriptor_type: DT_INTERFACE_ASSOCIATION,
                first_interface: CONTROL_INTERFACE,
                interface_count: 2,
                function_class: CLASS_VIDEO,
                function_subclass: SUBCLASS_INTERFACE_COLLECTION,
                function_protocol: 0,
                name: NAME_INDEX,
            },
            control: Interface::new(CONTROL_INTERFACE, 0, SUBCLASS_CONTROL),
            control_header: ControlHeader {
                length: length::<ControlHeader>(),
                descriptor_type: DT_CS_INTERFACE,
                descriptor_subtype: VC_HEADER,
                uvc: U16::new(UVC_VERSION),
                total_length: total_length(
                    size_of::<ControlHeader>()
                        + size_of::<CameraTerminal>()
                        + size_of::<OutputTerminal>(),
                ),
                clock_frequency: U32::new(CLOCK_FREQUENCY),
                in_collection: 1,
                interface_number: STREAMING_INTERFACE,
            },
            camera: CameraTerminal {
                length: length::<CameraTerminal>(),
                descriptor_type: DT_CS_INTERFACE,
                descriptor_subtype: VC_INPUT_TERMINAL,
                terminal_id: CAMERA_TERMINAL_ID,
                terminal_type: U16::new(ITT_CAMERA),
                assoc_terminal: 0,
                name: 0,
                objective_focal_length_min: U16::ZERO,
                objective_focal_length_max: U16::ZERO,
                ocular_focal_length: U16::ZERO,
                control_size: 3,
                controls: [0; 3],
            },
            output: OutputTerminal {
                length: length::<OutputTerminal>(),
                descriptor_type: DT_CS_INTERFACE,
                descriptor_subtype: VC_OUTPUT_TERMINAL,
                terminal_id: OUTPUT_TERMINAL_ID,
                terminal_type: U16::new(TT_STREAMING),
                assoc_terminal: 0,
                source_id: CAMERA_TERMINAL_ID,
                name: 0,
            },
            streaming: Interface::new(STREAMING_INTERFACE, 1, SUBCLASS_STREAMING),
            streaming_header: StreamingHeader {
                length: length::<StreamingHeader>(),
                descriptor_type: DT_CS_INTERFACE,
                descriptor_subtype: VS_INPUT_HEADER,
                num_formats: 1,
                total_length: total_length(
                    size_of::<StreamingHeader>()
                        + size_of::<MjpegFormat>()
                        + size_of::<MjpegFrame>()
                        + size_of::<ColorMatching>(),
                ),
                endpoint_address: ENDPOINT_ADDRESS,
                info: 0,
                terminal_link: OUTPUT_TERMINAL_ID,
                still_capture_method: 0,
                trigger_support: 0,
                trigger_usage: 0,
                control_size: 1,
                controls: 0,
            },
            mjpeg: MjpegFormat {
                length: length::<MjpegFormat>(),
                descriptor_type: DT_CS_INTERFACE,
                descriptor_subtype: VS_FORMAT_MJPEG,
                format_index: 1,
                num_frame_descriptors: 1,
                flags: 0,
                default_frame_index: 1,
                aspect_ratio_x: 0,
                aspect_ratio_y: 0,
                interlace_flags: 0,
                copy_protect: 0,
            },
            frame: MjpegFrame {
                length: length::<MjpegFrame>(),
                descriptor_type: DT_CS_INTERFACE,
                descriptor_subtype: VS_FRAME_MJPEG,
                frame_index: 1,
                capabilities: 0,
                width: U16::new(format.width),
                height: U16::new(format.height),
                min_bit_rate: U32::new(pixels * 80),
                max_bit_rate: U32::new(pixels * 160),
                max_video_frame_buffer_size: U32::new(format.max_frame_size()),
                default_frame_interval: U32::new(format.frame_interval),
                frame_interval_type: 1,
                frame_interval: U32::new(format.frame_interval),
            },
            color: ColorMatching {
                length: length::<ColorMatching>(),
                descriptor_type: DT_CS_INTERFACE,
                descriptor_subtype: VS_COLORFORMAT,
                color_primaries: COLOR_PRIMARIES,
                transfer_characteristics: TRANSFER_CHARACTERISTICS,
                matrix_coefficients: MATRIX_COEFFICIENTS,
            },
            endpoint: Endpoint {
                length: length::<Endpoint>(),
                descriptor_type: DT_ENDPOINT,
                address: ENDPOINT_ADDRESS,
                attributes: ENDPOINT_BULK,
                max_packet_size: U16::new(max_packet),
                interval: 0,
            },
        }
    }
}

/// The interface association descriptor of the video function.
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct InterfaceAssociation {
    length: u8,
    descriptor_type: u8,
    first_interface: u8,
    interface_count: u8,
    function_class: u8,
    function_subclass: u8,
    function_protocol: u8,
    /// `iFunction`: the string index of the name.
    name: u8,
}

/// A standard interface descriptor.
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct Interface {
    length: u8,
    descriptor_type: u8,
    /// `bInterfaceNumber`.
    number: u8,
    alternate_setting: u8,
    num_endpoints: u8,
    /// `bInterfaceClass`.
    class: u8,
    /// `bInterfaceSubClass`.
    subclass: u8,
    /// `bInterfaceProtocol`.
    protocol: u8,
    /// `iInterface`: the string index of the name.
    name: u8,
}

impl Interface {
    /// Creates an interface of the video class with alternate setting 0.
    fn new(number: u8, endpoints: u8, subclass: u8) -> Self {
        Self {
            length: length::<Self>(),
            descriptor_type: DT_INTERFACE,
            number,
            alternate_setting: 0,
            num_endpoints: endpoints,
            class: CLASS_VIDEO,
            subclass,
            protocol: 0,
            name: NAME_INDEX,
        }
    }
}

/// The class-specific VideoControl interface header with one streaming
/// interface.
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct ControlHeader {
    length: u8,
    descriptor_type: u8,
    descriptor_subtype: u8,
    /// `bcdUVC`.
    uvc: U16,
    total_length: U16,
    clock_frequency: U32,
    in_collection: u8,
    /// `baInterfaceNr(1)`.
    interface_number: u8,
}

/// The camera terminal descriptor, with no controls.
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct CameraTerminal {
    length: u8,
    descriptor_type: u8,
    descriptor_subtype: u8,
    terminal_id: u8,
    terminal_type: U16,
    assoc_terminal: u8,
    /// `iTerminal`: the string index of the name, 0 for none.
    name: u8,
    objective_focal_length_min: U16,
    objective_focal_length_max: U16,
    ocular_focal_length: U16,
    control_size: u8,
    controls: [u8; 3],
}

/// The output terminal descriptor. It connects the camera terminal to the
/// streaming interface.
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct OutputTerminal {
    length: u8,
    descriptor_type: u8,
    descriptor_subtype: u8,
    terminal_id: u8,
    terminal_type: U16,
    assoc_terminal: u8,
    source_id: u8,
    /// `iTerminal`: the string index of the name, 0 for none.
    name: u8,
}

/// The class-specific VideoStreaming input header with one format.
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct StreamingHeader {
    length: u8,
    descriptor_type: u8,
    descriptor_subtype: u8,
    num_formats: u8,
    total_length: U16,
    endpoint_address: u8,
    info: u8,
    terminal_link: u8,
    still_capture_method: u8,
    trigger_support: u8,
    trigger_usage: u8,
    control_size: u8,
    /// `bmaControls(1)`.
    controls: u8,
}

/// The MJPEG format descriptor.
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct MjpegFormat {
    length: u8,
    descriptor_type: u8,
    descriptor_subtype: u8,
    format_index: u8,
    num_frame_descriptors: u8,
    flags: u8,
    default_frame_index: u8,
    aspect_ratio_x: u8,
    aspect_ratio_y: u8,
    interlace_flags: u8,
    copy_protect: u8,
}

/// The MJPEG frame descriptor with one discrete frame interval.
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct MjpegFrame {
    length: u8,
    descriptor_type: u8,
    descriptor_subtype: u8,
    frame_index: u8,
    capabilities: u8,
    width: U16,
    height: U16,
    min_bit_rate: U32,
    max_bit_rate: U32,
    max_video_frame_buffer_size: U32,
    default_frame_interval: U32,
    frame_interval_type: u8,
    frame_interval: U32,
}

/// The color matching descriptor.
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct ColorMatching {
    length: u8,
    descriptor_type: u8,
    descriptor_subtype: u8,
    color_primaries: u8,
    transfer_characteristics: u8,
    matrix_coefficients: u8,
}

/// The bulk IN endpoint descriptor.
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct Endpoint {
    length: u8,
    descriptor_type: u8,
    /// `bEndpointAddress`.
    address: u8,
    attributes: u8,
    max_packet_size: U16,
    interval: u8,
}

const _: () = {
    assert!(size_of::<InterfaceAssociation>() == 8);
    assert!(size_of::<Interface>() == 9);
    assert!(size_of::<ControlHeader>() == 13);
    assert!(size_of::<CameraTerminal>() == 18);
    assert!(size_of::<OutputTerminal>() == 9);
    assert!(size_of::<StreamingHeader>() == 14);
    assert!(size_of::<MjpegFormat>() == 11);
    assert!(size_of::<MjpegFrame>() == 30);
    assert!(size_of::<ColorMatching>() == 6);
    assert!(size_of::<Endpoint>() == 7);
};

#[cfg(test)]
mod tests {
    use core::fmt::Write;

    use super::*;

    const FORMAT: Format = Format {
        width: 1920,
        height: 1080,
        frame_interval: 333_333,
    };

    /// The number of fields in the FunctionFS header before the descriptors.
    const HEADER_FIELDS: usize = 5;

    /// Returns field `index` of the FunctionFS header.
    fn field(blob: &[u8], index: usize) -> usize {
        let bytes = blob[index * 4..index * 4 + 4].try_into().unwrap();
        usize::try_from(u32::from_le_bytes(bytes)).unwrap()
    }

    /// Returns the descriptors of each speed, split at their `bLength` and
    /// counted by the FunctionFS header.
    fn speeds(blob: &[u8]) -> (Vec<&[u8]>, Vec<&[u8]>) {
        let mut rest = &blob[HEADER_FIELDS * 4..];
        let mut take = |count: usize| -> Vec<&[u8]> {
            (0..count)
                .map(|_| {
                    let (descriptor, tail) = rest.split_at(usize::from(rest[0]));
                    rest = tail;
                    descriptor
                })
                .collect()
        };
        let full = take(field(blob, 3));
        let high = take(field(blob, 4));
        (full, high)
    }

    /// Returns the class-specific descriptor with `subtype` after the
    /// interface with `subclass`.
    fn class_descriptor<'a>(descriptors: &[&'a [u8]], subclass: u8, subtype: u8) -> &'a [u8] {
        let interface = descriptors
            .iter()
            .position(|d| d[1] == DT_INTERFACE && d[6] == subclass)
            .unwrap();
        descriptors[interface + 1..]
            .iter()
            .find(|d| d[1] == DT_CS_INTERFACE && d[2] == subtype)
            .unwrap()
    }

    /// Returns the length of the class-specific descriptors after the
    /// interface with `subclass`.
    fn class_length(descriptors: &[&[u8]], subclass: u8) -> usize {
        let interface = descriptors
            .iter()
            .position(|d| d[1] == DT_INTERFACE && d[6] == subclass)
            .unwrap();
        descriptors[interface + 1..]
            .iter()
            .take_while(|d| d[1] == DT_CS_INTERFACE)
            .map(|d| d.len())
            .sum()
    }

    /// Returns the name of a descriptor from its type and subtype.
    fn name(descriptor: &[u8], streaming: bool) -> &'static str {
        match (descriptor[1], descriptor.get(2), streaming) {
            (DT_INTERFACE_ASSOCIATION, ..) => "interface association",
            (DT_INTERFACE, ..) => "interface",
            (DT_ENDPOINT, ..) => "endpoint",
            (DT_CS_INTERFACE, Some(&VC_HEADER), false) => "VC header",
            (DT_CS_INTERFACE, Some(&VC_INPUT_TERMINAL), false) => "VC camera terminal",
            (DT_CS_INTERFACE, Some(&VC_OUTPUT_TERMINAL), false) => "VC output terminal",
            (DT_CS_INTERFACE, Some(&VS_INPUT_HEADER), true) => "VS input header",
            (DT_CS_INTERFACE, Some(&VS_FORMAT_MJPEG), true) => "VS MJPEG format",
            (DT_CS_INTERFACE, Some(&VS_FRAME_MJPEG), true) => "VS MJPEG frame",
            (DT_CS_INTERFACE, Some(&VS_COLORFORMAT), true) => "VS color matching",
            _ => "unknown",
        }
    }

    #[test]
    fn functionfs_header_has_the_magic_the_length_and_both_speeds() {
        let blob = descriptors(FORMAT);

        assert_eq!(field(&blob, 0), 3);
        assert_eq!(field(&blob, 1), blob.len());
        assert_eq!(field(&blob, 2), 3);
    }

    #[test]
    fn counted_descriptors_fill_the_blob() {
        let blob = descriptors(FORMAT);

        let (full, high) = speeds(&blob);

        let len: usize = full.iter().chain(&high).map(|d| d.len()).sum();
        assert_eq!(HEADER_FIELDS * 4 + len, blob.len());
    }

    #[test]
    fn speeds_differ_only_in_the_packet_size_of_the_endpoint() {
        let blob = descriptors(FORMAT);
        let (full, high) = speeds(&blob);

        let differing: Vec<(&[u8], &[u8])> = full
            .iter()
            .zip(&high)
            .filter(|(f, h)| f != h)
            .map(|(f, h)| (*f, *h))
            .collect();

        assert_eq!(
            differing,
            [(
                &[7, 5, 0x81, 2, 64, 0, 0][..],
                &[7, 5, 0x81, 2, 0, 2, 0][..]
            )]
        );
    }

    #[test]
    fn control_header_total_length_covers_the_control_descriptors() {
        let blob = descriptors(FORMAT);
        let (_, high) = speeds(&blob);

        let header = class_descriptor(&high, SUBCLASS_CONTROL, VC_HEADER);

        let total = u16::from_le_bytes([header[5], header[6]]);
        assert_eq!(usize::from(total), class_length(&high, SUBCLASS_CONTROL));
    }

    #[test]
    fn streaming_header_total_length_covers_the_streaming_descriptors() {
        let blob = descriptors(FORMAT);
        let (_, high) = speeds(&blob);

        let header = class_descriptor(&high, SUBCLASS_STREAMING, VS_INPUT_HEADER);

        let total = u16::from_le_bytes([header[4], header[5]]);
        assert_eq!(usize::from(total), class_length(&high, SUBCLASS_STREAMING));
    }

    #[test]
    fn strings_hold_the_name_in_us_english() {
        let blob = strings();

        #[rustfmt::skip]
        let expected: Vec<u8> = [
            &[2, 0, 0, 0][..],      // magic
            &[32, 0, 0, 0],         // length
            &[1, 0, 0, 0],          // str_count
            &[1, 0, 0, 0],          // lang_count
            &[0x09, 0x04],          // en-US
            b"Studio Camera\0",
        ]
        .concat();
        assert_eq!(blob, expected);
    }

    #[test]
    fn high_speed_descriptors_are_the_reviewed_bytes() {
        let blob = descriptors(FORMAT);
        let (_, high) = speeds(&blob);

        let mut dump = String::new();
        let mut streaming = false;
        for descriptor in high {
            if descriptor[1] == DT_INTERFACE {
                streaming = descriptor[6] == SUBCLASS_STREAMING;
            }
            let hex: Vec<String> = descriptor.iter().map(|b| format!("{b:02x}")).collect();
            writeln!(dump, "{}: {}", name(descriptor, streaming), hex.join(" ")).unwrap();
        }

        insta::assert_snapshot!(dump);
    }
}
