//! The timeline of the vendored AirPlay sender (`airplay_audio::timeline`):
//! when each packet is due and what its sync packet announces.

use std::time::Duration;

use airplay_audio::timeline::Timeline;

const RATE: u32 = 44_100;
const PACKET: u64 = 352;
const MS: u64 = 1_000_000;

fn timeline() -> Timeline {
    Timeline::new(RATE, PACKET as u32, Duration::from_millis(250))
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
fn the_margin_is_what_upstream_gave_in_its_steady_state() {
    // Render delay minus the ten packets upstream stamped ahead of sending.
    let margin = timeline().margin();
    assert_eq!(margin, 250_000_000 - i128::from(frames_ns(10 * PACKET)));
    assert!((170_181_000..170_182_000).contains(&margin), "{margin}");
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
    // Half the 170 ms margin: still playable, the timeline holds.
    let late = timeline.slot(100 * PACKET, frames_ns(100 * PACKET) + 85 * MS);
    assert!(!late.restarted);
    assert_eq!(timeline.restarts(), 0);

    let now = frames_ns(101 * PACKET) + 90 * MS;
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
fn without_a_margin_lateness_is_still_tolerated_a_little() {
    // A render delay shorter than ten packets: negative margin, as upstream.
    let mut timeline = Timeline::new(RATE, PACKET as u32, Duration::ZERO);
    assert!(timeline.margin() < 0);
    timeline.slot(0, 0);
    assert!(!timeline.slot(PACKET, frames_ns(PACKET) + 19 * MS).restarted);
    assert!(
        timeline
            .slot(2 * PACKET, frames_ns(2 * PACKET) + 21 * MS)
            .restarted
    );
}
