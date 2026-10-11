//! Creates and removes the USB gadget of PresenceMux in configfs.

mod configfs;
mod gadget;

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
            if root.exists() {
                let udc = configfs::bound_udc(root)
                    .map_err(Error::ReadUdc)?
                    .ok_or(Error::NotBound)?;
                return writeln!(out, "the gadget exists and is bound to {udc}")
                    .map_err(Error::Output);
            }
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
            writeln!(out, "bound to {udc}").map_err(Error::Output)
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

/// Returns the name of the first UDC, if one exists.
fn first_udc() -> io::Result<Option<String>> {
    let mut names = fs::read_dir(UDCS)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names.into_iter().next())
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
