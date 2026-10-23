//! A fake PTP grandmaster on the loopback interface, behaving like a HomePod
//! with a Mac: Announce every 250 ms, a two-step Sync every 125 ms, a
//! Delay_Resp for every Delay_Req, with configurable delays, spikes, drift and
//! failures. Shared by `tests/ptp_follower.rs` and `tests/buffered_session.rs`.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use airplay_audio::timeline::mono_now_ns;
use airplay_client::MediaClock;
use airplay_client::ptp_follower::wire::{self, AnnounceBody, Message, PortIdentity};
use airplay_client::ptp_follower::{FollowerConfig, PtpFollower};

pub const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
/// A HomePod's clock identity (from upstream's capture notes).
pub const GM_IDENTITY: [u8; 8] = [0x14, 0x14, 0x7d, 0xe8, 0x54, 0x98, 0x00, 0x08];
/// Grandmaster time minus local time at the start: about 11 days of uptime.
pub const GM_BASE_NS: i128 = 987_654_321_000_000;
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// How the fake grandmaster behaves; tests change it while it runs.
#[derive(Debug, Clone)]
pub struct Behaviour {
    /// Its clock runs this fast (ppm) against the local monotonic clock.
    pub ppm: f64,
    /// Added to its clock (a step).
    pub step_ns: i128,
    pub identity: [u8; 8],
    pub priority1: u8,
    /// One-way delay of every message, each way.
    pub delay: Duration,
    /// Every n-th Sync and Delay_Req (0: none) is held up by `spike` more.
    pub spike_every: u32,
    pub spike: Duration,
    /// Answer Delay_Req at all.
    pub answer_delay_req: bool,
    /// Send nothing at all.
    pub silent: bool,
    /// The PTP domain of everything it sends.
    pub domain: u8,
    /// The grandmaster its Announces name, if not itself: it then relays
    /// another clock's time, as a HomePod following an Apple TV would.
    pub announced_grandmaster: Option<[u8; 8]>,
    /// Send each Follow_Up before its Sync (by `FOLLOW_UP_LEAD`), as a
    /// reordering link, or two receiving threads, can present them.
    pub follow_up_first: bool,
}

/// How far ahead of its Sync a Follow_Up goes when `follow_up_first` is set.
pub const FOLLOW_UP_LEAD: Duration = Duration::from_millis(3);

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            ppm: 0.0,
            step_ns: 0,
            identity: GM_IDENTITY,
            priority1: 248,
            delay: Duration::from_micros(300),
            spike_every: 0,
            spike: Duration::ZERO,
            answer_delay_req: true,
            silent: false,
            domain: 0,
            announced_grandmaster: None,
            follow_up_first: false,
        }
    }
}

pub struct Clock {
    start_ns: u64,
}

impl Clock {
    /// Grandmaster time at local time `local_ns`.
    pub fn master_ns(&self, behaviour: &Behaviour, local_ns: u64) -> i128 {
        let elapsed = i128::from(local_ns - self.start_ns);
        GM_BASE_NS
            + i128::from(local_ns)
            + (elapsed as f64 * behaviour.ppm / 1e6) as i128
            + behaviour.step_ns
    }
}

/// A grandmaster on the loopback interface, the way a HomePod behaves with a
/// Mac: Announce every 250 ms, two-step Sync every 125 ms, Delay_Resp for
/// every Delay_Req, all sent to the follower's ports (set, or learnt from the
/// first messages the follower sends).
pub struct FakeGrandmaster {
    pub event_addr: SocketAddr,
    pub general_addr: SocketAddr,
    behaviour: Arc<Mutex<Behaviour>>,
    pub clock: Arc<Clock>,
    received: Arc<Mutex<Vec<(SocketAddr, Message)>>>,
    follower: Arc<Mutex<Option<(SocketAddr, SocketAddr)>>>,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeGrandmaster {
    pub fn start(behaviour: Behaviour) -> Self {
        let event = UdpSocket::bind((LOCALHOST, 0)).unwrap();
        let general = UdpSocket::bind((LOCALHOST, 0)).unwrap();
        event
            .set_read_timeout(Some(Duration::from_micros(200)))
            .unwrap();
        general.set_nonblocking(true).unwrap();
        let mut gm = Self {
            event_addr: event.local_addr().unwrap(),
            general_addr: general.local_addr().unwrap(),
            behaviour: Arc::new(Mutex::new(behaviour)),
            clock: Arc::new(Clock {
                start_ns: mono_now_ns(),
            }),
            received: Arc::default(),
            follower: Arc::default(),
            running: Arc::new(AtomicBool::new(true)),
            thread: None,
        };
        let behaviour = Arc::clone(&gm.behaviour);
        let clock = Arc::clone(&gm.clock);
        let received = Arc::clone(&gm.received);
        let follower = Arc::clone(&gm.follower);
        let running = Arc::clone(&gm.running);
        gm.thread = Some(thread::spawn(move || {
            run_grandmaster(
                &event, &general, &behaviour, &clock, &received, &follower, &running,
            )
        }));
        gm
    }

    /// A follower configured to talk to this grandmaster.
    pub fn follower_config(&self) -> FollowerConfig {
        FollowerConfig {
            local_event_port: 0,
            local_general_port: 0,
            master_event_port: self.event_addr.port(),
            master_general_port: self.general_addr.port(),
            ..FollowerConfig::new(LOCALHOST)
        }
    }

    pub fn start_follower(&self) -> PtpFollower {
        self.start_follower_with(self.follower_config())
    }

    pub fn start_follower_with(&self, config: FollowerConfig) -> PtpFollower {
        let follower = PtpFollower::start(config).unwrap();
        self.target(&follower);
        follower.introduce();
        follower
    }

    /// Sends to `follower` (another grandmaster's follower: this one then
    /// shares the address of the one it follows).
    pub fn target(&self, follower: &PtpFollower) {
        // It listens on all addresses; reach it on the loopback one.
        let event = SocketAddr::new(LOCALHOST, follower.event_addr().port());
        let general = SocketAddr::new(LOCALHOST, follower.general_addr().port());
        *self.follower.lock().unwrap() = Some((event, general));
    }

    pub fn change(&self, change: impl FnOnce(&mut Behaviour)) {
        change(&mut self.behaviour.lock().unwrap());
    }

    /// Grandmaster time now.
    pub fn now_ns(&self) -> i128 {
        self.master_ns(mono_now_ns())
    }

    /// Grandmaster time at local time `local_ns`.
    pub fn master_ns(&self, local_ns: u64) -> i128 {
        let behaviour = self.behaviour.lock().unwrap().clone();
        self.clock.master_ns(&behaviour, local_ns)
    }

    /// How far `clock` is from this grandmaster's clock, in ms: the reading
    /// is taken between two of the grandmaster's (retried if the test thread
    /// was preempted in between), so scheduling cannot inflate it.
    pub fn error_ms(&self, clock: &MediaClock) -> f64 {
        loop {
            let before = self.now_ns();
            let reading = i128::from(clock.now_ns());
            let after = self.now_ns();
            if after - before > 1_000_000 {
                continue;
            }
            let error = if reading < before {
                before - reading
            } else {
                (reading - after).max(0)
            };
            return error as f64 / 1e6;
        }
    }

    pub fn received(&self) -> Vec<(SocketAddr, Message)> {
        self.received.lock().unwrap().clone()
    }
}

impl Drop for FakeGrandmaster {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// `packet` in PTP domain `domain` (the header's domainNumber byte).
fn in_domain(mut packet: Vec<u8>, domain: u8) -> Vec<u8> {
    packet[4] = domain;
    packet
}

/// Something to send once the simulated network delivers it.
enum Outgoing {
    Packet {
        from_event: bool,
        bytes: Vec<u8>,
        to_general: bool,
    },
    /// A Sync and its Follow_Up, held up `held_ns` by the network: the
    /// Follow_Up's timestamp is taken when they are really sent, minus that,
    /// so the one-way delay the follower sees is exactly `held_ns` however
    /// late this loop gets to it.
    SyncPair { sequence: u16, held_ns: u64 },
}

#[allow(clippy::too_many_arguments)]
fn run_grandmaster(
    event: &UdpSocket,
    general: &UdpSocket,
    behaviour: &Mutex<Behaviour>,
    clock: &Clock,
    received: &Mutex<Vec<(SocketAddr, Message)>>,
    follower: &Mutex<Option<(SocketAddr, SocketAddr)>>,
    running: &AtomicBool,
) {
    // (local time it arrives, what)
    let mut queue: VecDeque<(u64, Outgoing)> = VecDeque::new();
    let mut next_sync = mono_now_ns();
    let mut next_announce = mono_now_ns();
    let mut sync_sequence = 0u16;
    let mut announce_sequence = 0u16;
    let mut spike_counter = 0u32;
    let mut buffer = [0u8; 512];

    while running.load(Ordering::Acquire) {
        let now = mono_now_ns();
        let current = behaviour.lock().unwrap().clone();
        let source = PortIdentity {
            clock: current.identity,
            port: 1,
        };
        let delay = current.delay.as_nanos() as u64;
        let mut spike = || {
            spike_counter += 1;
            if current.spike_every > 0 && spike_counter.is_multiple_of(current.spike_every) {
                current.spike.as_nanos() as u64
            } else {
                0
            }
        };

        if !current.silent && now >= next_sync {
            next_sync += 125_000_000;
            sync_sequence = sync_sequence.wrapping_add(1);
            // The network holds both the Sync and its Follow_Up.
            let held_ns = delay + spike();
            queue.push_back((
                now + held_ns,
                Outgoing::SyncPair {
                    sequence: sync_sequence,
                    held_ns,
                },
            ));
        }
        if !current.silent && now >= next_announce {
            next_announce += 250_000_000;
            announce_sequence = announce_sequence.wrapping_add(1);
            let body = AnnounceBody {
                priority1: current.priority1,
                priority2: 239,
                clock_accuracy: 0x21,
                variance: 17258,
                ..AnnounceBody::follower(current.announced_grandmaster.unwrap_or(current.identity))
            };
            queue.push_back((
                now + delay,
                Outgoing::Packet {
                    from_event: false,
                    bytes: wire::announce(source, announce_sequence, &body, -2),
                    to_general: true,
                },
            ));
        }

        let destinations = *follower.lock().unwrap();
        // Until it has heard from both of the follower's sockets, a learnt
        // destination may be the wrong one: that only delays the first Syncs.
        while queue.front().is_some_and(|&(at, _)| at <= mono_now_ns()) {
            let (_, outgoing) = queue.pop_front().unwrap();
            let Some((follower_event, follower_general)) = destinations else {
                continue;
            };
            match outgoing {
                Outgoing::Packet {
                    from_event,
                    bytes,
                    to_general,
                } => {
                    let socket = if from_event { event } else { general };
                    let to = if to_general {
                        follower_general
                    } else {
                        follower_event
                    };
                    let _ = socket.send_to(&in_domain(bytes, current.domain), to);
                }
                Outgoing::SyncPair { sequence, held_ns } => {
                    let sync = in_domain(wire::sync(source, sequence, true, 0), current.domain);
                    let follow_up = |sent: i128| {
                        in_domain(
                            wire::follow_up(source, sequence, sent as u128, 0),
                            current.domain,
                        )
                    };
                    if current.follow_up_first {
                        // The Sync leaves FOLLOW_UP_LEAD later (spun for, so
                        // its time is right to the microsecond).
                        let departs = mono_now_ns() + FOLLOW_UP_LEAD.as_nanos() as u64;
                        let sent = clock.master_ns(&current, departs - held_ns);
                        let _ = general.send_to(&follow_up(sent), follower_general);
                        while mono_now_ns() < departs {
                            std::hint::spin_loop();
                        }
                        let _ = event.send_to(&sync, follower_event);
                    } else {
                        let sent = clock.master_ns(&current, mono_now_ns() - held_ns);
                        let _ = event.send_to(&sync, follower_event);
                        let _ = general.send_to(&follow_up(sent), follower_general);
                    }
                }
            }
        }
        // Deliver in order of arrival (a held-up message arrives after the
        // ones sent behind it, as on a congested link).
        let mut sorted: Vec<_> = queue.drain(..).collect();
        sorted.sort_by_key(|&(at, _)| at);
        queue.extend(sorted);

        for (socket, is_event) in [(event, true), (general, false)] {
            let Ok((length, from)) = socket.recv_from(&mut buffer) else {
                continue;
            };
            let arrived = mono_now_ns();
            let Some(message) = wire::parse(&buffer[..length]) else {
                continue;
            };
            // Learn where the follower listens from what it sends.
            {
                let mut known = follower.lock().unwrap();
                let (mut event_to, mut general_to) = known.unwrap_or((from, from));
                if is_event {
                    event_to = from;
                } else {
                    general_to = from;
                }
                *known = Some((event_to, general_to));
            }
            if is_event
                && let Message::DelayReq { header, .. } = &message
                && current.answer_delay_req
                && !current.silent
            {
                // It reached us `delay` (plus any spike) after it left.
                let reached = clock.master_ns(&current, arrived + delay + spike());
                queue.push_back((
                    arrived,
                    Outgoing::Packet {
                        from_event: false,
                        bytes: wire::delay_resp(
                            source,
                            header.sequence,
                            reached as u128,
                            header.source,
                            0,
                        ),
                        to_general: true,
                    },
                ));
            }
            received.lock().unwrap().push((from, message));
        }
    }
}
