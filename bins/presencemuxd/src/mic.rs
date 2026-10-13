//! The adapter of the audio worker, a client of `io.presencemux.Audio`.

use std::path::PathBuf;
use std::time::Duration;

use presencemux_api::audio::{self as api, AudioProxy};
use presencemux_core::{Epoch, Granted, MicOutput, MicPermission, MicReport};
use zlink::Connection;

use crate::adapter::{Adapter, AdapterError, FIRST_WAIT, Latch, LatchReason, MAX_WAIT, Reply};
use crate::event_loop::{LEASE, Leased, Owner, REPORT_TIMEOUT, TICK};

// These asserts check the timing rules in `daemon.md`.
const _: () = {
    let exchange = <Mic as Adapter>::TIMEOUT.as_millis();
    assert!(TICK.as_millis() + exchange + FIRST_WAIT.as_millis() < LEASE.as_millis());
    assert!(exchange + MAX_WAIT.as_millis() < REPORT_TIMEOUT.as_millis());
};

/// A Varlink stream of the runtime.
type Stream = zlink::tokio::unix::Stream;

/// The adapter of the audio worker.
#[derive(Debug)]
pub(crate) struct Mic {
    socket: PathBuf,
    connection: Option<Connection<Stream>>,
    /// The session of the last lease, in microseconds.
    session: i64,
    /// The last rejection of the audio worker. The adapter logs a rejection
    /// only when it differs from this one.
    last_rejection: Option<api::Rejection>,
}

impl Mic {
    /// Creates the adapter for the audio worker on `socket`.
    pub(crate) fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            connection: None,
            session: 0,
            last_rejection: None,
        }
    }

    /// Returns the reply of the audio worker in the types of the daemon. Keeps
    /// only a latch and a grant in the session of the last lease, the session
    /// of this daemon. Returns an error for an epoch out of range.
    fn reply(&self, report: &api::Report) -> Result<Reply<MicReport>, AdapterError> {
        let latch = report
            .latched
            .filter(|latch| latch.session_usec == self.session)
            .map(|latch| {
                let epoch =
                    u32::try_from(latch.epoch).map_err(|_| AdapterError::Invalid("latch epoch"))?;
                Ok(Latch {
                    epoch: Epoch::new(epoch),
                    reason: latch.reason.into(),
                })
            })
            .transpose()?;
        let applied = report
            .applied
            .filter(|grant| grant.session_usec == self.session)
            .map(|grant| {
                let epoch = u32::try_from(grant.epoch)
                    .map_err(|_| AdapterError::Invalid("applied epoch"))?;
                Ok(Granted {
                    permission: match grant.permission {
                        api::Permission::Denied => MicPermission::Denied,
                        api::Permission::Permitted => MicPermission::Permitted,
                    },
                    epoch: Epoch::new(epoch),
                })
            })
            .transpose()?;
        Ok(Reply {
            report: MicReport {
                gate: report.gate.into(),
                interface: report.interface.into(),
                latched: latch.map(|latch| latch.epoch),
                output: match report.output {
                    api::Output::Silent => MicOutput::Silent,
                    api::Output::Closing => MicOutput::Closing,
                    api::Output::Passing => MicOutput::Passing,
                },
                exceeded: exceeded(report),
                applied,
            },
            latch,
            message: report
                .rejected
                .map(|rejection| format!("rejected the lease: {rejection}")),
        })
    }

    /// Logs the rejection of `report` if it differs from the last one.
    fn log(&mut self, report: &api::Report) {
        if report.rejected != self.last_rejection {
            if let Some(rejection) = report.rejected {
                tracing::warn!("the audio worker rejected the lease: {rejection}");
            }
            self.last_rejection = report.rejected;
        }
    }
}

impl Adapter for Mic {
    type Command = Leased<MicPermission>;
    type Report = MicReport;

    const OWNER: Owner = Owner::AudioWorker;
    const TIMEOUT: Duration = Duration::from_millis(500);

    async fn exchange(
        &mut self,
        command: Self::Command,
    ) -> Result<Reply<Self::Report>, AdapterError> {
        self.session = command.session.as_micros();
        let lease = lease(command);
        let connection = if let Some(connection) = self.connection.take() {
            connection
        } else {
            zlink::tokio::unix::connect(&self.socket)
                .await
                .map_err(transport)?
        };
        let connection = self.connection.insert(connection);
        let renewed = connection
            .renew(&lease)
            .await
            .map_err(transport)?
            .map_err(|api::Error::InvalidLease| AdapterError::Invalid("lease"))?;
        self.log(&renewed.report);
        self.reply(&renewed.report)
    }

    fn reset(&mut self) {
        self.connection = None;
    }
}

/// Returns whether the gate passes audio under a grant that does not permit
/// it. A closing ramp never exceeds its grant.
fn exceeded(report: &api::Report) -> bool {
    report.output == api::Output::Passing
        && report
            .output_grant
            .is_none_or(|grant| grant.permission != api::Permission::Permitted)
}

/// Returns `leased` in the wire types.
fn lease(leased: Leased<MicPermission>) -> api::Lease {
    api::Lease {
        session_usec: leased.session.as_micros(),
        epoch: i64::from(leased.granted.epoch.get()),
        permission: match leased.granted.permission {
            MicPermission::Denied => api::Permission::Denied,
            MicPermission::Permitted => api::Permission::Permitted,
        },
        until_usec: leased.until.as_micros(),
    }
}

/// Returns a failed connection or call as an adapter error.
fn transport(error: zlink::Error) -> AdapterError {
    AdapterError::Transport(Box::new(error))
}

impl From<api::LatchReason> for LatchReason {
    fn from(reason: api::LatchReason) -> Self {
        match reason {
            api::LatchReason::Expired => Self::Expired,
            api::LatchReason::Started => Self::Started,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use presencemux_core::{DeviceStatus, Granted};
    use tokio::net::UnixListener;
    use tokio::sync::Notify;
    use tokio::task::spawn_local;
    use tokio::time;
    use zlink::{ReadyListener, Server};

    use super::*;
    use crate::event_loop::Monotonic;

    /// The session of the leases in the tests.
    const SESSION: Duration = Duration::from_secs(40);
    /// [`SESSION`] in microseconds.
    const SESSION_USEC: i64 = 40_000_000;
    /// A session before [`SESSION`], in microseconds.
    const OLDER_SESSION_USEC: i64 = 39_000_000;

    /// A scripted reply of the fake audio worker.
    #[derive(Clone, Debug)]
    enum Reply {
        Report(api::Report),
        /// A report after this time.
        Slow(Duration, api::Report),
        /// The worker closes the connection without a reply.
        Close,
        /// The error `InvalidLease`.
        Invalid,
    }

    /// The state of the fake audio worker: the scripted replies and the
    /// leases that it received.
    #[derive(Debug, Default)]
    struct Worker {
        replies: VecDeque<Reply>,
        leases: Vec<api::Lease>,
    }

    /// A fake `io.presencemux.Audio` service on one connection.
    #[derive(Clone, Debug)]
    struct FakeService {
        worker: Rc<RefCell<Worker>>,
        /// The signal to close the connection.
        close: Rc<Notify>,
    }

    #[zlink::service(
        interface = "io.presencemux.Audio",
        types = [
            api::Lease,
            api::Permission,
            api::Report,
            api::DeviceStatus,
            api::Latch,
            api::LatchReason,
            api::Rejection,
            api::Output,
            api::Grant
        ]
    )]
    impl FakeService {
        async fn renew(&self, lease: api::Lease) -> Result<api::Renewed, api::Error> {
            let reply = {
                let mut worker = self.worker.borrow_mut();
                worker.leases.push(lease);
                worker
                    .replies
                    .pop_front()
                    .unwrap_or(Reply::Report(report()))
            };
            match reply {
                Reply::Report(report) => Ok(api::Renewed { report }),
                Reply::Slow(wait, report) => {
                    time::sleep(wait).await;
                    Ok(api::Renewed { report })
                }
                Reply::Close => {
                    self.close.notify_one();
                    std::future::pending().await
                }
                Reply::Invalid => Err(api::Error::InvalidLease),
            }
        }
    }

    fn report() -> api::Report {
        api::Report {
            gate: api::DeviceStatus::Ready,
            interface: api::DeviceStatus::Ready,
            latched: None,
            rejected: None,
            output: api::Output::Silent,
            output_grant: None,
            applied: None,
        }
    }

    fn leased(permission: MicPermission, epoch: u32) -> Leased<MicPermission> {
        Leased {
            session: Monotonic::from_duration(SESSION),
            granted: Granted {
                permission,
                epoch: Epoch::new(epoch),
            },
            until: Monotonic::from_duration(SESSION + Duration::from_millis(1_500)),
        }
    }

    /// Serves the fake audio worker on a socket in `dir`, and returns the
    /// adapter and the state of the fake audio worker.
    fn start(dir: &tempfile::TempDir, replies: Vec<Reply>) -> (Mic, Rc<RefCell<Worker>>) {
        let path = dir.path().join("io.presencemux.Audio");
        let listener = UnixListener::bind(&path).unwrap();
        let worker = Rc::new(RefCell::new(Worker {
            replies: replies.into(),
            leases: Vec::new(),
        }));
        let shared = Rc::clone(&worker);
        spawn_local(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let close = Rc::new(Notify::new());
                let service = FakeService {
                    worker: Rc::clone(&shared),
                    close: Rc::clone(&close),
                };
                spawn_local(async move {
                    let stream = zlink::tokio::unix::Stream::try_from(stream).unwrap();
                    let server = Server::new(ReadyListener::new(stream), service);
                    tokio::select! {
                        _ = server.run() => {}
                        () = close.notified() => {}
                    }
                });
            }
        });
        (Mic::new(path), worker)
    }

    #[tokio::test(flavor = "local")]
    async fn lease_on_the_wire_has_the_times_in_microseconds() {
        let dir = tempfile::tempdir().unwrap();
        let (mut mic, worker) = start(&dir, vec![]);

        mic.exchange(leased(MicPermission::Permitted, 7))
            .await
            .unwrap();

        assert_eq!(
            worker.borrow().leases[0],
            api::Lease {
                session_usec: 40_000_000,
                epoch: 7,
                permission: api::Permission::Permitted,
                until_usec: 41_500_000,
            }
        );
    }

    /// Returns a grant of epoch 2 in this session with `permission`.
    fn grant(permission: api::Permission) -> api::Grant {
        api::Grant {
            session_usec: SESSION_USEC,
            epoch: 2,
            permission,
        }
    }

    #[test]
    fn closing_output_under_a_denied_grant_does_not_exceed_it() {
        let report = api::Report {
            output: api::Output::Closing,
            output_grant: Some(grant(api::Permission::Denied)),
            ..report()
        };

        assert!(!exceeded(&report));
    }

    #[test]
    fn passing_output_under_a_denied_grant_exceeds_it() {
        let report = api::Report {
            output: api::Output::Passing,
            output_grant: Some(grant(api::Permission::Denied)),
            ..report()
        };

        assert!(exceeded(&report));
    }

    #[test]
    fn passing_output_without_a_grant_exceeds_it() {
        let report = api::Report {
            output: api::Output::Passing,
            output_grant: None,
            ..report()
        };

        assert!(exceeded(&report));
    }

    #[test]
    fn passing_output_under_its_own_grant_after_a_mute_does_not_exceed_it() {
        let report = api::Report {
            output: api::Output::Passing,
            output_grant: Some(grant(api::Permission::Permitted)),
            applied: Some(grant(api::Permission::Denied)),
            ..report()
        };

        assert!(!exceeded(&report));
    }

    #[tokio::test(flavor = "local")]
    async fn reply_gives_the_status_of_the_gate_and_the_interface() {
        let dir = tempfile::tempdir().unwrap();
        let reply = api::Report {
            interface: api::DeviceStatus::Missing,
            ..report()
        };
        let (mut mic, _worker) = start(&dir, vec![Reply::Report(reply)]);

        let reply = mic
            .exchange(leased(MicPermission::Denied, 0))
            .await
            .unwrap();

        assert_eq!(
            (reply.report.gate, reply.report.interface),
            (DeviceStatus::Ready, DeviceStatus::Missing)
        );
    }

    #[tokio::test(flavor = "local")]
    async fn latch_of_this_session_gives_the_latch_for_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let reply = api::Report {
            latched: Some(api::Latch {
                session_usec: SESSION_USEC,
                epoch: 2,
                reason: api::LatchReason::Expired,
            }),
            ..report()
        };
        let (mut mic, _worker) = start(&dir, vec![Reply::Report(reply)]);

        let reply = mic
            .exchange(leased(MicPermission::Permitted, 2))
            .await
            .unwrap();

        assert_eq!(
            reply.latch,
            Some(Latch {
                epoch: Epoch::new(2),
                reason: LatchReason::Expired,
            })
        );
    }

    #[tokio::test(flavor = "local")]
    async fn latch_of_this_session_gives_its_epoch_in_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let reply = api::Report {
            latched: Some(api::Latch {
                session_usec: SESSION_USEC,
                epoch: 2,
                reason: api::LatchReason::Expired,
            }),
            ..report()
        };
        let (mut mic, _worker) = start(&dir, vec![Reply::Report(reply)]);

        let reply = mic
            .exchange(leased(MicPermission::Permitted, 2))
            .await
            .unwrap();

        assert_eq!(reply.report.latched, Some(Epoch::new(2)));
    }

    #[tokio::test(flavor = "local")]
    async fn started_latch_gives_the_started_reason() {
        let dir = tempfile::tempdir().unwrap();
        let reply = api::Report {
            latched: Some(api::Latch {
                session_usec: SESSION_USEC,
                epoch: 0,
                reason: api::LatchReason::Started,
            }),
            ..report()
        };
        let (mut mic, _worker) = start(&dir, vec![Reply::Report(reply)]);

        let reply = mic
            .exchange(leased(MicPermission::Denied, 0))
            .await
            .unwrap();

        assert_eq!(
            reply.latch.map(|latch| latch.reason),
            Some(LatchReason::Started)
        );
    }

    #[tokio::test(flavor = "local")]
    async fn latch_with_an_epoch_out_of_range_gives_an_invalid_error() {
        let dir = tempfile::tempdir().unwrap();
        let reply = api::Report {
            latched: Some(api::Latch {
                session_usec: SESSION_USEC,
                epoch: -1,
                reason: api::LatchReason::Expired,
            }),
            ..report()
        };
        let (mut mic, _worker) = start(&dir, vec![Reply::Report(reply)]);

        let result = mic.exchange(leased(MicPermission::Denied, 0)).await;

        assert!(matches!(result, Err(AdapterError::Invalid("latch epoch"))));
    }

    #[tokio::test(flavor = "local")]
    async fn latch_of_another_session_gives_no_latch_in_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let reply = api::Report {
            latched: Some(api::Latch {
                session_usec: OLDER_SESSION_USEC,
                epoch: 2,
                reason: api::LatchReason::Expired,
            }),
            ..report()
        };
        let (mut mic, _worker) = start(&dir, vec![Reply::Report(reply)]);

        let reply = mic
            .exchange(leased(MicPermission::Permitted, 2))
            .await
            .unwrap();

        assert_eq!(reply.report.latched, None);
    }

    #[tokio::test(flavor = "local")]
    async fn invalid_lease_reply_gives_an_invalid_lease_error() {
        let dir = tempfile::tempdir().unwrap();
        let (mut mic, _worker) = start(&dir, vec![Reply::Invalid]);

        let result = mic.exchange(leased(MicPermission::Denied, 0)).await;

        assert!(matches!(result, Err(AdapterError::Invalid("lease"))));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn exchange_after_a_reset_reads_its_own_reply_and_not_a_late_one() {
        let dir = tempfile::tempdir().unwrap();
        let late = api::Report {
            interface: api::DeviceStatus::Fault,
            ..report()
        };
        let (mut mic, _worker) = start(&dir, vec![Reply::Slow(Duration::from_millis(300), late)]);
        time::timeout(
            Duration::from_millis(100),
            mic.exchange(leased(MicPermission::Denied, 0)),
        )
        .await
        .expect_err("the first exchange times out");
        mic.reset();
        time::sleep(Duration::from_millis(300)).await;

        let reply = mic
            .exchange(leased(MicPermission::Denied, 0))
            .await
            .unwrap();

        assert_eq!(reply.report.interface, DeviceStatus::Ready);
    }

    #[tokio::test(flavor = "local")]
    async fn exchange_after_a_closed_connection_and_a_reset_connects_again() {
        let dir = tempfile::tempdir().unwrap();
        let (mut mic, _worker) = start(&dir, vec![Reply::Close]);
        mic.exchange(leased(MicPermission::Denied, 0))
            .await
            .expect_err("the audio worker closes the connection");
        mic.reset();

        let result = mic.exchange(leased(MicPermission::Denied, 0)).await;

        assert!(result.is_ok());
    }
}
