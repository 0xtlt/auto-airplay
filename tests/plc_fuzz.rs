//! Randomised input to the capture side of the jitter buffer: whatever the
//! sizes, timings and backlogs, a push returns promptly and memory stays bounded.

use std::time::{Duration, Instant};

use auto_airplay::plc::JitterBuffer;

const RATE: u32 = 48_000;
const CH: usize = 2;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// Music-like audio with stretches of digital zeros and of near silence.
fn block(rng: &mut Rng, frames: usize, phase: &mut f32) -> Vec<i16> {
    let mut samples = Vec::with_capacity(frames * CH);
    let zeros = rng.chance(10);
    let quiet = rng.chance(10);
    for _ in 0..frames {
        *phase += 0.07;
        let value = if zeros {
            0
        } else if quiet {
            (rng.below(5) as i16) - 2
        } else {
            ((*phase).sin() * 9_000.0) as i16 + (rng.below(2_000) as i16 - 1_000)
        };
        samples.extend([value, value / 2]);
    }
    samples
}

fn fuzz(seed: u64, consumer: bool, jitter_ms: u32, simulated_s: u64) {
    let mut rng = Rng(seed);
    let mut buffer = JitterBuffer::new(RATE, CH as u8, jitter_ms);
    if consumer {
        buffer.start_playout();
    }
    let mut pacer = buffer.pacer();
    let mut chunk = vec![0_i16; buffer.chunk_frames() * CH];
    let mut phase = 0.0;
    // The first read finds a big backlog in the device.
    let mut first = true;
    let mut clock_ms = 0.0_f64;
    let mut worst = Duration::ZERO;
    let mut stats_ticks = 0;
    let mut next_stats = 60_000.0;
    let mut capacity_seen = 0;

    while clock_ms < (simulated_s * 1000) as f64 {
        let frames = 1 + rng.below(4_096) as usize;
        // Usually in step with real time, sometimes late, sometimes a long stall.
        clock_ms += frames as f64 * 1000.0 / f64::from(RATE);
        if rng.chance(5) {
            clock_ms += rng.below(300) as f64;
        }
        if rng.chance(1) {
            clock_ms += rng.below(5_000) as f64;
        }
        let backlog = if first {
            (rng.below(4) * 4_096) as usize
        } else if rng.chance(20) {
            rng.below(8_192) as usize
        } else {
            0
        };
        first = false;
        let samples = block(&mut rng, frames, &mut phase);

        let started = Instant::now();
        buffer.push_at(
            Duration::from_secs_f64(clock_ms / 1000.0),
            &samples,
            backlog,
        );
        worst = worst.max(started.elapsed());
        capacity_seen = capacity_seen.max(buffer.fill_frames());

        if consumer {
            let now = Duration::from_secs_f64(clock_ms / 1000.0);
            while buffer.pull_chunk(&mut pacer, now, &mut chunk) {
                pacer.sent(buffer.chunk_frames());
            }
        }
        while clock_ms >= next_stats {
            buffer.take_stats();
            next_stats += 60_000.0;
            stats_ticks += 1;
        }
    }

    assert!(
        worst < Duration::from_millis(250),
        "seed {seed}: a push took {worst:?}"
    );
    let limit = (2 * buffer.target_frames()).max(buffer.target_frames() + 2 * RATE as usize / 10)
        + 2 * 4_096;
    assert!(
        capacity_seen <= limit,
        "seed {seed}: fill {capacity_seen} over {limit}"
    );
    assert!(
        stats_ticks >= simulated_s / 60 - 1,
        "seed {seed}: {stats_ticks} stats ticks"
    );
}

#[test]
fn random_captures_never_block_or_grow_without_a_consumer() {
    for seed in 1..=6 {
        fuzz(
            seed * 0x9E37_79B9,
            false,
            [50, 400, 2_000][(seed % 3) as usize],
            600,
        );
    }
}

#[test]
fn random_captures_never_block_or_grow_with_a_consumer() {
    for seed in 1..=6 {
        fuzz(
            seed * 0x85EB_CA6B,
            true,
            [50, 400, 2_000][(seed % 3) as usize],
            600,
        );
    }
}

/// The widest range of inputs the capture side could ever see: any rate and
/// channel count, tiny and huge reads, identical timestamps, wild backlogs
/// (including garbage), digital silence, and stalls. Each seed runs under a
/// deadline, so a hang is a failure, not a stuck test run.
fn wild(seed: u64) {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut rng = Rng(seed | 1);
        let rate = [8_000, 44_100, 48_000, 96_000][rng.below(4) as usize];
        let channels = 1 + rng.below(2) as u8;
        let jitter = [50, 100, 400, 2_000, 10_000][rng.below(5) as usize];
        let mut buffer = JitterBuffer::new(rate, channels, jitter);
        if rng.chance(50) {
            buffer.start_playout();
        }
        let mut pacer = buffer.pacer();
        let mut chunk = vec![0_i16; buffer.chunk_frames() * usize::from(channels)];
        let mut clock = 0.0_f64;
        let mut phase = 0.0_f32;
        for _ in 0..2_000 {
            let frames = match rng.below(10) {
                0 => 1 + rng.below(3),
                1 => 1 + rng.below(20_000),
                _ => 1 + rng.below(1_100),
            } as usize;
            match rng.below(20) {
                0 => {}
                1 => clock += rng.below(3_000) as f64,
                2 => clock += rng.below(40) as f64,
                _ => clock += frames as f64 * 1000.0 / f64::from(rate),
            }
            let backlog = match rng.below(10) {
                0 => rng.next() as usize,
                1 => rng.below(1 << 20) as usize,
                _ => 0,
            };
            let mut samples = Vec::new();
            let zeros = rng.chance(30);
            for _ in 0..frames {
                phase += 0.05;
                let value = if zeros {
                    0
                } else {
                    (phase.sin() * 20_000.0) as i16
                };
                for _ in 0..channels {
                    samples.push(value);
                }
            }
            if rng.chance(5) {
                buffer.note_xrun();
            }
            buffer.push_at(Duration::from_secs_f64(clock / 1000.0), &samples, backlog);
            let now = Duration::from_secs_f64(clock / 1000.0);
            for _ in 0..3 {
                if !buffer.pull_chunk(&mut pacer, now, &mut chunk) {
                    break;
                }
                pacer.sent(buffer.chunk_frames());
            }
            if rng.chance(2) {
                buffer.take_stats();
            }
        }
        let _ = done.send(());
    });
    finished
        .recv_timeout(Duration::from_secs(60))
        .unwrap_or_else(|_| panic!("seed {seed}: the capture side hung"));
}

#[test]
fn wild_inputs_never_hang_or_panic() {
    for seed in 1..=60_u64 {
        wild(seed.wrapping_mul(0x2545_F491_4F6C_DD1D));
    }
}
