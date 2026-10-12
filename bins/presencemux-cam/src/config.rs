//! The slate files in the configuration file of PresenceMux.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The configuration file. presencemuxd reads the same file.
pub(crate) const CONFIG: &str = "/etc/presencemux/config.toml";

/// The part of the configuration file that the worker reads.
#[derive(Debug, Deserialize)]
struct File {
    #[serde(default)]
    slates: BTreeMap<String, toml::Table>,
}

/// The table of one slate.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SlateTable {
    /// The JPEG file of a still image, or the raw MJPEG file of a loop.
    file: PathBuf,
}

/// An error of the configuration file.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("cannot read {path}: {source}", path = path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid {path}: {message}", path = path.display())]
    Parse { path: PathBuf, message: String },
}

/// Returns the file of each slate in the configuration file at `path`, or the
/// error of its slate table.
pub(crate) fn slate_files(path: &Path) -> Result<BTreeMap<String, Result<PathBuf, String>>, Error> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })?;
    parse(&text).map_err(|message| Error::Parse {
        path: path.to_owned(),
        message,
    })
}

/// Returns the file of each slate in the configuration text `text`, or the
/// error of its slate table. Returns a one-line error if the text is not valid
/// TOML.
fn parse(text: &str) -> Result<BTreeMap<String, Result<PathBuf, String>>, String> {
    let file: File = toml::from_str(text).map_err(|error| one_line(text, &error))?;
    Ok(file
        .slates
        .into_iter()
        .map(|(name, table)| {
            let file = table
                .try_into::<SlateTable>()
                .map(|table| table.file)
                .map_err(|error| error.message().to_owned());
            (name, file)
        })
        .collect())
}

/// Returns `error` in one line, with the line number in `text`.
fn one_line(text: &str, error: &toml::de::Error) -> String {
    match error.span() {
        Some(span) => {
            let line = text[..span.start].lines().count().max(1);
            format!("line {line}: {}", error.message())
        }
        None => error.message().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slate_gives_its_file() {
        let files = parse("[slates.away]\nfile = \"/slates/away.jpg\"\n").unwrap();

        assert_eq!(files["away"], Ok(PathBuf::from("/slates/away.jpg")));
    }

    #[test]
    fn presets_give_no_slate_files() {
        let files = parse("[presets.away]\ncam = { slate = \"away\" }\n").unwrap();

        assert!(files.is_empty());
    }

    #[test]
    fn slate_without_a_file_gives_its_error() {
        let files = parse("[slates.away]\n").unwrap();

        assert_eq!(files["away"], Err("missing field `file`".to_owned()));
    }

    #[test]
    fn slate_with_an_unknown_key_gives_its_error() {
        let files = parse("[slates.away]\nfile = \"a.jpg\"\ntext = \"Away\"\n").unwrap();

        assert_eq!(
            files["away"],
            Err("unknown field `text`, expected `file`".to_owned())
        );
    }

    #[test]
    fn slate_error_leaves_the_other_slates() {
        let files = parse("[slates.away]\n[slates.brb]\nfile = \"brb.jpg\"\n").unwrap();

        assert_eq!(files["brb"], Ok(PathBuf::from("brb.jpg")));
    }

    #[test]
    fn invalid_toml_gives_one_line_with_the_line_number() {
        let error = parse("[slates.away]\nfile = \n").unwrap_err();

        assert_eq!(
            error,
            "line 2: string values must be quoted, expected literal string"
        );
    }

    #[test]
    fn shipped_configuration_has_the_files_of_the_default_slates() {
        let text = include_str!("../../presencemuxd/debian/config.toml");

        let files = parse(text).unwrap();

        assert_eq!(
            files,
            BTreeMap::from([
                (
                    "away".to_owned(),
                    Ok(PathBuf::from("/usr/share/presencemux-cam/slates/away.jpg"))
                ),
                (
                    "brb".to_owned(),
                    Ok(PathBuf::from("/usr/share/presencemux-cam/slates/brb.jpg"))
                ),
                (
                    "still-here".to_owned(),
                    Ok(PathBuf::from(
                        "/usr/share/presencemux-cam/slates/still-here.jpg"
                    ))
                ),
            ])
        );
    }
}
