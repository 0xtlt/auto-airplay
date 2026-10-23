# Vendored `airplay-audio`

Origin: `crates/airplay-audio` from <https://github.com/lmcgartland/airplay2-rs.git>,
commit `a7f019fe6246ebd9701201a8a5c31e9a15243956`.

License: GPL-2.0 (upstream `LICENSE`, copied here unchanged).

## Why

The live streaming path (`AudioStreamer::start_live`) decides when each RTP
packet is sent and which time each sync packet announces for it. That timing
made the receiver change speed and pitch (patch 1 below), and the crate gives no
way to change it from outside, so it is vendored like `vendor/airplay-client`.

## Differences from upstream (besides patches)

- `Cargo.toml` is rewritten to be standalone: sibling crates come from the
  upstream git repository at the same commit, `*.workspace = true` keys are
  replaced by the upstream workspace values, dev-dependencies are dropped.
- `tests/` is not copied.
- `rustfmt.toml` (`disable_all_formatting = true`) keeps `cargo fmt --all` from
  reformatting upstream sources, so diffs against upstream stay readable.
- `src/` is otherwise byte-identical to upstream until the patches below.

## Local patches

1. **Sync anchors and send times come from the stream position.** Upstream
   read the wall clock in `run_streamer` while preparing each packet, after
   decoding and encoding, and announced that time plus the render delay in the
   sync packet; the sender thread kept its own schedule and reset it whenever a
   packet went out more than one packet late. Every decode delay therefore
   moved the anchor of the next sync packet, the first sync (stamped before the
   queue to the sender thread filled) was ~80 ms off from all later ones, and
   every reset shifted the stream and all later anchors for good. The receiver
   follows the rate the anchors imply, i.e. it resampled: speed and pitch moved.
   New module `src/timeline.rs` (public but `#[doc(hidden)]`, only so the host
   crate's `tests/send_timeline.rs` can run it): the first packet sets an
   origin, packet `n` frames later is due at `origin + n / rate`, and its sync
   packet announces that deadline plus `render_delay - 10 packets` (the margin
   upstream had in its steady state, so latency is unchanged). `run_streamer`
   takes each packet's deadline and anchor from it and passes the deadline to
   the sender thread (`SenderMessage::Packet::deadline_ns`), which sleeps until
   it and sends: a late packet goes out at once and the deadlines after it do
   not move. Only a packet prepared more than half the margin late restarts the
   timeline (logged, `AirPlay timeline restarted`). The sender thread counts
   packets sent more than one packet late (`AudioStreamer::late_packets`);
   `run_streamer` logs them (`AirPlay packets sent after their deadline`, at
   most every 10 s) instead of the per-packet jitter warnings, and the reset
   warning is gone with the reset. The unused burst mode (always 1) is removed.
2. **Decoding never waits for input.** `LiveAudioDecoder`'s receive timeout
   defaults to zero (`try_recv`); upstream waited 2 ms whenever the capture had
   caught up, inside the send loop. The `Decode took … (blocking send loop!)`
   warning is now a debug message: decode time no longer affects timing.
3. **`AudioStreamer::stop` returns.** Upstream joined the sender thread after
   a `Stop` message that is lost when the queue is full, while the producer
   task and every clone of the streamer (the control channel holds one) kept
   the channel open and `run_streamer` only checked the state `stop()` sets
   last: the join never returned. The sender thread now exits on a shared flag
   (`sender_running`), `run_streamer` checks the cached state first, and `stop`
   aborts the producer task before joining the thread.
