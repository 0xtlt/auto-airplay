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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub volume: f32,
    pub preferred_sample_rate: Option<u32>,
    pub channels: u8,
    pub latency_ms: u32,
    /// RMS level (dBFS) below which the input counts as silent. Used together
    /// with `runtime.idle_silence_seconds` to release the AirPlay output.
    pub silence_threshold_dbfs: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            volume: 0.35,
            preferred_sample_rate: None,
            channels: 2,
            latency_ms: 500,
            silence_threshold_dbfs: -40.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    pub reconnect_delay_seconds: u64,
    pub idle_silence_seconds: u64,
    pub log_level: String,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            reconnect_delay_seconds: 3,
            idle_silence_seconds: 30,
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
