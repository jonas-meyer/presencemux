//! The adapters that connect the daemon to the owners of the devices.

use std::fmt;
use std::time::Duration;

use presencemux_core::{Epoch, Observation};
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

    /// The report of the owner in the types of core, for example `CamReport`.
    type Report: Into<Observation>;

    /// The owner of the devices.
    const OWNER: Owner;

    /// The longest time for one exchange. After it, the exchange fails.
    const TIMEOUT: Duration;

    /// Sends `command` to the owner and returns the reply of the owner.
    async fn exchange(
        &mut self,
        command: Self::Command,
    ) -> Result<Reply<Self::Report>, AdapterError>;

    /// Drops the connection to the owner after a failed or canceled
    /// exchange. The next exchange connects again.
    fn reset(&mut self);
}

/// The report of one owner, and its latch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reply<R> {
    pub(crate) report: R,
    /// The latch of the owner in the session of this daemon, if the owner is
    /// latched. The report holds its epoch.
    pub(crate) latch: Option<Latch>,
}

/// A block of a privacy plane at the owner until a lease with a newer grant
/// arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Latch {
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
/// event loop. After a failure, reports the owner as lost and sends the newest
/// command again after a growing wait. Finishes each exchange before it takes
/// a newer command. Returns when the event loop stops.
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
        let observation = match result {
            Ok(reply) => {
                backoff.restart();
                if last_error.take().is_some() {
                    tracing::info!("the {} replies again", A::OWNER);
                }
                log_latch(A::OWNER, reply.latch, &mut last_latch);
                reply.report.into()
            }
            Err(message) => {
                adapter.reset();
                if last_error.as_ref() != Some(&message) {
                    tracing::warn!("exchange with the {} failed: {message}", A::OWNER);
                    last_error = Some(message);
                }
                Observation::Lost(A::OWNER.plane())
            }
        };
        if handle.report(A::OWNER, observation).await.is_err() {
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

/// Logs `latch` if it differs from `last_latch`, and updates `last_latch`.
fn log_latch(owner: Owner, latch: Option<Latch>, last_latch: &mut Option<Latch>) {
    if let Some(latch) = latch
        && *last_latch != Some(latch)
    {
        tracing::info!(
            "the {owner} latched: {}, epoch {}",
            latch.reason,
            latch.epoch.get()
        );
    }
    *last_latch = latch;
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::future::pending;
    use std::rc::Rc;

    use tokio::sync::mpsc;
    use tokio::task::spawn_local;

    use presencemux_core::{CamReport, CamStream, DeviceStatus, PlaneId};

    use super::*;
    use crate::event_loop::Message;

    /// A scripted reply of the fake owner.
    #[derive(Debug)]
    enum Script {
        Ready,
        Error,
        /// A reply after this time.
        Slow(Duration),
        /// No reply.
        Hang,
    }

    /// A fake owner that gives the scripted replies, then `Ready`.
    #[derive(Debug)]
    struct Fake {
        replies: VecDeque<Script>,
        sent: Rc<RefCell<Vec<u32>>>,
        resets: Rc<Cell<u32>>,
    }

    impl Adapter for Fake {
        type Command = u32;
        type Report = CamReport;

        const OWNER: Owner = Owner::CamWorker;
        const TIMEOUT: Duration = Duration::from_millis(500);

        async fn exchange(&mut self, command: u32) -> Result<Reply<CamReport>, AdapterError> {
            self.sent.borrow_mut().push(command);
            match self.replies.pop_front().unwrap_or(Script::Ready) {
                Script::Ready => Ok(ready_reply()),
                Script::Error => Err(AdapterError::Invalid("command")),
                Script::Slow(wait) => {
                    time::sleep(wait).await;
                    Ok(ready_reply())
                }
                Script::Hang => pending().await,
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
        /// Returns the observation of the next report.
        async fn next_report(&mut self) -> Observation {
            match self.messages.recv().await {
                Some(Message::Report(_, observation)) => observation,
                other => panic!("expected a report, got {other:?}"),
            }
        }
    }

    const READY: CamReport = CamReport {
        worker: DeviceStatus::Ready,
        camera: DeviceStatus::Ready,
        stream: CamStream::Idle,
        latched: None,
    };

    fn ready_reply() -> Reply<CamReport> {
        Reply {
            report: READY,
            latch: None,
        }
    }

    /// Starts `drive` with the command 1 and the scripted `replies`.
    fn start(replies: Vec<Script>) -> Driven {
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
        let mut driven = start(vec![Script::Slow(Duration::from_millis(100))]);
        time::sleep(Duration::from_millis(50)).await;

        driven.commands.send_replace(2);
        let first = driven.next_report().await;
        driven.next_report().await;

        assert_eq!(first, Observation::Cam(READY));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn report_reaches_the_event_loop_as_one_observation() {
        let mut driven = start(vec![]);

        let observation = driven.next_report().await;

        assert_eq!(observation, Observation::Cam(READY));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn failed_exchange_reports_the_plane_lost() {
        let mut driven = start(vec![Script::Error]);

        let observation = driven.next_report().await;

        assert_eq!(observation, Observation::Lost(PlaneId::Cam));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn failed_exchange_resets_the_adapter() {
        let mut driven = start(vec![Script::Error]);

        driven.next_report().await;

        assert_eq!(driven.resets.get(), 1);
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn exchange_without_a_reply_reports_the_plane_lost() {
        let mut driven = start(vec![Script::Hang]);

        let observation = driven.next_report().await;

        assert_eq!(observation, Observation::Lost(PlaneId::Cam));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn exchange_without_a_reply_fails_after_500_ms() {
        let before = time::Instant::now();
        let mut driven = start(vec![Script::Hang]);

        driven.next_report().await;

        assert_eq!(before.elapsed(), Duration::from_millis(500));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn exchange_without_a_reply_resets_the_adapter() {
        let mut driven = start(vec![Script::Hang]);

        driven.next_report().await;

        assert_eq!(driven.resets.get(), 1);
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn failed_exchange_sends_the_newest_command_again_after_50_ms() {
        let mut driven = start(vec![Script::Error]);
        driven.next_report().await;
        driven.commands.send_replace(2);

        let before = time::Instant::now();
        driven.next_report().await;

        assert_eq!(before.elapsed(), Duration::from_millis(50));
        assert_eq!(*driven.sent.borrow(), [1, 2]);
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
