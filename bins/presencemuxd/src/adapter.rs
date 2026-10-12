//! The adapters that connect the daemon to the owners of the devices.

use std::fmt;
use std::time::Duration;

use presencemux_core::{Device, DeviceStatus, Epoch, Observation, PlaneId};
use tokio::sync::watch;
use tokio::time;

use crate::event_loop::{Handle, Owner};

/// The first wait after a failed exchange.
pub(crate) const FIRST_WAIT: Duration = Duration::from_millis(50);
/// The longest wait between two exchanges after failures.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(1);

/// A connection to one owner of devices.
pub(crate) trait Adapter {
    /// The command that the adapter sends, for example a lease.
    type Command: Clone;

    /// The owner of the devices.
    const OWNER: Owner;

    /// The longest time for one exchange. After it, the exchange fails.
    const TIMEOUT: Duration;

    /// Sends `command` to the owner and returns the report of the owner.
    async fn exchange(&mut self, command: Self::Command) -> Result<Report, AdapterError>;

    /// Drops the connection to the owner after a failed or canceled
    /// exchange. The next exchange connects again.
    fn reset(&mut self);
}

/// The status of the devices of one owner, and its latch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) devices: Vec<(Device, DeviceStatus)>,
    /// The latch of the owner in the session of this daemon, if the owner is
    /// latched.
    pub(crate) latch: Option<Latch>,
    /// The other observations of the owner, for example whether the host
    /// receives the stream.
    pub(crate) observations: Vec<Observation>,
}

/// A block of a privacy plane at the owner until a lease with a newer grant
/// arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Latch {
    pub(crate) plane: PlaneId,
    /// The epoch of the lease at the time of the latch.
    pub(crate) epoch: Epoch,
    pub(crate) reason: LatchReason,
}

/// The reason for a latch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LatchReason {
    /// The lease expired before the next lease arrived.
    Expired,
    /// A live lease had no fresh frame.
    Stalled,
    /// The owner started.
    Started,
}

impl fmt::Display for LatchReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Expired => "expired",
            Self::Stalled => "stalled",
            Self::Started => "started",
        })
    }
}

/// An error of an exchange.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AdapterError {
    /// The connection or the call failed.
    #[error(transparent)]
    Transport(Box<dyn std::error::Error>),
    /// The owner rejected the command as invalid, or sent an invalid report.
    /// This is a bug.
    #[error("invalid {0}")]
    Invalid(&'static str),
}

/// Sends the newest command to the owner, and forwards each report to the
/// event loop. After a failure, reports the devices of the owner at fault and
/// sends the newest command again after a growing wait. Finishes each exchange
/// before it takes a newer command. Returns when the event loop stops.
pub(crate) async fn drive<A: Adapter>(
    mut commands: watch::Receiver<A::Command>,
    mut adapter: A,
    handle: Handle,
) {
    let mut backoff = Backoff::default();
    let mut last_latch = None;
    let mut last_error = None;
    loop {
        let command = commands.borrow_and_update().clone();
        let result = match time::timeout(A::TIMEOUT, adapter.exchange(command)).await {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(_) => Err(format!("no reply within {} ms", A::TIMEOUT.as_millis())),
        };
        let observations = match result {
            Ok(report) => {
                backoff.restart();
                if last_error.take().is_some() {
                    tracing::info!("the {} replies again", A::OWNER);
                }
                observations(A::OWNER, report, &mut last_latch)
            }
            Err(message) => {
                adapter.reset();
                if last_error.as_ref() != Some(&message) {
                    tracing::warn!("exchange with the {} failed: {message}", A::OWNER);
                    last_error = Some(message);
                }
                A::OWNER.faults()
            }
        };
        if handle.report(A::OWNER, observations).await.is_err() {
            return;
        }
        if last_error.is_some() {
            time::sleep(backoff.next()).await;
            continue;
        }
        if commands.changed().await.is_err() {
            return;
        }
    }
}

/// The waits between exchanges after failures. Each wait doubles, from
/// [`FIRST_WAIT`] to [`MAX_WAIT`].
#[derive(Debug, Default)]
struct Backoff {
    next: Option<Duration>,
}

impl Backoff {
    /// Returns the next wait.
    fn next(&mut self) -> Duration {
        let wait = self.next.unwrap_or(FIRST_WAIT);
        self.next = Some((wait * 2).min(MAX_WAIT));
        wait
    }

    /// Starts again at [`FIRST_WAIT`].
    fn restart(&mut self) {
        self.next = None;
    }
}

/// Returns the observations of a report of `owner`. Gives `LeaseExpired` only
/// for a latch that differs from `last_latch`, and updates `last_latch`.
fn observations(owner: Owner, report: Report, last_latch: &mut Option<Latch>) -> Vec<Observation> {
    let mut observations: Vec<_> = report
        .devices
        .into_iter()
        .map(|(device, status)| Observation::Device(device, status))
        .collect();
    if let Some(latch) = report.latch
        && *last_latch != Some(latch)
    {
        tracing::info!(
            "the {owner} latched: {}, epoch {}",
            latch.reason,
            latch.epoch.get()
        );
        observations.push(Observation::LeaseExpired(latch.plane, latch.epoch));
    }
    *last_latch = report.latch;
    observations.extend(report.observations);
    observations
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::future::pending;
    use std::rc::Rc;

    use tokio::sync::mpsc;
    use tokio::task::spawn_local;

    use super::*;
    use crate::event_loop::Message;

    /// A scripted reply of the fake owner.
    #[derive(Debug)]
    enum Reply {
        Ready,
        Latched(Latch),
        Error,
        /// A reply after this time.
        Slow(Duration),
        /// No reply.
        Hang,
    }

    /// A fake owner that gives the scripted replies, then `Ready`.
    #[derive(Debug)]
    struct Fake {
        replies: VecDeque<Reply>,
        sent: Rc<RefCell<Vec<u32>>>,
        resets: Rc<Cell<u32>>,
    }

    impl Adapter for Fake {
        type Command = u32;

        const OWNER: Owner = Owner::CamWorker;
        const TIMEOUT: Duration = Duration::from_millis(500);

        async fn exchange(&mut self, command: u32) -> Result<Report, AdapterError> {
            self.sent.borrow_mut().push(command);
            match self.replies.pop_front().unwrap_or(Reply::Ready) {
                Reply::Ready => Ok(ready_report(None)),
                Reply::Latched(latch) => Ok(ready_report(Some(latch))),
                Reply::Error => Err(AdapterError::Invalid("command")),
                Reply::Slow(wait) => {
                    time::sleep(wait).await;
                    Ok(ready_report(None))
                }
                Reply::Hang => pending().await,
            }
        }

        fn reset(&mut self) {
            self.resets.set(self.resets.get() + 1);
        }
    }

    /// A running `drive` with a fake owner.
    struct Driven {
        commands: watch::Sender<u32>,
        messages: mpsc::Receiver<Message>,
        sent: Rc<RefCell<Vec<u32>>>,
        resets: Rc<Cell<u32>>,
    }

    impl Driven {
        /// Returns the observations of the next report.
        async fn next_report(&mut self) -> Vec<Observation> {
            match self.messages.recv().await {
                Some(Message::Report(_, observations)) => observations,
                other => panic!("expected a report, got {other:?}"),
            }
        }
    }

    fn ready_report(latch: Option<Latch>) -> Report {
        Report {
            devices: vec![
                (Device::Camera, DeviceStatus::Ready),
                (Device::CamWorker, DeviceStatus::Ready),
            ],
            latch,
            observations: Vec::new(),
        }
    }

    fn latch(epoch: u32, reason: LatchReason) -> Latch {
        Latch {
            plane: PlaneId::Cam,
            epoch: Epoch::new(epoch),
            reason,
        }
    }

    fn ready_observations() -> Vec<Observation> {
        vec![
            Observation::Device(Device::Camera, DeviceStatus::Ready),
            Observation::Device(Device::CamWorker, DeviceStatus::Ready),
        ]
    }

    fn fault_observations() -> Vec<Observation> {
        vec![
            Observation::Device(Device::Camera, DeviceStatus::Fault),
            Observation::Device(Device::CamWorker, DeviceStatus::Fault),
        ]
    }

    fn lease_expired(epoch: u32) -> Observation {
        Observation::LeaseExpired(PlaneId::Cam, Epoch::new(epoch))
    }

    /// Starts `drive` with the command 1 and the scripted `replies`.
    fn start(replies: Vec<Reply>) -> Driven {
        let (commands, receiver) = watch::channel(1);
        let (handle, messages) = Handle::detached();
        let sent = Rc::default();
        let resets = Rc::default();
        let fake = Fake {
            replies: replies.into(),
            sent: Rc::clone(&sent),
            resets: Rc::clone(&resets),
        };
        spawn_local(drive(receiver, fake, handle));
        Driven {
            commands,
            messages,
            sent,
            resets,
        }
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn drive_sends_the_command_at_start() {
        let mut driven = start(vec![]);

        driven.next_report().await;

        assert_eq!(*driven.sent.borrow(), [1]);
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn drive_sends_only_the_newest_of_two_commands() {
        let mut driven = start(vec![]);
        driven.next_report().await;

        driven.commands.send_replace(2);
        driven.commands.send_replace(3);
        driven.next_report().await;

        assert_eq!(*driven.sent.borrow(), [1, 3]);
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn new_command_does_not_cancel_a_slow_exchange() {
        let mut driven = start(vec![Reply::Slow(Duration::from_millis(100))]);
        time::sleep(Duration::from_millis(50)).await;

        driven.commands.send_replace(2);
        let first = driven.next_report().await;
        driven.next_report().await;

        assert_eq!(first, ready_observations());
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn report_gives_the_status_of_each_device() {
        let mut driven = start(vec![]);

        let report = driven.next_report().await;

        assert_eq!(report, ready_observations());
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn failed_exchange_reports_each_device_at_fault() {
        let mut driven = start(vec![Reply::Error]);

        let report = driven.next_report().await;

        assert_eq!(report, fault_observations());
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn failed_exchange_resets_the_adapter() {
        let mut driven = start(vec![Reply::Error]);

        driven.next_report().await;

        assert_eq!(driven.resets.get(), 1);
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn exchange_without_a_reply_reports_each_device_at_fault() {
        let mut driven = start(vec![Reply::Hang]);

        let report = driven.next_report().await;

        assert_eq!(report, fault_observations());
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn exchange_without_a_reply_fails_after_500_ms() {
        let before = time::Instant::now();
        let mut driven = start(vec![Reply::Hang]);

        driven.next_report().await;

        assert_eq!(before.elapsed(), Duration::from_millis(500));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn exchange_without_a_reply_resets_the_adapter() {
        let mut driven = start(vec![Reply::Hang]);

        driven.next_report().await;

        assert_eq!(driven.resets.get(), 1);
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn failed_exchange_sends_the_newest_command_again_after_50_ms() {
        let mut driven = start(vec![Reply::Error]);
        driven.next_report().await;
        driven.commands.send_replace(2);

        let before = time::Instant::now();
        driven.next_report().await;

        assert_eq!(before.elapsed(), Duration::from_millis(50));
        assert_eq!(*driven.sent.borrow(), [1, 2]);
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn same_latch_in_the_next_report_gives_no_lease_expired() {
        let expired = latch(3, LatchReason::Expired);
        let mut driven = start(vec![Reply::Latched(expired), Reply::Latched(expired)]);
        driven.next_report().await;

        driven.commands.send_replace(2);
        let second = driven.next_report().await;

        assert!(!second.contains(&lease_expired(3)));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn first_report_of_a_latch_gives_lease_expired() {
        let mut driven = start(vec![Reply::Latched(latch(3, LatchReason::Expired))]);

        let report = driven.next_report().await;

        assert!(report.contains(&lease_expired(3)));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn new_latch_reason_gives_lease_expired_again() {
        let mut driven = start(vec![
            Reply::Latched(latch(3, LatchReason::Stalled)),
            Reply::Latched(latch(3, LatchReason::Expired)),
        ]);
        driven.next_report().await;

        driven.commands.send_replace(2);
        let second = driven.next_report().await;

        assert!(second.contains(&lease_expired(3)));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn latch_after_a_report_without_a_latch_gives_lease_expired_again() {
        let expired = latch(3, LatchReason::Expired);
        let mut driven = start(vec![
            Reply::Latched(expired),
            Reply::Ready,
            Reply::Latched(expired),
        ]);
        driven.next_report().await;
        driven.commands.send_replace(2);
        driven.next_report().await;

        driven.commands.send_replace(3);
        let third = driven.next_report().await;

        assert!(third.contains(&lease_expired(3)));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn drive_returns_when_the_event_loop_stops() {
        let (_commands, receiver) = watch::channel(1);
        let (handle, messages) = Handle::detached();
        drop(messages);
        let fake = Fake {
            replies: VecDeque::new(),
            sent: Rc::default(),
            resets: Rc::default(),
        };

        time::timeout(Duration::from_secs(1), drive(receiver, fake, handle))
            .await
            .expect("drive returns");
    }

    #[test]
    fn backoff_doubles_from_50_ms_to_1_s() {
        let mut backoff = Backoff::default();

        let waits = [(); 7].map(|()| backoff.next().as_millis());

        assert_eq!(waits, [50, 100, 200, 400, 800, 1_000, 1_000]);
    }

    #[test]
    fn backoff_starts_again_at_50_ms_after_a_restart() {
        let mut backoff = Backoff::default();
        backoff.next();
        backoff.next();

        backoff.restart();

        assert_eq!(backoff.next(), Duration::from_millis(50));
    }
}
