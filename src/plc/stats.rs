//! Counters for the health of the Bluetooth input.

use std::fmt;

/// Events since the last [`JitterBuffer::take_stats`](super::JitterBuffer::take_stats).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Counters {
    /// The output stage ran out of audio.
    pub underruns: u64,
    /// Runs of digital zeros replaced by concealment.
    pub zero_runs: u64,
    /// Capture overruns reported by the capture device.
    pub xruns: u64,
    /// Holes found from arrival times.
    pub gaps: u64,
    /// Stretches of audio replaced by concealment.
    pub concealments: u64,
    pub concealed_frames: u64,
    pub drift_drops: u64,
    pub drift_stretches: u64,
}

/// A report on the input: counts since the previous report (taken once a
/// minute, hence the names) and the buffer level now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsSnapshot {
    /// Audio lost on the way in: underruns, zero runs, capture xruns and gaps.
    pub losses: u64,
    /// Times concealment replaced audio.
    pub concealed: u64,
    /// Total length of that concealment, in ms.
    pub concealed_ms: u64,
    pub buffer_fill_ms: u64,
    pub underruns: u64,
    pub zero_runs: u64,
    pub xruns: u64,
    pub gaps: u64,
    /// Clock-drift corrections: audio dropped or stretched to keep the fill.
    pub drift_corrections: u64,
}

impl StatsSnapshot {
    pub(crate) fn new(counters: Counters, sample_rate: u32, fill_frames: usize) -> Self {
        let to_ms = |frames: u64| frames * 1000 / u64::from(sample_rate.max(1));
        Self {
            losses: counters.underruns + counters.zero_runs + counters.xruns + counters.gaps,
            concealed: counters.concealments,
            concealed_ms: to_ms(counters.concealed_frames),
            buffer_fill_ms: to_ms(fill_frames as u64),
            underruns: counters.underruns,
            zero_runs: counters.zero_runs,
            xruns: counters.xruns,
            gaps: counters.gaps,
            drift_corrections: counters.drift_drops + counters.drift_stretches,
        }
    }
}

/// The greppable per-minute log line.
impl fmt::Display for StatsSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bt_input stats: losses_per_min={} concealed_per_min={} concealed_ms={} buffer_fill_ms={} \
             underruns={} zero_runs={} xruns={} gaps={} drift_corrections={}",
            self.losses,
            self.concealed,
            self.concealed_ms,
            self.buffer_fill_ms,
            self.underruns,
            self.zero_runs,
            self.xruns,
            self.gaps,
            self.drift_corrections,
        )
    }
}
