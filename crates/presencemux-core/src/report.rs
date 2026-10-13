use crate::{Device, DeviceStatus, Epoch};

/// The state of the reports from the owner of the devices of a plane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Reported<R> {
    /// No report since boot.
    #[default]
    Pending,
    /// The last report of the owner.
    Fresh(R),
    /// The owner does not reply, or sends no more reports.
    Lost,
}

/// A snapshot of the devices of one owner.
pub trait Report {
    /// Returns each device with its status, in the order of the causes. The
    /// worker comes first.
    fn devices(&self) -> [(Device, DeviceStatus); 2];

    /// Returns the epoch of the lease at the latch of the owner, if the owner
    /// is latched.
    fn latched(&self) -> Option<Epoch>;
}
