//! The daemon's part of `/etc/presencemux/config.toml`.

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use presencemux_api::{CAM_BLANK, CAM_LIVE, PRESET_PRIVACY};
use presencemux_core::{CamPermission, MicMute, Preset, SlateId};
use serde::Deserialize;
use serde::de::IgnoredAny;

/// The configuration of the daemon, checked and resolved to core types.
#[derive(Debug)]
pub(crate) struct Config {
    presets: BTreeMap<PresetName, Preset>,
    pub slates: Slates,
}

/// A configured preset name. The built-in [`PRESET_PRIVACY`] preset cannot be
/// configured.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct PresetName(String);

/// A configured slate name. It cannot be a cam value ([`CAM_BLANK`] or
/// [`CAM_LIVE`]).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(try_from = "String")]
pub(crate) struct SlateName(String);

/// A reference to the built-in preset or to a configured preset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PresetRef {
    Privacy,
    Configured(PresetName),
}

/// The configured slates. The position of a name is its [`SlateId`].
#[derive(Debug, Default)]
pub(crate) struct Slates(Vec<SlateName>);

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config {}: {source}", path.display())]
    Invalid { path: PathBuf, source: ParseError },
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ParseError {
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error("preset `{preset}` names the unknown slate `{slate}`")]
    UnknownSlate { preset: PresetName, slate: String },
    #[error("too many slates")]
    TooManySlates,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NameError {
    #[error("`{0}` is a built-in preset and cannot be configured")]
    ReservedPreset(String),
    #[error("`{0}` is a cam value and cannot be a slate name")]
    ReservedSlate(String),
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not `{PRESET_PRIVACY}` or a configured preset")]
pub(crate) struct UnknownPreset(String);

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not `{CAM_BLANK}`, `{CAM_LIVE}` or a configured slate")]
pub(crate) struct UnknownCam(String);

impl Config {
    pub(crate) fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text).map_err(|source| Error::Invalid {
            path: path.to_owned(),
            source,
        })
    }

    pub(crate) fn parse(text: &str) -> Result<Self, ParseError> {
        let file: File = toml::from_str(text)?;

        let names: Vec<SlateName> = file.slates.into_keys().collect();
        if u16::try_from(names.len()).is_err() {
            return Err(ParseError::TooManySlates);
        }
        let slates = Slates(names);

        let presets = file
            .presets
            .into_iter()
            .map(|(name, preset)| {
                let cam = preset
                    .cam
                    .map(|cam| cam.resolve(&slates, &name))
                    .transpose()?;
                let mic = preset.mic.map(MicMute::from);
                Ok((name, Preset { cam, mic }))
            })
            .collect::<Result<_, ParseError>>()?;

        Ok(Self { presets, slates })
    }

    /// Returns the names of the configured presets, in order.
    pub(crate) fn preset_names(&self) -> impl Iterator<Item = &str> {
        self.presets.keys().map(PresetName::as_str)
    }

    /// Returns the preset that an API name refers to.
    pub(crate) fn preset(&self, name: &str) -> Result<(PresetRef, Preset), UnknownPreset> {
        match name {
            PRESET_PRIVACY => Ok((PresetRef::Privacy, Preset::PRIVACY)),
            name => self
                .presets
                .get_key_value(name)
                .map(|(name, preset)| (PresetRef::Configured(name.clone()), *preset))
                .ok_or_else(|| UnknownPreset(name.to_owned())),
        }
    }
}

impl PresetRef {
    /// Returns the API name of the preset.
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Privacy => PRESET_PRIVACY,
            Self::Configured(name) => name.as_str(),
        }
    }
}

impl Slates {
    pub(crate) fn id(&self, name: &str) -> Option<SlateId> {
        let index = self.0.iter().position(|slate| slate.as_str() == name)?;
        u16::try_from(index).ok().map(SlateId)
    }

    pub(crate) fn name(&self, id: SlateId) -> Option<&str> {
        self.0.get(usize::from(id.0)).map(SlateName::as_str)
    }

    /// Parses a cam value from the API: [`CAM_BLANK`], [`CAM_LIVE`] or a slate
    /// name.
    pub(crate) fn parse_cam(&self, value: &str) -> Result<CamPermission, UnknownCam> {
        match value {
            CAM_BLANK => Ok(CamPermission::Blank),
            CAM_LIVE => Ok(CamPermission::Live),
            name => self
                .id(name)
                .map(CamPermission::Slate)
                .ok_or_else(|| UnknownCam(name.to_owned())),
        }
    }

    /// Returns the API value of a cam permission.
    pub(crate) fn cam_value(&self, permission: CamPermission) -> &str {
        match permission {
            CamPermission::Blank => CAM_BLANK,
            CamPermission::Live => CAM_LIVE,
            CamPermission::Slate(id) => {
                let name = self.name(id);
                debug_assert!(name.is_some(), "{id:?} has no slate name");
                name.unwrap_or("unknown")
            }
        }
    }
}

impl PresetName {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl SlateName {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PresetName {
    type Error = NameError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        match name.as_str() {
            PRESET_PRIVACY => Err(NameError::ReservedPreset(name)),
            _ => Ok(Self(name)),
        }
    }
}

impl TryFrom<String> for SlateName {
    type Error = NameError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        match name.as_str() {
            CAM_BLANK | CAM_LIVE => Err(NameError::ReservedSlate(name)),
            _ => Ok(Self(name)),
        }
    }
}

impl fmt::Display for PresetName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for SlateName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for PresetName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[expect(
        clippy::zero_sized_map_values,
        reason = "the file has a table for each slate, and the cam worker owns its content"
    )]
    #[serde(default)]
    slates: BTreeMap<SlateName, IgnoredAny>,
    #[serde(default)]
    presets: BTreeMap<PresetName, PresetFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetFile {
    cam: Option<CamFile>,
    mic: Option<MicFile>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CamFile {
    Mode(CamMode),
    Slate { slate: String },
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum CamMode {
    Blank,
    Live,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum MicFile {
    Muted,
    Unmuted,
}

impl CamFile {
    fn resolve(self, slates: &Slates, preset: &PresetName) -> Result<CamPermission, ParseError> {
        match self {
            Self::Mode(CamMode::Blank) => Ok(CamPermission::Blank),
            Self::Mode(CamMode::Live) => Ok(CamPermission::Live),
            Self::Slate { slate } => slates.id(&slate).map(CamPermission::Slate).ok_or_else(|| {
                ParseError::UnknownSlate {
                    preset: preset.clone(),
                    slate,
                }
            }),
        }
    }
}

impl From<MicFile> for MicMute {
    fn from(mic: MicFile) -> Self {
        match mic {
            MicFile::Muted => Self::Muted,
            MicFile::Unmuted => Self::Unmuted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cam_and_mic_values_resolve_to_core_values() {
        let config = Config::parse("[presets.live]\ncam = \"live\"\nmic = \"unmuted\"\n").unwrap();

        assert_eq!(
            config.presets["live"],
            Preset {
                cam: Some(CamPermission::Live),
                mic: Some(MicMute::Unmuted),
            }
        );
    }

    #[test]
    fn slate_resolves_to_its_id() {
        let config = Config::parse(
            r#"
            [slates.brb]

            [presets.brb]
            cam = { slate = "brb" }
            "#,
        )
        .unwrap();

        let brb = config.slates.id("brb").unwrap();
        assert_eq!(config.presets["brb"].cam, Some(CamPermission::Slate(brb)));
    }

    #[test]
    fn slate_id_maps_back_to_its_name() {
        let config = Config::parse("[slates.brb]\n").unwrap();

        let brb = config.slates.id("brb").unwrap();
        assert_eq!(config.slates.name(brb), Some("brb"));
    }

    #[test]
    fn slate_can_hold_cam_worker_keys() {
        let config = Config::parse(
            r#"
            [slates.brb]
            image = "brb.png"
            text = "Be right back"
            "#,
        )
        .unwrap();

        assert!(config.slates.id("brb").is_some());
    }

    #[test]
    fn a_preset_without_a_plane_keeps_that_plane() {
        let config = Config::parse("[presets.mute]\nmic = \"muted\"\n").unwrap();

        assert_eq!(config.presets["mute"].cam, None);
    }

    #[test]
    fn unknown_slate_is_rejected() {
        let error = Config::parse("[presets.brb]\ncam = { slate = \"brb\" }\n").unwrap_err();

        assert!(matches!(error, ParseError::UnknownSlate { .. }));
    }

    #[test]
    fn privacy_preset_is_rejected_with_its_line() {
        let error = Config::parse("\n[presets.privacy]\nmic = \"unmuted\"\n").unwrap_err();

        let message = error.to_string();
        assert!(message.contains("built-in preset"), "{message}");
        assert!(message.contains("line 2"), "{message}");
    }

    #[test]
    fn slate_named_like_a_cam_value_is_rejected() {
        let error = Config::parse("[slates.live]\n").unwrap_err();

        assert!(
            error.to_string().contains("cannot be a slate name"),
            "{error}"
        );
    }

    #[test]
    fn unknown_key_is_rejected() {
        let error = Config::parse("[presets.live]\ncamera = \"live\"\n").unwrap_err();

        assert!(matches!(error, ParseError::Toml(_)));
    }

    #[test]
    fn invalid_file_error_names_the_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, b"[presets.privacy]\n").unwrap();

        let error = Config::load(file.path()).unwrap_err();

        let message = error.to_string();
        assert!(
            message.contains(&file.path().display().to_string()),
            "{message}"
        );
    }

    #[test]
    fn privacy_is_a_built_in_preset() {
        let config = Config::parse("").unwrap();

        assert_eq!(
            config.preset("privacy"),
            Ok((PresetRef::Privacy, Preset::PRIVACY))
        );
    }

    #[test]
    fn configured_preset_ref_has_its_name() {
        let config = Config::parse("[presets.mute]\nmic = \"muted\"\n").unwrap();

        let (preset_ref, _) = config.preset("mute").unwrap();
        assert_eq!(preset_ref.as_str(), "mute");
    }

    #[test]
    fn unknown_preset_is_rejected() {
        let config = Config::parse("").unwrap();

        assert_eq!(config.preset("away"), Err(UnknownPreset("away".to_owned())));
    }

    #[test]
    fn unknown_cam_value_is_rejected() {
        let config = Config::parse("[slates.brb]\n").unwrap();

        assert_eq!(
            config.slates.parse_cam("away"),
            Err(UnknownCam("away".to_owned()))
        );
    }
}
