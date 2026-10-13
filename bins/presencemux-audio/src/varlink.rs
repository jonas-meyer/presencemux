//! The Varlink service `io.presencemux.Audio`. presencemuxd sends the leases.

use std::cell::RefCell;
use std::convert::Infallible;
use std::future::pending;
use std::io;
use std::mem::{Discriminant, discriminant};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use presencemux_api::audio::{
    self as api, Description, Error, LatchReason, Lease, Permission, Rejection, Renewed, Report,
};
use rustix::io::Errno;
use tokio::net::{UnixListener, UnixStream};
use zlink::connection::Gid;
use zlink::{Connection, Listener, Server};

use crate::clock;
use crate::gate::{self, Gate};
use crate::pipewire::RtState;

/// The gate and the state of the PipeWire thread. The lease checks and the
/// Varlink connections on one thread share it.
pub(crate) type Shared = Rc<RefCell<Control>>;

/// A Varlink stream of the runtime.
type Stream = zlink::tokio::unix::Stream;

const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// The number of generations of the gate word that [`Generations`] keeps.
const KEPT_GENERATIONS: usize = 4;

/// The grants of the last generations of the gate word. A generation starts
/// when the class of the gate state or the grant of its lease changes.
#[derive(Debug)]
struct Generations {
    /// The current generation.
    current: u16,
    /// The class of the gate state and the grant of the current generation.
    key: (Discriminant<gate::State>, Option<api::Grant>),
    /// The generation and its grant, at the index of the generation modulo
    /// [`KEPT_GENERATIONS`].
    kept: [Option<(u16, Option<api::Grant>)>; KEPT_GENERATIONS],
}

impl Default for Generations {
    /// Returns generation 0 of a blocked gate without a grant.
    fn default() -> Self {
        let mut kept = [None; KEPT_GENERATIONS];
        kept[0] = Some((0, None));
        Self {
            current: 0,
            key: (discriminant(&gate::State::Blocked), None),
            kept,
        }
    }
}

impl Generations {
    /// Returns the generation of `state` with `grant`. Starts a new
    /// generation if the class of `state` or `grant` changes. A new open
    /// time with the same grant keeps the generation.
    fn advance(&mut self, state: gate::State, grant: Option<api::Grant>) -> u16 {
        let key = (discriminant(&state), grant);
        if key != self.key {
            self.key = key;
            self.current = self.current.wrapping_add(1);
            self.kept[usize::from(self.current) % KEPT_GENERATIONS] = Some((self.current, grant));
        }
        self.current
    }

    /// Returns the grant of `generation`, or `None` if the generation has no
    /// grant or is not one of the kept generations.
    fn grant(&self, generation: u16) -> Option<api::Grant> {
        match self.kept[usize::from(generation) % KEPT_GENERATIONS] {
            Some((kept, grant)) if kept == generation => grant,
            _ => None,
        }
    }
}

/// The lease and the latch of the gate, and the state of the PipeWire thread.
#[derive(Debug)]
pub(crate) struct Control {
    gate: Gate,
    generations: Generations,
    rt: Arc<RtState>,
}

impl Control {
    pub(crate) fn new(rt: Arc<RtState>) -> Self {
        Self {
            gate: Gate::default(),
            generations: Generations::default(),
            rt,
        }
    }

    /// Latches the gate if its lease expires at or before `now`, and passes
    /// the state of the gate to the real-time thread.
    pub(crate) fn check(&mut self, now: Duration) {
        if let Some(latch) = self.gate.check(now) {
            tracing::info!("latched: {}, epoch {}", latch.reason, latch.grant.epoch);
        }
        self.publish(now);
    }

    /// Passes the state of the gate at `now` and its generation to the
    /// real-time thread.
    fn publish(&mut self, now: Duration) {
        let state = self.gate.state(now);
        let generation = self.generations.advance(state, self.applied(now));
        self.rt.set_gate(state, generation);
    }

    /// Returns the grant of the valid lease at `now`.
    fn applied(&self, now: Duration) -> Option<api::Grant> {
        self.gate.lease(now).map(|lease| api::Grant {
            session_usec: micros(lease.grant.session),
            epoch: i64::from(lease.grant.epoch),
            permission: permission_to_wire(lease.permission),
        })
    }

    /// Returns the report at the time `now`. `rejected` is the rejection of the
    /// lease in this call.
    fn report(&self, now: Duration, rejected: Option<gate::Rejection>) -> Report {
        let cycle = self.rt.last_cycle(now);
        Report {
            gate: self.rt.streams(),
            interface: self.rt.interface(),
            latched: self.gate.latch().map(|latch| api::Latch {
                session_usec: micros(latch.grant.session),
                epoch: i64::from(latch.grant.epoch),
                reason: latch.reason.into(),
            }),
            rejected: rejected.map(rejection_to_wire),
            output: cycle.map_or(api::Output::Silent, |cycle| cycle.output),
            output_grant: cycle.and_then(|cycle| self.generations.grant(cycle.generation)),
            applied: self.applied(now),
        }
    }
}

impl From<gate::Reason> for LatchReason {
    fn from(reason: gate::Reason) -> Self {
        match reason {
            gate::Reason::Expired => Self::Expired,
            gate::Reason::Started => Self::Started,
        }
    }
}

/// Returns `rejection` in the wire types.
fn rejection_to_wire(rejection: gate::Rejection) -> Rejection {
    match rejection {
        gate::Rejection::Late => Rejection::Late,
        gate::Rejection::TooLong => Rejection::TooLong,
        gate::Rejection::OldSession => Rejection::OldSession,
        gate::Rejection::OldEpoch => Rejection::OldEpoch,
    }
}

/// Returns `lease` in the types of the gate, or `InvalidLease` for a time or
/// an epoch out of range.
fn lease_from_wire(lease: Lease) -> Result<gate::Lease, Error> {
    let time = |micros: i64| {
        u64::try_from(micros)
            .map(Duration::from_micros)
            .map_err(|_| Error::InvalidLease)
    };
    Ok(gate::Lease {
        grant: gate::Grant {
            session: time(lease.session_usec)?,
            epoch: u32::try_from(lease.epoch).map_err(|_| Error::InvalidLease)?,
        },
        permission: match lease.permission {
            Permission::Denied => gate::Permission::Denied,
            Permission::Permitted => gate::Permission::Permitted,
        },
        until: time(lease.until_usec)?,
    })
}

/// Returns `lease` in the wire types.
fn lease_to_wire(lease: &gate::Lease) -> Lease {
    Lease {
        session_usec: micros(lease.grant.session),
        epoch: i64::from(lease.grant.epoch),
        permission: permission_to_wire(lease.permission),
        until_usec: micros(lease.until),
    }
}

/// Returns `permission` in the wire types.
fn permission_to_wire(permission: gate::Permission) -> Permission {
    match permission {
        gate::Permission::Denied => Permission::Denied,
        gate::Permission::Permitted => Permission::Permitted,
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

/// Lets the mic gate pass audio to the host, or closes it.
#[expect(
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    reason = "the service macro awaits every method"
)]
#[zlink::service(
    interface = "io.presencemux.Audio",
    vendor = "PresenceMux",
    product = "presencemux-audio",
    version = env!("CARGO_PKG_VERSION"),
    url = "https://github.com/jonas-meyer/presencemux",
    types = [
        Lease,
        Permission,
        Report,
        api::DeviceStatus,
        api::Latch,
        LatchReason,
        Rejection,
        api::Output,
        api::Grant
    ]
)]
impl Service {
    /// Accepts or rejects a lease, and returns the report.
    async fn renew(&self, lease: Lease) -> Result<Renewed, Error> {
        let lease = lease_from_wire(lease)?;
        let now = clock::now();
        let mut control = self.control.borrow_mut();
        let before = control.gate.latch();
        let rejected = control.gate.renew(lease, now).err();
        control.publish(now);
        if let Some(latch) = control.gate.latch()
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
        let now = clock::now();
        let control = self.control.borrow();
        Description {
            lease: control.gate.lease(now).map(lease_to_wire),
            report: control.report(now, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use expect_test::expect_file;
    use presencemux_api::audio::{AudioProxy, DeviceStatus, INTERFACE};
    use tokio::task::spawn_local;
    use zlink::Connection;
    use zlink::varlink_service::Proxy as _;

    use super::*;
    use crate::pipewire;

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
        let control: Shared = Rc::new(RefCell::new(Control::new(Arc::default())));
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

    /// Returns a lease of `epoch` that expires 1 ms from now.
    fn short_lease(epoch: i64, permission: Permission) -> Lease {
        Lease {
            until_usec: micros(clock::now()) + 1_000,
            ..lease(epoch, permission)
        }
    }

    /// Returns the grant of `epoch` in the session of the tests.
    fn grant(epoch: i64, permission: Permission) -> api::Grant {
        api::Grant {
            session_usec: SESSION_USEC,
            epoch,
            permission,
        }
    }

    /// Returns the open state until `seconds` after boot.
    fn open_until(seconds: u64) -> gate::State {
        gate::State::Open {
            until: Duration::from_secs(seconds),
        }
    }

    /// Returns a lease of `epoch` that expires one second from now.
    fn lease(epoch: i64, permission: Permission) -> Lease {
        Lease {
            session_usec: SESSION_USEC,
            epoch,
            permission,
            until_usec: micros(clock::now()) + 1_000_000,
        }
    }

    #[tokio::test(flavor = "local")]
    async fn report_of_the_first_lease_has_the_started_latch() {
        let (mut client, _control) = connect();

        let renewed = client
            .renew(&lease(3, Permission::Permitted))
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
    async fn first_permitted_lease_blocks_the_gate_on_the_real_time_thread() {
        let (mut client, control) = connect();

        client
            .renew(&lease(3, Permission::Permitted))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(control.borrow().rt.gate_state(), pipewire::BLOCKED);
    }

    #[tokio::test(flavor = "local")]
    async fn report_of_a_valid_lease_applies_its_grant() {
        let (mut client, _control) = connect();
        client
            .renew(&lease(0, Permission::Denied))
            .await
            .unwrap()
            .unwrap();

        let renewed = client
            .renew(&lease(1, Permission::Permitted))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            renewed.report.applied,
            Some(api::Grant {
                session_usec: SESSION_USEC,
                epoch: 1,
                permission: Permission::Permitted,
            })
        );
    }

    #[tokio::test(flavor = "local")]
    async fn gate_latched_by_its_first_lease_applies_the_grant_of_the_lease() {
        let (mut client, _control) = connect();

        let renewed = client
            .renew(&lease(3, Permission::Permitted))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            renewed.report.applied,
            Some(api::Grant {
                session_usec: SESSION_USEC,
                epoch: 3,
                permission: Permission::Permitted,
            })
        );
    }

    #[tokio::test(flavor = "local")]
    async fn report_without_a_lease_applies_no_grant() {
        let (mut client, _control) = connect();

        let description = client.describe().await.unwrap().unwrap();

        assert_eq!(description.report.applied, None);
    }

    #[tokio::test(flavor = "local")]
    async fn report_without_a_gate_cycle_is_silent() {
        let (mut client, _control) = connect();

        let renewed = client
            .renew(&lease(0, Permission::Permitted))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(renewed.report.output, api::Output::Silent);
    }

    #[test]
    fn new_generations_are_at_generation_0() {
        let generations = Generations::default();

        assert_eq!(generations.current, 0);
    }

    #[test]
    fn renewal_that_only_moves_the_open_time_keeps_the_generation() {
        let mut generations = Generations::default();
        generations.advance(open_until(7), Some(grant(1, Permission::Permitted)));

        let generation = generations.advance(open_until(8), Some(grant(1, Permission::Permitted)));

        assert_eq!(generation, 1);
    }

    #[test]
    fn new_grant_starts_a_new_generation() {
        let mut generations = Generations::default();
        generations.advance(open_until(7), Some(grant(1, Permission::Permitted)));

        let generation = generations.advance(open_until(7), Some(grant(2, Permission::Permitted)));

        assert_eq!(generation, 2);
    }

    #[test]
    fn new_class_with_the_same_grant_starts_a_new_generation() {
        let mut generations = Generations::default();
        generations.advance(open_until(7), Some(grant(1, Permission::Permitted)));

        let generation =
            generations.advance(gate::State::Blocked, Some(grant(1, Permission::Permitted)));

        assert_eq!(generation, 2);
    }

    #[test]
    fn kept_generation_gives_its_grant() {
        let mut generations = Generations::default();
        generations.advance(open_until(7), Some(grant(1, Permission::Permitted)));
        generations.advance(gate::State::Denied, Some(grant(2, Permission::Denied)));

        assert_eq!(generations.grant(1), Some(grant(1, Permission::Permitted)));
    }

    #[test]
    fn generation_4_changes_old_gives_no_grant() {
        let mut generations = Generations::default();
        generations.advance(gate::State::Denied, Some(grant(1, Permission::Denied)));
        generations.advance(gate::State::Denied, Some(grant(2, Permission::Denied)));
        generations.advance(gate::State::Denied, Some(grant(3, Permission::Denied)));
        generations.advance(gate::State::Denied, Some(grant(4, Permission::Denied)));
        generations.advance(gate::State::Denied, Some(grant(5, Permission::Denied)));

        assert_eq!(generations.grant(1), None);
    }

    #[test]
    fn generation_after_the_current_one_gives_no_grant() {
        let mut generations = Generations::default();
        generations.advance(gate::State::Denied, Some(grant(1, Permission::Denied)));

        assert_eq!(generations.grant(5), None);
    }

    #[test]
    fn generation_wraps_after_65535() {
        let mut generations = Generations {
            current: u16::MAX,
            ..Generations::default()
        };

        let generation =
            generations.advance(gate::State::Denied, Some(grant(1, Permission::Denied)));

        assert_eq!(generation, 0);
    }

    #[tokio::test(flavor = "local")]
    async fn renewal_that_only_moves_the_expiry_keeps_the_generation_on_the_real_time_thread() {
        let (mut client, control) = connect();
        client
            .renew(&lease(0, Permission::Denied))
            .await
            .unwrap()
            .unwrap();
        client
            .renew(&lease(1, Permission::Permitted))
            .await
            .unwrap()
            .unwrap();
        let first = control.borrow().rt.gate_generation();

        client
            .renew(&lease(1, Permission::Permitted))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(control.borrow().rt.gate_generation(), first);
    }

    #[tokio::test(flavor = "local")]
    async fn report_after_a_denied_lease_gives_the_permitted_grant_of_the_passing_cycle() {
        let (mut client, control) = connect();
        client
            .renew(&lease(0, Permission::Denied))
            .await
            .unwrap()
            .unwrap();
        client
            .renew(&lease(1, Permission::Permitted))
            .await
            .unwrap()
            .unwrap();
        {
            let rt = &control.borrow().rt;
            rt.publish_cycle(clock::now_nanos(), rt.gate_word(), api::Output::Passing);
        }

        let renewed = client
            .renew(&lease(2, Permission::Denied))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            (
                renewed.report.output,
                renewed.report.output_grant,
                renewed.report.applied
            ),
            (
                api::Output::Passing,
                Some(grant(1, Permission::Permitted)),
                Some(grant(2, Permission::Denied))
            )
        );
    }

    #[tokio::test(flavor = "local")]
    async fn cycle_of_an_unknown_generation_gives_no_output_grant() {
        let (mut client, control) = connect();
        client
            .renew(&lease(0, Permission::Denied))
            .await
            .unwrap()
            .unwrap();
        control.borrow().rt.publish_cycle(
            clock::now_nanos(),
            pipewire::gate_word(gate::State::Denied, 5),
            api::Output::Closing,
        );

        let description = client.describe().await.unwrap().unwrap();

        assert_eq!(description.report.output_grant, None);
    }

    #[tokio::test(flavor = "local")]
    async fn report_of_a_passing_cycle_100_ms_old_is_silent() {
        let (mut client, control) = connect();
        client
            .renew(&lease(0, Permission::Denied))
            .await
            .unwrap()
            .unwrap();
        {
            let rt = &control.borrow().rt;
            rt.publish_cycle(
                clock::now_nanos() - 100_000_000,
                rt.gate_word(),
                api::Output::Passing,
            );
        }

        let description = client.describe().await.unwrap().unwrap();

        assert_eq!(description.report.output, api::Output::Silent);
    }

    #[tokio::test(flavor = "local")]
    async fn report_of_a_passing_cycle_100_ms_old_gives_no_output_grant() {
        let (mut client, control) = connect();
        client
            .renew(&lease(0, Permission::Denied))
            .await
            .unwrap()
            .unwrap();
        {
            let rt = &control.borrow().rt;
            rt.publish_cycle(
                clock::now_nanos() - 100_000_000,
                rt.gate_word(),
                api::Output::Passing,
            );
        }

        let description = client.describe().await.unwrap().unwrap();

        assert_eq!(description.report.output_grant, None);
    }

    #[tokio::test(flavor = "local")]
    async fn permitted_lease_passes_its_expiry_time_to_the_real_time_thread() {
        let (mut client, control) = connect();
        client
            .renew(&lease(0, Permission::Denied))
            .await
            .unwrap()
            .unwrap();
        let permitted = lease(1, Permission::Permitted);
        let until_micros = u64::try_from(permitted.until_usec).unwrap();

        client.renew(&permitted).await.unwrap().unwrap();

        assert_eq!(control.borrow().rt.gate_state(), until_micros);
    }

    #[tokio::test(flavor = "local")]
    async fn lease_after_the_expiry_reports_the_expired_latch() {
        let (mut client, _control) = connect();
        client
            .renew(&short_lease(0, Permission::Denied))
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;

        let renewed = client
            .renew(&lease(0, Permission::Denied))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            renewed.report.latched.map(|latch| latch.reason),
            Some(LatchReason::Expired)
        );
    }

    #[tokio::test(flavor = "local")]
    async fn describe_after_the_expiry_returns_no_lease() {
        let (mut client, _control) = connect();
        client
            .renew(&short_lease(0, Permission::Denied))
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;

        let description = client.describe().await.unwrap().unwrap();

        assert_eq!(description.lease, None);
    }

    #[tokio::test(flavor = "local")]
    async fn lease_more_than_2_s_ahead_is_rejected_as_too_long() {
        let (mut client, _control) = connect();
        let long = Lease {
            until_usec: micros(clock::now()) + 3_000_000,
            ..lease(0, Permission::Denied)
        };

        let renewed = client.renew(&long).await.unwrap().unwrap();

        assert_eq!(renewed.report.rejected, Some(Rejection::TooLong));
    }

    #[tokio::test(flavor = "local")]
    async fn check_after_the_expiry_blocks_the_gate_on_the_real_time_thread() {
        let (mut client, control) = connect();
        client
            .renew(&lease(0, Permission::Denied))
            .await
            .unwrap()
            .unwrap();
        let permitted = lease(1, Permission::Permitted);
        client.renew(&permitted).await.unwrap().unwrap();
        let expiry = Duration::from_micros(u64::try_from(permitted.until_usec).unwrap());

        control.borrow_mut().check(expiry);

        assert_eq!(control.borrow().rt.gate_state(), pipewire::BLOCKED);
    }

    #[tokio::test(flavor = "local")]
    async fn denied_lease_closes_the_gate_on_the_real_time_thread() {
        let (mut client, control) = connect();
        client
            .renew(&lease(0, Permission::Denied))
            .await
            .unwrap()
            .unwrap();
        client
            .renew(&lease(1, Permission::Permitted))
            .await
            .unwrap()
            .unwrap();

        client
            .renew(&lease(2, Permission::Denied))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(control.borrow().rt.gate_state(), pipewire::DENIED);
    }

    #[tokio::test(flavor = "local")]
    async fn lease_with_an_older_epoch_is_rejected_as_old_epoch() {
        let (mut client, _control) = connect();
        client
            .renew(&lease(3, Permission::Denied))
            .await
            .unwrap()
            .unwrap();

        let renewed = client
            .renew(&lease(2, Permission::Denied))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(renewed.report.rejected, Some(Rejection::OldEpoch));
    }

    #[tokio::test(flavor = "local")]
    async fn lease_with_a_negative_epoch_is_an_invalid_lease() {
        let (mut client, _control) = connect();

        let result = client.renew(&lease(-1, Permission::Denied)).await.unwrap();

        assert_eq!(result, Err(Error::InvalidLease));
    }

    #[tokio::test(flavor = "local")]
    async fn lease_with_a_negative_expiry_time_is_an_invalid_lease() {
        let (mut client, _control) = connect();
        let negative = Lease {
            until_usec: -1,
            ..lease(0, Permission::Denied)
        };

        let result = client.renew(&negative).await.unwrap();

        assert_eq!(result, Err(Error::InvalidLease));
    }

    #[tokio::test(flavor = "local")]
    async fn lease_with_an_epoch_above_u32_max_is_an_invalid_lease() {
        let (mut client, _control) = connect();

        let result = client
            .renew(&lease(1 << 32, Permission::Denied))
            .await
            .unwrap();

        assert_eq!(result, Err(Error::InvalidLease));
    }

    #[tokio::test(flavor = "local")]
    async fn report_of_a_new_worker_has_a_missing_interface_and_an_initializing_gate() {
        let (mut client, _control) = connect();

        let renewed = client
            .renew(&lease(0, Permission::Denied))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            (renewed.report.gate, renewed.report.interface),
            (DeviceStatus::Initializing, DeviceStatus::Missing)
        );
    }

    #[tokio::test(flavor = "local")]
    async fn describe_returns_the_accepted_lease() {
        let (mut client, _control) = connect();
        let accepted = lease(0, Permission::Permitted);
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

        expect_file!["../../../crates/presencemux-api/io.presencemux.Audio.varlink"]
            .assert_eq(description.as_raw().unwrap());
    }
}
