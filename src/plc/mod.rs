//! Packet loss concealment for the Bluetooth input.
//!
//! A2DP over a noisy radio link loses packets. The capture device does not
//! stall when that happens: the audio of the lost packets is simply gone, and
//! what follows plays on in real time. Played back as it comes, every hole is
//! a splice in the waveform, heard as a click, and it also starves the AirPlay
//! sender, which resets its timing.
//!
//! This module sits between the capture device and the AirPlay sender and has
//! no platform dependency, so it can be tested anywhere:
//!
//! * [`JitterBuffer`] queues the captured audio, finds and conceals holes and
//!   runs of zeros, bridges underruns, and compensates for clock drift. See
//!   its documentation for the policies and constants.
//! * [`GapDetector`] finds holes in the stream from arrival times.
//! * [`Pacer`] decides when the output may take audio from the buffer.
//! * [`StatsSnapshot`] is the per-minute report.

mod buffer;
mod conceal;
mod gap;
mod pace;
mod stats;

pub use buffer::{JitterBuffer, PLAYOUT_CHUNK_FRAMES};
pub use gap::{GapDetector, GapVerdict};
pub use pace::{Due, Pacer};
pub use stats::StatsSnapshot;

/// How often the stats are reported.
pub const STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
