//! The stream's timeline (local patch, see VENDORED.md).
//!
//! A sync packet tells the receiver when an RTP frame plays, and the receiver
//! plays the stream at the rate consecutive sync packets imply. Upstream read
//! the clock while preparing each sync packet, after decoding and encoding, so
//! every delay in that work moved the announced time, and so did every reset
//! of the sender thread's schedule: the receiver then resampled, which is
//! heard as speed and pitch moving.
//!
//! Here the timeline is a pure function of the stream position. The first
//! packet sets an origin; packet `n` frames later is due at
//! `origin + n / rate`, and its sync packet announces that time plus a fixed
//! margin. Decoding, encoding and scheduling delays change when a packet is
//! prepared, never when it is due or when it plays. Only a packet that is
//! hopelessly late (more than half the margin) moves the timeline, once.

#[cfg(not(target_os = "linux"))]
use std::sync::OnceLock;
#[cfg(not(target_os = "linux"))]
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Packets the producer may queue ahead of the sender thread.
pub const SENDER_QUEUE: usize = 8;

/// How far ahead of its send deadline upstream stamped a packet: the queue,
/// plus the packet the sender thread holds, plus the one being prepared. The
/// sync anchor keeps that offset so that the latency stays what it was.
const STAMP_LEAD_PACKETS: u64 = SENDER_QUEUE as u64 + 2;

/// Lateness at which the timeline is moved, at least.
const MIN_MAX_LATE: Duration = Duration::from_millis(20);

/// The monotonic clock the sender thread sleeps on, in ns.
pub fn mono_now_ns() -> u64 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `clock_gettime` only writes the `timespec` it is given.
        unsafe {
            let mut ts: libc::timespec = std::mem::zeroed();
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
            ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        static BASE: OnceLock<Instant> = OnceLock::new();
        BASE.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
}

/// The wall clock (which the NTP timing server reports) minus the monotonic
/// clock, in ns. Constant unless the wall clock is stepped.
pub fn wall_minus_mono_ns() -> i128 {
    let mut best: Option<(u64, i128)> = None;
    for _ in 0..4 {
        let before = mono_now_ns();
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos() as i128;
        let after = mono_now_ns();
        let window = after.saturating_sub(before);
        let offset = wall - i128::from(before + window / 2);
        if best.map_or(true, |(narrowest, _)| window < narrowest) {
            best = Some((window, offset));
        }
        // Not preempted between the reads: good to well under a microsecond.
        if window < 20_000 {
            break;
        }
    }
    best.map_or(0, |(_, offset)| offset)
}

/// Where a packet sits on the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// When the packet is due to be sent, on [`mono_now_ns`].
    pub deadline_ns: u64,
    /// When its first frame plays, on [`mono_now_ns`]: what its sync packet
    /// announces (converted to the wall clock).
    pub anchor_ns: i128,
    /// The timeline (re)started at this packet: send a sync packet with it.
    pub restarted: bool,
}

/// The timeline of one stream. Positions are in frames of the stream's RTP
/// clock (the encoder's running sample count).
#[derive(Debug, Clone)]
pub struct Timeline {
    rate: u64,
    /// Sync anchor minus send deadline.
    margin_ns: i128,
    max_late_ns: u64,
    /// Monotonic ns and stream position of the origin.
    origin: Option<(u64, u64)>,
    restarts: u64,
}

impl Timeline {
    /// `render_delay` is the setting of the same name, `frames_per_packet` the
    /// packet size, both as upstream used them.
    pub fn new(rate: u32, frames_per_packet: u32, render_delay: Duration) -> Self {
        let rate = u64::from(rate.max(1));
        let stamp_lead_ns =
            STAMP_LEAD_PACKETS * u64::from(frames_per_packet) * 1_000_000_000 / rate;
        let margin_ns = render_delay.as_nanos() as i128 - i128::from(stamp_lead_ns);
        let half_margin = u64::try_from(margin_ns / 2).unwrap_or(0);
        Self {
            rate,
            margin_ns,
            max_late_ns: half_margin.max(MIN_MAX_LATE.as_nanos() as u64),
            origin: None,
            restarts: 0,
        }
    }

    /// The slot of the packet starting at stream position `position`, prepared
    /// at `now_ns`. The first packet, the first after [`Self::restart`], and
    /// one prepared more than half the margin after its deadline start the
    /// timeline at `now_ns`.
    pub fn slot(&mut self, position: u64, now_ns: u64) -> Slot {
        if let Some((origin_ns, origin_position)) = self.origin {
            if let Some(frames) = position.checked_sub(origin_position) {
                let offset = u128::from(frames) * 1_000_000_000 / u128::from(self.rate);
                let deadline_ns = origin_ns.saturating_add(offset as u64);
                if now_ns <= deadline_ns.saturating_add(self.max_late_ns) {
                    return Slot {
                        deadline_ns,
                        anchor_ns: i128::from(deadline_ns) + self.margin_ns,
                        restarted: false,
                    };
                }
            }
            self.restarts += 1;
        }
        self.origin = Some((now_ns, position));
        Slot {
            deadline_ns: now_ns,
            anchor_ns: i128::from(now_ns) + self.margin_ns,
            restarted: true,
        }
    }

    /// The next packet starts a new timeline (after a pause).
    pub fn restart(&mut self) {
        self.origin = None;
    }

    /// Times the timeline was moved because a packet was too late.
    pub fn restarts(&self) -> u64 {
        self.restarts
    }

    /// Sync anchor minus send deadline.
    pub fn margin(&self) -> i128 {
        self.margin_ns
    }
}
