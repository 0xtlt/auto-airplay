//! Detects audio that never arrived, from arrival times alone.

use std::time::Duration;

/// Upward creep of the arrival baseline, as a fraction of real time. It lets
/// the baseline follow a source clock that runs slower than ours (tens of ppm
/// in practice) without mistaking the slow drift for loss.
const DRIFT_ALLOWANCE: f64 = 0.001;

/// A late arrival this far past the baseline raises the suspicion of a hole.
/// One lost A2DP packet (SBC, 48 kHz) is about 19 ms, so it must be below
/// that; field data from a Pi with a silent turntable showed false holes of
/// about 10.5 ms, arrival lateness that persisted for the whole 80 ms window,
/// so it must be above that. 15 ms leaves about 4 ms on either side.
pub const GAP_SUSPECT_MS: u64 = 15;
/// A suspicion stacked on one that is already open only needs to rise this far
/// above the one below it. The lowest excess seen in the suspicion below
/// includes the arrival jitter of the audio right after that hole, so a second
/// lost packet looks that much smaller (a 21 ms packet after arrivals 9 ms late
/// is 12 ms); a hole that follows another within the window is a burst loss,
/// the common case, and the stacked suspicion still has to hold for the whole
/// window.
pub const GAP_STACKED_MS: u64 = 5;
/// A suspicion must hold this long (over at least three arrivals) to be
/// confirmed. The audio after the hole waits in the cushion meanwhile.
pub const GAP_WINDOW_MS: u64 = 80;
/// Longer stalls are a restarted stream, not packet loss.
pub const GAP_LIMIT_MS: u64 = 2_000;

/// What the arrival of a block of frames says about the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapVerdict {
    /// Nothing to report. (A late block may be under suspicion, see
    /// [`GapDetector`]; it is reported later, or never.)
    OnTime,
    /// Audio went missing: `frames` of it, at the point that is `behind`
    /// frames before the block that just arrived (that block not counted).
    Hole { behind: usize, frames: usize },
    /// The stream paused for longer than any packet loss would explain; it
    /// restarted, so the block is not preceded by a known hole.
    Restart,
}

/// A block arrived late; lost, or just late?
#[derive(Debug, Clone, Copy)]
struct Suspect {
    /// Frames received before the block that raised the suspicion: where the
    /// missing audio would be.
    received_at: f64,
    since: Duration,
    samples: u32,
    /// Lowest excess seen since the suspicion (measured from the baseline, so
    /// it includes the holes suspected before this one).
    min_excess: f64,
    /// How far above the suspicion below (or the baseline) an arrival must stay
    /// for this one to hold: decided when it was raised, and kept when the one
    /// below it is settled.
    gate: f64,
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
/// [`DRIFT_ALLOWANCE`]). The baseline absorbs the constant latency and the
/// clock drift. The deficit is only sampled right after data arrived, which
/// removes the sawtooth of burst delivery.
///
/// A deficit `suspect` above the baseline is only a suspicion, because a block
/// that is merely late looks the same at that instant. What tells them apart
/// is what follows. Late audio is followed by a catch-up and the deficit falls
/// back to the baseline; lost audio leaves the deficit raised for good. So the
/// suspicion is confirmed only if every arrival during `window` (and at least
/// three) is still `suspect` above the baseline, and the hole is the lowest
/// excess seen. The caller conceals it retroactively, at the recorded
/// position, while the audio after it is still in the cushion. A suspicion
/// raised while another is open (a burst loss) only has to rise
/// [`GAP_STACKED_MS`] above the one below it, see there. This is what
/// lets the threshold be as small as a single lost A2DP packet (about 20 ms)
/// without false alarms from ordinary arrival jitter.
#[derive(Debug, Clone)]
pub struct GapDetector {
    rate: f64,
    suspect: f64,
    stacked: f64,
    window: Duration,
    max_gap: f64,
    origin: Option<Duration>,
    last: Duration,
    received: f64,
    baseline: f64,
    /// Open suspicions, oldest first. Each sits on top of the ones before it:
    /// a second hole while the first is being checked is a second suspicion,
    /// settled on its own window.
    pending: Vec<Suspect>,
    /// The stacked gate also applies to new suspicions until then: a burst of
    /// losses and jitter does not end the moment one of its suspicions settles.
    burst_until: Option<Duration>,
}

impl GapDetector {
    /// The detector as the jitter buffer runs it.
    pub fn standard(sample_rate: u32) -> Self {
        Self::new(
            sample_rate,
            Duration::from_millis(GAP_SUSPECT_MS),
            Duration::from_millis(GAP_WINDOW_MS),
            Duration::from_millis(GAP_LIMIT_MS),
        )
    }

    /// `suspect` is the smallest hole reported, `window` how long a suspicion
    /// must hold, `max_gap` the longest hole treated as loss; anything longer
    /// is a [`GapVerdict::Restart`].
    pub fn new(sample_rate: u32, suspect: Duration, window: Duration, max_gap: Duration) -> Self {
        let rate = f64::from(sample_rate);
        Self {
            rate,
            suspect: suspect.as_secs_f64() * rate,
            stacked: Duration::from_millis(GAP_STACKED_MS).as_secs_f64() * rate,
            window,
            max_gap: max_gap.as_secs_f64() * rate,
            origin: None,
            last: Duration::ZERO,
            received: 0.0,
            baseline: 0.0,
            pending: Vec::new(),
            burst_until: None,
        }
    }

    /// Reports a block of `frames` that arrived at `now` (any monotonic
    /// epoch). `backlog` is the number of frames already waiting in the
    /// capture device: they are late, not lost, and count as received.
    pub fn observe(&mut self, now: Duration, frames: usize, backlog: usize) -> GapVerdict {
        let before = self.received;
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
        // The baseline stays put while a suspicion is being checked.
        let creep = if self.pending.is_empty() {
            DRIFT_ALLOWANCE * self.rate * elapsed
        } else {
            0.0
        };
        let baseline = self.baseline + creep;
        let excess = deficit - baseline;

        if excess > self.max_gap {
            self.pending.clear();
            self.baseline = deficit;
            return GapVerdict::Restart;
        }

        // Every open suspicion needs this arrival to still be `suspect` above
        // the one below it; the first that is not, and all above it, were late.
        let mut floor = 0.0;
        let mut holding = 0;
        for suspect in &mut self.pending {
            if excess - floor < suspect.gate {
                break;
            }
            suspect.samples += 1;
            suspect.min_excess = suspect.min_excess.min(excess);
            floor = suspect.min_excess;
            holding += 1;
        }
        if holding < self.pending.len() {
            self.pending.truncate(holding);
            if self.pending.is_empty() {
                // Caught up: it was late, not lost.
                self.baseline = baseline.min(deficit);
                return GapVerdict::OnTime;
            }
        }
        let in_burst =
            !self.pending.is_empty() || self.burst_until.is_some_and(|until| now < until);
        let gate = if in_burst { self.stacked } else { self.suspect };
        if excess - floor >= gate {
            self.pending.push(Suspect {
                received_at: before,
                since: now,
                samples: 1,
                min_excess: excess,
                gate,
            });
        }
        if self.pending.is_empty() {
            self.baseline = baseline.min(deficit);
            return GapVerdict::OnTime;
        }
        self.baseline = baseline;

        // Only a suspicion that held for the whole window is settled.
        let oldest = self.pending[0];
        if now.saturating_sub(oldest.since) >= self.window && oldest.samples >= 3 {
            self.pending.remove(0);
            if oldest.gate == self.stacked {
                // Jitter that was taken for a hole must not hide the next real
                // one: stay in burst mode for a while.
                self.burst_until = Some(now + 2 * self.window);
            }
            return self.settle(oldest, before);
        }
        GapVerdict::OnTime
    }

    /// Confirms a suspicion: the hole is filled by the caller, so it counts as
    /// received, which raises the baseline by its length (and lowers what the
    /// suspicions above it have left to explain).
    fn settle(&mut self, suspect: Suspect, before: f64) -> GapVerdict {
        self.baseline += suspect.min_excess;
        for rest in &mut self.pending {
            rest.min_excess -= suspect.min_excess;
        }
        GapVerdict::Hole {
            behind: (before - suspect.received_at).round() as usize,
            frames: suspect.min_excess.round() as usize,
        }
    }
}
