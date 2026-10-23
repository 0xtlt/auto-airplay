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

None yet.
