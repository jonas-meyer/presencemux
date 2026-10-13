//! The Varlink connections to presencemuxd. One connection subscribes to the
//! status, and one connection sends the commands.

use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use presencemux_api::{ControllerProxy, Mute};
use tokio::sync::{mpsc, watch};
use tokio::task::spawn_local;
use tokio::time;
use tokio_stream::StreamExt;
use zlink::Connection;

use crate::view::{Command, Presets, Snapshot, Values};

/// The first wait after a failed connection.
const FIRST_WAIT: Duration = Duration::from_millis(50);

/// The longest wait between connections.
const MAX_WAIT: Duration = Duration::from_secs(1);

/// The number of commands that can wait for the connection. If the queue is
/// full, the deck drops the command.
const COMMANDS: usize = 4;

type Client = Connection<zlink::tokio::unix::Stream>;

/// Starts the two connections to the socket of presencemuxd at `socket`.
/// Returns the receiver of the snapshot and the sender of the commands. The
/// snapshot is `None` without a subscription.
pub(crate) fn start(socket: PathBuf) -> (watch::Receiver<Option<Snapshot>>, mpsc::Sender<Command>) {
    let (status, statuses) = watch::channel(None);
    let (commands, queue) = mpsc::channel(COMMANDS);
    spawn_local(subscribe(socket.clone(), status));
    spawn_local(send(socket, queue));
    (statuses, commands)
}

/// An error that ends a subscription or a command.
#[derive(Debug, thiserror::Error)]
enum ClientError {
    #[error("cannot connect: {0}")]
    Connect(zlink::Error),
    #[error("{0}")]
    Transport(zlink::Error),
    #[error("presencemuxd rejected it: {0}")]
    Rejected(presencemux_api::Error),
    #[error("the subscription ended")]
    Ended,
}

impl From<zlink::Error> for ClientError {
    fn from(error: zlink::Error) -> Self {
        Self::Transport(error)
    }
}

impl From<presencemux_api::Error> for ClientError {
    fn from(error: presencemux_api::Error) -> Self {
        Self::Rejected(error)
    }
}

/// Keeps a subscription to the status, and sends each status with the presets
/// to `status`. Sends `None` while the deck has no subscription.
async fn subscribe(socket: PathBuf, status: watch::Sender<Option<Snapshot>>) {
    let mut retry = Retry::default();
    loop {
        let error = match zlink::tokio::unix::connect(&socket).await {
            Ok(mut connection) => statuses(&mut connection, &status, &mut retry).await,
            Err(error) => ClientError::Connect(error),
        };
        status.send_replace(None);
        if status.is_closed() {
            return;
        }
        time::sleep(retry.fail(&error)).await;
    }
}

/// Reads the presets, then sends each status of the subscription on
/// `connection` with the presets to `status`. Returns the error that ends the
/// subscription.
async fn statuses(
    connection: &mut Client,
    status: &watch::Sender<Option<Snapshot>>,
    retry: &mut Retry,
) -> ClientError {
    let presets: Arc<Presets> = match connection.list_presets().await {
        Ok(Ok(presets)) => Arc::new(
            presets
                .presets
                .iter()
                .map(|preset| (preset.name.clone(), Values::from(preset)))
                .collect(),
        ),
        Ok(Err(error)) => return error.into(),
        Err(error) => return error.into(),
    };
    let stream = match connection.subscribe_status().await {
        Ok(stream) => stream,
        Err(error) => return error.into(),
    };
    let mut stream = pin!(stream);
    while let Some(next) = stream.next().await {
        match next {
            Ok(Ok(next)) => {
                retry.succeed();
                status.send_replace(Some(Snapshot {
                    status: next,
                    presets: Arc::clone(&presets),
                }));
            }
            Ok(Err(error)) => return error.into(),
            Err(error) => return error.into(),
        }
    }
    ClientError::Ended
}

/// Sends each command of `queue` to presencemuxd. A command that fails is
/// lost.
async fn send(socket: PathBuf, mut queue: mpsc::Receiver<Command>) {
    let mut connection: Option<Client> = None;
    while let Some(command) = queue.recv().await {
        tracing::info!("command: {command:?}");
        if let Err(error) = deliver(&mut connection, &socket, &command).await {
            tracing::warn!("cannot send {command:?} to presencemuxd: {error}");
        }
    }
}

/// Sends `command` on `connection`, and connects first if there is none.
/// After a transport error on a kept connection, it connects again and sends
/// the command once more.
async fn deliver(
    connection: &mut Option<Client>,
    socket: &Path,
    command: &Command,
) -> Result<(), ClientError> {
    let reused = connection.is_some();
    let result = send_once(connection, socket, command).await;
    // A kept connection is dead after a restart of presencemuxd. Commands are
    // idempotent. A new connection sends the command once more.
    if reused && matches!(result, Err(ClientError::Transport(_))) {
        return send_once(connection, socket, command).await;
    }
    result
}

/// Sends `command` on `connection`, and connects first if there is none. A
/// transport error closes the connection.
async fn send_once(
    connection: &mut Option<Client>,
    socket: &Path,
    command: &Command,
) -> Result<(), ClientError> {
    let client = match connection {
        Some(client) => client,
        None => connection.insert(
            zlink::tokio::unix::connect(socket)
                .await
                .map_err(ClientError::Connect)?,
        ),
    };
    let result = match command {
        Command::ApplyPreset(name) => client.apply_preset(name).await,
        Command::SetCam(value) => client.set_cam(value.as_cam()).await,
        Command::SetMic(mute) => client.set_mic_mute(*mute == Mute::Muted).await,
        Command::Resume(plane) => client.resume(*plane).await,
    };
    match result {
        Ok(reply) => Ok(reply?),
        Err(error) => {
            *connection = None;
            Err(error.into())
        }
    }
}

/// The waits between connections after failures, and the log of the
/// failures. Each wait doubles, from [`FIRST_WAIT`] to [`MAX_WAIT`]. A
/// repeated error goes to the log once.
#[derive(Debug, Default)]
struct Retry {
    next: Option<Duration>,
    last_error: Option<String>,
}

impl Retry {
    /// Records the failure `error`, and returns the wait before the next
    /// connection.
    fn fail(&mut self, error: &ClientError) -> Duration {
        let error = error.to_string();
        if self.last_error.as_ref() != Some(&error) {
            tracing::warn!("no status from presencemuxd: {error}");
            self.last_error = Some(error);
        }
        let wait = self.next.unwrap_or(FIRST_WAIT);
        self.next = Some((wait * 2).min(MAX_WAIT));
        wait
    }

    /// Records a status. The next wait is [`FIRST_WAIT`] again.
    fn succeed(&mut self) {
        self.next = None;
        if self.last_error.take().is_some() {
            tracing::info!("presencemuxd sends the status again");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc as std_mpsc;
    use std::thread;

    use super::*;

    /// The reply to a call that succeeds.
    const OK: &str = "{}";

    /// The reply to a call with an unknown preset.
    const NO_SUCH_PRESET: &str = r#"{"error":"io.presencemux.Controller.NoSuchPreset"}"#;

    /// Starts a fake presencemuxd that listens at `socket`. It accepts
    /// `connections` connections, one after the other, and stops listening
    /// when it accepts the last one. It answers `calls` calls on each
    /// connection, and then closes the connection. The calls get `replies` in
    /// order. Returns the calls, each with the number of its connection.
    fn fake_daemon(
        socket: &Path,
        connections: usize,
        calls: usize,
        replies: &'static [&'static str],
    ) -> std_mpsc::Receiver<(usize, String)> {
        let listener = UnixListener::bind(socket).unwrap();
        let (record, recorded) = std_mpsc::channel();
        thread::spawn(move || {
            let mut listener = Some(listener);
            let mut replies = replies.iter();
            for connection in 0..connections {
                let (stream, _) = listener.as_ref().unwrap().accept().unwrap();
                if connection + 1 == connections {
                    listener = None;
                }
                let mut reader = BufReader::new(stream);
                for _ in 0..calls {
                    let mut call = Vec::new();
                    if reader.read_until(0, &mut call).unwrap() == 0 {
                        break;
                    }
                    call.pop();
                    record
                        .send((connection, String::from_utf8(call).unwrap()))
                        .unwrap();
                    let reply = replies.next().unwrap();
                    reader.get_mut().write_all(reply.as_bytes()).unwrap();
                    reader.get_mut().write_all(b"\0").unwrap();
                }
            }
        });
        recorded
    }

    fn apply_preset(name: &str) -> Command {
        Command::ApplyPreset(name.to_owned())
    }

    #[tokio::test]
    async fn command_after_a_restart_of_presencemuxd_is_sent_once_on_a_new_connection() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");
        let calls = fake_daemon(&socket, 2, 1, &[OK, OK]);
        let mut connection = None;
        deliver(&mut connection, &socket, &apply_preset("privacy"))
            .await
            .unwrap();

        let result = deliver(&mut connection, &socket, &apply_preset("live")).await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            calls.try_iter().collect::<Vec<_>>(),
            [
                (
                    0,
                    r#"{"method":"io.presencemux.Controller.ApplyPreset","parameters":{"name":"privacy"}}"#
                        .to_owned()
                ),
                (
                    1,
                    r#"{"method":"io.presencemux.Controller.ApplyPreset","parameters":{"name":"live"}}"#
                        .to_owned()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn command_without_a_listening_presencemuxd_gives_a_connect_error() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");

        let result = deliver(&mut None, &socket, &apply_preset("privacy")).await;

        assert!(matches!(result, Err(ClientError::Connect(_))), "{result:?}");
    }

    #[tokio::test]
    async fn connect_error_after_a_lost_kept_connection_ends_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");
        let calls = fake_daemon(&socket, 1, 1, &[OK]);
        let mut connection = None;
        deliver(&mut connection, &socket, &apply_preset("privacy"))
            .await
            .unwrap();

        let result = deliver(&mut connection, &socket, &apply_preset("live")).await;

        assert!(matches!(result, Err(ClientError::Connect(_))), "{result:?}");
        assert_eq!(
            calls.try_iter().collect::<Vec<_>>(),
            [(
                0,
                r#"{"method":"io.presencemux.Controller.ApplyPreset","parameters":{"name":"privacy"}}"#
                    .to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn rejected_command_on_a_kept_connection_is_not_sent_again() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");
        let calls = fake_daemon(&socket, 1, 3, &[OK, NO_SUCH_PRESET, OK]);
        let mut connection = None;
        deliver(&mut connection, &socket, &apply_preset("privacy"))
            .await
            .unwrap();

        let result = deliver(&mut connection, &socket, &apply_preset("party")).await;

        assert!(
            matches!(
                result,
                Err(ClientError::Rejected(presencemux_api::Error::NoSuchPreset))
            ),
            "{result:?}"
        );
        assert_eq!(
            calls.try_iter().collect::<Vec<_>>(),
            [
                (
                    0,
                    r#"{"method":"io.presencemux.Controller.ApplyPreset","parameters":{"name":"privacy"}}"#
                        .to_owned()
                ),
                (
                    0,
                    r#"{"method":"io.presencemux.Controller.ApplyPreset","parameters":{"name":"party"}}"#
                        .to_owned()
                ),
            ]
        );
    }

    #[test]
    fn retry_waits_double_from_50_ms_to_1_s() {
        let mut retry = Retry::default();

        let waits: Vec<Duration> = (0..6).map(|_| retry.fail(&ClientError::Ended)).collect();

        assert_eq!(
            waits,
            [50, 100, 200, 400, 800, 1000].map(Duration::from_millis)
        );
    }

    #[test]
    fn retry_after_a_status_waits_50_ms() {
        let mut retry = Retry::default();
        retry.fail(&ClientError::Ended);
        retry.fail(&ClientError::Ended);
        retry.succeed();

        assert_eq!(retry.fail(&ClientError::Ended), Duration::from_millis(50));
    }
}
