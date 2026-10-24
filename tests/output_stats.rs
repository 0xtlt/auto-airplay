//! The sender's output counters (`airplay_audio::output`), on a clock the
//! test controls: what counts as late, how old a requested packet is, and the
//! per-minute line they end up in.
//!
//! A packet plays the announced latency after its deadline on the stream's
//! timeline, whenever it really went out: a request is late once that time
//! has passed. Its age is measured from when it really went out.

use airplay_audio::RetransmitRequest;
use airplay_audio::output::{OutputCounters, OutputStats};
use auto_airplay::output_stats::OutputReport;

/// 352 frames at 44.1 kHz.
const PACKET_NS: u64 = 7_981_859;
/// The daemon's announced latency: 11025 frames, 250 ms.
const LATENCY_NS: u64 = 250_000_000;
const MS: u64 = 1_000_000;
/// The monotonic time the stream starts at (any; not zero).
const START: u64 = 10_000 * MS;

/// The age the counters give a packet sent at `sent` and asked for at `now`
/// (both kept to the µs).
fn age_us(now: u64, sent: u64) -> u64 {
    now / 1_000 - sent / 1_000
}

fn ask(first_sequence: u16, count: u16) -> RetransmitRequest {
    RetransmitRequest {
        first_sequence,
        count,
    }
}

/// Packets `first..first + count` sent on time on a timeline starting at
/// `origin` with packet `first`.
fn send_on_time(counters: &OutputCounters, first: u16, count: u16, origin: u64) {
    for n in 0..count {
        let deadline = origin + u64::from(n) * PACKET_NS;
        counters.record_send(first.wrapping_add(n), deadline, deadline, PACKET_NS);
    }
}

#[test]
fn a_packet_sent_late_is_late_from_its_deadline_not_from_its_send() {
    let counters = OutputCounters::new();
    // Packet 7 went out 100 ms after its deadline; the speaker asks for it
    // 200 ms after it really went out.
    let deadline = START;
    counters.record_send(7, deadline, deadline + 100 * MS, PACKET_NS);
    counters.record_retransmit(&ask(7, 1), Some(1), deadline + 300 * MS, LATENCY_NS);

    let stats = counters.take();
    // It played at its deadline plus 250 ms: the request came 50 ms after.
    assert_eq!(stats.rtx_late, 1, "{stats:?}");
    assert_eq!(stats.rtx_max_age_us, 200_000, "{stats:?}");
    assert_eq!((stats.late_packets, stats.max_late_us), (1, 100_000));

    // Asked for 10 ms before the time it plays: not late, 140 ms old.
    counters.record_retransmit(&ask(7, 1), Some(1), deadline + 240 * MS, LATENCY_NS);
    let stats = counters.take();
    assert_eq!(stats.rtx_late, 0, "{stats:?}");
    assert_eq!(stats.rtx_max_age_us, 140_000, "{stats:?}");
}

#[test]
fn the_time_a_packet_plays_counts_from_the_end_of_its_latency_exactly() {
    let counters = OutputCounters::new();
    counters.record_send(3, START, START, PACKET_NS);

    counters.record_retransmit(&ask(3, 1), Some(1), START + LATENCY_NS - MS, LATENCY_NS);
    assert_eq!(counters.take().rtx_late, 0, "1 ms before it plays");
    counters.record_retransmit(&ask(3, 1), Some(1), START + LATENCY_NS, LATENCY_NS);
    assert_eq!(counters.take().rtx_late, 1, "the moment it plays");
}

#[test]
fn after_a_timeline_restart_each_packet_keeps_its_own_deadline() {
    let counters = OutputCounters::new();
    // Packets 0 to 9 on the first timeline; a stall, and the timeline
    // restarts with packet 10, one second later.
    send_on_time(&counters, 0, 10, START);
    let restart = START + 1_000 * MS;
    send_on_time(&counters, 10, 10, restart);

    // 100 ms after the restart, the speaker asks for packets 5 and 12.
    let now = restart + 100 * MS;
    counters.record_retransmit(&ask(5, 1), Some(1), now, LATENCY_NS);
    counters.record_retransmit(&ask(12, 1), Some(1), now, LATENCY_NS);

    let stats = counters.take();
    // Packet 5 played 290 ms after the start, long before; packet 12 plays
    // 266 ms after the restart, later.
    assert_eq!(stats.rtx_late, 1, "{stats:?}");
    // Packet 5 went out 1060 ms before the request: the timeline moved, the
    // age does not extrapolate across it.
    let age_5 = age_us(now, START + 5 * PACKET_NS);
    assert!(age_5 > 1_000_000, "{age_5}");
    assert_eq!(stats.rtx_max_age_us, age_5, "{stats:?}");
}

#[test]
fn a_request_spanning_the_play_time_counts_only_the_packets_already_played() {
    let counters = OutputCounters::new();
    send_on_time(&counters, 100, 40, START);
    // Asked for at the time packet 110 plays: packets 100 to 110 have
    // played, 111 to 119 have not.
    let now = START + 10 * PACKET_NS + LATENCY_NS;
    counters.record_retransmit(&ask(100, 20), Some(20), now, LATENCY_NS);

    let stats = counters.take();
    assert_eq!(stats.rtx_late, 11, "{stats:?}");
    assert_eq!(stats.rtx_max_age_us, age_us(now, START), "packet 100");
}

#[test]
fn packets_with_no_send_record_are_neither_late_nor_dated() {
    let counters = OutputCounters::new();
    // 600 packets sent: the records keep the last 512.
    send_on_time(&counters, 0, 600, START);
    let now = START + 600 * PACKET_NS + 10 * MS;
    // Packet 50 is out of the records, 700 was never sent.
    counters.record_retransmit(&ask(50, 1), None, now, LATENCY_NS);
    counters.record_retransmit(&ask(700, 2), Some(0), now, LATENCY_NS);

    let stats = counters.take();
    assert_eq!(
        (
            stats.rtx_requests,
            stats.rtx_requested,
            stats.rtx_served,
            stats.rtx_missed
        ),
        (2, 3, 0, 3)
    );
    assert_eq!((stats.rtx_late, stats.rtx_max_age_us), (0, 0), "{stats:?}");

    // Packet 599 is on record.
    counters.record_retransmit(&ask(599, 1), Some(1), now, LATENCY_NS);
    assert_eq!(
        counters.take().rtx_max_age_us,
        age_us(now, START + 599 * PACKET_NS)
    );
}

#[test]
fn sequence_numbers_wrap() {
    let counters = OutputCounters::new();
    send_on_time(&counters, 65_530, 12, START);
    let now = START + 12 * PACKET_NS + LATENCY_NS;
    // 65535, 0 and 1: sent sixth to eighth.
    counters.record_retransmit(&ask(65_535, 3), Some(3), now, LATENCY_NS);

    let stats = counters.take();
    assert_eq!((stats.rtx_requested, stats.rtx_late), (3, 3), "{stats:?}");
    assert_eq!(stats.rtx_max_age_us, age_us(now, START + 5 * PACKET_NS));
}

#[test]
fn sends_count_packets_late_by_more_than_one_packet() {
    let counters = OutputCounters::new();
    counters.record_send(0, START, START, PACKET_NS);
    counters.record_send(1, START + PACKET_NS, START + PACKET_NS + 5 * MS, PACKET_NS);
    counters.record_send(
        2,
        START + 2 * PACKET_NS,
        START + 2 * PACKET_NS + 9 * MS,
        PACKET_NS,
    );

    let stats = counters.take();
    assert_eq!(
        (stats.packets_sent, stats.late_packets, stats.max_late_us),
        (3, 1, 9_000)
    );
}

#[test]
fn each_period_starts_at_zero_and_the_records_are_kept() {
    let counters = OutputCounters::new();
    send_on_time(&counters, 0, 5, START);
    counters.note_unparsed();
    counters.record_retransmit(&ask(1, 1), None, START + 300 * MS, LATENCY_NS);
    let first = counters.take();
    assert_eq!(
        (
            first.packets_sent,
            first.rtx_unparsed,
            first.rtx_missed,
            first.rtx_late
        ),
        (5, 1, 1, 1)
    );

    assert_eq!(counters.take(), OutputStats::default());
    // The next period still dates packets sent in the previous one.
    counters.record_retransmit(&ask(1, 1), Some(1), START + 300 * MS, LATENCY_NS);
    assert_eq!(counters.take().rtx_late, 1);
}

#[test]
fn the_output_line_has_the_greppable_shape() {
    let stats = OutputStats {
        packets_sent: 7_517,
        late_packets: 1,
        max_late_us: 8_412,
        rtx_requests: 3,
        rtx_unparsed: 2,
        rtx_requested: 4,
        rtx_served: 3,
        rtx_missed: 1,
        rtx_late: 1,
        rtx_max_age_us: 412_999,
    };
    assert_eq!(
        OutputReport(stats).to_string(),
        "airplay_output stats: packets=7517 late_packets=1 send_jitter_max_ms=8.4 \
         rtx_requests=3 rtx_unparsed=2 rtx_requested=4 rtx_served=3 rtx_missed=1 rtx_late=1 rtx_age_max_ms=412"
    );
    assert_eq!(
        OutputReport(OutputStats::default()).to_string(),
        "airplay_output stats: packets=0 late_packets=0 send_jitter_max_ms=0.0 \
         rtx_requests=0 rtx_unparsed=0 rtx_requested=0 rtx_served=0 rtx_missed=0 rtx_late=0 rtx_age_max_ms=0"
    );
}
