use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read configuration at {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse configuration at {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("cannot serialize configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("cannot write configuration at {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub version: u32,
    pub input: InputConfig,
    pub output: OutputConfig,
    pub audio: AudioConfig,
    pub runtime: RuntimeConfig,
}

impl AppConfig {
    /// Configuration keys that are still accepted but no longer have any effect.
    pub fn obsolete_fields(&self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.audio.legacy_volume.is_some() {
            fields.push("audio.volume");
        }
        fields
    }

    /// Checks the settings that drive sound detection. The daemon refuses to
    /// start with a configuration that fails this check.
    pub fn validate_detection(&self) -> Result<(), String> {
        let silence = self.audio.silence_threshold_dbfs;
        if !is_valid_dbfs(silence) {
            return Err("audio silence_threshold_dbfs must be between -120.0 and 0.0".into());
        }
        if let Some(sound) = self.audio.sound_threshold_dbfs {
            if !is_valid_dbfs(sound) {
                return Err("audio sound_threshold_dbfs must be between -120.0 and 0.0".into());
            }
            if sound < silence {
                return Err(format!(
                    "audio sound_threshold_dbfs ({sound}) must not be below silence_threshold_dbfs ({silence})"
                ));
            }
        }
        if !SOUND_CONFIRM_MS_RANGE.contains(&self.runtime.sound_confirm_ms) {
            return Err(format!(
                "runtime sound_confirm_ms must be between {} and {}",
                SOUND_CONFIRM_MS_RANGE.start(),
                SOUND_CONFIRM_MS_RANGE.end()
            ));
        }
        Ok(())
    }
}

fn is_valid_dbfs(value: f32) -> bool {
    value.is_finite() && (-120.0..=0.0).contains(&value)
}

/// Accepted values of `runtime.sound_confirm_ms`.
pub const SOUND_CONFIRM_MS_RANGE: std::ops::RangeInclusive<u64> = 250..=60_000;

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 1,
            input: InputConfig::default(),
            output: OutputConfig::default(),
            audio: AudioConfig::default(),
            runtime: RuntimeConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputConfig {
    pub kind: InputKind,
    pub bluetooth_address: Option<String>,
    pub alsa_device: Option<String>,
    pub display_name: Option<String>,
    pub auto_connect: bool,
    pub send_avrcp_play: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            kind: InputKind::Bluetooth,
            bluetooth_address: None,
            alsa_device: None,
            display_name: None,
            auto_connect: true,
            send_avrcp_play: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Bluetooth,
    Alsa,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    pub selector: Option<String>,
    pub protocol: OutputProtocol,
    pub auto_reconnect: bool,
    pub discovery_timeout_seconds: u64,
    pub render_delay_ms: u32,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            selector: None,
            protocol: OutputProtocol::AirPlay2,
            auto_reconnect: true,
            discovery_timeout_seconds: 4,
            render_delay_ms: 250,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputProtocol {
    AirPlay1,
    AirPlay2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// Obsolete: auto-airplay never changes the speaker volume. Older
    /// configurations may still carry `audio.volume`; any value is accepted,
    /// ignored, and never written back.
    #[serde(default, rename = "volume", skip_serializing)]
    pub legacy_volume: Option<serde::de::IgnoredAny>,
    pub preferred_sample_rate: Option<u32>,
    pub channels: u8,
    pub latency_ms: u32,
    /// RMS level (dBFS) below which the input counts as silent. Used together
    /// with `runtime.idle_silence_seconds` to release the AirPlay output.
    pub silence_threshold_dbfs: f32,
    /// RMS level (dBFS) the input must reach, and keep for
    /// `runtime.sound_confirm_ms`, before the AirPlay output is acquired.
    /// Defaults to `silence_threshold_dbfs + 6.0` (capped at 0.0). Must not be
    /// below `silence_threshold_dbfs`: the gap between the two ignores noise.
    pub sound_threshold_dbfs: Option<f32>,
}

impl AudioConfig {
    /// The level that counts as the start of sound, with the default applied.
    pub fn effective_sound_threshold_dbfs(&self) -> f32 {
        self.sound_threshold_dbfs
            .unwrap_or_else(|| (self.silence_threshold_dbfs + DEFAULT_SOUND_MARGIN_DB).min(0.0))
    }
}

/// Distance above `silence_threshold_dbfs` of the default sound threshold.
const DEFAULT_SOUND_MARGIN_DB: f32 = 6.0;

/// Equality ignores the obsolete `volume` field, so adding, removing or editing
/// it never looks like a configuration change (which would restart playback).
impl PartialEq for AudioConfig {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            legacy_volume: _,
            preferred_sample_rate,
            channels,
            latency_ms,
            silence_threshold_dbfs,
            sound_threshold_dbfs,
        } = self;
        *preferred_sample_rate == other.preferred_sample_rate
            && *channels == other.channels
            && *latency_ms == other.latency_ms
            && *silence_threshold_dbfs == other.silence_threshold_dbfs
            && *sound_threshold_dbfs == other.sound_threshold_dbfs
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            legacy_volume: None,
            preferred_sample_rate: None,
            channels: 2,
            latency_ms: 500,
            silence_threshold_dbfs: -40.0,
            sound_threshold_dbfs: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    pub reconnect_delay_seconds: u64,
    pub idle_silence_seconds: u64,
    /// How long the input must stay at or above the sound threshold, without a
    /// single dip below it, before the AirPlay output is acquired.
    pub sound_confirm_ms: u64,
    pub log_level: String,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            reconnect_delay_seconds: 3,
            idle_silence_seconds: 30,
            sound_confirm_ms: 2_000,
            log_level: "info".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<AppConfig, ConfigError> {
        let raw = fs::read_to_string(&self.path).map_err(|source| ConfigError::Read {
            path: self.path.clone(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: self.path.clone(),
            source,
        })
    }

    pub fn load_or_create(&self) -> Result<AppConfig, ConfigError> {
        match self.load() {
            Ok(config) => Ok(config),
            Err(ConfigError::Read { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                let config = AppConfig::default();
                self.save(&config)?;
                Ok(config)
            }
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, config: &AppConfig) -> Result<(), ConfigError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: self.path.clone(),
            source,
        })?;

        let raw = toml::to_string_pretty(config)?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| ConfigError::Write {
                path: self.path.clone(),
                source,
            })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|source| ConfigError::Write {
                    path: self.path.clone(),
                    source,
                })?;
        }

        temporary
            .write_all(raw.as_bytes())
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(|source| ConfigError::Write {
                path: self.path.clone(),
                source,
            })?;

        temporary
            .persist(&self.path)
            .map_err(|error| ConfigError::Write {
                path: self.path.clone(),
                source: error.error,
            })?;
        Ok(())
    }
}
