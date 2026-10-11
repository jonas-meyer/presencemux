//! The configfs operations of the gadget, and the UDC binding.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::Path;

use crate::gadget::Op;

/// The error of one configfs operation.
#[derive(Debug, thiserror::Error)]
#[error("{op}: {source}")]
pub(crate) struct OpError {
    op: String,
    source: io::Error,
}

/// The error of `apply`, with the error of the undo of the applied operations.
#[derive(Debug)]
pub(crate) struct ApplyError {
    pub(crate) error: OpError,
    pub(crate) undo: Option<OpError>,
}

/// Applies the operations in order. On an error, undoes the operations that it
/// applied.
pub(crate) fn apply(root: &Path, ops: &[Op]) -> Result<(), ApplyError> {
    for (applied, op) in ops.iter().enumerate() {
        let result = match op {
            Op::Dir(path) => fs::create_dir(root.join(path)),
            Op::Write { path, value } => fs::write(root.join(path), value),
            Op::Link { target, link } => symlink(root.join(target), root.join(link)),
        };
        if let Err(source) = result {
            let error = OpError {
                op: op.to_string(),
                source,
            };
            return Err(ApplyError {
                error,
                undo: undo(root, &ops[..applied]).err(),
            });
        }
    }
    Ok(())
}

/// Undoes the operations in reverse order: removes each link and directory
/// that exists. Continues after an error and returns the first error.
pub(crate) fn undo(root: &Path, ops: &[Op]) -> Result<(), OpError> {
    let mut first = None;
    for op in ops.iter().rev() {
        let result = match op {
            Op::Dir(path) => fs::remove_dir(root.join(path)),
            Op::Link { link, .. } => fs::remove_file(root.join(link)),
            Op::Write { .. } => Ok(()),
        };
        match result {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                first.get_or_insert(OpError {
                    op: format!("undo {op}"),
                    source,
                });
            }
            Ok(()) => {}
        }
    }
    first.map_or(Ok(()), Err)
}

/// Returns the UDC that the gadget is bound to, or `None` if it is not bound.
pub(crate) fn bound_udc(root: &Path) -> io::Result<Option<String>> {
    let udc = fs::read_to_string(root.join("UDC"))?;
    let udc = udc.trim();
    Ok((!udc.is_empty()).then(|| udc.to_owned()))
}

/// Binds the gadget to the UDC.
pub(crate) fn bind(root: &Path, udc: &str) -> io::Result<()> {
    fs::write(root.join("UDC"), udc)
}

/// Unbinds the gadget from its UDC. The kernel waits until every program
/// closes the gadget node.
pub(crate) fn unbind(root: &Path) -> io::Result<()> {
    fs::write(root.join("UDC"), "\n")
}
