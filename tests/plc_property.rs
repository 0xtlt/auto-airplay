//! Property test of the whole input path: whatever mix of lost packets,
//! outages, jitter and late bursts the Bluetooth link throws at the jitter
//! buffer, the paced output never steps more than the signal's own slope
//! allows. A step is what a click is.

use std::time::Duration;

use auto_airplay::plc::{JitterBuffer, PLAYOUT_CHUNK_FRAMES};

const RATE: u32 = 48_000;
const CH: usize = 2;
/// How long the source runs (ms). The last event is at 8.5 s at the latest and
/// its recovery (up to 1.3 s of outage, then the cushion and the refill) is
/// played out after that: the run goes on until the queue is drained.
const SOURCE_MS: u32 = 12_000;
/// Events happen between these times: after even a 2 s cushion has filled.
const EVENTS_FROM_MS: f64 = 3_500.0;
const EVENTS_TO_MS: f64 = 8_500.0;
const ATTACH_MS: u32 = 1_000;
const CUSHIONS: [u32; 3] = [200, 400, 2_000];
/// More with `PLC_SEEDS=2000 cargo test --release --test plc_property`.
const DEFAULT_SEEDS: u64 = 200;
/// A step may be this many times the signal's natural maximum step...
const STEP_FACTOR: f32 = 1.5;
/// ...plus this much (dither of the integer signal, rounding of the mixes).
const STEP_EPSILON: f32 = 6.0;

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
    fn between(&mut self, low: f64, high: f64) -> f64 {
        low + (self.next() % 1_000_000) as f64 / 1_000_000.0 * (high - low)
    }
}

struct Scenario {
    signal: Vec<i16>,
    natural: [f32; CH],
    /// (arrival ms, first frame, frames) of each read, in arrival order.
    arrivals: Vec<(f64, usize, usize)>,
    description: String,
}

fn scenario(seed: u64) -> Scenario {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    for _ in 0..4 {
        rng.next();
    }
    let period = [512, 960, 1_024][rng.below(3) as usize];
    let blocks = SOURCE_MS as usize * RATE as usize / 1000 / period;
    let hz = rng.between(100.0, 2_000.0);
    let mut natural = [0.0; CH];
    let mut amplitude = [0.0; CH];
    let mut phase = [0.0; CH];
    for c in 0..CH {
        amplitude[c] = rng.between(0.1, 0.9) * 32_767.0;
        phase[c] = rng.between(0.0, std::f64::consts::TAU);
        natural[c] = (amplitude[c] * std::f64::consts::TAU * hz / f64::from(RATE)) as f32;
    }
    let block_ms = period as f64 * 1000.0 / f64::from(RATE);
    let nominal = |block: usize| (block + 1) as f64 * block_ms;
    let mut lost = vec![false; blocks];
    // The source jumps ahead by this many frames at a block, with nothing in
    // the timing to show it: only the waveform can tell.
    let mut jump = vec![0_usize; blocks];
    // Blocks that reach the buffer as several short reads (64 to 512 frames):
    // the first after a loss or an outage, so that crossfades are resumed.
    let mut short = vec![false; blocks];
    let mut late = vec![0.0_f64; blocks];
    let mut description = format!("{hz:.0} Hz, period {period}");

    let mut time = EVENTS_FROM_MS;
    while time < EVENTS_TO_MS {
        let block = (time / block_ms) as usize;
        match rng.below(7) {
            0 | 1 => {
                // One packet, or a burst of them.
                let count = if rng.below(2) == 0 {
                    1
                } else {
                    1 + rng.below(9) as usize
                };
                description += &format!(", loss {count}@{block}");
                lost[block..block + count].fill(true);
            }
            2 => {
                let count = 10 + rng.below(50) as usize;
                description += &format!(", outage {count}@{block}");
                lost[block..block + count].fill(true);
            }
            3 => {
                // Jitter: constant or flickering, 0 to 14 ms, for some blocks.
                let count = 1 + rng.below(14) as usize;
                let ms = rng.between(0.0, 14.0);
                let flicker = rng.below(2) == 0;
                description += &format!(", jitter {ms:.0}ms x{count}@{block}");
                for slot in &mut late[block..block + count] {
                    *slot = if flicker { rng.between(0.0, ms) } else { ms };
                }
            }
            4 => {
                // Held back and delivered together.
                let count = 2 + rng.below(13) as usize;
                let release = nominal(block + count);
                description += &format!(", late burst {count}@{block}");
                for (offset, slot) in late[block..block + count].iter_mut().enumerate() {
                    *slot = release - nominal(block + offset);
                }
            }
            6 => {
                let frames = 1 + rng.below(6 * period as u64) as usize;
                description += &format!(", skip {frames}f@{block}");
                jump[block] += frames;
                short[block] = rng.below(2) == 0;
            }
            _ => {
                // A loss, jitter on the recovery, another loss.
                let first = 1 + rng.below(3) as usize;
                let gap = 1 + rng.below(8) as usize;
                let ms = rng.between(0.0, 14.0);
                description += &format!(", loss {first}+jitter {ms:.0}ms x{gap}+loss@{block}");
                lost[block..block + first].fill(true);
                late[block + first..block + first + gap].fill(ms);
                lost[block + first + gap] = true;
            }
        }
        time += block_ms * (4.0 + rng.below(40) as f64);
    }

    for block in 1..blocks {
        if lost[block - 1] && !lost[block] && rng.below(3) != 0 {
            short[block] = true;
        }
    }
    description += &format!(
        ", {} short recoveries",
        short.iter().filter(|&&s| s).count()
    );

    // Where each block starts in the source, and the source itself.
    let mut start_of = Vec::with_capacity(blocks);
    let mut skipped = 0;
    for (block, &ahead) in jump.iter().enumerate() {
        skipped += ahead;
        start_of.push(block * period + skipped);
    }
    let total = blocks * period + skipped;
    let mut signal = Vec::with_capacity(total * CH);
    for frame in 0..total {
        for c in 0..CH {
            let angle = std::f64::consts::TAU * hz * frame as f64 / f64::from(RATE) + phase[c];
            signal.push((angle.sin() * amplitude[c]) as i16);
        }
    }

    // In order: a block never arrives before the one before it.
    let mut arrivals = Vec::new();
    let mut previous = 0.0_f64;
    for block in 0..blocks {
        if lost[block] {
            continue;
        }
        let when = (nominal(block) + late[block]).max(previous);
        previous = when;
        let first = start_of[block];
        if short[block] {
            let mut done = 0;
            while done < period {
                let count = (64 + rng.below(449) as usize).min(period - done);
                arrivals.push((when, first + done, count));
                done += count;
            }
        } else {
            arrivals.push((when, first, period));
        }
    }
    Scenario {
        signal,
        natural,
        arrivals,
        description,
    }
}

fn at(ms: f64) -> Duration {
    Duration::from_secs_f64(ms / 1000.0)
}

/// Runs the scenario through a paced buffer, attached at 1 s; returns the
/// audio that went out and the stats.
fn play(scenario: &Scenario, cushion_ms: u32) -> (Vec<i16>, String) {
    let mut buffer = JitterBuffer::new(RATE, CH as u8, cushion_ms);
    let mut pacer = buffer.pacer();
    let mut chunk = vec![0_i16; PLAYOUT_CHUNK_FRAMES * CH];
    let mut out = Vec::new();
    let mut next = 0;
    let last_arrival = scenario.arrivals.last().map_or(0.0, |arrival| arrival.0);
    let limit = last_arrival as u32 + 3 * cushion_ms + 4_000;
    let mut drained = false;
    for tick in 0..limit {
        let now = f64::from(tick);
        while next < scenario.arrivals.len() && scenario.arrivals[next].0 <= now {
            let (time, start, count) = scenario.arrivals[next];
            buffer.push_at(
                at(time),
                &scenario.signal[start * CH..(start + count) * CH],
                0,
            );
            next += 1;
        }
        if tick == ATTACH_MS {
            buffer.start_playout();
        }
        if tick >= ATTACH_MS {
            while buffer.pull_chunk(&mut pacer, at(now), &mut chunk) {
                pacer.sent(PLAYOUT_CHUNK_FRAMES);
                out.extend_from_slice(&chunk);
            }
        }
        // Everything that arrived has been played: every junction of the run
        // went through the output.
        if next == scenario.arrivals.len() && buffer.fill_frames() < PLAYOUT_CHUNK_FRAMES {
            drained = true;
            break;
        }
    }
    assert!(drained, "the queue never drained: {}", scenario.description);
    (out, buffer.take_stats().to_string())
}

fn worst_step_ratio(out: &[i16], natural: [f32; CH]) -> (f32, usize) {
    let mut worst = (0.0_f32, 0);
    for (c, &slope) in natural.iter().enumerate() {
        let values: Vec<f32> = out
            .iter()
            .skip(c)
            .step_by(CH)
            .map(|&v| f32::from(v))
            .collect();
        for (index, pair) in values.windows(2).enumerate() {
            let step = (pair[1] - pair[0]).abs();
            let allowed = STEP_FACTOR * slope + STEP_EPSILON;
            if step / allowed > worst.0 {
                worst = (step / allowed, index);
            }
        }
    }
    worst
}

#[test]
fn whatever_the_link_does_the_output_never_clicks() {
    let seeds: u64 = std::env::var("PLC_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SEEDS);
    let mut failures = Vec::new();
    let mut totals = (0_u64, 0_u64);
    let mut per = ([0_u64; 3], [0_u64; 3]);
    for seed in 1..=seeds {
        let scenario = scenario(seed);
        for cushion in CUSHIONS {
            let (out, stats) = play(&scenario, cushion);
            let by = match cushion {
                200 => 0,
                400 => 1,
                _ => 2,
            };
            for (key, total) in [
                ("gaps=", &mut totals.0),
                ("splices=", &mut totals.1),
                ("underruns=", &mut per.0[by]),
                ("splices=", &mut per.1[by]),
            ] {
                let at = stats.find(key).unwrap() + key.len();
                *total += stats[at..]
                    .split(' ')
                    .next()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap();
            }
            assert_eq!(
                out.len() % (PLAYOUT_CHUNK_FRAMES * CH),
                0,
                "seed {seed}: whole chunks only"
            );
            let (ratio, index) = worst_step_ratio(&out, scenario.natural);
            if ratio > 1.0 {
                failures.push(format!(
                    "seed {seed} cushion {cushion}: step {ratio:.2}x the limit at output frame {index} ({})\n    {stats}",
                    scenario.description
                ));
            }
        }
    }
    println!(
        "TOTALS gaps={} splices={} underruns/cushion={:?} splices/cushion={:?}",
        totals.0, totals.1, per.0, per.1
    );
    assert!(
        failures.is_empty(),
        "{} of {} runs clicked:\n{}",
        failures.len(),
        seeds * CUSHIONS.len() as u64,
        failures
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
