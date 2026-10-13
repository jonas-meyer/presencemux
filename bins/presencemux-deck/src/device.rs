//! The open device. A tokio task reads its input reports. One writer thread
//! does every write and every feature report, in order.

use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;
use std::time::Duration;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::oneshot;

use crate::hidraw;
use crate::protocol::{
    FIRMWARE_VERSION, FeatureReport, INPUT_LEN, OutputReport, Pressed, firmware_version,
};

/// The number of writes that can wait for the writer thread. A full queue
/// means that the device takes no more writes.
const QUEUE: usize = 256;

/// The longest wait for an output report that the device does not take yet.
const BUSY_WAIT: Duration = Duration::from_millis(100);

/// A write to the device.
#[derive(Debug)]
pub(crate) enum Write {
    Output(Box<OutputReport>),
    Feature(FeatureReport),
}

/// An error of an open device. After an error, the device is lost.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DeviceError {
    #[error("cannot open the deck: {0}")]
    Open(io::Error),
    #[error("cannot read the deck: {0}")]
    Read(io::Error),
    #[error("cannot write to the deck: {0}")]
    Write(io::Error),
    #[error("the deck takes no more writes")]
    Full,
    #[error("the writer thread of the deck stopped")]
    Stopped,
    #[error("the deck sent an invalid input report: {0}")]
    Input(#[from] crate::protocol::InputError),
}

/// The target of the writes to a device. The tests use a target that records
/// the writes.
pub(crate) trait Sink {
    /// Queues `write`. Does not wait.
    fn send(&self, write: Write) -> Result<(), DeviceError>;
}

/// An open hidraw node of the deck, with its writer thread.
pub(crate) struct Device {
    reader: AsyncFd<OwnedFd>,
    writes: SyncSender<Write>,
    failed: oneshot::Receiver<io::Error>,
}

impl Device {
    /// Opens the device at `path`, and starts its writer thread. The writer
    /// thread logs the firmware version first. Does not wait for the device.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let fd = hidraw::open(path)?;
        let writer = fd.try_clone()?;
        let (writes, queue) = mpsc::sync_channel(QUEUE);
        let (fail, failed) = oneshot::channel();
        thread::Builder::new()
            .name("deck-writer".to_owned())
            .spawn(move || {
                log_firmware_version(&writer);
                if let Err(error) = write_all(&writer, &queue) {
                    let _ = fail.send(error);
                }
            })?;
        let reader = AsyncFd::with_interest(fd, Interest::READABLE)?;
        Ok(Self {
            reader,
            writes,
            failed,
        })
    }

    /// Waits for the next input report, and returns the inputs that it shows
    /// as pressed. Returns an error if the device is gone or if a write
    /// failed.
    pub(crate) async fn next(&mut self) -> Result<Pressed, DeviceError> {
        let mut report = [0; INPUT_LEN];
        loop {
            tokio::select! {
                failed = &mut self.failed => {
                    return Err(failed.map_or(DeviceError::Stopped, DeviceError::Write));
                }
                guard = self.reader.readable() => {
                    let mut guard = guard.map_err(DeviceError::Read)?;
                    match guard.try_io(|fd| Ok(rustix::io::read(fd.get_ref(), &mut report)?)) {
                        // Hidraw does not return a read of 0 bytes. The deck
                        // handles it as a lost device.
                        Ok(Ok(0)) => {
                            return Err(DeviceError::Read(io::ErrorKind::UnexpectedEof.into()));
                        }
                        Ok(Ok(len)) => return Ok(Pressed::parse(&report[..len])?),
                        Ok(Err(error)) => return Err(DeviceError::Read(error)),
                        Err(_would_block) => {}
                    }
                }
            }
        }
    }
}

impl Sink for Device {
    /// Queues `write` for the writer thread. Does not wait.
    fn send(&self, write: Write) -> Result<(), DeviceError> {
        self.writes.try_send(write).map_err(|error| match error {
            TrySendError::Full(_) => DeviceError::Full,
            TrySendError::Disconnected(_) => DeviceError::Stopped,
        })
    }
}

/// Reads the firmware version of the device at `fd`, and logs it.
fn log_firmware_version(fd: &OwnedFd) {
    match hidraw::get_feature(fd, FIRMWARE_VERSION) {
        Ok(report) => tracing::info!(
            "the deck has firmware {}",
            firmware_version(&report).unwrap_or("unknown")
        ),
        Err(error) => tracing::warn!("cannot read the firmware version of the deck: {error}"),
    }
}

/// Writes each write of `queue` to `fd`, until the queue closes or a write
/// fails.
fn write_all(fd: &OwnedFd, queue: &mpsc::Receiver<Write>) -> io::Result<()> {
    while let Ok(write) = queue.recv() {
        match write {
            Write::Output(report) => write_output(fd, &report)?,
            Write::Feature(report) => {
                hidraw::set_feature(fd, &report)?;
            }
        }
    }
    Ok(())
}

/// Writes the output report `report`. If the device is busy, tries again for
/// up to [`BUSY_WAIT`].
fn write_output(fd: &OwnedFd, report: &OutputReport) -> io::Result<()> {
    let mut waited = Duration::ZERO;
    loop {
        match rustix::io::write(fd, report) {
            Ok(len) if len == report.len() => return Ok(()),
            Ok(len) => {
                return Err(io::Error::other(format!(
                    "the deck took {len} of {} bytes",
                    report.len()
                )));
            }
            Err(rustix::io::Errno::AGAIN) if waited < BUSY_WAIT => {
                thread::sleep(Duration::from_millis(1));
                waited += Duration::from_millis(1);
            }
            Err(error) => return Err(error.into()),
        }
    }
}
