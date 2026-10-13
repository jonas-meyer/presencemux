//! The search for the hidraw node of the deck with udev.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use tokio::io::unix::AsyncFd;

/// The udev tag that the rule of the package gives the hidraw node of the
/// deck.
pub(crate) const TAG: &str = "presencemux-deck";

/// The subsystem of the node.
const SUBSYSTEM: &str = "hidraw";

/// The node of the deck, kept current by udev events.
pub(crate) struct Discovery {
    socket: AsyncFd<udev::MonitorSocket>,
    node: Option<PathBuf>,
}

impl Discovery {
    /// Starts the udev monitor and finds the node, if it exists.
    pub(crate) fn new() -> io::Result<Self> {
        // The monitor starts before the enumeration. A node that appears
        // during the enumeration then gives an event.
        let monitor = udev::MonitorBuilder::new()?
            .match_subsystem(SUBSYSTEM)?
            .match_tag(TAG)?;
        let mut discovery = Self {
            socket: AsyncFd::new(monitor.listen()?)?,
            node: None,
        };
        record(&mut discovery.node, find()?.as_deref());
        Ok(discovery)
    }

    /// Returns the path of the node, if it exists.
    pub(crate) fn node(&self) -> Option<&Path> {
        self.node.as_deref()
    }

    /// Waits for udev events. Returns when the path of the node changes.
    pub(crate) async fn changed(&mut self) -> io::Result<()> {
        loop {
            let mut guard = self.socket.readable().await?;
            let mut changed = false;
            loop {
                // The iterator ends at each message that libudev cannot give.
                // The socket can still hold more messages.
                let socket = guard.get_inner();
                for event in socket.iter() {
                    changed |= apply(&mut self.node, &event);
                }
                // libudev reports errors only through errno. An errno from an
                // earlier call causes one more search.
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(Errno::NOBUFS.raw_os_error()) {
                    tracing::warn!("the udev socket lost events, the deck searches again");
                    changed |= record(&mut self.node, find()?.as_deref());
                }
                if !has_messages(guard.get_inner())? {
                    guard.clear_ready();
                    break;
                }
            }
            if changed {
                return Ok(());
            }
        }
    }
}

/// Returns the path of the node that exists, if any.
pub(crate) fn find() -> io::Result<Option<PathBuf>> {
    let mut enumerator = udev::Enumerator::new()?;
    enumerator.match_subsystem(SUBSYSTEM)?;
    enumerator.match_tag(TAG)?;
    enumerator.match_is_initialized()?;
    Ok(enumerator
        .scan_devices()?
        .find_map(|device| device.devnode().map(Path::to_owned)))
}

/// Applies one udev event to `node`. Returns whether the path changed.
fn apply(node: &mut Option<PathBuf>, event: &udev::Event) -> bool {
    let Some(path) = event.devnode() else {
        return false;
    };
    let ours = node.as_deref() == Some(path);
    match event.event_type() {
        udev::EventType::Add | udev::EventType::Change if tagged(event) => record(node, Some(path)),
        // A changed rule can remove the tag of a node.
        udev::EventType::Add | udev::EventType::Change | udev::EventType::Remove if ours => {
            record(node, None)
        }
        _ => false,
    }
}

/// Sets `node` to `path` and logs a change. Returns whether the path changed.
fn record(node: &mut Option<PathBuf>, path: Option<&Path>) -> bool {
    if node.as_deref() == path {
        return false;
    }
    match (path, node.as_deref()) {
        (Some(path), _) => tracing::info!("found the deck at {}", path.display()),
        (None, Some(old)) => tracing::info!("lost the deck at {}", old.display()),
        (None, None) => {}
    }
    *node = path.map(Path::to_owned);
    true
}

/// Returns whether the udev tags of `device` include [`TAG`]. The tag list
/// has the form `:tag:tag:`.
fn tagged(device: &udev::Device) -> bool {
    device
        .property_value("CURRENT_TAGS")
        .and_then(OsStr::to_str)
        .is_some_and(has_tag)
}

/// Returns whether the udev tag list `tags` includes [`TAG`].
fn has_tag(tags: &str) -> bool {
    tags.split(':').any(|tag| tag == TAG)
}

/// Returns whether `socket` holds a message, without waiting.
fn has_messages(socket: &udev::MonitorSocket) -> io::Result<bool> {
    let mut fds = [PollFd::new(socket, PollFlags::IN)];
    rustix::event::poll(&mut fds, Some(&Timespec::default()))?;
    Ok(fds[0].revents().contains(PollFlags::IN))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = include_str!("../debian/70-presencemux-deck.rules");

    #[test]
    fn tag_list_with_the_deck_tag_has_the_tag() {
        assert!(has_tag(":seat:presencemux-deck:uaccess:"));
    }

    #[test]
    fn tag_list_without_the_deck_tag_has_no_tag() {
        assert!(!has_tag(":seat:uaccess:"));
    }

    #[test]
    fn rules_tag_the_node_with_the_tag_of_the_code() {
        assert!(RULES.contains(&format!("TAG+=\"{TAG}\"")));
    }

    #[test]
    fn new_path_is_a_change() {
        let mut node = None;

        assert!(record(&mut node, Some(Path::new("/dev/hidraw0"))));
    }

    #[test]
    fn same_path_is_no_change() {
        let mut node = Some(PathBuf::from("/dev/hidraw0"));

        assert!(!record(&mut node, Some(Path::new("/dev/hidraw0"))));
    }
}
