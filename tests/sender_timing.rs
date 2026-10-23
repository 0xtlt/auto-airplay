//! The AirPlay sender's clock, measured: what each sync packet announces, when
//! the packets really leave, and how both relate to the input clock.
//!
//! A sync packet pairs an RTP timestamp with the time it must play (its
//! anchor). The receiver plays the stream at the rate consecutive anchors
//! imply: two anchors one second apart that disagree by 2 ms make it play
//! 0.2 % fast or slow, heard as speed and pitch moving. Bluetooth clock drift
//! is tens of ppm by comparison, and the jitter buffer absorbs it.
//!
//! Each run drives the real sender (`AudioStreamer::start_live`, the path the
//! daemon uses) against a receiver on the loopback interface. The input path
//! is the daemon's: a simulated Bluetooth source, 50 ppm fast, captures ALSA
//! periods into the jitter buffer, whose pacer hands chunks to the sender's
//! live queue. The same runs are made, side by side, with the pinned upstream
//! `airplay-audio` and with the vendored one, and compared:
//!
//! * anchors against the RTP timeline (`rtp / 44100` s), on the monotonic
//!   clock: any change between two syncs is a rate change the receiver
//!   follows;
//! * packet sends against the RTP timeline: lasting shifts, and lateness;
//! * the margin between a packet's arrival and its anchor (the latency the
//!   receiver is given);
//! * the input clock against the monotonic clock.
//!
//! Environment: `AUTO_AIRPLAY_TIMING_SECONDS` sets the measured part of each
//! run (default 8), `AUTO_AIRPLAY_TIMING_CSV` a directory for the raw
//! timelines, `AUTO_AIRPLAY_TIMING_LOG` a log filter (`airplay_audio=warn`).

use std::collections::HashMap;
use std::fmt;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
/// Startup is excluded from the steady-state figures.
const WARM_UP_NS: i128 = 2_000_000_000;
const NTP_UNIX_OFFSET_S: i128 = 2_208_988_800;
/// The daemon gives stop and disconnect 4 s together.
const STOP_LIMIT: Duration = Duration::from_secs(2);

fn measured_duration() -> Duration {
    let seconds = std::env::var("AUTO_AIRPLAY_TIMING_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(8);
    Duration::from_secs(seconds.max(5))
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
            // NTP sync: [8..16] NTP time, [16..20] RTP timestamp.
            84 if len >= 20 => log.syncs.push(SyncPacket {
                arrival: Arrival {
                    mono,
                    wall,
                    rtp: word(16),
                },
                anchor: fixed_point_ns(i128::from(word(8)) - NTP_UNIX_OFFSET_S, word(12)),
                payload_type: 84,
            }),
            // PTP sync: [4..8] RTP timestamp, [8..16] time.
            87 if len >= 28 => log.syncs.push(SyncPacket {
                arrival: Arrival {
                    mono,
                    wall,
                    rtp: word(4),
                },
                anchor: fixed_point_ns(i128::from(word(8)), word(12)),
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
    ($run:ident, $empty_decode:ident, $krate:ident) => {
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

        fn $run(scenario: &Scenario, duration: Duration) -> Run {
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
                tokio::time::sleep(duration).await;
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

        /// Time `calls` decodes take on a live queue with nothing in it (at
        /// the stream rate: no resampler to build).
        fn $empty_decode(calls: u32) -> Duration {
            let (_sender, mut decoder) =
                $krate::LiveAudioDecoder::create_pair(44_100, CHANNELS as u8, LIVE_QUEUE);
            let format = AudioFormat::default();
            let started = Instant::now();
            for _ in 0..calls {
                assert!(decoder.decode_resampled(&format, 352).unwrap().is_none());
            }
            started.elapsed()
        }
    };
}

sender_under_test!(run_upstream, empty_decode_upstream, airplay_audio_upstream);
sender_under_test!(run_vendored, empty_decode_vendored, airplay_audio);

// ----- the figures -----

#[derive(Debug)]
struct Report {
    sender: &'static str,
    scenario: &'static str,
    /// Sync packets in the steady state, and how many were PTP (PT 87).
    syncs: usize,
    ptp_syncs: usize,
    /// How far the first regular sync moved the timeline the first sync set.
    startup_ms: f64,
    /// Anchor minus RTP timeline, steady state: range, largest change between
    /// consecutive syncs, the playback rate change that change implies, and
    /// the anchors' rate against real time.
    anchor_range_ms: f64,
    anchor_step_ms: f64,
    rate_swing_ppm: f64,
    anchor_clock_ppm: f64,
    /// Sends against the RTP timeline, steady state: their rate against real
    /// time (from the lasting shift: median of the last second against median
    /// of the first), the worst lateness against the median, and the packets
    /// the sender itself counted as late, if it counts.
    send_clock_ppm: f64,
    send_late_ms: f64,
    late_count: Option<u64>,
    /// Anchor minus arrival of the packet the sync names: the receiver's
    /// margin, i.e. the latency it is given.
    margin_ms: (f64, f64),
    stop_ms: Option<f64>,
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

fn ms(ns: f64) -> f64 {
    ns / 1e6
}

impl Report {
    fn new(sender: &'static str, scenario: &'static str, run: Run) -> Self {
        let Run {
            mut log,
            stopped,
            stop_at,
            late_count,
        } = run;
        log.audio.sort_by_key(|packet| packet.mono);
        log.syncs.sort_by_key(|sync| sync.arrival.mono);
        dump(sender, scenario, &log);
        let first = *log.audio.first().expect("no audio arrived");
        let rtp_ns = |rtp: u32| f64::from(rtp.wrapping_sub(first.rtp)) * 1e9 / RTP_RATE;
        let steady =
            |arrival: &Arrival| arrival.mono - first.mono >= WARM_UP_NS && arrival.mono < stop_at;

        // Anchors against the RTP timeline, on the monotonic clock (so that
        // the wall clock being slewed meanwhile does not count).
        let residual = |sync: &SyncPacket| {
            (sync.anchor - (sync.arrival.wall - sync.arrival.mono)) as f64
                - rtp_ns(sync.arrival.rtp)
        };
        let regular = log
            .syncs
            .iter()
            .find(|sync| rtp_ns(sync.arrival.rtp) >= 1e9)
            .expect("no regular sync");
        let startup_ms = ms(residual(regular) - residual(&log.syncs[0]));
        let syncs: Vec<&SyncPacket> = log
            .syncs
            .iter()
            .filter(|sync| steady(&sync.arrival))
            .collect();
        assert!(syncs.len() >= 3, "too few sync packets: {}", syncs.len());
        let (low, high) = range(syncs.iter().map(|sync| residual(sync)));
        let mut anchor_step_ms = 0.0_f64;
        let mut rate_swing_ppm = 0.0_f64;
        for pair in syncs.windows(2) {
            let step = residual(pair[1]) - residual(pair[0]);
            let span = rtp_ns(pair[1].arrival.rtp) - rtp_ns(pair[0].arrival.rtp);
            anchor_step_ms = anchor_step_ms.max(ms(step.abs()));
            rate_swing_ppm = rate_swing_ppm.max(step.abs() / span * 1e6);
        }
        let (head, tail) = (syncs[0], syncs[syncs.len() - 1]);
        let anchor_clock_ppm = -(residual(tail) - residual(head))
            / (rtp_ns(tail.arrival.rtp) - rtp_ns(head.arrival.rtp))
            * 1e6;

        // Sends against the RTP timeline.
        let sends: Vec<(i128, f64)> = log
            .audio
            .iter()
            .filter(|packet| steady(packet))
            .map(|packet| {
                let mono = packet.mono - first.mono;
                (mono, mono as f64 - rtp_ns(packet.rtp))
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

        // Arrival-to-anchor margin of the packet each sync names.
        let arrivals: HashMap<u32, i128> = log
            .audio
            .iter()
            .map(|packet| (packet.rtp, packet.wall))
            .collect();
        let margin_ms = range(syncs.iter().filter_map(|sync| {
            arrivals
                .get(&sync.arrival.rtp)
                .map(|wall| ms((sync.anchor - wall) as f64))
        }));

        Self {
            sender,
            scenario,
            syncs: syncs.len(),
            ptp_syncs: syncs.iter().filter(|sync| sync.payload_type == 87).count(),
            startup_ms,
            anchor_range_ms: ms(high - low),
            anchor_step_ms,
            rate_swing_ppm,
            anchor_clock_ppm,
            send_clock_ppm,
            send_late_ms,
            late_count,
            margin_ms,
            stop_ms: stopped.map(|elapsed| elapsed.as_secs_f64() * 1e3),
        }
    }

    fn header() -> String {
        format!(
            "Input: simulated Bluetooth clock {INPUT_DRIFT_PPM:+.0} ppm against real time. Clocks below are against real time (monotonic).\n\
             sender    scenario        syncs ptp | startup  | anchors vs RTP: range  max step  rate swing   clock     | sends: clock   late  (counted) | margin (latency) | stop\n\
             ----------------------------------+----------+---------------------------------------------------+--------------------------------+------------------+--------"
        )
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:<9} {:<15} {:>5} {:>3} | {:>+5.1} ms | {:>14.3} ms {:>6.3} ms {:>6.0} ppm {:>+7.0} ppm | {:>+6.0} ppm {:>6.2} ms {:>7} | {:>6.1}..{:<5.1} ms | {}",
            self.sender,
            self.scenario,
            self.syncs,
            self.ptp_syncs,
            self.startup_ms,
            self.anchor_range_ms,
            self.anchor_step_ms,
            self.rate_swing_ppm,
            self.anchor_clock_ppm,
            self.send_clock_ppm,
            self.send_late_ms,
            self.late_count
                .map_or_else(|| "-".to_owned(), |count| format!("({count})")),
            self.margin_ms.0,
            self.margin_ms.1,
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
    let first = log.audio[0];
    let rtp_ms = |rtp: u32| f64::from(rtp.wrapping_sub(first.rtp)) * 1e3 / RTP_RATE;
    let mut csv = String::from("kind,arrival_ms,rtp_ms,anchor_minus_rtp_ms\n");
    for packet in &log.audio {
        let arrival = (packet.mono - first.mono) as f64 / 1e6;
        csv += &format!("audio,{arrival:.3},{:.3},\n", rtp_ms(packet.rtp));
    }
    for sync in &log.syncs {
        let arrival = (sync.arrival.mono - first.mono) as f64 / 1e6;
        let anchor =
            (sync.anchor - (sync.arrival.wall - sync.arrival.mono) - first.mono) as f64 / 1e6;
        csv += &format!(
            "sync,{arrival:.3},{:.3},{:.3}\n",
            rtp_ms(sync.arrival.rtp),
            anchor - rtp_ms(sync.arrival.rtp)
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
    logs();
    let duration = measured_duration();
    let (upstream, vendored) = thread::scope(|scope| {
        let upstream = scope.spawn(|| run_upstream(scenario, duration));
        let vendored = scope.spawn(|| run_vendored(scenario, duration));
        (upstream.join().unwrap(), vendored.join().unwrap())
    });
    let upstream = Report::new("upstream", scenario.name, upstream);
    let vendored = Report::new("vendored", scenario.name, vendored);
    println!("{}\n{upstream}\n{vendored}", Report::header());
    (upstream, vendored)
}

/// What the vendored sender must do in every scenario.
fn assert_on_time(fixed: &Report, upstream: &Report) {
    // Every sync packet sits on the RTP timeline: no rate change to follow.
    assert!(fixed.anchor_range_ms < 0.05, "anchors wander: {fixed}");
    assert!(fixed.anchor_clock_ppm.abs() < 5.0, "anchors drift: {fixed}");
    assert!(
        fixed.rate_swing_ppm < 50.0,
        "anchors imply a rate change: {fixed}"
    );
    assert!(
        fixed.startup_ms.abs() < 0.05,
        "the timeline moved at startup: {fixed}"
    );
    // Packets go out on that timeline: late ones catch up, nothing shifts it.
    assert!(
        fixed.send_clock_ppm.abs() < 500.0,
        "the sends drift: {fixed}"
    );
    // The receiver is given no more latency than upstream gave it in its
    // steady state (render delay minus ten packets: 170.2 ms), and late
    // packets stay well inside it.
    assert!(
        fixed.margin_ms.1 < 170.3,
        "more latency than before: {fixed}"
    );
    assert!(
        fixed.margin_ms.0 > 70.0,
        "packets sent far too late: {fixed}"
    );
    assert!(
        upstream.margin_ms.1 > fixed.margin_ms.1,
        "upstream gave less latency: {upstream}"
    );
    // Live streaming never uses PTP sync packets.
    assert_eq!(fixed.ptp_syncs, 0);
    assert!(
        fixed.stop_ms.is_some_and(|ms| ms < 1_000.0),
        "stop: {fixed}"
    );
}

#[test]
fn sync_anchors_follow_the_rtp_timeline() {
    let (upstream, fixed) = compare(&Scenario {
        name: "48 kHz input",
        input_rate: 48_000,
        stalls: None,
    });
    // Upstream stamps its first sync before its queue fills, every later one
    // with the queue full: the timeline jumps back by the queue's depth.
    assert!(upstream.startup_ms < -20.0, "{upstream}");
    assert_on_time(&fixed, &upstream);
}

#[test]
fn stalls_of_the_sending_task_move_neither_anchors_nor_sends() {
    let (upstream, fixed) = compare(&Scenario {
        name: "stalls 1-11 ms",
        input_rate: 48_000,
        stalls: Some(Stalls {
            gap_us: (0, 20_000),
            block_us: (1_000, 11_000),
            seed: 0x5eed,
        }),
    });
    // Upstream announces each sync at the time it was prepared: a stall
    // before that moves the anchor by its length.
    assert!(upstream.anchor_step_ms > 1.0, "{upstream}");
    assert_on_time(&fixed, &upstream);
}

#[test]
fn the_live_decoder_does_not_wait_for_input() {
    const CALLS: u32 = 50;
    let upstream = empty_decode_upstream(CALLS);
    let vendored = empty_decode_vendored(CALLS);
    println!(
        "{CALLS} decodes on an empty live queue: upstream {upstream:?}, vendored {vendored:?}"
    );
    // Upstream waits 2 ms for input on every call, in the send loop.
    assert!(
        upstream >= Duration::from_millis(u64::from(CALLS) * 2),
        "{upstream:?}"
    );
    assert!(vendored < Duration::from_millis(10), "{vendored:?}");
}
