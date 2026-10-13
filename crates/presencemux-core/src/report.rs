use core::fmt::Debug;

use crate::{Device, DeviceStatus, Epoch, Granted, OnAir};

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

/// A snapshot of the devices of one owner, and of its output.
pub trait Report {
    /// The permission of the grants that the owner applies.
    type Permission: Copy + PartialEq + Debug;

    /// Returns each device with its status, in the order of the causes. The
    /// worker comes first.
    fn devices(&self) -> [(Device, DeviceStatus); 2];

    /// Returns the epoch of the lease at the latch of the owner, if the owner
    /// is latched.
    fn latched(&self) -> Option<Epoch>;

    /// Returns the on-air level of the output that the host gets now.
    fn output(&self) -> OnAir;

    /// Returns whether the output is above the grant under which the owner
    /// made it.
    fn exceeded(&self) -> bool;

    /// Returns the grant of the lease that the owner applies, if the owner has
    /// a valid lease.
    fn applied(&self) -> Option<Granted<Self::Permission>>;
}
