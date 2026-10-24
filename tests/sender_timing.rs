//! The AirPlay sender's clock, measured: what each sync packet announces, when
//! the packets really leave, and how both relate to the input clock.
//!
//! A sync packet pairs an RTP timestamp with the time it must play (its
//! anchor): the frame in bytes 4-7 plays at the NTP time, so the one in bytes
//! 16-19 plays the latency (their difference) later. The receiver plays the
//! stream at the rate consecutive anchors imply: two anchors one second apart
//! that disagree by 2 ms make it play 0.2 % fast or slow, heard as speed and
//! pitch moving. Bluetooth clock drift is tens of ppm by comparison, and the
//! jitter buffer absorbs it.
//!
//! Each run drives the real sender (`AudioStreamer::start_live`, the path the
//! daemon uses) against a receiver on the loopback interface. The input path
//! is the daemon's: a simulated Bluetooth source, 50 ppm fast, captures ALSA
//! periods into the jitter buffer, whose pacer hands chunks to the sender's
//! live queue.
//!
//! The tests that run by default check what holds for the vendored sender
//! however the machine schedules the test (preempted, throttled, suspended):
//! every change of anchor between two sync packets is either none or a
//! restart of the timeline after a packet too late to play (a step of more
//! than half the margin), never a small step; every sync packet announces the
//! render delay as its latency, and no packet arrives earlier than that before
//! it plays; `stop` returns; decoding never waits for input.
//!
//! `compare_with_upstream` is ignored by default: it times the pinned upstream
//! sender and the vendored one side by side and prints the figures:
//!
//! ```text
//! cargo test --test sender_timing -- --ignored --nocapture
//! ```
//!
//! * anchors against the RTP timeline (`rtp / 44100` s), on the monotonic
//!   clock: any change between two syncs is a rate change the receiver
//!   follows;
//! * packet sends against the RTP timeline: lasting shifts, and lateness;
//! * the margin between a packet's arrival and its anchor (the latency the
//!   receiver is given), and the latency the sync packets announce (upstream:
//!   none).
//!
//! Environment: `AUTO_AIRPLAY_TIMING_SECONDS` sets the length of each timed
//! run (default 8), `AUTO_AIRPLAY_TIMING_CSV` a directory for the raw
//! timelines, `AUTO_AIRPLAY_TIMING_LOG` a log filter (`airplay_audio=warn`).

use std::collections::HashMap;
use std::fmt;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use airplay_audio::timeline::{Timeline, announced_latency};
use airplay_client::{AudioFormat, ClockOffset, StreamConfig};
use auto_airplay::plc::{JitterBuffer, PLAYOUT_CHUNK_FRAMES};

/// The RTP clock of the stream (ALAC, 44.1 kHz, 352 frames per packet).
const RTP_RATE: f64 = 44_100.0;
/// The daemon's settings, unchanged.
const RENDER_DELAY_MS: u32 = 250;
const JITTER_BUFFER_MS: u32 = 400;
const LIVE_QUEUE: usize = 64;
const PREFILL: Duration = Duration::from_millis(500);
const CHANNELS: usize = 2;
/// The ALSA period the daemon asks for.
const PERIOD: usize = 1024;
/// A plausible crystal error for the Bluetooth source.
const INPUT_DRIFT_PPM: f64 = 50.0;
/// Startup is excluded from the steady-state figures of the comparison.
const WARM_UP_NS: i128 = 2_000_000_000;
const NTP_UNIX_OFFSET_S: i128 = 2_208_988_800;
/// How long `stop` may take before it counts as hung.
const STOP_LIMIT: Duration = Duration::from_secs(5);
/// Anchor changes up to this are clock-reading noise (two clocks read on
/// each side, with the wall clock possibly slewed meanwhile), not a step.
const NO_STEP_MS: f64 = 0.5;

fn timed_run() -> Duration {
    let seconds = std::env::var("AUTO_AIRPLAY_TIMING_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(8);
    Duration::from_secs(seconds.max(5))
}

/// The vendored sender's timeline with the daemon's settings.
fn timeline() -> Timeline {
    let config = StreamConfig::default();
    let latency = announced_latency(
        Duration::from_millis(u64::from(RENDER_DELAY_MS)),
        RTP_RATE as u32,
        config.latency_min,
        config.latency_max,
    );
    Timeline::new(RTP_RATE as u32, latency)
}

fn logs() {
    if let Ok(filter) = std::env::var("AUTO_AIRPLAY_TIMING_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(std::io::stderr)
            .try_init();
    }
}

// ----- the receiver -----

#[derive(Debug, Clone, Copy)]
struct Arrival {
    /// Monotonic arrival time, ns since the receiver started.
    mono: i128,
    /// Wall-clock arrival time, Unix ns.
    wall: i128,
    rtp: u32,
}

#[derive(Debug, Clone, Copy)]
struct SyncPacket {
    arrival: Arrival,
    /// When the sync says `arrival.rtp` plays, Unix ns.
    anchor: i128,
    /// The latency it announces, in frames (NTP sync only).
    latency: u32,
    payload_type: u8,
}

#[derive(Default)]
struct Log {
    audio: Vec<Arrival>,
    syncs: Vec<SyncPacket>,
}

/// Records every packet on a data and a control port, as a receiver sees them.
struct Receiver {
    data: SocketAddr,
    control: SocketAddr,
    base: Instant,
    log: Arc<Mutex<Log>>,
    running: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Receiver {
    fn start() -> Self {
        let log = Arc::new(Mutex::new(Log::default()));
        let running = Arc::new(AtomicBool::new(true));
        let base = Instant::now();
        let mut addresses = Vec::new();
        let mut threads = Vec::new();
        for _ in 0..2 {
            let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
            socket
                .set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            addresses.push(socket.local_addr().unwrap());
            let (log, running) = (Arc::clone(&log), Arc::clone(&running));
            threads.push(thread::spawn(move || {
                listen(&socket, base, &log, &running);
            }));
        }
        Self {
            data: addresses[0],
            control: addresses[1],
            base,
            log,
            running,
            threads,
        }
    }

    fn finish(self) -> Log {
        self.running.store(false, Ordering::Release);
        for thread in self.threads {
            thread.join().unwrap();
        }
        std::mem::take(&mut *self.log.lock().unwrap())
    }
}

fn unix_now() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i128
}

/// Now on both clocks: monotonic ns since `base`, and wall-clock Unix ns read
/// between two monotonic reads (retried if the thread was preempted in
/// between, which would make the two disagree).
fn now(base: Instant) -> (i128, i128) {
    let mut best = (i128::MAX, 0, 0);
    for _ in 0..4 {
        let before = base.elapsed().as_nanos() as i128;
        let wall = unix_now();
        let after = base.elapsed().as_nanos() as i128;
        if after - before < best.0 {
            best = (after - before, before + (after - before) / 2, wall);
        }
        if after - before < 20_000 {
            break;
        }
    }
    (best.1, best.2)
}

/// Seconds and a 32-bit binary fraction to ns.
fn fixed_point_ns(seconds: i128, fraction: u32) -> i128 {
    seconds * 1_000_000_000 + ((i128::from(fraction) * 1_000_000_000) >> 32)
}

fn listen(socket: &UdpSocket, base: Instant, log: &Mutex<Log>, running: &AtomicBool) {
    let mut buffer = [0_u8; 2048];
    while running.load(Ordering::Acquire) {
        let Ok((len, _)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        let (mono, wall) = now(base);
        let packet = &buffer[..len];
        if len < 12 {
            continue;
        }
        let word = |at: usize| u32::from_be_bytes(packet[at..at + 4].try_into().unwrap());
        let mut log = log.lock().unwrap();
        match packet[1] & 0x7f {
            96 => log.audio.push(Arrival {
                mono,
                wall,
                rtp: word(4),
            }),
            // NTP sync: [4..8] RTP timestamp minus the latency, which plays
            // at [8..16] the NTP time; [16..20] RTP timestamp, which plays
            // the latency later.
            84 if len >= 20 => {
                let latency = word(16).wrapping_sub(word(4));
                log.syncs.push(SyncPacket {
                    arrival: Arrival {
                        mono,
                        wall,
                        rtp: word(16),
                    },
                    anchor: fixed_point_ns(i128::from(word(8)) - NTP_UNIX_OFFSET_S, word(12))
                        + (f64::from(latency) * 1e9 / RTP_RATE).round() as i128,
                    latency,
                    payload_type: 84,
                });
            }
            // PTP sync: [4..8] RTP timestamp, [8..16] time.
            87 if len >= 28 => log.syncs.push(SyncPacket {
                arrival: Arrival {
                    mono,
                    wall,
                    rtp: word(4),
                },
                anchor: fixed_point_ns(i128::from(word(8)), word(12)),
                latency: 0,
                payload_type: 87,
            }),
            _ => {}
        }
    }
}

// ----- the input -----

/// Packets the sender itself counted as sent late; upstream does not count.
trait LateCount {
    fn late_count(&self) -> Option<u64>;
}

impl LateCount for airplay_audio_upstream::AudioStreamer {
    fn late_count(&self) -> Option<u64> {
        None
    }
}

impl LateCount for airplay_audio::AudioStreamer {
    fn late_count(&self) -> Option<u64> {
        Some(self.late_packets())
    }
}

/// A sender's live queue, of either crate.
trait Sink: Send + 'static {
    fn is_full(&self) -> bool;
    fn send(&self, samples: Vec<i16>, sample_rate: u32) -> bool;
}

/// The daemon's input side: a capture thread on the input clock fills the
/// jitter buffer; once a stream is attached, a playout thread hands it paced
/// chunks, as `run_playout` and `Stream::feed` do.
struct Input {
    rate: u32,
    buffer: Arc<Mutex<JitterBuffer>>,
    epoch: Instant,
    running: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

fn tone(first_frame: usize, rate: u32) -> Vec<i16> {
    (0..PERIOD * CHANNELS)
        .map(|n| {
            let t = (first_frame + n / CHANNELS) as f64 / f64::from(rate);
            let a = (t * std::f64::consts::TAU * 440.0).sin() * 6_000.0;
            let b = (t * std::f64::consts::TAU * 1_234.5).sin() * 3_000.0;
            (a + b) as i16
        })
        .collect()
}

impl Input {
    fn start(rate: u32) -> Self {
        let buffer = Arc::new(Mutex::new(JitterBuffer::new(
            rate,
            CHANNELS as u8,
            JITTER_BUFFER_MS,
        )));
        let epoch = Instant::now();
        let running = Arc::new(AtomicBool::new(true));
        let capture = {
            let (buffer, running) = (Arc::clone(&buffer), Arc::clone(&running));
            let period = PERIOD as f64 / (f64::from(rate) * (1.0 + INPUT_DRIFT_PPM * 1e-6));
            thread::spawn(move || {
                let mut index = 0_u64;
                while running.load(Ordering::Acquire) {
                    let due = epoch + Duration::from_secs_f64(index as f64 * period);
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        thread::sleep(wait);
                    }
                    let samples = tone(index as usize * PERIOD, rate);
                    buffer.lock().unwrap().push_at(epoch.elapsed(), &samples, 0);
                    index += 1;
                }
            })
        };
        Self {
            rate,
            buffer,
            epoch,
            running,
            threads: vec![capture],
        }
    }

    fn attach(&mut self, sink: impl Sink) {
        let (buffer, running, epoch, rate) = (
            Arc::clone(&self.buffer),
            Arc::clone(&self.running),
            self.epoch,
            self.rate,
        );
        self.threads.push(thread::spawn(move || {
            let mut pacer = {
                let mut buffer = buffer.lock().unwrap();
                buffer.start_playout();
                buffer.pacer()
            };
            let mut chunk = vec![0_i16; PLAYOUT_CHUNK_FRAMES * CHANNELS];
            while running.load(Ordering::Acquire) {
                let now = epoch.elapsed();
                let sent = if sink.is_full() {
                    pacer.blocked(now);
                    false
                } else if buffer
                    .lock()
                    .unwrap()
                    .pull_chunk(&mut pacer, now, &mut chunk)
                {
                    let accepted = sink.send(chunk.clone(), rate);
                    if accepted {
                        pacer.sent(PLAYOUT_CHUNK_FRAMES);
                    }
                    accepted
                } else {
                    false
                };
                if !sent {
                    thread::sleep(Duration::from_millis(3));
                }
            }
        }));
    }

    fn stop(self) {
        self.running.store(false, Ordering::Release);
        for thread in self.threads {
            thread.join().unwrap();
        }
    }
}

// ----- the sender under test -----

#[derive(Debug, Clone, Copy)]
struct Stalls {
    /// Microseconds between two stalls (uniform).
    gap_us: (u64, u64),
    /// Microseconds each stall blocks the sending task's thread (uniform).
    block_us: (u64, u64),
    seed: u64,
}

#[derive(Debug, Clone, Copy)]
struct Scenario {
    name: &'static str,
    input_rate: u32,
    /// Blocks the thread the sending task runs on now and then, as a slow
    /// decode does.
    stalls: Option<Stalls>,
}

fn between(state: &mut u64, (low, high): (u64, u64)) -> u64 {
    // xorshift64*
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    let value = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
    low + value % (high - low + 1)
}

async fn stall(stalls: Stalls) {
    let mut state = stalls.seed.max(1);
    loop {
        let gap = between(&mut state, stalls.gap_us);
        tokio::time::sleep(Duration::from_micros(gap)).await;
        let block = between(&mut state, stalls.block_us);
        // Blocking on purpose: the sending task cannot run meanwhile.
        thread::sleep(Duration::from_micros(block));
    }
}

/// When a run ends: after `min`, once `syncs` sync packets arrived, and in any
/// case after `max`.
#[derive(Debug, Clone, Copy)]
struct Until {
    min: Duration,
    syncs: usize,
    max: Duration,
}

impl Until {
    /// Long enough for the invariants, however slow the machine.
    const CHECK: Self = Self {
        min: Duration::from_secs(4),
        syncs: 5,
        max: Duration::from_secs(30),
    };

    fn timed(duration: Duration) -> Self {
        Self {
            min: duration,
            syncs: 0,
            max: duration,
        }
    }
}

/// One run: what the receiver saw, and how long `AudioStreamer::stop` took
/// (`None`: not done after [`STOP_LIMIT`]).
struct Run {
    log: Log,
    stopped: Option<Duration>,
    /// When `stop` was called, on the receiver's monotonic clock.
    stop_at: i128,
    /// Packets the sender counted as sent more than a packet late.
    late_count: Option<u64>,
}

macro_rules! sender_under_test {
    ($run:ident, $empty_decodes:ident, $krate:ident) => {
        impl Sink for $krate::LiveFrameSender {
            fn is_full(&self) -> bool {
                $krate::LiveFrameSender::is_full(self)
            }

            fn send(&self, samples: Vec<i16>, sample_rate: u32) -> bool {
                self.try_send($krate::LivePcmFrame {
                    samples,
                    channels: CHANNELS as u8,
                    sample_rate,
                })
            }
        }

        fn $run(scenario: &Scenario, until: Until) -> Run {
            let receiver = Receiver::start();
            let mut input = Input::start(scenario.input_rate);
            // The daemon captures all along: the cushion is full when a
            // stream is attached.
            thread::sleep(Duration::from_millis(u64::from(JITTER_BUFFER_MS) + 600));
            let (sender, decoder) = $krate::LiveAudioDecoder::create_pair(
                scenario.input_rate,
                CHANNELS as u8,
                LIVE_QUEUE,
            );
            input.attach(sender);
            thread::sleep(PREFILL);

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (stopped, stop_at, late_count) = runtime.block_on(async {
                let mut rtp = $krate::RtpSender::new(receiver.data, 0x0a1b_2c3d);
                rtp.set_control_dest(receiver.control);
                rtp.bind(0).unwrap();
                rtp.set_control_socket(UdpSocket::bind("0.0.0.0:0").unwrap());
                let mut streamer = $krate::AudioStreamer::new(StreamConfig::default());
                streamer.set_rtp_sender(rtp).await;
                streamer.set_render_delay_ms(RENDER_DELAY_MS).await;
                // NTP timing, as `Connection::setup` leaves it: the sender is
                // the reference clock, offset zero.
                streamer.set_timing_offset(ClockOffset::default()).await;
                if let Some(stalls) = scenario.stalls {
                    tokio::spawn(stall(stalls));
                }
                streamer.start_live(decoder).await.unwrap();
                let streaming = Instant::now();
                loop {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let elapsed = streaming.elapsed();
                    let syncs = receiver.log.lock().unwrap().syncs.len();
                    if elapsed >= until.max || (elapsed >= until.min && syncs >= until.syncs) {
                        break;
                    }
                }
                let late_count = streamer.late_count();
                let stopping = Instant::now();
                let stopped = tokio::time::timeout(STOP_LIMIT, streamer.stop())
                    .await
                    .ok()
                    .map(|stopped| {
                        stopped.unwrap();
                        stopping.elapsed()
                    });
                (
                    stopped,
                    stopping.duration_since(receiver.base).as_nanos() as i128,
                    late_count,
                )
            });
            // A sender that did not stop goes with the runtime.
            runtime.shutdown_timeout(STOP_LIMIT);
            input.stop();
            Run {
                log: receiver.finish(),
                stopped,
                stop_at,
                late_count,
            }
        }

        /// How long each of `calls` decodes takes on a live queue with
        /// nothing in it (at the stream rate: no resampler to build).
        fn $empty_decodes(calls: u32) -> Vec<Duration> {
            let (_sender, mut decoder) =
                $krate::LiveAudioDecoder::create_pair(44_100, CHANNELS as u8, LIVE_QUEUE);
            let format = AudioFormat::default();
            (0..calls)
                .map(|_| {
                    let started = Instant::now();
                    assert!(decoder.decode_resampled(&format, 352).unwrap().is_none());
                    started.elapsed()
                })
                .collect()
        }
    };
}

sender_under_test!(run_upstream, empty_decodes_upstream, airplay_audio_upstream);
sender_under_test!(run_vendored, empty_decodes_vendored, airplay_audio);

const STEADY: Scenario = Scenario {
    name: "48 kHz input",
    input_rate: 48_000,
    stalls: None,
};

/// The sending task blocked for 1 to 11 ms (the decodes the Pi logs) every
/// 0 to 20 ms.
const STALLS: Scenario = Scenario {
    name: "stalls 1-11 ms",
    input_rate: 48_000,
    stalls: Some(Stalls {
        gap_us: (0, 20_000),
        block_us: (1_000, 11_000),
        seed: 0x5eed,
    }),
};

// ----- the figures -----

fn ms(ns: f64) -> f64 {
    ns / 1e6
}

fn range(values: impl IntoIterator<Item = f64>) -> (f64, f64) {
    values
        .into_iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), value| {
            (low.min(value), high.max(value))
        })
}

fn median(mut values: Vec<f64>) -> f64 {
    assert!(!values.is_empty());
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// A run's sync packets, sorted, with what they announce against the RTP
/// timeline.
struct Anchors<'a> {
    log: &'a Log,
    /// The first audio packet: the origin of the RTP timeline.
    first: Arrival,
    syncs: Vec<&'a SyncPacket>,
}

/// Puts a run's packets in arrival order.
fn sort(log: &mut Log) {
    log.audio.sort_by_key(|packet| packet.mono);
    log.syncs.sort_by_key(|sync| sync.arrival.mono);
}

impl<'a> Anchors<'a> {
    /// `log` must be [`sort`]ed.
    fn new(log: &'a Log) -> Self {
        let first = *log.audio.first().expect("no audio arrived");
        let syncs = log.syncs.iter().collect();
        Self { log, first, syncs }
    }

    /// RTP time of `rtp` since the first audio packet, ns.
    fn rtp_ns(&self, rtp: u32) -> f64 {
        f64::from(rtp.wrapping_sub(self.first.rtp)) * 1e9 / RTP_RATE
    }

    /// Anchor minus RTP time, on the monotonic clock (so that the wall clock
    /// being slewed meanwhile does not count), ns.
    fn residual(&self, sync: &SyncPacket) -> f64 {
        (sync.anchor - (sync.arrival.wall - sync.arrival.mono)) as f64
            - self.rtp_ns(sync.arrival.rtp)
    }

    /// Change of anchor between consecutive syncs, ms, with the RTP time
    /// between them, ns.
    fn steps(&self, syncs: &[&SyncPacket]) -> Vec<(f64, f64)> {
        syncs
            .windows(2)
            .map(|pair| {
                (
                    ms(self.residual(pair[1]) - self.residual(pair[0])),
                    self.rtp_ns(pair[1].arrival.rtp) - self.rtp_ns(pair[0].arrival.rtp),
                )
            })
            .collect()
    }

    /// Anchor minus arrival of the audio packet each sync names, ms: the
    /// receiver's margin.
    fn margins(&self, syncs: &[&SyncPacket]) -> Vec<f64> {
        let arrivals: HashMap<u32, i128> = self
            .log
            .audio
            .iter()
            .map(|packet| (packet.rtp, packet.wall))
            .collect();
        syncs
            .iter()
            .filter_map(|sync| {
                arrivals
                    .get(&sync.arrival.rtp)
                    .map(|wall| ms((sync.anchor - wall) as f64))
            })
            .collect()
    }

    fn span(&self, arrivals: impl Iterator<Item = i128>) -> (usize, Option<f64>, Option<f64>) {
        let times: Vec<f64> = arrivals
            .map(|at| ms((at - self.first.mono) as f64))
            .collect();
        (times.len(), times.first().copied(), times.last().copied())
    }

    /// Counts, first and last arrival (ms), for failure messages.
    fn summary(&self) -> String {
        format!(
            "syncs {:?}, audio {:?}",
            self.span(self.log.syncs.iter().map(|sync| sync.arrival.mono)),
            self.span(self.log.audio.iter().map(|packet| packet.mono)),
        )
    }
}

/// What holds for the vendored sender however the test was scheduled.
fn assert_on_the_timeline(mut run: Run) {
    let timeline = timeline();
    let restart_ms = ms(timeline.max_late_ns() as f64);
    let margin_ms = ms(timeline.margin() as f64);
    sort(&mut run.log);
    let anchors = Anchors::new(&run.log);
    let syncs = anchors.syncs.clone();
    assert!(
        syncs.len() >= 3,
        "too few sync packets: {}",
        anchors.summary()
    );

    // Every sync sits on the timeline the previous one set, or restarts it
    // after a packet too late to play: never a small step, which is a rate
    // change the receiver would follow.
    let steps = anchors.steps(&syncs);
    let restarts = steps.iter().filter(|(step, _)| *step > restart_ms).count();
    for (step, _) in &steps {
        assert!(
            step.abs() < NO_STEP_MS || *step > restart_ms,
            "anchor step of {step:.3} ms (steps {steps:?}; {})",
            anchors.summary()
        );
    }
    println!(
        "{} syncs, {} anchor steps under {NO_STEP_MS} ms, {restarts} restarts",
        syncs.len(),
        steps.len() - restarts
    );

    // Live streaming never uses PTP sync packets, and every sync announces
    // the render delay as its latency: bytes 4-7 are the RTP timestamp of
    // bytes 16-19 minus 11025 frames (250 ms, SETUP's latencyMin).
    assert!(syncs.iter().all(|sync| sync.payload_type == 84));
    for sync in &syncs {
        assert_eq!(
            sync.latency,
            timeline.latency_frames(),
            "announced latency ({})",
            anchors.summary()
        );
    }
    // No packet arrives earlier than the announced latency before it plays:
    // none leaves before its deadline.
    for margin in anchors.margins(&syncs) {
        assert!(
            margin <= margin_ms + 0.1,
            "margin {margin:.3} ms over {margin_ms:.3} ms"
        );
    }
    assert!(run.stopped.is_some(), "stop() did not return");
}

#[test]
fn anchors_stay_on_the_rtp_timeline() {
    logs();
    assert_on_the_timeline(run_vendored(&STEADY, Until::CHECK));
}

#[test]
fn stalls_of_the_sending_task_do_not_move_the_anchors() {
    logs();
    assert_on_the_timeline(run_vendored(&STALLS, Until::CHECK));
}

#[test]
fn the_live_decoder_does_not_wait_for_input() {
    // A preempted or suspended call only takes longer: the fastest one shows
    // whether decoding waits.
    let fastest = *empty_decodes_vendored(50).iter().min().unwrap();
    assert!(fastest < Duration::from_millis(1), "{fastest:?}");
}

// ----- the comparison with upstream -----

#[derive(Debug)]
struct Report {
    sender: &'static str,
    scenario: &'static str,
    /// Sync packets in the steady state.
    syncs: usize,
    /// Anchor steps larger than a restart of the vendored timeline.
    restarts: usize,
    /// How far the first regular sync moved the timeline the first sync set.
    startup_ms: f64,
    /// Steady state, restarts aside: the largest change of anchor between
    /// consecutive syncs, and the playback rate change it implies.
    anchor_step_ms: f64,
    rate_swing_ppm: f64,
    /// Sends against the RTP timeline, steady state: their rate against real
    /// time (from the lasting shift: median of the last second against median
    /// of the first), the worst lateness against the median, and the packets
    /// the sender itself counted as late, if it counts.
    send_clock_ppm: f64,
    send_late_ms: f64,
    late_count: Option<u64>,
    /// The receiver's margin, i.e. the latency it is given.
    margin_ms: (f64, f64),
    /// The latency the sync packets announce (bytes 16-19 minus bytes 4-7),
    /// frames: lowest and highest.
    announced: (u32, u32),
    stop_ms: Option<f64>,
}

impl Report {
    fn new(sender: &'static str, scenario: &'static str, mut run: Run) -> Self {
        sort(&mut run.log);
        dump(sender, scenario, &run.log);
        let restart_ms = ms(timeline().max_late_ns() as f64);
        let anchors = Anchors::new(&run.log);
        let first = anchors.first;
        let steady = |arrival: &Arrival| {
            arrival.mono - first.mono >= WARM_UP_NS && arrival.mono < run.stop_at
        };

        let regular = anchors
            .syncs
            .iter()
            .find(|sync| anchors.rtp_ns(sync.arrival.rtp) >= 1e9)
            .expect("no regular sync");
        let startup_ms = ms(anchors.residual(regular) - anchors.residual(anchors.syncs[0]));
        let syncs: Vec<&SyncPacket> = anchors
            .syncs
            .iter()
            .copied()
            .filter(|sync| steady(&sync.arrival))
            .collect();
        assert!(
            syncs.len() >= 3,
            "{sender}: too few sync packets in the steady state ({})",
            anchors.summary()
        );
        let steps = anchors.steps(&syncs);
        let restarts = steps.iter().filter(|(step, _)| *step > restart_ms).count();
        let (anchor_step_ms, rate_swing_ppm) = steps
            .iter()
            .filter(|(step, _)| *step <= restart_ms)
            .fold((0.0_f64, 0.0_f64), |(step_ms, swing), (step, span)| {
                (
                    step_ms.max(step.abs()),
                    swing.max(step.abs() * 1e6 / span * 1e6),
                )
            });

        // Sends against the RTP timeline.
        let sends: Vec<(i128, f64)> = run
            .log
            .audio
            .iter()
            .filter(|packet| steady(packet))
            .map(|packet| {
                let mono = packet.mono - first.mono;
                (mono, mono as f64 - anchors.rtp_ns(packet.rtp))
            })
            .collect();
        let (start, end) = (sends[0].0, sends[sends.len() - 1].0);
        let part = |from: i128, to: i128| {
            median(
                sends
                    .iter()
                    .filter(|(mono, _)| (from..to).contains(mono))
                    .map(|(_, send)| *send)
                    .collect(),
            )
        };
        let shift = part(end - 1_000_000_000, end + 1) - part(start, start + 1_000_000_000);
        let send_clock_ppm = -shift / (end - start - 1_000_000_000) as f64 * 1e6;
        let typical = median(sends.iter().map(|(_, send)| *send).collect());
        let send_late_ms = ms(range(sends.iter().map(|(_, send)| *send)).1 - typical);

        Self {
            sender,
            scenario,
            syncs: syncs.len(),
            restarts,
            startup_ms,
            anchor_step_ms,
            rate_swing_ppm,
            send_clock_ppm,
            send_late_ms,
            late_count: run.late_count,
            margin_ms: range(anchors.margins(&syncs)),
            announced: syncs.iter().fold((u32::MAX, 0), |(low, high), sync| {
                (low.min(sync.latency), high.max(sync.latency))
            }),
            stop_ms: run.stopped.map(|elapsed| elapsed.as_secs_f64() * 1e3),
        }
    }

    fn header() -> String {
        format!(
            "Input: simulated Bluetooth clock {INPUT_DRIFT_PPM:+.0} ppm against real time. Clocks are against real time (monotonic).\n\
             sender    scenario        syncs restarts | startup  | anchors vs RTP: max step  rate swing | sends: clock   late  (counted) | margin (latency) | announced | stop\n\
             ---------------------------------------+----------+-------------------------------+--------------------------------+------------------+-----------+--------"
        )
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:<9} {:<15} {:>5} {:>8} | {:>+5.1} ms | {:>14.3} ms {:>6.0} ppm | {:>+6.0} ppm {:>6.2} ms {:>7} | {:>6.1}..{:<5.1} ms | {:>9} | {}",
            self.sender,
            self.scenario,
            self.syncs,
            self.restarts,
            self.startup_ms,
            self.anchor_step_ms,
            self.rate_swing_ppm,
            self.send_clock_ppm,
            self.send_late_ms,
            self.late_count
                .map_or_else(|| "-".to_owned(), |count| format!("({count})")),
            self.margin_ms.0,
            self.margin_ms.1,
            if self.announced.0 == self.announced.1 {
                format!("{} fr", self.announced.0)
            } else {
                format!("{}..{}", self.announced.0, self.announced.1)
            },
            self.stop_ms
                .map_or_else(|| "hung".to_owned(), |ms| format!("{ms:.0} ms")),
        )
    }
}

/// Writes the raw timelines to `AUTO_AIRPLAY_TIMING_CSV`, if set.
fn dump(sender: &str, scenario: &str, log: &Log) {
    let Ok(directory) = std::env::var("AUTO_AIRPLAY_TIMING_CSV") else {
        return;
    };
    let anchors = Anchors::new(log);
    let rtp_ms = |rtp: u32| anchors.rtp_ns(rtp) / 1e6;
    let mut csv = String::from("kind,arrival_ms,rtp_ms,anchor_minus_rtp_ms\n");
    for packet in &anchors.log.audio {
        let arrival = ms((packet.mono - anchors.first.mono) as f64);
        csv += &format!("audio,{arrival:.3},{:.3},\n", rtp_ms(packet.rtp));
    }
    for sync in &anchors.syncs {
        let arrival = ms((sync.arrival.mono - anchors.first.mono) as f64);
        csv += &format!(
            "sync,{arrival:.3},{:.3},{:.3}\n",
            rtp_ms(sync.arrival.rtp),
            ms(anchors.residual(sync))
        );
    }
    let name: String = format!("{sender}-{scenario}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    std::fs::write(format!("{directory}/{name}.csv"), csv).unwrap();
}

/// Runs `scenario` with both senders at once and prints the figures.
fn compare(scenario: &Scenario) -> (Report, Report) {
    let until = Until::timed(timed_run());
    let (upstream, vendored) = thread::scope(|scope| {
        let upstream = scope.spawn(|| run_upstream(scenario, until));
        let vendored = scope.spawn(|| run_vendored(scenario, until));
        (upstream.join().unwrap(), vendored.join().unwrap())
    });
    let upstream = Report::new("upstream", scenario.name, upstream);
    let vendored = Report::new("vendored", scenario.name, vendored);
    println!("{}\n{upstream}\n{vendored}\n", Report::header());
    (upstream, vendored)
}

/// The timed comparison: needs a machine that keeps real time (no throttling,
/// no suspension), so it only runs on request.
#[test]
#[ignore = "timed comparison with upstream; run with --ignored --nocapture"]
fn compare_with_upstream() {
    logs();
    let fastest = |durations: Vec<Duration>| durations.into_iter().min().unwrap();
    let upstream = fastest(empty_decodes_upstream(50));
    let vendored = fastest(empty_decodes_vendored(50));
    println!(
        "Fastest of 50 decodes on an empty live queue: upstream {upstream:?}, vendored {vendored:?}\n"
    );
    // Upstream waits 2 ms for input on every call, in the send loop.
    assert!(upstream >= Duration::from_millis(2), "{upstream:?}");
    assert!(vendored < Duration::from_millis(1), "{vendored:?}");

    let restart_ms = ms(timeline().max_late_ns() as f64);
    let margin_ms = ms(timeline().margin() as f64);
    for scenario in [STEADY, STALLS] {
        let (upstream, fixed) = compare(&scenario);
        // Upstream stamps its first sync before its queue fills, every later
        // one with the queue full: the timeline jumps back by the queue.
        assert!(upstream.startup_ms < -20.0, "{upstream}");
        // The vendored anchors stay on the RTP timeline.
        assert!(fixed.anchor_step_ms < NO_STEP_MS, "{fixed}");
        assert!(
            fixed.startup_ms.abs() < NO_STEP_MS || fixed.startup_ms > restart_ms,
            "{fixed}"
        );
        // Upstream announces no latency (bytes 4-7 equal bytes 16-19); the
        // vendored sender announces the render delay, and its packets play
        // that long after their deadline, never earlier.
        assert_eq!(upstream.announced, (0, 0), "{upstream}");
        let latency = timeline().latency_frames();
        assert_eq!(fixed.announced, (latency, latency), "{fixed}");
        assert!(fixed.margin_ms.1 <= margin_ms + 0.1, "{fixed}");
        assert!(fixed.stop_ms.is_some(), "{fixed}");
        if fixed.restarts == 0 {
            assert!(fixed.send_clock_ppm.abs() < 500.0, "{fixed}");
        }
        if scenario.stalls.is_some() {
            // Upstream announces each sync at the time it was prepared: a
            // stall before that moves the anchor by its length.
            assert!(upstream.anchor_step_ms > 1.0, "{upstream}");
        }
    }
}
