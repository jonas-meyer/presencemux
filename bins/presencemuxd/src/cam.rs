//! The adapter of the cam worker, a client of `io.presencemux.Cam`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use presencemux_api::cam::{self as api, CamProxy};
use presencemux_core::{CamOutput, CamPermission, CamReport, Epoch, Granted, OnAir, SlateId};
use zlink::Connection;

use crate::adapter::{Adapter, AdapterError, FIRST_WAIT, Latch, LatchReason, MAX_WAIT, Reply};
use crate::config::Config;
use crate::event_loop::{LEASE, Leased, Owner, REPORT_TIMEOUT, TICK};

// These asserts check the timing rules in `daemon.md`.
const _: () = {
    let exchange = <Cam as Adapter>::TIMEOUT.as_millis();
    assert!(TICK.as_millis() + exchange + FIRST_WAIT.as_millis() < LEASE.as_millis());
    assert!(exchange + MAX_WAIT.as_millis() < REPORT_TIMEOUT.as_millis());
};

/// A Varlink stream of the runtime.
type Stream = zlink::tokio::unix::Stream;

/// The adapter of the cam worker.
#[derive(Debug)]
pub(crate) struct Cam {
    socket: PathBuf,
    connection: Option<Connection<Stream>>,
    config: Arc<Config>,
    /// The session of the last lease, in microseconds.
    session: i64,
    /// The last rejection or detail of the cam worker. The adapter logs a
    /// message only when it differs from this one.
    last_message: Option<String>,
}

impl Cam {
    /// Creates the adapter for the cam worker on `socket`.
    pub(crate) fn new(socket: PathBuf, config: Arc<Config>) -> Self {
        Self {
            socket,
            connection: None,
            config,
            session: 0,
            last_message: None,
        }
    }

    /// Returns `leased` in the wire types. A slate without a name gives blank.
    fn lease(&self, leased: Leased<CamPermission>) -> api::Lease {
        let (permission, slate) = match leased.granted.permission {
            CamPermission::Blank => (api::Permission::Blank, None),
            CamPermission::Live => (api::Permission::Live, None),
            CamPermission::Slate(id) => match self.config.slates.name(id) {
                Some(name) => (api::Permission::Slate, Some(name.to_owned())),
                None => (api::Permission::Blank, None),
            },
        };
        api::Lease {
            session_usec: leased.session.as_micros(),
            epoch: i64::from(leased.granted.epoch.get()),
            permission,
            slate,
            until_usec: leased.until.as_micros(),
        }
    }

    /// Returns the reply of the cam worker in the types of the daemon. Keeps
    /// only a latch and a grant in the session of the last lease, the session
    /// of this daemon. Returns an error for an epoch out of range, or for a
    /// granted slate name without a slate id.
    fn reply(&self, report: &api::Report) -> Result<Reply<CamReport>, AdapterError> {
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
            .as_ref()
            .filter(|grant| grant.session_usec == self.session)
            .map(|grant| {
                let epoch = u32::try_from(grant.epoch)
                    .map_err(|_| AdapterError::Invalid("applied epoch"))?;
                Ok(Granted {
                    permission: self.permission(grant.permission, grant.slate.as_deref())?,
                    epoch: Epoch::new(epoch),
                })
            })
            .transpose()?;
        let output = match report.output {
            api::Output::Nothing => CamOutput::Nothing,
            api::Output::Blank => CamOutput::Blank,
            api::Output::Live => CamOutput::Live,
            api::Output::Slate => CamOutput::Slate(
                report
                    .slate
                    .as_deref()
                    .and_then(|name| self.config.slates.id(name)),
            ),
        };
        Ok(Reply {
            report: CamReport {
                worker: report.worker.into(),
                camera: report.camera.into(),
                stream: report.host.into(),
                latched: latch.map(|latch| latch.epoch),
                output,
                exceeded: exceeded(report),
                applied,
            },
            latch,
            message: message(report),
        })
    }

    /// Returns `permission` with the slate `slate` in the types of core.
    fn permission(
        &self,
        permission: api::Permission,
        slate: Option<&str>,
    ) -> Result<CamPermission, AdapterError> {
        Ok(match permission {
            api::Permission::Blank => CamPermission::Blank,
            api::Permission::Live => CamPermission::Live,
            api::Permission::Slate => CamPermission::Slate(self.slate_id(slate)?),
        })
    }

    /// Returns the id of the slate `name`, or an error without a name or an id.
    fn slate_id(&self, name: Option<&str>) -> Result<SlateId, AdapterError> {
        name.and_then(|name| self.config.slates.id(name))
            .ok_or(AdapterError::Invalid("slate"))
    }

    /// Logs the rejection of `report`, or else its detail, if the message
    /// differs from the last one.
    fn log(&mut self, report: &api::Report) {
        let message = message(report);
        if message != self.last_message {
            if let Some(message) = &message {
                tracing::warn!("the cam worker: {message}");
            }
            self.last_message = message;
        }
    }
}

impl Adapter for Cam {
    type Command = Leased<CamPermission>;
    type Report = CamReport;

    const OWNER: Owner = Owner::CamWorker;
    const TIMEOUT: Duration = Duration::from_millis(500);

    async fn exchange(
        &mut self,
        command: Self::Command,
    ) -> Result<Reply<Self::Report>, AdapterError> {
        self.session = command.session.as_micros();
        let lease = self.lease(command);
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

/// Returns the rejection of `report`, or else its detail.
fn message(report: &api::Report) -> Option<String> {
    report
        .rejected
        .map(|rejection| format!("rejected the lease: {rejection}"))
        .or_else(|| report.detail.clone())
}

/// Returns whether the output of `report` is above the grant under which the
/// cam worker queued it. Without a grant, any output above blank exceeds it.
fn exceeded(report: &api::Report) -> bool {
    let output = match report.output {
        api::Output::Nothing | api::Output::Blank => OnAir::Off,
        api::Output::Slate => OnAir::Canned,
        api::Output::Live => OnAir::Live,
    };
    let permitted =
        report
            .output_grant
            .as_ref()
            .map_or(OnAir::Off, |grant| match grant.permission {
                api::Permission::Blank => OnAir::Off,
                api::Permission::Slate => OnAir::Canned,
                api::Permission::Live => OnAir::Live,
            });
    output > permitted
}

/// Returns a failed connection or call as an adapter error.
fn transport(error: zlink::Error) -> AdapterError {
    AdapterError::Transport(Box::new(error))
}

impl From<api::LatchReason> for LatchReason {
    fn from(reason: api::LatchReason) -> Self {
        match reason {
            api::LatchReason::Expired => Self::Expired,
            api::LatchReason::Stalled => Self::Stalled,
            api::LatchReason::Started => Self::Started,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use presencemux_core::{CamStream, DeviceStatus, Granted, SlateId};
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

    /// A scripted reply of the fake cam worker.
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

    /// The state of the fake cam worker: the scripted replies and the leases
    /// that it received.
    #[derive(Debug, Default)]
    struct Worker {
        replies: VecDeque<Reply>,
        leases: Vec<api::Lease>,
    }

    /// A fake `io.presencemux.Cam` service on one connection.
    #[derive(Clone, Debug)]
    struct FakeService {
        worker: Rc<RefCell<Worker>>,
        /// The signal to close the connection.
        close: Rc<Notify>,
    }

    #[zlink::service(
        interface = "io.presencemux.Cam",
        types = [
            api::Lease,
            api::Permission,
            api::Report,
            api::DeviceStatus,
            api::Latch,
            api::LatchReason,
            api::Rejection,
            api::Output,
            api::Grant,
            api::HostStream
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
            worker: api::DeviceStatus::Ready,
            camera: api::DeviceStatus::Ready,
            latched: None,
            rejected: None,
            output: api::Output::Blank,
            slate: None,
            output_grant: None,
            applied: None,
            host: api::HostStream::Idle,
            detail: None,
        }
    }

    fn leased(permission: CamPermission, epoch: u32) -> Leased<CamPermission> {
        Leased {
            session: Monotonic::from_duration(SESSION),
            granted: Granted {
                permission,
                epoch: Epoch::new(epoch),
            },
            until: Monotonic::from_duration(SESSION + Duration::from_millis(1_500)),
        }
    }

    /// Serves the fake cam worker on a socket in `dir`, and returns the adapter
    /// and the state of the fake cam worker.
    fn start(
        dir: &tempfile::TempDir,
        config: &str,
        replies: Vec<Reply>,
    ) -> (Cam, Rc<RefCell<Worker>>) {
        let path = dir.path().join("io.presencemux.Cam");
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
        let config = Arc::new(Config::parse(config).unwrap());
        (Cam::new(path, config), worker)
    }

    #[tokio::test(flavor = "local")]
    async fn lease_on_the_wire_has_the_times_in_microseconds() {
        let dir = tempfile::tempdir().unwrap();
        let (mut cam, worker) = start(&dir, "", vec![]);

        cam.exchange(leased(CamPermission::Live, 7)).await.unwrap();

        assert_eq!(
            worker.borrow().leases[0],
            api::Lease {
                session_usec: 40_000_000,
                epoch: 7,
                permission: api::Permission::Live,
                slate: None,
                until_usec: 41_500_000,
            }
        );
    }

    #[tokio::test(flavor = "local")]
    async fn slate_lease_carries_the_slate_name() {
        let dir = tempfile::tempdir().unwrap();
        let (mut cam, worker) = start(&dir, "[slates.away]\n", vec![]);

        cam.exchange(leased(CamPermission::Slate(SlateId(0)), 1))
            .await
            .unwrap();

        assert_eq!(worker.borrow().leases[0].slate.as_deref(), Some("away"));
    }

    #[tokio::test(flavor = "local")]
    async fn reply_gives_the_status_of_the_worker_and_the_camera() {
        let dir = tempfile::tempdir().unwrap();
        let reply = api::Report {
            camera: api::DeviceStatus::Initializing,
            ..report()
        };
        let (mut cam, _worker) = start(&dir, "", vec![Reply::Report(reply)]);

        let reply = cam.exchange(leased(CamPermission::Blank, 0)).await.unwrap();

        assert_eq!(
            (reply.report.worker, reply.report.camera),
            (DeviceStatus::Ready, DeviceStatus::Initializing)
        );
    }

    #[tokio::test(flavor = "local")]
    async fn reply_gives_the_stream_state_of_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let reply = api::Report {
            host: api::HostStream::Streaming,
            ..report()
        };
        let (mut cam, _worker) = start(&dir, "", vec![Reply::Report(reply)]);

        let reply = cam.exchange(leased(CamPermission::Blank, 0)).await.unwrap();

        assert_eq!(reply.report.stream, CamStream::Streaming);
    }

    /// Returns a grant of epoch 2 in this session with `permission`, and the
    /// slate `away` for a slate permission.
    fn grant(permission: api::Permission) -> api::Grant {
        api::Grant {
            session_usec: SESSION_USEC,
            epoch: 2,
            permission,
            slate: (permission == api::Permission::Slate).then(|| "away".to_owned()),
        }
    }

    #[test]
    fn live_output_under_its_own_grant_after_a_downgrade_does_not_exceed_it() {
        let report = api::Report {
            output: api::Output::Live,
            output_grant: Some(grant(api::Permission::Live)),
            applied: Some(grant(api::Permission::Slate)),
            ..report()
        };

        assert!(!exceeded(&report));
    }

    #[test]
    fn live_output_under_a_slate_grant_exceeds_it() {
        let report = api::Report {
            output: api::Output::Live,
            output_grant: Some(grant(api::Permission::Slate)),
            ..report()
        };

        assert!(exceeded(&report));
    }

    #[test]
    fn slate_output_without_a_grant_exceeds_it() {
        let report = api::Report {
            output: api::Output::Slate,
            slate: Some("away".to_owned()),
            output_grant: None,
            ..report()
        };

        assert!(exceeded(&report));
    }

    #[tokio::test(flavor = "local")]
    async fn output_slate_without_a_slate_id_gives_a_slate_without_an_id() {
        let dir = tempfile::tempdir().unwrap();
        let reply = api::Report {
            output: api::Output::Slate,
            slate: Some("gone".to_owned()),
            ..report()
        };
        let (mut cam, _worker) = start(&dir, "", vec![Reply::Report(reply)]);

        let reply = cam.exchange(leased(CamPermission::Blank, 0)).await.unwrap();

        assert_eq!(reply.report.output, CamOutput::Slate(None));
    }

    /// Returns a report of the worker with a stalled latch at epoch 2 in
    /// `session_usec`.
    fn latched_report(session_usec: i64) -> api::Report {
        api::Report {
            latched: Some(api::Latch {
                session_usec,
                epoch: 2,
                reason: api::LatchReason::Stalled,
            }),
            ..report()
        }
    }

    #[tokio::test(flavor = "local")]
    async fn latch_of_this_session_gives_the_latch_for_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let reply = latched_report(SESSION_USEC);
        let (mut cam, _worker) = start(&dir, "", vec![Reply::Report(reply)]);

        let reply = cam.exchange(leased(CamPermission::Live, 2)).await.unwrap();

        assert_eq!(
            reply.latch,
            Some(Latch {
                epoch: Epoch::new(2),
                reason: LatchReason::Stalled,
            })
        );
    }

    #[tokio::test(flavor = "local")]
    async fn latch_of_this_session_gives_its_epoch_in_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let reply = latched_report(SESSION_USEC);
        let (mut cam, _worker) = start(&dir, "", vec![Reply::Report(reply)]);

        let reply = cam.exchange(leased(CamPermission::Live, 2)).await.unwrap();

        assert_eq!(reply.report.latched, Some(Epoch::new(2)));
    }

    #[tokio::test(flavor = "local")]
    async fn latch_of_another_session_gives_no_latch_in_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let reply = latched_report(OLDER_SESSION_USEC);
        let (mut cam, _worker) = start(&dir, "", vec![Reply::Report(reply)]);

        let reply = cam.exchange(leased(CamPermission::Live, 2)).await.unwrap();

        assert_eq!(reply.report.latched, None);
    }

    #[tokio::test(flavor = "local")]
    async fn invalid_lease_gives_an_invalid_error() {
        let dir = tempfile::tempdir().unwrap();
        let (mut cam, _worker) = start(&dir, "", vec![Reply::Invalid]);

        let result = cam.exchange(leased(CamPermission::Blank, 0)).await;

        assert!(matches!(result, Err(AdapterError::Invalid("lease"))));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn exchange_after_a_reset_reads_its_own_reply_and_not_a_late_one() {
        let dir = tempfile::tempdir().unwrap();
        let late = api::Report {
            camera: api::DeviceStatus::Fault,
            ..report()
        };
        let (mut cam, _worker) = start(
            &dir,
            "",
            vec![Reply::Slow(Duration::from_millis(300), late)],
        );
        time::timeout(
            Duration::from_millis(100),
            cam.exchange(leased(CamPermission::Blank, 0)),
        )
        .await
        .expect_err("the first exchange times out");
        cam.reset();
        time::sleep(Duration::from_millis(300)).await;

        let reply = cam.exchange(leased(CamPermission::Blank, 0)).await.unwrap();

        assert_eq!(reply.report.camera, DeviceStatus::Ready);
    }

    #[tokio::test(flavor = "local")]
    async fn exchange_after_a_closed_connection_and_a_reset_connects_again() {
        let dir = tempfile::tempdir().unwrap();
        let (mut cam, _worker) = start(&dir, "", vec![Reply::Close]);
        cam.exchange(leased(CamPermission::Blank, 0))
            .await
            .expect_err("the cam worker closes the connection");
        cam.reset();

        let result = cam.exchange(leased(CamPermission::Blank, 0)).await;

        assert!(result.is_ok());
    }
}
