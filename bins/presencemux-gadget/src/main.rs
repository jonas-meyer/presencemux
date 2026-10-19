//! Creates and removes the USB gadget of PresenceMux in configfs.

mod configfs;
mod gadget;
mod interrupt;

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::configfs::{ApplyError, OpError};

/// The configfs directory of the gadget.
const GADGET: &str = "/sys/kernel/config/usb_gadget/presencemux";
/// The configfs directory of all gadgets. `libcomposite` creates it.
const GADGETS: &str = "/sys/kernel/config/usb_gadget";
const UDCS: &str = "/sys/class/udc";
const SERIAL_NUMBER: &str = "/proc/device-tree/serial-number";
const INTERRUPTS: &str = "/proc/interrupts";

/// Creates or removes the USB gadget of PresenceMux.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Creates the gadget and binds it to the UDC. Does nothing if the gadget
    /// exists and is bound.
    Up,
    /// Unbinds the gadget and removes it. Does nothing if no gadget exists.
    Down,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("{GADGETS} does not exist: configfs is not mounted, or libcomposite is not loaded")]
    NoConfigfs,
    #[error(
        "no UDC: add dtoverlay=dwc2,dr_mode=peripheral under [pi5] in /boot/firmware/config.txt"
    )]
    NoUdc,
    #[error("cannot read the UDCs: {0}")]
    Udcs(io::Error),
    #[error("cannot read the serial number {SERIAL_NUMBER}: {0}")]
    Serial(io::Error),
    #[error("the serial number in {SERIAL_NUMBER} is empty")]
    EmptySerial,
    #[error("cannot read the UDC of the gadget: {0}")]
    ReadUdc(io::Error),
    #[error("the gadget exists but is not bound to a UDC: run down, then up")]
    NotBound,
    #[error("cannot create the gadget: {0}")]
    Create(OpError),
    #[error("cannot bind the gadget to the UDC {udc}: {source}")]
    Bind { udc: String, source: io::Error },
    #[error("{error}. Cannot remove the partial gadget: {undo}")]
    Undo { error: Box<Self>, undo: OpError },
    #[error("cannot unbind the gadget: {0}")]
    Unbind(io::Error),
    #[error("cannot remove the gadget: {0}")]
    Remove(OpError),
    #[error("cannot write the output: {0}")]
    Output(io::Error),
}

impl Error {
    /// Adds the error of the undo of a partial gadget, if any.
    fn with_undo(self, undo: Option<OpError>) -> Self {
        match undo {
            Some(undo) => Self::Undo {
                error: Box::new(self),
                undo,
            },
            None => self,
        }
    }
}

fn main() -> ExitCode {
    match run(&Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "presencemux-gadget: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(), Error> {
    let root = Path::new(GADGET);
    let mut out = io::stdout().lock();
    match cli.command {
        Command::Up => {
            let udc = if root.exists() {
                let udc = configfs::bound_udc(root)
                    .map_err(Error::ReadUdc)?
                    .ok_or(Error::NotBound)?;
                writeln!(out, "the gadget exists and is bound to {udc}").map_err(Error::Output)?;
                udc
            } else {
                let udc = create(root)?;
                writeln!(out, "bound to {udc}").map_err(Error::Output)?;
                udc
            };
            move_interrupts(&mut out, &udc).map_err(Error::Output)
        }
        Command::Down => {
            if !root.exists() {
                return writeln!(out, "no gadget exists").map_err(Error::Output);
            }
            if configfs::bound_udc(root).map_err(Error::ReadUdc)?.is_some() {
                writeln!(out, "waits until every program closes the gadget node")
                    .map_err(Error::Output)?;
                configfs::unbind(root).map_err(Error::Unbind)?;
            }
            // The removal ignores the values. It needs no serial number.
            configfs::undo(root, &gadget::plan("")).map_err(Error::Remove)?;
            writeln!(out, "removed").map_err(Error::Output)
        }
    }
}

/// Creates the gadget in `root`, binds it to the first UDC and returns the name
/// of the UDC. Removes the partial gadget on an error.
fn create(root: &Path) -> Result<String, Error> {
    if !Path::new(GADGETS).is_dir() {
        return Err(Error::NoConfigfs);
    }
    let udc = first_udc().map_err(Error::Udcs)?.ok_or(Error::NoUdc)?;
    let ops = gadget::plan(&serial_number()?);
    configfs::apply(root, &ops)
        .map_err(|ApplyError { error, undo }| Error::Create(error).with_undo(undo))?;
    if let Err(source) = configfs::bind(root, &udc) {
        let undo = configfs::undo(root, &ops).err();
        return Err(Error::Bind { udc, source }.with_undo(undo));
    }
    Ok(udc)
}

/// Returns the name of the first UDC, if one exists.
fn first_udc() -> io::Result<Option<String>> {
    let mut names = fs::read_dir(UDCS)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names.into_iter().next())
}

/// Moves the interrupts of the UDC to `interrupt::CPU`. The gadget works
/// without it, so an error is only a warning.
fn move_interrupts(out: &mut impl Write, udc: &str) -> io::Result<()> {
    let result = fs::read_to_string(INTERRUPTS).and_then(|interrupts| {
        let numbers = interrupt::of_device(&interrupts, udc);
        for number in &numbers {
            let path = format!("/proc/irq/{number}/smp_affinity_list");
            fs::write(&path, interrupt::CPU.to_string())
                .map_err(|error| io::Error::new(error.kind(), format!("{path}: {error}")))?;
        }
        Ok(numbers)
    });
    match result {
        Ok(numbers) if numbers.is_empty() => {
            writeln!(out, "warning: {INTERRUPTS} has no interrupt of {udc}")
        }
        Ok(numbers) => writeln!(
            out,
            "moved the interrupts {numbers:?} of {udc} to CPU {}",
            interrupt::CPU
        ),
        Err(error) => writeln!(
            out,
            "warning: cannot move the interrupts of {udc} to CPU {}: {error}",
            interrupt::CPU
        ),
    }
}

/// Returns the serial number of the Raspberry Pi. The host identifies the
/// gadget by it.
fn serial_number() -> Result<String, Error> {
    let serial = fs::read_to_string(SERIAL_NUMBER).map_err(Error::Serial)?;
    let serial = serial.trim_end_matches('\0').trim();
    if serial.is_empty() {
        return Err(Error::EmptySerial);
    }
    Ok(serial.to_owned())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn command_line_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
