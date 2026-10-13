//! The open call and the feature report ioctls of the hidraw node.

#![expect(unsafe_code, reason = "the feature report ioctls of hidraw")]

use std::ffi::c_void;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use rustix::fs::{Mode, OFlags};
use rustix::ioctl::{Direction, Ioctl, IoctlOutput, Opcode, ioctl, opcode};

use crate::protocol::{FEATURE_LEN, FeatureReport};

/// Opens the hidraw node at `path` for reads and writes, without blocking
/// reads.
pub(crate) fn open(path: &Path) -> io::Result<OwnedFd> {
    let flags = OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC;
    Ok(rustix::fs::open(path, flags, Mode::empty())?)
}

/// Returns the opcode of the feature report ioctl `number` for
/// [`FEATURE_LEN`] bytes.
const fn feature(number: u8) -> Opcode {
    opcode::from_components(Direction::ReadWrite, b'H', number, FEATURE_LEN)
}

/// `HIDIOCSFEATURE(32)`.
const SET_FEATURE: Opcode = feature(0x06);
/// `HIDIOCGFEATURE(32)`.
const GET_FEATURE: Opcode = feature(0x07);

/// A feature report ioctl on a buffer of 32 bytes. Byte 0 is the report id.
struct Feature<'a, const OPCODE: Opcode>(&'a mut FeatureReport);

// SAFETY: OPCODE encodes the size 32, and the kernel reads and writes at most
// 32 bytes at the pointer (`hidraw_send_report`, `hidraw_get_report` in
// drivers/hid/hidraw.c).
unsafe impl<const OPCODE: Opcode> Ioctl for Feature<'_, OPCODE> {
    type Output = usize;

    const IS_MUTATING: bool = true;

    fn opcode(&self) -> Opcode {
        OPCODE
    }

    fn as_ptr(&mut self) -> *mut c_void {
        self.0.as_mut_ptr().cast()
    }

    unsafe fn output_from_ptr(
        output: IoctlOutput,
        _: *mut c_void,
    ) -> rustix::io::Result<Self::Output> {
        Ok(usize::try_from(output).unwrap_or(0))
    }
}

/// Sends the feature report `report`. Returns the number of bytes that the
/// device took. Waits for the device for up to 5 s.
pub(crate) fn set_feature(fd: impl AsFd, report: &FeatureReport) -> io::Result<usize> {
    let mut report = *report;
    // SAFETY: See the impl of `Ioctl` for `Feature`.
    Ok(unsafe { ioctl(fd, Feature::<SET_FEATURE>(&mut report)) }?)
}

/// Reads the feature report with the id `id`. Waits for the device for up to
/// 5 s.
pub(crate) fn get_feature(fd: impl AsFd, id: u8) -> io::Result<FeatureReport> {
    let mut report = [0; FEATURE_LEN];
    report[0] = id;
    // SAFETY: See the impl of `Ioctl` for `Feature`.
    unsafe { ioctl(fd, Feature::<GET_FEATURE>(&mut report)) }?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_feature_opcode_is_hidiocsfeature_32() {
        assert_eq!(SET_FEATURE, 0xc020_4806);
    }

    #[test]
    fn get_feature_opcode_is_hidiocgfeature_32() {
        assert_eq!(GET_FEATURE, 0xc020_4807);
    }
}
