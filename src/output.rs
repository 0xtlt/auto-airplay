use std::io::Seek;
use std::time::Duration;

use airplay_audio::AudioDecoder;
use airplay_client::{AirPlayClient, RaopConnection};
use airplay_core::Device;
use anyhow::{Context, Result, bail};

use crate::audio::{sine_wave, write_pcm16_wav};
use crate::config::{OutputConfig, OutputProtocol};
use crate::discovery::{DiscoveredOutput, select_output};

pub async fn discover_outputs(seconds: u64) -> Result<Vec<DiscoveredOutput>> {
    let devices = discover_devices(seconds).await?;
    let mut outputs = devices.iter().map(device_summary).collect::<Vec<_>>();
    outputs.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(outputs)
}

pub async fn test_output(
    selector: &str,
    frequency: f32,
    duration: f32,
    config: &OutputConfig,
) -> Result<()> {
    if !(20.0..=20_000.0).contains(&frequency) {
        bail!("test frequency must be between 20 and 20000 Hz");
    }
    if !(0.25..=30.0).contains(&duration) {
        bail!("test duration must be between 0.25 and 30 seconds");
    }

    println!("Discovering AirPlay receivers...");
    let devices = discover_devices(config.discovery_timeout_seconds).await?;
    let summaries = devices.iter().map(device_summary).collect::<Vec<_>>();
    let selected = select_output(&summaries, selector)
        .with_context(|| format!("AirPlay receiver `{selector}` was not found"))?;
    let device = devices
        .into_iter()
        .find(|device| device.id.to_mac_string().replace(':', "") == selected.id)
        .context("the selected receiver disappeared")?;

    let mut wave = tempfile::NamedTempFile::new()?;
    let samples = sine_wave(44_100, 2, frequency, duration);
    write_pcm16_wav(wave.as_file_mut(), 44_100, 2, &samples)?;
    wave.as_file_mut().rewind()?;
    wave.as_file().sync_all()?;

    println!(
        "Playing a {:.0} Hz test tone on {} for {:.1} seconds...",
        frequency, selected.name, duration
    );

    match config.protocol {
        OutputProtocol::AirPlay1 => {
            let stream_config = airplay_core::StreamConfig::default();
            let decoder = AudioDecoder::open(wave.path())?;
            let mut connection = RaopConnection::connect(device, stream_config).await?;
            connection.set_render_delay_ms(config.render_delay_ms);
            connection.setup().await?;
            connection.set_volume(0.20).await?;
            connection.start_streaming(decoder).await?;
            tokio::time::sleep(Duration::from_secs_f32(duration + 1.0)).await;
            connection.stop().await?;
            connection.disconnect().await?;
        }
        OutputProtocol::AirPlay2 => {
            let mut client = AirPlayClient::new()?;
            client.set_render_delay_ms(config.render_delay_ms);
            client.connect(&device).await?;
            client.set_volume(0.20).await?;
            client.play_file(wave.path()).await?;
            tokio::time::sleep(Duration::from_secs_f32(duration + 1.0)).await;
            client.stop().await?;
            client.disconnect().await?;
        }
    }

    println!("Output test completed successfully.");
    Ok(())
}

async fn discover_devices(seconds: u64) -> Result<Vec<Device>> {
    let client = AirPlayClient::new()?;
    let devices = client
        .discover(Duration::from_secs(seconds.clamp(1, 30)))
        .await?;
    if devices.is_empty() {
        bail!("no AirPlay receivers were discovered");
    }
    Ok(devices)
}

#[cfg(target_os = "linux")]
pub(crate) async fn resolve_device(selector: &str, seconds: u64) -> Result<Device> {
    let devices = discover_devices(seconds).await?;
    let summaries = devices.iter().map(device_summary).collect::<Vec<_>>();
    let selected = select_output(&summaries, selector)
        .with_context(|| format!("AirPlay receiver `{selector}` was not found"))?;
    devices
        .into_iter()
        .find(|device| device.id.to_mac_string().replace(':', "") == selected.id)
        .context("the selected receiver disappeared")
}

fn device_summary(device: &Device) -> DiscoveredOutput {
    DiscoveredOutput {
        name: device.name.clone(),
        id: device.id.to_mac_string().replace(':', ""),
        model: device.model.clone(),
        addresses: device.addresses.clone(),
        port: device.raop_port.unwrap_or(device.port),
        supports_airplay2: device.supports_airplay2(),
    }
}
