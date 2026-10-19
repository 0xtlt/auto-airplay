#[cfg(target_os = "linux")]
use std::future::Future;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use airplay_client::{AirPlayClient, Device, LiveAudioDecoder};
#[cfg(target_os = "linux")]
use anyhow::{Context, anyhow};
use anyhow::{Result, bail};
#[cfg(target_os = "linux")]
use tokio::signal::unix::{Signal, SignalKind, signal};
#[cfg(target_os = "linux")]
use tokio::time::MissedTickBehavior;

#[cfg(target_os = "linux")]
use crate::capture::{AudioSink, CaptureHandle};
use crate::config::ConfigStore;
#[cfg(target_os = "linux")]
use crate::config::{AppConfig, OutputProtocol};
#[cfg(target_os = "linux")]
use crate::silence::{Action, DEFAULT_RESUME_DURATION, SilenceMonitor, linear_to_dbfs};

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

/// Cadence of the input level check that drives acquiring and releasing.
#[cfg(target_os = "linux")]
const LEVEL_INTERVAL: Duration = Duration::from_millis(250);
#[cfg(target_os = "linux")]
const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2);
/// Lets the capture thread fill the stream buffer before streaming starts.
#[cfg(target_os = "linux")]
const PREFILL: Duration = Duration::from_millis(500);
#[cfg(target_os = "linux")]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(target_os = "linux")]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "linux")]
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Everything needed to (re)connect to the configured AirPlay speaker.
#[cfg(target_os = "linux")]
struct OutputTarget {
    selector: String,
    discovery_seconds: u64,
    render_delay_ms: u32,
    volume: f32,
    sample_rate: u32,
    channels: u8,
    /// Last device that connected successfully; avoids a discovery per resume.
    device: Option<Device>,
}

#[cfg(target_os = "linux")]
async fn run_session(store: &ConfigStore, config: AppConfig) -> Result<SessionExit> {
    if config.output.protocol != OutputProtocol::AirPlay2 {
        bail!("live streaming currently requires `output.protocol = \"air_play2\"`");
    }
    let selector = config
        .output
        .selector
        .clone()
        .context("no AirPlay output configured")?;

    crate::platform::ensure_bluetooth_input(&config.input).await?;
    let input = crate::platform::open_input(&config.input, &config.audio).await?;
    tracing::info!(
        sample_rate = input.format.sample_rate,
        channels = input.format.channels,
        bits = input.format.bits_per_sample,
        "input format negotiated"
    );

    let mut target = OutputTarget {
        selector,
        discovery_seconds: config.output.discovery_timeout_seconds,
        render_delay_ms: config.output.render_delay_ms,
        volume: config.audio.volume.clamp(0.0, 1.0),
        sample_rate: input.format.sample_rate,
        channels: input.format.channels,
        device: None,
    };

    // Capture runs for the whole session; the AirPlay output is only held
    // while there is something to play.
    let sink = AudioSink::new();
    let capture = CaptureHandle::start(input, Arc::clone(&sink))?;

    let idle = Duration::from_secs(config.runtime.idle_silence_seconds);
    let mut monitor = SilenceMonitor::new(
        config.audio.silence_threshold_dbfs,
        idle,
        DEFAULT_RESUME_DURATION,
    );
    let reconnect_delay = Duration::from_secs(config.runtime.reconnect_delay_seconds.max(1));
    if idle.is_zero() {
        tracing::info!("silence release disabled; holding the AirPlay output");
    } else {
        tracing::info!("waiting for sound before acquiring the AirPlay output");
    }

    let changed = wait_for_config_change(store, &config);
    tokio::pin!(changed);
    let mut level_tick = tokio::time::interval(LEVEL_INTERVAL);
    level_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut feedback = tokio::time::interval(FEEDBACK_INTERVAL);
    feedback.set_missed_tick_behavior(MissedTickBehavior::Skip);
    feedback.tick().await;

    let mut signals = ShutdownSignals::new()?;
    let mut client: Option<AirPlayClient> = None;
    let mut retry_at: Option<Instant> = None;

    let result = loop {
        let mut failure: Option<anyhow::Error> = None;

        tokio::select! {
            _ = level_tick.tick() => {
                if !capture.is_running() {
                    break Err(anyhow!("audio capture stopped"));
                }
                let now = Instant::now();
                let level = linear_to_dbfs(sink.take_peak_level());
                if client.is_some() && sink.take_failed() {
                    failure = Some(anyhow!("AirPlay stream ended unexpectedly"));
                } else {
                    match monitor.observe(level, now) {
                        Some(Action::Release) => {
                            tracing::info!(
                                idle_seconds = idle.as_secs(),
                                "input silent; releasing AirPlay output"
                            );
                            if let Some(active) = client.take() {
                                release_output(active, &sink).await;
                            }
                            monitor.mark_released(now);
                            retry_at = None;
                        }
                        Some(Action::Acquire) if retry_at.is_none_or(|at| now >= at) => {
                            tracing::info!("sound detected; acquiring AirPlay output");
                            match acquire_with_interrupts(&mut target, &sink, &mut changed, &mut signals).await {
                                Acquisition::Ready(acquired) => {
                                    client = Some(*acquired);
                                    monitor.mark_active(Instant::now());
                                    retry_at = None;
                                    tracing::info!("streaming started");
                                }
                                Acquisition::Failed(error) => {
                                    tracing::warn!(
                                        %error,
                                        retry_seconds = reconnect_delay.as_secs(),
                                        "cannot acquire AirPlay output"
                                    );
                                    retry_at = Some(Instant::now() + reconnect_delay);
                                }
                                Acquisition::Interrupted(exit) => break exit,
                            }
                        }
                        Some(Action::Acquire) | None => {}
                    }
                }
            }
            _ = feedback.tick(), if client.is_some() => {
                if let Some(active) = client.as_mut()
                    && let Err(error) = active.send_feedback().await
                {
                    failure = Some(error.into());
                }
            }
            next = &mut changed => {
                break next.map(|_| SessionExit::ConfigurationChanged);
            }
            _ = signals.recv() => {
                break Ok(SessionExit::Shutdown);
            }
        }

        if let Some(error) = failure {
            if let Some(active) = client.take() {
                release_output(active, &sink).await;
            }
            let now = Instant::now();
            monitor.mark_released(now);
            if !config.output.auto_reconnect {
                break Err(error);
            }
            tracing::warn!(
                %error,
                retry_seconds = reconnect_delay.as_secs(),
                "AirPlay output lost; reconnecting once sound is present"
            );
            retry_at = Some(now + reconnect_delay);
        }
    };

    // A clean TEARDOWN frees the speaker immediately for other sources.
    if let Some(active) = client.take() {
        release_output(active, &sink).await;
    }
    drop(capture);
    result
}

/// SIGINT (ctrl-c) and SIGTERM (systemd stop/restart). The listeners live for
/// the whole session so a signal received while the loop is busy is not lost,
/// and both lead to a clean TEARDOWN that frees the speaker.
#[cfg(target_os = "linux")]
struct ShutdownSignals {
    interrupt: Signal,
    terminate: Signal,
}

#[cfg(target_os = "linux")]
impl ShutdownSignals {
    fn new() -> Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).context("cannot listen for SIGINT")?,
            terminate: signal(SignalKind::terminate()).context("cannot listen for SIGTERM")?,
        })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}

#[cfg(target_os = "linux")]
enum Acquisition {
    Ready(Box<AirPlayClient>),
    Failed(anyhow::Error),
    /// Shutdown or a configuration change arrived while connecting.
    Interrupted(Result<SessionExit>),
}

/// Acquires the output but stays responsive to shutdown and config changes.
#[cfg(target_os = "linux")]
async fn acquire_with_interrupts(
    target: &mut OutputTarget,
    sink: &AudioSink,
    changed: &mut (impl Future<Output = Result<AppConfig>> + Unpin),
    signals: &mut ShutdownSignals,
) -> Acquisition {
    // Holds a connected client so it can be torn down if the attempt is
    // cancelled midway.
    let mut pending: Option<AirPlayClient> = None;

    let outcome = tokio::select! {
        acquired = acquire_output(target, sink, &mut pending) => match acquired {
            Ok(()) => None,
            Err(error) => Some(Err(error)),
        },
        next = &mut *changed => Some(Ok(next.map(|_| SessionExit::ConfigurationChanged))),
        _ = signals.recv() => Some(Ok(Ok(SessionExit::Shutdown))),
    };

    match outcome {
        None => match pending.take() {
            Some(client) => Acquisition::Ready(Box::new(client)),
            None => Acquisition::Failed(anyhow!("AirPlay client vanished after connecting")),
        },
        Some(Err(error)) => Acquisition::Failed(error),
        Some(Ok(exit)) => {
            sink.detach();
            if let Some(client) = pending.take() {
                release_output(client, sink).await;
            }
            Acquisition::Interrupted(exit)
        }
    }
}

/// Connects to the speaker and starts streaming. On success the connected
/// client is left in `slot`; on failure everything is torn down.
#[cfg(target_os = "linux")]
async fn acquire_output(
    target: &mut OutputTarget,
    sink: &AudioSink,
    slot: &mut Option<AirPlayClient>,
) -> Result<()> {
    let client = slot.insert(connect_output(target).await?);
    if let Err(error) = start_stream(client, target, sink).await {
        sink.detach();
        if let Some(client) = slot.take() {
            disconnect_quietly(client).await;
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn connect_output(target: &mut OutputTarget) -> Result<AirPlayClient> {
    let Some(cached) = target.device.clone() else {
        let device = resolve_target(target).await?;
        let client = connect_device(&device, target.render_delay_ms).await?;
        target.device = Some(device);
        return Ok(client);
    };

    match connect_device(&cached, target.render_delay_ms).await {
        Ok(client) => Ok(client),
        Err(error) => {
            // The speaker may have a new address; look it up again once.
            tracing::debug!(%error, "cached AirPlay device unreachable; rediscovering");
            target.device = None;
            let device = resolve_target(target).await?;
            let client = connect_device(&device, target.render_delay_ms).await?;
            target.device = Some(device);
            Ok(client)
        }
    }
}

#[cfg(target_os = "linux")]
async fn resolve_target(target: &OutputTarget) -> Result<Device> {
    // Discovery itself clamps to 1..=30 seconds; leave some slack on top.
    let limit = Duration::from_secs(target.discovery_seconds.clamp(1, 30) + 5);
    tokio::time::timeout(
        limit,
        crate::output::resolve_device(&target.selector, target.discovery_seconds),
    )
    .await
    .context("AirPlay discovery timed out")?
}

#[cfg(target_os = "linux")]
async fn connect_device(device: &Device, render_delay_ms: u32) -> Result<AirPlayClient> {
    let mut client = AirPlayClient::new()?;
    client.set_render_delay_ms(render_delay_ms);
    tokio::time::timeout(CONNECT_TIMEOUT, client.connect(device))
        .await
        .context("connecting to the AirPlay output timed out")??;
    Ok(client)
}

#[cfg(target_os = "linux")]
async fn start_stream(
    client: &mut AirPlayClient,
    target: &OutputTarget,
    sink: &AudioSink,
) -> Result<()> {
    tokio::time::timeout(REQUEST_TIMEOUT, client.set_volume(target.volume))
        .await
        .context("setting the AirPlay volume timed out")??;

    let (sender, decoder) = LiveAudioDecoder::create_pair(target.sample_rate, target.channels, 64);
    sink.attach(sender);
    tokio::time::sleep(PREFILL).await;
    tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.start_live_streaming_with_decoder(decoder),
    )
    .await
    .context("starting the AirPlay stream timed out")??;
    Ok(())
}

/// Stops streaming and tears the session down. Never fails: the speaker may
/// already be gone, and the daemon must carry on regardless.
#[cfg(target_os = "linux")]
async fn release_output(mut client: AirPlayClient, sink: &AudioSink) {
    sink.detach();
    best_effort("stopping the AirPlay stream", client.stop()).await;
    best_effort("disconnecting from the AirPlay output", client.disconnect()).await;
}

#[cfg(target_os = "linux")]
async fn disconnect_quietly(mut client: AirPlayClient) {
    best_effort("disconnecting from the AirPlay output", client.disconnect()).await;
}

#[cfg(target_os = "linux")]
async fn best_effort<E: std::fmt::Display>(
    what: &str,
    operation: impl Future<Output = std::result::Result<(), E>>,
) {
    match tokio::time::timeout(TEARDOWN_TIMEOUT, operation).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "{what} failed"),
        Err(_) => tracing::warn!("{what} timed out"),
    }
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
    if !config.audio.silence_threshold_dbfs.is_finite()
        || !(-120.0..=0.0).contains(&config.audio.silence_threshold_dbfs)
    {
        bail!("audio silence_threshold_dbfs must be between -120.0 and 0.0");
    }
    Ok(config)
}

#[cfg(target_os = "linux")]
async fn wait_for_config_change(store: &ConfigStore, current: &AppConfig) -> Result<AppConfig> {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let next = store.load()?;
        if &next != current {
            return Ok(next);
        }
    }
}
