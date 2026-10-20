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
```

Test the output before starting the bridge (the tone plays at the speaker's current volume):

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

## Configuration

The configuration lives in `/etc/auto-airplay/config.toml` and is normally written by the CLI. The keys that control when the speaker is held are:

```toml
[audio]
# RMS level (dBFS) below which the input counts as silent.
silence_threshold_dbfs = -40.0

[runtime]
# Release the speaker after this many seconds of continuous silence (0 disables).
idle_silence_seconds = 30
# Wait this long before retrying after a failed connection.
reconnect_delay_seconds = 3

[output]
# Reconnect (instead of exiting) when the speaker connection is lost.
auto_reconnect = true
```

## Speaker volume

`auto-airplay` never changes the speaker volume: not on the first connection, not when it re-acquires the speaker after silence, not after an automatic reconnect, a process recycle, or a restart caused by a configuration change. Set the volume on the speaker, the iPhone, or the Home app and it is kept. `auto-airplay output test` also plays at the speaker's current volume.

Older versions had an `audio.volume` setting and an `auto-airplay volume` command. A leftover `audio.volume` in the configuration file is still accepted but ignored, and the daemon logs a warning at startup (`doctor` reports it too). Delete the line when convenient; any command that rewrites the configuration drops it. The `volume` command was removed and fails without touching the configuration.

## Releasing the speaker on silence

An AirPlay speaker can only play one source at a time, so `auto-airplay` does not hold it while the input is silent:

- The daemon starts without connecting to the speaker. Audio is captured continuously, but the AirPlay session is only opened once sound is detected. Restarting the Pi while someone else is playing on the speaker does not interrupt them.
- After about 1 second of sustained sound, the bridge discovers the speaker, connects, and starts streaming. A single needle pop or static click is ignored. Because discovery and connection take time, the start of the audio after the input resumes is not played: typically 1 to 6 seconds with the default `output.discovery_timeout_seconds = 4`, longer with a longer discovery timeout, a slow connection, or after a failed attempt (`runtime.reconnect_delay_seconds`).
- After `runtime.idle_silence_seconds` (default 30) of continuous silence, the bridge stops streaming and closes the AirPlay session cleanly, so iPhones, Apple TVs, and other sources can use the speaker right away. Sound resets the countdown, so quiet passages and the gaps between tracks do not release it unless they last that long.
- Stopping or restarting the service (SIGTERM or ctrl-c) and configuration changes also close the AirPlay session cleanly.
- `runtime.idle_silence_seconds = 0` disables this behavior: the bridge connects at startup and holds the speaker for as long as it runs.

`audio.silence_threshold_dbfs` (default `-40.0`) sets the RMS level, in dBFS, that separates sound from silence. The default sits above the idle noise floor of a Bluetooth turntable such as the AT-LP60-BT, which stays above `-50` while powered on with no record playing. Check the logs to see when the speaker is acquired and released, then tune it:

- If the speaker is never released because turntable hum or noise stays above the threshold, raise the value toward `-35`.
- If quiet passages are cut or the speaker is released during soft music, lower the value toward `-50` or `-60`.

If the connection to the speaker fails or is lost, `runtime.reconnect_delay_seconds` (default 3, minimum 1) is the pause before the next attempt. With `output.auto_reconnect = true` (default) the bridge keeps running and reconnects as soon as sound is present. With `false`, a lost connection makes the process exit with an error so the supervisor (systemd) restarts it. A failed attempt to acquire the speaker is always retried after the delay.

Each AirPlay session runs in its own process: after the speaker is released, or after a failed or lost connection, `auto-airplay` replaces itself with a fresh copy of the same command (the PID stays the same). This keeps per-connection resources of the AirPlay library from accumulating over days of use. On these internal restarts the Bluetooth AVRCP Play command is not sent again, so a source that was stopped on purpose stays stopped.

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
- Debug logs from protocol dependencies are disabled by default. The one exception is the speaker's events channel (`events channel …` lines), which is logged at info level to diagnose dropped sessions; it records request methods and sizes, never keys or content.
- The systemd template restricts filesystem access and grants only real-time scheduling capability.

Do not commit a real configuration file. Common secret and configuration filenames are ignored by Git.

## Development

```bash
cargo fmt --all -- --check # vendor/airplay-client is excluded via its rustfmt.toml
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

GPL-2.0-only. The AirPlay implementation currently uses the GPL-2.0 Rust project [`airplay2-rs`](https://github.com/lmcgartland/airplay2-rs), pinned to a reviewed commit for reproducible builds. The `airplay-client` crate of that project is vendored in [`vendor/airplay-client`](vendor/airplay-client/VENDORED.md) with a small patch: upstream sent a 100 % volume every time a stream started, which `auto-airplay` must not do. Its origin and patches are listed in `VENDORED.md`.
