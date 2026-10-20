use std::collections::VecDeque;
use std::time::Duration;

use super::conceal::{Concealment, ramp, to_i16};
use super::gap::{GapDetector, GapVerdict};
use super::pace::Pacer;
use super::stats::{Counters, StatsSnapshot};

/// Length of the chunks the output stage works in.
pub const CHUNK_MS: u32 = 10;
/// Concealment fades to silence over this long (raised cosine).
const FADE_OUT_MS: u32 = 30;
/// Concealment is crossfaded into the real audio over this long.
const CROSSFADE_MS: u32 = 10;
/// The mirrored continuation is built from this much recent audio.
const HISTORY_MS: u32 = 20;
/// The smallest hole in the arrival times that counts as lost audio.
const GAP_THRESHOLD_MS: u64 = 40;
/// Longer stalls are a restarted stream, not packet loss.
const GAP_LIMIT_MS: u64 = 2_000;
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
/// Least audio played between two clock-drift corrections.
const CORRECTION_SPACING_MS: u32 = 1_000;
/// The output may run this far ahead of real time (see [`Pacer`]), capped to a
/// quarter of the target.
const LEAD_MS: u32 = 100;

/// Bluetooth jitter buffer with packet loss concealment.
///
/// Interleaved 16-bit audio goes in at the capture side ([`push_at`](Self::push_at))
/// and comes out in fixed chunks at the playout side ([`pull`](Self::pull)).
/// Which audio comes out is decided by these rules, all of which keep the
/// signal continuous (no step bigger than the signal's own slope):
///
/// * **Cushion.** Playback starts with `jitter_buffer_ms` of audio queued.
///   With no consumer attached, only the most recent `target` is kept, so a
///   stream that attaches starts at once with a full cushion.
/// * **Lost audio.** Bluetooth packet loss removes audio instead of delaying
///   it. [`GapDetector`] spots the hole from arrival times and the buffer
///   fills it, in place, with the mirrored continuation of the audio before
///   it, faded to silence over 30 ms and crossfaded into the audio after it
///   over 10 ms. The cushion stays at its level and the timeline stays whole.
/// * **Zero runs.** A run of at least 2 ms of exact digital zeros right after
///   audible signal (a loss filled with zeros) is treated the same way.
///   Vinyl surface noise is never exactly zero, and silence that follows
///   silence is passed through untouched.
/// * **Underruns.** When the consumer wants more than the buffer holds, it gets
///   what there is followed by the same concealment, exactly the requested
///   length. Concealment then continues (silence after the fade) until the
///   buffer holds half its target again, to avoid flapping, and resumes with
///   a 10 ms crossfade.
/// * **Clock drift.** The Bluetooth source and this machine disagree on the
///   length of a second by tens of ppm. When the fill exceeds the target by
///   100 ms, 20 to 60 ms are dropped from the front, crossfaded over 10 ms; when
///   it falls below a quarter of the target, 20 ms of mirrored audio are
///   inserted, crossfaded the same way. At most once per second of playback,
///   so a clock that is off by 100 ppm corrects itself every 3 minutes or so.
///   Data that arrives late in a burst is trimmed the same way.
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
    chunk: usize,
    fade_out: usize,
    crossfade: usize,
    history_len: usize,
    zero_run_min: usize,
    drop_min: usize,
    drop_max: usize,
    stretch_len: usize,
    correction_spacing: usize,
    lead: Duration,

    fifo: VecDeque<i16>,
    consumer: bool,
    read: ReadState,
    /// The last audio handed to the consumer, to continue it on underrun.
    tail: Vec<i16>,
    /// Frames played since the last clock-drift correction.
    since_correction: usize,

    gaps: GapDetector,
    /// A concealed stretch is being crossfaded into the audio being pushed.
    pending: Option<Blend>,
    zero: ZeroRun,
    xrun_pending: bool,
    counters: Counters,
}

enum ReadState {
    Playing,
    /// Output is concealment; real audio resumes once the buffer is back at
    /// the resume level.
    Concealing(Concealment),
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
    /// `jitter_buffer_ms` is clamped to `20..=10_000`.
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
            resume: (target / 2).max(crossfade),
            low: target / 4,
            high,
            capacity: (target * 2).max(high + frames(HIGH_MARGIN_MS)),
            chunk: frames(CHUNK_MS).max(1),
            fade_out: frames(FADE_OUT_MS).max(2),
            crossfade,
            history_len,
            zero_run_min: frames(ZERO_RUN_MS).max(1),
            drop_min: frames(DROP_MIN_MS).max(crossfade),
            drop_max: frames(DROP_MAX_MS).max(crossfade),
            stretch_len: frames(STRETCH_MS).max(1),
            correction_spacing: frames(CORRECTION_SPACING_MS),
            lead: Duration::from_millis(u64::from(LEAD_MS.min(jitter / 4))),
            fifo: VecDeque::new(),
            consumer: false,
            read: ReadState::Playing,
            tail: Vec::new(),
            since_correction: 0,
            gaps: GapDetector::new(
                sample_rate,
                Duration::from_millis(GAP_THRESHOLD_MS),
                Duration::from_millis(GAP_LIMIT_MS),
            ),
            pending: None,
            zero: ZeroRun::Idle,
            xrun_pending: false,
            counters: Counters::default(),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Frames per chunk of the output stage (10 ms).
    pub fn chunk_frames(&self) -> usize {
        self.chunk
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
        self.tail.clear();
        self.since_correction = 0;
    }

    /// The consumer went away: the buffer only keeps the most recent `target`.
    pub fn stop_playout(&mut self) {
        self.consumer = false;
        self.read = ReadState::Playing;
        self.tail.clear();
    }

    /// The capture device overran and was recovered.
    pub fn note_xrun(&mut self) {
        self.counters.xruns += 1;
        self.xrun_pending = true;
    }

    /// Takes the counters accumulated since the last call and starts anew.
    pub fn take_stats(&mut self) -> StatsSnapshot {
        let counters = std::mem::take(&mut self.counters);
        StatsSnapshot::new(counters, self.sample_rate, self.fill_frames())
    }

    // ----- input -----

    /// Pushes audio that arrived at `now` (any monotonic epoch), with
    /// `backlog` more frames already waiting in the capture device. A hole in
    /// the arrival times is concealed first.
    pub fn push_at(&mut self, now: Duration, samples: &[i16], backlog: usize) {
        let frames = samples.len() / self.channels;
        if frames == 0 {
            return;
        }
        match self.gaps.observe(now, frames, backlog) {
            GapVerdict::OnTime => {}
            GapVerdict::Gap(missing) => self.conceal_gap(missing),
            GapVerdict::Restart => self.splice(),
        }
        self.push(samples);
        self.xrun_pending = false;
    }

    /// Pushes audio without looking at arrival times.
    pub fn push(&mut self, samples: &[i16]) {
        let channels = self.channels;
        let frames = samples.len() / channels;
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
        self.enforce_bounds();
    }

    /// Fills a hole of `missing` frames in the timeline.
    fn conceal_gap(&mut self, missing: usize) {
        if self.consumer && matches!(self.read, ReadState::Concealing(_)) {
            // The output stage already bridged this hole, and counted it.
            return;
        }
        if !self.xrun_pending {
            self.counters.gaps += 1;
        }
        if let ZeroRun::Concealing(mut conceal) = std::mem::replace(&mut self.zero, ZeroRun::Idle) {
            // The hole extends a concealed run of zeros.
            self.fill_from(&mut conceal, missing);
            self.zero = ZeroRun::Concealing(conceal);
            return;
        }
        self.release_held_zeros();
        let mut conceal = Concealment::new(&self.history(), self.channels, Some(self.fade_out));
        self.fill_from(&mut conceal, missing);
        self.pending = Some(Blend { conceal, pos: 0 });
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
        let wanted = self.history_len * self.channels;
        let queued = self.fifo.len().min(wanted);
        let played = self.tail.len().min(wanted - queued);
        let mut history = Vec::with_capacity(queued + played);
        history.extend_from_slice(&self.tail[self.tail.len() - played..]);
        history.extend(self.fifo.range(self.fifo.len() - queued..));
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
        self.resume_if_ready();
        if matches!(self.read, ReadState::Playing) && self.consumer {
            self.correct_drift();
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
                self.counters.concealments += 1;
                self.counters.concealed_frames += (frames - fill) as u64;
                self.read = ReadState::Concealing(conceal);
            }
            ReadState::Concealing(mut conceal) if allow_conceal => {
                conceal.fill(out);
                self.remember(out);
                self.counters.concealed_frames += frames as u64;
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
    fn correct_drift(&mut self) {
        if self.since_correction < self.correction_spacing {
            return;
        }
        let fill = self.fill_frames();
        if fill > self.high {
            let drop = (fill - self.high + self.drop_min).clamp(self.drop_min, self.drop_max);
            self.drop_front(drop);
            self.counters.drift_drops += 1;
            self.since_correction = 0;
        } else if fill < self.low && fill >= self.crossfade && self.tail.len() >= 4 {
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
