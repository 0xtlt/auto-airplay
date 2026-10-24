//! The sender's counters for a periodic report (local patch, see VENDORED.md).
//!
//! Public but `#[doc(hidden)]`, only so the host crate's tests can drive the
//! counters with a clock of their own.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::rtp::RetransmitRequest;

/// What the sender did over a period: counts since the previous
/// [`OutputCounters::take`] (`AudioStreamer::take_output_stats`), maxima over
/// the same period.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputStats {
    /// Audio packets sent.
    pub packets_sent: u64,
    /// Of those, sent more than one packet duration after their deadline.
    pub late_packets: u64,
    /// The latest a packet was sent after its deadline, in µs.
    pub max_late_us: u64,
    /// Retransmit requests received.
    pub rtx_requests: u64,
    /// Retransmit requests (payload type 85) of a length the control channel
    /// does not understand: ignored, nothing sent again.
    pub rtx_unparsed: u64,
    /// Packets those requests asked for.
    pub rtx_requested: u64,
    /// Requested packets sent again.
    pub rtx_served: u64,
    /// Requested packets not sent again: not in the history (any more), or
    /// the resend failed.
    pub rtx_missed: u64,
    /// Requested packets asked for once the time the sync packets announced
    /// for them had passed (their deadline plus the announced latency): a
    /// resend cannot arrive before they play. Only packets the sender still
    /// has a record of (the last 512 sent) are judged.
    pub rtx_late: u64,
    /// The longest a requested packet had been out when its request came in,
    /// in µs: from the moment it was really sent, late or not.
    pub rtx_max_age_us: u64,
}

/// Packets remembered for judging requests: as many as the retransmit
/// history keeps.
const RECORDS: usize = 512;
/// The low 48 bits of a record: µs on the monotonic clock, about 9 years.
const TIME_MASK: u64 = (1 << 48) - 1;
/// A slot no packet was recorded in.
const EMPTY: u64 = u64::MAX;

/// The counters behind [`OutputStats`], shared by the sender thread, which
/// records each packet it sends, and the retransmit path. Lock-free: every
/// field is an atomic, and the sender thread only stores and adds.
#[derive(Debug)]
pub struct OutputCounters {
    sent: AtomicU64,
    late: AtomicU64,
    max_late_us: AtomicU64,
    rtx_requests: AtomicU64,
    rtx_unparsed: AtomicU64,
    rtx_requested: AtomicU64,
    rtx_served: AtomicU64,
    rtx_missed: AtomicU64,
    rtx_late: AtomicU64,
    rtx_max_age_us: AtomicU64,
    /// Per packet, by sequence number modulo [`RECORDS`]: its deadline on the
    /// stream's timeline (what the sync packets announced for it) and when it
    /// really went out, in µs, each tagged with the sequence number in the
    /// top 16 bits. The two are judged separately, so a reader that sees one
    /// updated and not the other yet only skips that one.
    due: Box<[AtomicU64]>,
    went_out: Box<[AtomicU64]>,
}

impl Default for OutputCounters {
    fn default() -> Self {
        Self::new()
    }
}

fn records() -> Box<[AtomicU64]> {
    (0..RECORDS).map(|_| AtomicU64::new(EMPTY)).collect()
}

fn tagged(sequence: u16, ns: u64) -> u64 {
    (u64::from(sequence) << 48) | ((ns / 1_000) & TIME_MASK)
}

/// The time recorded for `sequence`, in µs, if its slot still holds it.
fn recorded(records: &[AtomicU64], sequence: u16) -> Option<u64> {
    let record = records[usize::from(sequence) % RECORDS].load(Ordering::Relaxed);
    (record != EMPTY && (record >> 48) as u16 == sequence).then_some(record & TIME_MASK)
}

impl OutputCounters {
    pub fn new() -> Self {
        Self {
            sent: AtomicU64::new(0),
            late: AtomicU64::new(0),
            max_late_us: AtomicU64::new(0),
            rtx_requests: AtomicU64::new(0),
            rtx_unparsed: AtomicU64::new(0),
            rtx_requested: AtomicU64::new(0),
            rtx_served: AtomicU64::new(0),
            rtx_missed: AtomicU64::new(0),
            rtx_late: AtomicU64::new(0),
            rtx_max_age_us: AtomicU64::new(0),
            due: records(),
            went_out: records(),
        }
    }

    /// The sender thread sent the audio packet `sequence`, due at
    /// `deadline_ns`, at `sent_ns` (both on `timeline::mono_now_ns`).
    /// Packets are `packet_ns` apart.
    pub fn record_send(&self, sequence: u16, deadline_ns: u64, sent_ns: u64, packet_ns: u64) {
        let late_ns = sent_ns.saturating_sub(deadline_ns);
        self.sent.fetch_add(1, Ordering::Relaxed);
        if late_ns > packet_ns {
            self.late.fetch_add(1, Ordering::Relaxed);
        }
        self.max_late_us.fetch_max(late_ns / 1_000, Ordering::Relaxed);
        let slot = usize::from(sequence) % RECORDS;
        self.due[slot].store(tagged(sequence, deadline_ns), Ordering::Relaxed);
        self.went_out[slot].store(tagged(sequence, sent_ns), Ordering::Relaxed);
    }

    /// A retransmit request came in at `now_ns`, and `served` of its packets
    /// went out again (`None`: the resend failed). The sync packets announce
    /// that a packet plays `latency_ns` after its deadline.
    pub fn record_retransmit(
        &self,
        request: &RetransmitRequest,
        served: Option<u16>,
        now_ns: u64,
        latency_ns: u64,
    ) {
        let count = u64::from(request.count);
        let served = u64::from(served.unwrap_or(0)).min(count);
        self.rtx_requests.fetch_add(1, Ordering::Relaxed);
        self.rtx_requested.fetch_add(count, Ordering::Relaxed);
        self.rtx_served.fetch_add(served, Ordering::Relaxed);
        self.rtx_missed.fetch_add(count - served, Ordering::Relaxed);

        let now_us = (now_ns / 1_000) & TIME_MASK;
        let latency_us = latency_ns / 1_000;
        let mut late = 0;
        for offset in 0..request.count {
            let sequence = request.first_sequence.wrapping_add(offset);
            // Late: the time announced for it has passed, however late it
            // really went out.
            if recorded(&self.due, sequence).is_some_and(|due| now_us >= due + latency_us) {
                late += 1;
            }
            // Its age: since it really went out.
            if let Some(went_out) = recorded(&self.went_out, sequence) {
                self.rtx_max_age_us
                    .fetch_max(now_us.saturating_sub(went_out), Ordering::Relaxed);
            }
        }
        self.rtx_late.fetch_add(late, Ordering::Relaxed);
    }

    /// The control channel received a retransmit request it could not parse.
    pub fn note_unparsed(&self) {
        self.rtx_unparsed.fetch_add(1, Ordering::Relaxed);
    }

    /// The counts since the previous call and the maxima over that time; the
    /// next period starts at zero. The packet records are kept.
    pub fn take(&self) -> OutputStats {
        let take = |counter: &AtomicU64| counter.swap(0, Ordering::Relaxed);
        OutputStats {
            packets_sent: take(&self.sent),
            late_packets: take(&self.late),
            max_late_us: take(&self.max_late_us),
            rtx_requests: take(&self.rtx_requests),
            rtx_unparsed: take(&self.rtx_unparsed),
            rtx_requested: take(&self.rtx_requested),
            rtx_served: take(&self.rtx_served),
            rtx_missed: take(&self.rtx_missed),
            rtx_late: take(&self.rtx_late),
            rtx_max_age_us: take(&self.rtx_max_age_us),
        }
    }
}
