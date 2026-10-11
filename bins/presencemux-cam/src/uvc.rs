//! The UVC protocol of the gadget function, without I/O.
//!
//! The kernel sends each setup request of the host as a V4L2 event and waits
//! for one answer. `docs/cam.md` describes the protocol.

use zerocopy::little_endian::{U16, U32};
use zerocopy::{Immutable, IntoBytes, KnownLayout, Unaligned};

/// The first private V4L2 event type, `V4L2_EVENT_PRIVATE_START`.
const EVENT_BASE: u32 = 0x0800_0000;
pub(crate) const EVENT_CONNECT: u32 = EVENT_BASE;
pub(crate) const EVENT_DISCONNECT: u32 = EVENT_BASE + 1;
pub(crate) const EVENT_STREAMON: u32 = EVENT_BASE + 2;
pub(crate) const EVENT_STREAMOFF: u32 = EVENT_BASE + 3;
pub(crate) const EVENT_SETUP: u32 = EVENT_BASE + 4;
pub(crate) const EVENT_DATA: u32 = EVENT_BASE + 5;

/// The frame interval of the only format of the gadget, in units of 100 ns.
pub(crate) const FRAME_INTERVAL: u32 = 333_333;

/// The largest data stage that the kernel passes to the worker.
const DATA_LEN: u8 = 60;

/// The answer length for a protocol stall. The kernel stalls on any negative
/// length.
const STALL: i32 = -1;

const SET_CUR: u8 = 0x01;
const GET_CUR: u8 = 0x81;
const GET_MIN: u8 = 0x82;
const GET_MAX: u8 = 0x83;
const GET_RES: u8 = 0x84;
const GET_LEN: u8 = 0x85;
const GET_INFO: u8 = 0x86;
const GET_DEF: u8 = 0x87;

/// The `GET_INFO` bit for a control that supports `GET_CUR`.
const INFO_GET: u8 = 0x01;
/// The `GET_INFO` bit for a control that supports `SET_CUR`.
const INFO_SET: u8 = 0x02;

const INTERFACE_CONTROL: u8 = 0;
const INTERFACE_STREAMING: u8 = 1;

const VS_PROBE: u8 = 0x01;
const VS_COMMIT: u8 = 0x02;

const ENTITY_INTERFACE: u8 = 0;
const ENTITY_CAMERA_TERMINAL: u8 = 1;
const ENTITY_PROCESSING_UNIT: u8 = 2;

const VC_REQUEST_ERROR_CODE: u8 = 0x02;
const CT_AE_MODE: u8 = 0x02;
const PU_BRIGHTNESS: u8 = 0x02;

/// The auto exposure mode "auto", the only mode of the gadget.
const AE_MODE_AUTO: u8 = 0x02;
const BRIGHTNESS_MAX: i16 = 255;
const BRIGHTNESS_DEFAULT: i16 = 128;

/// The `bmHint` bit for a fixed frame interval.
const HINT_FRAME_INTERVAL: u16 = 0x0001;
/// The `bmFramingInfo` bits FID and EOF, which the kernel sets in each payload
/// header.
const FRAMING_FID_EOF: u8 = 0x03;
/// The clock of the PTS and SCR fields in the payload headers, 48 MHz.
const CLOCK_FREQUENCY: u32 = 48_000_000;

/// The length of `struct uvc_streaming_control` in UVC 1.1.
const STREAMING_CONTROL_LEN: u16 = 34;

/// A UVC event from the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Input {
    Connect,
    Disconnect,
    StreamOn,
    StreamOff,
    Setup(Setup),
    Data(RequestData),
}

impl Input {
    /// Reads an event from its type and the payload of `struct v4l2_event`.
    /// Returns `None` for an event type that is not a UVC event.
    pub(crate) fn parse(kind: u32, payload: &[u8; 64]) -> Option<Self> {
        match kind {
            EVENT_CONNECT => Some(Self::Connect),
            EVENT_DISCONNECT => Some(Self::Disconnect),
            EVENT_STREAMON => Some(Self::StreamOn),
            EVENT_STREAMOFF => Some(Self::StreamOff),
            EVENT_SETUP => {
                let [request_type, request, v0, v1, i0, i1, l0, l1, ..] = *payload;
                Some(Self::Setup(Setup {
                    request_type,
                    request,
                    value: u16::from_le_bytes([v0, v1]),
                    index: u16::from_le_bytes([i0, i1]),
                    length: u16::from_le_bytes([l0, l1]),
                }))
            }
            EVENT_DATA => {
                let [l0, l1, l2, l3, data @ ..] = *payload;
                let len = i32::from_le_bytes([l0, l1, l2, l3]);
                Some(Self::Data(RequestData {
                    len: u8::try_from(len.clamp(0, i32::from(DATA_LEN))).unwrap_or_default(),
                    data,
                }))
            }
            _ => None,
        }
    }
}

/// A setup request of the host (`struct usb_ctrlrequest`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Setup {
    pub(crate) request_type: u8,
    pub(crate) request: u8,
    pub(crate) value: u16,
    pub(crate) index: u16,
    pub(crate) length: u16,
}

impl Setup {
    /// Returns the control selector.
    fn selector(self) -> u8 {
        self.value.to_le_bytes()[1]
    }

    /// Returns the interface: 0 for VideoControl, 1 for VideoStreaming. The
    /// kernel writes these values in place of the interface numbers.
    fn interface(self) -> u8 {
        self.index.to_le_bytes()[0]
    }

    /// Returns the unit or terminal of a VideoControl request.
    fn entity(self) -> u8 {
        self.index.to_le_bytes()[1]
    }
}

/// The data stage of a setup request (`struct uvc_request_data`). It holds the
/// data from the host or the data of an answer.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RequestData {
    len: u8,
    data: [u8; DATA_LEN as usize],
}

impl RequestData {
    fn new(bytes: &[u8]) -> Self {
        let mut data = [0; DATA_LEN as usize];
        data[..bytes.len()].copy_from_slice(bytes);
        Self {
            len: u8::try_from(bytes.len()).expect("the data has at most 60 bytes"),
            data,
        }
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.data[..usize::from(self.len)]
    }
}

impl PartialEq for RequestData {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for RequestData {}

/// The answer to one setup request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Response {
    /// An answer with data, for a `GET_*` request.
    Data(RequestData),
    /// An answer that accepts the data stage of a `SET_CUR` request with this
    /// length.
    Accept(u16),
    /// A protocol stall.
    Stall,
}

impl Response {
    fn data(bytes: &[u8]) -> Self {
        Self::Data(RequestData::new(bytes))
    }

    /// Returns the length and the data of `struct uvc_request_data`.
    pub(crate) fn wire(self) -> (i32, [u8; DATA_LEN as usize]) {
        match self {
            Self::Data(data) => (i32::from(data.len), data.data),
            Self::Accept(len) => (i32::from(len), [0; DATA_LEN as usize]),
            Self::Stall => (STALL, [0; DATA_LEN as usize]),
        }
    }
}

/// An action of the worker after an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Sends the answer with `UVCIOC_SEND_RESPONSE`.
    Respond(Response),
    /// Allocates the buffers, calls `VIDIOC_STREAMON` and queues the first
    /// frame.
    StartStream,
    /// Stops the frame output, calls `VIDIOC_STREAMOFF` and frees the buffers.
    StopStream,
}

/// The values that the probe and commit answers need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    /// The `sizeimage` of the current format.
    pub(crate) max_frame_size: u32,
    /// `streaming_maxpacket` of the gadget.
    pub(crate) max_payload: u32,
}

/// The probe and commit answer (`struct uvc_streaming_control`, UVC 1.1).
#[derive(IntoBytes, Immutable, KnownLayout, Unaligned)]
#[repr(C)]
struct StreamingControl {
    hint: U16,
    format_index: u8,
    frame_index: u8,
    frame_interval: U32,
    key_frame_rate: U16,
    p_frame_rate: U16,
    comp_quality: U16,
    comp_window_size: U16,
    delay: U16,
    max_video_frame_size: U32,
    max_payload_transfer_size: U32,
    clock_frequency: U32,
    framing_info: u8,
    preferred_version: u8,
    min_version: u8,
    max_version: u8,
}

const _: () = assert!(size_of::<StreamingControl>() == STREAMING_CONTROL_LEN as usize);

impl StreamingControl {
    /// Creates the answer for the only format of the gadget.
    fn new(limits: Limits) -> Self {
        Self {
            hint: U16::new(HINT_FRAME_INTERVAL),
            format_index: 1,
            frame_index: 1,
            frame_interval: U32::new(FRAME_INTERVAL),
            key_frame_rate: U16::ZERO,
            p_frame_rate: U16::ZERO,
            comp_quality: U16::ZERO,
            comp_window_size: U16::ZERO,
            delay: U16::ZERO,
            max_video_frame_size: U32::new(limits.max_frame_size),
            max_payload_transfer_size: U32::new(limits.max_payload),
            clock_frequency: U32::new(CLOCK_FREQUENCY),
            framing_info: FRAMING_FID_EOF,
            preferred_version: 1,
            min_version: 1,
            max_version: 1,
        }
    }
}

/// The request error code of the VideoControl interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum ErrorCode {
    NoError = 0x00,
    InvalidUnit = 0x05,
    InvalidControl = 0x06,
    InvalidRequest = 0x07,
}

/// A `SET_CUR` request that waits for its data stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Brightness,
}

/// The state of the UVC protocol.
#[derive(Clone, Debug)]
pub(crate) struct Uvc {
    limits: Limits,
    streaming: bool,
    error: ErrorCode,
    brightness: i16,
    pending: Option<Pending>,
}

impl Uvc {
    pub(crate) fn new(limits: Limits) -> Self {
        Self {
            limits,
            streaming: false,
            error: ErrorCode::NoError,
            brightness: BRIGHTNESS_DEFAULT,
            pending: None,
        }
    }

    /// Creates the protocol state for a gadget node with a running stream,
    /// after a restart.
    pub(crate) fn with_stream(limits: Limits) -> Self {
        Self {
            streaming: true,
            ..Self::new(limits)
        }
    }

    /// Updates the state for an event and returns the action for it.
    #[must_use]
    pub(crate) fn handle(&mut self, input: Input) -> Option<Action> {
        match input {
            Input::StreamOn if !self.streaming => {
                self.streaming = true;
                Some(Action::StartStream)
            }
            Input::StreamOff | Input::Disconnect if self.streaming => {
                self.streaming = false;
                Some(Action::StopStream)
            }
            Input::Setup(setup) => Some(Action::Respond(self.setup(setup))),
            Input::Data(data) => {
                self.data(data);
                None
            }
            Input::Connect | Input::StreamOn | Input::StreamOff | Input::Disconnect => None,
        }
    }

    fn setup(&mut self, setup: Setup) -> Response {
        self.pending = None;
        match setup.interface() {
            INTERFACE_STREAMING => self.streaming_request(setup),
            INTERFACE_CONTROL => self.control_request(setup),
            _ => Response::Stall,
        }
    }

    fn streaming_request(&self, setup: Setup) -> Response {
        match (setup.selector(), setup.request) {
            (VS_PROBE | VS_COMMIT, SET_CUR) => Response::Accept(setup.length),
            (VS_PROBE | VS_COMMIT, GET_CUR) | (VS_PROBE, GET_MIN | GET_MAX | GET_DEF) => {
                Response::data(StreamingControl::new(self.limits).as_bytes())
            }
            (VS_PROBE, GET_RES) => Response::data(&[0; STREAMING_CONTROL_LEN as usize]),
            (VS_PROBE | VS_COMMIT, GET_INFO) => Response::data(&[INFO_GET | INFO_SET]),
            (VS_PROBE | VS_COMMIT, GET_LEN) => Response::data(&STREAMING_CONTROL_LEN.to_le_bytes()),
            _ => Response::Stall,
        }
    }

    fn control_request(&mut self, setup: Setup) -> Response {
        let result = match (setup.entity(), setup.selector(), setup.request) {
            (ENTITY_INTERFACE, VC_REQUEST_ERROR_CODE, GET_CUR) => {
                // A read of the error code does not change it.
                return Response::data(&[self.error as u8]);
            }
            (ENTITY_INTERFACE, VC_REQUEST_ERROR_CODE, GET_INFO) => Ok(Response::data(&[INFO_GET])),
            (ENTITY_PROCESSING_UNIT, PU_BRIGHTNESS, request) => {
                self.brightness_request(request, setup.length)
            }
            (ENTITY_CAMERA_TERMINAL, CT_AE_MODE, SET_CUR) => Ok(Response::Accept(setup.length)),
            (ENTITY_CAMERA_TERMINAL, CT_AE_MODE, GET_CUR | GET_DEF | GET_RES) => {
                Ok(Response::data(&[AE_MODE_AUTO]))
            }
            (ENTITY_CAMERA_TERMINAL, CT_AE_MODE, GET_INFO) => {
                Ok(Response::data(&[INFO_GET | INFO_SET]))
            }
            (ENTITY_INTERFACE, VC_REQUEST_ERROR_CODE, _)
            | (ENTITY_CAMERA_TERMINAL, CT_AE_MODE, _) => Err(ErrorCode::InvalidRequest),
            (ENTITY_INTERFACE | ENTITY_CAMERA_TERMINAL | ENTITY_PROCESSING_UNIT, _, _) => {
                Err(ErrorCode::InvalidControl)
            }
            _ => Err(ErrorCode::InvalidUnit),
        };
        self.error = result.err().unwrap_or(ErrorCode::NoError);
        result.unwrap_or(Response::Stall)
    }

    fn brightness_request(&mut self, request: u8, length: u16) -> Result<Response, ErrorCode> {
        let value = match request {
            SET_CUR => {
                self.pending = Some(Pending::Brightness);
                return Ok(Response::Accept(length));
            }
            GET_INFO => return Ok(Response::data(&[INFO_GET | INFO_SET])),
            GET_CUR => self.brightness,
            GET_MIN => 0,
            GET_MAX => BRIGHTNESS_MAX,
            GET_RES => 1,
            GET_DEF => BRIGHTNESS_DEFAULT,
            _ => return Err(ErrorCode::InvalidRequest),
        };
        Ok(Response::data(&value.to_le_bytes()))
    }

    fn data(&mut self, data: RequestData) {
        if let (Some(Pending::Brightness), &[low, high, ..]) =
            (self.pending.take(), data.as_slice())
        {
            self.brightness = i16::from_le_bytes([low, high]).clamp(0, BRIGHTNESS_MAX);
        }
    }
}

#[cfg(test)]
mod tests {
    use core::fmt::Write;

    use proptest::prelude::*;

    use super::*;

    const LIMITS: Limits = Limits {
        max_frame_size: 4_147_200,
        max_payload: 2048,
    };

    const CONTROL: u16 = 0x0000;
    const STREAMING: u16 = 0x0001;
    const CAMERA_TERMINAL: u16 = 0x0100;
    const PROCESSING_UNIT: u16 = 0x0200;
    const UNKNOWN_UNIT: u16 = 0x0900;
    const UNKNOWN_INTERFACE: u16 = 0x0002;
    const UNKNOWN_SELECTOR: u8 = 0x7f;

    fn setup(request: u8, selector: u8, index: u16, length: u16) -> Input {
        Input::Setup(Setup {
            request_type: 0xa1,
            request,
            value: u16::from(selector) << 8,
            index,
            length,
        })
    }

    fn data(bytes: &[u8]) -> Input {
        Input::Data(RequestData::new(bytes))
    }

    fn answer(bytes: &[u8]) -> Action {
        Action::Respond(Response::data(bytes))
    }

    fn data_payload(len: i32, bytes: &[u8]) -> [u8; 64] {
        let mut payload = [0; 64];
        payload[..4].copy_from_slice(&len.to_le_bytes());
        payload[4..4 + bytes.len()].copy_from_slice(bytes);
        payload
    }

    fn input() -> impl Strategy<Value = Input> {
        prop_oneof![
            Just(Input::Connect),
            Just(Input::Disconnect),
            Just(Input::StreamOn),
            Just(Input::StreamOff),
            any::<(u8, u8, u16, u16, u16)>().prop_map(|(t, r, v, i, l)| {
                Input::Setup(Setup {
                    request_type: t,
                    request: r,
                    value: v,
                    index: i,
                    length: l,
                })
            }),
            proptest::collection::vec(any::<u8>(), 0..=usize::from(DATA_LEN))
                .prop_map(|b| data(&b)),
        ]
    }

    proptest! {
        #[test]
        fn stream_starts_and_stops_alternate(
            adopted in any::<bool>(),
            inputs in proptest::collection::vec(input(), 0..64),
        ) {
            let mut uvc = if adopted { Uvc::with_stream(LIMITS) } else { Uvc::new(LIMITS) };
            let mut running = adopted;

            for input in inputs {
                match uvc.handle(input) {
                    Some(Action::StartStream) => {
                        prop_assert!(!running);
                        running = true;
                    }
                    Some(Action::StopStream) => {
                        prop_assert!(running);
                        running = false;
                    }
                    Some(Action::Respond(_)) | None => {}
                }
            }
        }
    }

    #[test]
    fn parse_reads_each_event_type_of_the_kernel() {
        let payload = [0; 64];

        assert_eq!(Input::parse(0x0800_0000, &payload), Some(Input::Connect));
        assert_eq!(Input::parse(0x0800_0001, &payload), Some(Input::Disconnect));
        assert_eq!(Input::parse(0x0800_0002, &payload), Some(Input::StreamOn));
        assert_eq!(Input::parse(0x0800_0003, &payload), Some(Input::StreamOff));
        assert!(matches!(
            Input::parse(0x0800_0004, &payload),
            Some(Input::Setup(_))
        ));
        assert!(matches!(
            Input::parse(0x0800_0005, &payload),
            Some(Input::Data(_))
        ));
        assert_eq!(Input::parse(0x0800_0006, &payload), None);
    }

    #[test]
    fn setup_event_reads_little_endian_fields() {
        let mut payload = [0; 64];
        payload[..8].copy_from_slice(&[0xa1, 0x81, 0x00, 0x01, 0x01, 0x00, 0x22, 0x00]);

        let input = Input::parse(EVENT_SETUP, &payload);

        assert_eq!(
            input,
            Some(Input::Setup(Setup {
                request_type: 0xa1,
                request: GET_CUR,
                value: 0x0100,
                index: 0x0001,
                length: 34,
            }))
        );
    }

    #[test]
    fn data_event_reads_its_length_and_bytes() {
        let input = Input::parse(EVENT_DATA, &data_payload(3, &[1, 2, 3, 4]));

        assert_eq!(input, Some(data(&[1, 2, 3])));
    }

    #[test]
    fn data_event_longer_than_60_bytes_reads_60_bytes() {
        let input = Input::parse(EVENT_DATA, &data_payload(1000, &[7; 60]));

        assert_eq!(input, Some(data(&[7; 60])));
    }

    #[test]
    fn data_event_gets_no_answer() {
        let mut uvc = Uvc::new(LIMITS);

        assert_eq!(uvc.handle(data(&[1, 2])), None);
    }

    #[test]
    fn data_answer_has_its_length() {
        let (length, _) = Response::data(&[1, 2]).wire();

        assert_eq!(length, 2);
    }

    #[test]
    fn accept_has_the_length_of_the_data_stage() {
        let (length, _) = Response::Accept(34).wire();

        assert_eq!(length, 34);
    }

    #[test]
    fn stall_has_a_negative_length() {
        let (length, _) = Response::Stall.wire();

        assert!(length < 0);
    }

    #[test]
    fn disconnect_during_a_stream_stops_it_and_the_next_stream_starts() {
        let mut uvc = Uvc::new(LIMITS);
        let _ = uvc.handle(Input::StreamOn);

        assert_eq!(uvc.handle(Input::Disconnect), Some(Action::StopStream));
        assert_eq!(uvc.handle(Input::StreamOn), Some(Action::StartStream));
    }

    #[test]
    fn adopted_stream_does_not_start_again() {
        let mut uvc = Uvc::with_stream(LIMITS);

        assert_eq!(uvc.handle(Input::StreamOn), None);
    }

    #[test]
    fn adopted_stream_stops_on_stream_off() {
        let mut uvc = Uvc::with_stream(LIMITS);

        assert_eq!(uvc.handle(Input::StreamOff), Some(Action::StopStream));
    }

    #[test]
    fn probe_length_is_little_endian() {
        let mut uvc = Uvc::new(LIMITS);

        let answered = uvc.handle(setup(GET_LEN, VS_PROBE, STREAMING, 2));

        assert_eq!(answered, Some(answer(&[0x22, 0x00])));
    }

    #[test]
    fn probe_block_holds_the_only_format() {
        let block = StreamingControl::new(LIMITS);

        #[rustfmt::skip]
        assert_eq!(block.as_bytes(), [
            0x01, 0x00,             // bmHint
            0x01,                   // bFormatIndex
            0x01,                   // bFrameIndex
            0x15, 0x16, 0x05, 0x00, // dwFrameInterval, 333333
            0x00, 0x00,             // wKeyFrameRate
            0x00, 0x00,             // wPFrameRate
            0x00, 0x00,             // wCompQuality
            0x00, 0x00,             // wCompWindowSize
            0x00, 0x00,             // wDelay
            0x00, 0x48, 0x3f, 0x00, // dwMaxVideoFrameSize, 4147200
            0x00, 0x08, 0x00, 0x00, // dwMaxPayloadTransferSize, 2048
            0x00, 0x6c, 0xdc, 0x02, // dwClockFrequency, 48000000
            0x03,                   // bmFramingInfo
            0x01,                   // bPreferedVersion
            0x01,                   // bMinVersion
            0x01,                   // bMaxVersion
        ]);
    }

    #[test]
    fn out_of_range_probe_from_the_host_reads_as_the_only_format() {
        let mut uvc = Uvc::new(LIMITS);
        let _ = uvc.handle(setup(SET_CUR, VS_PROBE, STREAMING, 34));
        let _ = uvc.handle(data(&[0xff; 34]));

        let answered = uvc.handle(setup(GET_CUR, VS_PROBE, STREAMING, 34));

        assert_eq!(
            answered,
            Some(answer(StreamingControl::new(LIMITS).as_bytes()))
        );
    }

    #[test]
    fn brightness_above_the_range_reads_as_the_maximum() {
        let mut uvc = Uvc::new(LIMITS);
        let _ = uvc.handle(setup(SET_CUR, PU_BRIGHTNESS, PROCESSING_UNIT, 2));
        let _ = uvc.handle(data(&1000_i16.to_le_bytes()));

        let answered = uvc.handle(setup(GET_CUR, PU_BRIGHTNESS, PROCESSING_UNIT, 2));

        assert_eq!(answered, Some(answer(&BRIGHTNESS_MAX.to_le_bytes())));
    }

    #[test]
    fn brightness_below_the_range_reads_as_zero() {
        let mut uvc = Uvc::new(LIMITS);
        let _ = uvc.handle(setup(SET_CUR, PU_BRIGHTNESS, PROCESSING_UNIT, 2));
        let _ = uvc.handle(data(&(-5_i16).to_le_bytes()));

        let answered = uvc.handle(setup(GET_CUR, PU_BRIGHTNESS, PROCESSING_UNIT, 2));

        assert_eq!(answered, Some(answer(&0_i16.to_le_bytes())));
    }

    #[test]
    fn data_after_another_setup_request_does_not_change_brightness() {
        let mut uvc = Uvc::new(LIMITS);
        let _ = uvc.handle(setup(SET_CUR, PU_BRIGHTNESS, PROCESSING_UNIT, 2));
        let _ = uvc.handle(setup(SET_CUR, VS_PROBE, STREAMING, 34));
        let _ = uvc.handle(data(&[10, 0]));

        let answered = uvc.handle(setup(GET_CUR, PU_BRIGHTNESS, PROCESSING_UNIT, 2));

        assert_eq!(answered, Some(answer(&BRIGHTNESS_DEFAULT.to_le_bytes())));
    }

    #[test]
    fn second_data_event_does_not_change_brightness() {
        let mut uvc = Uvc::new(LIMITS);
        let _ = uvc.handle(setup(SET_CUR, PU_BRIGHTNESS, PROCESSING_UNIT, 2));
        let _ = uvc.handle(data(&[100, 0]));
        let _ = uvc.handle(data(&[200, 0]));

        let answered = uvc.handle(setup(GET_CUR, PU_BRIGHTNESS, PROCESSING_UNIT, 2));

        assert_eq!(answered, Some(answer(&[100, 0])));
    }

    #[test]
    fn unknown_control_sets_the_error_code_invalid_control() {
        let mut uvc = Uvc::new(LIMITS);
        let _ = uvc.handle(setup(GET_CUR, UNKNOWN_SELECTOR, PROCESSING_UNIT, 2));

        let answered = uvc.handle(setup(GET_CUR, VC_REQUEST_ERROR_CODE, CONTROL, 1));

        assert_eq!(answered, Some(answer(&[ErrorCode::InvalidControl as u8])));
    }

    #[test]
    fn successful_request_clears_the_error_code() {
        let mut uvc = Uvc::new(LIMITS);
        let _ = uvc.handle(setup(GET_CUR, UNKNOWN_SELECTOR, PROCESSING_UNIT, 2));
        let _ = uvc.handle(setup(GET_CUR, PU_BRIGHTNESS, PROCESSING_UNIT, 2));

        let answered = uvc.handle(setup(GET_CUR, VC_REQUEST_ERROR_CODE, CONTROL, 1));

        assert_eq!(answered, Some(answer(&[ErrorCode::NoError as u8])));
    }

    #[test]
    fn answers_to_setup_requests() {
        let requests = [
            ("probe", VS_PROBE, STREAMING, 34),
            ("commit", VS_COMMIT, STREAMING, 34),
            ("error code", VC_REQUEST_ERROR_CODE, CONTROL, 1),
            ("brightness", PU_BRIGHTNESS, PROCESSING_UNIT, 2),
            ("auto exposure mode", CT_AE_MODE, CAMERA_TERMINAL, 1),
            ("unknown control", UNKNOWN_SELECTOR, PROCESSING_UNIT, 1),
            ("unknown unit", PU_BRIGHTNESS, UNKNOWN_UNIT, 1),
            ("unknown interface", VS_PROBE, UNKNOWN_INTERFACE, 1),
        ];
        let codes = [
            ("SET_CUR", SET_CUR),
            ("GET_CUR", GET_CUR),
            ("GET_MIN", GET_MIN),
            ("GET_MAX", GET_MAX),
            ("GET_RES", GET_RES),
            ("GET_LEN", GET_LEN),
            ("GET_INFO", GET_INFO),
            ("GET_DEF", GET_DEF),
        ];
        let probe_block = StreamingControl::new(LIMITS);
        let mut table = String::from("request | code | answer | error code after\n");
        for (name, selector, index, length) in requests {
            for (code_name, code) in codes {
                let mut uvc = Uvc::new(LIMITS);
                let answer = match uvc.handle(setup(code, selector, index, length)) {
                    Some(Action::Respond(Response::Data(data)))
                        if data.as_slice() == probe_block.as_bytes() =>
                    {
                        "probe block".to_owned()
                    }
                    Some(Action::Respond(Response::Data(data))) => data
                        .as_slice()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    Some(Action::Respond(Response::Accept(len))) => format!("accept {len}"),
                    Some(Action::Respond(Response::Stall)) => "stall".to_owned(),
                    other => format!("{other:?}"),
                };
                let error = match uvc.handle(setup(GET_CUR, VC_REQUEST_ERROR_CODE, CONTROL, 1)) {
                    Some(Action::Respond(Response::Data(data))) => format!("{:02x}", data.data[0]),
                    other => format!("{other:?}"),
                };
                writeln!(table, "{name} | {code_name} | {answer} | {error}").unwrap();
            }
        }
        insta::assert_snapshot!(table);
    }
}
