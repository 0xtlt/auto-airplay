# Security

## Reporting

Please report security issues privately to the repository owner instead of opening a public issue.

## Data handling

`auto-airplay` processes PCM audio in memory and sends it only to the configured receiver on the local network. It does not include telemetry, analytics, or cloud storage.

Configuration files do not store Bluetooth PINs or AirPlay passwords. Runtime logs must not contain credentials, private keys, or raw audio payloads.

## Deployment

Use the included systemd template instead of running the daemon as root. Keep the operating system, BlueZ, BlueALSA, and Rust dependencies updated.
