#[cfg(target_os = "linux")]
pub(crate) mod linux;

#[cfg(target_os = "linux")]
pub use linux::{
    doctor, ensure_bluetooth_input, list_bluetooth_inputs, open_input, pair_bluetooth_input,
    probe_input,
};

#[cfg(not(target_os = "linux"))]
mod unsupported {
    use anyhow::{Result, bail};

    use crate::audio::AudioFormat;
    use crate::config::{AudioConfig, ConfigStore, InputConfig};

    pub async fn list_bluetooth_inputs(_seconds: u64) -> Result<()> {
        bail!("Bluetooth discovery is only available on Linux")
    }

    pub async fn probe_input(_input: &InputConfig, _audio: &AudioConfig) -> Result<AudioFormat> {
        bail!("input probing is only available on Linux")
    }

    pub async fn pair_bluetooth_input(_address: &str) -> Result<()> {
        bail!("Bluetooth pairing is only available on Linux")
    }

    pub async fn doctor(_store: &ConfigStore) -> Result<()> {
        println!("Configuration path is accessible");
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
pub use unsupported::{doctor, list_bluetooth_inputs, pair_bluetooth_input, probe_input};
