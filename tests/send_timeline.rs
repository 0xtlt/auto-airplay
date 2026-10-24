//! The timeline of the vendored AirPlay sender (`airplay_audio::timeline`):
//! when each packet is due, when it plays, and the latency its sync packet
//! announces.

use std::time::Duration;

use airplay_audio::timeline::{Timeline, announced_latency};
use airplay_client::StreamConfig;

const RATE: u32 = 44_100;
const PACKET: u64 = 352;
const MS: u64 = 1_000_000;
/// The daemon's `output.render_delay_ms`.
const RENDER_DELAY: Duration = Duration::from_millis(250);

/// The latency the daemon's sync packets announce, in frames.
fn daemon_latency() -> u32 {
    let config = StreamConfig::default();
    announced_latency(RENDER_DELAY, RATE, config.latency_min, config.latency_max)
}

fn timeline() -> Timeline {
    Timeline::new(RATE, daemon_latency())
}

/// ns of `frames` at 44.1 kHz, rounded down.
fn frames_ns(frames: u64) -> u64 {
    (u128::from(frames) * 1_000_000_000 / u128::from(RATE)) as u64
}

#[test]
fn deadlines_and_anchors_are_a_function_of_the_position_only() {
    let mut timeline = timeline();
    let origin = 5_000 * MS;
    let first = timeline.slot(1_000, origin);
    assert!(first.restarted);
    assert_eq!(first.deadline_ns, origin);

    // Prepared early, on time or a little late: the slot does not move.
    for (packet, prepared_at) in [
        (1, origin - 70 * MS),
        (2, origin + 16 * MS),
        (3, origin + 40 * MS),
    ] {
        let slot = timeline.slot(1_000 + packet * PACKET, prepared_at);
        assert!(!slot.restarted);
        assert_eq!(slot.deadline_ns, origin + frames_ns(packet * PACKET));
        assert_eq!(
            slot.anchor_ns - i128::from(slot.deadline_ns),
            timeline.margin()
        );
    }
}

#[test]
fn the_render_delay_is_announced_as_the_latency_setup_declares_as_minimum() {
    // SETUP declares latencyMin 11025 and latencyMax 88200 frames (250 ms and
    // 2 s at 44.1 kHz): the 250 ms render delay is exactly the minimum.
    let config = StreamConfig::default();
    assert_eq!((config.latency_min, config.latency_max), (11_025, 88_200));
    assert_eq!(daemon_latency(), 11_025);
    // A packet plays that long after its deadline: 250 ms, where the timeline
    // used to subtract the 10 packets (80 ms) upstream stamped ahead of
    // sending.
    let timeline = timeline();
    assert_eq!(timeline.latency_frames(), 11_025);
    assert_eq!(timeline.margin(), 250_000_000);
}

#[test]
fn the_announced_latency_stays_within_what_setup_declares() {
    let latency = |ms| announced_latency(Duration::from_millis(ms), RATE, 11_025, 88_200);
    // Below the minimum: a receiver would clamp it there anyway.
    assert_eq!(latency(0), 11_025);
    assert_eq!(latency(100), 11_025);
    assert_eq!(latency(500), 22_050);
    assert_eq!(latency(2_000), 88_200);
    assert_eq!(latency(5_000), 88_200);
    // Frames of the stream's clock, rounded to the nearest.
    assert_eq!(announced_latency(RENDER_DELAY, 48_000, 0, u32::MAX), 12_000);
    let rounded = |us| announced_latency(Duration::from_micros(us), RATE, 0, u32::MAX);
    assert_eq!(rounded(250_005), 11_025); // 11025.2 frames
    assert_eq!(rounded(250_020), 11_026); // 11025.9 frames
}

#[test]
fn a_week_of_packets_does_not_drift() {
    let mut timeline = timeline();
    timeline.slot(0, 0);
    let week = u64::from(RATE) * 7 * 24 * 3_600;
    let slot = timeline.slot(week, 7 * 24 * 3_600 * 1_000 * MS);
    assert!(!slot.restarted);
    assert_eq!(slot.deadline_ns, 7 * 24 * 3_600 * 1_000 * MS);
}

#[test]
fn only_a_hopelessly_late_packet_moves_the_timeline() {
    let mut timeline = timeline();
    timeline.slot(0, 0);
    // Half the 250 ms latency: still playable, the timeline holds.
    assert_eq!(timeline.max_late_ns(), 125 * MS);
    let late = timeline.slot(100 * PACKET, frames_ns(100 * PACKET) + 125 * MS);
    assert!(!late.restarted);
    assert_eq!(timeline.restarts(), 0);

    let now = frames_ns(101 * PACKET) + 130 * MS;
    let hopeless = timeline.slot(101 * PACKET, now);
    assert!(hopeless.restarted);
    assert_eq!(hopeless.deadline_ns, now);
    assert_eq!(timeline.restarts(), 1);
    // The new timeline goes on from there.
    let next = timeline.slot(102 * PACKET, now);
    assert_eq!(next.deadline_ns, now + frames_ns(PACKET));
}

#[test]
fn a_restart_or_a_jump_back_starts_a_new_timeline() {
    let mut timeline = timeline();
    timeline.slot(10 * PACKET, 0);
    timeline.restart();
    let resumed = timeline.slot(11 * PACKET, 3_000 * MS);
    assert!(resumed.restarted);
    assert_eq!(resumed.deadline_ns, 3_000 * MS);
    let back = timeline.slot(PACKET, 3_001 * MS);
    assert!(back.restarted);
    assert_eq!(back.deadline_ns, 3_001 * MS);
}

#[test]
fn without_latency_lateness_is_still_tolerated_a_little() {
    let mut timeline = Timeline::new(RATE, 0);
    assert_eq!(timeline.margin(), 0);
    timeline.slot(0, 0);
    assert!(!timeline.slot(PACKET, frames_ns(PACKET) + 19 * MS).restarted);
    assert!(
        timeline
            .slot(2 * PACKET, frames_ns(2 * PACKET) + 21 * MS)
            .restarted
    );
}

/// The producer runs 9 packets ahead of the sender thread, as in the
/// streamer; the whole process is suspended for 300 ms at t = 6 s.
#[test]
fn a_suspension_restarts_the_timeline_once_and_only_moves_the_anchors_once() {
    let mut timeline = timeline();
    let ahead = frames_ns(9 * PACKET);
    let suspended_at = 6_000 * MS;
    let suspension = 300 * MS;
    // Monotonic time at which the packet starting at `position` is prepared.
    let prepared = |position: u64, shift: u64| {
        let due = (frames_ns(position) + shift).saturating_sub(ahead);
        if due >= suspended_at {
            due.max(suspended_at + suspension)
        } else {
            due
        }
    };

    let mut shift = 0;
    let mut anchor_steps = Vec::new();
    let mut previous: Option<(u64, i128)> = None;
    for packet in 0..2_000 {
        let position = packet * PACKET;
        let now = prepared(position, shift);
        let slot = timeline.slot(position, now);
        assert!(slot.deadline_ns + timeline.max_late_ns() >= now);
        assert_eq!(
            slot.anchor_ns - i128::from(slot.deadline_ns),
            timeline.margin()
        );
        if slot.restarted && packet > 0 {
            // From here on the producer runs ahead of the new timeline.
            shift = slot.deadline_ns - frames_ns(position);
        }
        // Anchor minus the RTP timeline: constant (to the nanosecond that
        // `frames_ns` rounds away), but for the restart.
        let residual = slot.anchor_ns - i128::from(frames_ns(position));
        if let Some((_, before)) = previous
            && (residual - before).abs() > 1_000
        {
            anchor_steps.push(residual - before);
        }
        previous = Some((position, residual));
    }

    assert_eq!(timeline.restarts(), 1);
    assert_eq!(anchor_steps.len(), 1, "{anchor_steps:?}");
    // The first packet prepared after the suspension was due 72 to 80 ms
    // after it began: 220 to 228 ms late, so the anchors move that much, once.
    let step = anchor_steps[0];
    assert!(step > i128::from(timeline.max_late_ns()), "{step}");
    assert!(
        (219 * MS as i128..=229 * MS as i128).contains(&step),
        "{step}"
    );
}
