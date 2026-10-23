//! The NTP sync packet (payload type 84) the vendored AirPlay sender builds.
//!
//! Bytes 4-7 carry the RTP timestamp of bytes 16-19 minus the latency: the
//! frame that plays at the NTP time of bytes 8-15. Receivers read the
//! difference of the two timestamps as the latency they must buffer
//! (shairport-sync: `sync_rtp_timestamp - rtp_timestamp_less_latency`);
//! iTunes sends 88200 frames, OwnTone and pyatv their own buffer. Bytes 2-3
//! are not a sequence number: every sender writes 7 there.

use std::net::SocketAddr;
use std::time::Duration;

use airplay_audio::RtpSender;
use airplay_audio::timeline::announced_latency;
use airplay_client::StreamConfig;

/// 250 ms at 44.1 kHz: the daemon's render delay, and the `latencyMin` that
/// SETUP declares.
const LATENCY: u32 = 11_025;

fn sender() -> RtpSender {
    let speaker: SocketAddr = "127.0.0.1:7000".parse().unwrap();
    let mut sender = RtpSender::new(speaker, 0x0a1b_2c3d);
    sender.set_control_dest("127.0.0.1:7001".parse().unwrap());
    sender
}

fn word(packet: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(packet[at..at + 4].try_into().unwrap())
}

#[test]
fn bytes_4_to_7_are_the_rtp_timestamp_minus_the_latency() {
    let ntp: u64 = 0xec8f_4a21_8000_0000;
    let packet = sender()
        .prepare_sync(1_000_000, LATENCY, ntp)
        .unwrap()
        .expect("a control port is set");

    assert_eq!(packet.len(), 20);
    // First sync after RECORD: extension bit; marker bit and payload type 84.
    assert_eq!(packet[..2], [0x90, 0xd4]);
    assert_eq!(packet[4..8], 988_975_u32.to_be_bytes());
    assert_eq!(packet[8..16], ntp.to_be_bytes());
    assert_eq!(packet[16..20], 1_000_000_u32.to_be_bytes());
    // What a receiver reads as the latency: never zero again.
    assert_eq!(word(&packet, 16).wrapping_sub(word(&packet, 4)), LATENCY);
}

#[test]
fn bytes_4_to_7_wrap_at_the_start_of_a_stream() {
    // The encoder starts at RTP timestamp 0: the frame playing when the first
    // packet is sent comes "before" it, modulo 2^32 like any RTP timestamp.
    let mut sender = sender();
    let first = sender.prepare_sync(0, LATENCY, 0).unwrap().unwrap();
    assert_eq!(word(&first, 4), 4_294_956_271);
    assert_eq!(word(&first, 16).wrapping_sub(word(&first, 4)), LATENCY);

    // One second in: the same latency, and no extension bit any more.
    let next = sender.prepare_sync(44_100, LATENCY, 0).unwrap().unwrap();
    assert_eq!(next[0], 0x80);
    assert_eq!(word(&next, 4), 33_075);
    assert_eq!(word(&next, 16).wrapping_sub(word(&next, 4)), LATENCY);
}

#[test]
fn bytes_2_and_3_are_7_in_every_sync_packet() {
    // iTunes, OwnTone, pyatv and libraop send 7 there in every sync packet;
    // shairport-sync reads 7 as "add 11025 frames of latency". Upstream sent
    // a counter: the 8th sync packet (counter 7) announced 250 ms more than
    // the others.
    let mut sender = sender();
    for n in 0..300_u32 {
        let packet = sender
            .prepare_sync(n * 44_100, LATENCY, 0)
            .unwrap()
            .unwrap();
        assert_eq!(packet[2..4], [0x00, 0x07], "sync packet {n}");
        assert_eq!(word(&packet, 16).wrapping_sub(word(&packet, 4)), LATENCY);
    }
}

#[test]
fn the_daemon_announces_its_render_delay_within_setup_bounds() {
    // What `AudioStreamer` passes to `prepare_sync`, with the daemon's
    // stream config and `output.render_delay_ms = 250`.
    let config = StreamConfig::default();
    let latency = announced_latency(
        Duration::from_millis(250),
        config.audio_format.sample_rate.as_hz(),
        config.latency_min,
        config.latency_max,
    );
    assert_eq!(latency, LATENCY);
    assert!((config.latency_min..=config.latency_max).contains(&latency));
}
