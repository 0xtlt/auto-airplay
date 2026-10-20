//! Real-time pacing of the output side.

use std::time::Duration;

/// Whether the output stage may produce a chunk now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Far enough ahead of real time: produce nothing.
    Wait,
    /// Inside the lead the output may build up: produce a chunk if real audio
    /// is there, but never invent any.
    Optional,
    /// Behind real time: the receiver would starve, produce a chunk even if
    /// it has to be concealment.
    Required,
}

/// Paces the audio handed to the AirPlay sender against a real-time clock.
///
/// The jitter buffer must not be emptied into the sender's queue (which is
/// deeper than the jitter buffer, and would swallow the whole cushion, leaving
/// the playout to conceal at the slightest delay). The pacer lets the output
/// run `lead` ahead of real time, no more, and only demands audio (even
/// concealment) once it fell behind real time itself. At the start, the lead is
/// the burst that fills the sender's pipeline; afterwards the output follows
/// the clock, and the sender's own buffer plus the jitter buffer absorb delays.
#[derive(Debug, Clone)]
pub struct Pacer {
    rate: f64,
    lead: f64,
    origin: Option<Duration>,
    sent: f64,
}

impl Pacer {
    pub fn new(sample_rate: u32, lead: Duration) -> Self {
        let rate = f64::from(sample_rate);
        Self {
            rate,
            lead: lead.as_secs_f64() * rate,
            origin: None,
            sent: 0.0,
        }
    }

    fn ahead(&mut self, now: Duration) -> f64 {
        let origin = *self.origin.get_or_insert(now);
        self.sent - now.saturating_sub(origin).as_secs_f64() * self.rate
    }

    /// What the output may do at `now`.
    pub fn due(&mut self, now: Duration) -> Due {
        let ahead = self.ahead(now);
        if ahead < 0.0 {
            Due::Required
        } else if ahead < self.lead {
            Due::Optional
        } else {
            Due::Wait
        }
    }

    /// Records a chunk handed over.
    pub fn sent(&mut self, frames: usize) {
        self.sent += frames as f64;
    }

    /// The sender refused audio because its queue is full: it has at least a
    /// full lead of audio, whatever the clock says. Keeps the output from
    /// bursting to "catch up" once the queue drains again.
    pub fn blocked(&mut self, now: Duration) {
        let ahead = self.ahead(now);
        if ahead < self.lead
            && let Some(origin) = self.origin.as_mut()
        {
            *origin += Duration::from_secs_f64((self.lead - ahead) / self.rate);
        }
    }
}
