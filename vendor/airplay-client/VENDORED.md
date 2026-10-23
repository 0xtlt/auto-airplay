# Vendored `airplay-client`

Origin: `crates/airplay-client` from <https://github.com/lmcgartland/airplay2-rs.git>,
commit `a7f019fe6246ebd9701201a8a5c31e9a15243956`.

License: GPL-2.0 (upstream `LICENSE`, copied here unchanged).

## Why

The upstream crate sends an implicit `SET_PARAMETER volume` (100 %) every time a
stream starts, and `AirPlayClient` gives no way to opt out. auto-airplay must
never change the speaker volume, so the crate is vendored and patched.

## Differences from upstream (besides patches)

- `Cargo.toml` is rewritten to be standalone: sibling crates come from the
  upstream git repository at the same commit, `*.workspace = true` keys are
  replaced by the upstream workspace values, dev-dependencies are dropped.
- `examples/` and `.DS_Store` are not copied.
- `rustfmt.toml` (`disable_all_formatting = true`) keeps `cargo fmt --all` from
  reformatting upstream sources, so diffs against upstream stay readable.
- `src/` is otherwise byte-identical to upstream until the patches below.

## Local patches

1. **No implicit volume at stream start.** Removed the
   `self.set_volume(self.volume)` call (and its "Sending SET_PARAMETER volume"
   log) from `Connection::start_streaming`, `Connection::start_streaming_live`
   (`src/connection.rs`) and from the RAOP streaming start
   (`RaopConnection::start_streaming`, `src/raop_connection.rs`, formerly
   "Send initial volume"). Upstream always sent its default volume of `1.0`
   (0 dB, 100 %) there. The public `set_volume()` API is unchanged and still
   sends the volume when called explicitly.
2. **Answer the receiver's requests on the events channel.** Upstream opens the
   TCP connection to the receiver's `eventPort` after SETUP phase 1 but never
   reads it. The receiver sends HomeKit-encrypted RTSP-style requests there and
   may close the connection when they stay unanswered. New module
   `src/receiver_events.rs` (public but `#[doc(hidden)]`, only so the host
   crate's `tests/receiver_events.rs` can run it) derives the events keys from
   the pairing shared secret (HKDF-SHA512, salt `Events-Salt`, sender encrypts
   with `Events-Read-Encryption-Key`, decrypts with `Events-Write-Encryption-Key`,
   swapped once if the first frame fails authentication) and answers every
   request with a bare `200 OK`. It never acts on a request. `Connection` keeps
   the shared secret (`events_secret`, filled in all connect paths) and the task
   handle (`events_task`, aborted in `disconnect()` and `Drop`); without a secret
   the old behaviour (stream held open, unread) remains.
3. **PTP follower and media clock for buffered streams.** New modules, no
   upstream code changed. `src/ptp_follower.rs` (public but `#[doc(hidden)]`,
   only so the host crate's `tests/ptp_follower.rs` can drive it against a
   fake grandmaster) follows the receiver's PTP clock the way a Mac does with
   a HomePod: it introduces itself with the messages upstream's
   `run_bmca_yield_flow` sends (byte for byte, priority1 250 against the
   HomePod's 248), yields, then pairs each Sync with its Follow_Up and sends a
   Delay_Req for each. Its estimate takes the fastest Sync and the fastest
   Delay_Req of the last 4 s (delays only ever add up, so those bound the
   offset from both sides, whatever the Wi-Fi spikes); the published clock
   only slews toward it (500 ppm at most), so it never jumps. Its timestamps
   are on the monotonic clock (`airplay_audio::timeline::mono_now_ns`), not
   the wall clock upstream used. It reports a grandmaster that falls silent
   (15 s), changes, or jumps (over 50 ms for 3 s). Sync and Follow_Up arrive
   on two sockets read by two threads, so either can be handled first:
   `SyncPairer` pairs them in either order (on port identity, domain and
   sequence), each waiting at most 1 s and 32 deep. Only one clock is
   followed: the port identity and domain of the first Announce (or of the
   first Sync paired before any Announce); messages from any other port or
   domain at the speaker's address are ignored (counted), so they can neither
   move the estimate nor keep a silent clock looking alive. Delay_Req goes out
   in the followed clock's domain. `src/media_clock.rs`
   (`MediaClock`, exported) hands that clock to the host's output pacer.
4. **Buffered sessions.** `Connection::setup` and `start_streaming_live`
   hand buffered stream configurations (`StreamType::Buffered`) to a new
   submodule, `src/connection/buffered.rs`; `stop`, `disconnect`,
   `send_feedback` and `streamer_packets_sent` gain a buffered branch;
   realtime streams take exactly the old paths. The session: start the PTP
   follower (fails early if ports 319/320 cannot be bound), SETUP of the
   timing channel with PTP and our address as timing peer, the events
   connection, `SETPEERS` (the speaker's timing addresses and ours, the way
   Apple senders send it), the PTP introduction, SETUP of a type 103 stream
   (AAC-LC 44.1 kHz stereo, 1024 frames per packet), RECORD, then waiting
   (8 s at most) until the speaker's clock is followed. Streaming connects to
   the stream's data port, starts `airplay_audio::BufferedStreamer` and sends
   one `SETRATEANCHORTIME`: RTP frame 0 plays `latency` (default 2 s) from now
   on the speaker's PTP timeline. `pause` and `resume` have buffered branches
   too (upstream's FLUSH and RECORD are realtime requests): pause stops making
   packets (the live input of the pause is dropped), sends
   `SETRATEANCHORTIME` with rate 0 and `FLUSHBUFFERED` up to the first packet
   after the pause, so the speaker drops the audio it still holds; resume
   restarts the AAC encoder and anchors that packet `latency` from now at
   rate 1. Sequence numbers and RTP time carry on across the pause. `stop`
   removes the stream with a `TEARDOWN` that lists it. `send_feedback` fails when the stream's connection failed
   (with the I/O error kind, so a receiver dropping it reads as such) or the
   clock was lost. `AirPlayClient::set_buffered(latency)` selects all this;
   `media_clock()` gives the speaker's clock to pace the input with;
   `buffered_stats()` the stream's counters. When a buffered SETUP fails, the
   client tears the connection down at once (realtime keeps upstream's
   behaviour). `Connection::connect_unpaired`, `set_ptp_config` and
   `AirPlayClient::connect_unpaired`, `set_ptp_config` are public but
   `#[doc(hidden)]`, only so the host crate's `tests/buffered_session.rs` can
   run a session against a fake speaker.
