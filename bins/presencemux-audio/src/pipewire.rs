//! The PipeWire client of the gate. A capture stream reads the audio
//! interface. A playback stream writes the gadget mic. The capture stream
//! triggers the playback stream. The playback stream gates the samples in the
//! same cycle, as `libpipewire-module-loopback` does.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::thread;

use pipewire::context::ContextRc;
use pipewire::core::PW_ID_CORE;
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::{PropertiesBox, properties};
use pipewire::registry::GlobalObject;
use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS};
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Object, Pod, Value};
use pipewire::spa::utils::Direction;
use pipewire::spa::utils::dict::DictRef;
use pipewire::stream::{Stream, StreamFlags, StreamRc, StreamState};
use pipewire::types::ObjectType;
use pipewire::{keys, spa};
use presencemux_api::audio::DeviceStatus;
use tokio::sync::oneshot;
use zerocopy::FromBytes;

use crate::clock;
use crate::fade::{Fader, Mode};
use crate::gate;

/// The node of the audio interface input, from the WirePlumber configuration.
const INTERFACE_NODE: &str = "presencemux.interface.in";
/// The node of the gadget mic, from the WirePlumber configuration.
const GADGET_NODE: &str = "presencemux.gadget.mic";
/// The group of the two streams. PipeWire schedules the two streams together.
/// WirePlumber does not link one stream to the other.
const GROUP: &str = "presencemux.mic";
const RATE: u32 = 48_000;
/// The size of one sample, F32 in a planar buffer.
const SAMPLE_SIZE: usize = size_of::<f32>();
/// The stride of one frame in bytes. A frame has one sample.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "the size of one sample fits in i32"
)]
const STRIDE: i32 = SAMPLE_SIZE as i32;

/// The gate word of a lease that denies the mic. The gate closes with a ramp.
pub(crate) const DENIED: u64 = 0;
/// The gate word without a valid lease, or with a latch. It is a time in the
/// past. The gate closes at once.
pub(crate) const BLOCKED: u64 = 1;

/// The state that the PipeWire thread shares with the other threads.
#[derive(Debug)]
pub(crate) struct RtState {
    /// The gate for the real-time thread: [`DENIED`], [`BLOCKED`], or the time
    /// on `CLOCK_MONOTONIC` in nanoseconds before which the gate passes audio.
    gate: AtomicU64,
    /// The state of the capture stream.
    capture: AtomicU8,
    /// The state of the playback stream.
    playback: AtomicU8,
    /// Whether a node of the audio interface input exists.
    interface: AtomicBool,
}

impl Default for RtState {
    /// Returns a state with a blocked gate, two starting streams and no audio
    /// interface.
    fn default() -> Self {
        Self {
            gate: AtomicU64::new(BLOCKED),
            capture: AtomicU8::new(STARTING),
            playback: AtomicU8::new(STARTING),
            interface: AtomicBool::new(false),
        }
    }
}

impl RtState {
    /// Passes the state of the gate to the real-time thread. An open time
    /// out of range blocks the gate.
    pub(crate) fn set_gate(&self, state: gate::State) {
        let word = match state {
            gate::State::Open { until } => u64::try_from(until.as_nanos())
                .ok()
                .filter(|&until| until > BLOCKED)
                .unwrap_or(BLOCKED),
            gate::State::Denied => DENIED,
            gate::State::Blocked => BLOCKED,
        };
        self.gate.store(word, Ordering::Release);
    }

    /// Returns the gate word for the real-time thread.
    #[cfg(test)]
    pub(crate) fn gate_word(&self) -> u64 {
        self.gate.load(Ordering::Acquire)
    }

    /// Returns the status of the gate from the state of its two streams.
    /// `Ready` means that PipeWire can link and run both streams.
    pub(crate) fn streams(&self) -> DeviceStatus {
        let states = [&self.capture, &self.playback].map(|state| state.load(Ordering::Acquire));
        if states.contains(&FAULT) {
            DeviceStatus::Fault
        } else if states.iter().all(|&state| state == LINKABLE) {
            DeviceStatus::Ready
        } else {
            DeviceStatus::Initializing
        }
    }

    /// Returns the status of the audio interface. The status is `Ready` while
    /// a node of the interface exists. Otherwise, it is `Missing`.
    pub(crate) fn interface(&self) -> DeviceStatus {
        if self.interface.load(Ordering::Acquire) {
            DeviceStatus::Ready
        } else {
            DeviceStatus::Missing
        }
    }
}

/// A stream that does not run yet.
const STARTING: u8 = 0;
/// A stream that PipeWire can link and run.
const LINKABLE: u8 = 1;
/// A stream with an error.
const FAULT: u8 = 2;

/// An error of the PipeWire thread.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("cannot start the PipeWire thread: {0}")]
    Thread(std::io::Error),
    #[error("cannot prepare the PipeWire client: {0}")]
    Setup(#[from] pipewire::Error),
    #[error("the connection to PipeWire failed: {0}")]
    Connection(String),
    #[error("the PipeWire thread has ended")]
    Ended,
}

/// Starts the PipeWire client on its own thread. The receiver gets the error
/// when the thread ends.
pub(crate) fn spawn(rt: Arc<RtState>) -> Result<oneshot::Receiver<Error>, Error> {
    let (ended, receiver) = oneshot::channel();
    thread::Builder::new()
        .name("pipewire".to_owned())
        .spawn(move || {
            let _ = ended.send(run(&rt));
        })
        .map_err(Error::Thread)?;
    Ok(receiver)
}

/// Runs the PipeWire client until the connection fails, and returns the
/// error.
fn run(rt: &Arc<RtState>) -> Error {
    match connect_and_run(rt) {
        Ok(error) | Err(error) => error,
    }
}

/// Connects to PipeWire, creates the streams and runs the main loop. Returns
/// the error that ends the main loop, or an error before the main loop.
fn connect_and_run(rt: &Arc<RtState>) -> Result<Error, Error> {
    pipewire::init();
    let main_loop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;

    let failure = Rc::new(RefCell::new(None));
    let _core_listener = core
        .add_listener_local()
        .error({
            let main_loop = main_loop.downgrade();
            let failure = Rc::clone(&failure);
            move |id, _seq, _res, message| {
                if id == PW_ID_CORE {
                    *failure.borrow_mut() = Some(message.to_owned());
                    if let Some(main_loop) = main_loop.upgrade() {
                        main_loop.quit();
                    }
                }
            }
        })
        .register();

    let registry = core.get_registry_rc()?;
    let interfaces = Rc::new(RefCell::new(BTreeSet::new()));
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let rt = Arc::clone(rt);
            let interfaces = Rc::clone(&interfaces);
            move |global| {
                if is_interface(global) {
                    interfaces.borrow_mut().insert(global.id);
                    rt.interface.store(true, Ordering::Release);
                }
            }
        })
        .global_remove({
            let rt = Arc::clone(rt);
            move |id| {
                let mut interfaces = interfaces.borrow_mut();
                if interfaces.remove(&id) {
                    rt.interface
                        .store(!interfaces.is_empty(), Ordering::Release);
                }
            }
        })
        .register();

    let playback = StreamRc::new(core.clone(), "presencemux-mic-out", playback_props())?;
    let capture = StreamRc::new(core.clone(), "presencemux-mic-in", capture_props())?;
    // PipeWire calls only the process callback of the first listener that has
    // one. Each process listener has its own user data.
    let _playback_process = playback
        .add_local_listener_with_user_data(Fader::default())
        .process({
            let capture = capture.clone();
            let rt = Arc::clone(rt);
            move |playback, fader| gate_cycle(&capture, playback, fader, &rt)
        })
        .register()?;
    let _playback_state = playback
        .add_local_listener()
        .state_changed(state_changed(Arc::clone(rt), Side::Playback))
        .register()?;
    let _capture_process = capture
        .add_local_listener()
        .process({
            let playback = playback.clone();
            move |capture, ()| {
                // If the trigger fails, no playback cycle reads the capture
                // buffers. This callback dequeues them.
                if playback.trigger_process().is_err() {
                    while capture.dequeue_buffer().is_some() {}
                }
            }
        })
        .register()?;
    let _capture_state = capture
        .add_local_listener()
        .state_changed(state_changed(Arc::clone(rt), Side::Capture))
        .register()?;

    let flags = StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS;
    let playback_format = format(spa::sys::SPA_AUDIO_CHANNEL_MONO);
    playback.connect(
        Direction::Output,
        None,
        flags | StreamFlags::TRIGGER,
        &mut [pod(&playback_format)],
    )?;
    let capture_format = format(spa::sys::SPA_AUDIO_CHANNEL_FL);
    // The capture stream is asynchronous. The playback cycle dequeues its
    // buffers.
    let async_flag =
        StreamFlags::from_bits_retain(pipewire::sys::pw_stream_flags_PW_STREAM_FLAG_ASYNC);
    capture.connect(
        Direction::Input,
        None,
        flags | async_flag,
        &mut [pod(&capture_format)],
    )?;

    main_loop.run();
    let failure = failure.borrow_mut().take();
    Ok(failure.map_or(Error::Ended, Error::Connection))
}

/// Returns whether `global` is a node of the audio interface input.
fn is_interface(global: &GlobalObject<&DictRef>) -> bool {
    global.type_ == ObjectType::Node
        && global
            .props
            .and_then(|props| props.get(*keys::NODE_NAME))
            .is_some_and(|name| name == INTERFACE_NODE)
}

/// The properties of the stream from the audio interface. It reads the
/// first channel.
fn capture_props() -> PropertiesBox {
    properties! {
        *keys::NODE_NAME => "presencemux.mic.in",
        *keys::NODE_DESCRIPTION => "PresenceMux mic gate",
        *keys::MEDIA_TYPE => "Audio",
        *keys::MEDIA_CATEGORY => "Capture",
        *keys::TARGET_OBJECT => INTERFACE_NODE,
        *keys::NODE_GROUP => GROUP,
        *keys::NODE_LINK_GROUP => GROUP,
        *keys::NODE_VIRTUAL => "true",
        *keys::STREAM_DONT_REMIX => "true",
        "node.dont-fallback" => "true",
        "node.linger" => "true",
        "resample.prefill" => "true",
    }
}

/// The properties of the stream to the gadget mic.
fn playback_props() -> PropertiesBox {
    properties! {
        *keys::NODE_NAME => "presencemux.mic.out",
        *keys::NODE_DESCRIPTION => "PresenceMux mic gate",
        *keys::MEDIA_TYPE => "Audio",
        *keys::MEDIA_CATEGORY => "Playback",
        *keys::TARGET_OBJECT => GADGET_NODE,
        *keys::NODE_GROUP => GROUP,
        *keys::NODE_LINK_GROUP => GROUP,
        *keys::NODE_VIRTUAL => "true",
        "node.dont-fallback" => "true",
        "node.linger" => "true",
        "resample.prefill" => "true",
    }
}

/// Returns the format of one planar F32 channel at 48 kHz in `position`, as
/// the bytes of a pod.
fn format(position: u32) -> Vec<u8> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32P);
    info.set_rate(RATE);
    info.set_channels(1);
    let mut positions = [0; MAX_CHANNELS];
    positions[0] = position;
    info.set_position(positions);
    let object = Value::Object(Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Format,
        id: spa::sys::SPA_PARAM_EnumFormat,
        properties: info.into(),
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object)
        .expect("a format object always serializes")
        .0
        .into_inner()
}

fn pod(bytes: &[u8]) -> &Pod {
    Pod::from_bytes(bytes).expect("the serializer writes a valid pod")
}

#[derive(Clone, Copy, Debug)]
enum Side {
    Capture,
    Playback,
}

/// Returns the callback that stores the state of the stream on `side`.
fn state_changed(
    rt: Arc<RtState>,
    side: Side,
) -> impl Fn(&Stream, &mut (), StreamState, StreamState) + 'static {
    move |_stream, (), _old, new| {
        let state = match &new {
            StreamState::Paused | StreamState::Streaming => LINKABLE,
            StreamState::Unconnected | StreamState::Connecting => STARTING,
            StreamState::Error(message) => {
                tracing::error!("the {side:?} stream of the gate failed: {message}");
                FAULT
            }
        };
        let target = match side {
            Side::Capture => &rt.capture,
            Side::Playback => &rt.playback,
        };
        target.store(state, Ordering::Release);
    }
}

/// Reads the newest capture buffer and writes the gated samples to the
/// playback buffer. Runs on the real-time thread.
fn gate_cycle(capture: &Stream, playback: &Stream, fader: &mut Fader, rt: &RtState) {
    let mut input = None;
    while let Some(buffer) = capture.dequeue_buffer() {
        input = Some(buffer);
    }
    let Some(mut output) = playback.dequeue_buffer() else {
        return;
    };
    let mode = mode(rt.gate.load(Ordering::Acquire), clock::now_nanos());
    let requested = usize::try_from(output.requested()).unwrap_or(usize::MAX);
    let in_samples = input
        .as_mut()
        .and_then(|buffer| buffer.datas_mut().first_mut())
        .and_then(|data| {
            let chunk = data.chunk();
            let (offset, size) = (chunk.offset(), chunk.size());
            chunk_samples(data.data()?, offset, size)
        });
    // A buffer without a data plane carries no samples.
    let Some(out_data) = output.datas_mut().first_mut() else {
        return;
    };
    let frames = out_data
        .data()
        .and_then(samples_mut)
        .map_or(0, |out_samples| {
            gate_frames(in_samples, out_samples, requested, mode, fader)
        });
    let chunk = out_data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = STRIDE;
    *chunk.size_mut() = u32::try_from(frames * SAMPLE_SIZE).unwrap_or(0);
}

/// Returns the mode of the fader for the gate word `word` at `now`, in
/// nanoseconds on `CLOCK_MONOTONIC`.
fn mode(word: u64, now: u64) -> Mode {
    if word == DENIED {
        Mode::Close
    } else if now < word {
        Mode::Open
    } else {
        Mode::Cut
    }
}

/// Writes the gated `input` to `output`, or `requested` frames of zeros
/// without input. Returns the number of frames, at most the size of
/// `output`.
fn gate_frames(
    input: Option<&[f32]>,
    output: &mut [f32],
    requested: usize,
    mode: Mode,
    fader: &mut Fader,
) -> usize {
    if let Some(input) = input {
        let frames = input.len().min(output.len());
        fader.process(&input[..frames], &mut output[..frames], mode);
        frames
    } else {
        let frames = requested.min(output.len());
        fader.silence(&mut output[..frames]);
        frames
    }
}

/// Returns the samples of the chunk at `offset` with `size` bytes in `bytes`,
/// or `None` if the chunk is out of range or not aligned.
fn chunk_samples(bytes: &[u8], offset: u32, size: u32) -> Option<&[f32]> {
    let start = usize::try_from(offset).ok()?;
    let end = start.checked_add(usize::try_from(size).ok()?)?;
    let bytes = bytes.get(start..end)?;
    let whole = bytes.len() / SAMPLE_SIZE * SAMPLE_SIZE;
    <[f32]>::ref_from_bytes(&bytes[..whole]).ok()
}

/// Returns the whole samples in `bytes`, or `None` if `bytes` is not aligned.
fn samples_mut(bytes: &mut [u8]) -> Option<&mut [f32]> {
    let whole = bytes.len() / SAMPLE_SIZE * SAMPLE_SIZE;
    <[f32]>::mut_from_bytes(&mut bytes[..whole]).ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use zerocopy::IntoBytes;

    use super::*;

    /// A time on `CLOCK_MONOTONIC` in nanoseconds.
    const NOW: u64 = 5_000_000_000;

    #[test]
    fn denied_word_closes_with_the_ramp() {
        assert_eq!(mode(DENIED, NOW), Mode::Close);
    }

    #[test]
    fn blocked_word_cuts() {
        assert_eq!(mode(BLOCKED, NOW), Mode::Cut);
    }

    #[test]
    fn open_word_before_its_time_opens() {
        assert_eq!(mode(NOW + 1, NOW), Mode::Open);
    }

    #[test]
    fn open_word_at_its_time_cuts() {
        assert_eq!(mode(NOW, NOW), Mode::Cut);
    }

    #[test]
    fn open_state_out_of_range_blocks_the_gate() {
        let rt = RtState::default();

        rt.set_gate(gate::State::Open {
            until: Duration::MAX,
        });

        assert_eq!(rt.gate_word(), BLOCKED);
    }

    #[test]
    fn open_state_gives_its_time_in_nanoseconds() {
        let rt = RtState::default();

        rt.set_gate(gate::State::Open {
            until: Duration::from_secs(7),
        });

        assert_eq!(rt.gate_word(), 7_000_000_000);
    }

    #[test]
    fn no_input_writes_zeros_for_the_requested_frames() {
        let mut output = [9.0; 8];

        gate_frames(None, &mut output, 5, Mode::Open, &mut Fader::default());

        assert_eq!(output, [0.0, 0.0, 0.0, 0.0, 0.0, 9.0, 9.0, 9.0]);
    }

    #[test]
    fn request_without_input_larger_than_the_output_gives_the_output_size() {
        let mut output = [9.0; 8];

        let frames = gate_frames(None, &mut output, 100, Mode::Open, &mut Fader::default());

        assert_eq!(frames, 8);
    }

    #[test]
    fn input_longer_than_the_output_gives_the_size_of_the_output() {
        let mut output = [9.0; 4];

        let frames = gate_frames(
            Some(&[1.0; 6]),
            &mut output,
            6,
            Mode::Cut,
            &mut Fader::default(),
        );

        assert_eq!(frames, 4);
    }

    #[test]
    fn input_in_cut_mode_gives_zeros() {
        let mut output = [9.0; 4];

        gate_frames(
            Some(&[1.0; 4]),
            &mut output,
            4,
            Mode::Cut,
            &mut Fader::default(),
        );

        assert_eq!(output, [0.0; 4]);
    }

    #[test]
    fn chunk_in_range_gives_its_samples() {
        let buffer = [1.0_f32, 2.0, 3.0, 4.0];

        let samples = chunk_samples(buffer.as_bytes(), 4, 8);

        assert_eq!(samples, Some(&[2.0, 3.0][..]));
    }

    #[test]
    fn chunk_out_of_range_gives_none() {
        let buffer = [1.0_f32, 2.0];

        let samples = chunk_samples(buffer.as_bytes(), 4, 8);

        assert_eq!(samples, None);
    }

    #[test]
    fn chunk_at_an_unaligned_offset_gives_none() {
        let buffer = [1.0_f32, 2.0, 3.0];

        let samples = chunk_samples(buffer.as_bytes(), 1, 4);

        assert_eq!(samples, None);
    }

    #[test]
    fn chunk_with_a_partial_sample_gives_the_whole_samples() {
        let buffer = [1.0_f32, 2.0, 3.0];

        let samples = chunk_samples(buffer.as_bytes(), 0, 6);

        assert_eq!(samples, Some(&[1.0][..]));
    }

    #[test]
    fn new_rt_state_blocks_the_gate() {
        let rt = RtState::default();

        assert_eq!(rt.gate_word(), BLOCKED);
    }

    #[test]
    fn open_state_at_one_nanosecond_blocks_the_gate() {
        let rt = RtState::default();

        rt.set_gate(gate::State::Open {
            until: Duration::from_nanos(1),
        });

        assert_eq!(rt.gate_word(), BLOCKED);
    }

    #[test]
    fn interface_without_a_node_is_missing() {
        let rt = RtState::default();

        assert_eq!(rt.interface(), DeviceStatus::Missing);
    }

    #[test]
    fn stream_with_an_error_gives_a_fault() {
        let rt = RtState::default();
        rt.capture.store(LINKABLE, Ordering::Release);
        rt.playback.store(FAULT, Ordering::Release);

        assert_eq!(rt.streams(), DeviceStatus::Fault);
    }

    #[test]
    fn two_linkable_streams_give_ready() {
        let rt = RtState::default();
        rt.capture.store(LINKABLE, Ordering::Release);
        rt.playback.store(LINKABLE, Ordering::Release);

        assert_eq!(rt.streams(), DeviceStatus::Ready);
    }

    #[test]
    fn one_starting_stream_gives_initializing() {
        let rt = RtState::default();
        rt.capture.store(LINKABLE, Ordering::Release);

        assert_eq!(rt.streams(), DeviceStatus::Initializing);
    }
}
