//! Packet loss concealment, jitter buffer, gap detection and pacing, on
//! simulated Bluetooth streams. Signals are near-full-scale sines: any splice
//! shows as a step much bigger than the sine's own slope.

use std::time::Duration;

use auto_airplay::plc::{
    Due, GapDetector, GapVerdict, JitterBuffer, MIN_JITTER_BUFFER_MS, PLAYOUT_CHUNK_FRAMES, Pacer,
};

const RATE: u32 = 48_000;
const CH: usize = 2;
const AMPLITUDE: f32 = 0.9 * 32_767.0;
const LEFT_HZ: f32 = 1_000.0;
const RIGHT_HZ: f32 = 1_300.0;
/// A step may exceed the signal's own maximum slope by this factor.
const STEP_MARGIN: f32 = 1.5;
const PERIOD: usize = 1024;

fn frames(ms: u32) -> usize {
    (RATE as usize * ms as usize) / 1000
}

fn ms(duration: usize) -> f64 {
    duration as f64 * 1000.0 / f64::from(RATE)
}

/// `count` stereo frames of the source starting at absolute frame `start`.
/// Left and right differ in frequency, so channels are told apart.
fn source(start: usize, count: usize) -> Vec<i16> {
    let mut samples = Vec::with_capacity(count * CH);
    for frame in start..start + count {
        for hz in [LEFT_HZ, RIGHT_HZ] {
            let phase = std::f32::consts::TAU * hz * frame as f32 / RATE as f32;
            samples.push((phase.sin() * AMPLITUDE) as i16);
        }
    }
    samples
}

fn natural_step(hz: f32) -> f32 {
    AMPLITUDE * std::f32::consts::TAU * hz / RATE as f32
}

fn max_step(samples: &[i16], channel: usize) -> f32 {
    let values: Vec<f32> = samples
        .iter()
        .skip(channel)
        .step_by(CH)
        .map(|&sample| f32::from(sample))
        .collect();
    values
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0, f32::max)
}

/// No step in either channel is bigger than the sine's own slope (plus margin).
#[track_caller]
fn assert_continuous(samples: &[i16]) {
    for (channel, hz) in [(0, LEFT_HZ), (1, RIGHT_HZ)] {
        let step = max_step(samples, channel);
        let limit = STEP_MARGIN * natural_step(hz);
        assert!(
            step <= limit,
            "channel {channel}: step {step} exceeds {limit} (natural {})",
            natural_step(hz)
        );
    }
}

fn pull_all(buffer: &mut JitterBuffer, count: usize) -> Vec<i16> {
    let mut out = vec![0_i16; count * CH];
    assert!(buffer.pull(&mut out, true));
    out
}

/// Takes every complete chunk of real audio out of the buffer.
fn drain(buffer: &mut JitterBuffer) -> Vec<i16> {
    let mut all = Vec::new();
    let mut chunk = vec![0_i16; buffer.chunk_frames() * CH];
    while buffer.pull(&mut chunk, false) {
        all.extend_from_slice(&chunk);
    }
    all
}

fn new_buffer(jitter_ms: u32) -> JitterBuffer {
    JitterBuffer::new(RATE, CH as u8, jitter_ms)
}

fn detector() -> GapDetector {
    GapDetector::standard(RATE)
}

fn at(ms: f64) -> Duration {
    Duration::from_secs_f64(ms / 1000.0)
}

fn is_silent(samples: &[i16]) -> bool {
    samples.iter().all(|&sample| sample == 0)
}

// ----- output length and startup -----

#[test]
fn pull_always_returns_exactly_the_requested_length() {
    let mut buffer = new_buffer(100);
    buffer.start_playout();
    let mut position = 0;
    for (round, count) in [1, 7, 480, 3, 1000, 480, 480, 2, 9_999, 480]
        .into_iter()
        .enumerate()
    {
        if round % 3 == 0 {
            buffer.push(&source(position, 700));
            position += 700;
        }
        let mut out = vec![i16::MAX; count * CH];
        assert!(buffer.pull(&mut out, true), "round {round}");
        assert_eq!(out.len(), count * CH);
    }
}

#[test]
fn startup_with_an_empty_buffer_is_silence_then_a_fade_in() {
    let mut buffer = new_buffer(100);
    buffer.start_playout();

    let silence = pull_all(&mut buffer, frames(50));
    assert!(is_silent(&silence));
    let stats = buffer.take_stats();
    assert_eq!(
        (stats.losses, stats.underruns),
        (0, 0),
        "startup is not a loss"
    );

    buffer.push(&source(0, frames(300)));
    let out = pull_all(&mut buffer, frames(100));

    assert!(
        out[0].unsigned_abs() < 200 && out[1].unsigned_abs() < 200,
        "starts from silence"
    );
    assert_continuous(&out);
    // Past the 10 ms crossfade the real audio comes through unchanged.
    let skip = frames(10) * CH;
    assert_eq!(out[skip..], source(0, frames(100))[skip..]);
}

#[test]
fn attaching_fades_in_over_the_queued_audio() {
    let mut buffer = new_buffer(400);
    buffer.push(&source(0, frames(1_000)));
    buffer.start_playout();

    let out = pull_all(&mut buffer, frames(50));

    assert!(out[0].unsigned_abs() < 200);
    assert_continuous(&out);
    assert_eq!(buffer.take_stats().losses, 0);
}

// ----- underrun concealment -----

#[test]
fn underrun_is_continuous_and_exactly_the_requested_length() {
    let mut buffer = new_buffer(100);
    buffer.start_playout();
    buffer.push(&source(0, frames(100)));
    let mut out = pull_all(&mut buffer, frames(60));
    // Asks for more than is left: 40 ms of audio, then concealment.
    out.extend(pull_all(&mut buffer, frames(40) + 333));
    assert_eq!(out.len(), (frames(100) + 333) * CH);
    out.extend(pull_all(&mut buffer, frames(20)));

    assert_continuous(&out);
    let stats = buffer.take_stats();
    assert_eq!((stats.underruns, stats.losses, stats.concealed), (1, 1, 1));
    assert_eq!(stats.concealed_ms, 26, "333 + 960 frames");
}

#[test]
fn a_long_underrun_fades_to_silence_and_stays_silent() {
    let mut buffer = new_buffer(100);
    buffer.start_playout();
    buffer.push(&source(0, frames(100)));
    let mut out = pull_all(&mut buffer, frames(100));
    out.extend(pull_all(&mut buffer, frames(300)));

    assert_continuous(&out);
    let after_fade = &out[(frames(100) + frames(30)) * CH..];
    assert!(is_silent(after_fade), "silent once the 30 ms fade is over");
    let during_fade = &out[frames(100) * CH..(frames(100) + frames(15)) * CH];
    assert!(!is_silent(during_fade), "the signal carries on at first");
    assert_eq!(buffer.take_stats().underruns, 1, "one event, however long");
}

#[test]
fn an_underrun_shorter_than_the_fade_resumes_with_a_crossfade() {
    let mut buffer = new_buffer(100);
    buffer.start_playout();
    buffer.push(&source(0, frames(100)));
    let mut out = pull_all(&mut buffer, frames(100));
    out.extend(pull_all(&mut buffer, frames(5)));
    // The late audio arrives in force: the resume threshold is met at once.
    buffer.push(&source(frames(300), frames(200)));
    out.extend(pull_all(&mut buffer, frames(100)));

    assert_continuous(&out);
    let resumed = &out[(frames(105) + frames(20)) * CH..];
    assert!(!is_silent(resumed));
}

#[test]
fn concealment_waits_for_the_whole_cushion_to_be_back() {
    let mut buffer = new_buffer(400);
    buffer.start_playout();
    buffer.push(&source(0, frames(400)));
    let mut out = pull_all(&mut buffer, frames(400));
    out.extend(pull_all(&mut buffer, frames(10)));
    let resume = buffer.resume_frames();
    assert_eq!(resume, buffer.target_frames(), "the whole cushion");
    assert_eq!(resume, frames(400));

    // Just short of the target: still concealing, and a pull that would need
    // real audio is refused.
    buffer.push(&source(frames(2_000), resume - 1));
    let mut chunk = vec![0_i16; frames(10) * CH];
    assert!(!buffer.pull(&mut chunk, false), "below the threshold");
    assert!(buffer.pull(&mut chunk, true));
    assert_eq!(
        buffer.fill_frames(),
        resume - 1,
        "audio is kept, not consumed"
    );

    // At the target, real audio resumes, with a crossfade.
    buffer.push(&source(frames(2_000) + resume - 1, 1));
    assert!(buffer.pull(&mut chunk, false));
    out.extend_from_slice(&chunk);
    assert_continuous(&out);
    assert_eq!(buffer.fill_frames(), resume - frames(10));
    assert_eq!(buffer.take_stats().underruns, 1, "no flapping");
}

#[test]
fn a_cold_start_waits_for_the_whole_cushion() {
    let mut buffer = new_buffer(400);
    buffer.push(&source(0, frames(250)));
    buffer.start_playout();
    let mut chunk = vec![0_i16; frames(10) * CH];

    // Not enough queued: real audio only, so nothing goes out yet.
    assert!(!buffer.pull(&mut chunk, false));
    buffer.push(&source(frames(250), frames(150)));
    assert!(buffer.pull(&mut chunk, false), "the full 400 ms is queued");
    assert_eq!(buffer.take_stats().losses, 0);
}

#[test]
fn repeated_back_to_back_underruns_stay_continuous() {
    let mut buffer = new_buffer(100);
    buffer.start_playout();
    let mut out = Vec::new();
    let mut position = 0;
    for round in 0..12 {
        let burst = frames(60 + 7 * round);
        buffer.push(&source(position, burst));
        position += burst + frames(40);
        out.extend(pull_all(&mut buffer, burst + frames(25 + 3 * round)));
    }
    out.extend(pull_all(&mut buffer, frames(50)));

    assert_continuous(&out);
    assert!(buffer.take_stats().underruns >= 6);
}

#[test]
fn a_second_underrun_straight_after_resuming_is_continuous() {
    let mut buffer = new_buffer(50);
    buffer.start_playout();
    buffer.push(&source(0, frames(50)));
    let mut out = pull_all(&mut buffer, frames(60));
    // Enough to resume (the whole 50 ms), consumed immediately, over and over.
    for round in 0..8 {
        buffer.push(&source(frames(1_000 * (round + 1)), frames(51)));
        out.extend(pull_all(&mut buffer, frames(60)));
    }

    assert_continuous(&out);
}

// ----- lost audio found from the arrival times -----

/// Real-time arrival of `PERIOD`-frame blocks of the source, with some blocks
/// lost, optionally delayed, as the capture thread would push them. Pulls
/// 10 ms chunks through a [`Pacer`] like the playout thread. Everything that
/// came out is in `out`.
struct Pipeline {
    buffer: JitterBuffer,
    pacer: Pacer,
    out: Vec<i16>,
    now_ms: f64,
    pushed_frames: usize,
    /// The fill (ms of the source clock, frames) every 100 ms.
    fills: Vec<(f64, usize)>,
}

impl Pipeline {
    fn attached(jitter_ms: u32, prefill_ms: u32) -> Self {
        let mut buffer = new_buffer(jitter_ms);
        // The stream has been running for a while: audio is queued already.
        let blocks = frames(prefill_ms) / PERIOD;
        for block in 0..blocks {
            buffer.push_at(
                at(block as f64 * block_ms()),
                &source(block * PERIOD, PERIOD),
                0,
            );
        }
        let pushed_frames = blocks * PERIOD;
        let now_ms = blocks as f64 * block_ms();
        buffer.start_playout();
        let pacer = buffer.pacer();
        Self {
            buffer,
            pacer,
            out: Vec::new(),
            now_ms,
            pushed_frames,
            fills: Vec::new(),
        }
    }

    /// Runs 1 ms steps. `arrival(block)` is when source block `block` reaches
    /// the buffer, in ms of the source clock, or `None` if it is lost.
    fn run(&mut self, seconds: usize, first_block: usize, arrival: impl Fn(usize) -> Option<f64>) {
        let mut pending: Vec<(f64, usize)> = (first_block..)
            .take_while(|&block| {
                (block as f64 * block_ms()) < self.now_ms + seconds as f64 * 1e3 + 500.0
            })
            .filter_map(|block| arrival(block).map(|time| (time, block)))
            .collect();
        pending.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut next = 0;
        let end = self.now_ms + seconds as f64 * 1000.0;
        let chunk_len = self.buffer.chunk_frames();
        let mut chunk = vec![0_i16; chunk_len * CH];

        let mut step = 0_u32;
        while self.now_ms < end {
            self.now_ms += 1.0;
            step += 1;
            if step.is_multiple_of(100) {
                self.fills.push((self.now_ms, self.buffer.fill_frames()));
            }
            while next < pending.len() && pending[next].0 <= self.now_ms {
                let (time, block) = pending[next];
                self.buffer
                    .push_at(at(time), &source(block * PERIOD, PERIOD), 0);
                self.pushed_frames += PERIOD;
                next += 1;
            }
            while self
                .buffer
                .pull_chunk(&mut self.pacer, at(self.now_ms), &mut chunk)
            {
                self.pacer.sent(chunk_len);
                self.out.extend_from_slice(&chunk);
            }
        }
    }
}

fn block_ms() -> f64 {
    ms(PERIOD)
}

#[test]
fn a_170_ms_hole_is_concealed_in_place() {
    let mut pipeline = Pipeline::attached(400, 400);
    let first = pipeline.pushed_frames / PERIOD;
    // 8 blocks (170.7 ms) never arrive; the source clock keeps going.
    let lost = first + 40..first + 48;
    pipeline.run(10, first, |block| {
        (!lost.contains(&block)).then(|| block as f64 * block_ms())
    });

    assert_continuous(&pipeline.out);
    let stats = pipeline.buffer.take_stats();
    assert_eq!(stats.gaps, 1, "{stats}");
    assert_eq!(
        (stats.losses, stats.concealed, stats.underruns),
        (1, 1, 0),
        "{stats}"
    );
    assert!((stats.concealed_ms as i64 - 170).abs() <= 3, "{stats}");
    // The timeline stays whole: audio out == real audio in + concealment.
    let queued = pipeline.buffer.fill_frames();
    let concealed = (stats.concealed_ms as usize * RATE as usize) / 1000;
    let accounted = pipeline.pushed_frames + concealed;
    let played = pipeline.out.len() / CH;
    assert!(
        (accounted as i64 - (played + queued) as i64).abs() <= 400,
        "pushed {} + concealed {concealed} vs played {played} + queued {queued}: {stats}",
        pipeline.pushed_frames,
    );
    // The cushion survives the loss.
    assert!(
        ms(queued) > 250.0 && ms(queued) < 450.0,
        "fill {} ms",
        ms(queued)
    );
}

#[test]
fn the_hole_is_bridged_by_a_fade_and_a_crossfade() {
    let mut buffer = new_buffer(2_000);
    let mut clock = 0.0;
    let mut position = 0;
    for _ in 0..40 {
        buffer.push_at(at(clock), &source(position, PERIOD), 0);
        position += PERIOD;
        clock += block_ms();
    }
    // Eight blocks go missing.
    position += 8 * PERIOD;
    clock += 8.0 * block_ms();
    for _ in 0..20 {
        buffer.push_at(at(clock), &source(position, PERIOD), 0);
        position += PERIOD;
        clock += block_ms();
    }
    let all = drain(&mut buffer);

    assert_continuous(&all);
    // 170 ms of source audio replaced: fading out over 30 ms, then silent,
    // then back in over 10 ms.
    let hole = 40 * PERIOD;
    let silent = &all[(hole + frames(31)) * CH..(hole + 8 * PERIOD - frames(1)) * CH];
    assert!(is_silent(silent));
    assert!(!is_silent(&all[hole * CH..(hole + frames(10)) * CH]));
    assert_eq!(buffer.take_stats().gaps, 1);
}

/// `late_blocks` blocks reach the buffer together, long after their time.
fn late_burst(jitter_ms: u32, prefill_ms: u32, late_blocks: usize, seconds: usize) -> Pipeline {
    let mut pipeline = Pipeline::attached(jitter_ms, prefill_ms);
    let first = pipeline.pushed_frames / PERIOD;
    let late = first + 40..first + 40 + late_blocks;
    let burst = (first + 40 + late_blocks) as f64 * block_ms();
    pipeline.run(seconds, first, |block| {
        Some(if late.contains(&block) {
            burst
        } else {
            block as f64 * block_ms()
        })
    });
    pipeline
}

#[test]
fn audio_that_is_merely_late_is_left_alone() {
    let mut pipeline = late_burst(400, 400, 8, 30);

    assert_continuous(&pipeline.out);
    let stats = pipeline.buffer.take_stats();
    assert_eq!(
        (stats.gaps, stats.underruns, stats.concealed_ms),
        (0, 0, 0),
        "{stats}"
    );
    assert!(stats.buffer_fill_ms <= 500, "within the band: {stats}");
}

#[test]
fn a_slow_catch_up_is_taken_for_a_hole_and_the_surplus_is_trimmed() {
    // 300 ms late, then caught up at twice the speed: the suspicion outlasts
    // its window before the audio is back, so the hole is concealed, and the
    // audio that then arrives is a surplus, trimmed without a click.
    let mut pipeline = Pipeline::attached(1_000, 900);
    let first = pipeline.pushed_frames / PERIOD;
    // Delivery stalls for 14 blocks (298 ms), then the backlog goes out at
    // twice real time, in order, new blocks queueing behind it.
    let mut arrival = Vec::new();
    let mut previous = 0.0;
    for block in 0..6_000_usize {
        let nominal = block as f64 * block_ms();
        let time = match block.checked_sub(first + 40) {
            None => nominal,
            Some(0) => nominal + 14.0 * block_ms(),
            Some(_) => nominal.max(previous + block_ms() / 2.0),
        };
        arrival.push(time);
        previous = time;
    }
    pipeline.run(60, first, |block| Some(arrival[block]));

    assert_continuous(&pipeline.out);
    let stats = pipeline.buffer.take_stats();
    assert_eq!((stats.gaps, stats.underruns), (1, 0), "{stats}");
    assert!(
        stats.drift_corrections >= 1,
        "the surplus is trimmed: {stats}"
    );
    assert!(
        stats.buffer_fill_ms <= 1_100,
        "back within the band: {stats}"
    );
}

/// Feeds the detector blocks `from..to` of a source whose block `k` reaches
/// it at `arrival(k)` ms, `None` if lost; returns every verdict that is not
/// `OnTime`, with the block it came with.
fn verdicts(
    detector: &mut GapDetector,
    from: usize,
    to: usize,
    arrival: impl Fn(usize) -> Option<f64>,
) -> Vec<(usize, GapVerdict)> {
    let mut arrivals: Vec<(f64, usize)> = (from..to)
        .filter_map(|block| arrival(block).map(|time| (time, block)))
        .collect();
    arrivals.sort_by(|a, b| a.0.total_cmp(&b.0));
    arrivals
        .into_iter()
        .filter_map(|(time, block)| {
            let verdict = detector.observe(at(time), PERIOD, 0);
            (verdict != GapVerdict::OnTime).then_some((block, verdict))
        })
        .collect()
}

fn on_time(block: usize) -> Option<f64> {
    Some(block as f64 * block_ms())
}

#[test]
fn a_hole_is_confirmed_after_the_window_with_its_length_and_position() {
    let mut detector = detector();
    // Eight blocks (170 ms) never arrive.
    let found = verdicts(&mut detector, 0, 200, |block| {
        (!(100..108).contains(&block))
            .then(|| on_time(block))
            .flatten()
    });

    // Block 108 is the first after the hole; the suspicion holds for 80 ms and
    // is confirmed with block 112, four blocks after the hole.
    assert_eq!(found.len(), 1, "{found:?}");
    let (block, GapVerdict::Hole { behind, frames }) = found[0] else {
        panic!("expected a hole, got {found:?}");
    };
    assert_eq!(block, 112);
    assert_eq!(behind, 4 * PERIOD);
    // The baseline creeps up by the drift allowance (1 ms per second) meanwhile.
    assert!(frames.abs_diff(8 * PERIOD) <= 16, "{frames}");
}

#[test]
fn a_single_lost_packet_of_21_ms_is_confirmed() {
    let mut detector = detector();
    let found = verdicts(&mut detector, 0, 200, |block| {
        (block != 100).then(|| on_time(block)).flatten()
    });

    assert_eq!(found.len(), 1, "{found:?}");
    let GapVerdict::Hole { behind, frames } = found[0].1 else {
        panic!("expected a hole, got {found:?}");
    };
    assert!(frames.abs_diff(PERIOD) <= 16, "{frames}");
    assert_eq!(behind, 4 * PERIOD);
}

#[test]
fn two_losses_in_quick_succession_are_two_holes() {
    let mut detector = detector();
    let found = verdicts(&mut detector, 0, 300, |block| {
        (block != 100 && block != 102)
            .then(|| on_time(block))
            .flatten()
    });

    let total: usize = found
        .iter()
        .map(|(_, verdict)| match verdict {
            GapVerdict::Hole { frames, .. } => *frames,
            other => panic!("unexpected {other:?}"),
        })
        .sum();
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(total.abs_diff(2 * PERIOD) <= 32, "{total}");
}

#[test]
fn delays_that_grow_and_shrink_are_not_a_hole() {
    // In order, nothing lost: 12 ms, 24 ms, then 6 ms late. The second delay
    // must not settle the first suspicion before its window is over.
    let delays = [12.0, 24.0, 6.0];
    let mut detector = detector();
    for block in 0..120 {
        let delay = match block {
            100..=102 => delays[block - 100],
            _ => 0.0,
        };
        let verdict = detector.observe(at(block as f64 * block_ms() + delay), PERIOD, 0);
        assert_eq!(verdict, GapVerdict::OnTime, "block {block}");
    }
    // Also with the delays piling up, then catching up before the window ends.
    let mut detector = self::detector();
    for block in 0..200 {
        let delay = match block {
            100 => 12.0,
            101 => 24.0,
            102 => 36.0,
            _ => 0.0,
        };
        assert_eq!(
            detector.observe(at(block as f64 * block_ms() + delay), PERIOD, 0),
            GapVerdict::OnTime,
            "block {block}"
        );
    }
}

#[test]
fn a_late_bump_on_top_of_a_real_loss_only_cancels_itself() {
    // One packet is lost; a following block is then 15 ms late once.
    let mut detector = detector();
    let found = verdicts(&mut detector, 0, 300, |block| {
        (block != 100).then(|| block as f64 * block_ms() + if block == 103 { 15.0 } else { 0.0 })
    });

    assert_eq!(found.len(), 1, "{found:?}");
    let GapVerdict::Hole { frames, .. } = found[0].1 else {
        panic!("{found:?}");
    };
    assert!(frames.abs_diff(PERIOD) <= 16, "{frames}");
}

#[test]
fn a_late_packet_is_not_a_hole() {
    // Blocks arrive 21, 30 or 45 ms late, one at a time; the next one is on time.
    for late in [21.0, 30.0, 45.0] {
        let mut detector = detector();
        let found = verdicts(&mut detector, 0, 400, |block| {
            Some(block as f64 * block_ms() + if block % 50 == 25 { late } else { 0.0 })
        });

        assert!(found.is_empty(), "{late} ms late: {found:?}");
    }
}

#[test]
fn audio_that_is_late_and_then_catches_up_in_a_burst_is_not_a_hole() {
    let mut detector = detector();
    // Blocks 100..108 are held back 170 ms and arrive together.
    let found = verdicts(&mut detector, 0, 400, |block| {
        Some(if (100..108).contains(&block) {
            108.0 * block_ms()
        } else {
            block as f64 * block_ms()
        })
    });

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn normal_arrival_jitter_is_not_a_gap() {
    let mut state = 12_345_u32;
    let mut jitter = move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(state >> 8) / f64::from(1_u32 << 24)
    };
    let mut detector = detector();
    // Up to 9 ms late, in no particular order, for ten minutes.
    for block in 0..10 * 60_000 / 21 {
        let time = block as f64 * block_ms() + 9.0 * jitter();
        assert_eq!(
            detector.observe(at(time), PERIOD, 0),
            GapVerdict::OnTime,
            "block {block}"
        );
    }
}

#[test]
fn large_arrival_jitter_that_does_not_last_is_not_a_hole() {
    let mut state = 99_u32;
    let mut jitter = move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(state >> 8) / f64::from(1_u32 << 24)
    };
    let mut detector = detector();
    // Up to 14 ms: now and then over the suspicion threshold, but never for
    // long enough to be confirmed.
    let mut confirmed = 0;
    for block in 0..60_000 / 21 {
        let time = block as f64 * block_ms() + 14.0 * jitter();
        if detector.observe(at(time), PERIOD, 0) != GapVerdict::OnTime {
            confirmed += 1;
        }
    }
    assert!(confirmed <= 1, "{confirmed} false holes in a minute");
}

#[test]
fn bursty_packet_delivery_is_not_a_gap() {
    // A2DP delivers a few ms of audio per packet, every ~20 ms: the capture
    // device returns what is there, in uneven pieces.
    let mut detector = detector();
    let mut delivered = 0;
    for tick in 0..30_000 {
        let time = f64::from(tick) * 2.0;
        let due = (time / 1000.0 * f64::from(RATE)) as usize;
        let packet = if tick % 10 == 9 { due - delivered } else { 0 };
        if packet > 0 {
            delivered += packet;
            assert_eq!(
                detector.observe(at(time), packet, 0),
                GapVerdict::OnTime,
                "tick {tick}"
            );
        }
    }
}

#[test]
fn slow_clock_drift_is_not_a_gap() {
    for ppm in [-300.0_f64, -100.0, -20.0, 20.0, 100.0, 300.0] {
        let mut detector = detector();
        // The source's second is `1 + ppm/1e6` of ours.
        let scale = 1.0 + ppm / 1e6;
        for block in 0..60 * 48_000 / PERIOD {
            let time = block as f64 * block_ms() * scale;
            assert_eq!(
                detector.observe(at(time), PERIOD, 0),
                GapVerdict::OnTime,
                "{ppm} ppm, block {block}"
            );
        }
    }
}

#[test]
fn frames_waiting_in_the_capture_device_are_late_not_lost() {
    let mut detector = detector();
    for block in 0..100 {
        detector.observe(at(block as f64 * block_ms()), PERIOD, 0);
    }
    // The thread overslept 90 ms; the device kept the audio meanwhile.
    let backlog = 4 * PERIOD;
    assert_eq!(
        detector.observe(at(104.0 * block_ms()), PERIOD, backlog),
        GapVerdict::OnTime
    );
}

#[test]
fn a_long_stall_is_a_restart_not_a_loss() {
    let mut detector = detector();
    for block in 0..100 {
        detector.observe(at(block as f64 * block_ms()), PERIOD, 0);
    }
    assert_eq!(
        detector.observe(at((100.0 + 600.0) * block_ms()), PERIOD, 0),
        GapVerdict::Restart
    );
    assert_eq!(
        detector.observe(at(701.0 * block_ms()), PERIOD, 0),
        GapVerdict::OnTime
    );
}

#[test]
fn a_restart_is_spliced_without_a_click_or_a_loss() {
    let mut buffer = new_buffer(2_000);
    let mut clock = 0.0;
    let mut position = 0;
    for _ in 0..40 {
        buffer.push_at(at(clock), &source(position, PERIOD), 0);
        position += PERIOD;
        clock += block_ms();
    }
    clock += 5_000.0;
    position += 234 * PERIOD;
    for _ in 0..20 {
        buffer.push_at(at(clock), &source(position, PERIOD), 0);
        position += PERIOD;
        clock += block_ms();
    }
    let all = drain(&mut buffer);

    assert_continuous(&all);
    assert_eq!(buffer.take_stats().losses, 0);
    // No time was invented (a drained buffer may stretch by 20 ms at the end).
    let played = all.len() / CH;
    assert!(
        (60 * PERIOD..60 * PERIOD + frames(40)).contains(&played),
        "{played}"
    );
}

#[test]
fn a_hole_right_after_a_hole_while_the_crossfade_is_running() {
    let mut buffer = new_buffer(1_000);
    let mut clock = 0.0;
    let mut position = 0;
    let push = |buffer: &mut JitterBuffer, clock: &mut f64, position: &mut usize, count: usize| {
        buffer.push_at(at(*clock), &source(*position, count), 0);
        *position += count;
        *clock += ms(count);
    };
    for _ in 0..40 {
        push(&mut buffer, &mut clock, &mut position, PERIOD);
    }
    for _ in 0..3 {
        // 100 ms lost, then 100 frames (2 ms) of audio: inside the crossfade.
        position += frames(100);
        clock += 100.0;
        push(&mut buffer, &mut clock, &mut position, 100);
    }
    for _ in 0..20 {
        push(&mut buffer, &mut clock, &mut position, PERIOD);
    }
    buffer.start_playout();
    let all = drain(&mut buffer);

    assert_continuous(&all);
    assert_eq!(buffer.take_stats().gaps, 3);
}

#[test]
fn an_xrun_and_the_hole_it_leaves_are_one_loss() {
    let mut buffer = new_buffer(400);
    let mut clock = 0.0;
    for block in 0..40 {
        buffer.push_at(at(clock), &source(block * PERIOD, PERIOD), 0);
        clock += block_ms();
    }
    buffer.note_xrun();
    clock += 100.0;
    // The hole is confirmed a few blocks later.
    for block in 40..60 {
        buffer.push_at(at(clock), &source(block * PERIOD, PERIOD), 0);
        clock += block_ms();
    }

    let stats = buffer.take_stats();
    assert_eq!(
        (stats.xruns, stats.gaps, stats.losses),
        (1, 0, 1),
        "{stats}"
    );
    assert_eq!(stats.concealed, 1, "the hole is still concealed");
}

// ----- zero runs -----

#[test]
fn a_run_of_zeros_inside_music_is_concealed() {
    let mut buffer = new_buffer(400);
    let mut input = source(0, frames(100));
    input.extend(vec![0_i16; frames(20) * CH]);
    input.extend(source(frames(120), frames(100)));
    for chunk in input.chunks(PERIOD * CH) {
        buffer.push(chunk);
    }
    let out = drain(&mut buffer);

    assert!(out.len() >= input.len() - 480 * CH);
    assert_continuous(&out);
    let hole = &out[frames(100) * CH..frames(120) * CH];
    assert!(
        !is_silent(hole),
        "the zeros were replaced by a fading continuation"
    );
    let stats = buffer.take_stats();
    assert_eq!(
        (stats.zero_runs, stats.losses, stats.concealed),
        (1, 1, 1),
        "{stats}"
    );
    assert!(stats.concealed_ms == 20, "{stats}");
}

#[test]
fn a_zero_run_split_over_many_pushes_is_one_event() {
    let mut buffer = new_buffer(1_000);
    buffer.push(&source(0, frames(100)));
    for _ in 0..13 {
        buffer.push(&vec![0_i16; 100 * CH]);
    }
    buffer.push(&source(frames(100) + 1_300, frames(100)));
    let out = drain(&mut buffer);

    assert_continuous(&out);
    let stats = buffer.take_stats();
    assert_eq!((stats.zero_runs, stats.concealed), (1, 1), "{stats}");
}

#[test]
fn a_long_run_of_zeros_ends_in_silence_and_counts_once() {
    let mut buffer = new_buffer(1_000);
    let mut input = source(0, frames(100));
    input.extend(vec![0_i16; frames(300) * CH]);
    input.extend(source(frames(400), frames(100)));
    buffer.push(&input);
    let out = drain(&mut buffer);

    assert_continuous(&out);
    assert!(is_silent(
        &out[(frames(100) + frames(31)) * CH..(frames(400) - frames(1)) * CH]
    ));
    assert_eq!(buffer.take_stats().zero_runs, 1);
}

#[test]
fn silence_after_silence_passes_through_untouched() {
    let mut buffer = new_buffer(1_000);
    let mut input = vec![0_i16; frames(500) * CH];
    input.extend(source(0, frames(100)));
    buffer.push(&input);
    let out = drain(&mut buffer);

    assert_eq!(out[..frames(500) * CH], input[..frames(500) * CH]);
    assert_eq!(buffer.take_stats().losses, 0);
}

#[test]
fn zeros_after_the_music_faded_out_are_just_silence() {
    let mut buffer = new_buffer(1_000);
    // A fade-out to near zero (peak under 32), then digital silence.
    let mut input = source(0, frames(100));
    input.extend((0..frames(10)).flat_map(|n| {
        let value = (20.0 * (1.0 - n as f32 / frames(10) as f32)) as i16;
        [value, value]
    }));
    input.extend(vec![0_i16; frames(300) * CH]);
    buffer.push(&input);
    let out = drain(&mut buffer);

    assert_eq!(buffer.take_stats().losses, 0);
    assert!(out[..frames(100) * CH + frames(10) * CH] == input[..frames(110) * CH]);
    assert!(is_silent(&out[frames(110) * CH..frames(400) * CH]));
}

#[test]
fn short_runs_of_zeros_are_untouched() {
    let mut buffer = new_buffer(1_000);
    let mut input = source(0, frames(100));
    // 1 ms of zeros, and a single zero frame, inside the music.
    input[frames(40) * CH..frames(41) * CH].fill(0);
    input[frames(60) * CH..frames(60) * CH + CH].fill(0);
    buffer.push(&input);
    let out = drain(&mut buffer);

    assert_eq!(out[..], input[..out.len()]);
    assert_eq!(buffer.take_stats().losses, 0);
}

#[test]
fn zeros_in_one_channel_only_are_not_a_run() {
    let mut buffer = new_buffer(1_000);
    let mut input = source(0, frames(300));
    for frame in frames(100)..frames(200) {
        input[frame * CH] = 0;
    }
    buffer.push(&input);
    let out = drain(&mut buffer);

    assert_eq!(out[..], input[..out.len()]);
    assert_eq!(buffer.take_stats().losses, 0);
}

// ----- cushion, capacity, drift -----

#[test]
fn without_a_consumer_only_the_most_recent_target_is_kept() {
    let mut buffer = new_buffer(400);
    let total = frames(5_000);
    let all = source(0, total);
    for chunk in all.chunks(PERIOD * CH) {
        buffer.push(chunk);
        assert!(buffer.fill_frames() <= buffer.target_frames());
    }

    assert_eq!(buffer.fill_frames(), buffer.target_frames());
    let kept = drain(&mut buffer);
    let kept_frames = kept.len() / CH;
    assert!(kept_frames + buffer.chunk_frames() > frames(400));
    assert_eq!(
        kept[..],
        all[(total - kept_frames - buffer.fill_frames()) * CH..][..kept.len()]
    );
}

#[test]
fn a_two_second_buffer_holds_the_most_recent_two_seconds() {
    let mut buffer = new_buffer(2_000);
    let total = frames(7_000);
    let all = source(0, total);
    for chunk in all.chunks(PERIOD * CH) {
        buffer.push(chunk);
    }
    assert_eq!(buffer.target_frames(), frames(2_000));
    assert_eq!(buffer.fill_frames(), frames(2_000));
    assert_eq!(buffer.take_stats().buffer_fill_ms, 2_000);

    // Attaching starts playback straight away, from exactly those 2 s.
    buffer.start_playout();
    let out = drain(&mut buffer);

    assert!(out.len() / CH + PLAYOUT_CHUNK_FRAMES >= frames(2_000));
    assert_continuous(&out);
    // The first second is the most recent audio, unchanged past the 10 ms
    // fade-in. (Draining the buffer dry later stretches it near the end.)
    let start = total - frames(2_000);
    let fade = frames(10);
    let second = frames(1_000);
    assert_eq!(
        out[fade * CH..second * CH],
        all[(start + fade) * CH..(start + second) * CH]
    );
    assert_eq!(buffer.take_stats().losses, 0);
}

#[test]
fn a_two_second_buffer_survives_ten_minutes_of_streaming_without_growing() {
    let mut buffer = new_buffer(2_000);
    buffer.push(&source(0, frames(2_000)));
    buffer.start_playout();
    let mut position = frames(2_000);
    let (mut peak, mut lowest) = (0, usize::MAX);
    let mut out = vec![0_i16; frames(10) * CH];
    for _ in 0..600 * 100 {
        buffer.push(&source(position, frames(10)));
        position += frames(10);
        assert!(buffer.pull(&mut out, true));
        peak = peak.max(buffer.fill_frames());
        lowest = lowest.min(buffer.fill_frames());
    }

    assert!(peak <= 2 * buffer.target_frames(), "peak {} ms", ms(peak));
    // And it does not run down either: three quarters of the target, at least.
    assert!(lowest >= frames(1_500), "lowest {} ms", ms(lowest));
    assert_eq!(buffer.take_stats().losses, 0);
}

#[test]
fn the_capacity_follows_the_configured_size() {
    for jitter in [50, 400, 2_000, 10_000] {
        let mut buffer = new_buffer(jitter);
        buffer.start_playout();
        // Nobody pulls for far longer than the buffer holds.
        for _ in 0..(frames(jitter) * 3 / PERIOD + 10) {
            buffer.push(&source(0, PERIOD));
        }
        assert!(
            buffer.fill_frames() <= 2 * frames(jitter) + frames(200),
            "{jitter} ms: {} ms",
            ms(buffer.fill_frames())
        );
        assert!(buffer.fill_frames() >= frames(jitter), "{jitter} ms");
    }
}

/// Pushes `push` frames per `pull` frames pulled, as a source whose clock
/// runs fast or slow does, for `seconds` of playback.
fn run_drift(
    buffer: &mut JitterBuffer,
    seconds: usize,
    push: usize,
    pull: usize,
) -> (Vec<i16>, usize) {
    let mut position = 0;
    let mut out = Vec::new();
    let mut peak = 0;
    for _ in 0..seconds * 1000 / 10 {
        buffer.push(&source(position, push));
        position += push;
        out.extend(pull_all(buffer, pull));
        peak = peak.max(buffer.fill_frames());
    }
    (out, peak)
}

#[test]
fn a_fast_source_is_trimmed_toward_the_target_without_a_click() {
    let mut buffer = new_buffer(100);
    buffer.push(&source(0, frames(100)));
    buffer.start_playout();
    // Two frames more per 10 ms: 0.4 % fast, +125 ms over a minute.
    let (out, peak) = run_drift(&mut buffer, 60, 482, 480);

    assert_continuous(&out);
    let stats = buffer.take_stats();
    assert!(stats.drift_corrections >= 1, "{stats}");
    assert_eq!(stats.losses, 0, "{stats}");
    assert!(ms(peak) < 230.0, "peak fill {} ms stays bounded", ms(peak));
}

#[test]
fn a_slow_source_is_stretched_toward_the_target_without_a_click() {
    let mut buffer = new_buffer(100);
    buffer.push(&source(0, frames(100)));
    buffer.start_playout();
    // One frame less per 10 ms: 0.2 % slow, -125 ms over a minute.
    let (out, _) = run_drift(&mut buffer, 60, 479, 480);

    assert_continuous(&out);
    let stats = buffer.take_stats();
    assert!(stats.drift_corrections >= 1, "{stats}");
    assert_eq!(stats.underruns, 0, "{stats}");
}

#[test]
fn drift_corrections_are_rare() {
    let mut buffer = new_buffer(400);
    buffer.push(&source(0, frames(400)));
    buffer.start_playout();
    // 100 ppm: ~5 frames per second.
    let mut position = 0;
    let mut credit = 0.0;
    for _ in 0..600 * 100 {
        credit += 480.0 * 1.0001;
        let count = credit as usize;
        credit -= count as f64;
        buffer.push(&source(position, count));
        position += count;
        pull_all(&mut buffer, 480);
    }

    let stats = buffer.take_stats();
    assert!(stats.drift_corrections <= 2, "{stats}");
    assert_eq!(stats.losses, 0, "{stats}");
}

// ----- the startup burst -----

#[test]
fn the_startup_burst_is_not_counted_as_a_loss() {
    let mut pipeline = Pipeline::attached(400, 400);
    let first = pipeline.pushed_frames / PERIOD;
    pipeline.run(20, first, |block| Some(block as f64 * block_ms()));

    assert_continuous(&pipeline.out);
    let stats = pipeline.buffer.take_stats();
    assert_eq!(
        (stats.losses, stats.concealed, stats.concealed_ms),
        (0, 0, 0),
        "{stats}"
    );
    // The output ran in real time, ahead by no more than the lead.
    let played = ms(pipeline.out.len() / CH);
    assert!((played - 20_000.0).abs() < 200.0, "{played} ms");
    assert!(
        stats.buffer_fill_ms >= 250,
        "the cushion is intact: {stats}"
    );
}

#[test]
fn an_outage_longer_than_the_cushion_is_concealed_and_recovers() {
    let mut pipeline = Pipeline::attached(400, 400);
    let first = pipeline.pushed_frames / PERIOD;
    // Two seconds without any audio, then it is back (and was lost meanwhile).
    let lost = first + 100..first + 194;
    pipeline.run(15, first, |block| {
        (!lost.contains(&block)).then(|| block as f64 * block_ms())
    });

    assert_continuous(&pipeline.out);
    let stats = pipeline.buffer.take_stats();
    // The output stage bridged the outage once the cushion ran dry; the hole
    // found at the capture side is the same loss and is not counted twice.
    assert_eq!(
        (stats.underruns, stats.gaps, stats.losses),
        (1, 0, 1),
        "{stats}"
    );
    assert!((1_500..2_200).contains(&stats.concealed_ms), "{stats}");
    assert!(stats.buffer_fill_ms >= 100, "refilled: {stats}");
}

// ----- stats -----

#[test]
fn stats_count_every_kind_of_loss_and_reset_each_minute() {
    let mut buffer = new_buffer(400);
    buffer.start_playout();
    buffer.push(&source(0, frames(400)));
    let mut out = pull_all(&mut buffer, frames(400) + frames(25));
    buffer.note_xrun();
    buffer.note_xrun();
    let mut zeros = source(frames(1_000), frames(100));
    zeros.extend(vec![0_i16; frames(10) * CH]);
    zeros.extend(source(frames(1_110), frames(400)));
    buffer.push(&zeros);
    out.extend(pull_all(&mut buffer, frames(50)));

    let stats = buffer.take_stats();
    assert_eq!(stats.underruns, 1);
    assert_eq!(stats.xruns, 2);
    assert_eq!(stats.zero_runs, 1);
    assert_eq!(stats.gaps, 0);
    assert_eq!(stats.losses, 4);
    assert_eq!(stats.concealed, 2, "the underrun and the zero run");
    assert_eq!(stats.concealed_ms, 25 + 10);
    assert_eq!(stats.buffer_fill_ms, ms(buffer.fill_frames()) as u64);

    let next = buffer.take_stats();
    assert_eq!((next.losses, next.concealed, next.concealed_ms), (0, 0, 0));
    assert_eq!(
        next.buffer_fill_ms, stats.buffer_fill_ms,
        "the level is current"
    );
}

#[test]
fn the_stats_line_has_the_greppable_shape() {
    let mut buffer = new_buffer(400);
    buffer.push(&source(0, frames(250)));
    let line = buffer.take_stats().to_string();

    assert_eq!(
        line,
        "bt_input stats: losses_per_min=0 concealed_per_min=0 concealed_ms=0 buffer_fill_ms=250 \
         underruns=0 zero_runs=0 xruns=0 gaps=0 drift_corrections=0 reads=0 avail_errors=0 splices=0"
    );
}

// ----- pacing -----

#[test]
fn the_pacer_allows_a_lead_then_follows_real_time() {
    let mut pacer = Pacer::new(RATE, Duration::from_millis(100));
    let chunk = frames(10);

    // The initial burst: real audio only, up to the lead.
    let mut burst = 0;
    while pacer.due(at(0.0)) == Due::Optional {
        pacer.sent(chunk);
        burst += 1;
    }
    assert_eq!(burst, 10);
    assert_eq!(pacer.due(at(0.0)), Due::Wait);

    // Then one chunk per 10 ms.
    assert_eq!(pacer.due(at(10.0)), Due::Optional);
    pacer.sent(chunk);
    assert_eq!(pacer.due(at(10.0)), Due::Wait);

    // Only once the output is behind real time is audio required.
    assert_eq!(pacer.due(at(110.0)), Due::Optional);
    assert_eq!(pacer.due(at(110.5)), Due::Required);
}

#[test]
fn a_blocked_sender_does_not_make_the_pacer_burst_later() {
    let mut pacer = Pacer::new(RATE, Duration::from_millis(100));
    assert_eq!(pacer.due(at(0.0)), Due::Optional);
    // The sender's queue stays full for two seconds.
    for step in 0..200 {
        pacer.blocked(at(f64::from(step) * 10.0));
    }
    let mut burst = 0;
    while pacer.due(at(2_000.0)) == Due::Optional {
        pacer.sent(frames(10));
        burst += 1;
    }
    assert_eq!(burst, 10, "no more than the lead");
}

// ----- chunks for the AirPlay resampler -----

/// What the AirPlay live decoder does with every frame it receives
/// (`LiveAudioDecoder::decode_resampled`): resample it on its own with a
/// fixed-input resampler, which fails on anything but exactly 1024 frames.
#[track_caller]
fn assert_every_chunk_resamples(out: &[i16], channels: usize) {
    use airplay_resampler::{DEFAULT_CHUNK_SIZE, Resampler};

    assert!(!out.is_empty());
    assert_eq!(PLAYOUT_CHUNK_FRAMES, DEFAULT_CHUNK_SIZE);
    let chunk_samples = PLAYOUT_CHUNK_FRAMES * channels;
    assert_eq!(out.len() % chunk_samples, 0, "only whole chunks go out");
    let mut resampler = Resampler::new(RATE, 44_100, channels as u8).unwrap();
    for (index, chunk) in out.chunks(chunk_samples).enumerate() {
        assert_eq!(chunk.len(), chunk_samples);
        let resampled = resampler
            .process(chunk)
            .unwrap_or_else(|error| panic!("chunk {index}: {error}"));
        assert!(!resampled.is_empty(), "chunk {index}");
    }
}

#[test]
fn playout_chunks_are_what_the_airplay_resampler_expects() {
    let buffer = new_buffer(400);

    assert_eq!(buffer.chunk_frames(), 1024);
    assert_eq!(PLAYOUT_CHUNK_FRAMES, 1024);
}

#[test]
fn the_resampler_takes_every_chunk_in_normal_flow_and_at_start_up() {
    let mut pipeline = Pipeline::attached(400, 400);
    let first = pipeline.pushed_frames / PERIOD;
    pipeline.run(20, first, |block| Some(block as f64 * block_ms()));

    assert_eq!(pipeline.buffer.take_stats().losses, 0);
    assert_every_chunk_resamples(&pipeline.out, CH);
}

#[test]
fn the_resampler_takes_every_chunk_when_starting_from_nothing() {
    // The speaker is acquired before any audio is queued: silence, then fade-in.
    let mut pipeline = Pipeline::attached(400, 0);
    pipeline.run(10, 0, |block| Some(block as f64 * block_ms() + 50.0));

    assert_continuous(&pipeline.out);
    assert_every_chunk_resamples(&pipeline.out, CH);
}

#[test]
fn the_resampler_takes_every_chunk_through_a_170_ms_hole() {
    let mut pipeline = Pipeline::attached(400, 400);
    let first = pipeline.pushed_frames / PERIOD;
    let lost = first + 40..first + 48;
    pipeline.run(10, first, |block| {
        (!lost.contains(&block)).then(|| block as f64 * block_ms())
    });

    assert_eq!(pipeline.buffer.take_stats().gaps, 1);
    assert_every_chunk_resamples(&pipeline.out, CH);
}

#[test]
fn the_resampler_takes_every_chunk_through_underruns() {
    let mut pipeline = Pipeline::attached(400, 400);
    let first = pipeline.pushed_frames / PERIOD;
    let lost = first + 100..first + 194;
    pipeline.run(15, first, |block| {
        (!lost.contains(&block)).then(|| block as f64 * block_ms())
    });

    assert_eq!(pipeline.buffer.take_stats().underruns, 1);
    assert_every_chunk_resamples(&pipeline.out, CH);
}

#[test]
fn the_resampler_takes_every_chunk_through_drift_trims_and_inserts() {
    // A source clock 0.5 % fast (the cushion grows, then is trimmed), and
    // 0.07 % slow (it shrinks, then is stretched).
    for (scale, seconds, label) in [(0.995, 40, "fast"), (1.0007, 100, "slow")] {
        let mut pipeline = Pipeline::attached(100, 100);
        let first = pipeline.pushed_frames / PERIOD;
        let start = first as f64 * block_ms();
        pipeline.run(seconds, first, |block| {
            Some(start + (block - first) as f64 * block_ms() * scale)
        });

        let stats = pipeline.buffer.take_stats();
        assert!(stats.drift_corrections >= 1, "{label}: {stats}");
        assert_eq!(stats.underruns, 0, "{label}: {stats}");
        assert_every_chunk_resamples(&pipeline.out, CH);
    }
}

#[test]
fn chunks_keep_their_size_whatever_the_capture_pushes() {
    // Capture periods, partial reads and the 32-bit path all end up as pushes
    // of any size; the output does not care.
    let mut buffer = new_buffer(400);
    let mut pacer = buffer.pacer();
    let mut position = 0;
    let mut out = Vec::new();
    let mut chunk = vec![0_i16; PLAYOUT_CHUNK_FRAMES * CH];
    let sizes = [1024, 333, 512, 1, 2048, 97, 1024, 750];
    for step in 0..6_000 {
        let size = sizes[step % sizes.len()];
        let now = at(position as f64 * 1000.0 / f64::from(RATE));
        buffer.push_at(now, &source(position, size), 0);
        position += size;
        while buffer.pull_chunk(&mut pacer, now, &mut chunk) {
            pacer.sent(PLAYOUT_CHUNK_FRAMES);
            out.extend_from_slice(&chunk);
        }
    }

    assert_every_chunk_resamples(&out, CH);
}

#[test]
fn mono_chunks_are_also_1024_frames() {
    let mut buffer = JitterBuffer::new(RATE, 1, 400);
    let mut pacer = buffer.pacer();
    let mono: Vec<i16> = (0..frames(2_000))
        .map(|frame| ((frame as f32 * 0.13).sin() * 20_000.0) as i16)
        .collect();
    buffer.push(&mono[..frames(500)]);
    buffer.start_playout();
    let mut out = Vec::new();
    let mut chunk = vec![0_i16; PLAYOUT_CHUNK_FRAMES];
    let mut clock = 0.0;
    let mut next = frames(500);
    while clock < 1_500.0 {
        clock += 1.0;
        let due = (frames(500) as f64 + clock * f64::from(RATE) / 1000.0) as usize;
        if due > next {
            buffer.push(&mono[next..due]);
            next = due;
        }
        while buffer.pull_chunk(&mut pacer, at(clock), &mut chunk) {
            pacer.sent(PLAYOUT_CHUNK_FRAMES);
            out.extend_from_slice(&chunk);
        }
    }

    assert_every_chunk_resamples(&out, 1);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "fixed size")]
fn a_chunk_of_another_size_trips_the_debug_assertion() {
    let mut buffer = new_buffer(400);
    let mut pacer = buffer.pacer();
    let mut chunk = vec![0_i16; 480 * CH];

    buffer.pull_chunk(&mut pacer, at(0.0), &mut chunk);
}

#[cfg(not(debug_assertions))]
#[test]
fn a_chunk_of_another_size_is_refused() {
    let mut buffer = new_buffer(400);
    buffer.push(&source(0, frames(400)));
    let mut pacer = buffer.pacer();
    let mut chunk = vec![0_i16; 480 * CH];

    assert!(!buffer.pull_chunk(&mut pacer, at(0.0), &mut chunk));
    assert_eq!(buffer.fill_frames(), frames(400));
}

#[test]
fn small_cushions_work_with_whole_chunks() {
    // The chunk (21.3 ms) is coarse next to the cushion: even the lowest
    // accepted value (50 ms) streams without losses.
    for jitter in [50, 60, 75, 100, 150, 400, 2_000] {
        let mut pipeline = Pipeline::attached(jitter, jitter);
        let first = pipeline.pushed_frames / PERIOD;
        pipeline.run(30, first, |block| Some(block as f64 * block_ms()));

        let stats = pipeline.buffer.take_stats();
        assert_eq!(
            (stats.losses, stats.drift_corrections),
            (0, 0),
            "{jitter} ms: {stats}"
        );
        assert_continuous(&pipeline.out);
    }
}

// ----- single lost packets -----

/// A mono tone on both channels, for a low frequency whose natural slope is small.
fn tone(start: usize, count: usize, hz: f32) -> Vec<i16> {
    let mut samples = Vec::with_capacity(count * CH);
    for frame in start..start + count {
        let phase = std::f32::consts::TAU * hz * frame as f32 / RATE as f32;
        let value = (phase.sin() * AMPLITUDE) as i16;
        samples.extend([value; CH]);
    }
    samples
}

#[test]
fn a_single_lost_packet_is_concealed_without_a_click() {
    // One lost 1024-frame block (21.3 ms) from a 173 Hz sine: spliced as it
    // is, the step is 48,000 counts against a natural maximum of 670. The
    // cushion is big enough to still hold the hole when it is inspected.
    let hz = 173.0;
    let natural = natural_step(hz);
    let mut buffer = new_buffer(2_000);
    let mut clock = 0.0;
    let mut position = 0;
    let mut pushed = 0;
    for block in 0..80 {
        if block == 20 {
            position += PERIOD;
            clock += block_ms();
        }
        buffer.push_at(at(clock), &tone(position, PERIOD, hz), 0);
        position += PERIOD;
        pushed += PERIOD;
        clock += block_ms();
    }
    assert!(
        buffer.fill_frames() >= pushed,
        "nothing may have left the FIFO"
    );
    let stats = buffer.take_stats();
    let out = drain(&mut buffer);

    // The hole was found, counted and concealed in the audio that is checked.
    assert_eq!(
        (stats.gaps, stats.losses, stats.concealed),
        (1, 1, 1),
        "{stats}"
    );
    assert!((19..=22).contains(&stats.concealed_ms), "{stats}");
    let concealed = pushed + frames(21);
    assert!(out.len() / CH + PLAYOUT_CHUNK_FRAMES >= concealed.min(buffer_total(&out)));
    for channel in 0..CH {
        let step = max_step(&out, channel);
        assert!(step <= STEP_MARGIN * natural, "step {step} vs {natural}");
    }
    // The audio at the hole is not the bare source any more: the sine's samples
    // there are the concealment, faded, not the lost block's neighbours.
    let at_hole = &out[(20 * PERIOD) * CH..(20 * PERIOD + frames(15)) * CH];
    let source_there = tone(20 * PERIOD, frames(15), hz);
    assert_ne!(at_hole, &source_there[..]);
}

fn buffer_total(out: &[i16]) -> usize {
    out.len() / CH
}

#[test]
fn a_hole_inside_the_queue_of_a_cold_start_is_concealed() {
    // The consumer attaches while the cushion is still filling: the output is
    // silence, and the audio queueing up behind it holds a lost packet. That
    // hole has not been bridged by anything and must be concealed.
    let hz = 173.0;
    let natural = natural_step(hz);
    let mut cold = new_buffer(400);
    cold.start_playout();
    let mut pacer = cold.pacer();
    let mut chunk = vec![0_i16; PLAYOUT_CHUNK_FRAMES * CH];
    let mut audio = Vec::new();
    let mut next = 0;
    for tick in 0..2_000 {
        let now = f64::from(tick);
        while next as f64 * block_ms() <= now {
            if next != 5 {
                cold.push_at(
                    at(next as f64 * block_ms()),
                    &tone(next * PERIOD, PERIOD, hz),
                    0,
                );
            }
            next += 1;
        }
        while cold.pull_chunk(&mut pacer, at(now), &mut chunk) {
            pacer.sent(PLAYOUT_CHUNK_FRAMES);
            audio.extend_from_slice(&chunk);
        }
    }

    for channel in 0..CH {
        let step = max_step(&audio, channel);
        assert!(step <= STEP_MARGIN * natural, "step {step} vs {natural}");
    }
    let stats = cold.take_stats();
    assert_eq!(
        (stats.gaps, stats.concealed, stats.underruns),
        (1, 1, 0),
        "{stats}"
    );
}

#[test]
fn without_concealment_the_lost_packet_would_click() {
    // The control of the test above: the bare splice.
    let hz = 173.0;
    let mut spliced = tone(0, 40 * PERIOD, hz);
    spliced.extend(tone(41 * PERIOD, 40 * PERIOD, hz));

    assert!(max_step(&spliced, 0) > 20.0 * natural_step(hz));
}

#[test]
fn single_lost_packets_through_the_playout_are_concealed() {
    let mut pipeline = Pipeline::attached(400, 400);
    let first = pipeline.pushed_frames / PERIOD;
    let lost = [first + 40, first + 140, first + 300];
    pipeline.run(20, first, |block| {
        (!lost.contains(&block)).then(|| block as f64 * block_ms())
    });

    assert_continuous(&pipeline.out);
    let stats = pipeline.buffer.take_stats();
    assert_eq!((stats.gaps, stats.underruns), (3, 0), "{stats}");
    assert_every_chunk_resamples(&pipeline.out, CH);
}

#[test]
fn ordinary_arrival_jitter_conceals_nothing() {
    let mut state = 4_242_u32;
    let mut jitter = move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(state >> 8) / f64::from(1_u32 << 24)
    };
    let noise: Vec<f64> = (0..4_000).map(|_| 9.0 * jitter()).collect();
    let mut pipeline = Pipeline::attached(400, 400);
    let first = pipeline.pushed_frames / PERIOD;
    pipeline.run(60, first, |block| {
        Some(block as f64 * block_ms() + noise[block % noise.len()])
    });

    let stats = pipeline.buffer.take_stats();
    assert_eq!(
        (stats.gaps, stats.concealed_ms, stats.drift_corrections),
        (0, 0, 0),
        "{stats}"
    );
    assert_continuous(&pipeline.out);
}

#[test]
fn a_hole_that_already_played_is_counted_but_not_concealed() {
    // With a 50 ms cushion the audio after a hole is played before the hole
    // is confirmed (80 ms): nothing can be bridged any more.
    let mut pipeline = Pipeline::attached(50, 50);
    let first = pipeline.pushed_frames / PERIOD;
    pipeline.run(10, first, |block| {
        (block != first + 100).then(|| block as f64 * block_ms())
    });

    let stats = pipeline.buffer.take_stats();
    assert_eq!(stats.gaps, 1, "{stats}");
    assert_eq!(stats.concealed_ms, 0, "{stats}");
    assert_every_chunk_resamples(&pipeline.out, CH);
}

// ----- the cushion -----

/// The fill stays between three quarters of the target and the high watermark
/// once the cushion is built, with nothing to correct.
#[track_caller]
fn assert_cushion_held(pipeline: &Pipeline, jitter_ms: u32, after_ms: f64) {
    let low = frames(jitter_ms) * 3 / 4;
    let high = frames(jitter_ms) + frames(100);
    let settled: Vec<usize> = pipeline
        .fills
        .iter()
        .filter(|(time, _)| *time >= after_ms)
        .map(|&(_, fill)| fill)
        .collect();
    assert!(settled.len() > 100);
    let (min, max) = (
        *settled.iter().min().unwrap(),
        *settled.iter().max().unwrap(),
    );
    assert!(
        min >= low && max <= high,
        "{jitter_ms} ms: fill {} to {} ms, allowed {} to {} ms",
        ms(min),
        ms(max),
        ms(low),
        ms(high)
    );
}

#[test]
fn a_cold_start_builds_the_whole_cushion_and_keeps_it() {
    for jitter in [400, 2_000] {
        // Attached with only half the cushion queued.
        let mut pipeline = Pipeline::attached(jitter, jitter / 2);
        let first = pipeline.pushed_frames / PERIOD;
        pipeline.run(120, first, |block| Some(block as f64 * block_ms()));

        assert_cushion_held(&pipeline, jitter, f64::from(jitter) + 3_000.0);
        let stats = pipeline.buffer.take_stats();
        assert_eq!(
            (stats.underruns, stats.gaps, stats.drift_corrections),
            (0, 0, 0),
            "{jitter} ms: {stats}"
        );
        assert_continuous(&pipeline.out);
    }
}

#[test]
fn an_attached_full_cushion_keeps_its_level() {
    for jitter in [400, 2_000] {
        let mut pipeline = Pipeline::attached(jitter, jitter);
        let first = pipeline.pushed_frames / PERIOD;
        pipeline.run(120, first, |block| Some(block as f64 * block_ms()));

        assert_cushion_held(&pipeline, jitter, 3_000.0);
        assert_eq!(pipeline.buffer.take_stats().losses, 0);
    }
}

#[test]
fn after_an_outage_the_cushion_is_back_at_the_target() {
    let mut pipeline = Pipeline::attached(400, 400);
    let first = pipeline.pushed_frames / PERIOD;
    // 1.5 s without audio: longer than the cushion.
    let lost = first + 100..first + 170;
    pipeline.run(60, first, |block| {
        (!lost.contains(&block)).then(|| block as f64 * block_ms())
    });

    assert_eq!(pipeline.buffer.take_stats().underruns, 1);
    assert_continuous(&pipeline.out);
    assert_cushion_held(&pipeline, 400, 15_000.0);
}

// ----- round 3 -----

/// Source blocks reach the buffer in real time (`lost` ones never do) and the
/// output is attached at `attach_ms` and paced like the playout thread. A
/// 173 Hz tone, so a splice is a step far above the natural slope. Returns the
/// audio that went out, and the buffer.
fn paced_tone(
    jitter_ms: u32,
    attach_ms: u32,
    seconds: u32,
    lost: impl Fn(usize) -> bool,
) -> (Vec<i16>, JitterBuffer) {
    let hz = 173.0;
    let mut buffer = new_buffer(jitter_ms);
    let mut pacer = None;
    let mut chunk = vec![0_i16; PLAYOUT_CHUNK_FRAMES * CH];
    let mut out = Vec::new();
    let mut next = 0;
    for tick in 0..seconds * 1_000 {
        let now = f64::from(tick);
        while next as f64 * block_ms() <= now {
            if !lost(next) {
                buffer.push_at(
                    at(next as f64 * block_ms()),
                    &tone(next * PERIOD, PERIOD, hz),
                    0,
                );
            }
            next += 1;
        }
        if tick == attach_ms {
            buffer.start_playout();
            pacer = Some(buffer.pacer());
        }
        if let Some(pacer) = pacer.as_mut() {
            while buffer.pull_chunk(pacer, at(now), &mut chunk) {
                pacer.sent(PLAYOUT_CHUNK_FRAMES);
                out.extend_from_slice(&chunk);
            }
        }
    }
    (out, buffer)
}

#[track_caller]
fn assert_tone_continuous(out: &[i16]) {
    let natural = natural_step(173.0);
    for channel in 0..CH {
        let step = max_step(out, channel);
        assert!(step <= STEP_MARGIN * natural, "step {step} vs {natural}");
    }
}

#[test]
fn a_loss_right_after_an_outage_recovery_is_its_own_loss() {
    // 400 ms cushion, attached at 1 s. Blocks 70-99 never arrive (an outage
    // that dries the output up), 100 and 101 do, block 102 is lost again, and
    // the stream carries on. The second hole is only two blocks past the place
    // where the output ran dry, and sits in the audio queued for the refill.
    let (out, mut buffer) = paced_tone(400, 1_000, 9, |block| {
        (70..100).contains(&block) || block == 102
    });

    assert_tone_continuous(&out);
    let stats = buffer.take_stats();
    assert_eq!((stats.underruns, stats.gaps), (1, 1), "{stats}");
    assert_eq!(stats.losses, 2, "{stats}");
    assert!(stats.concealed >= 2, "{stats}");
}

#[test]
fn the_outage_itself_is_still_counted_once() {
    let (out, mut buffer) = paced_tone(400, 1_000, 9, |block| (70..100).contains(&block));

    assert_tone_continuous(&out);
    let stats = buffer.take_stats();
    assert_eq!(
        (stats.underruns, stats.gaps, stats.losses),
        (1, 0, 1),
        "{stats}"
    );
}

#[test]
fn a_single_lost_packet_is_concealed_at_every_accepted_cushion() {
    // Paced output, attached and settled, one packet lost: no step above the
    // tone's own slope, at the minimum cushion and above.
    for jitter in [MIN_JITTER_BUFFER_MS, 400, 2_000] {
        let attach = jitter + 500;
        let loss_block = (attach as f64 / block_ms()) as usize + 150;
        let (out, mut buffer) = paced_tone(jitter, attach, attach / 1_000 + 10, |block| {
            block == loss_block
        });

        assert_tone_continuous(&out);
        let stats = buffer.take_stats();
        assert_eq!(
            (stats.gaps, stats.underruns),
            (1, 0),
            "{jitter} ms: {stats}"
        );
        assert!(
            (19..=22).contains(&stats.concealed_ms),
            "{jitter} ms: {stats}"
        );
    }
}

#[test]
fn the_minimum_cushion_outlasts_the_hole_confirmation() {
    // The numbers the constant in the buffer is derived from, spelled out
    // here for a 44.1 kHz stream (24 ms chunks), the slowest ordinary rate.
    let chunk_ms = (PLAYOUT_CHUNK_FRAMES as u32 * 1000).div_ceil(44_100);
    let window_and_lead = 80 + 50;
    assert!(MIN_JITTER_BUFFER_MS >= window_and_lead + 2 * chunk_ms + 20);
    assert_eq!(
        *auto_airplay::config::JITTER_BUFFER_MS_RANGE.start(),
        MIN_JITTER_BUFFER_MS
    );
    assert_eq!(MIN_JITTER_BUFFER_MS, 200);
}

// ----- round 4 -----

/// An outage that dries the output up (blocks 70-99 never arrive), then
/// `recovered` frames of audio, then a loss of 1024 frames, then the stream
/// carries on. 173 Hz tone, 400 ms cushion, attached at 1 s, paced output.
/// `xrun` reports a capture overrun at the end of the outage.
fn outage_then_loss(recovered: usize, xrun: bool) -> (Vec<i16>, JitterBuffer) {
    outage_then_loss_with(400, recovered, xrun, 0.0)
}

/// [`outage_then_loss`] for any cushion, with the first two blocks after the
/// outage arriving `recovery_jitter_ms` late.
fn outage_then_loss_with(
    cushion_ms: u32,
    recovered: usize,
    xrun: bool,
    recovery_jitter_ms: f64,
) -> (Vec<i16>, JitterBuffer) {
    let hz = 173.0;
    let hole_start = 70 * PERIOD;
    let recovery = 100 * PERIOD;
    let second = recovery + recovered;
    let mut events: Vec<(f64, usize, usize)> = Vec::new();
    let mut start = 0;
    while start < RATE as usize * 7 {
        if start == hole_start {
            start = recovery;
            continue;
        }
        if start == second {
            start += PERIOD;
            continue;
        }
        let limit = if start < hole_start {
            hole_start
        } else if start < second {
            second
        } else {
            usize::MAX
        };
        let end = (start + PERIOD).min(limit);
        let late = if start == recovery || start == recovery + PERIOD {
            recovery_jitter_ms
        } else {
            0.0
        };
        events.push((
            end as f64 * 1000.0 / f64::from(RATE) + late,
            start,
            end - start,
        ));
        start = end;
    }

    let mut buffer = new_buffer(cushion_ms);
    let mut pacer = buffer.pacer();
    let mut chunk = vec![0_i16; PLAYOUT_CHUNK_FRAMES * CH];
    let mut out = Vec::new();
    let (mut next, mut marked) = (0, false);
    for tick in 0..6_800 {
        let now = f64::from(tick);
        while next < events.len() && events[next].0 <= now {
            let (time, start, count) = events[next];
            if xrun && !marked && start >= recovery {
                buffer.note_xrun();
                marked = true;
            }
            buffer.push_at(at(time), &tone(start, count, hz), 0);
            next += 1;
        }
        if tick == 1_000 {
            buffer.start_playout();
        }
        if tick >= 1_000 {
            while buffer.pull_chunk(&mut pacer, at(now), &mut chunk) {
                pacer.sent(PLAYOUT_CHUNK_FRAMES);
                out.extend_from_slice(&chunk);
            }
        }
    }
    (out, buffer)
}

#[test]
fn a_mirror_during_a_refill_is_built_from_one_signal() {
    // 128 frames arrive after the outage, then 1024 are lost: the output is
    // still faded silence, the queue holds a few ms of real audio. Mirroring
    // those 128 frames together with the output's silence is a 6,334 count
    // step (natural maximum 657).
    let (out, mut buffer) = outage_then_loss(128, false);

    assert_tone_continuous(&out);
    let stats = buffer.take_stats();
    assert_eq!(
        (stats.underruns, stats.gaps, stats.losses),
        (1, 1, 2),
        "{stats}"
    );
}

#[test]
fn a_loss_right_after_the_recovery_is_its_own_loss_to_the_frame() {
    // However little arrives between the outage and the next loss, the next
    // loss is a loss of its own, found, concealed and counted. (63, 64 and 65
    // frames were where a tolerance window used to decide.)
    for recovered in [1, 2, 32, 63, 64, 65, 128, 1_024, 2_048, 3_072, 4_096] {
        let (out, mut buffer) = outage_then_loss(recovered, false);

        assert_tone_continuous(&out);
        let stats = buffer.take_stats();
        assert_eq!(
            (stats.underruns, stats.gaps, stats.losses),
            (1, 1, 2),
            "{recovered} frames: {stats}"
        );
        assert!(stats.concealed >= 2, "{recovered} frames: {stats}");
    }
}

#[test]
fn a_loss_right_after_an_overrun_is_its_own_loss_to_the_frame() {
    for recovered in [1, 2, 63, 64, 65, 1_024, 2_048] {
        let (out, mut buffer) = outage_then_loss(recovered, true);

        assert_tone_continuous(&out);
        let stats = buffer.take_stats();
        assert_eq!(
            (stats.xruns, stats.underruns, stats.gaps),
            (1, 1, 1),
            "{recovered} frames: {stats}"
        );
    }
}

#[test]
fn a_loss_straight_after_the_outage_with_nothing_between_is_one_loss() {
    // Nothing at all arrives between the outage and the next loss: the two
    // holes are one, and the underrun already counted it.
    let (out, mut buffer) = outage_then_loss(0, false);

    assert_tone_continuous(&out);
    let stats = buffer.take_stats();
    assert_eq!((stats.underruns, stats.gaps), (1, 0), "{stats}");
}

#[test]
fn persistent_lateness_just_above_10_ms_is_not_a_hole() {
    // Field data (a silent turntable on the Pi): arrivals about 10.5 ms late
    // for longer than the 80 ms window were taken for lost packets, four a
    // minute. A lost packet is about 19 ms.
    for late in [10.0, 10.5, 12.0, 14.0] {
        let mut detector = detector();
        let found = verdicts(&mut detector, 0, 600, |block| {
            Some(
                block as f64 * block_ms()
                    + if (100..140).contains(&(block % 150)) {
                        late
                    } else {
                        0.0
                    },
            )
        });
        assert!(found.is_empty(), "{late} ms late: {found:?}");
    }
}

#[test]
fn a_flickering_10_to_12_ms_lateness_through_the_buffer_conceals_nothing() {
    let mut state = 77_u32;
    let mut noise = move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(state >> 8) / f64::from(1_u32 << 24)
    };
    let lateness: Vec<f64> = (0..8_000).map(|_| 10.0 + 2.0 * noise()).collect();
    let mut pipeline = Pipeline::attached(400, 400);
    let first = pipeline.pushed_frames / PERIOD;
    pipeline.run(120, first, |block| {
        // Late by 10 to 12 ms, all the time.
        Some(block as f64 * block_ms() + lateness[block % lateness.len()])
    });

    let stats = pipeline.buffer.take_stats();
    assert_eq!((stats.gaps, stats.concealed_ms), (0, 0), "{stats}");
}

#[test]
fn a_19_ms_packet_is_still_confirmed_as_a_loss() {
    // SBC at 48 kHz: a packet is about 19 ms (912 frames).
    let packet = 912;
    let mut detector = detector();
    let mut found = Vec::new();
    for index in 0..200_usize {
        if index == 100 {
            continue;
        }
        let time = index as f64 * ms(packet);
        let verdict = detector.observe(at(time), packet, 0);
        if verdict != GapVerdict::OnTime {
            found.push(verdict);
        }
    }
    assert_eq!(found.len(), 1, "{found:?}");
    let GapVerdict::Hole { frames, .. } = found[0] else {
        panic!("{found:?}");
    };
    assert!(frames.abs_diff(packet) <= 16, "{frames}");
}

// ----- round 5 -----

#[test]
fn a_lost_packet_is_found_behind_the_jitter_of_the_recovery() {
    // Blocks 70-99 are lost, two recovery blocks arrive late by some jitter,
    // the next 1024-frame block (a whole packet) is lost, delivery is punctual
    // again. The jitter of the recovery inflates the lowest excess of the first
    // suspicion, so the second hole shows up smaller than it is: 12 ms after
    // 9 ms of jitter, 7 ms after 14 ms.
    for cushion in [200, 400, 2_000] {
        for jitter in [0.0, 5.0, 9.0, 14.0] {
            let (out, mut buffer) = outage_then_loss_with(cushion, 2_048, false, jitter);

            assert_tone_continuous(&out);
            let stats = buffer.take_stats();
            // Two losses. A cushion that is still filling when the outage hits
            // (2 s: not complete at 1.5 s) never underruns, so it finds both
            // holes at the capture side; the others run dry on the first.
            assert_eq!(
                stats.losses, 2,
                "cushion {cushion} ms, jitter {jitter} ms: {stats}"
            );
            assert_eq!(
                stats.underruns,
                u64::from(cushion < 2_000),
                "cushion {cushion} ms, jitter {jitter} ms: {stats}"
            );
            assert!(stats.concealed >= 2, "{cushion}/{jitter}: {stats}");
        }
    }
}

#[test]
fn the_lowest_level_still_needs_the_full_gate() {
    // Stacked suspicions have a lower gate, but only on top of a suspicion
    // that is already open: persistent 10 to 14 ms lateness on its own, with
    // no loss anywhere near, is still no hole.
    let mut detector = detector();
    let found = verdicts(&mut detector, 0, 900, |block| {
        Some(
            block as f64 * block_ms()
                + if block % 100 >= 20 && block % 100 < 60 {
                    12.0
                } else {
                    0.0
                },
        )
    });
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn lateness_on_top_of_a_real_loss_costs_at_most_a_small_hole() {
    // A real loss, and while it is checked a stretch of 7 ms lateness (below
    // the stacked gate): the loss is found at its own size, nothing more.
    let mut detector = detector();
    let found = verdicts(&mut detector, 0, 400, |block| {
        let late = if (101..110).contains(&block) {
            7.0
        } else {
            0.0
        };
        (block != 100).then_some(block as f64 * block_ms() + late)
    });
    assert_eq!(found.len(), 1, "{found:?}");
}

// ----- round 6: the safety net and the burst context -----

/// 173 Hz at 29,000, phase +1 rad: the reviewer's replay of a spurious hole
/// masking a real one. Blocks 70-99 are lost, 100 is punctual, 101-106 are
/// `late_ms` late, `loss` is lost, delivery is punctual from the next.
fn replay_round6(cushion_ms: u32, late_ms: f64, loss: usize) -> (Vec<i16>, JitterBuffer) {
    let tone = |start: usize, count: usize| -> Vec<i16> {
        (start..start + count)
            .flat_map(|frame| {
                let angle = std::f64::consts::TAU * 173.0 * frame as f64 / f64::from(RATE) + 1.0;
                let value = (29_000.0 * angle.sin()) as i16;
                [value, value]
            })
            .collect()
    };
    let mut events = Vec::new();
    for block in 0..330_usize {
        if (70..100).contains(&block) || block == loss {
            continue;
        }
        let late = if (101..107).contains(&block) {
            late_ms
        } else {
            0.0
        };
        events.push(((block + 1) as f64 * block_ms() + late, block * PERIOD));
    }
    let mut buffer = new_buffer(cushion_ms);
    let mut pacer = buffer.pacer();
    let mut chunk = vec![0_i16; PLAYOUT_CHUNK_FRAMES * CH];
    let mut out = Vec::new();
    let mut next = 0;
    for tick in 0..6_800 {
        let now = f64::from(tick);
        while next < events.len() && events[next].0 <= now {
            let (time, start) = events[next];
            buffer.push_at(at(time), &tone(start, PERIOD), 0);
            next += 1;
        }
        if tick == 1_000 {
            buffer.start_playout();
        }
        if tick >= 1_000 {
            while buffer.pull_chunk(&mut pacer, at(now), &mut chunk) {
                pacer.sent(PLAYOUT_CHUNK_FRAMES);
                out.extend_from_slice(&chunk);
            }
        }
    }
    (out, buffer)
}

#[test]
fn a_spurious_hole_does_not_hide_the_next_real_loss() {
    // 6 blocks 6 to 14 ms late are taken for a (small) hole once the first
    // suspicion settles; the real lost packet after them must still be found.
    for cushion in [200, 400, 2_000] {
        for late in [0.0, 6.0, 7.0, 9.0, 10.0, 12.0, 14.0] {
            let (out, mut buffer) = replay_round6(cushion, late, 107);

            assert_tone_continuous(&out);
            let stats = buffer.take_stats();
            assert_eq!(
                stats.splices, 0,
                "the timing check, not the safety net, finds it: cushion {cushion} ms, {late} ms late: {stats}"
            );
        }
    }
}

/// Pushes `blocks` (each `PERIOD` frames of the 173 Hz tone) with the given
/// source blocks missing, without any arrival times: nothing but the waveform
/// can tell.
fn blind_stream(missing: &[usize], blocks: usize) -> (Vec<i16>, JitterBuffer) {
    let mut buffer = new_buffer(2_000);
    for block in 0..blocks {
        if !missing.contains(&block) {
            buffer.push(&tone(block * PERIOD, PERIOD, 173.0));
        }
    }
    let out = drain(&mut buffer);
    (out, buffer)
}

#[test]
fn a_hole_nothing_found_is_smoothed_by_the_safety_net() {
    // No timing, so no detector: the splice guard alone.
    let (out, mut buffer) = blind_stream(&[20], 60);

    assert_tone_continuous(&out);
    let stats = buffer.take_stats();
    assert_eq!((stats.splices, stats.losses), (1, 0), "{stats}");
    assert_eq!(
        out.len() / CH,
        59 * PERIOD / PLAYOUT_CHUNK_FRAMES * PLAYOUT_CHUNK_FRAMES
    );
}

#[test]
fn several_missed_holes_are_all_smoothed() {
    let (out, mut buffer) = blind_stream(&[10, 11, 25, 40, 41, 42], 80);

    assert_tone_continuous(&out);
    assert_eq!(buffer.take_stats().splices, 3);
}

#[test]
fn a_clean_stream_is_never_spliced() {
    // Music-like audio (a sine and noise), 30 s in 1024-frame blocks.
    let mut buffer = new_buffer(2_000);
    let mut noise = 12_345_u32;
    for block in 0..30 * RATE as usize / PERIOD {
        let samples: Vec<i16> = (0..PERIOD * CH)
            .map(|n| {
                noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let frame = block * PERIOD + n / CH;
                let wave = (frame as f32 * 0.07).sin() * 9_000.0;
                wave as i16 + ((noise >> 20) as i16 - 2_048)
            })
            .collect();
        buffer.push(&samples);
        drain(&mut buffer);
    }

    assert_eq!(buffer.take_stats().splices, 0);
}

#[test]
fn a_transient_at_a_block_boundary_is_left_alone() {
    // Silence, then at exactly a block boundary a loud noise burst: the
    // signal moves a lot around the junction, so the step is not anomalous.
    let mut buffer = new_buffer(2_000);
    buffer.push(&vec![0_i16; PERIOD * CH * 4]);
    let mut noise = 99_u32;
    let burst: Vec<i16> = (0..PERIOD * CH)
        .map(|_| {
            noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (noise >> 16) as i16 / 2
        })
        .collect();
    buffer.push(&burst);
    let out = drain(&mut buffer);

    assert_eq!(buffer.take_stats().splices, 0);
    assert_eq!(out[PERIOD * CH * 4..PERIOD * CH * 4 + 64], burst[..64]);
}

#[test]
fn small_steps_are_never_smoothed() {
    // A step of 100 counts (about -50 dBFS) between blocks of near-silence.
    let mut buffer = new_buffer(2_000);
    buffer.push(&vec![0_i16; PERIOD * CH]);
    buffer.push(&vec![100_i16; PERIOD * CH]);

    assert_eq!(buffer.take_stats().splices, 0);
}

#[test]
fn a_splice_in_a_tiny_block_is_smoothed_across_pushes() {
    // The block after the junction is shorter than the 5 ms crossfade: the
    // rest of the fade carries on into the blocks that follow.
    let hz = 173.0;
    let mut buffer = new_buffer(2_000);
    buffer.push(&tone(0, 4 * PERIOD, hz));
    // 30 ms are missing, then the audio carries on in small pushes.
    let mut position = 4 * PERIOD + frames(30);
    for _ in 0..12 {
        buffer.push(&tone(position, 64, hz));
        position += 64;
    }
    buffer.push(&tone(position, 4 * PERIOD, hz));
    let out = drain(&mut buffer);

    assert_tone_continuous(&out);
    assert_eq!(buffer.take_stats().splices, 1);
}

#[test]
fn a_splice_the_playout_is_about_to_reach_is_smoothed_in_time() {
    // The consumer is playing; the new block follows a hole that nothing
    // reveals and joins right at the front of the queue.
    let hz = 173.0;
    let mut buffer = new_buffer(MIN_JITTER_BUFFER_MS);
    buffer.push(&tone(0, frames(MIN_JITTER_BUFFER_MS), hz));
    buffer.start_playout();
    let mut out = Vec::new();
    let mut chunk = vec![0_i16; PLAYOUT_CHUNK_FRAMES * CH];
    let mut position = frames(MIN_JITTER_BUFFER_MS) + 7 * PERIOD;
    for round in 0..40 {
        assert!(buffer.pull(&mut chunk, true));
        out.extend_from_slice(&chunk);
        if round == 3 {
            // Everything queued is played out, then audio arrives that does
            // not follow what was played.
            while buffer.fill_frames() >= PLAYOUT_CHUNK_FRAMES {
                assert!(buffer.pull(&mut chunk, false));
                out.extend_from_slice(&chunk);
            }
        }
        buffer.push(&tone(position, PLAYOUT_CHUNK_FRAMES, hz));
        position += PLAYOUT_CHUNK_FRAMES + if round == 5 { 3 * PERIOD } else { 0 };
    }

    assert_tone_continuous(&out);
    assert!(buffer.take_stats().splices >= 1);
}
