use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use anyhow::{Context, Result, bail};
use futures::StreamExt;

use crate::audio::{AudioFormat, FormatCandidate, select_best_format};
use crate::config::{AudioConfig, ConfigStore, InputConfig, InputKind};
use crate::discovery::select_output;

const COMMON_SAMPLE_RATES: [u32; 8] = [
    192_000, 96_000, 88_200, 48_000, 44_100, 32_000, 24_000, 16_000,
];

pub async fn list_bluetooth_inputs(seconds: u64) -> Result<()> {
    let session = bluer::Session::new()
        .await
        .context("cannot connect to BlueZ")?;
    let adapter = session
        .default_adapter()
        .await
        .context("no Bluetooth adapter found")?;
    adapter.set_powered(true).await?;

    let mut discovery = adapter.discover_devices().await?;
    let deadline = Instant::now() + Duration::from_secs(seconds.clamp(1, 30));
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if tokio::time::timeout(remaining.min(Duration::from_millis(500)), discovery.next())
            .await
            .is_err()
        {
            continue;
        }
    }
    drop(discovery);

    let mut found = 0_usize;
    for address in adapter.device_addresses().await? {
        let device = adapter.device(address)?;
        let name = device
            .alias()
            .await
            .ok()
            .or(device.name().await.ok().flatten())
            .unwrap_or_else(|| "Unknown device".into());
        let paired = device.is_paired().await.unwrap_or(false);
        let connected = device.is_connected().await.unwrap_or(false);
        let uuids = device.uuids().await.ok().flatten().unwrap_or_default();
        let audio_source = uuids
            .iter()
            .any(|uuid| uuid.to_string().starts_with("0000110a-"));

        if audio_source || paired || connected {
            println!(
                "{}\t{}\tpaired={}\tconnected={}\taudio_source={}",
                address, name, paired, connected, audio_source
            );
            found += 1;
        }
    }

    if found == 0 {
        println!("No Bluetooth audio sources found.");
    }
    Ok(())
}

pub async fn probe_input(input: &InputConfig, audio: &AudioConfig) -> Result<AudioFormat> {
    Ok(open_input(input, audio).await?.format)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleEncoding {
    Signed16,
    Signed32,
}

pub struct OpenedInput {
    pub pcm: PCM,
    pub format: AudioFormat,
    pub encoding: SampleEncoding,
    pub period_frames: usize,
}

pub async fn open_input(input: &InputConfig, audio: &AudioConfig) -> Result<OpenedInput> {
    let native_bluetooth_format = match input.kind {
        InputKind::Bluetooth => Some(bluealsa_native_format(input).await?),
        InputKind::Alsa => None,
    };
    open_input_with_hint(input, audio, native_bluetooth_format)
}

fn open_input_with_hint(
    input: &InputConfig,
    audio: &AudioConfig,
    native_hint: Option<AudioFormat>,
) -> Result<OpenedInput> {
    let device = alsa_device_name(input)?;
    let pcm = PCM::new(&device, Direction::Capture, true)
        .with_context(|| format!("cannot open ALSA capture device `{device}`"))?;
    let params = HwParams::any(&pcm)?;
    params.set_access(Access::RWInterleaved)?;

    let channels = native_hint
        .map(|format| format.channels as u32)
        .unwrap_or_else(|| audio.channels.max(1) as u32);
    let negotiated_channels = params.set_channels_near(channels)?;

    let (alsa_format, encoding) = if params.test_format(Format::s16()).is_ok() {
        params.set_format(Format::s16())?;
        (Format::s16(), SampleEncoding::Signed16)
    } else if params.test_format(Format::s32()).is_ok() {
        params.set_format(Format::s32())?;
        (Format::s32(), SampleEncoding::Signed32)
    } else {
        bail!("input supports neither signed 16-bit nor signed 32-bit PCM")
    };

    let bits = if alsa_format == Format::s16() { 16 } else { 32 };
    let selected_rate = if let Some(native) = native_hint {
        native.sample_rate
    } else {
        let mut candidates = BTreeSet::new();
        for rate in COMMON_SAMPLE_RATES {
            if params.test_rate(rate).is_ok() {
                candidates.insert(rate);
            }
        }

        if candidates.is_empty() {
            let maximum = params.get_rate_max()?;
            let minimum = params.get_rate_min()?;
            candidates.insert(maximum.max(minimum));
        }

        let formats = candidates
            .into_iter()
            .map(|rate| FormatCandidate::new(rate, negotiated_channels as u8, bits))
            .collect::<Vec<_>>();
        select_best_format(&formats, audio.preferred_sample_rate)
            .context("input did not expose a usable audio format")?
            .sample_rate
    };

    let actual_rate = params.set_rate_near(selected_rate, ValueOr::Nearest)?;
    let period_frames = params.set_period_size_near(1024, ValueOr::Nearest)? as usize;
    params.set_buffer_size_near(4096)?;
    pcm.hw_params(&params)?;
    pcm.prepare()?;

    let current = pcm.hw_params_current()?;
    let format = AudioFormat::new(
        current.get_rate().unwrap_or(actual_rate),
        current.get_channels().unwrap_or(negotiated_channels) as u8,
        bits,
    );
    drop(current);
    drop(params);

    Ok(OpenedInput {
        pcm,
        format,
        encoding,
        period_frames,
    })
}

async fn bluealsa_native_format(input: &InputConfig) -> Result<AudioFormat> {
    let address = input
        .bluetooth_address
        .as_deref()
        .context("no Bluetooth input address configured")?
        .to_ascii_uppercase()
        .replace(':', "_");
    let path = format!("/org/bluealsa/hci0/dev_{address}/a2dpsnk/source");
    let connection = zbus::Connection::system().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.bluealsa",
        path.as_str(),
        "org.bluealsa.PCM1",
    )
    .await
    .context("BlueALSA PCM is unavailable; connect and start the input first")?;

    let sample_rate = proxy
        .get_property::<u32>("Sampling")
        .await
        .context("BlueALSA did not report its native sample rate")?;
    let channels = proxy
        .get_property::<u8>("Channels")
        .await
        .context("BlueALSA did not report its channel count")?;
    Ok(AudioFormat::new(sample_rate, channels, 16))
}

pub async fn ensure_bluetooth_input(input: &InputConfig) -> Result<()> {
    if input.kind != InputKind::Bluetooth {
        return Ok(());
    }

    let address = input
        .bluetooth_address
        .as_deref()
        .context("no Bluetooth input address configured")?
        .parse::<bluer::Address>()
        .context("invalid Bluetooth input address")?;
    let session = bluer::Session::new()
        .await
        .context("cannot connect to BlueZ")?;
    let adapter = session
        .default_adapter()
        .await
        .context("no Bluetooth adapter")?;
    adapter.set_powered(true).await?;
    let device = adapter.device(address)?;

    if !device.is_paired().await.unwrap_or(false) {
        bail!("Bluetooth input is not paired; run `auto-airplay input pair {address}`")
    }
    if !device.is_trusted().await.unwrap_or(false) {
        device.set_trusted(true).await?;
    }
    if !device.is_connected().await.unwrap_or(false) {
        device
            .connect()
            .await
            .with_context(|| format!("cannot connect Bluetooth input {address}"))?;
    }

    if input.send_avrcp_play {
        send_avrcp_play(address).await?;
    }
    Ok(())
}

pub async fn pair_bluetooth_input(address: &str) -> Result<()> {
    let address = address
        .parse::<bluer::Address>()
        .context("invalid Bluetooth input address")?;
    let session = bluer::Session::new()
        .await
        .context("cannot connect to BlueZ")?;
    let adapter = session
        .default_adapter()
        .await
        .context("no Bluetooth adapter")?;
    adapter.set_powered(true).await?;
    adapter.set_pairable(true).await?;
    let device = adapter.device(address)?;

    if !device.is_paired().await.unwrap_or(false) {
        tokio::time::timeout(Duration::from_secs(60), device.pair())
            .await
            .context("Bluetooth pairing timed out")??;
    }
    device.set_trusted(true).await?;
    if !device.is_connected().await.unwrap_or(false) {
        device.connect().await?;
    }
    Ok(())
}

async fn send_avrcp_play(address: bluer::Address) -> Result<()> {
    let device = address.to_string().replace(':', "_");
    let path = format!("/org/bluez/hci0/dev_{device}/player0");
    let connection = zbus::Connection::system().await?;

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_error = None;
    while Instant::now() < deadline {
        match zbus::Proxy::new(
            &connection,
            "org.bluez",
            path.as_str(),
            "org.bluez.MediaPlayer1",
        )
        .await
        {
            Ok(proxy) => match proxy.call_method("Play", &()).await {
                Ok(_) => return Ok(()),
                Err(error) => last_error = Some(error.to_string()),
            },
            Err(error) => last_error = Some(error.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    bail!(
        "Bluetooth input connected but AVRCP Play failed: {}",
        last_error.unwrap_or_else(|| "MediaPlayer1 did not appear".into())
    )
}

pub async fn doctor(store: &ConfigStore) -> Result<()> {
    let config = store
        .load()
        .context("configuration is missing or invalid")?;
    println!("[ok] config: {}", store.path().display());
    for field in config.obsolete_fields() {
        println!(
            "[warn] config: `{field}` is obsolete and ignored; {}",
            crate::config::obsolete_field_hint(field)
        );
    }

    let session = bluer::Session::new()
        .await
        .context("BlueZ D-Bus is unavailable")?;
    let adapter = session
        .default_adapter()
        .await
        .context("no Bluetooth adapter")?;
    println!("[ok] bluetooth: {}", adapter.name());

    ensure_bluetooth_input(&config.input).await?;
    let format = probe_input(&config.input, &config.audio).await?;
    println!(
        "[ok] input: {} Hz / {} ch / {}-bit",
        format.sample_rate, format.channels, format.bits_per_sample
    );

    let selector = config
        .output
        .selector
        .as_deref()
        .context("no AirPlay output configured")?;
    let outputs = crate::output::discover_outputs(config.output.discovery_timeout_seconds).await?;
    let output = select_output(&outputs, selector)
        .with_context(|| format!("AirPlay output `{selector}` is not reachable"))?;
    println!("[ok] output: {} ({})", output.name, output.model);
    println!("Ready.");
    Ok(())
}

pub(crate) fn alsa_device_name(input: &InputConfig) -> Result<String> {
    match input.kind {
        InputKind::Bluetooth => {
            let address = input
                .bluetooth_address
                .as_deref()
                .context("no Bluetooth input address configured")?;
            Ok(format!(
                "bluealsa:DEV={},PROFILE=a2dp",
                address.to_ascii_uppercase()
            ))
        }
        InputKind::Alsa => input
            .alsa_device
            .clone()
            .context("no ALSA input device configured"),
    }
}
