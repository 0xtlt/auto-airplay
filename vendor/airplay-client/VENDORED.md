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
3. **Output counters.** `AirPlayClient::take_output_stats` and
   `Connection::take_output_stats` hand over the streamer's
   `take_output_stats` (see `vendor/airplay-audio`, patch 6; `OutputStats` is
   re-exported). The control channel loops (`Connection::start_streaming`,
   `Connection::start_streaming_live`, the group listener) count retransmit
   requests (payload type 85) they cannot parse with
   `AudioStreamer::note_unparsed_retransmit_request`; upstream dropped them
   without a log in the live path.
