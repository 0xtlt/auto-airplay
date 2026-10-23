//! The once-a-minute report on the AirPlay output, next to the `bt_input
//! stats` line on the input.
//!
//! Platform independent, so the line can be tested anywhere.

use std::fmt;

use airplay_client::OutputStats;

/// The greppable per-minute line on what the AirPlay sender did, from the
/// counters [`AirPlayClient::take_output_stats`] hands over:
///
/// * `packets`: audio packets sent;
/// * `late_packets`: of those, sent more than one packet (8 ms) after their
///   deadline;
/// * `send_jitter_max_ms`: the latest a packet went out after its deadline;
/// * `rtx_requests`: retransmit requests from the speaker, which sends one
///   when audio packets did not reach it;
/// * `rtx_unparsed`: requests of a length the control channel does not
///   understand, ignored;
/// * `rtx_requested`, `rtx_served`, `rtx_missed`: the packets they asked
///   for, sent again, and not sent again (out of the history, about 4 s);
/// * `rtx_late`: requested packets asked for after the time the sync packets
///   say they play (the announced latency after they were sent), too late
///   for a resend to help;
/// * `rtx_age_max_ms`: the longest between sending a packet and receiving
///   the request for it again.
///
/// [`AirPlayClient::take_output_stats`]: airplay_client::AirPlayClient::take_output_stats
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputReport(pub OutputStats);

impl fmt::Display for OutputReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stats = &self.0;
        write!(
            f,
            "airplay_output stats: packets={} late_packets={} send_jitter_max_ms={}.{} \
             rtx_requests={} rtx_unparsed={} rtx_requested={} rtx_served={} rtx_missed={} rtx_late={} \
             rtx_age_max_ms={}",
            stats.packets_sent,
            stats.late_packets,
            stats.max_late_us / 1_000,
            stats.max_late_us % 1_000 / 100,
            stats.rtx_requests,
            stats.rtx_unparsed,
            stats.rtx_requested,
            stats.rtx_served,
            stats.rtx_missed,
            stats.rtx_late,
            stats.rtx_max_age_us / 1_000,
        )
    }
}
