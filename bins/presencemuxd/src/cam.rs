//! The adapter of the cam worker, a client of `io.presencemux.Cam`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use presencemux_api::cam::{self as api, CamProxy};
use presencemux_core::{CamPermission, Device, Epoch, PlaneId};
use zlink::Connection;

use crate::adapter::{Adapter, AdapterError, FIRST_WAIT, Latch, LatchReason, MAX_WAIT, Report};
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

    /// Returns the report of the cam worker in the types of the daemon. Keeps
    /// only a latch in the session of the last lease, the session of this
    /// daemon. Returns an error for a latch with an epoch out of range.
    fn report(&self, report: &api::Report) -> Result<Report, AdapterError> {
        let latch = report
            .latched
            .filter(|latch| latch.session_usec == self.session)
            .map(|latch| {
                let epoch =
                    u32::try_from(latch.epoch).map_err(|_| AdapterError::Invalid("latch epoch"))?;
                Ok(Latch {
                    plane: PlaneId::Cam,
                    epoch: Epoch::new(epoch),
                    reason: latch.reason.into(),
                })
            })
            .transpose()?;
        Ok(Report {
            devices: vec![
                (Device::Camera, report.camera.into()),
                (Device::CamWorker, report.worker.into()),
            ],
            latch,
        })
    }

    /// Logs the rejection of `report`, or else its detail, if the message
    /// differs from the last one.
    fn log(&mut self, report: &api::Report) {
        let message = report
            .rejected
            .map(|rejection| format!("the cam worker rejected the lease: {rejection}"))
            .or_else(|| {
                report
                    .detail
                    .as_ref()
                    .map(|detail| format!("the cam worker reports: {detail}"))
            });
        if message != self.last_message {
            if let Some(message) = &message {
                tracing::warn!("{message}");
            }
            self.last_message = message;
        }
    }
}

impl Adapter for Cam {
    type Command = Leased<CamPermission>;

    const OWNER: Owner = Owner::CamWorker;
    const TIMEOUT: Duration = Duration::from_millis(500);

    async fn exchange(&mut self, command: Self::Command) -> Result<Report, AdapterError> {
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
        self.report(&renewed.report)
    }

    fn reset(&mut self) {
        self.connection = None;
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

    use presencemux_core::{DeviceStatus, Granted, SlateId};
    use tokio::net::UnixListener;
    use tokio::sync::Notify;
    use tokio::task::spawn_local;
    use tokio::time;
    use zlink::{ReadyListener, Server};

    use super::*;
    use crate::event_loop::Monotonic;

    /// The session of the leases in the tests.
    const SESSION: Duration = Duration::from_secs(40);

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
            api::Shown,
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
            shown: api::Shown::Blank,
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
    async fn report_gives_the_status_of_the_camera_and_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        let reply = api::Report {
            camera: api::DeviceStatus::Initializing,
            ..report()
        };
        let (mut cam, _worker) = start(&dir, "", vec![Reply::Report(reply)]);

        let report = cam.exchange(leased(CamPermission::Blank, 0)).await.unwrap();

        assert_eq!(
            report.devices,
            [
                (Device::Camera, DeviceStatus::Initializing),
                (Device::CamWorker, DeviceStatus::Ready),
            ]
        );
    }

    #[tokio::test(flavor = "local")]
    async fn latch_of_this_session_gives_a_latch() {
        let dir = tempfile::tempdir().unwrap();
        let reply = api::Report {
            latched: Some(api::Latch {
                session_usec: 40_000_000,
                epoch: 2,
                reason: api::LatchReason::Stalled,
            }),
            ..report()
        };
        let (mut cam, _worker) = start(&dir, "", vec![Reply::Report(reply)]);

        let report = cam.exchange(leased(CamPermission::Live, 2)).await.unwrap();

        assert_eq!(
            report.latch,
            Some(Latch {
                plane: PlaneId::Cam,
                epoch: Epoch::new(2),
                reason: LatchReason::Stalled,
            })
        );
    }

    #[tokio::test(flavor = "local")]
    async fn latch_of_another_session_gives_no_latch() {
        let dir = tempfile::tempdir().unwrap();
        let reply = api::Report {
            latched: Some(api::Latch {
                session_usec: 39_000_000,
                epoch: 2,
                reason: api::LatchReason::Expired,
            }),
            ..report()
        };
        let (mut cam, _worker) = start(&dir, "", vec![Reply::Report(reply)]);

        let report = cam.exchange(leased(CamPermission::Live, 2)).await.unwrap();

        assert_eq!(report.latch, None);
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

        let report = cam.exchange(leased(CamPermission::Blank, 0)).await.unwrap();

        assert_eq!(report.devices[0], (Device::Camera, DeviceStatus::Ready));
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
