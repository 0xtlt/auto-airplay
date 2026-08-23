#[cfg(target_os = "linux")]
use anyhow::{Context, anyhow};
use anyhow::{Result, bail};

use crate::config::ConfigStore;
#[cfg(target_os = "linux")]
use crate::config::{AppConfig, OutputProtocol};

#[cfg(target_os = "linux")]
pub async fn run(store: ConfigStore) -> Result<()> {
    run_linux(store).await
}

#[cfg(not(target_os = "linux"))]
pub async fn run(_store: ConfigStore) -> Result<()> {
    bail!("the audio bridge can only run on Linux")
}

#[cfg(target_os = "linux")]
async fn run_linux(store: ConfigStore) -> Result<()> {
    let config = validate_config(store.load()?)?;
    tracing::info!(path = %store.path().display(), "starting auto-airplay");

    match run_session(&store, config).await? {
        SessionExit::ConfigurationChanged => {
            tracing::info!("configuration changed; exiting for a clean supervisor restart");
        }
        SessionExit::Shutdown => tracing::info!("shutdown requested"),
    }
    Ok(())
}

#[cfg(target_os = "linux")]
enum SessionExit {
    ConfigurationChanged,
    Shutdown,
}

#[cfg(target_os = "linux")]
async fn run_session(store: &ConfigStore, config: AppConfig) -> Result<SessionExit> {
    if config.output.protocol != OutputProtocol::AirPlay2 {
        bail!("live streaming currently requires `output.protocol = \"air_play2\"`");
    }

    let selector = config
        .output
        .selector
        .as_deref()
        .context("no AirPlay output configured")?;
    let device =
        crate::output::resolve_device(selector, config.output.discovery_timeout_seconds).await?;

    let mut client = airplay_client::AirPlayClient::new()?;
    client.set_render_delay_ms(config.output.render_delay_ms);
    client.connect(&device).await?;
    client
        .set_volume(config.audio.volume.clamp(0.0, 1.0))
        .await?;

    crate::platform::ensure_bluetooth_input(&config.input).await?;
    let input = crate::platform::open_input(&config.input, &config.audio).await?;
    tracing::info!(
        sample_rate = input.format.sample_rate,
        channels = input.format.channels,
        bits = input.format.bits_per_sample,
        "input format negotiated"
    );

    let (sender, decoder) = airplay_client::LiveAudioDecoder::create_pair(
        input.format.sample_rate,
        input.format.channels,
        64,
    );
    let capture = crate::capture::CaptureHandle::start(input, sender)?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    client.start_live_streaming_with_decoder(decoder).await?;
    tracing::info!(output = %device.name, "streaming started");

    let changed = wait_for_config_change(store, &config);
    tokio::pin!(changed);
    let mut feedback = tokio::time::interval(std::time::Duration::from_secs(2));
    feedback.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    feedback.tick().await;

    let result = loop {
        tokio::select! {
            _ = feedback.tick() => {
                if !capture.is_running() {
                    break Err(anyhow!("audio capture stopped"));
                }
                if let Err(error) = client.send_feedback().await {
                    break Err(error.into());
                }
            }
            next = &mut changed => {
                break next.map(|_| SessionExit::ConfigurationChanged);
            }
            _ = tokio::signal::ctrl_c() => {
                break Ok(SessionExit::Shutdown);
            }
        }
    };

    drop(capture);
    result
}

#[cfg(target_os = "linux")]
fn validate_config(config: AppConfig) -> Result<AppConfig> {
    if config.input.bluetooth_address.is_none() && config.input.alsa_device.is_none() {
        bail!("no input configured");
    }
    if config.output.selector.is_none() {
        bail!("no output configured");
    }
    if !(0.0..=1.0).contains(&config.audio.volume) {
        bail!("audio volume must be between 0.0 and 1.0");
    }
    Ok(config)
}

#[cfg(target_os = "linux")]
async fn wait_for_config_change(store: &ConfigStore, current: &AppConfig) -> Result<AppConfig> {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let next = store.load()?;
        if &next != current {
            return Ok(next);
        }
    }
}
