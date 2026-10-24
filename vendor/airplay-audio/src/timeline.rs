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
//! `origin + n / rate`, and plays the announced latency later: its sync
//! packet, sent with it, carries the deadline and the latency (see
//! [`announced_latency`]). Decoding, encoding and scheduling delays change
//! when a packet is prepared, never when it is due or when it plays. Only a
//! packet that is hopelessly late (more than half the latency) moves the
//! timeline, once.

#[cfg(not(target_os = "linux"))]
use std::sync::OnceLock;
#[cfg(not(target_os = "linux"))]
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Packets the producer may queue ahead of the sender thread.
pub const SENDER_QUEUE: usize = 8;

/// Lateness at which the timeline is moved, at least.
const MIN_MAX_LATE: Duration = Duration::from_millis(20);

/// The latency the sync packets announce, in frames of the stream's RTP
/// clock: the render delay, kept within the bounds the stream declared at
/// SETUP (`latencyMin`, `latencyMax`), where a receiver would clamp it anyway.
///
/// A sync packet carries it as the difference of its two RTP timestamps
/// (bytes 16-19 minus bytes 4-7): the receiver plays a packet that long after
/// the time the sync packet sent with it carries, plus its own latency.
pub fn announced_latency(render_delay: Duration, rate: u32, min_frames: u32, max_frames: u32) -> u32 {
    let frames = (render_delay.as_nanos() * u128::from(rate) + 500_000_000) / 1_000_000_000;
    u32::try_from(frames)
        .unwrap_or(u32::MAX)
        .max(min_frames)
        .min(max_frames.max(min_frames))
}

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
    /// When the packet is due to be sent, on [`mono_now_ns`]. A sync packet
    /// sent with it carries this time (converted to the wall clock).
    pub deadline_ns: u64,
    /// When its first frame plays (before the receiver's own latency), on
    /// [`mono_now_ns`]: the deadline plus the announced latency.
    pub anchor_ns: i128,
    /// The timeline (re)started at this packet: send a sync packet with it.
    pub restarted: bool,
}

/// The timeline of one stream. Positions are in frames of the stream's RTP
/// clock (the encoder's running sample count).
#[derive(Debug, Clone)]
pub struct Timeline {
    rate: u64,
    /// The announced latency, in frames.
    latency_frames: u32,
    /// The same in ns: anchor minus send deadline.
    margin_ns: i128,
    max_late_ns: u64,
    /// Monotonic ns and stream position of the origin.
    origin: Option<(u64, u64)>,
    restarts: u64,
}

impl Timeline {
    /// `latency_frames` is what the sync packets announce (see
    /// [`announced_latency`]): a packet plays that long after its deadline.
    pub fn new(rate: u32, latency_frames: u32) -> Self {
        let rate = u64::from(rate.max(1));
        let latency_ns = u64::from(latency_frames) * 1_000_000_000 / rate;
        Self {
            rate,
            latency_frames,
            margin_ns: i128::from(latency_ns),
            max_late_ns: (latency_ns / 2).max(MIN_MAX_LATE.as_nanos() as u64),
            origin: None,
            restarts: 0,
        }
    }

    /// The slot of the packet starting at stream position `position`, prepared
    /// at `now_ns`. The first packet, the first after [`Self::restart`], and
    /// one prepared more than half the latency after its deadline start the
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

    /// The announced latency, in frames: what bytes 4-7 of a sync packet
    /// subtract from the RTP timestamp in bytes 16-19.
    pub fn latency_frames(&self) -> u32 {
        self.latency_frames
    }

    /// Anchor minus send deadline: the announced latency, in ns.
    pub fn margin(&self) -> i128 {
        self.margin_ns
    }

    /// How late a packet may be prepared before the timeline restarts: a
    /// restart moves the anchors by more than this.
    pub fn max_late_ns(&self) -> u64 {
        self.max_late_ns
    }
}
