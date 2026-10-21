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
        if self.audio.legacy_latency_ms.is_some() {
            fields.push("audio.latency_ms");
        }
        fields
    }

    /// Checks the settings that drive sound detection, preemption handling and
    /// input buffering. The daemon refuses to start with a configuration that
    /// fails this check.
    pub fn validate_detection(&self) -> Result<(), String> {
        if !JITTER_BUFFER_MS_RANGE.contains(&self.audio.jitter_buffer_ms) {
            return Err(format!(
                "audio jitter_buffer_ms must be between {} and {}",
                JITTER_BUFFER_MS_RANGE.start(),
                JITTER_BUFFER_MS_RANGE.end()
            ));
        }
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
        let runtime = &self.runtime;
        if !PREEMPTED_SILENCE_SECONDS_RANGE.contains(&runtime.preempted_silence_seconds) {
            return Err(format!(
                "runtime preempted_silence_seconds must be between {} and {}",
                PREEMPTED_SILENCE_SECONDS_RANGE.start(),
                PREEMPTED_SILENCE_SECONDS_RANGE.end()
            ));
        }
        if runtime.preemption_backoff_seconds < 1 {
            return Err("runtime preemption_backoff_seconds must be at least 1".into());
        }
        if runtime.preemption_backoff_max_seconds < runtime.preemption_backoff_seconds {
            return Err(
                "runtime preemption_backoff_max_seconds must not be below preemption_backoff_seconds"
                    .into(),
            );
        }
        if runtime.preemption_backoff_max_seconds > MAX_PREEMPTION_BACKOFF_SECONDS {
            return Err(format!(
                "runtime preemption_backoff_max_seconds must be at most {MAX_PREEMPTION_BACKOFF_SECONDS}"
            ));
        }
        Ok(())
    }
}

/// Why an obsolete key is ignored, and what replaces it.
pub fn obsolete_field_hint(field: &str) -> &'static str {
    match field {
        "audio.volume" => "auto-airplay never changes the speaker volume",
        "audio.latency_ms" => "use `audio.jitter_buffer_ms` for input buffering",
        _ => "it has no effect any more",
    }
}

fn is_valid_dbfs(value: f32) -> bool {
    value.is_finite() && (-120.0..=0.0).contains(&value)
}

/// Accepted values of `audio.jitter_buffer_ms`: from the least that still
/// leaves room to conceal a lost packet (see
/// [`MIN_JITTER_BUFFER_MS`](crate::plc::MIN_JITTER_BUFFER_MS)) to ten seconds,
/// far beyond what a turntable needs.
pub const JITTER_BUFFER_MS_RANGE: std::ops::RangeInclusive<u32> =
    crate::plc::MIN_JITTER_BUFFER_MS..=10_000;

/// Default of `audio.jitter_buffer_ms`.
pub const DEFAULT_JITTER_BUFFER_MS: u32 = 400;

/// Accepted values of `runtime.preempted_silence_seconds`.
pub const PREEMPTED_SILENCE_SECONDS_RANGE: std::ops::RangeInclusive<u64> = 1..=3_600;

/// Upper bound of `runtime.preemption_backoff_max_seconds` (one day).
pub const MAX_PREEMPTION_BACKOFF_SECONDS: u64 = 86_400;

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
    /// Obsolete: the value was never used. Any value is accepted, ignored, and
    /// never written back; see `jitter_buffer_ms`.
    #[serde(default, rename = "latency_ms", skip_serializing)]
    pub legacy_latency_ms: Option<serde::de::IgnoredAny>,
    /// Audio (ms) queued between the Bluetooth input and AirPlay. It absorbs
    /// delays and is where lost packets are concealed; it adds that much
    /// latency (on top of `output.render_delay_ms`). Fixed: it never adapts.
    /// Must be within [`JITTER_BUFFER_MS_RANGE`].
    pub jitter_buffer_ms: u32,
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

/// Equality ignores the obsolete `volume` and `latency_ms` fields, so adding,
/// removing or editing them never looks like a configuration change (which
/// would restart playback).
impl PartialEq for AudioConfig {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            legacy_volume: _,
            preferred_sample_rate,
            legacy_latency_ms: _,
            channels,
            jitter_buffer_ms,
            silence_threshold_dbfs,
            sound_threshold_dbfs,
        } = self;
        *preferred_sample_rate == other.preferred_sample_rate
            && *channels == other.channels
            && *jitter_buffer_ms == other.jitter_buffer_ms
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
            legacy_latency_ms: None,
            jitter_buffer_ms: DEFAULT_JITTER_BUFFER_MS,
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
    /// After another AirPlay source takes over the speaker, the input must be
    /// silent for this long (seconds) before new sound may re-acquire it.
    pub preempted_silence_seconds: u64,
    /// Pause (seconds) before re-acquiring after a preemption; doubles for
    /// every consecutive one, up to `preemption_backoff_max_seconds`.
    pub preemption_backoff_seconds: u64,
    pub preemption_backoff_max_seconds: u64,
    pub log_level: String,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            reconnect_delay_seconds: 3,
            idle_silence_seconds: 30,
            sound_confirm_ms: 2_000,
            preempted_silence_seconds: 5,
            preemption_backoff_seconds: 30,
            preemption_backoff_max_seconds: 600,
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
