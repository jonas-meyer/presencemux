//! What the deck shows, and what a key press does. It has no I/O.
//!
//! presencemuxd classifies each plane (`exposure`, `flow`, `reason`). The deck
//! reads that classification once into [`Planes`], and [`screen`] derives
//! every key, touch point, the info bar and the brightness from it.
//! `docs/deck.md` (Model) holds the tables.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::ops::Index;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use presencemux_api::{
    CAM_BLANK, CAM_LIVE, CAM_NOTHING, CamStream, Cause, DeviceStatus, Exposure, Flow, Health, Mute,
    Plane, PresetValues, Reason, ReportState, Status, age,
};

use crate::config::Config;
use crate::protocol::{KEYS, Key};

/// What a key does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Applies `privacy` or a configured preset.
    Preset(String),
    /// Sets the cam to blank, live or a slate.
    Cam(Value),
    Mic(Mute),
    /// Turns a plane on or off, from what its key shows.
    Toggle(Plane),
    /// Applies the request of each resumable plane again.
    Resume,
    Nothing,
}

/// A key action in the configuration that the deck does not know.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown key action {0:?}: use preset:<name>, cam:<value>, mic:muted, mic:unmuted, \
     toggle:cam, toggle:mic, resume or none"
)]
pub(crate) struct UnknownAction(String);

impl FromStr for Action {
    type Err = UnknownAction;

    fn from_str(action: &str) -> Result<Self, Self::Err> {
        match action.split_once(':') {
            Some(("preset", name)) if !name.is_empty() => Ok(Self::Preset(name.to_owned())),
            Some(("cam", value)) if !value.is_empty() => Ok(Self::Cam(Value::cam(value))),
            Some(("mic", "muted")) => Ok(Self::Mic(Mute::Muted)),
            Some(("mic", "unmuted")) => Ok(Self::Mic(Mute::Unmuted)),
            Some(("toggle", "cam")) => Ok(Self::Toggle(Plane::Cam)),
            Some(("toggle", "mic")) => Ok(Self::Toggle(Plane::Mic)),
            None if action == "resume" => Ok(Self::Resume),
            None if action == "none" => Ok(Self::Nothing),
            _ => Err(UnknownAction(action.to_owned())),
        }
    }
}

/// A command for presencemuxd.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    ApplyPreset(String),
    SetCam(Value),
    SetMic(Mute),
    Resume(Plane),
}

/// The status of presencemuxd, with the values of its presets. The presets
/// change only with a new connection. The snapshots of one connection share
/// them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) status: Status,
    pub(crate) presets: Arc<Presets>,
}

/// The values of each preset by name, from presencemuxd.
pub(crate) type Presets = HashMap<String, Values>;

/// The value of a plane: off, a slate, or live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    Off,
    /// A slate, with its name.
    Slate(String),
    Live,
}

impl Value {
    /// Returns the value of `value`, a cam value of the API: `blank`,
    /// `nothing`, `live` or a slate name.
    fn cam(value: &str) -> Self {
        match value {
            CAM_BLANK | CAM_NOTHING => Self::Off,
            CAM_LIVE => Self::Live,
            name => Self::Slate(name.to_owned()),
        }
    }

    const fn mic(mute: Mute) -> Self {
        match mute {
            Mute::Muted => Self::Off,
            Mute::Unmuted => Self::Live,
        }
    }

    /// Returns the value as a cam value of the API: `blank`, `live` or a
    /// slate name.
    pub(crate) fn as_cam(&self) -> &str {
        match self {
            Self::Off => CAM_BLANK,
            Self::Slate(name) => name,
            Self::Live => CAM_LIVE,
        }
    }

    /// Returns the level of the value.
    const fn level(&self) -> Exposure {
        match self {
            Self::Off => Exposure::Off,
            Self::Slate(_) => Exposure::Slate,
            Self::Live => Exposure::Live,
        }
    }
}

/// The values that a key sets. `None` leaves a plane as it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Values {
    pub(crate) cam: Option<Value>,
    pub(crate) mic: Option<Value>,
}

impl From<&PresetValues> for Values {
    fn from(preset: &PresetValues) -> Self {
        Self {
            cam: preset.cam.as_deref().map(Value::cam),
            mic: preset.mic.map(Value::mic),
        }
    }
}

impl Values {
    /// Returns each plane that the values set, with its value.
    fn planes(&self) -> impl Iterator<Item = (Plane, &Value)> {
        [
            (Plane::Cam, self.cam.as_ref()),
            (Plane::Mic, self.mic.as_ref()),
        ]
        .into_iter()
        .filter_map(|(plane, value)| Some((plane, value?)))
    }

    /// Returns whether a key with these values is lit. A key is lit if every
    /// plane that it sets is in place.
    fn lit(&self, planes: &Planes<'_>) -> bool {
        self.planes()
            .all(|(plane, value)| planes[plane].in_place(value))
    }

    /// Returns whether the values raise a plane above its level.
    fn raise(&self, planes: &Planes<'_>) -> bool {
        self.planes()
            .any(|(plane, value)| value.level() > planes[plane].level())
    }

    /// Returns the first plane that the values raise to live while it has a
    /// hold.
    fn blocked_by<'p, 'a>(&self, planes: &'p Planes<'a>) -> Option<&'p PlaneView<'a>> {
        self.planes()
            .filter(|(_, value)| **value == Value::Live)
            .map(|(plane, _)| &planes[plane])
            .find(|state| state.held())
    }

    /// Returns the tone of a lit key with these values: red with a live value,
    /// amber with a slate, and light with off only.
    fn tone(&self) -> Tone {
        match self.planes().map(|(_, value)| value.level()).max() {
            Some(Exposure::Live) => Tone::Plane(PlaneTone::Live),
            Some(Exposure::Slate) => Tone::Plane(PlaneTone::Canned),
            Some(Exposure::Off) | None => Tone::Private,
        }
    }
}

/// Returns the name of `plane` on the deck.
const fn label(plane: Plane) -> &'static str {
    match plane {
        Plane::Cam => "CAM",
        Plane::Mic => "MIC",
    }
}

/// Why the exposure of a plane differs from its request. It is the reason of
/// the status, with the cause of a hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
    /// Automation holds the plane, with the cause if the status gives one.
    Held(Option<Cause>),
    Updating,
    Blocked,
    Waiting,
}

impl Why {
    /// Returns the reason of a plane with the status reason `reason` and the
    /// cause `hold` of its hold.
    const fn new(reason: Reason, hold: Option<Cause>) -> Self {
        match reason {
            Reason::Held => Self::Held(hold),
            Reason::Updating => Self::Updating,
            Reason::Blocked => Self::Blocked,
            Reason::Waiting => Self::Waiting,
        }
    }
}

/// One plane of the status, as the deck reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaneView<'a> {
    plane: Plane,
    requested: Value,
    /// What the host can get without a press. `None` if it is unknown.
    exposure: Option<Exposure>,
    /// The slate name of the exposure, if it has one.
    slate: Option<&'a str>,
    /// Whether the host gets the exposure now. `None` if it is unknown.
    flow: Option<Flow>,
    why: Option<Why>,
    resumable: bool,
    /// Whether the host receives the stream. `None` for the mic.
    stream: Option<CamStream>,
    /// The time at which the stream timeout sets the cam to blank.
    blank_at_usec: Option<i64>,
}

impl<'a> PlaneView<'a> {
    fn new(status: &'a Status, plane: Plane) -> Self {
        match plane {
            Plane::Cam => Self {
                plane,
                requested: Value::cam(&status.cam.requested),
                exposure: status.cam.exposure,
                slate: status.cam.slate.as_deref(),
                flow: status.cam.flow,
                why: status
                    .cam
                    .reason
                    .map(|reason| Why::new(reason, status.cam.hold)),
                resumable: status.cam.resumable,
                stream: Some(status.cam.stream),
                blank_at_usec: status.cam.blank_at_usec,
            },
            Plane::Mic => Self {
                plane,
                requested: Value::mic(status.mic.requested),
                exposure: status.mic.exposure,
                slate: None,
                flow: status.mic.flow,
                why: status
                    .mic
                    .reason
                    .map(|reason| Why::new(reason, status.mic.hold)),
                resumable: status.mic.resumable,
                stream: None,
                blank_at_usec: None,
            },
        }
    }

    /// Returns whether automation holds the plane.
    const fn held(&self) -> bool {
        matches!(self.why, Some(Why::Held(_)))
    }

    /// Returns the level that presses compare with: the lower of the request
    /// and the exposure. It is off while the plane has a hold or while the
    /// exposure is unknown.
    fn level(&self) -> Exposure {
        match (self.held(), self.exposure) {
            (false, Some(exposure)) => exposure.min(self.requested.level()),
            (true, _) | (false, None) => Exposure::Off,
        }
    }

    /// Returns whether the plane is in place for `value`: requested, without a
    /// reason, and exposed at the level and slate of the value.
    fn in_place(&self, value: &Value) -> bool {
        let exposed = match value {
            Value::Slate(name) => {
                self.exposure == Some(Exposure::Slate) && self.slate == Some(name.as_str())
            }
            Value::Off | Value::Live => self.exposure == Some(value.level()),
        };
        self.requested == *value && self.why.is_none() && exposed
    }

    /// Returns whether a toggle of this plane turns it on: on from off, and
    /// off from any other exposure.
    fn toggles_on(&self) -> bool {
        self.exposure == Some(Exposure::Off)
    }

    /// Returns the tone of the exposure.
    const fn tone(&self) -> PlaneTone {
        match self.exposure {
            None => PlaneTone::Unknown,
            Some(Exposure::Off) => PlaneTone::Off,
            Some(Exposure::Slate) => PlaneTone::Canned,
            Some(Exposure::Live) => PlaneTone::Live,
        }
    }

    /// Returns the word of the exposure: `ON AIR` or `LIVE`, the slate name,
    /// `OFF`, or `?`.
    fn word(&self) -> String {
        match (self.exposure, self.plane) {
            (None, _) => "?".to_owned(),
            (Some(Exposure::Off), _) => "OFF".to_owned(),
            (Some(Exposure::Slate), _) => self.slate.unwrap_or("SLATE").to_uppercase(),
            (Some(Exposure::Live), Plane::Cam) => "ON AIR".to_owned(),
            (Some(Exposure::Live), Plane::Mic) => "LIVE".to_owned(),
        }
    }

    /// Returns the line below the title: the reason, or `IDLE` while the host
    /// gets less than the exposure.
    fn line(&self) -> Option<String> {
        match (self.why, self.flow) {
            (Some(Why::Held(cause)), _) => Some(match cause {
                Some(cause) => format!("HELD: {}", words(cause)),
                None => "HELD".to_owned(),
            }),
            (Some(Why::Updating), _) => Some("UPDATING".to_owned()),
            (Some(Why::Blocked), _) => Some("BLOCKED".to_owned()),
            (Some(Why::Waiting), _) => Some("WAITING".to_owned()),
            (None, Some(Flow::Idle)) => Some("IDLE".to_owned()),
            (None, Some(Flow::Flowing) | None) => None,
        }
    }
}

/// Both planes of a status.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Planes<'a> {
    cam: PlaneView<'a>,
    mic: PlaneView<'a>,
}

impl<'a> Planes<'a> {
    fn new(status: &'a Status) -> Self {
        Self {
            cam: PlaneView::new(status, Plane::Cam),
            mic: PlaneView::new(status, Plane::Mic),
        }
    }

    fn iter(&self) -> impl Iterator<Item = &PlaneView<'a>> {
        [&self.cam, &self.mic].into_iter()
    }
}

impl<'a> Index<Plane> for Planes<'a> {
    type Output = PlaneView<'a>;

    fn index(&self, plane: Plane) -> &Self::Output {
        match plane {
            Plane::Cam => &self.cam,
            Plane::Mic => &self.mic,
        }
    }
}

/// A snapshot as the deck reads it.
struct Model<'a> {
    status: &'a Status,
    presets: &'a Presets,
    planes: Planes<'a>,
}

impl<'a> Model<'a> {
    fn new(snapshot: &'a Snapshot) -> Self {
        Self {
            status: &snapshot.status,
            presets: &snapshot.presets,
            planes: Planes::new(&snapshot.status),
        }
    }
}

/// What a key sets: a preset, or one plane.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target<'a> {
    /// A preset, with its values. `None` for a preset that presencemuxd does
    /// not have.
    Preset {
        name: &'a str,
        values: Option<&'a Values>,
    },
    Cam(Value),
    Mic(Mute),
}

impl<'a> Target<'a> {
    /// Returns what `action` sets in `model`, or `None` for a key that sets
    /// nothing.
    fn of(action: &'a Action, model: &Model<'a>) -> Option<Self> {
        match action {
            Action::Preset(name) => Some(Self::Preset {
                name,
                values: model.presets.get(name),
            }),
            Action::Cam(value) => Some(Self::Cam(value.clone())),
            Action::Mic(mute) => Some(Self::Mic(*mute)),
            Action::Toggle(Plane::Cam) => Some(Self::Cam(if model.planes.cam.toggles_on() {
                Value::Live
            } else {
                Value::Off
            })),
            Action::Toggle(Plane::Mic) => Some(Self::Mic(if model.planes.mic.toggles_on() {
                Mute::Unmuted
            } else {
                Mute::Muted
            })),
            Action::Resume | Action::Nothing => None,
        }
    }

    /// Returns the values of the target, or `None` if the deck does not know
    /// them.
    fn values(&self) -> Option<Cow<'a, Values>> {
        match self {
            Self::Preset { values, .. } => values.map(Cow::Borrowed),
            Self::Cam(value) => Some(Cow::Owned(Values {
                cam: Some(value.clone()),
                mic: None,
            })),
            Self::Mic(mute) => Some(Cow::Owned(Values {
                cam: None,
                mic: Some(Value::mic(*mute)),
            })),
        }
    }

    fn command(self) -> Command {
        match self {
            Self::Preset { name, .. } => Command::ApplyPreset(name.to_owned()),
            Self::Cam(value) => Command::SetCam(value),
            Self::Mic(mute) => Command::SetMic(mute),
        }
    }
}

/// When the commands of a key press act.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Timing {
    /// At once, on the press.
    Press,
    /// On the release, after a check with the status of that moment.
    Release,
}

/// Returns the commands of a press of a key with `action`, and when they act.
/// A press that raises a plane acts on the release, and every other press at
/// once. Returns `None` if the press does nothing.
fn plan(action: &Action, model: &Model<'_>) -> Option<(Timing, Vec<Command>)> {
    if *action == Action::Resume {
        let resumed: Vec<Command> = model
            .planes
            .iter()
            .filter(|state| state.resumable)
            .map(|state| Command::Resume(state.plane))
            .collect();
        return (!resumed.is_empty()).then_some((Timing::Release, resumed));
    }
    let target = Target::of(action, model)?;
    let timing = match target.values() {
        // A preset that the deck does not know can raise a plane.
        None => Timing::Release,
        Some(values) if values.blocked_by(&model.planes).is_some() => return None,
        Some(values) if values.raise(&model.planes) => Timing::Release,
        Some(_) => Timing::Press,
    };
    Some((timing, vec![target.command()]))
}

/// Returns the plan of a press of a key with `action`, for `snapshot`.
/// Without a snapshot, a press does nothing.
fn plan_with(action: &Action, snapshot: Option<&Snapshot>) -> Option<(Timing, Vec<Command>)> {
    snapshot.and_then(|snapshot| plan(action, &Model::new(snapshot)))
}

/// The keys that wait for their release, with their action.
#[derive(Debug, Default)]
pub(crate) struct Presses {
    pending: HashMap<Key, Action>,
}

impl Presses {
    /// Returns the commands of a press of `key` with `action`. A press that
    /// raises a plane waits for its release. Without a snapshot, a press does
    /// nothing.
    pub(crate) fn press(
        &mut self,
        key: Key,
        action: &Action,
        snapshot: Option<&Snapshot>,
    ) -> Vec<Command> {
        self.pending.remove(&key);
        match plan_with(action, snapshot) {
            Some((Timing::Press, commands)) => commands,
            Some((Timing::Release, _)) => {
                self.pending.insert(key, action.clone());
                Vec::new()
            }
            None => Vec::new(),
        }
    }

    /// Returns the commands of the release of `key`. The deck checks the press
    /// again with `snapshot`. A press that is not allowed now gives no command.
    pub(crate) fn release(&mut self, key: Key, snapshot: Option<&Snapshot>) -> Vec<Command> {
        let Some(action) = self.pending.remove(&key) else {
            return Vec::new();
        };
        plan_with(&action, snapshot)
            .map(|(_, commands)| commands)
            .unwrap_or_default()
    }

    /// Drops every press that waits for its release.
    pub(crate) fn cancel(&mut self) {
        self.pending.clear();
    }
}

/// The color group of a plane: what the host can get.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaneTone {
    /// The host can get live video or audio.
    Live,
    /// The host can get a slate.
    Canned,
    /// The host gets blank or silence.
    Off,
    /// The deck does not know what the host can get.
    Unknown,
}

/// The color group of a key image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    /// A key that shows a plane, or a lit key that sets a slate or live.
    Plane(PlaneTone),
    /// A lit key that sets only privacy.
    Private,
    /// A key that is not lit, or that does not show a plane.
    Neutral,
    /// A key without an action now.
    Dark,
}

/// The content of a key image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyView {
    pub(crate) tone: Tone,
    /// The small name of the plane above the title.
    pub(crate) label: Option<&'static str>,
    pub(crate) title: String,
    pub(crate) detail: Option<String>,
}

impl KeyView {
    fn new(tone: Tone, title: impl Into<String>) -> Self {
        Self {
            tone,
            label: None,
            title: title.into(),
            detail: None,
        }
    }
}

/// The content of the info bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InfoBarView {
    pub(crate) cam: Segment,
    pub(crate) mic: Segment,
    pub(crate) bottom: String,
}

/// The state of one plane in the info bar: a colored dot and a text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) tone: PlaneTone,
    pub(crate) text: String,
}

/// Everything that the device shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Screen {
    /// The view of each key, row by row from the top left.
    pub(crate) keys: [KeyView; KEYS as usize],
    pub(crate) info_bar: InfoBarView,
    /// The tones of the touch points: the cam on the left, the mic on the
    /// right.
    pub(crate) touch_points: [PlaneTone; 2],
    /// The brightness in percent.
    pub(crate) brightness: u8,
}

/// Returns what the device shows with `snapshot` at `now_usec`, in
/// `CLOCK_MONOTONIC` microseconds, after `idle` without a press.
pub(crate) fn screen(
    config: &Config,
    snapshot: Option<&Snapshot>,
    now_usec: i64,
    idle: Duration,
) -> Screen {
    let model = snapshot.map(Model::new);
    let model = model.as_ref();
    let tone = |plane| model.map_or(PlaneTone::Unknown, |model| model.planes[plane].tone());
    let dim = config.dim_after.is_some_and(|after| idle >= after)
        && model.is_some_and(|model| {
            model
                .planes
                .iter()
                .all(|state| matches!(state.exposure, Some(Exposure::Off | Exposure::Slate)))
        });
    Screen {
        keys: Key::all().map(|key| key_view(config.action(key), model)),
        info_bar: info_bar_view(model, now_usec),
        touch_points: [tone(Plane::Cam), tone(Plane::Mic)],
        brightness: if dim {
            config.dim_brightness
        } else {
            config.brightness
        },
    }
}

/// Returns the view of a key with `action`. Without a model, every key shows
/// `OFFLINE`.
fn key_view(action: &Action, model: Option<&Model<'_>>) -> KeyView {
    let Some(model) = model else {
        return KeyView::new(Tone::Plane(PlaneTone::Unknown), "OFFLINE");
    };
    let title = match action {
        Action::Preset(name) => name.to_uppercase(),
        Action::Cam(Value::Off) => "BLANK".to_owned(),
        Action::Cam(value) => value.as_cam().to_uppercase(),
        Action::Mic(Mute::Muted) => "MUTE".to_owned(),
        Action::Mic(Mute::Unmuted) => "UNMUTE".to_owned(),
        Action::Toggle(plane) => return plane_key(&model.planes[*plane]),
        Action::Resume => return resume_key(&model.planes),
        Action::Nothing => return KeyView::new(Tone::Dark, ""),
    };
    match Target::of(action, model).and_then(|target| target.values()) {
        Some(values) => preset_key(title, &values, &model.planes),
        None => KeyView::new(Tone::Neutral, title),
    }
}

/// Returns the view of a plane key.
fn plane_key(state: &PlaneView<'_>) -> KeyView {
    KeyView {
        tone: Tone::Plane(state.tone()),
        label: Some(label(state.plane)),
        title: state.word(),
        detail: state.line(),
    }
}

/// Returns the view of a key that sets `values`, with the title `title`. The
/// first row that applies of the table in `docs/deck.md` (Preset keys) gives
/// the view.
fn preset_key(title: String, values: &Values, planes: &Planes<'_>) -> KeyView {
    let mut view = KeyView::new(Tone::Neutral, title);
    if values.lit(planes) {
        view.tone = values.tone();
    } else if let Some(blocking) = values.blocked_by(planes) {
        view.detail = blocking.line();
    } else if values
        .planes()
        .all(|(plane, value)| planes[plane].requested == *value)
    {
        view.detail = values.planes().find_map(|(plane, _)| {
            let state = &planes[plane];
            match state.exposure {
                None => Some("?".to_owned()),
                Some(_) => state.why.and_then(|_| state.line()),
            }
        });
    }
    view
}

/// Returns the view of the resume key. With a hold, the key shows the requests
/// that a press applies again. The key is dark unless a plane is resumable.
fn resume_key(planes: &Planes<'_>) -> KeyView {
    let held: Vec<&PlaneView<'_>> = planes.iter().filter(|state| state.held()).collect();
    if held.is_empty() {
        return KeyView::new(Tone::Dark, "RESUME");
    }
    let requests: Vec<String> = held
        .iter()
        .map(|state| {
            let value = match &state.requested {
                Value::Off => "off",
                Value::Slate(name) => name,
                Value::Live => "live",
            };
            format!("{} {value}", label(state.plane).to_lowercase())
        })
        .collect();
    let tone = if held.iter().any(|state| state.resumable) {
        Tone::Neutral
    } else {
        Tone::Dark
    };
    KeyView {
        detail: Some(requests.join(", ")),
        ..KeyView::new(tone, "RESUME")
    }
}

/// Returns the view of the info bar at `now_usec`, in `CLOCK_MONOTONIC`
/// microseconds.
fn info_bar_view(model: Option<&Model<'_>>, now_usec: i64) -> InfoBarView {
    let Some(model) = model else {
        let unknown = |plane| Segment {
            tone: PlaneTone::Unknown,
            text: format!("{} ?", label(plane)),
        };
        return InfoBarView {
            cam: unknown(Plane::Cam),
            mic: unknown(Plane::Mic),
            bottom: "no controller".to_owned(),
        };
    };
    let segment = |state: &PlaneView<'_>| Segment {
        tone: state.tone(),
        text: format!("{} {}", label(state.plane), state.word()),
    };
    InfoBarView {
        cam: segment(&model.planes.cam),
        mic: segment(&model.planes.mic),
        bottom: bottom_line(model, now_usec),
    }
}

/// Returns the second line of the info bar. The line shows the first that
/// applies: the worst condition, a hold, a cam without a stream, an idle mic,
/// the last preset if its key is lit, or `ready`.
fn bottom_line(model: &Model<'_>, now_usec: i64) -> String {
    let Model {
        status,
        presets,
        planes,
    } = model;
    if status.health != Health::Ready {
        return worst_condition(status, now_usec);
    }
    if let Some(held) = planes.iter().find(|state| state.held()) {
        return held.line().unwrap_or_default().to_lowercase();
    }
    let cam = &planes.cam;
    if cam.flow == Some(Flow::Idle) && cam.stream == Some(CamStream::Idle) {
        return match cam.blank_at_usec {
            Some(blank_at) => format!(
                "no stream, blank in {}s",
                (blank_at - now_usec).max(0) / 1_000_000
            ),
            None => "no stream".to_owned(),
        };
    }
    if planes.mic.flow == Some(Flow::Idle) {
        return "mic idle".to_owned();
    }
    status
        .preset
        .as_ref()
        .filter(|name| presets.get(*name).is_some_and(|values| values.lit(planes)))
        .cloned()
        .unwrap_or_else(|| "ready".to_owned())
}

/// Returns the worst condition with its age, for a status whose health is not
/// ready: a lost owner, or else the worst device.
fn worst_condition(status: &Status, now_usec: i64) -> String {
    if let Some(owner) = status
        .owners
        .iter()
        .find(|owner| owner.report == ReportState::Lost)
    {
        return format!(
            "{}: {} lost {}",
            status.health,
            words(owner.owner),
            age(now_usec - owner.since_usec)
        );
    }
    let worst = status
        .owners
        .iter()
        .flat_map(|owner| &owner.devices)
        .filter(|device| device.status != DeviceStatus::Ready)
        .max_by_key(|device| device.status);
    match worst {
        Some(device) => format!(
            "{}: {} {} {}",
            status.health,
            words(device.device),
            device.status,
            age(now_usec - device.since_usec)
        ),
        None => status.health.to_string(),
    }
}

/// Returns the name of `value` with spaces between the words.
fn words(value: impl fmt::Display) -> String {
    value.to_string().replace('_', " ")
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use presencemux_api::{
        CamStatus, Device, DeviceCondition, Host, MicOutput, MicStatus, Owner, OwnerStatus,
        Permission,
    };

    use super::*;
    use crate::config::DEFAULT_KEYS;

    /// The time of the tests, in `CLOCK_MONOTONIC` microseconds.
    const NOW: i64 = 1_000_000_000;

    fn key(index: u8) -> Key {
        Key::new(index).unwrap()
    }

    /// Returns the presets of the default configuration.
    fn presets() -> Presets {
        let preset = |cam, mic| Values {
            cam: Some(cam),
            mic: Some(mic),
        };
        let slate = |name: &str| Value::Slate(name.to_owned());
        Presets::from([
            ("privacy".to_owned(), preset(Value::Off, Value::Off)),
            ("live".to_owned(), preset(Value::Live, Value::Live)),
            (
                "still-here".to_owned(),
                preset(slate("still-here"), Value::Live),
            ),
            ("brb".to_owned(), preset(slate("brb"), Value::Off)),
            ("away".to_owned(), preset(slate("away"), Value::Off)),
        ])
    }

    /// Returns a configuration with `action` on key 0 and no action on the
    /// other keys.
    fn one_key(action: &str) -> Config {
        let none = r#", "none""#.repeat(7);
        Config::parse(&format!("[deck]\nkeys = [\"{action}\"{none}]\n")).unwrap()
    }

    fn device(device: Device, status: DeviceStatus) -> DeviceCondition {
        DeviceCondition {
            device,
            status,
            since_usec: NOW - 180_000_000,
        }
    }

    /// Returns the snapshot of privacy: both planes off as requested, a
    /// connected host, and fresh owners with ready devices.
    fn privacy() -> Snapshot {
        Snapshot {
            status: Status {
                cam: CamStatus {
                    requested: "blank".to_owned(),
                    granted: "blank".to_owned(),
                    actual: Some("blank".to_owned()),
                    exposure: Some(Exposure::Off),
                    slate: None,
                    flow: Some(Flow::Flowing),
                    reason: None,
                    hold: None,
                    resumable: false,
                    stream: CamStream::Streaming,
                    blank_at_usec: None,
                },
                mic: MicStatus {
                    requested: Mute::Muted,
                    granted: Permission::Denied,
                    actual: Some(MicOutput::Silent),
                    exposure: Some(Exposure::Off),
                    flow: Some(Flow::Flowing),
                    reason: None,
                    hold: None,
                    resumable: false,
                },
                owners: vec![
                    OwnerStatus {
                        owner: Owner::CamWorker,
                        report: ReportState::Fresh,
                        since_usec: 0,
                        devices: vec![
                            device(Device::CamWorker, DeviceStatus::Ready),
                            device(Device::Camera, DeviceStatus::Ready),
                        ],
                        message: None,
                    },
                    OwnerStatus {
                        owner: Owner::AudioWorker,
                        report: ReportState::Fresh,
                        since_usec: 0,
                        devices: vec![
                            device(Device::Gate, DeviceStatus::Ready),
                            device(Device::AudioInterface, DeviceStatus::Ready),
                        ],
                        message: None,
                    },
                ],
                host: Host::Connected,
                health: Health::Ready,
                preset: Some("privacy".to_owned()),
            },
            presets: Arc::new(presets()),
        }
    }

    /// Returns `snapshot` with the cam requested and granted at `value`, the
    /// actual output `actual`, and the exposure `exposure` with `flow`. A slate
    /// exposure has the name `value`.
    fn cam(
        mut snapshot: Snapshot,
        value: &str,
        actual: &str,
        exposure: Exposure,
        flow: Flow,
    ) -> Snapshot {
        let cam = &mut snapshot.status.cam;
        cam.requested = value.to_owned();
        cam.granted = value.to_owned();
        cam.actual = Some(actual.to_owned());
        cam.exposure = Some(exposure);
        cam.slate = (exposure == Exposure::Slate).then(|| value.to_owned());
        cam.flow = Some(flow);
        snapshot
    }

    /// Returns `snapshot` with the mic requested and granted unmuted, the
    /// actual output `actual`, and a live exposure with `flow`.
    fn mic_live(mut snapshot: Snapshot, actual: MicOutput, flow: Flow) -> Snapshot {
        let mic = &mut snapshot.status.mic;
        mic.requested = Mute::Unmuted;
        mic.granted = Permission::Permitted;
        mic.actual = Some(actual);
        mic.exposure = Some(Exposure::Live);
        mic.flow = Some(flow);
        snapshot
    }

    /// Returns the snapshot of the live preset in a call: both planes live
    /// and flowing.
    fn live() -> Snapshot {
        let mut snapshot = mic_live(
            cam(privacy(), "live", "live", Exposure::Live, Flow::Flowing),
            MicOutput::Passing,
            Flow::Flowing,
        );
        snapshot.status.preset = Some("live".to_owned());
        snapshot
    }

    /// Returns `snapshot` with the mic held by `cause` while unmuted is
    /// requested, and resumable as `resumable`.
    fn mic_held(mut snapshot: Snapshot, cause: Cause, resumable: bool) -> Snapshot {
        let mic = &mut snapshot.status.mic;
        mic.requested = Mute::Unmuted;
        mic.reason = Some(Reason::Held);
        mic.hold = Some(cause);
        mic.resumable = resumable;
        snapshot
    }

    /// Returns `snapshot` with the cam held by `cause` while live is requested.
    fn cam_held(mut snapshot: Snapshot, cause: Cause) -> Snapshot {
        let cam = &mut snapshot.status.cam;
        cam.requested = "live".to_owned();
        cam.reason = Some(Reason::Held);
        cam.hold = Some(cause);
        snapshot
    }

    /// Returns the scenarios of `docs/deck.md` (Model, Scenarios).
    fn scenarios() -> Vec<(&'static str, Option<Snapshot>)> {
        let mut no_stream = cam(live(), "live", "nothing", Exposure::Live, Flow::Idle);
        no_stream.status.cam.stream = CamStream::Idle;
        no_stream.status.cam.blank_at_usec = Some(NOW + 18_000_000);
        no_stream.status.mic.actual = Some(MicOutput::Silent);
        no_stream.status.mic.flow = Some(Flow::Idle);
        let mut timeout = cam_held(
            mic_live(privacy(), MicOutput::Silent, Flow::Idle),
            Cause::StreamStopped,
        );
        timeout.status.cam.stream = CamStream::Idle;
        let mut fault = cam_held(privacy(), Cause::CameraFault);
        fault.status.health = Health::Fault;
        fault.status.owners[0].devices[1].status = DeviceStatus::Fault;
        let mut back = cam_held(privacy(), Cause::CameraFault);
        back.status.cam.resumable = true;
        let mut updating = live();
        updating.status.cam.exposure = Some(Exposure::Off);
        updating.status.cam.actual = Some("blank".to_owned());
        updating.status.cam.reason = Some(Reason::Updating);
        let mut latched = cam(privacy(), "live", "blank", Exposure::Off, Flow::Flowing);
        latched.status.cam.reason = Some(Reason::Blocked);
        let mut waiting = privacy();
        waiting.status.cam.requested = "live".to_owned();
        waiting.status.cam.reason = Some(Reason::Waiting);
        waiting.status.health = Health::Degraded;
        waiting.status.owners[0].devices[1].status = DeviceStatus::Missing;
        let mut brb = cam(privacy(), "brb", "brb", Exposure::Slate, Flow::Flowing);
        brb.status.preset = Some("brb".to_owned());
        let mut brb_idle = cam(brb.clone(), "brb", "nothing", Exposure::Slate, Flow::Idle);
        brb_idle.status.cam.stream = CamStream::Idle;
        brb_idle.status.cam.blank_at_usec = Some(NOW + 5_000_000);
        let mut lost = cam_held(privacy(), Cause::CamWorkerLost);
        lost.status.cam.actual = None;
        lost.status.cam.exposure = None;
        lost.status.cam.flow = None;
        lost.status.health = Health::Fault;
        lost.status.owners[0].report = ReportState::Lost;
        lost.status.owners[0].since_usec = NOW - 5_000_000;
        lost.status.owners[0].devices[0].status = DeviceStatus::Unknown;
        lost.status.owners[0].devices[1].status = DeviceStatus::Unknown;
        vec![
            ("privacy", Some(privacy())),
            ("live in a call", Some(live())),
            (
                "mic idle",
                Some(mic_live(
                    cam(privacy(), "live", "live", Exposure::Live, Flow::Flowing),
                    MicOutput::Silent,
                    Flow::Idle,
                )),
            ),
            ("no stream", Some(no_stream)),
            ("stream timeout", Some(timeout)),
            ("camera fault", Some(fault)),
            ("camera back", Some(back)),
            ("updating", Some(updating)),
            ("latched", Some(latched)),
            ("waiting", Some(waiting)),
            ("brb", Some(brb)),
            ("brb without a stream", Some(brb_idle)),
            (
                "mixed",
                Some(cam(
                    privacy(),
                    "live",
                    "live",
                    Exposure::Live,
                    Flow::Flowing,
                )),
            ),
            ("lost owner", Some(lost)),
            ("offline", None),
        ]
    }

    /// Returns the press of a key with `action` as text: when it acts, and its
    /// commands.
    fn press(action: &Action, snapshot: Option<&Snapshot>) -> String {
        match plan_with(action, snapshot) {
            None => "nothing".to_owned(),
            Some((timing, commands)) => format!("{timing:?}: {commands:?}"),
        }
    }

    #[test]
    fn scenarios_match_the_model() {
        let config = Config::parse("").unwrap();
        let mut table = String::new();
        for (name, snapshot) in scenarios() {
            let snapshot = snapshot.as_ref();
            writeln!(table, "## {name}").unwrap();
            let screen = screen(&config, snapshot, NOW, Duration::ZERO);
            for ((name, key), view) in DEFAULT_KEYS.iter().zip(Key::all()).zip(screen.keys) {
                writeln!(
                    table,
                    "{name:18} {:16} {:11} {:24} press {}",
                    format!("{:?}", view.tone),
                    view.title,
                    view.detail.unwrap_or_default(),
                    press(config.action(key), snapshot)
                )
                .unwrap();
            }
            let bar = screen.info_bar;
            writeln!(
                table,
                "touch {:?} | bar {} / {} / {}\n",
                screen.touch_points, bar.cam.text, bar.mic.text, bar.bottom
            )
            .unwrap();
        }
        insta::assert_snapshot!(table);
    }

    #[test]
    fn preset_action_names_its_preset() {
        assert_eq!("preset:brb".parse(), Ok(Action::Preset("brb".to_owned())));
    }

    #[test]
    fn preset_action_without_a_name_is_unknown() {
        assert_eq!(
            "preset:".parse::<Action>(),
            Err(UnknownAction("preset:".to_owned()))
        );
    }

    #[test]
    fn raise_acts_on_the_release() {
        let mut presses = Presses::default();
        let live = Action::Preset("live".to_owned());
        let _ = presses.press(key(1), &live, Some(&privacy()));

        let commands = presses.release(key(1), Some(&privacy()));

        assert_eq!(commands, [Command::ApplyPreset("live".to_owned())]);
    }

    #[test]
    fn raise_does_nothing_on_the_press() {
        let mut presses = Presses::default();

        let commands = presses.press(key(1), &Action::Preset("live".to_owned()), Some(&privacy()));

        assert_eq!(commands, []);
    }

    #[test]
    fn hold_between_press_and_release_drops_the_raise() {
        let mut presses = Presses::default();
        let live = Action::Preset("live".to_owned());
        let _ = presses.press(key(1), &live, Some(&privacy()));

        let commands = presses.release(key(1), Some(&cam_held(privacy(), Cause::CameraFault)));

        assert_eq!(commands, []);
    }

    #[test]
    fn cancel_drops_a_waiting_press() {
        let mut presses = Presses::default();
        let _ = presses.press(key(1), &Action::Preset("live".to_owned()), Some(&privacy()));
        presses.cancel();

        let commands = presses.release(key(1), Some(&privacy()));

        assert_eq!(commands, []);
    }

    #[test]
    fn release_without_a_snapshot_gives_no_command() {
        let mut presses = Presses::default();
        let _ = presses.press(key(1), &Action::Preset("live".to_owned()), Some(&privacy()));

        let commands = presses.release(key(1), None);

        assert_eq!(commands, []);
    }

    #[test]
    fn release_of_another_key_gives_no_command() {
        let mut presses = Presses::default();
        let _ = presses.press(key(1), &Action::Preset("live".to_owned()), Some(&privacy()));

        let commands = presses.release(key(2), Some(&privacy()));

        assert_eq!(commands, []);
    }

    #[test]
    fn second_press_of_a_key_replaces_its_waiting_action() {
        let mut presses = Presses::default();
        let _ = presses.press(key(1), &Action::Preset("live".to_owned()), Some(&privacy()));
        let _ = presses.press(key(1), &Action::Preset("brb".to_owned()), Some(&privacy()));

        let commands = presses.release(key(1), Some(&privacy()));

        assert_eq!(commands, [Command::ApplyPreset("brb".to_owned())]);
    }

    #[test]
    fn press_without_a_snapshot_gives_no_command() {
        let mut presses = Presses::default();

        let commands = presses.press(key(0), &Action::Preset("privacy".to_owned()), None);

        assert_eq!(commands, []);
    }

    #[test]
    fn lower_acts_on_the_press() {
        let mut presses = Presses::default();

        let commands = presses.press(key(0), &Action::Preset("privacy".to_owned()), Some(&live()));

        assert_eq!(commands, [Command::ApplyPreset("privacy".to_owned())]);
    }

    #[test]
    fn mic_toggle_of_a_live_mic_mutes_on_the_press() {
        let mut presses = Presses::default();

        let commands = presses.press(key(2), &Action::Toggle(Plane::Mic), Some(&live()));

        assert_eq!(commands, [Command::SetMic(Mute::Muted)]);
    }

    #[test]
    fn unknown_preset_acts_on_the_release() {
        let mut presses = Presses::default();
        let other = Action::Preset("other".to_owned());
        let at_press = presses.press(key(1), &other, Some(&privacy()));

        let at_release = presses.release(key(1), Some(&privacy()));

        assert_eq!(
            (at_press, at_release),
            (vec![], vec![Command::ApplyPreset("other".to_owned())])
        );
    }

    #[test]
    fn slate_during_a_cam_hold_acts_on_the_release() {
        let mut presses = Presses::default();
        let brb = Action::Preset("brb".to_owned());
        let held = cam_held(privacy(), Cause::CameraFault);
        let _ = presses.press(key(5), &brb, Some(&held));

        let commands = presses.release(key(5), Some(&held));

        assert_eq!(commands, [Command::ApplyPreset("brb".to_owned())]);
    }

    #[test]
    fn live_preset_on_a_held_cam_gives_no_command() {
        let mut presses = Presses::default();
        let held = cam_held(privacy(), Cause::CameraFault);
        let at_press = presses.press(key(1), &Action::Preset("live".to_owned()), Some(&held));

        let at_release = presses.release(key(1), Some(&held));

        assert_eq!((at_press, at_release), (vec![], vec![]));
    }

    #[test]
    fn cam_toggle_on_a_held_cam_gives_no_command() {
        let mut presses = Presses::default();
        let held = cam_held(privacy(), Cause::CameraFault);
        let at_press = presses.press(key(3), &Action::Toggle(Plane::Cam), Some(&held));

        let at_release = presses.release(key(3), Some(&held));

        assert_eq!((at_press, at_release), (vec![], vec![]));
    }

    #[test]
    fn cam_live_action_on_a_held_cam_gives_no_command() {
        let mut presses = Presses::default();
        let held = cam_held(privacy(), Cause::CameraFault);
        let at_press = presses.press(key(3), &Action::Cam(Value::Live), Some(&held));

        let at_release = presses.release(key(3), Some(&held));

        assert_eq!((at_press, at_release), (vec![], vec![]));
    }

    #[test]
    fn live_request_again_while_blocked_acts_on_the_release() {
        let mut presses = Presses::default();
        let mut blocked = cam(privacy(), "live", "blank", Exposure::Off, Flow::Flowing);
        blocked.status.cam.reason = Some(Reason::Blocked);
        let at_press = presses.press(key(3), &Action::Cam(Value::Live), Some(&blocked));

        let at_release = presses.release(key(3), Some(&blocked));

        assert_eq!(
            (at_press, at_release),
            (vec![], vec![Command::SetCam(Value::Live)])
        );
    }

    #[test]
    fn resume_acts_on_the_release_for_each_resumable_plane() {
        let mut presses = Presses::default();
        let held = mic_held(
            cam_held(privacy(), Cause::CameraFault),
            Cause::AudioInterfaceFault,
            true,
        );
        let at_press = presses.press(key(7), &Action::Resume, Some(&held));

        let at_release = presses.release(key(7), Some(&held));

        assert_eq!(
            (at_press, at_release),
            (vec![], vec![Command::Resume(Plane::Mic)])
        );
    }

    #[test]
    fn resume_that_is_not_resumable_at_the_release_gives_no_command() {
        let mut presses = Presses::default();
        let resumable = mic_held(privacy(), Cause::AudioInterfaceFault, true);
        let _ = presses.press(key(7), &Action::Resume, Some(&resumable));

        let commands = presses.release(
            key(7),
            Some(&mic_held(privacy(), Cause::AudioInterfaceFault, false)),
        );

        assert_eq!(commands, []);
    }

    #[test]
    fn resume_key_with_one_resumable_plane_is_not_dark() {
        let held = mic_held(
            cam_held(privacy(), Cause::CameraFault),
            Cause::AudioInterfaceFault,
            true,
        );

        let screen = screen(&one_key("resume"), Some(&held), NOW, Duration::ZERO);

        assert_eq!(screen.keys[0].tone, Tone::Neutral);
    }

    #[test]
    fn lit_live_preset_is_red() {
        let screen = screen(&one_key("preset:live"), Some(&live()), NOW, Duration::ZERO);

        assert_eq!(screen.keys[0].tone, Tone::Plane(PlaneTone::Live));
    }

    #[test]
    fn lit_slate_preset_is_amber() {
        let brb = cam(privacy(), "brb", "brb", Exposure::Slate, Flow::Flowing);

        let screen = screen(&one_key("preset:brb"), Some(&brb), NOW, Duration::ZERO);

        assert_eq!(screen.keys[0].tone, Tone::Plane(PlaneTone::Canned));
    }

    #[test]
    fn lit_privacy_preset_is_private() {
        let screen = screen(
            &one_key("preset:privacy"),
            Some(&privacy()),
            NOW,
            Duration::ZERO,
        );

        assert_eq!(screen.keys[0].tone, Tone::Private);
    }

    #[test]
    fn touch_points_show_a_slate_cam_and_an_off_mic() {
        let brb = cam(privacy(), "brb", "brb", Exposure::Slate, Flow::Flowing);

        let screen = screen(&one_key("none"), Some(&brb), NOW, Duration::ZERO);

        assert_eq!(screen.touch_points, [PlaneTone::Canned, PlaneTone::Off]);
    }

    #[test]
    fn deck_without_live_dims_after_the_dim_time() {
        let config = Config::parse("[deck]\ndim_after = 300\ndim_brightness = 10\n").unwrap();

        let screen = screen(&config, Some(&privacy()), NOW, Duration::from_secs(300));

        assert_eq!(screen.brightness, 10);
    }

    #[test]
    fn deck_stays_bright_just_before_the_dim_time() {
        let config = Config::parse("[deck]\nbrightness = 60\ndim_after = 300\n").unwrap();

        let screen = screen(&config, Some(&privacy()), NOW, Duration::from_secs(299));

        assert_eq!(screen.brightness, 60);
    }

    #[test]
    fn live_exposure_keeps_the_deck_bright() {
        let config = Config::parse("[deck]\nbrightness = 60\ndim_after = 300\n").unwrap();

        let screen = screen(&config, Some(&live()), NOW, Duration::from_secs(300));

        assert_eq!(screen.brightness, 60);
    }

    #[test]
    fn offline_deck_stays_bright() {
        let config = Config::parse("[deck]\nbrightness = 60\ndim_after = 300\n").unwrap();

        let screen = screen(&config, None, NOW, Duration::from_secs(300));

        assert_eq!(screen.brightness, 60);
    }

    #[test]
    fn info_bar_shows_the_worst_device() {
        let mut snapshot = privacy();
        snapshot.status.health = Health::Fault;
        snapshot.status.owners[0].devices[1].status = DeviceStatus::Missing;
        snapshot.status.owners[1].devices[0].status = DeviceStatus::Fault;
        snapshot.status.owners[1].devices[1].status = DeviceStatus::Initializing;
        let config = Config::parse("").unwrap();

        let screen = screen(&config, Some(&snapshot), NOW, Duration::ZERO);

        assert_eq!(screen.info_bar.bottom, "fault: gate fault 3m");
    }
}
