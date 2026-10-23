use std::io::Seek;
use std::time::Duration;

use airplay_audio::AudioDecoder;
use airplay_client::{
    AirPlayClient, LiveAudioDecoder, LiveFrameSender, LivePcmFrame, MediaClock, RaopConnection,
};
use airplay_core::Device;
use anyhow::{Context, Result, bail};

use crate::audio::{sine_wave, write_pcm16_wav};
use crate::config::{OutputConfig, OutputProtocol, StreamMode};
use crate::discovery::{DiscoveredOutput, select_output};

/// Sample rate of the test tone sent as a live stream.
const TONE_RATE: u32 = 44_100;
/// Frames per chunk of the live test tone (what the AirPlay sender takes).
const TONE_CHUNK_FRAMES: usize = 1024;

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
            connection.start_streaming(decoder).await?;
            tokio::time::sleep(Duration::from_secs_f32(duration + 1.0)).await;
            connection.stop().await?;
            connection.disconnect().await?;
        }
        OutputProtocol::AirPlay2 if config.stream_mode == StreamMode::Buffered => {
            // No fallback here: this is the place to see why buffered mode
            // does not work with a speaker.
            let latency = Duration::from_millis(config.speaker_buffer_ms.into());
            println!(
                "Buffered mode: the tone starts about {:.1} s after streaming starts.",
                latency.as_secs_f32()
            );
            let mut client = AirPlayClient::new()?;
            client.set_buffered(latency);
            client
                .connect(&device)
                .await
                .context("cannot set up a buffered AirPlay stream")?;
            let clock = client
                .media_clock()
                .context("the speaker's clock is not followed")?;
            let (sender, decoder) = LiveAudioDecoder::create_pair(TONE_RATE, 2, 64);
            let tone = sine_wave(TONE_RATE, 2, frequency, duration);
            let feeder = std::thread::spawn(move || feed_live_tone(&sender, &clock, &tone));
            client.start_live_streaming_with_decoder(decoder).await?;
            tokio::time::sleep(Duration::from_secs_f32(duration + 1.0) + latency).await;
            if let Some(stats) = client.buffered_stats() {
                println!(
                    "Sent {} packets ({} bytes), {} dropped.",
                    stats.sent, stats.bytes, stats.dropped
                );
            }
            let _ = feeder.join();
            client.stop().await?;
            client.disconnect().await?;
        }
        OutputProtocol::AirPlay2 => {
            let mut client = AirPlayClient::new()?;
            client.set_render_delay_ms(config.render_delay_ms);
            client.connect(&device).await?;
            client.play_file(wave.path()).await?;
            tokio::time::sleep(Duration::from_secs_f32(duration + 1.0)).await;
            client.stop().await?;
            client.disconnect().await?;
        }
    }

    println!("Output test completed successfully.");
    Ok(())
}

/// Hands `tone` (interleaved stereo at [`TONE_RATE`]) to a live stream in
/// chunks, at the pace of the speaker's `clock`, as the daemon's playout does.
fn feed_live_tone(sender: &LiveFrameSender, clock: &MediaClock, tone: &[i16]) {
    let start = clock.now_ns();
    for (index, chunk) in tone.chunks(TONE_CHUNK_FRAMES * 2).enumerate() {
        let due = start + (index * TONE_CHUNK_FRAMES) as u64 * 1_000_000_000 / u64::from(TONE_RATE);
        while clock.now_ns() < due {
            std::thread::sleep(Duration::from_millis(2));
        }
        // The sender resamples whole chunks only: pad the last one.
        let mut samples = chunk.to_vec();
        samples.resize(TONE_CHUNK_FRAMES * 2, 0);
        let frame = LivePcmFrame {
            samples,
            channels: 2,
            sample_rate: TONE_RATE,
        };
        if !sender.send(frame) {
            return;
        }
    }
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
