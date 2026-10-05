/// Whether a laptop has configured the USB gadget.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum HostStatus {
    #[default]
    Disconnected,
    /// The bus is idle. The same laptop can resume without a new enumeration.
    Suspended,
    Connected,
}
