//! Follows the receiver's PTP clock for buffered streams (local addition, see
//! VENDORED.md).
//!
//! A buffered stream (type 103) is anchored to a PTP timeline: the sender
//! tells the receiver "the frame with RTP timestamp X plays at network time T
//! on the timeline of clock C" (`SETRATEANCHORTIME`), and the receiver plays
//! from there on that clock. The sender therefore has to know which timeline
//! the receiver follows (the grandmaster's clock identity) and what time it is
//! on it.
//!
//! This module follows the receiver's clock the way a Mac does with a HomePod:
//! it introduces itself with a few Sync and Announce messages that rank worse
//! than the HomePod (priority1 250 against 248), so the HomePod stays the
//! grandmaster, then tells it to use its default intervals and listens. Every
//! Sync/Follow_Up pair from the grandmaster gives one forward sample, every
//! Delay_Req/Delay_Resp pair one reverse sample.
//!
//! The estimate is built for Wi-Fi, where a message can be held up for
//! hundreds of milliseconds but never sped up: over a window of a few seconds,
//! the fastest Sync bounds the offset from below and the fastest Delay_Req
//! from above, whatever the spikes in between ([`OffsetEstimator`]). The
//! published clock only slews toward that estimate, at most 500 ppm, so it is
//! continuous and runs within 500 ppm of the local clock ([`ClockModel`]).
//!
//! Times on this machine are `airplay_audio::timeline::mono_now_ns` (the
//! monotonic clock), the same clock the rest of the sender uses.
//!
//! Public only so the host crate's tests can drive it against a fake
//! grandmaster.

use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use airplay_audio::timeline::mono_now_ns;

use crate::media_clock::MediaClock;

/// PTP event port (Sync, Delay_Req).
pub const PTP_EVENT_PORT: u16 = 319;
/// PTP general port (Follow_Up, Delay_Resp, Announce, Signaling).
pub const PTP_GENERAL_PORT: u16 = 320;

/// Window over which the fastest messages are taken.
pub const ESTIMATE_WINDOW: Duration = Duration::from_secs(4);
/// The published clock moves toward the estimate at most this fast.
pub const SLEW_PPM: i128 = 500;
/// An estimate this far from the published clock...
pub const DIVERGENCE_LIMIT_NS: i128 = 50_000_000;
/// ...for this long means the receiver's timeline jumped.
pub const DIVERGENCE_PERSIST: Duration = Duration::from_secs(3);
/// Forward samples (about one second of Syncs) before the clock is trusted.
pub const LOCK_MIN_SYNCS: usize = 8;
/// No Follow_Up for this long, once locked, means the clock is lost.
pub const SILENCE_LIMIT: Duration = Duration::from_secs(15);
/// Our priority1: worse than a HomePod (248), as a Mac announces itself.
pub const FOLLOWER_PRIORITY1: u8 = 250;
/// How long Syncs may come without an Announce before the follower locks on
/// the Sync's sender as the timeline.
const ANNOUNCE_WAIT: Duration = Duration::from_secs(3);
/// Interval of the introductory Syncs (logSyncInterval -3).
const INTRO_INTERVAL: Duration = Duration::from_millis(125);
/// Receive timeout of the socket threads, so they notice `stop`.
const RECV_POLL: Duration = Duration::from_millis(100);
/// Two-step Syncs and Delay_Reqs remembered while waiting for their pair.
const PENDING_LIMIT: usize = 32;

/// PTP messages: wire format (IEEE 1588-2008, the parts AirPlay uses).
///
/// What we send matches, byte for byte, what the pinned upstream
/// `airplay-timing` sends in its "BMCA yield" flow (the flow upstream used
/// with HomePods), apart from the timestamps; `tests/ptp_follower.rs` checks
/// it.
pub mod wire {
    pub const SYNC: u8 = 0x0;
    pub const DELAY_REQ: u8 = 0x1;
    pub const FOLLOW_UP: u8 = 0x8;
    pub const DELAY_RESP: u8 = 0x9;
    pub const ANNOUNCE: u8 = 0xB;
    pub const SIGNALING: u8 = 0xC;

    pub const HEADER_LEN: usize = 34;
    /// `twoStepFlag` in the header flags.
    pub const TWO_STEP: u16 = 0x0200;

    const ORG_EXTENSION: u16 = 0x0003;
    const ORG_IEEE_802_1AS: [u8; 3] = [0x00, 0x80, 0xC2];
    const ORG_APPLE: [u8; 3] = [0x00, 0x0D, 0x93];

    /// A clock identity and port number.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PortIdentity {
        pub clock: [u8; 8],
        pub port: u16,
    }

    impl PortIdentity {
        fn write(&self, out: &mut Vec<u8>) {
            out.extend_from_slice(&self.clock);
            out.extend_from_slice(&self.port.to_be_bytes());
        }

        fn read(bytes: &[u8]) -> Self {
            let mut clock = [0u8; 8];
            clock.copy_from_slice(&bytes[..8]);
            Self {
                clock,
                port: u16::from_be_bytes([bytes[8], bytes[9]]),
            }
        }
    }

    /// The common message header.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Header {
        pub message_type: u8,
        pub length: u16,
        pub domain: u8,
        pub flags: u16,
        /// In ns × 2^16.
        pub correction: i64,
        pub source: PortIdentity,
        pub sequence: u16,
        pub control: u8,
        pub log_interval: i8,
    }

    impl Header {
        /// A header for `message_type` with the usual control field.
        pub fn new(message_type: u8, source: PortIdentity, sequence: u16) -> Self {
            let control = match message_type {
                SYNC => 0,
                DELAY_REQ => 1,
                FOLLOW_UP => 2,
                DELAY_RESP => 3,
                _ => 5,
            };
            Self {
                message_type,
                length: 0,
                domain: 0,
                flags: 0,
                correction: 0,
                source,
                sequence,
                control,
                log_interval: 0,
            }
        }

        /// The correction field in ns.
        pub fn correction_ns(&self) -> i128 {
            i128::from(self.correction) >> 16
        }

        fn write(&self, out: &mut Vec<u8>) {
            out.push(self.message_type & 0x0F);
            out.push(0x02);
            out.extend_from_slice(&self.length.to_be_bytes());
            out.push(self.domain);
            out.push(0);
            out.extend_from_slice(&self.flags.to_be_bytes());
            out.extend_from_slice(&self.correction.to_be_bytes());
            out.extend_from_slice(&[0; 4]);
            self.source.write(out);
            out.extend_from_slice(&self.sequence.to_be_bytes());
            out.push(self.control);
            out.push(self.log_interval as u8);
        }

        /// Parses a header (`None` if too short or not PTP version 2).
        pub fn parse(bytes: &[u8]) -> Option<Self> {
            if bytes.len() < HEADER_LEN || bytes[1] & 0x0F != 2 {
                return None;
            }
            let mut correction = [0u8; 8];
            correction.copy_from_slice(&bytes[8..16]);
            Some(Self {
                message_type: bytes[0] & 0x0F,
                length: u16::from_be_bytes([bytes[2], bytes[3]]),
                domain: bytes[4],
                flags: u16::from_be_bytes([bytes[6], bytes[7]]),
                correction: i64::from_be_bytes(correction),
                source: PortIdentity::read(&bytes[20..30]),
                sequence: u16::from_be_bytes([bytes[30], bytes[31]]),
                control: bytes[32],
                log_interval: bytes[33] as i8,
            })
        }
    }

    /// Writes a PTP timestamp (48-bit seconds, 32-bit nanoseconds).
    pub fn write_timestamp(out: &mut Vec<u8>, ns: u128) {
        let seconds = (ns / 1_000_000_000) as u64;
        let nanos = (ns % 1_000_000_000) as u32;
        out.extend_from_slice(&seconds.to_be_bytes()[2..]);
        out.extend_from_slice(&nanos.to_be_bytes());
    }

    /// Reads a PTP timestamp, in ns.
    pub fn read_timestamp(bytes: &[u8]) -> u128 {
        let mut seconds = [0u8; 8];
        seconds[2..].copy_from_slice(&bytes[..6]);
        let nanos = u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
        u128::from(u64::from_be_bytes(seconds)) * 1_000_000_000 + u128::from(nanos)
    }

    /// The grandmaster data set an Announce carries.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AnnounceBody {
        pub utc_offset: i16,
        pub priority1: u8,
        pub clock_class: u8,
        pub clock_accuracy: u8,
        pub variance: u16,
        pub priority2: u8,
        pub grandmaster: [u8; 8],
        pub steps_removed: u16,
        pub time_source: u8,
    }

    impl AnnounceBody {
        /// The data set we introduce ourselves with: it ranks below a
        /// HomePod's (priority1 248), like a Mac's.
        pub fn follower(identity: [u8; 8]) -> Self {
            Self {
                utc_offset: 37,
                priority1: super::FOLLOWER_PRIORITY1,
                clock_class: 248,
                clock_accuracy: 0xFE,
                variance: 0xFFFF,
                priority2: 128,
                grandmaster: identity,
                steps_removed: 0,
                time_source: 0xA0,
            }
        }

        /// Best master clock ranking: the lower ranks first.
        pub fn rank(&self) -> (u8, u8, u8, u16, u8, [u8; 8]) {
            (
                self.priority1,
                self.clock_class,
                self.clock_accuracy,
                self.variance,
                self.priority2,
                self.grandmaster,
            )
        }
    }

    /// A parsed message.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Message {
        Sync {
            header: Header,
            origin_ns: u128,
        },
        DelayReq {
            header: Header,
            origin_ns: u128,
        },
        FollowUp {
            header: Header,
            precise_origin_ns: u128,
        },
        DelayResp {
            header: Header,
            receive_ns: u128,
            requesting: PortIdentity,
        },
        Announce {
            header: Header,
            body: AnnounceBody,
        },
        Signaling {
            header: Header,
        },
    }

    /// Parses a message (`None` for anything else or anything malformed).
    pub fn parse(bytes: &[u8]) -> Option<Message> {
        let header = Header::parse(bytes)?;
        let body = &bytes[HEADER_LEN..];
        match header.message_type {
            SYNC if body.len() >= 10 => Some(Message::Sync {
                header,
                origin_ns: read_timestamp(body),
            }),
            DELAY_REQ if body.len() >= 10 => Some(Message::DelayReq {
                header,
                origin_ns: read_timestamp(body),
            }),
            FOLLOW_UP if body.len() >= 10 => Some(Message::FollowUp {
                header,
                precise_origin_ns: read_timestamp(body),
            }),
            DELAY_RESP if body.len() >= 20 => Some(Message::DelayResp {
                header,
                receive_ns: read_timestamp(body),
                requesting: PortIdentity::read(&body[10..20]),
            }),
            ANNOUNCE if body.len() >= 30 => {
                let mut grandmaster = [0u8; 8];
                grandmaster.copy_from_slice(&body[19..27]);
                Some(Message::Announce {
                    header,
                    body: AnnounceBody {
                        utc_offset: i16::from_be_bytes([body[10], body[11]]),
                        priority1: body[13],
                        clock_class: body[14],
                        clock_accuracy: body[15],
                        variance: u16::from_be_bytes([body[16], body[17]]),
                        priority2: body[18],
                        grandmaster,
                        steps_removed: u16::from_be_bytes([body[27], body[28]]),
                        time_source: body[29],
                    },
                })
            }
            SIGNALING => Some(Message::Signaling { header }),
            _ => None,
        }
    }

    fn finish(mut header: Header, body: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        let mut rest = Vec::new();
        body(&mut rest);
        header.length = (HEADER_LEN + rest.len()) as u16;
        let mut out = Vec::with_capacity(HEADER_LEN + rest.len());
        header.write(&mut out);
        out.extend_from_slice(&rest);
        out
    }

    fn tlv(out: &mut Vec<u8>, organization: [u8; 3], subtype: [u8; 3], payload: &[u8]) {
        out.extend_from_slice(&ORG_EXTENSION.to_be_bytes());
        out.extend_from_slice(&((6 + payload.len()) as u16).to_be_bytes());
        out.extend_from_slice(&organization);
        out.extend_from_slice(&subtype);
        out.extend_from_slice(payload);
    }

    /// Sync. A two-step Sync carries no timestamp: its Follow_Up does.
    pub fn sync(source: PortIdentity, sequence: u16, two_step: bool, origin_ns: u128) -> Vec<u8> {
        let mut header = Header::new(SYNC, source, sequence);
        if two_step {
            header.flags = TWO_STEP;
        }
        finish(header, |out| {
            write_timestamp(out, if two_step { 0 } else { origin_ns })
        })
    }

    /// Follow_Up with the 802.1AS Follow_Up information TLV.
    pub fn follow_up(
        source: PortIdentity,
        sequence: u16,
        precise_origin_ns: u128,
        correction: i64,
    ) -> Vec<u8> {
        let mut header = Header::new(FOLLOW_UP, source, sequence);
        header.correction = correction;
        finish(header, |out| {
            write_timestamp(out, precise_origin_ns);
            // cumulativeScaledRateOffset, gmTimeBaseIndicator,
            // lastGmPhaseChange, scaledLastGmFreqChange: all zero.
            tlv(out, ORG_IEEE_802_1AS, [0, 0, 1], &[0; 22]);
        })
    }

    /// Delay_Req, stamped with the time it is sent.
    pub fn delay_req(source: PortIdentity, sequence: u16, origin_ns: u128) -> Vec<u8> {
        finish(Header::new(DELAY_REQ, source, sequence), |out| {
            write_timestamp(out, origin_ns)
        })
    }

    /// Delay_Resp: when the grandmaster received `requesting`'s Delay_Req.
    pub fn delay_resp(
        source: PortIdentity,
        sequence: u16,
        receive_ns: u128,
        requesting: PortIdentity,
        correction: i64,
    ) -> Vec<u8> {
        let mut header = Header::new(DELAY_RESP, source, sequence);
        header.correction = correction;
        finish(header, |out| {
            write_timestamp(out, receive_ns);
            requesting.write(out);
        })
    }

    /// Announce.
    pub fn announce(
        source: PortIdentity,
        sequence: u16,
        body: &AnnounceBody,
        log_interval: i8,
    ) -> Vec<u8> {
        let mut header = Header::new(ANNOUNCE, source, sequence);
        header.log_interval = log_interval;
        finish(header, |out| {
            write_timestamp(out, 0);
            out.extend_from_slice(&body.utc_offset.to_be_bytes());
            out.push(0);
            out.push(body.priority1);
            out.push(body.clock_class);
            out.push(body.clock_accuracy);
            out.extend_from_slice(&body.variance.to_be_bytes());
            out.push(body.priority2);
            out.extend_from_slice(&body.grandmaster);
            out.extend_from_slice(&body.steps_removed.to_be_bytes());
            out.push(body.time_source);
        })
    }

    fn signaling(source: PortIdentity, sequence: u16, tlvs: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        let header = Header::new(SIGNALING, source, sequence);
        finish(header, |out| {
            // targetPortIdentity: all ports.
            out.extend_from_slice(&[0xFF; 10]);
            tlvs(out);
        })
    }

    /// The Signaling a Mac sends when it meets a HomePod: an 802.1AS message
    /// interval request (Sync and link delay every 125 ms, Announce every
    /// 250 ms) and Apple's two TLVs, with the values a Mac uses.
    pub fn signaling_mac_style(source: PortIdentity, sequence: u16) -> Vec<u8> {
        signaling(source, sequence, |out| {
            tlv(
                out,
                ORG_IEEE_802_1AS,
                [0, 0, 2],
                &[(-3i8) as u8, (-3i8) as u8, (-2i8) as u8, 0x02],
            );
            tlv(
                out,
                ORG_APPLE,
                [0, 0, 1],
                &[
                    0x00, 0x03, 0, 0, 0, 0, 0x27, 0x10, 0, 0, 0x27, 0x10, 0, 0, 0, 0,
                ],
            );
            let mut extended = [0u8; 26];
            extended[..12].copy_from_slice(&[0x00, 0x0F, 0, 0, 0, 0, 0x27, 0x10, 0, 0, 0x27, 0x10]);
            tlv(out, ORG_APPLE, [0, 0, 5], &extended);
        })
    }

    /// The Signaling a Mac sends once it yields to a better grandmaster: a
    /// message interval request back to the initial intervals.
    pub fn signaling_initial_intervals(source: PortIdentity, sequence: u16) -> Vec<u8> {
        signaling(source, sequence, |out| {
            tlv(out, ORG_IEEE_802_1AS, [0, 0, 2], &[0x7E, 0x7E, 0x7E, 0x00]);
        })
    }
}

/// Bounds the offset (grandmaster time minus local time) with the fastest
/// messages of a recent window.
///
/// A Sync sent at grandmaster time `t1` and received at local time `t2`
/// gives `t1 - t2 = offset - delay`: a lower bound, tight for the fastest
/// Sync. A Delay_Req sent at `t3` and received at grandmaster time `t4` gives
/// `t4 - t3 = offset + delay`: an upper bound. The estimate is the middle of
/// the tightest bounds (or the lower bound alone while there is no Delay_Resp).
#[derive(Debug, Clone)]
pub struct OffsetEstimator {
    window_ns: u64,
    forward: VecDeque<(u64, i128)>,
    reverse: VecDeque<(u64, i128)>,
}

/// What the window says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    /// Grandmaster time minus local time, in ns.
    pub offset_ns: i128,
    /// The tightest lower bound (fastest Sync).
    pub lower_ns: i128,
    /// The tightest upper bound (fastest Delay_Req), if any.
    pub upper_ns: Option<i128>,
}

impl Estimate {
    /// Round-trip of the fastest messages (upper minus lower bound).
    pub fn round_trip_ns(&self) -> Option<i128> {
        self.upper_ns.map(|upper| upper - self.lower_ns)
    }
}

impl OffsetEstimator {
    pub fn new(window: Duration) -> Self {
        Self {
            window_ns: window.as_nanos() as u64,
            forward: VecDeque::new(),
            reverse: VecDeque::new(),
        }
    }

    /// A Sync received at `local_rx_ns`, sent at `master_tx_ns`.
    pub fn add_forward(&mut self, local_rx_ns: u64, master_tx_ns: i128) {
        self.forward
            .push_back((local_rx_ns, master_tx_ns - i128::from(local_rx_ns)));
        self.prune(local_rx_ns);
    }

    /// A Delay_Req sent at `local_tx_ns`, received at `master_rx_ns`.
    pub fn add_reverse(&mut self, local_tx_ns: u64, master_rx_ns: i128) {
        self.reverse
            .push_back((local_tx_ns, master_rx_ns - i128::from(local_tx_ns)));
        self.prune(local_tx_ns);
    }

    fn prune(&mut self, now_ns: u64) {
        let oldest = now_ns.saturating_sub(self.window_ns);
        while self.forward.front().is_some_and(|&(at, _)| at < oldest) {
            self.forward.pop_front();
        }
        while self.reverse.front().is_some_and(|&(at, _)| at < oldest) {
            self.reverse.pop_front();
        }
    }

    /// Forward samples in the window.
    pub fn forward_samples(&self) -> usize {
        self.forward.len()
    }

    /// Reverse samples in the window.
    pub fn reverse_samples(&self) -> usize {
        self.reverse.len()
    }

    /// The estimate from the window (`None` without any Sync).
    pub fn estimate(&self) -> Option<Estimate> {
        let lower = self.forward.iter().map(|&(_, bound)| bound).max()?;
        let upper = self.reverse.iter().map(|&(_, bound)| bound).min();
        let offset = match upper {
            Some(upper) => lower + (upper - lower) / 2,
            None => lower,
        };
        Some(Estimate {
            offset_ns: offset,
            lower_ns: lower,
            upper_ns: upper,
        })
    }
}

/// The published offset: set by the first estimate, then slewed toward each
/// new one at most [`SLEW_PPM`], so the clock built on it never jumps.
#[derive(Debug, Clone)]
pub struct ClockModel {
    offset_ns: Option<i128>,
    updated_ns: u64,
    diverging_since: Option<u64>,
    divergence_persist_ns: u64,
}

impl Default for ClockModel {
    fn default() -> Self {
        Self::new(DIVERGENCE_PERSIST)
    }
}

/// What an update did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelUpdate {
    /// First estimate: the offset is set.
    Locked,
    /// The offset moved toward the estimate.
    Tracking,
    /// The estimate stayed more than [`DIVERGENCE_LIMIT_NS`] away for
    /// [`DIVERGENCE_PERSIST`]: the timeline jumped.
    Diverged { error_ns: i128 },
}

impl ClockModel {
    /// A model that reports a divergence lasting `divergence_persist`.
    pub fn new(divergence_persist: Duration) -> Self {
        Self {
            offset_ns: None,
            updated_ns: 0,
            diverging_since: None,
            divergence_persist_ns: divergence_persist.as_nanos() as u64,
        }
    }

    /// The published offset, once locked.
    pub fn offset_ns(&self) -> Option<i128> {
        self.offset_ns
    }

    /// Takes the estimate `measured_ns` made at local time `now_ns`.
    pub fn update(&mut self, now_ns: u64, measured_ns: i128) -> ModelUpdate {
        let Some(offset) = self.offset_ns else {
            self.offset_ns = Some(measured_ns);
            self.updated_ns = now_ns;
            return ModelUpdate::Locked;
        };
        let elapsed = i128::from(now_ns.saturating_sub(self.updated_ns));
        let max_step = elapsed * SLEW_PPM / 1_000_000;
        let error = measured_ns - offset;
        self.offset_ns = Some(offset + error.clamp(-max_step, max_step));
        self.updated_ns = now_ns;

        if error.abs() > DIVERGENCE_LIMIT_NS {
            let since = *self.diverging_since.get_or_insert(now_ns);
            if now_ns.saturating_sub(since) >= self.divergence_persist_ns {
                return ModelUpdate::Diverged { error_ns: error };
            }
        } else {
            self.diverging_since = None;
        }
        ModelUpdate::Tracking
    }
}

/// Where the follower talks PTP.
#[derive(Debug, Clone)]
pub struct FollowerConfig {
    /// The receiver (the grandmaster we follow).
    pub master: IpAddr,
    /// Our ports. The receiver sends to 319 and 320; tests use 0.
    pub local_event_port: u16,
    pub local_general_port: u16,
    /// The receiver's ports.
    pub master_event_port: u16,
    pub master_general_port: u16,
    /// Our clock identity.
    pub identity: [u8; 8],
    /// No Follow_Up for this long, once locked, loses the clock.
    pub silence_limit: Duration,
    /// An estimate off by more than [`DIVERGENCE_LIMIT_NS`] for this long
    /// loses the clock.
    pub divergence_persist: Duration,
}

impl FollowerConfig {
    /// The standard ports, a random clock identity.
    pub fn new(master: IpAddr) -> Self {
        Self {
            master,
            local_event_port: PTP_EVENT_PORT,
            local_general_port: PTP_GENERAL_PORT,
            master_event_port: PTP_EVENT_PORT,
            master_general_port: PTP_GENERAL_PORT,
            identity: random_identity(),
            silence_limit: SILENCE_LIMIT,
            divergence_persist: DIVERGENCE_PERSIST,
        }
    }
}

fn random_identity() -> [u8; 8] {
    use rand::Rng;
    let mut identity = [0u8; 8];
    rand::thread_rng().fill(&mut identity);
    // Not all-zero nor all-ones, which mean "none" and "all".
    identity[0] = identity[0].clamp(1, 0xFE);
    identity
}

/// What the follower knows, for logs and checks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FollowerStatus {
    /// The published clock is valid.
    pub locked: bool,
    /// The timeline the receiver plays on: the grandmaster's clock identity.
    pub timeline_id: Option<u64>,
    /// The grandmaster's Announce, if one came.
    pub grandmaster: Option<wire::AnnounceBody>,
    /// Published offset (grandmaster minus local), in ns.
    pub offset_ns: Option<i128>,
    /// Round-trip of the fastest messages in the window, in ns.
    pub round_trip_ns: Option<i128>,
    /// Samples in the window.
    pub forward_samples: usize,
    pub reverse_samples: usize,
    /// Messages handled since the start.
    pub syncs: u64,
    pub delay_responses: u64,
    /// Why the clock is no longer valid, if it is not.
    pub lost: Option<String>,
}

struct PendingSync {
    sequence: u16,
    source: wire::PortIdentity,
    received_ns: u64,
    correction_ns: i128,
}

#[derive(Default)]
struct State {
    estimator: Option<OffsetEstimator>,
    model: ClockModel,
    pending_syncs: VecDeque<PendingSync>,
    pending_delay_reqs: VecDeque<(u16, u64)>,
    delay_req_sequence: u16,
    announce: Option<wire::AnnounceBody>,
    sync_source: Option<[u8; 8]>,
    timeline_id: Option<u64>,
    first_follow_up_ns: Option<u64>,
    last_follow_up_ns: Option<u64>,
    syncs: u64,
    delay_responses: u64,
    lost: Option<String>,
}

/// The offset clock readings use: it moves from `offset_ns` (at local time
/// `local_ns`) toward the model's `target_ns` at [`SLEW_PPM`], so the clock is
/// continuous and its rate stays within 500 ppm of the local clock's.
#[derive(Debug, Clone, Copy, Default)]
struct ClockLine {
    local_ns: u64,
    offset_ns: i128,
    target_ns: i128,
}

impl ClockLine {
    fn offset_at(&self, local_ns: u64) -> i128 {
        let elapsed = i128::from(local_ns.saturating_sub(self.local_ns));
        let reach = elapsed * SLEW_PPM / 1_000_000;
        self.offset_ns + (self.target_ns - self.offset_ns).clamp(-reach, reach)
    }
}

struct Shared {
    config: FollowerConfig,
    identity: wire::PortIdentity,
    state: Mutex<State>,
    /// What clock readings are made of (valid once `locked`).
    line: Mutex<ClockLine>,
    locked: AtomicBool,
    /// Last clock reading handed out: readings never go backwards.
    last_reading_ns: AtomicU64,
    running: AtomicBool,
    /// Sequence numbers of what we introduce ourselves with.
    sync_sequence: AtomicU16,
    signaling_sequence: AtomicU16,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn line(&self) -> MutexGuard<'_, ClockLine> {
        self.line
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now_ns(&self) -> u64 {
        let line = *self.line();
        let local = mono_now_ns();
        let now = (i128::from(local) + line.offset_at(local)).clamp(0, i128::from(u64::MAX)) as u64;
        let previous = self.last_reading_ns.fetch_max(now, Ordering::AcqRel);
        previous.max(now)
    }

    fn from_master(&self, from: &SocketAddr) -> bool {
        canonical(from.ip()) == canonical(self.config.master)
    }

    fn lose(&self, state: &mut State, reason: String) {
        if state.lost.is_none() {
            tracing::warn!(reason = %reason, "PTP: lost the speaker's clock");
            state.lost = Some(reason);
        }
    }

    /// Handles one datagram from either socket, received at `received_ns`.
    fn handle(&self, data: &[u8], from: SocketAddr, received_ns: u64, event: &UdpSocket) {
        if !self.from_master(&from) {
            tracing::trace!(%from, "PTP: ignoring a message from another peer");
            return;
        }
        let Some(message) = wire::parse(data) else {
            return;
        };
        let mut state = self.state();
        match message {
            wire::Message::Sync { header, origin_ns } => {
                state.syncs += 1;
                if header.flags & wire::TWO_STEP != 0 {
                    if state.pending_syncs.len() >= PENDING_LIMIT {
                        state.pending_syncs.pop_front();
                    }
                    state.pending_syncs.push_back(PendingSync {
                        sequence: header.sequence,
                        source: header.source,
                        received_ns,
                        correction_ns: header.correction_ns(),
                    });
                } else {
                    let sent = origin_ns as i128 + header.correction_ns();
                    self.forward_sample(&mut state, header.source, received_ns, sent, event);
                }
            }
            wire::Message::FollowUp {
                header,
                precise_origin_ns,
            } => {
                let position = state.pending_syncs.iter().position(|sync| {
                    sync.sequence == header.sequence && sync.source == header.source
                });
                if let Some(sync) = position.and_then(|index| state.pending_syncs.remove(index)) {
                    let sent =
                        precise_origin_ns as i128 + sync.correction_ns + header.correction_ns();
                    self.forward_sample(&mut state, header.source, sync.received_ns, sent, event);
                }
            }
            wire::Message::DelayResp {
                header,
                receive_ns,
                requesting,
            } => {
                if requesting != self.identity {
                    return;
                }
                let position = state
                    .pending_delay_reqs
                    .iter()
                    .position(|&(sequence, _)| sequence == header.sequence);
                if let Some((_, sent_ns)) =
                    position.and_then(|index| state.pending_delay_reqs.remove(index))
                {
                    state.delay_responses += 1;
                    let received = receive_ns as i128 - header.correction_ns();
                    if let Some(estimator) = state.estimator.as_mut() {
                        estimator.add_reverse(sent_ns, received);
                    }
                }
            }
            wire::Message::Announce { body, .. } => {
                if let Some(timeline) = state.timeline_id {
                    if u64::from_be_bytes(body.grandmaster) != timeline {
                        let reason = format!(
                            "the speaker's PTP grandmaster changed from {timeline:016x} to {}",
                            hex(&body.grandmaster)
                        );
                        self.lose(&mut state, reason);
                    }
                }
                if state.announce.is_none() {
                    tracing::info!(
                        grandmaster = %hex(&body.grandmaster),
                        priority1 = body.priority1,
                        clock_class = body.clock_class,
                        steps_removed = body.steps_removed,
                        "PTP: speaker announced its clock"
                    );
                }
                state.announce = Some(body);
            }
            wire::Message::DelayReq { .. } | wire::Message::Signaling { .. } => {}
        }
    }

    /// A Sync and its time are paired: take the sample, ask for the reverse
    /// path, and update the clock.
    fn forward_sample(
        &self,
        state: &mut State,
        source: wire::PortIdentity,
        received_ns: u64,
        sent_ns: i128,
        event: &UdpSocket,
    ) {
        state.first_follow_up_ns.get_or_insert(received_ns);
        state.last_follow_up_ns = Some(received_ns);
        state.sync_source.get_or_insert(source.clock);
        state
            .estimator
            .get_or_insert_with(|| OffsetEstimator::new(ESTIMATE_WINDOW))
            .add_forward(received_ns, sent_ns);
        self.send_delay_req(state, event);
        self.update_model(state, received_ns);
    }

    fn send_delay_req(&self, state: &mut State, event: &UdpSocket) {
        state.delay_req_sequence = state.delay_req_sequence.wrapping_add(1);
        let sequence = state.delay_req_sequence;
        let sent_ns = mono_now_ns();
        let packet = wire::delay_req(self.identity, sequence, u128::from(sent_ns));
        let to = SocketAddr::new(self.config.master, self.config.master_event_port);
        match event.send_to(&packet, to) {
            Ok(_) => {
                if state.pending_delay_reqs.len() >= PENDING_LIMIT {
                    state.pending_delay_reqs.pop_front();
                }
                state.pending_delay_reqs.push_back((sequence, sent_ns));
            }
            Err(error) => tracing::debug!(%error, "PTP: cannot send Delay_Req"),
        }
    }

    fn update_model(&self, state: &mut State, now_ns: u64) {
        let Some(estimate) = state.estimator.as_ref().and_then(OffsetEstimator::estimate) else {
            return;
        };
        if state.model.offset_ns().is_none() {
            // Lock on a second of Syncs, and on the grandmaster's Announce
            // (the timeline is its identity) unless none came for a while.
            let forward = state
                .estimator
                .as_ref()
                .map_or(0, OffsetEstimator::forward_samples);
            let listening = state
                .first_follow_up_ns
                .map_or(0, |first| now_ns.saturating_sub(first));
            let announced =
                state.announce.is_some() || listening >= ANNOUNCE_WAIT.as_nanos() as u64;
            if forward < LOCK_MIN_SYNCS || !announced {
                return;
            }
        }
        match state.model.update(now_ns, estimate.offset_ns) {
            ModelUpdate::Locked => {
                let timeline = state
                    .announce
                    .map(|announce| announce.grandmaster)
                    .or(state.sync_source)
                    .map(u64::from_be_bytes);
                state.timeline_id = timeline;
                tracing::info!(
                    timeline = %timeline.map(|id| format!("{id:016x}")).unwrap_or_default(),
                    offset_ms = estimate.offset_ns as f64 / 1e6,
                    round_trip_ms = estimate.round_trip_ns().map(|ns| ns as f64 / 1e6),
                    "PTP: following the speaker's clock"
                );
            }
            ModelUpdate::Tracking => {}
            ModelUpdate::Diverged { error_ns } => {
                let reason = format!(
                    "the speaker's PTP clock jumped by {:.1} ms",
                    error_ns as f64 / 1e6
                );
                self.lose(state, reason);
            }
        }
        if let Some(target) = state.model.offset_ns() {
            let local = mono_now_ns();
            let mut line = self.line();
            // The first estimate is taken whole; later ones are slewed to.
            let offset = if self.locked.load(Ordering::Acquire) {
                line.offset_at(local)
            } else {
                target
            };
            *line = ClockLine {
                local_ns: local,
                offset_ns: offset,
                target_ns: target,
            };
            drop(line);
            if state.timeline_id.is_some() {
                self.locked.store(true, Ordering::Release);
            }
        }
    }
}

/// Follows the receiver's PTP clock from two UDP sockets on their own threads.
pub struct PtpFollower {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    introductions: Mutex<Vec<JoinHandle<()>>>,
    event: UdpSocket,
    general: UdpSocket,
    event_addr: SocketAddr,
    general_addr: SocketAddr,
}

impl PtpFollower {
    /// Binds the PTP ports and listens. Fails if the ports cannot be bound
    /// (319 and 320 need `CAP_NET_BIND_SERVICE` or root on Linux, and no
    /// other PTP daemon on this machine). Call [`Self::introduce`] once the
    /// receiver knows us as a timing peer.
    pub fn start(config: FollowerConfig) -> io::Result<Self> {
        let any: IpAddr = match config.master {
            IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
            IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
        };
        let bind = |port: u16| {
            UdpSocket::bind(SocketAddr::new(any, port)).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot bind UDP port {port} for PTP: {error}"),
                )
            })
        };
        let event = bind(config.local_event_port)?;
        let general = bind(config.local_general_port)?;
        event.set_read_timeout(Some(RECV_POLL))?;
        general.set_read_timeout(Some(RECV_POLL))?;
        let event_addr = event.local_addr()?;
        let general_addr = general.local_addr()?;

        let identity = wire::PortIdentity {
            clock: config.identity,
            port: 1,
        };
        let state = State {
            model: ClockModel::new(config.divergence_persist),
            ..State::default()
        };
        let shared = Arc::new(Shared {
            config,
            identity,
            state: Mutex::new(state),
            line: Mutex::new(ClockLine::default()),
            locked: AtomicBool::new(false),
            last_reading_ns: AtomicU64::new(0),
            running: AtomicBool::new(true),
            sync_sequence: AtomicU16::new(0),
            signaling_sequence: AtomicU16::new(0),
        });

        let mut threads = Vec::new();
        for (name, socket) in [
            ("ptp-event", event.try_clone()?),
            ("ptp-general", general.try_clone()?),
        ] {
            let shared = Arc::clone(&shared);
            let event = event.try_clone()?;
            threads.push(
                thread::Builder::new()
                    .name(name.into())
                    .spawn(move || receive_loop(&shared, &socket, &event))?,
            );
        }
        {
            let shared = Arc::clone(&shared);
            let general = general.try_clone()?;
            threads.push(
                thread::Builder::new()
                    .name("ptp-watch".into())
                    .spawn(move || watch(&shared, &general))?,
            );
        }
        tracing::info!(
            master = %shared.config.master,
            event = %event_addr,
            general = %general_addr,
            identity = %hex(&shared.config.identity),
            "PTP: following the speaker's clock from these ports"
        );
        Ok(Self {
            shared,
            threads,
            introductions: Mutex::new(Vec::new()),
            event,
            general,
            event_addr,
            general_addr,
        })
    }

    /// Introduces us to the receiver the way a Mac does (three Syncs, two
    /// Announces ranking below a HomePod, a Signaling), in the background.
    /// Once the receiver announces a better clock, we yield to it.
    pub fn introduce(&self) {
        let shared = Arc::clone(&self.shared);
        let sockets = self
            .event
            .try_clone()
            .and_then(|event| Ok((event, self.general.try_clone()?)));
        let spawned = sockets.and_then(|(event, general)| {
            thread::Builder::new()
                .name("ptp-intro".into())
                .spawn(move || introduce(&shared, &event, &general))
        });
        match spawned {
            Ok(thread) => self
                .introductions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(thread),
            Err(error) => tracing::warn!(%error, "PTP: cannot introduce ourselves to the speaker"),
        }
    }

    /// Our event socket (Sync, Delay_Req).
    pub fn event_addr(&self) -> SocketAddr {
        self.event_addr
    }

    /// Our general socket (Follow_Up, Announce, ...).
    pub fn general_addr(&self) -> SocketAddr {
        self.general_addr
    }

    /// Our clock identity.
    pub fn identity(&self) -> [u8; 8] {
        self.shared.identity.clock
    }

    /// Whether the clock is valid (locked and not lost).
    pub fn is_locked(&self) -> bool {
        self.shared.locked.load(Ordering::Acquire) && self.shared.state().lost.is_none()
    }

    /// Waits until the clock is valid, at most `timeout`.
    pub async fn wait_until_locked(&self, timeout: Duration) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(reason) = self.lost() {
                return Err(reason);
            }
            if self.is_locked() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                let status = self.status();
                return Err(if status.syncs == 0 {
                    format!(
                        "no PTP Sync from the speaker within {} s (is UDP {} reachable from it?)",
                        timeout.as_secs(),
                        self.shared.config.local_event_port
                    )
                } else {
                    format!(
                        "the speaker's PTP clock did not settle within {} s ({} Syncs)",
                        timeout.as_secs(),
                        status.syncs
                    )
                });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Why the clock was lost, if it was.
    pub fn lost(&self) -> Option<String> {
        self.shared.state().lost.clone()
    }

    /// The timeline the receiver plays on (the grandmaster's clock identity).
    pub fn timeline_id(&self) -> Option<u64> {
        self.shared.state().timeline_id
    }

    /// The receiver's clock as seen from here. Only meaningful once locked.
    pub fn clock(&self) -> MediaClock {
        let shared = Arc::clone(&self.shared);
        MediaClock::from_fn(move || shared.now_ns())
    }

    /// What the follower knows.
    pub fn status(&self) -> FollowerStatus {
        let state = self.shared.state();
        let estimate = state.estimator.as_ref().and_then(OffsetEstimator::estimate);
        FollowerStatus {
            locked: self.shared.locked.load(Ordering::Acquire),
            timeline_id: state.timeline_id,
            grandmaster: state.announce,
            offset_ns: state.model.offset_ns(),
            round_trip_ns: estimate.and_then(|estimate| estimate.round_trip_ns()),
            forward_samples: state
                .estimator
                .as_ref()
                .map_or(0, OffsetEstimator::forward_samples),
            reverse_samples: state
                .estimator
                .as_ref()
                .map_or(0, OffsetEstimator::reverse_samples),
            syncs: state.syncs,
            delay_responses: state.delay_responses,
            lost: state.lost.clone(),
        }
    }

    /// Stops the threads and closes the sockets.
    pub fn stop(&mut self) {
        self.shared.running.store(false, Ordering::Release);
        let introductions = std::mem::take(
            &mut *self
                .introductions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for thread in self.threads.drain(..).chain(introductions) {
            let _ = thread.join();
        }
    }
}

impl Drop for PtpFollower {
    fn drop(&mut self) {
        self.stop();
    }
}

fn receive_loop(shared: &Shared, socket: &UdpSocket, event: &UdpSocket) {
    let mut buffer = [0u8; 512];
    while shared.running.load(Ordering::Acquire) {
        match socket.recv_from(&mut buffer) {
            Ok((length, from)) => {
                let received_ns = mono_now_ns();
                shared.handle(&buffer[..length], from, received_ns, event);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => {
                tracing::debug!(%error, "PTP: receive failed");
                thread::sleep(RECV_POLL);
            }
        }
    }
}

fn send(socket: &UdpSocket, packet: &[u8], to: SocketAddr) {
    if let Err(error) = socket.send_to(packet, to) {
        tracing::debug!(%error, %to, "PTP: send failed");
    }
}

/// Introduces us like a Mac does: three two-step Syncs with their
/// Follow_Ups, two Announces ranking below a HomePod, then the Mac's
/// Signaling (the same messages upstream's `run_bmca_yield_flow` sends).
fn introduce(shared: &Shared, event: &UdpSocket, general: &UdpSocket) {
    let config = &shared.config;
    let event_to = SocketAddr::new(config.master, config.master_event_port);
    let general_to = SocketAddr::new(config.master, config.master_general_port);
    let ours = wire::AnnounceBody::follower(config.identity);
    for round in 0..3 {
        if !shared.running.load(Ordering::Acquire) {
            return;
        }
        let sequence = shared
            .sync_sequence
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let sent = u128::from(mono_now_ns());
        send(
            event,
            &wire::sync(shared.identity, sequence, true, sent),
            event_to,
        );
        send(
            general,
            &wire::follow_up(shared.identity, sequence, sent, 0),
            general_to,
        );
        if round < 2 {
            send(
                general,
                &wire::announce(shared.identity, sequence, &ours, 1),
                general_to,
            );
        }
        thread::sleep(INTRO_INTERVAL);
    }
    let sequence = shared
        .signaling_sequence
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1);
    send(
        general,
        &wire::signaling_mac_style(shared.identity, sequence),
        general_to,
    );
}

/// Yields to the receiver's clock once it announces a better one, then
/// watches for silence.
fn watch(shared: &Shared, general: &UdpSocket) {
    let config = &shared.config;
    let general_to = SocketAddr::new(config.master, config.master_general_port);
    let ours = wire::AnnounceBody::follower(config.identity);
    let mut yielded = false;
    while shared.running.load(Ordering::Acquire) {
        let announce = shared.state().announce;
        if let (false, Some(theirs)) = (yielded, announce) {
            yielded = true;
            if theirs.rank() < ours.rank() {
                let sequence = shared
                    .signaling_sequence
                    .fetch_add(1, Ordering::AcqRel)
                    .wrapping_add(1);
                send(
                    general,
                    &wire::signaling_initial_intervals(shared.identity, sequence),
                    general_to,
                );
            } else {
                tracing::warn!(
                    their_priority1 = theirs.priority1,
                    our_priority1 = ours.priority1,
                    "PTP: the speaker's clock ranks below ours; it may not act as the grandmaster"
                );
            }
        }
        {
            let mut state = shared.state();
            let silent_for = state
                .last_follow_up_ns
                .map(|at| Duration::from_nanos(mono_now_ns().saturating_sub(at)));
            if shared.locked.load(Ordering::Acquire)
                && silent_for.is_some_and(|silent| silent >= config.silence_limit)
            {
                let reason = format!(
                    "no PTP Sync from the speaker for {:.1} s",
                    config.silence_limit.as_secs_f64()
                );
                shared.lose(&mut state, reason);
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// An IPv4-mapped IPv6 address as IPv4, so both forms compare equal.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
