//! The Varlink service `io.presencemux.Cam`. presencemuxd sends the leases.

use std::cell::RefCell;
use std::convert::Infallible;
use std::future::pending;
use std::io;
use std::rc::Rc;
use std::time::Duration;

use presencemux_api::cam::{
    self as api, Description, DeviceStatus, Error, HostStream, LatchReason, Lease, Permission,
    Rejection, Renewed, Report, Shown,
};
use rustix::io::Errno;
use tokio::net::{UnixListener, UnixStream};
use zlink::connection::Gid;
use zlink::{Connection, Listener, Server};

use crate::capture::CameraStatus;
use crate::choice::{self, Choice};
use crate::v4l2::monotonic_now;

/// The lease, the latch and the reported state, shared by the event loop and
/// the Varlink connections on one thread.
pub(crate) type Shared = Rc<RefCell<Control>>;

/// A Varlink stream of the runtime.
type Stream = zlink::tokio::unix::Stream;

const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// The lease, the latch and the state of the worker.
#[derive(Debug)]
pub(crate) struct Control {
    pub(crate) choice: Choice,
    pub(crate) worker: DeviceStatus,
    pub(crate) camera: CameraStatus,
    /// Whether the host receives a stream.
    pub(crate) streaming: bool,
    /// The capture time of the newest live frame. The worker does not capture
    /// live frames yet, and a live lease latches as stalled.
    pub(crate) newest: Option<Duration>,
}

impl Control {
    pub(crate) fn new() -> Self {
        Self {
            choice: Choice::default(),
            worker: DeviceStatus::Missing,
            camera: CameraStatus::Missing,
            streaming: false,
            newest: None,
        }
    }

    /// Returns the report at the time `now`. `rejected` is the rejection of the
    /// lease in this call.
    fn report(&self, now: Duration, rejected: Option<choice::Rejection>) -> Report {
        let (shown, detail) = frame(self.choice.shown(now, self.newest));
        Report {
            worker: self.worker,
            camera: self.camera.into(),
            latched: self.choice.latch().map(|latch| api::Latch {
                session_usec: micros(latch.grant.session),
                epoch: i64::from(latch.grant.epoch),
                reason: latch.reason.into(),
            }),
            rejected: rejected.map(Rejection::from),
            shown,
            host: if self.streaming {
                HostStream::Streaming
            } else {
                HostStream::Idle
            },
            detail,
        }
    }
}

impl From<CameraStatus> for DeviceStatus {
    fn from(status: CameraStatus) -> Self {
        match status {
            CameraStatus::Missing => Self::Missing,
            CameraStatus::Initializing => Self::Initializing,
            CameraStatus::Ready => Self::Ready,
            CameraStatus::Fault => Self::Fault,
        }
    }
}

impl From<choice::Reason> for LatchReason {
    fn from(reason: choice::Reason) -> Self {
        match reason {
            choice::Reason::Expired => Self::Expired,
            choice::Reason::Stalled => Self::Stalled,
            choice::Reason::Started => Self::Started,
        }
    }
}

impl From<choice::Rejection> for Rejection {
    fn from(rejection: choice::Rejection) -> Self {
        match rejection {
            choice::Rejection::Late => Self::Late,
            choice::Rejection::TooLong => Self::TooLong,
            choice::Rejection::OldSession => Self::OldSession,
            choice::Rejection::OldEpoch => Self::OldEpoch,
            choice::Rejection::Raised => Self::Raised,
        }
    }
}

/// Returns the frame that the worker sends for `shown`, and a detail for the
/// report. The worker has no slates yet, and sends blank for each slate.
fn frame(shown: choice::Shown<'_>) -> (Shown, Option<String>) {
    match shown {
        choice::Shown::Blank => (Shown::Blank, None),
        choice::Shown::Live => (Shown::Live, None),
        choice::Shown::Slate(name) => (Shown::Blank, Some(format!("no slate named {name}"))),
    }
}

/// Returns `lease` in the types of frame choice, or `InvalidLease` for a time
/// or an epoch out of range, or for a slate name that does not match the
/// permission.
fn lease_from_wire(lease: Lease) -> Result<choice::Lease, Error> {
    let time = |micros: i64| {
        u64::try_from(micros)
            .map(Duration::from_micros)
            .map_err(|_| Error::InvalidLease)
    };
    let permission = match (lease.permission, lease.slate) {
        (Permission::Blank, None) => choice::Permission::Blank,
        (Permission::Live, None) => choice::Permission::Live,
        (Permission::Slate, Some(name)) => choice::Permission::Slate(name),
        _ => return Err(Error::InvalidLease),
    };
    Ok(choice::Lease {
        grant: choice::Grant {
            session: time(lease.session_usec)?,
            epoch: u32::try_from(lease.epoch).map_err(|_| Error::InvalidLease)?,
        },
        permission,
        until: time(lease.until_usec)?,
    })
}

/// Returns `lease` in the wire types.
fn lease_to_wire(lease: &choice::Lease) -> Lease {
    let (permission, slate) = match &lease.permission {
        choice::Permission::Blank => (Permission::Blank, None),
        choice::Permission::Slate(name) => (Permission::Slate, Some(name.clone())),
        choice::Permission::Live => (Permission::Live, None),
    };
    Lease {
        session_usec: micros(lease.grant.session),
        epoch: i64::from(lease.grant.epoch),
        permission,
        slate,
        until_usec: micros(lease.until),
    }
}

/// Returns a time in microseconds. The largest value is `i64::MAX`.
fn micros(time: Duration) -> i64 {
    i64::try_from(time.as_micros()).unwrap_or(i64::MAX)
}

/// Returns whether a peer with the user `uid` and the groups `gids` may call
/// the service. Root and peers with the group of the socket file may call it.
pub(crate) fn permitted(
    uid: u32,
    gids: impl IntoIterator<Item = u32>,
    socket_gid: Option<u32>,
) -> bool {
    uid == 0 || socket_gid.is_some_and(|socket_gid| gids.into_iter().any(|gid| gid == socket_gid))
}

/// Serves each connection from a permitted peer. `socket_gid` is the group of
/// the socket file. Returns when the socket fails.
pub(crate) async fn serve(
    listener: UnixListener,
    service: Service,
    socket_gid: Option<u32>,
) -> io::Result<Infallible> {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::task::spawn_local(serve_connection(stream, service.clone(), socket_gid));
            }
            Err(error) if is_transient(&error) => {
                tracing::warn!(
                    "cannot accept a Varlink connection, next try in {} ms: {error}",
                    ACCEPT_RETRY.as_millis()
                );
                tokio::time::sleep(ACCEPT_RETRY).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Returns whether an `accept` error is temporary, for example until other
/// connections close.
fn is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
    ) || Errno::from_io_error(error).is_some_and(|errno| {
        [Errno::MFILE, Errno::NFILE, Errno::NOBUFS, Errno::NOMEM].contains(&errno)
    })
}

/// Serves one connection if the peer is permitted, and closes it if not.
async fn serve_connection(stream: UnixStream, service: Service, socket_gid: Option<u32>) {
    let mut connection = match Stream::try_from(stream) {
        Ok(stream) => Connection::new(stream),
        Err(error) => {
            tracing::debug!("Varlink connection failed: {error}");
            return;
        }
    };
    match connection.peer_credentials().await {
        Ok(peer) => {
            let uid = peer.unix_user_id().as_raw();
            let gids = std::iter::once(peer.unix_primary_group_id())
                .chain(peer.unix_supplementary_group_ids().iter().copied())
                .map(Gid::as_raw);
            if !permitted(uid, gids, socket_gid) {
                tracing::warn!("refused a Varlink connection from user {uid}");
                return;
            }
        }
        Err(error) => {
            tracing::warn!("cannot read the peer of a Varlink connection: {error}");
            return;
        }
    }
    if let Err(error) = Server::new(
        Accepted {
            connection: Some(connection),
            closed: false,
        },
        service,
    )
    .run()
    .await
    {
        tracing::debug!("Varlink connection failed: {error}");
    }
}

/// A listener that gives one accepted connection to a server, then closes.
#[derive(Debug)]
struct Accepted {
    connection: Option<Connection<Stream>>,
    closed: bool,
}

impl Listener for Accepted {
    type Socket = Stream;

    async fn accept(&mut self) -> zlink::Result<Option<Connection<Stream>>> {
        if let Some(connection) = self.connection.take() {
            return Ok(Some(connection));
        }
        if !self.closed {
            self.closed = true;
            return Ok(None);
        }
        pending().await
    }
}

/// The Varlink service of the worker.
#[derive(Clone, Debug)]
pub(crate) struct Service {
    control: Shared,
}

impl Service {
    pub(crate) fn new(control: Shared) -> Self {
        Self { control }
    }
}

/// Sets the frame that the cam worker sends to the host: live video, a slate
/// or blank.
#[expect(
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    reason = "the service macro awaits every method"
)]
#[zlink::service(
    interface = "io.presencemux.Cam",
    vendor = "PresenceMux",
    product = "presencemux-cam",
    version = env!("CARGO_PKG_VERSION"),
    url = "https://github.com/jonas-meyer/presencemux",
    types = [Lease, Permission, Report, DeviceStatus, api::Latch, LatchReason, Rejection, Shown, HostStream]
)]
impl Service {
    /// Accepts or rejects a lease, and returns the report.
    async fn renew(&self, lease: Lease) -> Result<Renewed, Error> {
        let lease = lease_from_wire(lease)?;
        let now = monotonic_now();
        let mut control = self.control.borrow_mut();
        let before = control.choice.latch();
        let rejected = control.choice.renew(lease, now).err();
        if let Some(latch) = control.choice.latch()
            && Some(latch) != before
        {
            tracing::info!("latched: {}, epoch {}", latch.reason, latch.grant.epoch);
        }
        if let Some(rejection) = rejected {
            tracing::debug!("rejected a lease: {rejection}");
        }
        Ok(Renewed {
            report: control.report(now, rejected),
        })
    }

    /// Returns the last accepted lease that did not expire, and the report.
    async fn describe(&self) -> Description {
        let now = monotonic_now();
        let control = self.control.borrow();
        Description {
            lease: control.choice.lease(now).map(lease_to_wire),
            report: control.report(now, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use expect_test::expect_file;
    use presencemux_api::cam::{CamProxy, INTERFACE};
    use tokio::task::spawn_local;
    use zlink::Connection;
    use zlink::varlink_service::Proxy as _;

    use super::*;

    type Client = Connection<Stream>;

    /// The session of the leases in the tests, one second after boot.
    const SESSION_USEC: i64 = 1_000_000;
    /// The group of the socket file in the `permitted` tests.
    const SOCKET_GID: u32 = 997;
    /// The user of presencemuxd in the `permitted` tests.
    const DAEMON_UID: u32 = 998;
    /// A user without the socket group in the `permitted` tests.
    const OTHER_UID: u32 = 1000;
    /// The primary group of `OTHER_UID`.
    const OTHER_GID: u32 = 1000;

    /// Connects a client to a new service through a socket pair. The socket
    /// file has the group `socket_gid`.
    fn connect_with(socket_gid: Option<u32>) -> (Client, Shared) {
        let control: Shared = Rc::new(RefCell::new(Control::new()));
        let (server, client) = UnixStream::pair().unwrap();
        let service = Service::new(Rc::clone(&control));
        spawn_local(serve_connection(server, service, socket_gid));
        let client = Connection::new(Stream::try_from(client).unwrap());
        (client, control)
    }

    /// Connects a client with the group of the socket file to a new service.
    fn connect() -> (Client, Shared) {
        connect_with(Some(rustix::process::getgid().as_raw()))
    }

    /// Returns a lease of `epoch` that expires one second from now.
    fn lease(epoch: i64, permission: Permission, slate: Option<&str>) -> Lease {
        Lease {
            session_usec: SESSION_USEC,
            epoch,
            permission,
            slate: slate.map(str::to_owned),
            until_usec: micros(monotonic_now()) + 1_000_000,
        }
    }

    #[tokio::test(flavor = "local")]
    async fn report_of_the_first_lease_has_the_started_latch() {
        let (mut client, _control) = connect();

        let renewed = client
            .renew(&lease(3, Permission::Live, None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            renewed.report.latched,
            Some(api::Latch {
                session_usec: SESSION_USEC,
                epoch: 3,
                reason: LatchReason::Started,
            })
        );
    }

    #[tokio::test(flavor = "local")]
    async fn lease_with_an_older_epoch_is_rejected_as_old_epoch() {
        let (mut client, _control) = connect();
        client
            .renew(&lease(3, Permission::Blank, None))
            .await
            .unwrap()
            .unwrap();

        let renewed = client
            .renew(&lease(2, Permission::Blank, None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(renewed.report.rejected, Some(Rejection::OldEpoch));
    }

    #[tokio::test(flavor = "local")]
    async fn lease_with_a_negative_epoch_is_an_invalid_lease() {
        let (mut client, _control) = connect();

        let result = client
            .renew(&lease(-1, Permission::Blank, None))
            .await
            .unwrap();

        assert_eq!(result, Err(Error::InvalidLease));
    }

    #[tokio::test(flavor = "local")]
    async fn lease_with_a_negative_expiry_time_is_an_invalid_lease() {
        let (mut client, _control) = connect();
        let negative = Lease {
            until_usec: -1,
            ..lease(0, Permission::Blank, None)
        };

        let result = client.renew(&negative).await.unwrap();

        assert_eq!(result, Err(Error::InvalidLease));
    }

    #[tokio::test(flavor = "local")]
    async fn slate_lease_without_a_name_is_an_invalid_lease() {
        let (mut client, _control) = connect();

        let result = client
            .renew(&lease(0, Permission::Slate, None))
            .await
            .unwrap();

        assert_eq!(result, Err(Error::InvalidLease));
    }

    #[tokio::test(flavor = "local")]
    async fn blank_lease_with_a_slate_name_is_an_invalid_lease() {
        let (mut client, _control) = connect();

        let result = client
            .renew(&lease(0, Permission::Blank, Some("away")))
            .await
            .unwrap();

        assert_eq!(result, Err(Error::InvalidLease));
    }

    #[tokio::test(flavor = "local")]
    async fn unknown_slate_shows_blank_with_a_detail() {
        let (mut client, _control) = connect();

        let renewed = client
            .renew(&lease(0, Permission::Slate, Some("away")))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(renewed.report.shown, Shown::Blank);
        assert_eq!(
            renewed.report.detail.as_deref(),
            Some("no slate named away")
        );
    }

    #[tokio::test(flavor = "local")]
    async fn report_has_the_worker_camera_and_host_status() {
        let (mut client, control) = connect();
        {
            let mut control = control.borrow_mut();
            control.worker = DeviceStatus::Ready;
            control.camera = CameraStatus::Initializing;
            control.streaming = true;
        }

        let renewed = client
            .renew(&lease(0, Permission::Blank, None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            (
                renewed.report.worker,
                renewed.report.camera,
                renewed.report.host
            ),
            (
                DeviceStatus::Ready,
                DeviceStatus::Initializing,
                HostStream::Streaming
            )
        );
    }

    #[tokio::test(flavor = "local")]
    async fn describe_returns_the_accepted_lease() {
        let (mut client, _control) = connect();
        let accepted = lease(0, Permission::Slate, Some("away"));
        client.renew(&accepted).await.unwrap().unwrap();

        let description = client.describe().await.unwrap().unwrap();

        assert_eq!(description.lease, Some(accepted));
    }

    #[tokio::test(flavor = "local")]
    async fn describe_without_a_lease_returns_no_lease() {
        let (mut client, _control) = connect();

        let description = client.describe().await.unwrap().unwrap();

        assert_eq!(description.lease, None);
    }

    #[tokio::test(flavor = "local")]
    async fn lease_with_an_epoch_above_u32_max_is_an_invalid_lease() {
        let (mut client, _control) = connect();

        let result = client
            .renew(&lease(1 << 32, Permission::Blank, None))
            .await
            .unwrap();

        assert_eq!(result, Err(Error::InvalidLease));
    }

    #[tokio::test(flavor = "local")]
    async fn peer_without_the_socket_group_is_refused() {
        let (mut client, _control) = connect_with(Some(u32::MAX - 1));

        let result = client.describe().await;

        assert!(result.is_err());
    }

    #[test]
    fn root_is_permitted() {
        assert!(permitted(0, [0], Some(SOCKET_GID)));
    }

    #[test]
    fn root_is_permitted_if_the_socket_group_is_unknown() {
        assert!(permitted(0, [0], None));
    }

    #[test]
    fn peer_with_the_socket_group_as_primary_group_is_permitted() {
        assert!(permitted(DAEMON_UID, [SOCKET_GID], Some(SOCKET_GID)));
    }

    #[test]
    fn peer_with_the_socket_group_as_supplementary_group_is_permitted() {
        assert!(permitted(
            OTHER_UID,
            [OTHER_GID, SOCKET_GID],
            Some(SOCKET_GID)
        ));
    }

    #[test]
    fn peer_without_the_socket_group_is_not_permitted() {
        assert!(!permitted(OTHER_UID, [OTHER_GID], Some(SOCKET_GID)));
    }

    #[test]
    fn non_root_peer_is_not_permitted_if_the_socket_group_is_unknown() {
        assert!(!permitted(DAEMON_UID, [SOCKET_GID], None));
    }

    #[test]
    fn running_out_of_file_descriptors_is_transient() {
        assert!(is_transient(&io::Error::from_raw_os_error(
            Errno::MFILE.raw_os_error()
        )));
    }

    #[test]
    fn closed_socket_is_not_transient() {
        assert!(!is_transient(&io::Error::from_raw_os_error(
            Errno::BADF.raw_os_error()
        )));
    }

    #[tokio::test(flavor = "local")]
    async fn interface_matches_the_interface_file() {
        let (mut client, _control) = connect();

        let description = client
            .get_interface_description(INTERFACE)
            .await
            .unwrap()
            .unwrap();

        expect_file!["../../../crates/presencemux-api/io.presencemux.Cam.varlink"]
            .assert_eq(description.as_raw().unwrap());
    }
}
