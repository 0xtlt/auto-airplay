# auto-airplay

`auto-airplay` is a small Rust audio bridge for Raspberry Pi. It receives audio from a Bluetooth or ALSA input and sends it to a selected AirPlay speaker.

The project is intentionally focused: one input, one output, automatic reconnection, automatic input format negotiation, and a CLI that does not require editing configuration files by hand.

## Status

Early development release. The following path is tested on a Raspberry Pi 4:

```text
Bluetooth turntable -> BlueZ/BlueALSA -> auto-airplay -> AirPlay 2 speaker
```

Application code is Rust. Linux still provides BlueZ, BlueALSA, ALSA, networking, and systemd.

## Install a release

Download the installer, inspect it, then run it for your Linux user:

```bash
curl -fsSLO https://raw.githubusercontent.com/0xtlt/auto-airplay/main/install.sh
less install.sh
sudo sh install.sh --user "$USER"
```

The installer detects ARM64 or ARMv7, downloads the matching GitHub release, verifies its SHA-256 checksum, and installs the binary and systemd template. It does not enable, start, or restart the service.

To install a specific release, add `--version v0.1.0`.

## Build from source

Install the Linux build requirements and Rust:

```bash
sudo apt install build-essential pkg-config libasound2-dev libdbus-1-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
cargo build --release
```

Install the binary and service template:

```bash
sudo install -m 0755 target/release/auto-airplay /usr/local/bin/auto-airplay
sudo install -m 0644 packaging/auto-airplay@.service /etc/systemd/system/auto-airplay@.service
sudo install -d -m 0750 -o "$USER" -g "$USER" /etc/auto-airplay
```

Discover and configure devices:

```bash
auto-airplay input list
auto-airplay input pair AA:BB:CC:DD:EE:FF --name "My turntable"
auto-airplay output list
auto-airplay output set "Living Room" --protocol airplay2
auto-airplay volume 35
```

Test the output before starting the bridge:

```bash
auto-airplay output test
```

Probe the input and run diagnostics:

```bash
auto-airplay input probe
auto-airplay doctor
```

Start at boot for the current Linux user:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now "auto-airplay@$USER.service"
journalctl -u "auto-airplay@$USER.service" -f
```

Configuration changes are detected while the daemon is running. The systemd service restarts the process automatically, providing a clean audio session with no inherited protocol workers.

## CLI

```text
auto-airplay config show [--json]
auto-airplay input list [--seconds 8]
auto-airplay input pair <MAC> [--name NAME]
auto-airplay input set-bluetooth <MAC> [--name NAME]
auto-airplay input set-alsa <DEVICE> [--name NAME]
auto-airplay input probe
auto-airplay output list [--seconds 4]
auto-airplay output set <NAME|ID|IP> [--protocol airplay2]
auto-airplay output test [--target NAME] [--frequency 880] [--duration 3]
auto-airplay volume <0-100>
auto-airplay doctor
auto-airplay run
```

Use `--config <path>` to use a configuration file other than `/etc/auto-airplay/config.toml`.

## Input format detection

For Bluetooth inputs, the Linux adapter reads the source format reported by BlueALSA over D-Bus and opens ALSA with that exact rate and channel count. For direct ALSA inputs, it probes the device capabilities and reports the actual negotiated format. A preferred rate can be set in the configuration, but no rate is hard-coded by default.

The capture loop runs on a dedicated blocking thread. Audio buffers are bounded, recovery handles ALSA underruns, and AirPlay resampling is only used when the negotiated input rate differs from the AirPlay stream rate.

## Raspberry Pi support

The code has no Pi-model-specific paths and is intended for:

- Raspberry Pi Zero 2 W;
- Raspberry Pi 3, 4, and 5;
- other ARMv7 or ARM64 Raspberry Pi OS/Debian installations.

The original Pi Zero and Pi 1 are experimental because AirPlay encryption and ALAC encoding may exceed their real-time CPU budget.

## Privacy and security

- No telemetry or analytics.
- No cloud account.
- No PIN, password, or audio content is written to logs.
- Configuration writes are atomic and use mode `0600`.
- Debug logs from protocol dependencies are disabled by default.
- The systemd template restricts filesystem access and grants only real-time scheduling capability.

Do not commit a real configuration file. Common secret and configuration filenames are ignored by Git.

## Development

```bash
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Hardware-dependent tests are performed with `input probe`, `output test`, and `doctor` on the target Pi.

Installer tests are isolated in Docker:

```bash
docker build --file tests/install/Dockerfile --tag auto-airplay-installer-test .
```

## Releases

Pushing a version tag such as `v0.1.0` builds checksum-protected ARM64 and ARMv7 archives and publishes them to GitHub Releases. The tag must match the version in `Cargo.toml`. The release workflow can also be run manually to validate both builds without publishing a release.

## License and acknowledgements

GPL-2.0-only. The AirPlay implementation currently uses the GPL-2.0 Rust project [`airplay2-rs`](https://github.com/lmcgartland/airplay2-rs), pinned to a reviewed commit for reproducible builds.
