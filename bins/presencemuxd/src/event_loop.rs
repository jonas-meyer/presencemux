//! The event loop and the leases that it renews.

use std::sync::Arc;
use std::time::Duration;

use presencemux_api::Status;
use presencemux_core::{
    CamPermission, Command, Controller, Effects, Event, Granted, MicPermission, Observation,
    PlaneId, Preset,
};
use rustix::time::{ClockId, clock_gettime};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{self, Instant, MissedTickBehavior};

use crate::config::{Config, PresetRef};
use crate::status::status;
use crate::systemd;

const CAPACITY: usize = 64;
const TICK: Duration = Duration::from_millis(200);
const LEASE: Duration = Duration::from_secs(1);

/// A time on `CLOCK_MONOTONIC`. Other processes on the same computer read
/// the same clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Monotonic(Duration);

/// A target that the media layer may apply until `until`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Leased<T> {
    /// The start of this daemon run.
    pub(crate) session: Monotonic,
    pub(crate) granted: Granted<T>,
    pub(crate) until: Monotonic,
}

/// A request from the user.
#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) preset: Preset,
    /// The preset name for the status. `None` for a change of single planes.
    pub(crate) name: Option<PresetRef>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum HandleError {
    #[error("the event loop is busy")]
    Busy,
    #[error("the event loop has stopped")]
    Stopped,
}

/// A cloneable connection to the event loop.
#[derive(Clone, Debug)]
pub(crate) struct Handle(mpsc::Sender<Message>);

/// The receivers of the leased targets and the status.
#[derive(Debug)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "only the tests use this until the adapters exist")
)]
pub(crate) struct Outputs {
    pub(crate) cam: watch::Receiver<Leased<CamPermission>>,
    pub(crate) mic: watch::Receiver<Leased<MicPermission>>,
    pub(crate) status: watch::Receiver<Status>,
}

/// The task that owns the [`Controller`].
#[derive(Debug)]
pub(crate) struct EventLoop {
    controller: Controller,
    config: Arc<Config>,
    preset: Option<PresetRef>,
    clock: Clock,
    messages: mpsc::Receiver<Message>,
    watchdog: Option<Duration>,
    cam: watch::Sender<Leased<CamPermission>>,
    mic: watch::Sender<Leased<MicPermission>>,
    status: watch::Sender<Status>,
}

#[derive(Debug)]
enum Message {
    Request(Request, oneshot::Sender<()>),
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only the tests use this until the adapters exist")
    )]
    Observation(Observation),
}

/// A conversion from Tokio time to `CLOCK_MONOTONIC`. Tokio time follows
/// `CLOCK_MONOTONIC` on Linux.
#[derive(Debug)]
struct Clock {
    start: Monotonic,
    started: Instant,
}

impl Handle {
    /// Sends a request and waits until the event loop handles it. It
    /// returns [`HandleError::Busy`] at once if the channel is full.
    pub(crate) async fn request(&self, request: Request) -> Result<(), HandleError> {
        let (done, handled) = oneshot::channel();
        self.0
            .try_send(Message::Request(request, done))
            .map_err(|error| match error {
                TrySendError::Full(_) => HandleError::Busy,
                TrySendError::Closed(_) => HandleError::Stopped,
            })?;
        handled.await.map_err(|_| HandleError::Stopped)
    }

    /// Sends an observation. It waits while the channel is full.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only the tests use this until the adapters exist")
    )]
    pub(crate) async fn observe(&self, observation: Observation) -> Result<(), HandleError> {
        self.0
            .send(Message::Observation(observation))
            .await
            .map_err(|_| HandleError::Stopped)
    }
}

impl EventLoop {
    /// Creates the event loop at Privacy, with the boot targets leased.
    pub(crate) fn new(config: Arc<Config>) -> (Self, Handle, Outputs) {
        let (controller, _) = Controller::new();
        let clock = Clock::new();
        let (sender, messages) = mpsc::channel(CAPACITY);
        let (cam, cam_receiver) = watch::channel(clock.lease(controller.cam().target()));
        let (mic, mic_receiver) = watch::channel(clock.lease(controller.mic().target()));
        let (status, status_receiver) = watch::channel(status(&controller, &config.slates, None));
        let event_loop = Self {
            controller,
            config,
            preset: None,
            clock,
            messages,
            watchdog: None,
            cam,
            mic,
            status,
        };
        let outputs = Outputs {
            cam: cam_receiver,
            mic: mic_receiver,
            status: status_receiver,
        };
        (event_loop, Handle(sender), outputs)
    }

    /// Sets how often the tick pings the systemd watchdog. `None` turns the
    /// ping off.
    pub(crate) fn with_watchdog(mut self, period: Option<Duration>) -> Self {
        self.watchdog = period;
        self
    }

    /// Runs until no [`Handle`] remains.
    pub(crate) async fn run(mut self) {
        let mut tick = time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_tick = Instant::now();
        let mut last_ping: Option<Instant> = None;
        loop {
            tokio::select! {
                biased;
                _ = tick.tick() => {
                    let now = Instant::now();
                    if now - last_tick > LEASE {
                        self.lapse();
                    }
                    last_tick = now;
                    self.renew();
                    if let Some(period) = self.watchdog
                        && last_ping.is_none_or(|last| now - last >= period)
                    {
                        systemd::watchdog();
                        last_ping = Some(now);
                    }
                }
                message = self.messages.recv() => match message {
                    Some(message) => self.handle(message),
                    None => return,
                },
            }
        }
    }

    fn handle(&mut self, message: Message) {
        match message {
            Message::Request(Request { preset, name }, done) => {
                if let Some(name) = &name {
                    tracing::info!("applying preset {}", name.as_str());
                } else {
                    tracing::info!("applying {preset:?}");
                }
                self.preset = name;
                self.apply(Command::Apply(preset).into());
                self.publish();
                let _ = done.send(());
            }
            Message::Observation(observation) => {
                if self.apply(observation.into()) {
                    self.publish();
                }
            }
        }
    }

    /// Lowers every plane after a tick that came later than the lease length.
    fn lapse(&mut self) {
        tracing::warn!("late tick, lowering every plane");
        for plane in PlaneId::ALL {
            if self.apply(Observation::LeaseExpired(plane).into()) {
                self.publish();
            }
        }
    }

    fn renew(&self) {
        self.send_leases(self.controller.resync());
    }

    /// Gives the event to the controller and leases the new targets. Returns
    /// whether the controller changed.
    fn apply(&mut self, event: Event) -> bool {
        let effects = self.controller.handle_input(event);
        self.send_leases(effects);
        effects.status_changed
    }

    fn send_leases(&self, effects: Effects) {
        let Effects {
            cam,
            mic,
            status_changed: _,
        } = effects;
        if let Some(granted) = cam {
            self.cam.send_replace(self.clock.lease(granted));
        }
        if let Some(granted) = mic {
            self.mic.send_replace(self.clock.lease(granted));
        }
    }

    /// Sends the status if a field of it changed.
    fn publish(&self) {
        let next = status(&self.controller, &self.config.slates, self.preset.as_ref());
        self.status.send_if_modified(|current| {
            let modified = *current != next;
            if modified {
                *current = next;
            }
            modified
        });
    }
}

impl Clock {
    fn new() -> Self {
        let now = clock_gettime(ClockId::Monotonic);
        Self {
            start: Monotonic(Duration::try_from(now).expect("CLOCK_MONOTONIC is not negative")),
            started: Instant::now(),
        }
    }

    fn lease<T>(&self, granted: Granted<T>) -> Leased<T> {
        Leased {
            session: self.start,
            granted,
            until: Monotonic(self.start.0 + self.started.elapsed() + LEASE),
        }
    }
}

#[cfg(test)]
mod tests {
    use presencemux_api::{Cause, Mute};
    use presencemux_core::{Device, DeviceStatus, MicMute};

    use super::*;

    fn start(config: &str) -> (Arc<Config>, Handle, Outputs) {
        let config = Arc::new(Config::parse(config).unwrap());
        let (event_loop, handle, outputs) = EventLoop::new(Arc::clone(&config));
        tokio::spawn(event_loop.run());
        (config, handle, outputs)
    }

    async fn ready(handle: &Handle, devices: &[Device]) {
        for &device in devices {
            handle
                .observe(Observation::Device(device, DeviceStatus::Ready))
                .await
                .unwrap();
        }
    }

    fn live() -> Request {
        Request {
            preset: Preset {
                cam: Some(CamPermission::Live),
                mic: Some(MicMute::Unmuted),
            },
            name: None,
        }
    }

    fn mic(mute: MicMute) -> Request {
        Request {
            preset: Preset {
                cam: None,
                mic: Some(mute),
            },
            name: None,
        }
    }

    /// Lets the event loop handle the messages. Paused time advances when every
    /// task waits.
    async fn settle() {
        time::sleep(Duration::from_millis(10)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn status_shows_a_request_when_the_request_returns() {
        let (_, handle, outputs) = start("");

        handle.request(mic(MicMute::Unmuted)).await.unwrap();

        assert_eq!(outputs.status.borrow().mic.desired, Mute::Unmuted);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_target_is_leased_before_the_next_tick() {
        let (_, handle, outputs) = start("");
        ready(&handle, &Device::ALL).await;

        handle.request(live()).await.unwrap();

        assert_eq!(outputs.cam.borrow().granted.permission, CamPermission::Live);
        assert_eq!(
            outputs.mic.borrow().granted.permission,
            MicPermission::Permitted
        );
    }

    #[tokio::test(start_paused = true)]
    async fn status_shows_a_preset_name_that_changes_nothing() {
        let (_, handle, outputs) = start("");

        handle
            .request(Request {
                preset: Preset::PRIVACY,
                name: Some(PresetRef::Privacy),
            })
            .await
            .unwrap();

        assert_eq!(outputs.status.borrow().preset.as_deref(), Some("privacy"));
    }

    #[tokio::test(start_paused = true)]
    async fn each_tick_renews_the_leases() {
        let (_, _handle, outputs) = start("");
        let before = *outputs.mic.borrow();

        time::sleep(TICK).await;
        settle().await;

        assert!(outputs.mic.borrow().until > before.until);
    }

    #[tokio::test(start_paused = true)]
    async fn late_tick_lowers_every_plane() {
        let (_, handle, outputs) = start("");
        ready(&handle, &Device::ALL).await;
        handle.request(live()).await.unwrap();

        time::advance(2 * LEASE).await;
        settle().await;

        assert_eq!(
            outputs.cam.borrow().granted.permission,
            CamPermission::Blank
        );
        assert_eq!(
            outputs.mic.borrow().granted.permission,
            MicPermission::Denied
        );
        let status = outputs.status.borrow();
        assert_eq!(status.cam.cause, Some(Cause::ControllerStalled));
        assert_eq!(status.mic.cause, Some(Cause::ControllerStalled));
    }

    #[tokio::test(start_paused = true)]
    async fn automation_keeps_the_preset_name() {
        let (config, handle, mut outputs) = start("[presets.talk]\nmic = \"unmuted\"\n");
        let (name, preset) = config.preset("talk").unwrap();
        ready(&handle, &[Device::AudioInterface, Device::Gate]).await;
        handle
            .request(Request {
                preset,
                name: Some(name),
            })
            .await
            .unwrap();
        outputs.status.mark_unchanged();

        handle
            .observe(Observation::Device(Device::Gate, DeviceStatus::Fault))
            .await
            .unwrap();
        time::timeout(LEASE, outputs.status.changed())
            .await
            .expect("the status changes")
            .unwrap();

        let status = outputs.status.borrow();
        assert_eq!(status.mic.cause, Some(Cause::GateFault));
        assert_eq!(status.preset.as_deref(), Some("talk"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_single_plane_change_clears_the_preset_name() {
        let (config, handle, outputs) = start("[presets.talk]\nmic = \"unmuted\"\n");
        let (name, preset) = config.preset("talk").unwrap();
        handle
            .request(Request {
                preset,
                name: Some(name),
            })
            .await
            .unwrap();

        handle.request(mic(MicMute::Muted)).await.unwrap();

        assert_eq!(outputs.status.borrow().preset, None);
    }

    #[tokio::test(start_paused = true)]
    async fn report_that_changes_no_status_field_sends_no_status() {
        let (_, handle, outputs) = start("");

        ready(&handle, &[Device::Camera]).await;
        settle().await;

        assert!(!outputs.status.has_changed().unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn request_to_a_full_channel_is_busy() {
        let config = Arc::new(Config::parse("").unwrap());
        let (_event_loop, handle, _outputs) = EventLoop::new(config);
        for _ in 0..CAPACITY {
            ready(&handle, &[Device::Camera]).await;
        }

        let result = handle.request(mic(MicMute::Muted)).await;

        assert_eq!(result, Err(HandleError::Busy));
    }

    #[tokio::test(start_paused = true)]
    async fn request_to_a_stopped_loop_fails() {
        let config = Arc::new(Config::parse("").unwrap());
        let (event_loop, handle, _outputs) = EventLoop::new(config);
        drop(event_loop);

        let result = handle.request(mic(MicMute::Muted)).await;

        assert_eq!(result, Err(HandleError::Stopped));
    }
}
