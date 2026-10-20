//! The answers to the setup requests of the host, and the stream state.
//!
//! A commit of the format (`SET_CUR` of `VS_COMMIT_CONTROL`) starts the
//! stream. A commit during a stream restarts it. An `ENABLE`, `DISABLE` or
//! `UNBIND` event of FunctionFS stops the stream. A write to the endpoint that
//! does not complete in time also stops it.
//!
//! The function has no unit or terminal controls. `f_fs` writes the interface
//! number over all of `wIndex`, and the unit or terminal ID is lost.

use zerocopy::little_endian::{U16, U32};
use zerocopy::{Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::{CONTROL_INTERFACE, Format, STREAMING_INTERFACE};

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

const VS_PROBE: u8 = 0x01;
const VS_COMMIT: u8 = 0x02;
const VC_REQUEST_ERROR_CODE: u8 = 0x02;

/// The `bmRequestType` bit for a request from the device to the host.
const DIR_IN: u8 = 0x80;
/// The `bmRequestType` bits of the request type.
const TYPE_MASK: u8 = 0x60;
const TYPE_CLASS: u8 = 0x20;

/// The `bmHint` bit for a fixed frame interval.
const HINT_FRAME_INTERVAL: u16 = 0x0001;
/// The `bmFramingInfo` bits FID and EOF, which each payload header sets.
const FRAMING_FID_EOF: u8 = 0x03;
/// The clock of the PTS and SCR fields. The payload headers have neither.
const CLOCK_FREQUENCY: u32 = 48_000_000;

/// The length of `struct uvc_streaming_control` in UVC 1.1.
const STREAMING_CONTROL_LEN: u16 = 34;

/// The event types of FunctionFS (`enum usb_functionfs_event_type`).
const FUNCTIONFS_BIND: u8 = 0;
const FUNCTIONFS_UNBIND: u8 = 1;
const FUNCTIONFS_ENABLE: u8 = 2;
const FUNCTIONFS_DISABLE: u8 = 3;
const FUNCTIONFS_SETUP: u8 = 4;
const FUNCTIONFS_SUSPEND: u8 = 5;
const FUNCTIONFS_RESUME: u8 = 6;

/// An event of FunctionFS (`struct usb_functionfs_event`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Bind,
    Unbind,
    /// The host set the configuration or an interface. `f_fs` disabled and
    /// enabled all endpoints again.
    Enable,
    /// The host reset the bus or removed the configuration.
    Disable,
    Setup(Setup),
    Suspend,
    Resume,
}

impl Event {
    /// The size of `struct usb_functionfs_event`.
    pub const LEN: usize = 12;

    /// Reads an event from the bytes of `struct usb_functionfs_event`. Returns
    /// `None` for an unknown event type.
    pub fn parse(bytes: &[u8; Self::LEN]) -> Option<Self> {
        let [request_type, request, v0, v1, i0, i1, l0, l1, kind, ..] = *bytes;
        match kind {
            FUNCTIONFS_BIND => Some(Self::Bind),
            FUNCTIONFS_UNBIND => Some(Self::Unbind),
            FUNCTIONFS_ENABLE => Some(Self::Enable),
            FUNCTIONFS_DISABLE => Some(Self::Disable),
            FUNCTIONFS_SETUP => Some(Self::Setup(Setup {
                request_type,
                request,
                value: u16::from_le_bytes([v0, v1]),
                index: u16::from_le_bytes([i0, i1]),
                length: u16::from_le_bytes([l0, l1]),
            })),
            FUNCTIONFS_SUSPEND => Some(Self::Suspend),
            FUNCTIONFS_RESUME => Some(Self::Resume),
            _ => None,
        }
    }
}

/// A setup request of the host (`struct usb_ctrlrequest`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    /// Returns `true` for a request with a data stage from the device to the
    /// host.
    pub fn is_in(self) -> bool {
        self.request_type & DIR_IN != 0
    }

    fn is_class(self) -> bool {
        self.request_type & TYPE_MASK == TYPE_CLASS
    }

    fn selector(self) -> u8 {
        self.value.to_le_bytes()[1]
    }

    /// Returns the interface number of the function. `f_fs` writes it in place
    /// of the interface number of the configuration.
    fn interface(self) -> u8 {
        self.index.to_le_bytes()[0]
    }
}

/// The data stage of an answer, cut to the length that the host asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answer {
    len: u8,
    data: [u8; STREAMING_CONTROL_LEN as usize],
}

impl Answer {
    fn new(bytes: &[u8], length: u16) -> Self {
        let len = bytes.len().min(usize::from(length));
        let mut data = [0; STREAMING_CONTROL_LEN as usize];
        data[..len].copy_from_slice(&bytes[..len]);
        Self {
            len: u8::try_from(len).expect("an answer has at most 34 bytes"),
            data,
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data[..usize::from(self.len)]
    }
}

/// An action of the worker after an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Writes the data stage of an IN request to `ep0`.
    Write(Answer),
    /// Reads the data stage of an OUT request from `ep0`, then calls
    /// [`Function::data_stage`]. With a length of 0, the read only accepts
    /// the request.
    Read(u16),
    /// Stalls the request. The worker reads `ep0` for an IN request and
    /// writes `ep0` for an OUT request.
    Stall { is_in: bool },
    /// Starts the stream. The first frame is blank or a slate.
    Start,
    /// Starts the stream again after a new commit. The worker cancels the
    /// write in flight first.
    Restart,
    /// Stops the stream. The worker cancels the write in flight.
    Stop,
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
    /// Creates the answer for the only format. One payload holds one frame.
    fn new(format: Format) -> Self {
        Self {
            hint: U16::new(HINT_FRAME_INTERVAL),
            format_index: 1,
            frame_index: 1,
            frame_interval: U32::new(format.frame_interval),
            key_frame_rate: U16::ZERO,
            p_frame_rate: U16::ZERO,
            comp_quality: U16::ZERO,
            comp_window_size: U16::ZERO,
            delay: U16::ZERO,
            max_video_frame_size: U32::new(format.max_frame_size()),
            max_payload_transfer_size: U32::new(format.max_payload_size()),
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
    InvalidControl = 0x06,
    InvalidRequest = 0x07,
}

/// An OUT request whose data stage the worker reads next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Probe,
    Commit,
}

/// The UVC function: the answers to the host and the stream state.
#[derive(Clone, Debug)]
pub struct Function {
    format: Format,
    streaming: bool,
    error: ErrorCode,
    pending: Option<Pending>,
}

impl Function {
    pub fn new(format: Format) -> Self {
        Self {
            format,
            streaming: false,
            error: ErrorCode::NoError,
            pending: None,
        }
    }

    /// Returns `true` while the host has a committed stream.
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// Updates the state for an event and returns the action for it.
    #[must_use]
    pub fn event(&mut self, event: Event) -> Option<Action> {
        match event {
            Event::Setup(setup) => Some(self.setup(setup)),
            Event::Enable | Event::Disable | Event::Unbind => {
                self.pending = None;
                self.stop()
            }
            Event::Bind | Event::Suspend | Event::Resume => None,
        }
    }

    /// Updates the state after the worker read the data stage of the last
    /// OUT request. The function does not use the bytes. A data stage after
    /// another event changes nothing.
    #[must_use]
    pub fn data_stage(&mut self) -> Option<Action> {
        match self.pending.take() {
            Some(Pending::Commit) if self.streaming => Some(Action::Restart),
            Some(Pending::Commit) => {
                self.streaming = true;
                Some(Action::Start)
            }
            Some(Pending::Probe) | None => None,
        }
    }

    /// Stops the stream after a write that did not complete in time.
    #[must_use]
    pub fn write_timeout(&mut self) -> Option<Action> {
        self.stop()
    }

    fn stop(&mut self) -> Option<Action> {
        if !self.streaming {
            return None;
        }
        self.streaming = false;
        Some(Action::Stop)
    }

    fn setup(&mut self, setup: Setup) -> Action {
        self.pending = None;
        let stall = Action::Stall {
            is_in: setup.is_in(),
        };
        if !setup.is_class() {
            return stall;
        }
        match setup.interface() {
            STREAMING_INTERFACE => self.streaming_request(setup).unwrap_or(stall),
            CONTROL_INTERFACE => self.control_request(setup).unwrap_or(stall),
            _ => stall,
        }
    }

    fn streaming_request(&mut self, setup: Setup) -> Option<Action> {
        let answer = |bytes: &[u8]| Some(Action::Write(Answer::new(bytes, setup.length)));
        match (setup.selector(), setup.request) {
            (VS_PROBE, SET_CUR) => {
                self.pending = Some(Pending::Probe);
                Some(Action::Read(setup.length))
            }
            (VS_COMMIT, SET_CUR) => {
                self.pending = Some(Pending::Commit);
                Some(Action::Read(setup.length))
            }
            (VS_PROBE | VS_COMMIT, GET_CUR) | (VS_PROBE, GET_MIN | GET_MAX | GET_DEF) => {
                answer(StreamingControl::new(self.format).as_bytes())
            }
            (VS_PROBE, GET_RES) => answer(&[0; STREAMING_CONTROL_LEN as usize]),
            (VS_PROBE | VS_COMMIT, GET_INFO) => answer(&[INFO_GET | INFO_SET]),
            (VS_PROBE | VS_COMMIT, GET_LEN) => answer(&STREAMING_CONTROL_LEN.to_le_bytes()),
            _ => None,
        }
    }

    /// Answers a request to VideoControl, and keeps its result as the
    /// request error code.
    fn control_request(&mut self, setup: Setup) -> Option<Action> {
        let answer = |bytes: &[u8]| Action::Write(Answer::new(bytes, setup.length));
        let result = match (setup.selector(), setup.request) {
            (VC_REQUEST_ERROR_CODE, GET_CUR) => {
                // A read of the error code does not change it.
                return Some(answer(&[self.error as u8]));
            }
            (VC_REQUEST_ERROR_CODE, GET_INFO) => Ok(answer(&[INFO_GET])),
            (VC_REQUEST_ERROR_CODE, _) => Err(ErrorCode::InvalidRequest),
            _ => Err(ErrorCode::InvalidControl),
        };
        self.error = result.err().unwrap_or(ErrorCode::NoError);
        result.ok()
    }
}

#[cfg(test)]
mod tests {
    use core::fmt::Write;

    use proptest::prelude::*;

    use super::*;

    const FORMAT: Format = Format {
        width: 1920,
        height: 1080,
        frame_interval: 333_333,
    };

    const CONTROL: u16 = 0;
    const STREAMING: u16 = 1;
    const UNKNOWN_INTERFACE: u16 = 2;
    const UNKNOWN_SELECTOR: u8 = 0x7f;
    const CLASS_IN: u8 = 0xa1;
    const CLASS_OUT: u8 = 0x21;

    /// The request error code "no error".
    const NO_ERROR: u8 = 0x00;
    /// The request error code "invalid control".
    const INVALID_CONTROL: u8 = 0x06;

    /// The probe and commit answer for [`FORMAT`].
    #[rustfmt::skip]
    const PROBE_BLOCK: [u8; 34] = [
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
        0x02, 0x48, 0x3f, 0x00, // dwMaxPayloadTransferSize, 4147202
        0x00, 0x6c, 0xdc, 0x02, // dwClockFrequency, 48000000
        0x03,                   // bmFramingInfo
        0x01,                   // bPreferedVersion
        0x01,                   // bMinVersion
        0x01,                   // bMaxVersion
    ];

    fn setup(request: u8, selector: u8, index: u16, length: u16) -> Event {
        let request_type = if request & DIR_IN == 0 {
            CLASS_OUT
        } else {
            CLASS_IN
        };
        Event::Setup(Setup {
            request_type,
            request,
            value: u16::from(selector) << 8,
            index,
            length,
        })
    }

    fn answer(bytes: &[u8]) -> Action {
        Action::Write(Answer::new(bytes, 64))
    }

    /// Returns a function after a commit, with a running stream.
    fn streaming() -> Function {
        let mut function = Function::new(FORMAT);
        let _ = function.event(setup(SET_CUR, VS_COMMIT, STREAMING, 34));
        let _ = function.data_stage();
        function
    }

    fn event() -> impl Strategy<Value = Event> {
        prop_oneof![
            Just(Event::Bind),
            Just(Event::Unbind),
            Just(Event::Enable),
            Just(Event::Disable),
            Just(Event::Suspend),
            Just(Event::Resume),
            any::<(u8, u8, u16, u16, u16)>().prop_map(|(t, r, v, i, l)| {
                Event::Setup(Setup {
                    request_type: t,
                    request: r,
                    value: v,
                    index: i,
                    length: l,
                })
            }),
            Just(setup(SET_CUR, VS_COMMIT, STREAMING, 34)),
        ]
    }

    /// A step of the worker: an event, the data stage of the last request, or
    /// a write timeout.
    #[derive(Clone, Debug)]
    enum Step {
        Event(Event),
        Data,
        Timeout,
    }

    fn step() -> impl Strategy<Value = Step> {
        prop_oneof![
            event().prop_map(Step::Event),
            Just(Step::Data),
            Just(Step::Timeout),
        ]
    }

    proptest! {
        #[test]
        fn stream_actions_follow_the_stream_state(steps in proptest::collection::vec(step(), 0..64)) {
            let mut function = Function::new(FORMAT);
            let mut running = false;

            for step in steps {
                let action = match step {
                    Step::Event(event) => function.event(event),
                    Step::Data => function.data_stage(),
                    Step::Timeout => function.write_timeout(),
                };
                match action {
                    Some(Action::Start) => {
                        prop_assert!(!running);
                        running = true;
                    }
                    Some(Action::Restart) => prop_assert!(running),
                    Some(Action::Stop) => {
                        prop_assert!(running);
                        running = false;
                    }
                    _ => {}
                }
                prop_assert_eq!(function.is_streaming(), running);
            }
        }

        #[test]
        fn every_setup_gets_exactly_one_reply(event in event()) {
            let mut function = Function::new(FORMAT);

            let action = function.event(event);

            if let Event::Setup(_) = event {
                let replied = matches!(
                    action,
                    Some(Action::Write(_) | Action::Read(_) | Action::Stall { .. })
                );
                prop_assert!(replied);
            }
        }
    }

    #[test]
    fn parse_reads_each_event_type() {
        let kinds: Vec<Option<Event>> = (0..8)
            .map(|kind| {
                let mut bytes = [0; Event::LEN];
                bytes[8] = kind;
                Event::parse(&bytes)
            })
            .collect();

        assert!(matches!(
            kinds[..],
            [
                Some(Event::Bind),
                Some(Event::Unbind),
                Some(Event::Enable),
                Some(Event::Disable),
                Some(Event::Setup(_)),
                Some(Event::Suspend),
                Some(Event::Resume),
                None,
            ]
        ));
    }

    #[test]
    fn setup_event_reads_little_endian_fields() {
        let bytes = [0xa1, 0x81, 0x00, 0x01, 0x01, 0x00, 0x22, 0x00, 4, 0, 0, 0];

        let event = Event::parse(&bytes);

        assert_eq!(
            event,
            Some(Event::Setup(Setup {
                request_type: 0xa1,
                request: GET_CUR,
                value: 0x0100,
                index: 0x0001,
                length: 34,
            }))
        );
    }

    #[test]
    fn commit_starts_the_stream() {
        let mut function = Function::new(FORMAT);
        let read = function.event(setup(SET_CUR, VS_COMMIT, STREAMING, 34));

        let action = function.data_stage();

        assert_eq!(read, Some(Action::Read(34)));
        assert_eq!(action, Some(Action::Start));
    }

    #[test]
    fn commit_during_a_stream_restarts_it() {
        let mut function = streaming();
        let _ = function.event(setup(SET_CUR, VS_COMMIT, STREAMING, 34));

        let action = function.data_stage();

        assert_eq!(action, Some(Action::Restart));
    }

    #[test]
    fn probe_does_not_start_the_stream() {
        let mut function = Function::new(FORMAT);
        let _ = function.event(setup(SET_CUR, VS_PROBE, STREAMING, 34));

        let action = function.data_stage();

        assert_eq!(action, None);
    }

    #[test]
    fn commit_data_after_another_event_does_not_start_the_stream() {
        let mut function = Function::new(FORMAT);
        let _ = function.event(setup(SET_CUR, VS_COMMIT, STREAMING, 34));
        let _ = function.event(Event::Enable);

        let action = function.data_stage();

        assert_eq!(action, None);
    }

    #[test]
    fn short_commit_from_the_host_starts_the_stream() {
        let mut function = Function::new(FORMAT);
        let read = function.event(setup(SET_CUR, VS_COMMIT, STREAMING, 26));

        let action = function.data_stage();

        assert_eq!(read, Some(Action::Read(26)));
        assert_eq!(action, Some(Action::Start));
    }

    #[test]
    fn enable_stops_the_stream() {
        let mut function = streaming();

        assert_eq!(function.event(Event::Enable), Some(Action::Stop));
    }

    #[test]
    fn disable_stops_the_stream() {
        let mut function = streaming();

        assert_eq!(function.event(Event::Disable), Some(Action::Stop));
    }

    #[test]
    fn unbind_stops_the_stream() {
        let mut function = streaming();

        assert_eq!(function.event(Event::Unbind), Some(Action::Stop));
    }

    #[test]
    fn suspend_does_not_stop_the_stream() {
        let mut function = streaming();

        assert_eq!(function.event(Event::Suspend), None);
        assert!(function.is_streaming());
    }

    #[test]
    fn write_timeout_stops_the_stream() {
        let mut function = streaming();

        assert_eq!(function.write_timeout(), Some(Action::Stop));
        assert!(!function.is_streaming());
    }

    #[test]
    fn write_timeout_without_a_stream_does_nothing() {
        let mut function = Function::new(FORMAT);

        assert_eq!(function.write_timeout(), None);
    }

    #[test]
    fn answer_is_cut_to_the_length_of_the_request() {
        let mut function = Function::new(FORMAT);

        let action = function.event(setup(GET_CUR, VS_PROBE, STREAMING, 26));

        let Some(Action::Write(answer)) = action else {
            panic!("{action:?}");
        };
        assert_eq!(answer.as_slice(), &PROBE_BLOCK[..26]);
    }

    #[test]
    fn standard_request_to_the_interface_stalls() {
        let mut function = Function::new(FORMAT);

        let action = function.event(Event::Setup(Setup {
            request_type: 0x81,
            request: 0x06,
            value: 0x2200,
            index: 0,
            length: 64,
        }));

        assert_eq!(action, Some(Action::Stall { is_in: true }));
    }

    #[test]
    fn stall_of_an_out_request_is_a_write() {
        let mut function = Function::new(FORMAT);

        let action = function.event(setup(SET_CUR, UNKNOWN_SELECTOR, CONTROL, 1));

        assert_eq!(action, Some(Action::Stall { is_in: false }));
    }

    #[test]
    fn probe_answers_with_the_only_format() {
        let mut function = Function::new(FORMAT);

        let action = function.event(setup(GET_CUR, VS_PROBE, STREAMING, 34));

        assert_eq!(action, Some(answer(&PROBE_BLOCK)));
    }

    #[test]
    fn unknown_control_sets_the_error_code_invalid_control() {
        let mut function = Function::new(FORMAT);
        let _ = function.event(setup(GET_CUR, UNKNOWN_SELECTOR, CONTROL, 1));

        let action = function.event(setup(GET_CUR, VC_REQUEST_ERROR_CODE, CONTROL, 1));

        assert_eq!(action, Some(answer(&[INVALID_CONTROL])));
    }

    #[test]
    fn successful_request_clears_the_error_code() {
        let mut function = Function::new(FORMAT);
        let _ = function.event(setup(GET_CUR, UNKNOWN_SELECTOR, CONTROL, 1));
        let _ = function.event(setup(GET_INFO, VC_REQUEST_ERROR_CODE, CONTROL, 1));

        let action = function.event(setup(GET_CUR, VC_REQUEST_ERROR_CODE, CONTROL, 1));

        assert_eq!(action, Some(answer(&[NO_ERROR])));
    }

    #[test]
    fn answers_to_setup_requests() {
        let requests = [
            ("probe", VS_PROBE, STREAMING, 34),
            ("commit", VS_COMMIT, STREAMING, 34),
            ("error code", VC_REQUEST_ERROR_CODE, CONTROL, 1),
            ("unknown control", UNKNOWN_SELECTOR, CONTROL, 1),
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
        let mut table = String::from("request | code | answer | error code after\n");
        for (name, selector, index, length) in requests {
            for (code_name, code) in codes {
                let mut function = Function::new(FORMAT);
                let answer = match function.event(setup(code, selector, index, length)) {
                    Some(Action::Write(answer)) if answer.as_slice() == PROBE_BLOCK => {
                        "probe block".to_owned()
                    }
                    Some(Action::Write(answer)) => answer
                        .as_slice()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    Some(Action::Read(len)) => format!("read {len}"),
                    Some(Action::Stall { .. }) => "stall".to_owned(),
                    other => format!("{other:?}"),
                };
                let error = match function.event(setup(GET_CUR, VC_REQUEST_ERROR_CODE, CONTROL, 1))
                {
                    Some(Action::Write(answer)) => format!("{:02x}", answer.as_slice()[0]),
                    other => format!("{other:?}"),
                };
                writeln!(table, "{name} | {code_name} | {answer} | {error}").unwrap();
            }
        }
        insta::assert_snapshot!(table);
    }
}
