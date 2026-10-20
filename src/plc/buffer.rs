use std::collections::VecDeque;
use std::time::Duration;

use super::conceal::{Concealment, ramp, to_i16};
use super::gap::{GAP_WINDOW_MS, GapDetector, GapVerdict};
use super::pace::{Due, Pacer};
use super::stats::{Counters, StatsSnapshot};

/// Frames in every chunk handed to the AirPlay sender, whatever the channel
/// count or the sample rate.
///
/// The sender resamples each chunk on its own (48 kHz to 44.1 kHz) with a
/// fixed-input sinc resampler built for `airplay_resampler::DEFAULT_CHUNK_SIZE`
/// frames: a shorter chunk fails the whole stream ("Insufficient buffer size"),
/// and frames beyond that size are silently dropped. Pacing, the lead and the
/// chunk granularity of the output stage all follow from this size (about
/// 21.3 ms at 48 kHz); the lengths of fades and crossfades do not, and work
/// across chunk boundaries.
pub const PLAYOUT_CHUNK_FRAMES: usize = 1024;
/// Concealment fades to silence over this long (raised cosine).
const FADE_OUT_MS: u32 = 30;
/// Concealment is crossfaded into the real audio over this long.
const CROSSFADE_MS: u32 = 10;
/// The mirrored continuation is built from this much recent audio.
const HISTORY_MS: u32 = 20;
/// A junction step this many times the signal's largest per-frame move around
/// it is a splice (see `guard_junction`); 1.5 is also the most a step across a
/// crossfade of two unrelated stretches of a signal can exceed its own slope.
const SPLICE_FACTOR: f32 = 1.5;
/// Steps up to this size (about -48 dBFS) are never smoothed.
const SPLICE_FLOOR: i32 = 128;
/// How far each side of a junction sets the signal's own movement.
const SPLICE_REF_MS: u32 = 2;
/// The crossfade that smooths a splice.
const SPLICE_FADE_MS: u32 = 5;
/// A splice is left this long for the timing check to find the hole and
/// conceal it properly (it needs 80 ms plus an arrival) before the waveform
/// check smooths it.
const SPLICE_WAIT_MS: u32 = 120;
/// Digital zeros shorter than this are ordinary signal.
const ZERO_RUN_MS: u32 = 2;
/// Zeros only count as lost audio when the signal right before them reached
/// at least this peak (about -60 dBFS): silence after silence is just silence.
const ZERO_RUN_MIN_PEAK: u16 = 32;
/// The buffer may hold this much above its target before the clock-drift
/// correction trims it.
const HIGH_MARGIN_MS: u32 = 100;
/// Smallest and largest piece dropped by one clock-drift correction.
const DROP_MIN_MS: u32 = 20;
const DROP_MAX_MS: u32 = 60;
/// Length of the piece inserted when the buffer runs low.
const STRETCH_MS: u32 = 20;
/// The fill must stay below the low watermark this long before audio is
/// inserted: a loss dips it too, but briefly.
const LOW_HOLD_MS: u32 = 500;
/// Least audio played between two clock-drift corrections.
const CORRECTION_SPACING_MS: u32 = 1_000;
/// The shortest cushion `audio.jitter_buffer_ms` accepts.
///
/// A lost packet is confirmed `GAP_WINDOW_MS` after it, on the arrival that
/// completes the window (up to one capture period later), and only then can
/// the concealment be inserted, at a position that must not have played yet.
/// The audio queued ahead of the position is the cushion less what the sender
/// already holds (the lead plus up to one chunk of overshoot), and the capture
/// period between pushes dips it once more. At 44.1 kHz a chunk and a capture
/// period are 24 ms each, which is what the sum below uses (the slowest rate
/// that is not exotic); the rest is margin.
pub const MIN_JITTER_BUFFER_MS: u32 = 200;

const _: () = {
    const CHUNK_MS_AT_44K1: u32 = (PLAYOUT_CHUNK_FRAMES as u32 * 1000).div_ceil(44_100);
    const MARGIN_MS: u32 = 20;
    assert!(
        MIN_JITTER_BUFFER_MS
            >= GAP_WINDOW_MS as u32 + CHUNK_MS_AT_44K1 + LEAD_MS + CHUNK_MS_AT_44K1 + MARGIN_MS,
        "the shortest cushion must outlast the hole confirmation"
    );
};

/// The output may run this far ahead of real time (see [`Pacer`]), capped to a
/// tenth of the target: that audio leaves the queue for the sender's.
const LEAD_MS: u32 = 50;

/// Bluetooth jitter buffer with packet loss concealment.
///
/// Interleaved 16-bit audio goes in at the capture side ([`push_at`](Self::push_at))
/// and comes out in chunks of [`PLAYOUT_CHUNK_FRAMES`] at the playout side
/// ([`pull_chunk`](Self::pull_chunk)).
/// Which audio comes out is decided by these rules, all of which keep the
/// signal continuous (no step bigger than the signal's own slope):
///
/// * **Cushion.** Playback starts only once `jitter_buffer_ms` of audio is
///   queued (a short wait if less was). With no consumer attached, exactly the
///   most recent `target` is kept, so a stream that attaches starts at once
///   with a full cushion. Handing the sender a short lead (at most 50 ms, a
///   tenth of the target) leaves the queue within 90 to 100 % of the target.
/// * **Lost audio.** Bluetooth packet loss removes audio instead of delaying
///   it. [`GapDetector`] spots a hole from arrival times, from a single lost
///   packet (15 ms and up) to a long one, and confirms it after 80 ms, once
///   it is clear the audio is lost, not late. The buffer then fills the hole
///   retroactively, at the recorded position (the audio after it has not
///   played yet), with the mirrored continuation of the audio before it,
///   faded to silence over 30 ms and crossfaded into the audio after it over
///   10 ms. The cushion keeps its level and the timeline stays whole. Audio
///   that was merely late is not touched. With a cushion shorter than
///   [`MIN_JITTER_BUFFER_MS`] the position may have played by the time the
///   hole is confirmed: it is then counted but not concealed.
/// * **Zero runs.** A run of at least 2 ms of exact digital zeros right after
///   audible signal (a loss filled with zeros) is treated the same way.
///   Vinyl surface noise is never exactly zero, and silence that follows
///   silence is passed through untouched.
/// * **Underruns.** When the consumer wants more than the buffer holds, it gets
///   what there is followed by the same concealment, exactly the requested
///   length. Concealment then continues (silence after the fade) until the
///   buffer holds its whole target again, so the cushion is restored (and
///   playback does not flap), and resumes with a 10 ms crossfade.
/// * **Clock drift.** The Bluetooth source and this machine disagree on the
///   length of a second by tens of ppm. When the fill exceeds the target by
///   100 ms, 20 to 60 ms are dropped from the front, crossfaded over 10 ms; when
///   it falls a quarter of the target (at least three chunks) below it, 20 ms of
///   mirrored audio are inserted, crossfaded the same way. At most once per
///   second of playback, so a clock that is off by 100 ppm corrects itself every
///   3 minutes or so.
///
/// The target is exactly the configured size and stays so: nothing here sizes
/// the buffer from what it observes. The drift correction above is clock-drift
/// compensation toward that fixed target, not adaptive sizing.
///
/// All the counters behind the per-minute log line live here too.
pub struct JitterBuffer {
    channels: usize,
    sample_rate: u32,

    // Sizes in frames.
    target: usize,
    resume: usize,
    low: usize,
    high: usize,
    capacity: usize,
    fade_out: usize,
    crossfade: usize,
    history_len: usize,
    zero_run_min: usize,
    splice_ref: usize,
    splice_fade: usize,
    splice_wait: usize,
    drop_min: usize,
    drop_max: usize,
    stretch_len: usize,
    low_hold: usize,
    correction_spacing: usize,
    lead: Duration,

    fifo: VecDeque<i16>,
    consumer: bool,
    read: ReadState,
    /// The consumer attached and the cushion is not complete yet: the silence
    /// it gets meanwhile is not concealment of anything lost.
    starting: bool,
    /// The last audio handed to the consumer, to continue it on underrun.
    tail: Vec<i16>,
    /// Frames played since the last clock-drift correction.
    since_correction: usize,
    /// Frames played with the fill below the low watermark, without a break.
    /// Drift keeps the fill low; a hole in the arrivals only dips it for a
    /// moment (until it is confirmed and concealed).
    low_for: usize,

    gaps: GapDetector,
    /// Frames received through `push_at`, and where the capture last overran.
    received: u64,
    xrun_at: Option<u64>,
    /// Frames received when the output last ran dry: the audio up to there
    /// has been played.
    underrun_at: Option<u64>,
    /// A concealed stretch is being crossfaded into the audio being pushed.
    pending: Option<Blend>,
    /// Junctions that did not join, waiting for the timing check.
    junctions: Vec<Junction>,
    zero: ZeroRun,
    counters: Counters,
}

enum ReadState {
    Playing,
    /// Output is concealment; real audio resumes once the buffer is back at
    /// the resume level.
    Concealing(Concealment),
}

/// A block junction that does not join (see `JitterBuffer::mark_junction`).
struct Junction {
    /// Frames received before the block: where the junction is, in the audio
    /// received.
    received_at: u64,
    /// Concealment inserted after the junction since.
    inserted: usize,
}

struct Blend {
    conceal: Concealment,
    /// Frames of the crossfade already applied.
    pos: usize,
}

/// State of the scan for runs of zeros in the pushed audio.
enum ZeroRun {
    Idle,
    /// A run of this many zero frames has started; they are held back (not in
    /// the FIFO yet) until the run is long enough to tell what it is.
    Counting(usize),
    /// A run being replaced by concealment.
    Concealing(Concealment),
    /// A run that is just silence: passed through.
    Passing,
}

impl JitterBuffer {
    /// `jitter_buffer_ms` is clamped to `20..=10_000`. (The configuration only
    /// accepts [`MIN_JITTER_BUFFER_MS`] and up; below that, holes may already
    /// have played when they are confirmed.)
    pub fn new(sample_rate: u32, channels: u8, jitter_buffer_ms: u32) -> Self {
        let frames = |ms: u32| (u64::from(sample_rate) * u64::from(ms) / 1000) as usize;
        let jitter = jitter_buffer_ms.clamp(20, 10_000);
        let target = frames(jitter);
        let high = target + frames(HIGH_MARGIN_MS);
        let crossfade = frames(CROSSFADE_MS).max(2);
        let history_len = frames(HISTORY_MS).max(2);
        Self {
            channels: usize::from(channels.max(1)),
            sample_rate,
            target,
            resume: target,
            low: target.saturating_sub((target / 4).max(3 * PLAYOUT_CHUNK_FRAMES)),
            high,
            capacity: (target * 2).max(high + frames(HIGH_MARGIN_MS)),
            fade_out: frames(FADE_OUT_MS).max(2),
            crossfade,
            history_len,
            zero_run_min: frames(ZERO_RUN_MS).max(1),
            splice_ref: frames(SPLICE_REF_MS).max(2),
            splice_fade: frames(SPLICE_FADE_MS).max(2),
            splice_wait: frames(SPLICE_WAIT_MS),
            junctions: Vec::new(),
            drop_min: frames(DROP_MIN_MS).max(crossfade),
            drop_max: frames(DROP_MAX_MS).max(crossfade),
            stretch_len: frames(STRETCH_MS).max(1),
            low_hold: frames(LOW_HOLD_MS),
            correction_spacing: frames(CORRECTION_SPACING_MS),
            lead: Duration::from_millis(u64::from(LEAD_MS.min(jitter / 10))),
            fifo: VecDeque::new(),
            consumer: false,
            read: ReadState::Playing,
            starting: false,
            tail: Vec::new(),
            since_correction: 0,
            low_for: 0,
            gaps: GapDetector::standard(sample_rate),
            pending: None,
            zero: ZeroRun::Idle,
            received: 0,
            xrun_at: None,
            underrun_at: None,
            counters: Counters::default(),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Frames per playout chunk: [`PLAYOUT_CHUNK_FRAMES`].
    pub fn chunk_frames(&self) -> usize {
        PLAYOUT_CHUNK_FRAMES
    }

    /// The fill the buffer aims for, in frames.
    pub fn target_frames(&self) -> usize {
        self.target
    }

    /// Frames the buffer must hold before concealment gives way to audio.
    pub fn resume_frames(&self) -> usize {
        self.resume
    }

    /// Frames currently queued.
    pub fn fill_frames(&self) -> usize {
        self.fifo.len() / self.channels
    }

    /// A pacer for a new consumer of this buffer.
    pub fn pacer(&self) -> Pacer {
        Pacer::new(self.sample_rate, self.lead)
    }

    // ----- control -----

    /// A consumer attached: playback starts from silence with a fade-in over
    /// the audio queued so far.
    pub fn start_playout(&mut self) {
        self.consumer = true;
        self.read = ReadState::Concealing(self.silence());
        self.starting = true;
        self.underrun_at = None;
        self.tail.clear();
        self.since_correction = 0;
        self.low_for = 0;
    }

    /// The consumer went away: the buffer only keeps the most recent `target`.
    pub fn stop_playout(&mut self) {
        self.consumer = false;
        self.starting = false;
        self.underrun_at = None;
        self.read = ReadState::Playing;
        self.tail.clear();
    }

    /// The query for the frames waiting in the capture device failed.
    pub fn note_avail_error(&mut self) {
        self.counters.avail_errors += 1;
    }

    /// The capture device overran and was recovered.
    pub fn note_xrun(&mut self) {
        self.counters.xruns += 1;
        self.xrun_at = Some(self.received);
    }

    /// Takes the counters accumulated since the last call and starts anew.
    pub fn take_stats(&mut self) -> StatsSnapshot {
        let counters = std::mem::take(&mut self.counters);
        StatsSnapshot::new(counters, self.sample_rate, self.fill_frames())
    }

    // ----- input -----

    /// Pushes audio that arrived at `now` (any monotonic epoch), with
    /// `backlog` more frames already waiting in the capture device. A hole in
    /// the arrival times is concealed once it is confirmed, which takes a few
    /// more pushes (see [`GapDetector`]).
    pub fn push_at(&mut self, now: Duration, samples: &[i16], backlog: usize) {
        let frames = samples.len() / self.channels;
        if frames == 0 {
            return;
        }
        self.counters.reads += 1;
        match self.gaps.observe(now, frames, backlog) {
            GapVerdict::OnTime => {}
            GapVerdict::Hole { behind, frames } => self.conceal_hole(behind, frames),
            GapVerdict::Restart => self.splice(),
        }
        self.push(samples);
    }

    /// Pushes audio without looking at arrival times.
    pub fn push(&mut self, samples: &[i16]) {
        let channels = self.channels;
        let frames = samples.len() / channels;
        let junction = self.fill_frames();
        let mut at = 0;
        while at < frames {
            match std::mem::replace(&mut self.zero, ZeroRun::Idle) {
                ZeroRun::Idle => {
                    let zeros = zero_run_start(samples, channels, at);
                    self.append(&samples[at * channels..zeros * channels]);
                    at = zeros;
                    if at < frames {
                        self.zero = ZeroRun::Counting(0);
                    }
                }
                ZeroRun::Counting(held) => {
                    let end = zero_run_end(samples, channels, at);
                    let held = held + (end - at);
                    at = end;
                    if held >= self.zero_run_min {
                        self.zero = self.begin_zero_run(held);
                    } else if at < frames {
                        // The run ended short: ordinary zero crossings.
                        self.fifo.extend(std::iter::repeat_n(0, held * channels));
                    } else {
                        self.zero = ZeroRun::Counting(held);
                    }
                }
                ZeroRun::Concealing(mut conceal) => {
                    let end = zero_run_end(samples, channels, at);
                    self.fill_from(&mut conceal, end - at);
                    at = end;
                    if at < frames {
                        self.pending = Some(Blend { conceal, pos: 0 });
                    } else {
                        self.zero = ZeroRun::Concealing(conceal);
                    }
                }
                ZeroRun::Passing => {
                    let end = zero_run_end(samples, channels, at);
                    self.fifo
                        .extend(std::iter::repeat_n(0, (end - at) * channels));
                    at = end;
                    if at >= frames {
                        self.zero = ZeroRun::Passing;
                    }
                }
            }
        }
        self.mark_junction(junction, self.received);
        self.received += frames as u64;
        self.settle_junctions(0);
        self.enforce_bounds();
    }

    /// The safety net under all the timing heuristics: a block that does not
    /// join the audio before it is smoothed in, whatever the reason (a hole
    /// that was not found, a stream that jumped).
    ///
    /// The step across the junction between the queue and the audio just
    /// pushed (`junction` frames in) is compared with how much the signal
    /// moves per frame around it, over `SPLICE_REF_MS` on each side. A step
    /// that is more than `SPLICE_FACTOR` times the largest of those moves (and
    /// above `SPLICE_FLOOR`, so near-silence is left alone) is a splice. Only
    /// block junctions are looked at, so transients in the signal are never
    /// touched; a false trigger costs a 5 ms crossfade.
    fn junction_is_splice(&self, junction: usize) -> bool {
        let channels = self.channels;
        let queued = self.fill_frames();
        if queued <= junction + 1 {
            return false;
        }
        // The frame before the junction: the queue's, or else the last played.
        let before: Vec<i32> = if junction > 0 {
            (0..channels)
                .map(|c| i32::from(self.fifo[(junction - 1) * channels + c]))
                .collect()
        } else if matches!(self.read, ReadState::Playing) && self.tail.len() >= channels {
            self.tail[self.tail.len() - channels..]
                .iter()
                .map(|&sample| i32::from(sample))
                .collect()
        } else {
            return false;
        };
        let step: Vec<i32> = (0..channels)
            .map(|c| (i32::from(self.fifo[junction * channels + c]) - before[c]).abs())
            .collect();
        if step.iter().all(|&s| s <= SPLICE_FLOOR) {
            return false;
        }

        let history = self.history_before(junction);
        let have = history.len() / channels;
        if have < 2 {
            return false;
        }
        let reference = self.splice_ref;
        for c in 0..channels {
            let mut largest = 0;
            for frame in have.saturating_sub(reference)..have - 1 {
                let a = i32::from(history[frame * channels + c]);
                let b = i32::from(history[(frame + 1) * channels + c]);
                largest = largest.max((b - a).abs());
            }
            let after = (queued - junction).min(reference + 1);
            for frame in 0..after - 1 {
                let a = i32::from(self.fifo[(junction + frame) * channels + c]);
                let b = i32::from(self.fifo[(junction + frame + 1) * channels + c]);
                largest = largest.max((b - a).abs());
            }
            if step[c] > SPLICE_FLOOR && step[c] as f32 > SPLICE_FACTOR * largest as f32 {
                return true;
            }
        }
        false
    }

    /// Crossfades the first `SPLICE_FADE_MS` of the audio after `junction`
    /// from the mirrored continuation of the audio before it. No time is
    /// inserted and nothing is counted as lost.
    fn smooth_junction(&mut self, junction: usize) {
        let channels = self.channels;
        let queued = self.fill_frames();
        let history = self.history_before(junction);
        let mut conceal = Concealment::new(&history, channels, None);
        let blended = self.splice_fade.min(queued - junction);
        let range = junction * channels..(junction + blended) * channels;
        let mut seg: Vec<i16> = self.fifo.range(range.clone()).copied().collect();
        conceal.blend_into(&mut seg, 0, self.splice_fade);
        for (slot, value) in self.fifo.range_mut(range).zip(seg) {
            *slot = value;
        }
        if blended < self.splice_fade {
            self.pending = Some(Blend {
                conceal,
                pos: blended,
            });
        }
        self.counters.splices += 1;
    }

    /// Looks at the junction of the block just pushed (`junction` frames in).
    /// A splice is not smoothed at once: the timing check may still find a
    /// hole there and conceal it properly, which takes `SPLICE_WAIT_MS`. The
    /// junction is marked, and dealt with by [`settle_junctions`](Self::settle_junctions)
    /// when that time is up, or when the playout is about to reach it.
    fn mark_junction(&mut self, junction: usize, received_at: u64) {
        if self.junction_is_splice(junction) {
            self.junctions.push(Junction {
                received_at,
                inserted: 0,
            });
        }
    }

    /// Where a marked junction is in the queue now, if it still is.
    fn junction_index(&self, mark: &Junction) -> Option<usize> {
        self.junction_index_of(mark, self.fill_frames())
    }

    fn junction_index_of(&self, mark: &Junction, queued: usize) -> Option<usize> {
        let held = match self.zero {
            ZeroRun::Counting(held) => held,
            _ => 0,
        };
        let since = (self.received - mark.received_at) as usize;
        let after = (since + mark.inserted).saturating_sub(held);
        (after <= queued).then(|| queued - after)
    }

    /// Smooths the marked junctions that are due: older than `SPLICE_WAIT_MS`,
    /// or within `ahead` frames of the front of the queue (about to play). A
    /// junction that has played, or that the timing check dealt with in the
    /// meantime, is forgotten.
    fn settle_junctions(&mut self, ahead: usize) {
        if self.junctions.is_empty() {
            return;
        }
        let queued = self.fill_frames();
        let marks = std::mem::take(&mut self.junctions);
        for mark in marks {
            let Some(junction) = self.junction_index_of(&mark, queued) else {
                continue; // played or trimmed away
            };
            let since = (self.received - mark.received_at) as usize;
            let due =
                since >= self.splice_wait || (ahead > 0 && junction < ahead + self.splice_fade);
            if !due {
                self.junctions.push(mark);
            } else if self.junction_is_splice(junction) {
                self.smooth_junction(junction);
            }
        }
    }

    /// Fills a confirmed hole of `missing` frames, `behind` frames before the
    /// end of the audio received so far, with concealment: the audio that
    /// followed it is moved back, crossfaded in.
    fn conceal_hole(&mut self, behind: usize, missing: usize) {
        let position = self.received.saturating_sub(behind as u64);
        if self.underrun_at == Some(position) {
            // The output ran dry exactly where the audio stopped: the output
            // stage bridged this hole, and counted it. (A hole anywhere else,
            // even right after, is in audio that is queued while the cushion
            // rebuilds, and is concealed as usual.)
            return;
        }
        if self.xrun_at != Some(position) {
            // A capture overrun leaves a hole; it is one loss, counted once.
            self.counters.gaps += 1;
        }

        // Zeros held back are not queued yet: they come after the hole.
        let held = match self.zero {
            ZeroRun::Counting(held) => held,
            _ => 0,
        };
        let queued = self.fill_frames();
        let after = behind.saturating_sub(held);
        if after > queued {
            // Already played: too late to bridge it.
            return;
        }
        let at = queued - after;
        let channels = self.channels;
        let mut conceal = Concealment::new(&self.history_before(at), channels, Some(self.fade_out));
        let mut hidden = vec![0_i16; missing * channels];
        conceal.fill(&mut hidden);
        let mut rest: Vec<i16> = self.fifo.drain(at * channels..).collect();
        let blended = self.crossfade.min(queued - at);
        conceal.blend_into(&mut rest[..blended * channels], 0, self.crossfade);
        self.fifo.extend(hidden);
        self.fifo.extend(rest);
        if blended < self.crossfade {
            // The hole is at the very end: the rest of the crossfade is
            // applied to the audio still to come.
            self.pending = Some(Blend {
                conceal,
                pos: blended,
            });
        }
        self.counters.concealments += 1;
        self.counters.concealed_frames += missing as u64;
        // The timing check dealt with the junction at this hole; the ones
        // before it now have the concealment between them and the tail.
        self.junctions.retain_mut(|mark| {
            if mark.received_at.abs_diff(position) <= 16 {
                return false;
            }
            if mark.received_at < position {
                mark.inserted += missing;
            }
            true
        });
    }

    /// The stream restarted after a long stall: joins the audio before and
    /// after with a crossfade, without inventing any time.
    fn splice(&mut self) {
        if matches!(self.zero, ZeroRun::Concealing(_)) {
            return;
        }
        self.release_held_zeros();
        let history = self.history();
        if history.len() >= 2 * self.channels {
            let conceal = Concealment::new(&history, self.channels, Some(self.fade_out));
            self.pending = Some(Blend { conceal, pos: 0 });
        }
    }

    fn release_held_zeros(&mut self) {
        match std::mem::replace(&mut self.zero, ZeroRun::Idle) {
            ZeroRun::Counting(held) => {
                self.fifo
                    .extend(std::iter::repeat_n(0, held * self.channels));
            }
            ZeroRun::Passing => {}
            other => self.zero = other,
        }
    }

    /// A run of `held` zero frames is long enough to judge. Conceals it if the
    /// signal before it was audible.
    fn begin_zero_run(&mut self, held: usize) -> ZeroRun {
        let history = self.history();
        let recent = history
            .len()
            .saturating_sub(self.zero_run_min * self.channels);
        let peak = history[recent..]
            .iter()
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap_or(0);
        if peak < ZERO_RUN_MIN_PEAK {
            self.fifo
                .extend(std::iter::repeat_n(0, held * self.channels));
            return ZeroRun::Passing;
        }
        self.counters.zero_runs += 1;
        let mut conceal = Concealment::new(&history, self.channels, Some(self.fade_out));
        self.fill_from(&mut conceal, held);
        ZeroRun::Concealing(conceal)
    }

    /// Appends `frames` of concealment to the FIFO and counts it as concealed.
    /// A new concealment event is counted when it starts (`conceal` at 0).
    fn fill_from(&mut self, conceal: &mut Concealment, frames: usize) {
        if frames == 0 {
            return;
        }
        let mut buffer = vec![0_i16; frames * self.channels];
        conceal.fill(&mut buffer);
        self.fifo.extend(buffer);
        self.counters.concealed_frames += frames as u64;
        if conceal_is_new(conceal, frames) {
            self.counters.concealments += 1;
        }
    }

    /// Appends real audio, blending the start into a pending concealment.
    fn append(&mut self, audio: &[i16]) {
        if audio.is_empty() {
            return;
        }
        let Some(mut blend) = self.pending.take() else {
            self.fifo.extend(audio.iter().copied());
            return;
        };
        let mut audio = audio.to_vec();
        let frames = audio.len() / self.channels;
        let blended = (self.crossfade - blend.pos).min(frames);
        blend.conceal.blend_into(
            &mut audio[..blended * self.channels],
            blend.pos,
            self.crossfade,
        );
        blend.pos += blended;
        if blend.pos < self.crossfade {
            self.pending = Some(blend);
        }
        self.fifo.extend(audio);
    }

    /// Last `history_len` frames of the timeline, oldest first: the queue's
    /// tail, completed with what was already played if the queue is shorter.
    fn history(&self) -> Vec<i16> {
        self.history_before(self.fill_frames())
    }

    /// Like [`history`](Self::history), for the point `at` frames into the
    /// queue.
    ///
    /// A mirror must be built from one stretch of one signal. What was played
    /// continues into the queue only while real audio is being played: while
    /// the output is concealment (the cushion filling, or refilling after an
    /// outage) it is unrelated to the queue and is left out, so the history
    /// may be shorter than asked for (see `MIN_MIRROR_FRAMES`).
    fn history_before(&self, at: usize) -> Vec<i16> {
        let mut wanted = self.history_len * self.channels;
        let end = at * self.channels;
        // Nor across a junction that does not join yet (see `mark_junction`):
        // the history starts after the last one.
        let after_junction = self
            .junctions
            .iter()
            .filter_map(|mark| self.junction_index(mark))
            .filter(|&index| index < at)
            .max();
        if let Some(index) = after_junction {
            wanted = wanted.min((at - index) * self.channels);
        }
        let queued = end.min(wanted);
        let played = if after_junction.is_none() && matches!(self.read, ReadState::Playing) {
            self.tail.len().min(wanted - queued)
        } else {
            0
        };
        let mut history = Vec::with_capacity(queued + played);
        history.extend_from_slice(&self.tail[self.tail.len() - played..]);
        history.extend(self.fifo.range(end - queued..end));
        history
    }

    fn silence(&self) -> Concealment {
        Concealment::new(&[], self.channels, Some(self.fade_out))
    }

    fn enforce_bounds(&mut self) {
        let fill = self.fill_frames();
        if !self.consumer {
            if fill > self.target {
                self.fifo.drain(..(fill - self.target) * self.channels);
            }
        } else if fill > self.capacity {
            self.drop_front(fill - self.high);
        }
    }

    // ----- output -----

    /// Pulls the next playout chunk if `pacer` says one is due: real audio
    /// inside the lead, concealment where the output fell behind real time.
    /// `chunk` must hold exactly [`PLAYOUT_CHUNK_FRAMES`] frames; any other
    /// size is refused (and trips a debug assertion), so that no frame of the
    /// wrong size can reach the AirPlay resampler. The caller reports the chunk
    /// with [`Pacer::sent`] once it is handed over.
    pub fn pull_chunk(&mut self, pacer: &mut Pacer, now: Duration, chunk: &mut [i16]) -> bool {
        let expected = PLAYOUT_CHUNK_FRAMES * self.channels;
        debug_assert_eq!(chunk.len(), expected, "playout chunks have a fixed size");
        if chunk.len() != expected {
            return false;
        }
        let allow_conceal = match pacer.due(now) {
            Due::Wait => return false,
            Due::Optional => false,
            Due::Required => true,
        };
        self.pull(chunk, allow_conceal)
    }

    /// Fills `out` (a whole number of frames) with the next audio.
    ///
    /// With `allow_conceal`, `out` is always filled: real audio first, then
    /// concealment for whatever the buffer lacks. Without it, the call only
    /// succeeds (returns `true`) if all of `out` can be real audio, and leaves
    /// the buffer alone otherwise.
    pub fn pull(&mut self, out: &mut [i16], allow_conceal: bool) -> bool {
        let channels = self.channels;
        let frames = out.len() / channels;
        if frames == 0 {
            return true;
        }
        // Junctions the playout is about to reach are smoothed now. (While
        // concealing, nothing of the queue is played.)
        let playing = matches!(self.read, ReadState::Playing) || self.fill_frames() >= self.resume;
        self.settle_junctions(if playing { frames } else { 0 });
        self.resume_if_ready();
        if matches!(self.read, ReadState::Playing) && self.consumer {
            self.correct_drift(frames);
        }

        let fill = self.fill_frames();
        match std::mem::replace(&mut self.read, ReadState::Playing) {
            ReadState::Playing if fill >= frames => {
                self.take_real(out);
                self.since_correction += frames;
            }
            ReadState::Playing if allow_conceal => {
                let (real, hidden) = out.split_at_mut(fill * channels);
                self.take_real(real);
                let mut conceal = Concealment::new(&self.tail, channels, Some(self.fade_out));
                conceal.fill(hidden);
                self.remember(hidden);
                self.counters.underruns += 1;
                self.underrun_at = Some(self.received);
                self.counters.concealments += 1;
                self.counters.concealed_frames += (frames - fill) as u64;
                self.read = ReadState::Concealing(conceal);
            }
            ReadState::Concealing(mut conceal) if allow_conceal => {
                conceal.fill(out);
                self.remember(out);
                if !self.starting {
                    self.counters.concealed_frames += frames as u64;
                }
                self.read = ReadState::Concealing(conceal);
            }
            state => {
                self.read = state;
                return false;
            }
        }
        true
    }

    /// Copies `out.len()` samples from the queue into `out`.
    fn take_real(&mut self, out: &mut [i16]) {
        let count = out.len();
        for (slot, sample) in out.iter_mut().zip(self.fifo.drain(..count)) {
            *slot = sample;
        }
        self.remember(out);
    }

    fn remember(&mut self, played: &[i16]) {
        let keep = self.history_len * self.channels;
        if played.len() >= keep {
            self.tail.clear();
            self.tail.extend_from_slice(&played[played.len() - keep..]);
        } else {
            self.tail.extend_from_slice(played);
            if self.tail.len() > keep {
                self.tail.drain(..self.tail.len() - keep);
            }
        }
    }

    /// Ends a concealment once the buffer is back at the resume level, by
    /// crossfading its continuation into the first audio in the queue.
    fn resume_if_ready(&mut self) {
        if !matches!(self.read, ReadState::Concealing(_)) || self.fill_frames() < self.resume {
            return;
        }
        let ReadState::Concealing(mut conceal) =
            std::mem::replace(&mut self.read, ReadState::Playing)
        else {
            return;
        };
        self.starting = false;
        self.blend_front(&mut conceal, 0);
    }

    /// Crossfades the first `crossfade` frames of the queue with `conceal`
    /// and puts `inserted` frames of it in front of them.
    fn blend_front(&mut self, conceal: &mut Concealment, inserted: usize) {
        let blended = self.crossfade.min(self.fill_frames());
        let mut head: Vec<i16> = self.fifo.drain(..blended * self.channels).collect();
        let mut front = vec![0_i16; inserted * self.channels];
        conceal.fill(&mut front);
        conceal.blend_into(&mut head, 0, self.crossfade);
        front.extend(head);
        for sample in front.into_iter().rev() {
            self.fifo.push_front(sample);
        }
    }

    /// Keeps the fill inside its band against clock drift, at most once per
    /// `CORRECTION_SPACING_MS` of playback.
    fn correct_drift(&mut self, played: usize) {
        let fill = self.fill_frames();
        if fill < self.low {
            self.low_for += played;
        } else {
            self.low_for = 0;
        }
        if self.since_correction < self.correction_spacing {
            return;
        }
        if fill > self.high {
            let drop = (fill - self.high + self.drop_min).clamp(self.drop_min, self.drop_max);
            self.drop_front(drop);
            self.counters.drift_drops += 1;
            self.since_correction = 0;
        } else if self.low_for >= self.low_hold && fill >= self.crossfade && self.tail.len() >= 4 {
            let mut conceal = Concealment::new(&self.tail, self.channels, None);
            self.blend_front(&mut conceal, self.stretch_len);
            self.counters.drift_stretches += 1;
            self.since_correction = 0;
        }
    }

    /// Removes `frames` from the front of the queue without a step: the audio
    /// that follows is crossfaded in over the audio that would have played.
    fn drop_front(&mut self, frames: usize) {
        let fill = self.fill_frames();
        let drop = frames.max(self.crossfade);
        if fill < drop + 1 {
            return;
        }
        let channels = self.channels;
        let blended = self.crossfade.min(fill - drop);
        for frame in 0..blended {
            let weight = ramp(frame, blended);
            for channel in 0..channels {
                let old = f32::from(self.fifo[frame * channels + channel]);
                let at = (drop + frame) * channels + channel;
                let new = f32::from(self.fifo[at]);
                self.fifo[at] = to_i16(old * (1.0 - weight) + new * weight);
            }
        }
        self.fifo.drain(..drop * channels);
    }
}

fn is_zero_frame(samples: &[i16], channels: usize, frame: usize) -> bool {
    samples[frame * channels..(frame + 1) * channels]
        .iter()
        .all(|&sample| sample == 0)
}

/// First all-zero frame at or after `from`, or the frame count.
fn zero_run_start(samples: &[i16], channels: usize, from: usize) -> usize {
    let frames = samples.len() / channels;
    (from..frames)
        .find(|&frame| is_zero_frame(samples, channels, frame))
        .unwrap_or(frames)
}

/// First frame at or after `from` that is not all zeros, or the frame count.
fn zero_run_end(samples: &[i16], channels: usize, from: usize) -> usize {
    let frames = samples.len() / channels;
    (from..frames)
        .find(|&frame| !is_zero_frame(samples, channels, frame))
        .unwrap_or(frames)
}

/// Whether this was the first batch generated by `conceal`.
fn conceal_is_new(conceal: &Concealment, just_generated: usize) -> bool {
    conceal.elapsed() == just_generated
}
