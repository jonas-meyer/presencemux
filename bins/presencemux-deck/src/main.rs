//! The owner of the Stream Deck Neo. It draws the status of presencemuxd on
//! the deck and sends the key presses to presencemuxd.
//!
//! `docs/deck.md` holds the design.

mod client;
mod config;
mod device;
mod discovery;
mod hidraw;
mod protocol;
mod render;
mod systemd;
mod view;

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use presencemux_api::SOCKET_PATH;
use rustix::time::{ClockId, clock_gettime};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::{self, MissedTickBehavior};
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::config::{CONFIG, Config};
use crate::device::{Device, DeviceError, Sink, Write};
use crate::discovery::Discovery;
use crate::protocol::{Change, Input, Key, OutputReport, Pressed, Region, Side};
use crate::render::Fonts;
use crate::view::{Command, InfoBarView, KeyView, Presses, Screen, Snapshot};

/// The sleep timer of the device. The deck arms it again at each [`TICK`]. If
/// the process stops, the device goes dark after this time.
const DEAD_MAN: Duration = Duration::from_secs(5);

/// The period of the sleep timer, the watchdog and the ages in the info bar.
const TICK: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("cannot register the signal handlers: {0}")]
    Signal(io::Error),
    #[error("{0}")]
    Config(#[from] config::ConfigError),
    #[error("{0}")]
    Font(#[from] render::FontError),
    #[error("cannot start the udev monitor: {0}")]
    Udev(io::Error),
    #[error("cannot read the udev events: {0}")]
    UdevEvents(io::Error),
    #[error("cannot reset the deck: {0}")]
    Reset(io::Error),
}

#[tokio::main(flavor = "local")]
async fn main() -> ExitCode {
    init_logging();
    let result = if std::env::args().nth(1).as_deref() == Some("--reset") {
        reset().map_err(Error::Reset)
    } else {
        run().await
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("presencemux-deck failed: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Resets the deck, if it exists. The deck then shows its own screen.
fn reset() -> io::Result<()> {
    let Some(path) = discovery::find()? else {
        return Ok(());
    };
    let fd = hidraw::open(&path)?;
    hidraw::set_feature(&fd, &protocol::reset())?;
    tracing::info!("reset the deck at {}", path.display());
    Ok(())
}

async fn run() -> Result<(), Error> {
    let mut terminate = signal(SignalKind::terminate()).map_err(Error::Signal)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(Error::Signal)?;
    let config = Config::read(Path::new(CONFIG))?;
    let fonts = Fonts::load(Path::new(render::FONT_DIR))?;
    let mut discovery = Discovery::new().map_err(Error::Udev)?;
    let (mut statuses, commands) = client::start(PathBuf::from(SOCKET_PATH));
    let mut deck = Deck::new(config, fonts, commands);
    deck.sync(discovery.node());
    let mut watchdog = systemd::Watchdog::new();
    let mut tick = time::interval(TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    systemd::ready();
    tracing::info!("presencemux-deck started");
    loop {
        tokio::select! {
            biased;
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            changed = discovery.changed() => {
                // A udev error repeats at once. The process ends, and systemd
                // starts it again.
                changed.map_err(Error::UdevEvents)?;
                deck.sync(discovery.node());
                deck.draw(statuses.borrow().as_ref());
            }
            pressed = next_input(&mut deck.link) => match pressed {
                Ok(pressed) => deck.input(pressed, statuses.borrow().as_ref()),
                Err(error) => deck.lose(&error),
            },
            changed = statuses.changed() => {
                if changed.is_err() {
                    break;
                }
                deck.status(statuses.borrow_and_update().as_ref());
            }
            _ = tick.tick() => {
                watchdog.ping();
                deck.sync(discovery.node());
                deck.tick(statuses.borrow().as_ref());
            }
        }
    }
    systemd::stopping();
    tracing::info!("presencemux-deck stopped");
    Ok(())
}

/// Waits for the next input report of the open device. Without an open
/// device, waits forever.
async fn next_input(link: &mut Link) -> Result<Pressed, DeviceError> {
    match link {
        Link::Open(session) => session.device.next().await,
        Link::Absent | Link::Failed => std::future::pending().await,
    }
}

/// The state of the deck. It writes to the device through `D`.
struct Deck<D = Device> {
    config: Config,
    fonts: Fonts,
    commands: mpsc::Sender<Command>,
    link: Link<D>,
}

/// The connection of the deck to the device.
enum Link<D = Device> {
    /// udev shows no node.
    Absent,
    /// The node exists, but the last open failed or the device was lost. The
    /// deck tries to open the node again at each tick.
    Failed,
    Open(Box<Session<D>>),
}

/// An open device and what the deck knows about it. A new device starts a new
/// session without the state of the old device.
struct Session<D = Device> {
    path: PathBuf,
    device: D,
    /// What the device shows. `None` if the screen is unknown, for example
    /// before the first draw.
    shown: Option<Screen>,
    /// The inputs that the last input report showed as pressed.
    pressed: Pressed,
    presses: Presses,
    last_press: Instant,
}

/// An error that stops the deck from drawing an image.
#[derive(Debug, thiserror::Error)]
enum DrawError {
    #[error("cannot encode the image: {0}")]
    Encode(#[from] render::EncodeError),
    #[error("cannot write the image: {0}")]
    TooLarge(#[from] protocol::ImageTooLarge),
}

impl Deck {
    fn new(config: Config, fonts: Fonts, commands: mpsc::Sender<Command>) -> Self {
        Self {
            config,
            fonts,
            commands,
            link: Link::Absent,
        }
    }

    /// Opens the device at `node` if the deck has no open device for it.
    /// Closes the device if `node` is `None`.
    fn sync(&mut self, node: Option<&Path>) {
        let link = std::mem::replace(&mut self.link, Link::Absent);
        self.link = match (link, node) {
            (_, None) => Link::Absent,
            (Link::Open(session), Some(path)) if session.path == path => Link::Open(session),
            (link, Some(path)) => match Session::open(path) {
                Ok(session) => {
                    tracing::info!("opened the deck at {}", path.display());
                    Link::Open(Box::new(session))
                }
                Err(error) => {
                    // A failed open repeats at each tick. The deck logs only the
                    // first failure.
                    if !matches!(link, Link::Failed) {
                        tracing::warn!("{error} at {}", path.display());
                    }
                    Link::Failed
                }
            },
        };
    }
}

impl<D: Sink> Deck<D> {
    /// Closes the device after `error`. The next tick opens it again.
    fn lose(&mut self, error: &DeviceError) {
        tracing::warn!("lost the deck: {error}");
        self.link = Link::Failed;
    }

    /// Handles a new status, or the loss of the status.
    fn status(&mut self, snapshot: Option<&Snapshot>) {
        if let (None, Link::Open(session)) = (snapshot, &mut self.link) {
            session.presses.cancel();
        }
        self.draw(snapshot);
    }

    /// Handles the input report that shows `pressed`.
    fn input(&mut self, pressed: Pressed, snapshot: Option<&Snapshot>) {
        let Link::Open(session) = &mut self.link else {
            return;
        };
        for command in session.input(pressed, &self.config, snapshot) {
            if self.commands.try_send(command).is_err() {
                tracing::warn!("presencemuxd takes no more commands, the deck drops a key press");
            }
        }
        self.draw(snapshot);
    }

    /// Arms the sleep timer again, and draws the ages and the brightness.
    fn tick(&mut self, snapshot: Option<&Snapshot>) {
        let Link::Open(session) = &self.link else {
            return;
        };
        match session.arm() {
            Ok(()) => self.draw(snapshot),
            Err(error) => self.lose(&error),
        }
    }

    /// Draws each part of the screen that changed.
    fn draw(&mut self, snapshot: Option<&Snapshot>) {
        let Link::Open(session) = &mut self.link else {
            return;
        };
        let screen = view::screen(
            &self.config,
            snapshot,
            now_usec(),
            session.last_press.elapsed(),
        );
        if let Err(error) = session.show(screen, &self.fonts) {
            self.lose(&error);
        }
    }
}

impl Session {
    /// Opens the device at `path`, and arms its sleep timer.
    fn open(path: &Path) -> Result<Self, DeviceError> {
        Self::new(path, Device::open(path).map_err(DeviceError::Open)?)
    }
}

impl<D: Sink> Session<D> {
    /// Starts a session of the device at `path` that takes the writes of
    /// `device`, and arms its sleep timer.
    fn new(path: &Path, device: D) -> Result<Self, DeviceError> {
        let session = Self {
            path: path.to_owned(),
            device,
            shown: None,
            pressed: Pressed::default(),
            presses: Presses::default(),
            last_press: Instant::now(),
        };
        session.arm()?;
        Ok(session)
    }

    /// Arms the sleep timer of the device for [`DEAD_MAN`].
    fn arm(&self) -> Result<(), DeviceError> {
        self.send(Write::Feature(protocol::sleep_timer(DEAD_MAN)))
    }

    fn send(&self, write: Write) -> Result<(), DeviceError> {
        self.device.send(write)
    }

    /// Records the input report that shows `pressed`, and returns the
    /// commands of its key presses.
    fn input(
        &mut self,
        pressed: Pressed,
        config: &Config,
        snapshot: Option<&Snapshot>,
    ) -> Vec<Command> {
        let before = std::mem::replace(&mut self.pressed, pressed);
        self.last_press = Instant::now();
        before
            .changes(pressed)
            .flat_map(|change| match change {
                Change::Press(Input::Key(key)) => {
                    self.presses.press(key, config.action(key), snapshot)
                }
                Change::Release(Input::Key(key)) => self.presses.release(key, snapshot),
                Change::Press(Input::TouchPoint(_)) | Change::Release(Input::TouchPoint(_)) => {
                    Vec::new()
                }
            })
            .collect()
    }

    /// Writes each part of `next` that differs from what the device shows.
    /// After a device error, the session ends. If the deck cannot draw an
    /// image, the next draw writes every part again.
    fn show(&mut self, next: Screen, fonts: &Fonts) -> Result<(), DeviceError> {
        let shown = self.shown.take();
        let shown = shown.as_ref();
        let mut complete = true;
        for (index, key) in Key::all().into_iter().enumerate() {
            let view = &next.keys[index];
            if shown.is_none_or(|shown| shown.keys[index] != *view) {
                match key_reports(fonts, key, view) {
                    Ok(reports) => self.write_all(reports)?,
                    Err(error) => {
                        tracing::warn!("cannot draw key {}: {error}", key.index());
                        complete = false;
                    }
                }
            }
        }
        if shown.is_none_or(|shown| shown.info_bar != next.info_bar) {
            match info_bar_reports(fonts, &next.info_bar) {
                Ok(reports) => self.write_all(reports)?,
                Err(error) => {
                    tracing::warn!("cannot draw the info bar: {error}");
                    complete = false;
                }
            }
        }
        for (index, side) in [Side::Left, Side::Right].into_iter().enumerate() {
            let tone = next.touch_points[index];
            if shown.is_none_or(|shown| shown.touch_points[index] != tone) {
                let rgb = render::touch_point(tone).bytes();
                self.send(Write::Feature(protocol::fill(Input::TouchPoint(side), rgb)))?;
            }
        }
        if shown.is_none_or(|shown| shown.brightness != next.brightness) {
            self.send(Write::Feature(protocol::brightness(next.brightness)))?;
        }
        self.shown = complete.then_some(next);
        Ok(())
    }

    /// Queues each output report of `reports`.
    fn write_all(&self, reports: Vec<OutputReport>) -> Result<(), DeviceError> {
        reports
            .into_iter()
            .try_for_each(|report| self.send(Write::Output(Box::new(report))))
    }
}

/// Returns the output reports of the image of `key` with `view`.
fn key_reports(fonts: &Fonts, key: Key, view: &KeyView) -> Result<Vec<OutputReport>, DrawError> {
    let jpeg = render::key(fonts, view)?;
    Ok(protocol::image(Region::Input(Input::Key(key)), &jpeg)?)
}

/// Returns the output reports of the image of the info bar with `view`.
fn info_bar_reports(fonts: &Fonts, view: &InfoBarView) -> Result<Vec<OutputReport>, DrawError> {
    let jpeg = render::info_bar(fonts, view)?;
    Ok(protocol::image(Region::InfoBar, &jpeg)?)
}

/// Returns the time now on `CLOCK_MONOTONIC`, in microseconds.
fn now_usec() -> i64 {
    let now = clock_gettime(ClockId::Monotonic);
    now.tv_sec * 1_000_000 + now.tv_nsec / 1_000
}

fn init_logging() {
    let under_systemd = std::env::var_os("JOURNAL_STREAM").is_some_and(|value| !value.is_empty());
    let (journald, journald_error) = match under_systemd.then(tracing_journald::layer).transpose() {
        Ok(layer) => (layer, None),
        Err(error) => (None, Some(error)),
    };
    let stderr = journald
        .is_none()
        .then(|| tracing_subscriber::fmt::layer().with_writer(io::stderr));
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::registry()
        .with(journald)
        .with(stderr)
        .with(filter)
        .init();
    if let Some(error) = journald_error {
        tracing::warn!("cannot log to the journal, logs go to stderr: {error}");
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::*;

    /// A device path for the tests. The fake deck does not open it.
    const PATH: &str = "/dev/hidraw-test";

    /// A fake device that records its writes. After [`FakeDeck::unplug`],
    /// each write fails.
    #[derive(Clone, Default)]
    struct FakeDeck {
        writes: Rc<RefCell<Vec<Write>>>,
        unplugged: Rc<Cell<bool>>,
    }

    impl Sink for FakeDeck {
        fn send(&self, write: Write) -> Result<(), DeviceError> {
            if self.unplugged.get() {
                return Err(DeviceError::Stopped);
            }
            self.writes.borrow_mut().push(write);
            Ok(())
        }
    }

    impl FakeDeck {
        /// Returns one line for each part in the writes: the command and the
        /// index of an image, or the first 3 bytes of a feature report.
        fn parts(&self) -> Vec<String> {
            let mut parts: Vec<String> = self
                .writes
                .borrow()
                .iter()
                .map(|write| match write {
                    Write::Output(report) => format!("image {:#04x} {}", report[1], report[2]),
                    Write::Feature(report) => format!("feature {:02x?}", &report[..3]),
                })
                .collect();
            // An image takes one or more output reports.
            parts.dedup();
            parts
        }

        fn clear(&self) {
            self.writes.borrow_mut().clear();
        }

        fn unplug(&self) {
            self.unplugged.set(true);
        }
    }

    fn fonts() -> Fonts {
        Fonts::load(Path::new(render::FONT_DIR)).unwrap()
    }

    /// Returns the screen of the default configuration without a status.
    fn offline_screen() -> Screen {
        view::screen(&Config::parse("").unwrap(), None, 0, Duration::ZERO)
    }

    /// Returns a deck with the default configuration and `session`.
    fn deck(session: Session<FakeDeck>) -> Deck<FakeDeck> {
        let (commands, _) = mpsc::channel(1);
        Deck {
            config: Config::parse("").unwrap(),
            fonts: fonts(),
            commands,
            link: Link::Open(Box::new(session)),
        }
    }

    #[test]
    fn new_session_arms_the_sleep_timer_of_5_s() {
        let device = FakeDeck::default();

        Session::new(Path::new(PATH), device.clone()).unwrap();

        assert_eq!(device.parts(), ["feature [03, 0d, 05]"]);
    }

    #[test]
    fn tick_arms_the_sleep_timer_again() {
        let device = FakeDeck::default();
        let mut deck = deck(Session::new(Path::new(PATH), device.clone()).unwrap());
        deck.draw(None);
        device.clear();

        deck.tick(None);

        assert_eq!(device.parts(), ["feature [03, 0d, 05]"]);
    }

    #[test]
    fn first_show_writes_every_part() {
        let device = FakeDeck::default();
        let mut session = Session::new(Path::new(PATH), device.clone()).unwrap();
        device.clear();

        session.show(offline_screen(), &fonts()).unwrap();

        assert_eq!(
            device.parts(),
            [
                "image 0x07 0",
                "image 0x07 1",
                "image 0x07 2",
                "image 0x07 3",
                "image 0x07 4",
                "image 0x07 5",
                "image 0x07 6",
                "image 0x07 7",
                "image 0x0b 0",
                "feature [03, 06, 08]",
                "feature [03, 06, 09]",
                "feature [03, 08, 3c]",
            ]
        );
    }

    #[test]
    fn show_of_the_shown_screen_writes_nothing() {
        let device = FakeDeck::default();
        let fonts = fonts();
        let mut session = Session::new(Path::new(PATH), device.clone()).unwrap();
        session.show(offline_screen(), &fonts).unwrap();
        device.clear();

        session.show(offline_screen(), &fonts).unwrap();

        assert_eq!(device.parts(), Vec::<String>::new());
    }

    #[test]
    fn show_with_a_changed_key_writes_only_the_image_of_that_key() {
        let device = FakeDeck::default();
        let fonts = fonts();
        let mut session = Session::new(Path::new(PATH), device.clone()).unwrap();
        session.show(offline_screen(), &fonts).unwrap();
        device.clear();
        let mut next = offline_screen();
        next.keys[3].title = "CHANGED".to_owned();

        session.show(next, &fonts).unwrap();

        assert_eq!(device.parts(), ["image 0x07 3"]);
    }

    #[test]
    fn write_error_on_a_tick_ends_the_session() {
        let device = FakeDeck::default();
        let mut deck = deck(Session::new(Path::new(PATH), device.clone()).unwrap());
        device.unplug();

        deck.tick(None);

        assert!(matches!(deck.link, Link::Failed));
    }
}
