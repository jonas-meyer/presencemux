//! The Varlink service `io.presencemux.Controller`.

use std::convert::Infallible;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use presencemux_api::{
    CamStatus, Cause, Error, Health, Host, MicStatus, Mute, Permission, Presets, Status,
};
use presencemux_core::{MicMute, Preset};
use rustix::io::Errno;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, watch};
use tokio::time;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::{Stream, StreamExt};
use zlink::{ReadyListener, Reply, Server};

use crate::config::Config;
use crate::event_loop::{Handle, HandleError, Request};

/// The number of clients that the daemon serves at the same time. Further
/// clients wait in the socket backlog. The limit keeps file descriptors free
/// for the daemon itself, for example for the watchdog ping.
pub(crate) const MAX_CONNECTIONS: usize = 256;

const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// The state that every connection shares.
#[derive(Clone, Debug)]
pub(crate) struct Service {
    handle: Handle,
    status: watch::Receiver<Status>,
    config: Arc<Config>,
}

/// Serves each connection with its own server, up to `max_connections` at a
/// time. A client that does not read its replies blocks its own connection and
/// no other. Returns when the socket fails.
pub(crate) async fn serve(
    listener: UnixListener,
    service: Service,
    max_connections: usize,
) -> io::Result<Infallible> {
    let slots = Arc::new(Semaphore::new(max_connections));
    loop {
        let slot = Arc::clone(&slots)
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        match listener.accept().await {
            Ok((stream, _)) => {
                let service = service.clone();
                tokio::task::spawn_local(async move {
                    serve_connection(stream, service).await;
                    drop(slot);
                });
            }
            Err(error) if is_transient(&error) => {
                tracing::warn!("cannot accept a Varlink connection, trying again: {error}");
                time::sleep(ACCEPT_RETRY).await;
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

async fn serve_connection(stream: UnixStream, service: Service) {
    let result = match zlink::tokio::unix::Stream::try_from(stream) {
        Ok(stream) => Server::new(ReadyListener::new(stream), service).run().await,
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        tracing::debug!("Varlink connection failed: {error}");
    }
}

impl Service {
    pub(crate) fn new(
        handle: Handle,
        status: watch::Receiver<Status>,
        config: Arc<Config>,
    ) -> Self {
        Self {
            handle,
            status,
            config,
        }
    }

    async fn request(&self, request: Request) -> Result<(), Error> {
        self.handle
            .request(request)
            .await
            .map_err(|error| match error {
                HandleError::Busy => Error::TooManyRequests,
                HandleError::Stopped => Error::NotAvailable,
            })
    }
}

/// Controls the privacy of the camera and the microphone.
#[expect(
    clippy::unused_async,
    clippy::unused_async_trait_impl,
    reason = "the service macro awaits every method"
)]
#[zlink::service(
    interface = "io.presencemux.Controller",
    vendor = "PresenceMux",
    product = "presencemuxd",
    version = env!("CARGO_PKG_VERSION"),
    url = "https://github.com/jonas-meyer/presencemux",
    types = [CamStatus, MicStatus, Mute, Permission, Host, Health, Cause]
)]
impl Service {
    /// Returns the status.
    async fn describe(&self) -> Status {
        self.status.borrow().clone()
    }

    /// Returns the status, then the status after each change. Without `more`,
    /// it returns the status once.
    #[zlink(more)]
    async fn subscribe_status(&self, more: bool) -> impl Stream<Item = Reply<Status>> + Unpin {
        let count = if more { usize::MAX } else { 1 };
        WatchStream::new(self.status.clone())
            .take(count)
            .map(move |status| Reply::new(Some(status)).set_continues(Some(more)))
    }

    /// Applies `privacy` or a configured preset.
    async fn apply_preset(&self, name: &str) -> Result<(), Error> {
        let (name, preset) = self.config.preset(name).map_err(|_| Error::NoSuchPreset)?;
        self.request(Request {
            preset,
            name: Some(name),
        })
        .await
    }

    /// Sets the cam to `blank`, `live` or a slate name.
    async fn set_cam(&self, cam: &str) -> Result<(), Error> {
        let cam = self
            .config
            .slates
            .parse_cam(cam)
            .map_err(|_| Error::NoSuchSlate)?;
        self.request(Request {
            preset: Preset {
                cam: Some(cam),
                mic: None,
            },
            name: None,
        })
        .await
    }

    /// Mutes or unmutes the mic.
    async fn set_mic_mute(&self, muted: bool) -> Result<(), Error> {
        let mute = if muted {
            MicMute::Muted
        } else {
            MicMute::Unmuted
        };
        self.request(Request {
            preset: Preset {
                cam: None,
                mic: Some(mute),
            },
            name: None,
        })
        .await
    }

    /// Returns the names of the configured presets. The list does not include
    /// the built-in `privacy`.
    async fn list_presets(&self) -> Presets {
        Presets {
            presets: self.config.preset_names().map(str::to_owned).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use expect_test::expect_file;
    use presencemux_api::{ControllerProxy, INTERFACE};
    use tokio::task::spawn_local;
    use tokio::time;
    use zlink::Connection;
    use zlink::varlink_service::Proxy as _;

    use super::*;
    use crate::event_loop::EventLoop;

    type Client = Connection<zlink::tokio::unix::Stream>;

    fn start(config: &str) -> Service {
        let config = Arc::new(Config::parse(config).unwrap());
        let (event_loop, handle, outputs) = EventLoop::new(Arc::clone(&config));
        spawn_local(event_loop.run());
        Service::new(handle, outputs.status, config)
    }

    /// Connects a client through the same path as a connection from the
    /// socket.
    fn connect(service: &Service) -> Client {
        let (server, client) = UnixStream::pair().unwrap();
        spawn_local(serve_connection(server, service.clone()));
        Connection::new(zlink::tokio::unix::Stream::try_from(client).unwrap())
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn describe_returns_the_current_status() {
        let service = start("");
        let mut client = connect(&service);

        let status = client.describe().await.unwrap().unwrap();

        assert_eq!(status, *service.status.borrow());
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn set_mic_mute_changes_the_desired_mic() {
        let mut client = connect(&start(""));

        client.set_mic_mute(false).await.unwrap().unwrap();

        let status = client.describe().await.unwrap().unwrap();
        assert_eq!(status.mic.desired, Mute::Unmuted);
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn set_cam_accepts_a_slate_name() {
        let mut client = connect(&start("[slates.brb]\n"));

        client.set_cam("brb").await.unwrap().unwrap();

        let status = client.describe().await.unwrap().unwrap();
        assert_eq!(status.cam.desired, "brb");
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn apply_preset_shows_the_preset_name() {
        let mut client = connect(&start("[presets.talk]\nmic = \"unmuted\"\n"));

        client.apply_preset("talk").await.unwrap().unwrap();

        let status = client.describe().await.unwrap().unwrap();
        assert_eq!(status.preset.as_deref(), Some("talk"));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn unknown_preset_is_no_such_preset() {
        let mut client = connect(&start(""));

        let result = client.apply_preset("talk").await.unwrap();

        assert_eq!(result, Err(Error::NoSuchPreset));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn unknown_cam_value_is_no_such_slate() {
        let mut client = connect(&start(""));

        let result = client.set_cam("brb").await.unwrap();

        assert_eq!(result, Err(Error::NoSuchSlate));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn list_presets_returns_the_configured_names() {
        let mut client = connect(&start(
            "[presets.talk]\nmic = \"unmuted\"\n\n[presets.brb]\nmic = \"muted\"\n",
        ));

        let presets = client.list_presets().await.unwrap().unwrap();

        assert_eq!(presets.presets, ["brb", "talk"]);
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn subscription_starts_with_the_current_status() {
        let service = start("");
        let mut client = connect(&service);

        let mut statuses = std::pin::pin!(client.subscribe_status().await.unwrap());
        let first = statuses.next().await.unwrap().unwrap().unwrap();

        assert_eq!(first, *service.status.borrow());
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn subscription_sends_each_change() {
        let service = start("");
        let mut subscriber = connect(&service);
        let mut other = connect(&service);
        let mut statuses = std::pin::pin!(subscriber.subscribe_status().await.unwrap());
        statuses.next().await.unwrap().unwrap().unwrap();

        other.set_mic_mute(false).await.unwrap().unwrap();

        let next = statuses.next().await.unwrap().unwrap().unwrap();
        assert_eq!(next.mic.desired, Mute::Unmuted);
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn stuck_subscriber_does_not_block_other_clients() {
        let service = start("");
        let mut stuck = connect(&service);
        let mut other = connect(&service);
        let _unread = stuck.subscribe_status().await.unwrap();

        // The socket buffer of the stuck subscriber cannot hold this many changes.
        let changes = async {
            for change in 0..1000 {
                other.set_mic_mute(change % 2 == 0).await.unwrap().unwrap();
            }
        };

        time::timeout(Duration::from_secs(3600), changes)
            .await
            .expect("the other client is not blocked");
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn a_connection_over_the_limit_waits_for_a_free_slot() {
        let service = start("");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control");
        spawn_local(serve(UnixListener::bind(&path).unwrap(), service, 1));
        let mut first = zlink::tokio::unix::connect(&path).await.unwrap();
        first.describe().await.unwrap().unwrap();
        let second = spawn_local(async move {
            let mut second = zlink::tokio::unix::connect(&path).await.unwrap();
            second.describe().await.unwrap().unwrap()
        });

        time::sleep(Duration::from_secs(60)).await;
        assert!(!second.is_finished());
        drop(first);

        time::timeout(Duration::from_secs(60), second)
            .await
            .expect("the second client is served")
            .unwrap();
    }

    #[test]
    fn running_out_of_file_descriptors_is_transient() {
        assert!(is_transient(&io::Error::from_raw_os_error(
            Errno::MFILE.raw_os_error()
        )));
    }

    #[test]
    fn a_closed_socket_is_not_transient() {
        assert!(!is_transient(&io::Error::from_raw_os_error(
            Errno::BADF.raw_os_error()
        )));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn request_to_a_stopped_event_loop_is_not_available() {
        let config = Arc::new(Config::parse("").unwrap());
        let (event_loop, handle, outputs) = EventLoop::new(Arc::clone(&config));
        drop(event_loop);
        let mut client = connect(&Service::new(handle, outputs.status, config));

        let result = client.set_mic_mute(false).await.unwrap();

        assert_eq!(result, Err(Error::NotAvailable));
    }

    #[tokio::test(flavor = "local", start_paused = true)]
    async fn interface_matches_the_interface_file() {
        let mut client = connect(&start(""));

        let description = client
            .get_interface_description(INTERFACE)
            .await
            .unwrap()
            .unwrap();

        expect_file!["../../../crates/presencemux-api/io.presencemux.Controller.varlink"]
            .assert_eq(description.as_raw().unwrap());
    }
}
