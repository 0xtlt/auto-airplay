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
# Audio (ms) queued between the Bluetooth input and AirPlay; see "Bluetooth packet loss" below.
# 50 to 10000, fixed: it never adapts to what it sees. Adds that much latency.
jitter_buffer_ms = 400
# RMS level (dBFS) below which the input counts as silent.
silence_threshold_dbfs = -40.0
# RMS level (dBFS) the input must reach, and keep, before the speaker is acquired.
# Default: silence_threshold_dbfs + 6.0 (-34.0 with the default threshold).
# sound_threshold_dbfs = -34.0

[runtime]
# Release the speaker after this many seconds of continuous silence (0 disables).
idle_silence_seconds = 30
# The input must stay at or above the sound threshold this long (250 to 60000 ms).
sound_confirm_ms = 2000
# After another AirPlay source takes the speaker: silence required before new sound
# may re-acquire it (1 to 3600 s), then the backoff before reconnecting, doubling for
# each consecutive takeover up to the maximum.
preempted_silence_seconds = 5
preemption_backoff_seconds = 30
preemption_backoff_max_seconds = 600
# Wait this long before retrying after a failed connection.
reconnect_delay_seconds = 3

[output]
# Reconnect (instead of exiting) when the speaker connection is lost.
auto_reconnect = true
```

## Bluetooth packet loss

A2DP over a noisy radio link drops packets, and a dropped packet is audio that never arrives: `bluealsa` logs `Missing RTP packets`, and the capture simply continues about 20 ms (one packet) to a few hundred ms later. Played as it comes, that is a splice in the waveform (a crackle) and it starves the AirPlay sender, which resets its timing. `auto-airplay` therefore never forwards the capture as it comes. It goes through a jitter buffer that conceals the holes:

- **Cushion.** Playback runs `audio.jitter_buffer_ms` (default 400, 50 to 10000) behind the capture, so delays up to that long are absorbed. The value is fixed; nothing resizes it. A turntable has no picture to keep in sync, so a few hundred ms to a couple of seconds are free; the AirPlay `output.render_delay_ms` (default 250) and the speaker's own buffer come on top. Larger values only cost memory (the buffer may hold twice the cushion: about 400 KB per second of `jitter_buffer_ms` at 48 kHz stereo) and a longer wait before the music follows the turntable. While no speaker is held, only the most recent `jitter_buffer_ms` is kept, so streaming starts at once with a full cushion.
- **Lost audio.** A hole is found from arrival times (the capture falls behind the real-time clock by the length of the hole, beyond the usual burstiness of A2DP, from 40 ms up) and is filled in place: the last few ms of the audio before it, played backwards from the last sample so the waveform stays continuous, fade out over 30 ms, then silence, and a 10 ms crossfade into the audio after it. The same is done for runs of exact digital zeros (2 ms or more right after audible signal), which is how some stacks fill missing frames; vinyl surface noise never is exactly zero, and silence after silence is passed through unchanged. If the buffer ever runs dry, the output is concealed the same way until it has refilled to half of the target.
- **Clock drift.** The turntable's clock and the Pi's differ by tens of ppm. When the buffer drifts 100 ms above the target, 20 to 60 ms are dropped with a crossfade; when it falls below a quarter of the target, 20 ms are stretched the same way, at most once per second of playback. Nothing is clicked or skipped, and with typical drift this happens every few minutes at most.

Once a minute, while capturing, the daemon logs the health of the Bluetooth input, even when everything is zero:

```text
bt_input stats: losses_per_min=2 concealed_per_min=2 concealed_ms=340 buffer_fill_ms=312 underruns=0 zero_runs=0 xruns=0 gaps=2 drift_corrections=0
```

`losses_per_min` counts the audio lost on the way in (`underruns`, `zero_runs`, capture `xruns` and arrival `gaps`), `concealed_per_min` the times concealment replaced audio, `concealed_ms` how much, `buffer_fill_ms` the current level, and `drift_corrections` the clock-drift trims. Find it with `journalctl -u "auto-airplay@$USER.service" | grep bt_input`. Many losses per minute mean a weak Bluetooth link: move the Pi's adapter and the turntable closer, or away from Wi-Fi and USB 3 devices.

`audio.latency_ms` was never used and is deprecated. It is still accepted but ignored, the daemon warns about it at startup, and any command that rewrites the configuration drops it. Use `audio.jitter_buffer_ms` instead.

## Speaker volume

`auto-airplay` never changes the speaker volume: not on the first connection, not when it re-acquires the speaker after silence, not after an automatic reconnect, a process recycle, or a restart caused by a configuration change. Set the volume on the speaker, the iPhone, or the Home app and it is kept. `auto-airplay output test` also plays at the speaker's current volume.

Older versions had an `audio.volume` setting and an `auto-airplay volume` command. A leftover `audio.volume` in the configuration file is still accepted but ignored, and the daemon logs a warning at startup (`doctor` reports it too). Delete the line when convenient; any command that rewrites the configuration drops it. The `volume` command was removed and fails without touching the configuration.

## Releasing the speaker on silence

An AirPlay speaker can only play one source at a time, so `auto-airplay` does not hold it while the input is silent:

- The daemon starts without connecting to the speaker. Audio is captured continuously, but the AirPlay session is only opened once sound is detected. Restarting the Pi while someone else is playing on the speaker does not interrupt them.
- After `runtime.sound_confirm_ms` (default 2000) of sustained sound, the bridge discovers the speaker, connects, and starts streaming. "Sustained" is strict: the level must be at or above the sound threshold (see below) on every check, about every 250 ms, for the whole window; a single check below it restarts the window. A needle pop, a static click, or idle noise therefore does not take over the speaker. Because discovery and connection take time, the start of the audio after the input resumes is not played: typically 1 to 6 seconds with the default `output.discovery_timeout_seconds = 4`, longer with a longer discovery timeout, a slow connection, or after a failed attempt (`runtime.reconnect_delay_seconds`).
- After `runtime.idle_silence_seconds` (default 30) of continuous silence, the bridge stops streaming and closes the AirPlay session cleanly, so iPhones, Apple TVs, and other sources can use the speaker right away. Sound resets the countdown, so quiet passages and the gaps between tracks do not release it unless they last that long.
- Stopping or restarting the service (SIGTERM or ctrl-c) and configuration changes also close the AirPlay session cleanly.
- `runtime.idle_silence_seconds = 0` disables this behavior: the bridge connects at startup and holds the speaker for as long as it runs.

Two levels, in dBFS (RMS), decide what counts as sound:

- `audio.silence_threshold_dbfs` (default `-40.0`): while the speaker is held, a level below this counts as silence and runs the `idle_silence_seconds` countdown.
- `audio.sound_threshold_dbfs` (optional, default `silence_threshold_dbfs + 6`, so `-34.0` with the default): while the speaker is released, the level must reach this and stay there for the confirm window before the speaker is acquired. It must not be lower than the silence threshold.

The gap between the two is deliberate. The idle noise of a Bluetooth turntable such as the AT-LP60-BT stays above `-50` with no record playing and can drift above the silence threshold; with the gap it is not loud enough to acquire the speaker. Releasing is unchanged: only levels below `silence_threshold_dbfs` count as silence.

When the speaker is acquired, the log line `sound detected; acquiring AirPlay output` carries `min_dbfs` and `max_dbfs` (the lowest and highest level during the confirm window), `sound_threshold_dbfs`, and `confirm_ms`. Use them to tune:

- If the speaker is acquired with nothing playing, `min_dbfs` shows how close the noise is to the threshold. Raise `sound_threshold_dbfs` above `max_dbfs` of those false starts, or lengthen `runtime.sound_confirm_ms`.
- If real music is not picked up, or starts late, `min_dbfs` of a normal start shows the quietest moment of the first seconds. Lower `sound_threshold_dbfs` (never below `silence_threshold_dbfs`) or shorten the window.
- If the speaker is never released because hum or noise stays above `silence_threshold_dbfs`, raise it toward `-35`; raise `sound_threshold_dbfs` with it, since it cannot be lower.
- If quiet passages are cut or the speaker is released during soft music, lower `silence_threshold_dbfs` toward `-50` or `-60`.

### When another source takes over the speaker

If an Apple TV, an iPhone, or another source starts playing on the speaker while the bridge streams, the speaker closes the bridge's session. The bridge recognizes this (the receiver closed the connection: `error_kind` `eof` or `reset-or-broken-pipe` in the logs) and does not fight for the speaker. It logs a warning, `AirPlay output taken by another source; waiting for silence then new sound before reconnecting`, with `consecutive_preemptions`, `backoff_seconds`, `error_kind`, and `since_stream_start_ms`, and then restarts into a waiting state. This is not an error: it happens whatever `output.auto_reconnect` is set to, and the process does not exit.

This classification is a conservative heuristic. An end of stream or a connection reset can also come from a HomePod reboot or a network failure; the bridge then waits for silence and new sound before reconnecting, even with `output.auto_reconnect = true`. A takeover whose close is only seen as a feedback timeout goes through the normal reconnect path (a known limitation).

The speaker is only taken back when all of these hold:

1. The backoff has elapsed. It starts when the new process starts and is `runtime.preemption_backoff_seconds` (default 30) for the first takeover, doubling for each consecutive one (30, 60, 120, ...) up to `runtime.preemption_backoff_max_seconds` (default 600). Silence heard during the backoff counts for the next condition.
2. The input has been continuously silent, below `audio.silence_threshold_dbfs`, for `runtime.preempted_silence_seconds` (default 5). Idle noise between the two thresholds is not silence.
3. After that silence, new sound is confirmed as described above (sound threshold held for `runtime.sound_confirm_ms`).

Sound that never stops, such as a record that is still playing or constant noise, therefore never takes the speaker back: the turntable has to go quiet first, for example when the record ends or is lifted. This also applies when `runtime.idle_silence_seconds = 0`.

The count of consecutive takeovers is kept through failed attempts to connect and through connections lost for another reason, so the wait applies again after them. It starts over when the session is released on silence, and when a session streamed for at least `runtime.preemption_backoff_max_seconds` before being lost or taken over (a takeover then counts as the first one). It is handed to the next process in the `AUTO_AIRPLAY_PREEMPTIONS` environment variable, which is internal: a missing or invalid value means no takeover, and restarting the service (or changing the configuration) always starts clean. Stopping the service or changing the configuration during the wait is handled immediately.

If the connection to the speaker fails or is lost for any other reason, `runtime.reconnect_delay_seconds` (default 3, minimum 1) is the pause before the next attempt. With `output.auto_reconnect = true` (default) the bridge keeps running and reconnects as soon as sound is present. With `false`, a lost connection makes the process exit with an error so the supervisor (systemd) restarts it. A failed attempt to acquire the speaker is always retried after the delay.

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
