#[cfg(target_os = "linux")]
use std::future::Future;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU64, Ordering};
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
use crate::config::{AppConfig, OutputProtocol, StreamMode};
#[cfg(target_os = "linux")]
use crate::preemption::{
    ExitKind, PREEMPTIONS_ENV, PreemptionGate, backoff_delay, count_after, decode_preemptions,
    encode_preemptions, is_preemption_kind,
};
#[cfg(target_os = "linux")]
use crate::shutdown::ShutdownFlag;
#[cfg(target_os = "linux")]
use crate::silence::{Action, SilenceMonitor, linear_to_dbfs};

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
    for field in config.obsolete_fields() {
        tracing::warn!(
            field,
            "obsolete configuration field `{field}` ignored: {}",
            crate::config::obsolete_field_hint(field)
        );
    }
    let started = Instant::now();
    let recycled = std::env::var_os(RECYCLED_ENV).is_some();
    let preemptions = decode_preemptions(
        std::env::var_os(PREEMPTIONS_ENV)
            .as_deref()
            .and_then(|raw| raw.to_str()),
    );
    tracing::info!(
        path = %store.path().display(),
        recycled,
        preemptions,
        "starting auto-airplay"
    );

    match run_session(&store, config, recycled, preemptions, started).await? {
        SessionExit::ConfigurationChanged => {
            tracing::info!("configuration changed; exiting for a clean supervisor restart");
        }
        SessionExit::Shutdown => tracing::info!("shutdown requested"),
        SessionExit::Recycle(shutdown, preemptions) => {
            tracing::info!("restarting in a fresh process for the next AirPlay session");
            return recycle_process(&shutdown, preemptions);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
enum SessionExit {
    ConfigurationChanged,
    Shutdown,
    /// The AirPlay session is over (released, lost, or could not start); run
    /// the next one in a fresh process image, unless a shutdown arrives first.
    /// The count is the consecutive preemptions to carry over (0 for none).
    Recycle(ShutdownFlag, u32),
}

/// Set on processes started by [`recycle_process`].
#[cfg(target_os = "linux")]
const RECYCLED_ENV: &str = "AUTO_AIRPLAY_RECYCLED";

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
const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(2);
/// Shared budget for stop + disconnect. Worst-case shutdown is a pending
/// feedback (2 s) + teardown (4 s) + runtime shutdown (2 s), which fits in
/// the service's `TimeoutStopSec=10`.
#[cfg(target_os = "linux")]
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(4);

/// Everything needed to connect to the configured AirPlay speaker.
#[cfg(target_os = "linux")]
struct OutputTarget {
    selector: String,
    discovery_seconds: u64,
    render_delay_ms: u32,
    stream_mode: StreamMode,
    buffered_latency: Duration,
    sample_rate: u32,
    channels: u8,
}

/// Runs at most one AirPlay session: wait for sound, acquire the speaker,
/// stream until the input goes silent, then release it.
///
/// `preemptions` is the number of consecutive times another AirPlay source took
/// the speaker over (from [`PREEMPTIONS_ENV`]). While it is positive, the
/// speaker is not re-acquired before the backoff (counted from `started`) has
/// elapsed, the input has been silent for `runtime.preempted_silence_seconds`,
/// and new sound has been confirmed. A preemption is not an error: it always
/// recycles into that wait, whatever `output.auto_reconnect` says.
///
/// The pinned AirPlay library leaks a polling thread, sockets and an mDNS
/// daemon per connection, and none of them stop on disconnect. Instead of
/// reconnecting in-process, the session ends with [`SessionExit::Recycle`] and
/// the next one runs in a fresh process image, which bounds those leaks.
#[cfg(target_os = "linux")]
async fn run_session(
    store: &ConfigStore,
    config: AppConfig,
    recycled: bool,
    preemptions: u32,
    started: Instant,
) -> Result<SessionExit> {
    if config.output.protocol != OutputProtocol::AirPlay2 {
        bail!("live streaming currently requires `output.protocol = \"air_play2\"`");
    }
    let selector = config
        .output
        .selector
        .clone()
        .context("no AirPlay output configured")?;

    // AVRCP Play is only sent on a real start: after a silence release the
    // source was stopped on purpose (e.g. a paused phone) and must stay so.
    let mut input_config = config.input.clone();
    input_config.send_avrcp_play &= !recycled;
    crate::platform::ensure_bluetooth_input(&input_config).await?;
    let input = crate::platform::open_input(&config.input, &config.audio).await?;
    tracing::info!(
        sample_rate = input.format.sample_rate,
        channels = input.format.channels,
        bits = input.format.bits_per_sample,
        "input format negotiated"
    );

    let target = OutputTarget {
        selector,
        discovery_seconds: config.output.discovery_timeout_seconds,
        render_delay_ms: config.output.render_delay_ms,
        stream_mode: config.output.stream_mode,
        buffered_latency: Duration::from_millis(config.output.speaker_buffer_ms.into()),
        sample_rate: input.format.sample_rate,
        channels: input.format.channels,
    };
    if target.stream_mode == StreamMode::Buffered {
        tracing::info!(
            speaker_buffer_ms = config.output.speaker_buffer_ms,
            "AirPlay 2 buffered mode: the speaker holds this much audio"
        );
    }

    // Capture runs for the whole session; the AirPlay output is only held
    // while there is something to play.
    let sink = AudioSink::new();
    let capture = CaptureHandle::start(input, Arc::clone(&sink), config.audio.jitter_buffer_ms)?;

    let idle = Duration::from_secs(config.runtime.idle_silence_seconds);
    let sound_threshold = config.audio.effective_sound_threshold_dbfs();
    let sound_confirm = Duration::from_millis(config.runtime.sound_confirm_ms);
    let mut monitor = SilenceMonitor::new(
        config.audio.silence_threshold_dbfs,
        sound_threshold,
        idle,
        sound_confirm,
    );
    let reconnect_delay = Duration::from_secs(config.runtime.reconnect_delay_seconds.max(1));
    let backoff_base = Duration::from_secs(config.runtime.preemption_backoff_seconds);
    let backoff_max = Duration::from_secs(config.runtime.preemption_backoff_max_seconds);
    if preemptions > 0 {
        let backoff = backoff_delay(backoff_base, backoff_max, preemptions);
        let quiet = Duration::from_secs(config.runtime.preempted_silence_seconds);
        monitor = monitor.with_gate(PreemptionGate::new(quiet, started + backoff));
        tracing::info!(
            consecutive_preemptions = preemptions,
            backoff_seconds = backoff.as_secs(),
            silence_seconds = quiet.as_secs(),
            "AirPlay output was taken by another source; waiting for the backoff, then silence, then new sound before reconnecting"
        );
    } else if idle.is_zero() {
        tracing::info!("silence release disabled; acquiring the AirPlay output");
    } else {
        tracing::info!("waiting for sound before acquiring the AirPlay output");
    }

    let changed = wait_for_config_change(store, &config);
    tokio::pin!(changed);
    let mut signals = ShutdownSignals::new()?;
    let mut level_tick = tokio::time::interval(LEVEL_INTERVAL);
    level_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut feedback = tokio::time::interval(FEEDBACK_INTERVAL);
    feedback.set_missed_tick_behavior(MissedTickBehavior::Skip);
    feedback.tick().await;

    let mut client: Option<AirPlayClient> = None;
    // Timestamps for the failure diagnostics.
    let mut stream_started: Option<Instant> = None;
    // Diagnostics only. `send_feedback` returns Ok(()) on its own internal 2 s
    // timeout and for any receiver status, so this is the last time it returned
    // Ok, not proof that the receiver answered the feedback.
    let mut last_feedback_ok_return: Option<Instant> = None;
    // Logged once, when a preemption gate has seen enough silence.
    let mut quiet_logged = false;
    // Pause before recycling after a failure, so a missing speaker is not
    // hammered with attempts.
    let mut retry_delay: Option<Duration> = None;

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
                    failure = Some(anyhow!("AirPlay stream stopped accepting audio"));
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
                            let next = count_after(
                                ExitKind::SilenceRelease,
                                preemptions,
                                Duration::ZERO,
                                backoff_max,
                            );
                            break Ok(signals.recycle(next));
                        }
                        Some(Action::Acquire) => {
                            let (min_dbfs, max_dbfs) = monitor.sound_levels().unzip();
                            tracing::info!(
                                min_dbfs = min_dbfs.map(round_db),
                                max_dbfs = max_dbfs.map(round_db),
                                sound_threshold_dbfs = round_db(sound_threshold),
                                confirm_ms = sound_confirm.as_millis() as u64,
                                "sound detected; acquiring AirPlay output"
                            );
                            match acquire_with_interrupts(&target, &sink, &mut changed, &mut signals).await {
                                Acquisition::Ready(acquired) => {
                                    client = Some(*acquired);
                                    stream_started = Some(Instant::now());
                                    last_feedback_ok_return = None;
                                    sink.arm();
                                    monitor.mark_active(Instant::now());
                                    tracing::info!("streaming started");
                                }
                                Acquisition::Failed(error) => {
                                    tracing::warn!(
                                        error = format!("{error:#}"),
                                        retry_seconds = reconnect_delay.as_secs(),
                                        "cannot acquire AirPlay output"
                                    );
                                    retry_delay = Some(reconnect_delay);
                                    // A failed attempt says nothing about the other
                                    // source: the gate must apply again.
                                    let next = count_after(
                                        ExitKind::AcquireFailed,
                                        preemptions,
                                        Duration::ZERO,
                                        backoff_max,
                                    );
                                    break Ok(signals.recycle(next));
                                }
                                Acquisition::Interrupted(exit) => break exit,
                            }
                        }
                        None => {}
                    }
                    if !quiet_logged && monitor.gate_quiet_reached() {
                        quiet_logged = true;
                        tracing::info!(
                            "input quiet after the preemption; waiting for new sound before reconnecting"
                        );
                    }
                }
            }
            _ = feedback.tick(), if client.is_some() => {
                if let Some(active) = client.as_mut() {
                    match tokio::time::timeout(FEEDBACK_TIMEOUT, active.send_feedback()).await {
                        Ok(Ok(())) => last_feedback_ok_return = Some(Instant::now()),
                        Ok(Err(error)) => failure = Some(error.into()),
                        Err(_) => failure = Some(anyhow!("AirPlay feedback timed out")),
                    }
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
            let error_kind = classify_error(&error);
            let now = Instant::now();
            let since_ms = |earlier: Option<Instant>| {
                earlier.map(|earlier| now.duration_since(earlier).as_millis() as u64)
            };
            let since_stream_start_ms = since_ms(stream_started);
            let stats = client.as_ref().map(AirPlayClient::stats_snapshot);
            tracing::warn!(
                error = format!("{error:#}"),
                error_kind,
                since_stream_start_ms,
                since_last_feedback_ok_return_ms = since_ms(last_feedback_ok_return),
                packets_sent = stats.as_ref().map(|stats| stats.packets_sent),
                underruns = stats.as_ref().map(|stats| stats.underruns),
                rtx_requested = stats.as_ref().map(|stats| stats.rtx_requested),
                rtx_fulfilled = stats.as_ref().map(|stats| stats.rtx_fulfilled),
                "AirPlay session failed"
            );
            if let Some(active) = client.take() {
                release_output(active, &sink).await;
            }
            let streamed = stream_started.map_or(Duration::ZERO, |at| now.duration_since(at));
            if is_preemption_kind(error_kind) {
                // The receiver closed the session: someone else most likely
                // took the speaker. This is a conservative heuristic: EOF or a
                // reset can also come from a HomePod reboot or a network
                // failure. A takeover that is only seen as a feedback timeout
                // goes through the normal reconnect path below (known gap).
                // Not an error, and `auto_reconnect` does not apply.
                let consecutive =
                    count_after(ExitKind::Preempted, preemptions, streamed, backoff_max);
                let backoff = backoff_delay(backoff_base, backoff_max, consecutive);
                tracing::warn!(
                    error = format!("{error:#}"),
                    error_kind,
                    consecutive_preemptions = consecutive,
                    backoff_seconds = backoff.as_secs(),
                    since_stream_start_ms,
                    "AirPlay output taken by another source; waiting for silence then new sound before reconnecting"
                );
                break Ok(signals.recycle(consecutive));
            }
            if !config.output.auto_reconnect {
                break Err(error);
            }
            tracing::warn!(
                error = format!("{error:#}"),
                error_kind,
                retry_seconds = reconnect_delay.as_secs(),
                "AirPlay output lost; reconnecting once sound is present"
            );
            retry_delay = Some(reconnect_delay);
            // Keep the gate unless the session was stable for long enough.
            let next = count_after(ExitKind::Lost, preemptions, streamed, backoff_max);
            break Ok(signals.recycle(next));
        }
    };

    // A clean TEARDOWN frees the speaker immediately for other sources.
    if let Some(active) = client.take() {
        release_output(active, &sink).await;
    }
    drop(capture);

    if let Ok(SessionExit::Recycle(..)) = result {
        // Signals and config changes that arrived during teardown must win
        // over the restart; `exec` would drop them.
        if signals.requested() {
            return Ok(SessionExit::Shutdown);
        }
        let delay = retry_delay.unwrap_or(Duration::ZERO);
        tokio::select! {
            biased;
            _ = signals.recv() => return Ok(SessionExit::Shutdown),
            next = &mut changed => return next.map(|_| SessionExit::ConfigurationChanged),
            _ = tokio::time::sleep(delay) => {}
        }
    }
    result
}

/// Replaces this process with a fresh copy of itself (same arguments), so the
/// next AirPlay session starts without the previous session's leftovers.
/// Returns `Ok` without restarting if a shutdown was requested; otherwise
/// only returns on failure, and the caller exits so systemd restarts us.
#[cfg(target_os = "linux")]
fn recycle_process(shutdown: &ShutdownFlag, preemptions: u32) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let program = std::env::current_exe().unwrap_or_else(|_| "/proc/self/exe".into());
    let mut command = std::process::Command::new(program);
    command
        .args(std::env::args_os().skip(1))
        .env(RECYCLED_ENV, "1");
    // Explicit either way: a stale count must not outlive the preemption.
    match encode_preemptions(preemptions) {
        Some(count) => command.env(PREEMPTIONS_ENV, count),
        None => command.env_remove(PREEMPTIONS_ENV),
    };
    // Last check, as close to exec as possible.
    if shutdown.is_requested() {
        tracing::info!("shutdown requested");
        return Ok(());
    }
    let error = command.exec();
    Err(anyhow::Error::new(error).context("cannot restart auto-airplay for the next session"))
}

/// SIGINT (ctrl-c) and SIGTERM (systemd stop/restart). The listeners live for
/// the whole session so a signal received while the loop is busy is not lost,
/// and both lead to a clean TEARDOWN that frees the speaker.
#[cfg(target_os = "linux")]
struct ShutdownSignals {
    interrupt: Signal,
    terminate: Signal,
    /// Set synchronously by the signal handler, unlike the Tokio listeners.
    flag: ShutdownFlag,
}

#[cfg(target_os = "linux")]
impl ShutdownSignals {
    fn new() -> Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).context("cannot listen for SIGINT")?,
            terminate: signal(SignalKind::terminate()).context("cannot listen for SIGTERM")?,
            flag: ShutdownFlag::register().context("cannot record shutdown signals")?,
        })
    }

    fn requested(&self) -> bool {
        self.flag.is_requested()
    }

    fn recycle(&self, preemptions: u32) -> SessionExit {
        SessionExit::Recycle(self.flag.clone(), preemptions)
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
    target: &OutputTarget,
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

/// Discovers and connects to the speaker, then starts streaming. On success
/// the connected client is left in `slot`; on failure it is torn down.
///
/// A buffered stream that cannot be set up (PTP ports unavailable, speaker
/// refusing it, no PTP clock) falls back to a realtime stream for this
/// session: music with the old margin beats silence. The next session tries
/// buffered again.
#[cfg(target_os = "linux")]
async fn acquire_output(
    target: &OutputTarget,
    sink: &AudioSink,
    slot: &mut Option<AirPlayClient>,
) -> Result<()> {
    let device = resolve_target(target).await?;
    if target.stream_mode == StreamMode::Buffered {
        match acquire_with(&device, target, StreamMode::Buffered, sink, slot).await {
            Ok(()) => return Ok(()),
            Err(error) => tracing::warn!(
                error = format!("{error:#}"),
                hint = buffered_hint(&error),
                "AirPlay buffered stream unavailable; falling back to realtime for this session"
            ),
        }
    }
    acquire_with(&device, target, StreamMode::Realtime, sink, slot).await
}

/// Connects to `device` and starts a stream of the given mode.
#[cfg(target_os = "linux")]
async fn acquire_with(
    device: &Device,
    target: &OutputTarget,
    mode: StreamMode,
    sink: &AudioSink,
    slot: &mut Option<AirPlayClient>,
) -> Result<()> {
    let client = slot.insert(connect_device(device, target, mode).await?);
    if let Err(error) = start_stream(client, target, mode, sink).await {
        sink.detach();
        if let Some(client) = slot.take() {
            disconnect_quietly(client).await;
        }
        return Err(error);
    }
    Ok(())
}

/// What to do about a buffered stream that could not start, for the log.
#[cfg(target_os = "linux")]
fn buffered_hint(error: &anyhow::Error) -> &'static str {
    let message = format!("{error:#}");
    if message.contains("PTP ports") {
        "UDP ports 319 and 320 are needed: grant CAP_NET_BIND_SERVICE (see the README) and make sure no other PTP daemon runs"
    } else if message.contains("PTP") {
        "the speaker's PTP clock did not answer; check that UDP 319 and 320 are not filtered"
    } else if message.contains("buffered stream") {
        "the speaker refused a buffered stream"
    } else {
        "see the error"
    }
}

#[cfg(target_os = "linux")]
async fn resolve_target(target: &OutputTarget) -> Result<Device> {
    // The library's mDNS scan blocks its thread without yielding, so it runs
    // on the blocking pool: shutdown and config changes can still interrupt
    // the acquisition (the abandoned scan ends on its own).
    let selector = target.selector.clone();
    let seconds = target.discovery_seconds;
    let runtime = tokio::runtime::Handle::current();
    let scan = tokio::task::spawn_blocking(move || {
        runtime.block_on(crate::output::resolve_device(&selector, seconds))
    });
    // Discovery itself clamps to 1..=30 seconds; leave some slack on top.
    let limit = Duration::from_secs(seconds.clamp(1, 30) + 5);
    tokio::time::timeout(limit, scan)
        .await
        .context("AirPlay discovery timed out")?
        .context("AirPlay discovery task failed")?
}

#[cfg(target_os = "linux")]
async fn connect_device(
    device: &Device,
    target: &OutputTarget,
    mode: StreamMode,
) -> Result<AirPlayClient> {
    let mut client = AirPlayClient::new()?;
    client.set_render_delay_ms(target.render_delay_ms);
    if mode == StreamMode::Buffered {
        client.set_buffered(target.buffered_latency);
    }
    tokio::time::timeout(CONNECT_TIMEOUT, client.connect(device))
        .await
        .context("connecting to the AirPlay output timed out")??;
    Ok(client)
}

#[cfg(target_os = "linux")]
async fn start_stream(
    client: &mut AirPlayClient,
    target: &OutputTarget,
    mode: StreamMode,
    sink: &AudioSink,
) -> Result<()> {
    let (sender, decoder) = LiveAudioDecoder::create_pair(target.sample_rate, target.channels, 64);
    match mode {
        StreamMode::Realtime => {
            sink.attach(sender);
            tokio::time::sleep(PREFILL).await;
        }
        // The speaker buffers: no prefill. The output is paced on the
        // speaker's clock, which the buffered stream plays on.
        StreamMode::Buffered => {
            let clock = client
                .media_clock()
                .context("the speaker's clock is not followed")?;
            sink.attach_with_clock(sender, Some(clock));
        }
    }
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
    let started = Instant::now();
    // Milliseconds `stop()` took, `u64::MAX` while it has not finished: tells
    // which step a timed-out teardown was stuck in.
    let stop_ms = AtomicU64::new(u64::MAX);
    let teardown = async {
        let step = Instant::now();
        if let Err(error) = client.stop().await {
            tracing::warn!(
                %error,
                elapsed_ms = step.elapsed().as_millis() as u64,
                "stopping the AirPlay stream failed"
            );
        }
        stop_ms.store(step.elapsed().as_millis() as u64, Ordering::Relaxed);

        let step = Instant::now();
        if let Err(error) = client.disconnect().await {
            tracing::warn!(
                %error,
                elapsed_ms = step.elapsed().as_millis() as u64,
                "disconnecting from the AirPlay output failed"
            );
        }
        tracing::debug!(
            stop_ms = stop_ms.load(Ordering::Relaxed),
            disconnect_ms = step.elapsed().as_millis() as u64,
            "AirPlay teardown finished"
        );
    };
    if tokio::time::timeout(TEARDOWN_TIMEOUT, teardown)
        .await
        .is_err()
    {
        let stop_done = stop_ms.load(Ordering::Relaxed);
        tracing::warn!(
            stuck_in = if stop_done == u64::MAX {
                "stop"
            } else {
                "disconnect"
            },
            stop_ms = (stop_done != u64::MAX).then_some(stop_done),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "AirPlay teardown timed out"
        );
    }
}

/// Levels are logged to a tenth of a dB.
#[cfg(target_os = "linux")]
fn round_db(level: f32) -> f64 {
    (f64::from(level) * 10.0).round() / 10.0
}

/// Coarse class of a session failure, for the logs: tells a receiver that
/// closed the connection (`eof`, `reset-or-broken-pipe`) from our own timeouts.
#[cfg(target_os = "linux")]
fn classify_error(error: &anyhow::Error) -> &'static str {
    use std::io::ErrorKind;

    for cause in error.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            match io.kind() {
                ErrorKind::UnexpectedEof => return "eof",
                ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe => return "reset-or-broken-pipe",
                ErrorKind::TimedOut => return "timeout",
                _ => {}
            }
        }
        match cause.downcast_ref::<airplay_core::error::Error>() {
            Some(airplay_core::error::Error::Timeout) => return "timeout",
            Some(airplay_core::error::Error::Crypto(_)) => return "crypto",
            _ => {}
        }
    }
    if format!("{error:#}").contains("timed out") {
        return "timeout";
    }
    "other"
}

#[cfg(target_os = "linux")]
async fn disconnect_quietly(mut client: AirPlayClient) {
    match tokio::time::timeout(TEARDOWN_TIMEOUT, client.disconnect()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "disconnecting from the AirPlay output failed"),
        Err(_) => tracing::warn!("disconnecting from the AirPlay output timed out"),
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
    if let Err(message) = config.validate_detection() {
        bail!(message);
    }
    if let Err(message) = config.validate_output() {
        bail!(message);
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::io::{Error as IoError, ErrorKind};

    use super::classify_error;
    use crate::preemption::is_preemption_kind;

    fn io(kind: ErrorKind) -> anyhow::Error {
        airplay_core::error::Error::from(IoError::new(kind, "x")).into()
    }

    #[test]
    fn classifies_receiver_closing_the_connection() {
        assert_eq!(classify_error(&io(ErrorKind::UnexpectedEof)), "eof");
        assert_eq!(
            classify_error(&io(ErrorKind::ConnectionReset)),
            "reset-or-broken-pipe"
        );
        assert_eq!(
            classify_error(&io(ErrorKind::BrokenPipe)),
            "reset-or-broken-pipe"
        );
    }

    #[test]
    fn classifies_timeouts_crypto_and_the_rest() {
        assert_eq!(classify_error(&io(ErrorKind::TimedOut)), "timeout");
        assert_eq!(
            classify_error(&airplay_core::error::Error::Timeout.into()),
            "timeout"
        );
        assert_eq!(
            classify_error(&anyhow::anyhow!("AirPlay feedback timed out")),
            "timeout"
        );
        assert_eq!(classify_error(&io(ErrorKind::PermissionDenied)), "other");
        assert_eq!(
            classify_error(&anyhow::anyhow!("AirPlay stream stopped accepting audio")),
            "other"
        );
    }

    #[test]
    fn finds_the_io_error_through_added_context() {
        let error = anyhow::Error::from(IoError::new(ErrorKind::UnexpectedEof, "early eof"))
            .context("feedback");
        assert_eq!(classify_error(&error), "eof");
    }

    fn is_preempted(error: &anyhow::Error) -> bool {
        is_preemption_kind(classify_error(error))
    }

    #[test]
    fn the_receiver_closing_the_session_is_a_preemption() {
        assert!(is_preempted(&io(ErrorKind::UnexpectedEof)));
        assert!(is_preempted(&io(ErrorKind::ConnectionReset)));
        assert!(is_preempted(&io(ErrorKind::ConnectionAborted)));
        assert!(is_preempted(&io(ErrorKind::BrokenPipe)));

        // Early eof on a feedback request, with context added on the way up.
        let feedback = anyhow::Error::from(IoError::new(ErrorKind::UnexpectedEof, "early eof"))
            .context("POST /feedback");
        assert!(is_preempted(&feedback));
    }

    #[test]
    fn other_failures_are_not_preemptions() {
        assert!(!is_preempted(&io(ErrorKind::TimedOut)));
        assert!(!is_preempted(&io(ErrorKind::PermissionDenied)));
        assert!(!is_preempted(&airplay_core::error::Error::Timeout.into()));
        assert!(!is_preempted(&anyhow::anyhow!(
            "AirPlay feedback timed out"
        )));
        assert!(!is_preempted(&anyhow::anyhow!(
            "AirPlay stream stopped accepting audio"
        )));
    }
}
