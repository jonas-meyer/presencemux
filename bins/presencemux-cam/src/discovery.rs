//! The search for the gadget node and the camera node with udev.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use tokio::io::unix::AsyncFd;

/// A V4L2 node that the worker uses. The udev rule of the package tags each
/// node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Node {
    /// The output node of the UVC gadget function.
    Gadget,
    /// The capture node of the camera.
    Camera,
}

impl Node {
    const ALL: [Self; 2] = [Self::Gadget, Self::Camera];

    /// Returns the udev tag of the node.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Gadget => "presencemux-gadget",
            Self::Camera => "presencemux-camera",
        }
    }

    /// Returns the node that the udev tag list `tags` names, if any. The list
    /// has the form `:tag:tag:`.
    pub(crate) fn tagged(tags: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|node| tags.split(':').any(|tag| tag == node.tag()))
    }
}

/// One appearance of the camera node. A camera that udev removes and adds
/// again at the same path gets a new generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CameraNode {
    pub(crate) path: PathBuf,
    pub(crate) generation: u64,
}

/// The paths of the gadget node and the camera node.
#[derive(Debug, Default)]
pub(crate) struct Nodes {
    gadget: Option<PathBuf>,
    camera: Option<CameraNode>,
    /// The generation of the last camera node.
    generation: u64,
}

impl Nodes {
    pub(crate) fn gadget(&self) -> Option<&Path> {
        self.gadget.as_deref()
    }

    pub(crate) fn camera(&self) -> Option<&CameraNode> {
        self.camera.as_ref()
    }

    /// Returns the path of `node`, if it exists.
    pub(crate) fn path(&self, node: Node) -> Option<&Path> {
        match node {
            Node::Gadget => self.gadget(),
            Node::Camera => self.camera().map(|camera| camera.path.as_path()),
        }
    }

    /// Sets the path of `node`, or clears it with `None`. Returns whether the
    /// path changed.
    pub(crate) fn set(&mut self, node: Node, path: Option<&Path>) -> bool {
        match node {
            Node::Gadget => {
                if self.gadget.as_deref() == path {
                    return false;
                }
                self.gadget = path.map(Path::to_owned);
            }
            Node::Camera => {
                if self.path(Node::Camera) == path {
                    return false;
                }
                self.generation += 1;
                self.camera = path.map(|path| CameraNode {
                    path: path.to_owned(),
                    generation: self.generation,
                });
            }
        }
        true
    }

    /// Returns the node at `path`, if any.
    pub(crate) fn node_at(&self, path: &Path) -> Option<Node> {
        [Node::Gadget, Node::Camera]
            .into_iter()
            .find(|&node| self.path(node) == Some(path))
    }
}

/// The nodes of the worker, kept current by udev events.
pub(crate) struct Discovery {
    socket: AsyncFd<udev::MonitorSocket>,
    nodes: Nodes,
}

impl Discovery {
    /// Starts the udev monitor and finds the nodes that exist.
    pub(crate) fn new() -> io::Result<Self> {
        // The monitor starts before the enumeration. A node that appears
        // during the enumeration then gives an event.
        let mut monitor = udev::MonitorBuilder::new()?.match_subsystem("video4linux")?;
        for node in Node::ALL {
            monitor = monitor.match_tag(node.tag())?;
        }
        let mut discovery = Self {
            socket: AsyncFd::new(monitor.listen()?)?,
            nodes: Nodes::default(),
        };
        scan(&mut discovery.nodes)?;
        Ok(discovery)
    }

    pub(crate) fn nodes(&self) -> &Nodes {
        &self.nodes
    }

    /// Waits for udev events. Returns when the path of a node changes.
    pub(crate) async fn changed(&mut self) -> io::Result<()> {
        loop {
            let mut guard = self.socket.readable().await?;
            let mut changed = false;
            loop {
                // The iterator ends at each message that libudev cannot give.
                // The socket can still hold more messages.
                let socket = guard.get_inner();
                for event in socket.iter() {
                    changed |= apply(&mut self.nodes, &event);
                }
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(Errno::NOBUFS.raw_os_error()) {
                    tracing::warn!("the udev socket lost events, the worker searches again");
                    changed |= scan(&mut self.nodes)?;
                }
                if !has_messages(socket)? {
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

/// Applies one udev event to `nodes`. Returns whether a path changed.
fn apply(nodes: &mut Nodes, event: &udev::Event) -> bool {
    let Some(path) = event.devnode() else {
        return false;
    };
    match event.event_type() {
        udev::EventType::Add | udev::EventType::Change => match tagged(event) {
            Some(node) => record(nodes, node, Some(path), serial(event)),
            // A changed rule can remove the tag of a node.
            None => nodes
                .node_at(path)
                .is_some_and(|node| record(nodes, node, None, None)),
        },
        udev::EventType::Remove => nodes
            .node_at(path)
            .is_some_and(|node| record(nodes, node, None, None)),
        _ => false,
    }
}

/// Sets `nodes` to the nodes that exist. Returns whether a path changed.
fn scan(nodes: &mut Nodes) -> io::Result<bool> {
    let mut changed = false;
    for node in Node::ALL {
        let mut enumerator = udev::Enumerator::new()?;
        enumerator.match_subsystem("video4linux")?;
        enumerator.match_tag(node.tag())?;
        enumerator.match_is_initialized()?;
        let found = enumerator.scan_devices()?.find_map(|device| {
            let path = device.devnode()?.to_owned();
            Some((path, serial(&device).map(str::to_owned)))
        });
        let (path, serial) = found.unzip();
        changed |= record(nodes, node, path.as_deref(), serial.flatten().as_deref());
    }
    Ok(changed)
}

/// Sets the path of `node` and logs a change. Returns whether the path
/// changed.
fn record(nodes: &mut Nodes, node: Node, path: Option<&Path>, serial: Option<&str>) -> bool {
    let old = nodes.path(node).map(Path::to_owned);
    if !nodes.set(node, path) {
        return false;
    }
    match (node, path, old) {
        (Node::Gadget, Some(path), _) => {
            tracing::info!("found the gadget node at {}", path.display());
        }
        (Node::Camera, Some(path), _) => tracing::info!(
            "found the camera at {}, serial {}",
            path.display(),
            serial.unwrap_or("unknown")
        ),
        (Node::Gadget, None, Some(old)) => {
            tracing::info!("lost the gadget node at {}", old.display());
        }
        (Node::Camera, None, Some(old)) => {
            tracing::info!("lost the camera at {}", old.display());
        }
        (_, None, None) => {}
    }
    true
}

/// Returns the node that the udev tags of the device name, if any.
fn tagged(device: &udev::Device) -> Option<Node> {
    text(device.property_value("CURRENT_TAGS")).and_then(Node::tagged)
}

/// Returns the serial number of the USB device of the node.
fn serial(device: &udev::Device) -> Option<&str> {
    text(device.property_value("ID_SERIAL_SHORT"))
}

/// Returns the value as text, if it is UTF-8.
fn text(value: Option<&OsStr>) -> Option<&str> {
    value.and_then(OsStr::to_str)
}

/// Returns whether the socket holds a message. Does not wait.
fn has_messages(socket: &udev::MonitorSocket) -> io::Result<bool> {
    let mut fds = [PollFd::new(socket, PollFlags::IN)];
    rustix::event::poll(&mut fds, Some(&Timespec::default()))?;
    Ok(fds[0].revents().contains(PollFlags::IN))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = include_str!("../debian/70-presencemux-cam.rules");

    #[test]
    fn node_with_the_camera_tag_is_the_camera() {
        assert_eq!(
            Node::tagged(":seat:presencemux-camera:uaccess:"),
            Some(Node::Camera)
        );
    }

    #[test]
    fn node_with_the_gadget_tag_is_the_gadget() {
        assert_eq!(Node::tagged(":presencemux-gadget:"), Some(Node::Gadget));
    }

    #[test]
    fn node_with_a_longer_tag_is_not_a_node() {
        assert_eq!(Node::tagged(":presencemux-camera-old:"), None);
    }

    #[test]
    fn node_without_tags_is_not_a_node() {
        assert_eq!(Node::tagged(""), None);
    }

    #[test]
    fn udev_rule_sets_the_tag_of_each_node() {
        for node in Node::ALL {
            let tag = format!("TAG+=\"{}\"", node.tag());

            assert!(RULES.contains(&tag), "the rule does not set {tag}");
        }
    }

    #[test]
    fn udev_rule_passes_udevadm_verify() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/debian/70-presencemux-cam.rules"
        );

        let status = std::process::Command::new("udevadm")
            .args(["verify", "--no-summary", path])
            .status()
            .expect("udevadm runs");

        assert!(status.success());
    }

    #[test]
    fn camera_removed_and_added_at_the_same_path_is_a_new_camera() {
        let path = Path::new("/dev/video1");
        let mut nodes = Nodes::default();
        nodes.set(Node::Camera, Some(path));
        let first = nodes.camera().cloned();

        nodes.set(Node::Camera, None);
        nodes.set(Node::Camera, Some(path));

        assert_ne!(nodes.camera(), first.as_ref());
        assert_eq!(
            nodes.camera().map(|camera| camera.path.as_path()),
            Some(path)
        );
    }

    #[test]
    fn second_add_event_of_the_camera_changes_nothing() {
        let path = Path::new("/dev/video1");
        let mut nodes = Nodes::default();
        nodes.set(Node::Camera, Some(path));
        let first = nodes.camera().cloned();

        let changed = nodes.set(Node::Camera, Some(path));

        assert!(!changed);
        assert_eq!(nodes.camera(), first.as_ref());
    }

    #[test]
    fn removed_path_of_another_node_is_not_a_node() {
        let mut nodes = Nodes::default();
        nodes.set(Node::Gadget, Some(Path::new("/dev/video0")));

        assert_eq!(nodes.node_at(Path::new("/dev/video19")), None);
    }

    #[test]
    fn removed_path_of_the_gadget_is_the_gadget() {
        let mut nodes = Nodes::default();
        nodes.set(Node::Gadget, Some(Path::new("/dev/video0")));

        assert_eq!(nodes.node_at(Path::new("/dev/video0")), Some(Node::Gadget));
    }
}
