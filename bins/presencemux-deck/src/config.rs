//! The `[deck]` section of the configuration file.

use std::io;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::protocol::{KEYS, Key};
use crate::view::Action;

/// The configuration file of PresenceMux.
pub(crate) const CONFIG: &str = "/etc/presencemux/config.toml";

/// The key actions if the file has no `[deck]` section, row by row from the top
/// left.
pub(crate) const DEFAULT_KEYS: [&str; KEYS as usize] = [
    "preset:privacy",
    "preset:live",
    "toggle:mic",
    "toggle:cam",
    "preset:still-here",
    "preset:brb",
    "preset:away",
    "resume",
];

/// The configuration of the deck.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Config {
    /// The brightness in percent.
    pub(crate) brightness: u8,
    /// The time without a press after which the deck dims. `None` if the deck
    /// does not dim.
    pub(crate) dim_after: Option<Duration>,
    /// The brightness of a dim deck in percent.
    pub(crate) dim_brightness: u8,
    /// The action of each key, row by row from the top left.
    keys: [Action; KEYS as usize],
}

/// A configuration file that the deck cannot use.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read { path: String, source: io::Error },
    #[error("invalid configuration {path}: {message}")]
    Invalid { path: String, message: String },
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    deck: DeckTable,
}

/// The `[deck]` table.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct DeckTable {
    brightness: u8,
    dim_after: u64,
    dim_brightness: u8,
    keys: Vec<String>,
}

impl Default for DeckTable {
    fn default() -> Self {
        Self {
            brightness: 60,
            dim_after: 300,
            dim_brightness: 10,
            keys: DEFAULT_KEYS.map(str::to_owned).to_vec(),
        }
    }
}

impl Config {
    /// Reads the `[deck]` section of the file at `path`. A missing file gives
    /// the defaults.
    pub(crate) fn read(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tracing::warn!(
                    "{} does not exist, the deck uses its defaults",
                    path.display()
                );
                String::new()
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        Self::parse(&text).map_err(|message| ConfigError::Invalid {
            path: path.display().to_string(),
            message,
        })
    }

    /// Returns the action of `key`.
    pub(crate) fn action(&self, key: Key) -> &Action {
        &self.keys[usize::from(key.index())]
    }

    /// Returns the configuration in `text`, or the error as one line. The
    /// deck ignores the other sections.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let file: File = toml::from_str(text).map_err(|error| one_line(text, &error))?;
        let deck = file.deck;
        let actions = deck
            .keys
            .iter()
            .map(|key| key.parse().map_err(|error| format!("deck.keys: {error}")))
            .collect::<Result<Vec<Action>, _>>()?;
        let keys = <[Action; KEYS as usize]>::try_from(actions).map_err(|actions| {
            format!(
                "deck.keys has {} actions, the deck has {KEYS} keys",
                actions.len()
            )
        })?;
        Ok(Self {
            brightness: deck.brightness.min(100),
            dim_after: (deck.dim_after > 0).then(|| Duration::from_secs(deck.dim_after)),
            dim_brightness: deck.dim_brightness.min(100),
            keys,
        })
    }
}

/// Returns `error` in `text` as one line, with its line number.
fn one_line(text: &str, error: &toml::de::Error) -> String {
    match error.span() {
        Some(span) => {
            let line = text[..span.start].matches('\n').count() + 1;
            format!("line {line}: {}", error.message())
        }
        None => error.message().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use presencemux_api::{Mute, Plane};

    use super::*;

    #[test]
    fn empty_file_gives_the_default_keys() {
        let config = Config::parse("").unwrap();

        assert_eq!(
            config.action(Key::new(3).unwrap()),
            &Action::Toggle(Plane::Cam)
        );
    }

    #[test]
    fn file_without_a_deck_section_ignores_the_other_sections() {
        let config = Config::parse("[cam]\nstream_timeout = 30\n");

        assert_eq!(config, Config::parse(""));
    }

    #[test]
    fn deck_section_sets_the_keys() {
        let text = r#"
            [deck]
            keys = ["mic:muted", "none", "none", "none", "none", "none", "none", "resume"]
        "#;

        let config = Config::parse(text).unwrap();

        assert_eq!(
            config.action(Key::new(0).unwrap()),
            &Action::Mic(Mute::Muted)
        );
    }

    #[test]
    fn seven_keys_are_an_error() {
        let text = r#"
            [deck]
            keys = ["none", "none", "none", "none", "none", "none", "none"]
        "#;

        let error = Config::parse(text).unwrap_err();

        assert_eq!(error, "deck.keys has 7 actions, the deck has 8 keys");
    }

    #[test]
    fn unknown_action_is_an_error() {
        let text = r#"
            [deck]
            keys = ["live", "none", "none", "none", "none", "none", "none", "none"]
        "#;

        let error = Config::parse(text).unwrap_err();

        assert!(error.starts_with("deck.keys: unknown key action \"live\""));
    }

    #[test]
    fn unknown_deck_field_is_an_error_with_its_line() {
        let error = Config::parse("[deck]\nbrightnes = 50\n").unwrap_err();

        assert_eq!(
            error,
            "line 2: unknown field `brightnes`, expected one of `brightness`, `dim_after`, \
             `dim_brightness`, `keys`"
        );
    }

    #[test]
    fn dim_after_0_turns_dimming_off() {
        let config = Config::parse("[deck]\ndim_after = 0\n").unwrap();

        assert_eq!(config.dim_after, None);
    }

    #[test]
    fn brightness_above_100_is_100() {
        let config = Config::parse("[deck]\nbrightness = 150\n").unwrap();

        assert_eq!(config.brightness, 100);
    }
}
