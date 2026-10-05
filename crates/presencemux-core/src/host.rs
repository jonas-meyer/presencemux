/// Whether a host computer has configured the USB gadget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum HostStatus {
    #[default]
    Disconnected,
    /// The bus is idle. The same host can resume without a new enumeration.
    Suspended,
    Connected,
}
