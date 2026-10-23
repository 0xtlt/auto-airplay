# Vendored `airplay-rtsp`

Origin: `crates/airplay-rtsp` from <https://github.com/lmcgartland/airplay2-rs.git>,
commit `a7f019fe6246ebd9701201a8a5c31e9a15243956`.

License: GPL-2.0 (upstream `LICENSE`, copied here unchanged).

## Why

AirPlay 2 buffered streaming (stream type 103) needs RTSP requests the crate
cannot send (`SETRATEANCHORTIME`, `SETPEERS` the way Apple senders send it) and
SETUP payloads and replies it does not handle, and the crate gives no way to add
them from outside, so it is vendored like `vendor/airplay-client`.

## Differences from upstream (besides patches)

- `Cargo.toml` is rewritten to be standalone: sibling crates come from the
  upstream git repository at the same commit, `*.workspace = true` keys are
  replaced by the upstream workspace values, dev-dependencies are dropped.
- `rustfmt.toml` (`disable_all_formatting = true`) keeps `cargo fmt --all` from
  reformatting upstream sources, so diffs against upstream stay readable.
- `src/` is otherwise byte-identical to upstream until the patches below.

## Local patches

1. **Buffered streams (type 103).** What AirPlay 2 buffered streaming needs and
   upstream could not express:
   - `RtspMethod::SetRateAnchorTime` and `RtspRequest::set_rate_anchor_time`;
     `RtspSession::build_rate_anchor` builds its body
     (`plist_codec::SetRateAnchorTimeRequest`): the RTP timestamp, the network
     time as seconds plus a 64-bit binary fraction, the timeline (grandmaster
     clock identity) and the rate.
   - `RtspRequest::setpeers_for_session`: `SETPEERS` on the session URI with
     the content type Apple senders use (`/peer-list-changed`). Upstream's
     `setpeers` (URI `/peer-list-changed`, binary plist content type) is kept.
   - `RtspRequest::teardown_streams` and `RtspSession::build_teardown_streams`:
     `TEARDOWN` with a body that removes the stream but keeps the session.
   - SETUP phase 2: `ct` comes from `session::compression_type`, which treats
     compression types as the bit flags they are (1 LPCM, 2 ALAC, 4 AAC-LC,
     8 AAC-ELD, 32 Opus); upstream's `AudioCodec::compression_type()` says 3
     for AAC and 4 for AAC-ELD. A buffered stream sends no `asc` (Apple
     senders do not; `audioFormat` says the format).
   - Replies: `controlPort` of a phase 2 stream and both fields of
     `timingPeerInfo` default when absent instead of failing the SETUP;
     `audioBufferSize` is read (`RtspSession::audio_buffer_size`); the
     receiver's timing peer addresses are kept for `SETPEERS`
     (`RtspSession::receiver_timing_addresses`).
   - `plist_codec` is public (`#[doc(hidden)]`) so the host crate's tests can
     decode what is sent.
