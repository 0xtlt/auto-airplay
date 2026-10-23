# Vendored `airplay-audio`

Origin: `crates/airplay-audio` from <https://github.com/lmcgartland/airplay2-rs.git>,
commit `a7f019fe6246ebd9701201a8a5c31e9a15243956`.

License: GPL-2.0 (upstream `LICENSE`, copied here unchanged).

## Why

The live streaming path (`AudioStreamer::start_live`) decides when each RTP
packet is sent and which time each sync packet announces for it. auto-airplay
needs to change that timing, and the crate gives no way to do it from outside,
so it is vendored like `vendor/airplay-client`.

## Differences from upstream (besides patches)

- `Cargo.toml` is rewritten to be standalone: sibling crates come from the
  upstream git repository at the same commit, `*.workspace = true` keys are
  replaced by the upstream workspace values, dev-dependencies are dropped.
- `tests/` is not copied.
- `rustfmt.toml` (`disable_all_formatting = true`) keeps `cargo fmt --all` from
  reformatting upstream sources, so diffs against upstream stay readable.
- `src/` is otherwise byte-identical to upstream until the patches below.

## Local patches

None yet.
