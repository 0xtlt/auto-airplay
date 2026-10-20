//! Detects audio that never arrived, from arrival times alone.

use std::time::Duration;

/// Upward creep of the arrival baseline, as a fraction of real time. It lets
/// the baseline follow a source clock that runs slower than ours (tens of ppm
/// in practice) without mistaking the slow drift for loss.
const DRIFT_ALLOWANCE: f64 = 0.001;

/// What the arrival of a block of frames says about the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapVerdict {
    /// The block arrived when expected.
    OnTime,
    /// This many frames went missing right before the block.
    Gap(usize),
    /// The stream paused for longer than any packet loss would explain; it
    /// restarted, so the block is not preceded by a known hole.
    Restart,
}

/// Compares the frames received with the time that passed.
///
/// Bluetooth packet loss does not delay the audio, it removes it: the capture
/// device just returns nothing for a while and then carries on in real time.
/// So the number of frames received falls behind the real-time clock by the
/// length of the hole, and stays behind.
///
/// The detector keeps `deficit = elapsed - received` (in frames) and a
/// baseline: the lowest deficit seen, creeping up slowly (see
/// [`DRIFT_ALLOWANCE`]). The baseline absorbs the constant latency, the
/// burstiness of A2DP packets and the clock drift. A deficit that rises
/// `threshold` above the baseline is a hole of that many frames; the caller
/// fills it with concealment, which counts as received. Data that was only
/// late, not lost, shows up afterwards as a deficit *below* the baseline: the
/// baseline drops, and the surplus is trimmed by the jitter buffer.
///
/// Only call [`observe`](Self::observe) right after data arrived: sampling the
/// deficit at those instants removes the sawtooth of burst arrival.
#[derive(Debug, Clone)]
pub struct GapDetector {
    rate: f64,
    threshold: f64,
    max_gap: f64,
    origin: Option<Duration>,
    last: Duration,
    received: f64,
    baseline: f64,
}

impl GapDetector {
    /// `threshold` is the smallest hole reported, `max_gap` the longest one
    /// treated as loss; anything longer is a [`GapVerdict::Restart`].
    pub fn new(sample_rate: u32, threshold: Duration, max_gap: Duration) -> Self {
        let rate = f64::from(sample_rate);
        Self {
            rate,
            threshold: threshold.as_secs_f64() * rate,
            max_gap: max_gap.as_secs_f64() * rate,
            origin: None,
            last: Duration::ZERO,
            received: 0.0,
            baseline: 0.0,
        }
    }

    /// Reports a block of `frames` that arrived at `now` (any monotonic
    /// epoch). `backlog` is the number of frames already waiting in the
    /// capture device: they are late, not lost, and count as received.
    pub fn observe(&mut self, now: Duration, frames: usize, backlog: usize) -> GapVerdict {
        self.received += frames as f64;
        let held = self.received + backlog as f64;
        let Some(origin) = self.origin else {
            self.origin = Some(now);
            self.last = now;
            self.baseline = -held;
            return GapVerdict::OnTime;
        };

        let deficit = now.saturating_sub(origin).as_secs_f64() * self.rate - held;
        let elapsed = now.saturating_sub(self.last).as_secs_f64();
        self.last = now;
        let baseline = self.baseline + DRIFT_ALLOWANCE * self.rate * elapsed;
        let excess = deficit - baseline;

        if excess <= self.threshold {
            self.baseline = baseline.min(deficit);
            GapVerdict::OnTime
        } else if excess > self.max_gap {
            self.baseline = deficit;
            GapVerdict::Restart
        } else {
            self.baseline = baseline;
            self.received += excess;
            GapVerdict::Gap(excess.round() as usize)
        }
    }
}
