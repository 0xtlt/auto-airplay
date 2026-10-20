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

None yet.
