#!/bin/sh

set -eu

repository="0xtlt/auto-airplay"
version="latest"
service_user=${AUTO_AIRPLAY_USER:-${SUDO_USER:-}}
prefix="/usr/local"
config_dir="/etc/auto-airplay"
systemd_dir="/etc/systemd/system"

usage() {
    cat <<'EOF'
Install a pre-built auto-airplay release.

Usage:
  sudo ./install.sh --user USER [options]

Options:
  --user USER          Linux user that will run auto-airplay.
  --version VERSION    Release tag to install, for example v0.1.0 (default: latest).
  --prefix PATH        Installation prefix (default: /usr/local).
  --config-dir PATH    Configuration directory (default: /etc/auto-airplay).
  --systemd-dir PATH   systemd unit directory (default: /etc/systemd/system).
  -h, --help           Show this help.

The installer never enables, starts, or restarts the service.
EOF
}

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

require_value() {
    [ "$#" -ge 2 ] || fail "missing value for $1"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --user)
            require_value "$@"
            service_user=$2
            shift 2
            ;;
        --version)
            require_value "$@"
            version=$2
            shift 2
            ;;
        --prefix)
            require_value "$@"
            prefix=$2
            shift 2
            ;;
        --config-dir)
            require_value "$@"
            config_dir=$2
            shift 2
            ;;
        --systemd-dir)
            require_value "$@"
            systemd_dir=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown option: $1"
            ;;
    esac
done

[ "$(id -u)" -eq 0 ] || fail "run this installer as root (for example with sudo)"
[ -n "$service_user" ] || fail "pass --user USER (or run with sudo from that user)"
id "$service_user" >/dev/null 2>&1 || fail "Linux user does not exist: $service_user"
service_group=$(id -gn "$service_user")

machine=${AUTO_AIRPLAY_ARCH:-$(uname -m)}
case "$machine" in
    aarch64|arm64)
        target="aarch64-unknown-linux-gnu"
        ;;
    armv7l|armv7)
        target="armv7-unknown-linux-gnueabihf"
        ;;
    *)
        fail "unsupported architecture: $machine (supported: aarch64/arm64, armv7l)"
        ;;
esac

release_base=${AUTO_AIRPLAY_RELEASE_BASE_URL:-"https://github.com/${repository}/releases"}
case "$version" in
    latest)
        download_base="${release_base}/latest/download"
        ;;
    v[0-9]*)
        case "$version" in
            *[!A-Za-z0-9._-]*) fail "invalid version: $version" ;;
        esac
        download_base="${release_base}/download/${version}"
        ;;
    *)
        fail "invalid version: $version (expected latest or a tag such as v0.1.0)"
        ;;
esac

archive="auto-airplay-${target}.tar.gz"
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

download() {
    source_url=$1
    destination=$2
    case "$source_url" in
        https://*)
            curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
                --retry 3 --output "$destination" "$source_url"
            ;;
        file://*)
            [ "${AUTO_AIRPLAY_ALLOW_FILE_URLS:-0}" = "1" ] || \
                fail "file URLs are disabled"
            curl --fail --silent --show-error --location --output "$destination" "$source_url"
            ;;
        *)
            fail "release URL must use HTTPS"
            ;;
    esac
}

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"
command -v install >/dev/null 2>&1 || fail "install is required"

printf 'Downloading %s for %s...\n' "$version" "$target"
download "${download_base}/${archive}" "${temporary}/${archive}"
download "${download_base}/${archive}.sha256" "${temporary}/${archive}.sha256"

expected=$(sed -n '1{s/[[:space:]].*//;p;}' "${temporary}/${archive}.sha256")
case "$expected" in
    ''|*[!0-9a-fA-F]*) fail "release checksum is invalid" ;;
esac
[ "${#expected}" -eq 64 ] || fail "release checksum is invalid"

if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "${temporary}/${archive}" | sed 's/[[:space:]].*//')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "${temporary}/${archive}" | sed 's/[[:space:]].*//')
else
    fail "sha256sum or shasum is required"
fi
[ "$actual" = "$expected" ] || fail "release checksum verification failed"

if tar -tzf "${temporary}/${archive}" | grep -E '(^/|(^|/)\.\.(/|$))' >/dev/null 2>&1; then
    fail "release archive contains an unsafe path"
fi

mkdir "${temporary}/release"
tar -xzf "${temporary}/${archive}" -C "${temporary}/release"
[ -f "${temporary}/release/auto-airplay" ] || fail "release does not contain auto-airplay"
[ -f "${temporary}/release/auto-airplay@.service" ] || \
    fail "release does not contain the systemd unit"

install -d -m 0755 "${prefix}/bin"
install -m 0755 "${temporary}/release/auto-airplay" "${prefix}/bin/auto-airplay"
install -d -m 0755 "$systemd_dir"
install -m 0644 "${temporary}/release/auto-airplay@.service" \
    "${systemd_dir}/auto-airplay@.service"
install -d -m 0750 -o "$service_user" -g "$service_group" "$config_dir"

if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload
fi

cat <<EOF
auto-airplay ${version} installed for ${service_user}.

The service was not started or restarted. Next steps:
  auto-airplay input list
  auto-airplay output list
  sudo systemctl enable --now auto-airplay@${service_user}.service
EOF
